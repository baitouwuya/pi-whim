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
    (
        "empty-projects",
        "Add a project folder to begin.",
        "添加一个项目文件夹以开始。",
    ),
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
        "composer-placeholder",
        "Tell Pi what you want to do...",
        "告诉 Pi 你想完成什么...",
    ),
    // One picker takes both, so the button says so rather than naming a kind.
    (
        "add-attachment",
        "Attach files or folders",
        "附加文件或文件夹",
    ),
    // Shown in place of the model picker when there is nothing to pick.
    ("no-models-available", "No models available", "没有可用模型"),
    ("stop-turn", "Stop the turn in flight", "停止当前回合"),
    ("model", "Model", "模型"),
    // The thinking picker shows "Thinking: <level>", so the prefix carries the
    // separator: Chinese does not put a space before the colon.
    ("thinking-prefix", "Thinking: ", "思考："),
    ("thinking-off", "off", "关闭"),
    ("session-title", "Session title", "会话名称"),
    ("title-field", "TITLE", "名称"),
    // The status pill's own vocabulary, lowercase because it is set in the mono
    // voice pi.dev uses for machine state.
    ("status-offline", "offline", "离线"),
    ("status-starting", "starting", "启动中"),
    ("status-ready", "ready", "就绪"),
    ("status-streaming", "streaming", "生成中"),
    ("status-compacting", "compacting", "压缩中"),
    ("status-failed", "failed", "失败"),
    ("settings", "Settings", "设置"),
    // The theme toggle names where it goes, not where it is.
    ("switch-to-dark", "Switch to dark", "切换到深色"),
    ("switch-to-light", "Switch to light", "切换到浅色"),
    ("general", "General", "通用"),
    ("providers", "Providers", "模型提供商"),
    ("web-search", "Web Search", "网页搜索"),
    (
        "web-search-help",
        "Configure web search engines in fallback order.",
        "配置按顺序尝试的网页搜索引擎。",
    ),
    ("search-engines", "Search engines", "搜索引擎"),
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
    // The permission level, shown beside the prompt as well as in settings: it is
    // what the agent may reach without asking, so it belongs where a turn is sent.
    ("permission-level", "Permission", "权限"),
    ("permission-read-only", "Read-only", "只读"),
    ("permission-controlled", "Approval", "审批"),
    ("permission-full", "Full access", "完全访问"),
    (
        "provider-help",
        "Keys are stored securely in macOS Keychain.",
        "密钥安全存储在 macOS Keychain。",
    ),
    // Dialogs the agent raises. The title and message come from the agent when it
    // sends them; these stand in when it does not, and the buttons are always the
    // app's own words.
    ("confirm-title", "Pi confirmation", "Pi 确认"),
    (
        "confirm-message",
        "Allow this operation?",
        "允许这个操作吗？",
    ),
    ("agent-request", "Agent request", "代理请求"),
    ("allow-once", "Allow once", "允许一次"),
    // Messages the host reports through the notification stack. They are written
    // where the failure happens, which is outside any view, so they are looked up
    // against the stored language rather than a view's own copy of it.
    (
        "notice-select-project-attachments",
        "Select a project before adding attachments.",
        "请先选择一个项目，然后再添加附件。",
    ),
    (
        "notice-select-project-send",
        "Select a project before sending a message.",
        "请先选择一个项目，然后再发送消息。",
    ),
    (
        "notice-not-ready",
        "Pi is not ready for the selected project yet.",
        "所选项目的 Pi 还没有就绪。",
    ),
    ("notice-no-session", "No active session.", "没有活动会话。"),
    (
        "notice-session-gone",
        "The session is no longer running.",
        "该会话已不在运行。",
    ),
    (
        "notice-asking-session-gone",
        "The session that asked is no longer running.",
        "发起询问的会话已不在运行。",
    ),
    (
        "notice-no-session-to-name",
        "No active session to name.",
        "没有可命名的活动会话。",
    ),
    (
        "notice-key-unreadable",
        "The API key could not be read back from Keychain. Pi was not restarted; try Save and apply again.",
        "无法从 Keychain 读回 API Key。Pi 未重启；请再次尝试保存并应用。",
    ),
    (
        "notice-key-missing",
        "This provider has no API key in Keychain. Enter and save its API key before starting Pi.",
        "该提供商在 Keychain 中没有 API Key。请先输入并保存，然后再启动 Pi。",
    ),
    (
        "notice-search-engine-untestable",
        "Enter a name and valid HTTP or HTTPS base URL before testing.",
        "请先填写名称和有效的 HTTP 或 HTTPS 基础 URL，然后再测试。",
    ),
    (
        "notice-search-engine-ok",
        "is reachable and returned valid SearXNG JSON.",
        "可访问，并返回了有效的 SearXNG JSON。",
    ),
    ("notice-test-failed", "test failed", "测试失败"),
    (
        "notice-no-models-discovered",
        "The provider returned no models; add a model ID manually.",
        "该提供商没有返回任何模型；请手动添加模型 ID。",
    ),
    (
        "notice-session-exported",
        "Session exported to",
        "会话已导出到",
    ),
    (
        "notice-export-failed",
        "Could not export the session for sharing.",
        "无法导出会话以供分享。",
    ),
    ("notice-share-url", "Share URL:", "分享链接："),
    (
        "notice-gh-unavailable",
        "GitHub CLI unavailable:",
        "GitHub CLI 不可用：",
    ),
    (
        "notice-trash-failed",
        "Could not move the Pi session to Trash.",
        "无法把 Pi 会话移到废纸篓。",
    ),
    (
        "notice-name-usage",
        "Usage: /name <name>",
        "用法：/name <名称>",
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
    ("allow", "Allow", "允许"),
    ("deny", "Deny", "拒绝"),
    ("blocked-patterns", "Blocked patterns", "阻止模式"),
    (
        "blocked-patterns-help",
        "One Bash command blocking pattern per line.",
        "每行一个 Bash 命令阻止模式。",
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
    ("models", "Models", "模型"),
    (
        "models-help",
        "Discover or manually add available models.",
        "发现或手动添加可用模型。",
    ),
    ("compacting-banner", "Compacting context", "正在压缩上下文"),
    (
        "compacting-detail",
        "Pi is condensing earlier messages.",
        "Pi 正在整理早期消息。",
    ),
    ("copy-session-id", "Copy session ID", "复制会话 ID"),
    ("hint-enter", "Enter to send", "Enter 发送"),
    (
        "hint-shift-enter",
        "Shift+Enter for a new line",
        "Shift+Enter 换行",
    ),
    ("search-models", "Search models", "搜索模型"),
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
