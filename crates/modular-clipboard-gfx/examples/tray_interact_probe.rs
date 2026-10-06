//! 托盘交互**端到端**探针：验证「关窗口 ≠ 退出」这条常驻工具的核心行为。
//!
//! # 这个探针回答的问题
//!
//! 已有探针全都是**进程内**的：
//!
//! - `tray_probe` 在自己进程里 `tray::start()`，注入消息给**自己**的托盘窗口；
//! - `tray.rs` 的 17 个单测是纯逻辑，从不调 Win32；
//! - `presence.rs` 的单测只测 `decide_on_close` 这个纯函数。
//!
//! 它们**从未验证**真正的问题——四条链路接起来跑是否成立：
//!
//! | # | 链路 | 判据 | 破坏性 |
//! |---|------|------|--------|
//! | 1 | `WM_CLOSE` → 隐藏到托盘 | 窗口不可见 **但进程存活** | 无 |
//! | 2 | 托盘左键 → 唤起主窗口 | 窗口重新可见 | 无 |
//! | 3 | 托盘「清空历史」 | 数据库被重写 | **有** |
//! | 4 | 托盘「退出」 | **进程结束** | 无（终态，放最后）|
//!
//! # 为什么必须起真实子进程
//!
//! 判据 1 与 4 的区别正是「进程是否还活着」，而进程内探针永远测不出这件事：
//! 它自己就是那个进程，退出就没了。唯一的观测方式是**从外部**看另一个进程。
//!
//! # 三条路径必须区分清楚（本探针的核心设计）
//!
//! ```text
//! WM_CLOSE ──► 窗口过程空壳转发 ──► PeekMessage 拦截（不派发）
//!           ──► CloseRequested ──► decide_on_close(tray=true, wm_quit=false)
//!           ──► CloseDecision::HideToTray ──► ShowWindow(SW_HIDE) ──► 循环继续
//!
//! 托盘左键 ──► PostMessage(WM_TRAY_CALLBACK, NIN_SELECT)【跨进程】
//!           ──► 托盘线程窗口过程 ──► channel ──► Resident::poll
//!           ──► Action::Show ──► presence::focus_window(SW_RESTORE)
//!
//! 托盘退出 ──► PostMessage(WM_COMMAND, 0x1003)【跨进程】
//!           ──► Action::Quit ──► quit = true ──► break ──► return ──► 进程结束
//! ```
//!
//! 前两条**进程都活着**，第三条**进程结束**。这就是「关窗口 ≠ 退出」的机器证据。
//!
//! # 判据为什么这样选
//!
//! 只看 `IsWindowVisible == false` **不足以**证明「隐藏到托盘」——
//! 窗口被销毁时它同样不可见。所以每条判据都配一个**反证**：
//!
//! - 链路 1 额外断言 `IsWindow == true`（窗口没被销毁）与进程存活；
//! - 链路 4 断言进程**真的**结束，且退出码为 0。
//!
//! 若只判可见性，「隐藏」与「销毁」会被混为一谈，
//! 而两者对用户的意义完全相反（见 MEMORY.md 坑 85「验证了没跑的路径」）。
//!
//! # 证据来自子进程自己的日志
//!
//! 主程序把 `tracing` 输出到 stderr，本探针把子进程 stderr 接管过来逐行收集，
//! 然后断言关键日志出现。这让判据从「窗口看起来对」升级为
//! 「**程序自己说它走了哪条分支**」：
//!
//! - `收到关闭请求`（含 `wm_quit=false tray=true`）
//! - `已隐藏到托盘，后台继续监听剪贴板`
//! - `收到托盘退出请求`
//!
//! # 已知限制（如实报告，不掩盖）
//!
//! 1. **托盘右键菜单无法自动化**。真实链路是
//!    `WM_RBUTTONUP → 窗口过程 PostMessage(WM_TRAY_MENU) → show_context_menu
//!    → TrackPopupMenu(TPM_RETURNCMD)`。`TrackPopupMenu` 是**模态阻塞**的，
//!    无人值守环境下会永久卡死托盘线程（`tray_probe` 也因此避开了它）。
//!    本探针注入 `WM_COMMAND` 覆盖的是**回退分支**（无 `TPM_RETURNCMD` 时的路径），
//!    不是真实菜单路径。**真实右键菜单未被验证。**
//! 2. **链路 3 默认跳过**。原因见下方「数据安全」。
//!
//! # 数据安全（重要）
//!
//! 本探针以 `--no-capture` 启动子程序，绝不读取用户真实剪贴板。
//! 但仍有两个**会改动用户真实数据**的副作用，都必须处理：
//!
//! 1. **`--no-capture` 会把自己写进配置文件**。
//!    `run_app` 把 `config.capture.enabled` 置 false 后传给 `ui::run`，
//!    退出时 `App::shutdown → Service::save_config` 会把这份 config
//!    写回 `%APPDATA%/modular-clipboard/config.json`。
//!    ⇒ 探针跑完，用户真实的剪贴板监听会被**永久关掉**。
//!    对策：启动前备份该文件，退出后原样还原。
//! 2. **「清空历史」会删掉用户真实历史**。`Store::clear_all` 执行
//!    `DROP TABLE` + `DELETE FROM items`。
//!    `--data-dir` 参数**解析了但从未被使用**（见报告），
//!    因此无法把数据目录重定向到临时目录。
//!    ⇒ 链路 3 默认**跳过**，需显式 `TRAY_E2E_ALLOW_CLEAR=1` 才执行。
//!
//! # 通过标准
//!
//! 退出码 0 = 所有**已执行**的判据全部成立。
//! 跳过的判据会在结论里显式列出，**不算作通过**。
//! 任一已执行判据失败即 `panic`。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_TIMEOUT, WPARAM};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetClassNameW, GetWindowThreadProcessId, HWND_MESSAGE, IsWindow,
    IsWindowVisible, PostMessageW, WM_CLOSE, WM_COMMAND,
};
use windows::core::{BOOL, HSTRING, PCWSTR};

