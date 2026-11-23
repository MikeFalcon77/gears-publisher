//! What the publisher knows about runs, independent of storage and transport.
use chrono::{DateTime, Utc};
use toolkit_durable_execution::observation::{RunHistory, RunProgress};
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Someone who may use the publisher.
#[domain_model]
#[derive(Debug, Clone)]
pub struct Person {
    /// Stable identity: what runs and queued CI are owned by.
    pub subject_id: Uuid,
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub role: Role,
}

impl Person {
    /// How runs and the log name this person: the email, else the name, else the uid.
    #[must_use]
    pub fn label(&self) -> &str {
        self.email
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or(&self.uid)
    }

    /// Whether this person may see a run of `kind` owned by `owner`. Everyone sees
    /// releases; a member sees only the PR CI runs they started.
    #[must_use]
    pub fn may_see(&self, kind: RunKind, owner: Option<Uuid>) -> bool {
        match self.role {
            Role::Admin => true,
            Role::Member => kind == RunKind::Release || owner == Some(self.subject_id),
        }
    }

    /// Whether this person may start, retry, resume or cancel a run of `kind` owned by
    /// `owner`: admins any, members their own PR CI runs.
    #[must_use]
    pub fn may_control(&self, kind: RunKind, owner: Option<Uuid>) -> bool {
        match self.role {
            Role::Admin => true,
            Role::Member => kind == RunKind::Ci && owner == Some(self.subject_id),
        }
    }

    /// Whether this person may start runs of `kind` at all.
    #[must_use]
    pub fn may_start(&self, kind: RunKind) -> bool {
        self.may_control(kind, Some(self.subject_id))
    }
}

/// What a person may do.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Configured in `publisher.admins`: releases, PR CI, approving access.
    Admin,
    /// Approved by an admin: their own PR CI runs, and releases read-only.
    Member,
}

/// Where an access request stands.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessStatus {
    Pending,
    Approved,
    Rejected,
}

impl AccessStatus {
    pub const ALL: [Self; 3] = [Self::Pending, Self::Approved, Self::Rejected];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == value)
    }
}

/// Someone who asked for access, and what became of it. Admins are not stored.
#[domain_model]
#[derive(Debug, Clone)]
pub struct UserRecord {
    pub subject_id: Uuid,
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    /// Firebase's `sign_in_provider`, e.g. `google.com`, `github.com`.
    pub provider: Option<String>,
    /// What they wrote when asking.
    pub note: String,
    pub status: AccessStatus,
    pub requested_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    /// The admin who decided, by label.
    pub decided_by: Option<String>,
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
    /// `None` for runs recorded before owners were (unless a member's label matched).
    pub started_by_subject: Option<Uuid>,
    pub dry_run: bool,
    pub created_at: DateTime<Utc>,
    /// The workflow's state after its last finished step.
    pub facts: serde_json::Value,
}

/// A PR's CI waiting for the run in progress to finish.
#[domain_model]
#[derive(Debug, Clone)]
pub struct QueuedCi {
    pub id: Uuid,
    pub pr: u64,
    /// Run only these jobs; `None`: every job.
    pub jobs: Option<Vec<String>>,
    /// By label, as runs record it.
    pub requested_by: String,
    pub requested_by_subject: Uuid,
    pub queued_at: DateTime<Utc>,
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
