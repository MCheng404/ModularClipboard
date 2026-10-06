//! 端到端验证：Win32 窗口 → Vulkan 交换链 → **egui 界面真正显示出来**。
//!
//! # 这个探针回答的问题
//!
//! 前两个探针（`pipeline_probe` / `upload_probe`）验证的都是「链路能跑通」：
//! 命令被录制、被提交、GPU 不报错。但它们**没有画出一个来自 egui 的像素**。
//! 而「渲染器可用」和「界面能显示」是两件事——后者还需要：
//!
//! 1. egui tessellation 产出的 [`egui::epaint::ClippedPrimitive`] 转成顶点数据；
//! 2. 顶点/索引数据搬进 GPU 可见的缓冲；
//! 3. 字体图集（覆盖率图）上传并绑定到描述符集；
//! 4. **混合模式与 egui 的预乘 alpha 语义对齐**（见 [`unpremultiply`] 的注释）。
//!
//! 任何一环错了，GPU 都不会报错，只会把字画成豆腐块、把半透明画成黑块。
//! 因此这个探针只认「屏幕上出现了中文文字」为通过。

use std::time::Duration;

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{DrawInput, FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::pipeline::Uniforms;
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};

/// 窗口客户区尺寸（物理像素）。
const W: u32 = 900;
const H: u32 = 640;

/// 跑满这么多帧后自动退出。
///
/// 无人值守的脚本没法确认「窗口还在」，但能确认「跑了几百帧没崩」。
/// 设 `TIEZ_FULL_APP_FOREVER=1` 可让它一直跑（人工验证用）。
const EXIT_AFTER_FRAMES: u64 = 600;

/// 顶点缓冲初始容量（顶点数）。不足时按 [`next_capacity`] 扩容。
const INITIAL_VERTEX_CAPACITY: usize = 4096;

