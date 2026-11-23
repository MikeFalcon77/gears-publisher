//! Use cases behind the REST API. Every durable call uses the service identity;
//! see [`crate::domain::access`]. Who may see and control which run is decided here,
//! by [`Person::may_see`] and [`Person::may_control`].
use crate::domain::ci::{self, CiInput, CiState};
use crate::domain::error::{DomainError, Result};
use crate::domain::models::{
    LiveEvent, LogLine, NewLine, Person, QueuedCi, Role, RunDetail, RunKind, RunRecord, RunSummary,
};
use crate::domain::release::{ReleaseInput, ReleaseState};
use crate::domain::repos::RunRepository;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;
use toolkit_durable_execution::ExecutionInspector;
use toolkit_durable_execution::observation::{RunProgress, WorkflowQuery};
use toolkit_durable_execution::prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// How far back the run list reaches; older runs' logs are deleted.
pub const LIST_DAYS: i64 = 180;
/// Runs read per page of the engine's list.
const PAGE: u32 = 100;
/// Runs listed per kind.
const LISTED: usize = 100;

/// What starting a PR's CI did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStart {
    Started(Uuid),
    /// Waiting for the run in progress; `position` 1 starts next.
    Queued {
        id: Uuid,
        position: usize,
    },
}

/// Workflow references, set once the engine is serving.
#[derive(Default)]
pub struct Workflows {
    pub release: OnceLock<WorkflowRef<ReleaseInput, ReleaseState>>,
    pub ci: OnceLock<WorkflowRef<CiInput, CiState>>,
}

pub struct RunService {
    client: ExecutionClient,
    inspector: ExecutionInspector,
    pub workflows: Workflows,
    repo: Arc<dyn RunRepository>,
    service_ctx: SecurityContext,
    /// Why PR CI cannot run on this host, if it cannot.
    ci_refusal: Option<&'static str>,
    /// Held while deciding to start or continue a run, and while starting it.
    start_lock: tokio::sync::Mutex<()>,
}

impl RunService {
    #[must_use]
    pub fn new(
        client: ExecutionClient,
        inspector: ExecutionInspector,
        repo: Arc<dyn RunRepository>,
        service_ctx: SecurityContext,
        ci_refusal: Option<&'static str>,
    ) -> Self {
        Self {
            client,
            inspector,
            workflows: Workflows::default(),
            repo,
            service_ctx,
            ci_refusal,
            start_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Why PR CI is unavailable, if it is.
    #[must_use]
    pub fn ci_refusal(&self) -> Option<&'static str> {
        self.ci_refusal
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.repo.subscribe()
    }

    /// Runs of `kind` (all kinds: `None`) of the last [`LIST_DAYS`], newest first.
    async fn page(&self, kind: Option<RunKind>) -> Result<Vec<RunProgress>> {
        let since = Utc::now() - ChronoDuration::days(LIST_DAYS);
        let mut items = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .inspector
                .list(
                    &self.service_ctx,
                    WorkflowQuery::new(since).limit(PAGE).offset(offset),
                )
                .await?;
            let n = page.items.len();
            items.extend(
                page.items
                    .into_iter()
                    .filter(|p| kind.is_none_or(|k| RunKind::of(&p.definition) == Some(k))),
            );
            offset += PAGE;
            if n < PAGE as usize || u64::from(offset) >= page.total {
                break;
            }
        }
        items.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        Ok(items)
    }

    /// Runs of `kind` still in progress.
    async fn unsettled(&self, kind: RunKind) -> Result<Vec<RunProgress>> {
        let mut runs = self.page(Some(kind)).await?;
        runs.retain(|p| !p.state.is_settled());
        Ok(runs)
    }

