//! The real engine on PostgreSQL: a run survives a restart in the middle of a step.
//!
//! `PUBLISHER_TEST_PG_URL=postgres://… cargo test -p publisher --test pg_engine -- --ignored`
use async_trait::async_trait;
use publisher::auth::{Person, ServiceAuthorizer, service_context};
use publisher::crates_io::Registry;
use publisher::runner::{Cmd, CmdOutput, Exec, ExecError, StepScope};
use publisher::service::Service;
use publisher::store::PgStore;
use publisher::workflow::{self, Deps, ReleaseInput};
use secrecy::SecretString;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use toolkit_durable_execution::observation::RunStatus;
use toolkit_durable_execution::{Engine, ExecutionMode, QueueConfig};

/// `make` runs until cancelled the first time and succeeds afterwards; nothing to release.
struct Fake {
    makes: AtomicUsize,
}

#[async_trait]
impl Exec for Fake {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
        let ok = |stdout: &str| CmdOutput {
            code: Some(0),
            stdout: stdout.to_owned(),
            ..CmdOutput::default()
        };
        if cmd.program == "make" && self.makes.fetch_add(1, Ordering::SeqCst) == 0 {
            scope.info("long build started").await;
            scope.cancel.cancelled().await;
            return Err(ExecError::Cancelled);
        }
        if cmd.display().contains("release_retry.py collect")
            && let Some((_, path)) = cmd.env.iter().find(|(k, _)| k == "GITHUB_OUTPUT")
        {
            std::fs::write(path, "releases=[]\n").unwrap();
        }
        Ok(match cmd.display().as_str() {
            line if line.contains("release-pr") => ok(r#"{"prs":[]}"#),
            line if line.contains("auth token") => ok("gho_token_x\n"),
            _ => ok(""),
        })
    }
}

struct NoRegistry;
#[async_trait]
impl Registry for NoRegistry {
    async fn versions(&self, _: &str) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct Server {
    service: Arc<Service>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

async fn start(url: &str, queue: &str, work: &std::path::Path, exec: Arc<dyn Exec>) -> Server {
    let db = toolkit_db::connect_db(url, toolkit_db::ConnectOpts::default())
        .await
        .unwrap();
    let mut migrations = publisher::store::migrations::all();
    migrations.extend(toolkit_durable_execution::migrations());
    toolkit_db::migration_runner::run_migrations_for_gear(&db, "publisher", migrations)
        .await
        .unwrap();
    let store = Arc::new(PgStore::new(toolkit_db::DBProvider::new(db.clone())));
    let engine = Engine::builder(db, Arc::new(ServiceAuthorizer))
        .mode(ExecutionMode::Worker(QueueConfig::new(
            SecretString::from(url.to_owned()),
            queue,
        )))
        .dispatch_interval(Duration::from_secs(1))
        .lease(Duration::from_secs(9))
        .heartbeat(Duration::from_secs(2))
        .shutdown_timeout(Duration::from_secs(6))
        .build()
        .unwrap();
    let secrets = work.join("secrets.env");
    std::fs::write(&secrets, "CARGO_REGISTRY_TOKEN=cio_test_token\n").unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "secrets_file": secrets.display().to_string(),
        "work_dir": work.display().to_string(),
        "target_dir": work.join("target").display().to_string(),
        "queue_dsn": url,
        "firebase_project_id": "test",
    }))
    .unwrap();
    let deps = Arc::new(Deps {
        config,
        exec,
        journal: store.clone(),
        registry: Arc::new(NoRegistry),
    });
    let service = Arc::new(Service::new(
        engine
            .client()
            .with_poll_interval(Duration::from_millis(100)),
        engine.inspector(),
        store,
        publisher::auth::Directory::default(),
        service_context(),
    ));
    let registered = engine
        .registry()
        .register_workflow(workflow::definition(&deps).unwrap())
        .await
        .unwrap();
    let runner = engine.prepare().await.map_err(|e| e.to_string()).unwrap();
    service.workflow.set(registered.reference).ok().unwrap();
    let stop = CancellationToken::new();
    let token = stop.clone();
    let task = tokio::spawn(async move {
        runner.run(token).await.unwrap();
    });
    Server {
        service,
        stop,
        task,
    }
}

async fn wait_for(
    service: &Service,
    id: uuid::Uuid,
    what: &str,
    done: impl Fn(&toolkit_durable_execution::observation::RunProgress) -> bool,
) -> toolkit_durable_execution::observation::RunProgress {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let progress = service.progress(id).await.unwrap();
        if done(&progress) {
            return progress;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            progress.state
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs PUBLISHER_TEST_PG_URL"]
async fn a_run_continues_after_a_restart_in_the_middle_of_a_step() {
    let url = std::env::var("PUBLISHER_TEST_PG_URL").expect("PUBLISHER_TEST_PG_URL");
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("gears-rust/.git")).unwrap();
    let queue = format!("publisher-test-{}", uuid::Uuid::new_v4().simple());
    let person = Person {
        uid: "u".into(),
        email: "tester@example.com".into(),
    };

    let first = start(
        &url,
        &queue,
        work.path(),
        Arc::new(Fake {
            makes: AtomicUsize::new(0),
        }),
    )
    .await;
    let id = first
        .service
        .start(&person, ReleaseInput { dry_run: false })
        .await
        .unwrap();
    // A second start while one is active is refused.
    assert!(
        first
            .service
            .start(&person, ReleaseInput::default())
            .await
            .is_err()
    );
    wait_for(&first.service, id, "checks to run", |p| {
        p.steps.iter().any(|s| {
            s.id.as_str() == "checks"
                && matches!(
                    s.state,
                    toolkit_durable_execution::observation::StepState::Running { .. }
                )
        })
    })
    .await;

    // "systemctl restart": stop the engine while `checks` runs.
    first.stop.cancel();
    first.task.await.unwrap();

    let second = start(
        &url,
        &queue,
        work.path(),
        Arc::new(Fake {
            makes: AtomicUsize::new(1),
        }),
    )
    .await;
    let done = wait_for(&second.service, id, "the run to settle", |p| {
        p.state.is_settled()
    })
    .await;
    assert_eq!(
        done.state.status(),
        RunStatus::Succeeded,
        "{:?}",
        done.state
    );
    let detail = second.service.get(id).await.unwrap();
    let attempts = |step: &str| {
        detail
            .progress
            .steps
            .iter()
            .find(|s| s.id.as_str() == step)
            .map(|s| s.attempts)
    };
    assert_eq!(attempts("prepare"), Some(1), "prepare is not repeated");
    assert_eq!(attempts("checks"), Some(2), "checks restarts once");
    assert_eq!(detail.record.unwrap().started_by, "tester@example.com");
    let log = second.service.log(id, 0, 5000).await.unwrap();
    assert!(log.iter().any(|l| l.line == "long build started"));
    assert!(
        log.iter()
            .any(|l| l.line.contains("started by tester@example.com"))
    );
    second.stop.cancel();
    second.task.await.unwrap();
}
