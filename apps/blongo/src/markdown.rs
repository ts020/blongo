//! Streaming Markdown split into blocks (paragraph, heading, fenced code).
//!
//! One timeline row is one block, so a streamed token re-measures only the
//! live tail row. Finished blocks are frozen once (display text + inline
//! highlights) and never re-parsed. Inline support is deliberately small:
//! `**bold**`, `` `code` `` and list bullets.

use std::ops::Range;
use std::sync::Arc;

use gpui::SharedString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading,
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    Bold,
    Code,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    /// Display text: markers stripped (code: fence lines removed).
    pub text: SharedString,
    pub inline: Arc<[(Range<usize>, Inline)]>,
}

impl Block {
    pub fn new(kind: BlockKind, raw: &str) -> Self {
        match kind {
            BlockKind::Code => Self {
                kind,
                text: SharedString::from(code_body(raw)),
                inline: Arc::new([]),
            },
            BlockKind::Heading => {
                let (text, inline) = parse_inline(raw.trim_start_matches('#').trim());
                Self {
                    kind,
                    text: text.into(),
                    inline: inline.into(),
                }
            }
            BlockKind::Paragraph => {
                let (text, inline) = parse_inline(raw.trim_start_matches([' ', '\n']));
                Self {
                    kind,
                    text: text.into(),
                    inline: inline.into(),
                }
            }
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
        self.blocks.push(Block::new(kind, trimmed));
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

/// Strip `**` / `` ` `` markers and list dashes; return display text and
/// highlight ranges in it.
pub fn parse_inline(src: &str) -> (String, Vec<(Range<usize>, Inline)>) {
    let mut out = String::with_capacity(src.len());
    let mut spans = Vec::new();
    for (ix, line) in src.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let indent = line.len() - line.trim_start().len();
        let rest = &line[indent..];
        let body = if let Some(item) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
            out.push_str(&line[..indent]);
            out.push_str("• ");
            item
        } else {
            line
        };
        let mut chars = body.char_indices().peekable();
        let mut bold_start: Option<usize> = None;
        while let Some((i, c)) = chars.next() {
            if c == '`' {
                if let Some(close) = body[i + 1..].find('`') {
                    let start = out.len();
                    out.push_str(&body[i + 1..i + 1 + close]);
                    spans.push((start..out.len(), Inline::Code));
                    let resume = i + 1 + close + 1;
                    while chars.peek().is_some_and(|(j, _)| *j < resume) {
                        chars.next();
                    }
                    continue;
                }
            } else if c == '*' && body[i..].starts_with("**") {
                chars.next();
                match bold_start.take() {
                    Some(start) => spans.push((start..out.len(), Inline::Bold)),
                    None => {
                        if body[i + 2..].contains("**") {
                            bold_start = Some(out.len());
                        } else {
                            out.push_str("**");
                        }
                    }
                }
                continue;
            }
            out.push(c);
        }
    }
    spans.sort_by_key(|(r, _)| r.start);
    (out, spans)
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

    #[test]
    fn inline_markers_become_highlights() {
        let (text, spans) = parse_inline("Use **bold** and `code` here");
        assert_eq!(text, "Use bold and code here");
        assert_eq!(spans, vec![(4..8, Inline::Bold), (13..17, Inline::Code)]);
        let (text, spans) = parse_inline("- one\n  * two `x`\nnot ** closed");
        assert_eq!(text, "• one\n  • two x\nnot ** closed");
        assert_eq!(spans, vec![(18..19, Inline::Code)]);
        let (text, _) = parse_inline("界 `界` **界**");
        assert_eq!(text, "界 界 界");
    }
}
