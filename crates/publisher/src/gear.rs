//! Gear wiring: configuration, the durable engine, REST routes and the lifecycle.
use crate::api;
use crate::auth::{Directory, ServiceAuthorizer, service_context};
use crate::config::PublisherConfig;
use crate::crates_io::SparseIndex;
use crate::runner::{ProcessExec, RunAs};
use crate::service::Service;
use crate::store::{PgStore, migrations};
use crate::workflow::{self, Deps};
use async_trait::async_trait;
use parking_lot::Mutex;
use sea_orm_migration::MigrationTrait;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use toolkit::api::OpenApiRegistry;
use toolkit::{DatabaseCapability, Gear, GearCtx, RestApiCapability};
use toolkit_durable_execution::{Engine, EngineBuilder, ExecutionMode, QueueConfig};

#[toolkit::gear(
    name = "publisher",
    capabilities = [db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout_fn = "stop_timeout")
)]
#[derive(Default)]
pub struct PublisherGear {
    engine: Mutex<Option<Engine>>,
    deps: OnceLock<Arc<Deps>>,
    service: OnceLock<Arc<Service>>,
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

#[async_trait]
impl Gear for PublisherGear {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let config: PublisherConfig = ctx.config_expanded()?;
        let db = ctx.db_required()?;
        let store = Arc::new(PgStore::new(db.clone()));
        let queue = QueueConfig::new(config.queue_dsn.clone(), config.queue.clone())
            .concurrency(NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN));
        let engine = Engine::builder(db.db(), Arc::new(ServiceAuthorizer))
            .mode(ExecutionMode::Worker(queue))
            .build()?;
        let directory = Directory::from_config(&config);
        if config.users.is_empty() {
            tracing::warn!("publisher.users is empty: nobody can start a release");
        }
        let service = Arc::new(Service::new(
            engine.client(),
            engine.inspector(),
            Arc::clone(&store),
            directory,
            service_context(),
        ));
        let deps = Arc::new(Deps {
            exec: Arc::new(ProcessExec {
                base_env: config.env.clone(),
                run_as: config.build_user().map(|user| RunAs {
                    user: user.to_owned(),
                    sudo: config.sudo_bin.clone(),
                }),
            }),
            journal: store,
            registry: Arc::new(SparseIndex::new(config.crates_index_url.clone())?),
            config: config.clone(),
        });
        let already = || anyhow::anyhow!("publisher gear initialized twice");
        self.web_root
            .set(PathBuf::from(&config.web_root))
            .map_err(|_| already())?;
        self.firebase_project_id
            .set(config.firebase_project_id.clone())
            .map_err(|_| already())?;
        self.shutdown_timeout
            .set(engine.shutdown_timeout())
            .map_err(|_| already())?;
        self.deps.set(deps).map_err(|_| already())?;
        self.service.set(service).map_err(|_| already())?;
        *self.engine.lock() = Some(engine);
        Ok(())
    }
}

impl PublisherGear {
    /// Register the workflow, start delivery and serve until the host stops.
    async fn serve(self: Arc<Self>, cancel: CancellationToken) -> anyhow::Result<()> {
        let not_ready = || anyhow::anyhow!("publisher gear is not initialized");
        let engine = self.engine.lock().take().ok_or_else(not_ready)?;
        let deps = self.deps.get().ok_or_else(not_ready)?;
        let service = self.service.get().ok_or_else(not_ready)?;
        let registered = engine
            .registry()
            .register_workflow(workflow::definition(deps)?)
            .await?;
        let runner = engine
            .prepare()
            .await
            .map_err(|e| anyhow::anyhow!("durable engine failed to start: {e}"))?;
        service
            .workflow
            .set(registered.reference)
            .map_err(|_| anyhow::anyhow!("publisher gear served twice"))?;
        tracing::info!(
            workflow = workflow::WORKFLOW,
            build_user = deps
                .config
                .build_user()
                .unwrap_or("(none: the service user)"),
            "release engine is serving"
        );
        tokio::spawn(prune_logs(Arc::clone(&service.store), cancel.clone()));
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
async fn prune_logs(store: Arc<PgStore>, cancel: CancellationToken) {
    loop {
        match store.prune_log(crate::service::LIST_DAYS).await {
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
        let state = api::AppState {
            service: Arc::clone(self.service.get().ok_or_else(not_ready)?),
            web_root: self.web_root.get().ok_or_else(not_ready)?.clone(),
            csp: api::content_security_policy(
                self.firebase_project_id.get().ok_or_else(not_ready)?,
            ),
        };
        Ok(api::routes(router, openapi, Arc::new(state)))
    }
}
