//! The window, and the wiring between it and the orchestration.
//!
//! Split from [`crate::app`] because it is the only part that names a UI: typed
//! app and shell command signals flow into the orchestration here, and state
//! ChangeSet-driven feature projections flow back to the view.
//!
//! There is no frame loop here. The egui build polled every session's channel and
//! every control refresh once a frame, whether or not anything had arrived, and
//! asked for a redraw every 50ms while a session was busy. Under gpui the arrivals
//! do the waking: [`pi_whim_gpui::pump`] blocks on a background thread and returns
//! to the main thread with each batch, so an idle app costs nothing.

use std::{collections::VecDeque, convert::Infallible, path::Path};

use gpui::{ClipboardItem, Context, Entity, IntoElement, PathPromptOptions, Render, Task, Window};
use pi_whim_core::{Attachment, SubmitMode};
use pi_whim_engine::{
    ChangeSet,
    commands::{
        AppCommand, CommandControlPolicy, CommandDiagnostic, CommandEnvelope, CommandLifecycle,
        CommandStage, ShellCommand,
    },
};
use pi_whim_gpui::{
    ConversationProjection, NavigationProjection, RuntimeProjection, SettingsProjection,
    SignalBridge, StateSignalBridge, Workspace, WorkspaceStateSelections, pump,
};
use pi_whim_one_shot_ai::MAX_ONE_SHOT_INPUT_BYTES;
use pi_whim_persistence::session_title_context_from_jsonl;
use pi_whim_runtime::{AgentRuntime, test_search_engine};
use pi_whim_signal::SignalEvent;

use crate::app::{CommandHookController, CommandHookError, PiWhimApplication, Picker};

fn command_values<T>(events: Vec<SignalEvent<T, Infallible>>) -> impl Iterator<Item = T> {
    events.into_iter().filter_map(|event| match event {
        SignalEvent::Next(command) => Some(command),
        SignalEvent::Error(error) => match error {},
        SignalEvent::Complete => None,
    })
}

fn latest_state_value<T>(events: Vec<SignalEvent<T, Infallible>>) -> Option<T> {
    command_values(events).last()
}

#[derive(Clone, PartialEq)]
struct PromptDraft {
    content: String,
    attachments: Vec<Attachment>,
    mode: SubmitMode,
}

fn preserve_prompt_draft(command: AppCommand) -> (AppCommand, Option<PromptDraft>) {
    match command {
        AppCommand::SubmitPrompt {
            content,
            attachments,
            mode,
        } => {
            let draft = PromptDraft {
                content,
                attachments,
                mode,
            };
            (
                AppCommand::SubmitPrompt {
                    content: draft.content.clone(),
                    attachments: draft.attachments.clone(),
                    mode: draft.mode,
                },
                Some(draft),
            )
        }
        command => (command, None),
    }
}

struct QueuedCommandControl {
    controller: CommandHookController,
    envelope: CommandEnvelope<AppCommand>,
    lifecycle: CommandLifecycle,
    prompt_draft: Option<PromptDraft>,
}

struct QueuedLifecycleObserve {
    controller: CommandHookController,
    lifecycle: CommandLifecycle,
}

struct OrderedControlQueue<T> {
    pending: VecDeque<T>,
    in_flight: bool,
}

impl<T> Default for OrderedControlQueue<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            in_flight: false,
        }
    }
}

impl<T> OrderedControlQueue<T> {
    fn enqueue(&mut self, item: T) {
        self.pending.push_back(item);
    }

    fn begin_next(&mut self) -> Option<T> {
        if self.in_flight {
            return None;
        }
        let next = self.pending.pop_front()?;
        self.in_flight = true;
        Some(next)
    }

    fn finish_current(&mut self) {
        self.in_flight = false;
    }
}

enum ControlledCompletion {
    Execute(Box<CommandEnvelope<AppCommand>>),
    Denied(CommandDiagnostic),
}

fn requires_background_control(command: &AppCommand, has_controller: bool) -> bool {
    has_controller
        && !command.is_safety_command()
        && command.control_policy() == CommandControlPolicy::GateTransform
}

