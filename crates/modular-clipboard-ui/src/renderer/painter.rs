//! `Painter`：一帧的绘制上下文。
//!
//! 持有字体图集、用户纹理表、离屏模糊链与磨砂背景资源；
//! `Drop` 按「描述符引用者 -> 被引用者」逆序释放。
//! 本文件只放 Painter 本体；批次与纯函数在根，槽位缓冲在根。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::backdrop::CAPTURE_INTERVAL;
use modular_clipboard_gfx::backdrop::DesktopCapture;
use modular_clipboard_gfx::backdrop::MotionPredictor;
use modular_clipboard_gfx::backdrop::capture_due;
use modular_clipboard_gfx::backdrop::capture_size;
use modular_clipboard_gfx::backdrop::client_origin;
use modular_clipboard_gfx::blur::BlurChain;
use modular_clipboard_gfx::blur::BlurPipeline;
use modular_clipboard_gfx::blur::DEFAULT_DIVISOR;
use modular_clipboard_gfx::blur::DEFAULT_RADIUS;
use modular_clipboard_gfx::buffer::UniformBuffer;
use modular_clipboard_gfx::buffer::Vertex;
use modular_clipboard_gfx::frame::CommonBindings;
use modular_clipboard_gfx::frame::DrawInput;
use modular_clipboard_gfx::frame::FrameRenderer;
use modular_clipboard_gfx::frame::USER_TEXTURE_SET_CAP;
use modular_clipboard_gfx::texture::DeviceImage;
use modular_clipboard_gfx::texture::FONT_FORMAT;
use modular_clipboard_gfx::texture::Sampler;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Instant;

use super::{Batch, FONT_TEXTURE_ID, INITIAL_INDEX_CAPACITY, INITIAL_VERTEX_CAPACITY, SlotBuffers, UniformKey, is_full_image_update, tessellate_into, uniforms_for};

// ---------------------------------------------------------------------------
// Painter
// ---------------------------------------------------------------------------

/// 一张用户纹理（图片缩略图）在 GPU 侧的全部资源。
///
/// `set` 是**这张纹理专属**的描述符集：uniform / sampler / 字体图集
/// 与帧槽集合相同，只有 `BINDING_USER_TEXTURE` 指向本图的视图。
/// 批次绘制前按需重绑（见 `DrawBatch::user_set`）。
pub(super) struct UserTexture {
    image: DeviceImage,
    set: vk::DescriptorSet,
    /// 本纹理的采样器是否为**环绕**（`TextureOptions::wrap_mode =
    /// Repeat`）。
    ///
    /// CAUTION: 必须记住——`rewrite_user_texture_sets` 在字体图集重建时
    /// 要重写全部用户纹理集合，若统一用 clamp 采样器，环绕纹理会静默
    /// 退化（UV 超出 [0,1] 被拉到角落像素，平铺质感变成一块纯色）。
    /// 缩略图全是 clamp，默认 false。
    wrap_repeat: bool,
}

/// 一帧的绘制上下文。
///
/// 借用 `Gpu`（内部要建缓冲、查表面能力）。`Drop` 时先 `device_wait_idle`
/// 再按「描述符引用者 → 被引用者」的逆序释放，调用方无需手工清理。
pub struct Painter<'a> {
    gpu: &'a Gpu,
    /// 字体图集采样器。**必须点采样**：线性插值会把邻接字形的覆盖率渗进
    /// 当前字形边缘，小字号下整段文字糊成一团灰。这是自写字体图集渲染器
    /// 最常见的错误来源。
    sampler: Sampler,
    /// 用户纹理（缩略图 / 原图）采样器，**线性过滤**。
    ///
    /// 与 [`Self::sampler`] 分开是必要的：缩略图显示尺寸（160pt 高）与
    /// 纹理尺寸（512²）相差 1.6~2.4×，即典型的**降采样**。最近邻在
    /// 降采样下不做什么平均，逐像素抽稀会引入摩尔纹与闪点（细密纹理上
    /// 尤其明显）；线性过滤靠双线性插值做等效 2×2 平均，虽不及 mipmap
    /// 也能显著缓解。字体图集反过来必须点采样（见 [`Self::sampler`]），
    /// 两者无法共用一个 sampler ⇒ 着色器里分开两个 binding
    /// （`BINDING_SAMPLER` / `BINDING_USER_SAMPLER`）。
    sampler_linear: Sampler,
    /// 用户纹理的环绕采样器（`TextureOptions::wrap_mode = Repeat`），
    /// 见 `Painter::new` 的分配处与 `UserTexture::wrap_repeat`。
    sampler_linear_repeat: Sampler,
    /// 字体图集（覆盖率图，`R8_UNORM`）。
    font: Option<DeviceImage>,
    /// 用户纹理（图片缩略图，RGBA）。
    ///
    /// 与字体图集分开的理由：两者格式不同（覆盖率 vs 彩色），
    /// 且片元着色器对它们的合成方式不同——字体图集取单通道
    /// 乘顶点色，缩略图直接用 RGBA。
    ///
    /// # 为什么是一张表而不是一张图
    ///
    /// 列表里可以同时显示多张缩略图（若干图片行），每张是各自的
    /// egui `TextureId`。早前只存**一张**，后上传的覆盖先前的 ⇒
    /// 所有图片行采样到同一张图（且 `upload_font_delta` 的
    /// `pos` 判据还是反的，整图更新被当局部更新跳过——症状是
    /// 「缩略图区域一片空白」）。现在每张纹理带自己的描述符集，
    /// 由 `record_to` 在画该批前重绑（见 `DrawBatch::user_set`）。
    user_textures: HashMap<egui::TextureId, UserTexture>,
    /// 用户纹理的 LRU 顺序，队首最久未用。超过
    /// [`USER_TEXTURE_SET_CAP`](modular_clipboard_gfx::frame::USER_TEXTURE_SET_CAP)
    /// 时从队首淘汰（销毁图像 + 归还描述符集）。
    user_order: VecDeque<egui::TextureId>,
    /// 1×1 白色占位纹理，绑在 `BINDING_USER_TEXTURE` 上当没有缩略图时用。
    ///
    /// # 为什么必须有占位
    ///
    /// 描述符**绝不能留空**——未初始化的描述符是未定义行为
    /// （读到垃圾纹理、采样越界，或直接设备丢失）。
    /// 而 `BINDING_USER_TEXTURE` 在着色器里是硬编码的，
    /// 每帧都要有值。所以没有缩略图时也得绑一张合法的图。
    ///
    /// 用纯白而非黑：白图乘顶点色后与清屏色叠加看不出差别，
    /// 而黑图在深色主题下会形成可见的黑色方块。
    placeholder: Option<DeviceImage>,
    uniform: UniformBuffer,
    slots: SlotBuffers,
    // CPU 侧复用缓冲，避免每帧重新分配。
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    batches: Vec<Batch>,
    draw_batches: Vec<modular_clipboard_gfx::frame::DrawBatch>,
    /// 描述符绑定需要重做的标记。
    bindings_dirty: bool,
    /// 上次写入 uniform 时的内容指纹。
    last_uniform: Option<UniformKey>,
    /// 后处理资源：超采样离屏目标 + 模糊链（懒创建）。
    ///
    /// 超采样倍率 > 1 时每个窗口都需要它（内容先画到 N× 离屏目标再
    /// 降采样）；模糊链只是复用同一套资源。倍率为 1 时永远为 `None`。
    post: Option<PostFx>,
    /// 自研磨砂背景（懒创建，仅主窗口在 Acrylic 模式下存在）。
    backdrop: BackdropState,
    /// 已经告警过的磨砂降级**原因**（每种只记一次）。
    ///
    /// 为什么不是一个 `bool`：磨砂链上有四个独立的失败点（拿不到命令
    /// 缓冲 / 位图上传失败 / 强度更新失败 / 模糊链录制失败）。单个
    /// `bool` 意味着「第一次失败的原因」会把后面所有原因一起吞掉——
    /// 比如首帧偶发上传失败置位后，之后真正的**持续性**故障
    /// （驱动抽风导致卷积反复失败）就再也不会出现在日志里，
    /// 而那恰恰是最该被看见的那次。
    ///
    /// 按原因去重则每个原因各记一次，既不刷屏也不丢信息。
    backdrop_warned: std::collections::HashSet<BackdropWarn>,
    /// 累计统计，供上层显示与诊断。
    pub stats: FrameStats,
}

/// 磨砂降级的**原因**分类（告警按它去重，见 `Painter::backdrop_warned`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum BackdropWarn {
    /// 拿不到当前命令缓冲（`acquire` 未成功等）。
    NoCommandBuffer,
    /// 捕获的桌面位图上传 staging 失败。
    Upload,
    /// 写卷积半径 uniform 失败。
    SetRadius,
    /// 录制高斯卷积命令失败。
    Record,
}

/// 一帧后处理所需的全部 GPU 资源。
///
/// 生命周期与「窗口的**渲染目标**尺寸」绑定：尺寸或超采样倍率变化时
/// 整体重建。重建前必须 `device_wait_idle`——旧的 framebuffer 与
/// 图像可能仍被在途命令引用。
pub(super) struct PostFx {
    pipeline: BlurPipeline,
    chain: BlurChain,
}

