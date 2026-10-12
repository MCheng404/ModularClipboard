//! MCP 服务连通性探测。
//!
//! # 为什么需要它
//!
//! MCP 服务是**独立进程**（`modular-clipboard.exe --mcp-server` /
//! `--mcp-http`，由 MCP 客户端 spawn），与 GUI 进程没有共享内存。因此
//! 界面上的「已连接 / 未连接」**不可能**靠读某个局部变量得到——必须真的
//! 去探一下。
//!
//! 早前这个徽标取的是 `MCP_CAPS` 里一个**编译期常量** `ready`（「该能力
//! 是否已实现」）。于是只要代码里写了这个能力，界面就永远显示「已连接」，
//! 与服务是否在运行毫无关系——把「功能已实现」误标成「连接正常」，
//! 用户据此以为配好了，实际一次都调不通。
//!
//! # 探测方式
//!
//! HTTP 传输有一个**免鉴权**的 `GET /health`（见 `mcp/src/http.rs`），
//! 返回 `{"service":"modular-clipboard-mcp", ...}`。本模块发这个请求并
//! 校验响应里确实含 `modular-clipboard-mcp`——**只看端口能不能连是不够的**，
//! 那可能连到任意一个恰好占用该端口的程序，从而谎报「已连接」。
//!
//! # 为什么放在后台线程
//!
//! 连不上时的 `connect_timeout` 是实打实的等待。放在帧循环里会卡界面
//! （远程/被防火墙丢包的地址要等满超时），因此探测跑在独立线程，
//! 界面只读一个原子量。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use modular_clipboard_core::config::McpConfig;

/// 两次探测之间的间隔。
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
/// 建立连接的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// 读取响应的超时。
const READ_TIMEOUT: Duration = Duration::from_millis(400);
/// 停止标志的轮询粒度（让 `stop` 不必等满 `PROBE_INTERVAL`）。
const STOP_POLL: Duration = Duration::from_millis(100);

/// MCP 服务的连通状态。
///
/// 四态而不是布尔：`未配置` 与 `探测失败` 的处理方式完全不同
/// （前者要用户去填地址，后者要用户去启动服务），合成一个「未连接」
/// 会让界面无法给出有用的提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpConn {
    /// 总开关关闭。
    #[default]
    Disabled,
    /// 未配置 HTTP 地址（`port == 0`）。
    ///
    /// stdio 模式由客户端 spawn 本程序、走管道通信，**本进程无从探测**；
    /// 因此这里如实报「未配置」而不是谎报「未连接」。
    Unconfigured,
    /// 探测到 MCP 服务在监听。
    Connected,
    /// 配置了地址，但探测失败。
    Disconnected,
}

impl McpConn {
    /// 徽标文案。
    pub fn label(self) -> &'static str {
        match self {
            Self::Connected => "已连接",
            _ => "未连接",
        }
    }

    /// 是否算「已连接」。
    pub fn is_connected(self) -> bool {
        self == Self::Connected
    }

    /// 状态说明（给界面上的状态行用）。
    pub fn detail(self) -> &'static str {
        match self {
            Self::Disabled => "总开关已关闭",
            Self::Unconfigured => "未配置 HTTP 端口；stdio 模式由客户端托管，无法检测",
            Self::Connected => "服务运行中",
            Self::Disconnected => "未检测到服务（请确认已用 --mcp-http 启动）",
        }
    }

    /// 紧凑编码，供原子量存储。
    fn code(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Unconfigured => 1,
            Self::Connected => 2,
            Self::Disconnected => 3,
        }
    }

    /// 从紧凑编码还原。未知值按 `Disconnected` 处理（保守：不谎报已连接）。
    fn from_code(c: u8) -> Self {
        match c {
            0 => Self::Disabled,
            1 => Self::Unconfigured,
            2 => Self::Connected,
            _ => Self::Disconnected,
        }
    }
}

/// 探测目标。由界面每帧同步过来。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Target {
    enabled: bool,
    host: String,
    port: u16,
}

impl Target {
    /// 从配置构造。
    fn from_config(cfg: &McpConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            host: cfg.host.clone(),
            port: cfg.port,
        }
    }

    /// 实际要连的 `host:port`。
    ///
    /// 与 `mcp/src/http.rs` 的默认值保持一致：host 空则 `127.0.0.1`。
    /// 两处若不一致，界面会去探一个没人监听的地址，永远显示未连接。
    fn addr(&self) -> String {
        let host = if self.host.trim().is_empty() {
            "127.0.0.1"
        } else {
            self.host.trim()
        };
        format!("{host}:{}", self.port)
    }

    /// 该目标是否值得探测（总开关开 且 配了端口）。
    fn probeable(&self) -> bool {
        self.enabled && self.port != 0
    }
}

/// MCP 连通性探测器：后台线程 + 原子状态。
pub struct McpProbe {
    target: Arc<Mutex<Target>>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
}

impl McpProbe {
    /// 启动探测线程。
    pub fn start() -> Self {
        let target = Arc::new(Mutex::new(Target::default()));
        let state = Arc::new(AtomicU8::new(McpConn::Disabled.code()));
        let stop = Arc::new(AtomicBool::new(false));

        let (t, s, k) = (target.clone(), state.clone(), stop.clone());
        std::thread::Builder::new()
            .name("mcp-probe".into())
            .spawn(move || probe_loop(&t, &s, &k))
            .expect("无法启动 MCP 探测线程");

        Self { target, state, stop }
    }

