//! 设置数据模型：卡片表、设置项表、数值规格与读写访问器。
//!
//! 数据驱动：界面渲染、搜索、重置都遍历同一份 `SETTINGS_CARDS`，
//! 新增设置项只改这里 + `app.rs` 的字段处理器。

use egui::Align2;
use egui::Rect;
use egui::Ui;
use egui::vec2;
use modular_clipboard_core::config::RowMetaPart;
use modular_clipboard_core::config::WindowBackdrop;
use modular_clipboard_core::i18n::Bi;
use modular_clipboard_core::i18n::t;
use crate::card::PinnedMode;

use super::{Frame, Op, ConfigGroup, SettingsCat, HotkeySlot};

use super::settings_rows::CTRL_MIN_H;
use super::settings_rows::head_line_height;
use super::settings_rows::settings_item_row;
use super::settings_rows::settings_recent_view;
use super::settings_rows::settings_search_results;
use super::controls::draw_slider;
use super::theme_editor::draw_token_panel;

// ---------------------------------------------------------------------------
// 设置卡片描述：每张卡片 = 标题 + 设置项列表。
//
// 数据驱动的好处：
// 1. 「重置按钮」与「搜索」都遍历同一份结构，不会出现
//    「界面渲染一项、重置漏一项」的分叉；
// 2. 新增设置项只改这里 + `app.rs` 的字段处理器。
// ---------------------------------------------------------------------------

/// 数值项规格（含存储值 ↔ 显示值换算，如字节 ↔ MB）。
pub(super) struct NumberSpec {
    pub(super) min: f64,
    pub(super) max: f64,
    pub(super) step: f64,
    pub(super) unit: Bi,
    /// 存储值 → 显示值。
    pub(super) display: fn(f64) -> f64,
    /// 显示值 → 存储值。
    pub(super) store: fn(f64) -> f64,
}

/// 可选数值项规格（`None` = 不限）。
pub(super) struct OptNumberSpec {
    pub(super) default: f64,
    pub(super) step: f64,
    pub(super) unit: Bi,
}

pub(super) const fn identity_num(v: f64) -> f64 {
    v
}
pub(super) const fn bytes_to_mb(v: f64) -> f64 {
    v / 1048576.0
}
pub(super) const fn mb_to_bytes(v: f64) -> f64 {
    v * 1048576.0
}

pub(super) const NUMBER_POLL_MS: NumberSpec = NumberSpec {
    min: 50.0,
    max: 2000.0,
    step: 50.0,
    unit: (" ms", " ms"),
    display: identity_num,
    store: identity_num,
};
/// 迷你面板显示条数：1 ~ 10，步进 1。
pub(super) const NUMBER_MINI_COUNT: NumberSpec = NumberSpec {
    min: 1.0,
    max: 10.0,
    step: 1.0,
    unit: ("条", " items"),
    display: |v| v,
    store: |v| v,
};

pub(super) const NUMBER_MINI_PREVIEW: NumberSpec = NumberSpec {
    min: 10.0,
    max: 100.0,
    step: 4.0,
    unit: ("字符", " chars"),
    display: |v| v,
    store: |v| v,
};

pub(super) const NUMBER_MAX_PAYLOAD_MB: NumberSpec = NumberSpec {
    min: 1.0,
    max: 4096.0,
    step: 1.0,
    unit: (" MB", " MB"),
    display: bytes_to_mb,
    store: mb_to_bytes,
};
pub(super) const OPTN_MAX_ITEMS: OptNumberSpec = OptNumberSpec {
    default: 1000.0,
    step: 100.0,
    unit: (" 条", " entries"),
};

