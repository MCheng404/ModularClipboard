//! 存储层：SQLite 元数据 + FTS5 全文索引 + 文件化载荷。
//!
//! 设计要点：
//! 1. 大载荷（图片、大段文本）不进数据库，单独存文件，库中只留引用。
//!    这让 SQLite 始终保持在几十 MB 量级，查询不会因大 blob 变慢。
//! 2. 文本类条目同步写入 FTS5 虚表，支持中文全文检索与前缀匹配。
//! 3. 所有写操作走单连接，由上层保证串行。
//!    落盘连接的 `journal_mode=WAL` + `synchronous=NORMAL`（见
//!    `open.rs` 的 `open()`）：WAL 让读不被写阻塞，`NORMAL` 在 WAL 下
//!    只丢「最后一次事务」而不丢一致性，对剪贴板这种可重建数据够用。
//!    `open_in_memory()` 走内存库，两者都不该被这条影响。
//!
//! # 模块地图
//!
//! [`Store`] 的方法按职责拆在多个 `impl` 块里（Rust 允许同类型多
//! `impl` 块散落多文件）；删一个功能 = 删一个文件 + 一行 `mod`。
//!
//! | 文件 | 职责 |
//! |------|------|
//! | `open.rs` | 打开、建表迁移、位置访问（`data_dir` / `path`） |
//! | `query.rs` | 分页列表、全文搜索与 LIKE 回退、行反序列化 |
//! | `write.rs` | 插入、就地更新、置顶、删除（含引用计数删载荷） |
//! | `retention.rs` | 保留策略淘汰、全量清空、孤儿载荷回收 |
//! | `groups.rs` | 分组定义的增删改查 |
//! | `blobs.rs` | 载荷文件层：路径规范化（SEC-006）、原子写、尺寸嗅探 |
//!
//! 本文件（root）只留：句柄定义 + `Drop`、路径常量、指纹、
//! [`GroupFilter`] 词汇，以及注入配置的三个小方法。

use rusqlite::Connection;
use std::path::PathBuf;
use modular_clipboard_core::{ClipKind, Config};

mod open;
mod query;
mod write;
mod retention;
mod groups;
mod blobs;


/// 默认载荷上限，构造后由 `apply_config` 覆盖。
pub const MAX_PAYLOAD_DEFAULT: u64 = 32 * 1024 * 1024;

/// 计算内容指纹。用于去重与FTS 关联。
pub fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, 16)
}

/// 取摘要前 `n` 字节的小写十六进制串。
///
/// NOTE: 用查表逐字节写入而不是 `format!("{b:02x}")`——后者每字节构造一次
/// `Arguments` 并做一次格式化，32 字节摘要就是 32 次；指纹在每条剪贴板
/// 内容上都要算一遍（含去重查询前的临时计算），是热路径。
fn hex_prefix(bytes: &[u8], n: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let take = bytes.len().min(n);
    let mut s = String::with_capacity(take * 2);
    for &b in &bytes[..take] {
        s.push(HEX[usize::from(b >> 4)] as char);
        s.push(HEX[usize::from(b & 0x0f)] as char);
    }
    s
}

/// 存储句柄。内部持有 SQLite 连接与载荷目录。
pub struct Store {
    conn: Connection,
    blob_dir: PathBuf,
    db_path: PathBuf,
    /// 载荷体积上限，由 [`Store::set_payload_limit`] 从配置注入。
    payload_limit: std::sync::atomic::AtomicU64,
    /// 载荷目录是否由本 store 独占拥有（临时目录）。
    ///
    /// 只有 `true` 时 [`Drop`] 才会删除它。为 `false` 时目录属于用户
    /// 真实数据（`%APPDATA%` 或 `--data-dir`），**任何情况下都不能删**。
    owns_blob_dir: bool,
}

impl Drop for Store {
    /// 清理本 store 独占创建的临时载荷目录。
    ///
    /// 只在 `owns_blob_dir` 为真时动手：
    /// - `open_in_memory()` 的目录是进程私有的、名字带唯一序号，
    ///   随 store 一起消失才不会在 `%TEMP%` 留下垃圾。
    /// - `open()` / `open_in()` 的目录是用户历史数据，删掉等于清空用户剪贴板历史。
    ///
    /// 降级模式（进程长期存活）不受影响：目录只在 `Store` **被 drop 时**
    /// 才删除，而 store 活着时目录始终有效。
    fn drop(&mut self) {
        if self.owns_blob_dir && self.blob_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.blob_dir);
        }
    }
}

/// 数据目录名（`ProjectDirs` 的第三参数）。
///
/// 单一来源：帮助文本、默认路径、临时目录都从这里派生。
/// 之前这些字符串散落三处，改名时漏了一处，导致
/// `--help` 显示的路径与实际行为不符。
pub const DATA_DIR_NAME: &str = "modular-clipboard";

/// 用于展示的默认数据目录全路径。
///
/// 帮助文本用它，避免文档与实现漂移。
pub fn default_data_dir_display() -> String {
    match default_data_dir() {
        Some(d) => d.display().to_string(),
        None => DATA_DIR_NAME.to_string(),
    }
}

/// 默认数据目录（`%APPDATA%/modular-clipboard`）。
///
/// 抽成函数是为了让 [`default_data_dir_display`] 与 [`Store::open_default`]
/// 共用同一份推导逻辑，避免「展示的路径」与「实际打开的路径」再次漂移。
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", DATA_DIR_NAME)
        .map(|d| d.data_dir().to_path_buf())
}

