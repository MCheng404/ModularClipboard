//! Markdown 子集渲染（列表行内嵌显示）。
//!
//! 2026-10-15 第三轮原型移植项（原型 `main-proto/index.html` 的同名功能；
//! 原型本身已于 2026-10-23 仓库瘦身移除，源码在 `old` 分支），
//! 设置项 `ui.md_rendered` 关闭时调用方回落为等宽源码文本。
//!
//! # 覆盖范围
//!
//! - 块级：ATX 标题（`#`~`######`）、段落、无序 / 有序列表、引用、
//!   围栏代码块（``` ```）、分割线（`---` / `***`）；
//! - 行内：`**粗**`、`*斜*`、`` `码` ``、`[文字](链接)`。
//!
//! # 三个关键设计
//!
//! 1. **粗体用「提亮 + 字号」表达，不用字重**。egui 的 `FontId` 只有
//!    `family` + `size`，没有粗细轴，全仓也不注册粗体字体；靠颜色
//!    （`text_bright`）与字号区分层级，和纯平审美一致。
//! 2. **测量与绘制走同一条代码路径**（`layout_block` 的
//!    `paint: Option<..>`）：行高必须先于绘制算出来（滚动数学要用），
//!    两条路径各算一遍必然漂移——那正是「行高算一套、绘制判一套」
//!    这类历史 bug 的成因。egui 内部对 galley 有缓存，重复排版不贵。
//! 3. **不 panic**：源码是剪贴板内容（不可信输入），解析器对任何
//!    畸形输入（未闭合的代码栏、孤立的 `**`、嵌套标记）都给出
//!    可渲染的结果，最坏情况是按纯文本显示。

use crate::theme::Palette;
use egui::{pos2, text::LayoutJob, Pos2, Rect, Stroke, TextFormat, Vec2};

/// 块之间的纵向间距（逻辑点）。
const BLOCK_GAP: f32 = 6.0;
/// 列表缩进（逻辑点）。
const LIST_INDENT: f32 = 14.0;
/// 引用竖条宽度（逻辑点）。
const QUOTE_BAR: f32 = 3.0;
/// 引用竖条与文字的间距（逻辑点）。
const QUOTE_GAP: f32 = 8.0;
/// 代码块内边距（逻辑点）。
const CODE_PAD: f32 = 8.0;

/// 行内片段。
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    /// 普通文字。
    Text(String),
    /// 强调（渲染为 `text_bright`）。
    Strong(String),
    /// 斜体语义（渲染为 `text` + 左右书名号感的下划线色，见 `span_fmt`）。
    Em(String),
    /// 行内代码（等宽 + `surface_variant` 底）。
    Code(String),
    /// 链接文字（`accent` 色；不做实际跳转，列表行不可点链接）。
    Link(String),
    /// 行内图片（`![alt](src)`）。
    ///
    /// 渲染为占位方块 + alt 文字 + src 类型标签（路径/URL）。
    /// **不**自动加载：剪贴板 Markdown 里 src 多为本地图片或远端 URL，
    /// 自动加载会让 md.rs 引入 image crate / 网络 IO，破坏分层。
    /// 用户可在设置/工具里加「点开图片」动作。
    Image { alt: String, src: String },
}

/// 块级元素。
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// 标题，`level` 1~6。
    Heading { level: u8, spans: Vec<Inline> },
    /// 段落。
    Para(Vec<Inline>),
    /// 列表，`ordered` 区分有序 / 无序。
    List {
        ordered: bool,
        items: Vec<Vec<Inline>>,
    },
    /// 引用。
    Quote(Vec<Inline>),
    /// 代码块（原文，逐行保留）。
    Code(String),
    /// 分割线。
    Hr,
}

/// 一份解析后的 Markdown 文档。
#[derive(Debug, Clone, PartialEq)]
pub struct MdDoc {
    pub blocks: Vec<Block>,
}

