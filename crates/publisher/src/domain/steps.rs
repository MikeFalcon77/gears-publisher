//! What every workflow's steps share: the step wrapper, running commands, GitHub
//! credentials and the build slot.
use crate::config::PublisherConfig;
use crate::domain::ports::{Cmd, CmdOutput, Exec, ExecError, Journal, StepScope};
use parking_lot::Mutex;
use secrecy::SecretString;
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, SemaphorePermit};
use toolkit_durable_execution::prelude::*;
use toolkit_durable_execution::workflow::Payload;

/// Wrap a step function: scope, start/finish notes, and the state saved as the run's
/// facts after every success.
pub fn step<D, I, S, F, Fut>(
    deps: &Arc<D>,
    journal: &Arc<dyn Journal>,
    id: &'static str,
    timeout: Duration,
    retry: RetryPolicy,
    f: F,
) -> Step<I, S>
where
    D: Send + Sync + 'static,
    I: Payload,
    S: Payload + Serialize,
    F: Fn(Arc<D>, StepScope, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<S, ActivityError>> + Send + 'static,
{
    let deps = Arc::clone(deps);
    let journal = Arc::clone(journal);
    Step::new(id, timeout, move |ctx: ActivityContext, input: I| {
        let scope = StepScope {
            run_id: ctx.run_id().0,
            step: id.to_owned(),
            attempt: ctx.attempt(),
            cancel: ctx.cancellation().clone(),
            journal: Arc::clone(&journal),
        };
        let work = f(Arc::clone(&deps), scope.clone(), input);
        let journal = Arc::clone(&journal);
        async move {
            scope
                .info(format!("{id}: attempt {} started", scope.attempt))
                .await;
            let result = work.await;
            match &result {
                Ok(state) => {
                    scope.info(format!("{id}: done")).await;
                    if let Ok(facts) = serde_json::to_value(state) {
                        journal.facts(scope.run_id, facts).await;
                    }
                }
                Err(ActivityError::Retryable { code, retry_after }) => {
                    let when = retry_after
                        .map(|d| format!(" in at least {}s", d.as_secs()))
                        .unwrap_or_default();
                    scope
                        .info(format!("{id}: failed ({code}), will retry{when}"))
                        .await;
                }
                Err(error) => scope.info(format!("{id}: failed ({error})")).await,
            }
            result
        }
    })
    .retry(retry)
}

#[must_use]
pub fn exec_error(error: &ExecError) -> ActivityError {
    match error {
        ExecError::Cancelled => ActivityError::Cancelled,
        ExecError::Spawn { .. } | ExecError::Io { .. } => ActivityError::retryable("spawn_failed"),
    }
}

/// A command that ends because the run is cancelled or the service stops, or that
/// was killed by a signal, is an interruption, never a verdict on the run.
pub async fn run(exec: &dyn Exec, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ActivityError> {
    let out = exec.run(scope, cmd).await.map_err(|e| exec_error(&e))?;
    if scope.cancel.is_cancelled() {
        return Err(ActivityError::Cancelled);
    }
    if out.code.is_none() {
        scope.info("the command was killed by a signal").await;
        return Err(ActivityError::retryable("killed"));
    }
    Ok(out)
}

/// Run and require exit 0; otherwise fail with `code`.
pub async fn run_ok(
    exec: &dyn Exec,
    scope: &StepScope,
    cmd: Cmd,
    fail: fn(&'static str) -> ActivityError,
    code: &'static str,
) -> Result<CmdOutput, ActivityError> {
    let out = run(exec, scope, cmd).await?;
    if out.success() {
        Ok(out)
    } else {
        Err(fail(code))
    }
}

/// `gh` runs as the service (it holds the GitHub login) and outside any clone.
#[must_use]
pub fn gh(config: &PublisherConfig) -> Cmd {
    Cmd::new(&config.gh_bin, "/").trusted()
}

/// The service's GitHub token (`gh auth token`). It can push: never give it to a
/// command that runs a pull request's code.
pub async fn gh_token(
    exec: &dyn Exec,
    config: &PublisherConfig,
    scope: &StepScope,
) -> Result<SecretString, ActivityError> {
    let out = run_ok(
        exec,
        scope,
        gh(config).args(["auth", "token"]).quiet(),
        ActivityError::retryable,
        "gh_not_logged_in",
    )
    .await?;
    let token = out.stdout.trim();
    if token.is_empty() {
        return Err(ActivityError::retryable("gh_not_logged_in"));
    }
    Ok(SecretString::from(token.to_owned()))
}

/// GitHub refuses anonymous git reads of gears-rust, and the build user has no
/// credential helper (the gh login stays with the service). Like actions/checkout,
/// git gets the token as an `http.extraheader` through `GIT_CONFIG_*` variables:
/// in the environment of that one command, never on disk or in its arguments.
/// Not for release-plz, which authenticates with `GIT_TOKEN` itself (two
/// `Authorization` headers are rejected).
#[must_use]
pub fn git_auth(cmd: Cmd, token: &SecretString) -> Cmd {
    git_env(cmd, &[], Some(token))
}

/// Git settings for one command through `GIT_CONFIG_*`: `settings` as given, then
/// the token as in [`git_auth`]. One list: a second `GIT_CONFIG_COUNT` would replace
/// the first.
#[must_use]
pub fn git_env(mut cmd: Cmd, settings: &[(String, String)], token: Option<&SecretString>) -> Cmd {
    use base64::Engine as _;
    use secrecy::ExposeSecret as _;
    let mut count = 0;
    for (key, value) in settings {
        cmd = cmd
            .env(format!("GIT_CONFIG_KEY_{count}"), key)
            .env(format!("GIT_CONFIG_VALUE_{count}"), value);
        count += 1;
    }
    if let Some(token) = token {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{}", token.expose_secret()));
        cmd = cmd
            .env(
                format!("GIT_CONFIG_KEY_{count}"),
                "http.https://github.com/.extraheader",
            )
            .secret(
                format!("GIT_CONFIG_VALUE_{count}"),
                SecretString::from(format!("AUTHORIZATION: basic {basic}")),
            );
        count += 1;
    }
    if count > 0 {
        cmd = cmd.env("GIT_CONFIG_COUNT", count.to_string());
    }
    cmd
}

/// `key` from the secrets file, or from the service's environment.
#[must_use]
pub fn secret(config: &PublisherConfig, key: &str) -> Option<SecretString> {
    if let Some(path) = &config.secrets_file
        && let Ok(text) = std::fs::read_to_string(path)
        && let Some(value) = env_file_value(&text, key)
    {
        return Some(SecretString::from(value));
    }
    std::env::var(key)
        .ok()
        .filter(|t| !t.is_empty())
        .map(SecretString::from)
}

/// `KEY=value` lookup in an env file (`export`, quotes and comments allowed).
#[must_use]
pub fn env_file_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        (!v.is_empty()).then(|| v.to_owned())
    })
}

