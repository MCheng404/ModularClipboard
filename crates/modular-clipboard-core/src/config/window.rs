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

/// `card_blur` 的 serde 缺省值：0 = 关闭（不抓屏、不上传纹理）。
pub(super) fn default_card_blur() -> u8 {
    0
}

/// 卡片磨砂档位 → 捕获**降采样倍率**。
///
/// # 为什么「降采样」就是「模糊」
///
/// 抓屏走 GDI 的 `HALFTONE` 模式：缩放时做**盒式平均**而不是最近邻
/// 采样。所以「按 1/N 分辨率抓，再拉伸回卡片尺寸」天然就是一次模糊，
/// 不必再跑一遍卷积——CPU 侧省一次全图遍历，GPU 侧零新增资源。
///
/// 档位 1（最轻）→ 倍率 3，档位 10（最重）→ 倍率 12。
/// 倍率再往上，卡片上就只剩几块平均色，纹理感全丢。
pub fn card_blur_downscale(strength: u8) -> u32 {
    (3 + strength.saturating_sub(1) as u32).min(12)
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
