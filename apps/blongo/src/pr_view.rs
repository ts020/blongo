//! The PR tab: a thread's pull request in full (merge readiness and why
//! not, checks with links to their logs, reviews, review conversations,
//! local commits not pushed), and its title and description edited in
//! place.
//!
//! The detail is fetched when the tab opens or on Refresh (a workspace
//! query, so remote environments work the same) and dropped with the view
//! when the tab closes; only the small status lives on the thread.

use std::sync::Arc;

use blongo_client::Backend;
use blongo_protocol::workspace::{Query, QueryReply};
use blongo_protocol::{CheckState, PrDetail, PrState, PrStatus, ReviewState, ThreadId};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, Hsla, SharedString, Window, div,
    prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::button;

/// Characters of a review comment shown before it is cut.
const COMMENT_PREVIEW: usize = 600;

pub struct PrView {
    backend: Arc<dyn Backend>,
    pub thread_id: ThreadId,
    detail: Option<PrDetail>,
    loading: bool,
    error: Option<SharedString>,
    editing: bool,
    title_input: Entity<TextInput>,
    body_input: Entity<TextInput>,
    /// Result of the last edit.
    message: Option<(bool, SharedString)>,
    show_resolved: bool,
    /// Focus the view at the next render (the form just closed).
    refocus: bool,
    focus: FocusHandle,
    _subscription: gpui::Subscription,
}

impl PrView {
    pub fn new(backend: Arc<dyn Backend>, thread_id: ThreadId, cx: &mut Context<Self>) -> Self {
        let title_input = cx.new(|cx| TextInput::new("Title", false, cx));
        let body_input = cx.new(|cx| TextInput::new("Description", true, cx));
        let subscription = cx.subscribe(&title_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.save(cx),
            InputEvent::Cancel => this.stop_edit(cx),
            _ => {}
        });
        let mut this = Self {
            backend,
            thread_id,
            detail: None,
            loading: false,
            error: None,
            editing: false,
            title_input,
            body_input,
            message: None,
            show_resolved: false,
            refocus: false,
            focus: cx.focus_handle(),
            _subscription: subscription,
        };
        this.reload(cx);
        this
    }

    /// Fetch the detail again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        cx.notify();
        let query = Query::PrDetail {
            thread_id: self.thread_id,
        };
        crate::query::ask(
            &self.backend,
            query,
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                this.loading = false;
                match result {
                    Ok(QueryReply::PrDetail(detail)) => {
                        this.detail = Some(*detail);
                        this.error = None;
                    }
                    Ok(_) => {}
                    Err(err) => this.error = Some(err.into()),
                }
                cx.notify();
            },
        );
    }

    /// The thread's status changed (a poll): fetch again when it is news
    /// to the detail shown.
    pub fn on_status(&mut self, status: &PrStatus, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else {
            return;
        };
        if status.error.is_none() && *status != detail.status {
            self.reload(cx);
        }
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else {
            return;
        };
        let (title, body) = (detail.status.title.clone(), detail.body.clone());
        self.title_input.update(cx, |i, cx| i.set_text(&title, cx));
        self.body_input.update(cx, |i, cx| i.set_text(&body, cx));
        self.editing = true;
        self.message = None;
        window.focus(&self.title_input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Leave the form; keys go to the view again (the inputs are gone).
    fn stop_edit(&mut self, cx: &mut Context<Self>) {
        self.editing = false;
        self.refocus = true;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(detail) = &self.detail else {
            return;
        };
        let title = self.title_input.read(cx).text().trim().to_owned();
        let body = self.body_input.read(cx).text().to_owned();
        let title = (title != detail.status.title).then_some(title);
        let body = (body != detail.body).then_some(body);
        if title.is_none() && body.is_none() {
            return self.stop_edit(cx);
        }
        let query = Query::PrEdit {
            thread_id: self.thread_id,
            title,
            body,
        };
        crate::query::ask(
            &self.backend,
            query,
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                match result {
                    Ok(QueryReply::Done(text)) => {
                        this.stop_edit(cx);
                        this.message = Some((true, text.into()));
                        this.reload(cx);
                    }
                    Ok(_) => {}
                    Err(err) => this.message = Some((false, err.into())),
                }
                cx.notify();
            },
        );
    }
}

impl Focusable for PrView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .mt_3()
        .mb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(title)
}

fn check_glyph(state: CheckState) -> (&'static str, Hsla) {
    match state {
        CheckState::Success => ("✓", theme::success().into()),
        CheckState::Failure => ("✗", theme::danger().into()),
        CheckState::Pending => ("…", theme::warning().into()),
        CheckState::Neutral => ("–", theme::text_faint()),
    }
}

fn review_label(state: ReviewState) -> (&'static str, Hsla) {
    match state {
        ReviewState::Approved => ("approved", theme::success().into()),
        ReviewState::ChangesRequested => ("requested changes", theme::danger().into()),
        ReviewState::Commented => ("commented", theme::text_muted().into()),
        ReviewState::Dismissed => ("dismissed", theme::text_faint()),
        ReviewState::Pending => ("pending", theme::text_faint()),
    }
}

