//! Sidebar: projects → threads, the add-project form and the t3code import.
//!
//! Its own entity, rendered as a cached view: streaming text notifies only
//! the timeline, so the sidebar's element tree is reused frame after frame
//! instead of being rebuilt with every delta. It is notified only when a
//! project or thread actually changes.

use blongo_protocol::{Project, ProjectId, Thread, ThreadId, ThreadStatus};
use gpui::{
    Context, Entity, EventEmitter, Focusable, FontWeight, SharedString, Subscription, Window, div,
    prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;

pub enum SidebarEvent {
    Select(ThreadId),
    NewThread {
        project_id: ProjectId,
        worktree: bool,
    },
    Archive(ThreadId),
    AddProject(String),
    /// The add-project form was dismissed.
    Dismissed,
    ImportT3,
}

pub struct Sidebar {
    pub projects: Vec<Project>,
    /// Newest first.
    pub threads: Vec<Thread>,
    pub selected: Option<ThreadId>,
    pub adding_project: bool,
    pub project_input: Entity<TextInput>,
    /// Shown under the add-project form (a rejected path).
    pub form_notice: Option<SharedString>,
    /// Shown at the bottom (import progress / result).
    pub footer_notice: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project_input = cx.new(|cx| TextInput::new("/path/to/project", false, cx));
        let subscriptions = vec![cx.subscribe_in(
            &project_input,
            window,
            |this, input, event, _, cx| match event {
                InputEvent::Submit => {
                    let path = input.read(cx).text().trim().to_owned();
                    if !path.is_empty() {
                        cx.emit(SidebarEvent::AddProject(path));
                    }
                }
                InputEvent::Cancel => {
                    this.adding_project = false;
                    this.form_notice = None;
                    cx.emit(SidebarEvent::Dismissed);
                    cx.notify();
                }
                InputEvent::SubmitAlt => {}
            },
        )];
        Self {
            projects: Vec::new(),
            threads: Vec::new(),
            selected: None,
            adding_project: false,
            project_input,
            form_notice: None,
            footer_notice: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    pub fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Change one thread in place; notifies only when `f` reports a change.
    pub fn update_thread(
        &mut self,
        id: ThreadId,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Thread) -> bool,
    ) -> bool {
        let changed = self.threads.iter_mut().find(|t| t.id == id).is_some_and(f);
        if changed {
            cx.notify();
        }
        changed
    }

    pub fn set_threads(&mut self, mut threads: Vec<Thread>) {
        threads.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        self.threads = threads;
    }

    fn toggle_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.adding_project = !self.adding_project;
        self.form_notice = None;
        if self.adding_project {
            window.focus(&self.project_input.focus_handle(cx), cx);
        } else {
            cx.emit(SidebarEvent::Dismissed);
        }
        cx.notify();
    }
}

fn small_action(id: SharedString, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_1()
        .rounded_sm()
        .text_xs()
        .text_color(theme::text_muted())
        .hover(|d| d.bg(theme::surface_hover()))
        .cursor_pointer()
        .child(label)
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("blongo"),
            )
            .child(
                div()
                    .id("add-project")
                    .px_2()
                    .rounded_md()
                    .text_color(theme::text_muted())
                    .hover(|d| d.bg(theme::surface_hover()))
                    .cursor_pointer()
                    .child("+ Project")
                    .text_xs()
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_form(window, cx))),
            );

        let add_form = self.adding_project.then(|| {
            div()
                .mx_2()
                .mb_2()
                .p_2()
                .rounded_md()
                .bg(theme::surface())
                .border_1()
                .border_color(theme::border())
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child("Add a project folder (Enter to add, Esc to cancel)"),
                )
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme::code_bg())
                        .text_sm()
                        .child(self.project_input.clone()),
                )
                .when_some(self.form_notice.clone(), |d, notice| {
                    d.child(div().text_xs().text_color(theme::danger()).child(notice))
                })
        });

        let mut list = div()
            .id("projects")
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .gap_0p5();
        for project in &self.projects {
            let project_id = project.id;
            list = list.child(
                div()
                    .id(SharedString::from(format!("p-{project_id}")))
                    .mt_2()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_muted())
                            .child(SharedString::from(project.name.clone())),
                    )
                    .child(
                        small_action(format!("wt-{project_id}").into(), "+ Worktree").on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(SidebarEvent::NewThread {
                                    project_id,
                                    worktree: true,
                                })
                            }),
                        ),
                    )
                    .child(
                        small_action(format!("new-{project_id}").into(), "+ New").on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(SidebarEvent::NewThread {
                                    project_id,
                                    worktree: false,
                                })
                            }),
                        ),
                    ),
            );
            for thread in self.threads.iter().filter(|t| t.project_id == project_id) {
                let thread_id = thread.id;
                let selected = self.selected == Some(thread_id);
                let dot = match thread.status {
                    ThreadStatus::Running => Some(theme::accent()),
                    ThreadStatus::Waiting => Some(theme::warning().into()),
                    ThreadStatus::Failed => Some(theme::danger().into()),
                    ThreadStatus::Idle => None,
                };
                let badge = thread
                    .worktree
                    .is_some()
                    .then_some("⑂")
                    .or(thread.forked_from.is_some().then_some("↳"));
                list = list.child(
                    div()
                        .id(SharedString::from(format!("t-{thread_id}")))
                        .group("thread")
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl_3()
                        .pr_1()
                        .py_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .text_color(if selected {
                            theme::text()
                        } else {
                            theme::text_muted()
                        })
                        .when(selected, |d| d.bg(theme::surface_hover()))
                        .hover(|d| d.bg(theme::surface()))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SidebarEvent::Select(thread_id));
                        }))
                        .child(
                            div()
                                .size(px(7.))
                                .flex_shrink_0()
                                .rounded_full()
                                .when_some(dot, |d, c| d.bg(c)),
                        )
                        .when_some(badge, |d, b| {
                            d.child(div().text_xs().text_color(theme::text_faint()).child(b))
                        })
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(SharedString::from(thread.title.clone())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .child(provider_initial(thread)),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("archive-{thread_id}")))
                                .invisible()
                                .group_hover("thread", |d| d.visible())
                                .px_1()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .hover(|d| d.text_color(theme::danger()))
                                .child("×")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.emit(SidebarEvent::Archive(thread_id));
                                })),
                        ),
                );
            }
        }

        let footer = div()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .gap_1()
            .child(
                small_action("import-t3".into(), "Import from t3code…")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ImportT3))),
            )
            .when_some(self.footer_notice.clone(), |d, n| {
                d.child(div().text_xs().text_color(theme::text_faint()).child(n))
            });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::border())
            .child(header)
            .children(add_form)
            .child(list)
            .child(footer)
    }
}

fn provider_initial(thread: &Thread) -> &'static str {
    match thread.provider {
        blongo_protocol::ProviderKind::Codex => "Cx",
        blongo_protocol::ProviderKind::ClaudeCode => "Cl",
        blongo_protocol::ProviderKind::Antigravity => "Ag",
    }
}
