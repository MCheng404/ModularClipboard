//! 全局热键绑定（两个槽位共用这一结构）与键名规范化。

use serde::Deserialize;
use serde::Serialize;

/// 单个快捷键绑定。
///
/// NOTE: 容器级 `#[serde(default)]`：字段缺失时用 `Hotkey::default()` 补齐。
/// 缺了它，任何新增字段都会让旧config.json 解析失败，
/// 而 [`Config::load`] 会静默回退默认值——用户的快捷键**直接消失**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkey {
    pub key: String,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

impl Hotkey {
    /// 序列化为 `Ctrl+Shift+V` 形式，供 UI 显示。
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.win {
            parts.push("Win".to_string());
        }
        parts.push(self.key.to_uppercase());
        parts.join("+")
    }
}

impl Hotkey {
    /// 解析 `Ctrl+Shift+V` 形式的绑定文本。
    ///
    /// 大小写与分隔符都宽松：`ctrl + shift + v`、`Ctrl+Shift+V`、空格都接受，
    /// 因为用户是**手打**这段文本的，苛刻的格式要求只会换来「保存不了」。
    ///
    /// 顺序任意（`V+Ctrl` 也行），但主键必须是最后那个非修饰键的记号。
    /// 没有修饰键的组合会被拒绝——`V` 单独注册成全局热键会劫持整个键盘。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut win = false;
        let mut key = String::new();

        // 空格与 `+` 都当分隔符：用户手打时两种写法都会出现。
        let text = text.replace(' ', "+");
        for part in text.split('+') {
            let t = part.trim();
            if t.is_empty() {
                continue;
            }
            match t.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" | "menu" => alt = true,
                "win" | "super" | "meta" => win = true,
                other => {
                    if !key.is_empty() {
                        return Err(format!("快捷键里有多个主键：{key} 与 {other}"));
                    }
                    key = other.to_string();
                }
            }
        }

        if key.is_empty() {
            return Err("缺少主键（例如 Ctrl+Shift+V 里的 V）".into());
        }
        if !ctrl && !shift && !alt && !win {
            return Err("至少需要一个修饰键（Ctrl/Shift/Alt/Win），否则会劫持整个键盘".into());
        }
        let out = Self {
            key: normalize_key(&key)?,
            ctrl,
            shift,
            alt,
            win,
        };
        Ok(out)
    }
}

impl Default for Hotkey {
    /// 唤起主窗口的默认键：**Win+V**。
    ///
    /// 与 Windows 自带的「剪贴板历史」同一个键位——这是刻意的：
    /// 用户本来就会按 Win+V，接管它才能让「换工具」没有学习成本。
    ///
    /// 代价要知道：**系统可能已经占用这个组合**，那时注册会失败，
    /// 界面会明确提示（见 `hotkey::register` 的错误处理），
    /// 用户可以换成别的组合。
    fn default() -> Self {
        Self {
            key: "V".into(),
            ctrl: false,
            shift: false,
            alt: false,
            win: true,
        }
    }
}

/// 把用户写的主键名规范成 `V` / `5` / `F5` 形式。
fn normalize_key(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("主键为空".into());
    }
    // F1..F24
    if let Some(rest) = t.strip_prefix(['f', 'F'])
        && let Ok(n) = rest.parse::<u32>()
        && (1..=24).contains(&n)
    {
        return Ok(format!("F{n}"));
    }
    // 单个字母或数字
    let mut chars = t.chars();
    let c = chars.next().unwrap_or('\0');
    if chars.next().is_some() {
        return Err(format!("主键 {t:?} 不是单个按键（仅支持 A-Z、0-9、F1-F24）"));
    }
    if c.is_ascii_alphanumeric() {
        Ok(c.to_ascii_uppercase().to_string())
    } else {
        Err(format!("主键 {c:?} 不受支持（仅支持 A-Z、0-9、F1-F24）"))
    }
}