/// 自研磨砂的一帧资源：模糊管线 + 链 + 捕获纹理。
pub(super) struct BackdropFx {
    pipeline: BlurPipeline,
    chain: BlurChain,
    /// 捕获的桌面纹理（窗口物理像素尺寸，交换链同格式）。
    capture_img: DeviceImage,
    /// 捕获图是否至少上传过一次。
    ///
    /// `DeviceImage::new` 只分配内存、不写数据：未上传就采样是未定义
    /// 行为。首帧捕获失败（最小化/GDI 异常）时底图合成必须跳过，
    /// 等待第一次成功上传。
    uploaded: bool,
}

/// 磨砂背景的运行状态：GPU 资源 + GDI 捕获器 + 节流时间戳 + 背景亮度。
pub(super) struct BackdropState {
    fx: Option<BackdropFx>,
    capture: Option<DesktopCapture>,
    last_capture: Option<Instant>,
    /// 上一次成功捕获的背景平均亮度（0~1），供上层决定背景层实度。
    luminance: Option<f32>,
    /// 上一次卷积所用的高斯半径；与本帧设置不同则强制重捕获，
    /// 让用户拖「磨砂强度」滑块时立即见效（不等 30fps 捕获节流）。
    blur_radius: u32,
    /// 窗口客户区原点的运动轨迹（移动预测，见 `gfx::backdrop::MotionPredictor`）。
    ///
    /// 每帧观测一次（`record_backdrop_blur` 入口，**先于节流判断**）——
    /// 节流帧不观测的话拖动中的速度采样会稀疏成 30fps，预测失真。
    predictor: MotionPredictor,
    /// 最新一次观测到的客户区原点（屏幕物理像素）。
    origin: Option<(f32, f32)>,
    /// **上一次成功捕获时**的客户区原点。合成底图时按
    /// `(预测位置 − 本原点) / 交换链尺寸` 平移采样坐标，把过期快照
    /// 贴回窗口当前位置（消除拖动磨砂的轻微拖影）。
    capture_origin: Option<(f32, f32)>,
}

impl Default for BackdropState {
    fn default() -> Self {
        Self {
            fx: None,
            capture: None,
            last_capture: None,
            luminance: None,
            blur_radius: DEFAULT_RADIUS,
            predictor: MotionPredictor::new(),
            origin: None,
            capture_origin: None,
        }
    }
}

impl BackdropState {
    /// 销毁全部 GPU 资源。前置条件：调用方已 `device_wait_idle`。
    fn destroy(&mut self, gpu: &Gpu) {
        if let Some(mut fx) = self.fx.take() {
            fx.chain.destroy(&gpu.device);
            fx.pipeline.destroy(gpu);
            fx.capture_img.destroy(&gpu.device);
        }
        self.capture = None;
        self.last_capture = None;
        self.luminance = None;
        // 捕获原点作废：下一帧重新捕获前没有可对齐的快照。
        // 预测器保留无妨（窗口还在动的话轨迹仍有效），但原点必须清，
        // 否则新快照会按旧原点的位移平移——凭空错位一帧。
        self.capture_origin = None;
    }
}

/// 累计统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// 最后一帧的顶点数。
    pub vertices: usize,
    /// 最后一帧的索引数。
    pub indices: usize,
    /// 最后一帧合并后的 draw call 数。
    pub draw_calls: u32,
    /// 字体图集累计上传次数。
    pub texture_uploads: u64,
    /// 交换链累计重建次数。
    pub rebuilds: u32,
}

/// 帧合成路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaintMode {
    /// 普通路径：内容直接画到交换链（或经超采样离屏）。
    Plain,
    /// 设置窗口 MCP 覆盖层：分割点之前的内容离屏模糊。
    OverlayBlur,
    /// 主窗口自研磨砂：窗口身后桌面的实时模糊作底图。
    BackdropBlur,
}

impl<'a> Painter<'a> {
    /// 绑定设备并分配资源。`slot_count` 应传在飞帧槽位数
    /// （即 `FrameRenderer::slot_count()`）。
    pub fn new(gpu: &'a Gpu, slot_count: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(slot_count > 0, "在飞帧槽位数必须大于 0");
        let sampler = Sampler::new_nearest(&gpu.device)?;
        let sampler_linear = Sampler::new_linear_clamped(&gpu.device)?;
        // 用户纹理的**环绕**采样器：egui 的 `TextureOptions::wrap_mode =
        // Repeat` 要用（程序化质感 tile 靠它平铺，见 `crate::tex`）。
        // 缺了它，UV 超出 [0,1] 的部分被 clamp 到 tile 的角落像素 ⇒
        // 整个窗口被一个像素拉伸铺满，「纹理没有纹理」（2026-10-18
        // 纸质/星空主题的实际故障）。
        let sampler_linear_repeat =
            Sampler::from_egui(&gpu.device, &egui::TextureOptions::LINEAR_REPEAT)?;
        let uniform = UniformBuffer::new(gpu)?;
        let slots = SlotBuffers::new(
            gpu,
            slot_count,
            INITIAL_VERTEX_CAPACITY,
            INITIAL_INDEX_CAPACITY,
        )?;
        // 占位纹理在首帧上传（需acquire 后的命令缓冲），见 paint()。
        Ok(Self {
            gpu,
            sampler,
            sampler_linear,
            sampler_linear_repeat,
            font: None,
            user_textures: HashMap::new(),
            user_order: VecDeque::new(),
            // 首帧上传，见 paint()。
            placeholder: None,
            uniform,
            slots,
            vertices: Vec::with_capacity(INITIAL_VERTEX_CAPACITY),
            indices: Vec::with_capacity(INITIAL_INDEX_CAPACITY),
            batches: Vec::new(),
            draw_batches: Vec::new(),
            // 字体图集尚未创建，首帧上传后必然要绑一次。
            bindings_dirty: true,
            last_uniform: None,
            // 后处理资源按需创建，见 `ensure_post`。
            post: None,
            backdrop: BackdropState::default(),
            backdrop_warned: std::collections::HashSet::new(),
            stats: FrameStats::default(),
        })
    }

    /// 帧循环里的一帧。必须在 `acquire` 成功之后、`present` 之前调用。
    ///
    /// # 参数
    ///
    /// - `primitives`：`Context::tessellate` 的产物。
    /// - `pixels_per_point`：来自 [`egui::FullOutput::pixels_per_point`]。
    /// - `textures_delta`：**会被清空**（见 [`Painter::upload_font_delta`]）。
    ///
    /// # 前置条件
    ///
    /// `fr.acquire()` 必须已返回 `Some`——`current_slot()` 依赖它。
    pub fn paint(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        primitives: &[egui::epaint::ClippedPrimitive],
        pixels_per_point: f32,
        dpi_scale: f32,
        textures_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        self.paint_inner(
            fr,
            primitives,
            pixels_per_point,
            dpi_scale,
            textures_delta,
            PaintMode::Plain,
            None,
            DEFAULT_RADIUS,
        )
    }

    /// 与 [`Self::paint`] 相同，但**启用自研 GPU 模糊**。
    ///
    /// 当图元里含 `PaintCallback` 分割标记时（见 [`tessellate_into`] 的
    /// 返回值），把标记**之前**的内容画到离屏场景图并卷积，标记**之后**
    /// 的内容（覆盖层面板）保持清晰、叠加在模糊底图之上。
    ///
    /// 没有标记时行为与 [`Self::paint`] 完全一致——调用方可以无条件
    /// 走本方法，不必自己判断「这一帧有没有覆盖层」。
    pub fn paint_blurred(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        primitives: &[egui::epaint::ClippedPrimitive],
        pixels_per_point: f32,
        dpi_scale: f32,
        textures_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        self.paint_inner(
            fr,
            primitives,
            pixels_per_point,
            dpi_scale,
            textures_delta,
            PaintMode::OverlayBlur,
            None,
            DEFAULT_RADIUS,
        )
    }

    /// 与 [`Self::paint`] 相同，但先铺一层「窗口身后桌面的实时模糊」。
    ///
    /// 自研磨砂（`WindowBackdrop::Acrylic`）主路径：
    ///
    /// ① 节流捕获（[`CAPTURE_INTERVAL`]，约 30fps）——GDI 抓窗口客户区
    ///    身后的桌面（配合 `WDA_EXCLUDEFROMCAPTURE` 自排除，见
    ///    [`modular_clipboard_gfx::backdrop`]）；
    /// ② 上传 GPU → 三趟卷积（降采样 + 横向 + 纵向高斯）；
    /// ③ 合成：交换链先铺模糊底图（`prelude`），egui 内容叠在其上——
    ///    背景层按调色板的半透明变体绘制，文字/卡片保持不透明。
    ///
    /// # 性能
    ///
    /// 捕获 + 上传 + 卷积在 720×800 下约 2ms/次（30fps 节流后占帧预算
    /// 6%），1/4 分辨率卷积的 GPU 开销可忽略。节流未到或窗口最小化时
    /// 跳过捕获，直接复用上一帧的模糊结果（`blur_a` 仍在）——不捕获
    /// 不等于没有底图。
    ///
    /// `hwnd` 以 `isize` 传入（UI 层从 `HWND` 转换），仅本方法使用。
    // 参数与 `paint_inner` 三个薄入口共享同一份签名（拆出去只是把参数
    // 列表换个地方排）；8 个参数触到 clippy 的 7 个上限，显式豁免。
    #[allow(clippy::too_many_arguments)]
    pub fn paint_backdrop(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        hwnd: isize,
        blur_radius: u32,
        primitives: &[egui::epaint::ClippedPrimitive],
        pixels_per_point: f32,
        dpi_scale: f32,
        textures_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        self.paint_inner(
            fr,
            primitives,
            pixels_per_point,
            dpi_scale,
            textures_delta,
            PaintMode::BackdropBlur,
            Some(hwnd),
            blur_radius,
        )
    }

