//! The slash-command palette: what typing `/` in the composer can offer.
//!
//! Every option here is derived from state — which models the agent reported,
//! which thinking levels it offers, which user messages a fork could start from —
//! so this is a pure query over `AppState`, not a widget.
//!
//! It lived in the egui crate, coupled to it by exactly one thing: each option
//! named an `egui::icons::Icon`. That one type was enough to keep 500 lines of
//! translation out of reach of a second view, so the icon becomes a purpose
//! ([`CommandIcon`]) that each view maps to its own glyph.

use pi_whim_core::{AppState, ModelOption, SessionStatus, ThinkingLevel};

/// What an option is *for*, so each view can pick its own glyph.
///
/// Deliberately smaller than either view's icon set: this names the handful of
/// meanings the palette needs, and nothing about how they are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandIcon {
    /// A model or provider choice.
    Model,
    /// Reasoning effort.
    Thinking,
    /// Anything that duplicates or exports.
    Copy,
    /// A message, session, or anything conversational.
    Message,
    /// Context compaction.
    Compact,
    /// An attachment from disk.
    File,
    /// Preferences and shortcuts.
    Settings,
    /// Interrupting the turn in flight.
    Stop,
}

/// One thing the palette can do.
///
/// `PartialEq` is here so a host's request queue stays comparable in tests —
/// asserting that picking a row queued the right command beats reading it back
/// out through a match.
#[derive(Clone, Debug, PartialEq)]
pub enum SlashCommand {
    NewSession,
    AddAttachment,
    ChooseModel,
    ChooseThinkingLevel,
    Compact,
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
    CopyLastMessage,
    NameSession(Option<String>),
    ShowSessionInfo,
    ShowHotkeys,
    ShowChangelog,
    ChooseFork,
    Fork(String),
    Clone,
    Export(Option<String>),
    Share,
    SubmitDynamic(String),
    Stop,
}

#[derive(Clone, Debug)]
pub struct SlashCommandOption {
    pub command: SlashCommand,
    pub trigger: String,
    pub title: String,
    pub detail: String,
    pub icon: CommandIcon,
    keywords: Vec<String>,
}

impl SlashCommandOption {
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.trigger.to_lowercase().contains(&query)
            || self.title.to_lowercase().contains(&query)
            || self.detail.to_lowercase().contains(&query)
            || self
                .keywords
                .iter()
                .any(|keyword| keyword.to_lowercase().contains(&query))
    }
}

/// Returns the commands relevant to a composer that begins with `/`.
pub fn options(state: &AppState, composer: &str) -> Option<Vec<SlashCommandOption>> {
    let query = composer.strip_prefix('/')?;
    if query.contains(['\n', '\r']) {
        return None;
    }
    let query = query.trim_start();
    let chinese = state.language == pi_whim_core::Language::SimplifiedChinese;

    if let Some(model_query) = command_argument_any(query, &["model", "模型"]) {
        return Some(model_options(state, model_query, chinese));
    }
    if let Some(level_query) = command_argument_any(query, &["thinking", "思考"]) {
        return Some(thinking_options(state, level_query, chinese));
    }
    if let Some(name) = command_argument(query, "name") {
        return Some(vec![direct_option(
            SlashCommand::NameSession((!name.is_empty()).then(|| name.to_owned())),
            "/name",
            text(chinese, "设置会话名称", "Set session name"),
            name,
            CommandIcon::Message,
        )]);
    }
    if let Some(path) = command_argument(query, "export") {
        return Some(vec![direct_option(
            SlashCommand::Export((!path.is_empty()).then(|| path.to_owned())),
            "/export",
            text(chinese, "导出会话", "Export session"),
            path,
            CommandIcon::Copy,
        )]);
    }
    if let Some(fork_query) = command_argument(query, "fork") {
        return Some(fork_options(state, fork_query, chinese));
    }
    if state.available_commands.iter().any(|command| {
        query
            .strip_prefix(&command.name)
            .is_some_and(|argument| argument.starts_with(char::is_whitespace))
    }) {
        return None;
    }

    Some(
        primary_options(state, chinese)
            .into_iter()
            .filter(|option| option.matches(query))
            .collect(),
    )
}

fn command_argument<'a>(query: &'a str, command: &str) -> Option<&'a str> {
    query
        .strip_prefix(command)
        .filter(|argument| argument.starts_with(char::is_whitespace))
        .map(str::trim)
}

fn command_argument_any<'a>(query: &'a str, commands: &[&str]) -> Option<&'a str> {
    commands
        .iter()
        .find_map(|command| command_argument(query, command))
}

