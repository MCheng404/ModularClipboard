//! 主窗口「自研磨砂」端到端探针：精确复刻 `ui/src/renderer.rs` 的
//! `ensure_backdrop` + `record_backdrop_blur` 路径——
//!
//! ```text
//! 捕获图(1/2，alpha=255) 上传
//!   → bind_source(SET_DOWNSAMPLE_BACKDROP)
//!   → record_from(backdrop_set)：降采样 + 横向 + 纵向卷积
//!   → composite(SET_COMPOSITE)：把模糊结果铺满全屏目标
//!   → 读回全屏目标，判定它不透明且确实含捕获内容
//! ```
//!
//! # 为什么 `blur_probe` 覆盖不到这个 bug
//!
//! `blur_probe` 走的是 `pipeline.record()`（降采样源是链内 `scene`），
//! 且它自己在开头就调了 `pipeline.configure(&chain)`。它**从不**使用
//! `backdrop_set`，也从不调用 `composite()`。而主窗口路径的根因是
//! `ensure_backdrop` 漏了 `configure()`、只调了 `bind_source()`——于是
//! 合成用的 `SET_COMPOSITE`（以及横向/纵向两组）描述符从未被写入，
//! 采样到未绑定的空纹理，最终铺满全屏的是透明/黑：只有透明、没有模糊。
//!
//! # 复现修复前
//!
//! 设置 `MC_PROBE_BUG=1` 时探针**故意不调 `configure()`**，只 `bind_source`
//! （即修复前的调用顺序）。此时 `composite` 读空，读回底图全黑/全透明，
//! 探针必须 FAIL；去掉该变量（修复后）必须 PASS。这样本探针能长期守护
//! 「绑定遗漏」这类静默失效。
//!
//! 运行：`cargo run -p modular-clipboard-gfx --example backdrop_probe`，
//! 退出码非 0 表示失败。

use ash::vk;
use modular_clipboard_gfx::backdrop::capture_size;
use modular_clipboard_gfx::blur::{BlurChain, BlurPipeline, DEFAULT_DIVISOR};
use modular_clipboard_gfx::buffer::Buffer;
use modular_clipboard_gfx::frame::StagingArena;
use modular_clipboard_gfx::pipeline::RenderPass;
use modular_clipboard_gfx::texture::DeviceImage;
use modular_clipboard_gfx::window::Window;
use modular_clipboard_gfx::Gpu;

/// 全屏目标尺寸（模拟交换链客户区，物理像素）。
const W: u32 = 512;
const H: u32 = 384;
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