    /// 启用/停用自研磨砂背景。
    ///
    /// 停用时立即销毁 GPU 资源（用户在设置里关掉磨砂不应继续占显存）。
    /// 需 `device_wait_idle`——在飞命令可能仍引用旧图。
    pub fn set_backdrop_enabled(&mut self, on: bool) {
        if on {
            // 资源在下一帧 paint_backdrop 里懒创建。
            return;
        }
        if let Some(mut fx) = self.backdrop.fx.take() {
            self.gpu.wait_idle();
            fx.chain.destroy(&self.gpu.device);
            fx.pipeline.destroy(self.gpu);
            fx.capture_img.destroy(&self.gpu.device);
        }
        self.backdrop.capture = None;
        self.backdrop.last_capture = None;
        // 捕获原点与轨迹作废：再开启时首帧重新捕获，避免用过期原点
        // 算出错误位移（表现为「重新开启磨砂那一下底图错位」）。
        self.backdrop.capture_origin = None;
        self.backdrop.predictor.reset();
    }

    /// 一帧的实际绘制逻辑。`mode` 决定走哪条合成路径；
    /// `backdrop_hwnd` 仅在 [`PaintMode::BackdropBlur`] 下使用。
    // 参数本来可以拆结构体，但 paint/paint_blurred/paint_backdrop 三个
    // 薄入口共享同一份签名，拆出去只是把参数列表换个地方排——不值得。
    #[allow(clippy::too_many_arguments)]
    fn paint_inner(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        primitives: &[egui::epaint::ClippedPrimitive],
        pixels_per_point: f32,
        dpi_scale: f32,
        textures_delta: &mut egui::TexturesDelta,
        mode: PaintMode,
        backdrop_hwnd: Option<isize>,
        blur_radius: u32,
    ) -> anyhow::Result<()> {
        let blur_split = tessellate_into(
            primitives,
            &mut self.vertices,
            &mut self.indices,
            &mut self.batches,
        );
        self.stats.vertices = self.vertices.len();
        self.stats.indices = self.indices.len();
        self.stats.draw_calls = self.batches.len() as u32;

        // ---- 顶点数据 → 当前槽位 ---------------------------------------
        //
        // `acquire` 已等待过该槽位栅栏，所以「拿到 slot 就往这个槽位写」是
        // 安全的：上一轮占用它的 GPU 工作已完成。这是用host-visible 环缓冲
        // 而非 device-local + staging 的理由——同样正确，且每帧少一次拷贝。
        let slot = fr
            .current_slot()
            .ok_or_else(|| anyhow::anyhow!("paint 必须在 acquire 成功后调用"))?;
        self.slots
            .write(self.gpu, slot, &self.vertices, &self.indices)?;

        // ---- 字体图集上传 -----------------------------------------------
        // 首帧创建 1×1 白色占位纹理。
        //
        // `BINDING_USER_TEXTURE` 在着色器里是硬编码的，**每帧都要有值**。
        // 若从未写入，该描述符保持未初始化状态 = 未定义行为
        // （读到垃圾纹理、采样越界，或设备丢失）。
        // 即使没有任何缩略图，也必须绑一张合法的图。
        //
        // 选白色而非黑色：白图乘顶点色后与背景叠加看不出差别；
        // 黑图在深色主题下会形成可见的黑色方块。
        if self.placeholder.is_none() {
            let mut img = DeviceImage::new(
                self.gpu,
                vk::Extent2D {
                    width: 1,
                    height: 1,
                },
                vk::Format::R8G8B8A8_UNORM,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            )?;
            fr.record_texture_upload(&mut img, &[255u8, 255, 255, 255], None, (1, 1))?;
            self.placeholder = Some(img);
            self.bindings_dirty = true;
        }

        let uploaded = self.upload_font_delta(fr, textures_delta)?;
        if uploaded > 0 {
            self.stats.texture_uploads += uploaded;
            // 图集可能已被重建（尺寸变化）→ 图像视图变了 → 必须重绑。
            self.bindings_dirty = true;
        }

        // ---- 渲染目标尺寸（超采样）---------------------------------------
        //
        // 界面先画到 `render_extent` 的离屏目标，再降采样到交换链。
        // `render_extent = 交换链尺寸 × 超采样倍率`。
        //
        // IMPORTANT: uniform 与批次裁剪都必须用 `render_extent`（不是
        // `fr.extent()`）——它们描述的是**绘制目标**的像素空间，
        // 而目标比交换链大 `render_scale` 倍。用错会让整幅画面只占
        // 左上 1/N（与历史上「UI 只画左上角 2/3」同源，且同样不报错）。
        let ssaa = if dpi_scale > 0.01 {
            // 反推：窗口层设的 ppp = dpi_scale × 实际倍率。
            //
            // IMPORTANT: 下限**不能**是 1.0。250%/300% DPI 时
            // `render_scale_for` 会把倍率压到 1.0 **以下**（欠采样），
            // 好让 `ppp` 落回 `MAX_RENDER_PPP` 以内——那是唯一能避免
            // 「图标字形被画坏」的做法（见 `gfx::render_scale_for`）。
            // 这里若又抬回 1.0，两侧对倍率的认知就不一致了：
            // uniform 的 `size_in_pixels` 按抬后的值算、窗口层给的
            // `ppp` 按压后的值算 ⇒ 不变式 `screen_rect × ppp ==
            // size_in_pixels` 断裂 ⇒ 画面整体缩放错误且**不报 GPU 错误**。
            // 上限 3.0 与 `render_scale()` 的取值域一致，防脏数据；
            // 下限 0.25 与 `render_scale_for` 的兜底下限一致。
            (pixels_per_point / dpi_scale).clamp(0.25, 3.0)
        } else {
            1.0
        };
        let swap_extent = fr.extent();
        let render_extent = vk::Extent2D {
            width: ((swap_extent.width as f32 * ssaa).round() as u32).max(1),
            height: ((swap_extent.height as f32 * ssaa).round() as u32).max(1),
        };

        // ---- 描述符绑定 -------------------------------------------------
        if let Some(f) = self.font.as_ref() {
            if self.bindings_dirty {
                self.bind_all(fr, f)?;
                self.bindings_dirty = false;
            }
            // uniform 内容只随「渲染目标尺寸 / dpr」变化。多帧在途时改写同一份
            // uniform 缓冲会被上一帧的 GPU 读到，因此**只在内容真的变了才写**
            // ——写相同内容则不存在竞态。
            let key = UniformKey {
                width: render_extent.width,
                height: render_extent.height,
                pixels_per_point,
            };
            if self.last_uniform != Some(key) {
                self.uniform
                    .write(&self.gpu.device, &uniforms_for(render_extent, pixels_per_point))?;
                self.last_uniform = Some(key);
            }
        }

        // ---- 录制绘制 ----------------------------------------------------
        // 缩略图批换成该纹理自己的描述符集（多张缩略图同屏时各自采样
        // 各自的图）；字体批保持 null，用帧槽集合。
        //
        // CAUTION: 必须写成字段级借用的 for 循环，不能用
        // `extend(map(|b| ... self.user_set_for(...)))`——后者对整个
        // `self` 不可变借用，与 `self.draw_batches` 的可变借用冲突
        // （E0502）。`user_textures` 与 `batches`/`draw_batches` 是
        // 不同字段，直接访问字段即可各自借开。
        self.draw_batches.clear();
        for b in &self.batches {
            let mut d = b.to_draw(render_extent, pixels_per_point);
            d.user_set = match self.user_textures.get(&b.tex_id) {
                Some(t) if b.tex_id != FONT_TEXTURE_ID => t.set,
                _ => vk::DescriptorSet::null(),
            };
            self.draw_batches.push(d);
        }

        // 模糊路径仅在「覆盖层模式 + 本帧确有分割标记 + 标记两侧都有内容」
        // 时走。标记落在 0（覆盖层前没有内容）或末尾（没有覆盖层）时走
        // 普通路径——那时模糊没有可糊的东西，白跑三趟卷积。
        let use_blur =
            mode == PaintMode::OverlayBlur && blur_split > 0 && blur_split < self.draw_batches.len();
        let use_backdrop = mode == PaintMode::BackdropBlur;
        // 模糊链相对渲染目标的降采样倍数：乘上超采样倍率后，`blur_a`
        // 恒为交换链的 1/4——模糊强度不随超采样设置变化。
        let divisor = (DEFAULT_DIVISOR as f32 * ssaa).round() as u32;
        let vertex_buffer = self.slots.vertex_buffer(slot);
        let index_buffer = self.slots.index_buffer(slot);

        // ---- 自研磨砂：捕获 → 上传 → 卷积 --------------------------------
        //
        // 必须在**任何渲染通道**录制之前：纹理上传不允许出现在渲染通道
        // 作用域内（Vulkan 硬约束），卷积本身也要在交换链那趟通道之前完成。
        //
        // 节流未到 / 最小化 / 尺寸未定时不捕获——`blur_a` 仍持有上一帧的
        // 模糊结果，底图照铺，只是内容不更新（磨砂背景对 33ms 的延迟不敏感）。
        if use_backdrop {
            self.record_backdrop_blur(
                fr,
                backdrop_hwnd.unwrap_or_default(),
                blur_radius,
                render_extent,
                divisor,
            )?;
        }
        // 合成底图用的句柄（`Blit` 是 Copy 的小结构，先取出避免借用冲突）。
        // 首帧尚未上传过捕获图时不能铺底图（采样未初始化图像是 UB），
        // 退化为透明清屏 + 半透明背景层——下一帧自动恢复。
        //
        // `uv_offset`：磨砂移动预测补偿。拖动窗口时底图是过期快照，
        // 这里按「已走位移 + 一帧预测」平移采样坐标，让磨砂实时跟住
        // 窗口；静止时位移为 0，与旧行为逐字节一致。
        let backdrop_blit = if use_backdrop
            && self.backdrop.fx.as_ref().is_some_and(|fx| fx.uploaded)
        {
            let offset = self.backdrop_uv_offset(swap_extent);
            Some(
                self.backdrop
                    .fx
                    .as_ref()
                    .expect("上面刚检查过")
                    .pipeline
                    .composite(offset),
            )
        } else {
            None
        };

        // 倍率**恰好**为 1 时不做离屏，直接画到交换链（最省的老路径，
        // 也是出问题时的逃生开关）。
        //
        // CAUTION: 判据必须是 `== 1.0` 而不是 `<= 1.0`。超高 DPI
        // （250%/300%）下 `render_scale_for` 会把倍率压到 1.0 **以下**
        // （欠采样，让 `ppp` 落回 `MAX_RENDER_PPP` 以内）。那时
        // `render_extent < swap_extent`，若还走直画路径，uniform 里的
        // `size_in_pixels = render_extent` 与真实目标（交换链）不符 ⇒
        // 画面只占左上角一块，且**不报任何 GPU 错误**——与历史上
        // 「UI 只画左上角 2/3」完全同一类故障。
        // 走离屏路径即可：末尾那趟 blit 是线性采样的全屏三角形，
        // 放大与缩小都能正确处理（`fs_copy` 按 uv 采样）。
        if (ssaa - 1.0).abs() < f32::EPSILON {
            let input = DrawInput {
                vertex_buffer,
                index_buffer,
                batches: &self.draw_batches,
                prelude: backdrop_blit,
                ..Default::default()
            };
            return fr.record(&input);
        }

        // ---- 超采样：内容 → 离屏目标 → 降采样到交换链 ---------------------
        //
        // 顺序不能换：内容必须**先**画到离屏图；模糊要读它；合成也必须
        // 发生在离屏图上（这样覆盖层面板同样获得超采样），最后统一降采样。
        self.ensure_post(fr, render_extent, divisor)?;
        let gpu = self.gpu;
        let cmd = fr
            .current_command_buffer()
            .ok_or_else(|| anyhow::anyhow!("超采样路径要求 acquire 已成功"))?;
        let render_pass = fr.render_pass();

        let res = self.post.as_mut().expect("ensure_post 刚确保存在");

        // ① 内容 → 离屏场景图（模糊时只画分割点之前的部分）。
        let content = if use_blur {
            &self.draw_batches[..blur_split]
        } else {
            &self.draw_batches[..]
        };
        let scene_input = DrawInput {
            vertex_buffer,
            index_buffer,
            batches: content,
            // 磨砂底图先铺进场景图（模糊结果上采样），egui 内容再叠加。
            prelude: backdrop_blit,
            ..Default::default()
        };
        fr.record_to(
            res.chain.scene.framebuffer,
            res.chain.scene.extent,
            &scene_input,
        )?;
        // IMPORTANT: 离屏 pass 必须自己收尾——`present` 只会关闭**交换链**
        // 那一趟通道。漏掉这里，紧随其后的卷积/合成会因「已有通道激活」
        // 全部失败（`cmd_begin_render_pass` 报错，画面全黑）。
        fr.end_pass();

        if use_blur {
            // ② 降采样 + 横向卷积 + 纵向卷积。
            res.pipeline.record(gpu, cmd, &mut res.chain, render_pass)?;

            // ③ 把「模糊底图 + 覆盖层清晰图元」重新画回场景图。
            //
            // 为什么不在交换链上直接画：交换链只有 1× 分辨率，而覆盖层
            // 图元的顶点按 `dpr = ppp`（含超采样倍率）换算，直接画会
            // 放大 N 倍溢出。先合成到 N× 的场景图，最后统一降采样，
            // 覆盖层面板才能和背景一样享受抗锯齿。
            let composite_input = DrawInput {
                vertex_buffer,
                index_buffer,
                batches: &self.draw_batches[blur_split..],
                // 覆盖层模糊的合成无位移可言（场景图就是本帧刚画的），
                // 恒推零。
                prelude: Some(res.pipeline.composite([0.0, 0.0])),
                ..Default::default()
            };
            fr.record_to(
                res.chain.scene.framebuffer,
                res.chain.scene.extent,
                &composite_input,
            )?;
            fr.end_pass();
        }

        // ④ 场景图 → 交换链（线性降采样，抗锯齿就在这里发生）。
        let input = DrawInput {
            vertex_buffer,
            index_buffer,
            batches: &[],
            prelude: Some(res.pipeline.downsample()),
            ..Default::default()
        };
        fr.record(&input)
    }