    /// 同步探测目标。目标没变时是空操作（避免每帧无谓加锁+克隆）。
    pub fn update(&self, cfg: &McpConfig) {
        let next = Target::from_config(cfg);
        let mut guard = match self.target.lock() {
            Ok(g) => g,
            // 锁中毒：说明探测线程 panic 过。此时不能 `unwrap` 把界面也带走，
            // 退化为「保持上一次目标」即可。
            Err(_) => return,
        };
        if *guard != next {
            *guard = next;
        }
    }

    /// 当前连通状态（原子读，界面每帧调用）。
    pub fn state(&self) -> McpConn {
        McpConn::from_code(self.state.load(Ordering::Relaxed))
    }

    /// 请求停止探测线程（幂等）。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for McpProbe {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 探测线程主循环。
fn probe_loop(target: &Arc<Mutex<Target>>, state: &Arc<AtomicU8>, stop: &Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let t = match target.lock() {
            Ok(g) => g.clone(),
            Err(_) => Target::default(),
        };
        let now = if !t.enabled {
            McpConn::Disabled
        } else if !t.probeable() {
            McpConn::Unconfigured
        } else if probe_health(&t.addr()) {
            McpConn::Connected
        } else {
            McpConn::Disconnected
        };
        state.store(now.code(), Ordering::Relaxed);

        // 可中断的睡眠：`stop` 后最多等 STOP_POLL 就退出，
        // 不必等满整个 PROBE_INTERVAL。
        let mut slept = Duration::ZERO;
        while slept < PROBE_INTERVAL {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(STOP_POLL);
            slept += STOP_POLL;
        }
    }
}

/// 向 `addr` 发一次 `GET /health`，判断它是否为本项目的 MCP 服务。
///
/// 只认响应体里的服务标识：端口能连上不代表对面就是 MCP 服务
/// （任何程序都可能占着那个端口），那样会把「连上了别的程序」
/// 报成「MCP 已连接」。
fn probe_health(addr: &str) -> bool {
    let Ok(mut sock_addr) = addr.to_socket_addrs() else {
        return false;
    };
    let Some(sa) = sock_addr.next() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    // `Connection: close` 让服务端在响应后关连接，`read_to_string` 才会返回。
    let req = format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    // 读失败（超时/对端直接关）时 resp 里仍是已读到的部分——
    // 对健康检查而言，只要头部标识到了就够，不必读完整。
    let _ = stream.read_to_string(&mut resp);
    resp.contains("modular-clipboard-mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, host: &str, port: u16) -> McpConfig {
        McpConfig {
            enabled,
            host: host.to_string(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn empty_host_falls_back_to_loopback() {
        // 与 `mcp/src/http.rs` 的默认值必须一致，否则会去探一个没人监听的地址。
        let t = Target::from_config(&cfg(true, "", 9123));
        assert_eq!(t.addr(), "127.0.0.1:9123");
        let t = Target::from_config(&cfg(true, "  ", 9123));
        assert_eq!(t.addr(), "127.0.0.1:9123", "空白 host 也应回落");
        let t = Target::from_config(&cfg(true, "192.168.1.5", 8080));
        assert_eq!(t.addr(), "192.168.1.5:8080");
    }

    #[test]
    fn only_configured_targets_are_probed() {
        // 总开关关：不探测（省掉无谓的网络活动）。
        assert!(!Target::from_config(&cfg(false, "127.0.0.1", 9123)).probeable());
        // 端口 0 = 未配置 HTTP 地址（用户多半在用 stdio）。
        assert!(!Target::from_config(&cfg(true, "127.0.0.1", 0)).probeable());
        assert!(Target::from_config(&cfg(true, "127.0.0.1", 9123)).probeable());
    }

    #[test]
    fn state_round_trips_through_code() {
        for s in [
            McpConn::Disabled,
            McpConn::Unconfigured,
            McpConn::Connected,
            McpConn::Disconnected,
        ] {
            assert_eq!(McpConn::from_code(s.code()), s);
        }
        // 未知编码不得被当成「已连接」——宁可少报，不可谎报。
        assert_eq!(McpConn::from_code(99), McpConn::Disconnected);
        assert_eq!(McpConn::from_code(255), McpConn::Disconnected);
    }

    #[test]
    fn only_connected_maps_to_connected_label() {
        assert!(McpConn::Connected.is_connected());
        assert_eq!(McpConn::Connected.label(), "已连接");
        for s in [
            McpConn::Disabled,
            McpConn::Unconfigured,
            McpConn::Disconnected,
        ] {
            assert!(!s.is_connected(), "{s:?} 不应算已连接");
            assert_eq!(s.label(), "未连接");
            assert!(!s.detail().is_empty(), "每种未连接态都要有可读的说明");
        }
        assert_eq!(McpConn::default(), McpConn::Disabled);
    }

    #[test]
    fn update_ignores_unchanged_target() {
        // 目标未变时不应反复加锁+克隆（每帧都会调一次）。
        // 这里只验证「同配置产出的目标相等」，即跳过判据成立。
        let a = Target::from_config(&cfg(true, "127.0.0.1", 9123));
        let b = Target::from_config(&cfg(true, "127.0.0.1", 9123));
        assert_eq!(a, b);
        let c = Target::from_config(&cfg(true, "127.0.0.1", 9124));
        assert_ne!(a, c, "端口变化必须被识别");
    }

    #[test]
    fn probe_fails_fast_on_dead_port() {
        // 连一个几乎不可能有人监听的端口：必须返回 false 而不是 panic。
        // 端口号取高位随机区间，降低撞上真实服务的概率。
        assert!(!probe_health("127.0.0.1:1"));
    }
}
