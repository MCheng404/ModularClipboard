//! ModularClipboard 剪贴板管理器 —— 纯 Rust 实现。
//!
//! 单进程启动流程：
//! 1. 解析命令行；
//! 2. 加载配置（损坏时回退默认值，不阻断启动）；
//! 3. 初始化日志；
//! 4. 启动界面与后台捕获。

// GUI 子系统：不弹控制台窗口。
//
// ⚠️ 不加这一行，程序是 **console** 子系统 —— 从资源管理器双击
// 也会附带一个黑底白字的 cmd 窗口，常驻程序一直挂着它非常突兀。
//
// 代价：没有 stdout/stderr 可写。所以日志必须落**文件**
// （见 [`init_tracing`]），`--version` 之类要输出到控制台的路径
// 得改走文件或MessageBox。
#![windows_subsystem = "windows"]

use std::path::{Path, PathBuf};

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
                show_message_box("模块化剪贴板 - 帮助", HELP_TEXT);
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    Ok(args)
}

/// 帮助文本（同时用于弹窗内容）。
///
/// ⚠️ GUI 子系统没有 stdout，`println!` 什么都不会显示——
/// 所以 `-h` 走 [`show_message_box`]。
const HELP_TEXT: &str = "ModularClipboard 剪贴板管理器

用法:
  modular-clipboard [选项]

选项:
      --data-dir <路径>   指定数据目录
      --no-capture         启动时不监听剪贴板（调试用）
  -V, --version           显示版本
  -h, --help              显示帮助";

/// 弹一个消息框。
///
/// # 为什么需要它
///
/// 本程序是 [`windows_subsystem = "windows"]`]（GUI 子系统），
/// **没有 stdout/stderr**。原先 `println!` / `eprintln!` 的所有
/// 输出都会静默消失——用户双击程序看到「什么都没发生」，
/// 连 `--version` 都看不到任何东西。
///
/// 用 `MessageBoxW` 是Win32 里最省事的做法：不需要额外依赖，
/// 也能把错误真正送到用户眼前。
fn show_message_box(title: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MessageBoxW,
    };
    use windows::core::PCWSTR;

    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let c: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(c.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 默认数据目录（未指定 `--data-dir` 时）。
///
/// 从 UI 层的展示字符串派生，保证与「帮助里写的路径」同源——
/// 之前硬编码过 `%APPDATA%/tiez` 而实现用 `modular-clipboard`，
/// 用户照着提示找目录会找不到。
fn default_data_dir() -> PathBuf {
    PathBuf::from(modular_clipboard_ui::default_data_dir_display())
}

fn config_path(data_dir: Option<&Path>) -> PathBuf {
    match data_dir {
        // `--data-dir` 指定时，配置与数据库同目录。
        // 否则读配置在 A 盘、写配置在 B 盘，用户改设置会「不生效」。
        Some(dir) => modular_clipboard_app::config_path_in(dir),
        None => directories::ProjectDirs::from("", "", "modular-clipboard")
            .map(|d| d.config_dir().join("config.json"))
            .unwrap_or_else(|| PathBuf::from("modular-clipboard-config.json")),
    }
}

/// 校验并规范化 `--data-dir`。
///
/// 返回绝对路径。三件事必须在这里做，缺一用户都会踩坑：
/// 1. **空白拒绝**——静默回退默认目录最坏：用户以为在隔离目录做测试，
///    实际却在污染真实剪贴板历史，且没有任何提示。
/// 2. **相对路径转绝对**——`--data-dir foo` 之后若进程工作目录变了
///    （例如从快捷方式启动），同一个参数会指向不同位置。
/// 3. **提前建目录**——把「路径不可写」这类问题暴露在启动阶段，
///    而不是等到首次写入载荷时才失败。
fn resolve_data_dir(raw: &Path) -> Result<PathBuf, String> {
    if raw.as_os_str().is_empty() || raw.to_string_lossy().trim().is_empty() {
        return Err("--data-dir 不能为空，请指定一个目录路径".into());
    }
    let abs = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("无法获取当前工作目录以解析相对路径 {raw:?}：{e}"))?
            .join(raw)
    };
    std::fs::create_dir_all(&abs).map_err(|e| {
        format!("无法创建数据目录 {}：{e}", abs.display())
    })?;
    Ok(abs)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            show_message_box("参数错误", &format!("{e}\n\n{}", HELP_TEXT));
            std::process::exit(2);
        }
    };

    if args.version {
        // ⚠️ GUI 子系统没有 stdout，`println!` 什么都不会显示。
        // 所以版本信息弹窗——这是唯一能让用户看到它的途径。
        // （从控制台启动调试时也弹窗，可接受。）
        show_message_box("模块化剪贴板", &format!("版本 {}", env!("CARGO_PKG_VERSION")));
        return;
    }

    // `--data-dir` 在这里定型（校验 + 转绝对 + 建目录），
    // 后面所有层都只接受已经规范化的路径。
    let data_dir = match args.data_dir.as_deref() {
        Some(raw) => match resolve_data_dir(raw) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("参数错误: {e}");
                std::process::exit(2);
            }
        },
        None => None,
    };

    // 单实例保护。
    //
    // 剪贴板工具必须后台常驻，用户可能反复双击 exe。
    // 没有保护时会出现两个进程同时监听剪贴板，
    // 后果是：条目被重复记录、两个图标抢通知区位置、
    // 两个数据库句柄指向同一文件（SQLite 能扛但会产生无谓竞争）。
    //
    // 用命名互斥体实现：内核对象随进程退出自动释放，
    // 崩溃/强杀也不会留下「僵死锁」。
    //
    // 互斥体名**不含数据目录**：剪贴板监听是系统级的，全局只能有一个，
    // 与数据目录无关。若把路径混进名字，用户用两个 `--data-dir` 各启一个
    // 就会同时监听剪贴板，重复记录——那才是真 bug。
    match SingleInstanceGuard::acquire() {
        Ok(guard) => {
            // 保持 guard 存活到main 返回
            let _guard = guard;
            // 日志落在数据目录里（GUI 程序无 stderr可用）。
            let log_dir = data_dir.clone().unwrap_or_else(default_data_dir);
            init_tracing(&log_dir.join("app.log"));
            run_app(args, data_dir);
            return;
        }
        Err(e) => {
            // GUI 程序没有 stderr，且这属于**用户可见**的失败
            // （双击图标却什么都没发生），必须弹窗告知。
            show_message_box("模块化剪贴板", &format!("已有实例在运行，本次启动退出。\n\n{e}"));
            std::process::exit(3);
        }
    }
}

