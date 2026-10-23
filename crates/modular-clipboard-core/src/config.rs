//! 运行时配置与用户偏好。

use serde::{Deserialize, Serialize};

pub mod hotkey;
pub mod storage;
pub mod layout;
pub mod ui;
pub mod panels;
pub mod card_row;
pub mod window;
pub mod secondary;
pub mod theme_params;
pub mod accent;
pub mod mcp;

pub use hotkey::Hotkey;
pub use storage::CaptureConfig;
pub use storage::StorageConfig;
pub use layout::LayoutConfig;
pub use ui::UiConfig;
pub use panels::SEARCH_PANEL_H_MAX;
pub use panels::SEARCH_PANEL_H_MIN;
pub use panels::SearchPanelConfig;
pub use panels::SearchPanelPos;
pub use panels::TYPE_RAIL_SIZE_MAX;
pub use panels::TYPE_RAIL_SIZE_MIN;
pub use panels::TypeRailConfig;
pub use panels::TypeRailPos;
pub use panels::clamp_search_panel_height;
pub use panels::clamp_type_rail_size;
pub use card_row::CardShadowConfig;
pub use card_row::RowMetaConfig;
pub use card_row::RowMetaPart;
pub use window::WindowBackdrop;
pub use window::WindowPos;
pub use secondary::SecondaryTheme;
pub use theme_params::ThemeKnob;
pub use theme_params::ThemeParams;
pub use theme_params::clamp_theme_param;
pub use accent::AccentColor;
pub use accent::ThemeOverrides;
pub use mcp::ActionRule;
pub use mcp::McpConfig;
pub use mcp::PastePreviousMode;


#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub storage: StorageConfig,
    pub capture: CaptureConfig,
    pub ui: UiConfig,
    pub hotkey_toggle: Hotkey,
    pub hotkey_paste_previous: Hotkey,
    /// 「粘贴上一条」的行为模式（见 [`PastePreviousMode`]）。
    pub paste_previous_mode: PastePreviousMode,
    /// 「迷你快捷复制面板」：修饰键 + 鼠标右键唤出最近几条。
    pub mini_panel: MiniPanelConfig,
    pub rules: Vec<ActionRule>,
    pub mcp: McpConfig,
}

/// 迷你快捷复制面板（修饰键 + 右键在鼠标处弹出）。
///
/// # 为什么默认关闭
///
/// 它挂的是**全局鼠标钩子**，会在每一次右键点击时被回调一次。
/// 这个代价对「随手 Ctrl+右键 复制」的用户是值得的，
/// 但对不需要它的人就是白付——而且右键是高频操作，
/// 一旦钩子写错，卡的是整个系统的鼠标。所以默认关闭、显式开启。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MiniPanelConfig {
    /// 是否启用。默认关闭，见类型文档。
    pub enabled: bool,
    /// 面板里显示最近几条。默认 3。
    ///
    /// 上限 10：面板是「随手一点」的东西，条目多了就变成第二个主窗口，
    /// 失去快捷的意义。
    pub count: usize,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// 预览文本最大字符数（控制面板宽度）。默认 48。
    ///
    /// 系统菜单不支持直接设像素宽度，字符数是唯一的宽度杠杆
    /// （2026-10-22 用户要求「限制宽度」）。范围 10..=100。
    pub preview_chars: usize,
}

impl MiniPanelConfig {
    /// 面板里实际显示的条数（已夹到 1..=10）。
    pub fn effective_count(&self) -> usize {
        self.count.clamp(1, 10)
    }

    /// 预览文本最大字符数（已夹到 10..=100）。
    pub fn effective_preview_chars(&self) -> usize {
        self.preview_chars.clamp(10, 100)
    }

    /// 修饰键的可读文本，如 `Ctrl+右键`。
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.win {
            parts.push("Win");
        }
        parts.push("右键");
        parts.join("+")
    }
}

impl Default for MiniPanelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            count: 3,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
            preview_chars: 48,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storage: StorageConfig::default(),
            capture: CaptureConfig::default(),
            ui: UiConfig::default(),
            hotkey_toggle: Hotkey::default(),
            // 粘贴上一条：默认 **Ctrl+Alt+V**。
            //
            // 不选 `Ctrl+Win+V`——2026-10-13 在本机实测，
            // `Ctrl+Win+V` 与 `Win+Shift+V` 都被系统/外部软件占着
            // （RegisterHotKey 回 ERROR_HOTKEY_ALREADY_REGISTERED），
            // 照它出厂必然有一个槽位是死的。`Ctrl+Alt+V` 实测可用，
            // 且与 `Win+V`（toggle）不共用任何修饰键，误触率低。
            hotkey_paste_previous: Hotkey {
                key: "V".into(),
                ctrl: true,
                shift: false,
                alt: true,
                win: false,
            },
            paste_previous_mode: PastePreviousMode::default(),
            mini_panel: MiniPanelConfig::default(),
            rules: Vec::new(),
            mcp: McpConfig::default(),
        }
    }
}