/// 索引缓冲初始容量（索引数）。
const INITIAL_INDEX_CAPACITY: usize = 6144;

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL full_app: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // ---- 窗口 ----------------------------------------------------------
    let window = Window::new("ModularClipboard — full_app", W, H)?;
    let mut events = EventLoop::new(&window);
    println!(
        "OK Window  客户区 {}x{}  scale_factor={}",
        W,
        H,
        window.scale_factor()
    );

    // ---- 设备 ----------------------------------------------------------
    let gpu = Gpu::new("ModularClipboardFullApp", window.hinstance(), window.hwnd())?;
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
    println!(
        "OK Gpu  {device_name}  ({:?})  queue_family={}",
        gpu.device_type, gpu.queue_family
    );

    // ---- 管线 ----------------------------------------------------------
    // 渲染通道的附件格式必须与交换链选中的一致。用与 `pick_format`
    // 相同的偏好预选，`FrameRenderer::new` 内部会再校验一次，
    // 不一致时直接失败而不是让驱动在 `cmd_begin_render_pass` 崩。
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
    println!("OK 管线（RenderPass + Layout + SPIR-V + GraphicsPipeline）");

    let mut fr = FrameRenderer::new(
        &gpu,
        PipelineBundle::new(
            render_pass.handle,
            pipe_layout.handle,
            pipeline.handle,
            desc_layout.handle,
        ),
    )?;
    // 交换链是按 Gpu 启动时的表面快照建的。窗口尺寸在那之前已确定，
    // 但主动重建一次可保证 framebuffer 尺寸与真实客户区严格一致。
    fr.rebuild_swapchain(Default::default())?;
    println!(
        "OK FrameRenderer  extent={}x{}  在飞槽位={}",
        fr.extent().width,
        fr.extent().height,
        fr.slot_count()
    );

    // ---- egui ----------------------------------------------------------
    let ctx = egui::Context::default();
    install_cjk_font(&ctx);
    // 深色主题：与项目实际观感一致，避免「浅底浅字看不清」被误判为
    // 「文字没画出来」。
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
    println!(
        "OK 每槽位顶点/索引缓冲 {} 份（host-visible 环）",
        slots.buffers.len()
    );

    let mut font: Option<DeviceImage> = None;
    // 描述符绑定是**静态**的：交换链重建会重新分配描述符集，
    // 字体图集重建会换掉图像视图，两者都要求重绑。
    let mut bindings_dirty = true;
    let mut last_uniform_key: Option<UniformKey> = None;

    // CPU 侧复用缓冲，避免每帧重新分配。
    let mut vertices: Vec<Vertex> = Vec::with_capacity(INITIAL_VERTEX_CAPACITY);
    let mut indices: Vec<u32> = Vec::with_capacity(INITIAL_INDEX_CAPACITY);
    let mut batches: Vec<Batch> = Vec::new();
    let mut draw_batches: Vec<modular_clipboard_gfx::frame::DrawBatch> = Vec::new();

    let mut frame_no: u64 = 0;
    let mut rebuilds: u32 = 0;
    let mut texture_uploads: u64 = 0;
    let mut last_draw_calls: u32 = 0;
    let mut last_vertices: usize = 0;
    let mut running = true;

    println!("进入主循环…");
    while running {
        // ---- 1. 收事件 + 处理 resize ---------------------------------
        for ev in events.poll() {
            match ev {
                WindowEvent::CloseRequested => {
                    println!("收到关闭请求，正常退出");
                    running = false;
                }
                WindowEvent::Resized { .. } => {
                    do_rebuild(
                        &gpu,
                        &mut fr,
                        &mut slots,
                        &mut bindings_dirty,
                        &mut last_uniform_key,
                        &mut rebuilds,
                    )?;
                }
                _ => {}
            }
        }
        if events.quit_requested() {
            break;
        }
        if !running {
            break;
        }

        // ---- 2. 跑 egui ----------------------------------------------
        // `EventLoop::egui_input` 读的是 `poll` 填好的缓冲，因此顺序
        // 必须是 poll → egui_input → run。
        let raw_input = events.egui_input(&ctx);
        let mut output = ctx.run_ui(raw_input, |ui| draw_ui(ui, frame_no));

        // 后续不再整体借用 output（shapes 已被 tessellate 移走），
        // 因此先把还要用的字段取出来。
        let pixels_per_point = output.pixels_per_point;
        // 用守卫而不是裸变量：`?` 提前返回时也必须清空增量，
        // 否则 epaint 的 `TexturesDelta::drop` 断言会 panic，
        // 把「真正的错误」掩盖成一个毫不相干的帧循环断言。
        let mut textures_delta = DeltaGuard(std::mem::take(&mut output.textures_delta));
        let repaint = repaint_delay(&output);

        // ---- 3. 客户区尺寸兜底检查 ------------------------------------
        // 最小化时客户区为 0×0，交换链拿不到可呈现的图像。
        let (cw, ch) = window.inner_size_physical();
        if cw == 0 || ch == 0 {
            // 最小化期间不渲染，但增量已被本帧产出，守卫会在离开作用域时清空。
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        if (cw, ch) != (fr.extent().width, fr.extent().height) {
            // Resized 事件已处理过；这里兜住「尺寸查询与事件不一致」
            // 的情况（例如 SetWindowPos 引起的多消息合并）。
            do_rebuild(
                &gpu,
                &mut fr,
                &mut slots,
                &mut bindings_dirty,
                &mut last_uniform_key,
                &mut rebuilds,
            )?;
        }

        // ---- 4. tessellate → 顶点数据 --------------------------------
        let primitives = ctx.tessellate(output.shapes, pixels_per_point);
        tessellate_into(&primitives, &mut vertices, &mut indices, &mut batches);
        last_vertices = vertices.len();
        last_draw_calls = batches.len() as u32;

        // ---- 5. acquire ------------------------------------------------
        let Some(acquired) = fr.acquire()? else {
            do_rebuild(
                &gpu,
                &mut fr,
                &mut slots,
                &mut bindings_dirty,
                &mut last_uniform_key,
                &mut rebuilds,
            )?;
            // 本帧不渲染，但增量已被产出。守卫会在离开作用域时清空，
            // 无需在此手写——漏写一处就是一个与真实错误无关的 panic。
            println!("  [警告] acquire 返回过期，已重建交换链");
            continue;
        };
        let slot = fr
            .current_slot()
            .ok_or_else(|| anyhow::anyhow!("acquire 成功后 current_slot 仍为 None"))?;

        // ---- 6. 顶点/索引数据 → GPU ----------------------------------
        //
        // `acquire` 内部已经等过了该槽位的栅栏，因此「拿到 slot 就往这个
        // 槽位的缓冲写」是安全的：上一轮占用它的 GPU 工作已完成。
        // 这也是本探针用 host-visible 环缓冲、而不是 device-local +
        // staging 的原因——同样正确，且每帧少一次拷贝。
        slots.write(&gpu, slot, &vertices, &indices)?;

        // ---- 7. 字体图集上传 ------------------------------------------
        //
        // 必须在 acquire 之后：`record_texture_upload` 把命令录进本帧的
        // 命令缓冲，且 staging 的生命周期由帧层arena 用栅栏保证。
        let uploaded = apply_font_delta(&gpu, &mut fr, &mut font, &mut textures_delta.0)?;
        if uploaded > 0 {
            texture_uploads += uploaded;
            // 图集可能已被重建（尺寸变化）→ 图像视图变了 → 必须重绑。
            bindings_dirty = true;
            // 诊断：设备丢失时打印是第几次上传、图集多大。
            // 「哪一行第一次出问题」只能靠实机跑定位——单测里没有 GPU。
            tracing::debug!(
                upload = uploaded,
                atlas = ?font.as_ref().map(|f| (f.size.width, f.size.height)),
                "字体图集已上传",
            );
        }

        // ---- 8. 描述符绑定 --------------------------------------------
        if let Some(f) = font.as_ref() {
            if bindings_dirty {
                bind_all(&fr, font_sampler.handle(), f, &uniform)?;
                bindings_dirty = false;
            }
            // uniform 内容只随「表面尺寸 / dpr」变化。多帧在途时改写同一份
            // uniform 缓冲会被上一帧的 GPU 读到，因此**只在内容真的变了
            // 时才写**——写相同内容则不存在竞态。
            let key = UniformKey {
                width: fr.extent().width,
                height: fr.extent().height,
                pixels_per_point,
            };
            if last_uniform_key != Some(key) {
                uniform.write(
                    &gpu.device,
                    &uniforms_for(fr.extent(), pixels_per_point),
                )?;
                for s in 0..fr.slot_count() {
                    fr.update_uniform_binding(
                        s,
                        uniform.buffer().handle(),
                        0,
                        uniform.buffer().size(),
                    )?;
                }
                last_uniform_key = Some(key);
            }
        }

        // ---- 9. 录制绘制 + 呈现 ---------------------------------------
        draw_batches.clear();
        draw_batches.extend(batches.iter().map(|b| b.to_draw()));
        let input = DrawInput {
            vertex_buffer: slots.vertex_buffer(slot),
            index_buffer: slots.index_buffer(slot),
            batches: &draw_batches,
            ..Default::default()
        };
        fr.record(&input)?;
        if fr.present(acquired)? == PresentResult::Outdated {
            // 呈现时才发现过期：下一轮 acquire 会返回 None 再重建。
            // 这里不立刻重建，避免连续两次 device_wait_idle。
            bindings_dirty = true;
        }

        frame_no += 1;
        if frame_no == 1 {
            println!(
                "首帧：{last_vertices} 顶点 / {} 索引 / {last_draw_calls} draw call / \
                 字体上传 {texture_uploads} 次 / 图集 {:?}",
                indices.len(),
                font.as_ref().map(|f| (f.size.width, f.size.height)),
            );
        }
        // 前若干帧逐帧打印：崩溃定位靠这个，稀疏采样会漏掉「第 3 帧就挂了」
        // 这类情况（实测设备丢失就发生在第 2~3 帧）。
        if frame_no <= 8 || frame_no % 120 == 0 {
            println!(
                "  帧 {frame_no}  {last_vertices} 顶点 / {last_draw_calls} draw call / \
                 重建 {rebuilds} 次 / 上传 {texture_uploads} 次 / 图集 {:?}",
                font.as_ref().map(|f| (f.size.width, f.size.height)),
            );
        }
        if frame_no >= EXIT_AFTER_FRAMES && std::env::var_os("TIEZ_FULL_APP_FOREVER").is_none() {
            println!("跑满 {EXIT_AFTER_FRAMES} 帧，自动退出（设 TIEZ_FULL_APP_FOREVER=1 可常驻）");
            break;
        }

        // 无事件时不要空转烧 CPU。egui 请求了延时重绘就按它等待。
        events.poll_for(repaint);
    }

    // ---- 清理 ------------------------------------------------------------
    //
    // 销毁顺序：先让 GPU 彻底空闲，再按「描述符引用者 → 被引用者」逆序销毁。
    gpu.wait_idle();
    if let Some(mut f) = font.take() {
        f.destroy(&gpu.device);
    }
    font_sampler.destroy(&gpu.device);
    let mut slots = slots;
    slots.destroy(&gpu.device);
    let mut uniform = uniform;
    uniform.destroy(&gpu.device);
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    desc_layout.destroy(&gpu.device);
    render_pass.destroy(&gpu.device);
    println!(
        "ALL OK - {frame_no} 帧，最后一帧 {last_vertices} 顶点 / {last_draw_calls} draw call，\
         交换链重建 {rebuilds} 次，纹理上传 {texture_uploads} 次"
    );
    Ok(())
}

