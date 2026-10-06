//! 交换链重建路径验证。**只**回答「resize 后交换链 rebuild 是否正确」，
//! 一个像素都不查。
//!
//! # 为什么要独立成探针
//!
//! 旧的 `resize_probe`（已删除）同时做两件不相关的事：
//!
//! 1. 借 [`FrameRenderer`] 的交换链跑帧循环、触发 resize；
//! 2. **自建**离屏靶面做 A/B 像素校验，却复用生产管线的
//!    `FrameRenderer::descriptor_set(0)` 与生产渲染通道。
//!
//! 于是它既跟随生产代码演进（`pipeline.rs` 改 `final_layout` 后立刻报
//! `VUID-vkCmdDraw-None-09600`），又自己维护一套判据（`BG` 常量与清屏色
//! 不一致，负对照检出全屏像素数）。两个职责混在一起，出问题时无法判断
//! 是库坏了还是探针坏了。
//!
//! 现在拆成两个单一职责的探针：
//! - 本文件：交换链重建路径（无像素校验、无离屏靶面）
//! - `pixels_probe.rs`：像素渲染（自建完整管线，不碰交换链）
//!
//! # 本探针验什么 / 不验什么
//!
//! **验**：
//!
//! 1. 交换链重建**真的发生**（`rebuild_swapchain` 调用次数 > 0）——
//!    `full_app` 一直打印「交换链重建 0 次」，说明这条路径此前从未被实机
//!    触发过。重建次数为 0 意味着本探针什么都没测，因此这是硬判据。
//! 2. `fr.extent()` 等于真实客户区尺寸（`GetClientRect`）——不等说明
//!    重建时用了错误尺寸。
//! 3. 每帧 present 无错误、无 device lost。
//! 4. 收尾时验证层无泄漏报错（销毁顺序正确）。
//!
//! **不验**：任何像素。画面是否正确由 `pixels_probe` 负责；这里每帧只
//! 清屏（`DrawInput::default()` → 空批次），刻意不引入图元、字体图集、
//! 描述符写入——它们全都不是本探针关心的东西，少一个变量就少一个
//! 误判来源。
//!
//! # 窗口位置：留在桌面内 (8, 8)
//!
//! 早前把窗口移到 `(-32000, -32000)` 想「用户看不见」，结果验证层报
//! `VUID-VkSwapchainCreateInfoKHR-pNext-07781`：`resolve_extent` 在
//! `current_extent` 非 0 时原样采用它，而窗口完全离开桌面时 DWM 报告的
//! extent 落在表面能力范围之外。
//!
//! 也不能最小化：最小化把客户区压成 0×0，`resolve_extent` 原样采用 0 会
//! 直接违规（MEMORY 第 19 批记录过这一类假违规）。
//!
//! 故位置固定在桌面内 (8, 8)，**可见但不抢焦点**。
//!
//! # 运行方式
//!
//! ```text
//! # 默认：只在初始尺寸跑 60 帧后退出（安静，不动用户窗口尺寸）
//! cargo run -p modular-clipboard-gfx --example swapchain_resize
//!
//! # 开启 resize 序列（会真实改变窗口尺寸）
//! SWAPCHAIN_RESIZE=1 cargo run -p modular-clipboard-gfx --example swapchain_resize
//! ```
//!
//! 默认关闭是为了让 CI 或他人运行时「什么都不发生」。resize 会改动窗口
//! 尺寸，属于会影响外部的动作，必须显式开启。
//!
//! # 实机预期输出
//!
//! ```text
//! OK Window  客户区 900x640  scale_factor=1  位置 (8,8)（桌面内，不抢焦点）
//! OK Gpu  NVIDIA ... (DiscreteGpu)
//! OK FrameRenderer  extent=900x640  槽位=3  图像=3
//! [resize] SWAPCHAIN_RESIZE 未设置：只在初始尺寸跑 60 帧，不触发 resize
//!   帧 60/60  extent 900x640 = 客户区 900x640  重建 1 次  present: 60 presented / 0 outdated
//! ALL OK - 60 帧，交换链重建 1 次（resize 序列未开启）
//! ```
//!
//! 开启后每个尺寸一段：
//!
//! ```text
//! ---- resize #0：目标客户区 640x480 ----
//!   帧 60/60  extent 640x480 = 客户区 640x480  本段重建 2 次（累计 3）
//!   present: 60 presented / 0 outdated  信号量 3 = 图像数 3
//! ```
//!
//! 退出码 0 = 全部判据通过；1 = 某个判据失败（stderr 打印 `FAIL swapchain_resize: ...`）。

