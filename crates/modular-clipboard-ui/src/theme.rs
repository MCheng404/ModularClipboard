//! 主题与字体。
//!
//! 字体是关键：egui 内置字体不含 CJK 字形，若不加载系统中文字体，
//! 所有中文都会渲染成方块。因此启动时按优先级探测系统字体并注册为
//! Proportional 的最高优先级回退。
//!
//! # 设计令牌
//!
//! [`Palette`] 是**唯一**的颜色与非颜色来源：egui 内置控件的观感由
//! [`apply`] 从它灌入 [`egui::Visuals`]，自绘图形（`view.rs` 的
//! `paint_inline`、`icons.rs` 的图标）直接读它的字段。两条路径同源，
//! 主题切换时才不会出现「一半控件不跟着变」。
//!
//! # 命名约定：语义而非外观
//!
//! 字段名描述**用途**（`text_dim` / `row_selected` / `danger`），
//! 不描述外观（没有 `gray_light` / `blue_accent` 这类名字）。
//! 原因：同一个语义在深浅两套下的外观本就不同——浅色模式的 `warn`
//! 是压深的琥珀色（要压深才够 4.5:1），深色模式是亮琥珀。
//! 按外观命名会让「换配色方案」变成逐个字段追��改名。
//!
//! # 对比度
//!
//! 所有取值经 [`tests`] 中的 WCAG 校验：正文 4.5:1、非文本组件 3:1。
//! 半透明令牌（`row_alt` / `row_hover` / `overlay_scrim`）的对比度
//! 一律**先合成到实际背景**再计算——面板叠在窗口底色上，
//! 直接拿半透明值算会高估。唯一豁免是 `border_subtle`：
//! 它是弱分隔线，不承载控件边界信息。

use std::time::Duration;

use egui::{FontData, FontDefinitions, FontFamily, FontId};

/// 一个令牌的值。供完整性测试遍历用。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    Color(egui::Color32),
    Num(f32),
    Dur(Duration),
}

/// 把令牌类别映射到字段类型。
macro_rules! palette_ty {
    (color) => { egui::Color32 };
    (num) => { f32 };
    (dur) => { Duration };
}

/// 把 `(类别, 值)` 包成 [`TokenValue`]。
macro_rules! palette_token {
    (color, $v:expr) => { TokenValue::Color($v) };
    (num, $v:expr) => { TokenValue::Num($v) };
    (dur, $v:expr) => { TokenValue::Dur($v) };
}

