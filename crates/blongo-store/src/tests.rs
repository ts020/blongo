use super::*;
use blongo_protocol::{ApprovalState, ToolStatus};

struct Fixture {
    project: Project,
    thread: Thread,
}

fn fixture(store: &mut Store) -> Fixture {
    let project = Project {
        id: ProjectId::new(),
        name: "demo".into(),
        path: "/tmp/demo".into(),
        created_at: Timestamp(1),
    };
    let thread = Thread {
        id: ThreadId::new(),
        project_id: project.id,
        title: "New thread".into(),
        status: ThreadStatus::Idle,
        archived: false,
        created_at: Timestamp(2),
        updated_at: Timestamp(2),
        provider_thread_id: None,
    };
    store
        .commit(Batch {
            command_id: Some(CommandId::new()),
            events: vec![
                EventKind::ProjectCreated {
                    project: project.clone(),
                },
                EventKind::ThreadCreated {
                    thread: thread.clone(),
                },
            ],
            effects: vec![],
        })
        .unwrap();
    Fixture { project, thread }
}

fn item(thread: ThreadId, run: RunId, ordinal: u32, kind: ItemKind, text: &str) -> TurnItem {
    TurnItem {
        id: ItemId::new(),
        thread_id: thread,
        run_id: Some(run),
        ordinal,
        created_at: Timestamp(3),
        kind,
        text: text.into(),
    }
}

fn run(thread: ThreadId) -> Run {
    Run {
        id: RunId::new(),
        thread_id: thread,
        parent_run_id: None,
        status: RunStatus::Starting,
        created_at: Timestamp(3),
        ended_at: None,
        error: None,
    }
}

fn committed(outcome: CommitOutcome) -> (Vec<DomainEvent>, Vec<OutboxRow>) {
    match outcome {
        CommitOutcome::Committed { events, outbox } => (events, outbox),
        CommitOutcome::Duplicate(r) => panic!("unexpected duplicate {r:?}"),
    }
}

#[test]
fn commit_writes_events_projections_receipt_and_outbox_together() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    assert_eq!(store.last_sequence(), 2);

    let run = run(f.thread.id);
    let msg = item(f.thread.id, run.id, 0, ItemKind::UserMessage, "hello");
    let command_id = CommandId::new();
    let (events, outbox) = committed(
        store
            .commit(Batch {
                command_id: Some(command_id),
                events: vec![
                    EventKind::RunCreated { run: run.clone() },
                    EventKind::ItemAdded {
                        item: Arc::new(msg.clone()),
                    },
                ],
                effects: vec![Effect::ProviderTurnStart {
                    thread_id: f.thread.id,
                    run_id: run.id,
                    message_id: msg.id,
                }],
            })
            .unwrap(),
    );
    assert_eq!(
        events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        [3, 4]
    );
    assert!(events.iter().all(|e| e.command_id == Some(command_id)));
    assert_eq!(outbox.len(), 1);
    assert_eq!(store.pending_effects().unwrap(), outbox);

    // Projections.
    assert_eq!(store.projects().unwrap(), vec![f.project.clone()]);
    let thread = store.thread(f.thread.id).unwrap().unwrap();
    assert_eq!(thread.status, ThreadStatus::Running);
    assert_eq!(store.runs(f.thread.id).unwrap(), vec![run.clone()]);
    let items = store.items(f.thread.id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(&*items[0].text, "hello");
    assert_eq!(store.next_ordinal(f.thread.id).unwrap(), 1);

    // Receipt.
    let receipt = store.receipt(command_id).unwrap().unwrap();
    assert_eq!(receipt.sequences, Some((3, 4)));

    // The log has no bodies, and replays to the same kinds.
    let log = store.events_after(2, 100).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].kind, events[0].kind);
    let EventKind::ItemAdded { item: logged } = &log[1].kind else {
        panic!()
    };
    assert_eq!(&*logged.text, "");
    assert_eq!(logged.kind, ItemKind::UserMessage);

    store
        .set_effect_status(outbox[0].id, EffectStatus::Done)
        .unwrap();
    assert!(store.pending_effects().unwrap().is_empty());
    assert_eq!(store.prune_effects().unwrap(), 1);
}

