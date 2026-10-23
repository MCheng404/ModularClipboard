//! 条目查询：分页列表、全文搜索与 LIKE 回退、行反序列化。
//!
//! `SELECT_COLS` 与 `row_to_item` 是读路径的单一来源，
//! 列序改了必须两处同步（它们按**列下标**取值）。

use anyhow::Result;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::EntryId;
use modular_clipboard_core::PayloadRef;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::params;
use rusqlite::params_from_iter;

use super::Store;
use super::{GroupFilter, parse_kind};

use super::blobs::{safe_blob_path};

impl Store {
    /// 查找指定指纹对应的**未置顶**条目（按时间倒序取最新一条）。
    ///
    /// 全局查找、不设时间窗口：无论隔了多久再次复制相同内容，
    /// 都视为重复（不新增、只置顶更新时间）。窗口语义已被全局去重
    /// 取代——剪贴板管理器对「复制同一段内容」的期望就是只留一条。
    /// > IMPORTANT: `ORDER BY` 必须带 **`id DESC` 兜底**。
    ///
    /// `created_at` 只到**毫秒**。同一毫秒内插入两条同哈希记录时
    /// 时间戳完全相同，SQLite 这时返回哪一条是**未定义**的——实测
    /// 表现为 `find_by_hash_finds_latest_unpinned` 偶发失败（有时拿
    /// 到先插入的那条）。`id` 是 `AUTOINCREMENT`，在并列时按插入
    /// 顺序取后到的那条，结果与「最新」一致。
    pub fn find_by_hash(&self, hash: &str) -> Result<Option<EntryId>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM items WHERE hash = ?1 AND pinned = 0
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![hash],
                |r| r.get::<_, EntryId>(0),
            )
            .optional()?;
        Ok(id)
    }

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
        //
        // `id DESC` 是并列兜底：`created_at` 只到毫秒，同一毫秒插入的多条
        // 记录时间戳完全相同，不加兜底时排序结果未定义（分页会重复/漏行）。
        sql.push_str(
            " ORDER BY pinned DESC, created_at DESC, id DESC LIMIT ? OFFSET ?",
        );
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

        // FTS5 trigram 分词器以 **3 字符**为最小匹配单位：1-2 字符的查询
        // 不会报错，而是静默返回 0 行——`Err` 兜底等不到它该出场的时机。
        // 中文两字词（「剪贴」）恰好处在最常用的检索长度上，必须提前闸断。
        // 判据按字符数而非字节数：一个汉字 3 字节，按字节会把两字词放过去。
        if q.chars().count() < 3 {
            return self.like_search(q, limit);
        }

        let fts_sql = format!(
            "SELECT {} FROM items
             JOIN items_fts_map m ON m.item_id = items.id
             JOIN items_fts f ON f.rowid = m.rowid
             WHERE items_fts MATCH ?1
             ORDER BY items.pinned DESC, bm25(items_fts), items.created_at DESC, items.id DESC
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
    pub(crate) fn like_search(&self, q: &str, limit: usize) -> Result<Vec<ClipItem>> {
        let pattern = format!("%{}%", Self::escape_like(q));
        let sql = format!(
            // `id DESC` 是并列兜底，与 `list()` / FTS 路径保持一致：
            // `created_at` 只到毫秒，同一毫秒插入的多条时间戳完全相同，
            // 不加兜底时排序结果未定义（同一查询两次调用可能给不同顺序）。
            "SELECT {} FROM items
             WHERE preview LIKE ?1 ESCAPE '\\'
             ORDER BY pinned DESC, created_at DESC, id DESC LIMIT ?2",
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
    pub(crate) fn escape_like(raw: &str) -> String {
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
        let path = safe_blob_path(&self.blob_dir, &p.file)
            .ok_or_else(|| anyhow::anyhow!("载荷文件名不合法：{}", p.file))?;
        std::fs::read(path).map_err(Into::into)
    }

}
