//! 实机验证纹理上传全链路：`cmd_copy_buffer_to_image` + 布局过渡 +
//! staging arena 的跨帧退休/复用。
//!
//! # 为什么这个探针不可省略
//!
//! 纹理上传路径**只有单测覆盖，而单测不跑 GPU**。因此在这条路径被真正
//! 执行过一次之前，谁也不能断言它是可用的——包括驱动层面的格式支持、
//! 屏障配对、staging 内存的跨帧存活。
//!
//! # 验证的四个层次
//!
//! 1. **不崩、不报 GPU 错误**：`device_wait_idle` 之后无错。
//! 2. **局部更新真的落在正确位置**：每帧上传**不同区域**（模拟 egui 的
//!    [`egui::epaint::ImageDelta`] 增量），而不是每帧都整图覆盖。
//! 3. **arena 的退休/复用是对的**：跑满 60 帧，让多块 staging 轮转
//!    占用与退休。只跑一帧测不出「过早复用导致 GPU 读到上一帧数据」。
//! 4. **像素逐字节正确**：把纹理读回 CPU，与 CPU 侧的影子缓冲比对。
//!    use-after-free 的典型症状是**静默读到错误数据**，驱动不报错，
//!    只看「无 GPU 错误」会漏掉。
//!
//! # 硬性约束：禁止 fallback
//!
//! 找不到硬件 GPU 就**直接失败退出**。软件渲染跑通只能证明「代码路径
//! 是对的」，不能证明「用户机器上驱动是正常的」——这是两件事，
//! 混为一谈就是自欺欺人。因此本探针在 `device_type == CPU` 时拒绝运行。

use ash::vk;
use std::time::Instant;
use tiez_gfx::Gpu;
use tiez_gfx::frame::{PipelineBundle, PresentResult};
use tiez_gfx::texture::{DeviceImage, FONT_FORMAT};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 窗口尺寸。
const W: i32 = 640;
const H: i32 = 480;

/// 纹理边长。取 256 使整图恰好 64 KiB = `MIN_STAGING_CAPACITY`，
/// 从而「整图上传」与「多次局部上传」落在同一个容量档位上。
const TEX: u32 = 256;

/// 上传帧数。必须远超在飞帧数（3），否则测不到 arena 的退休/复用。
const FRAMES: u32 = 60;

/// 进度打印间隔。
const PROGRESS_EVERY: u32 = 10;

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL upload_probe: {e:#}");
        std::process::exit(1);
    }
    println!("ALL OK - 纹理上传全链路验证通过（{FRAMES} 帧局部更新，像素逐字节一致）");
}

