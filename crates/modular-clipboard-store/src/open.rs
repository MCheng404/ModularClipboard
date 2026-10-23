//! 库的打开、建表迁移与位置访问。
//!
//! `open_in_memory` 的载荷目录必须每次独占（带 pid + 原子序号 +
//! 随机分量，SEC-005），否则并行测试会互相删对方的载荷文件。

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

use super::Store;
use super::{BLOB_DIR_NAME, DATA_DIR_NAME, DB_FILE_NAME, MAX_PAYLOAD_DEFAULT, default_data_dir};

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


    /// 数据目录（db 文件所在目录）。
    ///
    /// 关于页「数据目录」超链接要展示并打开它；`--data-dir` 自定义
    /// 目录时也以这里为准（不是写死的 `%APPDATA%`）。
    pub fn data_dir(&self) -> &Path {
        self.db_path
            .parent()
            .expect("db_path 总带文件名，parent 恒存在")
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
        // SEC-005：目录名带随机分量。「pid + 序号」是可预测的——本地攻击者
        // 能预置 junction，让降级模式下写入的明文剪贴板载荷落进任意目录。
        // RandomState 每进程随机播种，hash_one 混入 pid 与序号后不可预测。
        use std::hash::{BuildHasher, RandomState};
        let rand = RandomState::new().hash_one((std::process::id(), n));
        let blob_dir = std::env::temp_dir().join(format!(
            "{DATA_DIR_NAME}-mem-{}-{n}-{rand:016x}",
            std::process::id()
        ));
        // 路径若已存在且是链接（symlink / junction），拒绝：create_dir_all
        // 对已存在的 reparse point 不报错，写入会静默落到链接目标。
        if let Ok(meta) = std::fs::symlink_metadata(&blob_dir) {
            anyhow::ensure!(
                !meta.file_type().is_symlink(),
                "载荷目录已被预置为链接，拒绝写入：{}",
                blob_dir.display()
            );
        }
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
            -- 列表查询的排序键（PF-6）：`list()` 固定
            -- `ORDER BY pinned DESC, created_at DESC`，没有这个复合索引时
            -- 每次查询都对全表（上限 1 万条）做一次 temp B-tree 排序。
            -- 与 idx_items_created 并存不浪费：前者服务单列倒序扫描，
            -- 本索引服务 (pinned, created_at) 的两列组合。
            CREATE INDEX IF NOT EXISTS idx_items_pinned_created
                ON items(pinned DESC, created_at DESC);

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


    pub fn path(&self) -> &Path {
        self.db_path.as_path()
    }

}
