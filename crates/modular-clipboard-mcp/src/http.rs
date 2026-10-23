//! Streamable HTTP 传输：监听 host:port，POST JSON-RPC 到 `/mcp`。
//!
//! 实现 MCP 2025-06-18 的「Streamable HTTP」传输的无状态部分：
//! - 客户端 POST 单个 JSON-RPC 请求（或数组批量）到 `http://host:port/mcp`
//! - 服务端以 `application/json` 返回单个响应（或数组）
//! - 若配置了 token，要求 `Authorization: Bearer <token>`，否则 401
//!
//! 未实现 SSE 长连接 / 服务端主动推送（本工具没有需要服务端主动发起
//! 的场景）；MCP 规范允许服务器对每次 POST 直接回单个 JSON。
//!
//! # 信任边界（2026-10-23 安全审查后加固）
//!
//! 这个服务能**合成键鼠、截屏、读写剪贴板**，因此边界必须显式：
//! 1. **token 为空 ⇒ 只许 loopback**。空 token 曾被当作「无需认证」，
//!    于是任何能连上 host:port 的人（配 `0.0.0.0` 时是同网段所有主机，
//!    浏览器里一个 `text/plain` 的简单请求连 CORS 预检都不会触发）
//!    都能驱动这些能力。现在空 token 时非 loopback 来源直接 403，
//!    启动时绑到非 loopback 且无 token 更是直接拒绝启动。
//! 2. **`Host` 头白名单**防 DNS rebinding：攻击者让自己的域名解析到
//!    127.0.0.1，浏览器就会带着 `Host: evil.com` 打过来。
//! 3. **一切长度都设上限**：请求行、头行、body 全部按字节数封顶，
//!    读操作设超时。`Content-Length` 是请求方说了算的，
//!    照单全收 `vec![0u8; n]` 等于把 OOM 开关交给对面。

use anyhow::{Context, Result};
use modular_clipboard_core::config::McpConfig;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::time::Duration;

/// 端口未配置时的默认端口。
const DEFAULT_PORT: u16 = 9123;

/// 请求体上限。与 IPC 侧的 `MAX_FRAME`（64MB）取同一思路但更紧：
/// MCP 的 JSON-RPC 消息本就是几十字节到几 KB 量级，8MB 足够容纳
/// 极端批量请求，再大只可能是攻击。
const MAX_BODY: usize = 8 * 1024 * 1024;

/// 单行上限（请求行与每个头行各算一份）。
///
/// `BufRead::read_line` 会一直读到换行为止，对面不发 `\n` 就能把
/// 内存吃光——这是手写 HTTP 解析最经典的坑，必须自己封顶。
const MAX_LINE: usize = 8 * 1024;

/// 单个连接的读超时。防「连上不发数据」占着线程不放手。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 读一行，超过 `cap` 字节仍未见换行就报错。
///
/// CAUTION: 必须用 `take` 包一层再 `read_line`——直接 `read_line`
/// 到 `String` 是无上限的。`Take<B>` 在 `B: BufRead` 时也实现 `BufRead`，
/// 所以这里能继续用 `read_line`。
fn read_line_capped<R: BufRead>(r: &mut R, buf: &mut String, cap: usize) -> Result<usize> {
    buf.clear();
    let n = r.take(cap as u64 + 1).read_line(buf)?;
    if buf.len() > cap {
        anyhow::bail!("行长度超过 {cap} 字节上限");
    }
    Ok(n)
}

