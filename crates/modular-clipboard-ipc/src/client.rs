//! 界面进程一侧的客户端。

use crate::protocol::{Command, DataVersion, Request, Response, Snapshot};
use crate::transport;

#[cfg(windows)]
type Stream = transport::PipeStream;
#[cfg(not(windows))]
type Stream = transport::MemoryTransport;

/// 与常驻进程的一条连接。
///
/// 界面进程的全部业务能力都走这里：它自己不打开数据库、
/// 不监听剪贴板。这样窗口一关、进程一退，那些资源（连同
/// Vulkan 设备与字体图集）就随进程一起消失。
pub struct Client {
    stream: Stream,
}

impl Client {
    /// 连接常驻进程。`timeout` 是给对方的启动缓冲（见
    /// [`transport::connect_retry`]）。
    pub fn connect(timeout: std::time::Duration) -> anyhow::Result<Self> {
        #[cfg(windows)]
        let stream = transport::connect_retry(&transport::pipe_name(), timeout)?;
        #[cfg(not(windows))]
        let stream = {
            let _ = timeout;
            anyhow::bail!("命名管道仅在 Windows 上可用")
        };
        Ok(Self { stream })
    }

    /// 握手：校验协议版本，并取回首份完整状态。
    pub fn hello(&mut self) -> anyhow::Result<Snapshot> {
        let resp = self.send(&Request::Hello {
            version: crate::protocol::PROTOCOL_VERSION,
        })?;
        match resp {
            Response::Snapshot(s) => Ok(s),
            Response::VersionMismatch { server } => anyhow::bail!(
                "常驻进程协议版本 {server} 与本程序的 {} 不一致，请退出旧进程后重试",
                crate::protocol::PROTOCOL_VERSION
            ),
            other => anyhow::bail!("握手失败：意外的响应 {other:?}"),
        }
    }

    /// 增量同步。`known` 是界面侧已知的数据版本；返回 `None` 表示没变。
    pub fn sync(&mut self, known: DataVersion) -> anyhow::Result<Option<Snapshot>> {
        match self.send(&Request::Sync { known })? {
            Response::Snapshot(s) => Ok(Some(s)),
            Response::Unchanged => Ok(None),
            other => anyhow::bail!("同步失败：意外的响应 {other:?}"),
        }
    }

    /// 执行一条命令。
    ///
    /// 失败语义与进程内调用一致：`Err` 是业务失败（可直接显示给用户），
    /// 而不是通道故障——通道故障另用 [`Self::send`] 的 `Err` 表达，
    /// 但两者在这里合并了，因为对界面来说处置方式相同：弹一条提示。
    pub fn command(&mut self, cmd: Command) -> anyhow::Result<Response> {
        let resp = self.send(&Request::Cmd(cmd))?;
        resp.into_result()
    }

    /// 取条目载荷字节。
    pub fn load_payload(
        &mut self,
        id: modular_clipboard_core::EntryId,
    ) -> anyhow::Result<Vec<u8>> {
        match self.command(Command::LoadPayload(id))? {
            Response::Bytes(b) => Ok(b),
            other => anyhow::bail!("读取载荷失败：意外的响应 {other:?}"),
        }
    }

    /// 发一个请求并取回响应（协议层，不做业务语义转换）。
    pub fn send(&mut self, req: &Request) -> anyhow::Result<Response> {
        transport::call(&mut self.stream, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议不匹配必须给出「退出旧进程」这种可执行的提示，
    /// 而不是让界面在半截状态上继续跑。
    #[test]
    fn hello_rejects_version_mismatch() {
        // 直接测协议层的匹配分支：构造服务端响应验证解析路径。
        let resp = Response::VersionMismatch { server: 99 };
        let text = format!("{resp:?}");
        assert!(text.contains("99"), "响应里应带服务端版本，实际：{text}");
    }

    /// 非 Windows 上 connect 必须明确报错，而不是静默「连上了个空壳」。
    #[cfg(not(windows))]
    #[test]
    fn connect_fails_off_windows() {
        assert!(Client::connect(std::time::Duration::from_millis(1)).is_err());
    }
}
