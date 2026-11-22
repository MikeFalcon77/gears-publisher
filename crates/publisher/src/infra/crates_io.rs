//! The crates.io sparse index over HTTPS.
use crate::domain::ports::Registry;
use crate::domain::release::crates_io::{index_path, versions};
use async_trait::async_trait;
use std::time::Duration;

/// The crates.io sparse index over HTTPS.
pub struct SparseIndex {
    pub base: String,
    pub http: reqwest::Client,
}

impl SparseIndex {
    pub fn new(base: String) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("gears-publisher (release verification)")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { base, http })
    }
}

#[async_trait]
impl Registry for SparseIndex {
    async fn versions(&self, name: &str) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/{}", self.base.trim_end_matches('/'), index_path(name));
        // The CDN may serve a stale file right after a publish; ask for a fresh one.
        let response = self
            .http
            .get(&url)
            .header("Cache-Control", "no-cache")
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::FORBIDDEN => Ok(Vec::new()),
            status if status.is_success() => Ok(versions(&response.text().await?)),
            status => anyhow::bail!("{url}: HTTP {status}"),
        }
    }
}
