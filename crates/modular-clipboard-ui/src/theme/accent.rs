//! 强调色：预设色板、HSV 派生与覆盖层。
//!
//! 派生只微调明度不重建——同一个色要同时服务深浅两套主题。

use super::{Palette, TokenValue};
use modular_clipboard_core::i18n::msg;

/// 强调色预设色板（设置界面的一排色块）。
///
/// 取名不按色相名而按「感觉」：用户找的是「那个像微信一样的绿」，
/// 不是「hue=140」。全部取中间明度——同一个色要同时服务深浅两套
/// （派生时只微调明度，不重建），过亮或过暗的色在一套里会不可读。
pub const ACCENT_PRESETS: &[(modular_clipboard_core::i18n::Msg, [u8; 3])] = &[
    (msg("Ocean blue"), [0x3B, 0x82, 0xC4]),
    (msg("Teal"), [0x2A, 0xA7, 0xA0]),
    (msg("Emerald"), [0x4C, 0x9A, 0x4C]),
    (msg("Olive"), [0x8A, 0x9A, 0x3C]),
    (msg("Amber"), [0xC0, 0x8A, 0x2E]),
    (msg("Coral"), [0xC2, 0x5E, 0x4A]),
    (msg("Rose"), [0xC2, 0x44, 0x7F]),
    (msg("Violet"), [0x8A, 0x5C, 0xC2]),
    (msg("Indigo"), [0x5A, 0x6F, 0xD6]),
    (msg("Graphite"), [0x6B, 0x72, 0x80]),
];

/// 由用户强调色派生整套 accent 令牌（覆盖一级主题的蓝系）。
///
/// 派生规则（而不是让用户逐个调五个令牌）：
/// - `accent` = 用户色原样；
/// - `accent_hover` / `accent_active` = 明度 ×0.94 / ×0.88
///   （与内置两套的方向一致：内置深色 accent #84B7EB → hover #7EB6F1
///   也是压暗，浅色 #1D65AF → #1464B8 同样压暗）；
/// - `row_selected` / `pin_tint` = 用户色 + 内置同款 alpha
///   （56/52、18/30——那是多年调出来的「看得出来但不抢字」的值）；
/// - `text_on_accent` 按强调色亮度自动选黑/白（阈值 0.5，与内置两套
///   的实际取值一致）。
pub fn derive_accent(pal: &Palette, accent: &modular_clipboard_core::config::AccentColor) -> Palette {
    let mut p = pal.clone();
    let base = accent.to_rgb();
    let a = egui::Color32::from_rgb(base[0], base[1], base[2]);
    let v_of = |c: egui::Color32| c.r() as f32 / 255.0;
    let hover = shift_value(a, v_of(a) * 0.94);
    let active = shift_value(a, v_of(a) * 0.88);
    p.accent = a;
    p.accent_hover = hover;
    p.accent_active = active;
    // 选中/置顶底色：色相跟随用户，alpha 沿用内置（深浅两套各自的
    // 经验值——深色底上需要更实一点的色调才看得出）。
    let (sel_a, pin_a) = if p.is_dark { (56u8, 30u8) } else { (52u8, 18u8) };
    p.row_selected = egui::Color32::from_rgba_unmultiplied(base[0], base[1], base[2], sel_a);
    p.pin_tint = egui::Color32::from_rgba_unmultiplied(base[0], base[1], base[2], pin_a);
    // 强调色上的文字：按相对亮度自动选（WCAG 的相对亮度近似，
    // 阈值 0.5——内置深色 #84B7EB 亮度 0.69 配黑字、浅色 #1D65AF
    // 亮度 0.36 配白字，阈值 0.5 两边都成立）。
    let lum = 0.2126 * base[0] as f32 / 255.0
        + 0.7152 * base[1] as f32 / 255.0
        + 0.0722 * base[2] as f32 / 255.0;
    p.text_on_accent = if lum > 0.5 {
        egui::Color32::from_rgb(0x0A, 0x0D, 0x12)
    } else {
        egui::Color32::WHITE
    };
    p
}

/// 把一个颜色换到指定明度（保色相饱和度，HSV 空间）。
pub(super) fn shift_value(c: egui::Color32, v: f32) -> egui::Color32 {
    let hsv = modular_clipboard_core::config::AccentColor::from_rgb([c.r(), c.g(), c.b()]);
    let shifted = modular_clipboard_core::config::AccentColor::new(hsv.h, hsv.s, v);
    let rgb = shifted.to_rgb();
    egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// 应用令牌级覆盖（用户显式微调，优先级最高）。
///
/// 逐条 `set_token`：名字或类型对不上的条目**跳过**——覆盖表可能
/// 来自旧版本配置（令牌有增删），为一个失效条目让整个主题加载
/// 失败不值得。
pub fn apply_overrides(pal: &mut Palette, ov: &modular_clipboard_core::config::ThemeOverrides) {
    for (name, rgba) in &ov.colors {
        pal.set_token(
            name,
            TokenValue::Color(egui::Color32::from_rgba_unmultiplied(
                rgba[0], rgba[1], rgba[2], rgba[3],
            )),
        );
    }
    for (name, v) in &ov.nums {
        pal.set_token(name, TokenValue::Num(*v));
    }
}