#[test]
fn replayed_command_is_a_no_op() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let command_id = CommandId::new();
    let batch = || Batch {
        command_id: Some(command_id),
        events: vec![EventKind::ThreadRenamed {
            thread_id: f.thread.id,
            title: "Renamed".into(),
        }],
        effects: vec![],
    };
    committed(store.commit(batch()).unwrap());
    let seq = store.last_sequence();
    match store.commit(batch()).unwrap() {
        CommitOutcome::Duplicate(receipt) => {
            assert_eq!(receipt.sequences, Some((seq, seq)));
        }
        other => panic!("expected duplicate, got {other:?}"),
    }
    assert_eq!(store.last_sequence(), seq);
    assert_eq!(store.events_after(0, 100).unwrap().len() as u64, seq);
}

#[test]
fn failed_commit_writes_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let before = store.last_sequence();
    let command_id = CommandId::new();
    // The second event references a thread that does not exist: the whole
    // batch (first event, receipt, outbox row) must roll back.
    let err = store.commit(Batch {
        command_id: Some(command_id),
        events: vec![
            EventKind::ThreadRenamed {
                thread_id: f.thread.id,
                title: "should not stick".into(),
            },
            EventKind::ThreadRenamed {
                thread_id: ThreadId::new(),
                title: "x".into(),
            },
        ],
        effects: vec![Effect::ProviderInterrupt {
            thread_id: f.thread.id,
            run_id: RunId::new(),
        }],
    });
    assert!(err.is_err());
    assert_eq!(store.last_sequence(), before);
    assert_eq!(store.events_after(0, 100).unwrap().len() as u64, before);
    assert_eq!(
        store.thread(f.thread.id).unwrap().unwrap().title,
        "New thread"
    );
    assert!(store.receipt(command_id).unwrap().is_none());
    assert!(store.pending_effects().unwrap().is_empty());
    // And the store is still usable, continuing the same sequence.
    committed(
        store
            .commit(Batch::default().event(EventKind::ThreadArchived {
                thread_id: f.thread.id,
            }))
            .unwrap(),
    );
    assert_eq!(store.last_sequence(), before + 1);
    assert!(store.threads(false).unwrap().is_empty());
    assert_eq!(store.threads(true).unwrap().len(), 1);
}

#[test]
fn foreign_keys_are_enforced() {
    let mut store = Store::open_in_memory().unwrap();
    let thread = Thread {
        id: ThreadId::new(),
        project_id: ProjectId::new(),
        title: "orphan".into(),
        status: ThreadStatus::Idle,
        archived: false,
        created_at: Timestamp(1),
        updated_at: Timestamp(1),
        provider_thread_id: None,
    };
    assert!(
        store
            .commit(Batch::default().event(EventKind::ThreadCreated { thread }))
            .is_err()
    );
}

