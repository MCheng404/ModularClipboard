//! 常驻进程：监听剪贴板、存历史、守托盘，**不创建任何窗口**。
//!
//! # 为什么要有它
//!
//! 一个窗口的固定成本是 Vulkan 设备 + 交换链 + 字体图集（实测
//! 80~100MB），而且销毁之后内存被驱动收回自己的池子、不还给系统。
//! 只要窗口和后台同进程，「关掉窗口」就永远换不回内存——
//! 用户看到的是进程一直占着 100 多 MB。
//!
//! 拆开之后：
//! - 常驻进程（本模块）只跑业务，不开窗口、不初始化 Vulkan，
//!   内存量级在十几 MB；
//! - 界面进程按需启动、关闭即退出，内存由操作系统整体回收。
//!
//! # 线程模型
//!
//! - **IPC 线程**：只做收发，把界面请求转给主线程；
//! - **主线程**：执行请求 + 轮询托盘/热键 + 排空捕获队列。
//!
//! 之所以不让 IPC 线程直接执行业务：`Service` 握着 SQLite 连接，
//! **不是 `Send`**，进不了别的线程。转回主线程反而更好——
//! 请求天然串行，界面看到的状态永远是一致的。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use modular_clipboard_core::Config;
use modular_clipboard_ipc::protocol::{
    Command, DataVersion, PROTOCOL_VERSION, Request, Response, Snapshot,
};
use modular_clipboard_ipc::{Server, request_show_ui, ui_is_running};
use modular_clipboard_platform::hotkey::{self, Slot};
use modular_clipboard_platform::tray::{self, TrayEvent};

use crate::Service;

/// 托盘提示文本。
const TRAY_TOOLTIP: &str = "ModularClipboard 剪贴板";

/// 空闲时的轮询间隔。
///
/// 捕获线程自己有 `poll_interval_ms` 的节奏，这里只是把队列里的
/// 内容排空入库 + 收托盘事件。50ms 足够跟手，又不会让常驻进程
/// 变成 CPU 上的常客。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 一个待处理的请求，附带回信通道。
type Job = (Request, std::sync::mpsc::Sender<Response>);

/// 跨线程共享的控制状态（不含业务状态）。
struct Core {
    /// 数据版本：每次历史/状态变更 +1。
    ///
    /// 界面每帧问一次「变了吗」，比对的就是它——所以常态回答是
    /// [`Response::Unchanged`]，不重传列表。
    version: AtomicU64,
    /// 退出请求（托盘「退出程序」/ 界面里的退出按钮）。
    quit: AtomicBool,
    /// 本进程可执行文件路径，用于拉起界面进程。
    exe: Option<PathBuf>,
    /// `--data-dir` 指定的数据目录。
    ///
    /// 拉起界面时要**原样传下去**：不传的话界面会把日志写到默认目录，
    /// 用户用 `--data-dir` 做的隔离测试就只隔离了一半
    /// （数据隔离了、日志没隔离，排查时看到的是两处日志各一半）。
    data_dir: Option<PathBuf>,
    /// 最近一次快捷键注册失败的信息（`None` = 正常）。
    ///
    /// 放在这里而不是 Service 里：它不是业务状态，而是「环境能力」的
    /// 快照——注册成败取决于别的程序有没有占用组合，与数据无关。
    hotkey_error: std::sync::Mutex<Option<String>>,
}

impl Core {
    fn version(&self) -> DataVersion {
        self.version.load(Ordering::Relaxed)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, service: &Service) -> Snapshot {
        Snapshot {
            version: self.version(),
            items: service.state.items.clone(),
            groups: service.state.groups.clone(),
            stats: service.state.stats,
            capturing: service.is_capturing(),
            hotkey_error: self
                .hotkey_error
                .lock()
                .ok()
                .and_then(|g| g.clone()),
            config: service.state.config.clone(),
            list_version: service.state.list_version,
        }
    }

