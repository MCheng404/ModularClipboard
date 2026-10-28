//! 液态玻璃合成管线（`fs_glass` 的宿主）。
//!
//! # 它为什么能复用 [`crate::blur::Blit`]
//!
//! 玻璃合成与「模糊结果整屏拷贝」的契约完全一致：一次全屏三角形绘制、
//! 写满整个目标、一个 `vec2<f32>` 的 push constant（UV 位移）。区别只在
//! 管线（`fs_glass` vs `fs_copy`）与描述符集（两张源纹理 + 带数组的
//! uniform）。因此 [`GlassPipeline::blit`] 直接产出 `Blit`，渲染层的
//! `prelude` 机制一行都不用改。
//!
//! # 每帧槽位一套资源
//!
//! uniform 里是**卡片矩形**——滚动时每帧都变。多帧在途时改写同一份
//! uniform 会让上一帧的 GPU 读到本帧的矩形（玻璃错位一帧）。所以按帧
//! 槽位各给一份缓冲与描述符集；槽位的 GPU 工作在 `acquire` 时已被
//! fence 等过，此时重写是安全的（这也是本项目一贯的槽位纪律）。
//!
//! 同理不需要 `UPDATE_AFTER_BIND`：写的永远是**当前**槽位，而它的上一
//! 轮早已完成。纹理视图因尺寸变化被销毁重建时，调用方会先
//! `wait_idle`（见 `ui::renderer::painter::ensure_backdrop`）。

use ash::vk;

use crate::blur::Blit;
use crate::buffer::Buffer;
use crate::pipeline::{
    BINDING_SAMPLER, BINDING_TEXTURE, BINDING_TEXTURE_BLUR, BINDING_UNIFORM, DescriptorLayout,
    GraphicsPipeline, PipelineLayout,
};
use crate::shader::{GLASS_FRAGMENT_ENTRY, ShaderModule};
use crate::texture::Sampler;
use crate::Gpu;

/// 同时存在的玻璃面板上限。必须与 `glass.wgsl` 的 `MAX_GLASS_CARDS` 一致。
///
/// 两侧漂移的后果：CPU 写进 uniform 的矩形数超过着色器声明的数组长度，
/// 多出的那些被**静默忽略**（WGSL 越界访问在 uniform 里是实现定义的），
/// 表现为「有的卡片没有玻璃」且不报任何错。
///
/// # 为什么从 16 提到 24（2026-10-27）
///
/// 16 只够「约 10 张可见行 + 顶栏 + 搜索面板 + 类型栏」。窗口一拉高
/// （或字号调小让行变多）可见行就到 18~20，末尾几张卡直接没有玻璃——
/// 用户看到的是「有的卡片是玻璃、有的是磨砂」，而不是报错。
///
/// 24 覆盖 800~1200px 高的窗口；每像素多 8 次 AABB 比较（4 个分量），
/// uniform 从 304 涨到 432 字节，代价可忽略。再往上就要改成按行索引的
/// 空间结构（着色器里逐矩形遍历是 O(n)），不值当。
pub const MAX_GLASS_CARDS: usize = 24;

/// push constant 的阶段掩码（只有片元读 UV 位移）。
const OFFSET_STAGE: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// push constant 字节数：一个 `vec2<f32>`。
///
/// 必须与 `blur.wgsl` / `glass.wgsl` 的 `CopyOffset` 逐字节一致——
/// 玻璃复用 `Blit`，而 `Blit` 被 `fs_copy` 与 `fs_glass` 共用。
const OFFSET_SIZE: u32 = 8;