    /// 确保后处理资源存在且与当前**渲染目标**尺寸匹配，否则（重）创建。
    ///
    /// # 参数
    /// - `render_extent`：超采样后的渲染目标尺寸（交换链 × 倍率）。
    /// - `divisor`：模糊链相对**渲染目标**的降采样倍数。它已经乘过
    ///   超采样倍率，因此 `blur_a` 的实际分辨率始终约为交换链的 1/4
    ///   ——不随超采样倍率变化，模糊强度保持一致。
    ///
    /// # 为什么在这里等 GPU 空闲
    ///
    /// 重建会销毁旧的 framebuffer / 图像，而在飞命令可能仍引用它们。
    /// `wait_idle` 在本场景下代价可忽略——只在首帧或窗口 resize 后触发，
    /// 不是每帧。
    fn ensure_post(
        &mut self,
        fr: &FrameRenderer<'a>,
        render_extent: vk::Extent2D,
        divisor: u32,
    ) -> anyhow::Result<()> {
        let up_to_date = self
            .post
            .as_ref()
            .is_some_and(|p| p.chain.matches(render_extent, divisor));
        if up_to_date {
            return Ok(());
        }

        if let Some(mut old) = self.post.take() {
            self.gpu.wait_idle();
            old.chain.destroy(&self.gpu.device);
            old.pipeline.destroy(self.gpu);
        }

        let render_pass = fr.render_pass();
        let format = fr.format();
        let mut pipeline = BlurPipeline::new(self.gpu, render_pass)?;
        let chain = BlurChain::new(self.gpu, render_pass, format, render_extent, divisor)?;
        // 描述符集必须在链创建之后写一次——它引用链里的图像视图。
        pipeline.configure(self.gpu, &chain)?;
        tracing::debug!(
            ?render_extent,
            divisor,
            low = ?chain.blur_a.extent,
            "超采样/模糊资源已创建"
        );
        self.post = Some(PostFx { pipeline, chain });
        Ok(())
    }

    /// 采样估算图像的平均亮度（0~1，sRGB 字节域的 Rec.709 加权均值）。
    ///
    /// 只用于「背景该压多实」这类粗粒度决策，因此**稀疏采样**即可：
    /// 每 97 个像素取一个（素数步长，避免与行宽对齐产生条纹偏差）。
    /// 每帧成本约 260×400/97 ≈ 1000 次加法，可忽略。
    fn luminance_of(rgba: &[u8]) -> f32 {
        let mut sum: u64 = 0;
        let mut n: u64 = 0;
        let mut i = 0usize;
        while i + 2 < rgba.len() {
            // Rec.709 亮度权重（整数化：2126/7152/722 ≈ 0.2126/0.7152/0.0722）
            sum += 2126 * u64::from(rgba[i])
                + 7152 * u64::from(rgba[i + 1])
                + 722 * u64::from(rgba[i + 2]);
            n += 10_000;
            i += 4 * 97;
        }
        if n == 0 {
            return 0.5;
        }
        (sum as f32 / n as f32 / 255.0).clamp(0.0, 1.0)
    }

    /// 背景（窗口身后桌面）的平均亮度，`None` = 还没有成功捕获过。
    pub fn backdrop_luminance(&self) -> Option<f32> {
        self.backdrop.luminance
    }