/// 设置项类型。
pub(super) enum ItemKind {
    Bool(ConfigGroup, &'static str),
    Number(ConfigGroup, &'static str, &'static NumberSpec),
    OptNumber(ConfigGroup, &'static str, &'static OptNumberSpec),
    /// 置顶模式单选（共用单栏 / 独立分栏）。
    PinnedMode,
    /// 主题单选（跟随系统 / 深色 / 浅色）。
    Theme,
    /// 主题强调色：预设色板 + HSV 滑条 + 十六进制输入 + 恢复默认。
    ///
    /// 独立成变体：这是「一个配置字段 + 三种调节方式」的富编辑器，
    /// `Bool`/`Number` 的通用渲染表达不了色板与多滑条。
    Accent,
    /// 二级主题选择（无 / 纸质纹理 / 护眼绿 / 星空紫 / OLED）。
    ///
    /// 与 [`ItemKind::Backdrop`] 同款分段控件，只是选项是风格层。
    SecondaryTheme,
    /// 当前二级主题的参数旋钮（按 `SecondaryTheme::knobs()` 动态生成）。
    ///
    /// 二级主题选「无」时整项禁用并压暗——与磨砂强度在非磨砂模式下
    /// 禁用的处理一致（见 [`ItemKind::BlurStrength`]）。
    ThemeParams,
    /// 令牌级覆盖编辑器（高级微调）：列出全部主题令牌，逐项可调。
    ///
    /// 「界面上任何元素、任何参数都允许自由调整」的最终出口：二级
    /// 主题的旋钮覆盖常见需求，这里覆盖其余一切。
    TokenOverrides,
    /// 背景效果单选（无 / 磨砂 / 半透明）。
    Backdrop,
    /// 背景不透明度滑块。
    Opacity,
    /// 卡片不透明度滑块（磨砂 / 半透明下让行卡片也透出模糊底图）。
    CardOpacity,
    /// 磨砂强度档位滑块（1 ~ 10，仅磨砂模式有意义）。
    BlurStrength,
    /// 卡片阴影：开关行 + （开启时）模糊 / 浓度 / 垂直偏移三个滑条。
    ///
    /// 与 [`ItemKind::Opacity`] 同理独立成变体——参数不止一个，
    /// 走 `Bool`/`Number` 的通用渲染会得到三个分散的开关/输入框，
    /// 无法表达「关掉开关后参数一起失效」的依赖关系。
    CardShadow,
    /// 右下角信息组合：图标 / 类型 / 大小三个可点击、可拖拽的元素。
    ///
    /// 单击切换显隐（亮 = 显示，灰 = 隐藏），按住拖动重排前后顺序。
    /// 独立成变体：需要同时编辑「顺序 + 三个显隐位」，通用的
    /// `Bool`/`Number` 渲染表达不了拖拽排序。
    RowMeta,
    /// 悬浮类型过滤栏：总开关 + 始终显示 + 多选 + 标签 + 停靠位置 + 图标大小。
    ///
    /// 独立成变体（同 [`ItemKind::CardShadow`]）：六个参数一张互相
    /// 牵连的卡，关掉总开关后其余五个一起失效，走通用渲染会得到
    /// 六个互不相干的开关/滑块。
    TypeRail,
    /// 悬浮搜索面板：停靠位置（窗口上/下）+ 高度。
    ///
    /// 与 [`ItemKind::TypeRail`] 同款收拢，但轻得多（两个参数）。
    SearchPanel,
    /// 通知呈现方式：窗内 toast / 独立弹窗（分段控件）。
    ///
    /// 落点是布尔配置 `ui.notice_popup`（见 `Op::SetBool`）。
    NoticeMode,
    /// 字体缩放：**预设档位**分段控件（小 / 标准 / 大 / 特大 / 超大）。
    ///
    /// # 为什么不用滑条 / 数字输入
    ///
    /// 字号没有「连续可调」的真实需求——用户要的是「字小一点 / 大一点」，
    /// 而 0.05 一档的滑条既看不出差别，又让每拖动一格都触发全量重排。
    /// 预设档位一次到位、可预期、可回归测试（点第 N 段必得第 N 个倍率）。
    /// 值不在预设上时（手工改过配置文件）显示为「自定义」，不假装选中。
    FontScale,
    /// 界面语言单选（跟随系统 / 中文 / English）。
    Language,
    /// 快捷键绑定（只读展示）。
    Hotkey(HotkeySlot),
    /// 「粘贴上一条」的行为模式（仅粘贴并提示 / 直接粘贴并输入）。
    PastePreviousMode,
    /// 开机自启动开关（要写注册表，故单独一个变体）。
    Autostart,
    /// MCP 总开关行：右侧先画开关（最右），再画 ⓘ 详细按钮（开关左边），
    /// 点 ⓘ 打开 MCP 调用设置覆盖层。
    McpToggle,
    /// 文本输入行（MCP 服务地址 / 密钥）。
    ///
    /// `secret=true` 时用密码样式输入（密钥，默认无）。
    Text {
        group: ConfigGroup,
        field: &'static str,
        ph: Bi,
        secret: bool,
    },
    /// MCP 端口（数字输入，留空 = 未设置）。
    McpPort,
    /// 分组小标题（关于页的排布分节）。
    ///
    /// 不是设置项：无值、不可改、无重置。标题文字走 [`SettingsItem::label`]。
    Group,
    /// 数据目录超链接行（关于页）：显示实际目录、点击在文件管理器
    /// 中打开（[`Op::OpenDataDir`]）。
    OpenDataDir,
    /// 可点击的外部链接（关于页）：显示文本即 URL，点击用默认浏览器打开。
    Link(&'static str),
    /// 退出程序按钮（关于页）。
    Quit,
    /// 版本行（关于页）：动态取，cl 版显示 commit、稳定版显示 `0.2`。
    ///
    /// 与静态只读行的区别：静态行的文本是编译期固定的
    /// `&'static str`，而版本号要在**运行时**由
    /// [`about::version_label`] 决定（两分支拼出来的字符串长度不同）。
    Version,
    /// 技术栈行（关于页）：一段可点击文字链，每段打开对应 GitHub 仓库。
    ///
    /// 拆成独立卡片（2026-10-22）：技术栈是「这个程序用什么做的」，
    /// 与「版本 / 协议 / 作者」不是一类信息，且每个词条都要可点。
    Stack,
    /// 开源协议行（关于页）：点击弹出悬浮面板显示协议**全文**。
    ///
    /// 独立成变体：它要画一层覆盖界面（不是改配置、不是开浏览器），
    /// 通用的只读行 / 外链行都表达不了。
    LicenseView,
    /// 版本检查（关于页）：点击后台拉取 git tag 并与本地比较。
    UpdateCheck,
}

/// 字体缩放预设（百分比 → 档位名）。
///
/// 用**整数百分比**而不是 f32 当值：`draw_segmented` 靠 `PartialEq`
/// 判断哪一段高亮，而 f32 的相等比较会被浮点误差击穿（1.1500001 与
/// 1.15 不相等 ⇒ 没有任何一段被选中，界面看着像「没生效」）。
/// 存进配置时再除以 100 还原成倍率。
///
/// 档位覆盖 [`crate::theme::FONT_SCALE_MIN`]~[`crate::theme::FONT_SCALE_MAX`]
/// 区间内的常用档：标准 100% 是默认值，两侧各两档。
pub const FONT_SCALE_PRESETS: &[(u8, Bi)] = &[
    (85, ("小", "Small")),
    (100, ("标准", "Default")),
    (115, ("大", "Large")),
    (130, ("特大", "X-Large")),
    (150, ("超大", "XX-Large")),
];

/// 配置里的 `font_scale` 倍率 → 预设百分比（取最接近的一档）。
///
/// 不在预设上（手工改过配置文件）时也返回最近档位，但
/// [`font_scale_is_preset`] 会是 `false`——调用方据此显示「自定义」，
/// 而不是把最近的档位当成「当前选中」误导用户。
pub(super) fn font_scale_percent(scale: f32) -> u8 {
    let pct = (crate::theme::clamp_font_scale(scale) * 100.0).round() as i32;
    let mut best = FONT_SCALE_PRESETS[0].0;
    let mut best_d = i32::MAX;
    for (p, _) in FONT_SCALE_PRESETS {
        let d = (i32::from(*p) - pct).abs();
        if d < best_d {
            best_d = d;
            best = *p;
        }
    }
    best
}

/// 当前倍率是否**正好**落在某个预设档位上（±0.5% 容差）。
pub(super) fn font_scale_is_preset(scale: f32) -> bool {
    let pct = (crate::theme::clamp_font_scale(scale) * 100.0).round() as i32;
    FONT_SCALE_PRESETS.iter().any(|(p, _)| (i32::from(*p) - pct).abs() <= 0)
}

/// MCP 可调用能力清单（叠加层内展示，顺序即显示顺序）。
pub(super) struct McpCap {
    pub(super) field: &'static str,
    pub(super) name: Bi,
    pub(super) desc: Bi,
    pub(super) ready: bool,
}

/// 全部 MCP 可调用能力（与 `config::McpConfig` 字段一一对应）。
pub(super) static MCP_CAPS: &[McpCap] = &[
    McpCap { field: "resize_window", name: ("调节窗口大小", "Resize windows"), desc: ("调整任意窗口的尺寸与位置", "Resize and move any window"), ready: true },
    McpCap { field: "read_layout", name: ("获取元素布局", "Read UI layout"), desc: ("读取界面元素的几何与层级信息", "Read geometry and hierarchy of UI elements"), ready: true },
    McpCap { field: "click_element", name: ("点击界面元素", "Click UI elements"), desc: ("在任意位置点击或激活元素", "Click or activate elements anywhere"), ready: true },
    McpCap { field: "type_text", name: ("输入文本", "Type text"), desc: ("向焦点控件键入文字与快捷键", "Type text and shortcuts into the focused control"), ready: true },
    McpCap { field: "scroll_view", name: ("滚动视图", "Scroll views"), desc: ("滚动页面与列表到目标位置", "Scroll pages and lists to a target position"), ready: true },
    McpCap { field: "screen_capture", name: ("屏幕截图", "Screen capture"), desc: ("截取屏幕或窗口画面", "Capture the screen or a window"), ready: true },
    McpCap { field: "window_focus", name: ("切换焦点窗口", "Focus windows"), desc: ("激活指定窗口或应用", "Activate a given window or app"), ready: false },
    McpCap { field: "drag_drop", name: ("拖拽操作", "Drag and drop"), desc: ("拖拽元素或执行拖放", "Drag elements or perform drag-and-drop"), ready: false },
    McpCap { field: "key_press", name: ("按键模拟", "Keystrokes"), desc: ("按下组合键与功能键（快捷键、确认/取消）", "Press key combos and function keys (shortcuts, confirm/cancel)"), ready: true },
    McpCap { field: "clipboard", name: ("剪贴板读写", "Clipboard access"), desc: ("读取或写入系统剪贴板文本", "Read or write system clipboard text"), ready: true },
];

/// 读一个 MCP 能力开关。
pub(super) fn get_mcp_cap(cfg: &modular_clipboard_core::config::Config, field: &str) -> bool {
    let m = &cfg.mcp;
    match field {
        "resize_window" => m.resize_window,
        "read_layout" => m.read_layout,
        "click_element" => m.click_element,
        "type_text" => m.type_text,
        "scroll_view" => m.scroll_view,
        "screen_capture" => m.screen_capture,
        "window_focus" => m.window_focus,
        "drag_drop" => m.drag_drop,
        "key_press" => m.key_press,
        "clipboard" => m.clipboard,
        _ => false,
    }
}

/// 一键 AI 配对提示词：可调用能力清单 + stdio / HTTP 两种接入配置。
///
/// `pub`：`App::apply_ops` 的 `CopyMcpPairPrompt` 分支读取它写入剪贴板。
pub fn mcp_pair_prompt(cfg: &modular_clipboard_core::config::Config) -> String {
    let mut s = String::from(t((
        "【modular-clipboard · MCP 服务配对】\nmodular-clipboard 提供 Model Context Protocol（MCP）服务，可对本机执行界面自动化操作。\n\n可调用工具（10 项）：\n",
        "【modular-clipboard · MCP pairing】\nmodular-clipboard exposes a Model Context Protocol (MCP) service that can run UI automation on this machine.\n\nAvailable tools (10):\n",
    )));
    for cap in MCP_CAPS {
        s.push_str(&format!("- {}：{}\n", t(cap.name), t(cap.desc)));
    }

    // 当前 exe 完整路径（stdio 配置里 command 用）；取不到时用占位。
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| t(("<modular-clipboard.exe 的完整路径>", "<full path to modular-clipboard.exe>")).into());

    s.push_str(t((
        "\n=== 方式一：stdio（本地，推荐） ===\n适用于 Claude Desktop / Cursor 等本地客户端，在 MCP 配置中加入：\n",
        "\n=== Option 1: stdio (local, recommended) ===\nFor local clients such as Claude Desktop / Cursor, add this to your MCP config:\n",
    )));
    s.push_str(&format!(
        "{{\n  \"mcpServers\": {{\n    \"modular-clipboard\": {{\n      \"command\": \"{exe}\",\n      \"args\": [\"--mcp-server\"]\n    }}\n  }}\n}}\n"
    ));

    // HTTP 段：地址、端口、认证。
    let host = if cfg.mcp.host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        cfg.mcp.host.clone()
    };
    let port = if cfg.mcp.port == 0 { 9123 } else { cfg.mcp.port };
    let auth = if cfg.mcp.token.is_empty() {
        t(("无", "None")).to_string()
    } else {
        t(("Bearer Token（已在设置中配置）", "Bearer token (configured in settings)")).to_string()
    };
    s.push_str(
        &t((
            "\n=== 方式二：HTTP（远程 / 自定义地址） ===\n服务地址：http://{host}:{port}/mcp\n认证：{auth}\n配置：\n",
            "\n=== Option 2: HTTP (remote / custom address) ===\nService URL: http://{host}:{port}/mcp\nAuth: {auth}\nConfig:\n",
        ))
        .replace("{host}", &host)
        .replace("{port}", &port.to_string())
        .replace("{auth}", &auth),
    );
    s.push_str(&format!(
        "{{\n  \"mcpServers\": {{\n    \"modular-clipboard\": {{\n      \"url\": \"http://{host}:{port}/mcp\"\n    }}\n  }}\n}}\n"
    ));
    s.push_str(t((
        "启动 HTTP 服务：modular-clipboard.exe --mcp-http\n",
        "Start the HTTP service: modular-clipboard.exe --mcp-http\n",
    )));
    s
}