    /// 唤起界面：已运行就显示，没运行就启动。
    fn show_ui(&self) -> bool {
        if ui_is_running() {
            if !request_show_ui() {
                // 界面刚起来、窗口还没建出来时会走到这里。
                // 它自己马上就会显示，这里绝不能再拉起第二个进程——
                // 那会让两个界面抢同一个实例互斥体。
                tracing::debug!("界面已运行但窗口尚未创建，等待它自己显示");
            }
            return true;
        }
        self.spawn_ui()
    }

    fn spawn_ui(&self) -> bool {
        let Some(exe) = self.exe.as_deref() else {
            tracing::error!("无法确定本程序路径，无法启动界面");
            return false;
        };
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--ui");
        if let Some(dir) = self.data_dir.as_deref() {
            cmd.arg("--data-dir").arg(dir);
        }
        match cmd.spawn() {
            Ok(_) => {
                tracing::info!("已启动界面进程");
                true
            }
            Err(e) => {
                tracing::error!(%e, "启动界面进程失败");
                false
            }
        }
    }
}

/// 常驻进程入口。阻塞直到收到退出请求。
///
/// `start_ui` 为 `true` 时启动后立即拉起界面（对应「双击图标」）；
/// 单独以 `--daemon` 启动时传 `false`（只常驻、不弹窗）。
pub fn run(config: Config, data_dir: Option<&Path>, start_ui: bool) -> anyhow::Result<()> {
    // 热键要从配置里来，而不是模块里的默认值——
    // 否则用户在设置里改了绑定，重启后又被改回去。
    let toggle = hotkey::spec_or_default(&config.hotkey_toggle);
    let paste = hotkey::spec_or_default(&config.hotkey_paste_previous);
    let mut service = Service::with_data_dir(config, data_dir)?;
    service.start_capture();
    // 迷你面板的条目快照：托盘线程弹菜单时要读它（那边没有数据库）。
    let mini_items: tray::MiniItems = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let core = Arc::new(Core {
        version: AtomicU64::new(1),
        quit: AtomicBool::new(false),
        exe: std::env::current_exe().ok(),
        data_dir: data_dir.map(Path::to_path_buf),
        hotkey_error: std::sync::Mutex::new(None),
    });

    if start_ui {
        core.show_ui();
    }

    // ---- IPC 线程：只负责收发，不碰业务状态 ----
    let (tx, rx) = std::sync::mpsc::channel::<Job>();
    let ipc = std::thread::Builder::new()
        .name("mc-ipc".into())
        .spawn(move || ipc_loop(tx))
        .map_err(|e| anyhow::anyhow!("无法创建 IPC 线程：{e}"))?;
    let _ = ipc; // detach：常驻进程活着它就活着。

    // ---- 托盘 ----
    //
    // 托盘自带独立线程与消息循环（见 platform::tray 的模块文档），
    // 这里只是每轮收一次事件。启动失败**不阻断**常驻进程：
    // 没有托盘时用户仍可用快捷键唤起界面（热键由托盘模块一并注册）。
    let tray = match tray::start_with_hotkeys(TRAY_TOOLTIP, toggle, paste) {
        Ok(t) => {
            // 启动注册失败的快捷键要能走到界面上：用户按 Win+V 没反应时，
            // 只会以为程序坏了。信息随快照下发（见 `hotkey_error`）。
            set_hotkey_error(&core, t.failed_hotkeys().to_vec());
            // 钩子必须**在托盘起来之后**挂：它跑在托盘线程的消息循环里。
            sync_mini_items(&service, &mini_items);
            apply_mini_panel(&service.state.config, &mini_items);
            Some(t)
        }
        Err(e) => {
            tracing::error!(%e, "托盘启动失败：迷你面板与托盘菜单都不可用");
            None
        }
    };

    // 已应用的迷你面板掩码：主循环据此判断「配置变了没有」。
    // `set_mini_panel` 每次都会重装全局鼠标钩子，不能每帧无脑调。
    let mut applied_mini = tray::mini_mask(&service.state.config.mini_panel);

    tracing::info!("常驻进程已就绪（监听剪贴板 + 等待界面连接）");

    // ---- 主循环 ----
    //
    // 用 `recv_timeout` 而不是「收完再睡」：界面每帧都要同步一次，
    // 睡满 50ms 会把帧率压到 20fps（拖动窗口会有明显的迟滞感）。
    // 有请求时立刻醒，没请求时才走到下面的轮询。
    while !core.quit.load(Ordering::Relaxed) {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok((req, reply)) => {
                let resp = handle(&core, &mut service, req);
                // 命令可能改了列表（增删置顶清空）⇒ 面板内容要跟着变。
                sync_mini_items(&service, &mini_items);
                // 设置里也可能刚改了迷你面板（开关/修饰键）：
                // 掩码变了才重装钩子——它在每一次右键上都会被回调。
                let mask = tray::mini_mask(&service.state.config.mini_panel);
                if mask != applied_mini {
                    applied_mini = mask;
                    tray::set_mini_panel(mask, mini_items.clone());
                }
                if reply.send(resp).is_err() {
                    // 界面在等回应的过程中退出了，正常现象。
                    tracing::debug!("界面在请求处理期间断开");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::error!("IPC 线程已退出，常驻进程一并退出");
                break;
            }
        }

        if let Some(t) = tray.as_ref() {
            for ev in t.poll() {
                match ev {
                    // 托盘左键与「显示」热键是同一个意图：把界面叫出来。
                    TrayEvent::Show => {
                        // 托盘左键保持「显示」语义：点它就是要把窗口叫出来。
                        core.show_ui();
                    }
                    TrayEvent::Hotkey => {
                        // Toggle 语义：窗口可见就隐藏、不可见就显示。
                        // 系统 Win+V 就是 toggle，用户预期「再按一次收起
                        // 来」——只唤不藏会让热键变成单向的「显示」。
                        let visible = modular_clipboard_ipc::ui_window_visible();
                        if visible {
                            if !modular_clipboard_ipc::hide_ui_window() {
                                // 隐藏失败（窗口刚好在销毁）就别较劲，
                                // 退回「确保显示」。
                                tracing::warn!("热键 toggle：隐藏主窗口失败，退回显示");
                                core.show_ui();
                            } else {
                                tracing::info!("热键 toggle：主窗口已隐藏");
                            }
                        } else {
                            tracing::info!("热键 toggle：唤起主窗口");
                            core.show_ui();
                        }
                    }
                    TrayEvent::HotkeyShown => {
                        // 托盘线程已在 WM_HOTKEY 前台授权窗口内直接完成
                        // 跨进程 ShowWindow + SetForegroundWindow，
                        // 这里绝不能再按 toggle 把它藏回去。
                        // 再投递一次 WM_SHOW_MAIN（幂等）：让 UI 主线程的
                        // focus_window 兜底置前、立刻重建交换链并恢复
                        // 渲染，不必等 200ms 可见性对账轮询。
                        if !request_show_ui() {
                            tracing::debug!("HotkeyShown 兜底投递 WM_SHOW_MAIN 失败");
                        }
                    }
                    // 「粘贴上一条」不经过界面：直接把最新一条写回剪贴板
                    // 并发 Ctrl+V 给前台窗口——走界面就太慢了。
                    TrayEvent::HotkeyPaste => {
                        if let Some(id) = service.state.items.first().map(|i| i.id) {
                            if let Err(e) = service.copy_and_paste(id) {
                                tracing::error!(%e, "粘贴上一条失败");
                            }
                        } else {
                            tracing::debug!("历史为空，忽略「粘贴上一条」");
                        }
                    }
                    TrayEvent::ClearHistory => {
                        if let Err(e) = service.clear_all() {
                            tracing::error!(%e, "清空历史失败");
                        } else {
                            core.bump();
                        }
                    }
                    // 迷你面板里选中一条：写回剪贴板。
                    // 不自动粘贴——用户按住修饰键点的右键，
                    // 粘贴到哪要他自己决定。
                    TrayEvent::MiniPick(id) => {
                        sync_mini_items(&service, &mini_items);
                        if let Err(e) = service.copy_item(id) {
                            tracing::error!(%e, "迷你面板复制条目失败");
                        }
                    }
                    TrayEvent::Quit => {
                        tracing::info!("托盘请求退出");
                        core.quit.store(true, Ordering::Relaxed);
                    }
                }
            }
        }

        if service.pump() {
            core.bump();
            sync_mini_items(&service, &mini_items);
        }
        // 心跳：主循环卡死（托盘菜单能弹但点项没反应 = 这个循环不再转）
        // 时，这条日志的停顿就是唯一证据。约每 2 秒一条。
        {
            static HB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if HB
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .is_multiple_of(100)
            {
                tracing::debug!("常驻主循环心跳");
            }
        }
    }

    // 退出前落盘：捕获开关的临时覆盖在这里被还原（见 `save_config`）。
    if let Err(e) = service.save_config() {
        tracing::warn!(%e, "退出时保存配置失败");
    }
    service.stop_capture();
    tracing::info!("常驻进程退出");
    Ok(())
}

