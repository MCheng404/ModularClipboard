//! 自绘标题栏。
//!
//! # 为什么必须自绘
//!
//! 窗口是 `WS_POPUP` 无边框的（见 `gfx::window`），没有系统标题栏，
//! 于是「拖动窗口」「最小化」「关闭」三件事都没有系统实现，得自己在
//! egui 里画一套。
//!
//! # 拖动区的关键约束
//!
//! 拖动区通过 [`gfx::window::Window::set_drag_region`] 告知外壳，
//! **由窗口过程在 `WM_NCHITTEST` 里命中测试**。落在拖动矩形内的按下会被
//! 系统拿走变成拖窗口，egui 收不到——所以所有按钮**必须画在拖动区之外**，
//! 否则点按钮会变成拖窗口，表现为「按钮毫无反应」。
//!
//! 这条约束在本文件里由 [`drag_region`] 集中保证：它算出拖动区时
//! **扣掉所有按钮矩形**，且有测试固定住「按钮不与拖动区相交」。
//! 绘制层与上报层共用同一个 [`TitlebarLayout::drag`]，不可能不一致。

use egui::{Align2, Color32, CornerRadius, Rect, Stroke, Ui, pos2, vec2};

use crate::icons::Icon;
use crate::theme::{Palette, sized};

/// 标题栏高度（逻辑点）。
///
/// 32 略小于 Windows 的 31——不，比它大一点，因为这里的标题栏同时是
/// 搜索框所在的那一行，需要容纳输入框。
pub const TITLEBAR_HEIGHT: f32 = 36.0;

/// 窗口按钮尺寸（逻辑点）。
pub const BUTTON_SIZE: f32 = 28.0;

/// 关闭按钮宽度（逻辑点）。比其它按钮宽，符合桌面惯例。
pub const CLOSE_BUTTON_WIDTH: f32 = 44.0;

/// 拖动区左右留白（逻辑点）。
///
/// 留一点边距，拖动区才不会紧贴按钮——否则用户很难判断
/// 「这一小块到底能不能拖」。
const DRAG_MARGIN: f32 = 8.0;

/// 标题栏上各元素的矩形。
///
/// 一次性算好、绘制与上报共用，是「按钮不会落进拖动区」的实现基础。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TitlebarLayout {
    /// 整条标题栏。
    pub full: Rect,
    /// 应用标题文本区。
    pub title: Rect,
    /// 搜索框。
    pub search: Rect,
    /// 设置按钮。
    pub settings: Rect,
    /// 最小化按钮。
    pub minimize: Rect,
    /// 关闭按钮。
    pub close: Rect,
    /// 上报给外壳的拖动区。**已扣掉上面所有按钮**。
    pub drag: Rect,
}

impl TitlebarLayout {
    /// 按 `bar` 矩形与缩放计算各元素位置。
    ///
    /// 从右往左摆按钮、往左放标题、中间给搜索框，是标题栏的标准排布。
    pub fn new(bar: Rect, scale: f32) -> Self {
        let btn = BUTTON_SIZE * scale;
        let close_w = CLOSE_BUTTON_WIDTH * scale;
        let inset = DRAG_MARGIN * scale;

        let close = Rect::from_min_size(
            pos2(bar.max.x - close_w, bar.min.y),
            vec2(close_w, bar.height()),
        );
        let minimize = Rect::from_min_size(
            pos2(close.min.x - btn, bar.min.y),
            vec2(btn, bar.height()),
        );
        let settings = Rect::from_min_size(
            pos2(minimize.min.x - btn, bar.min.y),
            vec2(btn, bar.height()),
        );
        // 标题吃掉自己那份，剩下的全给搜索框。
        let title_w = (120.0 * scale).min(bar.width() * 0.3);
        let title = Rect::from_min_size(
            pos2(bar.min.x + inset, bar.min.y),
            vec2(title_w, bar.height()),
        );
        let search_x0 = title.max.x + inset;
        let search = Rect::from_min_size(
            pos2(search_x0, bar.min.y + (bar.height() - btn) / 2.0),
            vec2(
                (settings.min.x - inset - search_x0).max(0.0),
                btn,
            ),
        );
        // 拖动区 = 标题栏扣掉**所有**按钮（含设置）。
        //
        // 这里减去设置按钮是有代价的：设置按钮左侧那段也拖不动窗口了。
        // 但反过来把设置按钮放进拖动区，用户点设置就会变成拖窗口——
        // 后者是不能接受的。
        let drag = Rect::from_min_max(
            pos2(bar.min.x, bar.min.y),
            pos2(settings.min.x, bar.max.y),
        );
        Self {
            full: bar,
            title,
            search,
            settings,
            minimize,
            close,
            drag,
        }
    }

