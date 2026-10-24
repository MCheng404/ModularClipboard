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
    /// 液态玻璃：在磨砂之上再加一层 GPU 折射（两级：中心穹顶 + 边缘
    /// 轮廓）、色散与 Fresnel 边缘光。
    ///
    /// 与 [`Self::Acrylic`] 的关系：玻璃**复用** Acrylic 的抓屏与模糊链
    /// （同一份 `capture_img` + `blur_a`），只在其上多画一趟玻璃 pass。
    /// 所以它是「磨砂 + 光学层」，不是另一套背景机制。
    ///
    /// 审美取向（折射多强、色散多宽、亮边多亮）**不设默认答案**——
    /// 全部做成可调项，由用户定。
    Glass,
}

/// `window_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
pub(super) fn default_window_opacity() -> f32 {
    1.0
}

/// `card_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
pub(super) fn default_card_opacity() -> f32 {
    1.0
}

/// 玻璃折射强度的 serde 缺省。
///
/// 0.35 来自 bergice/liquidglass 的边缘轮廓折射系数（`refractVec * 0.35`），
/// 是「能看出折射、又不把内容撕走」的经验值。用户可调 0 ~ 1。
pub(super) fn default_glass_refract() -> f32 {
    0.35
}

/// 色散强度缺省（0 = 关闭色散，1 = 满）。
///
/// 色散 = R/G/B 各用一个 IOR（1.50/1.52/1.54）分别采样，边缘因此出现
/// 彩色描边。开太大会像「显卡坏了」，故缺省取中等偏弱。
pub(super) fn default_glass_blur() -> u8 {
    7
}

pub(super) fn default_glass_dispersion() -> f32 {
    0.5
}

/// Fresnel 边缘光幂次缺省。
///
/// `pow(1 - |dot(eye, n)|, power)`：越小光晕越铺开到整张卡片，越大越集中
/// 在边缘一两像素。5.0 是 Heckel 教程里的值，边缘一圈亮线。
pub(super) fn default_glass_edge() -> f32 {
    5.0
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
/// 卡片磨砂档位 → 降采样之后再叠的**盒式模糊趟数**。
///
/// 降采样本身是块平均，放大回卡片尺寸会看见方格边界与随窗口位置
/// 抖动的接缝。叠几趟 3×3 分离式盒式把它抹平——盒式迭代定理：3 趟
/// 已相当接近高斯。档位越高越需要它（倍率大 ⇒ 方块更大）。
pub fn card_blur_passes(strength: u8) -> u32 {
    (1 + strength as u32 / 4).min(3)
}

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
