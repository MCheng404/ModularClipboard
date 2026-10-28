//! GPU 自检探针。
//!
//! # 为什么需要它
//!
//! 「玻璃 / 磨砂全都不见了」这类故障，有一半的根因在 GPU 侧：
//! 选了张不支持呈现的卡、`create_device` 失败、驱动只暴露了软件渲染。
//! 这些情况下界面照常画（CPU 侧一切正常），只是效果变淡或变灰——
//! 用户和开发者都很难从画面上判断「是不是显卡的问题」。
//!
//! 本模块把「枚举 → 选择 → 建设备」这条链跑一遍并给出结构化结论：
//! - 启动时由 [`probe`] 自动跑一次，结果写进日志（`RUST_LOG=info` 可见）；
//! - 设置页用它列适配器（[`crate::enumerate_adapters`]）；
//! - 单测覆盖 [`format_report`] 与选择原因（纯函数，不需要 GPU）。
//!
//! # 它能查什么、不能查什么
//!
//! 能查：适配器是否可见、有没有图形队列、`vkCreateDevice` 是否成功。
//! **不能**查：交换链与呈现——那需要 surface，而 surface 需要窗口。
//! 探针刻意不建窗口：设置页要能在任何时候调用它。

use crate::{AdapterInfo, GpuSelection, score_adapter};
use ash::vk;

/// 探针选中的适配器是**为什么**被选中的。
///
/// 报告里必须写清楚：用户选了「核显」却拿到独显时，第一件事就是看
/// 这里是 `NamedFallback`（名字没匹配上）还是 `Auto`（配置根本没读进去）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickReason {
    /// `Named` 精确命中。
    Named,
    /// `Named` 没命中，回退自动选择。
    NamedFallback,
    /// 类型偏好（独显 / 核显）命中。
    Preference,
    /// 自动（独显优先）。
    Auto,
    /// 没有任何可用适配器。
    None,
}

impl PickReason {
    /// 报告里的中文说明。
    pub fn label(self) -> &'static str {
        match self {
            PickReason::Named => "按名称指定",
            PickReason::NamedFallback => "指定的适配器不存在，已回退自动",
            PickReason::Preference => "按类型偏好",
            PickReason::Auto => "自动（独显优先）",
            PickReason::None => "无可用适配器",
        }
    }
}

/// 一次自检的结论。
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// 本机全部适配器（顺序与驱动枚举一致）。
    pub adapters: Vec<AdapterInfo>,
    /// 选中的那个；没有任何可用适配器时为 `None`。
    pub picked: Option<AdapterInfo>,
    /// 选中原因。
    pub reason: PickReason,
    /// `vkCreateDevice` 是否成功。
    pub device_ok: bool,
    /// 失败原因（枚举失败 / 建设备失败）。成功时为 `None`。
    pub error: Option<String>,
}

impl ProbeReport {
    /// 一行摘要（启动日志用）。
    ///
    /// 形如 `GPU 2 张 / 选中 NVIDIA RTX 4090（独显） / 设备创建成功`。
    /// 不换行：启动日志里挤在一条更好 grep。
    pub fn summary(&self) -> String {
        let n = self.adapters.len();
        let picked = match &self.picked {
            Some(a) => a.display(),
            None => "（无）".to_string(),
        };
        let dev = if self.device_ok { "成功" } else { "失败" };
        format!("GPU {n} 张 / 选中 {picked} / 设备创建{dev}")
    }
}

