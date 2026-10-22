//! 运行时配置与用户偏好。

use serde::{Deserialize, Serialize};

/// 单个快捷键绑定。
///
/// NOTE: 容器级 `#[serde(default)]`：字段缺失时用 `Hotkey::default()` 补齐。
/// 缺了它，任何新增字段都会让旧config.json 解析失败，
/// 而 [`Config::load`] 会静默回退默认值——用户的快捷键**直接消失**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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

impl Hotkey {
    /// 解析 `Ctrl+Shift+V` 形式的绑定文本。
    ///
    /// 大小写与分隔符都宽松：`ctrl + shift + v`、`Ctrl+Shift+V`、空格都接受，
    /// 因为用户是**手打**这段文本的，苛刻的格式要求只会换来「保存不了」。
    ///
    /// 顺序任意（`V+Ctrl` 也行），但主键必须是最后那个非修饰键的记号。
    /// 没有修饰键的组合会被拒绝——`V` 单独注册成全局热键会劫持整个键盘。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut win = false;
        let mut key = String::new();

        // 空格与 `+` 都当分隔符：用户手打时两种写法都会出现。
        let text = text.replace(' ', "+");
        for part in text.split('+') {
            let t = part.trim();
            if t.is_empty() {
                continue;
            }
            match t.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" | "menu" => alt = true,
                "win" | "super" | "meta" => win = true,
                other => {
                    if !key.is_empty() {
                        return Err(format!("快捷键里有多个主键：{key} 与 {other}"));
                    }
                    key = other.to_string();
                }
            }
        }

        if key.is_empty() {
            return Err("缺少主键（例如 Ctrl+Shift+V 里的 V）".into());
        }
        if !ctrl && !shift && !alt && !win {
            return Err("至少需要一个修饰键（Ctrl/Shift/Alt/Win），否则会劫持整个键盘".into());
        }
        let out = Self {
            key: normalize_key(&key)?,
            ctrl,
            shift,
            alt,
            win,
        };
        Ok(out)
    }
}

impl Default for Hotkey {
    /// 唤起主窗口的默认键：**Win+V**。
    ///
    /// 与 Windows 自带的「剪贴板历史」同一个键位——这是刻意的：
    /// 用户本来就会按 Win+V，接管它才能让「换工具」没有学习成本。
    ///
    /// 代价要知道：**系统可能已经占用这个组合**，那时注册会失败，
    /// 界面会明确提示（见 `hotkey::register` 的错误处理），
    /// 用户可以换成别的组合。
    fn default() -> Self {
        Self {
            key: "V".into(),
            ctrl: false,
            shift: false,
            alt: false,
            win: true,
        }
    }
}

/// 把用户写的主键名规范成 `V` / `5` / `F5` 形式。
fn normalize_key(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("主键为空".into());
    }
    // F1..F24
    if let Some(rest) = t.strip_prefix(['f', 'F'])
        && let Ok(n) = rest.parse::<u32>()
        && (1..=24).contains(&n)
    {
        return Ok(format!("F{n}"));
    }
    // 单个字母或数字
    let mut chars = t.chars();
    let c = chars.next().unwrap_or('\0');
    if chars.next().is_some() {
        return Err(format!("主键 {t:?} 不是单个按键（仅支持 A-Z、0-9、F1-F24）"));
    }
    if c.is_ascii_alphanumeric() {
        Ok(c.to_ascii_uppercase().to_string())
    } else {
        Err(format!("主键 {c:?} 不受支持（仅支持 A-Z、0-9、F1-F24）"))
    }
}

