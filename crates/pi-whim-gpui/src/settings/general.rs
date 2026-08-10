//! General and execution settings over one shared set of persistent fields.
//!
//! Every row reads from `AppState`. Values that restart Pi stay in their input
//! fields until an explicit Apply, so editing cannot abort a turn in flight.

use gpui::{AnyElement, Entity, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputState, NumberInput},
};
use pi_whim_core::{
    AgentTeamConfig, AppState, BashPolicy, Language, ProjectHookStatus, QueueMode,
    strings::{text as translate, tr},
};
use pi_whim_theme::Tokens;

use crate::elements::isolated_vertical_wheel_region;
use crate::settings::{
    Emit, SettingsEvent,
    dropdown::{self, Choice, ChoiceState},
    form, toggle,
};

pub(super) const BLOCKED_PATTERN_ROWS: usize = 3;

/// The typed fields on this page.
///
/// Held by the settings view because a `NumberInput` and a text field both need
/// an `InputState` that survives between frames.
pub struct Fields {
    /// Shell patterns to refuse, one per line.
    pub blocked_patterns: Entity<InputState>,
    pub max_depth: Entity<InputState>,
    pub max_agents_per_level: Entity<InputState>,
    pub language: Entity<ChoiceState<Language>>,
    pub bash_policy: Entity<ChoiceState<BashPolicy>>,
    pub steering_mode: Entity<ChoiceState<QueueMode>>,
    pub follow_up_mode: Entity<ChoiceState<QueueMode>>,
}

/// Build the General page: app-wide presentation and conversation behavior.
pub fn render_general(state: &AppState, fields: &Fields, tokens: Tokens, emit: Emit) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(form::page_header(tr(state, "general"), None, tokens))
        .child(form::group_stack(vec![
            form::group(
                tr(state, "appearance"),
                None,
                tokens,
                vec![language_row(state, fields, tokens)],
            ),
            form::group(
                tr(state, "context"),
                Some(tr(state, "context-help")),
                tokens,
                vec![auto_compaction_row(state, tokens, emit.clone())],
            ),
            form::group(
                tr(state, "queue-mode"),
                None,
                tokens,
                vec![
                    steering_row(state, fields, tokens),
                    follow_up_row(state, fields, tokens),
                ],
            ),
        ]))
        .into_any_element()
}

/// Build the Execution page: local tool policy and agent-team limits.
pub fn render_execution(
    state: &AppState,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(form::page_header(tr(state, "execution"), None, tokens))
        .child(form::group_stack(vec![
            form::group(
                tr(state, "shell"),
                None,
                tokens,
                vec![
                    bash_policy_row(state, fields, tokens),
                    form::row(
                        tr(state, "blocked-patterns"),
                        Some(tr(state, "blocked-patterns-help")),
                        tokens,
                        blocked_patterns_control(fields, emit.clone(), state.language),
                    ),
                ],
            ),
            form::group(
                tr(state, "agent-team"),
                Some(tr(state, "agent-team-help")),
                tokens,
                vec![
                    form::row(
                        tr(state, "max-agent-depth"),
                        None,
                        tokens,
                        numeric(&fields.max_depth),
                    ),
                    form::row(
                        tr(state, "max-agents-per-level"),
                        None,
                        tokens,
                        numeric(&fields.max_agents_per_level),
                    ),
                    agent_team_apply(fields, state, emit.clone()),
                ],
            ),
            form::group(
                tr(state, "project-hooks"),
                None,
                tokens,
                project_hook_rows(state, tokens, emit),
            ),
        ]))
        .into_any_element()
}

