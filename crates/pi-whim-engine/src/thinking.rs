//! Splitting assistant text at `<thinking>` boundaries.
//!
//! Pi wraps a model's reasoning in `<thinking>` tags inside the same text as its
//! reply, and the two want rendering differently — reasoning muted, the reply as
//! normal prose. That split is Pi-specific parsing rather than markdown, so it
//! lives here where both presentation layers can use it, instead of in whichever
//! view happened to need it first.

use std::ops::Range;

/// A region of the source document, split at `<thinking>` tag boundaries.
#[derive(Debug)]
pub enum Segment {
    Markdown(Range<usize>),
    Thinking(Range<usize>),
}

/// Split `source` into markdown and thinking segments.
///
/// `<thinking>...</thinking>` blocks become `Segment::Thinking`; everything
/// else stays `Segment::Markdown`. Tags inside fenced code blocks are left
/// literal so they keep rendering as code. An unclosed `<thinking>` (still
/// streaming) captures the rest of the document.
pub fn split_thinking_segments(source: &str) -> Vec<Segment> {
    const OPEN: &str = "<thinking>";
    const CLOSE: &str = "</thinking>";

    let fenced = fenced_ranges(source);
    let in_fence = |offset: usize| fenced.iter().any(|range| range.contains(&offset));

    let mut segments = Vec::new();
    let mut markdown_start = 0;
    let mut cursor = 0;
    while cursor < source.len() {
        let Some(open) = source[cursor..].find(OPEN).map(|index| cursor + index) else {
            break;
        };
        if in_fence(open) {
            cursor = open + OPEN.len();
            continue;
        }
        if markdown_start < open {
            segments.push(Segment::Markdown(markdown_start..open));
        }
        let thinking_start = open + OPEN.len();
        let mut search = thinking_start;
        let close = loop {
            let Some(close) = source[search..].find(CLOSE).map(|index| search + index) else {
                break None;
            };
            if in_fence(close) {
                search = close + CLOSE.len();
                continue;
            }
            break Some(close);
        };
        match close {
            Some(close) => {
                segments.push(Segment::Thinking(thinking_start..close));
                markdown_start = close + CLOSE.len();
                cursor = markdown_start;
            }
            None => {
                segments.push(Segment::Thinking(thinking_start..source.len()));
                return segments;
            }
        }
    }
    if markdown_start < source.len() {
        segments.push(Segment::Markdown(markdown_start..source.len()));
    }
    segments
}

/// Keep a streaming prefix from exposing a half-written thinking tag.
///
/// The typewriter reveals graphemes, but the thinking parser needs the entire
/// marker at once. Only a suffix that the complete message proves is a real tag
/// is withheld, so ordinary text such as `x < y` still streams normally.
pub(crate) fn trim_incomplete_tag<'a>(visible: &'a str, full: &str) -> &'a str {
    if visible.len() >= full.len() || !full.starts_with(visible) {
        return visible;
    }

    for tag in ["<thinking>", "</thinking>"] {
        let longest = visible.len().min(tag.len() - 1);
        for suffix_len in (1..=longest).rev() {
            let start = visible.len() - suffix_len;
            if visible.is_char_boundary(start)
                && tag.starts_with(&visible[start..])
                && full[start..].starts_with(tag)
            {
                return &visible[..start];
            }
        }
    }
    visible
}

/// Byte ranges of fenced code blocks (``` or ~~~), fences included.
///
/// An unclosed opening fence (still streaming) swallows the rest of the
/// document, mirroring how CommonMark would parse the partial input.
fn fenced_ranges(source: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(u8, usize, usize)> = None; // (marker, fence length, block start)
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        let indent = line.len() - trimmed.len();
        let marker = trimmed.as_bytes().first().copied();
        if indent <= 3 && matches!(marker, Some(b'`') | Some(b'~')) {
            let marker = marker.expect("marker checked above");
            let fence_len = trimmed.bytes().take_while(|byte| *byte == marker).count();
            if fence_len >= 3 {
                match open {
                    Some((open_marker, open_len, start))
                        if open_marker == marker && fence_len >= open_len =>
                    {
                        ranges.push(start..offset + line.len());
                        open = None;
                    }
                    None => open = Some((marker, fence_len, offset)),
                    _ => {}
                }
            }
        }
        offset += line.len();
    }
    if let Some((_, _, start)) = open {
        ranges.push(start..source.len());
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_segments_split_closed_blocks() {
        let source = "before\n\n<thinking>\nwhy\n</thinking>\n\nafter";
        let segments = split_thinking_segments(source);
        assert_eq!(segments.len(), 3);
        match (&segments[0], &segments[1], &segments[2]) {
            (Segment::Markdown(a), Segment::Thinking(b), Segment::Markdown(c)) => {
                assert_eq!(&source[a.clone()], "before\n\n");
                assert_eq!(&source[b.clone()], "\nwhy\n");
                assert_eq!(&source[c.clone()], "\n\nafter");
            }
            _ => panic!("unexpected segments: {segments:?}"),
        }
    }

    #[test]
    fn thinking_segments_unclosed_block_runs_to_end() {
        let source = "before\n\n<thinking>\nstill streaming";
        let segments = split_thinking_segments(source);
        assert_eq!(segments.len(), 2);
        match (&segments[0], &segments[1]) {
            (Segment::Markdown(a), Segment::Thinking(b)) => {
                assert_eq!(&source[a.clone()], "before\n\n");
                assert_eq!(&source[b.clone()], "\nstill streaming");
            }
            _ => panic!("unexpected segments"),
        }
    }

    #[test]
    fn thinking_tags_inside_code_fences_stay_markdown() {
        let source = "text\n\n```\n<thinking>\nnot a block\n</thinking>\n```\n\nafter";
        let segments = split_thinking_segments(source);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            Segment::Markdown(range) => assert_eq!(&source[range.clone()], source),
            Segment::Thinking(_) => panic!("expected a single markdown segment"),
        }
    }

    #[test]
    fn thinking_tags_after_unclosed_fence_stay_markdown() {
        let source = "```\nunclosed fence\n<thinking>\nrest";
        let segments = split_thinking_segments(source);
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Markdown(_)));
    }

    #[test]
    fn incomplete_streaming_tags_are_withheld_until_complete() {
        let full = "before<thinking>reason</thinking>after";
        assert_eq!(trim_incomplete_tag("before<thi", full), "before");
        assert_eq!(
            trim_incomplete_tag("before<thinking>reason</thin", full),
            "before<thinking>reason"
        );
        assert_eq!(
            trim_incomplete_tag("before<thinking>", full),
            "before<thinking>"
        );
    }

    #[test]
    fn non_tag_suffixes_are_not_withheld() {
        assert_eq!(trim_incomplete_tag("x <", "x < y"), "x <");
        assert_eq!(
            trim_incomplete_tag("literal <thi", "literal <thing"),
            "literal <thi"
        );
    }
}
