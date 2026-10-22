//! 应用服务层：编排捕获、过滤、存储与界面事件。
//!
//! 线程模型：
//! - **捕获线程**：独立线程，仅做序列号轮询与内容读取，不触碰数据库。
//! - **UI 线程**：egui 事件循环，负责所有存储读写与绘制。
//!
//! 两者通过无锁环形队列（rtrb）通信。这样做的关键原因是
//! 剪贴板读取可能被其他进程短暂锁定，若在读取时同步写库，
//! 会连带卡住界面；解耦后即使读取失败也只影响后台线程。

pub mod action;
pub mod daemon;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use modular_clipboard_capture::ClipboardReader;
use modular_clipboard_core::{CapturedPayload, ClipItem, ClipKind, Config, DropStats, EntryId, now_ms};
use modular_clipboard_ipc::{Command, Response};
use modular_clipboard_store::{GroupFilter, Store, fingerprint};

mod ingest;
mod paths;

pub use ingest::{PREVIEW_MAX_CHARS, format_time};
pub use paths::{CONFIG_FILE_NAME, config_path_in};
use ingest::*;
use paths::*;


/// 界面与应用共享的状态。
pub struct AppState {
    pub config: Config,
    pub items: Vec<ClipItem>,
    /// `items` 的版本号：每次整体替换（`reload_list`）时 +1。
    ///
    /// 绘制层靠它判断「行高缓存是否已失效」——行高只取决于条目文本与
    /// 布局参数，跨帧不变；没有版本号就无法知道列表什么时候换过，
    /// 只能每帧重算（PF-1：2000 次 LayoutJob 测量/帧）。
    pub list_version: u64,
    pub groups: Vec<modular_clipboard_core::Group>,
    pub selected: Option<EntryId>,
    pub query: String,
    /// 当前过滤的分组，`None` 为全部。
    pub group_filter: Option<GroupFilter>,
    pub notice: Option<(String, Instant)>,
    pub stats: DropStats,
    /// 监听是否处于运行状态。
    pub capturing: bool,
    /// 最近一次全局快捷键注册失败的信息（`None` = 正常）。
    ///
    /// 单进程模式下由界面进程自己写入；常驻进程模式下随快照下发。
    /// 界面只在它**变化**时提示，见 `App::surface_hotkey_error`。
    pub hotkey_error: Option<String>,
}

impl AppState {
    /// 展示中的条目数量。
    pub fn visible_count(&self) -> usize {
        self.items.len()
    }
}

/// 后台捕获线程句柄。
pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    /// 已捕获总数，供界面展示。
    pub captured_count: Arc<AtomicU64>,
}

/// 停止后台捕获线程。
impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// 捕获线程发送的信号。
enum Signal {
    Captured {
        payload: CapturedPayload,
        /// 内容指纹，由**捕获线程**预计算（PF-2）。
        ///
        /// SHA-256 对 32MB 载荷约 60~100ms：放在 UI 线程就是「复制
        /// 大图时窗口卡住、掉帧、拖动无响应」的直接原因。捕获线程两轮
        /// 轮询之间有整个 interval（默认 300ms）的空闲，这点计算
        /// 不占它任何预算。
        hash: String,
    },
}

/// 应用服务。
/// 自写抑制窗口的**下限**（毫秒）。实际窗口按轮询间隔动态计算，
/// 见 [`self_write_guard_ms`]。
const SELF_WRITE_GUARD_MS_FLOOR: i64 = 900;

pub struct Service {
    pub state: AppState,
    /// 业务在哪里执行。**界面代码不应感知这个字段**——
    /// 它只调方法、只读 `state`，两种后端的方法签名完全一致。
    backend: Backend,
    /// 当前 `capture.enabled` 是否为命令行临时覆盖（如 `--no-capture`）。
    /// 保存配置时会被还原，避免永久改写用户的监听开关。
    capture_override: bool,
}

/// 服务的执行位置。
///
/// # 为什么有两种
///
/// 渲染一个窗口的固定成本（Vulkan 设备 + 交换链 + 字体图集）在
/// 80~100MB 量级，且销毁后被驱动收进池子、不还给系统。窗口与后台
/// 同进程时，「关掉窗口」就永远换不回内存。
///
/// 拆进程后，界面进程持 [`Backend::Remote`]：它不打开数据库、
/// 不监听剪贴板，业务操作全部转发给常驻进程；窗口一关进程就退出，
/// 内存由操作系统整体回收。
///
/// 单进程模式（[`Backend::Local`]）保留下来，用于测试与应急回退。
#[allow(clippy::large_enum_variant)]
enum Backend {
    // 本地变体大是必然的：它握着 SQLite 连接与捕获线程句柄。
    // 装箱它只会给每一次 `pump()`（每帧一次）多加一层间接，
    // 换不来任何内存收益——同一时刻只有一个 `Service`。
    Local(LocalBackend),
    Remote(RemoteBackend),
}

/// 本地后端：本进程直接操作存储与剪贴板。
struct LocalBackend {
    store: Store,
    registry: action::ActionRegistry,
    /// 事件接收端。`None` 表示捕获未启动。
    ///
    /// rtrb 的 Producer 不可克隆，因此它在启动捕获时被移动进后台线程，
    /// 这里只保留 Consumer。
    rx: Option<rtrb::Consumer<Signal>>,
    capture: Option<CaptureHandle>,
    /// 最近一次自写剪贴板的时刻（Unix 毫秒，0 表示从未写过）。
    last_self_write: Arc<AtomicU64>,
    /// 配置落盘路径。`None` 表示无法确定（`save_config` 静默跳过）。
    ///
    /// 存字段而非每次现算：`--data-dir` 生效后配置写在指定目录下，
    /// 现算会退回 `%APPDATA%`，导致「读 A 写 B」。
    config_path: Option<std::path::PathBuf>,
}

/// 远端后端：一条到常驻进程的连接。
///
/// `known` 是界面侧已看到的数据版本。带上它，每帧同步的常态回答是
/// 「没变」（几十字节），而不是把两千条历史重新序列化一遍。
struct RemoteBackend {
    client: modular_clipboard_ipc::Client,
    known: modular_clipboard_ipc::DataVersion,
    /// 与常驻进程的连接是否已经断了。
    ///
    /// 断了之后界面**不该继续装作没事**：用户会看到列表永远不变、
    /// 点什么都没反应，却不知道是后台没了。置上这个标志，界面会自己退出，
    /// 用户下次点托盘就能重新拉起一对进程。
    dead: bool,
}

impl Service {
    /// 本进程是否只是界面（业务在常驻进程里）。
    ///
    /// 界面侧有几件事必须据此分叉：不启动捕获线程、不创建托盘、
    /// 关闭窗口即退出进程。
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, Backend::Remote(_))
    }

    /// 常驻进程是否已经失联（仅远端模式可能为真）。
    pub fn daemon_gone(&self) -> bool {
        matches!(&self.backend, Backend::Remote(r) if r.dead)
    }
}

/// 把一份常驻进程的同步结果写进界面状态。
///
/// 刻意**不**覆盖 `config` / `query` / `selected` / `notice`：
/// 那是界面进程自己的瞬时状态（用户可能正在设置里改、正在搜索框里输），
/// 被后台的一次同步冲掉是纯粹的 bug。
fn apply_snapshot(snap: modular_clipboard_ipc::Snapshot, state: &mut AppState) {
    state.items = snap.items;
    state.groups = snap.groups;
    state.stats = snap.stats;
    state.capturing = snap.capturing;
    state.hotkey_error = snap.hotkey_error;
    state.list_version = snap.list_version;
}

impl Service {
    /// 打开**默认**数据目录下的存储并载入初始状态。
    pub fn new(config: Config) -> Result<Self> {
        Self::with_data_dir(config, None)
    }

