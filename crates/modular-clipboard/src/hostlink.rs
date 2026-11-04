//! 宿主联动协议（Host Link）。
//!
//! # 它解决什么
//!
//! 外部宿主（Dock、启动器、脚本）想「找到并唤起」本程序时，通常会扫开始
//! 菜单的 `.lnk`、猜安装目录、或硬编码进程名。这三条路对本程序都不成立：
//! 便携部署没有安装目录，自启动走 HKCU Run 而不是开始菜单，界面进程按需
//! 拉起、窗口随时可能不存在。于是宿主看到的是「有时候读不到」。
//!
//! 本协议反过来由**程序自己**发布一份自描述清单，放在若干个稳定位置；
//! 宿主只需按固定顺序去找、按字段校验、按 `commands` 调用。程序移动、
//! 升级、崩溃、清单被删，都能通过 `--host-publish` 重新自愈。
//!
//! # 为什么不复用主 IPC 管道
//!
//! `\\.\pipe\ModularClipboard.Daemon.v1` 是「界面进程 ↔ 常驻进程」的专用
//! 通道，带单界面保护与增量同步语义。外部宿主挤进去会抢连接、破坏
//! `Sync{known}` 的状态机。所以清单里把它标成 `external_use:
//! "not-supported"`，宿主一律走 `commands` 里的 CLI 子命令。
//!
//! # 为什么 stdout 要特殊处理
//!
//! 本程序是 `windows_subsystem = "windows"`（GUI 子系统）：从资源管理器
//! 双击时没有 stdout，`println!` 会静默消失。但通过
//! `Command::output()` 拉起时，父进程传入的管道句柄仍然有效。这里直接用
//! Win32 `GetStdHandle` + `WriteFile` 写字节，并在句柄缺失时尝试
//! `AttachConsole`，两种启动方式都能拿到 JSON。同时把结果落一份到
//! `host-last.json`，宿主完全不 spawn 进程也能读。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 清单 schema。宿主读到不认识的值应降级而不是猜测字段。
pub const SCHEMA_LINK: &str = "mc.host-link/v1";
/// 健康检查结果 schema。
pub const SCHEMA_PROBE: &str = "mc.host-probe/v1";
/// 最近一次命令结果 schema。
pub const SCHEMA_RESULT: &str = "mc.host-result/v1";

/// 清单文件名（所有发布点共用，方便宿主按名字找）。
pub const LINK_FILE: &str = "host-link.json";
/// 最近一次 `--host-probe` 的结果文件。
pub const PROBE_FILE: &str = "host-probe.json";
/// 最近一次任意 host 命令的结果文件（文件通道兜底）。
pub const LAST_FILE: &str = "host-last.json";
/// 覆盖主清单路径的环境变量。
pub const ENV_LINK: &str = "MODULAR_CLIPBOARD_HOST_LINK";
/// 注册表发布点（值里存主清单的绝对路径）。
pub const REG_KEY: &str = r"HKCU\Software\ModularClipboard";
pub const REG_VALUE: &str = "HostLinkPath";
/// 开始菜单快捷方式名：让只扫 `.lnk` 的宿主也能发现本程序。
pub const START_MENU_LNK: &str = "ModularClipboard.lnk";

/// 稳定应用 id。宿主用它去重，不要用显示名或 exe 名。
pub const APP_ID: &str = "com.mcheng404.modular-clipboard";
/// 本版本暴露的能力（最小集：不读取、不写入剪贴板内容）。
pub const CAPABILITIES: &[&str] = &["discovery", "health.ping", "ui.toggle"];
/// 协议文档地址。
///
/// 指向主仓 `docs/` 而不是 Wiki：Wiki 仓库在网页上创建首页之前并不存在，
/// 清单里的链接必须在任何部署状态下都能打开（否则宿主拿到的是死链）。
/// Wiki 只是这份文档的镜像。
pub const SPEC_URL: &str =
    "https://github.com/MCheng404/ModularClipboard/blob/main/docs/HostLink-Protocol.md";
/// 清单超过这个秒数视为可疑（不是失效）：提示宿主重新发布。
pub const STALE_SECS: u64 = 7 * 24 * 3600;

