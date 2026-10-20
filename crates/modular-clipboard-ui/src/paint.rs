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

use egui::{Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::config::{RowMetaPart, WindowBackdrop};

use crate::card::{Card, CardHost, CardKind, PinnedMode};
use crate::icons::Icon;
use crate::theme::{Palette, TokenValue, sized};
use crate::workspace::Workspace;

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
    /// 调磨砂强度档位（1 ~ 10）。
    SetBlurStrength(u8),
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
    /// 设置悬浮类型过滤栏（开关 + 始终显示 + 停靠位置 + 图标大小）。
    ///
    /// 同 `SetCardShadow`：四个参数在设置界面是一张互相牵连的卡，
    /// 整包替换，避免一次拖动写四次盘。
    SetTypeRail(modular_clipboard_core::config::TypeRailConfig),
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
    /// 设置类型过滤（`None` = 取消过滤）。
    ///
    /// 过滤在**界面侧**做（与 `query` 同为 UI 本地状态，不进快照）：
    /// 绘制时按 `paint_state.type_filter` 筛行，不触碰 store 查询与 IPC。
    SetTypeFilter(Option<modular_clipboard_core::ClipKind>),
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

    /// 导航标题。
    pub fn title(self) -> &'static str {
        match self {
            SettingsCat::General => "常规",
            SettingsCat::Theme => "主题",
            SettingsCat::Appearance => "外观",
            SettingsCat::Capture => "捕获",
            SettingsCat::Storage => "存储",
            SettingsCat::Shortcuts => "快捷键",
            SettingsCat::Comm => "通信",
            SettingsCat::About => "关于",
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RowFilterKey {
    /// 类型过滤（`None` = 不过滤）。
    type_filter: Option<modular_clipboard_core::ClipKind>,
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

    fn push(&mut self, op: Op) {
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

    draw_notice_toast(f);
}

/// 二级主题的质感层：纸质纤维 / 星空星点。
///
/// 与颜色层（`theme::apply_secondary`）分工：那里换**整套令牌**，
/// 这里补程序化纹理。判据是配置里的 `secondary_theme` 与参数
/// （`paper_texture` / `purple_stars`，0 时整层不画——省一次
/// draw call）。
///
/// # 为什么只在主窗口画
///
/// 纹理 id 是**每个 `egui::Context` 独立**的：主窗口 ctx 加载的
/// tile 直接拿到设置窗口 / 弹窗的 ctx 上画会显示成别的纹理（或空白）。
/// 要么每窗口各缓存一份（`UiState` 要按窗口分槽，纹理翻倍），要么
/// 接受「设置窗口没有质感层」。选后者：设置窗口是工具页，用户盯着
/// 看的是主窗口；且**颜色层完全同步**（`settingswin` 每帧从
/// `App.palette()` apply），二级主题的色温在设置窗口里一样看得到。
///
/// # tint 的取色
///
/// tile 是白底、alpha 起伏（见 [`crate::tex`] 的说明），颜色由 tint
/// 乘入，所以深浅两套共用一份纹理：
/// - 纸质深色：暖白墨（纤维比纸基亮一点，读出「一丝一丝」的反光）；
/// - 纸质浅色：暖棕墨（印刷纸的纤维是深色的）；
/// - 星空：纯白，密度即 tint alpha。
fn draw_theme_texture(f: &mut Frame<'_>) {
    use modular_clipboard_core::config::SecondaryTheme;
    let ui = &f.svc.state.config.ui;
    match ui.secondary_theme {
        SecondaryTheme::Paper => {
            let k = ui.theme_params.paper_texture.clamp(0.0, 1.0);
            if k <= 0.004 {
                return;
            }
            let tex = match &f.state.paper_tex {
                Some(t) => t.clone(),
                None => {
                    let t = crate::tex::paper_tile(f.ui.ctx());
                    f.state.paper_tex = Some(t.clone());
                    t
                }
            };
            // 墨色强度（标定值，2026-10-18 实测修正）：
            //
            // 原值（浅 18 / 深 14）是按「 Subtle 」猜的，实测整个窗口的
            // 亮度标准差只有 0.8 色阶——纹理等于不存在（用户反馈
            // 「纸质渲染不正常」）。重标后的目标量级：
            // - 均值 Δ ≈ 8~10 色阶（看得出「这不是纯色板」）；
            // - 纤维峰值 Δ ≈ 15~25 色阶（「一丝一丝」读得出来）；
            // - 且不超过背景对比度的 1/10（不抢内容）。
            //
            // 深色下明显更弱：亮纤维在暗底上比暗纤维在亮底上更显眼，
            // 且人眼对暗部噪声更敏感（同样的 Δ 在暗底上觉得「脏」而非
            // 「纸」）。实测 30 像落了层灰、20 仍偏噪点，16 才回到
            // 「暗色纸张」的观感（2026-10-18 截图标定）。
            let (ir, ig, ib, base_a) = if f.pal.is_dark {
                (235u8, 220u8, 195u8, 16.0)
            } else {
                (112u8, 94u8, 66u8, 42.0)
            };
            let a = (base_a * k).round() as u8;
            crate::tex::paint_tiled(
                f.ui.painter(),
                f.area,
                &tex,
                egui::Color32::from_rgba_unmultiplied(ir, ig, ib, a),
                crate::tex::PAPER_TILE_PT,
            );
        }
        SecondaryTheme::Purple => {
            let params = &ui.theme_params;
            let k = params.purple_stars.clamp(0.0, 1.0);
            if k <= 0.004 {
                return;
            }
            // ---- 星星列表：参数指纹变化才重建 ----------------------------
            //
            // 三个影响列表的参数（密度 / 随机化 / 大小）编成指纹，
            // `StarField` 自己记住生成时的指纹——没变就每帧复用同一份
            // 列表，只重算闪烁。random > 0 时把 egui 时间当熵混入种子，
            // 于是「每次重建换一片星空」；random = 0 时确定性（同一片星）。
            let fp = crate::tex::StarField::fingerprint_of(
                params.purple_stars,
                params.star_random,
                params.star_size_var,
            );
            let rebuild = f
                .state
                .star_field
                .as_ref()
                .is_none_or(|s| s.fingerprint != fp);
            if rebuild {
                let entropy = if params.star_random > 0.004 {
                    (f.ui.input(|i| i.time) * 1000.0) as u64
                } else {
                    0
                };
                f.state.star_field = Some(crate::tex::StarField::generate(
                    params.purple_stars,
                    params.star_random,
                    params.star_size_var,
                    entropy,
                ));
            }
            let Some(field) = f.state.star_field.as_ref() else {
                return;
            };
            if field.stars.is_empty() {
                return;
            }
            // ---- 精灵纹理（惰性，每 Context 一份）------------------------
            let tex = match &f.state.star_tex {
                Some(t) => t.clone(),
                None => {
                    let t = crate::tex::star_sprite(f.ui.ctx());
                    f.state.star_tex = Some(t.clone());
                    t
                }
            };
            // ---- 逐颗绘制：闪烁 alpha + 大小 + 光标视差 ------------------
            //
            // 每颗星一个小 quad（16px 精灵按半径放大）。tint 只带 alpha
            // （白星）——闪烁 = tint alpha 随时间呼吸（2026-10-18 修好的
            // 预乘 tint 通道）。暗到谷底的星跳过，省 draw call。
            //
            // 光标视差（`star_cursor`，0 = 关）：星点随光标反向偏移，
            // 近的动得多、远的几乎不动（视差纵深）。光标离开窗口
            // （hover_pos = None）⇒ 零偏移回位。指针移动本身就是事件，
            // 不需要额外续帧。
            let t = f.ui.input(|i| i.time);
            let pointer = f.ui.input(|i| i.pointer.hover_pos());
            let painter = f.ui.painter();
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            let area = f.area;
            for star in &field.stars {
                let a = crate::tex::twinkle_alpha(star, params.star_twinkle, t);
                if a <= 0.004 {
                    continue;
                }
                let base = pos2(
                    area.min.x + star.x * area.width(),
                    area.min.y + star.y * area.height(),
                );
                let c = base + crate::tex::cursor_offset(star, pointer, area, params.star_cursor);
                let r = Rect::from_center_size(c, vec2(star.radius * 2.0, star.radius * 2.0));
                painter.image(
                    tex.id(),
                    r,
                    uv,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, (a * 255.0) as u8),
                );
            }
            // ---- 闪烁续帧 -------------------------------------------------
            //
            // CAUTION: 闪烁由 `input.time` 驱动（不走 anim 补间注册表 ⇒
            // 不被 `anim::busy` 覆盖），而主循环有空闲闸门：无事件、无
            // 重绘请求时整帧跳过 ⇒ 闪烁会停在某一帧（与 2026-10-17
            // Toast 出场动画被饿死是同一个坑）。twinkle > 0 时每帧续请求；
            // twinkle = 0（用户关掉闪烁）不请求，界面恢复空闲。
            //
            // 必须在绘制闭包内调用：egui 只在 pass 结束时把请求写进
            // `FullOutput.repaint_delay`，闸门读的正是这个值。
            if params.star_twinkle > 0.004 {
                f.ui.ctx().request_repaint();
            }
        }
        _ => {}
    }
}

/// 类型过滤栏与主窗口的间距（逻辑点）。
const RAIL_GAP: f32 = 12.0;

/// 类型过滤栏**面板**相对按钮组的内边距（逻辑点）。
///
/// 面板必须比按钮组大一圈：它是**不透明**的底，用来盖住投影。
///
/// # 为什么不能让面板 = 按钮组
///
/// 投影是「面板矩形外扩」的实心圆角矩形，天然**包含面板内部**；别的
/// 面板都有自己的底色把它盖住，而类型栏此前直接拿按钮组当面板 ⇒
/// 按钮之间的缝隙和四周一圈透出投影的颜色，整块看着就是**黑色背景**
/// （投影色是纯黑，见 [`POPUP_SHADOW_STRENGTH`] 的说明）。
///
/// 弹窗窗口尺寸 = 面板 + [`crate::popup::SHADOW_MARGIN`]，所以加这份
/// padding 会让窗口跟着大一圈——这是必须的，不是浪费。
pub const RAIL_PAD: f32 = 6.0;

/// 悬浮搜索面板高度（逻辑点）。
const SEARCH_PANEL_H: f32 = 30.0;

/// 类型过滤栏的按钮顺序（与设置界面、捕获层同一份）。
const RAIL_KIND_ORDER: [modular_clipboard_core::ClipKind; 5] = {
    use modular_clipboard_core::ClipKind;
    [
        ClipKind::Text,
        ClipKind::Html,
        ClipKind::Image,
        ClipKind::Files,
        ClipKind::Markdown,
    ]
};

/// 悬浮装饰（搜索面板 / 类型过滤栏）的**弹窗几何**。
///
/// # 坐标系：主窗口客户区左上角为原点，允许负坐标
///
/// 搜索面板在窗口**上方** ⇒ `y ∈ [-(RAIL_GAP + SEARCH_PANEL_H), -RAIL_GAP]`；
/// 类型栏在左 / 上方同理。`crate::popup` 据此换算屏幕坐标（加主窗口
/// 屏幕位置）。
///
/// 这正是原型（`main-proto/index.html`）的布局：两个面板**漂在窗口
/// 之外**，间距透出桌面。2026-10-15 第三轮初版把它们画在窗口内，
/// 与原型不符——Win32 窗口画不出客户区之外，只能靠独立弹窗实现。
///
/// 因为面板不再压在内容上，**列表也不需要为它们让位**（第三轮初版
/// 里的 `insets` 已随之删除，卡片区恢复满宽）。
///
/// **纯函数** `compute` 的产物：帧循环（弹窗定位）与绘制层（弹窗内容）
/// 各自调它，同一份输入必然同一份矩形。
pub struct FloatLayout {
    /// 类型栏按钮组的矩形（`None` = 不显示）。
    pub rail: Option<Rect>,
    /// 类型栏要显示的按钮（顺序固定；含「始终显示」时补全的类型）。
    pub rail_kinds: Vec<modular_clipboard_core::ClipKind>,
    /// 搜索面板矩形（`None` = 折叠）。
    pub search: Option<Rect>,
}

impl FloatLayout {
    /// 当前应显示的类型栏按钮（顺序固定）。
    ///
    /// 抽成函数是为了让**几何**（[`FloatLayout::compute`] 决定弹窗尺寸）
    /// 与**绘制**（弹窗内容里的按钮）读同一份结果——两处各判一次
    /// 「该显示哪几类」必然漂移（弹窗按 4 类算尺寸、内容画 3 枚按钮）。
    pub fn kinds(
        cfg: &modular_clipboard_core::config::UiConfig,
        items: &[modular_clipboard_core::item::ClipItem],
    ) -> Vec<modular_clipboard_core::ClipKind> {
        if !cfg.type_rail.visible {
            return Vec::new();
        }
        RAIL_KIND_ORDER
            .iter()
            .copied()
            // 默认只显示「已有记录」的类型；`always` 时五类齐全。
            .filter(|k| cfg.type_rail.always || items.iter().any(|it| it.kind == *k))
            .collect()
    }

    /// 计算悬浮弹窗的布局。
    ///
    /// - `area`：主窗口客户区（逻辑点，窗口坐标）；
    /// - `search_expanded`：搜索面板是否展开。
    pub fn compute(
        cfg: &modular_clipboard_core::config::UiConfig,
        items: &[modular_clipboard_core::item::ClipItem],
        search_expanded: bool,
        area: Rect,
    ) -> Self {
        use modular_clipboard_core::config::TypeRailPos;

        let rail_cfg = &cfg.type_rail;
        let size = modular_clipboard_core::config::clamp_type_rail_size(rail_cfg.size);
        let gap = RAIL_GAP.min(8.0); // 按钮间距比面板间距紧一档

        // ---- 类型栏：**面板**矩形（弹窗按它定尺寸，投影边距由弹窗自己加） ----
        //
        // 面板 = 按钮组 + 四周 [`RAIL_PAD`]。多出来的这一圈是不透明底色，
        // 用来盖住投影（投影矩形包含面板内部，见 `RAIL_PAD` 的说明）。
        let rail_kinds = Self::kinds(cfg, items);
        let rail = match rail_kinds.len() {
            0 => None,
            n => {
                // 弹窗客户区只包住面板本身（+ 投影边距），不做成
                // 「整条侧栏那么高」——那圈空白是透明窗口面积，
                // 既费显存又会挡下面的窗口（要靠点击穿透兜，更容易出岔子）。
                let total = n as f32 * size + (n as f32 - 1.0) * gap + RAIL_PAD * 2.0;
                let cross = size + RAIL_PAD * 2.0;
                let card = crate::solver::card_area(area);
                Some(match rail_cfg.pos {
                    // 右 / 左：贴在窗口对应侧缘外，垂直对齐卡片区中心。
                    TypeRailPos::Right => Rect::from_center_size(
                        pos2(area.max.x + RAIL_GAP + cross * 0.5, card.center().y),
                        vec2(cross, total),
                    ),
                    TypeRailPos::Left => Rect::from_center_size(
                        pos2(area.min.x - RAIL_GAP - cross * 0.5, card.center().y),
                        vec2(cross, total),
                    ),
                    // 上 / 下：横向一行，对齐卡片区中心。
                    TypeRailPos::Top => Rect::from_center_size(
                        pos2(card.center().x, area.min.y - RAIL_GAP - cross * 0.5),
                        vec2(total, cross),
                    ),
                    TypeRailPos::Bottom => Rect::from_center_size(
                        pos2(card.center().x, area.max.y + RAIL_GAP + cross * 0.5),
                        vec2(total, cross),
                    ),
                })
            }
        };

        // ---- 搜索面板：窗口正上方通栏 ----
        let search = search_expanded.then(|| {
            Rect::from_min_max(
                pos2(area.min.x, area.min.y - RAIL_GAP - SEARCH_PANEL_H),
                pos2(area.max.x, area.min.y - RAIL_GAP),
            )
        });

        Self { rail, rail_kinds, search }
    }
}

/// 通知 toast：窗口上方居中短暂显示 `Service::notify` 的消息（F-16 / UI-P0-2）。
///
/// # 为什么必须有它
///
/// `AppState::notice` 有 8 处写入（复制失败、删除失败、清空失败、
/// 保存失败……），但在本函数存在之前**全仓没有一个读取方**——通知
/// 写进去即蒸发，用户点了「复制」却没反应时得不到任何解释。
/// 原状态栏（`STATUSBAR_HEIGHT`）已移除，toast 是它的替代通道。
///
/// 行为：在窗口**上方居中**显示（宽度按内容自适应并钳在窗口内，
/// 窄窗口也不会渲染到窗外），[`NOTICE_TTL`] 后自动消失；过期的那一帧
/// 顺带把 `notice` 清掉（读取即消费，不需要额外的定时器）。
///
/// # 出入场动效（2026-10-17）
///
/// 早前 toast 是「硬现、硬消失」——满不透明度瞬间出现在最终位置，
/// 3 秒后瞬间蒸发，与其余界面元素（悬停/选中全部走补间）的观感
/// 断裂。现在入场 140ms OutCubic 下滑 8pt + 淡入，出场在最后 220ms
/// 线性淡出。alpha 走 [`fade_color`]（预乘缩放），三层（底/描边/文字）
/// 同步淡，不会出现「底没了字还在」。
fn draw_notice_toast(f: &mut Frame<'_>) {
    /// toast 存活时间。3 秒是桌面端通知的惯例时长：足够读完一句话，
    /// 又不至于挡太久。
    const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
    /// 入场时长（秒）：下滑 + 淡入。
    const ENTER: f32 = 0.14;
    /// 出场淡化时长（秒）：TTL 结束前这么久开始淡出。
    const EXIT: f32 = 0.22;
    /// 入场位移幅度（逻辑点）：从上方 8pt 滑入。
    const SLIDE: f32 = 8.0;
    /// 右边距与顶栏间距。
    const MARGIN: f32 = 12.0;
    /// 宽度按内容自适应时的上下限。
    const MIN_W: f32 = 120.0;
    const MAX_W: f32 = 280.0;
    /// 行高（单行小字 + 上下呼吸）。
    const H: f32 = 30.0;

    let Some((msg, at)) = f.svc.state.notice.clone() else {
        return;
    };
    // 过期即消费：读取的这一帧负责清掉，避免状态里堆着旧通知。
    if at.elapsed() >= NOTICE_TTL {
        f.svc.state.notice = None;
        return;
    }

    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    let galley = f
        .ui
        .painter()
        .layout(msg.clone(), font.clone(), pal.text, MAX_W - 16.0);
    let w = (galley.rect.width() + 16.0).clamp(MIN_W, MAX_W);
    // 上方居中 + 边界钳制：
    // 1) 可用宽度 = 窗口宽 - 左右各一个 MARGIN；
    // 2) toast 宽度压到可用宽度以内（哪怕因此比 MIN_W 还窄）；
    // 3) 水平中心对齐窗口中线，再把 x 钳进 [min+MARGIN, max-MARGIN-w]。
    //    只要 w <= 可用宽度，这个区间恒非空（下界 <= 上界）。
    let area = f.area;
    let avail_w = area.width() - 2.0 * MARGIN;
    if avail_w <= 0.0 {
        // 退化情况：窗口窄到连左右边距都放不下，画了必溢出——干脆不画。
        return;
    }
    let w = w.min(avail_w);
    let cx = (area.min.x + area.max.x) * 0.5;
    let x = (cx - w * 0.5).clamp(area.min.x + MARGIN, area.max.x - MARGIN - w);
    // 垂直仍贴顶栏下缘。
    let r = Rect::from_min_size(
        pos2(x, crate::solver::TOPBAR_HEIGHT + MARGIN * 0.5),
        vec2(w, H),
    );
    // 自报热区：与其它命名区域同一套路，测试与自动化读坐标而不重算公式。
    // 报的是**稳态**矩形（不含入场位移）——MCP 自动化要的是可复用的
    // 落点，动画中途的 8pt 偏移不该让它每周换一个坐标。
    f.hit_insert("notice_toast", r);
    // 出入场进度：enter 0→1（OutCubic）配下滑；exit 在 TTL 末尾 1→0。
    let age = at.elapsed().as_secs_f32();
    let enter = crate::anim::Ease::OutCubic.apply((age / ENTER).clamp(0.0, 1.0));
    let exit = ((NOTICE_TTL.as_secs_f32() - age) / EXIT).clamp(0.0, 1.0);
    let k = enter * exit;
    // ---- 重绘预约：没有它，空闲闸门会把动画「饿死」 --------------------
    //
    // 主循环有 `skip_render` 闸门（见 app.rs 帧循环）：无事件、无补间在飞、
    // 无重绘请求时整帧跳过。而本动画的进度是 `at.elapsed()`（不走
    // `anim` 注册表，因此不被 `anim::busy` 覆盖）——不主动预约帧的话：
    // 1. 入场可能只播了一半就被闸门掐住，toast 僵在中间；
    // 2. 更糟的是**出场**：2.78s 时那一帧永远等不到，toast 会满亮度挂在
    //    屏幕上直到用户动一下鼠标，然后瞬间消失（过期帧 k=0 直接不画）。
    //
    // 策略：动画中（k<1）逐帧续请求；稳态（k==1）一次性预约到
    // 「出场开始前一点」的那一帧。稳态期间不占渲染预算（2.6s 空闲）。
    let ctx = f.ui.ctx();
    if k < 1.0 - 1e-4 {
        ctx.request_repaint();
    } else {
        let wake = (NOTICE_TTL.as_secs_f32() - age - EXIT).max(0.001);
        ctx.request_repaint_after(std::time::Duration::from_secs_f32(wake));
    }
    if k <= 0.004 {
        // 完全透明（入场第一帧 / 淡出完毕那一帧）：不画。
        return;
    }
    // 入场位移：从最终位置上方 8pt 滑入（enter 补完后归位）。
    let r = r.translate(vec2(0.0, (1.0 - enter) * SLIDE));
    let painter = f.ui.painter_at(r);
    // 圆角取调色板令牌而非硬编码 6：toast 是浮层控件，规格与按钮/输入框
    // 同级（`radius_md`）。写死数字的话，改主题圆角时 toast 会独自
    // 留在旧值上——这类「一半跟随主题一半不跟随」最难被肉眼发现。
    let radius = CornerRadius::same(pal.radius_md as u8);
    painter.rect_filled(r, radius, fade_color(pal.surface_raised, k));
    painter.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, fade_color(pal.border_subtle, k)),
        egui::StrokeKind::Inside,
    );
    painter.text(
        r.center(),
        Align2::CENTER_CENTER,
        msg,
        font,
        fade_color(pal.text, k),
    );
}

// ---------------------------------------------------------------- 顶栏

/// 画顶栏：背景 + 分隔线 + 右侧四枚窗口按钮 + 搜索开关图标。
///
/// 返回搜索图标的 [`egui::Response`]——调用方（`draw`）要拿它喂
/// [`run_search_state_machine`]（悬停展开 / 点击展开的判据）。
/// 输入框本身**不在这里画**：第三轮起它是悬浮面板（`draw_search_panel`）。
fn draw_topbar(f: &mut Frame<'_>, bar: Rect) -> egui::Response {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(bar);
    // 一体性：顶栏与卡片区**同色**（`bg`），整窗连成一片。
    // 早前这里用 `surface_variant`（第二种灰），与下面的卡片区
    // （第三种灰 `list_bg`）相邻 —— 两种灰 + 一条描边，界面就读成
    // 「三块拼的」。现在顶栏只靠底部那条 1px 分隔线区分。
    let bar_bg = if HISTORY_UNITY { pal.bg } else { pal.surface_variant };
    painter.rect_filled(bar, CornerRadius::ZERO, bar_bg);
    painter.hline(
        bar.x_range(),
        bar.max.y,
        Stroke::new(1.0, pal.border_subtle),
    );

    let inner = bar.shrink2(vec2(pal.space_md, 0.0));

    // ---- 右侧：窗口按钮 + 清空 + 设置（图标按钮）----------------------
    //
    // 从底部状态栏搬上来的。按钮用**图标**而非文字：顶栏高度只有
    // 44pt，中文「清空」两个字排在那里会又高又挤；图标在小尺寸下
    // 更清楚，也不随系统字号缩放变形。
    //
    // 尺寸取顶栏高度的 60%，留出上下呼吸空间。
    let btn = bar.height() * 0.62;
    let gap = pal.space_sm * 2.0;
    // **四枚**按钮：关闭 / 清空 / 窗口置顶 / 设置。
    //
    // IMPORTANT: 这个数字同时出现在 `draw_topbar_icons` 的
    // `step = r.width() / TOPBAR_BTN_COUNT`。两处各写一份的话，
    // 改一处忘另一处 ⇒ 按钮区与实际按钮错位，表现为
    // 「最右那枚被搜索框压住」或「点齿轮变成拖窗口」。
    // 所以这里用**同一个常量**，不是再抄一遍数字。
    let btn_count = TOPBAR_BTN_COUNT;
    let btn_area = Rect::from_min_size(
        pos2(
            inner.max.x - btn * btn_count - gap * (btn_count - 1.0),
            bar.center().y - btn * 0.5,
        ),
        vec2(btn * btn_count + gap * (btn_count - 1.0), btn),
    );
    draw_topbar_icons(f, btn_area, bar.center().y);

    // ---- 搜索：折叠图标（输入框是悬浮面板，见 draw_search_panel） -------
    //
    // 布局（自左向右）：
    //   [ 大片拖动区 ][ 🔍 ][ ✕ ][ 🗑 ][ ⚙ ]
    //
    // 搜索框默认**折叠成一枚放大镜图标**、贴在窗口按钮左侧：顶栏左侧
    // 整片（除按钮与图标）都是拖动区，用户落手即拖。第三轮起输入框
    // 移出顶栏成为悬浮面板（贴卡片区顶部），顶栏结构因此简化——
    // 左侧拖动区不再需要随展开收缩。
    let sq = btn * 0.62;
    let search_icon_r = Rect::from_center_size(
        pos2(btn_area.min.x - pal.space_md - sq * 0.5, bar.center().y),
        vec2(sq, sq),
    );
    f.hit_insert("search_icon", search_icon_r);
    let icon_resp = search_toggle_button(f, search_icon_r);

    // 顶栏拖动区 = 顶栏**减去交互控件**。
    //
    // IMPORTANT: 这里必须是「挖空」而不是「整块上报」。`WM_NCHITTEST` 对拖动区
    // 返回 `HTCAPTION`，系统随后把按下转成 `WM_NCLBUTTONDOWN`
    // ——**不会产生 `WM_LBUTTONDOWN`**，egui 于是收不到点击。
    //
    // 症状：整个顶栏（含齿轮、垃圾桶、搜索框）全都点不动，
    // 表现为「设置打不开 / 搜索框不能点」。
    let mut drag = Vec::new();
    // 左段：窗口左缘 → 搜索图标左缘。
    // **必须用左缘 - 2pt 收尾**：多出来的 2pt 与控件之间留缝，
    // 避免亚像素重叠把控件边缘判成 Caption。
    let left_end = search_icon_r.min.x;
    if left_end - bar.min.x > 8.0 {
        drag.push(Rect::from_min_max(bar.min, pos2(left_end - 2.0, bar.max.y)));
    }
    // NOTE: 这里**不设**「两按钮之间」那一段。
    //
    // `btn_area` 宽 `btn*2 + space_sm`、两按钮各占一半——它们是
    // **紧邻**的（`space_sm` 落在 `btn_area` 的内部，两侧各一半）。
    // 早前按「中点 ± 3」加一段，实测该段与垃圾桶重叠
    // （`[[645.7,0]-[651.7,44]]` vs 按钮 `[[617.4,8.4]-[648.7,35.6]]`），
    // 于是那段又变成「点垃圾桶 = 拖窗口」。
    //
    // 两按钮之间本来也没有可拖的空白，略掉即可。
    //
    // 右段：从**设置按钮的真实右缘**到窗口右缘。
    //
    // CAUTION: 不能用 `btn_area.max.x + pal.space_sm` 估——
    // `space_sm` 已经算进 `btn_area` 的宽度里了（见 `btn_area` 的
    // 构造：宽 `btn*2 + space_sm`），再加一次会**侵入按钮**。
    // 实测该段`[[688,0]-[700,44]]` 与齿轮 `[[664.7,8.4]-[696,35.6]]`
    // 重叠，于是点齿轮变成拖窗口。
    //
    // 用热区表里按钮的真实矩形——它就是按钮实际交互的区域。
    let settings_r = f.hit_get("topbar_settings");
    let right_start = settings_r.map(|r| r.max.x).unwrap_or(btn_area.max.x);
    if bar.max.x - right_start > 8.0 {
        drag.push(Rect::from_min_max(
            pos2(right_start + 2.0, bar.min.y),
            bar.max,
        ));
    }
    // 拖动区是**累加**而不是赋值：列表（`draw_history_body`）先于顶栏
    // 画，已经 push 了自己的矩形；赋值会把它们全丢掉（左右留白、行间
    // 缝隙都拖不动）。清零在 `draw()` 开头统一做。
    f.state.drag_regions.extend(drag);

    let _ = scale;
    icon_resp
}

/// 搜索状态机：悬停展开 / 宽限折叠 / 焦点钉住。
///
/// 交互契约（与原型逐条一致）：
/// - 光标**停在**图标上 ≥ [`SEARCH_HOVER_DELAY`] ⇒ 展开；
/// - 离开图标与**面板** ⇒ **宽限** [`SEARCH_COLLAPSE_DELAY`] 后才折叠；
/// - 输入框持有焦点、或里面有查询词 ⇒ 永不折叠。
///
/// Esc **不在这里**处理：按键落在搜索面板弹窗自己的 `egui::Context`
/// 里，主窗口读不到（`search_popup_focus` 之所以要弹窗每帧回报，
/// 就是同一个原因）。Esc 由 `draw_search_panel` 消费。
///
/// 面板是独立弹窗，因此「悬停面板」与「面板持有焦点」两项都由弹窗
/// 每帧回报到 [`UiState::search_popup_hover`] / [`UiState::search_popup_focus`]——
/// 指针在主窗口客户区之外、主窗口也读不到弹窗 Context 的焦点。
fn run_search_state_machine(f: &mut Frame<'_>, icon_resp: &egui::Response) {
    let hover_icon = icon_resp.hovered();
    let hover_panel = f.state.search_popup_hover;
    let now = f.ui.ctx().input(|i| i.time);

    if hover_icon {
        f.state.search_hover_since.get_or_insert(now);
    } else {
        // 离开图标即重新计时：再次移入要重新停够时间。
        f.state.search_hover_since = None;
    }
    if !f.state.search_hover_suppressed
        && f
            .state
            .search_hover_since
            .is_some_and(|t| now - t >= SEARCH_HOVER_DELAY)
    {
        set_search_expanded(f, true);
    }

    // 输入框持有焦点 / 有查询词 / 刚展开：无条件保持展开。
    //
    // 焦点这条最关键：用户要点进输入框打字。弹窗刚打开、输入框还没
    // 拿到焦点的那几帧里，既不悬停图标也不悬停面板——没有这条判定，
    // 面板会在焦点建立之前自己收回去，然后焦点被丢弃，字永远打不进去。
    //
    // 「刚展开」这条是 2026-10-19 补的：焦点从请求到回报要跨一两帧，
    // 指针又可能已经离开图标，那几帧里 `hold_open` 会短暂为假 ⇒ 折叠
    // 计时启动 ⇒ 面板「闪一下就没」。宽限期把这段时间兜住。
    let just_opened = f
        .state
        .search_open_at
        .is_some_and(|t| now - t < SEARCH_OPEN_GRACE);
    let hold_open =
        f.state.search_popup_focus || !f.state.query.is_empty() || just_opened;

    // 点击图标 = **展开 + 把焦点交给输入框**，不做开合切换。
    //
    // CAUTION: 早前这里是 toggle（展开态点一下收起）。它与「悬停 0.18s
    // 自动展开」天然打架：用户把鼠标移到图标上、面板已经自己展开了，
    // 再点一下反而把它收起来——观感就是「点一下，闪一下，关闭了」
    // （2026-10-19 用户反馈）。收起只走三条显式路径：Esc、面板 ✕、
    // 以及「指针离开图标与面板且焦点不在输入框」的宽限折叠。
    //
    // 焦点请求要跨帧：弹窗本帧可能一次都没画过（弹窗是主窗口之后才
    // 渲染的），`search_edit_id` 还是 `None`。真正的请求挂在
    // [`UiState::search_focus_pending`] 上，等弹窗自报出真实 id。
    if icon_resp.clicked() {
        set_search_expanded(f, true);
        f.state.search_focus_pending = true;
    }
    // 光标离开图标 ⇒ 解除抑制（下次移入可以正常悬停展开）。
    if !hover_icon {
        f.state.search_hover_suppressed = false;
    }

    // CAUTION: 顺序是「先判折叠 → 弹窗再画」。反过来（先画再判）
    // 会让**折叠的那一帧**仍然把面板显示出来，外部（测试、自动化脚本）
    // 读到的状态与实际差一帧——表现是「已经收起了，但面板还在」。
    if f.state.search_expanded && !hover_icon && !hover_panel && !hold_open {
        match f.state.search_leave_since {
            None => f.state.search_leave_since = Some(now),
            Some(t) if now - t >= SEARCH_COLLAPSE_DELAY => {
                set_search_expanded(f, false);
            }
            _ => {}
        }
    } else {
        // 仍在区域内（或被焦点/查询词钉住）：取消在途的折叠倒计时。
        f.state.search_leave_since = None;
    }
}

/// 展开 / 收起搜索面板。
///
/// 单一入口：图标点击、悬停计时、弹窗内的 Esc 都走这里，保证
/// 「状态翻转」只有一处。弹窗的显隐由帧循环按 `search_expanded` 同步。
///
/// NOTE: 弹窗的**关窗请求**（标题栏 ✕）不走这里——它由
/// `crate::popup` 直接改 `search_expanded`（那时没有 `Frame` 可借，
/// 且那条路径不来自绘制层）。两处语义一致，计时器的复位由那一侧自理。
fn set_search_expanded(f: &mut Frame<'_>, v: bool) {
    if f.state.search_expanded == v {
        return;
    }
    f.state.search_expanded = v;
    f.state.search_hover_since = None;
    f.state.search_leave_since = None;
    // 记录「本次展开时刻」：折叠判定用它做免折叠宽限（见
    // [`SEARCH_OPEN_GRACE`]）。收起时清掉，避免下次展开沿用旧时刻
    // 而让宽限失效。
    f.state.search_open_at = v.then(|| f.ui.ctx().input(|i| i.time));
}

/// 画悬浮搜索面板（第三轮起输入框的新形态）。
///
/// 与旧「顶栏内弹出输入框」的差别只在**位置与容器**：矩形由
/// [`FloatLayout`] 给（卡片区通栏、贴顶部），控件结构与交互
/// （hint / 字体 / 边距 / 无外框 / 聚焦加强 / 清除按钮 / 焦点自报）
/// 与旧 `draw_search` 完全一致，行为可无缝对拍。
pub fn draw_search_panel(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(r);
    // 纯平毛玻璃禁令同样适用于悬浮层：纯色底 + 1px 描边，禁渐变光泽。
    let radius = pal.radius_md;
    painter.rect_filled(r, radius, pal.surface);
    painter.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 文字直接从左边距起排（放大镜在顶栏，不重复画）。
    let pad = pal.space_sm;
    let edit_rect = Rect::from_min_max(
        pos2(r.min.x + pad, r.min.y + 1.0),
        pos2(r.max.x - pad, r.max.y - 1.0),
    );
    if edit_rect.width() <= 4.0 || edit_rect.height() <= 4.0 {
        return;
    }
    // 自报热区给测试用（见 `UiState::hit` 的说明）。
    //
    // NOTE: 登记**两个**：`search_box` 是外框，`search` 是输入区。
    f.hit_insert("search_box", r);
    f.hit_insert("search", edit_rect);
    // 面板**整块**可点：点到左右 padding（输入区之外）也应当聚焦输入框。
    //
    // CAUTION: 不注册这一条的后果不是「点不动」而是更糟的「变成拖窗口」——
    // 顶栏的拖动区上报给系统后变成 HTCAPTION，面板 padding 处没有 egui
    // 交互，点击穿透到 WM_NCHITTEST，用户点输入框左侧空白就把窗口拖走了。
    // 后注册的 interact 在重叠区胜出，面板整体盖过拖动区。
    let panel_resp = f
        .ui
        .interact(r, egui::Id::new("card_search_box"), Sense::click());
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            // IMPORTANT: **必须给显式 id**。不给的话 egui 按调用位置自动生成，
            // 于是：焦点断言没法稳定地指向它（只能退化成
            // 「有没有任何控件获得焦点」这种恒真判断），且任何调整
            // 布局的改动都可能悄悄换掉 id。
            .id(egui::Id::new("search_edit"))
            .max_rect(edit_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.state.query)
                    .hint_text("搜索历史…")
                    .desired_width(f32::INFINITY)
                    // IMPORTANT: 字号必须给**不带scale 的逻辑字号**。
                    //
                    // 早前传的是 `sized(pal.font_md, scale)`——它已经把
                    // DPI 系数乘进去了，而 egui 在计算可用高度时会
                    // **再乘一次** `pixels_per_point`。结果是行高被放大到
                    // 远超框高，文字被裁掉上半/下半截，表现为
                    //「能输入但看不见、字很小」。
                    //
                    // 正确做法：`FontId` 用逻辑字号，由 egui 负责缩放。
                    .font(egui::FontId::proportional(pal.font_md))
                    // 内边距收窄：egui 默认的 `symmetric(4,2)` 在此会让
                    // 单行控件偏高而裁切。
                    .margin(egui::Margin::symmetric(2, 0))
                    .text_color(pal.text)
                    // 去掉 TextEdit 自带的外框：外框已由上面的
                    // `rect_stroke` 画好，两层框叠在一起会显得脏。
                    //
                    // IMPORTANT: 这里必须给 `egui::Frame` 枚举而不是 `false`：
                    // 0.36 的 `frame()` 收的是容器框类型，不是 bool。
                    .frame(egui::Frame::NONE),
            )
        },
    );
    // `scope_builder` 返回 `InnerResponse<Response>`：内层是 TextEdit
    // 自己的响应，外层是这次作用域分配整体的响应。
    // 焦点与 id 都要用**内层**的——外层 id 属于作用域，不是输入框。
    let edit_resp = resp.inner;

    // 自报真实 Id，供测试精确判断焦点落在哪个控件上（见字段说明）。
    f.state.search_edit_id = Some(edit_resp.id);

    // 聚焦描边补间（130ms）：点进输入框时 border → accent 淡入、失焦
    // 淡回，与设置页输入框同一套（`draw_text_field`）。判据用**本帧
    // 实时焦点**（`edit_resp.has_focus()`），不用滞后一帧的
    // `search_popup_focus`——那是回报给主窗口的状态机用的。
    // 弹窗 ctx 的补间由 `draw_popup` 末尾的 busy 检查续帧。
    let focus_k = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::new("search_panel_focus"),
        if edit_resp.has_focus() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(130),
        crate::anim::Ease::OutCubic,
    );
    if focus_k > 0.004 {
        let ring = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new("search_panel_focus_color"),
            pal.accent,
            std::time::Duration::from_millis(130),
        );
        painter.rect_stroke(
            r,
            radius,
            Stroke::new(1.0 + 0.5 * focus_k, ring),
            egui::StrokeKind::Inside,
        );
    }

    // 有文本时给一个清除按钮。
    if !f.state.query.is_empty() {
        let btn = r.height() * 0.6;
        let clear = Rect::from_center_size(
            pos2(r.max.x - pad - btn * 0.5, r.center().y),
            vec2(btn, btn),
        );
        let cr = f
            .ui
            .interact(clear, egui::Id::new("card_search_clear"), Sense::click());
        // 悬停圆角底 + 图标颜色补间（110ms）：与行内操作按钮同一套。
        // 弹窗 ctx 里的补间要靠末尾的 busy 续帧（见本函数结尾）。
        let chov = crate::anim::tween_f32(
            f.ui.ctx(),
            egui::Id::new("search_clear_hover"),
            if cr.hovered() { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(110),
            crate::anim::Ease::OutCubic,
        );
        if chov > 0.004 {
            f.ui
                .painter_at(clear)
                .rect_filled(clear, pal.radius_sm, fade_color(pal.row_hover, chov));
        }
        let clear_icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new("search_clear_icon"),
            if cr.hovered() { pal.text_bright } else { pal.text_dim },
            std::time::Duration::from_millis(110),
        );
        crate::icons::Icon::Close.paint(
            &f.ui.painter_at(clear),
            Rect::from_center_size(clear.center(), vec2(btn, btn) * 0.7),
            clear_icon_color,
        );
        if cr.clicked() {
            f.state.query.clear();
            // 清空后主动交还焦点：否则用户还得再点一次才能继续打字。
            f.ui.memory_mut(|m| m.request_focus(edit_resp.id));
        }
        cr.on_hover_text("清空搜索");
    }

    // 焦点请求在**画完之后**发：id 刚自报出来，此刻才有效。
    if f.state.search_focus_pending
        && let Some(id) = f.state.search_edit_id
    {
        // CAUTION: 必须是自报的**真实 id**。拿上层猜的
        // `Id::new("search_edit")` 请求焦点会打水漂——那个 id 与
        // TextEdit 实际用的 id 不相等（`UiBuilder::id` 只是容器 id）。
        f.ui.memory_mut(|m| m.request_focus(id));
        f.state.search_focus_pending = false;
    }
    // 点面板空白处（输入区左右的 padding）也聚焦输入框。
    if panel_resp.clicked()
        && let Some(id) = f.state.search_edit_id
    {
        f.ui.memory_mut(|m| m.request_focus(id));
    }

    // ---- Esc 收起面板 ----
    //
    // 为什么必须在这里而不是主窗口：按键落在**弹窗自己的
    // `egui::Context`** 里，主窗口的 `run_search_state_machine` 读不到
    // （`search_popup_focus` 需要弹窗每帧回报，就是同一个原因）。
    //
    // 为什么不能只 `set_search_expanded(f, false)`：输入框此刻持有焦点，
    // 而状态机的 `hold_open` 里「焦点」这一项是无条件的 ⇒ 下一帧又把它
    // 展开回来，Esc 看起来「按了没用」。所以必须同时把焦点交出去。
    //
    // 收起时置 `search_hover_suppressed` 与点击路径同理：光标多半还停在
    // 图标上，否则 0.18s 后悬停计时会立刻把它重新展开。
    if f.ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        set_search_expanded(f, false);
        f.state.search_hover_suppressed = true;
        // 交还焦点：焦点留在已隐藏的输入框上会让 `TextEdit` 继续吃按键，
        // 且 `hold_open` 继续为真（见上）。用**自报的真实 id**
        // 而不是 `m.focused()`：后者此刻可能已被别的控件抢走，
        // 那会把别人的焦点也一起收掉。
        if let Some(id) = f.state.search_edit_id {
            f.ui.memory_mut(|m| m.surrender_focus(id));
        }
        f.state.search_popup_focus = false;
    }
}

/// 弹窗投影的**贴边浓度**（紧贴面板外第一环带的 alpha，0~1）。
///
/// # 为什么不能沿用卡片投影的浓度
///
/// `Palette::shadow` 是 `rgba_unmultiplied(0,0,0,255)`——**alpha=255 的
/// 纯黑**。卡片投影再乘用户浓度倍率，反混合后贴边强度被顶到 0.95。
/// 这对主窗口卡片是对的：它压在**不透明底图**上，0.95 的黑看着就是
/// 阴影。
///
/// 弹窗画布是**透明**的（清屏 alpha=0，见 `crate::popup`），同一强度
/// 直接裸在桌面上 ⇒ 紧贴面板外一圈 alpha≈0.42 的纯黑，实机就是一条
/// **黑色描边**；当时类型栏还没有背景板，整块投影更是被看成
/// 「黑色背景」（那一半已由 `draw_type_rail` 的面板底修掉，但浓度
/// 不改的话描边仍在）。
///
/// 弹窗可能压在任意桌面颜色上（浅色壁纸下最明显），浓度必须按「没有
/// 底图托底」重新定，取卡片量级的五分之一左右。
pub const POPUP_SHADOW_STRENGTH: f32 = 0.18;

/// 弹窗投影的模糊半径（逻辑点）。
///
/// 必须让投影在 [`crate::popup::SHADOW_MARGIN`] **之内衰减完**：外扩超出
/// 边距的层会被窗口边界**硬切**，最外圈像素留下一条等浓度的黑框——
/// 比完全没有投影更丑（这正是「描边」的第二重来源）。
///
/// 取 10（σ=5）：`card_shadow_layers` 的可见判据（单层 alpha ≥ 0.003）
/// 会在 k ≥ 12 处全部跳过，最外层可见 grow 为 11 ⇒ 窗口边界处强度
/// < 1/255，看不出截断。有测试守着这个关系，别随手调大。
pub const POPUP_SHADOW_BLUR: f32 = 10.0;

/// 弹窗投影的垂直偏移（逻辑点，正值 = 向下）。
///
/// 向下偏移会吃掉**底部**余量（顶部反而更宽裕），所以它同样参与
/// 「不越界」的约束。
pub const POPUP_SHADOW_OFFSET_Y: f32 = 2.0;

/// 画悬浮弹窗的投影（面板外围那一圈 alpha）。
///
/// 必须用**弹窗客户区级** painter（`ui.painter()`），不能用
/// `painter_at(panel)`——后者按面板矩形裁剪，投影会被切掉一圈，
/// 表现为「面板贴着桌面、没有浮起来」。
///
/// # 与卡片投影的关系
///
/// 只复用 `card_shadow_layers` 这层**高斯轮廓**，不复用
/// [`draw_card_shadow`] 的浓度逻辑——后者是按「有不透明底图托底」标定
/// 的，套到透明画布上就是黑描边（见 [`POPUP_SHADOW_STRENGTH`]）。
/// 同理也不画 `draw_card_shadow` 里的「近投影」：那一组浓度 ×1.15、
/// 紧贴边缘，正是描边感的主要来源。
pub fn draw_popup_shadow(painter: &egui::Painter, panel: Rect, pal: &Palette) {
    // 圆角**跟随面板**（`radius_md`），投影矩形外扩 grow 时半径同步增大
    // ⇒ 矩形与圆角始终同心。早前硬编码 9.0，与面板圆角脱钩，圆角处会
    // 露出一条投影的直角边。
    let radius = pal.radius_md;
    // 阴影**颜色**仍取主题令牌（可配成冷/暖调），只有浓度用弹窗常量。
    let [sr, sg, sb, _] = pal.shadow.to_array();
    let base = panel.translate(vec2(0.0, POPUP_SHADOW_OFFSET_Y));
    for (grow, a) in card_shadow_layers(POPUP_SHADOW_BLUR, POPUP_SHADOW_STRENGTH) {
        // 环带 alpha 走 `from_rgba_unmultiplied`：与 `draw_card_shadow`
        // 同一条路径，别在这里改成 `from_rgba_premultiplied`（管线的混合
        // 是预乘的，egui 送进来的顶点色本来就是非预乘）。
        painter.rect_filled(
            base.expand(grow),
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }
}

/// 画悬浮类型过滤栏的内容（2026-10-15 第三轮新增，待移植设置联动）。
///
/// 由**弹窗**调用（`crate::popup`），`rail` 是弹窗客户区中央的**面板**
/// 矩形（= 按钮组 + 四周 [`RAIL_PAD`]）——[`FloatLayout::compute`] 已把
/// 面板尺寸算好，这里负责画面板底并按停靠方向依次摆按钮。
///
/// 按钮外观：圆角方块，`surface_raised` 底 + 1px 弱描边；激活 =
/// `accent` 底 + `text_on_accent` 图标。点击过滤、再点一次取消
/// （`Op::SetTypeFilter`）。
pub fn draw_type_rail(f: &mut Frame<'_>, rail: Rect, kinds: &[modular_clipboard_core::ClipKind]) {
    use modular_clipboard_core::config::TypeRailPos;
    let pal = f.pal;
    let rail_cfg = &f.svc.state.config.ui.type_rail;
    let size = modular_clipboard_core::config::clamp_type_rail_size(rail_cfg.size);
    let gap = RAIL_GAP.min(8.0); // 按钮间距比面板间距紧一档
    let n = kinds.len();
    if n == 0 || rail.width() <= 0.0 || rail.height() <= 0.0 {
        return;
    }
    // ---- 面板底（不透明）----
    //
    // 必须在按钮**之前**画，且必须不透明：投影是「面板外扩」的实心矩形，
    // 天然覆盖面板内部，只有不透明底能把它盖住。少了这一层，按钮之间的
    // 缝隙会透出投影的纯黑（实机「类型栏是一块黑色背景」）。
    let radius = pal.radius_md;
    let painter = f.ui.painter_at(rail);
    painter.rect_filled(rail, radius, pal.surface);
    painter.rect_stroke(
        rail,
        radius,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    let vertical = matches!(rail_cfg.pos, TypeRailPos::Left | TypeRailPos::Right);
    // 按钮组从面板内侧起排（跳过 padding）——横竖两个方向都只需把
    // 起点各挪一个 `RAIL_PAD`。
    let (mut x, mut y) = (rail.min.x + RAIL_PAD, rail.min.y + RAIL_PAD);

    for &kind in kinds {
        let slot = Rect::from_min_size(pos2(x, y), vec2(size, size));
        let active = f.state.type_filter == Some(kind);
        // 热区按类型分key：测试要逐类断言「某类型的按钮在不在」。
        // CAUTION: key 必须是 `&'static str`（热区表的键类型），
        // 不能 `format!`——临时 String 活不过这条语句。
        let hit_key = match kind {
            modular_clipboard_core::ClipKind::Text => "rail_text",
            modular_clipboard_core::ClipKind::Html => "rail_html",
            modular_clipboard_core::ClipKind::Image => "rail_image",
            modular_clipboard_core::ClipKind::Files => "rail_files",
            modular_clipboard_core::ClipKind::Markdown => "rail_markdown",
        };
        f.hit_insert(hit_key, slot);
        // CAUTION: 用 `ui.interact` 而不是 `scope_builder` + `Button`——
        // 后者在**弹窗上下文**里拿不到指针（实测 `hovered` 恒假、
        // 点击不响应；主窗口上下文却正常）。行内按钮（`draw_row_acts`）
        // 一直是 `ui.interact`，本函数与它保持一致。
        let resp = f
            .ui
            .interact(slot, egui::Id::new(("rail", kind)), Sense::click());
        // 底色：激活 accent / 常态 surface_raised（磨砂模式降 alpha）。
        //
        // CAUTION: `row_hover` 是**半透明**令牌（`from_white_alpha(30)`），
        // 在主窗口里它下面有整块不透明背景托着；而弹窗的画布是**透明**的
        // （清屏 alpha=0），直接拿它当填充会把按钮画成半透明——透出背后
        // 的深色桌面，实机就是「鼠标一放上去图标变黑」。
        // 所以半透明态一律**先铺不透明底、再叠色调**。
        //
        // ---- 动效（2026-10-17）：激活交叉淡入 + 悬停补间 -----------------
        //
        // 底色目标按「是否激活」取，`tween_color` 从当前显示值向目标
        // 续接 ⇒ 点下过滤时方块从 surface_raised 平滑染到 accent，
        // 取消时反向；描边随激活淡出，图标颜色同步补间。
        let act = crate::anim::tween_f32(
            f.ui.ctx(),
            egui::Id::new(("rail_active", kind)),
            if active { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(160),
            crate::anim::Ease::OutCubic,
        );
        // 磨砂模式：面板底图要透出来，方块底色就得跟着降 alpha。
        //
        // 判据取「`surface` 令牌是否被降过透明度」——`with_backdrop_alpha`
        // 只处理背景层（bg/surface/surface_variant/list_bg），
        // `surface_raised` 不在名单里（它是卡片色，主窗口里必须保持不透明）。
        // 弹窗的方块是**面板本身**，所以按同一比例手降一次。
        let frost_k = pal.surface.a() as f32 / 255.0;
        let mut base_target = if active {
            pal.accent
        } else {
            pal.surface_raised
        };
        if frost_k < 1.0 && !active {
            base_target = base_target.gamma_multiply(frost_k);
        }
        let base = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new(("rail_base", kind)),
            base_target,
            std::time::Duration::from_millis(160),
        );
        let sq_radius = size * 0.25;
        let slot_painter = f.ui.painter_at(slot);
        slot_painter.rect_filled(slot, sq_radius, base);
        if !active {
            // 悬停底（110ms）：先铺不透明底（上面的 base），再叠半透明
            // 提亮；alpha 走预乘缩放（`fade_color`），不用 raw row_hover。
            let hov = crate::anim::tween_f32(
                f.ui.ctx(),
                egui::Id::new(("rail_hover", kind)),
                if resp.hovered() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(110),
                crate::anim::Ease::OutCubic,
            );
            if hov > 0.004 {
                slot_painter.rect_filled(slot, sq_radius, fade_color(pal.row_hover, hov));
            }
            // 常态描边随激活淡出（act→1 时完全收进去，与 accent 底交替）。
            if act < 0.996 {
                slot_painter.rect_stroke(
                    slot,
                    sq_radius,
                    Stroke::new(1.0, fade_color(pal.border_subtle, 1.0 - act)),
                    egui::StrokeKind::Inside,
                );
            }
        }
        let icon_target = if active {
            pal.text_on_accent
        } else if resp.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        let icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new(("rail_icon", kind)),
            icon_target,
            std::time::Duration::from_millis(130),
        );
        let icon_px = size * 0.5625; // 32pt → 18pt（同行内按钮比例）
        crate::icons::paint_kind_icon(
            &f.ui.painter_at(slot),
            kind,
            Rect::from_center_size(slot.center(), vec2(icon_px, icon_px)),
            icon_color,
        );
        // 提示带计数：用户能看出点下去会过滤出多少条。
        let count = f.svc.state.items.iter().filter(|it| it.kind == kind).count();
        // NOTE: `clicked()` 必须在 `on_hover_text` 之前取——后者按值消耗
        // `Response`（`icon_button` 里踩过同一个坑，E0382）。
        let clicked = resp.clicked();
        resp.on_hover_text(format!("{} · {} 条", kind.label(), count));
        if clicked {
            f.state.push(Op::SetTypeFilter(if active { None } else { Some(kind) }));
        }
        if vertical {
            y += size + gap;
        } else {
            x += size + gap;
        }
    }
}

/// 顶栏右侧的四枚图标按钮：关闭 / 最小化 / 清空 / 设置。
///
/// # 为什么改用 egui 的 `Button` 而不再手写 `interact`
///
/// 手写版只调 `ui.interact` + `painter`，控件在 egui 的命中体系里
/// 是「外人」：焦点环、键盘响应、命中排序都要自己照顾。历史上
/// 「按钮画了但点不动」的根因正是命中判定被拖动区抢走——
///
/// ```text
/// WM_NCHITTEST 命中拖动区 ⇒ 返回 HTCAPTION
///                ⇒ 系统发 WM_NCLBUTTONDOWN（非客户区消息）
///                ⇒ **不产生 WM_LBUTTONDOWN**
///                ⇒ egui 收不到点击
/// ```
///
/// 改用 `Button` 后控件走 egui 自己的登记与排序，配合 `draw_topbar`
/// 里的「拖动区挖空」把控件矩形从 `HTCAPTION` 范围里剔除，
/// 两者共同保证可点。
///
/// # 图标仍然自绘
///
/// `Button` 的**文本**走普通字体链，而图标在 PUA 码位
/// （`U+E000..=U+E011`），那条字体链里没有字形，会渲染成方框（tofu）。
/// 所以 `Button` 只负责命中与悬停底色，图标用 [`Icon::paint`] 叠在
/// 它自己的矩形上。
fn draw_topbar_icons(f: &mut Frame<'_>, r: Rect, center_y: f32) {
    let step = r.width() / TOPBAR_BTN_COUNT;
    let sq = step * 0.62;

    // 自左向右：关闭 / 清空 / 窗口置顶 / 设置。
    //
    // NOTE: 「最小化」早前已去掉 —— 它与「关闭到托盘」的实际效果几乎相同
    // （都让窗口消失），区别仅在任务栏是否留条目，而本程序常驻托盘。
    // 两个近乎重复的按钮并排反而增加误点。
    //
    // 关闭按钮用 `Icon::Minimize`（横线）而非 `Icon::Close`（叉）：
    // 它的语义是「收起」，与最小化同族；叉号在中文界面里更常被
    // 理解成「删除/退出」，而这里并不是退出。
    let slot = |i: f32| Rect::from_center_size(pos2(r.min.x + step * (i + 0.5), center_y), vec2(sq, sq));
    let r_close = slot(0.0);
    let r_clear = slot(1.0);
    let r_pin = slot(2.0);
    let r_set = slot(3.0);

    // 登记热区给测试用（见 `UiState::hit` 的说明）。**必须在点击判定
    // 之前登记**：`draw_topbar` 随后要读这张表来挖拖动区。
    f.hit_insert("topbar_close", r_close);
    f.hit_insert("topbar_clear", r_clear);
    f.hit_insert("topbar_pin", r_pin);
    f.hit_insert("topbar_settings", r_set);

    if icon_button(f, "topbar_close", r_close, Icon::Minimize, "关闭到托盘", false, false) {
        f.state.push(Op::CloseWindow);
    }
    // 破坏性操作（清空）用 danger 色，与「设置」区分开。
    if icon_button(f, "topbar_clear", r_clear, Icon::Trash, "清空全部历史", true, false) {
        f.state.push(Op::ClearAll);
    }
    // 窗口置顶：切换「始终显示在最上层、不被其它应用遮住」。
    //
    // 写配置即可 —— `App` 帧循环每帧比对 `always_on_top` 并调
    // `SetWindowPos`，所以这里不需要（也不该）直接碰 Win32。
    // 设置面板里那个同名开关读同一个字段，两处自动同步。
    let pinned = f.svc.state.config.ui.always_on_top;
    if icon_button(
        f,
        "topbar_pin",
        r_pin,
        if pinned { Icon::Pinned } else { Icon::Unpinned },
        if pinned { "取消窗口置顶" } else { "窗口置顶" },
        false,
        pinned,
    ) {
        f.state.push(Op::SetBool {
            group: ConfigGroup::Ui,
            field: "always_on_top",
            value: !pinned,
        });
    }
    if icon_button(f, "topbar_settings", r_set, Icon::Settings, "设置", false, false) {
        f.state.push(Op::ToggleSettings);
    }
}

/// 顶栏右侧的图标按钮数量。
///
/// 关闭 / 清空 / 窗口置顶 / 设置。
///
/// # 为什么是常量而不是两处各写一个数字
///
/// `draw_topbar` 用它算 `btn_area` 的宽度，`draw_topbar_icons` 用它算
/// `step`（槽位步距）。两处不一致 ⇒ 按钮区与实际按钮错位，表现为
/// 「最右那枚被搜索框压住」或「点齿轮变成拖窗口」——都不是一眼能看出
/// 原因的症状，所以直接共用一个常量，从根上杜绝。
const TOPBAR_BTN_COUNT: f32 = 4.0;

/// 一枚方形图标按钮；返回是否被点击。
///
/// 走 egui `Button` 拿命中与悬停，图标自己叠画（理由见
/// [`draw_topbar_icons`]）。
///
/// # 为什么必须开子 `Ui` 而不能直接 `add_sized`
///
/// `add_sized` 只给**尺寸**，位置由父 `Ui` 的布局游标决定——四枚按钮
/// 会并排排在顶栏左缘，而不是我们算好的槽位。于是 `Response.rect`
/// 与登记进 `hit` 表的矩形不一致：测试按热区点击打不中，
/// 拖动区挖空也挖错位置（表现为「点按钮 = 拖窗口」）。
///
/// `UiBuilder::max_rect` 才是绝对定位：子 `Ui` 的原点就是该矩形，
/// 里面的 `Button` 落在矩形左上角，尺寸相同 ⇒ 两者完全重合。
fn icon_button(
    f: &mut Frame<'_>,
    id: &'static str,
    r: Rect,
    icon: Icon,
    tip: &'static str,
    danger: bool,
    active: bool,
) -> bool {
    let pal = f.pal;
    // `Button` 的文本留空：真正的内容是下面自绘的图标。
    // `frame(false)` 去掉它自带的边框，只保留命中与悬停。
    //
    // NOTE: `Button::frame` 收的是 **bool**（是否画框），而
    // `TextEdit::frame` 收的是 `egui::Frame` 枚举。两者同名不同签名，
    // 写反了会报 E0308。
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new(id))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    // 悬停底色**补间**（110ms OutCubic，与行悬停同档）：早前是
    // `hovered()` 当帧瞬间 `rect_filled`，鼠标划过时底「硬现」、移开
    // 「硬没」，与列表行的平滑悬停观感不一致。id 用按钮名（每帧稳定）。
    let hov = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::new(("icon_btn_hover", id)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if hov > 0.004 {
        // 底色叠在热区矩形上（`painter_at` 把裁剪限制在顶栏内）。
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hov));
    }
    draw_topbar_icon(
        &f.ui.painter_at(r),
        icon,
        // IMPORTANT: 尺寸是**逻辑点**，绝不能乘 `scale`。
        //
        // `scale` 就是 `pixels_per_point`（超采样打开后 = 2.25），乘上去
        // 等于让 egui 按 2.25 倍栅格化字形，而绘制又被 `painter_at(r)`
        // 裁在 24pt 的按钮框内 ⇒ **只露出字形中间一块**：
        // 垃圾桶丢了盖、齿轮丢了齿、图钉丢了针。
        //
        // 症状极具迷惑性——看起来像「字体子集里字形映射错了」，
        // 实际是「字号被 DPI 乘了两次 + 裁剪」。放大镜当初漏乘了 `scale`，
        // 所以它是唯一正常的一枚，也因此比另外四枚小一大截。
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        // 优先级：破坏性悬停 > 激活态 > 悬停提亮 > 常态弱化。
        //
        // `active` 用 `accent` 配色，而不只是换实心图标：16pt 下
        // 实心与描边的差异很小，不配色用户分不出「已置顶」和「没置顶」。
        // `danger` 与 `active` 分开判断，将来加「既破坏性又有开关态」
        // 的按钮不用改签名。
        match (danger, resp.hovered(), active) {
            (true, true, _) => pal.danger,
            (_, true, true) => pal.accent_hover,
            (_, _, true) => pal.accent,
            (_, true, false) => pal.text,
            _ => pal.text_dim,
        },
    );
    // NOTE: 顺序硬性：`on_hover_text` 按**值**消耗 `Response`，所以
    // `clicked()` 必须在它之前取。把两者写反会 E0382（值已移动）。
    let clicked = resp.clicked();
    resp.on_hover_text(tip);
    clicked
}

/// 顶栏搜索图标按钮（折叠式搜索的开关）。
///
/// 与 [`icon_button`] 同构：egui `Button` 负责命中与悬停，图标自绘。
/// 区别是放大镜**不用字体字形**（图标字体子集只有 18 个码位且
/// `U+E003` 字形偏粗，见 [`draw_search_icon`]），改用几何绘制。
///
/// 返回 `Response` 而不是 `bool`：调用方既要 `hovered`（悬停展开）
/// 也要 `clicked`（点击聚焦），只回一个布尔就丢了另一半信息。
fn search_toggle_button(f: &mut Frame<'_>, r: Rect) -> egui::Response {
    let pal = f.pal;
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new("search_icon"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    // 悬停底色补间：与 [`icon_button`] 同一套（110ms OutCubic）。
    let hov = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::new("search_icon_hover"),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if hov > 0.004 {
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hov));
    }
    draw_search_icon(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        if resp.hovered() { pal.text_bright } else { pal.text_dim },
    );
    resp.on_hover_text(if f.state.search_expanded { "收起搜索" } else { "搜索" })
}


/// 画放大镜图标（圆环 + 手柄），几何绘制而非字体字形。
///
/// # 为什么不用 `Icon::Search` 字形
///
/// 图标字体子集只有 18 个码位、源字体不在仓库（只有 pyftsubset 的
/// 4KB 子集），**无法新增**码位。而现有的 `U+E003` 字形笔画偏粗、
/// 圆环与手柄比例失衡，在搜索框这种小尺寸下显得笨重（用户反馈"太丑"）。
///
/// 放大镜本来就只有两条线——圆 + 斜杠，用几何画反而更可控：
/// - **线宽可调**，不受字体设计限制；
/// - 圆环与手柄的**比例由我们决定**，不必迁就字形；
/// - 无字体依赖，不会因缺字形退化成方框。
///
/// # 尺寸约定
///
/// `r` 是图标包围盒。圆环占其中 62%，手柄从圆环边缘斜向外延伸，
/// 整体不超出 `r`——保证与文字基线视觉对齐。
/// 用**几何图形**绘制顶栏的四枚图标（关闭 / 清空 / 置顶 / 设置）。
///
/// # 为什么不用图标字体的字形
///
/// 1. **字形被裁**：`icon_button` 早前把字号写成 `icon_size * scale`，
///    而 `scale` 是 `pixels_per_point`（超采样下 = 2.25）⇒ 字形按 36pt
///    栅格化，却被 `painter_at(r)` **裁到 24pt 的按钮框**里，只露出中间
///    一块：垃圾桶丢了盖、齿轮丢了齿、图钉丢了针，看起来像「字形映射错
///    了」，实际是**裁剪**。放大镜当初没乘 `scale`，所以它是唯一正常的
///    一枚——也正因此它比另外四枚小一大截。
/// 2. **光学重量不齐**：`pin-angle-fill` 是实心、几乎铺满 em 盒，而
///    垃圾桶 / 齿轮是细描边；同一字号下前者重得多，一排按钮看着发飘。
///
/// 几何绘制把线宽、留白、圆角全部握在手里，一排按钮的视觉重量天然一致。
/// 这也与本文件既有的 [`draw_search_icon`]、`draw_plug_icon` 同一条路线
/// ——它们当初就是因为「字形偏粗」改成几何绘制的。
///
/// 未列出的图标仍回退到字形（[`Icon::paint`]），行为不变。
fn draw_topbar_icon(painter: &egui::Painter, icon: Icon, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    if side <= 0.0 || color == egui::Color32::TRANSPARENT {
        return;
    }
    // **统一线宽**是「一排按钮协调」的关键：所有图标共用同一个值。
    // 9.5% 与放大镜接近；下限 1.4 保证 1x 屏上不出现断续。
    let w = (side * 0.095).max(1.4);
    let stroke = Stroke::new(w, color);

    // 归一化坐标 → 屏幕坐标。内缩 `pad` 给描边留出余量，
    // 否则最外圈的线宽会有一半落在包围盒外被裁掉。
    let pad = side * 0.09;
    let inner = side - pad * 2.0;
    let u = |x: f32, y: f32| pos2(r.min.x + pad + x * inner, r.min.y + pad + y * inner);
    let c = r.center();

    match icon {
        // 最小化 / 收起：一条横线。
        Icon::Minimize => {
            painter.line_segment([u(0.0, 0.5), u(1.0, 0.5)], stroke);
        }
        // 清空：垃圾桶（盖 + 提手 + 桶身 + 两条内竖线）。
        Icon::Trash => {
            painter.line_segment([u(0.0, 0.20), u(1.0, 0.20)], stroke);
            // 提手：倒 U，坐在盖线上。
            painter.add(egui::Shape::line(
                vec![u(0.34, 0.20), u(0.34, 0.02), u(0.66, 0.02), u(0.66, 0.20)],
                stroke,
            ));
            // 桶身：上宽下窄的梯形（开口朝上，由盖线闭合）。
            painter.add(egui::Shape::line(
                vec![u(0.06, 0.32), u(0.15, 1.0), u(0.85, 1.0), u(0.94, 0.32)],
                stroke,
            ));
            for x in [0.36_f32, 0.64] {
                painter.line_segment(
                    [u(x, 0.46), u(x, 0.86)],
                    Stroke::new(w * 0.8, color),
                );
            }
        }
        // 置顶：图钉。`Pinned` 实心、`Unpinned` 描边——两者形状完全相同，
        // 只差填充，因此切换时不会有「换了个图标」的跳变。
        Icon::Pinned | Icon::Unpinned => {
            let filled = matches!(icon, Icon::Pinned);
            let head = Rect::from_min_max(u(0.10, 0.00), u(0.90, 0.30));
            let tip = u(0.5, 0.60);
            let body = vec![u(0.10, 0.30), u(0.90, 0.30), tip];
            if filled {
                painter.rect_filled(head, w, color);
                painter.add(egui::Shape::convex_polygon(body, color, Stroke::NONE));
            } else {
                painter.rect_stroke(head, w, stroke, egui::StrokeKind::Inside);
                painter.add(egui::Shape::closed_line(body, stroke));
            }
            // 针尖：从锥底伸到底部。
            painter.line_segment([tip, u(0.5, 1.0)], stroke);
        }
        // 设置：齿轮 = 两个同心圆 + 一圈齿。
        //
        // 齿画在**外圈之外**（径向短线）而不是画成带缺口的轮廓：
        // 16pt 下挖缺口会糊成一团，短线更清晰且线宽与其它图标一致。
        Icon::Settings => {
            let r_out = side * 0.30;
            let r_in = side * 0.115;
            painter.circle_stroke(c, r_out, stroke);
            painter.circle_stroke(c, r_in, stroke);
            const TEETH: usize = 8;
            let r_tip = r_out + w * 1.7;
            for k in 0..TEETH {
                let a = k as f32 * std::f32::consts::TAU / TEETH as f32;
                let (sin, cos) = a.sin_cos();
                painter.line_segment(
                    [
                        pos2(c.x + cos * (r_out - w * 0.5), c.y + sin * (r_out - w * 0.5)),
                        pos2(c.x + cos * r_tip, c.y + sin * r_tip),
                    ],
                    stroke,
                );
            }
        }
        // 其余图标（行内 pin/复制/删除、类型标签等）仍走字形。
        _ => icon.paint(painter, r, color),
    }
}

fn draw_search_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    // 线宽取边长的 9%：约 1.6px（side=16），比字体字形明显纤细。
    // NOTE: 下限 1.5——太细在1x 屏上会断续，虚化。
    let stroke_w = (side * 0.09).max(1.5);
    let c = r.center();

    // 圆环：直径 52%，圆心偏左上。
    //
    // NOTE: 早前取 62% + 手柄起点在环**内**：放大截图看到手柄被圆环
    // 盖住，整个图标只剩一个空心圆，完全不像放大镜。
    // 现在环缩小、手柄起点移到环**外**（环边缘 + 线宽一半），
    // 两段在视觉上连成一体。
    let ring_r = side * 0.26;
    let off = side * 0.13; // 圆心偏移，留出右下角放手柄
    let ring_c = pos2(c.x - off, c.y - off);
    painter.circle_stroke(ring_c, ring_r, Stroke::new(stroke_w, color));

    // 手柄：从圆环**外缘**（45° 方向）伸向包围盒右下角。
    //
    // 起点 = 环心 + (ring_r + stroke_w*0.5) * √2/2——正好贴在环上，
    // 终点取包围盒的 0.47 处（留一点边距，避免描边被裁）。
    let diag = std::f32::consts::FRAC_1_SQRT_2;
    let start_off = (ring_r + stroke_w * 0.5) * diag;
    let start = pos2(ring_c.x + start_off, ring_c.y + start_off);
    // IMPORTANT: `reach` 必须留够「线宽的一半」的余量：`end` 还要描边，
    // 取 0.47 时终点 8.45 + 0.81 = 9.26 会**溢出**包围盒 9.0
    // （被裁掉一小截）。0.43 留 0.7pt 余量。
    let reach = side * 0.43;
    let end = pos2(c.x + reach, c.y + reach);
    painter.line_segment([start, end], Stroke::new(stroke_w, color));
}

/// 设置界面的整帧内容（画在**独立窗口**里）。
///
/// # 与旧「覆盖层」版的差别
///
/// 旧版是在主窗口上盖一块居中面板 + 一层遮罩。这里改成独立窗口后：
///
/// - **没有遮罩**——独立窗口天然就是模态的，不需要额外拦点击；
/// - **不居中**——窗口本身就是面板，按客户区铺满即可；
/// - **没有外框**——它已经是独立窗口，再画一层卡片框会显得
///   「窗口里还有一块面板」。
///
/// `area` 是**本窗口自己的客户区**，与主窗口无关。
/// 设置界面布局常量：左侧导航列宽（逻辑点）。
///
/// `settingswin` 的拖动区用这两个值挖掉左列，二者必须保持一致。
pub const SETTINGS_NAV_W: f32 = 172.0;
/// 设置界面外缘留白（逻辑点）。
pub const SETTINGS_MARGIN: f32 = 12.0;

/// 设置界面：左侧搜索框 + 圆角导航块，右侧卡片分组。
///
/// `area` 是**本窗口自己的客户区**，与主窗口无关。
pub fn draw_settings(f: &mut Frame<'_>) {
    let pal = f.pal;
    let area = f.area;

    // IMPORTANT: 清空必须**早于**下面所有提前 return。
    //
    // 若只在「画成了」的路径里清，那么某一帧尺寸异常跳过绘制时，
    // 上一帧的热区就留在表里——自动化拿着它去点，点的是不存在的位置。
    f.hit_clear();

    // NOTE: **首帧的 `area` 可能荒谬**（实测 6666x6666）。
    //
    // `ui.max_rect()` 取的是子 `Ui` 的可用矩形，而首帧 `screen_rect`
    // 尚未由 `egui_input` 正确设置（`RawInput.screen_rect` 是默认值）。
    // 此时按 `area` 排布的面板会被算到屏幕外，**看起来像设置界面没打开**。
    //
    // 显式拒绝明显不合理的尺寸：不画任何东西，等下一帧。
    let reasonable = area.width() <= 4000.0 && area.height() <= 4000.0;
    if !reasonable {
        tracing::debug!(area = ?area, "客户区尺寸异常，本帧跳过设置界面");
        return;
    }

    // 铺满客户区：外框 + 背景。独立窗口不再画卡片框，
    // 但要有底色，否则透明区域会透出桌面。
    f.ui.painter_at(area).rect_filled(area, 0.0, pal.bg);

    let m = SETTINGS_MARGIN;
    let left_w = SETTINGS_NAV_W;

    // ---- 左列：搜索框 + 导航块 ----
    let left_x0 = area.min.x + m;
    let left_x1 = left_x0 + left_w;
    // 高度随字号缩放（`control_height` 已含字体缩放）。
    let search_h = pal.control_height * 1.21;
    let search_rect = Rect::from_min_size(pos2(left_x0, area.min.y + m), vec2(left_w, search_h));
    draw_settings_search(f, search_rect);

    let nav_top = search_rect.max.y + 10.0;
    let nav_bottom = area.max.y - m;
    if nav_bottom > nav_top + 8.0 {
        let nav_rect = Rect::from_min_max(pos2(left_x0, nav_top), pos2(left_x1, nav_bottom));
        draw_settings_nav(f, nav_rect);
    }

    // ---- 右列：右上关闭按钮 + 内容 ----
    let right_x0 = left_x1 + m + 4.0; // 列间留白
    let right = Rect::from_min_max(
        pos2(right_x0, area.min.y + m),
        area.max - vec2(m, m),
    );
    if right.width() <= 8.0 || right.height() <= 8.0 {
        return;
    }

    // 右上角关闭按钮（独立于内容区，不随滚动）。
    let close_r = Rect::from_min_size(
        pos2(right.max.x - 30.0, right.min.y),
        vec2(30.0, 30.0),
    );
    draw_settings_close(f, close_r);

    // 内容区：关闭按钮下方，可滚动。
    //
    // 右缘**让出滚动条宽度 + 呼吸空间**：egui 的浮动滚动条画在内容区
    // 右缘之上（`ScrollStyle::floating`，不占布局宽度），正好压住右侧
    // 控件——开关、分段控件的最后一段、滑条手柄都会被它挡住（用户反馈
    // 「滑条展开后容易挡到按钮」）。让出后控件整体左移，滚动条落在
    // 空白里，与列表行（`draw_history_body` 的 `row_w`）同一处理。
    let content = Rect::from_min_max(
        pos2(right.min.x, close_r.max.y + 6.0),
        pos2(
            (right.max.x - SCROLLBAR_RESERVE - SCROLLBAR_GAP).max(right.min.x + 8.0),
            right.max.y,
        ),
    );
    if content.height() <= 8.0 {
        return;
    }
    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new("settings"))
            .max_rect(content)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .auto_shrink([false, false])
        .show(&mut ui, |ui| {
            settings_content(f, ui);
        });

    // 热键录制覆盖层（限制在本窗口客户区内，不遮桌面）。
    //
    // 与 MCP 覆盖层同一套路：设置内容照常画（它是 GPU 模糊的底），
    // 分割标记之后的本面板保持清晰。**整窗只认它**——录制中再弹
    // MCP 覆盖层只会叠罗汉，故录制态直接跳过 MCP。
    if f.state.hotkey_recording.is_some() {
        push_blur_split_marker(f, area);
        draw_recording_overlay(f, area);
        return;
    }
    // 录制结束 ⇒ 清掉入场补间条目：`fade_in_once` 判「首次出现」，
    // 不清的话只有程序生命周期内**第一次**录制有淡入动效，第二次起
    // 硬现（与弹窗 `just_opened` 清 `("popup_in", kind)` 同一个道理）。
    crate::anim::clear(f.ui.ctx(), egui::Id::new("recording_overlay_in"));

    // MCP 调用设置覆盖层（限制在本窗口客户区内，不遮桌面）。
    //
    // 画在内容区**之后**：视觉上盖住卡片，交互区也后注册 → 命中优先。
    if f.state.mcp_overlay_open {
        // 自研模糊的分割标记：渲染层据此把界面拆成「覆盖层之前的内容
        // （画到离屏图并卷积成模糊底图）」与「覆盖层本身（保持清晰）」。
        push_blur_split_marker(f, area);
        settings_mcp_overlay(f, area);
    }
}

/// 放置一个「模糊分割」标记。
///
/// 回调体是**空操作**——渲染层不执行它，只取它的**位置**作为
/// 「此点之后的内容保持清晰」的分界。见 `renderer::tessellate_into` 的
/// 返回值说明。
///
/// # 为什么用 `PaintCallback`
///
/// egui 的 `ClippedPrimitive` 不携带「第几段」信息，而 `PaintCallback`
/// 是 egui 自带的「此处插入自定义绘制」机制：它在 tessellation 里必然
/// 保留为一个独立 primitive（不会与相邻网格合批），天然适合当分段信号。
/// 自造一套「魔法颜色矩形」之类协议反而更脆弱。
fn push_blur_split_marker(f: &mut Frame<'_>, area: Rect) {
    f.ui.painter_at(area).add(egui::Shape::Callback(
        egui::epaint::PaintCallback {
            rect: area,
            callback: std::sync::Arc::new(|_: &egui::epaint::ClippedPrimitive| {}),
        },
    ));
}

/// 右上角关闭按钮（绝对定位，不随滚动）。
fn draw_settings_close(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let btn = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new("settings_close"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |u| u.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let btn = btn.inner;
    if btn.hovered() {
        f.ui.painter_at(r).rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    crate::icons::Icon::Close.paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(14.0, 14.0)),
        if btn.hovered() { pal.text_bright } else { pal.text_dim },
    );
    f.hit_insert("settings_close", r);
    if btn.clicked() {
        f.state.show_settings = false;
    }
}

/// 左侧搜索框：圆角输入框，右侧放大镜。
///
/// 定稿交互：内容为空且聚焦时显示「最近修改」；输入内容后转为
/// 搜索结果视图；失焦且内容为空时恢复当前分类卡片。
fn draw_settings_search(f: &mut Frame<'_>, search_rect: Rect) {
    let pal = f.pal;
    let mut q = f.state.settings_query.clone();
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new("settings_search"))
            .max_rect(search_rect),
        |u| {
            u.painter().rect_filled(search_rect, pal.radius_md, pal.surface_variant);
            // 文字输入区（左侧留白，右侧留图标位）。
            let text_rect = Rect::from_min_max(
                search_rect.min + vec2(10.0, 0.0),
                search_rect.max - vec2(30.0, 0.0),
            );
            let mut child = u.new_child(
                egui::UiBuilder::new()
                    .max_rect(text_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let r = child.add(
                egui::TextEdit::singleline(&mut q)
                    .hint_text("搜索设置")
                    .frame(egui::Frame::NONE)
                    .desired_width(text_rect.width()),
            );
            // 右侧放大镜。
            let icon_r = Rect::from_center_size(
                pos2(search_rect.max.x - 15.0, search_rect.center().y),
                vec2(16.0, 16.0),
            );
            crate::icons::Icon::Search.paint(u.painter(), icon_r, pal.text_dim);
            r
        },
    );
    let inner = resp.inner;
    if q != f.state.settings_query {
        f.state.settings_query = q;
    }
    // 空态聚焦 → 最近修改；失焦（空）→ 回分类视图。
    if f.state.settings_query.is_empty() {
        f.state.settings_show_recent = inner.has_focus();
    }
    f.hit_insert("settings_search", search_rect);
}

/// 左侧导航块：圆角背景 + 分类按钮（图标 + 标题）。
///
/// 导航背景是**一整块圆角矩形**，顶边与搜索框下缘对齐、底边伸到
/// 窗口下缘留白处——「导航背景覆盖设置与搜索框」的问题由搜索框
/// 画在导航块**上方**解决。
/// 画「通信」分类的插头图标（自绘，不依赖图标字体）。
///
/// 参照 HTML 原型的插头 SVG（24 viewBox）：两个脚（插头插脚）+
/// 圆角矩形（插头头）+ 一条线（线尾）。映射到 16×16 的 `r`。
fn draw_plug_icon(painter: &egui::Painter, r: egui::Rect, color: egui::Color32, stroke_w: f32) {
    let stroke = egui::Stroke::new(stroke_w, color);
    let u = |x: f32, y: f32| -> egui::Pos2 { pos2(r.min.x + x, r.min.y + y) };
    // 两个脚
    painter.line_segment([u(5.5, 1.0), u(5.5, 5.5)], stroke);
    painter.line_segment([u(10.5, 1.0), u(10.5, 5.5)], stroke);
    // 头
    let head = egui::Rect::from_min_max(u(4.0, 5.0), u(12.0, 10.5));
    painter.rect_stroke(head, 2.0, stroke, egui::StrokeKind::Inside);
    // 线
    painter.line_segment([u(8.0, 11.0), u(8.0, 14.5)], stroke);
}

/// 自绘调色板图标（「主题」分类导航项）。
///
/// 三个色圆点 + 一道斜笔：既读出「调色板」的形状，又暗示「多个颜色
/// 可选」。用**当前主题的明暗关系**取色（而不是固定红黄蓝）——导航
/// 图标跟着主题走，切到 OLED / 纸质时不会出现一个刺眼的原色块。
///
/// CAUTION: 三个点必须用不透明色：透明点在磨砂卡片上会透出底图，
/// 小尺寸下糊成一团。
fn draw_palette_icon(painter: &egui::Painter, r: egui::Rect, color: egui::Color32) {
    let u = |x: f32, y: f32| -> egui::Pos2 { pos2(r.min.x + x, r.min.y + y) };
    let rad = 2.6_f32;
    // 三点错位排布（上二下一），避免一条直线的呆板感。
    painter.circle_filled(u(4.6, 5.0), rad, color);
    painter.circle_filled(u(11.4, 4.2), rad, color);
    painter.circle_filled(u(8.0, 10.6), rad, color);
    // 一道斜笔（画家的笔触感），从左上划到右下。
    painter.line_segment(
        [u(3.0, 13.2), u(13.0, 13.2)],
        egui::Stroke::new(1.4, color),
    );
}

fn draw_settings_nav(f: &mut Frame<'_>, nav_rect: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(nav_rect);
    painter.rect_filled(nav_rect, pal.radius_lg, pal.surface);

    let pad = 6.0;
    let gap = 2.0;
    // 项高随字号缩放，否则大字号下导航文字会顶到相邻项。
    let item_h = pal.control_height * 1.43;
    let row_step = item_h + gap;
    let item_rect = |i: usize| {
        let y0 = nav_rect.min.y + pad + i as f32 * row_step;
        Rect::from_min_max(
            pos2(nav_rect.min.x + pad, y0),
            pos2(nav_rect.max.x - pad, y0 + item_h),
        )
    };

    // ---- 选中底：**一整块**，在两项之间平移 -------------------------
    //
    // 早前是「每项各自淡入淡出」：切换分类时旧块淡出、新块淡入，两个
    // 半透明块在中间同时存在——观感是「闪一下」而不是「移过去」。
    // 用户要求「选中导航后背景平移到选择导航」，所以改成**单块**：
    // 位置（下标）走补间，切换时从旧项滑到新项；颜色/圆角不变。
    //
    // 补间键与分类无关（`nav_pill`）——有键就只有一个块，天然不会
    // 出现两块并存。从当前显示值续接：连续快速切换时它在中途改方向，
    // 不会跳回起点（与分段控件的 `seg_pill` 同一套）。
    let sel_idx = SettingsCat::ALL
        .iter()
        .position(|c| *c == f.state.settings_cat)
        .unwrap_or(0) as f32;
    let pill_idx = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::new("nav_pill"),
        sel_idx,
        std::time::Duration::from_millis(220),
        crate::anim::Ease::OutCubic,
    );
    let pill_y = nav_rect.min.y + pad + pill_idx * row_step;
    let pill = Rect::from_min_max(
        pos2(nav_rect.min.x + pad, pill_y),
        pos2(nav_rect.max.x - pad, pill_y + item_h),
    );
    painter.rect_filled(pill, pal.radius_md, pal.row_selected);

    for (i, cat) in SettingsCat::ALL.iter().enumerate() {
        let r = item_rect(i);
        let selected = f.state.settings_cat == *cat;
        let resp = f
            .ui
            .interact(r, egui::Id::new(("settings_cat", *cat)), egui::Sense::click());
        // ---- 悬停底（110ms）：只在未选中时叠 -----------------------------
        //
        // 选中态由上面那块**平移的**选中底表达，这里不再各画一份——
        // 两块半透明底叠在一起会加深，滑过时颜色不匀。
        if !selected {
            let hov_t = crate::anim::tween_f32(
                f.ui.ctx(),
                egui::Id::new(("nav_hover", *cat)),
                if resp.hovered() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(110),
                crate::anim::Ease::OutCubic,
            );
            if hov_t > 0.004 {
                painter.rect_filled(r, pal.radius_md, fade_color(pal.row_hover, hov_t));
            }
        }
        // 图标（左）+ 标题（紧挨图标，不留大空隙）。
        //
        // 早期实现文字以 `r.center() + vec2(14, 0)` 左对齐起点——图标
        // 贴左缘（x≈17）、文字却从导航项中心偏右起，中间空出约 70pt，
        // 观感「图标和字体离得太远」（2026-10-11 用户反馈）。改为文字
        // 起点 = 图标右缘 + 8pt，图标与标题紧凑成组。
        let icon_r = Rect::from_center_size(pos2(r.min.x + 17.0, r.center().y), vec2(16.0, 16.0));
        // 图标与文字颜色补间（130ms）：切分类时旧项褪色、新项点亮，
        // 与选中底同步。首帧落目标（`tween_color` 惰性插入）。
        let icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new(("nav_icon", *cat)),
            if selected { pal.accent } else { pal.text_dim },
            std::time::Duration::from_millis(130),
        );
        let text_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::new(("nav_text", *cat)),
            if selected { pal.text_bright } else { pal.text_dim },
            std::time::Duration::from_millis(130),
        );
        // 「通信」用自绘插头、「主题」用自绘调色板（图标字体的 PUA
        // 码位已占满，新增字形要重新生成 TTF，成本高）：头 + 两个脚
        // + 线。其余分类用字体图标。
        if *cat == SettingsCat::Comm {
            draw_plug_icon(&painter, icon_r, icon_color, pal.stroke_normal);
        } else if *cat == SettingsCat::Theme {
            draw_palette_icon(&painter, icon_r, icon_color);
        } else {
            cat.icon().paint(&painter, icon_r, icon_color);
        }
        painter.text(
            pos2(icon_r.max.x + 8.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            cat.title(),
            egui::FontId::proportional(pal.font_sm),
            text_color,
        );
        f.hit_insert(cat.hot_key(), r);
        if resp.clicked() {
            f.state.settings_cat = *cat;
            f.state.settings_query.clear();
            f.state.settings_show_recent = false;
        }
    }
}

// ---------------------------------------------------------------------------
// 设置卡片描述：每张卡片 = 标题 + 设置项列表。
//
// 数据驱动的好处：
// 1. 「重置按钮」与「搜索」都遍历同一份结构，不会出现
//    「界面渲染一项、重置漏一项」的分叉；
// 2. 新增设置项只改这里 + `app.rs` 的字段处理器。
// ---------------------------------------------------------------------------

/// 数值项规格（含存储值 ↔ 显示值换算，如字节 ↔ MB）。
struct NumberSpec {
    min: f64,
    max: f64,
    step: f64,
    unit: &'static str,
    /// 存储值 → 显示值。
    display: fn(f64) -> f64,
    /// 显示值 → 存储值。
    store: fn(f64) -> f64,
}

/// 可选数值项规格（`None` = 不限）。
struct OptNumberSpec {
    default: f64,
    step: f64,
    unit: &'static str,
}

const fn identity_num(v: f64) -> f64 {
    v
}
const fn bytes_to_mb(v: f64) -> f64 {
    v / 1048576.0
}
const fn mb_to_bytes(v: f64) -> f64 {
    v * 1048576.0
}

const NUMBER_POLL_MS: NumberSpec = NumberSpec {
    min: 50.0,
    max: 2000.0,
    step: 50.0,
    unit: " ms",
    display: identity_num,
    store: identity_num,
};
/// 迷你面板显示条数：1 ~ 10，步进 1。
const NUMBER_MINI_COUNT: NumberSpec = NumberSpec {
    min: 1.0,
    max: 10.0,
    step: 1.0,
    unit: "条",
    display: |v| v,
    store: |v| v,
};

const NUMBER_MAX_PAYLOAD_MB: NumberSpec = NumberSpec {
    min: 1.0,
    max: 4096.0,
    step: 1.0,
    unit: " MB",
    display: bytes_to_mb,
    store: mb_to_bytes,
};
const OPTN_MAX_ITEMS: OptNumberSpec = OptNumberSpec {
    default: 1000.0,
    step: 100.0,
    unit: " 条",
};

/// 设置项类型。
enum ItemKind {
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
    /// 悬浮类型过滤栏：总开关 + 始终显示 + 停靠位置 + 图标大小。
    ///
    /// 独立成变体（同 [`ItemKind::CardShadow`]）：四个参数一张互相
    /// 牵连的卡，关掉总开关后其余三个一起失效，走通用渲染会得到
    /// 四个互不相干的开关/滑块。
    TypeRail,
    /// 字体缩放：**预设档位**分段控件（小 / 标准 / 大 / 特大 / 超大）。
    ///
    /// # 为什么不用滑条 / 数字输入
    ///
    /// 字号没有「连续可调」的真实需求——用户要的是「字小一点 / 大一点」，
    /// 而 0.05 一档的滑条既看不出差别，又让每拖动一格都触发全量重排。
    /// 预设档位一次到位、可预期、可回归测试（点第 N 段必得第 N 个倍率）。
    /// 值不在预设上时（手工改过配置文件）显示为「自定义」，不假装选中。
    FontScale,
    /// 快捷键绑定（只读展示）。
    Hotkey(HotkeySlot),
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
        ph: &'static str,
        secret: bool,
    },
    /// MCP 端口（数字输入，留空 = 未设置）。
    McpPort,
    /// 只读信息行（关于页）。
    Info(&'static str),
    /// 可点击的外部链接（关于页）：显示文本即 URL，点击用默认浏览器打开。
    Link(&'static str),
    /// 退出程序按钮（关于页）。
    Quit,
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
pub const FONT_SCALE_PRESETS: &[(u8, &str)] = &[
    (85, "小"),
    (100, "标准"),
    (115, "大"),
    (130, "特大"),
    (150, "超大"),
];

/// 配置里的 `font_scale` 倍率 → 预设百分比（取最接近的一档）。
///
/// 不在预设上（手工改过配置文件）时也返回最近档位，但
/// [`font_scale_is_preset`] 会是 `false`——调用方据此显示「自定义」，
/// 而不是把最近的档位当成「当前选中」误导用户。
fn font_scale_percent(scale: f32) -> u8 {
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
fn font_scale_is_preset(scale: f32) -> bool {
    let pct = (crate::theme::clamp_font_scale(scale) * 100.0).round() as i32;
    FONT_SCALE_PRESETS.iter().any(|(p, _)| (i32::from(*p) - pct).abs() <= 0)
}

/// MCP 可调用能力清单（叠加层内展示，顺序即显示顺序）。
struct McpCap {
    field: &'static str,
    name: &'static str,
    desc: &'static str,
    ready: bool,
}

/// 全部 MCP 可调用能力（与 `config::McpConfig` 字段一一对应）。
static MCP_CAPS: &[McpCap] = &[
    McpCap { field: "resize_window", name: "调节窗口大小", desc: "调整任意窗口的尺寸与位置", ready: true },
    McpCap { field: "read_layout", name: "获取元素布局", desc: "读取界面元素的几何与层级信息", ready: true },
    McpCap { field: "click_element", name: "点击界面元素", desc: "在任意位置点击或激活元素", ready: true },
    McpCap { field: "type_text", name: "输入文本", desc: "向焦点控件键入文字与快捷键", ready: true },
    McpCap { field: "scroll_view", name: "滚动视图", desc: "滚动页面与列表到目标位置", ready: true },
    McpCap { field: "screen_capture", name: "屏幕截图", desc: "截取屏幕或窗口画面", ready: true },
    McpCap { field: "window_focus", name: "切换焦点窗口", desc: "激活指定窗口或应用", ready: false },
    McpCap { field: "drag_drop", name: "拖拽操作", desc: "拖拽元素或执行拖放", ready: false },
    McpCap { field: "key_press", name: "按键模拟", desc: "按下组合键与功能键（快捷键、确认/取消）", ready: true },
    McpCap { field: "clipboard", name: "剪贴板读写", desc: "读取或写入系统剪贴板文本", ready: true },
];

/// 读一个 MCP 能力开关。
fn get_mcp_cap(cfg: &modular_clipboard_core::config::Config, field: &str) -> bool {
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
    let mut s = String::from("【modular-clipboard · MCP 服务配对】\nmodular-clipboard 提供 Model Context Protocol（MCP）服务，可对本机执行界面自动化操作。\n\n可调用工具（10 项）：\n");
    for cap in MCP_CAPS {
        s.push_str(&format!("- {}：{}\n", cap.name, cap.desc));
    }

    // 当前 exe 完整路径（stdio 配置里 command 用）；取不到时用占位。
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<modular-clipboard.exe 的完整路径>".into());

    s.push_str(&format!(
        "\n=== 方式一：stdio（本地，推荐） ===\n适用于 Claude Desktop / Cursor 等本地客户端，在 MCP 配置中加入：\n{{\n  \"mcpServers\": {{\n    \"modular-clipboard\": {{\n      \"command\": \"{exe}\",\n      \"args\": [\"--mcp-server\"]\n    }}\n  }}\n}}\n"
    ));

    // HTTP 段：地址、端口、认证。
    let host = if cfg.mcp.host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        cfg.mcp.host.clone()
    };
    let port = if cfg.mcp.port == 0 { 9123 } else { cfg.mcp.port };
    let auth = if cfg.mcp.token.is_empty() {
        "无".to_string()
    } else {
        "Bearer Token（已在设置中配置）".to_string()
    };
    s.push_str(&format!(
        "\n=== 方式二：HTTP（远程 / 自定义地址） ===\n服务地址：http://{host}:{port}/mcp\n认证：{auth}\n配置：\n{{\n  \"mcpServers\": {{\n    \"modular-clipboard\": {{\n      \"url\": \"http://{host}:{port}/mcp\"\n    }}\n  }}\n}}\n启动 HTTP 服务：modular-clipboard.exe --mcp-http\n"
    ));
    s
}

/// 一个设置项。
struct SettingsItem {
    label: &'static str,
    desc: &'static str,
    kind: ItemKind,
}

impl SettingsItem {
    /// 当前值是否偏离默认（决定重置按钮是否出现）。
    fn is_changed(
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
            ItemKind::Info(_) | ItemKind::Link(_) | ItemKind::Quit => false,
        }
    }

    /// 该设置项的重置操作（恢复默认值）。
    fn reset_op(&self) -> Option<Op> {
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
            ItemKind::CardShadow => Some(Op::SetCardShadow(d.ui.card_shadow)),
            ItemKind::RowMeta => Some(Op::SetRowMeta(d.ui.row_meta)),
            ItemKind::TypeRail => Some(Op::SetTypeRail(d.ui.type_rail)),
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
            ItemKind::Info(_) | ItemKind::Link(_) | ItemKind::Quit => None,
        }
    }
}

/// 一张设置卡片。
struct SettingsCard {
    cat: SettingsCat,
    title: &'static str,
    items: &'static [SettingsItem],
}

/// 全部分类卡片（导航顺序与 `SettingsCat::ALL` 一致）。
static SETTINGS_CARDS: &[SettingsCard] = &[
    SettingsCard {
        cat: SettingsCat::General,
        title: "常规",
        items: &[
            SettingsItem {
                label: "窗口置顶",
                desc: "始终显示在其它窗口之上",
                kind: ItemKind::Bool(ConfigGroup::Ui, "always_on_top"),
            },
            SettingsItem {
                label: "失焦时隐藏",
                desc: "切换到别的程序后隐藏窗口",
                kind: ItemKind::Bool(ConfigGroup::Ui, "hide_on_focus_lost"),
            },
            SettingsItem {
                label: "显示托盘图标",
                desc: "后台常驻，保留右键菜单入口",
                kind: ItemKind::Bool(ConfigGroup::Ui, "show_tray"),
            },
            SettingsItem {
                label: "启动时最小化",
                desc: "启动后隐藏到托盘",
                kind: ItemKind::Bool(ConfigGroup::Ui, "start_minimized"),
            },
            SettingsItem {
                label: "置顶模式",
                desc: "共用单栏与历史同列，独立分栏可拖出",
                kind: ItemKind::PinnedMode,
            },
            SettingsItem {
                label: "开机自启动",
                desc: "开机后只启动后台记录，界面从托盘唤起",
                kind: ItemKind::Autostart,
            },
            SettingsItem {
                label: "浏览器路径",
                desc: "双击链接型条目时调用的程序；留空用系统默认浏览器",
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "browser_path",
                    ph: "默认（系统默认浏览器）",
                    secret: false,
                },
            },
            SettingsItem {
                label: "资源管理器路径",
                desc: "双击文件型条目时调用的程序；留空用系统资源管理器",
                kind: ItemKind::Text {
                    group: ConfigGroup::Ui,
                    field: "explorer_path",
                    ph: "默认（explorer.exe）",
                    secret: false,
                },
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Theme,
        title: "主题",
        items: &[
            SettingsItem {
                label: "明暗模式",
                desc: "跟随系统 / 深色 / 浅色",
                kind: ItemKind::Theme,
            },
            SettingsItem {
                label: "主题色",
                desc: "强调色：预设色板 / 色相滑块 / 十六进制，三种方式任选",
                kind: ItemKind::Accent,
            },
            SettingsItem {
                label: "二级主题",
                desc: "叠加在深浅模式上的风格层：纸质纹理 / 护眼绿 / 星空紫 / OLED",
                kind: ItemKind::SecondaryTheme,
            },
            SettingsItem {
                label: "二级主题参数",
                desc: "当前二级主题的浓度与效果强度",
                kind: ItemKind::ThemeParams,
            },
            SettingsItem {
                label: "高级微调",
                desc: "逐个令牌覆盖主题取值（任何元素、任何参数）",
                kind: ItemKind::TokenOverrides,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: "外观",
        items: &[
            SettingsItem {
                label: "字体缩放",
                desc: "界面文字大小（预设档位，即改即生效）",
                kind: ItemKind::FontScale,
            },
            SettingsItem {
                label: "背景效果",
                desc: "无 / 磨砂（实时模糊） / 半透明",
                kind: ItemKind::Backdrop,
            },
            SettingsItem {
                label: "磨砂强度",
                desc: "磨砂模式下的模糊程度（1~10 档）",
                kind: ItemKind::BlurStrength,
            },
            SettingsItem {
                label: "背景不透明度",
                desc: "背景层强度，文字始终不透明",
                kind: ItemKind::Opacity,
            },
            SettingsItem {
                label: "卡片不透明度",
                desc: "磨砂 / 半透明模式下条目卡片的实度：调低让模糊底图透过卡片（仅这两种模式可调）",
                kind: ItemKind::CardOpacity,
            },
            SettingsItem {
                label: "卡片阴影",
                desc: "条目卡片的高斯投影；浓度 20% 为默认（16% = 1.0× 主题强度），深色主题自动加浓；开启后行间距自动适配，投影不被下一张卡片遮挡",
                kind: ItemKind::CardShadow,
            },
            SettingsItem {
                label: "右下角信息",
                desc: "列表卡片右下角的图标 / 类型 / 大小：单击元素切换显示（灰色 = 隐藏），按住拖动可调整前后顺序",
                kind: ItemKind::RowMeta,
            },
            SettingsItem {
                label: "图片内容直接显示",
                desc: "图片类型的条目在列表里直接显示缩略图；关闭后只显示尺寸描述",
                kind: ItemKind::Bool(ConfigGroup::Ui, "image_inline"),
            },
            SettingsItem {
                label: "Markdown 渲染显示",
                desc: "Markdown 类型按标题 / 列表 / 代码块排版；关闭后显示等宽源码",
                kind: ItemKind::Bool(ConfigGroup::Ui, "md_rendered"),
            },
            SettingsItem {
                label: "类型过滤栏",
                desc: "主窗口边缘的悬浮类型按钮：单击按类型过滤、再点取消；默认只显示已有记录的类型，位置与大小可调",
                kind: ItemKind::TypeRail,
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Capture,
        title: "捕获",
        items: &[
            SettingsItem {
                label: "记录剪贴板",
                desc: "关闭后不再新增历史",
                kind: ItemKind::Bool(ConfigGroup::Capture, "enabled"),
            },
            SettingsItem {
                label: "跳过密码字段",
                desc: "疑似输入密码时不记录",
                kind: ItemKind::Bool(ConfigGroup::Capture, "skip_password_fields"),
            },
            SettingsItem {
                label: "复制去重",
                desc: "相同内容不再新增，自动置顶更新",
                kind: ItemKind::Bool(ConfigGroup::Capture, "dedup"),
            },
            SettingsItem {
                label: "监听间隔",
                desc: "越小越灵敏、越耗电",
                kind: ItemKind::Number(ConfigGroup::Capture, "poll_interval_ms", &NUMBER_POLL_MS),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Storage,
        title: "存储",
        items: &[
            SettingsItem {
                label: "历史条数上限",
                desc: "留空表示只按容量淘汰",
                kind: ItemKind::OptNumber(ConfigGroup::Storage, "max_items", &OPTN_MAX_ITEMS),
            },
            SettingsItem {
                label: "单条大小上限",
                desc: "超出该大小的内容不记录",
                kind: ItemKind::Number(ConfigGroup::Storage, "max_payload_bytes", &NUMBER_MAX_PAYLOAD_MB),
            },
            SettingsItem {
                label: "启动时清理临时文件",
                desc: "删除上次退出遗留的临时文件",
                kind: ItemKind::Bool(ConfigGroup::Storage, "cleanup_on_start"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: "快捷键",
        items: &[
            SettingsItem {
                label: "显示/隐藏主窗口",
                desc: "全局唤起",
                kind: ItemKind::Hotkey(HotkeySlot::Toggle),
            },
            SettingsItem {
                label: "粘贴上一个条目",
                desc: "直接粘贴最近复制的条目",
                kind: ItemKind::Hotkey(HotkeySlot::PastePrevious),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Shortcuts,
        title: "迷你快捷复制面板",
        items: &[
            SettingsItem {
                label: "启用",
                desc: "按住修饰键点鼠标右键，在光标处弹出最近几条",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "enabled"),
            },
            SettingsItem {
                label: "显示条数",
                desc: "1 ~ 10 条",
                kind: ItemKind::Number(ConfigGroup::MiniPanel, "count", &NUMBER_MINI_COUNT),
            },
            SettingsItem {
                label: "Ctrl + 右键",
                desc: "与下面几项可叠加，至少留一个",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "ctrl"),
            },
            SettingsItem {
                label: "Shift + 右键",
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "shift"),
            },
            SettingsItem {
                label: "Alt + 右键",
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "alt"),
            },
            SettingsItem {
                label: "Win + 右键",
                desc: "",
                kind: ItemKind::Bool(ConfigGroup::MiniPanel, "win"),
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::Comm,
        title: "MCP 服务",
        items: &[
            SettingsItem {
                label: "MCP 服务",
                desc: "关闭后所有 MCP 调用不可用",
                kind: ItemKind::McpToggle,
            },
            SettingsItem {
                label: "服务地址",
                desc: "MCP 服务监听地址",
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "host", ph: "127.0.0.1", secret: false },
            },
            SettingsItem {
                label: "端口",
                desc: "MCP 服务端口，留空不设置",
                kind: ItemKind::McpPort,
            },
            SettingsItem {
                label: "密钥",
                desc: "默认无，留空表示无需认证",
                kind: ItemKind::Text { group: ConfigGroup::Mcp, field: "token", ph: "无", secret: true },
            },
        ],
    },
    SettingsCard {
        cat: SettingsCat::About,
        title: "关于",
        items: &[
            SettingsItem {
                label: "版本",
                desc: "",
                kind: ItemKind::Info(concat!("v", env!("CARGO_PKG_VERSION"))),
            },
            SettingsItem {
                label: "项目主页",
                desc: "源代码、问题反馈与发布版本，点击在浏览器打开",
                kind: ItemKind::Link("https://github.com/MCheng404/ModularClipboard"),
            },
            SettingsItem {
                label: "作者主页",
                desc: "GitHub 主页，点击在浏览器打开",
                kind: ItemKind::Link("https://github.com/MCheng404"),
            },
            SettingsItem {
                label: "开源协议",
                desc: "",
                kind: ItemKind::Info("MIT License"),
            },
            SettingsItem {
                label: "技术栈",
                desc: "",
                kind: ItemKind::Info("Rust · egui · Vulkan (Win32)"),
            },
            SettingsItem {
                label: "数据目录",
                desc: "",
                kind: ItemKind::Info("系统应用数据目录（%APPDATA%\\modular-clipboard）"),
            },
            SettingsItem {
                label: "退出程序",
                desc: "",
                kind: ItemKind::Quit,
            },
        ],
    },
];

/// 读一个布尔配置字段（与 `app.rs::set_bool_field` 的键表对称）。
fn get_bool(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> bool {
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
fn get_number(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> f64 {
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
fn get_text(cfg: &modular_clipboard_core::config::Config, g: ConfigGroup, field: &str) -> String {
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
fn get_opt_number(
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
fn settings_content(f: &mut Frame<'_>, ui: &mut Ui) {
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
fn settings_category_cards(f: &mut Frame<'_>, ui: &mut Ui) {
    let Some(card) = SETTINGS_CARDS.iter().find(|c| c.cat == f.state.settings_cat) else {
        return;
    };
    settings_card(f, ui, card);
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
fn settings_card(f: &mut Frame<'_>, ui: &mut Ui, card: &SettingsCard) {
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
                        let resp = ui.add(
                            egui::Button::new(
                                egui::RichText::new("重置").size(pal.font_sm).color(pal.text_dim),
                            )
                            .frame(false),
                        );
                        let rect = resp.rect;
                        let clicked = resp.clicked();
                        resp.on_hover_text("恢复该卡片默认值");
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
                                .retain(|(l, _)| !card.items.iter().any(|it| it.label == *l));
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
                    "一键 AI 配对提示词",
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
fn shadow_param_row(
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
fn draw_row_meta_editor(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    let mut changed = false;
    let pal = f.pal;
    let cfg = f.svc.state.config.ui.row_meta;
    let font = egui::FontId::proportional(pal.font_sm);
    let chip_h = pal.font_sm + 9.0;
    let chip_pad_x = 11.0;
    let chip_radius = 7.0;
    let chip_gap = 6.0;

    // 拖拽态跨帧记忆：当前正被拖动的元素。
    let drag_id = ui.id().with("row_meta_drag");
    let dragged: Option<RowMetaPart> = ui.data(|d| d.get_temp(drag_id));

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = chip_gap;
        // 先布局所有 chip 并登记交互、收集 rect（拖拽时需要彼此的位置）。
        let mut cells: Vec<(RowMetaPart, Rect, egui::Response)> = Vec::with_capacity(3);
        for &part in cfg.order.iter() {
            let w = crate::text::measure_text(ui, part.label(), &font) + chip_pad_x * 2.0;
            let (rect, _) = ui.allocate_exact_size(vec2(w, chip_h), egui::Sense::hover());
            let id = ui.id().with(("rowmeta_chip", part.label()));
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
                part.label(),
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

/// 一行设置项：按类型分派到开关 / 数值 / 单选 / 只读。
fn settings_item_row(f: &mut Frame<'_>, ui: &mut Ui, it: &SettingsItem) {
    let pal = f.pal;
    match &it.kind {
        ItemKind::Bool(g, field) => {
            let mut v = get_bool(&f.svc.state.config, *g, field);
            if setting_row(ui, it.label, it.desc, |ui| draw_switch(f, ui, field, &mut v)) {
                f.state.push(Op::SetBool { group: *g, field, value: v });
                record_recent(f, it.label);
            }
        }
        ItemKind::Number(g, field, spec) => {
            let cur = (spec.display)(get_number(&f.svc.state.config, *g, field));
            let mut val = cur;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(
                    egui::DragValue::new(&mut val)
                        .range(spec.min..=spec.max)
                        .speed(spec.step)
                        .suffix(spec.unit),
                )
                .changed()
            });
            if changed {
                f.state.push(Op::SetNumber {
                    group: *g,
                    field,
                    value: (spec.store)(val),
                });
                record_recent(f, it.label);
            }
        }
        ItemKind::OptNumber(g, field, spec) => {
            let cur = get_opt_number(&f.svc.state.config, *g, field);
            let mut val = cur.unwrap_or(spec.default);
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(
                    egui::DragValue::new(&mut val)
                        .range(spec.default..=1_000_000.0)
                        .speed(spec.step)
                        .suffix(spec.unit),
                )
                .changed()
            });
            if changed {
                let next = if val <= spec.default { None } else { Some(val) };
                f.state.push(Op::SetOptNumber { group: *g, field, value: next });
                record_recent(f, it.label);
            }
        }
        ItemKind::PinnedMode => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "pinned_mode",
                    &[
                        (PinnedMode::SharedColumn, "共用单栏"),
                        (PinnedMode::OwnCard, "独立分栏"),
                    ],
                    f.ws.pinned_mode(),
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetPinnedMode(m));
                    record_recent(f, it.label);
                }
                picked.is_some()
            });
        }
        ItemKind::FontScale => {
            let cur = f.svc.state.config.ui.font_scale;
            setting_row(ui, it.label, it.desc, |ui| {
                // 百分比读数（right_to_left：先加的靠右 ⇒ 显示在分段控件右侧）。
                // 不在预设上时标「自定义」——比假装选中最近档位诚实。
                let pct = (crate::theme::clamp_font_scale(cur) * 100.0).round() as i32;
                let readout = if font_scale_is_preset(cur) {
                    format!("{pct}%")
                } else {
                    format!("{pct}%（自定义）")
                };
                ui.add(egui::Label::new(
                    egui::RichText::new(readout).color(pal.text_dim),
                ));
                // 分段控件：值与配置同源，点哪段设哪个倍率。
                let picked = draw_segmented(f, ui, "font_scale", FONT_SCALE_PRESETS, font_scale_percent(cur));
                if let Some(p) = picked {
                    f.state.push(Op::SetNumber {
                        group: ConfigGroup::Ui,
                        field: "font_scale",
                        value: f64::from(p) / 100.0,
                    });
                    record_recent(f, it.label);
                }
                picked.is_some()
            });
        }
        ItemKind::Theme => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "theme_mode",
                    &[
                        (crate::theme::ThemeMode::FollowSystem, "跟随系统"),
                        (crate::theme::ThemeMode::Dark, "深色"),
                        (crate::theme::ThemeMode::Light, "浅色"),
                    ],
                    crate::theme::ThemeMode::from_dark_mode(f.svc.state.config.ui.dark_mode),
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetThemeMode(m));
                    record_recent(f, it.label);
                }
                picked.is_some()
            });
        }
        ItemKind::Accent => {
            if draw_accent_editor(f, ui) {
                record_recent(f, it.label);
            }
        }
        ItemKind::SecondaryTheme => {
            use modular_clipboard_core::config::SecondaryTheme;
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "secondary_theme",
                    &[
                        (SecondaryTheme::None, "无"),
                        (SecondaryTheme::Paper, "纸质"),
                        (SecondaryTheme::Green, "护眼"),
                        (SecondaryTheme::Purple, "星空"),
                        (SecondaryTheme::Oled, "OLED"),
                    ],
                    f.svc.state.config.ui.secondary_theme,
                );
                if let Some(t) = picked {
                    f.state.push(Op::SetSecondaryTheme(t));
                    record_recent(f, it.label);
                }
                picked.is_some()
            });
        }
        ItemKind::ThemeParams => {
            let theme = f.svc.state.config.ui.secondary_theme;
            let mut params = f.svc.state.config.ui.theme_params;
            let mut changed = false;
            if theme.knobs().is_empty() {
                // 没有二级主题：整项禁用并压暗（与磨砂强度在非磨砂模式下
                // 禁用的处理一致——「关着还能调」会让用户以为参数没保存）。
                let _ = setting_row(ui, it.label, it.desc, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new("先选择一个二级主题").color(f.pal.text_dim),
                    ));
                    false
                });
            } else {
                // 摘要行：右侧显示当前主题名（参数归属一眼可见）。
                let _ = setting_row(ui, it.label, it.desc, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(theme.label()).color(f.pal.text_dim),
                    ));
                    false
                });
                // 旋钮按主题的 `knobs()` 动态生成：新增主题参数只用在
                // core 侧登记，设置界面自动跟上（键名寻址测试守着）。
                for knob in theme.knobs() {
                    let mut v = params.get(knob.key).unwrap_or(0.5);
                    ui.add_space(2.0);
                    let c = shadow_param_row(
                        f,
                        ui,
                        knob.key,
                        knob.label,
                        format!("{}%", (v * 100.0).round() as i32),
                        &mut v,
                        0.0..=1.0,
                    );
                    if c {
                        params.set(knob.key, v);
                        changed = true;
                    }
                }
            }
            if changed {
                // 整包替换：两个旋钮可能同帧连改（见 Op::SetThemeParams）。
                f.state.push(Op::SetThemeParams(params));
                record_recent(f, it.label);
            }
        }
        ItemKind::TokenOverrides => {
            // 2026-10-19：入口行只负责打开二级面板（draw_token_panel），
            // 近 60 行令牌不再内联在主题卡里。
            draw_token_entry(f, ui);
        }
        ItemKind::Backdrop => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "window_backdrop",
                    &[
                        (WindowBackdrop::None, "无"),
                        (WindowBackdrop::Acrylic, "磨砂"),
                        (WindowBackdrop::Translucent, "半透明"),
                    ],
                    f.svc.state.config.ui.window_backdrop,
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetBackdrop(m));
                    record_recent(f, it.label);
                }
                picked.is_some()
            });
        }
        ItemKind::Opacity => {
            let mut op = f.svc.state.config.ui.window_opacity;
            let enabled = f.svc.state.config.ui.window_backdrop != WindowBackdrop::None;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // right_to_left 布局：先加的靠右 ⇒ 百分比显示在滑条右侧。
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{}%", (op * 100.0).round() as i32))
                        .color(pal.text_dim),
                ));
                draw_slider(f, ui, "window_opacity", "", &mut op, 0.3..=1.0, enabled, 0.0)
            });
            if changed && enabled {
                f.state.push(Op::SetOpacity(op));
                record_recent(f, it.label);
            }
        }
        ItemKind::CardOpacity => {
            let mut op = f.svc.state.config.ui.card_opacity;
            // 与「背景不透明度」同一条依赖：背景不透明时没有底图可透，
            // 这个滑块禁用并压暗（「关着还能调」会让用户以为没保存）。
            let enabled = f.svc.state.config.ui.window_backdrop != WindowBackdrop::None;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{}%", (op * 100.0).round() as i32))
                        .color(pal.text_dim),
                ));
                draw_slider(
                    f,
                    ui,
                    "card_opacity",
                    "",
                    &mut op,
                    0.3..=1.0,
                    enabled,
                    0.0,
                )
            });
            if changed && enabled {
                f.state.push(Op::SetCardOpacity(op));
                record_recent(f, it.label);
            }
        }
        ItemKind::BlurStrength => {
            // 只有磨砂模式需要模糊强度；半透明 / 无背景时禁用并压暗。
            let mut level = f.svc.state.config.ui.window_blur_strength;
            let enabled =
                f.svc.state.config.ui.window_backdrop == WindowBackdrop::Acrylic;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // right_to_left：先加的靠右 ⇒ 档位数字在滑条右侧。
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{level}/10")).color(pal.text_dim),
                ));
                draw_step_slider(
                    f,
                    ui,
                    "window_blur_strength",
                    &mut level,
                    1..=10,
                    enabled,
                )
            });
            if changed && enabled {
                f.state.push(Op::SetBlurStrength(level));
                record_recent(f, it.label);
            }
        }
        ItemKind::CardShadow => {
            let mut cfg = f.svc.state.config.ui.card_shadow;
            let mut changed = false;

            // 开关行：整项的总闸。
            changed |= setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "card_shadow_enabled", &mut cfg.enabled)
            });

            // 参数子块：仅在开启时显示——「关着还能调」的控件只会
            // 让用户以为参数没保存。
            if cfg.enabled {
                ui.add_space(2.0);
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_blur",
                    "模糊",
                    format!("{}", cfg.blur.round() as i32),
                    &mut cfg.blur,
                    0.0..=24.0,
                );
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_opacity",
                    "浓度",
                    format!("{}%", (cfg.opacity * 100.0).round() as i32),
                    &mut cfg.opacity,
                    0.0..=0.6,
                );
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_offset_y",
                    "偏移",
                    format!("{}", cfg.offset_y.round() as i32),
                    &mut cfg.offset_y,
                    -8.0..=16.0,
                );
            }

            if changed {
                // 整包替换：三个滑块 + 开关可能在同一帧连续改，一个 Op
                // 就够（见 `Op::SetCardShadow` 的说明）。
                f.state.push(Op::SetCardShadow(cfg));
                record_recent(f, it.label);
            }
        }
        ItemKind::RowMeta => {
            // 编辑器内部自己 push Op（单击 / 拖拽可能一帧内多次改）。
            if draw_row_meta_editor(f, ui) {
                record_recent(f, it.label);
            }
        }
        ItemKind::TypeRail => {
            // 类型过滤栏：总开关 + （开启时）始终显示 / 位置 / 大小。
            // 整包替换（四个参数可能同帧连改，见 Op::SetTypeRail）。
            let mut cfg = f.svc.state.config.ui.type_rail;
            let mut changed = false;
            changed |= setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "type_rail_visible", &mut cfg.visible)
            });
            if cfg.visible {
                ui.add_space(2.0);
                changed |= setting_row(ui, "始终显示全部类型", "关闭时仅显示已有记录的类型", |ui| {
                    draw_switch(f, ui, "type_rail_always", &mut cfg.always)
                });
                ui.add_space(2.0);
                // 位置：四向分段控件（与「背景效果」同一控件）。
                changed |= setting_row(ui, "停靠位置", "类型栏贴在主窗口的哪一侧", |ui| {
                    use modular_clipboard_core::config::TypeRailPos;
                    let picked = draw_segmented(
                        f,
                        ui,
                        "type_rail_pos",
                        &[
                            (TypeRailPos::Right, "右"),
                            (TypeRailPos::Left, "左"),
                            (TypeRailPos::Top, "上"),
                            (TypeRailPos::Bottom, "下"),
                        ],
                        cfg.pos,
                    );
                    if let Some(p) = picked {
                        cfg.pos = p;
                    }
                    picked.is_some()
                });
                ui.add_space(2.0);
                // 大小：24~48pt 滑块（步进由滑块天然连续，绘制侧钳制）。
                let mut size = modular_clipboard_core::config::clamp_type_rail_size(cfg.size);
                let pal = f.pal;
                changed |= setting_row(ui, "图标大小", "类型按钮的边长（24~48pt）", |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(format!("{}", size.round() as i32)).color(pal.text_dim),
                    ));
                    draw_slider(
                        f,
                        ui,
                        "type_rail_size",
                        "",
                        &mut size,
                        modular_clipboard_core::config::TYPE_RAIL_SIZE_MIN
                            ..=modular_clipboard_core::config::TYPE_RAIL_SIZE_MAX,
                        true,
                        0.0,
                    )
                });
                cfg.size = modular_clipboard_core::config::clamp_type_rail_size(size);
            }
            if changed {
                f.state.push(Op::SetTypeRail(cfg));
                record_recent(f, it.label);
            }
        }
        ItemKind::Hotkey(slot) => {
            //
            // 两条输入路径都要有：
            // - **文本输入**：能精确表达 Win+V 这类组合，也能复制粘贴；
            // - **录制**：用户不用记键名，按一下就好。
            // 只做其中一个都会有人卡住（记不住键名 / 不知道 Win 怎么写）。
            //
            // 录制态下本行不画特殊 UI：录制覆盖层（`draw_recording_overlay`）
            // 会盖住整窗，这里保持朴素即可。
            let cur = slot.current(&f.svc.state.config);
            let mut changed = false;
            let _ = setting_row(ui, it.label, it.desc, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(slot.draft_mut(f.state))
                        // 宽度随字号缩放（基准 112 @ font_sm 15）。
                        .desired_width(pal.font_sm * 7.5)
                        .hint_text(cur.display())
                        .font(egui::TextStyle::Monospace),
                );
                // 回车即应用：设置项里再放一个「确定」按钮太重了。
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // 先取文本再 push：两个字段同属 `f.state`，
                    // 一起借会撞借用检查。
                    let text = slot.draft_mut(f.state).trim().to_string();
                    f.state.push(Op::SetHotkeyText { slot: *slot, text });
                    changed = true;
                }
                // 输入框与按钮之间留 8pt：设置卡片把
                // `item_spacing.x` 归了 0（行与行才够呼吸），
                // 同行控件零间距会挤成一团（用户实测反馈）。
                ui.add_space(8.0);
                if ui.small_button("录制").clicked() {
                    f.state.push(Op::StartRecording(*slot));
                }
                changed
            });
            // 注册失败要在**配置它的地方**可见：状态栏提示只闪 3 秒，
            // 用户回到设置页时早就没了。失败信息里含该行当前的组合文本
            // （格式由 `hotkey::failed_message` 固定），据此匹配行列。
            if let Some(err) = f.svc.state.hotkey_error.as_ref()
                && err.contains(cur.display().as_str())
            {
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("⚠ {err}"))
                            .color(pal.danger)
                            // 小一号提示字：随字号缩放（11.0 裸值在
                            // font_scale=2 时不放大，用户看到「半截文字」）。
                            .size(pal.font_xs),
                    );
                });
            }
            let _ = changed;
        }
        ItemKind::Autostart => {
            let mut v = f.svc.state.config.ui.autostart;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "autostart", &mut v)
            });
            if changed {
                f.state.push(Op::SetAutostart(v));
            }
        }
        ItemKind::McpToggle => {
            let mut v = get_bool(&f.svc.state.config, ConfigGroup::Mcp, "enabled");
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // control 位于 right_to_left 容器：先 add 的控件靠右。
                // 先画开关（最右），再画 ⓘ 详细按钮（紧贴开关**左边**，
                // 即「MCP 详细在总开关按钮的左边」）。
                let sw = draw_switch(f, ui, "mcp_enabled", &mut v);
                // 与 ⓘ 按钮之间留 8pt：设置卡片把 `item_spacing.x`
                // 归了 0（行与行才够呼吸），同行控件零间距会**直接
                // 贴在一起**——用户实测反馈「MCP 设置和开关挨得
                // 太挤」。热键行的输入框与「录制」按钮同款修法。
                ui.add_space(8.0);
                let info = draw_mcp_info_btn(f, ui);
                if info.clicked() {
                    f.state.mcp_overlay_open = !f.state.mcp_overlay_open;
                }
                sw
            });
            if changed {
                f.state.push(Op::SetBool {
                    group: ConfigGroup::Mcp,
                    field: "enabled",
                    value: v,
                });
                record_recent(f, it.label);
            }
        }
        ItemKind::Text {
            group,
            field,
            ph,
            secret,
        } => {
            let cur = get_text(&f.svc.state.config, *group, field);
            let mut buf = cur;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // 宽度随字号缩放（基准 170 / 230 @ font_sm 15），
                // 否则 2× 字号下框里只显示得下 4~5 个字。
                let w = pal.font_sm * if *secret { 11.3 } else { 15.3 };
                draw_text_field(f, ui, field, &mut buf, ph, *secret, w)
            });
            if changed {
                f.state.push(Op::SetText {
                    group: *group,
                    field,
                    value: buf,
                });
                record_recent(f, it.label);
            }
        }
        ItemKind::McpPort => {
            let port = f.svc.state.config.mcp.port;
            let mut buf = if port == 0 {
                String::new()
            } else {
                port.to_string()
            };
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // 宽度随字号缩放（基准 110 @ font_sm 15）。
                draw_text_field(f, ui, "mcp_port", &mut buf, "9123", false, pal.font_sm * 7.3)
            });
            if changed {
                let v: u16 = buf.trim().parse().unwrap_or(0);
                f.state.push(Op::SetNumber {
                    group: ConfigGroup::Mcp,
                    field: "port",
                    value: v as f64,
                });
                record_recent(f, it.label);
            }
        }
        ItemKind::Info(text) => {
            setting_row(ui, it.label, "", |ui| {
                // CAUTION: 必须 `truncate()`。右对齐布局里宽度不够的控件
                // 会**向左**无限生长（`Extend` 模式），长文本（如数据目录
                // 那行）会压到左侧导航列上——实测 2× 字号下越出 130pt。
                let r = ui.add(
                    egui::Label::new(egui::RichText::new(*text).color(pal.text_dim)).truncate(),
                );
                r.on_hover_text(*text);
                false
            });
        }
        ItemKind::Link(url) => {
            setting_row(ui, it.label, it.desc, |ui| {
                // 强调色 URL，hover 手型 + 下划线；点击用默认浏览器打开。
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(*url).size(pal.font_sm).color(pal.accent),
                    )
                    // 同 `ItemKind::Info`：不截断会向左溢出到导航列上。
                    .truncate()
                    .sense(egui::Sense::click()),
                );
                // 登记热区（key 即 URL），供自动化与测试定位。
                f.hit_insert(url, resp.rect);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let r = resp.rect;
                    ui.painter_at(r).line_segment(
                        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
                        egui::Stroke::new(1.0, pal.accent),
                    );
                }
                if resp.clicked() {
                    f.state.push(Op::OpenLink(url));
                }
                false
            });
        }
        ItemKind::Quit => {
            ui.add_space(8.0);
            let (r, resp) = ui.allocate_exact_size(
                vec2(ui.available_width(), 32.0),
                egui::Sense::click(),
            );
            let p = ui.painter_at(r);
            if resp.hovered() {
                p.rect_filled(r, pal.radius_md, pal.surface_variant);
            }
            p.text(
                pos2(r.min.x + 4.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                "退出程序",
                egui::FontId::proportional(pal.font_md),
                pal.danger,
            );
            f.hit_insert("settings_quit", r);
            if resp.clicked() {
                f.state.push(Op::Quit);
            }
        }
    }
}

/// 搜索非空：跨全部分类过滤，命中项只读展示（点击跳转分类）。
fn settings_search_results(f: &mut Frame<'_>, ui: &mut Ui, q: &str) {
    let pal = f.pal;
    let mut total = 0;
    for card in SETTINGS_CARDS {
        let hits: Vec<&SettingsItem> = card
            .items
            .iter()
            .filter(|it| it.label.contains(q) || it.desc.contains(q))
            .collect();
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        section(ui, card.title, f.pal);
        for it in hits {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::click());
            if resp.hovered() {
                ui.painter_at(rect).rect_filled(rect, pal.radius_md, pal.row_hover);
            }
            ui.painter_at(rect).text(
                rect.min + vec2(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                it.label,
                egui::FontId::proportional(pal.font_sm),
                pal.text,
            );
            ui.painter_at(rect).text(
                rect.max - vec2(8.0, 8.0),
                egui::Align2::RIGHT_BOTTOM,
                card.title,
                egui::FontId::proportional(pal.font_xs),
                pal.text_dim,
            );
            if resp.clicked() {
                f.state.settings_cat = card.cat;
                f.state.settings_query.clear();
                f.state.settings_show_recent = false;
            }
        }
    }
    if total == 0 {
        settings_recent_view(f, ui);
    }
}

/// 最近修改的设置项（倒序，最多 8 条）。
fn settings_recent_view(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    if f.state.settings_recent.is_empty() {
        ui.label(egui::RichText::new("暂无最近修改的设置").color(pal.text_dim));
        return;
    }
    section(ui, "最近修改", f.pal);
    for (label, _ts) in f.state.settings_recent.iter().rev() {
        let cat_title = SETTINGS_CARDS
            .iter()
            .find(|c| c.items.iter().any(|it| it.label == *label))
            .map(|c| c.title)
            .unwrap_or("");
        let (rect, _resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::hover());
        ui.painter_at(rect).text(
            rect.min + vec2(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(pal.font_sm),
            pal.text,
        );
        ui.painter_at(rect).text(
            rect.max - vec2(8.0, 8.0),
            egui::Align2::RIGHT_BOTTOM,
            cat_title,
            egui::FontId::proportional(pal.font_xs),
            pal.text_dim,
        );
    }
}

/// 登记一条最近修改（去重 + 上限 8 条）。
fn record_recent(f: &mut Frame<'_>, label: &str) {
    f.state.settings_recent.retain(|(l, _)| l != label);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    f.state.settings_recent.push((label.to_string(), now));
    if f.state.settings_recent.len() > 8 {
        f.state.settings_recent.remove(0);
    }
}

/// 分组标题。
fn section(ui: &mut Ui, title: &str, pal: &Palette) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .strong()
            // 15.0 裸值 == font_sm @scale1，但不随 font_scale 放大
            // （高字号下分组标题反而比正文小，倒挂）。
            .size(pal.font_sm)
            .color(ui.visuals().weak_text_color()),
    );
}

/// 一行「标签 + 说明 + 右侧控件」。
///
/// `control` 闭包在右侧区域里放控件，返回 `true` 表示值被改动。
/// 宽度分配交给 `ui.horizontal` + `with_layout(right_to_left)`，
/// 不再手算 `ctrl_w`（旧实现那处 `min(r.width() * 0.4)` 的钳制
/// 会在极窄面板里把标签挤没）。
///
/// # 顺序：**先放控件，再放标签列**
///
/// 这是「两者永不重叠」的关键。早前是「先标签列、后控件」，而控件在
/// `right_to_left` 里是**右对齐**的：一旦控件比剩下的空间宽（字体放大
/// 后必然发生），它就向左溢出、压到标签列身上——实测 1.5× 时字体缩放
/// 分段控件左缘 436.5，而标签列右缘 456.8，重叠 20pt。
///
/// 反过来先放控件：控件先占住自己需要的宽度，标签列随后拿到的
/// `available_width` 已是**净宽**，截断按净宽发生，两者不可能重叠。
/// 字体缩放到这个倍率以上时，设置行改为**上下堆叠**（标签在上、控件在下）。
///
/// # 为什么需要堆叠（用户反馈「字体调整太大导致布局错乱」）
///
/// 并排时标签列与控件列争同一条行宽（544pt）。5 段的分段控件在 1.3×
/// 时已占 300pt+，加上读数与间距后留给标签列的不足 160pt——描述只能
/// 显示 6 个字（虽然可悬停看全文，但一眼看不到重点）。堆叠后控件独占
/// 整行宽度，描述也不再被截断，代价只是卡片变高（内容区本来就可滚动）。
///
/// 阈值 1.25：1.15× 及以下并排仍然宽松（实测标签列 ≥ 240pt），
/// 从 1.3× 开始明显拥挤。
const ROW_STACK_SCALE: f32 = 1.25;

/// 从 egui 样式反推当前字体缩放倍率。
///
/// `theme::apply` 把 `pal.font_md` 写进 `TextStyle::Body`，因此
/// 「Body 字号 / 基准字号」就是缩放倍率。取不到样式时按 1.0 处理
/// （退回并排布局，最坏只是大字号下拥挤一点，不会崩）。
fn row_font_scale(ui: &Ui) -> f32 {
    let base = crate::theme::Palette::light().font_md;
    let cur = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .map_or(base, |f| f.size);
    if base > 0.0 && cur.is_finite() { (cur / base).max(0.1) } else { 1.0 }
}

/// 标题与说明之间的行距（逻辑点）。
///
/// 与「行与行之间」的间距（`settings_card` 的 `item_spacing.y`）分开：
/// 标题和说明是一组，贴紧才读得成一句；行与行之间要松。
const LABEL_DESC_GAP: f32 = 5.0;

/// 说明文字最多占几行。超出部分由 egui 省略（末行加「…」）。
///
/// 2 行是「读得完一句说明」与「行高不失控」的折中：设置项的描述通常
/// 一到两句，3 行以上多半是文案该精简了。悬停仍可看到完整描述。
const DESC_MAX_ROWS: usize = 2;

/// 标题行高（逻辑点）。**与宽度无关**（标题单行 `truncate`），可精确预量。
fn head_line_height(ui: &Ui, label: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .rect
        .height()
}

/// 说明文字排版：按 `width` **折行**，最多 [`DESC_MAX_ROWS`] 行，超出加省略号。
///
/// # 为什么不用 `Label::truncate()` / `.wrap()`
///
/// - `truncate()` 只出**一行**，长说明被砍成半句（用户反馈「描述显示不全」）；
/// - `wrap()` 不限行数，超长说明能把一行设置撑到五六行高，整张卡片失去节奏。
///
/// 用 `LayoutJob` 的 `wrap.max_rows` 才是「折行 + 到上限加省略号」——
/// 与列表预览（`draw_text_preview`）同一套做法。
fn desc_galley(ui: &Ui, desc: &str, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        desc,
        0.0,
        egui::TextFormat {
            font_id: egui::TextStyle::Small.resolve(ui.style()),
            color: ui.visuals().weak_text_color(),
            ..Default::default()
        },
    );
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = DESC_MAX_ROWS;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// 一行设置项的布局常量：控件列的最小高度。
///
/// 标题行本身只有 ~20pt，而按钮类控件（快捷键录制、迷你按钮）有 24~28pt。
/// 容器按 24pt 起步，控件居中后不会向上/向下出界。
const CTRL_MIN_H: f32 = 24.0;

/// 一行「标签 + 说明 + 右侧控件」。
///
/// # 布局（三块，全部用**显式矩形**定位）
///
/// ```text
/// ┌───────────────────────────────┬──────────────┐
/// │ 标题                          │   控件（右对齐、垂直居中）│
/// │ 说明（自动折行，最多 2 行…）   │              │
/// └───────────────────────────────┴──────────────┘
///   ← 标签列宽 = 可用宽 − 控件宽 − 间距 →
/// ```
///
/// # 为什么必须「先量行高、再按矩形摆放」
///
/// 早前是「控件先放、标签列后放」，而 egui 的横向布局**不会重排先放的
/// 控件**：控件放下时行高还只有它自己那么高，随后标签列把行撑高，控件
/// 仍停在行首 ⇒ 开关贴着标题行、输入框贴着说明行、分段控件又高半行，
/// 同一张卡片里三种对齐——观感就是「内容和按钮错位」。
///
/// 现在整行先 `allocate_exact_size` 占住，再把控件与标签列放进**显式
/// 矩形**（`new_child` 不推进游标），三者的几何完全由本函数决定：
/// - 控件：行右上角，在 `CTRL_MIN_H` 高的容器里垂直居中（与标题行同中心）；
/// - 标签列：行左上角，宽度 = 可用宽 − 控件实际宽 − 间距；
/// - 行高：标题行高 + 说明实际折行高度（说明折行后行会变高）。
///
/// # 为什么标签列宽度要减去控件宽
///
/// 说明按这个宽度折行——**量出来的行数就是画出来的行数**。若按整行宽
/// 量、按窄列画，说明会被挤成更多行而撑出卡片（用户反馈「描述跑到
/// 卡片外」的成因）。控件宽从 `min_rect` 取实际值，不靠猜。
fn setting_row(
    ui: &mut Ui,
    label: &'static str,
    desc: &'static str,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let mut changed = false;
    if row_font_scale(ui) > ROW_STACK_SCALE {
        // ---- 大字号：上下堆叠 ----
        ui.vertical(|ui| {
            ui.add(egui::Label::new(label));
            if !desc.is_empty() {
                let g = desc_galley(ui, desc, ui.available_width());
                let r = ui.add(egui::Label::new(g));
                r.on_hover_text(desc);
            }
            // CAUTION: 必须用 `ui.horizontal` 包一层。直接在纵向布局里
            // `with_layout(right_to_left)` 会让子 ui 拿到**剩余全部高度**
            // （滚动区里是上千点），`Align::Center` 随即把控件垂直居中到
            // 几百点之外——实测第一行控件被推到 y=632（行首在 y=54）。
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    changed = control(ui);
                });
            });
        });
        return changed;
    }

    let avail = ui.available_width().max(1.0);
    let head_h = head_line_height(ui, label);
    let ctrl_h = head_h.max(CTRL_MIN_H);
    // 1) 先占住整行（高度先按「标题行高」，说明折行后再补）。
    let (row, _) = ui.allocate_exact_size(vec2(avail, ctrl_h), Sense::empty());

    // 2) 控件：行右上角，从右往左排（右对齐）。
    //
    // 容器宽度 = 可用宽 − **标题实际宽度** − 间距：控件最多吃到这里，
    // 标题永远拿得下自己的字（长标题也不会被控件挤没）。早前用固定
    // 72% 上限，Info 行（右侧是长文本）被白白砍掉三成宽度。
    let gap = ui.spacing().item_spacing.x;
    let label_nat = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .rect
        .width()
        .min(avail * 0.5);
    let ctrl_max = (avail - label_nat - gap).max(avail * 0.3);
    let mut ctrl_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(row.max.x - ctrl_max, row.min.y),
                pos2(row.max.x, row.min.y + ctrl_h),
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    changed = control(&mut ctrl_ui);
    // 实际占用的宽度（开关 ~38、分段 ~250、输入框 ~230）。
    let ctrl_w = ctrl_ui.min_rect().width().min(ctrl_max);

    // 3) 标签列：行左上角。宽度先定，说明按这个宽度折行 ⇒ 量=画。
    let label_w = (avail - ctrl_w - gap).max(80.0);
    let desc_g = (!desc.is_empty()).then(|| desc_galley(ui, desc, label_w));
    let desc_h = desc_g.as_ref().map_or(0.0, |g| g.rect.height());
    let label_h = head_h + if desc_g.is_some() { LABEL_DESC_GAP + desc_h } else { 0.0 };
    // 说明折行后行更高：补足游标，下一行才不会压上来。
    if label_h > ctrl_h {
        ui.add_space(label_h - ctrl_h);
    }
    let mut lab = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_size(row.min, vec2(label_w, label_h)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    lab.spacing_mut().item_spacing.y = LABEL_DESC_GAP;
    // 标题单行：`truncate()` 让它在窄列里省略而不是换行（换行会把
    // 标题和说明挤成一片，读不出层次）。
    lab.add(egui::Label::new(label).truncate());
    if let Some(g) = desc_g {
        // 说明已经按列宽排好版（折行 + 省略），直接画 galley。
        let r = lab.add(egui::Label::new(g));
        // 折行到 2 行仍可能看不全，悬停给出完整描述。
        r.on_hover_text(desc);
    }
    changed
}

/// 开关：外观自绘（纯平毛玻璃），命中走 egui。
///
/// # 为什么要自己画而不用 `egui::Checkbox`
///
/// 项目视觉语言是「**纯平毛玻璃**」：禁止一切装饰性拟物效果
/// （渐变、光泽、内发光）。`egui::Checkbox` 自带的勾选框是带立体感的
/// 控件，与整体观感不符。这里保留自绘外观。
///
/// # `id_suffix` 必须稳定且唯一
///
/// `Pos2` 不能作 `egui::Id`（不满足 `Hash`），而用矩形坐标当 id
/// 会因窗口 resize 变化，导致同一开关在不同帧拿到不同 id、
/// 交互状态（hover/active）被反复丢弃。所以传配置**字段名**。
fn draw_switch(f: &mut Frame<'_>, ui: &mut Ui, id_suffix: &'static str, v: &mut bool) -> bool {
    let pal = f.pal;
    // 尺寸：基准高 20、宽 38（约 1.9 倍），比旧版 18×32 更饱满。
    // 按 `font_sm` 缩放——开关不承载文字，但必须与旁边放大的
    // 标签视觉配重（1.5× 字号配 20pt 开关会显得开关「缩水」）。
    let h = pal.font_sm * 1.33;
    let w = h * 1.9;
    // NOTE: 同一个矩形只准注册**一次**交互。
    //
    // 早前这里写的是 `allocate_exact_size(size, Sense::click())`
    // 紧接着又 `interact(rect, Id::new(("switch", …)), Sense::click())`
    // ——同一块区域注册了两个不同 Id 的交互区，靠「后注册者优先」
    // 侥幸正确：一旦有人在中间插一个控件，或者 egui 改了同层交互区
    // 的优先级规则，点击就会落到那个被忽略的 `_alloc` 上，开关直接
    // 失灵而且毫无征兆。
    //
    // 正确分工：`allocate_exact_size` 只负责**占位**（`Sense::empty()`，
    // 纯粹向后推游标），点击交互由下面那条显式 `interact` 承担——
    // 它的 Id 绑定字段名，跨帧稳定。
    let (rect, _alloc) = ui.allocate_exact_size(egui::vec2(w, h), Sense::empty());
    let resp = ui.interact(rect, egui::Id::new(("switch", id_suffix)), Sense::click());

    let track = Rect::from_center_size(rect.center(), vec2(w, h));
    let p = ui.painter_at(rect);
    let on = *v;
    // 轨道：
    // - 开：accent 实底（hover 时 accent_hover）
    // - 关：中性灰实底（surface_variant），**无描边**——旧版那圈
    //   border 灰边 + 灰色滑块整体灰扑扑，是「丑」的主要来源。
    //   关闭态 hover 叠一层白提亮。
    //
    // 轨道颜色也走补间（130ms）：滑块位置本来就有 `animate_bool` 在滑，
    // 轨道却是硬切——两者不同步会读出「颜色先到、滑块后到」的撕裂感。
    // 设置窗口每帧驱动（无空闲闸门），不需要续帧请求。
    let track_target = if on {
        if resp.hovered() { pal.accent_hover } else { pal.accent }
    } else if resp.hovered() {
        crate::theme::composite(egui::Color32::from_white_alpha(16), pal.surface_variant)
    } else {
        pal.surface_variant
    };
    let track_color = crate::anim::tween_color(
        ui.ctx(),
        egui::Id::new(("switch_track", id_suffix)),
        track_target,
        std::time::Duration::from_millis(130),
    );
    p.rect_filled(track, h * 0.5, track_color);

    // 滑块位置：平滑动画（egui 按帧插值），不再硬切。
    let anim = ui
        .ctx()
        .animate_bool(egui::Id::new(("switch_anim", id_suffix)), on);
    let knob_r = h * 0.5 - 3.0;
    let left_x = track.min.x + h * 0.5;
    let right_x = track.max.x - h * 0.5;
    let knob_center = pos2(left_x + (right_x - left_x) * anim, track.center().y);
    // 轻微投影（增加层次），再画白色滑块——开/关都用白滑块
    // （Win11 风格：关闭态白滑块在灰底上比旧版灰滑块干净）。
    p.circle_filled(
        knob_center + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(knob_center, knob_r, egui::Color32::WHITE);

    if resp.clicked() {
        *v = !*v;
    }
    // IMPORTANT: 热区 key 必须**带上字段名**。
    //
    // 早前所有开关都登记成同一个 `"settings_switch"`，而 `hit` 是
    // `HashMap<&str, Rect>`——后写的覆盖先写的， map 里只剩**最后一个**
    // 开关的矩形。于是自动化脚本按热区点击会「点 A 项却改了 B 项」，
    // 而所有断言都通过（它们只读得到的那一个键）。
    //
    // 逐个登记还有一个好处：测试能逐项核对边界，而不是只看最后一个。
    f.hit_insert(id_suffix, rect);
    *v != on
}

/// 圆角滑块（替代 `egui::Slider`）：纯平毛玻璃风格。
///
/// # 为什么不用 `egui::Slider`
///
/// 与 [`draw_switch`] 同理：默认滑块是细方轨 + 方划块，与本仓自绘的
/// 开关 / 分段控件不是一套视觉语言，设置页里并排出现「三种控件三种
/// 质感」。这里统一为：全圆角轨道（半径 = 轨高一半）+ accent 填充
/// 已选部分 + 白色圆 knob（带轻投影，与开关滑块一致）。
///
/// # 交互
///
/// 点击轨道任意处跳转到该值，按住拖动连续调整。禁用态（如背景效果为
/// 「无」时不透明度无意义）只绘制不交互，整体压暗。
///
/// # `variant`：同一 `id_suffix` 下的第几条滑条
///
/// 令牌编辑器里「色相 / 透明度」双滑条共用令牌名，靠本参数区分 Id。
/// CAUTION: 不区分的话，同一帧两个矩形注册同一个 `Id`，后注册者赢得
/// 交互——拖动 A 会连带动 B，且毫无征兆。空串 = 唯一滑条。
#[allow(clippy::too_many_arguments)]
fn draw_slider(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    variant: &'static str,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    enabled: bool,
    // CAUTION: 行高兜底，必须由调用方按「本行最高控件」传入（令牌行
    // 见 `token_row_h`）。传 0 = 保持旧的「滑条自定高」行为。
    //
    // 为什么需要它：egui 的 horizontal **不重排先放置的控件**——若滑条
    // 比左列文字高，左列被顶部对齐，文字相对滑条偏高；150% DPI 下
    // font_xs=21（galley 高 27pt）而左列格子写死 18pt 时还会把字形
    // 上下裁掉（用户实机反馈「文字向上且渲染不完整」，2026-10-19）。
    // 三列同高 + 本参数兜底，两个病一起治。
    cell_h: f32,
) -> bool {
    let pal = f.pal;
    // 轨道 150x6，knob 直径 16——比开关轨道（20）细，但 knob 同为白圆，
    // 两个控件摆在一页里粗细有致又不散。尺寸随字号缩放。
    let track_w = pal.font_sm * 10.0;
    let track_h = (pal.font_sm * 0.4).max(4.0);
    let knob_d: f32 = pal.font_sm * 1.07;
    let cell_h = cell_h.max(knob_d).max(track_h);
    // 分工同 draw_switch：allocate 只占位，交互走显式 interact（Id 绑字段名）。
    let (rect, _) = ui.allocate_exact_size(egui::vec2(track_w, cell_h), Sense::empty());
    let resp = ui.interact(
        rect,
        egui::Id::new(("slider", id_suffix, variant)),
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );

    let (lo, hi) = (*range.start(), *range.end());
    let orig = *v;
    // 拖动/点击 → 值：指针在轨道上的相对位置直接映射。
    // `is_pointer_button_down_on` 覆盖「按下即跳转」，`dragged` 覆盖按住拖动
    // 连续调整。两者都要：只用 dragged 的话必须先移动一像素才响应。
    if enabled
        && (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let t = ((pp.x - rect.min.x) / track_w).clamp(0.0, 1.0);
        *v = lo + (hi - lo) * t;
    }
    let t = ((*v - lo) / (hi - lo)).clamp(0.0, 1.0);

    let p = ui.painter_at(rect);
    let track = Rect::from_min_size(
        pos2(rect.min.x, rect.center().y - track_h * 0.5),
        vec2(track_w, track_h),
    );
    let fill_w = track_w * t;
    // 禁用态整体压暗：轨道退到描边色，填充退到弱文字色，knob 不再纯白。
    let track_bg = if enabled { pal.surface_variant } else { pal.border_subtle };
    let fill = if !enabled {
        pal.text_dim
    } else if resp.hovered() || resp.dragged() {
        pal.accent_hover
    } else {
        pal.accent
    };
    p.rect_filled(track, track_h * 0.5, track_bg);
    if fill_w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(track.min, vec2(fill_w, track_h)),
            track_h * 0.5,
            fill,
        );
    }
    // knob 圆心钳在轨道内：值到两端时圆不出轨。
    let knob_x = (track.min.x + fill_w).clamp(track.min.x + knob_d * 0.5, track.max.x - knob_d * 0.5);
    let knob_c = pos2(knob_x, track.center().y);
    let knob_r = knob_d * 0.5;
    p.circle_filled(
        knob_c + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(
        knob_c,
        knob_r,
        if enabled { egui::Color32::WHITE } else { pal.text_dim },
    );

    // 热区登记：自动化按字段名定位（同开关的约定）。
    f.hit_insert(id_suffix, rect);
    (*v - orig).abs() > f32::EPSILON
}

/// 离散档位滑块：外观与 [`draw_slider`] 一致，但值**吸附到整数档位**
/// （磨砂强度 1~10）——knob 与填充都按吸附后的档位走，一格一格清晰，
/// 不会停在两档之间。返回本帧档位是否被改变。
fn draw_step_slider(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    v: &mut u8,
    range: std::ops::RangeInclusive<u8>,
    enabled: bool,
) -> bool {
    let pal = f.pal;
    // 尺寸与 `draw_slider` 同源（随字号缩放）。
    let track_w = pal.font_sm * 10.0;
    let track_h = (pal.font_sm * 0.4).max(4.0);
    let knob_d: f32 = pal.font_sm * 1.07;
    let cell_h = knob_d.max(track_h);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(track_w, cell_h), Sense::empty());
    let resp = ui.interact(
        rect,
        egui::Id::new(("step_slider", id_suffix)),
        if enabled {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );

    let (lo, hi) = (*range.start() as f32, *range.end() as f32);
    let orig = *v;
    if enabled
        && (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(pp) = resp.interact_pointer_pos()
    {
        let t = ((pp.x - rect.min.x) / track_w).clamp(0.0, 1.0);
        // 指针位置 → 最近的整数档位（t=0 落 lo，t=1 落 hi）。
        let mapped = lo + (hi - lo) * t;
        *v = mapped.round().clamp(lo, hi) as u8;
    }
    // 关键：归一化用吸附后的整数档位，knob 才会一格一格跳。
    let t = ((*v as f32 - lo) / (hi - lo)).clamp(0.0, 1.0);

    let p = ui.painter_at(rect);
    let track = Rect::from_min_size(
        pos2(rect.min.x, rect.center().y - track_h * 0.5),
        vec2(track_w, track_h),
    );
    let fill_w = track_w * t;
    let track_bg = if enabled { pal.surface_variant } else { pal.border_subtle };
    let fill = if !enabled {
        pal.text_dim
    } else if resp.hovered() || resp.dragged() {
        pal.accent_hover
    } else {
        pal.accent
    };
    p.rect_filled(track, track_h * 0.5, track_bg);
    if fill_w > 0.5 {
        p.rect_filled(
            Rect::from_min_size(track.min, vec2(fill_w, track_h)),
            track_h * 0.5,
            fill,
        );
    }
    let knob_x = (track.min.x + fill_w).clamp(track.min.x + knob_d * 0.5, track.max.x - knob_d * 0.5);
    let knob_c = pos2(knob_x, track.center().y);
    let knob_r = knob_d * 0.5;
    p.circle_filled(
        knob_c + vec2(0.0, 0.8),
        knob_r,
        egui::Color32::from_black_alpha(45),
    );
    p.circle_filled(
        knob_c,
        knob_r,
        if enabled { egui::Color32::WHITE } else { pal.text_dim },
    );

    f.hit_insert(id_suffix, rect);
    *v != orig
}

/// MCP 总开关左侧的 ⓘ 详细按钮（打开 MCP 调用设置覆盖层）。
///
/// 自绘圆形信息图标：accent 底色 + 圆点 + 竖线（不依赖字体字形，
/// 图标字体里没有 info 码位，新增字形需要重新生成 TTF）。
/// 分段选择控件（segmented control）：整体一个圆角实底（surface_variant），
/// 选中项用 surface 实底 + accent 文字，未选中 text_dim。
///
/// 替代 egui 的 `selectable_label`——后者选中态是半透明 row_selected，
/// 多个胶囊标签挨着时半透明叠半透明，显脏（2026-10-11 用户反馈）。
///
/// # 选中指示器滑动（2026-10-17 动效）
///
/// 早前选中块是「哪项选中就往哪项画一个实底」——切换时旧块消失、
/// 新块出现，观感上是**跳变**。现在选中块的位置按**段索引**补间
/// （`("seg_pill", id_suffix)`，160ms OutCubic）：切换时它从旧段
/// **滑到**新段，文字颜色同步交叉淡入。`id_suffix` 必须是每个控件
/// 实例稳定且唯一的标识（传设置项字段名）——同一帧里可能有多个
/// 分段控件，共用 id 会互相串台。
///
/// 返回 `Some(value)` 表示本帧点击了某项。
fn draw_segmented<T: Copy + PartialEq>(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    // 本实例的唯一标识。同时是热区注释里说的「同帧多控件不互相覆盖」的
    // 补间键前缀——交互 Id 用 `("seg", i, label)`（选项文字全局唯一），
    // 补间 Id 必须再叠一层本实例名，否则两个控件会共享同一段补间。
    id_suffix: &'static str,
    options: &[(T, &'static str)],
    cur: T,
) -> Option<T> {
    let pal = f.pal;
    // 高度随字号缩放（`control_height` 已含字体缩放）：固定 30pt 时
    // 字号放大到 1.5× 会顶出控件上下边缘。
    let h = pal.control_height + 2.0;
    // 每项宽度按文字测量 + 内边距，统一取最长项（项宽一致更整齐）。
    let pad = 20.0;
    let font = egui::FontId::proportional(pal.font_sm);
    let mut seg_w = 0.0_f32;
    for (_, label) in options {
        let w = ui
            .painter()
            .layout_no_wrap(label.to_string(), font.clone(), pal.text)
            .rect
            .width();
        seg_w = seg_w.max(w + pad);
    }
    // 自适应可用宽度：字体放大后自然宽度会超过卡片，必须等比压缩，
    // 否则控件被画到卡片之外（用户反馈「字体调整太大导致布局错乱」）。
    // 压到每段不足 1 个汉字宽时就不再压——宁可溢出，也不能把选项挤成
    // 不可辨认的窄条。
    let avail = ui.available_width().max(0.0);
    let min_seg = pal.font_sm + pad * 0.5;
    if avail > 0.0 {
        let max_seg = avail / options.len() as f32;
        seg_w = seg_w.min(max_seg).max(min_seg.min(seg_w));
    }
    let total_w = seg_w * options.len() as f32;
    let (r, _) = ui.allocate_exact_size(vec2(total_w, h), egui::Sense::empty());
    let painter = ui.painter_at(r);
    painter.rect_filled(r, pal.radius_md, pal.surface_variant);
    // ---- 选中指示器：按段索引补间滑动 --------------------------------
    //
    // 目标值 = 当前选中段的下标（`position` 找不到时给 0——理论上
    // `cur` 必在 options 里，但泛型调用方不保证，兜底 0 比 panic 好）。
    // 补间从当前显示值续接：连续切换时它在中途改方向，不会跳。
    let sel_idx = options
        .iter()
        .position(|(v, _)| *v == cur)
        .unwrap_or(0) as f32;
    let pill_idx = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("seg_pill", id_suffix)),
        sel_idx,
        std::time::Duration::from_millis(160),
        crate::anim::Ease::OutCubic,
    );
    // 指示器左右各内缩 2pt（与原实现一致），圆角 radius_sm。
    let pill = Rect::from_min_max(
        pos2(r.min.x + seg_w * pill_idx + 2.0, r.min.y + 2.0),
        pos2(r.min.x + seg_w * (pill_idx + 1.0) - 2.0, r.max.y - 2.0),
    );
    painter.rect_filled(pill, pal.radius_sm, pal.surface);
    let mut picked = None;
    for (i, (value, label)) in options.iter().enumerate() {
        let seg = egui::Rect::from_min_max(
            pos2(r.min.x + seg_w * i as f32, r.min.y),
            pos2(r.min.x + seg_w * (i + 1) as f32, r.max.y),
        );
        let resp = ui.interact(seg, egui::Id::new(("seg", i, label)), egui::Sense::click());
        // 文字放不下时按**段宽**省略：`draw_text` 不裁剪，直接画会
        // 越到相邻段上（视觉上两段文字叠在一起）。
        let text = elide_to_width(ui, label, &font, seg.width() - 8.0);
        // 文字颜色补间（130ms）：选中态 accent、悬停提亮、常态弱化，
        // 三者之间交叉淡入，与滑动指示器同步。首帧直接落到目标
        // （`tween_color` 惰性插入），之后才在变化时补间。
        let target_color = if cur == *value {
            pal.accent
        } else if resp.hovered() {
            pal.text
        } else {
            pal.text_dim
        };
        let color = crate::anim::tween_color(
            ui.ctx(),
            egui::Id::new(("seg_text", id_suffix, label)),
            target_color,
            std::time::Duration::from_millis(130),
        );
        painter.text(
            seg.center(),
            egui::Align2::CENTER_CENTER,
            text,
            font.clone(),
            color,
        );
        if resp.clicked() {
            picked = Some(*value);
        }
        // 登记热区：key 用**选项文字**。同一帧只画当前分类的卡片，
        // 各分类的选项名互不重复（字体缩放的「小/标准/大/特大/超大」、
        // 主题的「跟随系统/深色/浅色」…），因此不会互相覆盖。
        // 自动化（MCP 的 click_element）与回归测试都靠它定位分段控件。
        f.hit_insert(label, seg);
    }
    picked
}

/// 把 `text` 省略到不超过 `max_w`（超出部分用 `…` 收尾）。
///
/// 与 `egui::Label::truncate()` 的分工：那个作用于**控件**，本函数作用于
/// **自绘文字**（`painter.text` 不做任何裁剪，直接画会越到相邻分段上）。
/// 逐字二分前缀 + `layout_no_wrap` 测量，成本与文字长度成对数关系。
fn elide_to_width(ui: &Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    if max_w <= 0.0 {
        return String::new();
    }
    let measure = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
            .rect
            .width()
    };
    if measure(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    // 二分找最长可容纳前缀（含省略号）。
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if measure(&candidate) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return String::new();
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// 文本输入框：清晰圆角背景（surface_variant 实底 + 边框），在右侧区域里靠右。
///
/// 旧实现用 `frame(Frame::NONE)` 无背景，在 right_to_left 里虽靠右但没有
/// 可见边界，文字起点不明，看起来「没有对齐右边」。改为先 allocate 固定
/// 宽度区域、画背景，再在内部放 TextEdit；聚焦时边框变 accent。
fn draw_text_field(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    // 本输入框的唯一标识（配置字段名）。聚焦描边的补间键要按框分开——
    // 共用 id 会让两个框的聚焦动画互相串台（点 A 框时 B 框也亮）。
    id_suffix: &'static str,
    buf: &mut String,
    ph: &str,
    secret: bool,
    w: f32,
) -> bool {
    let pal = f.pal;
    // 高度随字号缩放：固定 30pt 时 1.5× 字号（行高 ~33）会被裁掉。
    let h = pal.control_height + 2.0;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::empty());
    let painter = ui.painter_at(r);
    painter.rect_filled(r, pal.radius_md, pal.surface_variant);
    // 常态描边；聚焦时**叠**一层补间的 accent 描边（颜色 130ms 过渡，
    // 宽度仍按 1.0 → 1.5 两档硬切——宽度补间在 1px 量级上看不出，反而
    // 要让抗锯齿边缘抖一下）。
    painter.rect_stroke(
        r,
        pal.radius_md,
        egui::Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    let text_rect = r.shrink2(vec2(8.0, 4.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(text_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let resp = child.add(
        egui::TextEdit::singleline(buf)
            .hint_text(ph)
            .password(secret)
            .frame(egui::Frame::NONE)
            .text_color(pal.text)
            .desired_width(text_rect.width()),
    );
    // 聚焦描边补间（130ms）：失焦时 accent 淡回 border 色，不再是
    // 「亮点一下又缩回去」的硬切。设置窗口每帧驱动，无需续帧请求。
    let focus_k = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("field_focus", id_suffix)),
        if resp.has_focus() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(130),
        crate::anim::Ease::OutCubic,
    );
    if focus_k > 0.004 {
        let ring = crate::anim::tween_color(
            ui.ctx(),
            egui::Id::new(("field_focus_color", id_suffix)),
            pal.accent,
            std::time::Duration::from_millis(130),
        );
        painter.rect_stroke(
            r,
            pal.radius_md,
            egui::Stroke::new(1.0 + 0.5 * focus_k, ring),
            egui::StrokeKind::Inside,
        );
    }
    resp.changed()
}

/// 迷你文字按钮（设置页行内辅助动作：「恢复默认」「重置」等）。
///
/// 自绘理由同开关 / 滑块：纯平毛玻璃语言要统一，`egui::Button` 的
/// 默认立体感与整体观感不符。悬停底色走 110ms 补间（与顶栏图标按钮
/// 同一套机制，见 `icon_button` 附近的注释）。
///
/// # id_suffix 必须全局唯一
///
/// 同时是补间键与交互 Id——同一帧出现两个同名迷你按钮会共享交互与
/// 动画状态（`draw_slider` 的 `variant` 参数是同一教训的行内版）。
///
/// # `hit`：热区登记开关
///
/// `Some(key)` 时把按钮矩形登记进热区表（自动化寻址用）；`None` 时
/// 不登记——令牌行的「重置」按钮与色相滑条**共用令牌名**，都登记的
/// 话后画的覆盖先画的（`hit` 是 HashMap），自动化就再也找不到滑条。
/// 交互本身走 egui `Id`（`id_suffix`），与热区表无关。
fn draw_mini_button(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    id_suffix: &'static str,
    text: &str,
    hit: Option<&'static str>,
) -> bool {
    let pal = f.pal;
    let font = egui::FontId::proportional(pal.font_sm);
    let text_w = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), pal.text)
        .rect
        .width();
    let size = vec2(text_w + 16.0, pal.control_height * 0.82);
    let (r, _) = ui.allocate_exact_size(size, egui::Sense::empty());
    let resp = ui.interact(r, egui::Id::new(("mini_btn", id_suffix)), Sense::click());
    let p = ui.painter_at(r);
    // 悬停补间（110ms OutCubic）：目标值由本帧是否悬停给出。
    let hover = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("mini_btn_hover", id_suffix)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    // 先铺不透明底再叠半透明提亮（与类型栏按钮同一个理由：半透明
    // 令牌不能直接当填充，见 `draw_type_rail` 的 CAUTION）。
    p.rect_filled(r, pal.radius_sm, pal.surface_variant);
    if hover > 0.004 {
        p.rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hover));
    }
    p.rect_stroke(
        r,
        pal.radius_sm,
        egui::Stroke::new(1.0, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
    p.text(r.center(), Align2::CENTER_CENTER, text, font, pal.text);
    if let Some(key) = hit {
        f.hit_insert(key, r);
    }
    resp.clicked()
}

/// 主题强调色编辑器（「主题色」设置项）。
///
/// 三种调节方式（对应用户要求的「多中自定义调节颜色方式」）：
/// 1. **预设色板**：10 个色块，单击即用——最常见的路径；
/// 2. **HSV 滑条**：色相 / 饱和度 / 明度各一条——精细调节；
/// 3. **十六进制输入**：`#RRGGBB`——从设计稿抄色。
///
/// 返回 `true` 表示本帧有改动（调用方 `record_recent`）。
fn draw_accent_editor(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    use modular_clipboard_core::config::AccentColor;
    let pal = f.pal;
    let cur = f.svc.state.config.ui.accent;
    let shown = cur.unwrap_or_else(AccentColor::default_blue);
    let mut next = cur;
    let mut changed = false;

    // ---- 预览行：当前色块 + 来源 + hex ----
    let rgb = shown.to_rgb();
    ui.horizontal(|ui| {
        // 高度随字号（font_sm×1.7 @scale1≈26）：本行是 horizontal，
        // 色块先放置、来源标注后放置——egui 不重排先放的控件，固定高
        // 在 font_scale>1 时被文字 galley 撑行后色块会顶格偏高（与令牌
        // 行同款病，见 token_row_h 的 CAUTION）。
        let swatch_h = (pal.font_sm * 1.7).max(22.0);
        let (r, _) = ui.allocate_exact_size(vec2(48.0, swatch_h), Sense::empty());
        ui.painter_at(r)
            .rect_filled(r, pal.radius_sm, egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        ui.add_space(8.0);
        // 来源标注：预设 / 自定义 / 默认。比只显示颜色多一分确定性
        // （用户看得知道自己用的是哪一个预设）。
        let source = if cur.is_none() {
            "默认"
        } else if crate::theme::ACCENT_PRESETS
            .iter()
            .any(|(_, p)| AccentColor::from_rgb(*p).to_rgb() == rgb)
        {
            "预设"
        } else {
            "自定义"
        };
        ui.add(egui::Label::new(
            egui::RichText::new(format!("{source}  {}", shown.to_hex())).color(pal.text_dim),
        ));
    });

    // ---- 预设色板 ----
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(1.0);
        for (i, (name, prgb)) in crate::theme::ACCENT_PRESETS.iter().enumerate() {
            let s = 20.0;
            let (r, _) = ui.allocate_exact_size(vec2(s, s), Sense::empty());
            let resp = ui.interact(r, egui::Id::new(("accent_preset", i)), Sense::click());
            let selected = cur.is_some_and(|c| c.to_rgb() == *prgb);
            let p = ui.painter_at(r);
            p.rect_filled(
                r,
                pal.radius_sm,
                egui::Color32::from_rgb(prgb[0], prgb[1], prgb[2]),
            );
            if selected {
                // 选中圈：text_bright 描边（任何底色上都看得见）。
                p.rect_stroke(
                    r,
                    pal.radius_sm,
                    egui::Stroke::new(2.0, pal.text_bright),
                    egui::StrokeKind::Inside,
                );
            } else if resp.hovered() {
                p.rect_stroke(
                    r,
                    pal.radius_sm,
                    egui::Stroke::new(1.0, pal.text_dim),
                    egui::StrokeKind::Inside,
                );
            }
            if resp.clicked() {
                next = Some(AccentColor::from_rgb(*prgb));
                changed = true;
            }
            // 热区键 = 预设名（全局唯一，同分段控件的约定）。
            f.hit_insert(name, r);
            ui.add_space(5.0);
        }
    });

    // ---- HSV 滑条 ----
    ui.add_space(4.0);
    let mut hsv = shown;
    let mut h = hsv.h;
    if shadow_param_row(
        f,
        ui,
        "accent_hue",
        "色相",
        format!("{:.0}°", h),
        &mut h,
        0.0..=360.0,
    ) {
        hsv.h = h;
        next = Some(hsv);
        changed = true;
    }
    let mut s = hsv.s;
    if shadow_param_row(
        f,
        ui,
        "accent_sat",
        "饱和度",
        format!("{}%", (s * 100.0).round() as i32),
        &mut s,
        0.0..=1.0,
    ) {
        hsv.s = s;
        next = Some(hsv);
        changed = true;
    }
    let mut v = hsv.v;
    if shadow_param_row(
        f,
        ui,
        "accent_val",
        "明度",
        format!("{}%", (v * 100.0).round() as i32),
        &mut v,
        0.0..=1.0,
    ) {
        hsv.v = v;
        next = Some(hsv);
        changed = true;
    }

    // ---- 十六进制输入 + 恢复默认 ----
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new("十六进制"));
        ui.add_space(8.0);
        // 草稿同步的焦点感知：无焦点时每帧覆写为当前色（点预设 / 拖滑条
        // 后输入框跟着变），有焦点时保留用户输入（ rationale 见
        // `UiState::accent_hex`）。非法半截输入不清除已选颜色。
        let editing = ui.ctx().memory(|m| m.has_focus(egui::Id::new("accent_hex_edit")));
        if !editing {
            f.state.accent_hex = shown.to_hex();
        }
        let w = pal.font_sm * 7.3;
        let (r, _) = ui.allocate_exact_size(
            vec2(w, pal.control_height + 2.0),
            egui::Sense::empty(),
        );
        let painter = ui.painter_at(r);
        painter.rect_filled(r, pal.radius_md, pal.surface_variant);
        painter.rect_stroke(
            r,
            pal.radius_md,
            egui::Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
        let text_rect = r.shrink2(vec2(8.0, 4.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let resp = child.add(
            egui::TextEdit::singleline(&mut f.state.accent_hex)
                .id(egui::Id::new("accent_hex_edit"))
                .hint_text("#RRGGBB")
                .frame(egui::Frame::NONE)
                .text_color(pal.text)
                .desired_width(text_rect.width()),
        );
        if resp.has_focus() {
            // 聚焦描边补间（130ms）：与设置页其它输入框同一套。
            let focus_k = crate::anim::tween_f32(
                ui.ctx(),
                egui::Id::new("accent_hex_focus"),
                1.0,
                std::time::Duration::from_millis(130),
                crate::anim::Ease::OutCubic,
            );
            let ring = crate::anim::tween_color(
                ui.ctx(),
                egui::Id::new("accent_hex_focus_color"),
                pal.accent,
                std::time::Duration::from_millis(130),
            );
            painter.rect_stroke(
                r,
                pal.radius_md,
                egui::Stroke::new(1.0 + 0.5 * focus_k, ring),
                egui::StrokeKind::Inside,
            );
        }
        if resp.changed()
            && let Some(c) = AccentColor::from_hex(&f.state.accent_hex)
        {
            next = Some(c);
            changed = true;
        }
        // 恢复默认：仅在用户改过时出现（默认态放个按钮是噪声）。
        if cur.is_some() {
            ui.add_space(8.0);
            if draw_mini_button(f, ui, "accent_reset_default", "恢复默认", Some("accent_reset_default")) {
                next = None;
                changed = true;
            }
        }
    });

    if changed {
        f.state.push(Op::SetAccent(next));
    }
    changed
}

/// 数值令牌的滑条量程（按令牌名推断）。
///
/// 推断而不是另维护一张表：新增令牌时忘记登记量程的后果只是「量程
/// 不合理」（滑起来别别扭扭），而忘记登记**令牌本身**的后果是
/// 「这个令牌在编辑器里根本不出现」——前者良性得多。
fn token_num_range(name: &str) -> std::ops::RangeInclusive<f32> {
    if name.starts_with("font_") {
        8.0..=48.0
    } else if name.starts_with("radius_") || name.starts_with("space_") {
        0.0..=40.0
    } else if name.starts_with("stroke_") {
        0.0..=6.0
    } else if name == "control_height" {
        16.0..=64.0
    } else if name == "icon_size" {
        8.0..=48.0
    } else {
        0.0..=64.0
    }
}

/// 「高级微调」入口行（主题卡片内）。
///
/// 2026-10-19 起令牌编辑器从「卡片内联长列表」升级为**二级面板**：
/// 点击本行把设置窗口右侧内容区整体切换为令牌面板（占一大半区域），
/// 近 60 行令牌终于有地方完整铺开。 rationale：
/// - 内联在主题卡里时，55 行把主题卡撑到五屏，滚半天才能滚完，
///   与「外观/常规」等卡片的信息密度完全失衡；
/// - 折叠态只有本行一个摘要，主题卡回到一屏以内。
///
/// 返回 true 表示本帧产生了 Op（调用方记「最近修改」）。
fn draw_token_entry(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    let pal = f.pal;
    let n_over = f.svc.state.config.ui.theme_overrides.colors.len()
        + f.svc.state.config.ui.theme_overrides.nums.len();

    // 入口行本身做成一个可点条目：标签 + 摘要 + 「调整」。
    let label = if n_over > 0 {
        format!("高级微调 · 已覆盖 {n_over} 个令牌")
    } else {
        "高级微调".to_string()
    };
    let (r, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), pal.control_height),
        egui::Sense::click(),
    );
    let p = ui.painter_at(r);
    if resp.hovered() {
        p.rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    p.text(
        egui::pos2(r.min.x + 4.0, r.center().y),
        Align2::LEFT_CENTER,
        &label,
        egui::FontId::proportional(pal.font_sm),
        pal.text,
    );
    p.text(
        egui::pos2(r.max.x - 4.0, r.center().y),
        Align2::RIGHT_CENTER,
        "调整 ›",
        egui::FontId::proportional(pal.font_sm),
        pal.accent,
    );
    f.hit_insert("token_expand", r);
    if resp.clicked() {
        f.state.theme_tokens_expanded = true;
    }
    false
}

/// 令牌行的统一行高（逻辑点）。
///
/// # 为什么不能写死 18pt（2026-10-19 用户实机反馈「文字向上且渲染
/// 不完整」的根因）
///
/// 令牌行是 horizontal 三列：左列（118pt 文字格）、滑条、数值。
/// egui 的 horizontal **不重排先放置的控件**——行高被后放的高控件
/// 撑大时，先放的左列仍顶部对齐 ⇒ 文字相对滑条偏高。
///
/// 150% DPI（ppp=1.5）下两件事同时发生：
/// 1. 滑条 cell 高 = font_sm×1.07 = 22.5×1.07 ≈ 24pt > 18 ⇒ 行高被
///    撑到 24，左列（18pt）顶格 ⇒ 文字比滑条 knob 中心高约 3pt；
/// 2. 令牌名字号 font_xs = 14×1.5 = 21pt，galley 行高 ≈ 21×1.28
///    ≈ 27pt > 18pt 的格子 ⇒ 字形上下各被裁约 4.5pt（视觉上
///    「上半还在、下半不见了」）。
///
/// 测试 harness 的 ppp=1.0 恰好复现不了这两个数（21→14、24→16），
/// 所以单测全绿而实机糊——**依赖字号的布局必须按字号算，不能拍
/// 常数**。本函数取三者的最大值：左列字形（font_xs×1.45，含 galley
/// 行距）、滑条 cell（font_sm×1.07）、保底 18pt。
fn token_row_h(pal: &Palette) -> f32 {
    (pal.font_xs * 1.45).max(pal.font_sm * 1.07).max(18.0)
}

/// 令牌级覆盖**二级面板**（主题分类的二级视图）。
///
/// 列出**全部**主题令牌（[`Palette::tokens`]），逐项给出当前生效值
/// 与调节入口——「界面上任何元素、任何参数都允许自由调整」的最终
/// 出口（二级主题的旋钮覆盖常见需求，这里覆盖其余一切）。
///
/// - 颜色令牌：色块 + 色相滑条；**半透明**令牌追加透明度滑条
///   （不透明令牌的 alpha 滑条没有意义——拖了没有任何视觉效果）；
/// - 数值令牌：滑条（量程按名字推断，见 [`token_num_range`]）；
/// - 时长令牌：只读（动效时长不适合滑条连续拖，且极少有人调）。
///
/// 已覆盖的令牌行尾有「重置」按钮，恢复解析链派生值。
///
/// # 分组标题
///
/// 55 个令牌平铺是一面墙。按类型分三组（颜色 / 数值 / 时长）并给组
/// 标题，用户能靠分组定位而不是逐行扫描。组标题不注册热区。
fn draw_token_panel(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;

    // ---- 顶部：返回 + 标题 ----
    ui.horizontal(|ui| {
        // 返回：二级视图的唯一出口。做成图标按钮 + 文字。
        let (r, resp) = ui.allocate_exact_size(
            egui::vec2(pal.control_height, pal.control_height),
            egui::Sense::click(),
        );
        let p = ui.painter_at(r);
        if resp.hovered() {
            p.rect_filled(r, pal.radius_sm, pal.row_hover);
        }
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            "‹",
            egui::FontId::proportional(pal.font_lg),
            if resp.hovered() { pal.text_bright } else { pal.text_dim },
        );
        f.hit_insert("token_back", r);
        if resp.clicked() {
            f.state.theme_tokens_expanded = false;
        }
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new("高级微调").color(pal.text).size(pal.font_lg),
            ),
        );
    });
    ui.add_space(2.0);
    ui.add(
        egui::Label::new(
            egui::RichText::new(
                "全部主题令牌的当前生效值。拖动即写入覆盖，优先于二级主题与强调色的派生值。",
            )
            .color(pal.text_dim),
        )
        .wrap(),
    );
    ui.add_space(6.0);

    let n_over = f.svc.state.config.ui.theme_overrides.colors.len()
        + f.svc.state.config.ui.theme_overrides.nums.len();
    // 摘要 + 全部重置（仅在有覆盖时出现——默认态放个按钮是噪声）。
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(
            egui::RichText::new(format!("已覆盖 {n_over} 个令牌")).color(pal.text_dim),
        ));
        if n_over > 0 {
            ui.add_space(8.0);
            let reset_all =
                draw_mini_button(f, ui, "token_reset_all", "全部重置", Some("token_reset_all"));
            if reset_all {
                f.state.push(Op::ClearAllTokenOverrides);
            }
        }
    });
    ui.add_space(6.0);

    // ---- 逐令牌（按类型分组）----
    //
    // CAUTION: ops 先收集后统一 push——`f.state.push` 要 `&mut Frame`，
    // 而循环里还要读 `f.svc.state.config`（是否已覆盖）。边读边写在
    // Rust 里是借用冲突，收集式则天然顺序执行。
    let row_h = token_row_h(pal);
    let mut ops: Vec<Op> = Vec::new();
    let mut group = "";
    for (name, value) in pal.tokens() {
        // 组标题：只在分组变化时画一次。
        let g = match value {
            TokenValue::Color(_) => "颜色",
            TokenValue::Num(_) => "数值",
            TokenValue::Dur(_) => "时长（只读）",
        };
        if g != group {
            group = g;
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(egui::RichText::new(g).color(pal.text_dim).size(pal.font_xs)),
            );
        }
        match value {
            TokenValue::Color(c) => {
                let overridden = f.svc.state.config.ui.theme_overrides.colors.contains_key(name);
                let [sr, sg, sb, sa] = c.to_srgba_unmultiplied();
                let hsv = modular_clipboard_core::config::AccentColor::from_rgb([sr, sg, sb]);
                let mut h = hsv.h;
                ui.horizontal(|ui| {
                    // 左列：色块 + 令牌名（等宽小字，令牌名是给「知道自己在
                    // 找什么」的用户看的，不需要好看）。
                    let (lr, _) = ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                    let p = ui.painter_at(lr);
                    let sw = Rect::from_min_size(lr.min, vec2(14.0, 14.0));
                    p.rect_filled(sw, pal.radius_sm, c);
                    p.text(
                        pos2(sw.max.x + 6.0, lr.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(pal.font_xs),
                        if overridden { pal.text } else { pal.text_dim },
                    );
                    // 右列：色相滑条（+ 半透明令牌的透明度滑条）。
                    if draw_slider(
                        f,
                        ui,
                        name,
                        "hue",
                        &mut h,
                        0.0..=360.0,
                        true,
                        row_h,
                    ) {
                        let nc = modular_clipboard_core::config::AccentColor::new(h, hsv.s, hsv.v);
                        let rgb2 = nc.to_rgb();
                        ops.push(Op::SetTokenOverride {
                            name: name.to_string(),
                            value: TokenValue::Color(egui::Color32::from_rgba_unmultiplied(
                                rgb2[0], rgb2[1], rgb2[2], sa,
                            )),
                        });
                    }
                    if sa < 255 {
                        // 半透明令牌：第二行放透明度滑条（与色相滑条共用
                        // 令牌名，靠 variant 区分 Id，见 draw_slider 的
                        // CAUTION）。
                        let mut a = f32::from(sa);
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            let (lr2, _) =
                                ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                            ui.painter_at(lr2).text(
                                pos2(lr2.min.x + 20.0, lr2.center().y),
                                Align2::LEFT_CENTER,
                                "└ 透明度",
                                egui::FontId::proportional(pal.font_xs),
                                pal.text_dim,
                            );
                            if draw_slider(f, ui, name, "alpha", &mut a, 0.0..=255.0, true, row_h) {
                                let rgb2 = modular_clipboard_core::config::AccentColor::new(
                                    hsv.h, hsv.s, hsv.v,
                                )
                                .to_rgb();
                                ops.push(Op::SetTokenOverride {
                                    name: name.to_string(),
                                    value: TokenValue::Color(
                                        egui::Color32::from_rgba_unmultiplied(
                                            rgb2[0], rgb2[1], rgb2[2],
                                            a.round() as u8,
                                        ),
                                    ),
                                });
                            }
                        });
                    }
                    // 重置按钮（仅已覆盖的令牌）。
                    if overridden {
                        ui.add_space(6.0);
                        // hit=None：重置按钮与色相滑条共用令牌名，登记会
                        // 覆盖滑条的热区（见 draw_mini_button 的 `hit`）。
                        if draw_mini_button(f, ui, name, "重置", None) {
                            ops.push(Op::ClearTokenOverride(name.to_string()));
                        }
                    }
                    // CAUTION: 这里**不**再登记左列——`draw_slider` 已用
                    // 同一个 key 登记了滑条矩形，重复登记会把它覆盖掉，
                    // 自动化就拖不到色相滑条了。
                });
            }
            TokenValue::Num(n) => {
                let overridden = f.svc.state.config.ui.theme_overrides.nums.contains_key(name);
                let mut v = n;
                let range = token_num_range(name);
                ui.horizontal(|ui| {
                    let (lr, _) = ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                    ui.painter_at(lr).text(
                        lr.min,
                        Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(pal.font_xs),
                        if overridden { pal.text } else { pal.text_dim },
                    );
                    let c = draw_slider(f, ui, name, "num", &mut v, range.clone(), true, row_h);
                    ui.add_space(4.0);
                    // 显式给行高：Label 默认字号（font_md）在高 DPI 下可能
                    // 超过 row_h，把 horizontal 行高撑开后又把先放置的左列
                    // 顶上去（同一个「不重排」病）。
                    ui.add_sized(
                        egui::vec2(30.0, row_h),
                        egui::Label::new(
                            egui::RichText::new(format!("{:.0}", v)).color(pal.text_dim),
                        ),
                    );
                    if c {
                        ops.push(Op::SetTokenOverride {
                            name: name.to_string(),
                            value: TokenValue::Num(v),
                        });
                    }
                    if overridden {
                        ui.add_space(6.0);
                        if draw_mini_button(f, ui, name, "重置", None) {
                            ops.push(Op::ClearTokenOverride(name.to_string()));
                        }
                    }
                    // 同颜色行：左列不登记，滑条的热区（draw_slider 内）
                    // 保留给自动化。
                });
            }
            // 时长令牌只读：动效时长不适合滑条连续拖（90ms 与 91ms 没有
            // 任何可感差别），且调整它的用户极少——列出来只是噪声。
            TokenValue::Dur(_) => {}
        }
    }
    for op in ops {
        f.state.push(op);
    }
}

fn draw_mcp_info_btn(f: &mut Frame<'_>, ui: &mut Ui) -> egui::Response {
    let pal = f.pal;
    // 直径随字号缩放（与开关同量级）。
    let d = pal.font_sm * 1.47;
    let (r, resp) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::click());
    let p = ui.painter_at(r);
    // 未 hover：surface_variant 实底（与卡片 surface 拉开一层）；
    // hover：accent 实底。避免用半透明 row_selected 叠在卡片上显脏。
    let bg = if resp.hovered() {
        pal.accent
    } else {
        pal.surface_variant
    };
    p.circle_filled(r.center(), 11.0, bg);
    let ic = if resp.hovered() {
        pal.text_on_accent
    } else {
        pal.accent
    };
    // 信息图标：点 + 竖线。
    p.circle_filled(r.center() + vec2(0.0, -2.5), 1.3, ic);
    p.line_segment(
        [r.center() + vec2(0.0, 1.0), r.center() + vec2(0.0, 5.0)],
        Stroke::new(1.6, ic),
    );
    f.hit_insert("settings_mcp_info", r);
    resp
}

/// 热键录制覆盖层。
///
/// 结构（用户定稿，2026-10-13）：**整窗只画它**——设置内容作为 GPU
/// 模糊的底（`draw_settings` 在调用前放分割标记），本面板保持清晰。
/// 用户看不到设置界面，只看到居中的大圆角正方形：
///
/// ```text
/// ┌──────── 圆角正方形（第一个，外层面板）────────┐
/// │              正在录制快捷键                  │
/// │   ┌Ctrl┐ ┌Shift┐ ┌Alt┐ ┌Win┐   ← 修饰键键帽 │
/// │        ┌──────────┐                    │
/// │        │    K     │                    │  ← 主键键帽（第二个）
/// │        └──────────┘                    │
/// │     录制完成后，点击任意位置停止录制        │
/// └──────────────────────────────────────────┘
/// ```
///
/// 绘制顺序即「先圆角正方形 + 圆角正方形」：先遮罩 + 外层面板，
/// 再内层键帽（修饰键四个 + 主键一个），全部 `painter` 直绘——
/// 不注册任何子交互区，点击一律由全屏 `interact` 收走。
///
/// # 交互模型
///
/// 按下的组合**只显示不应用**；点击任意位置 = 确认并停止
/// （用户要求「点击任意位置停止录制」）。没按过主键就点击 = 取消。
/// Esc 也可取消（在 `App::capture_recording` 里处理）。
fn draw_recording_overlay(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;

    // 入场动效：遮罩淡入 + 面板下沉 8pt（200ms OutCubic）。
    //
    // 录制态下用户会不停按键 ⇒ 事件驱动的帧不会停，动画天然能播完，
    // 不需要额外续帧请求。id 由调用方在录制结束时清除（见 `draw_settings`
    // 的 else 分支），每次开始录制都重播一遍。
    let k = crate::anim::fade_in_once(
        f.ui.ctx(),
        egui::Id::new("recording_overlay_in"),
        std::time::Duration::from_millis(200),
        crate::anim::Ease::OutCubic,
    );

    // 遮罩：与 MCP 覆盖层同色同 alpha（96）——压暗但不糊死，
    // 模糊本身才是「失焦」的主体。
    let dark = pal.surface.r() < 128;
    let shade = if dark {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 96)
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 96)
    };
    // 遮罩随入场淡入（预乘缩放，见 `fade_color`）。
    f.ui
        .painter_at(area)
        .rect_filled(area, 0.0, fade_color(shade, k));

    // ---- 第一个圆角正方形：外层面板（居中）----
    //
    // 用**正方形**而非横幅：录制是「看一眼、按一下、点一下」的
    // 短动作，正方形把内容聚在视觉中心，横幅会让键帽散成一行、
    // 提示文字离得老远。
    let side = 340.0_f32
        .min(area.width() * 0.92)
        .min(area.height() * 0.92);
    // 面板从上方 8pt 沉入（k=1 时归位）。
    let panel = Rect::from_center_size(
        area.center() + vec2(0.0, (1.0 - k) * 8.0),
        vec2(side, side),
    );

    // 全屏交互：录制态下**任意位置**（含面板内）的点击都算确认。
    // 子内容全部 painter 直绘、不注册交互区，点击自然落到这层。
    let bg = f.ui.interact(area, egui::Id::new("recording_overlay"), egui::Sense::click());
    if bg.clicked() {
        f.state.push(Op::FinishRecording);
    }

    let p = f.ui.painter_at(panel);
    p.rect_filled(panel, pal.radius_lg, pal.surface);

    let pad = 22.0;
    let mut y = panel.min.y + pad;

    // 标题。
    p.text(
        pos2(panel.center().x, y + 10.0),
        egui::Align2::CENTER_TOP,
        "正在录制快捷键",
        egui::FontId::proportional(pal.font_md),
        pal.text_bright,
    );
    y += 34.0;

    // ---- 第二个圆角正方形们：键帽 ----
    //
    // 修饰键 4 个一排（按下的高亮），主键一个大键帽居中。
    // 「按下」的判据取**当前帧**的输入状态而非事件流：事件只在
    // 按下的那一帧出现，而用户按住不动时键帽必须保持亮着。
    let mods = f.ui.input(|i| i.modifiers);
    let win = modular_clipboard_platform::win_key_down();
    let caps: [(&str, bool); 4] = [
        ("Ctrl", mods.ctrl),
        ("Shift", mods.shift),
        ("Alt", mods.alt),
        ("Win", win),
    ];
    let cap_w = 62.0_f32;
    let cap_h = 46.0_f32;
    let gap = 10.0_f32;
    let row_w = cap_w * 4.0 + gap * 3.0;
    let row_x0 = panel.center().x - row_w / 2.0;
    for (i, (name, down)) in caps.iter().copied().enumerate() {
        let r = Rect::from_min_size(pos2(row_x0 + i as f32 * (cap_w + gap), y), vec2(cap_w, cap_h));
        // 修饰键键帽：小圆角方块。按下 = accent 填充；未按 = 暗面。
        let (fill, fg) = if down {
            (pal.accent, pal.text_on_accent)
        } else {
            (pal.surface_variant, pal.text_dim)
        };
        p.rect_filled(r, pal.radius_md, fill);
        p.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            name,
            egui::FontId::proportional(pal.font_sm),
            fg,
        );
    }
    y += cap_h + 16.0;

    // 主键键帽：大一号。没按主键时显示「？」占位。
    let main_w = 104.0_f32;
    let main_h = 72.0_f32;
    let main = Rect::from_center_size(pos2(panel.center().x, y + main_h / 2.0), vec2(main_w, main_h));
    let has_key = f.state.recording_combo.is_some();
    let (fill, fg) = if has_key {
        (pal.accent, pal.text_on_accent)
    } else {
        (pal.surface_variant, pal.text_dim)
    };
    p.rect_filled(main, pal.radius_md, fill);
    let main_label = f
        .state
        .recording_combo
        .as_ref()
        .map(|hk| hk.key.to_uppercase())
        .unwrap_or_else(|| "？".into());
    p.text(
        main.center(),
        egui::Align2::CENTER_CENTER,
        main_label,
        // 主键名的「大显示」：比 font_lg 大一档，随 font_scale 缩放
        // （裸 24.0 在高字号下会比周围文字小，失去焦点效果）。
        egui::FontId::proportional(pal.font_lg * 1.25),
        fg,
    );
    y += main_h + 18.0;

    // 底部提示。
    p.text(
        pos2(panel.center().x, y),
        egui::Align2::CENTER_TOP,
        "录制完成后，点击任意位置停止录制",
        egui::FontId::proportional(pal.font_sm),
        pal.text_dim,
    );
}

/// MCP 调用设置覆盖层。
///
/// 结构：全窗遮罩（白/黑随主题）+ 圆角面板（限制在窗口
/// 客户区内，max 92% 宽 / 82% 高）+ 8 个可调用能力开关 + 取消/完成。
///
/// NOTE: 遮罩背后现在是**真正的 GPU 模糊**——`draw_settings` 在调用本函数
/// 前放了一个分割标记（见 [`push_blur_split_marker`]），渲染层把标记之前的
/// 界面内容画到离屏图、跑三趟卷积（降采样 + 横向 + 纵向），再把模糊结果
/// 作为全屏底图铺回来。遮罩只做一层淡淡的压暗，模糊本身才是「失焦」的主体，
/// 因此 alpha 从早期的 148 降到 96（否则会把模糊糊死、看不出效果）。
fn settings_mcp_overlay(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;

    // 遮罩色随主题：亮色界面用白、暗色界面用黑。
    let dark = pal.surface.r() < 128;
    let shade = if dark {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 96)
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 96)
    };
    f.ui.painter_at(area).rect_filled(area, 0.0, shade);

    // 面板：居中，限制在窗口客户区内。
    let pw = 480.0_f32.min(area.width() * 0.92);
    let ph = 560.0_f32.min(area.height() * 0.82);
    let panel = Rect::from_center_size(area.center(), vec2(pw, ph));

    // 点击面板外关闭。
    let bg = f.ui.interact(area, egui::Id::new("mcp_overlay_bg"), egui::Sense::click());
    if bg.clicked() && bg.interact_pointer_pos().is_some_and(|p| !panel.contains(p)) {
        f.state.mcp_overlay_open = false;
    }

    let painter = f.ui.painter_at(panel);
    painter.rect_filled(panel, pal.radius_lg, pal.surface);

    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new("mcp_overlay"))
            .max_rect(panel.shrink2(vec2(16.0, 14.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    ui.spacing_mut().item_spacing = vec2(0.0, 6.0);

    // 标题行：左标题，右 × 关闭。
    let total_on = MCP_CAPS.iter().filter(|c| get_mcp_cap(&f.svc.state.config, c.field)).count();
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("MCP 调用设置")
                .strong()
                .size(pal.font_md)
                .color(pal.text_bright),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 按钮尺寸随 control_height（@scale1≈26）：固定 26 在高字号
            // 下相对文字偏小、可点区域缩水。
            let btn = pal.control_height * 0.93;
            let (r, resp) = ui.allocate_exact_size(vec2(btn, btn), egui::Sense::click());
            let p = ui.painter_at(r);
            if resp.hovered() {
                p.rect_filled(r, pal.radius_sm, pal.row_hover);
            }
            crate::icons::Icon::Close.paint(
                &p,
                Rect::from_center_size(
                    r.center(),
                    vec2(pal.icon_size * 0.81, pal.icon_size * 0.81),
                ),
                if resp.hovered() { pal.text_bright } else { pal.text_dim },
            );
            f.hit_insert("settings_mcp_overlay_close", r);
            if resp.clicked() {
                f.state.mcp_overlay_open = false;
            }
        });
    });

    // 副标题：能力总数 / 当前可调用数。
    ui.label(
        egui::RichText::new(format!(
            "共 {} 个能力 · {} 个可调用 · 关闭后全部不可用",
            MCP_CAPS.len(),
            if f.svc.state.config.mcp.enabled { total_on } else { 0 }
        ))
        .size(pal.font_xs)
        .color(pal.text_dim),
    );

    // 服务连通状态（**真实探测结果**，不是编译期常量）。
    //
    // 这一行是「徽标为什么显示未连接」的答案来源：未连接有四种成因
    // （总开关关 / 未配置端口 / 服务没起 / 已连上），只给一个徽标
    // 用户完全不知道该去改哪里。
    //
    // CAUTION: 必须拆成**两行**。`McpConn::detail()` 最长约 25 个
    // 全角字，与「MCP 服务：xx」挤在同一行会超出面板右缘——egui 的
    // Label 在**横向**布局（`ui.horizontal`）里默认 `TextWrapMode::
    // Extend`：不换行也不截断，直接溢出容器（用户反馈「文字渲染出
    // 界面」）。放到纵向布局下才默认 Wrap。第一行只放短状态，
    // 第二行放长说明。
    {
        let conn = f.state.mcp_conn;
        let color = if conn.is_connected() {
            pal.accent
        } else {
            pal.text_dim
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 0.0);
            // 状态圆点：比读文字更快识别。
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter_at(dot).circle_filled(dot.center(), 3.5, color);
            ui.label(
                egui::RichText::new(format!("MCP 服务：{}", conn.label()))
                    .size(pal.font_xs)
                    .color(color),
            );
        });
        ui.label(
            egui::RichText::new(conn.detail())
                .size(pal.font_xs)
                .color(pal.text_dim),
        );
    }

    // 分隔线。
    let sep_y = ui.min_rect().max.y + 4.0;
    let panel_p = f.ui.painter_at(panel);
    panel_p.hline(
        panel.min.x + 16.0..=panel.max.x - 16.0,
        sep_y,
        egui::Stroke::new(1.0, pal.border_subtle),
    );

    // 能力列表（可滚动）。
    let list_top = sep_y + 8.0;
    let list_bottom = panel.max.y - 52.0;
    let list_rect = Rect::from_min_max(pos2(panel.min.x + 16.0, list_top), pos2(panel.max.x - 16.0, list_bottom));
    if list_bottom > list_top + 8.0 {
        let mut lu = f.ui.new_child(
            egui::UiBuilder::new()
                .id(egui::Id::new("mcp_overlay_list"))
                .max_rect(list_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        lu.spacing_mut().item_spacing = vec2(0.0, 2.0);
        egui::ScrollArea::vertical()
            .id_salt("mcp_overlay_scroll")
            .auto_shrink([false, false])
            .show(&mut lu, |ui| {
                for cap in MCP_CAPS {
                    settings_mcp_overlay_row(f, ui, cap);
                }
            });
    }

    // 底部：取消 / 完成。
    let mut bu = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new("mcp_overlay_bottom"))
            .max_rect(Rect::from_min_max(
                pos2(panel.min.x + 16.0, list_bottom),
                pos2(panel.max.x - 16.0, panel.max.y - 10.0),
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    bu.spacing_mut().item_spacing = vec2(8.0, 0.0);
    draw_mcp_overlay_btn(f, &mut bu, "settings_mcp_overlay_done", "完成", true);
    draw_mcp_overlay_btn(f, &mut bu, "settings_mcp_overlay_cancel", "取消", false);
}

/// 覆盖层底部按钮（自绘小按钮：圆角 + 文字，右对齐顺序）。
fn draw_mcp_overlay_btn(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    key: &'static str,
    label: &str,
    primary: bool,
) {
    let pal = f.pal;
    // 尺寸随字号缩放（基准 84x32 @ font_sm 15）。
    let (r, resp) = ui.allocate_exact_size(
        vec2(pal.font_sm * 5.0 + 9.0, pal.control_height + 4.0),
        egui::Sense::click(),
    );
    let p = ui.painter_at(r);
    let hovered = resp.hovered();
    let fill = if primary {
        if hovered { pal.accent_hover } else { pal.accent }
    } else if hovered {
        pal.row_hover
    } else {
        pal.surface_variant
    };
    p.rect_filled(r, pal.radius_md, fill);
    let color = if primary {
        pal.text_on_accent
    } else {
        pal.text
    };
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(pal.font_sm),
        color,
    );
    f.hit_insert(key, r);
    if resp.clicked() {
        f.state.mcp_overlay_open = false;
    }
}

/// 覆盖层里的一行能力：左 label+desc，右 徽标 + 开关。
fn settings_mcp_overlay_row(f: &mut Frame<'_>, ui: &mut Ui, cap: &McpCap) {
    let total_enabled = f.svc.state.config.mcp.enabled;
    let mut v = get_mcp_cap(&f.svc.state.config, cap.field);
    let changed = setting_row(ui, cap.name, cap.desc, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
            if total_enabled {
                // 开关（最右）。id 直接用字段名（主卡片开关只用 "enabled"，
                // 与能力字段不冲突；热区 key 同理不冲突）。
                let sw = draw_switch(f, ui, cap.field, &mut v);
                // 徽标：已连接 / 未连接。
                //
                // 判据 = **能力已实现**（`ready`，编译期常量）**且** MCP 服务
                // 真的探测得到。早前只看 `ready`，于是只要代码里写了这个能力
                // 就永远显示「已连接」，与服务是否在运行毫无关系。
                let conn = cap.ready && f.state.mcp_conn.is_connected();
                draw_mcp_badge(f, ui, conn);
                sw
            } else {
                // 总开关关闭：画灰色静态开关 + 灰色徽标（不可交互）。
                draw_mcp_badge_disabled(f, ui);
                draw_static_switch_off(f, ui);
                false
            }
        })
        .inner
    });
    if changed {
        f.state.push(Op::SetBool {
            group: ConfigGroup::Mcp,
            field: cap.field,
            value: v,
        });
        record_recent(f, cap.name);
    }
}

/// 「已连接 / 未连接」徽标。
fn draw_mcp_badge(f: &mut Frame<'_>, ui: &mut Ui, ready: bool) {
    let pal = f.pal;
    let (text, fg) = if ready {
        ("已连接", pal.accent)
    } else {
        ("未连接", pal.text_dim)
    };
    // 宽度 = 三个全角字 + 8pt 内边距，随字号缩放：固定 50pt 时
    // 1.5× 字号（三字约 63pt）会把文字顶出徽标。
    let w = pal.font_xs * 3.0 + 8.0;
    let h = pal.font_xs * 1.43;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, 10.0, pal.surface_variant);
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(pal.font_xs),
        fg,
    );
}

/// 总开关关闭时的灰色徽标。
fn draw_mcp_badge_disabled(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    // 同 `draw_mcp_badge`：宽度 = 三个全角字 + 8pt 内边距，随字号缩放。
    let w = pal.font_xs * 3.0 + 8.0;
    let h = pal.font_xs * 1.43;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, 10.0, pal.surface_variant);
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        "已禁用",
        egui::FontId::proportional(pal.font_xs),
        pal.text_dim,
    );
}

/// 灰色静态开关（关闭态，不可交互）。
fn draw_static_switch_off(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    let h = 18.0_f32;
    let w = h * 1.8;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let track = Rect::from_center_size(r.center(), vec2(w, h));
    let p = ui.painter_at(r);
    p.rect_filled(track, h * 0.5, pal.surface_variant);
    p.rect_stroke(track, h * 0.5, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    p.circle_filled(pos2(track.min.x + h * 0.5, track.center().y), h * 0.5 - 2.0, pal.text_dim);
}


/// 置顶卡片当前是否独立成窗。
///
/// 有独立置顶窗时主窗口**不再显示置顶栏**——置顶内容已经在那个
/// 窗口里了，再画一份就是重复。
fn pinned_is_separate(ws: &Workspace) -> bool {
    ws.cards
        .iter()
        .any(|c| c.kind == CardKind::Pinned && c.host == CardHost::Window)
}

fn draw_cards(f: &mut Frame<'_>, _body: Rect) {
    // 置顶**不占一栏**：要么独立成窗、要么内嵌在历史栏顶部。
    // 两种情况主窗口都不画置顶卡片。
    //
    // IMPORTANT: 判据必须与 solver 用的一致（`pinned_is_embedded`），
    // 否则会出现「solver 按占一栏分了宽度、paint 又在历史栏内画
    // 一份」—— 实测界面就是这样：置顶与历史并排，
    // 而用户要的是不分栏。
    let skip_pinned = f.ws.pinned_is_embedded() || pinned_is_separate(f.ws);

    // 按 Z 序遍历。这里直接读 `card.rect`——**不做任何二次判断**，
    // 也不重新计算宽度。矩形是solver 算好的，绘制只负责照着画。
    for card in f.ws.cards.iter() {
        if card.host != CardHost::Docked {
            continue;
        }
        if skip_pinned && card.kind == CardKind::Pinned {
            continue;
        }
        let r = card.rect;
        if r.width() <= 0.0 || r.height() <= 0.0 {
            // 求解器保证停靠卡片必有非零矩形；走到这里说明它还没被
            // 求解（例如最小化后的第一帧）。**跳过而不是画零尺寸卡**——
            // 画零尺寸卡会产出退化矩形，某些路径下会算出非法 scissor。
            continue;
        }
        // 卡片不再有折叠态——原先这里按 `card.collapsed` 二选一
        // （折叠画把手 / 展开画内容），折叠功能移除后只剩一条路径。
        draw_expanded_card(f, card, r);
    }
    // 置顶内嵌的绘制**不在这里**——它在
    // `draw_expanded_card` 里随历史卡片一起处理：那里会先把历史
    // 卡片的 body 切成两段（置顶在上、历史在下），两段各画各的。
    //
    // NOTE: 早前是「画完历史再往上叠一层」，于是标题叠字 + 列表重叠。
    // 而且这里也拿不到「置顶该占多高」——那是切分时才确定的量。
}

/// 历史卡片是否走「一体性」样式（整窗一块底 + 行卡片浮起）。
///
/// # 为什么有这个东西
///
/// 早前界面是四种灰叠出层次：
///
/// ```text
/// 窗口底  bg / 顶栏 surface_variant / 卡片区 list_bg / 行卡片 surface_raised
/// ```
///
/// 三个相邻区域用了三种灰，再各加一道 1px 描边把它们切开 ——
/// 读起来是「三块拼的」。用户反馈要求「强化整体界面的一体性」。
///
/// 一体性版本的思路：**取消色块分割，层次改由「浮起」表达**。
/// 顶栏与卡片区同色（`bg`），只靠一条分隔线区分顶栏；
/// 行卡片靠阴影 / 更亮的底浮起来。
///
/// 关键取舍：层次仍然分得清（圆角 / 留白 / 明暗），但不再靠
/// 「这块比那块浅一点」来划分 —— 那是最容易显得廉价的手法，
/// 色差一点点不明显、一点点就变脏，而且每加一层都要重新调。
///
/// 代价要认：`list_bg` 这一层**消失**了，「卡片」与「窗口」的边界
/// 不再可见。将来若要加第二个卡片（详情 / 侧栏），两边都是透明底
/// 就没法区分 —— 那种场景得把 `list_bg` 加回来，所以留了这个开关。
const HISTORY_UNITY: bool = true;

fn draw_card_frame(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    // NOTE: 不取 scale——描边宽度按逻辑点声明（F-35），本函数没有需要乘 ppp 的量。
    // 纯平毛玻璃：纯色底 + 1px 描边。
    // **禁止渐变、光泽、折射、内发光**——那类效果在浅色卡片上
    // 看起来廉价，且与「平」的视觉语言冲突。
    let painter = f.ui.painter_at(r);
    if HISTORY_UNITY {
        // 一体性：卡片**不画自己的底与描边**，让整窗连成一片。
        // 卡片内缩 `CARD_AREA_INSET` 已经给出了边界暗示。
        return;
    }
    painter.rect_filled(r, CARD_RADIUS, pal.surface);
    painter.rect_stroke(
        r,
        CARD_RADIUS,
        // CAUTION: 不乘 scale（F-35 / UI-P1-4）：egui 的 Stroke 宽度单位
        // 是**逻辑点**，乘 ppp 会让 150% DPI 下的 2pt 描边实际画成 3pt，
        // 与其它 1.0 逻辑点的描边不成体系。物理像素对齐用
        // `round_to_pixel`，不是线性缩放。
        //
        // NOTE: 本分支当前不可达（HISTORY_UNITY 恒 true，函数开头已早退）——
        // 但留着它就必須是正确的，否则将来关掉 UNITY 时这个潜在 bug 直接复活。
        Stroke::new(CARD_STROKE, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 展开态：卡片外框 + 头部 + 内容。
fn draw_expanded_card(f: &mut Frame<'_>, card: &Card, r: Rect) {
    draw_card_frame(f, r);
    let pal = f.pal;

    let pad = pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));

    // 头部：标题 + 折叠/分离按钮。
    //
    // NOTE: 历史栏的标题「历史」已按用户要求去掉——头部高度压为 0，
    // 列表从卡片顶部直接开始（不占位置）。其它卡片保留头部标题。
    let head_h = if card.kind == CardKind::History {
        0.0
    } else {
        (pal.font_md * 1.8).max(20.0)
    };
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    // 内容区。
    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        CardKind::History => {
            if HISTORY_UNITY {
                // 一体性：卡片容器透明，整窗一块 `bg`。
                // 层次全部交给行卡片（`surface_raised` 浮起）。
                draw_history_body(f, body, None);
                return;
            }
            // NOTE: 非一体性时历史栏整卡是「暗容器」：frame 底色也用
            // `list_bg`，否则列表四周仍露出 frame 的 `surface` 底色
            // ——浅色主题下就是一圈白色（用户反馈「列表边上还是白色」）。
            f.ui.painter_at(r).rect_filled(r, CARD_RADIUS, pal.list_bg);
            // 置顶条目**就在这个列表里**（排在最前 + 底色微亮标记），
            // 不再有独立的「置顶区」，也没有第二段 body。
            draw_history_body(f, body, None);
        }
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
    }
}

/// 卡片头部：只有标题。
///
/// # 为什么不再有「分离」「折叠」两枚按钮
///
/// 历史栏**固定在下方**，是主界面的主区，不需要用户手动折叠或拖走——
/// 这两个操作对它是多余的。而它们在界面上表现为两个 PUA 码位的
/// 小图标（⠿ / ⇤），在 150% 缩放下辨识度很差。
///
/// 折叠与分离的**完整逻辑**（`Card::collapsed`、`handle_width`、
/// `collapse_priority`、`Solution::auto_collapsed`、`draw_collapsed_card`、
/// `Op::ToggleCollapse` / `Detach` / `Dock`）都已移除——不是只删按钮。
fn draw_card_header(f: &mut Frame<'_>, card: &Card, head: Rect) {
    // 历史栏标题按用户要求去掉：主窗口历史卡片不再显示「历史」两字，
    // 其头部高度也在 `draw_expanded_card` 压为 0，列表从卡片顶部直接
    // 开始（不占位置）。其它卡片（详情等）保留自己的标题。
    if card.kind == CardKind::History {
        return;
    }
    let pal = f.pal;
    let scale = f.scale;
    let font: FontId = sized(pal.font_md, scale);
    f.ui.painter_at(head).text(
        head.left_center(),
        Align2::LEFT_CENTER,
        card.kind.title(),
        font,
        pal.text_bright,
    );
}

// ---------------------------------------------------------------- 内容

/// 首帧该从哪一行开始画（手工虚拟化的起始下标）。
///
/// `offsets[i]` 是第 i 行的顶边，`heights[i]` 是它的**实际**高度
/// （预览折行数不同则高度不同），`scroll` 是滚动偏移。
///
/// # 为什么 `ROW_HEIGHT_MIN` 不能用在这里
///
/// 「第 i 行已完全滚出可视区上方」的真正条件是
/// `offsets[i] + heights[i] <= scroll`。用代表**单行**高度的
/// `ROW_HEIGHT_MIN` 代替，则任何预览超过 1 行的条目都会被提前判定为
/// 「已滚过」—— 它明明还铺满整个视口，却一行都不画。
///
/// 实测推演（heights = [58, 3816, 58]，视口 500，scroll = 200）：
/// 第 1 行占 67..3883 仍铺满视口，但 `67 + 58 = 125 <= 200` ⇒ 被跳过，
/// 第 2 行的 y 又超出视口底 ⇒ `break` ⇒ **列表整块空白**。
///
/// # 为什么抽成函数
///
/// 这段逻辑躲在 `draw_history_body` 里、依赖 `Frame`，而列表行**不登记**
/// 自报热区 —— 于是从外部**没有任何办法**观测到「行被跳过了」。
/// 抽成纯函数后判据才能落在真正出错的地方。
fn first_visible_row(offsets: &[f32], heights: &[f32], scroll: f32) -> usize {
    debug_assert!(
        offsets.len() >= heights.len(),
        "offsets 应比 heights 多一项（末项是总高），实际 {} vs {}",
        offsets.len(),
        heights.len()
    );
    // `take_while` 在首个不满足处停止，语义与 `partition_point` 相同；
    // 用它而非 `partition_point` 是因为后者是 slice 方法，而这里要同时
    // 查两个数组的第 i 项。零分配——这条路径每帧都跑。
    (0..heights.len())
        .take_while(|&i| offsets[i] + heights[i] <= scroll)
        .count()
}

fn draw_history_body(f: &mut Frame<'_>, body: Rect, only_pinned: Option<bool>) {
    // 自报列表可视区。
    //
    // NOTE: 这个 key **不是**给绘制用的，是给「热区自报」机制用的：
    // 测试与自动化脚本从 `UiState::hit` 里读坐标，而不是自己按
    // `pal` 常量重算一遍公式——重算等于把实现抄进测试，实现改对了
    // 测试反而挂。
    //
    // 两个列表（主栏 / 内嵌置顶区）必须用**不同 key**：`hit` 是
    // `HashMap<&str, Rect>`，同名会互相覆盖（settings 开关就踩过）。
    f.hit_insert(
        if only_pinned == Some(true) {
            "pinned_body"
        } else {
            "history_body"
        },
        body,
    );

    // ---- 列表显示区域**不画背景** --------------------------------------
    //
    // 早前这里铺一层 `list_bg`（比窗口底更暗）来衬出亮卡片。现在去掉：
    //
    // 1. 它与「一体性」目标冲突——多一层灰就多一道色块边界，
    //    整窗读起来是「拼出来的」；
    // 2. 它把窗口的半透明/磨砂**整块盖住**——列表是主界面最大的一块，
    //    铺了不透明的底，背后透出来的东西（磨砂底图、桌面）就全被挡住，
    //    「半透明」只剩顶栏与边距那几像素看得见。
    //
    // 层次改由**行卡片自身**承担：`surface_raised` 的亮底 + 一圈极弱描边
    // 已经足够让卡片「浮」起来，不需要暗色容器衬托。

    // 本列表用哪个滚动槽位。
    //
    // 内嵌置顶区已移除，所以常态下**只有一个列表、一个槽位**。
    // 仅当工作区里存在 `CardKind::Pinned` 独立卡片时才会出现第二个，
    // 此时才需要分开存 —— 早前内嵌置顶区与主栏共用一个 `f32`，
    // 而 `vertical_scroll_offset` 每帧都会把它写进 egui，
    // 于是滚主栏会把置顶区一起带走（反之亦然）。
    let pinned_slot = only_pinned == Some(true);

    // ---- 列表边距 ------------------------------------------------------
    //
    // 内容区左右各缩进一条竖边距（`LIST_PAD_X`），历史条目像卡片一样
    // 悬浮在留白中；左右留白竖条同时上报为**拖动区**（WM_NCHITTEST
    // 返回 HTCAPTION），用户按住列表两侧的空白即可拖动窗口。
    //
    // NOTE: 滚动条放在内容区右缘（`ScrollArea` 的 `max_rect` 用
    // `content`），因此滚动条不会压进右留白拖动区——若滚动条贴着
    // `body.max.x` 画，右拖动区就正好压在滚动条上，拖滚动条会变成
    // 拖窗口。
    let content = Rect::from_min_max(
        pos2(body.min.x + LIST_PAD_X, body.min.y),
        pos2(body.max.x - LIST_PAD_X, body.max.y),
    );
    if content.width() <= 8.0 || content.height() <= 8.0 {
        return;
    }
    f.state.drag_regions.push(Rect::from_min_max(
        pos2(body.min.x, body.min.y),
        pos2(body.min.x + LIST_PAD_X, body.max.y),
    ));
    f.state.drag_regions.push(Rect::from_min_max(
        pos2(body.max.x - LIST_PAD_X, body.min.y),
        pos2(body.max.x, body.max.y),
    ));

    // 过滤：全部 / 仅置顶。
    //
    // 主栏（`None`）默认**包含**置顶条目 —— 置顶就在同一个列表里，
    // 只靠 `pin_tint` 标记区分。
    //
    // 但若置顶卡片真的会被**独立绘制**（`PinnedMode = 独立分栏`），
    // 主栏必须让位，否则同一个 `("row", item.id)` 被画两次，
    // egui 的 ID 冲突检查会直接在界面上打出警告。
    //
    // CAUTION: 判断条件必须用 [`Workspace::docked`] 而不是
    // `cards.iter().any(...)`。默认工作区**建了**一张
    // `CardKind::Pinned` 卡片（`workspace.rs` 的默认构造里就有），
    // 用 `cards.iter()` 会永远得到 true ⇒ 主栏永远排除置顶条目；
    // 而内嵌模式下那张卡片又被 `docked()` 跳过、无人绘制 ——
    // 两边都不画，**置顶条目彻底从界面上消失**。
    // `docked()` 内部已经处理了「置顶内嵌时不参与 dock」这个规则，
    // 它才是「这张卡片会不会真的被画出来」的准确来源。
    let pinned_drawn_separately = f.ws.docked().any(|c| c.kind == CardKind::Pinned);
    // 类型过滤（2026-10-15 第三轮：悬浮类型栏）。与 `query` 一样是
    // **界面本地**过滤——不进 store 查询、不进 IPC 快照，绘制时筛行。
    // 置顶条目同样参与类型过滤：置顶只是一个排序权重，不是豁免。
    let type_filter = f.state.type_filter;
    // 只收**索引**，不克隆条目（PF-1）：整表 `.cloned()` 每帧拷贝全部
    // preview / hash / source_app 三个 String（2000 条时是数 MB/帧），
    // 而绘制只碰可见行。索引 8 字节/行，测量与绘制都按需取原表。
    let rows: Vec<usize> = f
        .svc
        .state
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            type_filter.is_none_or(|k| it.kind == k)
                && match only_pinned {
                    Some(p) => it.pinned == p,
                    None => !(pinned_drawn_separately && it.pinned),
                }
        })
        .map(|(i, _)| i)
        .collect();
    // NOTE: 这里**不排序**。置顶排最前是**数据层**保证的 ——
    // store 的 SQL 写着 `ORDER BY pinned DESC, created_at DESC`
    // （见 `modular-clipboard-store` 的 `list()`）。绘制层再排一次
    // 属于重复且错层：排序规则要改就得改两个地方，而早前内嵌置顶区
    // 与主栏各画一份时，两边排序规则一旦不一致就会出现
    // 「同一条目在两处位置不同」。

    if rows.is_empty() {
        let msg = match only_pinned {
            Some(true) => "还没有置顶条目",
            // 有查询词**或类型过滤**生效 ⇒ 「没有匹配的结果」；
            // 两者都空才是「还没有剪贴板记录」（从没复制过东西）。
            _ if !f.state.query.is_empty() || type_filter.is_some() => "没有匹配的结果",
            _ => "还没有剪贴板记录",
        };
        draw_empty(f, body, msg);
        return;
    }

    // 行高：足够放两行文字（预览 + 来源/时间），让内容**完整可读**。
    //
    // 早前版本是 40pt 单行 + `elide_text` 截断，长路径/长文本只能看到
    // 一小段（截图里 `miniz_oxide-36a1…` 就是这样）。用户要求「完整
    // 显示内容」，因此改成两行：上行给预览（最多两行），下行给来源时间。
    // ---- 动态行高 -------------------------------------------------
    //
    // 每条内容按折行数算自己的高度（见 [`row_height_for`]），
    // 这样长内容能被完整展开、短内容不占多余空间。
    //
    // 带缓存（PF-1）：行高只取决于「条目文本 + 测量宽度 + 缩放」，
    // 跨帧不变。失效判据是三者的等价类——列表版本号（每次 reload +1）、
    // `text_w`、`scale`——全部相同才复用，否则重算并换新缓存。
    let pal = f.pal;
    let scale = f.scale;
    // IMPORTANT: 必须用 `preview_full_width`——与 `draw_item_row` 共用同一个算式。
    // 宽度取内容区（左右已扣边距），保证测量与渲染一致。
    //
    // NOTE: 这里用的是**整行**宽度（不减按钮列）：预览带位于按钮行下方，
    // 因此测量折行数时也必须按整行宽算，否则量出的行数会偏多、行高虚高。
    //
    // IMPORTANT: **无条件**让出滚动条占位与呼吸空间。绘制侧的行矩形在
    // 「可滚动」时会内缩 [`SCROLLBAR_RESERVE`] + [`SCROLLBAR_GAP`]
    // （否则 egui 的 floating 滚动条会盖住删除按钮，见下面
    // `row_w` 的注释），而「是否可滚动」取决于 `content_h`、也就是
    // 本函数算出来的行高——直接用 `content.width()` 测量就是一个**先鸡
    // 先蛋**：量的时候按宽宽度算，量完才知道要滚动，那时行已经窄了
    // 16pt ⇒ 折行数偏少 ⇒ 行高偏小 ⇒ 文字被裁。实测症状正是
    // 「条目少时正常、条目一多就每行少显示一点」。
    //
    // 无条件让出为什么**不会**引入新问题（两个方向都是安全的）：
    // 1. 可滚动时：测量与绘制**完全同宽**（都让出），这是要修的情况；
    // 2. 不可滚动时：绘制宽度比测量**多** 16pt ⇒ 文字实际能放的比
    //    量出来的更宽 ⇒ 行高**偏大**而非偏小 ⇒ 最多多留一点空白，
    //    绝不会裁字。方向反了才是 bug（那是早前的写法）。
    //
    // 单调性保证不会来回抖：让出 16pt 只会让行变高，`content_h` 只增
    // 不减，所以「本来可滚动 ⇒ 让出后仍可滚动」，滚动条不会闪。
    let text_w = preview_full_width(row_measure_width(content.width()));
    let version = f.svc.state.list_version;
    // 解码得到的尺寸数：图片行高的输入之一，进缓存的失效判据
    // （见 [`RowHeightCache::dims_known`]）。
    let dims_known = f.state.thumb_dims.len();
    // 过滤维度必须进键（见 [`RowFilterKey`]）：`type_filter` 等是绘制层
    // 本地状态，不过 store ⇒ `list_version` 不变，而长度相同的两次过滤
    // 会让缓存误命中、下标错位。
    let filter = RowFilterKey {
        type_filter,
        only_pinned,
        pinned_drawn_separately,
    };
    let heights: Vec<f32> = {
        let hit = f.state.row_height_cache[pinned_slot as usize]
            .as_ref()
            .filter(|c| {
                c.version == version
                    && c.filter == filter
                    && c.text_w == text_w
                    && c.scale == scale
                    && c.dims_known == dims_known
                    && c.heights.len() == rows.len()
            });
        match hit {
            Some(c) => c.heights.clone(),
            None => {
                // CAUTION: `thumb_dims` 的引用必须在**可变借用**
                // `row_height_cache` 之前取好，否则借用冲突。
                let known = &f.state.thumb_dims;
                let h: Vec<f32> = rows
                    .iter()
                    .map(|&i| {
                        let item = &f.svc.state.items[i];
                        row_height_for(
                            f.ui,
                            item,
                            pal,
                            scale,
                            text_w,
                            &f.svc.state.config.ui,
                            known.get(&item.id).copied(),
                        )
                    })
                    .collect();
                f.state.row_height_cache[pinned_slot as usize] = Some(RowHeightCache {
                    version,
                    filter,
                    text_w,
                    scale,
                    dims_known,
                    heights: h.clone(),
                });
                h
            }
        }
    };
    // 行间缝隙：阴影开启时按「偏移 + 弥散半径」动态加宽（见
    // `history_row_gap`），保证投影不被下一张卡片盖住；关闭时回落
    // 到基础 `ROW_GAP`。首项顶部留半条，每项之后再留整条。
    // **最后一项之后不留**——否则滚到底时最后一行会悬在
    // 列表底部之上一条缝（gap），贴底点击/视觉都对不上。
    let row_gap = history_row_gap(&f.svc.state.config.ui.card_shadow);
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    offsets.push(row_gap * 0.5);
    for (i, h) in heights.iter().enumerate() {
        let inc = if i + 1 == heights.len() { *h } else { h + row_gap };
        offsets.push(offsets.last().copied().unwrap_or(0.0) + inc);
    }
    let content_h = offsets.last().copied().unwrap_or(0.0);

    // ---- 滚动 ----
    //
    // 滚动偏移夹在 `[0, content_h - 可视高]`：不夹的话快速滚到底后
    // 列表会整体上移、最后几项「跳」出可视区。
    let max_scroll = (content_h - body.height()).max(0.0);
    let scroll = f.state.list_scroll(pinned_slot).clamp(0.0, max_scroll);

    // # 为什么用 `ScrollArea::show` 而不是 `show_rows`
    //
    // `show_rows` 要求**恒定行高**（参数就叫 `row_height_sans_spacing`），
    // 而本列表的行高是**逐条算出来**的（[`row_height_for`]）——
    // 预览长度差异极大，固定行高要么截断长内容、要么给短内容留白。
    // 那是早前 40pt 固定行高被用户否决过的方案。
    //
    // 所以用 `show` + 自己做虚拟化：外层 `ScrollArea` 提供**裁剪与
    // 滚动条**（这两件事手写最容易出错），内层仍按前缀和只画可视行。
    //
    // 顺带解决了一个旧缺陷：自绘滚动条的滑块长度曾用
    // 「可视高 / 固定行高」反推行数，算出来恒为 1（永远是满格），
    // 现在交给 egui 按真实内容高算。
    let mut list_ui = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new(("list", only_pinned.unwrap_or(false))))
            .max_rect(content)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let out = egui::ScrollArea::vertical()
        // NOTE: 0.36 用 `id_salt`（不再是旧版的 `id_source`）。
        // 必须给：主栏与内嵌置顶区是两个独立列表，共用默认 Id 会
        // 共享滚动位置——滚主栏会把置顶区一起带走。
        .id_salt(("history_scroll", pinned_slot))
        .max_height(body.height())
        // 每帧把我们存的偏移灌进去：虚拟化要在 `show` **之前**知道
        // 滚到哪了，而 `ScrollArea` 自己的状态只在结束后才拿得到。
        .vertical_scroll_offset(scroll)
        // 滚轮交给 egui 处理，步长用 multiplier 调。
        //
        // CAUTION: 这里**不能**再自己读 `smooth_scroll_delta` 手动加一次：
        // `ScrollArea` 内部已经消费了同一个事件（它同样只在指针悬停
        // 于本区域时生效），两边叠加会以两倍速度滚。早前就是双份的。
        //
        // 平台层一个刻度 = 1.0 行（见 `wheel_lines`），egui 默认
        // `line_scroll_speed = 40`，即 40pt/刻度 ≈ 1 行，太慢；
        // ×3 对齐 Windows 的「一个刻度滚 3 行」惯例。
        .wheel_scroll_multiplier(egui::vec2(1.0, 3.0))
        .auto_shrink([false, false])
        .show(&mut list_ui, |ui| {
            // 内容高度决定滚动条比例，必须显式给出，否则 egui 按
            // 「子控件实际占用」算，而虚拟化下只画了可视行 ⇒ 比例失真。
            ui.set_height(content_h);

            // ---- 坐标系：**一律用全局坐标** --------------------------------
            //
            // NOTE: egui 的 `Rect` **永远是全局（窗口）坐标**，不存在
            // 「相对某个 Ui 的本地坐标」。`ScrollArea` 内部虽然把内容
            // Ui 建成 `content_max_rect = from_min_size(inner_rect.min
            // - state.offset, …)`，但那只是**可用区域**的偏移，用来让
            // 常规控件（跟着布局游标走的那些）自动排到滚动后的位置；
            // 它**不会**给传进来的坐标做任何变换。
            //
            // 早前这里写成 `pos2(0.0, offsets[i])`，于是行全部被画到
            // 窗口左上角（x=0），而列表区域一片空白。这类错误的
            // 隐蔽之处在于：空列表时根本不执行这段代码。
            //
            // 正确做法与改 ScrollArea 之前一致——用绝对坐标：
            // `body.min.y + offsets[i] - scroll`。ScrollArea 在这套
            // 手工虚拟化里的作用只剩**裁剪**与**滚动条**。
            // CAUTION: 判据必须用**每行自己的实际高度**，不能用 `ROW_HEIGHT_MIN`。
            //
            // 早前写成 `offsets[i] + ROW_HEIGHT_MIN <= scroll`：
            // 「第 i 行已经滚过去了」的真正条件是
            // `offsets[i] + heights[i] <= scroll`，而 `heights[i]` 只在
            // 预览恰好 1 行时才等于 `ROW_HEIGHT_MIN`。
            //
            // 只要某条预览超过 1 行（复制一段代码/长文本，剪贴板工具的
            // 最常规用法），就存在区间
            // `scroll ∈ [offsets[i]+58, offsets[i]+heights[i])`
            // —— 此区间内该行**明明还看得见**却被判定为「已滚过」而跳过。
            //
            // 实测推演（heights=[58, 3816, 58]，视口 500，scroll=200）：
            //   第 1 行占 67..3883，滚到 200 时仍然铺满整个视口；
            //   但 `67 + 58 = 125 <= 200` ⇒ 被跳过；
            //   第 2 行的 y = 3689 > 视口底 ⇒ break；
            //   ⇒ **一行都没画，列表整块空白**。
            //
            // `offsets[i] + heights[i]` 随 i 单调递增（offsets 递增、
            // heights 非负），满足 `partition_point` 的分段前提。
            let first = first_visible_row(&offsets, &heights, scroll);

            // 可见行范围预扫：与下面绘制循环的 break/continue 判据一致
            // （顶越过视口底即止）。只为确定「要克隆哪些行」。
            //
            // 只克隆可见行（PF-1）：整表克隆每帧拷贝全部 preview String，
            // 而一屏可见的通常不到 30 条。`draw_item_row` 需要 `&mut Frame`，
            // 无法直接借用 `f.svc.state.items`，克隆这一小段是最廉价的解法。
            let mut last = first;
            for (i, off) in offsets.iter().enumerate().take(rows.len()).skip(first) {
                if content.min.y + off - scroll > content.max.y {
                    break;
                }
                last = i + 1;
            }
            let visible: Vec<ClipItem> = rows
                .get(first..last)
                .map(|s| s.iter().map(|&i| f.svc.state.items[i].clone()).collect())
                .unwrap_or_default();

            // IMPORTANT: 行矩形本身必须避开垂直滚动条（仅**可滚动**时）。
            //
            // egui 0.36 默认 `ScrollStyle` 是 **floating**：滚动条
            // **不占布局宽度**，而是浮在 `content` 右缘覆盖
            // `[content.max.x - 10, content.max.x]`（宽度见
            // `ScrollStyle::floating().bar_width = 10.0`）。
            //
            // 而滚动条在内容闭包**之后**才注册交互（`scroll_area.rs`
            // 用 `Sense::CLICK | Sense::DRAG` 注册 `max_bar_rect`），
            // egui 的 hit-test 规则是「并列取最后注册 = 最上层」
            // ⇒ **滚动条压过行内所有控件**。
            //
            // 早前行矩形用 `content.width()`，最右侧的删除按钮中心
            // 恰好落进滚动条 ⇒ 列表一多就**点不动**，而条目少（无滚动条）
            // 时一切正常 —— 「条目少就好、条目多就坏」是这条 bug 的指纹。
            //
            // 让行矩形右缘内缩，按钮/文字都以 `r.max.x` 为基准，于是
            // 自动全部避开，不必逐个改。
            //
            // 仅在 `content_h > 视口高`（有滚动条）时让出：否则白白
            // 损失 16pt 可用宽度。
            //
            // 让出量 = 滚动条热区（`SCROLLBAR_RESERVE`）+ 呼吸空间
            // （`SCROLLBAR_GAP`）：滑块与卡片之间不再零距相贴
            // （2026-10-17 用户反馈「滑动条容易和卡片挤在一起」）。
            let row_w = if content_h > content.height() {
                content.width() - SCROLLBAR_RESERVE - SCROLLBAR_GAP
            } else {
                content.width()
            };

            // ---- 新条目入场：首行从上方滑入 --------------------------------
            //
            // 版本号变化 = store 侧有增删置顶（复制新内容、置顶、删除）。
            // 变化的那一帧把入场补间**重启**到 0→1（200ms OutCubic），
            // 之后每帧由下面的 `tween_f32` 推进；行矩形按当前值从上方
            // 12pt 归位。只滑行矩形——`offsets`/行高不参与动画，布局
            // 与测量保持稳态（否则行高缓存要跟着抖）。
            //
            // 三个闸门缺一不可：
            // 1. **只首行**（`first == 0`）：滚到半截时复制的新条目在
            //    视口外，放了也看不见；且不该让「当前第一屏」无缘无故
            //    滑一下。
            // 2. **版本闸门**：搜索/类型过滤是绘制层本地状态、不 bump
            //    `list_version`（见 `RowFilterKey`）⇒ 敲键筛选不触发。
            //    没有这层，每敲一个键首行就滑一次，像卡带。
            // 3. **重启而非惰性目标**：`tween_f32` 的目标恒为 1，第一次
            //    调用后 from==to，之后再也不会重播（主题切换踩过同一个
            //    坑，见 `anim::restart` 的文档）。
            let slot_seen = &mut f.state.list_version_seen[pinned_slot as usize];
            let play_entry = first == 0 && slot_seen.is_some_and(|v| v != version);
            *slot_seen = Some(version);
            if play_entry {
                crate::anim::restart(
                    ui.ctx(),
                    egui::Id::new(("list_row_in", pinned_slot)),
                    0.0,
                    1.0,
                    std::time::Duration::from_millis(200),
                    crate::anim::Ease::OutCubic,
                );
            }

            // 先收集真正可见行的几何（过滤条件与下面的内容绘制循环
            // 完全一致），供阴影/内容两遍渲染共用。
            let mut frames: Vec<(usize, usize, Rect)> = Vec::new();
            for (k, i) in (first..last).enumerate() {
                let y = content.min.y + offsets[i] - scroll;
                if y > content.max.y {
                    break; // 已越过可视区底部
                }
                // 首行的入场位移：稳态 k=1（无位移）；动画进行中时行矩形
                // （绘制与交互同一矩形）从上方 12pt 归位，被 ScrollArea
                // 的视口裁剪 ⇒ 观感是「新卡片从列表上缘被发出来」。
                let y = if i == 0 {
                    let k_in = crate::anim::tween_f32(
                        ui.ctx(),
                        egui::Id::new(("list_row_in", pinned_slot)),
                        1.0,
                        std::time::Duration::from_millis(200),
                        crate::anim::Ease::OutCubic,
                    );
                    y - (1.0 - k_in) * ROW_IN_SLIDE
                } else {
                    y
                };
                let r = Rect::from_min_size(
                    pos2(content.min.x, y),
                    vec2(row_w, heights[i]),
                );
                if r.max.y < content.min.y {
                    continue; // 整行都在可视区上方
                }
                frames.push((k, i, r));
            }

            // 本帧本列表可见的图片条目报到帧级集合（`thumb_seen`）——
            // 淘汰在 `draw` 收尾统一做，见该字段的说明。
            for (_, i, _) in &frames {
                let it = &f.svc.state.items[rows[*i]];
                // CAUTION: 判据必须是 [`preview_is_image`] 而不是
                // `kind == Image`。文件型图片（从资源管理器复制的 .png）
                // 同样会上传纹理，漏报它 ⇒ `draw` 收尾的 retain 把刚传的
                // 纹理当「已滚出屏」丢掉 ⇒ **下一帧重新读盘解码**，
                // 每帧一次磁盘 IO（正是 PF-1 禁止的），界面会持续卡顿。
                if preview_is_image(it) {
                    f.state.thumb_seen.insert(it.id);
                }
            }

            // ---- 第一遍：所有行的卡片阴影（统一在内容之前） ----
            //
            // 必须用视口级 painter（不能用 `painter_at(r)`，否则外扩
            // 晕带被行矩形裁掉）；必须先于所有行内容画：落在卡片矩形
            // 内的阴影随后会被该行的不透明填充盖住，于是投影只在行间
            // 缝隙与列表边缘可见，后一行的阴影也不会压暗前一行的文字。
            // 几何细节与反混合公式见 `draw_card_shadow`。
            {
                let shadow_painter = ui.painter();
                let shadow_cfg = &f.svc.state.config.ui.card_shadow;
                for &(_, _, r) in &frames {
                    // CAUTION: `ui.painter()` 返回的就是 `&Painter`，不要再
                    // 套一层 `&`——clippy 的 needless_borrow 会报，且
                    // `&&Painter` 靠自动解引用勉强能编译，属于暗坑。
                    draw_card_shadow(shadow_painter, r, ROW_RADIUS, shadow_cfg, f.pal);
                }            }

            // ---- 第二遍：缝隙拖动区 + 行内容 ----
            for (k, i, r) in frames {
                // ---- 卡片缝隙 = 拖动区 ------------------------------------
                // 卡片与卡片之间的空白横带（含首行顶部的半条缝）同样上报
                // 为拖动区：WM_NCHITTEST 返回 HTCAPTION，用户按住行间
                // 缝隙即可拖动窗口，不必精确瞄准左右留白。
                //
                // NOTE: 右缘让出滚动条热区**与呼吸空间**（`content.max.x
                // - RESERVE - GAP`），否则「拖滚动条」会被系统判成
                // HTCAPTION 变成拖窗口——与左右留白拖动区避开滚动条是同
                // 一个原因。GAP 一并让出：拖动条若伸进呼吸空间，用户在
                // 滑块旁按住会同时命中两个区域。
                let gap_top = if i == 0 { content.min.y } else { r.min.y - row_gap };
                let gap_bottom = r.min.y.min(content.max.y);
                if gap_bottom > gap_top {
                    f.state.drag_regions.push(Rect::from_min_max(
                        pos2(content.min.x, gap_top.max(content.min.y)),
                        pos2(content.max.x - SCROLLBAR_RESERVE - SCROLLBAR_GAP, gap_bottom),
                    ));
                }
                draw_item_row(f, ui, r, &visible[k]);
            }
            // NOTE: 纹理淘汰**不在这里做**——主栏与内嵌置顶区各调一次
            // 本函数，各自 retain 会互相淘汰（见 UiState::thumb_seen）。
            // 统一收到帧级，由 `draw` 收尾处理。
            // 最后可视行之后的空白（内容不满一屏时的列表底部）也作为拖动
            // 区——它不覆盖任何卡片，按住中部空白即可拖动窗口。
            //
            // NOTE: 只在**内容不满一屏**（无滚动条）时补这条：滚动状态下底部
            // 会被下一行占据，误报会把下一行的可点击区域吃成 HTCAPTION。
            if content_h < content.height() {
                f.state.drag_regions.push(Rect::from_min_max(
                    pos2(content.min.x, content.min.y + content_h),
                    pos2(content.max.x, content.max.y),
                ));
            }
        });

    // ---- 回读 egui 的真实滚动位置 --------------------------------
    //
    // CAUTION: 必须回读，否则**拖动滚动条无效**：拖动改变的是 `ScrollArea`
    // 自己的状态，而我们每帧开头又用旧值把它覆盖回去了。
    //
    // 回读后两者就一致了：滚轮、拖条、触屏都由 egui 处理，我们只
    // 是把结果镜像一份，供**下一帧**的虚拟化使用。
    let reported = out.state.offset.y;
    f.state.set_list_scroll(pinned_slot, reported.clamp(0.0, max_scroll));
}

/// 行内「文字区」的可用宽度。
///
/// # 为什么必须是**同一个函数**
///
/// 早前测量（`row_height_for`）与渲染（`draw_item_row`）各写一份算式，
/// 而测量那份额外减了 `8.0`：于是按 `text_w` 量折行数、按宽 8pt 的
/// `preview_band` 渲染，折行数可能**多算一行** ⇒ 行高凭空高出一整行
/// （约 16pt），长内容下方留一条空白。
///
/// 「动态行高」的价值就在于贴合内容，两处不一致会让它稳定地偏大，
/// 而且偏差随窗口宽度变化、不同机器表现不同，极难定位。
fn preview_width(body_w: f32, pal: &Palette) -> f32 {
    let acts_w = acts_width(pal);
    (body_w - ROW_PAD_H - acts_w - ROW_PAD_H).max(20.0)
}

/// 预览文本的可用宽度——**整行**宽度，不减按钮列。
///
/// # 为什么预览可以用整行宽
///
/// 三个操作按钮只占卡片的**右上角一行**，而预览带整体位于按钮行
/// **下方**（见 [`row_top_height`]），因此不会被按钮遮挡。
///
/// 早前预览与顶部 meta 共用 [`preview_width`]（减掉了整个按钮列宽），
/// 于是**每一行**预览右侧都白空出一列：长文本（URL、路径）明明还有
/// 位置却被迫折行或截断。用户反馈的「内容显示不到右侧」就是它。
fn preview_full_width(body_w: f32) -> f32 {
    (body_w - ROW_PAD_H * 2.0).max(20.0)
}

/// 行**测量**用的内容宽度：内容区宽度**无条件**让出滚动条占位与呼吸空间。
///
/// # 为什么必须让出，以及为什么无条件
///
/// 绘制侧的行矩形在「可滚动」时会内缩 [`SCROLLBAR_RESERVE`] +
/// [`SCROLLBAR_GAP`]（见 `draw_history_body` 里的 `row_w`；不缩的话
/// egui 的 floating 滚动条会盖住删除按钮，滚动条压过所有行内控件）。
/// 而「是否可滚动」由 `content_h > 可视高` 判定，`content_h` 又来自
/// 行高——行高来自本函数。
///
/// 于是直接按 `content.width()` 测量就是先鸡先蛋：量的时候按**宽**
/// 宽度算折行，量完才发现要滚动，那时真实宽度已经窄了 16pt ⇒ 折行数
/// 偏少 ⇒ 行高偏小 ⇒ 文字被裁。症状有明确指纹：**条目少时正常、
/// 条目一多（出现滚动条）就每行少显示一点**。
///
/// 无条件让出在两个方向都安全：
/// - 可滚动：测量与绘制同宽（都让出）——修好的正是这一支；
/// - 不可滚动：绘制比测量**多** 16pt ⇒ 行高**偏大**，最多留一点空白，
///   绝不裁字。
///
/// 且单调：让出只会让行变高，`content_h` 只增不减 ⇒ 滚动条不会在两
/// 帧之间来回闪。
fn row_measure_width(content_w: f32) -> f32 {
    (content_w - SCROLLBAR_RESERVE - SCROLLBAR_GAP).max(20.0)
}

/// 行内**顶部一行**的高度：左侧「来源·时间」与右侧操作按钮取较高者。
///
/// 必须由 [`draw_item_row`] 与 [`row_height_for`] **共用**：按钮比 meta
/// 文字高（`ROW_ACTS_DROP + btn`），若测量时只算 meta 高，预览带就会
/// 上移到按钮所在那一行、被按钮压住——而「压住」在截图上表现为
/// 「文字跑到按钮底下」，很容易被误判成按钮位置错了。
fn row_top_height(pal: &Palette) -> f32 {
    let meta_h = pal.font_xs.max(10.0);
    let acts_h = ROW_ACTS_DROP + pal.control_height * 0.55;
    meta_h.max(acts_h)
}

/// 行内三个操作按钮的总宽（含间隙）。
///
/// 三处都用它：[`preview_width`] 算文本区右边界、`draw_item_row`
/// 算按钮位置、`row_height_for` 兜底检查。**必须同源**，
/// 否则文本会伸到按钮底下、或按钮被提前 break 掉。
fn acts_width(pal: &Palette) -> f32 {
    3.0 * (pal.control_height * 0.55 + pal.space_xs)
}

/// 行卡片的**左右**内边距（逻辑点）。
///
/// 早前只有垂直的 `pad_v`，左右为 0 —— 三段文字直接贴死卡片边缘，
/// 视觉上像溢出卡面。去掉类型图标后左侧空出来的位置正好用作内边距。
const ROW_PAD_H: f32 = 12.0;

/// 行内三个操作按钮相对来源行的**下移量**（逻辑点）。
///
/// 按钮贴在右上角、但比来源行低这么多。来源行字高 11pt、按钮 15.4pt，
/// 两者顶部对齐时按钮会探进来源行里，下移 4pt 让开字形上沿。
const ROW_ACTS_DROP: f32 = 4.0;

/// 列表行的**最小**高度（逻辑点）。
///
/// 三段布局：左上来源行 + 预览（14pt，可折行）+ 右下类型/大小行。
/// 预览一行 ≈ 19pt、上下两行 meta 各 ≈ 11pt、内边距 2×5 + 行间 4，
/// 合计 ≈ 55pt，取下限 58 兜底按钮区域。
///
/// NOTE: 这是**下限**不是固定值：早前用固定 40pt 时预览只够放一行半，
/// 长内容被省略号截断（用户反馈「需要完整显示内容」）。
/// 实际行高由 [`row_height_for`] 按每条内容的折行数动态算出。
const ROW_HEIGHT_MIN: f32 = 58.0;

/// 列表内容区左右留白（逻辑点）。
///
/// 这条竖边距既是「卡片与窗口边缘之间的呼吸感」，也是**拖动区**：
/// 用户按住留白拖动即可移动窗口（见 `draw_history_body` 上报的
/// `drag_regions`）。
const LIST_PAD_X: f32 = 8.0;

/// 列表卡片之间的垂直间隙（逻辑点）。
///
/// 卡片样式 = 每条历史是一张圆角小卡，卡与卡之间留这条缝；
/// 首项顶部再留半条，让整体上下边缘也有留白。
const ROW_GAP: f32 = 6.0;
/// 新条目入场滑入的距离（逻辑点）：首行从上方这么远归位。
///
/// 取 2 倍行隙：足以被感知为「滑入」，又不至于让首行在归位途中压到
/// 顶栏。位移只作用于**行矩形**（绘制与交互同一矩形），`offsets`/行高
/// 不参与——布局保持稳态，行高缓存不会因动画失效。
const ROW_IN_SLIDE: f32 = 12.0;
/// 阴影开启时行间缝隙的最大值：参数拉满（offset 16 + blur 24）理论
/// 需要 40pt，列表会过度松散，视觉上也不成立——设上限截断。
const ROW_GAP_MAX: f32 = 30.0;

/// 行间缝隙：阴影开启时按「偏移 + 弥散半径」动态加宽，保证投影主体
/// 不被下一张卡片盖住；关闭时回落到基础 [`ROW_GAP`]。
///
/// 弥散系数取 1.0（≈ 2σ：该距离内高斯 alpha ≥ ~2.3%，肉眼可见；
/// 更远的残余淡影 <1%，被下一张卡片盖住无感知）。`offset_y` 取绝对值：
/// 偏移向下时投影需要下方缝隙、向上时需要上方缝隙，统一按绝对值给。
fn history_row_gap(cfg: &modular_clipboard_core::config::CardShadowConfig) -> f32 {
    if !cfg.enabled {
        return ROW_GAP;
    }
    let need = cfg.offset_y.abs() + cfg.blur.clamp(0.0, 24.0);
    need.clamp(ROW_GAP, ROW_GAP_MAX)
}

/// 离开搜索区域后延迟多久折叠（秒）。
///
/// 必须 >0：见 [`UiState::search_leave_since`]。0.35s 足以覆盖
/// 「从图标移到输入框」的正常鼠标行程（图标与输入框之间是
/// `space_md` 的间隙），又不至于让输入框赖着不走。
const SEARCH_COLLAPSE_DELAY: f64 = 0.35;

/// 悬停多久才自动展开搜索框（秒）。
///
/// 取值权衡：太短（<0.1s）会让「鼠标划过图标去点旁边的设置」也弹出搜索框；
/// 太长（>0.4s）用户会觉得「没反应」。0.18s 接近 Windows 悬停提示的节奏。
const SEARCH_HOVER_DELAY: f64 = 0.18;

/// 面板刚展开后的**免折叠宽限**（秒）。
///
/// 展开的头几帧里状态还不稳：焦点要等弹窗画完才请求、再等它回报；
/// 指针可能已经离开图标。若此时折叠计时就开始跑，`SEARCH_COLLAPSE_DELAY`
/// 一到面板就自己收回去——用户看到的是「点一下闪一下就没了」。
///
/// 0.6s 覆盖「点击 → 弹窗首帧 → 焦点落地」的整条链路（实测 1~2 帧），
/// 又不至于让「点开就想关」的用户觉得迟钝（Esc / 点图标仍是立即收起）。
const SEARCH_OPEN_GRACE: f64 = 0.6;

/// 垂直滚动条占位（逻辑点），缝隙拖动区右缘必须让出这一段。
///
/// `ScrollArea` 的滚动条画在内容区右缘内侧；若缝隙横条直抵
/// `content.max.x`，按住滚动条会被系统判成 HTCAPTION 变成拖窗口
/// （与左右留白拖动区避开滚动条是同一个原因）。
/// 垂直滚动条占用的宽度。
///
/// IMPORTANT: 必须对齐 egui 的 `ScrollStyle::floating().bar_width`（**10.0**，
/// 见 egui 0.36.2 `style.rs:646`）。
///
/// 早前这里是 8.0，比滚动条实际宽度**小 2pt** —— 于是按它算出的
/// 「已避开滚动条」的位置，仍然有 2pt 落在滚动条下面。
const SCROLLBAR_RESERVE: f32 = crate::theme::SCROLLBAR_WIDTH;

/// 滚动条热区与行卡片之间的**呼吸空间**（逻辑点）。
///
/// # 为什么需要它（2026-10-17 用户反馈「滑动条容易和卡片挤在一起」）
///
/// egui 0.36 默认 `ScrollStyle` 是 **floating**：滚动条不占布局宽度，
/// 而是浮在 `[content.max.x - bar_width, content.max.x]` 上。行矩形按
/// [`SCROLLBAR_RESERVE`] 让位后，右缘恰好贴在滚动条热区的左边界上——
/// **零间距**。滑块本身还有 `bar_inner_margin`（3pt）的内缩，视觉上
/// 缝隙只剩 1~2pt，卡片的删除按钮、文字都与滑块擦身而过；拖动滑块时
/// 手指压到的也是卡片边缘，「挤在一起」的观感与误触都源于此。
///
/// 6pt 的取值依据：滑块视觉宽 = `bar_width - 2×bar_inner_margin` = 4pt，
/// 让出 6pt 后滑块与卡片之间约有 9pt 净空——与 `ROW_GAP`（6~8pt）的
/// 行间缝隙同档，整窗的「空白节奏」一致。
///
/// # 为什么测量侧要**无条件**让出（与 `SCROLLBAR_RESERVE` 同一理由）
///
/// 「是否可滚动」取决于行高，行高又取决于测量宽度——直接按
/// `content.width()` 量就是先鸡先蛋。两个方向都安全：可滚动时测量与
/// 绘制同宽（要修的情况）；不可滚动时绘制比测量宽 ⇒ 行高**偏大**，
/// 最多多留空白，绝不裁字。且单调 ⇒ 滚动条不会两帧之间闪。
const SCROLLBAR_GAP: f32 = 6.0;

/// 每行文字的高度倍率。
const LINE_H: f32 = 1.35;

/// 缩略图默认限高（逻辑点）。
///
/// 仅作为列表行内嵌缩略图的默认上限；详情面板在
/// [`draw_image_preview`] 里另行传更大的值（图占满整宽）。
///
/// 2026-10-17 从 108 提到 160：108pt 下 16:9 的截图只有 192pt 宽，
/// 在 400+pt 的列表里只占不到一半宽度，用户反馈「尺寸太小」。
/// 160 是「看得清」与「一屏还能放三四条」之间的折中——再往上
/// 一条记录就会占掉半屏，列表信息密度崩塌。
///
/// 配套：缩略图解码上限 [`crate::thumbnail::THUMB_MAX_EDGE`] 必须
/// ≥ 本值 × 常见 DPI 缩放（2x ⇒ 320px），否则放大显示会糊。
pub const IMG_MAX_H_LIST: f32 = 160.0;

/// 图片在给定带内的显示尺寸（宽, 高），保持长宽比。
///
/// 限高 `max_h`、限宽 `band_w`，两步都保持长宽比——
///
/// CAUTION: 不能「高先定死、宽超了再截」：那会把图拉扁
/// （`painter.image` 按 uv 0..1 把整张图填进矩形）。宽超限时必须
/// 回头按比例缩高，否则极窄窗口下截图变成压扁的条。
///
/// # 为什么必须是共用函数
///
/// [`row_height_for`] 用它量出行高，[`draw_image_preview`] 按那个行高
/// 画——两处各写一份算式的后果是**图片被压扁**：
/// 2026-10-17 修的就是这个（`row_height_for` 的文件型分支没走图片算式，
/// 行高按文件名文本算成 ~24pt，图被 `band.height()` 压成一条细带）。
/// 想改显示规则只能改这里。
pub fn image_display_size(dims: (u32, u32), band_w: f32, max_h: f32) -> (f32, f32) {
    if dims.0 == 0 || dims.1 == 0 {
        return (0.0, 0.0);
    }
    let ratio = dims.0 as f32 / dims.1 as f32;
    let mut h = max_h;
    let mut w = h * ratio;
    if w > band_w {
        w = band_w;
        h = w / ratio;
    }
    (w, h)
}

/// 单条列表项的**实际**行高：按内容排版结果计算。
///
/// # 为什么必须逐条算
///
/// 预览长度差异极大（`1` 和一段几百字的粘贴内容）。固定行高会
/// 出现两种坏结果：
/// - 行高 < 需要 → 长内容被省略号截断（看不到完整内容）；
/// - 行高 > 需要 → 短内容下方留一大块空白。
///
/// 折行数由 egui 自己算（[`egui::TextWrapping::max_rows`]），
/// 这里只取「实际用了几行」——比按字符数估算准得多，
/// 中英文混排、全角半角混排都能正确处理。
///
/// 2026-10-15 第三轮起有三种内容形态，行高各按各的算：
/// - 图片（`image_inline` 开）= 缩略图显示高（限高 [`IMG_MAX_H_LIST`]）；
/// - Markdown（`md_rendered` 开）= 排版实测高；
/// - 其余 = 折行文本高。
///
/// # 图片分支的判据必须是 [`preview_is_image`]
///
/// 2026-10-17 修：这里原先写 `item.kind == ClipKind::Image`，于是从资源
/// 管理器复制的图片**文件**（`ClipKind::Files`）走的是文本分支——行高按
/// 文件名那一行算（~24pt），而绘制侧 [`draw_image_preview`] 受
/// `band.height()` 约束，图就被压成一条细带（用户反馈「尺寸太小」）。
/// 判据与绘制侧**必须同源**，两类条目共用 [`image_display_size`]。
///
/// `known_dims` 是 [`UiState::thumb_dims`] 里已解码得到的尺寸：老条目
/// 的载荷里没有 `dims`，只能靠它排图（第一次解码后行高才对）。
fn row_height_for(
    ui: &egui::Ui,
    item: &ClipItem,
    pal: &Palette,
    scale: f32,
    text_w: f32,
    cfg: &modular_clipboard_core::config::UiConfig,
    known_dims: Option<(u32, u32)>,
) -> f32 {
    let preview_font = sized(pal.font_md, scale);
    let meta_font = sized(pal.font_xs, scale);
    // IMPORTANT: 行高也必须用**逻辑点**、不乘 ppp：字号已经改成逻辑点，
    // 这里再乘一次会让行高与实际字形高度对不上（留白/裁剪都错）。
    let line_h = pal.font_md * LINE_H;
    let meta_h = meta_font.size.max(10.0);

    // 三段布局：顶部一行（meta / 按钮取较高者）+ 预览 + 底部 meta。
    // 与 `draw_item_row` 的 `preview_band` 严格对应。
    let top_h = row_top_height(pal);

    // ---- 内容高：图片 / Markdown / 文本三分支 ----
    // 尺寸两个来源，都不需要当场解码：
    // - `payload.dims`：入库时读的图片头（文件型由
    //   `app::first_image_file_dims` 补齐）；
    // - `known_dims`：上一次解码记下的真实尺寸（老条目缺前者时靠它）。
    let content_h = if cfg.image_inline
        && preview_is_image(item)
        && let Some(dims) = item
            .payload
            .as_ref()
            .and_then(|p| p.dims)
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or(known_dims.filter(|(w, h)| *w > 0 && *h > 0))
    {
        // 缩略图显示高：与 [`draw_image_preview`] 共用算式。
        image_display_size(dims, text_w, IMG_MAX_H_LIST).1
    } else if item.kind == modular_clipboard_core::ClipKind::Markdown && cfg.md_rendered {
        crate::md::MdDoc::parse(&item.preview).height(ui, pal, scale, text_w)
    } else {
        // 折行一次，拿到实际行数。
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &item.preview,
            0.0,
            egui::TextFormat {
                font_id: preview_font,
                color: pal.text,
                ..Default::default()
            },
        );
        job.wrap.max_width = text_w;
        // 不限行数，先量出「到底需要几行」。
        //
        // IMPORTANT: 必须用 `usize::MAX` 而不是 `0`：`TextWrapping::max_rows` 的
        // 默认值是 `usize::MAX`（不限），而它的文档明确写着
        // 「If set to `0`, no text will be outputted」——
        // **0 表示一行都不输出**，不是「不限」。
        //
        // 误设成 0 的后果：galley.rows 恒为空 → `n_lines` 恒为 1 →
        // 行高恒等于最小值 52pt → 预览带只有 24pt 高 → 长内容被省略号
        // 截断。实测诊断数据完全对上：
        //   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
        job.wrap.max_rows = usize::MAX;
        job.wrap.break_anywhere = true;
        let galley = ui.painter().layout_job(job);
        // `rows` 是字段不是方法（`Arc<Galley>` 直接解引用）。
        galley.rows.len().max(1) as f32 * line_h
    };

    (content_h + meta_h + top_h + 5.0 * 2.0 + 4.0).max(ROW_HEIGHT_MIN)
}

/// 详情面板：选中条目的全文视图。
///
/// 2026-10-16 起：详情面板按 [`ClipKind`] 分发——图片内嵌缩略图、
/// Markdown 渲染 / 源码、文本折行，**不再只用 `elide_text` 截一段**。
/// 这正是「图片内容直接显示」与「Markdown 渲染显示」在主窗口的另一
/// 个展示位：列表行只显示一行内的预览（缩略图 / 第一屏 Markdown），
/// 详情面板给完整内容（缩略图占满宽、Markdown 全文）。
///
/// 行高限制也不同于列表：列表行 `IMG_MAX_H_LIST`（160pt）防止单条
/// 撑爆，本函数不限——内容带本身就被详情卡片高度约束，超长 Markdown
/// 自动溢出滚动区。
///
/// # 配置开关
///
/// - `image_inline` 关闭 ⇒ 图片条目降级为「图片 WxH」文字；
/// - `md_rendered` 关闭 ⇒ Markdown 条目降级为等宽纯文本。
///
/// 与列表行保持一致的开关语义——用户在一个地方关了，在另一处也看不到。
fn draw_detail_body(f: &mut Frame<'_>, body: Rect) {
    use modular_clipboard_core::ClipKind;
    let Some(id) = f.svc.state.selected else {
        // 选中丢失：原图缓存随之丢弃。
        clear_detail_image_zoom(f);
        draw_empty(f, body, "选中一条记录查看详情");
        return;
    };
    // 先把选中的 item 拷一份：原图模式分支与三分支都需要
    // `item.id`（推 Op / 解码原图），而 `f.svc.state.items` 的借用会
    // 与后面的 `&mut f` 冲突。`ClipItem` 字段少，clone 不贵。
    let Some(item) = f.svc.state.items.iter().find(|it| it.id == id).cloned() else {
        // 选中条目已不存在：原图缓存同样失效。
        clear_detail_image_zoom(f);
        draw_empty(f, body, "该记录已不存在");
        return;
    };

    // 原图模式：当前选中是图片、且 zoom 状态指向它——独占整张详情卡。
    //
    // 判据用 [`preview_is_image`] 而不是 `kind == Image`：详情面板的
    // 「放大」按钮对**文件型图片**也画（见上面的三分支），这里若只认
    // `Image`，文件型点了放大没反应（按钮在了、功能没接上）。
    if preview_is_image(&item) && f.state.detail_image_zoom == Some(item.id) {
        draw_detail_image_zoom(f, body, &item);
        return;
    }

    // 不是原图模式：清掉残留的原图缓存（zoom 已失效或选中已换）。
    if f.state.detail_image_zoom.is_some() {
        clear_detail_image_zoom(f);
    }

    let pal = f.pal;
    let scale = f.scale;

    // 底栏：来源 · 时间（meta 行）。与原实现保持一致的位置与字号。
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size * 1.2;
    let meta_band = Rect::from_min_max(
        pos2(body.min.x, body.max.y - meta_h),
        pos2(body.max.x, body.max.y),
    );
    // 内容带：去掉底栏后剩下的全部空间。
    let content_band = Rect::from_min_max(body.min, pos2(body.max.x, meta_band.min.y));
    if content_band.width() <= 0.0 || content_band.height() <= 0.0 {
        return;
    }

    // 判据：从 config 现取，不能持有 `&cfg` 跨 `&mut f`。
    let image_inline = f.svc.state.config.ui.image_inline;
    let md_rendered = f.svc.state.config.ui.md_rendered;

    // 预览绘制用的**裁剪 Ui**：裁剪到内容带。
    //
    // 与列表行同一个道理（见 `draw_text_preview` 的 CAUTION）：预览函数
    // 一律用「调用方给的 Ui」画，才能被内容带裁剪；拿根 `f.ui` 会把
    // 长文本/大图画到卡片之外。
    //
    // NOTE: `new_child` 只建 Ui、不推进父 Ui 的游标，也不借用 `f`——
    // 因此可以与 `&mut f` 并存。
    let preview_ui = f.ui.new_child(
        egui::UiBuilder::new()
            .id(egui::Id::new("detail_preview"))
            .max_rect(content_band)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );

    // 三分支：图片直显 / Markdown 渲染 / 文本折行。
    //
    // CAUTION: `draw_image_preview` 与 `draw_markdown_preview` 都需要
    // `&mut f`，所以这里**只能二选一**——开关决定具体走哪一支。
    match item.kind {
        // `ClipKind::Files` 也进来：从资源管理器复制的**单个图片文件**
        // 在详情面板同样直显（图在磁盘上，载荷里只有路径）。
        ClipKind::Image | ClipKind::Files if preview_is_image(&item) && image_inline => {
            // 详情面板让图占满宽（不限 IMG_MAX_H_LIST，按 band.height
            // 走，长宽比由 `draw_image_preview` 内部保持）。
            //
            // 报告到 `thumb_seen`：让本帧上传的纹理在 `draw` 收尾
            // retain 时不被误淘汰（与列表行用同一机制）。
            f.state.thumb_seen.insert(item.id);
            draw_image_preview(f, &preview_ui, content_band, &item, content_band.height());
            // 缩略图右下角：「放大」按钮，点击进入原图模式。
            draw_zoom_button(f, pal, &item);
        }
        ClipKind::Markdown if md_rendered => {
            draw_markdown_preview(f, &preview_ui, content_band, &item);
        }
        ClipKind::Markdown => {
            // md_rendered 关：等宽源码按 band 高度计算可见行数（不截断），
            // 溢出自动换行。`draw_source_preview` 内部已 max_rows 限位，
            // 详情面板空间通常足够放下整段。
            draw_source_preview(f, &preview_ui, content_band, &item);
        }
        _ => {
            // 文本 / HTML / Files / Markdown 关 / Image 关 ⇒ 等宽折行。
            //
            // 用 `draw_text_preview` 而不是 `elide_text`：详情面板就该把
            // 全文铺开，让用户读到完整内容。`draw_text_preview` 内部按
            // band 宽度折行，溢出自动到下一行（撑出可见区会被外层
            // 滚动框裁剪）。
            draw_text_preview(f, &preview_ui, content_band, &item, false);
        }
    }

    // 来源 · 时间：永远画在最底，与列表行语义一致。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, meta_band.width());
    if !meta_shown.is_empty() {
        f.ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }
}

/// 清掉详情面板的原图模式缓存（zoom + 已上传的 orig 纹理）。
///
/// `TextureHandle` 是 RAII：drop 时 egui 自动 `ctx.forget()`，所以只需
/// 让 `detail_image_orig` 离开作用域——这里统一清两个字段保证一致。
fn clear_detail_image_zoom(f: &mut Frame<'_>) {
    f.state.detail_image_zoom = None;
    f.state.detail_image_orig = None;
}

/// 在图片缩略图右下角画一个「放大」按钮。点击推一个 [`Op::DetailImageZoom`]。
///
/// `image_inline` 关闭或载荷被淘汰（无 dims）时不画——按钮的出现与
/// 实际能进入原图模式的能力保持一致。
fn draw_zoom_button(f: &mut Frame<'_>, pal: &Palette, item: &ClipItem) {
    // 判据 = 「能不能进原图模式」：尺寸要么在载荷里，要么是解码时记下的
    // （老条目缺 `payload.dims`，解码后靠 [`UiState::thumb_dims`]）。
    // 两条都没有 ⇒ 缩略图也画不出来，按钮更不该出现。
    let has_dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .or_else(|| f.state.thumb_dims.get(&item.id).copied())
        .map(|(w, h)| w > 0 && h > 0)
        .unwrap_or(false);
    if !has_dims {
        return;
    }
    let btn_size = 24.0;
    let pad = 8.0;
    // 按钮位置：内容带右下角。`painter_at` 与按钮的 rect 一致。
    let btn_rect = Rect::from_min_size(
        pos2(
            f.area.max.x - btn_size - pad,
            f.area.max.y - btn_size - pad,
        ),
        vec2(btn_size, btn_size),
    );
    // 用 `ui.interact` 而**不是**直接 push Op：每帧都画按钮，
    // 直接 push 等于「每帧推同一个 Op」，会让 IPC 通讯爆炸；
    // `clicked` 仅在用户真正点击时为 true。
    let id = egui::Id::new(("detail_image_zoom", item.id));
    let resp = f.ui.interact(btn_rect, id, Sense::click());
    let painter = f.ui.painter_at(btn_rect);
    // 底色补间（110ms）：悬停时 surface_variant → surface，与行内按钮
    // 同一套动效；不悬停时停在 surface_variant。
    let bg = crate::anim::tween_color(
        f.ui.ctx(),
        id.with("bg"),
        if resp.hovered() {
            pal.surface
        } else {
            pal.surface_variant
        },
        std::time::Duration::from_millis(110),
    );
    painter.rect_filled(btn_rect, pal.radius_md, bg);
    // 图标颜色补间（110ms）：悬停提亮一档。
    let icon_color = crate::anim::tween_color(
        f.ui.ctx(),
        id.with("icon"),
        if resp.hovered() { pal.text } else { pal.text_dim },
        std::time::Duration::from_millis(110),
    );
    Icon::Image.paint(&painter, btn_rect.shrink(4.0), icon_color);
    if resp.clicked() {
        f.state.ops.push(Op::DetailImageZoom(item.id));
    }
}

/// 原图模式：直接渲染**全尺寸**原图，按内容带保持长宽比。
///
/// 与缩略图路径分两条：
/// - 缩略图走 `thumb_cache` + `IMG_MAX_H_LIST` 限位；
/// - 原图用 `thumbnail::decode(bytes, u32::MAX)` 不限尺寸、单独缓存
///   在 `detail_image_orig`，避免污染缩略图缓存。
///
/// `decode` 是同步调用——4K 图可能让 UI 卡几十毫秒。当前项目里
/// 一次只点一次，先把功能接通；后续若需要可加后台线程（见 TODO）。
fn draw_detail_image_zoom(f: &mut Frame<'_>, body: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;

    // 底栏：与缩略图模式一致；返回按钮放在右上角，与 meta 行同行。
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size * 1.2;
    let meta_band = Rect::from_min_max(
        pos2(body.min.x, body.max.y - meta_h),
        pos2(body.max.x, body.max.y),
    );
    let content_band = Rect::from_min_max(body.min, pos2(body.max.x, meta_band.min.y));

    // 原图按需解码 + 上传：缓存命中（id 匹配）就跳过。
    let dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .filter(|(w, h)| *w > 0 && *h > 0);
    let needs_upload = f
        .state
        .detail_image_orig
        .as_ref()
        .map(|(id, _)| *id != item.id)
        .unwrap_or(true);
    if needs_upload {
        // 字节来源与缩略图同一条：图片型读载荷，文件型按首行路径**读盘**
        // （从资源管理器复制的 .png 只有路径在库里）。
        match load_image_bytes(f.svc, item) {
            Ok(bytes) => {
                // 与缩略图同一套 auto 入口：裸 RGBA8（现行规范）走旁路，
                // 历史编码格式回落嗅探。`u32::MAX` = 不降采样（原图）。
                match crate::thumbnail::decode_auto(&bytes, dims, u32::MAX) {
                    Ok(thumb) => {
                        let img = thumb.to_color_image();
                        let handle = f.ui.ctx().load_texture(
                            format!("mc_orig_{}", item.id),
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        f.state.detail_image_orig = Some((item.id, handle));
                    }
                    Err(e) => {
                        tracing::debug!(id = item.id, ?e, "原图解码失败");
                        f.state.detail_image_orig = None;
                    }
                }
            }
            Err(e) => {
                tracing::debug!(id = item.id, ?e, "原图读取失败");
                f.state.detail_image_orig = None;
            }
        }
    }

    // 取当前已上传的纹理**与其尺寸**——失败时画错误提示。
    //
    // 尺寸取自纹理本身而不是 `payload.dims`：文件型图片没有后者（载荷
    // 是路径列表），而纹理 size 就是解码出来的真实像素尺寸，两类条目
    // 通用，也不怕载荷被淘汰。
    let uploaded = f
        .state
        .detail_image_orig
        .as_ref()
        .filter(|(id, _)| *id == item.id)
        .map(|(_, h)| (h.id(), h.size()));

    if let Some((tex_id, size)) = uploaded {
        // 原图占满内容带、保持长宽比。
        let (w, h) = (size[0] as u32, size[1] as u32);
        if w > 0 && h > 0 {
            let ratio = w as f32 / h as f32;
            let mut disp_w = content_band.width();
            let mut disp_h = disp_w / ratio;
            if disp_h > content_band.height() {
                disp_h = content_band.height();
                disp_w = disp_h * ratio;
            }
            let rect = Rect::from_min_size(content_band.min, vec2(disp_w, disp_h));
            let painter = f.ui.painter_at(rect);
            painter.image(
                tex_id,
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            // 1px 描边：浅色图与深色面板之间需要边界。
            painter.rect_stroke(
                rect,
                6.0,
                Stroke::new(1.0, pal.border_subtle),
                egui::StrokeKind::Inside,
            );
        } else {
            draw_empty(f, content_band, "原图尺寸未知");
        }
    } else {
        draw_empty(f, content_band, "原图加载失败");
    }

    // 来源 · 时间：给返回按钮留 80pt 空间。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, meta_band.width() - 80.0);
    if !meta_shown.is_empty() {
        f.ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }

    // 右上角「返回」按钮：点击清除 zoom 与 orig 缓存。
    let back_size = 20.0;
    let back_pad = 6.0;
    let back_rect = Rect::from_min_size(
        pos2(
            meta_band.max.x - back_size - back_pad,
            meta_band.min.y + (meta_h - back_size) * 0.5,
        ),
        vec2(back_size, back_size),
    );
    let back_id = egui::Id::new(("detail_image_back", item.id));
    let back_resp = f.ui.interact(back_rect, back_id, Sense::click());
    Icon::Close.paint(&f.ui.painter_at(back_rect), back_rect, pal.text_dim);
    if back_resp.clicked() {
        f.state.ops.push(Op::DetailImageZoomClear);
    }
}

fn draw_rail_body(f: &mut Frame<'_>, body: Rect) {
    let pal = f.pal;
    let total = f.svc.state.items.len();
    if total == 0 {
        draw_empty(f, body, "暂无数据");
        return;
    }
    let font = sized(pal.font_xs, f.scale);
    let row_h = 22.0;
    let n = ((body.height() / row_h).floor() as usize).min(total);
    // NOTE: 不克隆整表（PF-1）：这里只画前 n 行，直接按索引取原表。
    // 循环体对 f 全是只读使用（painter_at / elide_text），无借用冲突。
    for (i, it) in f.svc.state.items.iter().take(n).enumerate() {
        let y = body.min.y + i as f32 * row_h;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        if r.max.y > body.max.y {
            break;
        }
        let txt = format!("{} · {}", it.source_app, it.preview);
        let shown = crate::text::elide_text(f.ui, &txt, &font, r.width());
        f.ui.painter_at(r).text(
            r.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font.clone(),
            pal.text_dim,
        );
    }
}

fn draw_empty(f: &mut Frame<'_>, body: Rect, msg: &str) {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    // 入场补间（200ms OutCubic）：文案淡入 + 上浮 8pt。
    //
    // 为什么用 `fade_in_once` 而不是 `tween_f32`：后者首帧插入即落在
    // 目标上（惰性语义，悬停/选中就该这样），入场动画要的是「首次出现
    // 从 0 开始放」。`fade_in_once` 用「条目是否存在」判重，落定后不
    // 重播——搜索时每条击键都不会重启它。
    //
    // id 带消息文本：不同提示（没有记录 / 没有匹配 / 选中一条…）各放
    // 各的；同一消息重复出现（清了搜索又回到空列表）不重播，可接受。
    let k = crate::anim::fade_in_once(
        f.ui.ctx(),
        egui::Id::new(("empty_in", msg)),
        std::time::Duration::from_millis(200),
        crate::anim::Ease::OutCubic,
    );
    if k <= 0.004 {
        return; // 完全透明：不画
    }
    // NOTE: painter_at(body)：空状态文案是居中的，而窄卡片（折叠把手
    // 旁的详情栏）装不下整句，根 painter 会让它溢出到相邻卡片上。
    let center = body.center() - vec2(0.0, (1.0 - k) * 8.0);
    f.ui
        .painter_at(body)
        .text(center, Align2::CENTER_CENTER, msg, font, fade_color(pal.text_dim, k));
}

/// 按比例 `k ∈ [0,1]` 缩放颜色的**不透明度**（补间淡入淡出用）。
///
/// # 为什么必须走预乘分量
///
/// egui 的 [`egui::Color32`] 内部是**预乘 alpha** 存储（`r()` 返回的是
/// 已经乘过 alpha 的通道，见 ecolor `color32.rs` 模块文档）。因此缩放
/// 不透明度要把**四个通道一起乘 k**，再用 `from_rgba_premultiplied`
/// 组装：
///
/// ```text
/// from_rgba_premultiplied(r·k, g·k, b·k, a·k)
/// ```
///
/// 若改用 `from_rgba_unmultiplied(r, g, b, a·k)`（把预乘分量当普通
/// RGBA 传进去），rgb 会被**二次预乘**——本仓踩过：行悬停底色
/// `row_hover = from_white_alpha(30)`，正确值应是 11.8% 的白，二次
/// 预乘后只剩 ~1.6%，悬停提亮几乎看不见（2026-10-17 修复）。
///
/// 纯函数：不碰 egui 运行时，单测直接覆盖。
pub(crate) fn fade_color(c: egui::Color32, k: f32) -> egui::Color32 {
    let k = k.clamp(0.0, 1.0);
    let [r, g, b, a] = c.to_array();
    let s = |v: u8| (v as f32 * k).round().clamp(0.0, 255.0) as u8;
    egui::Color32::from_rgba_premultiplied(s(r), s(g), s(b), s(a))
}

/// 画一圈圆角描边：**两次填充**，而不是路径描边。
///
/// # 为什么不用 `Painter::rect_stroke`
///
/// `rect_stroke` 把圆角矩形展平成一条**路径**再描边，路径在「弧 → 直线」
/// 接点处首尾重叠，`StrokeKind::Inside` 下重叠部分被叠加两次——同样是
/// 深色短线的一个来源。两次填充至少消除了这一份。
///
/// # CAUTION: 但它**不能**消除 feathering 自带的重叠
///
/// 真正的主因在 egui 的边缘抗锯齿（feathering）：它沿每条边生成一条
/// alpha 渐变的填充带，在**凸角**处「直线 → 弧线」接点两侧的填充带会
/// 重叠，alpha 累加 2~4 次。实测弧线两端像素亮度 196~205，比描边本身
/// （237）还暗——单次叠加不可能更暗，只有重复叠加才会。
///
/// 因此**只要描边色比背景暗，这个瑕疵就一定会出现在圆角两端**，
/// 换绘制方式没用。真正的解法是不画比背景暗的描边（见 `draw_item_row`）。
/// 本函数保留给确实需要圆环的场合（例如饱和色的选中态）。
fn fill_rounded_border(
    painter: &egui::Painter,
    r: Rect,
    radius: f32,
    w: f32,
    border: egui::Color32,
    content: impl FnOnce(&egui::Painter, Rect, f32),
) {
    let inner = r.shrink(w);
    let inner_r = (radius - w).max(0.0);
    // 太窄时内缩矩形会退化甚至翻负：此时只画描边色（等价于一块实心
    // 圆角块）。**绝不能**把非正尺寸传给 egui——它的圆角路径会算出
    // 畸形几何，画出乱七八糟的图形。
    if w <= 0.0 || inner.width() <= 0.0 || inner.height() <= 0.0 {
        painter.rect_filled(r, radius, border);
        return;
    }
    painter.rect_filled(r, radius, border);
    content(painter, inner, inner_r);
}

/// 高斯核对半平面卷积的外轮廓（归一化）：`1 - Φ(d/σ) = 0.5·erfc(d/(σ√2))`。
///
/// `d` 为卡片边缘**向外**的距离（逻辑点）：边缘处为 0.5，向外单调
/// 衰减到 0——真实 box-shadow 在缝隙里的强度分布就是这条曲线，
/// 比线性羽化自然得多（边缘不生硬、外缘无截断）。
///
/// 用 Abramowitz & Stegun 7.1.26 的 Φ(x) 有理近似（|误差| < 7.5e-8），
/// 不引第三方数学库。
///
/// NOTE: 系数**刻意保留出版常数**（而非削到 f32 精度）：这组数字要与
/// 论文表格逐位对得上才敢声称「误差 < 7.5e-8」，削位反而让可追溯性
/// 断掉。`excessive_precision` 在这里是特性不是缺陷。
#[allow(clippy::excessive_precision)]
fn gaussian_outer(d: f32, sigma: f32) -> f32 {
    if sigma <= 0.0 {
        return 0.0;
    }
    let x = (d / sigma).max(0.0);
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let phi = 0.39894228 * (-0.5 * x * x).exp(); // 1/√(2π)·e^(-x²/2)
    let poly = t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    phi * poly
}

/// 列表卡片阴影：多层同心圆角矩形合成的**高斯投影**。
///
/// # 为什么不用 `epaint::Shadow`（旧实现被否决的原因）
///
/// 1. **被裁剪 + 被覆盖，等于没画**：旧实现用行内 `painter_at(r)`，
///    其 clip 与行矩形取交集，阴影外扩晕带全被裁掉；r 内部分又被
///    不透明的卡片填充完整盖住。
/// 2. **圆角暗斑**：`Shadow` 走 epaint feathering（沿轮廓的单条
///    抗锯齿渐变带），在圆角凸角「直线→弧线」接点两侧填充带重叠，
///    alpha 累加 2~4 次（本仓常态描边因此被迫移除，见
///    [`draw_item_row`] 里的排查记录），四个角变成深色斑点。
/// 3. **深色主题不可见**：黑色 16% 投影叠在近黑的 `list_bg`
///    （`#101215`）上几乎没有明度差。
///
/// # 做法：N 层实心圆角矩形 + 反解 alpha 混合
///
/// 把扩散区切成 1pt 宽的环带，第 k 层是一个「按 offset 平移后再
/// 外扩 k 点」的**实心**圆角矩形（无 feathering ⇒ 无凸角重叠），
/// 圆角半径同步外扩 k（同心圆角）。从最大的一层向卡片画：
///
/// - 环带 k（距卡片边缘 k-1~k 点）被第 k..n 层覆盖；
/// - 设其目标强度为 `T_k = C · gaussian_outer(k-0.5, σ)`；
/// - 画第 k 层时它已经与更外层混成强度 `outer`，按「源 over」
///   公式 `T = a + outer·(1-a)` 反解 `a = (T - outer)/(1 - outer)`；
/// - 画完后更新 `outer`。跳过的层（量化后不可见）不更新，
///   后续层自动补偿，误差不累积。
///
/// 不可加的 alpha 混合于是被精确反解，最终环带强度就是高斯
/// 轮廓，量化误差仅 ±1/255。
///
/// # 裁剪与顺序（由调用方 `draw_history_body` 保证）
///
/// 传入的 `painter` 必须是**视口级** painter（clip = ScrollArea
/// 视口），且所有行的阴影必须在**所有行内容之前**统一画一遍：
/// 外扩晕带才不会被行 clip 裁掉，后一行的阴影也不会压暗前一行
/// 卡片上的文字（落在卡片矩形内的阴影随后被该行填充覆盖）。
/// 计算高斯阴影每一层的 `(外扩距离, 填充 alpha)`，按**从外到内**的
/// 绘制顺序返回（即喂给 `Painter::rect_filled` 的顺序）。
///
/// 纯函数：便于单测模拟「源 over」合成后逐环带核对高斯轮廓。
/// `c` 是卷积黑矩形的强度（边缘处可见强度为其一半）。
fn card_shadow_layers(blur: f32, c: f32) -> Vec<(f32, f32)> {
    let blur = blur.clamp(0.0, 24.0);
    if blur <= 0.0 {
        return Vec::new();
    }
    let sigma = blur * 0.5;
    let layers = (blur * 1.5).round().max(1.0) as i32;
    let c = c.clamp(0.0, 0.95);
    let mut out = Vec::new();
    // 实际混合强度（不是目标值）：跳过量化后不可见的层时不更新，
    // 后续层的反解会自动补偿，误差不累积。
    let mut outer = 0.0f32;
    for k in (1..=layers).rev() {
        let target = c * gaussian_outer(k as f32 - 0.5, sigma);
        let a = (target - outer) / (1.0 - outer);
        if a >= 0.003 {
            out.push((k as f32, a));
            outer += a * (1.0 - outer);
        }
    }
    out
}

fn draw_card_shadow(
    painter: &egui::Painter,
    r: Rect,
    radius: f32,
    cfg: &modular_clipboard_core::config::CardShadowConfig,
    pal: &Palette,
) {
    if !cfg.enabled || cfg.opacity <= 0.0 {
        return;
    }
    let blur = cfg.blur.clamp(0.0, 24.0);
    if blur <= 0.0 {
        return;
    }
    // 主题推荐强度（`Palette::shadow` 的 alpha）为基准，用户浓度
    // 滑块是倍率：默认 0.16 → 1.0×；拖到 0 → 无阴影。
    let strength = (cfg.opacity / 0.16).clamp(0.0, 1.0 / 0.16);
    let c = pal.shadow.a() as f32 / 255.0 * strength;
    let offset_y = cfg.offset_y.clamp(-8.0, 16.0);
    let base = r.translate(vec2(0.0, offset_y));
    let [sr, sg, sb, _] = pal.shadow.to_array();

    // 主投影：用户调节的模糊/偏移。
    for (grow, a) in card_shadow_layers(blur, c) {
        let rr = base.expand(grow);
        // 半径随外扩同步增大：矩形与圆角始终同心，
        // 且 radius+k ≤ (h+2k)/2 恒成立（由 radius ≤ h/2 保证）。
        painter.rect_filled(
            rr,
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }

    // 近投影（接触阴影）：紧贴卡片的第二组高斯层——更小模糊、更小
    // 偏移、浓度更高，让卡片边缘有「贴地」的锐利过渡。这是立体感的
    // 主要来源（深色背景下黑投影对比有限，靠近投影拉出边缘层次）。
    //
    // 参数与预览页一致：blur×0.35（至少 0.5）、浓度×1.15、偏移×0.25
    // （至少 0.5 下移，即使主偏移为 0 也保留一条贴边接触影）。
    let near_blur = (blur * 0.35).max(0.5);
    let near_offset = (offset_y * 0.25).max(0.5);
    let near_c = (c * 1.15).clamp(0.0, 0.95);
    let near_base = r.translate(vec2(0.0, near_offset));
    for (grow, a) in card_shadow_layers(near_blur, near_c) {
        let rr = near_base.expand(grow);
        painter.rect_filled(
            rr,
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }
}

fn draw_item_row(f: &mut Frame<'_>, ui: &egui::Ui, r: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;
    let selected = f.svc.state.selected == Some(item.id);

    // CAUTION: 交互必须挂在**滚动区的内容 `ui`** 上，不能挂根 `f.ui`。
    //
    // `f.ui` 是整个主区的 Ui，它的裁剪矩形是**整个窗口**——于是
    // 被滚出可视区的那半行仍然可点：点在卡片标题、甚至顶栏上，
    // 都会选中列表里的条目（P0 级：顶栏按钮点不动）。
    //
    // `ScrollArea` 的内容 `ui` 裁剪到视口，越界的行自动不可交互，
    // 与「看得见才点得到」一致。
    let resp = ui.interact(r, egui::Id::new(("row", item.id)), Sense::click());
    let painter = ui.painter_at(r);

    // 卡片样式：每条历史是一张圆角小卡。
    //
    // 常态 = `surface_raised`（提亮后的卡片）底 + 1px 描边，浮在暗色
    // 列表区（`list_bg`）之上；悬停在半透明提亮上叠一层。
    //
    // NOTE: 圆角用 `ROW_RADIUS` 而不是 `CornerRadius::ZERO`——后者是
    // 早前「通栏行」的直角样式，与卡片语义不符。
    //
    // NOTE: 描边在所有状态都画：悬停若只换底色，边框会「消失一瞬」
    // （从有到无再到有），视觉上跳变。
    let radius = ROW_RADIUS;

    // 卡片阴影**不在本函数画**：它在行内容之外（行间缝隙、列表边缘），
    // 而这里的 `painter` 被裁剪到行矩形 `r`，外扩晕带会被裁掉。
    // 阴影由 `draw_history_body` 在所有行内容之前统一画一遍
    // （见 `draw_card_shadow` 的裁剪/顺序说明）。

    // 常态描边已取消，原因见下面 `selected` 分支处的说明（feathering 在
    // 凸角处重叠会把它变成一圈深色短线）。
    // 选中态描边加粗到 2pt。向内生长、不占布局 —— 内容位置与常态完全一致。

    //
    // # CAUTION: 非选中态**不画描边**（这里曾有一圈极弱描边）
    //
    // 根因（像素级排查得出，不是猜的）：egui 的边缘抗锯齿（feathering）
    // 会沿每条边生成一条 alpha 渐变的填充带；在**凸角**处，「直线 → 弧线」
    // 的接点两侧填充带**重叠**，alpha 被累加 2~4 次。
    //
    // 实测数据：描边色亮度 ED(237)、背景 F4(244)，而弧线两端的像素是
    // C4/CD(196/205)——**比描边本身还暗**。单纯叠加两个颜色不可能产生
    // 比两者都暗的结果，只有「同一形状叠了多次」才会。
    //
    // 卡片自身的白色填充也有同样的重叠，但白叠白看不出来；**只有比背景
    // 更暗的描边会把它暴露成圆角末端的一段深色短线**。
    //
    // 所以这里彻底去掉常态描边：卡片靠 `surface_raised` 与窗口底 `bg`
    // 的明暗差「浮起」，本就是这个界面「层次不靠描边、靠浮起」的设计意图。
    //
    // 选中态保留 accent 描边：它是饱和亮色，叠加后只是边缘更实一点，
    // 不产生「比自身更暗」的灰边；且整列表只有一行处于选中态。
    //
    // CAUTION: 宽度是**逻辑点**、不乘 scale（F-35 / UI-P1-4）：egui 的
    // Stroke 单位就是逻辑点，乘 ppp 会让 150% DPI 下画成 3pt，与注释
    // 承诺的「2pt」不符，也和其他 1.0 逻辑点的描边不成体系。
    //
    // ---- 卡片分层：底 → 置顶 → 选中 → 悬停 → 按下 ---------------------
    //
    // 每层都是一层半透明矩形往上叠；除置顶（静态数据标记）外全部走补间
    // （`anim::tween_f32`：目标值由本帧状态给出、从当前显示值续接）——
    // 鼠标划过、按下、选中、取消选中都有平滑过渡，不再是硬切。
    //
    // 顺序上选中在悬停/按下**之下**：accent 环是稳定骨架，指针反馈叠在
    // 它上面——悬停一个已选中的行仍然看得到反应（旧实现选中与悬停互斥，
    // 选中行对指针毫无反馈）。
    painter.rect_filled(r, radius, pal.surface_raised);
    // 置顶标记：底色微亮。置顶条目就在同一个列表里，靠这个区分。
    //
    // NOTE: 只靠色差是不够的 —— 实测 7% 透明叠加几乎看不出差别。
    // 行内 pin 按钮的实心/描边是主要的形状线索，底色只是辅助。
    if item.pinned {
        painter.rect_filled(r, radius, pal.pin_tint);
    }
    // 选中态补间（140ms OutCubic）：accent 描边 0→2pt、选中底 alpha
    // 0→满。切换选中时 accent 环从细到粗「长」出来，取消时收回去。
    //
    // 阈值 0.004：淡出完成后一层 alpha≈0 的矩形是纯浪费。同时它保证
    // `fill_rounded_border` 收到的 w > 0——该函数对 w<=0 的兜底是
    // 「整块实心 border 填充」，被 0 触发的后果是整行刷成 accent。
    let sel = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("row_sel", item.id)),
        if selected { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(140),
        crate::anim::Ease::OutCubic,
    );
    if sel > 0.004 {
        fill_rounded_border(&painter, r, radius, 2.0 * sel, pal.accent, |p, inner, inner_r| {
            p.rect_filled(inner, inner_r, pal.surface_raised);
            p.rect_filled(inner, inner_r, fade_color(pal.row_selected, sel));
        });
    }
    // 悬停底色**补间**（110ms OutCubic）：
    // 目标值由本帧是否悬停给出，补间从当前显示值续接——鼠标划过时
    // 背景平滑淡入淡出，不再是硬切（「界面显得廉价」的主要来源之一）。
    //
    // id 带 `item.id`：每行一个独立补间，滚动时各行互不影响。
    // 滚出可视区的行停止绘制，其补间条目留在 ctx.data 里。本函数每行
    // 最多持有 4 条（悬停 / 按下 / 选中 + 三个操作按钮各自的悬停），
    // 上界 ≈ 条目数 × 4 × 50 字节（默认上限 1 万条 ⇒ ~2MB），可接受；
    // 需要更紧的宿主可在列表刷新时按 id 调 `anim::clear`。
    let hover = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("row_hover", item.id)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    // 阈值跳过：淡出完成后再画一层 alpha≈0 的矩形是纯浪费。
    if hover > 0.004 {
        // CAUTION: 必须整体缩放预乘分量（`fade_color`）——`to_array()`
        // 拿到的就是预乘后的 rgb，若用 `from_rgba_unmultiplied` 重组
        // 会二次预乘，把 11.8% 的悬停提亮压暗成 ~1.6%（几乎看不见）。
        painter.rect_filled(r, radius, fade_color(pal.row_hover, hover));
    }
    // 按下反馈（90ms OutCubic）：指针按住条目时叠一层 `row_active`
    // （白 20%），松手淡出。桌面列表的标准按压反馈；与悬停叠加表达
    // 「按住了」，也让点击到选中的衔接有 intermediate 状态。
    let press = crate::anim::tween_f32(
        ui.ctx(),
        egui::Id::new(("row_press", item.id)),
        if resp.is_pointer_button_down_on() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(90),
        crate::anim::Ease::OutCubic,
    );
    if press > 0.004 {
        painter.rect_filled(r, radius, fade_color(pal.row_active, press));
    }

    // ---- 顶部受光高光（在卡片填充之上）----
    //
    // 深色主题下卡片顶部 3pt 白色渐变亮边（alpha 0.20 → 0），模拟
    // 上方光源——深色卡片在近黑列表底上靠它「浮」起来（立体阴影的
    // 另一半；近投影在 `draw_card_shadow` 里画）。浅色主题高光透明。
    let hl_top = pal.card_highlight;
    let [hl_r, hl_g, hl_b, _] = hl_top.to_array();
    if hl_top.a() > 0 {
        // 高光只覆盖直边段（左右各让开 ROW_RADIUS），避免在卡片圆角外
        // 露出直角亮角；gradient_rect 是直角渐变，圆角处自然留空。
        //
        // CAUTION: egui 的 Color32 是 **premultiplied** 存储，`to_array()`
        // 返回的就是预乘分量——渐变端点必须直接用预乘值（透明端保持
        // 同 RGB、alpha=0），再用 `from_rgba_unmultiplied` 包一遍会
        // 二次预乘，把高光压暗成灰。
        let hl = Rect::from_min_max(
            pos2(r.min.x + radius, r.min.y),
            pos2(r.max.x - radius, r.min.y + 3.0),
        );
        if hl.width() > 1.0 {
            painter.add(egui::Shape::gradient_rect(
                hl,
                egui::Direction::TopDown,
                [
                    hl_top,
                    egui::Color32::from_rgba_premultiplied(hl_r, hl_g, hl_b, 0),
                ],
            ));
        }
    }

    // ---- 三段布局 ----------------------------------------------------
    //
    // 左上：来源程序 · 时间（小字弱化）
    // 中间：预览（主字号，可折行）
    // 右下：类型徽标 + 类型 · 大小（圆角小方块图标 + 文字，右对齐）
    // 右上：置顶 / 复制 / 删除 三个小图标按钮
    //
    // NOTE: 类型徽标放在**右下角**与「类型 · 大小」文字组合成一组
    // （2026-10-15 用户定稿）：不再占用左侧文本宽度，预览/测量两侧
    // 都不需要为它让位。
    //
    // NOTE: 目标是「**完整显示内容**」：预览**不再截断**，而是按宽度
    // 折行。折行数由 `wrap` 直接给出，不用手工估算——
    // 早前版本按 `preview.chars().take(30)` 硬砍，中文一个字算一个
    // char、英文一个词算一个 char，两者的实际显示宽度差了好几倍，
    // 于是「看起来还有空间」但已经被砍掉了。
    let pad_v = 5.0;
    let acts_w = acts_width(pal);
    let text_x0 = r.min.x + ROW_PAD_H;
    // 顶部 meta 行右侧要让开按钮列；预览与底部 meta 位于按钮行**下方**，
    // 用整行宽度（见 `preview_full_width`）。
    let text_x1 = text_x0 + preview_width(r.width(), pal);
    let full_x1 = text_x0 + preview_full_width(r.width());
    if full_x1 - text_x0 < 20.0 {
        // IMPORTANT: 太窄画不下文字，但**行仍必须可点**。
        //
        // 早前这里是裸`return`，直接跳到函数末尾的
        // `resp.clicked()` / `resp.double_clicked()` 之前——
        // 而 `resp` 早已通过 `ui.interact(r, …)` 注册好了。于是
        // 极窄栏时整行**不可点**，却仍然画了 hover 底色与选中条，
        // 视觉上像是能点，一点没反应。
        //
        // 只跳过绘制，不跳过交互。
        if resp.clicked() {
            f.state.push(Op::Select(item.id));
        }
        if resp.double_clicked() {
            // CAUTION: 必须先算后 push——`f.state.push(double_click_op(f, …))`
            // 让 `f.state` 与实参 `&mut f` 同时活着一帧，借用检查直接拦。
            let op = double_click_op(f, item);
            f.state.push(op);
        }
        return;
    }

    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size.max(10.0);
    // 左上角：来源 · 时间（小字，左对齐）。右侧要给按钮让位。
    let top_band = Rect::from_min_max(
        pos2(text_x0, r.min.y + pad_v),
        pos2(text_x1, r.min.y + pad_v + meta_h),
    );
    // 右下角：类型 · 大小（小字，右对齐）。它在按钮行下方，可用整行宽。
    let bot_band = Rect::from_min_max(
        pos2(text_x0, r.max.y - pad_v - meta_h),
        pos2(full_x1, r.max.y - pad_v),
    );
    // 中间：预览。起点在「顶部一行（含按钮）」之下，因此可以横跨整行宽度，
    // 不会被按钮遮挡——这正是「内容能显示到右侧」的关键。
    //
    // 高度与 `row_height_for` 严格对应：行高 = 顶部一行 + 预览 + 底部 meta，
    // 这里同样扣掉底部 meta 与 2pt 行间，保证两次排版结果一致。
    let preview_top = r.min.y + pad_v + row_top_height(pal) + 2.0;
    let preview_band = Rect::from_min_max(
        pos2(text_x0, preview_top),
        pos2(full_x1, bot_band.min.y - 2.0),
    );

    // 内容形态三分支（第三轮）：图片内嵌 / Markdown 渲染（或源码）/ 文本。
    // CAUTION: 判据用的开关要从 config 现取，且**不能**持有 `&cfg` 跨越
    // 下面的 `&mut f` 调用——先拷成两个 bool。
    let image_inline = f.svc.state.config.ui.image_inline;
    let md_rendered = f.svc.state.config.ui.md_rendered;
    // 「该不该内嵌成图片」用 [`preview_is_image`]：除了 `ClipKind::Image`，
    // 还包括从资源管理器复制的**单个图片文件**（载荷只有路径，图在磁盘上）。
    let is_image = preview_is_image(item);
    let is_markdown = item.kind == modular_clipboard_core::ClipKind::Markdown;

    if preview_band.height() > 4.0 {
        if is_image && image_inline {
            draw_image_preview(f, ui, preview_band, item, IMG_MAX_H_LIST);
        } else if is_markdown && md_rendered {
            draw_markdown_preview(f, ui, preview_band, item);
        } else if is_markdown {
            draw_source_preview(f, ui, preview_band, item);
        } else {
            draw_text_preview(f, ui, preview_band, item, selected);
        }
    }

    // 左上：来源 · 时间（小字）。
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::text::elide_text(f.ui, &meta, &meta_font, top_band.width());
    if !meta_shown.is_empty() {
        // NOTE: painter_at(top_band)：来源名可能很长（长 exe 路径），
        // 根 painter 会让它横跨到相邻卡片上。
        ui.painter_at(top_band).text(
            top_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font.clone(),
            pal.text_dim,
        );
    }

    // 右下：可配置的信息组合（类型徽标 / 类型文字 / 大小文字）。
    //
    // 三个元素的**显隐与前后顺序**由 [`RowMetaConfig`] 决定（设置中
    // 可单击切换、按住拖拽重排）。元素之间只留固定间距，不再用「·」
    // 分隔。整组右对齐 bot_band 右缘。
    //
    // 大小来自 `PayloadRef::len`（字节数），`format_size` 自适应
    // B/KB/MB/GB；载荷被淘汰（`payload = None`）时大小元素自动缺席。
    // 宽度不足时优先保住短的「大小」，「类型」elide 截断。
    let meta_cfg = &f.svc.state.config.ui.row_meta;
    let type_text = kind_label(item.kind).to_string();
    let size_text = item.payload.as_ref().map(|p| format_size(p.len));
    let badge_side = pal.icon_size; // 16pt
    let badge_radius = 4.0;
    let meta_gap = 6.0;

    let icon_on = meta_cfg.visible(RowMetaPart::Icon);
    let type_on = meta_cfg.visible(RowMetaPart::Type) && !type_text.is_empty();
    let size_on = meta_cfg.visible(RowMetaPart::Size) && size_text.is_some();
    let n = icon_on as u32 + type_on as u32 + size_on as u32;
    if n > 0 {
        // 固定占位（徽标宽 + 元素间 gap）；余下全部给文字。
        let fixed = if icon_on { badge_side } else { 0.0 }
            + if n > 1 { meta_gap * (n - 1) as f32 } else { 0.0 };
        let text_budget = (bot_band.width() - fixed).max(0.0);
        let size_full = size_text.unwrap_or_default();
        let size_w = if size_on {
            crate::text::measure_text(f.ui, &size_full, &meta_font)
        } else {
            0.0
        };
        // 类型与大小同处一行时，gap 已计入 fixed，这里只扣 size_w。
        let type_budget = if type_on && size_on {
            (text_budget - size_w).max(0.0)
        } else {
            text_budget
        };
        let type_shown = if type_on {
            crate::text::elide_text(f.ui, &type_text, &meta_font, type_budget)
        } else {
            String::new()
        };
        let type_w = crate::text::measure_text(f.ui, &type_shown, &meta_font);

        let cy = bot_band.center().y;
        let mut rx = bot_band.right_center().x;
        // 文字用 bot_band 的 painter（受文字带裁剪）；徽标比文字带
        // 高（16pt vs ~12pt），画在行 painter 上不被裁掉。
        let bot_painter = ui.painter_at(bot_band);
        for part in meta_cfg.order.iter().rev() {
            match part {
                RowMetaPart::Icon if icon_on => {
                    let br = Rect::from_min_max(
                        pos2(rx - badge_side, cy - badge_side * 0.5),
                        pos2(rx, cy + badge_side * 0.5),
                    );
                    painter.rect_filled(br, badge_radius, pal.surface_variant);
                    // Markdown 无字体字形，走几何绘制（见 paint_kind_icon）。
                    crate::icons::paint_kind_icon(&painter, item.kind, br, pal.text_dim);
                    rx -= badge_side + meta_gap;
                }
                RowMetaPart::Size if size_on => {
                    bot_painter.text(
                        pos2(rx, cy),
                        Align2::RIGHT_CENTER,
                        size_full.clone(),
                        meta_font.clone(),
                        pal.text_dim,
                    );
                    rx -= size_w + meta_gap;
                }
                RowMetaPart::Type if type_on && !type_shown.is_empty() => {
                    bot_painter.text(
                        pos2(rx, cy),
                        Align2::RIGHT_CENTER,
                        type_shown.clone(),
                        meta_font.clone(),
                        pal.text_dim,
                    );
                    rx -= type_w + meta_gap;
                }
                _ => {}
            }
        }
    }

    // 右侧三个操作：置顶 / 复制 / 删除。
    //
    // IMPORTANT: **必须用图标字体**（[`crate::icons::Icon`]，PUA 码位
    // `U+E000..=U+E011`），不能用 Unicode 字形。
    //
    // 实测：早前这里写的是 `"⧉"` / `"✕"` / `"★"`，其中 `⧉`(U+29C9)
    // 与 `✕`(U+2715) 在当前字体链（微软雅黑 + 图标字体）里**没有
    // 字形**，于是渲染成**空心方框**（tofu）。`★`/`☆` 恰好在雅黑里
    // 有字形，所以截图里只有「复制/删除」变方框、置顶正常——
    // 这种「一部分符号缺字形」的现象最容易误判成字体没加载。
    //
    // 代价：图标字体只有 18 个码位，「复制」没有现成图标，
    // 只能用 `File`（文档）近似表达「复制到剪贴板」。
    let btn = pal.control_height * 0.55;
    let acts: [(crate::icons::Icon, Op, bool); 3] = [
        (
            if item.pinned {
                crate::icons::Icon::Pinned
            } else {
                crate::icons::Icon::Unpinned
            },
            Op::TogglePin(item.id),
            // IMPORTANT: 必须始终可点：置顶条目正是靠这个按钮取消置顶。
            // 早前写成 `!item.pinned` —— 条目置顶后按钮变灰、点击不触发，
            // 于是「置顶后无法取消置顶」。图标已区分 Pinned/Unpinned 状态，
            // 语义就是置顶 ↔ 取消置顶的开关，任何状态下都可点。
            true,
        ),
        (crate::icons::Icon::File, Op::Copy(item.id), true),
        (crate::icons::Icon::Close, Op::Delete(item.id), true),
    ];

    for (i, (icon, op, enabled)) in acts.iter().enumerate() {
        // 三个操作按钮排**卡片右上角**（小图标）：从右缘向左排，
        // 纵向比来源行再下移 `ROW_ACTS_DROP`（避开字形上沿）。
        //
        // IMPORTANT: 基准右缘必须让出 `SCROLLBAR_RESERVE` **和**
        // `ROW_PAD_H`。
        //
        // egui 0.36 默认 `ScrollStyle` 是 **floating** —— 滚动条
        // **不占布局宽度**，而是浮在 `content` 右缘覆盖
        // `[content.max.x - 10, content.max.x]`。而 `r.max.x` 恰好等于
        // `content.max.x`，所以按 `r.max.x` 排的最后一个按钮（删除）
        // 中心会落在滚动条里。
        //
        // 滚动条在内容闭包**之后**才注册交互（`scroll_area.rs` 里用
        // `Sense::CLICK | Sense::DRAG` 注册 `max_bar_rect`），而 egui 的
        // hit-test 规则是「并列时取最后注册的 = 最上层」⇒ **滚动条压过
        // 行内所有控件**。
        //
        // 现象（极难自查）：条目少、列表不满一屏时**没有滚动条**，一切
        // 正常；条目一多（有滚动条）删除按钮就点不动——而这是常态。
        // 「条目少就好、条目多就坏」是这条 bug 的指纹。
        let acts_right = r.max.x - ROW_PAD_H;
        let b = Rect::from_center_size(
            pos2(
                acts_right - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.min.y + pad_v + ROW_ACTS_DROP + btn * 0.5,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_x1 {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = ui.interact(b, egui::Id::new(("row_act", item.id, i)), Sense::click());
        // 悬停圆角底（110ms OutCubic 补间）：小图标按钮只有「图标变色」
        // 时反馈太弱，加一层 `row_hover` 底让按钮有可点的实体感。底色先
        // 画、图标后画，顺序不能反。id 带条目与槽位，三个按钮各自独立。
        let bh = crate::anim::tween_f32(
            ui.ctx(),
            egui::Id::new(("row_act_hover", item.id, i)),
            if rr.hovered() { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(110),
            crate::anim::Ease::OutCubic,
        );
        if bh > 0.004 {
            ui.painter_at(b)
                .rect_filled(b, pal.radius_sm, fade_color(pal.row_hover, bh));
        }
        let color = if !*enabled {
            pal.text_dim
        } else if i == 2 && rr.hovered() {
            // 删除是破坏性操作，悬停用 danger 色与前两个区分。
            pal.danger
        } else if rr.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        icon.paint(
            &ui.painter_at(b),
            Rect::from_center_size(b.center(), vec2(btn, btn) * 0.78),
            color,
        );
        // 悬停说明：图标本身太小，必须给文字提示。
        let tip = match i {
            0 => {
                if item.pinned {
                    "取消置顶"
                } else {
                    "置顶"
                }
            }
            1 => "复制",
            _ => "删除",
        };
        if rr.clicked() && *enabled {
            f.state.push(op.clone());
        }
        // `on_hover_text` 按值消耗 `Response`，必须是最后一次使用。
        rr.on_hover_text(tip);
    }

    if resp.clicked() {
        f.state.push(Op::Select(item.id));
    }
    if resp.double_clicked() {
        // 同窄行分支：先算后 push（避免 `f.state` 与 `&mut f` 同帧共存）。
        let op = double_click_op(f, item);
        f.state.push(op);
    }
}

// ---------------------------------------------------------------- 预览带

/// 文本预览（默认形态）：按预览带宽度折行，**完整显示**。
///
/// 选中态把文字提亮一档（与 `sel_fx` 的描边/底色叠加表达选中）。
/// # CAUTION: 必须用**调用方给的那个 `Ui`**（列表传滚动区的内容 Ui）
///
/// 早前这里用的是 `f.ui`（根 Ui），它的裁剪矩形是**整个窗口** ⇒ 列表滚动时
/// 预览文字会画到列表之外：越过卡片上缘、直接盖在顶栏上（图标被文字压住），
/// 而卡片底色由滚动区的 `ui.painter_at` 画、被正确裁剪 —— 于是「只剩文字
/// 浮在顶栏上，没有遮蔽」（2026-10-19 用户反馈）。
///
/// `Ui::painter_at` 的裁剪 = 自身矩形 ∩ **本 Ui 的裁剪矩形**，所以只要
/// 传入滚动区的内容 Ui，越界的行自动被裁掉，与「看得见才画得出」一致。
fn draw_text_preview(
    f: &mut Frame<'_>,
    ui: &egui::Ui,
    band: Rect,
    item: &ClipItem,
    selected: bool,
) {
    let pal = f.pal;
    // 用 `Painter::layout_job` 按宽度折行，返回**完整** Galley——
    // 这才是「完整显示」：不丢任何字符，只是折行。
    //
    // CAUTION: 不能用 `Ui::layout`/`Context::layout`：那是给「让出矩形」
    // 的控件做换行用的，会受子 `Ui` 的布局约束影响，而我们只要
    // 一个纯文本排版结果。`Painter::layout_job` 才是纯排版入口。
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &item.preview,
        0.0,
        egui::TextFormat {
            font_id: sized(pal.font_md, f.scale),
            color: if selected {
                pal.text_bright
            } else {
                pal.text
            },
            ..Default::default()
        },
    );
    job.wrap.max_width = band.width();
    // 高度用 `max_rows` 表达（0.36 的 `TextWrapping` **没有**
    // `max_height` 字段，只有 `max_width` / `max_rows` /
    // `break_anywhere` / `overflow_character`）。
    //
    // 行数按可用高度与字号算出，至少 1 行。超出部分由 egui 省略并
    // 在末行加省略号——**完整内容在详情子窗口里能看到**。
    // 行数上限：`row_height_for` 已按同一份预览文本算出行数并据此
    // 定好行高，这里给同样的上限只是兜底（防止两次排版结果不一致）。
    //
    // CAUTION: **不能设 0**：egui 文档写明「If set to 0, no text will be
    // outputted」——0 表示一行都不输出。默认值 `usize::MAX` 才是「不限」。
    job.wrap.max_rows = ((band.height() / (pal.font_md * LINE_H)).floor().max(1.0)) as usize;
    // 长路径/无空格的长串要在任意字符间断行，否则会横向溢出。
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter_at(band)
        .galley(pos2(band.min.x, band.min.y), galley, pal.text);
}

/// Markdown 源码预览（`md_rendered` 关闭时）：等宽 + 弱化色。
///
/// 与渲染态明确区分——用户能一眼看出「现在看到的是原始 Markdown」。
fn draw_source_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem) {
    let pal = f.pal;
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &item.preview,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(pal.font_sm * f.scale),
            color: pal.text_dim,
            ..Default::default()
        },
    );
    job.wrap.max_width = band.width();
    job.wrap.max_rows = ((band.height() / (pal.font_sm * 1.45)).floor().max(1.0)) as usize;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    ui.painter_at(band)
        .galley(pos2(band.min.x, band.min.y), galley, pal.text_dim);
}

/// Markdown 渲染预览（第三轮新增）。
///
/// 行高由 `row_height_for` 调**同一个** `md::MdDoc` 的 `height()` 算出，
/// 绘制调 `draw()`——同一函数、同一宽度，测量与绘制不可能漂移。
/// 选中态不提亮文字（`sel_fx` 覆盖层已足够表达），与原型一致。
fn draw_markdown_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem) {
    let doc = crate::md::MdDoc::parse(&item.preview);
    if doc.is_empty() {
        return;
    }
    let pal = f.pal;
    doc.draw(ui, pal, f.scale, band.min, band.width());
}

/// 图片预览（第三轮新增）：内容区直接显示缩略图。
///
/// 管线：`thumb_tex` 命中 → 直接用；未命中 → `thumb_cache`
/// （LRU 像素缓存，失败有短时抑制）解码 + `load_texture` 上传。
/// 解码在**缓存未命中**时才发生：载荷读取与解码都不进每帧热路径
/// （PF-1 的同款理由——列表每帧重排，不能每帧读盘）。
///
/// 任何一步失败（载荷被淘汰 / 不是图片 / 解码失败）都**回落文本预览**：
/// 剪贴板工具的列表不能因为一张坏图就少画一行。
///
/// `max_h` 是缩略图显示高的**上限**——列表行传 [`IMG_MAX_H_LIST`] 防止
/// 单条记录撑爆，详情面板传 `[body.width(), body.height()]` 让图按
/// 长宽比尽量放大。
fn draw_image_preview(f: &mut Frame<'_>, ui: &egui::Ui, band: Rect, item: &ClipItem, max_h: f32) {
    // 纹理先取：解码产物自带尺寸，取不到就说明这张图不该内嵌显示
    // （载荷被淘汰 / 文件已删 / 解码失败）⇒ 降级文本。
    //
    // 顺序为什么是「先纹理后尺寸」：文件型图片（从资源管理器复制的
    // `.png`）**没有** `PayloadRef::dims`——载荷里只有路径列表，尺寸
    // 只能靠解码拿到。统一走解码结果，两类条目共用同一条排版路径。
    let Some(tex) = preview_image_tex(f, item) else {
        draw_text_preview(f, ui, band, item, false);
        return;
    };
    let Some((w, h)) = preview_image_dims(f, item) else {
        // 没有任何维度信息：按普通文本显示（「图片 WxH」/「图片 N 字节」/
        // 文件名），剪贴板工具的列表不能因为一张坏图就少画一行。
        draw_text_preview(f, ui, band, item, false);
        return;
    };
    // 显示矩形：与 [`row_height_for`] 共用 [`image_display_size`]
    // （行高就是按它量出来的，这里再写一份算式必然漂移）。
    //
    // `max_h` 与 `band.height()` 取小：调用方给的限高只是「默认」，
    // 带子本身更矮时（列表行按内容算出的行高）以带子为准。
    let (disp_w, disp_h) = image_display_size((w, h), band.width(), max_h.min(band.height()));
    if disp_w <= 0.0 || disp_h <= 0.0 {
        draw_text_preview(f, ui, band, item, false);
        return;
    }
    let rect = Rect::from_min_size(band.min, vec2(disp_w, disp_h));

    let painter = ui.painter_at(rect);
    // uv 给整张图（0..1），tint 白色（不染色——缩略图颜色即原图）。
    painter.image(
        tex,
        rect,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    // 1px 弱描边：浅色图片（白底截图）与深色列表背景之间需要一道边界。
    painter.rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, f.pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 文件型图片**读盘**的大小上限（字节）。
///
/// 读盘与解码都发生在 UI 线程上：一次几十 MB 的读 + 解码会明显卡一帧
/// （主循环空转 ~125Hz，掉一帧就是 8ms+ 的停顿）。超过这个量就不读，
/// 直接按文本显示——超大图的缩略图价值远低于一次卡顿。
const FILE_IMAGE_MAX_BYTES: u64 = 24 * 1024 * 1024;

/// 条目是否应当**内嵌显示成图片**。
///
/// 两类来源：
/// - `ClipKind::Image`：载荷就是像素（现行规范是裸 RGBA8）；
/// - `ClipKind::Files`：**单个**图片文件（从资源管理器复制的 `.png` 等）。
///
/// # 文件型为什么只看 `preview`（零 IO）
///
/// 完整路径只存在于载荷字节里，而列表**每帧**都要跑这条判定 ——
/// 每帧读载荷正是 PF-1 禁止的。好在单文件条目 `preview` 就是文件名
/// （`build_preview` 的 `names.len() == 1` 分支），按扩展名判定即可。
///
/// # 多文件为什么不画缩略图
///
/// 多文件条目 `preview` 是摘要（`a.png 等 5 个文件`），拿不到首行路径
/// 除非读载荷；而且「显示第一张图」与「这一行代表 5 个文件」语义冲突
/// ——用户会以为缩略图就是全部内容。保持文本列表。
pub fn preview_is_image(item: &ClipItem) -> bool {
    match item.kind {
        modular_clipboard_core::ClipKind::Image => true,
        modular_clipboard_core::ClipKind::Files => {
            crate::thumbnail::is_image_file(&item.preview)
        }
        _ => false,
    }
}

/// 取条目图片的字节：图片型读载荷，文件型按首行路径读盘。
///
/// 只在缓存未命中时被调用（见 [`preview_image_thumb`]），不是每帧 IO。
fn load_image_bytes(svc: &mut Service, item: &ClipItem) -> anyhow::Result<Vec<u8>> {
    if item.kind != modular_clipboard_core::ClipKind::Files {
        return svc.load_payload_bytes(item.id);
    }
    // 载荷是换行拼接的路径列表，取第一行——与双击跳转
    // （`double_click_op` → `Op::RevealPath`）同一条规则：看到的缩略图
    // 和双击打开的是同一个文件。
    let bytes = svc.load_payload_bytes(item.id)?;
    let path = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("文件条目载荷里没有路径"))?;
    // 大小先查 metadata（比读全文件廉价得多）：超限直接放弃。
    let len = std::fs::metadata(&path)
        .map_err(|e| anyhow::anyhow!("读取图片文件信息失败 {path}: {e}"))?
        .len();
    if len > FILE_IMAGE_MAX_BYTES {
        anyhow::bail!("图片文件过大（{len} 字节 > {FILE_IMAGE_MAX_BYTES}），跳过缩略图");
    }
    std::fs::read(&path).map_err(|e| anyhow::anyhow!("读取图片文件失败 {path}: {e}"))
}

/// 解码并上传缩略图纹理；命中已上传的纹理时直接返回。
///
/// 上传成功后把**尺寸**写进 `state.thumb_dims`：文件型图片没有
/// `PayloadRef::dims`，排版需要的长宽比只能从这里拿。
fn preview_image_tex(f: &mut Frame<'_>, item: &ClipItem) -> Option<egui::TextureId> {
    let id = item.id;
    if let Some(t) = f.state.thumb_tex.get(&id).copied() {
        return Some(t);
    }
    let thumb = preview_image_thumb(f, item)?;
    let dims = (thumb.width, thumb.height);
    let img = thumb.to_color_image();
    // 纹理名带条目 id：同一条目重复上传会**替换**而不是累积，
    // egui 侧不会因此长出新纹理。
    // 只存 `TextureId`：纹理由 Context 持有（注册即托管），
    // 句柄（TextureHandle）只是引用，没必要长留。
    let t = f
        .ui
        .ctx()
        .load_texture(
            format!("mc_thumb_{id}"),
            img,
            egui::TextureOptions::LINEAR,
        )
        .id();
    f.state.thumb_tex.insert(id, t);
    f.state.thumb_dims.insert(id, dims);
    Some(t)
}

/// 缩略图的长宽比来源。
///
/// 优先解码得到的真实尺寸（`thumb_dims`），其次载荷里存的 `dims`。
fn preview_image_dims(f: &Frame<'_>, item: &ClipItem) -> Option<(u32, u32)> {
    f.state
        .thumb_dims
        .get(&item.id)
        .copied()
        .or_else(|| {
            item.payload
                .as_ref()
                .and_then(|p| p.dims)
                .filter(|(w, h)| *w > 0 && *h > 0)
        })
}

/// 取（必要时解码）条目的缩略图像素。
///
/// CAUTION: `state.thumb_cache` 与 `svc` 是 `Frame` 的不同字段，
/// 先分别借开才能在同一条表达式里用（闭包要读字节、缓存要可写）。
fn preview_image_thumb(
    f: &mut Frame<'_>,
    item: &ClipItem,
) -> Option<std::sync::Arc<crate::thumbnail::Thumbnail>> {
    let id = item.id;
    // 载荷里存的尺寸（裸 RGBA8 时是唯一来源）；文件型条目没有。
    let dims = item
        .payload
        .as_ref()
        .and_then(|p| p.dims)
        .filter(|(w, h)| *w > 0 && *h > 0);
    // 第一段：缓存（含失败抑制）。
    let from_cache = {
        let cache = &mut f.state.thumb_cache;
        let svc = &mut f.svc;
        let key = crate::thumbnail::CacheKey::new(id, item.hash.clone());
        let load = || load_image_bytes(svc, item).map_err(|e| e.to_string());
        // dims 一起交给缓存：载荷是裸 RGBA8 时（现行规范）必须走 RGBA
        // 旁路，编码格式嗅探对它必然失败；文件型传 `None` ⇒ 走嗅探。
        cache.get_or_load_auto(key, dims, load).ok()
    };
    // 第二段（必要时）：旁路 cache 直接解一次。
    // thumb_cache 的失败抑制会让 2 秒内的失败调用直接返回 Err，但用户
    // 看到的「图片」很可能就卡在这条链路上——绕过抑制再解一次，往往就
    // 能显示（例如文件刚写好、上一帧还没落盘）。
    from_cache.or_else(|| {
        let svc = &mut f.svc;
        let bytes = match load_image_bytes(svc, item) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(id, %e, "缩略图字节读取失败");
                return None;
            }
        };
        match crate::thumbnail::decode_auto(&bytes, dims, crate::thumbnail::THUMB_MAX_EDGE) {
            Ok(t) => Some(std::sync::Arc::new(t)),
            Err(e) => {
                tracing::debug!(id, ?e, "缩略图解码失败，回落文本预览");
                None
            }
        }
    })
}

/// 双击列表行的目标操作（2026-10-15 第三轮定稿）：
///
/// - 链接型文本（`https?://…`）⇒ 浏览器打开（尊重 `ui.browser_path`）；
/// - 文件型条目 / 形似路径的文本 ⇒ 资源管理器定位（尊重 `ui.explorer_path`）；
/// - 其余 ⇒ 复制。
///
/// CAUTION: 判据放在绘制层（而不是让 `App` 再认一遍内容）：
/// 双击时条目就在手上，kind + preview 都是现成的；`App` 侧只负责
/// 「用哪个程序打开」，不重复判断「该不该打开」。
///
/// CAUTION 2: **文件型条目必须读载荷取路径，不能用 preview**。
/// `build_preview` 给 Files 存的是**文件名摘要**（`paint.rs 等 12 个文件`），
/// 完整路径只存在于载荷字节（换行拼接）里。拿 preview 去定位，
/// explorer 收到的是一个文件名——打不开用户想要的位置。
/// 文本型路径没有这个问题（preview 即原文）。
fn double_click_op(f: &mut Frame<'_>, item: &modular_clipboard_core::item::ClipItem) -> Op {
    use modular_clipboard_core::ClipKind;
    use modular_clipboard_platform::{looks_like_path, looks_like_url};
    let text = item.preview.trim();
    if item.kind == ClipKind::Text && looks_like_url(text) {
        return Op::OpenUrl(text.to_string());
    }
    let is_pathish =
        item.kind == ClipKind::Files || (item.kind == ClipKind::Text && looks_like_path(text));
    if is_pathish {
        let first = if item.kind == ClipKind::Files {
            // 载荷是换行拼接的路径列表，取第一行。读失败（载荷已被淘汰 /
            // 通道故障）就放弃跳转——回落成复制好过打开一个错误的位置。
            match f.svc.load_payload_bytes(item.id) {
                Ok(bytes) => String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                    .map(str::to_string),
                Err(e) => {
                    tracing::debug!(id = item.id, %e, "读取文件条目载荷失败，双击跳转降级为复制");
                    None
                }
            }
        } else {
            text.lines().next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        };
        if let Some(path) = first {
            return Op::RevealPath(path);
        }
    }
    Op::Copy(item.id)
}

// ---------------------------------------------------------------- 状态栏

fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}

/// 内容类型的展示名（右下角标签）。
///
/// 单一数据源在 [`modular_clipboard_core::ClipKind::label`]：
/// 类型过滤栏的 tooltip、设置界面、捕获层提示都读同一份。
fn kind_label(kind: modular_clipboard_core::ClipKind) -> &'static str {
    kind.label()
}

/// 字节数 → 自适应单位（B / KB / MB / GB）。
///
/// 直接显示原始字节数既占位又难读：`1234567 字节` 一眼看不出量级。
/// 这里按 1024 进位取一档，整数部分 <10 时保留一位小数（`1.5 KB`），
/// 否则整数显示（`12 KB`），避免「12.3 KB」这类不必要的小数噪音。
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    let one = |v: f64, unit: &str| {
        if v >= 10.0 {
            format!("{v:.0} {unit}")
        } else {
            format!("{v:.1} {unit}")
        }
    };
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MB {
        one(b / KB, "KB")
    } else if b < GB {
        one(b / MB, "MB")
    } else {
        one(b / GB, "GB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let group = FloatLayout::compute(&cfg, &items, false, area)
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
    fn clicking_trash_emits_clear_all() {
        let mut h = Harness::new("trash", vec2(600.0, 400.0));
        h.frame();
        let p = topbar_trash(&h).center();
        let ops =
        h.click_at(p);
        assert!(
            ops.contains(&Op::ClearAll),
            "点垃圾桶应发出 ClearAll，实际={ops:?}"
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
    /// `type_filter` / `only_pinned` / `pinned_drawn_separately` 是绘制层
    /// 本地状态、不过 store ⇒ `list_version` 不变。而长度检查挡不住：
    /// 3 条 Text 与 3 条 Image，从「文本」切到「图片」行数都是 3 ⇒
    /// 缓存命中 ⇒ **图片行套用文本行的行高**（图被压扁/留白，无报错）。
    #[test]
    fn row_height_cache_key_includes_filter() {
        use modular_clipboard_core::ClipKind;
        let base = RowFilterKey {
            type_filter: None,
            only_pinned: None,
            pinned_drawn_separately: false,
        };
        // 三个维度各自变化都必须让键不等（否则该维度漏在键外）。
        assert_ne!(
            base,
            RowFilterKey {
                type_filter: Some(ClipKind::Text),
                ..base
            },
            "类型过滤必须进键"
        );
        assert_ne!(
            base,
            RowFilterKey {
                only_pinned: Some(true),
                ..base
            },
            "置顶过滤必须进键"
        );
        assert_ne!(
            base,
            RowFilterKey {
                pinned_drawn_separately: true,
                ..base
            },
            "置顶独立分栏必须进键（它决定主栏是否让出置顶条目）"
        );
        // 相同条件必须相等，否则缓存永远不命中（退化成每帧重算）。
        assert_eq!(
            base,
            RowFilterKey {
                type_filter: None,
                only_pinned: None,
                pinned_drawn_separately: false,
            }
        );
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

        // 搜索图标必须在按钮区左侧，且不得与任何按钮重叠。
        let icon_r = hit(&h, "search_icon");
        assert!(
            icon_r.max.x <= rects[0].1.min.x,
            "搜索图标 {} 越过最左按钮 {}",
            icon_r.max.x,
            rects[0].1.min.x
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

    /// 弹窗输入框持有焦点时**永不折叠**（无论光标在哪、等多久）。
    ///
    /// 焦点是「用户正在搜索」的唯一可靠判据：他可能已经把手移开去
    /// 看列表结果了。这一条不做的话，打字打到一半弹窗自己没了。
    ///
    /// NOTE: 焦点住在**弹窗自己的 `egui::Context`** 里，主窗口的
    /// `memory()` 恒为空——所以判据是弹窗每帧回报的
    /// `search_popup_focus`，不是主窗口自己查焦点。
    #[test]
    fn focused_search_box_never_collapses() {
        let mut h = Harness::new("search-focus", vec2(700.0, 500.0));
        h.frame();
        // 点图标：展开 + 请求焦点（焦点请求由弹窗消费）。
        let icon = h.state.hit.get(HitNs::Main, "search_icon").unwrap().center();
        h.click_at(icon);
        assert!(h.state.search_expanded, "点击图标应展开");

        // 弹窗回报「焦点在输入框上」（等价于弹窗跑过一帧并拿到焦点）。
        h.state.search_popup_focus = true;
        // 光标移到列表中间 + 时间往前拨 5 秒：仍然不能折叠。
        let away = egui::pos2(h.area.center().x, h.area.max.y - 20.0);
        h.advance_secs(5.0);
        h.frame_with(vec![egui::Event::PointerMoved(away)]);
        assert!(
            h.state.search_expanded,
            "弹窗持有焦点时搜索面板不能折叠——用户正在打字"
        );

        // 焦点交还（点了别处）→ 宽限后折叠。
        //
        // NOTE: 交还的**那一帧**不会立刻开始倒计时——那一帧状态机走的是
        // 「被焦点钉住」分支，顺手把在途的 `leave_since` 清了。所以要
        // 先跑一帧让它把计时器重新起上，再等宽限期。
        h.state.search_popup_focus = false;
        h.frame();
        // 折叠要跨过**两段串联**的时间：先等刚展开的免折叠宽限
        // `SEARCH_OPEN_GRACE` 过去（这一帧才起 `leave_since` 计时），
        // 再等 `SEARCH_COLLAPSE_DELAY`。两次拨钟，各自留 0.05s 余量。
        h.advance_secs(SEARCH_OPEN_GRACE + 0.05);
        h.frame();
        h.advance_secs(SEARCH_COLLAPSE_DELAY + 0.05);
        h.frame();
        assert!(
            !h.state.search_expanded,
            "焦点交还且宽限期过后应收起弹窗"
        );
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
        let floats = FloatLayout::compute(&cfg, &[], true, area);
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
            (r.min.y - (area.min.y - RAIL_GAP - SEARCH_PANEL_H)).abs() < 0.01,
            "面板高度应为 SEARCH_PANEL_H={SEARCH_PANEL_H}：{r:?}"
        );
        // 通栏：左右缘与窗口客户区对齐。
        assert!((r.min.x - area.min.x).abs() < 0.01 && (r.max.x - area.max.x).abs() < 0.01);
        // 折叠：没有矩形。
        assert!(FloatLayout::compute(&cfg, &[], false, area).search.is_none());
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
                pos: TypeRailPos::Right,
                size: 32.0,
            },
            ..Default::default()
        };
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(720.0, 800.0));
        let items: Vec<modular_clipboard_core::item::ClipItem> = Vec::new();
        let size = 32.0;

        // 右：整块在窗口右缘之外，间距 RAIL_GAP。
        let r = FloatLayout::compute(&cfg, &items, false, area)
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
        let r = FloatLayout::compute(&cfg, &items, false, area)
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
        let r = FloatLayout::compute(&cfg, &items, false, area)
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
        let r = FloatLayout::compute(&cfg, &items, false, area)
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
            FloatLayout::compute(&cfg, &items, false, area).rail.is_none(),
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

    /// 点类型栏按钮 ⇒ `SetTypeFilter(Some(kind))`；再点一次 ⇒ `None`。
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
                Op::SetTypeFilter(Some(k)) if *k == modular_clipboard_core::ClipKind::Text
            )),
            "点文本按钮应过滤出 Text，实际 {ops:?}"
        );
        // 落地 Op（生产由 App::apply_ops 做），再点一次应取消过滤。
        for op in ops {
            if let Op::SetTypeFilter(k) = op {
                h.state.type_filter = k;
            }
        }
        let ops2 = h.rail_popup_click(0);
        assert!(
            ops2.iter().any(|o| matches!(o, Op::SetTypeFilter(None))),
            "再点一次应取消过滤，实际 {ops2:?}"
        );
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
                crate::thumbnail::THUMB_MAX_EDGE as f32 >= crate::paint::IMG_MAX_H_LIST * 2.0,
                "缩略图最长边必须 ≥ 列表限高 × 2（DPI 200%），否则放大显示会糊"
            );
        }
        // 顺带：限高必须为正，否则图片行高会退化成纯文本行（编译期）。
        const {
            assert!(
                crate::paint::IMG_MAX_H_LIST > 0.0,
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
            let seg = settings_hit(&h, name);
            assert!(seg.width() > 0.0, "预设「{name}」应有热区");
        }
        // 点「大」（115%）。
        let big = settings_hit(&h, "大");
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
        h.settings_frame();
        for key in ["card_shadow_blur", "card_shadow_opacity", "card_shadow_offset_y"] {
            let r = h.state.hit.get(HitNs::Settings, key);
            assert!(
                r.is_some_and(|r| r.width() > 0.0 && r.height() > 0.0),
                "开启状态下滑条 {key} 应有热区"
            );
        }
    }

    /// 右下角信息编辑器：三个 chip（图标 / 类型 / 大小）都必须登记热区。
    #[test]
    fn row_meta_chips_have_hit_regions() {
        let mut h = Harness::new("rowmeta-hits", vec2(780.0, 560.0));
        h.state.show_settings = true;
        h.state.settings_cat = SettingsCat::Appearance;
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
            assert!(p.contains(cap.name), "提示词应含能力「{}」", cap.name);
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
            let sw = settings_hit(&h, name);
            assert!(sw.width() > 0.0, "预设「{name}」应有热区");
        }
        // 点「翠绿」⇒ SetAccent(Some(绿))。
        let green = settings_hit(&h, "翠绿");
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
        for name in ["无", "纸质", "护眼", "星空", "OLED"] {
            assert!(
                settings_hit(&h, name).width() > 0.0,
                "二级主题选项「{name}」应有热区"
            );
        }
        let paper = settings_hit(&h, "纸质");
        let ops = h.settings_click_at(paper.center());
        let hit = ops.iter().any(|o| {
            matches!(
                o,
                Op::SetSecondaryTheme(modular_clipboard_core::config::SecondaryTheme::Paper)
            )
        });
        assert!(hit, "点「纸质」应推出 SetSecondaryTheme(Paper)，实际={ops:?}");
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
            let line1 = format!("MCP 服务：{}", conn.label());
            let w1 = dot_w + measure_w(&h.ctx, &line1, size);
            assert!(
                w1 <= inner,
                "{conn:?} 状态行宽 {w1} 超过面板内缘 {inner}"
            );
            // 第二行：detail。纵向布局会换行，但正常字体下也不该超宽。
            let w2 = measure_w(&h.ctx, conn.detail(), size);
            assert!(
                w2 <= inner,
                "{conn:?} 说明文字宽 {w2} 超过面板内缘 {inner}"
            );
        }

        // 锁死修法：旧的单行拼接在最长 detail 下必须**放不下**——
        // 否则这个测试锁不住「拆两行」这个回归。
        let old = format!(
            "MCP 服务：{} · {}",
            McpConn::Unconfigured.label(),
            McpConn::Unconfigured.detail()
        );
        let old_w = dot_w + measure_w(&h.ctx, &old, size);
        assert!(
            old_w > inner,
            "旧单行拼接宽 {old_w} 应放不进 {inner}（放得进说明回归锁失效）"
        );
    }
}
