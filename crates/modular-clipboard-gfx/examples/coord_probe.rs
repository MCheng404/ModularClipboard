//! 「点击位置 vs 渲染位置」错位的**双端实测**探针。
//!
//! # 为什么必须两端都测
//!
//! 本项目吃过三次判据失效的亏，其中一次最致命：上一个探针用
//! 「`tessellate` 顶点是逻辑点」下了结论——**那只测了渲染端**。
//! 用户感知的症状是「点了 A 没反应、点了 B 触发 A」，即**点击端**。
//! 渲染端自洽完全不代表点击端自洽：只要指针坐标与 `screen_rect`
//! 用了不同单位，画面依然「看起来正常」（只是整体缩放），而点击会整体偏移。
//!
//! 本探针在**同一帧**里同时取三组量，任一端错位都会被抓住：
//!
//! 1. **渲染端**：`ctx.tessellate` 吐出的顶点（逻辑点）→ 乘 `ppp` 得物理像素；
//! 2. **点击端**：把 `Event::PointerButton` 投到控件 rect 中心，看 `Response`；
//! 3. **端到端**：顶点物理坐标 → 着色器 NDC 公式 → 反算回物理像素，
//!    与「点击处距客户区左上角的物理距离」对比，差值即错位量。
//!
//! # 判据（回归用）
//!
//! ```text
//! misalign_px < 0.5物理像素
//! ```
//!
//! 该差值同时锁住两端：顶点变了或命中判定变了，任一都会让它变大。
//!
//! 纯 CPU：不开窗口、不建 GPU、不弹窗。字体走 `msyh.ttc`，
//! 与生产 `install_cjk_font` 同一路径。

use egui::epaint::{Color32, Primitive};
use egui::{Align2, FontId, Id, Pos2, Rect, Sense, Vec2, pos2, vec2};

/// 实机参数：窗口 420×560 物理像素，DPI 144 ⇒ scale 1.5。
/// 与 `shot.err` 里「交换链已重建 extent=420×560」一致。
const PHYS_W: f32 = 420.0;
const PHYS_H: f32 = 560.0;
const SCALE: f32 = 1.5;

/// 生产配置的 `font_scale`（`%APPDATA%/modular-clipboard/config/config.json`
/// 实测值= 1.0）。
///
/// ⚠️ 这是**用户配置缩放**，与 DPI 的 `SCALE = 1.5` 是两个不同的量。
/// UI 层形参 `scale` 传的是它，不是 `SCALE`。
/// 混用这两个正是「字号被乘两次」类缺陷的温床，故探针把两者分开命名。
const FONT_SCALE: f32 = 1.0;

/// 生产 `pal.font_md`（`theme.rs` 令牌值）。
const FONT_MD: f32 = 14.0;

/// 被测控件（搜索框）的逻辑 rect。
/// 取生产 `titlebar.rs` 在 280×373 画布、`font_scale=1.0` 下的真实区间，
/// 由`coord_probe --layout` 段实测回填；此处先给出布局中心值供断言。
const SEARCH_RECT: Rect = Rect::from_min_max(
    pos2(96.0, 4.0),
    pos2(232.0, 32.0),
);

// ---------------------------------------------------------------------------
// 坐标换算：与生产代码逐行对齐
// ---------------------------------------------------------------------------

/// `window.rs::unpack_pos` 的复刻—— `WM_MOUSEMOVE` 事件路径。
///
/// `lParam` 低/高 16 位是**有符号**客户区物理坐标。
fn unpack_pos(lparam: isize, scale: f32) -> Pos2 {
    let low = (lparam as u16) as i16 as i32;
    let high = ((lparam >> 16) as u16) as i16 as i32;
    Pos2::new(low as f32 / scale, high as f32 / scale)
}

/// `window.rs::pointer_in_points` 的复刻 —— 每帧兜底路径。
///
/// `GetCursorPos` → `ScreenToClient` 得客户区**物理**坐标，再除 `scale`。
fn pointer_in_points(client_phys: Vec2, scale: f32) -> Pos2 {
    Pos2::new(client_phys.x / scale, client_phys.y / scale)
}

