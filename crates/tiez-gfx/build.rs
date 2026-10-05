//! 编译期把 WGSL 编译为 SPIR-V。
//!
//! 为什么不用 glslc / glslangValidator：它们是 C++ 工具链，会给项目引入
//! 外部构建依赖。而 naga 是纯 Rust 的 WGSL 前端加 SPIR-V 后端，能在
//! build.rs 里直接跑，保持「只需 cargo build 即可」的特性。
//!
//! naga 只在编译期出现，不进最终二进制——这正是用 build-dependency 而非
//! 普通 dependency 的原因。

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let shader_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("shaders");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", shader_dir.display());

    let entries = match fs::read_dir(&shader_dir) {
        Ok(e) => e,
        Err(e) => {
            panic!("无法读取着色器目录 {}: {e}", shader_dir.display());
        }
    };

    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("wgsl") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };

        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {e}", path.display()));

        let spirv = compile(&source, &path.display().to_string());

        // 同时输出原始字节与 32 位字面量表：
        // 前者供 include_bytes! 嵌入，后者供 Rust 侧逐字创建着色器模块。
        let bytes: Vec<u8> = spirv.iter().flat_map(|w| w.to_le_bytes()).collect();
        let words = spirv
            .iter()
            .map(|w| format!("0x{w:08x}"))
            .collect::<Vec<_>>()
            .join(", ");

        let bin_path = out_dir.join(format!("{name}.spv"));
        fs::write(&bin_path, &bytes)
            .unwrap_or_else(|e| panic!("写入 {} 失败: {e}", bin_path.display()));

        let rs_path = out_dir.join(format!("{name}.rs"));
        // 注意：生成的文件是被 `include!` 进**表达式位置**的，
        // 因此只能出现字面量，不能有 `pub` 或 `//!`（会被当作项文档）。
        fs::write(
            &rs_path,
            format!(
                "// 由 build.rs 从 shaders/{name}.wgsl 生成，请勿手工修改。\n\
                 [{words}]\n"
            ),
        )
        .unwrap_or_else(|e| panic!("写入 {} 失败: {e}", rs_path.display()));

        count += 1;
    }

    if count == 0 {
        panic!("在 {} 下未找到任何 .wgsl 文件", shader_dir.display());
    }
}

/// WGSL → SPIR-V。
fn compile(source: &str, origin: &str) -> Vec<u32> {
    let module = match naga::front::wgsl::parse_str(source) {
        Ok(m) => m,
        Err(e) => panic!("WGSL 解析失败（{origin}）:\n{}", e.emit_to_string(source)),
    };

    let info = match naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    {
        Ok(i) => i,
        Err(e) => panic!("WGSL 校验失败（{origin}）:\n{e:?}"),
    };

    naga::back::spv::write_vec(
        &module,
        &info,
        &naga::back::spv::Options::default(),
        None,
    )
    .unwrap_or_else(|e| panic!("SPIR-V 生成失败（{origin}）: {e:?}"))
}
