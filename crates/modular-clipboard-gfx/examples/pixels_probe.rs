//! 像素渲染验证。**只**回答「离屏渲染 + 读回的像素是否正确」，
//! 一个交换链对象都不碰。
//!
//! # 为什么要独立成探针
//!
//! 旧的 `resize_probe`（已删除）在同一份代码里混了两件事：
//!
//! 1. 借 `FrameRenderer` 的交换链跑帧循环、触发 resize；
//! 2. 自建离屏靶面做 A/B 像素校验，**却复用生产管线的描述符集
//!    （`fr.descriptor_set(0)`）与生产渲染通道**。
//!
//! 后果是它既无法跟随生产代码演进（`pipeline.rs` 改了 `final_layout`
//! 之后立刻报 `VUID-vkCmdDraw-None-09600`——描述符里声明的
//! `imageLayout` 与实际不符），又因为自己的判据常量与清屏色不一致而
//! 输出噪声（`BG = [0x1E,0x1E,0x22]` 而清屏色是 `[0,0,0]`，差 0x1E
//! ⇒ 每个像素都算「非背景」⇒ 负对照检出 576000 = 全屏像素数）。
//!
//! **只要诊断判据与被测实现不一致，检查结果就是噪声。**
//!
//! 现在拆成两个单一职责的探针：
//! - `swapchain_resize.rs`：交换链重建路径（不查像素）
//! - 本文件：像素渲染（**不借任何生产资源**）
//!
//! # 「不借生产资源」的具体含义
//!
//! 本探针**自己**拥有渲染通道、图形管线、描述符集布局、描述符池、
//! 描述符集、命令池、命令缓冲、栅栏、读回缓冲、离屏图像与 framebuffer。
//! 它只用库提供的**构造函数**（`RenderPass::new` 等）——那是工厂，
//! 不是共享资源。
//!
//! 与旧探针的关键差别：旧探针拿的是 `FrameRenderer` **持有的**描述符集，
//! 那个描述符集的 `imageLayout` 由帧层的布局记账决定，与离屏路径无关。
//! 本探针的描述符集由自己写、布局由自己算，不存在「借用」关系。
//!
//! 另外，渲染通道按**离屏自己的格式**（`R8G8B8A8_UNORM`）创建，
//! 不跟随交换链格式——这正是「自建」的含义：生产侧改格式选择逻辑时，
//! 本探针的判据不受影响。
//!
//! # 判据必须自洽（本文件的核心）
//!
//! 清屏色是判据的**唯一**来源：
//!
//! ```text
//! const CLEAR: [f32; 4]   ← 线性空间 RGBA，喂给 cmd_begin_render_pass
//! const BG:    [u8; 3]    ← 由 CLEAR 逐字节推导，读回时用它判「背景」
//! ```
//!
//! 两者不允许各写一份。`bg_is_derived_from_clear` 等单测 + 运行时的
//! `debug_assert` 一起锁住它们的一致性。
//!
//! 负对照与正对照走**完全相同的代码路径**（同一个
//! `Pixels::render_and_read`、同一描述符集、同一管线、同一对缓冲、
//! 同一读回流程），**只差「有没有发 draw」**：
//!
//! | 对照 | 批次 | 期望「非背景像素」 |
//! |------|------|------------------|
//! | 负对照 | 空 | **恰好 0**（只有清屏色） |
//! | 正对照 | 真实 egui 图元 | **> 0 且 < 全屏像素数** |
//!
//! 负对照若检出内容，说明「非背景」判定本身失效 ⇒ 正对照的结论不可信
//! ⇒ **主动 bail 并说明检查失效**，绝不降级判据去「让它通过」。
//!
//! 正对照的上界（`<` 全屏像素数）同样重要：界面会先铺一层与清屏色
//! **完全相同**的背景矩形，所以只有文字与色块算「非背景」。若这层背景
//! 根本没画出来，计数会接近全屏 ⇒ 判据能同时抓到「画太多」与
//! 「画太少」两种坏法。
//!
//! # 判据本身可单测（不依赖 GPU）
//!
//! `classify` 是纯函数。`negative_control_bytes_are_empty` /
//! `positive_control_bytes_are_drawn` / `criterion_separates_positive_from_negative`
//! 三条单测把「正对照的字节」与「纯背景字节」喂给它，断言结论翻转——
//! 于是判据的有效性不必等实机。
//!
//! # 关于那个窗口
//!
//! `Gpu::new` 需要 `HINSTANCE` / `HWND` 才能创建 `VkSurfaceKHR`，
//! 所以本探针仍然建一个窗口。**它从不被呈现、不被 resize**，
//! 只是设备上下文的一部分。窗口放在桌面内 (8, 8) 且尺寸很小。
//!
//! # 实机预期输出
//!
//! ```text
//! OK Window  客户区 320x240  scale_factor=1（仅为 Gpu::new 提供 HWND，本探针不呈现）
//! OK Gpu  NVIDIA ... (DiscreteGpu)
//! OK 离屏管线  格式 R8G8B8A8_UNORM（由本探针自定，与交换链格式无关）
//! OK 字体图集已加载：C:/Windows/Fonts/msyh.ttc
//! OK 合成字体图集 64x64 R8（全白覆盖率 4096 字节）
//! OK 占位用户纹理 4x4 R8G8B8A8（着色器声明了 binding 3，绝不能未初始化）
//! OK 描述符集 4 个绑定全部写入（uniform / sampler / font / user）
//!
//! ---- 靶面 #0  640x480（离屏资源重建 #1）----
//!   几何: 1234 顶点 / 3702 索引 / 12 draw call
//!   负对照（空批次，只清屏）: 非背景 0 / 307200 像素 -> Empty
//!   正对照（真实图元）      : 非背景 15203 / 307200 像素 -> Drawn
//!
//! ---- 靶面 #1  320x240（离屏资源重建 #2）----
//!   ...
//!
//! 汇总: 4 个尺寸，离屏资源重建 4 次
//! ALL OK - 4 个尺寸，离屏资源重建 4 次；非背景像素 负对照 [0, 0, 0, 0] / 正对照 [...]
//! ```
//!
//! 负对照**必须是 0**；正对照的具体数值取决于字体与 egui 版本，
//! 落在「数千 ~ 数万」量级即为正常（文字笔画 + 3 个 70×40 色块）。
//!
//! 退出码 0 = 全部判据通过；1 = 某个判据失败（stderr 打印 `FAIL pixels_probe: ...`）。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::DrawBatch;
use modular_clipboard_gfx::pipeline::{
    BINDING_SAMPLER, BINDING_TEXTURE, BINDING_UNIFORM, BINDING_USER_TEXTURE, Uniforms,
};
use modular_clipboard_gfx::staging::StagingSlice;
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};
use modular_clipboard_gfx::window::Window;

/// 窗口尺寸。窗口只为 `Gpu::new` 提供 HWND，从不被呈现或 resize。
const W: u32 = 320;
const H: u32 = 240;

/// 窗口位置：留在桌面内且不抢焦点。
///
/// 移出屏幕会让 DWM 报告的 `current_extent` 超出表面能力范围
/// （MEMORY 第 19 批）。本探针不使用交换链，但保持一致以免误导。
const WINDOW_X: i32 = 8;
const WINDOW_Y: i32 = 8;

/// 离屏靶面格式。
///
/// **刻意不用交换链格式**：`R8G8B8A8_UNORM` 的字节序是 R,G,B,A，
/// 与 [`CLEAR`] 的分量顺序一致，于是判据不需要任何 B/R 交换的换算。
/// 格式由本探针自己决定，因此交换链格式变化不影响本探针。
const TARGET_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// 清屏色（线性空间 RGBA，喂给 `cmd_begin_render_pass` 的 clear value）。
///
/// **判据的唯一来源**：读回时用来判「背景」的字节由它推导（见 [`BG`]）；
/// 界面绘制的背景矩形也用同一组分量（见 [`bg_as_egui_color`]）。
/// 三者因此不可能漂移。
const CLEAR: [f32; 4] = [
    0x1E as f32 / 255.0,
    0x1E as f32 / 255.0,
    0x22 as f32 / 255.0,
    1.0,
];

/// UNORM 浮点 → 8 位字节（带四舍五入）。
///
/// 规范允许驱动对 `f * 255` 用截断或四舍五入，故加 0.5 后再截断，
/// 在两种实现下都落在正确的一侧。
const fn unorm8(v: f32) -> u8 {
    let s = v * 255.0 + 0.5;
    if s <= 0.0 {
        0
    } else if s >= 255.0 {
        255
    } else {
        s as u8
    }
}

/// 读回时的「背景色」判据。**由 [`CLEAR`] 推导，不得独立书写。**
///
/// 顺序与 [`TARGET_FORMAT`] 的字节序一致：R, G, B。
const BG: [u8; 3] = [unorm8(CLEAR[0]), unorm8(CLEAR[1]), unorm8(CLEAR[2])];