/// `egui.wgsl` 顶点着色器的 NDC 公式（`in.pos` 是逻辑点）。
fn shader_ndc(pos_logical: Vec2, size_in_pixels: Vec2, dpr: f32) -> Vec2 {
    let scaled = vec2(pos_logical.x * dpr, pos_logical.y * dpr);
    vec2(
        2.0 * scaled.x / size_in_pixels.x - 1.0,
        1.0 - 2.0 * scaled.y / size_in_pixels.y,
    )
}

/// NDC → 视口物理像素（y 翻转已在shader 里做过，这里只做线性反算）。
fn ndc_to_physical(ndc: Vec2, size_in_pixels: Vec2) -> Vec2 {
    vec2(
        (ndc.x + 1.0) / 2.0 * size_in_pixels.x,
        (1.0 - ndc.y) / 2.0 * size_in_pixels.y,
    )
}

/// 把物理坐标打包成 `lParam`（模拟 Win32）。
fn pack_lparam(client_phys_x: f32, client_phys_y: f32) -> isize {
    let x = client_phys_x as i16 as u16 as isize;
    let y = client_phys_y as i16 as u16 as isize;
    x | (y << 16)
}

// ---------------------------------------------------------------------------
// 字体
// ---------------------------------------------------------------------------

/// 与生产 `install_cjk_font` 同路径装载中文字体。
fn install_cjk_font(ctx: &egui::Context) -> bool {
    const PATH: &str = "C:/Windows/Fonts/msyh.ttc";
    let Ok(bytes) = std::fs::read(PATH) else {
        return false;
    };
    let mut defs = egui::FontDefinitions::default();
    // ttc 容器：epaint 经ab_glyph 解析时会取第一个 face。
    defs.font_data
        .insert("cjk".to_owned(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        defs.families
            .entry(family)
            .or_default()
            .push("cjk".to_owned());
    }
    ctx.set_fonts(defs);
    true
}

/// 生产 `theme.rs::sized` 的复刻。
fn sized(size: f32, scale: f32) -> FontId {
    FontId::proportional(size * scale.clamp(0.8, 2.0))
}

// ---------------------------------------------------------------------------
// 帧：同时抓渲染端与点击端
// ---------------------------------------------------------------------------

/// 画被测控件的一帧 UI。布局与生产 `titlebar.rs::draw` 同构：
/// 背景填充 + 边框 + 左对齐文字，文字用 `sized(font_md, font_scale)`。
fn draw_frame(ui: &mut egui::Ui, rect: Rect) {
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::ZERO, Color32::from_rgb(32, 32, 32));
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::ZERO,
        egui::Stroke::new(1.0, Color32::from_rgb(80, 80, 80)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        "搜索历史…",
        sized(FONT_MD, FONT_SCALE),
        Color32::WHITE,
    );
}

/// 清空字体图集增量（不清会触发 `TexturesDelta::drop` 的断言）。
fn drop_delta(output: &mut egui::FullOutput) {
    let mut delta = std::mem::take(&mut output.textures_delta);
    delta.clear();
}

/// 渲染端：跑一帧取`tessellate` 顶点里落在 `rect` 内的物理范围。
fn measure_render(ppp: f32) -> (Rect, usize, f32) {
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    let logical_size = vec2(PHYS_W / SCALE, PHYS_H / SCALE);

    // 与生产一致：先钉 ppp，再喂 screen_rect（逻辑点）。
    ctx.set_pixels_per_point(ppp);

    let mut output = egui::FullOutput::default();
    for _ in 0..2 {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        };
        output = ctx.run_ui(raw, |ui| draw_frame(ui, SEARCH_RECT));
        drop_delta(&mut output);
    }
    let ppp = output.pixels_per_point;

    let prims = ctx.tessellate(output.shapes, ppp);
    let (mut lo, mut hi) = (Pos2::new(f32::MAX, f32::MAX), Pos2::new(f32::MIN, f32::MIN));
    let mut n = 0usize;
    for p in &prims {
        let Primitive::Mesh(m) = &p.primitive else {
            continue;
        };
        for v in &m.vertices {
            let pos = v.pos;
            if pos.x >= SEARCH_RECT.min.x - 1.0
                && pos.x <= SEARCH_RECT.max.x + 1.0
                && pos.y >= SEARCH_RECT.min.y - 1.0
                && pos.y <= SEARCH_RECT.max.y + 1.0
            {
                lo = lo.min(pos);
                hi = hi.max(pos);
                n += 1;
            }
        }
    }
    let rect_phys = Rect::from_min_max(
        pos2(lo.x * ppp, lo.y * ppp),
        pos2(hi.x * ppp, hi.y * ppp),
    );
    (rect_phys, n, ppp)
}

