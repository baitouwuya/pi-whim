use std::time::{Duration, Instant};

use eframe::egui::{Event, Frame, ImeEvent};
use pi_whim_core::ConversationRole;

use crate::{CHAT_BACKGROUND, TOOL_BACKGROUND, USER_BUBBLE};

pub const COPY_FEEDBACK_DURATION: Duration = Duration::from_millis(1600);

pub fn update_ime_composition(composing: &mut bool, events: &[Event]) -> bool {
    let was_composing = *composing;
    let mut committed_this_frame = false;
    for event in events {
        match event {
            Event::Ime(ImeEvent::Preedit(candidate)) => *composing = !candidate.is_empty(),
            Event::Ime(ImeEvent::Commit(_)) => {
                committed_this_frame = true;
                *composing = false;
            }
            Event::Ime(ImeEvent::Disabled) => *composing = false,
            Event::Ime(ImeEvent::Enabled) => {}
            _ => {}
        }
    }
    was_composing || *composing || committed_this_frame
}

pub fn should_submit_from_keyboard(has_focus: bool, enter_pressed: bool, ime_guard: bool) -> bool {
    has_focus && enter_pressed && !ime_guard
}

pub fn copy_feedback_active(copied_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(copied_at) < COPY_FEEDBACK_DURATION
}

pub fn message_frame(role: ConversationRole) -> Frame {
    match role {
        ConversationRole::User => Frame::default()
            .fill(USER_BUBBLE)
            .stroke(eframe::egui::Stroke::new(1.0_f32, crate::LINE))
            .corner_radius(0)
            .inner_margin(eframe::egui::Margin::symmetric(14, 10)),
        ConversationRole::Tool => Frame::default()
            .fill(TOOL_BACKGROUND)
            .corner_radius(0)
            .inner_margin(eframe::egui::Margin::symmetric(10, 6)),
        ConversationRole::Assistant | ConversationRole::System => {
            Frame::default().fill(CHAT_BACKGROUND)
        }
    }
}
