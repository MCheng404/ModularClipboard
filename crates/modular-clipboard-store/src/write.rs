//! 条目写入与变更：插入、就地更新、置顶、删除。
//!
//! 同哈希条目共享同一载荷文件（`{hash}.bin`），删除时必须按
//! 引用计数决定是否删文件（DF-01）；contentless FTS5 不支持
//! SQL DELETE，改索引一律「先 'delete' 命令 + 原文，再插入」。

use anyhow::Result;
use modular_clipboard_core::ClipItem;
use modular_clipboard_core::ClipKind;
use modular_clipboard_core::EntryId;
use modular_clipboard_core::PayloadRef;
use modular_clipboard_core::now_ms;
use rusqlite::OptionalExtension;
use rusqlite::params;
use rusqlite::params_from_iter;

use super::Store;
use super::{fingerprint, parse_kind};

use super::blobs::{compute_dims, write_blob_atomic};

impl Store {
    /// 插入新条目。载荷按类型决定是否落文件。
    ///
    /// - `text`/`html`：文本进FTS，载荷写文件（超过阈值时仅存元数据）。
    /// - `image`：原始字节写文件，不进 FTS。
    /// - `files`：路径清单写文件，不进 FTS。
    pub fn insert(&mut self, item: ClipItem, payload: Vec<u8>) -> Result<ClipItem> {
        self.insert_with_dims(item, payload, None)
    }

