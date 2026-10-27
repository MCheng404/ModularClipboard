//! 剪贴板捕获层。
//!
//! 平台差异全部收敛在本 crate：上层只看到 [`CapturedPayload`]。
//!
//! Windows 上使用 `GetClipboardSequenceNumber` 而非轮询读取内容：
//! 系统为每次剪贴板变更递增该序列号，只读序列号（一次内存读）比
//! 每 300ms 反序列化整个剪贴板便宜几个数量级。这直接决定了常驻工具
//! 的 CPU 占用能否做到接近零。

use std::time::Duration;

use modular_clipboard_core::{CapturedPayload, ClipKind};

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
                dims: None,
            });
        }

        // 图片。
        if let Ok(img) = self.inner.get().image() {
            let bytes = img.bytes.into_owned();
            if !bytes.is_empty() {
                return Some(CapturedPayload {
                    kind: ClipKind::Image,
                    // 尺寸必须在此刻留下：arboard 交出的是 DIB 解码后的
                    // 裸 RGBA8（见 CapturedPayload::dims 文档），字节本身
                    // 无法反推宽高，下游全部环节都靠这个字段。
                    dims: Some((img.width as u32, img.height as u32)),
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
            // 纯文本（无 HTML 风味）且形似 Markdown → Markdown 类。
            // 带 HTML 风味的一律按 Html：富文本编辑器复制出来的内容
            // 是渲染结果而非 Markdown 源码，不能按 Markdown 排版。
            (Some(t), h) if !t.is_empty() => Some(CapturedPayload {
                kind: if h.is_none() && looks_like_markdown(&t) {
                    ClipKind::Markdown
                } else {
                    ClipKind::Text
                },
                bytes: t.as_bytes().to_vec(),
                html: h,
                text: Some(t),
                source_app,
                files: Vec::new(),
                dims: None,
            }),
            (None, Some(h)) if !h.is_empty() => Some(CapturedPayload {
                kind: ClipKind::Html,
                bytes: h.as_bytes().to_vec(),
                html: Some(h.clone()),
                text: Some(strip_html(&h)),
                source_app,
                files: Vec::new(),
                dims: None,
            }),
            _ => None,
        }
    }

    /// 是否有新变化（基于序列号比较）。
    ///
    /// CAUTION: 本方法**无条件推进基线**。读取可能失败（剪贴板正被其他
    /// 进程持有）——先推进基线再读，失败的那次变化就永久丢了。需要
    /// 「失败可重试」语义的调用方必须用 [`Self::peek_changed`] +
    /// [`Self::commit_sequence`] 两步式。
    pub fn changed(&mut self) -> bool {
        let seq = current_sequence();
        if seq != self.last_sequence {
            self.last_sequence = seq;
            true
        } else {
            false
        }
    }

    /// 窥探序列号是否变化，**不推进基线**（DF-05）。
    ///
    /// 与 [`Self::changed`] 的区别：读取失败时基线未被消费，下一轮还能
    /// 重试——「复制瞬间剪贴板被 Office/远程桌面等短暂占用」不再永久
    /// 漏捕这一次复制。
    pub fn peek_changed(&self) -> bool {
        current_sequence() != self.last_sequence
    }

    /// 把当前序列号提交为基线。读取成功、或确认「本次变化无需读取」
    /// （如自写回抑制期）之后调用。
    pub fn commit_sequence(&mut self) {
        self.last_sequence = current_sequence();
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

/// 行首是否为 ATX 标题（`# ` ~ `###### `，后跟一个空格）。
fn is_md_heading(t: &str) -> bool {
    let n = t.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&n) && t[n..].starts_with(' ')
}

/// 行首是否为有序列表标记（`1. ` / `12) `，限 1~9 位数字）。
fn is_md_ordered_list(t: &str) -> bool {
    let n = t.chars().take_while(|c| c.is_ascii_digit()).count();
    (1..=9).contains(&n) && (t[n..].starts_with(". ") || t[n..].starts_with(") "))
}

/// 纯文本是否「看起来像 Markdown」。
///
/// # 判据为什么是「至少两处标记」
///
/// 误报的代价比漏报大：普通多行文本（日志、邮件、代码片段）被误判成
/// Markdown 后，界面会按标题/列表/代码块重新排版，用户只觉得
/// 「显示莫名其妙变了」。因此要求**至少两处**结构标记：
/// - 行首标记：标题 / 代码栏 / 引用 / 无序·有序列表，每行记一次；
/// - 行内标记：`**`、`` ` ``、`[..](..)` 任一处再记一次。
///
/// 单处标记（比如只有一行 `- item`）视为普通文本——那更可能是
/// CLI 帮助或破折号开头的一句话。
///
/// CAUTION: 只对**无 HTML 风味**的纯文本调用。富文本编辑器复制出的
/// 内容是渲染结果而非 Markdown 源码，必须留在 [`ClipKind::Html`]。
pub fn looks_like_markdown(text: &str) -> bool {
    let mut hits = 0u32;
    for line in text.lines() {
        let t = line.trim_start();
        if is_md_heading(t)
            || t.starts_with("```")
            || t.starts_with("> ")
            || t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("+ ")
            || is_md_ordered_list(t)
        {
            hits += 1;
        }
    }
    if text.contains("**") || text.contains('`') || (text.contains('[') && text.contains("](")) {
        hits += 1;
    }
    hits >= 2
}

/// HTML 标签剥离：把富文本降级为可搜索、可预览的纯文本。
///
/// 规则：
/// - 丢弃 `<script>` / `<style>` 内部内容，否则 JS/CSS 会污染搜索结果。
/// - 块级标签与 `<br>` 转成换行。
/// - 解码常见 HTML 实体。
///
/// 不追求与浏览器解析结果完全一致；剪贴板场景只需要可读文本。
///
/// # 为什么是单趟字节扫描
///
/// 旧实现把整份 HTML 收进 `Vec<char>`（UTF-8 的 4 倍内存）再逐字符推进，
/// 且**每个标签**都要 `collect::<String>()` + `to_ascii_lowercase()`
/// 各分配一次——一份带几百个 `<span style=...>` 的富文本复制（浏览器/
/// Word 的常态）就是上千次小分配。这里改为在 `&str` 上按字节游走：
/// 非标签文本整段 `push_str`，标签名用 `eq_ignore_ascii_case` 就地比较，
/// 全程只有输出串与（可能的）实体解码会分配。
///
/// CAUTION: 所有下标都必须是 **UTF-8 字符边界**。本函数只按 ASCII 字节
/// （`<`、`>`）切分，而 ASCII 字节在 UTF-8 里不可能是多字节序列的
/// 续字节，因此切点天然落在边界上；不要改成按任意字节切分。
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        if bytes[idx] != b'<' {
            // 整段普通文本一次拷走，直到下一个 '<'（或结尾）。
            let next = find_byte(bytes, idx, b'<').unwrap_or(bytes.len());
            out.push_str(&html[idx..next]);
            idx = next;
            continue;
        }

        // 找标签结束。未闭合的 '<' 按普通字符处理。
        let Some(end) = find_byte(bytes, idx + 1, b'>') else {
            out.push('<');
            idx += 1;
            continue;
        };
        let tag = &html[idx + 1..end];

        let skip_to = if starts_with_ignore_ascii_case(tag, "script") {
            Some("</script")
        } else if starts_with_ignore_ascii_case(tag, "style") {
            Some("</style")
        } else {
            None
        };
        if let Some(needle) = skip_to {
            // 跳过整个元素内容，直到对应的闭合标签。
            // 必须在「剩余 HTML」中查找，而不是在标签名里找 ——
            // 标签名只有 "style"/"script"，永远找不到 "</style>"。
            match find_ignore_ascii_case(html, end, needle) {
                Some(pos) => idx = pos,
                // 未闭合：丢弃其后全部内容。
                None => idx = bytes.len(),
            }
            continue;
        }

        // 块级/换行标签转成换行，其余标签直接丢弃。
        if is_block_tag(tag) {
            out.push('\n');
        }
        idx = end + 1;
    }

    // 折叠多余空行，但保留段落结构。
    let mut cleaned = String::with_capacity(out.len());
    for line in out.lines() {
        let l = line.trim_end();
        if l.is_empty() {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        cleaned.push_str(l);
    }
    decode_entities(&cleaned)
}

/// 从 `from` 起找第一个等于 `needle` 的字节，返回其绝对下标。
///
/// `needle` 必须是 ASCII：ASCII 字节不会出现在多字节字符内部，
/// 因此返回的下标必然是 UTF-8 字符边界。
fn find_byte(hay: &[u8], from: usize, needle: u8) -> Option<usize> {
    hay.get(from..)?
        .iter()
        .position(|b| *b == needle)
        .map(|i| from + i)
}

/// 在 `hay` 的 `from` 之后按 ASCII 大小写不敏感查找 `needle`。
///
/// `needle` 以 ASCII 开头（这里恒为 `</`），所以只需逐个候选 `<` 比较，
/// 不必先构造整段小写副本——旧实现为此复制了「剩余全文」。
fn find_ignore_ascii_case(hay: &str, from: usize, needle: &str) -> Option<usize> {
    let hay_bytes = hay.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut at = from;
    while let Some(pos) = find_byte(hay_bytes, at, b'<') {
        let end = pos + needle_bytes.len();
        if end <= hay_bytes.len() && hay_bytes[pos..end].eq_ignore_ascii_case(needle_bytes) {
            return Some(pos);
        }
        at = pos + 1;
    }
    None
}

/// `s` 是否以 `prefix` 开头（仅 ASCII 大小写不敏感）。
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    match s.as_bytes().get(..prefix.len()) {
        Some(head) => head.eq_ignore_ascii_case(prefix.as_bytes()),
        None => false,
    }
}

