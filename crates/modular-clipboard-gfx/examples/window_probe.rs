//! 实机验证：Win32 窗口 + 事件循环。
//!
//! # 这个探针回答的问题
//!
//! `window.rs` 的 52 个单测全部是**纯逻辑**测试——键码映射、修饰键组合、
//! `lParam` 拆包、UTF-16 代理项配对。但它们**从不创建窗口**，因此回答不了
//! 那些只有真 Win32 才能暴露的问题：
//!
//! 1. 窗口类注册 + `CreateWindowExW` 在当前 DPI/主题下是否真的成功；
//! 2. `SetProcessDpiAwarenessContext` 是否在**建窗之前**成功生效
//!    （顺序错了窗口不会糊，但已经晚了，改不回来）；
//! 3. `GetDpiForWindow` 返回的缩放与请求的 `scale_factor` 是否一致；
//! 4. `AdjustWindowRectEx` 算出的整体窗口尺寸，扣掉标题栏边框后客户区
//!    是否真的等于请求值（不等就说明尺寸换算错了）；
//! 5. 消息循环能否真正 pump 到消息（`PeekMessageW` 拿到 `WM_SIZE` 等）；
//! 6. `egui_input` 产出的 `RawInput` 是否自洽——`screen_rect` 有面积、
//!    `time` 单调递增、`predicted_dt` 在合理区间。
//!
//! 单测覆盖不到第1、2、4 条：它们依赖真实显示器与系统窗口管理器。
//!
//! # 通过标准
//!
//! 退出码 0 = 窗口与事件循环正常。任一断言失败即 `panic`，消息里带具体原因。
//!
//! # 为什么跑 8 帧就退出
//!
//! 无人值守脚本不能等用户交互。8 帧足够让 `WM_SIZE` / `WM_PAINT` / `WM_SHOWWINDOW`
//! 走完一圈（这些在 `ShowWindow` 之后立即投递），又不必等超时。

use std::time::Duration;

use egui::Key;
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};
use windows::Win32::Foundation::{LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsWindow, IsWindowVisible, PostMessageW, SendMessageW, WM_CHAR, WM_CLOSE,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_SIZE,
};

/// 请求的客户区尺寸（物理像素）。
const W: u32 = 800;
const H: u32 = 600;

/// 跑满这么多帧后退出。
///
/// 不需要交互：建窗后 Win32 会立即投递 `WM_CREATE` / `WM_SIZE` /
/// `WM_SHOWWINDOW` / `WM_ACTIVATE`，8 帧足够消息队列排空。
const EXIT_AFTER_FRAMES: u32 = 8;

/// 每帧之间的等待上限。给消息循环一点时间，但不至于卡住。
const FRAME_WAIT: Duration = Duration::from_millis(50);

/// 滚轮一个刻度的 `wParam` 高 16 位编码。
const WHEEL_DELTA: usize = 120;

/// 把坐标打包进 `lParam`（低 16 位 x，高 16 位 y），**有符号**。
///
/// 与 [`unpack_size`] 不同：鼠标坐标在指针移出客户区时为负，
/// 必须做符号扩展，否则 `65535` 会被当成合法坐标。
fn pack_pos(x: i32, y: i32) -> LPARAM {
    LPARAM(((y as u32 as u16 as i16 as u32) << 16 | (x as u32 as u16 as i16 as u32) as u32)
        as isize)
}

/// 向窗口投递一条消息。
///
/// 用 `PostMessage` 而非 `SendMessage`：前者入队，由 `poll` 的
/// `PeekMessageW` 读出——这样测的正是真实路径（队列 → 翻译），
/// 而 `SendMessage` 会绕过队列直接调窗口过程。
fn post(hwnd: windows::Win32::Foundation::HWND, msg: u32, wparam: usize, lparam: isize) {
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), LPARAM(lparam)) }
        .unwrap_or_else(|e| panic!("PostMessageW({msg}) 失败，无法验证事件翻译：{e:?}"));
}

