//! egui 绘制桥：把 [`egui::ClippedPrimitive`] 变成 GPU 可执行的一帧。
//!
//! # 职责边界
//!
//! | 层 | 负责 |
//! |----|------|
//! | [`modular_clipboard_gfx::frame::FrameRenderer`] | 交换链、渲染通道开启、命令录制与提交、呈现、重建 |
//! | 本模块（[`Painter`]） | 顶点数据构造、缓冲上传、uniform、描述符绑定 |
//! | `modular-clipboard-gfx` 资源层 | 缓冲/纹理的创建、销毁、显存分配 |
//!
//! 本模块**不含 Win32 代码**，也不持有 [`egui::Context`]——输入由
//! [`modular_clipboard_gfx::window::EventLoop::egui_input`] 翻译成 [`egui::RawInput`]，
//! tessellation 由调用方用 `Context::tessellate` 完成。
//! 这样同一套绘制逻辑既能被 `full_app` 探针用，也能被正式应用用。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::backdrop::{
    capture_due, capture_size, client_origin, DesktopCapture, MotionPredictor, CAPTURE_INTERVAL,
};
use modular_clipboard_gfx::blur::{BlurChain, BlurPipeline, DEFAULT_DIVISOR, DEFAULT_RADIUS};
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{
    CommonBindings, DrawInput, FrameRenderer, USER_TEXTURE_SET_CAP,
};
use modular_clipboard_gfx::pipeline::Uniforms;
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// 窗口标题，同时作为 Vulkan 应用标识。
/// 主窗口标题。
///
/// 单一数据源在 `modular-clipboard-ipc`：常驻进程要用同一个字符串
/// `FindWindow` 来唤起界面（托盘点击 / 重复双击图标）。两边各写一份时，
/// 改标题会让「点了托盘没反应」，且很难联想到是标题漂移。
pub const WINDOW_TITLE: &str = modular_clipboard_ipc::UI_WINDOW_TITLE;

/// 顶点缓冲初始容量（顶点数）。不足时按 [`next_capacity`] 扩容。
pub const INITIAL_VERTEX_CAPACITY: usize = 8192;

/// 索引缓冲初始容量（索引数）。
pub const INITIAL_INDEX_CAPACITY: usize = 12288;

/// egui 约定 `Managed(0)` 恒为字体图集（见 `epaint::TextureId` 的文档）。
///
/// 改错这个常量会让所有文字变成豆腐块——字体图集不会被采样，
/// 而图集左上角那个「纯白像素」正是 `WHITE_UV` 指向的位置。
pub const FONT_TEXTURE_ID: egui::TextureId = egui::TextureId::Managed(0);

/// 着色器里的「采样哪张纹理」：`0` = 字体图集，`1` = 用户纹理。
pub const TEX_SLOT_FONT: u32 = 0;
/// 见 [`TEX_SLOT_FONT`]。
pub const TEX_SLOT_USER: u32 = 1;

/// 把 egui 的 [`egui::TextureId`] 映射到着色器用的纹理槽位号。
///
/// # 为什么压成 0/1 而不是透传 `TextureId`
///
/// egui 的 `TextureId` 是 `Managed(u64) | Viewport(u32)`，直接传下去需要
/// 着色器侧再解一次枚举。缩略图只有「字体」与「非字体」两类，
/// 着色器一个 `if` 就够⇒ 顶点里带一个 `u32` 槽位号即可。
///
/// # 未知纹理一律当作用户纹理
///
/// 字体图集是 `Managed(0)`（egui 的硬约定）。其余任何纹理都归到
/// 「用户纹理」槽——出错时表现为**采样一张空白/错误图**，
/// 而静默把字体当用户纹理会让**所有文字消失**（字体图集是
/// 单通道覆盖率图，当彩色图采样出来几乎全黑）。
/// 两种错法都不报错，但后者影响面大得多。
pub fn texture_slot(id: egui::TextureId) -> u32 {
    if id == FONT_TEXTURE_ID {
        TEX_SLOT_FONT
    } else {
        TEX_SLOT_USER
    }
}

/// 纹理增量是否为「整图更新」。
///
/// # 这个判据曾经写反过（2026-10-16 修）
///
/// `upload_font_delta` 早前写成 `let Some(_pos) = last.pos else { continue }`
/// ——把 `pos == None`（**整图更新**，`ctx.load_texture` 的唯一形态）
/// 当成「局部更新」跳过 ⇒ 缩略图纹理从未上传，症状是
/// **「缩略图区域一片空白」且不报任何 GPU 错误**（描述符绑的是
/// 占位图，一切合法，只是没有内容）。
///
/// egui 语义（`epaint::ImageDelta`）：`pos == None` ⇒ 整图替换；
/// `Some(offset)` ⇒ 纹理内某个子区域的补丁。
pub fn is_full_image_update(pos: Option<[usize; 2]>) -> bool {
    pos.is_none()
}

// ---------------------------------------------------------------------------
// 批次
// ---------------------------------------------------------------------------

/// 一次绘制批次。
///
/// 比 [`modular_clipboard_gfx::frame::DrawBatch`] 多带 `clip` 与 `tex_id`：
/// 合并判定需要知道相邻两段是否属于同一裁剪区、同一张纹理，
/// 而 `frame::DrawBatch` 不暴露这两个信息。提交前用 [`Batch::to_draw`] 转换。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Batch {
    /// 索引缓冲中的起始索引。
    pub index_offset: u32,
    /// 索引个数。
    pub index_count: u32,
    /// 本段的裁剪矩形，仅用于合并判定，不提交给 GPU。
    pub clip: egui::Rect,
    /// 本段采样的纹理。
    ///
    /// # 为什么必须逐批记录
    ///
    /// 引入用户纹理（图片缩略图）后，一次绘制里会存在**两种**纹理：
    /// 字体图集与缩略图。若合批时只看 clip 与索引连续性，
    /// 两块不同纹理的图元会被并进同一批，而该批只绑一张纹理
    /// ⇒ 其中一半图元采样到错误纹理。
    ///
    /// 症状是「缩略图位置显示成字体图集里的某个字形」，
    /// **不报任何 GPU 错误、验证层也是干净的**——最难查的一类。
    ///
    /// egui 已经把每块 mesh 的纹理 ID 交给我们（`epaint::Mesh::texture_id`），
    /// 这里只是此前一直没读它。
    pub tex_id: egui::TextureId,
}

