//! Markdown 子集渲染（列表行内嵌显示）。
//!
//! 2026-10-15 第三轮原型移植项（`main-proto/index.html` 的同名功能），
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
    }
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
            let galley = ui.painter().layout_job(spans_job(spans, pal, scale, body, width));
            let h = galley.size().y;
            if let Some((tl, painter)) = paint {
                painter.galley(tl, galley, pal.text);
            }
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
                let galley = ui.painter().layout_job(spans_job(
                    item,
                    pal,
                    scale,
                    body,
                    (width - marker_w).max(20.0),
                ));
                let h = galley.size().y.max(line_h);
                if let Some((tl, painter)) = &paint {
                    let tl = *tl;
                    painter.text(
                        pos2(tl.x, tl.y + y),
                        egui::Align2::LEFT_TOP,
                        marker,
                        egui::FontId::proportional(body * scale),
                        pal.text_dim,
                    );
                    painter.galley(pos2(tl.x + marker_w, tl.y + y), galley, pal.text);
                }
                y += h;
            }
            y
        }
        Block::Quote(spans) => {
            let text_x = QUOTE_BAR + QUOTE_GAP;
            let inner_w = (width - text_x).max(20.0);
            // 引用整体降一档亮度：一次排版，测量与绘制共用。
            let mut job = spans_job(spans, pal, scale, body, inner_w);
            for sec in job.sections.iter_mut() {
                sec.format.color = pal.text_dim;
            }
            let galley = ui.painter().layout_job(job);
            let h = galley.size().y.max(line_h);
            if let Some((tl, painter)) = paint {
                let bar = Rect::from_min_size(tl, Vec2::new(QUOTE_BAR, h));
                painter.rect_filled(bar, 0.0, pal.accent);
                painter.galley(pos2(tl.x + text_x, tl.y), galley, pal.text_dim);
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
}
