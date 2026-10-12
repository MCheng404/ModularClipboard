//! Streamable HTTP 传输：监听 host:port，POST JSON-RPC 到 `/mcp`。
//!
//! 实现 MCP 2025-06-18 的「Streamable HTTP」传输的无状态部分：
//! - 客户端 POST 单个 JSON-RPC 请求（或数组批量）到 `http://host:port/mcp`
//! - 服务端以 `application/json` 返回单个响应（或数组）
//! - 若配置了 token，要求 `Authorization: Bearer <token>`，否则 401
//!
//! 未实现 SSE 长连接 / 服务端主动推送（本工具没有需要服务端主动发起
//! 的场景）；MCP 规范允许服务器对每次 POST 直接回单个 JSON。

use anyhow::{Context, Result};
use modular_clipboard_core::config::McpConfig;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

/// 端口未配置时的默认端口。
const DEFAULT_PORT: u16 = 9123;

/// 阻塞运行 HTTP MCP 服务。
pub fn run_http_blocking(cfg: McpConfig) -> Result<()> {
    crate::make_dpi_aware();
    let host = if cfg.host.is_empty() {
        "127.0.0.1"
    } else {
        cfg.host.as_str()
    };
    let port = if cfg.port == 0 { DEFAULT_PORT } else { cfg.port };
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).with_context(|| format!("绑定 {addr} 失败（端口可能被占用）"))?;
    tracing::info!("MCP HTTP 服务已启动：http://{addr}/mcp");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg2 = cfg.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_conn(stream, &cfg2) {
                        tracing::debug!("HTTP 连接处理结束：{e}");
                    }
                });
            }
            Err(e) => tracing::warn!("接受连接失败：{e}"),
        }
    }
    Ok(())
}

/// 处理一条 HTTP 连接。
fn handle_conn(stream: TcpStream, cfg: &McpConfig) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    // 读请求行
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return write_simple(&mut writer, 400, "Bad Request", "请求行不完整");
    }
    let method = parts[0];
    let path = parts[1];
    // 读 headers
    let mut content_length = 0usize;
    let mut auth = String::new();
    let mut content_type = String::new();
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        if n == 0 || header.trim().is_empty() {
            break;
        }
        // 字段名小写匹配，**值保留原始大小写**（Bearer token 区分大小写，
        // 早期把整行 lowercase 导致 "Bearer secret" 变成 "bearer secret"）。
        let Some((k, v)) = header.split_once(':') else { continue };
        let k = k.trim().to_lowercase();
        let v = v.trim();
        match k.as_str() {
            "content-length" => content_length = v.parse().unwrap_or(0),
            "authorization" => auth = v.to_string(),
            "content-type" => content_type = v.to_string(),
            _ => {}
        }
    }

    // 健康检查：GET / 返回服务信息。
    if method == "GET" && (path == "/" || path == "/health") {
        let body = serde_json::json!({
            "service": "modular-clipboard-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "endpoint": "/mcp",
        })
        .to_string();
        return write_json(&mut writer, 200, &body);
    }

    if method != "POST" || (path != "/mcp" && path != "/") {
        return write_simple(&mut writer, 404, "Not Found", "MCP 端点为 POST /mcp");
    }

    // 鉴权
    if !cfg.token.is_empty() && auth != format!("Bearer {}", cfg.token) {
        writer.write_all(b"HTTP/1.1 401 Unauthorized\r\n")?;
        writer.write_all(b"WWW-Authenticate: Bearer\r\n")?;
        writer.write_all(b"Content-Length: 0\r\n\r\n")?;
        return Ok(());
    }

    // 读 body
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    let _ = content_type;
    let body_str = String::from_utf8_lossy(&body);

    // 单对象或批量数组
    let answer = if body_str.trim_start().starts_with('[') {
        match serde_json::from_str::<Vec<crate::protocol::Message>>(&body_str) {
            Ok(msgs) => {
                let answers: Vec<String> = msgs
                    .iter()
                    .filter_map(|m| crate::handle_message(cfg, m).map(|r| r.to_line()))
                    .collect();
                format!("[{}]", answers.join(","))
            }
            Err(e) => crate::protocol::Message::err(
                None,
                crate::protocol::code::PARSE_ERROR,
                format!("批量请求解析失败：{e}"),
            )
            .to_line(),
        }
    } else {
        match serde_json::from_str::<crate::protocol::Message>(&body_str) {
            Ok(msg) => match crate::handle_message(cfg, &msg) {
                Some(r) => r.to_line(),
                None => "{}".into(),
            },
            Err(e) => crate::protocol::Message::err(
                None,
                crate::protocol::code::PARSE_ERROR,
                format!("JSON 解析失败：{e}"),
            )
            .to_line(),
        }
    };

    write_json(&mut writer, 200, &answer)
}

/// 写一个 JSON 响应。
fn write_json(w: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    w.write_all(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes())?;
    w.write_all(b"Content-Type: application/json; charset=utf-8\r\n")?;
    w.write_all(b"Cache-Control: no-store\r\n")?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())?;
    w.write_all(body.as_bytes())?;
    Ok(())
}

/// 写一个纯文本响应。
fn write_simple(w: &mut TcpStream, status: u16, reason: &str, body: &str) -> Result<()> {
    w.write_all(format!("HTTP/1.1 {status} {reason}\r\n").as_bytes())?;
    w.write_all(b"Content-Type: text/plain; charset=utf-8\r\n")?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())?;
    w.write_all(body.as_bytes())?;
    Ok(())
}
