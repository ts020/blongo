//! The window: sidebar (environments → projects → threads), the open
//! thread's timeline, the composer and the terminal panel.
//!
//! Every environment is a [`Backend`]: environment 0 is the in-process core
//! (typed values over channels, nothing serialized); the others are
//! `blongo serve` instances reached over the wire. Each one's events are
//! pumped separately and tagged with its [`EnvId`]; past that point the
//! shell treats them alike.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use blongo_client::environments::{self, Environment, EnvironmentFile};
use blongo_client::{Backend, Events};
use blongo_core::{CoreEvent, InstallState, LoginState};
use blongo_protocol::client::{ConnectionState, TerminalEvent};
use blongo_protocol::workspace::{DiffScope, Query, QueryReply};
use blongo_protocol::{
    ApprovalState, Command, CommandEnvelope, CommandId, Delivery, EventKind, ItemId, ItemKind,
    ModelInfo, ProjectId, ProviderKind, RunId, RunStatus, Schedule, ScheduleId, Thread, ThreadId,
    ThreadSnapshot, ThreadStatus, Timestamp, Usage,
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, SharedString, StyleRefinement,
    Subscription, Window, div, prelude::*, px,
};

use crate::deeplink::Link;
use crate::diff::{DiffEvent, DiffView};
use crate::files::FilesView;
use crate::inbox::InboxView;
use crate::input::{InputEvent, TextInput};
use crate::keymap::{Binding, Cmd};
use crate::palette::{Palette, PaletteEvent};
use crate::pr_create::PrCreateView;
use crate::pr_view::PrView;
use crate::settings::{NotifyMode, Settings, ThemeMode};
use crate::settings_view::{SettingsEvent, SettingsView, ShellInfo};
use crate::sidebar::{EnvId, EnvView, LOCAL, Sidebar, SidebarEvent};
use crate::terminal::{self, GridSize, TerminalView};
use crate::theme;
use crate::timeline::{Timeline, TimelineEvent, button};

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
    /// Open this view after the run (or after `delay` when `prompt` is
    /// empty): memory of the diff panel / file browser.
    pub view: Option<View>,
}

/// What the main area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Chat,
    Diff,
    Files,
    /// The thread's pull request.
    Pr,
    Inbox,
    Settings,
}

