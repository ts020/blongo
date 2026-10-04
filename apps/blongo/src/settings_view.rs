//! The settings screen: providers and models, approvals, theme,
//! notifications, review-inbox tokens, updates, keybindings, scheduled
//! runs, data locations and environments. Changes are saved at once
//! (`settings.json`, `forge.json`, both 0600); what needs the shell (the
//! core, schedules, environments) goes out as [`SettingsEvent`]s.

use std::path::PathBuf;

use blongo_client::forge::{self, ForgeKind, TokenFile};
use blongo_client::update;
use blongo_protocol::client::ApprovalPolicy;
use blongo_protocol::{BaseBranch, ForgeSettings, ProjectId, ProviderKind, Schedule, ScheduleId};
use gpui::{Context, Entity, FontWeight, SharedString, Subscription, Window, div, prelude::*, px};

use crate::input::{InputEvent, TextInput};
use crate::settings::{AcpCommand, NotifyMode, Settings, ThemeMode};
use crate::theme;
use crate::timeline::button;

pub enum SettingsEvent {
    /// The settings changed (already saved).
    Changed,
    CreateSchedule {
        cron: String,
        prompt: String,
        in_open_thread: bool,
    },
    ToggleSchedule(ScheduleId, bool),
    RunSchedule(ScheduleId),
    DeleteSchedule(ScheduleId),
    RemoveEnvironment(String),
    /// The open thread's project's GitHub settings.
    SetForge(ProjectId, ForgeSettings),
    ReloadKeybindings,
    TestNotification,
}

impl gpui::EventEmitter<SettingsEvent> for SettingsView {}

/// What the shell shows here and keeps up to date.
#[derive(Default)]
pub struct ShellInfo {
    pub schedules: Vec<Schedule>,
    /// (name, target) of the remote environments.
    pub environments: Vec<(String, String)>,
    /// The open thread's project, where a new schedule goes.
    pub schedule_target: Option<SharedString>,
    pub keybinding_problems: Vec<String>,
    pub data_dir: PathBuf,
    pub mcp: bool,
    /// The open thread's project (id, name) and its GitHub settings.
    pub forge: Option<(ProjectId, SharedString, ForgeSettings)>,
}