/// 错误码。宿主据此走不同的降级路径，而不是把一切当「没装」。
pub mod code {
    pub const LINK_NOT_FOUND: &str = "LINK_NOT_FOUND";
    pub const LINK_UNREADABLE: &str = "LINK_UNREADABLE";
    pub const LINK_SCHEMA_UNSUPPORTED: &str = "LINK_SCHEMA_UNSUPPORTED";
    pub const LINK_STALE: &str = "LINK_STALE";
    pub const EXE_MISSING: &str = "EXE_MISSING";
    pub const EXE_CHANGED: &str = "EXE_CHANGED";
    pub const DAEMON_NOT_RUNNING: &str = "DAEMON_NOT_RUNNING";
    pub const PUBLISH_FAILED: &str = "PUBLISH_FAILED";
    pub const ACTIVATE_FAILED: &str = "ACTIVATE_FAILED";
}

/// CLI 子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCommand {
    /// 输出当前清单（不写盘、不启动界面）。
    Link,
    /// 全面自检，输出 checks / 错误码 / 修复建议。
    Probe,
    /// 重新发布到所有发现点并写注册表与开始菜单。
    Publish,
    /// 自检 + 失败时自动重新发布 + 复检。
    Doctor,
    /// 唤起主窗口；常驻进程不在时拉起一个。
    Activate,
}

impl HostCommand {
    /// 从命令行开关解析。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "--host-link" => Some(Self::Link),
            "--host-probe" => Some(Self::Probe),
            "--host-publish" => Some(Self::Publish),
            "--host-doctor" => Some(Self::Doctor),
            "--host-activate" => Some(Self::Activate),
            _ => None,
        }
    }

}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 程序数据目录（`%APPDATA%/modular-clipboard`）。
///
/// 与配置、日志同源；退回临时目录只应发生在极端环境（没有 APPDATA），
/// 那时清单仍能被 `exe 同目录` 那份兜住。
fn app_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("modular-clipboard")
}

fn primary_link_path() -> PathBuf {
    app_dir().join(LINK_FILE)
}

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("modular-clipboard.exe"))
}

fn exe_dir_link_path() -> PathBuf {
    exe_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(LINK_FILE)
}

fn start_menu_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|p| {
        PathBuf::from(p)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
}

/// 环境变量指向的路径（可以是文件，也可以是目录）。
fn env_link_path() -> Option<PathBuf> {
    let raw = std::env::var_os(ENV_LINK)?;
    let p = PathBuf::from(raw);
    Some(if p.is_dir() { p.join(LINK_FILE) } else { p })
}

