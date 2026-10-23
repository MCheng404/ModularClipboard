//! 图标字体数据与度量基线。
//!
//! 字体是离线烘焙的 TTF（`scripts/make_icons.py` 产出），
//! 度量常量与 TTF 头里的 ascender/descender 必须一致。

/// 图标字体的名字，注册进 [`egui::FontDefinitions`] 时用。
pub const ICON_FONT_NAME: &str = "modular-clipboard-icons";

/// 嵌入的图标字体（子集，4080 字节）。
///
/// 只含本模块 [`Icon`] 的 18 个变体，码位 `U+E000..=U+E011`。
/// 由 `assets/` 下的源字体经`pyftsubset` 生成，改动 [`Icon`] 的码位
/// 必须同步重新生成该文件——[`tests::every_icon_codepoint_has_ink`]
/// 会拦住漏改。
pub(crate) static ICON_TTF: &[u8] = include_bytes!("../../assets/modular-clipboard-icons.ttf");

/// 图标的 [`egui::FontData`]。
///
/// 供 [`crate::theme::install_icon_font`] 注册用。走 `from_static`——
/// 字体已经嵌在二进制里，不需要再拷一份所有权。
///
/// baseline 补偿（[`BASELINE_FACTOR`]）挂在这里：
/// [`egui::FontTweak`] 是**注册期**属性，`FontId` 上没有对应字段。
pub fn icon_font_data() -> egui::FontData {
    egui::FontData::from_static(ICON_TTF).tweak(egui::FontTweak {
        y_offset_factor: BASELINE_FACTOR,
        ..Default::default()
    })
}

/// 图标墨迹相对基线的纵向补偿系数（[`egui::FontTweak::y_offset_factor`]）。
///
/// # 为什么需要补偿
///
/// epaint 的行盒高度取自 **OS/2** 的 typo 度量（skrifa 不用 hhea）：
/// `row_height = typoAscender - typoDescender + typoLineGap = 300 - 0 + 27 = 327`
/// 单位，而 em 只有 300。多出来的 27 单位 lineGap 全部堆在基线**下方**，
/// 于是行盒重心相对 em 盒下移，纯几何推算下墨迹平均会偏上
/// `(ascent + descent - lineGap)/2 = 136.5` 单位，而实际墨迹中心均值在 150.7。
///
/// 推导：设 `ink_cy` 为字形墨迹中心（字体单位），`asc/desc/gap` 为 OS/2 度量，
/// epaint 里基线位于 `行顶 + asc`，行盒居中于目标框，于是
///
/// ```text
/// 墨迹中心(相对框心) = (asc + desc - gap)/2 - ink_cy
/// ```
///
/// 代入 `(300 + 0 - 27)/2 - 150.7 = -14.2` 单位（偏上），
/// 折成比例即 `+14.2 / 300 ≈ 0.047`；实测标定值0.0553（差异来自
/// epaint 的整像素对齐与图集量化）。
///
/// # 符号：必须为正
///
/// [`egui::FontTweak::y_offset_factor`] 为负会把墨迹**上移**。
/// 本常量曾经是 `-0.055`——符号写反了，等于在3.5px 的自然偏移上
/// 再叠加 3.5px 反向偏移，实测把`Close` 的上下留白从 5/11 放大成 2/14。
///
/// # 校准方法
///
/// 不是拍脑袋，是 [`tests::report_ink_geometry`] 在 16 / 32 / 64 三个字号下
/// 实测「墨迹中心 − 框心」的均值，取其反号。换字号时因为补偿正比于字号，
/// 三个字号会同时收敛—— 单个字号碰巧居中不算数。
pub const BASELINE_FACTOR: f32 = 0.0553;

/// 字体的 em 大小（`head.unitsPerEm`）。
pub const EM_UNITS: f32 = 300.0;

/// 字体的 `OS/2` typo ascender。**epaint 读的是这一项，不是 `hhea.ascent`。**
///
/// 与 [`DESCENT_UNITS`]、`LINE_GAP_UNITS`] 一起决定行盒高度，
/// 因而是 [`BASELINE_FACTOR`] 存在的原因。
pub const ASCENT_UNITS: f32 = 300.0;

/// 字体的 `OS/2` typo descender（正值）。
pub const DESCENT_UNITS: f32 = 0.0;

/// 字体的 `OS/2` typo lineGap。
///
/// **这27 单位是baseline 补偿的真正来源**：行盒高度
/// `= ascender - descender + lineGap = 327`，比 em（300）高出 27，
/// 于是行盒重心相对 em 盒下移、墨迹整体偏上。
pub const LINE_GAP_UNITS: f32 = 27.0;

/// 行盒高度（字体单位）= `ascender - descender + lineGap`。
///
/// 与 [`EM_UNITS`] 的差值（27）即「需要补偿掉」的那个量。
pub const ROW_HEIGHT_UNITS: f32 = ASCENT_UNITS - DESCENT_UNITS + LINE_GAP_UNITS;
