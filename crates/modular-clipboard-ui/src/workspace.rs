//! 工作区：卡片的集合与排列。
//!
//! # 单一数据源
//!
//! 本模块是**卡片状态的唯一权威**。绘制层不持有任何卡片状态，
//! 布局层也不缓存副本——都从这里读。旧架构里「布局算一遍、
//! 绘制再判一遍」的双实现问题，根源就是没有这样一个明确的归属者。
//!
//! # 顺序即 Z 序
//!
//! [`Workspace::cards`] 的顺序就是绘制顺序：后面的盖在前面上面。
//! 不需要单独的 `z_index` 字段，也就不存在「z_index 与数组顺序
//! 不一致」这类 bug。

use egui::Rect;

use crate::card::{Card, CardHost, CardId, CardKind, PinnedMode};

/// 一组卡片及其排列状态。
#[derive(Debug)]
pub struct Workspace {
    /// 卡片列表，顺序即 Z 序。
    pub cards: Vec<Card>,
    /// 下一个可用Id。
    ///
    /// 单调递增、**永不复用**：Id 稳定是 egui 交互状态不错乱的前提，
    /// 复用会在卡片删除重建后把旧状态串到新卡片上。
    next_id: u32,
    /// 卡片之间的间隔（逻辑点）。
    pub gap: f32,
}

impl Default for Workspace {
    fn default() -> Self {
        let mut ws = Self {
            cards: Vec::new(),
            next_id: 0,
            // ⚠️ 卡片之间**不留间隙**：卡片本身有 1px 描边 + 圆角，
            // 再留 6pt 空隙会让相邻卡片之间露出一条明显的缝，
            // 看起来像「窗口之间有间隙」（用户反馈过这个现象）。
            // 设 0 让卡片边缘贴合，视觉上是一个连续的面板。
            gap: 0.0,
        };
        // 默认卡片集。
        //
        // ⚠️ `Detail` / `Rail` **默认就分离为独立子窗口**
        // （`CardHost::Window`）——主窗口只留「置顶 + 历史」两栏。
        // 它们仍留在 `cards` 里，只是 `host` 为 Window，
        // 于是 solver 不给它们分配宽度、主窗口也不绘制。
        for (kind, host) in [
            (CardKind::Pinned, CardHost::Docked),
            (CardKind::History, CardHost::Docked),
            (CardKind::Rail, CardHost::Window),
            (CardKind::Detail, CardHost::Window),
        ] {
            let id = ws.add(kind);
            ws.get_mut(id).expect("刚加入的卡片").host = host;
        }
        ws
    }
}

impl Workspace {
    /// 新建空工作区。
    pub fn empty() -> Self {
        Self {
            cards: Vec::new(),
            next_id: 0,
            gap: 6.0,
        }
    }

    /// 追加一张卡片，返回其 Id。
    pub fn add(&mut self, kind: CardKind) -> CardId {
        let id = CardId(self.next_id);
        self.next_id += 1;
        self.cards.push(Card::new(id, kind));
        id
    }

