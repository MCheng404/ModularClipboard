//! MCP 工具定义、能力过滤与分发。
//!
//! 10 个工具与 [`McpConfig`] 的 10 个能力布尔一一对应：只有
//! `mcp.enabled == true` 且对应能力为 `true` 的工具才会出现在
//! `tools/list` 里、才能被 `tools/call` 调用。
//!
//! 工具的实际后端在 [`crate::automation`]（Windows 实现）。

use modular_clipboard_core::config::McpConfig;
use serde_json::{Value, json};

/// 工具调用返回的一段内容。
pub enum Content {
    /// 文本（`{type:"text", text}`）。
    Text(String),
    /// 图像（`{type:"image", data:<base64>, mimeType}`）。
    Image { data: Vec<u8>, mime: String },
}

/// 工具调用结果。
pub struct ToolResult {
    pub content: Vec<Content>,
    /// `true` 表示工具执行失败（MCP 用 isError 区分正常/错误结果）。
    pub is_error: bool,
}

impl ToolResult {
    /// 一段文本的成功结果。
    pub fn text(t: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(t.into())],
            is_error: false,
        }
    }

    /// 错误结果（文本 + isError=true）。
    pub fn error(t: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(t.into())],
            is_error: true,
        }
    }

    /// 图像结果。
    pub fn image(data: Vec<u8>, mime: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Image {
                data,
                mime: mime.into(),
            }],
            is_error: false,
        }
    }

    /// 转成 MCP `tools/call` 的 result JSON。
    pub fn to_json(&self) -> Value {
        let content: Vec<Value> = self
            .content
            .iter()
            .map(|c| match c {
                Content::Text(t) => json!({ "type": "text", "text": t }),
                Content::Image { data, mime } => json!({
                    "type": "image",
                    "data": base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        data
                    ),
                    "mimeType": mime,
                }),
            })
            .collect();
        json!({ "content": content, "isError": self.is_error })
    }
}

/// 一个工具的静态定义（名字 / 说明 / 输入 schema）。
struct ToolMeta {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

/// 8 个工具的元数据表（顺序即对 AI 暴露的顺序）。
///
/// 描述用中文，明确告诉 AI 每个参数的含义与单位——AI 靠这段文字
/// 决定如何构造 arguments。
fn tool_metas() -> Vec<ToolMeta> {
    vec![
        ToolMeta {
            name: "screen_capture",
            description: "对屏幕截图，返回 PNG 图像。不传参数时截取主显示器整个屏幕；传 window_title 时截取匹配标题的窗口（模糊匹配，区分大小写）。",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_title": { "type": "string", "description": "要截取的窗口标题（可只给一部分）；省略则截取整个屏幕" }
                }
            }),
        },
        ToolMeta {
            name: "click_element",
            description: "在屏幕指定坐标点击鼠标。坐标为物理屏幕像素（左上角为 0,0）。",
            schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "横坐标（像素）" },
                    "y": { "type": "integer", "description": "纵坐标（像素）" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "鼠标按键，默认 left" },
                    "double_click": { "type": "boolean", "description": "是否双击，默认 false" }
                },
                "required": ["x", "y"]
            }),
        },
        ToolMeta {
            name: "type_text",
            description: "在当前焦点位置模拟键盘输入文本（支持中文等 Unicode）。调用前应先点击目标输入框使其获得焦点。",
            schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要输入的文本" },
                    "enter": { "type": "boolean", "description": "输入完成后是否按回车，默认 false" }
                },
                "required": ["text"]
            }),
        },
        ToolMeta {
            name: "scroll_view",
            description: "在指定位置滚动鼠标滚轮。delta_y 正值向下滚动、负值向上；delta_x 正值向右。单位为滚轮格（通常一格 ~ 行）。",
            schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "滚动位置横坐标，省略则用当前鼠标位置" },
                    "y": { "type": "integer", "description": "滚动位置纵坐标，省略则用当前鼠标位置" },
                    "delta_y": { "type": "integer", "description": "垂直滚动量（格），正向下" },
                    "delta_x": { "type": "integer", "description": "水平滚动量（格），正向右" }
                }
            }),
        },
        ToolMeta {
            name: "resize_window",
            description: "调整匹配标题的窗口的大小（和可选位置）。坐标/尺寸为屏幕像素。",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_title": { "type": "string", "description": "窗口标题（模糊匹配）" },
                    "width": { "type": "integer", "description": "新宽度（像素）" },
                    "height": { "type": "integer", "description": "新高度（像素）" },
                    "x": { "type": "integer", "description": "新的左上角横坐标（可选）" },
                    "y": { "type": "integer", "description": "新的左上角纵坐标（可选）" }
                },
                "required": ["window_title", "width", "height"]
            }),
        },
        ToolMeta {
            name: "window_focus",
            description: "把匹配标题的窗口切换到前台并获得焦点；若窗口最小化则还原。",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_title": { "type": "string", "description": "窗口标题（模糊匹配）" }
                },
                "required": ["window_title"]
            }),
        },
        ToolMeta {
            name: "drag_drop",
            description: "从一个坐标按住鼠标拖拽到另一个坐标后松开（可用于拖放文件、移动滑块等）。",
            schema: json!({
                "type": "object",
                "properties": {
                    "from_x": { "type": "integer", "description": "起点横坐标" },
                    "from_y": { "type": "integer", "description": "起点纵坐标" },
                    "to_x": { "type": "integer", "description": "终点横坐标" },
                    "to_y": { "type": "integer", "description": "终点纵坐标" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "鼠标按键，默认 left" }
                },
                "required": ["from_x", "from_y", "to_x", "to_y"]
            }),
        },
        ToolMeta {
            name: "read_layout",
            description: "获取界面元素布局：返回窗口及其子控件的树（标题、类名、矩形坐标、是否可见）。不传 window_title 时列出所有可见顶层窗口。",
            schema: json!({
                "type": "object",
                "properties": {
                    "window_title": { "type": "string", "description": "窗口标题（模糊匹配）；省略则列出所有顶层窗口" },
                    "depth": { "type": "integer", "description": "子控件遍历深度，默认 3" }
                }
            }),
        },
        ToolMeta {
            name: "key_press",
            description: "模拟按下组合键。写法形如 \"ctrl+shift+s\"、\"alt+f4\"、\"enter\"、\"esc\"、\"pagedown\"。修饰键支持 ctrl/alt/shift/win，主键支持字母、数字、f1-f24 与常见命名键。用于输入法切换、快捷键、确认/取消对话框等 type_text 覆盖不到的场景。",
            schema: json!({
                "type": "object",
                "properties": {
                    "combo": { "type": "string", "description": "组合键字符串，如 ctrl+c、alt+tab、enter" }
                },
                "required": ["combo"]
            }),
        },
        ToolMeta {
            name: "clipboard",
            description: "读写系统剪贴板文本。action=read 返回当前剪贴板内容；action=write 把 text 写入剪贴板。用于验证「复制」类操作是否真的写入了系统剪贴板。",
            schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["read", "write"], "description": "read（默认）或 write" },
                    "text": { "type": "string", "description": "action=write 时要写入的文本" }
                }
            }),
        },
    ]
}

