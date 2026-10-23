//! 媒体类型判定。
//!
//! 这一层**只做字符串判定**，不解码、不读盘——放在 core 是为了让
//! 捕获/入库侧（`app`）与绘制侧（`ui`）共用同一份白名单：两边各自
//! 维护一份，迟早会漂移成「这边判成图片、那边解不出来」。

/// 能解码的图片文件扩展名（小写）。
///
/// # 必须与 `image` crate 启用的 feature 严格对齐
///
/// 根 `Cargo.toml` 里 `image` 是 `default-features = false` +
/// `["png", "jpeg", "webp", "bmp", "gif"]`。**列进来的必须真能解**：
/// 判成图片却解不出来，条目只会在「缩略图」与「解码失败」之间反复
/// 横跳，比干脆按文本显示更糟。加/去格式时两侧一起改——UI 侧有测试
/// 逐个格式编码再解码，白名单写错会被抓到。
///
/// # 为什么按扩展名而不是「读进来嗅探魔数」
///
/// 廉价预筛。资源管理器复制的文件可以是任何东西（`.exe` / `.zip` /
/// `.txt`），对它们读全文件再嗅探失败，等于每遇一次就白读一次磁盘。
/// 扩展名是 `O(1)` 的字符串判定，能挡掉绝大多数非图片。
///
/// 反过来，扩展名不保证内容真是图片（改名过的文件）——那种情况由解码
/// 失败兜住（降级成文本显示），不影响正确性。
pub const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// 路径是否指向一个**能解码的图片文件**（只看扩展名，不读盘）。
///
/// 两处调用方：
/// - 入库侧：给文件型条目嗅尺寸前先筛一遍，别对 `.txt` 也去读文件头；
/// - 绘制侧：文件型条目（载荷只有路径列表）判断要不要走缩略图分支。
///
/// 大小写不敏感（`Path::extension` 保留原样，这里统一转小写）。
pub fn is_image_file(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTS.contains(&e.trim().to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_image_extensions_case_insensitively() {
        for p in ["a.png", "a.PNG", "a.jpg", "a.JPEG", "a.webp", "a.bmp", "a.gif"] {
            assert!(is_image_file(p), "{p} 应判为图片");
        }
        assert!(is_image_file(r"C:\Users\me\Pictures\shot 2026.Png"));
        assert!(is_image_file(r"\\?\D:\tmp\frame.JPG"));
    }

    #[test]
    fn rejects_non_images() {
        for p in [
            "notes.txt",
            "main.rs",
            "archive.zip",
            "setup.exe",
            "song.mp4",
            "noext",
            r"C:\dir\sub",
            "a.svg",  // 矢量图：image crate 未启用
            "a.heic", // 手机照片常见，同样未启用
            "a.png 等 5 个文件", // 多文件条目的 preview 摘要
        ] {
            assert!(!is_image_file(p), "{p} 不应判为图片");
        }
        assert!(!is_image_file(""));
        // 隐藏文件（`.png` 前面没有文件名）不算图片路径。
        assert!(!is_image_file(".png"));
    }
}
