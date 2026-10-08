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
/// 自写抑制窗口：写回剪贴板后这段时间内忽略捕获。
const SELF_WRITE_GUARD_MS: i64 = 900;

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
        // 存储与配置必须落在**同一个**目录，否则会出现
        // 「库在 D 盘、配置在 C 盘」的半生效状态。
        let config_path = data_dir.map(|d| d.join(CONFIG_FILE_NAME));
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
                    // ⚠️ 必须用饱和减法，不能写 `now - last`：
                    // `now_ms()` 取的是**墙钟**，NTP 校时或用户手动改时间
                    // 会让它瞬间小于 `last_self_write`。无符号减法在此
                    // 下溢成一个接近 2^64 的巨值，于是 `since < 900`
                    // 恒为 false——抑制**永久失效**，此后每次把历史写回
                    // 剪贴板都会被当成用户的复制重新记一条。
                    // 饱和减法在时钟回拨时得 0（仍在抑制窗口内），
                    // 时钟前移得最大值（窗口已过），两种方向都安全。
                    let now = now_ms() as i64;
                    let last = last_self_write.load(Ordering::Relaxed) as i64;
                    let since_self_write = now.saturating_sub(last);
                    if since_self_write < SELF_WRITE_GUARD_MS {
                        std::thread::sleep(Duration::from_millis(interval));
                        continue;
                    }

                    if reader.changed()
                        && let Some(payload) = reader.read()
                    {
                        c.fetch_add(1, Ordering::Relaxed);
                        if tx.push(Signal::Captured(payload)).is_err() {
                            // 队列满：丢���这次事件，不能阻塞剪贴板读取。
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
            return false;
        }

        // 来源程序屏蔽。
        if !payload.source_app.is_empty() && cfg.is_app_blocked(&payload.source_app) {
            self.state.stats.app_blocked += 1;
            return false;
        }

        // 密码框启发式：仅在敏感窗口为前台时跳过。
        if cfg.capture.skip_password_fields && modular_clipboard_platform::is_password_field() {
            self.state.stats.secret_field += 1;
            return false;
        }

        let hash = fingerprint(&payload.bytes);
        let preview = build_preview(&payload);

        // 去重：窗口内的相同内容仅刷新时间，不新增条目。
        if cfg.capture.dedup {
            match self
                .store
                .find_recent_by_hash(&hash, cfg.capture.dedup_window_secs)
            {
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
        // 标记抑制，避免刚写入的内容被当成新捕获。
        self.suppress_self_capture();
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
}
