//! Streaming Markdown split into blocks (paragraph, heading, fenced code).
//!
//! One timeline row is one block, so a streamed token re-measures only the
//! live tail row. Finished blocks are frozen once (display text + inline
//! highlights) and never re-parsed. Inline Markdown (emphasis, code spans,
//! links, strikethrough, lists, task lists) is parsed with pulldown-cmark;
//! fenced code keeps its language for lazy syntax highlighting.
//!
//! The live tail is rendered cheaply ([`LiveTail`]): its complete lines are
//! parsed once each, the line being typed is shown as it arrives, and code
//! lines are cut into strings once. A streamed token therefore costs work
//! proportional to the current line, not to the whole tail.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use gpui::SharedString;
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::highlight::{self, Lang};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading,
    Code,
}

/// Inline styles; several may cover the same text (bold inside a link).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    pub strike: bool,
}

/// Syntax highlighting of a code block, computed at most once, lazily.
#[derive(Default)]
pub struct CodeHighlight {
    started: AtomicBool,
    spans: OnceLock<Arc<highlight::Spans>>,
}

impl CodeHighlight {
    /// True exactly once: the caller should compute the spans now.
    pub fn claim(&self) -> bool {
        !self.started.swap(true, Ordering::Relaxed)
    }

    pub fn set(&self, spans: highlight::Spans) {
        let _ = self.spans.set(Arc::new(spans));
    }

    pub fn get(&self) -> Option<&Arc<highlight::Spans>> {
        self.spans.get()
    }
}

#[derive(Clone)]
pub struct Block {
    pub kind: BlockKind,
    /// Display text: markers stripped (code: fence lines removed).
    pub text: SharedString,
    /// Non-overlapping, sorted.
    pub inline: Arc<[(Range<usize>, Inline)]>,
    /// Code: the fence's language, when Blongo can highlight it.
    pub lang: Option<Lang>,
    /// Code: shared by every snapshot of a finished block.
    pub highlight: Option<Arc<CodeHighlight>>,
    /// Code: one display string per line (built once, cloned per frame).
    pub lines: Arc<[SharedString]>,
}

impl Block {
    /// `finished`: the block will not change any more (only those are
    /// highlighted).
    pub fn new(kind: BlockKind, raw: &str, finished: bool) -> Self {
        match kind {
            BlockKind::Code => {
                let lang = raw
                    .lines()
                    .next()
                    .and_then(|l| l.trim_start().strip_prefix("```"))
                    .and_then(Lang::from_info);
                let text = SharedString::from(code_body(raw));
                Self {
                    kind,
                    lines: text.split('\n').map(|l| l.to_owned().into()).collect(),
                    text,
                    inline: Arc::new([]),
                    lang,
                    highlight: (finished && lang.is_some()).then(Default::default),
                }
            }
            BlockKind::Heading => Self::text(kind, raw.trim_start_matches('#').trim()),
            BlockKind::Paragraph => Self::text(kind, raw.trim_start_matches([' ', '\n'])),
        }
    }

    fn text(kind: BlockKind, src: &str) -> Self {
        let (text, inline) = parse_inline(src);
        Self {
            kind,
            text: text.into(),
            inline: inline.into(),
            lang: None,
            highlight: None,
            lines: Arc::new([]),
        }
    }

    /// Compute the highlight now (call off the UI thread).
    pub fn compute_highlight(lang: Lang, text: &str, target: &CodeHighlight) {
        target.set(highlight::highlight(lang, text));
    }
}

/// Inline style ranges of a block.
type Spans = Arc<[(Range<usize>, Inline)]>;

/// The live tail block, kept up to date per flush at a cost proportional to
/// the line being streamed.
///
/// - Paragraph / heading: the complete lines are parsed with
///   [`parse_inline`] once (again only when a line completes); the partial
///   last line is appended as plain text until its line ends.
/// - Code: complete lines are kept as strings and only the partial last
///   line is rebuilt. `text` stays empty (live code is never highlighted).
#[derive(Default)]
pub struct LiveTail {
    kind: Option<BlockKind>,
    /// Paragraph: byte length of the parsed complete-lines prefix (of the
    /// tail with leading blank space removed) and its parse.
    parsed_len: usize,
    parsed: (String, Spans),
    /// Code: complete body lines and the body bytes they cover.
    code_lines: Vec<SharedString>,
    code_len: usize,
    lang: Option<Lang>,
}

