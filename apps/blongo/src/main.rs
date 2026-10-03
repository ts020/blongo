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
//! - Profiling (tools/profile.py): `BLONGO_PROFILE_PROMPT` is sent in a
//!   project for `BLONGO_PROFILE_PROJECT` (default: cwd) after
//!   `BLONGO_PROFILE_START_MS`; "blongo: replay done" is printed when the
//!   run ends.

mod input;
mod markdown;
mod shell;
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
    let (core, events) = match blongo_core::spawn(config.clone()) {
        Ok(core) => core,
        Err(err) => {
            eprintln!("blongo: cannot open {}: {err:#}", config.database.display());
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
        });
    let client = core.client();
    // Owned by the app; taken and shut down cleanly when the window closes.
    let core = Rc::new(RefCell::new(Some(core)));
    let events = RefCell::new(Some(events));

    application().run(move |cx: &mut App| {
        input::bind_keys(cx);
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
