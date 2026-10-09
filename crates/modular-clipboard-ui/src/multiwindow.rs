//! 多窗口的渲染资源组织。
//!
//! # 为什么需要这个模块
//!
//! 原先一个程序只有一个窗口，渲染资源直接摆在 `run()` 的栈上：
//! `Gpu` → `PipelineBundle` → `FrameRenderer` → `Painter`，
//! 一条直线用到底。
//!
//! 卡片要能分离成独立子窗口后，「一个窗口一套资源」变成
//! 「N 个窗口共享一台设备、各自一条交换链」：
//!
//! ```text
//!            ┌─ WindowGfx(主窗口) ─ FrameRenderer(交换链 A) + Painter(A)
//!  Gpu ─────┼─ WindowGfx(详情窗) ─ FrameRenderer(交换链 B) + Painter(B)
//!  (共享)   └─ WindowGfx(视图窗) ─ FrameRenderer(交换链 C) + Painter(C)
//!            PipelineBundle(共享一套渲染管线)
//! ```
//!
//! # 共享什么、各自持有什么
//!
//! | 资源 | 归属 | 理由 |
//! |---|---|---|
//! | [`Gpu`] | **共享** | Vulkan 设备/队列/内存分配器，创建代价高 |
//! | [`PipelineBundle`] | **共享** | 管线与渲染通道与窗口无关 |
//! | [`FrameRenderer`] | **每窗独有** | 交换链/帧缓冲绑定窗口尺寸 |
//! | [`Painter`] | **每窗独有** | 见下方「为什么不共享 Painter」 |
//!
//! # 为什么不共享 `Painter`
//!
//! `Painter<'a>` 内部持有 `&'a Gpu` **且** 一整套可变缓冲
//! （顶点/索引/ staging、描述符写入游标）。若让多个窗口共用一个
//! `Painter`，就需要同时持有 `&mut Painter`（写这一帧的顶点）与
//! 各窗口的 `FrameRenderer`（借用同一 `Gpu`）——
//!
//! 1. Rust 借用检查直接拒绝：`&mut Painter` 期间不能再借`Gpu`；
//! 2. 就算用 `RefCell`绕过，`Painter::paint` 里的「描述符写入游标
//!    递增 + 绑定」是**跨帧连续状态**，两个窗口交替绘制会让槽位
//!    与实际绑定错位，表现为随机花屏。
//!
//! 每个窗口一套 `Painter` 的代价是显存（每窗一份顶点缓冲与
//! 字体图集采样器）。字体图集是**按需增长**的共享图集吗？不是——
//! 每个 `Painter` 各建各的，所以 N 个窗口就有 N 份字体图集。
//! 对于 2~3 个窗口完全可以接受；真要省显存得把 `FontAtlas`
//! 抽出来共享，那是后续优化。
//!
//! # 借用关系
//!
//! [`WindowGfx<'a>`] 借用 `&'a Gpu`。因此主循环必须保证：
//! **`Gpu` 的声明在所有 `WindowGfx` 之前，且 drop 顺序相反**
//! （Rust 的结构体字段声明顺序即 drop 顺序，已自然满足）。

use std::time::Duration;

use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::frame::{FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::pipeline::{
    DescriptorLayout, GraphicsPipeline, PipelineLayout, RenderPass,
};
use modular_clipboard_gfx::shader::ShaderModule;
use modular_clipboard_gfx::window::Window;

use crate::renderer::Painter;

/// 共享渲染资源：Vulkan 设备 + 渲染管线。
///
/// 只创建一次。后续每个窗口用 [`WindowGfx::new`] 挂自己的交换链。
pub struct SharedGfx<'a> {
    /// Vulkan 设备。
    pub gpu: &'a Gpu,
    /// 渲染管线三件套 + 描述符布局。
    ///
    /// 与窗口无关，所以只建一份。但**必须**与各窗口交换链最终
    /// 选中的格式兼容——`FrameRenderer::new` 内部会校验，
    /// 不一致直接 `bail`（而不是让驱动在 `cmd_begin_render_pass`
    /// 时才崩，那种失败难查得多）。
    pub pipeline: PipelineBundle,
    /// 用户配置的自定义字体路径（`config.ui.font_path`）。
    ///
    /// 放在这里是为了让**每个**窗口装字体时都能取到它。
    ///
    /// ⚠️ 早前各窗口各自调 `install_cjk_font(&ctx, None)`，只有主窗口
    /// 传了真实的 `font_path` —— 于是同一个界面里，主窗口用用户指定
    /// 的字体、子窗口和设置窗口用回退字体。用户配了字体却「一半生效」，
    /// 排查时会以为是字体没加载成功。
    pub font_path: Option<String>,
}

impl<'a> SharedGfx<'a> {
    /// 按主窗口创建设备与管线。
    pub fn create(gpu: &'a Gpu) -> anyhow::Result<Self> {
        let surface_format = crate::app::pick_surface_format(gpu)?;
        let render_pass = RenderPass::new(&gpu.device, surface_format)?;
        let desc_layout = DescriptorLayout::new(
            &gpu.device,
            gpu.desc_caps.all_bindings_update_after_bind(),
        )?;
        let pipe_layout = PipelineLayout::new(
            &gpu.device,
            std::slice::from_ref(&desc_layout.handle),
        )?;
        let shader = ShaderModule::new(&gpu.device)?;
        let pipeline = GraphicsPipeline::new(
            &gpu.device,
            render_pass.handle,
            pipe_layout.handle,
            &shader,
        )?;
        Ok(Self {
            gpu,
            pipeline: PipelineBundle::new(
                render_pass.handle,
                pipe_layout.handle,
                pipeline.handle,
                desc_layout.handle,
            ),
            font_path: None,
        })
    }