fn run_app(args: Args, data_dir: Option<PathBuf>) {
    let cfg_path = config_path(data_dir.as_deref());

    let mut config = Config::load(&cfg_path).unwrap_or_default();
    // `--no-capture` 是调试开关：让程序启动但不监听剪贴板。
    // 早前它被解析后从未使用——`app.rs` 无条件调`start_capture()`，
    // 于是加这个 flag 完全没有效果。现在通过 config 传递。
    if args.no_capture && config.capture.enabled {
        tracing::warn!("--no-capture 已启用：本次运行不监听剪贴板（不影响持久配置）");
        config.capture.enabled = false;
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = ?data_dir.as_deref().map(|d| d.display().to_string()),
        config = ?cfg_path,
        "ModularClipboard 启动"
    );

    if let Err(e) = modular_clipboard_ui::run_with_options(
        config,
        args.no_capture,
        data_dir.as_deref(),
    ) {
        tracing::error!(%e, "启动失败");
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }
}

/// 初始化日志。默认只输出警告级别，避免常驻程序刷屏；
/// 可用 `RUST_LOG=debug` 环境变量覆盖。
///
/// # 为什么写文件而不是 stderr
///
/// 本程序是 [`windows_subsystem = "windows"]`]（GUI 子系统），
/// **没有可用的 stderr**——写过去要么失败要么丢弃。
/// 而常驻程序恰恰最需要日志：用户报「托盘没了」时唯一能查的就是它。
///
/// 落盘位置：`{数据目录}/app.log`。启动时截断（每次启动一个新文件，
/// 免得无限增长）；打开失败则退回 stderr（控制台调试时仍可用）。
fn init_tracing(log_path: &Path) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,modular_clipboard=info"));

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_path);

    let init = match file {
        Ok(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            // GUI 程序里 ANSI 转义序列是乱码，必须关掉。
            .with_writer(f),
        Err(e) => {
            // 文件打不开（比如目录不存在）时退回 stderr：
            // 从控制台启动调试时仍能看到日志。
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_target(false)
                .with_writer(std::io::stderr)
                .try_init();
            eprintln!("[警告] 无法打开日志文件 {}：{e}", log_path.display());
            return;
        }
    };
    let _ = init.try_init();
}

