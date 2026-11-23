//! The real engine on PostgreSQL: a run survives a restart in the middle of a step.
//!
//! `PUBLISHER_TEST_PG_URL=postgres://… cargo test -p publisher --test pg_engine -- --ignored`
use async_trait::async_trait;
use publisher::domain::access::{ServiceAuthorizer, service_context};
use publisher::domain::error::DomainError;
use publisher::domain::models::{AccessStatus, Person, QueuedCi, Role};
use publisher::domain::ports::{Cmd, CmdOutput, Exec, ExecError, Registry, StepScope};
use publisher::domain::release::{self as workflow, Deps, ReleaseInput};
use publisher::domain::repos::{NewRequest, RunRepository, UserRepository};
use publisher::domain::service::RunService as Service;
use publisher::domain::steps::BuildSlot;
use publisher::infra::db::{
    migrations,
    repo::{PgRunRepository, PgUserRepository},
};
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
            line if line.contains("rev-parse") => ok("abc123\n"),
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

/// The gear's migrations, once at a time: tests run in parallel on one database.
async fn migrate(db: &toolkit_db::Db) {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let mut all = migrations::all();
    all.extend(toolkit_durable_execution::migrations());
    toolkit_db::migration_runner::run_migrations_for_gear(db, "publisher", all)
        .await
        .unwrap();
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
    migrate(&db).await;
    let store = Arc::new(PgRunRepository::new(toolkit_db::DBProvider::new(
        db.clone(),
    )));
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
        slot: Arc::new(BuildSlot::default()),
    });
    let service = Arc::new(Service::new(
        engine
            .client()
            .with_poll_interval(Duration::from_millis(100)),
        engine.inspector(),
        store,
        service_context(),
        None,
    ));
    let registered = engine
        .registry()
        .register_workflow(workflow::definition(&deps).unwrap())
        .await
        .unwrap();
    let runner = engine.prepare().await.map_err(|e| e.to_string()).unwrap();
    service
        .workflows
        .release
        .set(registered.reference)
        .ok()
        .unwrap();
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