fn run() -> anyhow::Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap();
    let class_name = windows::core::HSTRING::from("TiezUploadProbeClass");
    let class_p = windows::core::PCWSTR(class_name.as_ptr());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_p,
        ..Default::default()
    };
    unsafe {
        if RegisterClassExW(&wc) == 0 {
            anyhow::bail!("RegisterClassExW 失败");
        }
    }
    println!("OK RegisterClassExW");

    let title = windows::core::HSTRING::from("Tiez Upload Probe");
    let title_p = windows::core::PCWSTR(title.as_ptr());
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_p,
            title_p,
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            W,
            H,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }?;
    println!("OK CreateWindowExW");

    // 跑消息循环让窗口完成显示。Win32 要求窗口真正可见后表面才可用。
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        for _ in 0..100 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    println!("OK message loop pumped");

    let gpu = Gpu::new("ModularClipboardUploadProbe", hinstance.into(), hwnd.into())?;
    let device_name = device_name_of(&gpu);
    let kind = device_kind(gpu.device_type);
    println!(
        "GPU: {device_name} ({kind})  api={}.{}  queue_family={}  formats={}",
        vk::api_version_major(gpu.properties.api_version),
        vk::api_version_minor(gpu.properties.api_version),
        gpu.queue_family,
        gpu.surface_formats.len()
    );

    // 禁止 fallback：软件渲染跑通不能证明硬件驱动正常。
    // 软件光栅器（llvmpipe / lavapipe）的 device_type 报 CPU。
    anyhow::ensure!(
        gpu.device_type != vk::PhysicalDeviceType::CPU,
        "当前 Vulkan 设备是 CPU 软件光栅器（{device_name}）。\
         本探针拒绝在软件渲染上运行：它只能证明代码路径正确，\
         不能证明硬件驱动正常。请检查显卡驱动是否提供 Vulkan 支持。"
    );
    //虚拟 GPU 同理：它可能直通到宿主机或落在模拟层上。
    anyhow::ensure!(
        gpu.device_type != vk::PhysicalDeviceType::VIRTUAL_GPU,
        "当前 Vulkan 设备是虚拟 GPU（{device_name}），无法作为硬件驱动正常的证据。"
    );
    println!("OK 硬件 GPU 自检通过（{kind}）");

    // 渲染通道必须与交换链选中同一格式。用 `pick_format` 的同一套逻辑
    // 预选，`Swapchain::new` 必然命中同一结果。
    let probe_format = gpu
        .surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .ok_or_else(|| anyhow::anyhow!("表面无可用格式"))?;

    let rp = tiez_gfx::pipeline::RenderPass::new(&gpu.device, probe_format)?;
    let layout = tiez_gfx::pipeline::DescriptorLayout::new(&gpu.device)?;
    let pipe_layout = tiez_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&layout.handle),
    )?;
    let shader = tiez_gfx::shader::ShaderModule::new(&gpu.device)?;
    let pipeline = tiez_gfx::pipeline::GraphicsPipeline::new(
        &gpu.device,
        rp.handle,
        pipe_layout.handle,
        &shader,
    )?;
    let bundle = PipelineBundle::new(
        rp.handle,
        pipe_layout.handle,
        pipeline.handle,
        layout.handle,
    );
    println!("OK pipeline bundle (RenderPass+Layout+SPIR-V)");

    let mut fr = tiez_gfx::frame::FrameRenderer::new(&gpu, bundle)?;
    println!(
        "OK FrameRenderer  extent={}x{}  在飞槽位={}",
        fr.extent().width,
        fr.extent().height,
        fr.slot_count(),
    );

    let mut image = DeviceImage::new(
        &gpu,
        vk::Extent2D {
            width: TEX,
            height: TEX,
        },
        FONT_FORMAT,
        vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED
            // 读回校验需要。生产路径不需要，因此由本探针单独加上。
            | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    println!("OK DeviceImage {TEX}x{TEX} {FONT_FORMAT:?}（含 TRANSFER_SRC 供读回）");

    // 读回缓冲必须是**主机可见**的。`Buffer::new` 要求 DEVICE_LOCAL，
    // 在独显上那是不可映射的显存，`map_memory` 会直接失败。
    let tex_bytes = (TEX * TEX) as vk::DeviceSize;
    let mut readback = tiez_gfx::buffer::Buffer::new_host_visible(
        &gpu,
        tex_bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;
    println!("OK readback buffer（{tex_bytes} 字节，主机可见）");

    // CPU 侧影子缓冲：记录「GPU 上应该是什么样」。最终逐字节比对。
    let mut shadow = vec![0u8; (TEX * TEX) as usize];

    // ---- 逐帧上传 ----------------------------------------------------
    let t0 = Instant::now();
    let mut done = 0u32;
    let mut rebuilds = 0u32;
    let mut uploads = 0u64;
    let mut post_present_rejections = 0u32;

    while done < FRAMES {
        // acquire 返回 None = 交换链过期。重建后重试**同一帧号**，
        // 保证帧计数与上传计划一一对应。
        let Some(acquired) = fr.acquire()? else {
            fr.rebuild_swapchain(Default::default())?;
            rebuilds += 1;
            println!("  [警告] 第 {done} 帧 acquire 返回过期，已重建交换链，重试该帧");
            continue;
        };

        // 本帧的上传计划：位置与尺寸都随帧号变化。
        let plan = plan_patches(done, TEX);
        let mut pending_data = Vec::with_capacity(plan.len());
        for (slot_in_frame, &(x, y, w, h)) in plan.iter().enumerate() {
            let data = make_pattern(done, slot_in_frame as u32, w, h);
            fr.record_texture_upload(&mut image, &data, Some((x, y)), (w, h))?;
            pending_data.push((x, y, w, h, data));
            uploads += 1;
        }

        // 只清屏不绘制：本探针验证的是上传路径，不是光栅化。
        fr.record(&Default::default())?;

        // 全部录制成功后才推进影子缓冲——顺序反了会在中途失败时
        // 留下「影子说写了、GPU 说没写」的不一致状态。
        for (x, y, w, h, data) in &pending_data {
            apply_patch(&mut shadow, TEX, (*x, *y, *w, *h), data);
        }
        let res = fr.present(acquired)?;
        if res == PresentResult::Outdated {
            fr.rebuild_swapchain(Default::default())?;
            rebuilds += 1;
        }

        // 验证「重复录制」确实被挡住。此处**不是**验证 `in_flight`
        // 分支（原因见下方 print_in_flight_is_unreachable 的说明），
        // 而是验证 present 之后命令缓冲不再可写。
        if let Err(e) = fr.record_texture_upload(&mut image, &[0u8], Some((0, 0)), (1, 1)) {
            anyhow::ensure!(
                e.to_string().contains("acquire"),
                "present 之后的重复上传应因「未 acquire」被拒，实际：{e}"
            );
            post_present_rejections += 1;
        } else {
            anyhow::bail!("第 {done} 帧：present 之后竟然还能录制，命令缓冲保护失效");
        }

        done += 1;
        if done % PROGRESS_EVERY == 0 {
            let elapsed = t0.elapsed();
            println!(
                "  进度 {done}/{FRAMES} 帧  上传 {uploads} 次  重建 {rebuilds} 次  \
                 总耗时 {:.0}ms  本段平均 {:.2}ms/帧",
                elapsed.as_secs_f64() * 1000.0,
                elapsed.as_secs_f64() * 1000.0 / done as f64
            );
        }
    }
    println!(
        "OK {done} 帧 / {uploads} 次上传完成  \
         (present 后重复录制被正确拒绝 {post_present_rejections} 次，交换链重建 {rebuilds} 次)"
    );

    // `can_record_upload` 的 in_flight 分支在当前 API 下不可达：
    // `present` 会取走 `pending`，因此 present 之后调用
    // `record_texture_upload`会先撞上「必须在 acquire 之后」这道门。
    // 也就是说 in_flight 标记目前是一道**冗余**防线，而非唯一防线。
    // 这里直接调用纯逻辑函数，把该规则本身固定下来。
    print_in_flight_is_unreachable();

    // ---- 读回校验 ----------------------------------------------------
    //
    // 前面 60 帧的局部更新已经在纹理上拼出一幅确定的图案。
    // 把它读回 CPU 与影子缓冲逐字节比对——这是唯一能证明
    // 「GPU 读到并写入的是正确数据」的手段。
    let cmd_pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { gpu.device.create_command_pool(&cmd_pool_info, None) }?;
    let cmd_alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd = unsafe { gpu.device.allocate_command_buffers(&cmd_alloc)? }
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("未分配到命令缓冲"))?;
    println!("OK command pool + buffer（供读回使用）");

    unsafe {
        gpu.device
            .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
        gpu.device
            .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        // upload 的收尾已把布局推到 SHADER_READ_ONLY_OPTIMAL，
        // 读回需要 TRANSFER_SRC_OPTIMAL。
        let mut bar = tiez_gfx::image_barrier(
            image.image,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_READ,
        );
        gpu.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            std::slice::from_mut(&mut bar),
        );
        gpu.device.cmd_copy_image_to_buffer(
            cmd,
            image.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.handle(),
            &[full_copy(TEX)],
        );
        gpu.device.end_command_buffer(cmd)?;
    }
    let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
    unsafe { gpu.device.queue_submit(gpu.queue, std::slice::from_ref(&submit), vk::Fence::null()) }?;
    gpu.wait_idle();
    println!("OK 读回提交 + device_wait_idle  <-- 无 GPU 错误");

    readback.map(&gpu.device)?;
    let got: Vec<u8> = readback.read::<u8>()?;
    readback.unmap(&gpu.device)?;

    verify_pixels(&got, &shadow)?;

    // ---- 清理 --------------------------------------------------------
    //
    // 上面的 wait_idle 已经保证 GPU 不再引用任何对象，此刻销毁安全。
    let mut image = image;
    image.destroy(&gpu.device);
    let mut readback = readback;
    readback.destroy(&gpu.device);
    unsafe {
        gpu.device.destroy_command_pool(cmd_pool, None);
    }
    // FrameRenderer 的 Drop 内部会再 wait_idle 一次再销毁交换链与 staging。
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    layout.destroy(&gpu.device);
    rp.destroy(&gpu.device);
    println!("OK 资源全部销毁，无泄漏迹象");
    Ok(())
}