impl Batch {
    /// 转成提交给渲染层的批次，并把裁剪矩形换算成**物理像素**。
    ///
    /// # 为什么必须在这里换算
    ///
    /// `prim.clip_rect` 是**逻辑点**，而 `vk::Rect2D` 要的是物理像素且
    /// 取整。渲染层（`gfx::frame`）不持有 `ppp`，所以换算只能在这里做。
    ///
    /// #NOTE: 换算里的三个坑
    ///
    /// 1. **负坐标必须钳到 0**。`painter_at` 允许传入屏幕外/负值的矩形
    ///    （例如浮层被拖到窗口左侧），`offset` 为负会让 `cmd_set_scissor`
    ///    触发 Vulkan 的 `vk::Scissor` 非法值（`x + extent <= x`），
    ///    规范下属于**未定义行为**——可能整帧不渲染或校验层报错。
    /// 2. **宽高必须至少 1 像素**。宽度为 0 的 scissor 是合法的「什么都不画」，
    ///    但 `extent` 为 0 且 `offset` 也为 0 时同样踩到上面的边界条件；
    ///    钳成 0×0 会被下面 `if w == 0` 分支整体跳过，语义更明确。
    /// 3. **必须与交换链尺寸求交**。裁剪矩形超出视口时 Vulkan 会把
    ///    矩形裁到视口内（这是合法的），但显式求交能让「整块被裁掉」
    ///    的情形走跳过分支，省掉一次 `draw_indexed`。
    pub fn to_draw(self, extent: vk::Extent2D, pixels_per_point: f32) -> modular_clipboard_gfx::frame::DrawBatch {
        let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };
        let c = self.clip;
        let x0 = (c.min.x * ppp).floor().max(0.0) as i32;
        let y0 = (c.min.y * ppp).floor().max(0.0) as i32;
        let x1 = (c.max.x * ppp).ceil().max(0.0) as i32;
        let y1 = (c.max.y * ppp).ceil().max(0.0) as i32;
        // 与视口求交（extent 是 u32，转 i64 避免溢出）。
        let vw = extent.width as i64;
        let vh = extent.height as i64;
        let x0 = x0.clamp(0, vw as i32);
        let y0 = y0.clamp(0, vh as i32);
        let x1 = x1.clamp(0, vw as i32);
        let y1 = y1.clamp(0, vh as i32);
        let w = (x1 - x0).max(0) as u32;
        let h = (y1 - y0).max(0) as u32;

        modular_clipboard_gfx::frame::DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
            // 宽或高为 0 ⇒ 整批被裁掉，用零 scissor（合法的「不画」）
            // 而不是 `None`：后者会退回全屏，等于裁剪没生效。
            clip: Some(vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D { width: w, height: h },
            }),
            // 默认不重绑（文字/形状批）。缩略图批由 paint() 在转换后
            // 按 `tex_id` 从 `Painter::user_textures` 查表填上该纹理
            // 的描述符集——本方法拿不到纹理表。
            user_set: vk::DescriptorSet::null(),
        }
    }
}

// ---------------------------------------------------------------------------
// tessellation：egui 图元 → 顶点数据
// ---------------------------------------------------------------------------

/// 把一批 [`egui::epaint::ClippedPrimitive`] 转换后写入三个向量
/// （三者都会被先清空，可反复复用而不重新分配）。
///
/// # 返回值：模糊分割点
///
/// 返回**首个** `Primitive::Callback` 出现时已生成的批次数。
///
/// # 为什么用 `PaintCallback` 当分割标记
///
/// 自研模糊要求把界面拆成「覆盖层之前的内容（要被模糊）」与
/// 「覆盖层本身（保持清晰）」两段。`ClippedPrimitive` 本身不带
/// 「这是第几段」的信息，而 `PaintCallback` 是 egui **自带**的
/// 「此处插入自定义绘制」机制——把它当成渲染层与 UI 层之间的
/// 分段信号，不需要额外发明协议，且它在 tessellation 里必然保留
/// 为一个独立的 primitive（不会与相邻网格合并）。
///
/// 没有标记时返回 `batches.len()`，调用方据此走普通绘制路径。
///
/// # 索引偏移
///
/// egui 的 `Mesh::indices` 是**相对该 mesh 自身顶点数组**的下标。多个 mesh
/// 拼进同一顶点缓冲时必须加上「本 mesh 之前已有多少顶点」，否则第二个 mesh
/// 会去索引第一个 mesh 的顶点——表现为界面元素错位，且**不触发任何 GPU 错误**。
///
/// # 批次合并
///
/// `clip_rect` 相同的**相邻**图元合并成一次 `draw_indexed`。着色器不处理
/// per-batch scissor（egui 在 CPU 侧 tessellation 时已按 `clip_rect` 裁掉
/// 几何），因此合并不改变画面，只减少 draw call。
pub fn tessellate_into(
    primitives: &[egui::epaint::ClippedPrimitive],
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
) -> usize {
    vertices.clear();
    indices.clear();
    batches.clear();
    // 首个 Callback 出现时的批次数。`None` 表示本帧没有分割标记。
    let mut blur_split: Option<usize> = None;

    for prim in primitives {
        let mesh = match &prim.primitive {
            egui::epaint::Primitive::Mesh(m) => m,
            // PaintCallback 需要用户代码参与光栅化，本渲染器不支持。
            //
            // 但**不丢弃它的位置信息**：它标记了「模糊分割点」——
            // 之前的批次画到离屏场景图并卷积，之后的批次保持清晰。
            // 只取第一个标记（一帧内最多一段模糊背景）。
            egui::epaint::Primitive::Callback(_) => {
                blur_split.get_or_insert(batches.len());
                continue;
            }
        };
        if mesh.indices.is_empty() {
            continue;
        }

        let base = vertices.len() as u32;
        let index_start = indices.len() as u32;
        let index_count = mesh.indices.len() as u32;

        for v in &mesh.vertices {
            vertices.push(Vertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: pack_color(v.color),
                tex_id: texture_slot(mesh.texture_id),
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));

        // 合批的三个条件：同裁剪区、同纹理、索引连续。
        // 索引连续由上面的追加式写入保证。
        //
        // `tex_id` 这条是引入用户纹理后**必须**加的：漏掉它会让不同纹理的
        // 图元并进同一批，而该批只绑一张纹理 ⇒ 采样到错误纹理，
        // 且不产生任何 GPU 错误。见 `Batch::tex_id` 的说明。
        let merge = batches.last().is_some_and(|b| {
            b.clip == prim.clip_rect
                && b.tex_id == mesh.texture_id
                && b.index_offset + b.index_count == index_start
        });
        if merge {
            let last = batches.last_mut().expect("刚判定过非空");
            last.index_count += index_count;
        } else {
            batches.push(Batch {
                index_offset: index_start,
                index_count,
                clip: prim.clip_rect,
                tex_id: mesh.texture_id,
            });
        }
    }

    // 没有分割标记时返回批次数（等价于「全部内容」），调用方据此走普通路径。
    blur_split.unwrap_or(batches.len())
}

/// egui 的 `Color32` → 顶点色 `u32`（小端 RGBA，着色器按
/// `unpack4x8unorm` 逐字节展开）。
///
/// # IMPORTANT: 必须原样上传，**不能**反预乘
///
/// epaint 的顶点色是 **sRGBA 预乘 alpha**（`r,g,b` 已乘过 `a`），而
/// `pipeline.rs` 的混合是预乘公式 `ONE / ONE_MINUS_SRC_ALPHA`（与
/// egui-wgpu 一致）。两者恰好配套：预乘色交给预乘混合，一次完成。
///
/// 早前这里做过一次「反预乘」——依据是当时认为混合是直通公式
/// （`SRC_ALPHA`）。后来管线为修「圆角羽化边缘发暗」改成了预乘公式，
/// 这里的反预乘却没有跟着删，于是 alpha 被抵消两次：
/// 半透明填充（选中行 `row_selected`、悬停 `row_hover`、遮罩）的 RGB
/// 以**全强度**叠加进画面，超出 1.0 的通道被裁剪——浅色主题下
/// 选中行 (29,101,175)@α52 叠在近白底上裁成 (228,255,255)，即
/// 用户看到的「选中态发青偏绿」。文字不受影响（alpha=255 时反预乘
/// 是恒等），所以这个错误长期只在这类低 alpha 大面积填充上显形。
///
/// 着色器输出 `color * vec4(1,1,1,cov)`（覆盖率乘全部四通道）：
/// 预乘色 × 覆盖率仍是预乘色，正是预乘「over」期望的形式。
pub fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    u32::from_le_bytes([r, g, b, a])
}

