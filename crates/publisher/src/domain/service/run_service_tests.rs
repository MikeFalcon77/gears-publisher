//! Who sees which run, and the PR CI queue, against a fake engine.
use super::*;
use crate::domain::access::{TENANT, service_context};
use crate::domain::ports::Journal;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use std::time::Duration;
use toolkit_durable_execution::contracts::ResultSelector;
use toolkit_durable_execution::observation::{RecordedTime, RunHistory, RunState, WorkflowPage};
use toolkit_durable_execution::testing::{InspectService, RunService as EngineService};
use toolkit_durable_execution::{
    AbortOptions, CancelResult, Error, ExecutionOwner, RunEvent, StartResult,
};

/// Runs exist as soon as they are started and stay running until a test settles them.
#[derive(Default)]
struct FakeEngine {
    runs: Mutex<Vec<RunProgress>>,
}

impl FakeEngine {
    fn add(&self, definition: &str) -> Uuid {
        let now = Utc::now();
        let id = Uuid::new_v4();
        self.runs.lock().push(RunProgress {
            id: RunId(id),
            definition: definition.to_owned(),
            owner: ExecutionOwner {
                tenant_id: TENANT,
                subject_id: crate::domain::access::SERVICE_SUBJECT,
            },
            epoch: 0,
            cursor: 0,
            state: RunState::Running,
            created_at: now,
            updated_at: now,
            steps: Vec::new(),
        });
        id
    }

    fn set(&self, id: Uuid, state: RunState) {
        let mut runs = self.runs.lock();
        let run = runs.iter_mut().find(|r| r.id.0 == id).expect("known run");
        run.state = state;
        run.updated_at = Utc::now();
    }

    fn state(&self, id: Uuid) -> RunState {
        self.find(RunId(id)).state
    }

    fn find(&self, id: RunId) -> RunProgress {
        self.runs
            .lock()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .expect("tests only ask for runs they started")
    }
}

#[async_trait]
impl EngineService for FakeEngine {
    async fn start(
        &self,
        _: &SecurityContext,
        definition: &str,
        _: Value,
        _: StartOptions,
    ) -> std::result::Result<StartResult, Error> {
        Ok(StartResult {
            run_id: RunId(self.add(definition)),
            requested_generation: 0,
            coalesced: false,
        })
    }

    async fn get(&self, _: &SecurityContext, id: RunId) -> std::result::Result<RunProgress, Error> {
        Ok(self.find(id))
    }

    async fn events(
        &self,
        _: &SecurityContext,
        _: RunId,
        _: u64,
        _: u32,
    ) -> std::result::Result<Vec<RunEvent>, Error> {
        Ok(Vec::new())
    }

    async fn history(
        &self,
        _: &SecurityContext,
        _: RunId,
    ) -> std::result::Result<RunHistory, Error> {
        Ok(RunHistory {
            epochs: Vec::new(),
            steps: Vec::new(),
        })
    }

    async fn result(
        &self,
        _: &SecurityContext,
        _: RunId,
        _: &str,
        _: &str,
        _: ResultSelector,
    ) -> std::result::Result<Value, Error> {
        unimplemented!("not used by the service")
    }

    async fn abort(
        &self,
        _: &SecurityContext,
        _: RunId,
        _: AbortOptions,
    ) -> std::result::Result<CancelResult, Error> {
        unimplemented!("not used by the service")
    }

    async fn cancel(
        &self,
        _: &SecurityContext,
        id: RunId,
        options: CancelOptions,
    ) -> std::result::Result<CancelResult, Error> {
        self.set(
            id.0,
            RunState::Cancelling {
                reason: options.reason,
            },
        );
        Ok(CancelResult::Requested)
    }

    async fn retry(
        &self,
        _: &SecurityContext,
        id: RunId,
        _: ContinueOptions,
    ) -> std::result::Result<RunProgress, Error> {
        self.set(id.0, RunState::Running);
        Ok(self.find(id))
    }

    async fn resume(
        &self,
        _: &SecurityContext,
        id: RunId,
        _: ContinueOptions,
    ) -> std::result::Result<RunProgress, Error> {
        self.set(id.0, RunState::Running);
        Ok(self.find(id))
    }
}