/// 玻璃 uniform。布局必须与 `glass.wgsl` 的 `GlassUniforms` 逐字段一致。
///
/// 大小 1184 字节：`rects` 384 + `tints` 384 + `alphas` 384 + `size` 8 +
/// 5 个 f32 20 + `_pad` 4。
/// **无隐式填充**（每个字段都命名），这是 `bytemuck::Pod` 的要求。
///
/// # 为什么 tint / alpha 是**逐矩形**的（2026-10-28）
///
/// 早前全局只有一份 `tint` / `card_alpha`，由绘制层「最后一次设置」决定。
/// 而参与玻璃的面板底色并不相同：行卡片是 `card_bg`（受「卡片不透明度」
/// 滑块影响）、顶栏是 `bg`、搜索面板与类型栏是 `surface`。绘制顺序是
/// 「顶栏 → 行卡片 → 类型栏 → 搜索面板」⇒ 最终生效的是**最后画的
/// chrome**，行卡片被染成面板色（或反过来），且没有任何报错。
///
/// 改成逐矩形后每块区域用自己的底色，语义也统一了：
/// **覆盖度 = 该区域底色的 alpha**。
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GlassUniforms {
    /// 卡片矩形：`xy = 中心`、`zw = 半尺寸`，单位是**目标像素**。
    pub rects: [[f32; 4]; MAX_GLASS_CARDS],
    /// 逐矩形的底色：`rgb` = 未预乘原色（0~1），`a` = 混合强度。
    ///
    /// CAUTION: `rgb` 必须是**未预乘**的原色。早前这层色由 egui 在玻璃
    /// pass 之后再画一个矩形，两层的 alpha 叠加成 `a + a(1-a)`：底色
    /// 不透明时玻璃整层被盖死（折射/色散全看不见）。
    pub tints: [[f32; 4]; MAX_GLASS_CARDS],
    /// 逐矩形的**覆盖度**（0 = 完全露出真实桌面，1 = 不透明），取 `.x`。
    ///
    /// 着色器按预乘使用（`rgb * a, a`）。与 `tints[i].a`（有多像一张
    /// 有色卡片）是两件事：覆盖度 1 + tint 0 = 不透明但完全通透的玻璃。
    ///
    /// CAUTION: 必须用 `vec4` 而不是 `f32`。WGSL uniform 空间要求数组
    /// 元素的**步长是 16 的倍数**，`array<f32, N>` 的步长是 4，naga 直接
    /// 报 `ArrayStride { stride: 4, alignment: Alignment(16) }` 校验失败。
    /// 只有 `x` 有意义，其余三个分量是填充。
    pub alphas: [[f32; 4]; MAX_GLASS_CARDS],
    /// 目标图的像素尺寸 `(w, h)`（uv → 像素用）。
    pub size: [f32; 2],
    /// 折射强度（0 = 只剩磨砂）。
    pub refract: f32,
    /// 色散差异量（0 = 无彩边，1 = 完全按各通道 IOR 分开）。
    pub dispersion: f32,
    /// Fresnel 幂次。
    pub edge: f32,
    /// 卡片圆角半径（像素）。
    pub radius: f32,
    /// 有效卡片数。
    pub count: u32,
    /// 尾部填充：把结构体补到 16 的倍数（WGSL uniform 的规则）。
    pub _pad: f32,
}

impl Default for GlassUniforms {
    fn default() -> Self {
        Self {
            rects: [[0.0; 4]; MAX_GLASS_CARDS],
            tints: [[0.0; 4]; MAX_GLASS_CARDS],
            // 默认覆盖度 1：万一某条路径忘了给 alpha，玻璃是**不透明**
            // 而不是「整片透出桌面」——后者会让界面看起来像坏了。
            alphas: [[1.0, 0.0, 0.0, 0.0]; MAX_GLASS_CARDS],
            size: [0.0; 2],
            refract: 0.0,
            dispersion: 0.0,
            edge: 5.0,
            radius: 8.0,
            count: 0,
            _pad: 0.0,
        }
    }
}

/// 玻璃合成管线。
pub struct GlassPipeline {
    desc_layout: DescriptorLayout,
    pipe_layout: PipelineLayout,
    pipeline: vk::Pipeline,
    shader: ShaderModule,
    sampler: Sampler,
    /// 每帧槽位一份 uniform 与描述符集（见模块文档）。
    uniforms: Vec<Buffer>,
    sets: Vec<vk::DescriptorSet>,
    descriptor_pool: vk::DescriptorPool,
}