fn resolve_controlled_command(
    result: Result<CommandEnvelope<AppCommand>, CommandHookError>,
) -> ControlledCompletion {
    match result {
        Ok(envelope) => ControlledCompletion::Execute(Box::new(envelope)),
        Err(error) => ControlledCompletion::Denied(command_hook_diagnostic(&error)),
    }
}

fn command_hook_diagnostic(error: &CommandHookError) -> CommandDiagnostic {
    let message = match error {
        CommandHookError::InvalidCommand(_) => "command rejected by typed validation",
        CommandHookError::Denied { .. } => "command denied by external hook",
        CommandHookError::FailedClosed { .. } => "command hook failed closed",
    };
    CommandDiagnostic::new(message)
}

fn prompt_route_is_current(
    expected_project: Option<pi_whim_core::ProjectId>,
    selected_project: Option<pi_whim_core::ProjectId>,
    can_submit: bool,
) -> bool {
    can_submit && expected_project.is_some() && expected_project == selected_project
}

/// The window's view, and what it drives.
///
/// One entity owns both typed command signal bridges and the state projection.
/// Each app or shell signal preserves its own FIFO order; safety app commands use
/// the existing bypass path, without promising ordering across the two lanes.
pub struct Host<R: AgentRuntime + 'static> {
    application: PiWhimApplication<R>,
    shell: Entity<Workspace>,
    /// The loops delivering session events, control answers, and the catalog.
    ///
    /// Held rather than detached, and never read: a [`Task`] cancels when dropped,
    /// so owning them here is what stops every pump when the window goes away.
    #[allow(dead_code, reason = "held for cancellation, not for reading")]
    pumps: Vec<Task<()>>,
    /// Retains command and state subscriptions for the Host lifetime.
    #[allow(dead_code, reason = "held for signal subscription lifetime")]
    app_command_bridge: SignalBridge<AppCommand, Infallible>,
    #[allow(dead_code, reason = "held for signal subscription lifetime")]
    shell_command_bridge: SignalBridge<ShellCommand, Infallible>,
    #[allow(dead_code, reason = "held for signal subscription lifetime")]
    change_set_bridge: SignalBridge<ChangeSet, Infallible>,
    state_selections: WorkspaceStateSelections,
    #[allow(dead_code, reason = "held for state signal subscription lifetime")]
    navigation_bridge: StateSignalBridge<NavigationProjection, Infallible>,
    #[allow(dead_code, reason = "held for state signal subscription lifetime")]
    conversation_bridge: StateSignalBridge<ConversationProjection, Infallible>,
    #[allow(dead_code, reason = "held for state signal subscription lifetime")]
    runtime_bridge: StateSignalBridge<RuntimeProjection, Infallible>,
    #[allow(dead_code, reason = "held for state signal subscription lifetime")]
    settings_bridge: StateSignalBridge<SettingsProjection, Infallible>,
    /// Gate/Transform commands waiting for the single background control slot.
    command_controls: OrderedControlQueue<QueuedCommandControl>,
    /// Metadata-only lifecycle observes, ordered but never awaited by handlers.
    lifecycle_observes: OrderedControlQueue<QueuedLifecycleObserve>,
}