#[async_trait]
impl InspectService for FakeEngine {
    async fn inspect(
        &self,
        _: &SecurityContext,
        id: RunId,
    ) -> std::result::Result<RunProgress, Error> {
        Ok(self.find(id))
    }

    async fn list(
        &self,
        _: &SecurityContext,
        _: WorkflowQuery,
    ) -> std::result::Result<WorkflowPage, Error> {
        let items = self.runs.lock().clone();
        Ok(WorkflowPage {
            total: items.len() as u64,
            items,
        })
    }
}

#[derive(Default)]
struct MemoryRuns {
    records: Mutex<Vec<RunRecord>>,
    lines: Mutex<Vec<LogLine>>,
    queue: Mutex<Vec<QueuedCi>>,
    live: OnceLock<broadcast::Sender<LiveEvent>>,
}

impl MemoryRuns {
    fn live(&self) -> &broadcast::Sender<LiveEvent> {
        self.live.get_or_init(|| broadcast::channel(64).0)
    }
}

#[async_trait]
impl Journal for MemoryRuns {
    async fn log(&self, line: NewLine) {
        let mut lines = self.lines.lock();
        let seq = i64::try_from(lines.len()).unwrap() + 1;
        lines.push(LogLine {
            seq,
            run_id: line.run_id,
            step: line.step,
            attempt: i32::try_from(line.attempt).unwrap(),
            stream: line.stream.to_owned(),
            line: line.line,
            ts: Utc::now(),
        });
    }

    async fn facts(&self, _: Uuid, _: Value) {}
}

#[async_trait]
impl RunRepository for MemoryRuns {
    fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.live().subscribe()
    }

    fn notify(&self, run_id: Uuid) {
        let _ = self.live().send(LiveEvent::Changed(run_id));
    }

    async fn create_run(
        &self,
        run_id: Uuid,
        started_by: &str,
        started_by_subject: Uuid,
        dry_run: bool,
        facts: Value,
    ) -> anyhow::Result<()> {
        self.records.lock().push(RunRecord {
            run_id,
            started_by: started_by.to_owned(),
            started_by_subject: Some(started_by_subject),
            dry_run,
            created_at: Utc::now(),
            facts,
        });
        Ok(())
    }

    async fn run(&self, run_id: Uuid) -> anyhow::Result<Option<RunRecord>> {
        Ok(self
            .records
            .lock()
            .iter()
            .find(|r| r.run_id == run_id)
            .cloned())
    }

    async fn runs(&self, ids: Vec<Uuid>) -> anyhow::Result<Vec<RunRecord>> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| ids.contains(&r.run_id))
            .cloned()
            .collect())
    }

    async fn lines(&self, run_id: Uuid, after: i64, limit: u64) -> anyhow::Result<Vec<LogLine>> {
        Ok(self
            .lines
            .lock()
            .iter()
            .filter(|l| l.run_id == run_id && l.seq > after)
            .take(usize::try_from(limit).unwrap())
            .cloned()
            .collect())
    }

    async fn prune_log(&self, _: i64) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn enqueue_ci(&self, entry: QueuedCi) -> anyhow::Result<()> {
        let mut queue = self.queue.lock();
        queue.push(entry);
        queue.sort_by_key(|e| e.queued_at);
        Ok(())
    }

    async fn ci_queue(&self) -> anyhow::Result<Vec<QueuedCi>> {
        Ok(self.queue.lock().clone())
    }

    async fn take_ci(&self, id: Uuid) -> anyhow::Result<Option<QueuedCi>> {
        let mut queue = self.queue.lock();
        Ok(queue
            .iter()
            .position(|e| e.id == id)
            .map(|i| queue.remove(i)))
    }
}

struct Fixture {
    engine: Arc<FakeEngine>,
    repo: Arc<MemoryRuns>,
    service: RunService,
}

