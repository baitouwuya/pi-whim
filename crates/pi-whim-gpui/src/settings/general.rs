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
    AgentTeamConfig, AppState, BashPolicy, Language, QueueMode,
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
                vec![auto_compaction_row(state, tokens, emit)],
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
                    agent_team_apply(fields, state, emit),
                ],
            ),
        ]))
        .into_any_element()
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