/// 跑一次自检。
///
/// 失败一律走 `ProbeReport::error` 而不是 `Result`：探针是诊断工具，
/// 「查不出来」本身就是要报告的结果，调用方不该为它写错误处理分支。
pub fn probe(selection: &GpuSelection) -> ProbeReport {
    let mut report = ProbeReport {
        adapters: Vec::new(),
        picked: None,
        reason: PickReason::None,
        device_ok: false,
        error: None,
    };

    // ---- 建临时实例（无扩展：枚举与建设备都不需要 surface）----
    //
    // `_entry` 必须活到 `destroy_instance` 之后（见 `create_headless_instance`
    // 的文档：Entry 先析构 = STATUS_ACCESS_VIOLATION）。
    let (_entry, instance) = match crate::create_headless_instance() {
        Ok(v) => v,
        Err(e) => {
            report.error = Some(format!("创建 Vulkan 实例失败: {e:#}"));
            return report;
        }
    };

    let devices = match unsafe { instance.enumerate_physical_devices() } {
        Ok(d) => d,
        Err(e) => {
            report.error = Some(format!("枚举物理设备失败: {e}"));
            unsafe { instance.destroy_instance(None) };
            return report;
        }
    };
    report.adapters = devices
        .iter()
        .map(|d| crate::adapter_info(&instance, *d))
        .collect();

    // ---- 选择：与 `select_device` 同一套打分 ----
    //
    // CAUTION: 这里**不校验呈现能力**（需要 surface）。所以探针选中的卡
    // 有可能在真正建表面时被 `select_device` 排除——报告里必须说明这一点，
    // 否则用户会拿探针结论去质疑界面行为。
    let mut best: Option<(i32, usize)> = None;
    for (i, a) in report.adapters.iter().enumerate() {
        if !a.graphics {
            continue;
        }
        let score = score_adapter(selection, a.device_type, &a.name);
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }

    let (score, idx, reason) = match best {
        Some((s, i)) if s > 0 => {
            let r = match selection {
                GpuSelection::Named(_) => PickReason::Named,
                GpuSelection::Preference(_) => PickReason::Preference,
                GpuSelection::Auto => PickReason::Auto,
            };
            (s, i, r)
        }
        // `Named` 全场 0 分：回退 Auto 再选一次（与 `select_device` 一致）。
        _ if matches!(selection, GpuSelection::Named(_)) => {
            let mut fallback: Option<(i32, usize)> = None;
            for (i, a) in report.adapters.iter().enumerate() {
                if !a.graphics {
                    continue;
                }
                let score = score_adapter(&GpuSelection::Auto, a.device_type, &a.name);
                if fallback.is_none_or(|(s, _)| score > s) {
                    fallback = Some((score, i));
                }
            }
            match fallback {
                Some((s, i)) => (s, i, PickReason::NamedFallback),
                None => {
                    report.error = Some("没有带图形队列的 Vulkan 设备".to_string());
                    unsafe { instance.destroy_instance(None) };
                    return report;
                }
            }
        }
        _ => {
            report.error = Some("没有带图形队列的 Vulkan 设备".to_string());
            unsafe { instance.destroy_instance(None) };
            return report;
        }
    };
    let _ = score;

    let picked = report.adapters[idx].clone();

    // ---- 建设备：验证这张卡真的能用 ----
    //
    // 只建 device + 取队列，不建任何资源。这是「能跑」的最低门槛：
    // 建不出 device 的卡，后面所有渲染都无从谈起。
    match create_probe_device(&instance, devices[idx]) {
        Ok(()) => report.device_ok = true,
        Err(e) => report.error = Some(format!("创建逻辑设备失败: {e:#}")),
    }

    report.picked = Some(picked);
    report.reason = reason;
    unsafe { instance.destroy_instance(None) };
    report
}