/// 数据目录下的数据库文件名。
pub const DB_FILE_NAME: &str = "history.db";

/// 数据目录下的载荷子目录名。
pub const BLOB_DIR_NAME: &str = "blobs";

impl Store {
    /// 当前载荷体积上限。由 [`Store::set_payload_limit`] 设置。
    fn current_payload_limit(&self) -> u64 {
        self.payload_limit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置载荷上限（来自配置）。在插入前调用。
    pub fn set_payload_limit(&self, limit: u64) {
        self.payload_limit
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    /// 从默认配置应用存储设置。
    pub fn apply_config(&self, cfg: &Config) {
        self.set_payload_limit(cfg.storage.max_payload_bytes);
    }
}

/// 分组过滤条件。
///
/// `Serialize` 是为了让它能跨进程传递（见 `modular-clipboard-ipc`）：
/// 界面进程不打开数据库，过滤条件只能走通道发给常驻进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GroupFilter {
    Group(i64),
    Ungrouped,
}

fn parse_kind(s: &str) -> ClipKind {
    match s {
        "html" => ClipKind::Html,
        "image" => ClipKind::Image,
        "files" => ClipKind::Files,
        "markdown" => ClipKind::Markdown,
        _ => ClipKind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::blobs::safe_blob_path;
    use std::path::Path;
    use modular_clipboard_core::{ClipItem, StorageConfig};
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        // LIKE 的 `%` 与 `_` 是通配符。用户搜 `%` 是想找字面量百分号，
        // 不是想列出全部。不转义的话搜索结果会完全跑偏。
        assert_eq!(Store::escape_like("100%"), r"100\%");
        assert_eq!(Store::escape_like("a_b"), r"a\_b");
        // 反斜杠自身也必须转义，否则它会把后面加的转义符再转义一次：
        // 输入 `a\b`（单个反斜杠）应变成 `a\\b`（两个）。
        assert_eq!(Store::escape_like("a\\b"), r"a\\b");
        // 普通字符与中文不受影响
        assert_eq!(Store::escape_like("剪贴板"), "剪贴板");
        assert_eq!(Store::escape_like(""), "");
    }

    #[test]
    fn like_search_treats_wildcards_literally() {
        // 端到端验证：插入含 % 的条目，搜 "%" 不应命中它。
        // 这是 escape_like 存在的理由——单测字符串变换不够，
        // 要确认最终 SQL 行为符合预期。
        let (mut st, dir) = temp_store();
        for preview in ["50% 折扣", "普通条目"] {
            let item = ClipItem::new(
                ClipKind::Text,
                "h".into(),
                preview.into(),
                "app".into(),
            );
            st.insert(item, preview.as_bytes().to_vec()).unwrap();
        }

        let hits = st.like_search("%", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "搜索 % 应只匹配字面量含 % 的那一条，实际命中 {} 条",
            hits.len()
        );
        assert!(hits[0].preview.contains('%'));

        let _ = std::fs::remove_dir_all(dir);
    }

    fn temp_store() -> (Store, PathBuf) {
        // 目录名必须全进程唯一：仅用 now_ms() 会在毫秒精度内并发测试时冲突。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("{DATA_DIR_NAME}-store-test-{}-{}", std::process::id(), n));
        let s = Store::open(&base.join("t.db"), &prepare_dir(&base)).unwrap();
        (s, base)
    }

    /// 清空并重建测试目录，返回其中的载荷子目录。
    ///
    /// 必须先清空再建：进程 ID 会被操作系统复用，目录名
    /// `store-test-<pid>-<n>` 因此可能撞上历史残留。曾只用
    /// `create_dir_all`，旧的 `t.db` 被保留下来，`Store::open`
    /// 直接打开旧库，旧条目混进当前测试，表现为随机的
    /// 「命中数不对」（实测：`fts_search_finds_text` 期望 1 条却拿到 2 条）。
    fn prepare_dir(base: &Path) -> PathBuf {
        let _ = std::fs::remove_dir_all(base);
        std::fs::create_dir_all(base).unwrap();
        base.join(BLOB_DIR_NAME)
    }

    /// 独占的临时目录名（进程 ID + 原子序号）。
    ///
    /// 不能只用 `now_ms()`：毫秒精度下并发测试会拿到同名目录，
    /// 互相 `remove_dir_all`，表现为随机的「表不存在」失败。
    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "{DATA_DIR_NAME}-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    /// `--data-dir` 的核心保证：数据库文件真的落在传入目录下。
    ///
    /// 回归测试。此前 `open_default` 内部无条件问 `ProjectDirs`，
    /// `--data-dir` 传了也被忽略，用户以为隔离了数据、实际写进了 `%APPDATA%`。
    #[test]
    fn open_in_creates_db_under_given_dir() {
        let base = unique_dir("open-in");
        assert!(!base.exists(), "前置条件：目录此刻应不存在");

        let store = Store::open_in(&base).expect("指定数据目录应能打开");
        drop(store);

        assert!(
            base.join(DB_FILE_NAME).is_file(),
            "数据库应创建在 {} 下，实际内容：{:?}",
            base.display(),
            std::fs::read_dir(&base)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect::<Vec<_>>()
        );
        assert!(
            base.join(BLOB_DIR_NAME).is_dir(),
            "载荷目录应创建在 {} 下",
            base.display()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 目录不存在时必须创建，而不是报错退出。
    ///
    /// 用户传 `--data-dir D:\clip` 时期望的是「程序自己建好」。
    #[test]
    fn open_in_creates_missing_dir_tree() {
        let base = unique_dir("open-in-missing").join("a").join("b");
        assert!(!base.exists());

        let store = Store::open_in(&base).expect("多级缺失目录应被自动创建");
        drop(store);

        assert!(base.join(DB_FILE_NAME).is_file());
        let _ = std::fs::remove_dir_all(base.parent().unwrap().parent().unwrap());
    }

    /// 载荷文件也必须写入指定目录下的 `blobs`。
    #[test]
    fn open_in_writes_payload_into_given_dir() {
        let base = unique_dir("open-in-blob");
        let mut store = Store::open_in(&base).unwrap();
        store.set_payload_limit(1 << 20);

        let item = ClipItem::new(ClipKind::Text, "hh".into(), "in-dir".into(), "app".into());
        let saved = store.insert(item, b"in-dir".to_vec()).unwrap();
        assert!(saved.payload.is_some());

        let loaded = store.get(saved.id).unwrap().unwrap();
        assert_eq!(store.load_payload(&loaded).unwrap(), b"in-dir");
        assert!(
            std::fs::read_dir(base.join(BLOB_DIR_NAME))
                .unwrap()
                .next()
                .is_some(),
            "载荷应落到 {} 下",
            base.join(BLOB_DIR_NAME).display()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 空白路径必须报错，**不能**静默回退到默认目录。
    ///
    /// 静默回退最坏：用户以为在用隔离目录做测试，实际却在污染真实历史库。
    #[test]
    fn open_in_rejects_blank_dir() {
        for blank in ["", " ", "\t", "\n  "] {
            let err = Store::open_in(Path::new(blank))
                .err()
                .unwrap_or_else(|| panic!("空白路径 {blank:?} 应被拒绝"));
            assert!(
                err.to_string().contains("数据目录"),
                "错误信息应指明是数据目录有问题，实际：{err}"
            );
        }
    }

    /// `open_default` 仍落在 `%APPDATA%/modular-clipboard`——
    /// 固定住「未传 `--data-dir` 时行为不变」。
    #[test]
    fn open_default_uses_project_dirs() {
        let Some(expected) = default_data_dir() else {
            eprintln!("跳过：无法确定默认数据目录");
            return;
        };
        let store = Store::open_default().expect("默认数据目录应能打开");
        drop(store);
        assert!(
            expected.join(DB_FILE_NAME).is_file(),
            "默认数据库应在 {} 下",
            expected.display()
        );
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1024 * 1024);
        let item = ClipItem::new(ClipKind::Text, "h1".into(), "hello world".into(), "app".into());
        let saved = store.insert(item, b"hello world".to_vec()).unwrap();
        assert!(saved.id > 0);
        assert!(saved.payload.is_some());

        let loaded = store.get(saved.id).unwrap().unwrap();
        assert_eq!(loaded.preview, "hello world");
        assert_eq!(loaded.kind, ClipKind::Text);
        assert_eq!(store.load_payload(&loaded).unwrap(), b"hello world");

        let _ = std::fs::remove_dir_all(base);
    }

    /// 全局去重：同一哈希无论隔了多久再次出现都命中（取最新未置顶条目）。
    ///
    /// 早期实现带 3 秒窗口（`dedup_window_secs`），隔几秒的重复复制
    /// 仍会新增一条；全局查找后相同内容永远只保留一条记录。
    #[test]
    fn find_by_hash_finds_latest_unpinned() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1024 * 1024);

        // 同一哈希插入两条（时间不同）→ 全局查找命中最新一条。
        //
        // NOTE: 两条间隔极短，**常常落在同一毫秒内**。这正是本条最值钱
        // 的地方：它顺带守住 `find_by_hash` 的 `id DESC` 兜底排序。
        // 去掉那个兜底，本条会退化成**偶发**失败——最容易被当成
        // 「测试不稳定」而放过去的一类缺陷。
        let a = ClipItem::new(ClipKind::Text, "same-hash".into(), "first".into(), "app".into());
        store.insert(a, b"first".to_vec()).unwrap();
        let b = ClipItem::new(ClipKind::Text, "same-hash".into(), "second".into(), "app".into());
        let saved_b = store.insert(b, b"second".to_vec()).unwrap();
        assert_eq!(
            store.find_by_hash("same-hash").unwrap(),
            Some(saved_b.id),
            "重复复制应命中最新那条，而不是 None（窗口外也生效）"
        );

        // 置顶条目不参与去重：用户手动置顶的旧内容不能被吞掉。
        let mut c = ClipItem::new(ClipKind::Text, "same-hash".into(), "pinned".into(), "app".into());
        c.pinned = true;
        let saved_c = store.insert(c, b"pinned".to_vec()).unwrap();
        assert_ne!(
            store.find_by_hash("same-hash").unwrap(),
            Some(saved_c.id),
            "置顶条目不应被去重逻辑命中"
        );

        // 不存在的哈希返回 None。
        assert_eq!(store.find_by_hash("nope").unwrap(), None);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn payload_over_limit_is_dropped_but_metadata_kept() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(4);
        let item = ClipItem::new(ClipKind::Text, "h2".into(), "0123456789".into(), "app".into());
        let saved = store.insert(item, b"0123456789".to_vec()).unwrap();
        assert!(saved.payload.is_none(), "超限载荷不应落盘");
        // 元数据仍可查询
        assert_eq!(store.get(saved.id).unwrap().unwrap().preview, "0123456789");
        let _ = std::fs::remove_dir_all(base);
    }

    /// `update_content` 是「编辑内容」的落地：换文本、换载荷、换索引，
    /// 但**不动** id / pinned / group / use_count（用户编辑一次文字不该
    /// 被踢出置顶，也不该丢使用计数）。
    #[test]
    fn update_content_swaps_text_and_keeps_identity() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "h0".into(), "旧内容".into(), "app".into());
        let saved = store.insert(item, "旧内容".as_bytes().to_vec()).unwrap();
        // 置顶 + 用几次，制造「不该被编辑冲掉」的身份信息。
        store.set_pinned(saved.id, true).unwrap();
        store.touch(saved.id).unwrap();
        store.touch(saved.id).unwrap();

        store
            .update_content(saved.id, ClipKind::Text, "新内容".into(), "新内容".as_bytes().to_vec(), None)
            .unwrap();

        let got = store.get(saved.id).unwrap().unwrap();
        assert_eq!(got.id, saved.id, "id 不能变");
        assert_eq!(got.preview, "新内容");
        assert!(got.pinned, "置顶状态必须保留");
        assert_eq!(got.use_count, 2, "使用计数必须保留");
        // 载荷已换成新文本（复制出去应是新内容）。
        let bytes = store.load_payload(&got).unwrap();
        assert_eq!(bytes, "新内容".as_bytes());
        // FTS 跟着换：搜旧的搜不到，搜新的命中。
        assert!(store.search("旧内容", 10).unwrap().is_empty(), "旧索引必须被删");
        assert_eq!(store.search("新内容", 10).unwrap().len(), 1);

        // 旧载荷文件按引用计数清掉（无其他条目共享）。
        let old_file = format!("{}.bin", fingerprint("旧内容".as_bytes()));
        assert!(!store.blob_dir.join(old_file).exists(), "旧载荷应被清理");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 编辑成**另一条已存在条目的同款内容**时，新载荷文件与那条共享
    /// （`{hash}.bin` 同名）——本行仍引用它，清理逻辑不能把文件删了
    /// （否则那条历史瞬间失去载荷，且毫无报错）。
    #[test]
    fn update_content_keeps_shared_blob_alive() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let a = ClipItem::new(ClipKind::Text, "h1".into(), "共享内容".into(), "app".into());
        let saved_a = store.insert(a, "共享内容".as_bytes().to_vec()).unwrap();
        let b = ClipItem::new(ClipKind::Text, "h2".into(), "另一条".into(), "app".into());
        let saved_b = store.insert(b, "另一条".as_bytes().to_vec()).unwrap();

        // 把 B 编辑成与 A 相同的内容：哈希相同 ⇒ 同一个载荷文件。
        store
            .update_content(saved_b.id, ClipKind::Text, "共享内容".into(), "共享内容".as_bytes().to_vec(), None)
            .unwrap();

        let file = format!("{}.bin", fingerprint("共享内容".as_bytes()));
        assert!(store.blob_dir.join(&file).exists(), "共享载荷不能被清掉");
        // A 的载荷仍然读得出来。
        let got_a = store.get(saved_a.id).unwrap().unwrap();
        assert_eq!(store.load_payload(&got_a).unwrap(), "共享内容".as_bytes());
        let _ = std::fs::remove_dir_all(base);
    }

