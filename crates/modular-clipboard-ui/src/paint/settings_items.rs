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
use modular_clipboard_core::i18n::Msg;
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
use modular_clipboard_core::i18n::msg;

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
    pub(super) unit: Msg,
    /// 英文单数形态（`unit.1` 的单数，如 `" item"`）——复数选形用。
    /// 不进译文表：英文是主语言，它本身就是英文。
    pub(super) unit_one: &'static str,
    /// 存储值 → 显示值。
    pub(super) display: fn(f64) -> f64,
    /// 显示值 → 存储值。
    pub(super) store: fn(f64) -> f64,
}

/// 可选数值项规格（`None` = 不限）。
pub(super) struct OptNumberSpec {
    pub(super) default: f64,
    pub(super) step: f64,
    pub(super) unit: Msg,
    /// 英文单数形态，同 [`NumberSpec::unit_one`]。
    pub(super) unit_one: &'static str,
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
    unit: " ms",
    unit_one: " ms",
    display: identity_num,
    store: identity_num,
};
/// 迷你面板显示条数：1 ~ 10，步进 1。
pub(super) const NUMBER_MINI_COUNT: NumberSpec = NumberSpec {
    min: 1.0,
    max: 10.0,
    step: 1.0,
    unit: msg(" items"),
    unit_one: " item",
    display: |v| v,
    store: |v| v,
};

pub(super) const NUMBER_MINI_PREVIEW: NumberSpec = NumberSpec {
    min: 10.0,
    max: 100.0,
    step: 4.0,
    unit: msg(" chars"),
    unit_one: " char",
    display: |v| v,
    store: |v| v,
};

pub(super) const NUMBER_MAX_PAYLOAD_MB: NumberSpec = NumberSpec {
    min: 1.0,
    max: 4096.0,
    step: 1.0,
    unit: " MB",
    unit_one: " MB",
    display: bytes_to_mb,
    store: mb_to_bytes,
};
pub(super) const OPTN_MAX_ITEMS: OptNumberSpec = OptNumberSpec {
    default: 1000.0,
    step: 100.0,
    unit: msg(" entries"),
    unit_one: " entry",
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
    /// GPU 适配器选择（下拉框，**重启生效**）。
    ///
    /// 独立成变体而不是 `Text`：候选值来自运行时枚举（`enumerate_adapters`），
    /// 不是静态列表；且要显示「当前正在用哪张卡」。
    GpuAdapter,
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
    /// 背景效果单选（无 / 磨砂 / 半透明 / 玻璃）。
    Backdrop,
    /// 玻璃模式的自有模糊档位（1 ~ 10，仅玻璃模式可用）。
    ///
    /// 与 [`ItemKind::BlurStrength`] 分开：后者只服务磨砂（玻璃模式下
    /// 禁用），而玻璃的光学合成需要不同的模糊量。共用一项会让用户
    /// 「想调玻璃模糊时唯一相关的滑块是死的」（2026-10-24 反馈）。
    GlassBlur,
    /// 液态玻璃的折射强度（0 ~ 1，仅玻璃模式可用）。
    GlassRefract,
    /// 液态玻璃的色散差异量（0 ~ 1，0 = 无彩边）。
    GlassDispersion,
    /// 液态玻璃的 Fresnel 幂次（1 ~ 12，越小光晕越铺开）。
    GlassEdge,
    /// 背景不透明度滑块。
    Opacity,
    /// 卡片不透明度滑块（磨砂 / 半透明下让行卡片也透出模糊底图）。
    CardOpacity,
    /// 卡片磨砂档位滑块（0 ~ 10，0 = 关闭）。
    ///
    /// 与 [`ItemKind::BlurStrength`] 不同：后者是**窗口背景层**的模糊，
    /// 这一项给**每张行卡片**单独铺一层模糊的身后内容。
    CardBlur,
    /// 磨砂强度档位滑块（1 ~ 10，仅磨砂模式有意义）。
    BlurStrength,
    /// 抓屏分辨率百分比滑块（15 ~ 100，磨砂与玻璃共用）。
    ///
    /// 独立成变体而不是 `Number`：它写的是 `u8` 百分比，且上下限与
    /// 渲染层的 `CAPTURE_PCT_RANGE` 同源（越界会被渲染层再钳一次）。
    BackdropScale,
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
        ph: Msg,
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
pub const FONT_SCALE_PRESETS: &[(u8, Msg)] = &[
    (85, msg("Small")),
    (100, msg("Standard")),
    (115, msg("Large")),
    (130, msg("X-Large")),
    (150, msg("XX-Large")),
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
    pub(super) name: Msg,
    pub(super) desc: Msg,
    pub(super) ready: bool,
}

/// 全部 MCP 可调用能力（与 `config::McpConfig` 字段一一对应）。
pub(super) static MCP_CAPS: &[McpCap] = &[
    McpCap { field: "resize_window", name: msg("Resize windows"), desc: msg("Resize and move any window"), ready: true },
    McpCap { field: "read_layout", name: msg("Read UI layout"), desc: msg("Read geometry and hierarchy of UI elements"), ready: true },
    McpCap { field: "click_element", name: msg("Click UI elements"), desc: msg("Click or activate elements anywhere"), ready: true },
    McpCap { field: "type_text", name: msg("Type text"), desc: msg("Type text and shortcuts into the focused control"), ready: true },
    McpCap { field: "scroll_view", name: msg("Scroll views"), desc: msg("Scroll pages and lists to a target position"), ready: true },
    McpCap { field: "screen_capture", name: msg("Screen capture"), desc: msg("Capture the screen or a window"), ready: true },
    McpCap { field: "window_focus", name: msg("Focus windows"), desc: msg("Activate a given window or app"), ready: false },
    McpCap { field: "drag_drop", name: msg("Drag and drop"), desc: msg("Drag elements or perform drag-and-drop"), ready: false },
    McpCap { field: "key_press", name: msg("Keystrokes"), desc: msg("Press key combos and function keys (shortcuts, confirm/cancel)"), ready: true },
    McpCap { field: "clipboard", name: msg("Clipboard access"), desc: msg("Read or write system clipboard text"), ready: true },
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
    let mut s = String::from(t("【modular-clipboard · MCP pairing】\nmodular-clipboard exposes a Model Context Protocol (MCP) service that can run UI automation on this machine.\n\nAvailable tools (10):\n"));
    for cap in MCP_CAPS {
        s.push_str(&format!("- {}：{}\n", t(cap.name), t(cap.desc)));
    }

    // 当前 exe 完整路径（stdio 配置里 command 用）；取不到时用占位。
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| t("<full path to modular-clipboard.exe>").into());

    s.push_str(t("\n=== Option 1: stdio (local, recommended) ===\nFor local clients such as Claude Desktop / Cursor, add this to your MCP config:\n"));
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
        t("None").to_string()
    } else {
        t("Bearer token (configured in settings)").to_string()
    };
    s.push_str(
        &t("\n=== Option 2: HTTP (remote / custom address) ===\nService URL: http://{host}:{port}/mcp\nAuth: {auth}\nConfig:\n")
        .replace("{host}", &host)
        .replace("{port}", &port.to_string())
        .replace("{auth}", &auth),
    );
    s.push_str(&format!(
        "{{\n  \"mcpServers\": {{\n    \"modular-clipboard\": {{\n      \"url\": \"http://{host}:{port}/mcp\"\n    }}\n  }}\n}}\n"
    ));
    s.push_str(t("Start the HTTP service: modular-clipboard.exe --mcp-http\n"));
    s
}