/// 构造本帧的 uniform。
///
/// `clip_from_uv` 传单位矩阵：着色器内部已把像素坐标换算成 NDC（并做了 y 轴
/// 翻转），再乘一个矩阵只会引入二次变换。
///
/// # IMPORTANT: 坐标系不变式（务必与 `EventLoop::egui_input` 成对阅读）
///
/// * `size_in_pixels` = **物理像素**（交换链尺寸），来自 [`Painter::paint`] 的
///   `fr.extent()`；
/// * `pixels_per_point` = egui 的**逻辑点→物理像素**比例，即
///   [`egui::FullOutput::pixels_per_point`]；
/// * 着色器算`ndc = 2 * (pos * dpr) / size_in_pixels`，其中 `pos` 是
///   `epaint` 吐出的**逻辑点**（`epaint::Vertex::pos` 的文档即如此）。
///
/// 于是必须满足
///
/// ```text
/// screen_rect.size() * pixels_per_point == size_in_pixels
/// ```
///
/// 这与 egui 官方后端 `egui-wgpu` 0.36 的写法**完全等价**：官方把
/// `size_in_pixels / ppp`（逻辑点）塞进 uniform，顶点着色器直接
/// `2 * pos / screen_size` 而不乘 dpr。同一件事的两种写法，本项目选
/// 「物理视口 + 乘 dpr」。
///
/// NOTE: 只要 `ppp` 被漏设成 1.0（DPI 144 时真实值是 1.5），这条不变式就破了：
/// 布局按逻辑点算好、换算按物理视口算，于是内容只占视口的 1/1.5，
/// 右侧与底部各空 1/3，且**不产生任何 GPU 错误**。
pub fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
    Uniforms::new(
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
        [extent.width as f32, extent.height as f32],
        pixels_per_point,
    )
}

/// uniform 的内容指纹。变了才需要重写缓冲。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UniformKey {
    pub width: u32,
    pub height: u32,
    /// egui 的逻辑点→物理像素比例。
    ///
    /// NOTE: 注意命名不一致：本字段叫 `pixels_per_point`，而它写进去的
    /// [`crate::pipeline::Uniforms`] 里对应字段叫 **`dpr`**。
    /// 同一个值两个名字，改动时容易写错。
    pub pixels_per_point: f32,
}

/// 扩容策略：1.5 倍并向上取整到 1024，避免「每帧只多一个元素就重建」。
pub fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

/// egui 请求的下一帧延迟。`None` 表示「有事件时立即处理，不要 sleep」。
/// 本帧之后还需等多久再重绘（`None` = 立即重绘）。
///
/// # egui 的 `repaint_delay` 语义（很容易搞反）
///
/// | 值 | 含义 |
/// |---|---|
/// | `Duration::MAX` | **不重绘**——初始值，等事件驱动 |
/// | `Duration::ZERO` | **立即重绘** |
/// | `Duration::from_millis(n)` | n 毫秒后重绘 |
///
/// 出处：`egui::Context` 的 `ViewportRepaintInfo::default()` 把
/// `repaint_delay` 设为 `Duration::MAX`（注释写着 "We haven't
/// scheduled a repaint yet."），而 `request_repaint()` 才置 ZERO。
///
/// NOTE: 早前一版把 `ZERO` 映射成 `None` 并注释说
///「ZERO 表示立即重绘，应转成 None 让调用方走有事件才处理」——
/// **语义反了**：调用方拿到 None 就不会再触发这一帧重绘，
/// 于是「有事件才处理」的路径永远等不到这次立即重绘，
/// 表现为控件状态该变却没变（聚焦高亮、光标闪烁都靠它驱动）。
pub fn repaint_delay(output: &egui::FullOutput) -> Option<std::time::Duration> {
    let v = output.viewport_output.get(&egui::ViewportId::ROOT)?;
    match v.repaint_delay {
        // Duration::MAX = egui 明确表示「这一帧不需要重绘」。
        d if d == std::time::Duration::MAX => None,
        // ZERO = 立即重绘，调用方应尽快跑下一帧。
        d => Some(d),
    }
}

// ---------------------------------------------------------------------------
// Painter
// ---------------------------------------------------------------------------

/// 一张用户纹理（图片缩略图）在 GPU 侧的全部资源。
///
/// `set` 是**这张纹理专属**的描述符集：uniform / sampler / 字体图集
/// 与帧槽集合相同，只有 `BINDING_USER_TEXTURE` 指向本图的视图。
/// 批次绘制前按需重绑（见 `DrawBatch::user_set`）。
struct UserTexture {
    image: DeviceImage,
    set: vk::DescriptorSet,
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
    /// 磨砂是否已降级过（只记一次告警，避免每帧刷屏）。
    backdrop_warned: bool,
    /// 累计统计，供上层显示与诊断。
    pub stats: FrameStats,
}

/// 一帧后处理所需的全部 GPU 资源。
///
/// 生命周期与「窗口的**渲染目标**尺寸」绑定：尺寸或超采样倍率变化时
/// 整体重建。重建前必须 `device_wait_idle`——旧的 framebuffer 与
/// 图像可能仍被在途命令引用。
struct PostFx {
    pipeline: BlurPipeline,
    chain: BlurChain,
}

/// 自研磨砂的一帧资源：模糊管线 + 链 + 捕获纹理。
struct BackdropFx {
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
struct BackdropState {
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
enum PaintMode {
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
            backdrop_warned: false,
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
            // 夹到 [1,3] 与 `render_scale()` 的取值域一致，防脏数据。
            (pixels_per_point / dpi_scale).clamp(1.0, 3.0)
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

        // 超采样倍率为 1 时不做离屏，直接画到交换链（最省的老路径，
        // 也是出问题时的逃生开关）。
        if ssaa <= 1.0 {
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
fn backdrop_uv_offset(&self, swap_extent: vk::Extent2D) -> [f32; 2] {
    let (Some(cap), Some(_cur)) = (self.backdrop.capture_origin, self.backdrop.origin) else {
        return [0.0, 0.0];
    };
    let pred = self
        .backdrop
        .predictor
        .predict_next_frame()
        .unwrap_or((cap.0, cap.1));
    let dx = pred.0 - cap.0;
    let dy = pred.1 - cap.1;
    let w = swap_extent.width.max(1) as f32;
    let h = swap_extent.height.max(1) as f32;
    [dx / w, dy / h]
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
                self.warn_backdrop("拿不到当前命令缓冲");
                return Ok(());
            }
        };
        let render_pass = fr.render_pass();
        let gpu = self.gpu;
        if let Err(e) = fr.record_texture_upload(&mut fx.capture_img, bits, None, (w, h)) {
            self.warn_backdrop(&format!("桌面位图上传失败: {e:#}"));
            return Ok(());
        }
        // 写入本帧的卷积半径（磨砂强度）。描述符集指向的是同一块 uniform
        // 缓冲，改完紧接着的 record_from 即按新半径卷积——无需重建管线。
        if let Err(e) = fx.pipeline.set_radius(gpu, &fx.chain, blur_radius) {
            self.warn_backdrop(&format!("磨砂强度更新失败: {e:#}"));
            return Ok(());
        }
        if let Err(e) = fx.pipeline.record_from(
            gpu,
            cmd,
            &mut fx.chain,
            render_pass,
            fx.pipeline.backdrop_set(),
        ) {
            self.warn_backdrop(&format!("模糊链录制失败: {e:#}"));
            return Ok(());
        }
        fx.uploaded = true;
        *last_capture = Some(now);
        // 到这里拆借已全部用完，可以安全写回捕获原点（引用在 NLL
        // 下于最后一次使用后结束；`fx.uploaded` 这行是最后一次）。
        self.backdrop.capture_origin = capture_succeeded;
        Ok(())
    }

