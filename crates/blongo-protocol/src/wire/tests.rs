use super::*;
use crate::{
    ApprovalDecision, ApprovalState, Command, Delivery, ItemKind, PlanStatus, PlanStep, Project,
    ProjectId, RunId, Thread, Timestamp, ToolStatus,
};

fn item(kind: ItemKind, text: &str) -> TurnItem {
    TurnItem {
        id: ItemId::new(),
        thread_id: ThreadId::new(),
        run_id: Some(RunId::new()),
        ordinal: 7,
        created_at: Timestamp(42),
        kind,
        text: text.into(),
    }
}

fn round_trip_server(msgs: Vec<ServerMsg>) -> Vec<ServerMsg> {
    let frame = ServerFrame(msgs);
    let bytes = encode(&frame, MAX_SERVER_FRAME).unwrap();
    let back: ServerFrame = decode(&bytes, MAX_SERVER_FRAME).unwrap();
    back.0
}

fn all_item_kinds() -> Vec<ItemKind> {
    vec![
        ItemKind::UserMessage,
        ItemKind::AssistantMessage { streaming: true },
        ItemKind::Reasoning { streaming: false },
        ItemKind::CommandExecution {
            call_id: "c".into(),
            command: "ls -la".into(),
            status: ToolStatus::Completed,
            output: "a\nb\n".into(),
            exit_code: Some(0),
        },
        ItemKind::FileChange {
            call_id: "f".into(),
            paths: vec!["src/main.rs".into()],
            status: ToolStatus::Running,
        },
        ItemKind::ToolCall {
            call_id: "t".into(),
            name: "web".into(),
            input: "{}".into(),
            status: ToolStatus::Failed,
            output: String::new(),
        },
        ItemKind::ApprovalRequest {
            provider_request_id: "0".into(),
            title: "Run ls?".into(),
            detail: "ls".into(),
            state: ApprovalState::Pending,
        },
        ItemKind::Plan {
            steps: vec![PlanStep {
                text: "step".into(),
                status: PlanStatus::InProgress,
            }],
        },
        ItemKind::SystemNotice {
            message: "note".into(),
        },
        ItemKind::Error {
            message: "boom".into(),
        },
    ]
}

#[test]
fn every_item_kind_round_trips_with_its_body() {
    for kind in all_item_kinds() {
        let it = Arc::new(item(kind, "body text — 日本語 ✓"));
        let event = DomainEvent {
            sequence: 9,
            at: Timestamp(5),
            command_id: Some(CommandId::new()),
            kind: EventKind::ItemAdded { item: it.clone() },
        };
        let msgs = round_trip_server(vec![ServerMsg::Seq(Sequenced {
            seq: 3,
            payload: Payload::Event(WireEvent::new(Arc::new(event.clone()))),
        })]);
        let [ServerMsg::Seq(Sequenced { seq, payload })] = msgs.as_slice() else {
            panic!("{msgs:?}")
        };
        assert_eq!(*seq, 3);
        let Payload::Event(wire) = payload.clone() else {
            panic!()
        };
        let back = wire.into_event();
        assert_eq!(back, event);
        let EventKind::ItemAdded { item: back_item } = back.kind else {
            panic!()
        };
        assert_eq!(&*back_item.text, "body text — 日本語 ✓");
    }
}