/// 一个设置项。
///
/// `key` 是**稳定标识**（搜索「最近修改」的去重键、复位时的匹配键）：
/// 文案会随界面语言变，不能当身份用（见 [`crate::i18n`] 模块注释）。
pub(super) struct SettingsItem {
    pub(super) key: &'static str,
    pub(super) label: Msg,
    pub(super) desc: Msg,
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
            ItemKind::GlassBlur => {
                cfg.ui.window_backdrop == WindowBackdrop::Glass && cfg.ui.glass_blur != d.ui.glass_blur
            }
            ItemKind::GlassRefract => {
                cfg.ui.window_backdrop == WindowBackdrop::Glass
                    && (cfg.ui.glass_refract - d.ui.glass_refract).abs() > 1e-6
            }
            ItemKind::GlassDispersion => {
                cfg.ui.window_backdrop == WindowBackdrop::Glass
                    && (cfg.ui.glass_dispersion - d.ui.glass_dispersion).abs() > 1e-6
            }
            ItemKind::GlassEdge => {
                cfg.ui.window_backdrop == WindowBackdrop::Glass
                    && (cfg.ui.glass_edge - d.ui.glass_edge).abs() > 1e-6
            }
            ItemKind::Opacity => (cfg.ui.window_opacity - 1.0).abs() > f32::EPSILON,
            ItemKind::CardOpacity => (cfg.ui.card_opacity - 1.0).abs() > f32::EPSILON,
            ItemKind::CardBlur => cfg.ui.card_blur != d.ui.card_blur,
            ItemKind::BlurStrength => cfg.ui.window_blur_strength != d.ui.window_blur_strength,
            // 只在真的抓屏时才算「偏离默认」：背景效果为「无」时这一项
            // 不参与任何渲染，把它的默认值当成脏状态会让整张卡一直挂着
            // 「重置」——而用户点下去什么都不会变。
            ItemKind::BackdropScale => {
                cfg.ui.window_backdrop != WindowBackdrop::None
                    && cfg.ui.backdrop_scale != d.ui.backdrop_scale
            }
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
            ItemKind::GpuAdapter => cfg.ui.gpu_adapter != d.ui.gpu_adapter,
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
            ItemKind::GpuAdapter => Some(Op::SetGpuAdapter(d.ui.gpu_adapter.clone())),
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
            ItemKind::CardBlur => Some(Op::SetCardBlur(d.ui.card_blur)),
            ItemKind::BlurStrength => Some(Op::SetBlurStrength(d.ui.window_blur_strength)),
            ItemKind::BackdropScale => Some(Op::SetBackdropScale(d.ui.backdrop_scale)),
            // 四个玻璃参数共享一个 Op：重置其中任意一个，其余三个一并
            // 归位。`reset_op` 拿不到当前配置（只有默认值 `d`），而
            // 「重置这一项」的语义本就是「这张卡回到默认」——四个都是
            // 这张卡的项，一起归位比「只归一个、其余停在用户值」更符合
            // 卡片级重置按钮的承诺（见 `settings_card` 的重置分支）。
            ItemKind::GlassBlur
            | ItemKind::GlassRefract
            | ItemKind::GlassDispersion
            | ItemKind::GlassEdge => Some(Op::SetGlass {
                refract: d.ui.glass_refract,
                dispersion: d.ui.glass_dispersion,
                edge: d.ui.glass_edge,
                blur: d.ui.glass_blur,
            }),
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
    pub(super) title: Msg,
    /// 条件卡：谓词返回 `false` 时整卡不画。
    ///
    /// 2026-10-25 用户反馈「磨砂强度跟随背景效果磨砂隐藏，而不是不可
    /// 选择」的卡片级推广：液态玻璃的四项在非玻璃背景下曾是「默认
    /// 折叠的一组禁用滑条」，现在整卡按背景效果显隐——不可用的设置
    /// 不该占据版面。
    pub(super) cond: Option<fn(&modular_clipboard_core::config::Config) -> bool>,
    pub(super) items: &'static [SettingsItem],
}

/// 当前配置下该项是否**整项不画**（依赖项不可用 ⇒ 隐藏，不是置灰）。
///
/// 2026-10-25 用户反馈：「磨砂强度跟随背景效果磨砂隐藏，而不是不可
/// 选择」「背景不透明度在背景效果为无时不可调节」。禁用态（压暗但
/// 仍可点）会让用户以为参数没保存；隐藏则让「不可用」与「不存在」
/// 一致，版面也只呈现与当前模式相关的内容。
///
/// 卡片级条件（液态玻璃整卡）走 [`SettingsCard::cond`]，不在这里。
///
/// CAUTION: 复合项（类型过滤栏 / 卡片阴影 / MCP 总开关行）的子参数
/// 由各自的渲染臂内部按开关收放（`if cfg.visible { … }`），不走本
/// 函数——它们的子行不是独立 `SettingsItem`。
pub(super) fn item_hidden(
    it: &SettingsItem,
    cfg: &modular_clipboard_core::config::Config,
) -> bool {
    use modular_clipboard_core::config::WindowBackdrop;
    match it.key {
        // 磨砂强度：仅磨砂模式有意义（半透明 / 无背景 / 玻璃都不用它）。
        "ui.blur_strength" => cfg.ui.window_backdrop != WindowBackdrop::Acrylic,
        // 背景不透明度：背景效果为「无」时窗口底是不透明的，调节无效。
        "ui.window_opacity" => cfg.ui.window_backdrop == WindowBackdrop::None,
        // 抓屏分辨率：背景效果为「无」时压根不抓屏（`paint_backdrop` 不跑），
        // 这一项没有任何作用——按「不可用即不存在」隐藏而不是压暗。
        "ui.backdrop_scale" => cfg.ui.window_backdrop == WindowBackdrop::None,
        // 卡片不透明度**恒显示**（2026-10-28 修「卡片透明度失效」）。
        //
        // 早前在「无背景效果且未开卡片磨砂」时整行隐藏，理由是「没有
        // 可透的底图，滑块无从表现」。这个理由只对了一半：卡片半透明
        // 还会与**窗口底色**混合（卡片变淡），那本身就是用户要的效果。
        // 隐藏的后果是用户配了 `card_opacity=0.6` 却永远看不到变化。
        // 卡片磨砂：玻璃模式下**整项隐藏**（2026-10-25）。玻璃 pass 用
        // 双底图折射充当卡片的身后内容（`row.rs` 跳过不透明补丁），每张
        // 卡片各自的 CPU 模糊补丁不再绘制、捕获链也停了——这一项在玻璃
        // 模式下是死的，铺在设置里就是「看得见但拖了没反应」。
        "ui.card_blur" => cfg.ui.window_backdrop == WindowBackdrop::Glass,
        // 风格参数：二级主题选「无」时没有任何旋钮。
        "ui.secondary_theme_params" => cfg.ui.secondary_theme.knobs().is_empty(),
        // 迷你面板子项：总开关关闭时整组隐藏（总开关行恒显示）。
        "mini_panel.count" | "mini_panel.preview_chars" | "mini_panel.ctrl"
        | "mini_panel.shift" | "mini_panel.alt" | "mini_panel.win" => {
            !cfg.mini_panel.enabled
        }
        // MCP 连接子项：总开关关闭时隐藏（总开关行恒显示）。
        "mcp.host" | "mcp.port" | "mcp.token" => !cfg.mcp.enabled,
        _ => false,
    }
}

/// 默认折叠的分组 key。
///
/// 2026-10-25 改版后为**空**：唯一需要默认折叠的「液态玻璃」组已改成
/// **条件卡**（背景效果 ≠ 玻璃时整卡不画，见 [`SettingsCard::cond`]），
/// 不再需要「铺出来再收起来」。保留机制本身——组折叠仍由用户点击驱动，
/// 只是没有初始快照了。
pub(super) const DEFAULT_FOLDED: &[&str] = &[];

/// 把 [`DEFAULT_FOLDED`] 灌进折叠快照（只做一次）。
///
/// # 为什么要 seed 而不是每次判断时异或
///
/// 异或方案下「集合里有 key」的含义随组而反（默认折叠的组里有 key
/// = 展开，默认展开的组里有 key = 折叠），点击分支因此必须分情况写。
/// 漏了分支的后果是**默认折叠的组永远展不开**：点它走 `remove`，而它
/// 本就不在集合里（no-op），下一帧异或回来还是折叠。2026-10-24 用户
/// 反馈「液态玻璃无法展开」正是这个。
///
/// seed 之后语义统一：有 key = 折叠，点击即切换（insert ↔ remove）。
fn seed_default_folds(f: &mut Frame<'_>) {
    if f.state.settings_folds_seeded {
        return;
    }
    f.state.settings_folds_seeded = true;
    for k in DEFAULT_FOLDED {
        f.state.settings_folded.insert((*k).to_string());
    }
}

