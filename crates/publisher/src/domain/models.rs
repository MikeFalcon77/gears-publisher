//! What the publisher knows about runs, independent of storage and transport.
use chrono::{DateTime, Utc};
use toolkit_durable_execution::observation::{RunHistory, RunProgress};
use toolkit_macros::domain_model;
use uuid::Uuid;

/// An allowlisted person.
#[domain_model]
#[derive(Debug, Clone)]
pub struct Person {
    pub uid: String,
    pub email: String,
}

/// Which workflow a run belongs to.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunKind {
    /// `gears-release.v1`.
    Release,
    /// `gears-pr-ci.v1`.
    Ci,
}

impl RunKind {
    pub const ALL: [Self; 2] = [Self::Release, Self::Ci];

    /// The durable workflow name.
    #[must_use]
    pub fn workflow(self) -> &'static str {
        match self {
            Self::Release => super::release::WORKFLOW,
            Self::Ci => super::ci::WORKFLOW,
        }
    }

    #[must_use]
    pub fn of(definition: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.workflow() == definition)
    }

    /// Step ids in execution order.
    #[must_use]
    pub fn steps(self) -> &'static [&'static str] {
        match self {
            Self::Release => &super::release::STEPS,
            Self::Ci => &super::ci::STEPS,
        }
    }
}

/// One stored log line. `seq` increases with every line of every run.
#[domain_model]
#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: i64,
    pub run_id: Uuid,
    pub step: String,
    pub attempt: i32,
    /// `stdout`, `stderr`, `cmd` (a started command) or `info` (the publisher's own notes).
    pub stream: String,
    pub line: String,
    pub ts: DateTime<Utc>,
}

/// A line before it is stored.
#[domain_model]
#[derive(Debug, Clone)]
pub struct NewLine {
    pub run_id: Uuid,
    pub step: String,
    pub attempt: u32,
    pub stream: &'static str,
    pub line: String,
}

/// A run's metadata row: who started it and what its steps found.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub run_id: Uuid,
    pub started_by: String,
    pub dry_run: bool,
    pub created_at: DateTime<Utc>,
    /// The workflow's state after its last finished step.
    pub facts: serde_json::Value,
}

/// Pushed to live streams.
#[domain_model]
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Log(LogLine),
    /// A step saved new facts, or the run's state may have changed.
    Changed(Uuid),
}

/// A run as the list shows it.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub progress: RunProgress,
    /// `None` for a run whose metadata row is missing.
    pub record: Option<RunRecord>,
}

/// A run with its attempt history.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RunDetail {
    pub progress: RunProgress,
    pub history: RunHistory,
    pub record: Option<RunRecord>,
}
