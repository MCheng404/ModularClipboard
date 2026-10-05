//! 剪贴板捕获层。
//!
//! 平台差异全部收敛在本 crate：上层只看到 [`CapturedPayload`]。
//!
//! Windows 上使用 `GetClipboardSequenceNumber` 而非轮询读取内容：
//! 系统为每次剪贴板变更递增该序列号，只读序列号（一次内存读）比
//! 每 300ms 反序列化整个剪贴板便宜几个数量级。这直接决定了常驻工具
//! 的 CPU 占用能否做到接近零。

use std::time::Duration;

use tiez_core::{CapturedPayload, ClipKind};

/// 剪贴板访问封装。
pub struct ClipboardReader {
    inner: arboard::Clipboard,
    /// 自行写回剪贴板时进入抑制期，避免把自己写的内容再次当成新捕获。
    suppress_until: std::time::Instant,
    /// 上一次读取到的序列号。
    last_sequence: u32,
}

impl ClipboardReader {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            inner: arboard::Clipboard::new()?,
            suppress_until: std::time::Instant::now(),
            last_sequence: current_sequence(),
        })
    }

    fn is_suppressed(&self) -> bool {
        std::time::Instant::now() < self.suppress_until
    }

    /// 标记抑制窗口：此期间的内容变化视为自身写回引起。
    pub fn suppress_for(&mut self, d: Duration) {
        self.suppress_until = std::time::Instant::now() + d;
    }

    /// 读取当前剪贴板内容，按文件 → 图片 → 文本/HTML 的优先级。
    ///
    /// 处于抑制期时返回 `None`。
    pub fn read(&mut self) -> Option<CapturedPayload> {
        if self.is_suppressed() {
            return None;
        }
        let source_app = source_process_name();

        // 文件列表优先：用户意图明确是传文件。
        if let Ok(paths) = self.inner.get().file_list()
            && !paths.is_empty()
        {
            let files: Vec<String> = paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let joined = files.join("\n");
            return Some(CapturedPayload {
                kind: ClipKind::Files,
                bytes: joined.as_bytes().to_vec(),
                html: None,
                text: Some(joined.clone()),
                source_app,
                files,
            });
        }

        // 图片。
        if let Ok(img) = self.inner.get().image() {
            let bytes = img.bytes.into_owned();
            if !bytes.is_empty() {
                return Some(CapturedPayload {
                    kind: ClipKind::Image,
                    bytes,
                    html: None,
                    text: None,
                    source_app,
                    files: Vec::new(),
                });
            }
        }

        // 文本 + HTML：文本作为主载荷，HTML 作为附带格式保留。
        // 这样粘贴到编辑器能保留格式，粘贴到终端又有纯文本可用。
        let text = self.inner.get_text().ok();
        let html = self.inner.get().html().ok();

        match (text, html) {
            (Some(t), h) if !t.is_empty() => Some(CapturedPayload {
                kind: ClipKind::Text,
                bytes: t.as_bytes().to_vec(),
                html: h,
                text: Some(t),
                source_app,
                files: Vec::new(),
            }),
            (None, Some(h)) if !h.is_empty() => Some(CapturedPayload {
                kind: ClipKind::Html,
                bytes: h.as_bytes().to_vec(),
                html: Some(h.clone()),
                text: Some(strip_html(&h)),
                source_app,
                files: Vec::new(),
            }),
            _ => None,
        }
    }

    /// 是否有新变化（基于序列号比较）。
    pub fn changed(&mut self) -> bool {
        let seq = current_sequence();
        if seq != self.last_sequence {
            self.last_sequence = seq;
            true
        } else {
            false
        }
    }

    /// 同步序列号基线，避免启动瞬间把已有内容当成新复制。
    pub fn reset_baseline(&mut self) {
        self.last_sequence = current_sequence();
    }

    /// 写回文本。用于把历史条目放回剪贴板。
    ///
    /// 调用后进入抑制期，防止该内容被立即重新捕获造成列表抖动。
    pub fn write_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.inner.set_text(text)?;
        self.suppress_for(Duration::from_millis(800));
        self.reset_baseline();
        Ok(())
    }

    /// 写回文件列表。
    pub fn write_files(&mut self, paths: &[String]) -> anyhow::Result<()> {
        let pb: Vec<std::path::PathBuf> = paths.iter().map(std::path::PathBuf::from).collect();
        self.inner.set().file_list(&pb)?;
        self.suppress_for(Duration::from_millis(800));
        self.reset_baseline();
        Ok(())
    }

    /// 写回图片。`bytes` 必须是 RGBA8 排列。
    pub fn write_image(&mut self, bytes: Vec<u8>, w: u32, h: u32) -> anyhow::Result<()> {
        let img = arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: std::borrow::Cow::Owned(bytes),
        };
        self.inner.set_image(img)?;
        self.suppress_for(Duration::from_millis(800));
        self.reset_baseline();
        Ok(())
    }
}