/// `1m 30s`.
pub fn duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// An `https` link GitHub gave (check logs); anything else is not opened.
fn open_https(url: &str, cx: &mut App) {
    if url.starts_with("https://") {
        cx.open_url(url);
    }
}

fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(COMMENT_PREVIEW).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

impl Render for PrView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) {
            window.focus(&self.focus, cx);
        }
        let mut root = div()
            .id("pr-view")
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
        let Some(detail) = &self.detail else {
            return root.child(
                div()
                    .text_xs()
                    .text_color(match &self.error {
                        Some(_) => theme::danger(),
                        None => theme::text_muted(),
                    })
                    .child(match &self.error {
                        Some(err) => err.clone(),
                        None => "Loading the pull request…".into(),
                    }),
            );
        };
        let link = detail.link.clone();
        let status = &detail.status;
        let state = match status.state {
            PrState::Open => "Open",
            PrState::Draft => "Draft",
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
        };
        let number = link.as_ref().map_or(0, |l| l.number);

        // Title row and actions.
        let mut actions = div().flex().gap_2().child(button(
            "pr-refresh".into(),
            if self.loading {
                "Refreshing…"
            } else {
                "Refresh"
            },
            theme::surface_hover(),
            theme::text(),
            cx.listener(|this, _, _, cx| this.reload(cx)),
        ));
        if let Some(link) = link.clone() {
            actions = actions.child(button(
                "pr-open".into(),
                "Open on GitHub",
                theme::surface_hover(),
                theme::text(),
                move |_, _, cx| {
                    if let Some(url) = crate::pr::safe_url(&link) {
                        cx.open_url(&url);
                    }
                },
            ));
        }
        if detail.can_edit && !self.editing {
            actions = actions.child(button(
                "pr-edit".into(),
                "Edit",
                theme::surface_hover(),
                theme::text(),
                cx.listener(|this, _, window, cx| this.start_edit(window, cx)),
            ));
        }
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
                                .child(SharedString::from(format!("{} #{number}", status.title))),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child(
                                    div()
                                        .text_color(crate::pr::color(status.badge()))
                                        .child(state),
                                )
                                .child(SharedString::from(format!(
                                    "{} wants to merge {} into {}",
                                    detail.author,
                                    link.as_ref().map_or("?", |l| l.head_branch.as_str()),
                                    link.as_ref().map_or("?", |l| l.base_branch.as_str()),
                                )))
                                .child(SharedString::from(format!(
                                    "+{} −{} in {} file{}",
                                    detail.additions,
                                    detail.deletions,
                                    detail.changed_files,
                                    if detail.changed_files == 1 { "" } else { "s" }
                                )))
                                .when(link.as_ref().is_some_and(|l| l.read_only), |d| {
                                    d.child("read-only")
                                }),
                        ),
                )
                .child(actions),
        );
        if self.editing {
            root = root.child(
                div()
                    .mt_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .p_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .child(self.title_input.clone()),
                    )
                    .child(
                        div()
                            .min_h(px(160.))
                            .p_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .child(self.body_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button(
                                "pr-save".into(),
                                "Save to GitHub",
                                theme::accent_bg(),
                                theme::text(),
                                cx.listener(|this, _, _, cx| this.save(cx)),
                            ))
                            .child(button(
                                "pr-cancel".into(),
                                "Cancel",
                                theme::surface_hover(),
                                theme::text(),
                                cx.listener(|this, _, _, cx| this.stop_edit(cx)),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_faint())
                                    .child("Enter in the title saves"),
                            ),
                    ),
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

        // Merge readiness.
        let blockers = detail.blockers();
        let ready = blockers.is_empty();
        let mut merge = div()
            .mt_3()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(if ready {
                theme::success()
            } else {
                theme::border()
            })
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if ready {
                        theme::success()
                    } else {
                        theme::text()
                    })
                    .child(if ready {
                        "Ready to merge"
                    } else {
                        "Not ready to merge"
                    }),
            );
        for reason in blockers {
            merge = merge.child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child(SharedString::from(format!("• {reason}"))),
            );
        }
        let local = match (detail.ahead, detail.behind) {
            (Some(a), Some(b)) => format!(
                "This folder: {a} commit{} not pushed, {b} behind the remote branch, {} uncommitted file{}",
                if a == 1 { "" } else { "s" },
                detail.uncommitted,
                if detail.uncommitted == 1 { "" } else { "s" },
            ),
            _ => format!(
                "The branch is not checked out in this folder ({} uncommitted file{})",
                detail.uncommitted,
                if detail.uncommitted == 1 { "" } else { "s" },
            ),
        };
        merge = merge.child(
            div()
                .text_xs()
                .text_color(theme::text_faint())
                .child(SharedString::from(local)),
        );
        root = root.child(merge);

        // Checks.
        root = root.child(section("CHECKS"));
        if detail.checks.is_empty() {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child("No checks on the head commit."),
            );
        }
        for (ix, check) in detail.checks.iter().enumerate() {
            let (glyph, color) = check_glyph(check.state);
            let name = match &check.workflow {
                Some(w) => format!("{w} / {}", check.name),
                None => check.name.clone(),
            };
            let url = check.url.clone();
            root = root.child(
                div()
                    .id(("pr-check", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_0p5()
                    .text_xs()
                    .child(div().w(px(12.)).text_color(color).child(glyph))
                    .child(div().flex_1().child(SharedString::from(name)))
                    .child(
                        div()
                            .text_color(theme::text_muted())
                            .child(SharedString::from(check.detail.clone())),
                    )
                    .when_some(check.duration_secs, |d, secs| {
                        d.child(
                            div()
                                .w(px(64.))
                                .text_color(theme::text_faint())
                                .child(SharedString::from(duration(secs))),
                        )
                    })
                    .when_some(url, |d, url| {
                        d.child(
                            div()
                                .id(("pr-check-log", ix))
                                .text_color(theme::accent())
                                .cursor_pointer()
                                .child("Logs")
                                .on_click(move |_, _, cx| open_https(&url, cx)),
                        )
                    }),
            );
        }
        if detail.more_checks > 0 {
            root = root.child(div().text_xs().text_color(theme::text_faint()).child(
                SharedString::from(format!("and {} more on GitHub", detail.more_checks)),
            ));
        }

        // Reviews.
        root = root.child(section("REVIEWS"));
        if detail.reviews.is_empty() {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child("No reviews yet."),
            );
        }
        for review in &detail.reviews {
            let (label, color) = review_label(review.state);
            root = root.child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(SharedString::from(review.author.clone()))
                    .child(div().text_color(color).child(label)),
            );
        }

        // Conversations, unresolved first.
        let resolved = detail.threads.iter().filter(|t| t.resolved).count();
        root = root.child(section("CONVERSATIONS"));
        if detail.threads.is_empty() {
            root = root.child(
                div()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child("No review comments."),
            );
        }
        let shown = detail.threads.iter().filter(|t| !t.resolved).chain(
            detail
                .threads
                .iter()
                .filter(|t| t.resolved && self.show_resolved),
        );
        for (ix, thread) in shown.enumerate() {
            let place = match thread.line {
                Some(line) => format!("{}:{line}", thread.path),
                None => thread.path.clone(),
            };
            let mut card = div()
                .id(("pr-thread", ix))
                .mt_1()
                .p_2()
                .rounded_md()
                .bg(theme::surface())
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(SharedString::from(place))
                        .when(thread.resolved, |d| d.child("resolved"))
                        .when(thread.outdated, |d| d.child("outdated")),
                );
            for comment in &thread.comments {
                card = card.child(
                    div()
                        .text_xs()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(SharedString::from(comment.author.clone())),
                        )
                        .child(
                            div()
                                .whitespace_normal()
                                .child(SharedString::from(preview(&comment.body))),
                        ),
                );
            }
            if thread.more > 0 {
                card = card.child(div().text_xs().text_color(theme::text_faint()).child(
                    SharedString::from(format!("{} more on GitHub", thread.more)),
                ));
            }
            root = root.child(card);
        }
        if resolved > 0 {
            root = root.child(
                div()
                    .id("pr-toggle-resolved")
                    .mt_1()
                    .text_xs()
                    .text_color(theme::accent())
                    .cursor_pointer()
                    .child(SharedString::from(if self.show_resolved {
                        "Hide resolved".to_owned()
                    } else {
                        format!("Show {resolved} resolved")
                    }))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_resolved = !this.show_resolved;
                        cx.notify();
                    })),
            );
        }
        if detail.more_threads > 0 {
            root = root.child(div().text_xs().text_color(theme::text_faint()).child(
                SharedString::from(format!(
                    "and {} more conversations on GitHub",
                    detail.more_threads
                )),
            ));
        }

        // Description (the edit form shows it while editing).
        if !self.editing {
            root = root.child(section("DESCRIPTION"));
            if detail.body.trim().is_empty() {
                root = root.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_faint())
                        .child("No description."),
                );
            }
            for line in detail.body.trim().lines() {
                root = root.child(
                    div()
                        .min_h(px(14.))
                        .text_xs()
                        .whitespace_normal()
                        .child(SharedString::from(line.to_owned())),
                );
            }
        }
        if let Some((ok, text)) = &self.message {
            root = root.child(
                div()
                    .mt_2()
                    .text_xs()
                    .text_color(if *ok {
                        theme::success()
                    } else {
                        theme::danger()
                    })
                    .child(text.clone()),
            );
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_previews() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(90), "1m 30s");
        assert_eq!(duration(3720), "1h 2m");
        assert_eq!(preview("short"), "short");
        assert_eq!(
            preview(&"x".repeat(700)).chars().count(),
            COMMENT_PREVIEW + 1
        );
    }
}
