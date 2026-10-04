//! The diff / review panel: changed files of a turn, of a whole thread or
//! of a pull request, with inline comments.
//!
//! Virtualized: everything is flattened into fixed-height rows drawn by a
//! `uniform_list`, so only the visible rows are built whatever the diff's
//! size. File bodies are loaded lazily: the summary lists the files, the
//! first few are fetched right away, the others when opened, each capped
//! at [`FIRST_LINES`] lines with a "show more" row (up to
//! `MAX_DIFF_LINES`).
//!
//! Comments are drafted on lines and sent together: to the agent as one
//! message (thread diffs) or as one review (pull requests); the host
//! decides, the panel only emits [`DiffEvent::Send`].

use std::ops::Range;
use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::ThreadId;
use blongo_protocol::workspace::{
    DiffFileStat, DiffScope, DiffSummary, FileDiff, LineKind, MAX_DIFF_LINES, Query, QueryReply,
    parse_unified_diff,
};
use gpui::{
    Context, Entity, EventEmitter, Focusable, FontWeight, SharedString, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::button;

pub const ROW_HEIGHT: f32 = 22.;
/// Lines fetched per file at first.
const FIRST_LINES: u32 = 1500;
/// Files whose bodies are fetched as soon as the summary arrives.
const OPEN_AT_FIRST: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub path: String,
    /// Line number on the side it was made on.
    pub line: u32,
    /// Made on a removed line (old side).
    pub old_side: bool,
    /// The commented line's text.
    pub code: String,
    pub text: String,
}

pub enum DiffEvent {
    /// Send these comments (and the summary typed with them).
    Send(Vec<Comment>),
}

pub enum Source {
    /// Ask the thread's backend for the summary and file bodies.
    Thread {
        backend: Arc<dyn Backend>,
        thread_id: ThreadId,
        scope: DiffScope,
    },
    /// Files whose patches are already known (pull requests).
    Patches,
}

struct FileEntry {
    stat: DiffFileStat,
    /// Known patch text (pull requests).
    patch: Option<String>,
    diff: Option<FileDiff>,
    open: bool,
    loading: bool,
    error: Option<String>,
    max_lines: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    /// Loading / binary / error / empty notes of a file.
    Note(usize),
    More(usize),
    Comment(usize),
    /// The comment being written, under this line.
    Composer,
}