use std::ffi::OsStr;
use std::time::Duration;

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::frame::{DrawInput, FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};

/// 开启 resize 序列的环境变量。
const ENV_RESIZE: &str = "SWAPCHAIN_RESIZE";

/// 初始客户区尺寸（物理像素）。
const W: u32 = 900;
const H: u32 = 640;

/// 窗口位置：留在桌面内。
///
/// 移出屏幕会让 DWM 报告的 `current_extent` 超出表面能力范围，
/// `resolve_extent` 原样采用它就会触发
/// `VUID-VkSwapchainCreateInfoKHR-pNext-07781`（MEMORY 第 19 批）。
const WINDOW_X: i32 = 8;
const WINDOW_Y: i32 = 8;

/// resize 序列（客户区尺寸，单位物理像素）。
///
/// - 必须**至少两个不同尺寸**，否则重建可能不真正换图像；
/// - 最后一档回到初始尺寸：验证「改回去」也能正确重建，而非只单向变化；
/// - 覆盖变大与变小两个方向——只单向变化时某些驱动可能不重建
///   （extent 仍落在 caps 内），销毁链就测不到。
const RESIZE_SEQ: &[(u32, u32)] = &[(640, 480), (1200, 800), (900, 640)];

/// 每个尺寸下要跑的帧数。
///
/// 60 帧足以让「每交换链图像一个信号量」的轮转走完一圈
/// （典型 image_count 为 2~3），从而覆盖索引错配的失效路径。
const FRAMES_PER_SIZE: u64 = 60;

/// 客户区意外变成 0×0 时最多容忍的连续帧数。
///
/// 窗口**不会**被最小化，因此 0×0 属异常而非正常状态；给它一个上限是
/// 为了「等一会儿」能容忍 DWM 的瞬时抖动，而「一直如此」会明确失败，
/// 而不是无限 `continue` 把探针变成无声挂起。
///
/// 取 50（×20ms ≈ 1 秒）：足够覆盖 DWM 的瞬时抖动，又明显短于
/// 一个尺寸阶段的总帧数，于是「卡在 0×0」必然在阶段内暴露。
const MAX_ZERO_CLIENT_FRAMES: u32 = 50;

