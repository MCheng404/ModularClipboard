//! 卡片子窗口的生命周期管理。
//!
//! # 职责边界
//!
//! 本模块只管「**窗口存不存在、何时创建、何时销毁**」，
//! 不碰绘制内容——那是 [`crate::paint`] 的事。
//!
//! # 与工作区的关系
//!
//! [`Workspace`] 里每张卡片的 [`CardHost`] 表达**意图**
//! （这张卡片该独立成窗还是停靠在主窗）；本模块把意图变成现实：
//! 意图为 [`CardHost::Window`] 但窗口还没建 → 建一个；
//! 卡片被收回（`CardHost::Docked`）→ 销毁对应窗口。
//!
//! # 为什么不用 `HashMap<CardId, _>`
//!
//! 卡片 Id 会随增删变化，而窗口一旦创建就与卡片一一对应。
//! 这里用 `Vec<ChildWindow>` + 按 `CardId` 线性查找：卡片数量是
//! 个位数，线性查找比`HashMap` 更快（无需哈希），且保持了
//! 创建顺序= Z 序。
//!
//! # 失败处理
//!
//! 建窗失败**不阻断主窗**：记录警告、跳过该卡片，用户仍能用主窗。
//! 一个附属面板起不来不该让整个程序不可用。

use modular_clipboard_gfx::window::{Window, WindowEvent};

use crate::card::{Card, CardHost, CardId, CardKind};
use crate::workspace::Workspace;

/// 一张卡片对应的独立窗口。
///
/// 算出某个卡片首次成窗时的位置（屏幕逻辑点）。
///
/// 置顶窗口摆在**主窗口左边**：用户要求两个窗口并排，而不是叠在一起。
/// 于是位置 = `(main.x - 置顶窗宽 - 8, main.y)`——顶部对齐，
/// 视觉上像「同一个面板的左右两半」。
///
/// 左边界为负（主窗口太靠左）时**夹到 0**：负坐标会被 Win32 当成
/// 「放到屏幕外」，窗口就找不到了。
/// 置顶窗口的位置（屏幕逻辑点）。
///
/// 优先级：
/// 1. 卡片自己记录的位置（上次关窗时写回）⇒ 用户拖过，尊重它；
/// 2. 否则摆在**主窗口左边**并顶部对齐 —— 两个窗口并排而非叠放。
///
/// 左边界为负（主窗口太靠左）时**夹到 0**：负坐标会被 Win32 当成
/// 「放到屏幕外」，窗口就找不到了。
fn pinned_window_pos(
    card: &Card,
    main_rect: Option<(egui::epaint::emath::Vec2, egui::epaint::emath::Vec2)>,
) -> egui::epaint::emath::Vec2 {
    let saved = card.window_pos;
    let Some((main_pos, _)) = main_rect else {
        return saved;
    };
    // 位置等于兜底值 (120,120) ⇒ 用户没拖过，走并排定位。
    let is_default = (saved.x - 120.0).abs() < 1.0 && (saved.y - 120.0).abs() < 1.0;
    if !is_default {
        return saved;
    }
    let x = main_pos.x - card.window_size.x - 8.0;
    egui::epaint::emath::vec2(x.max(0.0), main_pos.y)
}

/// 渲染资源（`FrameRenderer` + `Painter`）挂在窗口自己身上而不是
/// 放在 `ChildWindows` 的并行数组里：这样增删窗口时资源必然
/// 跟着走，不会出现「窗口删了但资源还在」或两者错位。
pub struct ChildWindow<'a> {
    /// 关联的卡片。
    pub card_id: CardId,
    /// Win32 窗口。
    pub window: Window,
    /// 该窗口的交换链。
    pub gfx: crate::multiwindow::WindowGfx<'a>,
    /// 本窗口的消息泵。
    pub events: modular_clipboard_gfx::window::EventLoop,
    /// 窗口的 `egui::Context`。
    ///
    /// 每个窗口需要**独立的 `Context`**：egui 的输入、焦点、
    /// 内存（悬停/焦点状态）都按 viewport 组织，共用一个 `Context`
    /// 会让两个窗口的鼠标位置互相覆盖（都以为自己拿到了焦点）。
    pub ctx: egui::Context,
    /// 窗口标题（显示在任务栏/Alt+Tab）。
    title: String,
}

impl<'a> ChildWindow<'a> {
    /// 该窗口当前是否可见。
    pub fn is_visible(&self) -> bool {
        // `EventLoop` 没有直接的可见性查询，但客户区为 0 通常意味着
        // 最小化。渲染主循环会用这个判断跳过渲染。
        self.window.inner_size_physical().0 > 0
    }
}

/// 所有卡片子窗口的集合。
pub struct ChildWindows<'a> {
    windows: Vec<ChildWindow<'a>>,
    /// 建窗失败后不再重试的卡片（记下来避免每帧重试刷日志）。
    failed: Vec<CardId>,
}

