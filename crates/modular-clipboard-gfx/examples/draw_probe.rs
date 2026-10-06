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
//! 1. **描述符集必须在 `record` 绑定它之前填好**。
//!    `allocate_descriptor_sets` 分配出的集合内容是未初始化的，
//!    着色器一旦解引用就是 UB → 驱动丢设备。本探针**故意**对
//!    **所有**槽位调用 `update_*_binding`。
//! 2. **纹理描述符的 `imageLayout` 必须是上传后的布局**。
//!    `DeviceImage::new` 的初始布局是 `UNDEFINED`，而规范禁止用
//!    `UNDEFINED` 更新 `COMBINED_IMAGE_SAMPLER`（纹理视图的内容
//!    本就Undefined，写进描述符等于宣称「没有内容」，自相矛盾）。
//!    ⇒ 绑定必须排在 [`FrameRenderer::record_texture_upload`] **之后**。
//!    见下方「为什么绑定不能写在 acquire 之前」的说明。
//! 3. `record` 有批次时会绑定描述符集并 `draw_indexed`，
//!    这条路必须不崩。
//!
//! # 用法
//!
//! ```text
//! cargo run -p modular-clipboard-gfx --example draw_probe
//! ```
//!
//! 退出码 0 = 首次绘制正常。

use ash::vk;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{DrawBatch, DrawInput, FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::pipeline::Uniforms;
use modular_clipboard_gfx::texture::{DeviceImage, Sampler, FONT_FORMAT};
use modular_clipboard_gfx::Gpu;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 窗口尺寸。
const W: i32 = 640;
const H: i32 = 480;

/// 字体图集边长（像素）。
const ATLAS: u32 = 256;

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
    let pipeline =
        modular_clipboard_gfx::pipeline::GraphicsPipeline::new(&gpu.device, rp.handle, pl.handle, &shader)?;
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
    // 字体图集用「图像 + 采样器」而不是 `FontTexture`：
    // 后者把 `DeviceImage` 私有封装起来了，而本探针必须拿到
    // `&mut DeviceImage` 才能调`record_texture_upload` 把布局从
    // `UNDEFINED` 推到 `SHADER_READ_ONLY_OPTIMAL`。
    //
    // 采样器必须是最近邻：字体图集是覆盖率图，线性插值会把相邻
    // 字形的覆盖率渗进当前字形边缘。
    let mut font = DeviceImage::new(
        &gpu,
        vk::Extent2D { width: ATLAS, height: ATLAS },
        FONT_FORMAT,
        vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
    )?;
    let font_sampler = Sampler::new_nearest(&gpu.device)?;
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

    // ⚠️ 关键顺序（本探针存在过的真实违规）：
    //
    // 纹理绑定必须在**第一次纹理上传之后**，且要对**所有**槽位都写。
    //
    // 两个约束各有各的理由，**不冲突**：
    //
    // - 「所有槽位都写」：GPU 可能还在读上一个槽位的描述符集，
    //   只写当前帧会让其它帧读到未初始化内容（UB → 丢设备）。
    // - 「上传之后」：`DeviceImage::new` 的 `initial_layout` 是
    //   `UNDEFINED`。规范禁止用 `UNDEFINED` 更新
    //   `COMBINED_IMAGE_SAMPLER`——纹理视图的内容本就Undefined，
    //   写进描述符等于宣称「此视图没有内容」，自相矛盾
    //   （`VUID-VkWriteDescriptorSet-descriptorType-04150`）。
    //   上传后 `font.layout` 才是 `SHADER_READ_ONLY_OPTIMAL`。
    //
    // 曾经写成「在第一次 acquire 之前绑定」而**从未上传**，
    // 于是把 `UNDEFINED` 写进了描述符——验证层稳定报 3 次该 VUID。
    //
    // uniform绑定不受此约束（它没有 imageLayout），但为便于对照，
    // 同样统一放在这里。
    let coverage = atlas_coverage(ATLAS);
    println!("OK 字体图集覆盖率数据已生成（{} 字节）", coverage.len());

    // --- 唯一的一帧：真正绘制 ---
    let frame = fr
        .acquire()?
        .ok_or_else(|| anyhow::anyhow!("首次 acquire 返回过期（不应发生）"))?;
    println!("OK acquire  image_index={}", frame.image_index);

    // 纹理上传：必须在 acquire 之后（命令录进本帧命令缓冲）、
    // record 之前（传输命令不能录在渲染通道作用域内）。
    // 本探针只跑 1 帧，因此 `can_record_upload` 的 in_flight 分支不会触发。
    fr.record_texture_upload(&mut font, &coverage, None, (ATLAS, ATLAS))?;
    anyhow::ensure!(
        font.layout == vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        "上传后布局应为 SHADER_READ_ONLY_OPTIMAL，实为 {:?}",
        font.layout
    );
    println!("OK record_texture_upload  布局已推进到 {:?}", font.layout);

    for slot in 0..fr.slot_count() {
        fr.update_texture_binding(slot, font_sampler.handle(), font.view, font.layout)?;
        fr.update_uniform_binding(slot, ub.buffer().handle(), 0, ub.buffer().size())?;
    }
    println!("OK 描述符绑定已写入全部 {} 个槽位", fr.slot_count());

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
    // 因此只需销毁本探针自己创建的对象。采样器必须在描述符不再引用后销毁，
    // 且图像要先于采样器销毁——`DeviceImage::destroy` 内部是视图→图像→显存。
    font.destroy(&gpu.device);
    font_sampler.destroy(&gpu.device);
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

/// 生成字体图集的覆盖率字节（单通道，R8 每纹素 1 字节）。
///
/// 本探针不跑 egui，只需要一份**有内容**的纹理——采样一个全零纹理
/// 会让三角形画成黑色，看不出绘制是否真的发生。用一个渐变加棋盘格，
/// 任何一块区域被采样到都能立刻从颜色上分辨。
fn atlas_coverage(size: u32) -> Vec<u8> {
    (0..(size * size) as usize)
        .map(|i| {
            let x = (i as u32) % size;
            let y = (i as u32) / size;
            let checker = ((x / 32) + (y / 32)) % 2 == 0;
            let gradient = ((x + y) % 256) as u8;
            if checker {
                gradient
            } else {
                255 - gradient
            }
        })
        .collect()
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