/// 判断是否为需要保留换行的标签。
///
/// 就地大小写不敏感比较：调用方传的是原始标签文本（可能带属性、可能
/// 大写），不能再为它分配一份小写副本。
fn is_block_tag(tag: &str) -> bool {
    const BLOCK: [&str; 12] = [
        "br", "p", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "blockquote",
    ];
    let name = tag
        .trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("");
    BLOCK.iter().any(|b| name.eq_ignore_ascii_case(b))
}

/// 常见 HTML 实体解码（单趟）。
///
/// 早先是 7 次 `str::replace` 串起来：每轮都把整串复制一遍，且**后一轮
/// 会解码前一轮产出的文本**——`&amp;nbsp;` 会被解成空格，而按 HTML
/// 规范它应该原样保留为 `&nbsp;`。单趟扫描天然没有这个二次解码问题，
/// 也把 7 次分配降到 1 次。
fn decode_entities(s: &str) -> String {
    /// 长的在前：`&nbsp;` 与 `&amp;` 前缀不重叠，但保持「最长优先」的
    /// 习惯可避免将来加入 `&amp` 这类前缀实体时踩坑。
    const ENTITIES: [(&str, &str); 7] = [
        ("&nbsp;", " "),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
    ];
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match ENTITIES.iter().find(|(k, _)| tail.starts_with(k)) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &tail[k.len()..];
            }
            // 不是已知实体：原样保留 '&'，继续扫描后面。
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
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

