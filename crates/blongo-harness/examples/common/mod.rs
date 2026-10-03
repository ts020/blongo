//! Shared CLI plumbing for the spike examples: argument parsing, event
//! printing, approval policy and RSS sampling from /proc (Linux).

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use blongo_harness::{ApprovalDecision, Session, SessionConfig};
use blongo_protocol::AgentEvent;

pub struct Args {
    pub exe: Option<PathBuf>,
    pub cwd: PathBuf,
    pub decision: ApprovalDecision,
    pub quiet: bool,
    /// Seconds to idle after SessionStarted before prompting / exiting.
    pub idle: u64,
    /// Interrupt the turn after this many text deltas.
    pub interrupt_after: Option<usize>,
    /// Max seconds to wait for a turn.
    pub timeout: u64,
    pub flags: Vec<String>,
    pub prompts: Vec<String>,
}

pub fn parse_args(usage: &str) -> Args {
    let mut args = Args {
        exe: None,
        cwd: std::env::current_dir().unwrap(),
        decision: ApprovalDecision::Allow,
        quiet: false,
        idle: 0,
        interrupt_after: None,
        timeout: 120,
        flags: Vec::new(),
        prompts: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| die(usage));
        match arg.as_str() {
            "--exe" => args.exe = Some(value().into()),
            "--cwd" => args.cwd = value().into(),
            "--approve" => {
                args.decision = match value().as_str() {
                    "allow" => ApprovalDecision::Allow,
                    "session" => ApprovalDecision::AllowForSession,
                    "deny" => ApprovalDecision::Deny,
                    _ => die(usage),
                }
            }
            "--quiet" => args.quiet = true,
            "--idle" => args.idle = value().parse().unwrap_or_else(|_| die(usage)),
            "--timeout" => args.timeout = value().parse().unwrap_or_else(|_| die(usage)),
            "--interrupt-after" => {
                args.interrupt_after = Some(value().parse().unwrap_or_else(|_| die(usage)))
            }
            "-h" | "--help" => die(usage),
            flag if flag.starts_with("--") => args.flags.push(flag.to_owned()),
            prompt => args.prompts.push(prompt.to_owned()),
        }
    }
    args
}

fn die(usage: &str) -> ! {
    eprintln!(
        "{usage}\n\ncommon flags: --exe PATH --cwd DIR --approve allow|session|deny \
               --quiet --idle SECS --timeout SECS --interrupt-after N  [PROMPT…]"
    );
    std::process::exit(2)
}

impl Args {
    pub fn config(&self) -> SessionConfig {
        let mut config = SessionConfig::new(&self.cwd);
        config.executable = self.exe.clone();
        config
    }
}

/// VmRSS / VmHWM of a pid in KiB.
pub fn rss_kib(pid: u32, field: &str) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find(|l| l.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// RSS of `pid` plus all of its descendants, with a per-process breakdown.
pub fn tree_rss(pid: u32) -> (u64, Vec<(u32, String, u64)>) {
    let mut parents: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Ok(p) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{p}/stat")) else {
                continue;
            };
            // ppid is the 2nd field after the parenthesised comm.
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                continue;
            };
            if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
                parents.entry(ppid).or_default().push(p);
            }
        }
    }
    let mut stack = vec![pid];
    let mut total = 0;
    let mut procs = Vec::new();
    while let Some(p) = stack.pop() {
        if let Some(rss) = rss_kib(p, "VmRSS:") {
            let comm = std::fs::read_to_string(format!("/proc/{p}/comm"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            total += rss;
            procs.push((p, comm, rss));
        }
        if let Some(children) = parents.get(&p) {
            stack.extend(children);
        }
    }
    (total, procs)
}

#[derive(Default)]
pub struct Peaks {
    pub self_rss: u64,
    pub child_tree: u64,
    pub samples: u64,
}

/// Sample our RSS and the agent tree every 100 ms until dropped.
pub fn start_sampler(child: Option<u32>) -> std::sync::Arc<std::sync::Mutex<Peaks>> {
    let peaks = std::sync::Arc::new(std::sync::Mutex::new(Peaks::default()));
    let p = peaks.clone();
    tokio::spawn(async move {
        loop {
            let me = rss_kib(std::process::id(), "VmRSS:").unwrap_or(0);
            let tree = child.map(|c| tree_rss(c).0).unwrap_or(0);
            {
                let mut peaks = p.lock().unwrap();
                peaks.self_rss = peaks.self_rss.max(me);
                peaks.child_tree = peaks.child_tree.max(tree);
                peaks.samples += 1;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    peaks
}

pub fn report(label: &str, session: &Session) {
    let me = rss_kib(std::process::id(), "VmRSS:").unwrap_or(0);
    let hwm = rss_kib(std::process::id(), "VmHWM:").unwrap_or(0);
    eprintln!("[rss] {label}: harness VmRSS={me} KiB VmHWM={hwm} KiB");
    if let Some(pid) = session.pid() {
        let (total, procs) = tree_rss(pid);
        eprintln!("[rss] {label}: agent tree={total} KiB");
        for (pid, comm, rss) in procs {
            eprintln!("[rss]     pid {pid} {comm}: {rss} KiB");
        }
    }
}

pub fn print_event(event: &AgentEvent, quiet: bool) {
    match event {
        AgentEvent::TextDelta { text } if !quiet => {
            use std::io::Write;
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::TextDelta { .. } | AgentEvent::ReasoningDelta { .. } if quiet => {}
        AgentEvent::ReasoningDelta { text } => eprintln!("[thinking] {text}"),
        other => eprintln!("\n[event] {}", serde_json::to_string(other).unwrap()),
    }
}

/// Drive one prompt to `TurnCompleted`, answering approvals per `args`.
/// Returns (text deltas, text bytes).
pub async fn run_turn(session: &mut Session, args: &Args) -> (usize, usize) {
    let mut deltas = 0;
    let mut bytes = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    loop {
        let event = match tokio::time::timeout_at(deadline, session.next_event()).await {
            Ok(Some(event)) => event,
            Ok(None) => {
                eprintln!("\n[session ended]");
                return (deltas, bytes);
            }
            Err(_) => {
                eprintln!("\n[timeout: interrupting]");
                let _ = session.interrupt();
                return (deltas, bytes);
            }
        };
        print_event(&event, args.quiet);
        match &event {
            AgentEvent::TextDelta { text } => {
                deltas += 1;
                bytes += text.len();
                if args.interrupt_after == Some(deltas) {
                    eprintln!("\n[interrupting]");
                    let _ = session.interrupt();
                }
            }
            AgentEvent::ApprovalRequest { request_id, .. } => {
                eprintln!("[approve] {request_id} -> {:?}", args.decision);
                let _ = session.approve(request_id.clone(), args.decision);
            }
            AgentEvent::TurnCompleted { .. } => return (deltas, bytes),
            _ => {}
        }
    }
}

/// Wait for SessionStarted (or the session ending), printing what comes.
pub async fn wait_started(session: &mut Session, secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, session.next_event()).await {
            Ok(Some(event)) => {
                print_event(&event, false);
                if matches!(event, AgentEvent::SessionStarted { .. }) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(_) => {
                eprintln!("[setup timeout]");
                return false;
            }
        }
    }
}
