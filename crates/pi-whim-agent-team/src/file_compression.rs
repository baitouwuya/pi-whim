//! Conservative, pure file-output compression for agent reads.
//!
//! This module does not perform filesystem I/O. Callers provide one stable UTF-8
//! snapshot and receive either an exact excerpt or an annotated outline whose
//! omitted ranges can be read again with [`ReadCursor`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_TARGET_TOKENS: usize = 6_000;
pub const DEFAULT_HARD_TOKENS: usize = 12_000;
pub const DEFAULT_TARGET_BYTES: usize = 96 * 1024;
pub const DEFAULT_HARD_BYTES: usize = 128 * 1024;
const MIN_GAIN_TOKENS: usize = 512;
const MIN_GAIN_RATIO_NUMERATOR: usize = 15;
const MIN_GAIN_RATIO_DENOMINATOR: usize = 100;
const MAX_MARKER_BYTES: usize = 2_048;
const MAX_MARKER_RATIO_NUMERATOR: usize = 8;
const MAX_MARKER_RATIO_DENOMINATOR: usize = 100;
const MAX_OMITTED_SEGMENTS: usize = 16;
const MAX_STRUCTURED_ANCHORS: usize = 24;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    #[default]
    Auto,
    Raw,
    Adaptive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Code,
    Markdown,
    Config,
    Text,
    Binary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CompressionRequest {
    #[serde(default)]
    pub mode: CompressionMode,
    #[serde(default)]
    pub path: Option<String>,
    /// One-based first line, matching Pi's read tool.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Number of lines to select, matching Pi's read tool.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default = "default_target_tokens")]
    pub target_tokens: usize,
    #[serde(default = "default_hard_tokens")]
    pub hard_tokens: usize,
    #[serde(default = "default_target_bytes")]
    pub target_bytes: usize,
    #[serde(default = "default_hard_bytes")]
    pub hard_bytes: usize,
    /// Opaque identity supplied by the filesystem owner. It is copied into cursors.
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// Optional exact byte bounds used by a continuation cursor. Bounds are half-open.
    #[serde(default)]
    pub byte_start: Option<usize>,
    #[serde(default)]
    pub byte_end: Option<usize>,
}

impl Default for CompressionRequest {
    fn default() -> Self {
        Self {
            mode: CompressionMode::Auto,
            path: None,
            offset: None,
            limit: None,
            target_tokens: DEFAULT_TARGET_TOKENS,
            hard_tokens: DEFAULT_HARD_TOKENS,
            target_bytes: DEFAULT_TARGET_BYTES,
            hard_bytes: DEFAULT_HARD_BYTES,
            snapshot_id: None,
            byte_start: None,
            byte_end: None,
        }
    }
}

fn default_target_tokens() -> usize {
    DEFAULT_TARGET_TOKENS
}

fn default_hard_tokens() -> usize {
    DEFAULT_HARD_TOKENS
}

fn default_target_bytes() -> usize {
    DEFAULT_TARGET_BYTES
}

fn default_hard_bytes() -> usize {
    DEFAULT_HARD_BYTES
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadCursor {
    pub offset: usize,
    pub limit: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    Raw,
    Omitted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ReadCursor>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompressionResult {
    pub content: String,
    pub mode: CompressionMode,
    pub format: FileKind,
    pub total_bytes: usize,
    pub total_lines: usize,
    pub selected_start_line: usize,
    pub selected_end_line: usize,
    pub raw_selected_bytes: usize,
    pub raw_selected_tokens: usize,
    pub output_bytes: usize,
    pub output_tokens: usize,
    pub truncated: bool,
    pub segments: Vec<Segment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<ReadCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionError {
    pub code: &'static str,
    pub message: String,
}

impl CompressionError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_budget() -> Self {
        Self::new(
            "file_invalid_budget",
            "compression budgets must be positive",
        )
    }

    fn invalid_limit() -> Self {
        Self::new("file_invalid_limit", "read limit must be positive")
    }

    fn invalid_mode() -> Self {
        Self::new(
            "file_invalid_mode",
            "compression mode must be auto, raw, or adaptive",
        )
    }

    fn invalid_cursor() -> Self {
        Self::new("file_invalid_cursor", "the continuation cursor is invalid")
    }

    fn invalid_byte_range() -> Self {
        Self::new("file_invalid_range", "the requested byte range is invalid")
    }

    fn invalid_utf8() -> Self {
        Self::new(
            "file_binary_unsupported",
            "file content is not valid UTF-8 text",
        )
    }

    fn stale_cursor() -> Self {
        Self::new(
            "stale_snapshot",
            "the continuation cursor belongs to another snapshot",
        )
    }

    fn offset_out_of_bounds(offset: usize, total_lines: usize) -> Self {
        Self::new(
            "file_invalid_range",
            format!("offset {offset} is beyond the end of the file ({total_lines} lines)"),
        )
    }
}

impl std::fmt::Display for CompressionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompressionError {}

#[derive(Clone, Copy, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
}

/// Estimate model tokens conservatively without introducing a tokenizer dependency.
/// ASCII text is treated as roughly four characters per token; non-ASCII text is
/// treated as roughly two code points per token. The result is never zero for a
/// non-empty string.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    let ascii_tokens = ascii.div_ceil(4);
    let non_ascii_tokens = non_ascii.div_ceil(2);
    (ascii_tokens + non_ascii_tokens).max(1)
}

pub fn classify(path: Option<&str>, content: &str) -> FileKind {
    let extension = path
        .and_then(|value| value.rsplit_once('.').map(|(_, extension)| extension))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "c"
            | "cc"
            | "cpp"
            | "h"
            | "hpp"
            | "go"
            | "java"
            | "kt"
            | "swift"
            | "py"
            | "rb"
            | "php"
            | "cs"
            | "sh"
            | "bash"
    ) {
        return FileKind::Code;
    }
    if matches!(extension.as_str(), "md" | "markdown" | "mdx" | "rst") {
        return FileKind::Markdown;
    }
    if matches!(
        extension.as_str(),
        "json"
            | "json5"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "conf"
            | "xml"
            | "env"
            | "lock"
    ) {
        return FileKind::Config;
    }
    let trimmed = content.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return FileKind::Config;
    }
    if content.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#') && !line.starts_with("#!")
    }) {
        return FileKind::Markdown;
    }
    if content.contains("fn ")
        || content.contains("function ")
        || content.contains("class ")
        || content.contains("import ")
        || content.contains("use ")
    {
        return FileKind::Code;
    }
    FileKind::Text
}