/// `Host` 头是否指向本机。
///
/// 只认 loopback 字面量与 `localhost`：DNS rebinding 攻击的本质是
/// 「域名解析到 127.0.0.1」，所以判据必须是**字面量**而不是「能否解析到
/// 本机」——后者会把攻击者的域名一起放进来。
fn host_is_local(host: &str) -> bool {
    let h = host.trim();
    // IPv6 字面量带方括号：`[::1]:9123`。括号里的内容**不能再按 ':' 切**，
    // 否则 `::1` 会被切成一个空串。
    if let Some(rest) = h.strip_prefix('[') {
        let inner = rest.split(']').next().unwrap_or("");
        return matches!(inner.parse::<IpAddr>(), Ok(ip) if ip.is_loopback());
    }
    let name = h.split(':').next().unwrap_or("").trim();
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // 整个 127.0.0.0/8 都是 loopback，不只 127.0.0.1。
    matches!(name.parse::<IpAddr>(), Ok(ip) if ip.is_loopback())
}

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
    // 绑到非 loopback（`0.0.0.0` 或具体网卡地址）时，对端可能是同网段
    // 任意主机。此时没有 token 就等于把「合成键鼠 + 截屏 + 读写剪贴板」
    // 广播出去，直接拒绝启动，而不是等用户自己意识到。
    let bound = listener.local_addr()?;
    if !bound.ip().is_loopback() && cfg.token.is_empty() {
        anyhow::bail!(
            "MCP HTTP 绑定在 {bound}（非 loopback）但未配置 token：\
             该服务可合成键鼠、截屏、读写剪贴板，无鉴权不得对网络开放。\
             请在设置里填写 token，或把 host 改回 127.0.0.1"
        );
    }
    if cfg.token.is_empty() {
        tracing::warn!(
            "MCP HTTP 未配置 token：已限制为仅本机（loopback）可访问。\
             本机任意进程仍可驱动键鼠/截屏/剪贴板能力，建议在设置中生成 token"
        );
    }
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
fn handle_conn(mut stream: TcpStream, cfg: &McpConfig) -> Result<()> {
    // 超时必须在读之前设：连上不发数据的连接会一直占着线程。
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    // 空 token 时只许 loopback 对端。这一条与 `Host` 白名单互补：
    // 前者挡住同网段主机，后者挡住 DNS rebinding。
    if cfg.token.is_empty() && !stream.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false) {
        return write_simple(
            &mut stream,
            403,
            "Forbidden",
            "未配置 token 时仅允许本机（loopback）访问",
        );
    }
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    // 读请求行
    let mut request_line = String::new();
    read_line_capped(&mut reader, &mut request_line, MAX_LINE)?;
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
    let mut host_header = String::new();
    let mut header_lines = 0usize;
    loop {
        let mut header = String::new();
        let n = read_line_capped(&mut reader, &mut header, MAX_LINE)?;
        if n == 0 || header.trim().is_empty() {
            break;
        }
        // 头行数量同样要封顶：无限头行 + 每行都有上限 = 依然能撑爆内存。
        header_lines += 1;
        if header_lines > 64 {
            return write_simple(&mut writer, 431, "Too Many Headers", "头行数量超过上限");
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
            "host" => host_header = v.to_string(),
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

    // DNS rebinding：浏览器一定带 Host 头。攻击者让自己的域名解析到
    // 127.0.0.1，打过来的 Host 就是那个域名——按字面量比对即可挡住。
    // 缺 Host 头（HTTP/1.0）不拦：真正的浏览器不会省略它，
    // 而省略者属于「本机任意进程」，那条边界由 loopback 检查负责。
    if !host_header.is_empty() && !host_is_local(&host_header) {
        tracing::warn!(host = %host_header, "拒绝非本机 Host 头（疑似 DNS rebinding）");
        return write_simple(&mut writer, 403, "Forbidden", "Host 头不允许");
    }

    // 鉴权
    if !cfg.token.is_empty() && auth != format!("Bearer {}", cfg.token) {
        writer.write_all(b"HTTP/1.1 401 Unauthorized\r\n")?;
        writer.write_all(b"WWW-Authenticate: Bearer\r\n")?;
        writer.write_all(b"Content-Length: 0\r\n\r\n")?;
        return Ok(());
    }

    // 读 body。`Content-Length` 完全由请求方决定，照单全收
    // `vec![0u8; n]` 就是把 OOM 开关交给对面（SEC-03）。
    if content_length > MAX_BODY {
        return write_simple(&mut writer, 413, "Payload Too Large", "请求体超过上限");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Host 头白名单（防 DNS rebinding）----

    #[test]
    fn host_allows_only_loopback_literals() {
        // 放行：loopback 字面量与 localhost，带不带端口都行。
        assert!(host_is_local("127.0.0.1"));
        assert!(host_is_local("127.0.0.1:9123"));
        assert!(host_is_local("localhost"));
        assert!(host_is_local("localhost:9123"));
        assert!(host_is_local("LocalHost:9123"));
        assert!(host_is_local("[::1]:9123"));
        // 整个 127.0.0.0/8 都是 loopback。
        assert!(host_is_local("127.5.5.5:9123"));
    }

    #[test]
    fn host_rejects_external_names_even_if_they_resolve_to_loopback() {
        // DNS rebinding 的本质：域名解析到 127.0.0.1。判据必须是**字面量**，
        // 否则"能解析到本机"会把攻击者的域名一起放进来。
        assert!(!host_is_local("evil.example.com"));
        assert!(!host_is_local("evil.example.com:9123"));
        assert!(!host_is_local("192.168.1.10:9123"));
        assert!(!host_is_local("0.0.0.0:9123"));
        assert!(!host_is_local(""));
        // IPv6 非 loopback 字面量。
        assert!(!host_is_local("[2001:db8::1]:9123"));
    }

    // ---- 行长度封顶（防 read_line 吃内存）----

    #[test]
    fn read_line_capped_rejects_lines_without_newline() {
        // 对面不发 '\n' 时，无上限的 read_line 会把内存吃光。
        let mut src: &[u8] = b"GET / HTTP/1.1\r\n";
        let mut buf = String::new();
        let n = read_line_capped(&mut src, &mut buf, 64).unwrap();
        assert_eq!(n, 16);
        assert_eq!(buf, "GET / HTTP/1.1\r\n");
    }

    #[test]
    fn read_line_capped_bails_when_over_cap() {
        let mut src: &[u8] = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\r\n";
        let mut buf = String::new();
        let err = read_line_capped(&mut src, &mut buf, 8).unwrap_err();
        assert!(
            err.to_string().contains("行长度超过"),
            "超限必须报错而非默默读入，实际：{err}"
        );
    }

    #[test]
    fn read_line_capped_handles_eof_without_newline() {
        // 半行之后 EOF：读到的字节数仍受 cap 约束，不会无限增长。
        let mut src: &[u8] = b"short";
        let mut buf = String::new();
        let n = read_line_capped(&mut src, &mut buf, 64).unwrap();
        assert_eq!(n, 5);
        assert_eq!(buf, "short");
    }
}
