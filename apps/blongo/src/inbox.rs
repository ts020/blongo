//! The review inbox: pull / merge requests waiting for the user's review on
//! GitHub and GitLab (tokens from `forge.json`, set in Settings), their
//! changes in the diff panel, and comments posted back as one review.
//!
//! Requests run on the network thread (`blongo_client::net`), which is
//! only started once the inbox is opened with a token configured.

use blongo_client::forge::{self, Forge, PrDetail, PullRequest, ReviewComment, TokenFile};
use blongo_protocol::workspace::DiffFileStat;
use gpui::{Context, Entity, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use crate::diff::{DiffEvent, DiffView, Source};
use crate::input::TextInput;
use crate::theme;
use crate::timeline::button;

pub struct InboxView {
    forges: Vec<Forge>,
    prs: Vec<PullRequest>,
    loading: usize,
    errors: Vec<SharedString>,
    selected: Option<usize>,
    detail: Option<PrDetail>,
    diff: Option<Entity<DiffView>>,
    summary_input: Entity<TextInput>,
    message: Option<(bool, SharedString)>,
    _diff_events: Option<Subscription>,
}

impl InboxView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let summary_input = cx.new(|cx| TextInput::new("Review summary (optional)", false, cx));
        let mut this = Self {
            forges: Vec::new(),
            prs: Vec::new(),
            loading: 0,
            errors: Vec::new(),
            selected: None,
            detail: None,
            diff: None,
            summary_input,
            message: None,
            _diff_events: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.errors.clear();
        self.prs.clear();
        self.selected = None;
        self.diff = None;
        self.detail = None;
        self.forges = match TokenFile::load(&forge::default_path()) {
            Ok(file) => file
                .forges
                .into_iter()
                .filter(|f| !f.token.is_empty())
                .collect(),
            Err(err) => {
                self.errors.push(err.into());
                Vec::new()
            }
        };
        for forge in self.forges.clone() {
            self.loading += 1;
            let task = blongo_client::net::handle().spawn(async move { forge.inbox().await });
            cx.spawn(async move |this, cx| {
                let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
                this.update(cx, |this, cx| {
                    this.loading -= 1;
                    match result {
                        Ok(prs) => {
                            this.prs.extend(prs);
                            this.prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                        }
                        Err(err) => this.errors.push(err.into()),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn forge_for(&self, pr: &PullRequest) -> Option<Forge> {
        self.forges.iter().find(|f| f.kind == pr.kind).cloned()
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = self.prs.get(ix).cloned() else {
            return;
        };
        let Some(forge) = self.forge_for(&pr) else {
            return;
        };
        self.selected = Some(ix);
        self.detail = None;
        self.message = None;
        let title = format!("{} — {}", pr.key(), pr.title);
        let diff = cx.new(|cx| DiffView::new(Source::Patches, title, "Submit review", window, cx));
        self._diff_events = Some(cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| {
            let DiffEvent::Send(comments) = event;
            this.submit(comments.clone(), cx);
        }));
        self.diff = Some(diff.clone());
        let task = blongo_client::net::handle().spawn(async move { forge.detail(&pr).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                match result {
                    Ok(detail) => {
                        let files = detail
                            .files
                            .iter()
                            .map(|f| {
                                (
                                    DiffFileStat {
                                        path: f.path.clone(),
                                        old_path: f.old_path.clone(),
                                        status: f.status,
                                        added: f.added,
                                        removed: f.removed,
                                        binary: false,
                                    },
                                    f.patch.clone(),
                                )
                            })
                            .collect();
                        diff.update(cx, |d, cx| d.set_patches(files, cx));
                        this.detail = Some(detail);
                    }
                    Err(err) => diff.update(cx, |d, cx| d.set_error(err, cx)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn submit(&mut self, comments: Vec<crate::diff::Comment>, cx: &mut Context<Self>) {
        let (Some(ix), Some(detail)) = (self.selected, self.detail.clone()) else {
            return;
        };
        let Some(pr) = self.prs.get(ix).cloned() else {
            return;
        };
        let Some(forge) = self.forge_for(&pr) else {
            return;
        };
        let mut summary = self.summary_input.read(cx).text().trim().to_owned();
        // Forges anchor review comments on the new side; ones on removed
        // lines go into the summary.
        let (new_side, old_side): (Vec<_>, Vec<_>) =
            comments.into_iter().partition(|c| !c.old_side);
        for c in &old_side {
            summary.push_str(&format!(
                "\n\n{} (removed line {}):\n> {}\n{}",
                c.path, c.line, c.code, c.text
            ));
        }
        if new_side.is_empty() && summary.trim().is_empty() {
            self.message = Some((false, "Add a comment or a summary first".into()));
            cx.notify();
            return;
        }
        let comments: Vec<ReviewComment> = new_side
            .into_iter()
            .map(|c| ReviewComment {
                path: c.path,
                line: c.line,
                body: c.text,
            })
            .collect();
        let count = comments.len();
        self.message = Some((true, "Submitting…".into()));
        let task = blongo_client::net::handle()
            .spawn(async move { forge.review(&pr, &detail, summary.trim(), &comments).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.message = Some((
                            true,
                            format!(
                                "Review submitted ({count} inline comment{})",
                                if count == 1 { "" } else { "s" }
                            )
                            .into(),
                        ));
                        this.summary_input.update(cx, |i, cx| i.set_text("", cx));
                        if let Some(diff) = &this.diff {
                            diff.update(cx, |d, cx| d.clear_comments(cx));
                        }
                    }
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl Render for InboxView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("inbox-list")
            .w(px(300.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Review requests"),
                    )
                    .child(button(
                        "inbox-refresh".into(),
                        "Refresh",
                        theme::surface_hover(),
                        theme::text(),
                        cx.listener(|this, _, _, cx| this.refresh(cx)),
                    )),
            );
        if self.forges.is_empty() && self.errors.is_empty() {
            list =
                list.child(
                    div().p_3().text_xs().text_color(theme::text_muted()).child(
                        "No GitHub or GitLab token yet: add one in Settings → Review inbox.",
                    ),
                );
        }
        if self.loading > 0 {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("Loading…"),
            );
        }
        for err in &self.errors {
            list = list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::danger())
                    .child(err.clone()),
            );
        }
        if self.loading == 0
            && !self.forges.is_empty()
            && self.prs.is_empty()
            && self.errors.is_empty()
        {
            list = list.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("Nothing waits for your review."),
            );
        }
        for (ix, pr) in self.prs.iter().enumerate() {
            let selected = self.selected == Some(ix);
            list = list.child(
                div()
                    .id(("pr", ix))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::surface_hover()))
                    .hover(|d| d.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _, window, cx| this.select(ix, window, cx)))
                    .child(div().text_sm().child(SharedString::from(pr.title.clone())))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_faint())
                            .child(format!(
                                "{} · {} · {}",
                                pr.key(),
                                pr.author,
                                pr.kind.label()
                            )),
                    ),
            );
        }
        let right = match &self.diff {
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme::text_muted())
                .child("Pick a pull request to review its changes.")
                .into_any_element(),
            Some(diff) => div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(diff.clone()))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(theme::border())
                        .child(
                            div()
                                .flex_1()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme::code_bg())
                                .text_sm()
                                .child(self.summary_input.clone()),
                        )
                        .when_some(self.message.clone(), |d, (ok, m)| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(if ok {
                                        theme::success()
                                    } else {
                                        theme::danger()
                                    })
                                    .child(m),
                            )
                        }),
                )
                .into_any_element(),
        };
        div().size_full().flex().child(list).child(right)
    }
}