pub fn compress(
    content: &str,
    request: &CompressionRequest,
) -> Result<CompressionResult, CompressionError> {
    validate_budget(request)?;
    let lines = line_spans(content);
    let (line_start, line_end, mut selected_start_line, mut selected_end_line) =
        selection(&lines, request.offset, request.limit)?;
    let selected_start = request.byte_start.unwrap_or(line_start);
    let selected_end = request.byte_end.unwrap_or(line_end);
    if selected_start > selected_end
        || selected_end > content.len()
        || !content.is_char_boundary(selected_start)
        || !content.is_char_boundary(selected_end)
    {
        return Err(CompressionError::invalid_byte_range());
    }
    if !lines.is_empty() {
        selected_start_line = line_for_start(&lines, selected_start).min(lines.len());
        selected_end_line =
            line_for_end(&lines, selected_end, selected_start_line).min(lines.len());
    }
    let selected = &content[selected_start..selected_end];
    let raw_bytes = selected.len();
    let raw_tokens = estimate_tokens(selected);
    let format = classify(request.path.as_deref(), content);

    let raw_fit_target = fits(
        raw_bytes,
        raw_tokens,
        request.target_bytes,
        request.target_tokens,
    );
    let raw_fit_hard = fits(
        raw_bytes,
        raw_tokens,
        request.hard_bytes,
        request.hard_tokens,
    );
    let explicit_range = request.offset.is_some()
        || request.limit.is_some()
        || request.byte_start.is_some()
        || request.byte_end.is_some();
    let should_return_raw = match request.mode {
        CompressionMode::Raw => raw_fit_hard,
        CompressionMode::Auto | CompressionMode::Adaptive => raw_fit_target,
    };
    if should_return_raw {
        return Ok(result_for_raw(
            selected,
            request,
            format,
            content.len(),
            lines.len(),
            selected_start_line,
            selected_end_line,
            selected_start,
            selected_end,
            None,
        ));
    }

    let paged = || {
        raw_page(
            content,
            &lines,
            request,
            format,
            selected_start,
            selected_end,
            selected_start_line,
            selected_end_line,
        )
    };
    if explicit_range
        || matches!(format, FileKind::Binary)
        || matches!(request.mode, CompressionMode::Raw)
    {
        return Ok(paged());
    }

    if let Some(adaptive) = adaptive_outline(
        content,
        &lines,
        request,
        format,
        selected_start,
        selected_end,
        selected_start_line,
        selected_end_line,
        raw_tokens,
    ) {
        return Ok(adaptive);
    }
    Ok(paged())
}

#[derive(Clone, Debug)]
pub struct RenderedText {
    pub text: String,
    pub details: Value,
}

pub trait TextInput {
    fn as_text(&self) -> Result<&str, CompressionError>;
}

impl TextInput for str {
    fn as_text(&self) -> Result<&str, CompressionError> {
        Ok(self)
    }
}

impl TextInput for String {
    fn as_text(&self) -> Result<&str, CompressionError> {
        Ok(self.as_str())
    }
}

impl TextInput for [u8] {
    fn as_text(&self) -> Result<&str, CompressionError> {
        std::str::from_utf8(self).map_err(|_| CompressionError::invalid_utf8())
    }
}

impl TextInput for Vec<u8> {
    fn as_text(&self) -> Result<&str, CompressionError> {
        self.as_slice().as_text()
    }
}

/// Adapter for the Rust supervisor's JSON tool boundary.
///
/// `max_tokens` and `max_bytes` are hard response limits. Automatic/adaptive
/// selection uses 75% of each as its target so the response retains room for
/// continuation markers and protocol metadata. A cursor is encoded as the JSON
/// representation of [`ReadCursor`], which keeps the wire format dependency-free.
#[allow(clippy::too_many_arguments)]
pub fn render_text<T: TextInput + ?Sized>(
    path: &std::path::Path,
    input: &T,
    mode: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    max_tokens: usize,
    max_bytes: usize,
    snapshot_id: Option<&str>,
    cursor: Option<&str>,
) -> Result<RenderedText, CompressionError> {
    let content = input.as_text()?;
    let mode = parse_mode(mode)?;
    let mut request = CompressionRequest {
        mode,
        path: Some(path.to_string_lossy().into_owned()),
        offset,
        limit,
        target_tokens: max_tokens.saturating_mul(3).saturating_div(4),
        hard_tokens: max_tokens,
        target_bytes: max_bytes.saturating_mul(3).saturating_div(4),
        hard_bytes: max_bytes,
        snapshot_id: snapshot_id.map(str::to_owned),
        ..CompressionRequest::default()
    };
    if let Some(encoded_cursor) = cursor {
        let parsed = serde_json::from_str::<ReadCursor>(encoded_cursor)
            .map_err(|_| CompressionError::invalid_cursor())?;
        if parsed.limit == 0 {
            return Err(CompressionError::invalid_cursor());
        }
        if let (Some(expected), Some(actual)) = (snapshot_id, parsed.snapshot_id.as_deref())
            && expected != actual
        {
            return Err(CompressionError::stale_cursor());
        }
        request.offset = Some(parsed.offset);
        request.limit = Some(parsed.limit);
        request.byte_start = Some(parsed.byte_start);
        request.byte_end = Some(parsed.byte_end);
        if request.snapshot_id.is_none() {
            request.snapshot_id = parsed.snapshot_id;
        }
    }
    let mut result = compress(content, &request)?;
    // Pi keeps details in the event layer, so expose only the continuation
    // record needed by a model when a read is resumable. Re-render with the
    // remaining budget so hard response limits still hold.
    if result.truncated {
        for _ in 0..3 {
            let metadata = continuation_metadata(&result, request.snapshot_id.as_deref());
            let available_bytes = max_bytes.saturating_sub(metadata.len());
            let available_tokens = max_tokens.saturating_sub(estimate_tokens(&metadata));
            if result.content.len() <= available_bytes
                && estimate_tokens(&result.content) <= available_tokens
            {
                break;
            }
            if available_bytes == 0 || available_tokens == 0 {
                break;
            }
            request.target_bytes = request.target_bytes.min(available_bytes);
            request.hard_bytes = request.hard_bytes.min(available_bytes);
            request.target_tokens = request.target_tokens.min(available_tokens);
            request.hard_tokens = request.hard_tokens.min(available_tokens);
            result = compress(content, &request)?;
            if !result.truncated {
                break;
            }
        }
    }
    let next_cursor = result
        .next
        .as_ref()
        .map(|next| serde_json::to_string(next).unwrap_or_default());
    let mut details = json!({
        "mode": result.mode,
        "format": result.format,
        "total_bytes": result.total_bytes,
        "total_lines": result.total_lines,
        "selected_start_line": result.selected_start_line,
        "selected_end_line": result.selected_end_line,
        "raw_selected_bytes": result.raw_selected_bytes,
        "raw_selected_tokens": result.raw_selected_tokens,
        "output_bytes": result.output_bytes,
        "output_tokens": result.output_tokens,
        "truncated": result.truncated,
        "segments": result.segments,
        "next": result.next,
        "next_cursor": next_cursor,
    });
    let text = if result.truncated {
        let metadata = continuation_metadata(&result, request.snapshot_id.as_deref());
        let mut visible = result.content;
        if visible.len() + metadata.len() <= max_bytes
            && estimate_tokens(&visible) + estimate_tokens(&metadata) <= max_tokens
        {
            visible.push_str(&metadata);
        }
        visible
    } else {
        result.content
    };
    details["output_bytes"] = json!(text.len());
    details["output_tokens"] = json!(estimate_tokens(&text));
    Ok(RenderedText { text, details })
}