    /// 磨砂底图合成时的 UV 位移（push constant，目标 UV 单位）。
    ///
    /// # 算的是什么
    ///
    /// 底图是 `capture_origin` 时刻抓的快照，而窗口现在（帧呈现时更晚）
    /// 在 `预测位置`。把采样坐标平移 `(预测 − 捕获) / 交换链尺寸` 后，
    /// 快照内容正好落在窗口当前所占的屏幕区域上——**静止时该值为 0**
    /// （两个位置相同），行为与加此机制前完全一致；拖动时它逐帧把
    /// 过期快照「拉」回窗口，消除磨砂背景跟不上窗口的轻微拖影。
    ///
    /// # 为什么除以交换链尺寸而不是模糊链尺寸
    ///
    /// 合成用的全屏三角形把 `[0,1]²` 的 UV 映射到**整个绘制目标**；
    /// 无论目标是交换链（`ssaa ≤ 1` 路径）还是超采样场景图
    /// （内容先画场景、再统一降采样），一像素的物理位移都等于
    /// `d / swap_extent` 个 UV 单位——两条路径口径一致。
    ///
    /// # 为什么还要一帧预测
    ///
    /// 补偿只算到「本帧观测时刻」，而帧提交后还要过约一个帧间隔才
    /// 真正呈现，那时窗口已又走过 `v × 帧间隔`。 `MotionPredictor`
    /// 用速度外推补上这段，观感上底图与窗口「正好跟住」而不是总慢
    /// 半拍。预测失误的代价是有界的：`CLAMP_TO_EDGE` 采样器只会把
    /// 越界的一小条边缘拉伸，而模糊半径（等效全分辨率 ~32px）远大于
    /// 单帧位移，看不出来。
    ///
    /// 无捕获原点（首帧前 / 刚重建）时返回零——那时本就不铺底图
    /// （`fx.uploaded == false`），零值只是不改变任何行为。
    ///
    /// # 分母必须是 `swap_extent`（物理像素），不是 `render_extent`
    ///
    /// 三个量纲必须对齐：`dx`/`dy` 来自 [`client_origin`]，是**屏幕物理
    /// 像素**；`swap_extent` 是客户区**物理像素**；而 `uv_offset` 是
    /// **归一化 UV**（`fs_copy` 在 `in.uv + offset` 处采样，源图铺满目标）。
    ///
    /// 用 `render_extent`（= 交换链 × 超采样倍率）当分母会让位移**小
    /// `ssaa` 倍**：窗口拖了 10 物理像素，2× 超采样下只补偿了 5 像素 ⇒
    /// 磨砂底图与窗口错开半个身位，表现为「拖动时底图跟不上」。
    /// 归一化的好处正在于**与目标分辨率无关**，所以分母取哪个物理尺寸
    /// 都行，唯独不能取「物理像素 × 倍率」这种混合量纲。
    ///
    /// 算术本身抽成 [`backdrop_uv_offset_from`]（纯函数，便于单测钉住量纲）。
    fn backdrop_uv_offset(&self, swap_extent: vk::Extent2D) -> [f32; 2] {
        let (Some(cap), Some(_cur)) = (self.backdrop.capture_origin, self.backdrop.origin) else {
            return [0.0, 0.0];
        };
        let pred = self
            .backdrop
            .predictor
            .predict_next_frame()
            .unwrap_or((cap.0, cap.1));
        backdrop_uv_offset_from(pred, cap, swap_extent)
    }

    /// 交换链为 RGBA 字节序时需要 R/B 交换（GDI DIB 恒为 BGRA）。
    ///
    /// `pick_format` 只会给出 `B8G8R8A8_UNORM` / `R8G8B8A8_UNORM` 两种；
    /// 其余（单格式表面报出的罕见格式）按不交换处理——错序仅影响
    /// 磨砂底图偏色，不影响主流程。
    fn needs_rb_swap(format: vk::Format) -> bool {
        matches!(
            format,
            vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB
        )
    }

    /// 自研磨砂：按需创建资源，节流捕获窗口身后的桌面并卷积。
    ///
    /// 在 `paint_inner` 的所有渲染通道之前调用（上传不能进渲染通道，
    /// 卷积要在交换链那趟通道前完成）。任何一步失败都只记日志并跳过
    /// ——磨砂是观感增强，绝不能把渲染主流程带崩。
    ///
    /// # 移动预测（本函数与 `paint_inner` 的合成配合）
    ///
    /// 1. 入口处**无论是否节流**都观测一次客户区原点（`client_origin`），
    ///    喂给 `MotionPredictor`；
    /// 2. 窗口在动（速度超阈值）时**强制本帧捕获**——否则 33ms 节流
    ///    会让快照比窗口落后最多两帧的位移，补偿量超出图像边界、
    ///    边缘出现拉伸条；
    /// 3. 捕获成功时记下 `capture_origin`；合成阶段
    ///    （[`Self::backdrop_uv_offset`]）按 `预测位置 − capture_origin`
    ///    平移采样坐标，过期快照重新贴住窗口。
    fn record_backdrop_blur(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        hwnd: isize,
        blur_radius: u32,
        render_extent: vk::Extent2D,
        divisor: u32,
    ) -> anyhow::Result<()> {
        let swap_extent = fr.extent();
        let format = fr.format();
        if hwnd == 0 {
            return Ok(());
        }
        self.ensure_backdrop(fr, render_extent, swap_extent, divisor, format)?;

        // ---- ① 运动观测（先于节流：节流帧也要喂预测器）----
        let now = Instant::now();
        if let Some(o) = client_origin(hwnd) {
            self.backdrop.predictor.observe(now, o);
            self.backdrop.origin = Some(o);
        }

        // 强度档位变了：强制本帧重新捕获 + 卷积，不等 30fps 节流——
        // 拖滑块时若还按 33ms 节流，强度要隔一两帧才跳，手感发黏。
        let radius_changed = self.backdrop.blur_radius != blur_radius;
        if radius_changed {
            self.backdrop.blur_radius = blur_radius;
        }
        // 节流：未到间隔、强度未变、且窗口没在动时直接复用上一帧的
        // 模糊结果。窗口移动时放行——见方法文档第 2 条。
        let moving = self.backdrop.predictor.is_moving();
        if !radius_changed && !moving && !capture_due(self.backdrop.last_capture, now, CAPTURE_INTERVAL) {
            return Ok(());
        }

        // 本帧观测到的原点先拷贝出来：下面按字段拆借 `&mut self.backdrop`
        // 期间不能再整体访问它。捕获与观测同帧发生，拷贝值即「快照
        // 对应的窗口位置」。
        let observed_origin = self.backdrop.origin;

        // 拆字段借用：`fx` 与 `capture` 是 `BackdropState` 的两个互不相交
        // 字段，逐字段借比整体 `&mut self.backdrop` 少一层间接冲突。
        let BackdropState {
            fx,
            capture,
            last_capture,
            luminance,
            // blur_radius / predictor / origin / capture_origin 已在
            // 节流判断前整体读写，这里无需再解构。
            ..
        } = &mut self.backdrop;
        let fx = fx.as_mut().expect("ensure_backdrop 刚确保存在");

        // 捕获。GDI 初始化失败属于「永不发生但必须兜住」的分支：
        // 降级为本帧不更新底图（已上传过的话上一帧结果仍在用）。
        let cap = match capture {
            Some(c) => c,
            None => match DesktopCapture::new(Self::needs_rb_swap(format)) {
                Ok(c) => capture.insert(c),
                Err(e) => {
                    tracing::warn!("桌面捕获器初始化失败，磨砂背景停更: {e:#}");
                    return Ok(());
                }
            },
        };
        let Some((bits, w, h)) = cap.capture_behind(hwnd)? else {
            return Ok(()); // 最小化：跳过
        };
        if (w, h) != capture_size(swap_extent.width, swap_extent.height) {
            // resize 竞态：客户区已变、交换链还没重建。跳过本帧，
            // 等下一轮 swapchain 重建后尺寸自然对齐。
            return Ok(());
        }
        // 捕获成功 ⇒ 记下快照对应的窗口位置。合成阶段用它算位移
        // （磨砂移动预测，见 [`Self::backdrop_uv_offset`]）。
        // 观测与捕获在同一帧内先后发生，原点即捕获时刻的位置。
        // 注意：拆借期间不能写 `self.backdrop.*`，改为拆借前取出、
        // 到这里再借道 `BackdropState` 的剩余字段——直接放回拆借模式
        // 会与 `last_capture` 的写冲突，所以用局部变量延迟到函数尾。
        let capture_succeeded = observed_origin;

        // 上传 + 三趟卷积（降采样、横向、纵向）。
        //
        // CAUTION: 这两步的失败必须**就地降级**，不能往上冒泡。
        // 早期版本让上传错误冒泡，首帧「字体图集 + 桌面捕获」同时上传
        // 撑满 staging arena 时，主循环直接以「启动失败」退出——
        // 一个观感增强不该有这个权限。降级后本帧沿用上一帧的底图。
        // 记下本帧背景的**平均亮度**（0~1）。渲染层据此调整背景层实度：
        // 纯黑壁纸 + 浅色主题时，若背景层太透，深色文字会落在被压暗的
        // 背景上，对比度直接崩掉（这是纯色桌面下最实际的可读性问题）。
        *luminance = Some(Self::luminance_of(bits));

        let cmd = match fr.current_command_buffer() {
            Some(c) => c,
            None => {
                self.warn_backdrop(BackdropWarn::NoCommandBuffer, "拿不到当前命令缓冲");
                return Ok(());
            }
        };
        let render_pass = fr.render_pass();
        let gpu = self.gpu;
        if let Err(e) = fr.record_texture_upload(&mut fx.capture_img, bits, None, (w, h)) {
            self.warn_backdrop(BackdropWarn::Upload, &format!("桌面位图上传失败: {e:#}"));
            return Ok(());
        }
        // 写入本帧的卷积半径（磨砂强度）。描述符集指向的是同一块 uniform
        // 缓冲，改完紧接着的 record_from 即按新半径卷积——无需重建管线。
        if let Err(e) = fx.pipeline.set_radius(gpu, &fx.chain, blur_radius) {
            self.warn_backdrop(BackdropWarn::SetRadius, &format!("磨砂强度更新失败: {e:#}"));
            return Ok(());
        }
        if let Err(e) = fx.pipeline.record_from(
            gpu,
            cmd,
            &mut fx.chain,
            render_pass,
            fx.pipeline.backdrop_set(),
        ) {
            self.warn_backdrop(BackdropWarn::Record, &format!("模糊链录制失败: {e:#}"));
            return Ok(());
        }
        fx.uploaded = true;
        *last_capture = Some(now);
        // 到这里拆借已全部用完，可以安全写回捕获原点（引用在 NLL
        // 下于最后一次使用后结束；`fx.uploaded` 这行是最后一次）。
        self.backdrop.capture_origin = capture_succeeded;
        Ok(())
    }