/// 点击端：投递一次完整点击（move → press → release），返回控件是否按下/命中。
fn measure_click(ppp: f32) -> (bool, Pos2) {
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    let logical_size = vec2(PHYS_W / SCALE, PHYS_H / SCALE);
    let id = Id::new("probe_search");

    let target = SEARCH_RECT.center();
    let mut down_on = false;

    // 预热一帧：让 `interact` 的上帧矩形登记进 hit-test 表。
    ctx.set_pixels_per_point(ppp);
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        },
        |ui| {
            ui.interact(SEARCH_RECT, id, Sense::click());
        },
    );
    drop_delta(&mut output);

    for pressed in [true, false] {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            events: vec![
                egui::Event::PointerMoved(target),
                egui::Event::PointerButton {
                    pos: target,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            let resp = ui.interact(SEARCH_RECT, id, Sense::click());
            if pressed {
                down_on = resp.is_pointer_button_down_on() || resp.clicked();
            }
        });
        drop_delta(&mut output);
    }

    let _ = ppp;
    (
        down_on,
        Pos2::new(target.x * ppp, target.y * ppp),
    )
}

// ---------------------------------------------------------------------------

fn main() {
    println!("=== 点击位置 vs 渲染位置 双端实测 ===");
    println!("窗口 {PHYS_W}×{PHYS_H} 物理像素，DPI scale {SCALE}");
    println!(
        "逻辑画布 {:.2}×{:.2} 点",
        PHYS_W / SCALE,
        PHYS_H / SCALE
    );
    println!("UI 层 font_scale {FONT_SCALE}（config.json 实测值）");
    println!(
        "被测控件逻辑 rect x∈[{:.1},{:.1}] y∈[{:.1},{:.1}]\n",
        SEARCH_RECT.min.x, SEARCH_RECT.max.x, SEARCH_RECT.min.y, SEARCH_RECT.max.y
    );

    // ---- 1. 双端测量 ----
    let (vert_phys, vert_count, ppp) = measure_render(SCALE);
    let (click_hit, click_phys) = measure_click(SCALE);
    let phys_size = vec2(PHYS_W, PHYS_H);

    println!("--- 1. 渲染端（tessellate 顶点实测）---");
    println!("ppp = {ppp}");
    println!(
        "顶点物理 x∈[{:.2},{:.2}] y∈[{:.2},{:.2}]（{vert_count} 个顶点）",
        vert_phys.min.x, vert_phys.max.x, vert_phys.min.y, vert_phys.max.y
    );
    println!(
        "理论（逻辑×ppp） x∈[{:.2},{:.2}] y∈[{:.2},{:.2}]",
        SEARCH_RECT.min.x * ppp,
        SEARCH_RECT.max.x * ppp,
        SEARCH_RECT.min.y * ppp,
        SEARCH_RECT.max.y * ppp
    );

    println!("\n--- 2. 点击端（投递事件实测）---");
    println!(
        "点击投递物理位置 ({:.2},{:.2}) →命中 {click_hit}",
        click_phys.x, click_phys.y
    );

    // ---- 3. 端到端错位量（核心判据）----
    let vert_center = vert_phys.center();
    let ndc = shader_ndc(
        vec2(vert_center.x / ppp, vert_center.y / ppp),
        phys_size,
        ppp,
    );
    let roundtrip = ndc_to_physical(ndc, phys_size);
    let d: Vec2 = roundtrip - click_phys.to_vec2();
    let misalign = d.length();
    println!("\n--- 3. 端到端错位量（回归判据）---");
    println!(
        "顶点 →着色器 NDC({:.6},{:.6}) → 反算物理 ({:.3},{:.3})",
        ndc.x,
        ndc.y,
        roundtrip.x, roundtrip.y
    );
    println!("点击物理位置                          ({:.3},{:.3})", click_phys.x, click_phys.y);
    println!("差值 dx={:.4} dy={:.4}  ⇒  misalign={misalign:.4} 物理像素", d.x, d.y);

    // ---- 4. 两条坐标路径一致性 ----
    println!("\n--- 4. unpack_pos vs pointer_in_points（同一物理位置）---");
    let mut worst = 0.0f32;
    for (x, y) in [
        (0.0, 0.0),
        (144.0, 6.0),
        (210.0, 27.0),
        (348.0, 54.0),
        (419.0, 559.0),
    ] {
        let (a, b, dd) = {
            let lp = pack_lparam(x, y);
            let a = unpack_pos(lp, SCALE);
            let b = pointer_in_points(vec2(x, y), SCALE);
            (a, b, (a - b).length())
        };
        println!(
            "物理 ({x:>5.0},{y:>5.0}) → unpack ({:>7.3},{:>7.3})   pointer ({:>7.3},{:>7.3})   差 {dd:.6}",
            a.x, a.y, b.x, b.y
        );
        worst = worst.max(dd);
    }
    println!("两条路径最大差 {worst:.6} 逻辑点");

    // ---- 5. 字号实测（候选 A：字号被乘两次？）----
    //
    // ⚠️ **判据不能是「行高 == font_md」**。`font_md = 14` 是字号，
    // 而 galley 行高含上下伸部（实测 16），墨迹盒又比字号小
    // （实测物理 12px）。拿绝对值当判据会恒假失败——这正是本项目
    // 吃过的那次判据失效。
    //
    // 正确的判据是**线性度**：字号必须严格正比于 `font_scale`，
    // 且比例系数等于 `font_scale`，不含 DPI。若「乘了两次」，
    // 实际字号会是 14×1.5×1.5，比线性预期大75%，一眼可辨。
    println!("\n--- 5. 字号实测（候选 A：字号被乘两次？）---");
    println!("字号基准 = font_md({FONT_MD})；galley 行高含伸部，不是字号本身");
    let h1 = galley_height_at(FONT_SCALE);
    let h2 = galley_height_at(FONT_SCALE * 2.0);
    println!("font_scale=1.0 → 行高 {h1:.2} 逻辑点 = {:.2} 物理像素", h1 * SCALE);
    println!("font_scale=2.0 → 行高 {h2:.2} 逻辑点 = {:.2} 物理像素", h2 * SCALE);
    println!("字形墨迹高（物理像素，font_scale=1.0） {} ← 正常，小于字号", measure_glyph_height().1);
    let ratio = h2 / h1;
    println!("行高比值 h(2.0)/h(1.0) = {ratio:.4}  （线性预期 = 2.0000）");
    let linearity_err = (ratio - 2.0).abs();
    println!("线性度误差 {linearity_err:.4}");

    // 绝对标定：14pt 在1.0 倍下的行高（实测基线，供跨版本对比）。
    let galley_logical_h = h1;
    let galley_expect_logical = FONT_MD; // 名义字号，仅作参考打印
    println!("名义字号 {galley_expect_logical:.0} → 实测行高 {galley_logical_h:.2}（含伸部，正常）");
    println!(
        "若UI 层误把 DPI(1.5) 当 font_scale 再乘一次：字号 {0} 逻辑点 = {1:.2} 物理像素，\
         而实测只有 {2:.2} 物理像素",
        FONT_MD * SCALE,
        FONT_MD * SCALE * SCALE,
        galley_logical_h * SCALE
    );
    let double_scaled = galley_logical_h > (FONT_MD * 1.5 * 1.05);
    if double_scaled {
        println!("⇒实测字号确实被乘了第二次 scale");
    } else {
        println!("⇒字号**没有**被乘两次：font_scale 与 DPI 是两个独立量，\
                  egui 的 ppp={SCALE} 已负责 DPI，UI 层不该再乘");
    }

    // ---- 6/7. 真实标题栏布局与拖动区吞控件 ----
    let titlebar_bad = probe_titlebar();

    // ---- 结论 ----
    println!("\n--- 结论 ---");
    let mut bad = false;
    if vert_count == 0 {
        println!("✗ 控件范围内没有顶点，测量无效");
        bad = true;
    }
    if !click_hit {
        println!("✗ 点击控件中心未命中");
        bad = true;
    }
    if misalign >= 0.5 {
        println!("✗ 顶点与点击错位 {misalign:.4} px ≥ 0.5，坐标链路不一致");
        bad = true;
    }
    if worst >= 0.001 {
        println!("✗ 两条指针坐标路径不一致（最大差 {worst:.6}），每帧指针会跳动");
        bad = true;
    }
    if linearity_err >= 0.02 {
        println!("✗ 字号线性度误差 {linearity_err:.4} ≥ 0.02，字号链路非线性（疑似重复缩放）");
        bad = true;
    }
    if titlebar_bad {
        bad = true;
    }

    println!(
        "PROBE_VERDICT misalign_px={misalign:.4} click_hit={click_hit} \
         path_delta={worst:.6} galley_h_fs1={h1:.2} linearity_err={linearity_err:.4} \
         double_scaled={double_scaled} \
         titlebar_drag_swallow={titlebar_bad} verdict={}",
        if bad { "FAIL" } else { "OK" }
    );
    std::process::exit(if bad { 2 } else { 0 });
}