#[test]
fn streaming_text_and_item_lifecycle() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let run = run(f.thread.id);
    let msg = item(
        f.thread.id,
        run.id,
        0,
        ItemKind::AssistantMessage { streaming: true },
        "",
    );
    let approval = item(
        f.thread.id,
        run.id,
        1,
        ItemKind::ApprovalRequest {
            provider_request_id: "0".into(),
            title: "Run command?".into(),
            detail: "ls".into(),
            state: ApprovalState::Pending,
        },
        "",
    );
    committed(
        store
            .commit(Batch {
                command_id: None,
                events: vec![
                    EventKind::RunCreated { run: run.clone() },
                    EventKind::ItemAdded {
                        item: Arc::new(msg.clone()),
                    },
                    EventKind::ItemAdded {
                        item: Arc::new(approval.clone()),
                    },
                ],
                effects: vec![],
            })
            .unwrap(),
    );
    for chunk in ["Hel", "lo ", "wörld"] {
        committed(
            store
                .commit(Batch::default().event(EventKind::ItemTextAppended {
                    thread_id: f.thread.id,
                    item_id: msg.id,
                    chunk: chunk.into(),
                    len: 0,
                }))
                .unwrap(),
        );
    }
    assert_eq!(store.open_items(run.id).unwrap().len(), 2);
    let mut answered = approval.clone();
    if let ItemKind::ApprovalRequest { state, .. } = &mut answered.kind {
        *state = ApprovalState::Approved;
    }
    let tool = item(
        f.thread.id,
        run.id,
        2,
        ItemKind::CommandExecution {
            call_id: "c1".into(),
            command: "ls".into(),
            status: ToolStatus::Running,
            output: String::new(),
            exit_code: None,
        },
        "",
    );
    committed(
        store
            .commit(Batch {
                command_id: None,
                events: vec![
                    EventKind::ItemFinished {
                        thread_id: f.thread.id,
                        item_id: msg.id,
                    },
                    EventKind::ItemUpdated {
                        item: Arc::new(answered),
                    },
                    EventKind::ItemAdded {
                        item: Arc::new(tool.clone()),
                    },
                    EventKind::RunStatusChanged {
                        thread_id: f.thread.id,
                        run_id: run.id,
                        status: RunStatus::Waiting,
                        error: None,
                    },
                ],
                effects: vec![],
            })
            .unwrap(),
    );
    assert_eq!(
        store.thread(f.thread.id).unwrap().unwrap().status,
        ThreadStatus::Waiting
    );
    let open = store.open_items(run.id).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, tool.id);
    let stored = store.item(msg.id).unwrap().unwrap();
    assert_eq!(&*stored.text, "Hello wörld");
    assert_eq!(stored.kind, ItemKind::AssistantMessage { streaming: false });
    assert_eq!(store.unfinished_runs().unwrap().len(), 1);

    committed(
        store
            .commit(Batch::default().event(EventKind::RunStatusChanged {
                thread_id: f.thread.id,
                run_id: run.id,
                status: RunStatus::Completed,
                error: None,
            }))
            .unwrap(),
    );
    let runs = store.runs(f.thread.id).unwrap();
    assert!(runs[0].ended_at.is_some());
    assert!(store.unfinished_runs().unwrap().is_empty());
    assert_eq!(
        store.thread(f.thread.id).unwrap().unwrap().status,
        ThreadStatus::Idle
    );
}

#[test]
fn child_run_does_not_move_the_thread() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let root = run(f.thread.id);
    let child = Run {
        parent_run_id: Some(root.id),
        ..run(f.thread.id)
    };
    committed(
        store
            .commit(Batch {
                command_id: None,
                events: vec![
                    EventKind::RunCreated { run: root.clone() },
                    EventKind::RunCreated { run: child.clone() },
                    EventKind::RunStatusChanged {
                        thread_id: f.thread.id,
                        run_id: child.id,
                        status: RunStatus::Completed,
                        error: None,
                    },
                ],
                effects: vec![],
            })
            .unwrap(),
    );
    assert_eq!(
        store.thread(f.thread.id).unwrap().unwrap().status,
        ThreadStatus::Running
    );
}

#[test]
fn reopen_file_database_keeps_state_and_sequence() {
    let dir = std::env::temp_dir().join(format!("blongo-store-test-{}", ThreadId::new()));
    let path = dir.join("blongo.sqlite");
    let thread_id;
    {
        let mut store = Store::open(&path).unwrap();
        thread_id = fixture(&mut store).thread.id;
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), LATEST_VERSION);
    assert_eq!(store.last_sequence(), 2);
    assert_eq!(store.threads(false).unwrap()[0].id, thread_id);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
