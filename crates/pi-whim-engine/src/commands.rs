//! Framework-independent commands crossing from a shell into application logic.
//!
//! [`AppCommand`] is the only command family intended for the external Hook
//! control pipeline. [`ShellCommand`] names host operations that require a
//! window, clipboard, platform service, credential handling, or provider I/O;
//! those commands must never be routed through application Hooks.

use std::{any::type_name, fmt};

use pi_whim_core::{
    AgentPermissionLevel, AgentTeamConfig, Attachment, BashPolicy, Language, ModelOption,
    OneShotAiConfig, ProjectId, ProviderId, ProviderProfile, ProviderProtocol, QueueMode,
    SearchEngineProfile, SubmitMode, ThinkingLevel,
};
use uuid::Uuid;

use crate::{
    dialogs::{Answer, ExtensionResponse},
    slash_commands::SlashCommand,
};

/// Domain commands owned by application orchestration.
///
/// This type deliberately excludes render/layout/frame work and operations that
/// require a GPUI window, clipboard, platform picker, provider discovery/test,
/// or a credential-bearing save. Only this command family may enter the
/// external Hook Gate/Transform pipeline.
#[derive(Clone, Debug, PartialEq)]
pub enum AppCommand {
    RemoveProject(ProjectId),
    OpenProject(ProjectId),
    NewSession(ProjectId),
    ActivateSession {
        project_id: ProjectId,
        path: String,
    },
    RenameSession {
        path: String,
        title: String,
    },
    CloneSession,
    DeleteSession(String),
    SubmitPrompt {
        content: String,
        attachments: Vec<Attachment>,
        mode: SubmitMode,
    },
    AnswerPrompt(Answer),
    DiscardAttachment(String),
    Stop,
    ClearQueue,
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    SetBlockedPatterns(Vec<String>),
    SetPermissionLevel(AgentPermissionLevel),
    SetAgentTeamConfig(AgentTeamConfig),
    LoadAgentsMdFiles,
    SaveGlobalAgentsMd(String),
    SaveProjectAgentsMd(String),
    ApproveProjectHooks {
        fingerprint: String,
    },
    RevokeProjectHooks,
    SetOneShotAiConfig(OneShotAiConfig),
    SetAutoCompaction(bool),
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
    SetQueueModes {
        steering: QueueMode,
        follow_up: QueueMode,
    },
    RunSlashCommand(SlashCommand),
    DeleteProvider(ProviderId),
    SaveSearchEngines(Vec<SearchEngineProfile>),
}

/// How external Hooks may participate in one application command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandControlPolicy {
    /// Execute without external Hook control. Builtin validation still applies.
    Bypass,
    /// Hooks may observe the outcome but may not block or rewrite the command.
    ObserveOnly,
    /// Hooks may transform authorized fields and then gate the final typed value.
    GateTransform,
}