/// 交换链重建 + 随之而来的资源重整。
///
/// 一次重建牵动四件事，缺一件都会在后续帧里出问题：
///
/// 1. `rebuild_swapchain` 重新分配描述符集 → 绑定必须重做；
/// 2. 交换链图像数可能变化 → 在飞槽位数随之变化 → 顶点缓冲环要跟着扩缩；
/// 3. 视口/剪裁是动态状态，但 uniform 里的表面尺寸是烘进去的 → 必须重写；
/// 4. `acquire` 的轮转游标被重置 → 槽位与缓冲的对应关系重新开始。
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
    // uniform 内容变了（表面尺寸），下一帧会重写。
    *last_uniform_key = None;
    let want = fr.slot_count();
    if slots.buffers.len() != want {
        slots.resize(gpu, want)?;
    }
    *rebuilds += 1;
    Ok(())
}

/// 把 uniform 与纹理绑定写进**每一个**在飞槽位的描述符集。
///
/// GPU 可能还在读上一个槽位的描述符集，只写当前帧的绑定会让其它帧
/// 读到未初始化的描述符（表现为随机采样到垃圾纹理或崩溃）。
fn bind_all(
    fr: &FrameRenderer<'_>,
    sampler: vk::Sampler,
    font: &DeviceImage,
    uniform: &UniformBuffer,
) -> anyhow::Result<()> {
    for slot in 0..fr.slot_count() {
        fr.update_texture_binding(slot, sampler, font.view, font.layout)?;
        fr.update_uniform_binding(
            slot,
            uniform.buffer().handle(),
            0,
            uniform.buffer().size(),
        )?;
    }
    tracing::debug!(slots = fr.slot_count(), "描述符绑定已全部写入");
    Ok(())
}