pub struct DiffView {
    source: Source,
    pub title: SharedString,
    summary: Option<DiffSummary>,
    files: Vec<FileEntry>,
    rows: Vec<Row>,
    pub comments: Vec<Comment>,
    /// (file, hunk, line) a comment is being written on.
    composing: Option<(usize, usize, usize)>,
    comment_input: Entity<TextInput>,
    error: Option<String>,
    /// What the send button says ("Send to agent", "Submit review").
    send_label: &'static str,
    /// Files waiting for their bodies, and whether one is being fetched.
    queue: std::collections::VecDeque<usize>,
    in_flight: bool,
    /// Bumped by a reload: replies for older summaries are dropped.
    generation: u64,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DiffEvent> for DiffView {}

impl DiffView {
    pub fn new(
        source: Source,
        title: impl Into<SharedString>,
        send_label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let comment_input =
            cx.new(|cx| TextInput::new("Comment (Enter to add, Esc to cancel)", false, cx));
        let subscriptions = vec![cx.subscribe_in(
            &comment_input,
            window,
            |this, input, event, _, cx| match event {
                InputEvent::Submit => {
                    let text = input.read(cx).text().trim().to_owned();
                    if !text.is_empty() {
                        this.add_comment(text, cx);
                    }
                    input.update(cx, |i, cx| i.set_text("", cx));
                }
                InputEvent::Cancel => {
                    this.composing = None;
                    this.rebuild(cx);
                }
                InputEvent::SubmitAlt => {}
            },
        )];
        let mut this = Self {
            source,
            title: title.into(),
            summary: None,
            files: Vec::new(),
            rows: Vec::new(),
            comments: Vec::new(),
            composing: None,
            comment_input,
            error: None,
            send_label,
            queue: std::collections::VecDeque::new(),
            in_flight: false,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.load_summary(cx);
        this
    }

    /// Pull requests: the files and their patches.
    pub fn set_patches(
        &mut self,
        files: Vec<(DiffFileStat, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        let (added, removed) = files.iter().fold((0u64, 0u64), |(a, r), (s, _)| {
            (a + u64::from(s.added), r + u64::from(s.removed))
        });
        self.summary = Some(DiffSummary {
            from: String::new(),
            to: String::new(),
            files: files.iter().map(|(s, _)| s.clone()).collect(),
            truncated: false,
            added,
            removed,
        });
        self.files = files
            .into_iter()
            .map(|(stat, patch)| FileEntry {
                stat,
                patch,
                diff: None,
                open: false,
                loading: false,
                error: None,
                max_lines: FIRST_LINES,
            })
            .collect();
        for ix in 0..self.files.len().min(OPEN_AT_FIRST) {
            self.open_file(ix, false, cx);
        }
        self.rebuild(cx);
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error);
        cx.notify();
    }

    fn load_summary(&mut self, cx: &mut Context<Self>) {
        let Source::Thread {
            backend,
            thread_id,
            scope,
        } = &self.source
        else {
            return;
        };
        let query = Query::DiffSummary {
            thread_id: *thread_id,
            scope: *scope,
        };
        let generation = self.generation;
        crate::query::ask(
            backend,
            query,
            cx.weak_entity(),
            cx,
            move |this, result, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(QueryReply::DiffSummary(summary)) => {
                        this.files = summary
                            .files
                            .iter()
                            .map(|stat| FileEntry {
                                stat: stat.clone(),
                                patch: None,
                                diff: None,
                                open: false,
                                loading: false,
                                error: None,
                                max_lines: FIRST_LINES,
                            })
                            .collect();
                        this.summary = Some(summary);
                        for ix in 0..this.files.len().min(OPEN_AT_FIRST) {
                            this.open_file(ix, false, cx);
                        }
                    }
                    Ok(other) => this.error = Some(format!("unexpected reply: {other:?}")),
                    Err(err) => this.error = Some(err),
                }
                this.rebuild(cx);
            },
        );
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if matches!(self.source, Source::Thread { .. }) {
            self.summary = None;
            self.files.clear();
            self.queue.clear();
            self.in_flight = false;
            self.generation += 1;
            self.error = None;
            self.composing = None;
            self.rebuild(cx);
            self.load_summary(cx);
        }
    }

    /// Open a file; its body is fetched in turn (one query at a time, so
    /// a large diff never has many big replies in memory at once).
    /// `first`: the user asked, so it goes before the ones opened at load.
    fn open_file(&mut self, ix: usize, first: bool, cx: &mut Context<Self>) {
        let Some(file) = self.files.get_mut(ix) else {
            return;
        };
        file.open = true;
        if file.diff.is_some() || file.loading || file.stat.binary {
            return;
        }
        if let Some(patch) = &file.patch {
            file.diff = Some(parse_unified_diff(&file.stat.path, patch, file.max_lines));
            return;
        }
        if matches!(self.source, Source::Patches) {
            file.error = Some("the forge sent no patch for this file (too large?)".into());
            return;
        }
        file.loading = true;
        if first {
            self.queue.push_front(ix);
        } else {
            self.queue.push_back(ix);
        }
        self.pump(cx);
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.in_flight {
            return;
        }
        let Source::Thread {
            backend, thread_id, ..
        } = &self.source
        else {
            return;
        };
        let Some(summary) = &self.summary else {
            return;
        };
        let Some(ix) = self.queue.pop_front() else {
            return;
        };
        let Some(file) = self.files.get(ix) else {
            return;
        };
        let query = Query::DiffFile {
            thread_id: *thread_id,
            from: summary.from.clone(),
            to: summary.to.clone(),
            path: file.stat.path.clone(),
            max_lines: file.max_lines,
        };
        let generation = self.generation;
        self.in_flight = true;
        crate::query::ask(
            backend,
            query,
            cx.weak_entity(),
            cx,
            move |this, result, cx| {
                if this.generation != generation {
                    return;
                }
                this.in_flight = false;
                if let Some(file) = this.files.get_mut(ix) {
                    file.loading = false;
                    match result {
                        Ok(QueryReply::DiffFile(diff)) => file.diff = Some(diff),
                        Ok(other) => file.error = Some(format!("unexpected reply: {other:?}")),
                        Err(err) => file.error = Some(err),
                    }
                }
                this.pump(cx);
                this.rebuild(cx);
            },
        );
    }

