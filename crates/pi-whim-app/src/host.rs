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

use gpui::{ClipboardItem, Context, Entity, IntoElement, Render, Task, Window};
use pi_whim_gpui::{Request, RequestsRaised, Workspace, pump};
use pi_whim_runtime::AgentRuntime;

use crate::app::PiWhimApplication;

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
        match request {
            Request::DiscoverProviderModels {
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
            Request::SaveProvider { profile, api_key } => {
                if let Some((id, key_saved)) = self.application.save_provider(profile, api_key) {
                    self.shell.update(cx, |shell, cx| {
                        shell.provider_saved(id, key_saved, window, cx);
                    });
                }
            }
            Request::AttachPaste(paste) => {
                // A paste of several files is several attachments, so this is a
                // list even though most pastes produce one.
                for attachment in self.application.attachments_for(paste) {
                    self.shell
                        .update(cx, |shell, cx| shell.attach(attachment, cx));
                }
            }
            Request::CopyToClipboard(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            other => self.application.handle(other),
        }
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