fn continuation_metadata(result: &CompressionResult, snapshot_id: Option<&str>) -> String {
    let omitted_total = result
        .segments
        .iter()
        .filter(|segment| segment.kind == SegmentKind::Omitted)
        .count();
    let omitted = result
        .segments
        .iter()
        .filter(|segment| segment.kind == SegmentKind::Omitted)
        .filter_map(|segment| {
            segment.cursor.as_ref().map(|cursor| {
                let mut entry = json!({ "offset": cursor.offset, "limit": cursor.limit });
                if let Some(label) = &segment.label {
                    entry["label"] = json!(label);
                }
                entry
            })
        })
        .take(16)
        .collect::<Vec<_>>();
    let next_cursor = result
        .next
        .as_ref()
        .and_then(|next| serde_json::to_string(next).ok());
    let value = json!({
        "snapshot_id": snapshot_id,
        "truncated": result.truncated,
        "next_cursor": next_cursor,
        "omitted": omitted,
        "omitted_total": omitted_total,
    });
    // Surface a plain-language caution ahead of the structured cursor block so
    // the model treats the view as lossy: decisions must be re-checked against
    // the omitted or paginated ranges before they are cited, never made from
    // the compressed excerpt alone.
    let warning = if omitted_total > 0 {
        "以上为压缩视图：标记 `[... lines X-Y omitted: 摘要]` 的行段已省略，并非完整原文。\
         需要精确判断时请先按下方 omitted 游标（含 label 提示）回读对应行段，再下结论，避免断章取义。\
         若要获取完整无省略的内容，请使用 mode=raw 重新读取。"
    } else {
        "以上内容因长度被截断，仅展示部分原文。\
         需要后续内容请按下方 next_cursor 继续，避免基于不完整信息断章取义。\
         若要获取完整无省略的内容，请使用 mode=raw 重新读取。"
    };
    format!(
        "<read_warning>{warning}</read_warning>\n<read_metadata>{}</read_metadata>\n",
        value
    )
}

fn parse_mode(mode: &str) -> Result<CompressionMode, CompressionError> {
    match mode {
        "auto" => Ok(CompressionMode::Auto),
        "raw" => Ok(CompressionMode::Raw),
        "adaptive" => Ok(CompressionMode::Adaptive),
        _ => Err(CompressionError::invalid_mode()),
    }
}

fn validate_budget(request: &CompressionRequest) -> Result<(), CompressionError> {
    if request.target_tokens == 0
        || request.hard_tokens == 0
        || request.target_bytes == 0
        || request.hard_bytes == 0
        || request.target_tokens > request.hard_tokens
        || request.target_bytes > request.hard_bytes
    {
        return Err(CompressionError::invalid_budget());
    }
    if request.limit == Some(0) {
        return Err(CompressionError::invalid_limit());
    }
    Ok(())
}

fn line_spans(content: &str) -> Vec<LineSpan> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (index, character) in content.char_indices() {
        if character == '\n' {
            spans.push(LineSpan {
                start,
                end: index + 1,
            });
            start = index + 1;
        }
    }
    if start < content.len() {
        spans.push(LineSpan {
            start,
            end: content.len(),
        });
    }
    spans
}