/// 存储与保留策略。
///
/// NOTE: 容器级 `#[serde(default)]`：语义同 [`Hotkey`]。没有它，
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
/// IMPORTANT: 容器级 `#[serde(default)]`：语义同 [`Hotkey`]。这一条尤其关键——
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
///
/// NOTE: 容器级 `#[serde(default)]`：语义同 [`Config`]。下面若干字段另有
/// 字段级标注（历史原因），容器级这条保证**将来新增**的字段也不会
/// 再触发整体回退——字段级标注管不到新字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// 跟随系统（None）/强制深色（Some(true)）/强制浅色（Some(false)）。
    pub dark_mode: Option<bool>,
    pub always_on_top: bool,
    pub hide_on_focus_lost: bool,
    /// 窗口宽度，单位**物理像素**（不是逻辑点）。
    ///
    /// `app.rs` 直接把它`as u32` 传给 `CreateWindowExW`，中间无任何
    /// DPI 换算，所以在 1.5x 缩放的屏幕上 720 物理像素只有 480 逻辑点。
    pub window_width: f32,
    /// 窗口高度，单位**物理像素**。语义同[`UiConfig::window_width`]。
    pub window_height: f32,
    pub show_tray: bool,
    pub start_minimized: bool,
    /// 开机自启动。
    ///
    /// 写入注册表的是**常驻进程**（）而不是主窗口：
    /// 开机就弹一个窗口是把用户当成了开发机，而常驻进程会安静地
    /// 守在托盘、继续记录剪贴板。
    pub autostart: bool,
    /// 自定义中文字体路径，`None` 时自动探测系统字体。
    pub font_path: Option<String>,
    pub font_scale: f32,
    /// 界面语言：`"system"`（跟随系统）/ `"zh-CN"` / `"en-US"`。
    ///
    /// 用字符串而不是枚举：配置文件是用户可手改的，宽松解析 + 明确
    /// 兜底比「写错一个值整个配置回退」友好（见 [`crate::i18n::parse_lang`]）。
    pub language: String,
    /// 面板布局（停靠 / 折叠 / 浮动位置）。`None` 表示用布局默认值。
    ///
    /// NOTE: `#[serde(default)]` 不是可选的：没有它，serde 会要求这个
    /// 字段**必须存在**，于是任何旧版config.json 解析失败，
    /// [`Config::load`] 静默回退到 `Config::default()`——
    /// 用户的窗口尺寸、主题、屏蔽名单**全部被抹掉**，且没有任何提示。
    #[serde(default)]
    pub layout: Option<LayoutConfig>,
    /// 主窗口位置（屏幕逻辑点，`None` = 用默认）。
    ///
    /// # 为什么必须持久化
    ///
    /// 早前窗口位置只存在内存里，每次启动都回到默认位置——
    /// 用户拖动窗口后的位置在重启后丢失。
    ///
    /// 用 `Option` 而非 `f32`：`0.0` 是**合法坐标**（屏幕左上角），
    /// 不能用它当「未设置」的哨兵值。
    #[serde(default)]
    pub window_pos: Option<WindowPos>,
    /// 置顶窗口位置（屏幕逻辑点）。语义同 [`UiConfig::window_pos`]。
    #[serde(default)]
    pub pinned_window_pos: Option<WindowPos>,
    /// 窗口背景效果（无 / 磨砂 / 半透明）。
    ///
    /// `#[serde(default)]` 保证旧版 config.json 缺字段时解析为
    /// [`WindowBackdrop::None`]，用户既有配置不会被抹掉。
    #[serde(default)]
    pub window_backdrop: WindowBackdrop,
    /// 背景不透明度（0.3 ~ 1.0，1.0 = 最不透明）。
    ///
    /// 映射为渲染层背景层的 alpha（强度），文字/卡片恒不透明：
    /// - `Acrylic`（自研磨砂）：映射区间较宽，模糊底图承担可读性；
    /// - `Translucent`（半透明）：映射区间更实，直接透出未模糊桌面。
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f32,
    /// 卡片不透明度（0.3 ~ 1.0，1.0 = 完全不透明）。
    ///
    /// 只在背景效果为磨砂 / 半透明时才有意义：此时窗口背景层已经半透明，
    /// 若条目卡片仍是不透明的实底，磨砂底图只能从行间缝隙透出来——
    /// 「磨砂」几乎看不见。这个值把行卡片（`surface_raised`）也降 alpha，
    /// 让模糊底图透过卡片显示，同时保留卡片自身的明暗层次。
    ///
    /// 默认 1.0（不改变既有观感）：需要「卡片也透」的用户自己往下调。
    #[serde(default = "default_card_opacity")]
    pub card_opacity: f32,
    /// 磨砂强度档位（1 ~ 10，默认 7）。仅在 `window_backdrop == Acrylic`
    /// 时生效。
    ///
    /// 映射到渲染层高斯卷积半径（见 `gfx::blur::radius_for_strength`）：
    /// 1 档最轻（半径 1）、10 档最重（半径 12），第 7 档对应历史固定
    /// 半径 8，因此旧用户升级后观感不变。
    #[serde(default = "default_window_blur_strength")]
    pub window_blur_strength: u8,
    /// 磨砂光标光晕（磨砂模式下光标靠近主窗口时的径向光晕）。
    ///
    /// 默认关：见 [`GlowConfig`] 的说明（装饰性 + 持续重绘成本）。
    /// 仅在 `window_backdrop == Acrylic` 时生效——普通不透明背景下
    /// 一团光晕没有「照亮磨砂」的语义。
    #[serde(default)]
    pub glow: GlowConfig,
    /// 列表卡片阴影（历史列表里每条条目卡片的投影）。
    ///
    /// 颜色由主题令牌 `Palette::shadow` 提供（深色主题自动加浓，
    /// 否则黑色投影在近黑的列表背景上看不见），效果通过
    /// [`CardShadowConfig`] 的模糊 / 浓度 / 垂直偏移自定义。
    #[serde(default)]
    pub card_shadow: CardShadowConfig,
    /// 列表行右下角信息组合（类型徽标 + 类型 + 大小）。
    ///
    /// 三个元素的显隐与前后顺序都可在设置中调整，见 [`RowMetaConfig`]。
    #[serde(default)]
    pub row_meta: RowMetaConfig,
    /// 图片条目在列表行里直接显示缩略图（默认开）。
    ///
    /// 关闭后回落为「尺寸描述」文本（`PayloadRef::dims`），
    /// 与早期版本行为一致。
    #[serde(default = "default_true")]
    pub image_inline: bool,
    /// 通知（操作反馈）以**独立弹窗**显示（默认关 = 窗口内 toast）。
    ///
    /// 同一则通知的两种呈现：
    /// - `false`：画在主窗口内部顶部（toast），随窗口移动、被窗口
    ///   裁剪——窗口很窄时文字空间受限；
    /// - `true`：弹出一块独立小面板（与搜索面板同套弹窗机制），
    ///   不占窗口内的列表空间，窄窗口下也能完整换行显示。
    ///
    /// 默认 `false`：既有观感不变，要弹窗的用户自己开。
    #[serde(default = "default_notice_popup")]
    pub notice_popup: bool,
    /// Markdown 条目按 Markdown 渲染（标题 / 列表 / 代码块等，默认开）。
    ///
    /// 关闭后按等宽纯文本显示源码。
    #[serde(default = "default_true")]
    pub md_rendered: bool,
    /// 主窗口悬浮类型过滤栏（默认开、停靠右侧、32pt、仅显示已有类型）。
    ///
    /// 四个参数收拢成一个结构体而不是四个平铺字段：它们在设置界面
    /// 是**一张互相牵连的卡**（关掉「显示」后其余三个一起失效），
    /// 与 [`CardShadowConfig`] / [`RowMetaConfig`] 同一惯例。
    #[serde(default)]
    pub type_rail: TypeRailConfig,
    /// 悬浮搜索面板（主窗口上/下方的搜索输入面板）。
    ///
    /// 与 [`TypeRailConfig`] 同款收拢：停靠位置与高度是一张轻量卡，
    /// 关掉「显示」由 `search_expanded` 状态机决定，不设总开关。
    #[serde(default)]
    pub search_panel: SearchPanelConfig,
    /// 双击链接型条目时调用的浏览器路径。
    ///
    /// 空 = 系统默认浏览器（`ShellExecuteW` 的 `open` 动词）。
    /// 自定义时可指向第三方浏览器（Chrome / Firefox / Edge 均可）。
    #[serde(default)]
    pub browser_path: String,
    /// 双击文件型条目时调用的资源管理器路径。
    ///
    /// 空 = 系统 `explorer.exe`。自定义时可指向第三方文件管理器
    /// （Directory Opus / Total Commander / Everything 等）。
    #[serde(default)]
    pub explorer_path: String,
    /// 主题强调色（`None` = 主题内置默认蓝）。
    ///
    /// 只换**强调色一族**（accent / accent_hover / accent_active /
    /// row_selected / pin_tint），背景与文字层不动——「护眼绿」这类
    /// 整体风格走 [`UiConfig::secondary_theme]。
    #[serde(default)]
    pub accent: Option<AccentColor>,
    /// 二级主题（叠加在一级深浅模式上的风格层，默认无）。
    ///
    /// 深浅两套变体由 ui 侧主题层提供（`theme::SecondaryTheme::apply`），
    /// 一级切换时自动落到对应变体。
    #[serde(default)]
    pub secondary_theme: SecondaryTheme,
    /// 二级主题参数（见 [`ThemeParams`]）。
    ///
    /// 按主题分字段平铺：切换主题时各主题参数互相保留。
    #[serde(default)]
    pub theme_params: ThemeParams,
    /// 令牌级覆盖（高级微调，见 [`ThemeOverrides`]）。
    #[serde(default)]
    pub theme_overrides: ThemeOverrides,
}

