//! General settings: language, context, shell policy, agent team, queue modes.
//!
//! Every row reads from `AppState`. Values that restart Pi stay in their input
//! fields until an explicit Apply, so editing cannot abort a turn in flight.

use gpui::{AnyElement, Entity, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState, NumberInput},
};
use pi_whim_core::{
    AgentTeamConfig, AppState, BashPolicy, Language, QueueMode,
    strings::{text as translate, tr},
};
use pi_whim_theme::Tokens;

use crate::settings::{
    Emit, SettingsEvent, form,
    segmented::{Segment, segmented},
};

/// The typed fields on this page.
///
/// Held by the settings view because a `NumberInput` and a text field both need
/// an `InputState` that survives between frames.
pub struct Fields {
    /// Shell patterns to refuse, one per line.
    pub blocked_patterns: Entity<InputState>,
    pub max_depth: Entity<InputState>,
    pub max_agents_per_level: Entity<InputState>,
}

/// Build the General page.
pub fn render(state: &AppState, fields: &Fields, tokens: Tokens, emit: Emit) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(form::page_header(tr(state, "general"), None, tokens))
        .child(form::section_header(tr(state, "appearance"), None, tokens))
        .child(language_row(state, tokens, emit.clone()))
        .child(form::section_header(
            tr(state, "context"),
            Some(tr(state, "context-help")),
            tokens,
        ))
        .child(auto_compaction_row(state, tokens, emit.clone()))
        .child(form::section_header(tr(state, "shell"), None, tokens))
        .child(bash_policy_row(state, tokens, emit.clone()))
        .child(form::row(
            tr(state, "blocked-patterns"),
            Some(tr(state, "blocked-patterns-help")),
            tokens,
            blocked_patterns_control(fields, emit.clone(), state.language),
        ))
        .child(form::section_header(
            tr(state, "agent-team"),
            Some(tr(state, "agent-team-help")),
            tokens,
        ))
        .child(form::row(
            tr(state, "max-agent-depth"),
            None,
            tokens,
            numeric(&fields.max_depth),
        ))
        .child(form::row(
            tr(state, "max-agents-per-level"),
            None,
            tokens,
            numeric(&fields.max_agents_per_level),
        ))
        .child(agent_team_apply(fields, state, emit.clone()))
        .child(form::section_header(tr(state, "queue-mode"), None, tokens))
        .child(steering_row(state, tokens, emit.clone()))
        .child(follow_up_row(state, tokens, emit))
        .into_any_element()
}

/// Keep destructive launch settings as a draft until the reader applies them.
///
/// Changing a character must not restart Pi and abort the current turn; only the
/// explicit button below crosses the application boundary.
fn blocked_patterns_control(fields: &Fields, emit: Emit, language: Language) -> AnyElement {
    let field = fields.blocked_patterns.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(Input::new(&field))
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

fn language_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    form::row(
        tr(state, "language"),
        None,
        tokens,
        segmented(
            "language",
            state.language,
            vec![
                Segment::new(Language::English, "English"),
                // Named in its own language: a reader who cannot read the current
                // one still has to be able to find their own.
                Segment::new(Language::SimplifiedChinese, "简体中文"),
            ],
            tokens,
            move |language, window, cx| emit(SettingsEvent::SetLanguage(language), window, cx),
        ),
    )
}

fn auto_compaction_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let enabled = state.auto_compaction_enabled;
    form::row(
        tr(state, "auto-compaction"),
        Some(tr(state, "auto-compaction-help")),
        tokens,
        Checkbox::new("auto-compaction")
            .label(tr(state, "auto-compaction"))
            .checked(enabled)
            .on_click(move |_, window, cx| {
                emit(SettingsEvent::SetAutoCompaction(!enabled), window, cx)
            }),
    )
}

fn bash_policy_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    form::row(
        tr(state, "bash-policy"),
        Some(tr(state, "bash-help")),
        tokens,
        segmented(
            "bash-policy",
            state.bash_policy,
            vec![
                Segment::new(BashPolicy::Ask, tr(state, "bash-ask")),
                Segment::new(BashPolicy::Allow, tr(state, "bash-allow")),
                Segment::new(BashPolicy::Deny, tr(state, "bash-deny")),
            ],
            tokens,
            move |policy, window, cx| emit(SettingsEvent::SetBashPolicy(policy), window, cx),
        ),
    )
}

/// The two queue-mode rows are one control each, but each carries the other's
/// current value: the domain action sets both at once.
fn steering_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let follow_up = state.follow_up_mode;
    form::row(
        tr(state, "steer-mode"),
        None,
        tokens,
        queue_segments(
            "steering-mode",
            state,
            state.steering_mode,
            tokens,
            move |steering, window, cx| {
                emit(
                    SettingsEvent::SetQueueModes {
                        steering,
                        follow_up,
                    },
                    window,
                    cx,
                )
            },
        ),
    )
}

fn follow_up_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let steering = state.steering_mode;
    form::row(
        tr(state, "follow-up-mode"),
        None,
        tokens,
        queue_segments(
            "follow-up-mode",
            state,
            state.follow_up_mode,
            tokens,
            move |follow_up, window, cx| {
                emit(
                    SettingsEvent::SetQueueModes {
                        steering,
                        follow_up,
                    },
                    window,
                    cx,
                )
            },
        ),
    )
}

fn queue_segments(
    id: &'static str,
    state: &AppState,
    current: QueueMode,
    tokens: Tokens,
    on_pick: impl Fn(QueueMode, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
) -> AnyElement {
    segmented(
        id,
        current,
        vec![
            Segment::new(QueueMode::OneAtATime, tr(state, "one-at-a-time")),
            Segment::new(QueueMode::All, tr(state, "all")),
        ],
        tokens,
        on_pick,
    )
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
}
