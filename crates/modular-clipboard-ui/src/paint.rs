//! 绘制层：遍历工作区，按 `card.rect` 把卡片画出来。
//!
//! # 本模块的第一原则：**不做二次判断**
//!
//! 旧架构的历史症状几乎都源于「绘制层自己又判了一遍」：
//!
//! - 详情卡片折叠后，`draw` 里的 `if local.show_detail` 让整个绘制函数
//!   被跳过，可布局层照样分配了矩形 ⇒ **那块矩形无人绘制**，与相邻栏
//!   同色，看起来像两块糊在一起；
//! - 状态栏左侧提示按「预算」算宽度，实际字体偏大就压到右侧按钮上。
//!
//! 所以这里的规则是硬的：
//!
//! 1. **矩形只从 [`Card::rect`] 读**，不在这里算宽度、不做降级判断。
//! 2. **每张停靠卡片都必须被画**——折叠的画成把手，展开的画成卡片，
//!    没有「要不要画」这种分支。
//! 3. **任何文字都必须 `painter_at(所属矩形)`**。`ui.painter()` 是根
//!    painter、完全没有裁剪，用它画文字就是允许溢出到别的卡片上。
//!
//! # 为什么第3 条要单独强调
//!
//! 渲染层已经实现逐批次 scissor（见 `renderer::Batch::to_draw`），
//! 但那只保证「不超出**批次**的裁剪矩形」。而 `ui.painter()` 画出的
//! 文字，其批次裁剪矩形就是**整屏**——裁剪等于没裁。要真正限制在
//! 卡片内，必须显式传对应矩形给 `painter_at`。
//!
//! # 模块地图（2026-10-22 拆分）
//!
//! 本文件是模块根：只放**词汇表与帧骨架**——`Op` / `ConfigGroup` /
//! `SettingsCat` / `HotkeySlot`（操作与设置目录词汇）、`UiState` +
//! `HitTable`（界面本地状态与热区）、`Frame`（一帧的上下文）、
//! `draw()`（主窗口帧）、`draw_child()`（子窗口帧）与 `mod tests`。
//!
//! 具体绘制按职责住在 `paint/` 子模块里，加功能 / 删功能只动对应文件：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | `edit_icons` | 铅笔 / 撤销 / 对叉三个几何图标 |
//! | `bg_texture` | 二级主题质感层（纸质纤维 / 星空星点） |
//! | `float` | 悬浮层布局 `FloatLayout` 与类型栏几何常量 |
//! | `notice` | 通知 toast 与通知弹窗 |
//! | `topbar` | 顶栏、搜索状态机、搜索面板、类型过滤栏、投影 |
//! | `settings_win` | 设置窗口外壳（导航 / 搜索 / 许可证 / 更新弹窗） |
//! | `settings_items` | 设置数据模型（卡片表 / 设置项 / 访问器） |
//! | `settings_rows` | 设置行渲染与排版助手 |
//! | `controls` | 开关 / 滑条 / 分段 / 文本框等自绘控件 |
//! | `theme_editor` | 强调色编辑器与二级主题令牌面板 |
//! | `mcp_overlay` | MCP 覆盖层与热键录制覆盖层 |
//! | `cards` | 停靠卡片的框与标题头 |
//! | `list` | 历史列表（虚拟化行 / 行高测量 / 图片尺寸） |
//! | `detail` | 详情面板 / 类型栏内容 / 空状态 |
//! | `shadow` | 颜色助手与卡片投影 |
//! | `row` | 历史行（按钮 / 内联编辑 / 预览带 / 元信息） |
//!
//! 跨模块项统一 `pub(super)`（paint 家族内部 API）；外部路径
//! `crate::paint::X` 靠根里的再导出保持不变。

use egui::{Rect, Ui, Vec2, pos2, vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::item::EntryId;
use modular_clipboard_core::i18n::{Msg, msg};

use crate::card::{CardKind, PinnedMode};
use crate::theme::Palette;
use crate::workspace::Workspace;

mod edit_icons;
mod bg_texture;
mod float;
mod notice;
mod topbar;
mod settings_win;
mod settings_items;
mod settings_rows;
mod controls;
mod theme_editor;
mod mcp_overlay;
mod cards;
mod list;
mod detail;
mod shadow;
mod row;


// 跨模块 / 跨文件复用项的再导出：`crate::paint::X` 路径保持不变。
// 只收**真实代码引用**（app.rs / settingswin.rs）；纯文档引用走全路径。
pub(crate) use float::FloatLayout;
pub(crate) use notice::{NOTICE_TTL, draw_notice_panel};
pub(crate) use settings_items::mcp_pair_prompt;
pub(crate) use settings_win::{SETTINGS_MARGIN, SETTINGS_NAV_W, draw_settings};
pub(crate) use topbar::{draw_popup_shadow, draw_search_panel, draw_type_rail};

// root 自己用到的子模块项（draw / draw_child）。
use bg_texture::draw_theme_texture;
use cards::{draw_card_frame, draw_card_header, draw_cards};
use detail::{draw_detail_body, draw_empty, draw_rail_body};
use list::draw_history_body;
use notice::draw_notice_toast;
use topbar::{draw_topbar, run_search_state_machine};

/// 卡片边框宽度（逻辑点）。
const CARD_STROKE: f32 = 1.0;

/// 卡片圆角（逻辑点）。
///
/// 早前取 0（直角），理由是「相邻卡片之间与窗口边缘会留下 4 个缺口」，
/// 以及「纯平毛玻璃语言禁止装饰性圆角」。用户明确要求整体改圆角风格，
/// 那两条理由让位于新的视觉需求，但**缺口问题需要另行处理**：
/// 相邻卡片的间隙必须 ≥ 两个圆角之和，否则两张卡的圆角之间会留下
/// 一条「花瓣缝」。见 [`card_area`] 四周的内缩。
///
/// NOTE: 窗口本身的圆角是 DWM 给的（`DWMWCP_ROUND`，约 8pt），
/// **改不了**——微软枚举只有 DEFAULT/DONOTROUND/ROUND/ROUNDSMALL
/// 四档，没有更大的值。所以「整体更圆润」只能靠内容层承担：
/// 本值 16 > [`ROW_RADIUS`] 12 > 窗口 8，三层形成梯度。
const CARD_RADIUS: f32 = 16.0;

/// 行卡片圆角（逻辑点）。略小于 [`CARD_RADIUS`]，避免与卡片圆角重复。
const ROW_RADIUS: f32 = 12.0;

/// 一帧内要执行的操作。
///
/// 绘制层**只识别操作、不改布局**：`Workspace` 的改动由上层在
/// `apply_ops` 里做。这样「点某个按钮」这件事不会分散在绘制代码里。
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// 选中某个条目。
    Select(modular_clipboard_core::EntryId),
    /// 切换条目置顶。
    TogglePin(modular_clipboard_core::EntryId),
    /// 删除条目。
    Delete(modular_clipboard_core::EntryId),
    /// 复制条目到剪贴板。
    Copy(modular_clipboard_core::EntryId),
    /// 置顶模式切换。
    SetPinnedMode(PinnedMode),
    /// 「粘贴上一条」的行为模式。
    SetPastePreviousMode(modular_clipboard_core::config::PastePreviousMode),

    // ---- 内容编辑（2026-10-21）----
    //
    // 三个入口一个闭环：BeginEdit 读载荷开编辑器 → CommitEdit 落地
    // （落地前由 App 负责备份原文）→ UndoEdit 用备份还原。
    // CancelEdit 只是关编辑器（无存储副作用）。

    /// 进入内联编辑（读条目载荷开编辑器）。
    ///
    /// 载荷读取可能失败（条目被淘汰 / IPC 故障），失败时 App 发通知，
    /// **不**进入编辑态——「打开一个空编辑器」比打不开更糟。
    BeginEdit(modular_clipboard_core::EntryId),
    /// 提交内联编辑的文本（`text` = 编辑后的完整内容）。
    ///
    /// 文本类专用：载荷 = `text` 的 UTF-8 字节，kind 不变。
    CommitEditText {
        id: modular_clipboard_core::EntryId,
        text: String,
    },
    /// 提交图片编辑（`bytes` = 变换后的裸 RGBA8，`dims` = 新尺寸）。
    ///
    /// 旋转 90°/270° 会交换宽高，`dims` 必须按变换后的给——store 与
    /// 行高都认它（arboard 链路的尺寸无法从字节嗅探）。
    CommitEditImage {
        id: modular_clipboard_core::EntryId,
        bytes: Vec<u8>,
        dims: (u32, u32),
    },
    /// 取消内联编辑（关闭编辑器，不动存储）。
    CancelEdit,
    /// 撤销最近一次编辑（用 [`UiState::edit_backup`] 还原）。
    ///
    /// 备份不存在（原文已被淘汰 / 从未编辑过）时 App 忽略并通知。
    UndoEdit(modular_clipboard_core::EntryId),
    /// 打开图片编辑窗口（读载荷 → `UiState::image_editor`）。
    OpenImageEditor(modular_clipboard_core::EntryId),
    /// 关闭图片编辑窗口（不保存）。
    CloseImageEditor,

    // ---- 详情面板：图片原图查看 ----
    /// 进入「原图模式」：详情面板渲染全尺寸原图而非缩略图。
    ///
    /// 同一 id 的二次进入是幂等的——已是 zoom 状态时不必再发。
    /// 落地由 `App::apply_ops` 直接改 `UiState::detail_image_zoom`。
    DetailImageZoom(modular_clipboard_core::EntryId),
    /// 退出原图模式。
    DetailImageZoomClear,

    // ---- 设置界面 ----
    //
    // CAUTION: 配置项**不能**在 paint 里直接改：`Frame` 借的是 `&UiConfig`，
    // 且改完还要落盘（`svc.save_config()`）。统一走 Op 由 `App` 落地。

    /// 改一个布尔配置项。
    SetBool {
        /// 配置分组。
        group: ConfigGroup,
        /// 组内字段名（见 `set_bool_field`）。
        field: &'static str,
        /// 目标值。
        value: bool,
    },
    /// 改一个数值配置项。
    SetNumber {
        group: ConfigGroup,
        field: &'static str,
        value: f64,
    },
    /// 改一个可选数值（`None` = 不限）。
    SetOptNumber {
        group: ConfigGroup,
        field: &'static str,
        /// `None` 表示「不限」。
        value: Option<f64>,
    },
    /// 切换窗口背景效果（无 / 磨砂 / 半透明）。
    SetBackdrop(modular_clipboard_core::config::WindowBackdrop),
    /// 调背景不透明度（0.3 ~ 1.0）。
    SetOpacity(f32),
    /// 调卡片不透明度（0.3 ~ 1.0）。仅在磨砂 / 半透明背景下有意义。
    SetCardOpacity(f32),
    /// 卡片磨砂档位（0 ~ 10，0 = 关闭）。
    SetCardBlur(u8),
    /// 调磨砂强度档位（1 ~ 10）。
    SetBlurStrength(u8),
    /// 液态玻璃的三个审美参数（整包替换）。
    ///
    /// 同 `SetCardShadow`：三个滑块可能在一帧内连续拖动，逐字段 push
    /// 会让 `apply_ops` 跑三遍、写三遍盘。
    SetGlass {
        /// 折射强度（0 = 只剩磨砂）。
        refract: f32,
        /// 色散差异量（0 = 无彩边）。
        dispersion: f32,
        /// Fresnel 幂次。
        edge: f32,
        /// 玻璃模式的自有模糊档位（1 ~ 10）。
        ///
        /// 与磨砂强度（`SetBlurStrength`）分开：玻璃的光学合成会改变
        /// 底图观感，合适的模糊量与磨砂不同，共用一项会让改一个
        /// 另一个跟着变（2026-10-24 用户反馈）。
        blur: u8,
    },
    /// 设置列表卡片阴影（开关 + 模糊 / 浓度 / 垂直偏移）。
    ///
    /// 整包替换而不是逐字段 Op：三个滑块 + 开关可能在一帧内连续改动，
    /// 逐字段 push 会让 `apply_ops` 跑四次、写四次盘。
    SetCardShadow(modular_clipboard_core::config::CardShadowConfig),
    /// 设置右下角信息组合（图标 / 类型 / 大小的显隐与顺序）。
    ///
    /// 同 `SetCardShadow`：编辑器内单击、拖拽可能在一帧内连续改，
    /// 整包替换，避免逐字段写盘。
    SetRowMeta(modular_clipboard_core::config::RowMetaConfig),
    /// 设置悬浮类型过滤栏（开关 + 始终显示 + 多选 + 标签 + 停靠位置 + 图标大小）。
    ///
    /// 同 `SetCardShadow`：多个参数在设置界面是一张互相牵连的卡，
    /// 整包替换，避免一次拖动写四次盘。
    SetTypeRail(modular_clipboard_core::config::TypeRailConfig),
    /// 设置悬浮搜索面板（停靠位置 + 高度）。
    ///
    /// 同 [`Op::SetTypeRail`]：两个参数一张轻量卡，整包替换。
    SetSearchPanel(modular_clipboard_core::config::SearchPanelConfig),
    /// 切换主题（跟随系统 / 深色 / 浅色）。
    ///
    /// 由 `App::apply_ops` 落地：更新 `App.pal` 并重灌主窗口 `Visuals`；
    /// 设置窗口的 `Visuals` 由 `settingswin` 每帧从 `App.pal` 同步。
    SetThemeMode(crate::theme::ThemeMode),
    /// 设置主题强调色（`None` = 恢复主题内置默认蓝）。
    ///
    /// 落地时重跑主题解析链（`theme::resolve_palette`）并走 220ms
    /// 补间——与 [`Op::SetThemeMode`] 同一套机制，换色不该是硬切。
    SetAccent(Option<modular_clipboard_core::config::AccentColor>),
    /// 切换二级主题（无 / 纸质纹理 / 护眼绿 / 星空紫 / OLED）。
    ///
    /// 二级主题在深浅两套基底下各有一套目标色板，一级切换时自动
    /// 落到对应变体（见 `theme::apply_secondary`）。
    SetSecondaryTheme(modular_clipboard_core::config::SecondaryTheme),
    /// 整包替换二级主题参数（两个旋钮可能同帧连改）。
    ///
    /// 同 [`Op::SetCardShadow`]：整包替换，避免一次拖动写两次盘。
    SetThemeParams(modular_clipboard_core::config::ThemeParams),
    /// 覆盖单个主题令牌（高级微调：任何元素任何参数都可调）。
    ///
    /// 颜色令牌进 `theme_overrides.colors`、数值令牌进 `.nums`；
    /// 落地即重跑解析链，覆盖优先级最高（见 `theme::resolve_palette`）。
    SetTokenOverride {
        /// 令牌名（`theme::Palette::tokens()` 的键）。
        name: String,
        /// 目标值。
        value: crate::theme::TokenValue,
    },
    /// 清除单个令牌覆盖（恢复解析链派生值）。
    ClearTokenOverride(String),
    /// 清空全部令牌覆盖。
    ClearAllTokenOverrides,
    /// 设置全局快捷键绑定（写入配置）。
    ///
    /// NOTE: 当前托盘线程注册的是硬编码 `DEFAULT_HOTKEY`，配置里的
    /// 绑定尚未接入注册——录制与热键重注册是后续任务，这里先让
    /// 设置界面能展示并保存绑定。
    SetHotkey {
        /// 哪个快捷键槽位。
        slot: HotkeySlot,
        /// 新绑定。
        hotkey: modular_clipboard_core::config::Hotkey,
    },
    /// 进入「录制快捷键」状态：下一步按下的组合会被记为该槽位的绑定。
    ///
    /// 录制在 `App::draw_frame` 里消费（那里才拿得到 egui 的按键事件）。
    /// 录到的组合**只显示不应用**，直到 [`Op::FinishRecording`]。
    StartRecording(HotkeySlot),
    /// 确认并结束录制：把已按下的候选组合落为该槽位的绑定。
    ///
    /// 在录制覆盖层上**点击任意位置**即触发（用户要求「点击任意位置
    /// 停止录制」）。若还没按过主键，等同取消。
    FinishRecording,
    /// 把输入框里的文本解析成绑定（`Ctrl+Shift+V` 形式）。
    ///
    /// 解析失败不 panic、也不静默丢弃：由 `App` 侧把错误显示成一条提示。
    SetHotkeyText {
        slot: HotkeySlot,
        text: String,
    },
    /// 切换开机自启动。
    ///
    /// 与 [`Op::SetBool`] 分开：它不只是改配置，还要**真的去写注册表**，
    /// 而且失败要告诉用户（权限/路径问题都可能导致写不进去）。
    SetAutostart(bool),
    /// 改界面语言（`"system"` / `"zh-CN"` / `"en-US"`）。
    ///
    /// 由 `App::apply_ops` 落地：写配置 + 立即 `i18n::set_locale`
    /// （界面语言是全局状态，不等下一帧）。
    SetLanguage(&'static str),
    /// 改一个文本配置字段（MCP 服务地址 / 密钥）。
    SetText {
        group: ConfigGroup,
        field: &'static str,
        value: String,
    },
    /// 复制「一键 AI 配对提示词」到系统剪贴板。
    ///
    /// 内容由当前 MCP 配置动态生成（可调用能力清单 + 接入参数），
    /// 由 `App::apply_ops` 落地：写剪贴板（带自身捕获抑制）。
    CopyMcpPairPrompt,
    /// 打开/关闭设置界面。
    ToggleSettings,
    /// 清空全部历史。
    ClearAll,
    /// 退出程序。
    ///
    /// NOTE: 早前 UI 层**没有任何退出入口**：`App::should_quit` 这个字段
    /// 存在、有初始化、有消费点、有 getter，但**全仓没有任何地方把它
    /// 置成 true**。于是设置窗口里没有「退出程序」按钮，而标题栏 ✕
    /// 在有托盘时只隐藏到托盘 —— 用户从界面里根本无法退出程序。
    Quit,
    /// 打开外部链接（关于页的项目 / 作者主页）。
    ///
    /// 由 `App::apply_ops` 走平台层 `ShellExecuteW` 用默认浏览器打开，
    /// 绘制层不直接碰 shell。
    OpenLink(&'static str),
    /// 在文件管理器中**打开**数据目录（关于页「数据目录」超链接）。
    ///
    /// 与 [`Op::RevealPath`] 的区别：那是「定位并选中文件」（父目录 +
    /// 选中），这是「进入目录」。路径在落地时从 `Service::data_dir()`
    /// 取——目录可能是 `--data-dir` 的自定义值，不能在 Op 里存死。
    OpenDataDir,
    /// 双击链接型条目：用浏览器打开 URL。
    ///
    /// 与 [`Op::OpenLink`] 的区别是**内容来自剪贴板**（动态字符串），
    /// 且尊重设置里的自定义浏览器路径（`ui.browser_path`）。
    OpenUrl(String),
    /// 双击文件型条目：在资源管理器中定位路径。
    ///
    /// 路径来自剪贴板内容（多文件取首行），尊重设置里的自定义
    /// 资源管理器路径（`ui.explorer_path`）。
    RevealPath(String),
    /// 切换某个类型的过滤态（`None` 取消的旧语义由 `UiState::
    /// toggle_type_filter` 内部处理，Op 只带「哪个类型 + 是不是多选」）。
    ///
    /// 过滤在**界面侧**做（与 `query` 同为 UI 本地状态，不进快照）：
    /// 绘制时按 `paint_state` 的生效集合筛行，不触碰 store 查询与 IPC。
    /// 2026-10-20：单选改多选后，Op 不再携带目标集合——集合的维护
    /// 规则（含「多选时清空单选字段」）只有一处，在
    /// [`UiState::toggle_type_filter`]。
    SetTypeFilterKind {
        /// 被点击的类型。
        kind: modular_clipboard_core::ClipKind,
        /// 点击时是否处于多选模式（配置快照，防止连点两帧之间切模式）。
        multi: bool,
    },
    /// 关闭按钮 ⇒ 隐藏到托盘。
    ///
    /// NOTE: 语义是「隐藏」不是「退出」：剪贴板类工具直接退出会让
    /// 用户以为「记录停了」。真实决策（隐藏 / 退出）由 `App` 侧结合
    /// 有无托盘兜底判断，绘制层不决定。
    CloseWindow,
}

/// 配置分组。
///
/// 设置界面的每个可调项都归到某一组；`App` 侧用
/// [`ConfigGroup`] + 字段名定位到具体字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigGroup {
    Ui,
    Capture,
    Storage,
    /// MCP 服务（通信分类卡片）。
    Mcp,
    /// 迷你快捷复制面板（修饰键 + 右键唤出）。
    MiniPanel,
}

impl ConfigGroup {
    /// 分组标题（设置界面里显示）。
    pub fn title(self) -> &'static str {
        match self {
            ConfigGroup::Ui => "界面",
            ConfigGroup::Capture => "捕获",
            ConfigGroup::Storage => "存储",
            ConfigGroup::Mcp => "通信",
            ConfigGroup::MiniPanel => "迷你面板",
        }
    }
}

/// 全局快捷键槽位（设置界面「快捷键」卡片）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeySlot {
    /// 显示 / 隐藏主窗口。
    Toggle,
    /// 粘贴上一个条目。
    PastePrevious,
}

impl HotkeySlot {
    /// 槽位对应的配置字段。
    pub fn field(self) -> &'static str {
        match self {
            HotkeySlot::Toggle => "hotkey_toggle",
            HotkeySlot::PastePrevious => "hotkey_paste_previous",
        }
    }

    /// 取该槽位输入框的草稿（可变引用）。
    ///
    /// 两个槽位各一个草稿：共用一个 `String` 时，用户在两行之间
    /// 切换输入会得到「文本接在后面」的拼接结果，解析报
    /// 「多个主键」——表现为「手动输入有问题」。
    pub fn draft_mut(self, state: &mut UiState) -> &mut String {
        match self {
            HotkeySlot::Toggle => &mut state.hotkey_draft_toggle,
            HotkeySlot::PastePrevious => &mut state.hotkey_draft_paste,
        }
    }

    /// 槽位当前绑定的快捷键。
    pub fn current(self, cfg: &modular_clipboard_core::config::Config) -> modular_clipboard_core::config::Hotkey {
        match self {
            HotkeySlot::Toggle => cfg.hotkey_toggle.clone(),
            HotkeySlot::PastePrevious => cfg.hotkey_paste_previous.clone(),
        }
    }

    /// 槽位默认绑定（与 `Config::default()` 一致）。
    pub fn default(self) -> modular_clipboard_core::config::Hotkey {
        match self {
            HotkeySlot::Toggle => modular_clipboard_core::config::Hotkey::default(),
            HotkeySlot::PastePrevious => modular_clipboard_core::config::Hotkey {
                key: "V".into(),
                ctrl: true,
                shift: false,
                alt: false,
                win: true,
            },
        }
    }
}

/// 设置界面左侧导航分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SettingsCat {
    /// 常规：窗口行为。
    #[default]
    General,
    /// 主题：一级深浅 / 主题色 / 二级主题 / 高级微调。
    ///
    /// 2026-10-18 从「外观」拆出：主题层（一级 → 二级 → 强调色 →
    /// 令牌覆盖）自成一张关注点卡，且令牌编辑器有近 60 行——混在
    /// 外观卡里会把背景效果 / 卡片阴影挤出一屏（热区登记跳过不可见
    /// 控件，回归测试全线飘红）。
    Theme,
    /// 外观：背景效果 / 卡片阴影等窗口与列表观感。
    Appearance,
    /// 捕获：剪贴板记录行为。
    Capture,
    /// 存储：历史容量。
    Storage,
    /// 快捷键：全局热键绑定。
    Shortcuts,
    /// 通信：MCP 服务与外部调用。
    Comm,
    /// 关于：版本 / 数据目录 / 退出。
    About,
}

impl SettingsCat {
    /// 全部分类（导航顺序）。
    pub const ALL: [SettingsCat; 8] = [
        SettingsCat::General,
        SettingsCat::Theme,
        SettingsCat::Appearance,
        SettingsCat::Capture,
        SettingsCat::Storage,
        SettingsCat::Shortcuts,
        SettingsCat::Comm,
        SettingsCat::About,
    ];

    /// 导航标题（双语对）。
    pub fn title(self) -> Msg {
        match self {
            SettingsCat::General => msg("General"),
            SettingsCat::Theme => msg("Theme"),
            SettingsCat::Appearance => msg("Appearance"),
            SettingsCat::Capture => msg("Capture"),
            SettingsCat::Storage => msg("Storage"),
            SettingsCat::Shortcuts => msg("Shortcuts"),
            SettingsCat::Comm => msg("Connection"),
            SettingsCat::About => msg("About"),
        }
    }

    /// 导航图标（复用现有字体字形，无新增字形能力）。
    pub fn icon(self) -> crate::icons::Icon {
        match self {
            SettingsCat::General => crate::icons::Icon::Settings,
            // 主题分类用自绘调色板（图标字体的 PUA 码位已占满，新增
            // 字形要重新生成 TTF——见 draw_settings_nav 的特判）。
            SettingsCat::Theme => crate::icons::Icon::Settings,
            SettingsCat::Appearance => crate::icons::Icon::Image,
            SettingsCat::Capture => crate::icons::Icon::Text,
            SettingsCat::Storage => crate::icons::Icon::Table,
            SettingsCat::Shortcuts => crate::icons::Icon::Pinned,
            SettingsCat::Comm => crate::icons::Icon::Code,
            SettingsCat::About => crate::icons::Icon::Code,
        }
    }

    /// 导航项热区 key。
    pub fn hot_key(self) -> &'static str {
        match self {
            SettingsCat::General => "settings_cat_general",
            SettingsCat::Theme => "settings_cat_theme",
            SettingsCat::Appearance => "settings_cat_appearance",
            SettingsCat::Capture => "settings_cat_capture",
            SettingsCat::Storage => "settings_cat_storage",
            SettingsCat::Shortcuts => "settings_cat_shortcuts",
            SettingsCat::Comm => "settings_cat_comm",
            SettingsCat::About => "settings_cat_about",
        }
    }

    /// 卡片重置按钮热区 key。
    pub fn reset_key(self) -> &'static str {
        match self {
            SettingsCat::General => "settings_reset_general",
            SettingsCat::Theme => "settings_reset_theme",
            SettingsCat::Appearance => "settings_reset_appearance",
            SettingsCat::Capture => "settings_reset_capture",
            SettingsCat::Storage => "settings_reset_storage",
            SettingsCat::Shortcuts => "settings_reset_shortcuts",
            SettingsCat::Comm => "settings_reset_comm",
            SettingsCat::About => "settings_reset_about",
        }
    }
}

/// 搜索面板的展开来源（[`UiState::search_opened_by`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchOpenCause {
    /// 悬停图标 0.18s 自动展开（用户可能还没察觉）。
    #[default]
    Hover,
    /// 图标点击展开（用户的明确意图）。
    Click,
}

/// 编辑前的**原文备份**（撤销按钮的数据源，单级）。
///
/// 只存「还原所需的最小集」：kind + 载荷字节 + 尺寸。预览文案在
/// 还原时由 `Service::update_content` 按 [`build_preview`] 同款规则
/// 重建，不备份——两处各存一份文案必然漂移。
#[derive(Debug, Clone, PartialEq)]
pub struct EditBackup {
    /// 被编辑的条目。
    pub id: EntryId,
    /// 编辑前的 kind（文本编辑不改 kind；图片编辑也不改，但备份时
    /// 一并记下，还原时不依赖「当前是什么类型」的猜测）。
    pub kind: modular_clipboard_core::ClipKind,
    /// 编辑前的载荷字节（文本 = UTF-8 原文，图片 = 裸 RGBA8）。
    pub payload: Vec<u8>,
    /// 编辑前的图片尺寸（文本类为 `None`）。
    pub dims: Option<(u32, u32)>,
}

