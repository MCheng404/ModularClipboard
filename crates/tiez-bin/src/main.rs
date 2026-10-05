//! ModularClipboard 剪贴板管理器 —— 纯 Rust 实现。
//!
//! 单进程启动流程：
//! 1. 解析命令行；
//! 2. 加载配置（损坏时回退默认值，不阻断启动）；
//! 3. 初始化日志；
//! 4. 启动界面与后台捕获。

use std::path::PathBuf;

use tiez_core::Config;

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
  tiez [选项]

选项:
      --data-dir <路径>   指定数据目录（默认 %APPDATA%/tiez）
      --no-capture         启动时不监听剪贴板（调试用）
  -V, --version           显示版本
  -h, --help              显示帮助
",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "modular-clipboard")
        .map(|d| d.config_dir().join("config.json"))
        .unwrap_or_else(|| PathBuf::from("tiez-config.json"))
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
        println!("tiez {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    init_tracing();

    let config = Config::load(&config_path()).unwrap_or_default();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = ?config_path().parent(),
        "ModularClipboard 启动"
    );

    if let Err(e) = tiez_ui::run(config) {
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
        .unwrap_or_else(|_| EnvFilter::new("warn,tiez=info"));

    let init = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);

    // 无控制台（如 GUI 程序从资源管理器启动）时忽略错误即可。
    let _ = init.try_init();
}