/// 类型过滤栏配置（第三轮原型移植项）。
///
/// 停靠某侧时该侧让出 `size + RAIL_GAP` 的条带（见 ui 侧
/// `paint::FloatLayout`），浮动按钮不压列表内容。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TypeRailConfig {
    /// 是否显示类型过滤栏。
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 是否始终显示全部类型（默认关 = 仅有该类型记录时才出现）。
    #[serde(default)]
    pub always: bool,
    /// 多选组合过滤（默认关 = 单选：再点一次取消）。
    ///
    /// 开启后类型栏按钮变成开关：可同时选中多个类型，列表显示
    /// 任一所选类型的条目；取消过滤需点「全部」或逐个再点一次。
    #[serde(default)]
    pub multi: bool,
    /// 按钮上显示类型名称文字（默认关 = 仅图标）。
    ///
    /// 图标对不熟悉的用户偏抽象（尤其 Markdown 的自绘徽标），
    /// 文字标签把「点什么过滤什么」写明白；竖直停靠时文字在图标
    /// 下方，水平停靠时在右侧。
    #[serde(default)]
    pub labels: bool,
    /// 停靠的主窗口边缘（默认右）。
    #[serde(default)]
    pub pos: TypeRailPos,
    /// 图标边长（逻辑点，钳制 [`TYPE_RAIL_SIZE_MIN`]~[`TYPE_RAIL_SIZE_MAX`]）。
    #[serde(default = "default_type_rail_size")]
    pub size: f32,
}

impl Default for TypeRailConfig {
    fn default() -> Self {
        Self {
            visible: true,
            always: false,
            multi: false,
            labels: false,
            pos: TypeRailPos::Right,
            size: default_type_rail_size(),
        }
    }
}

/// `image_inline` / `md_rendered` / [`TypeRailConfig::visible`] 的 serde 缺省值。
fn default_true() -> bool {
    true
}

/// `notice_popup` 的 serde 缺省值（窗口内 toast，既有观感）。
fn default_notice_popup() -> bool {
    false
}

/// 悬浮搜索面板配置（第三轮原型移植项）。
///
/// 面板本体是一条搜索输入框（ historically 固定 30pt 高、贴在
/// 主窗口上方通栏）。两个参数都收拢进来：停靠位置决定它从哪一侧
/// 滑入，高度决定面板与输入框的呼吸空间。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SearchPanelConfig {
    /// 停靠位置：主窗口上方（默认）/ 下方。
    #[serde(default)]
    pub pos: SearchPanelPos,
    /// 面板高度（逻辑点，钳制 [`SEARCH_PANEL_H_MIN`]~[`SEARCH_PANEL_H_MAX`]）。
    #[serde(default = "default_search_panel_height")]
    pub height: f32,
}

impl Default for SearchPanelConfig {
    fn default() -> Self {
        Self {
            pos: SearchPanelPos::Top,
            height: default_search_panel_height(),
        }
    }
}

/// `search_panel_height` 的 serde 缺省值（与历史固定高度一致）。
fn default_search_panel_height() -> f32 {
    30.0
}

/// 搜索面板高度的合法区间（逻辑点）。与设置侧滑块范围一致。
pub const SEARCH_PANEL_H_MIN: f32 = 30.0;
/// 见 [`SEARCH_PANEL_H_MIN`]。
pub const SEARCH_PANEL_H_MAX: f32 = 56.0;

/// 把任意来源的高度值收敛到合法区间（非有限值回落 30.0）。
pub fn clamp_search_panel_height(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(SEARCH_PANEL_H_MIN, SEARCH_PANEL_H_MAX)
    } else {
        default_search_panel_height()
    }
}

/// 搜索面板停靠位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchPanelPos {
    /// 主窗口上方（默认）。
    #[default]
    Top,
    /// 主窗口下方。
    Bottom,
}

impl SearchPanelPos {
    /// 设置界面展示名（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            SearchPanelPos::Top => ("窗口上方", "Top"),
            SearchPanelPos::Bottom => ("窗口下方", "Bottom"),
        }
    }
}

/// `type_rail_size` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
fn default_type_rail_size() -> f32 {
    32.0
}

/// 磨砂**光标光晕**配置（2026-10-21 新增）。
///
/// 语义：磨砂（`WindowBackdrop::Acrylic`）模式下，光标**靠近**主窗口时，
/// 在光标位置渲染一团径向渐变光晕，像「窗口被光标照亮」。三个参数
/// 收拢成一个结构体（同 [`CardShadowConfig`] / [`TypeRailConfig`] 惯例）：
/// 关掉总开关后半径与浓度一起失效，设置界面是一张卡。
///
/// 默认**关闭**：它是装饰性效果，且开启后主窗口会以 ~30fps 跟随光标
/// 重绘（空闲时也有成本），按需开启。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlowConfig {
    /// 总开关（默认关）。
    pub enabled: bool,
    /// 光晕半径（逻辑点，钳制 [`GLOW_RADIUS_MIN`]~[`GLOW_RADIUS_MAX`]）。
    ///
    /// 径向渐变的边缘半径；窗口外的「靠近」判定带另有一个固定值
    /// （见 ui 侧 `GLOW_NEAR`），两者独立。
    #[serde(default = "default_glow_radius")]
    pub radius: f32,
    /// 光晕浓度（0.0 ~ 1.0，钳制 [`GLOW_STRENGTH_MIN`]~[`GLOW_STRENGTH_MAX`]）。
    ///
    /// 映射为光晕中心颜色的 alpha 倍率。0 = 不可见，1 = 最浓。
    #[serde(default = "default_glow_strength")]
    pub strength: f32,
}

impl Default for GlowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: default_glow_radius(),
            strength: default_glow_strength(),
        }
    }
}

/// `glow.radius` 的 serde 缺省值。
fn default_glow_radius() -> f32 {
    160.0
}

/// `glow.strength` 的 serde 缺省值。
fn default_glow_strength() -> f32 {
    0.35
}

/// 光晕半径合法区间（逻辑点）。与设置侧滑块范围一致。
pub const GLOW_RADIUS_MIN: f32 = 60.0;
/// 见 [`GLOW_RADIUS_MIN`]。
pub const GLOW_RADIUS_MAX: f32 = 320.0;

/// 光晕浓度合法区间。
pub const GLOW_STRENGTH_MIN: f32 = 0.0;
/// 见 [`GLOW_STRENGTH_MIN`]。
pub const GLOW_STRENGTH_MAX: f32 = 1.0;

