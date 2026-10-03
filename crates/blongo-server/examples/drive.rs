//! Headless client for profiling `blongo serve` (tools/profile_serve.py):
//! pairs with the server, creates a project + thread, sends one prompt
//! (the fake Codex streams its replay), prints `drive: done` when the run
//! ends and then stays connected and idle until it is killed. Between
//! `drive: ready` (paired, thread open) and the prompt it waits for a line
//! on stdin (or its end), so the profiler controls when streaming starts.
//!
//! Usage: drive TARGET PAIRING-CODE PROJECT-DIR PROMPT

use std::time::Duration;

use blongo_client::Backend;
use blongo_protocol::client::CoreEvent;
use blongo_protocol::{Command, CommandEnvelope, Delivery, ItemId, ProjectId, RunId, ThreadId};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [target, code, project, prompt] = args.as_slice() else {
        eprintln!("usage: drive TARGET PAIRING-CODE PROJECT-DIR PROMPT");
        std::process::exit(2);
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let env = blongo_client::pairing::pair("profile", target, Some(code), "drive")
            .await
            .unwrap_or_else(|e| {
                eprintln!("drive: {e}");
                std::process::exit(1);
            });
        let (backend, mut events) = blongo_client::remote::connect_on(
            &tokio::runtime::Handle::current(),
            env,
            Default::default(),
        );
        // Wait for the first shell snapshot.
        loop {
            match events.recv().await {
                Some(CoreEvent::Shell(_)) => break,
                Some(_) => {}
                None => std::process::exit(1),
            }
        }
        let project_id = ProjectId::new();
        let thread_id = ThreadId::new();
        for command in [
            Command::ProjectCreate {
                project_id,
                name: String::new(),
                path: project.clone(),
            },
            Command::ThreadCreate {
                thread_id,
                project_id,
                title: String::new(),
                provider: Default::default(),
                model: None,
                worktree: false,
                parent_thread_id: None,
            },
        ] {
            backend.dispatch(CommandEnvelope::new(command));
        }
        backend.open_thread(thread_id);
        println!("drive: ready");
        let _ = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)
        })
        .await;
        backend.dispatch(CommandEnvelope::new(Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: prompt.clone(),
            delivery: Delivery::Queue,
        }));
        println!("drive: started");
        let (mut deltas, mut bytes) = (0usize, 0usize);
        loop {
            match events.recv().await {
                Some(CoreEvent::TextDelta { chunk, .. }) => {
                    deltas += 1;
                    bytes += chunk.len();
                }
                Some(CoreEvent::RunFinished { status, .. }) => {
                    println!("drive: done ({status:?}, {deltas} deltas, {bytes} bytes)");
                    break;
                }
                Some(CoreEvent::CommandRejected { reason, .. }) => {
                    eprintln!("drive: rejected: {reason}");
                }
                Some(_) => {}
                None => std::process::exit(1),
            }
        }
        // Stay subscribed (an idle remote client) until killed.
        loop {
            tokio::select! {
                e = events.recv() => if e.is_none() { break },
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            }
        }
    });
}