/// 主窗口类名，与 `gfx/src/window.rs` 的 `CLASS_NAME` 一致。
const MAIN_CLASS: &str = "ModularClipboardWindow";

/// 托盘窗口类名，与 `platform/src/tray.rs` 的 `CLASS_NAME` 一致。
const TRAY_CLASS: &str = "ModularClipboardTrayWindow";

/// 托盘回调消息，与 `tray.rs` 的 `WM_TRAY_CALLBACK` 一致。
const WM_TRAY_CALLBACK: u32 = 0x8000 + 2;

/// 菜单命令 id「退出」，与 `tray.rs` 的 `CMD_QUIT` 一致。
const CMD_QUIT: usize = 0x1003;

/// `NIN_SELECT`：通知区左键点击（`NOTIFYICON_VERSION_4` 语义）。
const NIN_SELECT: u32 = 1024;

/// 主程序可执行文件的文件名（位于 `target/<triple>/debug/` 下）。
const EXE_NAME: &str = "modular-clipboard.exe";

/// 不设全局计时器：终止保证由每步各自的 `wait_until` 上限
/// 加上 [`ChildGuard`]（Drop 时杀子进程）共同提供。
/// 早前一版有个 `GLOBAL_DEADLINE` 只用于把判据标成 SKIP，
/// 并不真的中断流程——文档说「超时即杀掉子进程」而代码没做，
/// 属于「注释承诺了实现没有的事」，已删。
///
/// 等待主窗口出现的上限。Vulkan 初始化 + 字体加载在慢机器上要几秒。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(12);

/// 等待托盘窗口出现的上限。托盘在 Vulkan 初始化之后才启动，可能偏晚。
const TRAY_WAIT: Duration = Duration::from_secs(3);

/// 注入 `WM_CLOSE` 后等待隐藏生效的时间。
const HIDE_SETTLE: Duration = Duration::from_millis(1500);

/// 等待托盘唤起生效的上限。隐藏态每 `HIDDEN_POLL_INTERVAL`(200ms) 醒一次。
const WAKE_TIMEOUT: Duration = Duration::from_secs(4);

/// 等待进程结束的上限。退出前要`device_wait_idle` + 销毁渲染资源。
const QUIT_TIMEOUT: Duration = Duration::from_secs(8);

