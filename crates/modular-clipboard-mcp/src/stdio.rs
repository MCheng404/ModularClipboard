//! stdio 传输：每行一个 JSON-RPC 消息，由 MCP 客户端 spawn 进程。
//!
//! 约定：
//! - 从 stdin 逐行读取请求（`Content-Length` 分帧是 LSP 的做法，MCP
//!   stdio 用换行分隔的 JSON）
//! - 响应写回 stdout，每行一个 JSON
//! - 所有日志必须走 **stderr**——stdout 上任何非协议字节都会让客户端
//!   解析失败

use anyhow::Result;
use modular_clipboard_core::config::McpConfig;
use std::io::{BufRead, BufReader, Write};

/// 阻塞运行 stdio MCP 服务，直到 stdin 关闭。
pub fn run_stdio_blocking(cfg: McpConfig) -> Result<()> {
    crate::make_dpi_aware();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<crate::protocol::Message>(trimmed) {
            Ok(msg) => crate::handle_message(&cfg, &msg).map(|r| r.to_line()),
            Err(e) => Some(
                crate::protocol::Message::err(
                    None,
                    crate::protocol::code::PARSE_ERROR,
                    format!("JSON 解析失败：{e}"),
                )
                .to_line(),
            ),
        };
        if let Some(answer) = answer {
            writeln!(out, "{answer}")?;
            out.flush()?;
        }
    }
    Ok(())
}