impl MdDoc {
    /// 解析 Markdown 源码。
    pub fn parse(src: &str) -> Self {
        // CAUTION: `replace` 的结果必须先绑定——`split` 借的是它，
        // 直接链式调用会让借引用指向一条语句结束时就被释放的临时值。
        let owned = src.replace("\r\n", "\n");
        let lines: Vec<&str> = owned.split('\n').collect();
        let mut blocks = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            // 围栏代码块：吞到下一个围栏或 EOF（未闭合不 panic）。
            if line.trim_start().starts_with("```") {
                let mut buf = Vec::new();
                i += 1;
                while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                    buf.push(lines[i]);
                    i += 1;
                }
                if i < lines.len() {
                    i += 1; // 吃掉收尾围栏
                }
                blocks.push(Block::Code(buf.join("\n")));
                continue;
            }
            let t = line.trim_start();
            if let Some(rest) = heading_level(t) {
                blocks.push(Block::Heading {
                    level: rest.0,
                    spans: parse_inline(rest.1),
                });
                i += 1;
                continue;
            }
            if t.starts_with("> ") || t == ">" {
                blocks.push(Block::Quote(parse_inline(t.trim_start_matches('>').trim())));
                i += 1;
                continue;
            }
            if is_ul(t) {
                let mut items = Vec::new();
                while i < lines.len() && is_ul(lines[i].trim_start()) {
                    items.push(parse_inline(lines[i].trim_start()[2..].trim()));
                    i += 1;
                }
                blocks.push(Block::List {
                    ordered: false,
                    items,
                });
                continue;
            }
            if is_ol(t) {
                let mut items = Vec::new();
                while i < lines.len() && is_ol(lines[i].trim_start()) {
                    let s = lines[i].trim_start();
                    let n = s.chars().take_while(|c| c.is_ascii_digit()).count();
                    // 前缀恒为「数字 + 2 字符」（". " 或 ") "）。
                    items.push(parse_inline(s[n + 2..].trim()));
                    i += 1;
                }
                blocks.push(Block::List {
                    ordered: true,
                    items,
                });
                continue;
            }
            if t == "---" || t == "***" || t == "___" {
                blocks.push(Block::Hr);
                i += 1;
                continue;
            }
            if t.is_empty() {
                i += 1;
                continue;
            }
            // 段落：吃到空行或下一个块级标记为止（软换行并成一段）。
            let mut buf = vec![t.to_string()];
            i += 1;
            while i < lines.len() {
                let s = lines[i].trim_start();
                if s.is_empty()
                    || s.starts_with("```")
                    || heading_level(s).is_some()
                    || s.starts_with("> ")
                    || is_ul(s)
                    || is_ol(s)
                    || s == "---"
                    || s == "***"
                {
                    break;
                }
                buf.push(s.to_string());
                i += 1;
            }
            blocks.push(Block::Para(parse_inline(&buf.join(" "))));
        }
        Self { blocks }
    }

    /// 是否空文档（无任何块）。
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 在 `width`（逻辑点）下排版后的总高度。
    pub fn height(&self, ui: &egui::Ui, pal: &Palette, scale: f32, width: f32) -> f32 {
        self.walk(ui, pal, scale, width, None).1
    }

    /// 从 `top_left` 开始绘制，返回实际占位总高度。
    pub fn draw(
        &self,
        ui: &egui::Ui,
        pal: &Palette,
        scale: f32,
        top_left: Pos2,
        width: f32,
    ) -> f32 {
        self.walk(ui, pal, scale, width, Some((top_left, ui.painter().clone()))).1
    }

    /// 遍历全部块：测量（`paint = None`）或测量 + 绘制。
    ///
    /// 返回 `(结束 y, 总高)`——`结束 y` 是相对 `top_left.y` 的偏移量，
    /// 调用方画完引用条之类的装饰时要用。
    fn walk(
        &self,
        ui: &egui::Ui,
        pal: &Palette,
        scale: f32,
        width: f32,
        paint: Option<(Pos2, egui::Painter)>,
    ) -> (f32, f32) {
        let mut y = 0.0;
        for (idx, block) in self.blocks.iter().enumerate() {
            if idx > 0 {
                y += BLOCK_GAP;
            }
            let h = layout_block(ui, pal, scale, block, width, paint.as_ref().map(|(tl, p)| (*tl + Vec2::new(0.0, y), p.clone())));
            y += h;
        }
        (y, y)
    }
}

/// 标题前缀：返回 `(级别, 其余文字)`。
fn heading_level(t: &str) -> Option<(u8, &str)> {
    let n = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&n) && t[n..].starts_with(' ') {
        Some((n as u8, t[n..].trim()))
    } else {
        None
    }
}

/// 无序列表标记（`- ` / `* ` / `+ `）。
fn is_ul(t: &str) -> bool {
    t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ")
}

/// 有序列表标记（`1. ` / `12) `）。
fn is_ol(t: &str) -> bool {
    let n = t.chars().take_while(|c| c.is_ascii_digit()).count();
    (1..=9).contains(&n) && (t[n..].starts_with(". ") || t[n..].starts_with(") "))
}