    /// 拖动区（上报给外壳的矩形）。
    pub fn drag_region(&self) -> Rect {
        self.drag
    }

    /// 校验：拖动区不与任何按钮**重叠**。
    ///
    /// 这是本文件最重要的一条：按钮落进拖动区 = 点按钮变成拖窗口。
    ///
    /// 注意用的是「重叠」而非 `Rect::intersects`：后者用 `<=` 比较，
    /// **边界相接也算相交**。而拖动区右缘恰好就是设置按钮的左缘
    /// （这正是本文件里`drag_region_stops_before_settings_button`
    /// 那条测试要求的），用 `intersects` 会永远得到「相交」——
    /// 那样的断言要么恒失败，要么逼人加容差蒙混过去。
    /// 真正要禁止的是**有面积的重叠**，即按钮的面积落进拖动区内。
    pub fn buttons_inside_drag(&self) -> bool {
        let b = [self.settings, self.minimize, self.close];
        b.iter().all(|r| !rects_overlap(self.drag, *r))
    }
}

/// 两个矩形是否有**正面积**重叠。
///
/// 与 `Rect::intersects` 的区别：边界相接不算重叠。
/// 判定「按钮有没有落进拖动区」要的是这个语义——
/// 按钮贴着拖动区的边是正常的布局，只有面积压进去才是 bug。
fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x && b.min.x < a.max.x && a.min.y < b.max.y && b.min.y < a.max.y
}

/// 标题栏上发生的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitlebarAction {
    /// 无。
    #[default]
    None,
    /// 关闭 → **隐藏到托盘**，不是退出。
    Close,
    /// 最小化。
    Minimize,
    /// 打开设置。
    Settings,
}