    /// 同 [`Self::insert`]，但由调用方提供图片尺寸。
    ///
    /// # 为什么需要这个入口
    ///
    /// 本项目图片载荷的规范形态是**裸 RGBA8 像素**（arboard 在 Windows 上
    /// 把 CF_DIBV5/CF_PNG 解码成 RGBA 后交出，字节里没有文件头），
    /// `compute_dims` 的格式嗅探对它必然失败——曾因此整条图片链路
    /// （预览文案 / dims 列 / 缩略图渲染）全部退化成「图片 N 字节」。
    /// 尺寸只能在捕获侧拿到（`CapturedPayload::dims`，arboard 白给
    /// `ImageData::width/height`，零成本），经此入口透传。
    ///
    /// `dims` 为 `None` 时（文本类 / 旧调用方）仍走 `compute_dims`
    /// 兜底，兼容历史编码格式数据。
    pub fn insert_with_dims(
        &mut self,
        mut item: ClipItem,
        payload: Vec<u8>,
        dims: Option<(u32, u32)>,
    ) -> Result<ClipItem> {
        let cfg_max = self.current_payload_limit();

        // 载荷落盘，超限则只保留元数据。
        //
        // 原子写（先 .tmp 再 rename）：`std::fs::write` 是 create(截断)+write，
        // 写一半失败（磁盘满）会把**已有**载荷毁掉。而同哈希条目共享同一
        // 载荷文件（置顶条目不参与去重、dedup=false 时每次复制都插入一条），
        // 一个文件损坏会同时摧毁多条历史记录。
        let payload_ref = if payload.len() as u64 <= cfg_max {
            // 显式尺寸优先；缺失才嗅探文件头。
            let dims = dims
                .filter(|(w, h)| *w > 0 && *h > 0)
                .or_else(|| compute_dims(&item.kind, &payload));
            let name = format!("{}.bin", item.hash);
            write_blob_atomic(&self.blob_dir, &name, &payload)?;
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
            // Markdown 载荷同为 UTF-8 文本，预览即原文摘要，纳入全文索引，
            // 否则搜索「剪贴板」找不到 Markdown 条目里的同一句话。
            ClipKind::Text | ClipKind::Html | ClipKind::Markdown => Some(item.preview.clone()),
            _ => None,
        };

        // DB 事务失败时尽量清掉刚写的载荷文件，免得它成为孤儿、等下次启动的
        // gc_orphan_blobs 才回收（反复失败会累积）。
        //
        // CAUTION: 载荷名是 `{hash}.bin`，与同哈希的其它条目**共享**。
        // 早先这里无条件 `remove_file`，等于「insert 失败就连带摧毁别人的
        // 载荷」——曾表现为「某次插入失败后，另一条历史读到 Err(2)」的假失败
        // （DL-02）。现在按引用计数删，与 [`Self::delete`] 同一规矩。
        let written_file = payload_ref.as_ref().map(|p| p.file.clone());
        let inserted = (|| -> Result<EntryId> {
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
            Ok(id)
        })();

        let id = match inserted {
            Ok(id) => id,
            Err(e) => {
                if let Some(f) = written_file {
                    // 只有**确认没有其它条目引用**时才删（DF-01 同款引用计数）。
                    // 查询本身失败 ⇒ 保守不删，留给 gc_orphan_blobs 回收；
                    // 反过来「查不到就删」会重复 DL-02 那个坑。
                    self.remove_blob_if_unreferenced(&f);
                }
                return Err(e);
            }
        };

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
            // 同哈希多条目共享同一载荷文件（`{hash}.bin`）：置顶条目不参与
            // 去重，`dedup=false` 时每次复制也都插入一条。删文件会摧毁仍在
            // 引用它的条目——默认配置下「置顶 → 重复制 → 删重复项」即可触发，
            // 保留策略的自动淘汰也会走到这里。只有最后一个引用消失后才删
            // （含文件名合法性校验，SEC-006；详见该方法的文档）。
            self.remove_blob_if_unreferenced(&f);
        }
        Ok(())
    }

    /// 就地更新条目内容（编辑落地，2026-10-21）。
    ///
    /// 保留 `id` / `created_at` / `pinned` / `group_id` / `use_count` /
    /// `last_used_at`——「修改内容」不是「删旧建新」：置顶状态、分组、
    /// 使用计数、以及在列表里的位置都应该留着，否则用户编辑一次文字
    /// 就被悄悄踢出置顶。
    ///
    /// 换掉的是 `kind` / `hash` / `preview` / 载荷文件 / `dims` / FTS 文本。
    /// 载荷按 [`Self::insert_with_dims`] 同款规则落盘（超限只存元数据），
    /// 旧载荷文件按引用计数清理（同哈希多条目共享 `{hash}.bin`）。
    ///
    /// # FTS 的delete-then-insert
    ///
    /// contentless FTS5 表不支持 SQL DELETE，更新索引必须先把**原文**
    /// 交给 `'delete'` 命令再插入新文本（与 [`Self::delete`] 同一规矩）。
    /// 两步在同一个事务里，不会出现「索引空了但行还在」的中间态。
    pub fn update_content(
        &mut self,
        id: EntryId,
        kind: ClipKind,
        preview: String,
        payload: Vec<u8>,
        dims: Option<(u32, u32)>,
    ) -> Result<()> {
        // 先读旧行：旧载荷文件名（清理用）与旧 FTS 原文（delete 用）。
        let old: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT i.blob_file,
                        (SELECT i2.preview FROM items i2
                         JOIN items_fts_map m ON m.item_id = i2.id
                         WHERE i2.id = i.id)
                 FROM items i WHERE i.id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((old_file, old_text)) = old else {
            anyhow::bail!("条目不存在：{id}");
        };

        let cfg_max = self.current_payload_limit();
        let hash = fingerprint(&payload);
        // 新载荷：超限时只存元数据（blob_file = NULL），与 insert 一致。
        let payload_ref = if payload.len() as u64 <= cfg_max {
            let dims = dims
                .filter(|(w, h)| *w > 0 && *h > 0)
                .or_else(|| compute_dims(&kind, &payload));
            let name = format!("{hash}.bin");
            write_blob_atomic(&self.blob_dir, &name, &payload)?;
            Some((name, payload.len() as u64, dims))
        } else {
            None
        };
        let text_for_fts = match kind {
            ClipKind::Text | ClipKind::Html | ClipKind::Markdown => Some(preview.clone()),
            _ => None,
        };

        {
            let tx = self.conn.transaction()?;
            // 1) 旧索引先删（原文必须来自库里那份，不是调用方传的）。
            if let Some(text) = old_text {
                tx.execute(
                    "INSERT INTO items_fts(items_fts, rowid, text) VALUES('delete', ?1, ?2)",
                    params![id, text],
                )?;
                tx.execute("DELETE FROM items_fts_map WHERE item_id = ?1", params![id])?;
            }
            // 2) 主行更新（保留 id 与全部统计字段）。
            tx.execute(
                "UPDATE items SET kind = ?1, hash = ?2, preview = ?3,
                    blob_file = ?4, blob_len = ?5, img_w = ?6, img_h = ?7
                 WHERE id = ?8",
                params![
                    kind.as_str(),
                    hash,
                    preview,
                    payload_ref.as_ref().map(|p| p.0.as_str()),
                    payload_ref.as_ref().map(|p| p.1 as i64).unwrap_or(0),
                    payload_ref.as_ref().and_then(|p| p.2).map(|d| d.0 as i64),
                    payload_ref.as_ref().and_then(|p| p.2).map(|d| d.1 as i64),
                    id,
                ],
            )?;
            // 3) 新索引（文本类）。rowid 仍为 items.id，与 insert 的约定一致。
            if let Some(text) = text_for_fts {
                tx.execute(
                    "INSERT INTO items_fts(rowid, text) VALUES (?1, ?2)",
                    params![id, text],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO items_fts_map(rowid, item_id) VALUES (?1, ?2)",
                    params![id, id],
                )?;
            }
            tx.commit()?;
        }

        // 旧载荷文件：与任何现存条目都不共享时才删（DF-01 同款引用计数）。
        // 注意顺序——DB 已提交，这里的 COUNT 看到的是更新后的世界；
        // 若新旧同名（内容哈希未变），本行仍引用它，计数 ≥ 1，不会误删。
        if let Some(f) = old_file {
            self.remove_blob_if_unreferenced(&f);
        }
        Ok(())
    }

    /// 给「当年因超限只存了元数据」的条目**补上载荷**（DL-01）。
    ///
    /// # 为什么需要这个入口
    ///
    /// 去重命中时只调 [`Self::bump_to_top`] 是不够的：若原条目插入时
    /// 载荷超过**当时**的上限，它的 `blob_file` 恒为 `NULL`——列表里
    /// 看得见（有 preview），但 [`Self::load_payload`] 必失败。更糟的是
    /// 用户把上限调大后再次复制同样内容**也修不好它**：去重挡住了新插入，
    /// 这条条目会永远停在「看得到、复制必败」的状态，且没有任何自愈路径。
    ///
    /// # 为什么用 `WHERE blob_file IS NULL` 而不是先读后写
    ///
    /// 条件由 SQL 自己保证，省掉一次读；重复调用（多次复制同一内容）
    /// 天然幂等——第二起 `execute` 影响 0 行，直接返回 `false`。
    ///
    /// # 写文件但不被采纳时为什么不删文件
    ///
    /// 文件名是 `{hash}.bin`，与同哈希的其它条目**共享**。删它可能摧毁
    /// 别人的载荷（DL-02 同款坑），所以留给 `gc_orphan_blobs` 回收。
    pub fn backfill_payload(
        &mut self,
        id: EntryId,
        bytes: &[u8],
        dims: Option<(u32, u32)>,
    ) -> Result<bool> {
        if bytes.is_empty() {
            return Ok(false);
        }
        // 上限仍是硬闸：不因为「补数据」就绕过配置。
        if bytes.len() as u64 > self.current_payload_limit() {
            return Ok(false);
        }
        // 文件名必须由库里的 hash 决定，而不是调用方给的——否则会写出一个
        // 谁也不引用的文件，还可能撞上别人的 `{hash}.bin`。
        // kind 也一并读出来：尺寸嗅探只对图片有意义（见 `compute_dims`）。
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT hash, kind FROM items WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((hash, kind_str)) = row else { return Ok(false) };
        // CAUTION: 文件名是 `{hash}.bin`，与同哈希的其它条目**共享**。
        // 若传入内容与库里的 hash 不符，写进去会同时摧毁所有这些条目的
        // 载荷（DF-01 同款污染）。正常调用方（去重命中）传的必然是同一份
        // 内容，但这里不信任调用方——先算指纹再决定写不写。
        if fingerprint(bytes) != hash {
            tracing::warn!(
                id,
                "回填载荷与条目 hash 不一致，拒绝写入（会污染同哈希共享文件）"
            );
            return Ok(false);
        }
        // 与 insert 同款：显式尺寸优先，缺失才嗅探（图片载荷是裸 RGBA8，
        // 嗅探必然失败，尺寸只能由捕获侧给）。
        let dims = dims
            .filter(|(w, h)| *w > 0 && *h > 0)
            .or_else(|| compute_dims(&parse_kind(&kind_str), bytes));
        let name = format!("{hash}.bin");
        write_blob_atomic(&self.blob_dir, &name, bytes)?;
        let n = self.conn.execute(
            "UPDATE items SET blob_file = ?2, blob_len = ?3, img_w = ?4, img_h = ?5
             WHERE id = ?1 AND blob_file IS NULL",
            params![
                id,
                name,
                bytes.len() as i64,
                dims.map(|d| d.0 as i64),
                dims.map(|d| d.1 as i64),
            ],
        )?;
        Ok(n > 0)
    }

    /// 批量删除（PF-7）：N 次 [`Self::delete`] 是 N 个独立事务（各带一次
    /// WAL 提交），保留策略一次淘汰上百条时成串同步 IO。这里合并为
    /// **单事务**——contentless FTS 的 `'delete'` 命令仍需逐条提供原文，
    /// 但全部在同一个事务里提交，DB 侧从 N 次事务降到 1 次。
    ///
    /// 文件删除仍逐個做引用计数检查（DF-01）：同一批里多条记录可能共享
    /// 同一载荷文件（`{hash}.bin`），提交后的计数对整批生效，语义与逐条
    /// 删除完全一致——只有最后一个引用消失才删文件。
    pub(crate) fn delete_many(&mut self, ids: &[EntryId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        // 先读后写：事务开始前取好（blob_file, FTS 原文）。
        // blob_file 对所有条目都取；FTS 原文只对进了索引的条目存在
        // （图片/文件类不进 FTS），用子查询而非 JOIN，避免漏取 blob_file。
        let mut files: Vec<String> = Vec::with_capacity(ids.len());
        let mut texts: Vec<(EntryId, String)> = Vec::with_capacity(ids.len());
        for &id in ids {
            let row: Option<(Option<String>, Option<String>)> = self
                .conn
                .query_row(
                    "SELECT i.blob_file,
                            (SELECT i2.preview FROM items i2
                             JOIN items_fts_map m ON m.item_id = i2.id
                             WHERE i2.id = i.id)
                     FROM items i WHERE i.id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((file, text)) = row {
                if let Some(f) = file {
                    files.push(f);
                }
                if let Some(t) = text {
                    texts.push((id, t));
                }
            }
        }

        let deleted;
        {
            let tx = self.conn.transaction()?;
            for (id, text) in &texts {
                tx.execute(
                    "INSERT INTO items_fts(items_fts, rowid, text) VALUES('delete', ?1, ?2)",
                    params![id, text],
                )?;
            }
            // IN 列表的占位符数量由 ids.len() 决定——SQL 文本里只有 `?`，
            // 没有任何外部输入，格式拼接是安全的。
            let marks = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                &format!("DELETE FROM items_fts_map WHERE item_id IN ({marks})"),
                params_from_iter(ids.iter()),
            )?;
            // 返回**实际**删除的行数（DL-09）：传入不存在的 id 时
            // `DELETE ... WHERE id IN (...)` 影响 0 行，早先直接
            // `Ok(ids.len())` 会把它们也算进去，使 `enforce_retention`
            // 的「删了几条」口径对任何调用方都不可信。
            deleted = tx.execute(
                &format!("DELETE FROM items WHERE id IN ({marks})"),
                params_from_iter(ids.iter()),
            )?;
            tx.commit()?;
        }

        for f in &files {
            // 引用计数 + 路径规范化（SEC-006）统一在 store 侧一处完成。
            self.remove_blob_if_unreferenced(f);
        }
        Ok(deleted)
    }

}
