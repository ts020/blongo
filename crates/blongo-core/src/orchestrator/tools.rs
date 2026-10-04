//! Workspace queries, scheduled tasks and the agents' MCP tools: the
//! orchestrator side (the MCP protocol is in `crate::mcp`).

use super::*;

/// Longest message text an MCP read returns.
const MAX_TOOL_TEXT: usize = 16_000;
/// How deep delegation goes (a child's child is the last level).
const MAX_DELEGATION_DEPTH: usize = 2;
/// Children of one thread live at once (running, queued or still being
/// created).
const MAX_ACTIVE_CHILDREN: usize = 4;
/// Agent-created threads live at once in one project, whoever made them.
const MAX_PROJECT_AGENT_THREADS: usize = 8;
/// Threads one thread may have created and not archived, working or not.
const MAX_CHILDREN: usize = 16;
/// Schedules agents may have waiting for approval in one project.
const MAX_PROPOSALS: usize = 10;
/// Shortest time between two runs of a schedule an agent proposes.
const MIN_PROPOSED_INTERVAL_MS: i64 = 15 * 60 * 1000;

/// A git object id as `snapshot_tree` / checkpoints print them (never an
/// option or a revision expression).
fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn status_str(thread: &Thread) -> &'static str {
    match thread.status {
        blongo_protocol::ThreadStatus::Idle => "idle",
        blongo_protocol::ThreadStatus::Running => "running",
        blongo_protocol::ThreadStatus::Waiting => "waiting",
        blongo_protocol::ThreadStatus::Failed => "failed",
    }
}

fn provider_arg(args: &Value, default: ProviderKind) -> Result<ProviderKind, String> {
    match args.get("provider").and_then(Value::as_str) {
        None => Ok(default),
        Some(name) => ProviderKind::parse(name).ok_or_else(|| format!("unknown provider `{name}`")),
    }
}

fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("`{name}` is required"))
}

impl Orchestrator {
    // ---------------------------------------------------------------- queries

