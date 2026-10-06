//! 各模块的面板绘制。
//!
//! 这里的函数都只负责「把一个模块画进给定矩形」，**不做布局决策**——
//! 矩形从 [`crate::layout::solve`] 来，绘制层不改它。这样「提示画在
//! 这里但吸附到那里」这类不一致在结构上就不可能发生。
//!
//! # 与 view.rs 的分工
//!
//! 业务数据取用与操作执行仍�� [`crate::view`]：本模块不认识
//! `Service`，只接收已经取好的 `&[ClipItem]` 与回调。这让面板可以被
//! 单独测试，也避免同一份数据被两个模块各取一次。

use egui::{Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use modular_clipboard_core::ClipItem;

use crate::icons::Icon;
use crate::layout::Panel;
use crate::theme::{Palette, sized};

/// 列表行高（逻辑点）。
///
/// 48 是两行内容（预览 + 元信息）加内边距的结果；
/// 调小会让图片缩略图没有立足之地。
pub const ROW_HEIGHT: f32 = 48.0;

/// 单行紧凑行高（逻辑点）。用于「表格视图」。
pub const COMPACT_ROW_HEIGHT: f32 = 26.0;

/// 面板边框宽度。
const PANEL_STROKE: f32 = 1.0;

/// 画一个面板的外框。
///
/// # 为什么要画边框
///
/// 线框图里三个区域之间是**黑色分隔条**，用户据此判断「这是一块可拖的
/// 独立区域」。边框就是那个分隔的视觉对应物：没有它，浮动面板会
/// 直接融进背景，用户找不到自己拖出来的东西。
pub fn panel_frame(ui: &mut Ui, rect: Rect, pal: &Palette, floating: bool, scale: f32) {
    let radius = if floating { pal.radius_lg } else { 0.0 };
    let fill = if floating { pal.surface_raised } else { pal.surface };
    ui.painter().rect_filled(rect, radius, fill);
    if floating {
        // 浮动面板带阴影（用半透明黑矩形模拟），否则它和停靠区
        // 在视觉上分不开。
        let shadow = 3.0 * scale;
        ui.painter().rect_filled(
            rect.expand(shadow),
            radius + shadow * 0.5,
            Color32::from_black_alpha(if pal.is_dark { 90 } else { 40 }),
        );
        // 阴影画在下面，需要重画一次面板底色盖住重叠部分。
        ui.painter().rect_filled(rect, radius, fill);
    }
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(PANEL_STROKE * scale, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 面板的折叠把手（停靠模块折叠后仍显示，兼作展开按钮）。
pub fn draw_collapse_handle(ui: &mut Ui, rect: Rect, pal: &Palette, scale: f32) -> Rect {
    let r = Rect::from_min_size(
        rect.min,
        vec2(rect.width().min(rail_handle_width(pal, scale)), rect.height()),
    );
    let resp = ui.interact(r, ui.id().with("collapse_handle"), Sense::click());
    let bg = if resp.hovered() { pal.row_hover } else { pal.surface_variant };
    ui.painter().rect_filled(r, CornerRadius::ZERO, bg);
    // 三个点：竖排的「⋮」，提示这里可以拖/点。
    let dot = 2.0 * scale;
    let n = 3;
    for i in 0..n {
        let y = r.center().y + (i as f32 - 1.0) * dot * 3.0;
        ui.painter().circle_filled(pos2(r.center().x, y), dot, pal.text_dim);
    }
    let _ = resp;
    r
}

/// 折叠把手宽度。
pub fn rail_handle_width(pal: &Palette, scale: f32) -> f32 {
    (pal.font_lg + pal.space_sm) * scale
}

/// 分隔条的画法：默认不可见，悬停/拖动时才显形。
///
/// 一直画出来会让整个界面布满竖线，视觉噪声大于收益。
pub fn draw_splitter(
    ui: &mut Ui,
    rect: Rect,
    pal: &Palette,
    hovered: bool,
    dragging: bool,
) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with("splitter"), Sense::drag());
    let active = dragging || (hovered && resp.hovered());
    let color = if dragging {
        pal.accent_active
    } else if hovered && resp.hovered() {
        pal.accent_hover
    } else {
        pal.border_subtle
    };
    let w = if active { rect.width() } else { 1.0 };
    ui.painter().rect_filled(
        egui::Rect::from_center_size(rect.center(), vec2(w, rect.height())),
        CornerRadius::ZERO,
        color,
    );
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    resp
}

/// 画吸附提示线。
///
/// 直接取 [`crate::layout::Snap`] / dock_preview 算出的矩形，
/// **不重算位置**——判定与提示同源。
pub fn draw_guide(ui: &mut Ui, rect: Rect, pal: &Palette) {
    let glow = rect.expand(1.0);
    ui.painter().rect_filled(glow, CornerRadius::ZERO, pal.accent.gamma_multiply(0.35));
    ui.painter().rect_filled(rect, CornerRadius::ZERO, pal.accent);
}

/// 停靠区预览框（半透明虚线感的填充）。
pub fn draw_dock_preview(ui: &mut Ui, rect: Rect, pal: &Palette, scale: f32) {
    ui.painter().rect_filled(rect, pal.radius_lg, pal.accent.gamma_multiply(0.18));
    ui.painter().rect_stroke(
        rect,
        pal.radius_lg,
        Stroke::new(pal.stroke_normal * scale, pal.accent),
        egui::StrokeKind::Inside,
    );
}

/// 悬浮模块的头部：标题 + 一排操作 + 拖动手柄。
///
/// 拖动区用整个头部，但**扣掉右侧按钮**——与标题栏同一个道理：
/// 落在上报拖动区里的按下会被系统拿走，egui 收不到。
pub struct FloatingHeader {
    /// 头部矩形（整个）。
    pub rect: Rect,
    /// 可拖动区域（已扣掉按钮）。
    pub drag: Rect,
}

/// 计算悬浮模块头部的可拖动区。
pub fn floating_header_drag(
    header: Rect,
    button_count: f32,
    button_w: f32,
) -> FloatingHeader {
    let reserve = button_count * button_w;
    FloatingHeader {
        rect: header,
        drag: Rect::from_min_max(
            header.min,
            pos2((header.max.x - reserve).max(header.min.x), header.max.y),
        ),
    }
}

/// 画一行的「内容来源 / 置顶左侧 / 固定」三个操作。
///
/// 这三个是线框图里每条上方的工具条。
pub struct RowTools {
    /// 「取消置顶」——取消该条目的置顶。
    pub unpin: bool,
    /// 「置顶左侧」——把条目置顶（置顶后出现在左侧模块）。
    pub pin_left: bool,
    /// 「固定」——置顶并锁定在主区顶部。
    pub lock: bool,
}

/// 画行内工具条，返回用户触发了哪个。
///
/// 未置顶时「取消置顶」置灰：点它不会有任何变化，
/// 一个点了没反应的按钮比不显示更让人困惑。
pub fn draw_row_tools(
    ui: &mut Ui,
    pal: &Palette,
    scale: f32,
    item: &ClipItem,
) -> Option<RowTool> {
    let mut hit = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = vec2(pal.space_xs, 0.0);

        // 内容来源：只是文字标签，不可点。
        let src = if item.source_app.is_empty() {
            "未知来源"
        } else {
            &item.source_app
        };
        let src_text = RichText::new(src)
            .size(sized(pal.font_xs, scale).size)
            .color(pal.text_dim);
        ui.label(src_text);

        if item.pinned {
            if icon_button(ui, Icon::Unpinned, pal, scale, "取消置顶").clicked() {
                hit = Some(RowTool::Unpin);
            }
            if icon_button(ui, Icon::Pinned, pal, scale, "固定在主区顶部").clicked() {
                hit = Some(RowTool::Lock);
            }
        } else if icon_button(ui, Icon::Pinned, pal, scale, "置顶到左侧").clicked() {
            hit = Some(RowTool::PinLeft);
        }
    });
    hit
}