/// 建一个最小的逻辑设备并立刻销毁（只验证「能不能建」）。
fn create_probe_device(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> anyhow::Result<()> {
    let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    let qf = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .ok_or_else(|| anyhow::anyhow!("该设备没有 GRAPHICS 队列族"))? as u32;

    let priorities = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(qf)
        .queue_priorities(&priorities);
    let create = vk::DeviceCreateInfo::default().queue_create_infos(std::slice::from_ref(&queue_info));

    // SAFETY：`instance` 存活，`create` 只引用栈上的 `queue_info`。
    let device = unsafe { instance.create_device(physical, &create, None) }
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // SAFETY：device 刚建、没有提交过任何工作。
    unsafe { device.destroy_device(None) };
    Ok(())
}

/// 把报告渲染成多行文本（设置页 / 日志 / 单测共用）。
///
/// 纯函数：不碰 GPU，测试直接喂构造好的报告。
pub fn format_report(r: &ProbeReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("GPU 探针：共 {} 个适配器\n", r.adapters.len()));
    for (i, a) in r.adapters.iter().enumerate() {
        out.push_str(&format!(
            "  [{i}] {}  Vulkan {}.{}  {}\n",
            a.display(),
            a.api.0,
            a.api.1,
            if a.graphics { "图形队列 ✓" } else { "无图形队列 ✗" }
        ));
    }
    match &r.picked {
        Some(a) => out.push_str(&format!("选中：{}（{}）\n", a.display(), r.reason.label())),
        None => out.push_str(&format!("选中：（无）—— {}\n", r.reason.label())),
    }
    out.push_str(&format!(
        "设备创建：{}\n",
        if r.device_ok { "成功" } else { "失败" }
    ));
    if let Some(e) = &r.error {
        out.push_str(&format!("错误：{e}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, t: vk::PhysicalDeviceType, graphics: bool) -> AdapterInfo {
        AdapterInfo {
            name: name.to_string(),
            device_type: t,
            api: (1, 3),
            graphics,
        }
    }

    /// 报告必须同时给出「有几个」「选了谁」「为什么」「成不成」。
    ///
    /// 少任何一项，用户都得去猜：只有「设备创建失败」而不知道选了哪张卡，
    /// 排查等于从零开始。
    #[test]
    fn report_mentions_every_essential_fact() {
        let r = ProbeReport {
            adapters: vec![
                info("NVIDIA GeForce RTX 4090", vk::PhysicalDeviceType::DISCRETE_GPU, true),
                info("Intel UHD 770", vk::PhysicalDeviceType::INTEGRATED_GPU, true),
            ],
            picked: Some(info(
                "NVIDIA GeForce RTX 4090",
                vk::PhysicalDeviceType::DISCRETE_GPU,
                true,
            )),
            reason: PickReason::Auto,
            device_ok: true,
            error: None,
        };
        let text = format_report(&r);
        assert!(text.contains("共 2 个适配器"), "{text}");
        assert!(text.contains("NVIDIA GeForce RTX 4090（独显）"), "{text}");
        assert!(text.contains("Intel UHD 770（核显）"), "{text}");
        assert!(text.contains("自动（独显优先）"), "{text}");
        assert!(text.contains("设备创建：成功"), "{text}");
        // 没有图形队列的适配器必须被标出来（它是「为什么没选它」的答案）。
        let r2 = ProbeReport {
            adapters: vec![info("Compute Only", vk::PhysicalDeviceType::OTHER, false)],
            picked: None,
            reason: PickReason::None,
            device_ok: false,
            error: Some("没有带图形队列的 Vulkan 设备".to_string()),
        };
        let text = format_report(&r2);
        assert!(text.contains("无图形队列 ✗"), "{text}");
        assert!(text.contains("错误："), "{text}");
    }

    /// 摘要必须单行（启动日志里挤成一条才好 grep）。
    #[test]
    fn summary_is_single_line() {
        let r = ProbeReport {
            adapters: vec![info("A", vk::PhysicalDeviceType::DISCRETE_GPU, true)],
            picked: Some(info("A", vk::PhysicalDeviceType::DISCRETE_GPU, true)),
            reason: PickReason::Named,
            device_ok: true,
            error: None,
        };
        let s = r.summary();
        assert!(!s.contains('\n'), "摘要不得换行：{s}");
        assert!(s.contains("GPU 1 张"), "{s}");
        assert!(s.contains("设备创建成功"), "{s}");
    }

    /// 探针**真的**跑一遍：不 panic，且报告自洽。
    ///
    /// # 为什么这条测试依赖机器上的显卡
    ///
    /// 它是唯一能覆盖「枚举 → 选择 → 建 vkDevice」整条链的测试——纯函数
    /// 测试只能验证报告格式与打分。在没有 Vulkan 的机器上（CI 容器、
    /// 只装了 DX 的虚拟机）`probe` 会返回 `error`，但**不会 panic**，
    /// 所以断言依然成立：这条测试守的是「不崩 + 结论自洽」，不是
    /// 「一定有显卡」。
    ///
    /// `device_ok == true` 时必须是本机真有可用 GPU，此时不允许同时有
    /// `error`——两个字段互相矛盾会让用户拿到「失败但成功」的报告。
    #[test]
    fn probe_runs_end_to_end_without_panicking() {
        let r = probe(&GpuSelection::Auto);
        // 报告文本任何时候都能渲染（含全失败的情况）。
        assert!(!format_report(&r).is_empty());
        assert!(!r.summary().is_empty());

        let has_graphics = r.adapters.iter().any(|a| a.graphics);
        if has_graphics {
            assert!(
                r.picked.is_some(),
                "有带图形队列的适配器却没选中任何一个"
            );
            assert_ne!(r.reason, PickReason::None, "选中了却没说原因");
        } else {
            // 没有可用适配器：必须给出原因，不能静默空报告。
            assert!(r.error.is_some(), "无可用适配器时必须写明原因");
        }
        if r.device_ok {
            assert!(r.error.is_none(), "device_ok 与 error 不能同时成立");
            assert!(r.picked.is_some(), "建成了设备却没记录是哪张卡");
        }
    }

    /// 每种选择原因都有非空的中文说明。
    #[test]
    fn every_reason_has_a_label() {
        for r in [
            PickReason::Named,
            PickReason::NamedFallback,
            PickReason::Preference,
            PickReason::Auto,
            PickReason::None,
        ] {
            assert!(!r.label().is_empty(), "{r:?} 缺说明");
        }
        // 回退必须**说出来**——静默换卡是这轮要避免的故障。
        assert!(PickReason::NamedFallback.label().contains("回退"));
    }
}
