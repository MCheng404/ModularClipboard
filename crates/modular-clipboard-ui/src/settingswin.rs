//! 设置界面：独立的顶层窗口。
//!
//! # 为什么独立成窗
//!
//! 设置面板原先是主窗口上的**覆盖层**：一块居中矩形 + 一层遮罩。
//! 那样做有四个问题：
//!
//! 1. **主窗口被压住**——设置只有 20 余项，弹层却要盖住整个主界面，
//!    用户改一个开关就没法顺便看历史；
//! 2. **遮罩吃掉主窗口点击**——弹层开着时主窗口完全不可交互；
//! 3. **尺寸被主窗口牵制**——主窗口很窄时设置面板挤成一团（实测
//!    600x400 下开关被排到客户区之外）；
//! 4. **不能挪到副屏**——改设置时想同时看着主界面做不到。
//!
//! # 实现
//!
//! 复用卡片子窗口那套机制（`Window` + 独立 `egui::Context` +
//! 独立交换链），差别只有三点：
//!
//! - 不绑定任何卡片（没有 `card_id`）；
//! - 画设置面板而不是卡片内容；
//! - 生命周期由 `UiState::show_settings` 决定，而不是 `Card::host`。

use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};

/// 设置窗口的标题（同时是类名）。
const TITLE: &str = "剪贴板设置";

/// 窗口默认尺寸（逻辑点）。
const DEFAULT_SIZE: (f32, f32) = (460.0, 560.0);

/// 设置窗口。
///
/// ⚠️ 与 [`super::childwin::ChildWindow`] 一样：每个窗口带**独立的
/// `egui::Context`**。共用一个 `Context` 会导致两个窗口的鼠标位置
/// 互相覆盖（都以为自己拿到焦点），键盘输入也会错投。
pub struct SettingsWindow<'a> {
    window: Window,
    gfx: crate::multiwindow::WindowGfx<'a>,
    events: EventLoop,
    ctx: egui::Context,
    /// 上一次调用 [`Self::sync_and_render`] 时的 `want_open`。
    was_open: bool,
    /// 收到关窗请求（标题栏 ✕）。
    close_requested: bool,
}

impl<'a> SettingsWindow<'a> {
    /// 创建设置窗口。
    ///
    /// `anchor` 是主窗口左上角的屏幕坐标（逻辑点），设置窗口摆在它
    /// 右边 —— 紧挨着而不是叠着，用户能同时看到两者。
    pub fn create(
        anchor: egui::epaint::emath::Vec2,
        scale_factor: f32,
        shared: &crate::multiwindow::SharedGfx<'a>,
    ) -> anyhow::Result<Self> {
        let s = if scale_factor > 0.01 { scale_factor } else { 1.0 };
        let window = Window::new(
            TITLE,
            (DEFAULT_SIZE.0 * s) as u32,
            (DEFAULT_SIZE.1 * s) as u32,
        )?;
        // `Window::new` 收物理像素，`anchor` 是逻辑点，要乘 `s`。
        // 中间留 12pt 缝，两窗口贴一起会显得像同一块面板。
        window.move_to(
            ((anchor.x + DEFAULT_SIZE.0 + 12.0) * s) as i32,
            (anchor.y * s) as i32,
        );

        // ⚠️ 同上：走 `new_context`，字体配置与主窗口一致。
        let ctx = shared.new_context();

        // 必须用本窗口自己的表面：用主窗口那个会报
        // `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`。
        let surface = shared
            .gpu
            .create_surface_for(window.hinstance(), window.hwnd())
            .map_err(|e| anyhow::anyhow!("创建设置窗口表面失败: {e}"))?;
        let gfx = crate::multiwindow::WindowGfx::new_for(shared, surface)
            .map_err(|e| anyhow::anyhow!("设置窗口交换链创建失败: {e}"))?;
        let events = EventLoop::new(&window);

        Ok(Self {
            window,
            gfx,
            events,
            ctx,
            was_open: false,
            close_requested: false,
        })
    }