fn fixture() -> Fixture {
    let engine = Arc::new(FakeEngine::default());
    let repo = Arc::new(MemoryRuns::default());
    let service = RunService::new(
        ExecutionClient::from_service(engine.clone()),
        ExecutionInspector::from_service(engine.clone()),
        repo.clone(),
        service_context(),
        None,
    );
    let ci = WorkflowBuilder::<CiInput>::new(ci::WORKFLOW)
        .then(Step::new(
            "changes",
            Duration::from_secs(5),
            |_, _: CiInput| async { Ok(CiState::default()) },
        ))
        .build()
        .unwrap();
    service.workflows.ci.set(ci.reference()).ok().unwrap();
    Fixture {
        engine,
        repo,
        service,
    }
}

fn person(n: u128, role: Role) -> Person {
    Person {
        subject_id: Uuid::from_u128(n),
        uid: format!("u{n}"),
        email: Some(format!("u{n}@example.com")),
        name: None,
        role,
    }
}

fn done() -> RunState {
    RunState::Succeeded {
        completed_at: RecordedTime::Unknown,
    }
}

fn started(start: CiStart) -> Uuid {
    match start {
        CiStart::Started(id) => id,
        CiStart::Queued { .. } => panic!("queued, expected a start: {start:?}"),
    }
}

fn queued(start: CiStart) -> (Uuid, usize) {
    match start {
        CiStart::Queued { id, position } => (id, position),
        CiStart::Started(_) => panic!("started, expected the queue: {start:?}"),
    }
}