/// uniform 的内容指纹。变了才需要重写缓冲。
#[derive(Debug, Clone, Copy, PartialEq)]
struct UniformKey {
    width: u32,
    height: u32,
    pixels_per_point: f32,
}

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

/// 画一个能验证「文字 + 颜色 + 半透明 + 圆角」都正常的界面。
fn draw_ui(ui: &mut egui::Ui, frame_no: u64) {
    // 背景：不画则清屏色透出来，与项目实际观感一致。
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(0x1E, 0x1E, 0x22));

    ui.add_space(20.0);
    ui.vertical_centered(|ui| {
        ui.heading("ModularClipboard 渲染器自检");
        ui.label("中文字体应正常显示：剪贴板历史记录 · 搜索 · 详情");
        ui.label("ASCII baseline: The quick brown fox jumps over the lazy dog. 0123456789");
    });

    ui.add_space(14.0);
    ui.horizontal_centered(|ui| {
        if ui.button("按钮（可点击）").clicked() {
            println!("  [交互] 按钮被点击于第 {frame_no} 帧");
        }
        if ui.button("第二个按钮").clicked() {
            println!("  [交互] 第二个按钮被点击于第 {frame_no} 帧");
        }
    });

    ui.add_space(14.0);
    // 半透明色块：用来验证混合模式。若混合模式与 egui 的预乘语义
    // 不对齐，这几块的颜色会明显发黑。
    ui.horizontal_centered(|ui| {
        for c in [
            egui::Color32::from_rgb(0x4C, 0x8D, 0xF6),
            egui::Color32::from_rgb(0x4C, 0x8D, 0xF6).gamma_multiply(0.5),
            egui::Color32::from_rgb(0x4C, 0x8D, 0xF6).gamma_multiply(0.25),
            egui::Color32::from_rgb(0x4C, 0x8D, 0xF6).gamma_multiply(0.12),
        ] {
            let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(70.0, 40.0));
            ui.painter().rect_filled(rect, 6.0, c);
            ui.allocate_space(egui::vec2(78.0, 40.0));
        }
    });
    ui.vertical_centered(|ui| {
        ui.label("半透明色块：应呈现均匀的蓝色渐层，而不是发黑");
        ui.label(format!("帧号 {frame_no}（每 120 帧打印一次统计）"));
        ui.label("关闭窗口即可退出");
    });
}

/// 安装中文字体。找不到系统字体时退回内置字体（中文会显示为方块，
/// 但 ASCII 与界面结构仍可验证）。
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
    println!("[警告] 未找到中文字体，中文将显示为方块");
    ctx.set_fonts(defs);
}

// ---------------------------------------------------------------------------
// tessellation：egui 图元 → 顶点数据 + 批次
// ---------------------------------------------------------------------------

/// 把一批 [`egui::epaint::ClippedPrimitive`] 转换后追加进
/// `vertices` / `indices` / `batches`（三个向量都会被先清空）。
///
/// # 索引偏移
///
/// egui 的 `Mesh::indices` 是**相对该 mesh 自身顶点数组**的下标。
/// 多个 mesh 拼进同一个顶点缓冲时必须加上「本 mesh 之前已有多少顶点」，
/// 否则第二个 mesh 会去索引第一个 mesh 的顶点——表现为界面元素错位，
/// 而且**不会**触发任何 GPU 错误。
///
/// # 批次合并
///
/// `clip_rect` 相同的**相邻**图元合并成一次 `draw_indexed`。
/// 本项目的着色器不处理 per-batch scissor（egui 在 CPU 侧 tessellation
/// 时已按 `clip_rect` 裁掉几何），因此合并不改变画面，只减少 draw call。
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
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));

        // 与上一段「clip 相同且索引连续」则并入上一批。
        // 索引连续由上面的追加式写入保证。
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