/// 判定「该像素与背景不同」的容差（字节）。
///
/// UNORM 浮点 → 8 位的转换误差最多 1（截断 vs 四舍五入之差），
/// 混合运算再引入 1~2。取 2 留足余量，同时远小于文字/色块与背景的差异
/// （背景 `0x1E`，白字 `0xFF`，差 0xE1）。
const BG_TOLERANCE: i32 = 2;

/// 离屏靶面尺寸序列（宽, 高，物理像素）。
///
/// - 覆盖**变大与变小**两个方向：只单向变化时某些驱动可能不真正
///   重建资源，销毁链就测不到；
/// - 最后一档回到第一档：验证「改回去」也能正确重建；
/// - 每换一档都必须重建离屏图像 / framebuffer / 读回缓冲
///   （相邻两档刻意不同，由 `target_sequence_has_distinct_neighbours` 守住）。
const TARGET_SEQ: &[(u32, u32)] = &[(640, 480), (320, 240), (800, 600), (640, 480)];

/// 顶点缓冲初始容量（顶点数）。
const INITIAL_VERTEX_CAPACITY: usize = 4096;

/// 索引缓冲初始容量（索引数）。
const INITIAL_INDEX_CAPACITY: usize = 6144;

/// 合成字体图集边长（像素）。全白覆盖率 ⇒ 采样结果恒为 1。
const SYNTH_FONT_TEX: u32 = 64;

/// 占位用户纹理边长（像素）。着色器声明了 binding 3，绝不能未初始化
/// （MEMORY 坑 82：未初始化描述符被绑定是 UB，症状是丢设备）。
const PLACEHOLDER_USER_TEX: u32 = 4;

/// 4×4 单位矩阵（列主序）。着色器自己做像素 → NDC，矩阵只作非零占位。
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// 等栅栏的超时（纳秒）。
///
/// 取 5 秒：本探针每次只提交一次很小的渲染，正常是毫秒级；
/// 超时意味着设备真的卡住了，此时明确失败远好于无限阻塞
/// （无限等待会把「设备卡住」变成无声挂起）。
const GPU_WAIT_TIMEOUT_NS: u64 = 5_000_000_000;

/// 读回内容的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Content {
    /// 全是背景色。
    Empty,
    /// 存在与背景色不同的像素。
    Drawn,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL pixels_probe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // ---- 窗口（只为 Gpu::new 提供 HWND）---------------------------------
    let window = Window::new("ModularClipboard — pixels_probe", W, H)?;
    move_window(&window, WINDOW_X, WINDOW_Y, W, H)?;
    println!(
        "OK Window  客户区 {}x{}  scale_factor={}（仅为 Gpu::new 提供 HWND，本探针不呈现）",
        window.inner_size_physical().0,
        window.inner_size_physical().1,
        window.scale_factor()
    );

    // ---- 设备 ----------------------------------------------------------
    let gpu = Gpu::new("ModularClipboardPixelsProbe", window.hinstance(), window.hwnd())?;
    let device_name = device_name_of(&gpu);
    println!("OK Gpu  {device_name}  ({:?})", gpu.device_type);

    // 判据自洽性：运行期再确认一次（单测之外的第一道闸）。
    anyhow::ensure!(
        bg_is_derived_from_clear(),
        "BG 与 CLEAR 不一致：判据与被测实现已经脱节，负对照必然失效"
    );

    // ---- egui ----------------------------------------------------------
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    ctx.set_visuals(ctx.style_of(egui::Theme::Dark).visuals.clone());

    // ---- 自建全套离屏管线 ----------------------------------------------
    let mut px = Pixels::new(&gpu)?;
    println!("OK 离屏管线  格式 {TARGET_FORMAT:?}（由本探针自定，与交换链格式无关）");

    // 合成字体图集：全白覆盖率。走**生产上传路径**（`DeviceImage::upload`），
    // 于是「纹理上传」这一环也在探针覆盖范围内。
    {
        let coverage = vec![255u8; (SYNTH_FONT_TEX * SYNTH_FONT_TEX) as usize];
        let len = coverage.len() as vk::DeviceSize;
        let size = (SYNTH_FONT_TEX, SYNTH_FONT_TEX);
        let mut staging = OneShotStaging::new(&gpu, len)?;
        staging.write(&coverage)?;
        let slice = staging.slice();
        px.record_and_submit(|px, cmd| {
            px.font.upload(&gpu, cmd, slice, &coverage, None, size)
        })?;
        println!(
            "OK 合成字体图集 {SYNTH_FONT_TEX}x{SYNTH_FONT_TEX} R8（全白覆盖率 {len} 字节）"
        );
    }

    // 占位用户纹理：本探针的图元全是 tex_id = 0，永不采样它，
    // 但描述符必须写齐。
    {
        let bytes = placeholder_rgba();
        let len = bytes.len() as vk::DeviceSize;
        let size = (PLACEHOLDER_USER_TEX, PLACEHOLDER_USER_TEX);
        let mut staging = OneShotStaging::new(&gpu, len)?;
        staging.write(&bytes)?;
        let slice = staging.slice();
        px.record_and_submit(|px, cmd| px.user.upload(&gpu, cmd, slice, &bytes, None, size))?;
        println!(
            "OK 占位用户纹理 {PLACEHOLDER_USER_TEX}x{PLACEHOLDER_USER_TEX} R8G8B8A8\
             （着色器声明了 binding 3，不能未初始化）"
        );
    }

    // 描述符集必须在任何 draw 之前写齐。
    px.write_descriptors()?;
    println!("OK 描述符集 4 个绑定全部写入（uniform / sampler / font / user）");

    // ---- 逐尺寸：离屏渲染 + 读回 + A/B ---------------------------------
    let mut vertices: Vec<Vertex> = Vec::with_capacity(INITIAL_VERTEX_CAPACITY);
    let mut indices: Vec<u32> = Vec::with_capacity(INITIAL_INDEX_CAPACITY);
    let mut batches: Vec<Batch> = Vec::new();
    let mut draws: Vec<DrawBatch> = Vec::new();
    let mut negative_counts: Vec<usize> = Vec::new();
    let mut positive_counts: Vec<usize> = Vec::new();

    for (i, &(tw, th)) in TARGET_SEQ.iter().enumerate() {
        let rebuilt_before = px.rebuild_count();
        let extent = vk::Extent2D {
            width: tw,
            height: th,
        };
        // 尺寸变化 → 重建离屏图像 / framebuffer / 读回缓冲，并重写描述符。
        px.set_target(extent)?;
        if px.rebuild_count() > rebuilt_before {
            println!(
                "\n---- 靶面 #{i}  {tw}x{th}（离屏资源重建 #{}）----",
                px.rebuild_count()
            );
        } else {
            println!("\n---- 靶面 #{i}  {tw}x{th} ----");
        }

        // 1. 跑一帧 egui。screen_rect 用**物理像素当逻辑点**
        //    （pixels_per_point 默认 1.0），于是顶点坐标与靶面像素一一对应，
        //    uniform 的 dpr 也取 1.0。
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(tw as f32, th as f32),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| draw_ui(ui, (tw, th)));
        let ppp = out.pixels_per_point;
        let mut textures_delta = DeltaGuard(std::mem::take(&mut out.textures_delta));

        // 2. 字体图集增量。本探针的图元全部是 tex_id = 0，
        //    但纹理路径仍要真跑一遍——纹理加载坏了会让文字变豆腐块，
        //    那在读回里表现为「非背景像素」减少，属于本探针该抓的坏法。
        let uploaded = apply_font_delta(&gpu, &mut px, &mut textures_delta.0)?;
        if uploaded > 0 {
            println!("   字体图集增量上传 {uploaded} 次");
            px.write_descriptors()?;
        }

        // 3. tessellation
        let primitives = ctx.tessellate(out.shapes, ppp);
        tessellate_into(&primitives, &mut vertices, &mut indices, &mut batches);
        anyhow::ensure!(
            !vertices.is_empty() && !batches.is_empty(),
            "靶面 #{i}（{tw}x{th}）：egui 没有产出任何图元（顶点 {} / 批次 {}）—— \
             靶图有问题，正对照无从谈起",
            vertices.len(),
            batches.len()
        );
        println!(
            "   几何: {} 顶点 / {} 索引 / {} draw call",
            vertices.len(),
            indices.len(),
            batches.len()
        );

        // 4. uniform 按新尺寸重写。
        //
        // 这里可以「每尺寸无条件写一次」而无需担心竞态：本探针的每次渲染
        // 都紧跟一个栅栏等待，不存在多帧在途。交换链路径做不到这一点，
        // 那边的 uniform 必须按内容指纹才写（见 `full_app` 的 `UniformKey`）。
        px.write_uniform(&uniforms_for(extent, ppp))?;

        // 5. 顶点/索引数据 → 私有缓冲。两个对照共用同一份数据，
        //    因此正负差异只可能来自「有没有发 draw」。
        px.set_geometry(&vertices, &indices)?;

        draws.clear();
        draws.extend(
            batches
                .iter()
                .map(|b| b.to_draw(extent, out.pixels_per_point)),
        );

        // 6. 负对照：**同一函数**、同一描述符集、同一次读回，
        //    只把批次列表清空。
        let neg_bytes = px.render_and_read(&[])?;
        let total = verify_len(&neg_bytes, extent, "负对照")?;
        let neg_content = classify(&neg_bytes);
        let neg_count = count_non_background(&neg_bytes);

        // 7. 正对照：真实图元。
        let pos_bytes = px.render_and_read(&draws)?;
        let pos_total = verify_len(&pos_bytes, extent, "正对照")?;
        anyhow::ensure!(
            pos_total == total,
            "正对照读回长度 {pos_total} 与负对照 {total} 不一致 —— \
             读回缓冲尺寸与靶面不匹配"
        );
        let pos_content = classify(&pos_bytes);
        let pos_count = count_non_background(&pos_bytes);

        println!(
            "   负对照（空批次，只清屏）: 非背景 {neg_count} / {total} 像素 -> {neg_content:?}"
        );
        println!(
            "   正对照（真实图元）      : 非背景 {pos_count} / {total} 像素 -> {pos_content:?}"
        );

        // ---- 判据 ----------------------------------------------------
        //
        // 负对照检出内容 ⇒ 「非背景」判定失效 ⇒ 正对照结论不可信。
        // 此时**主动失败**并说明检查失效，绝不降级判据去「让它通过」。
        anyhow::ensure!(
            neg_content == Content::Empty && neg_count == 0,
            "负对照失败：只清屏时检出 {neg_count} 个非背景像素 —— \
             像素检查没有鉴别力，正对照结论不可信。\
             请检查 BG 是否仍由 CLEAR 推导（unorm8 的舍入是否被改动）"
        );
        anyhow::ensure!(
            pos_content == Content::Drawn && pos_count > 0,
            "正对照失败：{tw}x{th} 下离屏渲染没有任何非背景像素 —— \
             文字/色块没画出来（黑屏），但 GPU 未报任何错误"
        );
        anyhow::ensure!(
            pos_count < total,
            "正对照失败：{pos_count}/{total} 个像素都与背景不同 —— 接近全屏。\
             界面本应先铺一层与清屏色完全相同的背景矩形，只有文字与色块\
             才算「非背景」。全屏命中说明背景矩形没画出来（uniform 尺寸或 \
             顶点坐标与靶面不匹配）"
        );

        negative_counts.push(neg_count);
        positive_counts.push(pos_count);
    }

    // ---- 汇总 ------------------------------------------------------------
    println!("\n================ 汇总 ================");
    println!(
        "靶面尺寸 {} 个，离屏资源重建 {} 次",
        TARGET_SEQ.len(),
        px.rebuild_count()
    );
    for (i, &(tw, th)) in TARGET_SEQ.iter().enumerate() {
        println!(
            "  #{i} {tw}x{th}: 负对照 {} / 正对照 {}",
            negative_counts[i], positive_counts[i]
        );
    }

    // ---- 清理 ------------------------------------------------------------
    //
    // 先 GPU 空闲，再按「引用者 → 被引用者」逆序销毁。验证层在线时，
    // 若重建路径有泄漏，此处会报 VUID-vkDestroyDevice-device-05137。
    gpu.wait_idle();
    px.destroy();

    println!(
        "\nALL OK - {} 个尺寸，离屏资源重建 {} 次；非背景像素 负对照 {:?} / 正对照 {:?}",
        TARGET_SEQ.len(),
        px.rebuild_count(),
        negative_counts,
        positive_counts
    );
    Ok(())
}