#[tokio::test]
async fn members_see_their_own_ci_runs_and_every_release() {
    let f = fixture();
    let admin = person(1, Role::Admin);
    let alice = person(2, Role::Member);
    let bob = person(3, Role::Member);
    let run = started(f.service.start_ci(&alice, 7, None).await.unwrap());
    f.engine.set(run, done());

    assert!(f.service.get(&alice, run).await.is_ok());
    assert!(f.service.get(&admin, run).await.is_ok());
    assert!(matches!(
        f.service.get(&bob, run).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        f.service.log(&bob, run, 0, 100).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        f.service.progress_for(&bob, run).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        f.service.cancel(&bob, run).await,
        Err(DomainError::NotFound)
    ));
    assert!(matches!(
        f.service.retry(&bob, run).await,
        Err(DomainError::NotFound)
    ));
    let ids = |runs: Vec<RunSummary>| {
        runs.into_iter()
            .map(|r| r.progress.id.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(f.service.list(&alice, Some(RunKind::Ci)).await.unwrap()),
        [run]
    );
    assert_eq!(ids(f.service.list(&admin, None).await.unwrap()), [run]);
    assert!(f.service.list(&bob, None).await.unwrap().is_empty());
    assert!(f.service.retry(&alice, run).await.is_ok());

    // An admin's release: everyone sees it, only admins control it.
    let release = f.engine.add(crate::domain::release::WORKFLOW);
    f.engine.set(release, done());
    f.repo
        .create_run(
            release,
            admin.label(),
            admin.subject_id,
            false,
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(f.service.get(&bob, release).await.is_ok());
    assert_eq!(
        ids(f.service.list(&bob, Some(RunKind::Release)).await.unwrap()),
        [release]
    );
    assert!(matches!(
        f.service.cancel(&bob, release).await,
        Err(DomainError::AdminOnly)
    ));
    assert!(matches!(
        f.service.retry(&bob, release).await,
        Err(DomainError::AdminOnly)
    ));

    // A run recorded before owners were, or with no record: admins only.
    let legacy = f.engine.add(ci::WORKFLOW);
    f.engine.set(legacy, done());
    assert!(matches!(
        f.service.get(&alice, legacy).await,
        Err(DomainError::NotFound)
    ));
    assert!(f.service.get(&admin, legacy).await.is_ok());
}

#[tokio::test]
async fn another_prs_ci_waits_in_the_queue_and_starts_when_the_run_settles() {
    let f = fixture();
    let admin = person(1, Role::Admin);
    let alice = person(2, Role::Member);
    let bob = person(3, Role::Member);
    let first = started(f.service.start_ci(&alice, 1, None).await.unwrap());

    let (_, place) = queued(f.service.start_ci(&bob, 2, None).await.unwrap());
    assert_eq!(place, 1);
    // Asked again: the entry is replaced, not doubled.
    let (bobs, place) = queued(
        f.service
            .start_ci(&bob, 2, Some(vec!["fmt".into()]))
            .await
            .unwrap(),
    );
    assert_eq!(place, 1);
    let (alices, place) = queued(f.service.start_ci(&alice, 3, None).await.unwrap());
    assert_eq!(place, 2);

    let view = |p: &Person| {
        let service = &f.service;
        let p = p.clone();
        async move {
            service
                .ci_queue(&p)
                .await
                .unwrap()
                .into_iter()
                .map(|(place, e)| (place, e.id))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(view(&bob).await, [(1, bobs)]);
    assert_eq!(view(&alice).await, [(2, alices)]);
    assert_eq!(view(&admin).await, [(1, bobs), (2, alices)]);
    assert!(matches!(
        f.service.cancel_queued(&bob, alices).await,
        Err(DomainError::NotFound)
    ));

    assert_eq!(
        f.service.dispatch_ci().await.unwrap(),
        None,
        "PR 1 is running"
    );
    f.engine.set(first, done());
    let second = f.service.dispatch_ci().await.unwrap().expect("bob's turn");
    let record = f.repo.run(second).await.unwrap().unwrap();
    assert_eq!(record.started_by_subject, Some(bob.subject_id));
    assert_eq!(record.started_by, "u3@example.com");
    assert_eq!(record.facts["pr_number"], 2);
    assert_eq!(record.facts["only"], serde_json::json!(["fmt"]));
    let log = f.service.log(&bob, second, 0, 100).await.unwrap();
    assert!(
        log.iter().any(|l| l
            .line
            .starts_with("PR #2 CI started by u3@example.com (re-run of fmt), queued at")),
        "{log:?}"
    );
    assert_eq!(
        f.service.dispatch_ci().await.unwrap(),
        None,
        "PR 2 is running"
    );
    assert_eq!(view(&alice).await, [(1, alices)]);

    // Taken out of the queue, it never starts.
    f.service.cancel_queued(&alice, alices).await.unwrap();
    f.engine.set(second, done());
    assert_eq!(f.service.dispatch_ci().await.unwrap(), None);
    assert!(f.repo.queue.lock().is_empty());
}

#[tokio::test]
async fn nothing_running_starts_at_once_and_waiting_entries_go_first() {
    let f = fixture();
    let alice = person(2, Role::Member);
    let bob = person(3, Role::Member);
    let first = started(f.service.start_ci(&alice, 1, None).await.unwrap());
    let (bobs, _) = queued(f.service.start_ci(&bob, 2, None).await.unwrap());
    // The run settles before the dispatcher's tick: the next request finds bob waiting.
    f.engine.set(first, done());
    let (_, place) = queued(f.service.start_ci(&alice, 3, None).await.unwrap());
    assert_eq!(place, 2);
    let running: Vec<_> = f.service.unsettled(RunKind::Ci).await.unwrap();
    assert_eq!(running.len(), 1, "bob's entry started in the same call");
    let record = f.repo.run(running[0].id.0).await.unwrap().unwrap();
    assert_eq!(record.started_by_subject, Some(bob.subject_id));
    assert!(f.repo.queue.lock().iter().all(|e| e.id != bobs));
}

#[tokio::test]
async fn a_new_run_of_the_same_pr_replaces_only_what_the_caller_controls() {
    let f = fixture();
    let admin = person(1, Role::Admin);
    let alice = person(2, Role::Member);
    let bob = person(3, Role::Member);
    let first = started(f.service.start_ci(&alice, 1, None).await.unwrap());
    // Her own run of the same PR: cancelled, and the new one starts while it winds down.
    let second = started(f.service.start_ci(&alice, 1, None).await.unwrap());
    assert!(matches!(f.engine.state(first), RunState::Cancelling { .. }));

    // Bob's request for the same PR does not touch alice's run: it waits.
    let (bobs, _) = queued(f.service.start_ci(&bob, 1, None).await.unwrap());
    assert!(matches!(f.engine.state(second), RunState::Running));

    // An admin's replaces both.
    started(f.service.start_ci(&admin, 1, None).await.unwrap());
    assert!(matches!(
        f.engine.state(second),
        RunState::Cancelling { .. }
    ));
    assert!(f.repo.queue.lock().iter().all(|e| e.id != bobs));
}