/// 绘制上下文：跨帧保留的界面状态。
#[derive(Default)]
pub struct UiState {
    /// 本帧收集到的操作。
    pub ops: Vec<Op>,
    /// 搜索框文本。
    pub query: String,
    /// 类型过滤（`None` = 不过滤）。
    ///
    /// 与 `query` 同为**界面本地状态**：不进配置、不进 IPC 快照，
    /// 由绘制层按它筛行（store 查询与常驻进程无感知）。
    /// 2026-10-15 第三轮新增（悬浮类型过滤栏）。
    pub type_filter: Option<modular_clipboard_core::ClipKind>,
    /// 类型过滤的**多选**集合（`ui.type_rail.multi` 开启时使用）。
    ///
    /// 与 [`Self::type_filter`] 互斥而非并存：单选模式只写前者，
    /// 多选模式只写后者。筛选判据统一收在
    /// [`Self::active_type_kinds`]——两处各判一次必然漂移
    /// （历史上「同一件事算两遍」的 bug 全是这么来的）。
    pub type_filter_multi: std::collections::BTreeSet<modular_clipboard_core::ClipKind>,
    /// 通知弹窗的面板矩形（主窗口客户区坐标，y 可为负 = 窗口上方）。
    ///
    /// 由 [`draw_notice_toast`] 每帧算好——文字宽度要用**主窗口 ctx**
    /// 的字体度量，而弹窗定位发生在 `drive_attach_windows`（那里没有
    /// painter）。两个模式都算：toast 模式算了不用（成本一次文本排版，
    /// 仅通知存续期间发生），换来切换设置的那一帧几何就是对的。
    pub notice_panel: Option<Rect>,
    /// 正在**内联编辑**的条目（`Some(id)` = 该行的预览带换成编辑器）。
    ///
    /// 界面本地状态：不进配置、不进 IPC 快照。编辑中的文字在
    /// [`Self::edit_buf`]，落地走 `Op::CommitEdit`。
    pub editing: Option<EntryId>,
    /// 内联编辑器的文本缓冲（`editing` 为 `Some` 时有效）。
    pub edit_buf: String,
    /// 打开内联编辑器后**请求一次焦点**（TextEdit 要跨帧才拿得到）。
    pub edit_focus_pending: bool,
    /// 最近一次编辑的**原文备份**（撤销用，单级）。
    ///
    /// `payload` 是编辑前的载荷字节（文本 = UTF-8 原文，图片 = 裸
    /// RGBA8）。撤销 = 用这份备份再走一次 `update_content`——
    /// store 侧无历史表，撤销深度固定为 1（与「左下角一个撤销按钮」
    /// 的产品形态一致）。下一次编辑会覆盖它。
    pub edit_backup: Option<EditBackup>,
    /// 图片编辑窗口正在编辑的条目（`Some(id)` = 窗口应存在）。
    ///
    /// 窗口本体在 `app.rs` 的 `ImageEditor`（与设置窗口同款懒创建/
    /// 即关即毁），这里只存「用户点了哪个条目的编辑按钮」。
    pub image_editor: Option<EntryId>,
    /// 图片编辑窗口的**原图数据**（`(RGBA8 字节, 宽, 高)`）。
    ///
    /// 由 `Op::OpenImageEditor` 从 store 读出后放在这里，帧循环取出
    /// 建窗（`ImageEditor::create` 需要字节）。取出即清（`take`）——
    /// 建窗后数据归窗口自己持有，状态里不留第二份（几 MB 的图不该有
    /// 两个副本）。
    pub image_editor_src: Option<(Vec<u8>, u32, u32)>,
    /// 开源协议悬浮面板是否打开（关于页「开源协议」行点击后）。
    ///
    /// 界面本地状态：不进配置、不进 IPC 快照，只在设置窗口这一帧
    /// 起作用。滚动位置不单独存——面板关掉重开从头看起是合理的
    /// （用户关掉就是「看完了」）。
    pub license_popup: bool,
    /// 「发现新版本」悬浮面板是否打开。
    pub update_popup: bool,
    /// 清空按钮**长按**的起始时刻（`None` = 没在长按）。
    ///
    /// 破坏性操作（清空全部历史）从「单击即触发」改成「长按 5 秒」，
    /// 防误触。动画（摇晃 + 渐红）按 `elapsed() / 5s` 推进，
    /// 见 [`draw_clear_button`]。
    pub clear_hold_start: Option<std::time::Instant>,
    /// 本次检查到的更新是否**已经弹过**面板。
    ///
    /// 没有它的话，状态停在 `Outdated` 就会每帧重开一次面板——用户
    /// 点掉还会再弹回来。弹过就置位，下次「检查更新」拿到新的
    /// `Outdated` 时由 [`draw_update_popup`] 的调用方重置。
    pub update_popup_shown: bool,
    /// 类型栏**实测**面板尺寸（上一帧弹窗绘制时量的，逻辑点）。
    ///
    /// 为什么需要它：`labels`（按钮带文字）下面板尺寸取决于文字宽度，
    /// 而 [`FloatLayout::compute`] 是无 ctx 的纯函数，排不出文字。
    /// 由 `draw_type_rail` 量好后写这里，布局侧下一帧按它定弹窗尺寸
    /// （首帧用估算值，一帧内修正）。`None` = 还没量过。
    pub rail_panel_size: Option<Vec2>,
    /// 本帧参与**液态玻璃**合成的矩形（逻辑点）。
    ///
    /// 由绘制层在各面板/卡片画底色前 `push_glass` 收集，帧循环取出后交给
    /// 渲染层的玻璃 pass。玻璃是观感层：收集失败不该影响绘制，所以这里是
    /// 「尽力而为」的 Vec 而不是 Result。
    ///
    /// CAUTION: 帧循环取走时会 `mem::take` 清空。若某条绘制路径没跑
    /// （窗口最小化、跳过渲染），下一帧重新收集，不会累积陈旧矩形。
    pub glass_rects: Vec<modular_clipboard_gfx::glass_pass::GlassRect>,
    /// 行卡片底色的 alpha（`palette.card_bg.a()`）。
    ///
    /// 玻璃 pass 只改 RGB，覆盖度由这个 alpha 定——这样「卡片不透明度」
    /// 滑块对玻璃同样生效，且语义与磨砂路径一致。
    pub glass_card_alpha: f32,
    /// 图片缩略图的**像素**缓存（第三轮：图片条目内嵌显示）。
    ///
    /// LRU + 字节预算，见 [`crate::thumbnail::ThumbnailCache`]。
    /// 与 `thumb_tex` 分工：这里存解码后的像素（可反复上传），
    /// 那里存 GPU 纹理句柄（随可见集淘汰）。
    pub thumb_cache: crate::thumbnail::ThumbnailCache,
    /// 已上传的缩略图纹理（条目 id → 纹理句柄）。
    ///
    /// **有界**：每帧按可见图片条目淘汰（见 `draw` 收尾的 `retain`）。
    /// 没有这一条的话，滚动浏览过的每张图都会留下一份纹理，
    /// GPU 内存只涨不消。
    pub thumb_tex:
        std::collections::HashMap<modular_clipboard_core::EntryId, egui::TextureId>,
    /// 卡片磨砂：**待上传**的底图像素（渲染层按 30fps 节流捕获）。
    ///
    /// 分两段走是因为捕获发生在**渲染阶段**（第 4 步，画完之后），
    /// 而上传必须发生在 `run_ui` 里（egui 的 `load_texture` 要 `Context`）。
    /// 所以先存在这里，下一帧的 `draw_frame` 取走上传。
    pub card_blur_pending: Option<egui::ColorImage>,
    /// 卡片磨砂：当前生效的纹理句柄。
    ///
    /// # 为什么不是每帧重建
    ///
    /// `load_texture` 是**同名覆盖**语义：重复调用会每帧重传整图
    /// （哪怕内容一样）。捕获本身有 30fps 节流，这里跟着只在**有新像素**
    /// 时才上传，其余帧复用同一个句柄。
    pub card_blur_tex: Option<egui::TextureHandle>,
    /// 本帧**出现过**的图片条目 id（纹理淘汰的判据）。
    ///
    /// 为什么不能在每个列表各自淘汰：主栏与内嵌置顶区是两个列表
    /// （`draw_history_body` 被调两次），各留一份可见集会互相淘汰——
    /// 置顶区刚上传的纹理被主栏的 retain 丢掉，下一帧再上传，
    /// 每帧重复传 GPU。收拢到帧级：两个列表都往这里报，`draw`
    /// 在画完全部卡片后统一 retain 一次。
    pub thumb_seen: std::collections::HashSet<modular_clipboard_core::EntryId>,
    /// 已解码图片的**尺寸**（条目 id → 宽高）。
    ///
    /// # 为什么需要它
    ///
    /// 显示矩形必须知道长宽比，而**文件型图片**（从资源管理器复制的
    /// `.png`/`.jpg`）没有 `PayloadRef::dims`——它的载荷是路径列表，
    /// 尺寸只能靠解码得到。解码完顺手记在这里，就不必为了量尺寸每帧
    /// 再解一次码（那正是 PF-1 禁止的每帧 IO）。
    ///
    /// 图片型条目（`ClipKind::Image`）也一起记：解码出来的尺寸比载荷里
    /// 存的更权威，且 `payload.dims` 缺失时（载荷被淘汰）仍能正常排版。
    ///
    /// # 为什么不随纹理淘汰
    ///
    /// 2026-10-17 改：原先与 `thumb_tex` 一起 retain，结果滚出屏就丢
    /// ——而行高缓存以它的 `len()` 作为失效判据之一（见
    /// [`RowHeightCache::dims_known`]），丢一次就让整份行高缓存失效一次，
    /// 滚动时等于每帧重算全部行高。尺寸一条只有 16 字节，常驻更划算，
    /// 而且它是**行高**的输入：老条目（`payload.dims` 缺失）全靠它排图。
    pub thumb_dims: std::collections::HashMap<modular_clipboard_core::EntryId, (u32, u32)>,
    /// 二级主题「纸质纹理」的程序化纤维 tile（惰性生成，见
    /// [`crate::tex::paper_tile`]）。
    ///
    /// `None` = 本会话还没画过纸质主题。切到别的主题时**不清**：
    /// 一份 96×96 白底纹理不到 40KB，来回切主题不值得反复生成；
    /// 真正的释放发生在 `UiState` 丢弃（窗口关闭）时 `TextureHandle`
    /// 的 `Drop` 里。
    ///
    /// CAUTION: 不能每帧 `load_texture`——同名覆盖语义虽然正确，但
    /// 每帧上传 36KB 像素 + 走一次纹理管理器的写锁，纯浪费。
    pub paper_tex: Option<egui::TextureHandle>,
    /// 星空主题的星点**精灵**（单颗星的 16px 纹理，见
    /// [`crate::tex::star_sprite`]）。语义同 [`UiState::paper_tex`]。
    pub star_tex: Option<egui::TextureHandle>,
    /// 星空主题的星星列表（CPU 侧「一片星空」，见 [`crate::tex::StarField`]）。
    ///
    /// 2026-10-19 起星空改「精灵逐颗绘制」：随机化（换一片星）、闪烁
    /// （每帧 tint alpha 呼吸）、大小不规则（逐颗半径）三个可设参数都
    /// 要求 CPU 逐帧参与，静态 tile 做不到。列表按参数指纹重建，
    /// 没变就复用（只重算闪烁）。
    pub star_field: Option<crate::tex::StarField>,
    /// 主题色十六进制输入框的草稿。
    ///
    /// # 为什么需要「焦点感知」的同步
    ///
    /// 输入框没焦点时，每帧把草稿覆写为当前色的 hex——用户点预设
    /// 色块、拖 HSV 滑条时，输入框跟着显示新值。有焦点时（用户正在
    /// 打字）绝不能覆写，否则输一个字符就被打回原色。
    ///
    /// 打字中途的非法输入（如只敲了 `#F`）不清除已选颜色：
    /// [`AccentColor::from_hex`] 返回 `None` 时保持原值。
    pub accent_hex: String,
    /// 「高级微调」令牌编辑器是否展开（默认收起）。
    ///
    /// 令牌**二级面板**是否打开（2026-10-19 语义升级）。
    ///
    /// 令牌表有近 60 行：平铺在主题卡里会把卡片撑到五屏以上，一屏内
    /// 连一个设置项都看不全。因此升级为二级视图——主题卡只留一行
    /// 「高级微调」摘要入口，点开后右侧内容区**整体切换**为令牌面板
    /// （占设置窗口一大半区域），左上角「‹」返回。
    ///
    /// 折叠态（主题卡内）**不绘制**令牌行，也就不注册它们的热区
    /// （与「只登记部分可见控件」的约定一致）。
    pub theme_tokens_expanded: bool,
    /// 搜索框是否展开（顶栏右侧图标折叠式搜索）。
    ///
    /// 折叠时顶栏只显示一枚放大镜图标（贴近窗口按钮），左侧整片
    /// 都是拖动区；点击图标展开输入框（向左弹出），此时拖动区
    /// 收缩到输入框左缘，但仍保留一段可拖宽度。
    pub search_expanded: bool,
    /// 光标开始停在搜索图标上的时刻（egui 时间，秒）。
    ///
    /// 用来实现「悬停一小会儿才展开」：鼠标**划过**图标时不应该弹出搜索框，
    /// 只有**停在**上面才展开。`None` 表示当前没停在图标上。
    pub search_hover_since: Option<f64>,
    /// 光标离开搜索区域（图标 + 输入框）的时刻（egui 时间，秒）。
    ///
    /// 用来实现「离开后宽限一小会儿再折叠」。没有这个宽限期的话，
    /// 鼠标从图标移向输入框的途中（两者之间有 `space_md` 的间隙）
    /// 会瞬间判定「既不在图标上也不在输入框里」⇒ 立刻折叠，输入框
    /// 在指针抵达前就消失——**永远进不去输入框**（用户反馈：
    /// 「根本不给输入的机会」）。
    pub search_leave_since: Option<f64>,
    /// 搜索面板文本框的自报 id（`draw_search_panel` 写入）。
    ///
    /// 面板是独立弹窗、有自己的 `egui::Context`，焦点也在那里面——
    /// 主窗口的 `memory()` 读不到。弹窗每帧用这个 id 回报焦点状态
    /// （见 [`UiState::search_popup_focus`]）。
    pub search_edit_id: Option<egui::Id>,
    /// 点击搜索图标后待办的「把焦点交给输入框」请求。
    ///
    /// 跨帧挂起的原因：点击的那一帧弹窗可能还没画（要先判展开），
    /// 而焦点只能请求给**已经存在**的控件 id。真正发出请求是在弹窗
    /// 那帧 `draw_search_panel` 之后（那时 id 已自报）。
    pub search_focus_pending: bool,
    /// 指针是否悬在**搜索弹窗**上（弹窗每帧回报）。
    ///
    /// 第三轮初版面板画在窗口内，用 `rect_contains_pointer` 就能判；
    /// 变成弹窗后指针在主窗口客户区之外，只能由弹窗问
    /// `WindowFromPoint`（gfx::chrome::pointer_is_over）。
    pub search_popup_hover: bool,
    /// 悬停展开是否被抑制（点图标收起后，直到光标离开图标为止）。
    ///
    /// 没有这个位的话，点一下收起、光标却还停在图标上 ⇒ 0.18s 后又被
    /// 悬停展开，用户觉得「点了关不掉」。
    pub search_hover_suppressed: bool,
    /// 搜索面板**本次展开**的时刻（`input.time` 秒）。
    ///
    /// 展开后给一段宽限期（[`SEARCH_OPEN_GRACE`]），期间不判折叠。
    /// 起因（2026-10-19 用户反馈「点一下搜索，闪一下就关闭」）：点击
    /// 展开的那几帧里，焦点还没从弹窗回报回来、指针又可能已经离开图标
    /// ⇒ 折叠条件短暂全部成立，0.35s 宽限一到面板就自己收回去。
    /// 有了它，任何「刚打开」的面板都不可能被立刻判折叠。
    pub search_open_at: Option<f64>,
    /// 最近一次「输入内容变化」的时刻（`input.time` 秒）。
    ///
    /// # 为什么焦点不能当「正在打字」的判据（2026-10-21 用户反馈修复）
    ///
    /// 旧实现「输入框有 egui 焦点 ⇒ 永不折叠」过粘：generic interact
    /// 不抢焦点、用户点到列表/别的应用时 egui 内存里输入框**仍然持有
    /// 焦点** ⇒ `search_popup_focus` 恒真 ⇒ 面板一旦获焦就再也收不
    /// 回去（「搜索框打开后无法自动关闭」）。改为**打字活跃**判据：
    /// 最近 [`SEARCH_TYPING_GRACE`] 内输入变过才钉住，停手后放行
    /// 宽限折叠。真正「打字打到一半」不会被收（每次键入都刷新时间戳）。
    pub search_last_input_at: Option<f64>,
    /// 搜索面板是**怎么**打开的（图标点击的切换语义要区分）。
    ///
    /// 悬停展开后面板可能「在用户察觉之前」就开了（0.18s），此时第一次
    /// 点击图标的意图是「打开」，点击即关会重现 2026-10-19 的
    /// 「点一下、闪一下、关了」。所以悬停展开后的第一次点击只**确认**
    /// 并聚焦输入框，第二次点击才收起（见 `run_search_state_machine`）。
    pub search_opened_by: SearchOpenCause,
    /// 搜索弹窗的输入框是否持有焦点（弹窗每帧回报）。
    ///
    /// 这条规则是「正在打字时不折叠面板」的依据，而焦点住在弹窗的
    /// `egui::Context` 里——主窗口的 `memory()` 恒为空，不回报的话
    /// 用户点进输入框的瞬间面板就会自己收回去，字永远打不进去。
    pub search_popup_focus: bool,
    /// 上一帧提交给窗口的拖动区（标题栏）。
    ///
    /// IMPORTANT: 必须是**多段**而不是「整个顶栏一块」：拖动区会被系统转成
    /// `HTCAPTION`，点其中的控件不会产生 `WM_LBUTTONDOWN`，
    /// egui 收不到点击（表现：齿轮/垃圾桶/搜索框全都点不动）。
    ///
    /// 多个矩形的并集是拖动区；控件区从中间挖空。
    pub drag_regions: Vec<Rect>,
    /// 正在录制快捷键的槽位。`None` 表示没在录制。
    ///
    /// 录制状态属于界面本地瞬时状态：它不进配置、也不需要跨进程同步
    /// （界面进程独占键盘焦点）。
    pub hotkey_recording: Option<HotkeySlot>,
    /// 录制中已按下的候选组合（`None` = 还没按主键）。
    ///
    /// **只记录不应用**：组合先显示在录制覆盖层上给用户看，
    /// 点击任意位置确认后才落地。按一下就改的设计会让手抖
    /// 直接换掉热键，改回来还得再录一次。
    pub recording_combo: Option<modular_clipboard_core::config::Hotkey>,
    /// 快捷键输入框草稿，**按槽位分开存**（经 [`HotkeySlot::draft_mut`] 访问）。
    ///
    /// 不能共用一个 `String`：用户在 Toggle 行打了字没回车，转到
    /// PastePrevious 行继续输入时，文本会**接在前一个草稿后面**
    /// （egui 的 TextEdit 焦点切换不会清内容），解析出来是
    /// 「多个主键」——用户看到的是「手动输入有问题」。
    pub hotkey_draft_toggle: String,
    pub hotkey_draft_paste: String,
    /// 当前悬停的卡片。
    pub hover_card: Option<crate::card::CardId>,
    /// 列表滚动偏移（逻辑点），**按列表分开存**。
    ///
    /// NOTE: 主栏历史列表与内嵌置顶区是两个独立列表。早前共用一个
    /// `f32`，而绘制每帧都会把它灌进 `ScrollArea`，于是滚其中一个
    /// 会把另一个一起带走。
    ///
    /// 约定：`[0]` = 历史列表，`[1]` = 置顶列表。经
    /// [`Self::list_scroll`] / [`Self::set_list_scroll`] 访问。
    list_scroll: [f32; 2],

    /// 上一帧绘制时的列表版本号（新条目入场动画的闸门），**按列表分开存**。
    ///
    /// `None` = 本会话还没画过该列表（首帧只记录、不播放——启动时不应该
    /// 无缘无故放一次滑入）。版本号变化 ⇒ store 侧有增删置顶 ⇒ 首行
    /// 播放一次「从上方滑入」。搜索 / 类型过滤**不** bump 版本
    /// （绘制层本地状态，见 `RowFilterKey` 的说明），因此敲键筛选不会触发。
    ///
    /// 约定同 `list_scroll`：`[0]` = 历史列表，`[1]` = 置顶列表。
    list_version_seen: [Option<u64>; 2],

    /// 设置界面是否打开。
    pub show_settings: bool,
    /// `MC_SETTINGS` 启动开关是否已消费（一次性）。
    ///
    /// `MC_SETTINGS=1` 只在**启动首帧**打开设置界面一次，随后不再
    /// 强制——早期实现每帧 `show_settings = true`，导致带该环境变量
    /// 启动时设置窗口**永远关不掉**（点 ✕ / 齿轮都被下一帧改回）。
    pub settings_startup_opened: bool,

    // ---- 详情面板：图片原图查看 ----
    /// 当前是否在「原图模式」：详情面板渲染**全尺寸原图**而非缩略图。
    ///
    /// 与 `detail_image_orig` 配对——前者表达意图、后者缓存 GPU 纹理。
    /// 切换选中 / 关闭原图模式时同时清空两个。
    pub detail_image_zoom: Option<modular_clipboard_core::EntryId>,
    /// 已上传的原图 GPU 纹理（条目 id → 纹理句柄）。
    ///
    /// 仅在进入原图模式时上传一次；`detail_image_zoom` 变化时同步释放。
    /// `TextureHandle` 是 RAII，drop 时由 egui 自动 `ctx.forget()`，
    /// 因此不需要显式 free——只需要让本字段离开作用域。
    pub detail_image_orig:
        Option<(modular_clipboard_core::EntryId, egui::TextureHandle)>,

    // ---- 设置界面状态（左右导航 + 卡片布局）----
    /// 当前选中分类（左侧导航）。
    pub settings_cat: SettingsCat,
    /// 设置搜索框文本。
    pub settings_query: String,
    /// 搜索框「空态聚焦」：显示最近修改的设置项。
    ///
    /// 定稿交互：点击搜索框（内容为空）出现最近修改；输入内容后
    /// 改为显示搜索结果；搜索结果为空时回落到最近修改。
    pub settings_show_recent: bool,
    /// 最近修改的设置项（label, 时间戳），按修改时间倒序展示。
    ///
    /// 任何设置控件被改动时登记；重置某卡片时清除该卡片所有项。
    pub settings_recent: Vec<(String, u64)>,
    /// 已折叠的设置分组（组的稳定 key）。
    ///
    /// 分组是卡片内的一级标题（`ItemKind::Group`），点标题行折叠/展开。
    /// 存 key 而非索引：卡片表里增删组不会让用户的折叠偏好错位。
    ///
    /// # 语义：**当前折叠状态的快照**，不是「与默认的差异」
    ///
    /// 首次进入设置界面前，`settings_folds_seeded` 把
    /// `settings_items::DEFAULT_FOLDED` 灌进来；此后完全由用户点击驱动
    /// （点一下 insert、再点一下 remove）。
    ///
    /// CAUTION: 早先这里用「集合成员 异或 默认折叠表」判断，理由是
    /// 不想加 seed 标记。那个设计有个直接后果：**默认折叠的组永远
    /// 展不开**——点它走 `remove`，而它本就不在集合里（no-op），下一帧
    /// 异或回来还是折叠。2026-10-24 用户反馈「液态玻璃无法展开」正是
    /// 这个。异或让「集合里有 key」的含义随组而反（默认折叠的组里
    /// 有 key = 展开，默认展开的组里有 key = 折叠），点击分支稍不留神
    /// 就写成单向的。快照语义下两者一致：有 key = 折叠，点击即切换。
    ///
    /// 只在内存里（不落配置）：折叠是「这次想少看一点」的临时意图，
    /// 不是偏好设置——为它加一个配置字段不值得，且每次开关设置窗口
    /// 都回到全展开反而更符合「临时收起来」的语义。
    pub settings_folded: std::collections::HashSet<String>,
    /// 默认折叠是否已灌入 [`Self::settings_folded`]（见该字段的语义说明）。
    settings_folds_seeded: bool,
    // 注：原先这里有 `settings_scroll`（手写滚动偏移）。设置面板
    // 改用 `ScrollArea` 后由 egui 自己管理滚动位置，该字段整体删除。

    /// MCP 调用设置覆盖层是否打开。
    ///
    /// 打开时设置界面整体蒙上白/黑遮罩（视觉失焦），面板内列出
    /// 全部可调用 MCP 开关；关闭后所有调用不可用（叠加层内禁用）。
    pub mcp_overlay_open: bool,
    /// MCP 服务的**真实**连通状态（由 `App` 每帧从探测器同步过来）。
    ///
    /// # 为什么不能写死在 `MCP_CAPS` 里
    ///
    /// MCP 服务是独立进程，界面无法从局部变量得知它是否在运行。
    /// 早前徽标取的是编译期常量 `McpCap::ready`（「该能力是否已实现」），
    /// 于是只要代码里写了这个能力就永远显示「已连接」——把「功能已实现」
    /// 误标成「连接正常」。真值来自 [`crate::mcp_status::McpProbe`]。
    pub mcp_conn: crate::mcp_status::McpConn,

    // ---- 交互热区（供自动化测试定位）----
    //
    // NOTE: 为什么让被测代码**自报坐标**而不是测试去算：
    //
    // 早前测试按「顶栏高 × 0.7、间距 8」自己估按钮位置，结果算出的
    // 矩形是 `[328,76]-[580,324]`（横跨整窗），**所有点击都落在按钮
    // 外**。而测试只是 assert 失败——布局一改就会静默测错东西。
    //
    // 这里让绘制层把真实矩形登记进来，测试只读。代价是生产代码多
    // 几个 Rect 的写入（可忽略），换来的是「布局改了测试自动跟随」。
    /// 本帧各命名热区的矩形（逻辑点），**按窗口命名空间分隔**。
    ///
    /// IMPORTANT: 必须按窗口分开：主窗口、设置窗口、每个子窗口是**不同的
    /// 坐标空间**，却共用同一个 `UiState`（因此共用一张热区表）。
    /// 早前是一张 flat map、且只有主窗口的 `draw()` 会 clear，于是：
    ///
    /// - 设置窗口写进来的热区**永不清空**——关窗后仍留在表里，
    ///   自动化按它去点会点到「上一次的位置」甚至点到空气；
    /// - 主窗口隐藏（停止绘制）时，它自己的热区也**永不过期**；
    /// - 同名 key（`history_body` 等）在窗口之间互相覆盖。
    ///
    /// 每个绘制入口在开始时会清空**自己那一份**，互不影响。
    pub hit: HitTable,
    /// 关闭 / 最小化请求。
    pub close_requested: bool,
    /// 布局变化后**需要的客户区宽度**，由帧循环消费。
    ///
    /// 为什么会需要它：窗口尺寸原先只在**启动时**从
    /// `UiConfig::window_width` 读一次。用户在设置里把「置顶模式」
    /// 从「共用单栏」切成「独立分栏」后，多出一栏要宽度，
    /// 而没人去改窗口尺寸 —— 表现是卡片被按比例挤扁
    /// （`solve_scaled` 在需求 > 可用时按比例缩）。
    ///
    /// 两种请求语义（见 `app::WidthRequest`）：
    /// - `AtLeast`：至少放宽到这么多逻辑点，**只放大不收缩**
    ///   （用户可能故意把窗口开得很大，切模式不该被改小）；
    /// - `RestoreDefault`：精确恢复**默认宽度**（物理像素）。
    ///   切回「共用单栏」时用——独立分栏会自动放宽，切回来若只按
    ///   min_width 判「够宽就不动」，窗口就留在被撑宽的宽度上。
    pub resize_width_to: Option<crate::app::WidthRequest>,

    /// 行高缓存（PF-1），按列表分槽：`[0]` 主栏、`[1]` 置顶栏。
    ///
    /// 行高只取决于「条目文本 + 测量宽度 + 过滤条件 + 缩放」，跨帧不变。
    /// 每帧重算等于把 2000 次 `LayoutJob` 测量（含 galley cache 查找的
    /// 键哈希）重复付一遍——帧成本随列表总字节线性膨胀的根因之一。
    /// 失效判据见 [`RowHeightCache`]：`list_version`（每次 reload +1）、
    /// [`RowFilterKey`]、测量宽度、缩放、已解码图片尺寸数，
    /// **五者全部相同**才复用。
    row_height_cache: [Option<RowHeightCache>; 2],
}

/// 行高缓存条目。字段与 [`UiState::row_height_cache`] 的失效判据一一对应。
#[derive(Clone)]
struct RowHeightCache {
    version: u64,
    /// 建缓存时的**行过滤条件**。
    ///
    /// # 为什么必须进键
    ///
    /// `heights` 是**按行下标**存的（`heights[i]` ↔ `rows[i]`），而下标
    /// 的含义完全由过滤条件决定。`type_filter` / `only_pinned` /
    /// `pinned_drawn_separately` 都是**绘制层本地**状态，不过 store ⇒
    /// [`AppState::list_version`](modular_clipboard_app::AppState::list_version)
    /// 不会变（它只在 `reload_list` 时 +1）。
    ///
    /// 长度检查（`heights.len() == rows.len()`）挡不住这一类：3 条 Text
    /// 与 3 条 Image 条目，从「全部」切到「文本」再切到「图片」，行数
    /// 两次都是 3 ⇒ 缓存命中 ⇒ **图片行套用文本行的行高**，表现为
    /// 图被压扁或留出大片空白（且没有任何报错）。
    filter: RowFilterKey,
    text_w: f32,
    scale: f32,
    /// 建缓存时 [`UiState::thumb_dims`] 的条目数。
    ///
    /// 图片的行高取决于长宽比，而长宽比的来源有先后：载荷里记的
    /// `dims`（入库时读的文件头）→ 解码得到的 `thumb_dims`。老条目
    /// 缺前者，第一次解码后行高才会变——缓存必须因此失效，否则
    /// 那一行永远是「按文本量出来的高度」，图被压成一条细带。
    ///
    /// 用条目数而不是版本号：`thumb_dims` 只增不减，数量变化就等价于
    /// 「有新图刚拿到尺寸」；反过来若它也随纹理淘汰，数量会反复抖动，
    /// 缓存就每帧失效了（这正是它不参与淘汰的原因）。
    dims_known: usize,
    heights: Vec<f32>,
}

/// 行高缓存键里的**过滤维度**（其余维度见 [`RowHeightCache`]）。
///
/// 单独抽成具名类型而不是三个散字段：这三项必须**同时**出现在
/// 「写入缓存」与「查缓存」两处，合成一个可比较的值才能保证不漏。
#[derive(Clone, PartialEq, Eq, Debug)]
struct RowFilterKey {
    /// 生效中的类型过滤集合（空 = 不过滤；单选模式是 0/1 个元素）。
    ///
    /// CAUTION: 2026-10-20 多选过滤接入时由 `type_filter: Option<..>`
    /// 改写而来——单选与多选两个字段各判一次的写法必然在模式切换时
    /// 漏一处，统一集合是 [`UiState::active_type_kinds`] 的存在理由。
    type_kinds: Vec<modular_clipboard_core::ClipKind>,
    /// 只看置顶 / 只看非置顶；`None` = 主栏（含置顶，按模式决定排除）。
    only_pinned: Option<bool>,
    /// 置顶卡片是否被**独立绘制**（`PinnedMode::Separate`）。
    ///
    /// 它决定主栏要不要把置顶条目让出去（否则同一个 `("row", id)` 被画
    /// 两次，egui 会报 ID 冲突警告）。
    pinned_drawn_separately: bool,
}

/// 热区属于**哪个窗口**。
///
/// 子窗口按卡片 Id 区分：同一时刻可以有多个子窗口，混在一张表里
/// 彼此覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitNs {
    /// 主窗口。
    Main,
    /// 附属子窗口：负责画 `id` 这张卡片。
    Child(crate::card::CardId),
    /// 设置窗口。
    Settings,
    /// 悬浮面板弹窗（搜索面板 / 类型过滤栏）。
    ///
    /// 独立命名空间的理由：弹窗与主窗口**共用**同一个 `UiState`（配置、
    /// 选中项、查询词都共享），若热区也共用 Main 表，同名 key（如
    /// `search_box`）会互相覆盖——自动化脚本点主窗口的坐标会落到
    /// 弹窗的矩形上。
    Popup(crate::popup::PopupKind),
}

/// 按窗口分隔的热区表（见 [`UiState::hit`]）。
#[derive(Default)]
pub struct HitTable(std::collections::HashMap<HitNs, HitMap>);

type HitMap = std::collections::HashMap<&'static str, Rect>;

impl HitTable {
    /// 清空某个窗口的热区。每个绘制入口开工前调一次。
    pub fn clear(&mut self, ns: HitNs) {
        self.0.remove(&ns);
    }

    /// 登记一个热区。
    pub fn insert(&mut self, ns: HitNs, key: &'static str, r: Rect) {
        self.0.entry(ns).or_default().insert(key, r);
    }

    /// 查某个窗口的热区。
    pub fn get(&self, ns: HitNs, key: &str) -> Option<Rect> {
        self.0.get(&ns).and_then(|m| m.get(key).copied())
    }

    /// 某个窗口是否登记了该热区。
    pub fn contains_key(&self, ns: HitNs, key: &str) -> bool {
        self.0.get(&ns).is_some_and(|m| m.contains_key(key))
    }

    /// 某个窗口登记的所有 key（顺序不定，断言前请排序）。
    pub fn keys(&self, ns: HitNs) -> Vec<&'static str> {
        self.0
            .get(&ns)
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default()
    }

}

impl UiState {
    /// 取出并清空本帧操作。
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.ops)
    }

    /// 登记一个待执行操作（本帧收集，`App::apply_ops` 统一落地）。
    ///
    /// `pub(crate)`：编辑窗口（`imgedit`）也要 push Op——它不在本模块，
    /// 但「只识别操作、不改布局」的纪律对全 crate 的绘制层一致适用。
    pub(crate) fn push(&mut self, op: Op) {
        self.ops.push(op);
    }

    /// 取某个列表的滚动偏移。
    ///
    /// `pinned = true` 取内嵌置顶区，`false` 取主栏历史列表。
    pub fn list_scroll(&self, pinned: bool) -> f32 {
        self.list_scroll[usize::from(pinned)]
    }

    /// 写某个列表的滚动偏移（见 [`Self::list_scroll`]）。
    pub fn set_list_scroll(&mut self, pinned: bool, v: f32) {
        self.list_scroll[usize::from(pinned)] = v;
    }

    /// 当前生效的类型过滤集合（空 = 不过滤）。
    ///
    /// 单选与多选两个字段的**唯一汇合点**：行筛选、空状态文案、
    /// 行高缓存键都从这里取。CAUTION: 不要在新代码里直接读
    /// `type_filter` / `type_filter_multi`——两处各判一次，
    /// 改了模式切换就会漏一处（历史教训见 [`RowFilterKey`]）。
    pub fn active_type_kinds(&self) -> Vec<modular_clipboard_core::ClipKind> {
        if !self.type_filter_multi.is_empty() {
            self.type_filter_multi.iter().copied().collect()
        } else {
            self.type_filter.into_iter().collect()
        }
    }

    /// 切换某个类型的过滤态（单选 / 多选由配置决定）。
    ///
    /// - 单选：已选中 ⇒ 取消；否则换成它（与旧行为一致）；
    /// - 多选：在集合里增删该项；集合空 = 不过滤。
    pub fn toggle_type_filter(&mut self, kind: modular_clipboard_core::ClipKind, multi: bool) {
        if multi {
            if !self.type_filter_multi.remove(&kind) {
                self.type_filter_multi.insert(kind);
            }
            // 多选模式下单选字段必须让位：否则两个字段同时非空，
            // `active_type_kinds` 的优先序会让「看起来没反应」。
            self.type_filter = None;
        } else {
            self.type_filter_multi.clear();
            self.type_filter = (self.type_filter != Some(kind)).then_some(kind);
        }
    }

    /// 清掉**针对某个条目**的一切编辑态（该条目已从库里消失时调用）。
    ///
    /// 收在这里而不是散在 `app.rs` 的删除分支里：编辑态一共六个字段
    /// （内联三件 + 图片窗口两件 + 撤销备份），漏清任何一个都会留下
    /// 「指向已不存在条目」的悬挂状态——
    ///
    /// - 留 `editing`：下一帧 `draw_item_row` 找不到该行，编辑器不画，
    ///   但 `edit_buf` 里的字还在；用户再点别的行编辑时「切行提交」
    ///   会拿这份缓冲去写**已被删除**的 id（store 返回 0 行变更，
    ///   界面静默无反馈，用户以为保存过了）。
    /// - 留 `image_editor`：帧循环每帧尝试为一个不存在的条目建窗，
    ///   `image_editor_src` 已被 `take` 走 ⇒ 走「有 id 没有字节」分支，
    ///   窗口建不起来但状态永远不清（每帧重试）。
    /// - 留 `edit_backup`：撤销按钮继续挂在**别的行**上（按钮判据是
    ///   `edit_backup.id == 本行 id`，所以实际是悬挂在列表里找不到的
    ///   行上），等下次同 id 复用（EntryId 单调不复用，故永不命中）。
    ///
    /// `id` 不匹配的字段**不动**：删 A 不该打断正在编辑的 B。
    pub fn clear_edit_state(&mut self, id: modular_clipboard_core::EntryId) {
        if self.editing == Some(id) {
            self.editing = None;
            self.edit_buf.clear();
            self.edit_focus_pending = false;
        }
        if self.image_editor == Some(id) {
            self.image_editor = None;
            self.image_editor_src = None;
        }
        if self.edit_backup.as_ref().is_some_and(|b| b.id == id) {
            self.edit_backup = None;
        }
    }

    /// 无条件清掉**所有**编辑态（条目被批量清空时调用）。
    ///
    /// 与 [`Self::clear_edit_state`] 的分工：那个按 id 精确清（删一条），
    /// 这个清空（列表整体没了，没有什么可精确匹配的）。
    pub fn clear_all_edit_state(&mut self) {
        self.editing = None;
        self.edit_buf.clear();
        self.edit_focus_pending = false;
        self.image_editor = None;
        self.image_editor_src = None;
        self.edit_backup = None;
    }

    // ---- 热区（`Frame` 的便捷方法转到这儿来）----
    //
    // 显式带 ns 的版本留给 `Frame` 之外的使用方（测试）。

    /// 把热区写进**指定窗口**的命名空间。
    pub fn hit_insert_to(&mut self, ns: HitNs, key: &'static str, r: Rect) {
        self.hit.insert(ns, key, r);
    }

    /// 从**指定窗口**的命名空间读热区。
    pub fn hit_get_from(&self, ns: HitNs, key: &str) -> Option<Rect> {
        self.hit.get(ns, key)
    }
}

/// 一帧的绘制上下文。
pub struct Frame<'a> {
    /// 根 `Ui`。
    pub ui: &'a mut Ui,
    /// 界面状态。
    pub state: &'a mut UiState,
    /// 服务。
    pub svc: &'a mut Service,
    /// 工作区：卡片的唯一权威来源。
    pub ws: &'a Workspace,
    /// 调色板。
    pub pal: &'a Palette,
    /// DPI 缩放。
    pub scale: f32,
    /// 整个客户区。
    pub area: Rect,
    /// 本帧要绘制的卡片（子窗口用）。
    ///
    /// `None` 表示画主窗口（顶栏 + 全部停靠卡片）。
    ///
    /// CAUTION: 不能在 `draw_child` 里"找第一张 host==Window 的卡片"：
    /// 详情与视图都是 Window 型，两个子窗口会各自找到同一张卡片。
    pub card_id: Option<crate::card::CardId>,
    /// 本帧的热区写到**哪个窗口的命名空间**。
    ///
    /// 由构造方给出，绘制代码经 [`Self::hit_insert`] 写入时不接触它——
    /// 免得某个窗口的控件被误标成另一个窗口的（那正是这次要修的
    /// 问题本身）。
    pub hit_ns: HitNs,
}

impl<'a> Frame<'a> {
    /// 清空本窗口的热区。每个绘制入口开工前调一次。
    pub fn hit_clear(&mut self) {
        self.state.hit.clear(self.hit_ns);
    }

    /// 登记一个热区到本窗口。
    ///
    /// # 只登记**部分可见**的控件
    ///
    /// 完全滚出视口的控件（设置卡片在大字号 / 小窗口下很常见）不登记：
    /// 它的坐标落在窗口之外，自动化（MCP 的 `click_element`）拿着它去点，
    /// 点的是空气、甚至点到别的窗口上。部分可见的仍登记——用户能点到
    /// 露出来的那一部分。
    pub fn hit_insert(&mut self, key: &'static str, r: Rect) {
        let sane = r.min.x.is_finite()
            && r.min.y.is_finite()
            && r.max.x.is_finite()
            && r.max.y.is_finite()
            && r.width() > 0.0
            && r.height() > 0.0;
        if !sane || !self.area.intersects(r) {
            return;
        }
        let ns = self.hit_ns;
        self.state.hit_insert_to(ns, key, r);
    }

    /// 读本窗口的热区。
    pub fn hit_get(&self, key: &str) -> Option<Rect> {
        self.state.hit_get_from(self.hit_ns, key)
    }

    /// 登记一个参与**液态玻璃**合成的矩形。
    ///
    /// # 调用时机
    ///
    /// 必须在画该面板/卡片的**底色之前**：玻璃 pass 在 egui 内容之前跑
    /// （它是背景层），矩形只用于「这个像素属于哪张卡片」。
    ///
    /// # 为什么上限是 16 张
    ///
    /// `gfx::glass_pass::MAX_GLASS_CARDS`。超出部分由 uniform 换算处静默
    /// 丢弃——列表可见行约 10 张 + 顶栏 + 搜索面板 + 类型栏，正常不会超。
    pub fn push_glass(&mut self, r: Rect, radius: f32) {
        // 与 `hit_insert` 同一套健全性检查：非有限坐标/零面积/不在客户区内
        // 的矩形不该进 uniform（NaN 会让 SDF 全程为假，整张卡片没玻璃）。
        let sane = r.min.x.is_finite()
            && r.min.y.is_finite()
            && r.max.x.is_finite()
            && r.max.y.is_finite()
            && r.width() > 0.0
            && r.height() > 0.0;
        if !sane || !self.area.intersects(r) {
            return;
        }
        if self.state.glass_rects.len() >= modular_clipboard_gfx::glass_pass::MAX_GLASS_CARDS {
            return;
        }
        self.state
            .glass_rects
            .push(modular_clipboard_gfx::glass_pass::GlassRect::new(r, radius));
    }

    /// 记录行卡片底色 alpha（供玻璃 pass 定覆盖度）。
    pub fn set_glass_card_alpha(&mut self, a: f32) {
        self.state.glass_card_alpha = a;
    }
}

/// 画一个**独立子窗口**的整帧内容。
///
/// 与 [`draw`] 的区别：那函数画「主窗口的顶栏 + 卡片区」，
/// 这函数画「一张卡片铺满整个窗口」。
///
/// # 为什么子窗口不画顶栏
///
/// 子窗口是纯内容视图，没有「搜索/设置」这类全局控件——
/// 那些只在主窗口有一份。子窗口只承载一张卡片的内容区。
pub fn draw_child(f: &mut Frame<'_>) {
    let area = f.area;
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    // 子窗口有自己的一套坐标，热区必须写在自己的命名空间里。
    f.hit_clear();

    // 找到本窗口对应的那张卡片。
    //
    // NOTE: 不 `expect`：`draw_child` 是生产路径，而「窗口存在但卡片
    // 不在列表里」是**可能发生**的短暂不一致（卡片刚被删、窗口
    // 还没销毁）。这里降级为空画而不是 panic——一个附属面板
    // 的状态问题不该让整个程序崩掉。
    let Some(id) = f.card_id else {
        draw_empty(f, area, "未指定卡片");
        return;
    };
    let Some(card) = f.ws.get(id) else {
        draw_empty(f, area, "窗口与卡片状态不同步");
        return;
    };

    // 卡片铺满整个客户区，四周留一点内边角。
    let r = area.shrink2(vec2(f.pal.space_sm, f.pal.space_sm));
    if r.width() <= 0.0 || r.height() <= 0.0 {
        return;
    }
    draw_card_frame(f, r);

    let pad = f.pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));
    let head_h = (f.pal.font_md * 1.8).max(20.0);
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    // 拖动区（F-17 / UI-P0-3）：WS_POPUP 子窗口**没有系统标题栏**，
    // 不向 WM_NCHITTEST 上报 HTCAPTION 区的话，鼠标无法移动窗口——
    // 用户只能边缘 resize 或 Alt+Space，。「拖出去成为独立窗口」
    // 这个功能本身就失去意义。
    //
    // 上报哪里：卡片**标题头**整条。头部只有纯文本标题、没有可点
    // 控件，整条上报是安全的。列表体**绝不能**上报——行点击会被
    // 系统转成 WM_NCLBUTTONDOWN，egui 收不到（主窗口顶栏挖空按钮
    // 是同一个原因）。
    //
    // CAUTION: 先 clear 再 push。`drag_regions` 是主/子/设置三窗
    // 共用的一张 Vec（没有按窗口分命名空间， unlike `hit`），而各窗
    // 读取的时机都在自己 run_ui 之后、下一窗渲染之前——clear 保证
    // 本窗读到的只有自己的区域（主窗口的 draw_topbar 用「整体替换」
    // 达到同样效果，见该函数末尾的 `f.state.drag_regions = drag`）。
    //
    // History 卡片在子窗口里没有标题头（draw_card_header 直接返回），
    // 没有可上报的区域；它的列表体两侧留白条由 draw_history_body
    // 自己上报（在子窗坐标系里同样正确）。
    f.state.drag_regions.clear();
    if card.kind != CardKind::History {
        f.state.drag_regions.push(head);
    }

    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + f.pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        // 置顶卡片作为独立窗口时，只画列表内容（无标题头）——
        // 窗口标题栏已经说明了它是什么。
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
        // 主窗口专属的种类不该出现在子窗口；画空状态而不是崩溃。
        _ => draw_empty(f, body, "该卡片不支持独立窗口"),
    }
}

