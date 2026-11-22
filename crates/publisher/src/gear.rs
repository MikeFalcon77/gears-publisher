//! Gear wiring: configuration, the durable engine, REST routes and the lifecycle.
use crate::api::rest::{self as rest, AppState};
use crate::config::PublisherConfig;
use crate::domain::access::{Directory, ServiceAuthorizer, service_context};
use crate::domain::ci::{self, CiDeps};
use crate::domain::ports::{Exec, Journal};
use crate::domain::release::{self, Deps};
use crate::domain::repos::RunRepository;
use crate::domain::service::RunService;
use crate::domain::steps::BuildSlot;
use crate::infra::crates_io::SparseIndex;
use crate::infra::db::{migrations, repo::PgRunRepository};
use crate::infra::exec::{ProcessExec, RefuseUntrusted, RunAs};
use async_trait::async_trait;
use parking_lot::Mutex;
use sea_orm_migration::MigrationTrait;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::{DatabaseCapability, Gear, GearCtx, RestApiCapability};
use toolkit_durable_execution::{Engine, EngineBuilder, ExecutionMode, QueueConfig};

/// Activities at a time: a release step, a PR CI step and a superseded CI run winding down.
const CONCURRENCY: usize = 3;

#[toolkit::gear(
    name = "publisher",
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout_fn = "stop_timeout")
)]
#[derive(Default)]
pub struct PublisherGear {
    engine: Mutex<Option<Engine>>,
    release: OnceLock<Arc<Deps>>,
    ci: OnceLock<Arc<CiDeps>>,
    service: OnceLock<Arc<RunService>>,
    repository: OnceLock<String>,
    web_root: OnceLock<PathBuf>,
    firebase_project_id: OnceLock<String>,
    shutdown_timeout: OnceLock<Duration>,
}

impl DatabaseCapability for PublisherGear {
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> {
        let mut all = migrations::all();
        all.extend(toolkit_durable_execution::migrations());
        all
    }
}

/// Commands as `user` (none: as the service), with `tmp_dir` as their own `TMPDIR`.
fn process_exec(config: &PublisherConfig, user: Option<&str>, tmp_dir: &Path) -> ProcessExec {
    let run_as = user.map(|user| {
        let own_tmp = tmp_dir.is_dir().then(|| tmp_dir.display().to_string());
        if own_tmp.is_none() {
            tracing::warn!(
                user,
                tmp_dir = %tmp_dir.display(),
                "no TMPDIR of its own: the build users share /tmp (run deploy/provision.sh)"
            );
        }
        RunAs {
            user: user.to_owned(),
            sudo: config.sudo_bin.clone(),
            tmp_dir: own_tmp,
        }
    });
    ProcessExec {
        base_env: config.env.clone(),
        run_as,
    }
}

#[async_trait]
impl Gear for PublisherGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: PublisherConfig = ctx.config_expanded()?;
        let db = ctx.db_required()?;
        let repo = Arc::new(PgRunRepository::new(db.clone()));
        let queue = QueueConfig::new(config.queue_dsn.clone(), config.queue.clone())
            .concurrency(NonZeroUsize::new(CONCURRENCY).unwrap_or(NonZeroUsize::MIN));
        let engine = Engine::builder(db.db(), Arc::new(ServiceAuthorizer))
            .mode(ExecutionMode::Worker(queue))
            .build()?;
        if config.users.is_empty() {
            tracing::warn!("publisher.users is empty: nobody can start a run");
        }
        let ci_refusal = config.ci_refusal();
        if let Some(refusal) = ci_refusal {
            tracing::warn!(refusal, "PR CI is disabled");
        }
        let service = Arc::new(RunService::new(
            engine.client(),
            engine.inspector(),
            Arc::clone(&repo) as Arc<dyn RunRepository>,
            Directory::from_config(&config),
            service_context(),
            ci_refusal,
        ));
        let journal = Arc::clone(&repo) as Arc<dyn Journal>;
        let slot = Arc::new(BuildSlot::default());
        let release = Arc::new(Deps {
            exec: Arc::new(process_exec(
                &config,
                config.build_user(),
                &config.tmp_dir(),
            )),
            journal: Arc::clone(&journal),
            registry: Arc::new(SparseIndex::new(config.crates_index_url.clone())?),
            slot: Arc::clone(&slot),
            config: config.clone(),
        });
        // A CI run recovered after PR CI was disabled must not run the PR as the service.
        let ci_exec: Arc<dyn Exec> = match ci_refusal {
            None => Arc::new(process_exec(
                &config,
                config.ci_user(),
                &config.ci_tmp_dir(),
            )),
            Some(_) => Arc::new(RefuseUntrusted(process_exec(
                &config,
                None,
                &config.ci_tmp_dir(),
            ))),
        };
        let ci = Arc::new(CiDeps {
            exec: ci_exec,
            journal,
            slot,
            clone: tokio::sync::Mutex::new(()),
            config: config.clone(),
        });
        let already = || anyhow::anyhow!("publisher gear initialized twice");
        self.repository
            .set(config.github_repository.clone())
            .map_err(|_| already())?;
        self.web_root
            .set(PathBuf::from(&config.web_root))
            .map_err(|_| already())?;
        self.firebase_project_id
            .set(config.firebase_project_id.clone())
            .map_err(|_| already())?;
        self.shutdown_timeout
            .set(engine.shutdown_timeout())
            .map_err(|_| already())?;
        self.release.set(release).map_err(|_| already())?;
        self.ci.set(ci).map_err(|_| already())?;
        self.service.set(service).map_err(|_| already())?;
        *self.engine.lock() = Some(engine);
        Ok(())
    }
}