/// 一个设置项。
///
/// `key` 是**稳定标识**（搜索「最近修改」的去重键、复位时的匹配键）：
/// 文案会随界面语言变，不能当身份用（见 [`crate::i18n`] 模块注释）。
pub(super) struct SettingsItem {
    pub(super) key: &'static str,
    pub(super) label: Bi,
    pub(super) desc: Bi,
    pub(super) kind: ItemKind,
}

impl SettingsItem {
    /// 当前值是否偏离默认（决定重置按钮是否出现）。
    pub(super) fn is_changed(
        &self,
        cfg: &modular_clipboard_core::config::Config,
        ws: &crate::workspace::Workspace,
    ) -> bool {
        let d = modular_clipboard_core::config::Config::default();
        match &self.kind {
            ItemKind::Bool(g, f) => get_bool(cfg, *g, f) != get_bool(&d, *g, f),
            ItemKind::Number(g, f, _) => get_number(cfg, *g, f) != get_number(&d, *g, f),
            ItemKind::OptNumber(g, f, _) => {
                get_opt_number(cfg, *g, f) != get_opt_number(&d, *g, f)
            }
            ItemKind::PinnedMode => ws.pinned_mode() != PinnedMode::SharedColumn,
            ItemKind::Theme => cfg.ui.dark_mode.is_some(),
            ItemKind::Accent => cfg.ui.accent.is_some(),
            ItemKind::SecondaryTheme => {
                cfg.ui.secondary_theme != modular_clipboard_core::config::SecondaryTheme::None
            }
            ItemKind::ThemeParams => {
                cfg.ui.theme_params != modular_clipboard_core::config::ThemeParams::default()
            }
            ItemKind::TokenOverrides => {
                !cfg.ui.theme_overrides.colors.is_empty()
                    || !cfg.ui.theme_overrides.nums.is_empty()
            }
            ItemKind::Backdrop => cfg.ui.window_backdrop != WindowBackdrop::None,
            ItemKind::Opacity => (cfg.ui.window_opacity - 1.0).abs() > f32::EPSILON,
            ItemKind::CardOpacity => (cfg.ui.card_opacity - 1.0).abs() > f32::EPSILON,
            ItemKind::BlurStrength => cfg.ui.window_blur_strength != d.ui.window_blur_strength,
            ItemKind::CardShadow => cfg.ui.card_shadow != d.ui.card_shadow,
            ItemKind::RowMeta => cfg.ui.row_meta != d.ui.row_meta,
            ItemKind::TypeRail => cfg.ui.type_rail != d.ui.type_rail,
            ItemKind::SearchPanel => cfg.ui.search_panel != d.ui.search_panel,
            ItemKind::NoticeMode => cfg.ui.notice_popup != d.ui.notice_popup,
            ItemKind::FontScale => (cfg.ui.font_scale - d.ui.font_scale).abs() > 1e-6,
            ItemKind::Hotkey(slot) => slot.current(cfg) != slot.default(),
            ItemKind::Autostart => cfg.ui.autostart != d.ui.autostart,
            ItemKind::McpToggle => {
                get_bool(cfg, ConfigGroup::Mcp, "enabled") != get_bool(&d, ConfigGroup::Mcp, "enabled")
            }
            ItemKind::Text { group, field, .. } => {
                get_text(cfg, *group, field) != get_text(&d, *group, field)
            }
            ItemKind::McpPort => {
                get_number(cfg, ConfigGroup::Mcp, "port") != get_number(&d, ConfigGroup::Mcp, "port")
            }
            ItemKind::Language => cfg.ui.language != d.ui.language,
            ItemKind::Group
            | ItemKind::OpenDataDir
            | ItemKind::Link(_)
            | ItemKind::Quit
            | ItemKind::Version
            | ItemKind::Stack
            | ItemKind::LicenseView
            | ItemKind::UpdateCheck
            | ItemKind::PastePreviousMode => false,
        }
    }