/// 全部分类卡片（导航顺序与 `SettingsCat::ALL` 一致）。
pub(super) static SETTINGS_CARDS: &[SettingsCard] = &[
    // ====================================================================
    // 常规（2026-10-25 重组：原「General」一张卡按功能相关性拆成两张）
    //
    // 启动行为与外部程序是两类关注点：混在一张卡里要靠组标题折叠才能
    // 分开，不如直接分成两张平级卡——「窗口与启动」在前（开机后的
    // 运行方式与主窗口行为），「外部程序」在后（两类路径覆盖）。
    // ====================================================================
    SettingsCard {
        cat: SettingsCat::General,
        title: msg("Window and startup"),
        cond: None,
        items: &[
            // 界面语言排第一：它决定用户能不能读懂后面所有项。
            SettingsItem {
                key: "ui.language",
                label: msg("Interface language"),
                desc: msg("Display language: follow the system, or pin one from the list"),
                kind: ItemKind::Language,
            },
            // 启动三件套：自启动 → 启动时最小化 → 托盘图标，
            // 按「登录后发生什么」的时间顺序排。
            SettingsItem {
                key: "ui.autostart",
                label: msg("Launch at startup"),
                desc: msg("Start recording in the background at sign-in; open the window from the tray"),
                kind: ItemKind::Autostart,
            },
            SettingsItem {
                key: "ui.start_minimized",
                label: msg("Start minimized"),
                desc: msg("Start hidden in the tray instead of opening the window"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "start_minimized"),
            },
            SettingsItem {
                key: "ui.show_tray",
                label: msg("Show tray icon"),
                desc: msg("Show an icon and menu in the system tray while running"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "show_tray"),
            },
            // 窗口行为两件套。
            SettingsItem {
                key: "ui.always_on_top",
                label: msg("Always on top"),
                desc: msg("Keep the main window above other windows"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "always_on_top"),
            },
            SettingsItem {
                key: "ui.hide_on_focus_lost",
                label: msg("Hide on focus loss"),
                desc: msg("Hide the main window when another app gains focus"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "hide_on_focus_lost"),
            },
            SettingsItem {
                key: "ui.pinned_mode",
                label: msg("Pinned layout"),
                desc: msg("Pin entries in the same list, or in a separate draggable column"),
                kind: ItemKind::PinnedMode,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::General,
        title: msg("External programs"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.browser_path",
                label: msg("Browser path"),
                desc: msg("App used to open link entries; empty = system default browser"),
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "browser_path",
                    ph: msg("Default (system browser)"),
                    secret: false,
                },
            },
            SettingsItem {
                key: "ui.explorer_path",
                label: msg("File manager path"),
                desc: msg("App used to open file entries; empty = system file manager"),
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "explorer_path",
                    ph: msg("Default (explorer.exe)"),
                    secret: false,
                },
            },
        ],
    },
    // ====================================================================
    // 外观（2026-10-25 重组：原独立「主题」分类并入「外观」）
    //
    // 明暗模式 / 主题色 / 风格层本就是外观的关注点，单独占一个导航位
    // 只会把「外观」拆成两半。合并后导航 8 → 7：常规 / 外观 / 捕获 /
    // 存储 / 快捷键 / 通信 / 关于。
    //
    // 外观内六张卡按「从全局到局部」排列：
    //   主题（我是谁）→ 文字（读什么）→ 窗口背景（站在什么前面）
    //   → 液态玻璃（背景的一种）→ 行卡片（每一条长什么样）
    //   → 悬浮面板（辅助层）。
    // ====================================================================
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Theme"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.dark_mode",
                label: msg("Light / dark mode"),
                desc: msg("Follow the system, or force dark / light"),
                kind: ItemKind::Theme,
            },
            SettingsItem {
                key: "ui.accent",
                label: msg("Accent color"),
                desc: msg("Accent color: pick from swatches, sliders, or a hex value"),
                kind: ItemKind::Accent,
            },
            SettingsItem {
                key: "ui.secondary_theme",
                label: msg("Style theme"),
                desc: msg("A style layer on top of light / dark: paper, eye-care green, starfield purple, OLED"),
                kind: ItemKind::SecondaryTheme,
            },
            // 风格参数：二级主题选「无」时整项隐藏（item_hidden）。
            SettingsItem {
                key: "ui.secondary_theme_params",
                label: msg("Style parameters"),
                desc: msg("Intensity and strength of the current style theme"),
                kind: ItemKind::ThemeParams,
            },
            // 令牌覆盖入口：二级视图占据大半个内容区，放在主题卡末行。
            SettingsItem {
                key: "ui.token_overrides",
                label: msg("Token overrides"),
                desc: msg("Override individual theme tokens: any element, any value"),
                kind: ItemKind::TokenOverrides,
            },
        ],
    },
    // 「文字」卡：字号与条目内容的呈现（从原外观卡拆出）。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Text"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.font_scale",
                label: msg("Font scale"),
                desc: msg("Text size; presets apply instantly"),
                kind: ItemKind::FontScale,
            },
            SettingsItem {
                key: "ui.image_inline",
                label: msg("Image thumbnails"),
                desc: msg("Show image entries as thumbnails; off = size text only"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "image_inline"),
            },
            SettingsItem {
                key: "ui.md_rendered",
                label: msg("Render Markdown"),
                desc: msg("Lay out headings, lists and code blocks; off = monospaced source"),
                kind: ItemKind::Bool(ConfigGroup::Ui, "md_rendered"),
            },
            SettingsItem {
                key: "ui.row_meta",
                label: msg("Card meta badges"),
                desc: msg("Icon, type and size at the card's corner: click to toggle, drag to reorder"),
                kind: ItemKind::RowMeta,
            },
        ],
    },
    // 「显卡」卡：只有一项，但必须独立成卡。
    //
    // 它是**启动期**决定且**重启生效**的——混进「窗口背景」会让用户以为
    // 拖一下就能看到变化。放在背景卡之前，是因为玻璃/磨砂的观感全都建立
    // 在「用的是哪张卡」之上（核显跑 200% 超采样会掉帧）。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Graphics"),
        cond: None,
        items: &[SettingsItem {
            key: "ui.gpu_adapter",
            label: msg("GPU adapter"),
            desc: msg("Which graphics card renders the UI; takes effect after a restart"),
            kind: ItemKind::GpuAdapter,
        }],
    },
    // 「窗口背景」卡：背景效果 + 两个依赖项。
    //
    // 磨砂强度仅在磨砂模式出现、背景不透明度在「无」时隐藏
    // （item_hidden）——2026-10-25 用户反馈的「隐藏而不是不可选择」。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Window background"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.window_backdrop",
                label: msg("Background effect"),
                desc: msg("None, frosted blur, translucent, or liquid glass"),
                kind: ItemKind::Backdrop,
            },
            SettingsItem {
                key: "ui.blur_strength",
                label: msg("Blur strength"),
                desc: msg("Blur intensity in frosted mode"),
                kind: ItemKind::BlurStrength,
            },
            SettingsItem {
                key: "ui.window_opacity",
                label: msg("Background opacity"),
                desc: msg("Transparency of the background layer; text stays opaque"),
                kind: ItemKind::Opacity,
            },
            SettingsItem {
                key: "ui.backdrop_scale",
                label: msg("Capture resolution"),
                desc: msg("Percentage of the window size to capture; 100% matches screen pixels exactly, lower costs less"),
                kind: ItemKind::BackdropScale,
            },
        ],
    },
    // 液态玻璃参数卡：**条件卡**——背景效果 = 玻璃时才出现。
    //
    // 2026-10-25 改版：从「外观卡里默认折叠的一组」升级为独立条件卡。
    // 两个理由：
    // 1. 它们只在玻璃模式下有意义，混在外观卡里会被误当成通用外观项；
    // 2. 折叠组仍会占一行标题 + 折叠箭头，条件卡在不该出现时**完全不占
    //    版面」——「不可用即不存在」比「不可用但可见」更诚实。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Liquid glass"),
        cond: Some(|cfg| cfg.ui.window_backdrop == WindowBackdrop::Glass),
        items: &[
            SettingsItem {
                key: "ui.glass_blur",
                label: msg("Glass blur"),
                desc: msg("Blur strength in glass mode, independent of frosted blur"),
                kind: ItemKind::GlassBlur,
            },
            SettingsItem {
                key: "ui.glass_refract",
                label: msg("Glass refraction"),
                desc: msg("How strongly the glass bends what is behind it; 0 = no bending"),
                kind: ItemKind::GlassRefract,
            },
            SettingsItem {
                key: "ui.glass_dispersion",
                label: msg("Glass dispersion"),
                desc: msg("Color fringing at the edges: 0 = none, higher = more prismatic"),
                kind: ItemKind::GlassDispersion,
            },
            SettingsItem {
                key: "ui.glass_edge",
                label: msg("Glass rim light"),
                desc: msg("Fresnel highlight along the edges; lower spreads it out"),
                kind: ItemKind::GlassEdge,
            },
        ],
    },
    // 「行卡片」卡：每一条历史条目长什么样（从原外观卡拆出）。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Row cards"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.card_opacity",
                label: msg("Card opacity"),
                desc: msg("Opacity of entry cards; without a background effect they blend into the window fill"),
                kind: ItemKind::CardOpacity,
            },
            SettingsItem {
                key: "ui.card_blur",
                label: msg("Card blur"),
                desc: msg("Frosted glass for entry cards: 0 = off, higher = blurrier"),
                kind: ItemKind::CardBlur,
            },
            SettingsItem {
                key: "ui.card_shadow",
                label: msg("Card shadow"),
                desc: msg("Drop shadow under entry cards; dark mode deepens it automatically"),
                kind: ItemKind::CardShadow,
            },
        ],
    },
    // 「悬浮面板」卡：类型栏 / 搜索面板 / 通知（从原外观卡拆出）。
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: msg("Panels and popups"),
        cond: None,
        items: &[
            SettingsItem {
                key: "ui.type_rail",
                label: msg("Type filter bar"),
                desc: msg("Floating type buttons at the window edge: click to filter; multi-select, adjustable position and size"),
                kind: ItemKind::TypeRail,
            },
            SettingsItem {
                key: "ui.search_panel",
                label: msg("Search panel"),
                desc: msg("Floating search panel; dock position and height adjustable"),
                kind: ItemKind::SearchPanel,
            },
            SettingsItem {
                key: "ui.notice_popup",
                label: msg("Notifications"),
                desc: msg("How feedback appears: in-window toasts or a separate popup"),
                kind: ItemKind::NoticeMode,
            },
        ],
    },
    // ====================================================================
    // 捕获（2026-10-25 重组：原无标题的散项并入「记录行为」卡）
    // ====================================================================
    SettingsCard {
        cat: SettingsCat::Capture,
        title: msg("Recording behavior"),
        cond: None,
        items: &[
            SettingsItem {
                key: "capture.enabled",
                label: msg("Record clipboard"),
                desc: msg("Stop adding new entries when off"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "enabled"),
            },
            SettingsItem {
                key: "capture.skip_password_fields",
                label: msg("Skip password fields"),
                desc: msg("Skip clipboard content while passwords are being typed"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "skip_password_fields"),
            },
            SettingsItem {
                key: "capture.dedup",
                label: msg("Deduplicate copies"),
                desc: msg("Identical content is bumped to the top instead of duplicated"),
                kind: ItemKind::Bool(ConfigGroup::Capture, "dedup"),
            },
            SettingsItem {
                key: "capture.poll_interval_ms",
                label: msg("Polling interval"),
                desc: msg("How often the clipboard is checked; lower is faster but heavier"),
                kind: ItemKind::Number(ConfigGroup::Capture, "poll_interval_ms", &NUMBER_POLL_MS),
            },
        ],
    },
    // ====================================================================
    // 存储（2026-10-25 重组：原无标题的散项并入「容量与清理」卡）
    // ====================================================================
    SettingsCard {
        cat: SettingsCat::Storage,
        title: msg("Capacity and cleanup"),
        cond: None,
        items: &[
            SettingsItem {
                key: "storage.max_items",
                label: msg("Max entries"),
                desc: msg("How many entries to keep; empty = size-based eviction only"),
                kind: ItemKind::OptNumber(ConfigGroup::Storage, "max_items", &OPTN_MAX_ITEMS),
            },
            SettingsItem {
                key: "storage.max_payload_bytes",
                label: msg("Max entry size"),
                desc: msg("Clipboard content larger than this is not recorded"),
                kind: ItemKind::Number(ConfigGroup::Storage, "max_payload_bytes", &NUMBER_MAX_PAYLOAD_MB),
            },
            SettingsItem {
                key: "storage.cleanup_on_start",
                label: msg("Clean up temp files"),
                desc: msg("Remove temp files left over from the previous run on startup"),
                kind: ItemKind::Bool(ConfigGroup::Storage, "cleanup_on_start"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: msg("Shortcuts"),
        cond: None,
        items: &[
            SettingsItem {
                key: "hotkey.toggle",
                label: msg("Show / hide main window"),
                desc: msg("Toggle the main window from anywhere"),
                kind: ItemKind::Hotkey(HotkeySlot::Toggle),
            },
            SettingsItem {
                key: "hotkey.paste_previous",
                label: msg("Paste previous"),
                desc: msg("Paste the most recent entry into the focused app"),
                kind: ItemKind::Hotkey(HotkeySlot::PastePrevious),
            },
            SettingsItem {
                key: "hotkey.paste_previous_mode",
                label: msg("Paste mode"),
                desc: msg("'Copy & notify' only fills the clipboard; 'Copy & paste' also sends Ctrl+V to the target"),
                kind: ItemKind::PastePreviousMode,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: msg("Mini panel"),
        cond: None,
        items: &[
            SettingsItem {
                key: "mini_panel.enabled",
                label: msg("Enable"),
                desc: msg("Right-click with a modifier to pop up recent entries at the cursor"),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "enabled"),
            },
            // 以下六项在总开关关闭时整项隐藏（item_hidden）——
            // 2026-10-25「隐藏而不是不可选择」。
            SettingsItem {
                key: "mini_panel.count",
                label: msg("Entry count"),
                desc: msg("Number of recent entries shown in the panel"),
                kind: ItemKind::Number(ConfigGroup::MiniPanel, "count", &NUMBER_MINI_COUNT),
            },
            SettingsItem {
                key: "mini_panel.preview_chars",
                label: msg("Preview length"),
                desc: msg("Max chars per item (controls width)"),
                kind: ItemKind::Number(ConfigGroup::MiniPanel, "preview_chars", &NUMBER_MINI_PREVIEW),
            },
            SettingsItem {
                key: "mini_panel.ctrl",
                label: msg("Ctrl + right-click"),
                desc: msg("Trigger modifier; these can stack, keep at least one"),
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "ctrl"),
            },
            SettingsItem {
                key: "mini_panel.shift",
                label: msg("Shift + right-click"),
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "shift"),
            },
            SettingsItem {
                key: "mini_panel.alt",
                label: msg("Alt + right-click"),
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "alt"),
            },
            SettingsItem {
                key: "mini_panel.win",
                label: msg("Win + right-click"),
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "win"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Comm,
        title: msg("MCP service"),
        cond: None,
        items: &[
            SettingsItem {
                key: "mcp.enabled",
                label: msg("MCP service"),
                desc: msg("Turn off to disable all MCP calls"),
                kind: ItemKind::McpToggle,
            },
            // 监听地址 / 端口 / 密钥：总开关关闭时隐藏（item_hidden）。
            SettingsItem {
                key: "mcp.host",
                label: msg("Listen address"),
                desc: msg("Address the MCP service listens on"),
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "host", ph: "127.0.0.1", secret: false },
            },
            SettingsItem {
                key: "mcp.port",
                label: msg("Port"),
                desc: msg("Port for the MCP service; empty = disabled"),
                kind: ItemKind::McpPort,
            },
            SettingsItem {
                key: "mcp.token",
                label: msg("Access token"),
                desc: msg("Access token; empty = no authentication"),
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "token", ph: msg("None"), secret: true },
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::About,
        title: msg("About"),
        cond: None,
        items: &[
            SettingsItem {
                key: "about.group.app",
                label: msg("App info"),
                desc: "",
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.version",
                label: msg("Version"),
                desc: "",
                kind: ItemKind::Version,
            },
            SettingsItem {
                key: "about.update",
                label: msg("Check for updates"),
                desc: msg("Fetch the latest tag and compare with local"),
                kind: ItemKind::UpdateCheck,
            },
            SettingsItem {
                key: "about.license",
                label: msg("License"),
                desc: msg("Click to view the full text"),
                kind: ItemKind::LicenseView,
            },
            SettingsItem {
                key: "about.group.project",
                label: msg("Project"),
                desc: "",
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.repo",
                label: msg("Project page"),
                desc: msg("Source code, issue tracker and releases"),
                kind: ItemKind::Link("https://github.com/MCheng404/ModularClipboard"),
            },
            SettingsItem {
                key: "about.author",
                label: msg("Author page"),
                desc: msg("The developer's GitHub profile"),
                kind: ItemKind::Link("https://github.com/MCheng404"),
            },
            SettingsItem {
                key: "about.group.data",
                label: msg("Data"),
                desc: "",
                kind: ItemKind::Group,
            },
            SettingsItem {
                key: "about.data_dir",
                label: msg("Data directory"),
                desc: msg("Click to open in the file manager"),
                kind: ItemKind::OpenDataDir,
            },
            SettingsItem {
                key: "about.quit",
                label: msg("Quit app"),
                desc: msg("Quit the app and stop recording"),
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
        title: msg("Built with"),
        cond: None,
        items: &[SettingsItem {
            key: "about.stack",
            label: msg("Built with"),
            desc: msg("Click a name to open its repository"),
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
    // 先灌默认折叠：下面三条视图（搜索 / 最近 / 令牌面板）都可能提前
    // return，而折叠状态是跨视图共享的，必须在最前面 seed。
    seed_default_folds(f);
    let q = f.state.settings_query.trim().to_string();
    if !q.is_empty() {
        settings_search_results(f, ui, &q);
    } else if f.state.settings_show_recent {
        settings_recent_view(f, ui);
    } else if f.state.settings_cat == SettingsCat::Appearance && f.state.theme_tokens_expanded {
        // 二级视图（2026-10-19）：令牌面板占据整个右侧内容区——用户要求
        // 「基本覆盖设置窗口的一大半区域」。左导航与搜索框保持可见，
        // 返回按钮（面板左上角 ‹）关掉它。
        //
        // 2026-10-25：主题并入外观后，二级视图的宿主分类从 Theme 改为
        // Appearance（入口行在外观 → 主题卡末行）。
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
/// # 条件卡
///
/// `SettingsCard::cond` 为 `Some` 且谓词不满足时整卡跳过（液态玻璃
/// 参数卡只在背景效果 = 玻璃时出现，2026-10-25）。
///
/// 卡片之间留 14pt 间距：`Frame` 的圆角背景自带外缘，不留白的话
/// 两张卡片会贴在一起，看不出是两块。12 → 14 是 2026-10-25 松散
/// 节奏改版的一部分（悬浮卡的投影/光晕也需要这段呼吸空间）。
pub(super) fn settings_category_cards(f: &mut Frame<'_>, ui: &mut Ui) {
    // CAUTION: 必须先把 `settings_cat` **拷出来**再迭代。
    //
    // 闭包里读 `f.state.settings_cat` 会借住 `&f`，而循环体里
    // `settings_card(f, ...)` 要 `&mut f`——两者同一作用域直接撞
    // 借用检查（E0502）。`SettingsCat` 是 Copy 的小枚举，拷出来零成本。
    let cat = f.state.settings_cat;
    let mut first = true;
    for card in SETTINGS_CARDS.iter().filter(|c| c.cat == cat) {
        if let Some(cond) = card.cond
            && !cond(&f.svc.state.config)
        {
            continue;
        }
        if !first {
            ui.add_space(14.0);
        }
        first = false;
        settings_card(f, ui, card);
    }
}

/// 可折叠的分组标题行：`>` 形箭头 + 组名，点击切换折叠。
///
/// # 为什么自绘而不用 `egui::CollapsingHeader`
///
/// 项目全程自绘控件（纯平风格 + 统一的 `pal` 量纲），`CollapsingHeader`
/// 自带背景/边框/缩进，与本项目的行控件语言不一致。而且它的高度与
/// 字体缩放不走 `pal`，会破坏「与文字同屏的固定高度按字号算」的约定。
///
/// # 箭头是什么：圆角折线，不是字符也不是多边形
///
/// 2026-10-24 用户要求「`>` / `v` 是渲染出来的而不是字符，圆角设计」。
/// 所以它既不是 `painter.text` 画的字符，也不是 `convex_polygon` 的实心
/// 三角，而是**两段线 + 三个圆**拼出来的圆角折线（chevron）：
///
/// - 两段 `line_segment` 给出折线的身段；
/// - 转角与两个外端各一个半径 = 线宽/2 的实心圆，把直角转折和端头
///   一并圆掉。
///
/// egui 的 `Stroke` 只有 `width` 与 `color`（没有线帽字段），
/// `Shape::line` 由 tessellator 以**平头**端点生成——所以圆角必须靠
/// 端头补圆，没有「设个 cap 就行」的捷径。
///
/// # `<` 与 `v` 是同一个形状的两个角度
///
/// 折线按展开态（`v`，顶点在下）定义，再绕自身中心旋转 `φ`：
/// `φ = 0°` 是 `v`，`φ = -90°` 是 `<`。折叠状态只是旋转目标不同，
/// 于是「展开 ↔ 折叠」自然就是一段 90° 的旋转补间
/// （[`crate::anim::tween_f32`]，`Ease::InOutCubic`——与主题切换、
/// 面板开合同款缓动）。
///
/// CAUTION: 补间是**惰性**的，而设置窗口有 `RedrawGate`：静止帧整帧
/// 跳过。动画进行中若不 `request_repaint_after` 预约下一拍，转序会在
/// 第一帧后被闸门饿死，看起来就是「图标瞬间跳变、没有动画」
/// （项目记忆里「计时器住在 draw() 里必被渲染闸门饿死」同款）。
///
/// # 命中区
///
/// 整行都是点击区（不只箭头）：组标题行的目的是「收起来」，一个小
/// 箭头不是可靠的靶子。登记用组的稳定 key，与 `settings_folded` 一致。
pub(super) fn settings_group_header(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    it: &SettingsItem,
    folded: bool,
    count: usize,
) {
    let pal = f.pal;
    ui.add_space(8.0);
    let row_h = pal.control_height * 0.72;
    let (bb, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), egui::Sense::click());
    let painter = ui.painter_at(bb);
    if resp.hovered() {
        // 全圆角改版（2026-10-25）：radius_sm → 胶囊（半高）。
        painter.rect_filled(bb, row_h * 0.5, pal.row_hover);
    }
    let hovered = resp.hovered();
    let clicked = resp.clicked();
    if clicked {
        // 快照语义下切换是无条件的：折叠 ⇒ 移除（展开），展开 ⇒ 插入
        // （折叠）。默认折叠的组此刻「在集合里」，点一下即移除——
        // 早先的异或设计在这里分情况写，漏了默认折叠那一支，导致
        // 「液态玻璃无法展开」。
        if folded {
            f.state.settings_folded.remove(it.key);
        } else {
            f.state.settings_folded.insert(it.key.to_string());
        }
    }
    f.hit_insert(it.key, bb);
    if hovered {
        resp.on_hover_text(if folded {
            t("Expand")
        } else {
            t("Collapse")
        });
    }

    // 行垂直中心：组名、计数、箭头三者共用。
    let cy = bb.center().y;

    // ---- 组名 + 计数（左）----
    //
    // 用 galley 而不是 `painter.text`：计数要紧跟组名，必须知道组名的
    // 实际宽度（`text` 直接画、不返回尺寸）。
    let label_galley = painter.layout_no_wrap(
        t(it.label).to_string(),
        egui::FontId::proportional(pal.font_sm),
        pal.text_dim,
    );
    let label_pos = egui::pos2(bb.min.x + 2.0, cy - label_galley.size().y * 0.5);
    painter.add(egui::Shape::galley(label_pos, label_galley.clone(), pal.text_dim));
    // 计数跟在组名右侧：分组标题的信息顺序是「名字 + 有几项」，状态
    // 指示（箭头）单独占最右端，三者不混在一起。
    if count > 0 {
        painter.text(
            egui::pos2(label_pos.x + label_galley.size().x + 6.0, cy),
            egui::Align2::LEFT_CENTER,
            format!("{count}"),
            egui::FontId::proportional(pal.font_xs),
            pal.text_dim,
        );
    }

    // ---- 折叠箭头（紧贴文字右侧）----
    //
    // 位置：2026-10-24 用户两次clarify——「移动到文字内容右边」指的是
    // **紧挨着文字**，不是飞到行末。第一版放到了最右端，被反馈「不挨着
    // 文字」。所以箭头中心 = 文字块（组名 + 计数）的右缘 + 一个呼吸位，
    // 组名长短变化时箭头跟着走，永远贴在一起。
    let text_right = if count > 0 {
        // 计数用的是 font_xs，宽度单独量（galley 不重复用）。
        let cnt = painter.layout_no_wrap(
            format!("{count}"),
            egui::FontId::proportional(pal.font_xs),
            pal.text_dim,
        );
        label_pos.x + label_galley.size().x + 6.0 + cnt.size().x
    } else {
        label_pos.x + label_galley.size().x
    };
    let arrow_w = pal.font_sm * 0.52; // 轮廓横向占位（半宽 + 端头）
    let cx = text_right + 6.0 + arrow_w * 0.5;
    let cy = bb.center().y;

    // 展开态（v）的三个关键点：顶点在下、两端在上方左右。
    // 尺寸按字号算 ⇒ 随 `font_scale` 等比缩放（与其它控件同一量纲）。
    //
    // CAUTION: 0.26 / 0.22 是量出来的，不是拍脑袋。第一版用 0.42 / 0.34，
    // 箭头总高 12.6pt——占 28pt 行高的 45%，线宽还是字号的 13%，
    // 2026-10-24 用户反馈「渲染出来太大」。现在总高 ≈ 7.8pt（行高的
    // 28%）、线宽 ≈ 字号的 8.5%，与项目里其它线性图标（`icons/draw.rs`）
    // 同一量级。
    let arm_h = pal.font_sm * 0.26; // 顶点/端点的纵向半距
    let arm_w = pal.font_sm * 0.22; // 两端点的横向半距

    // ---- 动画：弹性过冲 + 两臂错峰伸缩 ----
    //
    // 用户要求「抛弃传统平移动画，用更加灵动有趣、更加复杂的方式」。
    // 所以不是一个角度的简单补间，而是四个通道各走各的：
    //
    // 1. **旋转**（0° ↔ -90°）：主通道，`OutBack` 过冲回弹——箭头会
    //    冲过目标角度再弹回来，像被轻轻拨了一下；
    // 2. **两臂伸缩**：折叠时两臂先向内收（像捏合）、展开时先张开，
    //    各比旋转晚 40ms / 早 40ms 起步（错峰），读出「先转向、再张合」
    //    的层次而不是整体一起动；
    // 3. **线宽呼吸**：转动中线宽略收、停稳时回到全粗，让快速运动
    //    的那一小段显得更轻；
    // 4. **颜色**：hover 提亮同步补间。
    //
    // 四条通道都从 `tween_f32` 取（惰性 + retarget 续接），互不干扰的
    // 关键是 **id 各自独立**。任一路动画中都要 `request_repaint_after`
    // 预约下一拍，否则 `RedrawGate` 会把后续帧饿死（见函数文档）。
    let target_deg = if folded { -90.0 } else { 0.0 };
    let ctx = f.ui.ctx();
    let dur = std::time::Duration::from_millis(220);
    let deg = crate::anim::tween_f32(
        ctx,
        egui::Id::unique(("group_fold_rot", it.key)),
        target_deg,
        dur,
        crate::anim::Ease::OutBack,
    );
    // 两臂伸缩量：折叠 = 收拢到 0.72×，展开 = 全张 1.0×。与旋转错峰：
    // 同一 id 后缀不同起点时长 ⇒ 天然错开（晚起步的那条 lag 出来。
    // 这里用「延迟」表达：目标值一样，但 t0 更晚——靠 id 上的时长差
    // 近似不出错峰，改用「两次 tween 目标不同 + 不同 ease」制造层次：
    // A 臂用 OutBack（弹着张开）、B 臂用 OutCubic（稳着跟上）。
    let reach_a = crate::anim::tween_f32(
        ctx,
        egui::Id::unique(("group_fold_a", it.key)),
        if folded { 0.72 } else { 1.0 },
        std::time::Duration::from_millis(if folded { 200 } else { 260 }),
        crate::anim::Ease::OutBack,
    );
    let reach_b = crate::anim::tween_f32(
        ctx,
        egui::Id::unique(("group_fold_b", it.key)),
        if folded { 0.86 } else { 1.0 },
        std::time::Duration::from_millis(if folded { 260 } else { 200 }),
        crate::anim::Ease::OutCubic,
    );
    // 线宽呼吸：运动中收细到 0.82×。
    let moving = ((deg - target_deg).abs() > 1.0)
        || ((reach_a - if folded { 0.72 } else { 1.0 }).abs() > 0.02)
        || ((reach_b - if folded { 0.86 } else { 1.0 }).abs() > 0.02);
    let breath = crate::anim::tween_f32(
        ctx,
        egui::Id::unique(("group_fold_w", it.key)),
        if moving { 0.82 } else { 1.0 },
        std::time::Duration::from_millis(140),
        crate::anim::Ease::OutCubic,
    );
    if moving {
        f.ui
            .ctx()
            .request_repaint_after(std::time::Duration::from_millis(16));
    }

    let (sin, cos) = deg.to_radians().sin_cos();
    let rot = |v: egui::Vec2| -> egui::Pos2 {
        egui::pos2(cx + v.x * cos - v.y * sin, cy + v.x * sin + v.y * cos)
    };
    // 线宽 ≈ 字号的 8.5%：与 `icons/draw.rs` 的线性图标同量级。下限 1.0
    // 防极小字号下细到看不见。乘 breath ⇒ 运动中收细。
    let stroke_w = (pal.font_sm * 0.085 * breath).max(0.9);
    // 颜色补间：hover 时提亮，与旋转同步（不然颜色先跳、形状后转，
    // 观感是两段独立的动画）。
    let target_color = if hovered { pal.text } else { pal.text_dim };
    let arrow_color = crate::anim::tween_color(
        f.ui.ctx(),
        egui::Id::unique(("group_fold_c", it.key)),
        target_color,
        std::time::Duration::from_millis(120),
    );
    // ---- 几何：展开态三个关键点，绕中心旋转 ----
    // 两臂独立伸缩：A/B 端各按自己的 reach 系数收放，顶点保持不动。
    // 折叠时臂收拢（0.72×）、展开时全张（1.0×）⇒ 「捏合/张开」的层次感。
    let apex = egui::vec2(0.0, arm_h);
    let arm_a = egui::vec2(-arm_w, -arm_h) * reach_a;
    let arm_b = egui::vec2(arm_w, -arm_h) * reach_b;
    let (p_apex, p_a, p_b) = (rot(apex), rot(arm_a), rot(arm_b));
    // 圆半径 = 线宽 × 0.55（略大于半宽）：让相邻形状充分重叠，避免
    // 两个 AA 边界相切处露出 1px 淡线。详见下面绘制段的说明。
    let r = stroke_w * 0.55;

    // ---- 绘制：两段线 + 三个圆（多凸形状叠加）----
    //
    // CAUTION: 这是**第三次重写**。前两版都走「单一解析轮廓」路线
    // （构造闭合多边形一次填充），连栽四次：两侧法线同公式导致外沿内沿
    // 颠倒、端头帽绕序错乱、转角弧半径把 √(cr²+r²) 算成 cr+r、轮廓自交。
    // 这条路线的复杂度（绕序 / 法线 / 切点 / 弧半径 / 凹多边形三角化）
    // 远超收益——每多一步就多一处能悄悄错的地方，而视觉上根本看不出
    // 解析弧与折线采样的差别。
    //
    // 现在改为**多凸形状叠加**：两段 line_segment + 三个 circle_filled。
    // 每个形状都是凸的、彼此独立，绘制顺序明确，从**结构上**不可能自交
    // ——不存在「绕序错了」这回事。圆角线帽与圆角转角由三个圆补出：
    //
    // - 端头圆（a、b）：半径略大于半线宽，视觉上就是标准圆角线帽
    //   （圆的一半在线条内部、一半在外部）；
    // - 转角圆（v）：盖住两段线在顶点的平头接缝，外角自然圆润；
    // - 半径取线宽的 0.55 倍（略大于 0.5）：让相邻形状**充分重叠**，
    //   各自 AA 边界不会在内部露出细缝。0.5 刚好相切时，两个 AA 边
    //   相邻处会出现 1px 的淡线。
    let parts = chevron_parts(p_a, p_apex, p_b, r);
    for seg in parts.segments {
        painter.line_segment(seg, egui::Stroke::new(stroke_w, arrow_color));
    }
    // 圆画在线段**之后**：盖住端头平头。
    for c in parts.caps {
        painter.circle_filled(c, parts.radius, arrow_color);
    }
    ui.add_space(4.0);
}

/// 圆角 chevron 的绘制部件：两段线 + 三个圆角圆。
///
/// # 为什么是「多个凸形状叠加」而不是「单一闭合轮廓」
///
/// 2026-10-24 前两版都走「构造闭合多边形一次填充」路线，连栽四次：
/// 两侧法线同公式（外沿内沿颠倒）、端头帽绕序错乱、转角弧半径把
/// √(cr²+r²) 算成 cr+r、轮廓自交。这条路线要同时对绕序 / 法线 /
/// 切点 / 弧半径 / 凹多边形三角化五件事，每多一步就多一处能悄悄错的
/// 地方，而视觉上根本看不出解析弧与折线采样的差别。
///
/// 多凸叠加没有这些坑：两段线是凸的、圆是凸的，绘制顺序明确，**结构上**
/// 不存在自交。圆角线帽与圆角转角由三个圆补出（半径略大于半线宽），
/// 这正是多数绘图库实现 round join + round cap 的做法。
///
/// # 字段语义
///
/// - `segments`：两段线（a→v、v→b），各自等宽、平头端点
/// - `caps`：三个圆（a、v、b），盖住平头端点与转角接缝
/// - `radius`：圆的半径（≥ 线宽/2，保证与线段充分重叠）
pub(super) struct ChevronParts {
    pub(super) segments: [[egui::Pos2; 2]; 2],
    pub(super) caps: [egui::Pos2; 3],
    pub(super) radius: f32,
}

/// 组装 chevron 的绘制部件。
///
/// 纯函数：只做坐标装配，不做任何几何推断（推断就是上两版出错的地方）。
pub(super) fn chevron_parts(
    a: egui::Pos2,
    v: egui::Pos2,
    b: egui::Pos2,
    radius: f32,
) -> ChevronParts {
    ChevronParts {
        segments: [[a, v], [v, b]],
        caps: [a, v, b],
        radius,
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
///
/// # 悬浮式样式（2026-10-25 改版，HTML 原型定稿）
///
/// - 圆角 `radius_lg`(10) → **`radius_xl`(16)**；
/// - 内边距 `symmetric(12, 6)` → **`symmetric(20, 13)`**（宽松节奏）；
/// - 行间距 14 → **18**；
/// - 描边恒 1pt：浅色 `border_subtle` 发丝边，深色**透明**（边由
///   主题色环表达）。恒 1pt 是因为 `Frame` 把 stroke 宽度计入布局，
///   两种主题必须同宽，否则切换主题时内容会跳动 1pt；
/// - 内容画完后补**投影 / 围绕**（`draw_settings_card_shadow`）。
///
/// # 为什么投影只能「内容之后补」
///
/// 卡片高度由内容决定——`Frame::show` 返回后才知道外矩形，投影没有
/// 「先于填充」的绘制机会（列表行可以：行高静态已知）。解法是把晕带
/// 拆成四条 1pt 环带，整体位于卡片矩形之外 ⇒ 画在内容之后不盖任何
/// 东西。详见 [`crate::paint::shadow::draw_settings_card_shadow]。
pub(super) fn settings_card(f: &mut Frame<'_>, ui: &mut Ui, card: &SettingsCard) {
    let pal = f.pal;
    // 投影晕环的占位符：**先于帧背景**插入（帧自己的 Noop 占位在这
    // 之后），内容画完后把晕环形状写回这个索引 ⇒ 晕环位于帧填充与
    // 内容之下（z 序 = 插入次序）。卡片高度内容驱动，没有「先画」
    // 的机会，这是唯一能让外扩矩形盖住卡片内部却不被看见的画法。
    // 详见 `shadow::settings_halo_shape` 的文档。
    let halo_idx = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .fill(pal.surface)
        .stroke(egui::Stroke::new(
            1.0,
            if pal.is_dark {
                egui::Color32::TRANSPARENT
            } else {
                pal.border_subtle
            },
        ))
        .corner_radius(pal.radius_xl)
        .inner_margin(egui::Margin::symmetric(20, 13))
        .show(ui, |ui| {
            // 行间距：设置项之间拉开呼吸感（2.0 太挤，文字贴在一起）。
            //
            // 14.0 而不是 8.0：行高现在**精确等于标签块**（标题 + 说明），
            // 早前那 8pt 里其实混着控件溢出撑出来的空白，看起来才够松。
            // 行高收紧后必须把间距补回来，否则相邻两行贴在一起。
            // 2026-10-25：14 → 18，配合 20/13 的卡片内边距做「松散轻柔」
            // 的节奏（HTML 原型定稿）。
            ui.spacing_mut().item_spacing = vec2(0.0, 18.0);

            // ---- 卡片标题行：标题在左，条件重置按钮在右 ----
            //
            // 标题一直显示（早前**没有标题**，只有改动时才出现的重置
            // 按钮）。缺了它，同一个分类下的多张卡片（外观/液态玻璃、
            // 快捷键/迷你面板、关于/技术栈）看起来是同一块内容的断续
            // 拼接——2026-10-24 用户反馈「设置界面看着好乱」，这是
            // 首因：用户无法知道眼前这组开关属于哪张卡。
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(t(card.title))
                                .size(pal.font_md)
                                .color(pal.text),
                        )
                        .selectable(false),
                    );
                });
            });

            // 顶部行：仅当有项偏离默认值时在**右上方**出现「重置」。
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
                                egui::RichText::new(t("Reset"))
                                    .size(pal.font_sm)
                                    .color(pal.text),
                            )
                            .fill(pal.surface_variant)
                            // 全圆角改版（2026-10-25）：radius_md → 胶囊。
                            // 半径按控件高度档位取（按钮实际高度 ≈ 字号 +
                            // 内边距×2，`control_height` 是同档量纲）。
                            .corner_radius((pal.control_height + 2.0) * 0.5),
                        );
                        let rect = resp.rect;
                        let clicked = resp.clicked();
                        resp.on_hover_text(t("Restore this card to defaults"));
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

            // ---- 分组遍历：`ItemKind::Group` 是组标题，可折叠 ----
            //
            // 组标题之后的项归属该组；折叠时整组跳过（直到下一个组标题）。
            // 组 key 存 `settings_folded`，所以增删组不会让用户的折叠
            // 偏好错位。
            //
            // 组标题本身照常画（哪怕折叠）：折叠态要看得出「这里有一组
            // 东西被收起来了」，否则用户会以为那些设置消失了。
            let mut folded = false;
            let mut idx = 0usize;
            while idx < card.items.len() {
                let it = &card.items[idx];
                if matches!(it.kind, ItemKind::Group) {
                    // 快照语义：集合里有 key = 折叠。`DEFAULT_FOLDED` 已在
                    // `seed_default_folds` 灌进来，这里不做任何异或。
                    folded = f.state.settings_folded.contains(it.key);
                    // 组内项数：从下一个位置数到再一个组标题（或卡片末尾）。
                    let count = card.items[idx + 1..]
                        .iter()
                        .take_while(|x| !matches!(x.kind, ItemKind::Group))
                        .count();
                    settings_group_header(f, ui, it, folded, count);
                    if folded {
                        ui.add_space(2.0);
                    }
                    idx += 1;
                    continue;
                }
                if folded {
                    idx += 1;
                    continue;
                }
                // 2026-10-25「隐藏而不是不可选择」：依赖项在当前配置下
                // 无意义时整项跳过（磨砂强度 / 背景不透明度 / 风格参数 /
                // 卡片不透明度 / 迷你面板与 MCP 子项，见 `item_hidden`）。
                if item_hidden(it, &f.svc.state.config) {
                    idx += 1;
                    continue;
                }
                settings_item_row(f, ui, it);
                idx += 1;
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
                // 全圆角改版（2026-10-25）：radius_md → 胶囊（半高）。
                p.rect_filled(bb, bb.height() * 0.5, if hovered { pal.accent_hover } else { pal.accent });
                p.text(
                    bb.center(),
                    egui::Align2::CENTER_CENTER,
                    t("Copy AI pairing prompt"),
                    egui::FontId::proportional(pal.font_sm),
                    pal.text_on_accent,
                );
                f.hit_insert("settings_mcp_pair", bb);
                if resp.clicked() {
                    f.state.push(Op::CopyMcpPairPrompt);
                }
            }
        });

    // 写回投影晕环（占位符索引在帧背景之前 ⇒ 晕环在填充与内容之下）。
    // `InnerResponse::response.rect` 是**外矩形**（含描边与内边距），
    // 正是晕环要围绕的目标。
    ui.painter().set(
        halo_idx,
        super::shadow::settings_halo_shape(inner.response.rect, pal.radius_xl, pal),
    );
    // 边缘装饰（深色主题色环 + 受光高光）画在填充**之上**——描边要被
    // 看见必须叠在填充上；它与内容之间隔着内边距，不会压到文字。
    super::shadow::draw_settings_card_edge(
        ui.painter(),
        inner.response.rect,
        pal.radius_xl,
        pal,
    );
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
    // 全圆角改版（2026-10-25）：chip 从圆角方块(7)改**胶囊**（半高）。
    let chip_radius = chip_h * 0.5;
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

#[cfg(test)]
mod fold_arrow_tests {
    use super::chevron_parts;

    /// chevron 的三个关键点（展开态 v 形）：与 `settings_group_header`
    /// 同一套系数，确保测的是真形状。
    fn v_shape(scale: f32) -> (egui::Pos2, egui::Pos2, egui::Pos2) {
        let h: f32 = scale * 0.26;
        let w: f32 = scale * 0.22;
        (
            egui::pos2(0.0, h),  // 顶点（下）
            egui::pos2(-w, -h), // A 端
            egui::pos2(w, -h),  // B 端
        )
    }

    /// 线段必须**首尾相接**（a→v、v→b），不能有跳点。
    ///
    /// 多凸方案的正确性全靠「两段线在顶点相接」这一件事——相接了，
    /// 三个圆就一定能盖住所有接缝；没相接，中间就会出现断口。
    #[test]
    fn segments_share_the_apex() {
        let (apex, a, b) = v_shape(15.0);
        let parts = chevron_parts(a, apex, b, 1.0);
        assert_eq!(parts.segments[0], [a, apex], "第一段应是 a→apex");
        assert_eq!(parts.segments[1], [apex, b], "第二段应是 apex→b");
    }

    /// 三个圆心必须落在 a / v / b 上，且半径 ≥ 线宽/2。
    ///
    /// 半径 ≥ 半宽是硬要求：小于它时圆盖不住线段的平头端，两个 AA
    /// 边界相切处会露出 1px 淡线。0.55× 是实现里取的值（略大于半宽
    /// 以让相邻形状充分重叠）。
    #[test]
    fn caps_sit_on_the_endpoints_and_cover_the_strokes() {
        let (apex, a, b) = v_shape(15.0);
        let stroke_w = 15.0 * 0.085;
        let r = stroke_w * 0.55;
        let parts = chevron_parts(a, apex, b, r);
        assert_eq!(parts.caps, [a, apex, b], "圆心应落在 a / v / b");
        assert!(
            parts.radius >= stroke_w * 0.5,
            "圆半径 {} 必须 ≥ 线宽一半 {}，否则盖不住平头端",
            parts.radius,
            stroke_w * 0.5
        );
        assert!(
            parts.radius < stroke_w * 0.7,
            "圆半径 {} 也不该大到让端点鼓成球（上限 0.7×线宽）",
            parts.radius
        );
    }

    /// 两臂独立伸缩（动画）后，线段仍必须共享顶点。
    ///
    /// 动画里 A/B 臂各乘自己的 `reach` 系数，顶点不动。这条断言伸缩
    /// 不会把「相接」破坏掉——`ChevronParts` 的装配是纯的，只要输入
    /// 点位正确输出就正确，但万一将来装配逻辑改成「按 reach 计算端点」
    /// 就需要这条守着。
    #[test]
    fn arms_shrink_independently_but_stay_joined() {
        let (apex, a, b) = v_shape(15.0);
        for reach_a in [0.72f32, 1.0] {
            for reach_b in [0.86f32, 1.0] {
                // 顶点不动，两端按各自系数缩放（与 settings_group_header 一致）。
                let a2 = egui::pos2(a.x * reach_a, a.y * reach_a);
                let b2 = egui::pos2(b.x * reach_b, b.y * reach_b);
                let parts = chevron_parts(a2, apex, b2, 1.0);
                assert_eq!(parts.segments[0][1], parts.segments[1][0], "两段必须在顶点相接");
                assert_eq!(parts.caps[0], parts.segments[0][0]);
                assert_eq!(parts.caps[2], parts.segments[1][1]);
            }
        }
    }

    /// 装配是纯函数：同输入恒同输出，无隐藏状态。
    ///
    /// 这条是给「几何推断」那类 bug 的反向守门——上两版的轮廓构造把
    /// 法线/切点/弧半径都做成了隐式推断，错了要靠人眼发现。多凸方案
    /// 只做装配，本条断言它保持这个性质。
    #[test]
    fn assembly_is_pure_and_deterministic() {
        let (apex, a, b) = v_shape(15.0);
        let p1 = chevron_parts(a, apex, b, 1.0);
        let p2 = chevron_parts(a, apex, b, 1.0);
        assert_eq!(p1.segments, p2.segments);
        assert_eq!(p1.caps, p2.caps);
        assert_eq!(p1.radius, p2.radius);
    }

}

/// 设置项接线的守门测试（与折叠箭头的几何测试分开）。
#[cfg(test)]
mod gpu_adapter_tests {
    use super::*;

    /// 「卡片不透明度」**任何配置下都不得被隐藏**（2026-10-28 修失效）。
    ///
    /// 早前在「无背景效果且未开卡片磨砂」时整行隐藏，理由是「没有可透的
    /// 底图，滑块无从表现」。但卡片半透明还会与窗口底色混合（卡片变淡），
    /// 那本身就是用户要的效果——隐藏的后果是用户配了 `card_opacity=0.6`
    /// 却永远看不到变化，还以为设置坏了。
    #[test]
    fn card_opacity_item_is_never_hidden() {
        use modular_clipboard_core::config::WindowBackdrop;
        let item = SETTINGS_CARDS
            .iter()
            .flat_map(|c| c.items.iter())
            .find(|it| it.key == "ui.card_opacity")
            .expect("设置里必须有一行 ui.card_opacity");

        for backdrop in [
            WindowBackdrop::None,
            WindowBackdrop::Acrylic,
            WindowBackdrop::Translucent,
            WindowBackdrop::Glass,
        ] {
            for card_blur in [0u8, 5] {
                let mut cfg = modular_clipboard_core::Config::default();
                cfg.ui.window_backdrop = backdrop;
                cfg.ui.card_blur = card_blur;
                assert!(
                    !item_hidden(item, &cfg),
                    "背景 {backdrop:?} + 卡片磨砂 {card_blur} 时这一行被隐藏了"
                );
            }
        }
    }

    /// GPU 适配器这一行必须：在列表里、能报「改过」、能重置回 auto。
    ///
    /// 三个断言各对应一条真实故障：
    /// - 不在列表里 → 用户找不到这个设置（加了变体却忘了挂条目）；
    /// - 不报 changed → 选了不保存（`changed()` 漏分支会静默不落盘）；
    /// - 重置成空串 → 解析成 Auto 虽然能用，但配置里留个空值会让
    ///   下次读配置的人困惑。
    #[test]
    fn gpu_adapter_item_wires_change_and_reset() {
        let item = SETTINGS_CARDS
            .iter()
            .flat_map(|c| c.items.iter())
            .find(|it| it.key == "ui.gpu_adapter")
            .expect("设置里必须有一行 ui.gpu_adapter");
        assert!(matches!(item.kind, ItemKind::GpuAdapter));

        let ws = crate::workspace::Workspace::default();
        let mut cfg = modular_clipboard_core::Config::default();
        assert!(
            !item.is_changed(&cfg, &ws),
            "默认配置下不该显示为「已修改」"
        );
        cfg.ui.gpu_adapter = "name:NVIDIA GeForce RTX 4090".to_string();
        assert!(item.is_changed(&cfg, &ws), "改过之后必须报 changed");

        match item.reset_op() {
            Some(Op::SetGpuAdapter(v)) => assert_eq!(v, "auto"),
            other => panic!("重置应回到 auto，实际 {other:?}"),
        }
    }}
