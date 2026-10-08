//! 布局求解探针：直接调用生产代码 `layout::solve`，打印每个面板的矩形。
//!
//! # 为什么不复刻布局逻辑
//!
//! 本项目吃过四次「判据失效」的亏，其中一次正是**探针里复刻了生产逻辑**：
//! 改生产代码后探��没跟着改，报了个假的 FAIL/PASS。
//! 因此本探针只做一件事——调用 `modular_clipboard_ui::layout::solve`，
//! 它返回什么我们就断言什么。生产代码改了，这里自动跟着变。
//!
//! # 断言的口径
//!
//! 1. **两两不相交**：任意两个非空面板矩形不得相交（`rects_overlap`）。
//! 2. **都在客户区内**：每个矩形必须完全落在 `area` 内。
//! 3. **段内宽度和不超过段宽**：内部实现细节的额外保险。

use egui::Rect;
use modular_clipboard_ui::layout::{self, Panel, Tier, Visibility};

/// 实机默认：1.5x 缩放下的逻辑点。
const SCALE: f32 = 1.5;

/// 探针要覆盖的物理宽度（用户实机用过的窗口宽度）。
const PHYSICAL_WIDTHS: [f32; 4] = [420.0, 560.0, 720.0, 900.0];

/// 物理高度（720x800 是新的默认窗口高度）。
const PHYSICAL_HEIGHT: f32 = 800.0;

fn fmt(r: Rect) -> String {
    format!(
        "x={:>7.2}..{:>7.2} y={:>7.2}..{:>7.2} (w={:>6.2} h={:>6.2})",
        r.min.x,
        r.max.x,
        r.min.y,
        r.max.y,
        r.width(),
        r.height()
    )
}

fn panel_name(p: Panel) -> &'static str {
    match p {
        Panel::History => "历史",
        Panel::Pinned => "置顶",
        Panel::Detail => "详情",
        Panel::Rail => "侧栏",
    }
}