    /// 按 `want_open` 显示/隐藏，并在需要时绘制一帧。
    ///
    /// 返回 `true` 表示用户关掉了窗口（标题栏 ✕），调用方**必须**
    /// 据此把 `UiState::show_settings` 置回 `false` —— 否则窗口藏起来
    /// 了而状态还说「开着」，用户再点齿轮就会「没反应」。
    pub fn sync_and_render(
        &mut self,
        app: &mut crate::app::App,
        want_open: bool,
    ) -> anyhow::Result<bool> {
        let just_opened = !self.was_open && want_open;
        let just_closed = self.was_open && !want_open;
        self.was_open = want_open;

        // 隐藏态：poll 消息、**排空**事件，不渲染。
        //
        // ⚠️ 两件事都要做，缺一不可：
        //
        // 1. `poll_for` —— 驱动窗口过程，让系统把窗口画出来/响应 resize；
        // 2. `take_pending` —— **排空**积压的事件。
        //
        // 只做 1 不做 2（早前的写法）会让事件在 `EventLoop::pending`
        // 里无限累积：窗口关闭期间的所有鼠标/键盘都堆着，下次刚打开
        // 就一次性回放 ⇒ 界面「一闪就没」或「点齿轮没反应」。
        // 这是最容易被误判成「窗口创建失败」的一类 bug。
        if !want_open {
            self.events.poll_for(None);
            self.events.take_pending();
            // `just_closed` 这一帧要把待处理的关窗请求清掉：它是
            // 「关闭瞬间」才收到的，若不清，下一次打开会立刻被判关闭。
            if just_closed {
                self.close_requested = false;
            }
            crate::presence::hide_window(self.window.hwnd());
            return Ok(false);
        }

        if just_opened {
            crate::presence::focus_window(self.window.hwnd());
            // 新开时滚动归零：上次停在半截的设置项不该出现在顶部。
            //
            // ⚠️ 必须用**本窗口的** `self.ctx`，不能用 `app.ctx`。
            // `ScrollArea` 的滚动位置存在它所属 `Context` 的内存里，
            // 清主窗口的 Context 对设置窗口毫无作用——早前就是这么
            // 写的，等于没写。
            self.ctx
                .memory_mut(|m| m.data.remove::<egui::Id>(egui::Id::new("settings_scroll")));
        }

        // 客户区 0×0（最小化）时不能 acquire：交换链拿不到可呈现图像。
        let (cw, ch) = self.window.inner_size_physical();
        if cw == 0 || ch == 0 {
            self.events.poll_for(None);
            self.events.take_pending();
            return Ok(false);
        }

        let frame_events = self.events.take_pending();
        if frame_events.iter().any(|e| matches!(e, WindowEvent::CloseRequested)) {
            self.close_requested = true;
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        // 绘制委托给 `App` 的方法：把私有字段访问收敛在 app.rs 内，
        // 本模块不需要知道 `App` 的内部结构。
        let mut out = self.ctx.run_ui(raw_input, |ui| app.draw_settings_window(ui));
        let ppp = out.pixels_per_point;
        let mut textures_delta = std::mem::take(&mut out.textures_delta);
        let primitives = self.ctx.tessellate(std::mem::take(&mut out.shapes), ppp);
        drop(out);

        // 尺寸变了先重建，否则会用旧 framebuffer 呈现。
        self.gfx.rebuild_if_needed((cw, ch))?;

        let Some(acquired) = self.gfx.fr.acquire()? else {
            // 拿不到图像（正在最小化/被遮挡）⇒ 重建交换链，下帧重试。
            self.gfx.fr.rebuild_swapchain(Default::default())?;
            return Ok(false);
        };
        self.gfx.painter.paint(
            &mut self.gfx.fr,
            &primitives,
            ppp,
            &mut textures_delta,
        )?;
        let _ = self.gfx.fr.present(acquired)?;
        // ⚠️ 必须 clear：`TexturesDelta` 的 `Drop` 有debug 断言
        // 「未应用的 delta 必须先clear」，否则 panic 且报错与真实
        // 原因毫无关系。
        textures_delta.clear();

        let requested = self.close_requested;
        self.close_requested = false;
        Ok(requested)
    }

    /// 退出前隐藏窗口。
    pub fn close(&mut self) {
        crate::presence::hide_window(self.window.hwnd());
    }
}