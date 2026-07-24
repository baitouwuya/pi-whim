use std::ops::Range;

use eframe::egui::{
    self, Align, Color32, FontFamily, FontId, FontSelection, Frame, Label, Layout, Margin,
    RichText, Sense, Stroke, TextStyle, Ui,
};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use egui_extras::{Column, TableBuilder};
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};

const TEXT: Color32 = Color32::from_rgb(37, 47, 61);
const LINK: Color32 = Color32::from_rgb(58, 101, 151);
const BORDER: Color32 = Color32::from_rgb(214, 209, 200);
const TABLE_HEADER: Color32 = Color32::from_rgb(236, 231, 222);
const TABLE_STRIPE: Color32 = Color32::from_rgb(244, 241, 235);
const CODE_BACKGROUND: Color32 = Color32::from_rgb(233, 229, 221);
const HEADER_HEIGHT: f32 = 38.0;
const CELL_HORIZONTAL_PADDING: f32 = 10.0;
const MAX_TABLE_WIDTH: f32 = 760.0;

#[derive(Default)]
pub(crate) struct MarkdownRenderer {
    cache: CommonMarkCache,
}

impl MarkdownRenderer {
    pub(crate) fn show(&mut self, ui: &mut Ui, source_id: &str, source: &str) {
        ui.push_id(source_id, |ui| {
            ui.scope(|ui| {
                apply_markdown_style(ui);
                self.show_document(ui, source);
            });
        });
    }

    fn show_document(&mut self, ui: &mut Ui, source: &str) {
        let tables = parse_tables(source);
        if tables.is_empty() {
            self.show_commonmark(ui, source);
            return;
        }

        let mut cursor = 0;
        for (index, table) in tables.iter().enumerate() {
            if cursor < table.range.start {
                self.show_commonmark(ui, &source[cursor..table.range.start]);
            }
            ui.push_id(("table", index), |ui| render_table(ui, table));
            cursor = table.range.end;
        }
        if cursor < source.len() {
            self.show_commonmark(ui, &source[cursor..]);
        }
    }

    fn show_commonmark(&mut self, ui: &mut Ui, source: &str) {
        let source = source.trim_matches('\n');
        if source.trim().is_empty() {
            return;
        }
        CommonMarkViewer::new()
            .indentation_spaces(2)
            .default_width(Some(ui.available_width().max(1.0) as usize))
            .show(ui, &mut self.cache, source);
    }
}

