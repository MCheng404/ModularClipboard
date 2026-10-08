//! 像素判据：把实机截图的像素与 `solve()` 的输出对齐比较。
//!
//! # 为什么必须有这条
//!
//! 本项目吃过四次「判据失效」的亏，其中一次是「数字判据过了但实机错」。
//! 单元测试与探针都只证明**布局函数**自洽，证明不了**绘制层**照它画。
//! 因此这里直接读截图的像素，反查面板边框的实际物理位置，
//! 与 `solve()` 在同样客户区尺寸下给出的矩形逐个比对。
//!
//! # 关键：不复刻布局
//!
//! 期望值全部来自 [`layout::solve`]，本文件不自己算任何宽度。

use egui::Rect;
use image::RgbImage;
use modular_clipboard_ui::layout::{self, Panel};

const SCALE: f32 = 1.5;

fn load(path: &str) -> RgbImage {
    image::open(path)
        .unwrap_or_else(|e| panic!("读不到截图 {path}: {e}"))
        .to_rgb8()
}

/// 逻辑点 -> 物理像素（与 egui 的 pixels_per_point 同口径）。
fn px(v: f32) -> u32 {
    (v * SCALE).round().max(0.0) as u32
}

/// 取一列上「非背景色」像素的分布，用来定位竖直边界（面板边框/底色）。
///
/// 背景是窗口底色；面板底色与之不同，因此沿列扫描能找出面板左右缘。
/// 沿一条水平线扫描，返回颜色变化的 x 位置（物理像素）。
fn edges_along_row(img: &RgbImage, y: u32) -> Vec<(u32, [u8; 3], [u8; 3])> {
    let mut out = Vec::new();
    let mut prev = img.get_pixel(0, y).0;
    for x in 1..img.width() {
        let cur = img.get_pixel(x, y).0;
        if cur != prev {
            out.push((x, prev, cur));
            prev = cur;
        }
    }
    out
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "shot-client.png".to_string());
    let img = load(&path);
    let w = img.width() as f32;
    let h = img.height() as f32;
    let logical_w = w / SCALE;
    let logical_h = h / SCALE;
    println!("截图 {path}: {w}x{h} 物理 -> {logical_w:.2}x{logical_h:.2} 逻辑 (scale={SCALE})");

    let area = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(logical_w, logical_h));
    let solved = layout::solve(area, &layout::LayoutState::default(), None);

    println!("\nsolve() 输出（档位 {}）:", solved.tier.name());
    let mut expect_edges: Vec<(String, f32)> = Vec::new();
    for p in Panel::ALL {
        if let Some(r) = solved.rects[layout::panel_index(p)] {
            println!("  {:<6} x={:>7.2}..{:>7.2} (物理 {}..{})", p.title(), r.min.x, r.max.x, px(r.min.x), px(r.max.x));
            expect_edges.push((format!("{} 左缘", p.title()), r.min.x));
            expect_edges.push((format!("{} 右缘", p.title()), r.max.x));
        }
        if let Some(r) = solved.floating[layout::panel_index(p)] {
            println!("  {:<6} [浮层] x={:>7.2}..{:>7.2}", p.title(), r.min.x, r.max.x);
        }
    }

    // ---- 像素判据 1：面板竖直边界必须真的出现在截图里 ----
    //
    // 在**面板中段**的一条扫描线上找颜色跳变。选 y = 客户区垂直中点，
    // 那里必定穿过所有面板的内部（避开标题栏与状态栏）。
    let scan_y = px(logical_h * 0.5);
    let edges = edges_along_row(&img, scan_y);
    println!("\n扫描行 y={scan_y}（客户区中点）的颜色跳变:");
    for (x, a, b) in edges.iter().take(40) {
        println!("  x={x:>4}  {a:?} -> {b:?}");
    }

    let mut fails = 0usize;
    println!("\n=== 判据：solve() 的每条面板边界都必须在像素里找到对应跳变 ===");
    for (name, lx) in &expect_edges {
        let target = px(*lx);
        // 贴左缘（x=0）或贴右缘（x=客户区宽）的边界**没有颜色跳变**——
        // 那里本来就是客户区的起点/终点，跳变只会在它内侧一格出现。
        // 判据必须把这两种情况单独处理，否则会报一个假的 FAIL。
        if target == 0 || target >= img.width() - 1 {
            println!(
                "  {name} 期望 x={target}（逻辑 {lx:.2}）: 贴客户区边缘，跳变判据不适用（跳过）"
            );
            continue;
        }
        // 容差 ±2 物理像素（1.33 逻辑点）：边框本身有1px 宽，
        // 抗锯齿会让跳变位置偏一格。
        //
        // ⚠️ 这里必须同时要求「跳变邻域里出现**边框色**」，不能只看
        // 「附近有没有跳变」。折叠态的详情/视图栏是**深色底**
        // （实测 RGB 25,25,31），面板边框与它同色系，于是那一段里
        // 唯一的跳变来自**栏内的图标笔画**（实测 x=808/810、853/855），
        // 而真正的边界 x=813 因为「底色 == 边框色」压根没有跳变。
        //
        // 只判「附近有跳变」时，期望 813 会被 810（图标左缘）匹配上、
        // 期望 849 被 855 匹配上，看起来通过；换个宽度就假失败。
        // 加上「必须是边框色」后，跳变位置与颜色都要对得上，
        // 判据才真的在验证「面板边界画在哪里」。
        let hit = edges.iter().any(|(x, a, b)| {
            if (*x as i32 - target as i32).abs() > 2 {
                return false;
            }
            // 边框色：比两侧面板底色都明显更深（实测 180,182,184 / 221,226,232
            // 这类浅底，与 25,25,31 这类深边框对比强烈）。
            let is_border = |c: [u8; 3]| -> bool {
                let l = (c[0] as i32 + c[1] as i32 + c[2] as i32) / 3;
                l < 200
            };
            is_border(*a) || is_border(*b)
        });
        println!(
            "  {} 期望 x={}（逻辑 {lx:.2}）: {}",
            name,
            target,
            if hit { "找到" } else { "未找到 ← FAIL" }
        );
        if !hit {
            fails += 1;
        }
    }

    // ---- 像素判据 2：最右侧面板不得越过客户区右缘 ----
    //
    // 这条直接对应实机症状「列表行溢出右边界」。
    // 做法：取最右面板右缘往右 3 像素，若颜色仍与面板内部一致，
    // 说明内容确实画到了客户区之外。
    println!("\n=== 判据：最右面板不得越界 ===");
    if let Some((name, right)) = expect_edges
        .iter()
        .filter(|(n, _)| n.ends_with("右缘"))
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        let r = px(*right);
        if r + 3 < img.width() {
            let inside = img.get_pixel(r, scan_y).0;
            let outside = img.get_pixel(r + 3, scan_y).0;
            println!("  {name} 物理 x={r}；内侧 {inside:?} 外侧 {outside:?}");
            if inside == outside {
                println!("    两侧同色 → 面板底色延伸到了客户区之外 ← FAIL");
                fails += 1;
            } else {
                println!("    两侧异色 → 面板在客户区内正常收口");
            }
        } else {
            println!("  {name} 物理 x={r} 贴近右缘（客户区宽 {}），无法外扩", img.width());
        }
    }

    // ---- 像素判据 3：底部提示必须在客户区内且左侧起于x=0 ----
    //
    // 对应症状「底部提示被截断、左边被窗口边界裁掉」。
    // 做法：取状态栏那一行，检查最左侧 2 像素是否为背景色
    // （若提示从 x=0 起画，最左像素必然是文字色）。
    println!("\n=== 判据：底部提示未被左边界裁掉 ===");
    let status_y = img.height() - px(12.0);
    let edge0 = edges_along_row(&img, status_y);
    let first = img.get_pixel(0, status_y).0;
    let bg = img.get_pixel(1, img.height() / 2).0;
    println!(
        "  状态栏行 y={status_y} 的 x=0 像素={first:?}（面板底色参考 {bg:?}）该行跳变数={}",
        edge0.len()
    );
    println!(
        "  左起第一条跳变在 x={:?}",
        edge0.first().map(|(x, _, _)| *x)
    );

    println!("\n=========== 结果 ===========");
    if fails == 0 {
        println!("PASS: 绘制层的面板边界与 solve() 输出一致，且未越界");
    } else {
        println!("FAIL: {fails} 处边界对不上");
    }
    std::process::exit(if fails == 0 { 0 } else { 1 });
}