impl AppCommand {
    /// Stable, framework-independent command identifier used by audit and Hooks.
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::RemoveProject(_) => "project.remove",
            Self::OpenProject(_) => "project.open",
            Self::NewSession(_) => "session.new",
            Self::ActivateSession { .. } => "session.activate",
            Self::RenameSession { .. } => "session.rename",
            Self::CloneSession => "session.clone",
            Self::DeleteSession(_) => "session.delete",
            Self::SubmitPrompt { .. } => "prompt.submit",
            Self::AnswerPrompt(_) => "prompt.answer",
            Self::DiscardAttachment(_) => "attachment.discard",
            Self::Stop => "execution.stop",
            Self::ClearQueue => "queue.clear",
            Self::SetLanguage(_) => "settings.language",
            Self::SetBashPolicy(_) => "settings.bash_policy",
            Self::SetBlockedPatterns(_) => "settings.blocked_patterns",
            Self::SetPermissionLevel(_) => "settings.permission_level",
            Self::SetAgentTeamConfig(_) => "settings.agent_team",
            Self::LoadAgentsMdFiles => "settings.agents_md.load",
            Self::SaveGlobalAgentsMd(_) => "settings.agents_md.global.save",
            Self::SaveProjectAgentsMd(_) => "settings.agents_md.project.save",
            Self::ApproveProjectHooks { .. } => "settings.hooks.approve",
            Self::RevokeProjectHooks => "settings.hooks.revoke",
            Self::SetOneShotAiConfig(_) => "settings.one_shot_ai",
            Self::SetAutoCompaction(_) => "settings.auto_compaction",
            Self::SetModel(_) => "settings.model",
            Self::SetThinkingLevel(_) => "settings.thinking_level",
            Self::SetQueueModes { .. } => "settings.queue_modes",
            Self::RunSlashCommand(_) => "slash.run",
            Self::DeleteProvider(_) => "provider.delete",
            Self::SaveSearchEngines(_) => "search.save_all",
        }
    }

    /// Declares the strongest external Hook control allowed for this command.
    pub fn control_policy(&self) -> CommandControlPolicy {
        if self.is_unconditionally_bypassed() {
            return CommandControlPolicy::Bypass;
        }
        match self {
            Self::SubmitPrompt { .. }
            | Self::SetBashPolicy(_)
            | Self::SetBlockedPatterns(_)
            | Self::SetPermissionLevel(_)
            | Self::SetAgentTeamConfig(_)
            | Self::SaveGlobalAgentsMd(_)
            | Self::SaveProjectAgentsMd(_)
            | Self::SetOneShotAiConfig(_)
            | Self::SetModel(_)
            | Self::SetThinkingLevel(_)
            | Self::RunSlashCommand(_) => CommandControlPolicy::GateTransform,
            Self::OpenProject(_)
            | Self::NewSession(_)
            | Self::ActivateSession { .. }
            | Self::RenameSession { .. }
            | Self::CloneSession
            | Self::SetLanguage(_)
            | Self::LoadAgentsMdFiles
            | Self::SetAutoCompaction(_)
            | Self::SetQueueModes { .. }
            | Self::SaveSearchEngines(_) => CommandControlPolicy::ObserveOnly,
            Self::RemoveProject(_)
            | Self::DeleteSession(_)
            | Self::AnswerPrompt(_)
            | Self::DiscardAttachment(_)
            | Self::Stop
            | Self::ClearQueue
            | Self::ApproveProjectHooks { .. }
            | Self::RevokeProjectHooks
            | Self::DeleteProvider(_) => CommandControlPolicy::Bypass,
        }
    }

    /// Returns whether this command reduces authority, cancels work, denies an
    /// interaction, or removes retained state and therefore must not be blocked.
    pub fn is_safety_command(&self) -> bool {
        match self {
            Self::RemoveProject(_)
            | Self::DeleteSession(_)
            | Self::DiscardAttachment(_)
            | Self::Stop
            | Self::ClearQueue
            | Self::RevokeProjectHooks
            | Self::DeleteProvider(_) => true,
            Self::AnswerPrompt(answer) => answer_is_safety_decision(answer),
            Self::RunSlashCommand(SlashCommand::Stop) => true,
            Self::OpenProject(_)
            | Self::NewSession(_)
            | Self::ActivateSession { .. }
            | Self::RenameSession { .. }
            | Self::CloneSession
            | Self::SubmitPrompt { .. }
            | Self::SetLanguage(_)
            | Self::SetBashPolicy(_)
            | Self::SetBlockedPatterns(_)
            | Self::SetPermissionLevel(_)
            | Self::SetAgentTeamConfig(_)
            | Self::LoadAgentsMdFiles
            | Self::SaveGlobalAgentsMd(_)
            | Self::SaveProjectAgentsMd(_)
            | Self::ApproveProjectHooks { .. }
            | Self::SetOneShotAiConfig(_)
            | Self::SetAutoCompaction(_)
            | Self::SetModel(_)
            | Self::SetThinkingLevel(_)
            | Self::SetQueueModes { .. }
            | Self::RunSlashCommand(_)
            | Self::SaveSearchEngines(_) => false,
        }
    }

    fn is_unconditionally_bypassed(&self) -> bool {
        matches!(
            self,
            Self::RemoveProject(_)
                | Self::DeleteSession(_)
                | Self::AnswerPrompt(_)
                | Self::DiscardAttachment(_)
                | Self::Stop
                | Self::ClearQueue
                | Self::ApproveProjectHooks { .. }
                | Self::RevokeProjectHooks
                | Self::DeleteProvider(_)
                | Self::RunSlashCommand(SlashCommand::Stop)
        )
    }
}

