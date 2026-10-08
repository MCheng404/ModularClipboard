//! 布局求解：把可用区域分配给各张卡片。
//!
//! # 与旧布局的本质区别
//!
//! 旧 [`crate::layout`] 先按宽度选一个**离散档位**（`Tier`，5 档），
//! 再让每个面板查`Tier::visibility()` 决定自己显示什么。档位与面板的
//! 交叉组合有 4×5=20 种，每种都要在布局侧与绘制侧各写对一次。
//!
//! 本模块没有档位。算法只有三步：
//!
//! 1. 收集可见卡片的宽度需求；
//! 2. 够用 → 按`target_width` 比例分配剩余空间；
//! 3. 不够 → 按 [`CardKind::collapse_priority`] 依次折叠，每折叠一张
//!    就重新检查，够用即停。
//!
//! 输出是「每张卡片的矩形」，绘制层直接读，**不做二次判断**。
//!
//! # 不变式（测试逐条守住）
//!
//! - `I1` 所有分配到的矩形都在 `area` 内；
//! - `I2` 任意两张停靠卡片的矩形不相交；
//! - `I3` 宽度不够时折叠到够用为止，历史卡片永不折叠；
//! - `I4` 宽度足够时**不折叠任何可折叠卡片**（不无谓降级）。

use egui::{Rect, pos2, vec2};

use crate::workspace::Workspace;

/// 顶部标题栏高度（逻辑点）。卡片区从它下方开始。
///
/// 顶栏现在**只承载搜索框**（产品标题已移除），所以它不再是
/// 「标题 + 小控件」而是「一个主要输入区」——40pt 扣掉上下各
/// `space_sm` 后搜索框只剩 28pt，视觉上偏扁。抬到 44pt 让搜索框
/// 有 32pt 的净高，接近标准输入框的舒适区间。
pub const TOPBAR_HEIGHT: f32 = 44.0;

/// 底部状态栏高度（逻辑点）。
///
/// ⚠️ **已归零**：底部状态栏被移除（「清空 / 设置」上移到顶栏右侧，
/// 与搜索框并列）。保留常量是为了让 `card_area` 的算法结构不变——
/// 将来若要恢复底部栏，只需改这一个数字。
pub const STATUSBAR_HEIGHT: f32 = 0.0;

/// 求解结果：与 [`Workspace::cards`] **同序**的矩形列表。
///
/// 用 `Vec` 而不是往卡片里写，是为了让「求解」与「应用」分离——
/// 求解是纯函数，便于测试；应用只做一次批量赋值。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Solution {
    /// 每张卡片的矩形，索引与 `Workspace::cards` 对齐。
    pub rects: Vec<Rect>,
    /// 本次求解把哪些卡片折叠了（索引）。
    pub auto_collapsed: Vec<usize>,
}

impl Solution {
    /// 取某张卡片的矩形。
    pub fn rect_of(&self, index: usize) -> Option<Rect> {
        self.rects.get(index).copied()
    }
}

