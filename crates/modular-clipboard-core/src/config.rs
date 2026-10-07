//! 运行时配置与用户偏好。

use serde::{Deserialize, Serialize};

/// 单个快捷键绑定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

impl Hotkey {
    /// 序列化为 `Ctrl+Shift+V` 形式，供 UI 显示。
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        parts.push(self.key.to_uppercase());
        parts.join("+")
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            key: "V".into(),
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
        }
    }
}

/// 存储与保留策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 自动去重窗口：多少秒内的相同哈希视为重复。
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

/// 布局状态的磁盘表示（纯数字 DTO）。
///
/// # 为什么在 core 而不在 ui
///
/// 它是**用户配置的一部分**，必须与 [`UiConfig`] 的其余字段同处一个
/// crate：`Config` 被 core 定义，ui 反过来依赖 core。若把 `LayoutConfig`
/// 留在 ui crate，`UiConfig` 就没法持有它——要么 core 依赖 ui（形成
/// 循环依赖），要么配置里存不成布局。
///
/// 与运行时的 [`LayoutState`](https://docs.rs) 分离的原因：布局用的
/// `Rect` 是 egui 类型，直接序列化会把渲染库的内部结构写进配置文件，
/// 换 egui 版本就得迁移用户数据。这层 DTO 只用原始数字，是**稳定格式**。
///
/// `From<&LayoutState>` 的转换实现留在 ui crate——`LayoutState` 是
/// egui 侧的类型，core 不认识它。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// 每个模块：`[是否浮动, 槽位, 是否折叠, x, y, w, h]`。
    ///
    /// 位置与尺寸**只在浮动时有效**，停靠时仍写入——
    /// 停靠→浮动→停靠往返后能恢复原位置。
    pub modules: [[f32; 7]; 4],
    /// 宽度增量。
    pub width_delta: [f32; 4],
    /// 侧栏选中的视图下标。
    pub rail_view: usize,
    /// 顶栏高度。
    pub topbar_height: f32,
    /// 结构版本号，供将来迁移。
    pub version: u32,
}

impl LayoutConfig {
    /// 当前格式版本。
    pub const VERSION: u32 = 1;

    /// 全部为 0 的空布局。**不是**可用的默认布局。
    ///
    /// 真正的默认布局必须由 ui 侧的 `LayoutState::default()` 转换而来
    ///（它要知道每个模块的默认槽位），因此没有 `Default` 实现——
    /// 在这里给一个「看起来像默认」的全零值，调用方一旦误用就会得到
    /// 「所有模块堆在中央槽」的用户可见故障，且没有任何编译期提示。
    pub fn empty() -> Self {
        Self {
            modules: [[0.0; 7]; 4],
            width_delta: [0.0; 4],
            rail_view: 0,
            topbar_height: 0.0,
            version: Self::VERSION,
        }
    }
}

/// UI 偏好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiConfig {
    /// 跟随系统（None）/强制深色（Some(true)）/强制浅色（Some(false)）。
    pub dark_mode: Option<bool>,
    pub always_on_top: bool,
    pub hide_on_focus_lost: bool,
    pub window_width: f32,
    pub window_height: f32,
    pub show_tray: bool,
    pub start_minimized: bool,
    /// 自定义中文字体路径，`None` 时自动探测系统字体。
    pub font_path: Option<String>,
    pub font_scale: f32,
    /// 面板布局（停靠 / 折叠 / 浮动位置）。`None` 表示用布局默认值。
    ///
    /// ⚠️ `#[serde(default)]` 不是可选的：没有它，serde 会要求这个
    /// 字段**必须存在**，于是任何旧版config.json 解析失败，
    /// [`Config::load`] 静默回退到 `Config::default()`——
    /// 用户的窗口尺寸、主题、屏蔽名单**全部被抹掉**，且没有任何提示。
    #[serde(default)]
    pub layout: Option<LayoutConfig>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            dark_mode: None,
            always_on_top: true,
            hide_on_focus_lost: false,
            window_width: 420.0,
            window_height: 560.0,
            show_tray: true,
            start_minimized: false,
            font_path: None,
            font_scale: 1.0,
            layout: None,
        }
    }
}

/// 动作规则：当条目匹配条件时，可一键执行的动作。
///
/// 这是「软件联动」的扩展点。新增动作只需实现 [`Action`] trait 并注册，
/// 不需要改动捕获、存储或界面的任何代码。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRule {
    pub id: i64,
    pub name: String,
    /// 触发关键词，`None` 表示匹配所有条目。
    pub keyword: Option<String>,
    /// 匹配到的条目类型，`None` 表示不限。
    pub kind: Option<crate::item::ClipKind>,
    /// 动作标识，交由 [`ActionRegistry`] 解析为具体实现。
    pub action: String,
    /// 动作参数，例如命令行模板 `{selection}`。
    pub arg: Option<String>,
    pub sort_order: i64,
}

/// 全量配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub storage: StorageConfig,
    pub capture: CaptureConfig,
    pub ui: UiConfig,
    pub hotkey_toggle: Hotkey,
    pub hotkey_paste_previous: Hotkey,
    pub rules: Vec<ActionRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage: StorageConfig::default(),
            capture: CaptureConfig::default(),
            ui: UiConfig::default(),
            hotkey_toggle: Hotkey::default(),
            hotkey_paste_previous: Hotkey {
                key: "V".into(),
                ctrl: true,
                shift: false,
                alt: false,
                win: true,
            },
            rules: Vec::new(),
        }
    }
}