/// 按当前配置重新注册两个热键槽位；返回失败描述（空 = 全部成功）。
///
/// 同步等待注册结果（`tray::set_hotkey` 已是同步调用）：失败必须回传，
/// 界面靠它显示「已被其它程序占用」——静默失败等于「改了键位但按键
/// 没反应」的老 bug。
///
/// 单进程模式的界面进程也调用它（那时托盘就在本进程里）。
pub fn rebind_hotkeys(cfg: &modular_clipboard_core::Config) -> Vec<String> {
    let mut failed = Vec::new();
    for (slot, hk) in [
        (Slot::Toggle, &cfg.hotkey_toggle),
        (Slot::PastePrevious, &cfg.hotkey_paste_previous),
    ] {
        let spec = hotkey::spec_or_default(hk);
        if !tray::set_hotkey(slot, spec) {
            failed.push(hotkey::failed_message(slot, spec));
        }
    }
    failed
}

/// 记录 / 清除「快捷键注册失败」状态。空清单 = 全部成功 ⇒ 清除。
fn set_hotkey_error(core: &Core, failed: Vec<String>) {
    if let Ok(mut guard) = core.hotkey_error.lock() {
        *guard = if failed.is_empty() {
            None
        } else {
            Some(failed.join("；"))
        };
    }
}