/// 行内解析：`**粗**` / `*斜*` / `` `码` `` / `[文字](链接)`。
///
/// 单趟扫描，不成对的标记按普通文字输出（不丢内容）。
fn parse_inline(s: &str) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Inline>| {
        if !buf.is_empty() {
            out.push(Inline::Text(std::mem::take(buf)));
        }
    };
    while i < bytes.len() {
        // 行内代码
        // NOTE: 用 let 链（edition 2024）把「定界符 + 配对成功」压成一层——
        // 嵌套 if 在四个行内规则上各写一遍，clippy 的 collapsible_if
        // 会全部报警，而压平后判据反而更好读。
        if bytes[i] == '`'
            && let Some(end) = find(&bytes, i + 1, '`')
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Code(bytes[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        // 粗体 **
        if bytes[i] == '*'
            && i + 1 < bytes.len()
            && bytes[i + 1] == '*'
            && let Some(end) = find_seq(&bytes, i + 2, &['*', '*'])
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Strong(bytes[i + 2..end].iter().collect()));
            i = end + 2;
            continue;
        }
        // 斜体 *
        if bytes[i] == '*'
            && let Some(end) = find(&bytes, i + 1, '*')
            && end > i + 1
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Em(bytes[i + 1..end].iter().collect()));
            i = end + 1;
            continue;
        }
        // 链接 [文字](url)
        if bytes[i] == '['
            && let Some(close) = find(&bytes, i + 1, ']')
            && close + 1 < bytes.len()
            && bytes[close + 1] == '('
            && let Some(paren) = find(&bytes, close + 2, ')')
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Link(bytes[i + 1..close].iter().collect()));
            let _ = paren; // url 不渲染（列表行不跳转）
            i = paren + 1;
            continue;
        }
        // 行内图片 ![alt](src)。
        //
        // CAUTION: 必须在链接 `[文字]` 之前匹配——否则链接解析器会
        // 抢先吃掉 `[alt]` 这一段，留下的 `(src)` 当成普通字符。
        // 也就是说，顺序不是「都可以」而是「图片必须先」。
        if bytes[i] == '!'
            && i + 1 < bytes.len()
            && bytes[i + 1] == '['
            && let Some(close) = find(&bytes, i + 2, ']')
            && close + 1 < bytes.len()
            && bytes[close + 1] == '('
            && let Some(paren) = find(&bytes, close + 2, ')')
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Image {
                alt: bytes[i + 2..close].iter().collect(),
                src: bytes[close + 2..paren].iter().collect(),
            });
            i = paren + 1;
            continue;
        }
        buf.push(bytes[i]);
        i += 1;
    }
    flush(&mut buf, &mut out);
    out
}

/// 从 `from` 起找单个字符。
fn find(chars: &[char], from: usize, c: char) -> Option<usize> {
    chars.iter().skip(from).position(|x| *x == c).map(|p| p + from)
}

/// 从 `from` 起找字符序列。
fn find_seq(chars: &[char], from: usize, seq: &[char]) -> Option<usize> {
    if from + seq.len() > chars.len() {
        return None;
    }
    (from..=chars.len() - seq.len()).find(|&p| chars[p..p + seq.len()] == *seq)
}

/// 行内片段的文字内容（测量 / 搜索用）。
fn span_text(s: &Inline) -> &str {
    match s {
        Inline::Text(t) | Inline::Strong(t) | Inline::Em(t) | Inline::Code(t) | Inline::Link(t) => {
            t
        }
        // 图片：返回 alt（空 alt 时调用方会显示「图片」兜底标签）。
        Inline::Image { alt, .. } => alt,
    }
}

/// 行内图片占位的高度（逻辑点）。
///
/// 56 ≈ 图标尺寸(16) + 上下各 12pt 间距 + src 标签行。
/// 用单行高，避免与相邻段落过远；列表行预览条窄时也不会破版。
const IMG_PLACEHOLDER_H: f32 = 56.0;
/// 行内图片占位的圆角半径（逻辑点）。
const IMG_PLACEHOLDER_R: f32 = 6.0;
/// 行内图片占位的左/右内边距（逻辑点）。
const IMG_PLACEHOLDER_PAD: f32 = 8.0;

