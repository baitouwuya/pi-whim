//! The window, and the wiring between it and the orchestration.
//!
//! Split from [`crate::app`] because it is the only part that names a UI: the
//! orchestration reaches the view through `state()` / `apply()` and a queue of
//! requests, and this is where those two ends meet.
//!
//! There is no frame loop here. The egui build polled every session's channel and
//! every control refresh once a frame, whether or not anything had arrived, and
//! asked for a redraw every 50ms while a session was busy. Under gpui the arrivals
//! do the waking: [`pi_whim_gpui::pump`] blocks on a background thread and returns
//! to the main thread with each batch, so an idle app costs nothing.

use std::path::Path;

use gpui::{ClipboardItem, Context, Entity, IntoElement, PathPromptOptions, Render, Task, Window};
use pi_whim_core::{Attachment, SubmitMode};
use pi_whim_engine::commands::{AppCommand, ShellCommand, ShellPaste};
use pi_whim_gpui::{Request, RequestsRaised, Workspace, chat::Paste, pump};
use pi_whim_one_shot_ai::MAX_ONE_SHOT_INPUT_BYTES;
use pi_whim_persistence::session_title_context_from_jsonl;
use pi_whim_runtime::{AgentRuntime, test_search_engine};

use crate::app::{PiWhimApplication, Picker};

enum RequestRoute {
    App(AppCommand),
    Shell(ShellCommand),
    SubmitPrompt {
        content: String,
        attachments: Vec<Attachment>,
        mode: SubmitMode,
    },
    InsertPasteHandledByComposer,
}

fn adapt_request(request: Request) -> RequestRoute {
    match request {
        Request::AddProject => RequestRoute::Shell(ShellCommand::AddProject),
        Request::NewSession(project_id) => RequestRoute::App(AppCommand::NewSession(project_id)),
        Request::OpenProject(project_id) => RequestRoute::App(AppCommand::OpenProject(project_id)),
        Request::SetModel(model) => RequestRoute::App(AppCommand::SetModel(model)),
        Request::SetThinkingLevel(level) => RequestRoute::App(AppCommand::SetThinkingLevel(level)),
        Request::SetQueueModes {
            steering,
            follow_up,
        } => RequestRoute::App(AppCommand::SetQueueModes {
            steering,
            follow_up,
        }),
        Request::RunCommand(command) => RequestRoute::App(AppCommand::RunSlashCommand(command)),
        Request::RevealProject(project_id) => {
            RequestRoute::Shell(ShellCommand::RevealProject(project_id))
        }
        Request::RemoveProject(project_id) => {
            RequestRoute::App(AppCommand::RemoveProject(project_id))
        }
        Request::RenameSession { path, title } => {
            RequestRoute::App(AppCommand::RenameSession { path, title })
        }
        Request::SmartRenameSession {
            project_id,
            path,
            title,
        } => RequestRoute::Shell(ShellCommand::SmartRenameSession {
            project_id,
            path,
            title,
        }),
        Request::CloneSession => RequestRoute::App(AppCommand::CloneSession),
        Request::CopyToClipboard(text) => RequestRoute::Shell(ShellCommand::CopyToClipboard(text)),
        Request::DeleteSession(path) => RequestRoute::App(AppCommand::DeleteSession(path)),
        Request::AnswerPrompt(answer) => RequestRoute::App(AppCommand::AnswerPrompt(answer)),
        Request::AttachPaste(paste) => match shell_paste(paste) {
            Some(paste) => RequestRoute::Shell(ShellCommand::AttachPaste(paste)),
            None => RequestRoute::InsertPasteHandledByComposer,
        },
        Request::PickAttachments => RequestRoute::Shell(ShellCommand::PickAttachments),
        Request::DiscardAttachment(path) => RequestRoute::App(AppCommand::DiscardAttachment(path)),
        Request::SubmitPrompt {
            content,
            attachments,
            mode,
        } => RequestRoute::SubmitPrompt {
            content,
            attachments,
            mode,
        },
        Request::ActivateSession { project_id, path } => {
            RequestRoute::App(AppCommand::ActivateSession { project_id, path })
        }
        Request::Stop => RequestRoute::App(AppCommand::Stop),
        Request::ClearQueue => RequestRoute::App(AppCommand::ClearQueue),
        Request::SetLanguage(language) => RequestRoute::App(AppCommand::SetLanguage(language)),
        Request::SetBashPolicy(policy) => RequestRoute::App(AppCommand::SetBashPolicy(policy)),
        Request::SetBlockedPatterns(patterns) => {
            RequestRoute::App(AppCommand::SetBlockedPatterns(patterns))
        }
        Request::SetPermissionLevel(level) => {
            RequestRoute::App(AppCommand::SetPermissionLevel(level))
        }
        Request::SetAgentTeamConfig(config) => {
            RequestRoute::App(AppCommand::SetAgentTeamConfig(config))
        }
        Request::ApproveProjectHooks { fingerprint } => {
            RequestRoute::App(AppCommand::ApproveProjectHooks { fingerprint })
        }
        Request::RevokeProjectHooks => RequestRoute::App(AppCommand::RevokeProjectHooks),
        Request::SetOneShotAiConfig(config) => {
            RequestRoute::App(AppCommand::SetOneShotAiConfig(config))
        }
        Request::SetAutoCompaction(enabled) => {
            RequestRoute::App(AppCommand::SetAutoCompaction(enabled))
        }
        Request::SaveProvider { profile, api_key } => {
            RequestRoute::Shell(ShellCommand::SaveProvider { profile, api_key })
        }
        Request::DeleteProvider(profile_id) => {
            RequestRoute::App(AppCommand::DeleteProvider(profile_id))
        }
        Request::SaveSearchEngines(profiles) => {
            RequestRoute::App(AppCommand::SaveSearchEngines(profiles))
        }
        Request::SaveSearchEngine { profile, api_key } => {
            RequestRoute::Shell(ShellCommand::SaveSearchEngine { profile, api_key })
        }
        Request::TestSearchEngine {
            profile,
            api_key,
            editor,
        } => RequestRoute::Shell(ShellCommand::TestSearchEngine {
            profile,
            api_key,
            editor,
        }),
        Request::DiscoverProviderModels {
            profile_id,
            provider_name,
            base_url,
            protocol,
            api_key,
        } => RequestRoute::Shell(ShellCommand::DiscoverProviderModels {
            profile_id,
            provider_name,
            base_url,
            protocol,
            api_key,
        }),
    }
}

