//! One thread's timeline: a virtualized list whose rows are turn items, and
//! for assistant messages one row per Markdown block.
//!
//! Updates arrive as typed core events. Streamed text is appended to the
//! item's block splitter; at most once per frame the changed tail rows are
//! spliced into the list and the view is notified. Nothing here runs while
//! the thread is idle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use blongo_core::CoreClient;
use blongo_protocol::{
    ApprovalDecision, ApprovalState, Command, CommandEnvelope, EventKind, ItemId, ItemKind,
    ThreadId, ThreadSnapshot, ThreadStatus, ToolStatus, TurnItem,
};
use gpui::{
    AnyElement, Context, FollowMode, FontWeight, HighlightStyle, ListAlignment, ListState,
    SharedString, StyledText, Window, div, list, prelude::*, px,
};

use crate::markdown::{Block, BlockKind, BlockSplitter, Inline, code_body};
use crate::theme;

/// Coalesce streamed deltas into at most one re-layout per frame.
const FRAME: Duration = Duration::from_millis(16);
const MAX_WIDTH: f32 = 820.;
const OUTPUT_LINES: usize = 40;

enum Body {
    /// Tool, approval, notice and error items: everything is in `kind`.
    None,
    /// Assistant messages: block-split Markdown plus a per-frame snapshot of
    /// the live tail.
    Markdown { doc: BlockSplitter, tail: Block },
    /// User messages and reasoning.
    Plain {
        text: String,
        snapshot: SharedString,
    },
}

struct Entry {
    item: Arc<TurnItem>,
    body: Body,
}

impl Entry {
    fn new(item: &Arc<TurnItem>) -> Self {
        let body = match &item.kind {
            ItemKind::AssistantMessage { streaming } => {
                let mut doc = BlockSplitter::from_text(&item.text);
                if !streaming {
                    doc.finish();
                }
                let tail = Block::new(doc.tail_kind(), &doc.tail);
                Body::Markdown { doc, tail }
            }
            ItemKind::UserMessage | ItemKind::Reasoning { .. } => Body::Plain {
                text: item.text.to_string(),
                snapshot: SharedString::from(item.text.to_string()),
            },
            _ => Body::None,
        };
        // The body now lives in `body`; don't keep a second copy of it.
        let item = if item.text.is_empty() {
            item.clone()
        } else {
            Arc::new(TurnItem {
                text: "".into(),
                ..(**item).clone()
            })
        };
        Self { item, body }
    }

    /// Rows this entry occupies.
    fn rows(&self) -> usize {
        match &self.body {
            Body::Markdown { doc, .. } => doc.rows(),
            _ => 1,
        }
    }

    fn append(&mut self, chunk: &str) {
        match &mut self.body {
            Body::Markdown { doc, .. } => doc.push(chunk),
            Body::Plain { text, .. } => text.push_str(chunk),
            Body::None => {}
        }
    }

    /// Refresh per-frame snapshots of growing text.
    fn snapshot(&mut self) {
        match &mut self.body {
            Body::Markdown { doc, tail } => *tail = Block::new(doc.tail_kind(), &doc.tail),
            Body::Plain { text, snapshot } => {
                if snapshot.len() != text.len() {
                    *snapshot = SharedString::from(text.clone());
                }
            }
            Body::None => {}
        }
    }

    fn finish(&mut self) {
        if let Body::Markdown { doc, .. } = &mut self.body {
            doc.finish();
        }
        let mut item = (*self.item).clone();
        if let ItemKind::AssistantMessage { streaming } | ItemKind::Reasoning { streaming } =
            &mut item.kind
        {
            *streaming = false;
        }
        self.item = Arc::new(item);
    }
}

#[derive(Clone, Copy)]
enum Row {
    /// Entry index, part (block index for Markdown, else 0).
    Item(usize, usize),
    Footer,
}

pub struct Timeline {
    pub thread_id: ThreadId,
    core: CoreClient,
    entries: Vec<Entry>,
    index: HashMap<ItemId, usize>,
    rows: Vec<Row>,
    /// First row of each entry.
    entry_rows: Vec<usize>,
    list_state: ListState,
    /// First entry whose rows changed since the last flush.
    dirty_from: Option<usize>,
    footer_dirty: bool,
    flush_scheduled: bool,
    status: ThreadStatus,
    expanded: HashSet<ItemId>,
}

