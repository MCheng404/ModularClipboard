//! 布局引擎：模块的停靠、吸附、浮动、折叠。
//!
//! # 为什么要「纯逻辑 + 薄绘制」
//!
//! 「提示画在这里但实际吸到那里」这类 bug 极难发现：绘制对了、判定错了，
//! 两者各自看起来都合理，只有交互时才暴露。要杜绝它，唯一的办法是
//! **让判定与绘制读同一份数据**——本模块��出的 [`snap`] 结果里同时包含
//! 最终矩形与提示线，绘制层只负责把两者都画出来，不做任何二次计算。
//!
//! 于是这里全部是**纯函数**：输入若干矩形与阈值，输出一个结果结构，
//! 不碰 egui 的 `Ui`、不读时钟、不访问全局状态。egui 只在 `panels.rs`
//! 与 `view.rs` 里出现。
//!
//! # 坐标系
//!
//! 一律用 `egui::Rect`（逻辑点，客户区坐标系，原点左上）。窗口尺寸变化时
//! 调用方整体重算，不需要增量更新——一帧一次全量布局的代价远低于
//! 「算错了增量」的排查代价。
//!
//! # 模块的两种形态
//!
//! - [`Placement::Docked`]：占据主布局的一个槽位，参与宽度分配。
//! - [`Placement::Floating`]：脱离主布局，有自己的屏幕矩形，可拖动，
//!   靠近其它模块边缘时会被吸附回去（[`snap`]）。
//!
//! 停靠区（[`DockZone`]）是第三种形态的落点描述：把浮动模块丢到窗口
//! 边缘的预设区域，就落到那里——这是 IDE 里 dock 的核心体验。

use egui::{Rect, Vec2, vec2};

/// 分隔条的可拖动宽度（逻辑点）。
///
/// 取 6 是权衡：小于 4 难以点中，大于 8 会在窄窗口里吃掉内容宽度。
pub const SPLITTER_WIDTH: f32 = 6.0;

/// 吸附判定阈值（逻辑点）。
///
/// 拖动模块时，其边缘进入另一模块边缘的这个距离内即视为「靠得够近」。
/// 取 12 与 Windows 自身的 snap（~10px）接近，手感一致。
pub const SNAP_THRESHOLD: f32 = 12.0;

/// 停靠区触发的距离阈值（逻辑点）。
///
/// 模块中心进入窗口边缘这个距离内时，进入停靠区预览。
pub const DOCK_ZONE_THRESHOLD: f32 = 48.0;

/// 浮动模块的最小尺寸。
///
/// 小于这个尺寸的模块内容会被压到不可读，用户拖出来却什么都看不到，
/// 比不允许拖更糟。
pub const MIN_FLOAT_SIZE: Vec2 = Vec2::new(160.0, 100.0);

/// 停靠槽位最小宽度（逻辑点）。
///
/// 窄于此值时列表里的预览文字会被压成竖条，此时折叠比压窄更好。
pub const MIN_DOCK_WIDTH: f32 = 56.0;

/// 窄侧栏折叠后的宽度（逻辑点）。
///
/// 折叠态仍要露出一条能看见「表/密」竖排标签的窄条，
/// 否则用户不知道这个模块还在。
pub const RAIL_COLLAPSED_WIDTH: f32 = 28.0;

/// 一般模块折叠后的把手宽度（逻辑点）。
///
/// 折叠 = 「收成一条把手」而不是「消失」：把手可见可点，用户才知道这个
/// 模块还在、并且能点回来。侧栏的把手更宽（[`RAIL_COLLAPSED_WIDTH`]），
/// 因为它上面还要显示竖排视图标签。
///
/// 这个宽度参与降级阶梯的阈值计算，因此它**不是装饰值**：
/// 若为 0，`PinnedFolded` 与 `RailHidden` 两档的需求会相等，
/// 阶梯就退化成三级（少一级真正的让步）。
pub const COLLAPSED_HANDLE_WIDTH: f32 = 24.0;

/// 历史列表的最小**可用**宽度（逻辑点）。
///
/// [`MIN_DOCK_WIDTH`] 是「不被压成负宽」的物理底线，不是「还能用」的
/// 产品底线。一行历史条目同时要放下：类型图标 24 + 预览文字 + 两个行内
/// 工具 2×32，加面板内边距 2×8。预览文字低于 ~60 就会折行/竖排，
/// 于是这一档的合计下限 ≈ 24 + 60 + 64 + 16 = 164，取 **160**。
///
/// 低于这个宽度就该让别的模块让位，而不是继续压历史列表——
/// 历史是主区，压它等于把产品唯一不可替代的东西挤没。
pub const MIN_HISTORY_WIDTH: f32 = 160.0;

/// 置顶列表的最小可用宽度（逻辑点）。
///
/// 比历史少一个行内工具（只有「取消置顶」），故取 120。
pub const MIN_PINNED_WIDTH: f32 = 120.0;

/// 详情面板的最小可用宽度（逻辑点）。
///
/// 详情里的动作区是「主按钮 + 58宽副按钮」并排（见 `view.rs` 的
/// `draw_detail`），主按钮还要再减 64 给副按钮，低于 ~180 时
/// 中文动作标签（「粘贴到前台窗口」）必然折行。取 **180**。
pub const MIN_DETAIL_WIDTH: f32 = 180.0;

/// 侧栏展开后的最小可用宽度（逻辑点）。
///
/// 展开态是「竖排标签列 + 列表」并排，标签列 `rail_handle_width`
/// 约 28，留给列表至少 60 才看得出是一列条目。取 **88**。
pub const MIN_RAIL_EXPANDED_WIDTH: f32 = 88.0;

/// 左段（置顶）从剩余宽度里分到的比例。
///
/// 0.28 不是拍脑袋：默认宽度 200 / (200+320+240) ≈ 0.26，
/// 留一点给主区即可。真正决定宽度的是 [`segment_width`] 里的
/// 保底与上限，这个比例只在「宽裕到两个保底都不生效」时才起作用。
pub const LEFT_FRACTION: f32 = 0.28;

/// 右段（详情 + 侧栏）从剩余宽度里分到的比例。
///
/// 同 [`LEFT_FRACTION`]：默认 240+28 / (200+320+240+28) ≈ 0.34，
/// 实际取 0.28 是因为右段有两个模块，让它们各自再分一次更均匀。
pub const RIGHT_FRACTION: f32 = 0.28;

/// 可参与布局的模块。
///
/// 用 `Copy + PartialEq` 而非 `&str`：模块身份要在多处比对
/// （停靠表里查、吸附时排除自身、绘制时取内容），值类型比字符串比较
/// 更快也更容易保证「同一个模块只有一个身份」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Panel {
    /// 左侧：置顶项列表。
    Pinned,
    /// 主区：历史列表。
    History,
    /// 右侧：可切换视图的窄侧栏。
    Rail,
    /// 详情面板。
    Detail,
}

impl Panel {
    /// 全部模块。顺序即默认停靠顺序（历史 → 置顶 → 详情 → 侧栏）。
    pub const ALL: [Panel; 4] = [
        Panel::History,
        Panel::Pinned,
        Panel::Detail,
        Panel::Rail,
    ];

    /// 该模块默认停靠时的建议宽度（逻辑点）。
    pub fn default_width(self) -> f32 {
        match self {
            Panel::History => 320.0,
            Panel::Pinned => 200.0,
            Panel::Detail => 240.0,
            // 侧栏展开态的舒适宽度。默认**放置**是折叠（那是
            // `default_placement` 的事），但「展开后有多宽」是独立问题——
            // 早前这里返回 `RAIL_COLLAPSED_WIDTH`(28)，于是
            // `min_width`(88) > `default_width`(28)，
            // 降级阶梯算出的阈值与实际分配对不上。
            Panel::Rail => 96.0,
        }
    }

    /// 该模块的**最小可用**宽度（逻辑点）。
    ///
    /// 与 [`Panel::default_width`] 的区别是「建议」与「底线」：
    /// 前者是窗口宽裕时的理想值，后者是窄到再压就不可读的下限。
    /// 降级阶梯（[`tier_for`]）完全建立在这个下限之上——
    /// 阈值由它们**算出来**，而不是另拍一组数字。
    pub fn min_width(self) -> f32 {
        match self {
            Panel::History => MIN_HISTORY_WIDTH,
            Panel::Pinned => MIN_PINNED_WIDTH,
            Panel::Detail => MIN_DETAIL_WIDTH,
            Panel::Rail => MIN_RAIL_EXPANDED_WIDTH,
        }
    }

    /// 该模块折叠后占的宽度（逻辑点）。
    ///
    /// 折叠后留一条把手（[`COLLAPSED_HANDLE_WIDTH`]），而不是归零：
    /// 归零会让「折叠」与「隐藏」在求解结果里完全等价，
    /// 用户既看不到也点不回来，且降级阶梯少一级。
    /// 侧栏的把手更宽，因为它上面要显示竖排视图标签。
    pub fn collapsed_width(self) -> f32 {
        match self {
            Panel::Rail => RAIL_COLLAPSED_WIDTH,
            _ => COLLAPSED_HANDLE_WIDTH,
        }
    }

    /// 该模块的默认槽位。
    ///
    /// 降级动作（详情转浮层、置顶折叠、侧栏隐藏）**只作用于仍在默认槽位上的
    /// 模块**：用户已经把它拖去别的槽位，说明他有别的安排，此时强行降级
    /// 会与他的意图打架。
    pub fn default_slot(self) -> Slot {
        match self {
            Panel::History => Slot::Center,
            Panel::Pinned => Slot::Left,
            Panel::Detail | Panel::Rail => Slot::Right,
        }
    }

    /// 标题栏上显示的名字。
    pub fn title(self) -> &'static str {
        match self {
            Panel::History => "历史",
            Panel::Pinned => "置顶",
            Panel::Detail => "详情",
            Panel::Rail => "视图",
        }
    }
}

/// 模块的停靠槽位。
///
/// 槽位是**位置**而非顺序：`Left` / `Center` / `Right` 各自对应主布局上
/// 从左到右的一段。同一槽位可以放多个模块（它们再按 [`Panel::ALL`] 顺序
/// 依次排开），这样「拖到左边」不必关心目标槽位是否已满。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// 主布局左侧。
    Left,
    /// 主布局中央（默认槽）。
    Center,
    /// 主布局右侧。
    Right,
}

/// 停靠区：浮动模块丢到窗口边缘时的落点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockZone {
    /// 靠窗口左边缘 → 停到左槽。
    Left,
    /// 靠窗口右边缘 → 停到右槽。
    Right,
    /// 靠窗口上边缘 → 停到中央槽（顶栏之下）。
    Top,
    /// 靠窗口下边缘 → 停到中央槽。
    Bottom,
}

impl DockZone {
    /// 该停靠区对应的槽位。
    pub fn slot(self) -> Slot {
        match self {
            // 上下都归中央：竖直方向的停靠不改变左右分栏结构。
            DockZone::Left => Slot::Left,
            DockZone::Right => Slot::Right,
            DockZone::Top | DockZone::Bottom => Slot::Center,
        }
    }

    /// 该停靠区停靠后模块应占的高度比例。
    ///
    /// 上下停靠给 50%：占满整屏会把历史列表挤没，而用户
    /// 拖到上下边缘时想的是「我要一个独立的大视图」。
    pub fn height_fraction(self) -> f32 {
        match self {
            DockZone::Left | DockZone::Right => 1.0,
            DockZone::Top | DockZone::Bottom => 0.5,
        }
    }
}

/// 模块的放置方式与状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement {
    /// 停靠在主布局里。`collapsed` 为真时不占宽度（只留一条把手）。
    Docked {
        /// 停靠槽位。
        slot: Slot,
        /// 是否折叠。
        collapsed: bool,
    },
    /// 浮动在主布局之上。
    Floating {
        /// 屏幕矩形（逻辑点）。
        rect: Rect,
    },
}

impl Placement {
    /// 是否浮动。
    pub fn is_floating(&self) -> bool {
        matches!(self, Placement::Floating { .. })
    }

    /// 是否折叠。
    ///
    /// 浮动模块没有折叠态：折叠后再拖动会得到一个零尺寸的矩形，
    /// 之后无法再展开。折叠只对停靠模块有意义。
    pub fn is_collapsed(&self) -> bool {
        matches!(
            self,
            Placement::Docked {
                collapsed: true,
                ..
            }
        )
    }

    /// 浮动模块的矩形；非浮动返回 `None`。
    pub fn float_rect(&self) -> Option<Rect> {
        match self {
            Placement::Floating { rect } => Some(*rect),
            Placement::Docked { .. } => None,
        }
    }
}

/// 布局状态：可持久化的全部布局数据。
///
/// 只存**用户意图**（宽度、槽位、折叠、浮动位置），不存求解结果。
/// 求解结果每帧由 [`solve`] 从这份意图 + 当前窗口尺寸算出来，
/// 因此窗口 resize 后不需要迁移任何状态。
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutState {
    /// 每个模块的放置方式。
    pub placement: [Placement; 4],
    /// 拖动分隔条时的像素增量累加。
    ///
    /// 存的是「相对默认宽度的偏移」而非绝对宽度：窗口尺寸变化后
    /// 绝对宽度会失效，而偏移量能自动跟随。
    pub width_delta: [f32; 4],
    /// 右侧竖排标签选中的视图下标。
    pub rail_view: usize,
    /// 顶栏高度（逻辑点）。
    pub topbar_height: f32,
}

impl Default for LayoutState {
    fn default() -> Self {
        Self {
            placement: default_placement(),
            width_delta: [0.0; 4],
            rail_view: 0,
            topbar_height: 36.0,
        }
    }
}

/// 默认布局：历史居中，置顶在左，详情在右，侧栏折叠在最右。
fn default_placement() -> [Placement; 4] {
    let mut p = [Placement::Docked {
        slot: Slot::Center,
        collapsed: false,
    }; 4];
    p[panel_index(Panel::History)] = Placement::Docked {
        slot: Slot::Center,
        collapsed: false,
    };
    p[panel_index(Panel::Pinned)] = Placement::Docked {
        slot: Slot::Left,
        collapsed: false,
    };
    p[panel_index(Panel::Detail)] = Placement::Docked {
        slot: Slot::Right,
        collapsed: true,
    };
    p[panel_index(Panel::Rail)] = Placement::Docked {
        slot: Slot::Right,
        collapsed: true,
    };
    p
}

