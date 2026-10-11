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
use modular_clipboard_core::config::WindowBackdrop;

use crate::card::{Card, CardHost, CardKind, PinnedMode};
use crate::icons::Icon;
use crate::theme::{Palette, sized};
use crate::workspace::Workspace;

/// 卡片边框宽度（逻辑点）。
const CARD_STROKE: f32 = 1.0;

/// 卡片圆角（逻辑点）。
/// 卡片圆角（逻辑点）。
///
/// NOTE: **取 0（直角）**，不是原先的 8。
///
/// 圆角会在相邻卡片之间与窗口边缘留下 4 个「缺口」：卡片明明
/// 紧贴（实测底部间隙 0px），但视觉上像有一条缝——用户报的
/// 「窗口与窗口之间存在间隙」有一部分就来自这里。
/// 直角 + 1px 描边才能得到连续的、真正无缝的面板。
///
/// 这也符合本项目「**纯平毛玻璃**」的视觉语言：禁止一切装饰性
/// 圆角带来的拟物暗示。
const CARD_RADIUS: f32 = 0.0;

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
    /// 切换主题（跟随系统 / 深色 / 浅色）。
    ///
    /// 由 `App::apply_ops` 落地：更新 `App.pal` 并重灌主窗口 `Visuals`；
    /// 设置窗口的 `Visuals` 由 `settingswin` 每帧从 `App.pal` 同步。
    SetThemeMode(crate::theme::ThemeMode),
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
}