impl Config {
    /// 载入配置；文件不存在或损坏时回退到默认值而非报错退出。
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        match serde_json::from_str(&raw) {
            Ok(cfg) => Ok(cfg),
            Err(err) => {
                tracing::warn!(%err, "配置解析失败，回退默认值: {}", path.display());
                Ok(Self::default())
            }
        }
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// 判断进程是否在屏蔽名单中。大小写不敏感。
    pub fn is_app_blocked(&self, app: &str) -> bool {
        let lower = app.to_ascii_lowercase();
        self.capture
            .blocked_apps
            .iter()
            .any(|blocked| lower.starts_with(&blocked.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_display_is_ordered() {
        let h = Hotkey {
            key: "v".into(),
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
        };
        assert_eq!(h.display(), "Ctrl+Shift+V");
    }

    #[test]
    fn app_blocking_is_case_insensitive_prefix() {
        let cfg = Config::default();
        assert!(cfg.is_app_blocked("1Password.exe"));
        assert!(cfg.is_app_blocked("KeePassXC"));
        assert!(!cfg.is_app_blocked("notepad.exe"));
    }

    #[test]
    fn missing_file_yields_default() {
        let cfg = Config::load(std::path::Path::new("nonexistent.json")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn corrupt_file_falls_back_instead_of_panicking() {
        let dir = std::env::temp_dir().join("tiez-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg, Config::default());
        let _ = std::fs::remove_file(&path);
    }

    /// 旧版 config.json（**没有** `ui.layout` 字段）必须仍能完整读回。
    ///
    /// 这条守着 `#[serde(default)]`：字段漏了它，serde 会报
    /// 「missing field」，`Config::load` 于是回退到 `Config::default()`——
    /// 用户的窗口尺寸、主题、屏蔽名单被**静默抹掉**，且不报任何错。
    /// 上线新字段时把它删掉，这条测试立刻失败。
    #[test]
    fn legacy_config_without_layout_field_still_loads() {
        let legacy = r#"{
          "storage": {"max_items": 500, "max_bytes": 1024, "max_payload_bytes": 2048,
                      "cleanup_on_start": true},
          "capture": {"enabled": false, "poll_interval_ms": 500, "track_source_app": false,
                      "blocked_apps": ["钉钉"], "skip_password_fields": false,
                      "dedup": false, "dedup_window_secs": 9},
          "ui": {"dark_mode": false, "always_on_top": false, "hide_on_focus_lost": true,
                 "window_width": 777.0, "window_height": 888.0, "show_tray": false,
                 "start_minimized": true, "font_path": null, "font_scale": 1.5},
          "hotkey_toggle": {"key": "V", "ctrl": true, "shift": true, "alt": false, "win": false},
          "hotkey_paste_previous": {"key": "V", "ctrl": true, "shift": false, "alt": false, "win": true},
          "rules": []
        }"#;
        let cfg: Config = serde_json::from_str(legacy).expect("旧配置不该解析失败");
        assert_eq!(cfg.ui.layout, None, "缺失字段应落回 None（用布局默认值）");
        // 其余字段必须**逐个**保留，而不是被默认值覆盖。
        assert_eq!(cfg.ui.window_width, 777.0);
        assert_eq!(cfg.ui.window_height, 888.0);
        assert_eq!(cfg.ui.dark_mode, Some(false));
        assert!(!cfg.ui.always_on_top);
        assert!(!cfg.ui.show_tray);
        assert!(!cfg.capture.enabled, "capture 段也必须原样读回");
        assert_eq!(cfg.capture.poll_interval_ms, 500);
        assert_eq!(cfg.capture.blocked_apps, vec!["钉钉".to_string()]);
        assert_eq!(cfg.ui.font_scale, 1.5);
    }

    /// 布局字段存→取必须逐字段不变。
    ///
    /// 直接打 serde 与 [`Config::save`] / [`Config::load`] 两条路径，
    /// 因为两者都可能因未来重构而漂移。
    #[test]
    fn layout_config_survives_json_roundtrip() {
        let mut lay = LayoutConfig::empty();
        lay.modules[0] = [1.0, 0.0, 1.0, 10.0, 20.0, 300.0, 400.0];
        lay.width_delta = [1.5, -2.5, 0.0, 7.25];
        lay.rail_view = 2;
        lay.topbar_height = 44.0;

        // 路径1：裸 serde
        let text = serde_json::to_string(&lay).unwrap();
        let back: LayoutConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(lay, back);

        // 路径2：整个 Config 存盘再读回
        let dir = std::env::temp_dir().join("tiez-cfg-layout-rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.json");
        let mut cfg = Config::default();
        cfg.ui.layout = Some(lay.clone());
        cfg.save(&path).unwrap();
        let read = Config::load(&path).unwrap();
        assert_eq!(read.ui.layout, Some(lay));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `UiConfig::default()` 的布局必须是 `None`（= 用布局默认值）。
    ///
    /// 若默认给了某个具体布局，用户首次启动就会看到「模块已被安排好」的
    /// 状态，且那份默认值无处可查。
    #[test]
    fn default_ui_config_has_no_layout_override() {
        assert_eq!(UiConfig::default().layout, None);
    }

    /// 版本号必须稳定：它是将来做格式迁移的唯一依据。
    ///
    /// 一旦发布过就不能改——改了会让已存盘的旧版本号无法识别。
    #[test]
    fn layout_version_is_pinned() {
        assert_eq!(LayoutConfig::VERSION, 1);
        assert_eq!(LayoutConfig::empty().version, 1);
    }
}