fn main() -> anyhow::Result<()> {
    let bug = std::env::var_os("MC_PROBE_BUG").is_some();
    println!("[probe] 模式：{}", if bug {
        "MC_PROBE_BUG=1（复现修复前：只 bind_source，不 configure）"
    } else {
        "修复后：configure + bind_source"
    });

    let window = Window::new("backdrop_probe", W, H)?;
    let gpu = Gpu::new("backdrop_probe", window.hinstance(), window.hwnd())?;
    let render_pass = RenderPass::new(&gpu.device, FORMAT)?;
    let full = vk::Extent2D { width: W, height: H };

    // 与 ensure_backdrop 相同的资源创建顺序。
    let mut chain = BlurChain::new(&gpu, render_pass.handle, FORMAT, full, DEFAULT_DIVISOR)?;
    let mut pipeline = BlurPipeline::new(&gpu, render_pass.handle)?;
    let (cw, ch) = capture_size(W, H);
    let mut capture_img = DeviceImage::new(
        &gpu,
        vk::Extent2D { width: cw, height: ch },
        FORMAT,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )?;

    // 关键分歧点：修复前漏 configure。
    if !bug {
        pipeline.configure(&gpu, &chain)?;
    }
    pipeline.bind_source(&gpu, capture_img.view)?;

    // ---- 命令池 / 缓冲 ----
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { gpu.device.create_command_pool(&pool_info, None)? };
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd = unsafe { gpu.device.allocate_command_buffers(&alloc)? }[0];

    // ---- 捕获图内容：左半红、右半蓝（BGRA，alpha=255）----
    // 左右分色是为了绕开 y 翻转（全屏三角形在 shader 里翻转 v，会影响
    // 上下但不影响左右）；只要 composite 真的把捕获内容画上来，就一定
    // 能看到左红右蓝。
    let pattern = split_red_blue(cw, ch);
    let mut arena = StagingArena::new(&gpu, 1)?;
    arena.begin_frame();
    let slice = arena.allocate(pattern.len() as vk::DeviceSize)?;
    slice.write(&gpu.device, &pattern)?;

    // ---- 全屏目标读回缓冲（最终 composite 的结果）----
    let out_bytes = (W as vk::DeviceSize) * (H as vk::DeviceSize) * 4;
    let mut readback = Buffer::new_host_visible(
        &gpu,
        out_bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { gpu.device.begin_command_buffer(cmd, &begin_info)? };

    // ① 捕获内容 → capture_img（上传结束即 SHADER_READ_ONLY）。
    capture_img.upload(&gpu, cmd, slice, &pattern, None, (cw, ch))?;

    // ② 主窗口路径：backdrop_set 降采样 → 横向 → 纵向。
    //    结束后 blur_a 停在 SHADER_READ_ONLY，供 composite 采样。
    pipeline.record_from(&gpu, cmd, &mut chain, render_pass.handle, pipeline.backdrop_set())?;

    // ③ composite 铺到全屏目标（scene）。先把未碰过的 scene 转成颜色附件。
    chain.scene.image.transition_to(
        cmd,
        &gpu.device,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
    );
    // 透明清屏（alpha=0）：若 composite 没画出任何不透明像素，读回保持
    // 透明——这正是「只有透明」的直接判据。
    let clear = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: [0.0, 0.0, 0.0, 0.0],
        },
    };
    let begin = vk::RenderPassBeginInfo::default()
        .render_pass(render_pass.handle)
        .framebuffer(chain.scene.framebuffer)
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: full,
        })
        .clear_values(std::slice::from_ref(&clear));
    let blit = pipeline.composite([0.0, 0.0]);
    let viewport = vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: W as f32,
        height: H as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    };
    let scissor = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: full,
    };
    let sets = [blit.descriptor_set];
    unsafe {
        gpu.device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
        gpu.device.cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
        gpu.device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&scissor));
        gpu.device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, blit.pipeline);
        gpu.device.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::GRAPHICS,
            blit.layout,
            0,
            &sets,
            &[],
        );
        // 合成拷贝的 UV 位移 push constant：探针不走磨砂路径，恒推零。
        // 着色器声明了这块常量，不推 = 未定义内容 = 底图随机平移。
        let zero_offset = [0.0f32, 0.0];
        gpu.device.cmd_push_constants(
            cmd,
            blit.layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            bytemuck::bytes_of(&zero_offset),
        );
        gpu.device.cmd_draw(cmd, 3, 1, 0, 0);
        gpu.device.cmd_end_render_pass(cmd);
    }

    // ④ scene → 读回缓冲。
    chain.scene.image.transition_to(
        cmd,
        &gpu.device,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        vk::AccessFlags::TRANSFER_READ,
        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        vk::PipelineStageFlags::TRANSFER,
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
            width: W,
            height: H,
            depth: 1,
        });
    unsafe {
        gpu.device.cmd_copy_image_to_buffer(
            cmd,
            chain.scene.image.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.handle(),
            std::slice::from_ref(&copy),
        );
        gpu.device.end_command_buffer(cmd)?;
    }

    let fence = unsafe { gpu.device.create_fence(&vk::FenceCreateInfo::default(), None)? };
    unsafe {
        gpu.device.queue_submit(gpu.queue, &[vk::SubmitInfo::default().command_buffers(&[cmd])], fence)?;
        gpu.device.wait_for_fences(&[fence], true, u64::MAX)?;
        gpu.device.destroy_fence(fence, None);
    }

    let ptr = readback.map(&gpu.device)?;
    let out = unsafe { std::slice::from_raw_parts(ptr, out_bytes as usize) }.to_vec();
    readback.unmap(&gpu.device)?;

    let stats = region_stats(&out, W, H);
    println!(
        "[probe] 全屏底图：平均 alpha={:.0}/255",
        stats.alpha
    );
    println!(
        "[probe] 左半：R={:.0} B={:.0}（应偏红，R 显著>B）",
        stats.left_r, stats.left_b
    );
    println!(
        "[probe] 右半：R={:.0} B={:.0}（应偏蓝，B 显著>R）",
        stats.right_r, stats.right_b
    );

    // 判定：composite 必须画出不透明、且保留左红右蓝的捕获内容。
    let opaque = stats.alpha > 240.0;
    let left_red = stats.left_r > 120.0 && stats.left_r - stats.left_b > 60.0;
    let right_blue = stats.right_b > 120.0 && stats.right_b - stats.right_r > 60.0;
    let ok = opaque && left_red && right_blue;

    arena.retire();
    arena.destroy();
    readback.destroy(&gpu.device);
    capture_img.destroy(&gpu.device);
    chain.destroy(&gpu.device);
    pipeline.destroy(&gpu);
    unsafe { gpu.device.destroy_command_pool(cmd_pool, None) };
    render_pass.destroy(&gpu.device);

    if ok {
        println!("[probe] PASS：composite 铺满了不透明、且含捕获内容的模糊底图");
        Ok(())
    } else {
        let mut why = Vec::new();
        if !opaque {
            why.push("底图不透明（alpha 过低）——composite 没画出不透明像素，只透明");
        }
        if !left_red {
            why.push("左半不红");
        }
        if !right_blue {
            why.push("右半不蓝");
        }
        anyhow::bail!("FAIL：{}（模糊底图未正确合成，疑似描述符未绑定）", why.join("；"))
    }
}

/// 左半红、右半蓝的 BGRA 图（alpha 恒 255）。
fn split_red_blue(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let _ = y;
            if x < w / 2 {
                // BGRA 红：B=0 G=0 R=255
                v.extend_from_slice(&[0, 0, 255, 255]);
            } else {
                // BGRA 蓝：B=255 G=0 R=0
                v.extend_from_slice(&[255, 0, 0, 255]);
            }
        }
    }
    v
}

#[derive(Debug)]
struct Stats {
    alpha: f64,
    left_r: f64,
    left_b: f64,
    right_r: f64,
    right_b: f64,
}

/// 统计全屏底图的平均 alpha，以及左右两半的平均 R/B（BGRA：B=0 R=2 A=3）。
fn region_stats(bytes: &[u8], w: u32, h: u32) -> Stats {
    let mut a_sum = 0.0f64;
    let (mut lr, mut lb, mut rr, mut rb) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut ln, mut rn) = (0u64, 0u64);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            a_sum += bytes[i + 3] as f64;
            if x < w / 2 {
                lr += bytes[i + 2] as f64;
                lb += bytes[i] as f64;
                ln += 1;
            } else {
                rr += bytes[i + 2] as f64;
                rb += bytes[i] as f64;
                rn += 1;
            }
        }
    }
    let n = (w * h) as f64;
    Stats {
        alpha: a_sum / n,
        left_r: lr / ln.max(1) as f64,
        left_b: lb / ln.max(1) as f64,
        right_r: rr / rn.max(1) as f64,
        right_b: rb / rn.max(1) as f64,
    }
}