impl PublisherGear {
    /// Register the workflows, start delivery and serve until the host stops.
    async fn serve(self: Arc<Self>, cancel: CancellationToken) -> anyhow::Result<()> {
        let not_ready = || anyhow::anyhow!("publisher gear is not initialized");
        let engine = self.engine.lock().take().ok_or_else(not_ready)?;
        let release = self.release.get().ok_or_else(not_ready)?;
        let ci = self.ci.get().ok_or_else(not_ready)?;
        let service = self.service.get().ok_or_else(not_ready)?;
        let registry = engine.registry();
        let release_ref = registry
            .register_workflow(release::definition(release)?)
            .await?
            .reference;
        let ci_ref = registry
            .register_workflow(ci::definition(ci)?)
            .await?
            .reference;
        let runner = engine
            .prepare()
            .await
            .map_err(|e| anyhow::anyhow!("durable engine failed to start: {e}"))?;
        let served_twice = || anyhow::anyhow!("publisher gear served twice");
        service
            .workflows
            .release
            .set(release_ref)
            .map_err(|_| served_twice())?;
        service
            .workflows
            .ci
            .set(ci_ref)
            .map_err(|_| served_twice())?;
        tracing::info!(
            workflows = ?[release::WORKFLOW, ci::WORKFLOW],
            build_user = release.config.build_user().unwrap_or("(none: the service user)"),
            ci_user = release.config.ci_user().unwrap_or("(none)"),
            "engine is serving"
        );
        tokio::spawn(prune_logs(Arc::clone(service), cancel.clone()));
        Ok(runner.run(cancel).await?)
    }

    fn stop_timeout(&self) -> Duration {
        self.shutdown_timeout
            .get()
            .copied()
            .unwrap_or(EngineBuilder::DEFAULT_SHUTDOWN_TIMEOUT)
    }
}

/// Daily: delete the logs of runs that have dropped out of the run list.
async fn prune_logs(service: Arc<RunService>, cancel: CancellationToken) {
    loop {
        match service.prune_logs().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(lines = n, "pruned old run logs"),
            Err(error) => tracing::warn!(%error, "failed to prune old run logs"),
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_hours(24)) => {}
            () = cancel.cancelled() => return,
        }
    }
}

impl RestApiCapability for PublisherGear {
    fn register_rest(
        &self,
        _: &GearCtx,
        router: axum::Router,
        openapi: &dyn OpenApiRegistry,
    ) -> anyhow::Result<axum::Router> {
        let not_ready = || anyhow::anyhow!("publisher gear is not initialized");
        let state = AppState {
            service: Arc::clone(self.service.get().ok_or_else(not_ready)?),
            repository: self.repository.get().ok_or_else(not_ready)?.clone(),
            web_root: self.web_root.get().ok_or_else(not_ready)?.clone(),
            csp: rest::content_security_policy(
                self.firebase_project_id.get().ok_or_else(not_ready)?,
            ),
        };
        Ok(rest::register_routes(router, openapi, Arc::new(state)))
    }
}