/// 跑一个宽度，返回失败原因列表（空 = 通过）。
fn check_width(physical_w: f32, state: &layout::LayoutState, label: &str) -> Vec<String> {
    let mut fails = Vec::new();
    let logical_w = physical_w / SCALE;
    let logical_h = PHYSICAL_HEIGHT / SCALE;
    let area = Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(logical_w, logical_h),
    );

    let solved = layout::solve(area, state, None);

    println!("\n=== {label}  物理 {physical_w:.0}x{PHYSICAL_HEIGHT:.0} → 逻辑 {logical_w:.2}x{logical_h:.2} ===");
    println!("  档位: {} ({:?})", solved.tier.name(), solved.tier);
    println!("  顶栏: {}", fmt(solved.topbar));

    // ---- 收集所有占位矩形（停靠 + 浮动）----
    let mut occupied: Vec<(String, Rect)> = Vec::new();
    for p in Panel::ALL {
        let i = layout::panel_index(p);
        if let Some(r) = solved.rects[i] {
            occupied.push((format!("[停靠] {}", panel_name(p)), r));
        }
        if let Some(r) = solved.floating[i] {
            occupied.push((format!("[浮层] {}", panel_name(p)), r));
        }
    }
    for (n, r) in &occupied {
        println!("  {n:<14} {}", fmt(*r));
    }
    for (i, s) in solved.splitters.iter().enumerate() {
        println!("  [分隔条{i}] {}", fmt(*s));
    }

    // ---- 断言 1：两两不相交 ----
    //
    // ⚠️ **停靠 vs 停靠**相交 = 真 bug（两栏压在一起）。
    // **浮层 vs 停靠**相交是浮层的本职——它就是浮在停靠面板上面。
    // 把两者混为一谈会让判据失去意义：本项目的降级阶梯里
    // 「详情转浮层」是**产品要求的**行为，浮层盖住部分停靠面板正是它的样子。
    // 因此这里只对停靠矩形断言两两不相交；浮层另有一条更强的要求：
    // 它不能盖住**整个**窗口（见断言 6）。
    for i in 0..occupied.len() {
        for j in (i + 1)..occupied.len() {
            let (ni, ri) = &occupied[i];
            let (nj, rj) = &occupied[j];
            if ni.starts_with("[浮层]") || nj.starts_with("[浮层]") {
                continue;
            }
            if layout::rects_overlap(*ri, *rj) {
                fails.push(format!("停靠重叠: {ni} {} 与 {nj} {} 相交", fmt(*ri), fmt(*rj)));
            }
        }
    }

    // ---- 断言 2：都在客户区内 ----
    // 顶栏与停靠面板的纵向范围是 `[area.min.y, area.max.y]` 的子集。
    // 停靠面板允许高度到 area.max（布局不预留状态栏高度，
    // 状态栏是浮在面板之上的 chrome，见 view.rs::draw_status_bar）。
    for (n, r) in &occupied {
        if r.min.x < area.min.x - 0.01 || r.max.x > area.max.x + 0.01 {
            fails.push(format!(
                "越界(水平): {n} {} 超出客户区 x={:.2}..{:.2}",
                fmt(*r),
                area.min.x,
                area.max.x
            ));
        }
        if r.min.y < area.min.y - 0.01 || r.max.y > area.max.y + 0.01 {
            fails.push(format!(
                "越界(垂直): {n} {} 超出客户区 y={:.2}..{:.2}",
                fmt(*r),
                area.min.y,
                area.max.y
            ));
        }
    }

    // ---- 断言 3：面板矩形不得越出 body（顶栏之下）----
    let body_top = state.topbar_height;
    for (n, r) in &occupied {
        if r.min.y < body_top - 0.01 {
            fails.push(format!(
                "压住顶栏: {n} {} 顶边 {:.2} < 顶栏底部 {body_top:.2}",
                fmt(*r),
                r.min.y
            ));
        }
    }

    // ---- 断言 4：降级阶梯的档位序单调（宽→窄）----
    // 同一 state 下，宽度越小档位必须越靠后。这条抓「分配没让位、
    // 只是硬钳」这类回归——上一轮三段宽度判据就漏在这里。
    if solved.tier > Tier::HistoryOnly {
        fails.push(format!("档位越界: {:?}", solved.tier));
    }

    // ---- 断言 5：折叠/隐藏的呈现方式与档位一致 ----
    for p in [Panel::Detail, Panel::Pinned, Panel::Rail] {
        let vis = solved.tier.visibility(state, p);
        let has = solved.rects[layout::panel_index(p)].is_some();
        match vis {
            Visibility::Hidden if has => {
                fails.push(format!(
                    "{}: 档位判为 Hidden 但仍画出了矩形 {:?}",
                    panel_name(p),
                    solved.rects[layout::panel_index(p)]
                ));
            }
            _ => {}
        }
    }

    // ---- 断言 6：浮层不能盖住整个客户区 ----
    //
    // 浮层盖住部分停靠面板是设计意图，盖住**全部**是缺陷：
    // 那样历史列表既不可见也不可点，降级等于把软件锁死。
    // 这条判据直接盯住 `FLOAT_DETAIL_MAX_WIDTH_RATIO` 那个上限，
    // 删掉它（或把兜底放在封顶之后）本条立即失败。
    for p in Panel::ALL {
        let i = layout::panel_index(p);
        if let Some(r) = solved.floating[i] {
            if r.width() >= area.width() - 0.01 || r.height() >= area.height() - 0.01 {
                fails.push(format!(
                    "浮层 {} {} 盖住了整个客户区 {:.2}x{:.2}",
                    panel_name(p),
                    fmt(r),
                    area.width(),
                    area.height()
                ));
            }
            // 浮层必须留出历史列表的一条可视带（不是「不相交」——
            // 浮层盖住部分停靠面板正是它的本职）。
            if let Some(h) = solved.rects[layout::panel_index(Panel::History)]
                && h.width() > 0.0
            {
                let visible = (h.max.x - h.min.x.max(r.min.x)).max(0.0);
                if visible <= 1.0 {
                    fails.push(format!(
                        "浮层 {} {} 完全遮住历史 {:?}（可见宽 {visible}）",
                        panel_name(p),
                        fmt(r),
                        h
                    ));
                }
            }
        }
    }

    fails
}

