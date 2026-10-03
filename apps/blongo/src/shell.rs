//! The window: sidebar (environments → projects → threads), the open
//! thread's timeline, the composer and the terminal panel.
//!
//! Every environment is a [`Backend`]: environment 0 is the in-process core
//! (typed values over channels, nothing serialized); the others are
//! `blongo serve` instances reached over the wire. Each one's events are
//! pumped separately and tagged with its [`EnvId`]; past that point the
//! shell treats them alike.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use blongo_client::environments::{self, Environment, EnvironmentFile};
use blongo_client::{Backend, Events};
use blongo_core::{CoreEvent, InstallState, LoginState};
use blongo_protocol::client::{ConnectionState, TerminalEvent};
use blongo_protocol::{
    Command, CommandEnvelope, CommandId, Delivery, EventKind, ItemId, ItemKind, ModelInfo,
    ProjectId, ProviderKind, RunId, RunStatus, Thread, ThreadId, ThreadSnapshot, ThreadStatus,
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, SharedString, StyleRefinement,
    Subscription, Window, actions, div, prelude::*, px,
};

use crate::input::{InputEvent, TextInput};
use crate::sidebar::{EnvId, EnvView, LOCAL, Sidebar, SidebarEvent};
use crate::terminal::{self, GridSize, TerminalView};
use crate::theme;
use crate::timeline::{Timeline, TimelineEvent, button};

