//! 「关于」页的运行时事实：版本号、开源协议正文、版本检查。
//!
//! 这三件事都**不是**静态表能表达的，所以从 `paint.rs` 的 `SETTINGS_CARDS`
//! 拆出来：
//!
//! - **版本号**要动态取（cl 版 = 编译时的 git commit，稳定版 = `0.2`），
//!   静态表里写死 `concat!("v", CARGO_PKG_VERSION)` 会让开发构建也显示
//!   成一个发不出去的版本号；
//! - **协议正文**是整篇 GPL（30 多 KB），塞进设置项会让静态表暴涨；
//! - **版本检查**要拉远程 git，必须能在后台跑，不能挂在绘制路径上。
//!
//! # 为什么版本检查用静态状态而不是 Op
//!
//! 它是**纯 UI 侧的只读信息**：不改配置、不改存储、不跨窗口同步，只是
//! 把一次后台探测的结果显示出来。走 `Op` 就得给 `App` 加一个状态字段、
//! 一条 IPC/状态机通路，而它跟剪贴板状态机毫无关系（见 MEMORY「跨窗口
//! 共享状态铁律」：那条是给**共享状态**用的，这里没有共享状态）。
//!
//! 用 `OnceLock<Arc<Mutex<_>>>` 全局单例：后台线程写、每帧读，所有权
//! 很清楚，也不需要任何模块传递。

use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

/// 本项目仓库（版本检查的拉取源）。
pub const REPO_URL: &str = "https://github.com/MCheng404/ModularClipboard";
/// 仓库的 git 远程地址（`git ls-remote` 用）。
pub const REPO_GIT: &str = "https://github.com/MCheng404/ModularClipboard.git";

/// 稳定版号。
///
/// 用户口径：稳定版为 `0.2`。开发构建走 [`version_label`] 的 cl 分支，
/// 这个号只在「拿不到 git 信息」时显示（发布构建、源码包构建）。
pub const STABLE_VERSION: &str = "0.2.1";

/// 版本标签，两条分支：
///
/// | 构建情况 | 显示 |
/// |---|---|
/// | 有 git commit（开发构建） | `cl-a1b2c3d` |
/// | 无 git 信息（发布/源码包） | `0.2` |
///
/// # 为什么是 `String` 而不是 `&'static str`
///
/// commit 短 hash 是编译期注入的 `&'static str`（`env!`），但两条分支
/// 的拼接结果不同长度，没法直接返回同一个静态引用。设置项的
/// 设置项的种类（`settings_items::ItemKind`）也为此改成了动态行，
/// 而不是静态的 `Info`。
pub fn version_label() -> String {
    let hash = env!("GIT_SHORT_HASH");
    if hash.trim().is_empty() {
        STABLE_VERSION.to_string()
    } else {
        format!("cl-{hash}")
    }
}

/// 本次构建是否是开发版（能拿到 git commit）。
pub fn is_dev_build() -> bool {
    !env!("GIT_SHORT_HASH").trim().is_empty()
}

/// 开源协议全文（GPL-3.0，见仓库根 `LICENSE`）。
///
/// 用 `include_str!` 编译期纳入：协议正文必须**随 exe 一起**可读，
/// 运行时去读文件会遇到「安装目录里没有 LICENSE」——用户点「查看协议」
/// 却打不开，是最典型的「本该有的东西缺了」。
pub const LICENSE_TEXT: &str = include_str!("../../../LICENSE");

/// 技术栈词条（显示文字 + GitHub 仓库 / 官方主页链接）。
///
/// 「技术栈文字可点击（含 Github 仓库超链接）」：每一段都是一个链接，
/// 点击用默认浏览器打开。顺序即界面上的排列顺序（与 `·` 分隔的读法一致）。
pub const STACK_LINKS: &[(&str, &str)] = &[
    ("Rust", "https://www.rust-lang.org/"),
    ("egui", "https://github.com/emilk/egui"),
    ("Vulkan (Win32)", "https://www.khronos.org/vulkan/"),
    ("windows-rs", "https://github.com/microsoft/windows-rs"),
    ("rusqlite", "https://github.com/rusqlite/rusqlite"),
    ("image", "https://github.com/image-rs/image"),
];

