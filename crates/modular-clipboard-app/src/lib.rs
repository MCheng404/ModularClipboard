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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use modular_clipboard_capture::ClipboardReader;
use modular_clipboard_core::{CapturedPayload, ClipItem, ClipKind, Config, DropStats, EntryId, now_ms};
use modular_clipboard_store::{GroupFilter, Store, fingerprint};

/// 界面与应用共享的状态。
pub struct AppState {
    pub config: Config,
    pub items: Vec<ClipItem>,
    pub groups: Vec<modular_clipboard_core::Group>,
    pub selected: Option<EntryId>,
    pub query: String,
    /// 当前过滤的分组，`None` 为全部。
    pub group_filter: Option<GroupFilter>,
    pub notice: Option<(String, Instant)>,
    pub stats: DropStats,
    /// 监听是否处于运行状态。
    pub capturing: bool,
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
    Captured(CapturedPayload),
}

/// 应用服务。
/// 自写抑制窗口的**下限**（毫秒）。实际窗口按轮询间隔动态计算，
/// 见 [`self_write_guard_ms`]。
const SELF_WRITE_GUARD_MS_FLOOR: i64 = 900;

pub struct Service {
    pub store: Store,
    pub state: AppState,
    pub registry: action::ActionRegistry,
    /// 事件接收端。`None` 表示捕获未启动。
    ///
    /// rtrb 的 Producer 不可克隆，因此它在启动捕获时被移动进后台线程，
    /// 这里只保留 Consumer。
    rx: Option<rtrb::Consumer<Signal>>,
    capture: Option<CaptureHandle>,
    /// 最近一次自写剪贴板的时刻（Unix 毫秒，0 表示从未写过）。
    last_self_write: Arc<AtomicU64>,
    /// 当前 `capture.enabled` 是否为命令行临时覆盖（如 `--no-capture`）。
    /// 保存配置时会被还原，避免永久改写用户的监听开关。
    capture_override: bool,
    /// 配置落盘路径。`None` 表示无法确定（`save_config` 静默跳过）。
    ///
    /// 存字段而非每次现算：`--data-dir` 生效后配置写在指定目录下，
    /// 现算会退回 `%APPDATA%`，导致「读 A 写 B」。
    config_path: Option<std::path::PathBuf>,
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
            store,
            state: AppState {
                config,
                items,
                groups,
                selected: None,
                query: String::new(),
                group_filter: None,
                notice: None,
                stats: DropStats::default(),
                capturing: false,
            },
            registry: action::ActionRegistry::with_builtins(),
            rx: Some(rx),
            capture: None,
            last_self_write: Arc::new(AtomicU64::new(0)),
            capture_override: false,
            config_path,
        })
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
        self.capture.is_some()
    }

    pub fn start_capture(&mut self) {
        if self.capture.is_some() {
            return;
        }
        if !self.state.config.capture.enabled {
            tracing::info!("capture.enabled = false，不启动剪贴板监听");
            return;
        }
        let running = Arc::new(AtomicBool::new(true));
        let counted = Arc::new(AtomicU64::new(0));
        let interval = self.state.config.capture.poll_interval_ms.max(50);
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

                    // NOTE: 抑制期内**也要**推进序列号基线（`capture_should_read`
                    // 无条件调用 `changed()`）。
                    //
                    // 自写回（`copy_item`）会改变剪贴板序列号；若抑制期内跳过
                    // `changed()`，基线停留在自写前的值，抑制期结束后的第一轮
                    // 就会把「自写导致的变化」当成用户的新复制重新捕获——每次
                    // 把历史条目放回剪贴板都会**多记一条重复记录**。
                    // 抑制期内的用户复制（900ms 窗口内，概率极低）会被吸收进
                    // 基线，这正是抑制语义的本意：抑制期内的一切变化都视为
                    // 自写引起。
                    if capture_should_read(&mut reader, now, last, interval_ms)
                        && let Some(payload) = reader.read()
                    {
                        c.fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(kind = ?payload.kind, len = payload.bytes.len(), "捕获线程读取到内容");
                        if tx.push(Signal::Captured(payload)).is_err() {
                            // 队列满：丢弃这次事件，不能阻塞剪贴板读取。
                            tracing::debug!("事件队列已满，丢弃一条捕获");
                        }
                    }
                    std::thread::sleep(Duration::from_millis(interval));
                }
                tracing::debug!("捕获线程退出");
            });

        if spawned.is_err() {
            self.notify("无法创建捕获线程".into());
            return;
        }

        self.state.capturing = true;
        self.capture = Some(CaptureHandle {
            running,
            captured_count: counted,
        });
        tracing::info!(interval_ms = interval, "捕获线程已启动");
    }

    /// 停止捕获。
    pub fn stop_capture(&mut self) {
        self.capture.take();
        self.state.capturing = false;
    }

    /// 抑制接下来的若干次捕获，防止自写回被记录。
    ///
    /// 用时间戳而非一次性标志：UI 每帧都会调用 `pump`，
    /// 布尔标志会在下一次 pump（16ms 后）就被清除，起不到抑制作用。
    /// 时间戳由捕获线程自行判断窗口是否过期，两侧无需同步。
    fn suppress_self_capture(&self) {
        self.last_self_write
            .store(now_ms() as u64, Ordering::Relaxed);
    }

    // ---------- 事件处理 ----------

    /// 排空后台事件并处理。返回是否有界面需要重绘。
    pub fn pump(&mut self) -> bool {
        // 先把队列排空到本地缓冲，再逐条处理。
        // 若直接在循环里持有 rx 的借用，处理事件时又调用 self.ingest()
        // 会形成对 self 的重复可变借用。
        let mut batch: Vec<CapturedPayload> = Vec::new();
        if let Some(rx) = self.rx.as_mut() {
            while let Ok(sig) = rx.pop() {
                match sig {
                    Signal::Captured(p) => batch.push(p),
                }
            }
        }
        let mut dirty = false;
        for payload in batch {
            if self.ingest(payload) {
                dirty = true;
            }
        }
        dirty
    }

    /// 处理一条捕获内容：过滤 → 去重 → 入库。
    ///
    /// 返回是否产生了界面变更。
    pub fn ingest(&mut self, payload: CapturedPayload) -> bool {
        let cfg = &self.state.config;

        if !cfg.capture.enabled {
            self.state.stats.duplicate += 0; // 不计入丢弃统计
            return false;
        }

        // 空白内容直接忽略：用户按了 Ctrl+C 但没选中任何东西。
        if payload.bytes.iter().all(|b| b.is_ascii_whitespace()) {
            self.state.stats.empty += 1;
            tracing::debug!("捕获被过滤：空白内容");
            return false;
        }

        // 来源程序屏蔽。
        if !payload.source_app.is_empty() && cfg.is_app_blocked(&payload.source_app) {
            self.state.stats.app_blocked += 1;
            tracing::debug!(app = %payload.source_app, "捕获被过滤：来源程序被屏蔽");
            return false;
        }

        // 密码框启发式：仅在敏感窗口为前台时跳过。
        if cfg.capture.skip_password_fields && modular_clipboard_platform::is_password_field() {
            self.state.stats.secret_field += 1;
            tracing::debug!(class = %modular_clipboard_capture::foreground_class_name(), "捕获被过滤：疑似密码框");
            return false;
        }

        let hash = fingerprint(&payload.bytes);
        let preview = build_preview(&payload);

        // 去重：历史中已有相同内容（无论何时复制的）就不再新增，
        // 只把原条目刷新时间、顶到列表顶部——「复制相同内容」不应
        // 产生一条又一条的重复记录。
        if cfg.capture.dedup {
            match self.store.find_by_hash(&hash) {
                Ok(Some(existing)) => {
                    if self.store.bump_to_top(existing).is_ok() {
                        self.reload_list();
                        self.state.stats.duplicate += 1;
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

        match self.store.insert(item, payload.bytes) {
            Ok(saved) => {
                tracing::debug!(id = saved.id, kind = ?saved.kind, "新条目已入库");
                self.store.touch(saved.id).ok();
                // 写入后立刻执行保留策略，避免高频复制时库无限增长。
                if let Err(e) = self.store.enforce_retention(&self.state.config.storage) {
                    tracing::warn!(%e, "保留策略执行失败");
                }
                self.reload_list();
                true
            }
            Err(e) => {
                tracing::error!(%e, "入库失败");
                self.notify(format!("保存失败: {e}"));
                false
            }
        }
    }

    /// 重新读取列表。列表量级在数千条内，直接全量读取比增量维护更不容易出错。
    pub fn reload_list(&mut self) {
        let result = match self.state.query.trim() {
            q if q.is_empty() => {
                let filter = self.state.group_filter;
                self.store.list(PAGE_LIMIT, 0, filter)
            }
            q => self.store.search(q, PAGE_LIMIT),
        };
        match result {
            Ok(items) => self.state.items = items,
            Err(e) => {
                tracing::error!(%e, "读取列表失败");
                self.state.items.clear();
            }
        }
        self.state.groups = self.store.list_groups().unwrap_or_default();
    }

    /// 搜索。
    pub fn search(&mut self, query: String) {
        self.state.query = query;
        self.reload_list();
    }

    /// 设置分组过滤。
    pub fn filter_group(&mut self, filter: Option<GroupFilter>) {
        self.state.group_filter = filter;
        self.reload_list();
    }

    /// 把条目写回系统剪贴板。
    pub fn copy_item(&mut self, id: EntryId) -> Result<()> {
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
        self.suppress_self_capture();

        match item.kind {
            ClipKind::Text | ClipKind::Html => {
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
                let (rgba, w, h) = decode_to_rgba(&bytes)?;
                reader.write_image(rgba, w, h)?;
            }
        }

        self.store.touch(id).ok();
        Ok(())
    }

    /// 复制并自动粘贴到前台窗口。
    pub fn copy_and_paste(&mut self, id: EntryId) -> Result<()> {
        self.copy_item(id)?;
        modular_clipboard_platform::auto_paste(Duration::from_millis(250))?;
        Ok(())
    }

    /// 执行联动动作。
    pub fn run_action(&mut self, item: &ClipItem, proposal: &action::ActionProposal) -> Result<()> {
        action::dispatch(&proposal.plan)?;
        self.store.touch(item.id).ok();
        self.notify(format!("已执行：{}", proposal.label));
        Ok(())
    }

    /// 计算某条目的可用动作。
    pub fn proposals_for(&self, item: &ClipItem) -> Vec<action::ActionProposal> {
        action::propose(&self.registry, item, &self.state.config.rules)
    }

    // ---------- 条目操作 ----------

    pub fn delete_item(&mut self, id: EntryId) -> Result<()> {
        self.store.delete(id)?;
        if self.state.selected == Some(id) {
            self.state.selected = None;
        }
        self.reload_list();
        Ok(())
    }

    pub fn toggle_pin(&mut self, id: EntryId) -> Result<()> {
        if let Some(item) = self.store.get(id)? {
            let new_state = !item.pinned;
            self.store.set_pinned(id, new_state)?;
            self.reload_list();
        }
        Ok(())
    }

    pub fn move_to_group(&mut self, id: EntryId, group: Option<i64>) -> Result<()> {
        self.store.move_to_group(id, group)?;
        self.reload_list();
        Ok(())
    }

    pub fn create_group(&mut self, name: &str) -> Result<i64> {
        let id = self.store.create_group(name, DEFAULT_GROUP_COLOR)?;
        self.reload_list();
        Ok(id)
    }

    pub fn clear_all(&mut self) -> Result<()> {
        self.store.clear_all()?;
        self.state.selected = None;
        self.reload_list();
        Ok(())
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
        if let Some(path) = self.config_path.clone() {
            // 临时覆盖不落盘：读回磁盘上的真实值。
            let persisted = self
                .capture_override
                .then(|| std::fs::read_to_string(&path).ok())
                .flatten()
                .and_then(|raw| serde_json::from_str::<Config>(&raw).ok());
            if let Some(real) = persisted {
                if real.capture.enabled != self.state.config.capture.enabled {
                    tracing::info!(
                        persisted = real.capture.enabled,
                        "不把 --no-capture 临时覆盖写入 config.json"
                    );
                    self.state.config.capture.enabled = real.capture.enabled;
                }
            }
            self.state.config.save(&path)?;
            self.store.apply_config(&self.state.config);
        }
        Ok(())
    }

    /// 标记当前 `capture.enabled` 是命令行临时覆盖，保存配置时应跳过。
    pub fn set_capture_override(&mut self, is_override: bool) {
        self.capture_override = is_override;
    }

    /// 在状态栏显示一条消息，3 秒后过期。
    pub fn notify(&mut self, msg: String) {
        self.state.notice = Some((msg, Instant::now()));
    }
}

/// 捕获循环的单步决策：这一轮是否应该读取剪贴板内容。
///
/// 返回 `true` 当且仅当：序列号有新变化，且当前不在自写回抑制窗口内。
///
/// NOTE: 无论结果如何，都会推进序列号基线（`ClipboardReader::changed` 的
/// 副作用）。这是刻意为之：自写回（`copy_item`）同样会改变剪贴板序列号，
/// 若抑制期内跳过基线推进，自写导致的变化会在抑制期结束后被当成用户的
/// 新复制重新捕获——每次把历史条目放回剪贴板都多记一条重复记录。
///
/// `now` 为当前墙钟（Unix 毫秒），`last_self_write` 为上次自写回时刻。
/// 减法必须用 `saturating_sub`：墙钟可能被 NTP 校时或手动改时间回拨，
/// 自写抑制窗口，按**轮询间隔**动态计算。
///
/// # 为什么不能是常量
///
/// 设自写发生在 T，捕获线程上一轮在 `t0 ∈ (T-I, T]` 唤醒（I = 轮询间隔），
/// 下一轮在 `t0+I`，此时 `since = t0 + I - T`。最坏情形 `t0 → T`，
/// `since → I`。
///
/// 因此抑制窗口必须 **> I** 才能保证「自写后的第一轮一定落在窗口内」。
/// 早前 `SELF_WRITE_GUARD_MS` 是常量 900，而 `poll_interval_ms` 只夹
/// 下限（50）、**无上限**——用户在设置面板把间隔调到 1000ms（省电是很
/// 自然的动机）就会让 `since ≥ 900` 成立 ⇒ **抑制彻底失效**，此后每次
/// 从历史写回都多记一条。
///
/// 取 `2*I + 200`：覆盖「上一轮刚好在写之前唤醒」的最坏情形，并留余量。
///
/// # 已知取舍
///
/// 窗口越大，自写之后**用户真实的复制**被一并吞掉的概率越高（它们会被
/// `changed()` 吸收进基线后丢弃）。这是时间窗方案的固有代价。更彻底的
/// 解法是**计数式抑制**（写前 `fetch_add(1)`，捕获线程消费一次），
/// 与轮询间隔、墙钟回拨、NTP 校时全部解耦 —— 但那需要跨线程共享计数器，
/// 改动面大，留待后续。
fn self_write_guard_ms(poll_interval_ms: i64) -> i64 {
    (2 * poll_interval_ms + 200).max(SELF_WRITE_GUARD_MS_FLOOR)
}

/// 无符号减法会下溢成巨值，让抑制永久失效（见捕获线程的注释）。
fn capture_should_read(
    reader: &mut ClipboardReader,
    now: i64,
    last_self_write: i64,
    poll_interval_ms: i64,
) -> bool {
    let since = now.saturating_sub(last_self_write);
    reader.changed() && since >= self_write_guard_ms(poll_interval_ms)
}

/// 单页最多加载的条目数。
const PAGE_LIMIT: usize = 2000;

const DEFAULT_GROUP_COLOR: &str = "#5B8DEF";

/// 后台事件队列容量。溢出时丢弃新事件而非阻塞剪贴板读取。
const EVENT_QUEUE_CAPACITY: usize = 128;

/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// 未指定 `--data-dir` 时的配置路径（`%APPDATA%/modular-clipboard/config.json`）。
fn default_config_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "modular-clipboard")
        .map(|d| d.config_dir().join(CONFIG_FILE_NAME))
}

/// 数据目录内的配置文件路径。
///
/// 与 [`default_config_path`] 分开而不是「有data_dir 就 join、否则 ProjectDirs」：
/// `--data-dir` 指定的是**数据**目录，配置跟着走才能保证
/// 「读配置」与「写配置」在同一处（`save_config` 的读回逻辑依赖这一点）。
pub fn config_path_in(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join(CONFIG_FILE_NAME)
}

/// 生成预览文本。
fn build_preview(payload: &CapturedPayload) -> String {
    match payload.kind {
        ClipKind::Text => payload
            .text
            .clone()
            .unwrap_or_else(|| String::from_utf8_lossy(&payload.bytes).to_string()),
        ClipKind::Html => payload
            .text
            .clone()
            .unwrap_or_else(|| String::from_utf8_lossy(&payload.bytes).to_string()),
        ClipKind::Files => {
            let names: Vec<String> = payload
                .files
                .iter()
                .map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            if names.len() == 1 {
                names[0].clone()
            } else {
                format!("{} 等 {} 个文件", names[0], names.len())
            }
        }
        ClipKind::Image => {
            // 图片用尺寸描述作为预览，解码失败时给出可读提示。
            match image_dimensions(&payload.bytes) {
                Some((w, h)) => format!("图片 {w}×{h}"),
                None => format!("图片 {} 字节", payload.bytes.len()),
            }
        }
    }
}

/// 读取图片尺寸（仅解析文件头，不解码像素）。
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// 把任意格式图片解码为 RGBA8，供写回剪贴板使用。
fn decode_to_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes)?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

/// 时间格式化，用于界面显示。
pub fn format_time(ts_ms: i64, now_ms: i64) -> String {
    let diff = now_ms - ts_ms;
    if diff < 60_000 {
        "刚刚".to_string()
    } else if diff < 3_600_000 {
        format!("{} 分钟前", diff / 60_000)
    } else if diff < 86_400_000 {
        format!("{} 小时前", diff / 3_600_000)
    } else {
        format!("{} 天前", diff / 86_400_000)
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
        };
        assert!(build_preview(&p).starts_with("图片"));
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
            svc.config_path.is_some(),
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
        //    但基线必须已推进——自写导致的变化被吸收。
        assert!(
            !capture_should_read(&mut reader, last_self_write, last_self_write, 300),
            "抑制期内不应读取剪贴板"
        );

        // 2. 抑制期结束后（since = 900）：序列号无新变化 ⇒ 仍不读取。
        //    旧实现在这里返回 true——把自写回当成用户新复制（bug）。
        assert!(
            !capture_should_read(
                &mut reader,
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
                &mut reader,
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