impl<'a> ChildWindows<'a> {
    /// 新建空集合。
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// 已创建的窗口。
    pub fn iter(&self) -> impl Iterator<Item = &ChildWindow<'a>> {
        self.windows.iter()
    }

    /// 可变迭代（主循环渲染时需要）。
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut ChildWindow<'a>> {
        self.windows.iter_mut()
    }

    /// 按卡片 Id找窗口。
    pub fn by_card(&self, id: CardId) -> Option<&ChildWindow<'a>> {
        self.windows.iter().find(|w| w.card_id == id)
    }

    /// 按卡片 Id 找窗口（可变）。
    pub fn by_card_mut(&mut self, id: CardId) -> Option<&mut ChildWindow<'a>> {
        self.windows.iter_mut().find(|w| w.card_id == id)
    }

    /// 窗口数量。
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// 没有子窗口。
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// 让集合与工作区的 [`CardHost`] 保持一致。
    ///
    /// 幂等：每帧调用只会真正创建/销毁一次。
    pub fn sync_with(
        &mut self,
        ws: &mut Workspace,
        shared: &crate::multiwindow::SharedGfx<'a>,
        scale_factor: f32,
        main_rect: Option<(egui::epaint::emath::Vec2, egui::epaint::emath::Vec2)>,
    ) {
        // ---- 1. 该销毁的 ----
        //
        // 先收集要删的 Id，避免「边遍历边删」导致索引失效。
        let stale: Vec<CardId> = self
            .windows
            .iter()
            .filter(|w| ws.get(w.card_id).map(|c| c.host) != Some(CardHost::Window))
            .map(|w| w.card_id)
            .collect();
        for id in stale {
            // 卡片可能已从工作区删除，`remove_card` 里会一并处理。
            self.remove_card(id, ws);
        }

        // ---- 2. 该创建的 ----
        let wanted: Vec<(CardId, String, u32, u32, egui::epaint::emath::Vec2)> = ws
            .cards
            .iter()
            .filter(|c| {
                c.host == CardHost::Window
                    && !self.failed.contains(&c.id)
                    && !self.windows.iter().any(|w| w.card_id == c.id)
            })
            .map(|c| {
                // ⚠️ `window_size` 是**逻辑点**，`Window::new` 要的是
                // **物理像素**。早前这里乘 100 当 DPI 系数，得到
                // 36000x48000 的窗口，Vulkan 表面如实报告这个尺寸，
                // 于是交换链按 36000x48000 分配显存 → `OUT_OF_DEVICE_MEMORY`
                // →「device memory allocation has failed」。
                let ppp = if scale_factor > 0.01 { scale_factor } else { 1.0 };
                (
                    c.id,
                    c.kind.title().to_string(),
                    (c.window_size.x * ppp).round().max(160.0) as u32,
                    (c.window_size.y * ppp).round().max(120.0) as u32,
                    // 置顶窗要摆到主窗左边；其它卡片用自己记录的位置。
                    if c.kind == CardKind::Pinned {
                        pinned_window_pos(c, main_rect)
                    } else {
                        c.window_pos
                    },
                )
            })
            .collect();

        for (id, title, w, h, pos) in wanted {
            match ChildWindow::create(id, &title, w.max(160), h.max(120), pos, scale_factor, shared) {
                Ok(win) => {
                    tracing::info!(card = ?id, title = %title, w, h, "卡片子窗口已创建");
                    self.windows.push(win);
                }
                Err(e) => {
                    // ⚠️ 失败后记进 `failed`，**不**每帧重试：
                    // 建窗失败的常见原因是系统资源不足或窗口数超限，
                    // 每帧重试会刷爆日志且永远成功不了。
                    tracing::error!(card = ?id, %e, "卡片子窗口创建失败，本会话不再重试");
                    self.failed.push(id);
                }
            }
        }
    }

    /// 移除某个卡片的窗口（卡片已不存在或被收回）。
    fn remove_card(&mut self, id: CardId, ws: &mut Workspace) {
        let Some(pos) = self.windows.iter().position(|w| w.card_id == id) else {
            return;
        };
        let win = self.windows.remove(pos);
        // 卡片还在工作区里（只是被收回）⇒ 把窗口位置存回卡片。
        //
        // 不存的话，用户把卡片拖出去→ 收回 → 再拖出去，窗口每次都
        // 跳回默认位置。位置属于卡片的**持久状态**，不是窗口的临时状态。
        if let Some(card) = ws.get_mut(id) {
            let (w, h) = win.window.inner_size_points();
            card.window_size = egui::epaint::emath::vec2(w, h);
            card.window_pos = win.window.screen_position_points();
            tracing::debug!(
                card = ?id,
                pos = ?card.window_pos,
                "卡片子窗口已关闭，位置已存回卡片"
            );
        }
    }

    /// 关闭全部窗口（程序退出时）。
    pub fn close_all(&mut self) {
        for w in self.windows.drain(..) {
            w.window.destroy();
        }
    }
}

