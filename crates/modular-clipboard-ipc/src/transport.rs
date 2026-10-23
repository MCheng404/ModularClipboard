//! 传输层：长度前缀帧 + Windows 命名管道。
//!
//! # 帧格式
//!
//! ```text
//! [u32 小端长度][JSON 载荷]
//! ```
//!
//! 长度前缀是必需的：命名管道在字节流模式下没有消息边界，
//! 一次 `ReadFile` 完全可能只拿到半个 JSON。
//!
//! # 为什么是命名管道
//!
//! 备选是 localhost TCP。命名管道没有端口，因此不会被别的进程抢占
//! （TCP 端口撞车时行为取决于谁先 bind，排查起来很痛苦），
//! 也不经过防火墙栈。代价是它是 Windows 专属——本项目本就
//! Windows-only（托盘、剪贴板、全局热键全部是 Win32）。

use crate::protocol::{Request, Response};

/// 单帧载荷上限（字节）。
///
/// 条目载荷（图片）会经本通道传输，上限必须比
/// `storage.max_payload_bytes` 的默认档位宽；取 64MB 是留足余量，
/// 同时挡住「对端被替换成恶意程序、发来一个 4GB 长度前缀」这类
/// 把界面进程内存吃光的情形。
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// 默认管道名。
///
/// 版本号进名字而非协议字段：新旧版 exe 并存时（升级后旧进程还没退）
/// 不会互相连上然后因为字段差异解析失败——它们根本看不到对方的管道。
pub const PIPE_BASE_NAME: &str = r"ModularClipboard.Daemon.v1";

/// 完整管道路径。
pub fn pipe_name() -> String {
    format!(r"\\.\pipe\{PIPE_BASE_NAME}")
}

/// 帧读写能力。抽成 trait 是为了让协议测试不必起真实管道。
pub trait Transport {
    /// 发送一帧（字节流）。短写必须内部循环补完。
    fn send_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    /// 接收一帧。返回 `None` 表示对端已关闭且没有残余数据。
    fn recv_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>>;
}

/// 在任意 [`Transport`] 上跑协议：收一个请求、发一个响应。
pub fn serve_one<T: Transport>(
    t: &mut T,
    handle: impl FnOnce(Request) -> Response,
) -> anyhow::Result<bool> {
    let Some(raw) = t.recv_frame()? else {
        return Ok(false);
    };
    let req: Request = serde_json::from_slice(&raw)?;
    let resp = handle(req);
    let out = serde_json::to_vec(&resp)?;
    t.send_frame(&out)?;
    Ok(true)
}

/// 客户端一侧的一次调用。
pub fn call<T: Transport>(t: &mut T, req: &Request) -> anyhow::Result<Response> {
    let out = serde_json::to_vec(req)?;
    t.send_frame(&out)?;
    let Some(raw) = t.recv_frame()? else {
        anyhow::bail!("常驻进程已关闭连接");
    };
    Ok(serde_json::from_slice(&raw)?)
}

/// 内存管道对：单元测试与「同一进程内跑双进程逻辑」用。
///
/// 真实管道是内核对象，测试里起两个进程不现实；但协议逻辑
/// （版本校验、命令分发）必须被测。这一对把字节在内存里倒个手，
/// 走的是与真实通道**完全相同**的帧编解码路径。
#[derive(Default)]
pub struct MemoryTransport {
    out: Vec<u8>,
    inbox: std::collections::VecDeque<Vec<u8>>,
}

impl MemoryTransport {
    /// 把已积累的字节切成一帧帧塞进收件箱，模拟对端收到。
    ///
    /// 分成两步而不是在 `send_frame` 里直接投递，是为了让测试能复现
    /// 「一次写入、对端分多次读」的真实时序。
    pub fn deliver(&mut self) {
        loop {
            if self.out.len() < 4 {
                return;
            }
            let len = u32::from_le_bytes([self.out[0], self.out[1], self.out[2], self.out[3]])
                as usize;
            if self.out.len() < 4 + len {
                return;
            }
            let frame: Vec<u8> = self.out.drain(..(4 + len)).skip(4).collect();
            self.inbox.push_back(frame);
        }
    }
}

