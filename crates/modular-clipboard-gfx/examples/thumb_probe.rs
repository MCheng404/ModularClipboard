//! 缩略图上屏验证探针。
//!
//! # 为什么 `full_app` 不算验证过缩略图
//!
//! `full_app` 只画字体图集（`tex_id: 0`），且它的 `apply_font_delta`
//! **显式丢弃**非字体纹理（`tracing::warn!("忽略非字体纹理")` + `continue`）。
//! 因此它跑 600 帧、VE=0 也**完全没有触及**用户纹理路径——
//! `46518f3` 报告的「链路接通、VE=0」并不能证明缩略图能上屏。
//!
//! # 本探针分三部分
//!
//! 1. **egui 侧**（纯 CPU，`cargo test` 也能跑）：真实 `load_texture` +
//!    `ui.image` 会不会产出非字体纹理？tessellation 后的顶点是否真带
//!    `tex_id == 1`？
//! 2. **生产路径**：把 RGBA 字节喂给 `FrameRenderer::record_texture_upload`
//!    ——也就是 `ui::renderer` 的完全相同调用——看它到底成不成功。
//! 3. **A/B 反证**：绕开第 2 部分暴露的问题后，`tex_id` 机制本身
//!    是否真的在选纹理？用**离屏渲染 + 像素读回**做对照，不靠肉眼。
//!
//! # 已发现的断路（第 2 部分会实机复现）
//!
//! `texture.rs::DeviceImage::upload` 用 **R8 单通道**的字节数做硬校验：
//!
//! ```text
//! let needed = staging_size_r8(w, h);   // = w * h
//! ensure!(bytes.len() == needed, ...)
//! ```
//!
//! 而用户纹理是 **RGBA 四通道**，字节数 `4 * w * h`。两者恒不相等
//! ⇒ `ensure!` 必然 bail ⇒ **GPU 命令一条都没录制就失败**。
//! 连 `renderer.rs` 的 1×1 占位纹理（传 4 字节、校验要 1 字节）也会失败。
//!
//! **结论：缩略图上不了屏的直接原因不是 `tex_id`，而是这处字节校验。**
//! `texture.rs` 不在本探针的可改范围内，所以第 3 部分**手工绕开**校验
//! （自己录屏障 + `cmd_copy_buffer_to_image`），从而把
//! 「上传被拦」与「`tex_id` 失效」这两件事彻底分开。
//!
//! # 为什么离屏渲染而不是截窗口
//!
//! 交换链图像的 `image_usage` 只有 `COLOR_ATTACHMENT | TRANSFER_DST`
//! （`lib.rs::Swapchain::new`），**没有 `TRANSFER_SRC`**
//! ⇒ 无法把交换链像素拷回主机。而 `PrintWindow`/BitBlt 受遮挡、
//! 缩放、合成器影响，不确定。
//!
//! 离屏方案只用 gfx 的**公开 API**（`RenderPass::new` /
//! `GraphicsPipeline::new` / `DeviceImage::new` / `DeviceImage::read_to_buffer`）
//! 自建一套，**不修改 gfx 任何文件**，采样值完全确定可复现。
//!
//! # A/B 怎么保证「测得出差异」
//!
//! 三张纹理内容**刻意互斥**，任何「采样错纹理」都会立刻暴露：
//!
//! | 资源 | 内容 | 渲染出的颜色 |
//! |------|------|-------------|
//! | 清屏色 | 纯蓝 | 蓝 =「零覆盖 / 什么都没画」 |
//! | 合成字体图集（binding 2） | 左半 0 覆盖、右半 255 覆盖 | 左半蓝、右半白 |
//! | 缩略图（binding 3） | 8×8 红绿棋盘，alpha=255 | 饱和红或饱和绿 |
//!
//! - **A（`tex_id = 1`）**：整屏红绿棋盘。
//! - **B（`tex_id` 被强制为 0）**：左半蓝、右半白。
//!
//! 判据是「饱和红绿像素计数」：A 应占绝大多数，**B 必须恰好为 0**。
//!
//! 这个判据可靠的根本在于：**字体图集是单通道覆盖率图，采样结果必然
//! `r == g == b`（灰度）**，无论怎样采样都不可能出现饱和红绿。
//! 因此 B 组只要出现一个彩色像素，就说明采样错了纹理。
//!
//! # 用法
//!
//! ```text
//! cargo run -p modular-clipboard-gfx --example thumb_probe
//! ```
//!
//! 退出码 0 = 结论均符合预期（**含**「生产路径 RGBA 上传被拒」这一预期失败）。
//! 退出码 1 = 出现非预期结果（A/B 无差异、egui 侧断言失败等）。

use ash::vk;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{DrawInput, FrameRenderer, PipelineBundle};
use modular_clipboard_gfx::pipeline::{
    BINDING_SAMPLER, BINDING_TEXTURE, BINDING_UNIFORM, BINDING_USER_TEXTURE, Uniforms,
};
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};
use modular_clipboard_gfx::window::Window;
use modular_clipboard_gfx::Gpu;

/// 窗口客户区尺寸（物理像素）。
const W: u32 = 640;
const H: u32 = 480;

/// 离屏靶面边长（像素）。
///
/// 取 64：读回缓冲 `64*64*4 = 16KB`；8×8 棋盘放大到 64×64 后每格 8×8 像素，
/// 采样点能稳稳落在格子正中。
const TARGET: u32 = 64;

/// 缩略图边长（像素）。
const THUMB: u32 = 8;

/// 合成字体图集边长（像素）。
const FONT_TEX: u32 = 64;

/// 清屏色：纯蓝。
///
/// 选蓝而非黑：它是「零覆盖」的唯一标识，且与红/绿/白都不重合。
const CLEAR: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

/// egui 约定 `Managed(0)` 恒为字体图集。
const FONT_TEXTURE_ID: egui::TextureId = egui::TextureId::Managed(0);

/// 着色器纹理槽位：`0` = 字体图集，`1` = 用户纹理。
const TEX_SLOT_FONT: u32 = 0;
const TEX_SLOT_USER: u32 = 1;

/// 「饱和度」阈值。
///
/// 字体图集采样出的灰度满足 `max - min == 0`，与阈值无关；
/// 缩略图的红/绿满足 `max - min == 255`。取 60 留足量化余量。
const SATURATION_THRESHOLD: i32 = 60;

/// 四边形顶点数 / 索引数。
const QUAD_VERTS: usize = 4;
const QUAD_INDICES: [u32; 6] = [0, 1, 2, 0, 2, 3];

