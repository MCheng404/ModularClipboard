//! 领域模型：所有跨层传递的数据结构。
//!
//! 本crate 刻意不依赖任何 IO、GUI 或平台 crate，因此可以被全部下游层引用，
//! 而不会引入循环依赖或把平台细节泄漏到核心逻辑中。

use serde::{Deserialize, Serialize};

/// 条目 ID。单库自增，不跨设备复用。
pub type EntryId = i64;

/// 剪贴板内容的分类。决定预览渲染方式与可执行动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    /// 纯文本 / UTF-8
    Text,
    /// 富文本 HTML片段
    Html,
    /// 图片，载荷为原始编码字节
    Image,
    /// 文件路径列表
    Files,
}

impl ClipKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Html => "html",
            ClipKind::Image => "image",
            ClipKind::Files => "files",
        }
    }
}

/// 一条剪贴板历史记录。
///
/// 大字段（图片字节、大段文本）不直接存在这里，而是由 [`EntryPayload`]
/// 在存储层落盘，内存中仅保留指纹与摘要，避免长驻内存膨胀。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipItem {
    pub id: EntryId,
    pub kind: ClipKind,
    /// 内容指纹（十六进制 SHA-256 前 16 字节），用于去重。
    pub hash: String,
    /// 预览用文本：文本类为原文摘要，图片类为尺寸描述，文件类为路径摘要。
    pub preview: String,
    /// 载荷在存储层的位置。`None` 表示载荷过大已被淘汰，仅保留元数据。
    pub payload: Option<PayloadRef>,
    /// 来源进程名，用于过滤规则与展示。
    pub source_app: String,
    /// Unix 毫秒时间戳。
    pub created_at: i64,
    /// 置顶条目不参与自动淘汰，也不参与指纹去重。
    pub pinned: bool,
    /// 所属分组，`None` 表示未分组。
    pub group_id: Option<i64>,
    /// 累计使用次数，用于「最近常用」排序。
    pub use_count: i64,
    /// 最近一次被粘贴的时间。
    pub last_used_at: Option<i64>,
}

impl ClipItem {
    /// 新条目的公共字段构造。
    pub fn new(kind: ClipKind, hash: String, preview: String, source_app: String) -> Self {
        Self {
            id: 0,
            kind,
            hash,
            preview,
            payload: None,
            source_app,
            created_at: now_ms(),
            pinned: false,
            group_id: None,
            use_count: 0,
            last_used_at: None,
        }
    }

    /// 一行预览：把多行内容压成单行，便于列表显示。
    ///
    /// 先把 CRLF 整体折叠为单个空格，否则 Windows 风格的换行会产生双空格。
    pub fn one_line_preview(&self) -> String {
        let flat = self.preview.replace("\r\n", " ").replace(['\n', '\r'], " ");
        if flat.chars().count() <= 120 {
            flat
        } else {
            let truncated: String = flat.chars().take(120).collect();
            format!("{truncated}…")
        }
    }
}

/// 载荷在存储层的位置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadRef {
    /// 存储目录下的相对文件名。
    pub file: String,
    /// 字节数。
    pub len: u64,
    /// 图片类载荷的宽高，用于列表占位与缩略图。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<(u32, u32)>,
}

/// 分组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    /// 十六进制颜色，`#RRGGBB`。
    pub color: String,
    pub sort_order: i64,
    pub created_at: i64,
}

/// 捕获到但被规则丢弃的结果统计，用于设置页展示「拦截了多少」。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DropStats {
    pub empty: u64,
    pub duplicate: u64,
    pub too_large: u64,
    pub app_blocked: u64,
    pub secret_field: u64,
}

impl DropStats {
    pub fn total(&self) -> u64 {
        self.empty + self.duplicate + self.too_large + self.app_blocked + self.secret_field
    }
}

/// 当前单调时钟毫秒。抽成函数便于测试替换。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_newlines() {
        let mut item = ClipItem::new(
            ClipKind::Text,
            "h".into(),
            "line1\nline2\r\nline3".into(),
            "app".into(),
        );
        item.preview = "line1\nline2\r\nline3".into();
        assert_eq!(item.one_line_preview(), "line1 line2 line3");
    }

    #[test]
    fn preview_truncates_with_ellipsis() {
        let mut item = ClipItem::new(ClipKind::Text, "h".into(), "x".repeat(300), "a".into());
        item.preview = "x".repeat(300);
        let out = item.one_line_preview();
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 121);
    }

    #[test]
    fn kind_roundtrips_through_json() {
        for kind in [ClipKind::Text, ClipKind::Html, ClipKind::Image, ClipKind::Files] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<ClipKind>(&json).unwrap(), kind);
        }
    }
}