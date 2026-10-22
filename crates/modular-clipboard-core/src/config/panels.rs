//! 悬浮面板配置：类型过滤栏、搜索面板、停靠位置与几何钳制。

use serde::Deserialize;
use serde::Serialize;

/// 类型过滤栏配置（第三轮原型移植项）。
///
/// 停靠某侧时该侧让出 `size + RAIL_GAP` 的条带（见 ui 侧
/// `paint::FloatLayout`），浮动按钮不压列表内容。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TypeRailConfig {
    /// 是否显示类型过滤栏。
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 是否始终显示全部类型（默认关 = 仅有该类型记录时才出现）。
    #[serde(default)]
    pub always: bool,
    /// 多选组合过滤（默认关 = 单选：再点一次取消）。
    ///
    /// 开启后类型栏按钮变成开关：可同时选中多个类型，列表显示
    /// 任一所选类型的条目；取消过滤需点「全部」或逐个再点一次。
    #[serde(default)]
    pub multi: bool,
    /// 按钮上显示类型名称文字（默认关 = 仅图标）。
    ///
    /// 图标对不熟悉的用户偏抽象（尤其 Markdown 的自绘徽标），
    /// 文字标签把「点什么过滤什么」写明白；竖直停靠时文字在图标
    /// 下方，水平停靠时在右侧。
    #[serde(default)]
    pub labels: bool,
    /// 停靠的主窗口边缘（默认右）。
    #[serde(default)]
    pub pos: TypeRailPos,
    /// 图标边长（逻辑点，钳制 [`TYPE_RAIL_SIZE_MIN`]~[`TYPE_RAIL_SIZE_MAX`]）。
    #[serde(default = "default_type_rail_size")]
    pub size: f32,
}

impl Default for TypeRailConfig {
    fn default() -> Self {
        Self {
            visible: true,
            always: false,
            multi: false,
            labels: false,
            pos: TypeRailPos::Right,
            size: default_type_rail_size(),
        }
    }
}

/// `image_inline` / `md_rendered` / [`TypeRailConfig::visible`] 的 serde 缺省值。
pub(super) fn default_true() -> bool {
    true
}

/// `notice_popup` 的 serde 缺省值（窗口内 toast，既有观感）。
pub(super) fn default_notice_popup() -> bool {
    false
}

/// 悬浮搜索面板配置（第三轮原型移植项）。
///
/// 面板本体是一条搜索输入框（ historically 固定 30pt 高、贴在
/// 主窗口上方通栏）。两个参数都收拢进来：停靠位置决定它从哪一侧
/// 滑入，高度决定面板与输入框的呼吸空间。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SearchPanelConfig {
    /// 停靠位置：主窗口上方（默认）/ 下方。
    #[serde(default)]
    pub pos: SearchPanelPos,
    /// 面板高度（逻辑点，钳制 [`SEARCH_PANEL_H_MIN`]~[`SEARCH_PANEL_H_MAX`]）。
    #[serde(default = "default_search_panel_height")]
    pub height: f32,
}

impl Default for SearchPanelConfig {
    fn default() -> Self {
        Self {
            pos: SearchPanelPos::Top,
            height: default_search_panel_height(),
        }
    }
}

/// `search_panel_height` 的 serde 缺省值（与历史固定高度一致）。
fn default_search_panel_height() -> f32 {
    30.0
}

/// 搜索面板高度的合法区间（逻辑点）。与设置侧滑块范围一致。
pub const SEARCH_PANEL_H_MIN: f32 = 30.0;
/// 见 [`SEARCH_PANEL_H_MIN`]。
pub const SEARCH_PANEL_H_MAX: f32 = 56.0;

/// 把任意来源的高度值收敛到合法区间（非有限值回落 30.0）。
pub fn clamp_search_panel_height(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(SEARCH_PANEL_H_MIN, SEARCH_PANEL_H_MAX)
    } else {
        default_search_panel_height()
    }
}

/// 搜索面板停靠位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchPanelPos {
    /// 主窗口上方（默认）。
    #[default]
    Top,
    /// 主窗口下方。
    Bottom,
}

impl SearchPanelPos {
    /// 设置界面展示名（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            SearchPanelPos::Top => ("窗口上方", "Top"),
            SearchPanelPos::Bottom => ("窗口下方", "Bottom"),
        }
    }
}

/// `type_rail_size` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
fn default_type_rail_size() -> f32 {
    32.0
}

/// `type_rail_size` 的合法区间（逻辑点）。与设置侧滑块范围一致。
pub const TYPE_RAIL_SIZE_MIN: f32 = 24.0;
/// 见 [`TYPE_RAIL_SIZE_MIN`]。
pub const TYPE_RAIL_SIZE_MAX: f32 = 48.0;

/// 把任意来源的尺寸值收敛到合法区间（非有限值回落 32.0）。
///
/// 与 [`clamp_font_scale`] 同构：手改 config.json 是常态，
/// 不能让一个越界值把图标画成 0 或 480。
pub fn clamp_type_rail_size(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(TYPE_RAIL_SIZE_MIN, TYPE_RAIL_SIZE_MAX)
    } else {
        default_type_rail_size()
    }
}

/// 类型过滤栏停靠的主窗口边缘。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeRailPos {
    /// 右侧（默认），垂直居中。
    #[default]
    Right,
    /// 左侧，垂直居中。
    Left,
    /// 顶部，横向居中。
    Top,
    /// 底部，横向居中。
    Bottom,
}

impl TypeRailPos {
    /// 设置界面下拉框展示名（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            TypeRailPos::Right => ("右侧", "Right"),
            TypeRailPos::Left => ("左侧", "Left"),
            TypeRailPos::Top => ("顶部", "Top"),
            TypeRailPos::Bottom => ("底部", "Bottom"),
        }
    }

    /// 配置里的稳定字符串（与 serde 编码一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            TypeRailPos::Right => "right",
            TypeRailPos::Left => "left",
            TypeRailPos::Top => "top",
            TypeRailPos::Bottom => "bottom",
        }
    }

    /// 从字符串解析，无法识别时回落 [`TypeRailPos::Right`]。
    ///
    /// 容错而不是报错：这个值来自用户手装的 config.json，
    /// 为一个 cosmetic 设置让整个配置加载失败不值得。
    pub fn from_str_opt(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" => TypeRailPos::Left,
            "top" => TypeRailPos::Top,
            "bottom" => TypeRailPos::Bottom,
            _ => TypeRailPos::Right,
        }
    }
}