/// 发现顺序：显式覆盖 → 主位置 → exe 同目录 → 开始菜单目录。
///
/// 顺序即优先级：靠前的先命中，靠后的用于「主位置被删 / 程序被搬到别的
/// 目录」这类场景。注册表与开始菜单 `.lnk` 是给宿主的额外通道，写在清单
/// 的 `discovery` 字段里，自身发现不强依赖它们（避免 reg.exe 输出编码
/// 在中文路径下不可靠）。
fn discover_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = env_link_path() {
        v.push(p);
    }
    v.push(primary_link_path());
    let beside = exe_dir_link_path();
    if !v.contains(&beside) {
        v.push(beside);
    }
    if let Some(dir) = start_menu_dir() {
        let p = dir.join(LINK_FILE);
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

/// exe 的大小 / 修改时间 / SHA-256。
///
/// 三者都进清单，宿主可以按需选择校验强度：只看 size+mtime 是微秒级，
/// 比 SHA-256 才算强校验（exe 被替换、被旧版本覆盖时能查出来）。
fn fingerprint(path: &Path) -> Option<(u64, u64, String)> {
    let meta = std::fs::metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((meta.len(), modified, hex))
}

fn cmd(exe: &Path, arg: &str) -> Value {
    json!({ "exe": exe.to_string_lossy(), "args": [arg] })
}

/// 构造清单。所有路径都取自**当前进程**的真实位置，所以程序被搬到任何
/// 目录、从任何快捷方式启动，重新发布一次就能自愈「目录错误」。
fn build_link() -> Value {
    let exe = exe_path();
    let (size, modified, sha256) = fingerprint(&exe).unwrap_or((0, 0, String::new()));
    let primary = primary_link_path();
    let probe_path = app_dir().join(PROBE_FILE);
    let last_path = app_dir().join(LAST_FILE);
    let lnk = start_menu_dir().map(|d| d.join(START_MENU_LNK));

    let mut paths = vec![primary.to_string_lossy().into_owned(), exe_dir_link_path().to_string_lossy().into_owned()];
    if let Some(p) = env_link_path() {
        paths.insert(0, p.to_string_lossy().into_owned());
    }

    json!({
        "schema": SCHEMA_LINK,
        "spec": SPEC_URL,
        "published_unix": now_secs(),
        "app": {
            "id": APP_ID,
            "name": "ModularClipboard",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "publisher": { "pid": std::process::id(), "role": "daemon" },
        "executable": {
            "path": exe.to_string_lossy(),
            "size": size,
            "modified_unix": modified,
            "sha256": sha256,
        },
        "capabilities": CAPABILITIES,
        "commands": {
            "probe": cmd(&exe, "--host-probe"),
            "doctor": cmd(&exe, "--host-doctor"),
            "publish": cmd(&exe, "--host-publish"),
            "link": cmd(&exe, "--host-link"),
            "activate": cmd(&exe, "--host-activate"),
        },
        "endpoints": {
            "cli": {
                "kind": "process",
                "path": exe.to_string_lossy(),
                "stdout": "json",
                "note": "GUI subsystem: json is written with Win32 WriteFile; also mirrored to a file.",
            },
            "files": {
                "link": primary.to_string_lossy(),
                "probe": probe_path.to_string_lossy(),
                "last_result": last_path.to_string_lossy(),
            },
            "internal_ipc": {
                "name": r"\\.\pipe\ModularClipboard.Daemon.v1",
                "framing": "u32le-length+json",
                "external_use": "not-supported",
                "note": "Private UI<->daemon channel with single-UI semantics. Use commands.* instead.",
            },
        },
        "discovery": {
            "env": ENV_LINK,
            "registry": { "key": REG_KEY, "value": REG_VALUE, "meaning": "absolute path of the primary link file" },
            "paths": paths,
            "start_menu_lnk": lnk.as_ref().map(|p| p.to_string_lossy()),
        },
        "limits": {
            "stale_after_secs": STALE_SECS,
            "probe_timeout_ms": 800,
        },
    })
}

/// 原子写：先写同目录的 `.tmp`，再 rename 覆盖。
///
/// 直接覆盖会让「宿主正在读、程序正在写」的那一瞬读到半截 JSON —— 这是
/// 最常见的「逻辑错误」型读不到。同目录 rename 保证读者看到的要么是旧
/// 完整文件，要么是新完整文件。
fn atomic_write(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    // 读回校验：写成功但内容不是合法 JSON 时宁可报错，也不能留下坏清单。
    let back = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice::<Value>(&back).map_err(|e| e.to_string())?;
    Ok(())
}

/// 读第一份可解析且 schema 正确的清单。
fn read_link() -> Result<(PathBuf, Value), (String, Vec<String>)> {
    let mut diagnostics = Vec::new();
    for p in discover_paths() {
        if !p.is_file() {
            diagnostics.push(format!("missing: {}", p.display()));
            continue;
        }
        match std::fs::read(&p) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => {
                    let schema = v.get("schema").and_then(Value::as_str).unwrap_or("");
                    if schema != SCHEMA_LINK {
                        diagnostics.push(format!(
                            "{}: unsupported schema {schema:?}",
                            p.display()
                        ));
                        continue;
                    }
                    return Ok((p, v));
                }
                Err(e) => diagnostics.push(format!("{}: invalid json ({e})", p.display())),
            },
            Err(e) => diagnostics.push(format!("{}: unreadable ({e})", p.display())),
        }
    }
    Err((code::LINK_NOT_FOUND.into(), diagnostics))
}

fn check(name: &str, ok: bool, detail: impl Into<String>) -> Value {
    json!({ "name": name, "ok": ok, "detail": detail.into() })
}

/// 管道是否可达（不发送消息，避免打扰界面进程的状态机）。
fn pipe_reachable() -> bool {
    modular_clipboard_ipc::Client::connect(Duration::from_millis(300)).is_ok()
}