impl GlassPipeline {
    /// 创建管线。
    ///
    /// `render_pass` 必须是玻璃合成目标（交换链或超采样场景图）所用的
    /// 那一个——与模糊链同格式，因此可以直接复用同一个通道。
    ///
    /// `slot_count` 是帧渲染器的在飞槽位数（`FrameRenderer::slot_count`），
    /// 决定要备几份 uniform/集合。
    pub fn new(gpu: &Gpu, render_pass: vk::RenderPass, slot_count: usize) -> anyhow::Result<Self> {
        let device = &gpu.device;
        let n = slot_count.max(1);

        // CAUTION: 这里必须是 `true`。玻璃的描述符集**每帧都被重写**
        // （uniform 随卡片矩形变、两张源纹理的视图随尺寸重建而变），
        // 而帧槽的 GPU 工作与 CPU 的下一帧并行——规范禁止更新「被
        // pending 命令缓冲使用中」的集合，除非绑定带了
        // `UPDATE_AFTER_BIND`。`blur.rs` 传 `false` 是对的（它的集合只在
        // `configure`/`bind_source` 时写一次），玻璃不能照抄。
        let desc_layout = DescriptorLayout::glass(device, true)?;
        let pc = vk::PushConstantRange {
            stage_flags: OFFSET_STAGE,
            offset: 0,
            size: OFFSET_SIZE,
        };
        let pipe_layout = PipelineLayout::with_push_constants(
            device,
            std::slice::from_ref(&desc_layout.handle),
            &[pc],
        )?;
        let shader = ShaderModule::new_glass(device)?;

        // CAUTION: `fullscreen` 的**顶点入口是硬编码的**
        // (`crate::shader::BLUR_VERTEX_ENTRY` = `vs_fullscreen`)。玻璃着色器
        // 的顶点入口必须**同名**，否则 `create_graphics_pipelines` 报
        // EntryPointNotFound。glass.wgsl 的 `vs_fullscreen` 上方注明了这条。
        let pipeline = GraphicsPipeline::fullscreen(
            device,
            render_pass,
            pipe_layout.handle,
            &shader,
            GLASS_FRAGMENT_ENTRY,
        )?
        .handle;

        // 采样器：线性 + 钳到边缘。
        //
        // 折射会把采样点推到卡片外（边缘处位移可达 0.3 UV），钳到边缘
        // 让越界部分拉伸最后一列——比 REPEAT 把对侧内容卷过来自然。
        // 模糊链的卷积用 MIRRORED_REPEAT，这里不用：折射是**有方向**的
        // 位移，镜像会让边缘出现一块翻转的倒影。
        let sampler = Sampler::new_linear_clamped(device)?;

        let u_size = std::mem::size_of::<GlassUniforms>() as vk::DeviceSize;
        let mut uniforms = Vec::with_capacity(n);
        for _ in 0..n {
            uniforms.push(Buffer::new_host_visible(
                gpu,
                u_size,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                vk::SharingMode::EXCLUSIVE,
            )?);
        }

        // 池：n 组集合，每组 1 uniform + 1 sampler + 2 组合图采样器。
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: n as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: n as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: n as u32 * 2,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(n as u32)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .map_err(|e| anyhow::anyhow!("创建玻璃描述符池失败: {e:?}"))?;

        let layouts = vec![desc_layout.handle; n];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let sets = unsafe { device.allocate_descriptor_sets(&alloc_info)? };
        anyhow::ensure!(
            sets.len() == n,
            "玻璃描述符集分配数量不符：期望 {n}，实际 {}",
            sets.len()
        );

        Ok(Self {
            desc_layout,
            pipe_layout,
            pipeline,
            shader,
            sampler,
            uniforms,
            sets,
            descriptor_pool,
        })
    }