/// Bootstrap Icons `image` 在 PUA 区的码位（与 `assets/icons.rs` 的
/// `Icon::Image` 一致）。这里**不**走 `icons::Icon` 而是直接写码位，
/// 因为 md.rs 是无 IO 依赖的纯 CPU 模块；引入 icons 会把字体与渲染
/// 拉进来、破坏分层。码位是公开契约（icons.rs 同步保证），
/// 改了任何一边请同时改这条断言。
pub const IMG_ICON_CODEPOINT: char = '\u{e007}';
/// 图标字体名（与 `assets/modular-clipboard-icons.ttf` 的 family 名一致）。
pub const IMG_ICON_FONT: &str = "modular-clipboard-icons";

/// 行内图片占位的视觉行。
fn paint_inline_image(
    painter: &egui::Painter,
    top_left: Pos2,
    width: f32,
    pal: &Palette,
    scale: f32,
    alt: &str,
    src: &str,
) {
    let bg = Rect::from_min_size(top_left, Vec2::new(width, IMG_PLACEHOLDER_H));
    painter.rect_filled(bg, IMG_PLACEHOLDER_R, pal.surface_variant);
    // 左侧图标（Bootstrap Icons `image`）。
    let icon_size = (pal.font_md * scale).min(IMG_PLACEHOLDER_H - IMG_PLACEHOLDER_PAD * 2.0);
    let icon_x = top_left.x + IMG_PLACEHOLDER_PAD;
    let icon_y = top_left.y + (IMG_PLACEHOLDER_H - icon_size) * 0.5;
    painter.text(
        pos2(icon_x + icon_size * 0.5, icon_y + icon_size * 0.5),
        egui::Align2::CENTER_CENTER,
        IMG_ICON_CODEPOINT.to_string(),
        egui::FontId::new(icon_size, egui::FontFamily::Name(IMG_ICON_FONT.into())),
        pal.text_dim,
    );
    // 右侧文字：alt + src 类型标签。
    // - alt 为空则显示「图片」兜底；
    // - src 简化成「线上 / 内嵌 / 无路径」+ 短文件名 / 域名。
    let text_x = icon_x + icon_size + IMG_PLACEHOLDER_PAD;
    let text_w = (bg.max.x - IMG_PLACEHOLDER_PAD - text_x).max(20.0);
    let alt_text = if alt.is_empty() { modular_clipboard_core::i18n::t("Image") } else { alt };
    let (src_kind, src_label) = classify_src(src);
    let alt_font = egui::FontId::proportional(pal.font_sm * scale);
    let src_font = egui::FontId::proportional(pal.font_xs * scale);
    // alt 完整显示（最多一行），溢出 ellipsis。
    let alt_y = top_left.y + IMG_PLACEHOLDER_PAD;
    let alt_job = {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            alt_text,
            0.0,
            egui::TextFormat {
                font_id: alt_font.clone(),
                color: pal.text,
                ..Default::default()
            },
        );
        job.wrap.max_width = text_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        job
    };
    let alt_galley = painter.layout_job(alt_job);
    painter.galley(pos2(text_x, alt_y), alt_galley, pal.text);
    // 下方 src 标签：`线上/本地/内嵌/无路径 + 短标识`。
    let src_y = alt_y + pal.font_sm * 1.2 * scale + 1.0;
    let src_text = if src_label.is_empty() {
        src_kind.to_string()
    } else {
        format!("{src_kind} · {src_label}")
    };
    let src_job = {
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &src_text,
            0.0,
            egui::TextFormat {
                font_id: src_font,
                color: pal.text_dim,
                ..Default::default()
            },
        );
        job.wrap.max_width = text_w;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        job.wrap.overflow_character = Some('…');
        job
    };
    let src_galley = painter.layout_job(src_job);
    painter.galley(pos2(text_x, src_y), src_galley, pal.text_dim);
}

/// src 类型简标签。剪贴板 Markdown 里 src 通常是本地路径或远端 URL，
/// 完整路径挤不进占位框，所以只显示类型与末段（文件名 / 域名）。
fn classify_src(src: &str) -> (&'static str, String) {
    let s = src.trim();
    if s.is_empty() {
        return (modular_clipboard_core::i18n::t("No path"), String::new());
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        // 域名或末段路径。
        let after = s
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let (host, _) = after.split_once('/').unwrap_or((after, ""));
        return ("URL", host.to_string());
    }
    if s.starts_with("data:") {
        return (modular_clipboard_core::i18n::t("Embedded"), "data URL".into()); // 简化：永远显示 data URL 标签
    }
    // 本地路径：取末段（Win / Unix 都以 `/` 或 `\` 分隔）。
    let last = s
        .rsplit_once(['/', '\\'])
        .map(|(_, name)| name)
        .unwrap_or(s);
    (modular_clipboard_core::i18n::t("Local"), last.to_string())
}

