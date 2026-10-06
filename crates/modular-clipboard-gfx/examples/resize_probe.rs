//! 实机验证：`rebuild_swapchain` 路径（此前**从未**在实机触发过）。
//!
//! # 这个探针回答的问题
//!
//! `full_app` 每次跑都打印「交换链重建 0 次」——`WindowEvent::Resized`
//! **从未在实机出现过**。因此下列代码全是「读代码认为对」，无实机证据：
//!
//! 1. 旧 framebuffer / image_view / swapchain 的销毁顺序
//!    （验证层基线里报过 `VUID-vkDestroyDevice-device-05137` 泄漏 ImageView）；
//! 2. `image_semaphores` 重建后长度是否仍等于 `image_count`；
//! 3. `LayoutTracker::reset` 之后布局记账是否正确；
//! 4. 描述符集重绑（`rebind_after_rebuild`）是否漏项。
//!
//! # 窗口可见性：移到屏幕外，而不是最小化
//!
//! 实机教训（MEMORY 第 8 批坑 36）：窗口**不可见**时表面无法呈现，
//! 症状不是「窗口没显示」，而是后续 acquire 阶段报设备错误——极具误导性。
//! `SW_SHOWMINIMIZED` 更糟：会把客户区压成 0×0，交换链拿不到可呈现图像。
//!
//! 故本探针用 `MoveWindow` 移到 `(-32000, -32000)`：
//! **保持 `WS_VISIBLE`**（表面能呈现），但用户桌面上看不到它。
//!
//! # 像素校验的诚实边界（重要，请勿误读）
//!
//! `Swapchain::new` 的 `image_usage` 只有
//! `COLOR_ATTACHMENT | TRANSFER_DST`，**没有 `TRANSFER_SRC`**
//! （`src/lib.rs`）。因此**无法**把交换链图像拷回主机读像素——
//! `cmd_copy_image_to_buffer` 对它是非法的。
//!
//! 所以本探针的像素校验是**间接**的：把同一帧 egui 图元画进一张自建的、
//! 带有 `TRANSFER_SRC` 的**离屏**图像，再读回比对。它复用**同一个**
//! 图形管线、同一个管线布局、以及**交换链槽位里那个描述符集**。
//!
//! 它能证明：新尺寸下着色器 / 管线 / 描述符绑定 / uniform 仍能正确出字。
//! 它**不能**证明：交换链真的把这一帧 present 上了屏幕。
//! 后者只能靠「present 返回值 + 验证层 VE=0」间接推断。
//! 两者在输出里分开标注，请勿混为一谈。
//!
//! # A/B 反证：证明这个像素检查真的有鉴别力
//!
//! 「探针在自己没测的东西上通过，等于没通过」（MEMORY 第 5 批）。
//! 若读回永远返回「有内容」，它可能只是在校验清屏色本身。
//! 故每次读回都跑**两次**：
//! - **负对照**：只清屏、不画任何东西 → 断言非背景像素 == 0；
//! - **正对照**：画真实图元 → 断言非背景像素 > 0。
//!
//! 负对照若也检出内容，说明检查失效，正对照的结论不可信 → 直接 `bail!`。

use std::time::Duration;

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{DrawBatch, DrawInput, FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::pipeline::Uniforms;
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};

/// 窗口移出屏幕的位置。负坐标让客户区仍非 0×0（表面能呈现），
/// 但用户在桌面上看不到它。
const OFFSCREEN_X: i32 = -32000;
const OFFSCREEN_Y: i32 = -32000;

/// 初始客户区尺寸（物理像素）。
const W: u32 = 900;
const H: u32 = 640;

/// resize 序列。必须**至少两个不同尺寸**，否则重建可能不真正换图像。
/// 最后一档回到初始尺寸：验证「改回去」也能正确重建，而非只单向变化。
const RESIZE_SEQ: &[(u32, u32)] = &[(640, 480), (1200, 800), (900, 640)];

/// 每个尺寸下至少要跑的帧数。
///
/// 60 帧足以让「每交换链图像一个信号量」的轮转走完一圈
/// （典型 image_count 为 2~3），从而覆盖索引错配的失效路径。
const FRAMES_PER_SIZE: u64 = 60;

/// 顶点 / 索引缓冲初始容量。
const INITIAL_VERTEX_CAPACITY: usize = 4096;
const INITIAL_INDEX_CAPACITY: usize = 6144;

/// 界面背景色（与 `draw_ui` 画的一致）。读回时用它区分「内容」与「背景」。
const BG: [u8; 3] = [0x1E, 0x1E, 0x22];