impl<'a> Drop for ChildWindows<'a> {
    fn drop(&mut self) {
        // `Window` 的 `Drop` 也会销毁窗口，但显式调一次让意图清晰，
        // 且能保证在 `Drop` 里不依赖字段顺序。
    }
}

impl<'a> ChildWindow<'a> {
    /// 创建一个卡片子窗口。
    ///
    /// 每个窗口带**独立的 `egui::Context`**：egui 按 viewport 组织
    /// 输入、焦点与内存，共用一个 `Context` 会导致两个窗口的鼠标
    /// 位置互相覆盖（都以为自己拿到焦点）。
    pub fn create(
        card_id: CardId,
        title: &str,
        width: u32,
        height: u32,
        pos: egui::epaint::emath::Vec2,
        scale_factor: f32,
        shared: &crate::multiwindow::SharedGfx<'a>,
    ) -> anyhow::Result<Self> {
        let window = Window::new(title, width, height)?;
        // 立刻定位到卡片记录的位置。
        //
        // ⚠️ 早前 `window_pos` 只在「收回时写入」、**从不读取**，
        // 于是所有子窗口都停在Win32 默认位置（层叠在一起），
        // 表现为「多个窗口重叠、界面像缺了一块」。
        //
        // `window_pos` 是逻辑点，`move_to` 要物理像素。
        let s = if scale_factor > 0.01 { scale_factor } else { 1.0 };
        window.move_to((pos.x * s) as i32, (pos.y * s) as i32);
        let ctx = egui::Context::default();
        crate::theme::install_cjk_font(&ctx, None);
        crate::theme::install_icon_font(&ctx);
        // 必须给子窗口**自己的**表面：用主窗口那个会报
        // VK_ERROR_NATIVE_WINDOW_IN_USE_KHR。
        let surface = shared
            .gpu
            .create_surface_for(window.hinstance(), window.hwnd())
            .map_err(|e| anyhow::anyhow!("创建表面失败: {e}"))?;
        let gfx = crate::multiwindow::WindowGfx::new_for(shared, surface)
            .map_err(|e| anyhow::anyhow!("建交换链失败: {e}"))?;
        let events = modular_clipboard_gfx::window::EventLoop::new(&window);
        Ok(Self {
            card_id,
            window,
            gfx,
            events,
            ctx,
            title: title.to_string(),
        })
    }

    /// 画这个子窗口的一帧。
    ///
    /// 流程与主窗口一致：事件 → `run_ui` → `tessellate` → 绘制 → present。
    /// 差别只在用的是**本窗口自己的** `egui::Context` 与交换链。
    pub fn render(&mut self, app: &mut crate::app::App) -> anyhow::Result<()> {
        // 客户区 0×0（最小化）时不能 acquire：交换链拿不到可呈现图像。
        let (cw, ch) = self.window.inner_size_physical();
        if cw == 0 || ch == 0 {
            self.events.poll_for(Some(crate::multiwindow::CHILD_POLL_INTERVAL));
            return Ok(());
        }

        let frame_events = self.events.take_pending();
        for ev in &frame_events {
            if matches!(ev, WindowEvent::CloseRequested) {
                // 用户关了子窗口 ⇒ 把卡片收回工作区。
                // 直接销毁窗口会让卡片「消失」（模型说它在 Window，
                // 但窗口没了），所以必须改 host。
                app.ws.dock(self.card_id);
                return Ok(());
            }
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        // 绘制委托给 `App::draw_child_frame`：它把私有字段的访问
        // 收敛在 app.rs 内部，子窗口模块不需要知道 App 的内部布局。
        let card_id = self.card_id;
        let mut out = self
            .ctx
            .run_ui(raw_input, |ui| app.draw_child_frame(ui, card_id));
        let ppp = out.pixels_per_point;
        let mut textures_delta = std::mem::take(&mut out.textures_delta);
        let primitives = self.ctx.tessellate(std::mem::take(&mut out.shapes), ppp);
        drop(out);

        // 尺寸变了先重建，避免用旧 framebuffer 呈现。
        self.gfx.rebuild_if_needed((cw, ch))?;

        let Some(acquired) = self.gfx.fr.acquire()? else {
            self.gfx.fr.rebuild_swapchain(Default::default())?;
            return Ok(());
        };
        self.gfx
            .painter
            .paint(&mut self.gfx.fr, &primitives, ppp, &mut textures_delta)?;
        let _ = self.gfx.fr.present(acquired)?;
        textures_delta.clear();
        Ok(())
    }

    /// 窗口标题。
    pub fn title(&self) -> &str {
        &self.title
    }
}