/// 画标题栏。返回用户触发的动作。
///
/// `query` 是搜索词的**可变引用**：搜索框就地编辑，改动由调用方
/// 决定何时写回业务状态——绘制层不直接改 `Service`，
/// 这样「界面输入」与「业务状态变更」两个关注点不会纠缠。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut Ui,
    bar: Rect,
    pal: &Palette,
    scale: f32,
    query: &mut String,
    app_title: &str,
) -> TitlebarAction {
    let l = TitlebarLayout::new(bar, scale);
    let mut action = TitlebarAction::None;

    // ---- 背景 ----
    // 标题栏底色比窗口底色略亮一档：它与下面的内容区是不同的层。
    ui.painter()
        .rect_filled(bar, CornerRadius::ZERO, pal.surface_variant);
    // 底部分隔线，让标题栏与内容区有一道明确的界。
    ui.painter().hline(
        bar.x_range(),
        bar.max.y,
        Stroke::new(pal.stroke_thin, pal.border_subtle),
    );

    // ---- 标题 ----
    if l.title.width() > 8.0 {
        ui.painter().text(
            l.title.left_center(),
            Align2::LEFT_CENTER,
            app_title,
            sized(pal.font_md, scale),
            pal.text,
        );
    }

    // ---- 搜索框 ----
    //
    // 占满 `l.search`：它与顶栏右侧的设置按钮同在一条线上，
    // 正是线框图里「搜索框 ── 设置」那一行。
    if l.search.width() > 20.0 {
        let r = ui.interact(
            l.search,
            ui.id().with("topbar_search"),
            egui::Sense::click(),
        );
        let focused = r.has_focus() || ui.memory(|m| m.has_focus(ui.id().with("topbar_search")));
        ui.painter().rect_filled(
            l.search,
            CornerRadius::same(pal.radius_md as u8),
            pal.surface,
        );
        ui.painter().rect_stroke(
            l.search,
            CornerRadius::same(pal.radius_md as u8),
            Stroke::new(
                pal.stroke_normal,
                if focused { pal.border_strong } else { pal.border },
            ),
            egui::StrokeKind::Inside,
        );
        // 放大镜
        Icon::Search.paint(
            ui.painter(),
            egui::Rect::from_center_size(
                l.search.left_center() + vec2(10.0, 0.0),
                vec2(pal.icon_size, pal.icon_size),
            ),
            pal.text_dim,
        );
        // 文本
        let text_left = l.search.min.x + 26.0 * scale;
        let text = if query.is_empty() {
            ui.painter().text(
                pos2(text_left, l.search.center().y),
                Align2::LEFT_CENTER,
                "搜索历史…",
                sized(pal.font_md, scale),
                pal.text_dim,
            );
            String::new()
        } else {
            query.clone()
        };
        ui.painter().text(
            pos2(text_left, l.search.center().y),
            Align2::LEFT_CENTER,
            text,
            sized(pal.font_md, scale),
            pal.text,
        );
        if r.clicked() {
            ui.memory_mut(|m| m.request_focus(ui.id().with("topbar_search")));
        }
    }

    // ---- 窗口按钮 ----
    if title_button(ui, l.close, pal, Icon::Close, "关闭（隐藏到托盘）").clicked() {
        action = TitlebarAction::Close;
    }
    if title_button(ui, l.minimize, pal, Icon::Search, "最小化").clicked() {
        action = TitlebarAction::Minimize;
    }
    if title_button(ui, l.settings, pal, Icon::Settings, "设置").clicked() {
        action = TitlebarAction::Settings;
    }

    action
}