pub async fn cancellable_sleep(scope: &StepScope, duration: Duration) -> Result<(), ActivityError> {
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        () = scope.cancel.cancelled() => Err(ActivityError::Cancelled),
    }
}

/// The last line of `stdout` that parses as `T` (tools may print other lines).
#[must_use]
pub fn last_json<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    stdout
        .lines()
        .rev()
        .filter(|l| l.trim_start().starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
        .or_else(|| serde_json::from_str(stdout.trim()).ok())
}

/// One heavy build at a time on the host. Release checks and a PR's CI each start
/// `CARGO_BUILD_JOBS` compilers; together they run the host out of memory. The
/// queue is first come, first served.
pub struct BuildSlot {
    permits: Semaphore,
    holder: Mutex<Option<String>>,
}

impl Default for BuildSlot {
    fn default() -> Self {
        Self {
            permits: Semaphore::new(1),
            holder: Mutex::new(None),
        }
    }
}

/// Holds the slot until dropped.
pub struct BuildGuard<'a> {
    slot: &'a BuildSlot,
    _permit: SemaphorePermit<'a>,
}

impl Drop for BuildGuard<'_> {
    fn drop(&mut self) {
        *self.slot.holder.lock() = None;
    }
}

impl BuildSlot {
    /// Waits for the slot; `what` names the holder in other runs' logs.
    pub async fn acquire(
        &self,
        scope: &StepScope,
        what: String,
    ) -> Result<BuildGuard<'_>, ActivityError> {
        let permit = if let Ok(permit) = self.permits.try_acquire() {
            permit
        } else {
            let holder = self
                .holder
                .lock()
                .clone()
                .unwrap_or_else(|| "another run".to_owned());
            scope
                .info(format!("waiting for the build host: {holder} is building"))
                .await;
            let permit = tokio::select! {
                permit = self.permits.acquire() => {
                    permit.map_err(|_| ActivityError::retryable("build_slot_closed"))?
                }
                () = scope.cancel.cancelled() => return Err(ActivityError::Cancelled),
            };
            scope.info("the build host is free").await;
            permit
        };
        *self.holder.lock() = Some(what);
        Ok(BuildGuard {
            slot: self,
            _permit: permit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::journal::MemoryJournal;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    fn scope(journal: &Arc<MemoryJournal>) -> StepScope {
        StepScope {
            run_id: Uuid::nil(),
            step: "s".into(),
            attempt: 1,
            cancel: CancellationToken::new(),
            journal: Arc::clone(journal) as Arc<dyn Journal>,
        }
    }

    #[test]
    fn env_files_are_read() {
        let text = "# c\nexport CARGO_REGISTRY_TOKEN=\"cio_x\"\nOTHER=1\n";
        assert_eq!(
            env_file_value(text, "CARGO_REGISTRY_TOKEN").as_deref(),
            Some("cio_x")
        );
        assert_eq!(env_file_value(text, "MISSING"), None);
    }

    #[tokio::test]
    async fn the_build_slot_is_exclusive_and_names_its_holder() {
        let journal = Arc::new(MemoryJournal::default());
        let slot = Arc::new(BuildSlot::default());
        let first = slot
            .acquire(&scope(&journal), "release checks".into())
            .await;
        assert!(first.is_ok());
        let waiter = {
            let slot = Arc::clone(&slot);
            let scope = scope(&journal);
            tokio::spawn(async move { slot.acquire(&scope, "PR #1 clippy".into()).await.map(drop) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        drop(first);
        waiter.await.unwrap().unwrap();
        assert!(
            journal
                .lines
                .lock()
                .iter()
                .any(|l| l.line == "waiting for the build host: release checks is building")
        );
    }

    #[tokio::test]
    async fn waiting_for_the_build_slot_can_be_cancelled() {
        let journal = Arc::new(MemoryJournal::default());
        let slot = BuildSlot::default();
        let _held = slot.acquire(&scope(&journal), "x".into()).await.unwrap();
        let waiting = scope(&journal);
        waiting.cancel.cancel();
        assert!(matches!(
            slot.acquire(&waiting, "y".into()).await,
            Err(ActivityError::Cancelled)
        ));
    }
}
