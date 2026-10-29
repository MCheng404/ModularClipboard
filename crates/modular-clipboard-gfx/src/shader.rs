//! 着色器：加载编译期生成的 SPIR-V 并创建 Vulkan 着色器模块。
//!
//! SPIR-V 由 `build.rs` 中的 naga 在编译期生成，最终二进制内不含 WGSL 文本
//! 也不含 naga 本身。
//!
//! 一个 WGSL 文件里同时有 `vs_main` 与 `fs_main` 两个入口点，编译后是
//! **一个** SPIR-V 模块；创建管线时按入口点名分别引用，而不是编译两次。
//! 这样省一半编译产物，也避免两份代码不同步的风险。

use ash::vk;

/// `shaders/egui.wgsl` 编译出的 SPIR-V 词。
///
/// 生成文件里是一段裸的 `[..]` 数组字面量，由 build.rs 产出。
const SPIRV_WORDS: &[u32] = &include!(concat!(env!("OUT_DIR"), "/egui.rs"));

/// `shaders/blur.wgsl` 编译出的 SPIR-V 词。
///
/// 与 `egui.wgsl` 各是一个模块：两者的顶点输入布局、描述符布局、
/// 管线状态都不同，合在一起没有意义。`build.rs` 会把 `shaders/` 下
/// 每个 `.wgsl` 都编译成同名的 `{name}.rs`，这里按名 include 即可。
const BLUR_SPIRV_WORDS: &[u32] = &include!(concat!(env!("OUT_DIR"), "/blur.rs"));

/// WGSL 中的顶点入口点名。
pub const VERTEX_ENTRY: &std::ffi::CStr = c"vs_main";
/// WGSL 中的片元入口点名。
pub const FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_main";

/// `blur.wgsl` 的全屏三角形顶点入口。
pub const BLUR_VERTEX_ENTRY: &std::ffi::CStr = c"vs_fullscreen";
/// `blur.wgsl` 的分离式高斯片元入口。
pub const BLUR_FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_blur";
/// `blur.wgsl` 的直通拷贝片元入口（降采样 / 合成）。
pub const COPY_FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_copy";
/// `blur.wgsl` 的 tent 上采样片元入口（Dual Filtering 的升采样端）。
pub const UPSAMPLE_FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_upsample";

/// `glass.wgsl` 编译出的 SPIR-V 词。
///
/// 与 `blur.wgsl` 分开而不是并进去：玻璃只有**一个**入口（`fs_glass`），
/// 且要读两张源纹理 + 一份带数组的 uniform，描述符布局与模糊不同。
const GLASS_SPIRV_WORDS: &[u32] = &include!(concat!(env!("OUT_DIR"), "/glass.rs"));

/// `glass.wgsl` 的液态玻璃合成片元入口。
pub const GLASS_FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_glass";

/// 着色器模块。销毁时需调用 [`ShaderModule::destroy`]。
pub struct ShaderModule {
    module: vk::ShaderModule,
}

impl ShaderModule {
    /// 从内嵌 SPIR-V 创建着色器模块。
    pub fn new(device: &ash::Device) -> anyhow::Result<Self> {
        let info = vk::ShaderModuleCreateInfo::default().code(SPIRV_WORDS);
        let module = unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建着色器模块失败: {e:?}"))?;
        Ok(Self { module })
    }

    /// 从内嵌的模糊着色器创建模块（`blur.wgsl`）。
    ///
    /// 与 [`Self::new`] 的唯一区别是嵌入的 SPIR-V 来源不同。两者
    /// 刻意**分开**：`egui.wgsl` 的管线有顶点缓冲输入，模糊管线没有，
    /// 各自引用自己那份模块可避免「拿错入口点名」这类错误在运行期
    /// 才暴露（`create_graphics_pipelines` 报 EntryPointNotFound）。
    pub fn new_blur(device: &ash::Device) -> anyhow::Result<Self> {
        let info = vk::ShaderModuleCreateInfo::default().code(BLUR_SPIRV_WORDS);
        let module = unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建模糊着色器模块失败: {e:?}"))?;
        Ok(Self { module })
    }

    /// 从内嵌的玻璃着色器创建模块（`glass.wgsl`）。
    pub fn new_glass(device: &ash::Device) -> anyhow::Result<Self> {
        let info = vk::ShaderModuleCreateInfo::default().code(GLASS_SPIRV_WORDS);
        let module = unsafe { device.create_shader_module(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建玻璃着色器模块失败: {e:?}"))?;
        Ok(Self { module })
    }

    /// 句柄。
    pub fn handle(&self) -> vk::ShaderModule {
        self.module
    }

    /// 按**显式入口点名**构造顶点阶段。
    ///
    /// 供 `blur.wgsl` 这类「一个模块多个入口」的场景使用：
    /// [`Self::vertex_stage`] 写死了 `vs_main`，而模糊模块的入口叫
    /// `vs_fullscreen`。
    pub fn vertex_stage_named(
        &self,
        entry: &'static std::ffi::CStr,
    ) -> vk::PipelineShaderStageCreateInfo<'_> {
        stage(vk::ShaderStageFlags::VERTEX, self.module, entry)
    }

    /// 按**显式入口点名**构造片元阶段。见 [`Self::vertex_stage_named`]。
    pub fn fragment_stage_named(
        &self,
        entry: &'static std::ffi::CStr,
    ) -> vk::PipelineShaderStageCreateInfo<'_> {
        stage(vk::ShaderStageFlags::FRAGMENT, self.module, entry)
    }

