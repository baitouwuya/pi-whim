//! Progressive reveal of streaming assistant text.
//!
//! Pi delivers assistant text in bursts. Rendering each burst the instant it
//! arrives reads as stuttering, so the view reveals text at a steadier rate
//! and catches up when it falls behind.
//!
//! This was two fields on `ConversationItem` — `revealed_graphemes` and
//! `reveal_credit` — plus three methods on it, one of which took a frame
//! duration. A domain type carrying per-frame animation state meant every
//! construction site had to hand-fill `revealed_graphemes: 0, reveal_credit:
//! 0.0`, seven times in the app crate alone. Here the progress lives beside the
//! conversation, keyed by message id, and the domain type is just the message.

use std::collections::HashMap;

use pi_whim_core::{ConversationItem, grapheme_prefix};
use unicode_segmentation::UnicodeSegmentation;

use crate::thinking::trim_incomplete_tag;

/// Graphemes per second once the backlog is large enough that the reveal would
/// otherwise fall visibly behind the stream.
const CATCH_UP_SPEED: f32 = 240.0;
/// Backlog, in graphemes, past which the catch-up speed applies.
const CATCH_UP_BACKLOG: usize = 180;
/// Resting reveal speed, in graphemes per second.
const BASE_SPEED: f32 = 45.0;
/// How much the resting speed scales with the current backlog, and the ceiling
/// on that contribution.
const BACKLOG_SCALE: f32 = 0.45;
const BACKLOG_BONUS_CEILING: f32 = 95.0;

