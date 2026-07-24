use eframe::egui::{Align, Button, Layout, Response, RichText, Ui, Vec2, Widget};

use crate::{INK, MUTED_INK, serif_font};

const CONTENT_MAX_WIDTH: f32 = 620.0;
const WIDE_PAGE_PADDING: f32 = 32.0;
const NARROW_PAGE_PADDING: f32 = 12.0;
const FORM_LABEL_WIDTH: f32 = 180.0;
const FORM_COLUMN_GAP: f32 = 20.0;
const FORM_ROW_GAP: f32 = 14.0;
const CONTROL_MAX_WIDTH: f32 = 420.0;
const FORM_ROW_BREAKPOINT: f32 = FORM_LABEL_WIDTH + FORM_COLUMN_GAP + CONTROL_MAX_WIDTH;
const CONTROL_HEIGHT: f32 = 32.0;
const INLINE_GAP: f32 = 8.0;
const ACTION_BUTTON_WIDTH: f32 = 144.0;
const MODEL_ROW_HEIGHT: f32 = 52.0;

pub fn content(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    let available = ui.available_width();
    let padding = if available < CONTENT_MAX_WIDTH + WIDE_PAGE_PADDING * 2.0 {
        NARROW_PAGE_PADDING
    } else {
        WIDE_PAGE_PADDING
    };
    let width = content_width(available, padding);

    let leading_space = ((available - width) * 0.5).max(0.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(leading_space);
        ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().interact_size.y = CONTROL_HEIGHT;
            add_contents(ui);
        });
    });
}

pub fn page_header(ui: &mut Ui, title: &str, description: Option<&str>) {
    ui.heading(RichText::new(title).font(serif_font(30.0)).color(INK));
    if let Some(description) = description {
        ui.add_space(4.0);
        ui.label(RichText::new(description).color(MUTED_INK));
    }
    ui.add_space(16.0);
}

pub fn section_header(ui: &mut Ui, title: &str, description: Option<&str>) {
    ui.separator();
    ui.add_space(14.0);
    ui.heading(RichText::new(title).font(serif_font(19.0)).color(INK));
    if let Some(description) = description {
        ui.add_space(4.0);
        ui.label(RichText::new(description).small().color(MUTED_INK));
    }
    ui.add_space(14.0);
}

pub fn form_row(
    ui: &mut Ui,
    label: &str,
    description: Option<&str>,
    add_control: impl FnOnce(&mut Ui),
) {
    row(ui, Some((label, description)), add_control);
    ui.add_space(FORM_ROW_GAP);
}

/// Aligns actions and custom blocks with the same control column as form rows.
pub fn control_row(ui: &mut Ui, add_control: impl FnOnce(&mut Ui)) {
    row(ui, None, add_control);
    ui.add_space(FORM_ROW_GAP);
}

fn row(ui: &mut Ui, label: Option<(&str, Option<&str>)>, add_control: impl FnOnce(&mut Ui)) {
    let available = ui.available_width();
    if available >= FORM_ROW_BREAKPOINT {
        let control_width = wide_control_width(available);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(
                Vec2::new(FORM_LABEL_WIDTH, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    if let Some((label, description)) = label {
                        row_label(ui, label, description);
                    }
                },
            );
            ui.add_space(FORM_COLUMN_GAP);
            control_column(ui, control_width, add_control);
        });
    } else {
        if let Some((label, description)) = label {
            row_label(ui, label, description);
            ui.add_space(6.0);
        }
        control_column(ui, available.min(CONTROL_MAX_WIDTH), add_control);
    }
}

fn control_column(ui: &mut Ui, width: f32, add_control: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.set_width(width);
        add_control(ui);
    });
}

fn row_label(ui: &mut Ui, label: &str, description: Option<&str>) {
    ui.spacing_mut().item_spacing.y = 4.0;
    // Center the label's first line against the 32pt control column instead
    // of top-aligning it; mixed heights are what made the page look crooked.
    let pad =
        ((CONTROL_HEIGHT - ui.text_style_height(&eframe::egui::TextStyle::Body)) * 0.5).max(0.0);
    ui.add_space(pad);
    ui.label(RichText::new(label).strong().color(INK));
    if let Some(description) = description {
        ui.label(RichText::new(description).small().color(MUTED_INK));
    }
}

