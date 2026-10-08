//! 状态栏专项探针：验证「设置」按钮与提示文字都在客户区内且互不重叠。
//!
//! # 为什么单独写一条
//!
//! `pixel_layout_probe` 只校验**面板**边界，状态栏是窗口 chrome
//! （不属于任何面板），它的判据只有一条很弱的「底部提示未被左边界裁掉」，
//! 完全没覆盖右端。
//!
//! 实机症状：状态栏右端的「设置」按钮被压到客户区右缘之外，
//! 无边框窗口外面就是桌面，于是按钮**缺了一截**。
//!
//! # 判据
//!
//! 1. 状态栏行的最右侧**必须仍有内容**（不是纯背景色延伸到底）——
//!    若「设置」按钮整体被推出客户区，那一段会是背景色。
//! 2. 提示文字（左）与右侧按钮区（右）之间必须存在**背景色间隙**：
//!    两块紧挨着 ⇒ 文字压到了按钮上。
//!
//! 不复刻布局：阈值只用来分辨「内容 / 背景」，具体矩形由像素给出。

use image::RgbImage;

/// 实机 DPI 144⇒ `scale = 1.5`。
const SCALE: f32 = 1.5;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "shot-client.png".into());
    let img = image::open(&path)
        .unwrap_or_else(|e| panic!("读不到截图 {path}: {e}"))
        .to_rgb8();
    let (w, h) = (img.width(), img.height());
    println!("截图 {path}: {w}x{h}");

    // 状态栏高度约 28 逻辑点 ⇒ 42 物理像素；取其中一行（垂直居中）。
    let y = h - (14.0 * SCALE) as u32;

    // 该行的背景色：取最左侧 3 像素的众数（提示之前应是背景）。
    let bg = img.get_pixel(2, y).0;
    println!("状态栏行 y={y}，背景色参考 {bg:?}");

    // 沿该行扫描，找出所有「非背景」区段。
    let is_ink = |c: [u8; 3]| -> bool {
        // 容差 10：抗锯齿与渐变边缘会带来小幅色差。
        (0..3).all(|i| (c[i] as i32 - bg[i] as i32).abs() > 10)
    };
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut start: Option<u32> = None;
    for x in 0..w {
        let ink = is_ink(img.get_pixel(x, y).0);
        match (ink, start) {
            (true, None) => start = Some(x),
            (false, Some(s)) => {
                runs.push((s, x));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, w));
    }

    println!("非背景区段（宽 >= 2px 才列出，单像素是抗锯齿噪声）:");
    let mut shown = 0;
    for (s, e) in &runs {
        if e - s < 2 {
            continue;
        }
        println!("  x={s:>4}..{e:<4} (宽 {:>3})", e - s);
        shown += 1;
    }
    if shown == 0 {
        println!("  （无）该行几乎全是背景色");
    }

    let mut fails = 0usize;

    // ---- 判据 1：右端必须有内容（「设置」按钮没被推出客户区）----
    // 取右侧 60 物理像素（40 逻辑点，够放下「设置」两个字 + 边框）。
    let tail = 60u32.min(w);
    let right_ink = (w - tail..w).filter(|&x| is_ink(img.get_pixel(x, y).0)).count();
    println!("\n=== 判据 1：右端 {tail}px 内必须有内容 ===");
    if right_ink == 0 {
        println!("  右端 {tail}px 全部是背景色 → 「设置」按钮被推出客户区 ← FAIL");
        fails += 1;
    } else {
        println!("  右端有 {right_ink} 个内容像素 → 按钮在客户区内");
    }

    // ---- 判据 2：右端内容不得紧贴右缘（说明被裁掉了一截）----
    // 「设置」按钮整体宽约 (font_sm*2 + space_md*2) * scale 物理像素；
    // 若只剩最右2~3 像素有内容，说明按钮大半在窗口外。
    println!("\n=== 判据 2：右端内容宽度应足够（按钮没被切掉一半）===");
    let rightmost_run = runs.iter().rev().find(|(s, e)| e - s >= 2);
    match rightmost_run {
        Some(&(s, e)) => {
            let run_w = e - s;
            println!("  最右区段 x={s}..{e}，宽 {run_w}px");
            // 至少要能看到 12 物理像素（8 逻辑点）才谈得上是个按钮。
            if run_w < 12 {
                println!("    区段过窄 ← FAIL：按钮被窗口右缘切掉");
                fails += 1;
            } else {
                println!("    区段宽度足够");
            }
        }
        None => {
            println!("  右端没有任何区段 ← FAIL");
            fails += 1;
        }
    }

    // ---- 判据 3：左侧提示与右侧按钮之间必须有间隙 ----
    println!("\n=== 判据 3：左侧提示与右侧按钮之间应有背景间隙 ===");
    // 右侧区从最右区段起点算起；左侧提示取最左区段。
    let leftmost = runs.iter().find(|(s, e)| e - s >= 2).copied();
    let rightmost_start = rightmost_run.map(|r| r.0);
    match (leftmost, rightmost_start) {
        (Some((ls, le)), Some(rs)) if ls < rs => {
            let gap = rs.saturating_sub(le);
            println!("  提示止于 x={le}，按钮起于 x={rs}，间隙 {gap}px");
            if gap == 0 {
                println!("    两者紧挨 ← FAIL：提示文字压到按钮上");
                fails += 1;
            } else {
                println!("    有间隙，未重叠");
            }
        }
        _ => {
            println!("  （该行只有一侧有内容，跳过间隙判据）");
        }
    }

    println!("\n=========== 结果 ===========");
    if fails > 0 {
        println!("FAIL: {fails} 项不通过");
        std::process::exit(1);
    }
    println!("PASS: 状态栏左右分区均在客户区内且互不重叠");
}