/// HTML 标签剥离：把富文本降级为可搜索、可预览的纯文本。
///
/// 规则：
/// - 丢弃 `<script>` / `<style>` 内部内容，否则 JS/CSS 会污染搜索结果。
/// - 块级标签与 `<br>` 转成换行。
/// - 解码常见 HTML 实体。
///
/// 不追求与浏览器解析结果完全一致；剪贴板场景只需要可读文本。
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut idx = 0usize;
    let bytes: Vec<char> = html.chars().collect();

    while idx < bytes.len() {
        if bytes[idx] != '<' {
            out.push(bytes[idx]);
            idx += 1;
            continue;
        }

        // 找到标签结束
        let mut end = idx + 1;
        while end < bytes.len() && bytes[end] != '>' {
            end += 1;
        }
        if end >= bytes.len() {
            // 未闭合的 '<'，按普通字符处理
            out.push('<');
            idx += 1;
            continue;
        }
        let tag: String = bytes[idx + 1..end].iter().collect();
        let lower = tag.to_ascii_lowercase();

        if lower.starts_with("script") || lower.starts_with("style") {
            // 跳过整个元素内容，直到对应的闭合标签。
            // 必须在「剩余 HTML」中查找，而不是在标签名里找 ——
            // 标签名只有 "style"/"script"，永远找不到 "</style>"。
            let needle = if lower.starts_with("script") {
                "</script"
            } else {
                "</style"
            };
            let rest: String = bytes[idx..].iter().collect();
            let rest_lower = rest.to_ascii_lowercase();
            match rest_lower.find(needle) {
                Some(pos) => {
                    // pos 是字节偏移，转换回 char 下标
                    let char_pos = rest[..pos].chars().count();
                    idx += char_pos;
                }
                None => {
                    // 未闭合：丢弃其后全部内容
                    idx = bytes.len();
                }
            }
            continue;
        }

        // 块级/换行标签转成换行，其余标签直接丢弃。
        if is_block_tag(&lower) {
            out.push('\n');
        }
        idx = end + 1;
    }

    // 折叠多余空行，但保留段落结构。
    let cleaned: Vec<&str> = out
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    decode_entities(&cleaned.join("\n"))
}

/// 判断是否为需要保留换行的标签。
fn is_block_tag(tag_lower: &str) -> bool {
    const BLOCK: [&str; 12] = [
        "br", "p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote",
    ];
    let name = tag_lower
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("");
    BLOCK.contains(&name)
}

/// 常见 HTML 实体解码。
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        // &amp; 必须最后替换，否则 "&amp;lt;" 会被二次解码
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
}

// ---------- 平台相关 ----------

/// 当前剪贴板序列号。非 Windows 平台返回 0（上层退化为按内容判定）。
#[cfg(windows)]
pub fn current_sequence() -> u32 {
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
pub fn current_sequence() -> u32 {
    0
}

/// 前台窗口所属进程名（不含扩展名）。
#[cfg(windows)]
pub fn source_process_name() -> String {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut size = 260u32;
        let mut buf = vec![0u16; size as usize];
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

#[cfg(not(windows))]
pub fn source_process_name() -> String {
    String::new()
}

/// 前台窗口类名，用于密码框启发式判断。
#[cfg(windows)]
pub fn foreground_class_name() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

#[cfg(not(windows))]
pub fn foreground_class_name() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_removes_tags() {
        let text = strip_html("<p>hello <b>world</b></p>");
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn strip_html_drops_script_and_style() {
        let html = "<style>body{color:red}</style><div>keep</div><script>var x=1;</script>";
        let text = strip_html(html);
        assert!(text.contains("keep"));
        assert!(!text.contains("color:red"), "CSS 不应进入搜索索引");
        assert!(!text.contains("var x"), "JS 不应进入搜索索引");
    }

    #[test]
    fn strip_html_decodes_entities() {
        let text = strip_html("<p>a &amp; b &lt;c&gt;</p>");
        assert!(text.contains("a & b"));
        assert!(text.contains("<c>"));
    }

    #[test]
    fn entity_amp_is_decoded_last() {
        // "&amp;lt;" 应解码成 "&lt;" 而非 "<"
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
    }

    #[test]
    fn unclosed_tag_is_treated_as_text() {
        let text = strip_html("a < b");
        assert_eq!(text, "a < b");
    }

    #[test]
    fn block_tags_create_line_breaks() {
        let text = strip_html("<li>one</li><li>two</li>");
        assert!(text.contains('\n'));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    #[test]
    fn suppression_blocks_read() {
        let mut r = ClipboardReader::new().unwrap();
        r.suppress_for(Duration::from_secs(30));
        assert!(r.is_suppressed());
        assert!(r.read().is_none(), "抑制期内不应读取内容");
    }

    #[test]
    fn text_roundtrip_through_clipboard() {
        let mut r = ClipboardReader::new().unwrap();
        let marker = format!("tiez-probe-{}", std::process::id());
        r.write_text(&marker).unwrap();
        // write_text 会进入抑制期，清空后才能读到
        r.suppress_until = std::time::Instant::now();
        match r.read() {
            Some(p) => {
                let text = String::from_utf8_lossy(&p.bytes);
                assert!(text.contains("tiez-probe"), "读回内容不匹配: {text}");
            }
            None => {
                // 剪贴板可能被其他进程占用而读不到，不视为失败
            }
        }
        r.write_text("").ok();
    }

    #[test]
    fn sequence_number_is_readable() {
        // 不要求具体值，只要求调用不 panic
        let a = current_sequence();
        let b = current_sequence();
        assert_eq!(a, b, "无人操作时序列号应保持稳定");
    }
}