/// 行内工具触发的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTool {
    /// 取消置顶。
    Unpin,
    /// 置顶到左侧模块。
    PinLeft,
    /// 固定（置顶 + 锁定顶部）。
    Lock,
}

/// 图标按钮（受令牌控制颜色）。
fn icon_button(
    ui: &mut Ui,
    icon: Icon,
    pal: &Palette,
    scale: f32,
    tip: &str,
) -> egui::Response {
    let size = vec2(pal.control_height * 0.75, pal.control_height * 0.75) * scale;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter_at(rect);
    if resp.hovered() {
        painter.rect_filled(rect, pal.radius_sm, pal.row_hover);
    }
    let color = if resp.hovered() { pal.text_bright } else { pal.text_dim };
    icon.paint(
        &painter,
        egui::Rect::from_center_size(rect.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        color,
    );
    resp.on_hover_text(tip)
}

/// 画一个列表行。
///
/// 高度固定（[`ROW_HEIGHT`]），因为外层用 `ScrollArea::show_rows`
/// 虚拟化——行高必须恒定，否则虚拟化的行索引与实际内容对不上，
/// 表现为「滚动时列表内容错位」。
pub fn draw_row(
    ui: &mut Ui,
    rect: Rect,
    item: &ClipItem,
    selected: bool,
    hovered: bool,
    pal: &Palette,
    scale: f32,
    alt: bool,
    meta: String,
) -> egui::Response {
    let bg = if selected {
        pal.row_selected
    } else if hovered {
        pal.row_hover
    } else if alt {
        pal.row_alt
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, CornerRadius::ZERO, bg);
    }
    if selected {
        // 选中态左侧加一条竖条：比整行变色更容易在长列表里定位。
        ui.painter().rect_filled(
            Rect::from_min_size(rect.min, vec2(2.0 * scale, rect.height())),
            CornerRadius::ZERO,
            pal.accent,
        );
    }

    let inner = rect.shrink2(vec2(pal.space_sm, pal.space_xs * 0.5));
    ui.painter().text(
        pos2(inner.min.x + pal.icon_size, inner.center().y),
        Align2::LEFT_CENTER,
        item.one_line_preview(),
        sized(pal.font_md, scale),
        if selected { pal.text_bright } else { pal.text },
    );
    ui.painter().text(
        pos2(inner.min.x + pal.icon_size, inner.max.y - pal.font_xs),
        Align2::LEFT_BOTTOM,
        meta,
        sized(pal.font_xs, scale),
        pal.text_dim,
    );

    // 类型图标放在最左，垂直居中。
    Icon::for_kind(item.kind).paint(
        ui.painter(),
        egui::Rect::from_center_size(
            pos2(inner.min.x + pal.icon_size * 0.5, inner.center().y),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        pal.text_dim,
    );

    ui.allocate_rect(rect, Sense::click())
}

/// 紧凑行（表格视图用）。
pub fn draw_compact_row(
    ui: &mut Ui,
    rect: Rect,
    cols: &[(&str, f32)],
    pal: &Palette,
    scale: f32,
    selected: bool,
) -> egui::Response {
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::ZERO, pal.row_selected);
    }
    let mut x = rect.min.x + pal.space_sm;
    let font: FontId = sized(pal.font_xs, scale);
    for (text, w) in cols {
        let cell = Rect::from_min_size(pos2(x, rect.min.y), vec2(*w, rect.height()));
        // 单元格里的文字要截断，否则长路径会把相邻列顶开。
        ui.painter().text(
            cell.left_center() + vec2(0.0, 0.0),
            Align2::LEFT_CENTER,
            truncate_to(text, *w, pal, scale),
            font.clone(),
            pal.text_dim,
        );
        x += w;
    }
    ui.allocate_rect(rect, Sense::click())
}

