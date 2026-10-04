//! The Create PR form, shown in the PR tab of a thread with its own
//! worktree and no pull request yet: what the branch holds against the
//! base, title, description, base, branch name, draft, and a commit
//! message for uncommitted changes. The agent can draft the text; nothing
//! is committed, renamed or pushed until the user presses Create.

use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::workspace::{Query, QueryReply};
use blongo_protocol::{PrCreateRequest, PrPrepare, ThreadId};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, SharedString, Window, div,
    prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::button;

/// Uncommitted paths listed before "and N more".
const FILES_SHOWN: usize = 12;

pub struct PrCreateView {
    backend: Arc<dyn Backend>,
    pub thread_id: ThreadId,
    prep: Option<PrPrepare>,
    loading: bool,
    error: Option<SharedString>,
    title: Entity<TextInput>,
    body: Entity<TextInput>,
    base: Entity<TextInput>,
    branch: Entity<TextInput>,
    commit: Entity<TextInput>,
    draft: bool,
    /// The agent is drafting.
    drafting: bool,
    /// The create is on its way.
    creating: bool,
    /// Result of the last draft or create.
    message: Option<(bool, SharedString)>,
    focus: FocusHandle,
    _subscriptions: [gpui::Subscription; 1],
}

impl PrCreateView {
    pub fn new(backend: Arc<dyn Backend>, thread_id: ThreadId, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| TextInput::new("Title", false, cx));
        let body = cx.new(|cx| TextInput::new("Description (Markdown)", true, cx));
        let base = cx.new(|cx| TextInput::new("Base branch", false, cx));
        let branch = cx.new(|cx| TextInput::new("Branch", false, cx));
        let commit = cx.new(|cx| TextInput::new("Commit message", false, cx));
        // Only the Create button commits and pushes: Enter in the title
        // does nothing.
        let subscriptions = [cx.subscribe(&title, |_, _, _: &InputEvent, _| {})];
        let mut this = Self {
            backend,
            thread_id,
            prep: None,
            loading: false,
            error: None,
            title,
            body,
            base,
            branch,
            commit,
            draft: false,
            drafting: false,
            creating: false,
            message: None,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    /// Read the branch's state again (the form keeps what was typed).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        cx.notify();
        crate::query::ask(
            &self.backend,
            Query::PrPrepare {
                thread_id: self.thread_id,
            },
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.loading = false;
                match result {
                    Ok(QueryReply::PrPrepare(prep)) => {
                        let first = this.prep.is_none();
                        this.fill(&prep, first, cx);
                        this.prep = Some(*prep);
                        this.error = None;
                    }
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            },
        );
    }

    /// Put the prepared values into the empty fields (all of them the
    /// first time).
    fn fill(&mut self, prep: &PrPrepare, first: bool, cx: &mut Context<Self>) {
        let set = |input: &Entity<TextInput>, value: &str, cx: &mut Context<Self>| {
            input.update(cx, |i, cx| {
                if first || i.text().trim().is_empty() {
                    i.set_text(value, cx);
                }
            });
        };
        set(&self.title, &prep.title, cx);
        set(&self.body, prep.template.as_deref().unwrap_or(""), cx);
        set(&self.base, &prep.base, cx);
        set(&self.branch, &prep.suggested_branch, cx);
        let message = if prep.title.is_empty() {
            "Work in progress"
        } else {
            &prep.title
        };
        set(&self.commit, message, cx);
    }