/// 校验读回长度确实是 `w * h * 4`。
///
/// 长度不对时 `classify` 会给出**看似合理**的结论（`chunks_exact(4)`
/// 会静默丢掉尾部余数），所以必须先卡住长度。
fn verify_len(bytes: &[u8], extent: vk::Extent2D, which: &str) -> anyhow::Result<usize> {
    let need = (extent.width as usize) * (extent.height as usize) * 4;
    anyhow::ensure!(
        bytes.len() == need,
        "{which}：读回 {} 字节，应为 {need}（{}x{} 的 RGBA）",
        bytes.len(),
        extent.width,
        extent.height
    );
    Ok(need)
}

/// `device_name` 的 `[i8; 256]` → 可打印字符串（遇 NUL 即止）。
fn device_name_of(gpu: &Gpu) -> String {
    gpu.properties
        .device_name
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .map(|b| b as char)
        .collect()
}

/// 改变窗口位置与客户区尺寸。
///
/// `MoveWindow` 的参数是**整体窗口**尺寸；客户区要减去边框与标题栏。
/// `bRepaint = false` 避免额外 `WM_PAINT` 干扰事件顺序。
fn move_window(window: &Window, x: i32, y: i32, client_w: u32, client_h: u32) -> anyhow::Result<()> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, MoveWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };

    let mut r = RECT {
        left: 0,
        top: 0,
        right: client_w as i32,
        bottom: client_h as i32,
    };
    unsafe { AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)) }?;
    unsafe {
        MoveWindow(
            window.hwnd(),
            x,
            y,
            r.right - r.left,
            r.bottom - r.top,
            false,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 判据（纯函数，可单测）
// ---------------------------------------------------------------------------

/// 统计「与背景色不同」的像素数。字节序 R,G,B,A（见 [`TARGET_FORMAT`]）。
fn count_non_background(bytes: &[u8]) -> usize {
    bytes
        .chunks_exact(4)
        .filter(|p| {
            let d = |i: usize| (i32::from(p[i]) - i32::from(BG[i])).abs();
            d(0) > BG_TOLERANCE || d(1) > BG_TOLERANCE || d(2) > BG_TOLERANCE
        })
        .count()
}

/// 把读回字节分类为「空」或「有内容」。
///
/// 判据只有一条：**存在与 [`BG`] 相差超过 [`BG_TOLERANCE`] 的颜色通道**。
fn classify(bytes: &[u8]) -> Content {
    if count_non_background(bytes) == 0 {
        Content::Empty
    } else {
        Content::Drawn
    }
}

/// [`BG`] 是否确实等于「[`CLEAR`] 量化到 8 位」。
///
/// `BG` 是编译期常量、`CLEAR` 是喂给驱动的浮点。两者一旦漂移，
/// 负对照就会失效（要么恒为 0，要么恒为全屏）——而这两种失效在
/// 「正对照 > 0」面前都不会暴露。因此把它做成可断言的纯逻辑。
fn bg_is_derived_from_clear() -> bool {
    BG == [unorm8(CLEAR[0]), unorm8(CLEAR[1]), unorm8(CLEAR[2])]
}

/// 与 [`CLEAR`] 同色的 egui 颜色。
///
/// 界面铺的背景矩形用它，于是「清屏」与「绘制的背景」在像素上完全一致，
/// 正对照的非背景像素就只可能来自文字与色块。
fn bg_as_egui_color() -> egui::Color32 {
    egui::Color32::from_rgb(BG[0], BG[1], BG[2])
}

/// 占位用户纹理的 RGBA 字节（不透明品红）。
///
/// 本探针的图元全是 `tex_id = 0`，这个纹理永远不会被采样；
/// 但描述符必须写齐，否则着色器解引用未初始化描述符（UB → 丢设备）。
fn placeholder_rgba() -> Vec<u8> {
    let n = (PLACEHOLDER_USER_TEX * PLACEHOLDER_USER_TEX) as usize;
    let mut v = Vec::with_capacity(n * 4);
    for _ in 0..n {
        v.extend_from_slice(&[0xFF, 0x00, 0xFF, 0xFF]);
    }
    v
}

/// 构造本帧的 uniform。
fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
    Uniforms::new(
        IDENTITY,
        [extent.width as f32, extent.height as f32],
        pixels_per_point,
    )
}

/// 扩容策略：1.5 倍并向上取整到 1024，避免「每帧只多一个元素就重建」。
fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

// ---------------------------------------------------------------------------
// 自建离屏管线
// ---------------------------------------------------------------------------

/// 一次性的 staging 缓冲：`Drop` 时销毁。
///
/// # 为什么不复用 `StagingArena`
///
/// `StagingArena` 的生命周期由**帧槽位栅栏**保证，那是帧层的职责。
/// 本探针完全绕开帧层、自己提交 + 等栅栏，因此用最小的
/// 「自己开、自己等完再关」结构，避免把帧层的记账语义套到一个
/// 不属于帧层的场景上。
struct OneShotStaging<'a> {
    buffer: Buffer,
    device: &'a ash::Device,
}

