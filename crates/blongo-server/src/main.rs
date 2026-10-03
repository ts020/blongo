//! `blongo-serve`: Blongo's orchestrator without a window (no GPUI in this
//! binary), reachable from Blongo apps over its wire protocol.
//!
//! ```text
//! blongo-serve [--listen ADDR | --tailscale] [--port N] [--insecure-listen]
//!              [--data-dir DIR] [--no-socket] [--pair]
//! blongo-serve --stdio [--data-dir DIR]     (for `ssh host blongo-serve --stdio`)
//! blongo-serve pair [--ttl SECS] [--data-dir DIR]
//! blongo-serve devices [--data-dir DIR]
//! blongo-serve revoke DEVICE_ID_OR_NAME [--data-dir DIR]
//! blongo-serve mcp-bridge SOCKET TOKEN_FILE   (started by agents: MCP tools)
//! ```
//!
//! Environment: the core's (`BLONGO_DATA_DIR`, `BLONGO_CODEX_EXE`, …, see
//! the app), `BLONGO_TAILSCALE` (the `tailscale` CLI), `BLONGO_TERMINAL_SHELL`.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use blongo_core::CoreConfig;
use blongo_server::auth::AuthStore;
use blongo_server::{ServeConfig, describe};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const DEFAULT_PAIRING_TTL: u64 = 600;

struct Args {
    command: Option<String>,
    positional: Vec<String>,
    listen: Option<String>,
    port: Option<u16>,
    tailscale: bool,
    insecure: bool,
    data_dir: Option<PathBuf>,
    no_socket: bool,
    pair: bool,
    stdio: bool,
    ttl: u64,
}

fn usage() -> ! {
    eprintln!(
        "usage: blongo-serve [--listen ADDR | --tailscale] [--port N] [--insecure-listen] \
         [--data-dir DIR] [--no-socket] [--pair]\n       blongo-serve --stdio [--data-dir DIR]\n       \
         blongo-serve pair [--ttl SECS] | devices | revoke ID_OR_NAME   [--data-dir DIR]"
    );
    std::process::exit(2)
}

fn parse() -> Args {
    let mut args = Args {
        command: None,
        positional: Vec::new(),
        listen: None,
        port: None,
        tailscale: false,
        insecure: false,
        data_dir: None,
        no_socket: false,
        pair: false,
        stdio: false,
        ttl: DEFAULT_PAIRING_TTL,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next().unwrap_or_else(|| {
                eprintln!("blongo-serve: {name} needs a value");
                usage()
            })
        };
        match arg.as_str() {
            "--listen" => args.listen = Some(value("--listen")),
            "--port" => args.port = Some(value("--port").parse().unwrap_or_else(|_| usage())),
            "--tailscale" => args.tailscale = true,
            "--insecure-listen" => args.insecure = true,
            "--data-dir" => args.data_dir = Some(value("--data-dir").into()),
            "--no-socket" => args.no_socket = true,
            "--pair" => args.pair = true,
            "--stdio" => args.stdio = true,
            "--ttl" => args.ttl = value("--ttl").parse().unwrap_or_else(|_| usage()),
            "-h" | "--help" => usage(),
            s if s.starts_with('-') => {
                eprintln!("blongo-serve: unknown option {s}");
                usage()
            }
            s if args.command.is_none() => args.command = Some(s.to_owned()),
            s => args.positional.push(s.to_owned()),
        }
    }
    args
}

fn core_config(args: &Args) -> CoreConfig {
    let mut config = CoreConfig::from_env();
    if let Some(dir) = &args.data_dir {
        let defaults = CoreConfig::new(dir.join("blongo.sqlite"));
        config.database = defaults.database;
        config.data_dir = defaults.data_dir;
    }
    config
}

