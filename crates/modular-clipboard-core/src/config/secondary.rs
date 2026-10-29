//! 二级主题（纸质 / 护眼绿 / 星空紫 / OLED）及其可调参数。

use serde::Deserialize;
use serde::Serialize;

use super::theme_params::ThemeKnob;
use crate::i18n::msg;

/// 二级主题：叠加在一级（深浅）模式之上的**风格层**。
///
/// 与 `WindowBackdrop` 的分工：背景效果改的是「窗口怎么透出桌面」，
/// 二级主题改的是「整套配色与质感」。每个二级主题在深浅两种基底下
/// 各有一套取值（见 ui 侧 `theme::SecondaryTheme::apply`），一级切换时
/// 自动落到对应变体，不需要用户选两遍。
///
/// # 为什么不做成「配色方案」平铺
///
/// 一级模式（深浅）是**功能位**：它决定文字与底的明暗关系，错了整个
/// 界面不可读。二级主题是**审美位**：只在同一种明暗关系里换色温与
/// 质感。两者叠成一个枚举会让「OLED + 浅色」这种组合无法表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecondaryTheme {
    /// 不加二级主题（默认）：纯正的一级深浅主题。
    #[default]
    None,
    /// 纸质纹理：暖色纸基 + 程序化纤维/噪点。
    Paper,
    /// 护眼绿：低蓝光绿基。
    Green,
    /// 星空紫：深紫基 + 程序化星点。
    Purple,
    /// OLED：纯黑背景（自发光屏幕省电、暗环境不刺眼）。
    Oled,
}

impl SecondaryTheme {
    /// 设置界面展示名（双语对）。
    pub fn label(self) -> crate::i18n::Msg {
        match self {
            SecondaryTheme::None => msg("None"),
            SecondaryTheme::Paper => msg("Paper"),
            SecondaryTheme::Green => msg("Eye-care green"),
            SecondaryTheme::Purple => msg("Starfield purple"),
            SecondaryTheme::Oled => "OLED",
        }
    }

    /// 配置里的稳定字符串（与 serde 编码一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            SecondaryTheme::None => "none",
            SecondaryTheme::Paper => "paper",
            SecondaryTheme::Green => "green",
            SecondaryTheme::Purple => "purple",
            SecondaryTheme::Oled => "oled",
        }
    }

    /// 从字符串解析，无法识别时回落 [`SecondaryTheme::None`]。
    ///
    /// 容错而不是报错：这个值可能来自用户手装的 config.json，
    /// 为一个 cosmetic 设置让整个配置加载失败不值得。
    pub fn from_str_opt(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "paper" => SecondaryTheme::Paper,
            "green" => SecondaryTheme::Green,
            "purple" => SecondaryTheme::Purple,
            "oled" => SecondaryTheme::Oled,
            _ => SecondaryTheme::None,
        }
    }

    /// 本主题可调的参数旋钮（设置界面据此生成滑块）。
    ///
    /// 键名与 `ThemeParams` 的字段一一对应；`None` 主题没有旋钮。
    pub fn knobs(self) -> &'static [ThemeKnob] {
        match self {
            SecondaryTheme::None => &[],
            SecondaryTheme::Paper => PAPER_KNOBS,
            SecondaryTheme::Green => GREEN_KNOBS,
            SecondaryTheme::Purple => PURPLE_KNOBS,
            SecondaryTheme::Oled => OLED_KNOBS,
        }
    }
}

/// 纸质主题的旋钮（[`SecondaryTheme::Paper`]）。
static PAPER_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("paper_tint", msg("Paper tint"), msg("How strongly the warm paper base mixes in")),
    ThemeKnob::new("paper_texture", msg("Texture strength"), msg("How visible the fibers and grain are")),
];

/// 护眼绿主题的旋钮（[`SecondaryTheme::Green`]）。
static GREEN_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("green_tint", msg("Green tint"), msg("How strongly the green base mixes in")),
    ThemeKnob::new("green_warmth", msg("Warmth"), msg("From cyan (cool) to yellow (warm)")),
];

/// 星空紫主题的旋钮（[`SecondaryTheme::Purple`]）。
static PURPLE_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("purple_tint", msg("Purple tint"), msg("How strongly the purple base mixes in")),
    ThemeKnob::new("purple_stars", msg("Star density"), msg("How visible the background stars are")),
    ThemeKnob::new("star_random", msg("Randomize layout"), msg("0 = one fixed sky, 1 = reshuffle on every rebuild")),
    ThemeKnob::new("star_twinkle", msg("Twinkle"), msg("How strongly stars breathe between bright and dim")),
    ThemeKnob::new("star_size_var", msg("Size variation"), msg("How much star radii vary")),
    ThemeKnob::new("star_cursor", msg("Cursor parallax"), msg("Stars shift opposite to the cursor (parallax)")),
];

/// OLED 主题的旋钮（[`SecondaryTheme::Oled`]）。
static OLED_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("oled_black", msg("Black level"), msg("How far the background is pushed toward pure black")),
    ThemeKnob::new("oled_borders", msg("Borders"), msg("Separate layers with brighter borders on pure black")),
];