/// 把任意来源的半径值收敛到合法区间（非有限值回落默认）。
pub fn clamp_glow_radius(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(GLOW_RADIUS_MIN, GLOW_RADIUS_MAX)
    } else {
        default_glow_radius()
    }
}

/// 把任意来源的浓度值收敛到合法区间（非有限值回落默认）。
pub fn clamp_glow_strength(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(GLOW_STRENGTH_MIN, GLOW_STRENGTH_MAX)
    } else {
        default_glow_strength()
    }
}

/// `type_rail_size` 的合法区间（逻辑点）。与设置侧滑块范围一致。
pub const TYPE_RAIL_SIZE_MIN: f32 = 24.0;
/// 见 [`TYPE_RAIL_SIZE_MIN`]。
pub const TYPE_RAIL_SIZE_MAX: f32 = 48.0;

/// 把任意来源的尺寸值收敛到合法区间（非有限值回落 32.0）。
///
/// 与 [`clamp_font_scale`] 同构：手改 config.json 是常态，
/// 不能让一个越界值把图标画成 0 或 480。
pub fn clamp_type_rail_size(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(TYPE_RAIL_SIZE_MIN, TYPE_RAIL_SIZE_MAX)
    } else {
        default_type_rail_size()
    }
}

/// 类型过滤栏停靠的主窗口边缘。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeRailPos {
    /// 右侧（默认），垂直居中。
    #[default]
    Right,
    /// 左侧，垂直居中。
    Left,
    /// 顶部，横向居中。
    Top,
    /// 底部，横向居中。
    Bottom,
}

impl TypeRailPos {
    /// 设置界面下拉框展示名（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            TypeRailPos::Right => ("右侧", "Right"),
            TypeRailPos::Left => ("左侧", "Left"),
            TypeRailPos::Top => ("顶部", "Top"),
            TypeRailPos::Bottom => ("底部", "Bottom"),
        }
    }

    /// 配置里的稳定字符串（与 serde 编码一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            TypeRailPos::Right => "right",
            TypeRailPos::Left => "left",
            TypeRailPos::Top => "top",
            TypeRailPos::Bottom => "bottom",
        }
    }

    /// 从字符串解析，无法识别时回落 [`TypeRailPos::Right`]。
    ///
    /// 容错而不是报错：这个值来自用户手装的 config.json，
    /// 为一个 cosmetic 设置让整个配置加载失败不值得。
    pub fn from_str_opt(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" => TypeRailPos::Left,
            "top" => TypeRailPos::Top,
            "bottom" => TypeRailPos::Bottom,
            _ => TypeRailPos::Right,
        }
    }
}

/// 列表卡片阴影配置。
///
/// 渲染分两遍：先为所有可见行画投影，再画卡片本体——投影只在
/// 行间缝隙（`ROW_GAP`）与列表边缘可见，不会压暗相邻卡片的文字。
/// 投影本身是多层同心圆角矩形叠加出的**高斯衰减**（不是单条
/// 抗锯齿边），圆角处没有 alpha 重叠暗斑。
///
/// # 为什么颜色不开放
///
/// 投影色几乎总是黑色（深色主题下也是黑的，只是基准浓度更高），
/// 开放颜色只会多出一个没人用的旋钮；「自定义」集中在
/// 模糊（笔触软硬）、浓度（深浅）、垂直偏移（高低）三个
/// 真正影响观感的维度上。
///
/// 取值范围（绘制侧会再钳制一遍，防手改配置文件超界）：
/// - `blur`：0 ~ 24（高斯扩散半径，0 = 无投影）
/// - `opacity`：0.0 ~ 0.6（浓度倍率；0.16 = 1.0× 主题强度，默认 0.20 ≈ 1.25×，
///   实际像素 alpha 再按主题令牌 `Palette::shadow` 缩放，
///   深色主题会自动比浅色更浓）
/// - `offset_y`：-8 ~ 16（逻辑点，正值向下）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardShadowConfig {
    /// 是否启用列表卡片阴影。
    pub enabled: bool,
    /// 模糊半径（逻辑点，高斯扩散半径）。
    pub blur: f32,
    /// 浓度倍率：0.16 为 1.0× 主题强度，0 为无投影，上限 0.6。
    pub opacity: f32,
    /// 垂直偏移（逻辑点，正值向下）。
    pub offset_y: f32,
}

impl Default for CardShadowConfig {
    fn default() -> Self {
        // 2026-10-15 定稿：预览页验收后的推荐参数。
        Self {
            enabled: true,
            blur: 8.0,
            opacity: 0.20,
            offset_y: 0.0,
        }
    }
}

/// 列表行右下角信息组合的一个元素。
///
/// 三个元素可在设置中：
/// - 单击切换显隐（灰 = 不显示）；
/// - 按住拖动重排前后顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RowMetaPart {
    /// 圆角小方块类型徽标（内嵌类型图标）。
    Icon,
    /// 类型文字标签（文本 / HTML / 图片 / 文件）。
    #[serde(rename = "type")]
    Type,
    /// 大小文字（B / KB / MB / GB，自适应单位）。
    Size,
}

impl RowMetaPart {
    /// 全部三个元素（自然顺序），供设置编辑器与渲染侧遍历。
    pub const ALL: [RowMetaPart; 3] = [RowMetaPart::Icon, RowMetaPart::Type, RowMetaPart::Size];

    /// 元素在设置编辑器中的名称（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            RowMetaPart::Icon => ("图标", "Icon"),
            RowMetaPart::Type => ("类型", "Type"),
            RowMetaPart::Size => ("大小", "Size"),
        }
    }
}

/// 列表行右下角「类型徽标 + 类型 + 大小」组合的配置。
///
/// - `order`：三个元素从左到右的排列顺序；
/// - `icon` / `type` / `size`：各元素是否显示，默认全部显示。
///
/// 元素之间只留固定间距，不再用「·」分隔。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RowMetaConfig {
    /// 组合顺序（从左到右）。默认 图标 → 类型 → 大小。
    pub order: [RowMetaPart; 3],
    /// 是否显示类型徽标。
    pub icon: bool,
    /// 是否显示类型文字标签。
    #[serde(rename = "type")]
    pub type_: bool,
    /// 是否显示大小文字。
    pub size: bool,
}

impl Default for RowMetaConfig {
    fn default() -> Self {
        Self {
            order: RowMetaPart::ALL,
            icon: true,
            type_: true,
            size: true,
        }
    }
}

impl RowMetaConfig {
    /// 该元素当前是否显示。
    pub fn visible(&self, part: RowMetaPart) -> bool {
        match part {
            RowMetaPart::Icon => self.icon,
            RowMetaPart::Type => self.type_,
            RowMetaPart::Size => self.size,
        }
    }