/// 标题栏按钮的统一画法。
///
/// 悬停时才显出底色：默认状态下三个按钮是纯图标，
/// 标题栏因此不会被三个色块切成碎块（Windows 11 的做法）。
fn title_button(
    ui: &mut Ui,
    r: Rect,
    pal: &Palette,
    icon: Icon,
    tip: &str,
) -> egui::Response {
    let resp = ui.interact(r, ui.id().with(tip), egui::Sense::click());
    let bg = if resp.is_pointer_button_down_on() {
        pal.row_active
    } else if resp.hovered() {
        pal.row_hover
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        ui.painter().rect_filled(r, CornerRadius::ZERO, bg);
    }
    let color = if resp.hovered() { pal.text_bright } else { pal.text_dim };
    icon.paint(
        ui.painter(),
        egui::Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        color,
    );
    resp.on_hover_text(tip)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, TITLEBAR_HEIGHT))
    }

    #[test]
    fn layout_places_buttons_left_to_right() {
        let l = TitlebarLayout::new(bar(), 1.0);
        // 从右往左：设置 → 最小化 → 关闭
        assert!(l.settings.max.x <= l.minimize.min.x + 0.01, "设置应在最小化左侧");
        assert!(l.minimize.max.x <= l.close.min.x + 0.01, "最小化应在关闭左侧");
    }

    #[test]
    fn close_button_touches_right_edge() {
        let l = TitlebarLayout::new(bar(), 1.0);
        assert!((l.close.max.x - bar().max.x).abs() < 0.01);
    }

    #[test]
    fn drag_region_excludes_every_button() {
        // 这是本文件最重要的一条：按钮落进拖动区 = 点按钮变成拖窗口。
        let l = TitlebarLayout::new(bar(), 1.0);
        assert!(l.buttons_inside_drag(), "拖动区与按钮相交，按钮会失效");
        for r in [l.settings, l.minimize, l.close] {
            assert!(
                !rects_overlap(l.drag, r),
                "按钮 {r:?} 落进了拖动区 {:?}",
                l.drag
            );
        }
    }

    #[test]
    fn drag_region_starts_at_left_edge_and_full_height() {
        let l = TitlebarLayout::new(bar(), 1.0);
        assert!((l.drag.min.x - bar().min.x).abs() < 0.01);
        assert!((l.drag.min.y - bar().min.y).abs() < 0.01);
        assert!((l.drag.max.y - bar().max.y).abs() < 0.01);
    }

    #[test]
    fn drag_region_stops_before_settings_button() {
        let l = TitlebarLayout::new(bar(), 1.0);
        assert!(
            l.drag.max.x <= l.settings.min.x + 0.01,
            "拖动区右边界应止于设置按钮左缘"
        );
    }

    #[test]
    fn buttons_inside_drag_holds_at_higher_scale() {
        // 高 DPI 下按钮与拖动区都会变大，这条必须仍然成立。
        let l = TitlebarLayout::new(bar(), 2.0);
        assert!(l.buttons_inside_drag());
    }

    #[test]
    fn buttons_inside_drag_holds_at_lower_scale() {
        let l = TitlebarLayout::new(bar(), 0.75);
        assert!(l.buttons_inside_drag());
    }

    #[test]
    fn buttons_inside_drag_holds_for_narrow_window() {
        // 窄窗口下搜索框被压扁，但按钮与拖动区的关系不能破。
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, TITLEBAR_HEIGHT));
        let l = TitlebarLayout::new(narrow, 1.0);
        assert!(l.buttons_inside_drag(), "窄窗口下按钮不能落进拖动区");
    }

    #[test]
    fn search_box_precedes_settings() {
        let l = TitlebarLayout::new(bar(), 1.0);
        assert!(l.search.max.x <= l.settings.min.x + 0.01);
        assert!(l.title.max.x <= l.search.min.x + 0.01);
    }

    #[test]
    fn search_box_width_is_nonnegative_when_window_too_narrow() {
        // 窗口窄到放不下时搜索框宽度应为 0 而非负数——绘制层据此跳过。
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(60.0, TITLEBAR_HEIGHT));
        let l = TitlebarLayout::new(tiny, 1.0);
        assert!(l.search.width() >= 0.0, "搜索框宽度不应为负: {:?}", l.search);
        assert!(l.buttons_inside_drag());
    }

    #[test]
    fn titlebar_height_is_touch_friendly() {
        // 小于 28 会让最小化/关闭难以点中。
        assert!(TITLEBAR_HEIGHT >= 28.0, "标题栏高度过小: {TITLEBAR_HEIGHT}");
    }

    #[test]
    fn default_action_is_none() {
        assert_eq!(TitlebarAction::default(), TitlebarAction::None);
    }

    #[test]
    fn action_variants_are_distinct() {
        // 防止 match 漏分支后被「两个变体相等」掩盖。
        assert_ne!(TitlebarAction::Close, TitlebarAction::Minimize);
        assert_ne!(TitlebarAction::Minimize, TitlebarAction::Settings);
        assert_ne!(TitlebarAction::Close, TitlebarAction::Settings);
    }

    /// `Rect::intersects` 用 `<=`，边界相接会算相交。
    /// 这条测试固定住我们用的是「正面积重叠」语义——
    /// 否则按钮紧贴拖动区右缘就会被误判成 bug。
    #[test]
    fn touching_edge_is_not_overlap() {
        let a = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 20.0));
        let b = Rect::from_min_size(pos2(100.0, 0.0), vec2(20.0, 20.0));
        assert!(!rects_overlap(a, b), "边界相接不算重叠");
        // 对照：egui 自己的 intersects 认为相接也是相交。
        assert!(a.intersects(b), "egui 的 intersects 语义不同，属预期");
    }

    #[test]
    fn drag_region_accessor_matches_field() {
        let l = TitlebarLayout::new(bar(), 1.0);
        assert_eq!(l.drag_region(), l.drag);
    }

    #[test]
    fn layout_is_deterministic() {
        let a = TitlebarLayout::new(bar(), 1.0);
        let b = TitlebarLayout::new(bar(), 1.0);
        assert_eq!(a, b, "同一输入必须给出同一布局，否则上报的拖动区会跳");
    }
}