// ---------------------------------------------------------------------------
// GPU 自检
// ---------------------------------------------------------------------------

/// 从 [`vk::PhysicalDeviceProperties`] 取设备名。
///
/// `device_name` 是 `[c_char; 256]`（ash 0.38 下 `c_char = i8`），
/// 需逐字节 `as u8` 再在首个 NUL 处截断（坑 10）。
fn device_name_of(gpu: &Gpu) -> String {
    let bytes: Vec<u8> = gpu
        .properties
        .device_name
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 设备类型的中文标签。
fn device_kind(t: vk::PhysicalDeviceType) -> &'static str {
    match t {
        vk::PhysicalDeviceType::DISCRETE_GPU => "hardware",
        vk::PhysicalDeviceType::INTEGRATED_GPU => "hardware",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual GPU",
        vk::PhysicalDeviceType::CPU => "software rasterizer - 不能证明硬件驱动正常",
        _ => "其它",
    }
}

/// 把「`in_flight` 分支在当前 API 下不可达」这件事打印出来。
///
/// 不是为了报错，而是让读输出的人知道：**这里有一个观察到的结构问题，
/// 属于库代码（已冻结），报告在PROGRESS.md 的待决区。**
fn print_in_flight_is_unreachable() {
    match tiez_gfx::frame::can_record_upload(true, true) {
        Err(e) => println!("OK can_record_upload(in_flight=true) 正确拒绝：{e}"),
        Ok(()) => panic!("can_record_upload 未拦住 in_flight=true"),
    }
    // pending=false 时也必须拒绝。
    match tiez_gfx::frame::can_record_upload(false, false) {
        Err(e) => println!("OK can_record_upload(pending=false) 正确拒绝：{e}"),
        Ok(()) => panic!("can_record_upload 未拦住 pending=false"),
    }
}