fn main() {
    // An agent's MCP bridge to this server's core: plain byte pump, no
    // runtime, before anything else is set up.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("mcp-bridge") {
        let (Some(socket), Some(token)) = (raw.get(1), raw.get(2)) else {
            eprintln!("usage: blongo-serve mcp-bridge SOCKET TOKEN_FILE");
            std::process::exit(2)
        };
        std::process::exit(blongo_core::mcp::run_bridge(
            std::path::Path::new(socket),
            std::path::Path::new(token),
        ));
    }
    let args = parse();
    let core = core_config(&args);
    let state_dir = core.data_dir.join("server");
    match args.command.as_deref() {
        None => {}
        Some("pair") => std::process::exit(pair(&state_dir, args.ttl)),
        Some("devices") => std::process::exit(devices(&state_dir)),
        Some("revoke") => {
            let Some(who) = args.positional.first() else {
                usage()
            };
            std::process::exit(revoke(&state_dir, who))
        }
        Some(other) => {
            eprintln!("blongo-serve: unknown command {other}");
            usage()
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    std::process::exit(rt.block_on(serve(args, core, state_dir)));
}

async fn serve(args: Args, core: CoreConfig, state_dir: PathBuf) -> i32 {
    let mut config = ServeConfig::new(core);
    config.state_dir = state_dir.clone();
    config.unix_socket = !args.no_socket;
    config.insecure_listen = args.insecure;
    if args.stdio {
        // Bridge to a running server when there is one.
        match blongo_server::bridge_stdio(&state_dir).await {
            Ok(true) => return 0,
            Ok(false) => {}
            Err(e) => {
                eprintln!("blongo-serve: {e}");
                return 1;
            }
        }
        config.listen = None;
        config.unix_socket = false;
        let (reader, writer) = blongo_client::transport::stream_halves(
            tokio::io::stdin(),
            tokio::io::stdout(),
            blongo_protocol::wire::MAX_CLIENT_FRAME,
        );
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        blongo_server::run(config, Some((reader, writer)), shutdown_signal(), ready_tx).await;
        return match ready_rx.recv() {
            Ok(Ok(_)) => 0,
            Ok(Err(e)) => {
                eprintln!("blongo-serve: {e}");
                1
            }
            Err(_) => 1,
        };
    }
    let port = args.port.unwrap_or(blongo_client::target::DEFAULT_PORT);
    config.tailscale_listen = args.tailscale;
    config.listen = Some(if args.tailscale {
        match blongo_server::tailscale_ip() {
            Ok(ip) => SocketAddr::new(ip, port),
            Err(e) => {
                eprintln!("blongo-serve: {e}");
                return 1;
            }
        }
    } else if let Some(listen) = &args.listen {
        match listen.parse::<SocketAddr>() {
            Ok(addr) => addr,
            Err(_) => match listen.parse::<IpAddr>() {
                Ok(ip) => SocketAddr::new(ip, port),
                Err(_) => {
                    eprintln!("blongo-serve: bad --listen address {listen}");
                    return 2;
                }
            },
        }
    } else {
        SocketAddr::from(([127, 0, 0, 1], port))
    });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<blongo_server::Ready, String>>();
    let pair_on_start = args.pair;
    let ttl = args.ttl;
    let announce = std::thread::spawn(move || match ready_rx.recv() {
        Ok(Ok(ready)) => {
            if let Some(addr) = ready.addr {
                println!("blongo-serve: listening on ws://{addr}/ws");
            }
            if let Some(socket) = &ready.socket {
                println!("blongo-serve: local socket {}", socket.display());
            }
            if pair_on_start {
                match AuthStore::open(&state_dir).and_then(|a| {
                    a.add_pairing_code(ttl)
                        .map_err(|e| std::io::Error::other(e.to_string()))
                }) {
                    Ok(code) => println!(
                        "blongo-serve: pairing code (valid {} min, single use): {code}",
                        ttl / 60
                    ),
                    Err(e) => eprintln!("blongo-serve: cannot create a pairing code: {e}"),
                }
            }
            Some(ready.stats)
        }
        Ok(Err(e)) => {
            eprintln!("blongo-serve: {e}");
            None
        }
        Err(_) => None,
    });
    blongo_server::run(config, None, shutdown_signal(), ready_tx).await;
    match announce.join().ok().flatten() {
        Some(stats) => {
            eprintln!("blongo-serve: stopped ({})", describe(&stats));
            0
        }
        None => 1,
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

fn pair(state_dir: &std::path::Path, ttl: u64) -> i32 {
    match AuthStore::open(state_dir)
        .map_err(|e| e.to_string())
        .and_then(|a| a.add_pairing_code(ttl).map_err(|e| e.to_string()))
    {
        Ok(code) => {
            println!("{code}");
            eprintln!(
                "blongo-serve: single-use pairing code, valid {} min. On the client run \
                 `blongo env add NAME ws://HOST:PORT` and enter it (or use + Environment). \
                 Whoever holds it can pair a device with full access to this server.",
                ttl / 60
            );
            0
        }
        Err(e) => {
            eprintln!("blongo-serve: {e}");
            1
        }
    }
}

fn devices(state_dir: &std::path::Path) -> i32 {
    match AuthStore::open(state_dir) {
        Ok(a) => {
            for d in a.devices() {
                println!(
                    "{}  {:<24}  paired {}  last seen {}",
                    d.id, d.name, d.created_at, d.last_seen_at
                );
            }
            0
        }
        Err(e) => {
            eprintln!("blongo-serve: {e}");
            1
        }
    }
}

fn revoke(state_dir: &std::path::Path, who: &str) -> i32 {
    // A running server revokes and closes the device's live connections;
    // without one, editing the device list is enough.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let result = match rt.block_on(blongo_server::revoke_via_socket(state_dir, who)) {
        Ok(Some(n)) => Ok((n, true)),
        Ok(None) => AuthStore::open(state_dir)
            .map_err(|e| e.to_string())
            .and_then(|a| a.revoke(who).map_err(|e| e.to_string()))
            .map(|ids| (ids.len(), false)),
        Err(e) => Err(e),
    };
    match result {
        Ok((0, _)) => {
            eprintln!("blongo-serve: no device {who:?}");
            1
        }
        Ok((n, live)) => {
            eprintln!(
                "blongo-serve: revoked {n} device(s); {}",
                if live {
                    "their open connections were closed"
                } else {
                    "no server is running (a server started with --no-socket must be \
                     restarted to drop open connections)"
                }
            );
            0
        }
        Err(e) => {
            eprintln!("blongo-serve: {e}");
            1
        }
    }
}