    /// 记下用户的字体路径，供后续所有窗口使用。
    pub fn with_font_path(mut self, path: Option<&str>) -> Self {
        self.font_path = path.map(|s| s.to_string());
        self
    }

    /// 建一个**新窗口**的 `egui::Context`，字体与主窗口保持一致。
    ///
    /// # 为什么必须走这个函数
    ///
    /// 每个窗口必须有自己的 `egui::Context`（共用会让鼠标位置互相
    /// 覆盖、焦点错乱），但**装字体的步骤必须完全一致**——否则同一
    /// 个界面里不同窗口用不同字体。
    ///
    /// ⚠️ 顺序也有讲究：图标字体必须排在 CJK 之后。`install_cjk_font`
    /// 内部用 `set_fonts` 整体替换，先装图标会被冲掉（文档里写明）。
    ///
    /// 早前这段逻辑被复制了三份（`childwin.rs` / `settingswin.rs` /
    /// 测试夹具），其中两份传 `None` 而不是真实配置——典型的复制粘贴
    /// 漂移。收敛成一个函数，新增窗口不可能再漏。
    pub fn new_context(&self) -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_cjk_font(&ctx, self.font_path.as_deref());
        // 图标字体必须排在 CJK 之后；失败不致命（图标会缺，但界面可用）。
        if !crate::theme::install_icon_font(&ctx) {
            tracing::warn!("图标字体加载失败，界面图标可能显示为空");
        }
        ctx
    }
}

/// 一个窗口所需的全部 GPU 资源。
///
/// drop 顺序：字段按声明顺序 drop，`painter` 先于 `fr`——正确，
/// 因为 `Painter` 借用 `Gpu`，而 `fr` 也借用 `Gpu`，两者互不依赖。
pub struct WindowGfx<'a> {
    /// 共享设备（借用 `SharedGfx` 的那一个）。
    ///
    /// 存在的唯一理由：`ChildWindow` / `SettingsWindow` 要在 `Drop`
    /// 里销毁自己的 `vk::SurfaceKHR`，而表面是由 `Gpu` 创建、
    /// 也必须由 `Gpu` 销毁的。`FrameRenderer` 内部的 `surface`
    /// 字段是私有的，且它的 `drop` 不销毁表面。
    pub gpu: &'a Gpu,
    /// 该窗口的交换链与帧缓冲。
    pub fr: FrameRenderer<'a>,
    /// 该窗口的画笔（顶点/纹理/描述符）。
    pub painter: Painter<'a>,
}

impl<'a> WindowGfx<'a> {
    /// 从共享资源派生一个窗口的渲染器。
    ///
    /// 会主动重建一次交换链，保证 framebuffer 与真实客户区严格
    /// 一致（`Gpu` 里的表面能力是**创建时**的快照，之后窗口可能
    /// 已被 resize 过）。
    pub fn new(shared: &SharedGfx<'a>) -> anyhow::Result<Self> {
        Self::new_for(shared, shared.gpu.surface())
    }

    /// 为**指定表面**建窗口渲染器（子窗口用）。
    ///
    /// ⚠️ 不能对子窗口调 [`Self::new`]：它用 `gpu.surface()`，
    /// 那是主窗口的表面。`vk::SurfaceKHR` 与 `hwnd` 一一对应，
    /// 复用会撞上 `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`（实测必现）。
    pub fn new_for(shared: &SharedGfx<'a>, surface: ash::vk::SurfaceKHR) -> anyhow::Result<Self> {
        let mut fr = FrameRenderer::new_for_surface(shared.gpu, shared.pipeline, surface)?;
        fr.rebuild_swapchain(Default::default())?;
        let painter = Painter::new(shared.gpu, fr.slot_count())?;
        tracing::info!(
            extent = ?fr.extent(),
            slots = fr.slot_count(),
            "窗口渲染器就绪"
        );
        Ok(Self {
            gpu: shared.gpu,
            fr,
            painter,
        })
    }

    /// 客户区尺寸为 0（最小化 / 被 resize 到 0）时返回 true。
    ///
    /// 此时不能acquire——交换链拿不到可呈现的图像。
    pub fn is_degenerate(&self) -> bool {
        let (w, h) = self.extent();
        w == 0 || h == 0
    }

    /// 当前交换链尺寸。
    pub fn extent(&self) -> (u32, u32) {
        let e = self.fr.extent();
        (e.width, e.height)
    }

    /// 交换链尺寸与目标不符时重建。
    pub fn rebuild_if_needed(&mut self, want: (u32, u32)) -> anyhow::Result<bool> {
        if self.extent() == want {
            return Ok(false);
        }
        self.fr.rebuild_swapchain(Default::default())?;
        // `on_swapchain_rebuilt` 要 `&mut FrameRenderer`（它会重读
        // slot 数来调整在飞帧缓冲），所以放在 `rebuild` 之后单独调用。
        self.painter.on_swapchain_rebuilt(&mut self.fr)?;
        Ok(true)
    }

    /// 画一帧。
    ///
    /// `acquired` 为 `None` 表示需要重建交换链（调用方应重建后重试）。
    pub fn render(
        &mut self,
        primitives: &[egui::epaint::ClippedPrimitive],
        ppp: f32,
        textures_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<PresentResult> {
        let Some(acquired) = self.fr.acquire()? else {
            return Ok(PresentResult::Outdated);
        };
        self.painter
            .paint(&mut self.fr, primitives, ppp, textures_delta)?;
        self.fr.present(acquired)
    }
}

/// 帧统计日志的间隔。
pub const FRAME_LOG_INTERVAL: u64 = 300;

/// 轮询子窗口事件时用的等待时长。
pub const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[allow(unused_imports)]
use Window as _WindowDocLink;