    /// 同 [`Self::new`]，但可指定数据目录（对应 `--data-dir`）。
    ///
    /// `data_dir` 为 `None` 时退回 `%APPDATA%/modular-clipboard`。
    pub fn with_data_dir(config: Config, data_dir: Option<&std::path::Path>) -> Result<Self> {
        // CAUTION: `config_path` **不能**在 `data_dir` 为 `None` 时也是 `None`。
        //
        // 早前写 `data_dir.map(|d| d.join(CONFIG_FILE_NAME))`，
        // 于是默认启动（不带 `--data-dir`，也就是**所有**双击启动的用户）
        // 的 `config_path == None` ⇒ `save_config()` 直接静默返回 ⇒
        // **配置永不落盘**：
        //
        // - 设置面板里改的每一项（主题、置顶、监听开关、历史上限…）
        //   重启后全部归零；
        // - 退出时的窗口位置丢失，每次启动回默认位置；
        // - 「主窗口位置实时记忆」功能 100% 无效。
        //
        // 而 `main.rs::config_path()` **读**的正是
        // `ProjectDirs::config_dir()` —— 于是变成「读在 A 处、写无处可去」。
        //
        // 所以这里必须回退到默认配置文件路径，与读取端对齐。
        let config_path = match data_dir {
            Some(dir) => Some(dir.join(CONFIG_FILE_NAME)),
            None => default_config_path(),
        };
        let store = match data_dir {
            Some(dir) => Store::open_in(dir)?,
            None => Store::open_default()?,
        };
        Self::build(store, config, config_path)
    }

    fn build(
        store: Store,
        config: Config,
        config_path: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        store.apply_config(&config);

        if config.storage.cleanup_on_start {
            match store.gc_orphan_blobs() {
                Ok(n) if n > 0 => tracing::info!(count = n, "已清理孤儿载荷文件"),
                Err(e) => tracing::warn!(%e, "清理孤儿载荷失败"),
                _ => {}
            }
        }

        let (_tx, rx) = rtrb::RingBuffer::new(EVENT_QUEUE_CAPACITY);
        let items = store.list(PAGE_LIMIT, 0, None).unwrap_or_default();
        let groups = store.list_groups().unwrap_or_default();

        Ok(Self {
            state: AppState {
                config,
                items,
                list_version: 1,
                groups,
                selected: None,
                query: String::new(),
                group_filter: None,
                notice: None,
                stats: DropStats::default(),
                capturing: false,
                hotkey_error: None,
            },
            backend: Backend::Local(LocalBackend {
                store,
                registry: action::ActionRegistry::with_builtins(),
                rx: Some(rx),
                capture: None,
                last_self_write: Arc::new(AtomicU64::new(0)),
                config_path,
            }),
            capture_override: false,
        })
    }

    /// 连到常驻进程并以它的数据为初始状态。
    ///
    /// 用于**界面进程**：它自己不打开数据库，`state` 的初值来自握手
    /// 返回的那份快照。
    ///
    /// `timeout` 是给常驻进程的启动缓冲（托盘刚拉起它时管道可能还没建好）。
    pub fn connect_remote(timeout: std::time::Duration) -> Result<Self> {
        use modular_clipboard_ipc::{Client, Snapshot};

        let mut client = Client::connect(timeout)?;
        let Snapshot { version, config, .. } = client.hello()?;
        let mut svc = Self {
            state: AppState {
                config,
                items: Vec::new(),
                list_version: 1,
                groups: Vec::new(),
                selected: None,
                query: String::new(),
                group_filter: None,
                notice: None,
                stats: DropStats::default(),
                capturing: false,
                hotkey_error: None,
            },
            backend: Backend::Remote(RemoteBackend {
                client,
                known: version,
                dead: false,
            }),
            capture_override: false,
        };
        // 握手只带配置，列表还要再取一次（与本地路径一致：build 之后
        // 列表由 reload_list 填充）。
        svc.reload_list();
        Ok(svc)
    }

    /// 请求常驻进程退出（托盘「退出程序」/ 设置里的退出按钮）。
    ///
    /// 本地后端下这就是设置退出标志；远端后端下还要让常驻进程自己也退。
    pub fn request_quit(&mut self) -> Result<()> {
        if let Backend::Remote(r) = &mut self.backend {
            r.client.command(modular_clipboard_ipc::Command::Quit)?;
        }
        Ok(())
    }

    /// 请求唤起界面进程（用户重复双击图标时由第二个进程转发）。
    pub fn request_show_window(&mut self) -> Result<()> {
        if let Backend::Remote(r) = &mut self.backend {
            r.client.command(modular_clipboard_ipc::Command::ShowWindow)?;
        }
        Ok(())
    }

    /// 本地后端的存储句柄。远端后端下恒为 `None`。
    ///
    /// 界面侧**不应**用它——那等于绕开常驻进程直接读写数据库，
    /// 拆进程就白拆了。保留它是给常驻进程与测试用。
    /// 实际数据目录（关于页展示与「打开目录」用）。
    ///
    /// 本地后端问 store（`--data-dir` 也算数）；远端后端够不到常驻
    /// 进程的自定义目录，退回默认目录——两进程同机，默认值几乎总是
    /// 对的，错也只是展示错，不影响数据。
    pub fn data_dir(&self) -> std::path::PathBuf {
        match &self.backend {
            Backend::Local(b) => b.store.data_dir().to_path_buf(),
            Backend::Remote(_) => modular_clipboard_store::default_data_dir()
                .unwrap_or_else(|| std::path::PathBuf::from(".")),
        }
    }

    pub fn store(&mut self) -> Option<&mut Store> {
        match &mut self.backend {
            Backend::Local(l) => Some(&mut l.store),
            Backend::Remote(_) => None,
        }
    }

    /// 配置落盘路径。`None` 表示本进程不持有配置（界面进程）或路径不可确定。
    pub fn config_path(&self) -> Option<&std::path::Path> {
        match &self.backend {
            Backend::Local(l) => l.config_path.as_deref(),
            Backend::Remote(_) => None,
        }
    }

    /// 使用内存存储构造。仅在默认数据目录不可用时作为降级方案，
    /// 保证界面仍能启动（历史不会保留）。
    ///
    /// 配置路径仍走默认位置：内存库只是存储降级，
    /// 用户在设置界面改的选项不该跟着丢失。
    pub fn in_memory() -> Self {
        let store = Store::open_in_memory()
            .unwrap_or_else(|_| unreachable!("内存库创建不应失败"));
        Self::build(store, Config::default(), default_config_path())
            .expect("内存库初始化不应失败")
    }

    // ---------- 捕获线程 ----------

    /// 启动后台捕获。重复调用无副作用。
    /// 启动剪贴板监听。
    ///
    /// 若配置里 `capture.enabled == false`（例如命令行 `--no-capture`），
    /// **本方法为空操作**。
    ///
    /// 早前 `App::new` 无条件调用本函数，完全不检查该标志，
    /// 于是 `--no-capture` 这个调试开关**形同虚设**——
    /// 用户以为不会记录剪贴板，实际照常记录。
    /// 检查放在这里而非调用方，是因为本方法是唯一的启动入口，
    /// 放这里才能保证任何调用者都绕不过去。
    /// 剪贴板监听当前是否在运行。
    ///
    /// 两个用途：
    /// - 测试判定「`capture.enabled=false` 时确实没启动监听」——
    ///   否则只能间接观察，测试会写得脆弱
    /// - 托盘集成判断「窗口隐藏后后台是否还在记录」——
    ///   隐藏态必须继续 pump，否则「看不见 = 停止记录」，
    ///   与后台常驻的初衷相反
    pub fn is_capturing(&self) -> bool {
        match &self.backend {
            Backend::Local(l) => l.capture.is_some(),
            // 远端：界面进程看不到捕获线程，只能读同步下来的状态。
            Backend::Remote(_) => self.state.capturing,
        }
    }

