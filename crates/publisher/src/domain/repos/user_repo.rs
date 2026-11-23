//! Access requests and their decisions.
use crate::domain::models::{AccessStatus, UserRecord};
use async_trait::async_trait;
use uuid::Uuid;

/// What someone sends when asking for access; names come from their token.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub subject_id: Uuid,
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub note: String,
}

#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn get(&self, subject_id: Uuid) -> anyhow::Result<Option<UserRecord>>;
    /// Stores a new pending request, or refreshes the note and names of one still
    /// pending. A decided request is left as it is; returns the stored record.
    async fn request(&self, request: NewRequest) -> anyhow::Result<UserRecord>;
    /// Every request, newest first; only those in `status` when given.
    async fn list(&self, status: Option<AccessStatus>) -> anyhow::Result<Vec<UserRecord>>;
    /// Records an admin's decision; `None` when there is no such request.
    async fn decide(
        &self,
        subject_id: Uuid,
        status: AccessStatus,
        by: &str,
    ) -> anyhow::Result<Option<UserRecord>>;
}

/// In memory, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryUsers(parking_lot::Mutex<Vec<UserRecord>>);

#[cfg(test)]
#[async_trait]
impl UserRepository for MemoryUsers {
    async fn get(&self, subject_id: Uuid) -> anyhow::Result<Option<UserRecord>> {
        Ok(self
            .0
            .lock()
            .iter()
            .find(|u| u.subject_id == subject_id)
            .cloned())
    }

    async fn request(&self, request: NewRequest) -> anyhow::Result<UserRecord> {
        let mut users = self.0.lock();
        if let Some(user) = users
            .iter_mut()
            .find(|u| u.subject_id == request.subject_id)
        {
            if user.status == AccessStatus::Pending {
                user.note = request.note;
                user.email = request.email;
                user.name = request.name;
                user.provider = request.provider;
            }
            return Ok(user.clone());
        }
        let user = UserRecord {
            subject_id: request.subject_id,
            uid: request.uid,
            email: request.email,
            name: request.name,
            provider: request.provider,
            note: request.note,
            status: AccessStatus::Pending,
            requested_at: chrono::Utc::now(),
            decided_at: None,
            decided_by: None,
        };
        users.push(user.clone());
        Ok(user)
    }

    async fn list(&self, status: Option<AccessStatus>) -> anyhow::Result<Vec<UserRecord>> {
        let mut users: Vec<_> = self
            .0
            .lock()
            .iter()
            .filter(|u| status.is_none_or(|s| u.status == s))
            .cloned()
            .collect();
        users.sort_by_key(|u| std::cmp::Reverse(u.requested_at));
        Ok(users)
    }

    async fn decide(
        &self,
        subject_id: Uuid,
        status: AccessStatus,
        by: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let mut users = self.0.lock();
        Ok(users
            .iter_mut()
            .find(|u| u.subject_id == subject_id)
            .map(|user| {
                user.status = status;
                user.decided_at = Some(chrono::Utc::now());
                user.decided_by = Some(by.to_owned());
                user.clone()
            }))
    }
}