/// 把最近若干条复制进迷你面板的共享快照。
///
/// 只复制**预览文本**不复制载荷：面板要显示的就只是一行字，
/// 而载荷可能有几 MB。
///
/// 公开：单进程模式的界面进程也要维护同一份快照。
pub fn sync_mini_items(service: &Service, cell: &tray::MiniItems) {
    let n = service.state.config.mini_panel.effective_count();
    // 每项前缀「序号 + 置顶标记」：系统菜单是唯一能承载的元信息通道
    // （菜单项只有一行文本，没有副标题/图标）。序号让用户知道第几项
    // 对应第几条；置顶标记用 ASCII 的 `*` 而不是 `★`——后者依赖系统
    // 字体是否带该字形，缺字形会显示成豆腐块。
    //
    // CAUTION: 前缀占了宽度，预览必须相应缩短，否则菜单项过宽会被
    // 系统在屏幕边缘折行/截断（菜单不会省略号，只会难看）。
    const PREVIEW_CHARS: usize = 40;
    let snapshot: Vec<(i64, String)> = service
        .state
        .items
        .iter()
        .take(n)
        .enumerate()
        .map(|(idx, i)| {
            let mark = if i.pinned { "*" } else { " " };
            (
                i.id,
                format!(
                    "{}{}. {}",
                    mark,
                    idx + 1,
                    i.preview.chars().take(PREVIEW_CHARS).collect::<String>()
                ),
            )
        })
        .collect();
    if let Ok(mut g) = cell.lock() {
        *g = snapshot;
    }
}

