//! 二级主题：纸质 / 护眼绿 / 星空紫 / OLED 的取色。
//!
//! 一级主题决定「骨架色」，二级主题在其上按浓度 `k` 混向目标色；
//! `tint_tokens!` 用宏逐个令牌混色——手写漏字段不会有编译提示，
//! 而症状是「某个控件不跟着变」。

use super::{Palette, mix_straight};

/// 把一个令牌混向目标色（浓度 `k`）。
///
/// 二级主题的取值统一走它：`k=0` 完全是一级主题、`k=1` 完全是二级
/// 主题的目标色。半透明令牌（row_hover 等）的 alpha 也参与混合——
/// 二级主题有权调整交互态的明显程度。
pub(super) fn tint_field(c: &mut egui::Color32, target: egui::Color32, k: f32) {
    *c = mix_straight(*c, target, k);
}

/// 把一组「令牌 → 目标色」混入调色板（浓度 `k`）。
///
/// 用宏而不是手写逐个赋值：新增主题令牌时漏一个字段不会有任何编译
/// 提示，而主题色板漏一个令牌的症状是「某个控件不跟着变」——
/// 最难被肉眼发现的半截状态。
macro_rules! tint_tokens {
    ($pal:expr, $k:expr, $($name:ident => $target:expr),* $(,)?) => {
        $(
            tint_field(&mut $pal.$name, $target, $k);
        )*
    };
}

/// 二级主题：把 `base`（一级调色板）按参数混合成主题色板。
///
/// 每个主题深浅各一套目标色（由 `base.is_dark` 选择），浓度类旋钮
/// 控制混合比例；效果类旋钮（纹理 / 星点）在**渲染层**读（见
/// `tex::paper_tile` / `tex::star_tile`），不参与这里的颜色混合。
pub fn apply_secondary(
    theme: modular_clipboard_core::config::SecondaryTheme,
    base: &Palette,
    params: &modular_clipboard_core::config::ThemeParams,
) -> Palette {
    use modular_clipboard_core::config::SecondaryTheme;
    let mut p = base.clone();
    let k = |v: f32| clamp_param(v);
    match theme {
        SecondaryTheme::None => {}
        SecondaryTheme::Paper => paper_theme(&mut p, k(params.paper_tint)),
        SecondaryTheme::Green => green_theme(&mut p, k(params.green_tint), k(params.green_warmth)),
        SecondaryTheme::Purple => purple_theme(&mut p, k(params.purple_tint)),
        SecondaryTheme::Oled => oled_theme(&mut p, k(params.oled_black), k(params.oled_borders)),
    }
    p
}

/// 参数钳制的本地别名（core 的 [`clamp_theme_param`] 不依赖 egui，
/// 但这里每个主题函数都要用，包一层少打全名）。
pub(super) fn clamp_param(v: f32) -> f32 {
    modular_clipboard_core::config::clamp_theme_param(v)
}

/// 纸质主题：暖色纸基。深浅两套目标色，浓度 `k` 混合。
///
/// 设计约束：纸基**不抢对比度**——文字与边框的目标值都按 4.5:1 / 3:1
/// 选取，只是整体染上暖色温。纹理强度（`paper_texture`）是渲染层
/// 的事（程序化纤维噪点），与这里的颜色浓度解耦。
pub(super) fn paper_theme(p: &mut Palette, k: f32) {
    if k <= 0.0 {
        return;
    }
    if p.is_dark {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0x1C, 0x18, 0x13),
            surface => egui::Color32::from_rgb(0x26, 0x21, 0x19),
            surface_variant => egui::Color32::from_rgb(0x2F, 0x29, 0x20),
            list_bg => egui::Color32::from_rgb(0x17, 0x14, 0x10),
            surface_raised => egui::Color32::from_rgb(0x33, 0x2B, 0x21),
            border => egui::Color32::from_rgb(0x6B, 0x5F, 0x4E),
            border_strong => egui::Color32::from_rgb(0x9C, 0x8E, 0x74),
            border_subtle => egui::Color32::from_rgb(0x33, 0x2C, 0x22),
            text => egui::Color32::from_rgb(0xEB, 0xE3, 0xD3),
            text_dim => egui::Color32::from_rgb(0xC0, 0xB4, 0x9E),
            text_bright => egui::Color32::from_rgb(0xF7, 0xF0, 0xE2),
        );
    } else {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0xF4, 0xEF, 0xE3),
            surface => egui::Color32::from_rgb(0xFB, 0xF7, 0xEC),
            surface_variant => egui::Color32::from_rgb(0xEF, 0xE8, 0xD8),
            list_bg => egui::Color32::from_rgb(0xE9, 0xE1, 0xCF),
            surface_raised => egui::Color32::from_rgb(0xFF, 0xFD, 0xF6),
            border => egui::Color32::from_rgb(0x8A, 0x7E, 0x68),
            border_strong => egui::Color32::from_rgb(0x5C, 0x52, 0x40),
            border_subtle => egui::Color32::from_rgb(0xDC, 0xD2, 0xBC),
            text => egui::Color32::from_rgb(0x2E, 0x2A, 0x20),
            text_dim => egui::Color32::from_rgb(0x5C, 0x55, 0x45),
            text_bright => egui::Color32::from_rgb(0x1A, 0x17, 0x10),
        );
    }
}