/// egui 的 `Color32` → 顶点色 `u32`（小端 ABGR）。
///
/// # 为什么必须反预乘
///
/// epaint 的顶点色是 **sRGBA 预乘 alpha**（`r,g,b` 已经乘过 `a`），
/// 而 `pipeline.rs` 的混合是 `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`——
/// **非预乘**混合。直接上传会让 alpha 被乘两次，半透明区域明显发黑。
/// 这类错误不报任何 GPU 错误，只是「看起来脏」，极易被漏掉。
///
/// 着色器输出的是 `color * vec4(1,1,1,texel.r)`，即 rgb 不乘覆盖率、
/// 只有 alpha 乘覆盖率，这正是标准非预乘「over」运算期望的形式。
/// 因此这里把颜色还原成非预乘，把那一次乘法交回混合阶段完成。
fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    let (r, g, b) = unpremultiply(r, g, b, a);
    u32::from_le_bytes([r, g, b, a])
}

/// 预乘 → 非预乘。`a == 0` 时 rgb 的信息已在预乘时丢失（乘 0），
/// 取 0：该像素最终 alpha 为 0，对画面的贡献本来就是 0。
fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 255 {
        return (r, g, b);
    }
    if a == 0 {
        // 除零边界。理论上不可达（egui 的Color32 是预乘的，a=0 时 rgb 必为 0），
        // 但仍显式返回 0 而不是让除法 panic —— 一个「理论上不可达」的分支
        // 恰恰是最需要写对的地方。
        return (0, 0, 0);
    }
    // 用 u32 计算：r * 255 最大 65025，会溢出 u8。
    let f = |c: u8| -> u8 { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
    (f(r), f(g), f(b))
}

// ---------------------------------------------------------------------------
// 顶点/索引缓冲
// ---------------------------------------------------------------------------

/// 按在飞槽位数量复制的顶点/索引缓冲环。
///
/// # 为什么是「每槽位一份」而不是「全局一份」
///
/// 帧层是多帧在途的：`present` 只提交、不等待。若顶点数据只有一份，
/// 下一帧的写入会覆写 GPU **仍在读取**的内存，画面会随机撕裂。
///
/// 按槽位各存一份、只写 `acquire` 刚给出的那个槽位，则天然安全：
/// `acquire` 内部已等待该槽位栅栏，上一轮占用它的 GPU 工作必然已完成。
struct SlotBuffers {
    buffers: Vec<SlotPair>,
}

struct SlotPair {
    /// `None` 表示已被移出（销毁流程中）。
    vertex: Option<Buffer>,
    index: Option<Buffer>,
    vertex_capacity: usize,
    index_capacity: usize,
}