fn primary_options(state: &AppState, chinese: bool) -> Vec<SlashCommandOption> {
    let mut options = vec![
        builtin(
            SlashCommand::ChooseModel,
            "model",
            "选择模型",
            "Select model",
            "选择供应商和模型",
            "Choose a provider and model",
            CommandIcon::Model,
            chinese,
        ),
        builtin(
            SlashCommand::Export(None),
            "export",
            "导出会话",
            "Export session",
            "导出为 HTML；也可输入目标路径",
            "Export as HTML or provide a path",
            CommandIcon::Copy,
            chinese,
        ),
        builtin(
            SlashCommand::Share,
            "share",
            "分享会话",
            "Share session",
            "创建私密 GitHub Gist 分享链接",
            "Create a secret GitHub Gist share link",
            CommandIcon::Copy,
            chinese,
        ),
        builtin(
            SlashCommand::CopyLastMessage,
            "copy",
            "复制最后回复",
            "Copy last reply",
            "复制最后一条 agent 汇报",
            "Copy the last agent message",
            CommandIcon::Copy,
            chinese,
        ),
        builtin(
            SlashCommand::NameSession(None),
            "name",
            "命名会话",
            "Name session",
            "设置当前会话名称",
            "Set the current session name",
            CommandIcon::Message,
            chinese,
        ),
        builtin(
            SlashCommand::ShowSessionInfo,
            "session",
            "会话信息",
            "Session info",
            "显示当前会话统计",
            "Show current session statistics",
            CommandIcon::Message,
            chinese,
        ),
        builtin(
            SlashCommand::ShowChangelog,
            "changelog",
            "更新日志",
            "Changelog",
            "查看 Pi 更新日志",
            "View Pi changelog entries",
            CommandIcon::Message,
            chinese,
        ),
        builtin(
            SlashCommand::ShowHotkeys,
            "hotkeys",
            "快捷键",
            "Hotkeys",
            "显示键盘快捷键",
            "Show keyboard shortcuts",
            CommandIcon::Settings,
            chinese,
        ),
        builtin(
            SlashCommand::ChooseFork,
            "fork",
            "分叉会话",
            "Fork session",
            "从一条用户消息创建分支",
            "Fork from a previous user message",
            CommandIcon::Message,
            chinese,
        ),
        builtin(
            SlashCommand::Clone,
            "clone",
            "克隆会话",
            "Clone session",
            "复制当前会话位置",
            "Duplicate the current session position",
            CommandIcon::Copy,
            chinese,
        ),
        builtin(
            SlashCommand::NewSession,
            "new",
            "新建会话",
            "New session",
            "在当前项目开始新会话",
            "Start a fresh session in this project",
            CommandIcon::Message,
            chinese,
        ),
        builtin(
            SlashCommand::Compact,
            "compact",
            "压缩上下文",
            "Compact context",
            "立即压缩当前会话历史",
            "Compact the current session context",
            CommandIcon::Compact,
            chinese,
        ),
        builtin(
            SlashCommand::ChooseThinkingLevel,
            "thinking",
            "思考深度",
            "Thinking level",
            &format!(
                "{}: {}",
                text(chinese, "当前", "Current"),
                state.thinking_level.as_str()
            ),
            &format!("Current: {}", state.thinking_level.as_str()),
            CommandIcon::Thinking,
            chinese,
        ),
        builtin(
            SlashCommand::AddAttachment,
            "image",
            "添加附件",
            "Add attachment",
            "从本机选择文件或文件夹作为附件",
            "Attach files or folders from this Mac",
            CommandIcon::File,
            chinese,
        ),
    ];

    for command in &state.available_commands {
        options.push(option(
            SlashCommand::SubmitDynamic(command.name.clone()),
            format!("/{}", command.name),
            command.name.clone(),
            if command.description.is_empty() {
                command.source.clone()
            } else {
                format!("{} · {}", command.source, command.description)
            },
            CommandIcon::Message,
            &[&command.source],
        ));
    }

    if matches!(
        state.session_status,
        SessionStatus::Streaming | SessionStatus::Compacting
    ) {
        options.push(builtin(
            SlashCommand::Stop,
            "stop",
            "停止",
            "Stop",
            "停止当前响应",
            "Stop the current response",
            CommandIcon::Stop,
            chinese,
        ));
    }
    options
}

#[allow(clippy::too_many_arguments)]
fn builtin(
    command: SlashCommand,
    trigger: &str,
    chinese_title: &str,
    english_title: &str,
    chinese_detail: &str,
    english_detail: &str,
    icon: CommandIcon,
    chinese: bool,
) -> SlashCommandOption {
    option(
        command,
        format!("/{trigger}"),
        text(chinese, chinese_title, english_title),
        text(chinese, chinese_detail, english_detail),
        icon,
        &[trigger, chinese_title, english_title],
    )
}

