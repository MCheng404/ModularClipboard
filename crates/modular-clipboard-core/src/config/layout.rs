//! 旧四面板架构的持久化格式（遗留 DTO，仅迁移用）。

use serde::Deserialize;
use serde::Serialize;

/// 布局状态的磁盘表示（纯数字 DTO）。
///
/// # 为什么在 core 而不在 ui
///
/// 它是**用户配置的一部分**，必须与 [`UiConfig`] 的其余字段同处一个
/// crate：`Config` 被 core 定义，ui 反过来依赖 core。若把 `LayoutConfig`
/// 留在 ui crate，`UiConfig` 就没法持有它——要么 core 依赖 ui（形成
/// 循环依赖），要么配置里存不成布局。
///
/// 与运行时的 [`LayoutState`](https://docs.rs) 分离的原因：布局用的
/// `Rect` 是 egui 类型，直接序列化会把渲染库的内部结构写进配置文件，
/// 换 egui 版本就得迁移用户数据。这层 DTO 只用原始数字，是**稳定格式**。
///
/// `From<&LayoutState>` 的转换实现留在 ui crate——`LayoutState` 是
/// egui 侧的类型，core 不认识它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// 每个模块：`[是否浮动, 槽位, 是否折叠, x, y, w, h]`。
    ///
    /// 位置与尺寸**只在浮动时有效**，停靠时仍写入——
    /// 停靠→浮动→停靠往返后能恢复原位置。
    pub modules: [[f32; 7]; 4],
    /// 宽度增量。
    pub width_delta: [f32; 4],
    /// 侧栏选中的视图下标。
    pub rail_view: usize,
    /// 顶栏高度。
    pub topbar_height: f32,
    /// 结构版本号，供将来迁移。
    pub version: u32,
}

impl LayoutConfig {
    /// 当前格式版本。
    pub const VERSION: u32 = 1;

    /// 全部为 0 的空布局。**不是**可用的默认布局。
    ///
    /// 真正的默认布局必须由 ui 侧的 `LayoutState::default()` 转换而来
    ///（它要知道每个模块的默认槽位），因此没有 `Default` 实现——
    /// 在这里给一个「看起来像默认」的全零值，调用方一旦误用就会得到
    /// 「所有模块堆在中央槽」的用户可见故障，且没有任何编译期提示。
    pub fn empty() -> Self {
        Self {
            modules: [[0.0; 7]; 4],
            width_delta: [0.0; 4],
            rail_view: 0,
            topbar_height: 0.0,
            version: Self::VERSION,
        }
    }
}