    /// 切换某个元素的显隐（设置编辑器中单击）。
    pub fn toggle(&mut self, part: RowMetaPart) {
        match part {
            RowMetaPart::Icon => self.icon = !self.icon,
            RowMetaPart::Type => self.type_ = !self.type_,
            RowMetaPart::Size => self.size = !self.size,
        }
    }

    /// 把 `part` 移动到目标索引 `idx`（拖动重排）：先从原位置取出，
    /// 再插入到 `idx`，其余元素保持相对顺序不变。`idx` 越界会被钳制
    /// 到合法区间；元素不存在时不改动。
    ///
    /// 拖拽 UI 按鼠标位置直接算出目标索引（鼠标左侧有几个元素就插到
    /// 第几），因此每帧结果只取决于鼠标位置、天然幂等，不会在两个
    /// 相邻位置间来回抖动。
    pub fn move_to_index(&mut self, part: RowMetaPart, idx: usize) {
        let Some(cur) = self.order.iter().position(|p| *p == part) else {
            return;
        };
        let mut v: Vec<RowMetaPart> = self.order.to_vec();
        let moved = v.remove(cur);
        let idx = idx.min(v.len());
        v.insert(idx, moved);
        self.order = [v[0], v[1], v[2]];
    }
}

/// 窗口背景效果。
///
/// 三选一，互斥：磨砂与半透明都靠交换链的**像素 alpha**（PRE_MULTIPLIED）
/// 与桌面合成，机制相同不能同时开，因此不是位标志而是单选。
///
/// 均为自研实现，不依赖 DWM 原生 backdrop（`DWMWA_SYSTEMBACKDROP_TYPE`
/// 已移除：Win11 22H2+ 才可用、外观不可调、与暗色渲染层偏色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WindowBackdrop {
    /// 无效果：纯色不透明背景。
    #[default]
    None,
    /// 自研磨砂：GDI 实时捕获窗口身后的桌面（`WDA_EXCLUDEFROMCAPTURE`
    /// 自排除，见 `gfx::backdrop`），GPU 三趟卷积后作窗口底图；背景层
    /// 画低 alpha 让模糊透出，文字/卡片保持不透明。
    Acrylic,
    /// 逐像素半透明：背景层画低 alpha，桌面直接透出（无模糊）；
    /// 文字/卡片不透明保证可读性。不再用整窗 `LWA_ALPHA` 淡出
    /// （那会把文字一起变透明，低不透明度下不可读）。
    Translucent,
}

/// `window_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
fn default_window_opacity() -> f32 {
    1.0
}

/// `card_opacity` 的 serde 缺省值（与 [`UiConfig::default`] 一致）。
fn default_card_opacity() -> f32 {
    1.0
}

/// `window_blur_strength` 的 serde 缺省值：第 7 档对应历史固定半径 8。
fn default_window_blur_strength() -> u8 {
    7
}

/// 一个窗口的位置（屏幕逻辑点，左上角为原点）。
///
/// 独立成结构体而不是两个 `f32`，是为了让 `serde` 的
/// `#[serde(default)]` 在字段缺失时能正确填 `None`。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowPos {
    pub x: f32,
    pub y: f32,
}

impl WindowPos {
    /// 新建。
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// 二级主题：叠加在一级（深浅）模式之上的**风格层**。
///
/// 与 [`WindowBackdrop`] 的分工：背景效果改的是「窗口怎么透出桌面」，
/// 二级主题改的是「整套配色与质感」。每个二级主题在深浅两种基底下
/// 各有一套取值（见 ui 侧 `theme::SecondaryTheme::apply`），一级切换时
/// 自动落到对应变体，不需要用户选两遍。
///
/// # 为什么不做成「配色方案」平铺
///
/// 一级模式（深浅）是**功能位**：它决定文字与底的明暗关系，错了整个
/// 界面不可读。二级主题是**审美位**：只在同一种明暗关系里换色温与
/// 质感。两者叠成一个枚举会让「OLED + 浅色」这种组合无法表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecondaryTheme {
    /// 不加二级主题（默认）：纯正的一级深浅主题。
    #[default]
    None,
    /// 纸质纹理：暖色纸基 + 程序化纤维/噪点。
    Paper,
    /// 护眼绿：低蓝光绿基。
    Green,
    /// 星空紫：深紫基 + 程序化星点。
    Purple,
    /// OLED：纯黑背景（自发光屏幕省电、暗环境不刺眼）。
    Oled,
}

impl SecondaryTheme {
    /// 设置界面展示名（双语对）。
    pub fn label(self) -> crate::i18n::Bi {
        match self {
            SecondaryTheme::None => ("无", "None"),
            SecondaryTheme::Paper => ("纸纹", "Paper"),
            SecondaryTheme::Green => ("护眼绿", "Eye-care green"),
            SecondaryTheme::Purple => ("星空紫", "Starfield purple"),
            SecondaryTheme::Oled => ("OLED", "OLED"),
        }
    }

    /// 配置里的稳定字符串（与 serde 编码一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            SecondaryTheme::None => "none",
            SecondaryTheme::Paper => "paper",
            SecondaryTheme::Green => "green",
            SecondaryTheme::Purple => "purple",
            SecondaryTheme::Oled => "oled",
        }
    }

    /// 从字符串解析，无法识别时回落 [`SecondaryTheme::None`]。
    ///
    /// 容错而不是报错：这个值可能来自用户手装的 config.json，
    /// 为一个 cosmetic 设置让整个配置加载失败不值得。
    pub fn from_str_opt(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "paper" => SecondaryTheme::Paper,
            "green" => SecondaryTheme::Green,
            "purple" => SecondaryTheme::Purple,
            "oled" => SecondaryTheme::Oled,
            _ => SecondaryTheme::None,
        }
    }

    /// 本主题可调的参数旋钮（设置界面据此生成滑块）。
    ///
    /// 键名与 [`ThemeParams`] 的字段一一对应；`None` 主题没有旋钮。
    pub fn knobs(self) -> &'static [ThemeKnob] {
        match self {
            SecondaryTheme::None => &[],
            SecondaryTheme::Paper => PAPER_KNOBS,
            SecondaryTheme::Green => GREEN_KNOBS,
            SecondaryTheme::Purple => PURPLE_KNOBS,
            SecondaryTheme::Oled => OLED_KNOBS,
        }
    }
}

/// 纸质主题的旋钮（[`SecondaryTheme::Paper`]）。
static PAPER_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("paper_tint", ("纸基浓度", "Paper tint"), ("暖色纸基的混合比例", "How strongly the warm paper base mixes in")),
    ThemeKnob::new("paper_texture", ("纹理强度", "Texture strength"), ("纤维与噪点的明显程度", "How visible the fibers and grain are")),
];

