//! 卡片模型：界面的一等实体。
//!
//! # 为什么要有这个模块
//!
//! 旧架构把界面拆成「面板（Panel）+ 档位（Tier）」两层，导致
//! **同一件事必须算两遍**：
//!
//! | 事实 | 布局侧 | 绘制侧 |
//! |---|---|---|
//! | 模块是否折叠 | `Tier::visibility()` | `is_veiled()` |
//! | 面板矩形 | `layout::solve()` | `solved.rects[i]` |
//!
//! 两处判据靠人工保持同步，历史上多次出现「布局对了但画错」
//! （详情折叠态无人绘制、状态栏提示压按钮都是这个成因）。
//!
//! 新模型里**卡片自己就是全部真相**：它知道自己该不该折叠、
//! 自己的矩形是多少。布局只负责把矩形算出来写回去，
//! 绘制只负责读出来画上去，中间没有任何判断。
//!
//! # 不变式
//!
//! 1. **单一数据源**：`Card::rect` 是卡片矩形的唯一来源。
//!    绘制层直接读它，绝不自己再算一遍。
//! 2. **顺序即 Z 序**：`Workspace::cards` 的顺序就是绘制顺序，
//!    后面的盖在前面上面。不需要单独的 z_index 字段。
//! 3. **状态不变量**：[`Card::is_visible`] 为真时，本卡**要么**有非零
//!    宽高的 `rect`（由 [`crate::solver`] 保证），**要么**是「内嵌置顶」
//!    ——它画在历史栏顶部内部，布局宽度**刻意为 0**，不占一栏。
//!
//!    ⚠️ 早前这里写的是「`is_visible()` 为真时 `rect` 一定非零宽高」，
//!    而那条不变式**是假的**：内嵌置顶的 `is_visible()` 为真、但 `rect`
//!    宽度是 0（solver 的既定设计）。照着它写代码会得到错的判断。

use egui::{Rect, Vec2, vec2};

/// 卡片身份。跨帧稳定，用于 egui 的 widget Id 与状态查找。
///
/// ⚠️ 必须是**稳定标识**，不能用数组下标：卡片顺序会随拖拽变化，
/// 用下标会让 egui 把 A 卡片的悬停/焦点状态记到 B 卡片头上
/// （egui 按 Id 存交互状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CardId(pub u32);

/// 卡片种类。决定它的内容、最小宽度与折叠优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardKind {
    /// 历史列表。主区，任何时候都不折叠。
    History,
    /// 置顶。可选「共用单栏」或「独立卡片」两种形态。
    Pinned,
    /// 详情：显示选中条目的完整内容。
    Detail,
    /// 侧栏：表格 / 密文两种视图。
    Rail,
}

impl CardKind {
    /// 全部种类。顺序即默认 Z 序（越靠后越上层）。
    pub const ALL: [CardKind; 4] = [
        CardKind::Pinned,
        CardKind::History,
        CardKind::Detail,
        CardKind::Rail,
    ];

    /// 卡片标题（画在卡片头部）。
    pub fn title(self) -> &'static str {
        match self {
            CardKind::History => "历史",
            CardKind::Pinned => "置顶",
            CardKind::Detail => "详情",
            CardKind::Rail => "视图",
        }
    }

    /// 图标标识，用于卡片头部与空状态。
    pub fn icon(self) -> &'static str {
        match self {
            CardKind::History => "history",
            CardKind::Pinned => "pin",
            CardKind::Detail => "info",
            CardKind::Rail => "table",
        }
    }

    /// 展开态的最小宽度（逻辑点）。
    ///
    /// ⚠️ 宽度不够时**不再**降级成折叠态——原先的「折叠优先级」
    /// （`collapse_priority` + `handle_width` + `collapsible` +
    /// `Card::collapsed`）已整体移除。主界面固定为置顶 + 历史两栏，
    /// 窄窗口下由 [`crate::solver`] 缩放而非折叠。
    ///
    /// 数值来自旧布局的 `MIN_*_WIDTH`，含义是「分配时至少给这么多」。
    pub fn min_width(self) -> f32 {
        match self {
            CardKind::History => 160.0,
            CardKind::Pinned => 120.0,
            CardKind::Detail => 180.0,
            CardKind::Rail => 88.0,
        }
    }
}

