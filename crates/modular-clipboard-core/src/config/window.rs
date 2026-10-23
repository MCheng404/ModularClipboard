//! 窗口背景效果（不透明 / 自研磨砂 / 半透明）与窗口位置。

use serde::Deserialize;
use serde::Serialize;

/// 窗口背景效果。
///
/// 三选一，互斥：磨砂与半透明都靠交换链的**像素 alpha**（PRE_MULTIPLIED）
/// 与桌面合成，机制相同不能同时开，因此不是位标志而是单选。
///
/// 均为自研实现，不依赖 DWM 原生 backdrop（`DWMWA_SYSTEMBACKDROP_TYPE`
/// 已移除：Win11 22H2+ 才可用、外观不可调、与暗色渲染层偏色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WindowBackdrop {
    /// 无效果：纯色不透明背景。
    #[default]
    None,
    /// 自研磨砂：GDI 实时捕获窗口身后的桌面（`WDA_EXCLUDEFROMCAPTURE`
    /// 自排除，见 `gfx::backdrop`），GPU 三趟卷积后作窗口底图；背景层
    /// 画低 alpha 让模糊透出，文字/卡片保持不透明。
    Acrylic,
    /// 逐像素半透明：背景层画低 alpha，桌面直接透出（无模糊）；
    /// 文字/卡片不透明保证可读性。不再用整窗 `LWA_ALPHA` 淡出
    /// （那会把文字一起变透明，低不透明度下不可读）。
    Translucent,
}

/// `window_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
pub(super) fn default_window_opacity() -> f32 {
    1.0
}

/// `card_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
pub(super) fn default_card_opacity() -> f32 {
    1.0
}

/// `window_blur_strength` 的 serde 缺省值：第 7 档对应历史固定半径 8。
pub(super) fn default_window_blur_strength() -> u8 {
    7
}

/// 一个窗口的位置（屏幕逻辑点，左上角为原点）。
///
/// 独立成结构体而不是两个 `f32`，是为了让 `serde` 的
/// `#[serde(default)]` 在字段缺失时能正确填 `None`。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowPos {
    pub x: f32,
    pub y: f32,
}

impl WindowPos {
    /// 新建。
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}
