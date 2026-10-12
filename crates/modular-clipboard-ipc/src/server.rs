//! 常驻进程一侧的服务端。

use crate::protocol::{Request, Response};
use crate::transport;

/// 管道监听者。
///
/// 串行接受连接：同一时刻只有一个界面进程。多个界面进程本来也不该有
/// （第二个实例会先去唤起第一个），串行处理让状态不需要跨连接加锁。
pub struct Server {
    #[cfg(windows)]
    inner: transport::PipeServer,
}

impl Server {
    /// 绑定默认管道名。
    pub fn bind() -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: transport::PipeServer::bind(&transport::pipe_name())?,
            })
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("命名管道仅在 Windows 上可用")
        }
    }

    /// 阻塞等待一个界面进程连上来。
    pub fn accept(&self) -> anyhow::Result<Connection> {
        #[cfg(windows)]
        {
            Ok(Connection {
                stream: self.inner.accept()?,
            })
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("命名管道仅在 Windows 上可用")
        }
    }
}

/// 一条已建立的界面连接。
pub struct Connection {
    #[cfg(windows)]
    stream: transport::PipeStream,
}

impl Connection {
    /// 收一个请求。`None` 表示界面进程已关闭连接（正常退出）。
    pub fn recv(&mut self) -> anyhow::Result<Option<Request>> {
        #[cfg(windows)]
        {
            let Some(raw) = transport::Transport::recv_frame(&mut self.stream)? else {
                return Ok(None);
            };
            Ok(Some(serde_json::from_slice(&raw)?))
        }
        #[cfg(not(windows))]
        {
            anyhow::bail!("命名管道仅在 Windows 上可用")
        }
    }

    /// 发一个响应。
    pub fn send(&mut self, resp: &Response) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            let out = serde_json::to_vec(resp)?;
            transport::Transport::send_frame(&mut self.stream, &out)
        }
        #[cfg(not(windows))]
        {
            let _ = resp;
            anyhow::bail!("命名管道仅在 Windows 上可用")
        }
    }
}

/// 把「处理一个请求」包装成带错误的响应。
///
/// 处理器里的 `?` 会变成 [`Response::Error`] 而不是让连接断开——
/// 一次「条目不存在」不该把整个界面踢掉线。
///
/// `Hello` / `Sync` / `Cmd` 的语义由调用方（常驻进程）解释：
/// 本 crate 只知道帧与类型，不该替它决定「握手要不要带快照」。
pub fn handle(
    req: Request,
    f: impl FnOnce(Request) -> anyhow::Result<Response>,
) -> Response {
    f(req).unwrap_or_else(|e| Response::Error(e.to_string()))
}
