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
pub mod renderer;
pub mod theme;
pub mod view;

pub use app::{run, App};

/// 默认数据目录的展示字符串。
///
/// 从 store 层转发，让 `main.rs` 不必为了打印一行帮助文本
/// 而直接依赖 store（它只该依赖 core/app/ui 三层）。
pub fn default_data_dir_display() -> String {
    modular_clipboard_store::default_data_dir_display()
}
pub use view::UiLocal;