#[test]
fn snapshots_and_deltas_round_trip() {
    let project = Project {
        id: ProjectId::new(),
        name: "p".into(),
        path: "/tmp/p".into(),
        created_at: Timestamp(1),
    };
    let thread = Thread::new(ThreadId::new(), project.id, "t", Timestamp(2));
    let shell = ShellSnapshot {
        sequence: 10,
        projects: vec![project.clone()],
        threads: vec![thread.clone()],
        schedules: vec![crate::Schedule {
            id: crate::ScheduleId::new(),
            project_id: project.id,
            thread_id: Some(thread.id),
            cron: "*/5 * * * *".into(),
            prompt: "check".into(),
            provider: ProviderKind::Codex,
            enabled: true,
            created_at: Timestamp(4),
            next_run_at: Some(Timestamp(5)),
            last_run_at: None,
            last_thread_id: None,
            proposed_by: None,
        }],
    };
    let run = Run::new(
        RunId::new(),
        thread.id,
        RunStatus::Running,
        ProviderKind::Codex,
        Timestamp(3),
    );
    let snapshot = ThreadSnapshot {
        thread_id: thread.id,
        sequence: 11,
        runs: vec![run],
        items: all_item_kinds()
            .into_iter()
            .enumerate()
            .map(|(i, k)| Arc::new(item(k, &format!("text {i}"))))
            .collect(),
    };
    let msgs = round_trip_server(vec![
        ServerMsg::Seq(Sequenced {
            seq: 1,
            payload: Payload::Shell(shell.clone()),
        }),
        ServerMsg::Seq(Sequenced {
            seq: 2,
            payload: Payload::Thread(WireThread::from(&snapshot)),
        }),
        ServerMsg::Seq(Sequenced {
            seq: 3,
            payload: Payload::TextDelta {
                thread_id: thread.id,
                item_id: ItemId::new(),
                chunk: "chunk".into(),
            },
        }),
        ServerMsg::Models {
            provider: ProviderKind::ClaudeCode,
            models: vec![ModelInfo {
                id: "m".into(),
                label: "M".into(),
            }],
        },
        ServerMsg::Imported(Ok(ImportReport::default())),
        ServerMsg::Login {
            provider: ProviderKind::Antigravity,
            state: LoginState::Url("https://example.invalid".into()),
        },
        ServerMsg::Refused {
            code: RefuseCode::Unauthorized,
            message: "no".into(),
        },
        ServerMsg::TerminalOutput {
            id: 1,
            data: vec![0, 1, 2, 255],
        },
    ]);
    let ServerMsg::Seq(Sequenced {
        payload: Payload::Shell(back_shell),
        ..
    }) = &msgs[0]
    else {
        panic!()
    };
    assert_eq!(back_shell, &shell);
    let ServerMsg::Seq(Sequenced {
        payload: Payload::Thread(back),
        ..
    }) = &msgs[1]
    else {
        panic!()
    };
    let back = ThreadSnapshot::from(back.clone());
    assert_eq!(back, snapshot);
    for (a, b) in back.items.iter().zip(&snapshot.items) {
        assert_eq!(a.text, b.text);
    }
    assert_eq!(msgs.len(), 8);
}

#[test]
fn client_messages_round_trip() {
    let msgs = vec![
        ClientMsg::Hello(Hello::new(
            "test",
            Some(Resume {
                epoch: 1,
                last_seq: 99,
                threads: vec![ThreadId::new()],
            }),
        )),
        ClientMsg::Auth(AuthRequest::Token {
            device_id: "d".into(),
            proof: Proof {
                iat: 5,
                jti: vec![1; 16],
                signature: vec![2; 64],
            },
        }),
        ClientMsg::Auth(AuthRequest::Local),
        ClientMsg::Command(CommandEnvelope::new(Command::MessageDispatch {
            thread_id: ThreadId::new(),
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: "hello there".into(),
            delivery: Delivery::Steer,
        })),
        ClientMsg::Command(CommandEnvelope::new(Command::RuntimeRequestRespond {
            thread_id: ThreadId::new(),
            item_id: ItemId::new(),
            decision: ApprovalDecision::Approve,
        })),
        ClientMsg::Subscribe {
            threads: vec![ThreadId::new()],
        },
        ClientMsg::TerminalInput {
            id: 3,
            data: b"ls\r".to_vec(),
        },
        ClientMsg::Revoke {
            device: "laptop".into(),
        },
    ];
    for msg in msgs {
        let bytes = encode(&msg, MAX_CLIENT_FRAME).unwrap();
        let back: ClientMsg = decode(&bytes, MAX_CLIENT_FRAME).unwrap();
        assert_eq!(back, msg);
    }
}

#[test]
fn ids_are_compact_binary() {
    // 16-byte UUIDs, not 36-character strings.
    let msg = ClientMsg::Subscribe {
        threads: vec![ThreadId::new()],
    };
    let bytes = encode(&msg, MAX_CLIENT_FRAME).unwrap();
    assert!(bytes.len() < 40, "{} bytes", bytes.len());
}