/// `Panel` 在数组里的下标。
///
/// `Panel::ALL` 的顺序是**稳定契约**：持久化按这个下标写，
/// 调整 `ALL` 会让旧配置里的数据错位。新增模块请追加到末尾。
pub fn panel_index(p: Panel) -> usize {
    match p {
        Panel::History => 0,
        Panel::Pinned => 1,
        Panel::Detail => 2,
        Panel::Rail => 3,
    }
}

/// 按下标取回 `Panel`。越界返回 `None`（不 panic，见 [`panel_index`]）。
pub fn panel_from_index(i: usize) -> Option<Panel> {
    Panel::ALL.get(i).copied()
}

/// 停靠模块在某档降级下的呈现方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// 正常停靠，参与宽度分配。
    Docked,
    /// 折叠：只留折叠把手宽度（侧栏是窄条，其余为 0）。
    Collapsed,
    /// 完全不出现（矩形为 `None`）。
    Hidden,
}

/// 降级阶梯的一档。
///
/// # 排序方向
///
/// `Ord` 派生使 `Full < DetailFloating < ... < HistoryOnly`，
/// 即**枚举值越大 = 越窄 = 让步越多**。[`tier_for`] 依赖这个方向做
/// 「从宽到窄找第一个装得下的档」的线性扫描。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// 全部停靠（三栏 + 侧栏窄条）。
    Full,
    /// 详情转为浮动浮层，不占主布局宽度。
    DetailFloating,
    /// 置顶栏折叠（只留把手宽度）。
    PinnedFolded,
    /// 侧栏窄条也隐藏。
    RailHidden,
    /// 只剩历史列表。
    HistoryOnly,
}

impl Tier {
    /// 由宽到窄的全部档位。
    pub const ALL: [Tier; 5] = [
        Tier::Full,
        Tier::DetailFloating,
        Tier::PinnedFolded,
        Tier::RailHidden,
        Tier::HistoryOnly,
    ];

    /// 档位名（用于日志与调试输出）。
    pub fn name(self) -> &'static str {
        match self {
            Tier::Full => "三栏全展开",
            Tier::DetailFloating => "详情转浮动",
            Tier::PinnedFolded => "置顶折叠",
            Tier::RailHidden => "侧栏隐藏",
            Tier::HistoryOnly => "仅历史列表",
        }
    }

    /// 该档下每个模块的呈现方式。
    ///
    /// **降级只作用于仍在默认槽位上的模块**：用户把某模块拖去了别的槽位，
    /// 说明他有自己的安排，此时替他折叠会与意图相悖。
    pub fn visibility(self, state: &LayoutState, panel: Panel) -> Visibility {
        let i = panel_index(panel);
        let Placement::Docked { slot, collapsed } = state.placement[i] else {
            // 浮动模块不参与降级：它本来就自己占位。
            return Visibility::Hidden;
        };
        if slot != panel.default_slot() {
            return Visibility::Docked;
        }
        // 每模块的降级动作在它**第一次让步**的那一档触发，之后各档保持。
        //
        // # 为什么档位表排在「用户是否折叠」之前
        //
        // 详情与侧栏的**默认态就是折叠**。若先判折叠，
        // 它们在所有档位上都返回 `Collapsed`，降级就再也拿不回那24/28 点——
        // 于是 `Tier::DetailFloating` 与 `Tier::Full` 的需求完全相同，
        // 这一级成了永远选不到的死档，阶梯实际只剩三级。
        //
        // 因此规则是：**降级优先，用户折叠次之**。
        // 用户折叠仍被尊重（Docked 的候选里再查一次 `collapsed`），
        // 只是降级动作本身不受它阻挡。
        let user_collapsed = collapsed;
        match (panel, self) {
            // 历史是主区，任何档位都不降级。
            (Panel::History, _) => Visibility::Docked,
            // 详情：转浮层（不是隐藏——浮层仍可达，见 `detail_is_floating`）。
            (Panel::Detail, Tier::Full) => {
                if user_collapsed {
                    Visibility::Collapsed
                } else {
                    Visibility::Docked
                }
            }
            (Panel::Detail, _) => Visibility::Hidden,
            // 置顶：先收成把手，再彻底隐藏。
            (Panel::Pinned, Tier::Full | Tier::DetailFloating) => {
                if user_collapsed {
                    Visibility::Collapsed
                } else {
                    Visibility::Docked
                }
            }
            (Panel::Pinned, Tier::PinnedFolded | Tier::RailHidden) => Visibility::Collapsed,
            (Panel::Pinned, Tier::HistoryOnly) => Visibility::Hidden,
            // 侧栏：最晚让步（它默认就折叠，展开宽度也只有 96）。
            (Panel::Rail, Tier::Full | Tier::DetailFloating | Tier::PinnedFolded) => {
                if user_collapsed {
                    Visibility::Collapsed
                } else {
                    Visibility::Docked
                }
            }
            (Panel::Rail, Tier::RailHidden | Tier::HistoryOnly) => Visibility::Hidden,
        }
    }

    /// 详情面板在该档下是否转成浮动浮层。
    ///
    /// 详情是**唯一一个「转浮动」而非「隐藏」**的模块：它的内容是当前选中
    /// 条目的补充信息，隐藏等于功能消失，而浮层仍可达（点条目会重新出现）。
    pub fn detail_is_floating(self, state: &LayoutState) -> bool {
        let i = panel_index(Panel::Detail);
        let Placement::Docked { slot, collapsed } = state.placement[i] else {
            return false;
        };
        if collapsed || slot != Panel::Detail.default_slot() {
            return false;
        }
        self >= Tier::DetailFloating
    }

    /// 该档需要的最小宽度（逻辑点）。
    ///
    /// **阈值由它算出来**，而不是另拍一组魔数：三段保底 + 分隔条。
    /// 这样改了 [`Panel::min_width`] 阶梯会自动跟着变，
    /// 不会出现「保底改了、阈值忘了改」的错位。
    pub fn min_required_width(self, state: &LayoutState) -> f32 {
        let mut total = 0.0;
        let mut n_splitters = 0usize;
        for slot in [Slot::Left, Slot::Center, Slot::Right] {
            let mut seg = 0.0;
            // 段内**占位**成员数：折叠成员也占位（把手），
            // 但宽度为 0 的不算——它不需要分隔条。
            let mut n_occupied = 0usize;
            for p in Panel::ALL {
                let i = panel_index(p);
                let Placement::Docked { slot: s, .. } = state.placement[i] else {
                    continue;
                };
                if s != slot {
                    continue;
                }
                let w = match self.visibility(state, p) {
                    Visibility::Docked => {
                        n_occupied += 1;
                        p.min_width()
                    }
                    Visibility::Collapsed => {
                        let w = p.collapsed_width();
                        if w > 0.0 {
                            n_occupied += 1;
                        }
                        w
                    }
                    Visibility::Hidden => 0.0,
                };
                seg += w;
            }
            n_splitters += n_occupied.saturating_sub(1);
            total += seg;
        }
        total + n_splitters as f32 * SPLITTER_WIDTH
    }
}

/// 按可用宽度选出降级档位。
///
/// # 选档规则
///
/// 从 [`Tier::Full`] 往下找**第一个装得下**的档：
///
/// ```text
/// Tier::Full.min_required_width() <= usable   → Full
/// Tier::DetailFloating.min_required_width() <= usable → DetailFloating
/// ...
/// ```
///
/// # 为什么这样选而不是「从窄往宽试装」
///
/// - **自动**：窗口一 resize 就重算，无需任何持久状态；
/// - **可逆**：窗口变宽时同一函数立刻回到更宽的档，用户拖出的浮窗位置不受影响；
/// - **不留半档**：不会出现「置顶折了一半、侧栏只露 3 像素」这类中间态——
///   档位是离散的，每一档的所有模块都处在明确的「展开/折叠/隐藏」状态。
///
/// # 装不下时返回 [`Tier::HistoryOnly`]
///
/// 那时历史列表会拿到 `body.width()` 全部宽度（可能低于它的保底），
/// 但**绝不溢出**：分配的最后一步是无条件的硬钳制。
pub fn tier_for(state: &LayoutState, body_width: f32) -> Tier {
    for tier in Tier::ALL {
        if tier.min_required_width(state) <= body_width {
            return tier;
        }
    }
    Tier::HistoryOnly
}

/// 求解结果：把布局意图变成具体矩形。
///
/// **绘制层只读这份结果，不重算**。这是「提示与判定同源」的前提。
#[derive(Debug, Clone, PartialEq)]
pub struct Solved {
    /// 每个模块的最终矩形；已折叠的停靠模块为 `None`。
    ///
    /// 用 `None` 而非零宽矩形：零宽矩形画出来就是一条线，
    /// 而「这个模块此刻不占地方」与「它占了一个零宽的地方」
    /// 在后续判定里会走不同分支。
    pub rects: [Option<Rect>; 4],
    /// 顶栏矩形（搜索框 + 设置）。
    pub topbar: Rect,
    /// 分隔条矩形。
    ///
    /// 用 `Vec` 而非定长数组：4 个模块最多产生 3 条**段内**分隔条，
    /// 加上「左|中」「中|右」两条**段间**分隔条，上界是 5。
    /// 定长 3 的数组会在「四个模块全堆进中央槽」这种用户可达的布局下
    /// 静默丢掉两条——交互层于是找不到能拖的分隔条。
    pub splitters: Vec<Rect>,
    /// 分隔条分隔的模块对，与 [`Solved::splitters`] 一一对应。
    ///
    /// 绘制层拿到分隔条矩形后需要知道「拖它会改变哪两个模块的宽度」，
    /// 这个反查必须与矩形同源产出，不能各算各的。
    pub splitter_pairs: Vec<(Panel, Panel)>,
    /// 浮动模块的矩形。
    pub floating: [Option<Rect>; 4],
    /// 停靠区预览（拖动时命中某区才有值）。
    pub dock_preview: Option<(DockZone, Rect)>,
    /// 本帧生效的降级档位。
    ///
    /// 绘制层据此决定是否画「窄窗口提示」这类档位相关内容——
    /// 档位是**算出来的**，绘制层读它即可，不自己重算宽度。
    pub tier: Tier,
}

