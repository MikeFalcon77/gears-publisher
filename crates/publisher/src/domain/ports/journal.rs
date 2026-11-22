//! What steps write while they run.
use crate::domain::models::NewLine;
use async_trait::async_trait;
use uuid::Uuid;

/// A failure to record never fails a step.
#[async_trait]
pub trait Journal: Send + Sync + 'static {
    async fn log(&self, line: NewLine);
    async fn facts(&self, run_id: Uuid, facts: serde_json::Value);
}

/// In-memory journal for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryJournal {
    pub lines: parking_lot::Mutex<Vec<NewLine>>,
    pub facts: parking_lot::Mutex<Vec<serde_json::Value>>,
}

#[cfg(test)]
#[async_trait]
impl Journal for MemoryJournal {
    async fn log(&self, line: NewLine) {
        self.lines.lock().push(line);
    }
    async fn facts(&self, _: Uuid, facts: serde_json::Value) {
        self.facts.lock().push(facts);
    }
}