/// 按配置启用 / 停用迷你面板。
///
/// 掩码判定在 [`modular_clipboard_platform::tray::mini_mask`]——
/// 常驻进程与单进程模式的界面共用同一份判定，避免两边漂移。
pub fn apply_mini_panel(cfg: &modular_clipboard_core::Config, cell: &tray::MiniItems) {
    tray::set_mini_panel(tray::mini_mask(&cfg.mini_panel), cell.clone());
}

/// 接受界面连接，把请求转给主线程执行。
///
/// 串行接受：同一时刻只服务一个界面进程。多个界面本来也不该有
/// （第二个实例会先去唤起第一个），串行处理省掉了排队与隔离。
fn ipc_loop(tx: std::sync::mpsc::Sender<Job>) {
    let server = match Server::bind() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "无法监听命名管道：界面将打不开（常驻进程仍在记录剪贴板）");
            return;
        }
    };
    loop {
        let mut conn = match server.accept() {
            Ok(c) => c,
            Err(e) => {
                // 单个连接失败不该让整个监听死掉：可能只是界面进程
                // 在建连瞬间就被关掉了。
                tracing::warn!(%e, "接受界面连接失败，继续监听");
                continue;
            }
        };
        while let Ok(Some(req)) = conn.recv() {
            let (reply, answer) = std::sync::mpsc::channel();
            if tx.send((req, reply)).is_err() {
                break;
            }
            match answer.recv() {
                Ok(resp) => {
                    if conn.send(&resp).is_err() {
                        break;
                    }
                }
                // 主线程没回信（正在退出）：断开即可。
                Err(_) => break,
            }
        }
    }
}

/// 处理一个请求。
fn handle(core: &Core, svc: &mut Service, req: Request) -> Response {
    match req {
        Request::Hello { version } => {
            if version != PROTOCOL_VERSION {
                return Response::VersionMismatch { server: PROTOCOL_VERSION };
            }
            Response::Snapshot(core.snapshot(svc))
        }
        Request::Sync { known } => {
            if known == core.version() {
                Response::Unchanged
            } else {
                Response::Snapshot(core.snapshot(svc))
            }
        }
        Request::Cmd(cmd) => handle_cmd(core, svc, cmd),
    }
}