    /// 该设置项的重置操作（恢复默认值）。
    pub(super) fn reset_op(&self) -> Option<Op> {
        let d = modular_clipboard_core::config::Config::default();
        match &self.kind {
            ItemKind::Bool(g, f) => Some(Op::SetBool {
                group: *g,
                field: f,
                value: get_bool(&d, *g, f),
            }),
            ItemKind::Number(g, f, _) => Some(Op::SetNumber {
                group: *g,
                field: f,
                value: get_number(&d, *g, f),
            }),
            ItemKind::OptNumber(g, f, _) => Some(Op::SetOptNumber {
                group: *g,
                field: f,
                value: get_opt_number(&d, *g, f),
            }),
            ItemKind::Language => Some(Op::SetLanguage(
                modular_clipboard_core::i18n::LANG_SYSTEM,
            )),
            ItemKind::PinnedMode => Some(Op::SetPinnedMode(PinnedMode::SharedColumn)),
            ItemKind::Theme => Some(Op::SetThemeMode(crate::theme::ThemeMode::FollowSystem)),
            ItemKind::Accent => Some(Op::SetAccent(None)),
            ItemKind::SecondaryTheme => Some(Op::SetSecondaryTheme(
                modular_clipboard_core::config::SecondaryTheme::None,
            )),
            ItemKind::ThemeParams => Some(Op::SetThemeParams(
                modular_clipboard_core::config::ThemeParams::default(),
            )),
            ItemKind::TokenOverrides => Some(Op::ClearAllTokenOverrides),
            ItemKind::Backdrop => Some(Op::SetBackdrop(WindowBackdrop::None)),
            ItemKind::Opacity => Some(Op::SetOpacity(1.0)),
            ItemKind::CardOpacity => Some(Op::SetCardOpacity(d.ui.card_opacity)),
            ItemKind::BlurStrength => Some(Op::SetBlurStrength(d.ui.window_blur_strength)),
            // 字体缩放：恢复默认倍率（1.0×）。走 SetNumber 而非专用 Op——
            // 应用侧的即时生效钩子正是按 (Ui, "font_scale") 匹配的。
            ItemKind::FontScale => Some(Op::SetNumber {
                group: ConfigGroup::Ui,
                field: "font_scale",
                value: f64::from(d.ui.font_scale),
            }),
            ItemKind::Autostart => Some(Op::SetAutostart(d.ui.autostart)),
            ItemKind::Hotkey(slot) => Some(Op::SetHotkey {
                slot: *slot,
                hotkey: slot.default(),
            }),
            ItemKind::McpToggle => Some(Op::SetBool {
                group: ConfigGroup::Mcp,
                field: "enabled",
                value: get_bool(&d, ConfigGroup::Mcp, "enabled"),
            }),
            ItemKind::Text { group, field, .. } => Some(Op::SetText {
                group: *group,
                field,
                value: get_text(&d, *group, field),
            }),
            ItemKind::McpPort => Some(Op::SetNumber {
                group: ConfigGroup::Mcp,
                field: "port",
                value: get_number(&d, ConfigGroup::Mcp, "port"),
            }),
            ItemKind::CardShadow => Some(Op::SetCardShadow(d.ui.card_shadow)),
            ItemKind::RowMeta => Some(Op::SetRowMeta(d.ui.row_meta)),
            ItemKind::TypeRail => Some(Op::SetTypeRail(d.ui.type_rail)),
            ItemKind::SearchPanel => Some(Op::SetSearchPanel(d.ui.search_panel)),
            ItemKind::NoticeMode => Some(Op::SetBool {
                group: ConfigGroup::Ui,
                field: "notice_popup",
                value: d.ui.notice_popup,
            }),
            ItemKind::Group
            | ItemKind::OpenDataDir
            | ItemKind::Link(_)
            | ItemKind::Quit
            | ItemKind::Version
            | ItemKind::Stack
            | ItemKind::LicenseView
            | ItemKind::UpdateCheck
            | ItemKind::PastePreviousMode => None,
        }
    }
}

/// 一张设置卡片。
pub(super) struct SettingsCard {
    pub(super) cat: SettingsCat,
    pub(super) title: Bi,
    pub(super) items: &'static [SettingsItem],
}