/// 一段相邻的文字或图片。`![alt](src)` 强制独占一行，**不**与正文同行。
///
/// 把 `&[Inline]` 切成「文本段」与「图片段」后，绘制时交替铺：
/// 文本段走原 `spans_job` → galley → 画，图片段画占位方块。
/// 这样既能保留原行内排版的全部能力（强调 / 行内代码 / 链接），
/// 又能给图片一个可识别的视觉语义（图标 + alt + src 类型）。
///
/// 返回 `(文本段切片, 该段内是否含图片)`：图片段文本切片为空、
/// 图片信息为 `Some`；文本段图片信息为 `None`。
type SegItem<'a> = (&'a [Inline], Option<(&'a str, &'a str)>);

/// 把行内片段按图片边界切段。
///
/// 相邻的文本片段合并；每张图片独占一段（前后被切）。
/// 末段若是文本也单独成一项，调用方按列表长度判断段落是不是零。
fn inline_segments(spans: &[Inline]) -> Vec<SegItem<'_>> {
    let mut out: Vec<SegItem<'_>> = Vec::new();
    let mut text_start = 0usize;
    for (i, s) in spans.iter().enumerate() {
        if let Inline::Image { alt, src } = s {
            if text_start < i {
                out.push((&spans[text_start..i], None));
            }
            out.push((&[], Some((alt.as_str(), src.as_str()))));
            text_start = i + 1;
        }
    }
    if text_start < spans.len() {
        out.push((&spans[text_start..], None));
    }
    out
}

/// 行内片段的文本格式（颜色 / 字体 / 背景）。
fn span_fmt(s: &Inline, pal: &Palette, scale: f32, body: f32) -> TextFormat {
    let logical = |size: f32| egui::FontId::proportional(size * scale);
    match s {
        Inline::Text(_) => TextFormat {
            font_id: logical(body),
            color: pal.text,
            ..Default::default()
        },
        // 无字重轴：强调 = 提亮一档（见模块文档第 1 条）。
        Inline::Strong(_) => TextFormat {
            font_id: logical(body),
            color: pal.text_bright,
            ..Default::default()
        },
        Inline::Em(_) => TextFormat {
            font_id: logical(body),
            color: pal.text_dim,
            ..Default::default()
        },
        Inline::Code(_) => TextFormat {
            font_id: egui::FontId::monospace(pal.font_sm * scale),
            color: pal.text,
            background: pal.surface_variant,
            ..Default::default()
        },
        Inline::Link(_) => TextFormat {
            font_id: logical(body),
            color: pal.accent,
            ..Default::default()
        },
        // 图片本身不进入 LayoutJob（`inline_segments` 已把它切到独立段），
        // 但 spans_job 在空段落防御里会调一次 span_fmt——给个稳定值兜底。
        Inline::Image { .. } => TextFormat {
            font_id: logical(body),
            color: pal.text,
            ..Default::default()
        },
    }
}

/// 把行内片段序列铺成 `LayoutJob`。
fn spans_job(spans: &[Inline], pal: &Palette, scale: f32, body: f32, width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for s in spans {
        job.append(span_text(s), 0.0, span_fmt(s, pal, scale, body));
    }
    if job.sections.is_empty() {
        job.append("", 0.0, span_fmt(&Inline::Text(String::new()), pal, scale, body));
    }
    job.wrap.max_width = width;
    job.wrap.max_rows = usize::MAX;
    job.wrap.break_anywhere = true;
    job
}

