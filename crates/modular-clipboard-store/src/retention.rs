//! 保留策略与容量维护：数量/字节双上限淘汰、全量清空、孤儿回收。
//!
//! 淘汰走 `delete_many` 单事务（PF-7）；置顶条目永不淘汰。

use anyhow::Result;
use modular_clipboard_core::EntryId;
use modular_clipboard_core::StorageConfig;
use rusqlite::params;

use super::Store;

impl Store {
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
            // CAUTION: 不能直接 `max_items as i64`。`max_items` 是 `usize`
            // （64 位宽），配置里填 ≥ 2^63 时强转会回绕成**负数**，
            // `unpinned_keep` 被 `.max(0)` 兜成 0，于是「保留 0 条」⇒
            // 删光全部未置顶条目。`i64::MAX` 在这里的语义就是「不限制」。
            let cap = i64::try_from(max_items).unwrap_or(i64::MAX);
            let unpinned_keep = (cap - pinned_count).max(0);

            let ids: Vec<EntryId> = {
                let mut stmt = self.conn.prepare(
                    "SELECT id FROM items WHERE pinned = 0
                     ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1",
                )?;
                let rows = stmt.query_map(params![unpinned_keep], |r| r.get::<_, EntryId>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            // 批量删（PF-7）：逐条 delete 每条一个事务，淘汰上千条即上千次
            // WAL 提交。
            removed += self.delete_many(&ids)?;
        }

        // 容量淘汰：仅统计已落盘载荷。
        //
        // 批量版（PF-7）：按「超出量」一次取一批最旧受害者，累计释放量
        // 覆盖超出量即止，单事务删除后重新核对总量——原来每条一个事务。
        loop {
            let total: i64 = self
                .conn
                .query_row("SELECT COALESCE(SUM(blob_len),0) FROM items", [], |r| r.get(0))?;
            if total as u64 <= cfg.max_bytes {
                break;
            }
            let excess = total as u64 - cfg.max_bytes;
            let mut victims: Vec<EntryId> = Vec::new();
            let mut freed = 0u64;
            {
                let mut stmt = self.conn.prepare(
                    "SELECT id, blob_len FROM items WHERE pinned = 0 AND blob_len > 0
                     ORDER BY created_at ASC, id ASC LIMIT 512",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, EntryId>(0)?, r.get::<_, i64>(1)?))
                })?;
                for row in rows {
                    let (id, len) = row?;
                    victims.push(id);
                    freed += len.max(0) as u64;
                    if freed >= excess {
                        break;
                    }
                }
            }
            // 没有可淘汰者（全是置顶/无载荷）或本批零释放：退出防死循环。
            if victims.is_empty() || freed == 0 {
                break;
            }
            removed += self.delete_many(&victims)?;
        }
        Ok(removed)
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

}