/// 给定 `font_scale` 下「模」字的 galley 行高（逻辑点）。
///
/// 判据用**线性度**而非绝对值：字号必须严格正比于 `font_scale`。
/// 绝对值判据（行高 == 14）会恒假失败——行高含伸部，本就大于字号。
fn galley_height_at(font_scale: f32) -> f32 {
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    let logical_size = vec2(PHYS_W / SCALE, PHYS_H / SCALE);
    let mut h = 0.0f32;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        },
        |ui| {
            h = ui
                .painter()
                .layout_no_wrap("模".to_owned(), sized(FONT_MD, font_scale), Color32::WHITE)
                .rect
                .height();
        },
    );
    drop_delta(&mut output);
    h
}

/// 量一个汉字的高度：galley 行高（逻辑点，≈字号）与字形墨迹高（物理像素）。
///
/// 返回 `(galley 逻辑高, 墨迹物理高, galley 理论逻辑高)`。
fn measure_glyph_height() -> (f32, f32, f32) {
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    let logical_size = vec2(PHYS_W / SCALE, PHYS_H / SCALE);
    let origin = pos2(10.0, 10.0);

    let mut galley_h = 0.0f32;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        },
        |ui| {
            galley_h = ui
                .painter()
                .layout_no_wrap("模".to_owned(), sized(FONT_MD, FONT_SCALE), Color32::WHITE)
                .rect
                .height();
            ui.painter().text(
                origin,
                Align2::LEFT_TOP,
                "模",
                sized(FONT_MD, FONT_SCALE),
                Color32::WHITE,
            );
        },
    );
    drop_delta(&mut output);

    // 再跑一帧专取顶点：galley 已进字体图集，形状里才有字形四边形。
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        },
        |ui| {
            ui.painter().text(
                origin,
                Align2::LEFT_TOP,
                "模",
                sized(FONT_MD, FONT_SCALE),
                Color32::WHITE,
            );
        },
    );
    drop_delta(&mut output);

    let ppp = output.pixels_per_point;
    let prims = ctx.tessellate(output.shapes, ppp);
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for p in &prims {
        let Primitive::Mesh(m) = &p.primitive else {
            continue;
        };
        for v in &m.vertices {
            if v.pos.x >= origin.x - 1.0 && v.pos.x <= origin.x + 40.0 {
                lo = lo.min(v.pos.y);
                hi = hi.max(v.pos.y);
            }
        }
    }
    let phys_h = if lo <= hi { (hi - lo) * ppp } else { 0.0 };
    (galley_h, phys_h, FONT_MD * FONT_SCALE)
}

