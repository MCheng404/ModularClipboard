//! 载荷文件层：路径规范化、原子写、引用计数删除、图片尺寸嗅探。
//!
//! `safe_blob_path` 是 SEC-006 的闸门：DB 行内容不可信，
//! 含目录分量的 `blob_file` 会把读/删历史变成任意文件
//! 原语。正常文件名恒为 `{hash}.bin`。
//!
//! 同一个闸门也守在**写**侧（[`write_blob_atomic`]）：早先只有读/删
//! 两侧校验，写入侧直接把 `ClipItem.hash`（pub 字段、调用方给什么就是
//! 什么）拼进路径，构成一条「任意文件写」的纵深防御缺口（DL-08）。
//! 现在两侧共用 [`is_safe_blob_name`]，不会再出现「一边守、一边不守」。

use anyhow::Result;
use modular_clipboard_core::ClipKind;
use rusqlite::params;

use super::Store;

/// 载荷文件名是否可安全地拼进 blobs 目录。
///
/// 判据（读/删侧与写侧共用同一份）：
/// - 非空、无目录分量（`/`、`\`）、无 `..`；
/// - 无冒号：Windows 上既是盘符分隔符（`C:`）又是 NTFS 数据流
///   分隔符（`a.bin:evil`）；
/// - 不是 DOS 设备名（`NUL` / `CON` / `AUX` / `PRN` / `COM1..9` /
///   `LPT1..9`）：这些名字打开的是设备而不是文件，`write` 会写到设备上。
///
/// 正常的 `blob_file` 恒为 `{hash}.bin`（本 crate 自己写入），以上判据
/// 对它全部放行；加严只影响被篡改的库行与未来可能新增的调用方。
fn is_safe_blob_name(file: &str) -> bool {
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
        return false;
    }
    if file.contains(':') {
        return false;
    }
    // 设备名比较的是**主干**（去掉扩展名）：`NUL.txt` 在 Win32 下同样
    // 打开 NUL 设备。
    let stem = file.split('.').next().unwrap_or("");
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        return false;
    }
    true
}

/// 把 DB 行里的 `blob_file` 收敛为「blobs 目录下的一个纯文件名」。
///
/// 库文件可被外部篡改，行内容不可信：含目录分量或 `..` 的 `blob_file`
/// 会让 `blob_dir.join(file)` 逃出 blobs 目录，把「读历史载荷」变成
/// 任意文件读取原语、把「删历史」变成任意文件删除原语（SEC-006）。
/// 正常的 `blob_file` 恒为 `{hash}.bin`（本 crate 自己写入）。
pub(crate) fn safe_blob_path(blob_dir: &std::path::Path, file: &str) -> Option<std::path::PathBuf> {
    if !is_safe_blob_name(file) {
        return None;
    }
    let name = std::path::Path::new(file).file_name()?;
    if name != std::ffi::OsStr::new(file) {
        return None;
    }
    Some(blob_dir.join(name))
}

/// 原子写载荷文件：先写 `.tmp` 再 rename 覆盖。
///
/// `std::fs::write` 是 `create`(截断) + `write`：写一半失败（磁盘满、
/// 句柄异常）时原文件已被截断，内容全失。而同哈希条目共享同一载荷文件，
/// 一个文件损坏会同时摧毁多条历史记录（见 [`Store::delete`] 的引用计数）。
/// rename 在同一目录内是原子操作，要么旧内容完好、要么新内容完整。
///
/// IMPORTANT: 写入前必须过 [`is_safe_blob_name`]。这里的 `name` 由调用方
/// 从 `hash` 派生，而 `insert_with_dims` 是 pub API——不校验就等于把
/// 「任意文件写」原语暴露给未来的新调用方（DL-08）。
pub(crate) fn write_blob_atomic(blob_dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<()> {
    if !is_safe_blob_name(name) {
        anyhow::bail!("载荷文件名不合法，拒绝写入：{name:?}");
    }
    let tmp = blob_dir.join(format!("{name}.tmp"));
    let final_path = blob_dir.join(name);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// 尝试解析图片尺寸，失败返回 `None`。不解码全部像素，因此很快。
pub(crate) fn compute_dims(kind: &ClipKind, bytes: &[u8]) -> Option<(u32, u32)> {
    if !matches!(kind, ClipKind::Image) {
        return None;
    }
    // image::io::Reader 只读头部。
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

impl Store {
    /// 删除载荷文件——但只在**没有任何条目再引用它**时。
    ///
    /// # 为什么必须集中在一处
    ///
    /// 同哈希条目共享同一载荷文件（`{hash}.bin`）：置顶条目不参与去重
    /// （`find_by_hash` 只找 `pinned = 0`），`dedup=false` 时每次复制也都
    /// 插入一条。因此「删这个文件」在任何路径上都不是本地决定，必须先问
    /// 引用计数（DF-01）。这条纪律原先在 delete / delete_many /
    /// update_content / insert 失败回滚四处各写了一遍，且**四份实现并不
    /// 一致**：insert 的失败路径在查询出错时保守不删，另外三处则把错误
    /// 上抛（对 delete 尤其错——行已删掉，再报错只会让调用方以为没删成）。
    /// 收敛成一个入口后，语义只剩一种。
    ///
    /// # 失败策略
    ///
    /// 一律「保守保留 + 告警」，不返回错误：载荷文件是**可重建**的派生
    /// 数据，多留一个文件由启动时的 [`Store::gc_orphan_blobs`] 回收；
    /// 反过来误删会静默摧毁无关条目的内容，且不会自愈。
    pub(crate) fn remove_blob_if_unreferenced(&self, file: &str) {
        let Some(path) = safe_blob_path(&self.blob_dir, file) else {
            tracing::warn!(file = %file, "载荷文件名不合法，跳过删除");
            return;
        };
        let remaining: i64 = match self.conn.query_row(
            "SELECT COUNT(*) FROM items WHERE blob_file = ?1",
            params![file],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    file = %file,
                    error = %e,
                    "引用计数查询失败，保留载荷文件（留给 gc_orphan_blobs）"
                );
                return;
            }
        };
        if remaining != 0 {
            return;
        }
        // NotFound 是正常竞态（另一个进程刚删过），不算异常。
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(file = %file, error = %e, "删除载荷文件失败，留给 gc_orphan_blobs");
        }
    }
}