/// 窗口可用区域（去掉边框缩放区后的客户区）。
///
/// `dragging` 是当前正被拖动的模块矩形（浮动拖动中传入，其余时候 `None`）。
/// 它只影响 [`Solved::dock_preview`]：停靠区预览**只在拖动时**出现，
/// 否则用户会一直看到一个「看起来像停靠好了」的框，实际模块还浮在别处。
pub fn solve(area: Rect, state: &LayoutState, dragging: Option<Rect>) -> Solved {
    let topbar = Rect::from_min_size(area.min, vec2(area.width(), state.topbar_height));
    let body = Rect::from_min_max(
        egui::pos2(area.min.x, area.min.y + state.topbar_height),
        area.max,
    );

    let mut out = Solved {
        rects: [None; 4],
        topbar,
        splitters: Vec::new(),
        splitter_pairs: Vec::new(),
        floating: [None; 4],
        dock_preview: None,
        tier: Tier::HistoryOnly,
    };

    // ---- 降级：先定档，再谈宽度 ----
    //
    // 顺序很重要：**先**根据可用宽度决定哪些模块参与主布局，
    // **再**在剩下的模块之间分配宽度。反过来做（先分配、再发现装不下）
    // 就是旧版的 bug：分配出的宽度已经溢出，裁剪只会在段与段之间
    // 造成重叠，而不是让某个模块「让位」。
    let tier = tier_for(state, body.width());
    out.tier = tier;

    // ---- 浮动模块：直接用自己存的矩形，钳到窗口内 ----
    for p in Panel::ALL {
        let i = panel_index(p);
        if let Placement::Floating { rect } = state.placement[i] {
            out.floating[i] = Some(clamp_rect(rect, body, MIN_FLOAT_SIZE));
        }
    }
    // 详情在窄窗口下自动转浮层：给它一个「贴着右边缘、居中 vertically」
    // 的矩形。位置每次都从 `body` 现算，因此窗口变大后档位回到 Full，
    // 它自然回到右段——用户不需要做任何撤销操作。
    if tier.detail_is_floating(state) {
        let i = panel_index(Panel::Detail);
        let w = MIN_DETAIL_WIDTH.min(body.width()).max(MIN_FLOAT_SIZE.x.min(body.width()));
        let h = (body.height() * 0.8).max(MIN_FLOAT_SIZE.y).min(body.height());
        out.floating[i] = Some(Rect::from_min_size(
            egui::pos2(
                (body.max.x - w).max(body.min.x),
                body.min.y + (body.height() - h) / 2.0,
            ),
            vec2(w, h),
        ));
    }

    // ---- 停靠模块：按槽位分组 ----
    // 槽内按 Panel::ALL 顺序排，保证同一槽位的相对次序不随拖动而乱。
    //
    // ⚠️ `docked` 必须按**同一套可见性判据**过滤。旧版这里只过滤
    // 「非浮动」，而分配宽度时用的是「非折叠」的成员数，两个集合大小不同，
    // 于是 `splitter_total` 多算了分隔条，`usable` 偏小，
    // 三段宽度之和必然超出 `body.width()`。
    let docked: Vec<Panel> = Panel::ALL
        .iter()
        .copied()
        .filter(|p| {
            !state.placement[panel_index(*p)].is_floating()
                && tier.visibility(state, *p) != Visibility::Hidden
        })
        .collect();

    let mut groups: Vec<(Slot, Vec<Panel>)> = vec![
        (Slot::Left, vec![]),
        (Slot::Center, vec![]),
        (Slot::Right, vec![]),
    ];
    for p in docked.iter().copied() {
        let slot = match state.placement[panel_index(p)] {
            Placement::Docked { slot, .. } => slot,
            Placement::Floating { .. } => continue,
        };
        if let Some(g) = groups.iter_mut().find(|(s, _)| *s == slot) {
            g.1.push(p);
        }
    }

    // 分隔条总数：**只数真正占位的相邻对**。
    //
    // ⚠️ 必须按**同一套可见性判据**数。旧版用 `docked.len() - 1`，
    // 而 `docked` 当时只过滤了「非浮动」——把 `Hidden`（被降级隐藏）
    // 和宽度为 0 的折叠成员也算进去了，`usable` 因此被多扣几个
    // `SPLITTER_WIDTH`，三段之和必然超出 `body.width()`。
    //
    // 这里数的是「每段内占位成员的相邻对」，与下面真正画出的分隔条一一对应。
    let n_splitters: usize = [&groups[0].1, &groups[1].1, &groups[2].1]
        .iter()
        .map(|members| {
            let n_occupied = members
                .iter()
                .filter(|p| match tier.visibility(state, **p) {
                    Visibility::Docked => true,
                    Visibility::Collapsed => p.collapsed_width() > 0.0,
                    Visibility::Hidden => false,
                })
                .count();
            n_occupied.saturating_sub(1)
        })
        .sum::<usize>()
        // **段间**分隔条：「左|中」「中|右」两条。
        //
        // 漏掉它们会让三段**首尾相接**（中间没有 6 逻辑点的缝），
        // 于是相邻两段的边界只靠浮点凑巧对上；一旦某段宽度
        // 经过 `shrink_to_fit` 的按比例缩放，误差就会让两段
        // 真的重叠 1e-5 —— 实测在 308 逻辑点、用户把置顶拖到右槽时触发。
        + {
            // 某段「占位」= 它至少有一个宽度 > 0 的成员。
            let occupied = |members: &[Panel]| {
                members.iter().any(|p| match tier.visibility(state, *p) {
                    Visibility::Docked => true,
                    Visibility::Collapsed => p.collapsed_width() > 0.0,
                    Visibility::Hidden => false,
                })
            };
            usize::from(occupied(&groups[0].1) && occupied(&groups[1].1))
                + usize::from(occupied(&groups[1].1) && occupied(&groups[2].1))
        };
    let splitter_total = n_splitters as f32 * SPLITTER_WIDTH;
    let usable = body.width() - splitter_total;

    // 三段分配。`allocate` 保证 `left + center + right <= usable`。
    let seg = allocate(
        usable,
        &groups[0].1,
        &groups[1].1,
        &groups[2].1,
        state,
        tier,
    );
    let left_w = seg.left;
    let right_w = seg.right;
    let center_w = seg.center;

    let mut pairs: Vec<(Panel, Panel)> = Vec::new();
    // 三段的**起点各自独立**：左段贴左边界、右段贴右边界、
    // 中央段填两者之间（并给两条段间分隔条留出缝）。
    //
    // ⚠️ 旧版用一个共享的游标 `x` 依次摆放左段与右段，
    // 于是右段被排在**左段后面**而不是右边界处，紧接着中央段又从
    // `body.min.x + left_w` 开始画——右段与中央区 100% 重叠。
    // 这是「所有元素挤在一起」的第二条根因，与宽度溢出叠加后
    // 表现为整片糊在窗口左上角。修法就是给每段自己的起点。
    let occupied = |members: &[Panel]| {
        members.iter().any(|p| match tier.visibility(state, *p) {
            Visibility::Docked => true,
            Visibility::Collapsed => p.collapsed_width() > 0.0,
            Visibility::Hidden => false,
        })
    };
    // 段间分隔条占掉的缝：左段右边界到中央段左边界之间。
    let gap_left_center = if occupied(&groups[0].1) && occupied(&groups[1].1) {
        SPLITTER_WIDTH
    } else {
        0.0
    };
    let gap_center_right = if occupied(&groups[1].1) && occupied(&groups[2].1) {
        SPLITTER_WIDTH
    } else {
        0.0
    };
    let seg_origin = [
        body.min.x,                                        // 左段
        body.min.x + left_w + gap_left_center,            // 中央段
        body.max.x - right_w,                             // 右段
    ];

    // 段内分隔条：按成员逐个画。
    for (gi, members) in [&groups[0].1, &groups[2].1].into_iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let gi = if gi == 0 { 0usize } else { 2 };
        let total = if gi == 0 { left_w } else { right_w };
        let widths = split_within(total, members, state, tier);
        let mut x = seg_origin[gi];
        for (k, p) in members.iter().enumerate() {
            let i = panel_index(*p);
            let w = widths[k];
            out.rects[i] = match tier.visibility(state, *p) {
                Visibility::Hidden => None,
                // 折叠态：留一条把手供点击展开。宽度用**实际分配到的**
                // `w`（可能已被缩放压到 0），而不是固定的
                // `collapsed_width()`——否则极窄窗口下会画出
                // 一条越出客户区的把手。
                Visibility::Collapsed if w > 0.0 => Some(Rect::from_min_size(
                    egui::pos2(x, body.min.y),
                    vec2(w, body.height()),
                )),
                Visibility::Collapsed => None,
                Visibility::Docked => Some(Rect::from_min_size(
                    egui::pos2(x, body.min.y),
                    vec2(w, body.height()),
                )),
            };
            x += w;
            // 段内分隔条只在「下一个成员存在**且**占位」时画。
            if widths.get(k + 1).is_some_and(|v| *v > 0.0) {
                out.splitters.push(Rect::from_min_size(
                    egui::pos2(x, body.min.y),
                    vec2(SPLITTER_WIDTH, body.height()),
                ));
                pairs.push((*p, members[k + 1]));
                x += SPLITTER_WIDTH;
            }
        }
    }

    // 中央段起点：左段总宽之后。
    let center_x = seg_origin[1];
    let center_members = &groups[1].1;
    if center_members.is_empty() {
        // 中央空着也要给主区一个可点击的底板，否则窗口中央是死区。
        // 宽度用 `center_w` 而不是「到 body.max」——否则左段把中央区
        // 推出右边界时，这块底板会跟着越界。
        //
        // ⚠️ **仅当历史确实没被停到别的段时**才补这块底板。
        // 用户可能把历史拖到左槽（`dock(History, Slot::Left)`），
        // 此时中央段为空，历史已经在左段有矩形；若这里无条件写
        // `rects[History]`，会把它**覆盖**成中央段那块底板，
        // 于是历史与左段里的其他模块（如侧栏）重叠。
        if center_w > 0.0 && out.rects[panel_index(Panel::History)].is_none() {
            out.rects[panel_index(Panel::History)] = Some(Rect::from_min_size(
                egui::pos2(center_x, body.min.y),
                vec2(center_w, body.height()),
            ));
        }
    } else {
        let widths = split_within(center_w, center_members, state, tier);
        let mut cx = center_x;
        for (k, p) in center_members.iter().enumerate() {
            let i = panel_index(*p);
            let w = widths[k];
            out.rects[i] = match tier.visibility(state, *p) {
                Visibility::Hidden => None,
                Visibility::Collapsed if w > 0.0 => Some(Rect::from_min_size(
                    egui::pos2(cx, body.min.y),
                    vec2(w, body.height()),
                )),
                Visibility::Collapsed => None,
                Visibility::Docked => Some(Rect::from_min_size(
                    egui::pos2(cx, body.min.y),
                    vec2(w, body.height()),
                )),
            };
            cx += w;
            if widths.get(k + 1).is_some_and(|v| *v > 0.0) {
                out.splitters.push(Rect::from_min_size(
                    egui::pos2(cx, body.min.y),
                    vec2(SPLITTER_WIDTH, body.height()),
                ));
                pairs.push((*p, center_members[k + 1]));
                cx += SPLITTER_WIDTH;
            }
        }
    }

    // ---- 段间分隔条：左|中、中|右 ----
    //
    // 这两条不是可选装饰：它们的宽度已经从 `usable` 里扣掉了
    // （见上面 `n_splitters` 的段间部分），不画就等于凭空少了两条缝，
    // 相邻两段会贴在一起。
    //
    // 模块对取「左段最后一个占位成员」与「中央段第一个占位成员」。
    // 段内分隔条同样登记进 `splitter_pairs`，交互层因此能对
    // 任意一条分隔条反查它影响哪两个模块。
    let last_occupied = |members: &[Panel], widths: &[f32]| {
        members
            .iter()
            .enumerate()
            .rev()
            .find(|(k, _)| widths.get(*k).is_some_and(|w| *w > 0.0))
            .map(|(k, p)| (*p, widths[k]))
    };
    let first_occupied = |members: &[Panel], widths: &[f32]| {
        members
            .iter()
            .enumerate()
            .find(|(k, _)| widths.get(*k).is_some_and(|w| *w > 0.0))
            .map(|(k, p)| (*p, widths[k]))
    };
    if gap_left_center > 0.0 {
        // 宽度列表需重算一次：段内宽度在上面已算过，但这里只需要
        // 「最后一个/第一个占位成员是谁」，重新算代价可忽略，
        // 而缓存两份宽度列表容易在改动后不同步。
        let lw = split_within(left_w, &groups[0].1, state, tier);
        let cw = split_within(center_w, &groups[1].1, state, tier);
        if let (Some((lp, _)), Some((cp, _))) = (
            last_occupied(&groups[0].1, &lw),
            first_occupied(&groups[1].1, &cw),
        ) {
            out.splitters.push(Rect::from_min_size(
                egui::pos2(seg_origin[1] - gap_left_center, body.min.y),
                vec2(gap_left_center, body.height()),
            ));
            pairs.push((lp, cp));
        }
    }
    if gap_center_right > 0.0 {
        let cw = split_within(center_w, &groups[1].1, state, tier);
        let rw = split_within(right_w, &groups[2].1, state, tier);
        if let (Some((cp, _)), Some((rp, _))) = (
            last_occupied(&groups[1].1, &cw),
            first_occupied(&groups[2].1, &rw),
        ) {
            out.splitters.push(Rect::from_min_size(
                egui::pos2(seg_origin[2] - gap_center_right, body.min.y),
                vec2(gap_center_right, body.height()),
            ));
            pairs.push((cp, rp));
        }
    }
    out.splitter_pairs = pairs;

    // 停靠区预览：仅在拖动中且确实命中某个停靠区时出现。
    if let Some(moving) = dragging
        && let Some(zone) = dock_zone_of(Some(moving), body)
    {
        let slot = zone.slot();
        let w = moving
            .width()
            .min(center_w.max(MIN_FLOAT_SIZE.x))
            .max(MIN_DOCK_WIDTH)
            .min(body.width());
        let h = (body.height() * zone.height_fraction())
            .max(MIN_FLOAT_SIZE.y)
            .min(body.height());
        let (x, y) = match slot {
            Slot::Left => (body.min.x, body.min.y),
            Slot::Right => (body.max.x - w, body.min.y),
            _ => (
                center_x + (center_w - w) / 2.0,
                body.min.y + (body.height() - h) / 2.0,
            ),
        };
        out.dock_preview = Some((zone, Rect::from_min_size(egui::pos2(x, y), vec2(w, h))));
    }

    out
}

/// 一段的保底宽度：**各成员保底之和 + 段内分隔条**。
///
/// # 为什么必须把分隔条算进保底
///
/// 保底是「这一段至少要拿到多少宽度」。段内若有2 个占位成员，
/// 它们之间还有一条 [`SPLITTER_WIDTH`] 的分隔条也要从这一段里出。
/// 早前版本只累加成员保底、漏掉分隔条，于是分配函数认为该段
/// 「保底 = 24 + 28 = 52」已经够，实际画的时候还要再扣 6，
/// 结果是折叠把手被压到 21.5 / 25.1——**比它自己的名义宽度还窄**，
/// 侧栏那条「表/密」竖标签因此被裁掉。
fn segment_floor(members: &[Panel], state: &LayoutState, tier: Tier) -> f32 {
    let mut total = 0.0;
    let mut n_occupied = 0usize;
    for p in members {
        let w = match tier.visibility(state, *p) {
            Visibility::Docked => p.min_width(),
            Visibility::Collapsed => p.collapsed_width(),
            Visibility::Hidden => 0.0,
        };
        if w > 0.0 {
            n_occupied += 1;
        }
        total += w;
    }
    total + n_occupied.saturating_sub(1) as f32 * SPLITTER_WIDTH
}

/// 旧版 `slot_width` 里那句无效钳制的注解，保留在此处作为**回归警示**。
///
/// 原文：
/// ```text
/// (target + delta).max(floor).min(usable.max(floor))
/// ```
///
/// `floor > usable` 时 `usable.max(floor) == floor`，于是 `.min(floor)`
/// 恰好把上一行的 `.max(floor)` 又抬了回去——**钳制是空操作**，
/// 返回值仍然 `>= floor > usable`。三段各自如此，`usable` 装不下就被撑破，
/// 中央区被推出右边界，矩形互相重叠。
///
/// 现在这一职责由 [`allocate`] 承担：它先把两段保底预留出来，
/// 再用 `clamp` 把每段硬钳在剩余额度内——上界来自额度而不是来自 `floor`，
/// 因此不存在「上下界互相抵消」的可能。
/// 分隔条不再由本函数单独预留（易与段内预留重复计算），
/// 而是由 [`segment_floor`] 统一计入，`allocate` 与 [`Tier::min_required_width`]
/// 共用同一个口径。

/// 三段宽度的分配结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segments {
    /// 左段总宽（含其内部分隔条）。
    pub left: f32,
    /// 中央段总宽。
    pub center: f32,
    /// 右段总宽（含其内部分隔条）。
    pub right: f32,
}