/// 4×4 单位矩阵（列主序）。着色器自己做像素 → NDC，矩阵只作非零占位。
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL thumb_probe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // ---- 窗口 ----------------------------------------------------------
    let window = Window::new("ModularClipboard — thumb_probe", W, H)?;
    println!(
        "OK Window  客户区 {}x{}  scale_factor={}",
        W,
        H,
        window.scale_factor()
    );

    // ---- 设备 ----------------------------------------------------------
    let gpu = Gpu::new("ModularClipboardThumbProbe", window.hinstance(), window.hwnd())?;
    println!(
        "OK Gpu  {:?}  queue_family={}  formats={}",
        gpu.device_type,
        gpu.queue_family,
        gpu.surface_formats.len()
    );

    // ---- 管线（照 full_app 的初始化，用于第 2 部分的帧循环） --------------
    let surface_format = gpu
        .surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .ok_or_else(|| anyhow::anyhow!("表面无可用格式"))?;

    let render_pass = modular_clipboard_gfx::pipeline::RenderPass::new(&gpu.device, surface_format)?;
    let update_after_bind = gpu.desc_caps.all_bindings_update_after_bind();
    let desc_layout =
        modular_clipboard_gfx::pipeline::DescriptorLayout::new(&gpu.device, update_after_bind)?;
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
    println!("OK 管线（surface 格式 {surface_format:?}）");

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

    // =====================================================================
    // 第 1 部分：egui 侧
    // =====================================================================
    println!("\n===== 第 1 部分：egui 侧产出（纯 CPU） =====");
    let part1 = part1_egui_side(&window)?;

    // =====================================================================
    // 第 2 部分：生产路径 RGBA 上传
    // =====================================================================
    println!("\n===== 第 2 部分：生产路径 RGBA 上传 =====");
    let part2 = part2_production_upload(&gpu, &mut fr);

    // =====================================================================
    // 第 3 部分：A/B 像素反证
    // =====================================================================
    println!("\n===== 第 3 部分：A/B 像素反证（离屏 + 读回） =====");
    let part3 = part3_ab_pixel_readback(&gpu, pipe_layout.handle, desc_layout.handle, &shader)?;

    // ---- 清理 ------------------------------------------------------------
    gpu.wait_idle();
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    desc_layout.destroy(&gpu.device);
    render_pass.destroy(&gpu.device);

    // ---- 结论 ------------------------------------------------------------
    let total = part3.total;
    println!("\n===== 结论 =====");
    println!("【第 1 部分：egui 侧】");
    println!("  TexturesDelta.set 条目数   : {}", part1.delta_len);
    println!("  出现非字体纹理            : {}", yesno(part1.user_texture_seen));
    println!(
        "  顶点 tex_id==1 / tex_id==0: {} / {}",
        part1.vertices_user, part1.vertices_font
    );
    println!("【第 2 部分：生产路径 RGBA 上传】");
    match &part2 {
        UploadOutcome::Accepted => println!("  被接受（字节校验已修复）"),
        UploadOutcome::Rejected(msg) => println!("  被拒绝 -> {msg}"),
    }
    println!("【第 3 部分：A/B 像素反证】");
    println!("  A（tex_id=1）红/绿主导像素 : {}/{}", part3.a_warm, total);
    println!("  B（tex_id=0）红/绿主导像素 : {}/{}", part3.b_warm, total);
    println!("  A 主色: {}", part3.a_dominant);
    println!("  B 主色: {}", part3.b_dominant);
    println!("  A 采样点 RGBA: {:?}", part3.a_samples);
    println!("  B 采样点 RGBA: {:?}", part3.b_samples);

    // ---- 判据 ------------------------------------------------------------
    //
    // 硬判据只有一条：**A/B 必须有差异**。
    //
    // 第 2 部分的失败是**预期结果**（它要证明的就是断路存在），
    // 因此不参与退出码判定——它已被完整打印出来供人工裁决。
    anyhow::ensure!(
        part1.delta_len >= 2,
        "TexturesDelta.set.len() = {}，少于 2（应有字体图集 + 用户纹理两条）",
        part1.delta_len
    );
    anyhow::ensure!(
        part1.user_texture_seen,
        "egui 未产出非字体纹理：load_texture + ui.image 的路径没走通"
    );
    anyhow::ensure!(
        part1.vertices_user > 0,
        "顶点里没有任何 tex_id == 1 的顶点：tessellation 没把纹理槽位写进顶点"
    );
    anyhow::ensure!(
        part1.vertices_font > 0,
        "顶点里没有任何 tex_id == 0 的顶点：字体图集路径反而没产出，测试图有问题"
    );
    anyhow::ensure!(
        part3.a_warm > 0 && part3.b_warm == 0,
        "A/B 对照失效：A 有 {} 个红/绿主导像素、B 有 {} 个。\
         两者相同意味着 tex_id 机制已坏，或本探针的对照设计被破坏",
        part3.a_warm,
        part3.b_warm
    );

    println!("\nALL OK - 缩略图链路结论已产出（详见上方三部分）");
    Ok(())
}

// ---------------------------------------------------------------------------
// 第 1 部分：egui 侧
// ---------------------------------------------------------------------------

/// 第 1 部分的产出。
struct Part1 {
    delta_len: usize,
    user_texture_seen: bool,
    vertices_user: usize,
    vertices_font: usize,
}

/// 走真实 egui 路径：造图 → `load_texture` → `ui.image` + 文字 → tessellate。
///
/// 界面必须**同时**有文字和图片：文字产出字体图集增量（`Managed(0)`），
/// 图片产出用户纹理增量。两者缺一，就测不到「一次绘制里混两种纹理」
/// 这个 `tex_id` 方案唯一要解决的场景。
fn part1_egui_side(window: &Window) -> anyhow::Result<Part1> {
    let ctx = egui::Context::default();

    let handle = ctx.load_texture(
        "thumb-probe",
        checkerboard(THUMB),
        egui::TextureOptions::NEAREST,
    );
    println!("   缩略图 TextureId = {:?}（字体图集 = {FONT_TEXTURE_ID:?}）", handle.id());

    let (pw, ph) = window.inner_size_points();
    let screen = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(pw.max(1.0), ph.max(1.0)),
    );

    let raw = egui::RawInput {
        screen_rect: Some(screen),
        ..Default::default()
    };
    let mut out = ctx.run_ui(raw, |ui| {
        ui.label("ASCII baseline: The quick brown fox 0123456789");
        ui.add(egui::Image::new(&handle).fit_to_exact_size(egui::vec2(256.0, 256.0)));
    });

    // ---- 断言：TexturesDelta 里有非字体纹理 ------------------------------
    let delta_len = out.textures_delta.set.len();
    let ids: Vec<egui::TextureId> = out.textures_delta.set.keys().copied().collect();
    let user_texture_seen = ids.iter().any(|id| *id != FONT_TEXTURE_ID);
    println!("   TexturesDelta.set = {delta_len} 条：{ids:?}");
    // `Drop for TexturesDelta` 有 `debug_assert!(is_empty())`。
    // 必须显式清空，否则这个与本探针无关的 panic 会掩盖真正的断言失败。
    out.textures_delta.clear();

    // ---- 断言：顶点真的带上了纹理槽位 ------------------------------------
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let (vertices, _indices, _batches) = build_vertices(&prims, false);
    let vertices_user = vertices.iter().filter(|v| v.tex_id == TEX_SLOT_USER).count();
    let vertices_font = vertices.iter().filter(|v| v.tex_id == TEX_SLOT_FONT).count();
    println!(
        "   tessellate 产出 {} 顶点 / {} 图元：tex_id=1 有 {vertices_user} 个，tex_id=0 有 {vertices_font} 个",
        vertices.len(),
        prims.len()
    );

    Ok(Part1 {
        delta_len,
        user_texture_seen,
        vertices_user,
        vertices_font,
    })
}

// ---------------------------------------------------------------------------
// 第 2 部分：生产路径 RGBA 上传
// ---------------------------------------------------------------------------

/// 上传结果。
enum UploadOutcome {
    /// `upload()` 接受了 RGBA 字节。
    Accepted,
    /// 被拒（**预期结果**）。
    Rejected(String),
}

