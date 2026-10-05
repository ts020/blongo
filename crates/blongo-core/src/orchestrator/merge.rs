//! Merging a thread's pull request on GitHub, and archiving the thread
//! once it is merged.
//!
//! A merge names the head commit the user looked at, so GitHub refuses
//! it when new commits arrived meanwhile. Auto-merge asks GitHub to merge
//! once the branch protection's requirements pass. When a poll sees the
//! pull request merged, Blongo archives the thread (if the project says
//! so) or suggests it. Archiving removes the worktree under the usual
//! safety rules and the local branch when it is exactly the merged
//! commit; the branch on GitHub is deleted only when asked for, and only
//! while it is still at the merged commit.

use blongo_forge::github::RepoInfo;
use blongo_protocol::{MergeMethod, PrDetail, PrLink, PrState, PrStatus};

use super::forge::ForgeCtx;
use super::*;

/// A merge or archive job ended.
pub(super) enum MergeDone {
    Merged {
        query: QueryId,
        thread_id: ThreadId,
        link: PrLink,
        method: MergeMethod,
        result: Result<String, String>,
    },
    /// The branch on GitHub was deleted (or not): archive now.
    BranchDeleted {
        query: QueryId,
        thread_id: ThreadId,
        result: Result<(), String>,
    },
}

impl Orchestrator {
    fn method_key(link: &PrLink) -> String {
        format!("merge:{}/{}", link.host, link.repo)
    }

    /// The method chosen last time for the pull request's repository.
    fn last_method(&self, link: &PrLink) -> Option<MergeMethod> {
        let (text, _) = self
            .store
            .forge_cache(&Self::method_key(link))
            .ok()
            .flatten()?;
        MergeMethod::parse(&text)
    }

    /// What the PR tab needs to offer a merge, from what is known of the
    /// repository (`None`: not known yet; the detail job asks GitHub).
    pub(super) fn merge_facts(&self, link: &PrLink) -> (Option<RepoInfo>, Option<MergeMethod>) {
        (
            self.cached_repo_info(&link.host, &link.repo),
            self.last_method(link),
        )
    }

    /// `Query::PrMerge`.
    pub(super) fn pr_merge(
        &mut self,
        id: QueryId,
        thread_id: ThreadId,
        method: MergeMethod,
        sha: String,
        auto: bool,
    ) {
        let job = (|| {
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let thread = self.live_thread(thread_id)?;
            let link = thread.pr.clone().ok_or("no pull request is linked")?;
            if link.read_only {
                return Err(format!("{} is read-only here", link.label()));
            }
            let status = thread
                .pr_status
                .as_ref()
                .ok_or("the pull request is not known yet")?;
            match status.state {
                PrState::Open => {}
                PrState::Draft => {
                    return Err("it is a draft; mark it ready for review first".into());
                }
                PrState::Merged => return Err("it is merged already".into()),
                PrState::Closed => return Err("it is closed".into()),
            }
            if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("check the pull request again before merging".into());
            }
            if let Some(info) = self.cached_repo_info(&link.host, &link.repo)
                && !allowed(&info).contains(&method)
            {
                return Err(format!(
                    "the repository does not allow {} merges",
                    method.as_str()
                ));
            }
            Ok(link)
        })();
        let link = match job {
            Ok(link) => link,
            Err(err) => {
                return self.emit(CoreEvent::Reply {
                    id,
                    result: Err(err),
                });
            }
        };
        let ctx = self.forge_ctx();
        self.spawn_job(Key::None, async move {
            let result = merge(&ctx, &link, method, &sha, auto).await;
            JobDone::ForgeMerge(Box::new(MergeDone::Merged {
                query: id,
                thread_id,
                link,
                method,
                result,
            }))
        });
    }

    /// `Query::PrArchive`.
    pub(super) fn pr_archive(&mut self, id: QueryId, thread_id: ThreadId, delete_remote: bool) {
        let job = (|| {
            let thread = self.live_thread(thread_id)?;
            self.ensure_idle(thread)?;
            if !delete_remote {
                return Ok(None);
            }
            if !self.config.forge {
                return Err("GitHub integration is turned off".to_owned());
            }
            let link = thread.pr.clone().ok_or("no pull request is linked")?;
            let status = thread
                .pr_status
                .as_ref()
                .ok_or("the pull request is not known yet")?;
            if status.state != PrState::Merged {
                return Err("only the branch of a merged pull request is deleted".into());
            }
            let ours = thread.worktree.as_ref().map(|w| w.branch.as_str());
            if link.read_only
                || !forge::owns_branch(thread)
                || ours != Some(&link.head_branch)
                || self.borrowed_branch(thread.id)
            {
                return Err("only the thread that owns the branch can delete it".into());
            }
            Ok(Some((link, status.head_sha.clone())))
        })();
        match job {
            Ok(None) => self.archive_for(id, thread_id),
            Ok(Some((link, sha))) => {
                let ctx = self.forge_ctx();
                self.spawn_job(Key::None, async move {
                    let result = delete_remote_branch(&ctx, &link, &sha).await;
                    JobDone::ForgeMerge(Box::new(MergeDone::BranchDeleted {
                        query: id,
                        thread_id,
                        result,
                    }))
                });
            }
            Err(err) => self.emit(CoreEvent::Reply {
                id,
                result: Err(err),
            }),
        }
    }

    /// Archive the thread, answering `id` once it is archived.
    fn archive_for(&mut self, id: QueryId, thread_id: ThreadId) {
        self.forge.answers.insert(id, "Archived".into());
        self.deferred.push_back(Deferred::Dispatch(Pending::new(
            Command::ThreadArchive { thread_id },
            Reply::Query(id),
        )));
    }

    pub(super) fn merge_done(&mut self, done: MergeDone) {
        match done {
            MergeDone::Merged {
                query,
                thread_id,
                link,
                method,
                result,
            } => {
                if result.is_ok() {
                    if let Err(err) = self
                        .store
                        .set_forge_cache(&Self::method_key(&link), method.as_str())
                    {
                        eprintln!("blongo-core: forge cache: {err:#}");
                    }
                    self.forge.hurry(thread_id, Duration::ZERO);
                }
                self.emit(CoreEvent::Reply {
                    id: query,
                    result: result.map(QueryReply::Done),
                });
            }
            MergeDone::BranchDeleted {
                query,
                thread_id,
                result,
            } => match result {
                Ok(()) => self.archive_for(query, thread_id),
                Err(err) => self.emit(CoreEvent::Reply {
                    id: query,
                    result: Err(err),
                }),
            },
        }
    }

    /// A thread's pull request status was committed: on its merge (news,
    /// not a merged one first seen), archive the thread or suggest it.
    pub(super) fn consider_merged(&mut self, thread_id: ThreadId, status: &PrStatus) {
        let before = self.forge.states.insert(thread_id, status.state);
        if status.state != PrState::Merged || before.is_none_or(|s| s == PrState::Merged) {
            return;
        }
        let Some(thread) = self.threads.get(&thread_id).filter(|t| !t.archived) else {
            return;
        };
        let (Some(link), Some(project)) = (&thread.pr, self.projects.get(&thread.project_id))
        else {
            return;
        };
        let label = link.label();
        if project.forge.archive_on_merge && forge::owns_branch(thread) {
            if self.ensure_idle(thread).is_ok() {
                // Said when the archive commits (a turn starting first
                // refuses it, and the scheduler reports that).
                self.forge.auto_archiving.insert(thread_id);
                self.deferred.push_back(Deferred::Dispatch(Pending::new(
                    Command::ThreadArchive { thread_id },
                    Reply::Internal,
                )));
            } else {
                self.thread_notice(
                    thread_id,
                    &format!(
                        "{label} was merged. The thread is busy, so it was not archived; archive \
                         it from the PR tab when it is done."
                    ),
                );
            }
        } else {
            self.thread_notice(
                thread_id,
                &format!(
                    "{label} was merged. Archive the thread from the PR tab when you are done."
                ),
            );
        }
    }
}