/// 护眼绿主题的旋钮（[`SecondaryTheme::Green`]）。
static GREEN_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("green_tint", ("绿意浓度", "Green tint"), ("绿色基调的混合比例", "How strongly the green base mixes in")),
    ThemeKnob::new("green_warmth", ("色温", "Warmth"), ("偏青（冷）到偏黄（暖）", "From cyan (cool) to yellow (warm)")),
];

/// 星空紫主题的旋钮（[`SecondaryTheme::Purple`]）。
static PURPLE_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("purple_tint", ("紫度", "Purple tint"), ("紫色基调的混合比例", "How strongly the purple base mixes in")),
    ThemeKnob::new("purple_stars", ("星点密度", "Star density"), ("背景星点的明显程度", "How visible the background stars are")),
    ThemeKnob::new("star_random", ("分布随机化", "Randomize layout"), ("0 = 固定一片星空，1 = 每次重建换一片", "0 = one fixed sky, 1 = reshuffle on every rebuild")),
    ThemeKnob::new("star_twinkle", ("闪烁", "Twinkle"), ("星点缓慢明暗呼吸的强度", "How strongly stars breathe between bright and dim")),
    ThemeKnob::new("star_size_var", ("大小不规则", "Size variation"), ("星点半径的差异程度", "How much star radii vary")),
    ThemeKnob::new("star_cursor", ("光标互动", "Cursor parallax"), ("星点随光标位置反向偏移（视差）", "Stars shift opposite to the cursor (parallax)")),
];

/// OLED 主题的旋钮（[`SecondaryTheme::Oled`]）。
static OLED_KNOBS: &[ThemeKnob] = &[
    ThemeKnob::new("oled_black", ("纯黑程度", "Black level"), ("背景压向纯黑的比例", "How far the background is pushed toward pure black")),
    ThemeKnob::new("oled_borders", ("分隔提亮", "Borders"), ("纯黑下靠描边区分层级", "Separate layers with brighter borders on pure black")),
];

/// 二级主题的一个可调参数（[`SecondaryTheme::knobs`] 的元素）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeKnob {
    /// [`ThemeParams`] 里的字段名（稳定键）。
    pub key: &'static str,
    /// 设置界面展示名（双语对）。
    pub label: crate::i18n::Bi,
    /// 一句话说明（双语对）。
    pub desc: crate::i18n::Bi,
}

impl ThemeKnob {
    const fn new(key: &'static str, label: crate::i18n::Bi, desc: crate::i18n::Bi) -> Self {
        Self { key, label, desc }
    }
}

/// 二级主题参数（全部 0..1 归一化，显示侧各自映射量程）。
///
/// 平铺成一个结构体而不是按主题分结构：配置里只有一个
/// `theme_params` 对象，切换主题时各主题的参数**互相保留**——
/// 用户把纸质的纹理强度调到 0.9，切到 OLED 再切回来，还是 0.9。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThemeParams {
    /// 纸质：暖色纸基混合比例（0 = 原主题色，1 = 完全纸基）。
    #[serde(default = "default_param_high")]
    pub paper_tint: f32,
    /// 纸质：纹理强度（0 = 无纹理，1 = 满纹理）。
    #[serde(default = "default_param_mid")]
    pub paper_texture: f32,
    /// 护眼绿：绿意混合比例。
    #[serde(default = "default_param_high")]
    pub green_tint: f32,
    /// 护眼绿：色温（0 = 偏青，1 = 偏黄）。
    #[serde(default = "default_param_mid")]
    pub green_warmth: f32,
    /// 星空紫：紫色基调混合比例。
    #[serde(default = "default_param_high")]
    pub purple_tint: f32,
    /// 星空紫：星点密度。
    #[serde(default = "default_param_mid")]
    pub purple_stars: f32,
    /// 星空紫：星点**分布随机化**强度（0 = 固定种子，每次启动同一片
    /// 星空；1 = 每次重建混入时间熵，且每颗星叠加全幅位置抖动）。
    ///
    /// 中间值按比例混入两者。设计成「确定性 ↔ 随机」的连续旋钮而不是
    /// 布尔：回归测试需要 0 档可断言，用户需要 1 档每次不一样。
    #[serde(default = "default_param_mid")]
    pub star_random: f32,
    /// 星空紫：星点**闪烁**强度（0 = 静态星点；1 = 全幅缓慢闪烁）。
    ///
    /// 闪烁是「夜空是活的」的主要来源——完全静止的星点看起来像一张
    /// 贴图。每颗星有自己的相位与频率（不是齐刷刷地闪）。
    #[serde(default = "default_param_high")]
    pub star_twinkle: f32,
    /// 星空紫：星点**大小不规则度**（0 = 均一大小的哑星；1 = 半径在
    /// 0.6~3.2pt 全范围随机）。
    ///
    /// 真实星空的大小差异来自距离，这里用半径方差模拟——均一的星点
    /// 是「矢量图标感」，大小分级才读出纵深。
    #[serde(default = "default_param_mid")]
    pub star_size_var: f32,
    /// 星空紫：**光标与星点的互动强度**（0 = 关闭互动，星点静止于
    /// 各自位置；1 = 满幅视差——星点随光标位置反向偏移，且越近的星
    /// 偏移越大）。
    ///
    /// 交互语义是「视差」而不是「吸引/排斥」：光标靠近时星点轻轻让开
    /// （像被吹开），比吸引更符合「手电筒照夜空」的直觉，也不会把
    /// 星点拉成一团。光标离开窗口（`pointer.pos()` 为 None）时平滑
    /// 回到原位。
    #[serde(default = "default_param_off")]
    pub star_cursor: f32,
    /// OLED：背景压向纯黑的比例。
    #[serde(default = "default_param_high")]
    pub oled_black: f32,
    /// OLED：描边提亮（纯黑下靠描边区分层级）。
    #[serde(default = "default_param_mid")]
    pub oled_borders: f32,
}

impl Default for ThemeParams {
    fn default() -> Self {
        Self {
            paper_tint: default_param_high(),
            paper_texture: default_param_mid(),
            green_tint: default_param_high(),
            green_warmth: default_param_mid(),
            purple_tint: default_param_high(),
            purple_stars: default_param_mid(),
            star_random: default_param_mid(),
            star_twinkle: default_param_high(),
            star_size_var: default_param_mid(),
            star_cursor: default_param_off(),
            oled_black: default_param_high(),
            oled_borders: default_param_mid(),
        }
    }
}

/// 「互动类」旋钮的缺省值：默认关闭。
///
/// 视差互动是「用户主动要的玩具」，默认开会让光标划过时星点乱晃——
/// 工具软件不该默认动。要开的用户自己拖滑块。
fn default_param_off() -> f32 {
    0.0
}