fn project_hook_rows(state: &AppState, tokens: Tokens, emit: Emit) -> Vec<AnyElement> {
    let mut rows = vec![project_hooks_row(state, tokens, emit)];
    let grants = match &state.project_hook_status {
        ProjectHookStatus::ApprovalRequired { grants, .. }
        | ProjectHookStatus::Approved { grants, .. } => grants.as_slice(),
        ProjectHookStatus::NotPresent | ProjectHookStatus::Invalid(_) => &[],
    };
    rows.extend(grants.iter().map(|grant| {
        form::row(
            compact_display(&grant.hook_id, 28),
            Some(&grant_details(grant)),
            tokens,
            div().into_any_element(),
        )
    }));
    rows.extend(state.hook_audit.iter().take(5).map(|entry| {
        let revision = entry.revision.get(..19).unwrap_or(&entry.revision);
        let truncation = if entry.output_truncated {
            " truncated"
        } else {
            ""
        };
        let details = format!(
            "{} / {} / {} ms{truncation} / {revision}",
            entry.event, entry.outcome, entry.duration_ms
        );
        form::row(
            compact_display(&entry.hook_id, 28),
            Some(&details),
            tokens,
            div().into_any_element(),
        )
    }));
    rows
}

fn project_hooks_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let (status, action) = match &state.project_hook_status {
        ProjectHookStatus::NotPresent => (tr(state, "project-hooks-none").to_owned(), None),
        ProjectHookStatus::Invalid(error) => (
            compact_display(
                &format!("{}: {error}", tr(state, "project-hooks-invalid")),
                36,
            ),
            None,
        ),
        ProjectHookStatus::ApprovalRequired {
            fingerprint,
            grants_hash,
            grants,
        } => {
            let fingerprint_for_click = fingerprint.clone();
            let grants_hash_for_click = grants_hash.clone();
            let emit_for_click = emit.clone();
            let button = Button::new("approve-project-hooks")
                .primary()
                .small()
                .label(tr(state, "approve"))
                .on_click(move |_, window, cx| {
                    emit_for_click(
                        SettingsEvent::ApproveProjectHooks {
                            fingerprint: fingerprint_for_click.clone(),
                            grants_hash: grants_hash_for_click.clone(),
                        },
                        window,
                        cx,
                    );
                });
            (
                format!(
                    "{}: {} ({} / {})",
                    tr(state, "project-hooks-approval"),
                    grants.len(),
                    fingerprint.get(..12).unwrap_or(fingerprint),
                    grants_hash.get(..12).unwrap_or(grants_hash)
                ),
                Some(button.into_any_element()),
            )
        }
        ProjectHookStatus::Approved {
            fingerprint,
            grants_hash,
            grants,
        } => {
            let button = Button::new("revoke-project-hooks")
                .small()
                .label(tr(state, "revoke"))
                .on_click(move |_, window, cx| {
                    emit(SettingsEvent::RevokeProjectHooks, window, cx);
                });
            (
                format!(
                    "{}: {} ({} / {})",
                    tr(state, "project-hooks-approved"),
                    grants.len(),
                    fingerprint.get(..12).unwrap_or(fingerprint),
                    grants_hash.get(..12).unwrap_or(grants_hash)
                ),
                Some(button.into_any_element()),
            )
        }
    };
    form::row(
        tr(state, "project-hooks"),
        Some(&status),
        tokens,
        action.unwrap_or_else(|| div().into_any_element()),
    )
}

fn grant_details(grant: &pi_whim_core::HookGrantDescriptor) -> String {
    let fields = if grant.fields.is_empty() {
        "-".to_owned()
    } else {
        grant.fields.join(",")
    };
    let matcher = serde_json::to_string(&grant.matcher).unwrap_or_else(|_| "{}".into());
    format!(
        "{} / {:?} / fields=[{}] / matcher={} / delivery={:?}:{} / restart={}:{}-{} / sha256={}",
        grant.event,
        grant.kind,
        fields,
        matcher,
        grant.delivery.mode,
        grant.delivery.capacity,
        grant.restart.max_restarts,
        grant.restart.initial_backoff_ms,
        grant.restart.max_backoff_ms,
        grant
            .entrypoint_sha256
            .get(..12)
            .unwrap_or(&grant.entrypoint_sha256)
    )
}

