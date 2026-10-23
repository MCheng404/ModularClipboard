//! 分组定义的增删改查。
//!
//! 组内条目不随分组删除：外键 `ON DELETE SET NULL` 把
//! `group_id` 置空，条目本身留在历史里。

use anyhow::Result;
use modular_clipboard_core::Group;
use modular_clipboard_core::now_ms;
use rusqlite::params;

use super::Store;

impl Store {
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


    /// 删除分组。组内条目不删除，`group_id` 被置空（外键 ON DELETE SET NULL）。
    pub fn delete_group(&mut self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
        Ok(())
    }

}
