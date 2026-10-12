//! 自研 GPU 模糊的端到端验证探针。
//!
//! # 它验证什么
//!
//! 单测只能覆盖**纯逻辑**（高斯权重、降采样尺寸、uniform 布局）。
//! 「三趟卷积真的把高频抹掉了」这件事只有跑一次真实 Vulkan 才能确认。
//! 本探针因此做一次完整闭环：
//!
//! ```text
//! 灌入黑白棋盘（极高空间频率）
//!   → 上传到离屏场景图
//!   → 降采样 + 横向 + 纵向卷积
//!   → 读回低分辨率结果
//!   → 比较「读回结果」与「原始棋盘」的像素方差
//! ```
//!
//! 棋盘在 1/4 降采样后每个 4×4 块几乎都是灰，方差应当**大幅下降**。
//! 若布局屏障写错、采样器写错、方向传错，方差都不会按预期下降——
//! 这类错误在纯逻辑单测里全都测不到。
//!
//! # 运行
//!
//! ```bash
//! ./scripts/build.sh --example blur_probe
//! target/x86_64-pc-windows-msvc/debug/examples/blur_probe.exe
//! ```
//!
//! 探针跑完即退出（不进事件循环），退出码非 0 表示验证失败。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::blur::{BlurChain, BlurPipeline, DEFAULT_DIVISOR};
use modular_clipboard_gfx::buffer::Buffer;
use modular_clipboard_gfx::frame::StagingArena;
use modular_clipboard_gfx::pipeline::RenderPass;
use modular_clipboard_gfx::window::Window;

/// 场景图尺寸（物理像素）。
const W: u32 = 512;
/// 场景图尺寸（物理像素）。
const H: u32 = 384;
/// 离屏格式。与交换链同族即可——探针不建交换链，直接指定。
const FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

fn main() -> anyhow::Result<()> {
    let window = Window::new("blur_probe", W, H)?;
    let gpu = Gpu::new("blur_probe", window.hinstance(), window.hwnd())?;
    println!("[probe] Vulkan 设备就绪");

    let render_pass = RenderPass::new(&gpu.device, FORMAT)?;
    let extent = vk::Extent2D {
        width: W,
        height: H,
    };

    let mut chain = BlurChain::new(&gpu, render_pass.handle, FORMAT, extent, DEFAULT_DIVISOR)?;
    let mut pipeline = BlurPipeline::new(&gpu, render_pass.handle)?;
    pipeline.configure(&gpu, &chain)?;
    println!(
        "[probe] 模糊链就绪：scene {}x{} → blur {}x{}（1/{}）",
        chain.scene.extent.width,
        chain.scene.extent.height,
        chain.blur_a.extent.width,
        chain.blur_a.extent.height,
        chain.divisor
    );

    // ---- 命令池与命令缓冲 ------------------------------------------------
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { gpu.device.create_command_pool(&pool_info, None)? };
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd = unsafe { gpu.device.allocate_command_buffers(&alloc)? }[0];

    // ---- 测试图案：1px 黑白棋盘 -----------------------------------------
    let pattern = checkerboard(W, H);
    let src_high_freq = adjacent_diff(&pattern, W, H);

    let mut arena = StagingArena::new(&gpu, 1)?;
    arena.begin_frame();
    let slice = arena.allocate(pattern.len() as vk::DeviceSize)?;
    slice.write(&gpu.device, &pattern)?;

    // ---- 读回缓冲（低分辨率结果，BGRA 每像素 4 字节）--------------------
    let low = chain.blur_a.extent;
    let readback_bytes = (low.width as vk::DeviceSize) * (low.height as vk::DeviceSize) * 4;
    let mut readback = Buffer::new_host_visible(
        &gpu,
        readback_bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;

    // ---- 录制 ------------------------------------------------------------
    let begin = vk::CommandBufferBeginInfo::default()
        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { gpu.device.begin_command_buffer(cmd, &begin)? };

    // ① 图案 → 场景图（上传内部会把布局转到 SHADER_READ_ONLY）。
    chain
        .scene
        .image
        .upload(&gpu, cmd, slice, &pattern, None, (W, H))?;
    // ② 降采样 + 横向 + 纵向。
    pipeline.record(&gpu, cmd, &mut chain, render_pass.handle)?;
    // ③ 读回低分辨率结果。
    readback_low_res(&gpu, cmd, &mut chain, &readback)?;

    unsafe { gpu.device.end_command_buffer(cmd)? };

    // ---- 提交并等待 ------------------------------------------------------
    let fence = unsafe { gpu.device.create_fence(&vk::FenceCreateInfo::default(), None)? };
    let cmds = [cmd];
    let submit = vk::SubmitInfo::default().command_buffers(&cmds);
    unsafe {
        gpu.device.queue_submit(gpu.queue, &[submit], fence)?;
        gpu.device.wait_for_fences(&[fence], true, u64::MAX)?;
        gpu.device.destroy_fence(fence, None);
    }

    // ---- 读回并判定 ------------------------------------------------------
    let ptr = readback.map(&gpu.device)?;
    let out = unsafe { std::slice::from_raw_parts(ptr, readback_bytes as usize) }.to_vec();
    readback.unmap(&gpu.device)?;
    let out_high_freq = adjacent_diff(&out, low.width, low.height);

    println!("[probe] 原始棋盘相邻像素差 = {src_high_freq:.2} / 255");
    println!("[probe] 模糊结果相邻像素差 = {out_high_freq:.2} / 255");
    println!(
        "[probe] 高频衰减 = {:.1}%",
        (1.0 - out_high_freq / src_high_freq.max(1e-9)) * 100.0
    );

    // 判定：模糊必须显著抹掉相邻像素差。
    //
    // 棋盘在 1/4 降采样（线性采样恰好落在纹素边界、四邻取平均）后
    // 应当变成均匀灰，相邻像素差趋近 0。阈值取 5%——远高于「模糊完全
    // 没生效」时的 100%，同时给驱动实现差异留足余量。
    let ok = out_high_freq < src_high_freq * 0.05;

    // ---- 清理 ------------------------------------------------------------
    arena.retire();
    arena.destroy();
    readback.destroy(&gpu.device);
    chain.destroy(&gpu.device);
    pipeline.destroy(&gpu);
    unsafe {
        gpu.device.destroy_command_pool(cmd_pool, None);
    }
    render_pass.destroy(&gpu.device);

    if ok {
        println!("[probe] PASS：GPU 模糊确实抹掉了高频");
        Ok(())
    } else {
        anyhow::bail!(
            "FAIL：相邻像素差未按预期下降（{out_high_freq:.2} vs 阈值 {:.2}）——\
             模糊可能完全没生效（检查布局屏障 / 采样器 / 方向 uniform）",
            src_high_freq * 0.05
        )
    }
}

/// 生成 1px 黑白棋盘（BGRA）。
fn checkerboard(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let on = (x + y) % 2 == 0;
            let c = if on { 255u8 } else { 0u8 };
            v.extend_from_slice(&[c, c, c, 255]);
        }
    }
    v
}