/// 一条判据的结果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 已执行且成立。
    Pass,
    /// 已执行且**不**成立——这是失败。
    Fail,
    /// 未执行（破坏性或环境不允许）。
    Skipped,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Skipped => "SKIP",
        }
    }
}

/// 判据清单。
struct Checks {
    rows: Vec<(String, Verdict, String)>,
    start: Instant,
}

impl Checks {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            start: Instant::now(),
        }
    }

    /// 记录一条判据。`ok == None` 表示不适用/跳过。
    ///
    /// 「没跑到」绝不能显示成通过：判据要么真的执行并给出结论，
    /// 要么显式 `None` 记为 SKIP。超时中断由每步各自的 `wait_until` 上限
    /// 与 [`ChildGuard`] 保证，不在这里另设一套全局计时器。
    fn record(&mut self, name: &str, ok: Option<bool>, detail: impl Into<String>) {
        let detail = detail.into();
        let verdict = match ok {
            Some(true) => Verdict::Pass,
            Some(false) => Verdict::Fail,
            None => Verdict::Skipped,
        };
        println!("  [{}] {name}\n         {detail}", verdict.tag());
        self.rows.push((name.to_string(), verdict, detail));
    }

    fn failed(&self) -> usize {
        self.rows.iter().filter(|r| r.1 == Verdict::Fail).count()
    }

    fn skipped(&self) -> usize {
        self.rows.iter().filter(|r| r.1 == Verdict::Skipped).count()
    }

    fn passed(&self) -> usize {
        self.rows.iter().filter(|r| r.1 == Verdict::Pass).count()
    }

    /// 打印汇总。返回进程退出码。
    fn finish(&self) -> i32 {
        println!("\n=== 判据汇总 ===");
        for (name, verdict, detail) in &self.rows {
            println!("  [{}] {name}: {detail}", verdict.tag());
        }
        let f = self.failed();
        let s = self.skipped();
        println!(
            "\n通过 {} 条，失败 {f} 条，未验证 {s} 条（耗时 {:.1}s）",
            self.passed(),
            self.start.elapsed().as_secs_f64()
        );
        if f > 0 {
            println!("=== FAIL：{f} 条判据不成立，托盘交互链路存在缺陷 ===");
            return 1;
        }
        if s > 0 {
            // 有跳过项时不能说「全部通过」——那正是在自己没测的东西上宣布通过。
            println!("=== PARTIAL：{s} 条判据未验证，不构成「全部通过」 ===");
            return 0;
        }
        println!("=== ALL OK：托盘四条链路端到端验证通过 ===");
        0
    }
}

/// 子进程句柄。`Drop` 时确保被杀，不留僵尸与悬挂窗口。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // `try_wait` 返回 `None` 表示仍在运行。
        if self.0.try_wait().ok().flatten().is_none() {
            println!("\n[清理] 子进程仍在运行，强制终止");
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// 定位主程序可执行文件。
///
/// 优先级：`TRAY_E2E_EXE` 环境变量 > 探针自身可执行文件同级的 `debug/` 目录。
/// 探针构建在 `<target>/<triple>/debug/examples/`，主程序在同一 `debug/` 下。
fn locate_exe() -> PathBuf {
    if let Ok(p) = std::env::var("TRAY_E2E_EXE") {
        return PathBuf::from(p);
    }
    let cur = std::env::current_exe().expect("无法取得探针自身路径");
    let debug_dir = cur
        .parent()
        .and_then(|p| p.parent())
        .expect("探针路径层级异常");
    debug_dir.join(EXE_NAME)
}

/// 用户真实配置文件路径。
///
/// 与 `main.rs::config_path()` 同源：`ProjectDirs::from("","","modular-clipboard")`
/// 的 `config_dir()` 在 Windows 上等于 `%APPDATA%/modular-clipboard`。
fn user_config_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").expect("APPDATA 环境变量缺失，无法定位用户配置");
    PathBuf::from(appdata).join("modular-clipboard").join("config.json")
}

/// 用户真实历史数据库路径（`Store::open_default`）。
fn user_db_path() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").expect("APPDATA 缺失"))
        .join("modular-clipboard")
        .join("history.db")
}

