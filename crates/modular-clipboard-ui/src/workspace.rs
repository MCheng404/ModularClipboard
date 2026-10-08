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
        // 默认卡片集：只有「置顶 + 历史」两栏，都停靠在主窗口。
        //
        // ⚠️ 「视图」（`Rail`）与「详情」（`Detail`）**暂时移除**。
        //
        // 它们作为独立子窗口存在过，但实测问题较多：
        //   - 子窗口默认位置与主窗口重叠，遮挡主界面；
        //   - 关闭子窗口后卡片状态与窗口状态容易不同步；
        //   - 每窗口一套 `FrameRenderer` + `Painter`（含独立字体图集）
        //     的显存开销在这个体量下并不划算。
        //
        // **枚举、绘制代码、solver 分支全部保留**，只是不再实例化——
        // 将来要恢复，改回这里加两行即可，不必重写。
        for (kind, host) in [
            (CardKind::Pinned, CardHost::Docked),
            (CardKind::History, CardHost::Docked),
        ] {
            let id = ws.add(kind);
            ws.get_mut(id).expect("刚加入的卡片").host = host;
        }
        ws
    }

}

impl Workspace {
    /// 置顶内容当前是否**内嵌在历史栏里**（而非独立成窗/独立分栏）。
    ///
    /// # 为什么需要这个判定
    ///
    /// 用户要求「没有置顶窗口时在历史栏显示置顶，不用分栏」。
    /// 于是置顶有**三种**呈现方式：
    ///
    /// | 状态 | 置顶在哪 | 主窗口画什么 |
    /// |---|---|---|
    /// | 独立成窗 | 自己的窗口 | 只画历史栏 |
    /// | 停靠（内嵌） | 历史栏顶部 | 历史栏 + 内嵌置顶区 |
    ///
    /// solver 与 paint **都必须**问同一个问题，否则会出现
    /// 「solver 给置顶分配了一栏宽度、paint 又在历史栏里画一份」
    /// —— 实测正是这样，界面上置顶与历史并排出现，
    /// 而用户要的是不分栏。
    pub fn pinned_is_embedded(&self) -> bool {
        self.cards
            .iter()
            .find(|c| c.kind == CardKind::Pinned)
            .is_some_and(|c| c.host == CardHost::Docked)
    }

