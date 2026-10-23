//! 详情面板、类型栏（Rail）内容与空状态。
//!
//! 详情面板读 `state.selected` 的条目；图片支持缩放查看。

use egui::Align2;
use egui::Rect;
use egui::Sense;
use egui::Stroke;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::i18n::t;
use crate::icons::Icon;
use crate::theme::Palette;
use crate::theme::sized;

use super::{Frame, Op};

use super::shadow::fade_color;
use super::row::draw_image_preview;
use super::row::draw_markdown_preview;
use super::row::draw_source_preview;
use super::row::draw_text_preview;
use super::row::format_meta;
use super::row::load_image_bytes;
use super::row::preview_is_image;

/// 详情面板：选中条目的全文视图。
///
/// 2026-10-16 起：详情面板按 [`ClipKind`] 分发——图片内嵌缩略图、
/// Markdown 渲染 / 源码、文本折行，**不再只用 `elide_text` 截一段**。
/// 这正是「图片内容直接显示」与「Markdown 渲染显示」在主窗口的另一
/// 个展示位：列表行只显示一行内的预览（缩略图 / 第一屏 Markdown），
/// 详情面板给完整内容（缩略图占满宽、Markdown 全文）。
///
/// 行高限制也不同于列表：列表行 `IMG_MAX_H_LIST`（160pt）防止单条
/// 撑爆，本函数不限——内容带本身就被详情卡片高度约束，超长 Markdown
/// 自动溢出滚动区。
///
/// # 配置开关
///
/// - `image_inline` 关闭 ⇒ 图片条目降级为「图片 WxH」文字；
/// - `md_rendered` 关闭 ⇒ Markdown 条目降级为等宽纯文本。
///
/// 与列表行保持一致的开关语义——用户在一个地方关了，在另一处也看不到。
pub(super) fn draw_detail_body(f: &mut Frame<'_>, body: Rect) {
    use modular_clipboard_core::ClipKind;
    let Some(id) = f.svc.state.selected else {
        // 选中丢失：原图缓存随之丢弃。
        clear_detail_image_zoom(f);
        draw_empty(f, body, t("Select an entry to see details"));
        return;
    };
    // 先把选中的 item 拷一份：原图模式分支与三分支都需要
    // `item.id`（推 Op / 解码原图），而 `f.svc.state.items` 的借用会
    // 与后面的 `&mut f` 冲突。`ClipItem` 字段少，clone 不贵。
    let Some(item) = f.svc.state.items.iter().find(|it| it.id == id).cloned() else {
        // 选中条目已不存在：原图缓存同样失效。
        clear_detail_image_zoom(f);
        draw_empty(f, body, t("This entry no longer exists"));
        return;
    };

    // 原图模式：当前选中是图片、且 zoom 状态指向它——独占整张详情卡。
    //
    // 判据用 [`preview_is_image`] 而不是 `kind == Image`：详情面板的
    // 「放大」按钮对**文件型图片**也画（见上面的三分支），这里若只认
    // `Image`，文件型点了放大没反应（按钮在了、功能没接上）。
    if preview_is_image(&item) && f.state.detail_image_zoom == Some(item.id) {
        draw_detail_image_zoom(f, body, &item);
        return;
    }

    // 不是原图模式：清掉残留的原图缓存（zoom 已失效或选中已换）。
    if f.state.detail_image_zoom.is_some() {
        clear_detail_image_zoom(f);
    }

    let pal = f.pal;
    let scale = f.scale;

    // 底栏：来源 · 时间（meta 行）。与原实现保持一致的位置与字号。
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size * 1.2;
    let meta_band = Rect::from_min_max(
        pos2(body.min.x, body.max.y - meta_h),
        pos2(body.max.x, body.max.y),
    );
    // 内容带：去掉底栏后剩下的全部空间。
    let content_band = Rect::from_min_max(body.min, pos2(body.max.x, meta_band.min.y));
    if content_band.width() <= 0.0 || content_band.height() <= 0.0 {
        return;
    }

    // 判据：从 config 现取，不能持有 `&cfg` 跨 `&mut f`。
    let image_inline = f.svc.state.config.ui.image_inline;
    let md_rendered = f.svc.state.config.ui.md_rendered;

    // 预览绘制用的**裁剪 Ui**：裁剪到内容带。
    //
    // 与列表行同一个道理（见 `draw_text_preview` 的 CAUTION）：预览函数
    // 一律用「调用方给的 Ui」画，才能被内容带裁剪；拿根 `f.ui` 会把
    // 长文本/大图画到卡片之外。
    //
    // NOTE: `new_child` 只建 Ui、不推进父 Ui 的游标，也不借用 `f`——
    // 因此可以与 `&mut f` 并存。
    let preview_ui = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("detail_preview"))
            .max_rect(content_band)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );

    // 三分支：图片直显 / Markdown 渲染 / 文本折行。
    //
    // CAUTION: `draw_image_preview` 与 `draw_markdown_preview` 都需要
    // `&mut f`，所以这里**只能二选一**——开关决定具体走哪一支。
    match item.kind {
        // `ClipKind::Files` 也进来：从资源管理器复制的**单个图片文件**
        // 在详情面板同样直显（图在磁盘上，载荷里只有路径）。
        ClipKind::Image | ClipKind::Files if preview_is_image(&item) && image_inline => {
            // 详情面板让图占满宽（不限 IMG_MAX_H_LIST，按 band.height
            // 走，长宽比由 `draw_image_preview` 内部保持）。
            //
            // 报告到 `thumb_seen`：让本帧上传的纹理在 `draw` 收尾
            // retain 时不被误淘汰（与列表行用同一机制）。
            f.state.thumb_seen.insert(item.id);
            draw_image_preview(f, &preview_ui, content_band, &item, content_band.height());
            // 缩略图右下角：「放大」按钮，点击进入原图模式。
            draw_zoom_button(f, pal, &item);
        }
        ClipKind::Markdown if md_rendered => {
            draw_markdown_preview(f, &preview_ui, content_band, &item);
        }
        ClipKind::Markdown => {
            // md_rendered 关：等宽源码按 band 高度计算可见行数（不截断），
            // 溢出自动换行。`draw_source_preview` 内部已 max_rows 限位，
            // 详情面板空间通常足够放下整段。
            draw_source_preview(f, &preview_ui, content_band, &item);
        }
        _ => {
            // 文本 / HTML / Files / Markdown 关 / Image 关 ⇒ 等宽折行。
            //
            // 用 `draw_text_preview` 而不是 `elide_text`：详情面板就该把
            // 全文铺开，让用户读到完整内容。`draw_text_preview` 内部按
            // band 宽度折行，溢出自动到下一行（撑出可见区会被外层
            // 滚动框裁剪）。
            draw_text_preview(f, &preview_ui, content_band, &item, false);
        }
    }

    // 来源 · 时间：永远画在最底，与列表行语义一致。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, meta_band.width());
    if !meta_shown.is_empty() {
        f.ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }
}

