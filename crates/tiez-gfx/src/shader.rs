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

/// WGSL 中的顶点入口点名。
pub const VERTEX_ENTRY: &std::ffi::CStr = c"vs_main";
/// WGSL 中的片元入口点名。
pub const FRAGMENT_ENTRY: &std::ffi::CStr = c"fs_main";

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

    /// 句柄。
    pub fn handle(&self) -> vk::ShaderModule {
        self.module
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
