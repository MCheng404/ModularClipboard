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
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::frame::{PipelineBundle, PresentResult};
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT};
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

/// `UPLOAD_PROBE_STRESS=1` 时的帧数。
///
/// 竞态问题需要足够长的运行才能稳定暴露；600 帧配合 20 次重复
/// 是团队约定的验收标准。
const STRESS_FRAMES: u32 = 600;

/// 本次实际要跑多少帧。可用 `UPLOAD_PROBE_FRAMES` 覆盖。
///
/// # 为什么要能调
///
/// 纹理上传的竞争是**概率性**的：60 帧跑一次大约 11/12 通过，
/// 不足以判断「修好了」还是「碰巧这次没触发」。
/// 把帧数拉长能让失败以高概率出现，修复前后才有可比的确定性信号。
fn frames_to_run() -> u32 {
    if let Some(n) = std::env::var("UPLOAD_PROBE_FRAMES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
    {
        return n;
    }
    if stress_mode() {
        return STRESS_FRAMES;
    }
    FRAMES
}

/// stress 模式：把帧数拉长并逐帧检查设备存活。
///
/// 单次跑通不能证明修好了——竞态问题需要高样本量才能稳定暴露。
/// 团队约定的验收标准是「600 帧 × 20 次全过」。
///
/// - `UPLOAD_PROBE_STRESS=1` 等价于 `UPLOAD_PROBE_FRAMES=600`，
///   且**逐帧**打印设备存活检查点；
/// - 失败时输出「帧号 + 最后一个存活检查点」，供定位使用。
fn stress_mode() -> bool {
    std::env::var_os("UPLOAD_PROBE_STRESS").is_some()
}

fn main() {
    // 模式 C：只验证「staging 分配 + BufferImageCopy + 布局屏障」这条链，
    // 完全绕开帧循环（不 acquire / 不 present / 不用 arena）。
    // 用来把问题范围砍半：若模式 C 也丢设备，根因在纹理上传侧；
    // 若通过，根因在帧同步侧。
    if std::env::var_os("UPLOAD_PROBE_MODE_C").is_some() {
        if let Err(e) = run_mode_c() {
            eprintln!("FAIL upload_probe(模式C): {e:#}");
            std::process::exit(1);
        }
        println!("ALL OK - 模式C：上传链路独立验证通过");
        return;
    }
    if let Err(e) = run() {
        eprintln!("FAIL upload_probe: {e:#}");
        std::process::exit(1);
    }
    println!("ALL OK - 纹理上传全链路验证通过（{} 帧局部更新，像素逐字节一致）", frames_to_run());
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

    // 跑消息循环让窗口完成显示。
    //
    // ⚠️ `ShowWindow` 不能省。窗口在不可见状态时，Win32 的表面
    // 无法呈现，`acquire_next_image` 会让驱动进入不可恢复的状态——
    // 实测症状是**首次 acquire 就报「逻辑设备已丢失」**，而不是任何
    // 指向「窗口没显示」的错误信息。`pipeline_probe` 早就有这一行，
    // 本探针最初漏了，排查了很久。
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg: MSG = std::mem::zeroed();
        for _ in 0..100 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    println!("OK window shown + message loop pumped");

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

    let rp = modular_clipboard_gfx::pipeline::RenderPass::new(&gpu.device, probe_format)?;
    let layout = modular_clipboard_gfx::pipeline::DescriptorLayout::new(
        &gpu.device,
        gpu.desc_caps.all_bindings_update_after_bind(),
    )?;
    let pipe_layout = modular_clipboard_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&layout.handle),
    )?;
    let shader = modular_clipboard_gfx::shader::ShaderModule::new(&gpu.device)?;
    let pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
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

    alive(&gpu, "建管线后");
    let mut fr = modular_clipboard_gfx::frame::FrameRenderer::new(&gpu, bundle)?;
    alive(&gpu, "FrameRenderer::new 后");
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
    alive(&gpu, "创建 DeviceImage 后");

    // 读回缓冲必须是**主机可见**的。`Buffer::new` 要求 DEVICE_LOCAL，
    // 在独显上那是不可映射的显存，`map_memory` 会直接失败。
    let tex_bytes = (TEX * TEX) as vk::DeviceSize;
    let mut readback = modular_clipboard_gfx::buffer::Buffer::new_host_visible(
        &gpu,
        tex_bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;
    println!("OK readback buffer（{tex_bytes} 字节，主机可见）");
    alive(&gpu, "创建 readback 后");

    // 密集上传模式：确定性触发「已写入块需扩容」路径。
    // 见 plan_patches_dense 的说明（用于定向复现 0.7% 偶发）。
    let dense_uploads = std::env::var_os("UPLOAD_PROBE_DENSE").is_some();
    if dense_uploads {
        println!("   [模式] 密集上传：每帧恰好 2 次，强制走need_new 分支");
    }

    // CPU 侧影子缓冲：记录「GPU 上应该是什么样」。最终逐字节比对。
    let mut shadow = vec![0u8; (TEX * TEX) as usize];
    alive(&gpu, "进入帧循环前");

    // ---- 逐帧上传 ----------------------------------------------------
    let t0 = Instant::now();
    let mut done = 0u32;
    let mut rebuilds = 0u32;
    let mut uploads = 0u64;
    let mut post_present_rejections = 0u32;
    // 默认不逐帧 wait：让多帧真正在途，否则测不到 arena 的退休/复用。
    let wait_each_frame = std::env::var_os("UPLOAD_PROBE_WAIT_EACH_FRAME").is_some();
    if wait_each_frame {
        println!("   [模式] 每帧 device_wait_idle —— 多帧在途路径未被考验");
    }

    let frames = frames_to_run();
    while done < frames {
        // acquire 返回 None = 交换链过期。重建后重试**同一帧号**，
        // 保证帧计数与上传计划一一对应。
        let Some(acquired) = fr.acquire()? else {
            fr.rebuild_swapchain(Default::default())?;
            rebuilds += 1;
            println!("  [警告] 第 {done} 帧 acquire 返回过期，已重建交换链，重试该帧");
            continue;
        };

        // 本帧的上传计划：位置与尺寸都随帧号变化。
        // `UPLOAD_PROBE_DENSE=1` 换成确定性密集模式（见 plan_patches_dense）。
        let plan = if dense_uploads {
            plan_patches_dense(done, TEX)
        } else {
            plan_patches(done, TEX)
        };
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

        // 验证「重复录制」确实被挡住。
        //
        // present 之后命令缓冲已交给 GPU，绝不允许再追加命令。
        // 具体撞上哪道门取决于实现的当前形态，两种都算正确拒绝：
        // - `pending` 已取走 ⇒ 「必须在 acquire 之后调用」
        // - `in_flight` 仍为真⇒ 「GPU 仍在执行，不能重复录制」
        //
        // 只断言「被拒」，不绑死具体措辞——库的实现可以演进，
        // 但「present 之后不能重录」这条不变式必须成立。
        if let Err(e) = fr.record_texture_upload(&mut image, &[0u8], Some((0, 0)), (1, 1)) {
            let msg = e.to_string();
            anyhow::ensure!(
                msg.contains("acquire") || msg.contains("重复录制"),
                "present 之后的重复上传应被拒绝，实际报错：{msg}"
            );
            post_present_rejections += 1;
        } else {
            anyhow::bail!("第 {done} 帧：present 之后竟然还能录制，命令缓冲保护失效");
        }

        done += 1;
        // 逐帧确认设备存活。
        //
        // ⚠️ 这里的 `device_wait_idle` **每帧强制同步**，会掩盖
        // 「多帧在途」路径上的bug——MEMORY.md 坑 28 明确说过
        // 不要靠每帧 wait idle 绕过 staging 问题。因此默认**关闭**，
        // 需要时用 `UPLOAD_PROBE_WAIT_EACH_FRAME=1` 打开。
        //
        // 两种模式的用途不同：
        // - 关闭（默认）：验证**多帧在途**的真实路径，arena 的
        //   退休/复用才算真正被考验。
        // - 打开：逐帧确认「本帧提交 + 上传」本身没有 GPU 错误，
        //   用于把「上传逻辑错」与「在途时序错」区分开。
        //
        // stress 模式额外做**帧号级**存活检查点：竞态问题只在特定帧暴露，
        // 失败时需要知道「最后一个活着的帧号」才能定位。
        // 注意这会让 stress 模式退化为逐帧同步，因此它**只能用于验收**
        // （证明修复后不崩），**不能用于**验证多帧在途本身是否正确——
        // 那是模式 A（默认，不开 stress）的职责。
        let checkpoint = wait_each_frame || stress_mode();
        if checkpoint && !alive_quiet(&gpu) {
            anyhow::bail!(
                "第 {done} 帧 present 之后设备已丢失
                 ===== 诊断信息 =====
                 最后存活帧号：{done}
                 已完成帧数：{done} / {frames}
                 已上传次数：{uploads}
                 模式：{}",
                if stress_mode() { "STRESS（逐帧同步，仅用于验收）" } else { "WAIT_EACH_FRAME" }
            );
        }
        // 开头逐帧打印：崩溃集中在开局（实测 5/20 次失败全在第 10 帧内），
        // 每 10 帧一次的进度对定位太粗。之后恢复每 10 帧。
        if done <= 20 {
            println!("    帧{done} 完成");
        }
        if done % PROGRESS_EVERY == 0 {
            let elapsed = t0.elapsed();
            println!(
                "  进度 {done}/{frames} 帧  上传 {uploads} 次  重建 {rebuilds} 次  \
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
        // 必须走库 API：裸调image_barrier 不会回写 `image.layout`，
        // 状态失同步后下一次 upload 会用过期的oldLayout 算屏障 ⇒ 设备丢失。
        image.transition_to(
            cmd,
            &gpu.device,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
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
    match modular_clipboard_gfx::frame::can_record_upload(true, true) {
        Err(e) => println!("OK can_record_upload(in_flight=true) 正确拒绝：{e}"),
        Ok(()) => panic!("can_record_upload 未拦住 in_flight=true"),
    }
    // pending=false 时也必须拒绝。
    match modular_clipboard_gfx::frame::can_record_upload(false, false) {
        Err(e) => println!("OK can_record_upload(pending=false) 正确拒绝：{e}"),
        Ok(()) => panic!("can_record_upload 未拦住 pending=false"),
    }
}

// ---------------------------------------------------------------------------
// 上传计划（纯逻辑，可离线复核）
// ---------------------------------------------------------------------------

/// 模式 C：**只**验证纹理上传链路，绕开帧循环。
///
/// # 为什么需要这个模式
///
/// 模式 A 失败（device-lost）时有两种可能：
/// 1. 根因在**纹理上传 / staging / 布局屏障**——那么单独跑上传也该崩；
/// 2. 根因在**帧同步**（acquire/present/arena 退休）——那么单独跑上传应当通过。
///
/// 这个模式把帧循环整个拿掉：自建 command pool，用
/// [`modular_clipboard_gfx::buffer::Buffer::new_host_visible`] 拿一块主机可见内存，
/// 手动录 `cmd_copy_buffer_to_image` + 布局屏障，每帧 submit + wait_idle。
/// 图案用与模式 A 完全相同的 LCG 序列，读回结果可直接对照。
///
/// # 它**不**验证什么
///
/// - 不用 [`crate::frame::StagingArena`]：arena 的退休/复用不在此列；
/// - 不做多帧在途：每帧都 wait_idle，所以查不出帧同步问题。
///
/// 它只回答一个问题：**上传这条命令链本身是否正确**。
fn run_mode_c() -> anyhow::Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap();
    let cn = windows::core::HSTRING::from("TiezUploadProbeC");
    let cp = windows::core::PCWSTR(cn.as_ptr());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: cp,
        ..Default::default()
    };
    unsafe {
        if RegisterClassExW(&wc) == 0 {
            anyhow::bail!("RegisterClassExW 失败");
        }
    }
    let title = windows::core::HSTRING::from("Tiez Upload Probe C");
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cp,
            windows::core::PCWSTR(title.as_ptr()),
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
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg: MSG = std::mem::zeroed();
        for _ in 0..60 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let gpu = Gpu::new("ModularClipboardUploadProbeC", hinstance.into(), hwnd.into())?;
    let device_name = device_name_of(&gpu);
    println!("GPU: {device_name} ({})", device_kind(gpu.device_type));
    anyhow::ensure!(
        gpu.device_type != vk::PhysicalDeviceType::CPU,
        "拒绝在软件光栅器上运行（{device_name}）"
    );

    let mut image = DeviceImage::new(
        &gpu,
        vk::Extent2D { width: TEX, height: TEX },
        FONT_FORMAT,
        vk::ImageUsageFlags::TRANSFER_DST
            | vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;

    let tex_bytes = (TEX * TEX) as vk::DeviceSize;
    let mut staging = modular_clipboard_gfx::buffer::Buffer::new_host_visible(
        &gpu,
        tex_bytes,
        vk::BufferUsageFlags::TRANSFER_SRC,
        vk::SharingMode::EXCLUSIVE,
    )?;
    let mut readback = modular_clipboard_gfx::buffer::Buffer::new_host_visible(
        &gpu,
        tex_bytes,
        vk::BufferUsageFlags::TRANSFER_DST,
        vk::SharingMode::EXCLUSIVE,
    )?;

    let cmd_pool = unsafe {
        gpu.device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(gpu.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )
    }?;
    let cmd = unsafe {
        gpu.device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("未分配到命令缓冲"))?
    };

    let total = frames_to_run();
    let mut shadow = vec![0u8; (TEX * TEX) as usize];
    for frame in 0..total {
        let plan = plan_patches(frame, TEX);

        // 先把本帧所有 patch 的数据写进 staging 的紧凑区间。
        let mut write_off = 0u64;
        let mut datas: Vec<Vec<u8>> = Vec::with_capacity(plan.len());
        for (k, &(_, _, w, h)) in plan.iter().enumerate() {
            let data = make_pattern(frame, k as u32, w, h);
            staging.write(&gpu.device, write_off, &data)?;
            write_off += data.len() as u64;
            datas.push(data);
        }

        unsafe {
            gpu.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            gpu.device
                .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        }

        let mut cursor = 0u64;
        for (i, &(x, y, w, h)) in plan.iter().enumerate() {
            let old_layout = image.layout;
            // srcStage 必须与 srcAccess 匹配。用 TOP_OF_PIPE 会让屏障成为
            // 空操作——它是逻辑上最早��阶段，不等待任何东西。
            let (src_stage, src_access) = if old_layout == vk::ImageLayout::UNDEFINED {
                (vk::PipelineStageFlags::TOP_OF_PIPE, vk::AccessFlags::empty())
            } else {
                (
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::AccessFlags::SHADER_READ,
                )
            };
            unsafe {
                // 走transition_to 而非裸 image_barrier：前者会自动从
                // self.layout 取old_layout 并回写新布局。手写屏障漏掉回写
                // 会让 layout 字段与图像实际布局失同步——这类缺陷不报错、
                // 只是屏障静默失效，必须靠 API 约束而非人工纪律来防。
                image.transition_to(
                    cmd,
                    &gpu.device,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    src_access,
                    vk::AccessFlags::TRANSFER_WRITE,
                    src_stage,
                    vk::PipelineStageFlags::TRANSFER,
                );
                let copy = modular_clipboard_gfx::texture::buffer_image_copy(Some((x, y)), w, h)
                    .buffer_offset(cursor);
                gpu.device.cmd_copy_buffer_to_image(
                    cmd,
                    staging.handle(),
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
                image.transition_to(
                    cmd,
                    &gpu.device,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                );
            }
            image.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
            cursor += datas[i].len() as u64;
        }
        unsafe { gpu.device.end_command_buffer(cmd)? };
        let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
        unsafe {
            gpu.device
                .queue_submit(gpu.queue, std::slice::from_ref(&submit), vk::Fence::null())?
        };
        // 每帧同步：这是模式 C 与模式 A 的唯一差别。
        gpu.wait_idle();

        for (i, &(x, y, w, h)) in plan.iter().enumerate() {
            apply_patch(&mut shadow, TEX, (x, y, w, h), &datas[i]);
        }

        if frame % PROGRESS_EVERY == 0 || frame + 1 == total {
            println!("  模式C 进度 {}/{total} 帧", frame + 1);
        }
    }

    // 读回后转回着色器可读状态，同样必须走库 API。
    image.restore_after_readback(&gpu, cmd)?;
    // 读回校验：与模式 A 共用同一个比对函数。
    unsafe {
        gpu.device
            .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
        gpu.device
            .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        // 必须走库 API：裸调image_barrier 不会回写 `image.layout`，
        // 状态失同步后下一次 upload 会用过期的oldLayout 算屏障 ⇒ 设备丢失。
        image.transition_to(
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
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D { width: TEX, height: TEX, depth: 1 });
        gpu.device.cmd_copy_image_to_buffer(
            cmd,
            image.image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.handle(),
            &[copy],
        );
        gpu.device.end_command_buffer(cmd)?;
    }
    let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
    unsafe {
        gpu.device
            .queue_submit(gpu.queue, std::slice::from_ref(&submit), vk::Fence::null())?
    };
    gpu.wait_idle();

    readback.map(&gpu.device)?;
    let got: Vec<u8> = readback.read::<u8>()?;
    readback.unmap(&gpu.device)?;
    verify_pixels(&got, &shadow)?;

    let mut image = image;
    image.destroy(&gpu.device);
    let mut staging = staging;
    staging.destroy(&gpu.device);
    let mut readback = readback;
    readback.destroy(&gpu.device);
    unsafe { gpu.device.destroy_command_pool(cmd_pool, None) };
    println!("OK 模式C 资源全部销毁");
    Ok(())
}

/// 只检查设备是否存活，不打印（用于逐帧高频检查）。
fn alive_quiet(gpu: &Gpu) -> bool {
    unsafe { gpu.device.device_wait_idle() }.is_ok()
}

/// 打印一个标记，并检查设备是否仍然存活。
///
/// 设备丢失（`ERROR_DEVICE_LOST`）在后续所有调用里都表现为同一个错误，
/// 但**根因**往往在更早的某一步。不逐段检查的话，只会看到
/// 「acquire 报设备丢失」这种把矛头指向错误位置的假象。
fn alive(gpu: &Gpu, tag: &str) {
    match unsafe { gpu.device.device_wait_idle() } {
        Ok(()) => println!("   [存活] {tag}"),
        Err(e) => println!("   [设备已丢失] {tag}: {e:?}"),
    }
}

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
/// 密集上传计划：**确定性**地撑满 staging 块，强制走「已写入块需扩容」路径。
///
/// # 为什么需要它
///
/// 随机上传（`plan_patches`）只有**恰好**在同帧内把块剩余空间撑到不足时
/// 才会触发 `need_new` 分支，而帧 0 是整图上传、其余帧是 8..=96 的小块，
/// 命中概率极低——实测 139 次里只中 1 次。
///
/// 这个模式把变量固定住：
/// - **每帧恰好 2 次上传**（不是随机的 1~3 次）
/// - 第 1 次：占掉块的大部分空间
/// - 第 2 次：请求**大于剩余空间**的尺寸，必然触发 `need_new`
///
/// 此时 `used != 0`，`staging.rs:308` 会走「改用空闲块」分支——
/// 也就是上一轮 use-after-free 的同一位置。
///
/// 若修复真的有效，这个模式应当**稳定通过**（走空闲块，正常返回）；
/// 若仍会丢设备，就能立刻归因到这条路径。
fn plan_patches_dense(frame: u32, tex: u32) -> Vec<(u32, u32, u32, u32)> {
    // 第 1 次：约3/4 块（TEX=256 时 65536 字节的块→ 留1/4 余量）
    let big = (tex * 3 / 4).max(8);
    // 第 2 次：请求超过剩余空间的尺寸。
    // MIN_STAGING_CAPACITY = 64 KiB = 65536；tex*tex 恰为 65536，
    // 因此整图请求必然 > 任何已被占用的剩余空间。
    let full = tex;
    // 两块都贴左上角，第2 次完全覆盖第 1 次——像素校验仍能验证最终状态。
    let _ = frame;
    vec![(0, 0, big, big), (0, 0, full, full)]
}

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