    /// 磨砂降级告警：按**原因**去重（每种只记一次，避免每帧刷屏）。
    fn warn_backdrop(&mut self, kind: BackdropWarn, msg: &str) {
        if self.backdrop_warned.insert(kind) {
            tracing::warn!("磨砂背景降级（沿用上一张底图）: {msg}");
        }
    }

    /// 确保磨砂资源存在且尺寸匹配，否则整体重建。
    ///
    /// 模糊链按 `render_extent`（超采样后）建，`divisor` 已乘过超采样
    /// 倍率——与 `ensure_post` 同一套口径，`blur_a` 恒为交换链 1/4。
    /// 捕获图按**交换链**尺寸建（BitBlt 读的就是客户区物理像素）。
    fn ensure_backdrop(
        &mut self,
        fr: &FrameRenderer<'a>,
        render_extent: vk::Extent2D,
        swap_extent: vk::Extent2D,
        divisor: u32,
        format: vk::Format,
    ) -> anyhow::Result<()> {
        let (cw, ch) = capture_size(swap_extent.width, swap_extent.height);
        let up_to_date = self.backdrop.fx.as_ref().is_some_and(|fx| {
            fx.chain.matches(render_extent, divisor)
                && fx.capture_img.size == vk::Extent2D { width: cw, height: ch }
        });
        if up_to_date {
            return Ok(());
        }
        if let Some(mut old) = self.backdrop.fx.take() {
            self.gpu.wait_idle();
            old.chain.destroy(&self.gpu.device);
            old.pipeline.destroy(self.gpu);
            old.capture_img.destroy(&self.gpu.device);
        }
        let render_pass = fr.render_pass();
        let mut pipeline = BlurPipeline::new(self.gpu, render_pass)?;
        let chain = BlurChain::new(self.gpu, render_pass, format, render_extent, divisor)?;
        // 捕获图按**降采样后**的尺寸建（见 `CAPTURE_DOWNSCALE`）：
        // 全分辨率上传会把 staging arena 撑满。
        let (cw, ch) = capture_size(swap_extent.width, swap_extent.height);
        let capture_img = DeviceImage::new(
            self.gpu,
            vk::Extent2D {
                width: cw,
                height: ch,
            },
            format,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;
        // 描述符集引用链与捕获图的视图，两者就绪后写一次。
        //
        // IMPORTANT: 必须先 `configure` 再 `bind_source`。`configure` 写链内
        // 的 0..4 组（降采样 scene、横向、纵向、**合成 SET_COMPOSITE**、
        // 场景拷贝），`bind_source` 只写第 5 组（捕获图的降采样源）。
        // 早期版本漏了 `configure`，合成阶段采样的 SET_COMPOSITE 是从未
        // 写入的描述符，驱动返回空纹理（0,0,0,0）——自研磨砂于是只剩透明、
        // 没有模糊。覆盖层路径（`ensure_post`）两处都调了，故仅主窗口受影响。
        pipeline.configure(self.gpu, &chain)?;
        pipeline.bind_source(self.gpu, capture_img.view)?;
        tracing::debug!(
            ?render_extent,
            ?swap_extent,
            divisor,
            "磨砂背景资源已创建"
        );
        self.backdrop.fx = Some(BackdropFx {
            pipeline,
            chain,
            capture_img,
            uploaded: false,
        });
        Ok(())
    }

    /// 交换链重建后重整资源。
    ///
    /// 一次重建牵动四件事，缺一件都会在后续帧里出问题：
    ///
    /// 1. `rebuild_swapchain` 重新分配描述符集 → 绑定必须重做；
    /// 2. 交换链图像数可能变化 → 在飞槽位数随之变化 → 顶点缓冲环要跟着扩缩；
    /// 3. 视口/剪裁是动态状态，但 uniform 里的表面尺寸是烘进去的 → 必须重写；
    /// 4. `acquire` 的轮转游标被重置 → 槽位与缓冲的对应关系重新开始。
    pub fn on_swapchain_rebuilt(&mut self, fr: &mut FrameRenderer<'a>) -> anyhow::Result<()> {
        self.bindings_dirty = true;
        self.last_uniform = None;
        let want = fr.slot_count();
        if self.slots.buffers.len() != want {
            self.slots.resize(self.gpu, want)?;
        }
        // 离屏目标按交换链尺寸派生，重建后尺寸可能不符。
        // 整体销毁，下一帧按新尺寸懒重建。
        //
        // IMPORTANT: 此处可以安全销毁——`rebuild_swapchain` 开头已
        // `device_wait_idle`，在飞命令全部完成。
        if let Some(mut b) = self.post.take() {
            b.chain.destroy(&self.gpu.device);
            b.pipeline.destroy(self.gpu);
        }
        // 磨砂资源按交换链/渲染目标尺寸派生，同样整体销毁、下帧懒重建。
        // `rebuild_swapchain` 已 `device_wait_idle`，销毁安全。
        self.backdrop.destroy(self.gpu);
        // 用户纹理的描述符集合必须整体重新分配。
        //
        // `FrameRenderer::rebuild_swapchain` → `allocate_frame_resources`
        // 会**连描述符池一起销毁重建**（见 `destroy_frame_resources`），
        // 而每张用户纹理的集合都分配自那个池 ⇒ 重建后全部变成悬空句柄。
        // 照旧绑定就是「use-after-free 描述符」：验证层报
        // `VUID-vkCmdBindDescriptorSets-pDescriptorSets-00336`，实机上
        // 可能直接 device lost。图像本身还有效（不在池里），
        // 不能连图一起丢——egui 只在纹理变化时送增量，
        // 丢了就再也传不回来。
        //
        // 此刻 `rebuild_swapchain` 已 wait_idle，无在飞命令读旧集合，
        // 直接分配新集合替换即可（旧集合随池销毁，无需逐个 free）。
        if !self.user_textures.is_empty() {
            // 表非空 ⇒ paint() 至少跑过一次 ⇒ 占位图必然已建
            // （paint() 先建占位再处理纹理增量）。
            let placeholder = self
                .placeholder
                .as_ref()
                .expect("有用户纹理时占位图必已创建");
            // 字体视图取当前图集；图集还没建过时退回占位图
            // （与 `upload_user_texture` 的兜底一致）。
            let (font_view, font_layout) = match self.font.as_ref() {
                Some(f) => (f.view, f.layout),
                None => (placeholder.view, placeholder.layout),
            };
            let common = CommonBindings {
                sampler: self.sampler.handle(),
                user_sampler: self.sampler_linear.handle(),
                font_view,
                font_layout,
                uniform_buffer: self.uniform.buffer().handle(),
                uniform_offset: 0,
                uniform_range: self.uniform.buffer().size(),
            };
            for t in self.user_textures.values_mut() {
                t.set = fr.allocate_user_texture_set(common, t.image.view, t.image.layout)?;
            }
            tracing::debug!(
                sets = self.user_textures.len(),
                "交换链重建后已重配用户纹理描述符集合"
            );
        }
        self.stats.rebuilds += 1;
        Ok(())
    }

    /// 字体图集当前尺寸，`None` 表示尚未上传过。
    pub fn font_size(&self) -> Option<vk::Extent2D> {
        self.font.as_ref().map(|f| f.size)
    }

    /// 把 uniform 与纹理绑定写进**每一个**在飞槽位的描述符集。
    ///
    /// GPU 可能还在读上一个槽位的描述符集，只写当前帧的绑定会让其它帧读到
    /// 未初始化的描述符（表现为随机采样到垃圾纹理，或直接崩）。
    fn bind_all(&self, fr: &FrameRenderer<'a>, font: &DeviceImage) -> anyhow::Result<()> {
        for slot in 0..fr.slot_count() {
            fr.update_texture_binding(slot, self.sampler.handle(), font.view, font.layout)?;
            // 帧槽集合的用户纹理绑定永远写占位图：字体/形状批不采样它，
            // 而着色器静态声明了该绑定，任何被绑的集合都必须给它合法值。
            // 真正的缩略图用各自纹理的集合（`DrawBatch::user_set`）。
            if let Some(t) = self.placeholder.as_ref() {
                fr.update_user_texture_binding(
                    slot,
                    self.sampler.handle(),
                    t.view,
                    t.layout,
                )?;
            }
            fr.update_uniform_binding(
                slot,
                self.uniform.buffer().handle(),
                0,
                self.uniform.buffer().size(),
            )?;
        }
        tracing::debug!(slots = fr.slot_count(), "描述符绑定已全部写入");
        Ok(())
    }

    /// 应用 egui 的字体图集增量，返回本帧的上传次数。
    ///
    /// # 图集扩容
    ///
    /// egui 的字体图集**按需增长**：新字形放不下时整张重建，尺寸变化。
    /// 此时必须销毁旧图像并新建，图像视图随之改变 → 描述符需要重绑。
    /// 销毁前要 `device_wait_idle`：旧图像可能仍被在途命令引用。
    ///
    /// 应用 egui 的纹理增量：字体图集 + 用户纹理（图片缩略图）。
    ///
    /// # 两趟处理，顺序不能反
    ///
    /// 先字体图集、后用户纹理：用户纹理的描述符集里要写**字体图集
    /// 视图**，而 `delta.set` 是 `HashMap`，迭代顺序随机。若某帧同时
    /// 首帧创建字体图集与上传缩略图，先处理缩略图就会把 null 视图
    /// 写进它的集合（未定义行为）。分两趟后字体必然先就绪。
    ///
    /// # 用户纹理为什么是「一张表」而不是「一张图」
    ///
    /// 列表可同时显示多张缩略图，每张是各自的 egui `TextureId`。
    /// 只留一张会让所有图片行采样到同一张图。现在每张纹理带自己的
    /// 描述符集，`record_to` 画到该批前重绑（见 `DrawBatch::user_set`）。
    fn upload_font_delta(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<u64> {
        let mut uploads = 0u64;
        // 字体图集本趟是否被重建（视图变了 ⇒ 所有用户纹理集合要重写）。
        let mut font_view_changed = false;

        // ---- 第一趟：字体图集 -------------------------------------------
        for (id, deltas) in &delta.set {
            if *id != FONT_TEXTURE_ID {
                continue;
            }
            for d in deltas {
                let patch = d.image.size();
                let (w, h) = (patch[0] as u32, patch[1] as u32);
                anyhow::ensure!(w > 0 && h > 0, "字体图集增量的尺寸为 0：{patch:?}");

                // **只有整图更新才意味着图集扩容**。
                //
                // `ImageDelta::pos == None` 是「整图」的判据。局部更新的
                // `image.size()` 返回的是**补丁尺寸**——新字形触发时可能只有
                // 4x10 这么大。若拿它去比对图集尺寸，一次局部更新就会把
                // 2048x32 的图集换成 4x10 的小图，随后所有坐标全部越界。
                //
                // 这个 bug 由 `full_app` 实机跑出来：首帧成功渲染 552 个顶点，
                // 第二帧才崩在「更新区域 (1652, 0) 4x10 超出图像 4x10 范围」。
                //
                // egui 的 `TexturesDelta::push` 保证：整图增量会**替换**该 ID
                // 的全部历史增量，因此逐个判断是安全的。
                if d.pos.is_none() {
                    let need_new = match self.font.as_ref() {
                        None => true,
                        Some(f) => f.size.width != w || f.size.height != h,
                    };
                    if need_new {
                        if let Some(mut old) = self.font.take() {
                            self.gpu.wait_idle();
                            old.destroy(&self.gpu.device);
                        }
                        self.font = Some(DeviceImage::new(
                            self.gpu,
                            vk::Extent2D { width: w, height: h },
                            FONT_FORMAT,
                            vk::ImageUsageFlags::TRANSFER_DST
                                | vk::ImageUsageFlags::SAMPLED,
                        )?);
                        // 图集尺寸是「字形画错」类问题的关键数据，留一条 debug。
                        tracing::debug!(w, h, "字体图集纹理已重建");
                        // 视图换了：所有用户纹理集合里写的还是旧视图，
                        // 必须整体重写（上面刚 wait_idle，此刻无在飞读取）。
                        font_view_changed = true;
                    }
                }

                let bytes = DeviceImage::coverage_bytes(&d.image);
                let offset = d.pos.map(|p| (p[0] as u32, p[1] as u32));
                let f = self.font.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("字体图集收到局部更新 {offset:?}，但纹理尚未创建")
                })?;
                fr.record_texture_upload(f, &bytes, offset, (w, h))?;
                uploads += 1;
            }
        }

        // ---- 第二趟：用户纹理（图片缩略图）------------------------------
        //
        // 只取每个 ID 的**最后一个** delta：同一 TextureId 的后一次
        // 增量覆盖前一次（egui 语义）。
        //
        // CAUTION: 只处理**整图更新**（`pos == None`）。这个判据曾经
        // 写反过——`let Some(_pos) = last.pos else { continue }` 把
        // 整图更新当成「局部更新」跳过，缩略图纹理从未上传，
        // 症状是「缩略图区域一片空白」且不报任何 GPU 错误。
        // 局部更新（`pos == Some`）才没有对应能力：每张纹理都是
        // 独立整图，没有「子区域」概念。
        for (id, deltas) in &delta.set {
            if *id == FONT_TEXTURE_ID {
                continue;
            }
            let Some(last) = deltas.last() else { continue };
            if !is_full_image_update(last.pos) {
                tracing::debug!("跳过用户纹理 {id:?} 的局部更新");
                continue;
            }
            let [w, h] = last.image.size();
            let (w, h) = (w as u32, h as u32);
            if w == 0 || h == 0 {
                tracing::debug!("用户纹理 {id:?} 尺寸为 0，跳过");
                continue;
            }
            // CAUTION: 单张缩略图上传失败**就地降级**，不能往上冒泡。
            //
            // 2026-10-17 实测：缩略图分辨率提到 512² 后，首帧「字体图集
            // + 多张缩略图 + 桌面底图」会挤满 staging arena，上传返回
            // Err ⇒ 一路冒泡到 `run_remote` ⇒ 界面进程退出，用户只看到
            // 「无法打开界面」。一张缩略图是观感增强，它没有这个权限
            // （与磨砂底图同款处理，见 `paint_backdrop`）。
            //
            // 降级后把该纹理**移除**：图像尚未写入像素，留着会画出未定义
            // 内容；移除后下一帧会重新解码上传（像素仍在 `thumb_cache`，
            // 不会重复读盘）。
            if let Err(e) =
                self.upload_user_texture(fr, *id, w, h, &last.image, &last.options)
            {
                tracing::warn!(?id, w, h, "缩略图上传失败，本帧跳过该图: {e:#}");
                self.drop_user_texture(fr, *id);
                continue;
            }
            uploads += 1;
        }

        // 字体图集换了视图 ⇒ 重写全部用户纹理集合（此刻仍在录制之前，
        // 且重建路径已 wait_idle，无在飞命令读旧视图）。
        if font_view_changed {
            self.rewrite_user_texture_sets(fr)?;
        }

        // egui 的 `TexturesDelta` 在 drop 时断言「增量为空」。本函数已消费
        // 全部增量（字体图集 + 用户纹理），因此显式清空——否则 debug 断言
        // 误报，而那属于「帧循环写法不对」，不该掩盖真正的失败。
        delta.clear();
        Ok(uploads)
    }