fn shell_paste(paste: Paste) -> Option<ShellPaste> {
    match paste {
        Paste::Insert => None,
        Paste::Files(paths) => Some(ShellPaste::Files(paths)),
        Paste::Image { extension, bytes } => Some(ShellPaste::Image { extension, bytes }),
        Paste::LongText(text) => Some(ShellPaste::LongText(text)),
    }
}

/// The window's view, and what it drives.
///
/// One entity holding both, rather than the application observing the shell: the
/// requests go one way and the state snapshots come back, and a single owner is
/// what keeps the order of those two unambiguous.
pub struct Host<R: AgentRuntime + 'static> {
    application: PiWhimApplication<R>,
    shell: Entity<Workspace>,
    /// The loops delivering session events, control answers, and the catalog.
    ///
    /// Held rather than detached, and never read: a [`Task`] cancels when dropped,
    /// so owning them here is what stops every pump when the window goes away.
    #[allow(dead_code, reason = "held for cancellation, not for reading")]
    pumps: Vec<Task<()>>,
}

impl<R: AgentRuntime + 'static> Host<R> {
    /// Bind `application` to `shell`, and start listening.
    pub fn new(
        application: PiWhimApplication<R>,
        shell: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Subscribed before the first push, so a request raised while the state is
        // still being seeded is not lost.
        cx.subscribe_in(&shell, window, |host, _, _: &RequestsRaised, window, cx| {
            host.drain(window, cx);
        })
        .detach();

        let pumps = vec![
            pump::spawn(
                application.session_events(),
                window,
                cx,
                |host, batch, window, cx| {
                    host.application.handle_deliveries(batch);
                    host.publish(window, cx);
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
                    host.publish(window, cx);
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
                    host.publish(window, cx);
                },
            ),
            pump::spawn(
                application.one_shot_completions(),
                window,
                cx,
                |host, batch, window, cx| {
                    host.application.settle_one_shot_completions(batch);
                    host.publish(window, cx);
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
                    host.publish(window, cx);
                },
            ),
        ];

        let mut host = Self {
            application,
            shell,
            pumps,
        };
        host.publish(window, cx);
        host
    }

    /// Show what the reducer now holds.
    ///
    /// Called after anything that could have applied an action. A snapshot per
    /// batch rather than per action: a streaming turn applies several for one
    /// visible change, and the shell's `set_state` compares before syncing.
    fn publish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.application.state().clone();
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
            shell.set_state(state, window, cx);
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
        // The shell may have raised requests while being told any of that — a
        // cleared draft, a dismissed prompt — so drain before yielding.
        self.drain_once(window, cx);
    }

    /// Carry out everything the shell has asked for, then show the result.
    fn drain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drain_once(window, cx);
        self.publish(window, cx);
    }

    /// Carry out what has been asked for without publishing.
    ///
    /// Separate from [`Self::drain`] so `publish` can use it without recursing:
    /// handling a request can produce another, and a request that publishes which
    /// drains which publishes would not terminate.
    fn drain_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let requests = self.shell.update(cx, |shell, _| shell.take_requests());
        for request in requests {
            self.handle(request, window, cx);
        }
    }

    /// Carry out one request.
    ///
    /// Most go straight to the orchestration, which owns the store, the pool, and
    /// the keychain. The few that stay here are the ones whose answer is a change
    /// to the view rather than to the domain.
    fn handle(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        match adapt_request(request) {
            RequestRoute::App(command) => self.application.handle(command),
            RequestRoute::Shell(command) => self.handle_shell(command, window, cx),
            RequestRoute::SubmitPrompt {
                content,
                attachments,
                mode,
            } => {
                // The composer clears only after its local readiness check, but
                // a session can disappear before this queued request reaches the
                // host. Restore the exact draft if that race rejects the turn.
                if !self.application.can_submit_prompt() {
                    self.application.report_submission_unavailable();
                    self.shell.update(cx, |shell, cx| {
                        shell.restore_submission(content, attachments, window, cx);
                    });
                } else {
                    self.application.handle(AppCommand::SubmitPrompt {
                        content,
                        attachments,
                        mode,
                    });
                }
            }
            RequestRoute::InsertPasteHandledByComposer => {}
        }
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
                        host.publish(window, cx);
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
                                host.publish(window, cx);
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
                host.publish(window, cx);
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
    use pi_whim_core::{AttachmentKind, ProviderProfile, ProviderProtocol, SubmitMode};
    use pi_whim_engine::{commands::CommandControlPolicy, slash_commands::SlashCommand};
    use uuid::Uuid;

    #[test]
    fn domain_adapter_preserves_typed_and_safety_commands() {
        for request in [
            Request::Stop,
            Request::ClearQueue,
            Request::RevokeProjectHooks,
        ] {
            let RequestRoute::App(command) = adapt_request(request) else {
                panic!("safety request must map to AppCommand");
            };
            assert!(command.is_safety_command());
            assert_eq!(command.control_policy(), CommandControlPolicy::Bypass);
        }

        let RequestRoute::App(AppCommand::RunSlashCommand(command)) =
            adapt_request(Request::RunCommand(SlashCommand::ShowHotkeys))
        else {
            panic!("slash command must map to RunSlashCommand");
        };
        assert_eq!(command, SlashCommand::ShowHotkeys);
    }

    #[test]
    fn submit_prompt_adapter_preserves_draft_for_host_readiness_check() {
        let attachment = Attachment {
            name: "notes.txt".into(),
            path: "/tmp/notes.txt".into(),
            kind: AttachmentKind::File,
            generated_by_app: false,
        };
        let RequestRoute::SubmitPrompt {
            content,
            attachments,
            mode,
        } = adapt_request(Request::SubmitPrompt {
            content: "keep this exact draft".into(),
            attachments: vec![attachment.clone()],
            mode: SubmitMode::FollowUp,
        })
        else {
            panic!("submit prompt must retain the host readiness seam");
        };
        assert_eq!(content, "keep this exact draft");
        assert_eq!(attachments, vec![attachment]);
        assert_eq!(mode, SubmitMode::FollowUp);
    }

    #[test]
    fn shell_paste_conversion_is_lossless_and_debug_is_redacted() {
        let bytes = vec![0, 1, 2, 254, 255];
        let image = shell_paste(Paste::Image {
            extension: "png".into(),
            bytes: bytes.clone(),
        });
        assert_eq!(
            image,
            Some(ShellPaste::Image {
                extension: "png".into(),
                bytes,
            })
        );
        assert_eq!(
            shell_paste(Paste::Files(vec!["/tmp/a".into(), "/tmp/b".into()])),
            Some(ShellPaste::Files(vec!["/tmp/a".into(), "/tmp/b".into()]))
        );
        assert_eq!(
            shell_paste(Paste::LongText("paste-secret-99c1".into())),
            Some(ShellPaste::LongText("paste-secret-99c1".into()))
        );
        assert_eq!(shell_paste(Paste::Insert), None);

        let command = ShellCommand::SaveProvider {
            profile: ProviderProfile {
                id: Uuid::nil(),
                name: "provider".into(),
                base_url: "https://private.invalid".into(),
                protocol: ProviderProtocol::OpenAiCompletions,
                models: Vec::new(),
                updated_at_ms: 0,
                has_api_key: true,
            },
            api_key: Some("api-key-secret-70af".into()),
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("shell.provider.save"));
        assert!(!debug.contains("api-key-secret-70af"));
        assert!(!debug.contains("private.invalid"));

        let clipboard = ShellCommand::CopyToClipboard("clipboard-secret-51de".into());
        assert!(!format!("{clipboard:?}").contains("clipboard-secret-51de"));
        assert!(
            !format!("{:?}", ShellPaste::LongText("paste-secret-99c1".into()))
                .contains("paste-secret-99c1")
        );
        assert!(
            !format!(
                "{:?}",
                ShellPaste::Files(vec!["/private/secret.txt".into()])
            )
            .contains("/private/secret.txt")
        );
    }
}
