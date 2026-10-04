//! Spike 2: drive `codex app-server` over JSON-RPC.
//!
//! cargo run -p blongo-harness --example codex -- [--exe PATH] [--shim] [--idle SECS] [PROMPT…]
//!
//! With no prompt it only does the handshake (initialize, account/read,
//! thread/start), idles `--idle` seconds and reports the agent's RSS.
//! `--shim` launches the npm Node shim instead of the native binary.

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use blongo_harness::codex::{self, CodexOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = common::parse_args("usage: codex [--shim] [flags] [PROMPT…]");
    let options = CodexOptions {
        prefer_native: !args.flags.iter().any(|f| f == "--shim"),
        ..CodexOptions::default()
    };
    let exe = codex::resolve(&args.config(), &options)?;
    eprintln!("[codex] launching {}", exe.display());
    let mut session = codex::start_with(args.config(), options).await?;
    let peaks = common::start_sampler(session.pid());
    if !common::wait_started(&mut session, 60).await {
        anyhow::bail!("codex app-server did not start a thread");
    }
    common::report("after thread/start", &session);
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
