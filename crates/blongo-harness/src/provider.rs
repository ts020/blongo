//! One entry point for every provider: [`start`] picks the harness for a
//! [`ProviderKind`] and translates the provider-neutral [`StartOptions`]
//! (resume, native fork, native rewind) into that harness's options.
//!
//! What each provider can do is described by
//! [`ProviderKind::capabilities`]; callers branch on that, and only pass a
//! fork / rewind to providers whose capabilities say they support it.

use blongo_protocol::{PendingContext, ProviderKind};

use crate::{Session, SessionConfig, acp, claude, codex};

/// How a session continues an earlier conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartOptions {
    /// The provider's own thread / session id to continue.
    pub resume: Option<String>,
    /// Native fork or rewind to apply at start (`Handoff` is the caller's
    /// business: it changes the prompt, not the session).
    pub context: Option<PendingContext>,
}

/// Spawn the agent for `kind`.
pub async fn start(
    kind: ProviderKind,
    config: SessionConfig,
    options: StartOptions,
) -> anyhow::Result<Session> {
    match kind {
        ProviderKind::Codex => {
            let mut opts = codex::CodexOptions {
                resume_thread_id: options.resume,
                ..codex::CodexOptions::default()
            };
            match options.context {
                Some(PendingContext::Fork {
                    provider_thread_id,
                    up_to_turn,
                }) => {
                    opts.resume_thread_id = None;
                    opts.fork = Some((provider_thread_id, up_to_turn));
                }
                Some(PendingContext::Rewind { drop_from_turn, .. }) => {
                    opts.revert_before_turn = drop_from_turn;
                }
                Some(PendingContext::Handoff) | None => {}
            }
            codex::start_with(config, opts).await
        }
        ProviderKind::ClaudeCode => {
            let mut opts = claude::ClaudeOptions {
                resume: options.resume,
                ..claude::ClaudeOptions::default()
            };
            match options.context {
                Some(PendingContext::Fork {
                    provider_thread_id,
                    up_to_turn,
                }) => {
                    opts.resume = Some(provider_thread_id);
                    opts.fork_session = true;
                    opts.resume_session_at = up_to_turn;
                }
                Some(PendingContext::Rewind {
                    keep_through_turn, ..
                }) => match keep_through_turn {
                    Some(at) => opts.resume_session_at = Some(at),
                    // Everything was rolled back: start over.
                    None => opts.resume = None,
                },
                Some(PendingContext::Handoff) | None => {}
            }
            claude::start_with(config, opts).await
        }
        ProviderKind::Antigravity => {
            let mut agent = acp::antigravity();
            agent.resume_session = options.resume;
            acp::start(config, agent).await
        }
    }
}