impl<'a> OneShotStaging<'a> {
    fn new(gpu: &'a Gpu, size: vk::DeviceSize) -> anyhow::Result<Self> {
        anyhow::ensure!(size > 0, "staging 尺寸必须大于 0");
        Ok(Self {
            buffer: Buffer::new_host_visible(
                gpu,
                size,
                vk::BufferUsageFlags::TRANSFER_SRC,
                vk::SharingMode::EXCLUSIVE,
            )?,
            device: &gpu.device,
        })
    }

    /// 把整块缓冲包成 [`StagingSlice`]。
    ///
    /// `offset` 恒为 0，满足 `vkCmdCopyBufferToImage` 对 `bufferOffset`
    /// 的对齐要求（0 是任何对齐值的倍数）。
    fn slice(&self) -> StagingSlice {
        StagingSlice {
            buffer: self.buffer.handle(),
            memory: self.buffer.memory(),
            offset: 0,
            size: self.buffer.size(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.buffer.write(self.device, 0, bytes)
    }
}

impl Drop for OneShotStaging<'_> {
    fn drop(&mut self) {
        // 调用方必须已等过栅栏（GPU 可能仍在读它）。
        self.buffer.destroy(self.device);
    }
}

/// 自建的一整套离屏渲染资源。
///
/// 拥有清单（**全部**由本探针创建与销毁，不借用 `FrameRenderer` 的任何东西）：
/// 渲染通道、描述符集布局、管线布局、图形管线、着色器模块、描述符池、
/// 描述符集、命令池、命令缓冲、栅栏、uniform 缓冲、采样器、
/// 字体图集、用户纹理、离屏图像、离屏 framebuffer、读回缓冲、
/// 顶点缓冲、索引缓冲。
struct Pixels<'a> {
    gpu: &'a Gpu,
    format: vk::Format,
    render_pass: vk::RenderPass,
    desc_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    shader: vk::ShaderModule,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    uniform: UniformBuffer,
    sampler: Sampler,
    font: DeviceImage,
    user: DeviceImage,
    target: DeviceImage,
    framebuffer: vk::Framebuffer,
    readback: Buffer,
    vertex: Buffer,
    vertex_capacity: usize,
    index: Buffer,
    index_capacity: usize,
    extent: vk::Extent2D,
    /// 离屏资源重建次数。尺寸不变时不应增长。
    rebuilds: u32,
}

impl<'a> Pixels<'a> {
    fn new(gpu: &'a Gpu) -> anyhow::Result<Self> {
        let device = &gpu.device;

        // ---- 渲染通道：按**离屏自己的格式**创建 ------------------------
        let render_pass = modular_clipboard_gfx::pipeline::RenderPass::new(device, TARGET_FORMAT)?;

        // ---- 描述符布局 --------------------------------------------------
        //
        // `update_after_bind = false`：本探针**每个描述符集只写一次**，
        // 且每次渲染后都等栅栏，不存在「被 pending 命令缓冲使用中就
        // 被改写」的情形，因此不需要该标志。
        //
        // 三处（layout flags / pool flags / device feature）保持一致地
        // 「都不加」是最简形态：省掉一整类配对失误的可能性
        // （MEMORY 第 16 批记录过「三处改动缺一即违规」）。
        let desc_layout = modular_clipboard_gfx::pipeline::DescriptorLayout::new(device, false)?;
        let pipe_layout = modular_clipboard_gfx::pipeline::PipelineLayout::new(
            device,
            std::slice::from_ref(&desc_layout.handle),
        )?;
        let shader = modular_clipboard_gfx::shader::ShaderModule::new(device)?;
        let pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
            device,
            render_pass.handle,
            pipe_layout.handle,
            &shader,
        )?;

        // ---- 描述符池 / 描述符集 -----------------------------------------
        // 池的三种计数与 `frame.rs` 一致：1 uniform、1 采样器、
        // **2** 张组合图像采样器（字体 + 用户纹理）。
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: 1,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: 1,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 2,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let desc_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .map_err(|e| anyhow::anyhow!("创建离屏描述符池失败: {e:?}"))?;
        let layouts = [desc_layout.handle];
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(desc_pool)
            .set_layouts(&layouts);
        let sets = unsafe { device.allocate_descriptor_sets(&alloc) }
            .map_err(|e| anyhow::anyhow!("分配离屏描述符集失败: {e:?}"))?;
        anyhow::ensure!(
            sets.len() == 1,
            "描述符集分配数量不符：{}（MEMORY 坑 15：数量取自 set_layouts 的长度）",
            sets.len()
        );
        let desc_set = sets[0];