    /// 写入某一帧槽位的 uniform 与两张源纹理。
    ///
    /// `sharp` 是清晰的桌面捕获图，`blur` 是 Dual Filtering 的模糊结果。
    /// 两者的视图都可能因尺寸变化而变，所以**每次绘制前都要绑**。
    ///
    /// # 为什么不用 `UPDATE_AFTER_BIND`
    ///
    /// 写的永远是当前槽位，而该槽位的上一轮 GPU 工作已在 `acquire` 时
    /// 被 fence 等过——不存在「被 pending 命令缓冲使用中」的情况。
    pub fn prepare(
        &mut self,
        gpu: &Gpu,
        slot: usize,
        uniforms: &GlassUniforms,
        sharp: vk::ImageView,
        blur: vk::ImageView,
    ) -> anyhow::Result<()> {
        let slot = slot.min(self.uniforms.len() - 1);
        self.uniforms[slot].write(
            &gpu.device,
            0,
            bytemuck::bytes_of(uniforms),
        )?;

        let img = |view: vk::ImageView| vk::DescriptorImageInfo {
            sampler: self.sampler.handle(),
            image_view: view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        };
        let sharp_info = img(sharp);
        let blur_info = img(blur);
        let buf_info = vk::DescriptorBufferInfo {
            buffer: self.uniforms[slot].handle(),
            offset: 0,
            range: std::mem::size_of::<GlassUniforms>() as vk::DeviceSize,
        };
        let set = self.sets[slot];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_UNIFORM)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&buf_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&sharp_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&sharp_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_TEXTURE_BLUR)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&blur_info)),
        ];
        unsafe { gpu.device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    /// 产出可直接交给渲染层 `prelude` 的 [`Blit`]。
    ///
    /// `uv_offset` 与 Acrylic 的合成共用一套语义（磨砂移动预测，见
    /// `blur.wgsl` 模块头）。
    pub fn blit(&self, slot: usize, uv_offset: [f32; 2]) -> Blit {
        Blit {
            pipeline: self.pipeline,
            layout: self.pipe_layout.handle,
            descriptor_set: self.sets[slot.min(self.sets.len() - 1)],
            uv_offset,
        }
    }

    /// 当前槽位数（测试与诊断用）。
    pub fn slot_count(&self) -> usize {
        self.uniforms.len()
    }

    /// 释放全部 GPU 资源。
    pub fn destroy(&mut self, gpu: &Gpu) {
        let device = &gpu.device;
        for u in &mut self.uniforms {
            u.destroy(device);
        }
        self.uniforms.clear();
        self.sets.clear();
        unsafe {
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_pipeline(self.pipeline, None);
            self.pipe_layout.destroy(device);
            self.desc_layout.destroy(device);
        }
        self.descriptor_pool = vk::DescriptorPool::null();
        self.pipeline = vk::Pipeline::null();
        self.shader.destroy(device);
        self.sampler.destroy(device);
    }
}

impl std::fmt::Debug for GlassPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlassPipeline")
            .field("slots", &self.uniforms.len())
            .finish_non_exhaustive()
    }
}

/// 绘制层每帧送来的玻璃参数。
///
/// 全部字段都是**逻辑点 / 无量纲**（半径除外也是点）——换算成物理像素
/// 发生在 [`Self::uniform`]，因为只有渲染层知道 `pixels_per_point`
/// （含超采样倍率）。
#[derive(Debug, Clone, Default)]
pub struct GlassParams {
    /// 参与玻璃合成的矩形（列表卡片 / 顶栏 / 搜索面板 / 类型栏）。
    pub rects: Vec<GlassRect>,
    /// 折射强度（0 = 只剩磨砂）。
    pub refract: f32,
    /// 色散差异量（0 = 无彩边）。
    pub dispersion: f32,
    /// Fresnel 幂次。
    pub edge: f32,
    // 覆盖度与底色**不在这里**：它们是逐矩形的（见 `GlassRect`）。
    //
    // 早前这里有一组全局的 `card_alpha` / `tint` / `tint_strength`，由
    // 绘制层「最后一次设置」决定。而各面板底色并不相同（行卡片
    // `card_bg`、顶栏 `bg`、搜索/类型栏 `surface`），最终生效的只是最后
    // 画的那一块 ⇒ 其余面板被串色，且不报错。
}

impl GlassParams {
    /// 是否值得跑玻璃 pass。
    ///
    /// 没有矩形 = 没有卡片 = 玻璃无事可做（直接走普通磨砂合成更省一趟
    /// 全屏绘制）。
    pub fn is_active(&self) -> bool {
        !self.rects.is_empty()
    }

