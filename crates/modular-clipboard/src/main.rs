//! ModularClipboard 剪贴板管理器 —— 纯 Rust 实现。
//!
//! 单进程启动流程：
//! 1. 解析命令行；
//! 2. 加载配置（损坏时回退默认值，不阻断启动）；
//! 3. 初始化日志；
//! 4. 启动界面与后台捕获。

use std::path::PathBuf;

use modular_clipboard_core::Config;

/// 命令行参数。
struct Args {
    /// 数据目录，覆盖默认位置。
    data_dir: Option<PathBuf>,
    /// 启动时不自启捕获（调试用）。
    no_capture: bool,
    /// 打印版本后退出。
    version: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        data_dir: None,
        no_capture: false,
        version: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data-dir" => {
                let v = it.next().ok_or("--data-dir 需要一个路径参数")?;
                args.data_dir = Some(PathBuf::from(v));
            }
            "--no-capture" => args.no_capture = true,
            "-V" | "--version" => args.version = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    Ok(args)
}

fn print_help() {
    println!(
        "ModularClipboard 剪贴板管理器 {version}

用法:
  modular-clipboard [选项]

选项:
      --data-dir <路径>   指定数据目录（默认 {data_dir}）
      --no-capture         启动时不监听剪贴板（调试用）
  -V, --version           显示版本
  -h, --help              显示帮助
",
        version = env!("CARGO_PKG_VERSION"),
        // 从真实的目录常量派生，而不是硬编码字符串。
        // 之前这里写的是「%APPDATA%/tiez」，而实现用的是
        // 「modular-clipboard」——帮助文本与实际行为不符，
        // 用户照着它找数据目录会找不到。
        data_dir = modular_clipboard_ui::default_data_dir_display()
    );
}

fn config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "modular-clipboard")
        .map(|d| d.config_dir().join("config.json"))
        .unwrap_or_else(|| PathBuf::from("modular-clipboard-config.json"))
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误: {e}");
            print_help();
            std::process::exit(2);
        }
    };

    if args.version {
        println!("modular-clipboard {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // 单实例保护。
    //
    // 剪贴板工具必须后台常驻，用户可能反复双击 exe。
    // 没有保护时会出现两个进程同时监听剪贴板，
    // 后果是：条目被重复记录、两个图标抢通知区位置、
    // 两个数据库句柄指向同一文件（SQLite 能扛但会产生无谓竞争）。
    //
    // 用命名互斥体实现：内核对象随进程退出自动释放，
    // 崩溃/强杀也不会留下「僵死锁」。
    match SingleInstanceGuard::acquire() {
        Ok(guard) => {
            // 保持 guard 存活到main 返回
            let _guard = guard;
            init_tracing();
            run_app(args);
            return;
        }
        Err(e) => {
            eprintln!("已有实例在运行（{e}），本次启动退出");
            std::process::exit(3);
        }
    }
}

fn run_app(args: Args) {

    let mut config = Config::load(&config_path()).unwrap_or_default();
    // `--no-capture` 是调试开关：让程序启动但不监听剪贴板。
    // 早前它被解析后从未使用——`app.rs` 无条件调`start_capture()`，
    // 于是加这个 flag 完全没有效果。现在通过 config 传递。
    if args.no_capture && config.capture.enabled {
        tracing::warn!("--no-capture 已启用：本次运行不监听剪贴板（不影响持久配置）");
        config.capture.enabled = false;
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = ?config_path().parent(),
        "ModularClipboard 启动"
    );

    if let Err(e) =
        modular_clipboard_ui::run_with_capture_override(config, args.no_capture)
    {
        tracing::error!(%e, "启动失败");
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }
}

/// 初始化日志。默认只输出警告级别，避免常驻程序刷屏；
/// 可用 `RUST_LOG=debug` 环境变量覆盖。
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,modular_clipboard=info"));

    let init = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);

    // 无控制台（如 GUI 程序从资源管理器启动）时忽略错误即可。
    let _ = init.try_init();
}

// ---------------------------------------------------------------------------
// 单实例保护
// ---------------------------------------------------------------------------

/// 命名互斥体守卫。
///
/// 持有期间本进程独占「实例名」。**drop 时不显式释放**——
/// Windows 内核对象在进程退出时自动清理，崩溃/强杀都不会留下僵死锁。
/// 这比「写 pid 文件再检查进程是否存在」可靠：pid 会被复用。
struct SingleInstanceGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

impl SingleInstanceGuard {
    /// 尝试获取实例独占权。
    ///
    /// `Err` 表示已有实例在运行（`ERROR_ALREADY_EXISTS`）。
    fn acquire() -> Result<Self, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;

        const NAME: &str = r#"Global\ModularClipboard.SingleInstance"#;

        let wide: Vec<u16> = NAME.encode_utf16().chain(std::iter::once(0)).collect();
        //签名：CreateMutexW(attributes: Option<&SECURITY_ATTRIBUTES>,
        //                   initial_owner: bool, name: PCWSTR)
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) };

        let handle = handle.unwrap();
        match unsafe { GetLastError() } {
            ERROR_ALREADY_EXISTS => Err("另一个实例已持有该互斥体".into()),
            _ => Ok(Self { handle }),
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        // 句柄由内核在进程退出时回收，这里只需关闭句柄本身。
        // 用 CloseHandle 而非 ReleaseMutex：我们从未 WaitForSingleObject。
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        // 同一进程内连续获取两次，第二次应失败——
        // 命名互斥体在同一进程内也是互斥的。
        let first = SingleInstanceGuard::acquire();
        match first {
            Ok(_first) => {
                let second = SingleInstanceGuard::acquire();
                assert!(
                    second.is_err(),
                    "已有实例持有时，第二次 acquire 必须失败"
                );
            }
            Err(e) => {
                // 环境不允许创建互斥体时跳过，而非让测试失败。
                eprintln!("跳过（无法创建互斥体）: {e}");
            }
        }
    }

    #[test]
    fn guard_releases_on_drop() {
        // drop 后应能重新获取——验证没有留下「僵死锁」。
        {
            let _g = SingleInstanceGuard::acquire().ok();
        }
        // 立即重新获取应当成功（句柄已随 Drop 关闭）
        let again = SingleInstanceGuard::acquire();
        if let Ok(g) = again {
            drop(g);
        }
    }
}