        // ---- 命令池 / 命令缓冲 / 栅栏 ------------------------------------
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let pool = unsafe { device.create_command_pool(&pool_info, None) }
            .map_err(|e| anyhow::anyhow!("创建离屏命令池失败: {e:?}"))?;
        let cmd_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let bufs = unsafe { device.allocate_command_buffers(&cmd_info) }
            .map_err(|e| anyhow::anyhow!("分配离屏命令缓冲失败: {e:?}"))?;
        anyhow::ensure!(bufs.len() == 1, "命令缓冲分配数量不符：{}", bufs.len());
        let cmd = bufs[0];

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| anyhow::anyhow!("创建离屏栅栏失败: {e:?}"))?;

        // ---- 纹理与缓冲 --------------------------------------------------
        let uniform = UniformBuffer::new(gpu)?;
        let sampler = Sampler::new_nearest(device)?;
        let font = DeviceImage::new(
            gpu,
            vk::Extent2D {
                width: SYNTH_FONT_TEX,
                height: SYNTH_FONT_TEX,
            },
            FONT_FORMAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;
        let user = DeviceImage::new(
            gpu,
            vk::Extent2D {
                width: PLACEHOLDER_USER_TEX,
                height: PLACEHOLDER_USER_TEX,
            },
            TARGET_FORMAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;

        // 初始用一个极小靶面；首次 `set_target` 会重建。
        let extent = vk::Extent2D { width: 8, height: 8 };
        let target = Self::make_target(gpu, extent)?;
        let framebuffer = Self::make_framebuffer(gpu, render_pass.handle, &target, extent)?;
        let readback = Self::make_readback(gpu, extent)?;

        Ok(Self {
            gpu,
            format: TARGET_FORMAT,
            render_pass: render_pass.handle,
            desc_layout: desc_layout.handle,
            pipe_layout: pipe_layout.handle,
            pipeline: pipeline.handle,
            shader: shader.handle(),
            desc_pool,
            desc_set,
            pool,
            cmd,
            fence,
            uniform,
            sampler,
            font,
            user,
            target,
            framebuffer,
            readback,
            vertex: Self::make_vertex(gpu, INITIAL_VERTEX_CAPACITY)?,
            vertex_capacity: INITIAL_VERTEX_CAPACITY,
            index: Self::make_index(gpu, INITIAL_INDEX_CAPACITY)?,
            index_capacity: INITIAL_INDEX_CAPACITY,
            extent,
            rebuilds: 0,
        })
    }

    fn rebuild_count(&self) -> u32 {
        self.rebuilds
    }

    // ---- 录制与提交 -----------------------------------------------------

    /// 独立录制一次命令并提交，等栅栏。
    ///
    /// `body` 在命令缓冲处于录制状态时被调用——这是唯一允许调用
    /// `DeviceImage::upload` 的时机。
    fn record_and_submit(
        &mut self,
        body: impl FnOnce(&mut Self, vk::CommandBuffer) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let cmd = self.cmd;
        unsafe {
            self.gpu
                .device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            self.gpu
                .device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        }
        body(self, cmd)?;
        unsafe { self.gpu.device.end_command_buffer(cmd)? };
        self.submit_and_wait()
    }

    /// 提交并等栅栏，然后复位。
    ///
    /// 用栅栏而不是 `device_wait_idle`：后者会把整条队列强行串行化，
    /// 而 MEMORY 第 7/10 批反复记录了「诊断用的同步点会改变被诊断的
    /// 系统」。本探针不在帧循环里（每次渲染后立刻等），但仍应示范
    /// 「等待范围最小化」。
    fn submit_and_wait(&mut self) -> anyhow::Result<()> {
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cmd));
        unsafe {
            self.gpu
                .device
                .queue_submit(self.gpu.queue, std::slice::from_ref(&submit), self.fence)?;
            self.gpu
                .device
                .wait_for_fences(&[self.fence], true, GPU_WAIT_TIMEOUT_NS)?;
            self.gpu.device.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    // ---- 每帧写入 -------------------------------------------------------

    fn write_uniform(&mut self, data: &Uniforms) -> anyhow::Result<()> {
        self.uniform.write(&self.gpu.device, data)
    }

    /// 把本帧的顶点 / 索引写进私有缓冲，必要时扩容。
    ///
    /// 两个对照共用这一份数据，因此正负差异只可能来自「有没有发 draw」。
    fn set_geometry(&mut self, vertices: &[Vertex], indices: &[u32]) -> anyhow::Result<()> {
        if vertices.len() > self.vertex_capacity {
            let cap = next_capacity(vertices.len());
            // 扩容是安全的：上一次使用这些缓冲的渲染已经等过栅栏。
            self.vertex.destroy(&self.gpu.device);
            self.vertex = Self::make_vertex(self.gpu, cap)?;
            self.vertex_capacity = cap;
        }
        if indices.len() > self.index_capacity {
            let cap = next_capacity(indices.len());
            self.index.destroy(&self.gpu.device);
            self.index = Self::make_index(self.gpu, cap)?;
            self.index_capacity = cap;
        }
        if !vertices.is_empty() {
            self.vertex
                .write(&self.gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        if !indices.is_empty() {
            self.index
                .write(&self.gpu.device, 0, bytemuck::cast_slice(indices))?;
        }
        Ok(())
    }

    /// 把四个绑定写齐。
    ///
    /// 少写任何一个都是 UB：着色器声明了 binding 0/1/2/3，
    /// 未初始化的描述符集被绑定后 GPU 会解引用垃圾值（丢设备）。
    ///
    /// `imageLayout` 取 `DeviceImage::layout`（上传后为
    /// `SHADER_READ_ONLY_OPTIMAL`）。规范禁止把 `UNDEFINED` 写进
    /// `COMBINED_IMAGE_SAMPLER`（`VUID-VkWriteDescriptorSet-descriptorType-04150`）。
    fn write_descriptors(&mut self) -> anyhow::Result<()> {
        let font_info = self.font.descriptor_info(self.sampler.handle());
        let user_info = self.user.descriptor_info(self.sampler.handle());
        let ub_info = self.uniform.buffer().descriptor_info();
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(BINDING_UNIFORM)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&ub_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(BINDING_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&font_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(BINDING_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&font_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(BINDING_USER_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&user_info)),
        ];
        unsafe { self.gpu.device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    /// 渲染一帧到离屏靶面并读回全部像素。
    ///
    /// **负对照与正对照走的就是这个函数**，唯一差别是 `batches`：
    /// 空切片时仍然绑定管线 / 描述符集 / 顶点缓冲 / 索引缓冲，
    /// 只是不发 `cmd_draw_indexed`。这样「差异」必然来自绘制本身，
    /// 而不是「少绑了东西导致画面不同」。
    fn render_and_read(&mut self, batches: &[DrawBatch]) -> anyhow::Result<Vec<u8>> {
        let gpu = self.gpu;
        let device = &gpu.device;
        let cmd = self.cmd;
        let extent = self.extent;
        debug_assert_eq!(
            self.target.format, self.format,
            "靶面格式与探针声明的不一致"
        );

        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;

            // 当前布局 → COLOR_ATTACHMENT_OPTIMAL。
            //
            // 必须显式转换：本探针自建的渲染通道与 `pipeline.rs` 一样声明
            // `initial_layout = COLOR_ATTACHMENT_OPTIMAL`，不做预转换的话
            // 渲染通道的隐式转换与实际布局会脱节。
            let (src_stage, src_access) = match self.target.layout {
                vk::ImageLayout::UNDEFINED => (
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::AccessFlags::empty(),
                ),
                _ => (
                    vk::PipelineStageFlags::TRANSFER,
                    vk::AccessFlags::TRANSFER_READ,
                ),
            };
            self.target.transition_to(
                cmd,
                device,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                src_access,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                src_stage,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            );

            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            };
            device.cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
            device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&area));

            let clear = vk::ClearValue {
                color: vk::ClearColorValue { float32: CLEAR },
            };
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                // 绝不能传空 framebuffer：未定义行为，驱动直接崩（MEMORY 坑 14）。
                .framebuffer(self.framebuffer)
                .render_area(area)
                .clear_values(std::slice::from_ref(&clear));
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);

            // 无论有没有批次都绑定——两个对照必须走同一条路径。
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            let sets = [self.desc_set];
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipe_layout,
                0,
                &sets,
                &[],
            );
            let vbufs = [self.vertex.handle()];
            let offsets = [0];
            device.cmd_bind_vertex_buffers(cmd, 0, &vbufs, &offsets);
            device.cmd_bind_index_buffer(cmd, self.index.handle(), 0, vk::IndexType::UINT32);
            for b in batches {
                // 参数：命令缓冲 / 索引数 / 实例数 / 首索引 / 顶点偏移 / 首实例。
                // 实例数恒为 1；顶点偏移恒为 0（偏移已烘进 index_offset）。
                device.cmd_draw_indexed(cmd, b.index_count, 1, b.index_offset, 0, 0);
            }

            device.cmd_end_render_pass(cmd);

            // COLOR_ATTACHMENT_OPTIMAL → TRANSFER_SRC_OPTIMAL
            self.target.transition_to(
                cmd,
                device,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
            );

            let copy = vk::BufferImageCopy::default()
                .buffer_offset(0)
                // 0 = 紧密排列，与 4 字节/像素对应。
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cmd,
                self.target.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback.handle(),
                &[copy],
            );
            device.end_command_buffer(cmd)?;
        }

        self.submit_and_wait()?;

        self.readback.map(device)?;
        let all = self.readback.read::<u8>()?;
        let need = (extent.width as usize) * (extent.height as usize) * 4;
        anyhow::ensure!(
            all.len() >= need,
            "读回只有 {} 字节，不足 {need}",
            all.len()
        );
        let out = all[..need].to_vec();
        self.readback.unmap(device)?;
        Ok(out)
    }

    // ---- 离屏资源重建 ---------------------------------------------------

    /// 把离屏资源重建到 `extent`。尺寸相同时是空操作。
    ///
    /// 销毁顺序与创建相反：framebuffer 引用了图像视图，
    /// 反序会留下悬空引用（MEMORY 坑 14）。
    fn set_target(&mut self, extent: vk::Extent2D) -> anyhow::Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        let device = &self.gpu.device;
        // 显式 wait_idle：让「销毁前 GPU 空闲」这条硬前置
        // （`Buffer::destroy` / `DeviceImage::destroy` 的文档要求）
        // 不依赖调用顺序。上一次渲染其实已经等过栅栏，这里是纵深防御。
        self.gpu.wait_idle();

        unsafe { device.destroy_framebuffer(self.framebuffer, None) };
        self.framebuffer = vk::Framebuffer::null();
        self.target.destroy(device);
        self.readback.destroy(device);

        self.target = Self::make_target(self.gpu, extent)?;
        self.framebuffer =
            Self::make_framebuffer(self.gpu, self.render_pass, &self.target, extent)?;
        self.readback = Self::make_readback(self.gpu, extent)?;
        self.extent = extent;
        self.rebuilds += 1;

        // 描述符集本身与靶面尺寸无关（它引用的是字体图集、用户纹理与
        // uniform 缓冲，三者都不随靶面变化），但重建后无条件重写一遍，
        // 把「描述符与当前资源同步」从假设变成显式事实。
        self.write_descriptors()
    }

    /// 字体图集扩容时重建图像。旧图像可能仍被在途命令引用，故先等空闲。
    fn ensure_font_size(&mut self, w: u32, h: u32) -> anyhow::Result<()> {
        if self.font.size.width == w && self.font.size.height == h {
            return Ok(());
        }
        self.gpu.wait_idle();
        self.font.destroy(&self.gpu.device);
        self.font = DeviceImage::new(
            self.gpu,
            vk::Extent2D { width: w, height: h },
            FONT_FORMAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;
        Ok(())
    }

    fn make_target(gpu: &Gpu, extent: vk::Extent2D) -> anyhow::Result<DeviceImage> {
        // 必须含 TRANSFER_SRC：`cmd_copy_image_to_buffer` 对它是非法的
        // 除非带这个用途位。交换链图像恰恰**没有**这一位，
        // 这也是本探针不用交换链的原因之一。
        DeviceImage::new(
            gpu,
            extent,
            TARGET_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        )
    }

    fn make_framebuffer(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        target: &DeviceImage,
        extent: vk::Extent2D,
    ) -> anyhow::Result<vk::Framebuffer> {
        let attachments = [target.view];
        let info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&attachments)
            .width(extent.width)
            .height(extent.height)
            .layers(1);
        unsafe { gpu.device.create_framebuffer(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建离屏 framebuffer 失败: {e:?}"))
    }

    fn make_readback(gpu: &Gpu, extent: vk::Extent2D) -> anyhow::Result<Buffer> {
        Buffer::new_host_visible(
            gpu,
            (extent.width as vk::DeviceSize) * (extent.height as vk::DeviceSize) * 4,
            vk::BufferUsageFlags::TRANSFER_DST,
            vk::SharingMode::EXCLUSIVE,
        )
    }

    fn make_vertex(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
        Buffer::new_host_visible(
            gpu,
            (capacity * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            vk::SharingMode::EXCLUSIVE,
        )
    }

    fn make_index(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
        Buffer::new_host_visible(
            gpu,
            (capacity * std::mem::size_of::<u32>()) as vk::DeviceSize,
            vk::BufferUsageFlags::INDEX_BUFFER,
            vk::SharingMode::EXCLUSIVE,
        )
    }

    /// 逆序销毁：引用者 → 被引用者。调用前必须已 `device_wait_idle`。
    fn destroy(&mut self) {
        let device = &self.gpu.device;
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.pool, None);
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_descriptor_pool(self.desc_pool, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_shader_module(self.shader, None);
            device.destroy_pipeline_layout(self.pipe_layout, None);
            device.destroy_descriptor_set_layout(self.desc_layout, None);
            device.destroy_render_pass(self.render_pass, None);
        }
        self.framebuffer = vk::Framebuffer::null();
        self.desc_pool = vk::DescriptorPool::null();
        self.readback.destroy(device);
        self.vertex.destroy(device);
        self.index.destroy(device);
        self.uniform.destroy(device);
        self.sampler.destroy(device);
        self.target.destroy(device);
        self.user.destroy(device);
        self.font.destroy(device);
    }
}

// ---------------------------------------------------------------------------
// 界面 / 顶点数据
// ---------------------------------------------------------------------------

/// 画一个能验证「文字 + 颜色」都正常的界面。
///
/// 背景矩形用 [`bg_as_egui_color`]——与清屏色**逐字节相同**。
/// 于是正对照里的「非背景像素」只可能来自文字与色块，
/// 判据的上界（`<` 全屏像素数）才有意义。
fn draw_ui(ui: &mut egui::Ui, expect: (u32, u32)) {
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, bg_as_egui_color());
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.heading("ModularClipboard pixels_probe");
        ui.label("中文字体应正常显示：剪贴板历史记录 · 搜索 · 详情");
        ui.label("ASCII baseline: The quick brown fox 0123456789");
    });
    ui.add_space(14.0);
    ui.vertical_centered(|ui| {
        ui.label(format!("靶面 {}x{}", expect.0, expect.1));
        ui.label("每次换尺寸后应仍能正确显示本行文字");
    });
    ui.add_space(14.0);
    // 色块：验证颜色通道在各尺寸下仍正确。
    ui.horizontal_centered(|ui| {
        for c in [
            egui::Color32::from_rgb(0x4C, 0x8D, 0xF6),
            egui::Color32::from_rgb(0xF6, 0x8D, 0x4C),
            egui::Color32::from_rgb(0x4C, 0xF6, 0x8D),
        ] {
            let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(70.0, 40.0));
            ui.painter().rect_filled(rect, 6.0, c);
            ui.allocate_space(egui::vec2(78.0, 40.0));
        }
    });
}