impl LiveTail {
    pub fn block(&mut self, kind: BlockKind, raw: &str) -> Block {
        if self.kind != Some(kind) {
            *self = Self {
                kind: Some(kind),
                ..Self::default()
            };
        }
        match kind {
            BlockKind::Code => self.code(raw),
            BlockKind::Heading => Block {
                kind,
                text: raw.trim_start_matches('#').trim().to_owned().into(),
                inline: Arc::new([]),
                lang: None,
                highlight: None,
                lines: Arc::new([]),
            },
            BlockKind::Paragraph => self.paragraph(raw.trim_start_matches([' ', '\n'])),
        }
    }

    fn paragraph(&mut self, src: &str) -> Block {
        let complete = src.rfind('\n').unwrap_or(0);
        if complete != self.parsed_len {
            let (text, inline) = parse_inline(&src[..complete]);
            self.parsed = (text, inline.into());
            self.parsed_len = complete;
        }
        let partial = src[complete..].trim_start_matches('\n');
        let mut text = String::with_capacity(self.parsed.0.len() + 1 + partial.len());
        text.push_str(&self.parsed.0);
        if !text.is_empty() && !partial.is_empty() {
            text.push('\n');
        }
        text.push_str(partial);
        Block {
            kind: BlockKind::Paragraph,
            text: text.into(),
            inline: self.parsed.1.clone(),
            lang: None,
            highlight: None,
            lines: Arc::new([]),
        }
    }

    fn code(&mut self, raw: &str) -> Block {
        // The tail starts at its opening fence line.
        let (fence, body) = match raw.find('\n') {
            Some(nl) => (&raw[..nl], &raw[nl + 1..]),
            None => (raw, ""),
        };
        if self.code_len == 0 && self.code_lines.is_empty() {
            self.lang = fence
                .trim_start()
                .strip_prefix("```")
                .and_then(Lang::from_info);
        }
        if body.len() < self.code_len {
            self.code_lines.clear();
            self.code_len = 0;
        }
        while let Some(nl) = body[self.code_len..].find('\n') {
            let line = &body[self.code_len..self.code_len + nl];
            self.code_lines.push(line.to_owned().into());
            self.code_len += nl + 1;
        }
        let mut lines = self.code_lines.clone();
        let partial = &body[self.code_len..];
        if !partial.is_empty() {
            lines.push(partial.to_owned().into());
        }
        Block {
            kind: BlockKind::Code,
            text: SharedString::default(),
            inline: Arc::new([]),
            lang: self.lang,
            highlight: None,
            lines: lines.into(),
        }
    }
}

/// Splits streamed markdown into finished blocks plus one live tail.
#[derive(Default)]
pub struct BlockSplitter {
    pub blocks: Vec<Block>,
    /// Live, not yet finished block.
    pub tail: String,
    pub in_fence: bool,
    /// Byte offset in `tail` up to which complete lines were already scanned.
    scanned: usize,
}

impl BlockSplitter {
    pub fn from_text(text: &str) -> Self {
        let mut doc = Self::default();
        doc.push(text);
        doc
    }

    pub fn push(&mut self, text: &str) {
        self.tail.push_str(text);
        self.split_finished_blocks();
    }

    /// Finished blocks plus the live tail, if it has any content.
    pub fn rows(&self) -> usize {
        self.blocks.len() + usize::from(self.has_tail())
    }

    pub fn has_tail(&self) -> bool {
        !self.tail.trim().is_empty()
    }

    /// Kind of the live tail block.
    pub fn tail_kind(&self) -> BlockKind {
        if self.in_fence {
            BlockKind::Code
        } else if self.tail.starts_with('#') {
            BlockKind::Heading
        } else {
            BlockKind::Paragraph
        }
    }