    /// Resolve a query's folder and trees on the loop, run it as a task
    /// and answer straight from there.
    pub(super) fn query(&mut self, id: QueryId, query: Query) {
        if let Query::PrRefresh { thread_id } = query {
            return self.pr_refresh(id, thread_id);
        }
        if matches!(
            query,
            Query::PrDetail { .. } | Query::PrEdit { .. } | Query::PrPrepare { .. }
        ) {
            return self.pr_query(id, query);
        }
        if let Query::PrDraft { thread_id, prompt } = query {
            return self.pr_draft(id, thread_id, prompt);
        }
        if let Query::PrFix { thread_id } = query {
            return self.pr_fix(id, thread_id);
        }
        if let Query::PrComments { thread_id, threads } = query {
            return self.pr_comments(id, thread_id, threads);
        }
        match self.query_plan(&query) {
            Ok((cwd, plan)) => {
                let out = self.out.clone();
                tokio::spawn(async move {
                    let result = workspace::run(cwd, plan).await;
                    let _ = out.send(CoreEvent::Reply { id, result });
                });
            }
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    fn query_plan(&self, query: &Query) -> Result<(PathBuf, Plan), String> {
        let (_, cwd) = self.thread_cwd(query.thread_id())?;
        let cwd = PathBuf::from(cwd);
        let plan = match query {
            Query::DiffSummary { thread_id, scope } => {
                let runs = self.visible_runs(*thread_id)?;
                match scope {
                    DiffScope::Turn { run_id } => {
                        let pos = runs
                            .iter()
                            .position(|r| r.id == *run_id)
                            .ok_or("unknown turn")?;
                        let from = runs[pos]
                            .checkpoint
                            .clone()
                            .ok_or("no checkpoint was taken for this turn")?;
                        // Up to the next turn's checkpoint; the latest turn
                        // ends at the working tree as it is now.
                        let to = runs[pos + 1..].iter().find_map(|r| r.checkpoint.clone());
                        Plan::DiffSummary { from, to }
                    }
                    DiffScope::Thread => {
                        let from = runs
                            .iter()
                            .find_map(|r| r.checkpoint.clone())
                            .ok_or("no checkpoint was taken in this thread yet")?;
                        Plan::DiffSummary { from, to: None }
                    }
                }
            }
            Query::DiffFile {
                from,
                to,
                path,
                max_lines,
                ..
            } => {
                if !is_object_id(from) || !is_object_id(to) {
                    return Err("not a diff of this thread".into());
                }
                blongo_git::workspace::safe_relative(path).map_err(|e| format!("{e:#}"))?;
                Plan::DiffFile {
                    from: from.clone(),
                    to: to.clone(),
                    path: path.clone(),
                    max_lines: (*max_lines).min(blongo_protocol::workspace::MAX_DIFF_LINES),
                }
            }
            Query::SearchFiles { pattern, limit, .. } => Plan::Search {
                pattern: pattern.clone(),
                limit: *limit,
            },
            Query::ListDir { path, .. } => Plan::ListDir { path: path.clone() },
            Query::ReadFile {
                path, max_bytes, ..
            } => Plan::ReadFile {
                path: path.clone(),
                max_bytes: (*max_bytes).min(blongo_protocol::workspace::MAX_READ_BYTES),
            },
            Query::GitStatus { .. } => Plan::Status,
            Query::GitBranches { .. } => Plan::Branches,
            Query::GitSwitch { branch, create, .. } => Plan::Switch {
                branch: branch.clone(),
                create: *create,
            },
            Query::GitCommit { message, .. } => {
                if message.trim().is_empty() {
                    return Err("enter a commit message".into());
                }
                Plan::Commit {
                    message: message.clone(),
                }
            }
            Query::PrRefresh { .. }
            | Query::PrDetail { .. }
            | Query::PrEdit { .. }
            | Query::PrPrepare { .. }
            | Query::PrDraft { .. }
            | Query::PrCreate { .. }
            | Query::PrPush { .. }
            | Query::PrFix { .. }
            | Query::PrComments { .. }
            | Query::PrMergeBase { .. } => {
                return Err("not a workspace query".into());
            }
        };
        Ok((cwd, plan))
    }

    /// A query that changes the folder: only while the thread (and, for a
    /// branch switch, every thread in the same folder) is idle; it holds
    /// `key` until done.
    pub(super) fn mutate(&mut self, id: QueryId, query: Query, key: Key) {
        if matches!(query, Query::PrCreate { .. } | Query::PrPush { .. }) {
            return self.pr_mutate(id, query, key);
        }
        if let Query::PrMergeBase { thread_id } = query {
            return self.pr_merge_base(id, thread_id, key);
        }
        let checked = (|| {
            let thread_id = query.thread_id();
            let (thread, cwd) = self.thread_cwd(thread_id)?;
            self.ensure_idle(&thread)?;
            if matches!(query, Query::GitSwitch { .. })
                && let Some(busy) = self
                    .folder_sharers(thread_id, &cwd)
                    .iter()
                    .find(|t| self.is_busy(t.id))
            {
                return Err(format!(
                    "\"{}\" works in the same folder and is running; wait for it to finish",
                    busy.title
                ));
            }
            self.query_plan(&query)
        })();
        match checked {
            Ok((cwd, plan)) => self.spawn_job(key, async move {
                let result = workspace::run(cwd, plan).await;
                JobDone::Mutation { id, key, result }
            }),
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    // -------------------------------------------------------------- schedules

    /// Send a schedule's prompt: to its thread, or a new thread when it has
    /// none (or that thread is gone). `advance`: a timer firing (the next
    /// time moves on); a manual run keeps it.
    pub(super) fn fire_schedule(&mut self, id: ScheduleId, advance: bool) {
        let Some(mut schedule) = self.schedules.get(&id).cloned() else {
            return;
        };
        let now = Timestamp::now();
        let target = schedule.thread_id.filter(|t| self.live_thread(*t).is_ok());
        let dispatch = |thread_id| Command::MessageDispatch {
            thread_id,
            message_id: ItemId::new(),
            run_id: RunId::new(),
            text: schedule.prompt.clone(),
            delivery: Delivery::Queue,
        };
        let (thread_id, pending) = match target {
            Some(thread_id) => (
                thread_id,
                Pending::new(dispatch(thread_id), Reply::Internal),
            ),
            None => {
                let thread_id = ThreadId::new();
                let create = Command::ThreadCreate {
                    thread_id,
                    project_id: schedule.project_id,
                    title: format!("Scheduled: {}", title_from(&schedule.prompt)),
                    provider: schedule.provider,
                    model: None,
                    worktree: false,
                    parent_thread_id: None,
                };
                // Both go in line now: the create is decided first.
                self.deferred
                    .push_back(Deferred::Dispatch(Pending::new(create, Reply::Internal)));
                (
                    thread_id,
                    Pending::new(dispatch(thread_id), Reply::Internal),
                )
            }
        };
        self.deferred.push_back(Deferred::Dispatch(pending));
        schedule.last_run_at = Some(now);
        schedule.last_thread_id = Some(thread_id);
        if advance {
            schedule.next_run_at = Cron::parse(&schedule.cron)
                .ok()
                .and_then(|c| c.next_after(now));
            if schedule.next_run_at.is_none() {
                schedule.enabled = false;
            }
        }
        self.commit_events(vec![EventKind::ScheduleUpdated { schedule }]);
    }

    // ------------------------------------------------------------------ MCP

    /// How many parents `thread` has (stops counting past the limit).
    fn depth(&self, thread: &Thread) -> usize {
        let mut depth = 0;
        let mut cursor = thread.parent_thread_id;
        while let Some(parent) = cursor {
            depth += 1;
            if depth > MAX_DELEGATION_DEPTH {
                break;
            }
            cursor = self.threads.get(&parent).and_then(|t| t.parent_thread_id);
        }
        depth
    }

    /// Agent-created threads that are live: running, holding a job, with
    /// a message or their creation still waiting in line. Each entry is
    /// (thread, parent, project); creations count before they commit, so
    /// calls made while the line is held (a global job) cannot slip past
    /// the limits.
    fn live_agent_threads(&self) -> Vec<(ThreadId, ThreadId, ProjectId)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for t in self.threads.values() {
            if let Some(parent) = t.parent_thread_id
                && !t.archived
                && (self.is_busy(t.id) || self.busy.contains(&t.id))
                && seen.insert(t.id)
            {
                out.push((t.id, parent, t.project_id));
            }
        }
        for item in &self.deferred {
            let Deferred::Dispatch(pending) = item else {
                continue;
            };
            match &pending.command.command {
                Command::ThreadCreate {
                    thread_id,
                    project_id,
                    parent_thread_id: Some(parent),
                    ..
                } if seen.insert(*thread_id) => out.push((*thread_id, *parent, *project_id)),
                Command::MessageDispatch { thread_id, .. } => {
                    if let Some(t) = self.threads.get(thread_id)
                        && let Some(parent) = t.parent_thread_id
                        && seen.insert(t.id)
                    {
                        out.push((t.id, parent, t.project_id));
                    }
                }
                _ => {}
            }
        }
        for (run_id, (thread_id, sender, project)) in &self.agent_runs {
            if self.run_alive(*thread_id, *run_id) && seen.insert(*thread_id) {
                out.push((*thread_id, *sender, *project));
            }
        }
        out
    }

    /// Is `run_id` working, queued on `thread_id`, or still waiting in line?
    fn run_alive(&self, thread_id: ThreadId, run_id: RunId) -> bool {
        let rt = self.rt.get(&thread_id);
        rt.and_then(|rt| rt.run.as_ref())
            .is_some_and(|r| r.run_id == run_id)
            || rt.is_some_and(|rt| rt.queue.iter().any(|q| q.run_id == run_id))
            || self.deferred.iter().any(|item| {
                matches!(item, Deferred::Dispatch(p) if matches!(
                    &p.command.command,
                    Command::MessageDispatch { run_id: r, .. } if *r == run_id
                ))
            })
    }

    /// May `caller` wake `target` with a message? Waking a thread that is
    /// not already working adds one more agent at work: an agent-created
    /// target counts against its parent and the project, any other thread
    /// against the sender and the project, the same limits as starting one.
    fn check_wake(&mut self, caller: &Thread, target: &Thread) -> Result<(), String> {
        let stale: Vec<RunId> = self
            .agent_runs
            .iter()
            .filter(|(run, (thread, _, _))| !self.run_alive(*thread, **run))
            .map(|(run, _)| *run)
            .collect();
        for run in stale {
            self.agent_runs.remove(&run);
        }
        let live = self.live_agent_threads();
        let working = live.iter().any(|(t, _, _)| *t == target.id)
            || self.is_busy(target.id)
            || self.busy.contains(&target.id);
        if working {
            // The message waits for the work already counted (or the
            // user's own run) to finish: no extra agent at the same time.
            return Ok(());
        }
        let parent = target.parent_thread_id.unwrap_or(caller.id);
        if live.iter().filter(|(_, p, _)| *p == parent).count() >= MAX_ACTIVE_CHILDREN {
            return Err(format!(
                "{MAX_ACTIVE_CHILDREN} threads started by the same thread are already working; \
                 wait for one (t3_thread_wait) before waking another"
            ));
        }
        if live
            .iter()
            .filter(|(_, _, project)| *project == target.project_id)
            .count()
            >= MAX_PROJECT_AGENT_THREADS
        {
            return Err(format!(
                "{MAX_PROJECT_AGENT_THREADS} agent-started threads are already working in this \
                 project; wait for some to finish"
            ));
        }
        Ok(())
    }

    /// May `caller` start one more thread? Depth, its own live children
    /// and the project's live agent threads are all bounded.
    fn check_spawn(&self, caller: &Thread) -> Result<(), String> {
        if self.depth(caller) >= MAX_DELEGATION_DEPTH {
            return Err(format!(
                "threads started by agents are limited to {MAX_DELEGATION_DEPTH} levels; do \
                 this task yourself"
            ));
        }
        let created = self
            .threads
            .values()
            .filter(|t| t.parent_thread_id == Some(caller.id) && !t.archived)
            .count()
            + self
                .deferred
                .iter()
                .filter(|item| {
                    matches!(item, Deferred::Dispatch(p) if matches!(
                        &p.command.command,
                        Command::ThreadCreate { parent_thread_id: Some(parent), .. }
                            if *parent == caller.id
                    ))
                })
                .count();
        if created >= MAX_CHILDREN {
            return Err(format!(
                "this thread already started {MAX_CHILDREN} threads; reuse one with \
                 t3_thread_send, or ask the user to archive some"
            ));
        }
        let live = self.live_agent_threads();
        if live.iter().filter(|(_, p, _)| *p == caller.id).count() >= MAX_ACTIVE_CHILDREN {
            return Err(format!(
                "{MAX_ACTIVE_CHILDREN} threads this thread started are still working; wait for \
                 one (task_status / t3_thread_wait) before starting another"
            ));
        }
        if live
            .iter()
            .filter(|(_, _, project)| *project == caller.project_id)
            .count()
            >= MAX_PROJECT_AGENT_THREADS
        {
            return Err(format!(
                "{MAX_PROJECT_AGENT_THREADS} agent-started threads are already working in this \
                 project; wait for some to finish"
            ));
        }
        Ok(())
    }

    /// A schedule proposed by an agent in `agent`: its own project, a
    /// bounded number waiting, and runs at least 15 minutes apart.
    pub(super) fn check_proposal(
        &self,
        agent: ThreadId,
        project_id: ProjectId,
        cron: &str,
    ) -> Result<(), String> {
        if self.live_thread(agent)?.project_id != project_id {
            return Err("the proposing thread is in another project".into());
        }
        let waiting = self
            .schedules
            .values()
            .filter(|s| s.project_id == project_id && s.proposed_by.is_some())
            .count();
        if waiting >= MAX_PROPOSALS {
            return Err(format!(
                "{MAX_PROPOSALS} proposed schedules already wait for the user's approval"
            ));
        }
        let parsed = Cron::parse(cron)?;
        if let Some(gap) = parsed.min_gap(Timestamp::now(), 500)
            && gap < MIN_PROPOSED_INTERVAL_MS
        {
            return Err(format!(
                "`{cron}` runs more often than every {} minutes; agents may only propose \
                 schedules with runs at least that far apart",
                MIN_PROPOSED_INTERVAL_MS / 60_000
            ));
        }
        Ok(())
    }

    /// Answer `tx` once `thread_id`'s work (run and queued messages) is
    /// done; at once when it is idle.
    pub(super) fn wait_for(
        &mut self,
        thread_id: ThreadId,
        tx: oneshot::Sender<Result<Value, String>>,
    ) {
        if self.is_busy(thread_id) || self.busy.contains(&thread_id) {
            self.waiters.entry(thread_id).or_default().push(tx);
        } else {
            let _ = tx.send(Ok(self.thread_result(thread_id)));
        }
    }

    pub(super) fn resolve_waiters(&mut self, thread_id: ThreadId) {
        if let Some(waiters) = self.waiters.remove(&thread_id) {
            let result = self.thread_result(thread_id);
            for tx in waiters {
                let _ = tx.send(Ok(result.clone()));
            }
        }
    }

    /// A thread's state and its last assistant message, for tools.
    fn thread_result(&self, thread_id: ThreadId) -> Value {
        let Some(thread) = self.threads.get(&thread_id) else {
            return json!({ "threadId": thread_id.to_string(), "status": "unknown" });
        };
        let runs = self.store.runs(thread_id).unwrap_or_default();
        let last_run = runs
            .iter()
            .rev()
            .find(|r| r.parent_run_id.is_none() && r.status != RunStatus::Cancelled);
        let last_message = self.store.items(thread_id).ok().and_then(|items| {
            items
                .iter()
                .rev()
                .find(|i| matches!(i.kind, ItemKind::AssistantMessage { .. }) && !i.text.is_empty())
                .map(|i| cut(&i.text, MAX_TOOL_TEXT))
        });
        json!({
            "threadId": thread_id.to_string(),
            "title": thread.title,
            "status": if self.is_busy(thread_id) { "running" } else { status_str(thread) },
            "lastTurn": last_run.map(|r| format!("{:?}", r.status).to_lowercase()),
            "lastMessage": last_message,
        })
    }

    /// A thread the caller may see: live and in the caller's project.
    fn scoped(&self, caller: &Thread, args: &Value, name: &str) -> Result<Thread, String> {
        let id = match args.get(name).and_then(Value::as_str) {
            None => return Ok(caller.clone()),
            Some(s) => ThreadId::parse(s).ok_or_else(|| format!("unknown thread {s}"))?,
        };
        match self.threads.get(&id) {
            Some(t) if t.project_id == caller.project_id && !t.archived => Ok(t.clone()),
            // Other projects' threads do not exist for the caller.
            _ => Err(format!("unknown thread {id}")),
        }
    }

    pub(super) fn on_mcp(&mut self, call: McpCall) {
        let McpCall {
            thread_id,
            tool,
            args,
            reply,
        } = call;
        let caller = match self.live_thread(thread_id) {
            Ok(t) => t.clone(),
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        if let Err(err) = self.mcp_tool(&caller, &tool, &args, reply) {
            // `mcp_tool` consumed the reply only on success paths.
            eprintln!("blongo-core: MCP {tool}: {err}");
        }
    }

    /// Run one tool; the answer goes to `tx` (now, or once its command
    /// commits / its thread finishes).
    fn mcp_tool(
        &mut self,
        caller: &Thread,
        tool: &str,
        args: &Value,
        tx: oneshot::Sender<Result<Value, String>>,
    ) -> Result<(), String> {
        let answer = |tx: oneshot::Sender<Result<Value, String>>, r: Result<Value, String>| {
            let _ = tx.send(r);
            Ok(())
        };
        match tool {
            "t3_thread_list" => {
                let filter = args.get("status").and_then(Value::as_str);
                let mut threads: Vec<&Thread> = self
                    .threads
                    .values()
                    .filter(|t| t.project_id == caller.project_id && !t.archived)
                    .collect();
                threads.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
                let list: Vec<Value> = threads
                    .into_iter()
                    .map(|t| {
                        (
                            t,
                            if self.is_busy(t.id) {
                                "running"
                            } else {
                                status_str(t)
                            },
                        )
                    })
                    .filter(|(_, status)| filter.is_none_or(|f| f == *status))
                    .map(|(t, status)| {
                        json!({
                            "threadId": t.id.to_string(),
                            "title": t.title,
                            "status": status,
                            "provider": t.provider.id(),
                            "parentThreadId": t.parent_thread_id.map(|p| p.to_string()),
                            "isCaller": t.id == caller.id,
                        })
                    })
                    .collect();
                answer(tx, Ok(json!({ "threads": list })))
            }
            "t3_thread_read" => {
                let target = match self.scoped(caller, args, "threadId") {
                    Ok(t) => t,
                    Err(e) => return answer(tx, Err(e)),
                };
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 100) as usize;
                let items = self.store.items(target.id).unwrap_or_default();
                let mut messages: Vec<Value> = items
                    .iter()
                    .filter_map(|i| {
                        let role = match i.kind {
                            ItemKind::UserMessage => "user",
                            ItemKind::AssistantMessage { .. } => "assistant",
                            _ => return None,
                        };
                        Some(json!({ "role": role, "text": cut(&i.text, MAX_TOOL_TEXT) }))
                    })
                    .collect();
                let skip = messages.len().saturating_sub(limit);
                messages.drain(..skip);
                let mut result = self.thread_result(target.id);
                result["messages"] = Value::Array(messages);
                result.as_object_mut().map(|o| o.remove("lastMessage"));
                answer(tx, Ok(result))
            }
            "t3_thread_create" => {
                let provider = match provider_arg(args, caller.provider) {
                    Ok(p) => p,
                    Err(e) => return answer(tx, Err(e)),
                };
                // An agent's thread is the caller's child, under the same
                // depth and concurrency limits as delegation.
                if let Err(e) = self.check_spawn(caller) {
                    return answer(tx, Err(e));
                }
                let thread_id = ThreadId::new();
                let create = Command::ThreadCreate {
                    thread_id,
                    project_id: caller.project_id,
                    title: args
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    provider,
                    model: None,
                    worktree: false,
                    parent_thread_id: Some(caller.id),
                };
                let value = json!({ "threadId": thread_id.to_string() });
                let then = match args.get("message").and_then(Value::as_str) {
                    Some(text) if !text.trim().is_empty() => Then::Chain(Box::new(Pending::new(
                        Command::MessageDispatch {
                            thread_id,
                            message_id: ItemId::new(),
                            run_id: RunId::new(),
                            text: text.to_owned(),
                            delivery: Delivery::Queue,
                        },
                        Reply::Mcp {
                            tx: oneshot::channel().0,
                            then: Then::Value(value),
                        },
                    ))),
                    _ => Then::Value(value),
                };
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    create,
                    Reply::Mcp { tx, then },
                )));
                Ok(())
            }
            "t3_thread_send" => {
                let target = match self.scoped(caller, args, "threadId") {
                    Ok(t) => t,
                    Err(e) => return answer(tx, Err(e)),
                };
                let text = match str_arg(args, "message") {
                    Ok(t) => t.to_owned(),
                    Err(e) => return answer(tx, Err(e)),
                };
                if target.id == caller.id {
                    return answer(
                        tx,
                        Err("a thread cannot send to itself; answer in this turn instead".into()),
                    );
                }
                let delivery = match args.get("mode").and_then(Value::as_str) {
                    Some("steer") => Delivery::Steer,
                    _ => Delivery::Queue,
                };
                if let Err(e) = self.check_wake(caller, &target) {
                    return answer(tx, Err(e));
                }
                let run_id = RunId::new();
                if target.parent_thread_id.is_none() {
                    self.agent_runs
                        .insert(run_id, (target.id, caller.id, target.project_id));
                }
                let send = Command::MessageDispatch {
                    thread_id: target.id,
                    message_id: ItemId::new(),
                    run_id,
                    text,
                    delivery,
                };
                let value = json!({ "threadId": target.id.to_string(), "sent": true });
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    send,
                    Reply::Mcp {
                        tx,
                        then: Then::Value(value),
                    },
                )));
                Ok(())
            }
            "t3_thread_wait" => {
                let target = match self.scoped(caller, args, "threadId") {
                    Ok(t) => t,
                    Err(e) => return answer(tx, Err(e)),
                };
                if target.id == caller.id {
                    return answer(tx, Err("a thread cannot wait for itself".into()));
                }
                self.wait_for(target.id, tx);
                Ok(())
            }
            "t3_thread_interrupt" => {
                let target = match self.scoped(caller, args, "threadId") {
                    Ok(t) => t,
                    Err(e) => return answer(tx, Err(e)),
                };
                let value = json!({ "threadId": target.id.to_string(), "interrupted": true });
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    Command::RunInterrupt {
                        thread_id: target.id,
                    },
                    Reply::Mcp {
                        tx,
                        then: Then::Value(value),
                    },
                )));
                Ok(())
            }
            "delegate_task" => {
                let prompt = match str_arg(args, "prompt") {
                    Ok(p) => p.to_owned(),
                    Err(e) => return answer(tx, Err(e)),
                };
                let provider = match provider_arg(args, caller.provider) {
                    Ok(p) => p,
                    Err(e) => return answer(tx, Err(e)),
                };
                if let Err(e) = self.check_spawn(caller) {
                    return answer(tx, Err(e));
                }
                let child = ThreadId::new();
                let title = args
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Task: {}", title_from(&prompt)));
                let wait = args.get("mode").and_then(Value::as_str) != Some("async");
                let then = if wait {
                    Then::Wait(child)
                } else {
                    Then::Value(json!({ "childThreadId": child.to_string(), "status": "running" }))
                };
                let send = Pending::new(
                    Command::MessageDispatch {
                        thread_id: child,
                        message_id: ItemId::new(),
                        run_id: RunId::new(),
                        text: prompt,
                        delivery: Delivery::Queue,
                    },
                    Reply::Mcp {
                        tx: oneshot::channel().0,
                        then,
                    },
                );
                let create = Command::ThreadCreate {
                    thread_id: child,
                    project_id: caller.project_id,
                    title,
                    provider,
                    model: (provider == caller.provider)
                        .then(|| caller.model.clone())
                        .flatten(),
                    worktree: false,
                    parent_thread_id: Some(caller.id),
                };
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    create,
                    Reply::Mcp {
                        tx,
                        then: Then::Chain(Box::new(send)),
                    },
                )));
                Ok(())
            }
            "task_status" => {
                let child = args
                    .get("childThreadId")
                    .and_then(Value::as_str)
                    .and_then(ThreadId::parse)
                    .and_then(|id| self.threads.get(&id))
                    .filter(|t| t.parent_thread_id == Some(caller.id));
                match child {
                    Some(child) => answer(tx, Ok(self.thread_result(child.id))),
                    None => answer(tx, Err("not a task this thread delegated".into())),
                }
            }
            "list_scheduled_tasks" => {
                let list: Vec<Value> = self
                    .schedules
                    .values()
                    .filter(|s| s.project_id == caller.project_id)
                    .map(|s| {
                        json!({
                            "scheduleId": s.id.to_string(),
                            "cron": s.cron,
                            "prompt": cut(&s.prompt, 2000),
                            "enabled": s.enabled,
                            "awaitingApproval": s.proposed_by.is_some(),
                            "threadId": s.thread_id.map(|t| t.to_string()),
                            "nextRunAt": s.next_run_at.map(|t| t.0),
                            "lastRunAt": s.last_run_at.map(|t| t.0),
                        })
                    })
                    .collect();
                answer(tx, Ok(json!({ "schedules": list })))
            }
            "schedule_task" => {
                let (cron, prompt) = match (str_arg(args, "cron"), str_arg(args, "prompt")) {
                    (Ok(c), Ok(p)) => (c.to_owned(), p.to_owned()),
                    (Err(e), _) | (_, Err(e)) => return answer(tx, Err(e)),
                };
                let bind = args
                    .get("bindToCurrentThread")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                // Checked now too, so the agent hears why at once.
                if let Err(e) = self.check_proposal(caller.id, caller.project_id, &cron) {
                    return answer(tx, Err(e));
                }
                let schedule_id = ScheduleId::new();
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    Command::ScheduleCreate {
                        schedule_id,
                        project_id: caller.project_id,
                        thread_id: bind.then_some(caller.id),
                        cron,
                        prompt,
                        provider: caller.provider,
                        proposed_by: Some(caller.id),
                    },
                    Reply::Mcp {
                        tx,
                        then: Then::Value(json!({
                            "scheduleId": schedule_id.to_string(),
                            "enabled": false,
                            "note": "Proposed: it runs only after the user turns it on in \
                                     Blongo's settings (Scheduled runs).",
                        })),
                    },
                )));
                Ok(())
            }
            other => answer(tx, Err(format!("unknown tool `{other}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_only() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(!is_object_id("--output=/etc/passwd"));
        assert!(!is_object_id("HEAD~1"));
        assert_eq!(cut("héllo", 2), "h…");
    }
}
