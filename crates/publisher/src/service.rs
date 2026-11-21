//! Use cases behind the REST API. Every durable call uses the service identity;
//! see [`crate::auth`].
use crate::auth::{Directory, Person};
use crate::store::{Journal, LogLine, NewLine, PgStore, RunRecord};
use crate::workflow::{ReleaseInput, ReleaseState};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use toolkit_durable_execution::observation::{RunHistory, RunProgress, WorkflowQuery};
use toolkit_durable_execution::prelude::*;
use toolkit_durable_execution::{ErrorKind, ExecutionInspector};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// How far back the run list reaches; older runs' logs are deleted.
pub const LIST_DAYS: i64 = 180;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("this account is not configured in the publisher")]
    Forbidden,
    #[error("{0}")]
    Invalid(String),
    #[error("the release engine is starting, try again shortly")]
    Starting,
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<toolkit_durable_execution::Error> for ServiceError {
    fn from(error: toolkit_durable_execution::Error) -> Self {
        let message = error.to_string();
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::AlreadyExists | ErrorKind::FailedPrecondition | ErrorKind::Aborted => {
                Self::Conflict(message)
            }
            ErrorKind::InvalidArgument => Self::Invalid(message),
            ErrorKind::Unavailable => Self::Unavailable(message),
            _ => Self::Internal(anyhow::Error::new(error)),
        }
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// A run as the list shows it.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub progress: RunProgress,
    /// `None` for a run whose metadata row is missing.
    pub record: Option<RunRecord>,
}

/// A run with its attempt history.
#[derive(Debug, Clone, Serialize)]
pub struct RunDetail {
    pub progress: RunProgress,
    pub history: RunHistory,
    pub record: Option<RunRecord>,
}

pub struct Service {
    pub client: ExecutionClient,
    pub inspector: ExecutionInspector,
    /// Set once the engine is serving.
    pub workflow: OnceLock<WorkflowRef<ReleaseInput, ReleaseState>>,
    pub store: Arc<PgStore>,
    pub directory: Directory,
    pub service_ctx: SecurityContext,
    start_lock: tokio::sync::Mutex<()>,
}

impl Service {
    #[must_use]
    pub fn new(
        client: ExecutionClient,
        inspector: ExecutionInspector,
        store: Arc<PgStore>,
        directory: Directory,
        service_ctx: SecurityContext,
    ) -> Self {
        Self {
            client,
            inspector,
            workflow: OnceLock::new(),
            store,
            directory,
            service_ctx,
            start_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The configured person behind a request.
    pub fn person(&self, ctx: &SecurityContext) -> Result<&Person> {
        self.directory.person(ctx).ok_or(ServiceError::Forbidden)
    }

    async fn page(&self) -> Result<Vec<RunProgress>> {
        let since = Utc::now() - ChronoDuration::days(LIST_DAYS);
        let page = self
            .inspector
            .list(&self.service_ctx, WorkflowQuery::new(since).limit(100))
            .await?;
        let mut items = page.items;
        items.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        Ok(items)
    }

    pub async fn list(&self) -> Result<Vec<RunSummary>> {
        let items = self.page().await?;
        let mut records: HashMap<Uuid, RunRecord> = self
            .store
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

    /// All runs share one clone, so at most one may be active. `continuing` is the run
    /// a Retry or Resume would set going again: it must be the newest run, otherwise it
    /// would work on a tree and a `main` that later runs have moved on from.
    /// Call with `start_lock` held.
    async fn ensure_exclusive(&self, continuing: Option<Uuid>) -> Result<()> {
        let runs = self.page().await?;
        if let Some(active) = runs
            .iter()
            .find(|p| !p.state.is_settled() && Some(p.id.0) != continuing)
        {
            return Err(ServiceError::Conflict(format!(
                "run {} is still in progress",
                active.id
            )));
        }
        if let Some(id) = continuing
            && let Some(newest) = runs.first()
            && newest.id.0 != id
        {
            return Err(ServiceError::Conflict(format!(
                "run {} was started after this one; only the latest run can be continued, \
                 start a new release instead",
                newest.id
            )));
        }
        Ok(())
    }

    /// Start a release unless one is in progress.
    pub async fn start(&self, person: &Person, input: ReleaseInput) -> Result<Uuid> {
        let workflow = self.workflow.get().ok_or(ServiceError::Starting)?;
        let _guard = self.start_lock.lock().await;
        self.ensure_exclusive(None).await?;
        let dry_run = input.dry_run;
        let started = self
            .client
            .start(&self.service_ctx, workflow, input, StartOptions::new())
            .await?;
        let run_id = started.run_id().0;
        if let Err(error) = self.store.create_run(run_id, &person.email, dry_run).await {
            // A run nobody can attribute is not left running.
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
            return Err(error.into());
        }
        tracing::info!(%run_id, by = %person.email, dry_run, "release started");
        self.store
            .log(NewLine {
                run_id,
                step: "control".into(),
                attempt: 0,
                stream: "info",
                line: format!(
                    "{} started by {}",
                    if dry_run { "dry run" } else { "release" },
                    person.email
                ),
            })
            .await;
        Ok(run_id)
    }

    pub async fn get(&self, id: Uuid) -> Result<RunDetail> {
        let run = RunId(id);
        let progress = self.client.get(&self.service_ctx, run).await?;
        let history = self.client.history(&self.service_ctx, run).await?;
        let record = self.store.run(id).await?;
        Ok(RunDetail {
            progress,
            history,
            record,
        })
    }

    pub async fn progress(&self, id: Uuid) -> Result<RunProgress> {
        Ok(self.client.get(&self.service_ctx, RunId(id)).await?)
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
        self.ensure_exclusive(Some(id)).await?;
        let epoch = self.progress(id).await?.epoch;
        // Noted before the engine is told: the worker's first lines of the new attempt
        // must come after the note that opens it in the UI.
        self.after_command(person, id, "resume").await;
        if let Err(error) = self
            .client
            .resume(&self.service_ctx, RunId(id), ContinueOptions::new(epoch))
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
        self.ensure_exclusive(Some(id)).await?;
        let epoch = self.progress(id).await?.epoch;
        // Noted before the engine is told: the worker's first lines of the new attempt
        // must come after the note that opens it in the UI.
        self.after_command(person, id, "retry").await;
        if let Err(error) = self
            .client
            .retry(&self.service_ctx, RunId(id), ContinueOptions::new(epoch))
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
        self.store
            .log(NewLine {
                run_id: id,
                step: "control".into(),
                attempt: 0,
                stream: "info",
                line,
            })
            .await;
        self.store.notify(id);
    }

    pub async fn log(&self, id: Uuid, after: i64, limit: u64) -> Result<Vec<LogLine>> {
        Ok(self.store.lines(id, after, limit.clamp(1, 5000)).await?)
    }
}
