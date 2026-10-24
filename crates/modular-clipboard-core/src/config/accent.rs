//! 强调色（HSV / 十六进制）与主题令牌覆盖。

use serde::Deserialize;
use serde::Serialize;

use super::ui::UiConfig;
use super::panels::SearchPanelConfig;
use super::panels::TypeRailConfig;
use super::card_row::CardShadowConfig;
use super::card_row::RowMetaConfig;
use super::window::WindowBackdrop;
use super::window::default_glass_dispersion;
use super::window::default_glass_blur;
use super::window::default_glass_edge;
use super::window::default_glass_refract;
use super::window::default_window_blur_strength;
use super::secondary::SecondaryTheme;
use super::theme_params::ThemeParams;

/// 主题强调色（HSV 表示）。
///
/// `None`（配置里不设置）= 用主题内置默认蓝。设置后，一级主题的
/// 蓝系强调色（选中、主操作、相关底色）整体换成用户选的颜色，
/// 深浅两套都换。
///
/// # 为什么用 HSV 而不是 RGB
///
/// 「换主题色」的直觉是「沿着色相环挑一个颜色」：HSV 的 H 滑条正好
/// 对应这个动作，S/V 分别控制鲜艳与明暗。RGB 三个滑条没有一个是
/// 「 hue」，用户想从蓝渐变到紫得同时猜两个分量。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AccentColor {
    /// 色相（度，0..360，0/360 = 红）。
    pub h: f32,
    /// 饱和度（0..1）。
    pub s: f32,
    /// 明度（0..1）。
    pub v: f32,
}

impl AccentColor {
    /// 新建并钳制到合法区间（非有限值分量回落默认蓝的对应分量）。
    pub fn new(h: f32, s: f32, v: f32) -> Self {
        Self {
            h: if h.is_finite() { h.rem_euclid(360.0) } else { 211.0 },
            s: if s.is_finite() { s.clamp(0.0, 1.0) } else { 0.8 },
            v: if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.69 },
        }
    }

    /// 默认蓝（与内置主题的 accent 同色：`#1D65AF` 反推）。
    pub fn default_blue() -> Self {
        Self::from_rgb([0x1D, 0x65, 0xAF])
    }

    /// 转成 RGB 字节（标准 HSV→RGB，`v` 直接作为最大通道）。
    pub fn to_rgb(&self) -> [u8; 3] {
        let c = Self::new(self.h, self.s, self.v);
        let sector = (c.h / 60.0).floor() as i32 % 6;
        let f = (c.h / 60.0).fract();
        let chroma = c.v * c.s;
        let m = c.v - chroma;
        // 中间通道：偶数扇区取 `chroma * f`、奇数扇区取 `chroma * (1 - f)`
        // （`|h/60 mod 2 - 1|` 在两个扇区之间交替）。最小通道恒为 `m`。
        let mid = |k: i32| {
            if k % 2 == 0 {
                chroma * f
            } else {
                chroma * (1.0 - f)
            }
        };
        let (r, g, b) = match sector {
            0 => (c.v, mid(0) + m, m),
            1 => (mid(1) + m, c.v, m),
            2 => (m, c.v, mid(2) + m),
            3 => (m, mid(3) + m, c.v),
            4 => (mid(4) + m, m, c.v),
            _ => (c.v, m, mid(5) + m),
        };
        [
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
        ]
    }

    /// 从 RGB 字节反推 HSV（用于预设色板与十六进制输入落地）。
    pub fn from_rgb(rgb: [u8; 3]) -> Self {
        let [r, g, b] = [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0];
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let h = if d < f32::EPSILON {
            0.0
        } else if max == r {
            60.0 * (((g - b) / d).rem_euclid(6.0))
        } else if max == g {
            60.0 * (((b - r) / d) + 2.0)
        } else {
            60.0 * (((r - g) / d) + 4.0)
        };
        let s = if max < f32::EPSILON { 0.0 } else { d / max };
        Self::new(h, s, max)
    }

    /// `#RRGGBB` 十六进制字符串（设置界面输入框回显）。
    pub fn to_hex(&self) -> String {
        let [r, g, b] = self.to_rgb();
        format!("#{r:02X}{g:02X}{b:02X}")
    }

    /// 解析 `#RRGGBB` / `RRGGBB`（容忍缺失 `#` 与大小写）。
    ///
    /// 失败返回 `None`：输入框正在打字中途（如只敲了 `#F`）不能
    /// 把用户已选的颜色清掉。
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self::from_rgb([r, g, b]))
    }
}

/// 用户对单个主题令牌的覆盖——「界面上任何元素、任何参数都可调」
/// 的落地机制。
///
/// 键是 ui 侧 [`Palette`](crate::ui 不可见，见 `modular-clipboard-ui`
/// 的 `theme::Palette::tokens`) 的字段名；值是**非预乘** RGBA（颜色）
/// 或数值（几何 / 字号 / 描边宽度）。覆盖在一级主题、二级主题、
/// 强调色**之后**应用——用户显式改的旋钮优先级最高。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThemeOverrides {
    /// 令牌名 → 非预乘 RGBA。
    #[serde(default)]
    pub colors: std::collections::BTreeMap<String, [u8; 4]>,
    /// 令牌名 → 数值。
    #[serde(default)]
    pub nums: std::collections::BTreeMap<String, f32>,
}

impl UiConfig {
    /// 当前生效的强调色（配置优先，缺省用内置默认蓝）。
    pub fn accent_color(&self) -> AccentColor {
        self.accent.unwrap_or_else(AccentColor::default_blue)
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            dark_mode: None,
            always_on_top: true,
            hide_on_focus_lost: false,
            window_width: 720.0,
            window_height: 800.0,
            show_tray: true,
            start_minimized: false,
            autostart: false,
            font_path: None,
            font_scale: 1.0,
            language: crate::i18n::LANG_SYSTEM.to_string(),
            layout: None,
            window_pos: None,
            pinned_window_pos: None,
            window_backdrop: WindowBackdrop::None,
            window_opacity: 1.0,
            card_opacity: 1.0,
            card_blur: 0,
            glass_refract: default_glass_refract(),
            glass_dispersion: default_glass_dispersion(),
            glass_blur: default_glass_blur(),
            glass_edge: default_glass_edge(),
            window_blur_strength: default_window_blur_strength(),
            card_shadow: CardShadowConfig::default(),
            row_meta: RowMetaConfig::default(),
            image_inline: true,
            notice_popup: false,
            md_rendered: true,
            type_rail: TypeRailConfig::default(),
            search_panel: SearchPanelConfig::default(),
            browser_path: String::new(),
            explorer_path: String::new(),
            accent: None,
            secondary_theme: SecondaryTheme::None,
            theme_params: ThemeParams::default(),
            theme_overrides: ThemeOverrides::default(),
        }
    }
}