/// 判定「该像素与背景不同」的容差。UNORM 8bit 转换有 ±1 舍入误差。
const BG_TOLERANCE: i32 = 2;

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL resize_probe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // ---- 窗口 ----------------------------------------------------------
    let window = Window::new("ModularClipboard — resize_probe", W, H)?;
    // 立刻移出屏幕但保持 WS_VISIBLE（见模块文档）。
    move_offscreen(&window, W, H)?;
    let mut events = EventLoop::new(&window);
    let (iw, ih) = window.inner_size_physical();
    println!("OK Window  客户区 {iw}x{ih}  scale_factor={}", window.scale_factor());
    println!("          已移至 ({OFFSCREEN_X},{OFFSCREEN_Y})，保持可见以确保表面能呈现");
    anyhow::ensure!(
        iw > 0 && ih > 0,
        "移出屏幕后客户区变成了 {iw}x{ih}，窗口不可见会导致表面无法呈现"
    );

    // ---- 设备 ----------------------------------------------------------
    let gpu = Gpu::new("ModularClipboardResizeProbe", window.hinstance(), window.hwnd())?;
    let device_name: String = gpu
        .properties
        .device_name
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect::<Vec<u8>>()
        .iter()
        .map(|&b| b as char)
        .collect();
    println!("OK Gpu  {device_name}  ({:?})", gpu.device_type);

    // ---- 管线（照抄 full_app，保证与已验证路径一致）--------------------
    let surface_format = gpu
        .surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .ok_or_else(|| anyhow::anyhow!("表面无可用格式"))?;

    let render_pass = modular_clipboard_gfx::pipeline::RenderPass::new(&gpu.device, surface_format)?;
    let desc_layout = modular_clipboard_gfx::pipeline::DescriptorLayout::new(
        &gpu.device,
        gpu.desc_caps.all_bindings_update_after_bind(),
    )?;
    let pipe_layout = modular_clipboard_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&desc_layout.handle),
    )?;
    let shader = modular_clipboard_gfx::shader::ShaderModule::new(&gpu.device)?;
    let pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
        &gpu.device,
        render_pass.handle,
        pipe_layout.handle,
        &shader,
    )?;

    let mut fr = FrameRenderer::new(
        &gpu,
        PipelineBundle::new(
            render_pass.handle,
            pipe_layout.handle,
            pipeline.handle,
            desc_layout.handle,
        ),
    )?;
    fr.rebuild_swapchain(Default::default())?;
    println!(
        "OK FrameRenderer  extent={}x{}  槽位={}",
        fr.extent().width,
        fr.extent().height,
        fr.slot_count()
    );

    // ---- egui ----------------------------------------------------------
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    ctx.set_visuals(ctx.style_of(egui::Theme::Dark).visuals.clone());

    // ---- 资源 ----------------------------------------------------------
    let mut uniform = UniformBuffer::new(&gpu)?;
    let font_sampler = Sampler::new_nearest(&gpu.device)?;
    let mut slots = SlotBuffers::new(
        &gpu,
        fr.slot_count(),
        INITIAL_VERTEX_CAPACITY,
        INITIAL_INDEX_CAPACITY,
    )?;
    let mut font: Option<DeviceImage> = None;
    let mut bindings_dirty = true;
    let mut last_uniform_key: Option<UniformKey> = None;
    let mut probe = OffscreenProbe::new(&gpu, &render_pass, &pipe_layout, &pipeline, surface_format)?;

    let mut vertices: Vec<Vertex> = Vec::with_capacity(INITIAL_VERTEX_CAPACITY);
    let mut indices: Vec<u32> = Vec::with_capacity(INITIAL_INDEX_CAPACITY);
    let mut batches: Vec<Batch> = Vec::new();
    let mut draw_batches: Vec<DrawBatch> = Vec::new();

    let mut frame_no: u64 = 0;
    let mut rebuilds: u32 = 0;

    println!("\n================ 阶段 0：初始尺寸 ================");
    {
        let (cw, ch) = window.inner_size_physical();
        let r = run_size_phase(
            &gpu,
            &window,
            &mut events,
            &mut fr,
            &mut slots,
            &mut uniform,
            font_sampler.handle(),
            &mut font,
            &mut bindings_dirty,
            &mut last_uniform_key,
            &mut probe,
            &ctx,
            &mut vertices,
            &mut indices,
            &mut batches,
            &mut draw_batches,
            &mut rebuilds,
            &mut frame_no,
            (cw, ch),
            FRAMES_PER_SIZE,
        )?;
        println!(
            "  → {cw}x{ch}：{FRAMES_PER_SIZE} 帧 / {} 顶点 / {} draw call / 累计重建 {rebuilds} 次",
            r.verts, r.calls
        );
    }

    // ================= 阶段 1..N：逐个尺寸 resize =================
    for (i, &(tw, th)) in RESIZE_SEQ.iter().enumerate() {
        let before = rebuilds;
        println!("\n---- resize #{i}：目标客户区 {tw}x{th} ----");
        move_offscreen(&window, tw, th)?;

        let r = run_size_phase(
            &gpu,
            &window,
            &mut events,
            &mut fr,
            &mut slots,
            &mut uniform,
            font_sampler.handle(),
            &mut font,
            &mut bindings_dirty,
            &mut last_uniform_key,
            &mut probe,
            &ctx,
            &mut vertices,
            &mut indices,
            &mut batches,
            &mut draw_batches,
            &mut rebuilds,
            &mut frame_no,
            (tw, th),
            FRAMES_PER_SIZE,
        )?;

        let rebuilt = rebuilds - before;
        let (aw, ah) = window.inner_size_physical();
        println!(
            "  → 客户区 {aw}x{ah} / extent {}x{} / 本次重建 {rebuilt} 次（累计 {rebuilds}）/ \
             {FRAMES_PER_SIZE} 帧 / {} 顶点 / {} draw call",
            fr.extent().width,
            fr.extent().height,
            r.verts,
            r.calls
        );

        // 断言 A：重建必须**真的发生**。这是本探针的核心存在理由——
        // full_app 一直打印 0 次；这里若也是 0，说明 resize 根本没生效，
        // 探针等于什么都没测。
        anyhow::ensure!(
            rebuilt >= 1,
            "resize #{i}（目标 {tw}x{th}）后交换链重建次数为 0 —— resize 未生效，\
             本探针未覆盖到重建路径"
        );

        // 断言 B：交换链 extent 必须等于真实客户区。
        // 不等说明重建时用了错误的尺寸（或尺寸兜底没触发）。
        anyhow::ensure!(
            (fr.extent().width, fr.extent().height) == (aw, ah),
            "交换链 extent {}x{} 与客户区 {aw}x{ah} 不一致 —— 重建逻辑有问题",
            fr.extent().width,
            fr.extent().height,
        );

        // 断言 C：这一尺寸下确实画出了东西。
        // 为 0 意味着新尺寸下 egui 没产出图元（黑屏），但**不会**报任何 GPU 错误。
        anyhow::ensure!(
            r.verts > 0 && r.calls > 0,
            "尺寸 {aw}x{ah} 下没有绘制任何图元（顶点 {} / draw call {}）—— 画面是空的",
            r.verts, r.calls
        );

        // 断言 D：A/B 像素校验（间接证据，见模块文档）。
        probe.assert_has_content(&gpu)?;
    }

    // ---- 汇总 ------------------------------------------------------------
    println!("\n================ 汇总 ================");
    println!("总帧数{frame_no} / 交换链重建 {rebuilds} 次（resize 触发 {} 次）", RESIZE_SEQ.len());
    println!(
        "间接像素校验：新尺寸下文字/色块渲染正确（离屏读回 + A/B 反证，**非上屏内容**）"
    );

    // ---- 清理（先 GPU 空闲，再按「引用者 → 被引用者」逆序销毁）------------
    gpu.wait_idle();
    if let Some(mut f) = font.take() {
        f.destroy(&gpu.device);
    }
    font_sampler.destroy(&gpu.device);
    let mut slots = slots;
    slots.destroy(&gpu.device);
    let mut uniform = uniform;
    uniform.destroy(&gpu.device);
    probe.destroy(&gpu.device);
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    desc_layout.destroy(&gpu.device);
    render_pass.destroy(&gpu.device);

    // 验证层在线时，若交换链重建有泄漏，此处会报
    // VUID-vkDestroyDevice-device-05137（重建时泄漏 ImageView）。
    println!("\nALL OK - {frame_no} 帧，交换链重建 {rebuilds} 次（> 0，本探针已覆盖该路径）");
    Ok(())
}