/// 把 `blur_a` 读回主机缓冲。
fn readback_low_res(
    gpu: &Gpu,
    cmd: vk::CommandBuffer,
    chain: &mut BlurChain,
    dst: &Buffer,
) -> anyhow::Result<()> {
    let low = chain.blur_a.extent;
    // 卷积结束时 blur_a 停在 SHADER_READ_ONLY，转成拷贝源。
    chain.blur_a.image.transition_to(
        cmd,
        &gpu.device,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::AccessFlags::SHADER_READ,
        vk::AccessFlags::TRANSFER_READ,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
        vk::PipelineStageFlags::TRANSFER,
    );
    let copy = vk::BufferImageCopy::default()
        .buffer_offset(0)
        // 0 = 紧密排列（无行对齐填充）
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
            width: low.width,
            height: low.height,
            depth: 1,
        });
    unsafe {
        gpu.device.cmd_copy_image_to_buffer(
            cmd,
            chain.blur_a.image.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            dst.handle(),
            &[copy],
        );
    }
    Ok(())
}

/// 相邻像素差的平均值（只看 RGB，跳过 alpha）。
///
/// # 为什么不用方差
///
/// 方差会把 alpha 通道一起算进去。棋盘模糊后 RGB 变成均匀灰 127.5，
/// 而 alpha 恒为 255——「RGB 的均值」与「alpha = 255」之间的差会凭空
/// 贡献约 3000 的方差，让人误判成「模糊没生效」。实测正是踩了这个坑
/// （第一版探针因此误报 FAIL）。
///
/// 相邻像素差直接度量**高频**：棋盘的每一对相邻像素都差 255，
/// 均匀灰则差 0。它不受常量通道干扰，判据干净且与「模糊」的语义直接对应。
fn adjacent_diff(bytes: &[u8], w: u32, h: u32) -> f64 {
    if w < 2 || h == 0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    let mut n = 0u64;
    for y in 0..h {
        for x in 0..(w - 1) {
            let i = ((y * w + x) * 4) as usize;
            let j = ((y * w + x + 1) * 4) as usize;
            // 只看 B/G/R 三个颜色通道（BGRA 布局，alpha 在 +3）。
            for c in 0..3 {
                sum += (bytes[i + c] as f64 - bytes[j + c] as f64).abs();
                n += 1;
            }
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}
