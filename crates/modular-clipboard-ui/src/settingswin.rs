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
///
/// 定稿布局：左侧导航（搜索框 + 圆角导航块）+ 右侧卡片内容。
/// 宽度 780 容纳「常规/外观/捕获/存储/快捷键/关于」六分类导航
/// 与右侧完整卡片；固定大小（无 resize 逻辑）。
const DEFAULT_SIZE: (f32, f32) = (780.0, 560.0);

/// 设置窗口。
///
/// NOTE: 与 [`super::childwin::ChildWindow`] 一样：每个窗口带**独立的
/// `egui::Context`**。共用一个 `Context` 会导致两个窗口的鼠标位置
/// 互相覆盖（都以为自己拿到焦点），键盘输入也会错投。
pub struct SettingsWindow<'a> {
    /// 本窗口的 Vulkan 表面句柄（须自己留一份，理由见 `ChildWindow`）。
    surface: ash::vk::SurfaceKHR,
    window: Window,
    gfx: crate::multiwindow::WindowGfx<'a>,
    events: EventLoop,
    ctx: egui::Context,
    /// 上一次调用 [`Self::sync_and_render`] 时的 `want_open`。
    was_open: bool,
    /// 收到关窗请求（标题栏 ✕）。
    close_requested: bool,
    /// 已应用到 Win32 的置顶状态（幂等去重，见 [`Self::apply_topmost`]）。
    topmost: bool,
}

