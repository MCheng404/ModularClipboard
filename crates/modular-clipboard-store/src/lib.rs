//! 存储层：SQLite 元数据 + FTS5 全文索引 + 文件化载荷。
//!
//! 设计要点：
//! 1. 大载荷（图片、大段文本）不进数据库，单独存文件，库中只留引用。
//!    这让 SQLite 始终保持在几十 MB 量级，查询不会因大 blob 变慢。
//! 2. 文本类条目同步写入 FTS5 虚表，支持中文全文检索与前缀匹配。
//! 3. 所有写操作走单连接，由上层保证串行；连接本身不开WAL，
//!    因为剪贴板写入本身是低频操作，WAL 的收益不足以抵消复杂度。

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use std::path::{Path, PathBuf};
use modular_clipboard_core::{
    ClipItem, ClipKind, Config, EntryId, Group, PayloadRef, StorageConfig, now_ms,
};

/// 默认载荷上限，构造后由 `apply_config` 覆盖。
pub const MAX_PAYLOAD_DEFAULT: u64 = 32 * 1024 * 1024;

/// 计算内容指纹。用于去重与FTS 关联。
pub fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, 16)
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for b in bytes.iter().take(n) {
        s.push_str(&format!("{b:02x}"));
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
    /// 在标准数据目录下打开或创建库。
    pub fn open_default() -> Result<Self> {
        Self::open_in(
            &default_data_dir().ok_or_else(|| anyhow::anyhow!("无法确定数据目录"))?,
        )
    }

    /// 在**指定**数据目录下打开或创建库。
    ///
    /// 这是 `--data-dir` 真正落地的地方：数据库与载荷目录都由传入的
    /// `data_dir` 派生，而不是各自去问 `ProjectDirs`。
    ///
    /// 目录不存在时创建；路径为空白直接报错——静默回退到默认目录会让
    /// 用户以为数据写进了 `--data-dir` 指定的位置，实际却在 `%APPDATA%`。
    pub fn open_in(data_dir: &Path) -> Result<Self> {
        let raw = data_dir.to_string_lossy();
        if raw.trim().is_empty() {
            anyhow::bail!("数据目录不能为空");
        }
        Self::open(
            &data_dir.join(DB_FILE_NAME),
            &data_dir.join(BLOB_DIR_NAME),
        )
    }

    /// 在内存中打开，仅用于降级模式与测试。
    ///
    /// blob 目录必须**每次调用都独占**：只带进程 ID 时，同一进程内的多个实例
    /// 会共用一个目录，而 [`Store::clear_all`] 会删掉目录下所有文件——
    /// 于是并行测试之间互相删对方载荷，表现为随机的「载荷读不到」失败。
    /// 这里与测试里的 `unique_dir` 一样用「进程 ID + 原子序号」。
    pub fn open_in_memory() -> Result<Self> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let blob_dir = std::env::temp_dir()
            .join(format!("{DATA_DIR_NAME}-mem-{}-{n}", std::process::id()));
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn,
            blob_dir,
            db_path: std::path::PathBuf::from(":memory:"),
            payload_limit: std::sync::atomic::AtomicU64::new(MAX_PAYLOAD_DEFAULT),
            owns_blob_dir: true,
        };
        std::fs::create_dir_all(&store.blob_dir)?;
        store.migrate()?;
        Ok(store)
    }

    /// 指定路径打开。父目录会自动创建。
    pub fn open(db_path: &Path, blob_dir: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(blob_dir)?;
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        let store = Self {
            conn,
            blob_dir: blob_dir.to_path_buf(),
            db_path: db_path.to_path_buf(),
            payload_limit: std::sync::atomic::AtomicU64::new(
                crate::MAX_PAYLOAD_DEFAULT,
            ),
            // 目录属于调用方（通常是用户真实数据目录），Store 无权删除。
            owns_blob_dir: false,
        };
        store.migrate()?;
        Ok(store)
    }

    /// 建表与索引。幂等：重复调用安全。
    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS items (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                kind          TEXT    NOT NULL,
                hash          TEXT    NOT NULL,
                preview       TEXT    NOT NULL,
                blob_file     TEXT,
                blob_len      INTEGER NOT NULL DEFAULT 0,
                img_w         INTEGER,
                img_h         INTEGER,
                source_app    TEXT    NOT NULL DEFAULT '',
                created_at    INTEGER NOT NULL,
                pinned        INTEGER NOT NULL DEFAULT 0,
                group_id      INTEGER REFERENCES groups(id) ON DELETE SET NULL,
                use_count     INTEGER NOT NULL DEFAULT 0,
                last_used_at  INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_items_created ON items(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_items_hash    ON items(hash);
            CREATE INDEX IF NOT EXISTS idx_items_group   ON items(group_id);

            CREATE TABLE IF NOT EXISTS groups (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                name        TEXT NOT NULL UNIQUE,
                color       TEXT NOT NULL DEFAULT '#5B8DEF',
                sort_order  INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );

            -- 文本索引单独建表：图片/文件类不进FTS，避免索引膨胀。
            --
            -- 分词器选择很关键（已实测）：
            --   unicode61 把连续 CJK 视为单个 token，因此搜「剪贴板」匹配不到
            --   「剪贴板历史记录」，中文检索直接失效。
            --   trigram 按 3 字符滑窗切分，对中日韩子串检索有效，且无需词典分词。
            --   代价是索引体积约为 unicode61 的两倍，对剪贴板规模可接受。
            --
            -- content='' 表示不在 FTS 内保存正文（正文已在 items.preview），
            -- 代价是删除时必须用 'delete' 命令并提供原文，不能用 SQL DELETE。
            CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
                text,
                content='',
                tokenize='trigram'
            );

            -- FTS 与主表的关联表。
            CREATE TABLE IF NOT EXISTS items_fts_map (
                rowid  INTEGER PRIMARY KEY,
                item_id INTEGER NOT NULL UNIQUE
            );
            "#,
        )?;
        Ok(())
    }

    // ---------- 条目写入 ----------

    /// 查找指定指纹在时间窗口内的未置顶条目。
    pub fn find_recent_by_hash(&self, hash: &str, window_secs: i64) -> Result<Option<EntryId>> {
        let cutoff = now_ms() - window_secs * 1000;
        let id = self
            .conn
            .query_row(
                "SELECT id FROM items WHERE hash = ?1 AND pinned = 0 AND created_at >= ?2
                 ORDER BY created_at DESC LIMIT 1",
                params![hash, cutoff],
                |r| r.get::<_, EntryId>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// 插入新条目。载荷按类型决定是否落文件。
    ///
    /// - `text`/`html`：文本进FTS，载荷写文件（超过阈值时仅存元数据）。
    /// - `image`：原始字节写文件，不进 FTS。
    /// - `files`：路径清单写文件，不进 FTS。
    pub fn insert(&mut self, mut item: ClipItem, payload: Vec<u8>) -> Result<ClipItem> {
        let cfg_max = self.current_payload_limit();

        // 载荷落盘，超限则只保留元数据。
        let payload_ref = if payload.len() as u64 <= cfg_max {
            let dims = compute_dims(&item.kind, &payload);
            let name = format!("{}.bin", &item.hash);
            std::fs::write(self.blob_dir.join(&name), &payload)?;
            Some(PayloadRef {
                file: name,
                len: payload.len() as u64,
                dims,
            })
        } else {
            tracing::debug!(hash = %item.hash, len = payload.len(), "载荷超限，仅存元数据");
            None
        };

        let text_for_fts = match item.kind {
            ClipKind::Text | ClipKind::Html => Some(item.preview.clone()),
            _ => None,
        };

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO items
                (kind, hash, preview, blob_file, blob_len, img_w, img_h,
                 source_app, created_at, pinned, group_id, use_count, last_used_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                item.kind.as_str(),
                item.hash,
                item.preview,
                payload_ref.as_ref().map(|p| p.file.as_str()),
                payload_ref.as_ref().map(|p| p.len as i64).unwrap_or(0),
                payload_ref.as_ref().and_then(|p| p.dims).map(|d| d.0 as i64),
                payload_ref.as_ref().and_then(|p| p.dims).map(|d| d.1 as i64),
                item.source_app,
                item.created_at,
                item.pinned as i64,
                item.group_id,
                item.use_count,
                item.last_used_at,
            ],
        )?;
        let id = tx.last_insert_rowid();

        if let Some(text) = text_for_fts {
            // 显式指定 rowid = items.id，让两表键完全一致；
            // 这样删除时只需按 id 定位，不必再维护 last_insert_rowid 的对应关系。
            tx.execute(
                "INSERT INTO items_fts(rowid, text) VALUES (?1, ?2)",
                params![id, text],
            )?;
            tx.execute(
                "INSERT INTO items_fts_map(rowid, item_id) VALUES (?1, ?2)",
                params![id, id],
            )?;
        }
        tx.commit()?;

        item.id = id;
        item.payload = payload_ref;
        Ok(item)
    }

    /// 标记条目被再次使用：置顶时间戳回移但不新增条目。
    pub fn touch(&self, id: EntryId) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE items SET use_count = use_count + 1, last_used_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// 刷新条目的时间戳，让它回到列表顶部（重复内容复用时使用）。
    pub fn bump_to_top(&self, id: EntryId) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET created_at = ?2, use_count = use_count + 1, last_used_at = ?2
             WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    // ---------- 条目读取 ----------

    fn row_to_item(row: &Row<'_>) -> rusqlite::Result<ClipItem> {
        let kind_str: String = row.get(1)?;
        let blob_file: Option<String> = row.get(4)?;
        let blob_len: i64 = row.get(5)?;
        let img_w: Option<i64> = row.get(6)?;
        let img_h: Option<i64> = row.get(7)?;
        Ok(ClipItem {
            id: row.get(0)?,
            kind: parse_kind(&kind_str),
            hash: row.get(2)?,
            preview: row.get(3)?,
            payload: blob_file.map(|f| PayloadRef {
                file: f,
                len: blob_len.max(0) as u64,
                dims: match (img_w, img_h) {
                    (Some(w), Some(h)) => Some((w as u32, h as u32)),
                    _ => None,
                },
            }),
            source_app: row.get(8)?,
            created_at: row.get(9)?,
            pinned: row.get::<_, i64>(10)? != 0,
            group_id: row.get(11)?,
            use_count: row.get(12)?,
            last_used_at: row.get(13)?,
        })
    }

    const SELECT_COLS: &'static str =
        "id, kind, hash, preview, blob_file, blob_len, img_w, img_h,
         source_app, created_at, pinned, group_id, use_count, last_used_at";

    pub fn get(&self, id: EntryId) -> Result<Option<ClipItem>> {
        let sql = format!("SELECT {} FROM items WHERE id = ?1", Self::SELECT_COLS);
        let item = self
            .conn
            .query_row(&sql, params![id], Self::row_to_item)
            .optional()?;
        Ok(item)
    }

    /// 分页读取。`group` 为 `None` 时取全部（含未分组）。
    pub fn list(&self, limit: usize, offset: usize, group: Option<GroupFilter>) -> Result<Vec<ClipItem>> {
        let mut sql = format!("SELECT {} FROM items", Self::SELECT_COLS);
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        match group {
            None => {}
            Some(GroupFilter::Ungrouped) => {
                sql.push_str(" WHERE group_id IS NULL");
            }
            Some(GroupFilter::Group(gid)) => {
                sql.push_str(" WHERE group_id = ?");
                args.push(Box::new(gid));
            }
        }
        // 置顶优先，其余按时间倒序。
        sql.push_str(" ORDER BY pinned DESC, created_at DESC LIMIT ? OFFSET ?");
        args.push(Box::new(limit as i64));
        args.push(Box::new(offset as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter().map(|b| b.as_ref())), Self::row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 全文搜索。空查询返回按时间倒序的列表。
    ///
    /// FTS5 语法错误（例如用户输入了未闭合引号）不会导致失败，
    /// 而是回退为普通 LIKE 查询。
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let q = query.trim();
        if q.is_empty() {
            return self.list(limit, 0, None);
        }

        let fts_sql = format!(
            "SELECT {} FROM items
             JOIN items_fts_map m ON m.item_id = items.id
             JOIN items_fts f ON f.rowid = m.rowid
             WHERE items_fts MATCH ?1
             ORDER BY items.pinned DESC, bm25(items_fts), items.created_at DESC
             LIMIT ?2",
            Self::SELECT_COLS
        );

        match self
            .conn
            .prepare(&fts_sql)
            .and_then(|mut stmt| {
                stmt.query_map(params![q, limit as i64], Self::row_to_item)?
                    .collect::<rusqlite::Result<Vec<_>>>()
            }) {
            Ok(items) => Ok(items),
            Err(err) => {
                tracing::debug!(%err, %q, "FTS 查询失败，回退 LIKE");
                self.like_search(q, limit)
            }
        }
    }

    /// LIKE 回退查询。中文场景下 FTS 失效时的兜底。
    fn like_search(&self, q: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let pattern = format!("%{}%", Self::escape_like(q));
        let sql = format!(
            "SELECT {} FROM items
             WHERE preview LIKE ?1 ESCAPE '\\'
             ORDER BY pinned DESC, created_at DESC LIMIT ?2",
            Self::SELECT_COLS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], Self::row_to_item)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// LIKE 查询里匹配「字面量」而不是「通配符」。
    ///
    /// SQL 侧声明了 `ESCAPE '\'`，所以要用反斜杠转义三个字符。
    /// 顺序不能颠倒：**必须先转义反斜杠本身**，
    /// 否则后面新增的转义符会被它再次转义。
    ///
    /// 漏掉任何一项都会造成「语义注入」——搜索框里输入 `%`
    /// 会匹配全部条目，输入 `_` 会匹配任意单字符。
    /// 这不是 SQL 注入（整体仍是参数绑定），但结果完全不符合预期。
    fn escape_like(raw: &str) -> String {
        const ESCAPE: char = '\\';
        let mut out = String::with_capacity(raw.len() + 8);
        for c in raw.chars() {
            if matches!(c, ESCAPE | '%' | '_') {
                out.push(ESCAPE);
            }
            out.push(c);
        }
        out
    }

    /// 读取载荷字节。
    pub fn load_payload(&self, item: &ClipItem) -> Result<Vec<u8>> {
        let p = item
            .payload
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("该条目无载荷（可能已被淘汰）"))?;
        std::fs::read(self.blob_dir.join(&p.file)).map_err(Into::into)
    }

    // ---------- 更新与删除 ----------

    pub fn set_pinned(&self, id: EntryId, pinned: bool) -> Result<()> {
        self.conn
            .execute("UPDATE items SET pinned = ?2 WHERE id = ?1", params![id, pinned as i64])?;
        Ok(())
    }

    pub fn move_to_group(&self, id: EntryId, group: Option<i64>) -> Result<()> {
        self.conn
            .execute("UPDATE items SET group_id = ?2 WHERE id = ?1", params![id, group])?;
        Ok(())
    }

    /// 删除条目，同时清理 FTS 与载荷文件。
    pub fn delete(&mut self, id: EntryId) -> Result<()> {
        let file: Option<String> = self
            .conn
            .query_row("SELECT blob_file FROM items WHERE id = ?1", params![id], |r| r.get(0))
            .optional()?;

        let tx = self.conn.transaction()?;
        // contentless FTS5 表不支持 SQL DELETE，必须把原文传给 'delete' 命令。
        // rowid 同时作为 FTS 的 rowid，因此由 items.id 决定。
        let indexed: Option<String> = tx
            .query_row(
                "SELECT i.preview FROM items i
                 JOIN items_fts_map m ON m.item_id = i.id
                 WHERE i.id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(text) = indexed {
            tx.execute(
                "INSERT INTO items_fts(items_fts, rowid, text) VALUES('delete', ?1, ?2)",
                params![id, text],
            )?;
        }
        tx.execute("DELETE FROM items_fts_map WHERE item_id = ?1", params![id])?;
        tx.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        tx.commit()?;

        if let Some(f) = file {
            let path = self.blob_dir.join(f);
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    /// 清空全部历史。分组定义保留。
    pub fn clear_all(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        // contentless FTS5 不能 DELETE，重建表是最干净的全量清空方式。
        tx.execute("DROP TABLE IF EXISTS items_fts", [])?;
        tx.execute("DROP TABLE IF EXISTS items_fts_map", [])?;
        tx.execute(
            "CREATE VIRTUAL TABLE items_fts USING fts5(text, content='', tokenize='trigram')",
            [],
        )?;
        tx.execute("CREATE TABLE items_fts_map (rowid INTEGER PRIMARY KEY, item_id INTEGER NOT NULL UNIQUE)", [])?;
        tx.execute("DELETE FROM items", [])?;
        tx.commit()?;

        if self.blob_dir.exists() {
            for entry in std::fs::read_dir(&self.blob_dir)? {
                let p = entry?.path();
                if p.is_file() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(())
    }

    // ---------- 保留策略 ----------

    /// 按数量与容量双重上限淘汰。置顶条目永不淘汰。
    /// 返回被删除的条目数。
    pub fn enforce_retention(&mut self, cfg: &StorageConfig) -> Result<usize> {
        let mut removed = 0usize;

        if let Some(max_items) = cfg.max_items {
            // 置顶条目不占名额，因此单独计算需要保留的未置顶条目数。
            // 否则 LIMIT/OFFSET 会把置顶条目也算进窗口，导致多留 N 条。
            let pinned_count: i64 =
                self.conn
                    .query_row("SELECT COUNT(*) FROM items WHERE pinned = 1", [], |r| r.get(0))?;
            let unpinned_keep = (max_items as i64 - pinned_count).max(0);

            let ids: Vec<EntryId> = {
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM items WHERE pinned = 0
                     ORDER BY created_at DESC LIMIT -1 OFFSET ?1",
                )?;
                let rows = stmt.query_map(params![unpinned_keep], |r| r.get::<_, EntryId>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in ids {
                self.delete(id)?;
                removed += 1;
            }
        }

        // 容量淘汰：仅统计已落盘载荷。
        loop {
            let (total, victim): (i64, Option<EntryId>) = self.conn.query_row(
                "SELECT COALESCE(SUM(blob_len),0),
                        (SELECT id FROM items WHERE pinned = 0 AND blob_len > 0
                         ORDER BY created_at ASC LIMIT 1)
                 FROM items",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if total as u64 <= cfg.max_bytes {
                break;
            }
            match victim {
                Some(id) => {
                    self.delete(id)?;
                    removed += 1;
                }
                None => break,
            }
        }
        Ok(removed)
    }

    /// 存储统计，用于设置页展示。
    pub fn stats(&self) -> Result<(i64, i64)> {
        let (count, bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(blob_len),0) FROM items",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((count, bytes))
    }

    /// 启动时清理孤儿载荷文件（主库中无对应记录）。
    pub fn gc_orphan_blobs(&self) -> Result<usize> {
        let mut referenced: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT blob_file FROM items WHERE blob_file IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for f in rows {
                referenced.insert(f?);
            }
        }

        let mut removed = 0;
        for entry in std::fs::read_dir(&self.blob_dir)? {
            let p = entry?.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".bin") && !referenced.contains(name) && p.is_file() {
                std::fs::remove_file(&p)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    // ---------- 分组 ----------

    pub fn list_groups(&self) -> Result<Vec<Group>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, color, sort_order, created_at FROM groups
             ORDER BY sort_order ASC, created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Group {
                id: r.get(0)?,
                name: r.get(1)?,
                color: r.get(2)?,
                sort_order: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn create_group(&self, name: &str, color: &str) -> Result<i64> {
        let order: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM groups",
            [],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO groups(name, color, sort_order, created_at) VALUES (?1,?2,?3,?4)",
            params![name, color, order, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_group(&self, id: i64, name: &str) -> Result<()> {
        self.conn
            .execute("UPDATE groups SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
    }

    /// 删除分组。组内条目不删除，`group_id` 被置空（外键 ON DELETE SET NULL）。
    pub fn delete_group(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// 当前载荷体积上限。由 [`Store::set_payload_limit`] 设置。
    fn current_payload_limit(&self) -> u64 {
        self.payload_limit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置载荷上限（来自配置）。在插入前调用。
    pub fn set_payload_limit(&self, limit: u64) {
        self.payload_limit
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn path(&self) -> &Path {
        self.db_path.as_path()
    }

    /// 从默认配置应用存储设置。
    pub fn apply_config(&self, cfg: &Config) {
        self.set_payload_limit(cfg.storage.max_payload_bytes);
    }
}

/// 分组过滤条件。
#[derive(Debug, Clone, Copy)]
pub enum GroupFilter {
    Group(i64),
    Ungrouped,
}

fn parse_kind(s: &str) -> ClipKind {
    match s {
        "html" => ClipKind::Html,
        "image" => ClipKind::Image,
        "files" => ClipKind::Files,
        _ => ClipKind::Text,
    }
}

/// 尝试解析图片尺寸，失败返回 `None`。不解码全部像素，因此很快。
fn compute_dims(kind: &ClipKind, bytes: &[u8]) -> Option<(u32, u32)> {
    if !matches!(kind, ClipKind::Image) {
        return None;
    }
    // image::io::Reader 只读头部。
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

#[cfg(test)]
mod tests {
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
        std::fs::create_dir_all(&base).unwrap();
        let s = Store::open(&base.join("t.db"), &base.join("blobs")).unwrap();
        (s, base)
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

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_eq!(fingerprint(b"abc").len(), 32);
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
}