//! 联动引擎：根据条目内容推断可执行动作。
//!
//! 这是「软件联动」的核心。设计目标是**新增动作不改任何现有代码**：
//! 只要向 [`ActionRegistry`] 注册一个新 [`Action`]，界面与规则系统
//! 立即就能展示并调用它。
//!
//! 引擎本身不执行副作用，只产出 [`ActionProposal`]，由上层决定是否执行。
//! 这样引擎可以脱离文件系统与网络被纯函数式测试。


use modular_clipboard_core::{ActionRule, ClipItem, ClipKind};
use modular_clipboard_core::i18n::msg;

/// 动作的唯一标识。
pub type ActionId = &'static str;

/// 可执行动作。
///
/// 实现者只描述「我能做什么、是否接受该条目、生成什么计划」，
/// 真正的系统调用由上层 `ActionRegistry::dispatch` 统一执行。
pub trait Action: Send + Sync {
    /// 稳定标识，用于配置持久化。
    fn id(&self) -> ActionId;

    /// 界面显示名（双语对）。
    fn label(&self) -> modular_clipboard_core::i18n::Msg;

    /// 是否接受该条目。
    fn accepts(&self, item: &ClipItem) -> bool;

    /// 生成执行计划。`item` 是被选中的条目。
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan>;
}

/// 执行计划。
#[derive(Debug, Clone, PartialEq)]
pub enum ActionPlan {
    /// 用系统默认程序打开（文件或路径）。
    OpenExternal(String),
    /// 用浏览器打开 URL。
    OpenUrl(String),
    /// 在资源管理器中定位文件。
    RevealInExplorer(String),
    /// 运行外部命令，`{selection}` 会被替换为条目文本。
    RunCommand { program: String, arg: String },
    /// 仅写回剪贴板。
    CopyToClipboard,
    /// 什么都不做，仅提示用户手动处理。
    Notice(String),
}

// ---------- 内置动作 ----------

/// 用默认程序打开。适用于文件路径与可打开的文件。
pub struct OpenExternalAction;

impl Action for OpenExternalAction {
    fn id(&self) -> ActionId {
        "open_external"
    }
    fn label(&self) -> modular_clipboard_core::i18n::Msg {
        msg("Open")
    }
    fn accepts(&self, item: &ClipItem) -> bool {
        // 文件类直接接受；文本类需形如路径。
        match item.kind {
            ClipKind::Files => true,
            ClipKind::Text => extract_path(item).is_some(),
            _ => false,
        }
    }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        match item.kind {
            ClipKind::Files => {
                let first = item.preview.lines().next()?;
                Some(ActionPlan::OpenExternal(first.to_string()))
            }
            _ => extract_path(item).map(ActionPlan::OpenExternal),
        }
    }
}

/// 浏览器打开 URL。
pub struct OpenUrlAction;

impl Action for OpenUrlAction {
    fn id(&self) -> ActionId {
        "open_url"
    }
    fn label(&self) -> modular_clipboard_core::i18n::Msg {
        msg("Open in browser")
    }
    fn accepts(&self, item: &ClipItem) -> bool {
        item.kind == ClipKind::Text
            && extract_path(item)
                .is_none()
            && modular_clipboard_platform::looks_like_url(item.preview.trim())
    }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        let url = item.preview.trim();
        modular_clipboard_platform::looks_like_url(url).then(|| ActionPlan::OpenUrl(url.to_string()))
    }
}

/// 在资源管理器中定位文件。
pub struct RevealAction;

impl Action for RevealAction {
    fn id(&self) -> ActionId {
        "reveal"
    }
    fn label(&self) -> modular_clipboard_core::i18n::Msg {
        msg("Show in file manager")
    }
    fn accepts(&self, item: &ClipItem) -> bool {
        match item.kind {
            ClipKind::Files => true,
            ClipKind::Text => extract_path(item).is_some(),
            _ => false,
        }
    }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        match item.kind {
            ClipKind::Files => item.preview.lines().next().map(|s| ActionPlan::RevealInExplorer(s.to_string())),
            _ => extract_path(item).map(ActionPlan::RevealInExplorer),
        }
    }
}

