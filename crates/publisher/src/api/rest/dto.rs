//! Request and response bodies.
use crate::domain::ci;
use crate::domain::models::{LogLine, RunDetail, RunKind, RunRecord, RunSummary};
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
    pub email: String,
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

#[derive(Debug, Serialize)]
pub struct StartedDto {
    pub id: Uuid,
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
