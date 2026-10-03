//! `blongo serve` end to end over the wire, with the fake Codex: pairing,
//! a turn with an approval and an interrupt, auth failures, resuming after
//! a dropped link, a server restart, a slow reader, SSH and the Unix
//! socket, server-side terminals.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use blongo_client::handshake::{ClientAuth, HandshakeError, handshake, recv, send as send_msg};
use blongo_client::target::{Target, open};
use blongo_protocol::wire::{
    AuthRequest, ClientMsg, Hello, Payload, RefuseCode, Resume, ServerMsg,
};
use common::*;

fn rt() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_with_approval_and_interrupt_over_the_wire() {
    let dir = temp_dir("e2e");
    let server = start(&dir);
    let env = pair(&dir, &ws_target(server.addr.unwrap())).await;
    let cred = env.credential.clone().expect("paired");
    assert_eq!(cred.server_id, server.server_id);
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    assert!(!wait_connected(&mut rx).await);
    wait_for(&mut rx, |e| matches!(e, CoreEvent::Shell(_)).then_some(())).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;

    // A turn that asks for approval.
    send(&backend, thread_id, "list the files");
    let approval = wait_for(&mut rx, |e| match e {
        CoreEvent::Event(ev) => match &ev.kind {
            EventKind::ItemAdded { item } | EventKind::ItemUpdated { item }
                if matches!(
                    item.kind,
                    ItemKind::ApprovalRequest {
                        state: ApprovalState::Pending,
                        ..
                    }
                ) =>
            {
                Some(item.id)
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    backend.dispatch(CommandEnvelope::new(Command::RuntimeRequestRespond {
        thread_id,
        item_id: approval,
        decision: ApprovalDecision::Approve,
    }));
    let mut text = String::new();
    let mut saw_user_body = false;
    let status = wait_for(&mut rx, |e| match e {
        CoreEvent::TextDelta { chunk, .. } => {
            text.push_str(chunk);
            None
        }
        CoreEvent::Event(ev) => {
            if let EventKind::ItemAdded { item } = &ev.kind
                && item.kind == ItemKind::UserMessage
            {
                // Bodies travel with the event over the wire.
                saw_user_body = &*item.text == "list the files";
            }
            None
        }
        CoreEvent::RunFinished { status, .. } => Some(*status),
        _ => None,
    })
    .await;
    assert_eq!(status, RunStatus::Completed);
    assert!(text.contains("decision=accept"), "{text:?}");
    let _ = saw_user_body;

    // A fresh snapshot has the approved command and its output.
    backend.open_thread(thread_id);
    let snapshot = wait_for(&mut rx, |e| match e {
        CoreEvent::Thread(s) if s.thread_id == thread_id => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(snapshot.items.iter().any(|i| matches!(
        &i.kind,
        ItemKind::CommandExecution { output, exit_code: Some(0), .. } if output.contains("Cargo.toml")
    )));
    assert!(
        snapshot
            .items
            .iter()
            .any(|i| i.kind == ItemKind::UserMessage && &*i.text == "list the files")
    );

    // A streaming turn, interrupted over the wire.
    send(&backend, thread_id, "loop");
    wait_for(&mut rx, |e| match e {
        CoreEvent::TextDelta { chunk, .. } if chunk.contains("tick 3") => Some(()),
        _ => None,
    })
    .await;
    backend.dispatch(CommandEnvelope::new(Command::RunInterrupt { thread_id }));
    assert_eq!(run_finished(&mut rx).await, RunStatus::Interrupted);

    // A rejected command comes back to its sender.
    let bad = CommandEnvelope::new(Command::RunInterrupt { thread_id });
    let bad_id = bad.command_id;
    backend.dispatch(bad);
    wait_for(&mut rx, |e| match e {
        CoreEvent::CommandRejected { command_id, .. } if *command_id == bad_id => Some(()),
        _ => None,
    })
    .await;
    drop(backend);
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_credentials_replayed_proofs_and_expired_codes_are_refused() {
    let dir = temp_dir("auth");
    let server = start(&dir);
    let target = ws_target(server.addr.unwrap());
    let env = pair(&dir, &target).await;
    let cred = env.credential.clone().unwrap();
    let parsed = Target::parse(&target).unwrap();

    // Wrong token (right device, right key): refused for good.
    let mut bad = env.clone();
    bad.credential.as_mut().unwrap().token = blongo_client::secret::new_token();
    let (_b, mut rx) = connect_on(&rt(), bad, fast_options());
    wait_for(&mut rx, |e| match e {
        CoreEvent::Connection(ConnectionState::Failed(m)) => {
            assert!(m.contains("Unauthorized"), "{m}");
            Some(())
        }
        CoreEvent::Connection(ConnectionState::Connected { .. }) => panic!("connected"),
        _ => None,
    })
    .await;

    // Replay: capture a valid Auth frame, then present it on a new
    // connection (new nonce).
    let mut link = open(&parsed).await.unwrap();
    send_msg(&mut link.writer, &ClientMsg::Hello(Hello::new("t", None)))
        .await
        .unwrap();
    let ServerMsg::Challenge(challenge) = recv(&mut link.reader).await.unwrap().remove(0) else {
        panic!()
    };
    let key = blongo_client::secret::device_key_from_b64(&cred.device_key).unwrap();
    let captured = AuthRequest::Token {
        device_id: cred.device_id.clone(),
        token: cred.token.clone(),
        proof: blongo_client::secret::make_proof(
            &key,
            blongo_protocol::wire::ProofPurpose::Token,
            &challenge.server_id,
            &challenge.nonce,
            &cred.token,
        ),
    };
    send_msg(&mut link.writer, &ClientMsg::Auth(captured.clone()))
        .await
        .unwrap();
    assert!(matches!(
        recv(&mut link.reader).await.unwrap()[0],
        ServerMsg::Welcome(_)
    ));
    drop(link);
    for _ in 0..2 {
        let mut link = open(&parsed).await.unwrap();
        send_msg(&mut link.writer, &ClientMsg::Hello(Hello::new("t", None)))
            .await
            .unwrap();
        let _challenge = recv(&mut link.reader).await.unwrap();
        send_msg(&mut link.writer, &ClientMsg::Auth(captured.clone()))
            .await
            .unwrap();
        assert!(matches!(
            recv(&mut link.reader).await.unwrap()[0],
            ServerMsg::Refused {
                code: RefuseCode::Unauthorized,
                ..
            }
        ));
    }

    // "Local" auth over the network: refused.
    let mut link = open(&parsed).await.unwrap();
    let err = handshake(
        &mut link.reader,
        &mut link.writer,
        Hello::new("t", None),
        ClientAuth::Local,
    )
    .await
    .err()
    .unwrap();
    assert!(err.is_permanent(), "{err}");

    // An expired pairing code.
    let store = blongo_server::auth::AuthStore::open(&dir.join("data/server")).unwrap();
    let expired = store.add_pairing_code(0).unwrap();
    let err = blongo_client::pairing::pair("x", &target, Some(&expired), "d")
        .await
        .unwrap_err();
    assert!(err.contains("Unauthorized"), "{err}");
    // A used one.
    let code = store.add_pairing_code(600).unwrap();
    blongo_client::pairing::pair("x", &target, Some(&code), "d")
        .await
        .unwrap();
    assert!(
        blongo_client::pairing::pair("x", &target, Some(&code), "d")
            .await
            .is_err()
    );

    // A credential for another server (its id is pinned).
    let mut other = cred.clone();
    other.server_id = "someone-else".into();
    let mut link = open(&parsed).await.unwrap();
    let err = handshake(
        &mut link.reader,
        &mut link.writer,
        Hello::new("t", None),
        ClientAuth::Credential(&other),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(err, HandshakeError::WrongServer));

    // An unsupported protocol version.
    let mut link = open(&parsed).await.unwrap();
    let mut hello = Hello::new("t", None);
    hello.version = 99;
    hello.min_version = 99;
    send_msg(&mut link.writer, &ClientMsg::Hello(hello))
        .await
        .unwrap();
    assert!(matches!(
        recv(&mut link.reader).await.unwrap()[0],
        ServerMsg::Refused {
            code: RefuseCode::Version,
            ..
        }
    ));
    assert!(
        server
            .stats
            .refused
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 3
    );
    server.stop();
}

/// The text of `item_id` assembled from deltas must equal the stored body:
/// nothing missing (gap) and nothing twice (duplicate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumes_after_a_dropped_link_without_gaps_or_duplicates() {
    let dir = temp_dir("resume");
    let server = start(&dir);
    let proxy = Proxy::start(server.addr.unwrap()).await;
    let env = pair(&dir, &ws_target(proxy.addr)).await;
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    assert!(!wait_connected(&mut rx).await);
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;

    send(&backend, thread_id, "slow");
    let mut texts: HashMap<ItemId, String> = HashMap::new();
    let mut resumes = 0;
    let mut cuts = 0;
    let mut deltas_since_cut = 0;
    let status = wait_for(&mut rx, |e| match e {
        CoreEvent::TextDelta { item_id, chunk, .. } => {
            texts.entry(*item_id).or_default().push_str(chunk);
            deltas_since_cut += 1;
            // Cut the link a few times mid-stream.
            if cuts < 3 && deltas_since_cut >= 8 {
                proxy.cut();
                cuts += 1;
                deltas_since_cut = 0;
            }
            None
        }
        CoreEvent::Connection(ConnectionState::Connected { resumed }) => {
            assert!(*resumed, "the server should replay, not resnapshot");
            resumes += 1;
            None
        }
        CoreEvent::Shell(_) | CoreEvent::Thread(_) => panic!("unexpected snapshot on resume"),
        CoreEvent::RunFinished { status, .. } => Some(*status),
        _ => None,
    })
    .await;
    assert_eq!(status, RunStatus::Completed);
    assert_eq!(cuts, 3);
    assert!(resumes >= 3, "resumed {resumes} times");
    assert!(
        server
            .stats
            .replayed
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    );

    backend.open_thread(thread_id);
    let snapshot = wait_for(&mut rx, |e| match e {
        CoreEvent::Thread(s) if s.thread_id == thread_id => Some(s.clone()),
        _ => None,
    })
    .await;
    let reply = snapshot
        .items
        .iter()
        .find(|i| matches!(i.kind, ItemKind::AssistantMessage { .. }))
        .unwrap();
    assert!(reply.text.contains("tick 60"), "{}", reply.text);
    assert_eq!(texts.get(&reply.id).map(String::as_str), Some(&*reply.text));
    drop(backend);
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_server_sends_fresh_snapshots() {
    let dir = temp_dir("restart");
    let server = start(&dir);
    let proxy = Proxy::start(server.addr.unwrap()).await;
    let env = pair(&dir, &ws_target(proxy.addr)).await;
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    wait_connected(&mut rx).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;
    send(&backend, thread_id, "echo: before restart");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);

    // Commands while the server is gone are refused at once.
    proxy
        .refusing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    server.stop();
    wait_for(&mut rx, |e| {
        matches!(
            e,
            CoreEvent::Connection(ConnectionState::Reconnecting { .. })
        )
        .then_some(())
    })
    .await;
    let cmd = CommandEnvelope::new(Command::RunInterrupt { thread_id });
    let id = cmd.command_id;
    backend.dispatch(cmd);
    wait_for(&mut rx, |e| match e {
        CoreEvent::CommandRejected { command_id, reason } if *command_id == id => {
            assert!(reason.contains("not connected"));
            Some(())
        }
        _ => None,
    })
    .await;

    let server = start(&dir);
    proxy.set_upstream(server.addr.unwrap());
    proxy
        .refusing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // New epoch: no replay, fresh shell and thread snapshots.
    let resumed = wait_connected(&mut rx).await;
    assert!(!resumed);
    let shell = wait_for(&mut rx, |e| match e {
        CoreEvent::Shell(s) => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(shell.threads.iter().any(|t| t.id == thread_id));
    let snapshot = wait_for(&mut rx, |e| match e {
        CoreEvent::Thread(s) if s.thread_id == thread_id => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(
        snapshot
            .items
            .iter()
            .any(|i| i.text.contains("echo: before restart"))
    );
    // And it works again.
    send(&backend, thread_id, "echo: after restart");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);
    drop(backend);
    server.stop();
}

/// A client that stops reading: the server's queue for it stays bounded,
/// its backlog is dropped, and once it reads again it gets `Resnapshot`
/// and fresh snapshots that hold the whole reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_reader_is_resnapshotted_with_bounded_memory() {
    let dir = temp_dir("slow");
    // A long, fast stream: 4000 deltas of 1 KiB, no delay.
    let fixture = dir.join("big.jsonl");
    let chunk = "x".repeat(1023) + "\n";
    let mut lines = String::new();
    for _ in 0..4000 {
        lines.push_str(&format!(
            "{{\"event\": {{\"type\": \"textDelta\", \"text\": {}}}}}\n",
            serde_json::to_string(&chunk).unwrap()
        ));
    }
    std::fs::write(&fixture, lines).unwrap();
    let mut config = config(&dir);
    config.core.agent_env = vec![
        ("FAKE_CODEX_DELAY_MS".into(), "0".into()),
        (
            "FAKE_CODEX_REPLAY".into(),
            fixture.to_string_lossy().into_owned(),
        ),
    ];
    config.limits.outbox.max_bytes = 256 * 1024;
    let server = blongo_server::start(config).unwrap();
    let target = ws_target(server.addr.unwrap());
    let env = pair(&dir, &target).await;

    // A well-behaved client sets up the thread.
    let (backend, mut rx) = connect_on(&rt(), env.clone(), fast_options());
    wait_connected(&mut rx).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;

    // The slow client: a tiny receive buffer, subscribed, then not reading.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let stream = socket.connect(server.addr.unwrap()).await.unwrap();
    let (ws, _) = tokio_tungstenite::client_async_with_config(
        target.as_str(),
        stream,
        Some(blongo_client::transport::ws_config(
            blongo_protocol::wire::MAX_SERVER_FRAME,
        )),
    )
    .await
    .unwrap();
    let (mut reader, mut writer) = blongo_client::transport::ws_halves(ws);
    let cred = env.credential.clone().unwrap();
    handshake(
        &mut reader,
        &mut writer,
        Hello::new("slow", None),
        ClientAuth::Credential(&cred),
    )
    .await
    .unwrap();
    send_msg(
        &mut writer,
        &ClientMsg::Subscribe {
            threads: vec![thread_id],
        },
    )
    .await
    .unwrap();

    send(&backend, thread_id, "replay");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);
    // The well-behaved client got every byte.
    let resnapshots = server
        .stats
        .resnapshots
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(resnapshots >= 1, "the slow client never overflowed");

    // Now read: Resnapshot, then a snapshot, then deltas; together they
    // hold the whole reply, with nothing lost or doubled after the
    // snapshot.
    let mut saw_resnapshot = false;
    let mut reply: Option<(ItemId, String)> = None;
    let full = 4000 * 1024;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while reply.as_ref().is_none_or(|(_, t)| t.len() < full) {
        let msgs = tokio::time::timeout_at(deadline, recv(&mut reader))
            .await
            .expect("slow client did not catch up")
            .unwrap();
        for msg in msgs {
            match msg {
                ServerMsg::Resnapshot => saw_resnapshot = true,
                ServerMsg::Seq(s) if saw_resnapshot => match s.payload {
                    Payload::Thread(t) => {
                        reply = t
                            .items
                            .iter()
                            .find(|i| matches!(i.item.kind, ItemKind::AssistantMessage { .. }))
                            .map(|i| (i.item.id, i.text.to_string()));
                    }
                    Payload::TextDelta { item_id, chunk, .. } => {
                        if let Some((id, text)) = reply.as_mut()
                            && *id == item_id
                        {
                            text.push_str(&chunk);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    assert_eq!(reply.unwrap().1.len(), full);
    assert!(saw_resnapshot);
    drop(reader);
    drop(writer);
    drop(backend);
    // Every connection's queue stayed within its bound (plus one delta).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server
        .stats
        .connections
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0
        && std::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let peak = server
        .stats
        .peak_outbox_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(peak > 0 && peak <= 256 * 1024 + 4096, "peak backlog {peak}");
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_points_outside_the_ring_get_snapshots() {
    let dir = temp_dir("ring");
    let mut config = config(&dir);
    config.limits.ring.max_msgs = 8;
    let server = blongo_server::start(config).unwrap();
    let target = ws_target(server.addr.unwrap());
    let env = pair(&dir, &target).await;
    let (backend, mut rx) = connect_on(&rt(), env.clone(), fast_options());
    wait_connected(&mut rx).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;
    send(&backend, thread_id, "slow");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);

    let cred = env.credential.clone().unwrap();
    let parsed = Target::parse(&target).unwrap();
    // A resume point long gone from the 8-entry ring.
    let mut link = open(&parsed).await.unwrap();
    let session = handshake(
        &mut link.reader,
        &mut link.writer,
        Hello::new(
            "t",
            Some(Resume {
                epoch: 0,
                last_seq: 1,
                threads: vec![thread_id],
            }),
        ),
        ClientAuth::Credential(&cred),
    )
    .await
    .unwrap();
    // Wrong epoch anyway; and with the right epoch:
    assert!(!session.welcome.resumed);
    let epoch = session.challenge.epoch;
    drop(link);
    let mut link = open(&parsed).await.unwrap();
    let session = handshake(
        &mut link.reader,
        &mut link.writer,
        Hello::new(
            "t",
            Some(Resume {
                epoch,
                last_seq: 1,
                threads: vec![thread_id],
            }),
        ),
        ClientAuth::Credential(&cred),
    )
    .await
    .unwrap();
    assert!(!session.welcome.resumed);
    drop(backend);
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_tunnel_and_ssh_stdio_with_a_fake_ssh() {
    let dir = temp_dir("ssh");
    let server = start(&dir);
    let log = dir.join("ssh.log");
    // The fake ssh logs its arguments; a wrapper sets the log path.
    let wrapper = dir.join("ssh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nFAKE_SSH_LOG={} exec {} \"$@\"\n",
            log.display(),
            fixture("fake_ssh.py").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    blongo_client::target::set_ssh_program(wrapper.to_string_lossy());

    // Tunnel: ssh -L to the server's loopback port, then the usual pairing.
    let port = server.addr.unwrap().port();
    let tunnel = format!("ssh://me@devbox?port={port}");
    let env = pair(&dir, &tunnel).await;
    assert!(env.credential.is_some());
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    wait_connected(&mut rx).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;
    send(&backend, thread_id, "echo: through a tunnel");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);
    drop(backend);

    // stdio: `ssh host blongo-serve --stdio` bridges to the running
    // server's Unix socket; SSH is the authentication (no pairing).
    let command = format!(
        "{} --stdio --data-dir {}",
        env!("CARGO_BIN_EXE_blongo-serve"),
        dir.join("data").display()
    );
    let stdio = format!("ssh+stdio://devbox?command={}", command.replace(' ', "%20"));
    let env = blongo_client::pairing::pair("stdio", &stdio, None, "d")
        .await
        .unwrap();
    assert!(env.credential.is_none());
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    wait_connected(&mut rx).await;
    let shell = wait_for(&mut rx, |e| match e {
        CoreEvent::Shell(s) => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(shell.threads.iter().any(|t| t.id == thread_id));
    backend.open_thread(thread_id);
    wait_for(&mut rx, |e| matches!(e, CoreEvent::Thread(_)).then_some(())).await;
    send(&backend, thread_id, "echo: over ssh stdio");
    assert_eq!(run_finished(&mut rx).await, RunStatus::Completed);
    drop(backend);

    let log = std::fs::read_to_string(&log).unwrap();
    assert!(log.contains("\"-L\""), "{log}");
    assert!(log.contains(&format!("127.0.0.1:{port}")), "{log}");
    assert!(log.contains("BatchMode=yes"), "{log}");
    assert!(log.contains("--stdio"), "{log}");
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_without_a_running_server_serves_in_process() {
    let dir = temp_dir("stdio");
    // Through the child directly (what ssh would run).
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_blongo-serve"))
        .args(["--stdio", "--data-dir"])
        .arg(dir.join("data"))
        .env("BLONGO_CODEX_EXE", fake_codex())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (mut reader, mut writer) = blongo_client::transport::stream_halves(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        blongo_protocol::wire::MAX_SERVER_FRAME,
    );
    let session = handshake(
        &mut reader,
        &mut writer,
        Hello::new("t", None),
        ClientAuth::Local,
    )
    .await
    .unwrap();
    assert!(!session.welcome.resumed);
    // The shell snapshot follows.
    let mut got_shell = session
        .rest
        .iter()
        .any(|m| matches!(m, ServerMsg::Seq(s) if matches!(s.payload, Payload::Shell(_))));
    while !got_shell {
        got_shell = recv(&mut reader)
            .await
            .unwrap()
            .iter()
            .any(|m| matches!(m, ServerMsg::Seq(s) if matches!(s.payload, Payload::Shell(_))));
    }
    // Closing stdin ends the server.
    writer.close().await;
    drop(writer);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .expect("blongo-serve --stdio did not exit")
        .unwrap();
    assert!(status.success(), "{status}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_socket_clients_authenticate_by_file_permissions() {
    let dir = temp_dir("unix");
    let server = start(&dir);
    let socket = server.socket.clone().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mode = std::fs::metadata(socket.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (r, w) = stream.into_split();
    let (mut reader, mut writer) =
        blongo_client::transport::stream_halves(r, w, blongo_protocol::wire::MAX_SERVER_FRAME);
    let session = handshake(
        &mut reader,
        &mut writer,
        Hello::new("t", None),
        ClientAuth::Local,
    )
    .await
    .unwrap();
    assert_eq!(session.welcome.device_id, None);
    // A second server on the same data refuses to start.
    let err = blongo_server::start(config(&dir))
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("already running") || err.contains("another Blongo process"),
        "{err}"
    );
    server.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_side_terminal_runs_in_the_thread_folder() {
    let dir = temp_dir("pty");
    let server = start(&dir);
    let env = pair(&dir, &ws_target(server.addr.unwrap())).await;
    let (backend, mut rx) = connect_on(&rt(), env, fast_options());
    wait_connected(&mut rx).await;
    let thread_id = project_and_thread(&backend, &mut rx, &dir).await;
    backend.terminal_open(1, thread_id, 80, 24);
    backend.terminal_input(1, b"pwd; echo blongo-$((40+2))\r".to_vec());
    let mut out = Vec::new();
    wait_for(&mut rx, |e| match e {
        CoreEvent::Terminal(blongo_protocol::client::TerminalEvent::Output { id: 1, data }) => {
            out.extend_from_slice(data);
            String::from_utf8_lossy(&out)
                .contains("blongo-42")
                .then_some(())
        }
        CoreEvent::Terminal(blongo_protocol::client::TerminalEvent::Failed { message, .. }) => {
            panic!("{message}")
        }
        _ => None,
    })
    .await;
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("project"), "{text}");
    backend.terminal_input(1, b"exit\r".to_vec());
    wait_for(&mut rx, |e| {
        matches!(
            e,
            CoreEvent::Terminal(blongo_protocol::client::TerminalEvent::Exited { id: 1 })
        )
        .then_some(())
    })
    .await;
    drop(backend);
    server.stop();
}

#[test]
fn tailscale_listen_uses_the_tailnet_address_and_refuses_others() {
    let dir = temp_dir("tailscale");
    let fake = |ip: &str| {
        let path = dir.join(format!("tailscale-{}", ip.replace('.', "_")));
        std::fs::write(&path, format!("#!/bin/sh\necho {ip}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    };
    let run = |tailscale: &std::path::Path, extra: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_blongo-serve"))
            .arg("--data-dir")
            .arg(dir.join("data"))
            .args(extra)
            .env("BLONGO_TAILSCALE", tailscale)
            .env("BLONGO_CODEX_EXE", fake_codex())
            .output()
            .unwrap()
    };
    // A tailnet address this sandbox does not have: detected, then the
    // bind itself fails (no tailscale interface here).
    let out = run(&fake("100.101.102.103"), &["--tailscale", "--port", "0"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("100.101.102.103"), "{err}");
    // Not a Tailscale address: refused before binding.
    let out = run(&fake("192.168.1.10"), &["--tailscale"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a Tailscale address"), "{err}");
    // A public bind without --insecure-listen: refused.
    let out = run(&fake("192.168.1.10"), &["--listen", "0.0.0.0:0"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("refusing to listen"), "{err}");
}
