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
//! - Profiling (tools/profile.py): `BLONGO_PROFILE_PROMPT` is sent in a
//!   project for `BLONGO_PROFILE_PROJECT` (default: cwd) after
//!   `BLONGO_PROFILE_START_MS`; "blongo: replay done" is printed when the
//!   run ends.

mod highlight;
mod input;
mod markdown;
mod shell;
mod sidebar;
mod terminal;
mod theme;
mod timeline;

use std::cell::RefCell;
use std::rc::Rc;
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

fn main() {
    let config = CoreConfig::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("import-t3") {
        std::process::exit(import_t3(config, args.get(1).map(Into::into)));
    }
    let (core, events) = match blongo_core::spawn(config.clone()) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("blongo: cannot start the core: {err:#}");
            std::process::exit(1);
        }
    };
    let auto_prompt = std::env::var("BLONGO_PROFILE_PROMPT")
        .ok()
        .filter(|p| !p.is_empty())
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
        });
    let client = core.client();
    // Owned by the app; taken and shut down cleanly when the window closes.
    let core = Rc::new(RefCell::new(Some(core)));
    let events = RefCell::new(Some(events));

    application().run(move |cx: &mut App| {
        input::bind_keys(cx);
        shell::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("secondary-q", Quit, None)]);
        let quit_core = core.clone();
        cx.on_action(move |_: &Quit, cx| {
            if let Some(core) = quit_core.borrow_mut().take() {
                core.shutdown();
            }
            cx.quit();
        });
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let events = events.borrow_mut().take().expect("run once");
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
                    shell::Shell::new(client.clone(), events, auto_prompt.clone(), window, cx)
                })
            },
        )
        .expect("open window");
        let close_core = core.clone();
        cx.on_window_closed(move |cx, _| {
            if let Some(core) = close_core.borrow_mut().take() {
                core.shutdown();
            }
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
                    "imported from {}: {} projects, {} threads, {} runs, {} items ({} threads already imported)",
                    source.display(),
                    r.projects,
                    r.threads,
                    r.runs,
                    r.items,
                    r.skipped_threads
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