    /// Runs `person` may see, of `kind` (all kinds: `None`), newest first.
    pub async fn list(&self, person: &Person, kind: Option<RunKind>) -> Result<Vec<RunSummary>> {
        let items = self.page(kind).await?;
        let mut records: HashMap<Uuid, RunRecord> = self
            .repo
            .runs(items.iter().map(|p| p.id.0).collect())
            .await?
            .into_iter()
            .map(|r| (r.run_id, r))
            .collect();
        // Filtered before the cut, so that a member's list is not cut short by others'.
        let mut runs: Vec<RunSummary> = items
            .into_iter()
            .map(|progress| RunSummary {
                record: records.remove(&progress.id.0),
                progress,
            })
            .filter(|run| may_see(person, &run.progress, run.record.as_ref()))
            .collect();
        runs.truncate(if kind.is_some() {
            LISTED
        } else {
            LISTED * RunKind::ALL.len()
        });
        Ok(runs)
    }

    /// Releases share one clone, so at most one may be active. `continuing` is the run
    /// a Retry or Resume would set going again: it must be the newest release,
    /// otherwise it would work on a tree and a `main` that later runs have moved on
    /// from. A PR CI run continued while another one is active would share the CI
    /// clone with it, so that is refused too (new runs queue instead).
    /// Call with `start_lock` held.
    async fn ensure_exclusive(&self, kind: RunKind, continuing: Option<Uuid>) -> Result<()> {
        let runs = self.page(Some(kind)).await?;
        if let Some(active) = runs
            .iter()
            .find(|p| !p.state.is_settled() && Some(p.id.0) != continuing)
        {
            return Err(DomainError::Conflict(format!(
                "run {} is still in progress",
                active.id
            )));
        }
        if kind == RunKind::Release
            && let Some(id) = continuing
            && let Some(newest) = runs.first()
            && newest.id.0 != id
        {
            return Err(DomainError::Conflict(format!(
                "run {} was started after this one; only the latest run can be continued, \
                 start a new release instead",
                newest.id
            )));
        }
        Ok(())
    }

    /// Start a release unless one is in progress.
    pub async fn start_release(&self, person: &Person, input: ReleaseInput) -> Result<Uuid> {
        if !person.may_start(RunKind::Release) {
            return Err(DomainError::AdminOnly);
        }
        let workflow = self.workflows.release.get().ok_or(DomainError::Starting)?;
        let _guard = self.start_lock.lock().await;
        self.ensure_exclusive(RunKind::Release, None).await?;
        let dry_run = input.dry_run;
        let started = self
            .client
            .start(&self.service_ctx, workflow, input, StartOptions::new())
            .await?;
        let run_id = started.run_id().0;
        self.record(
            person.label(),
            person.subject_id,
            run_id,
            dry_run,
            serde_json::json!({}),
        )
        .await?;
        tracing::info!(%run_id, by = %person.label(), dry_run, "release started");
        let what = if dry_run { "dry run" } else { "release" };
        self.control_note(run_id, format!("{what} started by {}", person.label()))
            .await;
        Ok(run_id)
    }