    fn toggle_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.files[ix].open {
            self.files[ix].open = false;
            if self.composing.is_some_and(|(f, _, _)| f == ix) {
                self.composing = None;
            }
        } else {
            self.open_file(ix, true, cx);
        }
        self.rebuild(cx);
    }

    fn more(&mut self, ix: usize, cx: &mut Context<Self>) {
        let file = &mut self.files[ix];
        file.max_lines = (file.max_lines * 4).min(MAX_DIFF_LINES);
        file.diff = None;
        self.open_file(ix, true, cx);
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let mut rows = Vec::new();
        for (fx, file) in self.files.iter().enumerate() {
            rows.push(Row::File(fx));
            if !file.open {
                continue;
            }
            let Some(diff) = &file.diff else {
                rows.push(Row::Note(fx));
                continue;
            };
            if diff.binary || diff.hunks.is_empty() {
                rows.push(Row::Note(fx));
            }
            for (hx, hunk) in diff.hunks.iter().enumerate() {
                rows.push(Row::Hunk(fx, hx));
                for (lx, line) in hunk.lines.iter().enumerate() {
                    rows.push(Row::Line(fx, hx, lx));
                    let (number, old_side) = line_ref(line);
                    for (cx_ix, c) in self.comments.iter().enumerate() {
                        if c.path == file.stat.path
                            && Some(c.line) == number
                            && c.old_side == old_side
                        {
                            rows.push(Row::Comment(cx_ix));
                        }
                    }
                    if self.composing == Some((fx, hx, lx)) {
                        rows.push(Row::Composer);
                    }
                }
            }
            if diff.truncated && file.max_lines < MAX_DIFF_LINES {
                rows.push(Row::More(fx));
            } else if diff.truncated {
                rows.push(Row::Note(fx));
            }
        }
        self.rows = rows;
        cx.notify();
    }

    fn line(
        &self,
        fx: usize,
        hx: usize,
        lx: usize,
    ) -> Option<&blongo_protocol::workspace::DiffLine> {
        self.files
            .get(fx)?
            .diff
            .as_ref()?
            .hunks
            .get(hx)?
            .lines
            .get(lx)
    }

    fn start_comment(
        &mut self,
        at: (usize, usize, usize),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(line) = self.line(at.0, at.1, at.2) else {
            return;
        };
        if line.kind == LineKind::Meta {
            return;
        }
        self.composing = Some(at);
        self.comment_input.update(cx, |i, cx| i.set_text("", cx));
        window.focus(&self.comment_input.focus_handle(cx), cx);
        self.rebuild(cx);
        if let Some(row) = self.rows.iter().position(|r| *r == Row::Composer) {
            self.scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
    }

    fn add_comment(&mut self, text: String, cx: &mut Context<Self>) {
        let Some((fx, hx, lx)) = self.composing.take() else {
            return;
        };
        let Some(line) = self.line(fx, hx, lx) else {
            return;
        };
        let (number, old_side) = line_ref(line);
        let Some(number) = number else {
            return;
        };
        self.comments.push(Comment {
            path: self.files[fx].stat.path.clone(),
            line: number,
            old_side,
            code: line.text.clone(),
            text,
        });
        self.rebuild(cx);
    }

    pub fn clear_comments(&mut self, cx: &mut Context<Self>) {
        self.comments.clear();
        self.composing = None;
        self.rebuild(cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        range
            .filter_map(|ix| Some((ix, *self.rows.get(ix)?)))
            .map(|(ix, row)| self.render_row(ix, row, cx))
            .collect()
    }

    fn render_row(&self, ix: usize, row: Row, cx: &mut Context<Self>) -> gpui::AnyElement {
        let base = div()
            .id(("diff-row", ix))
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_xs();
        match row {
            Row::File(fx) => {
                let file = &self.files[fx];
                let s = &file.stat;
                let name = match &s.old_path {
                    Some(old) => format!("{old} → {}", s.path),
                    None => s.path.clone(),
                };
                base.px_2()
                    .gap_2()
                    .bg(theme::surface())
                    .border_t_1()
                    .border_color(theme::border())
                    .cursor_pointer()
                    .hover(|d| d.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(fx, cx)))
                    .child(div().text_color(theme::text_faint()).child(if file.open {
                        "▾"
                    } else {
                        "▸"
                    }))
                    .child(
                        div()
                            .w(px(12.))
                            .text_color(status_color(s.status))
                            .child(s.status.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .text_color(theme::success())
                            .child(format!("+{}", s.added)),
                    )
                    .child(
                        div()
                            .text_color(theme::danger())
                            .child(format!("−{}", s.removed)),
                    )
                    .into_any_element()
            }
            Row::Hunk(fx, hx) => {
                let header = self.files[fx]
                    .diff
                    .as_ref()
                    .and_then(|d| d.hunks.get(hx))
                    .map(|h| h.header.clone())
                    .unwrap_or_default();
                base.pl(px(96.))
                    .font_family(theme::MONO)
                    .text_color(theme::accent())
                    .bg(theme::code_bg())
                    .child(SharedString::from(header))
                    .into_any_element()
            }
            Row::Line(fx, hx, lx) => {
                let Some(line) = self.line(fx, hx, lx) else {
                    return base.into_any_element();
                };
                let (bg, sign, fg) = match line.kind {
                    LineKind::Added => (Some(theme::diff_added_bg()), "+", theme::text()),
                    LineKind::Removed => (Some(theme::diff_removed_bg()), "−", theme::text()),
                    LineKind::Context => (None, " ", theme::text_muted()),
                    LineKind::Meta => (None, " ", theme::text_muted()),
                };
                let num = |n: Option<u32>| {
                    div()
                        .w(px(40.))
                        .flex_shrink_0()
                        .text_right()
                        .pr_1()
                        .text_color(theme::text_faint())
                        .child(n.map(|n| n.to_string()).unwrap_or_default())
                };
                let text = line.text.replace('\t', "    ");
                base.font_family(theme::MONO)
                    .when_some(bg, |d, bg| d.bg(bg))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_comment((fx, hx, lx), window, cx)
                    }))
                    .child(num(line.old))
                    .child(num(line.new))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .text_color(theme::text_faint())
                            .child(sign),
                    )
                    .child(div().text_color(fg).child(SharedString::from(text)))
                    .into_any_element()
            }
            Row::Note(fx) => {
                let file = &self.files[fx];
                let text: SharedString = if let Some(err) = &file.error {
                    err.clone().into()
                } else if file.stat.binary || file.diff.as_ref().is_some_and(|d| d.binary) {
                    "Binary file".into()
                } else if file.loading || file.diff.is_none() {
                    "Loading…".into()
                } else if file.diff.as_ref().is_some_and(|d| d.truncated) {
                    format!("Diff cut at {} lines", file.max_lines).into()
                } else {
                    "No textual changes".into()
                };
                base.pl(px(96.))
                    .text_color(if file.error.is_some() {
                        theme::danger()
                    } else {
                        theme::text_muted()
                    })
                    .child(text)
                    .into_any_element()
            }
            Row::More(fx) => base
                .pl(px(96.))
                .text_color(theme::link())
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.more(fx, cx)))
                .child(format!(
                    "Show more lines ({} shown)",
                    self.files[fx].diff.as_ref().map_or(0, |d| d.line_count())
                ))
                .into_any_element(),
            Row::Comment(cix) => {
                let c = &self.comments[cix];
                base.pl(px(96.))
                    .pr_2()
                    .gap_2()
                    .bg(theme::comment_bg())
                    .child(div().text_color(theme::accent()).child("Comment"))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(SharedString::from(c.text.clone())),
                    )
                    .child(
                        div()
                            .id(("del-comment", cix))
                            .px_1()
                            .text_color(theme::text_faint())
                            .hover(|d| d.text_color(theme::danger()))
                            .cursor_pointer()
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if cix < this.comments.len() {
                                    this.comments.remove(cix);
                                    this.rebuild(cx);
                                }
                            })),
                    )
                    .into_any_element()
            }
            Row::Composer => base
                .pl(px(96.))
                .pr_2()
                .gap_2()
                .bg(theme::comment_bg())
                .child(div().text_color(theme::accent()).child("New comment"))
                .child(
                    div()
                        .flex_1()
                        .px_1()
                        .rounded_sm()
                        .bg(theme::code_bg())
                        .child(self.comment_input.clone()),
                )
                .into_any_element(),
        }
    }
}