/// 用**生产代码的完全相同调用**上传一张 RGBA 缩略图。
///
/// 调用链与 `ui::renderer::upload_font_delta` 的用户纹理分支一致：
/// `record_texture_upload(&mut image, &rgba_bytes, None, (w, h))`。
///
/// 无论成败都把这一帧正常收尾（record + present），
/// 否则 `FrameRenderer` 会一直认为「上一帧尚未 present」而拒绝下一帧。
fn part2_production_upload(gpu: &Gpu, fr: &mut FrameRenderer<'_>) -> UploadOutcome {
    let (w, h) = (THUMB, THUMB);

    let mut img = match DeviceImage::new(
        gpu,
        vk::Extent2D { width: w, height: h },
        vk::Format::R8G8B8A8_UNORM,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    ) {
        Ok(i) => i,
        Err(e) => {
            return UploadOutcome::Rejected(format!("创建 R8G8B8A8_UNORM 图像失败: {e:#}"));
        }
    };

    let rgba = flatten(checkerboard(h).pixels.iter().copied().collect());
    let r8_expected = modular_clipboard_gfx::texture::staging_size_r8(w, h);
    println!("   缩略图 {w}x{h}：{} 像素，实际传 {} 字节", w * h, rgba.len());
    println!("   upload() 的字节校验期望 {r8_expected} 字节（staging_size_r8 = w*h）");

    // acquire 是 `record_texture_upload` 的前置条件：
    // 命令必须录进本帧命令缓冲，且在渲染通道作用域之外。
    let acquired = match fr.acquire() {
        Ok(Some(a)) => a,
        Ok(None) => {
            img.destroy(&gpu.device);
            return UploadOutcome::Rejected("acquire 返回过期".to_string());
        }
        Err(e) => {
            img.destroy(&gpu.device);
            return UploadOutcome::Rejected(format!("acquire 失败: {e:#}"));
        }
    };

    // 关键：这就是生产路径的那一次调用。
    let result = fr.record_texture_upload(&mut img, &rgba, None, (w, h));

    // 无论成败都收尾这一帧。
    let finish = (|| -> anyhow::Result<()> {
        fr.record(&DrawInput::default())?;
        fr.present(acquired)?;
        gpu.wait_idle();
        Ok(())
    })();

    let layout = img.layout;
    img.destroy(&gpu.device);

    match (result, finish) {
        (Ok(()), Ok(())) => {
            println!("   上传成功，布局已推进到 {layout:?}");
            UploadOutcome::Accepted
        }
        (Ok(()), Err(e)) => UploadOutcome::Rejected(format!("上传成功但收尾失败: {e:#}")),
        (Err(e), Ok(())) => {
            println!("   失败信息：{e}");
            println!("   ⇒ 断点在 CPU 侧字节校验，GPU 命令一条都没录制");
            UploadOutcome::Rejected(format!("{e}"))
        }
        (Err(e), Err(f)) => UploadOutcome::Rejected(format!("{e}；且收尾失败: {f:#}")),
    }
}

// ---------------------------------------------------------------------------
// 第 3 部分：A/B 像素反证（离屏渲染 + 读回）
// ---------------------------------------------------------------------------

/// 第 3 部分的产出。
struct Part3 {
    total: usize,
    a_warm: usize,
    b_warm: usize,
    a_samples: Vec<[u8; 4]>,
    b_samples: Vec<[u8; 4]>,
    a_dominant: &'static str,
    b_dominant: &'static str,
}