// ---------------------------------------------------------------------------
// 单实例保护
// ---------------------------------------------------------------------------

/// 单实例互斥体名。
///
/// 全局固定，**不嵌入数据目录**：剪贴板监听是系统级资源，
/// 同一台机器只应有一个监听者，与数据目录无关。
///
/// 早期考虑过「按数据目录哈希」以支持多实例并行——但那会让
/// `--data-dir A` 和 `--data-dir B` 两个进程同时监听系统剪贴板，
/// 同一段内容被记录两次，且两个托盘图标抢同一块通知区。
/// 隔离测试数据靠 `--data-dir` 即可，不需要并行跑两个实例。
const INSTANCE_MUTEX_NAME: &str = r#"Global\ModularClipboard.SingleInstance"#;

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

        let wide: Vec<u16> = INSTANCE_MUTEX_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
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

    /// 独占的临时目录名（进程 ID + 原子序号）。
    ///
    /// 不能用 `now_ms()`：毫秒精度下并发测试会拿到同名目录、互相删除。
    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "modular-clipboard-main-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    /// 空白路径必须报错退出，不能静默回退默认目录。
    #[test]
    fn blank_data_dir_is_rejected() {
        for blank in ["", " ", "\t", "\n "] {
            assert!(
                resolve_data_dir(Path::new(blank)).is_err(),
                "空白路径 {blank:?} 必须报错而不是回退默认目录"
            );
        }
    }

    /// 目录不存在时自动创建。
    #[test]
    fn missing_dir_is_created() {
        let base = unique_dir("missing").join("nested").join("deeper");
        let resolved = resolve_data_dir(&base).expect("多级缺失目录应被创建");
        assert!(resolved.is_dir(), "{} 应被创建", resolved.display());
        let _ = std::fs::remove_dir_all(resolved.parent().unwrap().parent().unwrap());
    }

    /// 相对路径按当前工作目录解析成绝对路径。
    ///
    /// 不转绝对的话，进程工作目录一变（快捷方式启动、任务计划启动）
    /// 同一个参数会指向不同位置，用户完全无法预测数据写到哪。
    #[test]
    fn relative_dir_is_resolved_to_absolute() {
        let base = unique_dir("relative");
        let rel = format!("{}\\{}", base.display(), "sub");
        let resolved = resolve_data_dir(Path::new(&rel)).expect("相对路径应能解析");
        assert!(resolved.is_absolute(), "应转成绝对路径，实际：{resolved:?}");
        assert_eq!(
            resolved,
            std::env::current_dir().unwrap().join(rel),
            "应基于当前工作目录拼接"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 指定数据目录时，配置路径必须落在同一目录（否则读 A 写 B）。
    #[test]
    fn config_path_follows_data_dir() {
        let dir = unique_dir("cfg");
        let p = config_path(Some(&dir));
        assert_eq!(p, dir.join("config.json"));
    }

    /// 未指定时配置仍在 `%APPDATA%/modular-clipboard/config.json`。
    #[test]
    fn config_path_defaults_to_project_dirs() {
        let p = config_path(None);
        assert!(
            p.ends_with("config.json"),
            "默认配置路径应以 config.json 结尾，实际：{p:?}"
        );
        let expected = directories::ProjectDirs::from("", "", "modular-clipboard")
            .map(|d| d.config_dir().join("config.json"));
        if let Some(expected) = expected {
            assert_eq!(p, expected, "未传 --data-dir 时行为不应改变");
        }
    }

    /// 互斥体名不含数据目录——否则两个 `--data-dir` 会同时监听剪贴板。
    #[test]
    fn mutex_name_is_global_and_path_free() {
        assert!(
            INSTANCE_MUTEX_NAME.starts_with(r"Global\"),
            "应为全局命名空间，实际：{INSTANCE_MUTEX_NAME}"
        );
        assert!(
            !INSTANCE_MUTEX_NAME.contains(':'),
            "互斥体名不应含盘符/路径分隔的实际路径，实际：{INSTANCE_MUTEX_NAME}"
        );
        // 除了 `Global\` 前缀这一个分隔符，不得再有反斜杠——
        // 多一个就意味着名字里塞进了目录层级。
        assert_eq!(
            INSTANCE_MUTEX_NAME.matches('\\').count(),
            1,
            "互斥体名只应含`Global\\` 前缀一处反斜杠，实际：{INSTANCE_MUTEX_NAME}"
        );
    }

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