/// 清掉详情面板的原图模式缓存（zoom + 已上传的 orig 纹理）。
///
/// `TextureHandle` 是 RAII：drop 时 egui 自动 `ctx.forget()`，所以只需
/// 让 `detail_image_orig` 离开作用域——这里统一清两个字段保证一致。
pub(super) fn clear_detail_image_zoom(f: &mut Frame<'_>) {
    f.state.detail_image_zoom = None;
    f.state.detail_image_orig = None;
}

/// 在图片缩略图右下角画一个「放大」按钮。点击推一个 [`Op::DetailImageZoom`]。
///
/// `image_inline` 关闭或载荷被淘汰（无 dims）时不画——按钮的出现与
/// 实际能进入原图模式的能力保持一致。
pub(super) fn draw_zoom_button(f: &mut Frame<'_>, pal: &Palette, item: &ClipItem) {
    // 判据 = 「能不能进原图模式」：尺寸要么在载荷里，要么是解码时记下的
    // （老条目缺 `payload.dims`，解码后靠 [`UiState::thumb_dims`]）。
    // 两条都没有 ⇒ 缩略图也画不出来，按钮更不该出现。
    let has_dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .or_else(|| f.state.thumb_dims.get(&item.id).copied())
        .map(|(w, h)| w > 0 && h > 0)
        .unwrap_or(false);
    if !has_dims {
        return;
    }
    let btn_size = 24.0;
    let pad = 8.0;
    // 按钮位置：内容带右下角。`painter_at` 与按钮的 rect 一致。
    let btn_rect = Rect::from_min_size(
        pos2(
            f.area.max.x - btn_size - pad,
            f.area.max.y - btn_size - pad,
        ),
        vec2(btn_size, btn_size),
    );
    // 用 `ui.interact` 而**不是**直接 push Op：每帧都画按钮，
    // 直接 push 等于「每帧推同一个 Op」，会让 IPC 通讯爆炸；
    // `clicked` 仅在用户真正点击时为 true。
    let id = egui::Id::unique(("detail_image_zoom", item.id));
    let resp = f.ui.interact(btn_rect, id, Sense::click());
    let painter = f.ui.painter_at(btn_rect);
    // 底色补间（110ms）：悬停时 surface_variant → surface，与行内按钮
    // 同一套动效；不悬停时停在 surface_variant。
    let bg = crate::anim::tween_color(
        f.ui.ctx(),
        id.with("bg"),
        if resp.hovered() {
            pal.surface
        } else {
            pal.surface_variant
        },
        std::time::Duration::from_millis(110),
    );
    painter.rect_filled(btn_rect, pal.radius_md, bg);
    // 图标颜色补间（110ms）：悬停提亮一档。
    let icon_color = crate::anim::tween_color(
        f.ui.ctx(),
        id.with("icon"),
        if resp.hovered() { pal.text } else { pal.text_dim },
        std::time::Duration::from_millis(110),
    );
    Icon::Image.paint(&painter, btn_rect.shrink(4.0), icon_color);
    if resp.clicked() {
        f.state.ops.push(Op::DetailImageZoom(item.id));
    }
}

