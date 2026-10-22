//! 二级主题的参数面（旋钮定义 + 取值 + 钳制）。

use serde::Deserialize;
use serde::Serialize;

/// 二级主题的一个可调参数（[`SecondaryTheme::knobs`] 的元素）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeKnob {
    /// [`ThemeParams`] 里的字段名（稳定键）。
    pub key: &'static str,
    /// 设置界面展示名（双语对）。
    pub label: crate::i18n::Bi,
    /// 一句话说明（双语对）。
    pub desc: crate::i18n::Bi,
}

impl ThemeKnob {
    pub(super) const fn new(key: &'static str, label: crate::i18n::Bi, desc: crate::i18n::Bi) -> Self {
        Self { key, label, desc }
    }
}

/// 二级主题参数（全部 0..1 归一化，显示侧各自映射量程）。
///
/// 平铺成一个结构体而不是按主题分结构：配置里只有一个
/// `theme_params` 对象，切换主题时各主题的参数**互相保留**——
/// 用户把纸质的纹理强度调到 0.9，切到 OLED 再切回来，还是 0.9。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThemeParams {
    /// 纸质：暖色纸基混合比例（0 = 原主题色，1 = 完全纸基）。
    #[serde(default = "default_param_high")]
    pub paper_tint: f32,
    /// 纸质：纹理强度（0 = 无纹理，1 = 满纹理）。
    #[serde(default = "default_param_mid")]
    pub paper_texture: f32,
    /// 护眼绿：绿意混合比例。
    #[serde(default = "default_param_high")]
    pub green_tint: f32,
    /// 护眼绿：色温（0 = 偏青，1 = 偏黄）。
    #[serde(default = "default_param_mid")]
    pub green_warmth: f32,
    /// 星空紫：紫色基调混合比例。
    #[serde(default = "default_param_high")]
    pub purple_tint: f32,
    /// 星空紫：星点密度。
    #[serde(default = "default_param_mid")]
    pub purple_stars: f32,
    /// 星空紫：星点**分布随机化**强度（0 = 固定种子，每次启动同一片
    /// 星空；1 = 每次重建混入时间熵，且每颗星叠加全幅位置抖动）。
    ///
    /// 中间值按比例混入两者。设计成「确定性 ↔ 随机」的连续旋钮而不是
    /// 布尔：回归测试需要 0 档可断言，用户需要 1 档每次不一样。
    #[serde(default = "default_param_mid")]
    pub star_random: f32,
    /// 星空紫：星点**闪烁**强度（0 = 静态星点；1 = 全幅缓慢闪烁）。
    ///
    /// 闪烁是「夜空是活的」的主要来源——完全静止的星点看起来像一张
    /// 贴图。每颗星有自己的相位与频率（不是齐刷刷地闪）。
    #[serde(default = "default_param_high")]
    pub star_twinkle: f32,
    /// 星空紫：星点**大小不规则度**（0 = 均一大小的哑星；1 = 半径在
    /// 0.6~3.2pt 全范围随机）。
    ///
    /// 真实星空的大小差异来自距离，这里用半径方差模拟——均一的星点
    /// 是「矢量图标感」，大小分级才读出纵深。
    #[serde(default = "default_param_mid")]
    pub star_size_var: f32,
    /// 星空紫：**光标与星点的互动强度**（0 = 关闭互动，星点静止于
    /// 各自位置；1 = 满幅视差——星点随光标位置反向偏移，且越近的星
    /// 偏移越大）。
    ///
    /// 交互语义是「视差」而不是「吸引/排斥」：光标靠近时星点轻轻让开
    /// （像被吹开），比吸引更符合「手电筒照夜空」的直觉，也不会把
    /// 星点拉成一团。光标离开窗口（`pointer.pos()` 为 None）时平滑
    /// 回到原位。
    #[serde(default = "default_param_off")]
    pub star_cursor: f32,
    /// OLED：背景压向纯黑的比例。
    #[serde(default = "default_param_high")]
    pub oled_black: f32,
    /// OLED：描边提亮（纯黑下靠描边区分层级）。
    #[serde(default = "default_param_mid")]
    pub oled_borders: f32,
}

impl Default for ThemeParams {
    fn default() -> Self {
        Self {
            paper_tint: default_param_high(),
            paper_texture: default_param_mid(),
            green_tint: default_param_high(),
            green_warmth: default_param_mid(),
            purple_tint: default_param_high(),
            purple_stars: default_param_mid(),
            star_random: default_param_mid(),
            star_twinkle: default_param_high(),
            star_size_var: default_param_mid(),
            star_cursor: default_param_off(),
            oled_black: default_param_high(),
            oled_borders: default_param_mid(),
        }
    }
}

/// 「互动类」旋钮的缺省值：默认关闭。
///
/// 视差互动是「用户主动要的玩具」，默认开会让光标划过时星点乱晃——
/// 工具软件不该默认动。要开的用户自己拖滑块。
fn default_param_off() -> f32 {
    0.0
}

/// 「浓度类」旋钮的缺省值：默认就有效果，但留出调大空间。
fn default_param_high() -> f32 {
    0.8
}

/// 「效果类」旋钮的缺省值：默认可见但不喧宾夺主。
fn default_param_mid() -> f32 {
    0.5
}

impl ThemeParams {
    /// 按键名读参数（找不到返回 `None`）。
    ///
    /// 与 [`SecondaryTheme::knobs`] 的键名对应，供设置界面与绘制侧
    /// 用同一套字符串寻址。
    pub fn get(&self, key: &str) -> Option<f32> {
        Some(match key {
            "paper_tint" => self.paper_tint,
            "paper_texture" => self.paper_texture,
            "green_tint" => self.green_tint,
            "green_warmth" => self.green_warmth,
            "purple_tint" => self.purple_tint,
            "purple_stars" => self.purple_stars,
            "star_random" => self.star_random,
            "star_twinkle" => self.star_twinkle,
            "star_size_var" => self.star_size_var,
            "star_cursor" => self.star_cursor,
            "oled_black" => self.oled_black,
            "oled_borders" => self.oled_borders,
            _ => return None,
        })
    }

    /// 按键名写参数（找不到返回 `false`，调用方应报错而非静默）。
    pub fn set(&mut self, key: &str, v: f32) -> bool {
        let v = clamp_theme_param(v);
        match key {
            "paper_tint" => self.paper_tint = v,
            "paper_texture" => self.paper_texture = v,
            "green_tint" => self.green_tint = v,
            "green_warmth" => self.green_warmth = v,
            "purple_tint" => self.purple_tint = v,
            "purple_stars" => self.purple_stars = v,
            "star_random" => self.star_random = v,
            "star_twinkle" => self.star_twinkle = v,
            "star_size_var" => self.star_size_var = v,
            "star_cursor" => self.star_cursor = v,
            "oled_black" => self.oled_black = v,
            "oled_borders" => self.oled_borders = v,
            _ => return false,
        }
        true
    }
}

/// 把任意来源的参数值收敛到 0..1（非有限值回落 0.5）。
///
/// 手改 config.json 是常态，越界值不能让滑块或混合比例失控。
pub fn clamp_theme_param(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        default_param_mid()
    }
}