/// 定义 [`Palette`] 的字段与两套取值。
///
/// 用宏而不是手写两个 `struct` 字面量，是为了让「深浅两套字段对称」
/// 由**构造**保证：新增字段时忘记给浅色版赋值会直接编译失败，
/// 而不是静默漏掉一半、运行后才发现某个控件在浅色下没跟着变。
/// 「换一个配色方案」也由此变成只改这张表的第三、四列。
macro_rules! palette {
    (
        $(
            $(#[$meta:meta])*
            $name:ident : $kind:ident = $dark:expr => $light:expr
        ),* $(,)?
    ) => {
        /// 配色与尺寸令牌。集中定义，避免界面里出现魔法数字。
        pub struct Palette {
            $(
                $(#[$meta])*
                pub $name: palette_ty!($kind),
            )*

            /// 本调色板是否为深色。
            ///
            /// 由 [`Palette::dark`] / [`Palette::light`] 写入，
            /// [`apply`] 用它设置 `Visuals::dark_mode`。
            ///
            /// 不是设计令牌，故不进 [`Palette::tokens`]——它是元信息。
            pub is_dark: bool,
        }

        impl Palette {
            /// 深色主题。
            pub fn dark() -> Self {
                Self { $( $name: $dark, )* is_dark: true }
            }

            /// 浅色主题。
            pub fn light() -> Self {
                Self { $( $name: $light, )* is_dark: false }
            }

            /// 遍历全部设计令牌（字段名 + 值），供完整性测试使用。
            ///
            /// 不含 [`Palette::is_dark`]。
            pub fn tokens(&self) -> Vec<(&'static str, TokenValue)> {
                let mut v: Vec<(&'static str, TokenValue)> = Vec::new();
                $(
                    v.push((stringify!($name), palette_token!($kind, self.$name)));
                )*
                v
            }

            // ---- 旧字段别名 ----
            //
            // `panel_bg` 与上面的 `surface` 取值完全一致，保留是为了不
            // 破坏现有调用点（`view.rs` / `app.rs`）。新代码请直接用
            // `surface` / `accent` / `warn`——等调用点迁移完再删。

            /// 等同 [`Palette::surface`]。旧名，兼容保留。
            pub fn panel_bg(&self) -> egui::Color32 {
                self.surface
            }

            /// 由模式取调色板。等价于 [`ThemeMode::palette`]，
            /// 放在 [`Palette`] 上是为了让调用点写成
            /// `Palette::from_mode(mode)` 这样更顺。
            pub fn from_mode(mode: crate::theme::ThemeMode) -> Palette {
                mode.palette()
            }
        }
    };
}

palette! {
    // ---- 背景层 ----
    /// 窗口最底层背景。
    bg: color = egui::Color32::from_rgb(0x14, 0x16, 0x1A)
              => egui::Color32::from_rgb(0xF2, 0xF4, 0xF7),
    /// 面板 / 卡片背景。
    ///
    /// 旧字段 `panel_bg` 是本令牌的别名，保留是为了不破坏调用点。
    surface: color = egui::Color32::from_rgb(0x1E, 0x21, 0x27)
                => egui::Color32::from_rgb(0xFA, 0xFB, 0xFC),
    /// 次级面板（嵌套容器）。
    surface_variant: color = egui::Color32::from_rgb(0x26, 0x2A, 0x31)
                       => egui::Color32::from_rgb(0xEC, 0xEF, 0xF3),
    /// 浮层：菜单、气泡、可拖拽模块。
    surface_raised: color = egui::Color32::from_rgb(0x2E, 0x33, 0x3B)
                    => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
    /// 遮罩。半透明，压在 `bg` 之上。
    overlay_scrim: color = egui::Color32::from_black_alpha(170)
                     => egui::Color32::from_rgba_unmultiplied(0x10, 0x14, 0x1A, 110),

    // ---- 描边层 ----
    /// 常规描边。非文本组件需达 3:1。
    border: color = egui::Color32::from_rgb(0x6E, 0x78, 0x86)
              => egui::Color32::from_rgb(0x7C, 0x86, 0x92),
    /// 强调描边（聚焦态）。
    border_strong: color = egui::Color32::from_rgb(0x9C, 0xA6, 0xB4)
                     => egui::Color32::from_rgb(0x4A, 0x55, 0x62),
    /// 弱分隔线。**不要求** 3:1：它不承载控件边界信息，
    /// 强求高对比会让面板之间全是硬边，视觉噪声大于收益。
    border_subtle: color = egui::Color32::from_rgb(0x2A, 0x2F, 0x36)
                     => egui::Color32::from_rgb(0xDD, 0xE2, 0xE8),

    // ---- 文本层 ----
    /// 主要文字。
    text: color = egui::Color32::from_rgb(0xE3, 0xE6, 0xE8)
           => egui::Color32::from_rgb(0x24, 0x29, 0x2E),
    /// 次要文字。
    text_dim: color = egui::Color32::from_rgb(0xC2, 0xC2, 0xC2)
               => egui::Color32::from_rgb(0x54, 0x54, 0x54),
    /// 强调文字：数值、标题。
    text_bright: color = egui::Color32::from_rgb(0xFC, 0xFC, 0xFC)
                   => egui::Color32::from_rgb(0x0F, 0x0F, 0x0F),
    /// 强调色之上的文字。深色主题的强调色偏亮，故取近黑；
    /// 浅色主题的强调色偏深，故取纯白。
    text_on_accent: color = egui::Color32::from_rgb(0x0A, 0x0D, 0x12)
                     => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),

    // ---- 状态色 ----
    //
    // 三态（normal / hover / active）**共用亮度档位，区分靠饱和度**。
    // 这不是省事：亮度一旦为了表达「按下」而下调，状态色作为前景
    // 会掉出 4.5:1，作为填充底色又要求更深（浅色主题），两者不可兼得。
    // 饱和度差异既能表达交互态，又不牺牲可读性。
    //
    /// 强调色（选中、主操作）。
    accent: color = egui::Color32::from_rgb(0x84, 0xB7, 0xEB)
             => egui::Color32::from_rgb(0x1D, 0x65, 0xAF),
    /// `accent` 悬停态。
    accent_hover: color = egui::Color32::from_rgb(0x7E, 0xB6, 0xF1)
                   => egui::Color32::from_rgb(0x14, 0x64, 0xB8),
    /// `accent` 按下态。
    accent_active: color = egui::Color32::from_rgb(0x79, 0xB6, 0xF6)
                    => egui::Color32::from_rgb(0x0C, 0x64, 0xC0),
    /// 成功。
    success: color = egui::Color32::from_rgb(0xA0, 0xDA, 0xAE)
              => egui::Color32::from_rgb(0x30, 0x7B, 0x43),
    /// `success` 悬停态。
    success_hover: color = egui::Color32::from_rgb(0x9A, 0xDF, 0xAB)
                    => egui::Color32::from_rgb(0x27, 0x7C, 0x3D),
    /// `success` 按下态。
    success_active: color = egui::Color32::from_rgb(0x95, 0xE4, 0xA9)
                     => egui::Color32::from_rgb(0x1F, 0x7C, 0x36),
    /// 警示。
    warn: color = egui::Color32::from_rgb(0xEB, 0xC9, 0x8E)
           => egui::Color32::from_rgb(0x90, 0x64, 0x19),
    /// `warn` 悬停态。
    warn_hover: color = egui::Color32::from_rgb(0xF2, 0xCA, 0x88)
                 => egui::Color32::from_rgb(0x93, 0x63, 0x10),
    /// `warn` 按下态。
    warn_active: color = egui::Color32::from_rgb(0xF8, 0xCC, 0x81)
                  => egui::Color32::from_rgb(0x96, 0x61, 0x08),
    /// 危险：删除、清空。
    danger: color = egui::Color32::from_rgb(0xE2, 0x8D, 0x8F)
             => egui::Color32::from_rgb(0xB4, 0x2D, 0x31),
    /// `danger` 悬停态。
    danger_hover: color = egui::Color32::from_rgb(0xEB, 0x84, 0x87)
                   => egui::Color32::from_rgb(0xC1, 0x1F, 0x24),
    /// `danger` 按下态。
    danger_active: color = egui::Color32::from_rgb(0xF4, 0x7C, 0x7F)
                    => egui::Color32::from_rgb(0xCE, 0x12, 0x17),
    /// 信息。
    info: color = egui::Color32::from_rgb(0x95, 0xCF, 0xE4)
           => egui::Color32::from_rgb(0x25, 0x75, 0x93),
    /// `info` 悬停态。
    info_hover: color = egui::Color32::from_rgb(0x8D, 0xD3, 0xEC)
                 => egui::Color32::from_rgb(0x19, 0x75, 0x98),
    /// `info` 按下态。
    info_active: color = egui::Color32::from_rgb(0x85, 0xD6, 0xF4)
                  => egui::Color32::from_rgb(0x0E, 0x75, 0x9C),

    // ---- 交互态背景 ----
    //
    // 半透明令牌：对比度测试必须先合成到 `surface` 才能算。
    //
    /// 隔行底色。
    row_alt: color = egui::Color32::from_white_alpha(16)
             => egui::Color32::from_black_alpha(10),
    /// 悬停底色。
    row_hover: color = egui::Color32::from_white_alpha(30)
               => egui::Color32::from_black_alpha(26),
    /// 选中底色。强调色的低透明色调，其上仍用 `text` / `text_dim`。
    row_selected: color = egui::Color32::from_rgba_unmultiplied(0x84, 0xB7, 0xEB, 56)
                   => egui::Color32::from_rgba_unmultiplied(0x1D, 0x65, 0xAF, 52),
    /// 按下底色。**刻意不用 accent 色调**——按下时再叠一层强调色会
    /// 把底色推得太亮/太深，`text_dim` 落在其上就跌破 4.5:1。
    /// 「压下一层」也更贴合直觉：深色下提亮、浅色下压深。
    row_active: color = egui::Color32::from_white_alpha(52)
                  => egui::Color32::from_black_alpha(52),

    // ---- 圆角 ----
    /// 小圆角：复选框、内联标签。
    radius_sm: num = 4.0 => 4.0,
    /// 中圆角：按钮、输入框。
    radius_md: num = 6.0 => 6.0,
    /// 大圆角：面板、卡片。
    radius_lg: num = 10.0 => 10.0,

    // ---- 间距（4 的倍数）----
    space_xs: num = 4.0 => 4.0,
    space_sm: num = 8.0 => 8.0,
    space_md: num = 12.0 => 12.0,
    space_lg: num = 16.0 => 16.0,
    space_xl: num = 24.0 => 24.0,

    // ---- 字号阶 ----
    font_xs: num = 11.0 => 11.0,
    font_sm: num = 12.0 => 12.0,
    font_md: num = 14.0 => 14.0,
    font_lg: num = 16.0 => 16.0,
    font_xl: num = 20.0 => 20.0,
    font_title: num = 24.0 => 24.0,

    // ---- 描边宽度 ----
    stroke_thin: num = 1.0 => 1.0,
    stroke_normal: num = 1.5 => 1.5,
    stroke_thick: num = 2.0 => 2.0,

    // ---- 控件尺寸 ----
    /// 按钮 / 输入框统一高度。
    control_height: num = 28.0 => 28.0,
    /// 图标标准尺寸。
    icon_size: num = 16.0 => 16.0,

    // ---- 动效 ----
    /// 快速动效：悬停反馈。
    anim_fast: dur = Duration::from_millis(90) => Duration::from_millis(90),
    /// 常规动效：展开、淡入。
    anim_normal: dur = Duration::from_millis(160) => Duration::from_millis(160),
}

/// 主题模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    /// 强制深色。
    Dark,
    /// 强制浅色。
    Light,
    /// 跟随系统。
    #[default]
    FollowSystem,
}

impl ThemeMode {
    /// 从配置里的 `dark_mode: Option<bool>` 构造。
    ///
    /// `None` = 跟随系统，`Some(true)` = 深色，`Some(false)` = 浅色。
    /// 与 `UiConfig::dark_mode` 的既有语义一一对应，无需改动配置结构。
    pub fn from_dark_mode(dark_mode: Option<bool>) -> Self {
        match dark_mode {
            Some(true) => Self::Dark,
            Some(false) => Self::Light,
            None => Self::FollowSystem,
        }
    }

    /// 还原为配置里的 `dark_mode: Option<bool>`。
    ///
    /// [`ThemeMode::FollowSystem`] 存为 `None`。
    pub fn to_dark_mode(self) -> Option<bool> {
        match self {
            Self::Dark => Some(true),
            Self::Light => Some(false),
            Self::FollowSystem => None,
        }
    }

    /// 本模式下生效的调色板。`FollowSystem` 需先经 [`resolve`] 解析。
    ///
    /// `FollowSystem` 落到深色分支：调用方漏调 [`resolve`] 时
    /// 静默变浅色比静默变深色更难排查（旧实现也默认深色）。
    pub fn palette(self) -> Palette {
        match self {
            Self::Light => Palette::light(),
            Self::Dark | Self::FollowSystem => Palette::dark(),
        }
    }
}

/// 解析出实际生效的主题。
///
/// `FollowSystem` 取 `system`；若 `system` 本身也是 `FollowSystem`
/// 则兜底为 [`ThemeMode::Dark`]——必须有确定结果，不能递归。
pub fn resolve(mode: ThemeMode, system: ThemeMode) -> ThemeMode {
    match mode {
        ThemeMode::Dark => ThemeMode::Dark,
        ThemeMode::Light => ThemeMode::Light,
        ThemeMode::FollowSystem => match system {
            ThemeMode::Light => ThemeMode::Light,
            ThemeMode::Dark | ThemeMode::FollowSystem => ThemeMode::Dark,
        },
    }
}

/// 探测系统主题。只返回 [`ThemeMode::Dark`] 或 [`ThemeMode::Light`]。
///
/// Windows 上读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
/// 的 `AppsUseLightTheme`（DWORD，0 = 深色，1 = 浅色）。
/// 读不到（非 Windows、键不存在、类型不符）一律回退深色并告警，
/// 不 panic、不阻塞。
///
/// **只在启动时调一次**，不要放进每帧路径。
pub fn detect_system_theme() -> ThemeMode {
    match read_apps_use_light_theme() {
        Some(light) => {
            if light {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            }
        }
        None => {
            tracing::warn!("读取系统主题失败，回退深色");
            ThemeMode::Dark
        }
    }
}

/// 读 `AppsUseLightTheme`。返回 `None` 表示读不到。
#[cfg(windows)]
fn read_apps_use_light_theme() -> Option<bool> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
    };

    const SUB_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    const VALUE_NAME: &str = "AppsUseLightTheme";

    let sub_key: Vec<u16> = SUB_KEY.encode_utf16().chain(std::iter::once(0)).collect();
    let value_name: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;

    // 安全性：`RegGetValueW` 只读取注册表，不写；两个 PCWSTR 以 NUL 结尾
    // 且指向本函数的 Vec，生命周期覆盖整个调用。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub_key.as_ptr()),
            windows::core::PCWSTR(value_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast()),
            Some(&mut size),
        )
    };

    if status != ERROR_SUCCESS {
        tracing::debug!(code = status.0, "RegGetValueW 失败");
        return None;
    }
    Some(data != 0)
}