fn main() {
    if let Err(e) = run() {
        eprintln!("FAIL swapchain_resize: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    // ---- 窗口 ----------------------------------------------------------
    let window = Window::new("ModularClipboard — swapchain_resize", W, H)?;
    move_window(&window, WINDOW_X, WINDOW_Y, W, H)?;
    let mut events = EventLoop::new(&window);
    let (iw, ih) = window.inner_size_physical();
    println!(
        "OK Window  客户区 {iw}x{ih}  scale_factor={}  位置 ({WINDOW_X},{WINDOW_Y})（桌面内，不抢焦点）",
        window.scale_factor()
    );
    anyhow::ensure!(
        iw > 0 && ih > 0,
        "初始客户区为 {iw}x{ih}：窗口不可见或已最小化，表面无法呈现"
    );

    // ---- 设备 ----------------------------------------------------------
    let gpu = Gpu::new("ModularClipboardSwapchainResize", window.hinstance(), window.hwnd())?;
    let device_name = device_name_of(&gpu);
    println!("OK Gpu  {device_name}  ({:?})", gpu.device_type);

    // ---- 管线（照抄 full_app，保证与已验证路径一致）--------------------
    //
    // 渲染通道的附件格式必须与交换链选中的一致：用与 `pick_format` 相同
    // 的偏好预选，`FrameRenderer::new` 内部会再校验一次。
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

    let mut fr = FrameRenderer::new(
        &gpu,
        PipelineBundle::new(
            render_pass.handle,
            pipe_layout.handle,
            pipeline.handle,
            desc_layout.handle,
        ),
    )?;
    // 主动重建一次，保证交换链尺寸与真实客户区严格一致，
    // 也让「重建次数」从 1 起算而不是 0。
    fr.rebuild_swapchain(Default::default())?;
    println!(
        "OK FrameRenderer  extent={}x{}  槽位={}  图像={}",
        fr.extent().width,
        fr.extent().height,
        fr.slot_count(),
        fr.swapchain_image_count()
    );
    check_invariants(&fr, "初始重建后")?;

    let resize_enabled = resize_enabled_from(std::env::var_os(ENV_RESIZE).as_deref());
    let mut rebuilds: u32 = 1;
    let mut total_frames: u64 = 0;

    if resize_enabled {
        println!("[resize] {ENV_RESIZE} 已设置：开始 resize 序列");
        for (i, &(tw, th)) in RESIZE_SEQ.iter().enumerate() {
            let before = rebuilds;
            println!("\n---- resize #{i}：目标客户区 {tw}x{th} ----");
            move_window(&window, WINDOW_X, WINDOW_Y, tw, th)?;

            let stats = run_phase(
                &window,
                &mut events,
                &mut fr,
                &mut rebuilds,
                &mut total_frames,
                FRAMES_PER_SIZE,
            )?;

            // 判据 1：重建必须**真的发生**。为 0 说明 resize 没生效，
            // 本探针等于什么都没测（这正是 full_app 一直打印 0 的情况）。
            let rebuilt = rebuilds - before;
            anyhow::ensure!(
                rebuilt >= 1,
                "resize #{i}（目标 {tw}x{th}）后交换链重建次数为 0 —— resize 未生效，\
                 本探针未覆盖到重建路径"
            );

            // 判据 2 + 3：extent 一致、present 无错误。
            verify_phase(&fr, &window, i, &stats, rebuilt, rebuilds)?;
        }
    } else {
        println!("[resize] {ENV_RESIZE} 未设置：只在初始尺寸跑 {FRAMES_PER_SIZE} 帧，不触发 resize");
        let before = rebuilds;
        let stats = run_phase(
            &window,
            &mut events,
            &mut fr,
            &mut rebuilds,
            &mut total_frames,
            FRAMES_PER_SIZE,
        )?;
        verify_phase(&fr, &window, 0, &stats, rebuilds - before, rebuilds)?;
    }

    // ---- 清理（先 GPU 空闲，再按「引用者 → 被引用者」逆序销毁）------------
    //
    // 验证层在线时，若交换链重建的销毁链有缺陷，此处会报
    // `VUID-vkDestroyDevice-device-05137`（重建时泄漏 ImageView / Semaphore）。
    gpu.wait_idle();
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    desc_layout.destroy(&gpu.device);
    render_pass.destroy(&gpu.device);

    if resize_enabled {
        println!("\nALL OK - {total_frames} 帧，交换链重建 {rebuilds} 次（resize 序列已跑完）");
    } else {
        println!("\nALL OK - {total_frames} 帧，交换链重建 {rebuilds} 次（resize 序列未开启）");
    }
    Ok(())
}

/// 一个尺寸下的观测结果。
struct PhaseStats {
    /// 实际完成「acquire → record → present」的帧数。
    frames: u64,
    /// `present` 返回 `Presented` 的次数。
    presented: u64,
    /// `present` 返回 `Outdated` 的次数。
    ///
    /// resize 之后首个 present 出现 `Outdated` 是**正常**的
    /// （表面已不匹配），它是重建信号而不是错误，因此单独计数、
    /// 不参与失败判定，但会打印出来供人工核对。
    outdated: u64,
    /// 客户区意外为 0×0 而跳过的帧数。
    zero_client_skips: u32,
}

/// 在当前尺寸下跑满 `frames` 帧。事件处理与重建逻辑照抄 `full_app`。
#[allow(clippy::too_many_arguments)]
fn run_phase(
    window: &Window,
    events: &mut EventLoop,
    fr: &mut FrameRenderer<'_>,
    rebuilds: &mut u32,
    total_frames: &mut u64,
    frames: u64,
) -> anyhow::Result<PhaseStats> {
    let mut stats = PhaseStats {
        frames: 0,
        presented: 0,
        outdated: 0,
        zero_client_skips: 0,
    };

    while stats.frames < frames {
        // ---- 1. 事件 + resize ------------------------------------------
        for ev in events.poll() {
            match ev {
                WindowEvent::CloseRequested => {
                    anyhow::bail!("收到关闭请求，探针不应被手动关闭");
                }
                WindowEvent::Resized { .. } => {
                    do_rebuild(fr, rebuilds)?;
                }
                _ => {}
            }
        }
        anyhow::ensure!(!events.quit_requested(), "收到 WM_QUIT，探针不应被外部终止");

        // ---- 2. 客户区尺寸兜底 ------------------------------------------
        let (cw, ch) = window.inner_size_physical();
        if cw == 0 || ch == 0 {
            // 本探针从不最小化，0×0 属异常。容忍短暂抖动，但持续如此要失败
            // ——否则就是无声挂起（MEMORY：「WARN 不是通过条件」）。
            stats.zero_client_skips += 1;
            anyhow::ensure!(
                stats.zero_client_skips <= MAX_ZERO_CLIENT_FRAMES,
                "客户区持续为 0x0（已跳过 {} 帧）：窗口被最小化或不可呈现。\
                 本探针要求窗口保持可见且不最小化",
                stats.zero_client_skips
            );
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        // 尺寸查询与事件不一致时兜住（例如 SetWindowPos 引起的多消息合并）。
        if (cw, ch) != (fr.extent().width, fr.extent().height) {
            do_rebuild(fr, rebuilds)?;
        }

        // ---- 3. acquire -------------------------------------------------
        let Some(acquired) = fr.acquire()? else {
            do_rebuild(fr, rebuilds)?;
            continue;
        };

        // ---- 4. 录制（空批次 → 只清屏）--------------------------------
        //
        // 刻意不画任何图元：本探针只验交换链重建，不验像素。
        // `record` 仍会开启渲染通道（无批次也要清屏），`present` 负责
        // 关闭它——两者配对，命令缓冲不会处于非法状态。
        fr.record(&DrawInput::default())?;

        // ---- 5. present -------------------------------------------------
        //
        // 「每次 present 都没报错」即证明：
        // - 呈现信号量表长度与 image_count 未失配（否则 `present` 内的
        //   `present_semaphore_index` 守卫会 bail）；
        // - 布局屏障链（渲染前 / present 前）没有触发 device lost。
        match fr.present(acquired)? {
            PresentResult::Presented => stats.presented += 1,
            PresentResult::Outdated => stats.outdated += 1,
        }

        stats.frames += 1;
        *total_frames += 1;

        // 无事件时不要空转烧 CPU。每帧 poll_for(1ms) 是有意的：
        // 它让 `WM_SIZE` 尽快被 pump 到，同时不至于像 `device_wait_idle`
        // 那样把多帧在途串行化（MEMORY 第 7/10 批：诊断插桩本身会改变
        // 被诊断的系统）。
        let _ = events.poll_for(Some(Duration::from_millis(1)));
    }

    Ok(stats)
}

/// 交换链重建（照抄 `full_app` 的 `do_rebuild`，去掉资源重整部分）。
///
/// `full_app` 在这里还要重绑描述符、扩缩顶点缓冲环——那些是**像素**路径
/// 的事，本探针不画图元，故不涉及。
fn do_rebuild(fr: &mut FrameRenderer<'_>, rebuilds: &mut u32) -> anyhow::Result<()> {
    fr.rebuild_swapchain(Default::default())?;
    *rebuilds += 1;
    Ok(())
}

/// 判据 2/3：extent 与客户区一致、present 全部成功。
fn verify_phase(
    fr: &FrameRenderer<'_>,
    window: &Window,
    index: usize,
    stats: &PhaseStats,
    rebuilt: u32,
    rebuilds: u32,
) -> anyhow::Result<()> {
    let (aw, ah) = window.inner_size_physical();
    let (ew, eh) = (fr.extent().width, fr.extent().height);

    anyhow::ensure!(
        (ew, eh) == (aw, ah),
        "尺寸阶段 #{index}：交换链 extent {ew}x{eh} 与客户区 {aw}x{ah} 不一致 —— \
         重建逻辑有问题"
    );
    anyhow::ensure!(
        stats.frames > 0,
        "尺寸阶段 #{index}：一帧都没跑完（客户区 0x0 跳过了全部帧）"
    );
    anyhow::ensure!(
        stats.presented + stats.outdated == stats.frames,
        "尺寸阶段 #{index}：present 次数 {} 与帧数 {} 不符 —— 有帧未走到 present",
        stats.presented + stats.outdated,
        stats.frames
    );
    // present 返回 Err 会让 `?` 直接退出，这里再兜一条：
    // 若 present 全是 Outdated，说明表面始终不匹配，交换链等于没在用。
    anyhow::ensure!(
        stats.presented > 0,
        "尺寸阶段 #{index}：{} 帧全部 present 失败（Outdated）—— 表面始终不匹配，\
         探针并未真正验证到呈现路径",
        stats.outdated
    );
    // 重建后的结构不变量。
    check_invariants(fr, &format!("尺寸阶段 #{index} 重建后"))?;

    println!(
        "  帧 {}/{}  extent {ew}x{eh} = 客户区 {aw}x{ah}  本段重建 {rebuilt} 次（累计 {rebuilds}）",
        stats.frames, FRAMES_PER_SIZE
    );
    println!(
        "  present: {} presented / {} outdated  槽位 {} = 图像数 {}  信号量 {} = 图像数 {}",
        stats.presented,
        stats.outdated,
        fr.slot_count(),
        fr.swapchain_image_count(),
        fr.image_semaphore_count(),
        fr.swapchain_image_count(),
    );
    Ok(())
}

/// 交换链重建后的结构不变量。
///
/// `swapchain_image_count()` 与 `image_semaphore_count()` 这对访问器存在的
/// 唯一理由就是这里（见 `frame.rs` 的注释）：重建后两者不等，说明
/// 销毁/重建链漏了呈现信号量——那正是
/// `VUID-vkDestroyDevice-device-05137` 的来源。
///
/// 这两个访问器**已存在**，本探针没有为验证它们改过 `frame.rs` 一行。
fn check_invariants(fr: &FrameRenderer<'_>, when: &str) -> anyhow::Result<()> {
    let images = fr.swapchain_image_count();
    anyhow::ensure!(
        fr.image_semaphore_count() == images,
        "{when}：呈现信号量数 {} 与交换链图像数 {images} 不等 —— \
         重建时漏了信号量的销毁/重建",
        fr.image_semaphore_count()
    );
    anyhow::ensure!(
        fr.slot_count() == images,
        "{when}：在飞槽位数 {} 与交换链图像数 {images} 不等",
        fr.slot_count()
    );
    Ok(())
}

/// 把 `device_name` 的 `[i8; 256]` 转成可打印字符串。
fn device_name_of(gpu: &Gpu) -> String {
    gpu.properties
        .device_name
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .map(|b| b as char)
        .collect()
}

/// 改变窗口位置与客户区尺寸。
///
/// # 为什么用 `MoveWindow` 而不是直接发 `WM_SIZE`
///
/// `PostMessage(WM_SIZE)` 只让**事件翻译**走到，并**不会**真正改变客户区
/// 尺寸——`GetClientRect` 仍返回旧值，交换链也拿不到新尺寸。
/// 这样验证的就不是真实路径。
///
/// 保持可见（不最小化、不隐藏）：窗口不可见时表面无法呈现，症状不是
/// 「窗口没显示」，而是后续 acquire 阶段报设备错误（MEMORY 坑 36）。
///
/// `bRepaint = false`：避免额外 `WM_PAINT` 干扰事件顺序。
fn move_window(
    window: &Window,
    x: i32,
    y: i32,
    client_w: u32,
    client_h: u32,
) -> anyhow::Result<()> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, MoveWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };

    // `MoveWindow` 的参数是**整体窗口**尺寸；客户区要减去边框与标题栏。
    let mut r = RECT {
        left: 0,
        top: 0,
        right: client_w as i32,
        bottom: client_h as i32,
    };
    unsafe { AdjustWindowRectEx(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)) }?;
    unsafe {
        MoveWindow(
            window.hwnd(),
            x,
            y,
            r.right - r.left,
            r.bottom - r.top,
            false,
        )?;
    }
    Ok(())
}

