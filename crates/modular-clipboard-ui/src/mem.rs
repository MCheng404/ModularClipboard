//! 进程内存账目。**只用于诊断**，帮助定位驻留内存来自哪个子系统。
//!
//! # 为什么需要它
//!
//! 后台常驻的剪贴板工具应当是「几乎不占内存」的。实测 2026-10-12：
//! 只跑 MCP 服务 `--mcp-http` 时进程私有提交 **2.3MB**，而带界面跑起来
//! 是 **~165MB** —— 多出来的 160MB 全在「窗口 + GPU + egui」这条链路上。
//! 要优化它，先得知道是哪个**阶段**吃掉的：选项是创建了 Vulkan 设备、
//! 是 egui 的字体图集，还是别的东西。凭猜没法定，所以要打点。
//!
//! # 两个口径的区别
//!
//! - **私有提交**（`PrivateUsage`）：进程向系统承诺的内存。真正的成本。
//! - **工作集**（`WorkingSetSize`）：当前驻留在物理内存里的部分。可以
//!   被系统回收，也能用 `SetProcessWorkingSetSizeEx(h, -1, -1)` 主动
//!   裁剪。任务管理器默认显示的是「专用工作集」，比提交小。
//!
//! 优化要看提交；只把工作集压下去属于**自欺**：攒在自己手里的内存
//! 系统并没能拿去给别人用。
//!
//! # 怎么用
//!
//! 环境变量 `MC_MEM_TRACE=1` 打开打点（[`trace_stage`] 只在开关打开时
//! 被调用）：帧循环每 [`crate::multiwindow::FRAME_LOG_INTERVAL`] 帧记一条
//! `frame`，隐藏空闲退出前记一条 `idle_exit`。日志 target 是 `mem`，
//! 可用 `RUST_LOG=mem=info` 单独过滤。
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::GetCurrentProcess;

/// 当前进程的（私有提交 MB, 工作集 MB）。
pub fn current_mb() -> (f64, f64) {
    // NOTE: 必须用 **EX** 结构 + `size_of::<PROCESS_MEMORY_COUNTERS_EX>()`：
    // 基础 `PROCESS_MEMORY_COUNTERS` 里没有 `PrivateUsage`（只有
    // `PagefileUsage`），而 `PrivateUsage` 才是「私有提交」——真正的成本。
    // 传的 cb 决定 psapi 往里写多少字段，填小了就拿不到它。
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
    // SAFETY: 只查询本进程，句柄来自 `GetCurrentProcess()`（伪句柄）；
    // 缓冲区是栈上的 EX 结构，`cb` 按其真实尺寸给出。
    let ok = unsafe {
        GetProcessMemoryInfo(
            HANDLE(GetCurrentProcess().0),
            &mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    if !ok.is_ok() {
        return (-1.0, -1.0);
    }
    const MB: f64 = 1024.0 * 1024.0;
    (
        pmc.PrivateUsage as f64 / MB,
        pmc.WorkingSetSize as f64 / MB,
    )
}

/// 打一条阶段账目：`tracing::info!` 到 default target。
///
/// 增量（相对上一次打点）是最有用的那一列——它直接回答「这一段吃了多少」。
pub fn trace_stage(what: &str) {
    let (priv_now, ws_now) = current_mb();
    // 上一次的读数放在 thread-local 里：诊断期间 `run` 是单线程推进的，
    // 不需要跨线程同步，也避免用 static mutable。
    use std::cell::Cell;
    thread_local! {
        static LAST: Cell<(f64, f64)> = const { Cell::new((f64::NAN, f64::NAN)) };
    }
    let (prev_priv, prev_ws) = LAST.get();
    LAST.set((priv_now, ws_now));
    let (d_priv, d_ws) = if prev_priv.is_nan() {
        (0.0, 0.0)
    } else {
        (priv_now - prev_priv, ws_now - prev_ws)
    };
    tracing::info!(
        target: "mem",
        stage = what,
        private_mb = round1(priv_now),
        delta_mb = round1(d_priv),
        ws_mb = round1(ws_now),
        ws_delta_mb = round1(d_ws),
        "内存阶段账目"
    );
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
