//! 界面层：egui 排版 + 自写 Vulkan 渲染器。
//!
//! 该层只依赖 [`modular_clipboard_app::Service`] 暴露的服务接口，不直接访问数据库或剪贴板，
//! 因此可以整体替换而不影响业务逻辑。
//!
//! # 分层
//!
//! ```text
//! Win32 消息 ──▶ EventLoop::egui_input ──▶ egui::Context::run_ui
//!                                                     │
//!                                            view::draw（业务 UI）
//!                                                     │
//!                                            Context::tessellate
//!                                                     │
//!                                            renderer::Painter（顶点/纹理/描述符）
//!                                                     │
//!                                            FrameRenderer（交换链/提交/呈现）
//! ```
//!
//! 渲染后端是 ash 直驱的原生 Vulkan，**不经过 eframe / wgpu**。

pub mod app;
pub mod icons;
pub mod presence;
pub mod renderer;
pub mod theme;
pub mod thumbnail;
pub mod view;

pub use app::{run, run_with_capture_override, App};
pub use presence::Resident;
pub use view::UiLocal;

/// 默认数据目录的展示字符串。
///
/// 从 store 层转发，让 `main.rs` 不必为了打印一行帮助文本而直接依赖
/// store——按分层约定，bin 只该依赖 core / app / ui 三层。
///
/// 转发而非复制常量：帮助文本一旦与实现里的真实路径漂移，
/// 用户照着它去找数据目录就会找不到。单一数据源在 store。
pub fn default_data_dir_display() -> String {
    modular_clipboard_store::default_data_dir_display()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转发必须**逐字符**等于 store 的输出。
    ///
    /// 之前 `main.rs` 里硬编码了 `%APPDATA%/tiez`，而实现用的是
    /// `modular-clipboard` —— 帮助文本与真实路径不一致，用户照着它
    /// 找数据目录会找不到。这个测试固定住「转发不是二次加工」：
    /// 一旦有人在这里做格式化或截断，立刻失败。
    #[test]
    fn data_dir_display_forwards_store_verbatim() {
        assert_eq!(
            default_data_dir_display(),
            modular_clipboard_store::default_data_dir_display()
        );
    }

    /// 帮助文本里必须出现真实的产品目录名，而不是历史遗留的 `tiez`。
    #[test]
    fn data_dir_display_mentions_product_name() {
        let s = default_data_dir_display();
        assert!(
            s.contains("modular-clipboard"),
            "数据目录展示字符串应含产品名，实际：{s}"
        );
        assert!(
            !s.contains("tiez"),
            "改名后帮助文本不应再出现旧名 tiez，实际：{s}"
        );
    }
}
