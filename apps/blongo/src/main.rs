//! Blongo: a lightweight agent orchestrator (Rust + GPUI).
//!
//! The UI thread runs GPUI; the orchestrator (`blongo-core`) runs on its own
//! small tokio thread in the same process. They talk over channels with
//! typed values only.
//!
//! Environment:
//! - `BLONGO_DATA_DIR`: where `blongo.sqlite` lives (default: platform data
//!   dir).
//! - `BLONGO_CODEX_EXE`: Codex executable (default: `codex` on PATH, with
//!   the npm shim swapped for its native binary).
//! - `BLONGO_CLAUDE_EXE`, `BLONGO_ANTIGRAVITY_EXE`: the other providers.
//! - `BLONGO_T3_DB`: t3code database for the sidebar's import action
//!   (default `~/.t3/userdata/statev2.sqlite`, read through a copy).
//! - `BLONGO_TERMINAL_SHELL`: shell for the terminal panel (default
//!   `$SHELL`).
//!
//! `blongo import-t3 [PATH]` imports t3code's history without opening a
//! window (one-way, read-only: the source database is copied first).
//!
//! Remote environments (Phase 3):
//! - `blongo serve [ARGS]` runs the headless server (`blongo-serve`, next to
//!   this executable or on PATH; no GPUI in that process).
//! - `blongo env add NAME TARGET [CODE]` pairs with a server and saves the
//!   credential (0600) in `$BLONGO_CONFIG_DIR` (default: the platform
//!   config dir); `blongo env list`, `blongo env remove NAME`.
//!
//! Phase 4:
//! - `blongo mcp-bridge SOCKET TOKEN_FILE`: started by agents to reach
//!   Blongo's MCP server (docs/phase4/mcp.md).
//! - `blongo blongo://…` opens a link (handing it to a running instance);
//!   `blongo register-url-handler` registers the scheme (Linux).
//! - Settings: `settings.json`, `keybindings.json`, `forge.json` in
//!   `$BLONGO_CONFIG_DIR`. `BLONGO_NOTIFY_CMD` replaces the notifier.
//!
//! - Profiling (tools/profile.py): `BLONGO_PROFILE_PROMPT` is sent in a
//!   project for `BLONGO_PROFILE_PROJECT` (default: cwd) after
//!   `BLONGO_PROFILE_START_MS`; "blongo: replay done" is printed when the
//!   run ends. `BLONGO_PROFILE_VIEW=diff|files` then opens that view
//!   (with an empty prompt: right after the delay).

mod deeplink;
mod diff;
mod files;
mod highlight;
mod inbox;
mod input;
mod keymap;
mod markdown;
mod notify;
mod palette;
mod pr;
mod pr_view;
mod query;
mod settings;
mod settings_view;
mod shell;
mod sidebar;
mod terminal;
mod theme;
mod timeline;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use blongo_core::CoreConfig;
use gpui::{
    App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, prelude::*, px,
    size,
};
use gpui_platform::application;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

actions!(blongo, [Quit]);