/// 全部分类卡片（导航顺序与 `SettingsCat::ALL` 一致）。
pub(super) static SETTINGS_CARDS: &[SettingsCard] = &[
    SettingsCard {
        cat: SettingsCat::General,
        title: ("常规", "General"),
        items: &[
            SettingsItem {
                key: "ui.language",
                label: ("界面语言", "Interface language"),
                desc: (
                    "显示语言：跟随系统，或固定为列表中的一种",
                    "Display language: follow the system, or pin one from the list",
                ),
                kind: ItemKind::Language,
            },
            SettingsItem {
                key: "ui.always_on_top",
                label: ("窗口置顶", "Always on top"),
                desc: ("主窗口始终显示在其它窗口之上", "Keep the main window above other windows"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "always_on_top"),
            },
            SettingsItem {
                key: "ui.hide_on_focus_lost",
                label: ("失焦时隐藏", "Hide on focus loss"),
                desc: ("切换到其它程序时自动隐藏主窗口", "Hide the main window when another app gains focus"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "hide_on_focus_lost"),
            },
            SettingsItem {
                key: "ui.show_tray",
                label: ("显示托盘图标", "Show tray icon"),
                desc: ("后台常驻时在系统托盘显示图标与菜单", "Show an icon and menu in the system tray while running"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "show_tray"),
            },
            SettingsItem {
                key: "ui.start_minimized",
                label: ("启动时最小化", "Start minimized"),
                desc: ("启动后不显示主窗口，直接驻留托盘", "Start hidden in the tray instead of opening the window"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "start_minimized"),
            },
            SettingsItem {
                key: "ui.pinned_mode",
                label: ("置顶布局", "Pinned layout"),
                desc: ("置顶条目与历史同栏混排，或独立成可拖出的分栏", "Pin entries in the same list, or in a separate draggable column"),
                kind: ItemKind::PinnedMode,
            },
            SettingsItem {
                key: "ui.autostart",
                label: ("开机自启动", "Launch at startup"),
                desc: ("登录后仅启动后台记录，界面从托盘唤起", "Start recording in the background at sign-in; open the window from the tray"),
                kind: ItemKind::Autostart,
            },
            SettingsItem {
                key: "ui.browser_path",
                label: ("浏览器路径", "Browser path"),
                desc: ("打开链接条目用的程序；留空则用系统默认浏览器", "App used to open link entries; empty = system default browser"),
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "browser_path",
                    ph: ("默认（系统默认浏览器）", "Default (system browser)"),
                    secret: false,
                },
            },
            SettingsItem {
                key: "ui.explorer_path",
                label: ("文件管理器路径", "File manager path"),
                desc: ("打开文件条目用的程序；留空则用系统资源管理器", "App used to open file entries; empty = system file manager"),
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "explorer_path",
                    ph: ("默认（explorer.exe）", "Default (explorer.exe)"),
                    secret: false,
                },
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Theme,
        title: ("主题", "Theme"),
        items: &[
            SettingsItem {
                key: "ui.dark_mode",
                label: ("明暗模式", "Light / dark mode"),
                desc: ("跟随系统，或强制深色 / 浅色", "Follow the system, or force dark / light"),
                kind: ItemKind::Theme,
            },
            SettingsItem {
                key: "ui.accent",
                label: ("主题色", "Accent color"),
                desc: ("界面强调色：色板、色相滑块或十六进制任选", "Accent color: pick from swatches, sliders, or a hex value"),
                kind: ItemKind::Accent,
            },
            SettingsItem {
                key: "ui.secondary_theme",
                label: ("风格主题", "Style theme"),
                desc: ("叠加在明暗模式上的风格层：纸纹 / 护眼绿 / 星空紫 / OLED", "A style layer on top of light / dark: paper, eye-care green, starfield purple, OLED"),
                kind: ItemKind::SecondaryTheme,
            },
            SettingsItem {
                key: "ui.secondary_theme_params",
                label: ("风格参数", "Style parameters"),
                desc: ("当前风格主题的浓度与效果强度", "Intensity and strength of the current style theme"),
                kind: ItemKind::ThemeParams,
            },
            SettingsItem {
                key: "ui.token_overrides",
                label: ("令牌微调", "Token overrides"),
                desc: ("逐项覆盖主题取值：任何元素、任何参数", "Override individual theme tokens: any element, any value"),
                kind: ItemKind::TokenOverrides,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: ("外观", "Appearance"),
        items: &[
            SettingsItem {
                key: "ui.font_scale",
                label: ("字体缩放", "Font scale"),
                desc: ("界面文字大小；预设档位即改即生效", "Text size; presets apply instantly"),
                kind: ItemKind::FontScale,
            },
            SettingsItem {
                key: "ui.window_backdrop",
                label: ("背景效果", "Background effect"),
                desc: ("无、磨砂（实时模糊）或半透明", "None, frosted blur, or translucent"),
                kind: ItemKind::Backdrop,
            },
            SettingsItem {
                key: "ui.blur_strength",
                label: ("磨砂强度", "Blur strength"),
                desc: ("磨砂模式下的模糊程度", "Blur intensity in frosted mode"),
                kind: ItemKind::BlurStrength,
            },
            SettingsItem {
                key: "ui.window_opacity",
                label: ("背景不透明度", "Background opacity"),
                desc: ("背景层的透明程度；文字始终不透明", "Transparency of the background layer; text stays opaque"),
                kind: ItemKind::Opacity,
            },
            SettingsItem {
                key: "ui.card_opacity",
                label: ("卡片不透明度", "Card opacity"),
                desc: ("条目卡片的实度；调低可透出模糊背景（磨砂 / 半透明下可调）", "Opacity of entry cards; lower it to reveal the blurred background"),
                kind: ItemKind::CardOpacity,
            },
            SettingsItem {
                key: "ui.card_shadow",
                label: ("卡片阴影", "Card shadow"),
                desc: ("条目卡片的投影；深色模式下自动加深", "Drop shadow under entry cards; dark mode deepens it automatically"),
                kind: ItemKind::CardShadow,
            },
            SettingsItem {
                key: "ui.row_meta",
                label: ("卡片角标", "Card meta badges"),
                desc: ("卡片右下角的图标、类型与大小：单击显隐，拖动排序", "Icon, type and size at the card's corner: click to toggle, drag to reorder"),
                kind: ItemKind::RowMeta,
            },
            SettingsItem {
                key: "ui.image_inline",
                label: ("图片缩略图", "Image thumbnails"),
                desc: ("图片条目直接显示缩略图；关闭后仅显示尺寸", "Show image entries as thumbnails; off = size text only"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "image_inline"),
            },
            SettingsItem {
                key: "ui.md_rendered",
                label: ("Markdown 渲染", "Render Markdown"),
                desc: ("按标题、列表与代码块排版；关闭后显示等宽源码", "Lay out headings, lists and code blocks; off = monospaced source"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "md_rendered"),
            },
            SettingsItem {
                key: "ui.type_rail",
                label: ("类型过滤栏", "Type filter bar"),
                desc: ("窗口边缘的悬浮类型按钮：单击过滤，可多选、可调位置与大小", "Floating type buttons at the window edge: click to filter; multi-select, adjustable position and size"),
                kind: ItemKind::TypeRail,
            },
            SettingsItem {
                key: "ui.search_panel",
                label: ("搜索面板", "Search panel"),
                desc: ("悬浮搜索面板：停靠位置与高度可调", "Floating search panel; dock position and height adjustable"),
                kind: ItemKind::SearchPanel,
            },
            SettingsItem {
                key: "ui.notice_popup",
                label: ("通知方式", "Notifications"),
                desc: ("操作反馈的显示：窗口内提示或独立弹窗", "How feedback appears: in-window toasts or a separate popup"),
                kind: ItemKind::NoticeMode,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Capture,
        title: ("捕获", "Capture"),
        items: &[
            SettingsItem {
                key: "capture.enabled",
                label: ("记录剪贴板", "Record clipboard"),
                desc: ("关闭后不再新增历史条目", "Stop adding new entries when off"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "enabled"),
            },
            SettingsItem {
                key: "capture.skip_password_fields",
                label: ("跳过密码字段", "Skip password fields"),
                desc: ("疑似输入密码时不记录剪贴内容", "Skip clipboard content while passwords are being typed"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "skip_password_fields"),
            },
            SettingsItem {
                key: "capture.dedup",
                label: ("复制去重", "Deduplicate copies"),
                desc: ("内容相同时不新增，改为置顶更新", "Identical content is bumped to the top instead of duplicated"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "dedup"),
            },
            SettingsItem {
                key: "capture.poll_interval_ms",
                label: ("监听间隔", "Polling interval"),
                desc: ("剪贴板检查间隔；越小越灵敏、越耗资源", "How often the clipboard is checked; lower is faster but heavier"),
                kind: ItemKind::Number(ConfigGroup::Capture, "poll_interval_ms", &NUMBER_POLL_MS),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Storage,
        title: ("存储", "Storage"),
        items: &[
            SettingsItem {
                key: "storage.max_items",
                label: ("历史条数上限", "Max entries"),
                desc: ("保留的条目数量；留空则只按容量淘汰", "How many entries to keep; empty = size-based eviction only"),
                kind: ItemKind::OptNumber(ConfigGroup::Storage, "max_items", &OPTN_MAX_ITEMS),
            },
            SettingsItem {
                key: "storage.max_payload_bytes",
                label: ("单条大小上限", "Max entry size"),
                desc: ("超出该大小的剪贴内容不记录", "Clipboard content larger than this is not recorded"),
                kind: ItemKind::Number(ConfigGroup::Storage, "max_payload_bytes", &NUMBER_MAX_PAYLOAD_MB),
            },
            SettingsItem {
                key: "storage.cleanup_on_start",
                label: ("清理临时文件", "Clean up temp files"),
                desc: ("启动时删除上次退出遗留的临时文件", "Remove temp files left over from the previous run on startup"),
                kind: ItemKind::Bool(ConfigGroup::Storage, "cleanup_on_start"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: ("快捷键", "Shortcuts"),
        items: &[
            SettingsItem {
                key: "hotkey.toggle",
                label: ("显示 / 隐藏主窗口", "Show / hide main window"),
                desc: ("全局唤起或隐藏主窗口", "Toggle the main window from anywhere"),
                kind: ItemKind::Hotkey(HotkeySlot::Toggle),
            },
            SettingsItem {
                key: "hotkey.paste_previous",
                label: ("粘贴上一条", "Paste previous entry"),
                desc: ("把最近一条历史直接粘贴到当前输入处", "Paste the most recent entry into the focused app"),
                kind: ItemKind::Hotkey(HotkeySlot::PastePrevious),
            },
            SettingsItem {
                key: "hotkey.paste_previous_mode",
                label: ("粘贴方式", "Paste mode"),
                desc: (
                    "「仅粘贴并提示」只写剪贴板由你手动粘贴；「直接粘贴并输入」会向目标窗口发送 Ctrl+V",
                    "'Copy & notify' only fills the clipboard; 'Copy & paste' also sends Ctrl+V to the target",
                ),
                kind: ItemKind::PastePreviousMode,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: ("迷你面板", "Mini panel"),
        items: &[
            SettingsItem {
                key: "mini_panel.enabled",
                label: ("启用", "Enable"),
                desc: ("按住修饰键右键点击，在光标处弹出最近几条", "Right-click with a modifier to pop up recent entries at the cursor"),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "enabled"),
            },
            SettingsItem {
                key: "mini_panel.count",
                label: ("显示条数", "Entry count"),
                desc: ("面板中显示的最近条目数", "Number of recent entries shown in the panel"),
                kind: ItemKind::Number(ConfigGroup::MiniPanel, "count", &NUMBER_MINI_COUNT),
            },
            SettingsItem {
                key: "mini_panel.preview_chars",
                label: ("预览长度", "Preview length"),
                desc: ("每项显示的最大字符数（控制宽度）", "Max chars per item (controls width)"),
                kind: ItemKind::Number(ConfigGroup::MiniPanel, "preview_chars", &NUMBER_MINI_PREVIEW),
            },
            SettingsItem {
                key: "mini_panel.ctrl",
                label: ("Ctrl + 右键", "Ctrl + right-click"),
                desc: ("触发修饰键，可叠加；至少保留一个", "Trigger modifier; these can stack, keep at least one"),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "ctrl"),
            },
            SettingsItem {
                key: "mini_panel.shift",
                label: ("Shift + 右键", "Shift + right-click"),
                desc: ("", ""),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "shift"),
            },
            SettingsItem {
                key: "mini_panel.alt",
                label: ("Alt + 右键", "Alt + right-click"),
                desc: ("", ""),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "alt"),
            },
            SettingsItem {
                key: "mini_panel.win",
                label: ("Win + 右键", "Win + right-click"),
                desc: ("", ""),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "win"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Comm,
        title: ("MCP 服务", "MCP service"),
        items: &[
            SettingsItem {
                key: "mcp.enabled",
                label: ("MCP 服务", "MCP service"),
                desc: ("关闭后所有 MCP 调用不可用", "Turn off to disable all MCP calls"),
                kind: ItemKind::McpToggle,
            },
            SettingsItem {
                key: "mcp.host",
                label: ("监听地址", "Listen address"),
                desc: ("MCP 服务监听的地址", "Address the MCP service listens on"),
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "host", ph: ("127.0.0.1", "127.0.0.1"), secret: false },
            },
            SettingsItem {
                key: "mcp.port",
                label: ("端口", "Port"),
                desc: ("MCP 服务端口；留空则不启用", "Port for the MCP service; empty = disabled"),
                kind: ItemKind::McpPort,
            },
            SettingsItem {
                key: "mcp.token",
                label: ("访问密钥", "Access token"),
                desc: ("访问密钥；留空表示无需认证", "Access token; empty = no authentication"),
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "token", ph: ("无", "None"), secret: true },
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::About,
        title: ("关于", "About"),
        items: &[
            SettingsItem {
                key: "about.group.app",
                label: ("软件信息", "App info"),
                desc: ("", ""),
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.version",
                label: ("版本", "Version"),
                desc: ("", ""),
                kind: ItemKind::Version,
            },
            SettingsItem {
                key: "about.update",
                label: ("版本检查", "Check for updates"),
                desc: ("从仓库拉取最新 tag 并与本地比较", "Fetch the latest tag and compare with local"),
                kind: ItemKind::UpdateCheck,
            },
            SettingsItem {
                key: "about.license",
                label: ("开源协议", "License"),
                desc: ("点击查看协议全文", "Click to view the full text"),
                kind: ItemKind::LicenseView,
            },
            SettingsItem {
                key: "about.group.project",
                label: ("项目", "Project"),
                desc: ("", ""),
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.repo",
                label: ("项目主页", "Project page"),
                desc: ("源代码、问题反馈与发布版本", "Source code, issue tracker and releases"),
                kind: ItemKind::Link("https://github.com/MCheng404/ModularClipboard"),
            },
            SettingsItem {
                key: "about.author",
                label: ("作者主页", "Author page"),
                desc: ("开发者的 GitHub 主页", "The developer's GitHub profile"),
                kind: ItemKind::Link("https://github.com/MCheng404"),
            },
            SettingsItem {
                key: "about.group.data",
                label: ("数据", "Data"),
                desc: ("", ""),
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.data_dir",
                label: ("数据目录", "Data directory"),
                desc: ("点击在文件管理器中打开", "Click to open in the file manager"),
                kind: ItemKind::OpenDataDir,
            },
            SettingsItem {
                key: "about.quit",
                label: ("退出程序", "Quit"),
                desc: ("退出程序并停止后台记录", "Quit the app and stop recording"),
                kind: ItemKind::Quit,
            },
        ],
    },
    // 技术栈单列一张卡（2026-10-22 用户要求）：它是「用什么做的」，
    // 与「版本 / 协议 / 作者 / 数据目录」不同类，而且每个词条都要可点。
    // 放在「关于」卡片**之后**：先看软件自身的事实（版本/协议/数据），
    // 再看「它是怎么搭出来的」。
    SettingsCard {
        cat: SettingsCat::About,
        title: ("技术栈", "Built with"),
        items: &[SettingsItem {
            key: "about.stack",
            label: ("技术栈", "Built with"),
            desc: ("点击任一名称打开它的官方仓库", "Click a name to open its repository"),
            kind: ItemKind::Stack,
        }],
    },
];

/// 读一个布尔配置字段（与 `app.rs::set_bool_field` 的键表对称）。
pub(super) fn get_bool(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> bool {
    use ConfigGroup as G;
    match (g, field) {
        (G::Capture, "enabled") => cfg.capture.enabled,
        (G::Capture, "skip_password_fields") => cfg.capture.skip_password_fields,
        (G::Capture, "dedup") => cfg.capture.dedup,
        (G::Ui, "always_on_top") => cfg.ui.always_on_top,
        (G::Ui, "hide_on_focus_lost") => cfg.ui.hide_on_focus_lost,
        (G::Ui, "show_tray") => cfg.ui.show_tray,
        (G::Ui, "start_minimized") => cfg.ui.start_minimized,
        (G::Ui, "autostart") => cfg.ui.autostart,
        // ---- 第三轮新增 ----
        (G::Ui, "image_inline") => cfg.ui.image_inline,
        (G::Ui, "md_rendered") => cfg.ui.md_rendered,
        // NOTE: type_rail 的四个参数走 `Op::SetTypeRail` 整包替换
        // （ItemKind::TypeRail），这里不再按字段名分发。
        (G::MiniPanel, "enabled") => cfg.mini_panel.enabled,
        (G::MiniPanel, "ctrl") => cfg.mini_panel.ctrl,
        (G::MiniPanel, "shift") => cfg.mini_panel.shift,
        (G::MiniPanel, "alt") => cfg.mini_panel.alt,
        (G::MiniPanel, "win") => cfg.mini_panel.win,
        (G::Storage, "cleanup_on_start") => cfg.storage.cleanup_on_start,
        (G::Mcp, "enabled") => cfg.mcp.enabled,
        (G::Mcp, f) if MCP_CAPS.iter().any(|c| c.field == f) => get_mcp_cap(cfg, f),
        _ => false,
    }
}

/// 读一个数值配置字段（存储单位，未换算）。
pub(super) fn get_number(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> f64 {
    use ConfigGroup as G;
    match (g, field) {
        (G::Capture, "poll_interval_ms") => cfg.capture.poll_interval_ms as f64,
        (G::Ui, "font_scale") => cfg.ui.font_scale as f64,
        (G::MiniPanel, "count") => cfg.mini_panel.count as f64,
        (G::Storage, "max_payload_bytes") => cfg.storage.max_payload_bytes as f64,
        (G::Mcp, "port") => cfg.mcp.port as f64,
        _ => 0.0,
    }
}

/// 读一个文本配置字段（MCP 服务地址 / 密钥）。
pub(super) fn get_text(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> String {
    use ConfigGroup as G;
    match (g, field) {
        (G::Mcp, "host") => cfg.mcp.host.clone(),
        (G::Mcp, "token") => cfg.mcp.token.clone(),
        // 第三轮：双击跳转调用路径（空 = 系统默认）。
        (G::Ui, "browser_path") => cfg.ui.browser_path.clone(),
        (G::Ui, "explorer_path") => cfg.ui.explorer_path.clone(),
        _ => String::new(),
    }
}

/// 读一个可选数值配置字段。
pub(super) fn get_opt_number(
    cfg: &modular_clipboard_core::config::Config,
    g: ConfigGroup,
    field: &str,
) -> Option<f64> {
    use ConfigGroup as G;
    match (g, field) {
        (G::Storage, "max_items") => cfg.storage.max_items.map(|n| n as f64),
        _ => None,
    }
}

/// 设置内容视图分发：搜索非空 → 结果；空态聚焦 → 最近修改；否则当前分类。
pub(super) fn settings_content(f: &mut Frame<'_>, ui: &mut Ui) {
    let q = f.state.settings_query.trim().to_string();
    if !q.is_empty() {
        settings_search_results(f, ui, &q);
    } else if f.state.settings_show_recent {
        settings_recent_view(f, ui);
    } else if f.state.settings_cat == SettingsCat::Theme && f.state.theme_tokens_expanded {
        // 二级视图（2026-10-19）：令牌面板占据整个右侧内容区——用户要求
        // 「基本覆盖设置窗口的一大半区域」。左导航与搜索框保持可见，
        // 返回按钮（面板左上角 ‹）关掉它。
        draw_token_panel(f, ui);
    } else {
        settings_category_cards(f, ui);
    }
}

/// 当前分类的卡片组。
///
/// # CAUTION: 必须 `filter` 遍历全部，不能 `find` 只取第一张
///
/// 同一个 `SettingsCat` 可以有**多张**卡片（`About` 就有「关于」与
/// 「技术栈」两张）。早前这里用 `find(...)`，于是第二张起永远不会被
/// 画出来——表现为「技术栈卡片加了却看不见」（2026-10-22 用户反馈）。
///
/// 卡片之间留 12pt 间距：`Frame` 的圆角背景自带外缘，不留白的话
/// 两张卡片会贴在一起，看不出是两块。
pub(super) fn settings_category_cards(f: &mut Frame<'_>, ui: &mut Ui) {
    // CAUTION: 必须先把 `settings_cat` **拷出来**再迭代。
    //
    // 闭包里读 `f.state.settings_cat` 会借住 `&f`，而循环体里
    // `settings_card(f, ...)` 要 `&mut f`——两者同一作用域直接撞
    // 借用检查（E0502）。`SettingsCat` 是 Copy 的小枚举，拷出来零成本。
    let cat = f.state.settings_cat;
    let mut first = true;
    for card in SETTINGS_CARDS.iter().filter(|c| c.cat == cat) {
        if !first {
            ui.add_space(12.0);
        }
        first = false;
        settings_card(f, ui, card);
    }
}

/// 一张卡片：圆角背景 + 标题行（标题 + 条件重置按钮）+ 设置行。
///
/// # 背景为什么用 `egui::Frame` 而不是先算高度再 `rect_filled`
///
/// 早前高度用固定公式 `head_h + 44×行数 + pad×2` 估算，而行高实际随内容
/// 变化：描述文字换行、分段控件（30pt）、滑块、文本输入框都比 44pt 的
/// 假设高。公式低估时底部几行超出背景——用户看到「内容下半截露在卡片
/// 外」（外观卡有多行描述 + 分段控件，最明显）。
///
/// `Frame::show` 内部先往 painter 塞一个 `Noop` 占位，内容测量完再把背景
/// 形状填进那个占位符：背景尺寸恒等于内容尺寸，且 z 序在内容之下
/// （不会盖住文字）。这是「背景包裹内容」的标准做法，行高怎么变都不会露底。
pub(super) fn settings_card(f: &mut Frame<'_>, ui: &mut Ui, card: &SettingsCard) {
    let pal = f.pal;
    egui::Frame::new()
        .fill(pal.surface)
        .corner_radius(pal.radius_lg)
        .inner_margin(egui::Margin::symmetric(12, 6))
        .show(ui, |ui| {
            // 行间距：设置项之间拉开呼吸感（2.0 太挤，文字贴在一起）。
            //
            // 14.0 而不是 8.0：行高现在**精确等于标签块**（标题 + 说明），
            // 早前那 8pt 里其实混着控件溢出撑出来的空白，看起来才够松。
            // 行高收紧后必须把间距补回来，否则相邻两行贴在一起。
            ui.spacing_mut().item_spacing = vec2(0.0, 14.0);

            // 顶部行：无标题文字，仅当有项偏离默认值时在**右上方**出现「重置」。
            let changed_any = {
                let cfg = &f.svc.state.config;
                card.items.iter().any(|it| it.is_changed(cfg, f.ws))
            };
            if changed_any {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // 带按钮背景：与「检查更新」按钮（`settings_rows.rs`）
                        // 同一画法——`surface_variant` 填充 + `text` 文字色。
                        //
                        // CAUTION: 早前这里是 `.frame(false)`（纯文字、无背景），
                        // 在卡片标题行里读起来像一段说明文字而不是可点的控件：
                        // 用户反馈「重置按钮没有按钮背景」。`.frame(false)` 只该
                        // 用于「刻意做成文字链接」的场合（如工具栏图标按钮），
                        // 一个会清掉整卡设置的动作不该长成那样。
                        //
                        // 文字色从 `text_dim` 提到 `text`：有了填充之后再用次要
                        // 文字色，在浅色主题（surface_variant #ECEFF3）上对比度
                        // 偏低，且与「检查更新」不一致。
                        let resp = ui.add(
                            egui::Button::new(
                                egui::RichText::new(t(("重置", "Reset")))
                                    .size(pal.font_sm)
                                    .color(pal.text),
                            )
                            .fill(pal.surface_variant)
                            .corner_radius(pal.radius_md),
                        );
                        let rect = resp.rect;
                        let clicked = resp.clicked();
                        resp.on_hover_text(t(("恢复该卡片默认值", "Restore this card to defaults")));
                        f.hit_insert(card.cat.reset_key(), rect);
                        if clicked {
                            for it in card.items {
                                if let Some(op) = it.reset_op() {
                                    f.state.push(op);
                                }
                            }
                            // 通信卡片重置 = 整卡内容回到默认：MCP 总开关 +
                            // 8 个可调用能力全部恢复开启。
                            if card.cat == SettingsCat::Comm {
                                for cap in MCP_CAPS {
                                    f.state.push(Op::SetBool {
                                        group: ConfigGroup::Mcp,
                                        field: cap.field,
                                        value: true,
                                    });
                                }
                            }
                            // 该卡片全部项从「最近修改」里移除。
                            f.state
                                .settings_recent
                                .retain(|(l, _)| !card.items.iter().any(|it| it.key == *l));
                        }
                    });
                });
            }

            for it in card.items {
                settings_item_row(f, ui, it);
            }

            // 通信卡片底部：「一键 AI 配对提示词」按钮。
            //
            // 点击后由 `App::apply_ops` 把按当前 MCP 配置动态拼接的接入说明
            // 复制到系统剪贴板（写剪贴板带自身捕获抑制，见 `copy_mcp_pair_prompt`）。
            if card.cat == SettingsCat::Comm {
                ui.add_space(4.0);
                let btn_w = ui.available_width();
                // 用 allocate 推进游标再自绘（纯平风格，不用默认 Button 边框）。
                let (bb, resp) = ui.allocate_exact_size(vec2(btn_w, 36.0), egui::Sense::click());
                let p = ui.painter_at(bb);
                let hovered = resp.hovered();
                p.rect_filled(bb, pal.radius_md, if hovered { pal.accent_hover } else { pal.accent });
                p.text(
                    bb.center(),
                    egui::Align2::CENTER_CENTER,
                    t(("一键 AI 配对提示词", "Copy AI pairing prompt")),
                    egui::FontId::proportional(pal.font_sm),
                    pal.text_on_accent,
                );
                f.hit_insert("settings_mcp_pair", bb);
                if resp.clicked() {
                    f.state.push(Op::CopyMcpPairPrompt);
                }
            }
        });
}

/// 卡片阴影的参数行（滑条 + 右侧读数），标签是运行时文本。
///
/// 布局与 [`setting_row`] 一致（标签左、控件右），但 `setting_row` 的
/// 标签/描述是 `&'static str`，这里「模糊」「浓度」这类小标签不值得
/// 进静态表，就单独写一个接受 `&str` 的变体。
///
/// `id_suffix` 必须稳定唯一（`draw_slider` 的 id 依赖它）。
pub(super) fn shadow_param_row(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    label: &str,
    readout: String,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 与 `setting_row` 同款：先把行高定成标签行高，滑条与读数才会
            // 与标签在同一垂直中心上（否则滑条贴着行首，比文字高半行）。
            let row_h = head_line_height(ui, label).max(CTRL_MIN_H);
            let avail = ui.available_width();
            ui.allocate_ui_with_layout(
                vec2(avail, row_h),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    // 读数：与 `ItemKind::Opacity` 同一习惯，数字显示在滑条右侧。
                    ui.add(
                        egui::Label::new(egui::RichText::new(readout).color(f.pal.text_dim)),
                    );
                    changed = draw_slider(f, ui, id_suffix, "", v, range, true, 0.0);
                    // 剩余宽度给标签列（与 `setting_row` 同款布局，见其注释）。
                    // 高度同样必须是 `row_h`——0 会让内容溢出撑高整行。
                    let rest = (ui.available_width() - ui.spacing().item_spacing.x).max(0.0);
                    ui.allocate_ui_with_layout(
                        vec2(rest, row_h),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.add(egui::Label::new(label).truncate());
                        },
                    );
                },
            );
        });
    });
    changed
}

/// 右下角信息组合编辑器：三个 chip（图标 / 类型 / 大小）。
///
/// - **单击** chip：切换该元素显隐（亮 = 显示，灰 = 隐藏）；
/// - **按住拖动**：按鼠标落点重排前后顺序（每帧直接按鼠标位置算出
///   目标索引，结果只依赖鼠标位置、天然幂等，不会在相邻位置抖动）。
///
/// 返回 true 表示配置有变化（调用方 push `Op::SetRowMeta`）。
pub(super) fn draw_row_meta_editor(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    let mut changed = false;
    let pal = f.pal;
    let cfg = f.svc.state.config.ui.row_meta;
    let font = egui::FontId::proportional(pal.font_sm);
    let chip_h = pal.font_sm + 9.0;
    let chip_pad_x = 11.0;
    let chip_radius = 7.0;
    let chip_gap = 6.0;

    // 拖拽态跨帧记忆：当前正被拖动的元素。
    let drag_id = ui.scope_id().with("row_meta_drag");
    let dragged: Option<RowMetaPart> = ui.data(|d| d.get_temp(drag_id));

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = chip_gap;
        // 先布局所有 chip 并登记交互、收集 rect（拖拽时需要彼此的位置）。
        let mut cells: Vec<(RowMetaPart, Rect, egui::Response)> = Vec::with_capacity(3);
        for &part in cfg.order.iter() {
            let w = crate::text::measure_text(ui, t(part.label()), &font) + chip_pad_x * 2.0;
            let (rect, _) = ui.allocate_exact_size(vec2(w, chip_h), egui::Sense::hover());
            let id = ui.scope_id().with(("rowmeta_chip", part.label()));
            let resp = ui.interact(rect, id, egui::Sense::click_and_drag());
            cells.push((part, rect, resp));
        }
        let painter = ui.painter();
        let mut next = cfg;
        for (part, rect, resp) in &cells {
            let part = *part;
            // 登记热区（测试用 hit 名定位；与编辑器交互 id 区分）。
            let hit_key = match part {
                RowMetaPart::Icon => "rowmeta_chip_icon",
                RowMetaPart::Type => "rowmeta_chip_type",
                RowMetaPart::Size => "rowmeta_chip_size",
            };
            f.hit_insert(hit_key, *rect);
            let on = cfg.visible(part);
            let is_dragged = dragged == Some(part);
            // 隐藏态：底色与文字整体降对比（灰）。
            let bg = if on {
                pal.surface_variant
            } else {
                pal.surface_variant.gamma_multiply(0.4)
            };
            let fg = if on {
                pal.text_dim
            } else {
                pal.text_dim.gamma_multiply(0.38)
            };
            painter.rect_filled(*rect, chip_radius, bg);
            if is_dragged {
                // 拖动中给一个 accent 描边，明确「正在拿的是哪个」。
                painter.rect_stroke(
                    *rect,
                    chip_radius,
                    egui::Stroke::new(1.5, pal.accent),
                    egui::StrokeKind::Inside,
                );
            }
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                t(part.label()),
                font.clone(),
                fg,
            );

            // 单击切换显隐（发生过拖动时 clicked() 不会触发）。
            if resp.clicked() {
                next.toggle(part);
                changed = true;
            }
            if resp.drag_started() {
                ui.data_mut(|d| d.insert_temp(drag_id, part));
            }
            // 拖动重排：鼠标左侧有几个「其他」chip，就插到第几。
            if is_dragged
                && let Some(ptr) = resp.interact_pointer_pos()
            {
                let n_left = cells
                    .iter()
                    .filter(|(p, r, _)| *p != part && r.center().x < ptr.x)
                    .count();
                let cur = next.order.iter().position(|p| *p == part).unwrap_or(0);
                if cur != n_left {
                    next.move_to_index(part, n_left);
                    changed = true;
                }
            }
            if resp.drag_stopped() {
                ui.data_mut(|d| d.remove::<RowMetaPart>(drag_id));
            }
        }
        if changed {
            f.state.push(Op::SetRowMeta(next));
        }
    });
    changed
}
