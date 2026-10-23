//! 载荷文件层：路径规范化、原子写、图片尺寸嗅探。
//!
//! `safe_blob_path` 是 SEC-006 的闸门：DB 行内容不可信，
//! 含目录分量的 `blob_file` 会把读/删历史变成任意文件
//! 原语。正常文件名恒为 `{hash}.bin`。

use anyhow::Result;
use modular_clipboard_core::ClipKind;

/// 把 DB 行里的 `blob_file` 收敛为「blobs 目录下的一个纯文件名」。
///
/// 库文件可被外部篡改，行内容不可信：含目录分量或 `..` 的 `blob_file`
/// 会让 `blob_dir.join(file)` 逃出 blobs 目录，把「读历史载荷」变成
/// 任意文件读取原语、把「删历史」变成任意文件删除原语（SEC-006）。
/// 正常的 `blob_file` 恒为 `{hash}.bin`（本 crate 自己写入）。
pub(crate) fn safe_blob_path(blob_dir: &std::path::Path, file: &str) -> Option<std::path::PathBuf> {
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
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
pub(crate) fn write_blob_atomic(blob_dir: &std::path::Path, name: &str, bytes: &[u8]) -> Result<()> {
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
