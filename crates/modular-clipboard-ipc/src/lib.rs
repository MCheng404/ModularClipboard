//! 界面进程与常驻进程之间的通信。
//!
//! # 背景
//!
//! 窗口与后台同进程时，关掉窗口**换不回内存**：Vulkan 设备与交换链
//! 的开销由驱动持有，销毁后进入驱动池而不是还给系统
//! （实测数据见 `docs/内存分析-2026-10-12.md`）。
//!
//! 拆进程后：常驻进程只做「监听剪贴板 + 存历史 + 托盘」，
//! 界面进程按需启动、关闭即退出，内存由操作系统整体回收。
//!
//! # 通道
//!
//! Windows 命名管道，长度前缀帧 + JSON，请求-响应式。
//! 界面进程是**唯一**的发起方：常驻进程从不主动推消息，
//! 界面每帧一次 `Sync` 问「有新内容吗」。这样常驻侧不需要
//! 反向连接、也不需要异步运行时。

pub mod client;
pub mod instance;
pub mod protocol;
pub mod server;
pub mod transport;

pub use client::Client;
pub use protocol::{
    Command, DataVersion, PROTOCOL_VERSION, Request, Response, Snapshot,
};
pub use server::{Connection, Server};
pub use instance::{
    UiInstanceGuard, UI_WINDOW_TITLE, hide_ui_window, request_show_ui, ui_is_running,
    ui_window_exists, ui_window_hwnd, ui_window_visible,
};
pub use transport::pipe_name;