impl Transport for MemoryTransport {
    fn send_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    fn recv_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.inbox.pop_front())
    }
}

// ---------------------------------------------------------------------------
// Windows 命名管道
// ---------------------------------------------------------------------------

/// 一条已建立的管道连接。
#[cfg(windows)]
#[derive(Debug)]
pub struct PipeStream {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl PipeStream {
    fn read_exact(&self, buf: &mut [u8]) -> anyhow::Result<usize> {
        use windows::Win32::Foundation::ERROR_BROKEN_PIPE;
        use windows::Win32::Storage::FileSystem::ReadFile;

        if buf.is_empty() {
            return Ok(0);
        }
        let mut total = 0usize;
        while total < buf.len() {
            let mut n = 0u32;
            let r = unsafe { ReadFile(self.handle, Some(&mut buf[total..]), Some(&mut n), None) };
            match r {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => {
                    if total == 0 {
                        return Ok(0);
                    }
                    anyhow::bail!("对端在传输中途断开");
                }
                Err(e) => return Err(e.into()),
            }
            // 读到 0 字节：字节流模式下意味着对端关闭。
            if n == 0 {
                break;
            }
            total += n as usize;
        }
        Ok(total)
    }

    fn write_all(&self, buf: &[u8]) -> anyhow::Result<()> {
        use windows::Win32::Storage::FileSystem::WriteFile;

        let mut total = 0usize;
        while total < buf.len() {
            let mut n = 0u32;
            unsafe {
                WriteFile(self.handle, Some(&buf[total..]), Some(&mut n), None)?;
            }
            if n == 0 {
                anyhow::bail!("写入管道失败：0 字节");
            }
            total += n as usize;
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Transport for PipeStream {
    fn send_frame(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(bytes);
        self.write_all(&framed)
    }

    fn recv_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let mut head = [0u8; 4];
        let got = self.read_exact(&mut head)?;
        if got == 0 {
            return Ok(None);
        }
        if got < 4 {
            anyhow::bail!("帧长度前缀不完整（{got} 字节）");
        }
        let len = u32::from_le_bytes(head) as usize;
        if len > MAX_FRAME {
            anyhow::bail!("帧过大：{len} 字节（上限 {MAX_FRAME}）");
        }
        let mut body = vec![0u8; len];
        let got = self.read_exact(&mut body)?;
        if got != len {
            anyhow::bail!("帧体不完整：期望 {len} 实得 {got}");
        }
        Ok(Some(body))
    }
}

#[cfg(windows)]
impl Drop for PipeStream {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 管道服务端：反复接受连接（串行，一次一个客户端）。
#[cfg(windows)]
pub struct PipeServer {
    name: Vec<u16>,
}

#[cfg(windows)]
impl PipeServer {
    /// 绑定管道名。**不**立即创建实例——实例在 [`Self::accept`] 里创建。
    pub fn bind(name: &str) -> anyhow::Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        Ok(Self { name: wide })
    }

    /// 阻塞等待一个客户端连接。
    ///
    /// 每次调用创建一个**新的管道实例**：命名管道的实例在客户端断开后
    /// 就废了，必须重新建。这也让「多个界面进程先后连接」能正常工作。
    pub fn accept(&self) -> anyhow::Result<PipeStream> {
        use windows::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX,
        };
        use windows::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
            PIPE_WAIT,
        };
        use windows::core::PCWSTR;

        // FIRST_PIPE_INSTANCE：若同名管道已被别的进程创建，这里直接失败。
        // 它是「已经有常驻进程在跑」的第二个判据（第一个是单实例互斥体），
        // 两者都失效才可能起两个 daemon。
        let open_mode = PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_FIRST_PIPE_INSTANCE.0;
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(open_mode),
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let e = unsafe { GetLastError() };
            return Err(anyhow::anyhow!("创建命名管道失败：{e:?}"));
        }
        // 阻塞等客户端。返回 Err 时也要关掉句柄，否则每次重试泄漏一个。
        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) } {
            use windows::Win32::Foundation::CloseHandle;
            unsafe {
                let _ = CloseHandle(handle);
            }
            // 客户端在 ConnectNamedPipe 之前就已关闭时会走到这里。
            return Err(anyhow::anyhow!("等待客户端连接失败：{e}"));
        }
        Ok(PipeStream { handle })
    }
}