impl<R: AgentRuntime + 'static> Host<R> {
    /// Bind `application` to `shell`, and start listening.
    pub fn new(
        application: PiWhimApplication<R>,
        shell: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Subscribe before the first projection replay reaches the shell so no UI command
        // can be emitted between construction and Host ownership.
        let app_command_bridge = SignalBridge::new(&shell.read(cx).app_commands());
        let shell_command_bridge = SignalBridge::new(&shell.read(cx).shell_commands());
        let state_selections = WorkspaceStateSelections::new(application.state());
        let navigation_bridge = StateSignalBridge::new(&state_selections.navigation_signal())
            .expect("navigation state bridge worker must start");
        let conversation_bridge = StateSignalBridge::new(&state_selections.conversation_signal())
            .expect("conversation state bridge worker must start");
        let runtime_bridge = StateSignalBridge::new(&state_selections.runtime_signal())
            .expect("runtime state bridge worker must start");
        let settings_bridge = StateSignalBridge::new(&state_selections.settings_signal())
            .expect("settings state bridge worker must start");
        let change_set_bridge = SignalBridge::new(&application.change_sets());

        let pumps = vec![
            app_command_bridge.spawn(window, cx, |host, batch, window, cx| {
                let mut handled = false;
                for command in command_values(batch) {
                    handled = true;
                    host.handle_app_command(command, window, cx);
                }
                if handled {
                    host.flush_effects(window, cx);
                }
            }),
            shell_command_bridge.spawn(window, cx, |host, batch, window, cx| {
                let mut handled = false;
                for command in command_values(batch) {
                    handled = true;
                    host.handle_shell(command, window, cx);
                }
                if handled {
                    host.flush_effects(window, cx);
                }
            }),
            change_set_bridge.spawn(window, cx, |host, batch, _window, _cx| {
                for change_set in command_values(batch) {
                    host.state_selections
                        .publish(&change_set, host.application.state());
                }
            }),
            navigation_bridge.spawn(window, cx, |host, batch, window, cx| {
                if let Some(projection) = latest_state_value(batch) {
                    host.shell.update(cx, |shell, cx| {
                        shell.apply_navigation_projection(projection, window, cx);
                    });
                }
            }),
            conversation_bridge.spawn(window, cx, |host, batch, _window, cx| {
                if let Some(projection) = latest_state_value(batch) {
                    host.shell.update(cx, |shell, cx| {
                        shell.apply_conversation_projection(projection, cx);
                    });
                }
            }),
            runtime_bridge.spawn(window, cx, |host, batch, window, cx| {
                if let Some(projection) = latest_state_value(batch) {
                    host.shell.update(cx, |shell, cx| {
                        shell.apply_runtime_projection(projection, window, cx);
                    });
                }
            }),
            settings_bridge.spawn(window, cx, |host, batch, window, cx| {
                if let Some(projection) = latest_state_value(batch) {
                    host.shell.update(cx, |shell, cx| {
                        shell.apply_settings_projection(projection, window, cx);
                    });
                }
            }),
            pump::spawn(
                application.session_events(),
                window,
                cx,
                |host, batch, window, cx| {
                    host.application.handle_deliveries(batch);
                    host.flush_effects(window, cx);
                },
            ),
            pump::spawn(
                application.control_answers(),
                window,
                cx,
                |host, batch, window, cx| {
                    for (key, actions) in batch {
                        host.application.settle_controls(key, actions);
                    }
                    host.flush_effects(window, cx);
                },
            ),
            pump::spawn(
                application.one_shot_installs(),
                window,
                cx,
                |host, batch, window, cx| {
                    for (generation, resolved) in batch {
                        host.application
                            .settle_one_shot_install(generation, resolved);
                    }
                    host.flush_effects(window, cx);
                },
            ),
            pump::spawn(
                application.one_shot_completions(),
                window,
                cx,
                |host, batch, window, cx| {
                    host.application.settle_one_shot_completions(batch);
                    host.flush_effects(window, cx);
                },
            ),
            // Fires once, when the models.dev catalog lands. The egui build asked
            // every frame whether it had; here the arrival says so itself, and the
            // pump ends when the channel closes behind it.
            pump::spawn(
                application.catalog_refreshed(),
                window,
                cx,
                |host, _, window, cx| {
                    host.application.absorb_capability_catalog();
                    host.flush_effects(window, cx);
                },
            ),
        ];

        let mut host = Self {
            application,
            shell,
            pumps,
            app_command_bridge,
            shell_command_bridge,
            change_set_bridge,
            state_selections,
            navigation_bridge,
            conversation_bridge,
            runtime_bridge,
            settings_bridge,
            command_controls: OrderedControlQueue::default(),
            lifecycle_observes: OrderedControlQueue::default(),
        };
        host.flush_effects(window, cx);
        host
    }

    /// Deliver framework-bound effect outboxes after orchestration work.
    ///
    /// Committed domain state travels independently through ChangeSet-driven
    /// feature projections; this method intentionally transports no AppState.
    fn flush_effects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let notices = self.application.take_notices();
        let prompts = self.application.take_prompts();
        let closed = self.application.take_closed_sessions();
        let attachments = self.application.take_attachments();
        // The clipboard is the window's, not the app's, so `/copy` leaves the text
        // in an outbox for this to write.
        if let Some(text) = self.application.take_clipboard() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        // `/attach` and "add project" stage a picker for the same reason: it
        // needs the window.
        if let Some(picker) = self.application.take_picker() {
            self.open_picker(picker, window, cx);
        }
        self.shell.update(cx, |shell, cx| {
            for key in closed {
                shell.forget_session(&key, cx);
            }
            for prompt in prompts {
                shell.ask(prompt, cx);
            }
            // Staged rather than returned: a file picker's result cannot travel
            // back through a request handler that returns nothing.
            for attachment in attachments {
                shell.attach(attachment, cx);
            }
            for notice in notices {
                if notice.is_error() {
                    shell.report_error(notice.message, cx);
                } else {
                    shell.report_info(notice.message, cx);
                }
            }
        });
    }

    /// Carry out one typed domain command from the Workspace signal.
    fn handle_app_command(
        &mut self,
        command: AppCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (command, prompt_draft) = preserve_prompt_draft(command);
        if let Some(draft) = prompt_draft {
            let envelope = self.application.ui_command_envelope(command);
            let lifecycle = CommandLifecycle::submitted(&envelope);
            self.emit_command_lifecycle(lifecycle.clone(), window, cx);

            // The composer cleared only after its readiness check. Recheck
            // before Hook control and restore the exact original draft if
            // the signal delivery lost its session.
            if !prompt_route_is_current(
                envelope.project_id(),
                self.application.state().selected_project,
                self.application.can_submit_prompt(),
            ) {
                self.fail_submission(lifecycle, draft, window, cx);
            } else {
                self.dispatch_envelope(envelope, lifecycle, Some(draft), window, cx);
            }
        } else {
            self.submit_app_command(command, None, window, cx);
        }
    }

    fn emit_command_lifecycle(
        &mut self,
        lifecycle: CommandLifecycle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let observer = self.application.emit_command_lifecycle(lifecycle.clone());
        if let Some(controller) = observer {
            self.lifecycle_observes.enqueue(QueuedLifecycleObserve {
                controller,
                lifecycle,
            });
            self.start_next_lifecycle_observe(window, cx);
        }
    }

    fn start_next_lifecycle_observe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(queued) = self.lifecycle_observes.begin_next() else {
            return;
        };
        cx.spawn_in(window, async move |host, cx| {
            cx.background_executor()
                .spawn(async move { queued.controller.observe_lifecycle(&queued.lifecycle) })
                .await;
            let _ = host.update_in(cx, |host, window, cx| {
                host.lifecycle_observes.finish_current();
                host.start_next_lifecycle_observe(window, cx);
            });
        })
        .detach();
    }

    fn submit_app_command(
        &mut self,
        command: AppCommand,
        prompt_draft: Option<PromptDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let envelope = self.application.ui_command_envelope(command);
        let lifecycle = CommandLifecycle::submitted(&envelope);
        self.emit_command_lifecycle(lifecycle.clone(), window, cx);
        self.dispatch_envelope(envelope, lifecycle, prompt_draft, window, cx);
    }

    fn dispatch_envelope(
        &mut self,
        envelope: CommandEnvelope<AppCommand>,
        lifecycle: CommandLifecycle,
        prompt_draft: Option<PromptDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let controller = self.application.command_hook_controller(&envelope);
        if requires_background_control(envelope.payload(), controller.is_some()) {
            let Some(controller) = controller else {
                self.execute_envelope(envelope, lifecycle, prompt_draft, window, cx);
                return;
            };
            self.emit_command_lifecycle(
                lifecycle.clone().with_stage(CommandStage::Transforming),
                window,
                cx,
            );
            self.command_controls.enqueue(QueuedCommandControl {
                controller,
                envelope,
                lifecycle,
                prompt_draft,
            });
            self.start_next_command_control(window, cx);
        } else {
            self.execute_envelope(envelope, lifecycle, prompt_draft, window, cx);
        }
    }

    fn start_next_command_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(queued) = self.command_controls.begin_next() else {
            return;
        };
        let QueuedCommandControl {
            controller,
            envelope,
            lifecycle,
            prompt_draft,
        } = queued;
        cx.spawn_in(window, async move |host, cx| {
            let completion = cx
                .background_executor()
                .spawn(async move { resolve_controlled_command(controller.control(envelope)) })
                .await;
            let _ = host.update_in(cx, |host, window, cx| {
                host.command_controls.finish_current();
                host.finish_command_control(completion, lifecycle, prompt_draft, window, cx);
                host.start_next_command_control(window, cx);
                host.flush_effects(window, cx);
            });
        })
        .detach();
    }

    fn finish_command_control(
        &mut self,
        completion: ControlledCompletion,
        lifecycle: CommandLifecycle,
        prompt_draft: Option<PromptDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match completion {
            ControlledCompletion::Execute(envelope) => {
                self.execute_envelope(*envelope, lifecycle, prompt_draft, window, cx);
            }
            ControlledCompletion::Denied(diagnostic) => {
                self.emit_command_lifecycle(
                    lifecycle.with_stage(CommandStage::Denied(diagnostic.clone())),
                    window,
                    cx,
                );
                self.application.report_command_diagnostic(&diagnostic);
                if let Some(draft) = prompt_draft {
                    self.restore_submission(draft, window, cx);
                }
            }
        }
    }

    fn execute_envelope(
        &mut self,
        envelope: CommandEnvelope<AppCommand>,
        lifecycle: CommandLifecycle,
        prompt_draft: Option<PromptDraft>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = prompt_draft
            && !prompt_route_is_current(
                envelope.project_id(),
                self.application.state().selected_project,
                self.application.can_submit_prompt(),
            )
        {
            self.fail_submission(lifecycle, draft, window, cx);
            return;
        }

        self.emit_command_lifecycle(
            lifecycle.clone().with_stage(CommandStage::Accepted),
            window,
            cx,
        );
        self.emit_command_lifecycle(
            lifecycle.clone().with_stage(CommandStage::Executing),
            window,
            cx,
        );
        self.application.execute_command_envelope(envelope);
        self.emit_command_lifecycle(lifecycle.with_stage(CommandStage::Completed), window, cx);
    }

    fn fail_submission(
        &mut self,
        lifecycle: CommandLifecycle,
        draft: PromptDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let diagnostic = CommandDiagnostic::new("prompt session is no longer available");
        self.emit_command_lifecycle(
            lifecycle.with_stage(CommandStage::Failed(diagnostic)),
            window,
            cx,
        );
        self.application.report_submission_unavailable();
        self.restore_submission(draft, window, cx);
    }

    fn restore_submission(
        &mut self,
        draft: PromptDraft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shell.update(cx, |shell, cx| {
            shell.restore_submission(draft.content, draft.attachments, window, cx);
        });
    }

    fn handle_shell(&mut self, command: ShellCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            ShellCommand::AddProject => self.open_picker(Picker::Project, window, cx),
            ShellCommand::RevealProject(project_id) => {
                if let Some(project_path) = self.application.project_path(project_id) {
                    let _ = std::process::Command::new("open").arg(project_path).spawn();
                }
            }
            ShellCommand::SmartRenameSession {
                project_id,
                path,
                title,
            } => {
                let transcript_path = path.clone();
                cx.spawn_in(window, async move |host, cx| {
                    let context = cx
                        .background_executor()
                        .spawn(async move {
                            session_title_context_from_jsonl(
                                Path::new(&transcript_path),
                                MAX_ONE_SHOT_INPUT_BYTES,
                            )
                            .ok()
                            .flatten()
                        })
                        .await;
                    let _ = host.update_in(cx, |host, window, cx| {
                        host.application
                            .start_smart_session_rename(project_id, path, title, context);
                        host.flush_effects(window, cx);
                    });
                })
                .detach();
            }
            ShellCommand::CopyToClipboard(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            ShellCommand::AttachPaste(paste) => {
                // A paste of several files is several attachments, so this is a
                // list even though most pastes produce one.
                for attachment in self.application.attachments_for(paste) {
                    self.shell
                        .update(cx, |shell, cx| shell.attach(attachment, cx));
                }
            }
            ShellCommand::PickAttachments => {
                self.open_picker(Picker::Attachments, window, cx);
            }
            ShellCommand::SaveProvider { profile, api_key } => {
                if let Some((id, key_saved)) = self.application.save_provider(profile, api_key) {
                    self.shell.update(cx, |shell, cx| {
                        shell.provider_saved(id, key_saved, window, cx);
                    });
                }
            }
            ShellCommand::SaveSearchEngine { profile, api_key } => {
                if self.application.save_search_engine(profile, api_key) {
                    self.shell.update(cx, |shell, cx| {
                        shell.search_engine_saved(window, cx);
                    });
                }
            }
            ShellCommand::TestSearchEngine {
                profile,
                api_key,
                editor,
            } => {
                let profile_id = profile.id;
                let profile_name = profile.name.clone();
                match self
                    .application
                    .search_engine_test_api_key(&profile, api_key)
                {
                    Ok(api_key) => {
                        cx.spawn_in(window, async move |host, cx| {
                            let result = cx
                                .background_executor()
                                .spawn(
                                    async move { test_search_engine(&profile, api_key.as_deref()) },
                                )
                                .await;
                            let _ = host.update_in(cx, |host, window, cx| {
                                host.application
                                    .report_search_engine_test(&profile_name, &result);
                                host.shell.update(cx, |shell, cx| {
                                    shell.search_engine_test_finished(
                                        profile_id, editor, result, cx,
                                    );
                                });
                                host.flush_effects(window, cx);
                            });
                        })
                        .detach();
                    }
                    Err(error) => {
                        let result = Err(error);
                        self.application
                            .report_search_engine_test(&profile_name, &result);
                        self.shell.update(cx, |shell, cx| {
                            shell.search_engine_test_finished(profile_id, editor, result, cx);
                        });
                    }
                }
            }
            ShellCommand::DiscoverProviderModels {
                profile_id,
                provider_name,
                base_url,
                protocol,
                api_key,
            } => {
                let models = self.application.discover_provider_models(
                    profile_id,
                    provider_name,
                    base_url,
                    protocol,
                    api_key,
                );
                if let Some(models) = models {
                    self.shell.update(cx, |shell, cx| {
                        shell.set_discovered_models(models, cx);
                    });
                }
            }
        }
    }

    /// Open a picker on the orchestration's behalf, and hand back what was chosen.
    ///
    /// Attaching accepts files and folders in the same dialog. A menu used to ask
    /// which of the two first, which only added a click before the same window
    /// opened — the platform picker already lets the reader walk into a folder and
    /// take either. Adding a project is the narrower case: one folder, since that is
    /// what a project is.
    ///
    /// Asynchronous, and that is load-bearing rather than incidental. `rfd`'s
    /// blocking dialogs ran a nested native event loop on the main thread, which
    /// let a pump's task be polled while the app was already borrowed — an abort
    /// inside gpui's `RefCell`, reached without any panic of ours. Awaiting the
    /// paths means the borrow is taken once, after the answer.
    fn open_picker(&mut self, picker: Picker, window: &Window, cx: &mut Context<Self>) {
        let attaching = picker == Picker::Attachments;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            // A project is a folder, so its picker offers only those.
            files: attaching,
            directories: true,
            // Only attachments come in batches; a project is one folder.
            multiple: attaching,
            prompt: None,
        });
        cx.spawn_in(window, async move |host, cx| {
            // Three ways to have nothing: the channel dropped, the platform
            // failed, or the reader cancelled. None is worth reporting.
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let _ = host.update_in(cx, |host, window, cx| {
                host.application.picked(picker, paths);
                host.flush_effects(window, cx);
            });
        })
        .detach();
    }
}