/// 安装中文字体。找不到系统字体时退回内置（中文显示为方块，
/// 但 ASCII 与色块仍可验证）。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/simsun.ttc",
    ];

    if std::env::var_os("TIEZ_NO_CJK_FONT").is_some() {
        println!("TIEZ_NO_CJK_FONT 已设置，跳过中文字体");
        return;
    }

    let mut defs = egui::FontDefinitions::default();
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = 0;
        let name = "cjk".to_string();
        defs.font_data
            .insert(name.clone(), std::sync::Arc::new(data));
        // 插到 Proportional 首位，保证中文字形优先命中。
        let mut families = defs.families.clone();
        families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, name);
        defs.families = families;
        println!("OK 中文字体已加载：{path}");
        ctx.set_fonts(defs);
        return;
    }
    println!("[提示] 未找到中文字体，中文将显示为方块（ASCII 与色块仍可验证）");
    ctx.set_fonts(defs);
}

/// 把 egui 图元转成顶点 / 索引 / 批次。
///
/// 与 `full_app::tessellate_into` 同源：索引按 mesh 偏移、
/// 相邻同 clip 的图元合并成一批。
fn tessellate_into(
    primitives: &[egui::epaint::ClippedPrimitive],
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
) {
    vertices.clear();
    indices.clear();
    batches.clear();

    for prim in primitives {
        let mesh = match &prim.primitive {
            egui::epaint::Primitive::Mesh(m) => m,
            // PaintCallback 需要用户代码参与光栅化，本探针不支持。
            egui::epaint::Primitive::Callback(_) => continue,
        };
        if mesh.indices.is_empty() {
            continue;
        }
        let base = vertices.len() as u32;
        let index_start = indices.len() as u32;
        let index_count = mesh.indices.len() as u32;

        for v in &mesh.vertices {
            vertices.push(Vertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: pack_color(v.color),
                // 本探针只画字体图集，0 = 字体图集。
                tex_id: 0,
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));

        let merge = batches.last().is_some_and(|b| {
            b.clip == prim.clip_rect && b.index_offset + b.index_count == index_start
        });
        if merge {
            let last = batches.last_mut().expect("刚判定过非空");
            last.index_count += index_count;
        } else {
            batches.push(Batch {
                index_offset: index_start,
                index_count,
                clip: prim.clip_rect,
            });
        }
    }
}

/// egui 的 `Color32` → 顶点色 `u32`（小端 ABGR），反预乘。
///
/// epaint 顶点色是 **sRGBA 预乘 alpha**，而 `pipeline.rs` 的混合是
/// `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`（**非预乘**）。不还原会让半透明区发黑
/// ——不报任何 GPU 错误，只是「看起来脏」，极易被漏掉。
fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    let (r, g, b) = unpremultiply(r, g, b, a);
    u32::from_le_bytes([r, g, b, a])
}

/// 预乘 → 非预乘。`a == 0` 时 rgb 的信息已在预乘时丢失，取 0。
fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 255 {
        return (r, g, b);
    }
    if a == 0 {
        // 除零边界。理论上不可达（egui 的 Color32 是预乘的，a=0 时 rgb 必为 0），
        // 但仍显式返回 0 而不是让除法 panic ——「理论上不可达」的分支
        // 恰恰是最需要写对的地方。
        return (0, 0, 0);
    }
    // 用 u32 中转：r * 255 最大 65025，会溢出 u8。
    let f = |c: u8| -> u8 { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
    (f(r), f(g), f(b))
}

/// 一批绘制。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Batch {
    index_offset: u32,
    index_count: u32,
    clip: egui::Rect,
}

impl Batch {
    /// 与生产 `modular_clipboard_ui::renderer::Batch::to_draw` 保持一致：
    /// 裁剪矩形必须换算成物理像素并逐批提交 scissor，否则探针测的不是
    /// 产品那条路径（见 full_app.rs 里同名的注释）。
    fn to_draw(self, extent: ash::vk::Extent2D, ppp: f32) -> DrawBatch {
        let ppp = if ppp.is_finite() && ppp > 0.0 { ppp } else { 1.0 };
        let c = self.clip;
        let x0 = (c.min.x * ppp).floor().max(0.0) as i32;
        let y0 = (c.min.y * ppp).floor().max(0.0) as i32;
        let x1 = (c.max.x * ppp).ceil().max(0.0) as i32;
        let y1 = (c.max.y * ppp).ceil().max(0.0) as i32;
        let (vw, vh) = (extent.width as i64, extent.height as i64);
        let x0 = x0.clamp(0, vw as i32);
        let y0 = y0.clamp(0, vh as i32);
        let x1 = x1.clamp(0, vw as i32);
        let y1 = y1.clamp(0, vh as i32);
        DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
            clip: Some(ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: x0, y: y0 },
                extent: ash::vk::Extent2D {
                    width: (x1 - x0).max(0) as u32,
                    height: (y1 - y0).max(0) as u32,
                },
            }),
        }
    }
}

/// 离开作用域时清空 `TexturesDelta`。
///
/// `TexturesDelta::drop` 有 `debug_assert!(is_empty())`：增量必须被消费或
/// 显式 `clear()`。手写 `clear()` 极易漏（`?` 提前返回、`continue`
/// 分支各都要写），漏一处就是一个与真实错误毫无关系的 panic，
/// 把排查方向带偏。
struct DeltaGuard(egui::TexturesDelta);