    /// 换算成 uniform（逻辑点 → 目标像素）。
    pub fn uniform(&self, ppp: f32, target: vk::Extent2D) -> GlassUniforms {
        // 构造式初始化而不是 `default()` 后逐字段赋值（clippy 的
        // `field_reassign_with_default`）：少一次中间状态，也免得以后
        // 加字段时漏拷。
        let mut u = GlassUniforms {
            size: [target.width as f32, target.height as f32],
            refract: self.refract,
            dispersion: self.dispersion,
            edge: self.edge,
            ..GlassUniforms::default()
        };
        let n = self.rects.len().min(MAX_GLASS_CARDS);
        for (i, g) in self.rects.iter().take(n).enumerate() {
            let c = g.rect.center() * ppp;
            let h = g.rect.size() * 0.5 * ppp;
            u.rects[i] = [c.x, c.y, h.x, h.y];
            u.tints[i] = [g.tint[0], g.tint[1], g.tint[2], g.tint_strength];
            u.alphas[i] = [g.alpha, 0.0, 0.0, 0.0];
            // 圆角半径取**第一张**的：同一帧里各面板的圆角通常同源
            // （都走 `paint` 的圆角常量），差别在一两像素内，而折射
            // 边缘对圆角误差不敏感（差 1px 只是亮边宽一点）。
            if i == 0 {
                u.radius = g.radius * ppp;
            }
        }
        u.count = n as u32;
        u
    }
}

/// 单个参与玻璃合成的矩形（逻辑点）。
#[derive(Debug, Clone, Copy)]
pub struct GlassRect {
    /// 矩形（逻辑点，相对客户区左上角）。
    pub rect: egui::Rect,
    /// 圆角半径（逻辑点）——必须与 egui 画出来的圆角一致，否则折射
    /// 边缘会和卡片描边错开。
    pub radius: f32,
    /// 这一块的底色（sRGB 0~1，**未预乘**）。
    pub tint: [f32; 3],
    /// 底色在玻璃层里的混合强度（0 = 纯玻璃，1 = 完全是底色）。
    pub tint_strength: f32,
    /// 这一块的覆盖度（0 = 完全透出桌面，1 = 不透明）。
    pub alpha: f32,
}

impl GlassRect {
    /// 由「矩形 + 圆角 + 该区域的底色 + 玻璃强度」构造。
    ///
    /// # 为什么强度是**独立参数**而不是取 `fill.a()`
    ///
    /// 各面板底色的 alpha 语义并不统一：行卡片 `card_bg` 的 alpha 来自
    /// 「卡片不透明度」滑块（正是用户口中的「玻璃强度」），而顶栏 `bg` /
    /// 搜索面板 `surface` 的 alpha 来自 `with_backdrop_alpha`（磨砂模式下
    /// 「窗口背景有多透」，默认约 0.57）。若直接拿 `fill.a()` 当覆盖度，
    /// 顶栏玻璃只有卡片的六成强度——同一屏里两种玻璃，且没有任何报错。
    ///
    /// 所以：**`fill` 只决定颜色，`strength` 决定覆盖度**。后者由绘制层
    /// 统一取「卡片不透明度」，一个滑块管住所有玻璃。
    ///
    /// `tint_strength` 也由 `strength` 派生（而非 `fill.a()`）：卡片越不
    /// 透明，底色越浓——用户调低它就是要「更通透」，覆盖度与色浓一起降。
    pub fn new(
        rect: egui::Rect,
        radius: f32,
        fill: egui::Color32,
        strength: f32,
    ) -> Self {
        let strength = if strength.is_finite() {
            strength.clamp(0.0, 1.0)
        } else {
            1.0
        };
        // 预乘存储 ⇒ 必须用 `to_srgba_unmultiplied` 还原原色，否则
        // 「不透明度」会被算两次（色越调越暗）。
        let [r, g, b, _] = fill.to_srgba_unmultiplied();
        Self {
            rect,
            radius,
            tint: [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0],
            tint_strength: crate::glass::card_tint_strength(strength),
            alpha: strength,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, Rect, vec2};

    /// uniform 大小必须与 `glass.wgsl` 的 `GlassUniforms` 逐字节一致。
    ///
    /// # 为什么钉死数字而不是「对着着色器算」
    ///
    /// 着色器没法在 Rust 里反射布局，naga 的校验（`build.rs`）只保证
    /// **WGSL 自己**合法，不保证与 Rust 侧相等。两侧漂移的后果是
    /// `cmd_push_constants`/`write_buffer` 写进去的字节被按另一种结构
    /// 解读——所有参数都变成垃圾，但**不报错**（只表现为「玻璃看起来
    /// 完全不对」）。
    ///
    /// 1184 = rects 384 + tints 384 + alphas 384 + size 8 + 5×f32 20 + pad 4。
    /// 改字段时这个数字必须同步改，并回去改 `glass.wgsl` 的 `_pad`。
    #[test]
    fn uniform_size_is_pinned_to_wgsl_layout() {
        assert_eq!(std::mem::size_of::<GlassUniforms>(), 1184);
        // 16 的倍数：WGSL uniform 结构体的 size 必须是最大成员对齐的倍数。
        assert_eq!(std::mem::size_of::<GlassUniforms>() % 16, 0);
    }

    /// 每块面板的底色**互不串染**（2026-10-28 的核心回归）。
    ///
    /// 早前全局只有一份 `tint` / `card_alpha`，由绘制层「最后一次设置」
    /// 决定。而各面板底色不同（行卡片 `card_bg`、顶栏 `bg`、搜索/类型栏
    /// `surface`），最终生效的只是最后画的那一块 ⇒ 其余面板被串色，
    /// 且不报错。
    #[test]
    fn per_rect_tint_is_not_shared() {
        let row_fill = egui::Color32::from_rgb(0x2F, 0x35, 0x40);
        let bar_fill = egui::Color32::from_rgb(0x10, 0x12, 0x15);
        let params = GlassParams {
            rects: vec![
                GlassRect::new(
                    Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0)),
                    8.0,
                    row_fill,
                    1.0,
                ),
                GlassRect::new(
                    Rect::from_min_size(pos2(0.0, 60.0), vec2(100.0, 40.0)),
                    6.0,
                    bar_fill,
                    0.5,
                ),
            ],
            refract: 0.3,
            dispersion: 0.2,
            edge: 5.0,
        };
        let u = params.uniform(1.0, vk::Extent2D { width: 800, height: 600 });