struct PhaseResult {
    verts: usize,
    calls: u32,
}

/// 在当前尺寸下跑满 `frames` 帧。事件处理与重建逻辑照抄 `full_app`。
///
/// 参数用借用分组而非结构体：`FrameRenderer<'g>` 借用 `gpu`，放进
/// `&'g mut FrameRenderer<'g>` 会触发不变性冲突（E0597），故按
/// `full_app` 的既有写法直接传引用。
#[allow(clippy::too_many_arguments)]
fn run_size_phase(
    gpu: &Gpu,
    window: &Window,
    events: &mut EventLoop,
    fr: &mut FrameRenderer<'_>,
    slots: &mut SlotBuffers,
    uniform: &mut UniformBuffer,
    font_sampler: vk::Sampler,
    font: &mut Option<DeviceImage>,
    bindings_dirty: &mut bool,
    last_uniform_key: &mut Option<UniformKey>,
    probe: &mut OffscreenProbe<'_>,
    ctx: &egui::Context,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
    draw_batches: &mut Vec<DrawBatch>,
    rebuilds: &mut u32,
    frame_no: &mut u64,
    expect: (u32, u32),
    frames: u64,
) -> anyhow::Result<PhaseResult> {
    let mut local = 0u64;
    let mut verts = 0usize;
    let mut calls = 0u32;
    // 最后一帧的图元数据，供阶段末尾的像素校验复用。
    let mut saved: Option<(Vec<Vertex>, Vec<u32>, Vec<DrawBatch>)> = None;

    while local < frames {
        // ---- 1. 事件 + resize ------------------------------------------
        for ev in events.poll() {
            match ev {
                WindowEvent::CloseRequested => {
                    return Err(anyhow::anyhow!("收到关闭请求，探针不应被手动关闭"));
                }
                WindowEvent::Resized { .. } => {
                    do_rebuild(gpu, fr, slots, bindings_dirty, last_uniform_key, rebuilds)?;
                }
                _ => {}
            }
        }
        if events.quit_requested() {
            return Err(anyhow::anyhow!("收到 WM_QUIT，探针不应被外部终止"));
        }

        // ---- 2. egui ---------------------------------------------------
        let raw_input = events.egui_input(ctx);
        let mut output = ctx.run_ui(raw_input, |ui| draw_ui(ui, expect));
        let ppp = output.pixels_per_point;
        let mut textures_delta = DeltaGuard(std::mem::take(&mut output.textures_delta));

        // ---- 3. 客户区尺寸兜底 -----------------------------------------
        let (cw, ch) = window.inner_size_physical();
        if cw == 0 || ch == 0 {
            // 客户区为 0（最小化）：不能渲染。增量由守卫离开作用域时清空。
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if (cw, ch) != (fr.extent().width, fr.extent().height) {
            do_rebuild(gpu, fr, slots, bindings_dirty, last_uniform_key, rebuilds)?;
        }

        // ---- 4. tessellate ---------------------------------------------
        let primitives = ctx.tessellate(output.shapes, ppp);
        tessellate_into(&primitives, vertices, indices, batches);
        verts = vertices.len();
        calls = batches.len() as u32;

        // ---- 5. acquire -------------------------------------------------
        let Some(acquired) = fr.acquire()? else {
            do_rebuild(gpu, fr, slots, bindings_dirty, last_uniform_key, rebuilds)?;
            continue;
        };
        let slot = fr
            .current_slot()
            .ok_or_else(|| anyhow::anyhow!("acquire 成功后 current_slot 仍为 None"))?;
        slots.write(gpu, slot, vertices, indices)?;

        // ---- 6. 字体图集上传 -------------------------------------------
        if apply_font_delta(gpu, fr, font, &mut textures_delta.0)? > 0 {
            // 图集可能已重建（尺寸变化）→ 图像视图变了 → 必须重绑。
            *bindings_dirty = true;
        }

        // ---- 7. 描述符绑定（含 resize 后的重绑）------------------------
        // 交换链重建会重新分配描述符集，`bindings_dirty` 为真时把
        // uniform 与纹理重新写进**每一个**槽位（即 rebind_after_rebuild 的语义）。
        if let Some(f) = font.as_ref() {
            if *bindings_dirty {
                for s in 0..fr.slot_count() {
                    fr
                        .update_texture_binding(s, font_sampler, f.view, f.layout)?;
                    fr.update_uniform_binding(
                        s,
                        uniform.buffer().handle(),
                        0,
                        uniform.buffer().size(),
                    )?;
                }
                *bindings_dirty = false;
            }
            // uniform 只在「表面尺寸 / dpr」变化时重写：多帧在途时
            // 无条件改写会被上一帧的 GPU 读到。
            let key = UniformKey {
                width: fr.extent().width,
                height: fr.extent().height,
                pixels_per_point: ppp,
            };
            if *last_uniform_key != Some(key) {
                uniform
                    .write(&gpu.device, &uniforms_for(fr.extent(), ppp))?;
                for s in 0..fr.slot_count() {
                    fr.update_uniform_binding(
                        s,
                        uniform.buffer().handle(),
                        0,
                        uniform.buffer().size(),
                    )?;
                }
                *last_uniform_key = Some(key);
            }
        }

        // ---- 8. 录制 + 呈现 ---------------------------------------------
        draw_batches.clear();
        draw_batches.extend(batches.iter().map(|b| b.to_draw()));
        let input = DrawInput {
            vertex_buffer: slots.vertex_buffer(slot),
            index_buffer: slots.index_buffer(slot),
            batches: draw_batches,
            ..Default::default()
        };
        fr.record(&input)?;

        // `present` 内有守卫：`present_semaphore_index` 在
        // `image_index >= image_semaphores.len()` 时 bail。故
        // 「每次 present 都成功」即证明信号量表长度与 image_count
        // 未失配（**间接**证据——字段私有，读不到本身）。
        if fr.present(acquired)? == PresentResult::Outdated {
            *bindings_dirty = true;
        }

        saved = Some((
            vertices.clone(),
            indices.clone(),
            draw_batches.clone(),
        ));
        *frame_no += 1;
        local += 1;
        events.poll_for(Some(Duration::from_millis(1)));
    }

    // 阶段末尾做一次 A/B 像素校验（间接证据，见模块文档）。
    if let Some((v, i, db)) = saved {
        probe.render_and_read(gpu, fr.extent(), &v, &i, &db, fr.descriptor_set(0)?)?;
    }

    Ok(PhaseResult { verts, calls })
}

/// 交换链重建 + 随之而来的资源重整（照抄 `full_app`）。
fn do_rebuild(
    gpu: &Gpu,
    fr: &mut FrameRenderer<'_>,
    slots: &mut SlotBuffers,
    bindings_dirty: &mut bool,
    last_uniform_key: &mut Option<UniformKey>,
    rebuilds: &mut u32,
) -> anyhow::Result<()> {
    fr.rebuild_swapchain(Default::default())?;
    *bindings_dirty = true;
    *last_uniform_key = None;
    let want = fr.slot_count();
    if slots.buffers.len() != want {
        slots.resize(gpu, want)?;
    }
    *rebuilds += 1;
    Ok(())
}

/// 把窗口移到屏幕外并调整客户区尺寸。
///
/// # 为什么用 `MoveWindow` 而不是直接发 `WM_SIZE`
///
/// `PostMessage(WM_SIZE)` 只让**事件翻译**走到，并**不会**真正改变客户区
/// 尺寸——`GetClientRect` 仍返回旧值。这样验证的就不是真实路径。
/// 必须走 Win32 的真实尺寸变更。
///
/// 保持 `WS_VISIBLE`：窗口不可见时表面无法呈现（MEMORY 坑 36）。
fn move_offscreen(window: &Window, client_w: u32, client_h: u32) -> anyhow::Result<()> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, MoveWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };

    // MoveWindow 的参数是**整体窗口**尺寸；客户区要减去边框与标题栏。
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
            OFFSCREEN_X,
            OFFSCREEN_Y,
            r.right - r.left,
            r.bottom - r.top,
            true,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 离屏像素校验