    /// 图片类更新：kind 从 Text 换成 Image 时 FTS 索引必须**消失**
    /// （图片不进索引），否则搜得到一条「没有文字」的图片条目。
    #[test]
    fn update_content_drops_fts_when_kind_leaves_text() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "h3".into(), "可搜索的文字".into(), "app".into());
        let saved = store.insert(item, "可搜索的文字".as_bytes().to_vec()).unwrap();
        assert_eq!(store.search("可搜索", 10).unwrap().len(), 1);

        // 载荷换成 4×2 裸 RGBA8（16 字节），kind 换成 Image。
        let px = vec![255u8; 4 * 2 * 4];
        store
            .update_content(saved.id, ClipKind::Image, "图片 4×2".into(), px, Some((4, 2)))
            .unwrap();

        assert!(store.search("可搜索", 10).unwrap().is_empty(), "换类后旧索引必须删");
        let got = store.get(saved.id).unwrap().unwrap();
        assert_eq!(got.kind, ClipKind::Image);
        assert_eq!(got.payload.unwrap().dims, Some((4, 2)), "尺寸必须落到行");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn fts_search_finds_text() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let a = ClipItem::new(ClipKind::Text, "ha".into(), "剪贴板历史记录".into(), "app".into());
        store.insert(a, b"x".to_vec()).unwrap();
        let b = ClipItem::new(ClipKind::Text, "hb".into(), "totally different".into(), "app".into());
        store.insert(b, b"y".to_vec()).unwrap();

        let hits = store.search("剪贴板", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "剪贴板历史记录");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn malformed_fts_query_falls_back_to_like() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "hc".into(), "quoted \"value\" here".into(), "a".into());
        store.insert(item, b"z".to_vec()).unwrap();
        // 未闭合引号会让FTS MATCH 报错，必须回退而非 panic。
        let hits = store.search("\"value", 10).unwrap();
        assert!(!hits.is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn image_kind_is_not_indexed_by_fts() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Image, "hi".into(), "800x600 image".into(), "a".into());
        store.insert(item, vec![0u8; 16]).unwrap();
        // 图片不该污染全文索引
        assert!(store.search("800x600", 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn delete_removes_blob_file() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "hd".into(), "delete me".into(), "a".into());
        let saved = store.insert(item, b"delete me".to_vec()).unwrap();
        let file = saved.payload.unwrap().file;
        assert!(store.blob_dir.join(&file).exists());
        store.delete(saved.id).unwrap();
        assert!(!store.blob_dir.join(&file).exists());
        assert!(store.get(saved.id).unwrap().is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn delete_keeps_blob_shared_with_pinned_entry() {
        // DF-01 回归：载荷按 `{hash}.bin` 命名，而置顶条目不参与去重
        // （find_by_hash 带 `AND pinned = 0`）。于是「置顶 A → 再次复制
        // 同一内容得到 B」两条记录共享同一文件。删 B 绝不能摧毁 A 的载荷
        // ——保留策略的自动淘汰同样会走到 delete，默认配置即可触发。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let a = ClipItem::new(ClipKind::Text, "shared-hash".into(), "same content".into(), "a".into());
        let saved_a = store.insert(a, b"same content".to_vec()).unwrap();
        let mut b = ClipItem::new(
            ClipKind::Text,
            "shared-hash".into(),
            "same content".into(),
            "a".into(),
        );
        b.pinned = true;
        let saved_b = store.insert(b, b"same content".to_vec()).unwrap();
        assert_ne!(saved_a.id, saved_b.id, "同哈希应插入两条（置顶不参与去重）");
        let file = saved_a.payload.as_ref().unwrap().file.clone();
        assert_eq!(saved_b.payload.as_ref().unwrap().file, file, "两条应共享同一载荷文件");

        store.delete(saved_a.id).unwrap();

        assert!(
            store.blob_dir.join(&file).exists(),
            "仍有条目引用时不得删除载荷文件"
        );
        let reloaded = store.get(saved_b.id).unwrap().expect("置顶条目应在");
        let payload = store.load_payload(&reloaded).unwrap();
        assert_eq!(payload, b"same content", "置顶条目的载荷必须完好可读");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn insert_leaves_no_tmp_files_behind() {
        // 原子写的 .tmp 必须在成功 rename 后消失；残留会累积且不被
        // gc_orphan_blobs 识别（它只收「无引用」的正式文件）。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "tmp-h".into(), "atomic".into(), "a".into());
        store.insert(item, b"atomic".to_vec()).unwrap();
        let leftovers: Vec<String> = std::fs::read_dir(&store.blob_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不得留下 .tmp 残留：{leftovers:?}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn two_char_query_falls_back_to_like() {
        // DF-02 回归：trigram 以 3 字符为最小匹配单位，1-2 字符查询
        // **不报错**、静默返回 0 行——Err 兜底等不到出场时机。
        // 两字中文词（最常用的检索长度）必须经 LIKE 命中。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(
            ClipKind::Text,
            "h2c".into(),
            "剪贴板历史记录".into(),
            "a".into(),
        );
        store.insert(item, b"x".to_vec()).unwrap();
        let hits = store.search("剪贴", 10).unwrap();
        assert_eq!(hits.len(), 1, "两字查询必须经 LIKE 兜底命中");
        assert_eq!(hits[0].preview, "剪贴板历史记录");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn batch_delete_keeps_blob_shared_with_survivor() {
        // DF-01 × PF-7 交互：批量删除（保留策略）与共享载荷的交点。
        // 一批里删掉同哈希的多条，只要还有一条（置顶）在引用，
        // 文件就必须留下——逐条删与批量删的语义必须一致。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        // 三条同哈希（共享 {hash}.bin），两条未置顶 + 一条置顶。
        let mut ids = Vec::new();
        for pinned in [false, false, true] {
            let mut item = ClipItem::new(
                ClipKind::Text,
                "batch-shared".into(),
                "batch shared content".into(),
                "a".into(),
            );
            item.pinned = pinned;
            let saved = store.insert(item, b"batch shared content".to_vec()).unwrap();
            ids.push(saved.id);
        }
        let file = store
            .get(ids[2])
            .unwrap()
            .unwrap()
            .payload
            .unwrap()
            .file
            .clone();

        // 批量删前两条（未置顶）。
        store.delete_many(&ids[..2]).unwrap();

        assert!(
            store.blob_dir.join(&file).exists(),
            "置顶条目仍引用载荷文件，批量删不得删它"
        );
        let survivor = store.get(ids[2]).unwrap().expect("置顶条目应在");
        assert_eq!(store.load_payload(&survivor).unwrap(), b"batch shared content");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn safe_blob_path_rejects_traversal_and_absolute() {
        // SEC-006 回归：DB 行里的 blob_file 不可信，含目录分量或 `..`
        // 的取值必须被拒绝，否则 join 会逃出 blobs 目录。
        let dir = std::path::Path::new("/data/blobs");
        assert_eq!(
            safe_blob_path(dir, "abc123.bin"),
            Some(dir.join("abc123.bin")),
            "正常载荷名应放行"
        );
        assert_eq!(safe_blob_path(dir, ""), None, "空名拒绝");
        assert_eq!(safe_blob_path(dir, ".."), None, ".. 拒绝");
        assert_eq!(safe_blob_path(dir, "../secret.bin"), None, "相对逃逸拒绝");
        assert_eq!(safe_blob_path(dir, "a/b.bin"), None, "含 / 拒绝");
        assert_eq!(safe_blob_path(dir, "a\\b.bin"), None, "含 \\ 拒绝");
        assert_eq!(
            safe_blob_path(dir, r"C:\Windows\system32\cfg"),
            None,
            "绝对路径拒绝"
        );
        assert_eq!(safe_blob_path(dir, "/etc/passwd"), None, "绝对路径拒绝");
    }

    #[test]
    fn safe_blob_path_rejects_device_names_and_streams() {
        // 补 DL-08 附注的两类漏网：DOS 设备名（打开的是设备而不是文件）
        // 与 NTFS 数据流 / 盘符冒号。
        let dir = std::path::Path::new("/data/blobs");
        for bad in ["NUL", "nul", "NUL.txt", "CON", "COM1", "lpt9.log", "a.bin:evil", "C:"] {
            assert_eq!(safe_blob_path(dir, bad), None, "应拒绝：{bad}");
        }
        assert!(
            safe_blob_path(dir, "0123456789abcdef.bin").is_some(),
            "正常 hash 名必须放行"
        );
    }

    #[test]
    fn write_blob_atomic_refuses_unsafe_names() {
        // DL-08：写入侧必须与读/删侧共用同一套判据。早先写入侧完全不校验，
        // 而 `insert_with_dims` 是 pub API、文件名直接来自 `ClipItem.hash`，
        // 于是「传一个含 .. 的 hash」就是任意文件写原语。
        let (store, base) = temp_store();
        let escaped = base.join("escaped.bin");
        for bad in ["../escaped.bin", "a/b.bin", "a\\b.bin", "", "NUL", "x.bin:ads"] {
            let r = super::blobs::write_blob_atomic(&store.blob_dir, bad, b"payload");
            assert!(r.is_err(), "不合法文件名必须拒绝：{bad:?}");
        }
        assert!(!escaped.exists(), "不得写出 blobs 目录之外");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn retention_spares_pinned_items() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        for i in 0..5 {
            let mut item = ClipItem::new(
                ClipKind::Text,
                format!("h{i}"),
                format!("item {i}"),
                "a".into(),
            );
            if i == 0 {
                item.pinned = true;
            }
            store.insert(item, vec![0u8; 4]).unwrap();
        }
        let cfg = StorageConfig {
            max_items: Some(2),
            ..Default::default()
        };
        store.enforce_retention(&cfg).unwrap();
        let all = store.list(100, 0, None).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|i| i.pinned), "置顶条目必须保留");
        let _ = std::fs::remove_dir_all(base);
    }

    /// 巨大的 `max_items` 必须被理解为「不限制」，而不是删光全部条目。
    ///
    /// `max_items` 是 `usize`（64 位），直接 `as i64` 在 ≥ 2^63 时回绕成
    /// 负数，`unpinned_keep` 被 `.max(0)` 兜成 0 ⇒「保留 0 条」。
    /// 用户手填一个大数（或未来配置项改宽）就会静默丢光未置顶历史。
    #[test]
    fn retention_does_not_wipe_when_max_items_overflows_i64() {
        for huge in [usize::MAX, 1usize << 63, (1usize << 63) + 12345] {
            let (mut store, base) = temp_store();
            store.set_payload_limit(1 << 20);
            for i in 0..5 {
                let item = ClipItem::new(
                    ClipKind::Text,
                    format!("h{i}"),
                    format!("item {i}"),
                    "a".into(),
                );
                store.insert(item, vec![0u8; 4]).unwrap();
            }
            let cfg = StorageConfig {
                max_items: Some(huge),
                ..Default::default()
            };
            store.enforce_retention(&cfg).unwrap();
            let all = store.list(100, 0, None).unwrap();
            assert_eq!(all.len(), 5, "max_items={huge} 应视为不限制，却只剩 {} 条", all.len());
            let _ = std::fs::remove_dir_all(base);
        }
    }

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_eq!(fingerprint(b"abc").len(), 32);
    }

    #[test]
    fn hex_prefix_matches_reference_formatting() {
        // 查表实现的黄金值：必须与 `format!("{b:02x}")` 逐字节一致
        // （小写、两位数、不足补零）。
        let all: Vec<u8> = (0u8..=255).collect();
        let want16: String = all.iter().take(16).map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_prefix(&all, 16), want16);
        let want_all: String = all.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_prefix(&all, 999), want_all, "n 超长时应取全部字节");
        assert_eq!(hex_prefix(&[], 8), "");
    }

    #[test]
    fn delete_many_reports_actual_deleted_rows() {
        // DL-09：不存在的 id 不得计入返回值，否则 enforce_retention 的
        // 「删了几条」对任何调用方都不可信。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let mut ids = Vec::new();
        for i in 0..2 {
            let item = ClipItem::new(
                ClipKind::Text,
                format!("dm{i}"),
                format!("dm {i}"),
                "a".into(),
            );
            ids.push(store.insert(item, format!("dm {i}").into_bytes()).unwrap().id);
        }
        let n = store.delete_many(&[ids[0], ids[1], 99_999]).unwrap();
        assert_eq!(n, 2, "只应统计真正删掉的行");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn gc_reclaims_orphans_and_tmp_leftovers() {
        // DL-07：`write_blob_atomic` 的 `.tmp` 中间态（进程在 write 与
        // rename 之间被杀）永远不会被任何行引用，启动时必须回收；
        // 同时被引用的正式载荷必须原样留下。
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let item = ClipItem::new(ClipKind::Text, "keep".into(), "keep".into(), "a".into());
        let saved = store.insert(item, b"keep".to_vec()).unwrap();
        let live = saved.payload.as_ref().unwrap().file.clone();
        std::fs::write(store.blob_dir.join("deadbeef.bin.tmp"), b"half").unwrap();
        std::fs::write(store.blob_dir.join("cafebabe.bin"), b"orphan").unwrap();

        let n = store.gc_orphan_blobs().unwrap();
        assert_eq!(n, 2, "孤儿 .bin 与残留 .tmp 都应被回收");
        assert!(store.blob_dir.join(&live).exists(), "被引用的载荷必须保留");
        assert!(!store.blob_dir.join("deadbeef.bin.tmp").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn gc_tolerates_missing_blob_dir() {
        // 全新安装 / 目录被用户清掉时，GC 不该把启动流程带崩。
        let (store, base) = temp_store();
        std::fs::remove_dir_all(&store.blob_dir).unwrap();
        assert_eq!(store.gc_orphan_blobs().unwrap(), 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn group_crud_and_move() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(1 << 20);
        let gid = store.create_group("工作", "#FF0000").unwrap();
        let item = ClipItem::new(ClipKind::Text, "hg".into(), "grouped".into(), "a".into());
        let saved = store.insert(item, b"grouped".to_vec()).unwrap();
        store.move_to_group(saved.id, Some(gid)).unwrap();
        let grouped = store.list(100, 0, Some(GroupFilter::Group(gid))).unwrap();
        assert_eq!(grouped.len(), 1);
        store.delete_group(gid).unwrap();
        // 组删除后条目仍在，但变为未分组
        let after = store.get(saved.id).unwrap().unwrap();
        assert_eq!(after.group_id, None);
        let _ = std::fs::remove_dir_all(base);
    }

    /// 回归测试：`temp_store()` 不得复用上一次运行残留的数据库。
    ///
    /// 进程 ID 会被操作系统复用，目录名 `store-test-<pid>-<n>` 因此可能
    /// 撞上历史残留。`temp_store()` 曾用 `create_dir_all` 直接建目录，
    /// 旧的 `t.db` 就被保留下来，`Store::open` 会打开旧库——
    /// 于是旧条目混进当前测试，表现为随机的「命中数不对」
    /// （实测：`fts_search_finds_text` 期望 1 条却拿到 2 条）。
    ///
    /// 这里手工造一个「残留库」，再确认新 store 看不到它的数据。
    #[test]
    fn temp_store_ignores_leftover_db_from_previous_run() {
        let base = unique_dir("leftover-db");
        // 先造一个「上次运行的残留」：目录里有 t.db 与旧条目。
        {
            let mut stale = Store::open(&base.join("t.db"), &base.join(BLOB_DIR_NAME)).unwrap();
            stale.set_payload_limit(1 << 20);
            stale
                .insert(
                    ClipItem::new(ClipKind::Text, "old".into(), "剪贴板历史记录".into(), "old".into()),
                    b"stale".to_vec(),
                )
                .unwrap();
        } // 故意不删——这正是上次运行留下的残留

        // 走`temp_store()` 用的同一条准备路径。
        let blobs = prepare_dir(&base);
        let mut fresh = Store::open(&base.join("t.db"), &blobs).unwrap();
        fresh.set_payload_limit(1 << 20);
        fresh
            .insert(
                ClipItem::new(ClipKind::Text, "new".into(), "totally different".into(), "new".into()),
                b"fresh".to_vec(),
            )
            .unwrap();

        let hits = fresh.search("剪贴板", 10).unwrap();
        assert!(
            hits.is_empty(),
            "新store 读到了上次运行的残留数据，实际命中 {} 条：{:?}",
            hits.len(),
            hits.iter().map(|i| i.preview.clone()).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(base);
    }

    /// 回归测试：两次 `open_in_memory()` 必须拿到不同的blob 目录。
    ///
    /// 修复前目录名只带进程 ID，同进程内所有实例共用一个目录。
    /// 而 `clear_all()` 会删掉目录下**所有**文件，于是并行测试之间
    /// 互相删对方载荷，表现为随机的「载荷读不到」失败
    /// （`app/tests/integration.rs` 的 24 个测试全部经
    /// `Service::in_memory()` 走这条路，其中两个还会调 `clear_all`）。
    #[test]
    fn open_in_memory_gives_each_instance_its_own_blob_dir() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();

        assert_ne!(
            a.blob_dir, b.blob_dir,
            "两个内存库实例共用同一载荷目录 {}，并行测试会互相删对方载荷",
            a.blob_dir.display()
        );
        assert!(a.blob_dir.is_dir(), "载荷目录应被自动创建");
        assert!(b.blob_dir.is_dir(), "载荷目录应被自动创建");
    }

    /// 隔离性必须是真的：清空 A 不能删掉 B 的载荷。
    ///
    /// 只断言路径不同是不够的——万一两个目录名不同却仍指向同一处，
    /// 上面的断言会绿但数据照样被踩。这里实际写文件再验证存活。
    #[test]
    fn clearing_one_memory_store_keeps_other_payloads() {
        let mut a = Store::open_in_memory().unwrap();
        let mut b = Store::open_in_memory().unwrap();
        a.set_payload_limit(1 << 20);
        b.set_payload_limit(1 << 20);

        let in_a = a
            .insert(
                ClipItem::new(ClipKind::Text, "ka".into(), "in-a".into(), "app".into()),
                b"in-a".to_vec(),
            )
            .unwrap();
        let in_b = b
            .insert(
                ClipItem::new(ClipKind::Text, "kb".into(), "in-b".into(), "app".into()),
                b"in-b".to_vec(),
            )
            .unwrap();

        a.clear_all().unwrap();

        assert!(a.get(in_a.id).unwrap().is_none(), "A 自己应被清空");
        assert_eq!(
            b.load_payload(&in_b).unwrap(),
            b"in-b",
            "清空 A 删掉了 B 的载荷文件"
        );
    }

    /// in-memory store 的载荷目录随 store 一起消失，不在 `%TEMP%` 留垃圾。
    ///
    /// 用显式作用域观察 drop 前后的目录状态。
    #[test]
    fn memory_store_blob_dir_is_cleaned_when_store_drops() {
        let dir;
        {
            let store = Store::open_in_memory().unwrap();
            dir = store.blob_dir.clone();
            assert!(dir.is_dir(), "drop 前目录应存在");
        }
        assert!(
            !dir.exists(),
            "in-memory store 的载荷目录 {} 应随 store 一起清理",
            dir.display()
        );
    }

    /// 反向保证：`open_in()` 用的是用户真实数据目录，**绝不能**被 drop 删掉。
    ///
    /// 这是本次加 `Drop` 最危险的地方——一旦误删，用户剪贴板历史当场清空。
    #[test]
    fn on_disk_store_blob_dir_survives_drop() {
        let base = unique_dir("drop-keeps-data");
        let blob_dir;
        {
            let store = Store::open_in(&base).unwrap();
            blob_dir = store.blob_dir.clone();
            assert!(blob_dir.is_dir());
        }
        assert!(
            blob_dir.is_dir(),
            "真实数据载荷目录 {} 不应被 Store::drop 删除",
            blob_dir.display()
        );
        assert!(base.join(DB_FILE_NAME).is_file(), "数据库文件也不应被删");

        let _ = std::fs::remove_dir_all(&base);
    }
    /// DL-01：超限只存元数据的条目，必须能在上限调大后**补回**载荷。
    ///
    /// 这是去重链路的一环：用户把「单条载荷上限」调大后再次复制同样内容，
    /// 去重会命中旧条目而不是新插入一条。若旧条目的 `blob_file` 还是 NULL，
    /// 它会永远停在「看得见、复制必败」——没有自愈路径。
    #[test]
    fn backfill_payload_repairs_an_over_limit_entry() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(4);
        // hash 必须用**载荷的真指纹**——真实调用方（去重命中）传的就是它，
        // `backfill_payload` 也会校验这一点（防止污染同哈希共享文件）。
        let h = fingerprint(b"0123456789");
        let item = ClipItem::new(ClipKind::Text, h, "0123456789".into(), "app".into());
        let saved = store.insert(item, b"0123456789".to_vec()).unwrap();
        assert!(saved.payload.is_none(), "前置条件：超限载荷未落盘");
        // 补之前，读载荷必须失败（这正是用户遇到的「复制必败」）。
        assert!(store.load_payload(&saved).is_err(), "无载荷时读应失败");

        // 上限调大 ⇒ 可以补。
        store.set_payload_limit(1 << 20);
        let did = store.backfill_payload(saved.id, b"0123456789", None).unwrap();
        assert!(did, "上限已放宽，应当补回载荷");
        // CAUTION: 必须重新 get——`saved` 是 insert 返回的本地副本，
        // 它的 `payload` 字段不会随后端的 UPDATE 变化。
        let fresh = store.get(saved.id).unwrap().unwrap();
        assert_eq!(
            store.load_payload(&fresh).unwrap(),
            b"0123456789".to_vec(),
            "补回后必须读得到"
        );

        // 幂等：第二次补应报告「没做事」，且不破坏已有载荷。
        let again = store.backfill_payload(saved.id, b"9999999999", None).unwrap();
        assert!(!again, "已有载荷时不应重复补");
        let fresh2 = store.get(saved.id).unwrap().unwrap();
        assert_eq!(store.load_payload(&fresh2).unwrap(), b"0123456789".to_vec());

        let _ = std::fs::remove_dir_all(base);
    }

    /// DL-01 的另一半：上限**仍然太小**时不许补。
    /// 补数据不能成为绕过配置的旁路。
    #[test]
    fn backfill_payload_respects_the_current_limit() {
        let (mut store, base) = temp_store();
        store.set_payload_limit(4);
        let item = ClipItem::new(ClipKind::Text, "h3".into(), "abcdefghij".into(), "app".into());
        let saved = store.insert(item, b"abcdefghij".to_vec()).unwrap();
        assert!(saved.payload.is_none());

        // 上限仍是 4 ⇒ 不补，且不能留下没人引用的载荷文件。
        let did = store.backfill_payload(saved.id, b"abcdefghij", None).unwrap();
        assert!(!did, "上限未放宽时不得补");
        assert!(store.load_payload(&saved).is_err());
        let leftover: Vec<_> = std::fs::read_dir(base.join(BLOB_DIR_NAME))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "bin").unwrap_or(false))
            .collect();
        assert!(leftover.is_empty(), "不补时不应写出 .bin，实际 {:?}", leftover.len());

        let _ = std::fs::remove_dir_all(base);
    }
}