    /// 录制快捷键期间暂停 / 恢复全局热键。
    ///
    /// 恢复由持有托盘的一侧按当前配置重新注册。
    pub fn suspend_hotkeys(&mut self, suspend: bool) -> anyhow::Result<()> {
        match &mut self.backend {
            Backend::Local(_) => {
                if suspend {
                    modular_clipboard_platform::tray::suspend_hotkeys();
                } else {
                    // 单进程：托盘就在本进程，按当前配置直接重注册。
                    daemon::rebind_hotkeys(&self.state.config);
                }
                Ok(())
            }
            Backend::Remote(r) => r.unit(modular_clipboard_ipc::Command::SuspendHotkeys(suspend)),
        }
    }


    pub fn start_capture(&mut self) {
        match &mut self.backend {
            Backend::Local(l) => l.start_capture(&mut self.state),
            Backend::Remote(r) => r.set_capture(&mut self.state, true),
        }
    }


    /// 停止捕获。
    pub fn stop_capture(&mut self) {
        match &mut self.backend {
            Backend::Local(l) => l.stop_capture(&mut self.state),
            Backend::Remote(r) => r.set_capture(&mut self.state, false),
        }
    }


    /// 抑制接下来的若干次捕获，防止自写回被记录。
    ///
    /// 用时间戳而非一次性标志：UI 每帧都会调用 `pump`，
    /// 布尔标志会在下一次 pump（16ms 后）就被清除，起不到抑制作用。
    /// 时间戳由捕获线程自行判断窗口是否过期，两侧无需同步。
    ///
    /// `pub`：设置界面的「一键 AI 配对提示词」等写剪贴板的入口
    /// 也要在写前调用（同 [`Self::copy_item`] 的抑制规则）。
    pub fn suppress_self_capture(&mut self) {
        match &mut self.backend {
            Backend::Local(l) => {
                l.last_self_write
                    .store(now_ms() as u64, Ordering::Relaxed)
            }
            // 远端：写剪贴板的是常驻进程，抑制必须在**它那边**武装。
            // 在界面进程里记时间戳没有用——捕获线程看不见它。
            Backend::Remote(r) => {
                if let Err(e) = r.suppress_self_capture() {
                    tracing::warn!(%e, "通知常驻进程抑制自写回失败");
                }
            }
        }
    }


    // ---------- 事件处理 ----------

    /// 排空后台事件并处理。返回是否有界面需要重绘。
    pub fn pump(&mut self) -> bool {
        match &mut self.backend {
            Backend::Local(l) => l.pump(&mut self.state),
            Backend::Remote(r) => r.pump(&mut self.state),
        }
    }


    /// 处理一条捕获内容：过滤 → 去重 → 入库。
    ///
    /// 返回是否产生了界面变更。
    ///
    /// 指纹在**本线程**就地计算：供测试与同步调用路径使用。捕获线程的
    /// 热路径走 [`Self::ingest_with_fingerprint`]（PF-2）——那边算哈希
    /// 不占 UI 线程预算。
    pub fn ingest(&mut self, payload: CapturedPayload) -> bool {
        match &mut self.backend {
            Backend::Local(l) => {
                let hash = fingerprint(&payload.bytes);
                l.ingest_with_fingerprint(&mut self.state, payload, hash)
            }
            // 远端：指纹由常驻进程算（只有它握着库与捕获配置）。
            Backend::Remote(r) => r.ingest(&mut self.state, payload),
        }
    }


    /// 同 [`Self::ingest`]，但指纹由调用方预计算（捕获线程路径，PF-2）。
    pub fn ingest_with_fingerprint(&mut self, payload: CapturedPayload, hash: String) -> bool {
        match &mut self.backend {
            Backend::Local(l) => l.ingest_with_fingerprint(&mut self.state, payload, hash),
            Backend::Remote(r) => {
                // 远端侧自己重算指纹，传入值仅作调试线索。
                let _ = hash;
                r.ingest(&mut self.state, payload)
            }
        }
    }


    /// 重新读取列表。列表量级在数千条内，直接全量读取比增量维护更不容易出错。
    pub fn reload_list(&mut self) {
        match &mut self.backend {
            Backend::Local(l) => l.reload_list(&mut self.state),
            Backend::Remote(r) => {
                r.reload_list(&mut self.state);
            }
        }
    }


    /// 搜索。
    pub fn search(&mut self, query: String) {
        self.state.query = query.clone();
        match &mut self.backend {
            Backend::Local(l) => l.reload_list(&mut self.state),
            Backend::Remote(r) => {
                r.search(&mut self.state, query);
            }
        }
    }


    /// 设置分组过滤。
    pub fn filter_group(&mut self, filter: Option<GroupFilter>) {
        self.state.group_filter = filter;
        match &mut self.backend {
            Backend::Local(l) => l.reload_list(&mut self.state),
            Backend::Remote(r) => {
                r.filter_group(&mut self.state, filter);
            }
        }
    }