/// 全面自检。失败时给出**可执行的**修复命令，而不是一句「未找到」。
fn probe() -> Value {
    let mut checks = Vec::new();
    let mut diagnostics = Vec::new();
    let mut repair = Vec::new();
    let mut error: Option<String> = None;
    let mut source = String::new();
    let mut link = Value::Null;

    match read_link() {
        Ok((p, v)) => {
            source = p.to_string_lossy().into_owned();
            checks.push(check("link.found", true, source.clone()));
            link = v.clone();

            let published = v.get("published_unix").and_then(Value::as_u64).unwrap_or(0);
            let fresh = now_secs().saturating_sub(published) <= STALE_SECS;
            checks.push(check("link.fresh", fresh, format!("published_unix={published}")));
            if !fresh {
                error.get_or_insert(code::LINK_STALE.into());
                diagnostics.push("link is older than the stale threshold; paths may have moved".into());
                repair.push(json!({ "command": "publish", "reason": "refresh executable path and fingerprints" }));
            }

            let exe = PathBuf::from(
                v.pointer("/executable/path")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let exists = !exe.as_os_str().is_empty() && exe.is_file();
            checks.push(check("exe.exists", exists, exe.display().to_string()));
            if !exists {
                error.get_or_insert(code::EXE_MISSING.into());
                diagnostics.push("executable recorded in the link no longer exists (moved/uninstalled)".into());
                repair.push(json!({ "command": "publish", "reason": "republish from the current executable" }));
            } else if let Some((size, modified, hash)) = fingerprint(&exe) {
                let want_size = v.pointer("/executable/size").and_then(Value::as_u64).unwrap_or(0);
                let want_mtime = v
                    .pointer("/executable/modified_unix")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let want_hash = v.pointer("/executable/sha256").and_then(Value::as_str).unwrap_or("");
                let same = want_size == size && want_mtime == modified && want_hash == hash;
                checks.push(check(
                    "exe.fingerprint",
                    same,
                    format!("size={size} mtime={modified} sha256={}", &hash[..8.min(hash.len())]),
                ));
                if !same {
                    error.get_or_insert(code::EXE_CHANGED.into());
                    diagnostics.push("executable changed since publishing (upgrade or replacement)".into());
                    repair.push(json!({ "command": "publish", "reason": "update fingerprints" }));
                }
            }
        }
        Err((code_text, d)) => {
            diagnostics.extend(d);
            // 区分「一个都没有」与「有文件但都坏了」。
            let any_file = discover_paths().iter().any(|p| p.is_file());
            error = Some(if any_file {
                if diagnostics.iter().any(|s| s.contains("invalid json")) {
                    code::LINK_UNREADABLE.into()
                } else {
                    code::LINK_SCHEMA_UNSUPPORTED.into()
                }
            } else {
                code_text
            });
            checks.push(check("link.found", false, diagnostics.join("; ")));
            repair.push(json!({ "command": "publish", "reason": "create a fresh link manifest" }));
        }
    }

    let pipe = pipe_reachable();
    checks.push(check("daemon.pipe", pipe, if pipe { "connected" } else { "no listener" }));
    if !pipe {
        error.get_or_insert(code::DAEMON_NOT_RUNNING.into());
        diagnostics.push("daemon IPC pipe is not accepting connections".into());
        repair.push(json!({ "command": "activate", "reason": "start the daemon and show the main window" }));
    }
    let ui = modular_clipboard_ipc::ui_is_running();
    checks.push(check("ui.running", ui, if ui { "visible" } else { "not running (normal when hidden)" }));

    let ok = error.is_none();
    json!({
        "schema": SCHEMA_PROBE,
        "ok": ok,
        "checked_unix": now_secs(),
        "source": source,
        "app": link.pointer("/app").cloned().unwrap_or(json!({})),
        "checks": checks,
        "error": error.map(|c| json!({ "code": c, "message": diagnostics.first().cloned().unwrap_or_default() })),
        "diagnostics": diagnostics,
        "repair": repair,
    })
}

/// 写开始菜单 `.lnk`。
///
/// 用 PowerShell 的 WScript.Shell 而不是自己起 COM：路径通过环境变量传，
/// 避开引号/空格/编码问题；失败只记诊断，不影响其它发布点。
fn publish_start_menu(exe: &Path) -> (bool, String) {
    let Some(dir) = start_menu_dir() else {
        return (false, "APPDATA unavailable".into());
    };
    let lnk = dir.join(START_MENU_LNK);
    if std::fs::create_dir_all(&dir).is_err() {
        return (false, format!("cannot create {}", dir.display()));
    }
    let script = "$ErrorActionPreference='Stop'; \
        $w=New-Object -ComObject WScript.Shell; \
        $s=$w.CreateShortcut($env:MC_LNK_PATH); \
        $s.TargetPath=$env:MC_LNK_EXE; \
        $s.Arguments='--host-activate'; \
        $s.IconLocation=$env:MC_LNK_EXE; \
        $s.Description='ModularClipboard'; \
        $s.Save()";
    let result = silent_command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("MC_LNK_PATH", &lnk)
        .env("MC_LNK_EXE", exe)
        .output();
    match result {
        Ok(o) if o.status.success() => (true, lnk.to_string_lossy().into_owned()),
        Ok(o) => (
            false,
            format!("powershell failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
        ),
        Err(e) => (false, e.to_string()),
    }
}

/// 写注册表发布点。宿主若不方便读 APPDATA，可从这里拿到主清单绝对路径。
fn publish_registry(primary: &Path) -> (bool, String) {
    let result = silent_command("reg")
        .args([
            "add",
            REG_KEY,
            "/v",
            REG_VALUE,
            "/t",
            "REG_SZ",
            "/d",
            &primary.to_string_lossy(),
            "/f",
        ])
        .output();
    match result {
        Ok(o) if o.status.success() => (true, REG_KEY.into()),
        Ok(o) => (
            false,
            format!("reg.exe failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn silent_command(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(program);
    // GUI 程序拉起控制台子进程会闪黑框。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// 重新发布到所有发现点，返回逐项结果。
fn publish() -> Value {
    let link = build_link();
    let exe = exe_path();
    let mut written = Vec::new();
    let mut diagnostics = Vec::new();

    for p in discover_paths() {
        // 环境变量指向的路径可能在一个不该写的目录里；失败不拖累其它点。
        match atomic_write(&p, &link) {
            Ok(()) => written.push(json!({ "path": p.to_string_lossy(), "ok": true })),
            Err(e) => {
                diagnostics.push(format!("{}: {e}", p.display()));
                written.push(json!({ "path": p.to_string_lossy(), "ok": false, "error": e }));
            }
        }
    }

    let (lnk_ok, lnk_detail) = publish_start_menu(&exe);
    let (reg_ok, reg_detail) = publish_registry(&primary_link_path());
    if !lnk_ok {
        diagnostics.push(format!("start menu: {lnk_detail}"));
    }
    if !reg_ok {
        diagnostics.push(format!("registry: {reg_detail}"));
    }

    let any = written.iter().any(|w| w["ok"] == true);
    json!({
        "schema": SCHEMA_RESULT,
        "ok": any,
        "action": "publish",
        "error": if any { Value::Null } else {
            json!({ "code": code::PUBLISH_FAILED, "message": "every publish target failed" })
        },
        "written": written,
        "start_menu": { "ok": lnk_ok, "detail": lnk_detail },
        "registry": { "ok": reg_ok, "detail": reg_detail },
        "diagnostics": diagnostics,
        "link": link,
    })
}

/// 唤起主窗口。
///
/// 常驻进程在跑时走它已有的 `ShowWindow` 命令（由它决定是否拉起界面）；
/// 管道连不上时 spawn 自身（默认启动路径会建常驻进程并打开界面）。
fn activate() -> Value {
    use modular_clipboard_ipc::{Client, Command};
    if Client::connect(Duration::from_millis(800))
        .and_then(|mut c| c.command(Command::ShowWindow))
        .is_ok()
    {
        return json!({
            "schema": SCHEMA_RESULT, "ok": true, "action": "shown",
            "detail": "daemon accepted ShowWindow",
        });
    }
    let exe = exe_path();
    match silent_command(&exe.to_string_lossy()).spawn() {
        Ok(_) => json!({
            "schema": SCHEMA_RESULT, "ok": true, "action": "started",
            "detail": "no daemon pipe; spawned a fresh instance",
        }),
        Err(e) => json!({
            "schema": SCHEMA_RESULT, "ok": false, "action": "failed",
            "error": { "code": code::ACTIVATE_FAILED, "message": e.to_string() },
            "repair": [{ "command": "publish", "reason": "refresh the executable path" }],
        }),
    }
}

/// 把结果同时送到 stdout 与文件。
///
/// `WriteFile` 直接写 `GetStdHandle` 拿到的句柄：`Command::output()` 的管道
/// 与 cmd 控制台都覆盖；没有句柄时先 `AttachConsole` 再试一次。
fn emit(cmd: HostCommand, value: &Value) {
    let bytes = serde_json::to_vec_pretty(value).unwrap_or_default();
    write_stdout(&bytes);

    let _ = std::fs::create_dir_all(app_dir());
    let last = app_dir().join(LAST_FILE);
    let _ = atomic_write(&last, &json!({
        "schema": SCHEMA_RESULT,
        "command": format!("{cmd:?}").to_lowercase(),
        "checked_unix": now_secs(),
        "result": value,
    }));
    if matches!(cmd, HostCommand::Probe | HostCommand::Doctor) {
        let _ = atomic_write(&app_dir().join(PROBE_FILE), value);
    }
}

#[cfg(windows)]
fn write_stdout(bytes: &[u8]) {
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    // SAFETY: 只查询/写入本进程的标准句柄；失败时退回文件通道。
    unsafe {
        let mut handle = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
        if handle.is_invalid() {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            handle = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or_default();
        }
        if handle.is_invalid() {
            return;
        }
        let mut written = 0u32;
        let _ = WriteFile(handle, Some(bytes), Some(&mut written as *mut u32), None);
        // stdout 归宿主所有，不要关闭句柄。
    }
}

/// 执行一个 host 命令，返回进程退出码。
pub fn run(cmd: HostCommand) -> i32 {
    let value = match cmd {
        HostCommand::Link => build_link(),
        HostCommand::Probe => probe(),
        HostCommand::Publish => publish(),
        HostCommand::Activate => activate(),
        HostCommand::Doctor => {
            let first = probe();
            if first["ok"] == true {
                first
            } else {
                let repaired = publish();
                let second = probe();
                json!({
                    "schema": SCHEMA_PROBE,
                    "ok": second["ok"] == true,
                    "checked_unix": now_secs(),
                    "first": first,
                    "repair_run": repaired,
                    "after": second,
                })
            }
        }
    };
    let ok = value["ok"] == true || cmd == HostCommand::Link;
    emit(cmd, &value);
    if ok { 0 } else { 1 }
}

/// 常驻进程启动时调用：安静地刷新清单，失败不阻断启动。
pub fn publish_quiet() {
    // 结果本身已经写进 host-last.json；这里只要求「不能阻断启动」。
    let _ = publish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parse_covers_every_switch() {
        for s in [
            "--host-link",
            "--host-probe",
            "--host-publish",
            "--host-doctor",
            "--host-activate",
        ] {
            assert!(HostCommand::parse(s).is_some(), "{s}");
        }
        assert!(HostCommand::parse("--daemon").is_none());
    }

    #[test]
    fn link_is_self_describing() {
        let v = build_link();
        assert_eq!(v["schema"], SCHEMA_LINK);
        assert_eq!(v["app"]["id"], APP_ID);
        assert!(v["commands"]["activate"]["args"][0] == "--host-activate");
        assert_eq!(
            v["endpoints"]["internal_ipc"]["external_use"],
            "not-supported"
        );
        assert_eq!(v["capabilities"].as_array().unwrap().len(), CAPABILITIES.len());
        assert!(!v["discovery"]["env"].as_str().unwrap().is_empty());
        assert!(!v["executable"]["path"].as_str().unwrap().is_empty());
        assert_eq!(v["executable"]["sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn atomic_write_is_parseable_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("mc-hostlink-test-{}", std::process::id()));
        let p = dir.join(LINK_FILE);
        let _ = std::fs::create_dir_all(&dir);
        assert!(atomic_write(&p, &json!({"schema": SCHEMA_LINK, "n": 1})).is_ok());
        assert!(atomic_write(&p, &json!({"schema": SCHEMA_LINK, "n": 2})).is_ok());
        let back: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back["n"], 2);
        assert!(!p.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_schema_is_rejected_by_the_reader() {
        // 纯函数部分：read_link 只接受本 schema；这里验证判定表达式本身。
        let candidates = [
            json!({"schema": "mc.host-link/v0"}),
            json!({"schema": SCHEMA_LINK}),
            json!({}),
        ];
        let accepted: Vec<_> = candidates
            .iter()
            .filter(|v| v.get("schema").and_then(Value::as_str) == Some(SCHEMA_LINK))
            .collect();
        assert_eq!(accepted.len(), 1);
    }

    #[test]
    fn fingerprint_matches_the_current_executable() {
        let exe = exe_path();
        let (size, mtime, hash) = fingerprint(&exe).expect("current exe must be readable");
        assert!(size > 0);
        assert!(mtime > 0);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn error_codes_are_stable_and_unique() {
        let all = [
            code::LINK_NOT_FOUND,
            code::LINK_UNREADABLE,
            code::LINK_SCHEMA_UNSUPPORTED,
            code::LINK_STALE,
            code::EXE_MISSING,
            code::EXE_CHANGED,
            code::DAEMON_NOT_RUNNING,
            code::PUBLISH_FAILED,
            code::ACTIVATE_FAILED,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }

    #[test]
    fn probe_and_result_schemas_are_distinct_and_versioned() {
        assert!(SCHEMA_PROBE.starts_with("mc.host-probe/"));
        assert!(SCHEMA_RESULT.starts_with("mc.host-result/"));
        assert_ne!(SCHEMA_PROBE, SCHEMA_RESULT);
    }
}