fn answer_is_safety_decision(answer: &Answer) -> bool {
    match answer {
        Answer::Extension { response, .. } => matches!(
            response,
            ExtensionResponse::Confirmed(false) | ExtensionResponse::Cancelled
        ),
        Answer::Interaction { decision, .. } => matches_safety_word(decision),
    }
}

fn matches_safety_word(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "deny" | "denied" | "reject" | "rejected" | "cancel" | "cancelled" | "stop" | "abort"
    )
}

/// Framework-independent host operations excluded from the App Hook pipeline.
#[derive(Clone, PartialEq)]
pub enum ShellCommand {
    AddProject,
    RevealProject(ProjectId),
    SmartRenameSession {
        project_id: ProjectId,
        path: String,
        title: String,
    },
    CopyToClipboard(String),
    AttachPaste(ShellPaste),
    PickAttachments,
    SaveProvider {
        profile: ProviderProfile,
        api_key: Option<String>,
    },
    SaveSearchEngine {
        profile: SearchEngineProfile,
        api_key: Option<String>,
    },
    TestSearchEngine {
        profile: SearchEngineProfile,
        api_key: Option<String>,
        editor: bool,
    },
    DiscoverProviderModels {
        profile_id: Option<ProviderId>,
        provider_name: String,
        base_url: String,
        protocol: ProviderProtocol,
        api_key: Option<String>,
    },
}

impl ShellCommand {
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::AddProject => "shell.project.add",
            Self::RevealProject(_) => "shell.project.reveal",
            Self::SmartRenameSession { .. } => "shell.session.smart_rename",
            Self::CopyToClipboard(_) => "shell.clipboard.copy",
            Self::AttachPaste(_) => "shell.paste.attach",
            Self::PickAttachments => "shell.attachments.pick",
            Self::SaveProvider { .. } => "shell.provider.save",
            Self::SaveSearchEngine { .. } => "shell.search.save",
            Self::TestSearchEngine { .. } => "shell.search.test",
            Self::DiscoverProviderModels { .. } => "shell.provider.discover_models",
        }
    }
}

impl fmt::Debug for ShellCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShellCommand")
            .field("command_name", &self.command_name())
            .finish_non_exhaustive()
    }
}

/// Clipboard content already classified by the shell.
#[derive(Clone, PartialEq, Eq)]
pub enum ShellPaste {
    Files(Vec<String>),
    Image { extension: String, bytes: Vec<u8> },
    LongText(String),
}

impl fmt::Debug for ShellPaste {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Files(_) => "files",
            Self::Image { .. } => "image",
            Self::LongText(_) => "long_text",
        };
        formatter
            .debug_struct("ShellPaste")
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

/// Origin metadata for one command submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandSource {
    Ui,
    System,
    HookReplay,
}

/// Stable command identity, optional routing context, and typed payload.
#[derive(Clone, PartialEq)]
pub struct CommandEnvelope<T> {
    command_id: Uuid,
    source: CommandSource,
    project_id: Option<ProjectId>,
    session_key: Option<String>,
    payload: T,
}

impl<T> CommandEnvelope<T> {
    pub fn new(source: CommandSource, payload: T) -> Self {
        Self {
            command_id: Uuid::new_v4(),
            source,
            project_id: None,
            session_key: None,
            payload,
        }
    }

    pub fn ui(payload: T) -> Self {
        Self::new(CommandSource::Ui, payload)
    }