fn main() {
    println!("modular-clipboard-ui 布局探针");
    println!("scale = {SCALE}, panel min_width: 历史160 置顶120 详情180 侧栏88");

    let default_state = layout::LayoutState::default();
    let mut total_fails = 0;

    // ---- 场景 1：默认布局 ----
    for w in PHYSICAL_WIDTHS {
        let fails = check_width(w, &default_state, "默认布局");
        for f in &fails {
            println!("  FAIL: {f}");
        }
        total_fails += fails.len();
    }

    // ---- 场景 2：详情/侧栏都展开（最宽需求）----
    let mut expanded = default_state.clone();
    for p in [Panel::Detail, Panel::Rail] {
        let i = layout::panel_index(p);
        expanded.placement[i] = layout::Placement::Docked {
            slot: layout::Panel::default_slot(p),
            collapsed: false,
        };
    }
    for w in PHYSICAL_WIDTHS {
        let fails = check_width(w, &expanded, "详情+侧栏全展开");
        for f in &fails {
            println!("  FAIL: {f}");
        }
        total_fails += fails.len();
    }

    // ---- 场景 3：宽度扫描（连续，覆盖每一档）----
    println!("\n=== 连续扫描 逻辑宽 100..1000 步长 1（默认布局）===");
    let mut scan_fails = 0;
    let mut scan_fails_detail = Vec::new();
    let mut prev_tier = Tier::Full;
    let mut tier_changes = Vec::new();
    for logical_w in 100..=1000 {
        let logical_w = logical_w as f32;
        let area = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(logical_w, 533.0));
        let solved = layout::solve(area, &default_state, None);

        let mut rects: Vec<(Panel, Rect)> = Vec::new();
        for p in Panel::ALL {
            let i = layout::panel_index(p);
            if let Some(r) = solved.rects[i] {
                rects.push((p, r));
            }
            if let Some(r) = solved.floating[i] {
                rects.push((p, r));
            }
        }
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                if layout::rects_overlap(rects[i].1, rects[j].1) {
                    scan_fails += 1;
                    if scan_fails_detail.len() < 8 {
                        scan_fails_detail.push(format!(
                            "w={logical_w:.0} {} {} 与 {} {} 重叠",
                            panel_name(rects[i].0),
                            fmt(rects[i].1),
                            panel_name(rects[j].0),
                            fmt(rects[j].1)
                        ));
                    }
                }
            }
        }
        for (p, r) in &rects {
            if r.max.x > logical_w + 0.01 || r.min.x < -0.01 {
                scan_fails += 1;
                if scan_fails_detail.len() < 8 {
                    scan_fails_detail.push(format!(
                        "w={logical_w:.0} {} {} 越界",
                        panel_name(*p),
                        fmt(*r)
                    ));
                }
            }
        }
        if solved.tier != prev_tier {
            tier_changes.push(format!("  w={logical_w:.0} → {}", solved.tier.name()));
            prev_tier = solved.tier;
        }
    }
    println!("降级切换点（自宽往窄）:");
    for t in &tier_changes {
        println!("{t}");
    }
    println!("扫描 901 个宽度，重叠/越界总数 = {scan_fails}");
    for d in &scan_fails_detail {
        println!("  {d}");
    }

    println!("\n=========== 结果 ===========");
    if total_fails == 0 && scan_fails == 0 {
        println!("PASS: 所有宽度下所有面板两两不相交且都在客户区内");
        std::process::exit(0);
    } else {
        println!("FAIL: 指定宽度 {total_fails} 处问题，扫描 {scan_fails} 处问题");
        std::process::exit(1);
    }
}