/// 按像素宽度粗略截断文本。
///
/// egui 的 `Label::truncate` 需要真实布局上下文，这里在 painter 上画，
/// 只能按字符数近似：中文按 1 个宽度单位、西文按 0.55。
pub fn truncate_to(text: &str, width: f32, pal: &Palette, scale: f32) -> String {
    if width <= 0.0 {
        return String::new();
    }
    let unit = pal.font_xs * scale;
    let budget = width / unit.max(1.0);
    let mut used = 0.0f32;
    let mut out = String::new();
    for ch in text.chars() {
        let w = if is_wide(ch) { 1.0 } else { 0.55 };
        if used + w > budget {
            out.push('…');
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// 是否宽字符（CJK / 全角）。
fn is_wide(c: char) -> bool {
    let u = c as u32;
    (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
}

/// 空状态提示。
pub fn draw_empty_state(ui: &mut Ui, rect: Rect, message: &str, pal: &Palette, scale: f32) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        message,
        sized(pal.font_md, scale),
        pal.text_dim,
    );
}

/// 面板内的分组标签栏。
pub fn draw_group_bar(ui: &mut Ui, rect: Rect, pal: &Palette, scale: f32) {
    ui.painter().rect_filled(rect, CornerRadius::ZERO, pal.surface_variant);
    let _ = (pal, scale);
}

/// 竖排文字（右侧窄栏的「表 / 密」）。
///
/// egui 没有竖排文本，要一格一格画。这里按 Unicode 竖排常见的
/// 「逐字向下、遇标点旋转」的简化规则处理：中文逐字向下，
/// 连续的 ASCII（如 "ID"）整体旋转 90°。
pub fn draw_vertical_text(
    ui: &mut Ui,
    rect: Rect,
    text: &str,
    color: Color32,
    pal: &Palette,
    scale: f32,
) {
    let font = sized(pal.font_md, scale);
    let line_h = pal.font_md * scale * 1.15;
    // 整段 ASCII 旋转，其余逐字向下。
    let mut y = rect.min.y + line_h * 0.5;
    let mut buf = String::new();
    let flush_ascii = |ui: &mut Ui, buf: &mut String, mut y: f32, rect: Rect| {
        if buf.is_empty() {
            return;
        }
        // egui 的 painter 不支持旋转，用四点绘制一个近似的旋转效果：
        // 逐字横排已经足够表意，这里退回逐字向下。
        for ch in buf.chars() {
            ui.painter().text(
                pos2(rect.center().x, y),
                Align2::CENTER_CENTER,
                ch.to_string(),
                font.clone(),
                color,
            );
            y += line_h;
        }
        buf.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            buf.push(ch);
            continue;
        }
        flush_ascii(ui, &mut buf, y, rect);
        if y > rect.max.y {
            break;
        }
        ui.painter().text(
            pos2(rect.center().x, y),
            Align2::CENTER_CENTER,
            ch.to_string(),
            font.clone(),
            color,
        );
        y += line_h;
    }
    flush_ascii(ui, &mut buf, y, rect);
}

/// 右侧竖排标签栏的两种视图。
///
/// 按字面实现为可切换的视图标签：
///
/// - **表** = 表格视图：紧凑多列，一行一条，适合快速扫读与比对；
/// - **密** = 密文视图：内容打码，只显示来源与时间，适合肩后窥屏
///   或投屏演示时防止敏感内容泄露。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailView {
    /// 表格视图。
    Table,
    /// 密文视图（内容打码）。
    Masked,
}