/// 把 `usable` 分给三段，返回的三个值之和**恒`<= usable`**。
///
/// # 分配顺序（这个顺序就是不变式的证明）
///
/// 1. **先给中央段留保底**：左右两段的额度上限 = `usable - 中央段保底`。
///    于是左右两段加起来不可能超过这个额度，中央段必然拿得到保底——
///    前提是窗口装得下所有保底（[`tier_for`] 已保证）。
/// 2. 左段在自己的额度里按 [`LEFT_FRACTION`] 取宽。
/// 3. 右段在**左段用掉之后的剩余额度**里按 [`RIGHT_FRACTION`] 取宽。
///    两段额度之和 = `usable - 中央保底`，因此三段之和 `<= usable`。
/// 4. 窗口比所有保底之和还窄时，额度为 0，左右两段让位，
///    中央段拿全部 `usable`——它低于保底，但**不重叠**。
///
/// # 为什么不「先分完再裁剪」
///
/// 裁剪矩形不会减少对宽度的**需求**，只会让矩形彼此压盖。
/// 需求必须在分配阶段就让位（降级），这是本函数与旧版的根本区别。
pub fn allocate(
    usable: f32,
    left_members: &[Panel],
    center_members: &[Panel],
    right_members: &[Panel],
    state: &LayoutState,
    tier: Tier,
) -> Segments {
    let usable = usable.max(0.0);
    let center_floor = segment_floor(center_members, state, tier);
    let side_budget = (usable - center_floor).max(0.0);

    // ⚠️ **先把两段的保底都预留出来，再分配富余**。
    //
    // 早前版本是「左段先按自己的期望宽（200）取，取完再给右段剩下的」，
    // 于是左段的期望宽会把右段的保底吃掉：实测在 400 逻辑点下
    // 右段只拿到 34，而它两个折叠把手需要 24 + 28 + 6(分隔条) = 58，
    // `split_within` 只好按比例缩放，把侧栏窄条压到 12.9 ——
    // 窄于它自己的名义宽度，「表/密」竖标签被裁掉。
    //
    // 正确顺序：保底先分（谁都拿够），富余再按比例分（谁好看谁多拿）。
    let left_floor = segment_floor(left_members, state, tier);
    let right_floor = segment_floor(right_members, state, tier);
    let floors = left_floor + right_floor;

    let (mut left, mut right) = if floors <= side_budget {
        // **注水法（water-filling）**：先按保底起步，富余按「谁还没到舒适宽度」
        // 轮流给，直到两段都达到期望宽或富余用完。
        //
        // 为什么不是简单按比例：比例分配在「一段已到期望宽、另一段还没到」
        // 时会把宽度硬塞给已经够宽的那一段。早前版本正是这样——
        // 默认布局下右段两个成员都折叠（不再需要更宽），
        // 左段却按比例吃掉了几乎全部富余（120 → 576），
        // 而中央段只剩保底 160，比左段还窄。
        //
        // 注水结束后剩下的富余**自动流向中央段**（中央段吃 `usable` 的余额），
        // 因此不需要在这里显式分配。
        let mut surplus = side_budget - floors;
        let mut l = left_floor;
        let mut r = right_floor;
        let mut l_slack = (slot_desired(left_members, state, tier) - left_floor).max(0.0);
        let mut r_slack = (slot_desired(right_members, state, tier) - right_floor).max(0.0);
        // 每轮给「当前缺口 / 需求最大」的那一段，或各给一半。
        loop {
            if surplus <= 0.0 || (l_slack <= 0.0 && r_slack <= 0.0) {
                break;
            }
            let (to_left, to_right) = match (l_slack > 0.0, r_slack > 0.0) {
                (true, true) => {
                    if l_slack >= r_slack {
                        (surplus.min(l_slack), 0.0)
                    } else {
                        (0.0, surplus.min(r_slack))
                    }
                }
                (true, false) => (surplus.min(l_slack), 0.0),
                (false, true) => (0.0, surplus.min(r_slack)),
                (false, false) => break,
            };
            l += to_left;
            r += to_right;
            l_slack -= to_left;
            r_slack -= to_right;
            surplus -= to_left + to_right;
        }
        // 富余还有剩：**全部留给中央段**，不再按保底比例硬塞给两侧。
        //
        // 中央段是主区，它没有「期望宽」上限（默认宽 320 只是起点，
        // 窗口越宽它越宽）。早前版本把剩余按保底比例分给两侧，
        // 于是宽窗口下左段 120 → 576、中央段只剩 160，
        // 主区反而比侧边的置顶栏还窄——「历史区应比侧栏宽」这条
        // 直觉被破坏，且没有任何报错。
        //
        // 这里直接 `break`：`l` / `r` 停在各自的期望宽，
        // 剩下的宽度由下面的 `center = usable - left - right` 自然接手。
        let _ = surplus;
        (l, r)
    } else {
        // 两段保底之和都超出预算：按保底比例回收（此时必然降级到只剩历史，
        // 这条分支主要是兜底，让函数在任何输入下都返回合法值）。
        let k = if floors > 0.0 { side_budget / floors } else { 0.0 };
        (left_floor * k, right_floor * k)
    };

    // 叠加用户拖分隔条产生的偏移，并**硬钳制在剩余额度内**。
    // 偏移可能为负（往回拖），也可能是用户拖出来的巨大正值；
    // 两者都不能破坏「三段之和 <= usable」这条不变式。
    let dl = left_delta(left_members, state);
    let dr = right_delta(right_members, state);
    left = (left + dl).clamp(0.0, side_budget);
    right = (right + dr).clamp(0.0, (side_budget - left).max(0.0));

    // 中央段吃剩下的。`left + right <= side_budget = usable - center_floor`，
    // 所以这里恒为 `>= center_floor >= 0`。
    let center = (usable - left - right).max(0.0);

    debug_assert!(
        left + center + right <= usable + 0.01,
        "三段之和 {} 超出可用宽度 {usable}",
        left + center + right
    );
    Segments { left, center, right }
}

/// 一段的「期望宽度」：保底 + 各展开成员从保底到默认宽的差额。
///
/// 与 [`segment_floor`] 同一口径（含段内分隔条），
/// 两者相减才是「这一段还能舒服地长多少」。
fn slot_desired(members: &[Panel], state: &LayoutState, tier: Tier) -> f32 {
    segment_floor(members, state, tier)
        + members
            .iter()
            .map(|p| match tier.visibility(state, *p) {
                Visibility::Docked => p.default_width() - p.min_width(),
                _ => 0.0,
            })
            .sum::<f32>()
}

/// 一段内所有成员的宽度偏移之和（拖分隔条产生的）。
fn left_delta(members: &[Panel], state: &LayoutState) -> f32 {
    members
        .iter()
        .map(|p| state.width_delta[panel_index(*p)])
        .sum()
}

/// 右段版的 [`left_delta`]。命名只为对称，不承载语义。
fn right_delta(members: &[Panel], state: &LayoutState) -> f32 {
    left_delta(members, state)
}

/// 一段内各成员的占位宽度。
///
/// 折叠成员按 [`Panel::collapsed_width`] 单独占位，
/// 剩余宽度由**展开成员**均分——旧版按 `members.len()` 除，
/// 折叠成员白占了除数里一个名额，把展开成员压窄了三分之一。
///
/// ⚠️ **所有宽度之和 + 段内分隔条恒`<= total`**。
/// 这条约束不能只靠 `allocate` 保证：折叠成员的把手宽度是**固定值**，
/// 若这一段被分配到的 `total` 比它还小（极窄窗口、或与折叠成员同段的
/// 展开成员被降级隐藏），直接用它就会撑破段宽、越出客户区。
/// 早前版本正是这样在宽0时画出了一条 24 逻辑点宽的把手。
/// 因此这里在最后做一次**按比例缩放**，把总和压回 `total`。
fn split_within(total: f32, members: &[Panel], state: &LayoutState, tier: Tier) -> Vec<f32> {
    // `None` 标记「待均分的展开成员」。
    let widths: Vec<Option<f32>> = members
        .iter()
        .map(|p| match tier.visibility(state, *p) {
            Visibility::Docked => None,
            Visibility::Collapsed => Some(p.collapsed_width()),
            Visibility::Hidden => Some(0.0),
        })
        .collect();
    let n_open = widths.iter().filter(|w| w.is_none()).count();

    // 段内分隔条：占位成员数 - 1（零宽成员不算占位）。
    //
    // ⚠️ 它必须先从 `total` 里扣掉，再分配给成员。早前版本把分隔条
    // 只在计算 `each` 时减掉、却没算进最终的硬钳制上限，
    // 于是「成员宽度和 == total」但再加一条 6 逻辑点的分隔条就超出了
    // 段的额度——右段因此越过 `body.max.x`。
    let n_occupied = widths.iter().filter(|w| w.is_some_and(|v| v > 0.0)).count();
    let inner_splitters = n_occupied.saturating_sub(1) as f32 * SPLITTER_WIDTH;
    // 成员可用的总宽（已扣掉分隔条）。
    let budget = (total - inner_splitters).max(0.0);

    if n_open == 0 {
        let mut out: Vec<f32> = widths.iter().map(|w| w.unwrap_or(0.0)).collect();
        shrink_to_fit(&mut out, budget);
        return out;
    }

    // ⚠️ 均分前先看**每个展开成员能否拿到自己的保底**。
    //
    // 均分是「大家一样多」的最简做法，但段内成员需求差别很大：
    // 详情要 180、侧栏展开只要 88。若段宽 400，两段平分各 197 ——
    // 看着谁都不亏；但段宽 355 时平分各 174，**详情就低于它的保底**，
    // 里面的动作按钮（「粘贴到前台窗口」）会折行。
    //
    // 因此改为**带保底的注水**：先给每个成员它的保底，
    // 富余再按「谁离舒适宽更近」轮流给。
    // 段的保底（[`segment_floor`]）已经保证 `budget >= Σ保底`，
    // 所以这里保底一定给得起；给不起时退化为按比例分（`share_cheap`）。
    let mut alloc: Vec<f32> = widths
        .iter()
        .map(|w| match w {
            None => 0.0,
            Some(v) => *v,
        })
        .collect();
    let open_idx: Vec<usize> = widths
        .iter()
        .enumerate()
        .filter_map(|(k, w)| w.is_none().then_some(k))
        .collect();

    let min_of = |k: usize| match tier.visibility(state, members[k]) {
        Visibility::Docked => members[k].min_width(),
        _ => 0.0,
    };
    let comfy_of = |k: usize| match tier.visibility(state, members[k]) {
        Visibility::Docked => members[k].default_width(),
        _ => 0.0,
    };

    let base: f32 = alloc.iter().sum();
    if base >= budget {
        // 连保底都放不下（极窄，或有折叠把手占位）：按保底比例分。
        let k = if base > 0.0 { budget / base } else { 0.0 };
        for v in alloc.iter_mut() {
            *v *= k;
        }
        return alloc;
    }
    for &k in &open_idx {
        alloc[k] = min_of(k).min(budget);
    }
    let mut surplus = (budget - alloc.iter().sum::<f32>()).max(0.0);
    // 注水：每轮给「当前缺口 / 舒适宽」最大的那个成员。
    loop {
        if surplus <= 0.0 {
            break;
        }
        // 找「相对缺口」最大的成员：`(comfy - alloc) / comfy`。
        let mut best: Option<(usize, f32)> = None;
        for &k in &open_idx {
            let comfy = comfy_of(k);
            if comfy <= 0.0 {
                continue;
            }
            let gap = ((comfy - alloc[k]) / comfy).max(0.0);
            if gap <= 0.0 {
                continue;
            }
            if best.is_none_or(|(_, bg)| gap > bg) {
                best = Some((k, gap));
            }
        }
        let Some((k, gap)) = best else { break };
        // 至少给一点，避免 gap 极小时进展过慢；上限是它的舒适宽。
        let room = comfy_of(k) - alloc[k];
        let step = room.min(surplus).max(gap * comfy_of(k)).min(surplus);
        if step <= 0.0 {
            break;
        }
        alloc[k] += step;
        surplus -= step;
    }
    // 富余还有剩（都到舒适宽了）：按舒适宽比例分。
    let total_comfy: f32 = open_idx.iter().map(|&k| comfy_of(k)).sum();
    if surplus > 0.0 && total_comfy > 0.0 {
        for &k in &open_idx {
            let share = surplus * (comfy_of(k) / total_comfy);
            alloc[k] += share;
        }
    }
    // 浮点与累加误差的最后一轮硬钳制。
    let s: f32 = alloc.iter().sum();
    if s > budget && s > 0.0 {
        let f = budget / s;
        for v in alloc.iter_mut() {
            *v *= f;
        }
    }
    alloc
}

/// 把宽度列表按比例缩放到「和 `<= cap`」，且不产生负宽度。
///
/// 全部为 0 时原样返回（除零保护）。
fn shrink_to_fit(widths: &mut [f32], cap: f32) {
    let sum: f32 = widths.iter().sum();
    let cap = cap.max(0.0);
    if sum <= cap {
        return;
    }
    if sum <= 0.0 {
        widths.fill(0.0);
        return;
    }
    let k = cap / sum;
    for w in widths.iter_mut() {
        *w = (*w * k).max(0.0);
    }
}

// ---------------------------------------------------------------------------
// 吸附
// ---------------------------------------------------------------------------

/// 吸附结果。
///
/// **同时携带最终矩形与提示线**，绘制层两者都画，不做二次计算——
/// 这是「提示与判定同源」的实现方式。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snap {
    /// 吸附到的目标槽位。
    pub slot: Slot,
    /// 吸附后模块应占的矩形。
    pub rect: Rect,
    /// 要画的提示线（竖线或横线）。
    pub guide: Guide,
}

/// 吸附提示线。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Guide {
    /// 竖直提示线，`x` 为线位置，`y0..y1` 为覆盖范围。
    Vertical {
        /// 线的横坐标。
        x: f32,
        /// 线段上端。
        y0: f32,
        /// 线段下端。
        y1: f32,
    },
    /// 水平提示线。
    Horizontal {
        /// 线的纵坐标。
        y: f32,
        /// 线段左端。
        x0: f32,
        /// 线段右端。
        x1: f32,
    },
}

impl Guide {
    /// 画成 2 逻辑点宽的线段所需的矩形。
    pub fn rect(&self) -> Rect {
        const T: f32 = 2.0;
        match *self {
            Guide::Vertical { x, y0, y1 } => Rect::from_min_max(egui::pos2(x - T / 2.0, y0), egui::pos2(x + T / 2.0, y1)),
            Guide::Horizontal { y, x0, x1 } => Rect::from_min_max(egui::pos2(x0, y - T / 2.0), egui::pos2(x1, y + T / 2.0)),
        }
    }
}

/// 判定浮动模块应吸附到哪个槽位。
///
/// # 判定顺序
///
/// 1. 先看是否命中停靠区（靠窗口边缘）——停靠区优先于模块间吸附：
///    用户的动作意图是「放到屏幕边上」，比「靠近某个模块」更强。
/// 2. 否则看与左右相邻停靠模块边缘的距离，在 [`SNAP_THRESHOLD`] 内则吸附。
///
/// # 为什么不算「与所有模块两两比较」
///
/// 只与**当前停靠布局的槽边界**比较：吸附的目标是槽位，不是某个模块。
/// 与具体模块比较会让吸附结果依赖该模块的宽度，用户拖同一个模块到
/// 同样位置却因上次谁被拖过而落在不同地方。
pub fn snap(
    moving: Rect,
    body: Rect,
    threshold: f32,
) -> Option<Snap> {
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return None;
    }
    // 1. 停靠区
    if let Some(zone) = dock_zone_of(Some(moving), body) {
        let slot = zone.slot();
        let rect = docked_rect_for(slot, moving, body);
        // 停靠区提示画在窗口对应那条边。
        let guide = match zone {
            DockZone::Left => Guide::Vertical {
                x: body.min.x,
                y0: body.min.y,
                y1: body.max.y,
            },
            DockZone::Right => Guide::Vertical {
                x: body.max.x,
                y0: body.min.y,
                y1: body.max.y,
            },
            DockZone::Top => Guide::Horizontal {
                y: body.min.y,
                x0: body.min.x,
                x1: body.max.x,
            },
            DockZone::Bottom => Guide::Horizontal {
                y: body.max.y,
                x0: body.min.x,
                x1: body.max.x,
            },
        };
        return Some(Snap { slot, rect, guide });
    }

    // 2. 槽边界吸附：浮动模块左右边缘接近槽边界时吸上去。
    let x0 = body.min.x;
    let x2 = body.max.x;
    let mut best: Option<(f32, Snap)> = None;
    for (edge_x, slot) in [(x0, Slot::Left), (x2, Slot::Right)] {
        let d = (moving.min.x - edge_x).abs().min((moving.max.x - edge_x).abs());
        if d > threshold {
            continue;
        }
        let w = moving.width().min(body.width() - 2.0 * SPLITTER_WIDTH).max(MIN_DOCK_WIDTH);
        let rect = match slot {
            Slot::Left => Rect::from_min_max(
                egui::pos2(edge_x, body.min.y),
                egui::pos2(edge_x + w, body.max.y),
            ),
            Slot::Right => Rect::from_min_max(
                egui::pos2(edge_x - w, body.min.y),
                egui::pos2(edge_x, body.max.y),
            ),
            Slot::Center => continue,
        };
        let guide = Guide::Vertical {
            x: edge_x,
            y0: body.min.y,
            y1: body.max.y,
        };
        let cand = Snap { slot, rect, guide };
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, s)| s)
}

