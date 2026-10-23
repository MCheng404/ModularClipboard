//! 数据目录与配置路径推导、分页与事件队列容量。
//!
//! 路径推导历史上三处并存导致过「读 A 写 B」，收敛到此处。

/// 单页最多加载的条目数。
pub(super) const PAGE_LIMIT: usize = 2000;

pub(super) const DEFAULT_GROUP_COLOR: &str = "#5B8DEF";

/// 后台事件队列容量。溢出时丢弃新事件而非阻塞剪贴板读取。
pub(super) const EVENT_QUEUE_CAPACITY: usize = 128;

/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// 未指定 `--data-dir` 时的配置路径（`%APPDATA%/modular-clipboard/config.json`）。
pub(super) fn default_config_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "modular-clipboard")
        .map(|d| d.config_dir().join(CONFIG_FILE_NAME))
}

/// 数据目录内的配置文件路径。
///
/// 与 [`default_config_path`] 分开而不是「有data_dir 就 join、否则 ProjectDirs」：
/// `--data-dir` 指定的是**数据**目录，配置跟着走才能保证
/// 「读配置」与「写配置」在同一处（`save_config` 的读回逻辑依赖这一点）。
pub fn config_path_in(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(CONFIG_FILE_NAME)
}