/// 自建离屏靶面，渲染两次并读回像素做 A/B 对照。
///
/// 全程只用 gfx 的公开 API，**不修改 gfx 任何文件**。
fn part3_ab_pixel_readback(
    gpu: &Gpu,
    pipe_layout: vk::PipelineLayout,
    desc_set_layout: vk::DescriptorSetLayout,
    shader: &modular_clipboard_gfx::shader::ShaderModule,
) -> anyhow::Result<Part3> {
    let device = &gpu.device;

    // ---- 离屏靶面 --------------------------------------------------------
    //
    // 用途必须含 `TRANSFER_SRC`：`read_to_buffer` 靠它把像素拷回主机。
    // 交换链图像没有这个位，这正是本探针不用交换链的原因。
    let mut target = DeviceImage::new(
        gpu,
        vk::Extent2D {
            width: TARGET,
            height: TARGET,
        },
        vk::Format::R8G8B8A8_UNORM,
        vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_DST,
    )?;

    // 靶面渲染通道：格式必须与靶面图像一致。
    let target_pass = modular_clipboard_gfx::pipeline::RenderPass::new(device, target.format)?;
    // 管线必须与渲染通道配套（`GraphicsPipelineCreateInfo.render_pass`）。
    let target_pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
        device,
        target_pass.handle,
        pipe_layout,
        shader,
    )?;

    let fb_attachments = [target.view];
    let fb_info = vk::FramebufferCreateInfo::default()
        .render_pass(target_pass.handle)
        .attachments(&fb_attachments)
        .width(TARGET)
        .height(TARGET)
        .layers(1);
    let framebuffer = unsafe {
        device
            .create_framebuffer(&fb_info, None)
            .map_err(|e| anyhow::anyhow!("创建离屏 framebuffer 失败: {e:?}"))?
    };

    // ---- 命令缓冲 / 栅栏 ---------------------------------------------------
    let cmd_pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe {
        device
            .create_command_pool(&cmd_pool_info, None)
            .map_err(|e| anyhow::anyhow!("创建命令池失败: {e:?}"))?
    };
    let cmd_alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd_buffers = unsafe {
        device
            .allocate_command_buffers(&cmd_alloc)
            .map_err(|e| anyhow::anyhow!("分配命令缓冲失败: {e:?}"))?
    };
    anyhow::ensure!(cmd_buffers.len() == 1, "命令缓冲分配数量不符");
    let cmd = cmd_buffers[0];

    let fence = unsafe {
        device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .map_err(|e| anyhow::anyhow!("创建栅栏失败: {e:?}"))?
    };

    // 提交并等栅栏。离屏渲染完全绕开帧层，因此自己管同步。
    let submit_and_wait = || -> anyhow::Result<()> {
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
        unsafe {
            device.queue_submit(gpu.queue, std::slice::from_ref(&submit), fence)?;
            device.wait_for_fences(&[fence], true, u64::MAX)?;
            device.reset_fences(&[fence])?;
        }
        Ok(())
    };

    // ---- 纹理上传（独立一次提交） ------------------------------------------
    let sampler = Sampler::new_nearest(device)?;

    // 合成字体图集：左半 0 覆盖、右半 255 覆盖（R8，走**正常**上传路径）。
    //
    // 采样它会得到：左半 alpha=0（透出清屏蓝）、右半 alpha=1（白）。
    // 于是「采样了字体图集」在读回里表现为蓝/白，**绝无饱和红绿**。
    let mut font = DeviceImage::new(
        gpu,
        vk::Extent2D {
            width: FONT_TEX,
            height: FONT_TEX,
        },
        FONT_FORMAT,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )?;
    let font_coverage = left_dark_right_white(FONT_TEX);

    // 缩略图：8×8 红绿棋盘（RGBA）。
    let mut thumb = DeviceImage::new(
        gpu,
        vk::Extent2D {
            width: THUMB,
            height: THUMB,
        },
        vk::Format::R8G8B8A8_UNORM,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )?;
    let thumb_rgba = flatten(checkerboard(THUMB).pixels.iter().copied().collect());

    {
        // staging 生命周期必须覆盖 GPU 执行时间，所以活到 submit_and_wait 之后。
        // （MEMORY坑 28：上传函数内建局部 staging 是 use-after-free。）
        let mut font_staging = StagingBuffer::new(gpu, font_coverage.len() as vk::DeviceSize)?;
        let mut thumb_staging = StagingBuffer::new(gpu, thumb_rgba.len() as vk::DeviceSize)?;
        font_staging.write(&font_coverage)?;
        thumb_staging.write(&thumb_rgba)?;

        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        }
        // R8 字节数 = w*h，能过 `upload()` 的校验 ⇒ 这条路径是通的，
        // 与第 2 部分 RGBA 的失败形成对照。
        font.upload(gpu, cmd, font_staging.slice(), &font_coverage, None, (FONT_TEX, FONT_TEX))?;
        println!(
            "   字体图集走 upload() 上传 {} 字节（R8，字节数匹配）-> OK",
            font_coverage.len()
        );
        // RGBA 走手工路径，绕开那条字节校验。
        upload_rgba_bypassing_check(device, cmd, &mut thumb, thumb_staging.slice())?;
        println!(
            "   缩略图绕开校验上传 {} 字节（RGBA = 4*w*h）-> OK，布局 {:?}",
            thumb_rgba.len(),
            thumb.layout
        );
        unsafe {
            device.end_command_buffer(cmd)?;
        }
        submit_and_wait()?;
    }

    // ---- uniform + 描述符集 ------------------------------------------------
    let mut ub = UniformBuffer::new(gpu)?;
    ub.write(
        device,
        &Uniforms::new(IDENTITY, [TARGET as f32, TARGET as f32], 1.0),
    )?;

    // 池的三种计数与 `frame.rs` 一致：
    // 1 个 uniform、1 个采样器、**2** 张组合图像采样器（字体 + 用户）。
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
    let pool_flags = if gpu.desc_caps.all_bindings_update_after_bind() {
        vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND
    } else {
        vk::DescriptorPoolCreateFlags::empty()
    };
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .flags(pool_flags)
        .max_sets(1)
        .pool_sizes(&pool_sizes);
    let desc_pool = unsafe {
        device
            .create_descriptor_pool(&pool_info, None)
            .map_err(|e| anyhow::anyhow!("创建离屏描述符池失败: {e:?}"))?
    };

    let layouts = [desc_set_layout];
    let alloc_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(desc_pool)
        .set_layouts(&layouts);
    let sets = unsafe {
        device
            .allocate_descriptor_sets(&alloc_info)
            .map_err(|e| anyhow::anyhow!("分配离屏描述符集失败: {e:?}"))?
    };
    anyhow::ensure!(sets.len() == 1, "描述符集分配数量不符：{}", sets.len());
    let desc_set = sets[0];

    // 四个绑定全部写齐。
    //
    // `imageLayout` 必须是上传后的 `SHADER_READ_ONLY_OPTIMAL`——
    // 规范禁止把 `UNDEFINED` 写进 `COMBINED_IMAGE_SAMPLER`
    // （`VUID-VkWriteDescriptorSet-descriptorType-04150`）。
    // 上传已在上面完成，故此刻 `layout` 字段已是 SHADER_READ_ONLY_OPTIMAL。
    let font_info = font.descriptor_info(sampler.handle());
    let thumb_info = thumb.descriptor_info(sampler.handle());
    let ub_info = ub.buffer().descriptor_info();
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(desc_set)
            .dst_binding(BINDING_UNIFORM)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(std::slice::from_ref(&ub_info)),
        vk::WriteDescriptorSet::default()
            .dst_set(desc_set)
            .dst_binding(BINDING_SAMPLER)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(std::slice::from_ref(&font_info)),
        vk::WriteDescriptorSet::default()
            .dst_set(desc_set)
            .dst_binding(BINDING_TEXTURE)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&font_info)),
        vk::WriteDescriptorSet::default()
            .dst_set(desc_set)
            .dst_binding(BINDING_USER_TEXTURE)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&thumb_info)),
    ];
    unsafe { device.update_descriptor_sets(&writes, &[]) };

    // ---- 顶点 / 索引 / 读回缓冲 --------------------------------------------
    let mut vb = Buffer::new_host_visible(
        gpu,
        (QUAD_VERTS * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )?;
    let mut ib = Buffer::new_host_visible(
        gpu,
        (QUAD_INDICES.len() * 4) as vk::DeviceSize,
        vk::BufferUsageFlags::INDEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )?;
    ib.write(device, 0, bytemuck::cast_slice(&QUAD_INDICES))?;

    // 读回缓冲：R8G8B8A8 每像素 4 字节，给足 4 倍。
    // （`read_to_buffer` 的容量校验按 R8 算 `w*h`，因此必然通过。）
    let mut readback = Buffer::new_host_visible(
        gpu,
        (TARGET as u64 * TARGET as u64 * 4) as vk::DeviceSize,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;

    // ---- 渲染 A / B ------------------------------------------------------
    let mut render_and_read = |tex_id: u32| -> anyhow::Result<Vec<u8>> {
        vb.write(device, 0, bytemuck::cast_slice(&quad_vertices(tex_id)))?;

        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;

            // 靶面 → COLOR_ATTACHMENT_OPTIMAL。
            // 必须显式转换：本探针自建的渲染通道与 `pipeline.rs` 一样声明
            // `initial_layout = COLOR_ATTACHMENT_OPTIMAL`，不预转换会让
            // 渲染通道的隐式转换与实际布局脱节。
            target.transition_to(
                cmd,
                device,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            );

            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: TARGET as f32,
                height: TARGET as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            let scissor = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D {
                    width: TARGET,
                    height: TARGET,
                },
            };
            device.cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
            device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&scissor));

            let clear = vk::ClearValue {
                color: vk::ClearColorValue { float32: CLEAR },
            };
            let clear_values = [clear];
            let begin = vk::RenderPassBeginInfo::default()
                .render_pass(target_pass.handle)
                .framebuffer(framebuffer)
                .render_area(scissor)
                .clear_values(&clear_values);
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);

            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, target_pipeline.handle);
            let sets = [desc_set];
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                pipe_layout,
                0,
                &sets,
                &[],
            );
            let vbufs = [vb.handle()];
            let offsets = [0];
            device.cmd_bind_vertex_buffers(cmd, 0, &vbufs, &offsets);
            device.cmd_bind_index_buffer(cmd, ib.handle(), 0, vk::IndexType::UINT32);
            device.cmd_draw_indexed(cmd, QUAD_INDICES.len() as u32, 1, 0, 0, 0);

            device.cmd_end_render_pass(cmd);

            // 读回：转 TRANSFER_SRC → 拷贝 → 转回 SHADER_READ_ONLY。
            target.read_to_buffer(cmd, device, &readback)?;

            device.end_command_buffer(cmd)?;
        }
        submit_and_wait()?;

        readback.map(device)?;
        let all = readback.read::<u8>()?;
        let need = (TARGET as usize) * (TARGET as usize) * 4;
        anyhow::ensure!(all.len() >= need, "读回只有 {} 字节，不足 {need}", all.len());
        let out = all[..need].to_vec();
        readback.unmap(device)?;
        Ok(out)
    };

    // A：tex_id = 1 ⇒ 应采样缩略图 ⇒ 饱和红绿
    let a_px = render_and_read(TEX_SLOT_USER)?;
    let a_warm = count_thumbnail_like(&a_px);
    // B：tex_id = 0 ⇒ 应采样字体图集 ⇒ 蓝/白，零饱和红绿
    let b_px = render_and_read(TEX_SLOT_FONT)?;
    let b_warm = count_thumbnail_like(&b_px);

    let a_samples = sample_points(&a_px);
    let b_samples = sample_points(&b_px);
    let a_dominant = classify_dominant(&a_px);
    let b_dominant = classify_dominant(&b_px);

    println!("   A（tex_id=1）：{a_warm} 个红/绿主导像素，主色 {a_dominant}");
    println!("   A 采样点 RGBA: {a_samples:?}");
    println!("   B（tex_id=0）：{b_warm} 个红/绿主导像素，主色 {b_dominant}");
    println!("   B 采样点 RGBA: {b_samples:?}");

    // ---- 清理 ------------------------------------------------------------
    // 顺序：先等GPU 空闲，再按「引用者 → 被引用者」逆序销毁。
    gpu.wait_idle();
    unsafe {
        device.destroy_fence(fence, None);
        device.destroy_command_pool(cmd_pool, None);
        device.destroy_framebuffer(framebuffer, None);
        device.destroy_descriptor_pool(desc_pool, None);
    }
    readback.destroy(device);
    ib.destroy(device);
    vb.destroy(device);
    ub.destroy(device);
    thumb.destroy(device);
    font.destroy(device);
    sampler.destroy(device);
    target_pipeline.destroy(device);
    target_pass.destroy(device);
    target.destroy(device);

    Ok(Part3 {
        total: (TARGET as usize) * (TARGET as usize),
        a_warm,
        b_warm,
        a_samples,
        b_samples,
        a_dominant,
        b_dominant,
    })
}