impl Drop for DeltaGuard {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// 应用 egui 的字体图集增量，返回本帧的上传次数。
///
/// 与 `full_app::apply_font_delta` 的差别：**自己录制 + 等栅栏**，
/// 不录进帧槽位的命令缓冲（本探针没有帧槽位）。
///
/// staging 每帧开一块、等完再关——「上传函数内建局部 staging」是
/// use-after-free 的经典来源（MEMORY 第 3 批坑 28）。
///
/// **只有整图更新（`pos == None`）才意味着图集扩容**：局部更新的
/// `image.size()` 是补丁尺寸，拿它比对会把 2048x32 的图集换成 4x10 的
/// 小图，随后坐标全部越界（这个 bug 由 `full_app` 实机跑出来过）。
fn apply_font_delta(
    gpu: &Gpu,
    px: &mut Pixels<'_>,
    delta: &mut egui::TexturesDelta,
) -> anyhow::Result<u64> {
    let mut uploads = 0u64;
    // egui 约定 `Managed(0)` 恒为字体图集。
    const FONT_ID: egui::TextureId = egui::TextureId::Managed(0);

    for (id, deltas) in &delta.set {
        if *id != FONT_ID {
            // 用户纹理本探针不用（全是 tex_id = 0），显式跳过。
            continue;
        }
        for d in deltas {
            let patch = d.image.size();
            let (w, h) = (patch[0] as u32, patch[1] as u32);
            anyhow::ensure!(w > 0 && h > 0, "字体图集增量尺寸为 0：{patch:?}");
            if d.pos.is_none() {
                px.ensure_font_size(w, h)?;
            }
            let bytes = DeviceImage::coverage_bytes(&d.image);
            let offset = d.pos.map(|p| (p[0] as u32, p[1] as u32));
            let len = bytes.len() as vk::DeviceSize;

            // staging 必须活到 GPU 读完为止，所以每帧开一块、等完再关。
            let mut staging = OneShotStaging::new(gpu, len)?;
            staging.write(&bytes)?;
            let slice = staging.slice();
            px.record_and_submit(|px, cmd| {
                px.font.upload(gpu, cmd, slice, &bytes, offset, (w, h))
            })?;
            uploads += 1;
        }
    }

    // `TexturesDelta` 在 drop 时断言增量为空。本函数只消费字体图集，
    // 其余一律忽略，故显式清空。
    delta.clear();
    Ok(uploads)
}

// ---------------------------------------------------------------------------
// 测试（不建窗口，不跑 GPU，CI 可跑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 判据自洽：BG 必须由 CLEAR 推导 -------------------------------
    //
    // 这是 MEMORY 第 19 批坑 91 的直接对策：旧探针的 `BG` 与清屏色
    // 各写一份且不一致（`[0x1E,0x1E,0x22]` vs `[0,0,0]`），
    // 于是每个像素都算「非背景」，负对照检出 576000 = 全屏像素数。

    #[test]
    fn bg_matches_clear_byte_for_byte() {
        // BG 是编译期常量；改 CLEAR 而不改推导逻辑时这条会失败。
        assert_eq!(BG, [unorm8(CLEAR[0]), unorm8(CLEAR[1]), unorm8(CLEAR[2])]);
        assert!(bg_is_derived_from_clear());
    }

    #[test]
    fn clear_and_bg_agree_byte_for_byte() {
        // 清屏色的每个分量量化回 8 位后必须与 BG 逐字节相同。
        // 容差为 0：这条不允许任何舍入余量，否则负对照就没有确定答案。
        for (i, c) in CLEAR.iter().take(3).enumerate() {
            let q = unorm8(*c);
            assert_eq!(q, BG[i], "CLEAR[{i}]={c} 量化后是 {q}，BG[{i}]={}", BG[i]);
        }
    }

    #[test]
    fn ui_background_is_byte_identical_to_clear() {
        // 界面铺的背景矩形必须与清屏色**完全一致**，
        // 否则正对照的「非背景像素」会把背景也算进去，
        // 上界判据（`<` 全屏像素数）就失去意义。
        let c = bg_as_egui_color();
        assert_eq!([c.r(), c.g(), c.b()], BG);
        assert_eq!(c.a(), 255, "背景必须不透明，否则混合结果不等于清屏色");
    }

    // ---- 判据本身：喂字节，断言结论 -------------------------------------

    /// 构造「纯背景」字节：每个像素都是 BG + 不透明 alpha。
    fn pure_background_bytes(pixels: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(pixels * 4);
        for _ in 0..pixels {
            v.extend_from_slice(&[BG[0], BG[1], BG[2], 255]);
        }
        v
    }