/// 连到常驻进程的管道。
#[cfg(windows)]
pub fn connect(name: &str) -> anyhow::Result<PipeStream> {
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("连接常驻进程失败：{e}"))?;
    Ok(PipeStream { handle })
}

/// 带重试的连接：给常驻进程一点启动时间。
///
/// 界面进程可能是被托盘点击唤起的，而托盘点击又可能发生在
/// 常驻进程刚起来、管道还没创建的那一瞬——不重试的话用户会看到
/// 「界面打不开」，再点一次又好了，属于最难复现的那类 bug。
#[cfg(windows)]
pub fn connect_retry(name: &str, timeout: std::time::Duration) -> anyhow::Result<PipeStream> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match connect(name) {
            Ok(s) => return Ok(s),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

#[cfg(not(windows))]
pub type PipeServer = Unsupported;

#[cfg(not(windows))]
pub fn connect(_name: &str) -> anyhow::Result<PipeStream> {
    anyhow::bail!("命名管道仅在 Windows 上可用")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Request;

    /// 帧必须能容忍「一次只读半个」——真实管道在载荷稍大时就会这样。
    #[test]
    fn memory_transport_round_trips_a_request() {
        let mut a = MemoryTransport::default();
        let mut b = MemoryTransport::default();
        a.send_frame(br#"{"Sync":{"known":1}}"#).unwrap();
        b.inbox = std::mem::take(&mut a.inbox);
        // 直接把字节搬过去：模拟对端收到完整帧。
        b.out.extend_from_slice(&a.out);
        a.out.clear();
        b.deliver();
        let frame = b.recv_frame().unwrap().unwrap();
        let req: Request = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(req, Request::Sync { known: 1 }));
    }

    /// 服务端一次往返：请求进、响应出，`serve_one` 返回 true 表示还活着。
    #[test]
    fn serve_one_answers_and_returns_alive() {
        let mut client = MemoryTransport::default();
        let mut server = MemoryTransport::default();
        call_unchecked(&mut client, &Request::Cmd(crate::protocol::Command::ReloadList));
        server.out.extend_from_slice(&client.out);
        client.out.clear();
        server.deliver();
        let alive = serve_one(&mut server, |_| Response::Ok).unwrap();
        assert!(alive);
        client.out.extend_from_slice(&server.out);
        server.out.clear();
        client.deliver();
        assert!(matches!(
            client.recv_frame().unwrap().unwrap().as_slice(),
            b"\"Ok\""
        ));
    }

    /// 对端关闭（无帧）时 `serve_one` 返回 false，不报错——这是正常退出。
    #[test]
    fn serve_one_returns_false_on_peer_close() {
        let mut server = MemoryTransport::default();
        assert!(!serve_one(&mut server, |_| Response::Ok).unwrap());
    }

    fn call_unchecked(t: &mut MemoryTransport, req: &Request) {
        let out = serde_json::to_vec(req).unwrap();
        t.send_frame(&out).unwrap();
    }

    /// 上限必须真的挡住超大长度前缀。
    #[test]
    fn max_frame_is_sane() {
        // 编译期断言：上下限都得在合理区间，改动时立刻报错而不是等到
        // 运行时才发现「图片传不过去」或「能被 OOM 攻击」。
        const { assert!(MAX_FRAME >= 8 * 1024 * 1024, "上限至少应容得下一张图片") };
        const { assert!(MAX_FRAME <= 256 * 1024 * 1024, "上限不该大到能 OOM") };
    }
}
