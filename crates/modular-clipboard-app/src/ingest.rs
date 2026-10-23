//! 捕获入库管线：自写抑制、序列号闸门、预览生成、图片尺寸。
//!
//! `capture_should_read` 是「这次剪贴板变更要不要读」的唯一
//! 判据；`build_preview` 截断到 [`PREVIEW_MAX_CHARS`]。

use anyhow::Result;
use modular_clipboard_core::CapturedPayload;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::ClipKind;
use std::time::Instant;

use super::{AppState, SELF_WRITE_GUARD_MS_FLOOR};

/// 在状态栏显示一条消息（3 秒后由绘制层过期）。
pub(super) fn set_notice(state: &mut AppState, msg: String) {
    state.notice = Some((msg, Instant::now()));
}


/// 捕获循环的单步决策：这一轮是否应该读取剪贴板内容。
///
/// 返回 `true` 当且仅当：序列号有新变化，且当前不在自写回抑制窗口内。
///
/// NOTE: 无论结果如何，都会推进序列号基线（`ClipboardReader::changed` 的
/// 副作用）。这是刻意为之：自写回（`copy_item`）同样会改变剪贴板序列号，
/// 若抑制期内跳过基线推进，自写导致的变化会在抑制期结束后被当成用户的
/// 新复制重新捕获——每次把历史条目放回剪贴板都多记一条重复记录。
///
/// `now` 为当前墙钟（Unix 毫秒），`last_self_write` 为上次自写回时刻。
/// 减法必须用 `saturating_sub`：墙钟可能被 NTP 校时或手动改时间回拨，
/// 自写抑制窗口，按**轮询间隔**动态计算。
///
/// # 为什么不能是常量
///
/// 设自写发生在 T，捕获线程上一轮在 `t0 ∈ (T-I, T]` 唤醒（I = 轮询间隔），
/// 下一轮在 `t0+I`，此时 `since = t0 + I - T`。最坏情形 `t0 → T`，
/// `since → I`。
///
/// 因此抑制窗口必须 **> I** 才能保证「自写后的第一轮一定落在窗口内」。
/// 早前 `SELF_WRITE_GUARD_MS` 是常量 900，而 `poll_interval_ms` 只夹
/// 下限（50）、**无上限**——用户在设置面板把间隔调到 1000ms（省电是很
/// 自然的动机）就会让 `since ≥ 900` 成立 ⇒ **抑制彻底失效**，此后每次
/// 从历史写回都多记一条。
///
/// 取 `2*I + 200`：覆盖「上一轮刚好在写之前唤醒」的最坏情形，并留余量。
///
/// # 已知取舍
///
/// 窗口越大，自写之后**用户真实的复制**被一并吞掉的概率越高（它们会被
/// `changed()` 吸收进基线后丢弃）。这是时间窗方案的固有代价。更彻底的
/// 解法是**计数式抑制**（写前 `fetch_add(1)`，捕获线程消费一次），
/// 与轮询间隔、墙钟回拨、NTP 校时全部解耦 —— 但那需要跨线程共享计数器，
/// 改动面大，留待后续。
pub(super) fn self_write_guard_ms(poll_interval_ms: i64) -> i64 {
    (2 * poll_interval_ms + 200).max(SELF_WRITE_GUARD_MS_FLOOR)
}

/// 无符号减法会下溢成巨值，让抑制永久失效（见捕获线程的注释）。
///
/// 纯函数：序列号是否变化由调用方以 `peek_changed` 窥探后传入——
/// 决策与「提交基线」分离，读取失败时才能重试（DF-05）。
pub(super) fn capture_should_read(
    seq_changed: bool,
    now: i64,
    last_self_write: i64,
    poll_interval_ms: i64,
) -> bool {
    let since = now.saturating_sub(last_self_write);
    seq_changed && since >= self_write_guard_ms(poll_interval_ms)
}


/// 预览文本的最大字符数（PF-5 / SEC-018）。
///
/// 列表只需要「是否超过 N 行」的信息量；全文已文件化在 blob 里，
/// 详情面板可读全量。不截断的代价是三重复放大的：
/// DB 每条存几百 KB（1 万条上限即数 GB）→ 每次 `reload_list`
/// 全量物化 → UI 每帧再全量 clone + 全量哈希一遍（PF-1）。
///
/// 已知取舍：FTS 索引随 preview 截断，长文档只有前 512 字符可被
/// 搜索到。剪贴板场景的高价值内容（URL、代码开头、报错首行）
/// 基本都在开头；需要全文检索的场景应改用详情面板内的本地搜索。
pub const PREVIEW_MAX_CHARS: usize = 512;