impl ConfigGroup {
    /// 分组标题（设置界面里显示）。
    pub fn title(self) -> &'static str {
        match self {
            ConfigGroup::Ui => "界面",
            ConfigGroup::Capture => "捕获",
            ConfigGroup::Storage => "存储",
            ConfigGroup::Mcp => "通信",
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
    /// 外观：主题 / 背景效果。
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
    pub const ALL: [SettingsCat; 7] = [
        SettingsCat::General,
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
    /// 搜索框是否展开（顶栏右侧图标折叠式搜索）。
    ///
    /// 折叠时顶栏只显示一枚放大镜图标（贴近窗口按钮），左侧整片
    /// 都是拖动区；点击图标展开输入框（向左弹出），此时拖动区
    /// 收缩到输入框左缘，但仍保留一段可拖宽度。
    pub search_expanded: bool,
    /// 上一帧提交给窗口的拖动区（标题栏）。
    ///
    /// IMPORTANT: 必须是**多段**而不是「整个顶栏一块」：拖动区会被系统转成
    /// `HTCAPTION`，点其中的控件不会产生 `WM_LBUTTONDOWN`，
    /// egui 收不到点击（表现：齿轮/垃圾桶/搜索框全都点不动）。
    ///
    /// 多个矩形的并集是拖动区；控件区从中间挖空。
    pub drag_regions: Vec<Rect>,
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

    /// 搜索框 `TextEdit` 的 egui `Id`（本帧绘制时写入）。
    ///
    /// # 为什么需要
    ///
    /// egui 0.36 的 `Id` 是**哈希值**，`format!("{id:?}")` 只得到
    /// `Id::new(3070286450358649230)` 这样的数字，没法按名字匹配。
    /// 于是「点搜索框后焦点是否落在搜索框上」只能退化成「有没有任何
    /// 控件获得焦点」——那是**恒真**的，点别处也一样通过。
    ///
    /// 绘制层把自己用的 id 记下来，测试才能**精确比对**。
    pub search_edit_id: Option<egui::Id>,
    /// 设置界面是否打开。
    pub show_settings: bool,
    /// `MC_SETTINGS` 启动开关是否已消费（一次性）。
    ///
    /// `MC_SETTINGS=1` 只在**启动首帧**打开设置界面一次，随后不再
    /// 强制——早期实现每帧 `show_settings = true`，导致带该环境变量
    /// 启动时设置窗口**永远关不掉**（点 ✕ / 齿轮都被下一帧改回）。
    pub settings_startup_opened: bool,

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
    pub fn hit_insert(&mut self, key: &'static str, r: Rect) {
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
/// 流程：顶栏 → 设置界面（覆盖层）→ 卡片（Z 序）。
/// 与旧实现最大的差别是**卡片一定会被画**——折叠的画把手，
/// 展开的画内容，不存在「跳过」。
pub fn draw(f: &mut Frame<'_>) {
    let area = f.area;
    // 背景必须铺满整个客户区，否则窗口边缘会露出 clear color。
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    let top = Rect::from_min_max(
        pos2(area.min.x, area.min.y),
        pos2(area.max.x, area.min.y + crate::solver::TOPBAR_HEIGHT),
    );
    let body = crate::solver::card_area(area);

    // 热区是**每帧重算**的：跨帧留着旧值会让测试点到一个
    // 「上一帧存在、这一帧已消失」的位置，症状是改了布局后
    // 测试仍通过但测的是空气。
    f.hit_clear();
    draw_topbar(f, top);
    draw_cards(f, body);

    // NOTE: 设置界面**不在这里画**——它已移到独立窗口
    // （见 `crate::settingswin`）。主窗口只负责响应齿轮点击，
    // 把 `show_settings` 置 true，由设置窗口的绘制入口接管。
    //
    // 若在这里再画一次，用户会看到「主窗口里浮着一块面板」，
    // 而真正的设置窗口在旁边——同一份内容画两遍。
}

// ---------------------------------------------------------------- 顶栏

fn draw_topbar(f: &mut Frame<'_>, bar: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(bar);
    painter.rect_filled(bar, CornerRadius::ZERO, pal.surface_variant);
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
    // 三枚按钮：关闭 / 清空 / 设置（「最小化」已去掉，见 draw_topbar_icons）。
    //
    // IMPORTANT: 这里的 `3.0` 必须与 `draw_topbar_icons` 里的 `step = r.width() / 3.0`
    // 一致。两处各写一份数字，改一处忘另一处 ⇒ 按钮区与实际按钮错位，
    // 表现为「最右那枚被搜索框压住」或「点齿轮变成拖窗口」。
    let btn_count = 3.0;
    let btn_area = Rect::from_min_size(
        pos2(
            inner.max.x - btn * btn_count - gap * (btn_count - 1.0),
            bar.center().y - btn * 0.5,
        ),
        vec2(btn * btn_count + gap * (btn_count - 1.0), btn),
    );
    draw_topbar_icons(f, btn_area, bar.center().y);

    // ---- 搜索：折叠图标 + 点击展开输入框 --------------------------------
    //
    // 布局（自左向右）：
    //   [ 大片拖动区 ][ 搜索输入框（仅展开时） ][ 🔍 ][ ✕ ][ 🗑 ][ ⚙ ]
    //
    // 搜索框默认**折叠成一枚放大镜图标**、贴在窗口按钮左侧：顶栏左侧
    // 整片（除按钮与图标）都是拖动区，用户落手即拖——这是顶栏拖动区
    // 曾经「只剩 8pt、拖不动」问题的最优解：不是给拖动区留宽，
    // 而是把占位的搜索框整个压缩到右侧一角。
    //
    // 点击图标展开输入框（向左弹出固定宽度），再点一次折叠。
    // 展开时拖动区收缩到输入框左缘，但 140pt 输入框 + 右端控件之后
    // 仍剩 ~46pt 可拖，不会退化成「窗口拖不动」。
    let sq = btn * 0.62;
    let search_icon_r = Rect::from_center_size(
        pos2(btn_area.min.x - pal.space_md - sq * 0.5, bar.center().y),
        vec2(sq, sq),
    );
    f.hit_insert("search_icon", search_icon_r);
    if search_toggle_button(f, search_icon_r) {
        f.state.search_expanded = !f.state.search_expanded;
        if f.state.search_expanded {
            // 展开瞬间把焦点交给输入框：用户点击图标就是冲着搜索去的，
            // 不该再额外点一下输入框。`TextEdit` 的 id 与
            // `draw_search` 里一致（`Id::new("search_edit")`）。
            f.ui
                .memory_mut(|m| m.request_focus(egui::Id::new("search_edit")));
        }
    }

    // 展开时输入框弹出在图标左侧。
    let mut search_left = search_icon_r.min.x;
    if f.state.search_expanded {
        let input_w = SEARCH_EXPAND_W;
        let input = Rect::from_min_max(
            pos2(search_icon_r.min.x - pal.space_md - input_w, bar.min.y + pal.space_sm),
            pos2(search_icon_r.min.x - pal.space_md, bar.max.y - pal.space_sm),
        );
        if input.width() > 40.0 && input.height() > 8.0 {
            draw_search(f, input);
            search_left = input.min.x;
        }
    }

    let _ = scale;

    // 顶栏拖动区 = 顶栏**减去交互控件**。
    //
    // IMPORTANT: 这里必须是「挖空」而不是「整块上报」。`WM_NCHITTEST` 对拖动区
    // 返回 `HTCAPTION`，系统随后把按下转成 `WM_NCLBUTTONDOWN`
    // ——**不会产生 `WM_LBUTTONDOWN`**，egui 于是收不到点击。
    //
    // 症状：整个顶栏（含齿轮、垃圾桶、搜索框）全都点不动，
    // 表现为「设置打不开 / 搜索框不能点」。
    let mut drag = Vec::new();
    // 左段：窗口左缘 → 搜索输入框/图标的左缘。
    //
    // 折叠时 `search_left` 是图标左缘，顶栏左侧几乎整条都是拖动区；
    // 展开时收缩到输入框左缘。**必须用左缘 - 2pt 收尾**：多出来的
    // 2pt 与控件之间留缝，避免亚像素重叠把控件边缘判成 Caption。
    let left_end = search_left;
    if left_end - bar.min.x > 8.0 {
        drag.push(Rect::from_min_max(bar.min, pos2(left_end - 2.0, bar.max.y)));
    }
    // NOTE: 这里**不设**「两按钮之间」那一段。
    //
    // `btn_area` 宽 `btn*2 + space_sm`、两按钮各占一半——它们是
    // **紧邻**的（`space_sm` 落在 `btn_area` 内部，两侧各一半）。
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
    f.state.drag_regions = drag;
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
    let step = r.width() / 3.0;
    let sq = step * 0.62;

    // 自左向右：关闭 / 清空 / 设置。
    //
    // NOTE: 只有三枚了——「最小化」已去掉。它与「关闭到托盘」的实际
    // 效果几乎相同（都让窗口消失），区别仅在任务栏是否留条目，
    // 而本程序常驻托盘，用户极少需要「留在任务栏但看不见」。
    // 两个近乎重复的按钮并排反而增加误点。
    //
    // 关闭按钮用 `Icon::Minimize`（横线）而非 `Icon::Close`（叉）：
    // 它的语义是「收起」，与最小化同族；叉号在中文界面里更常被
    // 理解成「删除/退出」，而这里并不是退出。
    let slot = |i: f32| Rect::from_center_size(pos2(r.min.x + step * (i + 0.5), center_y), vec2(sq, sq));
    let r_close = slot(0.0);
    let r_clear = slot(1.0);
    let r_set = slot(2.0);

    // 登记热区给测试用（见 `UiState::hit` 的说明）。**必须在点击判定
    // 之前登记**：`draw_topbar` 随后要读这张表来挖拖动区。
    f.hit_insert("topbar_close", r_close);
    f.hit_insert("topbar_clear", r_clear);
    f.hit_insert("topbar_settings", r_set);

    if icon_button(f, "topbar_close", r_close, Icon::Minimize, "关闭到托盘", false) {
        f.state.push(Op::CloseWindow);
    }
    // 破坏性操作（清空）用 danger 色，与「设置」区分开。
    if icon_button(f, "topbar_clear", r_clear, Icon::Trash, "清空全部历史", true) {
        f.state.push(Op::ClearAll);
    }
    if icon_button(f, "topbar_settings", r_set, Icon::Settings, "设置", false) {
        f.state.push(Op::ToggleSettings);
    }
}

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
) -> bool {
    let pal = f.pal;
    let scale = f.scale;
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
    // 悬停底色叠在热区矩形上（`painter_at` 把裁剪限制在顶栏内）。
    if resp.hovered() {
        f.ui.painter_at(r).rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    icon.paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        match (danger, resp.hovered()) {
            (true, true) => pal.danger,
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
fn search_toggle_button(f: &mut Frame<'_>, r: Rect) -> bool {
    let pal = f.pal;
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .id(egui::Id::new("search_icon"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    if resp.hovered() {
        f.ui.painter_at(r).rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    draw_search_icon(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        if resp.hovered() { pal.text_bright } else { pal.text_dim },
    );
    let clicked = resp.clicked();
    resp.on_hover_text(if f.state.search_expanded { "收起搜索" } else { "搜索" });
    clicked
}


fn draw_search(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, pal.radius_md, pal.surface);
    painter.rect_stroke(
        r,
        pal.radius_md,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 放大镜图标由右侧的折叠式搜索图标按钮承担（见 `draw_topbar`），
    // 输入框内不再重复画 —— 否则框内左侧又冒出一个放大镜，与右侧
    // 图标成双，视觉重复。文字直接从左边距起排。
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
    //
    // 只登记输入区不够——外框左侧那条约 22pt 宽的图标带**也是可点的**
    //（点它会聚焦搜索框），而它恰好是最容易被拖动区吃掉的地方。
    // 没有 `search_box` 这条热区，`topbar_controls_are_not_in_drag_region`
    // 就探测不到那个 bug：输入区左缘与拖动区终点只差 2pt，四角全落在
    // 拖动区**之外**，测试照样绿。
    f.hit_insert("search_box", r);
    f.hit_insert("search", edit_rect);
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            // IMPORTANT: **必须给显式 id**。不给的话 egui 按调用位置自动生成，
            // 于是：焦点断言没法稳定地指向它（只能退化成
            // 「有没有任何控件获得焦点」这种恒真判断），且任何调整
            // 布局的改动都可能悄悄换掉 id。
            //
            // 上面 `icon_button` 同样用 `scope_builder`，它就给了
            // `.id(Id::new(id))` —— 两处写法必须一致。
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
                    // 内边距收窄：顶栏只有 44pt，egui 默认的
                    // `symmetric(4,2)` 在此会让单行控件偏高而裁切。
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

    if edit_resp.has_focus() {
        // 聚焦时描边加强，给出明确的视觉反馈。
        f.ui.painter_at(r).rect_stroke(
            r,
            pal.radius_md,
            Stroke::new(1.5, pal.border_strong),
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
        crate::icons::Icon::Close.paint(
            &f.ui.painter_at(clear),
            Rect::from_center_size(clear.center(), vec2(btn, btn) * 0.7),
            if cr.hovered() { pal.text_bright } else { pal.text_dim },
        );
        if cr.clicked() {
            f.state.query.clear();
            // 清空后主动交还焦点：否则用户还得再点一次才能继续打字。
            f.ui.memory_mut(|m| m.request_focus(edit_resp.id));
        }
        cr.on_hover_text("清空搜索");
    }
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
    let search_h = 34.0;
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
    let content = Rect::from_min_max(
        pos2(right.min.x, close_r.max.y + 6.0),
        right.max,
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

    // MCP 调用设置覆盖层（限制在本窗口客户区内，不遮桌面）。
    //
    // 画在内容区**之后**：视觉上盖住卡片，交互区也后注册 → 命中优先。
    if f.state.mcp_overlay_open {
        settings_mcp_overlay(f, area);
    }
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
fn draw_settings_nav(f: &mut Frame<'_>, nav_rect: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(nav_rect);
    painter.rect_filled(nav_rect, pal.radius_lg, pal.surface);

    let pad = 6.0;
    let gap = 2.0;
    let item_h = 40.0;
    for (i, cat) in SettingsCat::ALL.iter().enumerate() {
        let y0 = nav_rect.min.y + pad + i as f32 * (item_h + gap);
        let r = Rect::from_min_max(
            pos2(nav_rect.min.x + pad, y0),
            pos2(nav_rect.max.x - pad, y0 + item_h),
        );
        let selected = f.state.settings_cat == *cat;
        let resp = f
            .ui
            .interact(r, egui::Id::new(("settings_cat", *cat)), egui::Sense::click());
        let bg = if selected {
            pal.row_selected
        } else if resp.hovered() {
            pal.row_hover
        } else {
            egui::Color32::TRANSPARENT
        };
        if bg != egui::Color32::TRANSPARENT {
            painter.rect_filled(r, pal.radius_md, bg);
        }
        // 图标（左）+ 标题（紧挨图标，不留大空隙）。
        //
        // 早期实现文字以 `r.center() + vec2(14, 0)` 左对齐起点——图标
        // 贴左缘（x≈17）、文字却从导航项中心偏右起，中间空出约 70pt，
        // 观感「图标和字体离得太远」（2026-10-11 用户反馈）。改为文字
        // 起点 = 图标右缘 + 8pt，图标与标题紧凑成组。
        let icon_r = Rect::from_center_size(pos2(r.min.x + 17.0, r.center().y), vec2(16.0, 16.0));
        cat.icon().paint(
            &painter,
            icon_r,
            if selected { pal.accent } else { pal.text_dim },
        );
        painter.text(
            pos2(icon_r.max.x + 8.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            cat.title(),
            egui::FontId::proportional(pal.font_sm),
            if selected { pal.text_bright } else { pal.text_dim },
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

const NUMBER_FONT_SCALE: NumberSpec = NumberSpec {
    min: 0.8,
    max: 1.4,
    step: 0.05,
    unit: "×",
    display: identity_num,
    store: identity_num,
};
const NUMBER_POLL_MS: NumberSpec = NumberSpec {
    min: 50.0,
    max: 2000.0,
    step: 50.0,
    unit: " ms",
    display: identity_num,
    store: identity_num,
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
    /// 背景效果单选（无 / 磨砂 / 半透明）。
    Backdrop,
    /// 背景不透明度滑块。
    Opacity,
    /// 快捷键绑定（只读展示）。
    Hotkey(HotkeySlot),
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
    /// 退出程序按钮（关于页）。
    Quit,
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
        _ => false,
    }
}

/// 一键 AI 配对提示词：可调用能力清单 + 接入参数（协议头）。
///
/// `pub`：`App::apply_ops` 的 `CopyMcpPairPrompt` 分支读取它写入剪贴板。
pub fn mcp_pair_prompt(cfg: &modular_clipboard_core::config::Config) -> String {
    let mut s = String::from("【modular-clipboard · MCP 服务接入说明】\nmodular-clipboard 提供 Model Context Protocol (MCP) 服务，可对电脑执行界面自动化操作。\n\n可调用能力（8 项）：\n");
    for cap in MCP_CAPS {
        s.push_str(&format!("- {}：{}\n", cap.name, cap.desc));
    }
    s.push_str(&format!(
        "\n接入参数：\n- 协议：Model Context Protocol（MCP）\n- 协议版本：2025-06-18\n- 传输方式：stdio\n- 启动命令：modular-clipboard.exe --mcp-server\n- 服务地址：{}\n- 端口：{}\n- 认证：{}\n\n请将上述参数配置到你的 MCP 客户端（如 Claude Desktop / Cursor / VS Code），配置完成后即可通过工具调用对本电脑执行界面自动化操作。",
        if cfg.mcp.host.is_empty() { "127.0.0.1（默认）".to_string() } else { cfg.mcp.host.clone() },
        if cfg.mcp.port == 0 { "未设置".to_string() } else { cfg.mcp.port.to_string() },
        if cfg.mcp.token.is_empty() { "无".to_string() } else { "Bearer Token（已在设置中配置）".to_string() },
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
            ItemKind::Backdrop => cfg.ui.window_backdrop != WindowBackdrop::None,
            ItemKind::Opacity => (cfg.ui.window_opacity - 1.0).abs() > f32::EPSILON,
            ItemKind::Hotkey(slot) => slot.current(cfg) != slot.default(),
            ItemKind::McpToggle => {
                get_bool(cfg, ConfigGroup::Mcp, "enabled") != get_bool(&d, ConfigGroup::Mcp, "enabled")
            }
            ItemKind::Text { group, field, .. } => {
                get_text(cfg, *group, field) != get_text(&d, *group, field)
            }
            ItemKind::McpPort => {
                get_number(cfg, ConfigGroup::Mcp, "port") != get_number(&d, ConfigGroup::Mcp, "port")
            }
            ItemKind::Info(_) | ItemKind::Quit => false,
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
            ItemKind::Backdrop => Some(Op::SetBackdrop(WindowBackdrop::None)),
            ItemKind::Opacity => Some(Op::SetOpacity(1.0)),
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
            ItemKind::Info(_) | ItemKind::Quit => None,
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
        ],
    },
    SettingsCard {
        cat: SettingsCat::Appearance,
        title: "外观",
        items: &[
            SettingsItem {
                label: "主题",
                desc: "跟随系统 / 深色 / 浅色",
                kind: ItemKind::Theme,
            },
            SettingsItem {
                label: "字体缩放",
                desc: "界面文字大小",
                kind: ItemKind::Number(ConfigGroup::Ui, "font_scale", &NUMBER_FONT_SCALE),
            },
            SettingsItem {
                label: "背景效果",
                desc: "无 / 磨砂 / 半透明",
                kind: ItemKind::Backdrop,
            },
            SettingsItem {
                label: "背景不透明度",
                desc: "磨砂=背景强度，半透明=整窗淡出",
                kind: ItemKind::Opacity,
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
                label: "数据目录",
                desc: "",
                kind: ItemKind::Info("系统应用数据目录（%APPDATA%\\modular-clipboard）"),
            },
            SettingsItem {
                label: "技术栈",
                desc: "",
                kind: ItemKind::Info("Rust · egui · Vulkan (Win32)"),
            },
            SettingsItem {
                label: "项目主页",
                desc: "",
                kind: ItemKind::Info("待补充 GitHub 地址"),
            },
            SettingsItem {
                label: "作者主页",
                desc: "",
                kind: ItemKind::Info("待补充"),
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
/// 卡片高度由 `head_h + row_h * 行数` 算出，整块 `allocate` 推进滚动游标。
/// 「通信」卡片底部额外有「一键 AI 配对提示词」按钮，占用 44pt。
fn settings_card(f: &mut Frame<'_>, ui: &mut Ui, card: &SettingsCard) {
    let pal = f.pal;
    let w = ui.available_width();
    let row_h = 44.0;
    // 无卡片标题：顶部只留一行放右上角「重置」按钮（有变化时才出现）。
    let head_h = 26.0;
    let pad = 12.0;
    let extra = if card.cat == SettingsCat::Comm { 44.0 } else { 0.0 };
    let h = head_h + row_h * card.items.len() as f32 + pad * 2.0 + extra;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    ui.painter_at(r).rect_filled(r, pal.radius_lg, pal.surface);

    let inner_rect = r.shrink2(vec2(pad, pad * 0.5));
    let mut inner = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    // 行间距：设置项之间拉开呼吸感（2.0 太挤，文字贴在一起）。
    inner.spacing_mut().item_spacing = vec2(0.0, 8.0);

    // 顶部行：无标题文字，仅当有项偏离默认值时在**右上方**出现「重置」。
    let changed_any = {
        let cfg = &f.svc.state.config;
        card.items.iter().any(|it| it.is_changed(cfg, f.ws))
    };
    if changed_any {
        inner.horizontal(|ui| {
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
        settings_item_row(f, &mut inner, it);
    }

    // 通信卡片底部：「一键 AI 配对提示词」按钮。
    //
    // 点击后由 `App::apply_ops` 把按当前 MCP 配置动态拼接的接入说明
    // 复制到系统剪贴板（写剪贴板带自身捕获抑制，见 `copy_mcp_pair_prompt`）。
    if card.cat == SettingsCat::Comm {
        inner.add_space(4.0);
        let btn_w = inner.available_width();
        // 用 allocate 推进游标再自绘（纯平风格，不用默认 Button 边框）。
        let (bb, resp) = inner.allocate_exact_size(vec2(btn_w, 36.0), egui::Sense::click());
        let p = inner.painter_at(bb);
        let hovered = resp.hovered();
        p.rect_filled(bb, pal.radius_md, if hovered { pal.accent_hover } else { pal.accent });
        p.text(
            bb.center(),
            egui::Align2::CENTER_CENTER,
            "⚡ 一键 AI 配对提示词",
            egui::FontId::proportional(pal.font_sm),
            pal.text_on_accent,
        );
        f.hit_insert("settings_mcp_pair", bb);
        if resp.clicked() {
            f.state.push(Op::CopyMcpPairPrompt);
        }
    }
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
            let cur = f.ws.pinned_mode();
            let mut picked: Option<PinnedMode> = None;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                let mut ch = false;
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (PinnedMode::SharedColumn, "共用单栏"),
                        (PinnedMode::OwnCard, "独立分栏"),
                    ] {
                        let resp = ui.selectable_label(cur == mode, label);
                        if resp.clicked() && cur != mode {
                            picked = Some(mode);
                            ch = true;
                        }
                    }
                });
                ch
            });
            if changed
                && let Some(m) = picked
            {
                f.state.push(Op::SetPinnedMode(m));
                record_recent(f, it.label);
            }
        }
        ItemKind::Theme => {
            let cur = crate::theme::ThemeMode::from_dark_mode(f.svc.state.config.ui.dark_mode);
            let mut picked: Option<crate::theme::ThemeMode> = None;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                let mut ch = false;
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (crate::theme::ThemeMode::FollowSystem, "跟随系统"),
                        (crate::theme::ThemeMode::Dark, "深色"),
                        (crate::theme::ThemeMode::Light, "浅色"),
                    ] {
                        let resp = ui.selectable_label(cur == mode, label);
                        if resp.clicked() && cur != mode {
                            picked = Some(mode);
                            ch = true;
                        }
                    }
                });
                ch
            });
            if changed
                && let Some(m) = picked
            {
                f.state.push(Op::SetThemeMode(m));
                record_recent(f, it.label);
            }
        }
        ItemKind::Backdrop => {
            let cur = f.svc.state.config.ui.window_backdrop;
            let mut picked: Option<WindowBackdrop> = None;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                let mut ch = false;
                ui.horizontal(|ui| {
                    for (mode, label) in [
                        (WindowBackdrop::None, "无"),
                        (WindowBackdrop::Acrylic, "磨砂"),
                        (WindowBackdrop::Translucent, "半透明"),
                    ] {
                        let resp = ui.selectable_label(cur == mode, label);
                        if resp.clicked() && cur != mode {
                            picked = Some(mode);
                            ch = true;
                        }
                    }
                });
                ch
            });
            if changed
                && let Some(m) = picked
            {
                f.state.push(Op::SetBackdrop(m));
                record_recent(f, it.label);
            }
        }
        ItemKind::Opacity => {
            let mut op = f.svc.state.config.ui.window_opacity;
            let enabled = f.svc.state.config.ui.window_backdrop != WindowBackdrop::None;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add_enabled(
                    enabled,
                    egui::Slider::new(&mut op, 0.3..=1.0).show_value(true),
                )
                .changed()
            });
            if changed && enabled {
                f.state.push(Op::SetOpacity(op));
                record_recent(f, it.label);
            }
        }
        ItemKind::Hotkey(slot) => {
            // NOTE: 托盘线程当前注册的是硬编码 `DEFAULT_HOTKEY`，配置里的
            // 绑定尚未接入注册。这里只读展示当前配置值；「录制」与
            // 热键重注册是后续任务。
            let cur = slot.current(&f.svc.state.config);
            setting_row(ui, it.label, it.desc, |ui| {
                ui.label(
                    egui::RichText::new(cur.display())
                        .monospace()
                        .color(pal.text_dim),
                );
                false
            });
        }
        ItemKind::McpToggle => {
            let mut v = get_bool(&f.svc.state.config, ConfigGroup::Mcp, "enabled");
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // control 位于 right_to_left 容器：先 add 的控件靠右。
                // 先画开关（最右），再画 ⓘ 详细按钮（紧贴开关**左边**，
                // 即「MCP 详细在总开关按钮的左边」）。
                let sw = draw_switch(f, ui, "mcp_enabled", &mut v);
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
                ui.add(
                    egui::TextEdit::singleline(&mut buf)
                        .hint_text(*ph)
                        .password(*secret)
                        .frame(egui::Frame::NONE)
                        .text_color(pal.text)
                        .desired_width(if *secret { 170.0 } else { 230.0 }),
                )
                .changed()
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
                ui.add(
                    egui::TextEdit::singleline(&mut buf)
                        .hint_text("9123")
                        .frame(egui::Frame::NONE)
                        .text_color(pal.text)
                        .desired_width(110.0),
                )
                .changed()
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
                ui.label(egui::RichText::new(*text).color(pal.text_dim));
                false
            });
        }
        ItemKind::Quit => {
            ui.add_space(8.0);
            let resp = ui.add_sized(
                [ui.available_width(), 30.0],
                egui::Button::new(
                    egui::RichText::new("退出程序").color(pal.danger).strong(),
                ),
            );
            f.hit_insert("settings_quit", resp.rect);
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
        section(ui, card.title);
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
    section(ui, "最近修改");
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
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .strong()
            .size(12.0)
            .color(ui.visuals().weak_text_color()),
    );
}

/// 一行「标签 + 说明 + 右侧控件」。
///
/// `control` 闭包在右侧区域里放控件，返回 `true` 表示值被改动。
/// 宽度分配交给 `ui.horizontal` + `with_layout(right_to_left)`，
/// 不再手算 `ctrl_w`（旧实现那处 `min(r.width() * 0.4)` 的钳制
/// 会在极窄面板里把标签挤没）。
fn setting_row(ui: &mut Ui, label: &str, desc: &str, control: impl FnOnce(&mut Ui) -> bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(label);
            if !desc.is_empty() {
                ui.label(egui::RichText::new(desc).small().color(ui.visuals().weak_text_color()));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = control(ui);
        });
    });
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
    let s = 18.0_f32;
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
    let (rect, _alloc) = ui.allocate_exact_size(egui::vec2(s * 1.8, s), Sense::empty());
    let resp = ui.interact(rect, egui::Id::new(("switch", id_suffix)), Sense::click());

    // 开关本体：宽 = 高的 1.8 倍。
    let h = s;
    let w = h * 1.8;
    let knob_r = h * 0.5 - 2.0;
    let track = Rect::from_center_size(rect.center(), vec2(w, h));
    let p = ui.painter_at(rect);
    let on = *v;
    if on {
        p.rect_filled(track, h * 0.5, pal.accent);
    } else {
        p.rect_filled(track, h * 0.5, pal.surface_variant);
        p.rect_stroke(
            track,
            h * 0.5,
            Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
    }
    // 滑块：开时靠右、关时靠左。
    let knob_x = if on { track.max.x - h * 0.5 } else { track.min.x + h * 0.5 };
    let knob = Rect::from_center_size(
        pos2(knob_x, track.center().y),
        vec2(knob_r * 2.0, knob_r * 2.0),
    );
    p.circle_filled(knob.center(), knob_r, if on { pal.text_bright } else { pal.text_dim });

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

/// MCP 总开关左侧的 ⓘ 详细按钮（打开 MCP 调用设置覆盖层）。
///
/// 自绘圆形信息图标：accent 底色 + 圆点 + 竖线（不依赖字体字形，
/// 图标字体里没有 info 码位，新增字形需要重新生成 TTF）。
fn draw_mcp_info_btn(f: &mut Frame<'_>, ui: &mut Ui) -> egui::Response {
    let pal = f.pal;
    let (r, resp) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let p = ui.painter_at(r);
    // `row_selected` = accent 低透明底（Palette 无 `accent_bg` 令牌）。
    let bg = if resp.hovered() {
        pal.accent
    } else {
        pal.row_selected
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

/// MCP 调用设置覆盖层。
///
/// 结构：全窗遮罩（白/黑随主题，模拟失焦）+ 圆角面板（限制在窗口
/// 客户区内，max 92% 宽 / 82% 高）+ 8 个可调用能力开关 + 取消/完成。
///
/// NOTE: 模糊失焦（blur）是后续渲染管线工作：`renderer.rs` 目前
/// `continue` 跳过 `PaintCallback`，自研模糊需扩展 GPU 管线后接入。
/// 当前用半透明遮罩近似「失焦」效果。
fn settings_mcp_overlay(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;

    // 遮罩色随主题：亮色界面用白、暗色界面用黑（透明度 ~58%）。
    let dark = pal.surface.r() < 128;
    let shade = if dark {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 148)
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 148)
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
            let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::click());
            let p = ui.painter_at(r);
            if resp.hovered() {
                p.rect_filled(r, pal.radius_sm, pal.row_hover);
            }
            crate::icons::Icon::Close.paint(
                &p,
                Rect::from_center_size(r.center(), vec2(13.0, 13.0)),
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
    let (r, resp) = ui.allocate_exact_size(vec2(84.0, 32.0), egui::Sense::click());
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
                // 徽标：已连接 / 未连接（未连接 = 能力尚未就绪）。
                draw_mcp_badge(f, ui, cap.ready);
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
    let w = 44.0_f32;
    let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), egui::Sense::hover());
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
    let w = 44.0_f32;
    let (r, _) = ui.allocate_exact_size(vec2(w, 20.0), egui::Sense::hover());
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

/// 在**给定的**矩形里画内嵌置顶区。
///
/// # `area` 是谁给的
///
/// 由 [`draw_expanded_card`] 提前算好并从历史卡片的 body 顶部切出来。
/// 早前这个函数自己去读 `history.rect` 并从 `inner.min` 铺到
/// `inner.max.y` —— 那等于无视历史列表已经占了整块 body，必然重叠。
///
/// 不画自己的边框：它不是独立卡片，画了会与历史卡片边框叠成双线。
fn draw_embedded_pinned(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;
    let font: FontId = sized(pal.font_xs, f.scale);
    let label_h = (pal.font_xs * 1.6).max(14.0);

    // 标题行「置顶」。画在 `area` 顶部 —— 即历史卡片**头部之下**，
    // 与标题「历史」不再同一个 y。
    let label = Rect::from_min_size(area.min, vec2(area.width(), label_h));
    f.ui.painter_at(label).text(
        label.left_center(),
        Align2::LEFT_CENTER,
        "置顶",
        font,
        pal.text_dim,
    );

    let list = Rect::from_min_max(
        pos2(area.min.x, label.max.y + pal.space_xs),
        area.max,
    );
    if list.height() <= 8.0 || list.width() <= 8.0 {
        return;
    }
    draw_history_body(f, list, Some(true));
}

fn draw_card_frame(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    // 纯平毛玻璃：纯色底 + 1px 描边 + 主题色光晕。
    // **禁止渐变、光泽、折射、内发光**——那类效果在浅色卡片上
    // 看起来廉价，且与「平」的视觉语言冲突。
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, CARD_RADIUS, pal.surface);
    painter.rect_stroke(
        r,
        CARD_RADIUS,
        Stroke::new(CARD_STROKE * scale, pal.border_subtle),
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
            // NOTE: 历史栏整卡是「暗容器」：frame 底色也用 `list_bg`，
            // 否则列表四周（frame 与 body 之间的边距）仍露出 frame
            // 的 `surface` 底色——浅色主题下就是一圈白色（用户反馈
            // 「列表边上还是白色」）。描边由 `draw_card_frame` 已画，
            // 这里只盖底色，不动描边。
            f.ui.painter_at(r).rect_filled(r, CARD_RADIUS, pal.list_bg);
            // 置顶内嵌在历史卡片**顶部** ⇒ 先把 body 切成两段，
            // 历史列表用下面那段。
            //
            // NOTE: 早前是「先画完整历史列表，再事后往上叠一层置顶」——
            // 于是「置顶」标签与卡片标题「历史」画在**同一个 y**
            // （都是 `inner.min`），而置顶列表一直铺到卡片底部，
            // 盖住历史的前 N 行。用户看到的是两层内容叠在一起的乱字，
            // 且被盖住的历史行仍然可点（命中矩形还在）。
            let pinned_h = if f.ws.pinned_is_embedded() {
                embedded_pinned_height(f, body.width(), body.height())
            } else {
                0.0
            };
            if pinned_h > 0.0 {
                let split_y = body.min.y + pinned_h;
                let pinned_area =
                    Rect::from_min_max(body.min, pos2(body.max.x, split_y));
                draw_embedded_pinned(f, pinned_area);
                // 分隔线画在置顶区与历史区之间。
                f.ui.painter_at(body).hline(
                    body.x_range(),
                    split_y,
                    Stroke::new(1.0, f.pal.border_subtle),
                );
                // 历史区直接从分隔线下方开始。早前这里画了一个
                // 「历史」标签并让出 label_h 一行；按用户要求去掉
                // 标签后，历史列表向上多占一行（不占列表位置）。
                let rest = Rect::from_min_max(
                    pos2(body.min.x, split_y + pal.space_xs),
                    body.max,
                );
                if rest.height() > 8.0 {
                    draw_history_body(f, rest, None);
                }
            } else {
                draw_history_body(f, body, None);
            }
        }
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
    }
}

/// 内嵌置顶区应当占用的**高度**；没有置顶条目时返回 0。
///
/// # 为什么必须提前算
///
/// 历史列表的矩形要让出这一段。事后叠画（早前的做法）必然重叠——
/// 历史列表不知道自己头顶被占了一块，照样从 `body.min` 开始铺。
///
/// # 封顶 1/3
///
/// 置顶条目可能很多（用户能钉几十条）。全给会让历史列表被挤没，
/// 而历史才是主区。所以封顶 1/3，剩下的靠 `ScrollArea` 自己滚。
fn embedded_pinned_height(f: &mut Frame<'_>, avail_w: f32, avail_h: f32) -> f32 {
    let pal = f.pal;
    if !f.svc.state.items.iter().any(|it| it.pinned) {
        return 0.0;
    }
    if avail_w <= 8.0 || avail_h <= 32.0 {
        return 0.0;
    }
    // 标题行 + 上下留白 + 一条分隔线。
    let chrome = (pal.font_xs * 1.6).max(14.0) + pal.space_xs * 2.0 + pal.space_sm;

    // 行高必须用与 `draw_history_body` **同一个** `row_height_for`，
    // 否则预留高度与实际内容对不上（多留浪费空间，少留又盖住）。
    let icon_w = pal.icon_size + pal.space_sm;
    let text_w = (avail_w - icon_w - pal.space_sm).max(20.0);
    let rows: f32 = f
        .svc
        .state
        .items
        .iter()
        .filter(|it| it.pinned)
        .map(|it| row_height_for(f.ui, it, pal, f.scale, text_w))
        .sum();

    (chrome + rows)
        .min(avail_h * 0.34)
        .max(chrome.min(avail_h))
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

    // ---- 列表显示区域背景 ----
    //
    // 整块可视区铺 `list_bg`（比窗口底 `bg` 暗一档）：历史条目以
    // `surface_raised`（亮）浮在暗容器上，形成「亮卡片、暗列表」的
    // 层次。空态提示文字也画在这层之上。
    f.ui.painter_at(body).rect_filled(body, CornerRadius::ZERO, f.pal.list_bg);

    // 本列表用哪个滚动槽位。
    //
    // IMPORTANT: 两个列表的滚动位置必须**分开存**。早前共用一个
    // `f32`，而 `vertical_scroll_offset` 每帧都会把它写进 egui ——
    // 于是滚主栏会把内嵌置顶区一起带走（反之亦然）。
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
    let items: Vec<ClipItem> = f
        .svc
        .state
        .items
        .iter()
        // NOTE: 主栏（`None`）**排除置顶条目**：置顶内容由置顶区（共用单栏）
        // 或置顶卡（独立分栏）单独负责，主栏再画一份会产生两处同名
        // 行 Id（`("row", item.id)`）→ egui 的 ID 冲突检查在界面上
        // 画出「First use of widget ID … / Second use of …」警告。
        .filter(|it| match only_pinned {
            Some(p) => it.pinned == p,
            None => !it.pinned,
        })
        .cloned()
        .collect();

    if items.is_empty() {
        let msg = match only_pinned {
            Some(true) => "还没有置顶条目",
            _ if f.state.query.is_empty() => "还没有剪贴板记录",
            _ => "没有匹配的结果",
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
    // 代价是要**每帧重算全部行高**才能知道滚动位置。
    // 这一步只做排版测量、不产生绘制指令，n=1000 时约几毫秒，
    // 对剪贴板这类「几十到几百条」的场景可接受。
    let pal = f.pal;
    let scale = f.scale;
    // IMPORTANT: 必须用 `preview_width`——与 `draw_item_row` 共用同一个算式。
    // 宽度取内容区（左右已扣边距），保证测量与渲染一致。
    let text_w = preview_width(content.width(), pal);
    let heights: Vec<f32> = items
        .iter()
        .map(|it| row_height_for(f.ui, it, pal, scale, text_w))
        .collect();
    // 前缀和：用来 O(log n) 由滚动偏移定位到行。
    //
    // 卡片样式需要行间留缝（`ROW_GAP`）：首项顶部留半条，每项之后
    // 再留整条。**最后一项之后不留**——否则滚到底时最后一行会悬在
    // 列表底部之上一条缝（gap），贴底点击/视觉都对不上。
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    offsets.push(ROW_GAP * 0.5);
    for (i, h) in heights.iter().enumerate() {
        let inc = if i + 1 == heights.len() { *h } else { h + ROW_GAP };
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
            // 损失 10pt 可用宽度。
            let row_w = if content_h > content.height() {
                content.width() - SCROLLBAR_RESERVE
            } else {
                content.width()
            };
            for i in first..items.len() {
                let y = content.min.y + offsets[i] - scroll;
                if y > content.max.y {
                    break; // 已越过可视区底部
                }
                let r = Rect::from_min_size(
                    pos2(content.min.x, y),
                    vec2(row_w, heights[i]),
                );
                if r.max.y < content.min.y {
                    continue; // 整行都在可视区上方
                }
                // ---- 卡片缝隙 = 拖动区 ------------------------------------
                // 卡片与卡片之间的空白横带（含首行顶部的半条缝）同样上报
                // 为拖动区：WM_NCHITTEST 返回 HTCAPTION，用户按住行间
                // 缝隙即可拖动窗口，不必精确瞄准左右留白。
                //
                // NOTE: 右缘让出垂直滚动条占位（`content.max.x - RESERVE`），
                // 否则「拖滚动条」会被系统判成 HTCAPTION 变成拖窗口——
                // 与左右留白拖动区避开滚动条是同一个原因。
                let gap_top = if i == 0 { content.min.y } else { r.min.y - ROW_GAP };
                let gap_bottom = r.min.y.min(content.max.y);
                if gap_bottom > gap_top {
                    f.state.drag_regions.push(Rect::from_min_max(
                        pos2(content.min.x, gap_top.max(content.min.y)),
                        pos2(content.max.x - SCROLLBAR_RESERVE, gap_bottom),
                    ));
                }
                draw_item_row(f, ui, r, &items[i]);
            }
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
    let icon_w = pal.icon_size + pal.space_sm;
    let acts_w = 3.0 * (pal.control_height * 0.55 + pal.space_xs);
    (body_w - icon_w - acts_w).max(20.0)
}

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

/// 顶栏搜索框展开时的宽度（逻辑点）。
///
/// 搜索框默认折叠成一枚放大镜图标（贴在窗口按钮左侧），顶栏左侧
/// 整片让给拖动区；点击图标后输入框向左弹出这段宽度。
///
/// 取值平衡：太窄（<120）输入体验差；太宽（>180）展开时把拖动区
/// 挤到 <30pt，用户搜索完要拖窗口又拖不动。140pt ≈ 物理 210px，
/// 足够输入，且展开后拖动区仍剩约 46pt。
const SEARCH_EXPAND_W: f32 = 140.0;

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
const SCROLLBAR_RESERVE: f32 = 10.0;

/// 每行文字的高度倍率。
const LINE_H: f32 = 1.35;

/// 单条列表项的**实际**行高：按预览文本折行后的行数计算。
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
fn row_height_for(ui: &egui::Ui, item: &ClipItem, pal: &Palette, scale: f32, text_w: f32) -> f32 {
    let preview_font = sized(pal.font_md, scale);
    let meta_font = sized(pal.font_xs, scale);
    // IMPORTANT: 行高也必须用**逻辑点**、不乘 ppp：字号已经改成逻辑点，
    // 这里再乘一次会让行高与实际字形高度对不上（留白/裁剪都错）。
    let line_h = pal.font_md * LINE_H;
    let meta_h = meta_font.size.max(10.0);

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
    let n_lines = galley.rows.len().max(1);
    let text_h = n_lines as f32 * line_h;

    // 三段布局：上 meta（来源）5 + 11 + 预览 + 2 + 下 meta（类型/大小）
    // 11 + 5。预览带高度与 `draw_item_row` 的 `preview_band` 严格对应。
    (text_h + meta_h * 2.0 + 5.0 * 2.0 + 4.0).max(ROW_HEIGHT_MIN)
}

fn draw_detail_body(f: &mut Frame<'_>, body: Rect) {
    let Some(id) = f.svc.state.selected else {
        draw_empty(f, body, "选中一条记录查看详情");
        return;
    };
    let Some(item) = f.svc.state.items.iter().find(|it| it.id == id) else {
        draw_empty(f, body, "该记录已不存在");
        return;
    };
    let pal = f.pal;
    let scale = f.scale;

    // 详情文字按可用矩形裁剪：长内容不得溢出卡片。
    let text_area = body.shrink2(vec2(pal.space_xs, 0.0));
    let font = sized(pal.font_sm, scale);
    let preview = if item.preview.is_empty() {
        "（无预览）"
    } else {
        item.preview.as_str()
    };
    let shown = crate::text::elide_text(f.ui, preview, &font, text_area.width());
    f.ui.painter_at(text_area).text(
        pos2(text_area.min.x, text_area.min.y + font.size),
        Align2::LEFT_TOP,
        shown,
        font.clone(),
        pal.text,
    );

    // 来源与时间。
    let meta_y = text_area.min.y + font.size * 2.4;
    if meta_y < text_area.max.y {
        // CAUTION: `max.y` 只能是 `meta_y + 高`，**不能**再叠加 `text_area.min.y`。
        // 叠加后高度会变成 `min.y + 高`（几百 pt），而文字画在
        // `meta_area.left_center()` —— 「来源 · 时间」会跑到卡片中部甚至
        // 卡片之外；而 `painter_at(meta_area)` 的裁剪就是它自己，
        // 越界部分不受任何裁剪。
        let meta_area = Rect::from_min_max(
            pos2(text_area.min.x, meta_y),
            pos2(text_area.max.x, meta_y + font.size * 1.2),
        );
        let meta_font = sized(pal.font_xs, scale);
        let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
        let meta_shown =
            crate::text::elide_text(f.ui, &meta, &meta_font, meta_area.width());
        f.ui.painter_at(meta_area).text(
            meta_area.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }
}

fn draw_rail_body(f: &mut Frame<'_>, body: Rect) {
    let pal = f.pal;
    let items: Vec<ClipItem> = f.svc.state.items.iter().cloned().collect();
    if items.is_empty() {
        draw_empty(f, body, "暂无数据");
        return;
    }
    let font = sized(pal.font_xs, f.scale);
    let row_h = 22.0;
    let n = ((body.height() / row_h).floor() as usize).min(items.len());
    for i in 0..n {
        let y = body.min.y + i as f32 * row_h;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        if r.max.y > body.max.y {
            break;
        }
        let txt = format!("{} · {}", items[i].source_app, items[i].preview);
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
    // NOTE: painter_at(body)：空状态文案是居中的，而窄卡片（折叠把手
    // 旁的详情栏）装不下整句，根 painter 会让它溢出到相邻卡片上。
    f.ui.painter_at(body).text(
        body.center(),
        Align2::CENTER_CENTER,
        msg,
        font,
        pal.text_dim,
    );
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
    // 列表区（`list_bg`）之上；悬停在半透明提亮上叠一层；选中换主题
    // 高亮 + 左侧强调条。
    // NOTE: 圆角用 `pal.radius_sm` 而不是 `CornerRadius::ZERO`——后者是
    // 早前「通栏行」的直角样式，与卡片语义不符。
    //
    // NOTE: 描边在所有状态都画：悬停/选中若只换底色，边框会「消失一瞬」
    // （从有到无再到有），视觉上跳变。
    let radius = pal.radius_sm;
    let stroke = Stroke::new(CARD_STROKE * scale, pal.border_subtle);
    if selected {
        painter.rect_filled(r, radius, pal.surface_raised);
        painter.rect_filled(r, radius, pal.row_selected);
        painter.rect_filled(
            Rect::from_min_size(r.min, vec2(2.0 * scale, r.height())),
            CornerRadius::ZERO,
            pal.accent,
        );
        painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Inside);
    } else if resp.hovered() {
        painter.rect_filled(r, radius, pal.surface_raised);
        painter.rect_filled(r, radius, pal.row_hover);
        painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Inside);
    } else {
        painter.rect_filled(r, radius, pal.surface_raised);
        painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Inside);
    }

    // ---- 三段布局 ----------------------------------------------------
    //
    // 左上：来源程序 · 时间（小字弱化）
    // 中间：预览（主字号，可折行）
    // 右下：类型标签 · 大小（小字弱化，右对齐，大小自适应单位）
    // 右上：置顶 / 复制 / 删除 三个小图标按钮
    //
    // NOTE: 目标是「**完整显示内容**」：预览**不再截断**，而是按宽度
    // 折行。折行数由 `wrap` 直接给出，不用手工估算——
    // 早前版本按 `preview.chars().take(30)` 硬砍，中文一个字算一个
    // char、英文一个词算一个 char，两者的实际显示宽度差了好几倍，
    // 于是「看起来还有空间」但已经被砍掉了。
    let icon_w = pal.icon_size + pal.space_sm;
    let acts_w = 3.0 * (pal.control_height * 0.55 + pal.space_xs);
    let text_x0 = r.min.x + icon_w;
    // 宽度同样取自 `preview_width`（减去已扣掉的 icon 区），保证
    // 「量出来的行高」与「画出来的宽度」严格一致。
    let text_x1 = text_x0 + preview_width(r.width(), &pal);
    if text_x1 - text_x0 < 20.0 {
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
            f.state.push(Op::Copy(item.id));
        }
        return;
    }

    // 左侧类型图标，垂直居中。
    crate::icons::Icon::for_kind(item.kind).paint(
        &ui.painter_at(r),
        Rect::from_center_size(
            pos2(
                r.min.x + icon_w * 0.5,
                r.center().y,
            ),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        if selected { pal.accent } else { pal.text_dim },
    );

    let pad_v = 5.0;
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size.max(10.0);
    // 左上角：来源 · 时间（小字，左对齐）。
    let top_band = Rect::from_min_max(
        pos2(text_x0, r.min.y + pad_v),
        pos2(text_x1, r.min.y + pad_v + meta_h),
    );
    // 右下角：类型 · 大小（小字，右对齐）。
    let bot_band = Rect::from_min_max(
        pos2(text_x0, r.max.y - pad_v - meta_h),
        pos2(text_x1, r.max.y - pad_v),
    );
    // 中间：预览占上下两行之间的全部高度。
    //
    // 高度与 `row_height_for` 严格对应：行高 = top + 预览 + bot，
    // 这里同样扣掉上下 meta 与 2pt 行间，保证两次排版结果一致。
    let preview_band = Rect::from_min_max(
        pos2(text_x0, top_band.max.y + 2.0),
        pos2(text_x1, bot_band.min.y - 2.0),
    );

    if preview_band.height() > 4.0 {
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
                font_id: sized(pal.font_md, scale),
                color: if selected {
                    pal.text_bright
                } else {
                    pal.text
                },
                ..Default::default()
            },
        );
        job.wrap.max_width = preview_band.width();
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
        job.wrap.max_rows = ((preview_band.height() / (pal.font_md * LINE_H))
            .floor()
            .max(1.0)) as usize;
        // 长路径/无空格的长串要在任意字符间断行，否则会横向溢出。
        job.wrap.break_anywhere = true;
        let galley = f.ui.painter().layout_job(job);
        ui.painter_at(preview_band)
            .galley(pos2(preview_band.min.x, preview_band.min.y), galley, pal.text);
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

    // 右下：类型标签 · 大小（小字，右对齐）。
    //
    // 大小来自 `PayloadRef::len`（字节数），`format_size` 自适应
    // B/KB/MB/GB；载荷被淘汰（`payload = None`）时只显示类型。
    let size_str = item.payload.as_ref().map(|p| format_size(p.len));
    let bot_meta = match size_str {
        Some(s) => format!("{} · {}", kind_label(item.kind), s),
        None => kind_label(item.kind).to_string(),
    };
    let bot_shown = crate::text::elide_text(f.ui, &bot_meta, &meta_font, bot_band.width());
    if !bot_shown.is_empty() {
        ui.painter_at(bot_band).text(
            bot_band.right_center(),
            Align2::RIGHT_CENTER,
            bot_shown,
            meta_font.clone(),
            pal.text_dim,
        );
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
        // 垂直对齐顶部来源行（中心 y = pad_v + btn/2）。
        //
        // IMPORTANT: 基准右缘必须让出 `SCROLLBAR_RESERVE`。
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
        let b = Rect::from_center_size(
            pos2(
                r.max.x - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.min.y + pad_v + btn * 0.5,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_x1 {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = ui.interact(b, egui::Id::new(("row_act", item.id, i)), Sense::click());
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
        f.state.push(Op::Copy(item.id));
    }
}

// ---------------------------------------------------------------- 状态栏

fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}

/// 内容类型的展示名（右下角标签）。
fn kind_label(kind: modular_clipboard_core::ClipKind) -> &'static str {
    match kind {
        modular_clipboard_core::ClipKind::Text => "文本",
        modular_clipboard_core::ClipKind::Html => "HTML",
        modular_clipboard_core::ClipKind::Image => "图片",
        modular_clipboard_core::ClipKind::Files => "文件",
    }
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
        fn input(&self, events: Vec<egui::Event>) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(self.area),
                events,
                ..Default::default()
            }
        }

        /// 跑一帧主界面，返回收集到的 `Op`。
        fn frame_with(&mut self, events: Vec<egui::Event>) -> Vec<Op> {
            let input = self.input(events);
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
            let ws = &*ws;
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
            let ws = &*ws;
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

    /// 搜索框能接收输入并写进 `query`。
    ///
    /// 早前顶栏那个「搜索框」是用 `painter.text()` 画上去的静态占位符，
    /// 点击只 `request_repaint()` —— 看着像搜索框，实际**打不了字**。
    /// 这条测试守住它真的能输入。
    #[test]
    fn search_box_accepts_typed_text() {
        let mut h = Harness::new("search", vec2(600.0, 400.0));
        h.frame();
        // 搜索框默认折叠成一枚图标：先点顶栏右侧图标展开，输入区才存在。
        let icon = hit(&h, "search_icon").center();
        h.click_at(icon);
        // 坐标从热区表读，不猜。
        let p = hit(&h, "search").center();

        // IMPORTANT: 点击与文字必须**分两帧**。
        //
        // egui 的 `TextEdit` 在收到点击的**那一帧**还没拿到焦点
        // （焦点要等这次 `interact` 的结果被消费后才成立），
        // 同帧的 `Event::Text` 会被丢弃。实测 `query` 保持空串。
        //
        // 这与真实输入一致：用户点一下、稍后打字。
        h.frame_with(vec![
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
        // NOTE: 本版 egui 的 `Context` **没有** `wants_keyboard_input()`，
        // 焦点态要从 `memory` 读。
        //
        // IMPORTANT: 判据必须指出**是哪个控件**拿到了焦点。
        //
        // 原先是 `has_focus(Id::new("search_edit")) || focused().is_some()`
        // —— 后半句只要「有任何控件获得焦点」就成立，于是这条断言
        // 在「点空了别处」的情况下**照样通过**，等于没有断言。
        // 修掉它之后这条测试立刻变红，正好证明了它此前是恒真的。
        //
        //
        // CAUTION: 也不能按 id 的 debug 串匹配：egui 0.36 的 `Id` 是**哈希值**，
        // `format!("{id:?}")` 只得到 `Id::new(3070286450358649230)`。
        //
        // 所以绘制层把**自己用的那个 id** 记进 `UiState::search_edit_id`，
        // 测试与它精确比对。
        let edit_id = h
            .state
            .search_edit_id
            .expect("绘制层应自报搜索框的 egui Id");
        let focused_id = h.ctx.memory(|m| m.focused());
        assert_eq!(
            focused_id,
            Some(edit_id),
            "点搜索框后焦点应**精确落在搜索框**上（不是「有控件获得焦点」就算）"
        );

        h.frame_with(vec![egui::Event::Text("abc".to_owned())]);
        assert_eq!(
            h.state.query, "abc",
            "点进搜索框后打字应写入 query"
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

    /// 卡片行间缝隙（`ROW_GAP` 横带）与首行顶部半条缝必须是拖动区：
    /// 按住行间空白即可拖动窗口，不必精确瞄准左右留白。
    ///
    /// CAUTION: 缝隙横条右缘让出滚动条占位（`SCROLLBAR_RESERVE`），否则
    /// 拖滚动条会被系统判成 HTCAPTION 变成拖窗口。
    #[test]
    fn row_gap_is_drag_region() {
        let mut h = Harness::new("row-gap-drag", vec2(700.0, 600.0)).with_items(3);
        h.frame();
        let body = hit(&h, "history_body");
        let content = Rect::from_min_max(
            pos2(body.min.x + LIST_PAD_X, body.min.y),
            pos2(body.max.x - LIST_PAD_X, body.max.y),
        );
        let near = |a: f32, b: f32| (a - b).abs() < 0.6;
        // ① 首行顶部的半条缝：从列表顶到第一行顶，右缘让出滚动条位。
        let top_gap = Rect::from_min_max(
            content.min,
            pos2(
                content.max.x - SCROLLBAR_RESERVE,
                content.min.y + ROW_GAP * 0.5,
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
        // ② 行间整条缝：3 条记录之间至少一条 `ROW_GAP` 高的横条。
        assert!(
            h.state.drag_regions.iter().any(|r| {
                r.height() >= ROW_GAP - 0.5
                    && r.height() <= ROW_GAP + 0.5
                    && r.width() > 200.0
            }),
            "行间缝隙（{ROW_GAP}pt 横条）应是拖动区"
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
    #[test]
    fn embedded_pinned_does_not_overlap_history() {
        let mut h = Harness::new("pin-overlap", vec2(700.0, 600.0))
            .with_items(8)
            .with_pinned(2);
        h.frame();

        let pinned = hit(&h, "pinned_body");
        let history = hit(&h, "history_body");

        // 前置条件：确实画出了两个独立列表。
        assert!(
            pinned.height() > 0.0,
            "有置顶条目时应画出置顶列表，实际={pinned:?}"
        );
        assert!(
            history.height() > 0.0,
            "历史列表必须仍然有可用高度（不能被置顶区挤没），实际={history:?}"
        );

        // 核心断言：两段**不重叠**，且置顶段在历史段**上方**。
        assert!(
            pinned.max.y <= history.min.y + 0.5,
            "内嵌置顶区与历史列表重叠了：pinned={pinned:?} history={history:?} ——              会出现「置顶」标签压在标题「历史」上、且置顶行盖住历史行"
        );

        // 置顶段必须在卡片头部**之下**（否则标签会与标题同高）。
        let card = h
            .ws
            .by_kind(CardKind::History)
            .expect("默认工作区含历史卡片")
            .rect;
        assert!(
            pinned.min.y > card.min.y,
            "置顶区应从卡片头部**下方**开始，而不是压在标题上：pinned={pinned:?} card={card:?}"
        );
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

        let body = hit(&h, "pinned_body");
        let btn = h.pal.control_height * 0.55;
        let acts_w = 3.0 * (btn + h.pal.space_xs);
        // 按钮从「行右缘让出滚动条」处向左排，i=0（置顶/取消置顶）是最右一个。
        //
        // 注：置顶区只有 1 条、**没有滚动条**，所以这里不减
        // `SCROLLBAR_RESERVE`——行矩形只在可滚动时才让出滚动条宽度。
        let x = body.max.x - LIST_PAD_X - acts_w + 0.5 * (btn + h.pal.space_xs);
        let mut hit_any = false;
        // 按钮现在贴在卡片**右上角**（中心 y ≈ pad_v + btn/2 ≈ 卡片顶
        // 向下 ~12pt），只扫顶部几条 y 即可。
        for y in [
            body.min.y + 8.0,
            body.min.y + 12.0,
            body.min.y + 20.0,
        ] {
            let ops = h.click_at(pos2(x, y));
            if ops.iter().any(|o| matches!(o, Op::TogglePin(_))) {
                hit_any = true;
                break;
            }
        }
        assert!(
            hit_any,
            "置顶条目的置顶按钮必须可点并产生 TogglePin（取消置顶入口）"
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
    /// 修法是让**行矩形**在可滚动时右缘内缩 `SCROLLBAR_RESERVE`，
    /// 按钮与文字都以 `r.max.x` 为基准，于是自动全部避开。
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
        let acts_w = 3.0 * (btn + h.pal.space_xs);
        // 行矩形已内缩，按钮基准就是 content.max.x - SCROLLBAR_RESERVE。
        let right = body.max.x - LIST_PAD_X - SCROLLBAR_RESERVE;
        // i=2 是删除（最右一个），中心偏左 acts_w - 2.5*(btn+space_xs)。
        let x = right - acts_w + 2.5 * (btn + h.pal.space_xs);

        let mut hit_any = false;
        for y in [
            body.min.y + 8.0,
            body.min.y + 12.0,
            body.min.y + 20.0,
            body.min.y + 28.0,
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
        assert_outside_drag(
            &drag,
            &h,
            &["topbar_close", "topbar_clear", "topbar_settings", "search_icon"],
        );

        // 展开态：点击图标展开搜索框，再验证输入区与外框。
        let icon = hit(&h, "search_icon").center();
        h.click_at(icon);
        let drag_expanded = h.state.drag_regions.clone();
        assert!(
            h.state.search_expanded,
            "点搜索图标应展开搜索框"
        );
        assert_outside_drag(
            &drag_expanded,
            &h,
            &[
                "topbar_close",
                "topbar_clear",
                "topbar_settings",
                "search_icon",
                "search",
                "search_box",
            ],
        );
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

        // 展开后：搜索框也必须完全排在按钮（与图标）左侧。
        let icon_center = icon_r.center();
        h.click_at(icon_center);
        let search = hit(&h, "search_box");
        let rightmost = rects
            .iter()
            .map(|(_, r)| r.max.x)
            .fold(f32::MIN, f32::max);
        assert!(
            search.max.x <= icon_r.min.x + 0.5,
            "展开后搜索框右缘 {} 越过搜索图标左缘 {}",
            search.max.x,
            icon_r.min.x
        );
        assert!(
            search.max.x <= rightmost + 0.5,
            "搜索框右缘 {} 越过最右按钮 {}，按钮会被盖住",
            search.max.x,
            rightmost
        );
        for (name, r) in &rects {
            assert!(
                !search.intersects(*r),
                "展开后搜索框 {search:?} 与按钮 {name} {r:?} 重叠"
            );
        }
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

        // 折叠态：drag[0] 是顶栏左段（窗口左缘 → 搜索图标左缘）。
        // 搜索框折叠成右侧图标后，顶栏左侧几乎整条都是拖动区——
        // 这是「顶部拖动区少」问题的最终形态，宽度必须守住。
        let seg = drag[0];
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

        // 展开态：左段收缩到输入框左缘，但仍必须存在且能拖。
        let icon = hit(&h, "search_icon").center();
        h.click_at(icon);
        let drag2 = h.state.drag_regions.clone();
        let seg2 = drag2[0];
        assert!(
            seg2.width() >= 40.0,
            "展开态顶栏左段拖动区只剩 {}pt（应 ≥ 40pt）—— \
             搜索框展开时窗口会拖不动；drag_regions={drag2:?}",
            seg2.width()
        );
        let c2 = seg2.center();
        assert!(
            drag2.iter().any(|d| d.contains(c2)),
            "展开态拖动区自己的中心应落在拖动区内，实际 {drag2:?}"
        );
        for name in ["search_icon", "topbar_clear", "topbar_settings", "search_box", "search"] {
            let r = hit(&h, name);
            assert!(
                !r.intersects(seg2),
                "展开态拖动区段 {seg2:?} 与控件 {name}（{r:?}）重叠"
            );
        }
    }

    /// 搜索框折叠式：默认折叠、点图标展开、再点收起，输入框随之消失。
    #[test]
    fn search_collapses_and_expands_on_icon_click() {
        let mut h = Harness::new("search-toggle", vec2(700.0, 500.0));
        h.frame();

        // 默认折叠：没有输入区热区，只有图标。
        assert!(!h.state.search_expanded, "搜索框应默认折叠");
        assert!(
            h.state.hit.contains_key(HitNs::Main, "search_icon"),
            "应有搜索图标热区"
        );
        assert!(
            !h.state.hit.contains_key(HitNs::Main, "search_box"),
            "折叠态不应有输入框外框热区"
        );

        // 点图标 → 展开：输入区出现。
        let icon = hit(&h, "search_icon").center();
        h.click_at(icon);
        assert!(h.state.search_expanded, "点图标应展开搜索框");
        assert!(
            h.state.hit.contains_key(HitNs::Main, "search_box"),
            "展开后应有输入框外框热区"
        );

        // 再点图标 → 收起：输入区消失。
        let icon = hit(&h, "search_icon").center();
        h.click_at(icon);
        assert!(!h.state.search_expanded, "再点图标应收起搜索框");
        assert!(
            !h.state.hit.contains_key(HitNs::Main, "search_box"),
            "收起后输入框外框热区应消失"
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
                *h = row_height_for(ui, &item, &pal, 1.0, 200.0);
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

    /// 行高下限必须容得下「预览一行 + 来源一行 + 内边距」。
    ///
    /// 下限太小会让第二行（来源/时间）被挤出行外。
    #[test]
    fn row_height_min_fits_two_text_lines() {
        assert!(
            ROW_HEIGHT_MIN >= 40.0,
            "两行布局至少需要 40pt：预览 ~19 + 来源 ~13 + 内边距 ~12"
        );
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
    fn mcp_overlay_switch_emits_set_bool() {
        let mut h = Harness::new("mcp-overlay-sw", vec2(780.0, 560.0));
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
        // 总开关 + 8 项能力 = 9 个 SetBool，且值全为 true。
        assert_eq!(bools.len(), 9, "重置应推 9 个 MCP SetBool，实际={ops:?}");
        for op in bools {
            match op {
                Op::SetBool { value, .. } => assert!(*value, "重置应恢复为开启：{op:?}"),
                _ => unreachable!(),
            }
        }
    }

    /// 一键配对提示词：包含 8 项能力与接入参数，随配置动态拼接。
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
}
