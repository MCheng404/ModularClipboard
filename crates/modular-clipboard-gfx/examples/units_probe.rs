//! 判定式：`tessellate` 产出的顶点坐标是**逻辑点**还是**物理像素**。
//!
//! # 为什么必须判定
//!
//! `egui.wgsl` 顶点着色器里：
//!
//! ```wgsl
//! let scaled = vec2<f32>(in.pos.x * u.dpr, in.pos.y * u.dpr);
//! let ndc_x = 2.0 * scaled.x / u.size_in_pixels.x - 1.0;
//! ```
//!
//! 它假设 `in.pos` 是逻辑点。而 `epaint` 在 `tessellate_text` 里写着：
//! "The contents of the galley are already snapped to pixel coordinates"。
//! 若 galley 坐标已是物理像素，shader 再乘 `dpr`（1.5）就是**放大到2.25 倍**，
//! 画面必然溢出、错位、被截断。
//!
//! 两种假设不能靠读注释猜——必须实测。
//!
//! # 判据
//!
//! 画一条位于 x=50..52 的竖条（逻辑点），分别在 dpr=1.0 / 1.5 下tessellate：
//!
//! - 顶点 x 随 dpr 变（宽度 2 → 3）⇒ 矩形坐标是**逻辑点**，shader 乘dpr 正确
//! - 顶点 x 不随 dpr 变（宽度恒为 2）⇒ 矩形坐标**已是物理像素**，shader 重复缩放
//!
//! **同时**测一个中文字符的宽度。文字走 galley，与矩形可能不同源——
//! 项目症状是「中文错位但英文正常」，若两者单位不一致就能解释。
//!
//! 纯 CPU，不建窗口、不碰 GPU。

use egui::epaint::Color32;

/// 逻辑面板尺寸（点）。
const W: f32 = 200.0;
const H: f32 = 100.0;

struct MeshStats {
    min_x: f32,
    max_x: f32,
    vertices: usize,
}

/// 在给定 dpr 下跑 `run_ui` → `tessellate`，收集顶点 x 范围。
///
/// `mode` 决定画矩形还是画文字——用来对比两条生成路径的单位。
fn probe(dpr: f32, mode: Mode) -> MeshStats {
    let ctx = egui::Context::default();
    // egui 0.36：`ppp = zoom_factor * native_pixels_per_point`。
    // 原生 DPI 缩放走后者，故用它把 ppp 钉成目标值。
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(W * dpr, H * dpr));

    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(screen),
            viewports: [(
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    native_pixels_per_point: Some(dpr),
                    inner_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(W, H),
                    )),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        |ui| match mode {
            Mode::Rect => {
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(egui::pos2(50.0, 10.0), egui::pos2(52.0, 30.0)),
                    egui::CornerRadius::ZERO,
                    Color32::WHITE,
                );
            }
            Mode::Text => {
                ui.painter().text(
                    egui::pos2(50.0, 10.0),
                    egui::Align2::LEFT_TOP,
                    "模块",
                    egui::FontId::proportional(20.0),
                    Color32::WHITE,
                );
            }
        },
    );

    // 用 output 自报的 ppp，与主程序 app.rs:433 完全一致。
    // 字体图集增量与本探针无关（纯 CPU，不上传 GPU），但**必须清空**：
    // egui 的 `TexturesDelta::drop` 会断言「增量为空」，不清就 panic。
    // 主程序用 `DeltaGuard` 做同一件事。
    let mut delta = std::mem::take(&mut { output.textures_delta });
    let atlas_size = delta
        .set
        .values()
        .flatten()
        .next()
        .map(|d| d.image.size());
    delta.clear();

    let prims = ctx.tessellate(output.shapes, output.pixels_per_point);
    let mut s = MeshStats {
        min_x: f32::MAX,
        max_x: f32::MIN,
        vertices: 0,
    };
    for p in &prims {
        if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
            for v in &m.vertices {
                s.min_x = s.min_x.min(v.pos.x);
                s.max_x = s.max_x.max(v.pos.x);
                s.vertices += 1;
            }
        }
    }
    if let Some(sz) = atlas_size {
        println!(
            "  [{}] 字体图集尺寸 {sz:?}（uv 除数由此决定）",
            mode.name()
        );
    }
    s
}

#[derive(Clone, Copy)]
enum Mode {
    Rect,
    Text,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Rect => "rect",
            Mode::Text => "text",
        }
    }
}

/// 判定单个 mode 在两种 dpr 下坐标是否随 dpr 变化。
fn judge(mode: Mode) -> &'static str {
    let a = probe(1.0, mode);
    let b = probe(1.5, mode);
    if a.vertices == 0 || b.vertices == 0 {
        println!("  [{}] 顶点为 0，无法判定", mode.name());
        return "inconclusive";
    }
    let wa = a.max_x - a.min_x;
    let wb = b.max_x - b.min_x;
    println!(
        "  [{}] dpr=1.0: x∈[{:.2},{:.2}] 宽 {:.2} 顶点 {}",
        mode.name(),
        a.min_x,
        a.max_x,
        wa,
        a.vertices
    );
    println!(
        "  [{}] dpr=1.5: x∈[{:.2},{:.2}] 宽 {:.2} 顶点 {}",
        mode.name(),
        b.min_x,
        b.max_x,
        wb,
        b.vertices
    );
    if (wb - wa).abs() < 0.5 {
        "logical_points"
    } else {
        "physical_pixels"
    }
}

fn main() {
    println!("=== tessellate 坐标单位判定 ===");
    println!("逻辑面板 {W}x{H} 点；矩形 x=50..52（宽 2 点）；文字「模块」20pt\n");

    let rect = judge(Mode::Rect);
    let text = judge(Mode::Text);

    println!("\n--- 结论 ---");
    println!("矩形路径单位: {rect}");
    println!("文字路径单位: {text}");

    let mut bad = false;
    if rect == "physical_pixels" {
        println!("⇒矩形已是物理像素，shader 再乘 dpr 会放大 {W}→{W}px 布局整体溢出");
        bad = true;
    }
    if text == "physical_pixels" {
        println!("⇒ 文字已是物理像素，shader 再乘 dpr 会把字形放大 1.5 倍并错位");
        bad = true;
    }
    if rect != text {
        println!("⇒ 两条路径单位不一致！这会让文字相对面板错位");
        bad = true;
    }

    if bad {
        println!(
            "PROBE_VERDICT rect_units={rect} text_units={text} \
             dpr_double_scale=CONFIRMED shader_bug=YES"
        );
        std::process::exit(2);
    }
    println!("PROBE_VERDICT rect_units={rect} text_units={text} dpr_double_scale=absent");
    std::process::exit(0);
}