/// 「浓度类」旋钮的缺省值：默认就有效果，但留出调大空间。
fn default_param_high() -> f32 {
    0.8
}

/// 「效果类」旋钮的缺省值：默认可见但不喧宾夺主。
fn default_param_mid() -> f32 {
    0.5
}

impl ThemeParams {
    /// 按键名读参数（找不到返回 `None`）。
    ///
    /// 与 [`SecondaryTheme::knobs`] 的键名对应，供设置界面与绘制侧
    /// 用同一套字符串寻址。
    pub fn get(&self, key: &str) -> Option<f32> {
        Some(match key {
            "paper_tint" => self.paper_tint,
            "paper_texture" => self.paper_texture,
            "green_tint" => self.green_tint,
            "green_warmth" => self.green_warmth,
            "purple_tint" => self.purple_tint,
            "purple_stars" => self.purple_stars,
            "star_random" => self.star_random,
            "star_twinkle" => self.star_twinkle,
            "star_size_var" => self.star_size_var,
            "star_cursor" => self.star_cursor,
            "oled_black" => self.oled_black,
            "oled_borders" => self.oled_borders,
            _ => return None,
        })
    }

    /// 按键名写参数（找不到返回 `false`，调用方应报错而非静默）。
    pub fn set(&mut self, key: &str, v: f32) -> bool {
        let v = clamp_theme_param(v);
        match key {
            "paper_tint" => self.paper_tint = v,
            "paper_texture" => self.paper_texture = v,
            "green_tint" => self.green_tint = v,
            "green_warmth" => self.green_warmth = v,
            "purple_tint" => self.purple_tint = v,
            "purple_stars" => self.purple_stars = v,
            "star_random" => self.star_random = v,
            "star_twinkle" => self.star_twinkle = v,
            "star_size_var" => self.star_size_var = v,
            "star_cursor" => self.star_cursor = v,
            "oled_black" => self.oled_black = v,
            "oled_borders" => self.oled_borders = v,
            _ => return false,
        }
        true
    }
}

/// 把任意来源的参数值收敛到 0..1（非有限值回落 0.5）。
///
/// 手改 config.json 是常态，越界值不能让滑块或混合比例失控。
pub fn clamp_theme_param(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        default_param_mid()
    }
}

/// 主题强调色（HSV 表示）。
///
/// `None`（配置里不设置）= 用主题内置默认蓝。设置后，一级主题的
/// 蓝系强调色（选中、主操作、相关底色）整体换成用户选的颜色，
/// 深浅两套都换。
///
/// # 为什么用 HSV 而不是 RGB
///
/// 「换主题色」的直觉是「沿着色相环挑一个颜色」：HSV 的 H 滑条正好
/// 对应这个动作，S/V 分别控制鲜艳与明暗。RGB 三个滑条没有一个是
/// 「 hue」，用户想从蓝渐变到紫得同时猜两个分量。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AccentColor {
    /// 色相（度，0..360，0/360 = 红）。
    pub h: f32,
    /// 饱和度（0..1）。
    pub s: f32,
    /// 明度（0..1）。
    pub v: f32,
}

impl AccentColor {
    /// 新建并钳制到合法区间（非有限值分量回落默认蓝的对应分量）。
    pub fn new(h: f32, s: f32, v: f32) -> Self {
        Self {
            h: if h.is_finite() { h.rem_euclid(360.0) } else { 211.0 },
            s: if s.is_finite() { s.clamp(0.0, 1.0) } else { 0.8 },
            v: if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.69 },
        }
    }

    /// 默认蓝（与内置主题的 accent 同色：`#1D65AF` 反推）。
    pub fn default_blue() -> Self {
        Self::from_rgb([0x1D, 0x65, 0xAF])
    }

    /// 转成 RGB 字节（标准 HSV→RGB，`v` 直接作为最大通道）。
    pub fn to_rgb(&self) -> [u8; 3] {
        let c = Self::new(self.h, self.s, self.v);
        let sector = (c.h / 60.0).floor() as i32 % 6;
        let f = (c.h / 60.0).fract();
        let chroma = c.v * c.s;
        let m = c.v - chroma;
        // 中间通道：偶数扇区取 `chroma * f`、奇数扇区取 `chroma * (1 - f)`
        // （`|h/60 mod 2 - 1|` 在两个扇区之间交替）。最小通道恒为 `m`。
        let mid = |k: i32| {
            if k % 2 == 0 {
                chroma * f
            } else {
                chroma * (1.0 - f)
            }
        };
        let (r, g, b) = match sector {
            0 => (c.v, mid(0) + m, m),
            1 => (mid(1) + m, c.v, m),
            2 => (m, c.v, mid(2) + m),
            3 => (m, mid(3) + m, c.v),
            4 => (mid(4) + m, m, c.v),
            _ => (c.v, m, mid(5) + m),
        };
        [
            (r * 255.0).round() as u8,
            (g * 255.0).round() as u8,
            (b * 255.0).round() as u8,
        ]
    }

    /// 从 RGB 字节反推 HSV（用于预设色板与十六进制输入落地）。
    pub fn from_rgb(rgb: [u8; 3]) -> Self {
        let [r, g, b] = [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0];
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let h = if d < f32::EPSILON {
            0.0
        } else if max == r {
            60.0 * (((g - b) / d).rem_euclid(6.0))
        } else if max == g {
            60.0 * (((b - r) / d) + 2.0)
        } else {
            60.0 * (((r - g) / d) + 4.0)
        };
        let s = if max < f32::EPSILON { 0.0 } else { d / max };
        Self::new(h, s, max)
    }

    /// `#RRGGBB` 十六进制字符串（设置界面输入框回显）。
    pub fn to_hex(&self) -> String {
        let [r, g, b] = self.to_rgb();
        format!("#{r:02X}{g:02X}{b:02X}")
    }

    /// 解析 `#RRGGBB` / `RRGGBB`（容忍缺失 `#` 与大小写）。
    ///
    /// 失败返回 `None`：输入框正在打字中途（如只敲了 `#F`）不能
    /// 把用户已选的颜色清掉。
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self::from_rgb([r, g, b]))
    }
}

/// 用户对单个主题令牌的覆盖——「界面上任何元素、任何参数都可调」
/// 的落地机制。
///
/// 键是 ui 侧 [`Palette`](crate::ui 不可见，见 `modular-clipboard-ui`
/// 的 `theme::Palette::tokens`) 的字段名；值是**非预乘** RGBA（颜色）
/// 或数值（几何 / 字号 / 描边宽度）。覆盖在一级主题、二级主题、
/// 强调色**之后**应用——用户显式改的旋钮优先级最高。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThemeOverrides {
    /// 令牌名 → 非预乘 RGBA。
    #[serde(default)]
    pub colors: std::collections::BTreeMap<String, [u8; 4]>,
    /// 令牌名 → 数值。
    #[serde(default)]
    pub nums: std::collections::BTreeMap<String, f32>,
}