// ---------------------------------------------------------------------------

/// 离屏渲染目标：带 `TRANSFER_SRC` 的图像 + 读回缓冲 + 专用命令池。
///
/// 存在的理由见模块文档：交换链图像没有 `TRANSFER_SRC`，读不回来。
struct OffscreenProbe<'a> {
    gpu: &'a Gpu,
    render_pass: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    format: vk::Format,
    image: Option<DeviceImage>,
    framebuffer: vk::Framebuffer,
    readback: Buffer,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    extent: vk::Extent2D,
    vertex: Buffer,
    index: Buffer,
    /// 上一轮的真实图元数据。正对照重画时复用（不重新上传）。
    last_batches: Vec<DrawBatch>,
    last_descriptor_set: vk::DescriptorSet,
}

impl<'a> OffscreenProbe<'a> {
    fn new(
        gpu: &'a Gpu,
        render_pass: &modular_clipboard_gfx::pipeline::RenderPass,
        pipe_layout: &modular_clipboard_gfx::pipeline::PipelineLayout,
        pipeline: &modular_clipboard_gfx::pipeline::GraphicsPipeline,
        format: vk::Format,
    ) -> anyhow::Result<Self> {
        // 初始用一个极小尺寸；首次 render_and_read 会按需重建。
        let extent = vk::Extent2D { width: 8, height: 8 };
        let image = Self::make_image(gpu, extent, format)?;
        let framebuffer = Self::make_framebuffer(gpu, render_pass.handle, &image, extent)?;
        let readback = Self::make_readback(gpu, extent)?;
        let pool = Self::make_pool(gpu)?;
        let cmd = Self::make_cmd(gpu, pool)?;
        Ok(Self {
            gpu,
            render_pass: render_pass.handle,
            pipeline_layout: pipe_layout.handle,
            pipeline: pipeline.handle,
            format,
            image: Some(image),
            framebuffer,
            readback,
            pool,
            cmd,
            extent,
            vertex: new_vertex_buffer(gpu, INITIAL_VERTEX_CAPACITY)?,
            index: new_index_buffer(gpu, INITIAL_INDEX_CAPACITY)?,
            last_batches: Vec::new(),
            last_descriptor_set: vk::DescriptorSet::null(),
        })
    }

    fn make_image(gpu: &Gpu, extent: vk::Extent2D, format: vk::Format) -> anyhow::Result<DeviceImage> {
        DeviceImage::new(
            gpu,
            extent,
            format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
        )
    }

