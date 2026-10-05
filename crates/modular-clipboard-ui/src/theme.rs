//! 主题与字体。
//!
//! 字体是关键：egui 内置字体不含 CJK 字形，若不加载系统中文字体，
//! 所有中文都会渲染成方块。因此启动时按优先级探测系统字体并注册为
//! Proportional 的最高优先级回退。

use egui::{FontData, FontDefinitions, FontFamily, FontId};

/// 配色。集中定义，避免界面里出现魔法数字。
pub struct Palette {
    pub accent: egui::Color32,
    pub warn: egui::Color32,
    pub text_dim: egui::Color32,
    pub row_alt: egui::Color32,
    pub row_hover: egui::Color32,
    pub panel_bg: egui::Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            accent: egui::Color32::from_rgb(0x4C, 0x8D, 0xF6),
            warn: egui::Color32::from_rgb(0xE2, 0x4B, 0x4A),
            text_dim: egui::Color32::from_gray(150),
            row_alt: egui::Color32::from_black_alpha(12),
            row_hover: egui::Color32::from_white_alpha(18),
            panel_bg: egui::Color32::from_rgb(0x1E, 0x1E, 0x1E),
        }
    }

    pub fn light() -> Self {
        Self {
            accent: egui::Color32::from_rgb(0x18, 0x5F, 0xA5),
            warn: egui::Color32::from_rgb(0xA3, 0x2D, 0x2D),
            text_dim: egui::Color32::from_gray(110),
            row_alt: egui::Color32::from_black_alpha(6),
            row_hover: egui::Color32::from_black_alpha(12),
            panel_bg: egui::Color32::from_gray(248),
        }
    }
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
}