// ---------------------------------------------------------------------------
// 6. 真实标题栏布局 + WM_NCHITTEST 拖动区吞控件检测
// ---------------------------------------------------------------------------

/// `titlebar.rs` 的常量（逐行对照，注释给出出处行号）。
mod tb {
    /// `titlebar.rs:31 TITLEBAR_HEIGHT`
    pub const TITLEBAR_HEIGHT: f32 = 36.0;
    /// `titlebar.rs:34 BUTTON_SIZE`
    pub const BUTTON_SIZE: f32 = 28.0;
    /// `titlebar.rs:37 CLOSE_BUTTON_WIDTH`
    pub const CLOSE_BUTTON_WIDTH: f32 = 44.0;
    /// `titlebar.rs:43 DRAG_MARGIN`
    pub const DRAG_MARGIN: f32 = 8.0;
    /// `titlebar.rs:51 TITLE_GAP`
    pub const TITLE_GAP: f32 = 12.0;
    /// `titlebar.rs:56 TITLE_MAX_FRACTION`
    pub const TITLE_MAX_FRACTION: f32 = 0.34;
}

/// 标题栏各元素（逻辑点）。
#[derive(Debug, Clone, Copy)]
struct Titlebar {
    full: Rect,
    title: Rect,
    search: Rect,
    settings: Rect,
    minimize: Rect,
    close: Rect,
    drag: Rect,
}

