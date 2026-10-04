//! Spike 3: drive the `claude` CLI over stream-json.
//!
//! cargo run -p blongo-harness --example claude -- [--exe PATH] [--approve allow|deny] PROMPT…
//!
//! Replay without credentials (zeron's fake CLI):
//!   ZERON_REPLAY_JOURNAL=tools/fixtures/resource-stream.jsonl \
//!   cargo run --release -p blongo-harness --example claude -- \
//!     --exe ../zeron/scripts/replay-claude.py --quiet "write the tutorial"

#[path = "common/mod.rs"]
mod common;

use blongo_harness::claude;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = common::parse_args("usage: claude [flags] PROMPT…");
    let mut session = claude::start(args.config()).await?;
    let peaks = common::start_sampler(session.pid());
    common::report("after spawn", &session);
    let prompts = if args.prompts.is_empty() {
        vec!["Say hello.".to_owned()]
    } else {
        args.prompts.clone()
    };
    for prompt in &prompts {
        session.prompt(prompt.clone())?;
        let started = std::time::Instant::now();
        let (deltas, bytes) = common::run_turn(&mut session, &args).await;
        eprintln!(
            "\n[turn] {deltas} text deltas, {bytes} bytes in {:.1}s",
            started.elapsed().as_secs_f64()
        );
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