/// 版本检查的状态。
///
/// `Idle` 是 `Default`（派生即可，无需手写 impl）。
#[derive(Debug, Clone, Default)]
pub enum UpdateState {
    /// 还没检查过。
    #[default]
    Idle,
    /// 检查中（后台线程在跑 `git ls-remote`）。
    Checking,
    /// 本地已是最新（或远程没有可比的 tag）。
    UpToDate {
        /// 远程最高 tag（若有）。
        remote: String,
    },
    /// 远程有更新的 tag。
    Outdated {
        local: String,
        remote: String,
        /// release 详情（更新内容预览 + 下载页）。
        ///
        /// `None` = 拉不到（网络不通 / curl 不可用 / 仓库还没有 release），
        /// 界面降级成「只显示版本号 + 打开仓库」，不假装有更新说明。
        release: Option<ReleaseInfo>,
    },
    /// 检查失败（网络/ git 不可用），带原因。
    Failed(String),
    /// 下载新版本中（`curl` 在跑）。
    Downloading,
    /// 已下载并启动替换脚本，**即将退出当前进程**。
    ///
    /// UI 检测到这个状态就 push `Op::Quit`：替换脚本在等主进程退出后
    /// 才动 exe（Windows 对运行中的文件有锁），两边必须接力。
    Applying,
}



/// 全局检查状态（后台线程写、每帧读）。
///
/// `OnceLock` 只初始化一次：`StartCheck` 里会复用同一个 `Arc`，线程
/// 拿的是克隆，所以状态跨线程共享、生命周期跟进程一致。
fn shared_state() -> &'static Arc<Mutex<UpdateState>> {
    static STATE: OnceLock<Arc<Mutex<UpdateState>>> = OnceLock::new();
    STATE.get_or_init(|| Arc::new(Mutex::new(UpdateState::default())))
}

/// 当前检查状态的快照。
pub fn current_state() -> UpdateState {
    shared_state().lock().map(|g| g.clone()).unwrap_or_default()
}

/// 启动一次版本检查（幂等：已在检查中则直接返回）。
///
/// 后台线程跑 `git ls-remote --tags --refs`，取最高的语义版本号与本地
/// 比较。**不阻塞 UI**：点击后立刻返回，结果由 `current_state()` 在
/// 后续帧里读到。
pub fn start_check() {
    let st = shared_state();
    {
        let Ok(mut g) = st.lock() else { return };
        if matches!(*g, UpdateState::Checking) {
            return; // 已在跑，重复点击不重发
        }
        *g = UpdateState::Checking;
    }
    let st = Arc::clone(st);
    std::thread::spawn(move || {
        let result = fetch_latest_tag();
        if let Ok(mut g) = st.lock() {
            *g = result;
        }
    });
}

