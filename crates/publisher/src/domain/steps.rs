//! What every workflow's steps share: the step wrapper, running commands, GitHub
//! credentials and the build slot.
use crate::config::PublisherConfig;
use crate::domain::ports::{Cmd, CmdOutput, Exec, ExecError, Journal, StepScope};
use parking_lot::Mutex;
use secrecy::SecretString;
use serde::Serialize;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
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
/// `CARGO_BUILD_JOBS` compilers; together they run the host out of memory. A
/// waiting release build goes first: a half-published release should not wait
/// behind a PR's tests. Otherwise first come, first served.
#[derive(Default)]
pub struct BuildSlot {
    state: Mutex<SlotState>,
    freed: Notify,
}

#[derive(Default)]
struct SlotState {
    holder: Option<String>,
    /// Release builds waiting for the slot: PR CI does not take it meanwhile.
    releases_waiting: usize,
    /// Waiters in arrival order; the first eligible one takes the slot.
    queue: VecDeque<u64>,
    next_ticket: u64,
}

/// Who asks for the build slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builder {
    Release,
    PrCi,
}

/// Holds the slot until dropped.
pub struct BuildGuard<'a> {
    slot: &'a BuildSlot,
}

impl Drop for BuildGuard<'_> {
    fn drop(&mut self) {
        self.slot.state.lock().holder = None;
        self.slot.freed.notify_waiters();
    }
}

/// Leaves the queue when a waiter stops waiting (cancelled or done).
struct Waiting<'a> {
    slot: &'a BuildSlot,
    ticket: u64,
    builder: Builder,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut state = self.slot.state.lock();
        state.queue.retain(|t| *t != self.ticket);
        if self.builder == Builder::Release {
            state.releases_waiting -= 1;
        }
        drop(state);
        // The next waiter may be eligible now.
        self.slot.freed.notify_waiters();
    }
}

impl SlotState {
    /// Whether `ticket` of `builder` may take the free slot now.
    fn eligible(&self, ticket: u64, builder: Builder) -> bool {
        if self.holder.is_some() {
            return false;
        }
        match builder {
            // The earliest waiting release build.
            Builder::Release => true,
            Builder::PrCi => self.releases_waiting == 0 && self.queue.front() == Some(&ticket),
        }
    }
}

impl BuildSlot {
    /// Takes the free slot for `ticket` if it is its turn.
    fn take(&self, ticket: u64, builder: Builder, what: &str) -> bool {
        let mut state = self.state.lock();
        let eligible = state.eligible(ticket, builder);
        if eligible {
            state.holder = Some(what.to_owned());
        }
        eligible
    }

    /// Waits for the slot; `what` names the holder in other runs' logs.
    pub async fn acquire(
        &self,
        scope: &StepScope,
        what: String,
        builder: Builder,
    ) -> Result<BuildGuard<'_>, ActivityError> {
        let ticket = {
            let mut state = self.state.lock();
            let first = match builder {
                Builder::Release => true,
                Builder::PrCi => state.queue.is_empty() && state.releases_waiting == 0,
            };
            if state.holder.is_none() && first {
                state.holder = Some(what);
                return Ok(BuildGuard { slot: self });
            }
            let ticket = state.next_ticket;
            state.next_ticket += 1;
            state.queue.push_back(ticket);
            if builder == Builder::Release {
                state.releases_waiting += 1;
            }
            ticket
        };
        let waiting = Waiting {
            slot: self,
            ticket,
            builder,
        };
        let holder = self
            .state
            .lock()
            .holder
            .clone()
            .unwrap_or_else(|| "a release build is waiting".to_owned());
        scope
            .info(format!("waiting for the build host: {holder} is building"))
            .await;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if self.take(ticket, builder, &what) {
                drop(waiting);
                scope.info("the build host is free").await;
                return Ok(BuildGuard { slot: self });
            }
            tokio::select! {
                () = &mut freed => {}
                () = scope.cancel.cancelled() => return Err(ActivityError::Cancelled),
            }
        }
    }
}

