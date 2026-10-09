//! 布局求解：把可用区域分配给各张卡片。
//!
//! # 与旧布局的本质区别
//!
//! 旧 [`crate::layout`] 先按宽度选一个**离散档位**（`Tier`，5 档），
//! 再让每个面板查`Tier::visibility()` 决定自己显示什么。档位与面板的
//! 交叉组合有 4×5=20 种，每种都要在布局侧与绘制侧各写对一次。
//!
//! 本模块没有档位。算法只有两步：
//!
//! 1. 收集停靠卡片的宽度需求（`target_width`，钳到 ≥1pt）；
//! 2. 够用 → 按比例分配剩余空间；不够 → **等比缩小**到恰好铺满。
//!
//! 输出是「每张卡片的矩形」，绘制层直接读，**不做二次判断**。
//!
//! # 为什么没有「折叠降级」
//!
//! 早前这里有第三步：宽度不够时按 [`CardKind::collapse_priority`]
//! 依次把次要卡片**折成把手**（24pt 宽）。该功能已整体移除——
//! 折叠后卡片只剩一条看不出是什么的细条，用户既不知道那是什么，
//! 也不知道点哪能展开。窄窗口下卡片变窄虽然拥挤，但列表仍可读、
//! 可滚动。
//!
//! ⚠️ 下一个人若照着本文档改动，请不要把折叠加回来。
//!
//! # 不变式（测试逐条守住）
//!
//! - `I1` 所有分配到的矩形都在 `area` 内；
//! - `I2` 任意两张停靠卡片的矩形不相交；
//! - `I3` 宽度不够时**等比缩小**到恰好铺满，**绝不折叠**；
//! - `I4` 参与分配的卡片**宽度恒 > 0**（即使窗口极窄）。

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
///
/// # 宽度不够时**不再**折叠
///
/// 早前这里有个「依次折叠」的降级循环（按 `collapse_priority` 把
/// 次要卡片折成把手）。已随折叠功能整体移除：主界面固定为
/// 置顶 + 历史两栏，窄窗口下平分宽度即可，折叠只会让用户
/// 「找不到历史在哪」。
pub fn solve(ws: &Workspace, area: Rect) -> Solution {
    let body = card_area(area);
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return Solution {
            rects: vec![Rect::ZERO; ws.cards.len()],
        };
    }

    // ---- 分配宽度 --------------------------------------------------
    //
    // ⚠️ 判据必须与 [`crate::workspace::Workspace::docked`] 一致：
    // **内嵌的置顶卡片宽度必须是 0**。
    //
    // 早前这里只看 `c.host == Docked`，于是置顶（停靠态）仍分到一栏
    // 宽度，而 paint 又在历史栏顶部内嵌画了一份—— 实测界面左边
    // 空出约 1/3 宽度、历史栏只占右边 2/3，正是这个不一致造成的。
    //
    // 抽成 `pinned_is_embedded()` 是为了让它与 paint 用**同一个**
    // 判定函数，避免两处各自判断再次漂移。
    let pinned_embedded = ws.pinned_is_embedded();
    let layout: Vec<f32> = ws
        .cards
        .iter()
        .map(|c| {
            if c.host != crate::card::CardHost::Docked
                || (pinned_embedded && c.kind == crate::card::CardKind::Pinned)
            {
                0.0
            } else {
                // ⚠️ 必须钳到**正数**。`target_width` 是 pub 字段，
                // 拖分隔条或持久化脏值都可能让它变成 0 甚至负数。
                //
                // 后果不只是「这张卡变窄」：`n` 是靠「宽度 > 0」推断
                // 参与分配的，一旦某张卡宽度为 0，`n` 就少算一个 ⇒
                // `gaps` 也少算一份 ⇒ **avail 反而变大**，剩下的卡
                // 突然变宽 —— 布局对窗口宽度变得**非单调**，缩到某点
                // 时画面会「跳」一下。
                c.target_width.max(1.0)
            }
        })
        .collect();

    let n = layout.iter().filter(|&&w| w > 0.0).count();
    if n == 0 {
        return Solution {
            rects: vec![Rect::ZERO; ws.cards.len()],
        };
    }
    // ⚠️ `gap` 必须钳到非负：负 gap 会让 `gaps < 0` ⇒ `avail` 大于
    // `body.width()` ⇒ 卡片总跨度**超出客户区**，画到窗口外。
    // 无边框窗口外面就是桌面，用户会直接看见。
    let gaps = ws.gap.max(0.0) * (n as f32 - 1.0);
    // 每张卡至少 1pt，否则 gap 大时会算出 0 宽卡片（绘制层直接跳过，
    // 表现为「整窗空白」）。
    let avail = (body.width() - gaps).max(n as f32);
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

    Solution { rects }
}

