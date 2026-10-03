//! The window: sidebar (projects → threads), the open thread's timeline and
//! the composer. Fed by the core's event channel; sends commands back.

use std::path::PathBuf;
use std::time::Duration;

use blongo_core::{CoreClient, CoreEvent};
use blongo_protocol::{
    Command, CommandEnvelope, CommandId, EventKind, ItemId, Project, ProjectId, RunId, Thread,
    ThreadId, ThreadStatus,
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window,
    div, prelude::*, px,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::input::{InputEvent, TextInput};
use crate::theme;
use crate::timeline::{Timeline, button};

/// Profiling hook: once the shell is up, make sure a project + thread exist,
/// then send `prompt` after `delay` (see tools/profile.py).
#[derive(Clone)]
pub struct AutoPrompt {
    pub prompt: String,
    pub delay: Duration,
    pub project_dir: PathBuf,
}

pub struct Shell {
    core: CoreClient,
    projects: Vec<Project>,
    /// Newest first.
    threads: Vec<Thread>,
    selected: Option<ThreadId>,
    timeline: Option<Entity<Timeline>>,
    composer: Entity<TextInput>,
    project_input: Entity<TextInput>,
    adding_project: bool,
    pending_project: Option<CommandId>,
    /// Select this thread when its creation event arrives.
    pending_select: Option<ThreadId>,
    /// Last rejected command's reason.
    notice: Option<SharedString>,
    auto_prompt: Option<AutoPrompt>,
    /// Focus to apply on the next render (set where no window is at hand).
    pending_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(
        core: CoreClient,
        events: UnboundedReceiver<CoreEvent>,
        auto_prompt: Option<AutoPrompt>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new(
                "Ask Codex anything…  (Enter to send, Shift+Enter for a new line)",
                true,
                cx,
            )
        });
        let project_input = cx.new(|cx| TextInput::new("/path/to/project", false, cx));
        let subscriptions = vec![
            cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                if let InputEvent::Submit = event {
                    this.send(window, cx);
                }
            }),
            cx.subscribe_in(
                &project_input,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Submit => this.add_project(cx),
                    InputEvent::Cancel => {
                        this.adding_project = false;
                        this.notice = None;
                        window.focus(&this.composer.focus_handle(cx), cx);
                        cx.notify();
                    }
                },
            ),
        ];
        Self::pump(events, cx);
        Self {
            core,
            projects: Vec::new(),
            threads: Vec::new(),
            selected: None,
            timeline: None,
            composer,
            project_input,
            adding_project: false,
            pending_project: None,
            pending_select: None,
            notice: None,
            auto_prompt,
            pending_focus: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Receive core events on the UI thread. Each wake drains everything
    /// queued, so a burst of deltas costs one entity update.
    fn pump(mut events: UnboundedReceiver<CoreEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Some(first) = events.recv().await {
                batch.push(first);
                while let Ok(next) = events.try_recv() {
                    batch.push(next);
                }
                let alive = this.update(cx, |this, cx| {
                    for event in batch.drain(..) {
                        this.on_core_event(event, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    fn thread_mut(&mut self, id: ThreadId) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|t| t.id == id)
    }

    fn selected_status(&self) -> ThreadStatus {
        self.selected
            .and_then(|id| self.thread(id))
            .map_or(ThreadStatus::Idle, |t| t.status)
    }

    fn on_core_event(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::Shell(shell) => {
                self.projects = shell.projects.clone();
                self.threads = shell.threads.clone();
                self.threads
                    .sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
                // Restore: reopen the most recently active thread.
                let recent = shell.threads.first().map(|t| t.id);
                if let Some(id) = recent {
                    self.select(id, cx);
                }
                if self.projects.is_empty() && self.auto_prompt.is_none() {
                    self.adding_project = true;
                    self.pending_focus = Some(self.project_input.focus_handle(cx));
                }
                self.start_auto_prompt(cx);
                cx.notify();
            }
            CoreEvent::Thread(snapshot) => {
                if self.selected == Some(snapshot.thread_id) {
                    let status = self.selected_status();
                    let core = self.core.clone();
                    self.timeline = Some(cx.new(|_| Timeline::new(&snapshot, status, core)));
                    cx.notify();
                }
            }
            CoreEvent::TextDelta {
                thread_id,
                item_id,
                chunk,
            } => {
                if let Some(timeline) = self.timeline_for(thread_id) {
                    timeline.update(cx, |t, cx| t.append(item_id, &chunk, cx));
                }
            }
            CoreEvent::Event(event) => self.on_domain_event(&event.kind, event.command_id, cx),
            CoreEvent::CommandRejected { command_id, reason } => {
                if self.pending_project == Some(command_id) {
                    self.pending_project = None;
                }
                self.notice = Some(reason.into());
                cx.notify();
            }
            CoreEvent::CommandDuplicate { .. } => {}
            CoreEvent::RunFinished { .. } => {
                if self.auto_prompt.is_some() {
                    // tools/profile.py waits for this line.
                    eprintln!("blongo: replay done");
                }
            }
        }
    }

    fn timeline_for(&self, thread_id: ThreadId) -> Option<Entity<Timeline>> {
        self.timeline
            .as_ref()
            .filter(|_| self.selected == Some(thread_id))
            .cloned()
    }

    fn on_domain_event(
        &mut self,
        kind: &EventKind,
        command_id: Option<CommandId>,
        cx: &mut Context<Self>,
    ) {
        match kind {
            EventKind::ProjectCreated { project } => {
                self.projects.push(project.clone());
                if command_id.is_some() && command_id == self.pending_project {
                    self.pending_project = None;
                    self.adding_project = false;
                    self.notice = None;
                    self.project_input.update(cx, |i, cx| i.set_text("", cx));
                    // A new project starts with a thread, like t3code.
                    self.new_thread(project.id, cx);
                }
                cx.notify();
            }
            EventKind::ThreadCreated { thread } => {
                self.threads.insert(0, thread.clone());
                if self.pending_select == Some(thread.id) {
                    self.pending_select = None;
                    self.select(thread.id, cx);
                }
                cx.notify();
            }
            EventKind::ThreadRenamed { thread_id, title } => {
                if let Some(t) = self.thread_mut(*thread_id) {
                    t.title = title.clone();
                    cx.notify();
                }
            }
            EventKind::ThreadArchived { thread_id } => {
                self.threads.retain(|t| t.id != *thread_id);
                if self.selected == Some(*thread_id) {
                    self.selected = None;
                    self.timeline = None;
                }
                cx.notify();
            }
            EventKind::RunCreated { run } => {
                self.set_thread_status(run.thread_id, run.status.thread_status(), cx);
            }
            EventKind::RunStatusChanged {
                thread_id, status, ..
            } => {
                self.set_thread_status(*thread_id, status.thread_status(), cx);
            }
            EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } => {
                if let Some(timeline) = self.timeline_for(item.thread_id) {
                    timeline.update(cx, |t, cx| t.apply(kind, cx));
                }
            }
            EventKind::ItemFinished { thread_id, .. } => {
                if let Some(timeline) = self.timeline_for(*thread_id) {
                    timeline.update(cx, |t, cx| t.apply(kind, cx));
                }
            }
            EventKind::ThreadProviderBound { .. } | EventKind::ItemTextAppended { .. } => {}
        }
    }

    fn set_thread_status(
        &mut self,
        thread_id: ThreadId,
        status: ThreadStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(t) = self.thread_mut(thread_id)
            && t.status != status
        {
            t.status = status;
            if let Some(timeline) = self.timeline_for(thread_id) {
                timeline.update(cx, |t, cx| t.set_status(status, cx));
            }
            cx.notify();
        }
    }

    // --------------------------------------------------------------- actions

    fn select(&mut self, thread_id: ThreadId, cx: &mut Context<Self>) {
        if self.selected == Some(thread_id) {
            return;
        }
        self.selected = Some(thread_id);
        self.pending_focus = Some(self.composer.focus_handle(cx));
        // Only the open thread's timeline is kept in memory.
        self.timeline = None;
        self.notice = None;
        self.core.open_thread(thread_id);
        cx.notify();
    }

    fn new_thread(&mut self, project_id: ProjectId, cx: &mut Context<Self>) {
        let thread_id = ThreadId::new();
        self.pending_select = Some(thread_id);
        self.core
            .dispatch(CommandEnvelope::new(Command::ThreadCreate {
                thread_id,
                project_id,
                title: String::new(),
            }));
        cx.notify();
    }

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let path = self.project_input.read(cx).text().trim().to_owned();
        if path.is_empty() {
            return;
        }
        let envelope = CommandEnvelope::new(Command::ProjectCreate {
            project_id: ProjectId::new(),
            name: String::new(),
            path,
        });
        self.pending_project = Some(envelope.command_id);
        self.core.dispatch(envelope);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.selected else {
            return;
        };
        if matches!(
            self.selected_status(),
            ThreadStatus::Running | ThreadStatus::Waiting
        ) {
            return;
        }
        let text = self.composer.read(cx).text().trim_end().to_owned();
        if text.trim().is_empty() {
            return;
        }
        self.core
            .dispatch(CommandEnvelope::new(Command::MessageDispatch {
                thread_id,
                message_id: ItemId::new(),
                run_id: RunId::new(),
                text,
            }));
        self.notice = None;
        self.composer.update(cx, |c, cx| c.set_text("", cx));
        window.focus(&self.composer.focus_handle(cx), cx);
    }

    fn stop(&mut self, _cx: &mut Context<Self>) {
        if let Some(thread_id) = self.selected {
            self.core
                .dispatch(CommandEnvelope::new(Command::RunInterrupt { thread_id }));
        }
    }

    fn archive(&mut self, thread_id: ThreadId) {
        self.core
            .dispatch(CommandEnvelope::new(Command::ThreadArchive { thread_id }));
    }

    fn start_auto_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(auto) = self.auto_prompt.clone() else {
            return;
        };
        if self.projects.is_empty() {
            let project_id = ProjectId::new();
            self.core
                .dispatch(CommandEnvelope::new(Command::ProjectCreate {
                    project_id,
                    name: String::new(),
                    path: auto.project_dir.to_string_lossy().into_owned(),
                }));
            self.new_thread(project_id, cx);
        } else if self.threads.is_empty() {
            self.new_thread(self.projects[0].id, cx);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(auto.delay).await;
            this.update(cx, |this, cx| {
                let thread_id = this.selected.or(this.threads.first().map(|t| t.id));
                if let Some(thread_id) = thread_id {
                    this.core
                        .dispatch(CommandEnvelope::new(Command::MessageDispatch {
                            thread_id,
                            message_id: ItemId::new(),
                            run_id: RunId::new(),
                            text: auto.prompt.clone(),
                        }));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------------- rendering

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.adding_project = !this.adding_project;
                        this.notice = None;
                        if this.adding_project {
                            window.focus(&this.project_input.focus_handle(cx), cx);
                        }
                        cx.notify();
                    })),
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
                .when_some(
                    self.notice
                        .clone()
                        .filter(|_| self.pending_project.is_none() && self.adding_project),
                    |d, notice| d.child(div().text_xs().text_color(theme::danger()).child(notice)),
                )
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
                    .group("project")
                    .mt_2()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded_md()
                    .child(
                        div().flex().flex_col().overflow_hidden().child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_muted())
                                .child(SharedString::from(project.name.clone())),
                        ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("new-{project_id}")))
                            .px_1()
                            .rounded_sm()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .hover(|d| d.bg(theme::surface_hover()))
                            .cursor_pointer()
                            .child("+ New")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.new_thread(project_id, cx)),
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
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select(thread_id, cx);
                            window.focus(&this.composer.focus_handle(cx), cx);
                        }))
                        .child(
                            div()
                                .size(px(7.))
                                .flex_shrink_0()
                                .rounded_full()
                                .when_some(dot, |d, c| d.bg(c)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(SharedString::from(thread.title.clone())),
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
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.archive(thread_id);
                                })),
                        ),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .bg(theme::sidebar())
            .border_r_1()
            .border_color(theme::border())
            .child(header)
            .children(add_form)
            .child(list)
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(thread) = self.selected.and_then(|id| self.thread(id)).cloned() else {
            let hint = if self.projects.is_empty() {
                "Add a project folder to get started (+ Project, top left)."
            } else {
                "Select a thread, or start one with “+ New” next to a project."
            };
            return div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme::text_muted())
                .child(hint)
                .into_any_element();
        };
        let project = self
            .projects
            .iter()
            .find(|p| p.id == thread.project_id)
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let busy = matches!(thread.status, ThreadStatus::Running | ThreadStatus::Waiting);

        let header = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(thread.title.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child(SharedString::from(project)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_faint())
                    .child("Codex"),
            );

        let timeline = match &self.timeline {
            Some(t) => div().flex_1().min_h_0().child(t.clone()),
            None => div().flex_1(),
        };

        let action = if busy {
            button(
                "stop".into(),
                "Stop",
                theme::danger_bg(),
                theme::danger(),
                cx.listener(|this, _, _, cx| this.stop(cx)),
            )
        } else {
            button(
                "send".into(),
                "Send",
                theme::accent_bg(),
                theme::text(),
                cx.listener(|this, _, window, cx| this.send(window, cx)),
            )
        };

        let composer = div().flex().justify_center().px_6().pb_4().child(
            div()
                .w_full()
                .max_w(px(820.))
                .flex()
                .flex_col()
                .gap_2()
                .when_some(
                    self.notice.clone().filter(|_| !self.adding_project),
                    |d, notice| d.child(div().text_xs().text_color(theme::danger()).child(notice)),
                )
                .child(
                    div()
                        .p_3()
                        .rounded_lg()
                        .bg(theme::surface())
                        .border_1()
                        .border_color(theme::border())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().text_sm().child(self.composer.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_xs().text_color(theme::text_faint()).child(
                                    match thread.status {
                                        ThreadStatus::Running => "Codex is working…",
                                        ThreadStatus::Waiting => {
                                            "Codex is waiting for your approval"
                                        }
                                        _ => "Codex · on-request approvals",
                                    },
                                ))
                                .child(action),
                        ),
                ),
        );

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .min_w_0()
            .child(header)
            .child(timeline)
            .child(composer)
            .into_any_element()
    }
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(handle) = self.pending_focus.take() {
            window.focus(&handle, cx);
        }
        div()
            .flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
    }
}