fn selection(
    lines: &[LineSpan],
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<(usize, usize, usize, usize), CompressionError> {
    if lines.is_empty() {
        return Ok((0, 0, 1, 0));
    }
    let start_line = offset.unwrap_or(1).max(1);
    if start_line > lines.len() {
        return Err(CompressionError::offset_out_of_bounds(
            start_line,
            lines.len(),
        ));
    }
    let end_line = limit
        .map(|value| start_line.saturating_add(value).saturating_sub(1))
        .unwrap_or(lines.len())
        .min(lines.len());
    Ok((
        lines[start_line - 1].start,
        lines[end_line - 1].end,
        start_line,
        end_line,
    ))
}

fn fits(bytes: usize, tokens: usize, max_bytes: usize, max_tokens: usize) -> bool {
    bytes <= max_bytes && tokens <= max_tokens
}

#[allow(clippy::too_many_arguments)]
fn result_for_raw(
    selected: &str,
    _request: &CompressionRequest,
    format: FileKind,
    total_bytes: usize,
    total_lines: usize,
    selected_start_line: usize,
    selected_end_line: usize,
    selected_start: usize,
    selected_end: usize,
    next: Option<ReadCursor>,
) -> CompressionResult {
    let output_bytes = selected.len();
    let output_tokens = estimate_tokens(selected);
    CompressionResult {
        content: selected.to_owned(),
        mode: CompressionMode::Raw,
        format,
        total_bytes,
        total_lines,
        selected_start_line,
        selected_end_line,
        raw_selected_bytes: output_bytes,
        raw_selected_tokens: output_tokens,
        output_bytes,
        output_tokens,
        truncated: next.is_some(),
        segments: vec![Segment {
            kind: SegmentKind::Raw,
            start_line: selected_start_line,
            end_line: selected_end_line,
            start_byte: selected_start,
            end_byte: selected_end,
            label: None,
            cursor: None,
        }],
        next,
    }
}

#[allow(clippy::too_many_arguments)]
fn raw_page(
    content: &str,
    lines: &[LineSpan],
    request: &CompressionRequest,
    format: FileKind,
    selected_start: usize,
    selected_end: usize,
    selected_start_line: usize,
    selected_end_line: usize,
) -> CompressionResult {
    let boundary = page_boundary(
        content,
        lines,
        selected_start,
        selected_end,
        request.hard_bytes,
        request.hard_tokens,
    );
    let page = &content[selected_start..boundary];
    let page_end_line = line_for_end(lines, boundary, selected_start_line);
    let next = (boundary < selected_end)
        .then(|| cursor_for_bytes(lines, boundary, selected_end, request.snapshot_id.clone()));
    result_for_raw(
        page,
        request,
        format,
        content.len(),
        lines.len(),
        selected_start_line,
        page_end_line.min(selected_end_line),
        selected_start,
        boundary,
        next,
    )
}

fn page_boundary(
    content: &str,
    lines: &[LineSpan],
    start: usize,
    end: usize,
    max_bytes: usize,
    max_tokens: usize,
) -> usize {
    let mut boundary = start;
    for span in lines
        .iter()
        .filter(|span| span.start >= start && span.end <= end)
    {
        let candidate = &content[start..span.end];
        if fits(
            candidate.len(),
            estimate_tokens(candidate),
            max_bytes,
            max_tokens,
        ) {
            boundary = span.end;
        } else {
            break;
        }
    }
    if boundary > start {
        return boundary;
    }
    let maximum = start.saturating_add(max_bytes).min(end);
    let mut byte_boundary = maximum;
    while byte_boundary > start && !content.is_char_boundary(byte_boundary) {
        byte_boundary -= 1;
    }
    if byte_boundary == start && start < end {
        content[start..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| start + index)
            .unwrap_or(end)
    } else {
        byte_boundary
    }
}

fn line_for_end(lines: &[LineSpan], byte_end: usize, fallback: usize) -> usize {
    lines
        .iter()
        .position(|span| byte_end <= span.end)
        .map(|index| index + 1)
        .unwrap_or(fallback)
}

fn line_for_start(lines: &[LineSpan], byte_start: usize) -> usize {
    lines
        .iter()
        .position(|span| byte_start < span.end)
        .map(|index| index + 1)
        .unwrap_or(lines.len().saturating_add(1))
}

fn cursor_for_bytes(
    lines: &[LineSpan],
    byte_start: usize,
    byte_end: usize,
    snapshot_id: Option<String>,
) -> ReadCursor {
    let offset = line_for_start(lines, byte_start);
    let end_line = line_for_end(lines, byte_end, offset);
    ReadCursor {
        offset,
        limit: end_line.saturating_sub(offset).saturating_add(1),
        byte_start,
        byte_end,
        snapshot_id,
    }
}

fn render_outline(
    content: &str,
    lines: &[LineSpan],
    first: usize,
    selected_start_line: usize,
    keep: &[bool],
    snapshot_id: Option<&str>,
) -> (String, Vec<Segment>, usize) {
    let mut output = String::new();
    let mut segments = Vec::new();
    let mut cursor = 0usize;
    let mut omitted_count = 0usize;
    while cursor < keep.len() {
        if keep[cursor] {
            let start = cursor;
            while cursor < keep.len() && keep[cursor] {
                cursor += 1;
            }
            let span = lines[first + start].start..lines[first + cursor - 1].end;
            output.push_str(&content[span.clone()]);
            segments.push(Segment {
                kind: SegmentKind::Raw,
                start_line: selected_start_line + start,
                end_line: selected_start_line + cursor - 1,
                start_byte: span.start,
                end_byte: span.end,
                label: None,
                cursor: None,
            });
        } else {
            let start = cursor;
            while cursor < keep.len() && !keep[cursor] {
                cursor += 1;
            }
            let span = lines[first + start].start..lines[first + cursor - 1].end;
            // Keep the visible marker short. The complete byte cursor and
            // continuation coordinates remain in `segments`, so the model can
            // resume the omitted range without inflating the useful view. A
            // short label from the first non-empty omitted line hints at what
            // was elided so the model can judge whether to re-read it.
            let label = first_omitted_label(&content[span.clone()]);
            let marker = match &label {
                Some(label) => format!(
                    "[... lines {}-{} omitted: {}]\n",
                    selected_start_line + start,
                    selected_start_line + cursor - 1,
                    label,
                ),
                None => format!(
                    "[... lines {}-{} omitted]\n",
                    selected_start_line + start,
                    selected_start_line + cursor - 1,
                ),
            };
            output.push_str(&marker);
            let read_cursor =
                cursor_for_bytes(lines, span.start, span.end, snapshot_id.map(str::to_owned));
            segments.push(Segment {
                kind: SegmentKind::Omitted,
                start_line: selected_start_line + start,
                end_line: selected_start_line + cursor - 1,
                start_byte: span.start,
                end_byte: span.end,
                label,
                cursor: Some(read_cursor),
            });
            omitted_count += 1;
        }
    }
    (output, segments, omitted_count)
}

/// Pick a short human-readable hint for an omitted range: the first non-empty
/// line, trimmed and capped so a marker stays a single scannable line.
fn first_omitted_label(omitted: &str) -> Option<String> {
    const MAX_LABEL_CHARS: usize = 60;
    let line = omitted
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut label = line.chars().take(MAX_LABEL_CHARS).collect::<String>();
    if line.chars().count() > MAX_LABEL_CHARS {
        label.push('…');
    }
    Some(label)
}

#[allow(clippy::too_many_arguments)]
fn adaptive_outline(
    content: &str,
    lines: &[LineSpan],
    request: &CompressionRequest,
    format: FileKind,
    selected_start: usize,
    selected_end: usize,
    selected_start_line: usize,
    selected_end_line: usize,
    raw_tokens: usize,
) -> Option<CompressionResult> {
    let first = selected_start_line - 1;
    let last = selected_end_line;
    let selected_lines = (first..last)
        .map(|index| &content[lines[index].start..lines[index].end])
        .collect::<Vec<_>>();
    const EDGE_CONTEXT_LINES: usize = 8;
    let raw_selected_bytes = selected_end - selected_start;
    let minimum_gain =
        (raw_tokens * MIN_GAIN_RATIO_NUMERATOR / MIN_GAIN_RATIO_DENOMINATOR).max(MIN_GAIN_TOKENS);
    // Bound markers against the raw size, not the compressed output: a file
    // that collapses well leaves a short outline whose markers can dominate
    // the visible bytes even though they are a tiny fraction of the source.
    let marker_byte_limit = (raw_selected_bytes.saturating_mul(MAX_MARKER_RATIO_NUMERATOR)
        / MAX_MARKER_RATIO_DENOMINATOR)
        .max(MAX_MARKER_BYTES);

    // Try progressively sparser anchor sets: fewer kept declarations and a
    // shorter trailing context window. A dense file of small methods can keep
    // every signature plus 17 surrounding lines and still overflow the budget;
    // rather than abandon compression, we retry with a tighter outline until
    // the rendered view fits or the configuration is too sparse to help.
    let configs: [(usize, usize); 3] = [(MAX_STRUCTURED_ANCHORS, 17), (12, 6), (6, 3)];
    for (max_anchors, context_lines) in configs {
        let mut keep = structural_anchors(format, &selected_lines, max_anchors, context_lines);
        if keep.iter().all(|value| *value) {
            continue;
        }
        // Always retain enough local context at both ends for an omission
        // marker to remain a small part of the visible view, even for compact
        // configs.
        for index in 0..keep.len().min(EDGE_CONTEXT_LINES) {
            keep[index] = true;
        }
        for index in keep.len().saturating_sub(EDGE_CONTEXT_LINES)..keep.len() {
            keep[index] = true;
        }

        let (output, segments, omitted_count) = render_outline(
            content,
            lines,
            first,
            selected_start_line,
            &keep,
            request.snapshot_id.as_deref(),
        );
        if omitted_count == 0 {
            continue;
        }
        let output_tokens = estimate_tokens(&output);
        let marker_bytes = output
            .lines()
            .filter(|line| line.starts_with("[... lines "))
            .map(str::len)
            .sum::<usize>();
        let gain = raw_tokens.saturating_sub(output_tokens);
        if !fits(
            output.len(),
            output_tokens,
            request.target_bytes,
            request.target_tokens,
        ) || gain < minimum_gain
            || marker_bytes > marker_byte_limit
            || omitted_count > MAX_OMITTED_SEGMENTS
        {
            continue;
        }
        let next = segments.iter().find_map(|segment| {
            (segment.kind == SegmentKind::Omitted)
                .then(|| segment.cursor.clone())
                .flatten()
        });
        let output_bytes = output.len();
        return Some(CompressionResult {
            content: output,
            mode: CompressionMode::Adaptive,
            format,
            total_bytes: content.len(),
            total_lines: lines.len(),
            selected_start_line,
            selected_end_line,
            raw_selected_bytes,
            raw_selected_tokens: raw_tokens,
            output_bytes,
            output_tokens,
            truncated: true,
            segments,
            next,
        });
    }
    None
}

#[allow(dead_code)]
fn legacy_is_anchor(kind: FileKind, line: &str, index: usize, total: usize) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    match kind {
        FileKind::Code => {
            trimmed.starts_with("#!")
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
                || trimmed.starts_with("use ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("export ")
                || trimmed.starts_with("pub ")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("function ")
                || trimmed.starts_with("class ")
                || trimmed.starts_with("struct ")
                || trimmed.starts_with("enum ")
                || trimmed.starts_with("trait ")
                || trimmed.starts_with("interface ")
                || trimmed.starts_with("type ")
                || trimmed.starts_with("const ")
                || trimmed.starts_with("impl ")
                || trimmed == "}"
        }
        FileKind::Markdown => {
            trimmed.starts_with('#')
                || trimmed == "---"
                || trimmed.starts_with("```")
                || (index < 4)
                || (index + 4 >= total)
        }
        FileKind::Config => {
            trimmed.starts_with('{')
                || trimmed.starts_with('}')
                || trimmed.starts_with('[')
                || trimmed.starts_with(']')
                || trimmed.contains('=')
                || (trimmed.starts_with('"') && trimmed.contains(':'))
                || trimmed.starts_with('#')
                || trimmed.starts_with(';')
        }
        FileKind::Text => index < 4 || index + 4 >= total || trimmed.starts_with('#'),
        FileKind::Binary => false,
    }
}

fn structural_anchors(
    kind: FileKind,
    lines: &[&str],
    max_anchors: usize,
    context_lines: usize,
) -> Vec<bool> {
    match kind {
        FileKind::Code => code_anchors(lines, max_anchors, context_lines),
        FileKind::Markdown => markdown_anchors(lines),
        FileKind::Config => config_anchors(lines, max_anchors),
        FileKind::Text => lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                index < 12 || index + 12 >= lines.len() || line.trim_start().starts_with('#')
            })
            .collect(),
        FileKind::Binary => vec![false; lines.len()],
    }
}