/// 把文本送入外部命令（联动规则的通用入口）。
///
/// `id` 必须每个实例唯一：早前两个内置实例共用 `"run_command"`，
/// 第二个注册被 [`ActionRegistry::register`] 的重复 id 守卫静默拒绝
/// ——「翻译（DeepL）」内置动作其实从未注册过。
pub struct CommandAction {
    id: &'static str,
    program: String,
    arg_template: String,
    label: modular_clipboard_core::i18n::Msg,
}

impl CommandAction {
    pub fn new(
        id: &'static str,
        label: modular_clipboard_core::i18n::Msg,
        program: impl Into<String>,
        arg_template: impl Into<String>,
    ) -> Self {
        Self {
            id,
            program: program.into(),
            arg_template: arg_template.into(),
            label,
        }
    }
}

impl Action for CommandAction {
    fn id(&self) -> ActionId {
        self.id
    }
    fn label(&self) -> modular_clipboard_core::i18n::Msg {
        self.label
    }
    fn accepts(&self, item: &ClipItem) -> bool {
        matches!(item.kind, ClipKind::Text | ClipKind::Files)
    }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        let selection = item.preview.trim();
        Some(ActionPlan::RunCommand {
            program: self.program.clone(),
            // `{selection_urlencoded}`：percent-encode 后的文本，供 URL
            // 模板使用。剪贴板内容进入命令行前必须编码，否则 `&`/`%`
            // 等字符会改变命令结构（见 platform::run_command 的安全说明）。
            arg: self
                .arg_template
                .replace("{selection_urlencoded}", &percent_encode(selection))
                .replace("{selection}", selection),
        })
    }
}

/// 按 URL 分量规则 percent-encode：未保留字符全部编成 `%XX`。
///
/// 自实现而非引 crate：保留集固定（RFC 3986 unreserved），一个函数足够。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 动作注册表。持有全部可用动作。
#[derive(Default)]
pub struct ActionRegistry {
    actions: Vec<Box<dyn Action>>,
}

impl ActionRegistry {
    /// 注册内置动作。
    pub fn with_builtins() -> Self {
        let mut reg = Self::default();
        // 内置动作 id 互不重复，因此忽略返回值是安全的；
        // 重复 id 由 `register` 的测试守卫。
        let _ = reg.register(Box::new(OpenUrlAction));
        let _ = reg.register(Box::new(OpenExternalAction));
        let _ = reg.register(Box::new(RevealAction));
        // URL 类内置动作用 `explorer`（真实可执行文件）+ percent-encode：
        // 早前用 `start`（cmd 内建命令）+ 裸 `{selection}`，既依赖 shell
        // 解析（`&` 截断、命令注入），`start` 也无法被 argv 直传 spawn。
        let _ = reg.register(Box::new(CommandAction::new(
            "run_command",
            msg("Search in browser"),
            "explorer",
            "https://www.bing.com/search?q={selection_urlencoded}",
        )));
        let _ = reg.register(Box::new(CommandAction::new(
            "translate_deepl",
            msg("Translate (DeepL web)"),
            "explorer",
            "https://www.deepl.com/translator#en/zh/{selection_urlencoded}",
        )));
        reg
    }

    /// 注册新动作。重复 id 会返回错误，避免规则指向歧义实现。
    pub fn register(&mut self, action: Box<dyn Action>) -> Result<(), &'static str> {
        let id = action.id();
        if self.actions.iter().any(|a| a.id() == id) {
            return Err("动作 id 重复");
        }
        self.actions.push(action);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&dyn Action> {
        self.actions.iter().find(|a| a.id() == id).map(|a| a.as_ref())
    }

    pub fn all(&self) -> &[Box<dyn Action>] {
        &self.actions
    }

    /// 找出所有接受该条目的动作，按注册顺序返回。
    pub fn applicable(&self, item: &ClipItem) -> Vec<&dyn Action> {
        self.actions
            .iter()
            .filter(|a| a.accepts(item))
            .map(|a| a.as_ref())
            .collect()
    }
}

