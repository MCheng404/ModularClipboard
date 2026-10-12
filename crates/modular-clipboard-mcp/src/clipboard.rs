//! 系统剪贴板读写。
//!
//! 用 `arboard`（与捕获层同一套实现），保证与程序自身的剪贴板行为一致。
//!
//! # 为什么 MCP 需要这个能力
//!
//! 本程序的核心就是剪贴板。自动化验证「复制按钮真的把内容写进剪贴板了」
//! 需要能**读**剪贴板；构造测试前置状态（先放一段特定文本再触发粘贴）
//! 需要能**写**剪贴板。缺了它，验证只能靠截图看界面，看不到系统状态。

#![cfg(windows)]

use anyhow::Result;

/// 读取剪贴板中的文本。剪贴板为空或内容不是文本时返回 `Err`。
pub fn read_text() -> Result<String> {
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| anyhow::anyhow!("打开剪贴板失败：{e}"))?;
    cb.get_text()
        .map_err(|e| anyhow::anyhow!("剪贴板中没有可读文本：{e}"))
}

/// 把文本写入剪贴板。
///
/// NOTE: 写入会触发本程序自己的捕获线程——这正是「验证复制功能」想要的
/// 副作用（能顺带确认捕获链路正常）。但若调用方只想改剪贴板、不想产生
/// 历史记录，需要自行知晓这一点。
pub fn write_text(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| anyhow::anyhow!("打开剪贴板失败：{e}"))?;
    cb.set_text(text.to_string())
        .map_err(|e| anyhow::anyhow!("写入剪贴板失败：{e}"))
}