        let want_row = GlassRect::new(Rect::NOTHING, 0.0, row_fill, 1.0);
        let want_bar = GlassRect::new(Rect::NOTHING, 0.0, bar_fill, 0.5);
        assert_eq!(u.tints[0][..3], want_row.tint[..], "第一块的底色被串染");
        assert_eq!(u.tints[1][..3], want_bar.tint[..], "第二块的底色被串染");
        assert_ne!(
            u.tints[0][..3],
            u.tints[1][..3],
            "前置条件：两块底色应不同"
        );
        assert_eq!(u.alphas[0][0], want_row.alpha);
        assert_eq!(u.alphas[1][0], want_bar.alpha);
        assert_eq!(u.count, 2);
    }

    /// `GlassRect::new` 的派生关系：tint 取**非预乘**原色，alpha 取覆盖度。
    ///
    /// # 为什么这条重要
    ///
    /// `Color32` 是预乘存储。半透明底色若直接拿预乘分量当 tint，
    /// 颜色会随「卡片不透明度」二次变暗（预乘一次 + 混合一次）。
    /// 必须走 `to_srgba_unmultiplied` 还原色相。
    #[test]
    fn glass_rect_derives_unmultiplied_tint() {
        // 半透明红：预乘分量约为 (128, 0, 0)，非预乘应是 (255, 0, 0)。
        let fill = egui::Color32::from_rgba_unmultiplied(255, 0, 0, 128);
        let g = GlassRect::new(Rect::NOTHING, 4.0, fill, 128.0 / 255.0);
        assert!((g.alpha - 128.0 / 255.0).abs() < 1e-6, "覆盖度应等于底色 alpha");
        assert!(g.tint[0] > 0.95, "tint 应是未预乘的原色，实际 {}", g.tint[0]);
        assert!(g.tint[1] < 0.01 && g.tint[2] < 0.01);
        // tint 强度随覆盖度单调：半透明 → 约半个上限。
        let full = GlassRect::new(Rect::NOTHING, 4.0, egui::Color32::from_rgb(255, 0, 0), 1.0);
        assert!(g.tint_strength < full.tint_strength);
        assert!((full.tint_strength - crate::glass::CARD_TINT_MAX).abs() < 1e-6);
    }

    /// 默认值必须「不透明」而不是「全透」。
    ///
    /// 万一某条路径漏写 alpha，玻璃退化成实底还能看；退化成全透会让
    /// 界面像坏掉（整窗透出桌面、没有卡片）。
    #[test]
    fn default_alpha_is_opaque() {
        let u = GlassUniforms::default();
        assert!(u.alphas.iter().all(|a| a[0] == 1.0));
        assert_eq!(u.count, 0);
    }
}