/// 从条目文本中提取路径。仅接受单行、长度合理、指向已存在路径的候选。
fn extract_path(item: &ClipItem) -> Option<String> {
    let t = item.preview.trim();
    if t.is_empty() || t.contains('\n') || t.len() > 260 {
        return None;
    }
    let looks = modular_clipboard_platform::looks_like_path(t)
        || (t.starts_with("\\\\")
            && t.len() > 3); // UNC 路径
    looks.then(|| t.to_string())
}

/// 规则匹配结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ActionProposal {
    /// 触发的动作标识。
    pub action: ActionId,
    /// 界面显示名。
    pub label: String,
    /// 执行计划。
    pub plan: ActionPlan,
    /// 命中的规则 ID，`None` 表示来自内置推断而非用户规则。
    pub rule_id: Option<i64>,
}

/// 为条目生成建议动作。
///
/// 先执行用户配置的显式规则（顺序由 `sort_order` 决定），
/// 再补充内置的智能推断，因此用户规则总能覆盖默认行为。
pub fn propose(
    registry: &ActionRegistry,
    item: &ClipItem,
    rules: &[ActionRule],
) -> Vec<ActionProposal> {
    let mut out: Vec<ActionProposal> = Vec::new();
    let text = item.preview.trim();

    // 1. 用户显式规则。
    let mut sorted: Vec<&ActionRule> = rules.iter().collect();
    sorted.sort_by_key(|r| r.sort_order);

    for rule in sorted {
        let kind_ok = rule.kind.is_none_or(|k| k == item.kind);
        if !kind_ok {
            continue;
        }
        // 关键词为空表示匹配所有条目。
        let keyword_ok = match rule.keyword.as_deref().map(str::trim) {
            None | Some("") => true,
            Some(k) => text.to_lowercase().contains(&k.to_lowercase()),
        };
        if !keyword_ok {
            continue;
        }
        let Some(action) = registry.get(&rule.action) else {
            tracing::warn!(action = %rule.action, "规则指向未注册的动作，已跳过");
            continue;
        };
        if let Some(plan) = action.plan(item) {
            // 规则自带的 `arg` 是命令行模板，对 RunCommand 是**覆盖**而非
            // 补充：用户配的模板必须生效，否则「联动规则可自定义命令」
            // 只是摆设（rule.arg 曾经只写不读，规则永远跑内置模板）。
            let plan = match (rule.arg.as_deref(), &plan) {
                (Some(arg), ActionPlan::RunCommand { program, .. }) if !arg.trim().is_empty() => {
                    let selection = item.preview.trim();
                    ActionPlan::RunCommand {
                        program: program.clone(),
                        arg: arg
                            .replace("{selection_urlencoded}", &percent_encode(selection))
                            .replace("{selection}", selection),
                    }
                }
                _ => plan,
            };
            out.push(ActionProposal {
                action: action.id(),
                label: if rule.name.is_empty() {
                    modular_clipboard_core::i18n::t(action.label()).to_string()
                } else {
                    rule.name.clone()
                },
                plan,
                rule_id: Some(rule.id),
            });
        }
    }

    // 2. 内置推断，补充未被规则覆盖的动作。
    for action in registry.applicable(item) {
        let already = out.iter().any(|p| p.action == action.id());
        if already {
            continue;
        }
        if let Some(plan) = action.plan(item) {
            out.push(ActionProposal {
                action: action.id(),
                label: modular_clipboard_core::i18n::t(action.label()).to_string(),
                plan,
                rule_id: None,
            });
        }
    }

    out
}