/// 停靠区判定：`rect` 的中心进入窗口边缘阈值内即命中。
///
/// 用**中心点**而非任意一点：模块很大时它天然覆盖整条边，
/// 按中心判定才能区分「拖到边上」与「只是恰好很大」。
pub fn dock_zone_of(rect: Option<Rect>, body: Rect) -> Option<DockZone> {
    let r = rect?;
    let c = r.center();
    let t = DOCK_ZONE_THRESHOLD;
    // 先判左右：用户拖到侧边比拖到上下边更常见，
    // 且左右同时满足时靠边更符合直觉。
    if c.x <= body.min.x + t {
        return Some(DockZone::Left);
    }
    if c.x >= body.max.x - t {
        return Some(DockZone::Right);
    }
    if c.y <= body.min.y + t {
        return Some(DockZone::Top);
    }
    if c.y >= body.max.y - t {
        return Some(DockZone::Bottom);
    }
    None
}

/// 停靠到某槽位后应占的矩形。
fn docked_rect_for(slot: Slot, moving: Rect, body: Rect) -> Rect {
    let w = moving.width().clamp(MIN_DOCK_WIDTH, body.width());
    let h = (body.height() * 0.5).max(MIN_FLOAT_SIZE.y).min(body.height());
    match slot {
        Slot::Left => {
            Rect::from_min_max(egui::pos2(body.min.x, body.min.y), egui::pos2(body.min.x + w, body.max.y))
        }
        Slot::Right => Rect::from_min_max(
            egui::pos2(body.max.x - w, body.min.y),
            egui::pos2(body.max.x, body.max.y),
        ),
        Slot::Center => {
            let w = w.min(body.width());
            let x = body.min.x + (body.width() - w) / 2.0;
            let y = body.min.y + (body.height() - h) / 2.0;
            Rect::from_min_size(egui::pos2(x, y), vec2(w, h))
        }
    }
}

// ---------------------------------------------------------------------------
// 拖动与钳制
// ---------------------------------------------------------------------------

/// 把矩形钳进 `bounds`，并保证不小于 `min_size`。
///
/// `min_size` 在钳制**之后**生效：窗口比最小尺寸还小时，
/// 宁可让模块超出窗口（用户能拖回来），也不给一个看不见的模块。
pub fn clamp_rect(r: Rect, bounds: Rect, min_size: Vec2) -> Rect {
    let w = r.width().max(min_size.x);
    let h = r.height().max(min_size.y);
    let mut x = r.min.x;
    let mut y = r.min.y;
    if w < bounds.width() {
        x = x.clamp(bounds.min.x, bounds.max.x - w);
    }
    if h < bounds.height() {
        y = y.clamp(bounds.min.y, bounds.max.y - h);
    }
    Rect::from_min_size(egui::pos2(x, y), vec2(w, h))
}

/// 把浮动模块移动 `delta`。
pub fn drag_float(rect: Rect, delta: Vec2, body: Rect) -> Rect {
    let moved = rect.translate(delta);
    clamp_rect(moved, body, MIN_FLOAT_SIZE)
}

/// 拖动分隔条：返回更新后的宽度增量。
///
/// `delta_x` 为正表示分隔条右移，左侧模块变宽。返回**相对增量的变化量**，
/// 调用方累加到 [`LayoutState::width_delta`]——存相对量而非绝对宽度，
/// 这样窗口尺寸变化后布局自动跟随。
pub fn drag_splitter(
    state: &mut LayoutState,
    pair: (Panel, Panel),
    delta_x: f32,
    body_width: f32,
) {
    let (left, right) = pair;
    let li = panel_index(left);
    let ri = panel_index(right);
    let cap = (body_width * 0.5).max(MIN_DOCK_WIDTH);
    let left_slot = match state.placement[li] {
        Placement::Docked { slot, .. } => slot,
        Placement::Floating { .. } => return,
    };
    let right_slot = match state.placement[ri] {
        Placement::Docked { slot, .. } => slot,
        Placement::Floating { .. } => return,
    };
    // **用带符号的 `delta_x`，不取绝对值。**
    //
    // 分隔条右移 `dx` → 左侧变宽 `dx`、右侧变窄 `dx`；反向拖动则相反。
    // 两侧一增一减，总宽守恒，且天然可逆。
    //
    // 早前版本先取绝对值再按槽位判断加减，反向拖动时符号丢失，
    // 表现为「往回拖宽度只增不减」。
    let dx = delta_x;
    if left_slot == Slot::Center && matches!(right_slot, Slot::Right | Slot::Center) {
        // 中央段是「剩余空间」段：它变宽意味着两侧都要让出空间，
        // 因此只改中央段的偏移，两侧不动。
        state.width_delta[li] = (state.width_delta[li] + dx).clamp(-cap, cap);
    } else {
        state.width_delta[li] = (state.width_delta[li] + dx).clamp(-cap, cap);
        state.width_delta[ri] = (state.width_delta[ri] - dx).clamp(-cap, cap);
    }
}

/// 两个矩形是否相交（有正面积重叠）。
///
/// 用于「浮动模块是否互相重叠」这类判定。相切（边贴边）不算相交：
/// 两个模块并排贴在一起是正常布局，不是冲突。
pub fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.min.x < b.max.x && b.min.x < a.max.x && a.min.y < b.max.y && b.min.y < a.max.y
}

/// 命中测试：`pos` 是否落在 `rect` 内。
pub fn hit(rect: Rect, pos: egui::Pos2) -> bool {
    rect.contains(pos)
}

/// 把模块从浮动改为停靠。
pub fn dock(state: &mut LayoutState, panel: Panel, slot: Slot) {
    let i = panel_index(panel);
    state.placement[i] = Placement::Docked {
        slot,
        // 浮动过的模块停靠后默认展开：用户刚拖出来看过内容，
        // 收回去时不该是折叠的。
        collapsed: false,
    };
    state.width_delta[i] = 0.0;
}

/// 把模块变为浮动。
pub fn float(state: &mut LayoutState, panel: Panel, rect: Rect) {
    let i = panel_index(panel);
    state.placement[i] = Placement::Floating { rect };
}

/// 折叠/展开一个停靠模块。
///
/// 浮动模块不响应：折叠后再拖动得到零尺寸矩形，之后无法展开。
pub fn toggle_collapse(state: &mut LayoutState, panel: Panel) {
    let i = panel_index(panel);
    if let Placement::Docked { collapsed, .. } = &mut state.placement[i] {
        *collapsed = !*collapsed;
    }
}

/// 模块是否处于折叠态。
pub fn is_collapsed(state: &LayoutState, panel: Panel) -> bool {
    state.placement[panel_index(panel)].is_collapsed()
}