    /// Start a PR's CI, or queue it while another one runs. Like GitHub's
    /// `concurrency` group per PR, a new run for the same PR replaces the caller's run
    /// in progress or in the queue; someone else's is left alone, unless the caller
    /// is an admin.
    pub async fn start_ci(
        &self,
        person: &Person,
        pr: u64,
        jobs: Option<Vec<String>>,
    ) -> Result<CiStart> {
        if !person.may_start(RunKind::Ci) {
            return Err(DomainError::AdminOnly);
        }
        if let Some(refusal) = self.ci_refusal {
            return Err(DomainError::Unavailable(refusal.to_owned()));
        }
        if pr == 0 {
            return Err(DomainError::Invalid("a PR number is required".into()));
        }
        let jobs = match jobs {
            Some(ids) if ids.is_empty() => {
                return Err(DomainError::Invalid("select at least one job".into()));
            }
            Some(ids) => {
                if let Some(unknown) = ids.iter().find(|id| ci::jobs::job(id).is_none()) {
                    return Err(DomainError::Invalid(format!("unknown job `{unknown}`")));
                }
                Some(ids)
            }
            None => None,
        };
        let workflow = self.workflows.ci.get().ok_or(DomainError::Starting)?;
        let _guard = self.start_lock.lock().await;
        // A superseded run only has to stop: the new one may start while it winds down.
        let mut busy = false;
        for active in self.unsettled(RunKind::Ci).await? {
            let id = active.id.0;
            let record = self.repo.run(id).await?;
            let owner = record.as_ref().and_then(|r| r.started_by_subject);
            let running = record
                .as_ref()
                .and_then(|r| r.facts["pr_number"].as_u64())
                .unwrap_or_default();
            if running != pr || !person.may_control(RunKind::Ci, owner) {
                busy = true;
                continue;
            }
            self.after_command(person, id, "cancel").await;
            self.control_note(id, "superseded by a new run of the same PR".into())
                .await;
            // Already settling is fine: it only has to stop.
            let _ = self
                .client
                .cancel(
                    &self.service_ctx,
                    active.id,
                    CancelOptions::new(active.epoch).reason("superseded by a new run"),
                )
                .await;
        }
        let mut waiting = 0;
        for entry in self.repo.ci_queue().await? {
            if entry.pr == pr && person.may_control(RunKind::Ci, Some(entry.requested_by_subject)) {
                if self.repo.take_ci(entry.id).await?.is_some() {
                    tracing::info!(
                        pr,
                        by = %person.label(),
                        queued_by = %entry.requested_by,
                        "queued PR CI replaced by a new request"
                    );
                }
            } else {
                waiting += 1;
            }
        }
        if !busy && waiting == 0 {
            let id = self
                .launch_ci(workflow, person.label(), person.subject_id, pr, jobs, None)
                .await?;
            return Ok(CiStart::Started(id));
        }
        let entry = QueuedCi {
            id: Uuid::new_v4(),
            pr,
            jobs,
            requested_by: person.label().to_owned(),
            requested_by_subject: person.subject_id,
            queued_at: Utc::now(),
        };
        let id = entry.id;
        self.repo.enqueue_ci(entry).await?;
        let position = waiting + 1;
        tracing::info!(queued = %id, by = %person.label(), pr, position, "PR CI queued");
        // Nothing runs but others were waiting: the oldest goes now, not on the next tick.
        if !busy && let Err(error) = self.dispatch_locked().await {
            tracing::warn!(%error, "failed to start queued PR CI");
        }
        Ok(CiStart::Queued { id, position })
    }

    /// Start the oldest queued PR CI unless a CI run is in progress. Called
    /// periodically and whenever a run may have changed.
    pub async fn dispatch_ci(&self) -> Result<Option<Uuid>> {
        let _guard = self.start_lock.lock().await;
        self.dispatch_locked().await
    }

    /// Call with `start_lock` held.
    async fn dispatch_locked(&self) -> Result<Option<Uuid>> {
        if self.ci_refusal.is_some() {
            return Ok(None);
        }
        let Some(workflow) = self.workflows.ci.get() else {
            return Ok(None);
        };
        let Some(next) = self.repo.ci_queue().await?.into_iter().next() else {
            return Ok(None);
        };
        if !self.unsettled(RunKind::Ci).await?.is_empty() {
            return Ok(None);
        }
        let Some(entry) = self.repo.take_ci(next.id).await? else {
            return Ok(None);
        };
        match self
            .launch_ci(
                workflow,
                &entry.requested_by,
                entry.requested_by_subject,
                entry.pr,
                entry.jobs.clone(),
                Some(entry.queued_at),
            )
            .await
        {
            Ok(id) => Ok(Some(id)),
            Err(error) => {
                // Back in its place: same id, same time.
                self.repo.enqueue_ci(entry).await?;
                Err(error)
            }
        }
    }

    /// PR CI waiting to start, each with its place (1: next). A member sees only
    /// their own entries, with their places among everyone's.
    pub async fn ci_queue(&self, person: &Person) -> Result<Vec<(usize, QueuedCi)>> {
        Ok(self
            .repo
            .ci_queue()
            .await?
            .into_iter()
            .enumerate()
            .map(|(i, entry)| (i + 1, entry))
            .filter(|(_, e)| person.may_see(RunKind::Ci, Some(e.requested_by_subject)))
            .collect())
    }