fn apply_markdown_style(ui: &mut Ui) {
    let style = ui.style_mut();
    style.url_in_tooltip = true;
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(16.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(24.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(14.0, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.spacing.item_spacing.y = 6.0;
    style.spacing.interact_size.y = 30.0;
    style.visuals.faint_bg_color = TABLE_STRIPE;
    style.visuals.code_bg_color = CODE_BACKGROUND;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
}

#[derive(Clone, Default)]
struct InlineStyle {
    strong: bool,
    emphasis: bool,
    strikethrough: bool,
    code: bool,
    link: Option<String>,
}

#[derive(Clone)]
struct InlineRun {
    text: String,
    style: InlineStyle,
}

#[derive(Default)]
struct MarkdownCell {
    runs: Vec<InlineRun>,
    style: InlineStyle,
}

impl MarkdownCell {
    fn push_text(&mut self, text: impl Into<String>) {
        self.runs.push(InlineRun {
            text: text.into(),
            style: self.style.clone(),
        });
    }

    fn push_code(&mut self, text: impl Into<String>) {
        let mut style = self.style.clone();
        style.code = true;
        self.runs.push(InlineRun {
            text: text.into(),
            style,
        });
    }

    fn estimated_width(&self) -> f32 {
        self.runs
            .iter()
            .flat_map(|run| run.text.chars())
            .map(|character| if character.is_ascii() { 7.5 } else { 15.0 })
            .sum()
    }

    fn unique_link(&self) -> Option<&str> {
        let mut links = self.runs.iter().filter_map(|run| run.style.link.as_deref());
        let first = links.next()?;
        links.all(|link| link == first).then_some(first)
    }
}

struct MarkdownTable {
    range: Range<usize>,
    alignments: Vec<Alignment>,
    header: Vec<MarkdownCell>,
    rows: Vec<Vec<MarkdownCell>>,
}

struct TableParser {
    table: MarkdownTable,
    current_row: Vec<MarkdownCell>,
    current_cell: Option<MarkdownCell>,
}

impl TableParser {
    fn new(start: usize, alignments: Vec<Alignment>) -> Self {
        Self {
            table: MarkdownTable {
                range: start..start,
                alignments,
                header: Vec::new(),
                rows: Vec::new(),
            },
            current_row: Vec::new(),
            current_cell: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        let Some(cell) = self.current_cell.as_mut() else {
            return;
        };
        match event {
            Event::Start(Tag::Strong) => cell.style.strong = true,
            Event::End(TagEnd::Strong) => cell.style.strong = false,
            Event::Start(Tag::Emphasis) => cell.style.emphasis = true,
            Event::End(TagEnd::Emphasis) => cell.style.emphasis = false,
            Event::Start(Tag::Strikethrough) => cell.style.strikethrough = true,
            Event::End(TagEnd::Strikethrough) => cell.style.strikethrough = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                cell.style.link = Some(dest_url.into_string())
            }
            Event::End(TagEnd::Link) => cell.style.link = None,
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                cell.push_text(text.into_string())
            }
            Event::Code(code) => cell.push_code(code.into_string()),
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                cell.push_code(math.into_string())
            }
            Event::FootnoteReference(note) => cell.push_text(format!("[{note}]")),
            Event::SoftBreak | Event::HardBreak => cell.push_text("\n"),
            Event::Rule => cell.push_text("---"),
            Event::TaskListMarker(checked) => cell.push_text(if checked { "[x] " } else { "[ ] " }),
            Event::Start(_) | Event::End(_) => {}
        }
    }
}

fn parse_tables(source: &str) -> Vec<MarkdownTable> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut tables = Vec::new();
    let mut current: Option<TableParser> = None;

    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if current.is_none() {
            if let Event::Start(Tag::Table(alignments)) = event {
                current = Some(TableParser::new(range.start, alignments));
            }
            continue;
        }

        let parser = current.as_mut().expect("table parser exists");
        match event {
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                parser.current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                parser.current_cell = Some(MarkdownCell::default());
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(cell) = parser.current_cell.take() {
                    parser.current_row.push(cell);
                }
            }
            Event::End(TagEnd::TableHead) => {
                parser.table.header = std::mem::take(&mut parser.current_row);
            }
            Event::End(TagEnd::TableRow) => {
                parser
                    .table
                    .rows
                    .push(std::mem::take(&mut parser.current_row));
            }
            Event::End(TagEnd::Table) => {
                let mut parser = current.take().expect("table parser exists");
                parser.table.range.end = range.end;
                tables.push(parser.table);
            }
            event => parser.event(event),
        }
    }

    tables
}

fn render_table(ui: &mut Ui, table: &MarkdownTable) {
    let column_count = table
        .alignments
        .len()
        .max(table.header.len())
        .max(table.rows.iter().map(Vec::len).max().unwrap_or_default());
    if column_count == 0 {
        return;
    }

    ui.add_space(8.0);
    let outer_width = ui.available_width().max(240.0);
    let available_width = outer_width.min(MAX_TABLE_WIDTH);
    let left_padding = ((outer_width - available_width) / 2.0).max(0.0);
    let first_column_width = if column_count == 2 {
        (available_width * 0.28).clamp(120.0, 220.0)
    } else {
        available_width / column_count as f32
    };

    let mut frame_rect = None;
    ui.horizontal(|ui| {
        ui.add_space(left_padding);
        ui.allocate_ui_with_layout(
            egui::vec2(available_width, 0.0),
            Layout::top_down(Align::Min),
            |ui| {
                let frame = Frame::new()
                    .fill(Color32::from_rgb(247, 247, 245))
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(Margin::ZERO)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.set_min_width(ui.available_width());

                        let mut builder = TableBuilder::new(ui)
                            .id_salt("markdown-table")
                            .striped(true)
                            .vscroll(false)
                            .resizable(false)
                            .cell_layout(Layout::left_to_right(Align::Center));

                        if column_count == 1 {
                            builder = builder.column(Column::remainder());
                        } else if column_count == 2 {
                            builder = builder
                                .column(Column::initial(first_column_width).at_least(100.0))
                                .column(Column::remainder().at_least(140.0));
                        } else {
                            builder =
                                builder.columns(Column::remainder().at_least(72.0), column_count);
                        }

                        builder
                            .header(HEADER_HEIGHT, |mut header| {
                                for column in 0..column_count {
                                    header.col(|ui| {
                                        ui.painter().rect_filled(ui.max_rect(), 0.0, TABLE_HEADER);
                                        if let Some(cell) = table.header.get(column) {
                                            render_cell(
                                                ui,
                                                cell,
                                                true,
                                                table
                                                    .alignments
                                                    .get(column)
                                                    .copied()
                                                    .unwrap_or(Alignment::None),
                                            );
                                        }
                                    });
                                }
                            })
                            .body(|mut body| {
                                for row in &table.rows {
                                    let height = estimate_row_height(
                                        row,
                                        available_width,
                                        first_column_width,
                                        column_count,
                                    );
                                    body.row(height, |mut table_row| {
                                        for column in 0..column_count {
                                            table_row.col(|ui| {
                                                if let Some(cell) = row.get(column) {
                                                    render_cell(
                                                        ui,
                                                        cell,
                                                        false,
                                                        table
                                                            .alignments
                                                            .get(column)
                                                            .copied()
                                                            .unwrap_or(Alignment::None),
                                                    );
                                                }
                                            });
                                        }
                                    });
                                }
                            });
                    });
                frame_rect = Some(frame.response.rect);
            },
        );
    });

    if let Some(frame_rect) = frame_rect {
        let separator_y = frame_rect.top() + HEADER_HEIGHT;
        ui.painter().line_segment(
            [
                egui::pos2(frame_rect.left(), separator_y),
                egui::pos2(frame_rect.right(), separator_y),
            ],
            Stroke::new(1.0_f32, BORDER),
        );
    }
    ui.add_space(8.0);
}