    /// 把配置里持久化的窗口位置写回卡片。
    ///
    /// 必须在每次启动时调一次：置顶窗口独立成窗时靠它回到上次位置，
    /// 否则每次启动都回兜底值（主窗口左侧），用户拖过的地方丢失。
    ///
    /// 只写位置，**不写** `host` —— 是否独立成窗是运行期的用户选择，
    /// 不该跨重启恢复（否则用户关掉置顶窗，下次启动它又自己冒出来）。
    pub fn apply_saved_positions(&mut self, ui: &modular_clipboard_core::UiConfig) {
        if let Some(p) = ui.pinned_window_pos {
            if let Some(c) = self
                .cards
                .iter_mut()
                .find(|c| c.kind == CardKind::Pinned)
            {
                c.window_pos = egui::epaint::emath::vec2(p.x, p.y);
            }
        }
    }
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
        let mut card = Card::new(id, kind);
        // ⚠️ 每个窗口型卡片的默认位置必须**互相错开**。
        //
        // 早前所有卡片的 `window_pos` 都是 (120,120)，于是视图与详情
        // 两个子窗口完全重叠、后创建的压在前者上面。用户看到的
        // 「界面有黑区、一半内容不见了」其实只是窗口叠在一起——
        // 像素采样证实每个窗口**自身**渲染是完整的（背景占比 86%）。
        //
        // 按已有卡片数递增偏移：够错开，又不至于散得太开。
        //
        // ⚠️ 偏移基准改成 **(620, 80)** 而不是 (120,120)：
        // 主窗口默认 420x560、位于屏幕左上，子窗口若也从 (120,120)
        // 附近起就会**盖在主窗口上**——截图脚本只截主窗口矩形，
        // 于是画面里全是被遮挡的主窗口，看起来像「界面坏了」。
        //
        // 620 已越过主窗口右缘（420），80 让第一个子窗口露出上边。
        //置顶窗口默认摆在主窗口**左边**：主窗口约 420 物理像素宽，
        //置顶窗约 300宽，两者并排比上下堆叠更好读。
        //
        // ⚠️ 主窗口位置**不是常量**（用户可以拖到屏幕任何地方），
        // 所以真正的定位在 `childwin::ChildWindows::sync_with`里做——
        // 它拿得到主窗口句柄与真实矩形。这里只给一个兜底值，
        // 避免 `None` 时落在 (0,0)（屏幕左上角，与主窗重叠）。
        //
        // 兜底值取主窗口的常见位置（居中偏左），不追求精确——
        // 首帧之后会被真实位置覆盖。
        let n = self.cards.len() as f32;
        card.window_pos = egui::epaint::emath::vec2(120.0 + n * 40.0, 120.0);
        self.cards.push(card);
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
    /// 停靠卡片。
    ///
    /// ⚠️ **不含内嵌的置顶卡片**。
    ///
    /// 置顶停靠时不单独占一栏（用户要求「不用分栏」），
    /// 它由 [`Self::pinned_is_embedded`] 单独识别、
    /// 画进历史栏顶部。solver 用本方法算总需求宽度——
    /// 若这里仍返回置顶，就会「solver 分了一栏、paint 又内嵌画一份」，
    /// 实测界面就是这样（置顶与历史并排 + 历史栏内又一份）。
    pub fn docked(&self) -> impl DoubleEndedIterator<Item = &Card> {
        let pinned_embedded = self.pinned_is_embedded();
        self.cards
            .iter()
            .filter(move |c| {
                c.host == CardHost::Docked
                    && !(pinned_embedded && c.kind == CardKind::Pinned)
            })
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
    fn docked_pinned_does_not_occupy_a_column() {
        // 置顶停靠时**不占一栏**（用户要求「不用分栏」）。
        //
        // 若 solver 的 `docked()` 仍返回置顶，就会分给它一栏宽度，
        // 界面上出现「置顶 | 历史」并排——实测正是如此。
        let ws = Workspace::default();
        assert!(ws.pinned_is_embedded(), "置顶默认停靠 ⇒ 视为内嵌");
        let kinds: Vec<_> = ws.docked().map(|c| c.kind).collect();
        assert!(
            !kinds.contains(&CardKind::Pinned),
            "停靠卡片里不应含置顶（它内嵌在历史栏顶部），实际={kinds:?}"
        );
        assert!(kinds.contains(&CardKind::History), "历史栏必须在");
    }

    #[test]
    fn separated_pinned_also_excluded_from_columns() {
        // 置顶独立成窗时同样不占主窗口的栏。
        let mut ws = Workspace::default();
        let id = ws.by_kind(CardKind::Pinned).expect("置顶").id;
        ws.detach(id);
        assert!(!ws.pinned_is_embedded(), "独立成窗后不再是内嵌态");
        assert!(
            !ws.docked().any(|c| c.kind == CardKind::Pinned),
            "独立成窗后置顶不该出现在停靠列里"
        );
    }

    #[test]
    fn saved_pinned_position_is_restored_on_startup() {
        // 用户拖动置顶窗口后，重启必须回到同一位置。
        //
        // 早前位置只存在内存里，重启就丢——用户每次都要重新摆。
        let mut ws = Workspace::default();
        let mut ui = modular_clipboard_core::UiConfig::default();
        ui.pinned_window_pos = Some(modular_clipboard_core::WindowPos::new(333.0, 222.0));

        ws.apply_saved_positions(&ui);

        let pinned = ws.by_kind(CardKind::Pinned).expect("置顶卡片");
        assert_eq!(
            pinned.window_pos,
            vec2(333.0, 222.0),
            "配置里的置顶窗口位置应被写回卡片"
        );
    }

    #[test]
    fn missing_saved_position_keeps_default() {
        // 没有保存过位置时**不能**动卡片——否则会把兜底值
        // （主窗左侧的计算依据）改掉。
        let mut ws = Workspace::default();
        let before = ws.by_kind(CardKind::Pinned).expect("置顶卡片").window_pos;
        ws.apply_saved_positions(&modular_clipboard_core::UiConfig::default());
        let after = ws.by_kind(CardKind::Pinned).expect("置顶卡片").window_pos;
        assert_eq!(before, after, "无保存位置时不应改动");
    }

    #[test]
    fn saved_position_does_not_force_window_host() {
        // ⚠️ 位置持久化与「是否独立成窗」是**两件事**。
        //
        // 用户关掉置顶窗口就是不想看到它；下次启动若因为恢复了
        // 位置就把它重新拉起来，等于用户的关闭操作被无视了。
        let mut ws = Workspace::default();
        let mut ui = modular_clipboard_core::UiConfig::default();
        ui.pinned_window_pos = Some(modular_clipboard_core::WindowPos::new(50.0, 60.0));
        ws.apply_saved_positions(&ui);
        assert!(
            ws.by_kind(CardKind::Pinned)
                .expect("置顶卡片")
                .host
                == CardHost::Docked,
            "恢复位置不应把停靠卡片变成独立窗口"
        );
    }

    #[test]
    fn default_workspace_has_pinned_and_history_only() {
        // ⚠️ 「视图」（Rail）与「详情」（Detail）**暂时不在默认集里**
        // ——作为独立子窗口的收益不抵它的遮挡与状态同步成本。
        // 枚举与代码都保留，将来恢复时改 `Workspace::default()` 即可，
        // 这个断言也要跟着改回来。
        let ws = Workspace::default();
        for k in [CardKind::Pinned, CardKind::History] {
            assert!(ws.by_kind(k).is_some(), "默认工作区应含{k:?}卡片");
        }
        for k in [CardKind::Rail, CardKind::Detail] {
            assert!(
                ws.by_kind(k).is_none(),
                "{k:?} 已暂时移除，不应出现在默认工作区"
            );
        }
        // 默认卡片全部停靠在主窗口，不该有分离出去的。
        assert!(
            ws.cards.iter().all(|c| c.host == CardHost::Docked),
            "默认工作区不应含分离卡片"
        );
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
            ws.add(k);
        }
        let n = ws.docked().count() as f32;
        // ⚠️ `sum` 必须与 `n` **取自同一来源**：都用 `docked()`。
        //
        // 早前这里写 `CardKind::ALL.iter().map(min_width)`—— 那是
        // 「全部四种卡片」的宽度和，而 `n` 是 `docked()` 的数量。
        // 置顶内嵌后 `docked()` 少一张，两者口径不一致，
        // 期望值凭空多出一张卡的宽度（实测 560 vs 440）。
        let sum: f32 = ws.docked().map(|c| c.kind.min_width()).sum();
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

    /// 收回工作区是幂等的。
    ///
    /// 原先这条守的是「收回时自动展开折叠卡片」——折叠已移除，
    /// 现在只守 `detach` / `dock` 往返本身。
    #[test]
    fn detach_and_dock_roundtrip_is_idempotent() {
        let mut ws = Workspace::empty();
        let id = ws.add(CardKind::Detail);
        assert!(ws.detach(id), "首次分离应返回 true");
        assert!(!ws.detach(id), "重复分离应返回 false（幂等）");
        assert!(ws.dock(id), "首次收回应返回 true");
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