    pub fn system(payload: T) -> Self {
        Self::new(CommandSource::System, payload)
    }

    pub fn hook_replay(payload: T) -> Self {
        Self::new(CommandSource::HookReplay, payload)
    }

    pub fn with_context(
        mut self,
        project_id: Option<ProjectId>,
        session_key: Option<String>,
    ) -> Self {
        self.project_id = project_id;
        self.session_key = session_key;
        self
    }

    pub const fn command_id(&self) -> Uuid {
        self.command_id
    }

    pub const fn source(&self) -> CommandSource {
        self.source
    }

    pub const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    pub const fn payload(&self) -> &T {
        &self.payload
    }

    pub fn into_payload(self) -> T {
        self.payload
    }

    /// Maps the typed payload while preserving all authenticated envelope metadata.
    ///
    /// The command identifier and routing context remain private and cannot be
    /// supplied or changed by callers.
    pub fn map_payload<U>(self, map: impl FnOnce(T) -> U) -> CommandEnvelope<U> {
        CommandEnvelope {
            command_id: self.command_id,
            source: self.source,
            project_id: self.project_id,
            session_key: self.session_key,
            payload: map(self.payload),
        }
    }
}

impl<T> fmt::Debug for CommandEnvelope<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandEnvelope")
            .field("command_id", &self.command_id)
            .field("source", &self.source)
            .field("project_id", &self.project_id)
            .field("session_key", &self.session_key)
            .field("payload_type", &type_name::<T>())
            .finish()
    }
}

const MAX_DIAGNOSTIC_BYTES: usize = 512;

/// Bounded, caller-sanitized diagnostic carried by a terminal lifecycle stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandDiagnostic(String);

impl CommandDiagnostic {
    pub fn new(value: impl Into<String>) -> Self {
        let mut value = value.into();
        if value.len() > MAX_DIAGNOSTIC_BYTES {
            let mut boundary = MAX_DIAGNOSTIC_BYTES;
            while !value.is_char_boundary(boundary) {
                boundary -= 1;
            }
            value.truncate(boundary);
        }
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed command processing stage. No stage contains the command payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandStage {
    Submitted,
    Transforming,
    Accepted,
    Denied(CommandDiagnostic),
    Executing,
    Completed,
    Failed(CommandDiagnostic),
}

impl CommandStage {
    pub fn denied(diagnostic: impl Into<String>) -> Self {
        Self::Denied(CommandDiagnostic::new(diagnostic))
    }

    pub fn failed(diagnostic: impl Into<String>) -> Self {
        Self::Failed(CommandDiagnostic::new(diagnostic))
    }

    pub const fn stage_name(&self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Transforming => "transforming",
            Self::Accepted => "accepted",
            Self::Denied(_) => "denied",
            Self::Executing => "executing",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
        }
    }
}

/// Metadata-only lifecycle snapshot for one application command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLifecycle {
    command_id: Uuid,
    command_name: &'static str,
    source: CommandSource,
    project_id: Option<ProjectId>,
    session_key: Option<String>,
    stage: CommandStage,
}

impl CommandLifecycle {
    pub fn submitted(envelope: &CommandEnvelope<AppCommand>) -> Self {
        Self {
            command_id: envelope.command_id,
            command_name: envelope.payload.command_name(),
            source: envelope.source,
            project_id: envelope.project_id,
            session_key: envelope.session_key.clone(),
            stage: CommandStage::Submitted,
        }
    }

    pub fn with_stage(mut self, stage: CommandStage) -> Self {
        self.stage = stage;
        self
    }

    pub const fn command_id(&self) -> Uuid {
        self.command_id
    }