    /// Take a PR CI out of the queue.
    pub async fn cancel_queued(&self, person: &Person, id: Uuid) -> Result<()> {
        let entry = self
            .repo
            .ci_queue()
            .await?
            .into_iter()
            .find(|e| e.id == id)
            .filter(|e| person.may_see(RunKind::Ci, Some(e.requested_by_subject)))
            .ok_or(DomainError::NotFound)?;
        if !person.may_control(RunKind::Ci, Some(entry.requested_by_subject)) {
            return Err(DomainError::AdminOnly);
        }
        self.repo.take_ci(id).await?.ok_or(DomainError::NotFound)?;
        tracing::info!(queued = %id, pr = entry.pr, by = %person.label(), "queued PR CI cancelled");
        Ok(())
    }

    async fn launch_ci(
        &self,
        workflow: &WorkflowRef<CiInput, CiState>,
        by: &str,
        by_subject: Uuid,
        pr: u64,
        jobs: Option<Vec<String>>,
        queued_at: Option<DateTime<Utc>>,
    ) -> Result<Uuid> {
        let input = CiInput {
            pr,
            jobs: jobs.clone(),
        };
        let started = self
            .client
            .start(&self.service_ctx, workflow, input, StartOptions::new())
            .await?;
        let run_id = started.run_id().0;
        let facts = serde_json::json!({ "pr_number": pr, "only": jobs });
        self.record(by, by_subject, run_id, false, facts).await?;
        tracing::info!(%run_id, by, pr, queued = queued_at.is_some(), "PR CI started");
        let rerun = jobs
            .map(|ids| format!(" (re-run of {})", ids.join(", ")))
            .unwrap_or_default();
        let queued = queued_at
            .map(|at| format!(", queued at {}", at.format("%H:%M:%S UTC")))
            .unwrap_or_default();
        self.control_note(
            run_id,
            format!("PR #{pr} CI started by {by}{rerun}{queued}"),
        )
        .await;
        Ok(run_id)
    }

    /// Who started a run; a run nobody can attribute is not left running.
    async fn record(
        &self,
        by: &str,
        by_subject: Uuid,
        run_id: Uuid,
        dry_run: bool,
        facts: serde_json::Value,
    ) -> Result<()> {
        let Err(error) = self
            .repo
            .create_run(run_id, by, by_subject, dry_run, facts)
            .await
        else {
            return Ok(());
        };
        tracing::error!(%run_id, %error, "failed to record a run; cancelling it");
        if let Ok(progress) = self.progress(run_id).await {
            let _ = self
                .client
                .cancel(
                    &self.service_ctx,
                    RunId(run_id),
                    CancelOptions::new(progress.epoch).reason("could not be recorded"),
                )
                .await;
        }
        Err(error.into())
    }

    /// A run `person` may see, with its kind and record. Anyone else's is
    /// `NotFound`, so that its existence does not leak.
    async fn visible(
        &self,
        person: &Person,
        id: Uuid,
    ) -> Result<(RunProgress, RunKind, Option<RunRecord>)> {
        let progress = self.progress(id).await?;
        let kind = RunKind::of(&progress.definition).ok_or(DomainError::NotFound)?;
        let record = self.repo.run(id).await?;
        if !person.may_see(kind, record.as_ref().and_then(|r| r.started_by_subject)) {
            return Err(DomainError::NotFound);
        }
        Ok((progress, kind, record))
    }

    pub async fn get(&self, person: &Person, id: Uuid) -> Result<RunDetail> {
        let (progress, _, record) = self.visible(person, id).await?;
        let history = self.client.history(&self.service_ctx, RunId(id)).await?;
        Ok(RunDetail {
            progress,
            history,
            record,
        })
    }

    /// A run's progress, unchecked: for the service's own use once access is settled.
    pub async fn progress(&self, id: Uuid) -> Result<RunProgress> {
        Ok(self.client.get(&self.service_ctx, RunId(id)).await?)
    }

    /// A run's progress, if `person` may see it.
    pub async fn progress_for(&self, person: &Person, id: Uuid) -> Result<RunProgress> {
        Ok(self.visible(person, id).await?.0)
    }

    /// A run `person` may control: anyone else's is `NotFound` if they may not see
    /// it either, else `AdminOnly`.
    async fn controllable(&self, person: &Person, id: Uuid) -> Result<(RunProgress, RunKind)> {
        let (progress, kind, record) = self.visible(person, id).await?;
        if !person.may_control(kind, record.as_ref().and_then(|r| r.started_by_subject)) {
            return Err(DomainError::AdminOnly);
        }
        Ok((progress, kind))
    }

