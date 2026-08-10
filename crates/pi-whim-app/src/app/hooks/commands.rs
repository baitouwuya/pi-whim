//! Typed UI command Hook control and metadata-only command/state Observe adapters.

use crossbeam_channel::Sender;
use pi_whim_core::{ProjectId, SubmitMode, ThinkingLevel};
use pi_whim_engine::{
    changes::{ChangeSet, CommitScope, CommitSource},
    commands::{
        AppCommand, CommandControlPolicy, CommandEnvelope, CommandLifecycle, CommandSource,
        CommandStage,
    },
};
use pi_whim_hook_host::{
    HookAuditEvent, HookAuditOutcome, HookGateDecision, HookInvocationContext, HookPayload,
    HookScopeHandle, HookTransformResult,
};
use serde::{Deserialize, Serialize};

pub(super) const COMMAND_SUBMITTING_EVENT: &str = "pi.ui.command.submitting";
pub(super) const COMMAND_LIFECYCLE_EVENT: &str = "pi.ui.command.lifecycle";
pub(super) const STATE_COMMITTED_EVENT: &str = "pi.state.committed";
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_ATTACHMENTS: usize = 64;
const MAX_AGENTS_MD_BYTES: usize = 48 * 1024;
const MAX_BLOCKED_PATTERNS: usize = 64;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_SESSION_KEY_BYTES: usize = 512;
pub(super) const TYPED_VALIDATION_AUDIT_ID: &str = "app.typed_validation";

#[derive(Clone)]
pub(crate) struct CommandHookController {
    scope: HookScopeHandle,
    context: HookInvocationContext,
    audit_sender: Sender<HookAuditEvent>,
}

impl std::fmt::Debug for CommandHookController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandHookController")
            .field("scope_id", &self.context.scope_id)
            .field("revision", &self.context.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandHookError {
    InvalidCommand(&'static str),
    Denied { hook_id: String, message: String },
    FailedClosed { hook_id: String },
}

impl std::fmt::Display for CommandHookError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCommand(reason) => write!(formatter, "invalid command: {reason}"),
            Self::Denied { hook_id, message } => {
                write!(formatter, "command denied by hook {hook_id}: {message}")
            }
            Self::FailedClosed { hook_id } => {
                write!(formatter, "command denied because hook {hook_id} failed")
            }
        }
    }
}

