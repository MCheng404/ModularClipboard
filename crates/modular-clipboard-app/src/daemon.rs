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
    let mut service = Service::with_data_dir(config, data_dir)?;
    service.start_capture();

    let core = Arc::new(Core {
        version: AtomicU64::new(1),
        quit: AtomicBool::new(false),
        exe: std::env::current_exe().ok(),
        data_dir: data_dir.map(Path::to_path_buf),
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
    let tray = match tray::start(TRAY_TOOLTIP) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::error!(%e, "托盘启动失败：将无法从托盘唤起界面");
            None
        }
    };

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
                    // 托盘左键与全局热键是同一个意图：把界面叫出来。
                    TrayEvent::Show | TrayEvent::Hotkey => {
                        core.show_ui();
                    }
                    TrayEvent::ClearHistory => {
                        if let Err(e) = service.clear_all() {
                            tracing::error!(%e, "清空历史失败");
                        } else {
                            core.bump();
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
            match svc.save_config() {
                Ok(()) => Response::Ok,
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
        assert!(matches!(resp, Response::Ok), "实得 {resp:?}");
        assert!((f.svc.state.config.ui.window_opacity - 0.42).abs() < 1e-6);
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
