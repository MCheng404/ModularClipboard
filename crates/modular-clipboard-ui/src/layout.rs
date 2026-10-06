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
            // 侧栏默认就是折叠态的窄条。
            Panel::Rail => RAIL_COLLAPSED_WIDTH,
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
    /// 分隔条矩形，共 3 个（右槽 2 个模块 → 1 个分隔条；左槽同理）。
    pub splitters: [Rect; 3],
    /// 分隔条分隔的模块对，用于交互时反查「我拖的是哪两个之间」。
    pub splitter_pairs: [(Panel, Panel); 3],
    /// 浮动模块的矩形。
    pub floating: [Option<Rect>; 4],
    /// 停靠区预览（拖动时命中某区才有值）。
    pub dock_preview: Option<(DockZone, Rect)>,
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
        splitters: [Rect::ZERO; 3],
        splitter_pairs: [(Panel::History, Panel::History); 3],
        floating: [None; 4],
        dock_preview: None,
    };

    // ---- 浮动模块：直接用自己存的矩形，钳到窗口内 ----
    for p in Panel::ALL {
        let i = panel_index(p);
        if let Placement::Floating { rect } = state.placement[i] {
            out.floating[i] = Some(clamp_rect(rect, body, MIN_FLOAT_SIZE));
        }
    }

    // ---- 停靠模块：按槽位分组 ----
    // 槽内按 Panel::ALL 顺序排，保证同一槽位的相对次序不随拖动而乱。
    let docked: Vec<Panel> = Panel::ALL
        .iter()
        .copied()
        .filter(|p| !state.placement[panel_index(*p)].is_floating())
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

    // 三段的宽度预算。中央段吃掉剩余空间——它是默认的主区，
    // 窗口变大时应当由它变大。
    let splitter_total = docked.len().saturating_sub(1) as f32 * SPLITTER_WIDTH;
    let usable = body.width() - splitter_total;
    let left_w = slot_width(&groups[0].1, state, usable, 0.28);
    let right_w = slot_width(&groups[2].1, state, usable, 0.28);
    let center_w = (usable - left_w - right_w).max(MIN_DOCK_WIDTH);

    let mut x = body.min.x;
    let mut pairs = [(Panel::History, Panel::History); 3];
    let mut n_pairs = 0usize;
    // 只处理左/右两段：中央段始终占满剩余，内部不再切分隔条。
    for (gi, slot) in [(0usize, Slot::Left), (2usize, Slot::Right)] {
        let members = &groups[gi].1;
        if members.is_empty() {
            continue;
        }
        let total = if slot == Slot::Left { left_w } else { right_w };
        let n = members.len() as f32;
        for (k, p) in members.iter().enumerate() {
            let i = panel_index(*p);
            let collapsed = state.placement[i].is_collapsed();
            // 折叠的侧栏只留窄条，其余折叠模块完全不占位。
            let w = if collapsed {
                if *p == Panel::Rail {
                    RAIL_COLLAPSED_WIDTH
                } else {
                    0.0
                }
            } else {
                (total - SPLITTER_WIDTH * (n - 1.0)) / n
            };
            let r = Rect::from_min_size(egui::pos2(x, body.min.y), vec2(w, body.height()));
            if collapsed && w <= 0.0 {
                out.rects[i] = None;
            } else {
                out.rects[i] = Some(r);
            }
            x += w;
            if k + 1 < members.len() && n_pairs < 3 {
                let s = Rect::from_min_size(egui::pos2(x, body.min.y), vec2(SPLITTER_WIDTH, body.height()));
                pairs[n_pairs] = (*p, members[k + 1]);
                out.splitters[n_pairs] = s;
                n_pairs += 1;
                x += SPLITTER_WIDTH;
            }
        }
    }

    // 中央段起点：左段总宽之后。
    let center_x = body.min.x + left_w;
    let center_members = &groups[1].1;
    if center_members.is_empty() {
        // 中央空着也要给主区一个可点击的底板，否则窗口中央是死区。
        out.rects[panel_index(Panel::History)] =
            Some(Rect::from_min_max(egui::pos2(center_x, body.min.y), body.max));
    } else {
        let n = center_members.len() as f32;
        let inner = center_w - SPLITTER_WIDTH * (n - 1.0);
        let mut cx = center_x;
        for (k, p) in center_members.iter().enumerate() {
            let i = panel_index(*p);
            let collapsed = state.placement[i].is_collapsed();
            let w = if collapsed { 0.0 } else { inner / n };
            out.rects[i] = if collapsed && w <= 0.0 {
                None
            } else {
                Some(Rect::from_min_size(
                    egui::pos2(cx, body.min.y),
                    vec2(w, body.height()),
                ))
            };
            cx += w;
            if k + 1 < center_members.len() && n_pairs < 3 {
                out.splitters[n_pairs] = Rect::from_min_size(
                    egui::pos2(cx, body.min.y),
                    vec2(SPLITTER_WIDTH, body.height()),
                );
                pairs[n_pairs] = (*p, center_members[k + 1]);
                n_pairs += 1;
                cx += SPLITTER_WIDTH;
            }
        }
    }
    out.splitter_pairs = pairs;

    // 停靠区预览：仅在拖动中且确实命中某个停靠区时出现。
    if let Some(moving) = dragging
        && let Some(zone) = dock_zone_of(Some(moving), body)
    {
        let slot = zone.slot();
        let w = moving.width().min(center_w.max(MIN_FLOAT_SIZE.x)).max(MIN_DOCK_WIDTH);
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

/// 停靠段的总宽度：各模块默认宽之和，按 `frac` 分配可用宽。
///
/// 用比例而非固定值分配：`usable` 会随窗口宽度变，
/// 固定值在窄窗口下会把左段挤成不可用的宽度。
fn slot_width(members: &[Panel], state: &LayoutState, usable: f32, frac: f32) -> f32 {
    if members.is_empty() {
        return 0.0;
    }
    // 至少给每个成员留出可读宽度，不够则退回基础宽度。
    let floor = members.len() as f32 * MIN_DOCK_WIDTH;
    let target = (usable * frac).max(floor);
    // 这一段里**真正占位**的成员。折叠成员不占位，就该在这里被排除——
    // 早前版本把折叠成员算进 `base` 却不���进 `live`，比值大于 1，
    // 侧栏宽度会成倍放大（表现为「窗口越宽侧栏越胖」）。
    let occupied: f32 = members
        .iter()
        .map(|p| {
            let i = panel_index(*p);
            if state.placement[i].is_collapsed() {
                // 侧栏折叠后仍留一条窄条，它是有位置的。
                if *p == Panel::Rail {
                    RAIL_COLLAPSED_WIDTH
                } else {
                    0.0
                }
            } else {
                p.default_width()
            }
        })
        .sum();
    if occupied <= 0.0 {
        // 整段都是折叠态：只留折叠把手所需的最小宽度。
        return floor.min(usable.max(0.0));
    }
    // 累加用户拖动分隔条产生的偏移。
    let delta: f32 = members.iter().map(|p| state.width_delta[panel_index(*p)]).sum();
    (target + delta).max(floor).min(usable.max(floor))
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
        // 未使用的分隔条槽位是 `Rect::ZERO`，反查必须跳过——
    // 否则任何零宽矩形都会命中槽位 0，拿回一对无意义的
    // `(History, History)`，交互层就会去拖一个不存在的分隔条。
        .filter(|k| solved.splitters[*k].width() > 0.0)
        .map(|k| solved.splitter_pairs[k])
}

// ---------------------------------------------------------------------------
// 持久化
// ---------------------------------------------------------------------------

/// 布局状态的磁盘表示。
///
/// 与 [`LayoutState`] 分离的原因：布局用的 `Rect` 是 egui 类型，
/// 直接序列化会把渲染库的内部结构写进配置文件——换 egui 版本就得
/// 迁移用户数据。这层 DTO 只用原始数字，是**稳定格式**。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
}

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
        // 默认详情折叠 → 无矩形
        assert_eq!(s.rects[panel_index(Panel::Detail)], None);
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
}