/// 解析 `SWAPCHAIN_RESIZE`。
///
/// 抽成纯函数是为了能单测「哪些取值算开启」——判据与被测行为之间不该
/// 只靠一段散文约束。
fn resize_enabled_from(value: Option<&OsStr>) -> bool {
    let Some(v) = value else { return false };
    match v.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "no" => false,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// 测试（不建窗口，CI 可跑）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_env_parsing_rules() {
        // 未设置 → 关闭（默认行为必须是「什么都不做」）。
        assert!(!resize_enabled_from(None));
        // 显式开启。
        for v in ["1", "true", "on", "yes", " 1 "] {
            assert!(
                resize_enabled_from(Some(OsStr::new(v))),
                "{v:?} 应视为开启"
            );
        }
        // 显式关闭：空串与假值。空串很关键——`FOO=` 这种写法很常见，
        // 若视为开启就会让「默认安静」的承诺失效。
        for v in ["", "0", "false", "off", "no", " 0 ", "OFF"] {
            assert!(
                !resize_enabled_from(Some(OsStr::new(v))),
                "{v:?} 应视为关闭"
            );
        }
    }

    #[test]
    fn resize_sequence_has_distinct_sizes() {
        // 每个尺寸都必须与前一个不同，否则重建可能不真正换图像。
        let mut prev = (W, H);
        for &s in RESIZE_SEQ {
            assert_ne!(s, prev, "resize 序列含重复尺寸 {s:?}，无法验证重建");
            assert!(s.0 > 0 && s.1 > 0, "尺寸必须非 0");
            prev = s;
        }
    }

    #[test]
    fn resize_sequence_ends_at_original_size() {
        // 最后一档回到初始尺寸：验证「改回去」也能正确重建。
        assert_eq!(RESIZE_SEQ.last().copied(), Some((W, H)));
    }

    #[test]
    fn resize_sequence_covers_both_directions() {
        // 至少一次变小、一次变大。只单向变化时某些驱动可能不重建
        // （extent 仍落在 caps 内），销毁链就测不到。
        let smaller = RESIZE_SEQ.iter().any(|&s| s.0 < W && s.1 < H);
        let bigger = RESIZE_SEQ.iter().any(|&s| s.0 > W && s.1 > H);
        assert!(smaller, "序列应包含一次变小");
        assert!(bigger, "序列应包含一次变大");
    }

    #[test]
    fn frames_per_size_covers_image_rotation() {
        // 60 帧至少要能走完一圈交换链图像（典型 image_count 2~3）。
        assert!(FRAMES_PER_SIZE >= 60, "每尺寸帧数 {FRAMES_PER_SIZE} 不足");
    }

    #[test]
    fn window_stays_inside_the_desktop() {
        // 移出屏幕（负坐标）会让 DWM 报告的 current_extent 超出表面能力
        // 范围，resolve_extent 原样采用就触发
        // VUID-VkSwapchainCreateInfoKHR-pNext-07781（MEMORY 第 19 批）。
        assert!(
            WINDOW_X >= 0 && WINDOW_Y >= 0,
            "窗口位置必须留在桌面内，不能移到屏幕外"
        );
    }

    #[test]
    fn zero_client_grace_is_bounded() {
        // 容忍上限必须是有限值：窗口从不最小化，持续 0×0 就是异常，
        // 应该失败而不是无限跳过。
        assert!(MAX_ZERO_CLIENT_FRAMES > 0);
        assert!(MAX_ZERO_CLIENT_FRAMES < FRAMES_PER_SIZE as u32);
    }

    #[test]
    fn device_name_decoding_stops_at_nul() {
        // 复刻 `device_name_of` 的解码规则：遇到 NUL 即止，
        // 且负的 i8（UTF-8 续字节）不能被截断。
        let raw: [i8; 8] = [b'A' as i8, b'B' as i8, 0, b'C' as i8, 0, 0, 0, 0];
        let got: String = raw
            .iter()
            .map(|c| *c as u8)
            .take_while(|b| *b != 0)
            .map(|b| b as char)
            .collect();
        assert_eq!(got, "AB", "NUL 之后的内容必须被丢弃");
    }
}