/// 卡片停靠在哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardHost {
    /// 停靠在主窗口工作区内，由 [`crate::solver`] 分配矩形。
    Docked,
    /// 已分离为独立子窗口，矩形由该子窗口自己的客户区决定。
    Window,
}

/// 置顶卡片的呈现模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PinnedMode {
    /// 共用单栏：置顶与历史共享一个列表容器。
    ///
    /// 置顶不占独立列，而是历史列表顶部的一段。数据由同一个容器
    /// 按 `pinned` 分区渲染，因此**天然共享选中态与滚动联动**。
    #[default]
    SharedColumn,
    /// 独立分栏：置顶是一张独立卡片，可以拖出去成为子窗口。
    OwnCard,
}

/// 一张卡片。
#[derive(Debug, Clone)]
pub struct Card {
    /// 稳定身份。
    pub id: CardId,
    /// 种类。
    pub kind: CardKind,
    /// 停靠位置。
    pub host: CardHost,
    /// 当前矩形，由 solver 写入、绘制层读取。
    pub rect: Rect,
    /// 展开态的**目标**宽度（用户拖分隔条的结果）。
    ///
    /// 它是「用户意图」，solver 不会改写。
    pub target_width: f32,
    /// 目标高度（纵向平铺时使用）。
    pub target_height: f32,
    /// 置顶模式（仅 [`CardKind::Pinned`] 读取）。
    pub pinned_mode: PinnedMode,
    /// 分离为子窗口时的屏幕位置（逻辑点，虚拟桌面坐标）。
    pub window_pos: Vec2,
    /// 分离为子窗口时的尺寸（逻辑点）。
    pub window_size: Vec2,
}

impl Card {
    /// 按种类创建一张卡片。
    pub fn new(id: CardId, kind: CardKind) -> Self {
        Self {
            id,
            kind,
            host: CardHost::Docked,
            // 初始为零矩形；第一帧由 solver 写入。
            rect: Rect::ZERO,
            target_width: kind.min_width(),
            target_height: 320.0,
            pinned_mode: PinnedMode::default(),
            // 窗口位置/尺寸的**最终值**由 `Workspace::add` 按已有卡片数
            // 错开设置——所有卡片都用同一个默认值会让多个子窗口重叠
            // （后创建的压在前者上面，看起来像「界面缺了一块」）。
            window_pos: vec2(120.0, 120.0),
            window_size: vec2(360.0, 480.0),
        }
    }

    /// 本卡片在主窗口布局里占的宽度。
    pub fn layout_width(&self) -> f32 {
        if !self.is_visible() {
            0.0
        } else {
            self.target_width
        }
    }

    /// 本帧是否要绘制。
    ///
    /// 分离成子窗口的卡片**不由主窗口绘制**，它在自己的窗口里画。
    pub fn is_visible(&self) -> bool {
        self.host == CardHost::Docked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_card_takes_no_layout_width() {
        // 已分离的卡片在子窗口里画，主窗口不能给它留位置，
        // 否则工作区右边会多出一条空白缝。
        let mut c = Card::new(CardId(1), CardKind::Detail);
        assert!(c.layout_width() > 0.0, "停靠时应占宽度");
        c.host = CardHost::Window;
        assert_eq!(c.layout_width(), 0.0, "分离后不应占宽度");
        assert!(!c.is_visible(), "分离后主窗口不画它");
    }

    #[test]
    fn every_card_kind_has_a_distinct_min_width() {
        // 求解器按最小宽度分配；两个卡片同宽会让贪心分配失去意义。
        let mut seen: Vec<f32> = Vec::new();
        for k in CardKind::ALL {
            let w = k.min_width();
            assert!(!seen.contains(&w), "{k:?} 的最小宽度 {w} 与其它卡片重复");
            seen.push(w);
        }
    }

    #[test]
    fn card_ids_are_distinct_when_created_in_sequence() {
        // 下标当Id 会在拖拽换序后串状态；这里守住「用显式递增Id」。
        let cards: Vec<_> = (0..4u32)
            .map(|i| Card::new(CardId(i), CardKind::ALL[i as usize]))
            .collect();
        let mut ids: Vec<CardId> = cards.iter().map(|c| c.id).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "卡片 Id 必须互不相同");
    }
}