fn model_options(state: &AppState, query: &str, chinese: bool) -> Vec<SlashCommandOption> {
    let query = query.to_lowercase();
    state
        .available_models
        .iter()
        .filter(|model| {
            query.is_empty()
                || model.name.to_lowercase().contains(&query)
                || model.id.to_lowercase().contains(&query)
                || model.provider_name.to_lowercase().contains(&query)
        })
        .map(|model| {
            option(
                SlashCommand::SetModel(model.clone()),
                "/model",
                model.name.clone(),
                if model.name == model.id {
                    model.provider_name.clone()
                } else {
                    format!("{} · {}", model.provider_name, model.id)
                },
                CommandIcon::Model,
                &["model", "模型", if chinese { "选择" } else { "select" }],
            )
        })
        .collect()
}

fn thinking_options(state: &AppState, query: &str, chinese: bool) -> Vec<SlashCommandOption> {
    let query = query.to_lowercase();
    state
        .available_thinking_levels
        .iter()
        .copied()
        .filter(|level| query.is_empty() || level.as_str().contains(&query))
        .map(|level| {
            option(
                SlashCommand::SetThinkingLevel(level),
                "/thinking",
                level.as_str(),
                if level == state.thinking_level {
                    text(chinese, "当前思考深度", "Current thinking level")
                } else {
                    text(chinese, "切换思考深度", "Use this thinking level")
                },
                CommandIcon::Thinking,
                &["thinking", "reasoning", "思考", "深度"],
            )
        })
        .collect()
}

fn fork_options(state: &AppState, query: &str, chinese: bool) -> Vec<SlashCommandOption> {
    let query = query.to_lowercase();
    state
        .conversation
        .iter()
        .filter(|message| message.role == pi_whim_core::ConversationRole::User)
        .filter(|message| query.is_empty() || message.full_text.to_lowercase().contains(&query))
        .map(|message| {
            let preview = message.full_text.chars().take(80).collect::<String>();
            option(
                SlashCommand::Fork(message.id.clone()),
                "/fork",
                preview,
                text(chinese, "从这条用户消息分叉", "Fork from this user message"),
                CommandIcon::Message,
                &["fork", "分叉"],
            )
        })
        .collect()
}

fn direct_option(
    command: SlashCommand,
    trigger: &str,
    title: String,
    detail: &str,
    icon: CommandIcon,
) -> SlashCommandOption {
    option(
        command,
        trigger,
        title,
        if detail.is_empty() { trigger } else { detail },
        icon,
        &[trigger],
    )
}

fn option(
    command: SlashCommand,
    trigger: impl Into<String>,
    title: impl Into<String>,
    detail: impl Into<String>,
    icon: CommandIcon,
    keywords: &[&str],
) -> SlashCommandOption {
    SlashCommandOption {
        command,
        trigger: trigger.into(),
        title: title.into(),
        detail: detail.into(),
        icon,
        keywords: keywords
            .iter()
            .map(|keyword| (*keyword).to_owned())
            .collect(),
    }
}

fn text(chinese: bool, chinese_text: &str, english_text: &str) -> String {
    if chinese {
        chinese_text.into()
    } else {
        english_text.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_contains_session_commands_but_filters_gui_owned_commands() {
        let state = AppState::default();
        let options = options(&state, "/").unwrap();
        let triggers = options
            .iter()
            .map(|option| option.trigger.as_str())
            .collect::<Vec<_>>();
        for command in [
            "model",
            "export",
            "share",
            "copy",
            "name",
            "session",
            "changelog",
            "hotkeys",
            "fork",
            "clone",
            "new",
            "compact",
            "thinking",
            "image",
        ] {
            assert!(
                triggers.contains(&format!("/{command}").as_str()),
                "missing /{command}"
            );
        }
        for command in [
            "settings",
            "scoped-models",
            "import",
            "tree",
            "trust",
            "login",
            "logout",
            "resume",
            "reload",
            "quit",
        ] {
            assert!(
                !triggers.contains(&format!("/{command}").as_str()),
                "GUI-owned /{command} leaked into menu"
            );
        }
    }

    #[test]
    fn model_results_require_an_argument_after_the_model_command() {
        let state = AppState::default();
        assert!(matches!(
            options(&state, "/model").unwrap()[0].command,
            SlashCommand::ChooseModel
        ));
        let state = AppState {
            available_models: vec![ModelOption {
                provider: "test".into(),
                provider_name: "Test".into(),
                id: "gpt-example".into(),
                name: "GPT Example".into(),
            }],
            ..AppState::default()
        };
        assert!(matches!(
            options(&state, "/model gpt").unwrap()[0].command,
            SlashCommand::SetModel(_)
        ));
    }

    #[test]
    fn dynamic_runtime_commands_are_searchable() {
        let state = AppState {
            available_commands: vec![pi_whim_core::SlashCommandInfo {
                name: "skill:review".into(),
                description: "Review code".into(),
                source: "skill".into(),
            }],
            ..AppState::default()
        };
        let options = options(&state, "/review").unwrap();
        assert!(
            matches!(&options[0].command, SlashCommand::SubmitDynamic(name) if name == "skill:review")
        );
    }
}