    /// 上传（首建/重建）一张用户纹理，并分配或复用它自己的描述符集。
    ///
    /// 同尺寸的重复上传只传像素（图像与集合都不动）——缩略图滚动时
    /// 同一 `TextureId` 不会重复送增量，这条路径主要服务详情大图。
    fn upload_user_texture(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        id: egui::TextureId,
        w: u32,
        h: u32,
        image: &egui::ImageData,
        options: &egui::TextureOptions,
    ) -> anyhow::Result<()> {
        let same_size = self
            .user_textures
            .get(&id)
            .is_some_and(|t| t.image.size.width == w && t.image.size.height == h);
        if !same_size {
            // 尺寸变了（或首次见到）：旧图与旧集合都可能被在途命令
            // 引用，必须等 GPU 空闲再销毁/归还。
            if let Some(mut old) = self.user_textures.remove(&id) {
                self.gpu.wait_idle();
                old.image.destroy(&self.gpu.device);
                fr.free_user_texture_set(old.set)?;
            }
            // 环绕采样按纹理逐张开（egui 的 TextureOptions 语义）：
            // 缩略图 clamp、程序化质感 tile repeat。采样器绑在描述符
            // 集合里，集合只在尺寸变化时重建，因此这里选一次就定。
            let wrap_repeat = options.wrap_mode == egui::TextureWrapMode::Repeat
                || options.wrap_mode == egui::TextureWrapMode::MirroredRepeat;
            let user_sampler = if wrap_repeat {
                self.sampler_linear_repeat.handle()
            } else {
                self.sampler_linear.handle()
            };
            // `mut`：集合分配失败的分支要就地销毁它（见下）。
            let mut img = DeviceImage::new(
                self.gpu,
                vk::Extent2D { width: w, height: h },
                vk::Format::R8G8B8A8_UNORM,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            )?;
            // 字体视图取当前图集；首帧图集可能还没建（本函数在两趟
            // 处理的第二趟，正常已有），兜底用占位图——占位在 paint()
            // 里先于本函数创建，必然存在。
            let (font_view, font_layout) = match self.font.as_ref() {
                Some(f) => (f.view, f.layout),
                None => {
                    let p = self
                        .placeholder
                        .as_ref()
                        .expect("占位纹理应先于用户纹理创建");
                    (p.view, p.layout)
                }
            };
            let set = match fr.allocate_user_texture_set(
                CommonBindings {
                    sampler: self.sampler.handle(),
                    user_sampler,
                    font_view,
                    font_layout,
                    uniform_buffer: self.uniform.buffer().handle(),
                    uniform_offset: 0,
                    uniform_range: self.uniform.buffer().size(),
                },
                img.view,
                img.layout,
            ) {
                Ok(s) => s,
                Err(e) => {
                    // 集合分配失败时 `img` 还没入表，就地销毁再返回。
                    // 不销毁的代价是每次失败漏一张全尺寸 RGBA 图
                    // （512² = 1MB），失败路径会反复走到。
                    self.gpu.wait_idle();
                    img.destroy(&self.gpu.device);
                    return Err(e);
                }
            };
            self.user_textures.insert(
                id,
                UserTexture {
                    image: img,
                    set,
                    wrap_repeat,
                },
            );
            // LRU 位置由函数尾统一 touch（两条路径都要），这里不重复调。
            self.evict_user_textures_if_needed(fr)?;
        }
        // egui 的 `ColorImage.pixels` 是预乘 alpha 的 `Color32`，
        // 展平为 RGBA 字节（与 `coverage_bytes` 同样的取法，
        // 只是保留全部 4 通道而非取单通道）。
        let rgba: Vec<u8> = match image {
            egui::ImageData::Color(img) => img.pixels.iter().flat_map(|p| p.to_array()).collect(),
        };
        if let Some(t) = self.user_textures.get_mut(&id) {
            fr.record_texture_upload(&mut t.image, &rgba, None, (w, h))?;
        }
        // IMPORTANT: **两条路径都要**刷新 LRU 位置。
        //
        // 同尺寸重复上传不会重建纹理（上面 `same_size` 分支跳过），但它
        // 同样是「这张纹理这一帧正在被用」的证据。不touch 的后果：
        // 同一张图反复重传（同尺寸详情图切换、缩略图重解码出同尺寸）
        // 却一直挂在 LRU 队首附近，被 `evict_user_textures_if_needed`
        // 当成最久未用淘汰 ⇒ 下一帧又得重新上传（一次完整 GPU 上传，
        // 512² 缩略图 = 1MB），滚动时反复发生。
        self.touch_user_order(id);
        Ok(())
    }