/// 执行计划。会真实产生副作用，故与 [`propose`] 分离。
pub fn dispatch(plan: &ActionPlan) -> anyhow::Result<()> {
    match plan {
        ActionPlan::OpenExternal(p) => modular_clipboard_platform::open_external(p),
        ActionPlan::OpenUrl(u) => modular_clipboard_platform::open_url(u),
        ActionPlan::RevealInExplorer(p) => modular_clipboard_platform::reveal_in_explorer(p),
        ActionPlan::RunCommand { program, arg } => {
            // `arg` 在 plan 构造时已完成 `{selection}` 替换；此处传空
            // selection，run_command 内的替换是无副作用的幂等操作。
            modular_clipboard_platform::run_command(program, arg, "")
        }
        ActionPlan::CopyToClipboard => Ok(()),
        ActionPlan::Notice(_) => Ok(()),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn text_item(text: &str) -> ClipItem {
        ClipItem::new(ClipKind::Text, "h".into(), text.into(), "app".into())
    }

    fn files_item(files: &[&str]) -> ClipItem {
        ClipItem::new(
            ClipKind::Files,
            "h".into(),
            files.join("\n"),
            "app".into(),
        )
    }

    #[test]
    fn url_gets_open_url_action() {
        let reg = ActionRegistry::with_builtins();
        let props = propose(&reg, &text_item("https://example.com"), &[]);
        assert!(props.iter().any(|p| p.action == "open_url"));
    }

    #[test]
    fn plain_text_gets_no_open_action() {
        let reg = ActionRegistry::with_builtins();
        let props = propose(&reg, &text_item("just some words"), &[]);
        assert!(!props.iter().any(|p| p.action == "open_url"));
        assert!(!props.iter().any(|p| p.action == "open_external"));
    }

    #[test]
    fn path_text_gets_open_and_reveal() {
        let reg = ActionRegistry::with_builtins();
        let props = propose(&reg, &text_item(r"C:\Users\test\a.txt"), &[]);
        assert!(props.iter().any(|p| p.action == "open_external"));
        assert!(props.iter().any(|p| p.action == "reveal"));
        assert!(!props.iter().any(|p| p.action == "open_url"));
    }

    #[test]
    fn multi_line_text_is_not_treated_as_path() {
        // 多行内容不能被误判为路径
        let item = text_item("C:\\a.txt\nsome other line");
        assert_eq!(extract_path(&item), None);
    }

    #[test]
    fn files_item_reveals_first_path() {
        let reg = ActionRegistry::with_builtins();
        let props = propose(&reg, &files_item(&[r"D:\x.txt", r"D:\y.txt"]), &[]);
        let reveal = props.iter().find(|p| p.action == "reveal").unwrap();
        assert_eq!(reveal.plan, ActionPlan::RevealInExplorer(r"D:\x.txt".into()));
    }

    #[test]
    fn keyword_rule_takes_precedence_and_overrides_builtin() {
        let reg = ActionRegistry::with_builtins();
        let rules = vec![ActionRule {
            id: 1,
            name: "我的搜索".into(),
            keyword: Some("example".into()),
            kind: Some(ClipKind::Text),
            action: "run_command".into(),
            arg: None,
            sort_order: 0,
        }];
        let props = propose(&reg, &text_item("https://example.com"), &rules);
        // 内置 open_url 因被同 id 的规则结果去重而不应出现两次
        let open_url_count = props.iter().filter(|p| p.action == "open_url").count();
        assert!(open_url_count <= 1);
        assert!(props.iter().any(|p| p.rule_id == Some(1)));
    }

    #[test]
    fn rule_with_mismatched_kind_is_skipped() {
        let reg = ActionRegistry::with_builtins();
        let rules = vec![ActionRule {
            id: 2,
            name: "仅图片".into(),
            keyword: None,
            kind: Some(ClipKind::Image),
            action: "open_external".into(),
            arg: None,
            sort_order: 0,
        }];
        let props = propose(&reg, &text_item(r"C:\a.txt"), &rules);
        assert!(!props.iter().any(|p| p.rule_id == Some(2)));
    }

    #[test]
    fn unknown_action_in_rule_is_ignored_not_panicking() {
        let reg = ActionRegistry::with_builtins();
        let rules = vec![ActionRule {
            id: 3,
            name: "坏规则".into(),
            keyword: None,
            kind: None,
            action: "does_not_exist".into(),
            arg: None,
            sort_order: 0,
        }];
        let props = propose(&reg, &text_item("anything"), &rules);
        assert!(!props.iter().any(|p| p.rule_id == Some(3)));
    }

    #[test]
    fn command_action_substitutes_selection() {
        let action = CommandAction::new("search_test", "Search", "explorer", "https://x/?q={selection}");
        let item = text_item("rust lang");
        let plan = action.plan(&item).unwrap();
        match plan {
            ActionPlan::RunCommand { arg, .. } => {
                assert_eq!(arg, "https://x/?q=rust lang");
            }
            other => panic!("预期 RunCommand，实际 {other:?}"),
        }
    }

    #[test]
    fn command_action_percent_encodes_for_url_templates() {
        // DF-03 / SEC-001 回归：剪贴板文本进入 URL 前必须编码。
        // `&` 不被编码时会截断/改写命令结构。
        let action = CommandAction::new(
            "search_enc",
            "Search",
            "explorer",
            "https://x/?q={selection_urlencoded}",
        );
        let item = text_item("a&b c%d");
        let plan = action.plan(&item).unwrap();
        match plan {
            ActionPlan::RunCommand { arg, .. } => {
                assert_eq!(arg, "https://x/?q=a%26b%20c%25d");
            }
            other => panic!("预期 RunCommand，实际 {other:?}"),
        }
    }

    #[test]
    fn builtin_url_actions_have_unique_ids() {
        // 回归：两个内置 CommandAction 曾共用 "run_command"，第二个
        // 被重复 id 守卫静默拒绝——DeepL 翻译其实从未注册。
        let reg = ActionRegistry::with_builtins();
        assert!(reg.get("run_command").is_some(), "浏览器搜索应注册");
        assert!(reg.get("translate_deepl").is_some(), "DeepL 翻译应注册");
    }

    #[test]
    fn rule_arg_overrides_builtin_template() {
        // 回归：rule.arg 曾经只写不读——用户配的命令行模板永远不生效，
        // 规则跑的一直是内置模板。
        let reg = ActionRegistry::with_builtins();
        let rules = vec![ActionRule {
            id: 7,
            name: "我的搜索".into(),
            keyword: None,
            kind: Some(ClipKind::Text),
            action: "run_command".into(),
            arg: Some("https://example.com/?q={selection_urlencoded}".into()),
            sort_order: 0,
        }];
        let props = propose(&reg, &text_item("hello world"), &rules);
        let mine = props.iter().find(|p| p.rule_id == Some(7)).unwrap();
        match &mine.plan {
            ActionPlan::RunCommand { arg, .. } => {
                assert_eq!(arg, "https://example.com/?q=hello%20world");
            }
            other => panic!("预期 RunCommand，实际 {other:?}"),
        }
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let mut reg = ActionRegistry::default();
        assert!(reg.register(Box::new(OpenUrlAction)).is_ok());
        assert!(reg.register(Box::new(OpenUrlAction)).is_err());
    }

    #[test]
    fn keyword_match_is_case_insensitive() {
        let reg = ActionRegistry::with_builtins();
        let rules = vec![ActionRule {
            id: 4,
            name: "大写匹配".into(),
            keyword: Some("EXAMPLE".into()),
            kind: None,
            action: "run_command".into(),
            arg: None,
            sort_order: 0,
        }];
        let props = propose(&reg, &text_item("see example here"), &rules);
        assert!(props.iter().any(|p| p.rule_id == Some(4)));
    }

    #[test]
    fn rules_are_applied_in_sort_order() {
        let reg = ActionRegistry::with_builtins();
        let mk = |id: i64, order: i64| ActionRule {
            id,
            name: format!("规则{id}"),
            keyword: None,
            kind: None,
            action: "run_command".into(),
            arg: Some(format!("cmd{id}")),
            sort_order: order,
        };
        let rules = vec![mk(1, 10), mk(2, 1)];
        let props = propose(&reg, &text_item("x"), &rules);
        let rule_ids: Vec<i64> = props.iter().filter_map(|p| p.rule_id).collect();
        // sort_order 小的在前
        assert_eq!(rule_ids, vec![2, 1]);
    }

    #[allow(dead_code)]
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    #[test]
    fn registry_is_thread_safe() {
        let reg = Arc::new(ActionRegistry::with_builtins());
        assert_send_sync(&reg);
    }
}