impl UiConfig {
    /// 当前生效的强调色（配置优先，缺省用内置默认蓝）。
    pub fn accent_color(&self) -> AccentColor {
        self.accent.unwrap_or_else(AccentColor::default_blue)
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            dark_mode: None,
            always_on_top: true,
            hide_on_focus_lost: false,
            window_width: 720.0,
            window_height: 800.0,
            show_tray: true,
            start_minimized: false,
            autostart: false,
            font_path: None,
            font_scale: 1.0,
            language: crate::i18n::LANG_SYSTEM.to_string(),
            layout: None,
            window_pos: None,
            pinned_window_pos: None,
            window_backdrop: WindowBackdrop::None,
            window_opacity: 1.0,
            card_opacity: 1.0,
            window_blur_strength: default_window_blur_strength(),
            glow: GlowConfig::default(),
            card_shadow: CardShadowConfig::default(),
            row_meta: RowMetaConfig::default(),
            image_inline: true,
            notice_popup: false,
            md_rendered: true,
            type_rail: TypeRailConfig::default(),
            search_panel: SearchPanelConfig::default(),
            browser_path: String::new(),
            explorer_path: String::new(),
            accent: None,
            secondary_theme: SecondaryTheme::None,
            theme_params: ThemeParams::default(),
            theme_overrides: ThemeOverrides::default(),
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
    #[serde(default)]
    pub keyword: Option<String>,
    /// 匹配到的条目类型，`None` 表示不限。
    #[serde(default)]
    pub kind: Option<crate::item::ClipKind>,
    /// 动作标识，交由 [`ActionRegistry`] 解析为具体实现。
    pub action: String,
    /// 动作参数，例如命令行模板 `{selection}`。
    #[serde(default)]
    pub arg: Option<String>,
    #[serde(default)]
    pub sort_order: i64,
}

/// MCP（Model Context Protocol）服务配置。
///
/// 应用对外暴露 MCP 服务，供 AI 客户端（Claude Desktop / Cursor 等）
/// 调用界面自动化能力：调节窗口大小、获取元素布局、点击元素等。
///
/// `enabled=false` 时所有 MCP 调用不可用；其余 8 个布尔字段是
/// **逐项可调用开关**，默认全部开启。
///
/// NOTE: 容器级 `#[serde(default)]`：语义同 [`Hotkey`]——缺字段时用
/// 默认值补齐，不会让旧 config.json 整体失效。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// MCP 服务总开关。关闭后所有调用不可用。
    pub enabled: bool,
    /// 服务监听地址（自定义，默认空 = 未设置）。
    pub host: String,
    /// 服务端口（自定义，`0` = 未设置）。
    pub port: u16,
    /// 访问密钥（默认空 = 无需认证）。
    pub token: String,
    // ---- 可调用能力（默认全部开启）----
    /// 调节任意窗口的尺寸与位置。
    pub resize_window: bool,
    /// 读取界面元素的几何与层级信息。
    pub read_layout: bool,
    /// 在任意位置点击或激活元素。
    pub click_element: bool,
    /// 向焦点控件键入文字与快捷键。
    pub type_text: bool,
    /// 滚动页面与列表到目标位置。
    pub scroll_view: bool,
    /// 截取屏幕或窗口画面。
    pub screen_capture: bool,
    /// 激活指定窗口或应用。
    pub window_focus: bool,
    /// 拖拽元素或执行拖放。
    pub drag_drop: bool,
    /// 模拟按键（含组合键与功能键）。
    pub key_press: bool,
    /// 读写系统剪贴板文本。
    pub clipboard: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: String::new(),
            port: 0,
            token: String::new(),
            resize_window: true,
            read_layout: true,
            click_element: true,
            type_text: true,
            scroll_view: true,
            screen_capture: true,
            window_focus: true,
            drag_drop: true,
            key_press: true,
            clipboard: true,
        }
    }
}

/// 全量配置。
///
/// NOTE: 容器级 `#[serde(default)]`：**这是整个配置文件的最后一道防线**。
///
/// 没有它，任意一个子结构新增字段 → serde 报 "missing field" →
/// [`Config::load`] 静默回退 `Config::default()` → 用户的窗口尺寸、
/// 主题、快捷键、屏蔽名单**一次性全部归零**，且界面上没有任何提示
/// （只有一行 `tracing::warn`）。有了它，缺失字段由各自结构的
/// `Default` 补齐，其余字段**原样保留**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub storage: StorageConfig,
    pub capture: CaptureConfig,
    pub ui: UiConfig,
    pub hotkey_toggle: Hotkey,
    pub hotkey_paste_previous: Hotkey,
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
}

impl MiniPanelConfig {
    /// 面板里实际显示的条数（已夹到 1..=10）。
    pub fn effective_count(&self) -> usize {
        self.count.clamp(1, 10)
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

    /// 光晕两个参数的钳制（手改 config.json 是常态）。
    #[test]
    fn glow_params_clamp() {
        assert_eq!(clamp_glow_radius(160.0), 160.0);
        assert_eq!(clamp_glow_radius(10.0), GLOW_RADIUS_MIN);
        assert_eq!(clamp_glow_radius(9999.0), GLOW_RADIUS_MAX);
        assert_eq!(clamp_glow_radius(f32::NAN), default_glow_radius());
        assert_eq!(clamp_glow_strength(0.5), 0.5);
        assert_eq!(clamp_glow_strength(-1.0), GLOW_STRENGTH_MIN);
        assert_eq!(clamp_glow_strength(2.0), GLOW_STRENGTH_MAX);
        assert_eq!(clamp_glow_strength(f32::NAN), default_glow_strength());
    }

    /// 旧版 config.json 没有 `glow` 字段：必须解析为「关闭」而不是
    /// 整个配置回退默认（那会抹掉用户的全部设置）。
    #[test]
    fn missing_glow_field_defaults_off() {
        let cfg: Config = serde_json::from_str(r#"{"ui":{"dark_mode":false}}"#).unwrap();
        assert!(!cfg.ui.glow.enabled, "缺省必须关闭");
        assert_eq!(cfg.ui.glow.radius, default_glow_radius());
        assert_eq!(cfg.ui.glow.strength, default_glow_strength());
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
            assert!(!p.label().0.is_empty() && !p.label().1.is_empty());
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