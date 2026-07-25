use std::f32::consts::TAU;

use pi_whim_engine::slash_commands::CommandIcon;

use eframe::egui::{
    Button, Color32, CornerRadius, Painter, Rect, Response, Stroke, StrokeKind, Ui, Vec2, pos2,
    vec2,
};

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Back,
    ChevronRight,
    ChevronDown,
    Copy,
    Check,
    Close,
    Plus,
    Settings,
    Message,
    Send,
    File,
    Folder,
    Cube,
    Brain,
    Compress,
    Warning,
}

pub fn button(ui: &mut Ui, icon: Icon, label: &str, size: Vec2, frame: bool) -> Response {
    let response = ui.add_sized(size, Button::new("").frame(frame));
    let color = ui.style().interact(&response).fg_stroke.color;
    paint_centered(ui.painter(), response.rect, icon, color);
    response.on_hover_text(label)
}

pub fn filled_button(ui: &mut Ui, icon: Icon, label: &str, size: Vec2, fill: Color32) -> Response {
    let response = ui.add_sized(size, Button::new("").fill(fill));
    paint_centered(ui.painter(), response.rect, icon, Color32::WHITE);
    response.on_hover_text(label)
}

pub fn display(ui: &mut Ui, icon: Icon, size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, eframe::egui::Sense::hover());
    paint(ui.painter(), rect.shrink(2.0), icon, color);
}

/// Paint an icon into an explicit rect, for hand-drawn rows that do not
/// allocate child widgets.
pub fn paint_icon(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    paint(painter, rect, icon, color);
}

/// Paint into a square centered in `rect`, so non-square buttons never
/// stretch the artwork.
fn paint_centered(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let side = (rect.width().min(rect.height()) * 0.55).max(8.0);
    paint(
        painter,
        Rect::from_center_size(rect.center(), Vec2::splat(side)),
        icon,
        color,
    );
}

fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    let center = rect.center();
    match icon {
        Icon::Back => {
            painter.line_segment(
                [
                    pos2(rect.left() + rect.width() * 0.18, center.y),
                    pos2(rect.right() - rect.width() * 0.12, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(rect.left() + rect.width() * 0.18, center.y),
                    pos2(
                        rect.left() + rect.width() * 0.45,
                        rect.top() + rect.height() * 0.24,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(rect.left() + rect.width() * 0.18, center.y),
                    pos2(
                        rect.left() + rect.width() * 0.45,
                        rect.bottom() - rect.height() * 0.24,
                    ),
                ],
                stroke,
            );
        }
        Icon::ChevronRight => {
            let x = rect.center().x - rect.width() * 0.12;
            painter.line_segment(
                [
                    pos2(x, rect.top() + rect.height() * 0.23),
                    pos2(x + rect.width() * 0.28, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(x + rect.width() * 0.28, center.y),
                    pos2(x, rect.bottom() - rect.height() * 0.23),
                ],
                stroke,
            );
        }
        Icon::ChevronDown => {
            let y = rect.center().y - rect.height() * 0.12;
            painter.line_segment(
                [
                    pos2(rect.left() + rect.width() * 0.23, y),
                    pos2(center.x, y + rect.height() * 0.28),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(center.x, y + rect.height() * 0.28),
                    pos2(rect.right() - rect.width() * 0.23, y),
                ],
                stroke,
            );
        }
        Icon::Copy => {
            let back = Rect::from_min_max(
                rect.min + vec2(rect.width() * 0.25, 0.0),
                rect.max - vec2(0.0, rect.height() * 0.25),
            );
            let front = Rect::from_min_max(
                rect.min + vec2(0.0, rect.height() * 0.25),
                rect.max - vec2(rect.width() * 0.25, 0.0),
            );
            painter.rect_stroke(back, CornerRadius::same(1), stroke, StrokeKind::Inside);
            painter.rect_filled(front, CornerRadius::same(1), ui_background(painter));
            painter.rect_stroke(front, CornerRadius::same(1), stroke, StrokeKind::Inside);
        }
        Icon::Check => {
            painter.line_segment(
                [
                    pos2(rect.left() + rect.width() * 0.12, center.y),
                    pos2(
                        center.x - rect.width() * 0.08,
                        rect.bottom() - rect.height() * 0.2,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(
                        center.x - rect.width() * 0.08,
                        rect.bottom() - rect.height() * 0.2,
                    ),
                    pos2(
                        rect.right() - rect.width() * 0.08,
                        rect.top() + rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
        }
        Icon::Close => {
            painter.line_segment(
                [
                    pos2(
                        rect.left() + rect.width() * 0.18,
                        rect.top() + rect.height() * 0.18,
                    ),
                    pos2(
                        rect.right() - rect.width() * 0.18,
                        rect.bottom() - rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(
                        rect.right() - rect.width() * 0.18,
                        rect.top() + rect.height() * 0.18,
                    ),
                    pos2(
                        rect.left() + rect.width() * 0.18,
                        rect.bottom() - rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
        }
        Icon::Plus => {
            painter.line_segment(
                [pos2(center.x, rect.top()), pos2(center.x, rect.bottom())],
                stroke,
            );
            painter.line_segment(
                [pos2(rect.left(), center.y), pos2(rect.right(), center.y)],
                stroke,
            );
        }
        Icon::Settings => {
            painter.circle_stroke(center, rect.width() * 0.2, stroke);
            for index in 0..8 {
                let direction = Vec2::angled(index as f32 * TAU / 8.0);
                painter.line_segment(
                    [
                        center + direction * rect.width() * 0.31,
                        center + direction * rect.width() * 0.46,
                    ],
                    stroke,
                );
            }
        }
        Icon::Message => {
            let bubble = Rect::from_center_size(center - vec2(0.0, 1.0), rect.size() * 0.82);
            painter.rect_stroke(bubble, CornerRadius::same(3), stroke, StrokeKind::Inside);
            painter.line_segment(
                [
                    pos2(bubble.left() + bubble.width() * 0.22, bubble.bottom()),
                    pos2(bubble.left() + bubble.width() * 0.12, rect.bottom()),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(bubble.left() + bubble.width() * 0.12, rect.bottom()),
                    pos2(bubble.left() + bubble.width() * 0.48, bubble.bottom()),
                ],
                stroke,
            );
        }
        Icon::Send => {
            painter.line_segment(
                [pos2(center.x, rect.bottom()), pos2(center.x, rect.top())],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(center.x, rect.top()),
                    pos2(
                        rect.left() + rect.width() * 0.18,
                        center.y - rect.height() * 0.04,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(center.x, rect.top()),
                    pos2(
                        rect.right() - rect.width() * 0.18,
                        center.y - rect.height() * 0.04,
                    ),
                ],
                stroke,
            );
        }
        Icon::File => {
            let page = Rect::from_center_size(center, rect.size() * 0.76);
            painter.rect_stroke(page, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment(
                [
                    pos2(
                        page.left() + page.width() * 0.2,
                        page.top() + page.height() * 0.38,
                    ),
                    pos2(
                        page.right() - page.width() * 0.2,
                        page.top() + page.height() * 0.38,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(
                        page.left() + page.width() * 0.2,
                        page.top() + page.height() * 0.62,
                    ),
                    pos2(
                        page.right() - page.width() * 0.34,
                        page.top() + page.height() * 0.62,
                    ),
                ],
                stroke,
            );
        }
        Icon::Folder => {
            let folder = Rect::from_center_size(center + vec2(0.0, 1.0), rect.size() * 0.8);
            painter.rect_stroke(folder, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.line_segment(
                [
                    pos2(folder.left() + folder.width() * 0.08, folder.top()),
                    pos2(folder.left() + folder.width() * 0.36, folder.top()),
                ],
                stroke,
            );
        }
        Icon::Cube => {
            let top = pos2(center.x, rect.top() + rect.height() * 0.12);
            let left = pos2(
                rect.left() + rect.width() * 0.18,
                center.y - rect.height() * 0.14,
            );
            let right = pos2(
                rect.right() - rect.width() * 0.18,
                center.y - rect.height() * 0.14,
            );
            let bottom_left = pos2(
                rect.left() + rect.width() * 0.18,
                center.y + rect.height() * 0.26,
            );
            let bottom_right = pos2(
                rect.right() - rect.width() * 0.18,
                center.y + rect.height() * 0.26,
            );
            let bottom = pos2(center.x, rect.bottom() - rect.height() * 0.1);
            painter.line_segment([top, left], stroke);
            painter.line_segment([top, right], stroke);
            painter.line_segment([left, bottom_left], stroke);
            painter.line_segment([right, bottom_right], stroke);
            painter.line_segment([bottom_left, bottom], stroke);
            painter.line_segment([bottom_right, bottom], stroke);
            painter.line_segment([top, bottom], stroke);
        }
        Icon::Brain => {
            let radius = rect.width().min(rect.height()) * 0.3;
            painter.circle_stroke(center - vec2(radius * 0.32, 0.0), radius, stroke);
            painter.circle_stroke(center + vec2(radius * 0.32, 0.0), radius, stroke);
            painter.line_segment(
                [
                    pos2(center.x, rect.top() + rect.height() * 0.2),
                    pos2(center.x, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    pos2(center.x, center.y),
                    pos2(center.x, rect.bottom() - rect.height() * 0.16),
                ],
                stroke,
            );
        }
        Icon::Compress => {
            let inset = rect.width() * 0.16;
            let arm = rect.width() * 0.22;
            let left = rect.left() + inset;
            let right = rect.right() - inset;
            let top = rect.top() + inset;
            let bottom = rect.bottom() - inset;
            painter.line_segment([pos2(left, top + arm), pos2(left, top)], stroke);
            painter.line_segment([pos2(left, top), pos2(left + arm, top)], stroke);
            painter.line_segment([pos2(right - arm, top), pos2(right, top)], stroke);
            painter.line_segment([pos2(right, top), pos2(right, top + arm)], stroke);
            painter.line_segment([pos2(left, bottom - arm), pos2(left, bottom)], stroke);
            painter.line_segment([pos2(left, bottom), pos2(left + arm, bottom)], stroke);
            painter.line_segment([pos2(right - arm, bottom), pos2(right, bottom)], stroke);
            painter.line_segment([pos2(right, bottom), pos2(right, bottom - arm)], stroke);
        }
        Icon::Warning => {
            let top = pos2(center.x, rect.top() + rect.height() * 0.06);
            let left = pos2(
                rect.left() + rect.width() * 0.06,
                rect.bottom() - rect.height() * 0.12,
            );
            let right = pos2(
                rect.right() - rect.width() * 0.06,
                rect.bottom() - rect.height() * 0.12,
            );
            painter.line_segment([top, left], stroke);
            painter.line_segment([top, right], stroke);
            painter.line_segment([left, right], stroke);
            painter.line_segment(
                [
                    pos2(center.x, rect.top() + rect.height() * 0.3),
                    pos2(center.x, center.y + rect.height() * 0.04),
                ],
                stroke,
            );
            painter.circle_filled(
                pos2(center.x, rect.bottom() - rect.height() * 0.24),
                1.3,
                color,
            );
        }
    }
}

fn ui_background(painter: &Painter) -> Color32 {
    painter.ctx().style().visuals.extreme_bg_color
}

/// The glyph this build draws for a palette entry's purpose.
///
/// The palette names a meaning rather than an icon, since it is shared with the
/// gpui views; this is where that meaning becomes one of the hand-drawn paths.
pub fn for_command(icon: CommandIcon) -> Icon {
    match icon {
        CommandIcon::Model => Icon::Cube,
        CommandIcon::Thinking => Icon::Brain,
        CommandIcon::Copy => Icon::Copy,
        CommandIcon::Message => Icon::Message,
        CommandIcon::Compact => Icon::Compress,
        CommandIcon::File => Icon::File,
        CommandIcon::Settings => Icon::Settings,
        CommandIcon::Stop => Icon::Close,
    }
}