/// 排版（并可选绘制）一个块，返回其高度（逻辑点）。
///
/// `paint = Some((top_left, painter))` 时绘制；`None` 只测量。
/// **两条路径唯一的几何来源**——行高与绘制因此不可能漂移。
fn layout_block(
    ui: &egui::Ui,
    pal: &Palette,
    scale: f32,
    block: &Block,
    width: f32,
    paint: Option<(Pos2, egui::Painter)>,
) -> f32 {
    let body = pal.font_md;
    let line_h = body * 1.45;
    let paint_ref: Option<&(Pos2, egui::Painter)> = paint.as_ref();
    match block {
        Block::Heading { level, spans } => {
            let size = match level {
                1 => pal.font_lg * 1.2,
                2 => pal.font_lg,
                _ => body,
            };
            let job = {
                let mut j = spans_job(spans, pal, scale, size, width);
                // 标题统一提亮：无字重轴下的层级表达（见模块文档第 1 条）。
                for sec in j.sections.iter_mut() {
                    sec.format.color = pal.text_bright;
                }
                j
            };
            let galley = ui.painter().layout_job(job);
            // CAUTION: galley 是 `Arc`（不可 Copy）——必须先取高度再
            // 移交给 painter，反过来会被借用检查拦下。
            let h = galley.size().y.max(size * 1.4);
            if let Some((tl, painter)) = paint {
                painter.galley(tl, galley, pal.text_bright);
            }
            h
        }
        Block::Para(spans) => {
            // 段落含图片时不能用 spans_job 一次性排——图片必须换段。
            // 走统一入口：文本段与图片段都铺到同一高度。
            let h = layout_inline_segments(ui, pal, scale, spans, width, body, paint_ref, false);
            h.max(line_h)
        }
        Block::List { ordered, items } => {
            let mut y = 0.0;
            let marker_w = LIST_INDENT;
            for (n, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}. ", n + 1)
                } else {
                    "• ".to_string()
                };
                // 列表项本身可能含图片：同样走统一入口。
                let inner_w = (width - marker_w).max(20.0);
                let inner_h = layout_inline_segments(
                    ui,
                    pal,
                    scale,
                    item,
                    inner_w,
                    body,
                    paint_ref
                        .map(|(tl, p)| (*tl + Vec2::new(marker_w, y), p.clone()))
                        .as_ref(),
                    false,
                );
                let h = inner_h.max(line_h);
                if let Some((tl, painter)) = &paint {
                    let tl = *tl;
                    painter.text(
                        pos2(tl.x, tl.y + y),
                        egui::Align2::LEFT_TOP,
                        marker,
                        egui::FontId::proportional(body * scale),
                        pal.text_dim,
                    );
                }
                y += h;
            }
            y
        }
        Block::Quote(spans) => {
            let text_x = QUOTE_BAR + QUOTE_GAP;
            let inner_w = (width - text_x).max(20.0);
            // 引用整体降一档亮度：走 `layout_inline_segments`（图片占位自带
            // 背景亮度，与引用降亮不冲突）。
            let inner_h =
                layout_inline_segments(ui, pal, scale, spans, inner_w, body, paint_ref, true);
            let h = inner_h.max(line_h);
            if let Some((tl, painter)) = paint {
                let bar = Rect::from_min_size(tl, Vec2::new(QUOTE_BAR, h));
                painter.rect_filled(bar, 0.0, pal.accent);
            }
            h
        }
        Block::Code(src) => {
            let font = egui::FontId::monospace(pal.font_sm * scale);
            let inner_w = (width - CODE_PAD * 2.0).max(20.0);
            let mut job = LayoutJob::default();
            job.append(
                src,
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: pal.text,
                    ..Default::default()
                },
            );
            job.wrap.max_width = inner_w;
            job.wrap.max_rows = usize::MAX;
            job.wrap.break_anywhere = true;
            let galley = ui.painter().layout_job(job);
            let h = galley.size().y + CODE_PAD * 2.0;
            if let Some((tl, painter)) = paint {
                let bg = Rect::from_min_size(tl, Vec2::new(width, h));
                painter.rect_filled(bg, pal.radius_md, pal.surface_variant);
                painter.galley(pos2(tl.x + CODE_PAD, tl.y + CODE_PAD), galley, pal.text);
            }
            h
        }
        Block::Hr => {
            let h = 7.0;
            if let Some((tl, painter)) = paint {
                painter.hline(
                    tl.x..=tl.x + width,
                    tl.y + h * 0.5,
                    Stroke::new(1.0, pal.border_subtle),
                );
            }
            h
        }
    }
}