impl Timeline {
    pub fn new(snapshot: &ThreadSnapshot, status: ThreadStatus, core: CoreClient) -> Self {
        let list_state = ListState::new(0, ListAlignment::Bottom, px(600.));
        list_state.set_follow_mode(FollowMode::Tail);
        let mut this = Self {
            thread_id: snapshot.thread_id,
            core,
            entries: Vec::with_capacity(snapshot.items.len()),
            index: HashMap::with_capacity(snapshot.items.len()),
            rows: Vec::new(),
            entry_rows: Vec::new(),
            list_state,
            dirty_from: None,
            footer_dirty: false,
            flush_scheduled: false,
            status,
            expanded: HashSet::new(),
        };
        for item in &snapshot.items {
            this.index.insert(item.id, this.entries.len());
            this.entries.push(Entry::new(item));
        }
        this.rebuild_rows(0);
        this.list_state.reset(this.rows.len());
        this.list_state.scroll_to_end();
        this
    }

    pub fn set_status(&mut self, status: ThreadStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            self.footer_dirty = true;
            self.schedule(cx);
        }
    }

    pub fn apply(&mut self, event: &EventKind, cx: &mut Context<Self>) {
        match event {
            EventKind::ItemAdded { item } => {
                let ix = self.entries.len();
                self.index.insert(item.id, ix);
                self.entries.push(Entry::new(item));
                self.mark(ix, cx);
            }
            EventKind::ItemUpdated { item } => {
                if let Some(&ix) = self.index.get(&item.id) {
                    let entry = &mut self.entries[ix];
                    entry.item = Arc::new(TurnItem {
                        text: "".into(),
                        ..(**item).clone()
                    });
                    self.mark(ix, cx);
                }
            }
            EventKind::ItemFinished { item_id, .. } => {
                if let Some(&ix) = self.index.get(item_id) {
                    self.entries[ix].finish();
                    self.mark(ix, cx);
                }
            }
            _ => {}
        }
    }

    pub fn append(&mut self, item_id: ItemId, chunk: &str, cx: &mut Context<Self>) {
        if let Some(&ix) = self.index.get(&item_id) {
            self.entries[ix].append(chunk);
            self.mark(ix, cx);
        }
    }

    fn mark(&mut self, entry: usize, cx: &mut Context<Self>) {
        self.dirty_from = Some(self.dirty_from.map_or(entry, |d| d.min(entry)));
        self.schedule(cx);
    }

    fn schedule(&mut self, cx: &mut Context<Self>) {
        if !self.flush_scheduled {
            self.flush_scheduled = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FRAME).await;
                this.update(cx, |this, cx| this.flush(cx)).ok();
            })
            .detach();
        }
    }

    fn show_footer(&self) -> bool {
        matches!(self.status, ThreadStatus::Running | ThreadStatus::Waiting)
    }

    /// Row index where entry `from` starts (or where the next entry would).
    fn first_row(&self, from: usize) -> usize {
        let item_rows =
            self.rows.len() - usize::from(matches!(self.rows.last(), Some(Row::Footer)));
        self.entry_rows
            .get(from)
            .copied()
            .unwrap_or(item_rows)
            .min(item_rows)
    }

    /// Recompute `rows` from entry `from` on (plus the footer).
    fn rebuild_rows(&mut self, from: usize) {
        let first_row = self.first_row(from);
        self.rows.truncate(first_row);
        self.entry_rows.truncate(from);
        for (ix, entry) in self.entries.iter().enumerate().skip(from) {
            self.entry_rows.push(self.rows.len());
            for part in 0..entry.rows() {
                self.rows.push(Row::Item(ix, part));
            }
        }
        if self.show_footer() {
            self.rows.push(Row::Footer);
        }
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.flush_scheduled = false;
        let from = match (self.dirty_from.take(), self.footer_dirty) {
            (Some(entry), _) => entry,
            (None, true) => self.entries.len(),
            (None, false) => return,
        };
        self.footer_dirty = false;
        for entry in &mut self.entries[from..] {
            entry.snapshot();
        }
        let old_len = self.rows.len();
        let first_row = self.first_row(from);
        self.rebuild_rows(from);
        self.list_state
            .splice(first_row..old_len, self.rows.len() - first_row);
        cx.notify();
    }

    fn toggle(&mut self, id: ItemId, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        if let Some(&ix) = self.index.get(&id) {
            self.mark(ix, cx);
        }
    }

    fn respond(&self, item_id: ItemId, decision: ApprovalDecision) {
        self.core
            .dispatch(CommandEnvelope::new(Command::RuntimeRequestRespond {
                thread_id: self.thread_id,
                item_id,
                decision,
            }));
    }

    // ------------------------------------------------------------- rendering

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = match self.rows.get(ix) {
            Some(Row::Item(entry, part)) => self.render_item(*entry, *part, cx),
            Some(Row::Footer) => self.render_footer(),
            None => div().into_any_element(),
        };
        // Center a readable column.
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(div().w_full().max_w(px(MAX_WIDTH)).px_6().child(row))
            .into_any_element()
    }

    fn render_footer(&self) -> AnyElement {
        let (label, color) = match self.status {
            ThreadStatus::Waiting => ("Waiting for approval", theme::warning()),
            _ => ("Working…", theme::text_muted()),
        };
        div()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(color)
            .child(div().size(px(6.)).rounded_full().bg(color))
            .child(label)
            .into_any_element()
    }

    fn render_item(&mut self, ix: usize, part: usize, cx: &mut Context<Self>) -> AnyElement {
        let entry = &self.entries[ix];
        let item = entry.item.clone();
        let id = item.id;
        let expanded = self.expanded.contains(&id);
        match (&item.kind, &entry.body) {
            (ItemKind::UserMessage, Body::Plain { snapshot, .. }) => div()
                .pt_4()
                .pb_2()
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(MAX_WIDTH * 0.75))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(theme::surface())
                        .border_1()
                        .border_color(theme::border())
                        .text_sm()
                        .child(snapshot.clone()),
                )
                .into_any_element(),
            (ItemKind::AssistantMessage { .. }, Body::Markdown { doc, tail }) => {
                let block = doc.blocks.get(part).unwrap_or(tail);
                render_block(block)
            }
            (ItemKind::Reasoning { streaming }, Body::Plain { snapshot, .. }) => {
                let label = if *streaming { "Thinking…" } else { "Thought" };
                let chevron = if expanded { "▾" } else { "▸" };
                let preview: SharedString = if expanded {
                    snapshot.clone()
                } else {
                    first_line(snapshot, 120).into()
                };
                div()
                    .id(SharedString::from(format!("r-{id}")))
                    .py_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(id, cx)))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(format!("{chevron} {label}")),
                    )
                    .child(
                        div()
                            .ml_3()
                            .pl_2()
                            .border_l_1()
                            .border_color(theme::border())
                            .text_xs()
                            .text_color(theme::text_faint())
                            .child(preview),
                    )
                    .into_any_element()
            }
            (
                ItemKind::CommandExecution {
                    command,
                    status,
                    output,
                    exit_code,
                    ..
                },
                _,
            ) => {
                let has_output = !output.is_empty();
                let header = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(status_glyph(*status))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(theme::MONO)
                            .text_xs()
                            .text_color(theme::text())
                            .child(format!("$ {}", first_line(command, 200))),
                    )
                    .when_some(*exit_code, |d, code| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .child(format!("exit {code}")),
                        )
                    })
                    .when(has_output, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .child(if expanded { "▾" } else { "▸" }),
                        )
                    });
                tool_frame(id, cx)
                    .when(has_output, |d| {
                        d.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle(id, cx)))
                    })
                    .child(header)
                    .when(expanded && has_output, |d| {
                        d.child(
                            div()
                                .mt_1()
                                .p_2()
                                .rounded_md()
                                .bg(theme::code_bg())
                                .font_family(theme::MONO)
                                .text_xs()
                                .text_color(theme::text_muted())
                                .children(
                                    output
                                        .lines()
                                        .take(OUTPUT_LINES)
                                        .map(|l| div().child(SharedString::from(l.to_owned()))),
                                ),
                        )
                    })
                    .into_any_element()
            }
            (ItemKind::FileChange { paths, status, .. }, _) => {
                let files = if paths.is_empty() {
                    "files".to_owned()
                } else {
                    paths.join(", ")
                };
                let verb = match status {
                    ToolStatus::Running => "Editing",
                    ToolStatus::Completed => "Edited",
                    ToolStatus::Failed => "Failed to edit",
                    ToolStatus::Declined => "Declined edit of",
                };
                tool_frame(id, cx)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(status_glyph(*status))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text())
                                    .child(format!("{verb} {files}")),
                            ),
                    )
                    .into_any_element()
            }
            (
                ItemKind::ToolCall {
                    name,
                    input,
                    status,
                    ..
                },
                _,
            ) => tool_frame(id, cx)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(status_glyph(*status))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text())
                                .child(format!("{name} {}", first_line(input, 160))),
                        ),
                )
                .into_any_element(),
            (
                ItemKind::ApprovalRequest {
                    title,
                    detail,
                    state,
                    ..
                },
                _,
            ) => self.render_approval(id, title, detail, *state, cx),
            (ItemKind::SystemNotice { message }, _) => div()
                .py_2()
                .flex()
                .justify_center()
                .text_xs()
                .text_color(theme::text_muted())
                .child(SharedString::from(message.clone()))
                .into_any_element(),
            (ItemKind::Error { message }, _) => div()
                .my_1()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme::danger_bg())
                .text_xs()
                .text_color(theme::danger())
                .child(SharedString::from(message.clone()))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_approval(
        &self,
        id: ItemId,
        title: &str,
        detail: &str,
        state: ApprovalState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pending = state == ApprovalState::Pending;
        let footer = if pending {
            div()
                .flex()
                .gap_2()
                .child(button(
                    format!("approve-{id}"),
                    "Approve",
                    theme::accent_bg(),
                    theme::text(),
                    cx.listener(move |this, _, _, _| this.respond(id, ApprovalDecision::Approve)),
                ))
                .child(button(
                    format!("deny-{id}"),
                    "Deny",
                    theme::surface_hover(),
                    theme::text(),
                    cx.listener(move |this, _, _, _| this.respond(id, ApprovalDecision::Deny)),
                ))
        } else {
            let (label, color) = match state {
                ApprovalState::Approved => ("Approved", theme::success()),
                ApprovalState::Denied => ("Denied", theme::danger()),
                _ => ("Cancelled", theme::text_muted()),
            };
            div().text_xs().text_color(color).child(label)
        };
        div()
            .my_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(if pending {
                theme::warning()
            } else {
                theme::border()
            })
            .bg(if pending {
                theme::warning_bg()
            } else {
                theme::surface()
            })
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(title.to_owned())),
            )
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .font_family(theme::MONO)
                    .text_xs()
                    .text_color(theme::text_muted())
                    .children(
                        detail
                            .lines()
                            .take(12)
                            .map(|l| div().child(SharedString::from(l.to_owned()))),
                    ),
            )
            .child(footer)
            .into_any_element()
    }
}

