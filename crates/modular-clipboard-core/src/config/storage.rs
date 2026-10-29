//! 存储与保留策略、剪贴板捕获参数。

use serde::Deserialize;
use serde::Serialize;

/// 存储与保留策略。
///
/// NOTE: 容器级 `#[serde(default)]`：语义同 `Hotkey`。没有它，
/// 新增一个容量字段就会让旧配置整体失效，用户的保留策略被默认值覆盖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// 历史条目总数上限。`None` 表示仅按容量淘汰。
    pub max_items: Option<usize>,
    /// 存储总容量上限（字节）。
    pub max_bytes: u64,
    /// 单条载荷上限（字节）。超过则只存元数据。
    pub max_payload_bytes: u64,
    /// 启动时是否自动清理上次退出遗留的临时文件。
    pub cleanup_on_start: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_items: Some(10_000),
            max_bytes: 512 * 1024 * 1024,
            max_payload_bytes: 32 * 1024 * 1024,
            cleanup_on_start: true,
        }
    }
}

/// 捕获行为。
///
/// IMPORTANT: 容器级 `#[serde(default)]`：语义同 `Hotkey`。这一条尤其关键——
/// `blocked_apps` 是用户的**安全边界**（密码管理器名单）。缺了它，
/// 一次无关的格式调整就会让名单退回代码默认值，
/// 用户的自定义屏蔽项被静默丢弃，且用户以为自己仍有屏蔽保护。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub enabled: bool,
    /// 监听间隔（毫秒）。Windows 下为序列号轮询周期，非阻塞。
    pub poll_interval_ms: u64,
    /// 记录来源进程名。
    pub track_source_app: bool,
    /// 对以下进程名开头的程序不捕获（不区分大小写）。
    pub blocked_apps: Vec<String>,
    /// 纯密码字段场景下不记录（基于前台窗口类名启发式）。
    pub skip_password_fields: bool,
    /// 连续相同内容不重复入库。
    pub dedup: bool,
    /// 去重窗口（秒）。
    ///
    /// NOTE: 已由全局去重取代：`ingest` 现在按哈希查**全部历史**，
    /// 任何时间点的相同内容都不再新增。此字段保留仅为兼容
    /// 已落盘的旧配置文件，不再参与去重逻辑。
    pub dedup_window_secs: i64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 300,
            track_source_app: true,
            blocked_apps: vec![
                "1password".into(),
                "bitwarden".into(),
                "keepass".into(),
                "keePassXC".into(),
            ],
            skip_password_fields: true,
            dedup: true,
            dedup_window_secs: 3,
        }
    }
}
