//! The interface's strings, in both languages.
//!
//! One table rather than a 700-line `match` with a nested `if zh` per arm, which
//! is what the egui crate had. That shape made a missing translation invisible
//! and put the strings out of reach of the gpui views, which need the same ones.
//!
//! Keys are stable identifiers, not English text: an English string that changes
//! should not silently orphan its translation.

use crate::{AppState, Language};

/// Every string: key, English, Chinese.
///
/// Sorted by where it appears rather than alphabetically, so a section's strings
/// are edited together.
const STRINGS: &[(&str, &str, &str)] = &[
    ("projects", "Projects", "项目"),
    ("add-project", "Add local project", "添加本地项目"),
    ("search", "Search projects", "搜索项目"),
    ("show-finder", "Show in Finder", "在 Finder 中显示"),
    ("remove", "Remove", "移除"),
    ("rename", "Rename", "重命名"),
    ("rename-session", "Rename session", "重命名会话"),
    ("clone", "Clone session", "克隆会话"),
    ("delete", "Move to trash", "移至废纸篓"),
    ("save", "Save", "保存"),
    ("new-session", "New session", "新建会话"),
    (
        "empty-heading",
        "What should we make happen?",
        "我们应该在这里做些什么？",
    ),
    (
        "empty-detail",
        "Select a project, then tell Pi what you want to do.",
        "选择一个项目，然后告诉 Pi 你想完成什么。",
    ),
    (
        "select-project-to-chat",
        "Add and select a project from the sidebar before starting a conversation.",
        "先从左侧添加并选择一个项目，才能开始对话。",
    ),
    ("fork-here", "Fork from here", "从这里分叉"),
    (
        "composer-placeholder",
        "Tell Pi what you want to do...",
        "告诉 Pi 你想完成什么...",
    ),
    ("add-attachment", "Add attachment", "添加附件"),
    ("choose-files", "Choose files...", "选择文件..."),
    ("choose-folder", "Choose folder...", "选择文件夹..."),
    ("queued", "QUEUED", "已排队"),
    ("follow-ups", "FOLLOW-UPS", "后续队列"),
    ("thinking", "Thinking", "思考"),
    (
        "models-unavailable",
        "No models are available. Save a provider in Settings.",
        "没有可用模型。请在设置中保存一个模型提供商。",
    ),
    ("stop", "Stop", "停止"),
    ("settings", "Settings", "设置"),
    ("general", "General", "通用"),
    ("providers", "Providers", "模型提供商"),
    ("web-search", "Web Search", "网页搜索"),
    (
        "web-search-help",
        "Configure web search engines in fallback order.",
        "配置按顺序尝试的网页搜索引擎。",
    ),
    ("search-engines", "Search engines", "搜索引擎"),
    ("add-search-engine", "Add search engine", "添加搜索引擎"),
    (
        "search-engine-details",
        "Search engine details",
        "搜索引擎详情",
    ),
    (
        "searxng-url-help",
        "Root URL of the SearXNG instance.",
        "SearXNG 实例根 URL。",
    ),
    ("enabled", "Enabled", "启用"),
    ("save-search-engine", "Save search engine", "保存搜索引擎"),
    ("test-search-engine", "Test connection", "测试连接"),
    (
        "providers-help",
        "Configure model providers used by Pi.",
        "配置 Pi 使用的模型提供商。",
    ),
    ("provider-name", "Name", "名称"),
    ("preset", "Preset", "预设"),
    ("protocol", "Request protocol", "请求协议"),
    (
        "key-stored",
        "Stored in Keychain; leave blank to keep it",
        "已保存在 Keychain；留空可保持不变",
    ),
    ("key-required", "Enter API key", "输入 API Key"),
    ("discover-models", "Discover models", "发现模型"),
    ("add-model", "Add model", "添加模型"),
    ("model-id", "Manual model ID", "手动输入模型 ID"),
    ("no-models", "No models selected.", "尚未选择模型。"),
    ("save-provider", "Save provider", "保存提供商"),
    ("save-and-apply", "Save and apply", "保存并应用"),
    ("delete-provider", "Delete provider", "删除提供商"),
    ("language", "Language", "语言"),
    ("api-key", "API key", "API Key"),
    ("base-url", "Base URL", "基础 URL"),
    (
        "duplicate-provider-name",
        "Another provider already uses this name.",
        "已有另一个提供商在使用这个名称。",
    ),
    (
        "provider-incomplete",
        "A provider needs a name, a base URL, and at least one model.",
        "提供商需要名称、基础 URL 和至少一个模型。",
    ),
    (
        "search-engine-incomplete",
        "A search engine needs a name and a base URL.",
        "搜索引擎需要名称和基础 URL。",
    ),
    (
        "no-providers",
        "No providers configured.",
        "尚未配置提供商。",
    ),
    (
        "no-search-engines",
        "No search engines configured.",
        "尚未配置搜索引擎。",
    ),
    (
        "preset-help",
        "Fills in the base URL and protocol for a known provider.",
        "为已知提供商填入基础 URL 和协议。",
    ),
    ("shell", "Shell", "Shell"),
    ("bash-policy", "Bash commands", "Bash 命令"),
    ("bash-ask", "Ask", "询问"),
    ("bash-allow", "Allow", "允许"),
    ("bash-deny", "Deny", "拒绝"),
    (
        "bash-help",
        "Control how the Bash tool executes.",
        "控制 Bash 工具的执行方式。",
    ),
    ("queue-mode", "Queue mode", "队列模式"),
    (
        "provider-help",
        "Keys are stored securely in macOS Keychain.",
        "密钥安全存储在 macOS Keychain。",
    ),
    ("back", "Back", "返回"),
    ("appearance", "Appearance", "外观"),
    ("context", "Context", "上下文"),
    (
        "context-help",
        "Control conversation context management.",
        "控制会话上下文管理。",
    ),
    ("auto-compaction", "Automatic compaction", "自动压缩上下文"),
    (
        "auto-compaction-help",
        "Automatically compact when context approaches its limit.",
        "在上下文接近上限时自动压缩。",
    ),
    ("command-policy", "Execution policy", "执行策略"),
    ("allow", "Allow", "允许"),
    ("ask", "Ask", "询问"),
    ("deny", "Deny", "拒绝"),
    ("blocked-patterns", "Blocked patterns", "阻止模式"),
    (
        "blocked-patterns-help",
        "One Bash command blocking pattern per line.",
        "每行一个 Bash 命令阻止模式。",
    ),
    (
        "apply-command-filters",
        "Apply command filters",
        "应用命令过滤器",
    ),
    ("agent-team", "Agent team", "代理团队"),
    (
        "agent-team-help",
        "Limit delegated agent depth and count.",
        "限制委派代理的层级与数量。",
    ),
    ("max-agent-depth", "Maximum agent depth", "最大代理层级"),
    (
        "max-agents-per-level",
        "Maximum agents per level",
        "每层最大代理数",
    ),
    ("one-at-a-time", "One at a time", "逐个"),
    ("all", "All", "全部"),
    (
        "configured-providers",
        "Configured providers",
        "已配置的提供商",
    ),
    ("add-provider", "Add provider", "添加提供商"),
    ("connection", "Connection", "连接"),
    (
        "connection-help",
        "Fill in connection details before saving.",
        "保存前填写连接详情。",
    ),
    (
        "provider-name-duplicate",
        "This name is already in use.",
        "名称已被使用。",
    ),
    ("models", "Models", "模型"),
    (
        "models-help",
        "Discover or manually add available models.",
        "发现或手动添加可用模型。",
    ),
    ("show-error", "Show error", "显示错误"),
    ("error-banner-title", "Request failed", "请求失败"),
    ("dismiss", "Dismiss", "关闭"),
    ("copy-error", "Copy error", "复制错误"),
    ("compacting-banner", "Compacting context", "正在压缩上下文"),
    (
        "compacting-detail",
        "Pi is condensing earlier messages.",
        "Pi 正在整理早期消息。",
    ),
    ("auto-compact-on", "AUTO-COMPACT: ON", "自动压缩：开"),
    ("auto-compact-off", "AUTO-COMPACT: OFF", "自动压缩：关"),
    ("copy-session-id", "Copy session ID", "复制会话 ID"),
    ("hint-slash", "/ for quick actions", "/ 查看快捷操作"),
    ("hint-enter", "Enter to send", "Enter 发送"),
    (
        "hint-shift-enter",
        "Shift+Enter for a new line",
        "Shift+Enter 换行",
    ),
    ("search-models", "Search models", "搜索模型"),
    ("copy-report", "Copy reply", "复制回复"),
    ("raw-tool-details", "Raw tool details", "原始工具详情"),
    ("show-all", "Show all", "显示完整内容"),
    ("generating", "Generating", "正在生成"),
    ("send", "Send", "发送"),
    ("slash-commands", "Quick actions", "快捷操作"),
    (
        "slash-help",
        "Use arrows to select; Enter or Tab to confirm.",
        "上下方向键选择，Enter 或 Tab 确认。",
    ),
    // These two are proper nouns of the UI in both languages.
    ("steer-mode", "Steer", "Steer"),
    ("follow-up-mode", "Follow-up", "Follow-up"),
];