/// 构造一个 `git` 命令，并**抑制控制台窗口**。
///
/// # 为什么必须带 `CREATE_NO_WINDOW`
///
/// 本程序是 GUI 子系统（`windows_subsystem = "windows"`），它自己没有
/// 控制台。而 Windows 在 GUI 进程启动**控制台子程序**时会新建一个
/// 控制台窗口——用户点「检查更新」时屏幕上会**弹出一个黑色 cmd 窗口**
/// 一闪而过（2026-10-22 用户反馈），非常突兀，而且脚本/杀软扫描
/// 很容易把它当成异常行为。
///
/// `CREATE_NO_WINDOW` 让子进程照常运行但不给它控制台窗口，是这类
/// 场景的标准做法。
///
/// # 为什么不用 `DETACHED_PROCESS`
///
/// 那个标志的语义是「让子进程**放弃**父进程的控制台」，只有在父进程
/// 确实有控制台时才合理。GUI 程序用它会出现「子进程既没有窗口也没有
/// 控制台」的中间态，输出捕获在某些情况下会拿不到。
fn quiet_command(prog: &str) -> Command {
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// 拉一次远程 tag 并与本地比较。
///
/// 单独成函数是为了能被单测用假数据复用逻辑（`compare_tags`），
/// 真正走 git 的只留这一层。
fn fetch_latest_tag() -> UpdateState {
    // `--tags --refs`：只取 tag 引用本身，不取它们指向的 commit，
    // 输出少得多（几十行 vs 几百行）。
    let out = quiet_command("git")
        .args(["ls-remote", "--tags", "--refs", REPO_GIT])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return UpdateState::Failed(format!("git 不可用：{e}")),
    };
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        return UpdateState::Failed(if msg.is_empty() {
            "git ls-remote 失败".to_string()
        } else {
            msg.to_string()
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let remote = best_tag(&text);
    let mut st = compare_tags(&version_label(), remote);
    // 判定为「有更新」时再补拉 release 详情（更新内容 + 下载页）。
    //
    // 分两步而不是一次拉完：`compare_tags` 是**纯函数**（单测直接喂
    // 假数据覆盖各种新旧组合），把网络塞进来就没法测了。而详情只在
    // 真的有更新时才用得上，等于给绝大多数检查省了一次 HTTP 请求。
    if let UpdateState::Outdated { release, .. } = &mut st
        && let Some(info) = fetch_release_info()
    {
        *release = Some(info);
    }
    st
}

/// 一次 release 的元信息（来自 GitHub releases API）。
#[derive(Debug, Clone, Default)]
pub struct ReleaseInfo {
    /// tag 名（如 `v0.2`）。
    pub tag: String,
    /// 更新内容预览（release body，Markdown 源文）。
    pub notes: String,
    /// 下载页 URL（`html_url`）。
    pub url: String,
    /// 可直接下载的 exe 地址（`assets[].browser_download_url`）。
    ///
    /// `None` = 这次 release 没挂 exe 资产（只发了源码包），
    /// 自更新降级为「打开下载页」。**不能**假设一定有——用户
    /// 把仓库 release 只放源码 zip 是完全合法的发布方式。
    pub download_url: Option<String>,
}

/// 拉 GitHub `releases/latest`，拿更新内容与下载页。
///
/// # 为什么用 `curl` 而不是引 HTTP 客户端
///
/// 只为三个字符串字段引一个 HTTP + TLS 栈（ureq/reqwest + rustls/
/// native-tls）会让依赖面和二进制体积都涨一大截，而 Windows 10 1803+
/// 内置 `curl.exe`，足以覆盖本项目的目标平台。拿不到就降级——
/// 界面照样能用（只显示版本号 + 打开仓库），不会因为网络层不可用
/// 而让「检查更新」整个坏掉。
///
/// # User-Agent 必须给
///
/// GitHub API 对缺失/空 User-Agent 返回 **403**，而响应体是一段
/// JSON 错误消息——直接解析会拿到一堆 `null`，现象是「更新说明永远
/// 是空的」且看不出原因。这里显式带上标识。
fn fetch_release_info() -> Option<ReleaseInfo> {
    let out = quiet_command("curl")
        .args([
            "-sL",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: ModularClipboard",
            "https://api.github.com/repos/MCheng404/ModularClipboard/releases/latest",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    if v.get("message").is_some_and(|m| m.as_str().is_some()) {
        // GitHub 把错误也用 200 外的码返回，但 curl 不解析状态码；
        // 有 `message` 字段即视为错误体（如 `Not Found` = 没有 release）。
        return None;
    }
    // 在 assets 里找可直接下载的 exe：优先名字带项目名的，退而求其次
    // 取第一个 `.exe`。跳过 `.d`/`.pdb` 这类调试附属物（它们也常带
    // `.exe` 之外的后缀，但 `.exe.d` 这种边角情况用 ends_with 兜不住，
    // 所以额外排除）。
    let download_url = v.get("assets").and_then(|a| a.as_array()).and_then(|arr| {
        let mut exact: Option<String> = None;
        let mut any_exe: Option<String> = None;
        for item in arr {
            let name = item.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let url = item
                .get("browser_download_url")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if url.is_empty() || !name.ends_with(".exe") || name.ends_with(".exe.d") {
                continue;
            }
            if exact.is_none() && name.contains("modular-clipboard") {
                exact = Some(url.to_string());
            }
            if any_exe.is_none() {
                any_exe = Some(url.to_string());
            }
        }
        exact.or(any_exe)
    });
    Some(ReleaseInfo {
        tag: v.get("tag_name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        notes: v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        url: v
            .get("html_url")
            .and_then(|x| x.as_str())
            .unwrap_or(REPO_URL)
            .to_string(),
        download_url,
    })
}

/// 从 `git ls-remote --tags --refs` 的输出里挑出最高的版本号。
///
/// 输出形如：
/// ```text
/// 6d1f0a2\trefs/tags/v0.1
/// 8c3b1e4\trefs/tags/v0.2
/// ```
///
/// 只认 `v?<maj>.<min>[.<patch>]` 形态，其余（`refs/tags/beta1` 等）跳过
/// ——拿一个非语义标签当「最新版本」会把用户引到错误的结论。
pub fn best_tag(ls_remote_output: &str) -> Option<String> {
    let mut best: Option<(u64, u64, u64, String)> = None;
    for line in ls_remote_output.lines() {
        let Some(pos) = line.rfind("refs/tags/") else { continue };
        let tag = line[pos + "refs/tags/".len()..].trim();
        let Some((maj, min, pat)) = parse_semver(tag) else {
            continue;
        };
        let key = (maj, min, pat);
        if best.as_ref().is_none_or(|(a, b, c, _)| key > (*a, *b, *c)) {
            best = Some((maj, min, pat, tag.trim_start_matches('v').to_string()));
        }
    }
    best.map(|(_, _, _, t)| t)
}

/// 解析 `v0.2` / `1.2.3` 形态为三段数字；解析失败返回 `None`。
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut it = s.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    let pat = match it.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    // 后面不能再有段（`0.2.1.2` 不是三段语义版本）。
    if it.next().is_some() {
        return None;
    }
    Some((maj, min, pat))
}

/// 比较本地版本与远程最高 tag。
///
/// # 本地是 cl 版时的语义
///
/// cl 版是**开发构建**，不参与「有没有更新」的判断（它没有可比的
/// 版本号）——但仍然把远程最新 tag 告诉用户，让 ta 知道上游走到哪了。
/// 若因为「cl 版永远落后于 tag」就永远报「有更新」，那个提示会变成
/// 噪音，反而没人看。
pub fn compare_tags(local: &str, remote: Option<String>) -> UpdateState {
    let Some(remote) = remote else {
        // 远程没有可比的 tag：说「已是最新」是诚实的，不假装有版本信息。
        return UpdateState::UpToDate {
            remote: String::new(),
        };
    };
    if !local.starts_with("cl-") {
        // 稳定版：按语义版本比大小。
        let local_v = parse_semver(local);
        let remote_v = parse_semver(&remote);
        if let (Some(l), Some(r)) = (local_v, remote_v) {
            if r > l {
                return UpdateState::Outdated {
                    local: local.to_string(),
                    remote: remote.clone(),
                    release: None,
                };
            }
            return UpdateState::UpToDate { remote };
        }
    }
    // cl 版 / 任一侧解析不出语义版本：只报告远程最新，不判定新旧。
    UpdateState::UpToDate { remote }
}



// ---------------------------------------------------------------------------
// 应用内自更新
// ---------------------------------------------------------------------------

/// 启动一次自更新（下载新 exe -> 启动替换脚本 -> 状态转 `Applying`）。
///
/// # 为什么必须「外部脚本替换」而不是自己覆盖自己
///
/// Windows 对**运行中的 exe** 有文件锁，进程无法写入/删除自己的映像。
/// 通行做法是：把新 exe 下到临时目录，起一个**独立的**小脚本，
/// 让它等本进程退出后再动文件。本函数只负责「下载 + 起脚本」，
/// 真正的替换在 [`build_apply_script`] 生成的脚本里。
///
/// # 幂等
///
/// 已经在下载/替换中时重复点击直接忽略——否则两个下载线程会争同一个
/// 临时文件，替换脚本也会被启动两次。
pub fn start_self_update() {
    let st = shared_state();
    let release = {
        let Ok(g) = st.lock() else { return };
        match &*g {
            UpdateState::Outdated { release, .. } => release.clone(),
            _ => return,
        }
    };
    // 没有 exe 资产时调用方应降级为「打开下载页」，这里只管真正的自更新。
    let Some(release) = release else {
        return;
    };
    {
        let Ok(mut g) = st.lock() else { return };
        if matches!(*g, UpdateState::Downloading | UpdateState::Applying) {
            return;
        }
        *g = UpdateState::Downloading;
    }
    let st = Arc::clone(st);
    std::thread::spawn(move || {
        let result = do_self_update(release);
        if let Ok(mut g) = st.lock() {
            *g = result;
        }
    });
}

/// 自更新的实际步骤（跑在后台线程里，不在 UI 线程）。
fn do_self_update(rel: ReleaseInfo) -> UpdateState {
    // `creation_flags` 是 Windows 扩展 trait 的方法：不导入就找不到
    // （`quiet_command` 内部那份 `use` 只在它自己的函数体里可见）。
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let Some(url) = rel.download_url else {
        return UpdateState::Failed("该版本没有可下载的 exe".into());
    };
    let target = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return UpdateState::Failed(format!("定位当前程序失败：{e}")),
    };
    let dir = std::env::temp_dir().join("modular-clipboard-update");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return UpdateState::Failed(format!("创建临时目录失败：{e}"));
    }
    let src = dir.join("new.exe");
    let script = dir.join("apply.ps1");

    // ---- 1. 下载 ----
    //
    // `--fail` 让 HTTP 4xx/5xx 也返回非零退出码：不加的话 curl 会把
    // 错误页当成功下载写进 new.exe，然后替换出一个损坏的 exe。
    let out = quiet_command("curl")
        .args(["-sL", "--fail", "--max-time", "600", "-o"])
        .arg(&src)
        .arg(&url)
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr);
            let msg = msg.trim();
            return UpdateState::Failed(if msg.is_empty() {
                format!("下载失败（HTTP {}）", o.status)
            } else {
                format!("下载失败：{msg}")
            });
        }
        Err(e) => return UpdateState::Failed(format!("curl 不可用：{e}")),
    }
    // 内容校验：小于 1MB 的肯定不是完整 exe（本项目二进制 ~10MB）。
    // 这是最便宜的完整性门槛，能拦住「网络中断截断」「拿到错误页」两类。
    match std::fs::metadata(&src) {
        Ok(m) if m.len() > 1_000_000 => {}
        Ok(m) => {
            let _ = std::fs::remove_file(&src);
            return UpdateState::Failed(format!("下载的文件不完整（{} 字节）", m.len()));
        }
        Err(e) => return UpdateState::Failed(format!("读取下载文件失败：{e}")),
    }

    // ---- 2. 写替换脚本 ----
    if let Err(e) = std::fs::write(&script, build_apply_script(&target, &src)) {
        return UpdateState::Failed(format!("写入更新脚本失败：{e}"));
    }

    // ---- 3. 分离启动脚本 ----
    //
    // `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` 让脚本不随本进程
    // 退出而被回收——脚本要**等本进程退出**才能动 exe，它必须比本进程
    // 活得久。`CREATE_NO_WINDOW` 只是不弹控制台窗口，两者作用不同，
    // 要一起给（注意 `creation_flags` 是**整体覆盖**不是追加）。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = quiet_command("powershell");
    cmd.args([
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-File",
    ])
    .arg(&script)
    .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    match cmd.spawn() {
        Ok(_) => UpdateState::Applying,
        Err(e) => {
            let _ = std::fs::remove_file(&script);
            UpdateState::Failed(format!("启动更新脚本失败：{e}"))
        }
    }
}