/// 剪贴板是否正被其他窗口持有（`OpenClipboard` 后未 `CloseClipboard`）。
///
/// 此刻任何读取都会失败，且**失败的读数不可信**——不能据此判定
/// 「这次变化无需处理」。捕获循环见到 true 时应跳过本轮、保留基线，
/// 下一轮重试（DF-05：瞬时读取失败不永久丢捕获）。
#[cfg(windows)]
pub fn clipboard_held_by_other() -> bool {
    use windows::Win32::System::DataExchange::GetOpenClipboardWindow;
    // SAFETY: 纯查询函数，无参数，不持有任何资源。
    match unsafe { GetOpenClipboardWindow() } {
        // 非空句柄 = 有窗口正持有剪贴板。
        Ok(hwnd) => !hwnd.0.is_null(),
        // 查询失败保守视为「未持有」：不影响主流程，下一轮照常尝试。
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub fn clipboard_held_by_other() -> bool {
    false
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
    fn strip_html_decodes_entities() {        let text = strip_html("<p>a &amp; b &lt;c&gt;</p>");
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
    fn strip_html_keeps_multibyte_text_intact() {
        // 单趟字节扫描的下标必须落在 UTF-8 边界上：中文/emoji 紧邻标签、
        // 紧邻被跳过的 script 块时都不能切坏字符（旧实现按 char 下标走，
        // 换实现时最容易在这里 panic 或产生替换字符）。
        let html = "<p>剪贴板</p><script>var a='板';</script><div>emoji 😀 结尾</div>";
        let text = strip_html(html);
        assert!(text.contains("剪贴板"), "中文被切坏：{text:?}");
        assert!(text.contains("emoji 😀 结尾"), "emoji 被切坏：{text:?}");
        assert!(!text.contains("var a"), "script 内容不应保留");
    }

    #[test]
    fn strip_html_handles_uppercase_tags_and_multiple_scripts() {
        // 标签大小写不敏感（`<SCRIPT>` / `<STYLE>` / `<BR>`），
        // 且多个 script/style 块要各自跳过——旧实现每个块都复制一遍
        // 「剩余全文」，这里是同一行为的低成本版本。
        let html = "<STYLE>a{}</STYLE><p>x<BR>y</p><SCRIPT>bad1</SCRIPT><script>bad2</script>尾巴";
        let text = strip_html(html);
        assert!(text.contains('x'));
        assert!(text.contains('y'));
        assert!(text.contains("尾巴"));
        assert!(!text.contains("a{}"), "style 内容不应保留");
        assert!(!text.contains("bad1") && !text.contains("bad2"), "script 内容不应保留");
    }

    #[test]
    fn strip_html_unterminated_script_drops_rest() {
        let text = strip_html("<p>keep</p><script>never closed");
        assert!(text.contains("keep"));
        assert!(!text.contains("never closed"));
    }

    #[test]
    fn entity_double_escape_is_not_decoded_twice() {
        // 单趟解码不会回头再解一遍自己产出的文本：`&amp;nbsp;` 按规范
        // 应保留为字面量 `&nbsp;`（旧实现串联 7 次 replace 会解成空格）。
        assert_eq!(decode_entities("&amp;nbsp;"), "&nbsp;");
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("裸 & 符号"), "裸 & 符号");
        assert_eq!(decode_entities("结尾 &"), "结尾 &");
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
        // 只要求调用本身能成功返回，不要求两次读数相同。
        //
        // 序列号由**系统**在每次剪贴板变更时递增，而测试进程自己就在
        // 改剪贴板（同文件里的 `text_roundtrip_through_clipboard` 会写入）。
        // 并行执行时两次读数不同是正常的，早先断言 `a == b` 会间歇性失败。
        let a = current_sequence();
        let b = current_sequence();
        // 能连续读两次说明 API 可用；序列号本身允许被其它线程/进程推进。
        assert!(a > 0 || b > 0, "序列号应可读取且非负");
    }

    // ---- Markdown 探测 ----

    #[test]
    fn markdown_detects_heading_plus_list() {
        assert!(looks_like_markdown("# 标题\n\n- 一\n- 二\n"));
    }

    #[test]
    fn markdown_detects_code_fence_and_bold() {
        assert!(looks_like_markdown("```rust\nfn main() {}\n```\n\n**加粗** 一句"));
    }

    #[test]
    fn markdown_detects_quote_plus_inline_code() {
        assert!(looks_like_markdown("> 引用一行\n\n行内 `code` 标记"));
    }

    #[test]
    fn markdown_detects_ordered_list_plus_link() {
        assert!(looks_like_markdown("1. 第一项\n2. 第二项\n\n见 [文档](https://example.com)"));
    }

    #[test]
    fn markdown_rejects_plain_multiline_text() {
        // 日志 / 邮件式多行文本：无任何结构标记
        assert!(!looks_like_markdown("第一行普通文字\n第二行还是普通文字\n第三行"));
    }

    #[test]
    fn markdown_rejects_single_marker() {
        // 只有一处标记：更可能是破折号开头的一句话或 CLI 帮助
        assert!(!looks_like_markdown("- 只是一个列表项"));
        assert!(!looks_like_markdown("这句话里有 **加粗** 而已"));
    }

    #[test]
    fn markdown_rejects_heading_without_space() {
        // `#tag` 是话题标签不是标题；`#` 后必须有空格
        assert!(!looks_like_markdown("#tag 不是标题\n另一行也没有标记"));
    }

    #[test]
    fn markdown_ignores_indented_markers() {
        // 行首允许缩进（trim_start），引用块里的列表也算
        assert!(looks_like_markdown("  - 缩进列表一\n  - 缩进列表二"));
    }

    #[test]
    fn markdown_empty_and_short_text() {
        assert!(!looks_like_markdown(""));
        assert!(!looks_like_markdown("hello"));
    }
}