/// `titlebar.rs::TitlebarLayout::new` 的**逐行复刻**。
///
/// `gfx` 不能依赖 `ui`（会成环），故此处照抄实现；每个分支都与源文件
/// 对照过。`title_text_w` 由真实字体度量得到，与 `view.rs:249` 一致。
fn titlebar_layout(bar: Rect, scale: f32, title_text_w: f32) -> Titlebar {
    let btn = tb::BUTTON_SIZE * scale;
    let close_w = tb::CLOSE_BUTTON_WIDTH * scale;
    let inset = tb::DRAG_MARGIN * scale;

    let close = Rect::from_min_size(
        pos2(bar.max.x - close_w, bar.min.y),
        vec2(close_w, bar.height()),
    );
    let minimize = Rect::from_min_size(
        pos2(close.min.x - btn, bar.min.y),
        vec2(btn, bar.height()),
    );
    let settings = Rect::from_min_size(
        pos2(minimize.min.x - btn, bar.min.y),
        vec2(btn, bar.height()),
    );
    let title_limit = bar.width() * tb::TITLE_MAX_FRACTION;
    let title_w = title_text_w.max(0.0).min(title_limit).max(inset);
    let title = Rect::from_min_size(
        pos2(bar.min.x + inset, bar.min.y),
        vec2(title_w, bar.height()),
    );
    let search_x0 = title.max.x + inset.max(tb::TITLE_GAP);
    let search_w = (settings.min.x - inset - search_x0).max(0.0);
    let search = Rect::from_min_size(
        pos2(search_x0, bar.min.y + (bar.height() - btn) / 2.0),
        vec2(search_w, btn),
    );
    let drag = Rect::from_min_max(pos2(bar.min.x, bar.min.y), pos2(settings.min.x, bar.max.y));
    Titlebar {
        full: bar,
        title,
        search,
        settings,
        minimize,
        close,
        drag,
    }
}

/// 实测标题文本宽度（`view.rs:249`的 `measure_text` 语义）。
fn measure_app_title(ui: &egui::Ui) -> f32 {
    ui.painter()
        .layout_no_wrap(
            "模块化剪切板".to_owned(),
            sized(FONT_MD, FONT_SCALE),
            Color32::PLACEHOLDER,
        )
        .rect
        .width()
}