/// 把行内片段序列（文本 + 图片）排版并可选绘制，返回总高。
///
/// 与 `layout_block` 同款契约：`paint` 为 `None` 时只测量，绘制与
/// 测量走同一段代码（否则会出现「测量行高算一套、绘制用另一套」的漂移）。
///
/// `dim_text` 为 `true` 时把所有文本段的颜色替换为 `pal.text_dim`——
/// 引用块借这条把整段视觉降一档（图片占位的背景/前景不受影响）。
///
/// 段间留 `BLOCK_GAP * 0.5`——比块间距更紧，因为同一段落里的图片
/// 仍是正文的一部分，不应像独立块那样断开。
// 8 个参数超过了 clippy 默认的 7 个上限：这是 `layout_block` 的内联函数，
// 各项语义独立（UI/Palette/scale/spans/width/body/paint/dim_text），无自然分组；
// 硬塞进结构体反而抬高理解成本，豁免以保留函数语义。
#[allow(clippy::too_many_arguments)]
fn layout_inline_segments(
    ui: &egui::Ui,
    pal: &Palette,
    scale: f32,
    spans: &[Inline],
    width: f32,
    body: f32,
    paint: Option<&(Pos2, egui::Painter)>,
    dim_text: bool,
) -> f32 {
    let line_h = body * 1.45;
    let segments = inline_segments(spans);
    if segments.is_empty() {
        return 0.0;
    }
    let mut y = 0.0;
    for (i, (text_spans, image)) in segments.iter().enumerate() {
        if i > 0 {
            y += BLOCK_GAP * 0.5;
        }
        if let Some((alt, src)) = image {
            let h = IMG_PLACEHOLDER_H;
            if let Some((tl, painter)) = paint {
                let top_left = *tl + Vec2::new(0.0, y);
                paint_inline_image(painter, top_left, width, pal, scale, alt, src);
            }
            y += h;
        } else if !text_spans.is_empty() {
            let mut job = spans_job(text_spans, pal, scale, body, width);
            if dim_text {
                for sec in job.sections.iter_mut() {
                    sec.format.color = pal.text_dim;
                }
            }
            let galley = ui.painter().layout_job(job);
            let h = galley.size().y.max(line_h);
            if let Some((tl, painter)) = paint {
                // 引用 `text_dim` 已经在 spans 上挂了色，此处 galley.color
                // 调为 text_dim 只是兜底（让单段默认为正文色）。
                painter.galley(*tl + Vec2::new(0.0, y), galley, pal.text);
            }
            y += h;
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(spans: &[Inline]) -> String {
        spans.iter().map(span_text).collect()
    }

    #[test]
    fn parses_heading_list_and_quote() {
        let doc = MdDoc::parse("# 标题\n\n- 一\n- 二\n\n> 引用");
        assert_eq!(doc.blocks.len(), 3);
        match &doc.blocks[0] {
            Block::Heading { level, spans } => {
                assert_eq!(*level, 1);
                assert_eq!(spans_text(spans), "标题");
            }
            other => panic!("首块应是标题，实际 {other:?}"),
        }
        match &doc.blocks[1] {
            Block::List { ordered, items } => {
                assert!(!ordered);
                assert_eq!(items.len(), 2);
                assert_eq!(spans_text(&items[0]), "一");
            }
            other => panic!("第二块应是列表，实际 {other:?}"),
        }
        match &doc.blocks[2] {
            Block::Quote(spans) => assert_eq!(spans_text(spans), "引用"),
            other => panic!("第三块应是引用，实际 {other:?}"),
        }
    }

    #[test]
    fn parses_code_fence_and_inline_marks() {
        let doc = MdDoc::parse("```bash\n./build.sh\n```\n\n**粗** 与 *斜* 与 `码` 与 [文](https://x)");
        assert_eq!(doc.blocks.len(), 2);
        match &doc.blocks[0] {
            Block::Code(src) => assert_eq!(src, "./build.sh"),
            other => panic!("首块应是代码块，实际 {other:?}"),
        }
        match &doc.blocks[1] {
            Block::Para(spans) => {
                assert!(spans.contains(&Inline::Strong("粗".into())));
                assert!(spans.contains(&Inline::Em("斜".into())));
                assert!(spans.contains(&Inline::Code("码".into())));
                assert!(spans.contains(&Inline::Link("文".into())));
            }
            other => panic!("第二块应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn unterminated_code_fence_does_not_panic() {
        let doc = MdDoc::parse("```rust\nfn main() {");
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Code(src) => assert_eq!(src, "fn main() {"),
            other => panic!("应是代码块，实际 {other:?}"),
        }
    }

    #[test]
    fn stray_markers_become_plain_text() {
        let doc = MdDoc::parse("这句话有 ** 不成对 和 ` 孤码");
        match &doc.blocks[0] {
            Block::Para(spans) => {
                let text = spans_text(spans);
                assert!(text.contains("**"), "不成对标记应按原文保留：{text}");
                assert!(text.contains('`'), "孤码应按原文保留：{text}");
            }
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn ordered_list_uses_numbers() {
        let doc = MdDoc::parse("1. 第一项\n2. 第二项");
        match &doc.blocks[0] {
            Block::List { ordered, items } => {
                assert!(*ordered);
                assert_eq!(items.len(), 2);
            }
            other => panic!("应是有序列表，实际 {other:?}"),
        }
    }

    #[test]
    fn heading_requires_space_after_hash() {
        // `#tag` 是话题标签不是标题（与捕获层 looks_like_markdown 同判据）。
        let doc = MdDoc::parse("#tag 不是标题");
        match &doc.blocks[0] {
            Block::Para(spans) => assert_eq!(spans_text(spans), "#tag 不是标题"),
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_source_is_empty_doc() {
        assert!(MdDoc::parse("").is_empty());
        assert!(MdDoc::parse("\n\n\n").is_empty());
    }

    #[test]
    fn soft_line_breaks_merge_into_one_paragraph() {
        let doc = MdDoc::parse("第一行\n第二行");
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Para(spans) => assert_eq!(spans_text(spans), "第一行 第二行"),
            other => panic!("应是单段落，实际 {other:?}"),
        }
    }

    // ---- 行内图片 `![alt](src)` ----

    #[test]
    fn parses_inline_image_with_alt_and_src() {
        let doc = MdDoc::parse("![截图](C:/shots/foo.png)");
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Para(spans) => {
                assert_eq!(spans.len(), 1);
                match &spans[0] {
                    Inline::Image { alt, src } => {
                        assert_eq!(alt, "截图");
                        assert_eq!(src, "C:/shots/foo.png");
                    }
                    other => panic!("应是 Inline::Image，实际 {other:?}"),
                }
            }
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn inline_image_mixed_with_strong_and_text() {
        // 图片在段落中间：前后文字 + 强调仍能保留。
        let doc = MdDoc::parse("前置 **粗体** ![alt](logo.png) 后置");
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Para(spans) => {
                // 期望：[Text("前置 "), Strong("粗体"), Text(" "), Image{...}, Text(" 后置")]
                let kinds: Vec<&'static str> = spans
                    .iter()
                    .map(|s| match s {
                        Inline::Text(_) => "text",
                        Inline::Strong(_) => "strong",
                        Inline::Image { .. } => "image",
                        other => panic!("unexpected {other:?}"),
                    })
                    .collect();
                assert_eq!(kinds, ["text", "strong", "text", "image", "text"]);
                if let Inline::Image { alt, src } = &spans[3] {
                    assert_eq!(alt, "alt");
                    assert_eq!(src, "logo.png");
                }
            }
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn inline_image_takes_priority_over_plain_link() {
        // `![alt](url)` 整段必须被识别为图片，**不能**被链接解析器
        // 先吃掉 `[alt]`（否则 URL 会变成孤立的 `(url)` 文本）。
        let doc = MdDoc::parse("![alt](https://example.com/x.png)");
        match &doc.blocks[0] {
            Block::Para(spans) => {
                let has_img = spans.iter().any(|s| matches!(s, Inline::Image { .. }));
                let has_link = spans.iter().any(|s| matches!(s, Inline::Link(_)));
                assert!(has_img, "应识别为图片，实际 {spans:?}");
                assert!(!has_link, "不能被误识别为链接");
            }
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn inline_image_empty_alt_is_preserved() {
        // `![](src)` 是合法 Markdown（纯装饰图），alt 为空串。
        let doc = MdDoc::parse("![](logo.png)");
        match &doc.blocks[0] {
            Block::Para(spans) => match &spans[0] {
                Inline::Image { alt, src } => {
                    assert_eq!(alt, "");
                    assert_eq!(src, "logo.png");
                }
                other => panic!("应是 Inline::Image，实际 {other:?}"),
            },
            other => panic!("应是段落，实际 {other:?}"),
        }
    }

    #[test]
    fn classify_src_handles_url_and_local_path() {
        // 路径类：取末段（文件名）。
        let (k, n) = super::classify_src("C:/shots/sub/foo.png");
        assert_eq!(k, "本地");
        assert_eq!(n, "foo.png");
        // Unix 路径同样支持。
        let (k, n) = super::classify_src("/home/user/x.png");
        assert_eq!(k, "本地");
        assert_eq!(n, "x.png");
        // URL：取域名。
        let (k, n) = super::classify_src("https://example.com/a/b.png");
        assert_eq!(k, "URL");
        assert_eq!(n, "example.com");
        // 空 src。
        let (k, n) = super::classify_src("");
        assert_eq!(k, "无路径");
        assert!(n.is_empty());
        // data URL。
        let (k, _) = super::classify_src("data:image/png;base64,xxx");
        assert_eq!(k, "内嵌");
    }
}
