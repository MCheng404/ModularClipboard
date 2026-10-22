//! 把调色板灌进 egui `Visuals` 的入口（`apply` / `set_theme`）。
//!
//! 深浅两套都从 `Theme::Dark` 克隆后逐项全量覆盖：只设
//! `bg_fill` 漏掉 `fg_stroke` 会重现「白底白字」。


use super::{Palette, ThemeMode, resolve};

use super::system::detect_system_theme;

/// 滚动条热区宽度（逻辑点）。
///
/// 必须与 `paint::list` 的 `SCROLLBAR_RESERVE` 一致 —— 两处各写一份的话，
/// 改一处忘另一处就会让「已避开滚动条」的位置失准。
pub const SCROLLBAR_WIDTH: f32 = 10.0;

/// 把令牌灌进 egui 的 [`egui::Visuals`]——颜色体系的**单一入口**。
///
/// 调用方只需 [`set_theme`] 一句，egui 内置控件与自绘图形就同源。
/// 深浅两套都从 `Theme::Dark` 克隆后**逐项全量覆盖**：只设 `bg_fill`
/// 而漏掉 `fg_stroke` 会重现「白底白字」——界面能跑但什么都看不见。
///
/// 故意不映射的字段（都是行为开关或字体参数，不是颜色）：
/// - `text_options`：字体渲染参数，与配色无关
/// - `window_shadow` / `popup_shadow`：阴影用 egui 默认，其自带明暗适配
/// - `handle_shape` / `slider_trailing_fill` / `resize_corner_size`
/// - `striped` / `window_highlight_topmost` / `button_frame`
/// - `collapsing_header_frame` / `indent_has_left_vline`
/// - `interact_cursor` / `disabled_alpha` / `image_loading_spinners`
/// - `numeric_color_space` / `clip_rect_margin`（已废弃）
/// - `ime_composition.legacy_visuals`：Windows 上默认 true 是刻意为之，
///   涉及 winit 的中文输入法 bug（见 egui 源码注释），改动会导致
///   中文候选词渲染异常
pub fn apply(ctx: &egui::Context, palette: &Palette) {
    let p = palette;
    let mut v = ctx.style_of(egui::Theme::Dark).visuals.clone();

    v.dark_mode = p.is_dark;
    v.override_text_color = Some(p.text);
    // 直接设 `weak_text_color` 而非依赖 `weak_text_alpha` 推导：
    // `icons.rs` 的 `icon_button` 读 `ui.visuals().weak_text_color()`，
    // 必须与 `palette.text_dim` 完全一致才能同源。
    v.weak_text_color = Some(p.text_dim);

    // 背景
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface_raised;
    v.faint_bg_color = p.surface_variant;
    v.window_stroke = egui::Stroke::new(p.stroke_thin, p.border);
    v.window_corner_radius = egui::CornerRadius::same(p.radius_md as u8);
    v.menu_corner_radius = egui::CornerRadius::same(p.radius_sm as u8);

    let text_stroke = egui::Stroke::new(p.stroke_thin, p.text);
    let border_stroke = egui::Stroke::new(p.stroke_thin, p.border);
    let strong_stroke = egui::Stroke::new(p.stroke_normal, p.border_strong);

// egui 0.36 未从 crate 根导出 `WidgetVisuals`，故用闭包 + 类型推断
    // 统一赋值：五个 widget 态的字段集完全相同，逐个手写易漏字段。
    macro_rules! set_widget {
        ($wv:expr, $bg:expr, $weak:expr, $stroke:expr, $fg:expr) => {{
            let wv = $wv;
            wv.bg_fill = $bg;
            wv.weak_bg_fill = $weak;
            wv.bg_stroke = $stroke;
            wv.fg_stroke = $fg;
            wv.corner_radius = egui::CornerRadius::same(p.radius_sm as u8);
        }};
    }

    let bright_stroke = egui::Stroke::new(p.stroke_thin, p.text_bright);

    for wv in [&mut v.widgets.noninteractive, &mut v.widgets.inactive] {
        set_widget!(wv, p.surface, p.surface_variant, border_stroke, text_stroke);
    }
    set_widget!(
        &mut v.widgets.hovered,
        composite(p.row_hover, p.surface),
        p.row_selected,
        strong_stroke,
        bright_stroke
    );
    set_widget!(
        &mut v.widgets.active,
        composite(p.row_active, p.surface),
        p.row_selected,
        strong_stroke,
        bright_stroke
    );
    set_widget!(
        &mut v.widgets.open,
        composite(p.row_hover, p.surface),
        p.row_selected,
        strong_stroke,
        bright_stroke
    );

    // 选中态。合成到 bg 上：列表行画在面板上，而面板本身也在窗口底色上。
    v.selection.bg_fill = composite(p.row_selected, p.surface);
    v.selection.stroke = egui::Stroke::new(p.stroke_thin, p.text_bright);

    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;

    v.ime_composition
        .active_underline_stroke = egui::Stroke::new(p.stroke_thick, p.accent);
    v.ime_composition
        .inactive_underline_stroke = egui::Stroke::new(p.stroke_thin, p.border_strong);

    v.text_cursor.stroke = egui::Stroke::new(p.stroke_thick, p.accent);

    // `Context::set_visuals` 只写入 `theme_preference` 当前指向的那套
    // style。切主题时若另一套仍是旧值，egui 内部任何按 theme 取样式的
    // 路径（弹窗、原生窗口）就会读到上一次的配色——「切了但没全切」。
    // 因此两套都写，并把 preference 对齐到本调色板，
    // 使 `Context::theme()` 与 `palette.is_dark` 始终一致。
    let theme = if p.is_dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_visuals_of(theme, v.clone());
    // 另一套保留 egui 默认，只对齐 dark_mode 标记，
    // 避免它被误用时带着与本主题矛盾的明暗标记。
    let mut other = ctx.style_of(other_theme(theme)).visuals.clone();
    other.dark_mode = !p.is_dark;
    ctx.set_visuals_of(other_theme(theme), other);

    // 字号：egui 控件层（输入框 / 下拉 / 按钮内文字）跟随调色板字号阶，
    // 与自绘层（`pal.font_*`）同源，避免「自绘文字大、控件文字小」。
    // 两套 theme 都写，切换主题时控件字号不回落。
    //
    // CAUTION: 不能把 Dark 的整个 style clone 给 Light——那会把 Dark 的
    // `visuals.dark_mode`（及配色）带过去，浅色主题瞬间变黑（有测试守
    // 这条：`apply_covers_every_fg_stroke_in_both_themes`）。只换字号表。
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(p.font_md));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(p.font_md));
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(p.font_sm));
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(p.font_xl));
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(p.font_sm));
    // 滚动条：改成 Bing/Google 那种「常态微可见」。
    //
    // egui 0.36 的 `ScrollArea` **没有** `style()` 方法，它从
    // `ui.spacing().scroll` 读样式（见 egui `containers/scroll_area.rs`
    // 的 `ui.spacing().scroll`）。所以只能改 `Style.spacing`，
    // 而这必须是**全局**设置 —— 逐个 ScrollArea 设会漏。
    style.spacing.scroll = egui::style::ScrollStyle {
        // floating：滚动条浮在内容之上、**不占布局宽度**。
        // `paint::list::SCROLLBAR_RESERVE`(10) 与行内按钮的让位都按这个假设算。
        floating: true,
        // IMPORTANT: 这里**必须**是 10 —— 它是热区宽度，不是视觉宽度。
        // 改小会让「已避开滚动条」的位置仍落在滚动条下面，症状是
        // 「条目少时一切正常、条目一多删除按钮点不动」。
        // 想让滑块变细靠 `handle_min_length` 与透明度，不动这里。
        bar_width: SCROLLBAR_WIDTH,
        handle_min_length: 24.0,
        bar_inner_margin: 3.0,
        // 三级不透明度（常态 / 激活 / 可交互）。
        //
        // 常态刻意**不是 0**：全透明的滚动条在内容溢出时会被整体
        // 忽略 —— 用户不知道下面还有东西，得先随手拖一下才发现能滚。
        dormant_handle_opacity: 0.42,
        active_handle_opacity: 0.72,
        interact_handle_opacity: 0.85,
        // 轨道底色也要淡：画实色会在列表右缘留下一条竖色带，
        // 与「整窗一块底」的一体感冲突。
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.0,
        interact_background_opacity: 0.0,
        ..egui::style::ScrollStyle::floating()
    };
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    let ts = style.text_styles.clone();
    let sc = style.spacing.clone();
    let mut light_style = (*ctx.style_of(egui::Theme::Light)).clone();
    light_style.text_styles = ts;
    light_style.spacing = sc;
    ctx.set_style_of(egui::Theme::Light, light_style);

    ctx.set_theme(theme);
}