/// The line number a comment on `line` refers to, and whether it is on the
/// old side.
fn line_ref(line: &blongo_protocol::workspace::DiffLine) -> (Option<u32>, bool) {
    match line.kind {
        LineKind::Removed => (line.old, true),
        _ => (line.new, false),
    }
}

fn status_color(status: char) -> gpui::Hsla {
    match status {
        'A' => theme::success().into(),
        'D' => theme::danger().into(),
        'R' | 'C' => theme::accent(),
        _ => theme::warning().into(),
    }
}

/// The comments as one message for the agent.
pub fn comments_message(comments: &[Comment]) -> String {
    let mut text = String::from("Review comments on your changes:\n");
    for c in comments {
        let side = if c.old_side { " (removed line)" } else { "" };
        text.push_str(&format!(
            "\n{}:{}{side}\n> {}\n{}\n",
            c.path,
            c.line,
            c.code.trim_end(),
            c.text
        ));
    }
    text
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let stats: SharedString = match &self.summary {
            Some(s) => format!(
                "{} file{}  +{} −{}{}",
                s.files.len(),
                if s.files.len() == 1 { "" } else { "s" },
                s.added,
                s.removed,
                if s.truncated { "  (list cut)" } else { "" }
            )
            .into(),
            None if self.error.is_none() => "Loading changes…".into(),
            None => "".into(),
        };
        let n = self.comments.len();
        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title.clone()),
            )
            .child(div().flex_1().text_color(theme::text_muted()).child(stats))
            .when(n > 0, |d| {
                d.child(
                    div()
                        .text_color(theme::text_muted())
                        .child(format!("{n} comment{}", if n == 1 { "" } else { "s" })),
                )
                .child(button(
                    "diff-clear".into(),
                    "Discard",
                    theme::surface_hover(),
                    theme::text(),
                    cx.listener(|this, _, _, cx| this.clear_comments(cx)),
                ))
            })
            .child(button(
                "diff-send".into(),
                self.send_label,
                theme::accent_bg(),
                theme::text(),
                cx.listener(|this, _, _, cx| {
                    if !this.comments.is_empty() || matches!(this.source, Source::Patches) {
                        cx.emit(DiffEvent::Send(this.comments.clone()));
                    }
                }),
            ));
        let body = if let Some(err) = &self.error {
            div()
                .p_4()
                .text_sm()
                .text_color(theme::danger())
                .child(SharedString::from(err.clone()))
                .into_any_element()
        } else if self.summary.as_ref().is_some_and(|s| s.files.is_empty()) {
            div()
                .p_4()
                .text_sm()
                .text_color(theme::text_muted())
                .child("No changes.")
                .into_any_element()
        } else {
            uniform_list(
                "diff-rows",
                self.rows.len(),
                cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_quotes_each_line() {
        let msg = comments_message(&[
            Comment {
                path: "src/a.rs".into(),
                line: 3,
                old_side: false,
                code: "let x = 1;  ".into(),
                text: "Why 1?".into(),
            },
            Comment {
                path: "b.py".into(),
                line: 9,
                old_side: true,
                code: "old()".into(),
                text: "Keep this".into(),
            },
        ]);
        assert!(msg.contains("src/a.rs:3\n> let x = 1;\nWhy 1?"));
        assert!(msg.contains("b.py:9 (removed line)\n> old()\nKeep this"));
    }
}