    /// 按 Id 取卡片。
    pub fn get_mut(&mut self, id: CardId) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.id == id)
    }

    /// 按 Id 取卡片（只读）。
    pub fn get(&self, id: CardId) -> Option<&Card> {
        self.cards.iter().find(|c| c.id == id)
    }

    /// 按种类取第一张匹配卡片。
    pub fn by_kind(&self, kind: CardKind) -> Option<&Card> {
        self.cards.iter().find(|c| c.kind == kind)
    }

    /// 按种类取第一张匹配卡片（可变）。
    pub fn by_kind_mut(&mut self, kind: CardKind) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.kind == kind)
    }

    /// 停靠在主窗口、需要绘制的卡片，按 Z 序。
    ///
    /// 返回**具体迭代器类型**而非 `impl Iterator`：调用方需要
    /// `.rev()`（命中测试要从最上层往下找），那要求
    /// [`DoubleEndedIterator`]，用 `impl Iterator` 会把这个能力抹掉。
    pub fn docked(&self) -> impl DoubleEndedIterator<Item = &Card> {
        self.cards.iter().filter(|c| c.host == CardHost::Docked)
    }

    /// 已分离成子窗口的卡片。
    pub fn detached(&self) -> impl DoubleEndedIterator<Item = &Card> {
        self.cards.iter().filter(|c| c.host == CardHost::Window)
    }

    /// 停靠卡片的总需求宽度（含间隔）。
    pub fn required_width(&self) -> f32 {
        let n = self.docked().count();
        if n == 0 {
            return 0.0;
        }
        let sum: f32 = self.docked().map(|c| c.layout_width()).sum();
        sum + self.gap * (n as f32 - 1.0)
    }

    /// 把卡片移到Z 序的某一层（`to_front` 传卡片当前索引）。
    pub fn raise(&mut self, index: usize) {
        if index + 1 < self.cards.len() {
            let c = self.cards.remove(index);
            self.cards.push(c);
        }
    }

    /// 把卡片上移一层。返回是否真的移动了。
    pub fn move_up(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.cards.len() {
            return false;
        }
        self.cards.swap(index, index - 1);
        true
    }

    /// 把卡片下移一层。返回是否真的移动了。
    pub fn move_down(&mut self, index: usize) -> bool {
        if index + 1 >= self.cards.len() {
            return false;
        }
        self.cards.swap(index, index + 1);
        true
    }

    /// 命中测试：找出 `pos` 处的最上层卡片。
    ///
    /// 逆序遍历（Z 序从下往上），第一个命中的就是最上层的——
    /// 与「后画的盖在上面」一致。
    pub fn hit_test(&self, pos: egui::Pos2) -> Option<&Card> {
        self.docked()
            .filter(|c| c.rect.width() > 0.0 && c.rect.height() > 0.0)
            .rev()
            .find(|c| c.rect.contains(pos))
    }

    /// 把卡片从工作区分离成子窗口。
    ///
    /// 已在子窗口里的卡片返回 `false`（幂等）。
    pub fn detach(&mut self, id: CardId) -> bool {
        match self.get_mut(id) {
            Some(c) if c.host == CardHost::Docked => {
                c.host = CardHost::Window;
                true
            }
            _ => false,
        }
    }

    /// 把卡片收回工作区。已在工作区的返回 `false`（幂等）。
    pub fn dock(&mut self, id: CardId) -> bool {
        match self.get_mut(id) {
            Some(c) if c.host == CardHost::Window => {
                c.host = CardHost::Docked;
                // 收回时若处于折叠态会只剩一条把手，用户会觉得「卡片不见了」，
                // 所以自动展开——分离前的折叠状态没有保留需求。
                c.collapsed = false;
                true
            }
            _ => false,
        }
    }

    /// 置顶卡片的模式。
    pub fn pinned_mode(&self) -> PinnedMode {
        self.by_kind(CardKind::Pinned)
            .map(|c| c.pinned_mode)
            .unwrap_or_default()
    }

    /// 设置置顶卡片模式。返回是否设置成功（没有置顶卡片时为 false）。
    pub fn set_pinned_mode(&mut self, mode: PinnedMode) -> bool {
        match self.by_kind_mut(CardKind::Pinned) {
            Some(c) => {
                c.pinned_mode = mode;
                true
            }
            None => false,
        }
    }

    /// 把所有停靠卡片的 `rect` 归零。
    ///
    /// solver 出错或窗口最小化（客户区 0x0）时调用：
    /// 画一帧零尺寸卡片比画一帧过期矩形安全。
    pub fn clear_rects(&mut self) {
        for c in &mut self.cards {
            if c.host == CardHost::Docked {
                c.rect = Rect::ZERO;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    #[test]
    fn default_workspace_has_the_four_core_cards() {
        let ws = Workspace::default();
        for k in CardKind::ALL {
            assert!(ws.by_kind(k).is_some(), "默认工作区应含{k:?}卡片");
        }
    }

    #[test]
    fn card_ids_are_never_reused_after_removal() {
        // Id 复用会让 egui 把旧卡片的交互状态串到新卡片上。
        let mut ws = Workspace::empty();
        let a = ws.add(CardKind::History);
        ws.cards.clear();
        let b = ws.add(CardKind::History);
        assert_ne!(a, b, "卡片删除后新增的卡片必须拿到新 Id");
    }

    #[test]
    fn required_width_includes_gaps_but_not_trailing_one() {
        let mut ws = Workspace::empty();
        for k in CardKind::ALL {
            let id = ws.add(k);
            let c = ws.get_mut(id).expect("刚加的卡片");
            c.collapsed = false;
        }
        let n = ws.docked().count() as f32;
        let sum: f32 = CardKind::ALL.iter().map(|k| k.min_width()).sum();
        let expect = sum + ws.gap * (n - 1.0);
        assert!(
            (ws.required_width() - expect).abs() < 0.01,
            "总需求应含 {} 个间隔，期望 {expect}，实际 {}",
            n as i32 - 1,
            ws.required_width()
        );
    }

    #[test]
    fn detached_cards_are_excluded_from_required_width() {
        let mut ws = Workspace::empty();
        let a = ws.add(CardKind::History);
        let b = ws.add(CardKind::Detail);
        let before = ws.required_width();
        assert!(ws.detach(b), "第一次分离应成功");
        assert!(!ws.detach(b), "重复分离应返回 false（幂等）");
        let after = ws.required_width();
        assert!(after < before, "分离后总需求应变小：{before} -> {after}");
        // 且 a 必须仍然占位
        assert!(
            ws.get(a).expect("a 仍在").layout_width() > 0.0,
            "未分离的卡片必须继续占位"
        );
    }

    #[test]
    fn docking_back_expands_the_card() {
        // 收回时若保持折叠，用户会觉得卡片消失了。
        let mut ws = Workspace::empty();
        let id = ws.add(CardKind::Detail);
        ws.get_mut(id).expect("刚加").collapsed = true;
        assert!(ws.detach(id));
        assert!(ws.dock(id));
        assert!(
            !ws.get(id).expect("仍在").collapsed,
            "收回工作区时必须自动展开"
        );
        assert!(!ws.dock(id), "重复收回应返回 false（幂等）");
    }

    #[test]
    fn hit_test_returns_topmost_card() {
        let mut ws = Workspace::empty();
        let bottom = ws.add(CardKind::History);
        let top = ws.add(CardKind::Detail);
        // 两张卡片完全重叠。
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 200.0));
        ws.get_mut(bottom).expect("b").rect = r;
        ws.get_mut(top).expect("t").rect = r;
        // 后加的 = 更上层 = 应命中。
        let hit = ws.hit_test(pos2(50.0, 50.0)).expect("应命中某张卡片");
        assert_eq!(hit.id, top, "命中应是Z 序最上层的卡片");
    }

    #[test]
    fn hit_test_ignores_zero_sized_cards() {
        // solver 未运行时 rect 全是 Rect::ZERO，
        // 若不排除，命中测试会永远返回第一张卡片。
        let mut ws = Workspace::empty();
        let id = ws.add(CardKind::History);
        assert_eq!(ws.get(id).expect("a").rect, Rect::ZERO);
        assert!(ws.hit_test(pos2(0.0, 0.0)).is_none(), "零尺寸卡片不应被命中");
    }

    #[test]
    fn hit_test_ignores_detached_cards() {
        let mut ws = Workspace::empty();
        let id = ws.add(CardKind::Detail);
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 200.0));
        ws.get_mut(id).expect("a").rect = r;
        assert!(ws.hit_test(pos2(50.0, 50.0)).is_some());
        ws.detach(id);
        assert!(
            ws.hit_test(pos2(50.0, 50.0)).is_none(),
            "分离出去的卡片不在主窗口工作区内，不应被命中"
        );
    }

    #[test]
    fn move_up_down_respect_bounds() {
        let mut ws = Workspace::empty();
        for k in CardKind::ALL {
            ws.add(k);
        }
        assert!(!ws.move_up(0), "已在最底层，上移应返回 false");
        assert!(!ws.move_down(3), "已在最顶层，下移应返回 false");
        assert!(ws.move_up(3));
        assert!(ws.move_down(0));
    }

    #[test]
    fn clear_rects_only_touches_docked_cards() {
        // 子窗口的矩形由它自己的客户区决定，清掉会让子窗口变空白。
        let mut ws = Workspace::empty();
        let docked = ws.add(CardKind::History);
        let detached = ws.add(CardKind::Detail);
        let r = Rect::from_min_size(pos2(1.0, 2.0), vec2(30.0, 40.0));
        ws.get_mut(docked).expect("d").rect = r;
        ws.get_mut(detached).expect("x").rect = r;
        ws.detach(detached);
        ws.clear_rects();
        assert_eq!(ws.get(docked).expect("d").rect, Rect::ZERO);
        assert_eq!(
            ws.get(detached).expect("x").rect,
            r,
            "分离卡片的矩形不能被主窗口清掉"
        );
    }

    #[test]
    fn pinned_mode_round_trips() {
        let mut ws = Workspace::default();
        assert_eq!(ws.pinned_mode(), PinnedMode::SharedColumn, "默认共用单栏");
        assert!(ws.set_pinned_mode(PinnedMode::OwnCard));
        assert_eq!(ws.pinned_mode(), PinnedMode::OwnCard);
    }
}