    /// 构造「正对照」字节：整片背景 + 中间一块明显不同的内容。
    ///
    /// 这模拟「界面铺了背景 + 画了文字与色块」的读回形状。
    fn positive_control_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut v = pure_background_bytes((w * h) as usize);
        // 中间一半边长的区域填成白色（与背景差 0xE1，远超容差）。
        for y in (h / 4)..(h * 3 / 4) {
            for x in (w / 4)..(w * 3 / 4) {
                let i = ((y * w + x) * 4) as usize;
                v[i] = 255;
                v[i + 1] = 255;
                v[i + 2] = 255;
            }
        }
        v
    }

    #[test]
    fn negative_control_bytes_are_empty() {
        // 负对照的字节必须被判为「无内容」且计数恰好 0。
        // 这是「判据与被测实现一致」的机器化表述。
        let bytes = pure_background_bytes(4096);
        assert_eq!(count_non_background(&bytes), 0);
        assert_eq!(classify(&bytes), Content::Empty);
    }

    #[test]
    fn positive_control_bytes_are_drawn() {
        let bytes = positive_control_bytes(64, 64);
        let n = count_non_background(&bytes);
        assert_eq!(n, (32 * 32) as usize, "非背景计数应恰好等于内容面积");
        assert_eq!(classify(&bytes), Content::Drawn);
        assert!(n < 64 * 64, "非背景计数必须小于全屏像素数（上界判据）");
    }

    /// 判据可辨异性：同一份判据必须对两组字节给出**相反**结论。
    ///
    /// 直接对应「如果渲染真的坏了，检出会不会变？」——
    /// 若哪天 `classify` 变成恒真或恒假，本测试立刻失败。
    #[test]
    fn criterion_separates_positive_from_negative() {
        let neg = pure_background_bytes(64 * 64);
        let pos = positive_control_bytes(64, 64);
        assert_eq!(classify(&neg), Content::Empty);
        assert_eq!(classify(&pos), Content::Drawn);
        assert_ne!(classify(&neg), classify(&pos));
        assert!(count_non_background(&pos) > count_non_background(&neg));
    }

    /// 若渲染彻底坏掉（什么都没画出来），正对照会退化成负对照，
    /// 于是 `pos_count > 0` 失败 ⇒ 探针报错。这条把「坏掉后的后果」
    /// 写成可执行断言。
    #[test]
    fn broken_rendering_would_be_detected() {
        let broken = pure_background_bytes(64 * 64);
        assert_eq!(
            classify(&broken),
            Content::Empty,
            "渲染坏掉时正对照会退化为空 —— 主判据 pos_count > 0 必然失败"
        );
    }

    /// 若背景矩形没画出来（整片被填成别的颜色），计数会等于全屏
    /// ⇒ 上界判据 `pos_count < total` 失败。
    #[test]
    fn missing_background_would_be_detected() {
        let (w, h) = (64u32, 64u32);
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            v.extend_from_slice(&[0xFF, 0x00, 0xFF, 255]);
        }
        assert_eq!(
            count_non_background(&v),
            (w * h) as usize,
            "全屏非背景时上界判据必然失败"
        );
    }

    // ---- 容差边界 -------------------------------------------------------

    #[test]
    fn tolerance_boundary_is_exactly_two() {
        // 差 <= 容差不算内容。
        for delta in 0..=BG_TOLERANCE {
            let p0 = (i32::from(BG[0]) + delta).clamp(0, 255) as u8;
            assert_eq!(
                count_non_background(&[p0, BG[1], BG[2], 255]),
                0,
                "差 {delta} 应仍在容差内"
            );
        }
        // 差 > 容差算内容。
        let p0 = (i32::from(BG[0]) + BG_TOLERANCE + 1).clamp(0, 255) as u8;
        assert_eq!(
            count_non_background(&[p0, BG[1], BG[2], 255]),
            1,
            "超出容差必须算内容"
        );
    }

    #[test]
    fn quantizer_rounds_to_nearest() {
        // 驱动对 f * 255 可能截断也可能四舍五入；
        // unorm8 加 0.5 后截断，在两种实现下都给出正确字节。
        assert_eq!(unorm8(0.0), 0);
        assert_eq!(unorm8(1.0), 255);
        assert_eq!(unorm8(0.5), 128);
        // 往返一致：v/255 必须量化回 v。
        for v in [0x1Eu8, 0x22, 0x4C, 0x8D, 0xF6, 0x00, 0xFF] {
            assert_eq!(unorm8(v as f32 / 255.0), v, "{v:#04x} 量化往返失败");
        }
        // 越界输入被夹住，不 panic。
        assert_eq!(unorm8(-1.0), 0);
        assert_eq!(unorm8(2.0), 255);
    }

    #[test]
    fn alpha_channel_is_not_part_of_the_criterion() {
        // 判据只看 RGB：alpha 不同不应改变结论。
        // 混合的 alpha 语义由 `unpremultiply` 与管线的
        // srcAlpha/oneMinusSrcAlpha 负责，与「有没有画东西」无关。
        let mut with_a = pure_background_bytes(16);
        with_a[3] = 0;
        assert_eq!(count_non_background(&with_a), 0);
    }

    #[test]
    fn readback_length_check_rejects_short_buffers() {
        // 长度不对时 `classify` 会给出**看似合理**的结论
        // （chunks_exact 静默丢尾部），所以长度必须被卡住。
        let extent = vk::Extent2D {
            width: 4,
            height: 4,
        };
        assert!(verify_len(&pure_background_bytes(16), extent, "测试").is_ok());
        assert!(verify_len(&pure_background_bytes(15), extent, "测试").is_err());
        assert!(verify_len(&pure_background_bytes(17), extent, "测试").is_err());
    }

    // ---- 尺寸序列 -------------------------------------------------------

    #[test]
    fn target_sequence_has_distinct_neighbours() {
        let mut prev = TARGET_SEQ[0];
        for &s in &TARGET_SEQ[1..] {
            assert_ne!(s, prev, "相邻两档尺寸相同则不会触发离屏资源重建");
            prev = s;
        }
    }

    #[test]
    fn target_sequence_ends_where_it_started() {
        // 最后一档回到第一档：验证「改回去」也能正确重建。
        assert_eq!(TARGET_SEQ.last().copied(), TARGET_SEQ.first().copied());
    }

    #[test]
    fn target_sequence_covers_both_directions() {
        let first = TARGET_SEQ[0];
        assert!(
            TARGET_SEQ.iter().any(|&s| s.0 > first.0),
            "序列应包含一次变大"
        );
        assert!(
            TARGET_SEQ.iter().any(|&s| s.0 < first.0),
            "序列应包含一次变小"
        );
    }

    #[test]
    fn every_target_size_is_positive() {
        for &(tw, th) in TARGET_SEQ {
            assert!(tw > 0 && th > 0, "靶面尺寸必须非 0：{tw}x{th}");
        }
    }

    #[test]
    fn target_format_byte_order_matches_clear_channels() {
        // 判据按 R,G,B 顺序比较字节，因此格式必须是 R8G8B8A8。
        // 换成 B8G8R8A8 会让 BG 的 R 与 B 互换——背景是接近灰度的颜色，
        // 也许「碰巧不出错」，但那正是 MEMORY 坑 84 说的
        // 「测试验证的是自己重算的值」。这条把选择理由固定下来。
        assert_eq!(TARGET_FORMAT, vk::Format::R8G8B8A8_UNORM);
        // 4 字节 / 像素：readback 的尺寸计算与 BufferImageCopy 的
        // 紧密排列都依赖这一点。
        assert_eq!(
            modular_clipboard_gfx::texture::staging_size(TARGET_FORMAT, 7, 5),
            7 * 5 * 4
        );
    }

    // ---- 几何 -----------------------------------------------------------

    #[test]
    fn vertex_stride_matches_pipeline() {
        // pipeline.rs 声明 pos(8)+uv(8)+color(4)+tex_id(4) = 24 字节。
        // 断言真实大小而非重算（MEMORY 第 17 批坑 84）。
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
    }

    #[test]
    fn capacity_growth_is_monotonic() {
        assert!(next_capacity(1) >= 1);
        assert!(next_capacity(2000) >= 2000);
        assert!(next_capacity(next_capacity(2000)) > next_capacity(2000));
    }

    #[test]
    fn placeholder_texture_is_fully_opaque_and_right_sized() {
        let bytes = placeholder_rgba();
        let n = (PLACEHOLDER_USER_TEX * PLACEHOLDER_USER_TEX * 4) as usize;
        assert_eq!(bytes.len(), n);
        assert!(
            bytes.chunks_exact(4).all(|p| p[3] == 255),
            "占位纹理必须不透明：半透明会让描述符布局测试失真"
        );
    }

    #[test]
    fn wait_timeout_is_finite() {
        // 无限等待会把「设备卡住」变成无声挂起。
        assert!(GPU_WAIT_TIMEOUT_NS > 0);
        assert!(GPU_WAIT_TIMEOUT_NS <= 60_000_000_000);
    }

    // ---- egui 真实路径 ---------------------------------------------------

    /// 跑一帧真实 egui，返回转换后的顶点 / 索引 / 批次。
    fn run_one_frame(size: (u32, u32)) -> (Vec<Vertex>, Vec<u32>, Vec<Batch>) {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0 as f32, size.1 as f32),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| draw_ui(ui, size));
        // `TexturesDelta` 在 drop 时断言增量为空。本测试不消费纹理增量
        // （那需要 GPU），必须显式清空，否则 panic 会掩盖真正的断言失败。
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        (v, i, b)
    }

    #[test]
    fn real_egui_produces_geometry() {
        let (v, i, b) = run_one_frame((320, 240));
        assert!(!v.is_empty(), "egui 应产出顶点");
        assert!(!b.is_empty(), "应有绘制批次");
        // 索引必须全部落在顶点数组内，否则 draw_indexed 会越界读取。
        let n = v.len() as u32;
        assert!(i.iter().all(|&x| x < n), "存在越界索引");
        assert_eq!(i.len() % 3, 0, "索引数必须是 3 的倍数（三角形列表）");
    }

    #[test]
    fn all_primitives_use_the_font_atlas() {
        // 本探针不测缩略图，tex_id 必须恒为 0；
        // 若混入 1，正对照会因采样占位纹理而得到非预期颜色。
        let (v, _, _) = run_one_frame((320, 240));
        assert!(v.iter().all(|x| x.tex_id == 0));
    }

    #[test]
    fn batches_cover_all_indices_exactly_once() {
        // 批次区间必须无缝铺满索引数组：既不重叠也不留空洞。
        // 空洞会导致部分三角形没被画（画面局部空白），
        // 而那在读回里表现为「非背景像素偏少」——不易察觉。
        let (_, i, b) = run_one_frame((320, 240));
        let mut next = 0u32;
        for batch in &b {
            assert_eq!(batch.index_offset, next, "批次之间出现空洞或重叠");
            next = batch.index_offset + batch.index_count;
        }
        assert_eq!(next as usize, i.len(), "批次应覆盖全部索引");
    }

    #[test]
    fn every_target_size_produces_geometry() {
        // 判据的正对照要求每个尺寸都有图元；
        // 若某个尺寸下 egui 不产出东西（例如太窄），实机才暴露。
        for &(tw, th) in TARGET_SEQ {
            let (v, i, b) = run_one_frame((tw, th));
            assert!(
                !v.is_empty() && !i.is_empty() && !b.is_empty(),
                "尺寸 {tw}x{th} 下 egui 没有产出图元"
            );
        }
    }

    #[test]
    fn batch_conversion_preserves_offsets() {
        let b = Batch {
            index_offset: 12,
            index_count: 6,
            clip: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1.0, 1.0)),
        };
        let d = b.to_draw();
        assert_eq!(d.index_offset, 12);
        assert_eq!(d.index_count, 6);
    }

    // ---- 颜色打包 -------------------------------------------------------

    #[test]
    fn color_is_packed_as_abgr_little_endian() {
        let c = egui::Color32::from_rgb(0x11, 0x22, 0x33);
        let packed = pack_color(c);
        assert_eq!(packed & 0xFF, 0x11, "最低字节应是红");
        assert_eq!((packed >> 8) & 0xFF, 0x22);
        assert_eq!((packed >> 16) & 0xFF, 0x33);
        assert_eq!((packed >> 24) & 0xFF, 0xFF, "最高字节应是 alpha");
    }

    #[test]
    fn opaque_colors_survive_unpremultiply() {
        for c in [
            egui::Color32::WHITE,
            egui::Color32::BLACK,
            egui::Color32::from_rgb(1, 127, 254),
        ] {
            assert_eq!(
                unpremultiply(c.r(), c.g(), c.b(), 255),
                (c.r(), c.g(), c.b())
            );
        }
    }

    #[test]
    fn unpremultiply_handles_zero_alpha() {
        assert_eq!(unpremultiply(0, 0, 0, 0), (0, 0, 0));
        let t = egui::Color32::TRANSPARENT;
        assert_eq!(unpremultiply(t.r(), t.g(), t.b(), t.a()), (0, 0, 0));
    }

    #[test]
    fn unpremultiply_never_exceeds_255() {
        for a in 1..=254u8 {
            let (r, g, b) = unpremultiply(255, 255, 255, a);
            // 用 u32 中转比较：直接写 `v <= 255` 在 u8 上恒真。
            for v in [r, g, b] {
                assert!(u32::from(v) <= 255, "反预乘溢出 u8");
            }
        }
    }

    // ---- uniform --------------------------------------------------------

    #[test]
    fn uniform_carries_the_target_size() {
        // 着色器用 size_in_pixels 做像素 → NDC。若尺寸不是当前靶面，
        // 画面会被拉伸或裁掉——表现为「内容位置不对」，
        // 而非「没有内容」，很容易被读成噪声。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(u.size_in_pixels, [800.0, 600.0]);
    }

    #[test]
    fn uniform_matrix_is_identity() {
        // 着色器已自己做像素 → NDC；再乘非单位矩阵会二次变换。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(u.clip_from_uv, IDENTITY);
    }
}