/// Look `key` up in `language`.
///
/// An unknown key returns `"?"` rather than an empty string. Empty was the egui
/// build's fallback, which turned a typo into an invisible label; a visible mark
/// says something is missing. Use [`lookup`] to detect the miss instead of
/// showing it.
pub fn text(key: &str, language: Language) -> &'static str {
    lookup(key, language).unwrap_or("?")
}

/// Look `key` up, reporting whether it exists.
pub fn lookup(key: &str, language: Language) -> Option<&'static str> {
    STRINGS
        .iter()
        .find(|(candidate, _, _)| *candidate == key)
        .map(|&(_, english, chinese)| match language {
            Language::English => english,
            Language::SimplifiedChinese => chinese,
        })
}

/// Look `key` up in the language `state` is set to.
pub fn tr(state: &AppState, key: &str) -> &'static str {
    text(key, state.language)
}

/// Every key in the table, for callers that want to check coverage.
pub fn keys() -> impl Iterator<Item = &'static str> {
    STRINGS.iter().map(|&(key, _, _)| key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_key_appears_once() {
        // A duplicate key means one of the two is dead and whichever comes second
        // silently never renders.
        let mut seen = HashSet::new();
        for key in keys() {
            assert!(seen.insert(key), "duplicate key: {key}");
        }
    }

    #[test]
    fn every_key_has_both_languages() {
        // A blank translation renders as a missing label rather than as an error,
        // so it has to be caught here.
        for &(key, english, chinese) in STRINGS {
            assert!(!english.is_empty(), "{key} has no English");
            assert!(!chinese.is_empty(), "{key} has no Chinese");
        }
    }

    #[test]
    fn keys_are_identifiers_not_english_text() {
        // Keying by English means an editorial change orphans the translation.
        for key in keys() {
            assert!(
                key.chars().all(|character| character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '-'),
                "{key} is not kebab-case"
            );
        }
    }

    #[test]
    fn a_known_key_resolves_in_both_languages() {
        assert_eq!(text("projects", Language::English), "Projects");
        assert_eq!(text("projects", Language::SimplifiedChinese), "项目");
    }

    #[test]
    fn an_unknown_key_is_visible_rather_than_blank() {
        // The egui build returned "", which turned a typo into an invisible label.
        assert_eq!(lookup("no-such-key", Language::English), None);
        assert_eq!(text("no-such-key", Language::English), "?");
    }

    #[test]
    fn most_strings_actually_differ_between_the_languages() {
        // A table where everything matched would mean the Chinese column was
        // never filled in. A handful are proper nouns and legitimately identical.
        let identical = STRINGS
            .iter()
            .filter(|(_, english, chinese)| english == chinese)
            .count();
        assert!(
            identical * 10 < STRINGS.len(),
            "{identical} identical of {}",
            STRINGS.len()
        );
    }
}