/// 绕开 `upload()` 的 R8 字节校验，手工把 RGBA 数据拷进图像。
///
/// # 为什么必须绕开
///
/// `DeviceImage::upload` 的 `ensure!(bytes.len() == w*h)` 只接受 R8。
/// 本函数做的是与 `upload()` **完全相同**的三步
/// （转 `TRANSFER_DST` → `cmd_copy_buffer_to_image` → 转 `SHADER_READ_ONLY`），
/// 唯一区别是不做那条字节数校验。
///
/// # 为什么用 `transition_to` 而不是裸 `image_barrier`
///
/// `DeviceImage::layout` 是**权威记账**：`descriptor_info()` 把它写进描述符的
/// `imageLayout`。绕过它就会让下一次上传用**过期的 oldLayout** 算屏障
/// ⇒ 设备丢失（MEMORY 坑：曾有探针这样干，60 帧跑 10 帧就崩）。
///
/// # 前置条件
///
/// `cmd` 已 `begin_command_buffer` 且尚未 `end`。本函数在其内部完成
/// 全部布局转换，调用方无需额外插屏障。
fn upload_rgba_bypassing_check(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    img: &mut DeviceImage,
    staging: modular_clipboard_gfx::staging::StagingSlice,
) -> anyhow::Result<()> {
    anyhow::ensure!(!staging.is_null(), "staging 切片为空");
    let (w, h) = (img.size.width, img.size.height);

    // 源访问掩码按**原布局**决定（与 `upload()` 的 `upload_barrier_scope` 同规则）：
    // `UNDEFINED` 不需要等任何人。
    let (src_stage, src_access) = match img.layout {
        vk::ImageLayout::UNDEFINED => (
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::empty(),
        ),
        _ => (
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::AccessFlags::SHADER_READ,
        ),
    };
    img.transition_to(
        cmd,
        device,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        src_access,
        vk::AccessFlags::TRANSFER_WRITE,
        src_stage,
        vk::PipelineStageFlags::TRANSFER,
    );

    // 紧密排列：`buffer_row_length = 0`，与 `texture::buffer_image_copy` 一致。
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(staging.offset)
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
            width: w,
            height: h,
            depth: 1,
        });
    unsafe {
        device.cmd_copy_buffer_to_image(
            cmd,
            staging.buffer,
            img.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[copy],
        );
    }

    img.transition_to(
        cmd,
        device,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::AccessFlags::TRANSFER_WRITE,
        vk::AccessFlags::SHADER_READ,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
    );
    Ok(())
}

/// 一个主机可见的 staging 缓冲，`Drop` 时自动销毁。
///
/// # 为什么不直接用 `StagingArena`
///
/// `StagingArena` 的生命周期由**帧槽位栅栏**保证（那是帧层的职责）。
/// 本探针的离屏渲染完全绕开帧层、自己提交 + 等栅栏，
/// 因此用最小的「自己开、自己等完再关」结构，
/// 避免把帧层的记账语义套到一个不属于帧层的场景上。
struct StagingBuffer<'a> {
    buffer: Buffer,
    device: &'a ash::Device,
}

impl<'a> StagingBuffer<'a> {
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