/// How much of each streaming message has been revealed so far.
#[derive(Debug, Default)]
pub struct Typewriter {
    progress: HashMap<String, Progress>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Progress {
    revealed: usize,
    /// Fractional graphemes carried between frames, so a slow reveal is not
    /// rounded away to nothing each frame.
    credit: f32,
}

impl Typewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The portion of `message` to display.
    ///
    /// Finished messages render in full; only streaming text is withheld.
    pub fn visible_text<'a>(&self, message: &'a ConversationItem) -> &'a str {
        if !message.streaming {
            return &message.full_text;
        }
        let revealed = self
            .progress
            .get(&message.id)
            .map_or(0, |progress| progress.revealed);
        trim_incomplete_tag(
            grapheme_prefix(&message.full_text, revealed),
            &message.full_text,
        )
    }

    /// Advance every streaming message by `elapsed_seconds`.
    ///
    /// Returns whether anything became visible, so the caller can skip a
    /// redraw when nothing moved.
    pub fn advance(&mut self, messages: &[ConversationItem], elapsed_seconds: f32) -> bool {
        let mut changed = false;
        for message in messages.iter().filter(|message| message.streaming) {
            let total = message.full_text.graphemes(true).count();
            let progress = self.progress.entry(message.id.clone()).or_default();

            // Text can shrink if a message is replaced mid-stream.
            progress.revealed = progress.revealed.min(total);

            let backlog = total.saturating_sub(progress.revealed);
            let speed = if backlog > CATCH_UP_BACKLOG {
                CATCH_UP_SPEED
            } else {
                BASE_SPEED + (backlog as f32 * BACKLOG_SCALE).min(BACKLOG_BONUS_CEILING)
            };

            progress.credit += elapsed_seconds * speed;
            let advance = progress.credit.floor() as usize;
            progress.credit -= advance as f32;

            let next = (progress.revealed + advance).min(total);
            changed |= next != progress.revealed;
            progress.revealed = next;
        }
        changed
    }

    /// Reveal `message` in full, for when the reader asks to skip ahead.
    pub fn reveal_all(&mut self, message: &ConversationItem) {
        let progress = self.progress.entry(message.id.clone()).or_default();
        progress.revealed = message.full_text.graphemes(true).count();
        progress.credit = 0.0;
    }

    /// Forget a message's progress. Call when the conversation resets, so the
    /// map does not grow for the life of the process.
    pub fn forget(&mut self, id: &str) {
        self.progress.remove(id);
    }

    /// Move progress to a new id, for when a streaming message is re-keyed once
    /// Pi reports its real id.
    pub fn rekey(&mut self, from: &str, to: &str) {
        if let Some(progress) = self.progress.remove(from) {
            self.progress.insert(to.to_owned(), progress);
        }
    }

    pub fn clear(&mut self) {
        self.progress.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::ConversationRole;

    fn streaming(id: &str, text: &str) -> ConversationItem {
        ConversationItem {
            id: id.into(),
            role: ConversationRole::Assistant,
            full_text: text.into(),
            streaming: true,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn finished_messages_render_in_full_without_any_progress_recorded() {
        let mut message = streaming("a", "hello");
        message.streaming = false;

        let typewriter = Typewriter::new();
        assert_eq!(typewriter.visible_text(&message), "hello");
    }

    #[test]
    fn streaming_text_starts_hidden_and_is_revealed_over_time() {
        let message = streaming("a", "hello");
        let mut typewriter = Typewriter::new();

        assert_eq!(typewriter.visible_text(&message), "");

        assert!(typewriter.advance(std::slice::from_ref(&message), 0.1));
        assert!(!typewriter.visible_text(&message).is_empty());
    }

    #[test]
    fn a_long_backlog_reveals_faster_than_a_short_one() {
        let short = streaming("short", "hello");
        let long = streaming("long", &"x".repeat(500));

        let mut typewriter = Typewriter::new();
        typewriter.advance(&[short.clone(), long.clone()], 0.1);

        assert!(
            typewriter.visible_text(&long).len() > typewriter.visible_text(&short).len(),
            "catch-up speed should apply once the backlog is large"
        );
    }

    #[test]
    fn revealing_never_runs_past_the_available_text() {
        let message = streaming("a", "hi");
        let mut typewriter = Typewriter::new();

        // Far more time than the text needs.
        typewriter.advance(std::slice::from_ref(&message), 10.0);

        assert_eq!(typewriter.visible_text(&message), "hi");
        // And a further advance reports no change.
        assert!(!typewriter.advance(std::slice::from_ref(&message), 10.0));
    }

    #[test]
    fn fractional_progress_carries_between_frames() {
        let message = streaming("a", "hello world");
        let mut typewriter = Typewriter::new();

        // Each step is far too short to reveal a whole grapheme on its own; the
        // carried credit is what makes them add up.
        for _ in 0..40 {
            typewriter.advance(std::slice::from_ref(&message), 0.001);
        }

        assert!(!typewriter.visible_text(&message).is_empty());
    }

    #[test]
    fn skipping_ahead_reveals_everything() {
        let message = streaming("a", "hello");
        let mut typewriter = Typewriter::new();

        typewriter.reveal_all(&message);

        assert_eq!(typewriter.visible_text(&message), "hello");
    }

    #[test]
    fn progress_survives_a_rekey() {
        let draft = streaming("draft", "hello");
        let mut typewriter = Typewriter::new();
        typewriter.reveal_all(&draft);

        typewriter.rekey("draft", "real");

        let renamed = streaming("real", "hello");
        assert_eq!(typewriter.visible_text(&renamed), "hello");
    }

    #[test]
    fn shrinking_text_clamps_revealed_progress() {
        let long = streaming("a", "hello world");
        let mut typewriter = Typewriter::new();
        typewriter.reveal_all(&long);

        // The message is replaced mid-stream by something shorter.
        let short = streaming("a", "hi");
        typewriter.advance(std::slice::from_ref(&short), 0.0);

        assert_eq!(typewriter.visible_text(&short), "hi");
    }

    #[test]
    fn forgetting_and_clearing_drop_progress() {
        let message = streaming("a", "hello");
        let mut typewriter = Typewriter::new();

        typewriter.reveal_all(&message);
        typewriter.forget("a");
        assert_eq!(typewriter.visible_text(&message), "");

        typewriter.reveal_all(&message);
        typewriter.clear();
        assert_eq!(typewriter.visible_text(&message), "");
    }

    #[test]
    fn advancing_with_no_streaming_messages_reports_no_change() {
        let mut finished = streaming("a", "hello");
        finished.streaming = false;

        let mut typewriter = Typewriter::new();
        assert!(!typewriter.advance(std::slice::from_ref(&finished), 0.1));
    }

    #[test]
    fn thinking_tags_appear_atomically_while_streaming() {
        let message = streaming("a", "<thinking>reason</thinking>answer");
        let mut typewriter = Typewriter::new();

        typewriter.progress.insert(
            message.id.clone(),
            Progress {
                revealed: 4,
                credit: 0.0,
            },
        );
        assert_eq!(typewriter.visible_text(&message), "");

        typewriter.progress.get_mut(&message.id).unwrap().revealed = 10;
        assert_eq!(typewriter.visible_text(&message), "<thinking>");

        typewriter.progress.get_mut(&message.id).unwrap().revealed = 22;
        assert_eq!(typewriter.visible_text(&message), "<thinking>reason");
    }
}