actions!(
    shell,
    [
        NewThread,
        ForkThread,
        UndoLastTurn,
        ToggleTerminal,
        UseCodex,
        UseClaudeCode,
        UseAntigravity,
        NextModel,
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    let c = Some("Shell");
    cx.bind_keys([
        KeyBinding::new("secondary-n", NewThread, c),
        KeyBinding::new("alt-f", ForkThread, c),
        KeyBinding::new("alt-z", UndoLastTurn, c),
        KeyBinding::new("secondary-`", ToggleTerminal, c),
        KeyBinding::new("alt-1", UseCodex, c),
        KeyBinding::new("alt-2", UseClaudeCode, c),
        KeyBinding::new("alt-3", UseAntigravity, c),
        KeyBinding::new("alt-m", NextModel, c),
    ]);
}

const SIDEBAR_WIDTH: f32 = 260.;
const TERMINAL_HEIGHT: f32 = 240.;

/// Profiling hook: once the shell is up, make sure a project + thread exist,
/// then send `prompt` after `delay` (see tools/profile.py).
#[derive(Clone)]
pub struct AutoPrompt {
    pub prompt: String,
    pub delay: Duration,
    pub project_dir: PathBuf,
    /// Also open the terminal panel (memory with a terminal open).
    pub terminal: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Provider,
    Model,
}

/// One environment's link and what it told us that the sidebar does not
/// show.
struct Env {
    backend: Arc<dyn Backend>,
    /// Models each provider offered in its last handshake.
    models: HashMap<ProviderKind, Arc<[ModelInfo]>>,
    /// The first shell snapshot arrived.
    loaded: bool,
}

pub struct Shell {
    envs: Vec<Env>,
    sidebar: Entity<Sidebar>,
    timeline: Option<Entity<Timeline>>,
    /// Turn actions of the open timeline (replaced with it).
    timeline_events: Option<Subscription>,
    composer: Entity<TextInput>,
    pending_project: Option<(EnvId, CommandId)>,
    /// A sent message the backend has not accepted yet: its text comes
    /// back into the composer if the command is rejected.
    pending_message: Option<(EnvId, CommandId, String)>,
    /// Select this thread when its creation event arrives.
    pending_select: Option<(EnvId, ThreadId)>,
    /// An environment being paired (its name).
    pairing: Option<String>,
    /// Queued messages of the open thread, oldest first.
    queued: Vec<(RunId, SharedString)>,
    /// The open thread's run id → status (for the last-turn undo).
    runs: Vec<(RunId, RunStatus)>,
    /// Provider for new threads: the last one picked.
    default_provider: ProviderKind,
    picker: Option<Picker>,
    /// A rollback waiting for the user's confirmation.
    confirm_rollback: Option<RollbackConfirm>,
    /// Last rejected command's reason.
    notice: Option<SharedString>,
    /// Sign-in / install progress.
    provider_notice: Option<SharedString>,
    /// The local core stopped; shown instead of the main area.
    fatal: Option<SharedString>,
    terminal: Option<Entity<TerminalView>>,
    /// Id of the next server-side terminal.
    next_terminal: u32,
    auto_prompt: Option<AutoPrompt>,
    /// Focus to apply on the next render (set where no window is at hand).
    pending_focus: Option<FocusHandle>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    /// `local`: the in-process core and its events. Remote environments
    /// come from the saved environments file.
    pub fn new(
        local: Arc<dyn Backend>,
        events: Events,
        auto_prompt: Option<AutoPrompt>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextInput::new(
                "Ask anything…  (Enter to send or queue, Ctrl+Enter to steer, Shift+Enter for a new line)",
                true,
                cx,
            )
        });
        let sidebar = cx.new(|cx| Sidebar::new(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Submit => this.send(Delivery::Queue, window, cx),
                    InputEvent::SubmitAlt => this.send(Delivery::Steer, window, cx),
                    InputEvent::Cancel => {
                        this.picker = None;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(&sidebar, window, Self::on_sidebar_event),
        ];
        Self::pump(LOCAL, events, cx);
        let mut this = Self {
            envs: vec![Env {
                backend: local,
                models: HashMap::new(),
                loaded: false,
            }],
            sidebar,
            timeline: None,
            timeline_events: None,
            composer,
            pending_project: None,
            pending_message: None,
            pending_select: None,
            pairing: None,
            queued: Vec::new(),
            runs: Vec::new(),
            default_provider: ProviderKind::Codex,
            picker: None,
            confirm_rollback: None,
            notice: None,
            provider_notice: None,
            fatal: None,
            terminal: None,
            next_terminal: 1,
            auto_prompt,
            pending_focus: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        // Saved remote environments. Without any, no network thread is
        // ever started.
        match EnvironmentFile::load(&environments::default_path()) {
            Ok(file) => {
                for env in file.environments {
                    this.add_remote(env, cx);
                }
            }
            Err(err) => {
                this.sidebar.update(cx, |s, _| {
                    s.footer_notice = Some(format!("Environments: {err}").into());
                });
            }
        }
        this
    }

    /// Connect to a remote environment and show it in the sidebar.
    fn add_remote(&mut self, env: Environment, cx: &mut Context<Self>) -> EnvId {
        let name = env.name.clone();
        let (backend, events) =
            blongo_client::remote::connect(env, blongo_client::RemoteOptions::default());
        let id = self.envs.len();
        self.envs.push(Env {
            backend: Arc::new(backend),
            models: HashMap::new(),
            loaded: false,
        });
        self.sidebar.update(cx, |s, cx| {
            s.envs.push(EnvView::new(name, true));
            cx.notify();
        });
        Self::pump(id, events, cx);
        id
    }

    /// Receive one environment's events on the UI thread. Each wake drains
    /// everything queued, so a burst of deltas costs one entity update.
    fn pump(env: EnvId, mut events: Events, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Some(first) = events.recv().await {
                batch.push(first);
                while let Ok(next) = events.try_recv() {
                    batch.push(next);
                }
                let alive = this.update(cx, |this, cx| {
                    for event in batch.drain(..) {
                        this.on_core_event(env, event, cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn backend(&self, env: EnvId) -> &Arc<dyn Backend> {
        &self.envs[env].backend
    }

    fn selected(&self, cx: &App) -> Option<(EnvId, ThreadId)> {
        self.sidebar.read(cx).selected
    }

    fn is_selected(&self, env: EnvId, thread_id: ThreadId, cx: &App) -> bool {
        self.selected(cx) == Some((env, thread_id))
    }

    /// The environment of the open thread (the local core when none).
    fn selected_env(&self, cx: &App) -> EnvId {
        self.selected(cx).map_or(LOCAL, |(env, _)| env)
    }

    fn selected_thread(&self, cx: &App) -> Option<Thread> {
        let sidebar = self.sidebar.read(cx);
        sidebar
            .selected
            .and_then(|(env, id)| sidebar.thread(env, id))
            .cloned()
    }

    fn selected_status(&self, cx: &App) -> ThreadStatus {
        self.selected_thread(cx)
            .map_or(ThreadStatus::Idle, |t| t.status)
    }

    fn on_sidebar_event(
        &mut self,
        _: &Entity<Sidebar>,
        event: &SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SidebarEvent::Select(env, id) => {
                self.select(*env, *id, cx);
                window.focus(&self.composer.focus_handle(cx), cx);
            }
            SidebarEvent::NewThread {
                env,
                project_id,
                worktree,
            } => self.new_thread(*env, *project_id, *worktree, cx),
            SidebarEvent::Archive(env, id) => {
                self.dispatch(*env, Command::ThreadArchive { thread_id: *id })
            }
            SidebarEvent::AddProject(env, path) => {
                let envelope = CommandEnvelope::new(Command::ProjectCreate {
                    project_id: ProjectId::new(),
                    name: String::new(),
                    path: path.clone(),
                });
                self.pending_project = Some((*env, envelope.command_id));
                self.backend(*env).dispatch(envelope);
            }
            SidebarEvent::AddEnvironment(text) => self.add_environment(text, cx),
            SidebarEvent::Dismissed => {
                window.focus(&self.composer.focus_handle(cx), cx);
            }
            SidebarEvent::ImportT3 => {
                let source = std::env::var_os("BLONGO_T3_DB")
                    .map(PathBuf::from)
                    .or_else(blongo_core::t3_import::default_source);
                let message = match source {
                    Some(source) => {
                        let text = format!("Importing {}…", source.display());
                        self.backend(LOCAL).import_t3(Some(source));
                        text
                    }
                    None => "No home directory: cannot find t3code's database".into(),
                };
                self.sidebar.update(cx, |s, cx| {
                    s.footer_notice = Some(message.into());
                    cx.notify();
                });
            }
        }
    }

    /// "NAME TARGET [CODE]": pair (on the network thread), save the
    /// credential, connect.
    fn add_environment(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut words = text.split_whitespace();
        let (Some(name), Some(target)) = (words.next(), words.next()) else {
            self.form_notice(
                "Type a name and a target, e.g. devbox ws://100.64.0.2:7878 CODE",
                cx,
            );
            return;
        };
        let code = words.next().map(str::to_owned);
        if let Err(err) = blongo_client::target::Target::parse(target) {
            self.form_notice(&err, cx);
            return;
        }
        let taken = self
            .sidebar
            .read(cx)
            .envs
            .iter()
            .any(|e| e.name.as_ref() == name);
        if taken || name == "Local" {
            self.form_notice(&format!("An environment named {name} already exists"), cx);
            return;
        }
        if self.pairing.is_some() {
            return;
        }
        self.pairing = Some(name.to_owned());
        self.form_notice_muted(&format!("Connecting to {target}…"), cx);
        let (name, target) = (name.to_owned(), target.to_owned());
        let task = blongo_client::net::handle().spawn(async move {
            let device = blongo_client::pairing::device_name();
            let env =
                blongo_client::pairing::pair(&name, &target, code.as_deref(), &device).await?;
            let path = environments::default_path();
            EnvironmentFile::update(&path, |file| file.upsert(env.clone()))?;
            Ok::<_, String>(env)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                this.pairing = None;
                match result {
                    Ok(env) => {
                        this.add_remote(env, cx);
                        this.sidebar.update(cx, |s, cx| s.environment_added(cx));
                        this.pending_focus = Some(this.composer.focus_handle(cx));
                    }
                    Err(err) => this.form_notice(&err, cx),
                }
            })
            .ok();
        })
        .detach();
    }

    fn form_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        let text: SharedString = text.to_owned().into();
        self.sidebar.update(cx, |s, cx| {
            s.form_notice = Some(text);
            cx.notify();
        });
    }

    fn form_notice_muted(&mut self, text: &str, cx: &mut Context<Self>) {
        let text: SharedString = text.to_owned().into();
        self.sidebar.update(cx, |s, cx| {
            s.footer_notice = Some(text);
            cx.notify();
        });
    }

    fn on_core_event(&mut self, env: EnvId, event: CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::Reply { .. } => {}
            CoreEvent::Shell(shell) => {
                let first = !self.envs[env].loaded;
                self.envs[env].loaded = true;
                let no_projects = shell.projects.is_empty();
                let local_empty = env == LOCAL && no_projects && self.auto_prompt.is_none();
                let selected = self.selected(cx);
                let mut lost_selection = false;
                self.sidebar.update(cx, |s, cx| {
                    s.envs[env].projects = shell.projects.clone();
                    s.envs[env].set_threads(shell.threads.clone());
                    if let Some((e, id)) = selected
                        && e == env
                        && s.envs[env].thread(id).is_none()
                    {
                        s.selected = None;
                        lost_selection = true;
                    }
                    if first && local_empty && s.envs.len() == 1 {
                        s.open_project_form(LOCAL);
                    }
                    cx.notify();
                });
                if lost_selection {
                    self.timeline = None;
                    self.terminal = None;
                    self.confirm_rollback = None;
                }
                if first && env == LOCAL {
                    // Restore: reopen the most recently active thread.
                    if let Some(id) = shell.threads.first().map(|t| t.id) {
                        self.select(LOCAL, id, cx);
                    }
                    if local_empty && self.sidebar.read(cx).adding_project() {
                        self.pending_focus =
                            Some(self.sidebar.read(cx).project_input.focus_handle(cx));
                    }
                    self.start_auto_prompt(cx);
                }
                cx.notify();
            }
            CoreEvent::Thread(snapshot) => {
                if self.is_selected(env, snapshot.thread_id, cx) {
                    self.load_thread(env, &snapshot, cx);
                }
            }
            CoreEvent::TextDelta {
                thread_id,
                item_id,
                chunk,
            } => {
                if let Some(timeline) = self.timeline_for(env, thread_id, cx) {
                    timeline.update(cx, |t, cx| t.append(item_id, &chunk, cx));
                }
            }
            CoreEvent::Event(event) => {
                if event.command_id.is_some()
                    && self
                        .pending_message
                        .as_ref()
                        .map(|(e, id, _)| (*e, Some(*id)))
                        == Some((env, event.command_id))
                {
                    self.pending_message = None;
                }
                self.on_domain_event(env, &event.kind, event.command_id, cx)
            }
            CoreEvent::CommandRejected { command_id, reason } => {
                if self.pending_project == Some((env, command_id)) {
                    self.pending_project = None;
                    self.form_notice(&reason, cx);
                }
                if let Some((_, _, text)) = self
                    .pending_message
                    .take_if(|(e, id, _)| *e == env && *id == command_id)
                    && self.composer.read(cx).text().is_empty()
                {
                    self.composer.update(cx, |c, cx| c.set_text(&text, cx));
                }
                self.notice = Some(reason.into());
                cx.notify();
            }
            CoreEvent::CommandDuplicate { .. } => {}
            CoreEvent::Connection(state) => {
                if !state.is_connected() {
                    // Server-side terminals end with the connection.
                    if self.selected_env(cx) == env
                        && let Some(term) = &self.terminal
                        && term.read(cx).remote_id().is_some()
                    {
                        term.update(cx, |t, cx| t.set_exited(None, cx));
                    }
                }
                if let ConnectionState::Failed(reason) = &state {
                    eprintln!("blongo: environment {env}: {reason}");
                }
                self.sidebar.update(cx, |s, cx| {
                    if let Some(e) = s.envs.get_mut(env) {
                        e.status = Some(state);
                        cx.notify();
                    }
                });
            }
            CoreEvent::Terminal(event) => {
                let Some(term) = self.terminal.clone() else {
                    return;
                };
                if self.selected_env(cx) != env {
                    return;
                }
                let id = term.read(cx).remote_id();
                match event {
                    TerminalEvent::Output { id: t, data } if Some(t) == id => {
                        term.update(cx, |term, cx| term.feed(&data, cx));
                    }
                    TerminalEvent::Exited { id: t } if Some(t) == id => {
                        term.update(cx, |term, cx| term.set_exited(None, cx));
                    }
                    TerminalEvent::Failed { id: t, message } if Some(t) == id => {
                        term.update(cx, |term, cx| term.set_exited(Some(message), cx));
                    }
                    _ => {}
                }
            }
            CoreEvent::Failed { message } => {
                if env == LOCAL {
                    self.fatal = Some(message.into());
                } else {
                    self.notice = Some(message.into());
                }
                cx.notify();
            }
            CoreEvent::RunFinished { .. } => {
                if self.auto_prompt.is_some() {
                    // tools/profile.py waits for this line.
                    eprintln!("blongo: replay done");
                }
            }
            CoreEvent::Models { provider, models } => {
                self.envs[env].models.insert(provider, models);
                cx.notify();
            }
            CoreEvent::Login { provider, state } => {
                self.provider_notice = Some(match state {
                    LoginState::Url(url) => {
                        cx.open_url(&url);
                        format!("{provider}: finish signing in in your browser — {url}").into()
                    }
                    LoginState::Succeeded => format!("{provider}: signed in").into(),
                    LoginState::Failed(err) => format!("{provider}: sign-in failed: {err}").into(),
                });
                cx.notify();
            }
            CoreEvent::Install(state) => {
                self.provider_notice = Some(match state {
                    InstallState::Progress(p) => format!("Antigravity: {p}").into(),
                    InstallState::Done(path) => {
                        format!("Antigravity installed: {}", path.display()).into()
                    }
                    InstallState::Failed(err) => {
                        format!("Antigravity install failed: {err}").into()
                    }
                });
                cx.notify();
            }
            CoreEvent::Notice { message } => {
                self.notice = Some(if env == LOCAL {
                    message.into()
                } else {
                    format!("{}: {message}", self.sidebar.read(cx).envs[env].name).into()
                });
                cx.notify();
            }
            CoreEvent::Imported(result) => {
                let message: SharedString = match result {
                    Ok(r) => {
                        let mut text = format!(
                            "Imported {} projects, {} threads ({} updated, {} already there)",
                            r.projects,
                            r.threads,
                            r.updated_threads.len(),
                            r.skipped_threads
                        );
                        if r.bad_rows > 0 {
                            text.push_str(&format!("; skipped {} unreadable rows", r.bad_rows));
                        }
                        text.into()
                    }
                    Err(err) => format!("Import failed: {err}").into(),
                };
                self.sidebar.update(cx, |s, cx| {
                    s.footer_notice = Some(message);
                    cx.notify();
                });
            }
        }
    }

    fn load_thread(&mut self, env: EnvId, snapshot: &ThreadSnapshot, cx: &mut Context<Self>) {
        let status = self.selected_status(cx);
        let backend = self.backend(env).clone();
        let timeline = cx.new(|_| Timeline::new(snapshot, status, backend));
        let sub = cx.subscribe(
            &timeline,
            |this, _, event: &TimelineEvent, cx| match event {
                TimelineEvent::Fork(run_id) => this.fork(Some(*run_id), cx),
                TimelineEvent::Rollback(run_id) => this.rollback(*run_id, cx),
            },
        );
        self.timeline_events = Some(sub);
        self.timeline = Some(timeline);
        self.confirm_rollback = None;
        self.runs = snapshot.runs.iter().map(|r| (r.id, r.status)).collect();
        self.queued = snapshot
            .runs
            .iter()
            .filter(|r| r.status == RunStatus::Queued)
            .map(|r| {
                let text = snapshot
                    .items
                    .iter()
                    .find(|i| i.run_id == Some(r.id) && i.kind == ItemKind::UserMessage)
                    .map(|i| SharedString::from(i.text.to_string()))
                    .unwrap_or_default();
                (r.id, text)
            })
            .collect();
        cx.notify();
    }

    fn timeline_for(&self, env: EnvId, thread_id: ThreadId, cx: &App) -> Option<Entity<Timeline>> {
        self.timeline
            .as_ref()
            .filter(|_| self.is_selected(env, thread_id, cx))
            .cloned()
    }

    fn on_domain_event(
        &mut self,
        env: EnvId,
        kind: &EventKind,
        command_id: Option<CommandId>,
        cx: &mut Context<Self>,
    ) {
        match kind {
            EventKind::RunUsage { .. }
            | EventKind::ScheduleCreated { .. }
            | EventKind::ScheduleUpdated { .. }
            | EventKind::ScheduleDeleted { .. } => {}
            EventKind::ProjectCreated { project } => {
                let ours = command_id.is_some()
                    && self.pending_project.map(|(e, id)| (e, Some(id))) == Some((env, command_id));
                let project = project.clone();
                let project_id = project.id;
                self.sidebar.update(cx, |s, cx| {
                    s.envs[env].projects.push(project);
                    if ours {
                        s.close_form(cx);
                        s.project_input.update(cx, |i, cx| i.set_text("", cx));
                    }
                    cx.notify();
                });
                if ours {
                    self.pending_project = None;
                    self.notice = None;
                    // A new project starts with a thread, like t3code.
                    self.new_thread(env, project_id, false, cx);
                }
            }
            EventKind::ThreadCreated { thread } => {
                let thread = thread.clone();
                let id = thread.id;
                self.sidebar.update(cx, |s, cx| {
                    let threads = &mut s.envs[env].threads;
                    if !threads.iter().any(|t| t.id == id) {
                        threads.insert(0, thread);
                    }
                    cx.notify();
                });
                if self.pending_select == Some((env, id)) {
                    self.pending_select = None;
                    self.select(env, id, cx);
                }
            }
            EventKind::ThreadRenamed { thread_id, title } => {
                let changed = self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| {
                        t.title = title.clone();
                        true
                    })
                });
                if changed && self.is_selected(env, *thread_id, cx) {
                    cx.notify();
                }
            }
            EventKind::ThreadArchived { thread_id } => {
                let selected = self.is_selected(env, *thread_id, cx);
                self.sidebar.update(cx, |s, cx| {
                    s.envs[env].threads.retain(|t| t.id != *thread_id);
                    if selected {
                        s.selected = None;
                    }
                    cx.notify();
                });
                if selected {
                    self.timeline = None;
                    self.confirm_rollback = None;
                    self.terminal = None;
                }
                cx.notify();
            }
            EventKind::ThreadProviderChanged {
                thread_id,
                provider,
                model,
                provider_thread_id,
                pending_context,
            } => {
                self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| {
                        t.provider = *provider;
                        t.model = model.clone();
                        t.provider_thread_id = provider_thread_id.clone();
                        t.pending_context = pending_context.clone();
                        true
                    })
                });
                cx.notify();
            }
            EventKind::ThreadProviderBound {
                thread_id,
                provider_thread_id,
            } => {
                self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| {
                        t.provider_thread_id = Some(provider_thread_id.clone());
                        t.pending_context = None;
                        false
                    })
                });
            }
            EventKind::RunCreated { run } => {
                if self.is_selected(env, run.thread_id, cx) {
                    self.runs.push((run.id, run.status));
                    if run.status == RunStatus::Queued {
                        self.queued.push((run.id, SharedString::default()));
                        cx.notify();
                    }
                    if let Some(timeline) = &self.timeline {
                        timeline.update(cx, |t, cx| t.set_run_status(run.id, run.status, cx));
                    }
                }
                if let Some(status) = run.status.thread_status() {
                    self.set_thread_status(env, run.thread_id, status, cx);
                }
            }
            EventKind::RunStatusChanged {
                thread_id,
                run_id,
                status,
                ..
            } => {
                if self.is_selected(env, *thread_id, cx) {
                    if let Some(r) = self.runs.iter_mut().find(|(id, _)| id == run_id) {
                        r.1 = *status;
                    }
                    if *status != RunStatus::Queued {
                        let before = self.queued.len();
                        self.queued.retain(|(id, _)| id != run_id);
                        if before != self.queued.len() {
                            cx.notify();
                        }
                    }
                    if let Some(timeline) = &self.timeline {
                        timeline.update(cx, |t, cx| t.set_run_status(*run_id, *status, cx));
                    }
                }
                if let Some(status) = status.thread_status() {
                    self.set_thread_status(env, *thread_id, status, cx);
                }
            }
            EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } => {
                if self.is_selected(env, item.thread_id, cx)
                    && let (ItemKind::UserMessage, Some(run_id)) = (&item.kind, item.run_id)
                    && let Some(q) = self.queued.iter_mut().find(|(id, _)| *id == run_id)
                    && q.1.is_empty()
                {
                    q.1 = SharedString::from(item.text.to_string());
                    cx.notify();
                }
                if let Some(timeline) = self.timeline_for(env, item.thread_id, cx) {
                    timeline.update(cx, |t, cx| t.apply(kind, cx));
                }
            }
            EventKind::ItemFinished { thread_id, .. } => {
                if let Some(timeline) = self.timeline_for(env, *thread_id, cx) {
                    timeline.update(cx, |t, cx| t.apply(kind, cx));
                }
            }
            EventKind::RunProviderTurn { .. }
            | EventKind::RunCheckpointed { .. }
            | EventKind::ItemTextAppended { .. } => {}
        }
    }

    fn set_thread_status(
        &mut self,
        env: EnvId,
        thread_id: ThreadId,
        status: ThreadStatus,
        cx: &mut Context<Self>,
    ) {
        let changed = self.sidebar.update(cx, |s, cx| {
            s.update_thread(env, thread_id, cx, |t| {
                let changed = t.status != status;
                t.status = status;
                changed
            })
        });
        if changed && self.is_selected(env, thread_id, cx) {
            if let Some(timeline) = &self.timeline {
                timeline.update(cx, |t, cx| t.set_status(status, cx));
            }
            cx.notify();
        }
    }

    // --------------------------------------------------------------- actions

    fn dispatch(&self, env: EnvId, command: Command) {
        self.backend(env).dispatch(CommandEnvelope::new(command));
    }

    /// Send a command about the open thread to its environment.
    fn dispatch_selected(&self, command: Command, cx: &App) {
        self.dispatch(self.selected_env(cx), command);
    }

    fn select(&mut self, env: EnvId, thread_id: ThreadId, cx: &mut Context<Self>) {
        if self.selected(cx) == Some((env, thread_id)) {
            return;
        }
        self.sidebar.update(cx, |s, cx| {
            s.selected = Some((env, thread_id));
            cx.notify();
        });
        self.pending_focus = Some(self.composer.focus_handle(cx));
        // Only the open thread's timeline is kept in memory.
        self.timeline = None;
        self.queued.clear();
        self.runs.clear();
        self.notice = None;
        self.picker = None;
        // The terminal belongs to the thread's workspace.
        self.terminal = None;
        self.backend(env).open_thread(thread_id);
        cx.notify();
    }

    fn new_thread(
        &mut self,
        env: EnvId,
        project_id: ProjectId,
        worktree: bool,
        cx: &mut Context<Self>,
    ) {
        let thread_id = ThreadId::new();
        self.pending_select = Some((env, thread_id));
        self.dispatch(
            env,
            Command::ThreadCreate {
                thread_id,
                project_id,
                title: String::new(),
                provider: self.default_provider,
                model: None,
                worktree,
                parent_thread_id: None,
            },
        );
        cx.notify();
    }

    fn send(&mut self, delivery: Delivery, window: &mut Window, cx: &mut Context<Self>) {
        let Some((env, thread_id)) = self.selected(cx) else {
            return;
        };
        let text = self.composer.read(cx).text().trim_end().to_owned();
        if text.trim().is_empty() {
            return;
        }
        let envelope = CommandEnvelope::new(Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: text.clone(),
            delivery,
        });
        self.pending_message = Some((env, envelope.command_id, text));
        self.backend(env).dispatch(envelope);
        self.notice = None;
        self.picker = None;
        self.composer.update(cx, |c, cx| c.set_text("", cx));
        window.focus(&self.composer.focus_handle(cx), cx);
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some((env, thread_id)) = self.selected(cx) {
            self.dispatch(env, Command::RunInterrupt { thread_id });
        }
    }

    fn fork(&mut self, up_to_run_id: Option<RunId>, cx: &mut Context<Self>) {
        let Some((env, source_thread_id)) = self.selected(cx) else {
            return;
        };
        let thread_id = ThreadId::new();
        self.pending_select = Some((env, thread_id));
        self.dispatch(
            env,
            Command::ThreadFork {
                source_thread_id,
                thread_id,
                up_to_run_id,
            },
        );
    }

    /// Ask before rolling back: it rewrites files (of every thread in the
    /// same folder) and cannot be undone from here.
    fn rollback(&mut self, run_id: RunId, cx: &mut Context<Self>) {
        let Some(thread) = self.selected_thread(cx) else {
            return;
        };
        let env = self.selected_env(cx);
        let Some(pos) = self.runs.iter().position(|(id, _)| *id == run_id) else {
            return;
        };
        let turns = self.runs[pos..]
            .iter()
            .filter(|(_, s)| {
                !matches!(
                    s,
                    RunStatus::Queued | RunStatus::Cancelled | RunStatus::RolledBack
                )
            })
            .count();
        // Paths of a remote environment are the server's: compared as
        // written (canonicalizing them here would look at this machine).
        let local = env == LOCAL;
        let norm = |p: &str| {
            if local {
                canonical(p)
            } else {
                PathBuf::from(p)
            }
        };
        let view = &self.sidebar.read(cx).envs[env];
        let sharers = view
            .project(thread.project_id)
            .map(|project| {
                let mine = norm(thread.cwd(project));
                view.threads
                    .iter()
                    .filter(|t| t.id != thread.id && !t.archived)
                    .filter(|t| {
                        view.project(t.project_id).is_some_and(|p| {
                            let theirs = norm(t.cwd(p));
                            theirs.starts_with(&mine) || mine.starts_with(&theirs)
                        })
                    })
                    .map(|t| (t.id, t.title.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.notice = None;
        self.confirm_rollback = Some(RollbackConfirm {
            env,
            thread_id: thread.id,
            run_id,
            turns,
            sharers,
        });
        cx.notify();
    }

    fn confirm_rollback(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm_rollback.take() else {
            return;
        };
        self.dispatch(
            confirm.env,
            Command::ThreadRollback {
                thread_id: confirm.thread_id,
                run_id: confirm.run_id,
                acknowledged_sharers: confirm.sharers.iter().map(|(id, _)| *id).collect(),
            },
        );
        cx.notify();
    }

    fn render_rollback_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::Stateful<gpui::Div>> {
        let confirm = self.confirm_rollback.as_ref()?;
        let turns = match confirm.turns {
            1 => "the last turn".to_owned(),
            n => format!("the last {n} turns"),
        };
        let mut text = format!(
            "Undo {turns}? Files in this folder go back to how they were before it; \
             the current files are kept in a hidden git ref."
        );
        match confirm.sharers.len() {
            0 => {}
            1 => text.push_str(&format!(
                " 1 other thread works in this folder and will see its files change: {}.",
                confirm.sharers[0].1
            )),
            n => text.push_str(&format!(
                " {n} other threads work in this folder and will see their files change: {}.",
                confirm
                    .sharers
                    .iter()
                    .map(|(_, title)| title.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
        Some(
            div()
                .id("rollback-confirm")
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme::warning_bg())
                .text_xs()
                .child(div().w_full().text_color(theme::text()).child(text))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap_2()
                        .child(button(
                            "rollback-cancel".into(),
                            "Cancel",
                            theme::surface_hover(),
                            theme::text(),
                            cx.listener(|this, _, _, cx| {
                                this.confirm_rollback = None;
                                cx.notify();
                            }),
                        ))
                        .child(button(
                            "rollback-confirm-button".into(),
                            "Undo",
                            theme::danger_bg(),
                            theme::danger(),
                            cx.listener(|this, _, _, cx| this.confirm_rollback(cx)),
                        )),
                ),
        )
    }

    /// Undo the latest turn that is still part of the conversation.
    fn undo_last(&mut self, cx: &mut Context<Self>) {
        let last = self
            .runs
            .iter()
            .rev()
            .find(|(_, s)| {
                !matches!(
                    s,
                    RunStatus::Queued | RunStatus::Cancelled | RunStatus::RolledBack
                )
            })
            .map(|(id, _)| *id);
        if let Some(run_id) = last {
            self.rollback(run_id, cx);
        }
    }

    fn set_provider(
        &mut self,
        provider: ProviderKind,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.picker = None;
        self.default_provider = provider;
        let Some(thread) = self.selected_thread(cx) else {
            cx.notify();
            return;
        };
        if thread.provider != provider || thread.model != model {
            self.dispatch_selected(
                Command::ThreadSetProvider {
                    thread_id: thread.id,
                    provider,
                    model,
                },
                cx,
            );
        }
        cx.notify();
    }

    /// Models the open thread's environment offered for `provider`.
    fn models(&self, provider: ProviderKind, cx: &App) -> Arc<[ModelInfo]> {
        self.envs[self.selected_env(cx)]
            .models
            .get(&provider)
            .cloned()
            .unwrap_or_else(|| Arc::from([]))
    }

    fn next_model(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.selected_thread(cx) else {
            return;
        };
        let models = self.models(thread.provider, cx);
        // Cycle: default → each offered model → default.
        let ids: Vec<Option<String>> = std::iter::once(None)
            .chain(models.iter().map(|m| Some(m.id.clone())))
            .collect();
        let at = ids.iter().position(|m| *m == thread.model).unwrap_or(0);
        let next = ids[(at + 1) % ids.len()].clone();
        self.set_provider(thread.provider, next, cx);
    }

    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.take().is_some() {
            window.focus(&self.composer.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        if let Some(term) = self.open_terminal(cx) {
            window.focus(&term.focus_handle(cx), cx);
        }
    }

    /// Open a shell in the open thread's workspace (worktree or project):
    /// on this machine for the local core, on the server for a remote
    /// environment.
    fn open_terminal(&mut self, cx: &mut Context<Self>) -> Option<Entity<TerminalView>> {
        let thread = self.selected_thread(cx)?;
        let env = self.selected_env(cx);
        let cwd = match &thread.worktree {
            Some(w) => PathBuf::from(&w.path),
            None => self.sidebar.read(cx).envs[env]
                .project(thread.project_id)
                .map(|p| PathBuf::from(&p.path))
                .unwrap_or_else(|| ".".into()),
        };
        let result = if self.backend(env).is_remote() {
            let id = self.next_terminal;
            self.next_terminal = self.next_terminal.wrapping_add(1).max(1);
            let title: SharedString = format!(
                "{} — {}",
                self.sidebar.read(cx).envs[env].name,
                cwd.display()
            )
            .into();
            TerminalView::open_remote(self.backend(env).clone(), id, thread.id, title, cx)
        } else {
            let shell = std::env::var("BLONGO_TERMINAL_SHELL").ok();
            TerminalView::open(&cwd, shell, cx)
        };
        match result {
            Ok(term) => {
                self.terminal = Some(term.clone());
                cx.notify();
                Some(term)
            }
            Err(err) => {
                self.notice = Some(format!("Cannot open a terminal: {err:#}").into());
                cx.notify();
                None
            }
        }
    }

    fn start_auto_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(auto) = self.auto_prompt.clone() else {
            return;
        };
        let (no_projects, first_project, no_threads) = {
            let s = &self.sidebar.read(cx).envs[LOCAL];
            (
                s.projects.is_empty(),
                s.projects.first().map(|p| p.id),
                s.threads.is_empty(),
            )
        };
        if no_projects {
            let project_id = ProjectId::new();
            self.dispatch(
                LOCAL,
                Command::ProjectCreate {
                    project_id,
                    name: String::new(),
                    path: auto.project_dir.to_string_lossy().into_owned(),
                },
            );
            self.new_thread(LOCAL, project_id, false, cx);
        } else if no_threads && let Some(project_id) = first_project {
            self.new_thread(LOCAL, project_id, false, cx);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(auto.delay).await;
            this.update(cx, |this, cx| {
                let target = this.selected(cx).or(this.sidebar.read(cx).envs[LOCAL]
                    .threads
                    .first()
                    .map(|t| (LOCAL, t.id)));
                if let Some((env, thread_id)) = target {
                    this.dispatch(
                        env,
                        Command::MessageDispatch {
                            thread_id,
                            message_id: ItemId::new(),
                            run_id: RunId::new(),
                            text: auto.prompt.clone(),
                            delivery: Delivery::Queue,
                        },
                    );
                }
                cx.notify();
            })
            .ok();
            if auto.terminal {
                this.update(cx, |this, cx| {
                    this.open_terminal(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    // ------------------------------------------------------------- rendering

    fn render_picker(&self, thread: &Thread, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let picker = self.picker?;
        let mut menu = div()
            .absolute()
            .bottom(px(46.))
            .left(px(8.))
            .w(px(300.))
            .p_1()
            .rounded_md()
            .bg(theme::surface_hover())
            .border_1()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .text_sm();
        let row = |id: SharedString, label: SharedString, active: bool| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|d| d.bg(theme::surface()))
                .flex()
                .justify_between()
                .child(label)
                .when(active, |d| {
                    d.child(div().text_color(theme::accent()).child("✓"))
                })
        };
        match picker {
            Picker::Provider => {
                for provider in ProviderKind::ALL {
                    let caps = provider.capabilities();
                    menu =
                        menu.child(
                            row(
                                format!("pick-{}", provider.id()).into(),
                                provider.label().into(),
                                thread.provider == provider,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| this.set_provider(provider, None, cx),
                            )),
                        );
                    if caps.interactive_login {
                        menu = menu.child(
                            div()
                                .flex()
                                .gap_2()
                                .pl_4()
                                .pb_1()
                                .text_xs()
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "install-{}",
                                            provider.id()
                                        )))
                                        .text_color(theme::text_muted())
                                        .hover(|d| d.text_color(theme::text()))
                                        .cursor_pointer()
                                        .child("Install")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.picker = None;
                                            this.backend(this.selected_env(cx))
                                                .install_antigravity();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("login-{}", provider.id())))
                                        .text_color(theme::text_muted())
                                        .hover(|d| d.text_color(theme::text()))
                                        .cursor_pointer()
                                        .child("Sign in")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.picker = None;
                                            this.backend(this.selected_env(cx)).login(provider);
                                            cx.notify();
                                        })),
                                ),
                        );
                    }
                }
            }
            Picker::Model => {
                menu = menu.child(
                    row(
                        "model-default".into(),
                        "Default".into(),
                        thread.model.is_none(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let provider = this
                            .selected_thread(cx)
                            .map_or(this.default_provider, |t| t.provider);
                        this.set_provider(provider, None, cx)
                    })),
                );
                let models = self.models(thread.provider, cx);
                if models.is_empty() {
                    menu = menu.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(theme::text_faint())
                            .child("Models appear once the provider has started."),
                    );
                }
                for (ix, model) in models.iter().enumerate() {
                    let id = model.id.clone();
                    let provider = thread.provider;
                    menu = menu.child(
                        row(
                            format!("model-{ix}").into(),
                            model.label.clone().into(),
                            thread.model.as_deref() == Some(&model.id),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_provider(provider, Some(id.clone()), cx)
                        })),
                    );
                }
            }
        }
        Some(menu.into_any_element())
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(fatal) = self.fatal.clone() {
            return div()
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .p_8()
                .text_sm()
                .text_color(theme::danger())
                .child(fatal)
                .into_any_element();
        }
        let Some(thread) = self.selected_thread(cx) else {
            let hint = if self
                .sidebar
                .read(cx)
                .envs
                .iter()
                .all(|e| e.projects.is_empty())
            {
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
        let env = self.selected_env(cx);
        let mut location = match &thread.worktree {
            Some(w) => format!("{}  ⑂ {}", w.path, w.branch),
            None => self.sidebar.read(cx).envs[env]
                .project(thread.project_id)
                .map(|p| p.path.clone())
                .unwrap_or_default(),
        };
        if env != LOCAL {
            location = format!("{} · {location}", self.sidebar.read(cx).envs[env].name);
        }
        let busy = matches!(thread.status, ThreadStatus::Running | ThreadStatus::Waiting);
        let provider = thread.provider;
        let model_label: SharedString = match &thread.model {
            Some(id) => self
                .models(provider, cx)
                .iter()
                .find(|m| &m.id == id)
                .map_or_else(|| id.clone(), |m| m.label.clone())
                .into(),
            None => "Default model".into(),
        };

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
                    .child(SharedString::from(location)),
            )
            .child(
                header_action("fork-thread", "Fork")
                    .on_click(cx.listener(|this, _, _, cx| this.fork(None, cx))),
            )
            .child(
                header_action(
                    "toggle-terminal",
                    if self.terminal.is_some() {
                        "Close terminal"
                    } else {
                        "Terminal"
                    },
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
            );

        let timeline = match &self.timeline {
            Some(t) => div().flex_1().min_h_0().child(t.clone()),
            None => div().flex_1(),
        };

        let has_text = !self.composer.read(cx).text().trim().is_empty();
        let actions = if busy {
            div()
                .flex()
                .gap_2()
                .when(has_text, |d| {
                    d.child(button(
                        "queue".into(),
                        "Queue",
                        theme::surface_hover(),
                        theme::text(),
                        cx.listener(|this, _, window, cx| this.send(Delivery::Queue, window, cx)),
                    ))
                    .child(button(
                        "steer".into(),
                        "Steer",
                        theme::accent_bg(),
                        theme::text(),
                        cx.listener(|this, _, window, cx| this.send(Delivery::Steer, window, cx)),
                    ))
                })
                .child(button(
                    "stop".into(),
                    "Stop",
                    theme::danger_bg(),
                    theme::danger(),
                    cx.listener(|this, _, _, cx| this.stop(cx)),
                ))
        } else {
            div().child(button(
                "send".into(),
                "Send",
                theme::accent_bg(),
                theme::text(),
                cx.listener(|this, _, window, cx| this.send(Delivery::Queue, window, cx)),
            ))
        };

        let status_text: SharedString = match thread.status {
            ThreadStatus::Running => format!("{provider} is working…").into(),
            ThreadStatus::Waiting => format!("{provider} is waiting for your approval").into(),
            _ => "on-request approvals".into(),
        };

        let queued = (!self.queued.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(self.queued.iter().map(|(run_id, text)| {
                    let run_id = *run_id;
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme::surface())
                        .border_1()
                        .border_color(theme::border())
                        .text_xs()
                        .child(div().text_color(theme::text_faint()).child("Queued"))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .child(text.clone()),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("cancel-{run_id}")))
                                .px_1()
                                .text_color(theme::text_faint())
                                .hover(|d| d.text_color(theme::danger()))
                                .cursor_pointer()
                                .child("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some((env, thread_id)) = this.selected(cx) {
                                        this.dispatch(
                                            env,
                                            Command::RunCancel { thread_id, run_id },
                                        );
                                    }
                                })),
                        )
                }))
        });

        let picker = self.render_picker(&thread, cx);
        let rollback_confirm = self.render_rollback_confirm(cx);
        let composer = div().flex().justify_center().px_6().pb_4().child(
            div()
                .w_full()
                .max_w(px(820.))
                .flex()
                .flex_col()
                .gap_2()
                .children(queued)
                .children(rollback_confirm)
                .when_some(self.notice.clone(), |d, notice| {
                    d.child(div().text_xs().text_color(theme::danger()).child(notice))
                })
                .when_some(self.provider_notice.clone(), |d, notice| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child(notice),
                    )
                })
                .child(
                    div()
                        .relative()
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
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            pill("provider-picker", provider.label().into())
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.picker = match this.picker {
                                                        Some(Picker::Provider) => None,
                                                        _ => Some(Picker::Provider),
                                                    };
                                                    cx.notify();
                                                })),
                                        )
                                        .child(pill("model-picker", model_label).on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.picker = match this.picker {
                                                    Some(Picker::Model) => None,
                                                    _ => Some(Picker::Model),
                                                };
                                                cx.notify();
                                            }),
                                        ))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme::text_faint())
                                                .child(status_text),
                                        ),
                                )
                                .child(actions),
                        )
                        .children(picker),
                ),
        );

        let terminal = self.terminal.clone().map(|term| {
            // Fit the grid to the panel (cell size from the mono font).
            let font = gpui::Font {
                family: theme::MONO.into(),
                ..gpui::Font::default()
            };
            let ts = window.text_system();
            let cell_w = ts
                .resolve_font(&font)
                .pipe(|id| ts.advance(id, px(terminal::FONT_SIZE), 'm').ok())
                .map_or(7.2, |a| f32::from(a.width));
            let width = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH - 16.;
            let size = GridSize {
                columns: (width / cell_w).floor().max(10.) as usize,
                lines: ((TERMINAL_HEIGHT - 30.) / terminal::LINE_HEIGHT).floor() as usize,
            };
            term.update(cx, |t, _| t.resize(size));
            let title = term.read(cx).title.clone();
            div()
                .h(px(TERMINAL_HEIGHT))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme::border())
                .child(
                    div()
                        .px_3()
                        .py_0p5()
                        .text_xs()
                        .text_color(theme::text_faint())
                        .child(title),
                )
                .child(div().flex_1().min_h_0().child(term))
        });

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .min_w_0()
            .child(header)
            .child(timeline)
            .child(composer)
            .children(terminal)
            .into_any_element()
    }
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