/// Thin an anchor set so at most `max` of the listed indices survive, dropping
/// the rest to let adjacent omission ranges merge into larger ones. Keeping a
/// uniform 1-in-`step` sample preserves the overall structure while bounding
/// the number of omission markers a heavily nested file can emit.
fn thin_anchors(keep: &mut [bool], indices: &[usize], max: usize) {
    if indices.len() > max {
        let step = indices.len().div_ceil(max);
        for (ordinal, &index) in indices.iter().enumerate() {
            if ordinal % step != 0 {
                keep[index] = false;
            }
        }
    }
}

fn code_anchors(lines: &[&str], max_anchors: usize, context_lines: usize) -> Vec<bool> {
    let mut keep = vec![false; lines.len()];
    let mut brace_depth = 0i32;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len().saturating_sub(line.trim_start().len());
        let structural_level = brace_depth <= 1 || indent == 0;
        let import = ["use ", "import ", "from ", "mod ", "package "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        let doc = ["///", "//!", "/**", "*"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
        keep[index] = structural_level && (import || doc || is_code_declaration(trimmed));
        brace_depth += brace_delta(line);
    }
    let declaration_indices = keep
        .iter()
        .enumerate()
        .filter_map(|(index, keep)| {
            (*keep && is_code_declaration(lines[index].trim())).then_some(index)
        })
        .collect::<Vec<_>>();
    thin_anchors(&mut keep, &declaration_indices, max_anchors);
    let declarations = keep
        .iter()
        .enumerate()
        .filter_map(|(index, keep)| keep.then_some(index))
        .collect::<Vec<_>>();
    for declaration in declarations {
        for entry in keep
            .iter_mut()
            .take((declaration + context_lines).min(lines.len()))
            .skip(declaration)
        {
            *entry = true;
        }
    }
    keep
}

fn is_code_declaration(line: &str) -> bool {
    let normalized = line
        .trim_start_matches("pub(crate) ")
        .trim_start_matches("pub(super) ")
        .trim_start_matches("pub ")
        .trim_start_matches("export default ")
        .trim_start_matches("export ")
        .trim_start_matches("async ")
        .trim_start_matches("static ");
    [
        "fn ",
        "function ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "interface ",
        "type ",
        "impl ",
        "namespace ",
        "module ",
    ]
    .iter()
    .any(|prefix| normalized.starts_with(prefix))
        || (normalized.starts_with("const ") && normalized.contains([':', '=']))
        || looks_like_method_signature(normalized)
}

fn looks_like_method_signature(line: &str) -> bool {
    let prefix = line.split('(').next().unwrap_or_default().trim();
    line.contains('(')
        && !prefix.is_empty()
        && prefix.chars().all(|character| {
            character.is_alphanumeric() || matches!(character, '_' | '$' | '#' | ' ')
        })
        && ![
            "if", "for", "while", "match", "switch", "catch", "return", "throw",
        ]
        .iter()
        .any(|keyword| prefix == *keyword || prefix.starts_with(&format!("{keyword} ")))
}

fn brace_delta(line: &str) -> i32 {
    line.split("//")
        .next()
        .unwrap_or(line)
        .chars()
        .fold(0, |depth, character| match character {
            '{' => depth + 1,
            '}' => depth - 1,
            _ => depth,
        })
}

fn markdown_anchors(lines: &[&str]) -> Vec<bool> {
    let mut keep = vec![false; lines.len()];
    const MAX_HEADINGS: usize = 12;
    const MAX_CODE_BLOCKS: usize = 6;
    const MAX_FRONT_MATTER_LINES: usize = 20;
    const HEADING_CONTEXT_BYTES: usize = 512;
    const HEADING_CONTEXT_LINES: usize = 24;

    let mut scan_start = 0usize;
    if lines.first().is_some_and(|line| line.trim() == "---") {
        for (index, line) in lines.iter().enumerate().take(MAX_FRONT_MATTER_LINES) {
            keep[index] = true;
            scan_start = index + 1;
            if index > 0 && line.trim() == "---" {
                break;
            }
        }
    }

    let mut headings = Vec::new();
    let mut fences = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;
    for (index, line) in lines.iter().enumerate().skip(scan_start) {
        let trimmed = line.trim();
        let bytes = trimmed.as_bytes();
        let fence =
            bytes.len() >= 3 && (bytes[..3] == [96, 96, 96] || bytes[..3] == [126, 126, 126]);
        if fence {
            if in_fence {
                fences.push((fence_start, index));
            } else {
                fence_start = index;
            }
            in_fence = !in_fence;
        } else if !in_fence && trimmed.starts_with('#') {
            let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            if (1..=6).contains(&level) {
                headings.push((index, level));
            }
        }
    }

    let mut selected = headings
        .iter()
        .copied()
        .filter(|(_, level)| *level <= 2)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        selected = headings.iter().copied().take(MAX_HEADINGS).collect();
    }
    if selected.len() > MAX_HEADINGS {
        let step = selected.len().div_ceil(MAX_HEADINGS);
        selected = selected
            .into_iter()
            .enumerate()
            .filter_map(|(ordinal, heading)| (ordinal % step == 0).then_some(heading))
            .take(MAX_HEADINGS)
            .collect();
    } else if selected.len() < MAX_HEADINGS {
        for heading in headings.iter().copied().filter(|(_, level)| *level >= 3) {
            if selected.len() >= MAX_HEADINGS {
                break;
            }
            if !selected.iter().any(|(index, _)| *index == heading.0) {
                selected.push(heading);
            }
        }
    }
    selected.sort_unstable_by_key(|(index, _)| *index);

    let sampled = headings.len() > MAX_HEADINGS;
    for (heading, _) in selected {
        keep[heading] = true;
        let section_end = if sampled {
            (heading + HEADING_CONTEXT_LINES).min(lines.len())
        } else {
            headings
                .iter()
                .map(|(index, _)| *index)
                .find(|index| *index > heading)
                .unwrap_or(lines.len())
        };
        let mut context_lines = 0usize;
        let mut context_bytes = 0usize;
        for index in heading + 1..section_end {
            if !lines[index].trim().is_empty() {
                keep[index] = true;
                context_lines += 1;
                context_bytes += lines[index].len();
                if (!sampled && context_lines >= 2)
                    || (sampled && context_bytes >= HEADING_CONTEXT_BYTES)
                {
                    break;
                }
            }
        }
    }

    for (start, end) in fences.into_iter().take(MAX_CODE_BLOCKS) {
        if end.saturating_sub(start) <= 12 {
            for entry in keep.iter_mut().take(end + 1).skip(start) {
                *entry = true;
            }
        } else {
            keep[start] = true;
            keep[end] = true;
        }
    }

    for entry in keep.iter_mut().take(lines.len().min(4)) {
        *entry = true;
    }
    for entry in keep
        .iter_mut()
        .take(lines.len())
        .skip(lines.len().saturating_sub(4))
    {
        *entry = true;
    }
    keep
}

