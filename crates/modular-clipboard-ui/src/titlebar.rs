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

/// 标题与搜索框之间的最小间隙（逻辑点）。
///
/// 两者矩形不相交还不够：靠得太近时两段文字视觉上会黏成一句。
/// 实测 8pt 间距在这个字号下已经偏挤，取 12pt。
const TITLE_GAP: f32 = 12.0;

/// 标题最多占整条标题栏的宽度比例。
///
/// 标题再长也不能把搜索框挤没——搜索框是这个窗口的主要交互入口。
const TITLE_MAX_FRACTION: f32 = 0.34;

/// 搜索框内文字区相对框边的内缩（逻辑点）。
///
/// 左边让开放大镜（`icons::ICON_INSET` 那一条），右边留一点呼吸。
const SEARCH_TEXT_INSET: f32 = 14.0;

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
    /// 搜索框里**真正可画文字**的那一小块。
    ///
    /// 放大镜占掉左侧一小条，文字必须从它右边开始；右边还要留一点
    /// 内边距，否则最后一个字会贴着框线。文字宽度按这个矩形算，
    /// 不是按整个 `search` 算。
    pub search_text: Rect,
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
    /// 按 `bar` 矩形、缩放与**实测**标题宽度计算各元素位置。
    ///
    /// 从右往左摆按钮、往左放标题、中间给搜索框，是标题栏的标准排布。
    ///
    /// # `title_text_w` 必须是实测值
    ///
    /// 早先这里写死 `120.0 * scale`，于是标题「模块化剪切板」的真实宽度
    /// （6 个汉字 × 14pt ≈ 84pt）与假定值不符时，文字会**画到搜索框上**——
    /// 实机截图里两者糊成一团就是这个原因。字形宽度只有 egui 知道
    /// （[`crate::titlebar::measure_text`]），所以由调用方测好传进来。
    ///
    /// 标题宽度仍会被 [`TITLE_MAX_FRACTION`] 夹住：标题再长也不能把
    /// 搜索框挤没——搜索框是这个窗口的主要交互入口。
    pub fn new(bar: Rect, scale: f32, title_text_w: f32) -> Self {
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
        // 标题吃掉自己那份（实测宽度，上限见文档），剩下的全给搜索框。
        // 这里的 `title_text_w` 已含字形真实宽度，`title` 只比它宽一点留呼吸。
        let title_limit = bar.width() * TITLE_MAX_FRACTION;
        let title_w = title_text_w
            .max(0.0)
            .min(title_limit)
            .max(inset);
        let title = Rect::from_min_size(
            pos2(bar.min.x + inset, bar.min.y),
            vec2(title_w, bar.height()),
        );
        // 标题与搜索框之间必须留缝：贴在一起时即使不重叠，
        // 两段文字也会视觉上黏成一句（实测 8pt 间距看着像 0）。
        let search_x0 = title.max.x + inset.max(TITLE_GAP);
        let search_w = (settings.min.x - inset - search_x0).max(0.0);
        let search = Rect::from_min_size(
            pos2(search_x0, bar.min.y + (bar.height() - btn) / 2.0),
            vec2(search_w, btn),
        );
        // 文字区：左边让开放大镜，右边留一点内边距。
        //
        // ⚠️ 框被压得比两个内缩还窄时（极窄窗口），`from_min_max` 会得到
        // **负宽度**的矩形——那会让下游的截断预算与绘制全部拿到负数。
        // 这里显式夹成非负：宽度为 0 时绘制层自然跳过。
        let text_inset = SEARCH_TEXT_INSET * scale;
        let text_lo = search.min.x + text_inset;
        let text_hi = (search.max.x - text_inset).max(text_lo);
        let search_text = Rect::from_min_max(
            pos2(text_lo, search.min.y),
            pos2(text_hi, search.max.y),
        );
        // 拖动区 = 标题栏扣掉**所有**交互控件。
        //
        // ⚠️ 右界必须取 `search.min.x - gap`，**不是** `settings.min.x`。
        //
        // 实测踩坑：早先取 `settings.min.x`，于是拖动区是 `[0, 180]`，
        // 而搜索框在 `[104, 172]` —— **搜索框 100% 落在拖动区内**。
        // `chrome::hit_test` 命中拖动区就返回 `HTCAPTION`，
        // `WM_LBUTTONDOWN` 被系统接管成拖窗口，**egui 根本收不到这次点击**。
        // 症状是「点搜索框没反应、点别处反而触发了搜索框」——
        // 用户看到的「点击位置与渲染位置不对」，根因在这里，
        // 与坐标换算无关（那条链路实测差值 0.0000 物理像素）。
        //
        // 代价：标题与搜索框之间那一小段也拖不动窗口了。
        // 反过来把搜索框放进拖动区，用户点搜索框就变成拖窗口——
        // 后者是不能接受的。
        // ⚠️ 极窄窗口下按钮会从右往左溢出，最左那个（设置）可能落到
        // `bar.min.x` 之外甚至为负。此时拖动区右界必须再夹一次，
        // 否则「拖动区右界 = search.min.x - inset」算出的值可能
        // 大于最左按钮的左缘，把按钮圈进拖动区。
        let leftmost_btn = self_leftmost_button(settings, minimize, close);
        let drag_right = ((search.min.x - inset).max(bar.min.x)).min(leftmost_btn);
        let drag = Rect::from_min_max(
            pos2(bar.min.x, bar.min.y),
            pos2(drag_right, bar.max.y),
        );
        Self {
            full: bar,
            title,
            search,
            search_text,
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

    /// 校验：拖动区不与任何**交互控件**重叠。
    ///
    /// 这是本文件最重要的一条：控件落进拖动区 = 点它变成拖窗口。
    ///
    /// ⚠️ 检查对象必须包含**搜索框**，不能只查按钮。早先只查
    /// `[settings, minimize, close]` 三个按钮，搜索框漏在检查之外，
    /// 而它恰恰是标题栏里唯一的输入控件、也是最常被点的那个。
    /// 这就是「点搜索框没反应、点别处却触发了搜索框」的成因。
    ///
    /// 注意用的是「重叠」而非 `Rect::intersects`：后者用 `<=` 比较，
    /// **边界相接也算相交**。而拖动区右缘本就该紧贴搜索框左缘
    /// （这正是 `drag_region_stops_before_search` 那条测试要求的），
    /// 用 `intersects` 会永远得到「相交」——那样的断言要么恒失败，
    /// 要么逼人加容差蒙混过去。真正要禁止的是**有面积的重叠**。
    pub fn buttons_inside_drag(&self) -> bool {
        [self.search, self.settings, self.minimize, self.close]
            .iter()
            .all(|r| !rects_overlap(self.drag, *r))
    }

    /// 拖动区是否有正面积落在搜索框里。
    ///
    /// 单独给出是因为它是最严重的那个：搜索框是唯一的输入控件，
    /// 被吞掉等于「搜索功能完全不可用」，且用户会误以为是坐标错位。
    pub fn search_inside_drag(&self) -> bool {
        rects_overlap(self.drag, self.search)
    }
}

/// 三个窗口按钮里最靠左的那个的左缘。
///
/// 极窄窗口下按钮从右往左排会溢出，最左的设置按钮可能越过 `bar.min.x`
/// 甚至变成负值。拖动区右界必须夹在它左缘之内，否则按钮会被圈进
/// 拖动区——点设置按钮变成拖窗口。
fn self_leftmost_button(settings: Rect, minimize: Rect, close: Rect) -> f32 {
    settings.min.x.min(minimize.min.x).min(close.min.x)
}

/// 两个矩形是否有**正面积**重叠。
///
/// 与 `Rect::intersects` 的区别：边界相接不算重叠。
/// 判定「按钮有没有落进拖动区」要的是这个语义——
/// 按钮贴着拖动区的边是正常的布局，只有面积压进去才是 bug。
fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x && b.min.x < a.max.x && a.min.y < b.max.y && b.min.y < a.max.y
}