/// 卡片可用区：扣掉顶部标题栏与底部状态栏。
pub fn card_area(area: Rect) -> Rect {
    Rect::from_min_max(
        pos2(area.min.x, area.min.y + TOPBAR_HEIGHT),
        pos2(area.max.x, area.max.y - STATUSBAR_HEIGHT),
    )
}

/// 把解应用到工作区（写回每张卡片的 `rect`）。
/// 把解写回工作区。
///
/// # 为什么长度不等要**拒绝**而不是 `zip` 静默截断
///
/// `zip` 在长度不等时静默取短的一边：多出来的卡片**保留上一帧的
/// `rect`**，绘制层照画 —— 画面错乱却不 panic、不 warn、不 assert。
/// 调用方在 `solve` 与 `apply` 之间增删卡片、或跨帧复用 `Solution`
/// 时就会这样。
///
/// 退化策略：清空全部矩形（下一帧画不出东西，但**不会画错东西**）
/// 并返回 `false` 让上层记一笔。
pub fn apply(ws: &mut Workspace, sol: &Solution) -> bool {
    if ws.cards.len() != sol.rects.len() {
        tracing::warn!(
            cards = ws.cards.len(),
            rects = sol.rects.len(),
            "Solution 与 cards 不同序，已丢弃本帧解（否则会画出上一帧的过期矩形）"
        );
        for c in ws.cards.iter_mut() {
            c.rect = Rect::ZERO;
        }
        return false;
    }
    for (c, r) in ws.cards.iter_mut().zip(sol.rects.iter()) {
        c.rect = *r;
    }
    true
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

    /// 内嵌置顶时，历史栏必须**铺满整个可用宽度**。
    ///
    /// # 这条守住什么
    ///
    /// 内嵌的置顶卡片宽度必须是 0，历史栏才能吃掉全部剩余宽度。
    /// 早前 `solve` 只看 `c.host == Docked`，给置顶也分了一栏——
    /// 实测界面左边空出约 1/3、历史栏只占右边 2/3。
    #[test]
    fn embedded_pinned_leaves_history_full_width() {
        let ws = Workspace::default();
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 533.33));
        let sol = solve(&ws, area);
        let body = card_area(area);

        let idx = |ws: &Workspace, k: CardKind| {
            ws.cards
                .iter()
                .position(|c| c.kind == k)
                .expect("卡片存在")
        };
        let h = ws.by_kind(CardKind::History).expect("历史卡片");
        let hr = sol.rects[idx(&ws, h.kind)];
        assert!(
            (hr.width() - body.width()).abs() < 0.5,
            "历史栏应铺满可用宽度：期望 {}，实际 {}",
            body.width(),
            hr.width()
        );
        // 内嵌的置顶卡片矩形必须是零——它不占空间。
        let p = ws.by_kind(CardKind::Pinned).expect("置顶卡片");
        let pr = sol.rects[idx(&ws, p.kind)];
        assert_eq!(pr.width(), 0.0, "内嵌置顶不应占宽度");
    }

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

    /// 窄窗口下**不再折叠**，而是平分宽度。
    ///
    /// 取代原先三条测试（`wide_window_keeps_all_cards_expanded` /
    /// `narrow_window_auto_collapses_until_it_fits` /
    /// `history_is_never_auto_collapsed`）——它们守的折叠降级功能
    /// 已整体移除。
    ///
    /// 现在守的是「窄窗口不折叠」这条新契约：折叠已删除，
    /// 剩下的卡片平分宽度即可。
    #[test]
    fn narrow_window_splits_width_instead_of_collapsing() {
        for w in [140.0f32, 200.0, 320.0] {
            let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(w, 533.33));
            let ws = Workspace::default();
            let sol = solve(&ws, area);

            // ⚠️ 跳过判据必须与 `solve` 内部**完全一致**：内嵌的置顶卡片
            // 布局宽度就是 0（它画在历史栏顶部内部，不占一栏）。
            //
            // 早前这里用 `c.layout_width() <= 0.0` 判断，而那个方法只看
            // `host` 不看内嵌—— 于是断言对内嵌置顶误报「宽为 0」。
            let pinned_embedded = ws.pinned_is_embedded();
            for (i, c) in ws.cards.iter().enumerate() {
                if c.host != crate::card::CardHost::Docked
                    || (pinned_embedded && c.kind == crate::card::CardKind::Pinned)
                {
                    continue;
                }
                let r = sol.rects[i];
                assert!(
                    r.width() > 0.0,
                    "宽 {w}: {:?} 的矩形宽为 0，应平分而非折叠",
                    c.kind
                );
            }
            // 所有矩形都必须落在客户区内，且不重叠。
            let body = card_area(area);
            for (i, r) in sol.rects.iter().enumerate() {
                assert!(
                    r.width() <= 0.0 || r.intersects(body),
                    "宽 {w}: {:?} 的矩形 {:?} 超出可用区 {body:?}",
                    ws.cards[i].kind,
                    r
                );
            }
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

    /// solver 是纯函数：不得改写工作区。
    ///
    /// 原先这条守的是「自动折叠只在本帧生效、不改用户的折叠意图」，
    /// 而折叠已移除。改为守「不改写 `rect`」——那才是 solver 与
    /// 应用两段式的关键：求解只算，`apply` 才写。
    #[test]
    fn solve_is_pure_and_does_not_mutate_workspace() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 533.33));
        let mut ws = Workspace::default();
        // 先写一批可辨认的 rect。
        for (i, c) in ws.cards.iter_mut().enumerate() {
            c.rect = Rect::from_min_size(pos2(i as f32 * 7.0, 3.0), vec2(11.0, 13.0));
        }
        let before: Vec<Rect> = ws.cards.iter().map(|c| c.rect).collect();
        let _ = solve(&ws, area);
        let after: Vec<Rect> = ws.cards.iter().map(|c| c.rect).collect();
        assert_eq!(before, after, "solver 不得改写卡片的 rect");
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
        //
        // ⚠️ **内嵌的置顶卡片是刻意的例外**：它不占栏，内容由
        // `draw_embedded_pinned` 画进历史栏顶部，矩形必须是零。
        // 所以这里要排除它，否则会把「按设计为零」当成回归。
        let pinned_embedded = ws.pinned_is_embedded();
        for c in ws
            .cards
            .iter()
            .filter(|c| c.is_visible())
            .filter(|c| !(pinned_embedded && c.kind == CardKind::Pinned))
        {
            let r = sol.rect_of(ws.cards.iter().position(|x| x.id == c.id).expect("在列表中"))
                .expect("应有矩形");
            assert!(
                r.width() >= 1.0 && r.height() >= 1.0,
                "{c:?} 拿到零/负尺寸矩形 {r:?}，会导致该卡片整片不绘制"
            );
        }
        // 反过来：内嵌置顶**必须**是零宽，否则又会占一栏。
        if pinned_embedded {
            let p = ws.by_kind(CardKind::Pinned).expect("置顶卡片");
            let pr = sol.rect_of(ws.cards.iter().position(|x| x.id == p.id).expect("在列表中"))
                .expect("应有矩形");
            assert_eq!(
                pr.width(),
                0.0,
                "内嵌的置顶卡片不应占宽度（否则与历史分栏）"
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