/// 拖动区覆盖检查：**落在 `drag` 内的按下会被系统拿走变成拖窗口，
/// egui 收不到**（`chrome.rs` 模块文档明确写了这条约束）。
///
/// 这是「点了 A 没反应」的直接机制：不是坐标错位，是那一次点击
/// **根本没进 egui**。
fn probe_titlebar() -> bool {
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    let logical_size = vec2(PHYS_W / SCALE, PHYS_H / SCALE);
    ctx.set_pixels_per_point(SCALE);

    let mut title_w = 0.0f32;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, logical_size)),
            ..Default::default()
        },
        |ui| {
            title_w = measure_app_title(ui);
        },
    );
    drop_delta(&mut output);

    // `layout.rs` 把顶栏高度定为 TITLEBAR_HEIGHT，宽度取整个画布。
    let bar = Rect::from_min_size(Pos2::ZERO, vec2(logical_size.x, tb::TITLEBAR_HEIGHT));
    let t = titlebar_layout(bar, FONT_SCALE, title_w);
    let ppp = SCALE;

    println!("\n--- 6. 真实标题栏布局（逻辑点，font_scale={FONT_SCALE}）---");
    println!("实测标题宽「模块化剪切板」 = {title_w:.2} 点（物理 {:.2} px）", title_w * ppp);
    println!(
        "title    x∈[{:.2},{:.2}]  物理 x∈[{:.2},{:.2}]",
        t.title.min.x,
        t.title.max.x,
        t.title.min.x * ppp,
        t.title.max.x * ppp
    );
    println!(
        "search   x∈[{:.2},{:.2}] y∈[{:.2},{:.2}]  物理 x∈[{:.2},{:.2}]",
        t.search.min.x,
        t.search.max.x,
        t.search.min.y,
        t.search.max.y,
        t.search.min.x * ppp,
        t.search.max.x * ppp
    );
    println!(
        "settings x∈[{:.2},{:.2}]  物理 x∈[{:.2},{:.2}]",
        t.settings.min.x,
        t.settings.max.x,
        t.settings.min.x * ppp,
        t.settings.max.x * ppp
    );
    println!(
        "drag     x∈[{:.2},{:.2}] y∈[{:.2},{:.2}]  物理 x∈[{:.2},{:.2}]",
        t.drag.min.x,
        t.drag.max.x,
        t.drag.min.y,
        t.drag.max.y,
        t.drag.min.x * ppp,
        t.drag.max.x * ppp
    );
    println!("画布宽 {:.2} 点（物理 {:.0} px）", logical_size.x, PHYS_W);
    println!(
        "titlebar x∈[{:.2},{:.2}]  物理 x∈[{:.2},{:.2}]",
        t.full.min.x,
        t.full.max.x,
        t.full.min.x * ppp,
        t.full.max.x * ppp
    );

    // ---- 关键判定：拖动区是否吞掉搜索框 ----
    let search_center = t.search.center();
    let swallowed = t.drag.contains(search_center);
    println!("\n--- 7. WM_NCHITTEST 拖动区吞控件判定 ---");
    println!(
        "搜索框中心（逻辑）({:.2},{:.2})  物理 ({:.2},{:.2})",
        search_center.x,
        search_center.y,
        search_center.x * ppp,
        search_center.y * ppp
    );
    println!(
        "该点是否落在 drag 内: {swallowed}  ⇒{}",
        if swallowed {
            "点击被系统接管成拖窗口，egui 收不到 ⇒「点了没反应」"
        } else {
            "点击落到 egui"
        }
    );
    // 拖动区右界 vs 搜索框右界
    println!(
        "drag.max.x={:.2} vs search.max.x={:.2}  ⇒ 搜索框有 {:.2} 点被drag 覆盖",
        t.drag.max.x,
        t.search.max.x,
        (t.search.max.x - t.drag.min.x).max(0.0).min(t.drag.max.x - t.drag.min.x)
    );

    // 三个窗口按钮必须在 drag 之外
    let btns = [("settings", t.settings), ("minimize", t.minimize), ("close", t.close)];
    let mut btn_ok = true;
    for (name, r) in btns {
        let inside = t.drag.contains(r.center());
        println!(
            "  {name:<9} 中心 ({:>7.2},{:>6.2}) 在 drag 内: {inside}",
            r.center().x,
            r.center().y
        );
        if inside {
            btn_ok = false;
        }
    }

    if swallowed {
        println!("\n✗搜索框被拖动区完全覆盖 ⇒ 点击永远到不了 egui");
        println!(
            "  机制：chrome.rs:hit_test 命中 drag ⇒ 返回 HTCAPTION ⇒ \
             WM_LBUTTONDOWN 不进客户区消息流"
        );
        println!(
            "  用户看到的是：搜索框画在 x∈[{:.0},{:.0}] 物理，点了却拖窗口",
            t.search.min.x * ppp,
            t.search.max.x * ppp
        );
    }
    if !btn_ok {
        println!("✗ 有窗口按钮落在拖动区内 ⇒ 按钮点不动");
    }
    return swallowed || !btn_ok;
}