/// Draw the shell.
///
/// The host is the window's root view so that it lives as long as the window:
/// nothing else holds it, and a dropped host would take its pumps with it. What it
/// draws is only the shell — the host itself has no appearance.
impl<R: AgentRuntime + 'static> Render for Host<R> {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.shell.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{AttachmentKind, SubmitMode};
    use uuid::Uuid;

    #[test]
    fn command_signal_values_preserve_fifo_and_ignore_completion() {
        let commands: Vec<_> = command_values(vec![
            SignalEvent::Next(AppCommand::Stop),
            SignalEvent::Next(AppCommand::ClearQueue),
            SignalEvent::Complete,
        ])
        .collect();

        assert_eq!(commands, vec![AppCommand::Stop, AppCommand::ClearQueue]);
    }

    #[test]
    fn typed_prompt_signal_preserves_original_draft_for_recovery() {
        let attachment = Attachment {
            name: "notes.txt".into(),
            path: "/tmp/notes.txt".into(),
            kind: AttachmentKind::File,
            generated_by_app: false,
        };
        let (command, draft) = preserve_prompt_draft(AppCommand::SubmitPrompt {
            content: "keep this exact draft".into(),
            attachments: vec![attachment.clone()],
            mode: SubmitMode::FollowUp,
        });
        let Some(draft) = draft else {
            panic!("typed prompt must retain the Host recovery draft");
        };
        let AppCommand::SubmitPrompt {
            content,
            attachments,
            mode,
        } = command
        else {
            panic!("typed prompt variant must be preserved");
        };
        assert_eq!(content, "keep this exact draft");
        assert_eq!(attachments, vec![attachment.clone()]);
        assert_eq!(mode, SubmitMode::FollowUp);
        assert_eq!(draft.content, "keep this exact draft");
        assert_eq!(draft.attachments, vec![attachment]);
        assert_eq!(draft.mode, SubmitMode::FollowUp);
    }

    #[test]
    fn ordered_control_queue_runs_one_at_a_time_in_fifo_order() {
        let mut queue = OrderedControlQueue::default();
        queue.enqueue(1);
        queue.enqueue(2);
        queue.enqueue(3);

        assert_eq!(queue.begin_next(), Some(1));
        assert_eq!(queue.begin_next(), None);
        queue.finish_current();
        assert_eq!(queue.begin_next(), Some(2));
        queue.finish_current();
        assert_eq!(queue.begin_next(), Some(3));
        queue.finish_current();
        assert_eq!(queue.begin_next(), None);
    }

    #[test]
    fn safety_and_no_scope_commands_never_wait_for_background_control() {
        let submit = AppCommand::SubmitPrompt {
            content: "controlled".into(),
            attachments: Vec::new(),
            mode: SubmitMode::Prompt,
        };
        assert!(requires_background_control(&submit, true));
        assert!(!requires_background_control(&submit, false));
        assert!(!requires_background_control(&AppCommand::Stop, true));
        assert!(!requires_background_control(&AppCommand::ClearQueue, true));
    }

    #[test]
    fn denied_control_completion_cannot_reach_the_handler() {
        let completion = resolve_controlled_command(Err(CommandHookError::Denied {
            hook_id: "deny-hook".into(),
            message: "private-hook-output-984d".into(),
        }));
        let ControlledCompletion::Denied(diagnostic) = completion else {
            panic!("a denied hook result must not retain an executable envelope");
        };
        assert_eq!(diagnostic.as_str(), "command denied by external hook");
        assert!(!diagnostic.as_str().contains("private-hook-output-984d"));
    }

    #[test]
    fn prompt_route_rejects_session_races_and_missing_context() {
        let expected = Uuid::new_v4();
        assert!(prompt_route_is_current(
            Some(expected),
            Some(expected),
            true
        ));
        assert!(!prompt_route_is_current(
            Some(expected),
            Some(Uuid::new_v4()),
            true
        ));
        assert!(!prompt_route_is_current(
            Some(expected),
            Some(expected),
            false
        ));
        assert!(!prompt_route_is_current(None, Some(expected), true));
    }
}