    /// 把条目写回系统剪贴板。
    pub fn copy_item(&mut self, id: EntryId) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.copy_item(&mut self.state, id),
            Backend::Remote(r) => r.copy_item(id),
        }
    }


    /// 读取条目载荷字节（图片缩略图等 UI 预览用途）。
    ///
    /// 分离模式下走 IPC（`Command::LoadPayload` → `Response::Bytes`）；
    /// 单进程模式直接读库。两条路径都可能失败（条目不存在 / 载荷已被
    /// 淘汰 / 通道故障），调用方按「没有缩略图」降级即可。
    pub fn load_payload_bytes(&mut self, id: EntryId) -> Result<Vec<u8>> {
        match &mut self.backend {
            Backend::Local(l) => l.load_payload_bytes(id),
            Backend::Remote(r) => r.load_payload_bytes(id),
        }
    }


    /// 复制并自动粘贴到前台窗口。
    pub fn copy_and_paste(&mut self, id: EntryId) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.copy_and_paste(&mut self.state, id),
            // 自动粘贴要向前台窗口发按键，那是常驻进程更适合做的事
            // （它不需要抢焦点）。界面侧暂不支持。
            Backend::Remote(_) => anyhow::bail!("界面进程不支持自动粘贴"),
        }
    }


    /// 执行联动动作。
    pub fn run_action(&mut self, item: &ClipItem, proposal: &action::ActionProposal) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.run_action(&mut self.state, item, proposal),
            Backend::Remote(_) => anyhow::bail!("界面进程不支持联动动作"),
        }
    }


    /// 计算某条目的可用动作。
    pub fn proposals_for(&self, item: &ClipItem) -> Vec<action::ActionProposal> {
        match &self.backend {
            Backend::Local(l) => l.proposals_for(&self.state, item),
            // 联动动作尚未接入界面（见 `run_action`）。给空集而不是 panic：
            // 建议动作只是锦上添花，不该让它拖垮整帧。
            Backend::Remote(_) => Vec::new(),
        }
    }


    // ---------- 条目操作 ----------

    pub fn delete_item(&mut self, id: EntryId) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.delete_item(&mut self.state, id),
            Backend::Remote(r) => {
                r.delete_item(&mut self.state, id);
                Ok(())
            }
        }
    }


    pub fn toggle_pin(&mut self, id: EntryId) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.toggle_pin(&mut self.state, id),
            Backend::Remote(r) => {
                r.toggle_pin(&mut self.state, id);
                Ok(())
            }
        }
    }


    /// 就地更新条目内容（编辑落地，2026-10-21）。
    ///
    /// 文本类：`bytes` = UTF-8 原文，`dims` 传 `None`；
    /// 图片类：`bytes` = 裸 RGBA8 像素，`dims` 必须给（arboard 链路的
    /// 尺寸无法从字节嗅探，见 [`CapturedPayload::dims`]）。
    ///
    /// 预览文案由本方法按 [`build_preview`] 同款规则重建（与捕获入库
    /// 完全同源）——调用方（界面）只交字节，不交文案，避免两处各写
    /// 一份截断规则后漂移。
    ///
    /// 远端模式下走 IPC：常驻进程持有存储，编辑必须落在那边的库里。
    pub fn update_content(
        &mut self,
        id: EntryId,
        kind: ClipKind,
        bytes: Vec<u8>,
        dims: Option<(u32, u32)>,
    ) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => {
                let preview = build_preview(&CapturedPayload {
                    kind,
                    bytes: bytes.clone(),
                    dims,
                    html: None,
                    text: None,
                    source_app: String::new(),
                    files: Vec::new(),
                });
                l.store.update_content(id, kind, preview, bytes, dims)?;
                l.reload_list(&mut self.state);
                Ok(())
            }
            Backend::Remote(r) => {
                r.unit(modular_clipboard_ipc::Command::UpdateContent { id, kind, bytes, dims })?;
                r.reload_list(&mut self.state);
                Ok(())
            }
        }
    }


    pub fn move_to_group(&mut self, id: EntryId, group: Option<i64>) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.move_to_group(&mut self.state, id, group),
            Backend::Remote(_) => anyhow::bail!("界面进程不支持移动分组"),
        }
    }


    pub fn create_group(&mut self, name: &str) -> Result<i64> {
        match &mut self.backend {
            Backend::Local(l) => l.create_group(&mut self.state, name),
            Backend::Remote(_) => anyhow::bail!("界面进程不支持创建分组"),
        }
    }


    pub fn clear_all(&mut self) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.clear_all(&mut self.state),
            Backend::Remote(r) => {
                r.clear_all(&mut self.state);
                Ok(())
            }
        }
    }


    /// 保存配置到磁盘。
    /// 保存配置到磁盘。
    ///
    /// # `--no-capture` 覆盖必须跳过
    ///
    /// 命令行 `--no-capture` 会把 `capture.enabled` 置 false
    /// 以便调试时不动真实剪贴板。但这个覆盖是**临时**的——
    /// 若被写进 config.json，用户下次启动监听就是关的，
    /// **且没有任何提示**（这会静默破坏产品的核心功能）。
    ///
    /// 故用 [`Self::set_capture_override`] 标记的临时值
    /// 在保存时还原为磁盘上的真实配置。
    pub fn save_config(&mut self) -> Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.save_config(&mut self.state, self.capture_override),
            // 远端：配置由界面侧编辑、交常驻进程落盘——它才是
            // config.json 与数据库的持有者（避免「读 A 写 B」）。
            // 应用响应里的快照：热键注册失败信息就在里面。
            Backend::Remote(r) => r.save_config(&mut self.state),
        }
    }


    /// 标记当前 `capture.enabled` 是命令行临时覆盖，保存配置时应跳过。
    pub fn set_capture_override(&mut self, is_override: bool) {
        self.capture_override = is_override;
    }


    /// 把一段文本写回系统剪贴板（自带自写回抑制）。
    ///
    /// 供设置界面「一键 AI 配对提示词」等入口使用——与 [`Self::copy_item`]
    /// 同规则：抑制必须在写之前武装，防止捕获线程把自己写的内容再记一条。
    pub fn write_clipboard_text(&mut self, text: &str) -> anyhow::Result<()> {
        match &mut self.backend {
            Backend::Local(l) => l.write_clipboard_text(&mut self.state, text),
            Backend::Remote(r) => r.write_clipboard_text(text),
        }
    }


    /// 在状态栏显示一条消息，3 秒后过期。
    pub fn notify(&mut self, msg: String) {
        // 通知只在本进程的状态栏显示：它是界面的瞬时反馈，
        // 不该跨进程（常驻进程里没有状态栏可画）。
        set_notice(&mut self.state, msg);
    }

}
impl LocalBackend {
    fn start_capture(&mut self, state: &mut AppState) {
        if self.capture.is_some() {
            return;
        }
        if !state.config.capture.enabled {
            tracing::info!("capture.enabled = false，不启动剪贴板监听");
            return;
        }
        let running = Arc::new(AtomicBool::new(true));
        let counted = Arc::new(AtomicU64::new(0));
        let interval = state.config.capture.poll_interval_ms.max(50);
        // 捕获线程要用它算抑制窗口（见 `self_write_guard_ms`）。
        let interval_ms = interval as i64;
        let last_self_write = self.last_self_write.clone();

        // rtrb 的 Producer 不可克隆，因此在启动捕获时新建队列，
        // 把 producer 移入线程，consumer 留给本线程消费。
        let (mut tx, rx) = rtrb::RingBuffer::new(EVENT_QUEUE_CAPACITY);
        self.rx = Some(rx);

        let r = running.clone();
        let c = counted.clone();
        let spawned = std::thread::Builder::new()
            .name("tiez-capture".into())
            .spawn(move || {
                let mut reader = match ClipboardReader::new() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(%e, "无法访问剪贴板，捕获线程退出");
                        return;
                    }
                };
                // 启动时先对齐基线，避免把退出前遗留内容记成新复制。
                reader.reset_baseline();

                while r.load(Ordering::Relaxed) {
                    // 自写回抑制：应用刚把历史条目放回剪贴板时，
                    // 那次变化不是用户的复制行为，必须忽略。
                    //
                    // CAUTION: 必须用饱和减法，不能写 `now - last`：
                    // `now_ms()` 取的是**墙钟**，NTP 校时或用户手动改时间
                    // 会让它瞬间小于 `last_self_write`。无符号减法在此
                    // 下溢成一个接近 2^64 的巨值，于是 `since < 900`
                    // 恒为 false——抑制**永久失效**，此后每次把历史写回
                    // 剪贴板都会被当成用户的复制重新记一条。
                    // 饱和减法在时钟回拨时得 0（仍在抑制窗口内），
                    // 时钟前移得最大值（窗口已过），两种方向都安全。
                    let now = now_ms() as i64;
                    let last = last_self_write.load(Ordering::Relaxed) as i64;

                    // 序列号变化先**窥探**不提交（DF-05）：剪贴板可能正被
                    // 其他进程短暂持有（Office OLE、远程桌面、VMware 同步），
                    // 此刻读取必失败。若像旧实现那样先推进基线再读，失败的
                    // 那次变化就永久丢了——用户少一条历史记录且无感知。
                    //
                    // 三种分支（判据统一走 capture_should_read）：
                    // 1. 抑制期内：推进基线**吸收**这次变化（自写回语义），
                    //    不读取；
                    // 2. 非抑制但剪贴板被占用：什么都不做，下一轮重试；
                    // 3. 其余：提交基线并读取，成功则投递。
                    if reader.peek_changed() {
                        let past_guard =
                            capture_should_read(true, now, last, interval_ms);
                        if !past_guard {
                            reader.commit_sequence();
                        } else if modular_clipboard_capture::clipboard_held_by_other() {
                            tracing::debug!("剪贴板正被其他进程持有，本轮跳过，下轮重试");
                        } else {
                            reader.commit_sequence();
                            if let Some(payload) = reader.read() {
                                // 指纹在捕获线程算（PF-2，见 Signal::Captured
                                // 的文档）：UI 线程只做 DB 写入。
                                let hash = fingerprint(&payload.bytes);
                                c.fetch_add(1, Ordering::Relaxed);
                                tracing::debug!(kind = ?payload.kind, len = payload.bytes.len(), "捕获线程读取到内容");
                                if tx.push(Signal::Captured { payload, hash }).is_err() {
                                    // 队列满：丢弃这次事件，不能阻塞剪贴板读取。
                                    tracing::debug!("事件队列已满，丢弃一条捕获");
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(interval));
                }
                tracing::debug!("捕获线程退出");
            });

        if spawned.is_err() {
            set_notice(state, modular_clipboard_core::i18n::t(("无法创建捕获线程", "Failed to start the capture thread")).into());
            return;
        }

        state.capturing = true;
        self.capture = Some(CaptureHandle {
            running,
            captured_count: counted,
        });
        tracing::info!(interval_ms = interval, "捕获线程已启动");
        }

    fn stop_capture(&mut self, state: &mut AppState) {
        self.capture.take();
        state.capturing = false;
        }

    fn pump(&mut self, state: &mut AppState) -> bool {
        // 先把队列排空到本地缓冲，再逐条处理。
        // 若直接在循环里持有 rx 的借用，处理事件时又调用 self.ingest()
        // 会形成对 self 的重复可变借用。
        let mut batch: Vec<(CapturedPayload, String)> = Vec::new();
        if let Some(rx) = self.rx.as_mut() {
            while let Ok(sig) = rx.pop() {
                match sig {
                    Signal::Captured { payload, hash } => batch.push((payload, hash)),
                }
            }
        }
        let mut dirty = false;
        for (payload, hash) in batch {
            if self.ingest_with_fingerprint(state, payload, hash) {
                dirty = true;
            }
        }
        dirty
        }

    fn ingest_with_fingerprint(
        &mut self,
        state: &mut AppState,
        payload: CapturedPayload,
        hash: String,
    ) -> bool {
        let cfg = &state.config;

        if !cfg.capture.enabled {
            state.stats.duplicate += 0; // 不计入丢弃统计
            return false;
        }

        // 空白内容直接忽略：用户按了 Ctrl+C 但没选中任何东西。
        if payload.bytes.iter().all(|b| b.is_ascii_whitespace()) {
            state.stats.empty += 1;
            tracing::debug!("捕获被过滤：空白内容");
            return false;
        }

        // 来源程序屏蔽。
        if !payload.source_app.is_empty() && cfg.is_app_blocked(&payload.source_app) {
            state.stats.app_blocked += 1;
            tracing::debug!(app = %payload.source_app, "捕获被过滤：来源程序被屏蔽");
            return false;
        }

        // 密码框启发式：仅在敏感窗口为前台时跳过。
        if cfg.capture.skip_password_fields && modular_clipboard_platform::is_password_field() {
            state.stats.secret_field += 1;
            tracing::debug!(class = %modular_clipboard_capture::foreground_class_name(), "捕获被过滤：疑似密码框");
            return false;
        }

        // 指纹由参数传入（捕获线程预计算，PF-2）；`ingest` 包装器会
        // 就地算好后转调本函数。
        let preview = build_preview(&payload);

        // 去重：历史中已有相同内容（无论何时复制的）就不再新增，
        // 只把原条目刷新时间、顶到列表顶部——「复制相同内容」不应
        // 产生一条又一条的重复记录。
        if cfg.capture.dedup {
            match self.store.find_by_hash(&hash) {
                Ok(Some(existing)) => {
                    if self.store.bump_to_top(existing).is_ok() {
                        self.reload_list(state);
                        state.stats.duplicate += 1;
                        return true;
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(%e, "去重查询失败，按新条目处理"),
            }
        }

        let source_app = if cfg.capture.track_source_app {
            payload.source_app.clone()
        } else {
            String::new()
        };

        let item = ClipItem::new(payload.kind, hash, preview, source_app);

        // 图片尺寸透传：载荷是裸 RGBA8（arboard 解码 CF_DIBV5 的产物），
        // store 侧的格式嗅探对它无效，只能由捕获侧给（见 CapturedPayload::dims）。
        //
        // 文件型条目（`ClipKind::Files`）走不了这条链路——它的载荷是
        // **路径列表**，图在磁盘上。若第一个文件是图片，这里顺手读一下
        // 文件头补上尺寸：UI 的列表行高（`row_height_for`）只认
        // `payload.dims`，缺了它图片行会先按文本行排、解码出真实尺寸后
        // 又跳一次高度（列表抖动）。
        // 先取字段再移动 bytes，避免部分移动。
        let dims = payload
            .dims
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or_else(|| first_image_file_dims(&payload.files));
        match self.store.insert_with_dims(item, payload.bytes, dims) {
            Ok(saved) => {
                tracing::debug!(id = saved.id, kind = ?saved.kind, "新条目已入库");
                self.store.touch(saved.id).ok();
                // 写入后立刻执行保留策略，避免高频复制时库无限增长。
                if let Err(e) = self.store.enforce_retention(&state.config.storage) {
                    tracing::warn!(%e, "保留策略执行失败");
                }
                self.reload_list(state);
                true
            }
            Err(e) => {
                tracing::error!(%e, "入库失败");
                set_notice(state, format!("{} {e}", modular_clipboard_core::i18n::t(("保存失败:", "Save failed:"))));
                false
            }
        }
        }

    fn reload_list(&mut self, state: &mut AppState) {
        let result = match state.query.trim() {
            "" => {
                let filter = state.group_filter;
                self.store.list(PAGE_LIMIT, 0, filter)
            }
            q => self.store.search(q, PAGE_LIMIT),
        };
        match result {
            Ok(items) => state.items = items,
            Err(e) => {
                tracing::error!(%e, "读取列表失败");
                state.items.clear();
            }
        }
        // 列表整体换过：行高缓存（绘制层按本版本号判断失效）必须知道。
        state.list_version = state.list_version.wrapping_add(1);
        state.groups = self.store.list_groups().unwrap_or_default();
        }

    fn copy_item(&mut self, _state: &mut AppState, id: EntryId) -> Result<()> {
        let Some(item) = self.store.get(id)? else {
            anyhow::bail!("条目不存在");
        };
        let bytes = self.store.load_payload(&item)?;
        let mut reader = ClipboardReader::new()?;


        // CAUTION: 抑制必须在**写之前**武装，不能等写完再标。
        //
        // 早前 `suppress_self_capture()` 在 `match item.kind` **之后**，
        // 于是从「剪贴板已被改写（序列号 +1）」到「抑制生效」之间存在
        // 一段窗口（Win32 调用 + 一次 `store.touch` 的 SQLite 写）。
        // 捕获线程若在这段里醒来，看到的 `last_self_write` 仍是旧值
        // ⇒ 判定为「用户的新复制」⇒ **把自己刚写回的内容又记一条**。
        //
        // 默认 `poll_interval_ms = 300`，该窗口通常 1~5ms，单次命中率
        // 约 0.3%~1.7% —— 一天点几十次历史条目就会稳定复现。
        //
        // 现象：从历史里点一条复制出去，列表顶部立刻多出一条内容
        // 相同的记录。
        self.last_self_write
            .store(now_ms() as u64, Ordering::Relaxed);

        match item.kind {
            // Markdown 载荷与 Text 同为 UTF-8 文本：复制的是**源码**
            // （用户从哪复制的就还回什么，渲染只发生在界面显示侧）。
            ClipKind::Text | ClipKind::Html | ClipKind::Markdown => {
                reader.write_text(&String::from_utf8_lossy(&bytes))?;
            }
            ClipKind::Files => {
                let paths: Vec<String> = String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .collect();
                reader.write_files(&paths)?;
            }
            ClipKind::Image => {
                // 需要把原始图片字节解码为 RGBA 再写回。
                let (rgba, w, h) = decode_to_rgba(&item, &bytes)?;
                reader.write_image(rgba, w, h)?;
            }
        }

        self.store.touch(id).ok();
        Ok(())
    }

    /// 读取条目载荷字节（UI 图片缩略图用）。
    ///
    /// 与 `copy_item` 的读法相同（`store.get` + `load_payload`），
    /// 差别只是不写剪贴板、不武装自写回抑制——纯读取。
    fn load_payload_bytes(&mut self, id: EntryId) -> Result<Vec<u8>> {
        let Some(item) = self.store.get(id)? else {
            anyhow::bail!("条目不存在");
        };
        self.store.load_payload(&item)
    }

    fn copy_and_paste(&mut self, state: &mut AppState, id: EntryId) -> Result<()> {
        self.copy_item(state, id)?;
        // 「仅粘贴并提示」：剪贴板写好就收工，**不发按键**——粘到哪里
        // 由用户自己按 Ctrl+V 决定，绝不猜目标窗口（多屏/切窗口快时
        // 猜错会把内容粘到无关程序里）。发通知让用户知道已经就绪。
        if state.config.paste_previous_mode
            == modular_clipboard_core::config::PastePreviousMode::NotifyOnly
        {
            set_notice(
                state,
                modular_clipboard_core::i18n::t((
                    "已复制到剪贴板，按 Ctrl+V 粘贴",
                    "Copied — press Ctrl+V to paste",
                ))
                .to_string(),
            );
            return Ok(());
        }
        // 目标窗口在**写剪贴板的这一刻**抓住：此刻前台还是用户按热键时
        // 正在用的程序。等 250ms 延迟结束再查（auto_paste 内部的老做法）
        // 用户可能已经切走，按键会粘到错误的窗口上。
        let target = modular_clipboard_platform::foreground_window();
        // 前台是我们自己的界面窗口时不发按键：发过去只会把它激活
        // （用户看到「软件窗口被弹到前面」而不是粘贴——正是实测反馈的
        // 现象）。剪贴板此时已经写好，用户手动 Ctrl+V 即可。
        if Some(target) == modular_clipboard_ipc::ui_window_hwnd() {
            tracing::debug!("前台是本程序界面窗口，跳过自动发送按键（剪贴板已写入）");
            return Ok(());
        }
        // 按键发送放后台线程：auto_paste 内含 250ms 等待（让目标窗口
        // 完成 OLE 握手），在常驻进程主循环里同步跑会连带停掉托盘
        // 事件与 IPC 250ms。
        std::thread::spawn(move || {
            if let Err(e) =
                modular_clipboard_platform::auto_paste_to(Some(target), Duration::from_millis(250))
            {
                tracing::warn!(%e, "自动粘贴失败");
            }
        });
        Ok(())
    }

    fn run_action(
        &mut self,
        state: &mut AppState,
        item: &ClipItem,
        proposal: &action::ActionProposal,
    ) -> Result<()> {
        action::dispatch(&proposal.plan)?;
        self.store.touch(item.id).ok();
        set_notice(state, format!("{}{}", modular_clipboard_core::i18n::t(("已执行：", "Done: ")), proposal.label));
        Ok(())
        }

    fn proposals_for(&self, state: &AppState, item: &ClipItem) -> Vec<action::ActionProposal> {
        action::propose(&self.registry, item, &state.config.rules)
        }

    fn delete_item(&mut self, state: &mut AppState, id: EntryId) -> Result<()> {
        self.store.delete(id)?;
        if state.selected == Some(id) {
            state.selected = None;
        }
        self.reload_list(state);
        Ok(())
        }

    fn toggle_pin(&mut self, state: &mut AppState, id: EntryId) -> Result<()> {
        if let Some(item) = self.store.get(id)? {
            let new_state = !item.pinned;
            self.store.set_pinned(id, new_state)?;
            self.reload_list(state);
        }
        Ok(())
        }

    fn move_to_group(
        &mut self,
        state: &mut AppState,
        id: EntryId,
        group: Option<i64>,
    ) -> Result<()> {
        self.store.move_to_group(id, group)?;
        self.reload_list(state);
        Ok(())
        }

    fn create_group(&mut self, state: &mut AppState, name: &str) -> Result<i64> {
        let id = self.store.create_group(name, DEFAULT_GROUP_COLOR)?;
        self.reload_list(state);
        Ok(id)
        }

    fn clear_all(&mut self, state: &mut AppState) -> Result<()> {
        self.store.clear_all()?;
        state.selected = None;
        self.reload_list(state);
        Ok(())
        }

    fn save_config(&mut self, state: &mut AppState, capture_override: bool) -> Result<()> {
        if let Some(path) = self.config_path.clone() {
            // 临时覆盖不落盘：读回磁盘上的真实值。
            let persisted = capture_override
                .then(|| std::fs::read_to_string(&path).ok())
                .flatten()
                .and_then(|raw| serde_json::from_str::<Config>(&raw).ok());
            if let Some(real) = persisted
                && real.capture.enabled != state.config.capture.enabled {
                    tracing::info!(
                        persisted = real.capture.enabled,
                        "不把 --no-capture 临时覆盖写入 config.json"
                    );
                    state.config.capture.enabled = real.capture.enabled;
                }
            state.config.save(&path)?;
            self.store.apply_config(&state.config);
        }
        Ok(())
        }

    fn write_clipboard_text(
        &mut self,
        _state: &mut AppState,
        text: &str,
    ) -> anyhow::Result<()> {
        self.last_self_write
            .store(now_ms() as u64, Ordering::Relaxed);
        let mut reader = ClipboardReader::new()?;
        reader.write_text(text)
        }

}

impl RemoteBackend {
    /// 把常驻进程带回的新列表写进界面状态。返回状态是否变了。
    fn apply(&mut self, state: &mut AppState, resp: Response) -> bool {
        match resp {
            Response::Snapshot(s) => {
                self.known = s.version;
                apply_snapshot(s, state);
                true
            }
            Response::Ok | Response::Unchanged | Response::Bytes(_) => false,
            Response::VersionMismatch { server } => {
                tracing::error!(server, "与常驻进程的协议版本不一致");
                false
            }
            Response::Error(msg) => {
                tracing::error!(%msg, "常驻进程返回错误");
                false
            }
        }
    }

    /// 发一条**会改变列表**的命令，并就地应用结果。
    fn mutate(&mut self, state: &mut AppState, cmd: Command) -> bool {
        match self.client.command(cmd) {
            Ok(resp) => self.apply(state, resp),
            Err(e) => {
                // 通道断了（常驻进程退出）比业务失败严重得多。
                // 界面不崩，但要把自己标记成「失联」——继续跑下去
                // 只会让用户对着一扇不响应的窗口发呆。
                tracing::error!(%e, "向常驻进程发送命令失败");
                self.dead = true;
                false
            }
        }
    }

    /// 发一条**没有返回数据**的命令。
    fn unit(&mut self, cmd: Command) -> anyhow::Result<()> {
        self.client.command(cmd).map(|_| ())
    }

    /// 取条目载荷字节（图片缩略图）。
    ///
    /// 与 `mutate` 不同：`Response::Bytes` 不改变列表状态，
    /// `apply` 对它本就是空操作，这里直接取字节。
    fn load_payload_bytes(&mut self, id: EntryId) -> anyhow::Result<Vec<u8>> {
        self.client.load_payload(id)
    }

    fn reload_list(&mut self, state: &mut AppState) -> bool {
        self.mutate(state, Command::ReloadList)
    }

    fn search(&mut self, state: &mut AppState, query: String) -> bool {
        self.mutate(state, Command::Search(query))
    }

    fn filter_group(&mut self, state: &mut AppState, filter: Option<GroupFilter>) -> bool {
        self.mutate(state, Command::FilterGroup(filter))
    }

    fn ingest(&mut self, state: &mut AppState, payload: CapturedPayload) -> bool {
        self.mutate(state, Command::Ingest(payload))
    }

    fn delete_item(&mut self, state: &mut AppState, id: EntryId) -> bool {
        let changed = self.mutate(state, Command::DeleteItem(id));
        if state.selected == Some(id) {
            state.selected = None;
        }
        changed
    }

    fn toggle_pin(&mut self, state: &mut AppState, id: EntryId) -> bool {
        self.mutate(state, Command::TogglePin(id))
    }

    fn clear_all(&mut self, state: &mut AppState) -> bool {
        let changed = self.mutate(state, Command::ClearAll);
        state.selected = None;
        changed
    }

    /// 每帧一次：问常驻进程「有新内容吗」。
    ///
    /// 版本没变时回答是 `Unchanged`（几十字节），所以这可以每帧跑。
    fn pump(&mut self, state: &mut AppState) -> bool {
        match self.client.sync(self.known) {
            Ok(Some(s)) => {
                self.known = s.version;
                apply_snapshot(s, state);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::error!(%e, "与常驻进程同步失败");
                self.dead = true;
                false
            }
        }
    }

    /// 启停剪贴板监听。
    ///
    /// 与本地后端同规则：配置里关着的时候，「启动」是空操作——
    /// 否则用户在设置里关掉监听、界面一重启就又把它打开了。
    fn set_capture(&mut self, state: &mut AppState, on: bool) {
        if on && !state.config.capture.enabled {
            tracing::info!("capture.enabled = false，不启动剪贴板监听");
            return;
        }
        match self.client.command(Command::SetCaptureEnabled(on)) {
            Ok(_) => state.capturing = on,
            Err(e) => tracing::error!(%e, "切换剪贴板监听失败"),
        }
    }

    fn suppress_self_capture(&mut self) -> anyhow::Result<()> {
        self.unit(Command::SuppressSelfCapture)
    }

    fn copy_item(&mut self, id: EntryId) -> anyhow::Result<()> {
        self.unit(Command::CopyItem(id))
    }

    fn write_clipboard_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.unit(Command::WriteClipboardText(text.to_string()))
    }

    fn save_config(&mut self, state: &mut AppState) -> anyhow::Result<()> {
        // 用 `command` + `apply` 而不是 `unit`：常驻进程的响应里带着
        // **刚更新的快照**（含快捷键注册失败信息），就地应用才能让界面
        // 立刻显示「已被占用」；丢掉响应就只能等下一次列表变化顺带
        // 带过来。错误照旧上抛（保存失败要提示用户）。
        let cmd = Command::SaveConfig(state.config.clone());
        match self.client.command(cmd) {
            Ok(resp) => {
                self.apply(state, resp);
                Ok(())
            }
            Err(e) => {
                self.dead = true;
                Err(e)
            }
        }
    }
}


#[cfg(test)]
mod tests {

    /// 抑制窗口必须**始终大于**轮询间隔，否则间隔调大时抑制失效。
    ///
    /// # 为什么这条重要
    ///
    /// 设自写发生在 T，捕获线程上一轮在 `t0 ∈ (T-I, T]` 唤醒，下一轮
    /// 在 `t0+I`，`since = t0 + I - T`；最坏情形 `t0 → T` 时
    /// `since → I`。所以窗口必须 **> I** 才能保证自写后的第一轮落在
    /// 窗口内。
    ///
    /// 早前用常量 900，而 `poll_interval_ms` 无上限 —— 用户把间隔调到
    /// 1000ms 就让 `since ≥ 900` 成立 ⇒ 抑制彻底失效 ⇒ 每次从历史写回
    /// 都多记一条重复记录。
    #[test]
    fn self_write_guard_exceeds_poll_interval() {
        // 只夹下限（50）之后的所有合法间隔值。
        for interval in [50i64, 100, 300, 500, 900, 1000, 2000, 5000] {
            let guard = self_write_guard_ms(interval);
            assert!(
                guard > interval,
                "间隔 {interval}ms 时抑制窗口 {guard}ms 必须 > 间隔，                 否则「自写后的第一轮」会落在窗口外 ⇒ 抑制失效"
            );
        }
    }

    /// 间隔小于默认下限时，窗口退回下限（不必无限增大）。
    #[test]
    fn self_write_guard_floors_at_minimum() {
        assert_eq!(
            self_write_guard_ms(50),
            SELF_WRITE_GUARD_MS_FLOOR,
            "小间隔时窗口应取下限，避免过度吞掉用户的真实复制"
        );
    }

    use super::*;

    fn payload(kind: ClipKind, text: &str) -> CapturedPayload {
        CapturedPayload {
            kind,
            bytes: text.as_bytes().to_vec(),
            html: None,
            text: Some(text.to_string()),
            source_app: "test".into(),
            files: Vec::new(),
            dims: None,
        }
    }

    #[test]
    fn preview_for_files_summarizes_count() {
        let p = CapturedPayload {
            kind: ClipKind::Files,
            bytes: b"a\nb\nc".to_vec(),
            html: None,
            text: Some("a\nb\nc".into()),
            source_app: "x".into(),
            files: vec!["C:\\a.txt".into(), "C:\\b.txt".into(), "C:\\c.txt".into()],
            dims: None,
        };
        let preview = build_preview(&p);
        assert!(preview.contains("3 个文件"), "实际: {preview}");
    }

    #[test]
    fn preview_for_image_reports_size_or_bytes() {
        let p = CapturedPayload {
            kind: ClipKind::Image,
            bytes: vec![0u8; 32],
            html: None,
            text: None,
            source_app: "x".into(),
            files: Vec::new(),
            dims: None,
        };
        assert!(build_preview(&p).starts_with("图片"));
    }

    /// 裸 RGBA8 载荷（无文件头）也能得到尺寸文案——这正是修复前
    /// 「图片显示开启后还是显示 图片 N 字节」的回归测试：
    /// 尺寸必须来自捕获侧透传的 `dims`，而不是从字节里猜。
    #[test]
    fn preview_for_image_uses_captured_dims_for_raw_rgba() {
        let w = 64u32;
        let h = 48u32;
        let p = CapturedPayload {
            kind: ClipKind::Image,
            bytes: vec![0x80u8; (w * h * 4) as usize],
            html: None,
            text: None,
            source_app: "x".into(),
            files: Vec::new(),
            dims: Some((w, h)),
        };
        assert_eq!(build_preview(&p), "图片 64×48");
    }

    #[test]
    fn time_formatting_is_relative() {
        let now = 1_000_000_000i64;
        assert_eq!(format_time(now - 30_000, now), "刚刚");
        assert_eq!(format_time(now - 300_000, now), "5 分钟前");
        assert_eq!(format_time(now - 7_200_000, now), "2 小时前");
        assert_eq!(format_time(now - 172_800_000, now), "2 天前");
    }

    #[test]
    fn text_preview_is_verbatim() {
        assert_eq!(build_preview(&payload(ClipKind::Text, "abc")), "abc");
    }

    /// 独占的临时目录名（进程 ID + 原子序号）。
    ///
    /// 不能用 `now_ms()`：毫秒精度下并发测试会拿到同名目录、互相删除。
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "modular-clipboard-svc-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    /// `Service` 必须把数据库开在 `--data-dir` 指定目录下。
    ///
    /// 回归测试：此前 `Service::new` 无条件 `Store::open_default()`，
    /// `--data-dir` 传了也无效。
    #[test]
    fn service_opens_store_in_given_data_dir() {
        let dir = unique_dir("open");
        let mut svc = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();

        let p = payload(ClipKind::Text, "in data dir");
        assert!(svc.ingest(p), "入库应成功");
        svc.stop_capture();

        assert!(
            dir.join("history.db").is_file(),
            "数据库应在 {} 下",
            dir.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置写入必须跟着 `--data-dir` 走。
    ///
    /// 若 `save_config` 仍写 `%APPDATA%`，用户在隔离目录里改的设置
    /// 会写进真实配置——这正是 `--no-capture` 覆盖保护要防的那类事故。
    #[test]
    fn save_config_writes_into_given_data_dir() {
        let dir = unique_dir("save");
        let mut svc = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();
        svc.state.config.capture.enabled = true;
        svc.save_config().unwrap();

        let cfg = dir.join(CONFIG_FILE_NAME);
        assert!(cfg.is_file(), "配置应写到 {} 下", cfg.display());
        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(on_disk.capture.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **默认启动路径也必须能落盘**（不传 `--data-dir`）。
    ///
    /// # 为什么这条重要
    ///
    /// 早前 `with_data_dir` 把 `config_path` 写成
    /// `data_dir.map(|d| d.join(CONFIG_FILE_NAME))` —— `None` 时就是
    /// `None`，`save_config()` 于是静默什么都不做。
    ///
    /// 而**所有**双击启动的用户都走 `data_dir = None` 这条路径，
    /// 于是设置改了重启就没、窗口位置每次回默认。
    ///
    /// 现有 `save_config_writes_into_given_data_dir` 只覆盖了
    /// `Some(dir)` 分支，所以这条 bug 一直没被测到 —— 这是典型的
    /// 「测试只覆盖 happy path 的那一支」。
    #[test]
    fn default_data_dir_still_has_a_config_path() {
        // 只验证「config_path 有值」，不真的往 %APPDATA% 写文件：
        // 测试不该污染用户的真实配置。
        let svc = Service::with_data_dir(Config::default(), None)
            .expect("默认数据目录应能打开");
        assert!(
            svc.config_path().is_some(),
            "默认启动路径必须有配置写入路径，否则设置永不落盘 \\
             （历史 bug：所有不带 --data-dir 的用户设置重启即丢）"
        );
    }

    /// 「复制去重」开启时：重复复制相同内容不新增条目，
    /// 只把原记录顶到列表顶部（列表条目数不变）。
    #[test]
    fn dedup_collapses_repeated_copies() {
        let dir = unique_dir("dedup-on");
        let mut svc = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();
        assert!(svc.state.config.capture.dedup, "默认应开启复制去重");

        let first = payload(ClipKind::Text, "同一段文字 12345");
        assert!(svc.ingest(first), "首次复制应入库");
        assert_eq!(svc.state.items.len(), 1, "首次复制后列表应有 1 条");

        let again = payload(ClipKind::Text, "同一段文字 12345");
        assert!(svc.ingest(again), "去重命中时也应返回成功（列表已更新）");
        assert_eq!(
            svc.state.items.len(),
            1,
            "开启去重时重复复制不应新增条目"
        );
        assert_eq!(svc.state.stats.duplicate, 1, "应计入 duplicate 统计");

        svc.stop_capture();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「复制去重」关闭时：每次复制都新增一条记录。
    #[test]
    fn dedup_disabled_keeps_each_copy() {
        let dir = unique_dir("dedup-off");
        let mut svc = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();
        svc.state.config.capture.dedup = false;

        assert!(svc.ingest(payload(ClipKind::Text, "重复内容 abc")));
        assert!(svc.ingest(payload(ClipKind::Text, "重复内容 abc")));
        assert_eq!(
            svc.state.items.len(),
            2,
            "关闭去重时重复复制应各记一条"
        );
        assert_eq!(svc.state.stats.duplicate, 0, "关闭去重不应计入 duplicate");

        svc.stop_capture();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--no-capture` 的临时覆盖不能落进 `--data-dir` 指定的配置。
    ///
    /// 两条保护同时生效：目录跟着参数走，且覆盖值不持久化。
    #[test]
    fn capture_override_not_persisted_into_custom_dir() {
        let dir = unique_dir("override");
        // 先落一份 enabled=true 的真实配置。
        let mut seed = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();
        seed.state.config.capture.enabled = true;
        seed.save_config().unwrap();

        // 新会话模拟 `--data-dir X --no-capture`。
        let mut svc = Service::with_data_dir(Config::default(), Some(&dir)).unwrap();
        svc.state.config.capture.enabled = false;
        svc.set_capture_override(true);
        svc.save_config().unwrap();

        let raw = std::fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap();
        let on_disk: Config = serde_json::from_str(&raw).unwrap();
        assert!(
            on_disk.capture.enabled,
            "临时覆盖不应被写入 {}，实际 capture.enabled={}",
            dir.join(CONFIG_FILE_NAME).display(),
            on_disk.capture.enabled
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空白数据目录直接报错，不静默落到默认目录。
    #[test]
    fn service_rejects_blank_data_dir() {
        assert!(
            Service::with_data_dir(Config::default(), Some(std::path::Path::new("  "))).is_err(),
            "空白数据目录必须报错"
        );
    }

    /// 自写回的内容在抑制期结束后**不得**被当成新复制重新捕获。
    ///
    /// 回归场景：用户点击历史条目（`copy_item` 写回剪贴板）后，
    /// 捕获线程必须忽略这次变化。旧实现的捕获循环在抑制期内跳过
    /// `changed()`，序列号基线停留在自写前的值，抑制期结束后的第一轮
    /// 就会把「自写导致的变化」判为新变化——每次写回历史都多记一条
    /// 重复记录。
    ///
    /// 判据落在真实行为上（用真实剪贴板 + 捕获循环的单步决策函数），
    /// 且分三步验证：抑制期内不读、抑制期后不读、用户新复制必读。
    #[test]
    fn self_write_is_not_recaptured_after_suppression() {
        // 模拟捕获线程的 reader：基线停在写回之前。
        let mut reader = ClipboardReader::new().unwrap();

        // 模拟一次 copy_item 的自写回（写剪贴板 ⇒ 序列号 +1）。
        let marker = format!("tiez-self-write-{}", std::process::id());
        {
            let mut writer = ClipboardReader::new().unwrap();
            writer.write_text(&marker).unwrap();
        }
        let last_self_write = now_ms() as i64;

        // 1. 抑制期内（since = 0 < 900）：不读取；
        //    生产循环此时会 commit_sequence 把自写的变化吸收进基线。
        assert!(
            !capture_should_read(
                reader.peek_changed(),
                last_self_write,
                last_self_write,
                300,
            ),
            "抑制期内不应读取剪贴板"
        );
        // 与生产循环一致：抑制期内提交基线（吸收自写导致的变化）。
        reader.commit_sequence();

        // 2. 抑制期结束后（since = 900）：序列号无新变化 ⇒ 仍不读取。
        //    旧实现在这里返回 true——把自写回当成用户新复制（bug）。
        assert!(
            !capture_should_read(
                reader.peek_changed(),
                last_self_write + self_write_guard_ms(300),
                last_self_write,
                300,
            ),
            "自写回的内容在抑制期结束后被重新捕获——每次写回历史都多记一条重复记录"
        );

        // 3. 用户真的复制了新内容：序列号变化 ⇒ 必须读取。
        {
            let mut writer = ClipboardReader::new().unwrap();
            writer.write_text("tiez-new-user-copy").ok();
        }
        assert!(
            capture_should_read(
                reader.peek_changed(),
                last_self_write + self_write_guard_ms(300) + 1,
                last_self_write,
                300,
            ),
            "用户的新复制必须能被捕获线程读到"
        );

        // 清理：写空剪贴板，避免污染其它依赖真实剪贴板的测试。
        ClipboardReader::new().unwrap().write_text("").ok();
    }
}