/// 原图模式：直接渲染**全尺寸**原图，按内容带保持长宽比。
///
/// 与缩略图路径分两条：
/// - 缩略图走 `thumb_cache` + `IMG_MAX_H_LIST` 限位；
/// - 原图用 `thumbnail::decode(bytes, u32::MAX)` 不限尺寸、单独缓存
///   在 `detail_image_orig`，避免污染缩略图缓存。
///
/// `decode` 是同步调用——4K 图可能让 UI 卡几十毫秒。当前项目里
/// 一次只点一次，先把功能接通；后续若需要可加后台线程（见 TODO）。
pub(super) fn draw_detail_image_zoom(f: &mut Frame<'_>, body: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;

    // 底栏：与缩略图模式一致；返回按钮放在右上角，与 meta 行同行。
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size * 1.2;
    let meta_band = Rect::from_min_max(
        pos2(body.min.x, body.max.y - meta_h),
        pos2(body.max.x, body.max.y),
    );
    let content_band = Rect::from_min_max(body.min, pos2(body.max.x, meta_band.min.y));

    // 原图按需解码 + 上传：缓存命中（id 匹配）就跳过。
    let dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .filter(|(w, h)| *w > 0 && *h > 0);
    let needs_upload = f
        .state
        .detail_image_orig
        .as_ref()
        .map(|(id, _)| *id != item.id)
        .unwrap_or(true);
    if needs_upload {
        // 字节来源与缩略图同一条：图片型读载荷，文件型按首行路径**读盘**
        // （从资源管理器复制的 .png 只有路径在库里）。
        match load_image_bytes(f.svc, item) {
            Ok(bytes) => {
                // 与缩略图同一套 auto 入口：裸 RGBA8（现行规范）走旁路，
                // 历史编码格式回落嗅探。`u32::MAX` = 不降采样（原图）。
                match crate::thumbnail::decode_auto(&bytes, dims, u32::MAX) {
                    Ok(thumb) => {
                        let img = thumb.to_color_image();
                        let handle = f.ui.ctx().load_texture(
                            format!("mc_orig_{}", item.id),
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        f.state.detail_image_orig = Some((item.id, handle));
                    }
                    Err(e) => {
                        tracing::debug!(id = item.id, ?e, "原图解码失败");
                        f.state.detail_image_orig = None;
                    }
                }
            }
            Err(e) => {
                tracing::debug!(id = item.id, ?e, "原图读取失败");
                f.state.detail_image_orig = None;
            }
        }
    }

    // 取当前已上传的纹理**与其尺寸**——失败时画错误提示。
    //
    // 尺寸取自纹理本身而不是 `payload.dims`：文件型图片没有后者（载荷
    // 是路径列表），而纹理 size 就是解码出来的真实像素尺寸，两类条目
    // 通用，也不怕载荷被淘汰。
    let uploaded = f
        .state
        .detail_image_orig
        .as_ref()
        .filter(|(id, _)| *id == item.id)
        .map(|(_, h)| (h.id(), h.size()));

    if let Some((tex_id, size)) = uploaded {
        // 原图占满内容带、保持长宽比。
        let (w, h) = (size[0] as u32, size[1] as u32);
        if w > 0 && h > 0 {
            let ratio = w as f32 / h as f32;
            let mut disp_w = content_band.width();
            let mut disp_h = disp_w / ratio;
            if disp_h > content_band.height() {
                disp_h = content_band.height();
                disp_w = disp_h * ratio;
            }
            let rect = Rect::from_min_size(content_band.min, vec2(disp_w, disp_h));
            let painter = f.ui.painter_at(rect);
            painter.image(
                tex_id,
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            // 1px 描边：浅色图与深色面板之间需要边界。
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0, pal.border_subtle),
                egui::StrokeKind::Inside,
            );
        } else {
            draw_empty(f, content_band, t("Original size unknown"));
        }
    } else {
        draw_empty(f, content_band, t("Failed to load the original image"));
    }

    // 来源 · 时间：给返回按钮留 80pt 空间。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, meta_band.width() - 80.0);
    if !meta_shown.is_empty() {
        f.ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }

    // 右上角「返回」按钮：点击清除 zoom 与 orig 缓存。
    let back_size = 20.0;
    let back_pad = 6.0;
    let back_rect = Rect::from_min_size(
        pos2(
            meta_band.max.x - back_size - back_pad,
            meta_band.min.y + (meta_h - back_size) * 0.5,
        ),
        vec2(back_size, back_size),
    );
    let back_id = egui::Id::unique(("detail_image_back", item.id));
    let back_resp = f.ui.interact(back_rect, back_id, Sense::click());
    Icon::Close.paint(&f.ui.painter_at(back_rect), back_rect, pal.text_dim);
    if back_resp.clicked() {
        f.state.ops.push(Op::DetailImageZoomClear);
    }
}