/// Fixed facts the shell shows (settings screen) or needs.
pub struct ShellOptions {
    pub data_dir: PathBuf,
    pub mcp: bool,
    pub bindings: Vec<Binding>,
    pub keybinding_problems: Vec<String>,
    /// `blongo://` links from later processes (and the one this process
    /// was started with).
    pub links: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
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
    /// A `blongo://project` link for a folder that is not a project yet:
    /// added only when the user confirms.
    confirm_link_project: Option<String>,
    /// Last rejected command's reason.
    notice: Option<SharedString>,
    /// The answer to the last thing the user asked for that went well
    /// (a pull request check), shown muted.
    info: Option<SharedString>,
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
    view: View,
    diff: Option<Entity<DiffView>>,
    /// Thread and scope the diff view shows.
    diff_for: Option<(EnvId, ThreadId, DiffScope)>,
    diff_scope: DiffScope,
    files: Option<Entity<FilesView>>,
    files_for: Option<(EnvId, ThreadId)>,
    /// The PR tab (dropped, with its detail, when the tab closes).
    pr_view: Option<Entity<PrView>>,
    /// The Create PR form (the PR tab of a thread with no pull request).
    pr_create: Option<Entity<PrCreateView>>,
    pr_view_for: Option<(EnvId, ThreadId)>,
    /// When a thread's branch was last looked up on GitHub on opening it.
    pr_lookups: HashMap<(EnvId, ThreadId), std::time::Instant>,
    /// A file to open once the files view exists.
    open_file: Option<String>,
    inbox: Option<Entity<InboxView>>,
    settings_view: Option<Entity<SettingsView>>,
    palette: Option<Entity<Palette>>,
    bindings: Vec<Binding>,
    keybinding_problems: Vec<String>,
    /// Each environment's schedules.
    schedules: Vec<Vec<Schedule>>,
    pending_schedule: Option<(EnvId, CommandId)>,
    /// GitHub settings sent from the settings screen.
    pending_forge: Option<(EnvId, CommandId)>,
    /// Token use of the open thread's runs.
    usage: Vec<(RunId, Usage)>,
    /// Links waiting for the local core's first snapshot.
    links: Vec<Link>,
    window_active: bool,
    data_dir: PathBuf,
    mcp: bool,
    /// Subscriptions of the views above (replaced with them).
    view_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    /// `local`: the in-process core and its events. Remote environments
    /// come from the saved environments file.
    pub fn new(
        local: Arc<dyn Backend>,
        events: Events,
        auto_prompt: Option<AutoPrompt>,
        options: ShellOptions,
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
            cx.observe_window_activation(window, |this, window, _| {
                this.window_active = window.is_window_active();
            }),
        ];
        Self::pump(LOCAL, events, cx);
        if let Some(mut links) = options.links {
            cx.spawn(async move |this, cx| {
                while let Some(url) = links.recv().await {
                    if this.update(cx, |this, cx| this.on_link(&url, cx)).is_err() {
                        return;
                    }
                }
            })
            .detach();
        }
        let settings = cx.global::<Settings>().value.clone();
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
            default_provider: settings.default_provider,
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
            view: View::Chat,
            diff: None,
            diff_for: None,
            diff_scope: DiffScope::Thread,
            files: None,
            files_for: None,
            pr_view: None,
            pr_create: None,
            pr_view_for: None,
            pr_lookups: HashMap::new(),
            info: None,
            open_file: None,
            inbox: None,
            settings_view: None,
            confirm_link_project: None,
            palette: None,
            bindings: options.bindings,
            keybinding_problems: options.keybinding_problems,
            schedules: vec![Vec::new()],
            pending_schedule: None,
            pending_forge: None,
            usage: Vec::new(),
            links: Vec::new(),
            window_active: true,
            data_dir: options.data_dir,
            mcp: options.mcp,
            view_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        };
        if settings.check_updates
            && let Some((url, key)) = blongo_client::update::configured()
        {
            let task = blongo_client::net::handle().spawn(async move {
                blongo_client::update::check(&url, &key, env!("CARGO_PKG_VERSION")).await
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(blongo_client::update::Status::Available(m))) = task.await {
                    this.update(cx, |this, cx| {
                        this.notice = Some(
                            format!("Blongo {} is available (Settings → Updates)", m.version)
                                .into(),
                        );
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
        if !this.keybinding_problems.is_empty() {
            this.notice =
                Some(format!("keybindings.json: {}", this.keybinding_problems.join("; ")).into());
        }
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
        self.schedules.push(Vec::new());
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
            SidebarEvent::OpenInbox => self.set_view(View::Inbox, cx),
            SidebarEvent::OpenSettings => self.set_view(View::Settings, cx),
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
            CoreEvent::Reply { id, result } => crate::query::deliver(id, result, cx),
            CoreEvent::Shell(shell) => {
                self.schedules[env] = shell.schedules.clone();
                self.refresh_settings_info(cx);
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
                    for link in std::mem::take(&mut self.links) {
                        self.open_link(link, cx);
                    }
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
                if self.pending_schedule == Some((env, command_id)) {
                    self.pending_schedule = None;
                    if let Some(view) = &self.settings_view {
                        let reason = reason.clone();
                        view.update(cx, |v, cx| v.set_message(false, reason, cx));
                    }
                }
                if self.pending_forge == Some((env, command_id)) {
                    self.pending_forge = None;
                    if let Some(view) = &self.settings_view {
                        let reason = reason.clone();
                        view.update(cx, |v, cx| v.forge_refused(reason, cx));
                    }
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
            CoreEvent::RunFinished { thread_id, status } => {
                if let Some(auto) = &self.auto_prompt {
                    // tools/profile.py waits for this line.
                    eprintln!("blongo: replay done");
                    if let Some(view) = auto.view {
                        self.open_profile_view(view, cx);
                    }
                }
                let title = self.sidebar.read(cx).envs[env]
                    .thread(thread_id)
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                self.notify(
                    &format!("{} — {}", status_word(status), title),
                    "Blongo: a run finished",
                    cx,
                );
                if self.is_selected(env, thread_id, cx) {
                    if let Some(diff) = &self.diff {
                        diff.update(cx, |d, cx| d.reload(cx));
                    }
                    if let Some(files) = &self.files {
                        files.update(cx, |f, cx| f.refresh_git(cx));
                    }
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
                if message.starts_with("Automatic CI fixes stopped") {
                    self.notify(&message, "Blongo", cx);
                }
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
                TimelineEvent::Diff(run_id) => {
                    this.diff_scope = DiffScope::Turn { run_id: *run_id };
                    this.set_view(View::Diff, cx);
                }
            },
        );
        self.usage = snapshot
            .runs
            .iter()
            .filter_map(|r| Some((r.id, r.usage?)))
            .collect();
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
            EventKind::RunUsage {
                thread_id,
                run_id,
                usage,
            } => {
                if self.is_selected(env, *thread_id, cx) {
                    match self.usage.iter_mut().find(|(id, _)| id == run_id) {
                        Some(u) => u.1 = *usage,
                        None => self.usage.push((*run_id, *usage)),
                    }
                    cx.notify();
                }
            }
            EventKind::ScheduleCreated { schedule } => {
                self.schedules[env].push(schedule.clone());
                if command_id.is_some()
                    && self.pending_schedule.map(|(e, id)| (e, Some(id))) == Some((env, command_id))
                {
                    self.pending_schedule = None;
                    if let Some(view) = &self.settings_view {
                        view.update(cx, |v, cx| v.schedule_created(cx));
                    }
                }
                self.refresh_settings_info(cx);
            }
            EventKind::ScheduleUpdated { schedule } => {
                if let Some(s) = self.schedules[env].iter_mut().find(|s| s.id == schedule.id) {
                    *s = schedule.clone();
                }
                self.refresh_settings_info(cx);
            }
            EventKind::ScheduleDeleted { schedule_id } => {
                self.schedules[env].retain(|s| s.id != *schedule_id);
                self.refresh_settings_info(cx);
            }
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
                let thread = (**thread).clone();
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
            EventKind::ThreadPrLinked {
                thread_id,
                pr,
                manual,
            } => {
                let changed = self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| {
                        t.pr = pr.clone();
                        t.pr_status = None;
                        t.pr_dismissed = pr.is_none() && *manual;
                        true
                    })
                });
                if changed && self.is_selected(env, *thread_id, cx) {
                    if self.view == View::Pr {
                        // Linked (the form becomes the pull request),
                        // unlinked, or another one: start over.
                        self.pr_view = None;
                        self.pr_create = None;
                        self.pr_view_for = None;
                        if pr.is_none() {
                            self.view = View::Chat;
                        }
                    }
                    cx.notify();
                }
            }
            EventKind::ThreadBranchRenamed { thread_id, branch } => {
                let changed = self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| match t.worktree.as_mut() {
                        Some(w) => {
                            w.branch = branch.clone();
                            true
                        }
                        None => false,
                    })
                });
                if changed && self.is_selected(env, *thread_id, cx) {
                    cx.notify();
                }
            }
            EventKind::ThreadPrStatus { thread_id, status } => {
                let mut note = None;
                let changed = self.sidebar.update(cx, |s, cx| {
                    s.update_thread(env, *thread_id, cx, |t| {
                        if let (Some(pr), Some(new)) = (&t.pr, status) {
                            note = crate::pr::transition(t.pr_status.as_ref(), new)
                                .map(|what| (format!("{} {what}", pr.label()), t.title.clone()));
                        }
                        t.pr_status = status.clone();
                        true
                    })
                });
                if let Some((body, title)) = note {
                    self.notify(&body, &title, cx);
                }
                if let (Some(pr), Some(status)) = (&self.pr_view, status)
                    && self.pr_view_for == Some((env, *thread_id))
                {
                    pr.update(cx, |p, cx| p.on_status(status, cx));
                }
                if changed && self.is_selected(env, *thread_id, cx) {
                    cx.notify();
                }
            }
            EventKind::ProjectForgeChanged {
                project_id,
                settings,
            } => {
                self.sidebar.update(cx, |s, cx| {
                    if let Some(p) = s.envs[env]
                        .projects
                        .iter_mut()
                        .find(|p| p.id == *project_id)
                    {
                        p.forge = settings.clone();
                        cx.notify();
                    }
                });
                self.refresh_settings_info(cx);
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
                // A turn ended: the PR tab's local state (commits to push,
                // a fix on its way) may have changed.
                if status.is_terminal() {
                    self.reload_pr_view(env, *thread_id, cx);
                }
            }
            EventKind::ItemAdded { item } | EventKind::ItemUpdated { item } => {
                // A notice outside a turn (a CI fix pushed, …).
                if matches!(kind, EventKind::ItemAdded { .. })
                    && item.run_id.is_none()
                    && matches!(item.kind, ItemKind::SystemNotice { .. })
                {
                    self.reload_pr_view(env, item.thread_id, cx);
                }
                if let (
                    EventKind::ItemAdded { .. },
                    ItemKind::ApprovalRequest {
                        state: ApprovalState::Pending,
                        title,
                        ..
                    },
                ) = (kind, &item.kind)
                {
                    let thread = self.sidebar.read(cx).envs[env]
                        .thread(item.thread_id)
                        .map(|t| t.title.clone())
                        .unwrap_or_default();
                    self.notify(&format!("{thread}: {title}"), "Blongo: approval needed", cx);
                }
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
        self.usage.clear();
        self.diff_scope = DiffScope::Thread;
        if matches!(self.view, View::Inbox | View::Settings) {
            self.view = View::Chat;
        }
        if self.view == View::Pr {
            self.pr_view = None;
            self.pr_create = None;
            self.pr_view_for = None;
            if self.selected_thread(cx).is_none_or(|t| t.pr.is_none()) {
                self.view = View::Chat;
            }
        }
        self.info = None;
        self.backend(env).open_thread(thread_id);
        self.look_up_pr(env, cx);
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
                if auto.prompt.is_empty() {
                    if let Some(view) = auto.view {
                        this.open_profile_view(view, cx);
                    }
                } else if let Some((env, thread_id)) = target {
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

    /// Profiling: open a view and give it work (tools/profile.py waits for
    /// "view opened" / "search done").
    fn open_profile_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.set_view(view, cx);
        eprintln!("blongo: view opened");
        // `BLONGO_PROFILE_CLOSE_MS`: back to the chat after that long
        // (memory once the view is closed).
        if let Some(ms) = std::env::var("BLONGO_PROFILE_CLOSE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(ms))
                    .await;
                this.update(cx, |this, cx| {
                    this.set_view(View::Chat, cx);
                    eprintln!("blongo: view closed");
                })
                .ok();
            })
            .detach();
        }
        if view == View::Files
            && let Some((env, thread_id)) = self.selected(cx)
        {
            self.open_file = Some("README.md".into());
            let backend = self.backend(env).clone();
            let query = blongo_protocol::workspace::Query::SearchFiles {
                thread_id,
                pattern: "index".into(),
                limit: 50,
            };
            crate::query::ask(
                &backend,
                query,
                cx.weak_entity(),
                cx,
                |_, result, _| match result {
                    Ok(blongo_protocol::workspace::QueryReply::Files(f)) => {
                        eprintln!("blongo: search done ({} matches)", f.len())
                    }
                    other => eprintln!("blongo: search done ({other:?})"),
                },
            );
        }
    }

    // ------------------------------------------------------ views, commands

    pub fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if matches!(view, View::Chat | View::Diff | View::Files | View::Pr)
            && self.selected(cx).is_none()
        {
            return;
        }
        if view == View::Pr {
            let thread = self.selected_thread(cx);
            if thread
                .as_ref()
                .is_none_or(|t| t.pr.is_none() && !crate::pr::owns_branch(t))
            {
                self.notice = Some(
                    "No pull request is linked to this thread, and it has no worktree of its own \
                     to create one from"
                        .into(),
                );
                cx.notify();
                return;
            }
            if self.view == View::Pr {
                // Same tab again: fetch again.
                if let Some(pr) = &self.pr_view {
                    pr.update(cx, |p, cx| p.reload(cx));
                }
                if let Some(form) = &self.pr_create {
                    form.update(cx, |f, cx| f.reload(cx));
                }
            }
        }
        if self.view == View::Pr && view != View::Pr {
            self.pr_view = None;
            self.pr_create = None;
            self.pr_view_for = None;
        }
        if view == View::Diff && self.view == View::Diff {
            // Same tab again: show the latest state.
            if let Some(diff) = &self.diff {
                diff.update(cx, |d, cx| d.reload(cx));
            }
        }
        // Leaving the diff view frees what it loaded (patches can be tens
        // of MiB), unless it holds comments the user is still writing.
        if self.view == View::Diff
            && view != View::Diff
            && self
                .diff
                .as_ref()
                .is_some_and(|d| d.read(cx).comments.is_empty())
        {
            self.diff = None;
            self.diff_for = None;
            trim_heap_soon(cx);
        }
        self.view = view;
        self.palette = None;
        if view == View::Chat {
            self.pending_focus = Some(self.composer.focus_handle(cx));
        }
        cx.notify();
    }

    fn ensure_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<DiffView> {
        let (env, thread_id) = self.selected(cx).expect("diff view without a thread");
        let key = (env, thread_id, self.diff_scope);
        if let (Some(diff), true) = (&self.diff, self.diff_for == Some(key)) {
            return diff.clone();
        }
        let title = match self.diff_scope {
            DiffScope::Thread => "All changes in this thread".to_owned(),
            DiffScope::Turn { run_id } => format!("Changes of turn {}", self.turn_number(run_id)),
        };
        let backend = self.backend(env).clone();
        let scope = self.diff_scope;
        let diff = cx.new(|cx| {
            DiffView::new(
                crate::diff::Source::Thread {
                    backend,
                    thread_id,
                    scope,
                },
                title,
                "Send to agent",
                window,
                cx,
            )
        });
        self.view_subscriptions.push(cx.subscribe_in(
            &diff,
            window,
            |this, diff, event: &DiffEvent, window, cx| {
                let DiffEvent::Send(comments) = event;
                if comments.is_empty() {
                    return;
                }
                let Some((env, thread_id)) = this.selected(cx) else {
                    return;
                };
                this.dispatch(
                    env,
                    Command::MessageDispatch {
                        thread_id,
                        message_id: ItemId::new(),
                        run_id: RunId::new(),
                        text: crate::diff::comments_message(comments),
                        delivery: Delivery::Queue,
                    },
                );
                diff.update(cx, |d, cx| d.clear_comments(cx));
                this.set_view(View::Chat, cx);
                window.focus(&this.composer.focus_handle(cx), cx);
            },
        ));
        self.diff = Some(diff.clone());
        self.diff_for = Some(key);
        diff
    }

    fn ensure_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<FilesView> {
        let (env, thread_id) = self.selected(cx).expect("files view without a thread");
        let files = match (&self.files, self.files_for == Some((env, thread_id))) {
            (Some(files), true) => files.clone(),
            _ => {
                let backend = self.backend(env).clone();
                let files = cx.new(|cx| FilesView::new(backend, thread_id, window, cx));
                self.files = Some(files.clone());
                self.files_for = Some((env, thread_id));
                files
            }
        };
        if let Some(path) = self.open_file.take() {
            files.update(cx, |f, cx| f.open_file(path, cx));
        }
        files
    }

    fn ensure_pr(&mut self, cx: &mut Context<Self>) -> Entity<PrView> {
        let (env, thread_id) = self.selected(cx).expect("PR view without a thread");
        self.pr_create = None;
        match (&self.pr_view, self.pr_view_for == Some((env, thread_id))) {
            (Some(pr), true) => pr.clone(),
            _ => {
                let backend = self.backend(env).clone();
                let pr = cx.new(|cx| PrView::new(backend, thread_id, cx));
                self.pr_view = Some(pr.clone());
                self.pr_view_for = Some((env, thread_id));
                pr
            }
        }
    }

    fn ensure_pr_create(&mut self, cx: &mut Context<Self>) -> Entity<PrCreateView> {
        let (env, thread_id) = self.selected(cx).expect("PR view without a thread");
        self.pr_view = None;
        match (&self.pr_create, self.pr_view_for == Some((env, thread_id))) {
            (Some(form), true) => form.clone(),
            _ => {
                let backend = self.backend(env).clone();
                let form = cx.new(|cx| PrCreateView::new(backend, thread_id, cx));
                self.pr_create = Some(form.clone());
                self.pr_view_for = Some((env, thread_id));
                form
            }
        }
    }

    /// Opening a thread whose branch may have a pull request nobody linked
    /// yet (pushed by the agent, made on GitHub): look once a minute at
    /// most. The core asks GitHub only if the branch was pushed.
    fn look_up_pr(&mut self, env: EnvId, cx: &mut Context<Self>) {
        let Some(thread) = self.selected_thread(cx) else {
            return;
        };
        if thread.pr.is_some()
            || thread.pr_dismissed
            || thread.archived
            || !crate::pr::owns_branch(&thread)
        {
            return;
        }
        let now = std::time::Instant::now();
        let key = (env, thread.id);
        if self
            .pr_lookups
            .get(&key)
            .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(60))
        {
            return;
        }
        self.pr_lookups.insert(key, now);
        let backend = self.backend(env).clone();
        crate::query::ask(
            &backend,
            Query::PrRefresh {
                thread_id: thread.id,
            },
            cx.weak_entity(),
            cx,
            |_, _, _| {},
        );
    }

    fn ensure_inbox(&mut self, cx: &mut Context<Self>) -> Entity<InboxView> {
        if let Some(inbox) = &self.inbox {
            return inbox.clone();
        }
        let inbox = cx.new(InboxView::new);
        self.inbox = Some(inbox.clone());
        inbox
    }

    fn settings_info(&self, cx: &App) -> ShellInfo {
        let env = self.selected_env(cx);
        let sidebar = self.sidebar.read(cx);
        let schedule_target = self.selected_thread(cx).and_then(|t| {
            let project = sidebar.envs[env].project(t.project_id)?;
            Some(SharedString::from(format!(
                "{} ({})",
                project.name, t.title
            )))
        });
        let environments = EnvironmentFile::load(&environments::default_path())
            .map(|f| {
                f.environments
                    .into_iter()
                    .map(|e| (e.name, e.target))
                    .collect()
            })
            .unwrap_or_default();
        let forge = self.selected_thread(cx).and_then(|t| {
            let project = sidebar.envs[env].project(t.project_id)?;
            Some((
                project.id,
                SharedString::from(project.name.clone()),
                project.forge.clone(),
            ))
        });
        ShellInfo {
            forge,
            schedules: self.schedules.get(env).cloned().unwrap_or_default(),
            environments,
            schedule_target,
            keybinding_problems: self.keybinding_problems.clone(),
            data_dir: self.data_dir.clone(),
            mcp: self.mcp,
        }
    }

    fn refresh_settings_info(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.settings_view.clone() {
            let info = self.settings_info(cx);
            view.update(cx, |v, cx| {
                v.info = info;
                cx.notify();
            });
        }
    }

    fn ensure_settings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SettingsView> {
        if let Some(view) = &self.settings_view {
            return view.clone();
        }
        let info = self.settings_info(cx);
        let view = cx.new(|cx| SettingsView::new(info, window, cx));
        self.view_subscriptions
            .push(cx.subscribe_in(&view, window, Self::on_settings_event));
        self.settings_view = Some(view.clone());
        view
    }

    fn on_settings_event(
        &mut self,
        view: &Entity<SettingsView>,
        event: &SettingsEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SettingsEvent::Changed => self.apply_settings(cx),
            SettingsEvent::CreateSchedule {
                cron,
                prompt,
                in_open_thread,
            } => {
                let Some(thread) = self.selected_thread(cx) else {
                    return;
                };
                let env = self.selected_env(cx);
                let envelope = CommandEnvelope::new(Command::ScheduleCreate {
                    schedule_id: ScheduleId::new(),
                    project_id: thread.project_id,
                    thread_id: in_open_thread.then_some(thread.id),
                    cron: cron.clone(),
                    prompt: prompt.clone(),
                    provider: thread.provider,
                    proposed_by: None,
                });
                self.pending_schedule = Some((env, envelope.command_id));
                self.backend(env).dispatch(envelope);
            }
            SettingsEvent::ToggleSchedule(id, enabled) => self.dispatch_selected(
                Command::ScheduleUpdate {
                    schedule_id: *id,
                    enabled: Some(*enabled),
                    cron: None,
                    prompt: None,
                },
                cx,
            ),
            SettingsEvent::SetForge(project_id, settings) => {
                let env = self.selected_env(cx);
                let envelope = CommandEnvelope::new(Command::ProjectSetForge {
                    project_id: *project_id,
                    settings: settings.clone(),
                });
                self.pending_forge = Some((env, envelope.command_id));
                self.backend(env).dispatch(envelope);
            }
            SettingsEvent::RunSchedule(id) => {
                self.dispatch_selected(Command::ScheduleRunNow { schedule_id: *id }, cx)
            }
            SettingsEvent::DeleteSchedule(id) => {
                self.dispatch_selected(Command::ScheduleDelete { schedule_id: *id }, cx)
            }
            SettingsEvent::RemoveEnvironment(name) => {
                let result =
                    EnvironmentFile::update(&environments::default_path(), |f| f.remove(name));
                let (ok, text) = match result {
                    Ok(_) => (
                        true,
                        format!(
                            "Removed {name}: it disconnects at the next start; its credential \
                             works on the server until `blongo-serve revoke` there"
                        ),
                    ),
                    Err(err) => (false, err),
                };
                view.update(cx, |v, cx| v.set_message(ok, text, cx));
                self.refresh_settings_info(cx);
            }
            SettingsEvent::ReloadKeybindings => {
                self.reload_keybindings(cx);
                let text = if self.keybinding_problems.is_empty() {
                    "Keybindings reloaded".to_owned()
                } else {
                    format!(
                        "Reloaded with problems: {}",
                        self.keybinding_problems.join("; ")
                    )
                };
                let ok = self.keybinding_problems.is_empty();
                view.update(cx, |v, cx| v.set_message(ok, text, cx));
                self.refresh_settings_info(cx);
            }
            SettingsEvent::TestNotification => {
                crate::notify::send("Blongo", "Notifications work.");
            }
        }
    }

    pub fn reload_keybindings(&mut self, cx: &mut Context<Self>) {
        let (bindings, problems) = crate::bind_all(cx);
        self.bindings = bindings;
        self.keybinding_problems = problems;
    }

    /// The settings changed: theme, the local core's policy, defaults.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let settings = cx.global::<Settings>().value.clone();
        self.default_provider = settings.default_provider;
        self.backend(LOCAL).configure(settings.core());
        let light = settings.theme == ThemeMode::Light;
        if light != theme::is_light() {
            theme::set_light(light);
            // Cached elements keep their colors: rebuild them.
            self.sidebar.update(cx, |_, cx| cx.notify());
            if let Some((env, id)) = self.selected(cx) {
                self.backend(env).open_thread(id);
            }
            cx.refresh_windows();
        }
        cx.notify();
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::settings::AppSettings),
    ) {
        let settings = cx.global_mut::<Settings>();
        f(&mut settings.value);
        if let Err(err) = settings.save() {
            self.notice = Some(err.into());
        }
        self.apply_settings(cx);
        if let Some(view) = &self.settings_view {
            view.update(cx, |_, cx| cx.notify());
        }
    }

    fn context_names(&self, cx: &App) -> HashSet<&'static str> {
        let mut names = HashSet::new();
        if let Some(thread) = self.selected_thread(cx) {
            names.insert("threadOpen");
            if matches!(thread.status, ThreadStatus::Running | ThreadStatus::Waiting) {
                names.insert("busy");
            }
        }
        if self.selected_env(cx) != LOCAL {
            names.insert("remote");
        }
        if self.terminal.is_some() {
            names.insert("terminalOpen");
        }
        if self.palette.is_some() {
            names.insert("paletteOpen");
        }
        names.insert(match self.view {
            View::Chat => "view.chat",
            View::Diff => "view.diff",
            View::Files => "view.files",
            View::Pr => "view.pr",
            View::Inbox => "view.inbox",
            View::Settings => "view.settings",
        });
        names
    }

    fn on_cmd(&mut self, cmd: &Cmd, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(when) = &cmd.when
            && !when.eval(&self.context_names(cx))
        {
            cx.propagate();
            return;
        }
        self.run_command(&cmd.id, window, cx);
    }

    pub fn run_command(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if std::env::var_os("BLONGO_TRACE_COMMANDS").is_some() {
            // tools/e2e-gui-phase4.sh checks which command a key ran.
            eprintln!("blongo: command {id}");
        }
        match id {
            "palette.commands" => self.open_palette(false, window, cx),
            "palette.files" => self.open_palette(true, window, cx),
            "thread.new" => {
                let env = self.selected_env(cx);
                let project = self.selected_thread(cx).map(|t| t.project_id).or(self
                    .sidebar
                    .read(cx)
                    .envs[env]
                    .projects
                    .first()
                    .map(|p| p.id));
                if let Some(project_id) = project {
                    self.new_thread(env, project_id, false, cx);
                }
            }
            "thread.fork" => self.fork(None, cx),
            "pr.link" => {
                if self.selected(cx).is_some() {
                    self.show_palette(
                        crate::palette::Mode::Prompt {
                            id: "pr.link",
                            placeholder: "Pull request URL, owner/name#12 or #12",
                            hint: "Enter links this thread to the pull request.",
                        },
                        window,
                        cx,
                    );
                }
            }
            "pr.open" => {
                if let Some(pr) = self.selected_thread(cx).and_then(|t| t.pr) {
                    open_pr(&pr, cx);
                }
            }
            "pr.refresh" => self.refresh_pr(cx),
            "pr.unlink" => {
                let linked = self.selected_thread(cx).is_some_and(|t| t.pr.is_some());
                if let Some((_, thread_id)) = self.selected(cx).filter(|_| linked) {
                    self.dispatch_selected(Command::ThreadUnlinkPr { thread_id }, cx);
                }
            }
            "thread.undo" => self.undo_last(cx),
            "thread.stop" => self.stop(cx),
            "terminal.toggle" => self.toggle_terminal(window, cx),
            "view.chat" => self.set_view(View::Chat, cx),
            "view.diff" => self.set_view(View::Diff, cx),
            "view.files" => self.set_view(View::Files, cx),
            "view.pr" | "pr.create" => self.set_view(View::Pr, cx),
            "view.inbox" => self.set_view(View::Inbox, cx),
            "view.settings" => self.set_view(View::Settings, cx),
            "model.next" => self.next_model(cx),
            "theme.toggle" => self.update_settings(cx, |s| {
                s.theme = match s.theme {
                    ThemeMode::Dark => ThemeMode::Light,
                    ThemeMode::Light => ThemeMode::Dark,
                }
            }),
            "approval.toggle" => self.update_settings(cx, |s| {
                s.approval = match s.approval {
                    blongo_protocol::client::ApprovalPolicy::Ask => {
                        blongo_protocol::client::ApprovalPolicy::AutoApprove
                    }
                    blongo_protocol::client::ApprovalPolicy::AutoApprove => {
                        blongo_protocol::client::ApprovalPolicy::Ask
                    }
                }
            }),
            "git.refresh" => {
                if let Some(files) = &self.files {
                    files.update(cx, |f, cx| f.reload(cx));
                }
            }
            "keybindings.open" => {
                self.notice = Some(
                    format!(
                        "Keybindings file: {}",
                        crate::settings::keybindings_path().display()
                    )
                    .into(),
                );
                cx.notify();
            }
            other => {
                if let Some(provider) = other
                    .strip_prefix("provider.")
                    .and_then(ProviderKind::parse)
                {
                    self.set_provider(provider, None, cx);
                }
            }
        }
    }

    fn open_palette(&mut self, files: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            window.focus(&self.composer.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        let mode = if files {
            let Some((env, thread_id)) = self.selected(cx) else {
                return;
            };
            crate::palette::Mode::Files {
                backend: self.backend(env).clone(),
                thread_id,
            }
        } else {
            crate::palette::commands(&self.bindings)
        };
        self.show_palette(mode, window, cx);
    }

    fn on_prompt(&mut self, id: &str, text: &str, cx: &mut Context<Self>) {
        if id == "pr.link"
            && let Some((_, thread_id)) = self.selected(cx)
        {
            self.dispatch_selected(
                Command::ThreadLinkPr {
                    thread_id,
                    pr: text.to_owned(),
                },
                cx,
            );
        }
    }

    /// Ask the environment to check the open thread's pull request now.
    fn refresh_pr(&mut self, cx: &mut Context<Self>) {
        let Some((env, thread_id)) = self.selected(cx) else {
            return;
        };
        let backend = self.backend(env).clone();
        crate::query::ask(
            &backend,
            Query::PrRefresh { thread_id },
            cx.weak_entity(),
            cx,
            |this, result, cx| {
                match result {
                    Ok(QueryReply::Done(text)) => {
                        this.notice = None;
                        this.info = Some(text.into());
                    }
                    Ok(_) => return,
                    Err(err) => {
                        this.info = None;
                        this.notice = Some(err.into());
                    }
                }
                cx.notify();
            },
        );
    }

    fn show_palette(
        &mut self,
        mode: crate::palette::Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let palette = cx.new(|cx| Palette::new(mode, window, cx));
        self.view_subscriptions.push(cx.subscribe_in(
            &palette,
            window,
            |this, _, event: &PaletteEvent, window, cx| {
                this.palette = None;
                match event {
                    PaletteEvent::Run(id) => {
                        window.focus(&this.composer.focus_handle(cx), cx);
                        this.run_command(id, window, cx);
                    }
                    PaletteEvent::OpenFile(path) => {
                        this.open_file = Some(path.clone());
                        this.set_view(View::Files, cx);
                    }
                    PaletteEvent::Submit(id, text) => {
                        window.focus(&this.composer.focus_handle(cx), cx);
                        this.on_prompt(id, text, cx);
                    }
                    PaletteEvent::Dismiss => {
                        window.focus(&this.composer.focus_handle(cx), cx);
                    }
                }
                cx.notify();
            },
        ));
        self.palette = Some(palette);
        cx.notify();
    }

    /// Fetch the PR tab's detail again when it shows `thread_id`.
    fn reload_pr_view(&self, env: EnvId, thread_id: ThreadId, cx: &mut Context<Self>) {
        if let Some(pr) = &self.pr_view
            && self.pr_view_for == Some((env, thread_id))
        {
            pr.update(cx, |p, cx| p.reload(cx));
        }
    }

    /// A desktop notification, as the settings allow.
    fn notify(&self, body: &str, title: &str, cx: &App) {
        let show = match cx.global::<Settings>().value.notifications {
            NotifyMode::Off => false,
            NotifyMode::Unfocused => !self.window_active,
            NotifyMode::Always => true,
        };
        if show && self.auto_prompt.is_none() {
            crate::notify::send(title, body);
        }
    }

    fn on_link(&mut self, url: &str, cx: &mut Context<Self>) {
        match crate::deeplink::parse(url) {
            Ok(link) if self.envs[LOCAL].loaded => self.open_link(link, cx),
            Ok(link) => self.links.push(link),
            Err(err) => {
                self.notice = Some(err.into());
                cx.notify();
            }
        }
    }

    fn open_link(&mut self, link: Link, cx: &mut Context<Self>) {
        match link {
            Link::Thread(id) => {
                if self.sidebar.read(cx).envs[LOCAL].thread(id).is_some() {
                    self.select(LOCAL, id, cx);
                    self.view = View::Chat;
                } else {
                    self.notice = Some(format!("No thread {id} here").into());
                }
            }
            // Links only navigate: a known project is selected (its newest
            // thread), an unknown folder is added only after the user says
            // so. No thread is created by a link.
            Link::Project(path) => {
                let wanted = canonical(&path);
                let env = &self.sidebar.read(cx).envs[LOCAL];
                let existing = env
                    .projects
                    .iter()
                    .find(|p| canonical(&p.path) == wanted)
                    .map(|p| p.id);
                match existing {
                    Some(project_id) => {
                        let newest = env
                            .threads
                            .iter()
                            .find(|t| t.project_id == project_id && !t.archived)
                            .map(|t| t.id);
                        match newest {
                            Some(thread_id) => {
                                self.select(LOCAL, thread_id, cx);
                                self.view = View::Chat;
                            }
                            None => {
                                self.notice = Some(
                                    format!("{path} has no threads yet: start one with + New")
                                        .into(),
                                )
                            }
                        }
                    }
                    // A question already showing keeps its folder: a link
                    // arriving under the user's cursor cannot swap the path
                    // just before "Add project" is clicked.
                    None if self.confirm_link_project.is_some() => {
                        self.notice = Some(
                            format!(
                                "Ignored a blongo:// link to {path}: answer the open question first"
                            )
                            .into(),
                        )
                    }
                    None => self.confirm_link_project = Some(path),
                }
            }
            Link::Settings => self.view = View::Settings,
            Link::Inbox => self.view = View::Inbox,
        }
        cx.notify();
    }

    /// The question for a `blongo://project` link to a new folder.
    fn render_link_confirm(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let path = self.confirm_link_project.clone()?;
        Some(
            div()
                .absolute()
                .top(px(60.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("link-project-confirm")
                        .occlude()
                        .w(px(520.))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::surface())
                        .text_sm()
                        .child(div().child("A blongo:// link asks to add this folder as a project:"))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme::code_bg())
                                .text_xs()
                                .child(path),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::text_muted())
                                .child("Only add folders you trust: agents you start there can read and change them."),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap_2()
                                .child(button(
                                    "link-project-cancel".into(),
                                    "Cancel",
                                    theme::surface_hover(),
                                    theme::text(),
                                    cx.listener(|this, _, _, cx| {
                                        this.confirm_link_project = None;
                                        cx.notify();
                                    }),
                                ))
                                .child(button(
                                    "link-project-add".into(),
                                    "Add project",
                                    theme::accent_bg(),
                                    theme::text(),
                                    cx.listener(|this, _, _, cx| this.add_linked_project(cx)),
                                )),
                        ),
                ),
        )
    }

    /// The user confirmed a `blongo://project` link: add the folder (no
    /// thread is started; the user does that).
    fn add_linked_project(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.confirm_link_project.take() else {
            return;
        };
        let envelope = CommandEnvelope::new(Command::ProjectCreate {
            project_id: ProjectId::new(),
            name: String::new(),
            path,
        });
        self.backend(LOCAL).dispatch(envelope);
        self.view = View::Chat;
        cx.notify();
    }

    fn turn_number(&self, run_id: RunId) -> usize {
        self.runs
            .iter()
            .filter(|(_, s)| {
                !matches!(
                    s,
                    RunStatus::Queued | RunStatus::Cancelled | RunStatus::RolledBack
                )
            })
            .position(|(id, _)| *id == run_id)
            .map_or(0, |i| i + 1)
    }

    fn usage_label(&self) -> Option<SharedString> {
        if self.usage.is_empty() {
            return None;
        }
        let mut total = Usage::default();
        for (_, u) in &self.usage {
            total.add(u);
        }
        let mut text = format!("{} tokens", compact(total.total_tokens()));
        if let Some(micros) = total.cost_micros {
            text.push_str(&format!(" · ${:.2}", micros as f64 / 1_000_000.));
        }
        Some(text.into())
    }

    fn render_scope_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let turns: Vec<RunId> = self
            .runs
            .iter()
            .filter(|(_, s)| {
                !matches!(
                    s,
                    RunStatus::Queued | RunStatus::Cancelled | RunStatus::RolledBack
                )
            })
            .map(|(id, _)| *id)
            .collect();
        let first = turns.len().saturating_sub(8);
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .child(
                header_action("scope-thread", "All changes")
                    .when(self.diff_scope == DiffScope::Thread, |d| {
                        d.bg(theme::surface_hover()).text_color(theme::text())
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.diff_scope = DiffScope::Thread;
                        cx.notify();
                    })),
            )
            .children(
                turns
                    .into_iter()
                    .enumerate()
                    .skip(first)
                    .map(|(ix, run_id)| {
                        let active = self.diff_scope == DiffScope::Turn { run_id };
                        div()
                            .id(SharedString::from(format!("scope-turn-{}", ix + 1)))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .text_color(theme::text_muted())
                            .when(active, |d| {
                                d.bg(theme::surface_hover()).text_color(theme::text())
                            })
                            .hover(|d| d.bg(theme::surface_hover()))
                            .cursor_pointer()
                            .child(format!("Turn {}", ix + 1))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.diff_scope = DiffScope::Turn { run_id };
                                cx.notify();
                            }))
                    }),
            )
    }

    /// Settings and the inbox: a full-width page with a way back.
    fn render_page(
        &self,
        title: &'static str,
        body: gpui::AnyElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .flex_1()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(header_action("page-back", "← Back").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.view = View::Chat;
                            this.pending_focus = Some(this.composer.focus_handle(cx));
                            cx.notify();
                        },
                    )))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when_some(self.notice.clone(), |d, n| {
                        d.child(div().text_xs().text_color(theme::danger()).child(n))
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
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
        match self.view {
            View::Settings => {
                let body = self.ensure_settings(window, cx).into_any_element();
                return self.render_page("Settings", body, cx);
            }
            View::Inbox => {
                let body = self.ensure_inbox(cx).into_any_element();
                return self.render_page("Review inbox", body, cx);
            }
            _ => {}
        }
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

        let usage = self.usage_label();
        let pr_chip = thread.pr.as_ref().map(|pr| {
            let badge = crate::pr::badge(&thread).unwrap_or(blongo_protocol::PrBadge::Open);
            let error = thread.pr_status.as_ref().and_then(|s| s.error.clone());
            let title = thread
                .pr_status
                .as_ref()
                .map(|s| s.title.clone())
                .filter(|t| !t.is_empty());
            let link = pr.clone();
            let label = match &title {
                Some(t) => format!("#{} {t}", pr.number),
                None => format!("#{}", pr.number),
            };
            div()
                .id("pr-chip")
                .flex()
                .items_center()
                .gap_1()
                .max_w(px(320.))
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .hover(|d| d.bg(theme::surface_hover()))
                .on_click(cx.listener(move |_, _, _, cx| open_pr(&link, cx)))
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_color(theme::link())
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_color(crate::pr::color(badge))
                        .child(badge.label()),
                )
                .when_some(error, |d, e| {
                    d.child(
                        div()
                            .max_w(px(200.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(theme::warning())
                            .child(SharedString::from(format!("⚠ {e}"))),
                    )
                })
        });
        let tab = |id: &'static str, label: &'static str, view: View, current: View| {
            header_action(id, label)
                .when(view == current, |d| {
                    d.bg(theme::surface_hover()).text_color(theme::text())
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
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
                    .whitespace_nowrap()
                    .child(SharedString::from(thread.title.clone())),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(tab("tab-chat", "Chat", View::Chat, self.view))
                    .child(tab("tab-diff", "Changes", View::Diff, self.view))
                    .child(tab("tab-files", "Files", View::Files, self.view))
                    .when(thread.pr.is_some() || self.view == View::Pr, |d| {
                        d.child(tab("tab-pr", "PR", View::Pr, self.view))
                    }),
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
            .when_some(pr_chip, |d, chip| d.child(chip))
            .when(
                thread.pr.is_none() && crate::pr::owns_branch(&thread) && self.view != View::Pr,
                |d| {
                    d.child(header_action("create-pr", "Create PR").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.run_command("pr.create", window, cx)
                        }),
                    ))
                },
            )
            .when(thread.pr.is_none(), |d| {
                d.child(header_action("link-pr", "Link PR").on_click(
                    cx.listener(|this, _, window, cx| this.run_command("pr.link", window, cx)),
                ))
            })
            .when_some(usage, |d, usage| {
                d.child(
                    div()
                        .id("usage")
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child(usage),
                )
            })
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

        let timeline = match self.view {
            View::Diff => {
                let diff = self.ensure_diff(window, cx);
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.render_scope_bar(cx))
                    .child(div().flex_1().min_h_0().child(diff))
            }
            View::Files => {
                let files = self.ensure_files(window, cx);
                div().flex_1().min_h_0().child(files)
            }
            View::Pr if thread.pr.is_some() => {
                let pr = self.ensure_pr(cx);
                div().flex_1().min_h_0().child(pr)
            }
            View::Pr => {
                let form = self.ensure_pr_create(cx);
                div().flex_1().min_h_0().child(form)
            }
            _ => match &self.timeline {
                Some(t) => div().flex_1().min_h_0().child(t.clone()),
                None => div().flex_1(),
            },
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
            _ => cx.global::<Settings>().value.approval.label().into(),
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
                .when_some(self.info.clone(), |d, info| {
                    d.child(div().text_xs().text_color(theme::text_muted()).child(info))
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
        // However the PR tab was left (a link, a vanished thread), its
        // detail goes with it.
        if (self.pr_view.is_some() || self.pr_create.is_some())
            && (self.view != View::Pr || self.selected(cx).is_none())
        {
            self.pr_view = None;
            self.pr_create = None;
            self.pr_view_for = None;
        }
        let mut sidebar_style = StyleRefinement::default();
        sidebar_style.size.width = Some(px(SIDEBAR_WIDTH).into());
        sidebar_style.size.height = Some(gpui::relative(1.).into());
        sidebar_style.flex_shrink = Some(0.);
        div()
            .key_context(crate::keymap::CONTEXT)
            .on_action(cx.listener(Self::on_cmd))
            .relative()
            .flex()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text())
            // Cached: re-rendered only when the sidebar itself is notified.
            .child(self.sidebar.clone().cached(sidebar_style))
            .child(self.render_main(window, cx))
            .when_some(self.render_link_confirm(cx), |d, confirm| d.child(confirm))
            .when_some(self.palette.clone(), |d, palette| {
                d.child(
                    div()
                        .absolute()
                        .top(px(60.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(palette),
                )
            })
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

/// Give memory freed by a closed view back to the OS: glibc keeps freed
/// heap pages otherwise. Once the view's entity is gone (next frame or
/// so), on a background thread.
fn trim_heap_soon(cx: &mut Context<Shell>) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    cx.spawn(async move |_, cx| {
        cx.background_executor()
            .timer(Duration::from_millis(500))
            .await;
        cx.background_executor()
            .spawn(async {
                // SAFETY: malloc_trim has no preconditions.
                unsafe { libc::malloc_trim(0) };
            })
            .await;
    })
    .detach();
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = cx;
}

/// "Completed", "Failed", … for notifications.
fn status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "Completed",
        RunStatus::Failed => "Failed",
        RunStatus::Interrupted => "Stopped",
        _ => "Finished",
    }
}

/// 1234 → "1.2k", 3_400_000 → "3.4M".
fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.),
        _ => format!("{:.1}M", n as f64 / 1_000_000.),
    }
}

/// A timestamp as local "YYYY-MM-DD HH:MM".
pub fn local_time(t: Timestamp) -> String {
    blongo_core::cron::format_local(t.0)
}

fn canonical(path: &str) -> PathBuf {
    std::path::Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(path))
}

/// Open a pull request's page. The URL came from GitHub (or a remote
/// server): only an `https` page on the pull request's own host opens.
fn open_pr(pr: &blongo_protocol::PrLink, cx: &mut App) {
    if let Some(url) = crate::pr::safe_url(pr) {
        cx.open_url(&url);
    }
}
