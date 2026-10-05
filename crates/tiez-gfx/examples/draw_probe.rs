//! 最小「首次绘制」探针：只跑 1 帧，验证**真正 draw 一次**不会丢设备。
//!
//! # 为什么需要它
//!
//! `upload_probe` 验证的是纹理上传链路，但它 `record` 传的是空批次，
//! 会提前 return —— **既不绑描述符集，也不 `draw_indexed`**。
//! `pipeline_probe` 只 `allocate` 描述符集、从不绑定。
//!
//! 因此「首次真正绘制」这条路径此前**没有任何探针覆盖**，而它恰恰是
//! `full_app` 曾经丢设备的地方。本探针把这条路径单独钉住：
//! 它跑得比 600 帧的 `full_app` 快得多，且失败时归因非常直接。
//!
//! # 它守住的不变式
//!
//! 1. **描述符集必须在第一次 `acquire` 之前填好**。
//!    `allocate_descriptor_sets` 分配出的集合内容是未初始化的，
//!    着色器一旦解引用就是 UB → 驱动丢设备。本探针**故意**在
//!    `acquire` 之前对**所有**槽位调用 `update_*_binding`。
//! 2. `record` 有批次时会绑定描述符集并 `draw_indexed`，
//!    这条路必须不崩。
//!
//! # 用法
//!
//! ```text
//! cargo run -p tiez-gfx --example draw_probe
//! ```
//!
//! 退出码 0 = 首次绘制正常。

use ash::vk;
use tiez_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use tiez_gfx::frame::{DrawBatch, DrawInput, FrameRenderer, PipelineBundle, PresentResult};
use tiez_gfx::pipeline::Uniforms;
use tiez_gfx::texture::FontTexture;
use tiez_gfx::Gpu;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 窗口尺寸。
const W: i32 = 640;
const H: i32 = 480;