impl Config {
    /// 载入配置；文件不存在或损坏时回退到默认值而非报错退出。
    ///
    /// # 损坏文件不会被丢弃
    ///
    /// 解析失败时把原文**另存为 `.corrupt`**再回退默认值。
    /// 早前这里只`warn!` 然后丢掉全部内容：用户会遇到「配置被重置」
    /// 却既不知道丢了什么、也找不回原文，而里面可能有花时间调好的
    /// 屏蔽名单与快捷键。留一份原文，成本是一次文件复制。
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        match serde_json::from_str(&raw) {
            Ok(cfg) => {
                let mut cfg: Self = cfg;
                cfg.migrate();
                Ok(cfg)
            }
            Err(err) => {
                tracing::warn!(%err, "配置解析失败，回退默认值: {}", path.display());
                Self::backup_corrupt(path, &raw);
                Ok(Self::default())
            }
        }
    }

    /// 升级旧版本留下的、已证实不可用的配置值。
    ///
    /// # `Ctrl+Win+V` → `Ctrl+Alt+V`（paste 槽位）
    ///
    /// 2026-10-13 之前「粘贴上一条」的默认值是 `Ctrl+Win+V`，实测它在本机
    /// （也很可能在多数 Windows 上）被系统/外部软件占用——
    /// `RegisterHotKey` 回 `ERROR_HOTKEY_ALREADY_REGISTERED`，照它出厂的
    /// 用户这个快捷键是死的，按了只会「弹个软件窗口出来」（实际触发了
    /// 别的绑定）。新默认改为实测可用的 `Ctrl+Alt+V`。
    ///
    /// **只迁移恰好等于旧默认的值**：用户自己改过的绑定不动（万一他的
    /// 机器上 Ctrl+Win+V 没被占用呢）。只做一次，不写盘也行——下一次
    /// 保存设置时新值自然落盘。
    fn migrate(&mut self) {
        let legacy = Hotkey {
            key: "V".into(),
            ctrl: true,
            shift: false,
            alt: false,
            win: true,
        };
        if self.hotkey_paste_previous == legacy {
            tracing::info!(
                "paste 槽位默认值 Ctrl+Win+V 实测被系统/外部软件占用，已迁移到 Ctrl+Alt+V"
            );
            self.hotkey_paste_previous = Hotkey {
                key: "V".into(),
                ctrl: true,
                shift: false,
                alt: true,
                win: false,
            };
        }
    }

    /// 把无法解析的配置原文另存一份，避免用户的设置被静默销毁。
    ///
    /// 失败只记日志：这是「尽力而为」的抢救，不能反过来让启动失败。
    fn backup_corrupt(path: &std::path::Path, raw: &str) {
        let mut backup = path.as_os_str().to_os_string();
        backup.push(".corrupt");
        match std::fs::write(&backup, raw) {
            Ok(()) => tracing::warn!(
                "配置原文已另存为 {}，可手动检查后恢复",
                std::path::Path::new(&backup).display()
            ),
            Err(e) => tracing::warn!(%e, "配置损坏且备份失败，原文已丢失"),
        }
    }

    /// 原子落盘：先写临时文件，再替换目标。
    ///
    /// # 为什么不能直接 `fs::write`
    ///
    /// `fs::write` 等价于「截断 → 写入」。若在两步之间进程崩溃或断电，
    /// 留下的是一个**被截断的** config.json。下次启动解析失败 →
    /// 回退默认值（见 [`Config::load`]）→ 用户配置全丢，且因为文件
    /// 已被截断，**连原文都找不回来**。
    ///
    /// 先写 `<目标>.tmp` 再 `fs::rename`：Windows 的 `MoveFileEx`
    /// 在同卷内是原子的，读者要么看到完整旧文件、要么看到完整新文件，
    /// 不存在「半个文件」的中间态。
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;

        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);

        std::fs::write(&tmp, json)?;
        // rename 失败时不能留下 .tmp 干扰下次启动的读取。
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
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
    use super::{
        hotkey::*,
        layout::*,
        ui::*,
        panels::*,
        card_row::*,
        secondary::*,
        theme_params::*,
        accent::*,
        mcp::*,
    };
    use super::*;

    /// 旧默认 `Ctrl+Win+V` 必须被迁移到实测可用的 `Ctrl+Alt+V`。
    ///
    /// 照旧默认出厂的用户「粘贴上一条」是死的（组合被系统占用），
    /// 见 [`Config::migrate`] 的文档。
    #[test]
    fn migrate_replaces_legacy_paste_default() {
        // 手工把 paste 打回旧默认，模拟老 config.json。
        let mut cfg = Config {
            hotkey_paste_previous: Hotkey {
                key: "V".into(),
                ctrl: true,
                shift: false,
                alt: false,
                win: true,
            },
            ..Config::default()
        };
        cfg.migrate();
        assert_eq!(cfg.hotkey_paste_previous.display(), "Ctrl+Alt+V");
    }

    /// 用户自己改过的绑定**不**被迁移——万一他的机器上旧值没被占用。
    #[test]
    fn migrate_leaves_user_chosen_bindings_alone() {
        let mut cfg = Config {
            hotkey_paste_previous: Hotkey {
                key: "C".into(),
                ctrl: true,
                shift: true,
                alt: false,
                win: false,
            },
            ..Config::default()
        };
        cfg.migrate();
        assert_eq!(
            cfg.hotkey_paste_previous.display(),
            "Ctrl+Shift+C",
            "非旧默认值不该被动"
        );
    }

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

    // ---- 主题：强调色 / 二级主题 / 参数 / 覆盖 ----

    /// HSV→RGB 再回 HSV 必须闭环（预设色板与十六进制输入都走这条路）。
    #[test]
    fn accent_rgb_round_trips_through_hsv() {
        for c in [
            AccentColor::default_blue(),
            AccentColor::new(0.0, 1.0, 1.0),   // 红
            AccentColor::new(120.0, 0.5, 0.5), // 灰绿
            AccentColor::new(300.0, 0.9, 0.3), // 暗紫
        ] {
            let back = AccentColor::from_rgb(c.to_rgb());
            // 8bit 量化的往返容差：分量各自 ±1 档。
            assert!((back.h - c.h).abs() < 1.0, "{c:?} → {back:?}");
            assert!((back.s - c.s).abs() < 0.01, "{c:?} → {back:?}");
            assert!((back.v - c.v).abs() < 0.01, "{c:?} → {back:?}");
        }
    }

    /// 十六进制解析容忍缺 `#`，拒绝半截输入（打字中途不能清掉已选颜色）。
    #[test]
    fn accent_hex_parsing_tolerates_and_rejects() {
        assert_eq!(
            AccentColor::from_hex("#1D65AF"),
            Some(AccentColor::default_blue())
        );
        assert_eq!(AccentColor::from_hex("1d65af"), Some(AccentColor::default_blue()));
        assert_eq!(AccentColor::from_hex("#F"), None);
        assert_eq!(AccentColor::from_hex("#GGGGGG"), None);
        assert_eq!(AccentColor::from_hex(""), None);
    }

    /// 二级主题的字符串互转：serde 名与手写解析必须一致（`from_str_opt`
    /// 用于 MCP/脚本侧，绕过 serde）。
    #[test]
    fn secondary_theme_str_round_trips() {
        for t in [
            SecondaryTheme::None,
            SecondaryTheme::Paper,
            SecondaryTheme::Green,
            SecondaryTheme::Purple,
            SecondaryTheme::Oled,
        ] {
            assert_eq!(SecondaryTheme::from_str_opt(t.as_str()), t);
        }
        assert_eq!(SecondaryTheme::from_str_opt("不存在的主题"), SecondaryTheme::None);
    }

    /// 每个非 None 主题的旋钮键名必须能在 [`ThemeParams`] 上寻址——
    /// 键名打错字的话，设置界面滑块会静默失效（改的是另一个字段）。
    #[test]
    fn theme_knob_keys_address_params() {
        let params = ThemeParams::default();
        for t in [
            SecondaryTheme::Paper,
            SecondaryTheme::Green,
            SecondaryTheme::Purple,
            SecondaryTheme::Oled,
        ] {
            assert!(!t.knobs().is_empty(), "{t:?} 必须有旋钮");
            for k in t.knobs() {
                assert!(params.get(k.key).is_some(), "键名 {} 寻址不到参数", k.key);
            }
        }
        assert!(SecondaryTheme::None.knobs().is_empty());
    }

    /// 参数写入必须钳制（手改 config.json 是常态），且 get/set 闭环。
    #[test]
    fn theme_params_clamp_and_round_trip() {
        let mut p = ThemeParams::default();
        assert!(p.set("paper_texture", 2.5));
        assert_eq!(p.paper_texture, 1.0);
        assert!(!p.set("不存在的旋钮", 0.5), "未知键必须返回 false");
        assert!(p.get("oled_black").is_some());
        assert!(p.get("不存在的旋钮").is_none());
    }

    /// 旧版 config.json（无主题新字段）必须能解析——容器级 `serde(default)`
    /// 管不到**新字段**，字段级标注才行。少了它，升级用户的外观设置
    /// 会被整体回退抹掉。
    #[test]
    fn legacy_config_without_theme_fields_still_parses() {
        let json = r#"{"ui": {"dark_mode": true, "font_scale": 1.0}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.ui.accent, None);
        assert_eq!(cfg.ui.secondary_theme, SecondaryTheme::None);
        assert_eq!(cfg.ui.theme_params, ThemeParams::default());
        assert!(cfg.ui.theme_overrides.colors.is_empty());
        // 旧字段不被抹掉。
        assert_eq!(cfg.ui.dark_mode, Some(true));
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

    /// 上游实测的真Bug：任何字段缺 `default` 都会让整个配置归零。
    ///
    /// 这条比上面的 `legacy_config_without_layout_field` 更狠：它模拟
    /// 「**新增**字段」这一真实场景——旧config.json 里当然不会有
    /// 新字段。没有容器级 `#[serde(default)]` 时，`Config::load` 解析失败
    /// → 回退 `Config::default()` → 用户的窗口尺寸、屏蔽名单、快捷键
    /// **全部被抹掉**，且界面上零提示。
    ///
    /// 断言逐个字段核对「用户数据仍在」，而不只是「能解析」。
    #[test]
    fn unknown_or_missing_fields_never_reset_existing_values() {
        // 模拟未来版本新增 `capture.pause_on_fullscreen` 后的旧配置文件：
        // 字段缺失，但用户此前的每一项设置都必须活着。
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
        let cfg: Config = serde_json::from_str(legacy).expect("缺字段不该导致整体解析失败");

        // 缺失字段用各自Default 补齐…
        assert_eq!(cfg.ui.layout, None);
        assert_eq!(cfg.ui.window_pos, None);
        assert_eq!(cfg.ui.pinned_window_pos, None);
        // …但**已有**的设置一个都不能被默认值覆盖。
        assert_eq!(cfg.ui.window_width, 777.0, "窗口尺寸被重置了");
        assert_eq!(cfg.capture.blocked_apps, vec!["钉钉".to_string()], "屏蔽名单被重置了");
        assert_eq!(cfg.hotkey_toggle.display(), "Ctrl+Shift+V");
        // `display()` 的顺序固定为 Ctrl→Shift→Alt→Win→key，
        // 所以这组绑定渲染成 "Ctrl+Win+V" 而非 "Win+Ctrl+V"。
        assert_eq!(cfg.hotkey_paste_previous.display(), "Ctrl+Win+V");
        assert!(!cfg.capture.enabled);
    }

    /// `hotkey_toggle` 这类「整段缺失」也要能被补齐。
    ///
    /// 与上一条互补：那条测「段内缺字段」，这条测「整段不存在」。
    #[test]
    fn wholly_missing_sections_fall_back_per_section() {
        let partial = r#"{ "capture": { "blocked_apps": ["keepass"] } }"#;
        let cfg: Config = serde_json::from_str(partial).expect("整段缺失不该导致解析失败");

        //缺失段用Default…
        assert_eq!(cfg.hotkey_toggle, Hotkey::default());
        assert_eq!(cfg.ui, UiConfig::default());
        // …保留段里已存在的值逐个读回。
        assert_eq!(cfg.capture.blocked_apps, vec!["keepass".to_string()]);
        assert!(cfg.capture.enabled, "未写的 capture 字段应取默认 true");
    }

    /// 规则里可选字段缺失时按`None` 处理，必填字段仍需存在。
    ///
    /// `ActionRule` 用**字段级**而非容器级 default：一条规则缺 `action`
    /// 就没有可执行语义，不该被默认值造出一条假规则；缺 `keyword` 则
    /// 明确表示「匹配所有条目」，是合法含义。
    #[test]
    fn action_rule_tolerates_missing_optional_fields() {
        let rule: ActionRule =
            serde_json::from_str(r#"{"id": 7, "name": "打开", "action": "open_path"}"#)
                .expect("规则缺可选字段不该失败");
        assert_eq!(rule.id, 7);
        assert_eq!(rule.name, "打开");
        assert_eq!(rule.action, "open_path");
        assert_eq!(rule.keyword, None);
        assert_eq!(rule.kind, None);
        assert_eq!(rule.arg, None);
        assert_eq!(rule.sort_order, 0, "缺省应落0 而不是解析失败");
    }

    /// 损坏的配置必须留一份原文，用户才有找回设置的可能。
    ///
    /// 早前`load` 只 warn 然后丢弃——用户看到配置被重置，却既不知道
    /// 丢了什么，也找不回原文，而里面可能有调了很久的屏蔽名单。
    #[test]
    fn corrupt_config_leaves_original_for_recovery() {
        let dir = std::env::temp_dir().join("tiez-cfg-corrupt-backup");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let original = "{ \"ui\": { \"window_width\": 777.0 }, ";
        std::fs::write(&path, original).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg, Config::default(), "损坏时仍应回退默认值，不崩溃");

        let mut backup = path.as_os_str().to_os_string();
        backup.push(".corrupt");
        let backup = std::path::PathBuf::from(backup);
        assert!(backup.is_file(), "损坏原文应另存为 {}", backup.display());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            original,
            "备份内容必须与原文逐字节一致"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘后不留临时文件，且再次读取得到同一份配置。
    ///
    /// `save` 改成「写 .tmp 再 rename」后，若 rename 失败会把 `.tmp`
    /// 留下来。这条守住「正常路径不产生残留」。
    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join("tiez-cfg-atomic-save");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let mut cfg = Config::default();
        cfg.ui.window_width = 777.0;
        cfg.capture.blocked_apps = vec!["钉钉".into()];
        cfg.save(&path).unwrap();

        assert!(path.is_file(), "目标文件应存在");
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(
            !std::path::PathBuf::from(tmp).exists(),
            "rename 成功后不应残留 .tmp"
        );

        let back = Config::load(&path).unwrap();
        assert_eq!(back.ui.window_width, 777.0);
        assert_eq!(back.capture.blocked_apps, vec!["钉钉".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
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

    /// 默认磨砂强度档位必须是 7——它经 `gfx::blur::radius_for_strength`
    /// 映射回历史固定半径 8，老用户升级后磨砂观感保持一致（不突然变轻
    /// 或变糊）。
    #[test]
    fn default_blur_strength_is_seven_for_legacy_match() {
        assert_eq!(
            UiConfig::default().window_blur_strength,
            7,
            "缺省档位不是 7，会改变升级用户的磨砂观感"
        );
    }

    /// 卡片阴影配置必须能无损序列化往返，且旧版配置文件缺该字段时
    /// 回退到默认（开启）。
    #[test]
    fn card_shadow_survives_json_roundtrip_and_defaults() {
        // 自定义一组非默认值，走裸 serde 往返。
        let custom = CardShadowConfig {
            enabled: true,
            blur: 12.0,
            opacity: 0.3,
            offset_y: 6.0,
        };
        let text = serde_json::to_string(&custom).unwrap();
        let back: CardShadowConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(custom, back);

        // 字段缺失（旧配置）→ 容器级 serde(default) 补成默认值。
        let legacy = r#"{"always_on_top": true}"#;
        let cfg: UiConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(cfg.card_shadow, CardShadowConfig::default());
        assert!(cfg.card_shadow.enabled, "阴影默认应开启");
    }

    /// 默认值必须落在绘制侧承诺的取值区间内（防默认值本身越界）。
    #[test]
    fn card_shadow_defaults_are_within_range() {
        let d = CardShadowConfig::default();
        assert!((0.0..=24.0).contains(&d.blur));
        assert!((0.0..=0.6).contains(&d.opacity));
        assert!((-8.0..=16.0).contains(&d.offset_y));
    }

    /// 2026-10-15 预览页验收后用户定稿的默认参数：blur 8 / 浓度 20% /
    /// 偏移 0 / 阴影默认开启。锁死这批数值，防止后续改动悄悄改掉观感。
    #[test]
    fn card_shadow_defaults_match_2026_10_15_final() {
        let d = CardShadowConfig::default();
        assert!(d.enabled, "阴影默认开启");
        assert_eq!(d.blur, 8.0, "默认模糊半径 8.0");
        assert_eq!(d.opacity, 0.20, "默认浓度倍率 20%");
        assert_eq!(d.offset_y, 0.0, "默认纵向偏移 0");
    }

    /// 右下角信息组合默认三个元素全亮、顺序 图标 → 类型 → 大小。
    #[test]
    fn row_meta_defaults_all_visible_in_natural_order() {
        let d = RowMetaConfig::default();
        assert!(d.icon && d.type_ && d.size, "默认全部显示");
        assert_eq!(
            d.order,
            [RowMetaPart::Icon, RowMetaPart::Type, RowMetaPart::Size]
        );
        for p in RowMetaPart::ALL {
            assert!(d.visible(p));
        }
    }

    /// 2026-10-15 第三轮原型的默认值：图片直显 / Markdown 渲染 /
    /// 类型栏可见但仅显示已有类型 / 停靠右侧 / 32pt / 调用路径留空。
    #[test]
    fn proto_round3_defaults() {
        let d = UiConfig::default();
        assert!(d.image_inline, "图片直显默认开");
        assert!(d.md_rendered, "Markdown 渲染默认开");
        assert!(d.type_rail.visible, "类型栏默认显示");
        assert!(!d.type_rail.always, "默认仅显示已有类型");
        assert_eq!(d.type_rail.pos, TypeRailPos::Right);
        assert_eq!(d.type_rail.size, 32.0);
        assert!(d.browser_path.is_empty(), "浏览器默认走系统");
        assert!(d.explorer_path.is_empty(), "资源管理器默认走系统");
    }

    /// 旧版 config.json 缺第三轮字段时必须补默认值，不能让整个配置
    /// 解析失败（那会静默抹掉用户的全部设置，见 UiConfig 注释）。
    ///
    /// NOTE: 只测「键缺失」——显式 `null` 不在 `#[serde(default)]` 语义内，
    /// 与 `card_shadow` 等既有结构体字段行为一致（手写 null 会让整份配置
    /// 解析失败并回退默认，那是 serde 的既定语义，不为谁开特例）。
    #[test]
    fn proto_round3_fields_fall_back_for_legacy_config() {
        let legacy = r#"{"always_on_top": true, "window_width": 720.0}"#;
        let cfg: UiConfig = serde_json::from_str(legacy).unwrap();
        assert!(cfg.image_inline);
        assert!(cfg.md_rendered);
        assert!(cfg.type_rail.visible);
        assert!(!cfg.type_rail.always);
        assert_eq!(cfg.type_rail.pos, TypeRailPos::Right);
        assert_eq!(cfg.type_rail.size, 32.0);
        assert!(cfg.browser_path.is_empty());
        assert!(cfg.explorer_path.is_empty());
        // type_rail 整体键缺失也要补默认（结构体级 serde(default)）。
        let legacy2 = r#"{"image_inline": false}"#;
        let cfg2: UiConfig = serde_json::from_str(legacy2).unwrap();
        assert!(!cfg2.image_inline, "同键存在时应尊重用户值");
        assert_eq!(cfg2.type_rail, TypeRailConfig::default());
    }

    /// 自定义值无损往返（含枚举的小写编码）。
    #[test]
    fn proto_round3_survives_json_roundtrip() {
        let custom = UiConfig {
            image_inline: false,
            md_rendered: false,
            type_rail: TypeRailConfig {
                visible: false,
                always: true,
                multi: true,
                labels: true,
                pos: TypeRailPos::Top,
                size: 44.0,
            },
            browser_path: "C:\\app\\chrome.exe".into(),
            explorer_path: "D:\\Tools\\dopus.exe".into(),
            ..UiConfig::default()
        };
        let text = serde_json::to_string(&custom).unwrap();
        let back: UiConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(custom, back);
        assert!(text.contains("\"pos\":\"top\""));
    }

    /// 尺寸钳制：越界收边界、非有限值（NaN / ±inf）统一回落默认。
    #[test]
    fn type_rail_size_clamps() {
        assert_eq!(clamp_type_rail_size(32.0), 32.0);
        assert_eq!(clamp_type_rail_size(8.0), TYPE_RAIL_SIZE_MIN);
        assert_eq!(clamp_type_rail_size(999.0), TYPE_RAIL_SIZE_MAX);
        assert_eq!(clamp_type_rail_size(f32::NAN), 32.0);
        assert_eq!(clamp_type_rail_size(f32::INFINITY), 32.0);
        assert_eq!(clamp_type_rail_size(f32::NEG_INFINITY), 32.0);
    }

    /// 搜索面板高度钳制：同 type_rail 的纪律（手改配置是常态）。
    #[test]
    fn search_panel_height_clamps() {
        assert_eq!(clamp_search_panel_height(30.0), 30.0);
        assert_eq!(clamp_search_panel_height(10.0), SEARCH_PANEL_H_MIN);
        assert_eq!(clamp_search_panel_height(999.0), SEARCH_PANEL_H_MAX);
        assert_eq!(clamp_search_panel_height(f32::NAN), 30.0);
        assert_eq!(clamp_search_panel_height(f32::INFINITY), 30.0);
    }

    /// 本轮新增字段的缺省值：旧 config.json 升级后观感不能变。
    #[test]
    fn notice_popup_and_search_panel_defaults() {
        let d = UiConfig::default();
        assert!(!d.notice_popup, "通知默认走窗口内 toast（既有观感）");
        assert_eq!(d.search_panel, SearchPanelConfig::default());
        assert_eq!(d.search_panel.pos, SearchPanelPos::Top);
        assert_eq!(d.search_panel.height, 30.0, "与历史固定高度一致");
        assert!(!d.type_rail.multi, "类型栏默认单选");
        assert!(!d.type_rail.labels, "类型按钮默认仅图标");
        // 整体键缺失时补默认（结构体级 serde(default)）。
        let legacy: UiConfig = serde_json::from_str(r#"{"window_width":720.0}"#).unwrap();
        assert!(!legacy.notice_popup);
        assert_eq!(legacy.search_panel, SearchPanelConfig::default());
        let legacy2: UiConfig = serde_json::from_str(r#"{"search_panel":{}}"#).unwrap();
        assert_eq!(legacy2.search_panel, SearchPanelConfig::default());
    }

    /// 本轮新增字段无损往返（含枚举小写编码）。
    #[test]
    fn notice_and_search_panel_roundtrip() {
        let custom = UiConfig {
            notice_popup: true,
            search_panel: SearchPanelConfig {
                pos: SearchPanelPos::Bottom,
                height: 44.0,
            },
            type_rail: TypeRailConfig {
                multi: true,
                labels: true,
                ..TypeRailConfig::default()
            },
            ..UiConfig::default()
        };
        let text = serde_json::to_string(&custom).unwrap();
        let back: UiConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(custom, back);
        assert!(text.contains("\"pos\":\"bottom\""));
        assert!(text.contains("\"notice_popup\":true"));
    }

    /// 位置解析：合法值逐一命中，无法识别回落右侧（cosmetic 设置
    /// 不值得让配置加载失败）。
    #[test]
    fn type_rail_pos_from_str_falls_back_to_right() {
        assert_eq!(TypeRailPos::from_str_opt("left"), TypeRailPos::Left);
        assert_eq!(TypeRailPos::from_str_opt("TOP"), TypeRailPos::Top);
        assert_eq!(TypeRailPos::from_str_opt(" bottom "), TypeRailPos::Bottom);
        assert_eq!(TypeRailPos::from_str_opt("right"), TypeRailPos::Right);
        assert_eq!(TypeRailPos::from_str_opt(""), TypeRailPos::Right);
        assert_eq!(TypeRailPos::from_str_opt("diagonal"), TypeRailPos::Right);
        for p in [TypeRailPos::Right, TypeRailPos::Left, TypeRailPos::Top, TypeRailPos::Bottom] {
            assert_eq!(TypeRailPos::from_str_opt(p.as_str()), p);
            assert!(!p.label().is_empty());
        }
    }

    /// 单击切换显隐：toggle 必须翻转对应元素、不影响其他元素。
    #[test]
    fn row_meta_toggle_flips_only_target() {
        let mut d = RowMetaConfig::default();
        d.toggle(RowMetaPart::Type);
        assert!(!d.type_);
        assert!(d.icon && d.size, "其他元素不受影响");
        d.toggle(RowMetaPart::Type);
        assert!(d.type_, "再次点击恢复显示");
    }

    /// 拖动重排：move_to_index 把元素移到目标位置，其余相对顺序不变。
    #[test]
    fn row_meta_move_to_index_reorders_without_losing_elements() {
        use RowMetaPart::*;
        // 图标移到最右：idx=2。
        let mut d = RowMetaConfig::default();
        d.move_to_index(Icon, 2);
        assert_eq!(d.order, [Type, Size, Icon]);
        // 大小移到最左：idx=0。
        d.move_to_index(Size, 0);
        assert_eq!(d.order, [Size, Type, Icon]);
        // 越界 idx 钳制到末尾。
        d.move_to_index(Size, 99);
        assert_eq!(d.order, [Type, Icon, Size]);
        // 移到原位置（idx=1）：不变。
        d.move_to_index(Icon, 1);
        assert_eq!(d.order, [Type, Icon, Size]);
        // 三种元素始终各出现一次，没有丢失或重复。
        for p in RowMetaPart::ALL {
            assert_eq!(d.order.iter().filter(|x| **x == p).count(), 1);
        }
        // 幂等：同一位置反复调用结果相同（拖拽每帧重算不能抖动）。
        d.move_to_index(Type, 0);
        let once = d.order;
        d.move_to_index(Type, 0);
        assert_eq!(d.order, once);
    }

    /// 旧 config.json 缺 row_meta 字段时必须反序列化为默认组合，
    /// 不能让整份配置解析失败。
    #[test]
    fn row_meta_missing_in_config_falls_back_to_default() {
        let json = r#"{"window_width":720.0}"#;
        let ui: UiConfig = serde_json::from_str(json).expect("旧配置应能解析");
        assert_eq!(ui.row_meta, RowMetaConfig::default());
    }

    /// 版本号必须稳定：它是将来做格式迁移的唯一依据。
    ///
    /// 一旦发布过就不能改——改了会让已存盘的旧版本号无法识别。
    #[test]
    fn layout_version_is_pinned() {
        assert_eq!(LayoutConfig::VERSION, 1);
        assert_eq!(LayoutConfig::empty().version, 1);
    }

    /// 默认窗口尺寸必须容得下三栏布局。
    ///
    /// 这两个值是**物理像素**（`app.rs` 直接 `as u32` 传给
    /// `CreateWindowExW`，中间没有 DPI 换算）。旧的 420x560 在 1.5x
    /// 缩放下只剩 280x373 逻辑点，比三栏布局所需的 333 逻辑点还窄，
    /// 于是置顶栏压住历史列表、列表行溢出右边界。
    ///
    /// 720x800 在 1.5x 下是 480x533 逻辑点，留有余量。
    #[test]
    fn default_window_size_fits_three_column_layout() {
        let ui = UiConfig::default();
        assert_eq!(ui.window_width, 720.0);
        assert_eq!(ui.window_height, 800.0);
        // 三栏布局的逻辑点下限（实测值）。1.5x 是当前最常见的缩放档，
        // 低于它窗口就更宽裕，所以拿 1.5x 做最坏情况断言。
        const MIN_LOGICAL_WIDTH: f32 = 333.0;
        assert!(
            ui.window_width / 1.5 >= MIN_LOGICAL_WIDTH,
            "1.5x 缩放下窗口宽度必须 >= {MIN_LOGICAL_WIDTH} 逻辑点，当前 {} 物理像素",
            ui.window_width
        );
    }
}
    /// 默认必须是 `PasteAndType`——那是**既有行为**。
    ///
    /// 默认值变保守等于用户升级后按同样热键却不再粘贴，行为静默变化
    /// 比多一个选项糟糕得多。这条钉死默认。
    #[test]
    fn paste_previous_mode_defaults_to_existing_behavior() {
        assert_eq!(
            PastePreviousMode::default(),
            PastePreviousMode::PasteAndType,
            "默认必须是「直接粘贴并输入」（既有行为），否则升级后热键不粘贴了"
        );
    }

    /// 老配置文件没有 `paste_previous_mode` 字段时，反序列化必须**不报错**
    /// 且补默认值——`#[serde(default)]` 是整个配置文件的最后一道防线，
    /// 没有它任意新字段都会让用户的配置整份回退默认（窗口尺寸、主题、
    /// 快捷键一次性归零）。
    #[test]
    fn paste_previous_mode_missing_field_falls_back_to_default() {
        let json = r#"{"hotkey_paste_previous": {"key": "V", "ctrl": true, "shift": false, "alt": false, "win": true}}"#;
        let cfg: Config = serde_json::from_str(json).expect("缺字段应按 default 补齐");
        assert_eq!(
            cfg.paste_previous_mode,
            PastePreviousMode::PasteAndType,
            "缺字段应落到默认，而不是报错回退整份配置"
        );
    }

    /// 两种模式都要能往返序列化（用户改了要存得回去、读得回来）。
    #[test]
    fn paste_previous_mode_roundtrips() {
        for m in [PastePreviousMode::NotifyOnly, PastePreviousMode::PasteAndType] {
            let cfg = Config {
                paste_previous_mode: m,
                ..Config::default()
            };
            let s = serde_json::to_string(&cfg).unwrap();
            let back: Config = serde_json::from_str(&s).unwrap();
            assert_eq!(back.paste_previous_mode, m, "往返应保持不变：{m:?}");
        }
    }