pub fn control_width(ui: &Ui) -> f32 {
    ui.available_width().min(CONTROL_MAX_WIDTH)
}

pub fn control_height() -> f32 {
    CONTROL_HEIGHT
}

pub fn sized_control(ui: &mut Ui, widget: impl Widget) -> Response {
    ui.add_sized([control_width(ui), CONTROL_HEIGHT], widget)
}

pub fn compact_control(ui: &mut Ui, widget: impl Widget) -> Response {
    sized_control(ui, widget)
}

pub fn action_button(ui: &mut Ui, text: impl Into<RichText>) -> Response {
    let text: RichText = text.into();
    ui.add_sized([ACTION_BUTTON_WIDTH, CONTROL_HEIGHT], Button::new(text))
}

pub fn inline_gap() -> f32 {
    INLINE_GAP
}

pub fn action_button_width() -> f32 {
    ACTION_BUTTON_WIDTH
}

pub fn inline_leading_width(ui: &Ui, trailing_width: f32) -> f32 {
    inline_leading_width_for(control_width(ui), trailing_width)
}

pub fn model_row_height() -> f32 {
    MODEL_ROW_HEIGHT
}

pub fn segmented<T: Copy + PartialEq>(ui: &mut Ui, current: &mut T, options: &[(T, &str)]) {
    let width = control_width(ui);
    let gap = 6.0;
    let item_width = ((width - gap * (options.len().saturating_sub(1)) as f32)
        / options.len().max(1) as f32)
        .max(72.0);

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (value, label) in options {
            let selected = *current == *value;
            if ui
                .add_sized(
                    [item_width, CONTROL_HEIGHT],
                    Button::new(*label).selected(selected),
                )
                .clicked()
            {
                *current = *value;
            }
        }
    });
}

fn content_width(available: f32, padding: f32) -> f32 {
    (available - padding * 2.0).clamp(0.0, CONTENT_MAX_WIDTH)
}

fn wide_control_width(available: f32) -> f32 {
    (available - FORM_LABEL_WIDTH - FORM_COLUMN_GAP).clamp(0.0, CONTROL_MAX_WIDTH)
}

fn inline_leading_width_for(total_width: f32, trailing_width: f32) -> f32 {
    (total_width - INLINE_GAP - trailing_width).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_content_is_centered_and_width_limited() {
        assert_eq!(content_width(1200.0, WIDE_PAGE_PADDING), 620.0);
        assert_eq!(content_width(680.0, NARROW_PAGE_PADDING), 620.0);
        assert_eq!(content_width(640.0, NARROW_PAGE_PADDING), 616.0);
    }

    #[test]
    fn form_controls_use_stable_size_tokens() {
        assert_eq!(CONTROL_HEIGHT, 32.0);
        assert_eq!(CONTROL_MAX_WIDTH, 420.0);
        assert_eq!(wide_control_width(CONTENT_MAX_WIDTH), CONTROL_MAX_WIDTH);
        assert_eq!(FORM_ROW_BREAKPOINT, CONTENT_MAX_WIDTH);
        assert_eq!(ACTION_BUTTON_WIDTH, 144.0);
        assert_eq!(MODEL_ROW_HEIGHT, 52.0);
        assert_eq!(
            inline_leading_width_for(CONTROL_MAX_WIDTH, CONTROL_HEIGHT)
                + INLINE_GAP
                + CONTROL_HEIGHT,
            CONTROL_MAX_WIDTH
        );
        assert_eq!(
            inline_leading_width_for(CONTROL_MAX_WIDTH, ACTION_BUTTON_WIDTH)
                + INLINE_GAP
                + ACTION_BUTTON_WIDTH,
            CONTROL_MAX_WIDTH
        );
    }
}
