//! Virtualized transcript with block-granularity rows.
//!
//! One row is one markdown block (paragraph, heading or fenced code), not one
//! message, so a streamed token re-measures only the live tail row. Finished
//! blocks are frozen into `SharedString`s and never copied again; the tail is a
//! single growing `String` snapshotted at most once per frame.

use std::{path::PathBuf, time::Duration};

use gpui::{
    AnyElement, Context, FontWeight, ListAlignment, ListState, SharedString, Window, div, list,
    prelude::*, px, rgb,
};

/// Coalesce streamed deltas into at most one re-layout per frame.
const FRAME: Duration = Duration::from_millis(16);

#[cfg(target_os = "macos")]
const MONO: &str = "Menlo";
#[cfg(target_os = "windows")]
const MONO: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO: &str = "DejaVu Sans Mono";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Heading,
    Code,
}

struct Block {
    kind: BlockKind,
    text: SharedString,
}

/// Splits streamed markdown into finished blocks plus one live tail. Pure, so
/// it is unit-tested without a window.
#[derive(Default)]
pub struct BlockSplitter {
    blocks: Vec<Block>,
    /// Live, not yet finished block.
    tail: String,
    in_fence: bool,
    /// Byte offset in `tail` up to which complete lines were already scanned.
    scanned: usize,
}

pub struct Transcript {
    doc: BlockSplitter,
    tail_snapshot: SharedString,
    /// Rows the list currently knows about.
    rows: usize,
    /// First row whose content changed since the last flush.
    dirty_from: Option<usize>,
    flush_scheduled: bool,
    list_state: ListState,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            doc: BlockSplitter::default(),
            tail_snapshot: SharedString::default(),
            rows: 0,
            dirty_from: None,
            flush_scheduled: false,
            list_state: ListState::new(0, ListAlignment::Bottom, px(400.)),
        }
    }

    pub fn start_replay(
        &mut self,
        path: PathBuf,
        start: Duration,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let deltas = match load_replay(&path) {
            Ok(deltas) => deltas,
            Err(err) => {
                eprintln!("blongo: cannot load replay {}: {err:#}", path.display());
                return;
            }
        };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(start).await;
            for text in deltas {
                if this.update(cx, |this, cx| this.append(&text, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(delay).await;
            }
            eprintln!("blongo: replay done");
        })
        .detach();
    }

    pub fn follow_agent(
        &mut self,
        mut events: tokio::sync::mpsc::UnboundedReceiver<blongo_protocol::AgentEvent>,
        cx: &mut Context<Self>,
    ) {
        use blongo_protocol::AgentEvent;
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                match event {
                    AgentEvent::TextDelta { text } => {
                        if this.update(cx, |this, cx| this.append(&text, cx)).is_err() {
                            return;
                        }
                    }
                    AgentEvent::TurnCompleted { status } => {
                        eprintln!("blongo: replay done ({status:?})");
                    }
                    AgentEvent::Error { message } => eprintln!("blongo: agent error: {message}"),
                    _ => {}
                }
            }
        })
        .detach();
    }

    pub fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        let tail_row = self.doc.blocks.len();
        self.doc.push(text);
        self.dirty_from = Some(self.dirty_from.map_or(tail_row, |d| d.min(tail_row)));
        if !self.flush_scheduled {
            self.flush_scheduled = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FRAME).await;
                this.update(cx, |this, cx| this.flush(cx)).ok();
            })
            .detach();
        }
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.flush_scheduled = false;
        let Some(from) = self.dirty_from.take() else {
            return;
        };
        self.tail_snapshot = SharedString::from(self.doc.tail.clone());
        let rows = self.doc.rows();
        let from = from.min(self.rows);
        self.list_state.splice(from..self.rows, rows - from);
        self.rows = rows;
        cx.notify();
    }

    fn render_row(&mut self, ix: usize, _window: &mut Window, _cx: &mut Context<Self>) -> AnyElement {
        let doc = &self.doc;
        let (kind, text) = match doc.blocks.get(ix) {
            Some(block) => (block.kind, block.text.clone()),
            None => {
                let kind = if doc.in_fence {
                    BlockKind::Code
                } else if doc.tail.starts_with('#') {
                    BlockKind::Heading
                } else {
                    BlockKind::Paragraph
                };
                (kind, self.tail_snapshot.clone())
            }
        };
        let row = div().w_full().px_6().py_1p5();
        match kind {
            BlockKind::Paragraph => row.text_sm().child(text),
            BlockKind::Heading => row
                .pt_4()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(text.trim_start_matches('#').trim().to_owned())),
            BlockKind::Code => row.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(0x141417))
                    .border_1()
                    .border_color(rgb(0x2a2a2e))
                    .font_family(MONO)
                    .text_xs()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .children(text.lines().map(|l| div().child(SharedString::from(l.to_owned())))),
            ),
        }
        .into_any_element()
    }
}

impl BlockSplitter {
    pub fn push(&mut self, text: &str) {
        self.tail.push_str(text);
        self.split_finished_blocks();
    }

    /// Finished blocks plus the live tail, if it has any content.
    pub fn rows(&self) -> usize {
        self.blocks.len() + usize::from(!self.tail.trim().is_empty())
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
        self.blocks.push(Block {
            kind,
            text: SharedString::from(trimmed.to_owned()),
        });
    }
}

impl Render for Transcript {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(
            list(self.list_state.clone(), cx.processor(Self::render_row))
                .size_full()
                .py_4(),
        )
    }
}

/// Text deltas from a zeron-format replay journal (`{"event":{"type":"textDelta","text":..}}`).
fn load_replay(path: &PathBuf) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)?;
        let event = &value["event"];
        if event["type"] == "textDelta" {
            if let Some(text) = event["text"].as_str() {
                out.push(text.to_owned());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(doc: &BlockSplitter) -> Vec<(BlockKind, &str)> {
        doc.blocks.iter().map(|b| (b.kind, b.text.as_ref())).collect()
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
                (BlockKind::Heading, "# Title"),
                (BlockKind::Paragraph, "First para\nstill first."),
                (BlockKind::Code, "```rust\nfn main() {\n\n}\n```"),
                (BlockKind::Paragraph, "After code"),
            ]
        );
        assert_eq!(doc.tail, "tail");
        assert_eq!(doc.rows(), 5);
    }

    #[test]
    fn open_fence_stays_in_tail() {
        let mut doc = BlockSplitter::default();
        doc.push("intro\n```\ncode\n\nmore");
        assert_eq!(kinds(&doc), vec![(BlockKind::Paragraph, "intro")]);
        assert!(doc.in_fence);
        assert_eq!(doc.tail, "```\ncode\n\nmore");
    }
}
