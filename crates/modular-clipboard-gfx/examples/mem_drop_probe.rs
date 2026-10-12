//! 「销毁是否真的能把内存还给系统」判定探针。
//!
//! # 它回答什么问题
//!
//! 2026-10-12 的内存账目（打点实测，debug 构建）：
//!
//! ```text
//! 窗口创建后        ~3 MB
//! Gpu::new 后       ~60 MB   （+58）Vulkan 实例/设备
//! FrameRenderer 后  ~143 MB  （+82）交换链
//! 全部就绪          ~152 MB  （+8）egui + 中文字体
//! ```
//!
//! 那 +82MB 落在 `Swapchain::new_for_surface` 里，而**销毁旧交换链再建
//! 一个**（启动时 `rebuild_swapchain`）只花 0.3MB —— 强烈提示驱动把这
//! 些分配**留在自己的池里复用**，而不是还给进程。
//!
//! 这决定了后续优化方向：如果只有销毁 vs 不销毁的区别 Memory 都不回去，
//! 那么「隐藏时拆掉整套 GPU/窗口」的收益就很可疑，不值得为此做大改。
//! 反过来，如果销毁 `Gpu` 能把这 ~140MB 收回，那条路径就值得做。
//!
//! # 判定口径
//!
//! 看 **私有提交**（`PrivateUsage`）而不是工作集：工作集能被系统随时
//! 回收，只压它属于自欺。
//!
//! # 用法
//!
//! ```text
//! cargo run -p modular-clipboard-gfx --example mem_drop_probe
//! ```
//!
//! 输出每行一个时间点的 `private_mb`，自行比较。

use modular_clipboard_gfx::frame::{FrameRenderer, PipelineBundle};
use modular_clipboard_gfx::Gpu;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// （本文件）当前进程的私有提交 MB。
fn private_mb() -> f64 {
    use std::ffi::c_void;
    let mut buf = [0u8; 128];
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(handle: *mut c_void, counters: *mut u8, cb: u32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
    }
    // SAFETY: 只查询本进程，缓冲区 128 字节远大于结构体真实尺寸。
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), buf.as_mut_ptr(), 128) } == 0 {
        return -1.0;
    }
    let private = usize::from_ne_bytes(buf[72..80].try_into().unwrap_or([0u8; 8]));
    private as f64 / (1024.0 * 1024.0)
}

fn snap(what: &str) {
    println!("{:<26} private_mb={:>8.1}", what, private_mb());
}

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL mem_drop_probe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    snap("0 起点");

    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap();
    let hwnd = create_window(hinstance.into())?;
    pump_messages(hwnd);
    snap("1 窗口已创建");

    // ---- 建立整套 GPU 栈（作用域结束即全部销毁）--------------------
    {
        let gpu = Gpu::new("ModularClipboardMemProbe", hinstance.into(), hwnd)?;
        snap("2 Gpu::new（设备）");

        let format = pick(&gpu);
        let rp = modular_clipboard_gfx::pipeline::RenderPass::new(&gpu.device, format)?;
        let dl = modular_clipboard_gfx::pipeline::DescriptorLayout::new(
            &gpu.device,
            gpu.desc_caps.all_bindings_update_after_bind(),
        )?;
        let pl = modular_clipboard_gfx::pipeline::PipelineLayout::new(
            &gpu.device,
            std::slice::from_ref(&dl.handle),
        )?;
        let shader = modular_clipboard_gfx::shader::ShaderModule::new(&gpu.device)?;
        let pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
            &gpu.device,
            rp.handle,
            pl.handle,
            &shader,
        )?;
        snap("3 管线对象齐了");

        let bundle = PipelineBundle::new(rp.handle, pl.handle, pipeline.handle, dl.handle);
        let fr = FrameRenderer::new(&gpu, bundle)?;
        snap("4 FrameRenderer（交换链）");

        // 先等 GPU 闲下来再销毁：带在飞的活儿拆资源是另一回事，
        // 这个探针只想量「干净销毁」能收回多少。
        gpu.wait_idle();
        drop(fr);
        snap("5 只 drop FrameRenderer");

        // CAUTION: 这几个类型**没有 `Drop`**——`drop(x)` 对它们什么都不
        // 做（clippy 会警告），必须显式 `destroy(&device)`。早前这里写
        // 的 `drop(pipeline)` 因此是空操作，把「销毁管线对象」这一段
        // 的账目算错了（真实数字要跑到这里才看得出）。
        pipeline.destroy(&gpu.device);
        shader.destroy(&gpu.device);
        pl.destroy(&gpu.device);
        dl.destroy(&gpu.device);
        rp.destroy(&gpu.device);
        snap("6 管线对象已显式销毁");
        // 作用域出去 ⇒ `gpu` 在这之后 Drop（设备 + 实例）。
        println!("   （下一步销毁 Gpu，即 Vulkan 设备）");
    }
    snap("7 Gpu 已销毁");

    // 驱动不一定同步地归还：给一点时间再看，两次数都要。
    std::thread::sleep(std::time::Duration::from_millis(1500));
    snap("8 销毁后等 1.5s");

    Ok(())
}

fn pick(gpu: &Gpu) -> ash::vk::Format {
    gpu.surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .unwrap_or(ash::vk::Format::B8G8R8A8_UNORM)
}

fn create_window(hinstance: HINSTANCE) -> anyhow::Result<HWND> {
    let class_name = windows::core::HSTRING::from("TiezMemProbeClass");
    let cp = windows::core::PCWSTR(class_name.as_ptr());
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: cp,
        ..Default::default()
    };
    unsafe {
        if RegisterClassExW(&wc) == 0 {
            anyhow::bail!("RegisterClassExW 失败");
        }
    }
    let title = windows::core::HSTRING::from("Tiez Mem Probe");
    let tp = windows::core::PCWSTR(title.as_ptr());
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cp,
            tp,
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            520,
            800,
            None,
            None,
            Some(hinstance),
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW 失败: {e:?}"))?;
    Ok(hwnd)
}

fn pump_messages(hwnd: HWND) {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        let mut shown = false;
        for _ in 0..60 {
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
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