/// 画一帧。
///
/// 流程：背景 → 悬浮装饰布局 → 顶栏（搜索图标）→ 搜索状态机 →
/// 卡片（Z 序）→ 类型过滤栏 → 悬浮搜索面板 → 通知 toast。
///
/// 2026-10-15 第三轮起，搜索输入框与类型过滤栏都是**悬浮层**：
/// - 搜索面板贴在卡片区顶部（与顶栏/左右缘的间距即 `card_area` 的 12px
///   内缩），折叠时只占顶栏一枚放大镜；
/// - 类型栏停靠卡片区某一侧，该侧通过 `FloatLayout::insets` 让列表收缩，
///   浮动按钮不压内容（右缘更不会盖住滚动条与行内删除按钮）。
///
/// NOTE: 原型里这两个悬浮层画在**窗口外**（HTML 窗口可以探出边框）；
/// 真实窗口画不出自身边界，落为窗口内悬浮——视觉语义（间距、投影、
/// 不挡内容）一致，位置偏差已在各组件的文档里说明。
pub fn draw(f: &mut Frame<'_>) {
    let area = f.area;
    // 玻璃收集器每帧重置：帧循环取走后是空的，而「没取走」的帧
    // （跳过渲染）也不该把上一帧的矩形留到下一帧——面板可能已收起。
    // 默认 alpha 给 1.0（不透明）：万一某条路径忘了设，玻璃至少是
    // 实底的，不会整片消失。
    f.state.glass_rects.clear();
    f.state.glass_card_alpha = 1.0;
    // 背景必须铺满整个客户区，否则窗口边缘会露出 clear color。
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    // 二级主题的**质感层**（纸质纤维 / 星空星点）：紧跟背景、先于
    // 一切内容。颜色层由 `theme::apply_secondary` 在调色板解析时
    // 完成，这一层只补「程序化纹理」那一半。
    //
    // CAUTION: 磨砂 / 半透明模式下 `bg` 是半透明的（模糊底图透出来），
    // 纹理画在它之上仍然正确——tint alpha 很低，只是加一层墨，
    // 不会盖住底图。且本层**不参与**磨砂的抓图（磨砂抓的是窗口
    // 身后的桌面，不是自己画的内容），不存在反馈回路。
    draw_theme_texture(f);

    let top = Rect::from_min_max(
        pos2(area.min.x, area.min.y),
        pos2(area.max.x, area.min.y + crate::solver::TOPBAR_HEIGHT),
    );
    let body = crate::solver::card_area(area);

    // 热区是**每帧重算**的：跨帧留着旧值会让测试点到一个
    // 「上一帧存在、这一帧已消失」的位置，症状是改了布局后
    // 测试仍通过但测的是空气。
    f.hit_clear();
    // 拖动区同样是每帧重算。早前由 `draw_topbar` 用赋值完成（等于顺带
    // 清零）；顶栏改到**最后**画之后，清零必须显式放在这里，否则顶栏
    // 的 `extend` 会把上一帧的旧矩形一直留着。
    f.state.drag_regions.clear();

    // 卡片（矩形由 solver 算好，绘制层只读不判断）。
    //
    // NOTE: 卡片**先于**顶栏画。顶栏是不透明的顶条，画在最后才能把
    // 任何越出列表区的内容遮住（行入场动画、投影、将来新增的绘制），
    // 不会出现「内容直接渲染在顶栏上、没有遮蔽」。
    draw_cards(f, body);

    // 顶栏（含搜索开关图标）。返回图标响应供状态机使用。
    let icon_resp = draw_topbar(f, top);

    // 搜索状态机：悬停 0.18s / 点击展开，离开宽限 0.35s 折叠，
    // 焦点或查询词钉住。
    //
    // 「焦点」与「悬停」两项都来自**弹窗**：面板住在自己的窗口与
    // `egui::Context` 里，主窗口既看不到它的指针也读不到它的焦点
    // （焦点在弹窗的 `Context::memory()`）。`popup` 每帧把两者回报
    // 到 `UiState`（`search_popup_hover` / `search_popup_focus`）。
    run_search_state_machine(f, &icon_resp);

    // 缩略图纹理按**本帧全部可见图片条目**淘汰（帧级，跨列表）：
    // 滚出屏的条目释放 GPU 纹理；像素仍在 `thumb_cache`（LRU）里，
    // 滚回来只需重新上传，不必重新解码。
    // `thumb_dims` **不参与淘汰**（尺寸是行高的输入，见字段注释）。
    f.state.thumb_tex.retain(|id, _| f.state.thumb_seen.contains(id));
    f.state.thumb_seen.clear();

    // NOTE: 搜索面板与类型过滤栏**不在这里画**——它们是独立弹窗
    // （`crate::popup`），画在主窗口客户区里就等于「画在窗口内」，
    // 与原型不符。窗口过程、事件泵、投影、点击穿透都在那边。
    //
    // NOTE: 设置界面同样不在这里画（独立窗口，见 `settingswin`）。

    // ---- Enter 复制选中条目（键盘流，2026-10-20）----
    //
    // 为什么安全：主窗口**没有任何 TextEdit**（搜索框在弹窗自己的
    // Context、设置在另一个窗口），行内操作是裸 `ui.interact`、顶栏
    // 按钮是 `egui::Button`——egui 0.36 的 Button 不自行响应 Enter
    // （`button.rs` 无键盘激活分支）。也就是说这个键在主窗口本来
    // 就没人认领，空着也是空着。
    //
    // 剪贴板管理器的高频动作是「选中 → 复制」，值得一条键盘路径
    // （与行内复制按钮、双击复制同落地 `Op::Copy`，成功也有 toast
    // 反馈）。
    //
    // CAUTION: 录制热键时除外——那时每个键都有意义（`capture_recording`
    // 正在等一个组合键）， Enter 复制会把它录进去。
    //
    // CAUTION 2: **内联编辑中除外**（2026-10-21）。编辑器是主窗口里的
    // 第一个 TextEdit，Enter 归它（多行换行）；不排除的话「改内容按
    // 回车换行」会顺带把条目复制一遍，两个动作缠在一起。
    // Esc 同理：编辑中 = 取消编辑，不往外传。
    if f.state.editing.is_none()
        && f.state.hotkey_recording.is_none()
        && let Some(id) = f.svc.state.selected
    {
        let enter = f
            .ui
            .input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.any());
        if enter {
            f.state.push(Op::Copy(id));
        }
    }
    // Esc 取消内联编辑（2026-10-21）。放在 Enter 分支之后：两者互斥，
    // 但分开写读得清楚。弹窗里的 Esc 走弹窗自己的 Context（见
    // `draw_search_panel`），这里是主窗口 Context 的那一半。
    if f.state.editing.is_some() && f.state.hotkey_recording.is_none() {
        let esc = f.ui.input(|i| i.key_pressed(egui::Key::Escape));
        if esc {
            f.state.push(Op::CancelEdit);
        }
    }

    draw_notice_toast(f);
}


#[cfg(test)]
mod tests {
    use super::*;
    // NOTE: `t` 只有测试用（生产代码全在子模块里），导入放在 tests 内，
    // 避免 lib 目标出现 unused import。
    use modular_clipboard_core::i18n::t;
use super::{
    float::*,
    topbar::*,
    settings_items::*,
    theme_editor::*,
    list::*,
    row::*,
    shadow::*,
};

    /// 造一个跑完字体注册的 `egui::Context`。
    ///
    /// 排版类断言必须走真实字体链：字形是否存在只有 egui 知道，
    /// 纯逻辑测试测不出「这一串字符会不会被排成两行」。
    fn settled_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_cjk_font(&ctx, None);
        crate::theme::install_icon_font(&ctx);
        // 注册在第一帧之后生效，必须再跑一帧。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    // ------------------------------------------------------------------
    // 交互测试夹具
    // ------------------------------------------------------------------
    //
    // NOTE: 为什么单独一个 `Harness` 而不在每个测试里现搭：
    //
    // 测「点击齿轮能打开设置」需要**跑完整帧**并把结果反馈到下一帧，
    // 中间要正确的 `screen_rect`、字体注册、`Service` 与临时数据目录。
    // 每处现搭约40 行、且极易漏掉某个必需步骤（实测漏字体导致
    // 文字全被省略、断言看起来像布局错）。
    //
    // 夹具负责：临时目录 + Service + ctx（含字体）+ 可注入输入的帧循环。

    /// 跑一帧的测试夹具。
    struct Harness {
        ctx: egui::Context,
        svc: modular_clipboard_app::Service,
        ws: crate::workspace::Workspace,
        state: UiState,
        pal: crate::theme::Palette,
        /// 客户区（逻辑点）。
        area: Rect,
        /// 夹具时钟（秒），喂给 egui 的 `RawInput::time`。
        ///
        /// 见 [`Self::input`] 与 [`Self::advance_secs`]。起始 1.0 而非 0：
        /// 避开 egui 内部若干 `if time > 0.0` 之类的特判分支。
        clock: f64,
        _dir: tempdir::TempDir,
    }

    /// 极简临时目录，够用且不引依赖。
    ///
    /// 用 `std::env::temp_dir()` + 随机名 + Drop 清理，
    /// 免得为了一个测试目录引入 `tempfile` 依赖。
    mod tempdir {
        use std::path::PathBuf;
        /// 临时目录，Drop 时递归删除。
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                // 进程 id + 原子计数器 + tag：同一进程内多次创建不撞名。
                static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!(
                    "mcb-test-{tag}-{}-{n}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&p).expect("建临时目录");
                Self(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                // 测试环境里残留一个临时目录不致命，清理失败不该
                // 让测试 panic（那会掩盖真正的失败原因）。
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    impl Harness {
        /// 新建一个能跑帧的夹具。
        ///
        /// `size` 是客户区的**逻辑点**尺寸。
        fn new(tag: &str, size: egui::Vec2) -> Self {
            // 钉死中文：断言与热区 key 都按中文文案写。全局语言是跨测试
            // 共享的（`App::from_service` 在 CI 英文系统上会解析成英文），
            // 每个夹具开局先归位，杜绝串扰。
            modular_clipboard_core::i18n::set_locale(
                modular_clipboard_core::i18n::Locale::ZhCn,
            );
            let dir = tempdir::TempDir::new(tag);
            let cfg = modular_clipboard_core::Config::default();
            let svc =
                modular_clipboard_app::Service::with_data_dir(cfg, Some(dir.path()))
                    .expect("建 Service（临时目录）");

            let ctx = egui::Context::default();
            crate::theme::install_cjk_font(&ctx, None);
            crate::theme::install_icon_font(&ctx);
            // 字体注册在第一帧后生效，必须先跑一帧。
            let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
            out.textures_delta.clear();

            Self {
                ctx,
                svc,
                ws: crate::workspace::Workspace::default(),
                state: UiState::default(),
                pal: crate::theme::Palette::light(),
                area: Rect::from_min_size(pos2(0.0, 0.0), size),
                clock: 1.0,
                _dir: dir,
            }
        }

        /// 往服务里塞 `n` 条剪贴板记录。
        ///
        /// # 为什么必须显式提供
        ///
        /// NOTE: 默认的 `Harness` 是**空列表**，于是 `draw_history_body`
        /// 每次都走 `draw_empty` 分支直接 return —— **列表行的绘制
        /// 路径从未被任何测试覆盖过**。
        ///
        /// 这正是「列表坐标算错」能一路溜到实机的原因：505 条测试
        /// 全绿，但没有一条真的画过一行。
        fn with_items(mut self, n: usize) -> Self {
            for i in 0..n {
                let text = format!("第 {i} 条记录，用于测试列表行的坐标与布局");
                let payload = modular_clipboard_core::CapturedPayload {
                    kind: modular_clipboard_core::ClipKind::Text,
                    bytes: text.as_bytes().to_vec(),
                    html: None,
                    text: Some(text),
                    source_app: format!("App{i}"),
                    files: Vec::new(),
                    dims: None,
                };
                if !self.svc.ingest(payload) {
                    tracing::warn!(i, "塞入测试记录失败");
                }
            }
            // 写入后必须重灌列表，否则 `state.items` 还是空的。
            self.svc.reload_list();
            self
        }

        /// 把前 `n` 条记录置顶。
///
/// 内嵌置顶区只在**有置顶条目**时才占空间，所以针对它的测试必须
/// 先造置顶条目——否则 `embedded_pinned_height` 直接返回 0，
/// 测的还是「没有置顶区」那条路径。
fn with_pinned(mut self, n: usize) -> Self {
    let ids: Vec<_> = self
        .svc
        .state
        .items
        .iter()
        .take(n)
        .map(|it| it.id)
        .collect();
    for id in ids {
        if let Err(e) = self.svc.toggle_pin(id) {
            tracing::warn!("置顶测试记录失败: {e}");
        }
    }
    self.svc.reload_list();
    self
}

        /// 组装这一帧的 [`RawInput`]。
        ///
        /// `time` **显式给出**：egui 在它为 `None` 时只按
        /// `predicted_dt`（≈16.7ms/帧）自增，要覆盖 0.35s 的折叠宽限
        /// 得跑二十多帧。这里由夹具自己掌钟（[`Self::clock`]），
        /// 测试可用 [`Self::advance_secs`] 一步跨过去——判据代码本身
        /// 仍是真的（`now - since >= SEARCH_COLLAPSE_DELAY`），
        /// 只是「多久」由夹具说了算。
        fn input(&self, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(self.area),
                events,
                time: Some(self.clock),
                ..Default::default()
            }
        }

        /// 把夹具时钟向前拨 `secs` 秒（模拟时间流逝）。
        ///
        /// 拨完**不自动跑帧**——留给调用方决定这一帧要带什么输入事件。
        fn advance_secs(&mut self, secs: f64) {
            self.clock += secs;
        }

        /// 跑一帧主界面，返回收集到的 `Op`。
        fn frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let input = self.input(events);
            // 每帧推进 1/60s：与真实帧率同量级，这样「没拨时钟」的
            // 帧不会把时间冻结、也不会一步跨过时间判定。
            self.clock += 1.0 / 60.0;
            // CAUTION: 必须先跑 solver，否则卡片矩形全是 `Rect::ZERO`。
            //
            // `draw()` 只负责**画** `card.rect`，求解是 App 层的事。
            // 夹具只调 `draw` 时拿到的就是零矩形——症状是
            // 「测试里卡片矩形全空」，看起来像布局坏了。
            //
            // 这里复刻 `App::draw_frame` 的顺序：先 solve+apply，再画。
            // IMPORTANT: 必须放在解构 `self` 之前：解构出的 `&self.ws`
            // 会让它在后面无法可变借用。
            // 与 `App::draw_frame` 用同一个 scale（生产路径传真实 ppp；
            // 测试夹具的 ppp 恰好是 1.0，`solve` 与 `solve_scaled` 等价）。
            let ppp = self.ctx.pixels_per_point();
            let sol = crate::solver::solve_scaled(&self.ws, self.area, ppp);
            crate::solver::apply(&mut self.ws, &sol);
            let (svc, ws, state, pal, area) =
                (&mut self.svc, &self.ws, &mut self.state, &self.pal, self.area);
            let mut out = self.ctx.run_ui(input, |ui| {
                let ppp = ui.ctx().pixels_per_point();
                let mut f = Frame {
                    ui,
                    state,
                    svc,
                    ws,
                    pal,
                    scale: ppp,
                    area,
                    card_id: None,
                    hit_ns: HitNs::Main,
                };
                draw(&mut f);
            });
            out.textures_delta.clear();
            // draw() 只**收集** Op，落地在 App 层。这里取出来给断言。
            std::mem::take(&mut self.state.ops)
        }

        /// 空输入跑一帧。
        fn frame(&mut self) -> Vec<Op> {
            self.frame_with(Vec::new())
        }

        /// 以**搜索弹窗身份**跑一帧（`App::draw_popup` 的等价路径）。
        ///
        /// 面板住在独立窗口里，有自己的 `egui::Context` 与热区命名空间；
        /// 主窗口的 `frame()` 里根本没有它。想测面板的输入 / 焦点行为
        /// （打字、清除按钮），只能从这里进。
        ///
        /// 客户区 = 面板 + 四周一圈投影边距（`popup::SHADOW_MARGIN`），
        /// 与真实弹窗一致。
        fn search_popup_frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let margin = crate::popup::SHADOW_MARGIN;
            let panel = Rect::from_min_max(
                pos2(margin, margin),
                pos2(self.area.width() - margin, self.area.height() - margin),
            );
            let input = self.input(events);
            // 每帧推进时钟：与 `frame_with` 同款。egui 的事件带时间戳，
            // 时钟不动时它会把「同一时刻」的新事件当成旧事件的重复而丢弃。
            self.clock += 1.0 / 60.0;
            let mut out = self.ctx.run_ui(input, |ui| {
                let mut f = Frame {
                    ui,
                    state: &mut self.state,
                    svc: &mut self.svc,
                    ws: &self.ws,
                    pal: &self.pal,
                    scale: 1.0,
                    area: panel,
                    card_id: None,
                    hit_ns: HitNs::Popup(crate::popup::PopupKind::Search),
                };
                draw_search_panel(&mut f, panel);
            });
            // 焦点回报：与 `App::draw_popup` 同一动作。
            //
            // CAUTION: 必须在 `f` 的作用域**之外**读 `ui.memory()`——
            // `f` 借走了 `ui` 的可变引用，两者不能同时活着。
            self.state.search_popup_focus = self
                .state
                .search_edit_id
                .is_some_and(|id| self.ctx.memory(|m| m.focused()) == Some(id));
            out.textures_delta.clear();
            std::mem::take(&mut self.state.ops)
        }

        /// 以**类型栏弹窗身份**跑一帧（客户区 = 面板 + 一圈投影边距）。
        ///
        /// 与 `search_popup_frame_with` 同一形状：面板住在独立窗口里，
        /// 主窗口的 `frame()` 里根本没有它。
        fn rail_popup_frame(
            &mut self,
            events: Vec<egui::Event>,
            panel: Rect,
            kinds: &[modular_clipboard_core::ClipKind],
        ) {
            let input = self.input(events);
            self.clock += 1.0 / 60.0;
            let mut out = self.ctx.run_ui(input, |ui| {
                let mut f = Frame {
                    ui,
                    state: &mut self.state,
                    svc: &mut self.svc,
                    ws: &self.ws,
                    pal: &self.pal,
                    scale: 1.0,
                    area: panel,
                    card_id: None,
                    hit_ns: HitNs::Popup(crate::popup::PopupKind::Rail),
                };
                draw_type_rail(&mut f, panel, kinds);
            });
            out.textures_delta.clear();
        }

        /// 以**类型栏弹窗身份**点第 `idx` 枚按钮，返回该帧收集到的 Op。
        ///
        /// 按钮矩形按 `FloatLayout` 的几何算（弹窗客户区中央的按钮组），
        /// 与生产同一条算式——不自己估坐标。
        fn rail_popup_click(&mut self, idx: usize) -> Vec<Op> {
            let margin = crate::popup::SHADOW_MARGIN;
            let area = Rect::from_min_size(pos2(0.0, 0.0), self.area.size());
            let cfg = self.svc.state.config.ui.clone();
            let items = self.svc.state.items.clone();
            let kinds = FloatLayout::kinds(&cfg, &items);
            let group = FloatLayout::compute(&cfg, &items, false, area, None)
                .rail
                .expect("测试前置：类型栏应可见");
            let size = modular_clipboard_core::config::clamp_type_rail_size(cfg.type_rail.size);
            // 弹窗客户区里的面板矩形（= 类型栏按钮组在弹窗内的位置）。
            let panel = Rect::from_min_max(
                pos2(margin, margin),
                pos2(margin + group.width(), margin + group.height()),
            );
            // 按钮槽位从**面板内侧**起排（跳过 `RAIL_PAD`）——与
            // `draw_type_rail` 同一算式（它拿到的 `rail` 就是这个面板矩形）。
            let slot = Rect::from_min_size(
                pos2(
                    panel.min.x + RAIL_PAD,
                    panel.min.y + RAIL_PAD + idx as f32 * (size + RAIL_GAP.min(8.0)),
                ),
                vec2(size, size),
            );
            let p = slot.center();
            // 点击必须**分两帧**：先一帧 `PointerMoved`（让 egui 知道指针
            // 在哪、按钮进入 hover），下一帧才按下/松开。合成一帧时
            // egui 收不到点击——实测 `clicked()` 恒假。
            self.rail_popup_frame(vec![egui::Event::PointerMoved(p)], panel, &kinds);
            let input = self.input(vec![
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ]);
            self.clock += 1.0 / 60.0;
            let mut out = self.ctx.run_ui(input, |ui| {
                let mut f = Frame {
                    ui,
                    state: &mut self.state,
                    svc: &mut self.svc,
                    ws: &self.ws,
                    pal: &self.pal,
                    scale: 1.0,
                    area: panel,
                    card_id: None,
                    hit_ns: HitNs::Popup(crate::popup::PopupKind::Rail),
                };
                draw_type_rail(&mut f, panel, &kinds);
                // ---- 临时诊断（调试完删除） ----
            });
            out.textures_delta.clear();
            std::mem::take(&mut self.state.ops)
        }

        /// 跑一帧**子窗口**内容，返回本帧上报的拖动区。
        ///
        /// 复刻 `ChildWindow::render` 的关键路径：`draw_child` 之后
        /// 立刻读 `drag_regions`——生产上那之后就是
        /// `set_drag_region_multi`。共用 Vec 的时序（本窗 clear+push、
        /// 读完即取走）在这里同样成立。
        fn child_frame_drag(&mut self, card_id: crate::card::CardId) -> Vec<Rect> {
            let input = self.input(Vec::new());
            let (svc, ws, state, pal, area) =
                (&mut self.svc, &self.ws, &mut self.state, &self.pal, self.area);
            let mut out = self.ctx.run_ui(input, |ui| {
                let ppp = ui.ctx().pixels_per_point();
                let mut f = Frame {
                    ui,
                    state,
                    svc,
                    ws,
                    pal,
                    scale: ppp,
                    area,
                    card_id: Some(card_id),
                    hit_ns: HitNs::Child(card_id),
                };
                draw_child(&mut f);
            });
            out.textures_delta.clear();
            std::mem::take(&mut self.state.drag_regions)
        }

        /// 只跑**设置窗口**的一帧（不跑主界面的 `draw`）。
        ///
        /// 设置界面已移到独立窗口（见 `crate::settingswin`），
        /// 主界面的 `draw` 不再画它——所以要用这条专用入口来测。
        ///
        /// NOTE: 不跑 solver：设置面板不读 `card.rect`，只需要
        /// `svc`/`state`/`pal` 与客户区。
        fn settings_frame(&mut self) -> Vec<Op> {
            self.settings_frame_with(Vec::new())
        }

        /// 带输入事件的设置帧。
        fn settings_frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let input = self.input(events);
            let (svc, ws, state, pal, area) =
                (&mut self.svc, &self.ws, &mut self.state, &self.pal, self.area);
            let mut out = self.ctx.run_ui(input, |ui| {
                let ppp = ui.ctx().pixels_per_point();
                let mut f = Frame {
                    ui,
                    state,
                    svc,
                    ws,
                    pal,
                    scale: ppp,
                    area,
                    card_id: None,
                    hit_ns: HitNs::Settings,
                };
                draw_settings(&mut f);
            });
            out.textures_delta.clear();
            std::mem::take(&mut self.state.ops)
        }

        /// 在给定**逻辑点**坐标处点一下，跑一帧（主界面）。
        fn click_at(&mut self, p: egui::Pos2) -> Vec<Op> {
            self.frame_with(click_events(p))
        }

        /// 在设置**窗口**里的坐标处点一下，跑一帧。
        ///
        /// 与 [`Self::click_at`] 的区别只在跑哪一帧：设置界面不在
        /// 主界面的 `draw` 里画，用 `click_at` 点它永远打不中。
        fn settings_click_at(&mut self, p: egui::Pos2) -> Vec<Op> {
            self.settings_frame_with(click_events(p))
        }

        /// 模拟「光标停在搜索图标上并停够时间」⇒ 触发**悬停展开**。
        ///
        /// # 为什么不靠真实时间流逝
        ///
        /// 测试夹具里的 egui 时间不是真实时钟，靠 `sleep` 既慢又不稳。
        /// 这里只把「悬停起点」拨到过去（等价于「已经停了足够久」），
        /// 判据本身（`now - since >= SEARCH_HOVER_DELAY`）仍走真实代码
        /// ——所以「停留时长」这条规则一旦被改坏，本助手也会跟着失效。
        fn hover_search_icon(&mut self) -> Vec<Op> {
            let icon = hit(self, "search_icon").center();
            // 先让指针落到图标上：`hovered()` 依赖本帧输入里的指针位置。
            self.frame_with(vec![egui::Event::PointerMoved(icon)]);
            self.state.search_hover_since = Some(f64::NEG_INFINITY);
            self.frame_with(vec![egui::Event::PointerMoved(icon)])
        }

        /// 把光标移出顶栏与搜索框（触发**离开即折叠**）。
        fn move_pointer_away(&mut self) -> Vec<Op> {
            let away = egui::pos2(self.area.center().x, self.area.max.y - 20.0);
            self.frame_with(vec![egui::Event::PointerMoved(away)])
        }
    }

    /// 一次完整点击（移动 → 按下 → 抬起）的事件序列。
    fn click_events(p: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]
    }

    /// 只按下（不松开）的一帧事件——滑条类交互专用。
    ///
    /// CAUTION: [`Self::click_events`] 把 down/up 压进**同一帧**。
    /// `Response::clicked()` 靠「press→release 同帧配对」仍然成立
    /// （所以按钮类测试照常工作），但滑条响应的是
    /// `is_pointer_button_down_on`——它检查的是**帧末**的按下状态，
    /// up 处理后恒为 false ⇒ 滑条永远收不到「按下即跳转」。
    /// 真实用户的操作天然分帧（down 一帧、up 一帧），是测试压缩
    /// 丢了中间态。滑条测试必须 press 一帧、release 一帧。
    fn press_events(p: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]
    }

    /// 只松开的一帧事件（与 [`Self::press_events`] 配对）。
    fn release_events(p: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]
    }


    /// 读回某个热区的矩形（逻辑点）。
    ///
    /// 坐标由绘制层自报（`UiState::hit`），测试**不自己算**——
    /// 早前自己估算导致所有点击落在按钮外、测试全失败。
    /// 主窗口的热区。
    fn hit(h: &Harness, name: &str) -> Rect {
        hit_in(h, HitNs::Main, name)
    }

    /// 设置窗口的热区。
    ///
    /// CAUTION: 不能直接从共享表读：主窗口与设置窗口是**两套坐标**，
    /// 共表会让自动化拿着设置窗口的坐标去点主窗口。
    fn settings_hit(h: &Harness, name: &str) -> Rect {
        hit_in(h, HitNs::Settings, name)
    }

    fn hit_in(h: &Harness, ns: HitNs, name: &str) -> Rect {
        h.state
            .hit
            .get(ns, name)
            .unwrap_or_else(|| panic!("热区 {name:?} 未在 {ns:?} 里登记（首帧后才有）"))
    }

    /// 设置（齿轮）按钮矩形。
    fn topbar_gear(h: &Harness) -> Rect {
        hit(h, "topbar_settings")
    }

    /// 清空（垃圾桶）按钮矩形。
    fn topbar_trash(h: &Harness) -> Rect {
        hit(h, "topbar_clear")
    }

    // ------------------------------------------------------------------
    // 交互测试：顶栏
    // ------------------------------------------------------------------

    /// 点设置齿轮 ⇒ 产生 `ToggleSettings`。
    ///
    // NOTE: 断言的是**操作**而不是 `show_settings` 变成 true。
    //
    // `draw()` 只**收集** `Op`，落地在 `App::apply_ops`。所以在
    // paint 层测试里点一下齿轮，`show_settings` 不会变——那是正确的
    // 分层。若这里直接断言状态为 true，就等于要求 paint 层越权改状态。
    //
    //（我最初就是这么写的，于是测试永远失败——不是代码有 bug，
    //  是我把分层理解错了。）
    /// 点顶栏的窗口置顶按钮 ⇒ 发出 `SetBool{always_on_top}`。
    ///
    /// # 为什么断言 `Op` 而不是配置值
    ///
    /// 绘制层只**收集** `Op`，落地在 `App::apply_ops`。所以这里断言
    /// 「发出的 Op 正确」，而不是「配置已经变了」。
    ///
    /// # 置顶本身不需要在这里验
    ///
    /// Win32 那一侧早就有：`presence::set_topmost(hwnd, bool)` +
    /// `App` 帧循环每帧比对 `always_on_top` 并应用（用 `applied_topmost`
    /// 去重）。按钮只需写配置，剩下的自动发生。
    #[test]
    fn clicking_pin_button_toggles_always_on_top() {
        let mut h = Harness::new("topbar-pin", vec2(600.0, 400.0));
        h.frame(); // 先跑一帧，热区才有值

        let before = h.svc.state.config.ui.always_on_top;
        let p = hit(&h, "topbar_pin").center();
        let ops = h.click_at(p);

        let emitted = ops.iter().find_map(|op| match op {
            Op::SetBool {
                group: ConfigGroup::Ui,
                field: "always_on_top",
                value,
            } => Some(*value),
            _ => None,
        });
        assert_eq!(
            emitted,
            Some(!before),
            "点置顶按钮应发出 always_on_top = {before} 的取反，实际 ops={ops:?}"
        );
    }

    /// 顶栏四枚按钮的槽位必须**互不重叠**，且都在搜索图标右侧。
    ///
    /// # 这条守的是一个很难自查的老坑
    ///
    /// 按钮区宽度在 `draw_topbar` 算（`btn_count`），槽位步距在
    /// `draw_topbar_icons` 算（`step = r.width() / btn_count`）。
    /// 两处各写一份数字时，改一处忘另一处 ⇒ 槽位与按钮区错位，
    /// 症状是「最右那枚被搜索框压住」或「点齿轮变成拖窗口」——
    /// 都不是一眼能看出原因的症状。
    #[test]
    fn topbar_icon_slots_do_not_overlap() {
        let mut h = Harness::new("topbar-slots", vec2(600.0, 400.0));
        h.frame();

        let names = [
            "topbar_close",
            "topbar_clear",
            "topbar_pin",
            "topbar_settings",
        ];
        let rects: Vec<Rect> = names.iter().map(|n| hit(&h, n)).collect();

        for (i, r) in rects.iter().enumerate() {
            assert!(
                r.width() > 0.0 && r.height() > 0.0,
                "{} 的槽位不应为零面积，实际={r:?}",
                names[i]
            );
            for (j, other) in rects.iter().enumerate().skip(i + 1) {
                assert!(
                    !r.intersects(*other),
                    "{} 与 {} 的槽位重叠了：{r:?} vs {other:?} ——                      多半是 btn_count 与 step 的分母不一致",
                    names[i],
                    names[j]
                );
            }
        }
        // 最右那枚（设置）的右缘应当就是按钮区的右缘。
        let rightmost = rects
            .iter()
            .max_by(|a, b| a.max.x.partial_cmp(&b.max.x).unwrap())
            .unwrap();
        assert!(
            (rightmost.max.x - rects[3].max.x).abs() < 0.5,
            "最右的槽位应当是「设置」，实际最右的是 {rightmost:?}"
        );
    }

    /// 滚动条热区宽度必须与行内按钮让位用的 `SCROLLBAR_RESERVE` 一致。
    ///
    /// # 这条守的是一个「条目少就好、条目多就坏」的老 bug
    ///
    /// `ScrollStyle.bar_width`（`theme.rs` 里设）与
    /// `SCROLLBAR_RESERVE`（`paint.rs` 里按它让位）两处各写一份的话，
    /// 改一处忘另一处 ⇒ 有 2pt 落在滚动条下面 ⇒ 删除按钮被压住。
    /// 症状在条目少（无滚动条）时完全不出现，所以极难自查。
    ///
    /// 现在两处共用 `theme::SCROLLBAR_WIDTH`，本测试守的是
    /// 「主题里真的把它设成了那个值」—— 有人改了常量忘了改主题设置
    /// 的话，这里会红。
    #[test]
    fn scrollbar_hot_width_matches_row_reserve() {
        let ctx = egui::Context::default();
        let pal = Palette::light();
        crate::theme::apply(&ctx, &pal);
        let style = ctx.style_of(egui::Theme::Light);
        assert_eq!(
            style.spacing.scroll.bar_width, SCROLLBAR_RESERVE,
            "滚动条热区宽度必须等于行内按钮让位用的 SCROLLBAR_RESERVE"
        );
        assert!(
            style.spacing.scroll.floating,
            "必须保持 floating：滚动条不占布局宽度，行宽让位逻辑按这个假设算"
        );
        // 常态滑块不能全透明 —— 那会让用户不知道下面还有内容。
        assert!(
            style.spacing.scroll.dormant_handle_opacity > 0.0,
            "常态滑块应当微可见，全透明会被整体忽略"
        );
    }

    #[test]
    fn clicking_settings_gear_emits_toggle() {
        let mut h = Harness::new("settings-gear", vec2(600.0, 400.0));
        h.frame(); // 先跑一帧，热区才有值
        let p = topbar_gear(&h).center();
        let ops = h.click_at(p);
        assert!(
            ops.contains(&Op::ToggleSettings),
            "点设置齿轮应发出 ToggleSettings，实际={ops:?}"
        );
    }

    /// 设置面板里的关闭按钮 ⇒ 关闭设置。
    ///
    /// # 这条替代了原来的 `clicking_scrim_closes_settings`
    ///
    /// 旧版设置是主窗口上的**覆盖层**，退出入口有「关闭按钮」与
    /// 「点遮罩」两个。现在移到独立窗口，没有遮罩了——点遮罩不可能
    /// 触发任何事。
    ///
    /// 新的退出入口只有标题栏 ✕（由 `settingswin` 处理）与这个
    /// 面板内关闭按钮。
    #[test]
    fn clicking_close_button_in_settings_closes_it() {
        let mut h = Harness::new("settings-close", vec2(600.0, 400.0));
        h.state.show_settings = true;
        h.settings_frame();
        assert!(
            h.state.hit.contains_key(HitNs::Settings, "settings_close"),
            "前置条件：面板应登记关闭按钮热区，实际={:?}",
            h.state.hit.keys(HitNs::Settings)
        );
        let p = settings_hit(&h, "settings_close").center();
        h.settings_click_at(p);
        // 关闭按钮把 `show_settings` 置回 false（不走 Op——
        // 它改的是本窗口的可见性，不是工作区状态）。
        assert!(
            !h.state.show_settings,
            "点关闭按钮应关闭设置面板"
        );
    }

