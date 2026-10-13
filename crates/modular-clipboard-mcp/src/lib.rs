//! modular-clipboard MCP 服务。
//!
//! 提供 Model Context Protocol（JSON-RPC 2.0）服务，暴露 10 个界面
//! 自动化工具（每个工具可在设置中单独开关）。两种传输：
//! - **stdio**：`--mcp-server`，每行一个 JSON，由 MCP 客户端 spawn
//! - **Streamable HTTP**：`--mcp-http`，监听 host:port，Bearer token 鉴权
//!
//! 入口：
//! - [`stdio::run_stdio_blocking`]
//! - [`http::run_http_blocking`]
//! - 消息统一由 [`handle_message`] 处理

pub mod protocol;
pub mod tools;

#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod win;

pub mod http;
pub mod stdio;

use modular_clipboard_core::config::McpConfig;
use serde_json::{Value, json};

/// 把当前进程设为 per-monitor V2 DPI 感知。
///
/// 独立的 `--mcp-server` 进程不经过 winit（winit 会自己设 DPI 感知），
/// 必须手动设置，否则在高 DPI 屏上截图是缩放后的、坐标对不上物理像素。
#[cfg(windows)]
pub fn make_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[cfg(not(windows))]
pub fn make_dpi_aware() {}

/// 处理一条 MCP 消息，返回要写回的响应；通知（无 id）返回 `None`。
///
/// 支持的方法：`initialize`、`ping`、`tools/list`、`tools/call`；
/// 其余方法返回 JSON-RPC method not found。
pub fn handle_message(cfg: &McpConfig, msg: &protocol::Message) -> Option<protocol::Message> {
    if msg.is_notification() {
        // notifications/initialized、notifications/cancelled 等无需响应。
        return None;
    }
    let id = msg.id.clone().unwrap_or(Value::Null);
    let method = msg.method.as_deref().unwrap_or("");
    let params = msg.params.clone().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => initialize_result(&params),
        "ping" => json!({}),
        "tools/list" => tools::list_tools(cfg),
        "tools/call" => tools::call(cfg, &params).to_json(),
        _ => {
            return Some(protocol::Message::err(
                Some(id),
                protocol::code::METHOD_NOT_FOUND,
                format!("方法 {method} 未实现"),
            ));
        }
    };
    Some(protocol::Message::ok(id, result))
}

/// `initialize` 结果。
///
/// 客户端可能请求更低的协议版本（params.protocolVersion）：MCP 允许
/// 服务端返回自己实际使用的版本。这里固定返回本实现版本。
fn initialize_result(_params: &Value) -> Value {
    json!({
        "protocolVersion": protocol::PROTOCOL_VERSION,
        "capabilities": {
            "tools": { "listChanged": false },
        },
        "serverInfo": {
            "name": protocol::SERVER_NAME,
            "version": protocol::SERVER_VERSION,
        },
    })
}