/// 主题的对偶（深↔浅）。
pub(super) fn other_theme(t: egui::Theme) -> egui::Theme {
    match t {
        egui::Theme::Dark => egui::Theme::Light,
        egui::Theme::Light => egui::Theme::Dark,
    }
}

/// 解析主题并灌入 egui，返回生效的调色板。
///
/// 这是上层只需调用的一句入口：拿到返回值即可用于自绘。
/// 读取 `dark_mode` 时应传 [`crate::app::UiLocal`] 之外已解析的
/// `Option<bool>`；`None` 表示跟随系统。
pub fn set_theme(ctx: &egui::Context, mode: ThemeMode) -> Palette {
    let system = detect_system_theme();
    let palette = resolve(mode, system).palette();
    apply(ctx, &palette);
    palette
}

/// source-over alpha 合成：把半透明 `fg` 叠在不透明 `bg` 上，返回不透明色。
///
/// 委托给 [`egui::Color32::blend`]——[`Color32`] 内部按**预乘 alpha**
/// 存储（`r()` 返回的是预乘后的通道），手写 `f*a + b*(1-a)` 会把
/// alpha 算两遍，得到偏深的错误结果。合成语义与 egui 实际绘制一致，
/// 这点很关键：对比度必须针对**真正画出来的那个像素**计算。
///
/// 半透明令牌的对比度必须走这条路——面板叠在窗口底色上，
/// 直接拿半透明值参与 WCAG 计算会高估。
pub fn composite(fg: egui::Color32, bg: egui::Color32) -> egui::Color32 {
    bg.blend(fg).to_opaque()
}