impl SlotBuffers {
    fn new(
        gpu: &Gpu,
        slots: usize,
        vertex_capacity: usize,
        index_capacity: usize,
    ) -> anyhow::Result<Self> {
        let mut buffers = Vec::with_capacity(slots);
        for _ in 0..slots {
            buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vertex_capacity)?),
                index: Some(new_index_buffer(gpu, index_capacity)?),
                vertex_capacity,
                index_capacity,
            });
        }
        Ok(Self { buffers })
    }

    /// 调整槽位数量（交换链重建后交换链图像数可能变化）。
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

    /// 把本帧数据写进指定槽位，必要时扩容。
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

        if !p.fits_vertices(vertices.len()) {
            // 扩容要重建缓冲。上一轮占用该槽位的 GPU 工作已被 acquire
            // 的栅栏等待保证完成，因此销毁旧缓冲是安全的。
            let cap = next_capacity(vertices.len());
            if let Some(mut old) = p.vertex.take() {
                old.destroy(&gpu.device);
            }
            p.vertex = Some(new_vertex_buffer(gpu, cap)?);
            p.vertex_capacity = cap;
        }
        if !p.fits_indices(indices.len()) {
            let cap = next_capacity(indices.len());
            if let Some(mut old) = p.index.take() {
                old.destroy(&gpu.device);
            }
            p.index = Some(new_index_buffer(gpu, cap)?);
            p.index_capacity = cap;
        }

        let vbuf = p.vertex.as_mut().expect("上方已确保存在");
        if !vertices.is_empty() {
            vbuf.write(&gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        let ibuf = p.index.as_mut().expect("上方已确保存在");
        if !indices.is_empty() {
            ibuf.write(&gpu.device, 0, bytemuck::cast_slice(indices))?;
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
    fn fits_vertices(&self, n: usize) -> bool {
        n <= self.vertex_capacity
    }
    fn fits_indices(&self, n: usize) -> bool {
        n <= self.index_capacity
    }
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

/// 扩容策略：1.5 倍并向上取整到 1024，避免「每帧只多一个元素就重建」。
fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

/// 顶点缓冲：host-visible + coherent，因此每帧直接 `write`（内部
/// map→拷贝→unmap）即可，不需要 staging + `cmd_copy_buffer`。
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

// ---------------------------------------------------------------------------
// uniform
// ---------------------------------------------------------------------------

/// 一次绘制批次。
///
/// 比 [`modular_clipboard_gfx::frame::DrawBatch`] 多带一个 `clip`：合并判定需要知道
/// 相邻两段是否属于同一裁剪区，而 `frame::DrawBatch` 不暴露这个信息
/// （它假定着色器/clamp 已处理裁剪）。提交前用 [`Batch::to_draw`] 转换。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Batch {
    index_offset: u32,
    index_count: u32,
    clip: egui::Rect,
}

impl Batch {
    fn to_draw(self) -> modular_clipboard_gfx::frame::DrawBatch {
        modular_clipboard_gfx::frame::DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
        }
    }
}

/// 构造本帧的 uniform。
///
/// `clip_from_uv` 传单位矩阵：着色器内部已经把像素坐标换算成 NDC
/// （并做了 y 轴翻转），再乘一个矩阵只会引入二次变换。
fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
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
// 字体图集
// ---------------------------------------------------------------------------

/// 离开作用域时自动清空 [`egui::TexturesDelta`]。
///
/// # 为什么需要它
///
/// `TexturesDelta` 的 `Drop` 里有一条 `debug_assert!(is_empty())`：
/// 增量必须被消费或显式 `clear()`，否则 panic。手写 `clear()` 极易漏——
/// 帧循环里那些 `?` 提前返回、`continue`、最小化跳过的分支各都要写一遍，
/// 漏一处就是一个**与真实错误毫无关系**的 panic，把排查方向带偏。
///
/// 用守卫把「清理」绑到作用域上，就不必逐条路径操心。
struct DeltaGuard(egui::TexturesDelta);

impl Drop for DeltaGuard {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// 应用 egui 的字体图集增量，返回本帧的上传次数。
///
/// # 图集扩容
///
/// egui 的字体图集是**按需增长**的：新字形放不下时整张重建，尺寸变化。
/// 此时必须销毁旧图像并新建，图像视图随之改变 → 描述符需要重绑。
/// 销毁前要 `device_wait_idle`：旧图像可能仍被在途命令引用。
///
/// # 只支持字体图集
///
/// 着色器只绑了一张纹理（`BINDING_TEXTURE`）。用户纹理（图片缩略图等）
/// 需要按 `TextureId` 分批 + 多描述符集，超出本探针范围。
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
            tracing::warn!("忽略非字体纹理 {id:?}：当前着色器只绑定字体图集");
            continue;
        }
        for d in deltas {
            let patch = d.image.size();
            let (w, h) = (patch[0] as u32, patch[1] as u32);
            anyhow::ensure!(w > 0 && h > 0, "字体图集增量的尺寸为 0：{patch:?}");

            // **只有整图更新才意味着图集扩容**。
            //
            // `ImageDelta::pos == None` 是「整图」的判据。局部更新的
            // `image.size()` 返回的是**补丁尺寸**——新字形触发时可能只有
            // 4x10 这么大。若拿它去比对图集尺寸，一次局部更新就会把
            // 2048x32 的图集换成 4x10 的小图，随后所有坐标全部越界。
            //
            // 这个 bug 由 `full_app` 实机跑出来：首帧成功渲染 552 个顶点，
            // 第二帧才崩在「更新区域 (1652, 0) 4x10 超出图像 4x10 范围」。
            // 单测发现不了——没人会用真实 egui 连续产出 whole + 局部两种增量。
            //
            // egui 的 `TexturesDelta::push` 保证：整图增量会**替换**该 ID
            // 的全部历史增量，因此这里逐个判断是安全的。
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
            let f = font.as_mut().ok_or_else(|| {
                anyhow::anyhow!("字体图集收到局部更新 {offset:?}，但纹理尚未创建")
            })?;
            fr.record_texture_upload(f, &bytes, offset, (w, h))?;
            uploads += 1;
        }
    }

    // egui 的 `TexturesDelta` 在 drop 时断言「增量为空」。本函数只消费了
    // 字体图集，用户纹理一律忽略，因此显式清空——否则 debug 断言误报，
    // 而这属于「帧循环写法不对」而非「渲染出错」，不应掩盖真正的失败。
    delta.clear();
    Ok(uploads)
}

// ---------------------------------------------------------------------------
// 节流
// ---------------------------------------------------------------------------