    /// 构造顶点着色阶段。
    pub fn vertex_stage(&self) -> vk::PipelineShaderStageCreateInfo<'_> {
        stage(vk::ShaderStageFlags::VERTEX, self.module, VERTEX_ENTRY)
    }

    /// 构造片元着色阶段。
    pub fn fragment_stage(&self) -> vk::PipelineShaderStageCreateInfo<'_> {
        stage(
            vk::ShaderStageFlags::FRAGMENT,
            self.module,
            FRAGMENT_ENTRY,
        )
    }

    /// 销毁模块。
    pub fn destroy(&self, device: &ash::Device) {
        unsafe { device.destroy_shader_module(self.module, None) };
    }
}

fn stage<'a>(
    stage_flags: vk::ShaderStageFlags,
    module: vk::ShaderModule,
    entry: &'static std::ffi::CStr,
) -> vk::PipelineShaderStageCreateInfo<'a> {
    vk::PipelineShaderStageCreateInfo::default()
        .stage(stage_flags)
        .module(module)
        // ash 0.38 的 pName 接受 &CStr，而非裸指针。
        .name(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPIRV_MAGIC: u32 = 0x0723_0203;

    /// 校验内嵌 SPIR-V 的头部。
    ///
    /// 廉价但有效的守卫：若 build.rs 的编译流程被改坏（换错输出格式、
    /// 字节序写反等），这里立刻发现，而不是等到 `create_shader_module`
    /// 在运行时抛出难以定位的错误。
    #[test]
    fn spirv_header_is_valid() {
        assert!(!SPIRV_WORDS.is_empty(), "SPIR-V 为空");
        assert_eq!(
            SPIRV_WORDS[0], SPIRV_MAGIC,
            "SPIR-V 魔数错误，说明字节序或输出格式不对"
        );
        assert_ne!(SPIRV_WORDS[1], 0, "SPIR-V version 字段为零");
    }

    #[test]
    fn spirv_size_is_reasonable() {
        // 一个含 uniform + sampler + texture 的着色器应在数百到数千字节。
        // 过大说明嵌入了冗余内容，过小说明编译被截断。
        let bytes = SPIRV_WORDS.len() * 4;
        assert!(bytes > 200, "SPIR-V 过小（{bytes} 字节），可能被截断");
        assert!(bytes < 200_000, "SPIR-V 过大（{bytes} 字节），可能嵌入冗余内容");
    }

    #[test]
    fn blur_spirv_header_is_valid() {
        // 与 `spirv_header_is_valid` 同源的守卫，对象换成模糊模块。
        // 两套模块各自 include 一份生成文件，任一条编译链断掉都要立刻发现。
        assert!(!BLUR_SPIRV_WORDS.is_empty(), "模糊 SPIR-V 为空");
        assert_eq!(
            BLUR_SPIRV_WORDS[0], SPIRV_MAGIC,
            "模糊 SPIR-V 魔数错误，说明 build.rs 未产出 blur.rs 或格式不对"
        );
        let bytes = BLUR_SPIRV_WORDS.len() * 4;
        assert!(
            bytes > 200,
            "模糊 SPIR-V 过小（{bytes} 字节），可能被截断"
        );
    }

    #[test]
    fn blur_entry_names_are_distinct_from_egui() {
        // 模糊模块的入口点名必须与 egui 模块**不同**：若误写成
        // `vs_main`/`fs_main`，模糊管线会引用到一个不存在的入口，
        // 直到 `create_graphics_pipelines` 才报 EntryPointNotFound。
        assert_eq!(BLUR_VERTEX_ENTRY.to_str().unwrap(), "vs_fullscreen");
        assert_eq!(BLUR_FRAGMENT_ENTRY.to_str().unwrap(), "fs_blur");
        assert_eq!(COPY_FRAGMENT_ENTRY.to_str().unwrap(), "fs_copy");
        for e in [BLUR_VERTEX_ENTRY, BLUR_FRAGMENT_ENTRY, COPY_FRAGMENT_ENTRY] {
            assert_ne!(e, VERTEX_ENTRY);
            assert_ne!(e, FRAGMENT_ENTRY);
        }
    }

    #[test]
    fn entry_names_are_valid_c_strings() {
        // Vulkan 要求 pName 是以 NUL 结尾的 C 字符串。
        // c"..." 字面量自带隐式 NUL，这里验证 to_str 可读且内容正确。
        assert_eq!(VERTEX_ENTRY.to_str().unwrap(), "vs_main");
        assert_eq!(FRAGMENT_ENTRY.to_str().unwrap(), "fs_main");
        // 名字必须与 WGSL 中的 @vertex / @fragment 属性名逐一对应，
        // 否则 create_graphics_pipelines 会返回 EntryPointNotFound。
        for (name, entry) in [("vertex", VERTEX_ENTRY), ("fragment", FRAGMENT_ENTRY)] {
            let s = entry.to_str().expect("入口点名应为合法 UTF-8");
            assert!(!s.is_empty(), "{name} 入口点名为空");
        }
    }
}
