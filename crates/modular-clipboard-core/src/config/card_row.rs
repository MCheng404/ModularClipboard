//! 列表卡片阴影与行内元信息（类型 / 大小）配置。

use serde::Deserialize;
use serde::Serialize;

/// 列表卡片阴影配置。
///
/// 渲染分两遍：先为所有可见行画投影，再画卡片本体——投影只在
/// 行间缝隙（`ROW_GAP`）与列表边缘可见，不会压暗相邻卡片的文字。
/// 投影本身是多层同心圆角矩形叠加出的**高斯衰减**（不是单条
/// 抗锯齿边），圆角处没有 alpha 重叠暗斑。
///
/// # 为什么颜色不开放
///
/// 投影色几乎总是黑色（深色主题下也是黑的，只是基准浓度更高），
/// 开放颜色只会多出一个没人用的旋钮；「自定义」集中在
/// 模糊（笔触软硬）、浓度（深浅）、垂直偏移（高低）三个
/// 真正影响观感的维度上。
///
/// 取值范围（绘制侧会再钳制一遍，防手改配置文件超界）：
/// - `blur`：0 ~ 24（高斯扩散半径，0 = 无投影）
/// - `opacity`：0.0 ~ 0.6（浓度倍率；0.16 = 1.0× 主题强度，默认 0.20 ≈ 1.25×，
///   实际像素 alpha 再按主题令牌 `Palette::shadow` 缩放，
///   深色主题会自动比浅色更浓）
/// - `offset_y`：-8 ~ 16（逻辑点，正值向下）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardShadowConfig {
    /// 是否启用列表卡片阴影。
    pub enabled: bool,
    /// 模糊半径（逻辑点，高斯扩散半径）。
    pub blur: f32,
    /// 浓度倍率：0.16 为 1.0× 主题强度，0 为无投影，上限 0.6。
    pub opacity: f32,
    /// 垂直偏移（逻辑点，正值向下）。
    pub offset_y: f32,
}

impl Default for CardShadowConfig {
    fn default() -> Self {
        // 2026-10-15 定稿：预览页验收后的推荐参数。
        Self {
            enabled: true,
            blur: 8.0,
            opacity: 0.20,
            offset_y: 0.0,
        }
    }
}

/// 列表行右下角信息组合的一个元素。
///
/// 三个元素可在设置中：
/// - 单击切换显隐（灰 = 不显示）；
/// - 按住拖动重排前后顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowMetaPart {
    /// 圆角小方块类型徽标（内嵌类型图标）。
    Icon,
    /// 类型文字标签（文本 / HTML / 图片 / 文件）。
    #[serde(rename = "type")]
    Type,
    /// 大小文字（B / KB / MB / GB，自适应单位）。
    Size,
}

impl RowMetaPart {
    /// 全部三个元素（自然顺序），供设置编辑器与渲染侧遍历。
    pub const ALL: [RowMetaPart; 3] = [RowMetaPart::Icon, RowMetaPart::Type, RowMetaPart::Size];

    /// 元素在设置编辑器中的名称（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            RowMetaPart::Icon => ("图标", "Icon"),
            RowMetaPart::Type => ("类型", "Type"),
            RowMetaPart::Size => ("大小", "Size"),
        }
    }
}

/// 列表行右下角「类型徽标 + 类型 + 大小」组合的配置。
///
/// - `order`：三个元素从左到右的排列顺序；
/// - `icon` / `type` / `size`：各元素是否显示，默认全部显示。
///
/// 元素之间只留固定间距，不再用「·」分隔。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowMetaConfig {
    /// 组合顺序（从左到右）。默认 图标 → 类型 → 大小。
    pub order: [RowMetaPart; 3],
    /// 是否显示类型徽标。
    pub icon: bool,
    /// 是否显示类型文字标签。
    #[serde(rename = "type")]
    pub type_: bool,
    /// 是否显示大小文字。
    pub size: bool,
}

impl Default for RowMetaConfig {
    fn default() -> Self {
        Self {
            order: RowMetaPart::ALL,
            icon: true,
            type_: true,
            size: true,
        }
    }
}

impl RowMetaConfig {
    /// 该元素当前是否显示。
    pub fn visible(&self, part: RowMetaPart) -> bool {
        match part {
            RowMetaPart::Icon => self.icon,
            RowMetaPart::Type => self.type_,
            RowMetaPart::Size => self.size,
        }
    }

    /// 切换某个元素的显隐（设置编辑器中单击）。
    pub fn toggle(&mut self, part: RowMetaPart) {
        match part {
            RowMetaPart::Icon => self.icon = !self.icon,
            RowMetaPart::Type => self.type_ = !self.type_,
            RowMetaPart::Size => self.size = !self.size,
        }
    }

    /// 把 `part` 移动到目标索引 `idx`（拖动重排）：先从原位置取出，
    /// 再插入到 `idx`，其余元素保持相对顺序不变。`idx` 越界会被钳制
    /// 到合法区间；元素不存在时不改动。
    ///
    /// 拖拽 UI 按鼠标位置直接算出目标索引（鼠标左侧有几个元素就插到
    /// 第几），因此每帧结果只取决于鼠标位置、天然幂等，不会在两个
    /// 相邻位置间来回抖动。
    pub fn move_to_index(&mut self, part: RowMetaPart, idx: usize) {
        let Some(cur) = self.order.iter().position(|p| *p == part) else {
            return;
        };
        let mut v: Vec<RowMetaPart> = self.order.to_vec();
        let moved = v.remove(cur);
        let idx = idx.min(v.len());
        v.insert(idx, moved);
        self.order = [v[0], v[1], v[2]];
    }
}