// ---------------------------------------------------------------------------
// 上传计划（纯逻辑，可离线复核）
// ---------------------------------------------------------------------------

/// 线性同余发生器。
///
/// 用固定常量而非 `rand` 依赖：**同一个帧号必须永远产生同一块区域**，
/// 否则读回比对失败时无法区分「GPU 写错了」与「这次跑的区域不一样」。
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // 用大质数扩散，避免相邻种子产生相关序列。
        Lcg(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
    }

    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

/// 第 `frame` 帧要上传哪些矩形。
///
/// 第 0 帧是整图（对应 egui 图集首次创建时的 whole delta），
/// 其余帧是 1~3 块随机位置/随机尺寸的局部更新，且**强制轮流贴住
/// 四条边**——边界处的 `image_offset + image_extent` 越界是最容易
/// 被忽略的地方，只测中心区域永远测不到。
fn plan_patches(frame: u32, tex: u32) -> Vec<(u32, u32, u32, u32)> {
    if frame == 0 {
        return vec![(0, 0, tex, tex)];
    }
    let mut rng = Lcg::new(frame as u64 + 7);
    // 1~3 次上传/帧：验证同帧内arena 的「追加分配」路径。
    let count = 1 + rng.next() % 3;
    let mut out = Vec::with_capacity(count as usize);
    for k in 0..count {
        let w = 8 + rng.next() % 89; // 8..=96
        let h = 8 + rng.next() % 89;
        let max_x = tex - w;
        let max_y = tex - h;
        let mut x = rng.next() % (max_x + 1);
        let mut y = rng.next() % (max_y + 1);
        // 每帧让所有块贴同一条边，四帧轮完一圈。
        match (frame + k) % 4 {
            0 => x = 0,
            1 => x = max_x,
            2 => y = 0,
            _ => y = max_y,
        }
        out.push((x, y, w, h));
    }
    out
}

