//! MCP 服务配置、联动动作规则与「粘贴上一条」模式。

use serde::Deserialize;
use serde::Serialize;

/// 动作规则：当条目匹配条件时，可一键执行的动作。
///
/// 这是「软件联动」的扩展点。新增动作只需实现 `Action` trait 并注册，
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
    /// 动作标识，交由 `ActionRegistry` 解析为具体实现。
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
/// NOTE: 容器级 `#[serde(default)]`：语义同 `Hotkey`——缺字段时用
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
/// `Config::load` 静默回退 `Config::default()` → 用户的窗口尺寸、
/// 主题、快捷键、屏蔽名单**一次性全部归零**，且界面上没有任何提示
/// （只有一行 `tracing::warn`）。有了它，缺失字段由各自结构的
/// `Default` 补齐，其余字段**原样保留**。
/// 「粘贴上一条」的行为模式。
///
/// 同一个热键的两种语义，按用户的工作流选：
///
/// | 模式 | 行为 | 适用 |
/// |---|---|---|
/// | `PasteAndType` | 写剪贴板 + 向目标窗口发 Ctrl+V | 单屏单窗口、目标明确 |
/// | `NotifyOnly` | 只写剪贴板 + 通知，**不发按键** | 多屏/多窗口、目标程序对合成按键敏感 |
///
/// # 为什么默认是 `PasteAndType`
///
/// 那是本项目**既有行为**。默认值变保守等于用户升级后按同样热键
/// 却不再粘贴——行为静默变化比多一个选项糟糕得多。想更稳的用户
/// 自己切到 `NotifyOnly`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PastePreviousMode {
    /// 写剪贴板并**自动输入**（向目标窗口发 Ctrl+V）。
    ///
    /// 目标窗口在写剪贴板那一刻抓（见 `copy_and_paste` 的说明）——
    /// 猜错时会粘到别的窗口，这是这个模式固有的代价。
    #[default]
    PasteAndType,
    /// 只写剪贴板并**提示**，粘到哪里由用户按 Ctrl+V 决定。
    NotifyOnly,
}