/// 护眼绿主题：低蓝光绿基。`warmth` 0 = 偏青（冷）、1 = 偏黄（暖）。
///
/// 两套端点按 `warmth` 预混，再按浓度 `k` 混入一级色板——用户说的
/// 「色温」是绿内部的事，不应该拖慢整条链。
pub(super) fn green_theme(p: &mut Palette, k: f32, warmth: f32) {
    if k <= 0.0 {
        return;
    }
    let w = warmth.clamp(0.0, 1.0);
    // 冷端（青绿）与暖端（黄绿）的 bg / text 目标色；其余令牌只随
    // 浓度走冷端（色温对边框的影响远小于对底色）。
    let (bg_c, bg_w) = if p.is_dark {
        (
            egui::Color32::from_rgb(0x0D, 0x16, 0x13),
            egui::Color32::from_rgb(0x15, 0x19, 0x0E),
        )
    } else {
        (
            egui::Color32::from_rgb(0xE2, 0xF0, 0xEC),
            egui::Color32::from_rgb(0xEE, 0xF2, 0xDF),
        )
    };
    let bg = mix_straight(bg_c, bg_w, w);
    if p.is_dark {
        tint_tokens!(p, k,
            bg => bg,
            surface => egui::Color32::from_rgb(0x15, 0x21, 0x1A),
            surface_variant => egui::Color32::from_rgb(0x1C, 0x2B, 0x22),
            list_bg => egui::Color32::from_rgb(0x0A, 0x13, 0x0D),
            surface_raised => egui::Color32::from_rgb(0x24, 0x34, 0x29),
            border => egui::Color32::from_rgb(0x55, 0x6E, 0x5B),
            border_strong => egui::Color32::from_rgb(0x84, 0xA0, 0x88),
            border_subtle => egui::Color32::from_rgb(0x21, 0x31, 0x26),
            text => egui::Color32::from_rgb(0xDC, 0xEA, 0xDE),
            text_dim => egui::Color32::from_rgb(0xA6, 0xBE, 0xAA),
            text_bright => egui::Color32::from_rgb(0xEE, 0xF7, 0xEF),
        );
    } else {
        tint_tokens!(p, k,
            bg => bg,
            surface => egui::Color32::from_rgb(0xF4, 0xFA, 0xF1),
            surface_variant => egui::Color32::from_rgb(0xDF, 0xED, 0xDB),
            list_bg => egui::Color32::from_rgb(0xDC, 0xE9, 0xD8),
            surface_raised => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border => egui::Color32::from_rgb(0x6E, 0x8A, 0x70),
            border_strong => egui::Color32::from_rgb(0x45, 0x5E, 0x47),
            border_subtle => egui::Color32::from_rgb(0xC9, 0xDD, 0xC6),
            text => egui::Color32::from_rgb(0x1F, 0x2E, 0x1E),
            text_dim => egui::Color32::from_rgb(0x47, 0x57, 0x3F),
            text_bright => egui::Color32::from_rgb(0x14, 0x22, 0x13),
        );
    }
}

