//! 进程间协议：界面进程 ⇄ 常驻进程。
//!
//! # 为什么要它
//!
//! 渲染一个窗口的**固定成本**是 Vulkan 设备 + 交换链 + 字体图集，
//! 实测 80~100MB，且销毁后被驱动池吞掉、不会还给系统
//! （见 `docs/内存分析-2026-10-12.md`）。只要窗口与后台常驻同进程，
//! 「关掉窗口」就永远换不回内存。
//!
//! 于是把界面拆成独立进程：关闭即退出，内存由操作系统整体回收。
//! 两个进程之间只传**状态与命令**，不传渲染资源。
//!
//! # 传输形状
//!
//! 请求-响应式（界面问、常驻答），帧 = `u32` 长度前缀 + JSON。
//!
//! 用 JSON 而不是二进制是因为两侧都是同一份代码、同一版协议，
//! 序列化成本发生在「列表真的变了」的那一帧（几十毫秒级一次），
//! 而握手/同步的常态是一次几十字节的往返。
//!
//! # 版本与兼容
//!
//! [`PROTOCOL_VERSION`] 参与握手：不匹配时服务端直接拒绝，
//! 避免「升级了 exe 但旧 daemon 还在」时两侧按不同字段解析出乱码。

use modular_clipboard_core::{CapturedPayload, ClipItem, Config, DropStats, EntryId, Group};
use serde::{Deserialize, Serialize};

/// 协议版本。结构不兼容变更时必须 +1。
pub const PROTOCOL_VERSION: u32 = 1;

/// 数据版本号：常驻进程每次改动历史（增/删/置顶/清空）后递增。
///
/// 界面每帧都要问「有新内容吗」，若每次都回传整个列表，
/// 光 JSON 解析就能吃掉一帧预算。带上它，常态回答是
/// [`Response::Unchanged`]（几十字节）。
pub type DataVersion = u64;

/// 一次同步要传的全部状态。
///
/// 刻意**不含** `notice`/`selected`/`query`：那是界面自己的瞬时状态，
/// 由界面进程独占。混进来的话，常驻侧的一次同步就会把用户刚选中的行、
/// 刚输入的搜索词冲掉。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: DataVersion,
    pub items: Vec<ClipItem>,
    pub groups: Vec<Group>,
    pub stats: DropStats,
    /// 剪贴板监听是否在运行。
    pub capturing: bool,
    /// 持久配置。只在握手与显式拉取时下发，避免覆盖界面未保存的改动。
    pub config: Config,
    /// 列表结构版本（绘制层行高缓存的失效判据，见 `AppState::list_version`）。
    pub list_version: u64,
}

/// 界面进程发往常驻进程的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
// `large_enum_variant`：这些类型是**跨进程消息**，一次构造一次消费，
// 不驻留在集合里。装箱大变体只会多一次堆分配，换不来任何收益；
// 而把 `Config`/`Snapshot` 拆成 `Box` 会让序列化路径多一层间接。
#[allow(clippy::large_enum_variant)]
pub enum Request {
    /// 握手：校验协议版本并取一份完整快照。
    Hello { version: u32 },
    /// 增量同步。`known` 为界面侧已知的数据版本。
    Sync { known: DataVersion },
    /// 业务命令。
    Cmd(Command),
}

/// 业务命令。与 [`modular_clipboard_app::Service`] 的方法一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
// `large_enum_variant`：这些类型是**跨进程消息**，一次构造一次消费，
// 不驻留在集合里。装箱大变体只会多一次堆分配，换不来任何收益；
// 而把 `Config`/`Snapshot` 拆成 `Box` 会让序列化路径多一层间接。
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// 重新读取列表（搜索/过滤后）。
    ReloadList,
    /// 设置搜索词并重载。
    Search(String),
    /// 分组过滤。
    FilterGroup(Option<modular_clipboard_store::GroupFilter>),
    /// 把条目写回系统剪贴板。
    CopyItem(EntryId),
    /// 删除条目。
    DeleteItem(EntryId),
    /// 切换置顶。
    TogglePin(EntryId),
    /// 清空历史。
    ClearAll,
    /// 把界面侧的持久配置落盘。
    SaveConfig(Config),
    /// 启动/停止剪贴板监听。
    SetCaptureEnabled(bool),
    /// 把一段文本写回剪贴板（自带自写回抑制）。
    WriteClipboardText(String),
    /// 武装自写回抑制（不写剪贴板）。
    ///
    /// 界面进程自己不写剪贴板时也可能需要它：例如「复制后立刻清空」这类
    /// 组合操作，抑制窗口必须由握着捕获线程的那一边计时。
    SuppressSelfCapture,
    /// 直接投递一条捕获内容（测试与同步路径）。
    Ingest(CapturedPayload),
    /// 读取条目载荷字节（图片预览等）。
    LoadPayload(EntryId),
    /// 请求唤起界面进程（用户重复双击图标 / 托盘点击）。
    ShowWindow,
    /// 退出常驻进程（托盘「退出程序」、设置里的退出按钮）。
    Quit,
}