/// 8 个工具的 Windows 后端分发。
///
/// 工具调用返回的文本尽量包含「做了什么 + 结果坐标」，便于 AI 核对；
/// 失败返回 [`tools::ToolResult::error`]（isError=true，不中断 JSON-RPC）。
#[cfg(windows)]
pub fn dispatch(name: &str, args: &Value) -> tools::ToolResult {
    let ai = |k: &str| -> Option<i64> { args.get(k).and_then(Value::as_i64) };
    let as_ = |k: &str| -> Option<&str> { args.get(k).and_then(Value::as_str) };
    match name {
        "screen_capture" => match as_("window_title") {
            Some(t) => match win::find_window(t) {
                Some(hwnd) => match capture::capture_window(hwnd) {
                    Ok(png) => tools::ToolResult::image(png, "image/png"),
                    Err(e) => tools::ToolResult::error(format!("窗口截图失败：{e}")),
                },
                None => tools::ToolResult::error(format!("未找到标题包含「{t}」的窗口")),
            },
            None => match capture::capture_full_screen() {
                Ok(png) => tools::ToolResult::image(png, "image/png"),
                Err(e) => tools::ToolResult::error(format!("屏幕截图失败：{e}")),
            },
        },
        "click_element" => {
            let (Some(x), Some(y)) = (ai("x"), ai("y")) else {
                return tools::ToolResult::error("缺少必填参数 x、y");
            };
            let button = input::Button::from_str(as_("button").unwrap_or("left"));
            let dbl = args.get("double_click").and_then(Value::as_bool).unwrap_or(false);
            match input::click(x as i32, y as i32, button, dbl) {
                Ok(()) => tools::ToolResult::text(format!("已在 ({x}, {y}) 完成{}点击", if dbl { "双" } else { "" })),
                Err(e) => tools::ToolResult::error(format!("点击失败：{e}")),
            }
        }
        "type_text" => {
            let Some(text) = as_("text") else {
                return tools::ToolResult::error("缺少必填参数 text");
            };
            let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
            match input::type_text(text, enter) {
                Ok(()) => tools::ToolResult::text(format!("已输入 {} 个字符{}", text.chars().count(), if enter { "并回车" } else { "" })),
                Err(e) => tools::ToolResult::error(format!("输入失败：{e}")),
            }
        }
        "scroll_view" => {
            let x = ai("x").map(|v| v as i32);
            let y = ai("y").map(|v| v as i32);
            let dx = ai("delta_x").unwrap_or(0) as i32;
            let dy = ai("delta_y").unwrap_or(0) as i32;
            if dx == 0 && dy == 0 {
                return tools::ToolResult::error("delta_x 与 delta_y 至少要有一个非零");
            }
            match input::scroll(x, y, dx, dy) {
                Ok(()) => tools::ToolResult::text(format!("已滚动（水平 {dx} 格，垂直 {dy} 格）")),
                Err(e) => tools::ToolResult::error(format!("滚动失败：{e}")),
            }
        }
        "resize_window" => {
            let (Some(title), Some(w), Some(h)) = (as_("window_title"), ai("width"), ai("height"))
            else {
                return tools::ToolResult::error("缺少必填参数 window_title、width、height");
            };
            let pos = match (ai("x"), ai("y")) {
                (Some(x), Some(y)) => Some((x as i32, y as i32)),
                _ => None,
            };
            match win::resize(title, w as i32, h as i32, pos) {
                Ok(()) => tools::ToolResult::text(format!("已将「{title}」调整为 {w}×{h}")),
                Err(e) => tools::ToolResult::error(format!("调整窗口失败：{e}")),
            }
        }
        "window_focus" => {
            let Some(title) = as_("window_title") else {
                return tools::ToolResult::error("缺少必填参数 window_title");
            };
            match win::focus(title) {
                Ok(()) => tools::ToolResult::text(format!("已将「{title}」切换到前台")),
                Err(e) => tools::ToolResult::error(format!("置前失败：{e}")),
            }
        }
        "drag_drop" => {
            let (Some(fx), Some(fy), Some(tx), Some(ty)) =
                (ai("from_x"), ai("from_y"), ai("to_x"), ai("to_y"))
            else {
                return tools::ToolResult::error("缺少必填参数 from_x/from_y/to_x/to_y");
            };
            let button = input::Button::from_str(as_("button").unwrap_or("left"));
            match input::drag(fx as i32, fy as i32, tx as i32, ty as i32, button) {
                Ok(()) => tools::ToolResult::text(format!("已从 ({fx},{fy}) 拖拽到 ({tx},{ty})")),
                Err(e) => tools::ToolResult::error(format!("拖拽失败：{e}")),
            }
        }
        "read_layout" => {
            let title = as_("window_title");
            let depth = ai("depth").unwrap_or(3) as u32;
            let v = win::layout(title, depth);
            tools::ToolResult::text(v.to_string())
        }
        "key_press" => {
            let Some(combo) = as_("combo") else {
                return tools::ToolResult::error("缺少必填参数 combo");
            };
            match input::key_press(combo) {
                Ok(()) => tools::ToolResult::text(format!("已按下组合键「{combo}」")),
                Err(e) => tools::ToolResult::error(format!("按键失败：{e}")),
            }
        }
        "clipboard" => {
            // 默认 read：只读操作更安全，漏传 action 时不应意外**改写**剪贴板。
            let action = as_("action").unwrap_or("read");
            match action {
                "read" => match clipboard::read_text() {
                    Ok(t) => tools::ToolResult::text(format!(
                        "剪贴板文本（{} 个字符）：\n{}",
                        t.chars().count(),
                        t
                    )),
                    Err(e) => tools::ToolResult::error(format!("{e}")),
                },
                "write" => {
                    let Some(text) = as_("text") else {
                        return tools::ToolResult::error("action=write 时必须提供 text");
                    };
                    match clipboard::write_text(text) {
                        Ok(()) => tools::ToolResult::text(format!(
                            "已写入剪贴板（{} 个字符）",
                            text.chars().count()
                        )),
                        Err(e) => tools::ToolResult::error(format!("{e}")),
                    }
                }
                other => tools::ToolResult::error(format!(
                    "action 只能是 read 或 write，收到 {other:?}"
                )),
            }
        }
        _ => tools::ToolResult::error(format!("未实现的工具：{name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> McpConfig {
        McpConfig::default()
    }

    #[test]
    fn initialize_returns_server_info() {
        let req = protocol::Message::request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "clientInfo": { "name": "test", "version": "1" },
            }),
        );
        let resp = handle_message(&cfg(), &req).unwrap();
        assert_eq!(resp.result.unwrap()["serverInfo"]["name"], "modular-clipboard");
    }

    #[test]
    fn initialized_notification_gets_no_response() {
        let note = protocol::Message {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some("notifications/initialized".into()),
            params: None,
            result: None,
            error: None,
        };
        assert!(handle_message(&cfg(), &note).is_none());
    }

    #[test]
    fn unknown_method_is_error() {
        let req = protocol::Message::request(2, "resources/list", json!({}));
        let resp = handle_message(&cfg(), &req).unwrap();
        assert_eq!(resp.error.unwrap().code, protocol::code::METHOD_NOT_FOUND);
    }

    #[test]
    fn ping_returns_empty_object() {
        let req = protocol::Message::request(3, "ping", json!({}));
        let resp = handle_message(&cfg(), &req).unwrap();
        assert_eq!(resp.result.unwrap(), json!({}));
    }
}
