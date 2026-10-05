//! 界面层：egui 排版 + 自写 Vulkan 渲染器。
//!
//! 该层只依赖 [`tiez_app::Service`] 暴露的服务接口，不直接访问数据库或剪贴板，
//! 因此可以整体替换而不影响业务逻辑。
//!
//! # 分层
//!
//! ```
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
pub mod renderer;
pub mod theme;
pub mod view;

pub use app::{run, App};
pub use view::UiLocal;
