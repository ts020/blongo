//! Install the pinned Antigravity ACP server from the real origin.
//!
//! cargo run -p blongo-harness --example antigravity_install -- [ROOT]
//!
//! ROOT defaults to the managed location ($XDG_DATA_HOME/blongo/antigravity-acp).

use std::path::PathBuf;

use blongo_harness::antigravity_install as install;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(install::default_root)
        .ok_or_else(|| anyhow::anyhow!("no install root"))?;
    let pin =
        install::antigravity_pin().ok_or_else(|| anyhow::anyhow!("no build for this platform"))?;
    let started = std::time::Instant::now();
    let entry = install::install(&root, &pin, install::ANTIGRAVITY_ORIGIN, |line| {
        eprintln!("[{:>6.1}s] {line}", started.elapsed().as_secs_f64())
    })
    .await?;
    eprintln!(
        "[{:>6.1}s] installed: {}",
        started.elapsed().as_secs_f64(),
        entry.display()
    );
    Ok(())
}