/// xorshift: deterministic pseudo-random bytes without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn garbage_and_mutated_frames_never_panic() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let valid = encode(
        &ClientMsg::Command(CommandEnvelope::new(Command::ThreadRename {
            thread_id: ThreadId::new(),
            title: "x".repeat(40),
        })),
        MAX_CLIENT_FRAME,
    )
    .unwrap();
    for round in 0..20_000u32 {
        let bytes: Vec<u8> = if round.is_multiple_of(2) {
            let len = (rng.next() % 200) as usize;
            (0..len).map(|_| rng.next() as u8).collect()
        } else {
            let mut b = valid.clone();
            for _ in 0..(1 + rng.next() % 4) {
                let at = (rng.next() as usize) % b.len();
                b[at] = rng.next() as u8;
            }
            if rng.next().is_multiple_of(3) {
                b.truncate((rng.next() as usize) % b.len());
            }
            b
        };
        let _ = decode::<ClientMsg>(&bytes, MAX_CLIENT_FRAME);
        let _ = decode::<ServerFrame>(&bytes, MAX_SERVER_FRAME);
    }
}

#[test]
fn oversized_frames_are_refused() {
    let big = ClientMsg::Command(CommandEnvelope::new(Command::ThreadRename {
        thread_id: ThreadId::new(),
        title: "x".repeat(MAX_CLIENT_FRAME),
    }));
    assert!(matches!(
        encode(&big, MAX_CLIENT_FRAME),
        Err(WireError::TooLarge { .. })
    ));
    assert!(matches!(
        decode::<ClientMsg>(&vec![0; MAX_CLIENT_FRAME + 1], MAX_CLIENT_FRAME),
        Err(WireError::TooLarge { .. })
    ));
}

#[test]
fn frame_reader_splits_reassembles_and_bounds() {
    let a = length_prefixed(b"hello");
    let b = length_prefixed(&[7u8; 300]);
    let mut stream = [a, b].concat();
    stream.extend_from_slice(&length_prefixed(b""));
    let mut reader = FrameReader::new(1024);
    let mut frames = Vec::new();
    // Byte by byte: frames come out whole and in order.
    for byte in &stream {
        reader.push(std::slice::from_ref(byte));
        while let Some(f) = reader.next_frame().unwrap() {
            frames.push(f);
        }
    }
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0], b"hello");
    assert_eq!(frames[1], vec![7u8; 300]);
    assert!(frames[2].is_empty());
    assert_eq!(reader.buffered(), 0);

    // A length over the limit fails before the body is buffered.
    let mut reader = FrameReader::new(1024);
    reader.push(&(1025u32).to_be_bytes());
    assert!(matches!(
        reader.next_frame(),
        Err(WireError::TooLarge { len: 1025, .. })
    ));
}