pub(super) fn draw_rail_body(f: &mut Frame<'_>, body: Rect) {
    let pal = f.pal;
    let total = f.svc.state.items.len();
    if total == 0 {
        draw_empty(f, body, t("No data"));
        return;
    }
    let font = sized(pal.font_xs, f.scale);
    let row_h = 22.0;
    let n = ((body.height() / row_h).floor() as usize).min(total);
    // NOTE: 不克隆整表（PF-1）：这里只画前 n 行，直接按索引取原表。
    // 循环体对 f 全是只读使用（painter_at / elide_text），无借用冲突。
    for (i, it) in f.svc.state.items.iter().take(n).enumerate() {
        let y = body.min.y + i as f32 * row_h;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        if r.max.y > body.max.y {
            break;
        }
        let txt = format!("{} · {}", it.source_app, it.preview);
        let shown = crate::text::elide_text(f.ui, &txt, &font, r.width());
        f.ui.painter_at(r).text(
            r.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font.clone(),
            pal.text_dim,
        );
    }
}

pub(super) fn draw_empty(f: &mut Frame<'_>, body: Rect, msg: &str) {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    // 入场补间（200ms OutCubic）：文案淡入 + 上浮 8pt。
    //
    // 为什么用 `fade_in_once` 而不是 `tween_f32`：后者首帧插入即落在
    // 目标上（惰性语义，悬停/选中就该这样），入场动画要的是「首次出现
    // 从 0 开始放」。`fade_in_once` 用「条目是否存在」判重，落定后不
    // 重播——搜索时每条击键都不会重启它。
    //
    // id 带消息文本：不同提示（没有记录 / 没有匹配 / 选中一条…）各放
    // 各的；同一消息重复出现（清了搜索又回到空列表）不重播，可接受。
    let k = crate::anim::fade_in_once(
        f.ui.ctx(),
        egui::Id::unique(("empty_in", msg)),
        std::time::Duration::from_millis(200),
        crate::anim::Ease::OutCubic,
    );
    if k <= 0.004 {
        return; // 完全透明：不画
    }
    // NOTE: painter_at(body)：空状态文案是居中的，而窄卡片（折叠把手
    // 旁的详情栏）装不下整句，根 painter 会让它溢出到相邻卡片上。
    let center = body.center() - vec2(0.0, (1.0 - k) * 8.0);
    f.ui
        .painter_at(body)
        .text(center, Align2::CENTER_CENTER, msg, font, fade_color(pal.text_dim, k));
}