/// 在 `area` 内求解所有停靠卡片的矩形。
///
/// `area` 是**整个窗口客户区**（含标题栏与状态栏）；本函数自己
/// 扣掉它们，返回的是卡片可用区。
pub fn solve(ws: &Workspace, area: Rect) -> Solution {
    let body = card_area(area);
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return Solution {
            rects: vec![Rect::ZERO; ws.cards.len()],
            auto_collapsed: Vec::new(),
        };
    }

    // ---- 步骤 1：确定参与分配的卡片与初始折叠集 ----------------------
    //
    // 起点是「用户当前折叠状态」，但**不写回** `ws`：solver 是纯函数，
    // 自动折叠的结果只在本帧生效。用户的折叠意图在 `ws` 里保持不变。
    let mut collapsed: Vec<bool> = ws.cards.iter().map(|c| c.collapsed).collect();

    // 只有停靠且可折叠的卡片才可能被自动折叠。
    let collapsible: Vec<usize> = ws
        .cards
        .iter()
        .enumerate()
        .filter(|(_, c)| c.host == crate::card::CardHost::Docked && c.kind.collapsible())
        .map(|(i, _)| i)
        .collect();

    // ---- 步骤 2：不够就依次折叠 ----------------------------------------
    let mut auto_collapsed: Vec<usize> = Vec::new();
    // 按优先级从「先折叠」排到「后折叠」（数字大 = 先折叠）。
    let mut order = collapsible;
    order.sort_by_key(|&i| {
        std::cmp::Reverse(ws.cards[i].kind.collapse_priority())
    });

    let mut i = 0;
    while demand_of(ws, &collapsed) > body.width() && i < order.len() {
        let idx = order[i];
        i += 1;
        if collapsed[idx] {
            continue; // 已经折叠，跳过
        }
        collapsed[idx] = true;
        auto_collapsed.push(idx);
    }

    // ---- 步骤 3：分配宽度 ----------------------------------------------
    // 注意：即便历史卡片也放不下（窗口极窄），也不再继续折叠——
    // 它不可折叠。此时所有卡片会平分可用宽度，各自尽可能窄。
    let layout: Vec<f32> = ws
        .cards
        .iter()
        .enumerate()
        .map(|(idx, c)| {
            if c.host != crate::card::CardHost::Docked {
                0.0
            } else if collapsed[idx] {
                c.kind.handle_width()
            } else {
                c.target_width
            }
        })
        .collect();

    let n = layout.iter().filter(|&&w| w > 0.0).count();
    if n == 0 {
        return Solution {
            rects: vec![Rect::ZERO; ws.cards.len()],
            auto_collapsed,
        };
    }
    let gaps = ws.gap * (n as f32 - 1.0);
    let avail = (body.width() - gaps).max(0.0);
    let total: f32 = layout.iter().sum();

    // 需求 <= 可用：按 target_width 比例分剩余空间（让卡片保持用户设定的比例）。
    // 需求 > 可用（历史卡片都放不下的极端窄窗）：等分，各自尽可能窄。
    let widths: Vec<f32> = if total <= avail {
        let extra = avail - total;
        let total_f = if total > 0.0 { total } else { 1.0 };
        layout
            .iter()
            .map(|&w| if w > 0.0 { w + extra * (w / total_f) } else { 0.0 })
            .collect()
    } else {
        // 需求 > 可用（历史卡片都放不下的极端窄窗）：按需求比例缩到刚好装下。
        //
        // ⚠️ 不能给每张卡加一个「最小可见宽度」下限：60pt 宽的窗口里
        // 4 张卡各8pt 就是 32pt 需求，但gaps 已经是 18pt，
        // 硬下限会让总宽**超出**可用区，卡片被推出客户区右缘。
        // 那种情况下宁可让卡片窄到 1pt，也不能越界——越界会画到
        // 窗口外（无边框窗口外面就是桌面）。
        let total_f = if total > 0.0 { total } else { 1.0 };
        let mut scaled: Vec<f32> = layout
            .iter()
            .map(|&w| if w > 0.0 { (w / total_f * avail).max(0.0) } else { 0.0 })
            .collect();
        // 浮点与逐项 max 可能让总和略超 avail，强制按比例回缩。
        let sum: f32 = scaled.iter().sum();
        if sum > avail && sum > 0.0 {
            let k = avail / sum;
            for w in scaled.iter_mut() {
                *w *= k;
            }
        }
        scaled
    };

    // ---- 步骤 4：落位 --------------------------------------------------
    let mut rects = vec![Rect::ZERO; ws.cards.len()];
    let mut x = body.min.x;
    for (idx, &w) in widths.iter().enumerate() {
        if w > 0.0 {
            rects[idx] = Rect::from_min_size(pos2(x, body.min.y), vec2(w, body.height()));
            x += w + ws.gap;
        }
    }

    Solution {
        rects,
        auto_collapsed,
    }
}

/// 卡片可用区：扣掉顶部标题栏与底部状态栏。
pub fn card_area(area: Rect) -> Rect {
    Rect::from_min_max(
        pos2(area.min.x, area.min.y + TOPBAR_HEIGHT),
        pos2(area.max.x, area.max.y - STATUSBAR_HEIGHT),
    )
}

/// 当前折叠状态下的总需求宽度（含间隔）。
fn demand_of(ws: &Workspace, collapsed: &[bool]) -> f32 {
    let ws_gap = ws.gap;
    let ws_cards = &ws.cards;
    let n = ws_cards
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            c.host == crate::card::CardHost::Docked && !(collapsed[*i] && c.kind.collapsible())
        })
        .count();
    if n == 0 {
        return 0.0;
    }
    let sum: f32 = ws_cards
        .iter()
        .enumerate()
        .filter(|(_, c)| c.host == crate::card::CardHost::Docked)
        .map(|(i, c)| {
            if collapsed[i] && c.kind.collapsible() {
                c.kind.handle_width()
            } else {
                c.target_width
            }
        })
        .sum();
    sum + ws_gap * (n as f32 - 1.0)
}