    /// Ask the thread's agent for a title and description (it answers in
    /// the conversation; the form is filled when it is done).
    fn ask_draft(&mut self, cx: &mut Context<Self>) {
        let Some(prep) = &self.prep else {
            return;
        };
        if self.drafting {
            return;
        }
        self.drafting = true;
        self.message = Some((
            true,
            "The agent is drafting in the conversation; the form fills in when it answers.".into(),
        ));
        cx.notify();
        let query = Query::PrDraft {
            thread_id: self.thread_id,
            prompt: prep.draft_prompt.clone(),
        };
        crate::query::ask(
            &self.backend,
            query,
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.drafting = false;
                match result {
                    Ok(QueryReply::PrDraft(draft)) => {
                        this.title.update(cx, |i, cx| i.set_text(&draft.title, cx));
                        this.body.update(cx, |i, cx| i.set_text(&draft.body, cx));
                        if let Some(m) = &draft.commit_message {
                            this.commit.update(cx, |i, cx| i.set_text(m, cx));
                        }
                        this.message = Some((true, "Drafted by the agent; check it over.".into()));
                    }
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let Some(prep) = &self.prep else {
            return;
        };
        if self.creating {
            return;
        }
        let commit = self.commit.read(cx).text().trim().to_owned();
        let request = PrCreateRequest {
            title: self.title.read(cx).text().trim().to_owned(),
            body: self.body.read(cx).text().to_owned(),
            base: self.base.read(cx).text().trim().to_owned(),
            draft: self.draft,
            branch: self.branch.read(cx).text().trim().to_owned(),
            commit_message: (!prep.uncommitted.is_empty() && !commit.is_empty()).then_some(commit),
        };
        if request.title.is_empty() {
            self.message = Some((false, "Enter a title".into()));
            return cx.notify();
        }
        self.creating = true;
        self.message = None;
        cx.notify();
        crate::query::ask(
            &self.backend,
            Query::PrCreate {
                thread_id: self.thread_id,
                request,
            },
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.creating = false;
                match result {
                    // The link arrives as an event; the tab switches to it.
                    Ok(QueryReply::Done(text)) => this.message = Some((true, text.into())),
                    Ok(_) => {}
                    Err(err) => {
                        this.message = Some((false, err.into()));
                        // A step may have happened (commit, rename).
                        this.reload(cx);
                    }
                }
                cx.notify();
            },
        );
    }
}

impl Focusable for PrCreateView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn label(text: &'static str) -> gpui::Div {
    div()
        .mt_3()
        .mb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(text)
}

fn field(input: &Entity<TextInput>) -> gpui::Div {
    div()
        .p_1()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .child(input.clone())
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

impl Render for PrCreateView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .id("pr-create")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .px_6()
            .pt_4()
            // Room to scroll past the composer floating over the bottom.
            .pb(px(140.))
            .flex()
            .flex_col()
            .text_sm();
        let Some(prep) = &self.prep else {
            return root.child(
                div()
                    .text_xs()
                    .text_color(match &self.error {
                        Some(_) => theme::danger(),
                        None => theme::text_muted(),
                    })
                    .child(match &self.error {
                        Some(err) => err.clone(),
                        None => "Reading the branch…".into(),
                    }),
            );
        };

        root = root.child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Create a pull request"),
                        )
                        .child(div().text_xs().text_color(theme::text_muted()).child(
                            SharedString::from(format!(
                                "{} · {} into {} · {}, {}",
                                prep.repo,
                                prep.branch,
                                prep.base,
                                plural(prep.commits.len(), "commit"),
                                plural(prep.uncommitted.len(), "uncommitted file"),
                            )),
                        )),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(button(
                            "pr-create-reload".into(),
                            if self.loading {
                                "Reading…"
                            } else {
                                "Refresh"
                            },
                            theme::surface_hover(),
                            theme::text(),
                            cx.listener(|this, _, _, cx| this.reload(cx)),
                        ))
                        .child(button(
                            "pr-draft".into(),
                            if self.drafting {
                                "Drafting…"
                            } else {
                                "Ask agent to draft"
                            },
                            theme::surface_hover(),
                            theme::text(),
                            cx.listener(|this, _, _, cx| this.ask_draft(cx)),
                        )),
                ),
        );
        if !prep.can_push {
            root = root.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(theme::warning())
                    .child("The GitHub token cannot push to this repository."),
            );
        }
        if let Some(err) = &self.error {
            root = root.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(theme::danger())
                    .child(err.clone()),
            );
        }

        root = root
            .child(label("TITLE"))
            .child(field(&self.title))
            .child(label("DESCRIPTION"))
            .child(field(&self.body).min_h(px(160.)))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(label("BASE"))
                            .child(field(&self.base)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(label(if prep.pushed {
                                "BRANCH (pushed; keeps its name)"
                            } else {
                                "BRANCH (renamed before the first push)"
                            }))
                            .child(field(&self.branch)),
                    ),
            );
        if !prep.uncommitted.is_empty() {
            root = root
                .child(label("COMMIT THE UNCOMMITTED CHANGES AS"))
                .child(field(&self.commit));
            let mut files = prep
                .uncommitted
                .iter()
                .take(FILES_SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if prep.uncommitted.len() > FILES_SHOWN {
                files.push_str(&format!(
                    " and {} more",
                    prep.uncommitted.len() - FILES_SHOWN
                ));
            }
            root = root.child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child(SharedString::from(files)),
            );
        }

        let draft = self.draft;
        let create_label = match (self.creating, prep.uncommitted.is_empty()) {
            (true, _) => "Creating…",
            (false, false) => "Commit, push and create",
            (false, true) => "Push and create",
        };
        root = root.child(
            div()
                .mt_3()
                .flex()
                .items_center()
                .gap_2()
                .child(button(
                    "pr-create-submit".into(),
                    create_label,
                    theme::accent_bg(),
                    theme::text(),
                    cx.listener(|this, _, _, cx| this.create(cx)),
                ))
                .child(
                    div()
                        .id("pr-create-draft")
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .cursor_pointer()
                        .child(
                            div()
                                .w(px(14.))
                                .h(px(14.))
                                .rounded_sm()
                                .border_1()
                                .border_color(theme::border())
                                .when(draft, |d| d.bg(theme::accent()))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(theme::text())
                                .child(if draft { "✓" } else { "" }),
                        )
                        .child("Draft")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.draft = !this.draft;
                            cx.notify();
                        })),
                )
                .child(div().text_xs().text_color(theme::text_faint()).child(
                    "Nothing is committed or pushed until you press it; never force-pushed.",
                )),
        );
        if let Some((ok, text)) = &self.message {
            root = root.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(if *ok {
                        theme::text_muted()
                    } else {
                        theme::danger()
                    })
                    .child(text.clone()),
            );
        }

        if !prep.commits.is_empty() {
            root = root.child(label("COMMITS"));
            for c in &prep.commits {
                root = root.child(div().text_xs().child(SharedString::from(format!("• {c}"))));
            }
        }
        if !prep.diff_stat.is_empty() {
            root = root.child(label("CHANGED FILES"));
            for line in prep.diff_stat.lines() {
                root = root.child(
                    div()
                        .text_xs()
                        .font_family(theme::MONO)
                        .whitespace_nowrap()
                        .text_color(theme::text_muted())
                        .child(SharedString::from(line.to_owned())),
                );
            }
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "commit"), "1 commit");
        assert_eq!(plural(0, "commit"), "0 commits");
    }
}