/// 生成预览文本。
pub(super) fn build_preview(payload: &CapturedPayload) -> String {
    match payload.kind {
        // Markdown 的预览即**源码前 512 字符**：界面按 Markdown 渲染
        // 这一截（与 Text 同样的截断取舍，PF-5 / SEC-018）。
        ClipKind::Text | ClipKind::Html | ClipKind::Markdown => {
            let raw = payload
                .text
                .clone()
                .unwrap_or_else(|| String::from_utf8_lossy(&payload.bytes).to_string());
            truncate_chars(&raw, PREVIEW_MAX_CHARS)
        }
        ClipKind::Files => {
            let names: Vec<String> = payload
                .files
                .iter()
                .map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            if names.len() == 1 {
                names[0].clone()
            } else {
                modular_clipboard_core::i18n::t(("{a} 等 {b} 个文件", "{a} + {b} more files"))
                    .replace("{a}", &names[0])
                    .replace("{b}", &names.len().to_string())
            }
        }
        ClipKind::Image => {
            // 图片用尺寸描述作为预览，解码失败时给出可读提示。
            // dims 优先取捕获侧透传的（载荷为裸 RGBA8 时这是唯一来源），
            // 缺失才解析文件头（兼容历史编码格式数据）。
            let dims = payload
                .dims
                .filter(|(w, h)| *w > 0 && *h > 0)
                .or_else(|| image_dimensions(&payload.bytes));
            match dims {
                Some((w, h)) => format!(
                    "{} {w}×{h}",
                    modular_clipboard_core::i18n::t(("图片", "Image"))
                ),
                None => format!(
                    "{} {} {}",
                    modular_clipboard_core::i18n::t(("图片", "Image")),
                    payload.bytes.len(),
                    modular_clipboard_core::i18n::t(("字节", "bytes"))
                ),
            }
        }
    }
}

/// 按字符数截断（char boundary 安全），超长时追加省略号。
pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 读取图片尺寸（仅解析文件头，不解码像素）。
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 文件列表里**第一个图片文件**的像素尺寸（只读文件头，不读全文件）。
///
/// 供 `ClipKind::Files` 条目在入库时补齐 `dims`——见
/// [`crate::Service::ingest`] 里那段关于行高跳变的说明。
///
/// # 只对「扩展名像图片」的文件动手
///
/// 资源管理器复制的文件可以是任何东西，对每个都去读文件头等于每次
/// 复制都白搭一次磁盘 IO。`is_image_file` 是 `O(1)` 的字符串预筛。
///
/// # 失败即放弃
///
/// 文件被删、没权限、内容其实是别的格式——一律返回 `None`，条目退化成
/// 「普通文件条目」（文本预览）。入库路径不能被一个坏文件卡住。
pub(super) fn first_image_file_dims(files: &[String]) -> Option<(u32, u32)> {
    let path = files.first()?;
    if !modular_clipboard_core::media::is_image_file(path) {
        return None;
    }
    // `into_dimensions` 只解析文件头，不会把整张图读进内存。
    let f = std::fs::File::open(path).ok()?;
    image::ImageReader::new(std::io::BufReader::new(f))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 把载荷解码为 RGBA8，供写回剪贴板使用。
///
/// # 双形态
///
/// 1. **裸 RGBA8**（现行规范）：载荷长度与 `item.payload.dims` 声名的
///    `w*h*4` 精确吻合时直接透传。这是 arboard 在 Windows 上交给我们的
///    形态（CF_DIBV5 解码产物），`load_from_memory` 对它必然失败——
///    曾导致历史图片条目「能看不能复制」。
/// 2. **编码格式**（历史数据 / 其他来源）：走 `image` crate 全量解码。
pub(super) fn decode_to_rgba(item: &ClipItem, bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    if let Some((w, h)) = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .filter(|(w, h)| *w > 0 && *h > 0)
        && (w as u64)
            .saturating_mul(h as u64)
            .saturating_mul(4)
            == bytes.len() as u64
    {
        return Ok((bytes.to_vec(), w, h));
    }
    let img = image::load_from_memory(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

/// 时间格式化，用于界面显示。
pub fn format_time(ts_ms: i64, now_ms: i64) -> String {
    let diff = now_ms - ts_ms;
    if diff < 60_000 {
        modular_clipboard_core::i18n::t(("刚刚", "Just now")).to_string()
    } else if diff < 3_600_000 {
        format!("{} {}", diff / 60_000, modular_clipboard_core::i18n::t(("分钟前", "min ago")))
    } else if diff < 86_400_000 {
        format!("{} {}", diff / 3_600_000, modular_clipboard_core::i18n::t(("小时前", "hours ago")))
    } else {
        format!("{} 天前", diff / 86_400_000)
    }
}