/// 常驻进程回给界面进程的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
// `large_enum_variant`：这些类型是**跨进程消息**，一次构造一次消费，
// 不驻留在集合里。装箱大变体只会多一次堆分配，换不来任何收益；
// 而把 `Config`/`Snapshot` 拆成 `Box` 会让序列化路径多一层间接。
#[allow(clippy::large_enum_variant)]
pub enum Response {
    /// 握手成功。
    Snapshot(Snapshot),
    /// 数据版本未变，无需更新界面状态。
    Unchanged,
    /// 命令成功，无返回数据。
    Ok,
    /// 命令成功，附带条目载荷字节。
    Bytes(Vec<u8>),
    /// 命令失败。字符串是可直接展示给用户的中文说明。
    Error(String),
    /// 协议版本不匹配。
    VersionMismatch { server: u32 },
}

impl Response {
    /// 转成 `Result`，把 [`Self::Error`] 变成 `Err(anyhow!)`。
    pub fn into_result(self) -> anyhow::Result<Self> {
        match self {
            Response::Error(msg) => Err(anyhow::anyhow!(msg)),
            other => Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议是跨进程的契约：字段改名/重排都会让新旧 exe 互相解析失败。
    /// 这里固定住 JSON 的**形状**，改动必须是有意为之。
    #[test]
    fn request_json_shape_is_stable() {
        let json = serde_json::to_string(&Request::Sync { known: 7 }).unwrap();
        assert_eq!(json, r#"{"Sync":{"known":7}}"#);
    }

    #[test]
    fn command_variants_round_trip() {
        for cmd in [
            Command::ReloadList,
            Command::Search("abc".into()),
            Command::FilterGroup(Some(modular_clipboard_store::GroupFilter::Ungrouped)),
            Command::CopyItem(1),
            Command::DeleteItem(2),
            Command::TogglePin(3),
            Command::ClearAll,
            Command::SaveConfig(Config::default()),
            Command::SetCaptureEnabled(true),
            Command::WriteClipboardText("x".into()),
            Command::LoadPayload(9),
            Command::ShowWindow,
            Command::Quit,
        ] {
            let bytes = serde_json::to_vec(&cmd).unwrap();
            let back: Command = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(format!("{back:?}"), format!("{cmd:?}"));
        }
    }

    /// 版本不匹配必须被明确拒绝，而不是解析出半截结构。
    #[test]
    fn version_mismatch_is_a_distinct_response() {
        let r = Response::VersionMismatch { server: PROTOCOL_VERSION };
        assert!(matches!(r, Response::VersionMismatch { .. }));
        assert!(!matches!(r, Response::Error(_)));
    }

    /// 快照不携带界面瞬时状态——字段列表变了说明有人把 `query`
    /// 之类的东西塞了进来（那会让同步冲掉用户输入）。
    #[test]
    fn snapshot_has_no_ui_local_fields() {
        let snap = Snapshot {
            version: 1,
            items: Vec::new(),
            groups: Vec::new(),
            stats: DropStats::default(),
            capturing: false,
            config: Config::default(),
            list_version: 0,
        };
        let value = serde_json::to_value(&snap).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        for forbidden in ["query", "selected", "notice"] {
            assert!(
                !keys.contains(&forbidden),
                "快照不应含界面本地字段 {forbidden}，实际字段：{keys:?}"
            );
        }
    }

    /// `into_result` 只把 Error 变 Err，其余原样透传。
    #[test]
    fn into_result_only_maps_error() {
        assert!(Response::Error("x".into()).into_result().is_err());
        assert!(matches!(Response::Ok.into_result().unwrap(), Response::Ok));
        assert!(matches!(
            Response::Unchanged.into_result().unwrap(),
            Response::Unchanged
        ));
    }
}
