//! 界面配置主体（窗口 / 主题开关 / 各类子配置的挂载点）。

use serde::Deserialize;
use serde::Serialize;

use super::layout::LayoutConfig;
use super::panels::SearchPanelConfig;
use super::panels::TypeRailConfig;
use super::panels::default_notice_popup;
use super::panels::default_true;
use super::card_row::CardShadowConfig;
use super::card_row::RowMetaConfig;
use super::window::WindowBackdrop;
use super::window::WindowPos;
use super::window::default_card_blur;
use super::window::default_glass_dispersion;
use super::window::default_glass_edge;
use super::window::default_glass_refract;
use super::window::default_card_opacity;
use super::window::default_window_blur_strength;
use super::window::default_window_opacity;
use super::secondary::SecondaryTheme;
use super::theme_params::ThemeParams;
use super::accent::AccentColor;
use super::accent::ThemeOverrides;

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
    /// 卡片磨砂档位（0 ~ 10，**0 = 关闭**）。
    ///
    /// 与 `window_blur_strength` 不是一回事：后者模糊的是**窗口背景层**
    /// （身后桌面铺满整窗），这一项给**每一张行卡片**单独铺一层模糊的
    /// 身后内容，让卡片本身成为一块毛玻璃。
    ///
    /// # 为什么需要它
    ///
    /// 没有它时，`card_opacity` 调低只能透出**窗口自己的底色**
    /// （`list_bg`，比卡片更暗）⇒ 卡片看起来是「变暗」而不是「变透明」
    /// ——玻璃的关键不是能透多少，而是**透过来的是什么**。
    ///
    /// 默认 0（关）：多一次抓屏与纹理上传，不该默认付这个代价。
    #[serde(default = "default_card_blur")]
    pub card_blur: u8,
    /// 玻璃折射强度（0 ~ 1）。仅 `window_backdrop == Glass` 时生效。
    ///
    /// 审美项，无「正确值」：0 = 只剩磨砂（等同 Acrylic），1 = 边缘剧烈
    /// 扭曲。审美取向交给用户，故单独暴露。
    #[serde(default = "default_glass_refract")]
    pub glass_refract: f32,
    /// 色散强度（0 ~ 1）。0 = 关闭彩色描边。
    #[serde(default = "default_glass_dispersion")]
    pub glass_dispersion: f32,
    /// Fresnel 边缘光幂次（0.5 ~ 12）。
    #[serde(default = "default_glass_edge")]
    pub glass_edge: f32,
    /// 磨砂强度档位（1 ~ 10，默认 7）。仅在 `window_backdrop == Acrylic`
    /// 时生效。
    ///
    /// 映射到渲染层高斯卷积半径（见 `gfx::blur::radius_for_strength`）：
    /// 1 档最轻（半径 1）、10 档最重（半径 12），第 7 档对应历史固定
    /// 半径 8，因此旧用户升级后观感不变。
    #[serde(default = "default_window_blur_strength")]
    pub window_blur_strength: u8,
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