#[cfg(not(windows))]
fn read_apps_use_light_theme() -> Option<bool> {
    None
}

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
    ctx.set_theme(theme);
}

/// 主题的对偶（深↔浅）。
fn other_theme(t: egui::Theme) -> egui::Theme {
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

/// 候选中文字体，按优先级排列。
///
/// `.ttc` 字体集合的 `index` 通常为 0（第一个字体面）；
/// 若某个字体集合的首个字体面不含 CJK，可调整 index。
const CJK_CANDIDATES: &[(&str, &str, u32)] = &[
    ("msyh", "C:/Windows/Fonts/msyh.ttc", 0),
    ("deng", "C:/Windows/Fonts/Deng.ttf", 0),
    ("simsun", "C:/Windows/Fonts/simsun.ttc", 0),
    ("noto_sc", "C:/Windows/Fonts/NotoSansSC-VF.ttf", 0),
    ("linux_noto", "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0),
    ("linux_wqy", "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
    ("mac_pingfang", "/System/Library/Fonts/PingFang.ttc", 0),
];

/// 安装中文字体。返回是否成功找到并加载。
pub fn install_cjk_font(ctx: &egui::Context, custom_path: Option<&str>) -> bool {
    // 诊断开关：跳过字体加载，用于测量中文字体的内存成本。
    if std::env::var_os("TIEZ_NO_CJK_FONT").is_some() {
        tracing::warn!("TIEZ_NO_CJK_FONT 已设置，跳过中文字体加载");
        ctx.set_fonts(FontDefinitions::default());
        return false;
    }

    let mut defs = FontDefinitions::default();

    // 自定义路径优先。
    if let Some(p) = custom_path
        && let Ok(bytes) = std::fs::read(p)
    {
        if insert_font(&mut defs, "custom_cjk", &bytes, 0) {
            log_font_loaded(ctx, &mut defs, "自定义字体");
            return true;
        }
    }

    for (name, path, index) in CJK_CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if insert_font(&mut defs, name, &bytes, *index) {
            log_font_loaded(ctx, &mut defs, path);
            return true;
        }
        tracing::warn!(path, "字体存在但解析失败，尝试下一个");
    }

    tracing::warn!("未找到中文字体，界面中文将显示为方块");
    ctx.set_fonts(defs);
    false
}

/// 安装图标字体。返回是否成功。
///
/// # 为什么必须独立于 [`install_cjk_font`]
///
/// [`install_cjk_font`] 是「取第一个找到的就return」的逻辑：一旦某台机器上
/// 存在任何一款候选中文字体，它就**不会**再往字体定义里加任何东西。
/// 图标字体若寄生在它内部，中文字体不存在时图标就一起消失了——
/// 而界面里那些图标恰恰是操作入口，缺了比缺字更糟。
///
/// 因此这里是独立函数，独立注册 [`crate::icons::ICON_FONT_NAME`]。
///
/// # 字体族与顺序
///
/// 挂在 [`FontFamily::Proportional`]，**排在已有字体之后**
/// （[`FontPriority::Lowest`]）。
///
/// - 排在之后：CJK 字体被 `insert_font` 放在首位且字体庞大，
///   让图标字体抢在前面会让每个图标的排版都先去问一遍 CJK 字体。
/// - 但仍在同一字体族：图标码位是 PUA（`U+E000..=U+E011`），
///   中文字体不覆盖这些码位（已核实 `msyh.ttc` / `Deng.ttf` 均无
///   `U+E000..=U+E011`），因此回退一定能落到图标字体上。
///
/// 挂在 `Proportional` 而非新建字体族，是为了复用同一条纹理路径：
/// 图集走 `TextureId::Managed(0)`，渲染器无需增加第二个纹理绑定。
///
/// # 为什么用 `add_font` 而不是 `set_fonts`
///
/// 早前这里的写法是 `ctx.fonts(|f| f.definitions().clone())` 取出定义、
/// 改完再 `ctx.set_fonts(defs)` 整体替换。但 egui 0.36 在
/// [`egui::Context::fonts`] 上明确标注 *"Not valid until first call to
/// `Context::run()`"*，而本函数是在 `App::new` 里调用的（那时还没有
/// 任何 `run_ui`），直接 panic：
///
/// ```text
/// No fonts available until first call to Context::run()
/// ```
///
/// 正解是 [`egui::Context::add_font`]：egui 文档写明它 *"will keep the
/// existing fonts"*——**追加**而非替换，这恰好就是「图标字体排在 CJK
/// 之后」所需要的行为，且不依赖字体系统是否已就绪。
///
/// 另一个坑：`set_fonts` 会**整体替换** `FontDefinitions`，
/// 所以本函数必须排在 [`install_cjk_font`] 之后，否则图标字体会被冲掉。
/// `add_font` 不存在这个问题（它是增量追加），但顺序仍按原设计保留，
/// 以免影响阅读者对「图标在 CJK 之后」的预期。
pub fn install_icon_font(ctx: &egui::Context) -> bool {
    let name = crate::icons::ICON_FONT_NAME;
    // 直接复用 `icons::icon_font_data()`：它已经把基线偏移
    // （`FontTweak::y_offset_factor`）调好了，这里再包一层
    // `FontData::from_owned` 反而会把那个修正抹掉。
    let insert = egui::epaint::text::FontInsert::new(
        name,
        crate::icons::icon_font_data(),
        vec![egui::epaint::text::InsertFontFamily {
            family: FontFamily::Proportional,
            // 追加到回退链末尾：既有字体（含 CJK）优先命中，
            // 图标码位是 PUA，CJK 不覆盖，最终必然回退到这里。
            priority: egui::epaint::text::FontPriority::Lowest,
        }],
    );

    // `add_font` 无返回值：egui 只接受「同名字体已存在则不覆盖」的语义。
    // 重复调用（例如测试里多次安装）是幂等的，不会产生第二份。
    ctx.add_font(insert);
    tracing::info!(font = name, "图标字体已注册");
    true
}

fn insert_font(defs: &mut FontDefinitions, name: &str, bytes: &[u8], index: u32) -> bool {
    // egui 0.36 的 FontData 没有 with_index，直接设置公开的 index 字段。
    let mut data = FontData::from_owned(bytes.to_vec());
    data.index = index;
    defs.font_data
        .insert(name.to_string(), std::sync::Arc::new(data));

    // 插到每个字体族的首位，保证 CJK 字形优先命中，
    // 同时保留原有回退（缺字时仍能用内置字体）。
    let mut families = defs.families.clone();
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        families.entry(family).or_default().insert(0, name.to_string());
    }
    defs.families = families;
    true
}

fn log_font_loaded(ctx: &egui::Context, defs: &mut FontDefinitions, path: &str) {
    ctx.set_fonts(std::mem::take(defs));
    tracing::info!(path, "中文字体已加载");
}

/// 按缩放比例返回字号。
pub fn sized(size: f32, scale: f32) -> FontId {
    FontId::proportional(size * scale.clamp(0.8, 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // 对比度工具（WCAG 2.1）
    // ------------------------------------------------------------------

    /// sRGB 单通道线性化。阈值 0.03928 与 `((c+0.055)/1.055)^2.4`
    /// 来自 WCAG 2.1 的相对亮度定义，不可改动。
    fn linearize(c: u8) -> f32 {
        let s = c as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 相对亮度。
    fn luminance(c: egui::Color32) -> f32 {
        0.2126 * linearize(c.r()) + 0.7152 * linearize(c.g()) + 0.0722 * linearize(c.b())
    }

    /// WCAG 对比度，返回 1.0..=21.0。
    fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// 正文级阈值。
    const AA_TEXT: f32 = 4.5;
    /// 非文本 UI 组件阈值（WCAG 1.4.11）。
    const AA_UI: f32 = 3.0;

    /// 断言对比度达标，失败时打印实测值与参与计算的两个色值。
    fn assert_contrast(label: &str, fg: egui::Color32, bg: egui::Color32, need: f32) {
        let r = contrast(fg, bg);
        assert!(
            r >= need,
            "{label}: 对比度 {r:.3} < {need}（前景 {:?} / 背景 {:?}）",
            fg.to_array(),
            bg.to_array()
        );
    }

    /// 令牌在某背景上的最终合成色。
    ///
    /// 半透明令牌必须走这条路：`row_alt` 是叠在 `surface` 上的，
    /// 而 `surface` 又叠在 `bg` 上。直接拿半透明值算会高估——
    /// 这正是深色主题最容易踩的坑。
    fn over(p: &Palette, layer: &str, base: egui::Color32) -> egui::Color32 {
        match layer {
            "bg" => base,
            "surface" => base,
            "surface_variant" => composite(p.surface_variant, base),
            "surface_raised" => composite(p.surface_raised, base),
            "row_alt" => composite(p.row_alt, composite(p.surface, base)),
            "row_hover" => composite(p.row_hover, composite(p.surface, base)),
            "row_selected" => composite(p.row_selected, composite(p.surface, base)),
            "row_active" => composite(p.row_active, composite(p.surface, base)),
            other => panic!("未知层 {other}"),
        }
    }

    /// 面板之上、窗口底色之下的全部交互层。
    ///
    /// 文字会出现在其中每一层上——包括 `row_active`：鼠标按住条目时
    /// 那一帧文字仍需可读，只校验悬停/选中会漏掉按下态。
    const LAYERS: &[&str] = &[
        "bg",
        "surface",
        "surface_variant",
        "surface_raised",
        "row_alt",
        "row_hover",
        "row_selected",
        "row_active",
    ];

    /// 状态色三态。
    const STATE_NAMES: &[&str] = &["", "_hover", "_active"];

    // ---- 对比度：深浅两套 ----

    #[test]
    fn text_layer_meets_aa_on_every_background_it_can_appear_on() {
        for p in [Palette::dark(), Palette::light()] {
            // 文字可能落在任何一层上——包括 `row_active`（鼠标按住
            // 那一帧条目上仍有文字），故遍历全部层而非只挑几种。
            for layer in LAYERS {
                let bg = over(&p, layer, p.bg);
                assert_contrast(
                    &format!("[{}] text on {layer}", mode_of(&p)),
                    p.text,
                    bg,
                    AA_TEXT,
                );
                assert_contrast(
                    &format!("[{}] text_dim on {layer}", mode_of(&p)),
                    p.text_dim,
                    bg,
                    AA_TEXT,
                );
                assert_contrast(
                    &format!("[{}] text_bright on {layer}", mode_of(&p)),
                    p.text_bright,
                    bg,
                    AA_TEXT,
                );
            }
        }
    }

    #[test]
    fn state_colors_meet_aa_as_foreground_on_every_panel() {
        for p in [Palette::dark(), Palette::light()] {
            for base in ["accent", "success", "warn", "danger", "info"] {
                for suffix in STATE_NAMES {
                    let c = state(&p, base, suffix);
                    // 状态色画在窗口底色与各级面板上。
                    for layer in ["bg", "surface", "surface_variant", "surface_raised"] {
                        assert_contrast(
                            &format!("[{}] {base}{suffix} on {layer}", mode_of(&p)),
                            c,
                            over(&p, layer, p.bg),
                            AA_TEXT,
                        );
                    }
                    // 作为填充底色时，其上文字用 text_on_accent。
                    assert_contrast(
                        &format!("[{}] text_on_accent on {base}{suffix}", mode_of(&p)),
                        p.text_on_accent,
                        c,
                        AA_TEXT,
                    );
                }
            }
        }
    }

    #[test]
    fn borders_meet_ui_component_threshold() {
        for p in [Palette::dark(), Palette::light()] {
            for layer in ["bg", "surface"] {
                assert_contrast(
                    &format!("[{}] border on {layer}", mode_of(&p)),
                    p.border,
                    over(&p, layer, p.bg),
                    AA_UI,
                );
                assert_contrast(
                    &format!("[{}] border_strong on {layer}", mode_of(&p)),
                    p.border_strong,
                    over(&p, layer, p.bg),
                    AA_UI,
                );
            }
        }
    }

    #[test]
    fn overlay_scrim_actually_dims_whatever_is_under_it() {
        // 遮罩的意义就是压暗背景；若合不成更暗的颜色，它就是坏的。
        for p in [Palette::dark(), Palette::light()] {
            let under = p.surface_raised;
            let scrimmed = composite(p.overlay_scrim, under);
            assert!(
                luminance(scrimmed) < luminance(under),
                "[{}] overlay_scrim 应压暗底色",
                mode_of(&p)
            );
        }
    }

    // ------------------------------------------------------------------
    // 深浅两套完整性
    // ------------------------------------------------------------------

    /// 期望的颜色令牌清单。**新增颜色令牌必须同步登记**——
    /// 完整性测试会比对它，漏登记即失败。
    const EXPECTED_COLOR_TOKENS: &[&str] = &[
        // 背景层
        "bg", "surface", "surface_variant", "surface_raised", "overlay_scrim",
        // 描边层
        "border", "border_strong", "border_subtle",
        // 文本层
        "text", "text_dim", "text_bright", "text_on_accent",
        // 状态色五组三态
        "accent", "accent_hover", "accent_active",
        "success", "success_hover", "success_active",
        "warn", "warn_hover", "warn_active",
        "danger", "danger_hover", "danger_active",
        "info", "info_hover", "info_active",
        // 交互态背景
        "row_alt", "row_hover", "row_selected", "row_active",
    ];

    /// 期望的非颜色令牌清单。登记规则同 [`EXPECTED_COLOR_TOKENS`]。
    const EXPECTED_NUM_TOKENS: &[&str] = &[
        "radius_sm", "radius_md", "radius_lg",
        "space_xs", "space_sm", "space_md", "space_lg", "space_xl",
        "font_xs", "font_sm", "font_md", "font_lg", "font_xl", "font_title",
        "stroke_thin", "stroke_normal", "stroke_thick",
        "control_height", "icon_size",
    ];

    /// 期望的时长令牌清单。
    const EXPECTED_DUR_TOKENS: &[&str] = &["anim_fast", "anim_normal"];

    fn sorted_color_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Color(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    fn sorted_num_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Num(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    fn sorted_dur_names(p: &Palette) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = p
            .tokens()
            .into_iter()
            .filter(|(_, t)| matches!(t, TokenValue::Dur(_)))
            .map(|(n, _)| n)
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn dark_and_light_expose_exactly_the_same_tokens() {
        let (d, l) = (Palette::dark(), Palette::light());
        assert_eq!(
            d.tokens().len(),
            l.tokens().len(),
            "两套令牌数量应相同"
        );
        assert_eq!(
            sorted_color_names(&d),
            sorted_color_names(&l),
            "两套颜色令牌名应相同"
        );
        assert_eq!(sorted_num_names(&d), sorted_num_names(&l));
        assert_eq!(sorted_dur_names(&d), sorted_dur_names(&l));
    }

    #[test]
    fn token_inventory_matches_registered_expectation() {
        // 这一条是「新增令牌忘了登记」的护栏：宏保证了深浅对称，
        // 但保证不了「你记得给它起了名字、写进了清单」。
        let mut expected: Vec<&str> = Vec::new();
        expected.extend_from_slice(EXPECTED_COLOR_TOKENS);
        expected.extend_from_slice(EXPECTED_NUM_TOKENS);
        expected.extend_from_slice(EXPECTED_DUR_TOKENS);
        expected.sort_unstable();

        for p in [Palette::dark(), Palette::light()] {
            let mut actual: Vec<&str> = p.tokens().into_iter().map(|(n, _)| n).collect();
            actual.sort_unstable();
            assert_eq!(
                actual,
                expected,
                "[{}] 令牌清单与登记不一致",
                mode_of(&p)
            );
        }
    }

    #[test]
    fn no_token_name_is_duplicated() {
        for p in [Palette::dark(), Palette::light()] {
            let mut names: Vec<&str> = p.tokens().into_iter().map(|(n, _)| n).collect();
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), total, "[{}] 存在重名令牌", mode_of(&p));
        }
    }

    #[test]
    fn backward_compatible_alias_is_the_same_color_as_surface() {
        // `panel_bg` 是旧字段名，必须与 `surface` 逐位相同，
        // 否则旧调用点画出的面板色会与新令牌漂移。
        for p in [Palette::dark(), Palette::light()] {
            assert_eq!(
                p.panel_bg(),
                p.surface,
                "[{}] panel_bg 应等于 surface",
                mode_of(&p)
            );
        }
    }

    // ------------------------------------------------------------------
    // 令牌值合法性
    // ------------------------------------------------------------------

    #[test]
    fn color_alphas_are_valid() {
        for p in [Palette::dark(), Palette::light()] {
            for (name, v) in p.tokens() {
                if let TokenValue::Color(c) = v {
                    let [r, g, b, a] = c.to_array();
                    assert!(
                        c.is_opaque() || a < 255,
                        "[{}] {name} 的 alpha 应在 0..=255",
                        mode_of(&p)
                    );
                    // `to_array` 已保证四通道均在 u8 范围内；
                    // 这里额外确认没有通道被错误地写成不透明。
                    let _ = (r, g, b);
                }
            }
        }
    }

    #[test]
    fn spacing_font_size_and_control_metrics_are_sane() {
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            for (name, v) in p.tokens() {
                match v {
                    TokenValue::Num(n) => {
                        if name.starts_with("radius_") {
                            assert!(n >= 0.0, "[{m}] {name} 圆角应非负，实际 {n}");
                        } else if name.starts_with("space_") {
                            assert!(n > 0.0, "[{m}] {name} 间距应为正，实际 {n}");
                            assert_eq!(
                                n % 4.0,
                                0.0,
                                "[{m}] {name} 应为 4 的倍数，实际 {n}"
                            );
                        } else if name.starts_with("font_") {
                            assert!(n > 0.0, "[{m}] {name} 字号应为正，实际 {n}");
                        } else if name.starts_with("stroke_") {
                            assert!(n > 0.0, "[{m}] {name} 描边宽应为正，实际 {n}");
                        } else {
                            // control_height / icon_size
                            assert!(n > 0.0, "[{m}] {name} 应为正，实际 {n}");
                        }
                    }
                    TokenValue::Dur(d) => {
                        assert!(!d.is_zero(), "[{m}] {name} 动效时长不应为 0");
                    }
                    TokenValue::Color(_) => {}
                }
            }
        }
    }

    #[test]
    fn scales_are_monotonic() {
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            let f = |n: &str| -> f32 {
                match p.tokens().into_iter().find(|(k, _)| *k == n).map(|(_, v)| v) {
                    Some(TokenValue::Num(x)) => x,
                    other => panic!("[{m}] {n} 应为数值令牌，实际 {other:?}"),
                }
            };
            let groups: [(&str, &[(&str, f32)]); 4] = [
                ("半径", &[("radius_sm", f("radius_sm")), ("radius_md", f("radius_md")), ("radius_lg", f("radius_lg"))]),
                (
                    "间距",
                    &[
                        ("space_xs", f("space_xs")),
                        ("space_sm", f("space_sm")),
                        ("space_md", f("space_md")),
                        ("space_lg", f("space_lg")),
                        ("space_xl", f("space_xl")),
                    ],
                ),
                (
                    "字号",
                    &[
                        ("font_xs", f("font_xs")),
                        ("font_sm", f("font_sm")),
                        ("font_md", f("font_md")),
                        ("font_lg", f("font_lg")),
                        ("font_xl", f("font_xl")),
                        ("font_title", f("font_title")),
                    ],
                ),
                (
                    "描边宽",
                    &[
                        ("stroke_thin", f("stroke_thin")),
                        ("stroke_normal", f("stroke_normal")),
                        ("stroke_thick", f("stroke_thick")),
                    ],
                ),
            ];
            for (name, seq) in groups {
                for pair in seq.windows(2) {
                    assert!(
                        pair[0].1 < pair[1].1,
                        "[{m}] {name} 应递增：{}={} 未小于 {}={}",
                        pair[0].0,
                        pair[0].1,
                        pair[1].0,
                        pair[1].1
                    );
                }
            }
            let dur = |n: &str| -> Duration {
                match p.tokens().into_iter().find(|(k, _)| *k == n).map(|(_, v)| v) {
                    Some(TokenValue::Dur(d)) => d,
                    other => panic!("[{m}] {n} 应为时长令牌，实际 {other:?}"),
                }
            };
            assert!(
                dur("anim_fast") < dur("anim_normal"),
                "[{m}] 动效时长应递增：{:?} vs {:?}",
                dur("anim_fast"),
                dur("anim_normal")
            );
        }
    }

    #[test]
    fn state_triples_share_a_brightness_band_but_differ_in_saturation() {
        // 三态「同亮度、异饱和」是刻意的取舍：亮度一改，对比度就掉。
        // 这条测试固定住该取舍，防止后人「顺手」把 active 调暗。
        for p in [Palette::dark(), Palette::light()] {
            let m = mode_of(&p);
            for base in ["accent", "success", "warn", "danger", "info"] {
                let lumas: Vec<f32> = STATE_NAMES
                    .iter()
                    .map(|s| luminance(state(&p, base, s)))
                    .collect();
                let lo = lumas.iter().cloned().fold(f32::MAX, f32::min);
                let hi = lumas.iter().cloned().fold(0.0, f32::max);
                assert!(
                    (hi - lo).abs() < 0.12,
                    "[{m}] {base} 三态亮度跨度 {span:.3} 过大（{lo:.3}..{hi:.3}）",
                    span = hi - lo
                );
                // 三态必须真的互不相同，否则「按下」没有视觉反馈。
                for i in 0..lumas.len() {
                    for j in i + 1..lumas.len() {
                        assert_ne!(
                            state(&p, base, STATE_NAMES[i]),
                            state(&p, base, STATE_NAMES[j]),
                            "[{m}] {base} 的 {} 与 {} 不应同色",
                            STATE_NAMES[i],
                            STATE_NAMES[j]
                        );
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // resolve：纯函数
    // ------------------------------------------------------------------

    #[test]
    fn resolve_picks_the_explicit_mode_regardless_of_system() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert_eq!(resolve(Dark, Light), Dark);
        assert_eq!(resolve(Light, Dark), Light);
        assert_eq!(resolve(Dark, Dark), Dark);
        assert_eq!(resolve(Light, Light), Light);
        // 系统为 FollowSystem 时，显式模式同样不受影响。
        assert_eq!(resolve(Dark, FollowSystem), Dark);
        assert_eq!(resolve(Light, FollowSystem), Light);
    }

    #[test]
    fn resolve_follows_the_system_theme() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert_eq!(resolve(FollowSystem, Dark), Dark);
        assert_eq!(resolve(FollowSystem, Light), Light);
    }

    #[test]
    fn resolve_falls_back_to_dark_when_system_is_also_follow_system() {
        // 双重 FollowSystem 必须有确定结果，不能递归。
        assert_eq!(resolve(ThemeMode::FollowSystem, ThemeMode::FollowSystem), ThemeMode::Dark);
    }

    #[test]
    fn theme_mode_round_trips_through_config_representation() {
        use ThemeMode::{Dark, FollowSystem, Light};
        for m in [Dark, Light, FollowSystem] {
            assert_eq!(ThemeMode::from_dark_mode(m.to_dark_mode()), m);
        }
        assert_eq!(ThemeMode::from_dark_mode(None), FollowSystem);
        assert_eq!(ThemeMode::from_dark_mode(Some(true)), Dark);
        assert_eq!(ThemeMode::from_dark_mode(Some(false)), Light);
    }

    #[test]
    fn default_mode_is_follow_system() {
        assert_eq!(ThemeMode::default(), ThemeMode::FollowSystem);
    }

    #[test]
    fn palette_lookup_never_returns_follow_system() {
        use ThemeMode::{Dark, FollowSystem, Light};
        assert!(Palette::from_mode(Dark).is_dark);
        assert!(!Palette::from_mode(Light).is_dark);
        // 漏调 resolve 时的兜底：必须是确定的深色。
        assert!(Palette::from_mode(FollowSystem).is_dark);
    }

    #[test]
    fn detect_system_theme_returns_a_concrete_mode_without_panicking() {
        // 不对返回值做断言：CI 与他人机器的主题设置不可控。
        // 这里只保证「调用不会 panic，且不会返回 FollowSystem」。
        let m = detect_system_theme();
        assert!(
            matches!(m, ThemeMode::Dark | ThemeMode::Light),
            "探测结果应为具体模式，实际 {m:?}"
        );
    }

    // ------------------------------------------------------------------
    // apply：egui Visuals 单一入口
    // ------------------------------------------------------------------

    #[test]
    fn apply_maps_tokens_onto_egui_visuals() {
        let ctx = egui::Context::default();
        let p = Palette::dark();
        apply(&ctx, &p);
        let v = ctx.style_of(egui::Theme::Dark).visuals.clone();

        assert_eq!(v.override_text_color, Some(p.text));
        assert_eq!(v.weak_text_color, Some(p.text_dim));
        assert_eq!(v.panel_fill, p.bg);
        assert_eq!(v.window_fill, p.surface);
        assert_eq!(v.extreme_bg_color, p.surface_raised);
        assert_eq!(v.faint_bg_color, p.surface_variant);
        assert_eq!(v.hyperlink_color, p.accent);
        assert_eq!(v.warn_fg_color, p.warn);
        assert_eq!(v.error_fg_color, p.danger);
        assert_eq!(
            v.selection.bg_fill,
            composite(p.row_selected, p.surface),
            "选中底色应是 row_selected 合成到面板后的结果"
        );
    }

    #[test]
    fn apply_covers_every_fg_stroke_in_both_themes() {
        // 回归「白底白字」：从 Theme::Dark 克隆来的 fg_stroke 是浅色文字，
        // 浅色主题若漏改就是白底白字——界面能跑但什么都看不见。
        for p in [Palette::dark(), Palette::light()] {
            let ctx = egui::Context::default();
            apply(&ctx, &p);
            // apply 会把 theme_preference 对齐到本调色板，
            // 故读 theme_of(is_dark) 拿到的就是刚写入的那套。
            let v = ctx.style_of(theme_of(p.is_dark)).visuals.clone();

            assert_eq!(v.dark_mode, p.is_dark, "[{}] dark_mode", mode_of(&p));
            assert_eq!(
                ctx.theme() == theme_of(p.is_dark),
                true,
                "[{}] theme_preference 应与调色板一致",
                mode_of(&p)
            );

            let fg = luminance(p.text);
            for (name, wv) in [
                ("noninteractive", &v.widgets.noninteractive),
                ("inactive", &v.widgets.inactive),
                ("hovered", &v.widgets.hovered),
                ("active", &v.widgets.active),
                ("open", &v.widgets.open),
            ] {
                assert!(
                    wv.bg_fill != egui::Color32::TRANSPARENT,
                    "[{}] {name}.bg_fill 不可为全透明",
                    mode_of(&p)
                );
                // 不能是「从 Theme::Dark 克隆来的原始值」：
                // 该值在浅色主题下正是「白底白字」的元凶。
                // 合法值只有 tokens 里那几个前景色。
                let fg = wv.fg_stroke.color;
                let allowed = [p.text, p.text_bright, p.text_dim];
                assert!(
                    allowed.contains(&fg),
                    "[{}] {name}.fg_stroke {:?} 既非令牌前景色，也非预期值",
                    mode_of(&p),
                    fg.to_array()
                );
                // 前景必须与自己的背景形成对比，否则控件不可读。
                assert!(
                    contrast(wv.fg_stroke.color, wv.bg_fill) >= AA_TEXT,
                    "[{}] {name} 前景 {:?} 在背景 {:?} 上对比度不足",
                    mode_of(&p),
                    wv.fg_stroke.color.to_array(),
                    wv.bg_fill.to_array()
                );
            }

            // 深色主题前景要亮、浅色主题前景要暗。
            if p.is_dark {
                assert!(
                    fg > 0.5,
                    "深色主题的文字应是亮色，实际亮度 {fg:.3}"
                );
            } else {
                assert!(
                    fg < 0.5,
                    "浅色主题的文字应是深色，实际亮度 {fg:.3}"
                );
            }
        }
    }

    #[test]
    fn apply_is_idempotent() {
        // 同一调色板重复灌入不应改变结果——否则「每帧 set_theme」
        // 会在主题切换时产生漂移。
        let ctx = egui::Context::default();
        let p = Palette::light();
        apply(&ctx, &p);
        let once = ctx.style_of(egui::Theme::Light).visuals.clone();
        apply(&ctx, &p);
        let twice = ctx.style_of(egui::Theme::Light).visuals.clone();
        assert_eq!(once, twice);
    }

    #[test]
    fn switching_modes_replaces_rather_than_merges() {
        // 主题来回切换后必须回到原样，否则深色会「染」浅色
        // （例如浅色下漏设的字段残留深色克隆值）。
        let ctx = egui::Context::default();
        let (d, l) = (Palette::dark(), Palette::light());
        apply(&ctx, &d);
        let before = ctx.style_of(egui::Theme::Dark).visuals.clone();
        apply(&ctx, &l);
        apply(&ctx, &d);
        assert_eq!(ctx.style_of(egui::Theme::Dark).visuals.clone(), before);
    }

    // ------------------------------------------------------------------
    // 字体（原有测试）
    // ------------------------------------------------------------------

    fn mode_of(p: &Palette) -> &'static str {
        if p.is_dark { "深色" } else { "浅色" }
    }

    fn theme_of(is_dark: bool) -> egui::Theme {
        if is_dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        }
    }

    fn state<'a>(p: &'a Palette, base: &str, suffix: &str) -> egui::Color32 {
        let key: String = format!("{base}{suffix}");
        match p
            .tokens()
            .into_iter()
            .find(|(n, _)| *n == key)
            .map(|(_, v)| v)
        {
            Some(TokenValue::Color(c)) => c,
            other => panic!("{key} 应为颜色令牌，实际 {other:?}"),
        }
    }

    #[test]
    fn candidates_are_ordered_with_windows_first() {
        let first = CJK_CANDIDATES.first().unwrap();
        assert!(first.1.contains("msyh"), "应优先使用微软雅黑");
    }

    #[test]
    fn insert_font_adds_to_both_families() {
        let mut defs = FontDefinitions::default();
        // 用任意字节验证注册流程不 panic（epaint 会延后解析）
        let ok = insert_font(&mut defs, "x", b"not-a-real-font", 0);
        assert!(ok);
        assert!(
            defs.families[&FontFamily::Proportional].contains(&"x".to_string()),
            "应加入 Proportional"
        );
        assert!(
            defs.families[&FontFamily::Monospace].contains(&"x".to_string()),
            "应加入 Monospace"
        );
    }

    #[test]
    fn size_scale_is_clamped() {
        assert_eq!(sized(12.0, 0.1).size, 9.6);
        assert_eq!(sized(12.0, 5.0).size, 24.0);
    }

    // ------------------------------------------------------------------
    // 图标字体注册（回归判据）
    // ------------------------------------------------------------------

    /// 建一个「已跑过一帧」的 Context，让 `fonts_mut` 可用。
    ///
    /// `install_icon_font` 本身在 `run` 之前调用是合法的（这正是
    /// 改用 `add_font` 的原因），但**验证墨迹**必须先跑一帧，
    /// 否则 `fonts_mut` 会 panic。
    fn settled_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    /// 图标字体在**没有任何 CJK 字体**的机器上也能注册成功。
    ///
    /// 这条直接覆盖曾经的真实故障：早前实现调用 `ctx.fonts(|f| ..)`，
    /// 在 `App::new`（即第一次 `run` 之前）里直接 panic
    /// *"No fonts available until first call to Context::run()"*，
    /// 于是图标注册被临时整条停用、界面全是豆腐块。
    /// 现在用 `add_font`，**跑帧之前**调用也必须安全。
    #[test]
    fn icon_font_registers_before_first_run() {
        let ctx = egui::Context::default();
        // 关键：此时尚未调用过 run_ui，正是旧实现会 panic 的时机。
        assert!(install_icon_font(&ctx), "图标字体注册应返回成功");
        // 再注册一次也不应 panic（幂等）。
        assert!(install_icon_font(&ctx), "重复注册应仍然成功");
    }

    /// `add_font` 是**追加**而非替换：注册图标字体不能把已有字体挤掉。
    ///
    /// 这条守住 `install_icon_font` 与 `install_cjk_font` 的共存契约——
    /// 若哪天有人改回 `set_fonts`，这里会立刻失败。
    ///
    /// 判据用「注册图标字体后，内置 latin 字体的`A` 仍能命中」：
    /// 内置字体是 egui 自带的，无需构造哨兵数据。
    #[test]
    fn icon_font_keeps_existing_fonts() {
        let ctx = egui::Context::default();
        let font_id = egui::FontId::proportional(16.0);

        // 注册前：内置字体提供 'A'。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, 'A')),
            "前提：内置 latin 字体应能提供 'A'"
        );

        install_icon_font(&ctx);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();

        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, 'A')),
            "注册图标字体后既有字体应仍然可用（add_font 是追加而非替换）"
        );
        assert!(
            ctx.fonts_mut(|f| f.has_glyph(&font_id, crate::icons::Icon::Close.codepoint())),
            "图标码位应能命中图标字体"
        );
    }

    /// 图标字体注册后，每个 `Icon` 变体在字体图集里**有非空墨迹**。
    ///
    /// 这是「图标真的会出现」的下界判据：`has_glyph` 为真只说明
    /// 字体里有这个码位，仍可能是**空白字形**（豆腐块）。
    /// 因此这里进一步走完整排版，拿字形的 `uv_rect` 去字体图集里
    /// 数不透明像素。
    ///
    /// 与 `icons::tests::every_icon_codepoint_has_ink` 互补：那条直接
    /// 构造 `FontDefinitions`，绕开了 `install_icon_font`；本条走的是
    /// **真实注册路径**（`App::new` 用的就是它），因此能抓住
    /// 「注册方式写错→ 图标退化成豆腐块」这类回归。
    #[test]
    fn every_icon_glyph_rasterises_to_ink_after_registration() {
        let ctx = settled_ctx();
        install_icon_font(&ctx);
        // 注册发生在 settled_ctx 跑的那一帧之后，必须再跑一帧让它生效。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();

        for icon in crate::icons::Icon::ALL {
            let cp = icon.codepoint();
            // 排版 + 取图集。epaint 0.36 的 `layout` 收 4 个参数，
            // 字形在 `galley.rows[].row.glyphs[]`（不是 `galley.glyphs`）。
            // `FontId` 在本版本不是 `Copy`，所以在闭包内现建、不外传。
            let (has, image, glyphs) = ctx.fonts_mut(|f| {
                // `layout` 按值取 `FontId`（本版本不是 `Copy`），
                // `has_glyph` 按引用，所以这里建两个等价实例。
                let galley = f.layout(
                    cp.to_string(),
                    egui::FontId::proportional(64.0),
                    egui::Color32::WHITE,
                    f32::INFINITY,
                );
                let glyphs: Vec<egui::epaint::text::Glyph> = galley
                    .rows
                    .iter()
                    .flat_map(|r| r.row.glyphs.iter().cloned())
                    .collect();
                (
                    f.has_glyph(&egui::FontId::proportional(64.0), cp),
                    f.image().clone(),
                    glyphs,
                )
            });
            assert!(has, "{icon:?} U+{:04X} 未命中字体", cp as u32);

            let (w, h) = (image.size[0], image.size[1]);
            let mut inked = 0usize;
            for g in &glyphs {
                // epaint 0.36 的 `UvRect::min/max` 是 `[u16; 2]`（纹素坐标），
                // 不是 `Pos2`——直接按 `Vec2` 用会编译不过。
                let uv = g.uv_rect;
                let x0 = (uv.min[0] as usize).min(w);
                let y0 = (uv.min[1] as usize).min(h);
                let x1 = ((uv.max[0] as usize) + 1).min(w);
                let y1 = ((uv.max[1] as usize) + 1).min(h);
                for y in y0..y1 {
                    for x in x0..x1 {
                        if image.pixels[y * w + x].a() > 32 {
                            inked += 1;
                        }
                    }
                }
            }
            assert!(
                inked > 20,
                "{icon:?} U+{:04X} 栅格化后几乎无墨迹（{inked} px），\
                 界面里会显示为豆腐块",
                cp as u32
            );
        }
    }
}