    /// Freeze the tail (end of stream).
    pub fn finish(&mut self) {
        let kind = self.tail_kind();
        let end = self.tail.len();
        self.finish_block(end, kind);
        self.in_fence = false;
    }

    /// Move every finished block out of `tail`. Only lines not yet scanned are
    /// looked at, so a long tail is not rescanned on every delta.
    fn split_finished_blocks(&mut self) {
        while let Some(nl) = self.tail[self.scanned..].find('\n') {
            let line_start = self.scanned;
            let line_end = line_start + nl + 1;
            let line = &self.tail[line_start..line_end];
            let fence = line.trim_start().starts_with("```");
            if self.in_fence {
                self.scanned = line_end;
                if fence {
                    self.in_fence = false;
                    self.finish_block(line_end, BlockKind::Code);
                }
            } else if fence {
                // A fence opens a new block; whatever came before is finished.
                self.finish_block(line_start, BlockKind::Paragraph);
                self.in_fence = true;
                // `tail` now starts at the fence line.
                self.scanned = self.tail.find('\n').map_or(0, |i| i + 1);
            } else if line.trim().is_empty() {
                self.finish_block(line_end, BlockKind::Paragraph);
            } else {
                self.scanned = line_end;
            }
        }
    }

    fn finish_block(&mut self, end: usize, kind: BlockKind) {
        let rest = self.tail.split_off(end);
        let text = std::mem::replace(&mut self.tail, rest);
        self.scanned = 0;
        let trimmed = text.trim_end();
        if trimmed.trim().is_empty() {
            return;
        }
        let kind = if kind == BlockKind::Paragraph && trimmed.starts_with('#') {
            BlockKind::Heading
        } else {
            kind
        };
        self.blocks.push(Block::new(kind, trimmed, true));
    }
}

/// Code block body without its fence lines.
pub fn code_body(raw: &str) -> String {
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines
        .first()
        .is_some_and(|l| l.trim_start().starts_with("```"))
    {
        lines.remove(0);
    }
    if lines
        .last()
        .is_some_and(|l| l.trim_start().starts_with("```"))
    {
        lines.pop();
    }
    lines.join("\n")
}

/// Render inline Markdown to display text plus non-overlapping style
/// ranges. List items become `• ` / `1. ` lines; soft breaks stay line
/// breaks (agents format replies line by line).
pub fn parse_inline(src: &str) -> (String, Vec<(Range<usize>, Inline)>) {
    let mut out = String::with_capacity(src.len());
    // Open style spans: (start, style).
    let mut raw: Vec<(Range<usize>, Inline)> = Vec::new();
    let mut open: Vec<(usize, Inline)> = Vec::new();
    // Per list nesting level: the next ordinal (`None`: bullets).
    let mut lists: Vec<Option<u64>> = Vec::new();
    // Right after a bullet, a (loose list) paragraph must not break the line.
    let mut at_item_start = false;
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let newline = |out: &mut String| {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    for event in Parser::new_ext(src, options) {
        let item_start = std::mem::take(&mut at_item_start);
        match event {
            Event::Start(Tag::Paragraph) if item_start => at_item_start = true,
            Event::TaskListMarker(done) => {
                out.push_str(if done { "☑ " } else { "☐ " });
                at_item_start = item_start;
            }
            Event::Start(Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_)) => {
                newline(&mut out)
            }
            Event::Start(Tag::List(first)) => {
                newline(&mut out);
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                newline(&mut out);
                let depth = lists.len().saturating_sub(1);
                out.extend(std::iter::repeat_n("  ", depth));
                match lists.last_mut() {
                    Some(Some(n)) => {
                        out.push_str(&format!("{n}. "));
                        *n += 1;
                    }
                    _ => out.push_str("• "),
                }
                at_item_start = true;
            }
            Event::Start(Tag::Strong) => open.push((
                out.len(),
                Inline {
                    bold: true,
                    ..Default::default()
                },
            )),
            Event::Start(Tag::Emphasis) => open.push((
                out.len(),
                Inline {
                    italic: true,
                    ..Default::default()
                },
            )),
            Event::Start(Tag::Strikethrough) => open.push((
                out.len(),
                Inline {
                    strike: true,
                    ..Default::default()
                },
            )),
            Event::Start(Tag::Link { .. }) => open.push((
                out.len(),
                Inline {
                    link: true,
                    ..Default::default()
                },
            )),
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                if let Some((start, style)) = open.pop()
                    && start < out.len()
                {
                    raw.push((start..out.len(), style));
                }
            }
            Event::Code(text) => {
                let start = out.len();
                out.push_str(&text);
                raw.push((
                    start..out.len(),
                    Inline {
                        code: true,
                        ..Default::default()
                    },
                ));
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => {
                newline(&mut out);
                out.push_str("───");
            }
            _ => {}
        }
    }
    (out, flatten(raw))
}