/// 该能力在配置里是否开启（工具名 → McpConfig 字段）。
fn cap_enabled(cfg: &McpConfig, tool: &str) -> bool {
    if !cfg.enabled {
        return false;
    }
    match tool {
        "resize_window" => cfg.resize_window,
        "read_layout" => cfg.read_layout,
        "click_element" => cfg.click_element,
        "type_text" => cfg.type_text,
        "scroll_view" => cfg.scroll_view,
        "screen_capture" => cfg.screen_capture,
        "window_focus" => cfg.window_focus,
        "drag_drop" => cfg.drag_drop,
        "key_press" => cfg.key_press,
        "clipboard" => cfg.clipboard,
        _ => false,
    }
}

/// `tools/list` 结果：只包含总开关开启且对应能力开启的工具。
pub fn list_tools(cfg: &McpConfig) -> Value {
    let tools: Vec<Value> = tool_metas()
        .into_iter()
        .filter(|m| cap_enabled(cfg, m.name))
        .map(|m| {
            json!({
                "name": m.name,
                "description": m.description,
                "inputSchema": m.schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `tools/call` 分发：校验能力 → 取参数 → 调后端。
pub fn call(cfg: &McpConfig, params: &Value) -> ToolResult {
    let name = params.get("name").and_then(Value::as_str);
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let Some(name) = name else {
        return ToolResult::error("缺少工具名 name");
    };
    if !cap_enabled(cfg, name) {
        return ToolResult::error(format!(
            "工具 {name} 未启用：请在设置 → 通信 → MCP 调用设置中开启对应能力"
        ));
    }
    // 非 Windows 平台：后端不可用。
    #[cfg(not(windows))]
    {
        let _ = args;
        return ToolResult::error("界面自动化仅支持 Windows");
    }
    #[cfg(windows)]
    crate::dispatch(name, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_lists_all_tools() {
        let cfg = McpConfig::default();
        let v = list_tools(&cfg);
        // 工具数与 `McpConfig` 的能力字段数必须相等——加能力时漏加
        // 工具（或反之）会在这里立刻暴露。
        assert_eq!(v["tools"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn master_switch_disabled_lists_nothing() {
        let cfg = McpConfig {
            enabled: false,
            ..Default::default()
        };
        let v = list_tools(&cfg);
        assert_eq!(v["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn single_capability_filter() {
        let cfg = McpConfig {
            click_element: false,
            ..Default::default()
        };
        let v = list_tools(&cfg);
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"click_element"));
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn call_disabled_tool_returns_error_result() {
        let cfg = McpConfig {
            enabled: false,
            ..Default::default()
        };
        let r = call(
            &cfg,
            &json!({ "name": "click_element", "arguments": {"x":1,"y":1} }),
        );
        assert!(r.is_error);
    }

    #[test]
    fn text_result_serializes() {
        let r = ToolResult::text("hi");
        let j = r.to_json();
        assert_eq!(j["content"][0]["type"], "text");
        assert_eq!(j["content"][0]["text"], "hi");
        assert_eq!(j["isError"], false);
    }
}