fn compact_display(value: &str, maximum_chars: usize) -> String {
    let mut characters = value.chars();
    let prefix = characters.by_ref().take(maximum_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

/// Keep destructive launch settings as a draft until the reader applies them.
///
/// Changing a character must not restart Pi and abort the current turn; only the
/// explicit button below crosses the application boundary.
fn blocked_patterns_control(fields: &Fields, emit: Emit, language: Language) -> AnyElement {
    let field = fields.blocked_patterns.clone();
    let overflow_field = field.clone();
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            isolated_vertical_wheel_region("blocked-patterns-scroll", move |cx| {
                overflow_field.read(cx).value().split('\n').count() > BLOCKED_PATTERN_ROWS
            })
            .child(Input::new(&field).w_full().h(px(72.0))),
        )
        .child(
            div().flex().justify_end().child(
                Button::new("apply-blocked-patterns")
                    .primary()
                    .small()
                    .label(translate("apply", language))
                    .on_click(move |_, window: &mut Window, cx| {
                        let patterns = parse_blocked_patterns(&field.read(cx).value());
                        emit(SettingsEvent::SetBlockedPatterns(patterns), window, cx);
                    }),
            ),
        )
        .into_any_element()
}

fn agent_team_apply(fields: &Fields, state: &AppState, emit: Emit) -> AnyElement {
    let max_depth = fields.max_depth.clone();
    let max_agents = fields.max_agents_per_level.clone();
    let current = state.agent_team_config.clone();
    let language = state.language;
    form::control_row(
        Button::new("apply-agent-team")
            .primary()
            .small()
            .label(translate("apply", language))
            .on_click(move |_, window, cx| {
                let depth = max_depth
                    .read(cx)
                    .value()
                    .parse::<u8>()
                    .unwrap_or(current.max_depth);
                let agents = max_agents
                    .read(cx)
                    .value()
                    .parse::<u16>()
                    .unwrap_or(current.max_agents_per_level);
                let config = AgentTeamConfig {
                    max_depth: depth,
                    max_agents_per_level: agents,
                    ..current.clone()
                }
                .normalized();
                emit(SettingsEvent::SetAgentTeamConfig(config), window, cx);
            }),
    )
}

/// A number field, narrower than a text field because the values are single
/// digits and a 320px box for "8" reads as the wrong control.
fn numeric(state: &Entity<InputState>) -> AnyElement {
    div()
        .w(px(120.0))
        .child(NumberInput::new(state))
        .into_any_element()
}

fn language_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "language"),
        None,
        tokens,
        dropdown::dropdown(&fields.language),
    )
}

fn auto_compaction_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let enabled = state.auto_compaction_enabled;
    form::row(
        tr(state, "auto-compaction"),
        Some(tr(state, "auto-compaction-help")),
        tokens,
        toggle::toggle(
            "auto-compaction",
            tr(state, "auto-compaction"),
            enabled,
            tokens,
            move |checked, window, cx| emit(SettingsEvent::SetAutoCompaction(checked), window, cx),
        ),
    )
}

fn bash_policy_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "bash-policy"),
        Some(tr(state, "bash-help")),
        tokens,
        dropdown::dropdown(&fields.bash_policy),
    )
}

fn steering_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "steer-mode"),
        None,
        tokens,
        dropdown::dropdown(&fields.steering_mode),
    )
}

fn follow_up_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "follow-up-mode"),
        None,
        tokens,
        dropdown::dropdown(&fields.follow_up_mode),
    )
}

pub fn language_choices() -> Vec<Choice<Language>> {
    vec![
        Choice::new(Language::English, "English"),
        // Named in its own language so the picker stays usable after an
        // accidental language change.
        Choice::new(Language::SimplifiedChinese, "简体中文"),
    ]
}

pub fn bash_policy_choices(state: &AppState) -> Vec<Choice<BashPolicy>> {
    vec![
        Choice::new(BashPolicy::Ask, tr(state, "bash-ask")),
        Choice::new(BashPolicy::Allow, tr(state, "bash-allow")),
        Choice::new(BashPolicy::Deny, tr(state, "bash-deny")),
    ]
}

