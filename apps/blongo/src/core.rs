//! In-process core, Phase 0 edition: one agent session on its own
//! current-thread tokio runtime, events handed to the UI over a channel with
//! no serialization.

use std::{path::PathBuf, time::Duration};

use blongo_harness::{SessionConfig, claude};
use blongo_protocol::AgentEvent;
use tokio::sync::mpsc;

pub struct AgentRun {
    pub executable: Option<PathBuf>,
    pub prompt: String,
    pub start_after: Duration,
}

/// Spawn the core thread. Events arrive on the returned receiver, which can be
/// awaited from any executor (including GPUI's).
pub fn spawn_claude(run: AgentRun) -> mpsc::UnboundedReceiver<AgentEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("blongo-core".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let mut config = SessionConfig::new(cwd);
                if let Some(exe) = run.executable {
                    config = config.executable(exe);
                }
                let mut session = match claude::start(config).await {
                    Ok(session) => session,
                    Err(err) => {
                        let _ = tx.send(AgentEvent::Error {
                            message: format!("{err:#}"),
                        });
                        return;
                    }
                };
                tokio::time::sleep(run.start_after).await;
                if let Err(err) = session.prompt(run.prompt) {
                    let _ = tx.send(AgentEvent::Error {
                        message: format!("{err:#}"),
                    });
                    return;
                }
                while let Some(event) = session.next_event().await {
                    let done = matches!(event, AgentEvent::TurnCompleted { .. });
                    if tx.send(event).is_err() || done {
                        break;
                    }
                }
                // Keep the session (and its CLI) alive like a real idle thread.
                tx.closed().await;
                session.shutdown().await;
            });
        })
        .expect("spawn core thread");
    rx
}