/// 实测一段文本在给定字号下的宽度（逻辑点）。
///
/// **必须用它而不是估算**：中英文混排的宽度只能由字体度量得出，
/// 按「汉字算 1、西文算 0.55」估会在真实字体下差好几个点——
/// 而标题栏的间距本来就只有几个点，估错就直接压到搜索框上。
pub fn measure_text(ui: &Ui, text: &str, font: &egui::FontId) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    ui.painter()
        .layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
        .rect
        .width()
}

/// 把文本截断到 `max_w` 之内，超出部分用 `…` 收尾。
///
/// egui 自带的 `Label::truncate` 需要真实的布局上下文（要占一个 `Ui`），
/// 而这里是在 painter 上按绝对坐标画字，只能自己按实测宽度裁。
///
/// 按**实测**宽度逐字累加，而不是估算：与 [`measure_text`] 同源，
/// 保证「裁完一定放得下」。
pub fn elide_text(ui: &Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if text.is_empty() || max_w <= 0.0 {
        return String::new();
    }
    if measure_text(ui, text, font) <= max_w {
        return text.to_owned();
    }
    // 省略号本身也要占宽度，所以先给 ellipsis 留出预算再逐字加。
    let ellipsis = "…";
    let ellipsis_w = measure_text(ui, ellipsis, font);
    if ellipsis_w > max_w {
        // 连省略号都放不下：一个字都不给，返回空串而不是画出半截。
        return String::new();
    }
    let mut budget = max_w - ellipsis_w;
    let mut out = String::new();
    for ch in text.chars() {
        let w = measure_text(ui, &ch.to_string(), font);
        if w > budget {
            break;
        }
        budget -= w;
        out.push(ch);
    }
    out.push_str(ellipsis);
    out
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
    let title_font = sized(pal.font_md, scale);
    // 标题宽度实测后送进布局：字形多宽就占多宽，不再靠常数猜。
    let title_text_w = measure_text(ui, app_title, &title_font);
    let l = TitlebarLayout::new(bar, scale, title_text_w);
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
    //
    // 画在 `l.title` 内并按实测宽度截断：标题 rect 是布局分给它的**上限**，
    // 真文字比上限长时（字体回退、字号调大）必须裁，否则会压到搜索框上。
    if l.title.width() > 8.0 {
        ui.painter_at(l.title).text(
            l.title.left_center(),
            Align2::LEFT_CENTER,
            elide_text(ui, app_title, &title_font, l.title.width()),
            title_font,
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
        // 放大镜：坐在 `search_text` 左边那条空隙里，与文字左边界对齐。
        Icon::Search.paint(
            ui.painter(),
            egui::Rect::from_center_size(
                pos2(
                    l.search.min.x + SEARCH_TEXT_INSET * scale * 0.5,
                    l.search.center().y,
                ),
                vec2(pal.icon_size, pal.icon_size),
            ),
            pal.text_dim,
        );
        // 文本。**必须按 `search_text` 的宽度截断**：
        // placeholder「搜索历史…」在 72pt 宽的框里放不下，
        // 不裁就会画到框外、压住右侧的设置/最小化/关闭按钮
        // （实机截图里正是这样糊成一片）。
        let text = if query.is_empty() {
            elide_text(ui, "搜索历史…", &sized(pal.font_md, scale), l.search_text.width())
        } else {
            elide_text(ui, query, &sized(pal.font_md, scale), l.search_text.width())
        };
        if !text.is_empty() {
            ui.painter_at(l.search_text).text(
                pos2(l.search_text.min.x, l.search.center().y),
                Align2::LEFT_CENTER,
                text,
                sized(pal.font_md, scale),
                if query.is_empty() { pal.text_dim } else { pal.text },
            );
        }
        if r.clicked() {
            ui.memory_mut(|m| m.request_focus(ui.id().with("topbar_search")));
        }
    }

    // ---- 窗口按钮 ----
    if title_button(ui, l.close, pal, Icon::Close, "关闭（隐藏到托盘）").clicked() {
        action = TitlebarAction::Close;
    }
    if title_button(ui, l.minimize, pal, Icon::Minimize, "最小化").clicked() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个跑真实布局的 `Ui`：装上真实中文字体，
    /// 这样 [`measure_text`] 走的是 egui 真正的字体度量，
    /// 而不是「假设每个汉字都是 1.0 宽」这种估算。
    ///
    /// ⚠️ 这条对判据至关重要：本项目吃过一次「数字过了但用户看到重影」的亏，
    /// 根因就是判据用了估算宽度。标题栏的间隙只有几个点，
    /// 估算误差恰好能把它吃掉。
    fn layout_ui<R>(add_contents: impl FnMut(&mut Ui) -> R) -> R {
        let ctx = egui::Context::default();
        // 装字体失败（CI 上没装 msyh.ttc）时退回 egui 默认字体：
        // 默认字体对汉字是豆腐块但**有确定宽度**，判据依然成立。
        let _ = crate::theme::install_cjk_font(&ctx, None);
        // `run_ui` 的闭包是 FnMut，且只返回 FullOutput，
        // 所以被 closure 捕获的值要放进 Option 里带出来。
        let mut outbox = None;
        let mut add_contents = add_contents;
        // 这一帧的输出测试里用不到，但两样东西**必须显式处理**：
        // - `textures_delta`：字体图集 newly-uploaded 的纹理，epaint 在
        //   Drop 时会检查「有没有未处理的增量」并 panic。
        // - `FullOutput` 本身是 must_use。
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            if outbox.is_none() {
                outbox = Some(add_contents(ui));
            }
        });
        out.textures_delta.clear();
        drop(out);
        outbox.expect("run_ui 必定执行一次闭包")
    }

    fn bar() -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, TITLEBAR_HEIGHT))
    }

    /// 实机 420x560 物理 / 1.5x 缩放下的**逻辑**客户区宽度。
    const REAL_LOGICAL_W: f32 = 280.0;

    /// 「模块化剪切板」在标题字号下的实测宽度。
    fn measured_title_w(ui: &Ui) -> f32 {
        measure_text(ui, crate::view::APP_TITLE, &egui::FontId::proportional(14.0))
    }

    #[test]
    fn layout_places_buttons_left_to_right() {
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        // 从右往左：设置 → 最小化 → 关闭
        assert!(l.settings.max.x <= l.minimize.min.x + 0.01, "设置应在最小化左侧");
        assert!(l.minimize.max.x <= l.close.min.x + 0.01, "最小化应在关闭左侧");
    }

    #[test]
    fn close_button_touches_right_edge() {
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert!((l.close.max.x - bar().max.x).abs() < 0.01);
    }

    #[test]
    fn drag_region_excludes_every_button() {
        // 这是本文件最重要的一条：按钮落进拖动区 = 点按钮变成拖窗口。
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
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
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert!((l.drag.min.x - bar().min.x).abs() < 0.01);
        assert!((l.drag.min.y - bar().min.y).abs() < 0.01);
        assert!((l.drag.max.y - bar().max.y).abs() < 0.01);
    }

    #[test]
    fn drag_region_stops_before_search_box() {
        // 拖动区右界必须止于**搜索框**左缘，不是设置按钮。
        //
        // 回归守卫：早先取 `settings.min.x`，拖动区是 `[0, 180]` 而搜索框
        // 在 `[104, 172]` —— 搜索框整个落在拖动区里，点它变成拖窗口。
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert!(
            l.drag.max.x <= l.search.min.x + 0.01,
            "拖动区右边界应止于搜索框左缘，drag.max.x={} search.min.x={}",
            l.drag.max.x,
            l.search.min.x
        );
        assert!(
            !l.search_inside_drag(),
            "搜索框绝不能落进拖动区：它是唯一的输入控件，被吞掉等于搜索不可用"
        );
    }

    #[test]
    fn search_box_never_swallowed_by_drag_region() {
        // 这条是本缺陷的权威判据。它必须走 `TitlebarLayout::new`
        // 的真实计算路径，不是复刻一遍布局逻辑。
        for &(w, scale) in &[
            (420.0f32, 1.0f32),
            (420.0, 1.5),
            (420.0, 2.0),
            (280.0, 1.0),
            (600.0, 1.0),
            (900.0, 2.0),
        ] {
            let b = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 36.0));
            let l = TitlebarLayout::new(b, scale, 84.0 * scale);
            assert!(
                !l.search_inside_drag(),
                "w={w} scale={scale}: 搜索框 {:?} 落进拖动区 {:?}",
                l.search,
                l.drag
            );
            assert!(
                l.buttons_inside_drag(),
                "w={w} scale={scale}: 有控件落进拖动区 {:?}",
                l.drag
            );
        }
    }

    #[test]
    fn buttons_inside_drag_holds_at_higher_scale() {
        // 高 DPI 下按钮与拖动区都会变大，这条必须仍然成立。
        let l = TitlebarLayout::new(bar(), 2.0, 168.0);
        assert!(l.buttons_inside_drag());
    }

    #[test]
    fn buttons_inside_drag_holds_at_lower_scale() {
        let l = TitlebarLayout::new(bar(), 0.75, 63.0);
        assert!(l.buttons_inside_drag());
    }

    #[test]
    fn buttons_inside_drag_holds_for_narrow_window() {
        // 窄窗口下搜索框被压扁，但按钮与拖动区的关系不能破。
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, TITLEBAR_HEIGHT));
        let l = TitlebarLayout::new(narrow, 1.0, 84.0);
        assert!(l.buttons_inside_drag(), "窄窗口下按钮不能落进拖动区");
    }

    #[test]
    fn search_box_precedes_settings() {
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert!(l.search.max.x <= l.settings.min.x + 0.01);
    }

    #[test]
    fn search_box_width_is_nonnegative_when_window_too_narrow() {
        // 窗口窄到放不下时搜索框宽度应为 0 而非负数——绘制层据此跳过。
        let tiny = Rect::from_min_size(pos2(0.0, 0.0), vec2(60.0, TITLEBAR_HEIGHT));
        let l = TitlebarLayout::new(tiny, 1.0, 84.0);
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
        let l = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert_eq!(l.drag_region(), l.drag);
    }

    #[test]
    fn layout_is_deterministic() {
        let a = TitlebarLayout::new(bar(), 1.0, 84.0);
        let b = TitlebarLayout::new(bar(), 1.0, 84.0);
        assert_eq!(a, b, "同一输入必须给出同一布局，否则上报的拖动区会跳");
    }

    // -----------------------------------------------------------------------
    // 回归测试：标题 / 搜索框 / 按钮三者不得重叠
    //
    // 这几条针对的正是实机截图里「标题与搜索框文字糊成一片」那个现象。
    // 它们**走真实字体度量**（`measure_text` + 真实字体栈），
    // 而不是断言两个常数——常数对不代表布局对。
    // -----------------------------------------------------------------------

    /// 核心判据：标题矩形与搜索文字区**不得有正面积重叠**。
    #[test]
    fn title_rect_does_not_overlap_search_text_at_real_width() {
        let (title, search_text) = layout_ui(|ui| {
            let host =
                Rect::from_min_size(pos2(0.0, 0.0), vec2(REAL_LOGICAL_W, TITLEBAR_HEIGHT));
            let l = TitlebarLayout::new(host, 1.0, measured_title_w(ui));
            (l.title, l.search_text)
        });
        assert!(
            !rects_overlap(title, search_text),
            "标题 {title:?} 与搜索文字区 {search_text:?} 重叠"
        );
    }

    /// 更强的一条：两者之间必须留出至少 `TITLE_GAP` 的可见间隙。
    ///
    /// 只判「不相交」不够——矩形刚好贴在一起时两段文字视觉上仍会黏成一句，
    /// 实机截图里看到的正是这种「像重叠但其实没重叠」的状态。
    #[test]
    fn title_and_search_text_have_a_visible_gap() {
        let gap = layout_ui(|ui| {
            let host =
                Rect::from_min_size(pos2(0.0, 0.0), vec2(REAL_LOGICAL_W, TITLEBAR_HEIGHT));
            let l = TitlebarLayout::new(host, 1.0, measured_title_w(ui));
            l.search_text.min.x - l.title.max.x
        });
        assert!(
            gap >= TITLE_GAP - 0.01,
            "标题与搜索框之间只有 {gap:.2}pt 间隙，应 >= {TITLE_GAP}pt"
        );
    }

    /// 截断后的 placeholder **实测**宽度必须真的不超预算。
    ///
    /// 这是「按实测宽度裁」的自检：若哪天改成按字数估算，
    /// 这条会立刻失败——估算在真实字体下偏宽，文字又会压出去。
    #[test]
    fn elide_never_exceeds_budget_under_real_font_metrics() {
        let font = egui::FontId::proportional(14.0);
        let samples = [
            "搜索历史…",
            "unigpxidc_36a120ae1baca48pid",
            "设置自定名 asdfghjkl 测试",
            "监听中 · 拦截 128",
        ];
        layout_ui(|ui| {
            for s in samples {
                for budget in [0.0, 4.0, 12.0, 30.0, 64.0, 200.0] {
                    let e = elide_text(ui, s, &font, budget);
                    let w = measure_text(ui, &e, &font);
                    assert!(
                        w <= budget.max(0.0) + 0.01,
                        "截断结果超预算：{e:?} 实测 {w:.2} > {budget:.2}（原文 {s:?}）"
                    );
                }
            }
        });
    }

    /// 预算够宽时必须**原样返回**，不能多吞一个字。
    ///
    /// 只测「不超预算」会漏掉「什么都没截」这种过度截断的退化。
    #[test]
    fn elide_keeps_text_that_already_fits() {
        let font = egui::FontId::proportional(14.0);
        layout_ui(|ui| {
            let s = "监听中";
            let w = measure_text(ui, s, &font);
            assert_eq!(elide_text(ui, s, &font, w + 8.0), s, "够宽却截断了");
            assert_eq!(elide_text(ui, s, &font, 0.0), "", "零预算应返回空串");
            assert_eq!(elide_text(ui, "", &font, 100.0), "", "空串应返回空串");
        });
    }

    /// 实机那条判据：placeholder 截断后必须**恰好**落在文字区内。
    ///
    /// 记录修前/修后的具体数值，便于对着截图核对。
    #[test]
    fn placeholder_is_elided_to_fit_search_box_at_real_width() {
        let font = egui::FontId::proportional(14.0);
        let (raw_w, avail, elided, elided_w) = layout_ui(|ui| {
            let host =
                Rect::from_min_size(pos2(0.0, 0.0), vec2(REAL_LOGICAL_W, TITLEBAR_HEIGHT));
            let l = TitlebarLayout::new(host, 1.0, measured_title_w(ui));
            let raw_w = measure_text(ui, "搜索历史…", &font);
            let avail = l.search_text.width();
            let e = elide_text(ui, "搜索历史…", &font, avail);
            let ew = measure_text(ui, &e, &font);
            (raw_w, avail, e, ew)
        });
        // 修前：文字 62.5pt 画在 44pt 的框里 → 溢出 18.5pt 压到设置按钮上。
        // 修后：截断到 <= avail。
        assert!(
            elided_w <= avail + 0.01,
            "placeholder 截断后仍超界：{elided:?} {elided_w:.2} > {avail:.2}"
        );
        assert!(
            raw_w > avail,
            "原始 placeholder 本来就放得下（{raw_w:.2} <= {avail:.2}）——\
             这条测试的前提变了，请复核窗口宽度/字号"
        );
    }

    /// 极窄窗口下搜索框被压没，此时**不能**出现负宽度或倒挂的矩形。
    #[test]
    fn search_text_rect_stays_ordered_when_box_is_squeezed() {
        layout_ui(|ui| {
            let w = measured_title_w(ui);
            for width in [60.0_f32, 120.0, 200.0, REAL_LOGICAL_W, 600.0] {
                let host = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, TITLEBAR_HEIGHT));
                let l = TitlebarLayout::new(host, 1.0, w);
                assert!(l.search.width() >= 0.0, "width={width} 搜索框宽度为负");
                assert!(l.search_text.width() >= 0.0, "width={width} 文字区宽度为负");
                assert!(
                    l.search_text.min.x <= l.search_text.max.x + 0.01,
                    "width={width} 文字区左右缘倒挂: {:?}",
                    l.search_text
                );
            }
        });
    }

    /// 标题超长时必须让位给搜索框，而不是把搜索框挤没。
    #[test]
    fn overlong_title_yields_space_to_search_box() {
        let host = Rect::from_min_size(pos2(0.0, 0.0), vec2(REAL_LOGICAL_W, TITLEBAR_HEIGHT));
        // 一个荒谬地长的标题（400pt）
        let l = TitlebarLayout::new(host, 1.0, 400.0);
        assert!(
            l.title.width() <= host.width() * TITLE_MAX_FRACTION + 0.01,
            "标题未受 {TITLE_MAX_FRACTION} 比例限制: {:?}",
            l.title
        );
        assert!(l.search.width() > 0.0, "超长标题把搜索框挤没了");
        assert!(!rects_overlap(l.title, l.search), "超长标题压到搜索框");
    }

    /// 缩放 × 宽度全组合下，「不相交 + 有间隙 + 按钮在拖动区外」都成立。
    #[test]
    fn title_and_search_never_collide_at_any_scale() {
        layout_ui(|ui| {
            let w = measured_title_w(ui);
            for scale in [0.75_f32, 1.0, 1.5, 2.0] {
                for width in [200.0_f32, 280.0, 600.0] {
                    let host = Rect::from_min_size(pos2(0.0, 0.0), vec2(width, TITLEBAR_HEIGHT));
                    let l = TitlebarLayout::new(host, scale, w * scale);
                    if l.search.width() > 0.0 {
                        assert!(
                            !rects_overlap(l.title, l.search),
                            "scale={scale} width={width} 标题压到搜索框: {:?} {:?}",
                            l.title,
                            l.search
                        );
                        assert!(
                            l.search_text.min.x - l.title.max.x >= TITLE_GAP * scale - 0.01,
                            "scale={scale} width={width} 间隙不足"
                        );
                    }
                    assert!(l.buttons_inside_drag(), "scale={scale} 按钮落进拖动区");
                }
            }
        });
    }

}