/// Every key binding: the text input's, quit, then the shell's commands
/// (defaults overlaid with the user's keybindings.json). Returns the
/// shell's bindings and the problems found in the user's file.
pub fn bind_all(cx: &mut App) -> (Vec<keymap::Binding>, Vec<String>) {
    cx.clear_key_bindings();
    input::bind_keys(cx);
    cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
    keymap::install(&settings::keybindings_path(), cx)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The agents' MCP bridge: a byte pump, no core, no window.
    if args.first().map(String::as_str) == Some("mcp-bridge") {
        let (Some(socket), Some(token)) = (args.get(1), args.get(2)) else {
            eprintln!("usage: blongo mcp-bridge SOCKET TOKEN_FILE");
            std::process::exit(2);
        };
        std::process::exit(blongo_core::mcp::run_bridge(
            socket.as_ref(),
            token.as_ref(),
        ));
    }
    let mut config = CoreConfig::from_env();
    match args.first().map(String::as_str) {
        Some("import-t3") => std::process::exit(import_t3(config, args.get(1).map(Into::into))),
        Some("serve") => std::process::exit(serve(&args[1..])),
        Some("env") => std::process::exit(env_command(&args[1..])),
        Some("register-url-handler") => match deeplink::register() {
            Ok(text) => {
                println!("{text}");
                std::process::exit(0)
            }
            Err(err) => {
                eprintln!("blongo: {err}");
                std::process::exit(1)
            }
        },
        _ => {}
    }
    // `blongo blongo://…`: hand the link to a running instance if there is
    // one; otherwise start and open it.
    let start_link = args.iter().find(|a| a.starts_with("blongo://")).cloned();
    if let Some(url) = &start_link {
        match deeplink::forward(&config.data_dir, url) {
            Ok(true) => std::process::exit(0),
            Ok(false) => {}
            Err(err) => eprintln!("blongo: cannot reach the running instance: {err}"),
        }
    }
    // Leftovers of SSH tunnels from crashed runs.
    std::thread::spawn(|| {
        let n = blongo_client::target::clean_stale_tunnel_dirs();
        if n > 0 {
            eprintln!("blongo: removed {n} stale tunnel folders");
        }
    });
    let app_settings = settings::Settings::load();
    if let Some(err) = &app_settings.load_error {
        eprintln!("blongo: {err}; using defaults");
    }
    theme::set_light(app_settings.value.theme == settings::ThemeMode::Light);
    config.settings = app_settings.value.core();
    // The ACP agent from the settings, unless the environment names one.
    if config.acp_executable.is_none() && !app_settings.value.acp.executable.is_empty() {
        config.acp_executable = Some(app_settings.value.acp.executable.clone().into());
        config.acp_args = app_settings.value.acp.args.clone();
    }
    let data_dir = config.data_dir.clone();
    let mcp = config.mcp;
    let (link_tx, link_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(url) = start_link {
        let _ = link_tx.send(url);
    }
    {
        let tx = link_tx.clone();
        if let Err(err) = deeplink::listen(&data_dir, move |url| {
            let _ = tx.send(url);
        }) {
            eprintln!("blongo: cannot listen for blongo:// links: {err}");
        }
    }
    let (core, events) = match blongo_core::spawn(config.clone()) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("blongo: cannot start the core: {err:#}");
            std::process::exit(1);
        }
    };
    let profile_view = match std::env::var("BLONGO_PROFILE_VIEW").as_deref() {
        Ok("diff") => Some(shell::View::Diff),
        Ok("files") => Some(shell::View::Files),
        Ok("pr") => Some(shell::View::Pr),
        _ => None,
    };
    let auto_prompt = std::env::var("BLONGO_PROFILE_PROMPT")
        .ok()
        .filter(|p| !p.is_empty() || profile_view.is_some())
        .map(|prompt| shell::AutoPrompt {
            prompt,
            delay: Duration::from_millis(
                std::env::var("BLONGO_PROFILE_START_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            ),
            project_dir: std::env::var_os("BLONGO_PROFILE_PROJECT")
                .map(Into::into)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| ".".into()),
            terminal: std::env::var_os("BLONGO_PROFILE_TERMINAL").is_some(),
            view: profile_view,
        });
    let client: Arc<dyn blongo_client::Backend> =
        Arc::new(blongo_client::LocalBackend(core.client()));
    // Owned by the app; taken and shut down cleanly when the window closes.
    let core = Rc::new(RefCell::new(Some(core)));
    let events = RefCell::new(Some(events));
    let app_settings = RefCell::new(Some(app_settings));
    let link_rx = RefCell::new(Some(link_rx));
    let exit_dir = data_dir.clone();

    let app = application();
    // macOS delivers links to the running app this way.
    app.on_open_urls(move |urls| {
        for url in urls {
            let _ = link_tx.send(url);
        }
    });
    app.run(move |cx: &mut App| {
        cx.set_global(app_settings.borrow_mut().take().expect("run once"));
        let (bindings, keybinding_problems) = bind_all(cx);
        let quit_core = core.clone();
        let quit_dir = exit_dir.clone();
        cx.on_action(move |_: &Quit, cx| {
            if let Some(core) = quit_core.borrow_mut().take() {
                core.shutdown();
            }
            deeplink::unlisten(&quit_dir);
            cx.quit();
        });
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let events = events.borrow_mut().take().expect("run once");
        let options = shell::ShellOptions {
            data_dir: data_dir.clone(),
            mcp,
            bindings,
            keybinding_problems,
            links: link_rx.borrow_mut().take(),
        };
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Blongo".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    shell::Shell::new(
                        client.clone(),
                        events,
                        auto_prompt.clone(),
                        options,
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("open window");
        let close_core = core.clone();
        let closed_dir = exit_dir.clone();
        cx.on_window_closed(move |cx, _| {
            if let Some(core) = close_core.borrow_mut().take() {
                core.shutdown();
            }
            deeplink::unlisten(&closed_dir);
            cx.quit();
        })
        .detach();
        cx.activate(true);
    });
}