/// 把解应用到工作区（写回每张卡片的 `rect`）。
pub fn apply(ws: &mut Workspace, sol: &Solution) {
    for (c, r) in ws.cards.iter_mut().zip(sol.rects.iter()) {
        c.rect = *r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::CardKind;
    use crate::workspace::Workspace;
    /// 检查解满足三条不变式，返回违反的描述列表（空 = 全部满足）。
    fn check(ws: &Workspace, sol: &Solution, area: Rect) -> Vec<String> {
        let mut bad = Vec::new();
        let body = card_area(area);

        if sol.rects.len() != ws.cards.len() {
            bad.push(format!(
                "解长度 {} != 卡片数 {}",
                sol.rects.len(),
                ws.cards.len()
            ));
            return bad;
        }

        // I1：都在区内
        for (c, r) in ws.cards.iter().zip(sol.rects.iter()) {
            if c.host != crate::card::CardHost::Docked {
                continue;
            }
            if r.width() <= 0.0 && r.height() <= 0.0 {
                continue; // 未参与分配
            }
            if r.min.x < body.min.x - 0.01
                || r.max.x > body.max.x + 0.01
                || r.min.y < body.min.y - 0.01
                || r.max.y > body.max.y + 0.01
            {
                bad.push(format!("{c:?} 矩形 {:?} 越出卡片区 {body:?}", r));
            }
        }

        // I2：两两不相交
        let docked: Vec<(usize, Rect)> = ws
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| c.host == crate::card::CardHost::Docked)
            .map(|(i, _)| (i, sol.rects[i]))
            .filter(|(_, r)| r.width() > 0.0 && r.height() > 0.0)
            .collect();
        for a in 0..docked.len() {
            for b in (a + 1)..docked.len() {
                let (ia, ra) = docked[a];
                let (ib, rb) = docked[b];
                // ⚠️ 判据是「重叠量 > 0」，**不是** `Rect::intersects`。
                //
                // `gap = 0` 时相邻卡片**边界恰好相接**（A 的右边 == B 的左边）。
                // `Rect::intersects` 对这种「边贴边」返回 **true**，
                // 于是测试在每一档宽度都报相交，而实际画面完全正常——
                // 假失败会让人去改本来正确的求解器。
                //
                // 浮点上更要留一点余量：分配里有乘除，`7.8` 这类值
                // 相接时可能有 1e-6 级误差。取 0.01pt 阈值。
                let ov_x =
                    (ra.max.x.min(rb.max.x) - ra.min.x.max(rb.min.x)).max(0.0);
                let ov_y =
                    (ra.max.y.min(rb.max.y) - ra.min.y.max(rb.min.y)).max(0.0);
                let overlap = ov_x.min(ov_y);
                if overlap > 0.01 {
                    bad.push(format!(
                        "卡片 {ia} {ra:?} 与 {ib} {rb:?} 真正重叠 {overlap:.3}pt"
                    ));
                }
            }
        }
        bad
    }

    /// 覆盖 100..1200 逻辑宽，高度固定 533.33（800 物理 / 1.5）。
    const SCAN_W: &[f32] = &[
        60.0, 100.0, 140.0, 190.0, 224.0, 260.0, 300.0, 320.0, 350.0, 400.0, 450.0, 500.0, 560.0,
        600.0, 700.0, 800.0, 900.0, 1000.0, 1200.0,
    ];

    #[test]
    fn invariants_hold_across_widths() {
        // I1 + I2：任意宽度下矩形都在区内且互不相交。
        let mut fails = Vec::new();
        for &w in SCAN_W {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 533.33));
            let ws = Workspace::default();
            let sol = solve(&ws, area);
            for msg in check(&ws, &sol, area) {
                fails.push(format!("宽 {w}: {msg}"));
            }
        }
        assert!(fails.is_empty(), "以下宽度违反不变式：\n{}", fails.join("\n"));
    }

    #[test]
    fn wide_window_keeps_all_cards_expanded() {
        // I4：宽度足够时不折叠任何可折叠卡片。
        // 1200 逻辑宽（= 1800 物理）远大于总需求。
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 533.33));
        let ws = Workspace::default();
        let sol = solve(&ws, area);
        assert!(
            sol.auto_collapsed.is_empty(),
            "宽窗口不应自动折叠，却折叠了 {:?}",
            sol.auto_collapsed
                .iter()
                .map(|&i| ws.cards[i].kind)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn narrow_window_auto_collapses_until_it_fits() {
        // I3：不够就折叠到够用。
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 533.33));
        let ws = Workspace::default();
        let sol = solve(&ws, area);
        assert!(
            !sol.auto_collapsed.is_empty(),
            "200pt 宽放不下四张卡片，必须自动折叠"
        );
        // 折叠顺序：置顶先于侧栏，侧栏先于详情。
        let kinds: Vec<CardKind> = sol
            .auto_collapsed
            .iter()
            .map(|&i| ws.cards[i].kind)
            .collect();
        let pos_of = |k: CardKind| kinds.iter().position(|&x| x == k);
        if let (Some(p), Some(d)) = (pos_of(CardKind::Pinned), pos_of(CardKind::Detail)) {
            assert!(p < d, "置顶应比详情先折叠，实际顺序 {kinds:?}");
        }
    }

    #[test]
    fn history_is_never_auto_collapsed() {
        // I3：历史卡片永不折叠。
        for &w in SCAN_W {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 533.33));
            let ws = Workspace::default();
            let sol = solve(&ws, area);
            let hist_idx = ws
                .cards
                .iter()
                .position(|c| c.kind == CardKind::History)
                .expect("默认工作区含历史卡片");
            assert!(
                !sol.auto_collapsed.contains(&hist_idx),
                "宽 {w}: 历史卡片被自动折叠了"
            );
        }
    }

    #[test]
    fn zero_area_yields_zero_rects_not_panic() {
        // 窗口最小化时客户区是 0x0，绝不能 panic 或算出 NaN。
        for (w, h) in [(0.0, 0.0), (0.0, 533.0), (533.0, 0.0), (-1.0, -1.0)] {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h));
            let ws = Workspace::default();
            let sol = solve(&ws, area);
            assert_eq!(sol.rects.len(), ws.cards.len());
            for r in &sol.rects {
                assert_eq!(*r, Rect::ZERO, "零面积区域应产出零矩形");
            }
        }
    }

    #[test]
    fn detached_cards_get_zero_rects() {
        // 分离出去的卡片由子窗口自己管矩形，主窗口必须给它零。
        //
        // ⚠️ 必须**自己 add** 一张 Detail：`Workspace::default()`
        // 只含「置顶 + 历史」两栏（视图/详情暂时不默认创建），
        // 从 default 里找Detail 会得到 None。
        let mut ws = Workspace::default();
        let id = ws.add(CardKind::Detail);
        ws.detach(id);
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 533.33));
        let sol = solve(&ws, area);
        let idx = ws
            .cards
            .iter()
            .position(|c| c.id == id)
            .expect("卡片仍在列表中");
        assert_eq!(sol.rects[idx], Rect::ZERO, "分离卡片应得零矩形");
    }

    #[test]
    fn solve_is_pure_and_does_not_mutate_workspace() {
        // 自动折叠只是本帧的显示决策，不能改掉用户的折叠意图。
        // 否则窗口拉宽后卡片不会自动恢复——这正是旧布局的痛点之一。
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 533.33));
        let ws = Workspace::default();
        let before: Vec<bool> = ws.cards.iter().map(|c| c.collapsed).collect();
        let _ = solve(&ws, area);
        let after: Vec<bool> = ws.cards.iter().map(|c| c.collapsed).collect();
        assert_eq!(before, after, "solver 不得修改工作区的折叠状态");
    }

    #[test]
    fn apply_writes_rects_back_in_order() {
        let mut ws = Workspace::default();
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 533.33));
        let sol = solve(&ws, area);
        apply(&mut ws, &sol);
        for (c, r) in ws.cards.iter().zip(sol.rects.iter()) {
            assert_eq!(c.rect, *r, "apply 应按同序写回矩形");
        }
    }

    #[test]
    fn all_cards_get_a_visible_rect_at_usable_width() {
        // 关键回归：曾出现「折叠态矩形无人绘制」导致整片空白。
        // 这里守住：宽度够时每张停靠卡片都有非零矩形。
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 533.33));
        let ws = Workspace::default();
        let sol = solve(&ws, area);
        for c in ws.cards.iter().filter(|c| c.is_visible()) {
            let r = sol.rect_of(ws.cards.iter().position(|x| x.id == c.id).expect("在列表中"))
                .expect("应有矩形");
            assert!(
                r.width() >= 1.0 && r.height() >= 1.0,
                "{c:?} 拿到零/负尺寸矩形 {r:?}，会导致该卡片整片不绘制"
            );
        }
    }

    #[test]
    fn cards_tile_left_to_right_without_gaps_left_unfilled() {
        // 分配应当铺满整行：最后一张卡的右缘应贴近卡片区右缘。
        // 若明显留白，说明分配公式与 gaps 重复或漏算。
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 533.33));
        let ws = Workspace::default();
        let sol = solve(&ws, area);
        let body = card_area(area);
        let rightmost = sol
            .rects
            .iter()
            .filter(|r| r.width() > 0.0)
            .map(|r| r.max.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let slack = body.max.x - rightmost;
        assert!(
            slack >= -0.01 && slack <= 0.51,
            "分配后右侧剩余 {slack:.2}pt：应≈0（末端有半个gap）或恰好铺满"
        );
    }
}