/// A member sees a release but may not start or control one.
async fn a_member_cannot_touch_it(service: &Service, admin: &Person, id: uuid::Uuid) {
    let member = Person {
        subject_id: uuid::Uuid::new_v4(),
        role: Role::Member,
        ..admin.clone()
    };
    assert!(service.get(&member, id).await.is_ok());
    assert!(
        service
            .list(&member, None)
            .await
            .unwrap()
            .iter()
            .any(|r| r.progress.id.0 == id)
    );
    assert!(matches!(
        service
            .start_release(&member, ReleaseInput::default())
            .await,
        Err(DomainError::AdminOnly)
    ));
    assert!(matches!(
        service.cancel(&member, id).await,
        Err(DomainError::AdminOnly)
    ));
    assert!(matches!(
        service.retry(&member, id).await,
        Err(DomainError::AdminOnly)
    ));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs PUBLISHER_TEST_PG_URL"]
async fn a_run_continues_after_a_restart_in_the_middle_of_a_step() {
    let url = std::env::var("PUBLISHER_TEST_PG_URL").expect("PUBLISHER_TEST_PG_URL");
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(work.path().join("gears-rust/.git")).unwrap();
    let queue = format!("publisher-test-{}", uuid::Uuid::new_v4().simple());
    let person = Person {
        subject_id: uuid::Uuid::new_v4(),
        uid: "u".into(),
        email: Some("tester@example.com".into()),
        name: None,
        role: Role::Admin,
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
        .start_release(&person, ReleaseInput { dry_run: false })
        .await
        .unwrap();
    a_member_cannot_touch_it(&first.service, &person, id).await;
    // A second start while one is active is refused.
    assert!(
        first
            .service
            .start_release(&person, ReleaseInput::default())
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
    let detail = second.service.get(&person, id).await.unwrap();
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
    let record = detail.record.unwrap();
    assert_eq!(record.started_by, "tester@example.com");
    assert_eq!(record.started_by_subject, Some(person.subject_id));
    let log = second.service.log(&person, id, 0, 5000).await.unwrap();
    assert!(log.iter().any(|l| l.line == "long build started"));
    assert!(
        log.iter()
            .any(|l| l.line.contains("started by tester@example.com"))
    );
    second.stop.cancel();
    second.task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs PUBLISHER_TEST_PG_URL"]
async fn access_requests_round_trip_in_postgres() {
    let url = std::env::var("PUBLISHER_TEST_PG_URL").expect("PUBLISHER_TEST_PG_URL");
    let db = toolkit_db::connect_db(&url, toolkit_db::ConnectOpts::default())
        .await
        .unwrap();
    migrate(&db).await;
    let users = PgUserRepository::new(toolkit_db::DBProvider::new(db));
    let subject = uuid::Uuid::new_v4();
    let request = |note: &str| NewRequest {
        subject_id: subject,
        uid: format!("uid-{subject}"),
        email: None,
        name: Some("Вася Иванов".into()),
        provider: Some("github.com".into()),
        note: note.into(),
    };
    // A refused insert reports the database's error, not a vanished request.
    let refused = users.request(request("nul\0byte")).await.unwrap_err();
    assert!(!format!("{refused:#}").contains("vanished"), "{refused:#}");
    let first = users.request(request("first")).await.unwrap();
    assert_eq!(first.status, AccessStatus::Pending);
    assert_eq!(
        users.request(request("edited")).await.unwrap().note,
        "edited"
    );
    assert!(
        users
            .list(Some(AccessStatus::Pending))
            .await
            .unwrap()
            .iter()
            .any(|u| u.subject_id == subject)
    );
    let approved = users
        .decide(subject, AccessStatus::Approved, "root@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(approved.status, AccessStatus::Approved);
    assert_eq!(approved.decided_by.as_deref(), Some("root@example.com"));
    // A decided request keeps its note.
    assert_eq!(users.request(request("late")).await.unwrap().note, "edited");
    assert!(
        users
            .decide(uuid::Uuid::new_v4(), AccessStatus::Rejected, "x")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs PUBLISHER_TEST_PG_URL"]
async fn run_owners_and_the_ci_queue_round_trip_in_postgres() {
    let url = std::env::var("PUBLISHER_TEST_PG_URL").expect("PUBLISHER_TEST_PG_URL");
    let db = toolkit_db::connect_db(&url, toolkit_db::ConnectOpts::default())
        .await
        .unwrap();
    migrate(&db).await;
    let runs = PgRunRepository::new(toolkit_db::DBProvider::new(db));
    let owner = uuid::Uuid::new_v4();
    let run = uuid::Uuid::new_v4();
    runs.create_run(run, "dev@example.com", owner, false, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        runs.run(run).await.unwrap().unwrap().started_by_subject,
        Some(owner)
    );

    let now = chrono::Utc::now();
    let entry = |pr: u64, seconds: i64, jobs: Option<Vec<String>>| QueuedCi {
        id: uuid::Uuid::new_v4(),
        pr,
        jobs,
        requested_by: "dev@example.com".into(),
        requested_by_subject: owner,
        queued_at: now + chrono::Duration::seconds(seconds),
    };
    let later = entry(2, 1, None);
    let sooner = entry(1, 0, Some(vec!["fmt".into(), "test".into()]));
    runs.enqueue_ci(later.clone()).await.unwrap();
    runs.enqueue_ci(sooner.clone()).await.unwrap();
    let mine: Vec<QueuedCi> = runs
        .ci_queue()
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.id == later.id || e.id == sooner.id)
        .collect();
    assert_eq!(
        mine.iter().map(|e| e.id).collect::<Vec<_>>(),
        [sooner.id, later.id],
        "oldest first"
    );
    assert_eq!(mine[0].jobs, sooner.jobs);
    assert_eq!(mine[0].pr, 1);
    assert_eq!(mine[0].requested_by_subject, owner);
    let taken = runs.take_ci(sooner.id).await.unwrap().unwrap();
    assert_eq!(taken.jobs, sooner.jobs);
    assert!(
        runs.take_ci(sooner.id).await.unwrap().is_none(),
        "taken once"
    );
    assert!(runs.take_ci(later.id).await.unwrap().is_some());
}