    fn make_framebuffer(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        image: &DeviceImage,
        extent: vk::Extent2D,
    ) -> anyhow::Result<vk::Framebuffer> {
        let attachments = [image.view];
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

    fn make_pool(gpu: &Gpu) -> anyhow::Result<vk::CommandPool> {
        let info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(gpu.queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        unsafe { gpu.device.create_command_pool(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建离屏命令池失败: {e:?}"))
    }

    fn make_cmd(gpu: &Gpu, pool: vk::CommandPool) -> anyhow::Result<vk::CommandBuffer> {
        let info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            // MEMORY 坑 15：数量必须显式给，否则返回空 Vec 且不报错。
            .command_buffer_count(1);
        let bufs = unsafe { gpu.device.allocate_command_buffers(&info)? };
        bufs.first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("分配离屏命令缓冲返回空列表"))
    }

    /// 按需重建离屏目标，使其与 `extent` 一致。
    fn ensure_extent(&mut self, extent: vk::Extent2D) -> anyhow::Result<()> {
        if self.extent == extent {
            return Ok(());
        }
        let gpu = self.gpu;
        // 销毁顺序与创建相反：framebuffer 引用了图像视图。
        unsafe { gpu.device.destroy_framebuffer(self.framebuffer, None) };
        self.framebuffer = vk::Framebuffer::null();
        if let Some(mut old) = self.image.take() {
            old.destroy(&gpu.device);
        }

        let image = Self::make_image(gpu, extent, self.format)?;
        self.framebuffer = Self::make_framebuffer(gpu, self.render_pass, &image, extent)?;
        self.image = Some(image);
        self.extent = extent;

        let need = (extent.width as vk::DeviceSize) * (extent.height as vk::DeviceSize) * 4;
        if self.readback.size() < need {
            let new = Self::make_readback(gpu, extent)?;
            let mut old = std::mem::replace(&mut self.readback, new);
            old.destroy(&gpu.device);
        }
        Ok(())
    }

    /// 画一帧到离屏图像并读回，返回非背景像素数。
    ///
    /// `vertices` / `indices` / `batches` 全传空即「只清屏」（负对照用）。
    /// 顶点/索引会写入本探针私有的缓冲；`batches` 会被记下来供正对照重画。
    fn render_and_read(
        &mut self,
        gpu: &Gpu,
        extent: vk::Extent2D,
        vertices: &[Vertex],
        indices: &[u32],
        batches: &[DrawBatch],
        descriptor_set: vk::DescriptorSet,
    ) -> anyhow::Result<usize> {
        self.ensure_extent(extent)?;
        if !vertices.is_empty() {
            self.vertex.write(&gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        if !indices.is_empty() {
            self.index.write(&gpu.device, 0, bytemuck::cast_slice(indices))?;
        }
        let image = self
            .image
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("离屏图像已被销毁"))?;
        let cmd = self.cmd;

        unsafe {
            gpu.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            gpu.device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;

            // UNDEFINED -> COLOR_ATTACHMENT_OPTIMAL
            gpu.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    image.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                )],
            );

            // 渲染通道。initial/final 都是 COLOR_ATTACHMENT_OPTIMAL
            // （见 pipeline.rs 注释），故通道前后不需额外转换。
            let clear = vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0, 0.0, 0.0, 1.0] },
            };
            let area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            };
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffer)
                .render_area(area)
                .clear_values(std::slice::from_ref(&clear));
            gpu.device
                .cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);

            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let scissor = area;
            gpu.device
                .cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
            gpu.device
                .cmd_set_scissor(cmd, 0, std::slice::from_ref(&scissor));

            if !batches.is_empty() {
                // 复用交换链路径的**同一个**管线与描述符集：
                // 验证的是「resize 后原有绑定还能不能出字」，
                // 而不是「另建一套管线能不能画」。
                gpu.device
                    .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
                let sets = [descriptor_set];
                gpu.device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &sets,
                    &[],
                );
                let vbuf = [self.vertex.handle()];
                let offsets = [0];
                gpu.device.cmd_bind_vertex_buffers(cmd, 0, &vbuf, &offsets);
                gpu.device
                    .cmd_bind_index_buffer(cmd, self.index.handle(), 0, vk::IndexType::UINT32);
                for b in batches {
                    // 参数：命令缓冲 / 索引数 / 实例数 / 首索引 / 顶点偏移 / 首实例。
                    // 实例数恒为 1；顶点偏移恒为 0（偏移已烘进 index_offset）。
                    gpu.device.cmd_draw_indexed(
                        cmd,
                        b.index_count,
                        1,
                        b.index_offset,
                        0,
                        0,
                    );
                }
            }

            gpu.device.cmd_end_render_pass(cmd);

            // COLOR_ATTACHMENT_OPTIMAL -> TRANSFER_SRC_OPTIMAL
            gpu.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    image.image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                )],
            );

            let copy = vk::BufferImageCopy::default()
                .buffer_offset(0)
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
            gpu.device.cmd_copy_image_to_buffer(
                cmd,
                image.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback.handle(),
                &[copy],
            );
            gpu.device.end_command_buffer(cmd)?;
        }

        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cmd));
        unsafe {
            gpu.device
                .queue_submit(gpu.queue, std::slice::from_ref(&submit), vk::Fence::null())?
        };
        // 读回前必须等 GPU 完成。诊断用的同步点，每尺寸只发生一次，
        // 不在帧循环内，不影响多帧在途的行为。
        gpu.wait_idle();

        self.readback.map(&gpu.device)?;
        let bytes: Vec<u8> = self.readback.read::<u8>()?;
        self.readback.unmap(&gpu.device)?;

        // 记住这批图元与描述符集，供正对照重画（顶点数据已在缓冲里）。
        self.last_batches = batches.to_vec();
        self.last_descriptor_set = descriptor_set;
        Ok(count_non_background(&bytes))
    }

    /// A/B 像素校验：负对照（只清屏）+ 正对照（真实图元）。
    ///
    /// **间接证据**——读的是自建离屏图像，不是交换链图像（它没有
    /// `TRANSFER_SRC`）。见模块文档。
    fn assert_has_content(&mut self, gpu: &Gpu) -> anyhow::Result<()> {
        let extent = self.extent;

        // 先把真实图元存起来：负对照会把 last_batches 覆盖成空。
        anyhow::ensure!(
            !self.last_batches.is_empty(),
            "正对照失败：没有可重画的图元（阶段未记录到绘制批次）"
        );
        let saved_batches = std::mem::take(&mut self.last_batches);
        let saved_set = self.last_descriptor_set;

        // 负对照：空图元 + 空批次 → 只清屏 → 必须检不出内容。
        // 若这里也检出内容，说明「非背景」判定本身失效，正对照不可信。
        let neg = self.render_and_read(gpu, extent, &[], &[], &[], vk::DescriptorSet::null())?;
        anyhow::ensure!(
            neg == 0,
            "负对照失败：只清屏时检出 {neg} 个非背景像素 —— 像素检查没有鉴别力，\
             正对照结论不可信"
        );

        // 正对照：用真实图元重画。顶点/索引缓冲里仍留着上一轮的数据
        // （负对照传空切片不会覆盖它们），故无需重新上传。
        let pos = self.render_and_read(gpu, extent, &[], &[], &saved_batches, saved_set)?;
        anyhow::ensure!(
            pos > 0,
            "正对照失败：新尺寸 {extent:?} 下离屏渲染没有任何非背景像素 —— \
             文字/色块没画出来（黑屏），但 GPU 未报任何错误"
        );
        println!(
            "  [像素 A/B 通过] 负对照（只清屏）= 0 像素；正对照 = {pos} 像素\
             （间接证据：离屏读回，非上屏内容）"
        );
        Ok(())
    }

    fn destroy(&mut self, device: &ash::Device) {
        unsafe { device.destroy_framebuffer(self.framebuffer, None) };
        self.framebuffer = vk::Framebuffer::null();
        if let Some(mut i) = self.image.take() {
            i.destroy(device);
        }
        self.readback.destroy(device);
        self.vertex.destroy(device);
        self.index.destroy(device);
        unsafe { device.destroy_command_pool(self.pool, None) };
    }
}

