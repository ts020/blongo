//! "New thread from…": a project's open pull requests, issues and free
//! branches (or a URL, `#number` or branch name typed in), each a click
//! away from a thread in its own worktree. An issue's text lands in the
//! new thread's composer for the user to read before anything is sent.

use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::workspace::{Query, QueryReply};
use blongo_protocol::{ForgeCandidates, ProjectId, ThreadId, ThreadSource};
use gpui::{Context, Entity, EventEmitter, FontWeight, SharedString, Window, div, prelude::*};

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::button;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Pulls,
    Issues,
    Branches,
}

pub enum OpenFromEvent {
    /// The thread exists; `draft` goes into its composer.
    Opened {
        thread_id: ThreadId,
        draft: Option<String>,
    },
}

pub struct OpenFromView {
    backend: Arc<dyn Backend>,
    pub project_id: ProjectId,
    project_name: SharedString,
    provider: blongo_protocol::ProviderKind,
    candidates: Option<ForgeCandidates>,
    loading: bool,
    error: Option<SharedString>,
    tab: Tab,
    input: Entity<TextInput>,
    /// A thread is being opened.
    opening: bool,
    message: Option<(bool, SharedString)>,
    _input_events: gpui::Subscription,
}

impl EventEmitter<OpenFromEvent> for OpenFromView {}