#[test]
fn proof_message_binds_every_input() {
    let base = proof_message(
        ProofPurpose::Token,
        "server",
        &[1; 32],
        100,
        &[2; 16],
        &secret_sha256("token"),
        &[3; 32],
    );
    let variants = [
        proof_message(
            ProofPurpose::Pair,
            "server",
            &[1; 32],
            100,
            &[2; 16],
            &secret_sha256("token"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server2",
            &[1; 32],
            100,
            &[2; 16],
            &secret_sha256("token"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server",
            &[9; 32],
            100,
            &[2; 16],
            &secret_sha256("token"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server",
            &[1; 32],
            101,
            &[2; 16],
            &secret_sha256("token"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server",
            &[1; 32],
            100,
            &[8; 16],
            &secret_sha256("token"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server",
            &[1; 32],
            100,
            &[2; 16],
            &secret_sha256("token2"),
            &[3; 32],
        ),
        proof_message(
            ProofPurpose::Token,
            "server",
            &[1; 32],
            100,
            &[2; 16],
            &secret_sha256("token"),
            &[4; 32],
        ),
    ];
    for v in variants {
        assert_ne!(v, base);
    }
    // The secret itself never appears in what is signed.
    let secret = "a-very-distinctive-secret";
    let msg = proof_message(
        ProofPurpose::Token,
        "s",
        &[],
        0,
        &[],
        &secret_sha256(secret),
        &[],
    );
    assert!(!msg.windows(secret.len()).any(|w| w == secret.as_bytes()));
}

#[test]
fn versions_and_capabilities_negotiate() {
    let mut hello = Hello::new("t", None);
    assert_eq!(negotiate_version(&hello), Some(PROTOCOL_VERSION));
    hello.min_version = PROTOCOL_VERSION + 1;
    hello.version = PROTOCOL_VERSION + 3;
    assert_eq!(negotiate_version(&hello), None);
    hello.min_version = 0;
    assert_eq!(negotiate_version(&hello), Some(PROTOCOL_VERSION));
    assert_eq!(
        negotiate_caps(&["import".into(), "teleport".into()]),
        vec!["import".to_string()]
    );
}

#[test]
fn timeline_routing() {
    let it = Arc::new(item(ItemKind::UserMessage, "x"));
    let tid = it.thread_id;
    let added = Payload::Event(WireEvent::new(Arc::new(DomainEvent {
        sequence: 1,
        at: Timestamp(0),
        command_id: None,
        kind: EventKind::ItemAdded { item: it },
    })));
    assert_eq!(added.timeline_thread(), Some(tid));
    let status = Payload::Event(WireEvent::new(Arc::new(DomainEvent {
        sequence: 2,
        at: Timestamp(0),
        command_id: None,
        kind: EventKind::RunStatusChanged {
            thread_id: tid,
            run_id: RunId::new(),
            status: RunStatus::Completed,
            error: None,
        },
    })));
    // Run status drives the sidebar dot: everyone gets it.
    assert_eq!(status.timeline_thread(), None);
}

#[test]
fn queries_and_every_reply_round_trip() {
    use crate::workspace::*;
    let thread_id = ThreadId::new();
    let queries = vec![
        Query::DiffSummary {
            thread_id,
            scope: DiffScope::Turn {
                run_id: RunId::new(),
            },
        },
        Query::DiffSummary {
            thread_id,
            scope: DiffScope::Thread,
        },
        Query::DiffFile {
            thread_id,
            from: "a".repeat(40),
            to: "b".repeat(40),
            path: "src/x.rs".into(),
            max_lines: 100,
        },
        Query::SearchFiles {
            thread_id,
            pattern: "x".into(),
            limit: 5,
        },
        Query::GitSwitch {
            thread_id,
            branch: "b".into(),
            create: true,
        },
        Query::GitCommit {
            thread_id,
            message: "m".into(),
        },
    ];
    for query in queries {
        let msg = ClientMsg::Query { id: 9, query };
        let bytes = encode(&msg, MAX_CLIENT_FRAME).unwrap();
        let back: ClientMsg = decode(&bytes, MAX_CLIENT_FRAME).unwrap();
        assert_eq!(back, msg);
    }
    let replies = vec![
        Ok(QueryReply::DiffSummary(DiffSummary {
            from: "a".into(),
            to: "b".into(),
            files: vec![DiffFileStat {
                path: "x".into(),
                old_path: Some("y".into()),
                status: 'R',
                added: 1,
                removed: 2,
                binary: false,
            }],
            truncated: false,
            added: 1,
            removed: 2,
        })),
        Ok(QueryReply::DiffFile(parse_unified_diff(
            "x",
            "@@ -1 +1 @@\n-a\n+b\n",
            10,
        ))),
        Ok(QueryReply::Files(vec![FileMatch {
            path: "x".into(),
            score: 3,
            positions: vec![0],
        }])),
        Ok(QueryReply::Dir(vec![DirEntry {
            name: "src".into(),
            is_dir: true,
        }])),
        Ok(QueryReply::File(FileContent {
            path: "x".into(),
            text: "hi".into(),
            truncated: false,
            binary: false,
        })),
        Ok(QueryReply::GitStatus(GitStatusInfo {
            branch: Some("main".into()),
            upstream: None,
            ahead: 1,
            behind: 0,
            changes: vec![("??".into(), "x".into())],
        })),
        Ok(QueryReply::Branches(vec![BranchInfo {
            name: "main".into(),
            current: true,
        }])),
        Ok(QueryReply::Done("ok".into())),
        Err("nope".into()),
    ];
    let msgs: Vec<ServerMsg> = replies
        .into_iter()
        .map(|result| ServerMsg::Reply { id: 1, result })
        .chain([ServerMsg::TerminalInputDropped { id: 2 }])
        .collect();
    assert_eq!(round_trip_server(msgs.clone()), msgs);
}