    pub const fn command_name(&self) -> &'static str {
        self.command_name
    }

    pub const fn source(&self) -> CommandSource {
        self.source
    }

    pub const fn project_id(&self) -> Option<ProjectId> {
        self.project_id
    }

    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    pub const fn stage(&self) -> &CommandStage {
        &self.stage
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn commands() -> Vec<AppCommand> {
        let project_id = Uuid::nil();
        vec![
            AppCommand::RemoveProject(project_id),
            AppCommand::OpenProject(project_id),
            AppCommand::NewSession(project_id),
            AppCommand::ActivateSession {
                project_id,
                path: "session.jsonl".into(),
            },
            AppCommand::RenameSession {
                path: "session.jsonl".into(),
                title: "title".into(),
            },
            AppCommand::CloneSession,
            AppCommand::DeleteSession("session.jsonl".into()),
            AppCommand::SubmitPrompt {
                content: "prompt".into(),
                attachments: Vec::new(),
                mode: SubmitMode::Prompt,
            },
            AppCommand::AnswerPrompt(Answer::Interaction {
                session_key: "session".into(),
                request_id: "request".into(),
                decision: "approve".into(),
            }),
            AppCommand::DiscardAttachment("generated.txt".into()),
            AppCommand::Stop,
            AppCommand::ClearQueue,
            AppCommand::SetLanguage(Language::English),
            AppCommand::SetBashPolicy(BashPolicy::Ask),
            AppCommand::SetBlockedPatterns(vec!["rm".into()]),
            AppCommand::SetPermissionLevel(AgentPermissionLevel::Controlled),
            AppCommand::SetAgentTeamConfig(AgentTeamConfig::default()),
            AppCommand::LoadAgentsMdFiles,
            AppCommand::SaveGlobalAgentsMd("global".into()),
            AppCommand::SaveProjectAgentsMd("project".into()),
            AppCommand::ApproveProjectHooks {
                fingerprint: "fingerprint".into(),
            },
            AppCommand::RevokeProjectHooks,
            AppCommand::SetOneShotAiConfig(OneShotAiConfig::default()),
            AppCommand::SetAutoCompaction(true),
            AppCommand::SetModel(ModelOption {
                provider: "provider".into(),
                provider_name: "Provider".into(),
                id: "model".into(),
                name: "Model".into(),
            }),
            AppCommand::SetThinkingLevel(ThinkingLevel::default()),
            AppCommand::SetQueueModes {
                steering: QueueMode::All,
                follow_up: QueueMode::OneAtATime,
            },
            AppCommand::RunSlashCommand(SlashCommand::ShowHotkeys),
            AppCommand::DeleteProvider(project_id),
            AppCommand::SaveSearchEngines(Vec::new()),
        ]
    }

    #[test]
    fn every_app_command_has_a_unique_stable_name_and_policy() {
        let commands = commands();
        assert_eq!(commands.len(), 30);
        let names = commands
            .iter()
            .map(AppCommand::command_name)
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), commands.len());
        assert!(names.contains("prompt.submit"));
        assert!(names.contains("execution.stop"));
        assert!(names.contains("settings.hooks.revoke"));
        for command in commands {
            assert!(!command.command_name().is_empty());
            let _ = command.control_policy();
            let _ = command.is_safety_command();
        }
    }

    #[test]
    fn safety_commands_are_never_hook_controlled() {
        let mut safety = commands()
            .into_iter()
            .filter(AppCommand::is_safety_command)
            .collect::<Vec<_>>();
        safety.push(AppCommand::AnswerPrompt(Answer::Interaction {
            session_key: "session".into(),
            request_id: "request".into(),
            decision: "deny".into(),
        }));
        safety.push(AppCommand::AnswerPrompt(Answer::Extension {
            session_key: "session".into(),
            request_id: "request".into(),
            response: ExtensionResponse::Cancelled,
        }));
        safety.push(AppCommand::RunSlashCommand(SlashCommand::Stop));
        assert!(!safety.is_empty());
        for command in safety {
            assert!(command.is_safety_command());
            assert_eq!(command.control_policy(), CommandControlPolicy::Bypass);
        }
        assert_eq!(
            AppCommand::SubmitPrompt {
                content: "hello".into(),
                attachments: Vec::new(),
                mode: SubmitMode::Prompt,
            }
            .control_policy(),
            CommandControlPolicy::GateTransform
        );
    }

    #[test]
    fn envelope_debug_is_metadata_only() {
        let project_id = Uuid::new_v4();
        let envelope = CommandEnvelope::ui(AppCommand::SubmitPrompt {
            content: "prompt-secret-7f13".into(),
            attachments: Vec::new(),
            mode: SubmitMode::Prompt,
        })
        .with_context(Some(project_id), Some("session-key".into()));
        let cloned = envelope.clone();
        assert_eq!(cloned, envelope);
        assert_eq!(envelope.project_id(), Some(project_id));
        assert_eq!(envelope.session_key(), Some("session-key"));
        let debug = format!("{envelope:?}");
        assert!(debug.contains("CommandEnvelope"));
        assert!(debug.contains("AppCommand"));
        assert!(!debug.contains("prompt-secret-7f13"));
        assert!(!debug.contains("SubmitPrompt"));
    }

    #[test]
    fn map_payload_preserves_authenticated_metadata_and_changes_type() {
        let project_id = Uuid::new_v4();
        let envelope =
            CommandEnvelope::new(CommandSource::HookReplay, "payload-secret-26d9".to_owned())
                .with_context(Some(project_id), Some("session-key".to_owned()));
        let command_id = envelope.command_id();

        let mapped = envelope.map_payload(|payload| payload.len());

        assert_eq!(mapped.command_id(), command_id);
        assert_eq!(mapped.source(), CommandSource::HookReplay);
        assert_eq!(mapped.project_id(), Some(project_id));
        assert_eq!(mapped.session_key(), Some("session-key"));
        assert_eq!(*mapped.payload(), "payload-secret-26d9".len());
        let debug = format!("{mapped:?}");
        assert!(debug.contains("usize"));
        assert!(!debug.contains("payload-secret-26d9"));
    }

    #[test]
    fn lifecycle_has_all_typed_stages_without_payload() {
        let envelope = CommandEnvelope::hook_replay(AppCommand::SubmitPrompt {
            content: "prompt-secret-a981".into(),
            attachments: Vec::new(),
            mode: SubmitMode::Prompt,
        });
        let stages = [
            CommandStage::Submitted,
            CommandStage::Transforming,
            CommandStage::Accepted,
            CommandStage::denied("policy denied"),
            CommandStage::Executing,
            CommandStage::Completed,
            CommandStage::failed("provider diagnostic"),
        ];
        assert_eq!(
            stages.map(|stage| stage.stage_name()),
            [
                "submitted",
                "transforming",
                "accepted",
                "denied",
                "executing",
                "completed",
                "failed",
            ]
        );
        let lifecycle = CommandLifecycle::submitted(&envelope)
            .with_stage(CommandStage::failed("bounded failure"));
        assert_eq!(lifecycle.command_id(), envelope.command_id());
        assert_eq!(lifecycle.command_name(), "prompt.submit");
        assert_eq!(lifecycle.source(), CommandSource::HookReplay);
        let debug = format!("{lifecycle:?}");
        assert!(!debug.contains("prompt-secret-a981"));
        assert!(!debug.contains("api_key"));
    }

    #[test]
    fn diagnostics_and_shell_debug_are_bounded_and_redacted() {
        let diagnostic = CommandDiagnostic::new("界".repeat(300));
        assert!(diagnostic.as_str().len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(
            diagnostic
                .as_str()
                .is_char_boundary(diagnostic.as_str().len())
        );

        let command = ShellCommand::SaveProvider {
            profile: ProviderProfile {
                id: Uuid::nil(),
                name: "provider".into(),
                base_url: "https://example.invalid".into(),
                protocol: ProviderProtocol::OpenAiCompletions,
                models: Vec::new(),
                updated_at_ms: 0,
                has_api_key: true,
            },
            api_key: Some("api-key-secret-33c1".into()),
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("shell.provider.save"));
        assert!(!debug.contains("api-key-secret-33c1"));
        assert!(!debug.contains("example.invalid"));
    }
}
