//! Bottom strip: which session is visible, and what it has cost so far.

use gpui::{IntoElement, ParentElement, RenderOnce, Styled, Window, div, px};
use pi_whim_core::{AppState, SessionMetrics};
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// A terminal-style line along the bottom of the window.
#[derive(IntoElement)]
pub struct StatusStrip {
    project: Option<String>,
    session: Option<String>,
    metrics: Option<SessionMetrics>,
    auto_compaction: bool,
    tokens: Tokens,
}

impl StatusStrip {
    pub fn from_state(state: &AppState, tokens: Tokens) -> Self {
        let project = state
            .selected_project
            .and_then(|id| state.projects.iter().find(|project| project.id == id))
            .map(|project| project.name.clone());
        let session = state
            .selected_session
            .and_then(|id| {
                state
                    .sessions
                    .values()
                    .flatten()
                    .find(|summary| summary.id == id)
            })
            .map(|summary| summary.title.clone());
        Self {
            project,
            session,
            metrics: state.session_metrics.clone(),
            auto_compaction: state.auto_compaction_enabled,
            tokens,
        }
    }
}

/// Cost in dollars, from the microdollars Pi reports.
pub fn format_cost(cost_microusd: u64) -> String {
    format!("${:.4}", cost_microusd as f64 / 1_000_000.0)
}

/// Abbreviate a token count so the strip does not grow as a session does.
///
/// Long sessions reach millions of tokens, and the exact figure is never what
/// the reader wants at a glance.
pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

impl RenderOnce for StatusStrip {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let tokens = self.tokens;
        let mut segments: Vec<String> = Vec::new();
        if let Some(project) = &self.project {
            segments.push(project.clone());
        }
        if let Some(session) = &self.session {
            segments.push(session.clone());
        }
        if let Some(metrics) = &self.metrics {
            segments.push(format_cost(metrics.cost_microusd));
            segments.push(format!("{} tok", format_tokens(metrics.total_tokens)));
            segments.push(format!("{} msg", metrics.total_messages));
        }
        segments.push(
            if self.auto_compaction {
                "auto-compact on"
            } else {
                "auto-compact off"
            }
            .to_owned(),
        );

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .w_full()
            .px(px(12.0))
            .py(px(4.0))
            .bg(tokens.panel_soft.hsla())
            .border_t_1()
            .border_color(tokens.line.hsla())
            .text_size(px(text::MONO_DETAIL_SIZE))
            .text_color(tokens.muted.hsla())
            .children(segments.into_iter().map(|segment| div().child(segment)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_keeps_four_decimals_so_cheap_turns_are_not_zero() {
        assert_eq!(format_cost(0), "$0.0000");
        assert_eq!(format_cost(1_234), "$0.0012");
        assert_eq!(format_cost(2_500_000), "$2.5000");
    }

    #[test]
    fn token_counts_are_abbreviated_past_a_thousand() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(15_500), "15.5k");
        assert_eq!(format_tokens(999_999), "1000.0k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_400_000), "2.4M");
    }

    #[test]
    fn an_empty_state_still_reports_the_compaction_setting() {
        // With no project or session there is nothing else to say, but the strip
        // should not render blank.
        let strip = StatusStrip::from_state(&AppState::default(), Tokens::light());
        assert!(strip.project.is_none());
        assert!(strip.session.is_none());
        assert!(strip.metrics.is_none());
    }
}