    /// A run to Retry or Resume: controllable, runnable on this host, and alone.
    /// Call with `start_lock` held.
    async fn continuable(&self, person: &Person, id: Uuid) -> Result<RunProgress> {
        let (progress, kind) = self.controllable(person, id).await?;
        if kind == RunKind::Ci
            && let Some(refusal) = self.ci_refusal
        {
            return Err(DomainError::Unavailable(refusal.to_owned()));
        }
        self.ensure_exclusive(kind, Some(id)).await?;
        Ok(progress)
    }

    pub async fn cancel(&self, person: &Person, id: Uuid) -> Result<()> {
        let (progress, _) = self.controllable(person, id).await?;
        self.client
            .cancel(
                &self.service_ctx,
                RunId(id),
                CancelOptions::new(progress.epoch).reason("cancelled from the UI"),
            )
            .await?;
        self.after_command(person, id, "cancel").await;
        Ok(())
    }

    pub async fn resume(&self, person: &Person, id: Uuid) -> Result<()> {
        let _guard = self.start_lock.lock().await;
        let progress = self.continuable(person, id).await?;
        // Noted before the engine is told: the worker's first lines of the new attempt
        // must come after the note that opens it in the UI.
        self.after_command(person, id, "resume").await;
        if let Err(error) = self
            .client
            .resume(
                &self.service_ctx,
                RunId(id),
                ContinueOptions::new(progress.epoch),
            )
            .await
        {
            self.control_note(id, format!("resume rejected: {error}"))
                .await;
            return Err(error.into());
        }
        Ok(())
    }

    pub async fn retry(&self, person: &Person, id: Uuid) -> Result<()> {
        let _guard = self.start_lock.lock().await;
        let progress = self.continuable(person, id).await?;
        self.after_command(person, id, "retry").await;
        if let Err(error) = self
            .client
            .retry(
                &self.service_ctx,
                RunId(id),
                ContinueOptions::new(progress.epoch),
            )
            .await
        {
            self.control_note(id, format!("retry rejected: {error}"))
                .await;
            return Err(error.into());
        }
        Ok(())
    }

    async fn after_command(&self, person: &Person, id: Uuid, command: &str) {
        tracing::info!(run_id = %id, by = %person.label(), command, "run command");
        self.control_note(id, format!("{command} requested by {}", person.label()))
            .await;
    }

    /// A line on the `control` pseudo-step; the UI splits a run into attempts on them.
    async fn control_note(&self, id: Uuid, line: String) {
        self.repo
            .log(NewLine {
                run_id: id,
                step: "control".into(),
                attempt: 0,
                stream: "info",
                line,
            })
            .await;
        self.repo.notify(id);
    }

    /// Lines of a run `person` may see.
    pub async fn log(
        &self,
        person: &Person,
        id: Uuid,
        after: i64,
        limit: u64,
    ) -> Result<Vec<LogLine>> {
        self.visible(person, id).await?;
        self.lines(id, after, limit).await
    }

    /// Lines of a run, unchecked: for a stream whose access was checked when it opened.
    pub(crate) async fn lines(&self, id: Uuid, after: i64, limit: u64) -> Result<Vec<LogLine>> {
        Ok(self.repo.lines(id, after, limit.clamp(1, 5000)).await?)
    }

    /// Daily: delete the logs of runs that have dropped out of the run list.
    pub async fn prune_logs(&self) -> anyhow::Result<u64> {
        self.repo.prune_log(LIST_DAYS).await
    }
}

/// Whether `person` may see a run; one of an unknown workflow, only an admin.
fn may_see(person: &Person, progress: &RunProgress, record: Option<&RunRecord>) -> bool {
    let owner = record.and_then(|r| r.started_by_subject);
    RunKind::of(&progress.definition).map_or(person.role == Role::Admin, |kind| {
        person.may_see(kind, owner)
    })
}

#[cfg(test)]
#[path = "run_service_tests.rs"]
mod tests;
