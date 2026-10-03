//! Phase 0 shell: sidebar + virtualized transcript.
//!
//! `BLONGO_AGENT=claude` runs one Claude Code turn through the in-process core
//! (`BLONGO_CLAUDE_EXE` overrides the CLI, e.g. zeron's `replay-claude.py`),
//! prompting after `BLONGO_REPLAY_START_MS`.
//!
//! `BLONGO_REPLAY=<jsonl>` instead streams a recorded reply into the transcript at
//! `BLONGO_REPLAY_DELAY_MS` (default 40) after `BLONGO_REPLAY_START_MS`
//! (default 0), so the UI can be profiled against zeron with the same
//! workload (`tools/fixtures/resource-stream.jsonl`).

mod core;
mod transcript;

use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};
use gpui_platform::application;
use mimalloc::MiMalloc;
use transcript::Transcript;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

struct Shell {
    threads: Vec<SharedString>,
    selected: usize,
    transcript: Entity<Transcript>,
}

impl Render for Shell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = div()
            .flex()
            .flex_col()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(0x17171a))
            .border_r_1()
            .border_color(rgb(0x2a2a2e))
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(0x8a8a90))
                    .child("blongo"),
            )
            .children(self.threads.iter().enumerate().map(|(ix, title)| {
                let selected = ix == self.selected;
                div()
                    .id(ix)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(if selected {
                        rgb(0xf2f2f3)
                    } else {
                        rgb(0xb4b4b8)
                    })
                    .when(selected, |d| d.bg(rgb(0x26262b)))
                    .hover(|d| d.bg(rgb(0x202024)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = ix;
                        cx.notify();
                    }))
                    .child(title.clone())
            }));

        div()
            .flex()
            .size_full()
            .bg(rgb(0x1c1c20))
            .text_color(rgb(0xe6e6e8))
            .child(sidebar)
            .child(div().flex_1().h_full().child(self.transcript.clone()))
    }
}

fn main() {
    let replay = std::env::var("BLONGO_REPLAY").ok();
    let delay = env_ms("BLONGO_REPLAY_DELAY_MS", 40);
    let start = env_ms("BLONGO_REPLAY_START_MS", 0);

    application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                let transcript = cx.new(|cx| {
                    let mut t = Transcript::new();
                    if std::env::var("BLONGO_AGENT").as_deref() == Ok("claude") {
                        let events = core::spawn_claude(core::AgentRun {
                            executable: std::env::var_os("BLONGO_CLAUDE_EXE").map(Into::into),
                            prompt: "Write a detailed tutorial of Rust ownership.".into(),
                            start_after: start,
                        });
                        t.follow_agent(events, cx);
                    } else if let Some(path) = replay.clone() {
                        t.start_replay(path.into(), start, delay, cx);
                    }
                    t
                });
                cx.new(|_| Shell {
                    threads: ["Rust ownership tutorial", "Phase 0 spikes", "Untitled"]
                        .into_iter()
                        .map(SharedString::from)
                        .collect(),
                    selected: 0,
                    transcript,
                })
            },
        )
        .unwrap();
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}

fn env_ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}