impl OpenFromView {
    pub fn new(
        backend: Arc<dyn Backend>,
        project_id: ProjectId,
        project_name: String,
        provider: blongo_protocol::ProviderKind,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("URL or #number", false, cx));
        let sub = cx.subscribe(&input, |this, _, event, cx| {
            if let InputEvent::Submit = event {
                this.open_typed(cx);
            }
        });
        let mut this = Self {
            backend,
            project_id,
            project_name: project_name.into(),
            provider,
            candidates: None,
            loading: false,
            error: None,
            tab: Tab::Pulls,
            input,
            opening: false,
            message: None,
            _input_events: sub,
        };
        this.reload(cx);
        this
    }

    /// Shown again: what was said last time is old news.
    pub fn reopen(&mut self, cx: &mut Context<Self>) {
        self.message = None;
        self.reload(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.error = None;
        cx.notify();
        crate::query::ask(
            &self.backend,
            Query::ForgeCandidates {
                project_id: self.project_id,
            },
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.loading = false;
                match result {
                    Ok(QueryReply::Candidates(c)) => this.candidates = Some(*c),
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            },
        );
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        let hint = match tab {
            Tab::Pulls | Tab::Issues => "URL or #number",
            Tab::Branches => "Branch name",
        };
        self.input.update(cx, |i, cx| i.set_placeholder(hint, cx));
        cx.notify();
    }

    /// Open what was typed, as the current tab's kind.
    fn open_typed(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let source = match self.tab {
            Tab::Pulls => ThreadSource::Pull { reference: text },
            Tab::Issues => ThreadSource::Issue { reference: text },
            Tab::Branches => ThreadSource::Branch { name: text },
        };
        self.open(source, cx);
    }

    fn open(&mut self, source: ThreadSource, cx: &mut Context<Self>) {
        if self.opening {
            return;
        }
        self.opening = true;
        self.message = Some((true, "Opening…".into()));
        cx.notify();
        crate::query::ask(
            &self.backend,
            Query::ThreadFrom {
                project_id: Some(self.project_id),
                thread_id: ThreadId::new(),
                source,
                provider: self.provider,
                model: None,
            },
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.opening = false;
                match result {
                    Ok(QueryReply::ThreadOpened(o)) => {
                        this.message = Some((true, o.message.into()));
                        this.input.update(cx, |i, cx| i.set_text("", cx));
                        // Its branch is taken now.
                        this.candidates = None;
                        this.reload(cx);
                        cx.emit(OpenFromEvent::Opened {
                            thread_id: o.thread_id,
                            draft: o.draft,
                        });
                    }
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }

    fn tab_chip(&self, tab: Tab, label: String, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.tab == tab;
        let id = match tab {
            Tab::Pulls => "from-tab-pulls",
            Tab::Issues => "from-tab-issues",
            Tab::Branches => "from-tab-branches",
        };
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .text_xs()
            .bg(if on {
                theme::accent_bg()
            } else {
                theme::surface_hover()
            })
            .text_color(if on {
                theme::text()
            } else {
                theme::text_muted()
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
    }
}

/// A clickable row: title, then faint facts.
fn row(
    id: SharedString,
    title: String,
    facts: String,
    tag: Option<(&'static str, gpui::Rgba)>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(theme::border())
        .cursor_pointer()
        .hover(|d| d.bg(theme::surface_hover()))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_sm().child(title))
                .when_some(tag, |d, (t, c)| {
                    d.child(div().text_xs().text_color(c).child(t))
                }),
        )
        .child(div().text_xs().text_color(theme::text_faint()).child(facts))
}

/// "2026-10-04" of an RFC 3339 time.
fn day(at: &str) -> &str {
    at.get(..10).unwrap_or(at)
}

impl Render for OpenFromView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = self.candidates.clone().unwrap_or_default();
        let mut root = div()
            .id("open-from")
            .size_full()
            .overflow_y_scroll()
            .px_6()
            .pt_4()
            .pb_6()
            .flex()
            .flex_col()
            .gap_2()
            .text_sm();
        root = root.child(div().text_xs().text_color(theme::text_muted()).child(
            SharedString::from(format!(
                "A new thread in {} works in its own worktree on the branch you pick{}.",
                self.project_name,
                if c.repo.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", c.repo)
                }
            )),
        ));
        let tabs = div()
            .flex()
            .items_center()
            .gap_2()
            .child(self.tab_chip(Tab::Pulls, format!("Pull requests ({})", c.pulls.len()), cx))
            .child(self.tab_chip(Tab::Issues, format!("Issues ({})", c.issues.len()), cx))
            .child(self.tab_chip(
                Tab::Branches,
                format!("Branches ({})", c.branches.len()),
                cx,
            ))
            .child(div().flex_1())
            .child(button(
                "from-refresh".into(),
                if self.loading {
                    "Loading…"
                } else {
                    "Refresh"
                },
                theme::surface_hover(),
                theme::text(),
                cx.listener(|this, _, _, cx| this.reload(cx)),
            ));
        root = root.child(tabs);
        root = root.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme::code_bg())
                        .child(self.input.clone()),
                )
                .child(button(
                    "from-open".into(),
                    "Open",
                    theme::accent_bg(),
                    theme::text(),
                    cx.listener(|this, _, _, cx| this.open_typed(cx)),
                )),
        );
        if let Some((ok, m)) = &self.message {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(if *ok {
                        theme::success()
                    } else {
                        theme::danger()
                    })
                    .child(m.clone()),
            );
        }
        for problem in self
            .error
            .iter()
            .cloned()
            .chain(c.problem.clone().map(Into::into))
        {
            root = root.child(div().text_xs().text_color(theme::warning()).child(problem));
        }
        let mut list = div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme::border());
        let empty = |what: &'static str| {
            div()
                .p_3()
                .text_xs()
                .text_color(theme::text_muted())
                .child(what)
        };
        match self.tab {
            Tab::Pulls => {
                if c.pulls.is_empty() && !self.loading {
                    list = list.child(empty("No open pull requests."));
                }
                for p in &c.pulls {
                    let number = p.number;
                    let tag = if p.fork {
                        Some(("fork · read-only", theme::text_muted()))
                    } else if p.draft {
                        Some(("draft", theme::text_muted()))
                    } else {
                        None
                    };
                    let facts = format!(
                        "#{number} · {} · {} · {}{}",
                        p.author,
                        p.head_branch,
                        day(&p.updated_at),
                        if p.mine { " · for you" } else { "" }
                    );
                    list = list.child(
                        row(
                            format!("from-pull-{number}").into(),
                            p.title.clone(),
                            facts,
                            tag,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open(
                                ThreadSource::Pull {
                                    reference: format!("#{number}"),
                                },
                                cx,
                            )
                        })),
                    );
                }
            }
            Tab::Issues => {
                if c.issues.is_empty() && !self.loading {
                    list = list.child(empty("No open issues."));
                }
                for i in &c.issues {
                    let number = i.number;
                    let facts = format!(
                        "#{number} · {} · {}{}",
                        i.author,
                        day(&i.updated_at),
                        if i.mine { " · assigned to you" } else { "" }
                    );
                    list = list.child(
                        row(
                            format!("from-issue-{number}").into(),
                            i.title.clone(),
                            facts,
                            None,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open(
                                ThreadSource::Issue {
                                    reference: format!("#{number}"),
                                },
                                cx,
                            )
                        })),
                    );
                }
            }
            Tab::Branches => {
                if c.branches.is_empty() && !self.loading {
                    list = list.child(empty("No branch is free: each one is checked out already."));
                }
                for (ix, b) in c.branches.iter().enumerate() {
                    let name = b.name.clone();
                    list = list.child(
                        row(
                            format!("from-branch-{ix}").into(),
                            b.name.clone(),
                            if b.remote_only {
                                "on the remote only; fetched when opened".into()
                            } else {
                                "local".into()
                            },
                            None,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open(ThreadSource::Branch { name: name.clone() }, cx)
                        })),
                    );
                }
            }
        }
        root.child(list).child(
            div()
                .text_xs()
                .text_color(theme::text_faint())
                .font_weight(FontWeight::NORMAL)
                .child(
                    "A pull request from a fork opens read-only: Blongo never pushes to it. An \
                     issue's text goes into the composer; nothing is sent until you send it.",
                ),
        )
    }
}
