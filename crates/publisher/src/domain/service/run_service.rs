//! Use cases behind the REST API. Every durable call uses the service identity;
//! see [`crate::domain::access`].
use crate::domain::access::Directory;
use crate::domain::ci::{self, CiInput, CiState};
use crate::domain::error::{DomainError, Result};
use crate::domain::models::{
    LiveEvent, LogLine, NewLine, Person, RunDetail, RunKind, RunRecord, RunSummary,
};
use crate::domain::release::{ReleaseInput, ReleaseState};
use crate::domain::repos::RunRepository;
use chrono::{Duration as ChronoDuration, Utc};
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
    directory: Directory,
    service_ctx: SecurityContext,
    /// Why PR CI cannot run on this host, if it cannot.
    ci_refusal: Option<&'static str>,
    start_lock: tokio::sync::Mutex<()>,
}

impl RunService {
    #[must_use]
    pub fn new(
        client: ExecutionClient,
        inspector: ExecutionInspector,
        repo: Arc<dyn RunRepository>,
        directory: Directory,
        service_ctx: SecurityContext,
        ci_refusal: Option<&'static str>,
    ) -> Self {
        Self {
            client,
            inspector,
            workflows: Workflows::default(),
            repo,
            directory,
            service_ctx,
            ci_refusal,
            start_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The configured person behind a request.
    pub fn person(&self, ctx: &SecurityContext) -> Result<&Person> {
        self.directory.person(ctx).ok_or(DomainError::Forbidden)
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

    pub async fn list(&self, kind: Option<RunKind>) -> Result<Vec<RunSummary>> {
        let mut items = self.page(kind).await?;
        items.truncate(if kind.is_some() {
            LISTED
        } else {
            LISTED * RunKind::ALL.len()
        });
        let mut records: HashMap<Uuid, RunRecord> = self
            .repo
            .runs(items.iter().map(|p| p.id.0).collect())
            .await?
            .into_iter()
            .map(|r| (r.run_id, r))
            .collect();
        Ok(items
            .into_iter()
            .map(|progress| RunSummary {
                record: records.remove(&progress.id.0),
                progress,
            })
            .collect())
    }

    /// Releases share one clone, so at most one may be active. `continuing` is the run
    /// a Retry or Resume would set going again: it must be the newest release,
    /// otherwise it would work on a tree and a `main` that later runs have moved on
    /// from. PR CI runs each check out their own commit; only one at a time is active.
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
        let workflow = self.workflows.release.get().ok_or(DomainError::Starting)?;
        let _guard = self.start_lock.lock().await;
        self.ensure_exclusive(RunKind::Release, None).await?;
        let dry_run = input.dry_run;
        let started = self
            .client
            .start(&self.service_ctx, workflow, input, StartOptions::new())
            .await?;
        let run_id = started.run_id().0;
        self.record(person, run_id, dry_run, serde_json::json!({}))
            .await?;
        tracing::info!(%run_id, by = %person.email, dry_run, "release started");
        let what = if dry_run { "dry run" } else { "release" };
        self.control_note(run_id, format!("{what} started by {}", person.email))
            .await;
        Ok(run_id)
    }

    /// Start a PR's CI. Like GitHub's `concurrency` group per PR, a new run for the
    /// same PR cancels the one in progress; another PR's run must finish first.
    pub async fn start_ci(
        &self,
        person: &Person,
        pr: u64,
        jobs: Option<Vec<String>>,
    ) -> Result<Uuid> {
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
        for active in self
            .page(Some(RunKind::Ci))
            .await?
            .into_iter()
            .filter(|p| !p.state.is_settled())
        {
            let id = active.id.0;
            let record = self.repo.run(id).await?;
            let running = record
                .as_ref()
                .and_then(|r| r.facts["pr_number"].as_u64())
                .unwrap_or_default();
            if running != pr {
                return Err(DomainError::Conflict(format!(
                    "CI of PR #{running} is in progress (run {}); wait for it or cancel it",
                    active.id
                )));
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
        self.record(person, run_id, false, facts).await?;
        tracing::info!(%run_id, by = %person.email, pr, "PR CI started");
        let rerun = jobs
            .map(|ids| format!(" (re-run of {})", ids.join(", ")))
            .unwrap_or_default();
        self.control_note(
            run_id,
            format!("PR #{pr} CI started by {}{rerun}", person.email),
        )
        .await;
        Ok(run_id)
    }

    /// Who started a run; a run nobody can attribute is not left running.
    async fn record(
        &self,
        person: &Person,
        run_id: Uuid,
        dry_run: bool,
        facts: serde_json::Value,
    ) -> Result<()> {
        let Err(error) = self
            .repo
            .create_run(run_id, &person.email, dry_run, facts)
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

    pub async fn get(&self, id: Uuid) -> Result<RunDetail> {
        let run = RunId(id);
        let progress = self.client.get(&self.service_ctx, run).await?;
        let history = self.client.history(&self.service_ctx, run).await?;
        let record = self.repo.run(id).await?;
        Ok(RunDetail {
            progress,
            history,
            record,
        })
    }

    pub async fn progress(&self, id: Uuid) -> Result<RunProgress> {
        Ok(self.client.get(&self.service_ctx, RunId(id)).await?)
    }

    /// A run to continue, and its kind.
    async fn kind(&self, id: Uuid) -> Result<(RunProgress, RunKind)> {
        let progress = self.progress(id).await?;
        let kind = RunKind::of(&progress.definition).ok_or(DomainError::NotFound)?;
        if kind == RunKind::Ci
            && let Some(refusal) = self.ci_refusal
        {
            return Err(DomainError::Unavailable(refusal.to_owned()));
        }
        Ok((progress, kind))
    }

    pub async fn cancel(&self, person: &Person, id: Uuid) -> Result<()> {
        let epoch = self.progress(id).await?.epoch;
        self.client
            .cancel(
                &self.service_ctx,
                RunId(id),
                CancelOptions::new(epoch).reason("cancelled from the UI"),
            )
            .await?;
        self.after_command(person, id, "cancel").await;
        Ok(())
    }

    pub async fn resume(&self, person: &Person, id: Uuid) -> Result<()> {
        let _guard = self.start_lock.lock().await;
        let (progress, kind) = self.kind(id).await?;
        self.ensure_exclusive(kind, Some(id)).await?;
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
        let (progress, kind) = self.kind(id).await?;
        self.ensure_exclusive(kind, Some(id)).await?;
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
        tracing::info!(run_id = %id, by = %person.email, command, "run command");
        self.control_note(id, format!("{command} requested by {}", person.email))
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

    pub async fn log(&self, id: Uuid, after: i64, limit: u64) -> Result<Vec<LogLine>> {
        Ok(self.repo.lines(id, after, limit.clamp(1, 5000)).await?)
    }

    /// Daily: delete the logs of runs that have dropped out of the run list.
    pub async fn prune_logs(&self) -> anyhow::Result<u64> {
        self.repo.prune_log(LIST_DAYS).await
    }
}