    /// 把整块缓冲包成 [`StagingSlice`](modular_clipboard_gfx::staging::StagingSlice)。
    ///
    /// `offset` 恒为 0，满足 `vkCmdCopyBufferToImage` 的 `bufferOffset`
    /// 对齐要求（0 是任何对齐值的倍数）。
    fn slice(&self) -> modular_clipboard_gfx::staging::StagingSlice {
        modular_clipboard_gfx::staging::StagingSlice {
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

impl Drop for StagingBuffer<'_> {
    fn drop(&mut self) {
        // 调用方必须已 `device_wait_idle`（本探针每次上传后都等了栅栏）。
        self.buffer.destroy(self.device);
    }
}

// ---------------------------------------------------------------------------
// 测试图与纯逻辑
// ---------------------------------------------------------------------------

/// 合成一张 `size`×`size` 红绿棋盘（alpha 全不透明）。
///
/// # 为什么是红绿、为什么每格 2×2
///
/// - **红绿**：字体图集是单通道覆盖率，采样出来必然灰度；
///   用饱和红/绿作为「确实采到了用户纹理」的**充要证据**。
/// - **每格 2×2**：`size/4` 为格边长，8×8 图得到 4×4 格 = 每色 2×2 格，
///   放大到 64×64 靶面后每色占 32×32 像素，采样点不会骑在格边界上。
fn checkerboard(size: u32) -> egui::ColorImage {
    let cell = (size / 4).max(1);
    let mut px = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let odd = ((x / cell) + (y / cell)) % 2 == 0;
            px.push(if odd {
                egui::Color32::from_rgb(255, 0, 0)
            } else {
                egui::Color32::from_rgb(0, 255, 0)
            });
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &flatten(px))
}

/// `Vec<Color32>` → RGBA 字节。
///
/// 用 `to_array()` 而非手拆通道：它返回 `[r,g,b,a]` 且按**预乘**语义规范化，
/// 与 `renderer.rs`（`p.to_array()`）取字节的方式一致，
/// 保证探针喂给 GPU 的数据与生产路径同源。
fn flatten(px: Vec<egui::Color32>) -> Vec<u8> {
    px.iter().flat_map(|c| c.to_array()).collect()
}

/// 左半 0 覆盖、右半 255 覆盖的覆盖率数据（R8）。
fn left_dark_right_white(size: u32) -> Vec<u8> {
    (0..(size * size) as usize)
        .map(|i| if (i as u32) % size < size / 2 { 0u8 } else { 255u8 })
        .collect()
}

/// 覆盖整个靶面的四边形顶点。
///
/// 位置是**逻辑像素**且铺满 `TARGET`（着色器再乘 dpr 转 NDC）；
/// uv 覆盖 0..1 以采样整张纹理；顶点色不透明白。
///
/// # 顶点色为什么用不透明白
///
/// `tex_id == 1` 时片元着色器**直接返回纹理色、忽略顶点色**；
/// `tex_id == 0` 时返回 `顶点色 × (1,1,1,覆盖率)`。
/// 因此不透明白让B 组输出干净的 `覆盖率` 灰阶，便于读回判读。
fn quad_vertices(tex_id: u32) -> Vec<Vertex> {
    let t = TARGET as f32;
    let corners = [(0.0, 0.0), (t, 0.0), (t, t), (0.0, t)];
    let uvs = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    corners
        .iter()
        .zip(uvs.iter())
        .map(|(&(x, y), &(u, v))| Vertex {
            pos: [x, y],
            uv: [u, v],
            color: 0xFFFF_FFFF,
            tex_id,
        })
        .collect()
}

/// egui 图元 → 顶点数组（本探针只需顶点，索引不参与断言）。
///
/// `force_font_slot` 是 A/B 反证的核心开关：为 `true` 时**所有**纹理
/// 都被映射到槽位 0，即模拟「`texture_slot()` 恒返回 0」的那个 bug。
fn build_vertices(
    prims: &[egui::epaint::ClippedPrimitive],
    force_font_slot: bool,
) -> (Vec<Vertex>, Vec<u32>, usize) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut batches = 0usize;
    for prim in prims {
        let mesh = match &prim.primitive {
            egui::epaint::Primitive::Mesh(m) => m,
            egui::epaint::Primitive::Callback(_) => continue,
        };
        if mesh.indices.is_empty() {
            continue;
        }
        let base = vertices.len() as u32;
        for v in &mesh.vertices {
            vertices.push(Vertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: pack_color(v.color),
                tex_id: texture_slot(mesh.texture_id, force_font_slot),
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));
        batches += 1;
    }
    (vertices, indices, batches)
}

/// 把 egui 的 `TextureId` 映射到着色器槽位。
///
/// 与 `ui::renderer::texture_slot` 同逻辑。gfx 不能依赖 ui（反向会成环），
/// 故此处自持一份，并由单测锁定两处必须一致的行为。
fn texture_slot(id: egui::TextureId, force_font_slot: bool) -> u32 {
    if force_font_slot || id == FONT_TEXTURE_ID {
        TEX_SLOT_FONT
    } else {
        TEX_SLOT_USER
    }
}

/// egui 的 `Color32` → 顶点色 `u32`（小端 ABGR，非预乘）。
///
/// 与 `full_app::pack_color` 一致：着色器混合是
/// `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`（非预乘），而 epaint 顶点色是预乘的，
/// 不还原就会把 alpha 乘两次（半透明区域发黑，且不报任何 GPU 错误）。
fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    let (r, g, b) = if a == 255 {
        (r, g, b)
    } else if a == 0 {
        (0, 0, 0)
    } else {
        let f = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
        (f(r), f(g), f(b))
    };
    u32::from_le_bytes([r, g, b, a])
}

/// 统计「红或绿主导」的像素数 —— 即**只可能来自缩略图**的像素。
///
/// # 为什么不能用「饱和度」当判据（这是本探针自己踩的坑）
///
/// 最初判据是 `max - min > 阈值`。它对**灰度**确实永远为0
/// （字体图集是单通道覆盖率，采样必然 `r == g == b`），
/// 看起来很安全。但**清屏色是纯蓝 `(0,0,255)`，饱和度 255**，
/// 于是 B 组的左半（零覆盖⇒透出清屏蓝）会被大量计入，
/// 导致「B 组彩色数必须为 0」这条判据在**实现完全正确时也会失败**。
///
/// 这个错误是 `dominant_classification_separates_a_and_b` 单测抓出来的，
/// 不是实机——因此判据本身必须有单测，不能只靠 GPU 跑一遍。
///
/// # 现在为什么可靠
///
/// 判据改成「`r` 或 `g` 明显高于 `b`」，把蓝色**排除在外**：
///
/// - A 组（缩略图红/绿棋盘）：`r` 或 `g` 远高于 `b` ⇒ 几乎全部命中。
/// - B 组（字体图集）：只可能是灰度（`r == g == b`）或清屏蓝
///   （`r`、`g` 均为 0，远低于 `b`）⇒ **一个都不命中**。
///
/// 关键在于这两类颜色在数值上**不可能混淆**：
/// 蓝的 `b` 是最大通道，而红/绿的 `b` 是最小通道。
fn count_thumbnail_like(px: &[u8]) -> usize {
    let mut n = 0usize;
    for c in px.chunks_exact(4) {
        let (r, g, b) = (c[0] as i32, c[1] as i32, c[2] as i32);
        // 红主导或绿主导，且 `b` 不是最大通道。
        // 阈值 60 容忍驱动的量化偏差（1~2LSB）。
        if (r - b > SATURATION_THRESHOLD || g - b > SATURATION_THRESHOLD) && b <= r.max(g) {
            n += 1;
        }
    }
    n
}

/// 取四个采样点（四象限内）的 RGBA。
fn sample_points(px: &[u8]) -> Vec<[u8; 4]> {
    [
        (TARGET / 4, TARGET / 4),
        (TARGET * 3 / 4, TARGET / 4),
        (TARGET / 4, TARGET * 3 / 4),
        (TARGET * 3 / 4, TARGET * 3 / 4),
    ]
    .iter()
    .map(|&(x, y)| {
        let i = ((y as usize) * (TARGET as usize) + x as usize) * 4;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    })
    .collect()
}

/// 判定靶面主色分类（供人工读输出）。
///
/// # 为什么用「蓝 vs 白」的相对多少来分类
///
/// B 组的左半是「字体图集零覆盖 ⇒ 透出清屏蓝」、右半是「全覆盖 ⇒ 白」。
/// 哪一半更多取决于靶面与 uv 的对应关系，不该由本函数猜测，
/// 因此只如实报告两者的相对量。
fn classify_dominant(px: &[u8]) -> &'static str {
    let total = px.len() / 4;
    if count_thumbnail_like(px) * 2 > total {
        return "红绿主导（缩略图）";
    }
    let mut blue = 0usize;
    let mut white = 0usize;
    for c in px.chunks_exact(4) {
        let (r, g, b) = (c[0] as i32, c[1] as i32, c[2] as i32);
        if b > 120 && r < 90 && g < 90 {
            blue += 1;
        } else if r > 120 && g > 120 && b > 120 {
            white += 1;
        }
    }
    match blue.cmp(&white) {
        std::cmp::Ordering::Greater => "蓝为主（字体图集空白区 / 零覆盖）",
        std::cmp::Ordering::Less => "白为主（字体图集全覆盖区）",
        std::cmp::Ordering::Equal => "蓝白各半（字体图集左右分区）",
    }
}

