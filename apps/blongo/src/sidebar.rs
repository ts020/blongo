//! Sidebar: environments → projects → threads, the add-project and
//! add-environment forms and the t3code import.
//!
//! Its own entity, rendered as a cached view: streaming text notifies only
//! the timeline, so the sidebar's element tree is reused frame after frame
//! instead of being rebuilt with every delta. It is notified only when a
//! project, thread or connection actually changes.
//!
//! With only the local core there are no environment headers (the window
//! looks as before); each remote environment adds a header with its
//! connection state.

use blongo_protocol::client::ConnectionState;
use blongo_protocol::{Project, ProjectId, Thread, ThreadId, ThreadStatus};
use gpui::{
    Context, Entity, EventEmitter, Focusable, FontWeight, Hsla, SharedString, Subscription, Window,
    div, prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::theme;

/// Index into the shell's environments; 0 is the local core.
pub type EnvId = usize;
pub const LOCAL: EnvId = 0;

pub enum SidebarEvent {
    Select(EnvId, ThreadId),
    NewThread {
        env: EnvId,
        project_id: ProjectId,
        worktree: bool,
    },
    Archive(EnvId, ThreadId),
    AddProject(EnvId, String),
    /// "NAME TARGET [CODE]" from the add-environment form.
    AddEnvironment(String),
    /// A form was dismissed.
    Dismissed,
    ImportT3,
    OpenInbox,
    OpenSettings,
}

pub struct EnvView {
    pub name: SharedString,
    /// `None`: the local core (always there).
    pub status: Option<ConnectionState>,
    pub projects: Vec<Project>,
    /// Newest first.
    pub threads: Vec<Thread>,
}

impl EnvView {
    pub fn new(name: impl Into<SharedString>, remote: bool) -> Self {
        Self {
            name: name.into(),
            status: remote.then_some(ConnectionState::Connecting),
            projects: Vec::new(),
            threads: Vec::new(),
        }
    }

    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    pub fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn set_threads(&mut self, mut threads: Vec<Thread>) {
        threads.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        self.threads = threads;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Project(EnvId),
    Environment,
}

pub struct Sidebar {
    pub envs: Vec<EnvView>,
    pub selected: Option<(EnvId, ThreadId)>,
    form: Option<Form>,
    pub project_input: Entity<TextInput>,
    env_input: Entity<TextInput>,
    /// Shown under the open form (a rejected path, a failed pairing).
    pub form_notice: Option<SharedString>,
    /// Shown at the bottom (import progress / result).
    pub footer_notice: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project_input = cx.new(|cx| TextInput::new("/path/to/project", false, cx));
        let env_input = cx.new(|cx| TextInput::new("devbox ws://100.64.0.2:7878 CODE", false, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &project_input,
                window,
                |this, input, event, _, cx| match event {
                    InputEvent::Submit => {
                        let path = input.read(cx).text().trim().to_owned();
                        if let (false, Some(Form::Project(env))) = (path.is_empty(), this.form) {
                            cx.emit(SidebarEvent::AddProject(env, path));
                        }
                    }
                    InputEvent::Cancel => this.close_form(cx),
                    InputEvent::SubmitAlt => {}
                },
            ),
            cx.subscribe_in(
                &env_input,
                window,
                |this, input, event, _, cx| match event {
                    InputEvent::Submit => {
                        let text = input.read(cx).text().trim().to_owned();
                        if !text.is_empty() {
                            cx.emit(SidebarEvent::AddEnvironment(text));
                        }
                    }
                    InputEvent::Cancel => this.close_form(cx),
                    InputEvent::SubmitAlt => {}
                },
            ),
        ];
        Self {
            envs: vec![EnvView::new("Local", false)],
            selected: None,
            form: None,
            project_input,
            env_input,
            form_notice: None,
            footer_notice: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn thread(&self, env: EnvId, id: ThreadId) -> Option<&Thread> {
        self.envs.get(env)?.thread(id)
    }

    /// Change one thread in place; notifies only when `f` reports a change.
    pub fn update_thread(
        &mut self,
        env: EnvId,
        id: ThreadId,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Thread) -> bool,
    ) -> bool {
        let changed = self
            .envs
            .get_mut(env)
            .and_then(|e| e.threads.iter_mut().find(|t| t.id == id))
            .is_some_and(f);
        if changed {
            cx.notify();
        }
        changed
    }

    pub fn adding_project(&self) -> bool {
        matches!(self.form, Some(Form::Project(_)))
    }

    /// Open the add-project form for `env` (focus is the caller's).
    pub fn open_project_form(&mut self, env: EnvId) {
        self.form = Some(Form::Project(env));
        self.form_notice = None;
    }

    pub fn close_form(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        self.form_notice = None;
        cx.emit(SidebarEvent::Dismissed);
        cx.notify();
    }

    /// The add-environment form succeeded.
    pub fn environment_added(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        self.form_notice = None;
        self.footer_notice = None;
        self.env_input.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    fn toggle_form(&mut self, form: Form, window: &mut Window, cx: &mut Context<Self>) {
        if self.form == Some(form) {
            self.close_form(cx);
            return;
        }
        self.form = Some(form);
        self.form_notice = None;
        let input = match form {
            Form::Project(_) => &self.project_input,
            Form::Environment => &self.env_input,
        };
        window.focus(&input.focus_handle(cx), cx);
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

/// Dot color and label of a remote environment's link.
fn connection_badge(state: &ConnectionState) -> (Hsla, SharedString) {
    match state {
        ConnectionState::Connecting => (theme::warning().into(), "connecting…".into()),
        ConnectionState::Connected { .. } => (theme::success().into(), "connected".into()),
        ConnectionState::Reconnecting {
            retry_in_ms,
            attempt,
            ..
        } => (
            theme::warning().into(),
            format!(
                "offline, retrying in {}s (attempt {attempt})",
                retry_in_ms.div_ceil(1000)
            )
            .into(),
        ),
        ConnectionState::Failed(_) => (theme::danger().into(), "failed".into()),
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let multi = self.envs.len() > 1;
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
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        small_action("open-inbox".into(), "Inbox")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenInbox))),
                    )
                    .child(
                        small_action("open-settings".into(), "Settings").on_click(
                            cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenSettings)),
                        ),
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
                                this.toggle_form(Form::Project(LOCAL), window, cx)
                            })),
                    ),
            );

        let form = self.form.map(|form| {
            let (hint, input): (SharedString, _) = match form {
                Form::Project(env) => (
                    if multi {
                        format!(
                            "Add a project folder on {} (Enter to add, Esc to cancel)",
                            self.envs[env].name
                        )
                        .into()
                    } else {
                        "Add a project folder (Enter to add, Esc to cancel)".into()
                    },
                    self.project_input.clone(),
                ),
                Form::Environment => (
                    "Add an environment: NAME TARGET [PAIRING CODE]. Targets: ws://HOST:PORT, \
                     ssh://HOST?port=N, ssh+stdio://HOST"
                        .into(),
                    self.env_input.clone(),
                ),
            };
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
                .child(div().text_xs().text_color(theme::text_muted()).child(hint))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme::code_bg())
                        .text_sm()
                        .child(input),
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
        for (env_id, env) in self.envs.iter().enumerate() {
            if multi {
                let (dot, label): (Hsla, SharedString) = match &env.status {
                    None => (theme::success().into(), "this machine".into()),
                    Some(state) => connection_badge(state),
                };
                let error = match &env.status {
                    Some(ConnectionState::Failed(e))
                    | Some(ConnectionState::Reconnecting { error: e, .. }) => Some(e.clone()),
                    _ => None,
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!("env-{env_id}")))
                        .mt_3()
                        .px_2()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(dot))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::text())
                                        .child(env.name.clone()),
                                )
                                .child(div().flex_1())
                                .when(env.status.is_some(), |d| {
                                    d.child(
                                        small_action(
                                            format!("env-project-{env_id}").into(),
                                            "+ Project",
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.toggle_form(Form::Project(env_id), window, cx)
                                            }),
                                        ),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .pl_4()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(theme::text_faint())
                                .child(label),
                        )
                        .when_some(error, |d, e| {
                            d.child(
                                div()
                                    .pl_4()
                                    .text_xs()
                                    .text_color(theme::text_faint())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(SharedString::from(e)),
                            )
                        }),
                );
            }
            for project in &env.projects {
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
                                        env: env_id,
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
                                        env: env_id,
                                        project_id,
                                        worktree: false,
                                    })
                                }),
                            ),
                        ),
                );
                for thread in env.threads.iter().filter(|t| t.project_id == project_id) {
                    let thread_id = thread.id;
                    let selected = self.selected == Some((env_id, thread_id));
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
                                cx.emit(SidebarEvent::Select(env_id, thread_id));
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
                                        cx.emit(SidebarEvent::Archive(env_id, thread_id));
                                    })),
                            ),
                    );
                }
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
                div()
                    .flex()
                    .gap_2()
                    .child(
                        small_action("import-t3".into(), "Import from t3code…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ImportT3))),
                    )
                    .child(
                        small_action("add-env".into(), "+ Environment").on_click(cx.listener(
                            |this, _, window, cx| this.toggle_form(Form::Environment, window, cx),
                        )),
                    ),
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
            .children(form)
            .child(list)
            .child(footer)
    }
}

fn provider_initial(thread: &Thread) -> &'static str {
    match thread.provider {
        blongo_protocol::ProviderKind::Codex => "Cx",
        blongo_protocol::ProviderKind::ClaudeCode => "Cl",
        blongo_protocol::ProviderKind::Antigravity => "Ag",
        blongo_protocol::ProviderKind::Acp => "Ac",
    }
}