/// 星空紫主题：深紫基。星点密度（`purple_stars`）是渲染层的事
/// （程序化星点纹理），这里只换颜色。
pub(super) fn purple_theme(p: &mut Palette, k: f32) {
    if k <= 0.0 {
        return;
    }
    if p.is_dark {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0x13, 0x10, 0x1F),
            surface => egui::Color32::from_rgb(0x1C, 0x18, 0x30),
            surface_variant => egui::Color32::from_rgb(0x26, 0x20, 0x40),
            list_bg => egui::Color32::from_rgb(0x0F, 0x0C, 0x1A),
            surface_raised => egui::Color32::from_rgb(0x2C, 0x25, 0x47),
            border => egui::Color32::from_rgb(0x6A, 0x5F, 0x9E),
            border_strong => egui::Color32::from_rgb(0x9A, 0x8E, 0xC9),
            border_subtle => egui::Color32::from_rgb(0x2A, 0x24, 0x40),
            text => egui::Color32::from_rgb(0xE6, 0xDE, 0xF8),
            text_dim => egui::Color32::from_rgb(0xB3, 0xA6, 0xD4),
            text_bright => egui::Color32::from_rgb(0xF2, 0xED, 0xFF),
        );
    } else {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0xEF, 0xEB, 0xF9),
            surface => egui::Color32::from_rgb(0xF8, 0xF5, 0xFE),
            surface_variant => egui::Color32::from_rgb(0xE6, 0xE0, 0xF5),
            list_bg => egui::Color32::from_rgb(0xE4, 0xDD, 0xF2),
            surface_raised => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border => egui::Color32::from_rgb(0x7E, 0x72, 0xB0),
            border_strong => egui::Color32::from_rgb(0x50, 0x46, 0x80),
            border_subtle => egui::Color32::from_rgb(0xD8, 0xD0, 0xEC),
            text => egui::Color32::from_rgb(0x24, 0x1E, 0x3C),
            text_dim => egui::Color32::from_rgb(0x4E, 0x46, 0x70),
            text_bright => egui::Color32::from_rgb(0x15, 0x0F, 0x2E),
        );
    }
}

/// OLED 主题：纯黑背景 + 提亮描边。
///
/// `black` 控制背景压向纯黑的比例；`borders` 单独控制描边提亮——
/// 纯黑底下层级只能靠边框与投影区分（深色投影在纯黑上完全不可见，
/// 这是物理必然），所以描边亮度必须可调。
///
/// 浅色模式下的 OLED = 「纸白极简」：把底色推向纯白、文字压向近黑、
/// 描边加深。自发光屏的「纯黑」只对暗色界面有意义，浅色下的对应
/// 概念是「最大对比的纸白」。
pub(super) fn oled_theme(p: &mut Palette, k: f32, borders: f32) {
    if k <= 0.0 {
        return;
    }
    if p.is_dark {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0x00, 0x00, 0x00),
            surface => egui::Color32::from_rgb(0x05, 0x05, 0x05),
            surface_variant => egui::Color32::from_rgb(0x0C, 0x0C, 0x0C),
            list_bg => egui::Color32::from_rgb(0x00, 0x00, 0x00),
            surface_raised => egui::Color32::from_rgb(0x12, 0x12, 0x12),
            text => egui::Color32::from_rgb(0xEF, 0xEF, 0xEF),
            text_dim => egui::Color32::from_rgb(0xA8, 0xA8, 0xA8),
            text_bright => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
        );
        // 描边按 borders 单独混合（默认值已经偏亮，见主题注释）。
        tint_field(&mut p.border, egui::Color32::from_rgb(0x4A, 0x4A, 0x4A), borders);
        tint_field(&mut p.border_strong, egui::Color32::from_rgb(0x78, 0x78, 0x78), borders);
        tint_field(&mut p.border_subtle, egui::Color32::from_rgb(0x2A, 0x2A, 0x2A), borders);
    } else {
        tint_tokens!(p, k,
            bg => egui::Color32::from_rgb(0xFC, 0xFC, 0xFC),
            surface => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            surface_variant => egui::Color32::from_rgb(0xF4, 0xF4, 0xF4),
            list_bg => egui::Color32::from_rgb(0xF0, 0xF0, 0xF0),
            surface_raised => egui::Color32::from_rgb(0xFF, 0xFF, 0xFF),
            border => egui::Color32::from_rgb(0x9A, 0x9A, 0x9A),
            border_strong => egui::Color32::from_rgb(0x55, 0x55, 0x55),
            border_subtle => egui::Color32::from_rgb(0xE2, 0xE2, 0xE2),
            text => egui::Color32::from_rgb(0x1A, 0x1A, 0x1A),
            text_dim => egui::Color32::from_rgb(0x50, 0x50, 0x50),
            text_bright => egui::Color32::from_rgb(0x00, 0x00, 0x00),
        );
    }
}