fn tool_frame(id: ItemId, _cx: &mut Context<Timeline>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(format!("t-{id}")))
        .my_0p5()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme::surface())
}

pub fn button(
    id: String,
    label: &'static str,
    bg: gpui::Rgba,
    fg: gpui::Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .px_3()
        .py_1()
        .rounded_md()
        .bg(bg)
        .text_xs()
        .text_color(fg)
        .cursor_pointer()
        .hover(|d| d.opacity(0.85))
        .on_click(on_click)
        .child(label)
}

fn status_glyph(status: ToolStatus) -> AnyElement {
    let (glyph, color) = match status {
        ToolStatus::Running => ("○", theme::text_muted()),
        ToolStatus::Completed => ("✓", theme::success()),
        ToolStatus::Failed => ("✗", theme::danger()),
        ToolStatus::Declined => ("⊘", theme::warning()),
    };
    div()
        .w(px(12.))
        .text_xs()
        .text_color(color)
        .child(glyph)
        .into_any_element()
}

fn render_block(block: &Block) -> AnyElement {
    let row = div().w_full().py_1();
    match block.kind {
        BlockKind::Paragraph => row.text_sm().child(styled(block)).into_any_element(),
        BlockKind::Heading => row
            .pt_3()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .child(styled(block))
            .into_any_element(),
        BlockKind::Code => {
            // A live tail still carries its opening fence.
            let body = if block.text.starts_with("```") {
                SharedString::from(code_body(&block.text))
            } else {
                block.text.clone()
            };
            row.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .border_1()
                    .border_color(theme::border())
                    .font_family(theme::MONO)
                    .text_xs()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .children(
                        body.lines()
                            .map(|l| div().child(SharedString::from(l.to_owned()))),
                    ),
            )
            .into_any_element()
        }
    }
}