/// Before a heavy build: when the filesystem of `target` has less than `min_free_gb`
/// left, removes the build's own `target/debug` (the slot is held, so nothing builds
/// there). The next build starts cold instead of failing halfway with a full disk.
pub async fn make_room(exec: &dyn Exec, scope: &StepScope, target: &Path, min_free_gb: u64) {
    let Some(parent) = target.parent() else {
        return;
    };
    let free = |out: Result<CmdOutput, ActivityError>| {
        out.ok()
            .filter(CmdOutput::success)
            .and_then(|o| o.stdout.lines().nth(1)?.trim().parse::<u64>().ok())
    };
    let df = || {
        Cmd::new("df", parent)
            .args(["--output=avail", "-B1", "--"])
            .arg(parent.display().to_string())
            .capture()
    };
    let Some(before) = free(run(exec, scope, df()).await) else {
        return;
    };
    let gib = |bytes: u64| bytes >> 30;
    if gib(before) >= min_free_gb {
        return;
    }
    let debug = target.join("debug");
    scope
        .info(format!(
            "{} GiB free, below {min_free_gb} GiB: removing {} (the next build is cold)",
            gib(before),
            debug.display()
        ))
        .await;
    let removed = run(
        exec,
        scope,
        Cmd::new("rm", parent)
            .args(["-rf", "--"])
            .arg(debug.display().to_string()),
    )
    .await;
    if let (Ok(out), Some(after)) = (&removed, free(run(exec, scope, df()).await))
        && out.success()
    {
        scope.info(format!("{} GiB free", gib(after))).await;
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
            .acquire(&scope(&journal), "release checks".into(), Builder::Release)
            .await;
        assert!(first.is_ok());
        let waiter = {
            let slot = Arc::clone(&slot);
            let scope = scope(&journal);
            tokio::spawn(async move {
                slot.acquire(&scope, "PR #1 clippy".into(), Builder::PrCi)
                    .await
                    .map(drop)
            })
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
        let _held = slot
            .acquire(&scope(&journal), "x".into(), Builder::PrCi)
            .await
            .unwrap();
        let waiting = scope(&journal);
        waiting.cancel.cancel();
        assert!(matches!(
            slot.acquire(&waiting, "y".into(), Builder::Release).await,
            Err(ActivityError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn a_waiting_release_build_goes_before_pr_ci() {
        let journal = Arc::new(MemoryJournal::default());
        let slot = Arc::new(BuildSlot::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = slot
            .acquire(&scope(&journal), "PR #1 test".into(), Builder::PrCi)
            .await
            .unwrap();
        let wait = |what: &'static str, builder: Builder| {
            let (slot, order, scope) = (Arc::clone(&slot), Arc::clone(&order), scope(&journal));
            tokio::spawn(async move {
                let _guard = slot.acquire(&scope, what.into(), builder).await.unwrap();
                order.lock().push(what);
                tokio::time::sleep(Duration::from_millis(10)).await;
            })
        };
        // PR CI asked first, the release later; the release builds first.
        let ci = wait("PR #2 clippy", Builder::PrCi);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let release = wait("release-plz release", Builder::Release);
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        release.await.unwrap();
        ci.await.unwrap();
        assert_eq!(*order.lock(), ["release-plz release", "PR #2 clippy"]);
    }

    #[tokio::test]
    async fn a_cancelled_release_does_not_hold_back_pr_ci() {
        let journal = Arc::new(MemoryJournal::default());
        let slot = Arc::new(BuildSlot::default());
        let held = slot
            .acquire(&scope(&journal), "x".into(), Builder::PrCi)
            .await
            .unwrap();
        let release_scope = scope(&journal);
        let release = {
            let (slot, scope) = (Arc::clone(&slot), release_scope.clone());
            tokio::spawn(async move {
                slot.acquire(&scope, "release".into(), Builder::Release)
                    .await
                    .map(drop)
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        release_scope.cancel.cancel();
        assert!(matches!(
            release.await.unwrap(),
            Err(ActivityError::Cancelled)
        ));
        drop(held);
        let ci = tokio::time::timeout(
            Duration::from_secs(1),
            slot.acquire(&scope(&journal), "PR".into(), Builder::PrCi),
        )
        .await;
        assert!(matches!(ci, Ok(Ok(_))));
    }

    /// `df` reports `avail` bytes; `rm` is recorded.
    struct Disk {
        free: Mutex<Vec<u64>>,
        removed: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Exec for Disk {
        async fn run(&self, _: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
            let stdout = if cmd.program == "df" {
                format!("Avail\n{}\n", self.free.lock().remove(0))
            } else {
                self.removed.lock().push(cmd.args.join(" "));
                String::new()
            };
            Ok(CmdOutput {
                code: Some(0),
                stdout,
                ..CmdOutput::default()
            })
        }
    }

    #[tokio::test]
    async fn a_build_short_of_disk_removes_its_own_debug_artifacts() {
        let journal = Arc::new(MemoryJournal::default());
        let gib = |n: u64| n << 30;
        let disk = Disk {
            free: Mutex::new(vec![gib(200), gib(50), gib(150)]),
            removed: Mutex::new(Vec::new()),
        };
        let target = Path::new("/w/target");
        // Enough room: nothing removed.
        make_room(&disk, &scope(&journal), target, 80).await;
        assert!(disk.removed.lock().is_empty());
        // 50 GiB left: target/debug goes.
        make_room(&disk, &scope(&journal), target, 80).await;
        assert_eq!(*disk.removed.lock(), ["-rf -- /w/target/debug"]);
        let lines = journal.lines.lock();
        assert!(
            lines
                .iter()
                .any(|l| l.line.starts_with("50 GiB free, below 80 GiB"))
        );
        assert!(lines.iter().any(|l| l.line == "150 GiB free"));
    }
}