pub struct SettingsView {
    pub info: ShellInfo,
    model_inputs: Vec<(ProviderKind, Entity<TextInput>)>,
    acp_input: Entity<TextInput>,
    token_inputs: Vec<(ForgeKind, Entity<TextInput>, Entity<TextInput>)>,
    cron_input: Entity<TextInput>,
    prompt_input: Entity<TextInput>,
    base_input: Entity<TextInput>,
    prefix_input: Entity<TextInput>,
    /// The project (and settings) the GitHub inputs were filled from.
    forge_shown: Option<(ProjectId, ForgeSettings)>,
    in_open_thread: bool,
    forges: TokenFile,
    update_status: Option<(bool, SharedString)>,
    available: Option<update::Manifest>,
    message: Option<(bool, SharedString)>,
    pub tab: Tab,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(info: ShellInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = cx.global::<Settings>().value.clone();
        let mut subscriptions = Vec::new();
        let mut model_inputs = Vec::new();
        for provider in ProviderKind::ALL {
            let input = cx.new(|cx| {
                let mut i = TextInput::new("default", false, cx);
                if let Some(m) = settings.default_models.get(provider.id()) {
                    i.set_text(m, cx);
                }
                i
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, input, event, _, cx| {
                    if let InputEvent::Submit = event {
                        let model = input.read(cx).text().trim().to_owned();
                        this.update_settings(cx, |s| {
                            if model.is_empty() {
                                s.default_models.remove(provider.id());
                            } else {
                                s.default_models.insert(provider.id().into(), model.clone());
                            }
                        });
                    }
                },
            ));
            model_inputs.push((provider, input));
        }
        let acp_input = cx.new(|cx| {
            let mut i = TextInput::new("opencode acp", false, cx);
            let acp = &settings.acp;
            if !acp.executable.is_empty() {
                let mut line = acp.executable.clone();
                for a in &acp.args {
                    line.push(' ');
                    line.push_str(a);
                }
                i.set_text(&line, cx);
            }
            i
        });
        subscriptions.push(
            cx.subscribe_in(&acp_input, window, |this, input, event, _, cx| {
                if let InputEvent::Submit = event {
                    let mut words = input.read(cx).text().split_whitespace().map(str::to_owned);
                    let acp = AcpCommand {
                        executable: words.next().unwrap_or_default(),
                        args: words.collect(),
                    };
                    this.update_settings(cx, |s| s.acp = acp.clone());
                    this.message = Some((
                        true,
                        "Saved. The ACP agent's command applies after a restart.".into(),
                    ));
                }
            }),
        );
        let forges = TokenFile::load(&forge::default_path()).unwrap_or_default();
        let mut token_inputs = Vec::new();
        for kind in [ForgeKind::GitHub, ForgeKind::GitLab] {
            let token =
                cx.new(|cx| TextInput::new("token (Enter to save; empty removes)", false, cx));
            let api = cx.new(|cx| {
                let mut i = TextInput::new(kind.default_api(), false, cx);
                if let Some(f) = forges.forges.iter().find(|f| f.kind == kind)
                    && f.api != kind.default_api()
                {
                    i.set_text(&f.api, cx);
                }
                i
            });
            for input in [&token, &api] {
                subscriptions.push(
                    cx.subscribe_in(input, window, move |this, _, event, _, cx| {
                        if let InputEvent::Submit = event {
                            this.save_token(kind, cx);
                        }
                    }),
                );
            }
            token_inputs.push((kind, token, api));
        }
        let cron_input = cx.new(|cx| TextInput::new("0 9 * * 1-5", false, cx));
        let prompt_input = cx.new(|cx| TextInput::new("What should the agent do?", false, cx));
        subscriptions.push(
            cx.subscribe_in(&prompt_input, window, |this, _, event, _, cx| {
                if let InputEvent::Submit = event {
                    this.create_schedule(cx);
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(&cron_input, window, |this, _, event, _, cx| {
                if let InputEvent::Submit = event {
                    this.create_schedule(cx);
                }
            }),
        );
        let base_input = cx.new(|cx| TextInput::new("develop (Enter saves)", false, cx));
        let prefix_input = cx.new(|cx| TextInput::new("blongo/ (Enter saves)", false, cx));
        subscriptions.push(
            cx.subscribe_in(&base_input, window, |this, input, event, _, cx| {
                if let InputEvent::Submit = event {
                    let name = input.read(cx).text().trim().to_owned();
                    this.set_forge(cx, |f| {
                        f.base_branch = if name.is_empty() {
                            BaseBranch::GithubDefault
                        } else {
                            BaseBranch::Custom { name: name.clone() }
                        }
                    });
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(&prefix_input, window, |this, input, event, _, cx| {
                if let InputEvent::Submit = event {
                    let prefix = input.read(cx).text().trim().to_owned();
                    this.set_forge(cx, |f| f.branch_prefix = prefix.clone());
                }
            }),
        );
        let load_error = cx.global::<Settings>().load_error.clone();
        Self {
            info,
            model_inputs,
            acp_input,
            token_inputs,
            cron_input,
            prompt_input,
            base_input,
            prefix_input,
            forge_shown: None,
            in_open_thread: false,
            forges,
            update_status: None,
            available: None,
            message: load_error.map(|e| (false, format!("{e} (defaults in use)").into())),
            tab: Tab::General,
            _subscriptions: subscriptions,
        }
    }

    fn update_settings(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::settings::AppSettings),
    ) {
        let settings = cx.global_mut::<Settings>();
        f(&mut settings.value);
        self.message = match settings.save() {
            Ok(()) => Some((true, "Saved".into())),
            Err(err) => Some((false, err.into())),
        };
        cx.emit(SettingsEvent::Changed);
        cx.notify();
    }

    fn save_token(&mut self, kind: ForgeKind, cx: &mut Context<Self>) {
        let Some((_, token, api)) = self.token_inputs.iter().find(|(k, _, _)| *k == kind) else {
            return;
        };
        let token_text = token.read(cx).text().trim().to_owned();
        let api_text = api.read(cx).text().trim().to_owned();
        let existing = self.forges.get(kind).map(|f| f.token.clone());
        let token_value = if token_text.is_empty() {
            // Only the API changed: keep the token unless the API is
            // cleared too.
            match (&existing, api_text.is_empty()) {
                (Some(t), false) => t.clone(),
                _ => String::new(),
            }
        } else {
            token_text
        };
        if let Err(err) = blongo_client::http::check_url(if api_text.is_empty() {
            kind.default_api()
        } else {
            &api_text
        }) {
            self.message = Some((false, err.into()));
            cx.notify();
            return;
        }
        self.forges.set(kind, Some(api_text), token_value);
        let path = forge::default_path();
        if let Some(dir) = path.parent() {
            let _ = blongo_client::secret::private_dir(dir);
        }
        self.message = match self.forges.save(&path) {
            Ok(()) => Some((true, format!("{} settings saved", kind.label()).into())),
            Err(err) => Some((false, err.into())),
        };
        token.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    fn create_schedule(&mut self, cx: &mut Context<Self>) {
        let cron = self.cron_input.read(cx).text().trim().to_owned();
        let prompt = self.prompt_input.read(cx).text().trim().to_owned();
        if cron.is_empty() || prompt.is_empty() {
            self.message = Some((false, "A schedule needs a cron line and a prompt".into()));
            cx.notify();
            return;
        }
        if self.info.schedule_target.is_none() {
            self.message = Some((
                false,
                "Open a thread first: the schedule goes to its project".into(),
            ));
            cx.notify();
            return;
        }
        cx.emit(SettingsEvent::CreateSchedule {
            cron,
            prompt,
            in_open_thread: self.in_open_thread,
        });
    }

    /// Change the open project's GitHub settings (the core checks them;
    /// the shell shows a refusal).
    fn set_forge(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ForgeSettings)) {
        let Some((project_id, _, settings)) = &self.info.forge else {
            self.message = Some((
                false,
                "Open a thread first: settings are per project".into(),
            ));
            return cx.notify();
        };
        let mut settings = settings.clone();
        f(&mut settings);
        if settings
            == self
                .info
                .forge
                .as_ref()
                .map(|(_, _, s)| s.clone())
                .unwrap_or_default()
        {
            self.message = Some((true, "No change".into()));
            return cx.notify();
        }
        self.message = Some((true, "Saving…".into()));
        cx.emit(SettingsEvent::SetForge(*project_id, settings));
        cx.notify();
    }

    /// The core refused the GitHub settings: say why and show the kept
    /// ones again.
    pub fn forge_refused(&mut self, reason: String, cx: &mut Context<Self>) {
        self.forge_shown = None;
        self.message = Some((false, reason.into()));
        cx.notify();
    }

    /// Fill the GitHub inputs when the project or its settings changed.
    fn sync_forge(&mut self, cx: &mut Context<Self>) {
        let now = self
            .info
            .forge
            .as_ref()
            .map(|(id, _, settings)| (*id, settings.clone()));
        if now == self.forge_shown {
            return;
        }
        if let (Some((was, _)), Some((id, _))) = (&self.forge_shown, &now)
            && was == id
        {
            self.message = Some((true, "Saved".into()));
        }
        let (base, prefix) = match &now {
            Some((_, f)) => (
                match &f.base_branch {
                    BaseBranch::Custom { name } => name.clone(),
                    BaseBranch::GithubDefault => String::new(),
                },
                f.branch_prefix.clone(),
            ),
            None => (String::new(), String::new()),
        };
        self.base_input.update(cx, |i, cx| i.set_text(&base, cx));
        self.prefix_input
            .update(cx, |i, cx| i.set_text(&prefix, cx));
        self.forge_shown = now;
    }

    /// The shell accepted a new schedule.
    pub fn schedule_created(&mut self, cx: &mut Context<Self>) {
        self.cron_input.update(cx, |i, cx| i.set_text("", cx));
        self.prompt_input.update(cx, |i, cx| i.set_text("", cx));
        self.message = Some((true, "Schedule created".into()));
        cx.notify();
    }

    pub fn set_message(&mut self, ok: bool, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.message = Some((ok, text.into()));
        cx.notify();
    }

    fn check_update(&mut self, cx: &mut Context<Self>) {
        let Some((url, key)) = update::configured() else {
            self.update_status = Some((
                false,
                "No signed update channel is configured in this build.".into(),
            ));
            cx.notify();
            return;
        };
        self.update_status = Some((true, "Checking…".into()));
        let current = env!("CARGO_PKG_VERSION");
        let task = blongo_client::net::handle()
            .spawn(async move { update::check(&url, &key, current).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                this.update_status = Some(match result {
                    Ok(update::Status::UpToDate { latest }) => (
                        true,
                        format!("Up to date ({current}; latest {latest})").into(),
                    ),
                    Ok(update::Status::Available(m)) => {
                        let text = format!("Version {} is available: {}", m.version, m.notes);
                        this.available = Some(m);
                        (true, text.into())
                    }
                    Err(err) => (false, err.into()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn download_update(&mut self, cx: &mut Context<Self>) {
        let Some(manifest) = self.available.clone() else {
            return;
        };
        let dir = self.info.data_dir.join("updates");
        self.update_status = Some((true, "Downloading…".into()));
        let task = blongo_client::net::handle()
            .spawn(async move { update::download(&manifest, &dir).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |this, cx| {
                this.update_status = Some(match result {
                    Ok(path) => (
                        true,
                        format!(
                            "Verified and saved to {} (install it to update)",
                            path.display()
                        )
                        .into(),
                    ),
                    Err(err) => (false, err.into()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .pb_4()
        .mb_2()
        .border_b_1()
        .border_color(theme::border())
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
}

fn label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .w(px(170.))
        .flex_shrink_0()
        .text_xs()
        .text_color(theme::text_muted())
        .child(text.into())
}

fn field(input: &Entity<TextInput>) -> gpui::Div {
    div()
        .flex_1()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(theme::code_bg())
        .text_sm()
        .child(input.clone())
}

fn row() -> gpui::Div {
    div().flex().items_center().gap_2()
}

fn choice(id: SharedString, text: &'static str, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .bg(if active {
            theme::accent_bg()
        } else {
            theme::surface_hover()
        })
        .text_color(if active {
            theme::text()
        } else {
            theme::text_muted()
        })
        .child(text)
}

fn mono(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_xs()
        .font_family(theme::MONO)
        .text_color(theme::text())
        .child(text.into())
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_forge(cx);
        let s = cx.global::<Settings>().value.clone();
        let settings_path = cx.global::<Settings>().path.clone();

        // Providers and models.
        let mut providers =
            section("Providers & models").child(row().child(label("New threads use")).children(
                ProviderKind::ALL.map(|p| {
                    choice(
                        format!("default-{}", p.id()).into(),
                        p.label(),
                        s.default_provider == p,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_settings(cx, |s| s.default_provider = p)
                    }))
                }),
            ));
        for (provider, input) in &self.model_inputs {
            providers = providers.child(
                row()
                    .child(label(format!("{} default model", provider.label())))
                    .child(field(input)),
            );
        }
        providers = providers
            .child(
                row()
                    .child(label("ACP agent command"))
                    .child(field(&self.acp_input)),
            )
            .child(div().text_xs().text_color(theme::text_faint()).child(
                "Any agent that speaks the Agent Client Protocol (e.g. `opencode acp`). \
                 Enter saves; applies after a restart. Model ids are the provider's own.",
            ));

        let approvals = section("Approvals").child(
            row()
                .child(label("Agent requests"))
                .child(
                    choice(
                        "approval-ask".into(),
                        "Ask me",
                        s.approval == ApprovalPolicy::Ask,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_settings(cx, |s| s.approval = ApprovalPolicy::Ask)
                    })),
                )
                .child(
                    choice(
                        "approval-auto".into(),
                        "Auto-approve",
                        s.approval == ApprovalPolicy::AutoApprove,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_settings(cx, |s| s.approval = ApprovalPolicy::AutoApprove)
                    })),
                ),
        );

        let appearance = section("Appearance").child(
            row()
                .child(label("Theme"))
                .child(
                    choice("theme-dark".into(), "Dark", s.theme == ThemeMode::Dark).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.update_settings(cx, |s| s.theme = ThemeMode::Dark)
                        }),
                    ),
                )
                .child(
                    choice("theme-light".into(), "Light", s.theme == ThemeMode::Light).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.update_settings(cx, |s| s.theme = ThemeMode::Light)
                        }),
                    ),
                ),
        );

        let notifications = section("Notifications").child(
            row()
                .child(label("Run finished / approval needed"))
                .children(
                    [NotifyMode::Off, NotifyMode::Unfocused, NotifyMode::Always].map(|m| {
                        choice(
                            format!("notify-{m:?}").into(),
                            match m {
                                NotifyMode::Off => "Off",
                                NotifyMode::Unfocused => "In background",
                                NotifyMode::Always => "Always",
                            },
                            s.notifications == m,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_settings(cx, |s| s.notifications = m)
                        }))
                    }),
                )
                .child(
                    choice("notify-test".into(), "Test", false).on_click(
                        cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::TestNotification)),
                    ),
                ),
        );

        let mut inbox = section("Review inbox (GitHub / GitLab)");
        for (kind, token, api) in &self.token_inputs {
            let state: SharedString = match self.forges.get(*kind) {
                Some(f) => {
                    let tail: String = f
                        .token
                        .chars()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!("token set (…{tail}) · {}", f.api).into()
                }
                None => "no token".into(),
            };
            inbox = inbox
                .child(
                    row()
                        .child(label(format!("{} token", kind.label())))
                        .child(field(token))
                        .child(div().text_xs().text_color(theme::text_faint()).child(state)),
                )
                .child(
                    row()
                        .child(label(format!("{} API", kind.label())))
                        .child(field(api)),
                );
        }
        inbox = inbox.child(div().text_xs().text_color(theme::text_faint()).child(
            "Tokens are kept in forge.json (owner-only) next to settings.json, never in the \
             environment. Read access to pull requests and permission to comment are enough.",
        ));

        let updates = section("Updates")
            .child(
                row()
                    .child(label(format!("Version {}", env!("CARGO_PKG_VERSION"))))
                    .child(
                        choice("update-check".into(), "Check now", false)
                            .on_click(cx.listener(|this, _, _, cx| this.check_update(cx))),
                    )
                    .when(self.available.is_some(), |d| {
                        d.child(
                            choice("update-download".into(), "Download", false)
                                .on_click(cx.listener(|this, _, _, cx| this.download_update(cx))),
                        )
                    })
                    .child(
                        choice("update-auto".into(), "Check at start", s.check_updates).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.update_settings(cx, |s| s.check_updates = !s.check_updates)
                            }),
                        ),
                    ),
            )
            .when_some(self.update_status.clone(), |d, (ok, t)| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(if ok {
                            theme::text_muted()
                        } else {
                            theme::danger()
                        })
                        .child(t),
                )
            });

        let keybindings = section("Keybindings")
            .child(
                row()
                    .child(label("User file"))
                    .child(mono(
                        crate::settings::keybindings_path().display().to_string(),
                    ))
                    .child(choice("keys-reload".into(), "Reload", false).on_click(
                        cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::ReloadKeybindings)),
                    )),
            )
            .child(div().text_xs().text_color(theme::text_faint()).child(
                "A JSON list of {\"key\", \"command\", \"when\"}; \"-command\" removes a default. \
                 The command palette (Ctrl+Shift+P) lists every command id's title and key.",
            ))
            .children(self.info.keybinding_problems.iter().map(|p| {
                div()
                    .text_xs()
                    .text_color(theme::danger())
                    .child(SharedString::from(p.clone()))
            }));

        let mut schedules = section("Scheduled runs");
        if self.info.schedules.is_empty() {
            schedules = schedules.child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted())
                    .child("No schedules."),
            );
        }
        for (ix, sch) in self.info.schedules.iter().enumerate() {
            let id = sch.id;
            let enabled = sch.enabled;
            let proposed = sch.proposed_by.is_some();
            let next = if proposed {
                "proposed by an agent: approve to run it".to_owned()
            } else {
                sch.next_run_at
                    .map(|t| format!("next {}", crate::shell::local_time(t)))
                    .unwrap_or_else(|| "not scheduled".into())
            };
            let last = sch
                .last_run_at
                .map(|t| format!(" · last {}", crate::shell::local_time(t)))
                .unwrap_or_default();
            schedules = schedules.child(
                row()
                    .id(("schedule", ix))
                    .child(mono(sch.cron.clone()).w(px(110.)))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_xs()
                            .child(SharedString::from(sch.prompt.clone())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if proposed {
                                gpui::Hsla::from(theme::warning())
                            } else {
                                theme::text_faint()
                            })
                            .child(format!("{next}{last}")),
                    )
                    .child(
                        choice(
                            ("sch-toggle", ix).into_element_id_string(),
                            if proposed {
                                "Approve"
                            } else if enabled {
                                "On"
                            } else {
                                "Off"
                            },
                            enabled,
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(SettingsEvent::ToggleSchedule(id, !enabled))
                        })),
                    )
                    .child(
                        choice(("sch-run", ix).into_element_id_string(), "Run now", false)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::RunSchedule(id))
                            })),
                    )
                    .child(
                        choice(("sch-del", ix).into_element_id_string(), "Delete", false).on_click(
                            cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::DeleteSchedule(id))
                            }),
                        ),
                    ),
            );
        }
        let target: SharedString = self
            .info
            .schedule_target
            .clone()
            .unwrap_or_else(|| "open a thread to choose the project".into());
        schedules = schedules
            .child(
                row()
                    .child(label("Cron (local time)"))
                    .child(field(&self.cron_input)),
            )
            .child(
                row()
                    .child(label("Prompt"))
                    .child(field(&self.prompt_input)),
            )
            .child(
                row()
                    .child(label("Goes to"))
                    .child(div().text_xs().child(target))
                    .child(
                        choice(
                            "sch-in-thread".into(),
                            "Post into the open thread",
                            self.in_open_thread,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.in_open_thread = !this.in_open_thread;
                            cx.notify();
                        })),
                    )
                    .child(button(
                        "sch-create".into(),
                        "Create",
                        theme::accent_bg(),
                        theme::text(),
                        cx.listener(|this, _, _, cx| this.create_schedule(cx)),
                    )),
            )
            .child(div().text_xs().text_color(theme::text_faint()).child(
                "Five fields: minute hour day month weekday. Runs in the core, survive restarts; \
                 a time missed while the app was closed runs once at the next start.",
            ));

        let data = section("Data")
            .child(
                row()
                    .child(label("Data folder"))
                    .child(mono(self.info.data_dir.display().to_string())),
            )
            .child(
                row()
                    .child(label("Settings file"))
                    .child(mono(settings_path.display().to_string())),
            )
            .child(
                row()
                    .child(label("Agent tools (MCP)"))
                    .child(div().text_xs().child(if self.info.mcp {
                        "on: agents get t3_thread_* and delegate_task for their own project"
                    } else {
                        "off (BLONGO_MCP=0)"
                    })),
            );

        let mut envs = section("Environments");
        if self.info.environments.is_empty() {
            envs = envs.child(div().text_xs().text_color(theme::text_muted()).child(
                "Only this machine. Add one with “+ Environment” in the sidebar or `blongo env add`.",
            ));
        }
        for (ix, (name, target)) in self.info.environments.iter().enumerate() {
            let n = name.clone();
            envs = envs.child(
                row()
                    .child(label(name.clone()))
                    .child(mono(target.clone()).flex_1())
                    .child(
                        choice(("env-remove", ix).into_element_id_string(), "Remove", false)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(SettingsEvent::RemoveEnvironment(n.clone()))
                            })),
                    ),
            );
        }

        let mut github = section("GitHub pull requests");
        match &self.info.forge {
            None => {
                github = github.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .child("Open a thread: these settings belong to its project."),
                );
            }
            Some((_, name, f)) => {
                let custom = matches!(f.base_branch, BaseBranch::Custom { .. });
                github = github
                    .child(
                        row()
                            .child(label("Project"))
                            .child(div().text_xs().child(name.clone())),
                    )
                    .child(
                        row()
                            .child(label("Base branch"))
                            .child(
                                choice("forge-base-default".into(), "GitHub's default", !custom)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_forge(cx, |f| {
                                            f.base_branch = BaseBranch::GithubDefault
                                        })
                                    })),
                            )
                            .child(div().text_xs().text_color(theme::text_muted()).child("or"))
                            .child(field(&self.base_input)),
                    )
                    .child(
                        row()
                            .child(label("Branch prefix"))
                            .child(field(&self.prefix_input)),
                    )
                    .child(div().text_xs().text_color(theme::text_faint()).child(
                        "New worktrees start from the base branch as GitHub has it, and pull \
                         requests target it. When GitHub cannot be asked, the last known default \
                         is used, then the remote's HEAD. Branches Blongo names start with the \
                         prefix; an empty base field means GitHub's default.",
                    ));
            }
        }

        let body: Vec<gpui::Div> = match self.tab {
            Tab::General => vec![approvals, appearance, notifications],
            Tab::Providers => vec![providers],
            Tab::Schedules => vec![schedules],
            Tab::Inbox => vec![inbox],
            Tab::GitHub => vec![github],
            Tab::Updates => vec![updates],
            Tab::Keys => vec![keybindings],
            Tab::Data => vec![data, envs],
        };
        let nav = div()
            .w(px(170.))
            .flex_shrink_0()
            .h_full()
            .py_3()
            .px_2()
            .flex()
            .flex_col()
            .gap_0p5()
            .border_r_1()
            .border_color(theme::border())
            .children(Tab::ALL.iter().map(|&tab| {
                div()
                    .id(SharedString::from(format!("settings-tab-{}", tab.label())))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .text_color(if tab == self.tab {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .when(tab == self.tab, |d| d.bg(theme::surface_hover()))
                    .hover(|d| d.bg(theme::surface_hover()))
                    .child(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }));
        div().size_full().flex().child(nav).child(
            div()
                .id("settings")
                .flex_1()
                .h_full()
                .overflow_y_scroll()
                .child(
                    div()
                        .max_w(px(820.))
                        .px_6()
                        .py_3()
                        .flex()
                        .flex_col()
                        .child(row().min_h(px(20.)).pb_2().when_some(
                            self.message.clone(),
                            |d, (ok, m)| {
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
                            },
                        ))
                        .children(body),
                ),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Providers,
    Schedules,
    Inbox,
    GitHub,
    Updates,
    Keys,
    Data,
}

impl Tab {
    const ALL: [Tab; 8] = [
        Tab::General,
        Tab::Providers,
        Tab::Schedules,
        Tab::Inbox,
        Tab::GitHub,
        Tab::Updates,
        Tab::Keys,
        Tab::Data,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Providers => "Providers & models",
            Tab::Schedules => "Scheduled runs",
            Tab::Inbox => "Review inbox",
            Tab::GitHub => "GitHub",
            Tab::Updates => "Updates",
            Tab::Keys => "Keybindings",
            Tab::Data => "Data & environments",
        }
    }
}

trait IdString {
    fn into_element_id_string(self) -> SharedString;
}

impl IdString for (&'static str, usize) {
    fn into_element_id_string(self) -> SharedString {
        format!("{}-{}", self.0, self.1).into()
    }
}