/// 一个覆盖视口的三角形（三个顶点，足以触发一次真实光栅化）。
///
/// 位置是**逻辑像素**；着色器会乘 `dpr` 再变换到 NDC。
fn full_screen_triangle(w: f32, h: f32) -> [Vertex; 3] {
    let corners = [(0.0, 0.0), (w, 0.0), (w * 0.5, h)];
    corners
        .map(|(x, y)| Vertex {
            pos: [x, y],
            uv: [x / w, y / h],
            // 不透明白色，ABGR 小端序。
            color: 0xFFFF_FFFF,
        })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL draw_probe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap();
    let hwnd = create_window(hinstance.into())?;
    pump_messages(hwnd);
    println!("OK Win32 窗口已创建并显示");

    // --- Vulkan ---
    let gpu = Gpu::new("ModularClipboardDrawProbe", hinstance.into(), hwnd.into())?;
    println!(
        "OK Gpu::new  queue_family={} formats={}",
        gpu.queue_family,
        gpu.surface_formats.len()
    );

    // 渲染通道的附件格式必须与交换链一致（与 Swapchain::new 同一套选格式逻辑）。
    let format = gpu
        .surface_formats
        .iter()
        .map(|f| f.format)
        .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
        .or_else(|| gpu.surface_formats.first().map(|f| f.format))
        .ok_or_else(|| anyhow::anyhow!("表面无可用格式"))?;

    let rp = tiez_gfx::pipeline::RenderPass::new(&gpu.device, format)?;
    let dl = tiez_gfx::pipeline::DescriptorLayout::new(&gpu.device)?;
    let pl = tiez_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&dl.handle),
    )?;
    let shader = tiez_gfx::shader::ShaderModule::new(&gpu.device)?;
    let pipeline =
        tiez_gfx::pipeline::GraphicsPipeline::new(&gpu.device, rp.handle, pl.handle, &shader)?;
    println!("OK 渲染通道 + 描述符布局 + 管线 + 着色器");

    let bundle = PipelineBundle::new(rp.handle, pl.handle, pipeline.handle, dl.handle);
    let mut fr = FrameRenderer::new(&gpu, bundle)?;
    println!(
        "OK FrameRenderer  extent={}x{}  槽位={}",
        fr.extent().width,
        fr.extent().height,
        fr.slot_count()
    );

    // --- 绘制所需的资源 ---
    // 顶点/索引用 host-visible 缓冲直接写：与 `full_app` 同一做法。
    // 之所以不绕 staging，本探针只跑 1 帧，目的是隔离「首次绘制」
    // 这一个变量，不引入上传路径的干扰。
    let mut vb = Buffer::new_host_visible(
        &gpu,
        (3 * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )?;
    let mut ib = Buffer::new_host_visible(
        &gpu,
        (3 * std::mem::size_of::<u32>()) as vk::DeviceSize,
        vk::BufferUsageFlags::INDEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )?;
    let mut ub = UniformBuffer::new(&gpu)?;
    // FontTexture 自带最近邻采样器——字体图集必须点采样，
    // 线性插值会把相邻字形的覆盖率渗进当前字形边缘。
    let mut font = FontTexture::new(&gpu, 256, 256)?;
    println!("OK 顶点/索引/uniform 缓冲 + 字体纹理（含采样器）");

    let verts = full_screen_triangle(fr.extent().width as f32, fr.extent().height as f32);
    vb.write(&gpu.device, 0, bytemuck::cast_slice(&verts))?;
    let indices: [u32; 3] = [0, 1, 2];
    ib.write(&gpu.device, 0, bytemuck::cast_slice(&indices))?;

    // uniform：单位矩阵 + 视口尺寸。矩阵全 0 会让顶点退化但不丢设备，
    // 这里给单位矩阵以保证绘制本身有效。
    let (w, h) = (fr.extent().width as f32, fr.extent().height as f32);
    ub.write(&gpu.device, &Uniforms::new(IDENTITY, [w, h], 1.0))?;
    println!("OK 顶点/索引/uniform 数据已上传");

    // ⚠️ 关键顺序：描述符绑定必须在**第一次 acquire 之前**，
    // 且要对**所有**槽位都写。GPU 可能还在读上一个槽位的描述符集，
    // 只写当前帧会让其它帧读到未初始化内容（UB → 丢设备）。
    for slot in 0..fr.slot_count() {
        fr.update_texture_binding(slot, font.sampler(), font.view(), font.layout())?;
        fr.update_uniform_binding(slot, ub.buffer().handle(), 0, ub.buffer().size())?;
    }
    println!("OK 描述符绑定已写入全部 {} 个槽位", fr.slot_count());

    // --- 唯一的一帧：真正绘制 ---
    let frame = fr
        .acquire()?
        .ok_or_else(|| anyhow::anyhow!("首次 acquire 返回过期（不应发生）"))?;
    println!("OK acquire  image_index={}", frame.image_index);

    let input = DrawInput {
        vertex_buffer: vb.handle(),
        index_buffer: ib.handle(),
        batches: &[DrawBatch {
            index_offset: 0,
            index_count: 3,
        }],
        ..Default::default()
    };
    fr.record(&input)?;
    println!("OK record（1 个 draw call，含描述符绑定）");

    let result = fr.present(frame)?;
    anyhow::ensure!(
        result == PresentResult::Presented,
        "首次 present 返回 {result:?}（不应为 Outdated）"
    );
    println!("OK present  Presented");

    // 关键：等GPU 真正跑完。丢设备是异步的，不等就看不到。
    gpu.wait_idle();
    println!("OK device_wait_idle  <-- 首次绘制无 GPU 错误");

    // 清理：FrameRenderer 的 Drop 会 wait_idle 后按正确顺序销毁交换链资源，
    // 因此只需销毁本探针自己创建的对象。FontTexture 自持采样器与图像，
    // 由它自己的 Drop 处理。
    font.destroy(&gpu.device);
    ub.destroy(&gpu.device);
    ib.destroy(&gpu.device);
    vb.destroy(&gpu.device);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pl.destroy(&gpu.device);
    dl.destroy(&gpu.device);
    rp.destroy(&gpu.device);
    // 强制销毁 FrameRenderer，确保其在 Gpu 之前释放。
    drop(fr);
    drop(gpu);
    println!("ALL OK - 首次绘制链路正常");
    Ok(())
}

/// 4x4 单位矩阵（列主序）。
///
/// 本探针的着色器自己按 `size_in_pixels` 做像素→NDC 变换，
/// 这里的矩阵只作为「非零占位」传入。
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, //
];

fn create_window(hinstance: HINSTANCE) -> anyhow::Result<HWND> {
    let class_name = windows::core::HSTRING::from("TiezDrawProbeClass");
    let cp = windows::core::PCWSTR(class_name.as_ptr());
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
    let title = windows::core::HSTRING::from("Tiez Draw Probe");
    let tp = windows::core::PCWSTR(title.as_ptr());
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            cp,
            tp,
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
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW 失败: {e:?}"))?;
    Ok(hwnd)
}

/// 跑一小段消息循环让窗口完成创建与显示。
fn pump_messages(hwnd: HWND) {
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
}

/// 窗口过程。原样转发给 DefWindowProcW——加任何偏移都会破坏注册。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