fn styled(block: &Block) -> StyledText {
    let text = StyledText::new(block.text.clone());
    if block.inline.is_empty() {
        return text;
    }
    let mut end = 0;
    let highlights: Vec<_> = block
        .inline
        .iter()
        .filter(|(range, _)| {
            // Highlights must not overlap.
            let ok = range.start >= end && range.end <= block.text.len();
            if ok {
                end = range.end;
            }
            ok
        })
        .map(|(range, kind)| {
            let style = match kind {
                Inline::Bold => HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                },
                Inline::Code => HighlightStyle {
                    color: Some(theme::accent()),
                    background_color: Some(theme::code_bg().into()),
                    ..Default::default()
                },
            };
            (range.clone(), style)
        })
        .collect();
    text.with_highlights(highlights)
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() <= max && !text.contains('\n') {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(max).collect();
    out.push('…');
    out
}

impl Render for Timeline {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().flex().flex_col().child(
            list(self.list_state.clone(), cx.processor(Self::render_row))
                .size_full()
                .py_4(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blongo_protocol::{RunId, Timestamp};

    fn item(ordinal: u32, kind: ItemKind, text: &str) -> Arc<TurnItem> {
        Arc::new(TurnItem {
            id: ItemId::new(),
            thread_id: ThreadId::new(),
            run_id: Some(RunId::new()),
            ordinal,
            created_at: Timestamp(0),
            kind,
            text: text.into(),
        })
    }

    fn shape(t: &Timeline) -> Vec<String> {
        t.rows
            .iter()
            .map(|r| match r {
                Row::Item(e, p) => format!("{e}.{p}"),
                Row::Footer => "F".into(),
            })
            .collect()
    }

    #[test]
    fn rows_follow_entries_and_footer() {
        let snapshot = ThreadSnapshot {
            thread_id: ThreadId::new(),
            sequence: 1,
            runs: vec![],
            items: vec![
                item(0, ItemKind::UserMessage, "hi"),
                item(
                    1,
                    ItemKind::AssistantMessage { streaming: false },
                    "# A\n\npara\n\n```\ncode\n```\n",
                ),
            ],
        };
        let mut t = Timeline::new(&snapshot, ThreadStatus::Idle, CoreClient::disconnected());
        assert_eq!(shape(&t), ["0.0", "1.0", "1.1", "1.2"]);
        // Assistant body is held once, in blocks, not in the item.
        assert_eq!(&*t.entries[1].item.text, "");

        // A run starts: footer appears after the last entry.
        t.status = ThreadStatus::Running;
        t.rebuild_rows(t.entries.len());
        assert_eq!(shape(&t), ["0.0", "1.0", "1.1", "1.2", "F"]);

        // A new entry is inserted before the footer, never after it.
        let streaming = item(2, ItemKind::AssistantMessage { streaming: true }, "");
        t.index.insert(streaming.id, 2);
        t.entries.push(Entry::new(&streaming));
        assert_eq!(t.first_row(2), 4);
        t.entries[2].append("one\n\ntw");
        t.rebuild_rows(2);
        assert_eq!(shape(&t), ["0.0", "1.0", "1.1", "1.2", "2.0", "2.1", "F"]);

        // Run ends: footer goes away.
        t.status = ThreadStatus::Idle;
        let from = t.entries.len();
        t.rebuild_rows(from);
        assert_eq!(shape(&t), ["0.0", "1.0", "1.1", "1.2", "2.0", "2.1"]);
    }
}