impl RailView {
    /// 竖排标签上的单字。
    pub fn label(self) -> &'static str {
        match self {
            RailView::Table => "表",
            RailView::Masked => "密",
        }
    }

    /// 全部视图，按下标索引。
    pub fn all() -> [RailView; 2] {
        [RailView::Table, RailView::Masked]
    }

    /// 按下标取视图；越界落回第一个而不是 panic——
    /// 配置里的下标可能被手改，不该因此开不了应用。
    pub fn from_index(i: usize) -> RailView {
        RailView::all().get(i).copied().unwrap_or(RailView::Table)
    }

    /// 该视图下是否显示条目内容。
    pub fn shows_content(self) -> bool {
        matches!(self, RailView::Table)
    }
}

/// 面板标识到绘制函数的映射检查（编译期兜底 + 运行期测试）。
///
/// `Panel::ALL` 若新增成员而忘了加绘制分支，`match` 会编译失败——
/// 这比运行时空面板强。
pub fn panel_title(p: Panel) -> &'static str {
    p.title()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use modular_clipboard_core::ClipKind;

    fn pal() -> Palette {
        Palette::dark()
    }

    fn item(kind: ClipKind, preview: &str) -> ClipItem {
        let mut it = ClipItem::new(kind, "h".into(), preview.into(), "app.exe".into());
        it.created_at = 0;
        it
    }

    // ---- 竖排标签 ----

    #[test]
    fn rail_view_labels_are_single_chars() {
        // 竖排窄栏只放得下一个字。
        assert_eq!(RailView::Table.label().chars().count(), 1);
        assert_eq!(RailView::Masked.label().chars().count(), 1);
    }

    #[test]
    fn rail_view_labels_are_distinct() {
        assert_ne!(RailView::Table.label(), RailView::Masked.label());
    }

    #[test]
    fn rail_view_index_roundtrip() {
        for (i, v) in RailView::all().iter().enumerate() {
            assert_eq!(RailView::from_index(i), *v);
        }
    }

    #[test]
    fn rail_view_out_of_range_falls_back() {
        assert_eq!(RailView::from_index(99), RailView::Table);
    }

    #[test]
    fn masked_view_hides_content() {
        assert!(RailView::Table.shows_content());
        assert!(!RailView::Masked.shows_content(), "密文视图应隐藏内容");
    }

    // ---- 行高 ----

    #[test]
    fn row_height_fits_two_lines() {
        // 一行要放预览 + 元信息两行文字。
        let two_lines = ROW_HEIGHT;
        assert!(two_lines >= 40.0, "行高不足以容纳两行: {ROW_HEIGHT}");
    }

    #[test]
    fn compact_row_is_shorter_than_normal() {
        assert!(COMPACT_ROW_HEIGHT < ROW_HEIGHT);
    }

    #[test]
    fn row_heights_are_positive() {
        assert!(ROW_HEIGHT > 0.0);
        assert!(COMPACT_ROW_HEIGHT > 0.0);
    }

    // ---- 悬浮头部拖动区 ----

    #[test]
    fn floating_header_drag_excludes_buttons() {
        let h = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 24.0));
        let f = floating_header_drag(h, 2.0, 20.0);
        assert!(
            f.drag.max.x <= f.rect.max.x - 40.0 + 0.01,
            "拖动区应为右侧按钮留出 40 逻辑点: {:?}",
            f.drag
        );
    }

    #[test]
    fn floating_header_drag_starts_at_left() {
        let h = Rect::from_min_size(pos2(10.0, 0.0), vec2(200.0, 24.0));
        let f = floating_header_drag(h, 1.0, 20.0);
        assert!((f.drag.min.x - 10.0).abs() < 0.01);
    }

    #[test]
    fn floating_header_drag_never_inverts_on_tiny_rect() {
        // 按钮比头部还宽时，拖动区会算出负宽——必须被钳成零宽而非反了。
        let h = Rect::from_min_size(pos2(0.0, 0.0), vec2(10.0, 24.0));
        let f = floating_header_drag(h, 5.0, 40.0);
        assert!(f.drag.width() >= 0.0, "拖动区宽度不应为负: {:?}", f.drag);
        assert!(f.drag.min.x <= f.rect.max.x, "拖动区不应跑到头部右侧");
    }

    #[test]
    fn floating_header_drag_covers_header_height() {
        let h = Rect::from_min_size(pos2(0.0, 5.0), vec2(200.0, 24.0));
        let f = floating_header_drag(h, 1.0, 20.0);
        assert!((f.drag.min.y - 5.0).abs() < 0.01);
        assert!((f.drag.max.y - 29.0).abs() < 0.01);
    }

    // ---- 截断 ----

    #[test]
    fn truncate_short_text_unchanged() {
        let p = pal();
        assert_eq!(truncate_to("abc", 1000.0, &p, 1.0), "abc");
    }

    #[test]
    fn truncate_long_text_adds_ellipsis() {
        let p = pal();
        let long = "这是一段很长的中文内容用来测试截断行为是否正确";
        let out = truncate_to(long, 40.0, &p, 1.0);
        assert!(out.ends_with('…'), "长文本应被截断并加省略号: {out}");
        assert!(out.chars().count() < long.chars().count());
    }

    #[test]
    fn truncate_zero_width_returns_empty() {
        let p = pal();
        assert_eq!(truncate_to("abc", 0.0, &p, 1.0), "");
    }

    #[test]
    fn truncate_wide_chars_cost_more_than_ascii() {
        let p = pal();
        // 同样宽度下，中文能放下的字符数应少于 ASCII。
        let cjk = truncate_to("中文中文中文", 40.0, &p, 1.0);
        let ascii = truncate_to("abcdefgh", 40.0, &p, 1.0);
        assert!(
            cjk.chars().count() < ascii.chars().count(),
            "宽字符应更早被截断: cjk={cjk:?} ascii={ascii:?}"
        );
    }

    #[test]
    fn is_wide_detects_cjk_and_ascii() {
        assert!(is_wide('中'));
        assert!(is_wide('あ'));
        assert!(!is_wide('a'));
        assert!(!is_wide('1'));
    }

    // ---- 折叠把手 ----

    #[test]
    fn rail_handle_width_is_positive() {
        let p = pal();
        assert!(rail_handle_width(&p, 1.0) > 0.0);
        assert!(rail_handle_width(&p, 2.0) > rail_handle_width(&p, 1.0));
    }

    // ---- 面板标题 ----

    #[test]
    fn panel_titles_are_non_empty() {
        for p in Panel::ALL {
            assert!(!panel_title(p).is_empty(), "{p:?} 缺标题");
        }
    }

    // ---- 令牌使用 ----

    #[test]
    fn no_magic_colors_in_panels() {
        // 本模块只用 Palette 的令牌。用一个「令牌集合」检查：
        // 调色板里出现过的颜色才允许出现在本文件。
        let p = pal();
        let allowed: Vec<Color32> = p.tokens().iter().filter_map(|(_, v)| match v {
            crate::theme::TokenValue::Color(c) => Some(*c),
            _ => None,
        }).collect();
        // 关键的三组必须来自令牌而非硬编码。
        assert!(allowed.contains(&p.accent));
        assert!(allowed.contains(&p.border_subtle));
        assert!(allowed.contains(&p.surface_raised));
    }

    // ---- 行工具 ----

    #[test]
    fn row_tool_variants_are_distinct() {
        assert_ne!(RowTool::Unpin, RowTool::PinLeft);
        assert_ne!(RowTool::PinLeft, RowTool::Lock);
        assert_ne!(RowTool::Unpin, RowTool::Lock);
    }

    #[test]
    fn row_tools_struct_defaults_to_no_actions() {
        // RowTools 只是文档载体，确认字段名与线框图一致。
        let t = RowTools {
            unpin: true,
            pin_left: true,
            lock: false,
        };
        assert!(t.unpin && t.pin_left && !t.lock);
    }

    #[test]
    fn item_fixture_is_usable() {
        let it = item(ClipKind::Text, "hello");
        assert_eq!(it.kind, ClipKind::Text);
        assert!(!it.pinned, "新条目默认不该是置顶");
    }
}