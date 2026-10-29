//! MCP（Model Context Protocol）JSON-RPC 2.0 消息类型。
//!
//! 协议版本：`2025-06-18`。
//!
//! 只用 `serde_json::Value` 承载 `id` 与 `params`——MCP 客户端的 id
//! 可能是整数也可能是字符串，强类型会在边界处丢失信息。
//!
//! 参考：<https://modelcontextprotocol.io/specification/2025-06-18>

use serde::{Deserialize, Serialize};

/// 本实现支持的 MCP 协议版本。
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// 服务端名称与版本（initialize 结果里的 serverInfo）。
pub const SERVER_NAME: &str = "modular-clipboard";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 一条 JSON-RPC 消息（请求 / 通知 / 响应共用 envelope）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 固定 `"2.0"`。
    pub jsonrpc: String,
    /// 请求/响应 id；通知（notification）不带 id。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<serde_json::Value>,
    /// 方法名（请求/通知）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub method: Option<String>,
    /// 参数（请求/通知）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<serde_json::Value>,
    /// 结果（成功响应）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<serde_json::Value>,
    /// 错误（失败响应）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC 标准错误码。
///
/// 只定义**实际会发**的两个：解析失败（-32700）与方法不存在（-32601）。
/// 参数错误与内部错误按 MCP 约定走 `tools/call` 的 `isError: true`
/// （业务级错误，不中断 JSON-RPC），因此不定义 `INVALID_PARAMS` /
/// `INTERNAL_ERROR`——定义了也没人发，只会让读者以为存在另一条错误路径。
pub mod code {
    /// 服务端收到无法解析的 JSON。
    pub const PARSE_ERROR: i32 = -32700;
    /// 方法不存在 / 未实现。
    pub const METHOD_NOT_FOUND: i32 = -32601;
}

impl Message {
    /// 构造一条请求（测试用）。
    #[cfg(test)]
    pub fn request(id: impl Into<serde_json::Value>, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// 是否为通知（无 id）。
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }

    /// 成功响应。
    pub fn ok(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    /// 失败响应。
    pub fn err(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: None,
            params: None,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// 序列化为一行 JSON（stdio 传输按行分隔）。
    pub fn to_line(&self) -> String {
        // envelope 结构保证序列化不失败；失败时退回空对象由上层记录。
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip_preserves_string_id() {
        let msg = Message::request("abc-1", "ping", serde_json::json!({}));
        let line = msg.to_line();
        let back: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(back.id, Some(serde_json::json!("abc-1")));
        assert_eq!(back.method.as_deref(), Some("ping"));
    }

    #[test]
    fn notification_has_no_id_field() {
        let msg = Message {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some("notifications/initialized".into()),
            params: None,
            result: None,
            error: None,
        };
        let line = msg.to_line();
        // 序列化结果里不应出现 "id"
        assert!(!line.contains("id"));
        assert!(msg.is_notification());
    }

    #[test]
    fn error_response_shape() {
        let msg = Message::err(Some(serde_json::json!(1)), code::METHOD_NOT_FOUND, "nope");
        let e = msg.error.unwrap();
        assert_eq!(e.code, -32601);
        assert_eq!(msg.id, Some(serde_json::json!(1)));
    }
}
