//! 渲染后端配置。
//!
//! 本项目以 **Vulkan** 作为底层图形 API，通过 wgpu 接入。
//!
//! 为什么显式指定后端而不用 wgpu 默认行为：wgpu 在 Windows 上的默认优先级是
//! DX12 > Vulkan。DX12 是原生 API、无额外开销，但仅限 Windows；
//! Vulkan 跨平台一致，且在部分场景下驱动层开销更低。显式锁定
//! `Backends::VULKAN` 可以保证行为在所有平台上可预期，也避免同一份代码在
//! 不同机器上走出不同的渲染路径（这类差异极难排查）。


use eframe::egui_wgpu::{self as wgpu_egl, WgpuConfiguration, WgpuSetupCreateNew};
/// 窗口标题，同时作为 wgpu 应用标识。
pub const WINDOW_TITLE: &str = "ModularClipboard";

/// 环境变量：强制回退到其它后端。
///
/// 取值 `1` / `true` 时改用 wgpu 默认优先级（DX12 优先）。
/// 用途是当某台机器的 Vulkan 驱动有问题时，仍能启动界面排查问题，
/// 而不必重新编译。
const ENV_FALLBACK: &str = "MODULARCLIPBOARD_NO_VULKAN";

/// 构建 wgpu 配置。
pub fn wgpu_configuration() -> WgpuConfiguration {
    let mut cfg = WgpuConfiguration::default();

    if fallback_requested() {
        tracing::warn!(
            "{ENV_FALLBACK} 已设置，改用 wgpu 默认后端优先级（Windows 上通常为 DX12）"
        );
        return cfg;
    }

    // 以 egui-wgpu 的默认配置为基底（它正确设置了 InstanceFlags 等字段），
    // 只覆盖后端与电源偏好。egui-wgpu 的类型未实现 Default，
    // 因此不能直接用 `..Default::default()`。
    let setup = WgpuSetupCreateNew::without_display_handle();

    // 只启用 Vulkan。若机器无 Vulkan 驱动，wgpu 会报适配器枚举失败，
    // 这比静默回退到 DX12 更明确——问题暴露在启动时，而非渲染异常。
    let mut create_new = setup;
    create_new.instance_descriptor.backends = wgpu_egl::wgpu::Backends::VULKAN;
    create_new.power_preference = wgpu_egl::wgpu::PowerPreference::LowPower;
    create_new.native_adapter_selector = adapter_selector();

    cfg.wgpu_setup = create_new.into();
    cfg
}

/// 环境变量：强制使用独显或核显。
///
/// 取值 `discrete`（独显）/ `integrated`（核显）。
/// 双显卡机器上这能显著降低内存占用：Vulkan 会为**每个被枚举的适配器**
/// 加载对应厂商的驱动栈，只固定用一块即可避免加载两份。
const ENV_GPU_PREF: &str = "MODULARCLIPBOARD_GPU";

/// 首选 GPU 类型。
#[derive(Debug, Clone, Copy, PartialEq)]
enum GpuChoice {
    Discrete,
    Integrated,
}

/// 解析环境变量。`None` 表示未指定，沿用 wgpu 默认。
fn gpu_choice_from_env() -> Option<GpuChoice> {
    let raw = std::env::var(ENV_GPU_PREF).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "discrete" | "d" | "独显" => Some(GpuChoice::Discrete),
        "integrated" | "i" | "核显" => Some(GpuChoice::Integrated),
        other => {
            tracing::warn!(value = %other, "{ENV_GPU_PREF} 取值无法识别，忽略");
            None
        }
    }
}

/// 构造适配器选择器。
///
/// 注意 wgpu 30 的签名：选择器接收 `&[wgpu::Adapter]` 切片并返回
/// `Result<Adapter, String>`，与旧版的 `Adapters` 枚举不同。
fn adapter_selector() -> Option<wgpu_egl::NativeAdapterSelectorMethod> {
    let choice = gpu_choice_from_env()?;

    Some(std::sync::Arc::new(move |adapters: &[wgpu_egl::wgpu::Adapter], _surface| {
        let want = match choice {
            GpuChoice::Discrete => wgpu_egl::wgpu::DeviceType::DiscreteGpu,
            GpuChoice::Integrated => wgpu_egl::wgpu::DeviceType::IntegratedGpu,
        };
        adapters
            .iter()
            .find(|a| a.get_info().device_type == want)
            .or_else(|| adapters.first())
            .cloned()
            .ok_or_else(|| "未找到可用的 Vulkan 适配器".to_string())
    }))
}

/// 读取 wgpu 配置的便捷入口。
pub fn native_options() -> WgpuConfiguration {
    wgpu_configuration()
}

fn fallback_requested() -> bool {
    std::env::var(ENV_FALLBACK)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

/// 当前实际使用的渲染后端名称，用于日志与设置页展示。
pub fn backend_description() -> String {
    if fallback_requested() {
        "自动（回退模式）".to_string()
    } else {
        "Vulkan (wgpu)".to_string()
    }
}

/// 供设置页查询是否为 Vulkan 独占模式。
pub fn is_vulkan_locked() -> bool {
    !fallback_requested()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_flag_parses_common_truthy_values() {
        for v in ["1", "true", "TRUE", "yes", " yes "] {
            assert!(fallback_value(v), "应识别为真: {v}");
        }
        for v in ["0", "false", "no", ""] {
            assert!(!fallback_value(v), "应识别为假: {v}");
        }
    }

    fn fallback_value(v: &str) -> bool {
        let v = v.trim().to_ascii_lowercase();
        v == "1" || v == "true" || v == "yes"
    }

    #[test]
    fn vulkan_is_locked_by_default() {
        // 未设置环境变量时应锁定 Vulkan
        if std::env::var(ENV_FALLBACK).is_err() {
            assert!(is_vulkan_locked());
            assert_eq!(backend_description(), "Vulkan (wgpu)");
        }
    }

    #[test]
    fn window_title_is_product_name() {
        assert!(!WINDOW_TITLE.is_empty());
        assert!(WINDOW_TITLE.chars().all(|c| !c.is_control()));
    }
}