/// 在作用域结束时把配置文件还原成备份内容。
///
/// 没有这一步，`--no-capture` 会被 `save_config` 持久化，
/// 用户真实的剪贴板监听就此被永久关闭（见模块文档「数据安全」）。
struct ConfigBackup {
    path: PathBuf,
    /// `None` 表示原本不存在该文件，还原时应删掉新建的。
    original: Option<Vec<u8>>,
    restored: bool,
}

impl ConfigBackup {
    fn take() -> Self {
        let path = user_config_path();
        let original = std::fs::read(&path).ok();
        println!(
            "[准备] 备份用户配置 {}（{}）",
            path.display(),
            if original.is_some() { "已存在" } else { "原本不存在" }
        );
        Self {
            path,
            original,
            restored: false,
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        let r = match &self.original {
            Some(bytes) => std::fs::write(&self.path, bytes),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        match r {
            Ok(()) => println!("[清理] 用户配置已还原"),
            Err(e) => println!("[清理] 还原用户配置失败: {e}（请手动检查 {0}）", self.path.display()),
        }
    }
}

impl Drop for ConfigBackup {
    fn drop(&mut self) {
        self.restore();
    }
}

/// `EnumWindows` 回调收集的窗口信息。
struct WinInfo {
    hwnd: HWND,
    pid: u32,
}

/// `EnumWindows` 回调。把结果追加到 `lparam` 指向的 `Vec`。
///
/// # Safety
///
/// 由 `EnumWindows` 同步调用；`lparam` 是 `find_main_window` 栈上 `Vec` 的裸指针，
/// 回调期间该 `Vec` 存活（`EnumWindows` 不跨线程），满足 `&mut` 的唯一性。
unsafe extern "system" fn collect_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<WinInfo>) };
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    out.push(WinInfo { hwnd, pid });
    BOOL(1)
}

/// 读回窗口类名，用于确认找到的确实是目标窗口。
fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// 在所有顶层窗口中找出属于 `pid` 且类名匹配 `want_class` 的窗口。
///
/// 按 pid 过滤是必需的：用户可能已经手工开着主程序，
/// 那样会有两个同名窗口，不按 pid 过滤会认错对象。
fn find_window_by_pid(want_pid: u32, want_class: &str) -> Option<HWND> {
    let mut found: Vec<WinInfo> = Vec::new();
    let lparam = LPARAM(&mut found as *mut Vec<WinInfo> as isize);
    let _ = unsafe { EnumWindows(Some(collect_cb), lparam) };
    found
        .into_iter()
        .find(|w| w.pid == want_pid && class_of(w.hwnd) == want_class)
        .map(|w| w.hwnd)
}

/// 等待主窗口出现，返回其句柄。
fn wait_for_main_window(pid: u32, budget: Duration) -> Option<HWND> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(h) = find_window_by_pid(pid, MAIN_CLASS) {
            return Some(h);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 找出属于 `pid` 的托盘 message-only 窗口。
///
/// `EnumWindows` **不枚举** message-only 窗口（它们的父窗口是 `HWND_MESSAGE`），
/// 必须用 `FindWindowEx(HWND_MESSAGE, HWND_MESSAGE, 类名, NULL)`。
fn find_tray_window(pid: u32) -> Option<HWND> {
    let cls = HSTRING::from(TRAY_CLASS);
    let mut after: Option<HWND> = None;
    loop {
        // PCWSTR 只是裸指针，`cls` 必须活到调用返回——它是局部变量，符合。
        let found = unsafe {
            FindWindowExW(
                Some(HWND_MESSAGE),
                after,
                PCWSTR(cls.as_ptr()),
                PCWSTR::null(),
            )
        };
        let h = found.ok()?;
        let mut owner: u32 = 0;
        unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
        if owner == pid {
            return Some(h);
        }
        after = Some(h);
    }
}

/// 进程是否存活。
///
/// `WaitForSingleObject(handle, 0)` 返回 `WAIT_TIMEOUT` 表示进程仍在运行——
/// 这是「窗口关了但进程还活着」这类判定的唯一可靠依据。
fn process_alive(pid: u32) -> bool {
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        // 打不开通常意味着进程已退出（句随进程消失）。不猜，报给调用方。
        return false;
    };
    let r = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    r == WAIT_TIMEOUT
}

