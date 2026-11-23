//! Request and response bodies.
use crate::domain::ci;
use crate::domain::models::{
    AccessStatus, LogLine, Person, QueuedCi, RunDetail, RunKind, RunRecord, RunSummary, UserRecord,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use toolkit_durable_execution::observation::{RunHistory, RunProgress};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKindDto {
    Release,
    Ci,
}

impl From<RunKind> for RunKindDto {
    fn from(kind: RunKind) -> Self {
        match kind {
            RunKind::Release => Self::Release,
            RunKind::Ci => Self::Ci,
        }
    }
}

impl From<RunKindDto> for RunKind {
    fn from(kind: RunKindDto) -> Self {
        match kind {
            RunKindDto::Release => Self::Release,
            RunKindDto::Ci => Self::Ci,
        }
    }
}

/// One stored log line. `seq` increases with every line of every run.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct LogLineDto {
    pub seq: i64,
    pub run_id: Uuid,
    pub step: String,
    pub attempt: i32,
    /// `stdout`, `stderr`, `cmd` (a started command) or `info` (the publisher's own notes).
    pub stream: String,
    pub line: String,
    pub ts: DateTime<Utc>,
}

impl From<LogLine> for LogLineDto {
    fn from(l: LogLine) -> Self {
        Self {
            seq: l.seq,
            run_id: l.run_id,
            step: l.step,
            attempt: l.attempt,
            stream: l.stream,
            line: l.line,
            ts: l.ts,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecordDto {
    pub run_id: Uuid,
    pub started_by: String,
    pub dry_run: bool,
    pub created_at: DateTime<Utc>,
    pub facts: serde_json::Value,
}

impl From<RunRecord> for RunRecordDto {
    fn from(r: RunRecord) -> Self {
        Self {
            run_id: r.run_id,
            started_by: r.started_by,
            dry_run: r.dry_run,
            created_at: r.created_at,
            facts: r.facts,
        }
    }
}

fn kind(progress: &RunProgress) -> Option<RunKindDto> {
    RunKind::of(&progress.definition).map(Into::into)
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummaryDto {
    /// `None` for a workflow this version does not know.
    pub kind: Option<RunKindDto>,
    pub progress: RunProgress,
    pub record: Option<RunRecordDto>,
}

impl From<RunSummary> for RunSummaryDto {
    fn from(s: RunSummary) -> Self {
        Self {
            kind: kind(&s.progress),
            progress: s.progress,
            record: s.record.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetailDto {
    pub kind: Option<RunKindDto>,
    pub progress: RunProgress,
    pub history: RunHistory,
    pub record: Option<RunRecordDto>,
}

impl From<RunDetail> for RunDetailDto {
    fn from(d: RunDetail) -> Self {
        Self {
            kind: kind(&d.progress),
            progress: d.progress,
            history: d.history,
            record: d.record.map(Into::into),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct MeDto {
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    /// `admin`, `member`, `pending`, `rejected` or `none` (signed in, never asked).
    pub role: &'static str,
    /// The note of a pending or rejected request.
    pub note: Option<String>,
    /// Requests waiting for a decision; admins only.
    pub pending_requests: Option<usize>,
    /// Release step ids.
    pub steps: &'static [&'static str],
    pub ci: CiInfoDto,
}

/// What the UI needs to show PR CI runs.
#[derive(Debug, Serialize)]
pub struct CiInfoDto {
    pub steps: &'static [&'static str],
    pub jobs: Vec<CiJobDto>,
    pub repository: String,
    /// Why PR CI cannot run on this host, if it cannot.
    pub unavailable: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct CiJobDto {
    pub id: &'static str,
    pub name: &'static str,
    pub workflow: &'static str,
    pub steps: Vec<&'static str>,
    pub note: Option<&'static str>,
}

impl CiInfoDto {
    pub fn new(repository: String, unavailable: Option<&'static str>) -> Self {
        Self {
            steps: &ci::STEPS,
            jobs: ci::jobs::JOBS
                .iter()
                .map(|j| CiJobDto {
                    id: j.id,
                    name: j.name,
                    workflow: j.workflow,
                    steps: j.steps.iter().map(|s| s.name).collect(),
                    note: j.note,
                })
                .collect(),
            repository,
            unavailable,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ListQuery {
    pub kind: Option<RunKindDto>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StartReleaseReq {
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct StartCiReq {
    pub pr: u64,
    /// Only these jobs ("Re-run failed jobs"); omitted: all.
    #[serde(default)]
    pub jobs: Option<Vec<String>>,
}

/// A started run, or for PR CI, a queue entry (`queued`).
#[derive(Debug, Serialize)]
pub struct StartedDto {
    /// The run; the queue entry when `queued`.
    pub id: Uuid,
    pub queued: bool,
    /// The entry's place in the queue (1: next).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
}

impl StartedDto {
    #[must_use]
    pub fn run(id: Uuid) -> Self {
        Self {
            id,
            queued: false,
            position: None,
        }
    }
}

/// A PR CI waiting to start.
#[derive(Debug, Serialize)]
pub struct QueuedCiDto {
    pub id: Uuid,
    /// 1: next.
    pub position: usize,
    pub pr: u64,
    pub jobs: Option<Vec<String>>,
    pub requested_by: String,
    pub queued_at: DateTime<Utc>,
}

impl From<(usize, QueuedCi)> for QueuedCiDto {
    fn from((position, q): (usize, QueuedCi)) -> Self {
        Self {
            id: q.id,
            position,
            pr: q.pr,
            jobs: q.jobs,
            requested_by: q.requested_by,
            queued_at: q.queued_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AcceptedDto {
    pub ok: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LogQuery {
    pub after: i64,
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AccessRequestReq {
    pub note: String,
}

#[derive(Debug, Serialize)]
pub struct UserDto {
    pub subject_id: Uuid,
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub note: String,
    pub status: &'static str,
    pub requested_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
}

impl From<UserRecord> for UserDto {
    fn from(u: UserRecord) -> Self {
        Self {
            subject_id: u.subject_id,
            uid: u.uid,
            email: u.email,
            name: u.name,
            provider: u.provider,
            note: u.note,
            status: u.status.as_str(),
            requested_at: u.requested_at,
            decided_at: u.decided_at,
            decided_by: u.decided_by,
        }
    }
}

/// A configured admin, shown read-only.
#[derive(Debug, Serialize)]
pub struct AdminDto {
    pub uid: String,
    pub email: Option<String>,
}

impl From<Person> for AdminDto {
    fn from(p: Person) -> Self {
        Self {
            uid: p.uid,
            email: p.email,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AccessListDto {
    pub admins: Vec<AdminDto>,
    pub users: Vec<UserDto>,
}

/// `approve` or `reject` in the path.
#[must_use]
pub fn decision(action: &str) -> Option<AccessStatus> {
    match action {
        "approve" => Some(AccessStatus::Approved),
        "reject" => Some(AccessStatus::Rejected),
        _ => None,
    }
}