    /// 字体图集换视图后，重写全部用户纹理集合里的字体绑定。
    fn rewrite_user_texture_sets(&self, fr: &FrameRenderer<'a>) -> anyhow::Result<()> {
        let Some(font) = self.font.as_ref() else {
            return Ok(());
        };
        for t in self.user_textures.values() {
            // 按各纹理自己的 wrap 选采样器（见 `UserTexture::wrap_repeat`）。
            let user_sampler = if t.wrap_repeat {
                self.sampler_linear_repeat.handle()
            } else {
                self.sampler_linear.handle()
            };
            fr.rewrite_user_texture_set(
                t.set,
                CommonBindings {
                    sampler: self.sampler.handle(),
                    user_sampler,
                    font_view: font.view,
                    font_layout: font.layout,
                    uniform_buffer: self.uniform.buffer().handle(),
                    uniform_offset: 0,
                    uniform_range: self.uniform.buffer().size(),
                },
                t.image.view,
                t.image.layout,
            )?;
        }
        Ok(())
    }

    /// LRU 淘汰：超过 [`USER_TEXTURE_SET_CAP`] 时销毁最久未用的纹理。
    ///
    /// 图像与描述符集都可能被在飞命令引用，销毁前 `wait_idle`。
    /// 被淘汰的纹理若仍在屏幕上，该批会退化为采样占位图（空白），
    /// 等它滚出屏再滚回时 paint 侧重传、这里重建——自愈。
    fn evict_user_textures_if_needed(&mut self, fr: &FrameRenderer<'a>) -> anyhow::Result<()> {
        while self.user_textures.len() > USER_TEXTURE_SET_CAP as usize {
            // 队首可能已是残留键（淘汰/替换时未同步），取第一个仍在表里的。
            let Some(victim) = self.user_order.pop_front() else {
                break;
            };
            if let Some(mut t) = self.user_textures.remove(&victim) {
                self.gpu.wait_idle();
                t.image.destroy(&self.gpu.device);
                fr.free_user_texture_set(t.set)?;
            }
        }
        Ok(())
    }

    /// 按键移除一张用户纹理（上传失败降级用）。
    ///
    /// 与 [`Self::evict_user_textures_if_needed`] 的单次版本区别：
    /// - 表里没有这一项时**静默返回**——失败可能发生在建图阶段
    ///   （`DeviceImage::new` / 集合分配），那时表里本来就没有它；
    /// - 同时清 LRU 队列里的残留键（`touch_user_order` 在建表前就可能
    ///   已写过队尾，见淘汰函数里「队首可能有残留键」的注释）。
    ///
    /// `wait_idle` 的纪律与淘汰一致：图像与集合都可能被在飞命令引用。
    ///
    /// 返回 `()` 而不是 `Result`：这是**清理**路径，调用方（上传失败
    /// 降级）已经处于异常分支，此时归还描述符集再失败也只能记日志——
    /// 漏一个集合远好过把异常继续往上抛。返回 Result 会让调用方被迫
    /// 再包一层 `if let Err`，双重告警纯属噪音。
    fn drop_user_texture(&mut self, fr: &FrameRenderer<'a>, id: egui::TextureId) {
        self.user_order.retain(|k| *k != id);
        if let Some(mut t) = self.user_textures.remove(&id) {
            self.gpu.wait_idle();
            t.image.destroy(&self.gpu.device);
            if let Err(e) = fr.free_user_texture_set(t.set) {
                tracing::warn!(?id, "归还用户纹理描述符集失败（集合将泄漏）: {e:#}");
            }
        }
    }

    /// 把某个纹理移到 LRU 队尾（最近使用）。
    fn touch_user_order(&mut self, id: egui::TextureId) {
        if let Some(pos) = self.user_order.iter().position(|k| *k == id) {
            self.user_order.remove(pos);
        }
        self.user_order.push_back(id);
    }
}

/// 磨砂移动预测的 UV 位移（纯函数，便于单测）。
///
/// `pred` / `captured` 都是**屏幕物理像素**坐标，`extent` 是客户区
/// **物理像素**尺寸；返回**归一化** UV 位移。三者量纲必须一致，
/// 详见 [`Painter::backdrop_uv_offset`] 的说明（那里解释了为什么
/// 分母不能用 `render_extent`）。
pub(super) fn backdrop_uv_offset_from(
    pred: (f32, f32),
    captured: (f32, f32),
    extent: vk::Extent2D,
) -> [f32; 2] {
    let dx = pred.0 - captured.0;
    let dy = pred.1 - captured.1;
    let w = extent.width.max(1) as f32;
    let h = extent.height.max(1) as f32;
    [dx / w, dy / h]
}

impl Drop for Painter<'_> {
    fn drop(&mut self) {
        // 必须先让 GPU 彻底空闲：下面这些对象都可能被在途命令引用。
        self.gpu.wait_idle();
        if let Some(mut f) = self.font.take() {
            f.destroy(&self.gpu.device);
        }
        // NOTE: 早前**漏了** `placeholder` 与用户纹理。
        //
        // 用户纹理表里每张各持有 image + image view + DeviceMemory
        // 三样 Vulkan 对象，`placeholder` 同理。后者在第一次需要纹理时
        // 就会创建，之后**每个窗口都会有一份**（每个窗口一个 `Painter`），
        // 而设置窗口是「每次开关都新建」⇒ 反复开关设置会持续泄漏。
        //
        // 退出时验证层还会因此报 `VUID-vkDestroyDevice-device-05137`
        // （销毁 device 时仍有活对象）。
        //
        // 描述符集**不在这里释放**：它们属于 `FrameRenderer` 的描述符池，
        // 池销毁时统一收回；本结构只销毁自己创建的图像。
        for (_, mut t) in self.user_textures.drain() {
            t.image.destroy(&self.gpu.device);
        }
        self.user_order.clear();
        if let Some(mut t) = self.placeholder.take() {
            t.destroy(&self.gpu.device);
        }
        // 后处理资源（离屏图 + framebuffer + 管线 + 描述符池）同样必须销毁：
        // 漏掉会让验证层在退出时报 `VUID-vkDestroyDevice-device-05137`。
        if let Some(mut b) = self.post.take() {
            b.chain.destroy(&self.gpu.device);
            b.pipeline.destroy(self.gpu);
        }
        // 磨砂背景资源同理：漏销毁会在退出时报 VUID-vkDestroyDevice-device-05137。
        self.backdrop.destroy(self.gpu);
        self.sampler.destroy(&self.gpu.device);
        self.sampler_linear.destroy(&self.gpu.device);
        self.slots.destroy(&self.gpu.device);
        self.uniform.destroy(&self.gpu.device);
    }
}