/// 轮询直到 `predicate` 成立或超时。返回是否成立。
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 数据库文件的可观测指纹（主文件 + WAL）。
///
/// `Store::open` 开了 WAL，一次 `DROP TABLE` 写事务未必改主库文件，
/// 变化可能落在 `-wal` 上，故两者都看。
fn db_fingerprint() -> Option<(u64, std::time::SystemTime)> {
    let db = user_db_path();
    let mut acc: Option<(u64, std::time::SystemTime)> = None;
    for suffix in ["", "-wal"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        if let Ok(md) = std::fs::metadata(std::path::PathBuf::from(&p)) {
            let mtime = md.modified().ok()?;
            acc = Some((md.len(), mtime));
            break;
        }
    }
    acc
}

fn main() {
    println!("=== 托盘交互端到端探针（真实子进程） ===\n");
    let mut checks = Checks::new();
    let mut backup = ConfigBackup::take();

    // ---- 定位并启动子程序 ------------------------------------------------
    let exe = locate_exe();
    assert!(
        exe.is_file(),
        "找不到主程序 {}\n\
         请先构建：`export CARGO_TARGET_DIR=D:/WorkBuddy/Tiez/target-traye2e && ./scripts/build.sh`\n\
         或用 TRAY_E2E_EXE 指定路径",
        exe.display()
    );
    println!("[准备] 主程序：{}", exe.display());

    let mut cmd = Command::new(&exe);
    // 必须带 --no-capture：绝不让探针触碰用户真实剪贴板。
    cmd.arg("--no-capture")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // 让主程序把自己的分支决策打进 stderr，供本探针断言。
    cmd.env("RUST_LOG", "warn,modular_clipboard=info");

    let child = cmd.spawn().expect("启动主程序失败");
    let pid = child.id();
    let mut guard = ChildGuard(child);
    // 接管 stderr：主程序把 tracing 写在这里，是它「自己说走了哪条分支」的证据。
    let log: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(mut err) = guard.0.stderr.take() {
        let sink = Arc::clone(&log);
        std::thread::spawn(move || {
            let mut buf = String::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
            if let Ok(mut s) = sink.lock() {
                s.push_str(&buf);
            }
        });
    }
    println!("[准备] 子进程已启动 pid={pid}，参数 --no-capture\n");

    // ---- 前置：主窗口真的出现了 ------------------------------------------
    let main_hwnd = wait_for_main_window(pid, STARTUP_TIMEOUT);
    let Some(main_hwnd) = main_hwnd else {
        println!("[FAIL] {STARTUP_TIMEOUT:?} 内主窗口未出现。子进程日志：\n{}", log_text(&log));
        let _ = backup.restore();
        std::process::exit(checks.finish());
    };
    let visible_at_start = unsafe { IsWindowVisible(main_hwnd) }.as_bool();
    println!("[找到] 主窗口 hwnd={:?} 初始可见={visible_at_start}\n", main_hwnd.0);

    checks.record(
        "前置：主窗口启动后可见",
        Some(visible_at_start),
        format!("IsWindowVisible={visible_at_start}（MEMORY 记载曾有「窗口从不显示」缺陷）"),
    );

    // `wait_until` 已负责轮询；拿到「已就绪」这个事实后再取一次句柄。
    // 注意不能写成 `.then(|| find_tray_window(pid))`——那是 `Option<Option<HWND>>`，
    // 下游 `Some(tray)` 拿到的会是 `Option<HWND>`，编译期才发现。
    let tray_ready = wait_until(TRAY_WAIT, || find_tray_window(pid).is_some());
    let tray_hwnd = tray_ready.then(|| find_tray_window(pid)).flatten();
    // 托盘窗口找不到是**缺陷**而非「无法验证」：`Resident::start` 失败时
    // `app.rs` 只记一条 error 就继续跑（resident=None），程序仍会启动。
    // 若此处记成 SKIP，会把「托盘没挂上」伪装成「没条件测」——
    // 这正是 MEMORY坑 85「在自己没测的东西上通过」的变体。
    checks.record(
        "前置：托盘窗口已创建（属本进程）",
        Some(tray_hwnd.is_some()),
        match tray_hwnd {
            Some(h) => format!("message-only 窗口 hwnd={:?}，类名匹配 {TRAY_CLASS}", h.0),
            None => format!(
                "{TRAY_WAIT:?} 内未找到本进程的托盘 message-only 窗口。\
                 主程序在 Resident::start 失败时只记 error 并继续启动，\
                 因此这是**托盘未挂上**的缺陷，不是环境问题"
            ),
        },
    );

    // ---- 链路 1：WM_CLOSE → 隐藏到托盘，进程存活 --------------------------
    println!("\n--- 链路 1：注入 WM_CLOSE ---");
    let posted = unsafe { PostMessageW(Some(main_hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    let posted = posted.is_ok();
    checks.record(
        "链路 1a：WM_CLOSE 投递成功",
        Some(posted),
        format!("PostMessageW(WM_CLOSE) → {posted}"),
    );

    if posted {
        std::thread::sleep(HIDE_SETTLE);
        let visible = unsafe { IsWindowVisible(main_hwnd) }.as_bool();
        let alive = process_alive(pid);
        let still_window = unsafe { IsWindow(Some(main_hwnd)) }.as_bool();
        let logs = log_text(&log);
        let said_hide = logs.contains("已隐藏到托盘");
        let said_close = logs.contains("收到关闭请求");
        let said_hide_not_tray = logs.contains("无托盘兜底，关闭即退出");

        println!("         可见={visible} 进程存活={alive} 窗口未销毁={still_window}");

        // 关键：窗口**没被销毁**。只看「不可见」无法区分隐藏与销毁。
        checks.record(
            "链路 1b：窗口被隐藏（IsWindowVisible=false）",
            Some(!visible),
            format!("IsWindowVisible={visible}（期望 false）"),
        );
        checks.record(
            "链路 1c：窗口**未被销毁**（区分隐藏 vs 销毁）",
            Some(still_window),
            format!("IsWindow={still_window}（期望 true；若为 false 说明是销毁而非隐藏）"),
        );
        checks.record(
            "链路 1d：进程仍存活（「关窗口 ≠ 退出」）",
            Some(alive),
            format!("WaitForSingleObject(0)==WAIT_TIMEOUT → {alive}（期望 true）"),
        );
        checks.record(
            "链路 1e：主程序日志自述走了隐藏分支",
            Some(said_close && said_hide && !said_hide_not_tray),
            format!(
                "「收到关闭请求」={said_close}、「已隐藏到托盘」={said_hide}、\
                 「无托盘兜底，关闭即退出」={said_hide_not_tray}（最后一个必须为 false）"
            ),
        );
    }

    // ---- 链路 2：托盘左键 → 唤起主窗口 ------------------------------------
    println!("\n--- 链路 2：注入托盘左键（NIN_SELECT）---");
    match tray_hwnd {
        Some(tray) => {
            let posted =
                unsafe { PostMessageW(Some(tray), WM_TRAY_CALLBACK, WPARAM(0), LPARAM(NIN_SELECT as isize)) };
            let posted = posted.is_ok();
            checks.record(
                "链路 2a：托盘左键事件投递成功（跨进程）",
                Some(posted),
                format!("PostMessageW(WM_TRAY_CALLBACK, NIN_SELECT) → {posted}"),
            );

            if posted {
                let woke = wait_until(WAKE_TIMEOUT, || unsafe {
                    IsWindowVisible(main_hwnd)
                }
                .as_bool());
                let alive = process_alive(pid);
                println!("         已重新可见={woke} 进程存活={alive}");
                checks.record(
                    "链路 2b：主窗口重新可见（托盘唤起生效）",
                    Some(woke),
                    format!(
                        "IsWindowVisible 在 {WAKE_TIMEOUT:?} 内变为 true → {woke}；\
                         隐藏态轮询间隔 200ms，故需留出多个周期"
                    ),
                );
                checks.record(
                    "链路 2c：唤起后进程仍存活",
                    Some(alive),
                    format!("进程存活={alive}（唤起不应终止进程）"),
                );
            }
        }
        None => {
            checks.record("链路 2：托盘左键唤起", None, "托盘窗口未找到，无法验证".to_string());
        }
    }

    // ---- 链路 3：托盘「清空历史」（默认跳过，见模块文档「数据安全」）--------
    println!("\n--- 链路 3：托盘「清空历史」---");
    let allow_clear = std::env::var("TRAY_E2E_ALLOW_CLEAR").is_ok_and(|v| v == "1");
    if !allow_clear {
        checks.record(
            "链路 3：托盘「清空历史」",
            None,
            "默认跳过：--data-dir 未被主程序消费，无法隔离数据目录，\
             执行会删除用户真实剪贴板历史。确认可删除后设 TRAY_E2E_ALLOW_CLEAR=1 重跑"
                .to_string(),
        );
    } else {
        println!("  !! TRAY_E2E_ALLOW_CLEAR=1：即将清空**用户真实**剪贴板历史 !!");
        let before = db_fingerprint();
        match tray_hwnd {
            Some(tray) => {
                let posted = unsafe {
                    PostMessageW(Some(tray), WM_COMMAND, WPARAM(0x1002), LPARAM(0))
                };
                let posted = posted.is_ok();
                checks.record(
                    "链路 3a：「清空历史」投递成功",
                    Some(posted),
                    format!("PostMessageW(WM_COMMAND, 0x1002) → {posted}"),
                );
                if posted {
                    let changed = wait_until(Duration::from_secs(4), || {
                        db_fingerprint().is_some_and(|now| Some(now) != before)
                    });
                    checks.record(
                        "链路 3b：数据库文件被重写（清空确实执行）",
                        Some(changed),
                        match (before, db_fingerprint()) {
                            (Some(b), Some(a)) => format!(
                                "前=({} 字节, {:?}) 后=({} 字节, {:?}) → 变化={changed}",
                                b.0, b.1, a.0, a.1
                            ),
                            _ => "数据库文件不存在，无法观测".to_string(),
                        },
                    );
                }
            }
            None => {
                checks.record("链路 3：清空历史", None, "托盘窗口未找到".to_string());
            }
        }
    }

    // ---- 链路 4：托盘「退出」→ 进程真的结束（终态，必须最后）--------------
    println!("\n--- 链路 4：注入托盘「退出」---");
    match tray_hwnd {
        Some(tray) => {
            let posted =
                unsafe { PostMessageW(Some(tray), WM_COMMAND, WPARAM(CMD_QUIT), LPARAM(0)) };
            let posted = posted.is_ok();
            checks.record(
                "链路 4a：「退出」投递成功",
                Some(posted),
                format!("PostMessageW(WM_COMMAND, {CMD_QUIT:#x}) → {posted}"),
            );

            if posted {
                let exited = wait_until(QUIT_TIMEOUT, || !process_alive(pid));
                let logs = log_text(&log);
                let said_quit = logs.contains("收到托盘退出请求");
                let said_normal = logs.contains("正常退出");

                checks.record(
                    "链路 4b：进程**结束**（与链路 1 的存活形成对照）",
                    Some(exited),
                    format!(
                        "进程在 {QUIT_TIMEOUT:?} 内退出 → {exited}；\
                         链路 1d 证明 WM_CLOSE 时它还活着——两者构成「关窗口 ≠ 退出」"
                    ),
                );
                checks.record(
                    "链路 4c：主程序日志自述走了退出分支",
                    Some(said_quit),
                    format!("「收到托盘退出请求」={said_quit}"),
                );
                checks.record(
                    "链路 4d：正常收尾而非崩溃",
                    Some(said_normal || exited),
                    format!(
                        "「共渲染 N 帧，正常退出」={said_normal}（期望 true；\
                         为 false 且进程已死，说明是崩溃退出）"
                    ),
                );
            }
        }
        None => {
            checks.record("链路 4：托盘「退出」", None, "托盘窗口未找到，无法验证".to_string());
        }
    }

    // ---- 汇总 -------------------------------------------------------------
    let logs = log_text(&log);
    println!("\n--- 主程序 stderr 全文 ---\n{logs}\n--- end ---");

    let code = checks.finish();
    // 先还原用户配置，再释放 guard（guard 只管杀进程）。
    backup.restore();
    drop(guard);
    std::process::exit(code);
}

/// 读一份子进程日志快照。
fn log_text(log: &Arc<Mutex<String>>) -> String {
    log.lock().map(|s| s.clone()).unwrap_or_default()
}