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
            layout: None,
            window_pos: None,
            pinned_window_pos: None,
            window_backdrop: WindowBackdrop::None,
            window_opacity: 1.0,
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