/// 生成 `w x h` 的覆盖率数据。
///
/// 值同时依赖帧号、帧内序号与行号，三个维度缺一不可：
/// - 依赖帧号 → 跨帧误用 staging 会立刻被读回比对抓到；
/// - 依赖行号 → 上下错位（`image_offset.y` 写错）会被抓到；
/// - 帧内序号 → 同帧多次上传互相覆盖会被抓到。
///
/// 模 251（质数）保证 60 帧内不因字节截断而撞值。
fn make_pattern(frame: u32, k: u32, w: u32, h: u32) -> Vec<u8> {
    let base = (frame * 131 + k * 17) % 251;
    (0..(w * h) as usize)
        .map(|i| {
            let row = (i / w as usize) as u32;
            ((base + row * 7) % 251) as u8
        })
        .collect()
}

/// 把一次局部更新写进 CPU 侧的影子缓冲。
fn apply_patch(shadow: &mut [u8], tex: u32, (x, y, w, h): (u32, u32, u32, u32), data: &[u8]) {
    for row in 0..h {
        let dst_start = ((y + row) * tex + x) as usize;
        let src_start = (row * w) as usize;
        shadow[dst_start..dst_start + w as usize]
            .copy_from_slice(&data[src_start..src_start + w as usize]);
    }
}

/// 构造覆盖整张纹理的 `BufferImageCopy`（紧密排列）。
fn full_copy(tex: u32) -> vk::BufferImageCopy {
    vk::BufferImageCopy::default()
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
            width: tex,
            height: tex,
            depth: 1,
        })
}

/// 逐字节比对读回结果与影子缓冲，失败时给出第一处不符的坐标。
fn verify_pixels(got: &[u8], shadow: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        got.len() >= shadow.len(),
        "读回长度 {} 小于预期 {}",
        got.len(),
        shadow.len()
    );
    let mismatches: Vec<usize> = (0..shadow.len())
        .filter(|&i| got[i] != shadow[i])
        .collect();
    if mismatches.is_empty() {
        println!(
            "OK 像素逐字节比对通过（{} 字节全部一致，含 {} 个不同取值）",
            shadow.len(),
            shadow.iter().collect::<std::collections::BTreeSet<_>>().len()
        );
        return Ok(());
    }
    let first = mismatches[0];
    //影子缓冲恒为 TEX*TEX 的正方形，开方即得边长。
    // 报坐标而不只是线性偏移：能一眼看出是横向错位还是纵向错位
    //（后者说明 image_offset.y 或buffer_row_length 出了问题）。
    let side = (shadow.len() as f64).sqrt() as usize;
    anyhow::bail!(
        "像素校验失败：{}/{} 字节不符。第一处在 ({}, {})（线性偏移 {first}）：\
         期望 {} 实得 {}",
        mismatches.len(),
        shadow.len(),
        first % side,
        first / side,
        shadow[first],
        got[first]
    );
}

/// 窗口过程。原样转发给 `DefWindowProcW`（坑 26）。
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