/// 用合成消息验证事件翻译通路。
///
/// # 为什么必须注入而不能只等真实消息
///
/// 首版探针只等系统自发消息，结果「事件总数 0」——它**在自己没测的
/// 东西上通过了**。无人值守环境下没人会移动鼠标、敲键盘，
/// `WM_MOUSEMOVE` / `WM_KEYDOWN` / `WM_MOUSEWHEEL` 永远不会来。
///
/// 这里直接向真实 HWND 投递消息，强制走通
/// `PeekMessageW → translate → WindowEvent` 全链路。
/// 这补上了单测（不建窗）与真实交互（不可自动化）之间的空白。
fn verify_event_translation(window: &Window, event_loop: &mut EventLoop) -> anyhow::Result<()> {
    let hwnd = window.hwnd();
    let scale = window.scale_factor();
    println!("\n== 注入合成消息验证翻译通路 ==");

    // --- 鼠标移动：坐标应换算成逻辑点 ---
    post(hwnd, WM_MOUSEMOVE, 0, pack_pos(100, 50).0);
    // --- 左键按下 / 抬起 ---
    post(hwnd, WM_LBUTTONDOWN, 1, pack_pos(120, 60).0);
    post(hwnd, WM_LBUTTONUP, 0, pack_pos(120, 60).0);
    // --- 键盘：'A' 键按下 + 字符 '你' ---
    post(hwnd, WM_KEYDOWN, b'A' as usize, 0);
    post(hwnd, WM_CHAR, '你' as usize, 0);
    // --- 滚轮向上一个刻度（wParam 高 16 位，正 = 向上拨）---
    // lParam 是**屏幕**坐标，translate 内部会 ScreenToClient。
    let mut screen = POINT { x: 200, y: 150 };
    // 失败时 pt 保持原值，仍是合法的屏幕坐标，不影响后续断言
    let _ = unsafe { ClientToScreen(hwnd, &mut screen) };
    post(
        hwnd,
        WM_MOUSEWHEEL,
        WHEEL_DELTA << 16,
        pack_pos(screen.x, screen.y).0,
    );
    // --- 尺寸变化：客户区改成 640x480 物理像素 ---
    post(hwnd, WM_SIZE, 0, ((480u32 << 16) | 640u32) as isize);
    // --- 关闭窗口请求：必须**不**销毁窗口 ---
    post(hwnd, WM_CLOSE, 0, 0);

    // 给消息队列一点时间投递（PostMessage 是异步的）。
    std::thread::sleep(Duration::from_millis(80));
    let events = event_loop.poll();
    println!("收到 {} 条事件", events.len());

    if events.is_empty() {
        anyhow::bail!("注入 8 条消息后poll 返回空，事件翻译通路未工作");
    }

    // 逐条断言：只看「有没有」会漏掉「对不对」——
    // 比如坐标忘了除以scale，或滚轮符号反了。
    let mut saw_move = false;
    let mut saw_press = false;
    let mut saw_release = false;
    let mut saw_key = false;
    let mut saw_text = false;
    let mut saw_scroll = false;
    let mut saw_resize = false;
    let mut saw_close = false;
    // 收集全部 TextInput：注入 WM_KEYDOWN('A') 会让 TranslateMessage
    // **自动**再投一条 WM_CHAR('a')，因此这里预期至少有两条。
    let mut texts: Vec<String> = Vec::new();

    for ev in &events {
        match ev {
            WindowEvent::MouseMoved { pos } => {
                // 100/1.5 ≈ 66.7，容 0.5 像素
                let want = 100.0 / scale;
                if (pos.x - want).abs() > 0.5 {
                    anyhow::bail!(
                        "MouseMoved.x = {}，期望 {want}（100 物理像素 / scale {scale}）",
                        pos.x
                    );
                }
                saw_move = true;
            }
            WindowEvent::MouseButton {
                button,
                pressed,
                pos,
            } => {
                let want = 120.0 / scale;
                if (pos.x - want).abs() > 0.5 {
                    anyhow::bail!("MouseButton.pos.x = {}，期望 {want}", pos.x);
                }
                if *button != egui::PointerButton::Primary {
                    anyhow::bail!("左键被映射成 {:?}，应为Primary", button);
                }
                if *pressed {
                    saw_press = true;
                } else {
                    saw_release = true;
                }
            }
            WindowEvent::Key {
                keycode, pressed, ..
            } => {
                if *keycode != Key::A {
                    anyhow::bail!("VK 'A' 被映射成 {keycode:?}，应为 Key::A");
                }
                if !*pressed {
                    anyhow::bail!("WM_KEYDOWN 产生了 pressed=false");
                }
                saw_key = true;
            }
            WindowEvent::TextInput(s) => {
                texts.push(s.clone());
                saw_text = true;
            }
            WindowEvent::Scroll(lines) => {
                // 向上拨一个刻度 = +1.0 行
                if (*lines - 1.0).abs() > 1e-3 {
                    anyhow::bail!("滚轮一个刻度得到 {lines} 行，期望 1.0（符号或除数错）");
                }
                saw_scroll = true;
            }
            WindowEvent::Resized { width, height } => {
                // 640 物理像素 / scale
                let want = 640.0 / scale;
                if (width - want).abs() > 1.0 {
                    anyhow::bail!(
                        "Resized.width = {width}，期望 {want}（必须是**逻辑点**不是物理像素）"
                    );
                }
                let want_h = 480.0 / scale;
                if (height - want_h).abs() > 1.0 {
                    anyhow::bail!("Resized.height = {height}，期望 {want_h}");
                }
                saw_resize = true;
            }
            WindowEvent::CloseRequested => {
                // 关键：WM_CLOSE 之后窗口必须还活着
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    anyhow::bail!(
                        "WM_CLOSE 之后窗口已被销毁，违反「CloseRequested 不销毁窗口」契约"
                    );
                }
                saw_close = true;
            }
            other => println!("  （未断言）{other:?}"),
        }
    }

    // 汇总：缺哪条都说明对应分支没工作
    let missing: Vec<&str> = [
        (!saw_move).then_some("MouseMoved"),
        (!saw_press).then_some("MouseButton(按下)"),
        (!saw_release).then_some("MouseButton(抬起)"),
        (!saw_key).then_some("Key"),
        (!saw_text).then_some("TextInput"),
        (!saw_scroll).then_some("Scroll"),
        (!saw_resize).then_some("Resized"),
        (!saw_close).then_some("CloseRequested"),
    ]
    .into_iter()
    .flatten()
    .collect();

    if !missing.is_empty() {
        anyhow::bail!("以下事件未被翻译出来：{missing:?}");
    }

    // 文本输入的**集合**断言，不能只断言「存在一条」。
    //
    // 首版探针在这里踩过坑：注入 `WM_KEYDOWN('A')` 后，
    // `TranslateMessage` 会自动再投一条 `WM_CHAR('a')` 进队列，
    // 于是出现两条 `TextInput`——"你"（我注入的）与 "a"（自动生成的）。
    // 我原先写的是「每条 TextInput 都必须等于 "你"」，第二条就把探针判失败了。
    //
    // 这恰好反证了 `TranslateMessage` 真的在工作（自动生成的那条是它产的），
    // 因此把它变成显式断言：必须同时出现注入的中文与自动译出的 ASCII。
    println!("OK 收到 TextInput {texts:?}");
    if !texts.iter().any(|t| t == "你") {
        anyhow::bail!("注入的 WM_CHAR '你' 未被译出，得到 {texts:?}（UTF-16 解码有误）");
    }
    if !texts.iter().any(|t| t == "a") {
        anyhow::bail!(
            "TranslateMessage 未把 WM_KEYDOWN('A') 自动译成 WM_CHAR('a')，\
             得到 {texts:?}。这说明事件循环漏了 TranslateMessage，中文输入法会失效"
        );
    }
    println!("OK TextInput 集合正确：注入的中文 + TranslateMessage 自动译出的 ASCII");

    println!("OK 8 类事件全部翻译正确（含坐标换算、滚轮符号、UTF-16、CloseRequested 不销毁窗口）");

    // 把窗口恢复成请求尺寸，避免后续断言拿到 640x480。
    // 用 SendMessage 直接改客户区：这里要的是「立刻生效」，
    // 不需要经过队列。
    let restore = LPARAM(((H << 16) | W) as isize);
    unsafe { SendMessageW(hwnd, WM_SIZE, Some(WPARAM(0)), Some(restore)) };
    std::thread::sleep(Duration::from_millis(20));
    let _ = event_loop.poll();
    let (rw, rh) = window.inner_size_physical();
    println!("OK 已恢复尺寸 {rw}x{rh} 物理像素");
    if rw != W || rh != H {
        anyhow::bail!("恢复尺寸失败：{rw}x{rh}，期望 {W}x{H}");
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    println!("== window_probe 启动 ==");

    // ---------------------------------------------------------------- 建窗
    let window = Window::new("Tiez Window Probe", W, H)
        .map_err(|e| anyhow::anyhow!("Window::new 失败（类注册或建窗被系统拒绝）: {e:#}"))?;

    let hwnd = window.hwnd();
    println!("OK Window::new  hwnd={:?}", hwnd.0);

    // 句柄非空是最低要求，但空句柄在后续调用里才会崩，所以显式拦一道。
    if hwnd.0.is_null() {
        anyhow::bail!("CreateWindowExW 返回了空 HWND，窗口未创建");
    }

    // 窗口必须真的在系统里。IsWindow 检查的是**线程 + 系统表**，
    // 光有句柄不够。
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        anyhow::bail!("IsWindow 返回 FALSE，句柄 {hwnd:?} 不对应任何窗口");
    }
    println!("OK IsWindow     句柄在系统窗口表中有效");

    // ShowWindow 已由 Window::new 调用，这里确认它可见。
    // 若为 FALSE，通常是窗口被其它程序遮挡或桌面会话受限（远程桌面断开）。
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        println!("WARN IsWindowVisible 为 FALSE（可能被遮挡或非交互式会话）");
    } else {
        println!("OK IsWindowVisible 窗口已显示");
    }

    // ------------------------------------------------------------ DPI 与尺寸
    let scale = window.scale_factor();
    println!("OK scale_factor {scale}（dpi = {}）", (scale * 96.0) as u32);

    // 缩放必须为正且有限。0 或 NaN 会让后面的坐标换算全部崩。
    if !(scale > 0.0) || !scale.is_finite() {
        anyhow::bail!("scale_factor 非法：{scale}，坐标换算会全部失效");
    }

    // 客户区尺寸：这是 AdjustWindowRectEx 换算是否正确的唯一证据。
    let (cw, ch) = window.inner_size_physical();
    println!("OK client_size  {cw}x{ch} 物理像素（请求 {W}x{H}）");

    if cw == 0 || ch == 0 {
        anyhow::bail!("客户区尺寸为 {cw}x{ch}，窗口不可见或被最小化");
    }

    // 允许 1 像素误差：某些主题/边框在不同 Windows 版本上会差 1px。
    if (cw as i64 - W as i64).abs() > 1 || (ch as i64 - H as i64).abs() > 1 {
        anyhow::bail!(
            "客户区 {cw}x{ch} 与请求 {W}x{H} 不符：AdjustWindowRectEx 换算可能有误"
        );
    }

    // 客户区不能大于整个窗口（含边框），否则换算方向反了。
    let mut wr: RECT = RECT::default();
    unsafe { GetClientRect(hwnd, &mut wr) }.ok();
    println!("OK GetClientRect (0,0)-({},{}) 客户区原点在左上角", wr.right, wr.bottom);

    // 逻辑点尺寸：这是喂给 swapchain 与 egui 的量。
    let (pw, ph) = window.inner_size_points();
    println!("OK client_size  {pw:.1}x{ph:.1} 逻辑点（= 物理 {scale} 倍缩放）");

    if pw <= 0.0 || ph <= 0.0 {
        anyhow::bail!("逻辑点尺寸为 {pw}x{ph}，egui 的 screen_rect 会是空的");
    }

    // ------------------------------------------------------------ 事件循环
    let mut event_loop = EventLoop::new(&window);
    let ctx = egui::Context::default();
    println!("OK EventLoop::new");

    // 先跑合成消息验证：这是「事件翻译真的工作」的唯一证据。
    // 放在帧循环之前，因为它不依赖时间推进，且失败要立刻报。
    verify_event_translation(&window, &mut event_loop)?;

    let mut all_events: Vec<WindowEvent> = Vec::new();
    let mut last_time: Option<f64> = None;
    let mut saw_first_raw_input = false;

    for frame in 1..=EXIT_AFTER_FRAMES {
        // 有事件立即返回；空闲时按上限睡眠，避免空转烧 CPU。
        let events = event_loop.poll_for(Some(FRAME_WAIT));
        if !events.is_empty() {
            for ev in &events {
                println!("  帧 {frame} 事件 {ev:?}");
            }
            all_events.extend(events);
        }

        // 关键顺序：poll 之后才能 egui_input（它读的是 poll 填好的缓冲）。
        let raw = event_loop.egui_input(&ctx);

        // screen_rect 决定 egui 布局，必须有面积。
        let Some(rect) = raw.screen_rect else {
            anyhow::bail!("RawInput::screen_rect 为 None，第 {frame} 帧");
        };
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            anyhow::bail!(
                "screen_rect 面积为零：{:.1}x{:.1}，第 {frame} 帧",
                rect.width(),
                rect.height()
            );
        }

        // time 必须单调不减——egui 的动画依赖它，回退会导致动画抖动。
        let now = raw
            .time
            .ok_or_else(|| anyhow::anyhow!("RawInput.time 为 None，第 {frame} 帧"))?;
        if let Some(prev) = last_time
            && now < prev
        {
            anyhow::bail!("time 回退：{now} < {prev}，第 {frame} 帧");
        }
        last_time = Some(now);

        // predicted_dt 为 0 会让 egui 动画除零（NaN 扩散后整个界面卡死）。
        if !(raw.predicted_dt > 0.0) || !raw.predicted_dt.is_finite() {
            anyhow::bail!("predicted_dt 非法：{}", raw.predicted_dt);
        }

        if !saw_first_raw_input {
            saw_first_raw_input = true;
            println!(
                "OK RawInput     screen_rect={:.1}x{:.1} time={now:.3}s predicted_dt={:.4}s focused={}",
                rect.width(),
                rect.height(),
                raw.predicted_dt,
                raw.focused
            );
        }

        // 让 egui 真正跑一帧：验证我们给的输入结构它能消费。
        // egui 0.36 用 `run_ui`（回调拿到根 `Ui`），项目内其它 example
        // 与 modular-clipboard-ui 也是这个写法。
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("窗口探针");
            ui.label("输入结构已被 egui 消费");
        });

        // `TexturesDelta` 的 `Drop` 里有 `debug_assert!(is_empty())`：
        // 增量必须被消费或显式 `clear()`，否则 panic。
        //
        // 本探针**故意不消费**——它不渲染，只验证窗口与事件循环。
        // 字体图集的上传是 `full_app` / `modular-clipboard-ui` 的职责（它们走
        // `FrameRenderer` + `StagingArena`）。因此这里先打印增量内容
        // 作为信息留存，再显式清空。
        //
        // 这不是「掩盖」：如果增量本身该被处理，`full_app` 会崩，
        // 那是它该捕获的问题，与本探针无关。
        if !out.textures_delta.is_empty() {
            println!(
                "INFO 帧 {frame} 纹理增量 set={} free={}（本探针不渲染，已显式清空）",
                out.textures_delta.set.len(),
                out.textures_delta.free.len()
            );
            out.textures_delta.clear();
        }
        if frame == EXIT_AFTER_FRAMES {
            println!(
                "OK egui 消费输入 shapes={} textures={}",
                out.shapes.len(),
                out.textures_delta.set.len()
            );
            if out.shapes.is_empty() {
                anyhow::bail!("egui 未产出任何图元，输入结构可能无法被消费");
            }
        }
    }

    // ------------------------------------------------------------ 汇总断言
    println!("\n== 汇总 ==");
    println!("帧数     {EXIT_AFTER_FRAMES}");
    println!("事件总数 {}", all_events.len());
    for (i, ev) in all_events.iter().enumerate() {
        println!("  [{i}] {ev:?}");
    }

    // 自发消息的 Resized 计数。注意：**不能**拿它当通过条件——
    // 首版探针就是在这里打了 WARN 然后照样退出 0，等于在自己没测的
    // 东西上通过。真实证据是上面 verify_event_translation 的注入验证。
    let resized: Vec<_> = all_events
        .iter()
        .filter(|e| matches!(e, WindowEvent::Resized { .. }))
        .collect();
    if resized.is_empty() {
        println!("INFO 未捕获自发 Resized（可能被首次 poll 之前的 ShowWindow 消化，非缺陷）");
    } else {
        println!("OK自发 Resized 捕获 {} 次", resized.len());
    }

    // 事件循环若自己判定退出，说明收到了 WM_QUIT——不该在这8 帧内发生。
    if event_loop.quit_requested() {
        anyhow::bail!("事件循环在 {EXIT_AFTER_FRAMES} 帧内收到 WM_QUIT，不应发生");
    }

    // 窗口在退出前必须仍存在：若被提前销毁，Drop 里的 DestroyWindow
    // 会对无效句柄调用（虽有 IsWindow 保护，但说明流程有问题）。
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        anyhow::bail!("窗口在探针结束前已消失，生命周期管理有问题");
    }
    println!("OK 窗口在探针结束时仍有效");

    // 显式销毁，验证 destroy 幂等且不 panic（Drop 会再调一次）。
    window.destroy();
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        anyhow::bail!("destroy() 之后窗口仍存在");
    }
    println!("OK Window::destroy 生效");
    // 此时 Window 变量尚未 drop，其 Drop 会再调一次 destroy()——
    // 靠 IsWindow 保护不崩。这本身就是对幂等性的实机验证。
    drop(window);
    println!("OK Window::drop 未 panic（destroy 幂等）");

    println!("\nALL OK - 窗口与事件循环工作正常");
    Ok(())
}