fn config_anchors(lines: &[&str], max_anchors: usize) -> Vec<bool> {
    let mut keep = vec![false; lines.len()];
    let mut depth = 0i32;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len().saturating_sub(line.trim_start().len());
        let shallow = depth <= 1 && indent <= 2;
        let section = (trimmed.starts_with('[') && trimmed.ends_with(']'))
            || trimmed.starts_with('#')
            || trimmed.starts_with(';');
        let assignment = trimmed.contains('=') || trimmed.contains(':');
        let short_value = trimmed.len() <= 120;
        keep[index] = section || (shallow && assignment && short_value);
        depth += brace_delta(line);
    }
    let anchors = keep
        .iter()
        .enumerate()
        .filter_map(|(index, keep)| keep.then_some(index))
        .collect::<Vec<_>>();
    thin_anchors(&mut keep, &anchors, max_anchors);
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: CompressionMode) -> CompressionRequest {
        CompressionRequest {
            mode,
            target_tokens: 30,
            hard_tokens: 60,
            target_bytes: 120,
            hard_bytes: 240,
            ..CompressionRequest::default()
        }
    }

    #[test]
    fn small_file_is_exact_and_has_no_visible_metadata() {
        let source = "alpha\nbeta\n";
        let result = compress(source, &request(CompressionMode::Auto)).unwrap();
        assert_eq!(result.mode, CompressionMode::Raw);
        assert_eq!(result.content, source);
        assert!(!result.content.contains("offset="));
        assert_eq!(result.segments.len(), 1);
        assert_eq!(result.segments[0].start_byte, 0);
        assert_eq!(result.segments[0].end_byte, source.len());
    }

    #[test]
    fn explicit_offset_returns_the_original_contiguous_range() {
        let source = "one\ntwo\nthree\nfour\n";
        let mut read = request(CompressionMode::Adaptive);
        read.offset = Some(2);
        read.limit = Some(2);
        let result = compress(source, &read).unwrap();
        assert_eq!(result.content, "two\nthree\n");
        assert_eq!(result.selected_start_line, 2);
        assert_eq!(result.selected_end_line, 3);
        assert_eq!(result.segments[0].kind, SegmentKind::Raw);
    }

    #[test]
    fn adaptive_candidate_requires_a_real_gain() {
        let source = (0..80)
            .map(|index| format!("fn function_{index}() {{}}\n"))
            .collect::<String>();
        let mut read = request(CompressionMode::Auto);
        read.target_tokens = 20;
        read.hard_tokens = 40;
        read.target_bytes = 80;
        read.hard_bytes = 160;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.mode, CompressionMode::Raw);
        assert!(result.truncated);
        assert!(result.next.is_some());
    }

    #[test]
    fn adaptive_outline_exposes_structured_omitted_ranges() {
        let source = concat!(
            "# Heading\n",
            "intro\n",
            "body one\n",
            "body two\n",
            "body three\n",
            "body four\n",
            "body five\n",
            "# Next\n",
            "tail\n",
        );
        let mut read = request(CompressionMode::Adaptive);
        read.target_tokens = 12;
        read.hard_tokens = 30;
        read.target_bytes = 80;
        read.hard_bytes = 160;
        let result = compress(source, &read).unwrap();
        if result.mode == CompressionMode::Adaptive {
            let omitted = result
                .segments
                .iter()
                .find(|segment| segment.kind == SegmentKind::Omitted)
                .expect("adaptive output should expose an omitted segment");
            let cursor = omitted.cursor.as_ref().expect("omitted cursor");
            assert_eq!(cursor.byte_start, omitted.start_byte);
            assert_eq!(cursor.byte_end, omitted.end_byte);
            assert!(result.content.contains("omitted"));
        }
    }

    #[test]
    fn cursor_line_and_byte_ranges_are_consistent() {
        let source = "a\nb\nc\nd\n";
        let mut read = request(CompressionMode::Raw);
        read.target_tokens = 2;
        read.hard_tokens = 2;
        read.target_bytes = 4;
        read.hard_bytes = 4;
        let result = compress(source, &read).unwrap();
        let next = result.next.expect("page continuation");
        assert_eq!(next.offset, 3);
        assert_eq!(next.byte_start, 4);
        assert_eq!(&source[next.byte_start..next.byte_end], "c\nd\n");
    }

    #[test]
    fn token_estimate_handles_utf8_without_panic() {
        assert!(estimate_tokens("中文内容") >= 2);
        assert!(estimate_tokens("a".repeat(100).as_str()) >= 25);
    }

    #[test]
    fn structured_code_uses_declarations_and_short_context() {
        let mut source = String::from("use crate::Thing;\n\n");
        for index in 0..12 {
            source.push_str(&format!("pub fn task_{index}() {{\n"));
            for body_line in 0..36 {
                source.push_str(&format!("    let value_{body_line} = {body_line};\n"));
            }
            source.push_str("}\n\n");
        }
        let mut read = request(CompressionMode::Adaptive);
        read.target_tokens = 1_600;
        read.hard_tokens = 3_200;
        read.target_bytes = 6_400;
        read.hard_bytes = 12_800;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Code);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.content.contains("pub fn task_0"));
        assert!(result.content.contains("omitted"));
    }

    #[test]
    fn config_keeps_shallow_keys_without_retaining_every_brace() {
        let mut source = String::from("{\n  \"services\": {\n");
        for index in 0..160 {
            source.push_str(&format!(
                "    \"service_{index}\": {{ \"enabled\": true, \"port\": {index} }},\n"
            ));
        }
        source.push_str("  },\n  \"version\": 1\n}\n");
        let mut read = request(CompressionMode::Adaptive);
        read.target_tokens = 700;
        read.hard_tokens = 1_400;
        read.target_bytes = 2_800;
        read.hard_bytes = 5_600;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Config);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.content.contains("services"));
        assert!(result.content.contains("omitted"));
    }

    #[test]
    fn long_logs_collapse_to_edges_and_one_contiguous_omission() {
        let source = (0..2_000)
            .map(|index| format!("2026-07-23 request-{index:04} status=200\n"))
            .collect::<String>();
        let mut read = request(CompressionMode::Auto);
        read.target_tokens = 400;
        read.hard_tokens = 800;
        read.target_bytes = 1_600;
        read.hard_bytes = 3_200;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Text);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert_eq!(
            result
                .segments
                .iter()
                .filter(|segment| segment.kind == SegmentKind::Omitted)
                .count(),
            1
        );
    }

    #[test]
    fn render_text_exposes_only_continuation_metadata_when_truncated() {
        let source = (0..2_000)
            .map(|index| format!("2026-07-23 request-{index:04} status=200\n"))
            .collect::<String>();
        let rendered = render_text(
            std::path::Path::new("service.log"),
            &source,
            "auto",
            None,
            None,
            800,
            3_200,
            Some("fnv1a:test"),
            None,
        )
        .unwrap();
        assert!(rendered.text.contains("<read_metadata>"));
        assert!(rendered.text.contains("snapshot_id"));
        assert!(rendered.text.contains("next_cursor"));
        assert!(
            rendered.text.contains("<read_warning>"),
            "truncated output must carry a lossy-view caution"
        );
        assert!(
            rendered.text.contains("断章取义"),
            "caution must tell the model not to cite the partial view"
        );
        let warning_pos = rendered.text.find("<read_warning>").expect("warning tag");
        let metadata_pos = rendered.text.find("<read_metadata>").expect("metadata tag");
        assert!(
            warning_pos < metadata_pos,
            "caution must precede the cursor block"
        );
        let metadata = rendered
            .text
            .split_once("<read_metadata>")
            .and_then(|(_, value)| value.split_once("</read_metadata>"))
            .map(|(value, _)| value)
            .expect("continuation metadata block");
        let metadata: Value = serde_json::from_str(metadata).unwrap();
        let cursor = metadata["next_cursor"].as_str().expect("cursor string");
        let _: ReadCursor = serde_json::from_str(cursor).unwrap();
        assert!(rendered.text.len() <= 3_200);
        assert!(estimate_tokens(&rendered.text) <= 800);
    }

    #[test]
    fn config_yaml_compresses_under_tight_budget() {
        // YAML maps every leaf onto a `key: value` line, so a large file is a
        // forest of short repeatable runs separated by shallow section lines.
        // The adaptive path must still collapse it instead of falling back to a
        // raw page when the budget is tight.
        let mut source = String::from("services:\n");
        for block in 0..280 {
            source.push_str(&format!("  block_{block}:\n"));
            for entry in 0..8 {
                source.push_str(&format!(
                    "    key_{entry}: value_{entry}_with_some_padding_text_here\n"
                ));
            }
        }
        let mut read = request(CompressionMode::Adaptive);
        read.path = Some("big-config.yaml".into());
        read.target_tokens = 2_000;
        read.hard_tokens = 4_000;
        read.target_bytes = 6_000;
        read.hard_bytes = 12_000;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Config);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.output_bytes < result.raw_selected_bytes / 2);
        assert!(
            result
                .segments
                .iter()
                .any(|segment| segment.kind == SegmentKind::Omitted),
            "YAML should produce at least one omitted range"
        );
    }

    #[test]
    fn adaptive_output_warns_about_omitted_ranges() {
        let mut source = String::from("use crate::Thing;\n\n");
        for index in 0..40 {
            source.push_str(&format!("pub fn task_{index}() {{\n"));
            for body in 0..20 {
                source.push_str(&format!("    let value_{body} = {body};\n"));
            }
            source.push_str("}\n\n");
        }
        let rendered = render_text(
            std::path::Path::new("tasks.rs"),
            &source,
            "adaptive",
            None,
            None,
            1_500,
            6_000,
            None,
            None,
        )
        .unwrap();
        assert!(rendered.text.contains("<read_warning>"));
        assert!(
            rendered.text.contains("压缩视图"),
            "omission-bearing output must call itself a compressed view"
        );
        assert!(
            rendered.text.contains("断章取义"),
            "caution must warn against citing the partial view"
        );
        assert!(
            rendered.details["segments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|segment| segment["kind"] == "omitted"),
            "adaptive output should carry omitted ranges"
        );
    }

    #[test]
    fn dense_small_code_methods_compress_without_losing_signatures() {
        // A forest of tiny methods defeats a fixed 17-line trailing context:
        // every signature keeps its whole body, the outline overflows budget,
        // and compression used to fall back to a raw page. The retry loop must
        // tighten the anchor set so the view fits while still retaining the
        // type definitions and method signatures an agent needs for navigation.
        let mut source = String::from("#include <stdio.h>\n\n");
        for record in 0..10 {
            source.push_str(&format!(
                "typedef struct {{ int id; int value; }} Record{record};\n\n"
            ));
            for method in 0..8 {
                source.push_str(&format!(
                    "static int process_{record}_{method}(Record{record}* r) {{\n"
                ));
                for step in 0..6 {
                    source.push_str(&format!("    if (r->value == {step}) return {step};\n"));
                }
                source.push_str("    return r->value;\n}\n\n");
            }
        }
        let mut read = request(CompressionMode::Adaptive);
        read.path = Some("records.c".into());
        read.target_tokens = 1_500;
        read.hard_tokens = 3_000;
        read.target_bytes = 4_000;
        read.hard_bytes = 8_000;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Code);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.output_bytes < result.raw_selected_bytes / 2);
        assert!(result.output_bytes <= read.hard_bytes);
        assert!(
            result.content.contains("Record0"),
            "type definitions should survive compression"
        );
        assert!(
            result.content.contains("static int process_0_0"),
            "sampled method signatures should survive compression"
        );
        assert!(
            result.content.contains("omitted"),
            "repetitive bodies should be omitted"
        );
    }

    #[test]
    fn shell_nested_function_declarations_are_anchors() {
        // Shell allows bare `name() {` declarations nested inside an outer
        // function body. These must register as structural anchors so the
        // outline keeps each nested handler signature, mirroring how `fn`
        // declarations behave in brace languages.
        let mut source = String::new();
        for outer in 0..40 {
            source.push_str(&format!("function process_{outer}() {{\n"));
            for handler in 0..8 {
                source.push_str(&format!("    handler_{outer}_{handler}() {{\n"));
                for body in 0..6 {
                    source.push_str(&format!("        echo doing {handler} step {body}\n"));
                }
                source.push_str("    }\n");
            }
            source.push_str("}\n\n");
        }
        let mut read = request(CompressionMode::Adaptive);
        read.path = Some("handlers.sh".into());
        read.target_tokens = 6_000;
        read.hard_tokens = 12_000;
        read.target_bytes = 24_000;
        read.hard_bytes = 48_000;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Code);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.content.contains("function process_0"));
        assert!(
            result.content.contains("handler_0_0"),
            "nested bare handler declaration should be retained as an anchor"
        );
        assert!(
            result
                .segments
                .iter()
                .any(|segment| segment.kind == SegmentKind::Omitted),
            "shell outline should omit repetitive bodies"
        );
    }

    #[test]
    fn dense_markdown_uses_bounded_heading_sampling() {
        let mut source = String::new();
        for index in 0..1_900 {
            if index % 2 == 0 {
                source.push_str(&format!("## Section {index}\n"));
            } else {
                source.push_str(&format!("Paragraph {index}\n"));
            }
        }
        let mut read = request(CompressionMode::Adaptive);
        read.path = Some("big-doc.md".into());
        read.target_tokens = 2_000;
        read.hard_tokens = 4_000;
        read.target_bytes = 8_000;
        read.hard_bytes = 16_000;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.format, FileKind::Markdown);
        assert_eq!(result.mode, CompressionMode::Adaptive);
        assert!(result.content.contains("## Section"));
        assert!(result.content.contains("omitted"));
        assert!(result.output_bytes <= read.target_bytes);
    }

    #[test]
    fn omitted_markers_carry_a_content_label() {
        let mut source = String::from("use crate::Thing;\n\n");
        for index in 0..40 {
            source.push_str(&format!("pub fn task_{index}() {{\n"));
            for body in 0..20 {
                source.push_str(&format!("    let value_{body} = {body};\n"));
            }
            source.push_str("}\n\n");
        }
        let mut read = request(CompressionMode::Adaptive);
        read.path = Some("tasks.rs".into());
        read.target_tokens = 1_125;
        read.hard_tokens = 1_500;
        read.target_bytes = 4_500;
        read.hard_bytes = 6_000;
        let result = compress(&source, &read).unwrap();
        assert_eq!(result.mode, CompressionMode::Adaptive);
        let segment = result
            .segments
            .iter()
            .find(|segment| segment.kind == SegmentKind::Omitted)
            .expect("omitted segment");
        let label = segment.label.as_deref().expect("omitted segment label");
        assert!(
            result.content.contains(&format!("omitted: {label}]")),
            "marker should embed the label: {}",
            result.content
        );
        let omitted_text = &source[segment.start_byte..segment.end_byte];
        let first_line = omitted_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .expect("a non-empty omitted line");
        assert!(
            first_line.starts_with(label.trim_end_matches('…')),
            "label must quote the first omitted line: {label} vs {first_line}"
        );
    }

    #[test]
    fn metadata_omitted_entries_and_warning_guide_recovery() {
        let mut source = String::from("use crate::Thing;\n\n");
        for index in 0..40 {
            source.push_str(&format!("pub fn task_{index}() {{\n"));
            for body in 0..20 {
                source.push_str(&format!("    let value_{body} = {body};\n"));
            }
            source.push_str("}\n\n");
        }
        let rendered = render_text(
            std::path::Path::new("tasks.rs"),
            &source,
            "adaptive",
            None,
            None,
            1_500,
            6_000,
            None,
            None,
        )
        .unwrap();
        assert!(
            rendered.text.contains("mode=raw"),
            "warning must point at mode=raw for the unabridged view"
        );
        let metadata = rendered
            .text
            .split_once("<read_metadata>")
            .and_then(|(_, rest)| rest.split_once("</read_metadata>"))
            .map(|(json_text, _)| json_text)
            .expect("read metadata block");
        let metadata: serde_json::Value = serde_json::from_str(metadata).unwrap();
        let entry = metadata["omitted"]
            .as_array()
            .and_then(|entries| entries.first())
            .expect("at least one omitted cursor");
        assert!(
            entry["label"]
                .as_str()
                .is_some_and(|label| !label.is_empty()),
            "omitted cursor entries should carry a label hint: {entry}"
        );
        assert!(
            rendered.details["segments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|segment| segment["kind"] == "omitted" && segment.get("label").is_some()),
            "serialized segments should expose labels for omitted ranges"
        );
    }
}