    /// 磨砂降级告警：只记一次，避免每帧刷屏。
    fn warn_backdrop(&mut self, msg: &str) {
        if !self.backdrop_warned {
            self.backdrop_warned = true;
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
            if let Err(e) = self.upload_user_texture(fr, *id, w, h, &last.image) {
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
            self.user_textures.insert(id, UserTexture { image: img, set });
            self.touch_user_order(id);
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
        Ok(())
    }

    /// 字体图集换视图后，重写全部用户纹理集合里的字体绑定。
    fn rewrite_user_texture_sets(&self, fr: &FrameRenderer<'a>) -> anyhow::Result<()> {
        let Some(font) = self.font.as_ref() else {
            return Ok(());
        };
        for t in self.user_textures.values() {
            fr.rewrite_user_texture_set(
                t.set,
                CommonBindings {
                    sampler: self.sampler.handle(),
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
        self.slots.destroy(&self.gpu.device);
        self.uniform.destroy(&self.gpu.device);
    }
}

// ---------------------------------------------------------------------------
// 顶点/索引缓冲
// ---------------------------------------------------------------------------

/// 按在飞槽位数量复制的顶点/索引缓冲环。
///
/// # 为什么是「每槽位一份」而不是「全局一份」
///
/// 帧层是多帧在途的：`present` 只提交、不等待。若顶点数据只有一份，下一帧的
/// 写入会覆写 GPU **仍在读取**的内存，画面会随机撕裂。
///
/// 按槽位各存一份、只写 `acquire` 刚给出的那个槽位，则天然安全。
struct SlotBuffers {
    buffers: Vec<SlotPair>,
}

struct SlotPair {
    /// `None` 表示已被移出（销毁流程中）。
    vertex: Option<Buffer>,
    index: Option<Buffer>,
    vertex_capacity: usize,
    index_capacity: usize,
}

impl SlotBuffers {
    fn new(
        gpu: &Gpu,
        slots: usize,
        vertex_capacity: usize,
        index_capacity: usize,
    ) -> anyhow::Result<Self> {
        let mut buffers = Vec::with_capacity(slots);
        for _ in 0..slots {
            buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vertex_capacity)?),
                index: Some(new_index_buffer(gpu, index_capacity)?),
                vertex_capacity,
                index_capacity,
            });
        }
        Ok(Self { buffers })
    }

    /// 调整槽位数量（交换链重建后图像数可能变化）。
    fn resize(&mut self, gpu: &Gpu, slots: usize) -> anyhow::Result<()> {
        while self.buffers.len() > slots {
            let mut p = self.buffers.pop().expect("长度已确认");
            p.destroy(&gpu.device);
        }
        let (vc, ic) = self
            .buffers
            .first()
            .map(|p| (p.vertex_capacity, p.index_capacity))
            .unwrap_or((INITIAL_VERTEX_CAPACITY, INITIAL_INDEX_CAPACITY));
        while self.buffers.len() < slots {
            self.buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vc)?),
                index: Some(new_index_buffer(gpu, ic)?),
                vertex_capacity: vc,
                index_capacity: ic,
            });
        }
        Ok(())
    }

    /// 把本帧数据写进指定槽位，必要时扩容。
    fn write(
        &mut self,
        gpu: &Gpu,
        slot: usize,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> anyhow::Result<()> {
        let count = self.buffers.len();
        let p = self
            .buffers
            .get_mut(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位 {slot} 越界（共 {count} 个）"))?;

        if vertices.len() > p.vertex_capacity {
            // 扩容要重建缓冲。上一轮占用该槽位的 GPU 工作已被 acquire 的
            // 栅栏等待保证完成，因此销毁旧缓冲是安全的。
            let cap = next_capacity(vertices.len());
            if let Some(mut old) = p.vertex.take() {
                old.destroy(&gpu.device);
            }
            p.vertex = Some(new_vertex_buffer(gpu, cap)?);
            p.vertex_capacity = cap;
        }
        if indices.len() > p.index_capacity {
            let cap = next_capacity(indices.len());
            if let Some(mut old) = p.index.take() {
                old.destroy(&gpu.device);
            }
            p.index = Some(new_index_buffer(gpu, cap)?);
            p.index_capacity = cap;
        }

        if !vertices.is_empty() {
            let v = p.vertex.as_mut().expect("上方已确保存在");
            v.write(&gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        if !indices.is_empty() {
            let i = p.index.as_mut().expect("上方已确保存在");
            i.write(&gpu.device, 0, bytemuck::cast_slice(indices))?;
        }
        Ok(())
    }

    fn vertex_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .vertex
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn index_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .index
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn destroy(&mut self, device: &ash::Device) {
        for p in &mut self.buffers {
            p.destroy(device);
        }
        self.buffers.clear();
    }
}

impl SlotPair {
    fn destroy(&mut self, device: &ash::Device) {
        if let Some(mut v) = self.vertex.take() {
            v.destroy(device);
        }
        if let Some(mut i) = self.index.take() {
            i.destroy(device);
        }
        self.vertex_capacity = 0;
        self.index_capacity = 0;
    }
}

/// 顶点缓冲：host-visible + coherent，因此每帧直接 `write`（内部
/// map→拷贝→unmap）即可，不需要 staging + `cmd_copy_buffer`。
fn new_vertex_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

fn new_index_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<u32>()) as vk::DeviceSize,
        vk::BufferUsageFlags::INDEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ClippedPrimitive, Mesh, Primitive, Vertex as EVertex};

    fn rect_at(y: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::new(0.0, y), egui::vec2(10.0, 10.0))
    }

    fn prim(clip: egui::Rect, verts: usize, indices: usize) -> ClippedPrimitive {
        let mesh = Mesh {
            indices: (0..indices as u32).collect(),
            vertices: (0..verts)
                .map(|i| EVertex {
                    pos: egui::pos2(i as f32, 0.0),
                    uv: egui::pos2(0.0, 0.0),
                    color: egui::Color32::WHITE,
                })
                .collect(),
            texture_id: FONT_TEXTURE_ID,
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn color_is_packed_as_abgr_little_endian() {
        // 不透明色：反预乘是恒等，直接验证字节序。
        let packed = pack_color(egui::Color32::from_rgb(0x11, 0x22, 0x33));
        assert_eq!(packed & 0xFF, 0x11, "最低字节应是红");
        assert_eq!((packed >> 8) & 0xFF, 0x22);
        assert_eq!((packed >> 16) & 0xFF, 0x33);
        assert_eq!((packed >> 24) & 0xFF, 0xFF, "最高字节应是 alpha");
    }

    #[test]
    fn semi_transparent_colors_are_uploaded_premultiplied() {
        // 回归：早前这里对预乘顶点色做了「反预乘」，与管线的预乘混合
        // （ONE / ONE_MINUS_SRC_ALPHA）双重抵消，半透明选中色以全强度
        // 叠加、超界通道被裁剪，浅色主题下选中行发青偏绿。
        // 正确行为：预乘色原样上传，不做任何换算。
        let c = egui::Color32::from_rgba_unmultiplied(0x1D, 0x65, 0xAF, 52);
        let packed = pack_color(c);
        // egui 存储即预乘：r = round(29×52/255) = 6
        assert_eq!(packed & 0xFF, 6, "最低字节应是预乘后的红");
        assert_eq!((packed >> 8) & 0xFF, 21, "预乘后的绿");
        assert_eq!((packed >> 16) & 0xFF, 36, "预乘后的蓝");
        assert_eq!((packed >> 24) & 0xFF, 52, "alpha 原样");
    }

    #[test]
    fn indices_are_offset_per_mesh() {
        // 两个 mesh 的索引都是 0..n，合并后第二个必须整体平移。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(0.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(v.len(), 6, "两个 mesh 的顶点都应保留");
        assert_eq!(i, vec![0, 1, 2, 3, 4, 5], "第二个 mesh 的索引应平移到 3..6");
    }

    #[test]
    fn adjacent_primitives_with_same_clip_merge() {
        // 一次 draw call 优于 N 次：这是「相邻且 clip 相同可合并」的价值。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 1, "相同 clip 的相邻图元应合并为一批");
        assert_eq!(b[0].index_count, 9);
    }

    #[test]
    fn different_clips_stay_separate() {
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(20.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 2, "clip 不同的图元不应合并");
    }

    #[test]
    fn same_clip_but_non_adjacent_does_not_merge() {
        // 合并条件写错（忽略相邻性）会让批次区间跨越中间那批。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(20.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 3, "非相邻的同 clip 图元不应合并");
    }

    #[test]
    fn batches_cover_all_indices_exactly_once() {
        // 批次的索引区间必须无缝铺满整个索引数组：既不重叠也不留空洞。
        // 空洞会导致部分三角形没被画（表现为界面局部空白）。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(20.0), 6, 6),
            prim(rect_at(40.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        let mut next = 0u32;
        for batch in &b {
            assert_eq!(batch.index_offset, next, "批次之间出现了空洞或重叠");
            next = batch.index_offset + batch.index_count;
        }
        assert_eq!(next as usize, i.len(), "批次应覆盖全部索引");
    }

    #[test]
    fn empty_primitives_produce_no_batches() {
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty() && i.is_empty());
    }

    #[test]
    fn empty_mesh_is_skipped() {
        // 有顶点但无索引的 mesh 不能产生批次（index_count=0 会被跳过，
        // 但会留下一个空批次，徒增记录数）。
        let mut p = prim(rect_at(0.0), 3, 0);
        if let Primitive::Mesh(m) = &mut p.primitive {
            m.indices.clear();
        }
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty());
    }

    #[test]
    fn callback_primitive_is_ignored() {
        // PaintCallback 无法在不执行用户代码的情况下光栅化，必须跳过而非 panic。
        let p = ClippedPrimitive {
            clip_rect: rect_at(0.0),
            primitive: Primitive::Callback(egui::epaint::PaintCallback {
                rect: rect_at(0.0),
                // 回调签名是 `Fn(&ClippedPrimitive)`，此处永不会被调用。
                callback: std::sync::Arc::new(|_: &ClippedPrimitive| {}),
            }),
        };
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty());
    }

    /// 造一个 `PaintCallback` 图元，充当模糊分割标记。
    fn callback_prim() -> ClippedPrimitive {
        ClippedPrimitive {
            clip_rect: rect_at(0.0),
            primitive: Primitive::Callback(egui::epaint::PaintCallback {
                rect: rect_at(0.0),
                callback: std::sync::Arc::new(|_: &ClippedPrimitive| {}),
            }),
        }
    }

    #[test]
    fn callback_marks_blur_split_at_batch_boundary() {
        // 标记前两个网格、标记后一个 ⇒ 分割点应为 2（标记本身不成批）。
        let prims = vec![
            prim_with_clip(rect_at(0.0)),
            prim_with_clip(rect_at(20.0)),
            callback_prim(),
            prim_with_clip(rect_at(40.0)),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(split, 2, "分割点必须落在标记之前已生成的批次数上");
        assert_eq!(b.len(), 3, "标记本身不产生批次");
        // 分割点必须严格落在批次区间内，否则调用方会切出空区间。
        assert!(split < b.len());
    }

    #[test]
    fn no_callback_means_split_is_all_batches() {
        // 没有标记 ⇒ 分割点 = 批次数，调用方据此走普通绘制路径
        // （不能把「全部内容」误判成「全部要模糊」）。
        let prims = vec![prim_with_clip(rect_at(0.0)), prim_with_clip(rect_at(20.0))];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, b.len(), "无标记时分割点应等于批次数");
    }

    #[test]
    fn first_callback_wins_when_multiple_present() {
        // 一帧内最多一段模糊背景：出现多个标记时只认第一个，
        // 否则后面的标记会把分割点推到覆盖层内部。
        let prims = vec![
            prim_with_clip(rect_at(0.0)),
            callback_prim(),
            prim_with_clip(rect_at(20.0)),
            callback_prim(),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, 1, "只认第一个标记");
    }

    #[test]
    fn callback_at_start_yields_zero_split() {
        // 标记在第一个图元之前 ⇒ 分割点为 0。调用方据此判定
        // 「标记前没有内容」，退回普通路径（模糊没有可糊的东西）。
        let prims = vec![callback_prim(), prim_with_clip(rect_at(0.0))];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, 0);
    }

    #[test]
    fn real_egui_output_converts_cleanly() {
        // 用真实的 egui 跑一帧，覆盖「几十个图元 + 真实 Color32 + 真实
        // Mesh.indices」这一组合，而不是只测人造数据。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("标题");
            ui.label("正文 label with ascii");
            let _ = ui.button("按钮");
            // 浮层：验证不同 clip_rect 的图元也能走通（不会误合并批次）。
            egui::Window::new("窗口").show(&ctx, |ui| {
                ui.label("窗口内文字");
            });
        });
        // egui 的 `TexturesDelta` 在 drop 时断言增量为空。
        // 本测试不消费纹理增量（那是 `Painter` 的职责，需要 GPU），
        // 必须显式清空，否则 panic 会掩盖真正的断言失败。
        out.textures_delta.clear();

        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty(), "egui 应产出图元");

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert!(!v.is_empty() && !i.is_empty() && !b.is_empty());
        // 索引必须全部落在顶点数组范围内，否则 draw_indexed 越界读取。
        let n = v.len() as u32;
        assert!(i.iter().all(|&x| x < n), "存在越界索引");
        // 索引数必须是 3 的倍数（三角形列表）。
        assert_eq!(i.len() % 3, 0);
    }

    #[test]
    fn merging_never_increases_draw_call_count() {
        // 合并是纯优化：draw call 数必须**不增加**。
        // 若有人把合并条件写反（例如每个图元都新开一批），这个断言会失败。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            for i in 0..10 {
                ui.label(format!("第 {i} 行文字"));
            }
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(
            b.len() <= prims.len(),
            "合并后批次 {} 不应超过图元数 {}",
            b.len(),
            prims.len()
        );
    }

    #[test]
    fn font_texture_id_is_managed_zero() {
        // epaint 约定：Managed(0) 恒为字体图集。改错会让所有文字变豆腐块。
        assert_eq!(FONT_TEXTURE_ID, egui::TextureId::Managed(0));
    }

    #[test]
    fn texture_slot_maps_font_to_zero_and_others_to_one() {
        // 着色器只需二分：0 = 字体图集，1 = 用户纹理。
        assert_eq!(texture_slot(FONT_TEXTURE_ID), TEX_SLOT_FONT);
        assert_eq!(texture_slot(egui::TextureId::Managed(1)), TEX_SLOT_USER);
        assert_eq!(texture_slot(egui::TextureId::Managed(7)), TEX_SLOT_USER);
        // epaint 0.36 的 `TextureId` 只有 `Managed` / `User` 两个变体，
        // 没有 `Viewport`（那是更早版本的形状）。两者都必须落到用户槽。
        assert_eq!(texture_slot(egui::TextureId::User(0)), TEX_SLOT_USER);
    }

    #[test]
    fn vertices_carry_tex_id_matching_their_mesh() {
        // 端到端验证 `tessellate_into` 真的把纹理写进了顶点。
        // 单看 `texture_slot` 的单测不够——映射对了但没接上，
        // 或者接反了，这里才抓得住。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(v.len(), 6, "两个三角形共 6 个顶点");
        // 前 3 个来自字体图集，后 3 个来自缩略图。
        for vert in &v[..3] {
            assert_eq!(vert.tex_id, TEX_SLOT_FONT, "字体图元应带槽位 0");
        }
        for vert in &v[3..] {
            assert_eq!(vert.tex_id, TEX_SLOT_USER, "缩略图元应带槽位 1");
        }
    }

    #[test]
    fn real_egui_text_stays_on_the_font_slot() {
        // 用真实 egui 跑一帧：真实文字的 mesh 一定带字体图集 ID，
        // 顶点必须全是槽位 0。
        //
        // 回归意义：若 `texture_slot` 反了（字体判成 1），
        // 字体图集会被当彩色图采样 ⇒ **所有文字消失**且不报错。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("标题");
            ui.label("正文");
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(!v.is_empty(), "egui 应产出顶点");
        assert!(
            v.iter().all(|x| x.tex_id == TEX_SLOT_FONT),
            "纯文字界面的所有顶点都应走字体图集"
        );
    }

    #[test]
    fn uploaded_image_uses_the_user_slot() {
        // 真实走一次 `load_texture` + `Image`，确认 egui 派发的图元
        // 带的是**用户**纹理 ID，且被映射成槽位 1。
        // 这覆盖了「egui 怎么给用户纹理分配 TextureId」这个我无法预设的细节。
        let ctx = egui::Context::default();
        // ColorImage::new 需要像素数组，不是单个颜色
        let img = egui::ColorImage::new([2, 2], vec![egui::Color32::WHITE; 4]);
        let handle = ctx.load_texture(
            "unit-test-thumb",
            img,
            egui::TextureOptions::LINEAR,
        );
        let tex_id = handle.id();

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            // egui 0.36 只为 (TextureId, Vec2) 实现了 From，
            // 不为 (TextureHandle, Vec2) 实现
            ui.image((handle.id(), egui::vec2(64.0, 64.0)));
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_ne!(tex_id, FONT_TEXTURE_ID, "用户纹理不应与字体图集同 ID");
        assert!(
            v.iter().any(|x| x.tex_id == TEX_SLOT_USER),
            "图片图元应带槽位 1（tex_id = {tex_id:?}）"
        );
    }

    // ---- 多纹理：合批必须按 texture_id 分组 ----

    /// 造一个指定纹理的图元。`clip` 固定，便于构造
    /// 「clip 相同 + 索引连续」这个最容易触发误合并的组合。
    fn prim_with_tex(clip: egui::Rect, verts: usize, indices: usize, tex: egui::TextureId) -> ClippedPrimitive {
        let mesh = Mesh {
            indices: (0..indices as u32).collect(),
            vertices: (0..verts)
                .map(|i| EVertex {
                    pos: egui::pos2(i as f32, 0.0),
                    uv: egui::pos2(0.0, 0.0),
                    color: egui::Color32::WHITE,
                })
                .collect(),
            texture_id: tex,
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    /// 缩略图用的非字体纹理 ID。
    const THUMB_TEX: egui::TextureId = egui::TextureId::Managed(7);

    #[test]
    fn different_textures_are_never_merged() {
        // 回归测试：合批曾只看 clip + 索引连续，漏掉纹理判定。
        // 引入用户纹理后，字体图元与缩略图图元会被并进同一批，
        // 而该批只绑一张纹理 ⇒ 缩略图位置显示成字体图集里的字形。
        // 该错误不产生任何 GPU 错误、验证层也干净。
        let clip = rect_at(0.0);
        let prims = vec![
            // clip 相同、索引连续（同clip_rect + 追加式写入），
            // 只有纹理不同——这正是必须拆开的情形。
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 2, "不同纹理绝不能合批：{b:?}");
        assert_eq!(b[0].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[1].tex_id, THUMB_TEX);
    }

    #[test]
    fn same_texture_still_merges() {
        // 反向约束：修「漏判纹理」不能顺手把合批优化也干掉。
        // 同纹理 + 同 clip + 索引连续 ⇒ 必须仍然合并成一批。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 1, "同纹理应继续合批：{b:?}");
        assert_eq!(b[0].index_count, 6, "合并后索引数应累加");
    }

    #[test]
    fn same_texture_across_interleaved_primitives_groups_correctly() {
        // 字体 / 缩略图交替出现时应得到 4 批，而不是 1 批也不是 2 批。
        // 覆盖「相邻性」判定的正确性：只有**相邻且同纹理**才能合并。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 4, "交替纹理应逐个成批：{b:?}");
        assert_eq!(b[0].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[1].tex_id, THUMB_TEX);
        assert_eq!(b[2].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[3].tex_id, THUMB_TEX);
    }

    #[test]
    fn batch_tex_id_defaults_survive_to_draw_conversion() {
        // `to_draw` 本身不查纹理表（拿不到），默认 `user_set` 为 null
        // （文字/形状批就该用帧槽集合）；缩略图批的集合由 paint()
        // 在转换后按 `tex_id` 填。转换本身不能因此丢批次或崩。
        let b = Batch {
            index_offset: 4,
            index_count: 6,
            clip: rect_at(0.0),
            tex_id: THUMB_TEX,
        };
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let d = b.to_draw(extent, 1.0);
        assert_eq!(d.index_offset, 4);
        assert_eq!(d.index_count, 6);
        assert_eq!(
            d.user_set,
            vk::DescriptorSet::null(),
            "默认不得重绑用户纹理"
        );
    }

    // ---- 纹理增量的「整图 / 局部」判据 ---------------------------------

    #[test]
    fn full_image_update_is_pos_none() {
        // 回归测试：判据反过会让整图更新被当成局部更新跳过，
        // 缩略图永远上传不上去——表现为「缩略图区域一片空白」
        // （用户实机反馈：渲染空白画布），且不报任何 GPU 错误。
        assert!(is_full_image_update(None), "pos=None 才是整图更新");
        assert!(!is_full_image_update(Some([0, 0])), "pos=Some 是局部更新");
        assert!(!is_full_image_update(Some([16, 32])));
    }

    // ---- 裁剪矩形换算 --------------------------------------------------
    //
    // 这组测试守的是「UI 裁剪真的生效」这条链路的换算端。
    // 缺陷历史：clip_rect 曾被完全忽略（只用于合批判定），
    // 导致所有 painter_at 裁剪形同虚设、文字溢出面板。

    fn clip_of(r: egui::Rect) -> egui::Rect {
        r
    }

    #[test]
    fn clip_scales_by_ppp_into_physical_pixels() {
        // 逻辑点 (10,20)-(110,70)，ppp=1.5 ⇒ 物理 (15,30)-(165,105)
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(10.0, 20.0),
                egui::pos2(110.0, 70.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 900,
                height: 800,
            },
            1.5,
        );
        let c = d.clip.expect("必须带裁剪矩形，否则裁剪等于没做");
        assert_eq!((c.offset.x, c.offset.y), (15, 30));
        assert_eq!((c.extent.width, c.extent.height), (150, 75));
    }

    #[test]
    fn clip_negative_origin_is_clamped_to_zero() {
        // 浮层被拖到窗口左侧：x 为负。
        // Vulkan 的 scissor 要求 x + width > x，负 offset 属未定义行为，
        // 必须钳到 0 而不是原样传下去。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(-50.0, -20.0),
                egui::pos2(30.0, 40.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须带裁剪矩形");
        assert_eq!(c.offset.x, 0, "负 x 必须钳到 0");
        assert_eq!(c.offset.y, 0, "负 y 必须钳到 0");
        // 原矩形 x∈[-50,30]、y∈[-20,40]；钳位后可见部分
        // 是 x∈[0,30]、y∈[0,40] ⇒ 宽 30、高 40。
        // NOTE: 期望值是「钳位后的可见区域」，不是原矩形的宽高——
        //   写成 80/60（原点 + 原宽高）会算出一个延伸到 x=80 的
        //   矩形，把本该被裁掉的部分又画回来。
        assert_eq!(c.extent.width, 30, "宽度应是钳位后与视口相交的部分");
        assert_eq!(c.extent.height, 40);
    }

    #[test]
    fn clip_fully_outside_viewport_becomes_zero_area_not_fullscreen() {
        // NOTE: 这一条是「裁剪失效」与「裁剪生效」的分界：
        // 整块被裁掉时若退回 `None`（= 全屏），文字就会重新画到窗口上。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(5000.0, 5000.0),
                egui::pos2(6000.0, 6000.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须是 Some(零面积)，不能是 None");
        assert_eq!((c.offset.x, c.offset.y), (800, 600));
        assert_eq!((c.extent.width, c.extent.height), (0, 0));
    }

    #[test]
    fn clip_is_intersected_with_viewport() {
        // 右侧超出视口：显式求交，避免依赖 Vulkan 的隐式裁剪。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(700.0, 100.0),
                egui::pos2(2000.0, 200.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须带裁剪矩形");
        assert_eq!(c.extent.width, 100, "右缘应被视口右边界截断");
        assert_eq!(c.extent.height, 100);
    }

    #[test]
    fn clip_survives_degenerate_ppp() {
        // ppp 传0 / NaN / 负数时不能变成 0 宽或 panic，
        // 否则整个界面会在一次异常 ppp 后变成空白。
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let b = Batch {
                index_offset: 0,
                index_count: 3,
                clip: clip_of(egui::Rect::from_min_max(
                    egui::pos2(0.0, 0.0),
                    egui::pos2(100.0, 50.0),
                )),
                tex_id: FONT_TEXTURE_ID,
            };
            let d = b.to_draw(
                vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                bad,
            );
            let c = d.clip.expect("退化 ppp 也必须带裁剪矩形");
            assert!(
                c.extent.width > 0 && c.extent.height > 0,
                "ppp={bad} 时裁剪矩形退化为零面积，界面会变空白"
            );
        }
    }

    #[test]
    fn every_batch_carries_a_clip_rect() {
        // 合批后每个批次都必须有 clip：漏一个就等于那一段不裁剪。
        let prims = vec![
            prim_with_clip(egui::Rect::from_min_max(
                egui::pos2(0.0, 0.0),
                egui::pos2(50.0, 50.0),
            )),
            prim_with_clip(egui::Rect::from_min_max(
                egui::pos2(60.0, 0.0),
                egui::pos2(120.0, 50.0),
            )),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(b.len() >= 2, "不同 clip 不应合批");
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        for batch in &b {
            assert!(
                batch.to_draw(extent, 1.0).clip.is_some(),
                "每个批次都必须带 clip，否则该段不裁剪"
            );
        }
    }

    /// 造一个带指定裁剪矩形的最小图元（一个 2x2 的矩形网格）。
    fn prim_with_clip(clip: egui::Rect) -> egui::epaint::ClippedPrimitive {
        use egui::epaint::{Color32, Mesh, Primitive, Vertex};
        let mut mesh = Mesh::default();
        for (x, y) in [(0.0f32, 0.0f32), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)] {
            mesh.vertices.push(Vertex {
                pos: egui::pos2(x, y),
                uv: egui::pos2(0.0, 0.0),
                color: Color32::WHITE,
            });
        }
        mesh.indices.extend_from_slice(&[0, 1, 2, 1, 3, 2]);
        egui::epaint::ClippedPrimitive {
            primitive: Primitive::Mesh(mesh),
            clip_rect: clip,
        }
    }

    #[test]
    fn vertex_layout_matches_pipeline_stride() {
        // pipeline.rs 声明的步长是 pos(8)+uv(8)+color(4)+tex_id(4)=24 字节。
        // 布局不符时驱动不会报错，只会画出乱码。
        //
        // 这条断言在本项目改过两次：加 `tex_id` 前是 20 字节。
        // 两次都是「Rust 侧改了、忘了同步管线」的类型——
        // 由本测试当场抓住，而不是等到实机看见乱码。
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
    }

    #[test]
    fn capacity_growth_is_monotonic() {
        assert!(next_capacity(1) >= 1);
        assert!(next_capacity(2000) >= 2000);
        assert_eq!(next_capacity(2000) % 1024, 0);
        // 连续扩容不应原地踏步，否则每帧都重建。
        assert!(next_capacity(next_capacity(2000)) > next_capacity(2000));
    }

    #[test]
    fn initial_capacities_cover_a_typical_frame() {
        // 初始容量太小会让首帧就触发一次缓冲重建（多一次 GPU 分配）。
        // 2000 顶点的界面在剪贴板场景里算大，8192 起步合理。
        const {
            assert!(INITIAL_VERTEX_CAPACITY >= 2048);
            assert!(INITIAL_INDEX_CAPACITY >= INITIAL_VERTEX_CAPACITY);
        }
    }

    #[test]
    fn uniform_matrix_is_identity() {
        // 着色器已自己做像素→NDC；再乘非单位矩阵会二次变换。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(
            u.clip_from_uv,
            [
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                0.0, 0.0, 0.0, 1.0,
            ]
        );
        assert_eq!(u.size_in_pixels, [800.0, 600.0]);
    }

    /// **本缺陷的判据（渲染侧）**：顶点是逻辑点、视口是物理像素，
    /// 而两者之间唯一的换算依据是 `dpr`。
    ///
    /// # 判据为什么必须是这个不变式
    ///
    /// 本项目吃过三次判据失效的亏（数字对了但用户看到重影、截图没开 DPI
    /// 感知、数字判据与视觉判据不同源），所以这里明确选定**唯一**判据：
    ///
    /// ```text
    /// screen_rect.size() * dpr == size_in_pixels
    /// ```
    ///
    /// 它失败时画面整体缩放错（内容只占视口一部分），且**不产生任何
    /// GPU 错误、验证层也干净**——靠「有没有报错」或「三段宽度之和 ≤
    /// 可用宽度」都抓不到。
    ///
    /// 用真实的 `egui::Context` + `set_pixels_per_point`（与
    /// `EventLoop::egui_input` 里同源）产出真实 `ppp`，而不是手写常量，
    /// 这样本测试与`window.rs` 的守卫真的在同一条链路上。
    #[test]
    fn uniform_dpr_times_logical_canvas_equals_physical_viewport() {
        // 实机遇到的组合：420x560 物理像素 @ DPI 144（ppp 应为 1.5）。
        const PHYSICAL: vk::Extent2D = vk::Extent2D {
            width: 420,
            height: 560,
        };

        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let logical = egui::vec2(
                PHYSICAL.width as f32 / scale,
                PHYSICAL.height as f32 / scale,
            );
            let ctx = egui::Context::default();
            // 与 window.rs 里的调用完全一致。
            ctx.set_pixels_per_point(scale);
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical)),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |_ui| {});
            out.textures_delta.clear();

            // 真实 ppp 来自 egui 自身，而非我们手写。
            let u = uniforms_for(PHYSICAL, out.pixels_per_point);
            assert_eq!(
                logical * u.dpr,
                egui::vec2(PHYSICAL.width as f32, PHYSICAL.height as f32),
                "scale={scale}: screen_rect{logical:?} * dpr({}) 必须等于物理视口 {PHYSICAL:?}",
                u.dpr
            );
            // `size_in_pixels` 必须始终是**物理像素**，不能被顺手改成逻辑点
            // （否则就与顶点侧的 dpr 换算重复乘一次）。
            assert_eq!(
                u.size_in_pixels,
                [PHYSICAL.width as f32, PHYSICAL.height as f32],
                "size_in_pixels 必须是物理像素"
            );
        }
    }

    /// 同一不变式的端到端版：真跑一帧 egui，铺满逻辑画布的矩形经着色器
    /// 公式换算后必须正好铺满物理视口（`ndc` 落在 ±1）。
    ///
    /// 上一条验的是「输入之间自洽」，这条验的是「换算结果真的对」——
    /// 它直接对应实机现象：改前`ndc` 只到 ±1/1.5，右侧/底部空1/3。
    #[test]
    fn full_canvas_rect_maps_to_ndc_edges() {
        const PHYSICAL: vk::Extent2D = vk::Extent2D {
            width: 420,
            height: 560,
        };
        let scale = 1.5_f32;
        let logical = egui::vec2(PHYSICAL.width as f32 / scale, PHYSICAL.height as f32 / scale);

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(scale);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ctx| {
            // 铺满整个逻辑画布的矩形——实机 UI 的背景就是这么画的。
            let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, logical);
            egui::Painter::new(ctx.clone(), egui::LayerId::background(), canvas)
                .rect_filled(canvas, 0.0, egui::Color32::WHITE);
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let u = uniforms_for(PHYSICAL, out.pixels_per_point);

        // 顶点最大坐标（逻辑点）应覆盖整个逻辑画布。
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for p in &prims {
            if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
                for v in &m.vertices {
                    max_x = max_x.max(v.pos.x);
                    max_y = max_y.max(v.pos.y);
                }
            }
        }
        assert!(max_x >= logical.x - 1.0, "背景矩形应铺满画布宽，实际 {max_x}");

        // 复刻着色器 `vs_main` 的换算（egui.wgsl:53-55）。
        let ndc_x = 2.0 * (max_x * u.dpr) / u.size_in_pixels[0] - 1.0;
        let ndc_y = 1.0 - 2.0 * (max_y * u.dpr) / u.size_in_pixels[1];
        assert!(
            (ndc_x - 1.0).abs() < 0.01,
            "右边缘 ndc 应为 1.0（铺满视口），实际 {ndc_x}——\
             小于 1 即右侧留白"
        );
        assert!(
            (ndc_y + 1.0).abs() < 0.01,
            "下边缘 ndc 应为 -1.0（铺满视口），实际 {ndc_y}——\
             大于 -1 即底部留白"
        );
    }

    #[test]
    fn uniform_key_distinguishes_all_inputs() {
        // uniform 只在内容变化时重写：指纹必须能区分三种输入。
        let a = UniformKey {
            width: 800,
            height: 600,
            pixels_per_point: 1.0,
        };
        assert_ne!(
            a,
            UniformKey {
                width: 801,
                height: 600,
                pixels_per_point: 1.0
            }
        );
        assert_ne!(
            a,
            UniformKey {
                width: 800,
                height: 601,
                pixels_per_point: 1.0
            }
        );
        assert_ne!(
            a,
            UniformKey {
                width: 800,
                height: 600,
                pixels_per_point: 1.5
            }
        );
    }

    #[test]
    fn repaint_delay_maps_egui_duration_to_idle() {
        // 语义基准来自 `egui::Context`：`ViewportRepaintInfo::default()`
        // 把 `repaint_delay` 设为 `Duration::MAX`，注释是
        // "We haven't scheduled a repaint yet."；
        // `request_repaint()` 把它置为 `Duration::ZERO`。
        //
        // 因此：**MAX = 不重绘（该睡）**，**ZERO = 立即重绘（不该睡）**。
        let mut out = egui::FullOutput::default();
        let vp = || egui::ViewportId::ROOT;
        out.viewport_output.insert(
            vp(),
            egui::ViewportOutput {
                parent: vp(),
                class: egui::ViewportClass::Root,
                builder: egui::ViewportBuilder::default(),
                viewport_ui_cb: None,
                commands: Vec::new(),
                repaint_delay: std::time::Duration::MAX,
            },
        );
        assert_eq!(
            repaint_delay(&out),
            None,
            "Duration::MAX 表示 egui 不需要重绘 ⇒ 该交给事件驱动（None）"
        );

        // ZERO 必须是 Some(ZERO)：它表示「立刻再跑一帧」。
        // 早前一版把它映射成 None，会让这次立即重绘请求丢失。
        out.viewport_output
            .get_mut(&vp())
            .expect("刚插入")
            .repaint_delay = std::time::Duration::ZERO;
        assert_eq!(
            repaint_delay(&out),
            Some(std::time::Duration::ZERO),
            "Duration::ZERO 是「立即重绘」，必须原样传出而不是当成 None"
        );

        out.viewport_output
            .get_mut(&vp())
            .expect("刚插入")
            .repaint_delay = std::time::Duration::from_millis(250);
        assert_eq!(
            repaint_delay(&out),
            Some(std::time::Duration::from_millis(250))
        );
    }
}
