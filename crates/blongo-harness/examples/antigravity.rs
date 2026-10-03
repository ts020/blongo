//! Spike 4: drive Antigravity's ACP server (`agy_acp_server`).
//!
//! cargo run -p blongo-harness --example antigravity -- [--exe PATH] [--idle SECS] [PROMPT…]
//!
//! The executable defaults to $BLONGO_ANTIGRAVITY_EXECUTABLE, then the
//! managed install under $XDG_DATA_HOME/blongo/antigravity-acp/current, then
//! PATH. When not signed in the server's OAuth URL is printed as an
//! `auth_required` event and the session ends.

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use blongo_harness::acp;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = common::parse_args("usage: antigravity [flags] [PROMPT…]");
    let mut session = acp::start(args.config(), acp::antigravity()).await?;
    let peaks = common::start_sampler(session.pid());
    if !common::wait_started(&mut session, 180).await {
        common::report("setup ended", &session);
        session.shutdown().await;
        return Ok(());
    }
    common::report("after session/new", &session);
    if args.idle > 0 {
        tokio::time::sleep(Duration::from_secs(args.idle)).await;
        common::report(&format!("idle {}s", args.idle), &session);
    }
    for prompt in &args.prompts {
        session.prompt(prompt.clone())?;
        let (deltas, bytes) = common::run_turn(&mut session, &args).await;
        eprintln!("\n[turn] {deltas} text deltas, {bytes} bytes");
        common::report("after turn", &session);
    }
    {
        let p = peaks.lock().unwrap();
        eprintln!(
            "[rss] peak over {} samples: harness {} KiB, agent tree {} KiB",
            p.samples, p.self_rss, p.child_tree
        );
    }
    session.shutdown().await;
    Ok(())
}