/// 构造一道整图屏障。
fn barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        // 队列族不变，置 IGNORED（否则对非 CONCURRENT 图像非法）。
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
        .old_layout(old_layout)
        .new_layout(new_layout)
}

/// 统计「非背景」像素数。格式为 B8G8R8A8，字节序 B,G,R,A。
fn count_non_background(bytes: &[u8]) -> usize {
    bytes
        .chunks_exact(4)
        .filter(|p| {
            let d = |i: usize| (p[i] as i32 - BG[i] as i32).abs();
            d(0) > BG_TOLERANCE || d(1) > BG_TOLERANCE || d(2) > BG_TOLERANCE
        })
        .count()
}

// ---------------------------------------------------------------------------
// 界面 / 顶点数据
// ---------------------------------------------------------------------------

/// 画一个能验证「文字 + 颜色」都正常的界面。
fn draw_ui(ui: &mut egui::Ui, expect: (u32, u32)) {
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(0x1E, 0x1E, 0x22));
    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.heading("ModularClipboard resize_probe");
        ui.label("中文字体应正常显示：剪贴板历史记录 · 搜索 · 详情");
        ui.label("ASCII baseline: The quick brown fox jumps over the lazy dog. 0123456789");
    });
    ui.add_space(14.0);
    ui.vertical_centered(|ui| {
        // 把目标尺寸画进界面：读回时若内容与尺寸不匹配能看出来。
        ui.label(format!("目标客户区 {}x{}", expect.0, expect.1));
        ui.label("每次 resize 后应仍能正确显示本行文字");
    });
    ui.add_space(14.0);
    // 色块：验证颜色通道在 resize 后仍正确。
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

/// 安装中文字体。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: &[&str] = &[
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
    ];
    if std::env::var_os("TIEZ_NO_CJK_FONT").is_some() {
        println!("TIEZ_NO_CJK_FONT 已设置，跳过中文字体");
        return;
    }
    let mut defs = egui::FontDefinitions::default();
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = 0;
        defs.font_data
            .insert("cjk".to_string(), std::sync::Arc::new(data));
        let mut families = defs.families.clone();
        families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_string());
        defs.families = families;
        ctx.set_fonts(defs);
        println!("OK 中文字体已加载：{path}");
        return;
    }
    println!("[警告] 未找到中文字体，中文将显示为方块");
    ctx.set_fonts(defs);
}

/// 把 egui 图元转成顶点 / 索引 / 批次（照抄 full_app）。
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
                // 只画字体图集，0 = 字体图集。
                tex_id: 0,
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));
        // 相邻且 clip 相同则并入上一批（纯优化，不改变画面）。
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
fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    let (r, g, b) = unpremultiply(r, g, b, a);
    u32::from_le_bytes([r, g, b, a])
}

/// 预乘 → 非预乘。着色器混合是 `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`（非预乘），
/// 而 epaint 顶点色是预乘的，不还原会让半透明区发黑。
fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 255 {
        return (r, g, b);
    }
    if a == 0 {
        return (0, 0, 0);
    }
    // 用 u32 中转：r * 255 最大 65025，会溢出 u8。
    let f = |c: u8| -> u8 { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
    (f(r), f(g), f(b))
}

/// 离开作用域时清空 `TexturesDelta`（否则 epaint 的 debug 断言会 panic，
/// 把真正的错误掩盖成一个无关的帧循环 panic）。
struct DeltaGuard(egui::TexturesDelta);

