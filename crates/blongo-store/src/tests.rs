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
    let thread = Thread::new(ThreadId::new(), project.id, "New thread", Timestamp(2));
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
    Run::new(
        RunId::new(),
        thread,
        RunStatus::Starting,
        ProviderKind::Codex,
        Timestamp(3),
    )
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
fn completed_effects_leave_the_outbox() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let CommitOutcome::Committed { outbox, .. } = store
        .commit(Batch {
            command_id: Some(CommandId::new()),
            events: vec![],
            effects: vec![Effect::ProviderInterrupt {
                thread_id: f.thread.id,
                run_id: RunId::new(),
            }],
        })
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(store.pending_effects().unwrap().len(), 1);
    store.complete_effect(outbox[0].id).unwrap();
    assert!(store.pending_effects().unwrap().is_empty());
    assert_eq!(store.prune_effects().unwrap(), 0, "row already gone");
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
    let thread = Thread::new(ThreadId::new(), ProjectId::new(), "orphan", Timestamp(1));
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

#[test]
fn queued_and_rolled_back_runs_and_provider_changes() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let thread_id = f.thread.id;
    let status = |store: &Store| store.thread(thread_id).unwrap().unwrap().status;

    // First run running; a second one queued behind it leaves the thread
    // running, and cancelling a queued run does not make it idle.
    let first = run(thread_id);
    let first_msg = item(thread_id, first.id, 0, ItemKind::UserMessage, "one");
    store
        .commit(
            Batch::default()
                .event(EventKind::RunCreated { run: first.clone() })
                .event(EventKind::ItemAdded {
                    item: Arc::new(first_msg),
                }),
        )
        .unwrap();
    let mut queued = run(thread_id);
    queued.status = RunStatus::Queued;
    store
        .commit(Batch::default().event(EventKind::RunCreated {
            run: queued.clone(),
        }))
        .unwrap();
    assert_eq!(status(&store), ThreadStatus::Running);
    store
        .commit(Batch::default().event(EventKind::RunStatusChanged {
            thread_id,
            run_id: queued.id,
            status: RunStatus::Cancelled,
            error: None,
        }))
        .unwrap();
    assert_eq!(status(&store), ThreadStatus::Running);

    // Provider turn id and checkpoint are recorded on the run.
    store
        .commit(
            Batch::default()
                .event(EventKind::RunCheckpointed {
                    thread_id,
                    run_id: first.id,
                    commit: "abc123".into(),
                })
                .event(EventKind::RunProviderTurn {
                    thread_id,
                    run_id: first.id,
                    provider_turn_id: "turn-1".into(),
                })
                .event(EventKind::RunStatusChanged {
                    thread_id,
                    run_id: first.id,
                    status: RunStatus::Completed,
                    error: None,
                }),
        )
        .unwrap();
    let stored = store.run(first.id).unwrap().unwrap();
    assert_eq!(stored.checkpoint.as_deref(), Some("abc123"));
    assert_eq!(stored.provider_turn_id.as_deref(), Some("turn-1"));
    assert_eq!(status(&store), ThreadStatus::Idle);
    assert_eq!(store.items(thread_id).unwrap().len(), 1);

    // Rolling the run back hides its items without touching the thread.
    store
        .commit(Batch::default().event(EventKind::RunStatusChanged {
            thread_id,
            run_id: first.id,
            status: RunStatus::RolledBack,
            error: None,
        }))
        .unwrap();
    assert!(store.items(thread_id).unwrap().is_empty());
    assert_eq!(status(&store), ThreadStatus::Idle);

    // Provider switch: new provider, unbound, handoff pending; binding a
    // provider thread clears the pending context.
    store
        .commit(Batch::default().event(EventKind::ThreadProviderChanged {
            thread_id,
            provider: ProviderKind::ClaudeCode,
            model: Some("m".into()),
            provider_thread_id: None,
            pending_context: Some(PendingContext::Handoff),
        }))
        .unwrap();
    let t = store.thread(thread_id).unwrap().unwrap();
    assert_eq!(t.provider, ProviderKind::ClaudeCode);
    assert_eq!(t.model.as_deref(), Some("m"));
    assert_eq!(t.pending_context, Some(PendingContext::Handoff));
    store
        .commit(Batch::default().event(EventKind::ThreadProviderBound {
            thread_id,
            provider_thread_id: "sess".into(),
        }))
        .unwrap();
    let t = store.thread(thread_id).unwrap().unwrap();
    assert_eq!(t.pending_context, None);
    assert_eq!(t.provider_thread_id.as_deref(), Some("sess"));
}

#[test]
fn thread_fields_round_trip() {
    let mut store = Store::open_in_memory().unwrap();
    let f = fixture(&mut store);
    let mut thread = Thread::new(ThreadId::new(), f.project.id, "fork", Timestamp(9));
    thread.provider = ProviderKind::Antigravity;
    thread.worktree = Some(Worktree {
        path: "/tmp/wt".into(),
        branch: "blongo/x".into(),
    });
    thread.forked_from = Some(f.thread.id);
    thread.pending_context = Some(PendingContext::Fork {
        provider_thread_id: "p".into(),
        up_to_turn: Some("t".into()),
    });
    store
        .commit(Batch::default().event(EventKind::ThreadCreated {
            thread: thread.clone(),
        }))
        .unwrap();
    assert_eq!(store.thread(thread.id).unwrap().unwrap(), thread);
}