/// `blongo import-t3 [PATH]`: run the import through the core and report.
fn import_t3(config: CoreConfig, source: Option<std::path::PathBuf>) -> i32 {
    let Some(source) = source.or_else(blongo_core::t3_import::default_source) else {
        eprintln!("blongo: no t3code database given and no home directory");
        return 2;
    };
    let (core, mut events) = match blongo_core::spawn(config) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("blongo: cannot start the core: {err:#}");
            return 1;
        }
    };
    core.client().import_t3(source.clone());
    let code = loop {
        match events.blocking_recv() {
            Some(blongo_core::CoreEvent::Imported(Ok(r))) => {
                println!(
                    "imported from {}: {} projects, {} threads, {} runs, {} items ({} threads updated, {} unchanged, {} unreadable rows skipped)",
                    source.display(),
                    r.projects,
                    r.threads,
                    r.runs,
                    r.items,
                    r.updated_threads.len(),
                    r.skipped_threads,
                    r.bad_rows
                );
                break 0;
            }
            Some(blongo_core::CoreEvent::Imported(Err(err))) => {
                eprintln!("blongo: import failed: {err}");
                break 1;
            }
            Some(blongo_core::CoreEvent::Failed { message }) => {
                eprintln!("blongo: {message}");
                break 1;
            }
            Some(_) => {}
            None => break 1,
        }
    };
    core.shutdown();
    code
}

/// `blongo serve ...`: hand over to the headless server binary, so the
/// server process never loads the UI.
fn serve(args: &[String]) -> i32 {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("blongo-serve")))
        .filter(|p| p.exists());
    let program = sibling.unwrap_or_else(|| "blongo-serve".into());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&program).args(args).exec();
        eprintln!("blongo: cannot run {}: {err}", program.display());
        1
    }
    #[cfg(not(unix))]
    match std::process::Command::new(&program).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("blongo: cannot run {}: {err}", program.display());
            1
        }
    }
}

/// The pairing code from stdin (prompted on a terminal).
fn read_code() -> Option<String> {
    use std::io::{BufRead, IsTerminal, Write};
    if std::io::stdin().is_terminal() {
        eprint!("pairing code: ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    let code = line.trim().to_owned();
    (!code.is_empty()).then_some(code)
}

/// `blongo env add|list|remove`.
fn env_command(args: &[String]) -> i32 {
    use blongo_client::environments::{EnvironmentFile, default_path};
    let path = default_path();
    let file = match EnvironmentFile::load(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("blongo: {err}");
            return 1;
        }
    };
    let arg = |i: usize| args.get(i).map(String::as_str);
    match (arg(0), arg(1), arg(2)) {
        (Some("list"), None, None) => {
            for env in &file.environments {
                let auth = if env.credential.is_some() {
                    "paired"
                } else {
                    "transport-authenticated"
                };
                println!("{}\t{}\t{auth}", env.name, env.target);
            }
            0
        }
        (Some("remove"), Some(name), None) => {
            match EnvironmentFile::update(&path, |file| file.remove(name)) {
                Ok(true) => {
                    eprintln!(
                        "blongo: removed {name} here; its credential still works on the server \
                         until you run `blongo-serve revoke` there"
                    );
                    0
                }
                Ok(false) => {
                    eprintln!("blongo: no environment named {name}");
                    1
                }
                Err(err) => {
                    eprintln!("blongo: {err}");
                    1
                }
            }
        }
        (Some("add"), Some(name), Some(target)) => {
            let (name, target) = (name.to_owned(), target.to_owned());
            // The code is read from stdin unless given (an argument shows up
            // in `ps` and shell history). SSH stdio needs none.
            let needs_code = !matches!(
                blongo_client::target::Target::parse(&target),
                Ok(blongo_client::target::Target::SshStdio { .. })
            );
            let code = match arg(3) {
                Some(code) => Some(code.to_owned()),
                None if needs_code => match read_code() {
                    Some(code) => Some(code),
                    None => {
                        eprintln!("blongo: no pairing code given");
                        return 1;
                    }
                },
                None => None,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            blongo_client::net::handle().spawn(async move {
                let device = blongo_client::pairing::device_name();
                let result =
                    blongo_client::pairing::pair(&name, &target, code.as_deref(), &device).await;
                let _ = tx.send(result);
            });
            match rx.recv() {
                Ok(Ok(env)) => {
                    let name = env.name.clone();
                    if let Err(err) = EnvironmentFile::update(&path, |file| file.upsert(env)) {
                        eprintln!("blongo: {err}");
                        return 1;
                    }
                    println!("added {name} ({})", path.display());
                    0
                }
                Ok(Err(err)) => {
                    eprintln!("blongo: {err}");
                    1
                }
                Err(_) => 1,
            }
        }
        _ => {
            eprintln!(
                "usage: blongo env add NAME TARGET   (reads the pairing code from stdin) | blongo env list | \
                 blongo env remove NAME\n\
                 targets: ws://HOST[:PORT], ssh://[USER@]HOST?port=N, ssh+stdio://[USER@]HOST"
            );
            2
        }
    }
}