/// The merge methods `info` allows.
pub(super) fn allowed(info: &RepoInfo) -> Vec<MergeMethod> {
    let mut out = Vec::new();
    if info.allow_merge_commit {
        out.push(MergeMethod::Merge);
    }
    if info.allow_squash_merge {
        out.push(MergeMethod::Squash);
    }
    if info.allow_rebase_merge {
        out.push(MergeMethod::Rebase);
    }
    out
}

/// Fill the PR tab's merge fields.
pub(super) fn fill_merge_facts(
    detail: &mut PrDetail,
    info: Option<&RepoInfo>,
    last: Option<MergeMethod>,
) {
    if let Some(info) = info {
        detail.merge_methods = allowed(info);
        detail.delete_branch_on_merge = info.delete_branch_on_merge;
    }
    detail.merge_method = last.filter(|m| detail.merge_methods.contains(m));
}

async fn merge(
    ctx: &ForgeCtx,
    link: &PrLink,
    method: MergeMethod,
    sha: &str,
    auto: bool,
) -> Result<String, String> {
    let (gh, repo) = forge::client_for(ctx, link).await?;
    if auto {
        let detail = gh
            .detail(&repo, link.number)
            .await
            .map_err(|e| ctx.fail(&repo.host, e))?;
        if detail.status.head_sha != sha {
            return Err(
                "the pull request changed since you looked (new commits); check it again".into(),
            );
        }
        gh.enable_auto_merge(&detail.node_id, method, sha)
            .await
            .map_err(|e| ctx.fail(&repo.host, e))?;
        return Ok(format!(
            "Auto-merge is on for {}: GitHub merges it once its checks and reviews pass.",
            link.label()
        ));
    }
    gh.merge_pull(&repo, link.number, method, sha)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    Ok(format!("{} merged", link.label()))
}

/// Delete the merged branch on GitHub, only while it is still at the
/// merged commit (nobody pushed to it since). Asked of the pull
/// request's own repository, not the folder's remote.
async fn delete_remote_branch(ctx: &ForgeCtx, link: &PrLink, sha: &str) -> Result<(), String> {
    let (gh, repo) = forge::client_for(ctx, link).await?;
    let tip = gh
        .branch_tip(&repo, &link.head_branch)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))?;
    match tip {
        None => return Ok(()),
        Some(tip) if tip == sha => {}
        Some(_) => {
            return Err(format!(
                "{} on GitHub has commits that were not merged; it was kept, and so was the thread",
                link.head_branch
            ));
        }
    }
    gh.delete_branch(&repo, &link.head_branch)
        .await
        .map_err(|e| ctx.fail(&repo.host, e))
}