fn header_action(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .text_color(theme::text_muted())
        .hover(|d| d.bg(theme::surface_hover()).text_color(theme::text()))
        .cursor_pointer()
        .child(label)
}

fn pill(id: &'static str, label: SharedString) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .gap_1()
        .whitespace_nowrap()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(theme::surface_hover())
        .text_xs()
        .text_color(theme::text_muted())
        .hover(|d| d.text_color(theme::text()))
        .cursor_pointer()
        .child(label)
        .child("▾")
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
        let mut sidebar_style = StyleRefinement::default();
        sidebar_style.size.width = Some(px(SIDEBAR_WIDTH).into());
        sidebar_style.size.height = Some(gpui::relative(1.).into());
        sidebar_style.flex_shrink = Some(0.);
        div()
            .key_context("Shell")
            .on_action(cx.listener(|this, _: &NewThread, _, cx| {
                let env = this.selected_env(cx);
                let project = this.selected_thread(cx).map(|t| t.project_id).or(this
                    .sidebar
                    .read(cx)
                    .envs[env]
                    .projects
                    .first()
                    .map(|p| p.id));
                if let Some(project_id) = project {
                    this.new_thread(env, project_id, false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ForkThread, _, cx| this.fork(None, cx)))
            .on_action(cx.listener(|this, _: &UndoLastTurn, _, cx| this.undo_last(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleTerminal, window, cx| {
                    this.toggle_terminal(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &UseCodex, _, cx| {
                this.set_provider(ProviderKind::Codex, None, cx)
            }))
            .on_action(cx.listener(|this, _: &UseClaudeCode, _, cx| {
                this.set_provider(ProviderKind::ClaudeCode, None, cx)
            }))
            .on_action(cx.listener(|this, _: &UseAntigravity, _, cx| {
                this.set_provider(ProviderKind::Antigravity, None, cx)
            }))
            .on_action(cx.listener(|this, _: &NextModel, _, cx| this.next_model(cx)))
            .flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            // Cached: re-rendered only when the sidebar itself is notified.
            .child(self.sidebar.clone().cached(sidebar_style))
            .child(self.render_main(window, cx))
    }
}

struct RollbackConfirm {
    env: EnvId,
    thread_id: ThreadId,
    run_id: RunId,
    /// Turns the rollback drops.
    turns: usize,
    /// Titles of the other threads working in the same folder.
    sharers: Vec<(ThreadId, String)>,
}

fn canonical(path: &str) -> PathBuf {
    std::path::Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}