fn yesno(b: bool) -> &'static str {
    if b { "是" } else { "否" }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 跑一次「文字 + 图片」的 egui 帧，返回该帧的全部观测。
    ///
    /// 抽成公共函数让「第 1 部分」与单测走**完全同一条代码路径**——
    /// 否则测试验证的是一个副本，而副本可能已经与探针漂移。
    fn observe_frame(force_font_slot: bool) -> (usize, bool, Vec<Vertex>) {
        let ctx = egui::Context::default();
        let handle = ctx.load_texture(
            "unit",
            checkerboard(THUMB),
            egui::TextureOptions::NEAREST,
        );
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.label("The quick brown fox 0123456789");
            ui.add(egui::Image::new(&handle).fit_to_exact_size(egui::vec2(128.0, 128.0)));
        });
        let ids: Vec<egui::TextureId> = out.textures_delta.set.keys().copied().collect();
        // `Drop for TexturesDelta` 有 `debug_assert!(is_empty())`。
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let (v, _i, _b) = build_vertices(&prims, force_font_slot);
        (ids.len(), ids.iter().any(|id| *id != FONT_TEXTURE_ID), v)
    }

    // ---- 纹理槽位映射 ---------------------------------------------------

    #[test]
    fn font_texture_is_managed_zero() {
        // epaint 约定：Managed(0) 恒为字体图集。改错会让所有文字变豆腐块。
        assert_eq!(FONT_TEXTURE_ID, egui::TextureId::Managed(0));
        assert_eq!(texture_slot(FONT_TEXTURE_ID, false), TEX_SLOT_FONT);
    }

    #[test]
    fn every_other_texture_maps_to_slot_one() {
        for id in [
            egui::TextureId::Managed(1),
            egui::TextureId::Managed(7),
            egui::TextureId::User(0),
        ] {
            assert_eq!(texture_slot(id, false), TEX_SLOT_USER, "{id:?}");
        }
    }

    #[test]
    fn forced_font_slot_collapses_everything_to_zero() {
        // A/B 反证的 CPU 侧等价物：模拟 texture_slot() 恒返回 0。
        for id in [FONT_TEXTURE_ID, egui::TextureId::Managed(3)] {
            assert_eq!(texture_slot(id, true), TEX_SLOT_FONT);
        }
    }

    // ---- egui 真实路径 ---------------------------------------------------

    #[test]
    fn real_egui_image_produces_a_user_texture() {
        let (len, seen, _v) = observe_frame(false);
        assert!(seen, "ui.image 应产出非字体纹理");
        assert!(len >= 2, "应有字体图集 + 用户纹理两条增量，实得 {len}");
    }

    #[test]
    fn mixed_ui_produces_both_slots() {
        // 覆盖「一次绘制里混两种纹理」——正是 tex_id 方案唯一要解决的场景。
        let (_len, _seen, v) = observe_frame(false);
        assert!(v.iter().any(|x| x.tex_id == TEX_SLOT_FONT), "应含字体顶点");
        assert!(v.iter().any(|x| x.tex_id == TEX_SLOT_USER), "应含图片顶点");
    }

    #[test]
    fn forced_slot_zero_removes_every_user_vertex() {
        // A/B 的 CPU 侧：B 组不应残留任何 tex_id = 1 的顶点。
        let (_len, _seen, v) = observe_frame(true);
        assert!(!v.is_empty(), "顶点不应为空，否则 A/B 会因几何缺失而无意义");
        assert!(
            v.iter().all(|x| x.tex_id == TEX_SLOT_FONT),
            "强制槽位 0 后不应残留 tex_id = 1"
        );
    }

    #[test]
    fn text_only_ui_stays_on_the_font_slot() {
        // 回归：若 texture_slot 反了（字体判成 1），字体图集会被当彩色图采样
        // ⇒ 所有文字消失，且不产生任何 GPU 错误。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("Title");
            ui.label("body text");
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let (v, _i, _b) = build_vertices(&prims, false);
        assert!(!v.is_empty(), "egui 应产出顶点");
        assert!(v.iter().all(|x| x.tex_id == TEX_SLOT_FONT));
    }

    // ---- 已发现的断路：字节数校验 ---------------------------------------

    #[test]
    fn rgba_byte_count_can_never_satisfy_the_r8_check() {
        // upload() 的校验是 bytes.len() == staging_size_r8(w,h) == w*h，
        // 而用户纹理传 4*w*h。这条让「校验拒绝 RGBA」不依赖实机即可复现。
        for (w, h) in [(1u32, 1u32), (8, 8), (256, 256), (1920, 1080)] {
            let expected = modular_clipboard_gfx::texture::staging_size_r8(w, h);
            let actual = 4 * w as u64 * h as u64;
            assert_ne!(
                expected as u64, actual,
                "{w}x{h}: 若两者相等，本测试的前提（校验拒绝 RGBA）已不成立，需重新评估"
            );
        }
    }

    #[test]
    fn one_by_one_white_placeholder_also_fails_the_check() {
        // 比上一条更严重：它意味着 Painter::paint 的**首帧**就会失败。
        let expected = modular_clipboard_gfx::texture::staging_size_r8(1, 1);
        assert_ne!(expected as u64, 4, "1×1 占位纹理传 4 字节、校验要 1 字节");
    }

    #[test]
    fn our_r8_upload_bytes_do_match_the_check() {
        // 反向对照：合成字体图集的字节数确实等于 w*h ⇒ R8 路径能过校验。
        // 没有这条，就无法证明「R8 通、RGBA 不通」是字节数造成的。
        let cov = left_dark_right_white(FONT_TEX);
        let expected = modular_clipboard_gfx::texture::staging_size_r8(FONT_TEX, FONT_TEX);
        assert_eq!(cov.len() as u64, expected);
    }

    // ---- A/B 判据本身 ----------------------------------------------------

    #[test]
    fn grayscale_pixels_are_never_counted_as_thumbnail() {
        // A/B 可信的根本：字体图集是单通道覆盖率图，采样结果必然
        // r == g == b，因此无论阈值多少都不该被计入。
        for v in [0u8, 1, 60, 128, 200, 255] {
            assert_eq!(count_thumbnail_like(&[v, v, v, 255]), 0, "灰度 {v} 被误判");
        }
    }

    #[test]
    fn saturated_red_and_green_are_counted() {
        assert_eq!(count_thumbnail_like(&[255, 0, 0, 255]), 1);
        assert_eq!(count_thumbnail_like(&[0, 255, 0, 255]), 1);
    }

    /// 清屏蓝**必须不被计入**。
    ///
    /// 这条是判据设计的核心回归：最初的判据用「饱和度」，
    /// 而纯蓝的饱和度是 255，会被计入 —— 导致「B 组计数为 0」这条
    /// 断言在**实现完全正确时也会失败**（B 组左半正是透出清屏蓝）。
    ///
    /// 改成「红/绿主导」后，蓝的 `b` 是最大通道，被正确排除。
    #[test]
    fn pure_blue_clear_color_is_not_counted_as_thumbnail() {
        assert_eq!(count_thumbnail_like(&[0, 0, 255, 255]), 0);
        // 深蓝、浅蓝同样排除（b 始终是最大通道）
        assert_eq!(count_thumbnail_like(&[0, 0, 128, 255]), 0);
        assert_eq!(count_thumbnail_like(&[10, 10, 200, 255]), 0);
    }

    /// 反证的可执行版本：A 的像素与 B 的像素喂给判据，结论必须翻转。
    ///
    /// 直接对应「A/B 必须有差异」这个核心判据。
    /// 若哪天 `count_thumbnail_like` 变成恒真或恒假，本测试立刻失败。
    #[test]
    fn thumbnail_result_and_coverage_result_are_distinguishable() {
        let n = (TARGET * TARGET) as usize;
        // A：缩略图铺满，红绿相间（alpha 全 255 ⇒ 混合后原样透出）
        let a: Vec<u8> = (0..n)
            .flat_map(|i| {
                if i % 2 == 0 {
                    [255u8, 0, 0, 255]
                } else {
                    [0, 255, 0, 255]
                }
            })
            .collect();
        // B：字体图集采样结果，左半蓝（零覆盖⇒透出清屏）、右半白（全覆盖）
        let b: Vec<u8> = (0..n)
            .flat_map(|i| {
                if i % (TARGET as usize) < TARGET as usize / 2 {
                    [0u8, 0, 255, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();

        let ca = count_thumbnail_like(&a);
        let cb = count_thumbnail_like(&b);
        assert_eq!(ca, n, "A 组每个像素都应是红或绿");
        assert_eq!(cb, 0, "B 组一个红/绿像素都不该有");
        assert_ne!(ca, cb, "A/B 必须有差异，否则探针测不出 tex_id 是否生效");
    }

    /// 主色分类必须把 A/B 分到不同桶。
    #[test]
    fn dominant_classification_separates_a_and_b() {
        let n = (TARGET * TARGET) as usize;
        let a: Vec<u8> = (0..n)
            .flat_map(|i| {
                if i % 2 == 0 {
                    [255u8, 0, 0, 255]
                } else {
                    [0, 255, 0, 255]
                }
            })
            .collect();
        // B：左半蓝（字体图集空白）、右半白（字体图集全覆盖）
        let b: Vec<u8> = (0..n)
            .flat_map(|i| {
                if i % (TARGET as usize) < TARGET as usize / 2 {
                    [0u8, 0, 255, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        assert_eq!(classify_dominant(&a), "红绿主导（缩略图）");
        assert_eq!(
            classify_dominant(&b),
            "蓝白各半（字体图集左右分区）"
        );
        assert_ne!(classify_dominant(&a), classify_dominant(&b));
    }

    /// 若缩略图采样失败（退化到采样字体图集），A 组也必须被判为 0。
    ///
    /// 这条模拟「`tex_id` 机制坏了」的真实后果：
    /// A、B 都变成字体图集的灰阶 ⇒ A 的红/绿计数掉到 0
    /// ⇒ 主判据 `a_warm > 0` 失败 ⇒ 探针报错。
    #[test]
    fn broken_tex_id_mechanism_would_be_detected() {
        // 「坏掉」的 A：与 B 完全相同的像素（都退化成字体图集灰阶）
        let broken_a: Vec<u8> = (0..(TARGET * TARGET) as usize)
            .flat_map(|i| {
                if i % (TARGET as usize) < TARGET as usize / 2 {
                    [0u8, 0, 255, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
            .collect();
        // 主判据依赖「A 有红/绿像素」，坏掉时为 0 ⇒ 一定会被抓到。
        assert_eq!(count_thumbnail_like(&broken_a), 0);
    }

    // ---- 测试图自身 -------------------------------------------------------

    #[test]
    fn checkerboard_contains_both_red_and_green() {
        // 若测试图本身没有颜色，A/B 对照就失去区分能力。
        let img = checkerboard(THUMB);
        assert_eq!(img.size, [THUMB as usize, THUMB as usize]);
        let has_red = img
            .pixels
            .iter()
            .any(|p| p.r() > 200 && p.g() < 60 && p.b() < 60);
        let has_green = img
            .pixels
            .iter()
            .any(|p| p.g() > 200 && p.r() < 60 && p.b() < 60);
        assert!(has_red, "测试图缺红色格");
        assert!(has_green, "测试图缺绿色格");
        // 不能有蓝色格：蓝是清屏色/字体空白区的标识，
        // 混进来会让读回判读变模糊。
        let has_blue = img
            .pixels
            .iter()
            .any(|p| p.b() > 200 && p.r() < 60 && p.g() < 60);
        assert!(!has_blue, "测试图不应含蓝色格（与清屏色撞车）");
    }

    #[test]
    fn checkerboard_is_fully_opaque() {
        // 半透明会让混合结果依赖清屏色，A/B 的颜色就不再是纯棋盘格。
        let img = checkerboard(THUMB);
        assert!(img.pixels.iter().all(|p| p.a() == 255));
    }

    #[test]
    fn synthetic_font_atlas_is_half_dark_half_white() {
        let cov = left_dark_right_white(FONT_TEX);
        assert_eq!(cov.len(), (FONT_TEX * FONT_TEX) as usize);
        for y in 0..FONT_TEX {
            for x in 0..FONT_TEX {
                let v = cov[(y * FONT_TEX + x) as usize];
                if x < FONT_TEX / 2 {
                    assert_eq!(v, 0, "({x},{y}) 应为 0 覆盖");
                } else {
                    assert_eq!(v, 255, "({x},{y}) 应为 255 覆盖");
                }
            }
        }
    }

    #[test]
    fn quad_covers_the_whole_target() {
        // 四角必须正好落在靶面边界上，否则会漏采样边缘。
        let v = quad_vertices(TEX_SLOT_USER);
        assert_eq!(v.len(), QUAD_VERTS);
        for vert in &v {
            assert_eq!(vert.tex_id, TEX_SLOT_USER);
            assert!((0.0..=TARGET as f32).contains(&vert.pos[0]));
            assert!((0.0..=TARGET as f32).contains(&vert.pos[1]));
            assert!((0.0..=1.0).contains(&vert.uv[0]));
            assert!((0.0..=1.0).contains(&vert.uv[1]));
        }
        assert_eq!(v[0].pos, [0.0, 0.0]);
        assert_eq!(v[2].pos, [TARGET as f32, TARGET as f32]);
    }

    #[test]
    fn quad_indices_cover_all_four_vertices() {
        assert_eq!(QUAD_INDICES.len(), 6);
        let mut sorted = QUAD_INDICES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
    }

    #[test]
    fn sample_points_are_inside_the_buffer() {
        // 读回后按这些坐标取样，坐标越界会 panic 掩盖真正的结论。
        let px = vec![0u8; (TARGET * TARGET * 4) as usize];
        let s = sample_points(&px);
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn vertex_layout_is_24_bytes() {
        // 与 pipeline.rs 的步长一致。漂移时驱动不报错，只画乱码。
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
    }

    #[test]
    fn vertex_layout_offsets_match_pipeline_attributes() {
        let v = Vertex {
            pos: [1.0, 2.0],
            uv: [0.5, 0.25],
            color: 0xFF00_00FF,
            tex_id: TEX_SLOT_USER,
        };
        let base = &v as *const Vertex as usize;
        assert_eq!(&v.pos as *const _ as usize - base, 0);
        assert_eq!(&v.uv as *const _ as usize - base, 8);
        assert_eq!(&v.color as *const _ as usize - base, 16);
        assert_eq!(&v.tex_id as *const _ as usize - base, 20);
    }
}