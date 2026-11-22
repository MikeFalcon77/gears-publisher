//! Runs' metadata and logs, as the use cases need them.
use crate::domain::models::{LiveEvent, LogLine, RunRecord};
use crate::domain::ports::Journal;
use async_trait::async_trait;
use tokio::sync::broadcast;
use uuid::Uuid;

#[async_trait]
pub trait RunRepository: Journal {
    /// Every stored line and change notice, from now on.
    fn subscribe(&self) -> broadcast::Receiver<LiveEvent>;
    /// Tells live streams that a run may have changed.
    fn notify(&self, run_id: Uuid);
    async fn create_run(
        &self,
        run_id: Uuid,
        started_by: &str,
        dry_run: bool,
        facts: serde_json::Value,
    ) -> anyhow::Result<()>;
    async fn run(&self, run_id: Uuid) -> anyhow::Result<Option<RunRecord>>;
    async fn runs(&self, ids: Vec<Uuid>) -> anyhow::Result<Vec<RunRecord>>;
    /// Lines of a run after `after`, oldest first.
    async fn lines(&self, run_id: Uuid, after: i64, limit: u64) -> anyhow::Result<Vec<LogLine>>;
    /// Deletes the log of runs older than `days`; the runs themselves stay listed.
    async fn prune_log(&self, days: i64) -> anyhow::Result<u64>;
}