/// 单引号包裹的 PowerShell 字面量（内部单引号翻倍转义）。
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 生成替换脚本（PowerShell）。
///
/// 三个职责，缺一不可：
///
/// 1. **等本进程退出**——Windows 对运行中的 exe 有文件锁，提前动它
///    会 `Move-Item` 失败，结果是「新旧都在、一个都没换上」；
/// 2. **保留旧版**（`.old`）——替换中途失败时用户还能手动回退，
///    直接删掉就再也没有退路；
/// 3. **启动新版并自删**——脚本是一次性的，留在临时目录里是垃圾。
///
/// # 为什么用字符串拼接而不是 `format!`
///
/// PowerShell 脚本里满是 `{` `}`（`if` / `while` 的块），与 `format!`
/// 的占位符冲突——要写成 `{{` `}}`，漏一个就是编译期不报错、生成的
/// 脚本却跑不起来。拼接最直白。
///
/// 单独成纯函数是为了能被单测直接检查生成的脚本内容（路径转义、
/// 等待逻辑）——那些错误在实机上表现为「点了更新没反应」，极难追。
pub fn build_apply_script(target: &std::path::Path, src: &std::path::Path) -> String {
    let mut s = String::new();
    s.push_str("$ErrorActionPreference = 'SilentlyContinue'\n");
    s.push_str(&format!("$target = {}\n", ps_quote(&target.to_string_lossy())));
    s.push_str(&format!("$src = {}\n", ps_quote(&src.to_string_lossy())));
    s.push_str("$procName = [System.IO.Path]::GetFileNameWithoutExtension($target)\n\n");

    s.push_str("# 1. 等本进程退出（Windows 对运行中的 exe 有文件锁，抢跑必失败）\n");
    s.push_str("$deadline = (Get-Date).AddSeconds(60)\n");
    s.push_str(
        "while ((Get-Process -Name $procName -ErrorAction SilentlyContinue) -and ((Get-Date) -lt $deadline)) {\n",
    );
    s.push_str("    Start-Sleep -Milliseconds 500\n");
    s.push_str("}\n\n");

    s.push_str("# 2. 替换（旧版改名留作回退，不直接删）\n");
    s.push_str("if (Test-Path -LiteralPath $target) {\n");
    s.push_str("    $old = $target + '.old'\n");
    s.push_str("    Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue\n");
    s.push_str("    Move-Item -LiteralPath $target -Destination $old -Force -ErrorAction SilentlyContinue\n");
    s.push_str("}\n");
    s.push_str("Move-Item -LiteralPath $src -Destination $target -Force -ErrorAction SilentlyContinue\n\n");

    s.push_str("# 3. 启动新版；成功才自删脚本（失败时留着，方便排查）\n");
    s.push_str("if (Test-Path -LiteralPath $target) {\n");
    s.push_str("    Start-Process -FilePath $target\n");
    s.push_str("    Remove-Item -LiteralPath $MyInvocation.MyCommand.Path -Force -ErrorAction SilentlyContinue\n");
    s.push_str("}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_accepts_plain_and_v_forms() {
        assert_eq!(parse_semver("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_semver("v0.2"), Some((0, 2, 0)));
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_semver_rejects_non_semver() {
        // 非语义标签绝不能被当成版本号——那会把用户引到错误的结论。
        assert_eq!(parse_semver("beta1"), None);
        assert_eq!(parse_semver("v0.2.1.2"), None);
        assert_eq!(parse_semver(""), None);
        assert_eq!(parse_semver("0.x"), None);
    }

    #[test]
    fn best_tag_picks_highest_semver() {
        let out = "\
6d1f0a2\trefs/tags/v0.1
8c3b1e4\trefs/tags/v0.2
9aaa111\trefs/tags/beta
7bbb222\trefs/tags/v0.10.1
";
        assert_eq!(best_tag(out).as_deref(), Some("0.10.1"), "0.10.1 高于 0.2");
    }

    #[test]
    fn best_tag_returns_none_when_no_semver_tag() {
        assert_eq!(best_tag("abc123\trefs/tags/beta\n"), None);
        assert_eq!(best_tag(""), None);
    }

    #[test]
    fn stable_outdated_when_remote_is_newer() {
        let st = compare_tags("0.2", Some("0.3".to_string()));
        assert!(matches!(
            st,
            UpdateState::Outdated { ref remote, .. } if remote == "0.3"
        ));
    }

    #[test]
    fn stable_up_to_date_when_equal_or_newer() {
        assert!(matches!(
            compare_tags("0.2", Some("0.2".to_string())),
            UpdateState::UpToDate { .. }
        ));
        assert!(matches!(
            compare_tags("0.3", Some("0.2".to_string())),
            UpdateState::UpToDate { .. }
        ));
    }

    #[test]
    fn cl_build_never_reports_outdated() {
        // cl 版是开发构建、没有可比的版本号，「有更新」会变成常驻噪音，
        // 反而没人看。它只被告知远程最新，不判新旧。
        let st = compare_tags("cl-a1b2c3d", Some("9.9".to_string()));
        assert!(
            matches!(st, UpdateState::UpToDate { ref remote } if remote == "9.9"),
            "cl 版不应判定为有更新，实得 {st:?}"
        );
    }

    #[test]
    fn no_remote_tag_is_up_to_date_not_failed() {
        // 远程没有任何 tag：说「已是最新」是诚实的，不能谎报失败。
        assert!(matches!(
            compare_tags("0.2", None),
            UpdateState::UpToDate { ref remote } if remote.is_empty()
        ));
    }

    #[test]
    fn version_label_shape() {
        // 与「cl 版带 commit、稳定版 0.2」的口径一致：有 hash 必带
        // `cl-` 前缀，没有 hash 就是稳定版号。
        let v = version_label();
        if is_dev_build() {
            assert!(v.starts_with("cl-"), "开发版应为 cl-<hash>，实得 {v}");
            assert!(v.len() > 3);
        } else {
            assert_eq!(v, STABLE_VERSION);
        }
    }

    #[test]
    fn license_text_is_embedded() {
        // 协议正文必须随 exe 可读（`include_str!`）：缺了就是「点查看
        // 协议没反应」，而用户无从知道原因。
        assert!(!LICENSE_TEXT.is_empty(), "协议正文不能为空");
        assert!(
            LICENSE_TEXT.contains("GNU GENERAL PUBLIC LICENSE"),
            "应是 GPL 正文"
        );
    }

    #[test]
    fn stack_links_all_point_to_github_or_official() {
        // 技术栈链接必须是 https 且非空（点击要能打开）。
        assert!(!STACK_LINKS.is_empty());
        for (name, url) in STACK_LINKS {
            assert!(url.starts_with("https://"), "{name} 的链接必须是 https：{url}");
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn apply_script_waits_for_process_exit_before_touching_exe() {
        // 替换脚本**必须**先等本进程退出：Windows 对运行中的 exe 有
        // 文件锁，抢跑会让 Move-Item 失败——现象是「点了更新，新旧都在、
        // 一个都没换上」。这条守门防的就是有人图省事把等待删掉。
        let script = build_apply_script(
            std::path::Path::new(r"C:\app\modular-clipboard.exe"),
            std::path::Path::new(r"C:\tmp\new.exe"),
        );
        assert!(
            script.contains("Get-Process"),
            "脚本必须先确认主进程已退出，实得：{script}"
        );
        assert!(
            script.contains("Get-Date") && script.contains("AddSeconds"),
            "等待要有超时上限，否则主进程卡死时脚本会永久挂着"
        );
    }

    #[test]
    fn apply_script_keeps_old_build_as_backup() {
        // 旧版改名成 .old 而不是直接删：替换中途失败时用户还能手动回退。
        let script = build_apply_script(
            std::path::Path::new(r"C:\app\app.exe"),
            std::path::Path::new(r"C:\tmp\new.exe"),
        );
        assert!(script.contains(".old"), "必须保留 .old 备份，实得：{script}");
        assert!(
            script.contains("Move-Item"),
            "用改名而不是删除，失败时还能救回来"
        );
    }

    #[test]
    fn apply_script_restarts_new_version() {
        let script = build_apply_script(
            std::path::Path::new(r"C:\app\app.exe"),
            std::path::Path::new(r"C:\tmp\new.exe"),
        );
        assert!(script.contains("Start-Process"), "替换完必须启动新版");
    }

    #[test]
    fn apply_script_escapes_single_quotes_in_paths() {
        // 路径里出现单引号时必须翻倍转义（PowerShell 单引号字符串的规则），
        // 否则脚本解析直接失败，现象是「点了更新没反应」。
        let script = build_apply_script(
            std::path::Path::new("C:\\a'b\\app.exe"),
            std::path::Path::new("C:\\tmp\\new.exe"),
        );
        assert!(
            script.contains("'C:\\a''b\\app.exe'"),
            "单引号应翻倍转义，实得：{script}"
        );
        assert!(script.contains("'C:\\tmp\\new.exe'"), "路径要被引号包裹");
    }

    #[test]
    fn apply_script_uses_literal_paths() {
        // `-LiteralPath` 而不是 `-Path`：路径里带 `[` `]` `*` 这类
        // 通配符时，`-Path` 会当模式去匹配，找不到文件。
        let script = build_apply_script(
            std::path::Path::new(r"C:\app\app.exe"),
            std::path::Path::new(r"C:\tmp\new.exe"),
        );
        assert!(
            script.contains("-LiteralPath"),
            "路径操作必须用 -LiteralPath，实得：{script}"
        );
    }

    #[test]
    fn update_state_shows_progress_variants() {
        // 下载/替换这两个中间态必须存在：没有它们，用户点完「更新」就
        // 只能看到一行静止的文字，分不清是卡住了还是在跑。
        // 这里只断言状态能构造（渲染分支由编译器的 match 穷尽性保证）。
        let _ = UpdateState::Downloading;
        let _ = UpdateState::Applying;
    }
}
