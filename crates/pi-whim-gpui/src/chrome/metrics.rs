//! What the visible session has cost so far.
//!
//! This lived in a full-width strip along the bottom of the window, which spent a
//! row of chrome and a border on four short figures. They ride in the title row
//! instead, beside the status pill: same information, no second frame edge, and
//! nothing cutting across the graph paper.

use gpui::{IntoElement, ParentElement, RenderOnce, Styled, Window, div, px};
use pi_whim_core::SessionMetrics;
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// Cost, tokens, and message count for the visible session.
#[derive(IntoElement)]
pub struct SessionMeter {
    metrics: SessionMetrics,
    tokens: Tokens,
}

impl SessionMeter {
    /// The meter for a session that has reported metrics.
    ///
    /// Returns `None` before the first turn: zeroes would read as a measurement
    /// rather than as an absence of one.
    pub fn from_metrics(metrics: Option<&SessionMetrics>, tokens: Tokens) -> Option<Self> {
        metrics.map(|metrics| Self {
            metrics: metrics.clone(),
            tokens,
        })
    }

    /// The figures, in reading order.
    fn segments(&self) -> Vec<String> {
        vec![
            format_cost(self.metrics.cost_microusd),
            format!("{} tok", format_tokens(self.metrics.total_tokens)),
            format!("{} msg", self.metrics.total_messages),
        ]
    }
}

/// Cost in dollars, from the microdollars Pi reports.
pub fn format_cost(cost_microusd: u64) -> String {
    format!("${:.4}", cost_microusd as f64 / 1_000_000.0)
}

/// Abbreviate a token count so the figure does not grow as a session does.
///
/// Long sessions reach millions of tokens, and the exact number is never what the
/// reader wants at a glance.
pub fn format_tokens(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{:.1}k", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

impl RenderOnce for SessionMeter {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let tokens = self.tokens;
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .font_family(pi_whim_theme::font::MONO)
            .text_size(px(text::LABEL_SIZE))
            .text_color(tokens.muted.hsla())
            .children(
                self.segments()
                    .into_iter()
                    .map(|segment| div().child(segment)),
            )
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
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_400_000), "2.4M");
    }

    #[test]
    fn a_session_that_has_reported_nothing_shows_no_meter() {
        // Zeroes would read as a measurement rather than as the absence of one.
        assert!(SessionMeter::from_metrics(None, Tokens::light()).is_none());
    }

    #[test]
    fn the_meter_reports_cost_tokens_and_messages() {
        let metrics = SessionMetrics {
            cost_microusd: 2_500_000,
            total_tokens: 15_500,
            total_messages: 7,
            ..Default::default()
        };
        let meter = SessionMeter::from_metrics(Some(&metrics), Tokens::light()).expect("a meter");
        assert_eq!(meter.segments(), vec!["$2.5000", "15.5k tok", "7 msg"]);
    }
}