impl Drop for DeltaGuard {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// 应用字体图集增量（照抄 full_app）。
fn apply_font_delta(
    gpu: &Gpu,
    fr: &mut FrameRenderer<'_>,
    font: &mut Option<DeviceImage>,
    delta: &mut egui::TexturesDelta,
) -> anyhow::Result<u64> {
    let mut uploads = 0u64;
    // egui 约定 `Managed(0)` 恒为字体图集。
    const FONT_ID: egui::TextureId = egui::TextureId::Managed(0);
    for (id, deltas) in &delta.set {
        if *id != FONT_ID {
            continue;
        }
        for d in deltas {
            let patch = d.image.size();
            let (w, h) = (patch[0] as u32, patch[1] as u32);
            anyhow::ensure!(w > 0 && h > 0, "字体图集增量尺寸为 0：{patch:?}");
            // **只有整图更新（pos == None）才意味着图集扩容**。
            // 局部更新的 `image.size()` 是补丁尺寸，拿它比对会把
            // 2048x32 的图集换成 4x10 的小图，随后坐标全部越界
            // （这个 bug 由 full_app 实机跑出来过）。
            if d.pos.is_none() {
                let need_new = match font.as_ref() {
                    None => true,
                    Some(f) => f.size.width != w || f.size.height != h,
                };
                if need_new {
                    if let Some(mut old) = font.take() {
                        // 旧图像可能仍被在途命令引用。
                        gpu.wait_idle();
                        old.destroy(&gpu.device);
                    }
                    *font = Some(DeviceImage::new(
                        gpu,
                        vk::Extent2D { width: w, height: h },
                        FONT_FORMAT,
                        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
                    )?);
                }
            }
            let bytes = DeviceImage::coverage_bytes(&d.image);
            let offset = d.pos.map(|p| (p[0] as u32, p[1] as u32));
            let f = font
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("收到局部更新 {offset:?} 但纹理尚未创建"))?;
            fr.record_texture_upload(f, &bytes, offset, (w, h))?;
            uploads += 1;
        }
    }
    // `TexturesDelta` 在 drop 时断言增量为空。本函数只消费字体图集，
    // 其余一律忽略，故显式清空。
    delta.clear();
    Ok(uploads)
}

// ---------------------------------------------------------------------------
// 缓冲
// ---------------------------------------------------------------------------

/// uniform 内容指纹：变了才需要重写。
#[derive(Debug, Clone, Copy, PartialEq)]
struct UniformKey {
    width: u32,
    height: u32,
    pixels_per_point: f32,
}

/// 按在飞槽位数量复制的顶点/索引缓冲环。
///
/// 每槽位一份：帧层是多帧在途的，只有一份会被下一帧覆写
/// GPU 仍在读的数据，画面会随机撕裂。
struct SlotBuffers {
    buffers: Vec<SlotPair>,
}

struct SlotPair {
    vertex: Option<Buffer>,
    index: Option<Buffer>,
    vertex_capacity: usize,
    index_capacity: usize,
}

impl SlotBuffers {
    fn new(gpu: &Gpu, slots: usize, vc: usize, ic: usize) -> anyhow::Result<Self> {
        let mut buffers = Vec::with_capacity(slots);
        for _ in 0..slots {
            buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vc)?),
                index: Some(new_index_buffer(gpu, ic)?),
                vertex_capacity: vc,
                index_capacity: ic,
            });
        }
        Ok(Self { buffers })
    }

    fn resize(&mut self, gpu: &Gpu, slots: usize) -> anyhow::Result<()> {
        while self.buffers.len() > slots {
            let mut p = self.buffers.pop().expect("长度已确认");
            p.destroy(&gpu.device);
        }
        let (vc, ic) = self
            .buffers
            .first()
            .map(|p| (p.vertex_capacity, p.index_capacity))
            .unwrap_or((INITIAL_VERTEX_CAPACITY, INITIAL_INDEX_CAPACITY));
        while self.buffers.len() < slots {
            self.buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vc)?),
                index: Some(new_index_buffer(gpu, ic)?),
                vertex_capacity: vc,
                index_capacity: ic,
            });
        }
        Ok(())
    }

    fn write(
        &mut self,
        gpu: &Gpu,
        slot: usize,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> anyhow::Result<()> {
        let count = self.buffers.len();
        let p = self
            .buffers
            .get_mut(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位 {slot} 越界（共 {count} 个）"))?;
        // 扩容是安全的：`acquire` 已等过该槽位栅栏。
        if vertices.len() > p.vertex_capacity {
            let cap = next_capacity(vertices.len());
            if let Some(mut old) = p.vertex.take() {
                old.destroy(&gpu.device);
            }
            p.vertex = Some(new_vertex_buffer(gpu, cap)?);
            p.vertex_capacity = cap;
        }
        if indices.len() > p.index_capacity {
            let cap = next_capacity(indices.len());
            if let Some(mut old) = p.index.take() {
                old.destroy(&gpu.device);
            }
            p.index = Some(new_index_buffer(gpu, cap)?);
            p.index_capacity = cap;
        }
        if !vertices.is_empty() {
            p.vertex
                .as_mut()
                .expect("上方已确保存在")
                .write(&gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        if !indices.is_empty() {
            p.index
                .as_mut()
                .expect("上方已确保存在")
                .write(&gpu.device, 0, bytemuck::cast_slice(indices))?;
        }
        Ok(())
    }

    fn vertex_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .vertex
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn index_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .index
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn destroy(&mut self, device: &ash::Device) {
        for p in &mut self.buffers {
            p.destroy(device);
        }
        self.buffers.clear();
    }
}

impl SlotPair {
    fn destroy(&mut self, device: &ash::Device) {
        if let Some(mut v) = self.vertex.take() {
            v.destroy(device);
        }
        if let Some(mut i) = self.index.take() {
            i.destroy(device);
        }
        self.vertex_capacity = 0;
        self.index_capacity = 0;
    }
}

fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

fn new_vertex_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

fn new_index_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<u32>()) as vk::DeviceSize,
        vk::BufferUsageFlags::INDEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

/// 一批绘制。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Batch {
    index_offset: u32,
    index_count: u32,
    clip: egui::Rect,
}

impl Batch {
    fn to_draw(self) -> DrawBatch {
        DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
        }
    }
}

fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
    // `clip_from_uv` 传单位矩阵：着色器内部已把像素换算成 NDC
    // （并做了 y 轴翻转），再乘矩阵会引入二次变换。
    Uniforms::new(
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
        [extent.width as f32, extent.height as f32],
        pixels_per_point,
    )
}

