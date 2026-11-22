//! Published crate versions.
use async_trait::async_trait;

/// Looks up published versions. Faked in tests.
#[async_trait]
pub trait Registry: Send + Sync + 'static {
    /// Non-yanked versions of `name`; empty when the crate does not exist.
    async fn versions(&self, name: &str) -> anyhow::Result<Vec<String>>;
}