/// 找出分隔条反查的模块对。
///
/// 绘制层拿到分隔条矩形后需要知道「拖动它会改变哪两个模块」，
/// 这个反查必须在同一处完成，不能各画各的。
pub fn splitter_pair(solved: &Solved, splitter: Rect) -> Option<(Panel, Panel)> {
    solved
        .splitters
        .iter()
        .position(|r| *r == splitter)
        // `splitters` 与 `splitter_pairs` 由 `solve` 同源产出、长度一致，
        // 因此这里不需要跳过「未使用的槽位」—— 定长数组时代留下的
        // `Rect::ZERO` 占位已经不存在了（改成 `Vec` 之后长度即真实条数）。
        //
        // 仍保留零宽过滤作为防御：万一将来某条分隔条被算出宽度 0，
        // 交互层不该去拖它。
        .filter(|k| solved.splitters[*k].width() > 0.0)
        .and_then(|k| solved.splitter_pairs.get(k).copied())
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

/// 布局状态的磁盘表示由 core crate 定义。
///
/// 数据结构放在 [`modular_clipboard_core::LayoutConfig`] 而不是这里：
/// 它是**用户配置的一部分**，必须与 `UiConfig` 的其余字段同处一个 crate
/// （`Config` 由 core 定义，ui 反过来依赖 core）。留在 ui 会让
/// `UiConfig` 无法持有它——要么 core 依赖 ui 形成循环，要么布局存不进配置。
///
/// 转换函数则留在这里：`LayoutState` 用的是 egui 的 `Rect`，
/// core 不认识它，`From<&LayoutState>` 无法在 core 侧实现。
pub use modular_clipboard_core::LayoutConfig;

impl From<&LayoutState> for LayoutConfig {
    fn from(s: &LayoutState) -> Self {
        let mut modules = [[0.0f32; 7]; 4];
        for p in Panel::ALL {
            let i = panel_index(p);
            let (float, slot, collapsed, r) = match s.placement[i] {
                Placement::Docked { slot, collapsed } => (0.0, slot_code(slot), f32::from(collapsed), Rect::ZERO),
                Placement::Floating { rect } => (1.0, slot_code(Slot::Center), 0.0, rect),
            };
            modules[i] = [float, slot, collapsed, r.min.x, r.min.y, r.width(), r.height()];
        }
        Self {
            modules,
            width_delta: s.width_delta,
            rail_view: s.rail_view,
            topbar_height: s.topbar_height,
            version: Self::VERSION,
        }
    }
}

fn slot_code(s: Slot) -> f32 {
    match s {
        Slot::Left => 0.0,
        Slot::Center => 1.0,
        Slot::Right => 2.0,
    }
}

fn slot_from_code(c: f32) -> Slot {
    // 越界值（配置文件被手改）落回中央槽，不 panic：
    // 布局容错优先于严格校验，用户不该因为一个数字开不了应用。
    if c < 0.5 {
        Slot::Left
    } else if c < 1.5 {
        Slot::Center
    } else {
        Slot::Right
    }
}

impl From<&LayoutConfig> for LayoutState {
    fn from(c: &LayoutConfig) -> Self {
        let mut placement = [Placement::Docked {
            slot: Slot::Center,
            collapsed: false,
        }; 4];
        for p in Panel::ALL {
            let i = panel_index(p);
            let m = c.modules[i];
            placement[i] = if m[0] > 0.5 {
                Placement::Floating {
                    rect: Rect::from_min_max(
                        egui::pos2(m[3], m[4]),
                        egui::pos2(m[3] + m[5], m[4] + m[6]),
                    ),
                }
            } else {
                Placement::Docked {
                    slot: slot_from_code(m[1]),
                    collapsed: m[2] > 0.5,
                }
            };
        }
        Self {
            placement,
            width_delta: c.width_delta,
            // 下标越界时落回第一个视图，而不是 panic 或留空。
            rail_view: c.rail_view.min(2),
            topbar_height: if c.topbar_height.is_finite() && c.topbar_height > 0.0 {
                c.topbar_height
            } else {
                36.0
            },
        }
    }
}

/// 默认布局配置。
pub fn default_config() -> LayoutConfig {
    LayoutConfig::from(&LayoutState::default())
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::pos2(x, y), vec2(w, h))
    }

    fn body() -> Rect {
        r(0.0, 36.0, 800.0, 500.0)
    }

    // ---- 吸附判定 ----

    #[test]
    fn snaps_to_left_zone_when_center_near_left_edge() {
        let b = body();
        // 中心落在 x <= 48 → 左停靠区
        let moving = r(0.0, 200.0, 100.0, 80.0);
        let s = snap(moving, b, SNAP_THRESHOLD).expect("应命中左停靠区");
        assert_eq!(s.slot, Slot::Left);
    }

    #[test]
    fn snaps_to_right_zone_when_center_near_right_edge() {
        let b = body();
        let moving = r(760.0, 200.0, 40.0, 80.0);
        let s = snap(moving, b, SNAP_THRESHOLD).expect("应命中右停靠区");
        assert_eq!(s.slot, Slot::Right);
    }

    #[test]
    fn snaps_to_top_and_bottom_zones() {
        let b = body();
        // 顶部：中心 y <= 36 + 48
        let top = r(300.0, 30.0, 100.0, 80.0);
        assert_eq!(
            snap(top, b, SNAP_THRESHOLD).map(|s| s.slot),
            Some(Slot::Center)
        );
        // 底部：中心 y >= 536 - 48
        let bottom = r(300.0, 470.0, 100.0, 80.0);
        assert_eq!(
            snap(bottom, b, SNAP_THRESHOLD).map(|s| s.slot),
            Some(Slot::Center)
        );
    }

    #[test]
    fn no_snap_in_window_middle() {
        let b = body();
        // 悬在正中：既不靠边也不靠槽边界
        let moving = r(350.0, 200.0, 100.0, 80.0);
        assert_eq!(snap(moving, b, SNAP_THRESHOLD), None);
    }

    #[test]
    fn guide_x_matches_snapped_rect_edge() {
        // 判据同源：提示线的位置必须就是吸附后矩形的边。
        let b = body();
        let moving = r(0.0, 200.0, 100.0, 80.0);
        let s = snap(moving, b, SNAP_THRESHOLD).unwrap();
        match s.guide {
            Guide::Vertical { x, .. } => {
                assert!(
                    (s.rect.min.x - x).abs() < 0.001 || (s.rect.max.x - x).abs() < 0.001,
                    "提示线 x={x} 必须落在吸附矩形 {:?} 的某条边上",
                    s.rect
                );
            }
            other => panic!("左停靠区应给竖线，实际 {other:?}"),
        }
    }

    #[test]
    fn dock_zone_precedence_over_slot_snap() {
        // 同时靠近窗口左边缘与槽边界时，停靠区必须赢。
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(0.0, 300.0, 120.0, 90.0));
        let b = body();
        let moving = r(0.0, 300.0, 120.0, 90.0);
        let s = snap(moving, b, SNAP_THRESHOLD).unwrap();
        assert_eq!(s.slot, Slot::Left);
    }

    // ---- 停靠区 ----

    #[test]
    fn dock_zone_none_in_center() {
        let b = body();
        assert_eq!(dock_zone_of(Some(r(300.0, 200.0, 100.0, 100.0)), b), None);
    }

    #[test]
    fn dock_zone_none_for_absent_rect() {
        assert_eq!(dock_zone_of(None, body()), None);
    }

    #[test]
    fn large_rect_uses_center_not_edge() {
        // 大模块中心在中间就不该被判成靠边——用中心判定正是为此。
        let b = body();
        let big = r(-500.0, 36.0, 2000.0, 300.0);
        assert_eq!(dock_zone_of(Some(big), b), None);
    }

    #[test]
    fn left_wins_over_top_when_corner() {
        let b = body();
        // 中心同时满足左右与上下阈值 → 靠边优先
        let near_corner = r(0.0, 40.0, 20.0, 20.0);
        assert_eq!(dock_zone_of(Some(near_corner), b), Some(DockZone::Left));
    }

    #[test]
    fn zone_maps_to_expected_slot() {
        assert_eq!(DockZone::Left.slot(), Slot::Left);
        assert_eq!(DockZone::Right.slot(), Slot::Right);
        assert_eq!(DockZone::Top.slot(), Slot::Center);
        assert_eq!(DockZone::Bottom.slot(), Slot::Center);
        assert!((DockZone::Top.height_fraction() - 0.5).abs() < f32::EPSILON);
        assert!((DockZone::Left.height_fraction() - 1.0).abs() < f32::EPSILON);
    }

    // ---- 碰撞 ----

    #[test]
    fn overlap_detected_for_crossing_rects() {
        assert!(rects_overlap(r(0.0, 0.0, 100.0, 100.0), r(50.0, 50.0, 100.0, 100.0)));
    }

    #[test]
    fn touching_rects_do_not_overlap() {
        // 相切不算冲突：并排是正常布局。
        assert!(!rects_overlap(r(0.0, 0.0, 100.0, 100.0), r(100.0, 0.0, 100.0, 100.0)));
    }

    #[test]
    fn contained_rect_overlaps() {
        assert!(rects_overlap(r(0.0, 0.0, 100.0, 100.0), r(10.0, 10.0, 10.0, 10.0)));
    }

    #[test]
    fn separated_rects_do_not_overlap() {
        assert!(!rects_overlap(r(0.0, 0.0, 10.0, 10.0), r(50.0, 50.0, 10.0, 10.0)));
    }

    // ---- 钳制 ----

    #[test]
    fn clamp_pulls_rect_into_bounds() {
        let b = r(0.0, 0.0, 400.0, 300.0);
        let out = clamp_rect(r(-50.0, -20.0, 100.0, 100.0), b, MIN_FLOAT_SIZE);
        assert!(b.contains(out.min), "钳制后左上角应在界内: {out:?}");
        assert!(out.max.x <= b.max.x && out.max.y <= b.max.y, "越界: {out:?}");
    }

    #[test]
    fn clamp_enforces_min_size() {
        let b = r(0.0, 0.0, 400.0, 300.0);
        let out = clamp_rect(r(10.0, 10.0, 5.0, 5.0), b, MIN_FLOAT_SIZE);
        assert!(out.width() >= MIN_FLOAT_SIZE.x, "宽度应抬到最小值: {out:?}");
        assert!(out.height() >= MIN_FLOAT_SIZE.y, "高度应抬到最小值: {out:?}");
    }

    #[test]
    fn clamp_lets_oversized_rect_escape_bounds() {
        // 窗口比最小尺寸还小时，宁可越界也不能给看不见的模块。
        let tiny = r(0.0, 0.0, 80.0, 40.0);
        let out = clamp_rect(r(0.0, 0.0, 10.0, 10.0), tiny, MIN_FLOAT_SIZE);
        assert!(out.width() >= MIN_FLOAT_SIZE.x);
        assert!(out.height() >= MIN_FLOAT_SIZE.y);
    }

    #[test]
    fn drag_float_moves_by_delta() {
        let b = r(0.0, 0.0, 800.0, 600.0);
        let out = drag_float(r(100.0, 100.0, 100.0, 100.0), vec2(10.0, 5.0), b);
        assert!((out.min.x - 110.0).abs() < 0.001);
        assert!((out.min.y - 105.0).abs() < 0.001);
    }

    #[test]
    fn drag_float_clamps_at_right_edge() {
        let b = r(0.0, 0.0, 800.0, 600.0);
        let out = drag_float(r(700.0, 100.0, 100.0, 100.0), vec2(500.0, 0.0), b);
        assert!(out.max.x <= b.max.x + 0.001, "应被钳回窗口内: {out:?}");
    }

    // ---- 分隔条 ----

    #[test]
    fn splitter_drag_widens_left_panel() {
        let mut st = LayoutState::default();
        let before = st.width_delta[panel_index(Panel::Pinned)];
        drag_splitter(&mut st, (Panel::Pinned, Panel::Detail), 20.0, 800.0);
        assert!(
            st.width_delta[panel_index(Panel::Pinned)] > before,
            "分隔条右移应让左面板变宽"
        );
    }

    #[test]
    fn splitter_drag_is_reversible() {
        let mut st = LayoutState::default();
        drag_splitter(&mut st, (Panel::Pinned, Panel::Detail), 20.0, 800.0);
        let mid = st.width_delta[panel_index(Panel::Pinned)];
        drag_splitter(&mut st, (Panel::Pinned, Panel::Detail), -20.0, 800.0);
        assert!(
            (st.width_delta[panel_index(Panel::Pinned)] - mid + 20.0).abs() < 0.001,
            "反向拖动应把偏移退回去"
        );
    }

    #[test]
    fn splitter_drag_ignores_floating_panels() {
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(0.0, 0.0, 100.0, 100.0));
        let before = st.width_delta[panel_index(Panel::Pinned)];
        drag_splitter(&mut st, (Panel::Pinned, Panel::Detail), 20.0, 800.0);
        assert_eq!(st.width_delta[panel_index(Panel::Pinned)], before, "浮动面板不参与分隔条");
    }

    #[test]
    fn splitter_drag_is_capped() {
        let mut st = LayoutState::default();
        for _ in 0..100 {
            drag_splitter(&mut st, (Panel::Pinned, Panel::Detail), 100.0, 800.0);
        }
        assert!(
            st.width_delta[panel_index(Panel::Pinned)] <= 400.0 + f32::EPSILON,
            "拖动应被上限钳住，实际 {}",
            st.width_delta[panel_index(Panel::Pinned)]
        );
    }

    // ---- 折叠 / 停靠 / 浮动 ----

    #[test]
    fn toggle_collapse_flips_docked_panel() {
        let mut st = LayoutState::default();
        let p = Panel::History;
        let was = is_collapsed(&st, p);
        toggle_collapse(&mut st, p);
        assert_ne!(is_collapsed(&st, p), was);
    }

    #[test]
    fn toggle_collapse_ignores_floating_panel() {
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(0.0, 0.0, 200.0, 200.0));
        let before = st.placement[panel_index(Panel::Detail)];
        toggle_collapse(&mut st, Panel::Detail);
        assert_eq!(st.placement[panel_index(Panel::Detail)], before, "浮动模块不该被折叠");
    }

    #[test]
    fn dock_resets_collapsed_and_delta() {
        let mut st = LayoutState::default();
        toggle_collapse(&mut st, Panel::Detail);
        st.width_delta[panel_index(Panel::Detail)] = 55.0;
        dock(&mut st, Panel::Detail, Slot::Right);
        assert!(!is_collapsed(&st, Panel::Detail), "停靠后应展开");
        assert_eq!(st.width_delta[panel_index(Panel::Detail)], 0.0, "停靠后宽度偏移应清零");
    }

    #[test]
    fn solve_places_floating_panel_at_stored_rect() {
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(300.0, 200.0, 220.0, 180.0));
        let s = solve(body(), &st, None);
        let r = s.floating[panel_index(Panel::Detail)].expect("浮动模块应有矩形");
        assert!((r.min.x - 300.0).abs() < 0.001, "位置应保持: {r:?}");
    }

    #[test]
    fn solve_marks_collapsed_panel_none() {
        let st = LayoutState::default();
        let s = solve(body(), &st, None);
        // 默认详情折叠 → 留一条把手（不是 None）。
        //
        // ⚠️ 契约变更（本次布局重写）：折叠从「完全不占位」改为
        // 「收成把手」。旧断言要求 `None`，那时折叠等于消失，
        // 用户既看不到也点不回来——而`Visibility::Collapsed` 与
        // `Visibility::Hidden` 在求解结果里原本无法区分，
        // 降级阶梯也就少一级。
        // 折叠矩形宽度不超过 [`Panel::collapsed_width`]，
        // 且**必须落在客户区内**（极窄窗口下会是 `None`）。
        match s.rects[panel_index(Panel::Detail)] {
            Some(r) => {
                assert!(
                    r.width() <= Panel::Detail.collapsed_width() + 0.01,
                    "折叠把手宽度 {} 超过把手宽度",
                    r.width()
                );
                assert!(
                    r.max.x <= body().max.x + 0.01,
                    "折叠把手越出客户区: {r:?}"
                );
            }
            None => {
                // 只允许在窗口窄到连把手都放不下时为 None。
                assert!(
                    body().width() < Panel::Detail.collapsed_width(),
                    "宽窗口下折叠的详情不该没有矩形"
                );
            }
        }
    }

    #[test]
    fn solve_keeps_rail_visible_when_collapsed() {
        let st = LayoutState::default();
        let s = solve(body(), &st, None);
        let r = s.rects[panel_index(Panel::Rail)];
        assert!(r.is_some(), "侧栏折叠后仍应留窄条供点击展开");
        if let Some(r) = r {
            assert!((r.width() - RAIL_COLLAPSED_WIDTH).abs() < 0.001);
        }
    }

    #[test]
    fn solve_gives_history_the_largest_area() {
        let st = LayoutState::default();
        let s = solve(body(), &st, None);
        let h = s.rects[panel_index(Panel::History)].expect("历史区应有矩形");
        let p = s.rects[panel_index(Panel::Pinned)].expect("置顶区应有矩形");
        assert!(h.width() > p.width(), "历史区应比侧栏宽: {h:?} vs {p:?}");
    }

    #[test]
    fn solve_survives_zero_area() {
        let st = LayoutState::default();
        // 零尺寸窗口不应 panic
        let _ = solve(Rect::ZERO, &st, None);
    }

    #[test]
    fn splitter_pair_lookup_matches_rect() {
        let st = LayoutState::default();
        let s = solve(body(), &st, None);
        // 找到第一个有效分隔条，用它的矩形反查模块对，应当命中同一对。
        let found = s
            .splitters
            .iter()
            .enumerate()
            .find(|(k, r)| r.width() > 0.0 && s.splitter_pairs[*k] != (Panel::History, Panel::History));
        if let Some((k, sp)) = found {
            assert_eq!(splitter_pair(&s, *sp), Some(s.splitter_pairs[k]));
        }
        // 空矩形不应命中任何分隔条。
        assert_eq!(splitter_pair(&s, Rect::ZERO), None);
    }

    // ---- 持久化往返 ----

    #[test]
    fn config_roundtrip_preserves_layout() {
        let mut st = LayoutState::default();
        st.rail_view = 2;
        st.topbar_height = 44.0;
        st.width_delta = [1.0, -2.0, 3.0, -4.0];
        float(&mut st, Panel::Detail, r(120.0, 140.0, 260.0, 200.0));
        toggle_collapse(&mut st, Panel::Pinned);
        dock(&mut st, Panel::Rail, Slot::Left);

        let cfg = LayoutConfig::from(&st);
        let back: LayoutState = (&cfg).into();
        assert_eq!(st, back, "布局 → 配置 → 布局 必须还原");
    }

    #[test]
    fn config_survives_json_text() {
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(12.0, 34.0, 200.0, 100.0));
        let cfg = LayoutConfig::from(&st);
        let text = serde_json::to_string(&cfg).expect("序列化应成功");
        let parsed: LayoutConfig = serde_json::from_str(&text).expect("反序列化应成功");
        let back: LayoutState = (&parsed).into();
        assert_eq!(st, back, "经过 JSON 文本后布局不变");
    }

    #[test]
    fn config_version_is_set() {
        let cfg = default_config();
        assert_eq!(cfg.version, LayoutConfig::VERSION);
    }

    #[test]
    fn rail_view_out_of_range_clamps() {
        let mut cfg = default_config();
        cfg.rail_view = 99;
        let st: LayoutState = (&cfg).into();
        assert!(st.rail_view <= 2, "越界下标应被钳住: {}", st.rail_view);
    }

    #[test]
    fn bad_topbar_height_falls_back() {
        let mut cfg = default_config();
        cfg.topbar_height = f32::NAN;
        let st: LayoutState = (&cfg).into();
        assert!(st.topbar_height.is_finite() && st.topbar_height > 0.0);
    }

    #[test]
    fn floating_survives_dock_roundtrip_position() {
        let mut st = LayoutState::default();
        let rect = r(150.0, 180.0, 300.0, 220.0);
        float(&mut st, Panel::Pinned, rect);
        // 浮动模块经 配置往返 后位置不变。
        let cfg = LayoutConfig::from(&st);
        let st2: LayoutState = (&cfg).into();
        assert_eq!(
            st2.placement[panel_index(Panel::Pinned)].float_rect(),
            Some(rect),
            "浮动位置应逐字保留"
        );
        // 停靠会清掉浮动位置，这是有意的：折叠态无位置可记。
        let mut st3 = st2;
        dock(&mut st3, Panel::Pinned, Slot::Center);
        assert_eq!(st3.placement[panel_index(Panel::Pinned)].float_rect(), None);
    }

    // ---- 索引契约 ----

    #[test]
    fn panel_index_roundtrips() {
        for p in Panel::ALL {
            assert_eq!(panel_from_index(panel_index(p)), Some(p));
        }
    }

    #[test]
    fn panel_index_out_of_range_returns_none() {
        assert_eq!(panel_from_index(4), None);
        assert_eq!(panel_from_index(99), None);
    }

    #[test]
    fn panel_all_covers_every_panel() {
        // 防漏：新增 Panel 变体时若忘了加进 ALL，这里会失败。
        let mut all = Panel::ALL.to_vec();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), Panel::ALL.len(), "Panel::ALL 不该有重复项");
        assert_eq!(all.len(), 4, "Panel::ALL 应包含全部 4 个模块");
    }

    /// 真实 exe 的默认窗口是 420x560（逻辑点还要再除以 1.5DPI），
    /// 也就是**比本文档其它测试用的 800 宽小一半**。窄窗口下
    /// 三段分栏最容易挤成负宽度，因此单独固定住。
    #[test]
    fn narrow_window_never_yields_negative_width() {
        let st = LayoutState::default();
        for w in [200.0f32, 320.0, 420.0, 560.0, 800.0, 1600.0] {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 480.0));
            let s = solve(area, &st, None);
            for p in Panel::ALL {
                if let Some(r) = s.rects[panel_index(p)] {
                    assert!(
                        r.width() >= 0.0 && r.height() >= 0.0,
                        "宽{w} 下 {p:?} 矩形为负: {r:?}"
                    );
                }
            }
            for sp in s.splitters {
                assert!(sp.width() >= 0.0, "宽{w} 下分隔条为负: {sp:?}");
            }
        }
    }

    #[test]
    fn narrow_window_keeps_panels_inside_viewport() {
        let st = LayoutState::default();
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 480.0));
        let s = solve(area, &st, None);
        for p in Panel::ALL {
            if let Some(r) = s.rects[panel_index(p)] {
                assert!(
                    r.max.x <= area.max.x + 0.01,
                    "宽420 下 {p:?} 溢出右边界: {r:?} vs {area:?}"
                );
            }
        }
    }

    #[test]
    fn hit_uses_half_open_rect() {
        let rct = r(0.0, 0.0, 100.0, 100.0);
        assert!(hit(rct, egui::pos2(50.0, 50.0)));
        assert!(!hit(rct, egui::pos2(150.0, 50.0)));
    }

    #[test]
    fn guide_rect_is_thin() {
        let g = Guide::Vertical {
            x: 100.0,
            y0: 0.0,
            y1: 200.0,
        };
        let rr = g.rect();
        assert!(rr.width() <= 2.0, "竖线提示应很窄: {rr:?}");
        assert!((rr.height() - 200.0).abs() < 0.001);
    }

    // ---- 跨 crate 往返：布局 → UiConfig → 磁盘 → UiConfig → 布局 ----

    /// 用户真正走的路径：布局 → `UiConfig` → `Config::save` →
    /// `Config::load` → 布局，**布局必须不变**。
    ///
    /// 这条比 `config_survives_json_text` 更外一层：它同时验证
    /// 「`LayoutConfig` 真的挂在 `UiConfig.layout` 上」与「core 侧的
    /// serde 派生没漏」——只测 `LayoutConfig` 自己的 serde 的话，
    /// 有人把 `UiConfig.layout` 的 `#[serde(default)]` 删掉（或整个
    /// 字段删掉）测试仍会通过，而真实用户会在升级后**丢掉全部布局**。
    #[test]
    fn layout_survives_full_config_roundtrip() {
        let mut st = LayoutState::default();
        float(&mut st, Panel::Detail, r(30.0, 60.0, 260.0, 180.0));
        st.width_delta[panel_index(Panel::History)] = 42.0;
        st.rail_view = 1;

        // 存
        let dir = std::env::temp_dir().join("tiez-layout-full-rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.ui.layout = Some(LayoutConfig::from(&st));
        cfg.save(&path).expect("保存配置应成功");

        // 取
        let loaded = modular_clipboard_core::Config::load(&path).expect("读取配置应成功");
        let got = loaded.ui.layout.expect("布局应被存下来");
        let back: LayoutState = (&got).into();

        assert_eq!(st, back, "经过完整配置往返后布局必须不变");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `UiConfig.layout == None` 时必须落回 `LayoutState::default()`，
    /// 而不是「零值布局」（那会让所有模块堆在中央槽）。
    #[test]
    fn absent_layout_config_yields_default_state() {
        let cfg = modular_clipboard_core::Config::default();
        assert!(cfg.ui.layout.is_none(), "默认配置不应带布局覆盖");
        // 上层拿到 None 时的约定：直接用 LayoutState::default()。
        let st = LayoutState::default();
        let round: LayoutState = (&LayoutConfig::from(&st)).into();
        assert_eq!(st, round, "默认布局自洽");
    }

    /// `LayoutConfig::empty()` **不是**默认布局，且默认没实现 `Default`。
    ///
    /// 用测试固定这条约定：`empty()` 全零，误用会让所有模块挤进中央槽。
    /// 这里断言它与真正的默认布局**不等**，让误用在测试期就暴露。
    /// 占位的折叠模块**不得窄于它自己的把手宽度**。
///
/// 这条性质在「窗口刚好装下这一档」时最容易被破坏：
/// 分配函数给的段宽若漏算了段内分隔条，[`split_within`] 就会
/// 再扣一次，把把手压到名义宽度以下——实测曾在 640 物理像素
/// （426 逻辑点）下把侧栏窄条画成 25.1 而不是 28，
/// 「表/密」竖标签因此被裁掉。
///
/// 只在「该段宽度 >= 成员保底之和 + 段内分隔条」时断言，
/// 因为低于这个宽度连名义值都给不出，此时让位是正确行为。
#[test]
fn collapsed_handles_keep_their_nominal_width_when_affordable() {
    let mut st = LayoutState::default();
    for w in [
        333.0f32, 360.0, 400.0, 426.7, 500.0, 600.0, 800.0, 1200.0, 2000.0,
    ] {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
        let s = solve(area, &st, None);
        for p in Panel::ALL {
            if s.tier.visibility(&st, p) != Visibility::Collapsed {
                continue;
            }
            let nominal = p.collapsed_width();
            let r = match s.rects[panel_index(p)] {
                Some(r) => r,
                None => continue,
            };
            // 该模块所在段是否宽到能给它名义宽度？
            let affordable = segment_floor(&[p], &st, s.tier) <= nominal + 0.01
                && nominal <= s.tier.min_required_width(&st)
                && w >= s.tier.min_required_width(&st);
            if affordable {
                assert!(
                    r.width() >= nominal - 0.01,
                    "宽 {w}（档位 {:?}）下 {p:?} 的把手只有 {:.1}，窄于名义 {nominal}",
                    s.tier,
                    r.width()
                );
            }
        }
    }
    // 让详情与侧栏都展开，验证展开档位下三段都拿到保底。
    toggle_collapse(&mut st, Panel::Detail);
    toggle_collapse(&mut st, Panel::Rail);
    for w in [400.0f32, 500.0, 700.0, 1200.0] {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
        let s = solve(area, &st, None);
        let req = s.tier.min_required_width(&st);
        if w < req {
            continue;
        }
        for p in Panel::ALL {
            if s.tier.visibility(&st, p) != Visibility::Docked {
                continue;
            }
            let r = s.rects[panel_index(p)].expect("展开模块必须有矩形");
            assert!(
                r.width() >= p.min_width() - 0.01,
                "宽 {w}（档位 {:?}）下 {p:?} 只有 {:.1}，低于保底 {}",
                s.tier,
                r.width(),
                p.min_width()
            );
        }
    }
}

#[test]
    fn empty_layout_is_not_the_default_layout() {
        assert_ne!(
            LayoutConfig::empty(),
            default_config(),
            "empty() 被误当成了默认布局"
        );
    }

    // ---- 降级阶梯与宽度分配不变式 ----

    /// **核心不变式**：任何宽度下，画出来的矩形都不重叠、总宽不超出客户区。
    ///
    /// 这条测试直接调[`solve`]（被测对象本体），不复现任何分配逻辑——
    /// 早前项目吃过两次「假测试」的亏：测试里自己写了一份分配，
    /// 于是真实代码改坏了测试照样通过。这里读的就是 `solve` 的输出。
    #[test]
    fn solved_rects_never_overlap_at_any_width() {
        let st = LayoutState::default();
        // 覆盖「极窄 → 超宽」，并刻意跨过每一档降级阈值。
        let widths = [
            0.0f32, 1.0, 12.0, 24.0, 48.0, 56.0, 100.0, 120.0, 159.0, 160.0, 161.0, 186.7, 200.0,
            240.0, 280.0, 300.0, 307.0, 308.0, 309.0, 320.0, 400.0, 420.0, 480.0, 560.0, 600.0,
            720.0, 800.0, 1000.0, 1280.0, 1600.0, 2400.0,
        ];
        for w in widths {
            // 顶部 36留作标题栏，body 高度给足（高度不是本次的变量）。
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
            let s = solve(area, &st, None);
            let body_w = w;
            let mut occupied: Vec<(Panel, Rect)> = Vec::new();
            for p in Panel::ALL {
                if let Some(r) = s.rects[panel_index(p)] {
                    occupied.push((p, r));
                }
            }
            // 1) 每个矩形都在body 内，且宽高非负。
            for (p, r) in &occupied {
                assert!(
                    r.width() >= 0.0 && r.height() >= 0.0,
                    "宽 {w} 下 {p:?} 矩形尺寸为负: {r:?}"
                );
                assert!(
                    r.max.x <= body_w + 0.01 && r.min.x >= -0.01,
                    "宽 {w} 下 {p:?} 越出客户区: {r:?}（body 宽 {body_w}）"
                );
            }
            // 2) 任意两个停靠矩形不重叠（有正面积交）。
            for i in 0..occupied.len() {
                for j in (i + 1)..occupied.len() {
                    let (pa, ra) = occupied[i];
                    let (pb, rb) = occupied[j];
                    assert!(
                        !rects_overlap(ra, rb),
                        "宽 {w} 下 {pa:?}{ra:?} 与 {pb:?}{rb:?} 重叠（档位 {:?}）",
                        s.tier
                    );
                }
            }
            // 3) 停靠矩形 + 分隔条的总宽不超过客户区宽度。
            let rect_total: f32 = occupied.iter().map(|(_, r)| r.width()).sum();
            let spl_total: f32 = s
                .splitters
                .iter()
                .filter(|sp| sp.width() > 0.0)
                .map(|sp| sp.width())
                .sum();
            assert!(
                rect_total + spl_total <= body_w + 0.01,
                "宽 {w} 下停靠矩形({rect_total:.1}) + 分隔条({spl_total:.1}) = {:.1} 超出 {body_w}（档位 {:?}）",
                rect_total + spl_total,
                s.tier
            );
        }
    }

    /// 同上不变式，但**用户拖过分隔条**（`width_delta` 非零）之后。
    ///
    /// 必须单独测：用户拖过的偏移会一路加进 `slot_width`，
    /// 它是最容易把段宽推出 `usable` 的输入。旧版的
    /// `.min(usable.max(floor))` 空操作钳制在这里最容易暴露。
    #[test]
    fn solved_rects_never_overlap_after_splitter_drag() {
        let mut st = LayoutState::default();
        for delta in [-500.0f32, -80.0, -7.0, 0.0, 7.0, 80.0, 500.0] {
            st.width_delta = [delta; 4];
            for w in [80.0f32, 186.7, 308.0, 420.0, 800.0] {
                let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
                let s = solve(area, &st, None);
                let mut occupied: Vec<(Panel, Rect)> = Vec::new();
                for p in Panel::ALL {
                    if let Some(r) = s.rects[panel_index(p)] {
                        occupied.push((p, r));
                    }
                }
                for i in 0..occupied.len() {
                    for j in (i + 1)..occupied.len() {
                        assert!(
                            !rects_overlap(occupied[i].1, occupied[j].1),
                            "宽 {w} delta {delta} 下 {:?} 与 {:?} 重叠",
                            occupied[i].0,
                            occupied[j].0
                        );
                    }
                }
                let rect_total: f32 = occupied.iter().map(|(_, r)| r.width()).sum();
                let spl_total: f32 = s
                    .splitters
                    .iter()
                    .filter(|sp| sp.width() > 0.0)
                    .map(|sp| sp.width())
                    .sum();
                assert!(
                    rect_total + spl_total <= w + 0.01,
                    "宽 {w} delta {delta} 下总宽 {:.1} 超出",
                    rect_total + spl_total
                );
            }
        }
    }

    /// 不变式在**用户改过布局**之后同样成立。
    ///
    /// 覆盖「用户把模块拖到非默认槽位」「全部折叠」「全部浮动」等状态——
    /// 降级只作用于默认槽位上的模块（见 [`Tier::visibility`]），
    /// 所以这些状态下阈值会变，必须实测而不是假设。
    #[test]
    fn solved_rects_never_overlap_under_arbitrary_user_layouts() {
        let mut states: Vec<LayoutState> = Vec::new();

        // 默认。
        states.push(LayoutState::default());

        // 用户把各模块拖到非默认槽位。
        let mut s1 = LayoutState::default();
        dock(&mut s1, Panel::Pinned, Slot::Right);
        dock(&mut s1, Panel::Detail, Slot::Left);
        dock(&mut s1, Panel::Rail, Slot::Center);
        states.push(s1);

        // 全部折叠。
        let mut s2 = LayoutState::default();
        for p in Panel::ALL {
            toggle_collapse(&mut s2, p);
        }
        states.push(s2);

        // 全部浮动（主布局全空）。
        let mut s3 = LayoutState::default();
        for (k, p) in Panel::ALL.iter().enumerate() {
            float(&mut s3, *p, r(10.0 * k as f32, 40.0, 180.0, 120.0));
        }
        states.push(s3);

        // 全部堆进中央槽（一个槽里4 个模块）。
        let mut s4 = LayoutState::default();
        for p in Panel::ALL {
            dock(&mut s4, p, Slot::Center);
        }
        states.push(s4);

        // 堆进左槽。
        let mut s5 = LayoutState::default();
        for p in Panel::ALL {
            dock(&mut s5, p, Slot::Left);
        }
        states.push(s5);

        for (si, st) in states.iter().enumerate() {
            for w in [0.0f32, 24.0, 50.0, 56.0, 120.0, 186.7, 240.0, 308.0, 420.0, 800.0, 1600.0] {
                let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
                let s = solve(area, st, None);
                let mut occupied: Vec<(Panel, Rect)> = Vec::new();
                for p in Panel::ALL {
                    if let Some(r) = s.rects[panel_index(p)] {
                        occupied.push((p, r));
                    }
                }
                for i in 0..occupied.len() {
                    for j in (i + 1)..occupied.len() {
                        assert!(
                            !rects_overlap(occupied[i].1, occupied[j].1),
                            "状态{si} 宽 {w} 下 {:?} 与 {:?} 重叠（档位 {:?}）",
                            occupied[i].0,
                            occupied[j].0,
                            s.tier
                        );
                    }
                }
                let rect_total: f32 = occupied.iter().map(|(_, r)| r.width()).sum();
                let spl_total: f32 = s
                    .splitters
                    .iter()
                    .filter(|sp| sp.width() > 0.0)
                    .map(|sp| sp.width())
                    .sum();
                assert!(
                    rect_total + spl_total <= w + 0.01,
                    "状态 {si} 宽 {w} 下总宽 {:.1} 超出（档位 {:?}）",
                    rect_total + spl_total,
                    s.tier
                );
            }
        }
    }

    /// [`allocate`] 自身的不变式：返回值之和恒 `<= usable`。
    ///
    /// 直接测被测函数，而不是只靠 `solve` 的间接覆盖——
    /// 这样失败时报错能指向真正的分配函数而不是整个 `solve`。
    #[test]
    fn allocate_never_exceeds_usable() {
        let st = LayoutState::default();
        let left = [Panel::Pinned];
        let center = [Panel::History];
        let right = [Panel::Detail, Panel::Rail];
        for tier in Tier::ALL {
            for usable in [
                -100.0f32, 0.0, 1.0, 50.0, 100.0, 186.7, 250.0, 308.0, 420.0, 900.0, 5000.0,
            ] {
                let seg = allocate(usable, &left, &center, &right, &st, tier);
                assert!(
                    seg.left + seg.center + seg.right <= usable.max(0.0) + 0.01,
                    "档位 {tier:?} usable {usable}: 三段和 {:.1} 超出",
                    seg.left + seg.center + seg.right
                );
                assert!(
                    seg.left >= 0.0 && seg.center >= 0.0 && seg.right >= 0.0,
                    "档位 {tier:?} usable {usable}: 出现负宽度 {seg:?}"
                );
            }
        }
    }

    /// 降级必须**单调**：窗口越窄，档位只会越低。
    ///
    /// 单调性是「不出现中间态」的前提。若某处改成「按模块各自判断」
    /// 而失去单调，用户会看到「置顶没了但侧栏还在」这类半档状态。
    #[test]
    fn tier_is_monotonic_in_width() {
        let st = LayoutState::default();
        // 窗口从宽到窄，档位只能「变大」（= 让步更多）。
        let mut prev = Tier::Full;
        for w in (0..1200).rev().step_by(4) {
            let t = tier_for(&st, w as f32);
            assert!(
                t >= prev,
                "宽 {} 时档位从 {:?} 反而降到 {:?}，降级不单调",
                w,
                prev,
                t
            );
            prev = t;
        }
        assert_eq!(prev, Tier::HistoryOnly, "极窄时必须降到只剩历史");
    }

    /// 降级必须**可逆**：窗口变宽后档位逐级恢复。
    ///
    /// 「自动且可逆」是需求里的硬要求。可逆的检验方式是**往返**：
    /// 宽 → 窄 → 宽，档位必须回到最初那个。
    #[test]
    fn tier_recovers_when_window_widens() {
        let st = LayoutState::default();
        for narrow in [0.0f32, 50.0, 120.0, 186.7, 250.0, 307.0] {
            let low = tier_for(&st, narrow);
            for wide in [160.0f32, 188.0, 308.0, 400.0, 600.0, 1000.0] {
                // 只在「确实更宽」时比较；否则断言本身就没意义
                // （160 比 250 窄，档位理应更差）。
                if wide <= narrow {
                    continue;
                }
                let high = tier_for(&st, wide);
                assert!(
                    high <= low,
                    "窄 {narrow}({low:?}) → 宽 {wide} 档位反而更窄: {high:?}"
                );
                // 往返：档位必须是 `width` 的纯函数，不带隐藏状态。
                assert_eq!(
                    tier_for(&st, wide),
                    high,
                    "档位不是纯函数（宽 {wide}）"
                );
            }
        }
    }

    /// 每一档的阈值必须**严格递减**，否则阶梯退化（两级等价）。
    ///
    /// 这是「阶梯设计有五级」的保证：若某一级的
    /// `min_required_width` 与上一级相等，[`tier_for`] 永远选不到它，
    /// 于是那一级的降级动作（例如置顶收把手）实际上是死代码。
    #[test]
    fn tier_thresholds_strictly_decrease() {
        let st = LayoutState::default();
        let reqs: Vec<f32> = Tier::ALL
            .iter()
            .map(|t| t.min_required_width(&st))
            .collect();
        for w in reqs.windows(2) {
            assert!(
                w[1] < w[0],
                "降级阶梯退化：{:.1} -> {:.1} 没有严格递减（各档需求 {:?}）",
                w[0],
                w[1],
                reqs
            );
        }
    }

    /// 阈值与档位必须自洽：每一档都能被 [`tier_for`] 真正选中。
    ///
    /// 上一条只保证阈值递减，这条保证「递减的区间非空」——
    /// 两者合起来才说明五档都是可达的。
    #[test]
    fn every_tier_is_reachable_at_its_own_threshold() {
        let st = LayoutState::default();
        for (i, tier) in Tier::ALL.iter().enumerate() {
            let req = tier.min_required_width(&st);
            if i == 0 {
                // 最高档在「刚好等于自身需求」时就该被选中。
                assert_eq!(tier_for(&st, req), *tier, "最高档在 {req} 时未被选中");
                continue;
            }
            let prev_req = Tier::ALL[i - 1].min_required_width(&st);
            // 取「上一档需求 - 0.5」到「本档需求」之间的宽度，必须落回本档。
            let probe = (prev_req - 0.5).max(0.0);
            assert_eq!(
                tier_for(&st, probe),
                *tier,
                "档位 {:?} 不可达：prev_req={prev_req:.1} req={req:.1} probe={probe:.1}",
                tier
            );
            assert_eq!(
                tier_for(&st, req),
                *tier,
                "档位 {:?} 在自身阈值 {req:.1} 处未被选中",
                tier
            );
        }
    }

    /// 历史列表在任何档位下都不会消失（除了它自己被用户折叠）。
    ///
    /// 历史是主区，降级的全部意义就是「让别的模块给它让位」。
    /// 若某档连历史都保不住，这套阶梯就白设计了。
    #[test]
    fn history_survives_every_tier() {
        let st = LayoutState::default();
        for w in [0.0f32, 24.0, 100.0, 186.7, 308.0, 420.0, 900.0] {
            let s = solve(Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0)), &st, None);
            let vis = s.tier.visibility(&st, Panel::History);
            assert_eq!(
                vis,
                Visibility::Docked,
                "宽 {w} 下历史被降级成 {vis:?}（档位 {:?}）",
                s.tier
            );
        }
    }

    /// 降级只在模块仍处于**默认槽位**时生效。
    ///
    /// 用户把置顶拖到右槽之后，窄窗口不该再去折叠它——
    /// 那是用户的安排，不是默认布局的一部分。
    #[test]
    fn degradation_respects_non_default_slots() {
        let mut st = LayoutState::default();
        // 把置顶拖到右槽、并把侧栏展开（默认它是折叠的，
        // 展开才能验证「未到隐藏阈值时保持展开」）。
        dock(&mut st, Panel::Pinned, Slot::Right);
        toggle_collapse(&mut st, Panel::Rail);
        let tier = Tier::PinnedFolded;
        assert_eq!(
            tier.visibility(&st, Panel::Pinned),
            Visibility::Docked,
            "用户拖到右槽的置顶不该被默认槽的降级规则折叠"
        );
        // 默认槽上的侧栏仍然照常降级。
        assert_eq!(
            tier.visibility(&st, Panel::Rail),
            Visibility::Docked,
            "同档位下侧栏还没到隐藏阈值"
        );
        assert_eq!(
            Tier::RailHidden.visibility(&st, Panel::Rail),
            Visibility::Hidden,
            "侧栏在 RailHidden 档应隐藏"
        );
        // 历史被拖去别的槽位时也照样是展开（它永不被降级）。
        dock(&mut st, Panel::History, Slot::Right);
        assert_eq!(
            Tier::HistoryOnly.visibility(&st, Panel::History),
            Visibility::Docked,
            "历史在任何档位都不降级"
        );
    }

    /// 用户手动折叠的模块**不会被降级改回展开**。
    ///
    /// ⚠️ 但降级**可以**把它进一步压成隐藏——这是有意的：
    /// 折叠只表示「用户不想看它」，不表示「它必须占一条把手」。
    /// 极窄窗口下那条把手同样要让位，否则用户会看到
    /// 「明明折叠了还占着 24 逻辑点」。
    #[test]
    fn user_collapse_is_never_widened_by_degradation() {
        let mut st = LayoutState::default();
        toggle_collapse(&mut st, Panel::Pinned);
        for tier in Tier::ALL {
            let v = tier.visibility(&st, Panel::Pinned);
            assert!(
                v != Visibility::Docked,
                "档位 {tier:?} 把用户手动折叠的置顶改成了展开: {v:?}"
            );
        }
    }

    /// 详情在窄档位下**转浮层**而不是被彻底隐藏。
    ///
    /// 这是「降级不等于功能消失」的核心断言。
    #[test]
    fn detail_becomes_floating_when_narrow() {
        let mut st = LayoutState::default();
        // 先让详情展开（默认是折叠的，折叠态不参与「转浮层」）。
        toggle_collapse(&mut st, Panel::Detail);
        assert!(
            !Tier::Full.detail_is_floating(&st),
            "宽窗口下详情不该浮起来"
        );
        for tier in [
            Tier::DetailFloating,
            Tier::PinnedFolded,
            Tier::RailHidden,
            Tier::HistoryOnly,
        ] {
            assert!(
                tier.detail_is_floating(&st),
                "档位 {tier:?} 下详情应转浮层"
            );
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 600.0));
            let s = solve(area, &st, None);
            let r = s.floating[panel_index(Panel::Detail)]
                .expect("转浮层后必须给出浮层矩形");
            assert!(
                r.width() > 0.0 && r.height() > 0.0,
                "浮层矩形退化: {r:?}"
            );
            assert!(
                r.max.x <= 200.0 + 0.01,
                "浮层越出客户区: {r:?}"
            );
        }
    }

    /// 档位是 [`solve`] 输出的一部分，绘制层据此决定画什么。
    ///
    /// 断言 `Solved::tier` 与直接调 [`tier_for`] 一致——
    /// 防止将来有人在 `solve` 里传了不同的宽度（比如误传 `area.width()`
    /// 而非 `body.width()`），导致绘制层读到的档位与实际布局不符。
    #[test]
    fn solved_tier_matches_tier_for_body_width() {
        let st = LayoutState::default();
        for w in [50.0f32, 186.7, 308.0, 420.0, 900.0] {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
            let s = solve(area, &st, None);
            let body_w = area.width();
            assert_eq!(
                s.tier,
                tier_for(&st, body_w),
                "宽 {w}: solve 报的档位与 tier_for 不一致"
            );
        }
    }

    /// `Panel::min_width` 与 `Panel::default_width` 的关系必须合理。
    ///
    /// 保底大于建议宽会让 [`Tier::min_required_width`] 算出
    /// 「永远装不下」的档位，阶梯直接塌掉。
    #[test]
    fn min_width_never_exceeds_default_width() {
        for p in Panel::ALL {
            assert!(
                p.min_width() <= p.default_width(),
                "{p:?}: 保底 {} 大于建议宽{}",
                p.min_width(),
                p.default_width()
            );
            assert!(
                p.collapsed_width() < p.min_width(),
                "{p:?}: 折叠把手 {} 不该宽于保底 {}",
                p.collapsed_width(),
                p.min_width()
            );
        }
    }

    /// `allocate` 与 `solve` 必须用同一套可见性判据。
    ///
    /// 若两者分歧（例如 `solve` 少画了一个折叠成员），
    /// 分配时算好的宽度就与实际画出的矩形对不上，
    /// 表现是段与段之间留出一条没人用的空隙。
    #[test]
    fn allocate_and_solve_agree_on_segment_widths() {
        let st = LayoutState::default();
        for w in [186.7f32, 308.0, 420.0, 800.0] {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 600.0));
            let s = solve(area, &st, None);
            let tier = s.tier;

            // 复现 `solve` 的分组（同为`Panel::ALL` 顺序的停靠模块）。
            let docked: Vec<Panel> = Panel::ALL
                .iter()
                .copied()
                .filter(|p| {
                    !st.placement[panel_index(*p)].is_floating()
                        && tier.visibility(&st, *p) != Visibility::Hidden
                })
                .collect();
            let slot_of = |p: Panel| match st.placement[panel_index(p)] {
                Placement::Docked { slot, .. } => Some(slot),
                _ => None,
            };
            let left: Vec<Panel> = docked
                .iter()
                .copied()
                .filter(|p| slot_of(*p) == Some(Slot::Left))
                .collect();
            let center: Vec<Panel> = docked
                .iter()
                .copied()
                .filter(|p| slot_of(*p) == Some(Slot::Center))
                .collect();
            let right: Vec<Panel> = docked
                .iter()
                .copied()
                .filter(|p| slot_of(*p) == Some(Slot::Right))
                .collect();

            let spl_total: f32 = s
                .splitters
                .iter()
                .filter(|sp| sp.width() > 0.0)
                .count() as f32
                * SPLITTER_WIDTH;
            let usable = area.width() - spl_total;
            let seg = allocate(usable, &left, &center, &right, &st, tier);

            // 实际画出的中央段起点：左段起点 + 左段宽 + 段间分隔条。
            //
            // ⚠️ 必须把「左|中」那条段间分隔条算进去。
            // 它占的6 逻辑点已经从 `usable` 里扣掉，并由 `solve`
            // 画成一条真实的分隔条；漏掉它就会误判成「分配与绘制不一致」。
            let has_left = left
                .iter()
                .any(|p| tier.visibility(&st, *p) != Visibility::Hidden);
            let has_center = center
                .iter()
                .any(|p| tier.visibility(&st, *p) != Visibility::Hidden);
            let gap = if has_left && has_center {
                SPLITTER_WIDTH
            } else {
                0.0
            };
            let center_x = s
                .rects[panel_index(Panel::History)]
                .map(|r| r.min.x)
                .unwrap_or(-1.0);
            assert!(
                (center_x - (seg.left + gap)).abs() < 0.01,
                "宽 {w}: 中央段起点 {center_x} 与分配不符（left={} gap={gap}，档位 {tier:?}）",
                seg.left
            );
        }
    }
}