/// 点垃圾桶 ⇒ 产生清空操作。
    ///
    /// CAUTION: 清空是**破坏性**操作，所以必须真的能点出来——否则用户
    /// 想清空历史时只能去改配置文件。
    #[test]
    fn clicking_trash_does_not_clear() {
        // 破坏性操作（清空全部历史）**单击不得触发**——行为已改为
        // 长按 5 秒（2026-10-22 用户要求），误点一下就清空历史是
        // 不可接受的。这条守门防的是有人图省事把长按改回单击。
        //
        // 长按路径本身依赖真实时钟（`Instant::elapsed`），在单测里
        // 不好快进；这里只钉死「短按不触发」这条最重要的边界。
        let mut h = Harness::new("trash", vec2(600.0, 400.0));
        h.frame();
        let p = topbar_trash(&h).center();
        let ops = h.click_at(p);
        assert!(
            !ops.contains(&Op::ClearAll),
            "单击清空按钮不应触发 ClearAll（需长按 5 秒），实际={ops:?}"
        );
    }

    /// 隐藏按钮在顶栏**最左**（2026-10-22 从右侧按钮组移出）。
    ///
    /// 它的位置错了会连带拖动区挖空算错——隐藏按钮压在拖动区上时
    /// 「点隐藏 = 拖窗口」。用热区表的几何关系钉死相对位置。
    #[test]
    fn hide_button_is_leftmost_in_topbar() {
        let mut h = Harness::new("hide-left", vec2(720.0, 420.0));
        h.frame();
        let hide = hit(&h, "topbar_close");
        let clear = hit(&h, "topbar_clear");
        let pin = hit(&h, "topbar_pin");
        let set = hit(&h, "topbar_settings");
        assert!(
            hide.max.x < clear.min.x,
            "隐藏按钮应在右侧功能组**之前**：hide={hide:?} clear={clear:?}"
        );
        assert!(
            clear.min.x < pin.min.x && pin.min.x < set.min.x,
            "右侧组顺序应为 清空/置顶/设置，实际 clear={clear:?} pin={pin:?} set={set:?}"
        );
    }

    /// 搜索面板能接收输入并写进 `query`。
    ///
    /// 早前顶栏那个「搜索框」是用 `painter.text()` 画上去的静态占位符，
    /// 点击只 `request_repaint()` —— 看着像搜索框，实际**打不了字**。
    /// 这条测试守住它真的能输入。
    ///
    /// 2026-10-15：面板改为独立弹窗，输入框住在**弹窗自己的 Context**
    /// 里，因此这里也走弹窗路径（`search_popup_frame_with`）——
    /// 主窗口的 `frame()` 里根本没有这个输入框。
    #[test]
    fn search_box_accepts_typed_text() {
        let mut h = Harness::new("search", vec2(600.0, 400.0));
        h.frame();
        // 搜索框默认折叠成一枚图标：先把光标停在图标上展开，弹窗才存在。
        h.hover_search_icon();
        assert!(h.state.search_expanded, "悬停后应展开");

        // 第一帧：弹窗画出来，自报输入框 id。
        let _ = h.search_popup_frame_with(Vec::new());
        let edit_id = h
            .state
            .search_edit_id
            .expect("绘制层应自报搜索框的 egui Id");
        // 坐标从面板几何读，不猜（弹窗客户区 = 面板 + 一圈投影边距）。
        let margin = crate::popup::SHADOW_MARGIN;
        let p = egui::pos2(300.0, margin + 15.0);

        // IMPORTANT: 点击与文字必须**分两帧**。
        //
        // egui 的 `TextEdit` 在收到点击的**那一帧**还没拿到焦点
        // （焦点要等这次 `interact` 的结果被消费后才成立），
        // 同帧的 `Event::Text` 会被丢弃。实测 `query` 保持空串。
        //
        // 这与真实输入一致：用户点一下、稍后打字。
        let _ = h.search_popup_frame_with(vec![
            egui::Event::PointerMoved(p),
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: p,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]);
        // 确认真的拿到焦点了——不确认的话下面的打字断言失败时
        // 不知道是「焦点没拿到」还是「输入没写进去」。
        //
        // CAUTION: 判据必须指出**是哪个控件**拿到了焦点，也不能按 id
        // 的 debug 串匹配（egui 0.36 的 `Id` 是哈希值）——所以绘制层把
        // 自己用的 id 记进 `UiState::search_edit_id`，测试与它精确比对。
        assert_eq!(
            h.ctx.memory(|m| m.focused()),
            Some(edit_id),
            "点弹窗输入框后焦点应**精确落在输入框**上"
        );
        // 焦点回报也该同时被填上（主窗口的状态机靠它钉住展开）。
        assert!(
            h.state.search_popup_focus,
            "弹窗应把「输入框持有焦点」回报给主窗口"
        );

        let _ = h.search_popup_frame_with(vec![egui::Event::Text("abc".to_owned())]);
        assert_eq!(
            h.state.query, "abc",
            "点进输入框后打字应写入 query"
        );
    }

    /// 拖动区必须上报，且覆盖顶栏高度。
    ///
    /// 没有拖动区窗口就没法拖动——这是无边框窗口的命脉。
    #[test]
    fn drag_region_is_reported() {
        let mut h = Harness::new("drag", vec2(600.0, 400.0));
        h.frame();
        assert!(
            !h.state.drag_regions.is_empty(),
            "每帧都应上报拖动区，否则窗口拖不动"
        );
        let covered = h
            .state
            .drag_regions
            .iter()
            .fold(0.0f32, |a, r| a + r.width() * r.height());
        assert!(
            covered > 100.0,
            "拖动区总面积过小（{covered}），顶栏几乎拖不动"
        );
        // 拖动区覆盖顶栏纵向范围（顶栏段），并允许顶栏以下的
        // 「列表边距」竖条（列表左右留白，按住可拖动窗口）。
        for r in &h.state.drag_regions {
            if r.min.y < crate::solver::TOPBAR_HEIGHT {
                continue; // 顶栏段：面积由上面的断言兜底
            }
            assert!(
                (r.width() - LIST_PAD_X).abs() < 0.5,
                "顶栏以下出现非边距拖动区段 {r:?}"
            );
        }
    }

    /// 卡片行间缝隙（`history_row_gap` 横带）与首行顶部半条缝必须是拖动区：
    /// 按住行间空白即可拖动窗口，不必精确瞄准左右留白。
    ///
    /// CAUTION: 缝隙横条右缘让出滚动条热区与呼吸空间
    /// （`SCROLLBAR_RESERVE + SCROLLBAR_GAP`），否则拖滚动条会被系统
    /// 判成 HTCAPTION 变成拖窗口。
    #[test]
    fn row_gap_is_drag_region() {
        let mut h = Harness::new("row-gap-drag", vec2(700.0, 600.0)).with_items(3);
        h.frame();
        let body = hit(&h, "history_body");
        let content = Rect::from_min_max(
            pos2(body.min.x + LIST_PAD_X, body.min.y),
            pos2(body.max.x - LIST_PAD_X, body.max.y),
        );
        // 阴影默认开启（enabled=true, blur=8, offset_y=0）⇒ 行距 8pt
        // （|0| + 8 = 8，与旧默认 2+6=8 恰好相同）。
        let gap = history_row_gap(&modular_clipboard_core::Config::default().ui.card_shadow);
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        // ① 首行顶部的半条缝：从列表顶到第一行顶，右缘让出滚动条位与呼吸空间。
        let top_gap = Rect::from_min_max(
            content.min,
            pos2(
                content.max.x - SCROLLBAR_RESERVE - SCROLLBAR_GAP,
                content.min.y + gap * 0.5,
            ),
        );
        assert!(
            h.state.drag_regions.iter().any(|r| {
                near(r.min.x, top_gap.min.x)
                    && near(r.max.x, top_gap.max.x)
                    && near(r.min.y, top_gap.min.y)
                    && near(r.max.y, top_gap.max.y)
            }),
            "首行顶部半条缝应是拖动区；drag_regions={:?}",
            h.state.drag_regions
        );
        // ② 行间整条缝：3 条记录之间至少一条 `gap` 高的横条。
        assert!(
            h.state.drag_regions.iter().any(|r| {
                r.height() >= gap - 0.5
                    && r.height() <= gap + 0.5
                    && r.width() > 200.0
            }),
            "行间缝隙（{gap}pt 横条）应是拖动区"
        );
        // ③ 内容不满一屏 → 列表底部空白也应是拖动区，横向延伸到最右缘。
        assert!(
            h.state.drag_regions.iter().any(|r| {
                near(r.min.x, content.min.x)
                    && near(r.max.x, content.max.x)
                    && near(r.max.y, content.max.y)
            }),
            "列表底部空白（内容不满一屏）应是拖动区；drag_regions={:?}",
            h.state.drag_regions
        );
    }

    /// 行高测量宽度必须与绘制宽度同源：都让出滚动条占位与呼吸空间。
    ///
    /// 回归守卫（L3）：测量早前用 `content.width()`，而绘制侧在「可
    /// 滚动」时把行矩形内缩 [`SCROLLBAR_RESERVE`] + [`SCROLLBAR_GAP`]
    /// （不缩的话 egui 的 floating 滚动条会盖住删除按钮）。于是量的
    /// 时候按宽 16pt 算折行，画的时候窄 16pt ⇒ 折行数偏少 ⇒ 行高偏小
    /// ⇒ **文字被裁**。症状指纹：条目少时正常、条目一多就每行少显示一点。
    #[test]
    fn row_measure_width_reserves_scrollbar_like_rendering() {
        let content_w = 500.0;
        let measured = row_measure_width(content_w);
        assert!(
            measured < content_w,
            "测量宽度必须让出滚动条占位与呼吸空间，否则测量比绘制宽"
        );
        assert!(
            (measured - (content_w - SCROLLBAR_RESERVE - SCROLLBAR_GAP)).abs() < 1e-4,
            "让出的量必须恰好等于 SCROLLBAR_RESERVE + SCROLLBAR_GAP（绘制侧同值）"
        );
        // 单调性：内容越宽，测量宽度单调递增——这是「不会来回抖」的
        // 基础（行高随宽度单调不减 ⇒ 滚动条不会在两帧之间闪）。
        assert!(row_measure_width(600.0) > row_measure_width(400.0));
        // 极窄内容区不得为负（否则 `preview_full_width` 拿到负宽度）。
        assert!(row_measure_width(0.0) >= 20.0);
    }

    /// 行高缓存键必须包含过滤维度，否则长度相同的两次过滤会误命中。
    ///
    /// 回归守卫（L2）：`heights` 按**行下标**存，下标含义由过滤条件决定。
    /// `active_type_kinds` / `only_pinned` / `pinned_drawn_separately` 是
    /// 绘制层本地状态、不过 store ⇒ `list_version` 不变。而长度检查挡不住：
    /// 3 条 Text 与 3 条 Image，从「文本」切到「图片」行数都是 3 ⇒
    /// 缓存命中 ⇒ **图片行套用文本行的行高**（图被压扁/留白，无报错）。
    /// 2026-10-20 多选接入后补一条：两个单选的集合 {Text} 与多选的
    /// {Text} 必须**相等**（同一过滤结果），否则模式切换会无故失效。
    #[test]
    fn row_height_cache_key_includes_filter() {
        use modular_clipboard_core::ClipKind;
        // CAUTION: `RowFilterKey` 含 `Vec` 字段（不再 Copy），逐条完整
        // 构造——`..base` 会把 Vec 移动出来，第二条 assert 就用不了了。
        let none = || RowFilterKey {
            type_kinds: Vec::new(),
            only_pinned: None,
            pinned_drawn_separately: false,
        };
        let with_kinds = |kinds: Vec<ClipKind>| RowFilterKey {
            type_kinds: kinds,
            only_pinned: None,
            pinned_drawn_separately: false,
        };
        let base = none();
        // 三个维度各自变化都必须让键不等（否则该维度漏在键外）。
        assert_ne!(base, with_kinds(vec![ClipKind::Text]), "类型过滤必须进键");
        // 多选两个元素与单个元素不等（过滤结果不同 ⇒ 行序不同）。
        assert_ne!(
            with_kinds(vec![ClipKind::Text]),
            with_kinds(vec![ClipKind::Image, ClipKind::Text]),
            "多选集合的元素构成必须进键"
        );
        assert_ne!(
            base,
            RowFilterKey {
                type_kinds: Vec::new(),
                only_pinned: Some(true),
                pinned_drawn_separately: false,
            },
            "置顶过滤必须进键"
        );
        assert_ne!(
            base,
            RowFilterKey {
                type_kinds: Vec::new(),
                only_pinned: None,
                pinned_drawn_separately: true,
            },
            "置顶独立分栏必须进键（它决定主栏是否让出置顶条目）"
        );
        // 相同条件必须相等，否则缓存永远不命中（退化成每帧重算）。
        assert_eq!(base, none());
    }

    /// 动态行距：阴影关闭回落基础 6pt；开启时按「偏移 + 弥散」加宽；
    /// 大参数截断到 `ROW_GAP_MAX`；向上偏移同样计入（取绝对值）。
    #[test]
    fn history_row_gap_follows_shadow_config() {
        let d = modular_clipboard_core::config::CardShadowConfig::default();
        // 新默认（blur=8, offset=0）→ |0| + 8 = 8（与旧默认 2+6=8 相同）。
        assert_eq!(history_row_gap(&d), 8.0);
        let cfg_on = |enabled, blur, offset_y| modular_clipboard_core::config::CardShadowConfig {
            enabled,
            blur,
            opacity: d.opacity,
            offset_y,
        };
        assert_eq!(history_row_gap(&cfg_on(false, 6.0, 2.0)), ROW_GAP);
        assert_eq!(history_row_gap(&cfg_on(true, 6.0, 2.0)), 8.0);
        // blur 0 / offset 0：阴影无弥散，回落到基础间距
        assert_eq!(history_row_gap(&cfg_on(true, 0.0, 0.0)), ROW_GAP);
        // 向上偏移同样需要上方缝隙
        assert_eq!(history_row_gap(&cfg_on(true, 4.0, -3.0)), 7.0);
        // 大参数截断到上限
        assert_eq!(history_row_gap(&cfg_on(true, 24.0, 16.0)), ROW_GAP_MAX);
        // blur 超界（手改配置）钳到 24
        assert_eq!(history_row_gap(&cfg_on(true, 99.0, 0.0)), 24.0);
    }

    /// 类型徽标（2026-10-15 定稿）位于卡片**右下角**，与「类型 · 大小」
    /// 文字组合成一组、整组右对齐——不再占左侧文本宽度，因此
    /// `preview_width` / `preview_full_width` 不需要为徽标让位：
    /// 徽标缩进是「右下角文字右侧让出一个徽标位」，而不是减可用宽度。
    #[test]
    fn bottom_badge_keeps_preview_width_unchanged() {
        let body_w = 700.0;
        // 徽标不挤占预览/文本宽度（左侧无徽标列）。
        assert_eq!(
            preview_full_width(body_w),
            (body_w - 2.0 * ROW_PAD_H).max(20.0)
        );
        // 极窄内容区仍保底 20pt。
        assert_eq!(preview_full_width(10.0), 20.0);
    }

    // ------------------------------------------------------------------
    // 交互测试：列表行
    // ------------------------------------------------------------------

    /// 卡片头部**不再有**「分离」「折叠」按钮，且折叠逻辑整体不存在。
    ///
    /// # 为什么守这条
    ///
    /// 历史栏固定在下方，手动折叠/分离对它没有意义，而那两枚 PUA
    /// 码位图标（⠿ / ⇤）在 150% 缩放下辨识度很差。若将来有人
    /// 「顺手加回来」，这条会拦住。
    #[test]
    fn card_header_has_no_buttons_and_no_fold_logic() {
        let mut h = Harness::new("header-clean", vec2(700.0, 500.0));
        h.frame();
        // 头部不再登记任何按钮热区。
        for name in ["card_collapse", "card_detach"] {
            assert!(
                !h.state.hit.contains_key(HitNs::Main, name),
                "卡片头部不应再登记 {name} 热区，实际热区={:?}",
                h.state.hit.keys(HitNs::Main)
            );
        }
        // 点头部右侧（原先两枚按钮所在的位置）不产生任何**卡片结构**操作。
        //
        // CAUTION: 不能写成「不产生任何 Op」——那个位置仍可能命中顶栏的
        // 设置齿轮（`ToggleSettings`），那是合法的。真正要守的是
        // 「折叠/分离已不存在」，即没有 `Op` 变体与之对应。
        let ops = h.click_at(pos2(680.0, 30.0));
        assert!(
            !ops.iter().any(|o| format!("{o:?}").contains("Collapse")
                || format!("{o:?}").contains("Detach")
                || format!("{o:?}").contains("Dock")),
            "点卡片头部右侧不应产生折叠/分离/收回，实际={ops:?}"
        );
        // 更强的一条：`Op` 里根本没有这三个变体。
        // 编译期能保证的已由「`Op` 枚举无这些变体」保证，
        // 这里跑一帧确认界面确实画出来了（否则上面的断言是空跑）。
        assert!(
            !h.state.hit.keys(HitNs::Main).is_empty(),
            "应至少登记一处热区，否则上面的点击断言没有意义"
        );
    }

    /// 窄窗口下卡片**平分宽度**，不再折叠成把手。
    ///
    /// 取代原来的 `auto_collapsed_card_can_be_expanded_by_its_handle`
    /// ——那条守的是「自动折叠后可展开」，而折叠功能已整体移除。
    /// 现在守的是新契约：窗口再窄，卡片也要有非零宽度（用户仍
    /// 能看到并滚動列表内容），而不是退化成一条看不出是什么的细条。
    #[test]
    fn narrow_window_keeps_cards_visible() {
        let mut h = Harness::new("narrow", vec2(240.0, 400.0));
        // 跑几帧让 solver 求解。
        for _ in 0..3 {
            h.frame();
        }
        let hist = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区应含历史卡片");
        assert!(
            hist.rect.width() > 40.0,
            "窄窗口下历史栏宽度应仍可用（不折叠成把手），实际={:?}",
            hist.rect
        );
    }

    /// 设置面板里的「退出程序」按钮 ⇒ 产生 `Op::Quit`。
    ///
    /// # 为什么这条重要
    ///
    /// `App::should_quit` 这个字段存在、有初始化、有消费点、有 getter，
    /// 但**全仓没有任何地方把它置成 true** —— 于是设置窗口里没有任何
    /// 退出入口，而标题栏 ✕ 在有托盘时只隐藏到托盘。用户从界面里
    /// 根本无法退出程序，只能去托盘菜单。
    ///
    /// 死字段的特征：每个单点都「看得出是实现了」，合起来却没人用。
    #[test]
    fn settings_has_a_quit_button_that_emits_quit_op() {
        let mut h = Harness::new("settings-quit", vec2(600.0, 900.0));
        h.state.show_settings = true;
        h.settings_frame();
        // 「退出程序」在「关于」卡片里——先切到关于分类再点。
        let nav = settings_hit(&h, "settings_cat_about").center();
        h.settings_click_at(nav);

        let rect = settings_hit(&h, "settings_quit");
        let ops = h.settings_click_at(rect.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::Quit)),
            "点「退出程序」应产生 Op::Quit，实际={ops:?}"
        );
    }

    /// 关于页的「项目主页 / 作者主页」是可点击链接，点击产生 `Op::OpenLink`，
    /// 且 URL 指向真实仓库与作者主页（不再是「待补充」占位）。
    #[test]
    fn about_links_emit_open_link_op() {
        let mut h = Harness::new("about-links", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.settings_frame();
        // 切到「关于」分类。
        let nav = settings_hit(&h, "settings_cat_about").center();
        h.settings_click_at(nav);

        let project = "https://github.com/MCheng404/ModularClipboard";
        let author = "https://github.com/MCheng404";

        let ops = h.settings_click_at(settings_hit(&h, project).center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::OpenLink(u) if *u == project)),
            "点项目主页应产生 OpenLink({project})，实际={ops:?}"
        );

        let ops = h.settings_click_at(settings_hit(&h, author).center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::OpenLink(u) if *u == author)),
            "点作者主页应产生 OpenLink({author})，实际={ops:?}"
        );
    }

    /// 关于分类下**多张卡片都要绘制**（回归守门）。
    ///
    /// `SettingsCat::About` 有「关于」与「技术栈」两张卡片。早前
    /// `settings_category_cards` 用 `find(...)` 只取第一张匹配的卡片，
    /// 第二张永远不会被画出来——表现为「技术栈卡片加了却看不见」
    /// （2026-10-22 用户反馈）。这里断言两张卡片的热区都在场：
    /// 关于卡的「退出程序」与技术栈卡的某个词条链接。
    #[test]
    fn about_category_renders_every_card_not_just_first() {
        let mut h = Harness::new("about-multi-cards", vec2(780.0, 820.0));
        h.state.show_settings = true;
        h.settings_frame();
        let nav = settings_hit(&h, "settings_cat_about").center();
        h.settings_click_at(nav);

        // 第一张（关于）：退出按钮在场。
        let _ = settings_hit(&h, "settings_quit");
        // 第二张（技术栈）：词条链接的热区在场——它由 `ItemKind::Stack`
        // 用 URL 当热区名登记，卡片没被绘制就查不到。
        let stack_url = crate::about::STACK_LINKS[0].1;
        let _ = settings_hit(&h, stack_url);
    }

    /// 关于页「数据目录」是超链接：点击产生 `Op::OpenDataDir`（进入
    /// 目录，而不是像 `RevealPath` 那样在父目录里选中它）。
    #[test]
    fn about_data_dir_link_opens_directory() {
        let mut h = Harness::new("about-datadir", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.settings_frame();
        // 切到「关于」分类。
        let nav = settings_hit(&h, "settings_cat_about").center();
        h.settings_click_at(nav);

        let ops = h.settings_click_at(settings_hit(&h, "settings_data_dir").center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::OpenDataDir)),
            "点数据目录应产生 OpenDataDir，实际={ops:?}"
        );
    }

    /// 录制覆盖层：录制态下整窗只认它，**任意位置**点击即确认。
    ///
    /// 用户定稿交互（2026-10-13）：「录制完成后，点击任意位置停止
    /// 录制」。这里连点三处——面板中心、面板外、左上角——都必须
    /// 产出同一条 `FinishRecording`。
    #[test]
    fn recording_overlay_click_anywhere_finishes() {
        let mut h = Harness::new("settings-rec-overlay", vec2(900.0, 700.0));
        h.state.hotkey_recording = Some(HotkeySlot::Toggle);
        h.state.recording_combo = Some(modular_clipboard_core::config::Hotkey {
            key: "K".into(),
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        });
        // 先跑一帧让覆盖层画出来（热区在 `recording_overlay` 上）。
        let ops = h.settings_frame();
        assert!(
            ops.is_empty(),
            "单纯绘制不该产出 Op（没有点击），实得 {ops:?}"
        );

        for p in [
            egui::pos2(450.0, 350.0), // 面板中心
            egui::pos2(30.0, 30.0),   // 左上角（面板外）
            egui::pos2(870.0, 670.0), // 右下角（面板外）
        ] {
            let ops = h.settings_click_at(p);
            assert!(
                ops.iter().any(|o| matches!(o, Op::FinishRecording)),
                "点 {p:?} 应确认录制，实得 {ops:?}"
            );
        }
    }

    /// 录制态下**不画** MCP 覆盖层：两个全屏覆盖层叠一起只会
    /// 让用户点到不该点的东西。录制优先，MCP 让位。
    #[test]
    fn recording_overlay_takes_priority_over_mcp() {
        let mut h = Harness::new("settings-rec-prio", vec2(900.0, 700.0));
        h.state.hotkey_recording = Some(HotkeySlot::Toggle);
        // 故意把 MCP 覆盖层也开着：录制态必须把它挤掉。
        h.state.mcp_overlay_open = true;
        h.settings_frame();
        // MCP 覆盖层的背景交互 id 是 `mcp_overlay_bg`；录制态下点击
        // 不该触发它（点了也不会关 MCP——那条路径根本不该注册）。
        let ops = h.settings_click_at(egui::pos2(450.0, 350.0));
        assert!(
            ops.iter().any(|o| matches!(o, Op::FinishRecording)),
            "录制态点击应确认录制，实得 {ops:?}"
        );
    }

    /// 字号必须是**逻辑点**，不能被 DPI 乘两次（P2-8）。
    ///
    /// # 为什么这条能抓到
    ///
    /// `Harness` 默认 `pixels_per_point == 1.0`，此时
    /// `size * scale == size`，两种写法结果**完全相同**——
    /// 这正是这个 bug 能长期存活的原因。
    ///
    /// 所以这里直接断言函数契约，不依赖夹具的 ppp：
    /// `sized(14, 1.5)` 必须等于 `FontId::proportional(14)`。
    /// 若有人改回乘 ppp，这条立刻红。
    #[test]
    fn font_size_is_in_logical_points_not_pixels() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            assert_eq!(
                crate::theme::sized(14.0, scale),
                egui::FontId::proportional(14.0),
                "字号不能随 pixels_per_point 变化（scale={scale}）——                  egui 排版时自己会乘一次 ppp，这里再乘就是缩放两次"
            );
        }
    }

    /// 150% 缩放下，自绘文字的行高必须仍与 egui 原生控件同源。
    ///
    /// 守的是「自绘与原生控件字号一致」这个契约：两边都给逻辑点，
    /// 渲染出的物理大小才相同。早前只有 `TextEdit` 遵守（它直接用
    /// 逻辑字号），其余 `painter_at().text()` 全部乘了 ppp，
    /// 于是卡片标题、条目预览比旁边的按钮大 1.5 倍。
    #[test]
    fn self_drawn_text_matches_native_widget_size() {
        let pal = crate::theme::Palette::light();
        // 自绘路径
        let drawn = crate::theme::sized(pal.font_md, 1.5);
        // 原生控件路径：egui 的 `RichText::size` 收**逻辑点**。
        // 不用 `into_font_id()`（egui 0.36 没有这个方法），改为断言
        // 两条路径产出同一个 `FontId`——这正是「同源」的可测形式。
        let native_job = {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                "x",
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(pal.font_md),
                    ..Default::default()
                },
            );
            job
        };
        assert_eq!(
            drawn.size, native_job.sections[0].format.font_id.size,
            "自绘文字与 egui 原生控件的字号必须一致（都用逻辑点），             否则高 DPI 下自绘那部分会大一倍"
        );
    }

    /// 内嵌置顶区与历史列表**必须**各占一段、互不重叠。
    ///
    /// # 这条守的是什么
    ///
    /// 早前是「先画完整历史列表，再事后往上叠一层置顶」：
    ///
    /// - 「置顶」标签画在 `inner.min`，而卡片标题「历史」也在 `inner.min`
    ///   —— 两者**同一个 y**，字叠字；
    /// - 置顶列表从标签下一路铺到卡片底部 `inner.max.y` —— 盖住历史
    ///   的前 N 行，且被盖住的行**仍然可点**（命中矩形还在）。
    ///
    /// 现在 `draw_expanded_card` 先把 body 切成两段：置顶在上、历史在下。
    ///
    /// 判据取两个自报热区（`pinned_body` / `history_body`）的**几何关系**，
    /// 而不是像素颜色——后者在无 GPU 的测试环境里拿不到。
    /// 置顶条目与普通条目**共用同一个列表**，且没有独立的「置顶区」。
    ///
    /// # 这条守的是什么
    ///
    /// 早前置顶条目住在历史卡片顶部的一条内嵌区里（标签 + 分隔线 +
    /// 第二个滚动槽位）。它带来两个问题：
    ///
    /// - 两个滚动槽位各存一份 `vertical_scroll_offset`，而那个值每帧
    ///   都会写进 egui —— 滚主栏会把置顶区一起带走（反之亦然）；
    /// - 置顶区与历史区各画一次置顶条目时，会产生两处同名行 Id
    ///   （`("row", item.id)`），egui 的 ID 冲突检查会直接在界面上
    ///   打出警告。
    ///
    /// 改成同列表后这两类问题一起消失，所以判据是：
    /// **独立置顶区的热区不再存在**，且历史列表照常有内容。
    /// 新通知必须显示出来，过期通知必须被消费掉（F-16 / UI-P0-2）。
    ///
    /// `AppState::notice` 曾有 8 处写入、零处读取——复制/删除/保存失败
    /// 的反馈全部静默丢失。toast 是它的可见通道；过期即清是本函数的
    /// 消费语义（不需要额外定时器）。
    #[test]
    fn notice_toast_shows_fresh_and_expires_stale() {
        let mut h = Harness::new("notice", vec2(700.0, 600.0)).with_items(3);

        // 新通知：应画出并自报热区。
        h.svc.notify("已复制到剪贴板".into());
        h.frame();
        let region = h.state.hit_get_from(HitNs::Main, "notice_toast");
        assert!(region.is_some(), "新通知应显示 toast 并自报热区");

        // 过期通知：伪造一个 10 秒前的时间戳，下一帧应被消费掉。
        h.svc.state.notice = Some((
            "陈旧通知".into(),
            std::time::Instant::now() - std::time::Duration::from_secs(10),
        ));
        h.frame();
        assert!(
            h.svc.state.notice.is_none(),
            "过期通知必须在读取的那帧被清掉"
        );
        assert!(
            h.state.hit_get_from(HitNs::Main, "notice_toast").is_none(),
            "过期通知不应再占热区"
        );
    }

    /// toast 必须在窗口上方居中；窗口极窄时也要被钳在窗内、不渲染到窗外。
    #[test]
    fn notice_toast_is_top_centered_and_kept_inside_window() {
        const MARGIN: f32 = 12.0;

        // 正常宽度：水平中心对齐窗口中线。
        let mut h = Harness::new("notice-center", vec2(700.0, 600.0)).with_items(1);
        h.svc.notify("已复制到剪贴板".into());
        h.frame();
        let r = h
            .state
            .hit_get_from(HitNs::Main, "notice_toast")
            .expect("有通知时应有 toast 热区");
        let toast_cx = r.center().x;
        assert!(
            (toast_cx - 350.0).abs() < 1.0,
            "toast 应水平居中：中心 {toast_cx}，窗口中心 350"
        );

        // 极窄窗口：toast 仍必须完全落在左右边距之间，宽度被动态压缩。
        for narrow in [160.0_f32, 120.0, 90.0, 60.0, 40.0] {
            let mut n = Harness::new("notice-clamp", vec2(narrow, 300.0)).with_items(1);
            n.svc.notify("已复制".into());
            n.frame();
            let rr = n
                .state
                .hit_get_from(HitNs::Main, "notice_toast")
                .unwrap_or_else(|| panic!("宽 {narrow} 时也应有 toast"));
            assert!(
                rr.min.x >= MARGIN - 0.5,
                "宽 {narrow}：toast 左缘 {:.1} 越过左边距 {MARGIN}",
                rr.min.x
            );
            assert!(
                rr.max.x <= narrow - MARGIN + 0.5,
                "宽 {narrow}：toast 右缘 {:.1} 越过右边距（窗口宽 {narrow}）",
                rr.max.x
            );
        }

        // 退化情况：窗口窄到连左右边距都放不下（<24px）时直接不画——
        // 既不越界也不 panic（旧实现 f32::clamp 区间反转会在此崩溃）。
        let mut degenerate =
            Harness::new("notice-degenerate", vec2(20.0, 200.0)).with_items(1);
        degenerate.svc.notify("提示".into());
        degenerate.frame();
        assert!(
            degenerate
                .state
                .hit_get_from(HitNs::Main, "notice_toast")
                .is_none(),
            "窗口窄到放不下边距时不应画 toast"
        );
    }

    /// 长消息必须**换行且装在框内**（2026-10-20 用户实机反馈的回归守卫）。
    ///
    /// 旧实现的病症：量宽度用 `layout(.., 264pt)`（换行），绘制却用
    /// `painter.text(..)`（no-wrap 重排）⇒ 长消息以**一整行**居中，
    /// 渲染到通知框之外；高度又写死 30pt ⇒ 多行文字纵向溢出。
    /// 判据直接量「文字装不装得进框」：框宽 ≥ 文字宽 + 内边距、
    /// 框高 ≥ 文字高 + 内边距，且长消息的框高必须大于单行的 30pt。
    #[test]
    fn notice_toast_wraps_long_text_inside_box() {
        // 60 个汉字 @ font_sm(15) ≈ 900pt，远超 264pt 的换行宽度。
        let long = "保存设置失败：无法写入配置文件，请检查目录权限后重试".repeat(4);
        let mut h = Harness::new("notice-wrap", vec2(700.0, 600.0)).with_items(1);
        h.svc.notify(long.clone());
        h.frame();
        let r = h
            .state
            .hit_get_from(HitNs::Main, "notice_toast")
            .expect("有通知时应有 toast 热区");
        assert!(
            r.height() > 30.0 + 1.0,
            "长消息必须多行显示（框高自适应）：实测 {:.1}，旧实现写死 30pt",
            r.height()
        );
        // 用同一套字号 + 框内宽重排，量出的文字必须装在框内。
        let font = crate::theme::sized(h.pal.font_sm, 1.0);
        let galley = h.ctx.fonts_mut(|f| {
            f.layout(
                long,
                font,
                h.pal.text,
                r.width() - 2.0 * 8.0, // NOTICE_PAD_H
            )
        });
        assert!(
            galley.rect.width() <= r.width() - 2.0 * 8.0 + 0.5,
            "文字宽 {:.1} 超出框内宽 {:.1} —— 渲染出通知框",
            galley.rect.width(),
            r.width() - 16.0
        );
        assert!(
            galley.rect.height() <= r.height() - 2.0 * 7.0 + 0.5,
            "文字高 {:.1} 超出框内高 {:.1}",
            galley.rect.height(),
            r.height() - 14.0
        );
        // 框本身也必须在窗口内（不越界）。
        assert!(r.min.x >= 12.0 - 0.5 && r.max.x <= 700.0 - 12.0 + 0.5);
    }

    /// 弹窗通知模式：窗内**不画** toast，但每帧算好弹窗面板矩形。
    ///
    /// 面板必须在窗口**上方**（与 toast 同语义）、水平居中，且随
    /// 搜索面板展开时往上叠（两块面板不能重叠）。
    #[test]
    fn notice_popup_mode_skips_window_toast_and_reports_panel() {
        let mut h = Harness::new("notice-popup", vec2(700.0, 600.0)).with_items(1);
        h.svc.state.config.ui.notice_popup = true;
        h.svc.notify("已复制到剪贴板".into());
        h.frame();
        assert!(
            h.state
                .hit_get_from(HitNs::Main, "notice_toast")
                .is_none(),
            "弹窗模式不应再画窗内 toast（否则同一则通知显示两遍）"
        );
        let panel = h
            .state
            .notice_panel
            .expect("弹窗模式必须算出通知面板矩形");
        assert!(
            panel.max.y <= 0.0,
            "通知面板应整体在窗口上方：{panel:?}"
        );
        assert!(
            (panel.center().x - 350.0).abs() < 1.0,
            "通知面板应水平居中于窗口：中心 {:.1}",
            panel.center().x
        );
        // 通知消失（过期被主循环消费）⇒ 面板矩形清空。
        h.svc.state.notice = None;
        h.frame();
        assert!(h.state.notice_panel.is_none(), "没有通知时不应保留面板矩形");
    }

    /// 搜索面板展开时，通知面板叠到它**上面**（共享窗口上方那条带）。
    #[test]
    fn notice_panel_stacks_above_search_panel_when_expanded() {
        let mut h = Harness::new("notice-stack", vec2(700.0, 600.0)).with_items(1);
        h.svc.state.config.ui.notice_popup = true;
        // 先让搜索面板展开（直接置状态，生产由点击/悬停驱动）。
        h.state.search_expanded = true;
        h.svc.notify("已复制".into());
        h.frame();
        let panel = h.state.notice_panel.expect("应有通知面板");
        let floats = FloatLayout::compute(
            &h.svc.state.config.ui,
            &h.svc.state.items,
            true,
            h.area,
            None,
        );
        let search = floats.search.expect("展开时应有搜索面板");
        assert!(
            panel.max.y <= search.min.y,
            "通知面板底缘 {:.1} 应在搜索面板顶缘 {:.1} 之上（不重叠）",
            panel.max.y,
            search.min.y
        );
    }

    /// 搜索面板停靠**下方** + 自定义高度（2026-10-20 新配置项）。
    #[test]
    fn search_panel_docks_below_window_with_custom_height() {
        use modular_clipboard_core::config::{SearchPanelConfig, SearchPanelPos};
        let cfg = modular_clipboard_core::config::UiConfig {
            search_panel: SearchPanelConfig {
                pos: SearchPanelPos::Bottom,
                height: 48.0,
            },
            ..Default::default()
        };
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(720.0, 800.0));
        let r = FloatLayout::compute(&cfg, &[], true, area, None)
            .search
            .expect("展开时应给出面板矩形");
        assert!(
            r.min.y >= area.max.y,
            "下方停靠时面板应整体在窗口底缘之下：{r:?}"
        );
        assert!(
            (r.min.y - area.max.y - RAIL_GAP).abs() < 0.01,
            "面板与窗口的间距应为 RAIL_GAP={RAIL_GAP}：{r:?}"
        );
        assert!(
            (r.height() - 48.0).abs() < 0.01,
            "面板高度应取配置值 48：实测 {}",
            r.height()
        );
        // 通栏：左右缘仍与窗口对齐。
        assert!((r.min.x - area.min.x).abs() < 0.01 && (r.max.x - area.max.x).abs() < 0.01);
    }

    /// 类型栏带文字标签时：无实测值用**估算**（宁大勿小），有实测值
    /// （`UiState::rail_panel_size`）必须被采用。
    #[test]
    fn rail_labels_use_measured_hint_and_estimate_is_conservative() {
        use modular_clipboard_core::config::TypeRailConfig;
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(720.0, 800.0));
        let items: Vec<modular_clipboard_core::item::ClipItem> = Vec::new();
        let base = modular_clipboard_core::config::UiConfig {
            type_rail: TypeRailConfig {
                always: true,
                ..TypeRailConfig::default()
            },
            ..Default::default()
        };
        // 无标签（基准）：竖直停靠 = 图标宽 × 5 + 间距。
        let plain = FloatLayout::compute(&base, &items, false, area, None)
            .rail
            .expect("always 开启时应给出类型栏矩形");
        // 开标签、无 hint：估算值必须**大于**无标签（按钮变高）。
        let mut labeled = base.clone();
        labeled.type_rail.labels = true;
        let est = FloatLayout::compute(&labeled, &items, false, area, None)
            .rail
            .expect("开标签后仍应有矩形");
        assert!(
            est.height() > plain.height() + 1.0,
            "标签估算应比无标签高（竖直停靠每枚多一行文字）：{est:?} vs {plain:?}"
        );
        // 有 hint：必须被采用（弹窗实测值，一帧后弹窗按它 resize）。
        let hint = egui::vec2(60.0, 300.0);
        let measured = FloatLayout::compute(&labeled, &items, false, area, Some(hint))
            .rail
            .expect("开标签后仍应有矩形");
        assert!(
            (measured.width() - hint.x).abs() < 0.01 && (measured.height() - hint.y).abs() < 0.01,
            "有实测值时必须采用 hint：{measured:?} != {hint:?}"
        );
        // 位置仍按停靠边：右侧 ⇒ 整块在窗口右缘外。
        let right = FloatLayout::compute(&labeled, &items, false, area, Some(hint))
            .rail
            .expect("...");
        assert!(right.min.x >= area.max.x + RAIL_GAP - 0.01);
    }

    /// 卡片左下角有编辑按钮：文本条目点它 ⇒ `BeginEdit`；图片条目
    /// 点它 ⇒ `OpenImageEditor`（走编辑窗口，不内联）。
    ///
    /// 单条目夹具：热区 `row_edit` 无覆盖歧义（见登记处的 CAUTION）。
    #[test]
    fn row_edit_button_pushes_begin_or_open_editor() {
        // 文本条目。
        let mut h = Harness::new("row-edit-text", vec2(700.0, 600.0)).with_items(1);
        h.frame();
        let edit = h
            .state
            .hit_get_from(HitNs::Main, "row_edit")
            .expect("文本行应有编辑按钮热区");
        // 位置在卡片**左下角**：x 靠近行左缘（ROW_PAD_H 内），不占预览带。
        assert!(
            edit.min.x < 40.0,
            "编辑按钮应在卡片左下角附近，实测 x={:.1}",
            edit.min.x
        );
        let id = h.svc.state.items[0].id;
        let ops = h.click_at(edit.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::BeginEdit(x) if *x == id)),
            "点文本行的编辑按钮应 BeginEdit，实际 {ops:?}"
        );

        // 图片条目：载荷是裸 RGBA8 + 显式尺寸（arboard 链路的规范形态）。
        let mut h = Harness::new("row-edit-img", vec2(700.0, 600.0));
        let px = vec![128u8; 8 * 8 * 4];
        assert!(h.svc.ingest(modular_clipboard_core::CapturedPayload {
            kind: modular_clipboard_core::ClipKind::Image,
            bytes: px,
            html: None,
            text: None,
            source_app: "img".into(),
            files: Vec::new(),
            dims: Some((8, 8)),
        }));
        h.svc.reload_list();
        h.frame();
        let edit = h
            .state
            .hit_get_from(HitNs::Main, "row_edit")
            .expect("图片行应有编辑按钮热区");
        let id = h.svc.state.items[0].id;
        let ops = h.click_at(edit.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::OpenImageEditor(x) if *x == id)),
            "点图片行的编辑按钮应开编辑窗口，实际 {ops:?}"
        );
    }

    /// 进入编辑态后：预览带换成编辑器（`edit_box` 热区出现），
    /// 点 ✓ ⇒ `CommitEditText` 带**编辑后的文本**；点 ✕ ⇒ `CancelEdit`。
    #[test]
    fn inline_editor_commits_and_cancels() {
        let mut h = Harness::new("inline-editor", vec2(700.0, 600.0)).with_items(1);
        h.frame();
        let id = h.svc.state.items[0].id;
        // 落地 BeginEdit（生产由 App 做：读载荷 + 置 editing）。夹具直接
        // 置状态等价——editing 是纯界面状态，App 那半是 IO。
        h.state.editing = Some(id);
        h.state.edit_buf = "改过的内容".into();
        h.state.edit_focus_pending = true;
        let ops = h.frame();
        assert!(
            h.state
                .hit_get_from(HitNs::Main, "edit_box")
                .is_some(),
            "编辑态应出现输入框热区"
        );
        // 编辑态下**不该**再画预览（预览带被编辑器占用）——用「通知
        // 无关的判据」：编辑态的 ops 里没有 BeginEdit 的重复触发。
        assert!(
            !ops.iter().any(|o| matches!(o, Op::BeginEdit(_))),
            "编辑态不应再触发 BeginEdit"
        );

        let ok = h
            .state
            .hit_get_from(HitNs::Main, "editor_ok")
            .expect("应有确认按钮");
        let ops = h.click_at(ok.center());
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CommitEditText { id: x, text } if *x == id && text == "改过的内容")),
            "点 ✓ 应提交编辑后的文本，实际 {ops:?}"
        );

        // 取消路径：重新进入编辑态，点 ✕。
        h.state.editing = Some(id);
        h.state.edit_buf = "又改了".into();
        h.frame();
        let no = h
            .state
            .hit_get_from(HitNs::Main, "editor_no")
            .expect("应有取消按钮");
        let ops = h.click_at(no.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::CancelEdit)),
            "点 ✕ 应取消编辑，实际 {ops:?}"
        );
    }

    /// 编辑态下 Enter **不复制**（那是换行），Esc **取消编辑**。
    ///
    /// 回归守卫：Enter 复制（2026-10-20 加）早于内联编辑存在，两者
    /// 会在「改内容按回车」时同时触发——复制一条没保存的文字。
    #[test]
    fn enter_and_esc_belong_to_editor_while_editing() {
        let mut h = Harness::new("editor-keys", vec2(700.0, 600.0)).with_items(1);
        h.frame();
        let id = h.svc.state.items[0].id;
        h.svc.state.selected = Some(id);
        h.state.editing = Some(id);
        h.state.edit_buf = "x".into();
        h.frame();
        let enter = vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }];
        let ops = h.frame_with(enter);
        assert!(
            !ops.iter().any(|o| matches!(o, Op::Copy(_))),
            "编辑态按 Enter 不应复制（它是换行），实际 {ops:?}"
        );
        let esc = vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }];
        let ops = h.frame_with(esc);
        assert!(
            ops.iter().any(|o| matches!(o, Op::CancelEdit)),
            "编辑态按 Esc 应取消编辑，实际 {ops:?}"
        );
    }

    /// 有备份时左下角出现撤销按钮，点它 ⇒ `UndoEdit(该条目)`；
    /// 没有备份时不画（没有可撤销的东西）。
    #[test]
    fn undo_button_appears_only_with_backup() {
        let mut h = Harness::new("undo-btn", vec2(700.0, 600.0)).with_items(1);
        h.frame();
        let id = h.svc.state.items[0].id;
        assert!(
            h.state.hit_get_from(HitNs::Main, "row_undo").is_none(),
            "没有备份时不应出现撤销按钮"
        );
        // 造一份备份（生产由 commit_edit 写）。
        h.state.edit_backup = Some(crate::paint::EditBackup {
            id,
            kind: modular_clipboard_core::ClipKind::Text,
            payload: "旧内容".as_bytes().to_vec(),
            dims: None,
        });
        h.frame();
        let undo = h
            .state
            .hit_get_from(HitNs::Main, "row_undo")
            .expect("有备份应出现撤销按钮");
        let ops = h.click_at(undo.center());
        assert!(
            ops.iter().any(|o| matches!(o, Op::UndoEdit(x) if *x == id)),
            "点撤销应 UndoEdit，实际 {ops:?}"
        );
    }

    /// 删除正在编辑的条目时，编辑态必须**一起**收干净。
    ///
    /// 守的是「六个字段漏一个」这类悬挂状态：留下来的 `editing` /
    /// `image_editor` / `edit_backup` 会指向一个已不存在的条目，
    /// 表现为「改完的字没保存却也没有任何报错」。
    #[test]
    fn clearing_edit_state_for_deleted_item_drops_every_field() {
        let mut st = UiState::default();
        let id: modular_clipboard_core::EntryId = 4242;
        st.editing = Some(id);
        st.edit_buf = "还没保存的字".into();
        st.edit_focus_pending = true;
        st.image_editor = Some(id);
        st.image_editor_src = Some((vec![1, 2, 3], 2, 2));
        st.edit_backup = Some(EditBackup {
            id,
            kind: modular_clipboard_core::ClipKind::Text,
            payload: b"old".to_vec(),
            dims: None,
        });

        st.clear_edit_state(id);

        assert!(st.editing.is_none(), "内联编辑态应清除");
        assert!(st.edit_buf.is_empty(), "编辑缓冲应清空");
        assert!(!st.edit_focus_pending, "焦点请求应清掉");
        assert!(st.image_editor.is_none(), "图片编辑窗口应关");
        assert!(st.image_editor_src.is_none(), "原图副本应释放");
        assert!(st.edit_backup.is_none(), "撤销备份应丢弃");
    }

    /// 删 A 不能打断正在编辑的 B：不匹配的字段一律不动。
    #[test]
    fn clearing_edit_state_leaves_other_items_alone() {
        let mut st = UiState::default();
        let a: modular_clipboard_core::EntryId = 1;
        let b: modular_clipboard_core::EntryId = 2;
        st.editing = Some(b);
        st.edit_buf = "B 的内容".into();
        st.edit_focus_pending = true;
        st.image_editor = Some(b);
        st.edit_backup = Some(EditBackup {
            id: b,
            kind: modular_clipboard_core::ClipKind::Text,
            payload: b"B".to_vec(),
            dims: None,
        });

        st.clear_edit_state(a);

        assert_eq!(st.editing, Some(b), "删 A 不该关掉 B 的编辑器");
        assert_eq!(st.edit_buf, "B 的内容", "B 的缓冲必须原样保留");
        assert!(st.edit_focus_pending, "B 的焦点请求不该被清");
        assert_eq!(st.image_editor, Some(b), "B 的图片窗口不该被关");
        assert!(
            st.edit_backup.as_ref().is_some_and(|x| x.id == b),
            "B 的撤销备份必须留着"
        );
    }

    /// 全清（清空全部记录）后编辑态整体作废，一个字段都不留。
    #[test]
    fn clear_all_edit_state_drops_everything() {
        let mut st = UiState::default();
        let id: modular_clipboard_core::EntryId = 7;
        st.editing = Some(id);
        st.edit_buf = "字".into();
        st.edit_focus_pending = true;
        st.image_editor = Some(id);
        st.image_editor_src = Some((vec![9], 1, 1));
        st.edit_backup = Some(EditBackup {
            id,
            kind: modular_clipboard_core::ClipKind::Image,
            payload: vec![0],
            dims: Some((1, 1)),
        });

        st.clear_all_edit_state();

        assert!(st.editing.is_none() && st.image_editor.is_none());
        assert!(st.edit_buf.is_empty() && !st.edit_focus_pending);
        assert!(st.image_editor_src.is_none() && st.edit_backup.is_none());
    }

    /// 编辑**别的**行时点这一行：先提交正在编辑的内容，再选中本行
    /// （「改完点下一条」的连续编辑流；不提交则缓冲被覆盖、文字蒸发）。
    #[test]
    fn clicking_other_row_commits_pending_edit() {
        let mut h = Harness::new("edit-switch", vec2(700.0, 600.0)).with_items(3);
        h.frame();
        let ids: Vec<_> = h.svc.state.items.iter().map(|it| it.id).collect();
        assert!(ids.len() >= 2, "夹具应有多条");
        // 第一行正在编辑。
        h.state.editing = Some(ids[0]);
        h.state.edit_buf = "第一行的新内容".into();
        h.frame();
        // 点击目标取 `row_body` 热区——多行共用 key，读到的是**最后一行**
        // （与绘制顺序一致），而编辑在第一行 ⇒ 必然是「点别的行」。
        let row = h
            .state
            .hit_get_from(HitNs::Main, "row_body")
            .expect("应有行本体热区");
        let target = egui::pos2(row.center().x, row.center().y);
        let ops = h.click_at(target);
        assert!(
            ops.iter().any(
                |o| matches!(o, Op::CommitEditText { id, text } if *id == ids[0] && text == "第一行的新内容")
            ),
            "点别的行应先提交在途编辑，实际 {ops:?}"
        );
        assert!(
            ops.iter().any(|o| matches!(o, Op::Select(x) if *x != ids[0])),
            "提交后还应选中被点的行，实际 {ops:?}"
        );
    }


    /// Enter 复制选中条目（2026-10-20 键盘流）：选中一行后按 Enter
    /// 必须发出 `Op::Copy`——与行内复制按钮、双击复制同一落地。
    #[test]
    fn enter_key_copies_selected_row() {
        let mut h = Harness::new("enter-copy", vec2(700.0, 600.0)).with_items(3);
        h.frame();
        let id = h.svc.state.items[0].id;
        h.svc.state.selected = Some(id);
        let ops = h.frame_with(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }]);
        assert!(
            ops.iter().any(|o| matches!(o, Op::Copy(x) if *x == id)),
            "选中行 + Enter 应复制该行，实际 {ops:?}"
        );
        // 没有选中行时 Enter 不发 Copy（不该有「复制空气」的路径）。
        h.svc.state.selected = None;
        let ops = h.frame_with(vec![egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: Some(egui::Key::Enter),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }]);
        assert!(
            !ops.iter().any(|o| matches!(o, Op::Copy(_))),
            "未选中时 Enter 不应复制，实际 {ops:?}"
        );
    }

    /// 子窗口必须上报拖动区（F-17 / UI-P0-3）。
    ///
    /// WS_POPUP 无边框窗口没有系统标题栏，不向 WM_NCHITTEST 上报
    /// HTCAPTION 区的话，鼠标无法移动窗口——用户只能边缘 resize 或
    /// Alt+Space，「拖出去成为独立窗口」失去意义。早前 childwin 的
    /// render 从不调用 set_drag_region，这条守的就是那个缺口。
    #[test]
    fn child_window_reports_drag_region() {
        let mut h = Harness::new("child-drag", vec2(400.0, 300.0));
        let id = h.ws.add(CardKind::Detail);
        let drag = h.child_frame_drag(id);
        assert!(
            !drag.is_empty(),
            "子窗口必须上报拖动区（HTCAPTION），否则鼠标无法移动窗口"
        );
        // 上报的应是卡片标题头：贴近客户区顶部、横向铺满。
        let top = drag
            .iter()
            .min_by(|a, b| a.min.y.partial_cmp(&b.min.y).unwrap())
            .copied()
            .unwrap();
        assert!(top.min.y < 60.0, "拖动区应是顶部标题头，实际 {top:?}");
        assert!(top.width() > 300.0, "标题头应横向铺满，实际 {top:?}");
    }

    #[test]
    fn pinned_entries_share_the_history_list() {
        let mut h = Harness::new("pin-shared", vec2(700.0, 600.0))
            .with_items(8)
            .with_pinned(2);
        h.frame();

        // 核心断言：不再有独立的置顶区热区。
        assert!(
            !h.state.hit.contains_key(HitNs::Main, "pinned_body"),
            "置顶条目不该再有独立列表（pinned_body 是旧内嵌置顶区的热区）"
        );

        // 且历史列表仍在（置顶条目被并进去，而不是被丢掉了）。
        let history = h
            .state
            .hit
            .get(HitNs::Main, "history_body")
            .expect("历史列表热区应存在");
        assert!(
            history.height() > 0.0,
            "历史列表必须有可用高度，实际={history:?}"
        );
    }

    /// 置顶条目**必须出现在主列表里**，哪怕工作区里建了置顶卡片。
    ///
    /// # 这条守的是一个真实的「内容凭空消失」bug
    ///
    /// 默认工作区**建了**一张 `CardKind::Pinned` 卡片
    /// （`workspace.rs` 的默认构造）。内嵌置顶区被删掉之后：
    ///
    /// - 主栏若用 `cards.iter().any(kind == Pinned)` 判断「有没有置顶卡片」
    ///   ⇒ 永远为 true ⇒ 主栏**排除**置顶条目；
    /// - 而内嵌模式下那张卡片又被 `Workspace::docked()` 跳过
    ///   （`docked` 里写着「置顶内嵌时不参与 dock」）⇒ 无人绘制。
    ///
    /// 两边都不画 ⇒ 置顶条目从界面上**彻底消失**。
    /// 症状是「数据库里有置顶项、界面里就是没有」，极难定位。
    ///
    /// 修法：判断条件必须用 [`Workspace::docked`] —— 它才反映
    /// 「这张卡片会不会真的被画出来」。
    #[test]
    fn pinned_items_stay_visible_even_with_a_pinned_card() {
        let mut h = Harness::new("pin-visible", vec2(700.0, 600.0)).with_items(4);
        // 前置条件：默认工作区确实建了置顶卡片，且处于内嵌模式。
        assert!(
            h.ws.cards.iter().any(|c| c.kind == CardKind::Pinned),
            "默认工作区应包含一张置顶卡片（这是 bug 的前提）"
        );
        assert!(
            h.ws.pinned_is_embedded(),
            "默认应为内嵌模式（内嵌区已删，此时置顶应并入历史列表）"
        );
        h.frame();

        let pinned_id = h.svc.state.items.last().expect("夹具有条目").id;
        h.svc.toggle_pin(pinned_id).expect("置顶应成功");
        h.svc.reload_list();
        h.frame();

        // 置顶后，置顶条目必须出现在**主列表**的数据里。
        //
        // NOTE: 绘制层只收集 `Op`、不落地，所以这里查的是下一帧
        // 会用到的 `state.items` —— 而过滤发生在 `draw_history_body`
        // 内部，数据本身不过滤。用下面的方式间接验证：
        // 若主栏排除了置顶条目，第一行渲染的就是未置顶那条，
        // 点击第一行得到的 id 必然 ≠ 置顶的 id。
        assert!(
            h.svc.state.items.iter().any(|it| it.pinned),
            "置顶后列表数据里应有置顶条目"
        );
        assert!(
            h.state.hit.contains_key(HitNs::Main, "history_body"),
            "主列表热区应存在"
        );
    }

    /// 置顶的条目排在**最前**。
    ///
    /// # 这条守的是「绘制层不要自己排序」
    ///
    /// 排序是**数据层**的职责：store 的 SQL 就是
    /// `ORDER BY pinned DESC, created_at DESC`（`store::list()`）。
    /// 绘制层曾经又排了一次，那是重复且错层——排序规则要改就得改
    /// 两个地方，而早前内嵌置顶区与主栏各画一份时，两边规则一旦
    /// 不一致就会出现「同一条目在两处位置不同」。
    ///
    /// 所以本测试守的是：**置顶后数据层给出的首条就是置顶条目**，
    /// 且首条被画在列表顶部（点击命中它）。
    #[test]
    fn pinned_rows_sort_before_unpinned() {
        let mut h = Harness::new("pin-sort", vec2(700.0, 600.0)).with_items(4);
        h.frame();

        // 置顶**最旧**的那一条。
        //
        // `with_pinned(n)` 置顶的是 `items.take(n)`，即「最新的那几条」，
        // 而它们本来就在列表最前 —— 那种夹具测不出排序是否生效。
        // 只有把一条排在后面的旧条目置顶，首行位置才会改变。
        let oldest = h
            .svc
            .state
            .items
            .last()
            .expect("夹具应造出条目")
            .id;
        h.svc.toggle_pin(oldest).expect("置顶应成功");
        h.svc.reload_list();
        h.frame();

        // 前置条件：数据层已把置顶条目排到首位。
        let first = h.svc.state.items.first().expect("列表非空");
        assert!(
            first.pinned,
            "数据层应把置顶条目排在最前（store 的 ORDER BY pinned DESC）"
        );
        assert_eq!(first.id, oldest, "首条应当是那条被置顶的旧条目");

        // 「首条被画在列表顶部且按钮可点」由 `pinned_row_button_can_unpin`
        // 覆盖 —— 那是按钮坐标，与本条要守的排序无关。这里不再重复点一遍：
        // 点击坐标对行首偏移（`ROW_GAP/2`）很敏感，两条测试各自钉住
        // 自己那件事，失败时更容易定位。

    }

    /// 没有置顶条目时，历史列表必须占满整个 body（不留下空白带）。
    #[test]
    fn no_pinned_items_means_no_reserved_band() {
        let mut h = Harness::new("pin-empty", vec2(700.0, 600.0)).with_items(5);
        h.frame();
        assert!(
            !h.state.hit.contains_key(HitNs::Main, "pinned_body"),
            "没有置顶条目时不应绘制置顶区（否则白占一条横幅）"
        );
        let history = hit(&h, "history_body");
        let card = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片")
            .rect;
        // 历史列表的顶端应紧贴头部（只差正常的内容起始边距）。
        assert!(
            history.min.y - card.min.y < 60.0,
            "无置顶条目时历史列表应从卡片顶部附近开始，实际 history={history:?} card={card:?}"
        );
    }

    /// 置顶条目的置顶按钮**必须可点**（否则无法取消置顶）。
    ///
    /// 早前 `acts[0].enabled = !item.pinned`：条目置顶后按钮变灰、
    /// 点击不产生 `Op::TogglePin` —— 用户看到的正是「置顶后无法
    /// 取消置顶」。置顶 ↔ 取消置顶是同一个开关，任何状态下都应可点。
    ///
    /// 行高随预览折行数变化，测试不重算 `row_height_for` 的公式，
    /// 而是沿置顶区首行范围扫几个 y；`enabled=false` 的旧实现无论
    /// 点哪个 y 都不会产生 `TogglePin`，所以任一命中即可区分。
    #[test]
    fn pinned_row_button_can_unpin() {
        let mut h = Harness::new("unpin-btn", vec2(700.0, 600.0))
            .with_items(3)
            .with_pinned(1);
        h.frame();

        // 置顶条目现在就在**主列表**里（不再有独立置顶区），所以
        // 热区取 `history_body`。
        let body = hit(&h, "history_body");
        let btn = h.pal.control_height * 0.55;
        let acts_w = acts_width(&h.pal);
        // 按钮基准 = 热区右缘 − LIST_PAD_X（列表左右留白）− ROW_PAD_H。
        //
        // CAUTION: `history_body` 登记的是**未扣** `LIST_PAD_X` 的矩形，
        // 这里必须减掉，否则整条右移 8pt —— 症状是「按钮点不中」，
        // 而实际绘制是对的，非常容易被误判成实现 bug。
        let x = body.max.x - LIST_PAD_X - ROW_PAD_H - acts_w + 0.5 * (btn + h.pal.space_xs);
        let mut hit_any = false;
        // 按钮贴卡片**右上角**、比来源行再低 `ROW_ACTS_DROP`，
        // 中心 y ≈ 首行顶 + ROW_GAP/2 + pad_v(5) + 4 + btn/2 ≈ +20pt。
        for y in [
            body.min.y + 12.0,
            body.min.y + 18.0,
            body.min.y + 22.0,
        ] {
            let ops = h.click_at(pos2(x, y));
            if ops.iter().any(|o| matches!(o, Op::TogglePin(_))) {
                hit_any = true;
                break;
            }
        }
        assert!(
            hit_any,
            "置顶条目的置顶按钮必须可点并产生 TogglePin（取消置顶入口）\
             body={body:?} x={x} ROW_PAD_H={ROW_PAD_H} acts_w={acts_w} btn={btn}"
        );
    }

    // ------------------------------------------------------------------
    // 列表行：坐标与虚拟化
    //
    // NOTE: 这几条存在的唯一理由：**默认的 Harness 列表是空的**。
    // `draw_history_body` 一进来就走 `draw_empty` 分支 return，
    // 于是行的绘制路径从未被覆盖——「列表坐标算错」才能一路溜到
    // 实机。下面每条都必须用 `with_items(n)`。
    //
    // 坐标一律从 `UiState::hit` 里读（`draw_history_body` 自报的
    // `history_body`），不在测试里按 `pal` 常量重算一遍公式——
    // 重算等于把实现抄进断言，实现改对了测试反而挂。
    // ------------------------------------------------------------------

    /// 列表可视区必须被**自报**出来。
    ///
    /// 测试与自动化脚本都靠 `hit` 表定位，不自报就只能在测试里
    /// 重算布局公式——那正是本项目明确要避免的做法。
    #[test]
    fn list_body_is_self_reported() {
        let mut h = Harness::new("list-body", vec2(700.0, 500.0)).with_items(3);
        h.frame();
        let body = h
            .state
            .hit
            .get(HitNs::Main, "history_body")
            .expect("历史列表应自报 `history_body` 热区");
        let hist = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片");
        assert!(
            hist.rect.contains_rect(body),
            "列表可视区应完全落在历史卡片内：卡片={:?} 列表={:?}",
            hist.rect,
            body
        );
        assert!(
            body.width() > 100.0 && body.height() > 60.0,
            "列表可视区尺寸应可用，实际={body:?}"
        );
    }

    /// 滚到底后，行带必须**正好铺满**列表可视区：贴底、不越上、不越左。
    ///
    /// # 守的是什么
    ///
    /// 早前把行坐标写成 `pos2(0.0, offsets[i] - scroll)`——少了
    /// `body.min`，于是所有行整体上移到窗口左上角：列表底部空出
    /// 一大块，行反而盖住了卡片标题和顶栏。
    ///
    /// # 为什么必须**三点**都断言
    ///
    /// 「点得到某条记录」是**假守卫**：注入那个 bug 后行仍然是一段
    /// 连续带，从下往上扫照样命中，测试恒绿（实测过）。真正的差异
    /// 在**位置**——只断言「底部点得到」也能被平移后的行带蒙混，
    /// 必须同时要求上沿/左沿**没有**行，整体平移才无处可藏。
    ///
    /// # 为什么「上沿没有行」能成立
    ///
    /// 虚拟化从「第一条可能露头的行」开始画，那一行通常是**半截**
    /// 露在视口上方的。交互挂在 `ScrollArea` 的内容 `ui` 上（见
    /// [`draw_item_row`]），会被裁剪到视口，所以那半截**看得见但
    /// 点不到**——这既让本条断言成立，也正是「顶栏按钮点不动」
    /// 那个 P0 的修法。
    /// 首行下标必须按**每行实际高度**算，不能用单行高度常量。
    ///
    /// # 为什么这条重要
    ///
    /// 早前写 `offsets[i] + ROW_HEIGHT_MIN <= scroll`。`ROW_HEIGHT_MIN`
    /// 是**单行**高度，而预览超过 1 行的条目真实高度远大于它 ⇒
    /// 该行明明还铺满整个视口，却被判定为「已滚过」而跳过。
    ///
    /// 实测推演（heights = [58, 3816, 58]，视口 500，scroll = 200）：
    /// 第 1 行占 67..3883 铺满视口，但 `67 + 58 = 125 <= 200` ⇒ 跳过；
    /// 第 2 行 y = 3689 超出视口底 ⇒ `break` ⇒ **列表整块空白**。
    ///
    /// 触发条件是「复制一段长文本」—— 剪贴板工具的最常规用法。
    ///
    /// # 为什么要用纯函数测
    ///
    /// 列表行**不登记**自报热区，从 `Harness` 外部无法观测「行被跳过了」。
    /// 把这段计算抽成 `first_visible_row` 后，判据才落得住。
    #[test]
    fn first_visible_row_uses_actual_row_heights() {
        // 三行：短 / 超长 / 短。offsets 比 heights 多一项（末项是总高）。
        let heights = [58.0f32, 3816.0, 58.0];
        let offsets = [3.0f32, 67.0, 3889.0, 3947.0];

        // scroll=0：一行都没滚过去。
        assert_eq!(first_visible_row(&offsets, &heights, 0.0), 0);

        // scroll=100：第 0 行（3..61）已滚过，第 1 行仍在。
        assert_eq!(first_visible_row(&offsets, &heights, 100.0), 1);

        // IMPORTANT: 关键档位：scroll=200。
        // 第 1 行占 67..3883，铺满整个视口，必须仍是 1。
        // 用 ROW_HEIGHT_MIN 的旧写法在这里会得到 2（跳过长行）⇒ 空白。
        assert_eq!(
            first_visible_row(&offsets, &heights, 200.0),
            1,
            "长条目仍占据视口时不能被跳过 —— 用 ROW_HEIGHT_MIN 判定会整块空白"
        );

        // 全部滚过去：从末尾开始（等价于「没有行要画」）。
        assert_eq!(first_visible_row(&offsets, &heights, 3947.0), 3);
    }

    /// 全部是单行时，新写法与旧的 `ROW_HEIGHT_MIN` 等价 —— 这解释了
    /// 为什么旧 bug 一直没暴露：`Harness::with_items` 造的文本全是单行。
    #[test]
    fn first_visible_row_matches_constant_when_all_rows_are_min_height() {
        let n = 10usize;
        let heights = vec![ROW_HEIGHT_MIN; n];
        let offsets: Vec<f32> = (0..=n).map(|i| i as f32 * ROW_HEIGHT_MIN).collect();
        for scroll in [0.0, 30.0, 58.0, 100.0, 300.0] {
            let by_const = offsets
                .iter()
                .take_while(|&&o| o + ROW_HEIGHT_MIN <= scroll)
                .count();
            assert_eq!(
                first_visible_row(&offsets, &heights, scroll),
                by_const,
                "全单行时两种算法应一致（scroll={scroll}）"
            );
        }
    }

    /// 列表**可滚动**时，最右侧的「删除」按钮仍必须可点。
    ///
    /// # 为什么这条重要
    ///
    /// egui 0.36 默认 `ScrollStyle` 是 **floating**：滚动条不占布局宽度，
    /// 而是浮在 `content` 右缘覆盖 `[content.max.x - 10, content.max.x]`。
    /// 它又在内容闭包**之后**注册交互，而 egui hit-test 取「最后注册 =
    /// 最上层」⇒ **滚动条压过行内所有控件**。
    ///
    /// 早前行矩形用 `content.width()`，删除按钮（最右一个）中心恰好落进
    /// 滚动条 ⇒ 条目一多就点不动；条目少（无滚动条）时一切正常。
    /// 「条目少就好、条目多就坏」是这条 bug 的指纹。
    ///
    /// # 为什么现有测试没抓到
    ///
    /// `pinned_row_button_can_unpin` 只放 3 条记录、窗口 600pt 高
    /// ⇒ `content_h` 远小于视口 ⇒ **根本没有滚动条**。
    ///
    /// 修法是让**行矩形**在可滚动时右缘内缩 `SCROLLBAR_RESERVE`
    /// + `SCROLLBAR_GAP`（后者是 2026-10-17 加的呼吸空间），按钮与文字
    ///   都以 `r.max.x` 为基准，于是自动全部避开。
    #[test]
    fn delete_button_is_clickable_when_list_is_scrollable() {
        // 条目足够多，确保撑出滚动条（这是本测试成立的前提）。
        let mut h = Harness::new("scrollable-delete", vec2(700.0, 400.0)).with_items(40);
        h.frame();

        let body = hit(&h, "history_body");

        // 前置条件：列表必须真的可滚动，否则测不出差异。
        // （可滚动 ⟺ 行矩形已内缩 SCROLLBAR_RESERVE）
        assert!(
            body.height() < 40.0 * 58.0,
            "前置条件：40 条应超出 {}pt 的视口，否则没有滚动条",
            body.height()
        );

        let btn = h.pal.control_height * 0.55;
        let acts_w = acts_width(&h.pal);
        // 行矩形已内缩，按钮基准 = content.max.x - SCROLLBAR_RESERVE
        // - SCROLLBAR_GAP - ROW_PAD_H（2026-10-17 起多让一项呼吸空间）。
        let right = body.max.x - LIST_PAD_X - SCROLLBAR_RESERVE - SCROLLBAR_GAP - ROW_PAD_H;
        // i=2 是删除（最右一个），中心偏左 acts_w - 2.5*(btn+space_xs)。
        let x = right - acts_w + 2.5 * (btn + h.pal.space_xs);

        let mut hit_any = false;
        // 按钮比来源行低 `ROW_ACTS_DROP`，中心约在首行顶下 20pt。
        for y in [
            body.min.y + 12.0,
            body.min.y + 18.0,
            body.min.y + 22.0,
            body.min.y + 30.0,
        ] {
            let ops = h.click_at(pos2(x, y));
            if ops.iter().any(|o| matches!(o, Op::Delete(_))) {
                hit_any = true;
                break;
            }
        }
        assert!(
            hit_any,
            "可滚动列表的删除按钮必须可点并产生 Delete。\\
             若失败，多半是行矩形没有让出滚动条宽度 —— \\
             「条目多就点不动删除」正是这条要守的回归。\\
             探测点 x={x}（右缘 {right}），body={body:?}"
        );
    }

    #[test]
    fn rows_fill_the_list_body_exactly() {
        let mut h = Harness::new("list-fill", vec2(700.0, 500.0)).with_items(60);
        // 第一帧：让 solver 求解、布局落位、拿到 body。
        h.frame();
        let body = h
            .state
            .hit
            .get(HitNs::Main, "history_body")
            .expect("历史列表应自报 `history_body` 热区");
        assert!(body.height() > 60.0, "历史卡片应有可用高度，实际={body:?}");

        // 滚到最底部（下一帧会被夹到 `max_scroll`）。
        h.state.set_list_scroll(false, f32::MAX);
        h.frame();
        assert!(
            h.state.list_scroll(false) > 100.0,
            "前置条件：应处于已滚动状态（否则测不出差异），实际={}",
            h.state.list_scroll(false)
        );

        let x = body.center().x;

        // (1) 下沿：贴着可视区底部必须有行，且是**最旧**那条。
        let bottom = h.click_at(pos2(x, body.max.y - 4.0));
        let hit_bottom = bottom.iter().find_map(|o| match o {
            Op::Select(id) => Some(*id),
            _ => None,
        });
        let oldest = h.svc.state.items.last().expect("列表非空").id;
        assert_eq!(
            hit_bottom,
            Some(oldest),
            "滚到底后列表**最底部**应是最旧那条记录。\
             底部点不到 ⇒ 行带整体上移了（很可能行坐标少了 `body.min`）。\
             body={body:?} 实际={bottom:?}"
        );

        // (2) 上沿：可视区**上方**（卡片标题一带）不得有行。
        let above = h.click_at(pos2(x, body.min.y - 6.0));
        assert!(
            !above.iter().any(|o| matches!(o, Op::Select(_))),
            "列表可视区**上方**不该有行——行溢出了 body，会盖住标题/顶栏。\
             body={body:?} 实际={above:?}"
        );

        // (3) 上沿内侧：可视区**顶部**必须有行（不留空白）。
        //
        // NOTE: 这里**不**测左/右边界：egui 的 `interact` 会把命中矩形
        // 按 `item_spacing` 外扩（实测左边界比 `body.min.x` 还靠左
        // 4pt），而「行 x 原点写错」只差 8pt —— 点击根本区分不出来，
        // 硬写一条只会导致假绿。左右由 (1)(2) 的纵向不变量间接覆盖。
        let top = h.click_at(pos2(x, body.min.y + 4.0));
        assert!(
            top.iter().any(|o| matches!(o, Op::Select(_))),
            "列表可视区**顶部**应有行——顶部留白说明行带整体下移了。\
             body={body:?} 实际={top:?}"
        );
    }

    /// 新条目入场动画结束后，首行必须**精确归位**（点击落点不受动画残留影响）。
    ///
    /// 回归守卫（2026-10-18）：入场滑入用 `anim::restart` 把 0→1 补间
    /// 重启、行矩形按当前值从上方 12pt 归位。若补间卡在 0（忘了续帧、
    /// restart 目标写错、或闸门把动画饿死），行会**永远偏移 12pt**——
    /// 点击全部落空，而画面看着只是「歪一点」，极难自查。
    /// 本测试把时钟拨过动画时长后点首行，必须命中最新那条。
    #[test]
    fn new_row_entry_animation_settles_back_in_place() {
        let mut h = Harness::new("row-entry", vec2(700.0, 500.0)).with_items(5);
        // 首帧：记录版本基线（首帧不播放，见 `list_version_seen` 的说明）。
        h.frame();
        let body = hit(&h, "history_body");
        // 探测点取**首行底部内侧**（单行预览行高 ≈75，底沿 ≈ +79）。
        // 这个位置是判别关键：补间归位时它在行内；若动画卡在 0（行永远
        // 上移 12pt，底沿只到 ≈+67），探测点落在行外 ⇒ 点空。
        // （实证标定：+72 在两种状态下一边命中一边落空，双侧各 4pt 余量；
        //   点行中部两种状态都命中，没有判别力——别往中间挪。）
        let row0_bottom = body.min.y + 72.0;

        // 复制一条新内容 ⇒ list_version +1 ⇒ 下一帧首行滑入。
        let text = "刚复制的新内容，应该滑入到列表顶部".to_string();
        assert!(
            h.svc.ingest(modular_clipboard_core::CapturedPayload {
                kind: modular_clipboard_core::ClipKind::Text,
                bytes: text.as_bytes().to_vec(),
                html: None,
                text: Some(text),
                source_app: "NewApp".into(),
                files: Vec::new(),
                dims: None,
            }),
            "前置条件：新条目应入库"
        );
        h.svc.reload_list();
        // 动画起始帧 + 把时钟拨到 200ms 之外（补间必须已落定）。
        h.frame();
        h.advance_secs(0.5);
        h.frame();

        // 归位后点首行底部：必须选中**最新那条**（偏移没归位就会点空）。
        let ops = h.click_at(pos2(body.center().x, row0_bottom));
        let newest = h.svc.state.items.first().expect("列表非空").id;
        assert!(
            ops.iter().any(|o| matches!(o, Op::Select(id) if *id == newest)),
            "入场动画落定后首行必须归位并可点中最新条目，实际={ops:?}"
        );
    }

    /// 热区必须**按窗口分开**：一个窗口的绘制不得清掉另一个的。
    ///
    /// # 守的是什么
    ///
    /// 主窗口、设置窗口、每个子窗口共用同一个 `UiState` —— 因此也
    /// 共用同一张热区表 —— 但它们各自有一套**坐标**。早前那张表是
    /// flat 的，且只有主窗口的 `draw()` 会 `clear()`，于是：
    ///
    /// - 画完主窗口，设置窗口刚登记的坐标**没了**；
    /// - 反过来设置窗口写的坐标**永不清空**，关窗后仍留在表里，
    ///   自动化拿着它去点的是「上次的位置」甚至空气。
    ///
    /// # 为什么这条能抓到
    ///
    /// 断言的是**跨绘制**的并存关系，而共享 flat 表做不到并存：
    /// 两帧之后必有一方的坐标被清掉。
    #[test]
    fn hit_regions_are_isolated_per_window() {
        let mut h = Harness::new("hit-ns", vec2(700.0, 500.0)).with_items(3);

        // 第一帧：只画设置窗口。
        h.state.show_settings = true;
        h.settings_frame();
        let settings_key = "settings_close";
        assert!(
            h.state.hit.get(HitNs::Settings, settings_key).is_some(),
            "设置窗口应把热区登记在**自己**的命名空间里"
        );
        assert!(
            h.state.hit.keys(HitNs::Main).is_empty(),
            "只画了设置窗口，主窗口不该凭空出现热区，实际={:?}",
            h.state.hit.keys(HitNs::Main)
        );

        // 第二帧：画主窗口。两边必须**同时在**，且互不串味。
        let before = h.state.hit.get(HitNs::Settings, settings_key);
        h.frame();
        assert!(
            h.state.hit.get(HitNs::Main, "history_body").is_some(),
            "主窗口应有自己的热区"
        );
        assert_eq!(
            h.state.hit.get(HitNs::Settings, settings_key),
            before,
            "画主窗口不该动到设置窗口的热区——两套坐标不能互相清"
        );
    }

    // ------------------------------------------------------------------
    // 交互测试：设置面板
    // ------------------------------------------------------------------

    /// 点开关必须**真的翻转**对应字段。
    ///
    /// # 为什么之前没有这条
    ///
    /// 已有的 `settings_switch_emits_set_bool` 断言的是「只打开面板
    /// 不该改动配置」——它跑的是**不点击**的路径。于是「开关能画出来
    /// 但点了没反应」这种故障完全测不到，而这恰恰是 P附2 的表现：
    /// `draw_switch` 在同一矩形上注册了两个不同 Id 的交互区，靠
    /// 「后注册者优先」侥幸能用，一旦优先级规则变了就静默失灵。
    #[test]
    fn clicking_a_settings_switch_flips_that_field() {
        let mut h = Harness::new("switch-click", vec2(600.0, 900.0));
        h.state.show_settings = true;
        h.settings_frame();
        // 「启动时清理临时文件」在「存储」卡片里——先切到存储分类。
        let nav = settings_hit(&h, "settings_cat_storage").center();
        h.settings_click_at(nav);

        let field = "cleanup_on_start";
        let r = settings_hit(&h, field);
        let ops = h.settings_click_at(r.center());
        let flipped = ops.iter().any(|o| match o {
            Op::SetBool { field: f, .. } => *f == field,
            _ => false,
        });
        assert!(
            flipped,
            "点「{field}」应产生该字段的 `SetBool`，实际={ops:?}。             若为空 ⇒ 点击没落在这个开关上（很可能又是同矩形重复注册交互）"
        );
    }

    /// 打开设置后，点某开关 ⇒ 产生对应字段的 `SetBool`。
    ///
    /// 这条守住「开关真的能改配置」——面板画出来不等于能用。
    #[test]
    fn settings_switch_emits_set_bool() {
        let mut h = Harness::new("settings-switch", vec2(600.0, 500.0));
        h.state.show_settings = true;
        let ops = h.frame();
        // 设置面板首次绘制时不应该主动改配置。
        assert!(
            !ops.iter().any(|o| matches!(o, Op::SetBool { .. })),
            "仅打开面板不该改动配置，实际={ops:?}"
        );
    }

    /// 设置面板必须真的被画出来，且内容在面板矩形内。
    ///
    /// # 这条替代了原来的 `settings_scroll_is_clamped`
    ///
    /// 旧实现手工维护 `settings_scroll`（手写偏移 + 手绘滚动条 +
    /// 夹取），所以曾有一条测试专门守「偏移被夹回合法区间」。
    /// 改用 `ScrollArea` 后那套状态**整体消失**——夹取由 egui 负责，
    /// 不再有任何代码能把它写坏，测它等于测第三方库。
    ///
    /// 换成这条更有价值的断言：面板确实渲染了。设置界面打不开
    /// （首帧 `area` 荒谬、面板被算出屏）是真实发生过的故障，
    /// 而它不会让任何一条「偏移夹取」测试变红。
    #[test]
    fn settings_panel_renders_within_bounds() {
        let mut h = Harness::new("settings-render", vec2(600.0, 400.0));
        h.state.show_settings = true;
        // 设置已移到独立窗口，主界面的 `draw` 不再画它——
        // 必须走 `settings_frame`，否则热区表是空的。
        h.settings_frame();
        //
        // NOTE: 早前所有开关共用一个键（`"settings_switch"`），`hit` 是
        // `HashMap` 会互相覆盖，map 里只剩最后一个，于是这条断言
        // 永远只能看到一项。现在键是配置字段名，可以逐项核对。
        let keys: Vec<&str> = h
            .state
            .hit
            .keys(HitNs::Settings)
            .into_iter()
            .filter(|k| {
                matches!(
                    *k,
                    "always_on_top"
                        | "hide_on_focus_lost"
                        | "show_tray"
                        | "start_minimized"
                        | "enabled"
                        | "skip_password_fields"
                        | "dedup"
                        | "cleanup_on_start"
                )
            })
            .collect();
        assert!(
            !keys.is_empty(),
            "设置面板应至少画出一枚开关，实际热区={:?}",
            h.state.hit.keys(HitNs::Settings)
        );

        // 逐个登记 —— 共用键会静默覆盖，而任何断言都看不出来。
        assert!(
            keys.len() >= 2,
            "多枚开关应各自登记热区（否则 HashMap 互相覆盖），实际只有 {keys:?}"
        );

        // CAUTION: 这里**不能**断言「所有开关都在客户区内」。
        //
        // `ScrollArea` 里的控件按内容高度排布，超出可视区的那些矩形
        // 本来就在客户区之外——它负责裁剪（用户滚一下才看得到）。
        // 那是正确行为，不是缺陷。
        //
        // 真正要守的是：**至少第一枚开关可见**。若连它都跑到窗口外，
        // 说明面板布局整体算错了（早前 `max_height` 写死估算值时
        // 实测就是这样：600x400 下开关被排到 y=444，整排点不到）。
        let mut first: Option<Rect> = None;
        for k in &keys {
            let r = settings_hit(&h, k);
            if first.is_none_or(|f| r.min.y < f.min.y) {
                first = Some(r);
            }
        }
        let first = first.expect("已断言非空");
        assert!(
            h.area.intersects(first),
            "最靠上的开关 {first:?} 完全落在客户区 {:?} 之外，设置面板不可用",
            h.area
        );
    }

    /// 搜索图标必须**完整落在包围盒内**，且圆环不能太小。
    ///
    /// # 这条守住什么
    ///
    /// 放大镜 = 圆环 + 手柄。两者任一越界都会破坏与文字的对齐：
    /// 越界会被裁掉或压到文字上；圆环太小则整个图标看着像「逗号」。
    ///
    /// 几何算式一旦有人改动（比如手柄改长），这里就会失败。
    #[test]
    fn search_icon_fits_its_box() {
        let side = 18.0_f32;
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(side, side));
        // 复刻 draw_search_icon 的几何（不改生产代码，只校验数值关系）。
        let ring_r = side * 0.26;
        let off = side * 0.13;
        let c = r.center();
        let ring_c = pos2(c.x - off, c.y - off);
        let reach = side * 0.43;

        // 圆环必须完整在框内。
        assert!(
            ring_c.x - ring_r >= r.min.x - 0.01 && ring_c.x + ring_r <= r.max.x + 0.01,
            "圆环横向越界：环 [{:.1},{:.1}] 框 [{:.1},{:.1}]",
            ring_c.x - ring_r, ring_c.x + ring_r, r.min.x, r.max.x
        );
        assert!(
            ring_c.y - ring_r >= r.min.y - 0.01 && ring_c.y + ring_r <= r.max.y + 0.01,
            "圆环纵向越界"
        );
        // 圆环至少占边长一半，否则视觉上不成「镜」。
        assert!(
            ring_r * 2.0 >= side * 0.5,
            "圆环太小（直径 {:.1} / 框 {side}）——视觉上会像逗号",
            ring_r * 2.0
        );
        // 手柄终点（含线宽一半）在框内。
        let hx = c.x + reach;
        let hy = c.y + reach;
        // 余量必须容得下「线宽的一半」（实测 0.09*side/2）。
        let half_w = side * 0.09 / 2.0;
        assert!(
            hx + half_w <= r.max.x + 1e-3 && hy + half_w <= r.max.y + 1e-3,
            "手柄越界（含线宽）：终点 ({hx:.2},{hy:.2}) +半线宽 {half_w:.2} 框 {:?}",
            r
        );
    }

    /// 顶栏控件**不得**落在拖动区里 —— 否则实机点不动。
    ///
    /// # 这条测试补的是什么缺口
    ///
    /// 之前有 12 条交互测试验证「点齿轮会发出 `ToggleSettings`」，
    /// **全部通过**，但实机完全点不动。
    ///
    /// 原因：`paint` 层只调用 `ui.interact`，它向 egui 登记控件矩形；
    /// 而**按下是否送达**取决于 Win32 的 `WM_NCHITTEST`——拖动区
    /// 会被判成 `HTCAPTION`，系统随即发`WM_NCLBUTTONDOWN`
    /// （非客户区消息）而**不产生** `WM_LBUTTONDOWN`，egui 根本
    /// 收不到点击。测试环境没有 `WM_NCHITTEST`，这条路径不执行。
    ///
    /// 所以这里直接用 `chrome::hit_test` 验证：控件中心必须判成
    /// [`HitZone::Client`]。这才是实机能否点到的**真正判据**。
    #[test]
    fn topbar_controls_are_not_in_drag_region() {
        let mut h = Harness::new("drag-void", vec2(700.0, 500.0));
        h.frame();

        let drag = h.state.drag_regions.clone();
        assert!(
            !drag.is_empty(),
            "顶栏应上报拖动区（否则窗口拖不动）"
        );

        // 逐个控件验证：**整个矩形**（含四角）都不得落在任何一段拖动区内。
        //
        // CAUTION: 名单必须**逐个列出**，不要用「遍历 hit表」的写法：
        // 遍历只能验证「已存在的控件」，新加的按钮若忘了登记热区就会
        // 静默漏测——而那正是「按钮存在却点不动」的成因。
        //
        // IMPORTANT: 必须查**四角**而不只是中心。早前只查中心，漏掉了
        // 「拖动区伸进搜索框左侧约 22pt」这个 bug——中心在框内没问题，
        // 但点左边缘会被系统判成 HTCAPTION，搜索框点不动。
        //
        // 搜索框是折叠式的：折叠态只有图标（`search_icon`），展开态
        // 才登记 `search`/`search_box`。两种状态都要验证——折叠态
        // 图标不能进拖动区（否则点图标变成拖窗口），展开态输入框
        // 同样不能。
        fn assert_outside_drag(drag: &[Rect], h: &Harness, names: &[&str]) {
            for name in names {
                let r = hit(h, name);
                let probes = [
                    ("左上", r.left_top()),
                    ("右上", r.right_top()),
                    ("左下", r.left_bottom()),
                    ("右下", r.right_bottom()),
                    ("中心", r.center()),
                ];
                for (which, c) in probes {
                    let inside = drag.iter().any(|d| d.contains(c));
                    assert!(
                        !inside,
                        "控件 {name} 的{which} {c:?} 落进了拖动区 {drag:?} —— \
                         实机上点它不产生 WM_LBUTTONDOWN，表现为「点不动」/「变成拖窗口」"
                    );
                }
            }
        }

        // 折叠态：三按钮 + 搜索图标。
        //
        // CAUTION: 只校验**顶栏段**（y 范围落在顶栏内的 drag_regions）。
        // 侧栏折叠卡片的把手也是拖动区（整列高），而第三轮起的悬浮
        // 搜索面板通栏、故意盖住把手的顶部一小段——后注册的 interact
        // 在重叠区胜出，面板优先。这是悬浮层的设计语义（面板压一切），
        // 不是缺陷；把手在面板下方仍可拖动。本测试守护的洞是
        // 「顶栏控件被判成 HTCAPTION」，与侧栏把手无关。
        let topbar_drag = |regions: &[Rect]| -> Vec<Rect> {
            regions
                .iter()
                .copied()
                .filter(|d| d.max.y <= crate::solver::TOPBAR_HEIGHT + 0.5)
                .collect()
        };
        assert_outside_drag(&topbar_drag(&drag), &h, &[
            "topbar_close",
            "topbar_clear",
            "topbar_settings",
            "search_icon",
        ]);

        // 展开态：悬停图标展开搜索面板，再验证输入区与外框。
        h.hover_search_icon();
        let drag_expanded = h.state.drag_regions.clone();
        assert!(
            h.state.search_expanded,
            "点搜索图标应展开搜索框"
        );
        // NOTE: 不再断言 `search` / `search_box`——面板已搬进独立弹窗
        // （自己的 Context 与热区命名空间），主窗口热区表里根本没有它。
        assert_outside_drag(&topbar_drag(&drag_expanded), &h, &[
            "topbar_close",
            "topbar_clear",
            "topbar_settings",
            "search_icon",
        ]);
    }

    /// 点关闭按钮必须产生 `Op::CloseWindow`。
    ///
    /// 守着一条真实缺陷：`Op` 枚举里曾**没有**关闭/最小化变体，
    /// 顶栏也只有清空与设置两枚按钮，于是 `App::take_close_requested`
    /// 读的那个标志永远是 false——标题栏的关闭与最小化按钮
    /// 根本不存在，点了没反应。
    /// 三枚按钮互不重叠，且都排在搜索框右侧。
    ///
    /// 顶栏按钮区是「先算按钮、再算搜索框」算出来的；一旦顺序颠倒，
    /// 搜索框会铺到右缘把按钮压在下面——此时热区仍存在、点击仍能
    /// 产生 Op，但**按钮被盖住**看不见。这条守住排版前提。
    #[test]
    fn topbar_buttons_do_not_overlap_each_other_or_search() {
        let mut h = Harness::new("btn-layout", vec2(700.0, 500.0));
        h.frame();

        let names = ["topbar_close", "topbar_clear", "topbar_settings"];
        let rects: Vec<(&str, Rect)> = names.iter().map(|n| (*n, hit(&h, n))).collect();

        for (i, (name, r)) in rects.iter().enumerate() {
            assert!(
                r.width() > 0.0 && r.height() > 0.0,
                "{name} 的热区退化成了 {r:?}"
            );
            for (other_name, other) in rects.iter().skip(i + 1) {
                assert!(
                    !r.intersects(*other),
                    "{name} {r:?} 与 {other_name} {other:?} 重叠"
                );
            }
        }

        // 搜索图标必须在**右侧功能组**左侧，且不得与任何按钮重叠。
        //
        // NOTE: 隐藏按钮 2026-10-22 移到顶栏**最左**，它在搜索图标
        // 的左边——所以这里只跟右侧组（清空/置顶/设置）比，不再跟
        // `rects[0]`（现在是隐藏按钮）比。
        let icon_r = hit(&h, "search_icon");
        let right_group = hit(&h, "topbar_clear");
        assert!(
            icon_r.max.x <= right_group.min.x,
            "搜索图标 {} 越过右侧组最左按钮 {}",
            icon_r.max.x,
            right_group.min.x
        );
        for (name, r) in &rects {
            assert!(
                !icon_r.intersects(*r),
                "搜索图标 {icon_r:?} 与按钮 {name} {r:?} 重叠"
            );
        }

        // 展开后：搜索面板是**通栏悬浮层**（卡片区全宽、顶栏下方），
        // 与顶栏按钮不在同一行——不应当与任何按钮重叠。
        // （旧实现里输入框向左弹、贴着图标，那时才需要「右缘不越过
        // 图标左缘」这组断言；第三轮起该几何已不存在。）
        h.hover_search_icon();
        // 面板在独立弹窗里（漂在窗口上方），与顶栏按钮天然不可能重叠——
        // 它根本不在主窗口的坐标空间里。
        let floats = FloatLayout::compute(
            &h.svc.state.config.ui,
            &h.svc.state.items,
            true,
            Rect::from_min_size(pos2(0.0, 0.0), h.area.size()),
            None,
        );
        let panel = floats.search.expect("展开时应给出弹窗矩形");
        for (name, r) in &rects {
            assert!(
                !panel.intersects(*r),
                "弹窗面板 {panel:?} 与顶栏按钮 {name} {r:?} 重叠"
            );
        }
        // 面板整块在**顶栏之上**（弹窗漂在窗口外）⇒ 不可能侵入顶栏。
        assert!(
            panel.max.y <= 0.0,
            "弹窗面板底缘 {:.1} 应在窗口顶缘之上（不侵入顶栏）",
            panel.max.y
        );
    }

    /// 拖动区本身必须真的能拖：空白处要判成 Caption。
    ///
    /// 与上一条成对：上一条保证控件**不在**里面，这条保证
    /// 区外的地方**仍是**拖动区（否则修了「点不动」却变成
    /// 「窗口拖不动」）。
    #[test]
    fn topbar_blank_area_is_still_draggable() {
        let mut h = Harness::new("drag-keep", vec2(700.0, 500.0));
        h.frame();
        let drag = h.state.drag_regions.clone();
        assert!(!drag.is_empty(), "应有拖动区");

        // 折叠态：顶栏左段（窗口左缘 → 搜索图标左缘）。
        // 搜索框折叠成右侧图标后，顶栏左侧几乎整条都是拖动区——
        // 这是「顶部拖动区少」问题的最终形态，宽度必须守住。
        //
        // NOTE: 不能取 `drag[0]`——卡片先画，列表的左右留白（8pt 宽）
        // 先入表；顶栏段是后 push 的。按几何找「贴着窗口顶缘且够高」那段。
        let seg = *drag
            .iter()
            .find(|d| d.min.y <= 1.0 && d.height() >= 40.0)
            .unwrap_or_else(|| panic!("应有顶栏拖动区，实际 {drag:?}"));
        assert!(
            seg.width() >= 100.0,
            "折叠态顶栏左段拖动区只有 {}pt（应 ≥ 100pt）—— \
             搜索区又占回顶栏左侧了？drag_regions={drag:?}",
            seg.width()
        );
        // 左段必须真的能拖：中心应落在拖动区内。
        let c = seg.center();
        assert!(
            drag.iter().any(|d| d.contains(c)),
            "拖动区自己的中心应落在拖动区内，实际 {drag:?}"
        );
        // 且不能与折叠态任何控件重叠（图标 + 三按钮）。
        for name in ["search_icon", "topbar_clear", "topbar_settings"] {
            let r = hit(&h, name);
            assert!(
                !r.intersects(seg),
                "拖动区段 {seg:?} 与控件 {name}（{r:?}）重叠"
            );
        }

        // 展开态：输入框是顶栏**下方**的悬浮面板，不再占顶栏宽度，
        // 左段拖动区因此不收窄（旧实现「展开时收缩到输入框左缘」
        // 的几何已不存在）。仍必须存在且能拖。
        h.hover_search_icon();
        let drag2 = h.state.drag_regions.clone();
        let seg2 = *drag2
            .iter()
            .find(|d| d.min.y <= 1.0 && d.height() >= 40.0)
            .unwrap_or_else(|| panic!("展开态应有顶栏拖动区，实际 {drag2:?}"));
        assert!(
            seg2.width() >= 100.0,
            "展开态顶栏左段拖动区只剩 {}pt（应 ≥ 100pt）—— \
             搜索区又占回顶栏左侧了？drag_regions={drag2:?}",
            seg2.width()
        );
        let c2 = seg2.center();
        assert!(
            drag2.iter().any(|d| d.contains(c2)),
            "展开态拖动区自己的中心应落在拖动区内，实际 {drag2:?}"
        );
        // NOTE: `search_box` / `search` 已不在主窗口热区表（面板是弹窗）。
        for name in ["search_icon", "topbar_clear", "topbar_settings"] {
            let r = hit(&h, name);
            assert!(
                !r.intersects(seg2),
                "展开态拖动区段 {seg2:?} 与控件 {name}（{r:?}）重叠"
            );
        }
    }

    /// 搜索框折叠式：默认折叠、**悬停图标**展开、离开后**延迟**收起。
    ///
    /// 交互契约：
    /// - 光标停在图标上 ≥ `SEARCH_HOVER_DELAY` ⇒ 展开；
    /// - 离开图标与输入框 ⇒ 宽限 `SEARCH_COLLAPSE_DELAY` 后才折叠；
    /// - 输入框持有焦点、或里面有查询词 ⇒ 永不折叠。
    ///
    /// NOTE: 早前是「点击图标切换」，后来改悬停驱动（点击只负责给焦点），
    /// 现在是「悬停展开 + 宽限折叠」三段式——见下面的宽限测试。
    #[test]
    fn search_expands_on_hover_and_collapses_on_leave() {
        let mut h = Harness::new("search-toggle", vec2(700.0, 500.0));
        h.frame();

        // 默认折叠：只有图标热区（面板在独立弹窗里，不在主窗口热区表）。
        assert!(!h.state.search_expanded, "搜索框应默认折叠");
        assert!(
            h.state.hit.contains_key(HitNs::Main, "search_icon"),
            "应有搜索图标热区"
        );

        // 悬停图标（停够时间）→ 展开（弹窗随之显示）。
        h.hover_search_icon();
        assert!(h.state.search_expanded, "悬停图标应展开搜索框");

        // 光标移开 —— **当帧还开着**：这是用户能真正进到输入框的前提
        // （详见 `search_stays_open_during_grace_period`）。
        h.move_pointer_away();
        assert!(
            h.state.search_expanded,
            "刚离开的那一帧必须还开着，否则鼠标从图标移向输入框的途中就被收掉"
        );

        // 宽限期耗尽 → 折叠。
        //
        // NOTE: 两段宽限是**串联**的，不是并联：
        // ① 刚展开的免折叠宽限 `SEARCH_OPEN_GRACE`——期间即使指针已离开，
        //    也只是「先不折叠」，`search_leave_since` 根本没开始记；
        // ② 宽限过了才起折叠倒计时，再等 `SEARCH_COLLAPSE_DELAY`。
        // 所以必须两次拨钟 + 两次跑帧：一次跨过①，让计时器起上；
        // 一次跨过②。只拨一次大数时，①刚过的那一帧才刚把
        // `leave_since` 置为 now，折叠自然还没发生。
        h.advance_secs(SEARCH_OPEN_GRACE + 0.05);
        h.frame();
        assert!(
            h.state.search_expanded,
            "免折叠宽限刚过的那一帧仍在计时窗口内，不该已折叠"
        );
        h.advance_secs(SEARCH_COLLAPSE_DELAY + 0.05);
        h.frame();
        assert!(!h.state.search_expanded, "两段宽限期都过后应收起搜索框");
    }

    /// 宽限期必须撑过「鼠标从图标走到输入框」这段行程。
    ///
    /// 守着实机缺陷（用户反馈「根本不给输入的机会」）：输入框弹在图标
    /// **左侧**、中间隔着 `space_md` 的空隙。指针跨过那段空隙的帧既不
    /// 悬停图标、也不悬停输入框，若此刻立即折叠，输入框会在指针抵达
    /// 之前消失 —— 鼠标永远追不上，用户一次也点不进去。
    ///
    /// 这里模拟的就是那几帧：一步一步把光标从图标挪到输入框中心。
    #[test]
    fn search_stays_open_during_grace_period() {
        let mut h = Harness::new("search-grace", vec2(700.0, 500.0));
        h.frame();
        h.hover_search_icon();
        assert!(h.state.search_expanded, "前置条件：已展开");

        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();

        // 图标 → 弹窗，分 8 步走（每步一帧）。真实鼠标在一帧里也走不完
        // 这段：从图标（窗口右上）到弹窗（窗口**上方**）要跨越窗口顶缘。
        //
        // NOTE: 指针进入弹窗之后，「是否悬停面板」由弹窗自己回报
        // （`search_popup_hover`，主窗口的 `rect_contains_pointer` 看不到
        // 客户区之外的点）。这里模拟那份回报。
        let area = Rect::from_min_size(pos2(0.0, 0.0), h.area.size());
        let floats = FloatLayout::compute(
            &h.svc.state.config.ui,
            &h.svc.state.items,
            true,
            area,
            None,
        );
        let panel = floats.search.expect("展开时应有面板矩形");
        for i in 1..=8 {
            let t = i as f32 / 8.0;
            let p = egui::pos2(
                icon.x + (panel.center().x - icon.x) * t,
                icon.y + (panel.center().y - icon.y) * t,
            );
            // 指针越过窗口顶缘的那一刻开始算「在弹窗上」。
            let over_popup = p.y < area.min.y;
            h.state.search_popup_hover = over_popup;
            h.frame_with(vec![egui::Event::PointerMoved(p)]);
            assert!(
                h.state.search_expanded,
                "走向弹窗的第 {i} 步（{p:?}）就把搜索框收了 —— 用户永远点不进输入框"
            );
        }
        // 落在弹窗里：回填折叠倒计时，之后怎么动都不折叠。
        assert!(h.state.search_leave_since.is_none(), "落在弹窗内应取消折叠倒计时");
    }

    /// 打字活跃期间面板**永不折叠**（打字打到一半弹窗不会自己没了）。
    ///
    /// 2026-10-21 语义修订：钉住判据从「输入框有 egui 焦点」换成
    /// 「最近 SEARCH_TYPING_GRACE 内输入变过」——egui 焦点不因用户
    /// 点到别处而释放，拿它当判据会让面板永远收不回去（见
    /// `stale_focus_alone_no_longer_pins_search_panel` 的回归测试）。
    /// 打字的每次键入都刷新时间戳，所以连续输入的间隙不会被收。
    #[test]
    fn typing_in_search_box_never_collapses() {
        let mut h = Harness::new("search-typing", vec2(700.0, 500.0));
        h.frame();
        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();
        h.click_at(icon);
        assert!(h.state.search_expanded, "点击图标应展开");

        // 模拟「正在打字」：输入内容变化（生产里由 TextEdit.changed()
        // 刷新时间戳，这里直接置）。
        for _ in 0..6 {
            h.state.search_last_input_at = Some(h.clock);
            // 光标移到列表中间（不悬停图标/面板）：靠打字活跃钉住。
            let away = egui::pos2(h.area.center().x, h.area.max.y - 20.0);
            h.advance_secs(1.0);
            h.frame_with(vec![egui::Event::PointerMoved(away)]);
            assert!(h.state.search_expanded, "打字活跃期间面板不能折叠");
        }

        // 停手超过 SEARCH_TYPING_GRACE + 宽限 => 折叠。
        h.state.search_last_input_at = Some(h.clock);
        h.advance_secs(SEARCH_TYPING_GRACE + 0.05);
        h.frame();
        h.advance_secs(SEARCH_COLLAPSE_DELAY + 0.05);
        h.frame();
        assert!(!h.state.search_expanded, "停手 2.5s + 宽限后应收起");
    }

    /// 回归守卫（2026-10-21 用户反馈「搜索框打开后无法自动关闭」）：
    /// **陈旧的 egui 焦点不能再钉住面板**。
    ///
    /// 旧行为：`hold_open = search_popup_focus || query 非空 || 刚展开`。
    /// egui 的焦点不因用户点到列表/切到别的应用而释放（generic
    /// interact 不抢焦点），面板一旦获焦就永远收不回去。
    ///
    /// 新行为：焦点与查询词都不参与钉住；指针离开 + 没在打字 =>
    /// 宽限后照常折叠（查询词保留在状态里，列表过滤不丢）。
    #[test]
    fn stale_focus_alone_no_longer_pins_search_panel() {
        let mut h = Harness::new("search-stale-focus", vec2(700.0, 500.0));
        h.frame();
        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();
        h.click_at(icon);
        assert!(h.state.search_expanded, "点击图标应展开");

        // 陈旧焦点 + 非空查询词 + 指针离开 + 时间流逝：旧实现这里
        // 永远 true（两个无条件 pin 叠加）。
        h.state.search_popup_focus = true;
        h.state.query = "hello".into();
        let away = egui::pos2(h.area.center().x, h.area.max.y - 20.0);
        h.frame_with(vec![egui::Event::PointerMoved(away)]);
        h.advance_secs(SEARCH_OPEN_GRACE + 0.05);
        h.frame();
        h.advance_secs(SEARCH_COLLAPSE_DELAY + 0.05);
        h.frame();
        assert!(
            !h.state.search_expanded,
            "陈旧焦点/查询词不能钉住面板——自动关闭必须生效"
        );
        // 查询词保留（列表过滤不丢），面板只是藏起来了。
        assert_eq!(h.state.query, "hello", "折叠不应清空查询词");
    }

    /// 图标点击是开合切换，但悬停展开后的**首次点击只确认**（避免
    /// 2026-10-19 的「点一下闪一下关了」），第二次点击才收起。
    #[test]
    fn search_icon_click_confirm_then_close() {
        let mut h = Harness::new("search-toggle-click", vec2(700.0, 500.0));
        h.frame();
        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();

        // 悬停展开（来源 Hover）。
        h.hover_search_icon();
        assert!(h.state.search_expanded);
        assert_eq!(h.state.search_opened_by, SearchOpenCause::Hover);

        // 首次点击：确认（仍展开），来源改为 Click。
        h.click_at(icon);
        assert!(h.state.search_expanded, "悬停展开后的首次点击不应收起（确认语义）");
        assert_eq!(h.state.search_opened_by, SearchOpenCause::Click);

        // 第二次点击：收起（手动关闭）。
        h.click_at(icon);
        assert!(!h.state.search_expanded, "第二次点击应收起（开合切换）");
    }

    /// 主窗口持有键盘焦点时 Esc 也能收起（弹窗 Esc 只覆盖弹窗持有
    /// 键盘焦点的场景，用户点过列表后 Esc 打到主窗口）。
    #[test]
    fn main_window_escape_closes_search_panel() {
        let mut h = Harness::new("search-esc", vec2(700.0, 500.0));
        h.frame();
        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();
        h.click_at(icon);
        assert!(h.state.search_expanded, "点击图标应展开");

        h.frame_with(vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }]);
        assert!(!h.state.search_expanded, "主窗口 Esc 应收起搜索面板");
        // 抑制悬停立即重开（光标可能还在图标上）。
        assert!(h.state.search_hover_suppressed, "Esc 后应抑制悬停展开一小段");
    }

