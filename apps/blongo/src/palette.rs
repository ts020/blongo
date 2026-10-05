//! The command palette (every command, with its key) and the file finder
//! (fuzzy search in the thread's folder, answered by the backend off the
//! UI thread).

use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::ThreadId;
use blongo_protocol::workspace::{FileMatch, Query, QueryReply};
use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, HighlightStyle, SharedString,
    StyledText, Subscription, Window, div, prelude::*, px,
};

use crate::input::{self, InputEvent, TextInput};
use crate::keymap::{self, Binding};
use crate::theme;

const MAX_ROWS: usize = 12;
const FILE_LIMIT: u32 = 50;

pub enum PaletteEvent {
    Run(&'static str),
    OpenFile(String),
    /// A [`Mode::Prompt`] was answered: its id and the text.
    Submit(&'static str, String),
    Dismiss,
}

pub enum Mode {
    Commands {
        /// (id, title, key)
        all: Vec<(&'static str, &'static str, Option<String>)>,
    },
    Files {
        backend: Arc<dyn Backend>,
        thread_id: ThreadId,
    },
    /// One line of text (Enter submits it).
    Prompt {
        id: &'static str,
        placeholder: &'static str,
        hint: &'static str,
    },
}

enum Item {
    Command {
        id: &'static str,
        title: &'static str,
        key: Option<String>,
        positions: Vec<usize>,
    },
    File(FileMatch),
}

pub struct Palette {
    mode: Mode,
    input: Entity<TextInput>,
    items: Vec<Item>,
    selected: usize,
    /// The pattern the shown results are for, and one waiting to be asked
    /// while a search is in flight.
    asked: Option<String>,
    in_flight: bool,
    next_pattern: Option<String>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

pub fn commands(bindings: &[Binding]) -> Mode {
    Mode::Commands {
        all: keymap::COMMANDS
            .iter()
            .filter(|c| c.id != "palette.commands")
            .map(|c| {
                (
                    c.id,
                    c.title,
                    keymap::key_for(bindings, c.id).map(keymap::display),
                )
            })
            .collect(),
    }
}

impl Palette {
    pub fn new(mode: Mode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = match mode {
            Mode::Commands { .. } => "Type a command…",
            Mode::Files { .. } => "Search files by name…",
            Mode::Prompt { placeholder, .. } => placeholder,
        };
        let input = cx.new(|cx| TextInput::new(placeholder, false, cx));
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event, _, cx| match event {
                InputEvent::Submit | InputEvent::SubmitAlt => this.confirm(cx),
                InputEvent::Cancel => cx.emit(PaletteEvent::Dismiss),
            }),
            cx.observe(&input, |this, _, cx| this.on_input(cx)),
        ];
        window.focus(&input.focus_handle(cx), cx);
        let mut this = Self {
            mode,
            input,
            items: Vec::new(),
            selected: 0,
            asked: None,
            in_flight: false,
            next_pattern: None,
            error: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.on_input(cx);
        this
    }

    fn on_input(&mut self, cx: &mut Context<Self>) {
        let pattern = self.input.read(cx).text().trim().to_owned();
        if self.asked.as_deref() == Some(pattern.as_str()) {
            return;
        }
        self.asked = Some(pattern.clone());
        match &self.mode {
            Mode::Commands { all } => {
                let mut scored: Vec<(i32, Item)> = all
                    .iter()
                    .filter_map(|(id, title, key)| {
                        let (score, positions) = fuzzy(&pattern, title)
                            .or_else(|| fuzzy(&pattern, id).map(|(s, _)| (s - 5, Vec::new())))?;
                        Some((
                            score,
                            Item::Command {
                                id,
                                title,
                                key: key.clone(),
                                positions,
                            },
                        ))
                    })
                    .collect();
                scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
                self.items = scored.into_iter().map(|(_, i)| i).collect();
                self.selected = 0;
                cx.notify();
            }
            Mode::Files { .. } => {
                if self.in_flight {
                    self.next_pattern = Some(pattern);
                } else {
                    self.search(pattern, cx);
                }
            }
            Mode::Prompt { .. } => {}
        }
    }

    fn search(&mut self, pattern: String, cx: &mut Context<Self>) {
        let Mode::Files { backend, thread_id } = &self.mode else {
            return;
        };
        self.in_flight = true;
        let query = Query::SearchFiles {
            thread_id: *thread_id,
            pattern,
            limit: FILE_LIMIT,
        };
        crate::query::ask(backend, query, cx.weak_entity(), cx, |this, result, cx| {
            this.in_flight = false;
            match result {
                Ok(QueryReply::Files(files)) => {
                    this.items = files.into_iter().map(Item::File).collect();
                    this.selected = 0;
                    this.error = None;
                }
                Ok(_) => {}
                Err(err) => this.error = Some(err.into()),
            }
            if let Some(next) = this.next_pattern.take() {
                this.search(next, cx);
            }
            cx.notify();
        });
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Mode::Prompt { id, .. } = self.mode {
            let text = self.input.read(cx).text().trim().to_owned();
            if !text.is_empty() {
                cx.emit(PaletteEvent::Submit(id, text));
            }
            return;
        }
        match self.items.get(self.selected) {
            Some(Item::Command { id, .. }) => cx.emit(PaletteEvent::Run(id)),
            Some(Item::File(f)) => cx.emit(PaletteEvent::OpenFile(f.path.clone())),
            None => {}
        }
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
        cx.notify();
    }
}

/// Subsequence match, case-insensitive: a score (higher is better: runs
/// and word starts count, gaps cost) and the matched char positions.
pub fn fuzzy(pattern: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let pat: Vec<char> = pattern
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| *c != ' ')
        .collect();
    if pat.is_empty() {
        return Some((0, Vec::new()));
    }
    let lower: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let chars: Vec<char> = text.chars().collect();
    if lower.len() != chars.len() {
        return None;
    }
    // Greedy from every place the first character occurs; keep the best.
    (0..lower.len())
        .filter(|&i| lower[i] == pat[0])
        .filter_map(|start| greedy(&pat, &lower, &chars, start))
        .max_by_key(|(score, _)| *score)
}

fn greedy(pat: &[char], lower: &[char], chars: &[char], start: usize) -> Option<(i32, Vec<usize>)> {
    let mut positions = Vec::with_capacity(pat.len());
    let mut score = 0;
    let mut at = start;
    let mut last: Option<usize> = None;
    for &p in pat {
        let found = (at..lower.len()).find(|&i| lower[i] == p)?;
        let word_start = found == 0 || !chars[found - 1].is_alphanumeric();
        score += 10;
        if word_start {
            score += 8;
        }
        match last {
            Some(l) if found == l + 1 => score += 6,
            Some(l) => score -= (found - l - 1).min(10) as i32,
            None => score -= found.min(10) as i32,
        }
        positions.push(found);
        last = Some(found);
        at = found + 1;
    }
    Some((score, positions))
}

fn highlighted(text: &str, char_positions: &[usize]) -> StyledText {
    let mut ranges = Vec::new();
    for (ci, (bi, ch)) in text.char_indices().enumerate() {
        if char_positions.contains(&ci) {
            ranges.push((
                bi..bi + ch.len_utf8(),
                HighlightStyle {
                    color: Some(theme::accent()),
                    ..Default::default()
                },
            ));
        }
    }
    StyledText::new(SharedString::from(text.to_owned())).with_highlights(ranges)
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let start = self.selected.saturating_sub(MAX_ROWS - 1);
        let rows = self
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(MAX_ROWS)
            .map(|(ix, item)| {
                let selected = ix == self.selected;
                let row = div()
                    .id(("palette-row", ix))
                    .px_3()
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_sm()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::surface_hover()))
                    .hover(|d| d.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = ix;
                        this.confirm(cx);
                    }));
                match item {
                    Item::Command {
                        title,
                        key,
                        positions,
                        ..
                    } => {
                        row.child(highlighted(title, positions))
                            .when_some(key.clone(), |d, key| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .font_family(theme::MONO)
                                        .text_color(theme::text_faint())
                                        .child(key),
                                )
                            })
                    }
                    Item::File(f) => {
                        // Byte offsets from the backend → char positions.
                        let chars: Vec<usize> = f
                            .path
                            .char_indices()
                            .enumerate()
                            .filter(|(_, (b, _))| f.positions.contains(&(*b as u32)))
                            .map(|(c, _)| c)
                            .collect();
                        row.child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(highlighted(&f.path, &chars)),
                        )
                    }
                }
            })
            .collect::<Vec<_>>();
        let empty = self.items.is_empty();
        let hint = match self.mode {
            Mode::Prompt { hint, .. } => Some(hint),
            _ => None,
        };
        div()
            .id("palette")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(|this, _: &input::Up, _, cx| this.step(-1, cx)))
            .capture_action(cx.listener(|this, _: &input::Down, _, cx| this.step(1, cx)))
            .w(px(560.))
            .p_2()
            .rounded_lg()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .shadow_lg()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme::code_bg())
                    .text_sm()
                    .child(self.input.clone()),
            )
            .children(rows)
            .when_some(self.error.clone(), |d, e| {
                d.child(div().px_3().text_xs().text_color(theme::danger()).child(e))
            })
            .when_some(hint, |d, hint| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme::text_faint())
                        .child(hint),
                )
            })
            .when(empty && self.error.is_none() && hint.is_none(), |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme::text_faint())
                        .child(if self.in_flight {
                            "Searching…"
                        } else {
                            "No matches"
                        }),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy("xyz", "New thread").is_none());
        let (a, pos) = fuzzy("nt", "New thread").unwrap();
        assert_eq!(pos, vec![0, 4]);
        let (b, _) = fuzzy("nt", "Show the conversation").unwrap();
        assert!(a > b);
        let (run, _) = fuzzy("diff", "Show this thread's changes / diff").unwrap();
        let (scattered, _) = fuzzy("diff", "radio fluff").unwrap();
        assert!(run > scattered);
        assert_eq!(fuzzy("", "anything").unwrap().0, 0);
        assert_eq!(fuzzy("NEW", "new thread").unwrap().1, vec![0, 1, 2]);
    }
}