/// Split possibly nested style ranges into sorted, non-overlapping ranges
/// whose style is the union of everything covering them.
fn flatten(raw: Vec<(Range<usize>, Inline)>) -> Vec<(Range<usize>, Inline)> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut cuts: Vec<usize> = raw.iter().flat_map(|(r, _)| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<(Range<usize>, Inline)> = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let mut style = Inline::default();
        let mut covered = false;
        for (r, s) in &raw {
            if r.start <= a && b <= r.end {
                covered = true;
                style.bold |= s.bold;
                style.italic |= s.italic;
                style.code |= s.code;
                style.link |= s.link;
                style.strike |= s.strike;
            }
        }
        if covered {
            match out.last_mut() {
                Some((r, s)) if r.end == a && *s == style => r.end = b,
                _ => out.push((a..b, style)),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(doc: &BlockSplitter) -> Vec<(BlockKind, &str)> {
        doc.blocks
            .iter()
            .map(|b| (b.kind, b.text.as_ref()))
            .collect()
    }

    #[test]
    fn splits_paragraphs_headings_and_fences_across_arbitrary_deltas() {
        let text = "# Title\n\nFirst para\nstill first.\n\n```rust\nfn main() {\n\n}\n```\nAfter code\n\ntail";
        // Feed one byte at a time to exercise every split point.
        let mut doc = BlockSplitter::default();
        for ch in text.chars() {
            doc.push(&ch.to_string());
        }
        assert_eq!(
            kinds(&doc),
            vec![
                (BlockKind::Heading, "Title"),
                (BlockKind::Paragraph, "First para\nstill first."),
                (BlockKind::Code, "fn main() {\n\n}"),
                (BlockKind::Paragraph, "After code"),
            ]
        );
        assert_eq!(doc.tail, "tail");
        assert_eq!(doc.rows(), 5);
        doc.finish();
        assert_eq!(doc.rows(), 5);
        assert_eq!(doc.blocks[4].text.as_ref(), "tail");
    }

    #[test]
    fn open_fence_stays_in_tail() {
        let mut doc = BlockSplitter::default();
        doc.push("intro\n```\ncode\n\nmore");
        assert_eq!(kinds(&doc), vec![(BlockKind::Paragraph, "intro")]);
        assert!(doc.in_fence);
        assert_eq!(doc.tail_kind(), BlockKind::Code);
        assert_eq!(doc.tail, "```\ncode\n\nmore");
        assert_eq!(code_body(&doc.tail), "code\n\nmore");
    }

    fn bold() -> Inline {
        Inline {
            bold: true,
            ..Default::default()
        }
    }

    fn code() -> Inline {
        Inline {
            code: true,
            ..Default::default()
        }
    }

    #[test]
    fn inline_markers_become_highlights() {
        let (text, spans) = parse_inline("Use **bold** and `code` here");
        assert_eq!(text, "Use bold and code here");
        assert_eq!(spans, vec![(4..8, bold()), (13..17, code())]);
        let (text, spans) = parse_inline("- one\n  - two `x`\n\n1. first\n2. second");
        assert_eq!(text, "• one\n  • two x\n1. first\n2. second");
        assert_eq!(spans, vec![(18..19, code())]);
        let (text, _) = parse_inline("not ** closed");
        assert_eq!(text, "not ** closed");
        let (text, _) = parse_inline("界 `界` **界**");
        assert_eq!(text, "界 界 界");
    }

    #[test]
    fn nested_styles_are_flattened() {
        let (text, spans) = parse_inline("[a **b** c](http://x) ~~d~~ - [x] e");
        assert_eq!(text, "a b c d - [x] e");
        let link = Inline {
            link: true,
            ..Default::default()
        };
        let link_bold = Inline {
            link: true,
            bold: true,
            ..Default::default()
        };
        let strike = Inline {
            strike: true,
            ..Default::default()
        };
        assert_eq!(
            spans,
            vec![
                (0..2, link),
                (2..3, link_bold),
                (3..5, link),
                (6..7, strike)
            ]
        );
        let (text, _) = parse_inline("- [x] done\n- [ ] todo");
        assert_eq!(text, "• ☑ done\n• ☐ todo");
    }

    #[test]
    fn code_blocks_keep_their_language_and_highlight_lazily() {
        let mut doc = BlockSplitter::default();
        doc.push("```rust\nfn main() {}\n```\n```\nplain\n```\n");
        let rust = &doc.blocks[0];
        assert_eq!(rust.lang, Some(Lang::Rust));
        let hl = rust
            .highlight
            .clone()
            .expect("finished code is highlightable");
        assert!(hl.get().is_none());
        assert!(hl.claim());
        assert!(!hl.claim(), "claimed once");
        Block::compute_highlight(Lang::Rust, &rust.text, &hl);
        assert!(!hl.get().unwrap().is_empty());
        assert!(doc.blocks[1].lang.is_none() && doc.blocks[1].highlight.is_none());
        // A live tail is never highlighted.
        doc.push("```rust\nlet");
        let tail = LiveTail::default().block(doc.tail_kind(), &doc.tail);
        assert!(tail.highlight.is_none());
        assert_eq!(tail.lang, Some(Lang::Rust));
    }

    fn lines(block: &Block) -> Vec<&str> {
        block.lines.iter().map(|l| l.as_ref()).collect()
    }

    #[test]
    fn live_tail_parses_complete_lines_and_shows_the_partial_one() {
        let mut live = LiveTail::default();
        let b = live.block(BlockKind::Paragraph, "\nUse **bold**");
        assert_eq!(
            b.text.as_ref(),
            "Use **bold**",
            "the partial line is shown raw"
        );
        assert!(b.inline.is_empty());
        let b = live.block(BlockKind::Paragraph, "\nUse **bold**\nand `co");
        assert_eq!(b.text.as_ref(), "Use bold\nand `co");
        assert_eq!(&b.inline[..], &[(4..8, bold())]);
        let b = live.block(BlockKind::Paragraph, "\nUse **bold**\nand `code`\n");
        assert_eq!(b.text.as_ref(), "Use bold\nand code");
        // Same as a full parse of the finished block.
        let full = Block::new(BlockKind::Paragraph, "Use **bold**\nand `code`", true);
        assert_eq!(b.text, full.text);
        assert_eq!(b.inline, full.inline);
        let b = live.block(BlockKind::Heading, "## Ti");
        assert_eq!(b.text.as_ref(), "Ti");
    }

    #[test]
    fn live_code_tail_builds_lines_incrementally() {
        let mut live = LiveTail::default();
        assert!(live.block(BlockKind::Code, "```py").lines.is_empty());
        let b = live.block(BlockKind::Code, "```py\ndef f():\n    re");
        assert_eq!(lines(&b), vec!["def f():", "    re"]);
        assert_eq!(b.lang, Some(Lang::Python));
        let b = live.block(BlockKind::Code, "```py\ndef f():\n    return 1\n");
        assert_eq!(lines(&b), vec!["def f():", "    return 1"]);
        let full = Block::new(BlockKind::Code, "```py\ndef f():\n    return 1\n```", true);
        assert_eq!(lines(&full), lines(&b));
    }
}