impl<'a> SettingsWindow<'a> {
    /// 创建设置窗口。
    ///
    /// `anchor` 是主窗口左上角的屏幕坐标（逻辑点），`main_w` 是主窗口
    /// 宽度（逻辑点）。设置窗口紧贴**主窗口右缘**摆放（中间留 12pt
    /// 缝）——主窗口居中时也能两窗并排可见；右侧放不下再回退到左侧。
    pub fn create(
        anchor: egui::epaint::emath::Vec2,
        main_w: f32,
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
        // 设置窗口左缘 = 主窗口右缘 + 12pt 缝：两窗并排紧贴。
        // 早期实现是 `anchor.x + 780 + 12`（从主窗口左缘算起），
        // 主窗口居中时右侧放不下、左侧也没空间，设置窗口被钳到
        // 屏幕右缘——右侧内容贴屏幕边缘，观感上「内容文字位置
        // 不对」（2026-10-11 用户反馈）。
        let (mut x, y) = (
            ((anchor.x + main_w + 12.0) * s) as i32,
            (anchor.y * s) as i32,
        );
        let (cw, ch) = ((DEFAULT_SIZE.0 * s) as i32, (DEFAULT_SIZE.1 * s) as i32);
        // 右侧放不下时（主窗口被「记住位置」恢复到屏幕右侧），
        // 改为摆到**主窗口左侧**：两窗并排都可见，不重叠、不贴屏幕
        // 边缘。左侧也没空间时才退回整体 clamp。
        let (wl, _wt, wr, _wb) = crate::presence::work_area_of(window.hwnd());
        let right_x = x + cw;
        let left_x = ((anchor.x - DEFAULT_SIZE.0 - 12.0) * s) as i32;
        if right_x > wr && left_x >= wl {
            x = left_x;
        }
        // 兜底：仍超出工作区就钳制到工作区内（含下缘，防任务栏遮挡）。
        let (x, y) = crate::presence::clamp_to_work_area(window.hwnd(), x, y, cw, ch);
        tracing::info!(
            anchor_x = anchor.x,
            anchor_y = anchor.y,
            main_w,
            scale = s,
            right_x,
            left_x,
            work_right = wr,
            final_x = x,
            final_y = y,
            "设置窗口定位"
        );
        window.move_to(x, y);

        // NOTE: 同上：走 `new_context`，字体配置与主窗口一致。
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
            surface,
            window,
            gfx,
            events,
            ctx,
            was_open: false,
            close_requested: false,
            topmost: false,
        })
    }

    /// 应用置顶状态。幂等：状态没变就不调 `SetWindowPos`。
    ///
    /// 主窗口开着「总是置顶」时设置窗口必须跟着置顶，否则切走焦点后
    /// 设置窗口会被置顶的主窗口或其它置顶窗口压到下面。
    pub fn apply_topmost(&mut self, on: bool) {
        if self.topmost != on {
            self.topmost = on;
            crate::presence::set_topmost(self.window.hwnd(), on);
        }
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
        // CAUTION: 两件事都要做，缺一不可：
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
            // 先确保 Z 序在置顶组最上（即使抢焦点失败也不被主窗口遮），
            // 再尝试把前台焦点给设置窗口。
            crate::presence::raise_to_top(self.window.hwnd());
            crate::presence::focus_window(self.window.hwnd());
            // 新开时滚动归零：上次停在半截的设置项不该出现在顶部。
            //
            // CAUTION: 必须用**本窗口的** `self.ctx`，不能用 `app.ctx`。
            // `ScrollArea` 的滚动位置存在它所属 `Context` 的内存里，
            // 清主窗口的 Context 对设置窗口毫无作用——早前就是这么
            // 写的，等于没写。
            self.ctx
                .memory_mut(|m| m.data.remove::<egui::Id>(egui::Id::new("settings_scroll")));
            // 设置界面状态归零：每次打开都从「常规」分类、空搜索、
            // 非最近修改视图开始。
            //
            // 实测（2026-10-11）同一样例 exe 两次启动，首次打开时
            // 曾出现「外观」分类被选中——来源是打开瞬间被排空的
            // 积压输入里混入了落在导航项上的点击（窗口刚创建、
            // 位置/焦点尚未稳定）。这里显式复位三个状态位，配合
            // 下面「首帧丢弃积压输入」，任何竞态/残留都不可能让
            // 「刚打开的窗口」跳到别的分类。
            app.reset_settings_view();
        }

        // 设置窗口的拖动区：客户区顶部标题行（逻辑点、客户区坐标系）。
        //
        // 无边框窗口没有系统标题栏，**不设拖动区就无法拖动窗口**；
        // 主窗口的拖动区由 `app.drag_regions()` 每帧上报，设置窗口
        // 没有等价入口，必须在这里自己设。
        //
        // 新布局下拖动区要**挖掉两处**：
        // 1. 左侧搜索列（搜索框可点击，不能变成 `HTCAPTION`）；
        // 2. 右上角关闭按钮（否则点击变拖动、按钮点不动）。
        // 中间段整条都是拖动区——右侧内容区顶部（卡片标题之上）
        // 不承载可点控件，拖动窗口时不会被误触发。
        //
        // 每帧调用：`set_drag_regions` 只改内存里的小 Vec，无系统调用；
        // 只在创建时设一次的话，窗口 resize 后矩形就对不上了。
        let (cw, ch) = self.window.inner_size_points();
        let title_h = 44.0f32;
        let close_zone_w = 40.0f32;
        // 左挖到搜索列右缘：与 `paint::draw_settings` 的布局常量一致。
        let drag_left = crate::paint::SETTINGS_NAV_W + crate::paint::SETTINGS_MARGIN;
        if cw > drag_left + close_zone_w && ch > 0.0 {
            self.window.set_drag_region(Some(egui::Rect::from_min_max(
                egui::pos2(drag_left, 0.0),
                egui::pos2(cw - close_zone_w, title_h.min(ch)),
            )));
        } else {
            self.window.set_drag_region(None);
        }

        // 客户区 0×0（最小化）时不能 acquire：交换链拿不到可呈现图像。
        let (cw, ch) = self.window.inner_size_physical();
        if cw == 0 || ch == 0 {
            self.events.poll_for(None);
            self.events.take_pending();
            return Ok(false);
        }

        let frame_events = if just_opened {
            // 打开**首帧**把积压输入整个丢弃（不传进 egui）。
            //
            // 实测（2026-10-11）：同一样例 exe 连续两次启动，首次
            // 打开设置窗口偶发「外观」分类被选中——窗口刚创建、位置
            // 与焦点尚未稳定时，系统队列里可能混入落在导航项上的
            // 点击/移动（上一窗口激活、焦点抢夺产生的残留）。若把
            // 它们回放进首帧，`settings_cat` 会被「幽灵点击」改掉。
            // 首帧本就没有任何可信的用户输入（用户不可能在窗口
            // 出现的同一瞬间精确点到导航项），整批丢弃是安全的。
            //
            // IMPORTANT: 分支内也必须先 `poll_for` 再 `take_pending`：
            // `poll_for`（含 `poll`）是**唯一**把消息从系统队列泵进
            // `EventLoop::pending` 的入口；只调 `take_pending()` 会拿到
            // 恒空的缓冲——本窗口的鼠标/键盘消息一直堆在系统队列里，
            // 拖动、滚动、点击全部无效（9ddba86 只修了隐藏态与 0×0 态，
            // 显示态从创建起就缺这一步，设置窗口「画出来了但点不动」）。
            self.events.poll_for(None);
            let _ = self.events.take_pending();
            Vec::new()
        } else {
            self.events.poll_for(None);
            self.events.take_pending()
        };
        if frame_events.iter().any(|e| matches!(e, WindowEvent::CloseRequested)) {
            self.close_requested = true;
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        // 每帧把主窗口的调色板同步到设置窗口自己的 `egui::Context`。
        //
        // 设置窗口与主窗口各有一个独立 Context；`App::pal` 只在主题
        // 被切换（`Op::SetThemeMode`）时更新，灌入的是主窗口的
        // `Visuals`。这里把同一份调色板 `apply` 到设置窗口，保证
        // 「外观 → 主题」切换后两个窗口同时变色。
        crate::theme::apply(&self.ctx, app.palette());
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
        if let Err(e) = self.gfx.painter.paint(
            &mut self.gfx.fr,
            &primitives,
            ppp,
            &mut textures_delta,
        ) {
            // CAUTION: 出错必须放弃本帧：否则 `pending` 一直是 `Some`，之后
            // 每次 `acquire()` 都 bail ⇒ **该窗口永久死掉**（黑屏 +
            // 报「上一帧尚未 present」，与真实原因毫无关系）。
            self.gfx.fr.abandon_frame();
            return Err(e.into());
        }
        let _ = self.gfx.fr.present(acquired)?;
        // IMPORTANT: 必须 clear：`TexturesDelta` 的 `Drop` 有debug 断言
        // 「未应用的 delta 必须先clear」，否则 panic 且报错与真实
        // 原因毫无关系。
        textures_delta.clear();

        // CAUTION: 这里**不能**做任何周期性的 `SetWindowPos(HWND_TOPMOST)`
        // 提升：置顶窗口之间按激活顺序排 Z 序，用户点击主窗口后主
        // 窗口在上是 Windows 标准行为（用户主动切走）；每 1 秒把本
        // 窗口强提回顶会**持续抢焦点**——主窗口操作（搜索、列表）
        // 被反复打断，任务栏/输入法跟随抖动。
        //
        // 打开瞬间的 `raise_to_top`（just_opened 分支）已保证不被遮；
        // 之后用户想回设置窗口，点击它可见部分即可（激活自动到顶）。

        let requested = self.close_requested;
        self.close_requested = false;
        Ok(requested)
    }

    /// 退出前隐藏窗口。
    pub fn close(&mut self) {
        crate::presence::hide_window(self.window.hwnd());
    }
}

impl<'a> Drop for SettingsWindow<'a> {
    fn drop(&mut self) {
        // IMPORTANT: 必须销毁表面：设置窗口是「每次开关都新建」，不销毁就是
        // 每开关一次泄漏一个 `vk::SurfaceKHR`。
        //
        // 先销毁表面再让字段 drop —— `gfx`（含交换链）还引用着它。
        unsafe {
            self.gfx.gpu.destroy_surface(self.surface);
        }
    }
}