pub fn queue_mode_choices(state: &AppState) -> Vec<Choice<QueueMode>> {
    vec![
        Choice::new(QueueMode::OneAtATime, tr(state, "one-at-a-time")),
        Choice::new(QueueMode::All, tr(state, "all")),
    ]
}

/// Split the blocked-pattern field's text into patterns.
///
/// One per line, blanks dropped. A trailing newline is normal while typing, and
/// storing it as an empty pattern would match either everything or nothing
/// depending on how it is compared — neither is what was meant.
pub fn parse_blocked_patterns(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Join stored patterns back into the field's text.
pub fn format_blocked_patterns(patterns: &[String]) -> String {
    patterns.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_hook_grant_details_show_every_authority_dimension() {
        let grant = pi_whim_core::HookGrantDescriptor {
            hook_id: "policy".into(),
            event: "pi.ui.command.submitting".into(),
            kind: pi_whim_core::HookGrantKind::Transform,
            fields: vec!["arguments".into()],
            matcher: pi_whim_core::HookGrantMatcher {
                tools: Vec::new(),
                agent_levels: Vec::new(),
                metadata: std::collections::BTreeMap::from([(
                    "source".into(),
                    serde_json::json!("ui"),
                )]),
            },
            delivery: pi_whim_core::HookGrantDelivery {
                mode: pi_whim_core::HookGrantDeliveryMode::RequestResponse,
                capacity: 1,
            },
            restart: pi_whim_core::HookGrantRestart {
                max_restarts: 2,
                initial_backoff_ms: 250,
                max_backoff_ms: 1_000,
            },
            entrypoint_sha256: "0123456789abcdef".into(),
        };
        let details = grant_details(&grant);
        for expected in [
            "pi.ui.command.submitting",
            "Transform",
            "arguments",
            "source",
            "RequestResponse:1",
            "restart=2:250-1000",
            "0123456789ab",
        ] {
            assert!(details.contains(expected), "missing {expected}: {details}");
        }
    }

    #[test]
    fn patterns_are_one_per_line() {
        assert_eq!(
            parse_blocked_patterns("rm -rf\ncurl | sh"),
            vec!["rm -rf".to_owned(), "curl | sh".to_owned()]
        );
    }

    #[test]
    fn blank_lines_are_dropped() {
        // A trailing newline is normal while typing, and an empty pattern would
        // match either everything or nothing.
        assert_eq!(
            parse_blocked_patterns("rm -rf\n\n  \n"),
            vec!["rm -rf".to_owned()]
        );
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        // A pattern with a trailing space silently fails to match.
        assert_eq!(
            parse_blocked_patterns("  rm -rf  "),
            vec!["rm -rf".to_owned()]
        );
    }

    #[test]
    fn an_empty_field_blocks_nothing() {
        assert!(parse_blocked_patterns("").is_empty());
        assert!(parse_blocked_patterns("\n \n").is_empty());
    }

    #[test]
    fn formatting_and_parsing_round_trip() {
        let patterns = vec!["rm -rf".to_owned(), "curl | sh".to_owned()];

        assert_eq!(
            parse_blocked_patterns(&format_blocked_patterns(&patterns)),
            patterns
        );
    }

    #[test]
    fn inner_spacing_within_a_pattern_survives() {
        // The pattern is matched against a command line, where spacing matters.
        assert_eq!(
            parse_blocked_patterns("rm  -rf  /"),
            vec!["rm  -rf  /".to_owned()]
        );
    }

    #[test]
    fn hook_labels_are_bounded_without_splitting_unicode() {
        assert_eq!(compact_display("short-hook", 28), "short-hook");
        assert_eq!(compact_display("abcdef", 3), "abc...");
        assert_eq!(compact_display("项目策略钩子", 4), "项目策略...");
    }
}