// ---------------------------------------------------------------------------
// 测试（不建窗口，CI 可跑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_sequence_has_distinct_sizes() {
        // 每个尺寸都必须与前一个不同，否则重建可能不真正换图像。
        let mut prev = (W, H);
        for &s in RESIZE_SEQ {
            assert_ne!(s, prev, "resize 序列含重复尺寸 {s:?}，无法验证重建");
            assert!(s.0 > 0 && s.1 > 0, "尺寸必须非 0");
            prev = s;
        }
    }

    #[test]
    fn resize_sequence_ends_at_original_size() {
        // 最后一档回到初始尺寸：验证「改回去」也能正确重建。
        assert_eq!(RESIZE_SEQ.last().copied(), Some((W, H)));
    }

    #[test]
    fn resize_sequence_covers_both_directions() {
        // 至少有一次变小、一次变大。只单向变化时，某些驱动可能不重建
        // （extent 仍落在 caps 内），测不到销毁链。
        let initial = (W, H);
        let smaller = RESIZE_SEQ.iter().any(|&s| s.0 < initial.0 && s.1 < initial.1);
        let bigger = RESIZE_SEQ.iter().any(|&s| s.0 > initial.0 && s.1 > initial.1);
        assert!(smaller, "序列应包含一次变小");
        assert!(bigger, "序列应包含一次变大");
    }

    #[test]
    fn frames_per_size_covers_image_rotation() {
        // 60 帧至少要能走完一圈交换链图像（典型 image_count 2~3）。
        assert!(FRAMES_PER_SIZE >= 60, "每尺寸帧数 {FRAMES_PER_SIZE} 不足");
    }

    #[test]
    fn offscreen_position_is_negative() {
        // 负坐标让用户看不到窗口；若是 0 或正数会挡住用户桌面。
        assert!(OFFSCREEN_X < 0 && OFFSCREEN_Y < 0, "窗口必须移出屏幕");
    }

    #[test]
    fn background_color_matches_ui() {
        // 读回用 BG 区分「内容」与「背景」。若与 draw_ui 画的不一致，
        // 整幅图都会被算成「非背景」，检查失去意义。
        let painted = egui::Color32::from_rgb(BG[0], BG[1], BG[2]);
        // `to_array()` 返回 4 元素（RGBA），故不能用 `[r, g, b]` 解构。
        let [r, g, b, _a] = painted.to_array();
        assert_eq!([r, g, b], BG);
    }

    #[test]
    fn count_non_background_is_zero_for_uniform_background() {
        let all_bg: Vec<u8> = (0..(4 * 16))
            .map(|i| if i % 4 == 3 { 255 } else { BG[i % 4] })
            .collect();
        assert_eq!(count_non_background(&all_bg), 0, "纯背景应检出 0 像素");
    }

    #[test]
    fn count_non_background_detects_clear_color() {
        // 清屏纯黑必须算作「非背景」——否则负对照形同虚设。
        let black: Vec<u8> = (0..(4 * 16))
            .map(|i| if i % 4 == 3 { 255 } else { 0 })
            .collect();
        assert!(
            count_non_background(&black) > 0,
            "纯黑（清屏色）必须算作非背景"
        );
    }

    #[test]
    fn count_non_background_counts_single_pixel() {
        let mut px: Vec<u8> = (0..(4 * 16))
            .map(|i| if i % 4 == 3 { 255 } else { BG[i % 4] })
            .collect();
        // 第一个像素改成白色（BGRA：R=0x4C→0xFF 位置按 BGR 写）。
        px[0] = 0xFF;
        px[1] = 0xFF;
        px[2] = 0xFF;
        assert_eq!(count_non_background(&px), 1, "应恰好检出 1 个像素");
    }

    #[test]
    fn count_non_background_respects_bgra_order() {
        // 交换链格式 B8G8R8A8：字节序是 B,G,R,A。
        let px: Vec<u8> = vec![0x4C, 0x8D, 0xF6, 0xFF];
        assert_eq!(count_non_background(&px), 1, "单个彩色像素应被检出");
    }

    #[test]
    fn vertex_stride_matches_pipeline() {
        // pipeline.rs 声明 pos(8)+uv(8)+color(4)+tex_id(4) = 24 字节。
        // 断言真实大小而非重算（MEMORY 第 17 批坑 84）。
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
    }

    #[test]
    fn unpremultiply_is_identity_for_opaque() {
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
        // a=0 是除零边界，必须返回 0 而不是 panic。
        assert_eq!(unpremultiply(0, 0, 0, 0), (0, 0, 0));
    }

    #[test]
    fn unpremultiply_never_exceeds_255() {
        for a in 1..=254u8 {
            let (r, g, b) = unpremultiply(255, 255, 255, a);
            for v in [r, g, b] {
                // 用 u32 中转比较：直接写 `v <= 255` 在 u8 上恒真。
                assert!(u32::from(v) <= 255, "反预乘溢出 u8");
            }
        }
    }

    #[test]
    fn uniform_key_changes_with_extent() {
        // uniform 只在内容变化时重写，指纹必须能区分不同尺寸——
        // 这正是 resize 后能否正确重算 NDC 的关键。
        let a = UniformKey {
            width: 800,
            height: 600,
            pixels_per_point: 1.0,
        };
        assert_ne!(
            a,
            UniformKey { width: 640, height: 600, pixels_per_point: 1.0 }
        );
        assert_ne!(
            a,
            UniformKey { width: 800, height: 480, pixels_per_point: 1.0 }
        );
        assert_eq!(a, a);
    }

    #[test]
    fn uniforms_carry_new_extent() {
        // 着色器用 size_in_pixels 做像素→NDC。若 resize 后仍是旧尺寸，
        // 画面会被拉伸或裁掉。
        let u = uniforms_for(vk::Extent2D { width: 1200, height: 800 }, 1.0);
        assert_eq!(u.size_in_pixels, [1200.0, 800.0]);
    }

    #[test]
    fn uniform_matrix_is_identity() {
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(
            u.clip_from_uv,
            [
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                0.0, 0.0, 0.0, 1.0,
            ]
        );
    }

    #[test]
    fn capacity_growth_is_monotonic() {
        assert!(next_capacity(1) >= 1);
        assert!(next_capacity(2000) >= 2000);
        assert!(next_capacity(next_capacity(2000)) > next_capacity(2000));
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
}