fn render_cell(ui: &mut Ui, cell: &MarkdownCell, header: bool, alignment: Alignment) {
    let mut job = egui::text::LayoutJob::default();
    for run in &cell.runs {
        let mut text = RichText::new(&run.text)
            .font(FontId::new(15.0, FontFamily::Proportional))
            .color(if run.style.link.is_some() { LINK } else { TEXT });
        if header || run.style.strong {
            text = text.strong();
        }
        if run.style.emphasis {
            text = text.italics();
        }
        if run.style.strikethrough {
            text = text.strikethrough();
        }
        if run.style.code {
            text = text
                .font(FontId::new(13.5, FontFamily::Monospace))
                .background_color(CODE_BACKGROUND);
        }
        if run.style.link.is_some() {
            text = text.underline();
        }
        text.append_to(&mut job, ui.style(), FontSelection::Default, Align::Center);
    }

    ui.add_space(CELL_HORIZONTAL_PADDING);
    let horizontal_alignment = match alignment {
        Alignment::Center => Align::Center,
        Alignment::Right => Align::Max,
        Alignment::None | Alignment::Left => Align::Min,
    };
    let width = (ui.available_width() - CELL_HORIZONTAL_PADDING).max(1.0);
    ui.allocate_ui_with_layout(
        egui::vec2(width, ui.available_height()),
        Layout::top_down(horizontal_alignment),
        |ui| {
            let sense = if cell.unique_link().is_some() {
                Sense::click()
            } else {
                Sense::hover()
            };
            let response = ui.add(Label::new(job).wrap().sense(sense));
            if let Some(url) = cell.unique_link() {
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() {
                    ui.ctx().open_url(egui::OpenUrl {
                        url: url.to_owned(),
                        new_tab: true,
                    });
                }
            }
        },
    );
}

fn estimate_row_height(
    row: &[MarkdownCell],
    available_width: f32,
    first_column_width: f32,
    column_count: usize,
) -> f32 {
    let remaining_width = (available_width - first_column_width).max(100.0);
    let other_width = if column_count > 1 {
        remaining_width / (column_count - 1) as f32
    } else {
        available_width
    };
    let lines = row
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let width = if index == 0 {
                first_column_width
            } else {
                other_width
            };
            (cell.estimated_width() / (width - 24.0).max(48.0))
                .ceil()
                .max(1.0)
        })
        .fold(1.0_f32, f32::max);
    (16.0 + lines * 20.0).clamp(38.0, 112.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_table_parser_preserves_ranges_and_inline_styles() {
        let source = "Before\n\n| Tool | Detail |\n| :--- | ---: |\n| **read** | `file` |\n\nAfter";
        let tables = parse_tables(source);

        assert_eq!(tables.len(), 1);
        let table = &tables[0];
        assert_eq!(
            source[table.range.clone()].trim(),
            "| Tool | Detail |\n| :--- | ---: |\n| **read** | `file` |"
        );
        assert_eq!(table.alignments, [Alignment::Left, Alignment::Right]);
        assert_eq!(table.header.len(), 2);
        assert_eq!(table.rows.len(), 1);
        assert!(table.rows[0][0].runs[0].style.strong);
        assert!(table.rows[0][1].runs[0].style.code);
    }
}