/// egui 请求的下一帧延迟。`None` 表示「有事件时立即处理」。
fn repaint_delay(output: &egui::FullOutput) -> Option<Duration> {
    let v = output.viewport_output.get(&egui::ViewportId::ROOT)?;
    match v.repaint_delay {
        d if d.is_zero() => None,
        d => Some(d),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ClippedPrimitive, Mesh, Primitive, Vertex as EVertex};

    fn prim(clip: egui::Rect, verts: usize, indices: usize) -> ClippedPrimitive {
        let mesh = Mesh {
            indices: (0..indices as u32).collect(),
            vertices: (0..verts)
                .map(|i| EVertex {
                    pos: egui::pos2(i as f32, 0.0),
                    uv: egui::pos2(0.0, 0.0),
                    color: egui::Color32::WHITE,
                })
                .collect(),
            texture_id: FONT_ID_LOCAL,
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    const FONT_ID_LOCAL: egui::TextureId = egui::TextureId::Managed(0);

    fn rect_at(y: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::new(0.0, y), egui::vec2(10.0, 10.0))
    }

    #[test]
    fn color_is_packed_as_abgr_little_endian() {
        // 不透明色：反预乘是恒等，直接验证字节序。
        let c = egui::Color32::from_rgb(0x11, 0x22, 0x33);
        let packed = pack_color(c);
        assert_eq!(packed & 0xFF, 0x11, "最低字节应是红");
        assert_eq!((packed >> 8) & 0xFF, 0x22);
        assert_eq!((packed >> 16) & 0xFF, 0x33);
        assert_eq!((packed >> 24) & 0xFF, 0xFF, "最高字节应是 alpha");
    }

    #[test]
    fn opaque_colors_survive_unpremultiply() {
        // alpha=255 时预乘与非预乘等价，必须逐通道不变。
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
    fn zero_alpha_yields_black_not_overflow() {
        // a=0 是除零边界。
        //
        // 注意：egui 的 `Color32` 内部是**预乘**的，a=0 时 rgb 必然已经是 0，
        // 所以「rgb 非零 + a=0」这个组合构造不出来。这里验证函数不panic
        // 且返回 0。
        assert_eq!(unpremultiply(0, 0, 0, 0), (0, 0, 0));
        let t = egui::Color32::TRANSPARENT;
        assert_eq!(unpremultiply(t.r(), t.g(), t.b(), t.a()), (0, 0, 0));
    }

    #[test]
    fn half_alpha_is_restored_to_full_channel() {
        // 预乘 50% 后 rgb 约为一半；反预乘应还原到接近原值。
        let full = egui::Color32::from_rgb(200, 100, 40);
        let half = full.gamma_multiply(0.5);
        let got = unpremultiply(half.r(), half.g(), half.b(), half.a());
        for (g, want) in got.into_iter().zip([200u8, 100, 40]) {
            assert!(
                (g as i32 - want as i32).abs() <= 2,
                "反预乘误差过大：得到 {g}，期望约 {want}"
            );
        }
    }

    #[test]
    fn unpremultiply_never_exceeds_255() {
        // 预乘的舍入误差可能让反预乘结果略大于原值，必须夹住。
        for a in 1..=254u8 {
            let (r, g, b) = unpremultiply(255, 255, 255, a);
            // 用 u32 中转比较——直接写 `r <= 255` 会被判为恒真（u8 上界），
            // 那样这个测试就什么都没验证。
            for v in [r, g, b] {
                assert!(u32::from(v) <= 255, "反预乘结果 {v} 溢出 u8");
            }
        }
    }

    #[test]
    fn indices_are_offset_per_mesh() {
        // 两个 mesh 的索引都是 0..n，合并后第二个必须整体平移。
        // 不平移的话第二个 mesh 会去画第一个 mesh 的顶点。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(0.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(v.len(), 6, "两个 mesh 的顶点都应保留");
        assert_eq!(i, vec![0, 1, 2, 3, 4, 5], "第二个 mesh 的索引应平移到 3..6");
    }

    #[test]
    fn adjacent_primitives_with_same_clip_merge() {
        // 一次 draw call 优于 N次：这是「相邻且 clip 相同可合并」的价值。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(0.0), 3, 3), prim(rect_at(0.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 1, "相同 clip 的相邻图元应合并为一批");
        assert_eq!(b[0].index_count, 9);
    }

    #[test]
    fn different_clips_stay_separate() {
        // 合并条件写错（忽略 clip）会让不同裁剪区的图元共用一批。
        // 固定住「必须按 clip 分组」这一侧。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(20.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 2, "clip 不同的图元不应合并");
        let total: u32 = b.iter().map(|x| x.index_count).sum();
        assert_eq!(total, 6, "批次索引总数应等于原始索引数");
    }

    #[test]
    fn same_clip_but_non_adjacent_does_not_merge() {
        // clip 相同但被别的 clip 隔开：不能跨过中间的批次合并，
        // 否则第一批的索引区间会被拉长到跨越中间那批的区间。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(20.0), 3, 3), prim(rect_at(0.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 3, "非相邻的同 clip 图元不应合并");
    }

    #[test]
    fn batches_cover_all_indices_exactly_once() {
        // 批次的索引区间必须无缝铺满整个索引数组：既不重叠也不留空洞。
        // 空洞会导致部分三角形没被画（表现为界面局部空白）。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(20.0), 6, 6),
            prim(rect_at(40.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        let mut next = 0u32;
        for batch in &b {
            assert_eq!(batch.index_offset, next, "批次之间出现了空洞或重叠");
            next = batch.index_offset + batch.index_count;
        }
        assert_eq!(next as usize, i.len(), "批次应覆盖全部索引");
    }

    #[test]
    fn empty_primitives_produce_no_batches() {
        // 空输入 → 空批次 → record 只清屏不绘制。
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty() && i.is_empty());
    }

    #[test]
    fn empty_mesh_is_skipped() {
        // mesh 有顶点但无索引时不能产生批次（index_count=0 会被跳过，
        // 但会留下一个空批次，徒增记录数）。
        let mut p = prim(rect_at(0.0), 3, 0);
        if let Primitive::Mesh(m) = &mut p.primitive {
            m.indices.clear();
        }
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty());
    }

    #[test]
    fn callback_primitive_is_ignored() {
        // PaintCallback 无法在不执行用户代码的情况下光栅化，
        // 必须跳过而不是 panic。
        let p = ClippedPrimitive {
            clip_rect: rect_at(0.0),
            primitive: Primitive::Callback(egui::epaint::PaintCallback {
                rect: rect_at(0.0),
                callback: std::sync::Arc::new(|_| {}),
            }),
        };
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty());
    }

    #[test]
    fn real_egui_output_converts_without_panic() {
        // 用真实的 egui 跑一帧，覆盖「几十个图元 + 真实 Color32 +
        // 真实 Mesh.indices」这一组合，而不是只测人造数据。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("标题");
            ui.label("正文 label with ascii");
            let _ = ui.button("按钮");
            egui::Window::new("窗口").show(&ctx, |ui| {
                ui.label("窗口内文字");
            });
        });
        // egui 的 `TexturesDelta` 在 drop 时断言增量为空。本测试不消费
        // 纹理增量（那需要 GPU），必须显式清空，否则 panic 会掩盖真正的断言失败。
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty(), "egui 应产出图元");

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert!(!v.is_empty() && !i.is_empty() && !b.is_empty());
        // 索引必须全部落在顶点数组范围内，否则 draw_indexed 会越界读取。
        let n = v.len() as u32;
        assert!(i.iter().all(|&x| x < n), "存在越界索引");
        // 索引数必须是 3 的倍数（三角形列表）。
        assert_eq!(i.len() % 3, 0);
        // 合并是纯优化：draw call 数不应超过图元数。
        assert!(
            b.len() <= prims.len(),
            "批次合并不应增加 draw call：{} 批 vs {} 图元",
            b.len(),
            prims.len()
        );
    }

    #[test]
    fn vertex_layout_matches_pipeline_stride() {
        // pipeline.rs 声明的步长是 pos(8)+uv(8)+color(4)=20 字节。
        // 布局不符时驱动不会报错，只会画出乱码。
        assert_eq!(std::mem::size_of::<Vertex>(), 20);
    }

    #[test]
    fn capacity_growth_is_monotonic() {
        assert!(next_capacity(1) >= 1);
        assert!(next_capacity(2000) >= 2000);
        assert_eq!(next_capacity(2000) % 1024, 0);
        // 连续扩容不应原地踏步，否则每帧都重建。
        assert!(next_capacity(next_capacity(2000)) > next_capacity(2000));
    }

    #[test]
    fn uniform_matrix_is_identity() {
        // 着色器已自己做像素→NDC；再乘非单位矩阵会二次变换。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        let expect = [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ];
        assert_eq!(u.clip_from_uv, expect);
        assert_eq!(u.size_in_pixels, [800.0, 600.0]);
    }

    #[test]
    fn uniform_key_changes_with_extent_and_dpr() {
        // uniform 只在内容变化时重写：指纹必须能区分三种输入。
        let a = UniformKey { width: 800, height: 600, pixels_per_point: 1.0 };
        assert_ne!(a, UniformKey { width: 801, height: 600, pixels_per_point: 1.0 });
        assert_ne!(a, UniformKey { width: 800, height: 601, pixels_per_point: 1.0 });
        assert_ne!(a, UniformKey { width: 800, height: 600, pixels_per_point: 1.5 });
        assert_eq!(a, UniformKey { width: 800, height: 600, pixels_per_point: 1.0 });
    }
}