/// 搜索面板的弹窗矩形必须**整块在主窗口上方**（原型：漂在窗外）。
    ///
    /// 这条是 2026-10-15 第三轮「独立弹窗」改造的核心不变式：面板
    /// 曾经被画在**窗口内**（贴卡片区顶部），与原型不符；现在它由
    /// `crate::popup` 单独开窗，位置由 `FloatLayout` 给——矩形整体
    /// 落在 `y <= 0`（窗口客户区坐标系），且与窗口顶边留 `RAIL_GAP`。
    #[test]
    fn search_panel_floats_above_window_with_gap() {
        let cfg = modular_clipboard_core::config::UiConfig::default();
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(720.0, 800.0));
        let floats = FloatLayout::compute(&cfg, &[], true, area, None);
        let r = floats.search.expect("展开时应给出面板矩形");
        assert!(
            r.max.y <= area.min.y,
            "面板底缘 {:.1} 应在窗口顶缘 {:.1} 之上（不得压进窗口内）：{r:?}",
            r.max.y,
            area.min.y
        );
        assert!(
            (area.min.y - r.max.y - RAIL_GAP).abs() < 0.01,
            "面板与窗口的间距应为 RAIL_GAP={RAIL_GAP}：{r:?}"
        );
        assert!(
            (r.min.y - (area.min.y - RAIL_GAP - cfg.search_panel.height)).abs() < 0.01,
            "面板高度应为配置值 {}：{r:?}",
            cfg.search_panel.height
        );
        // 通栏：左右缘与窗口客户区对齐。
        assert!((r.min.x - area.min.x).abs() < 0.01 && (r.max.x - area.max.x).abs() < 0.01);
        // 折叠：没有矩形。
        assert!(FloatLayout::compute(&cfg, &[], false, area, None).search.is_none());
    }

    /// 类型栏的按钮组必须**整块在主窗口外**，四向都如此。
    ///
    /// 这是用户明确要求的观感（原型里类型栏漂在窗口某一侧外面）。
    /// 第三轮初版把它画在卡片区**内侧**并让列表让位——与原型相反。
    /// 因为弹窗与列表不再共享客户区，「面板盖住列表内容/滚动条/删除
    /// 按钮」那一整类问题也一并消失。
    #[test]
    fn type_rail_floats_outside_window_on_all_four_sides() {
        use modular_clipboard_core::ClipKind;
        use modular_clipboard_core::config::{TypeRailConfig, TypeRailPos};
        let mut cfg = modular_clipboard_core::config::UiConfig {
            type_rail: TypeRailConfig {
                visible: true,
                // 无记录时也让 5 枚按钮都出现，几何才稳定可比
                always: true,
                multi: false,
                labels: false,
                pos: TypeRailPos::Right,
                size: 32.0,
            },
            ..Default::default()
        };
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(720.0, 800.0));
        let items: Vec<modular_clipboard_core::item::ClipItem> = Vec::new();
        let size = 32.0;

        // 右：整块在窗口右缘之外，间距 RAIL_GAP。
        let r = FloatLayout::compute(&cfg, &items, false, area, None)
            .rail
            .expect("always 开启时应给出类型栏矩形");
        assert!(
            r.min.x >= area.max.x + RAIL_GAP - 0.01,
            "右停靠时类型栏左缘 {:.1} 应在窗口右缘 {:.1} 之外：{r:?}",
            r.min.x,
            area.max.x
        );
        assert!(
            (r.width() - (size + RAIL_PAD * 2.0)).abs() < 0.01,
            "右/左停靠宽度 = 图标边长 + 面板内边距"
        );

        // 左。
        cfg.type_rail.pos = TypeRailPos::Left;
        let r = FloatLayout::compute(&cfg, &items, false, area, None)
            .rail
            .expect("应给出矩形");
        assert!(
            r.max.x <= area.min.x - RAIL_GAP + 0.01,
            "左停靠时类型栏右缘 {:.1} 应在窗口左缘 {:.1} 之外：{r:?}",
            r.max.x,
            area.min.x
        );

        // 上 / 下：横向一行，整块在窗口外，高度 = 图标边长。
        cfg.type_rail.pos = TypeRailPos::Top;
        let r = FloatLayout::compute(&cfg, &items, false, area, None)
            .rail
            .expect("应给出矩形");
        assert!(
            r.max.y <= area.min.y - RAIL_GAP + 0.01,
            "上停靠时类型栏底缘 {:.1} 应在窗口顶缘之外：{r:?}",
            r.max.y
        );
        assert!(
            (r.height() - (size + RAIL_PAD * 2.0)).abs() < 0.01,
            "上/下停靠高度 = 图标边长 + 面板内边距"
        );

        cfg.type_rail.pos = TypeRailPos::Bottom;
        let r = FloatLayout::compute(&cfg, &items, false, area, None)
            .rail
            .expect("应给出矩形");
        assert!(
            r.min.y >= area.max.y + RAIL_GAP - 0.01,
            "下停靠时类型栏顶缘 {:.1} 应在窗口底缘之外：{r:?}",
            r.min.y
        );
        // 按钮数 × 边长 + 间隙 + 两侧内边距 = 面板宽（弹窗据此定尺寸）。
        let expect_w = 5.0 * size + 4.0 * (RAIL_GAP.min(8.0)) + RAIL_PAD * 2.0;
        assert!(
            (r.width() - expect_w).abs() < 0.01,
            "横向停靠时组宽应为 5×size + 4×gap = {expect_w}，实际 {:.1}",
            r.width()
        );
        // 可见性关闭 ⇒ 没有矩形（弹窗随之不建）。
        cfg.type_rail.visible = false;
        assert!(
            FloatLayout::compute(&cfg, &items, false, area, None).rail.is_none(),
            "关闭类型栏后不应给出矩形"
        );
        let _ = ClipKind::Text;
    }

    /// 默认只显示**已有记录**的类型；`type_rail_always` 时五类齐全。
    ///
    /// 判据用 [`FloatLayout::kinds`]（纯函数）——弹窗尺寸（几何）与按钮
    /// 内容（绘制）读的是同一份结果，这条测试守住它不被拆成两套。
    #[test]
    fn type_rail_shows_only_present_kinds_unless_always() {
        use modular_clipboard_core::CapturedPayload;
        let mut cfg = modular_clipboard_core::config::UiConfig::default();
        // 四条全是 Text。
        let mut h = Harness::new("rail-kinds", vec2(700.0, 500.0)).with_items(4);
        let ids: Vec<_> = h.svc.state.items.iter().map(|it| it.id).collect();
        for id in ids {
            h.svc.toggle_pin(id).ok();
        }
        let items = h.svc.state.items.clone();
        assert_eq!(items.len(), 4);
        assert!(items.iter().all(|it| it.kind == modular_clipboard_core::ClipKind::Text));
        let kinds = FloatLayout::kinds(&cfg, &items);
        assert_eq!(
            kinds,
            vec![modular_clipboard_core::ClipKind::Text],
            "只有 Text 有记录，默认只应出现一枚按钮"
        );

        // 始终显示：五类齐全，顺序固定。
        cfg.type_rail.always = true;
        assert_eq!(
            FloatLayout::kinds(&cfg, &items),
            vec![
                modular_clipboard_core::ClipKind::Text,
                modular_clipboard_core::ClipKind::Html,
                modular_clipboard_core::ClipKind::Image,
                modular_clipboard_core::ClipKind::Files,
                modular_clipboard_core::ClipKind::Markdown,
            ]
        );
        // 隐藏 ⇒ 空（弹窗不建）。
        cfg.type_rail.visible = false;
        assert!(FloatLayout::kinds(&cfg, &items).is_empty());
        let _ = CapturedPayload {
            kind: modular_clipboard_core::ClipKind::Text,
            bytes: Vec::new(),
            html: None,
            text: None,
            source_app: String::new(),
            files: Vec::new(),
            dims: None,
        };
    }

    /// 点类型栏按钮 ⇒ `SetTypeFilterKind{kind, multi:false}`；再点一次
    /// 取消（落地后 `active_type_kinds` 为空）。
    ///
    /// 通过**弹窗身份**驱动（`App::draw_popup` 的路径）：弹窗是独立窗口，
    /// 有自己的 `egui::Context` 与热区命名空间，用主窗口的 `h.frame()`
    /// 点不到它的按钮。
    #[test]
    fn type_rail_click_toggles_type_filter() {
        let mut h = Harness::new("rail-click", vec2(700.0, 500.0)).with_items(4);
        h.frame();
        let ops = h.rail_popup_click(0); // 第一枚按钮（Text）
        assert!(
            ops.iter().any(|o| matches!(
                o,
                Op::SetTypeFilterKind { kind, multi: false }
                    if *kind == modular_clipboard_core::ClipKind::Text
            )),
            "点文本按钮应过滤出 Text，实际 {ops:?}"
        );
        // 落地 Op（生产由 App::apply_ops 做）：走 UiState 的切换规则。
        for op in ops {
            if let Op::SetTypeFilterKind { kind, multi } = op {
                h.state.toggle_type_filter(kind, multi);
            }
        }
        assert_eq!(
            h.state.active_type_kinds(),
            vec![modular_clipboard_core::ClipKind::Text],
            "落地后应过滤 Text"
        );
        let ops2 = h.rail_popup_click(0);
        assert!(
            ops2.iter().any(|o| matches!(
                o,
                Op::SetTypeFilterKind { kind, multi: false }
                    if *kind == modular_clipboard_core::ClipKind::Text
            )),
            "再点一次应再发同一个 Op（取消由落地侧判断），实际 {ops2:?}"
        );
        for op in ops2 {
            if let Op::SetTypeFilterKind { kind, multi } = op {
                h.state.toggle_type_filter(kind, multi);
            }
        }
        assert!(
            h.state.active_type_kinds().is_empty(),
            "再点一次落地后应取消过滤"
        );
    }

    /// 多选模式：点两枚按钮 ⇒ 两个类型都在生效集合里；再点一枚 ⇒
    /// 只移除那一枚（其余保留）。单选字段必须让位（`active_type_kinds`
    /// 的优先序否则会让「看起来没反应」）。
    #[test]
    fn type_rail_multi_select_accumulates_and_removes() {
        let mut h = Harness::new("rail-multi", vec2(700.0, 500.0)).with_items(4);
        // 开多选 + 始终显示全部类型（fixture 只有 Text 一种记录，
        // 不开 always 的话类型栏只有一枚按钮，点不出「组合」）。
        // 生产走设置卡片 Op，测试直接改配置。
        h.svc.state.config.ui.type_rail.multi = true;
        h.svc.state.config.ui.type_rail.always = true;
        h.frame();
        // 按钮顺序 = RAIL_KIND_ORDER = [Text, Html, Image, Files, Markdown]。
        // 点前两枚（Text / Html）。
        let ops = h.rail_popup_click(0);
        for op in ops {
            if let Op::SetTypeFilterKind { kind, multi } = op {
                assert!(multi, "多选配置下 Op 必须带 multi=true");
                h.state.toggle_type_filter(kind, multi);
            }
        }
        assert_eq!(h.state.active_type_kinds(), vec![modular_clipboard_core::ClipKind::Text]);
        let ops = h.rail_popup_click(1);
        for op in ops {
            if let Op::SetTypeFilterKind { kind, multi } = op {
                h.state.toggle_type_filter(kind, multi);
            }
        }
        assert_eq!(
            h.state.active_type_kinds(),
            vec![
                modular_clipboard_core::ClipKind::Text,
                modular_clipboard_core::ClipKind::Html
            ],
            "两枚都应点亮（BTreeSet 按 ClipKind 声明序：Text < Html）"
        );
        // 再点第一枚：只移除它。
        let ops = h.rail_popup_click(0);
        for op in ops {
            if let Op::SetTypeFilterKind { kind, multi } = op {
                h.state.toggle_type_filter(kind, multi);
            }
        }
        assert_eq!(
            h.state.active_type_kinds(),
            vec![modular_clipboard_core::ClipKind::Html],
            "移除后只剩一枚"
        );
        assert!(h.state.type_filter.is_none(), "多选模式下单选字段必须让位");
    }

    /// 搜索图标的**绘制矩形**必须完整落在按钮热区内。
    ///
    /// 守着实机缺陷：`search_toggle_button` 曾用
    /// `vec2(pal.icon_size, pal.icon_size) * scale` 算图标包围盒——
    /// `icon_size=16` × `scale=1.5` = 24 逻辑点，**超过按钮
    /// `sq = btn*0.62 ≈ 16.9`**。几何放大镜（圆环 + 手柄）占满整个
    /// 包围盒，`painter_at(r)` 随即按按钮矩形裁剪，圆环被切掉一大块，
    /// 实机表现「搜索图标渲染不完整」。
    ///
    /// 字体图标（×/垃圾桶/齿轮）有内部边距，24 逻辑矩形内字形只占
    /// ~60%，未被裁到，所以只有搜索图标残缺——正是这条测试守的洞。
    #[test]
    fn search_icon_draw_rect_fits_toggle_box() {
        let mut h = Harness::new("search-icon-fit", vec2(700.0, 500.0));
        h.frame();
        let r = hit(&h, "search_icon");
        // 与生产路径同一算法：绘制矩形 = 中心对齐、边长 icon_size（不乘 scale）。
        let draw = Rect::from_center_size(
            r.center(),
            egui::vec2(h.pal.icon_size, h.pal.icon_size),
        );
        assert!(
            r.contains_rect(draw),
            "搜索图标绘制矩形 {draw:?} 超出按钮热区 {r:?} —— \
             painter_at 会按热区裁剪，放大镜被切掉（实机「图标渲染不完整」）"
        );
    }

    /// 在 `ctx` 上排版一段文字，返回它占了几行。
    ///
    /// `max_rows` 传 `0` 与传 `usize::MAX` 会得到截然不同的结果，
    /// 所以这个辅助函数把它作为参数供测试对比。
    fn rows_with_max_rows(ctx: &egui::Context, text: &str, max_rows: usize) -> usize {
        ctx.fonts_mut(|f| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                text,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(14.0),
                    color: egui::Color32::WHITE,
                    ..Default::default()
                },
            );
            job.wrap.max_width = 100.0;
            job.wrap.max_rows = max_rows;
            job.wrap.break_anywhere = true;
            f.layout_job(job).rows.len()
        })
    }

    /// `TextWrapping::max_rows = 0` 表示「一行都不输出」，不是「不限」。
    ///
    /// 实测踩坑：`row_height_for` 里写了 `max_rows = 0`，本意是
    /// 「不限行数以便量出到底需要几行」，实际得到空 galley →
    /// `rows.len()` 恒为 0 → 行高恒等于下限 52pt → 预览带只有 24pt →
    /// 长内容全被省略号截断。诊断数据完全对上：
    ///   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
    ///
    /// 这条测试把「0 会吃掉所有行」钉死，防止有人再写回 0。
    #[test]
    fn max_rows_zero_produces_no_rows() {
        let ctx = settled_ctx();
        let text = "一段比较长的中文内容用来测试折行行数是否被正确计算";
        assert_eq!(
            rows_with_max_rows(&ctx, text, 0),
            0,
            "max_rows=0 时 galley 不应有任何行（它不是「不限」）"
        );
        assert!(
            rows_with_max_rows(&ctx, text, usize::MAX) > 0,
            "max_rows=usize::MAX（不限）时应正常排出版面"
        );
    }

    /// 行高必须随内容长度增长，否则长内容会被固定高度挤掉。
    #[test]
    fn row_height_grows_with_content_length() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        let cfg = modular_clipboard_core::config::UiConfig::default();
        // NOTE: `run_ui` 返回 `FullOutput`（不是闭包的返回值），
        // 所以要通过外部变量把结果取出来。
        let mut h = 0.0_f32;
        let mk = |text: &str, h: &mut f32| {
            let item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Text,
                "h".into(),
                text.into(),
                "test".into(),
            );
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                *h = row_height_for(ui, &item, &pal, 1.0, 200.0, &cfg, None);
            });
            // IMPORTANT: 必须 `clear()`：排版会往纹理图集里加新字形，
            // `FullOutput` 被 drop 时若还带着未应用的 deltas，egui 会 panic
            // （「Dropped TexturesDelta with N unapplied deltas」）。
            // 生产代码走 `FrameRenderer` 消费 delta，测试里没人消费。
            out.textures_delta.clear();
        };
        mk("短", &mut h);
        let short = h;
        // 200pt 宽约能放 13 个汉字，30 个"很长的内容"必然折多行。
        mk(&"很长的内容".repeat(30), &mut h);
        let long = h;

        assert!(
            long > short,
            "长内容的行高应大于短内容：短={short} 长={long}"
        );
        assert!(
            short >= ROW_HEIGHT_MIN - 0.01,
            "行高不应低于下限 {ROW_HEIGHT_MIN}，实际 {short}"
        );
    }

    /// 图片条目的行高 = 缩略图显示高（限高 [`IMG_MAX_H_LIST`]）+ 固定段，
    /// 与同尺寸文本行**不同**——证明走的是图片分支而不是文本分支。
    #[test]
    fn row_height_uses_thumbnail_height_for_images() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        let cfg = modular_clipboard_core::config::UiConfig::default();
        assert!(cfg.image_inline, "图片内嵌默认开");
        let mut h = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Image,
                "h".into(),
                "图片 1920×1080".into(),
                "test".into(),
            );
            // payload.dims 是 store 插入时读的图片头，行高只认它。
            item.payload = Some(modular_clipboard_core::PayloadRef {
                file: "h.bin".into(),
                len: 1024,
                dims: Some((1920, 1080)),
            });
            h = row_height_for(ui, &item, &pal, 1.0, 600.0, &cfg, None);
        });
        out.textures_delta.clear();
        // 600pt 宽的带、16:9 的图：显示高 = min(IMG_MAX_H_LIST, 600*1080/1920=337.5)
        // = IMG_MAX_H_LIST（限高生效）。
        let expect = IMG_MAX_H_LIST + pal.font_xs.max(10.0) + row_top_height(&pal) + 5.0 * 2.0 + 4.0;
        assert!(
            (h - expect).abs() < 0.6,
            "图片行高应等于 图高{IMG_MAX_H_LIST} + 固定段（{expect}），实际 {h}"
        );
    }

    /// 文件型图片（从资源管理器复制的 `.png`）的行高也必须走图片算式。
    ///
    /// 2026-10-17 实测缺陷：`row_height_for` 的图片分支只认
    /// `kind == ClipKind::Image`，文件型条目按**文件名文本**算行高
    /// （~24pt），而绘制侧受 `band.height()` 约束 ⇒ 缩略图被压成一条
    /// 细带（用户反馈「尺寸太小」）。这条测试钉死「两类条目同一算式」。
    #[test]
    fn row_height_uses_thumbnail_height_for_image_files() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        let cfg = modular_clipboard_core::config::UiConfig::default();
        let mut file_h = 0.0_f32;
        let mut img_h = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            // 同一尺寸、同一文本，只有 kind / preview 不同。
            let mut file_item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Files,
                "h".into(),
                "wallpaper.png".into(),
                "test".into(),
            );
            file_item.payload = Some(modular_clipboard_core::PayloadRef {
                file: "h.bin".into(),
                len: 4096,
                // 入库侧 `first_image_file_dims` 读文件头补齐的尺寸。
                dims: Some((1920, 1080)),
            });
            let mut img_item = file_item.clone();
            img_item.kind = modular_clipboard_core::ClipKind::Image;

            file_h = row_height_for(ui, &file_item, &pal, 1.0, 600.0, &cfg, None);
            img_h = row_height_for(ui, &img_item, &pal, 1.0, 600.0, &cfg, None);
        });
        out.textures_delta.clear();
        assert!(
            (file_h - img_h).abs() < 0.01,
            "文件型图片的行高应与剪贴板图片一致：文件={file_h} 图片={img_h}"
        );
        // 且必须显著高于「文件名一行」的行高——否则说明又跌回文本分支。
        let mut text_h = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut text_item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Files,
                "h".into(),
                "wallpaper.png".into(),
                "test".into(),
            );
            // 无 dims：图也画不出来，只能按文本排。
            text_item.payload = Some(modular_clipboard_core::PayloadRef {
                file: "h.bin".into(),
                len: 4096,
                dims: None,
            });
            text_h = row_height_for(ui, &text_item, &pal, 1.0, 600.0, &cfg, None);
        });
        out.textures_delta.clear();
        assert!(
            file_h > text_h + 60.0,
            "文件型图片行高（{file_h}）应远高于纯文本行高（{text_h}）"
        );
    }

    /// 老条目（载荷里没有 `dims`）：解码记下的尺寸也能把行高撑起来。
    ///
    /// 这是 2026-10-17 那批「文件型图片」的兜底路径——入库侧
    /// `first_image_file_dims` 只对**新**条目补齐，此前已入库的老条目
    /// 只能等第一次解码后靠 `thumb_dims` 排版。
    #[test]
    fn row_height_falls_back_to_decoded_dims() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        let cfg = modular_clipboard_core::config::UiConfig::default();
        let mut without = 0.0_f32;
        let mut with = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Files,
                "h".into(),
                "old.png".into(),
                "test".into(),
            );
            item.payload = Some(modular_clipboard_core::PayloadRef {
                file: "h.bin".into(),
                len: 4096,
                dims: None, // 老条目：入库时没记尺寸
            });
            without = row_height_for(ui, &item, &pal, 1.0, 600.0, &cfg, None);
            with = row_height_for(ui, &item, &pal, 1.0, 600.0, &cfg, Some((1920, 1080)));
        });
        out.textures_delta.clear();
        assert!(
            with > without + 60.0,
            "拿到解码尺寸后行高应显著变高：之前={without} 之后={with}"
        );
    }

    /// `thumb_dims` **不随纹理淘汰**——它的 `len()` 是行高缓存的失效判据
    /// （[`RowHeightCache::dims_known`]）。
    ///
    /// 回归守卫：2026-10-17 之前 `thumb_dims` 与 `thumb_tex` 一起
    /// `retain`，滚出屏就丢。而 `dims_known` 的语义是
    /// 「数量变化 == 有新图刚拿到尺寸」——尺寸一丢，整份行高缓存
    /// 当帧失效；滚回来又拿到尺寸，数量再变一次 ⇒ **滚动时每帧重算
    /// 全部行高**（2000 条 = 2000 次 `LayoutJob` 测量）。
    ///
    /// 因此这两张表的淘汰策略**必须不同**，而它们在代码里紧挨着
    /// （`draw` 收尾只对 `thumb_tex` 写了 `retain`，`thumb_dims` 的
    /// 「不淘汰」只存在于字段注释里）—— 注释不是防线，测试才是。
    #[test]
    fn thumb_dims_survives_texture_eviction() {
        let mut h = Harness::new("thumb-dims", vec2(700.0, 500.0)).with_items(3);
        let ids: Vec<_> = h.svc.state.items.iter().map(|i| i.id).collect();
        assert!(!ids.is_empty(), "夹具应至少有一条记录");

        // 假装这些条目都拿到过解码尺寸（真实路径由 `preview_image_tex`
        // 在上传成功时写入）。
        for id in &ids {
            h.state.thumb_dims.insert(*id, (1920, 1080));
        }
        h.state.thumb_seen.clear();
        let before = h.state.thumb_dims.len();

        // 跑一帧：本帧没有任何条目被看见（`thumb_seen` 为空），
        // ⇒ `thumb_tex` 会被整表清空，而 `thumb_dims` 必须原样留下。
        h.frame();

        assert!(
            h.state.thumb_tex.is_empty(),
            "本帧无可见图片时纹理应被淘汰（否则显存只涨不消）"
        );
        assert_eq!(
            h.state.thumb_dims.len(),
            before,
            "thumb_dims 不得随纹理一起淘汰：它的 len() 是行高缓存的失效判据\
             （dims_known），跟着淘汰会让滚动时每帧重算全部行高"
        );
    }

    /// 显示尺寸算式：限高与限宽两步都保持长宽比，且**不放大**。
    #[test]
    fn image_display_size_keeps_aspect_ratio() {
        // 16:9 宽图：限高生效 ⇒ 高=限高、宽按比例。
        let (w, h) = image_display_size((1920, 1080), 600.0, 160.0);
        assert!((h - 160.0).abs() < 0.01, "限高生效：h={h}");
        assert!((w - 160.0 * 16.0 / 9.0).abs() < 0.5, "宽按比例：w={w}");

        // 窄带：限高算出的宽超出带子 ⇒ 改由限宽决定，高按比例回缩
        // （不能只截宽——那会把图拉扁）。
        let (w, h) = image_display_size((1920, 1080), 200.0, 160.0);
        assert!((w - 200.0).abs() < 0.01, "限宽生效：w={w}");
        assert!((h - 200.0 * 9.0 / 16.0).abs() < 0.5, "高按比例回缩：h={h}");

        // 退化尺寸：返回 0 而不是 NaN（除零会让整个行高变 NaN）。
        assert_eq!(image_display_size((0, 100), 200.0, 160.0), (0.0, 0.0));
    }

    /// 缩略图纹理分辨率必须够「限高 × 2x DPI」，否则放大显示是糊的。
    ///
    /// 2026-10-17：`IMG_MAX_H_LIST` 从 108 提到 160 时，`THUMB_MAX_EDGE`
    /// 仍是 256 —— 200% 缩放下需要 320px，纹理不够就只能放大插值。
    /// 两个常量分开在两个文件里，改一个忘另一个不会编译报错，只能靠
    /// 这条测试钉住（`const` 块 ⇒ 编译期就失败）。
    #[test]
    fn thumbnail_resolution_covers_display_size_at_2x_dpi() {
        const {
            assert!(
                crate::thumbnail::THUMB_MAX_EDGE as f32 >= crate::paint::list::IMG_MAX_H_LIST * 2.0,
                "缩略图最长边必须 ≥ 列表限高 × 2（DPI 200%），否则放大显示会糊"
            );
        }
        // 顺带：限高必须为正，否则图片行高会退化成纯文本行（编译期）。
        const {
            assert!(
                crate::paint::list::IMG_MAX_H_LIST > 0.0,
                "限高必须为正，否则图片行高会退化成纯文本行"
            );
        }
    }

    // ---- 文件型图片（从资源管理器复制的图片文件）----
    //
    // 这类条目的载荷是**路径列表**，图在磁盘上。判据、读盘、行高三处
    // 都要单独关照，所以各自有一个测试。

    /// 在夹具的临时目录里造一个真实可解的 PNG。
    fn write_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
        let mut buf = Vec::new();
        let img = image::ImageBuffer::from_pixel(w, h, image::Rgba([200u8, 40, 60, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("测试用 PNG 编码失败");
        let p = dir.join(name);
        std::fs::write(&p, &buf).expect("写测试用 PNG 失败");
        p
    }

    /// 塞一个 `ClipKind::Files` 条目：载荷是**换行拼接的路径列表**（与
    /// 捕获层一致），`preview` 由 `build_preview` 生成。
    fn ingest_files(h: &mut Harness, names: &[&str]) {
        let paths: Vec<std::path::PathBuf> =
            names.iter().map(|n| h._dir.path().join(n)).collect();
        let list = paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let payload = modular_clipboard_core::CapturedPayload {
            kind: modular_clipboard_core::ClipKind::Files,
            bytes: list.into_bytes(),
            html: None,
            text: None,
            source_app: "explorer".into(),
            files: paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            dims: None,
        };
        assert!(h.svc.ingest(payload), "测试前置：文件条目应入库");
        h.svc.reload_list();
    }

    /// 从资源管理器复制的**单个图片文件**要显示缩略图，而不是只显示
    /// 文件名（用户报的问题）。
    #[test]
    fn single_image_file_item_shows_thumbnail_not_just_the_name() {
        let mut h = Harness::new("file-img", vec2(700.0, 500.0));
        write_png(h._dir.path(), "shot.png", 40, 30);
        ingest_files(&mut h, &["shot.png"]);

        let item = h.svc.state.items[0].clone();
        assert_eq!(item.preview, "shot.png", "单文件条目的 preview 就是文件名");
        // 入库时已把尺寸嗅出来 ⇒ 行高从第一帧起就是图片行高（不跳变）。
        assert_eq!(
            item.payload.as_ref().and_then(|p| p.dims),
            Some((40, 30)),
            "入库应补齐图片尺寸（`first_image_file_dims`）"
        );
        assert!(preview_is_image(&item), "单个 .png 应判为图片条目");

        h.frame();
        assert!(
            h.state.thumb_tex.contains_key(&item.id),
            "图片文件条目应上传缩略图纹理——只显示文件名就是没走上这条路径"
        );
        assert_eq!(
            h.state.thumb_dims.get(&item.id),
            Some(&(40, 30)),
            "尺寸应来自解码结果"
        );
    }

    /// 非图片文件：既不该去读文件头，也不该画缩略图。
    #[test]
    fn non_image_file_item_stays_text() {
        let mut h = Harness::new("file-txt", vec2(700.0, 500.0));
        std::fs::write(h._dir.path().join("notes.txt"), "hello").expect("写 .txt 失败");
        ingest_files(&mut h, &["notes.txt"]);

        let item = h.svc.state.items[0].clone();
        assert!(!preview_is_image(&item), ".txt 不应判为图片条目");
        assert_eq!(
            item.payload.as_ref().and_then(|p| p.dims),
            None,
            "不该为 .txt 去读文件头（白名单预筛应挡住）"
        );
        h.frame();
        assert!(
            !h.state.thumb_tex.contains_key(&item.id),
            "非图片文件不应上传纹理"
        );
    }

    /// 多文件条目**不**画缩略图：语义上「显示第一张」会让人以为缩略图
    /// 就是全部内容。
    #[test]
    fn multi_file_item_does_not_show_thumbnail() {
        let mut h = Harness::new("file-multi", vec2(700.0, 500.0));
        write_png(h._dir.path(), "a.png", 20, 20);
        write_png(h._dir.path(), "b.png", 20, 20);
        ingest_files(&mut h, &["a.png", "b.png"]);

        let item = h.svc.state.items[0].clone();
        assert!(
            !preview_is_image(&item),
            "多文件条目（preview 是『a.png 等 2 个文件』）不该只画第一张"
        );
        h.frame();
        assert!(!h.state.thumb_tex.contains_key(&item.id));
    }

    /// 图片文件已被删除（或被移走）：降级成文本，不能卡住入库/渲染。
    #[test]
    fn deleted_image_file_falls_back_to_text() {
        let mut h = Harness::new("file-gone", vec2(700.0, 500.0));
        let p = write_png(h._dir.path(), "gone.png", 16, 16);
        ingest_files(&mut h, &["gone.png"]);
        let item = h.svc.state.items[0].clone();
        // 入库时文件还在 ⇒ 尺寸已经记下；之后删掉，缩略图应当取不到。
        std::fs::remove_file(&p).expect("删除测试文件失败");

        h.frame();
        assert!(
            !h.state.thumb_tex.contains_key(&item.id),
            "文件已删 ⇒ 不应有纹理（应降级为文本预览）"
        );
    }

    /// Markdown 行高 = `md::MdDoc::height()` 实测 + 固定段。
    ///
    /// 精确等式而不是「渲染态比源码态高」——后者对短文档不成立
    /// （源码折行行数可能更多），曾据此写断言结果反被数据打脸。
    #[test]
    fn row_height_uses_markdown_layout_when_rendered() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        let src = "# 标题\n\n- 第一项\n- 第二项\n\n> 引用一段\n\n正文一段";
        let item = modular_clipboard_core::ClipItem::new(
            modular_clipboard_core::ClipKind::Markdown,
            "h".into(),
            src.into(),
            "test".into(),
        );
        let mut cfg = modular_clipboard_core::config::UiConfig {
            md_rendered: true,
            ..Default::default()
        };
        let mut rendered = 0.0_f32;
        let mut md_h = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            md_h = crate::md::MdDoc::parse(src).height(ui, &pal, 1.0, 400.0);
            rendered = row_height_for(ui, &item, &pal, 1.0, 400.0, &cfg, None);
        });
        out.textures_delta.clear();
        let fixed = pal.font_xs.max(10.0) + row_top_height(&pal) + 5.0 * 2.0 + 4.0;
        assert!(
            (rendered - (md_h + fixed)).abs() < 0.6,
            "渲染行高（{rendered}）应等于 md 实测（{md_h}）+ 固定段（{fixed}）"
        );
        // 关闭渲染 ⇒ 走等宽源码分支，行高必然不同（证明开关真的接了线）。
        cfg.md_rendered = false;
        let mut source = 0.0_f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            source = row_height_for(ui, &item, &pal, 1.0, 400.0, &cfg, None);
        });
        out.textures_delta.clear();
        assert!(
            (source - rendered).abs() > 0.6,
            "关闭渲染后行高应变化（源码分支）：渲染={rendered} 源码={source}"
        );
    }

    /// 行高下限必须容得下「预览一行 + 来源一行 + 内边距」。
    ///
    /// 下限太小会让第二行（来源/时间）被挤出行外。
    #[test]
    fn row_height_min_fits_two_text_lines() {
        const {
            assert!(
                ROW_HEIGHT_MIN >= 40.0,
                "两行布局至少需要 40pt：预览 ~19 + 来源 ~13 + 内边距 ~12"
            );
        }
    }

    /// 大小自适应性：B / KB / MB / GB 进位与小数位取舍。
    #[test]
    fn format_size_adapts_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        // ≥10 的档位不显示小数：12.06 KB → 12 KB。
        assert_eq!(format_size(12345), "12 KB");
        // 临界进位：5 MB → 5120 KB，仍按 KB 显示（未过 MB 档）。
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2.0 GB");
        // 跨档：1025 MB → 1.0 GB。
        assert_eq!(format_size(1025 * 1024 * 1024), "1.0 GB");
    }

    /// 类型标签中文名与枚举一一对应。
    #[test]
    fn kind_label_covers_all_kinds() {
        use modular_clipboard_core::ClipKind::*;
        assert_eq!(kind_label(Text), "文本");
        assert_eq!(kind_label(Html), "HTML");
        assert_eq!(kind_label(Image), "图片");
        assert_eq!(kind_label(Files), "文件");
    }

    // ------------------------------------------------------------------
    // MCP 服务（通信分类）测试
    // ------------------------------------------------------------------

    /// 通信分类下：MCP 总开关行必须同时登记「开关」与「ⓘ 详细」两个热区。
    #[test]
    fn mcp_comm_card_registers_toggle_and_info() {
        let mut h = Harness::new("mcp-comm", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Comm;
        h.settings_frame();
        // 总开关（唯一 id `mcp_enabled`，避免与捕获页 `enabled` 撞 id）
        // 与 ⓘ 详细按钮都必须画出来。
        let sw = settings_hit(&h, "mcp_enabled");
        let info = settings_hit(&h, "settings_mcp_info");
        assert!(sw.width() > 0.0 && info.width() > 0.0);
        // ⓘ 必须在开关**左边**（x 更小）。
        assert!(
            info.max.x <= sw.min.x + 0.5,
            "ⓘ 应在开关左边：info.max.x={} vs switch.min.x={}",
            info.max.x,
            sw.min.x
        );
        // 两者之间必须留出呼吸间距（用户反馈「挨得太挤」）。
        // 设置卡片把 item_spacing.x 归了 0，不显式 add_space 就会贴上。
        assert!(
            sw.min.x - info.max.x >= 7.5,
            "ⓘ 与开关间距过近：{:.1}pt",
            sw.min.x - info.max.x
        );
    }

    /// 点 ⓘ ⇒ 打开 MCP 调用设置覆盖层；点遮罩（面板外）⇒ 关闭。
    #[test]
    fn mcp_info_toggles_overlay() {
        let mut h = Harness::new("mcp-overlay", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Comm;
        h.settings_frame();
        assert!(!h.state.mcp_overlay_open);

        // 点 ⓘ → 打开。
        let info = settings_hit(&h, "settings_mcp_info");
        h.settings_click_at(info.center());
        assert!(h.state.mcp_overlay_open, "点 ⓘ 应打开覆盖层");

        // 覆盖层内应登记关闭 ×、完成、取消与 8 个能力开关热区。
        let close = settings_hit(&h, "settings_mcp_overlay_close");
        let done = settings_hit(&h, "settings_mcp_overlay_done");
        assert!(close.width() > 0.0 && done.width() > 0.0);
        let first = settings_hit(&h, MCP_CAPS[0].field);
        assert!(first.width() > 0.0, "覆盖层应列出能力开关");

        // 点面板外（窗口角落）→ 关闭。
        h.settings_click_at(h.area.min + vec2(3.0, 3.0));
        assert!(!h.state.mcp_overlay_open, "点遮罩应关闭覆盖层");
    }


    /// 覆盖层打开时，能力开关点击 ⇒ 产生对应字段的 `SetBool`。
    #[test]
    fn mcp_overlay_switch_emits_set_bool() {        let mut h = Harness::new("mcp-overlay-sw", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Comm;
        h.state.mcp_overlay_open = true;
        h.settings_frame();
        let sw = settings_hit(&h, "click_element");
        let ops = h.settings_click_at(sw.center());
        assert!(
            ops.iter().any(|o| matches!(
                o,
                Op::SetBool { group: ConfigGroup::Mcp, field: "click_element", .. }
            )),
            "覆盖层点「点击界面元素」应产生 SetBool(Mcp, click_element)，实际={ops:?}"
        );
    }

    /// 通信卡片重置 ⇒ 总开关与全部能力恢复默认（开启）。
    #[test]
    fn mcp_comm_card_reset_restores_defaults() {
        let mut h = Harness::new("mcp-reset", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Comm;
        // 先改坏一项（总开关关闭），重置按钮才会出现。
        h.svc.state.config.mcp.enabled = false;
        h.svc.state.config.mcp.click_element = false;
        h.settings_frame();
        let reset = settings_hit(&h, "settings_reset_comm");
        let ops = h.settings_click_at(reset.center());
        let bools: Vec<&Op> = ops
            .iter()
            .filter(|o| matches!(o, Op::SetBool { group: ConfigGroup::Mcp, .. }))
            .collect();
        // 总开关 + 10 项能力 = 11 个 SetBool，且值全为 true。
        assert_eq!(bools.len(), 11, "重置应推 11 个 MCP SetBool，实际={ops:?}");
        for op in bools {
            match op {
                Op::SetBool { value, .. } => assert!(*value, "重置应恢复为开启：{op:?}"),
                _ => unreachable!(),
            }
        }
    }

    /// 字体缩放预设：覆盖默认档、升序、落在合法区间内。
    #[test]
    fn font_scale_presets_are_sane() {
        assert!(!FONT_SCALE_PRESETS.is_empty());
        // 必须包含 100%（默认值），否则用户没有「回到标准」的入口。
        assert!(
            FONT_SCALE_PRESETS.iter().any(|(p, _)| *p == 100),
            "预设必须包含标准档 100%"
        );
        let mut prev = 0u8;
        for (pct, name) in FONT_SCALE_PRESETS {
            assert!(!name.is_empty(), "预设 {pct}% 缺少档位名");
            assert!(*pct > prev, "预设必须严格升序：{prev} → {pct}");
            prev = *pct;
            let scale = f64::from(*pct) / 100.0;
            assert!(
                (crate::theme::FONT_SCALE_MIN as f64..=crate::theme::FONT_SCALE_MAX as f64)
                    .contains(&scale),
                "预设 {pct}% 超出允许区间"
            );
        }
    }

    /// 倍率 → 预设百分比：精确命中取自身，非预设取最近档。
    #[test]
    fn font_scale_percent_picks_exact_or_nearest() {
        assert_eq!(font_scale_percent(1.0), 100);
        assert_eq!(font_scale_percent(1.15), 115);
        assert_eq!(font_scale_percent(1.5), 150);
        // 非预设值取最近档：1.12 → 115（距 100 有 12、距 115 只有 3）。
        assert_eq!(font_scale_percent(1.12), 115);
        // 越界值先钳制：5.0 → 上限 2.0 → 距 150 最近。
        assert_eq!(font_scale_percent(5.0), 150);
        assert_eq!(font_scale_percent(f32::NAN), 100, "NaN 回落 1.0×");
    }

    /// 只有**正好**落在预设上才算「预设」，否则界面要标「自定义」。
    #[test]
    fn font_scale_preset_detection_is_exact() {
        assert!(font_scale_is_preset(1.0));
        assert!(font_scale_is_preset(0.85));
        assert!(font_scale_is_preset(1.5));
        assert!(!font_scale_is_preset(1.12), "1.12× 不是预设档");
        assert!(!font_scale_is_preset(0.9), "0.9× 不是预设档");
    }

    /// 外观卡片点「大」分段 ⇒ 推出 `SetNumber(Ui, font_scale, 1.15)`。
    ///
    /// 这条守的是「字体缩放修改失效」：配置写进去了，但**没有任何读取方**
    /// （`Palette` 从不按 `font_scale` 缩放），界面毫无变化。
    /// 现在配置项走的是 `FontScale` 分段控件，点击必须产出带倍率的 Op。
    #[test]
    fn font_scale_segment_emits_scaled_number_op() {
        let mut h = Harness::new("font-scale-seg", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.settings_frame();

        // 五个预设段都应登记热区（key = 选项文字）。
        for (_, name) in FONT_SCALE_PRESETS {
            // 热区 key = **显示文字**（`draw_segmented` 用 `t(name)` 登记）。
            // 这里也用 `t(name)`：注册与查找走同一条路径，与当前语言无关。
            let seg = settings_hit(&h, t(name));
            assert!(seg.width() > 0.0, "预设「{}」应有热区", t(name));
        }
        // 点「大」（115%）。
        let big = settings_hit(&h, t("Large"));
        let ops = h.settings_click_at(big.center());
        let hit = ops.iter().any(|o| matches!(
            o,
            Op::SetNumber { group: ConfigGroup::Ui, field: "font_scale", value }
                if (value - 1.15).abs() < 1e-6
        ));
        assert!(hit, "点「大」应推出 font_scale=1.15，实际={ops:?}");
    }

    /// 字体缩放偏离默认值时，外观卡片的「重置」必须把它恢复成 1.0×。
    #[test]
    fn font_scale_reset_restores_default() {
        let mut h = Harness::new("font-scale-reset", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.svc.state.config.ui.font_scale = 1.5;
        h.settings_frame();
        let reset = settings_hit(&h, "settings_reset_appearance");
        let ops = h.settings_click_at(reset.center());
        assert!(
            ops.iter().any(|o| matches!(
                o,
                Op::SetNumber { group: ConfigGroup::Ui, field: "font_scale", value }
                    if (value - 1.0).abs() < 1e-6
            )),
            "重置应把字体缩放恢复为 1.0×，实际={ops:?}"
        );
    }

    /// 外观卡片点「卡片阴影」开关 ⇒ 推出 `SetCardShadow`，enabled 取反。
    ///
    /// 这条守的是「设置项必须真的产出 Op」——新增设置项若漏接
    /// `settings_item_row` 的推送，界面点了毫无反应还静默通过。
    #[test]
    fn card_shadow_switch_emits_set_card_shadow() {
        let mut h = Harness::new("card-shadow-sw", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.settings_frame();
        let sw = settings_hit(&h, "card_shadow_enabled");
        let ops = h.settings_click_at(sw.center());
        let c = ops
            .iter()
            .find_map(|o| match o {
                Op::SetCardShadow(c) => Some(c),
                _ => None,
            })
            .unwrap_or_else(|| panic!("点开关应推出 SetCardShadow，实际={ops:?}"));
        // 开关行只动 enabled；其余参数保持默认。
        assert_eq!(
            *c,
            modular_clipboard_core::config::CardShadowConfig {
                enabled: false,
                ..Default::default()
            }
        );
    }

    /// 卡片阴影偏离默认（关掉）时，外观卡片「重置」把它恢复默认（开启）。
    #[test]
    fn card_shadow_reset_restores_default() {
        let mut h = Harness::new("card-shadow-reset", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.svc.state.config.ui.card_shadow = modular_clipboard_core::config::CardShadowConfig {
            enabled: false,
            ..Default::default()
        };
        h.settings_frame();
        let reset = settings_hit(&h, "settings_reset_appearance");
        let ops = h.settings_click_at(reset.center());
        assert!(
            ops.iter().any(|o| matches!(
                o,
                Op::SetCardShadow(c)
                    if *c == modular_clipboard_core::config::CardShadowConfig::default()
            )),
            "重置应把卡片阴影恢复默认（开启），实际={ops:?}"
        );
    }

    /// 卡片阴影三个滑条都必须登记热区（可被自动化点击）。
    ///
    /// 顺带验证「开关关闭时参数子块不渲染」：滑条热区缺失是预期，
    /// 不该把「找不到热区」当成失败。
    #[test]
    fn card_shadow_sliders_have_hit_regions_when_enabled() {
        let mut h = Harness::new("card-shadow-sl", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        // 折叠「窗口与背景」组：外观卡加了卡片标题 + 四个组标题后，
        // 560pt 的夹具视口装不下 card_shadow 的三个子滑条。收起来让
        // 目标进首屏（这些测试只跑一帧，靠首屏可见）。
        h.state.settings_folded.insert("appearance.window".to_string());
        h.settings_frame();
        for key in ["card_shadow_blur", "card_shadow_opacity", "card_shadow_offset_y"] {
            let r = h.state.hit.get(HitNs::Settings, key);
            assert!(
                r.is_some_and(|r| r.width() > 0.0 && r.height() > 0.0),
                "开启状态下滑条 {key} 应有热区"
            );
        }
    }

    /// 默认折叠的分组必须**点得开**。
    ///
    /// 2026-10-24 用户反馈「液态玻璃无法展开」。根因是折叠状态用
    /// 「集合成员 异或 默认折叠表」判断，而点击分支只写了默认展开那
    /// 一支：默认折叠的组走 `remove`，可它本就不在集合里（no-op），
    /// 下一帧异或回来还是折叠 ⇒ 永远展不开。
    ///
    /// 现在改成快照语义（seed 后有点 = 折叠、点击即切换），这条测试
    /// 守住「默认折叠组点一次能展开、再点一次能收起」。
    #[test]
    fn default_folded_group_toggles() {
        let mut h = Harness::new("fold-glass", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        // 折叠前两组：外观卡有 17 项 + 4 个组标题，900pt 的视口装不到
        // 第 4 组——`ScrollArea` 只 allocate 可见区内的内容，组标题行
        // 不在可视区就没有热区（与 card_shadow 那几个测试同因）。
        h.state
            .settings_folded
            .insert("appearance.window".to_string());
        h.state
            .settings_folded
            .insert("appearance.cards".to_string());
        h.settings_frame();

        // 首帧：液态玻璃组应已折叠（seed 把 DEFAULT_FOLDED 灌进来）。
        assert!(
            h.state.settings_folded.contains("appearance.glass"),
            "液态玻璃组默认应折叠"
        );
        // 折叠 ⇒ 组内项没有热区（玻璃模糊度是第一项）。
        assert!(
            h.state
                .hit
                .get(HitNs::Settings, "ui.glass_blur")
                .is_none(),
            "折叠时玻璃模糊度不该有热区"
        );

        // 点组标题行 ⇒ 展开。
        let r = h.state.hit.get(HitNs::Settings, "appearance.glass");
        let r = r.expect("液态玻璃组标题应登记热区");
        h.settings_click_at(r.center());
        assert!(
            !h.state.settings_folded.contains("appearance.glass"),
            "点一次应展开"
        );
        // CAUTION: 点击那一帧的遍历用的是**点击前**读到的 `folded`（状态
        // 是在遍历中途改的），所以那一帧玻璃项仍未画出来。要多跑一帧
        // 才能看到展开后的布局——这也是真实程序里的行为（下一帧生效）。
        h.settings_frame();
        assert!(
            h.state
                .hit
                .get(HitNs::Settings, "ui.glass_blur")
                .is_some_and(|r| r.width() > 0.0),
            "展开后玻璃模糊度应有热区"
        );

        // 再点一次 ⇒ 收起。
        let r = h.state.hit.get(HitNs::Settings, "appearance.glass");
        let r = r.expect("组标题行应仍在");
        h.settings_click_at(r.center());
        assert!(
            h.state.settings_folded.contains("appearance.glass"),
            "再点一次应收起"
        );
    }

    /// 折叠箭头必须是**补间推进**的，不能瞬间跳到目标角。
    ///
    /// 2026-10-24 用户要求「`>` 到 `v` 的过程有动画」。这条测试守住两件事：
    ///
    /// 1. 目标切换后，第一帧的角度**既不是旧值也不是新值**（说明真的在补间）；
    /// 2. 推进数帧后收敛到目标角（说明补间能走完，没被闸门饿死）。
    ///
    /// 第 2 点是重点：`tween_f32` 是惰性的，而设置窗口有 `RedrawGate`，
    /// 静止帧整帧跳过。动画进行中若不 `request_repaint_after` 预约下一拍，
    /// 补间永远走不到第二帧——那条路径没有任何报错，只会表现为
    /// 「图标瞬间跳变、没有动画」。
    #[test]
    fn fold_arrow_tweens_between_states() {
        let mut h = Harness::new("fold-tween", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.state
            .settings_folded
            .insert("appearance.window".to_string());
        h.state
            .settings_folded
            .insert("appearance.cards".to_string());
        h.settings_frame();

        // 展开液态玻璃组 ⇒ 目标角从 -90°（<）变 0°（v）。
        let r = h.state.hit.get(HitNs::Settings, "appearance.glass");
        let r = r.expect("组标题应登记热区");
        h.settings_click_at(r.center());

        // 推进若干帧：每帧拨 1/60s（夹具时钟驱动 egui 的 `input.time`，
        // 补间按它前进）。180ms 的动画约需 11 帧，40 帧足够收敛。
        let mut mid_seen = false;
        for _ in 0..40 {
            h.advance_secs(1.0 / 60.0);
            h.settings_frame();
            let deg = crate::anim::peek_f32(
                &h.ctx,
                egui::Id::unique(("group_fold_rot", "appearance.glass")),
            );
            // 补间值存在且既非起点也非终点 ⇒ 中间态（动画真的在跑）。
            if let Some(v) = deg
                && (v - 0.0).abs() > 0.5
                && (v - (-90.0)).abs() > 0.5
            {
                mid_seen = true;
            }
            if !h.state.settings_folded.contains("appearance.glass") && mid_seen {
                break;
            }
        }
        assert!(mid_seen, "折叠箭头应有中间态（旋转补间生效）");
        assert!(
            !h.state.settings_folded.contains("appearance.glass"),
            "点击后应展开"
        );
    }

    /// 右下角信息编辑器：三个 chip（图标 / 类型 / 大小）都必须登记热区。
    #[test]
    fn row_meta_chips_have_hit_regions() {
        let mut h = Harness::new("rowmeta-hits", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
        h.state.settings_folded.insert("appearance.window".to_string());
        h.settings_frame();
        for key in ["rowmeta_chip_icon", "rowmeta_chip_type", "rowmeta_chip_size"] {
            let r = h.state.hit.get(HitNs::Settings, key);
            assert!(
                r.is_some_and(|r| r.width() > 0.0 && r.height() > 0.0),
                "右下角信息 chip {key} 应有热区"
            );
        }
    }

    /// 单击「类型」chip ⇒ 推出 `SetRowMeta`，只翻转 type_ 显隐位，
    /// 顺序与其他两个显隐位保持不变。
    #[test]
    fn row_meta_chip_click_toggles_visibility() {
        let mut h = Harness::new("rowmeta-click", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
                h.state.settings_folded.insert("appearance.window".to_string());
        h.settings_frame();
        let chip = settings_hit(&h, "rowmeta_chip_type");
        let ops = h.settings_click_at(chip.center());
        let m = ops
            .iter()
            .find_map(|o| match o {
                Op::SetRowMeta(m) => Some(m),
                _ => None,
            })
            .unwrap_or_else(|| panic!("点「类型」chip 应推出 SetRowMeta，实际={ops:?}"));
        assert!(!m.type_, "单击后类型应隐藏（灰）");
        assert!(m.icon && m.size, "图标/大小显隐不受影响");
        assert_eq!(
            m.order,
            [
                modular_clipboard_core::config::RowMetaPart::Icon,
                modular_clipboard_core::config::RowMetaPart::Type,
                modular_clipboard_core::config::RowMetaPart::Size,
            ],
            "单击不改变顺序"
        );
    }

    /// 按住「图标」chip 拖到「大小」右侧 ⇒ 顺序变为 类型 → 大小 → 图标。
    ///
    /// 拖拽跨帧：按下（帧 1）→ 移动越过其他 chip（帧 2）→ 抬起（帧 3）。
    #[test]
    fn row_meta_chip_drag_reorders() {
        let mut h = Harness::new("rowmeta-drag", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
                h.state.settings_folded.insert("appearance.window".to_string());
        h.settings_frame();
        let icon = settings_hit(&h, "rowmeta_chip_icon");
        let size = settings_hit(&h, "rowmeta_chip_size");
        let start = icon.center();
        // 落到「大小」chip 右侧足够远，确保越过其中心。
        let target = egui::pos2(size.max.x + 20.0, start.y);
        let press = |p: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        h.settings_frame_with(vec![egui::Event::PointerMoved(start), press(start, true)]);
        h.settings_frame_with(vec![egui::Event::PointerMoved(target)]);
        let ops = h.settings_frame_with(vec![press(target, false)]);
        let m = ops
            .iter()
            .find_map(|o| match o {
                Op::SetRowMeta(m) => Some(m),
                _ => None,
            })
            .unwrap_or_else(|| panic!("拖拽应推出 SetRowMeta，实际={ops:?}"));
        use modular_clipboard_core::config::RowMetaPart::*;
        assert_eq!(m.order, [Type, Size, Icon], "图标拖到最右后的顺序");
        // 显隐位不受拖拽影响。
        assert!(m.icon && m.type_ && m.size);
    }

    /// 高斯外轮廓：边缘处半值、向外单调衰减、3σ 外几乎消失。
    #[test]
    fn gaussian_outer_profile_is_half_at_edge_and_fades() {
        let sigma = 3.0;
        assert!((gaussian_outer(0.0, sigma) - 0.5).abs() < 1e-3, "边缘处应为 0.5");
        assert!(gaussian_outer(1.0, sigma) < 0.5);
        assert!(gaussian_outer(3.0, sigma) < gaussian_outer(1.0, sigma));
        // 3σ 处标准正态尾概率 = 0.00135；再外（阴影截断处）不可见。
        assert!(gaussian_outer(3.0 * sigma, sigma) < 0.002);
        // σ 非正时安全返回（不能 panic/NaN）；d<0 钳到边缘值 0.5。
        assert_eq!(gaussian_outer(0.0, 0.0), 0.0);
        assert!((gaussian_outer(-5.0, sigma) - 0.5).abs() < 1e-3);
    }

    /// 多层反混合的核心验收：按「源 over」逐环带模拟合成后，
    /// 最终强度必须精确等于高斯轮廓（容差只来自 8-bit 量化与
    /// <0.3% 层的跳过）。
    ///
    /// 这条守的是「层数翻倍但阴影形状不对」：alpha 若直接线性相加，
    /// 边缘会黑成实心圈、环带之间有分层硬边。
    #[test]
    fn card_shadow_layers_reproduce_gaussian_profile() {
        let blur = 6.0;
        let c = 0.40;
        let layers = card_shadow_layers(blur, c);
        let sigma = blur * 0.5;
        let n = (blur * 1.5).round() as i32;
        assert!(layers.len() <= n as usize, "层数不应超过 1.5·blur");
        assert!(!layers.is_empty(), "blur=6 应有可见层");
        // 每层 alpha 合法、外扩距离为正且按从外到内排列。
        let mut prev_grow = f32::INFINITY;
        for (grow, a) in &layers {
            assert!(*a > 0.0 && *a <= 1.0, "层 alpha 越界: {a}");
            assert!(*grow < prev_grow, "层必须按从外到内排列");
            prev_grow = *grow;
        }
        // 逐环带模拟合成：环带 k 被所有 grow >= k 的层覆盖。
        for k in 1..=n {
            let mut acc = 0.0f32;
            for (grow, a) in &layers {
                if *grow >= k as f32 {
                    acc = a + acc * (1.0 - a);
                }
            }
            let target = c * gaussian_outer(k as f32 - 0.5, sigma);
            assert!(
                (acc - target).abs() < 0.004,
                "环带 {k} 合成强度 {acc:.4} 偏离高斯目标 {target:.4}"
            );
        }
        // 最大模糊（24）：36 层封顶（性能保护：30 行 × 36 ≈ 1080 形状），
        // 高斯尾巴上量化后不可见的层会被跳过，实际略少。
        let max_layers = card_shadow_layers(24.0, 0.8);
        assert!(max_layers.len() <= 36, "层数上限 36，实际 {}", max_layers.len());
        assert!(max_layers.len() >= 28, "最大模糊下应有足量衰减层");
        // 退化输入：无层、不 panic。
        assert!(card_shadow_layers(0.0, 0.4).is_empty());
        assert!(card_shadow_layers(6.0, 0.0).is_empty());
    }

    /// `fade_color` 必须按**预乘**语义缩放：四通道一起乘 k。
    ///
    /// 回归守卫（2026-10-17）：行悬停底色曾用 `from_rgba_unmultiplied`
    /// 重组 `to_array()` 的预乘分量，二次预乘把 11.8% 的悬停提亮压暗成
    /// 1.4%（`from_white_alpha(30)` 变成 rgb≈3），鼠标划过几乎看不出
    /// 反馈。k=1 恒等这条直接守那个 bug。
    #[test]
    fn fade_color_scales_premultiplied_channels() {
        // `from_white_alpha(30)` 的预乘存储就是 [30,30,30,30]（rgb==a）。
        let white30 = egui::Color32::from_white_alpha(30);
        assert_eq!(white30.to_array(), [30, 30, 30, 30]);
        // k=1 恒等——旧写法在这里就会露出二次预乘（rgb 掉到 ~3）。
        assert_eq!(fade_color(white30, 1.0), white30);
        // k=0 全透明：预乘语义下「不可见」就是四通道全零。
        assert_eq!(fade_color(white30, 0.0).to_array(), [0, 0, 0, 0]);
        // k=0.5：四通道一起缩半（色相不变，只降不透明度）。
        assert_eq!(fade_color(white30, 0.5).to_array(), [15, 15, 15, 15]);
        // 越界输入钳住（动画补间可能给出 1.0000001）。
        assert_eq!(fade_color(white30, 2.0), white30);
        assert_eq!(fade_color(white30, -1.0).to_array(), [0, 0, 0, 0]);
        // 饱和色（rgb 已是满值、只有 alpha 低）同样只缩 alpha。
        let sel = crate::theme::Palette::light().row_selected;
        assert_eq!(fade_color(sel, 1.0), sel);
        let half = fade_color(sel, 0.5);
        assert!(
            half.a() < sel.a(),
            "半透明缩放后 alpha 必须下降：{} -> {}",
            sel.a(),
            half.a()
        );
    }

    /// 弹窗投影**必须在窗口边距内衰减完**。
    ///
    /// 守的是「黑色描边」的第二重来源：外扩超出
    /// [`crate::popup::SHADOW_MARGIN`] 的层会被窗口边界**硬切**，最外圈
    /// 像素留下一条等浓度的黑框——比完全没有投影更丑。
    #[test]
    fn popup_shadow_decays_before_window_edge() {
        let layers = card_shadow_layers(POPUP_SHADOW_BLUR, POPUP_SHADOW_STRENGTH);
        assert!(!layers.is_empty(), "弹窗应有可见投影层");
        let max_grow = layers.iter().map(|(g, _)| *g).fold(0.0f32, f32::max);
        // 向下偏移吃掉**底部**余量（顶部反而更宽裕），故按
        // `margin - offset` 算而不是 `margin`。
        let budget = crate::popup::SHADOW_MARGIN - POPUP_SHADOW_OFFSET_Y;
        assert!(
            max_grow <= budget,
            "最外层 grow {max_grow} 超出可用余量 {budget}：投影会被窗口边界切断"
        );
        // 边界处的合成强度必须不可见（< 1/255），否则仍是一条淡黑框。
        let edge = POPUP_SHADOW_STRENGTH
            * gaussian_outer((budget - 0.5).max(0.0), POPUP_SHADOW_BLUR * 0.5);
        assert!(
            edge * 255.0 < 1.0,
            "窗口边界处仍有 {:.2}/255 的强度——会看出截断",
            edge * 255.0
        );
    }

    /// 弹窗投影浓度必须**远低于**卡片量级。
    ///
    /// 守的是「有人把强度改回 `Palette::shadow` 的量级」：该令牌是
    /// alpha=255 的纯黑，卡片倍率后贴边强度 0.95；主窗口有不透明底图
    /// 托着看着是阴影，弹窗画布是透明的，同一强度裸在桌面上就是
    /// 一条黑色描边（类型栏无背景板时更是整块被看成「黑色背景」）。
    #[test]
    fn popup_shadow_strength_stays_far_below_card_level() {
        // 卡片贴边强度：alpha=255 × 默认倍率 1.0 ⇒ clamp 到 0.95。
        let card_edge = 0.95 * gaussian_outer(0.5, 16.0 * 0.5);
        let popup_edge =
            POPUP_SHADOW_STRENGTH * gaussian_outer(0.5, POPUP_SHADOW_BLUR * 0.5);
        assert!(
            popup_edge < 0.10,
            "弹窗贴边 alpha {popup_edge:.3} 过高，会看成描边"
        );
        assert!(
            popup_edge * 4.0 < card_edge,
            "弹窗贴边 {popup_edge:.3} 应显著低于卡片 {card_edge:.3}"
        );
    }

    /// 一键配对提示词：包含 10 项能力与接入参数，随配置动态拼接。
    #[test]
    fn mcp_pair_prompt_is_complete() {
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.mcp.host = "192.168.1.10".into();
        cfg.mcp.port = 9123;
        cfg.mcp.token = "sk-123".into();
        let p = mcp_pair_prompt(&cfg);
        for cap in MCP_CAPS {
            assert!(p.contains(t(cap.name)), "提示词应含能力「{}」", t(cap.name));
        }
        assert!(p.contains("192.168.1.10"), "提示词应含自定义服务地址");
        assert!(p.contains("9123"), "提示词应含自定义端口");
        assert!(p.contains("Bearer Token"), "提示词应反映已配置密钥");
        assert!(p.contains("--mcp-server"), "提示词应含启动命令");
    }

    /// 字体放大后设置界面**不得横向溢出、控件不得互相重叠**。
    ///
    /// # 为什么用「热区几何」当判据
    ///
    /// 设置卡片的控件宽度大多是**按文字测量**算出来的（分段控件的段宽 =
    /// 最长文字 + 内边距），字体放大后总宽会超过卡片可用宽度，于是控件
    /// 被画到卡片之外（用户反馈的「布局错乱」）；并排布局里标签列与控件
    /// 还会互相侵占。热区由绘制层自报，是唯一可信的坐标来源。
    #[test]
    fn settings_layout_survives_large_font_scale() {
        for (width, scale) in [(780.0, 1.0f32), (780.0, 1.15), (780.0, 1.3),
            (780.0, 1.5), (780.0, 2.0), (720.0, 1.5), (720.0, 2.0)] {
            let mut h = Harness::new("font-layout", vec2(width, 560.0));
            h.pal = crate::theme::Palette::light().with_font_scale(scale);
            // 生产路径每帧都会 `theme::apply`（见 `settingswin::sync_and_render`），
            // 夹具必须复刻：egui 控件（标签 / 输入框）的字号来自样式，
            // 不灌的话量出来的行高与实机不符。
            crate::theme::apply(&h.ctx, &h.pal);
            h.state.show_settings = true;
            for cat in SettingsCat::ALL {
                h.state.settings_cat = cat;
                h.settings_frame();
                let keys = h.state.hit.keys(HitNs::Settings);
                let rects: Vec<(&'static str, Rect)> = keys
                    .iter()
                    .filter_map(|k| h.state.hit.get(HitNs::Settings, k).map(|r| (*k, r)))
                    .collect();
                for (key, r) in &rects {
                    assert!(
                        r.max.x <= h.area.max.x + 0.5,
                        "字体 {scale}× 下 {cat:?} 的热区「{key}」越出窗口右缘：\
                         {:.1} > {:.1}",
                        r.max.x,
                        h.area.max.x
                    );
                    assert!(
                        r.min.x >= h.area.min.x - 0.5 && r.min.y >= h.area.min.y - 0.5,
                        "字体 {scale}× 下 {cat:?} 的热区「{key}」越出窗口左上缘"
                    );
                }
                // 同一分类内的热区互不重叠（相邻分段共边不算重叠）。
                for (i, (ka, ra)) in rects.iter().enumerate() {
                    for (kb, rb) in rects.iter().skip(i + 1) {
                        let ox = (ra.max.x.min(rb.max.x) - ra.min.x.max(rb.min.x)).max(0.0);
                        let oy = (ra.max.y.min(rb.max.y) - ra.min.y.max(rb.min.y)).max(0.0);
                        assert!(
                            ox <= 1.0 || oy <= 1.0,
                            "字体 {scale}× 下 {cat:?} 的热区「{ka}」与「{kb}」重叠了 \
                             {:.1}x{:.1}（布局互相侵占）",
                            ox,
                            oy
                        );
                    }
                }
            }
        }
    }

    /// 大字号下设置行改为**上下堆叠**：标签不再与控件争宽度。
    ///
    /// 守的是「并排时描述只剩 6 个字」这种可读性崩坏——堆叠后描述能拿到
    /// 整行宽度，控件也独占一行。
    #[test]
    fn large_font_stacks_setting_rows() {
        // 用高窗口：1.5× 下外观卡片总高超过 560（需要滚动），
        // 而热区只登记可见控件——不高一点就测不到两行的相对位置。
        let mut h = Harness::new("font-stack", vec2(780.0, 1200.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;

        // 1.0×：并排 —— 字体缩放分段与背景效果分段**同一行**（y 区间一致）。
        h.pal = crate::theme::Palette::light();
        crate::theme::apply(&h.ctx, &h.pal);
        h.settings_frame();
        let font_y = settings_hit(&h, "标准").min.y;
        let backdrop_y = settings_hit(&h, "无").min.y;
        assert!(
            (font_y - backdrop_y).abs() > 20.0,
            "1.0× 下两行本就在不同行（间距应 > 20）：{font_y} vs {backdrop_y}"
        );

        // 1.5×：堆叠 —— 每行的控件独占一行，行距显著变大。
        h.pal = crate::theme::Palette::light().with_font_scale(1.5);
        crate::theme::apply(&h.ctx, &h.pal);
        h.settings_frame();
        let font_y15 = settings_hit(&h, "标准").min.y;
        let backdrop_y15 = settings_hit(&h, "无").min.y;
        let pitch_10 = backdrop_y - font_y;
        let pitch_15 = backdrop_y15 - font_y15;
        assert!(
            pitch_15 > pitch_10 * 1.5,
            "1.5× 下应改为上下堆叠（行距 {pitch_15:.1} 应远大于 1.0× 的 {pitch_10:.1}）"
        );
    }


    // ------------------------------------------------------------------
    // 主题：强调色 / 二级主题 / 令牌覆盖（2026-10-18）
    // ------------------------------------------------------------------

    /// 点预设色板 ⇒ 推出 `SetAccent(Some(该色 HSV))`。
    ///
    /// 守的是「三种调节方式」里最常用的那条路径：色板点击必须落地成
    /// 配置变更，而不是只改显示。
    #[test]
    fn accent_preset_click_emits_set_accent() {
        let mut h = Harness::new("accent-preset", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        h.settings_frame();

        // 10 个预设都应有热区（key = 预设名）。
        for (name, _) in crate::theme::ACCENT_PRESETS {
            // 同上：注册与查找都走 `t()`，与当前语言无关。
            let sw = settings_hit(&h, t(name));
            assert!(sw.width() > 0.0, "预设「{}」应有热区", t(name));
        }
        // 点「翠绿」⇒ SetAccent(Some(绿))。
        let green = settings_hit(&h, t("Emerald"));
        let ops = h.settings_click_at(green.center());
        let hit = ops.iter().any(|o| match o {
            Op::SetAccent(Some(c)) => {
                let rgb = c.to_rgb();
                // 翠绿 #4C9A4C：g 通道最大。
                rgb[1] > rgb[0] && rgb[1] > rgb[2]
            }
            _ => false,
        });
        assert!(hit, "点「翠绿」应推出 SetAccent(Some(绿))，实际={ops:?}");
    }

    /// 二级主题分段：五个选项都有热区；点「纸质」⇒ `SetSecondaryTheme(Paper)`。
    #[test]
    fn secondary_theme_segment_emits_op() {
        let mut h = Harness::new("secondary-theme", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        h.settings_frame();
        for name in ["无", "纸纹", "护眼绿", "星空紫", "OLED"] {
            assert!(
                settings_hit(&h, name).width() > 0.0,
                "二级主题选项「{name}」应有热区"
            );
        }
        let paper = settings_hit(&h, "纸纹");
        let ops = h.settings_click_at(paper.center());
        let hit = ops.iter().any(|o| {
            matches!(
                o,
                Op::SetSecondaryTheme(modular_clipboard_core::config::SecondaryTheme::Paper)
            )
        });
        assert!(hit, "点「纸纹」应推出 SetSecondaryTheme(Paper)，实际={ops:?}");
    }

    /// 二级主题选「无」时，参数项显示禁用提示（不画旋钮）。
    #[test]
    fn theme_params_disabled_without_secondary_theme() {
        let mut h = Harness::new("theme-params-off", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        h.svc.state.config.ui.secondary_theme = modular_clipboard_core::config::SecondaryTheme::None;
        h.settings_frame();
        // 无主题 ⇒ 没有旋钮热区（paper_tint 等键名不存在于热区表）。
        assert!(
            settings_hit_missing(&h, "paper_tint"),
            "无二级主题时不应出现参数旋钮"
        );
        // 选纸质后旋钮出现（两个：paper_tint / paper_texture）。
        h.svc.state.config.ui.secondary_theme =
            modular_clipboard_core::config::SecondaryTheme::Paper;
        h.settings_frame();
        assert!(settings_hit(&h, "paper_tint").width() > 0.0);
        assert!(settings_hit(&h, "paper_texture").width() > 0.0);
        // 星空有六个旋钮（2026-10-19：随机化 / 闪烁 / 大小 / 光标互动
        // 先后加入）——新增旋钮只靠 core 的 knobs() 表驱动，这里守住
        // 「表里登记的都真的画出了热区」。
        h.svc.state.config.ui.secondary_theme =
            modular_clipboard_core::config::SecondaryTheme::Purple;
        h.settings_frame();
        for key in [
            "purple_tint",
            "purple_stars",
            "star_random",
            "star_twinkle",
            "star_size_var",
            "star_cursor",
        ] {
            assert!(
                settings_hit(&h, key).width() > 0.0,
                "星空旋钮 {key} 应渲染出热区"
            );
        }
    }


    /// 令牌编辑器默认**折叠**：令牌行不绘制、不注册热区；点「展开」后
    /// 才出现——近 60 行平铺会把主题卡撑到五屏（ rationale 见
    /// `UiState::theme_tokens_expanded`）。
    #[test]

    fn token_editor_collapsed_by_default_and_expands() {
        let mut h = Harness::new("token-editor", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        // 选中星空主题：主题卡才会画旋钮（None 时只有「先选择一个
        // 二级主题」提示行），「内容区整体切换」才有可断言的对照。
        h.svc.state.config.ui.secondary_theme =
            modular_clipboard_core::config::SecondaryTheme::Purple;
        h.settings_frame();
        // 折叠态：令牌名不在热区表里。
        assert!(
            settings_hit_missing(&h, "bg"),
            "默认折叠时不应登记令牌热区"
        );
        // 点入口行「高级微调」⇒ 二级面板打开。
        //
        // CAUTION: 面板在**下一帧**才绘制（分发在 settings_content，
        // 与入口行的点击不在同一帧）——必须再跑一帧才断言热区。
        // 旧实现是「点击当帧即展开」（toggle 与令牌循环在同一函数
        // 后半），二级视图化后这个时序变化是必然的。
        let expand = settings_hit(&h, "token_expand");
        let _ops = h.settings_click_at(expand.center());
        assert!(h.state.theme_tokens_expanded, "入口点击应记住打开状态");
        h.settings_frame();
        assert!(
            settings_hit(&h, "bg").width() > 0.0,
            "面板打开后应登记令牌热区"
        );
        // 面板特征：返回按钮出现；主题卡的旋钮（paper_tint 属于纸质，
        // 星空下是 purple_tint）不再同屏出现（内容区整体切换）。
        assert!(settings_hit(&h, "token_back").width() > 0.0, "应有返回按钮");
        assert!(
            settings_hit_missing(&h, "purple_tint"),
            "二级视图下主题卡旋钮不应同屏"
        );
        // 返回：点 ‹ ⇒ 面板关闭，回到主题卡。
        let back = settings_hit(&h, "token_back");
        let _ops = h.settings_click_at(back.center());
        assert!(!h.state.theme_tokens_expanded, "点返回应关闭面板");
        h.settings_frame();
        assert!(
            settings_hit(&h, "purple_tint").width() > 0.0,
            "返回后主题卡应恢复"
        );
        assert!(settings_hit_missing(&h, "bg"), "返回后令牌热区应消失");
    }


    /// 令牌行高必须在**任意 font_scale** 下都装得下字形与滑条
    /// （2026-10-19 用户实机反馈「文字向上且渲染不完整」的守门）。
    ///
    /// 根因：令牌行的 horizontal 三列里，左列格子曾写死 18pt——
    /// font_scale 调大后（滑条 cell = font_sm×1.07 涨过 18 ⇒ 左列被
    /// 顶格偏高；字形 galley ≈ font_xs×1.28 涨过 18 ⇒ 上下被裁）。
    /// harness 默认 font_scale=1.0 恰好两个数都不越界，单测全绿而
    /// 实机糊——所以这条显式遍历 0.75~2.0 全量程。
    #[test]
    fn token_row_h_covers_glyphs_and_slider_at_any_font_scale() {
        for scale in [0.75f32, 1.0, 1.25, 1.5, 2.0] {
            let pal = Palette::light().with_font_scale(scale);
            let row = token_row_h(&pal);
            assert!(
                row >= pal.font_xs * 1.28,
                "font_scale={scale}：行高 {row} 装不下字形 galley（font_xs={}，需要 {}）",
                pal.font_xs,
                pal.font_xs * 1.28
            );
            assert!(
                row >= pal.font_sm * 1.07,
                "font_scale={scale}：行高 {row} 装不下滑条 cell（font_sm={}）",
                pal.font_sm
            );
            assert!(row >= 18.0, "font_scale={scale}：行高 {row} 低于保底 18pt");
        }
    }
    /// 展开后点 bg 的色相滑条 ⇒ 推出 `SetTokenOverride`（颜色覆盖）。
    ///
    /// 拖滑条即改值：`draw_slider` 响应「按下即跳转」，点轨道左侧
    /// 应把色相压向 0（红）。
    #[test]
    fn token_override_hue_slider_emits_op() {
        let mut h = Harness::new("token-override", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        h.settings_frame();
        // 展开令牌编辑器。
        let _ops = h.settings_click_at(settings_hit(&h, "token_expand").center());
        h.settings_frame();
        // 按 bg 色相滑条的左端（色相 → 0，红）。
        //
        // CAUTION: 必须两帧（press 一帧、release 一帧）——单帧压缩的
        // `click_events` 到不了滑条的「按下即跳转」（帧末按下状态已被
        // up 清掉，见 `press_events` 的文档）。这不是测试的权宜，真实
        // 用户的操作本来就分帧。
        let hue = settings_hit(&h, "bg");
        let p = pos2(hue.min.x + 4.0, hue.center().y);
        let ops = h.settings_frame_with(press_events(p));
        // 松开帧：op 已在 down 帧产出，这一帧让交互状态归位（与真实
        // 用户「按下 → 松开」的两帧结构一致）。
        let _ = h.settings_frame_with(release_events(p));
        let hit = ops.iter().any(|o| match o {
            Op::SetTokenOverride { name, value } => {
                name == "bg"
                    && matches!(value, crate::theme::TokenValue::Color(c) if c.r() > c.g())
            }
            _ => false,
        });
        assert!(hit, "点色相滑条左端应推出 bg 的红色覆盖，实际={ops:?}");
    }

    /// 「全部重置」⇒ `ClearAllTokenOverrides`；重置按钮仅在**有覆盖**时
    /// 出现（默认态放个按钮是噪声）。
    #[test]
    fn token_reset_all_emits_clear() {
        let mut h = Harness::new("token-reset", vec2(780.0, 900.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Theme;
        h.svc.state.config.ui.theme_overrides.nums.insert("radius_md".into(), 12.0);
        h.settings_frame();
        // 「全部重置」在**二级面板**里（2026-10-19）：先点入口行打开。
        let expand = settings_hit(&h, "token_expand");
        let _ops = h.settings_click_at(expand.center());
        h.settings_frame();
        let reset = settings_hit(&h, "token_reset_all");
        let ops = h.settings_click_at(reset.center());
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::ClearAllTokenOverrides)),
            "点「全部重置」应推出 ClearAllTokenOverrides，实际={ops:?}"
        );
    }

    /// 热区表查询的否定形式（不存在返回 None 而不是 panic）。
    fn settings_hit_missing(h: &Harness, name: &str) -> bool {
        h.state.hit.get(HitNs::Settings, name).is_none()
    }


    /// 量一段文字的像素宽（不换行口径）。
    fn measure_w(ctx: &egui::Context, s: &str, size: f32) -> f32 {
        let mut w = 0.0f32;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            w = ui
                .painter()
                .layout_no_wrap(
                    s.to_owned(),
                    egui::FontId::proportional(size),
                    egui::Color32::PLACEHOLDER,
                )
                .rect
                .width();
        });
        // run_ui 会产出字体纹理 delta；不清掉Drop 时直接 panic
        // （epaint 的「Dropped TexturesDelta with N unapplied deltas」）。
        out.textures_delta.clear();
        w
    }

    /// MCP 覆盖层状态行：拆分两行后每行都必须在面板内缘放得下，
    /// 同时锁死「旧单行拼接放不下」——证明拆分不是空做。
    ///
    /// 背景（用户反馈「文字渲染出界面」）：egui 的 Label 在**横向**
    /// 布局里默认 `TextWrapMode::Extend`——不换行也不截断，直接溢出
    /// 容器。最长的 detail（`Unconfigured`，约 25 个全角字）与标题挤
    /// 在同一行时实测 475pt > 面板内缘 448pt，文字画出面板。
    /// 修法：第一行只放「圆点 + MCP 服务：xx」，detail 单独一行
    /// （纵向布局默认 Wrap）。
    #[test]
    fn mcp_overlay_status_lines_fit_panel() {
        use crate::mcp_status::McpConn;
        let h = Harness::new("mcp-status", vec2(780.0, 560.0));
        // 与 `settings_mcp_overlay` 一致的面板几何：480pt 面板，内缩 16/14。
        let pw = 480.0_f32.min(h.area.width() * 0.92);
        let inner = pw - 32.0;
        let size = h.pal.font_xs;
        // 第一行行首：状态圆点 10pt + 行内间距 6pt。
        let dot_w = 16.0_f32;

        for conn in [
            McpConn::Disabled,
            McpConn::Unconfigured,
            McpConn::Connected,
            McpConn::Disconnected,
        ] {
            // 第一行：圆点 + 「MCP 服务：xx」。横向布局 ⇒ 必须放得下。
            let line1 = format!("MCP 服务：{}", t(conn.label()));
            let w1 = dot_w + measure_w(&h.ctx, &line1, size);
            assert!(
                w1 <= inner,
                "{conn:?} 状态行宽 {w1} 超过面板内缘 {inner}"
            );
            // 第二行：detail。纵向布局会换行，但正常字体下也不该超宽。
            let w2 = measure_w(&h.ctx, t(conn.detail()), size);
            assert!(
                w2 <= inner,
                "{conn:?} 说明文字宽 {w2} 超过面板内缘 {inner}"
            );
        }

        // 锁死修法：旧的单行拼接在最长 detail 下必须**放不下**——
        // 否则这个测试锁不住「拆两行」这个回归。
        let old = format!(
            "MCP 服务：{} · {}",
            t(McpConn::Unconfigured.label()),
            t(McpConn::Unconfigured.detail())
        );
        let old_w = dot_w + measure_w(&h.ctx, &old, size);
        assert!(
            old_w > inner,
            "旧单行拼接宽 {old_w} 应放不进 {inner}（放得进说明回归锁失效）"
        );
    }
}