/// 处理一条业务命令。
fn handle_cmd(core: &Core, svc: &mut Service, cmd: Command) -> Response {
    // 写类命令的统一收尾：版本 +1 并带回新列表。
    //
    // 带回列表而不是只回 `Ok`，是为了省掉界面「改完再问一次」的往返：
    // 界面每帧都在同步，多一次往返就是一帧的延迟。
    macro_rules! changed {
        () => {{
            core.bump();
            Response::Snapshot(core.snapshot(svc))
        }};
    }

    match cmd {
        Command::ReloadList => {
            svc.reload_list();
            changed!()
        }
        Command::Search(q) => {
            svc.search(q);
            changed!()
        }
        Command::FilterGroup(f) => {
            svc.filter_group(f);
            changed!()
        }
        Command::Ingest(payload) => {
            svc.ingest(payload);
            changed!()
        }
        Command::DeleteItem(id) => match svc.delete_item(id) {
            Ok(()) => changed!(),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::TogglePin(id) => match svc.toggle_pin(id) {
            Ok(()) => changed!(),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::ClearAll => match svc.clear_all() {
            Ok(()) => changed!(),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::CopyItem(id) => match svc.copy_item(id) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e.to_string()),
        },
        Command::WriteClipboardText(text) => match svc.write_clipboard_text(&text) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e.to_string()),
        },
        Command::SuppressSelfCapture => {
            svc.suppress_self_capture();
            Response::Ok
        }
        Command::SuspendHotkeys(suspend) => {
            // 录制快捷键：注销让按键能到界面窗口；恢复按当前配置重注册。
            if suspend {
                tray::suspend_hotkeys();
            } else {
                let failed = rebind_hotkeys(&svc.state.config);
                set_hotkey_error(core, failed);
            }
            Response::Ok
        }
        Command::SetCaptureEnabled(on) => {
            // 界面侧改的是「用户此刻想不想监听」，因此要一并落盘——
            // 否则界面一关、常驻进程一重启，开关又回到旧值。
            svc.state.config.capture.enabled = on;
            if on {
                svc.start_capture();
            } else {
                svc.stop_capture();
            }
            if let Err(e) = svc.save_config() {
                tracing::warn!(%e, "监听开关落盘失败");
            }
            changed!()
        }
        Command::SaveConfig(cfg) => {
            svc.state.config = cfg;
            // 用户可能刚改了快捷键：注册必须跟着变，否则界面上显示
            // 新绑定、实际按旧键才有效。失败结果回传界面（hotkey_error）。
            let failed = rebind_hotkeys(&svc.state.config);
            set_hotkey_error(core, failed);
            match svc.save_config() {
                // bump 版本并回一份快照：快照里带着 `hotkey_error`
                // 与刚保存的配置——界面这一帧就能拿到注册失败信息，
                // 不用等下一次列表变化。
                Ok(()) => changed!(),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::LoadPayload(id) => {
            let Some(store) = svc.store() else {
                return Response::Error("常驻进程未打开存储".into());
            };
            match store.get(id) {
                Ok(Some(item)) => match store.load_payload(&item) {
                    Ok(bytes) => Response::Bytes(bytes),
                    Err(e) => Response::Error(e.to_string()),
                },
                Ok(None) => Response::Error("条目不存在".into()),
                Err(e) => Response::Error(e.to_string()),
            }
        }
        Command::ShowWindow => {
            if core.show_ui() {
                Response::Ok
            } else {
                Response::Error("无法唤起界面".into())
            }
        }
        Command::Quit => {
            tracing::info!("界面请求常驻进程退出");
            core.quit.store(true, Ordering::Relaxed);
            Response::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modular_clipboard_core::{CapturedPayload, ClipKind};

    /// 测试用常驻进程环境。
    ///
    /// 数据目录必须指向临时目录：`Service` 的默认路径是用户的
    /// `%APPDATA%/modular-clipboard`，测试里 `SaveConfig` /
    /// `SetCaptureEnabled` 会**真的写配置文件**——那等于让测试去改
    /// 用户的设置。
    struct Fixture {
        core: Core,
        svc: Service,
        _dir: TempDir,
    }

    /// 极简临时目录（不为测试引新依赖）。
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("mc-daemon-test-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("建临时目录");
            Self(p)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        let dir = TempDir::new(tag);
        let mut config = Config::default();
        // 保持 capture.enabled = true（摄取路径会先检查它），但**不调用**
        // `start_capture`——捕获线程只有显式启动才会有，测试因此不会
        // 真的去读系统剪贴板。
        //
        // 挂一个独特的界面参数，用来验证「配置确实随快照下发了」：
        // 只断言 enabled 的话，字段没传也照样通过。
        config.ui.window_opacity = 0.77;
        let svc = Service::with_data_dir(config, Some(dir.path())).expect("打开临时库");
        Fixture {
            core: Core {
                version: AtomicU64::new(1),
                quit: AtomicBool::new(false),
                exe: None,
                data_dir: Some(dir.path().to_path_buf()),
                hotkey_error: std::sync::Mutex::new(None),
            },
            svc,
            _dir: dir,
        }
    }

    fn payload(text: &str) -> CapturedPayload {
        CapturedPayload {
            kind: ClipKind::Text,
            bytes: text.as_bytes().to_vec(),
            html: None,
            text: Some(text.to_string()),
            source_app: "test".into(),
            files: Vec::new(),
        }
    }

    /// 版本不匹配必须在握手阶段被拒绝。
    ///
    /// 放行的话，新旧 exe 并存时会按不同字段名解析同一段 JSON，
    /// 结果不是报错而是**静默错乱**——界面显示的是半截状态。
    #[test]
    fn hello_rejects_version_mismatch() {
        let mut f = fixture("hello-ver");
        let resp = handle(
            &f.core,
            &mut f.svc,
            Request::Hello {
                version: PROTOCOL_VERSION + 1,
            },
        );
        assert!(matches!(resp, Response::VersionMismatch { .. }));
    }

    /// 握手成功要带回配置与列表——界面进程的一切都靠它起家。
    #[test]
    fn hello_returns_a_full_snapshot() {
        let mut f = fixture("hello-ok");
        let resp = handle(
            &f.core,
            &mut f.svc,
            Request::Hello {
                version: PROTOCOL_VERSION,
            },
        );
        match resp {
            Response::Snapshot(s) => {
                assert_eq!(s.version, 1);
                assert!(s.items.is_empty());
                // 配置必须随快照下发，否则界面只能读自己的 config.json，
                // 又变成「两个权威来源」。
                assert!((s.config.ui.window_opacity - 0.77).abs() < 1e-6);
            }
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }
    }

    /// 版本没变时只回 `Unchanged`：这是每帧同步的常态路径，
    /// 回整份列表的话每帧都要序列化两千条历史。
    #[test]
    fn sync_with_same_version_is_cheap() {
        let mut f = fixture("sync-same");
        let resp = handle(&f.core, &mut f.svc, Request::Sync { known: 1 });
        assert!(matches!(resp, Response::Unchanged));
    }

    /// 写入一条历史后：版本号必须变、列表必须带回来。
    #[test]
    fn ingest_bumps_version_and_returns_items() {
        let mut f = fixture("ingest");
        let resp = handle(
            &f.core,
            &mut f.svc,
            Request::Cmd(Command::Ingest(payload("hello"))),
        );
        match resp {
            Response::Snapshot(s) => {
                assert_eq!(s.items.len(), 1);
                assert_eq!(s.items[0].preview, "hello");
                assert_eq!(s.version, 2, "写入后版本号必须递增");
            }
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }
        // 版本号真的变了之后，旧版本号的同步请求才会回列表。
        let resp = handle(&f.core, &mut f.svc, Request::Sync { known: 1 });
        assert!(matches!(resp, Response::Snapshot(_)));
        let resp = handle(&f.core, &mut f.svc, Request::Sync { known: 2 });
        assert!(matches!(resp, Response::Unchanged));
    }

    /// 删除 / 置顶 / 清空都要走同一条「改完带回列表」的路径。
    #[test]
    fn mutations_return_the_new_list() {
        let mut f = fixture("mutations");
        for t in ["a", "b"] {
            handle(&f.core, &mut f.svc, Request::Cmd(Command::Ingest(payload(t))));
        }
        let id = f.svc.state.items[0].id;

        match handle(&f.core, &mut f.svc, Request::Cmd(Command::TogglePin(id))) {
            Response::Snapshot(s) => {
                assert!(s.items.iter().any(|i| i.id == id && i.pinned), "应已置顶");
            }
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }

        match handle(&f.core, &mut f.svc, Request::Cmd(Command::DeleteItem(id))) {
            Response::Snapshot(s) => assert!(s.items.iter().all(|i| i.id != id), "应已删除"),
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }

        match handle(&f.core, &mut f.svc, Request::Cmd(Command::ClearAll)) {
            Response::Snapshot(s) => assert!(s.items.is_empty(), "应已清空"),
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }
    }

    /// 界面改的配置要落到常驻进程这一份上（它才是持盘者）。
    #[test]
    fn save_config_replaces_the_daemon_config() {
        let mut f = fixture("save-cfg");
        let mut cfg = f.svc.state.config.clone();
        cfg.ui.window_opacity = 0.42;
        let resp = handle(&f.core, &mut f.svc, Request::Cmd(Command::SaveConfig(cfg)));
        // 回快照而不是 Ok：配置变更也是状态变更（版本 +1），
        // 顺手把热键注册失败信息带回界面。
        assert!(matches!(resp, Response::Snapshot(_)), "实得 {resp:?}");
        assert!((f.svc.state.config.ui.window_opacity - 0.42).abs() < 1e-6);
    }

    /// 快捷键注册失败必须随快照回传——静默失败等于「改了键位但
    /// 按键没反应」的老 bug。
    ///
    /// 测试进程里没有托盘（没有可注册热键的窗口），`rebind_hotkeys`
    /// 必然失败，正好用来验证这条链路。若这里变成 None，说明失败
    /// 又被吞了。
    #[test]
    fn save_config_surfaces_hotkey_registration_failure() {
        let mut f = fixture("save-cfg-hotkey");
        let cfg = f.svc.state.config.clone();
        let resp = handle(&f.core, &mut f.svc, Request::Cmd(Command::SaveConfig(cfg)));
        let Response::Snapshot(snap) = resp else {
            panic!("期望 Snapshot，实得 {resp:?}");
        };
        let err = snap
            .hotkey_error
            .expect("无托盘时注册必然失败，应带回失败信息");
        assert!(err.contains("注册失败"), "信息应说明失败：{err}");
        // 两个槽位都应出现在信息里（此处两个都注册不了）。
        assert!(err.contains("显示主窗口"), "{err}");
        assert!(err.contains("粘贴上一个条目"), "{err}");
    }

    /// 关掉监听：常驻进程要停掉捕获，并回报 `capturing = false`。
    #[test]
    fn set_capture_off_stops_listening() {
        let mut f = fixture("cap-off");
        let resp = handle(
            &f.core,
            &mut f.svc,
            Request::Cmd(Command::SetCaptureEnabled(false)),
        );
        match resp {
            Response::Snapshot(s) => assert!(!s.capturing),
            other => panic!("期望 Snapshot，实得 {other:?}"),
        }
        assert!(!f.svc.is_capturing());
    }

    /// 读不存在的条目要回可读的错误，而不是断开连接。
    ///
    /// 一次「条目不存在」不该把整个界面踢下线。
    #[test]
    fn missing_payload_is_a_soft_error() {
        let mut f = fixture("no-payload");
        let resp = handle(&f.core, &mut f.svc, Request::Cmd(Command::LoadPayload(999)));
        match resp {
            Response::Error(msg) => assert!(!msg.is_empty(), "错误信息不应为空"),
            other => panic!("期望 Error，实得 {other:?}"),
        }
    }

    /// 退出命令要置上退出标志（主循环据此收工）。
    #[test]
    fn quit_command_sets_the_flag() {
        let mut f = fixture("quit");
        assert!(!f.core.quit.load(Ordering::Relaxed));
        let resp = handle(&f.core, &mut f.svc, Request::Cmd(Command::Quit));
        assert!(matches!(resp, Response::Ok));
        assert!(f.core.quit.load(Ordering::Relaxed));
    }

    /// 抑制自写回：命令必须被接受（真正的抑制发生在常驻进程的
    /// 捕获线程侧，这里只验证链路不报错）。
    #[test]
    fn suppress_self_capture_is_accepted() {
        let mut f = fixture("suppress");
        let resp = handle(
            &f.core,
            &mut f.svc,
            Request::Cmd(Command::SuppressSelfCapture),
        );
        assert!(matches!(resp, Response::Ok));
    }
}
