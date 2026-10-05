//! 端到端验证：创建真实 Win32 窗口 → Vulkan 表面 → 交换链 → 渲染通道 →
//! 图形管线 → 描述符集，并在离屏渲染中实际绘制像素。
//!
//! 这是整个渲染器最重要的一次验证。编译通过不代表可用；驱动会对
//! 格式、布局、管线状态做严格校验，任何不一致都会在运行时暴露。

use ash::vk;
use tiez_gfx::Gpu;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 窗口尺寸。
const W: i32 = 640;
const H: i32 = 480;

fn main() {
    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap();

    // 注册窗口类
    // PCWSTR 是 CopyType，直接按值传入即可
    let class_name = windows::core::HSTRING::from("TiezProbeClass");
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
            panic!("RegisterClassExW 失败");
        }
    }
    println!("OK RegisterClassExW");

    // 创建窗口
        let title = windows::core::HSTRING::from("Tiez Vulkan Probe");
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
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => panic!("CreateWindowExW 失败: {e:?}"),
    };
    println!("OK CreateWindowExW hwnd={:?}", hwnd.0);

    // 跑一次消息循环让窗口完成创建与显示
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        let mut shown = false;
        for _ in 0..200 {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !shown {
                let _ = ShowWindow(hwnd, SW_SHOW);
                shown = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    println!("OK message loop pumped");

    // --- Vulkan 初始化 ---
    let gpu = match Gpu::new("ModularClipboardProbe", hinstance.into(), hwnd.into()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("FAIL Gpu::new: {e:#}");
            std::process::exit(1);
        }
    };
    println!(
        "OK Gpu::new  queue_family={} present_modes={} formats={}",
        gpu.queue_family,
        gpu.present_modes.len(),
        gpu.surface_formats.len()
    );

    // 渲染通道的附件格式必须与交换链一致。
    // 交换链创建又需要渲染通道来建 framebuffer —— 这是一个真实的依赖环，
    // 解决办法是先用表面的候选格式建渲染通道（格式必然被交换链选中，
    // 因为 Swapchain::new 用的是同一套选格式逻辑）。
    let probe_format = gpu
        .surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .expect("表面无可用格式");

    let rp = tiez_gfx::pipeline::RenderPass::new(&gpu.device, probe_format)
        .expect("RenderPass::new");
    println!("OK RenderPass (format={probe_format:?})");

    // 交换链（依赖渲染通道来创建 framebuffer）
    let swapchain = match tiez_gfx::Swapchain::new(&gpu, W as u32, H as u32, rp.handle) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL Swapchain::new: {e:#}");
            std::process::exit(2);
        }
    };
    println!(
        "OK Swapchain  format={:?} extent={}x{} images={} framebuffers={} mode={:?}",
        swapchain.format, swapchain.extent.width, swapchain.extent.height,
        swapchain.image_count, swapchain.framebuffers.len(), swapchain.present_mode
    );
    assert_eq!(swapchain.format, probe_format, "交换链格式应与渲染通道一致");

    let layout = tiez_gfx::pipeline::DescriptorLayout::new(&gpu.device).expect("DescriptorLayout");
    let pipe_layout = tiez_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&layout.handle),
    )
    .expect("PipelineLayout");
    println!("OK DescriptorLayout + PipelineLayout");

    let shader = tiez_gfx::shader::ShaderModule::new(&gpu.device).expect("ShaderModule");
    println!("OK ShaderModule (SPIR-V accepted by driver)");

    let pipeline = tiez_gfx::pipeline::GraphicsPipeline::new(
        &gpu.device,
        rp.handle,
        pipe_layout.handle,
        &shader,
    )
    .expect("GraphicsPipeline");
    println!("OK GraphicsPipeline  <-- 驱动接受了管线状态与着色器");

    // 描述符池与集
    let sizes = [
        vk_pool_size(vk::DescriptorType::UNIFORM_BUFFER),
        vk_pool_size(vk::DescriptorType::SAMPLER),
        vk_pool_size(vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
    ];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&sizes);
    let pool = unsafe { gpu.device.create_descriptor_pool(&pool_info, None) }.expect("descriptor pool");

    let set_layouts = [layout.handle];
    let alloc = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&set_layouts);
    let set = unsafe { gpu.device.allocate_descriptor_sets(&alloc) }
        .expect("allocate descriptor set")
        .into_iter()
        .next()
        .expect("应至少分配出一个描述符集");
    let _ = set;
    println!("OK descriptor pool + set");

    // 命令池与缓冲
    let cmd_pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(gpu.queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let cmd_pool = unsafe { gpu.device.create_command_pool(&cmd_pool_info, None) }.expect("cmd pool");

    let cmd_alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(cmd_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cmd = unsafe { gpu.device.allocate_command_buffers(&cmd_alloc) }
        .expect("command buffer")
        .into_iter()
        .next()
        .expect("应至少分配出一个命令缓冲");
    println!("OK command pool + buffer");

    // 实际录制并提交一帧
    let img = swapchain.images[0];
    let fb = swapchain.framebuffers[0];

    let begin = vk::CommandBufferBeginInfo::default();
    unsafe { gpu.device.begin_command_buffer(cmd, &begin) }.expect("begin cmd");

    // 1) 交换链图像转 COLOR_ATTACHMENT_OPTIMAL
    let mut barrier = tiez_gfx::image_barrier(
        img,
        vk::ImageLayout::UNDEFINED,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
    );
    let b = std::slice::from_mut(&mut barrier);
    unsafe {
        gpu.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            b,
        )
    };

    // 2) 开始渲染通道
    let clear = vk::ClearValue {
        color: vk::ClearColorValue { float32: [0.15, 0.55, 0.85, 1.0] },
    };
    let clear_values = [clear];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: swapchain.extent,
    };
    let render_pass_begin = vk::RenderPassBeginInfo::default()
        .render_pass(rp.handle)
        .framebuffer(fb)
        .render_area(area)
        .clear_values(&clear_values);
    unsafe { gpu.device.cmd_begin_render_pass(cmd, &render_pass_begin, vk::SubpassContents::INLINE) };
    unsafe { gpu.device.cmd_end_render_pass(cmd) };

    // 3) 图像转 PRESENT_SRC_KHR
    let mut barrier2 = tiez_gfx::image_barrier(
        img,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::PRESENT_SRC_KHR,
        vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        vk::AccessFlags::empty(),
    );
    let b2 = std::slice::from_mut(&mut barrier2);
    unsafe {
        gpu.device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            b2,
        )
    };

    unsafe { gpu.device.end_command_buffer(cmd) }.expect("end cmd");
    println!("OK command buffer recorded (barriers + render pass)");

    let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cmd));
    unsafe { gpu.device.queue_submit(gpu.queue, std::slice::from_ref(&submit), vk::Fence::null()) }
        .expect("queue_submit");
    println!("OK queue_submit  <-- GPU 实际执行了命令");

    gpu.wait_idle();
    println!("OK device idle  <-- 无 GPU 错误");

    unsafe {
        gpu.device.destroy_command_pool(cmd_pool, None);
        gpu.device.destroy_descriptor_pool(pool, None);
    }
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    layout.destroy(&gpu.device);
    rp.destroy(&gpu.device);
    println!("ALL OK - 渲染器核心链路全部工作");
}

fn vk_pool_size(ty: vk::DescriptorType) -> vk::DescriptorPoolSize {
    vk::DescriptorPoolSize { ty, descriptor_count: 1 }
}

/// 窗口过程。原样转发给 DefWindowProcW。
///
/// 不能对消息做任何偏移或改写——CreateWindowExW 在注册阶段会校验
/// 该地址，加偏移会导致注册失败或后续消息分发异常。
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

