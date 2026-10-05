//! 跨层事件定义。
//!
//! 捕获层与平台层只负责「发出事件」，不直接调用界面；
//! 界面层只消费事件，不主动轮询后端。
//! 由此保证新增后端能力时界面无需改动。

use serde::{Deserialize, Serialize};
use crate::item::{ClipItem, EntryId};

/// 捕获层 → 上层：发现一条新内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedPayload {
    pub kind: crate::item::ClipKind,
    /// 载荷字节（文本为 UTF-8，图片为原始编码）。
    pub bytes: Vec<u8>,
    /// HTML 原文，若剪贴板同时提供。
    pub html: Option<String>,
    /// 解析后的文本，若有。
    pub text: Option<String>,
    pub source_app: String,
    /// 文件路径，若为文件类。
    pub files: Vec<String>,
}

/// 应用内流转的事件。
#[derive(Debug, Clone)]
pub enum AppEvent {
    /// 捕获层产出候选内容（尚未过滤/去重/入库）。
    Captured(CapturedPayload),
    /// 新条目已入库。
    Inserted(Box<ClipItem>),
    /// 已有条目被复用（重复内容仅更新计数与时间）。
    Reused(EntryId),
    /// 条目元数据更新。
    Updated(Box<ClipItem>),
    /// 条目被删除。
    Deleted(EntryId),
    /// 分组变化。
    GroupsChanged,
    /// 托盘/快捷键触发：显示主窗口。
    ShowWindow,
    /// 触发：隐藏主窗口。
    HideWindow,
    /// 触发：粘贴上一条。
    PastePrevious,
    /// 需要退出应用。
    Quit,
    /// 需要把所有已存内容重新写回系统剪贴板（切换到指定条目）。
    SetClipboard(EntryId),
    /// 通知性消息，显示在状态栏。
    Notice(String),
    /// 配置变更，需持久化。
    ConfigChanged,
}

/// 界面层 → 后端：用户操作意图。
#[derive(Debug, Clone)]
pub enum Command {
    Insert(Box<ClipItem>),
    Delete(EntryId),
    TogglePin(EntryId),
    MoveToGroup(EntryId, Option<i64>),
    RenameGroup(i64, String),
    CreateGroup(String),
    DeleteGroup(i64),
    ClearAll,
    SetClipboard(EntryId),
    PastePrevious,
    ConfigChanged,
    Quit,
}

/// 捕获层对外暴露的判定结果。
#[derive(Debug, Clone, PartialEq)]
pub enum IngestDecision {
    /// 接受并入库。
    Accept,
    /// 与既有条目重复，复用该 ID 并刷新计数。
    Duplicate(EntryId),
    /// 拒绝并说明原因。
    Reject(DropReason),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropReason {
    Empty,
    TooLarge,
    AppBlocked,
    SecretField,
    Disabled,
}