impl std::error::Error for CommandHookError {}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandSubmittingEvent<A> {
    command_id: String,
    command_name: String,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    arguments: A,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptArguments {
    content: String,
    mode: SubmitMode,
    attachment_count: usize,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlockedPatternsArguments {
    patterns: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentsMdArguments {
    content: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingLevelArguments {
    level: ThinkingLevel,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Clone)]
struct CommandMetadata {
    command_id: String,
    command_name: String,
    source: String,
    project_id: Option<String>,
}

impl CommandMetadata {
    fn from_envelope(envelope: &CommandEnvelope<AppCommand>) -> Result<Self, CommandHookError> {
        if envelope
            .session_key()
            .is_some_and(|key| key.len() > MAX_SESSION_KEY_BYTES)
        {
            return Err(CommandHookError::InvalidCommand(
                "session context exceeds its bound",
            ));
        }
        Ok(Self {
            command_id: envelope.command_id().to_string(),
            command_name: envelope.payload().command_name().to_owned(),
            source: command_source_name(envelope.source()).to_owned(),
            project_id: envelope.project_id().map(|id| id.to_string()),
        })
    }

    fn event<A>(&self, arguments: A) -> CommandSubmittingEvent<A> {
        CommandSubmittingEvent {
            command_id: self.command_id.clone(),
            command_name: self.command_name.clone(),
            source: self.source.clone(),
            project_id: self.project_id.clone(),
            arguments,
        }
    }

    fn validate<A>(&self, event: &CommandSubmittingEvent<A>) -> Result<(), CommandHookError> {
        if event.command_id != self.command_id
            || event.command_name != self.command_name
            || event.source != self.source
            || event.project_id != self.project_id
        {
            return Err(CommandHookError::InvalidCommand(
                "hook changed authenticated command metadata",
            ));
        }
        Ok(())
    }
}

impl CommandHookController {
    pub(super) fn new(
        scope: HookScopeHandle,
        context: HookInvocationContext,
        audit_sender: Sender<HookAuditEvent>,
    ) -> Self {
        Self {
            scope,
            context,
            audit_sender,
        }
    }

    #[cfg(test)]
    pub(super) fn scope_id(&self) -> &str {
        &self.context.scope_id
    }

    /// Applies the typed UI command policy without retaining application state.
    pub(crate) fn control(
        &self,
        envelope: CommandEnvelope<AppCommand>,
    ) -> Result<CommandEnvelope<AppCommand>, CommandHookError> {
        if envelope.payload().is_safety_command()
            || envelope.payload().control_policy() != CommandControlPolicy::GateTransform
        {
            return Ok(envelope);
        }

        let metadata = CommandMetadata::from_envelope(&envelope)?;
        let original_payload = command_payload(&metadata, envelope.payload())?;
        let transformed = match self.scope.transform(
            COMMAND_SUBMITTING_EVENT,
            self.context.clone(),
            original_payload.clone(),
        ) {
            Ok(HookTransformResult::Transformed(payload)) => payload,
            Ok(HookTransformResult::Preserved { .. }) | Err(_) => {
                self.audit_typed_preserve();
                original_payload.clone()
            }
        };
        let final_command =
            match parse_transformed_command(&metadata, envelope.payload(), transformed) {
                Ok(command) => command,
                Err(_) => {
                    self.audit_typed_preserve();
                    envelope.payload().clone()
                }
            };

        // Rebuild and validate the exact typed payload after Transform. Gate
        // therefore always observes the value the handler will receive.
        let final_payload = command_payload(&metadata, &final_command)?;
        match self.scope.gate(
            COMMAND_SUBMITTING_EVENT,
            self.context.clone(),
            final_payload,
        ) {
            Ok(HookGateDecision::Allow) => Ok(envelope.map_payload(|_| final_command)),
            Ok(HookGateDecision::Deny { hook_id, message }) => {
                Err(CommandHookError::Denied { hook_id, message })
            }
            Ok(HookGateDecision::FailedClosed { hook_id, .. }) => {
                Err(CommandHookError::FailedClosed { hook_id })
            }
            Err(_) => Err(CommandHookError::FailedClosed {
                hook_id: "hook-host".to_owned(),
            }),
        }
    }

    /// Emits metadata-only lifecycle state after the local typed signal.
    pub(crate) fn observe_lifecycle(&self, lifecycle: &CommandLifecycle) {
        let diagnostic = match lifecycle.stage() {
            CommandStage::Denied(diagnostic) | CommandStage::Failed(diagnostic) => {
                Some(diagnostic.as_str().to_owned())
            }
            CommandStage::Submitted
            | CommandStage::Transforming
            | CommandStage::Accepted
            | CommandStage::Executing
            | CommandStage::Completed => None,
        };
        let event = CommandLifecycleEvent {
            command_id: lifecycle.command_id().to_string(),
            command_name: lifecycle.command_name().to_owned(),
            source: command_source_name(lifecycle.source()).to_owned(),
            project_id: lifecycle.project_id().map(|id| id.to_string()),
            stage: lifecycle.stage().stage_name().to_owned(),
            diagnostic,
        };
        self.observe(COMMAND_LIFECYCLE_EVENT, &event);
    }

    /// Emits metadata-only reducer commit information after local publication.
    pub(crate) fn observe_change_set(&self, change_set: &ChangeSet) {
        let (scope, project_id) = change_set_scope(change_set);
        let event = StateCommittedEvent {
            revision: change_set.revision.get(),
            topics: change_set
                .changed_topics
                .iter()
                .map(|topic| topic.as_str().to_owned())
                .collect(),
            action_count: change_set.action_count,
            coalesced: change_set.coalesced,
            scope: scope.to_owned(),
            commit_source: commit_source_name(change_set.source).to_owned(),
            project_id: project_id.map(|id| id.to_string()),
        };
        self.observe(STATE_COMMITTED_EVENT, &event);
    }

    fn observe(&self, event: &str, payload: &impl Serialize) {
        if let Ok(payload) = serialize_payload(payload) {
            let _ = self.scope.observe(event, self.context.clone(), payload);
        }
    }

    fn audit_typed_preserve(&self) {
        let _ = self.audit_sender.send(HookAuditEvent {
            hook_id: TYPED_VALIDATION_AUDIT_ID.to_owned(),
            scope_id: self.context.scope_id.clone(),
            event: COMMAND_SUBMITTING_EVENT.to_owned(),
            kind: "transform".to_owned(),
            outcome: HookAuditOutcome::Preserved,
            duration_ms: 0,
            revision: self.context.revision.clone(),
            dropped: false,
            restart_count: 0,
            drop_count: 0,
            grants_hash: None,
        });
    }
}

#[derive(Serialize)]
struct CommandLifecycleEvent {
    command_id: String,
    command_name: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
}

#[derive(Serialize)]
struct StateCommittedEvent {
    revision: u64,
    topics: Vec<String>,
    action_count: usize,
    coalesced: bool,
    scope: String,
    commit_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
}

fn command_payload(
    metadata: &CommandMetadata,
    command: &AppCommand,
) -> Result<HookPayload, CommandHookError> {
    match command {
        AppCommand::SubmitPrompt {
            content,
            attachments,
            mode,
        } => {
            if attachments.len() > MAX_ATTACHMENTS {
                return Err(CommandHookError::InvalidCommand(
                    "prompt attachment count exceeds its bound",
                ));
            }
            validate_bounded(
                content,
                MAX_PROMPT_BYTES,
                "prompt content exceeds its bound",
            )?;
            serialize_command_event(metadata.event(PromptArguments {
                content: content.clone(),
                mode: *mode,
                attachment_count: attachments.len(),
            }))
        }
        AppCommand::SetBlockedPatterns(patterns) => {
            validate_patterns(patterns)?;
            serialize_command_event(metadata.event(BlockedPatternsArguments {
                patterns: patterns.clone(),
            }))
        }
        AppCommand::SaveGlobalAgentsMd(content) | AppCommand::SaveProjectAgentsMd(content) => {
            validate_bounded(
                content,
                MAX_AGENTS_MD_BYTES,
                "AGENTS.md content exceeds its bound",
            )?;
            serialize_command_event(metadata.event(AgentsMdArguments {
                content: content.clone(),
            }))
        }
        AppCommand::SetThinkingLevel(level) => {
            serialize_command_event(metadata.event(ThinkingLevelArguments { level: *level }))
        }
        command if command.control_policy() == CommandControlPolicy::GateTransform => {
            serialize_command_event(metadata.event(NoArguments::default()))
        }
        _ => Err(CommandHookError::InvalidCommand(
            "command is not eligible for hook control",
        )),
    }
}

fn parse_transformed_command(
    metadata: &CommandMetadata,
    original: &AppCommand,
    payload: HookPayload,
) -> Result<AppCommand, CommandHookError> {
    let value = payload.into_value();
    match original {
        AppCommand::SubmitPrompt { attachments, .. } => {
            let event: CommandSubmittingEvent<PromptArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            validate_bounded(
                &event.arguments.content,
                MAX_PROMPT_BYTES,
                "prompt content exceeds its bound",
            )?;
            if event.arguments.attachment_count != attachments.len() {
                return Err(CommandHookError::InvalidCommand(
                    "hook changed prompt attachment metadata",
                ));
            }
            Ok(AppCommand::SubmitPrompt {
                content: event.arguments.content,
                attachments: attachments.clone(),
                mode: event.arguments.mode,
            })
        }
        AppCommand::SetBlockedPatterns(_) => {
            let event: CommandSubmittingEvent<BlockedPatternsArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            validate_patterns(&event.arguments.patterns)?;
            Ok(AppCommand::SetBlockedPatterns(event.arguments.patterns))
        }
        AppCommand::SaveGlobalAgentsMd(_) => {
            let event: CommandSubmittingEvent<AgentsMdArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            validate_bounded(
                &event.arguments.content,
                MAX_AGENTS_MD_BYTES,
                "AGENTS.md content exceeds its bound",
            )?;
            Ok(AppCommand::SaveGlobalAgentsMd(event.arguments.content))
        }
        AppCommand::SaveProjectAgentsMd(_) => {
            let event: CommandSubmittingEvent<AgentsMdArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            validate_bounded(
                &event.arguments.content,
                MAX_AGENTS_MD_BYTES,
                "AGENTS.md content exceeds its bound",
            )?;
            Ok(AppCommand::SaveProjectAgentsMd(event.arguments.content))
        }
        AppCommand::SetThinkingLevel(_) => {
            let event: CommandSubmittingEvent<ThinkingLevelArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            Ok(AppCommand::SetThinkingLevel(event.arguments.level))
        }
        command if command.control_policy() == CommandControlPolicy::GateTransform => {
            let event: CommandSubmittingEvent<NoArguments> = parse_event(value)?;
            metadata.validate(&event)?;
            Ok(command.clone())
        }
        _ => Err(CommandHookError::InvalidCommand(
            "command is not eligible for hook control",
        )),
    }
}

fn serialize_command_event(event: impl Serialize) -> Result<HookPayload, CommandHookError> {
    serialize_payload(&event)
        .map_err(|_| CommandHookError::InvalidCommand("command payload is invalid"))
}

fn serialize_payload(payload: &impl Serialize) -> Result<HookPayload, String> {
    serde_json::to_value(payload)
        .map_err(|error| error.to_string())
        .and_then(|value| HookPayload::from_value(value).map_err(|error| error.to_string()))
}

fn parse_event<T: for<'de> Deserialize<'de>>(
    value: serde_json::Value,
) -> Result<T, CommandHookError> {
    serde_json::from_value(value)
        .map_err(|_| CommandHookError::InvalidCommand("hook returned an invalid typed payload"))
}

fn validate_bounded(
    value: &str,
    max_bytes: usize,
    reason: &'static str,
) -> Result<(), CommandHookError> {
    if value.len() > max_bytes {
        Err(CommandHookError::InvalidCommand(reason))
    } else {
        Ok(())
    }
}

fn validate_patterns(patterns: &[String]) -> Result<(), CommandHookError> {
    if patterns.len() > MAX_BLOCKED_PATTERNS {
        return Err(CommandHookError::InvalidCommand(
            "blocked pattern count exceeds its bound",
        ));
    }
    for pattern in patterns {
        validate_bounded(
            pattern,
            MAX_PATTERN_BYTES,
            "blocked pattern exceeds its bound",
        )?;
    }
    Ok(())
}

fn command_source_name(source: CommandSource) -> &'static str {
    match source {
        CommandSource::Ui => "ui",
        CommandSource::System => "system",
        CommandSource::HookReplay => "hook_replay",
    }
}

fn commit_source_name(source: CommitSource) -> &'static str {
    match source {
        CommitSource::RuntimeEvent => "runtime_event",
        CommitSource::UserCommand => "user_command",
        CommitSource::ControlRefresh => "control_refresh",
        CommitSource::PersistenceLoad => "persistence_load",
        CommitSource::InternalEffect => "internal_effect",
        CommitSource::Test => "test",
    }
}

fn change_set_scope(change_set: &ChangeSet) -> (&'static str, Option<ProjectId>) {
    match change_set.scope {
        CommitScope::Global => ("global", None),
        CommitScope::Session(identity) => ("session", identity.project_id),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::super::hook_test_lock;
    use super::*;
    use crossbeam_channel::{Receiver, unbounded};
    use pi_whim_core::{Attachment, AttachmentKind};
    use pi_whim_engine::{
        changes::{StateTopic, TransactionRevision},
        commands::CommandEnvelope,
    };
    use pi_whim_hook_host::{
        ApprovedHookManifest, EventRegistry, HookHostManager, HookManifest, HookScopeKey,
    };
    use pi_whim_persistence::hook_manifest_fingerprint;
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;

    fn resident_script(
        directory: &TempDir,
        name: &str,
        hook_id: &str,
        event: &str,
        kind: &str,
        response: &str,
        record: Option<&Path>,
    ) -> Result<PathBuf, String> {
        let path = directory.path().join(name);
        let record = record
            .map(|path| format!("printf '%s\\n' \"$request\" >> '{}'", path.display()))
            .unwrap_or_else(|| ":".to_owned());
        let source = format!(
            r#"#!/bin/sh
    IFS= read -r hello || exit 2
    hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
    printf '{{"type":"ready","hook_id":"{hook_id}","event":"{event}","kind":"{kind}","hello_id":"%s"}}\n' "$hello_id"
    while IFS= read -r request; do
      {record}
      request_id=$(printf '%s\n' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
      printf '{{"type":"response","request_id":"%s","hook_id":"{hook_id}","event":"{event}","response":{response}}}\n' "$request_id"
    done
    "#
        );
        fs::write(&path, source).map_err(|error| error.to_string())?;
        let mut permissions = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn conditional_gate_script(
        directory: &TempDir,
        name: &str,
        hook_id: &str,
        required: &[&str],
        forbidden: &[&str],
    ) -> Result<PathBuf, String> {
        checked_script(
            directory,
            name,
            hook_id,
            COMMAND_SUBMITTING_EVENT,
            "gate",
            required,
            forbidden,
            true,
        )
    }

    fn checked_observe_script(
        directory: &TempDir,
        name: &str,
        hook_id: &str,
        event: &str,
        required: &[&str],
        forbidden: &[&str],
    ) -> Result<PathBuf, String> {
        checked_script(
            directory, name, hook_id, event, "observe", required, forbidden, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn checked_script(
        directory: &TempDir,
        name: &str,
        hook_id: &str,
        event: &str,
        kind: &str,
        required: &[&str],
        forbidden: &[&str],
        gate: bool,
    ) -> Result<PathBuf, String> {
        let path = directory.path().join(name);
        let required_checks = required
            .iter()
            .map(|value| {
                format!(
                    "if ! printf '%s\\n' \"$request\" | grep -F -q '{}'; then valid=false; fi",
                    value
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        let forbidden_checks = forbidden
            .iter()
            .map(|value| {
                format!(
                    "if printf '%s\\n' \"$request\" | grep -F -q '{}'; then valid=false; fi",
                    value
                )
            })
            .collect::<Vec<_>>()
            .join("\n  ");
        let response = if gate {
            r#"decision=allow
      if [ "$valid" != true ]; then decision=deny; fi
      printf '{"type":"response","request_id":"%s","hook_id":"HOOK_ID","event":"EVENT","response":{"kind":"gate","decision":"%s","message":"unexpected typed payload"}}\n' "$request_id" "$decision""#.to_owned()
        } else {
            r#"if [ "$valid" != true ]; then exit 7; fi
      printf '{"type":"response","request_id":"%s","hook_id":"HOOK_ID","event":"EVENT","response":{"kind":"observe","accepted":true}}\n' "$request_id""#.to_owned()
        }
        .replace("HOOK_ID", hook_id)
        .replace("EVENT", event);
        let source = format!(
            r#"#!/bin/sh
    IFS= read -r hello || exit 2
    hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
    printf '{{"type":"ready","hook_id":"{hook_id}","event":"{event}","kind":"{kind}","hello_id":"%s"}}\n' "$hello_id"
    while IFS= read -r request; do
      request_id=$(printf '%s\n' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
      valid=true
      {required_checks}
      {forbidden_checks}
      {response}
    done
    "#
        );
        fs::write(&path, source).map_err(|error| error.to_string())?;
        let mut permissions = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn v2_controller(
        directory: &TempDir,
        hooks: Vec<Value>,
        entrypoints: &[(&str, &Path)],
    ) -> Result<
        (
            HookHostManager,
            CommandHookController,
            Receiver<HookAuditEvent>,
        ),
        String,
    > {
        let manifest = HookManifest::parse_json(&json!({"version": 2, "hooks": hooks}).to_string())
            .map(|manifest| manifest.with_revision("ui-r1"))
            .map_err(|error| error.to_string())?;
        let fingerprints = entrypoints
            .iter()
            .map(|(id, path)| {
                fs::read(path)
                    .map(|bytes| ((*id).to_owned(), hook_manifest_fingerprint(&bytes)))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let approved = ApprovedHookManifest::new(manifest, "ui-r1", fingerprints)
            .map_err(|error| error.to_string())?;
        let manager = HookHostManager::new_with_registry(EventRegistry::default(), approved)
            .map_err(|error| error.to_string())?;
        let key =
            HookScopeKey::project(directory.path(), "ui-r1").map_err(|error| error.to_string())?;
        let scope = manager
            .open_scope(key.clone(), None)
            .map_err(|error| error.to_string())?;
        let project_root = key
            .project_root
            .as_ref()
            .ok_or_else(|| "project scope lost its root".to_owned())?
            .to_string_lossy()
            .into_owned();
        let context =
            HookInvocationContext::project(scope.scope_id(), key.manifest_revision, project_root);
        let (audit_sender, audit_receiver) = unbounded();
        Ok((
            manager,
            CommandHookController::new(scope, context, audit_sender),
            audit_receiver,
        ))
    }

    fn prompt_envelope(project_id: ProjectId) -> CommandEnvelope<AppCommand> {
        CommandEnvelope::new(
            CommandSource::Ui,
            AppCommand::SubmitPrompt {
                content: "before".to_owned(),
                attachments: vec![Attachment {
                    name: "private.txt".to_owned(),
                    path: "/private/session/attachment-secret.txt".to_owned(),
                    kind: AttachmentKind::File,
                    generated_by_app: false,
                }],
                mode: SubmitMode::Prompt,
            },
        )
        .with_context(Some(project_id), Some("private-session-key".to_owned()))
    }

    #[test]
    fn transform_runs_before_gate_on_final_typed_prompt() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let transform = resident_script(
            &directory,
            "transform.sh",
            "transform",
            COMMAND_SUBMITTING_EVENT,
            "transform",
            r#"{"kind":"transform","payload":{"arguments":{"content":"after","mode":"steer","attachment_count":1}}}"#,
            None,
        )?;
        let gate = conditional_gate_script(
            &directory,
            "gate.sh",
            "gate",
            &[r#""content":"after""#, r#""attachment_count":1"#],
            &[
                "attachment-secret.txt",
                "private-session-key",
                r#""attachments""#,
                r#""session_path""#,
                r#""api_key""#,
                r#""endpoint""#,
                r#""secret""#,
                r#""clipboard""#,
            ],
        )?;
        let hooks = vec![
            json!({
                "id": "transform", "event": COMMAND_SUBMITTING_EVENT, "kind": "transform",
                "command": [transform],
                "fields": ["command_id", "command_name", "source", "project_id", "arguments"]
            }),
            json!({
                "id": "gate", "event": COMMAND_SUBMITTING_EVENT, "kind": "gate",
                "command": [gate],
                "fields": ["command_id", "command_name", "source", "project_id", "arguments"]
            }),
        ];
        let (_manager, controller, _audits) = v2_controller(
            &directory,
            hooks,
            &[("transform", &transform), ("gate", &gate)],
        )?;
        let project_id = ProjectId::new_v4();
        let original = prompt_envelope(project_id);
        let command_id = original.command_id();
        let controlled = controller
            .control(original)
            .map_err(|error| error.to_string())?;

        assert_eq!(controlled.command_id(), command_id);
        assert_eq!(controlled.source(), CommandSource::Ui);
        assert_eq!(controlled.project_id(), Some(project_id));
        assert_eq!(controlled.session_key(), Some("private-session-key"));
        let AppCommand::SubmitPrompt {
            content,
            attachments,
            mode,
        } = controlled.payload()
        else {
            return Err("controller changed the command variant".to_owned());
        };
        assert_eq!(content, "after");
        assert_eq!(*mode, SubmitMode::Steer);
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            attachments[0].path,
            "/private/session/attachment-secret.txt"
        );
        Ok(())
    }

    #[test]
    fn invalid_transform_preserves_original_before_gate_and_audits() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let transform = resident_script(
            &directory,
            "invalid-transform.sh",
            "invalid-transform",
            COMMAND_SUBMITTING_EVENT,
            "transform",
            r#"{"kind":"transform","payload":{"arguments":{"content":"changed","mode":"invalid","attachment_count":1}}}"#,
            None,
        )?;
        let gate = conditional_gate_script(
            &directory,
            "gate.sh",
            "gate",
            &[r#""content":"before""#, r#""mode":"prompt""#],
            &[r#""content":"changed""#],
        )?;
        let hooks = vec![
            json!({
                "id": "invalid-transform", "event": COMMAND_SUBMITTING_EVENT, "kind": "transform",
                "command": [transform], "fields": ["arguments"]
            }),
            json!({
                "id": "gate", "event": COMMAND_SUBMITTING_EVENT, "kind": "gate",
                "command": [gate], "fields": ["command_name", "arguments"]
            }),
        ];
        let (_manager, controller, audits) = v2_controller(
            &directory,
            hooks,
            &[("invalid-transform", &transform), ("gate", &gate)],
        )?;
        let controlled = controller
            .control(prompt_envelope(ProjectId::new_v4()))
            .map_err(|error| error.to_string())?;
        let AppCommand::SubmitPrompt { content, mode, .. } = controlled.payload() else {
            return Err("controller changed the command variant".to_owned());
        };
        assert_eq!(content, "before");
        assert_eq!(*mode, SubmitMode::Prompt);
        let audit = audits.try_recv().map_err(|error| error.to_string())?;
        assert_eq!(audit.hook_id, TYPED_VALIDATION_AUDIT_ID);
        assert_eq!(audit.outcome, HookAuditOutcome::Preserved);
        assert_eq!(audit.event, COMMAND_SUBMITTING_EVENT);
        Ok(())
    }

    #[test]
    fn gate_failure_is_fail_closed() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let gate = directory.path().join("failed-gate.sh");
        fs::write(
            &gate,
            format!(
                r#"#!/bin/sh
    IFS= read -r hello || exit 2
    hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
    printf '{{"type":"ready","hook_id":"failed-gate","event":"{COMMAND_SUBMITTING_EVENT}","kind":"gate","hello_id":"%s"}}\n' "$hello_id"
    IFS= read -r request || exit 3
    exit 7
    "#
            ),
        )
        .map_err(|error| error.to_string())?;
        let mut permissions = fs::metadata(&gate)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&gate, permissions).map_err(|error| error.to_string())?;
        let hooks = vec![json!({
            "id": "failed-gate", "event": COMMAND_SUBMITTING_EVENT, "kind": "gate",
            "command": [gate], "fields": ["command_name"]
        })];
        let (_manager, controller, _audits) =
            v2_controller(&directory, hooks, &[("failed-gate", &gate)])?;
        assert!(matches!(
            controller.control(prompt_envelope(ProjectId::new_v4())),
            Err(CommandHookError::FailedClosed { .. })
        ));
        Ok(())
    }

    #[test]
    fn safety_commands_bypass_external_control() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let gate_log = directory.path().join("gate.log");
        let gate = resident_script(
            &directory,
            "deny.sh",
            "deny",
            COMMAND_SUBMITTING_EVENT,
            "gate",
            r#"{"kind":"gate","decision":"deny","message":"blocked"}"#,
            Some(&gate_log),
        )?;
        let hooks = vec![json!({
            "id": "deny", "event": COMMAND_SUBMITTING_EVENT, "kind": "gate",
            "command": [gate], "fields": ["command_name"]
        })];
        let (_manager, controller, _audits) = v2_controller(&directory, hooks, &[("deny", &gate)])?;
        let envelope = CommandEnvelope::ui(AppCommand::Stop);
        let command_id = envelope.command_id();
        let controlled = controller
            .control(envelope)
            .map_err(|error| error.to_string())?;
        assert_eq!(controlled.command_id(), command_id);
        assert_eq!(controlled.payload(), &AppCommand::Stop);
        thread::sleep(Duration::from_millis(50));
        assert!(!gate_log.exists());
        Ok(())
    }

    #[test]
    fn lifecycle_and_state_observe_are_metadata_only() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let forbidden = [
            "payload-secret-123",
            "session-secret-456",
            r#""payload""#,
            r#""attachments""#,
            r#""api_key""#,
            r#""endpoint""#,
            r#""secret""#,
            r#""clipboard""#,
        ];
        let lifecycle = checked_observe_script(
            &directory,
            "lifecycle.sh",
            "lifecycle",
            COMMAND_LIFECYCLE_EVENT,
            &[
                r#""stage":"failed""#,
                "bounded diagnostic",
                r#""command_name":"prompt.submit""#,
            ],
            &forbidden,
        )?;
        let state = checked_observe_script(
            &directory,
            "state.sh",
            "state",
            STATE_COMMITTED_EVENT,
            &[
                r#""revision":7"#,
                r#""topics":["conversation","queue"]"#,
                r#""action_count":2"#,
            ],
            &forbidden,
        )?;
        let hooks = vec![
            json!({
                "id": "lifecycle", "event": COMMAND_LIFECYCLE_EVENT, "kind": "observe",
                "command": [lifecycle],
                "fields": ["command_id", "command_name", "source", "project_id", "stage", "diagnostic"]
            }),
            json!({
                "id": "state", "event": STATE_COMMITTED_EVENT, "kind": "observe",
                "command": [state],
                "fields": ["revision", "topics", "action_count", "coalesced", "scope", "commit_source", "project_id"]
            }),
        ];
        let (manager, controller, _audits) = v2_controller(
            &directory,
            hooks,
            &[("lifecycle", &lifecycle), ("state", &state)],
        )?;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_sink = observed.clone();
        let _subscription = manager.audit_signal().subscribe_fn(move |event| {
            if let Ok(mut events) = observed_sink.lock() {
                events.push(event);
            }
        });
        let project_id = ProjectId::new_v4();
        let envelope = CommandEnvelope::ui(AppCommand::SubmitPrompt {
            content: "payload-secret-123".to_owned(),
            attachments: Vec::new(),
            mode: SubmitMode::Prompt,
        })
        .with_context(Some(project_id), Some("session-secret-456".to_owned()));
        let lifecycle_event = CommandLifecycle::submitted(&envelope)
            .with_stage(CommandStage::failed("bounded diagnostic"));
        controller.observe_lifecycle(&lifecycle_event);
        controller.observe_change_set(&ChangeSet {
            revision: TransactionRevision::new(7),
            scope: CommitScope::Global,
            source: CommitSource::UserCommand,
            changed_topics: vec![StateTopic::Conversation, StateTopic::Queue],
            action_count: 2,
            coalesced: true,
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count = observed.lock().map_err(|error| error.to_string())?.len();
            if count >= 2 || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let audits = observed.lock().map_err(|error| error.to_string())?;
        assert_eq!(audits.len(), 2);
        assert!(
            audits
                .iter()
                .all(|audit| audit.outcome == HookAuditOutcome::Observed)
        );
        Ok(())
    }
}
