//! `gears-release.v1`: the full release of gears-rust as a durable workflow.
//!
//! Every step takes and returns the whole [`ReleaseState`], so later steps see what
//! earlier ones found and the UI can show it. Steps run at least once: a restart or a
//! lost lease starts the current step again, so each one is idempotent. A step that
//! uses the working tree first puts it at the commit the run recorded ([`Tree`]), so
//! nothing depends on what the clone looked like before.
//!
//! An interrupted attempt (service restart, deploy) still counts against the step's
//! `max_attempts`, so steps that can be interrupted get generous limits; real failures
//! are made permanent inside the step where that matters.
//!
//! The contract (step ids, timeouts, retry policies) is stored with the registration;
//! changing it requires a new workflow name (`gears-release.v2`). Intervals that do
//! not belong to the contract come from [`Timings`].
use crate::config::{PublisherConfig, Timings};
use crate::crates_io::{self, Registry};
use crate::runner::{Cmd, CmdOutput, Exec, ExecError, StepScope};
use crate::store::Journal;
use chrono::Utc;
use futures_util::StreamExt;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use toolkit_durable_execution::prelude::*;
use toolkit_durable_execution::{DefinitionError, workflow::Payload};

pub const WORKFLOW: &str = "gears-release.v1";

/// Step ids in execution order.
pub const STEPS: [&str; 9] = [
    "prepare",
    "checks",
    "pre_release",
    "release_pr",
    "wait_merge",
    "sync_main",
    "publish",
    "heal_tags",
    "verify",
];

const RETRY_AFTER_429_WITHOUT_TIME: Duration = Duration::from_mins(10);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReleaseInput {
    /// Prepare and verify everything without creating a PR, publishing or tagging.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Package {
    pub package_name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Release {
    pub package_name: String,
    pub version: String,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PublishOutcome {
    /// Lifetime attempt number of the successful `release-plz release`.
    pub attempt: u32,
    /// `releases` of that attempt (empty in a dry run).
    pub releases: Vec<Release>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct VerifyReport {
    /// Releasable packages checked.
    pub checked: usize,
    /// `name version` of crates whose manifest version is not on crates.io.
    pub missing_on_crates_io: Vec<String>,
    /// Tags `name-vversion` that do not exist.
    pub missing_tags: Vec<String>,
    /// Packages whose index lookup failed.
    pub errors: Vec<String>,
    /// Package name → the highest crates.io version below the one checked: what a
    /// release replaced. Absent for a crate's first version.
    #[serde(default)]
    pub previous: BTreeMap<String, String>,
    /// Packages released as tags only (`git_only` in release-plz.toml): not looked up
    /// on crates.io.
    #[serde(default)]
    pub git_only: Vec<String>,
}

impl VerifyReport {
    #[must_use]
    pub fn clean(&self) -> bool {
        self.missing_on_crates_io.is_empty()
            && self.missing_tags.is_empty()
            && self.errors.is_empty()
    }
}

/// Everything the run has learnt so far. Also stored as the run's facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReleaseState {
    pub dry_run: bool,
    /// `origin/main` the run started from.
    pub main_sha: Option<String>,
    pub pr: Option<PullRequest>,
    /// Packages the release PR bumps.
    pub packages: Vec<Package>,
    /// No package needs a release; the remaining steps do nothing.
    pub nothing_to_release: bool,
    /// Diff of `release-plz update` (dry run).
    pub update_diff: Option<String>,
    /// What the catch-up release on `main` published before the PR (normally nothing:
    /// it finishes an interrupted earlier release, like the push-triggered `release` job).
    pub pre_released: Option<Vec<Release>>,
    pub merge_sha: Option<String>,
    pub publish: Option<PublishOutcome>,
    /// `releases` after tag healing: everything this run released.
    pub releases: Option<Vec<Release>>,
    pub verify: Option<VerifyReport>,
    /// Steps that had nothing to do.
    pub skipped: Vec<String>,
}

impl ReleaseState {
    fn skip(&mut self, step: &str) {
        if !self.skipped.iter().any(|s| s == step) {
            self.skipped.push(step.to_owned());
        }
    }
}

/// What steps need. Shared by all step closures.
pub struct Deps {
    pub config: PublisherConfig,
    pub exec: Arc<dyn Exec>,
    pub journal: Arc<dyn Journal>,
    pub registry: Arc<dyn Registry>,
}

type StepResult = Result<ReleaseState, ActivityError>;

/// Build the workflow definition bound to `deps`.
pub fn definition(
    deps: &Arc<Deps>,
) -> Result<Workflow<ReleaseInput, ReleaseState>, DefinitionError> {
    WorkflowBuilder::<ReleaseInput>::new(WORKFLOW)
        .then(step(
            deps,
            "prepare",
            Duration::from_hours(1),
            RetryPolicy::exponential(10, Duration::from_secs(30), Duration::from_mins(5)),
            prepare,
        ))
        .then(step(
            deps,
            "checks",
            Duration::from_hours(20),
            // Failed checks are permanent; only interruptions (restarts, signals) retry.
            RetryPolicy::exponential(20, Duration::from_secs(30), Duration::from_mins(2)),
            checks,
        ))
        .then(step(
            deps,
            "pre_release",
            Duration::from_hours(12),
            // 429s wait for crates.io's time; other failures are limited by
            // `Timings::publish_attempts` inside the step.
            RetryPolicy::exponential(200, Duration::from_mins(5), Duration::from_mins(10)),
            pre_release,
        ))
        .then(step(
            deps,
            "release_pr",
            Duration::from_hours(2),
            RetryPolicy::exponential(10, Duration::from_mins(1), Duration::from_mins(5)),
            release_pr,
        ))
        .then(step(
            deps,
            "wait_merge",
            Duration::from_hours(2),
            // An attempt polls for up to an hour: 14 days, with slack for `gh` failures.
            RetryPolicy::exponential(14 * 24 * 2, Duration::from_mins(1), Duration::from_mins(1)),
            wait_merge,
        ))
        .then(step(
            deps,
            "sync_main",
            Duration::from_mins(30),
            RetryPolicy::exponential(10, Duration::from_secs(30), Duration::from_mins(5)),
            sync_main,
        ))
        .then(step(
            deps,
            "publish",
            Duration::from_hours(12),
            // 429s wait for crates.io's time; other failures are limited by
            // `Timings::publish_attempts` inside the step.
            RetryPolicy::exponential(200, Duration::from_mins(5), Duration::from_mins(10)),
            publish,
        ))
        .then(step(
            deps,
            "heal_tags",
            Duration::from_hours(1),
            RetryPolicy::exponential(10, Duration::from_mins(1), Duration::from_mins(5)),
            heal_tags,
        ))
        .then(step(
            deps,
            "verify",
            Duration::from_hours(1),
            // The index CDN can lag a few minutes behind a publish.
            RetryPolicy::exponential(10, Duration::from_mins(1), Duration::from_mins(5)),
            verify,
        ))
        .build()
}

/// Wrap a step function: scope, start/finish notes and facts.
fn step<I, F, Fut>(
    deps: &Arc<Deps>,
    id: &'static str,
    timeout: Duration,
    retry: RetryPolicy,
    f: F,
) -> Step<I, ReleaseState>
where
    I: Payload,
    F: Fn(Arc<Deps>, StepScope, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = StepResult> + Send + 'static,
{
    let deps = Arc::clone(deps);
    Step::new(id, timeout, move |ctx: ActivityContext, input: I| {
        let deps = Arc::clone(&deps);
        let scope = StepScope {
            run_id: ctx.run_id().0,
            step: id.to_owned(),
            attempt: ctx.attempt(),
            cancel: ctx.cancellation().clone(),
            journal: Arc::clone(&deps.journal),
        };
        let work = f(Arc::clone(&deps), scope.clone(), input);
        async move {
            scope
                .info(format!("{id}: attempt {} started", scope.attempt))
                .await;
            let result = work.await;
            match &result {
                Ok(state) => {
                    scope.info(format!("{id}: done")).await;
                    if let Ok(facts) = serde_json::to_value(state) {
                        deps.journal.facts(scope.run_id, facts).await;
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

// ---------------------------------------------------------------------------------
// Helpers

fn exec_error(error: &ExecError) -> ActivityError {
    match error {
        ExecError::Cancelled => ActivityError::Cancelled,
        ExecError::Spawn { .. } | ExecError::Io { .. } => ActivityError::retryable("spawn_failed"),
    }
}

impl Deps {
    fn clone_dir(&self) -> PathBuf {
        self.config.clone_dir()
    }

    fn run_dir(&self, scope: &StepScope) -> Result<PathBuf, ActivityError> {
        let dir = self.config.runs_dir().join(scope.run_id.to_string());
        std::fs::create_dir_all(&dir).map_err(|_| ActivityError::retryable("fs_failed"))?;
        Ok(dir)
    }

    /// `<run>/<phase>`: files of one release phase (`pre_release`, `publish`).
    fn phase_dir(&self, scope: &StepScope, phase: &str) -> Result<PathBuf, ActivityError> {
        let dir = self.run_dir(scope)?.join(phase);
        std::fs::create_dir_all(&dir).map_err(|_| ActivityError::retryable("fs_failed"))?;
        Ok(dir)
    }

    fn git(&self) -> Cmd {
        Cmd::new(&self.config.git_bin, self.clone_dir())
    }

    /// Tools that build the workspace share one target directory.
    fn cargo_env(&self, cmd: Cmd) -> Cmd {
        cmd.env("CARGO_TARGET_DIR", &self.config.target_dir)
    }

    /// A command that ends because the run is cancelled or the service stops, or that
    /// was killed by a signal, is an interruption, never a verdict on the release.
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ActivityError> {
        let out = self
            .exec
            .run(scope, cmd)
            .await
            .map_err(|e| exec_error(&e))?;
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
    async fn run_ok(
        &self,
        scope: &StepScope,
        cmd: Cmd,
        fail: fn(&'static str) -> ActivityError,
        code: &'static str,
    ) -> Result<CmdOutput, ActivityError> {
        let out = self.run(scope, cmd).await?;
        if out.success() {
            Ok(out)
        } else {
            Err(fail(code))
        }
    }

    /// `gh` runs as the service (it holds the GitHub login) and outside the clone.
    fn gh(&self) -> Cmd {
        Cmd::new(&self.config.gh_bin, "/").trusted()
    }

    async fn gh_token(&self, scope: &StepScope) -> Result<SecretString, ActivityError> {
        let out = self
            .run_ok(
                scope,
                self.gh().args(["auth", "token"]).quiet(),
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

    /// `CARGO_REGISTRY_TOKEN` from the secrets file, or from the environment.
    fn registry_token(&self) -> Option<SecretString> {
        if let Some(path) = &self.config.secrets_file
            && let Ok(text) = std::fs::read_to_string(path)
            && let Some(token) = env_file_value(&text, "CARGO_REGISTRY_TOKEN")
        {
            return Some(SecretString::from(token));
        }
        std::env::var("CARGO_REGISTRY_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(SecretString::from)
    }

    /// GitHub refuses anonymous git reads of gears-rust, and the build user has no
    /// credential helper (the gh login stays with the service). Like actions/checkout,
    /// git gets the token as an `http.extraheader` through `GIT_CONFIG_*` variables:
    /// in the environment of that one command, never on disk or in its arguments.
    /// Not for release-plz, which authenticates with `GIT_TOKEN` itself (two
    /// `Authorization` headers are rejected).
    async fn with_git_auth(&self, scope: &StepScope, cmd: Cmd) -> Result<Cmd, ActivityError> {
        use base64::Engine as _;
        use secrecy::ExposeSecret as _;
        let token = self.gh_token(scope).await?;
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{}", token.expose_secret()));
        Ok(cmd
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .secret(
                "GIT_CONFIG_VALUE_0",
                SecretString::from(format!("AUTHORIZATION: basic {basic}")),
            ))
    }

    /// `release_retry.py <command>`; it fetches tags, so it gets git credentials.
    async fn release_retry(&self, scope: &StepScope, command: &str) -> Result<Cmd, ActivityError> {
        let cmd = Cmd::new(&self.config.python, self.clone_dir())
            .arg(
                self.clone_dir()
                    .join("tools/scripts/release_retry.py")
                    .display()
                    .to_string(),
            )
            .arg(command);
        self.with_git_auth(scope, cmd).await
    }

    async fn fetch_tags(&self, scope: &StepScope) -> Result<(), ActivityError> {
        let fetch = self
            .git()
            .args(["fetch", "origin", "--tags", "--force", "--prune"]);
        self.run_ok(
            scope,
            self.with_git_auth(scope, fetch).await?,
            ActivityError::retryable,
            "git_fetch_failed",
        )
        .await
        .map(drop)
    }

    /// Remove a path owned by the build user (as that user).
    async fn remove(&self, scope: &StepScope, path: &Path) -> Result<(), ActivityError> {
        let parent = path.parent().unwrap_or(Path::new("/"));
        self.run_ok(
            scope,
            Cmd::new("rm", parent)
                .args(["-rf", "--"])
                .arg(path.display().to_string()),
            ActivityError::retryable,
            "fs_failed",
        )
        .await
        .map(drop)
    }

    /// Put the clone at the commit `tree` names for this run. Cheap when it is there
    /// already; `release-plz update` (dry run) is repeated only after another checkout.
    async fn ensure_tree(
        &self,
        scope: &StepScope,
        state: &ReleaseState,
        tree: Tree,
    ) -> Result<(), ActivityError> {
        let (rev, update) = match tree {
            Tree::Main => (state.main_sha.as_deref(), false),
            Tree::Release => match state.merge_sha.as_deref() {
                Some(merge) => (Some(merge), false),
                None => (state.main_sha.as_deref(), state.dry_run),
            },
        };
        let rev = rev.ok_or_else(|| ActivityError::permanent("commit_unknown"))?;
        let want = format!(
            "{} {rev} {}",
            scope.run_id,
            if update { "updated" } else { "clean" }
        );
        let marker = PathBuf::from(&self.config.work_dir).join("tree");
        let current = std::fs::read_to_string(&marker).unwrap_or_default();
        let head = self
            .run_ok(
                scope,
                self.git().args(["rev-parse", "HEAD"]).quiet(),
                ActivityError::retryable,
                "git_failed",
            )
            .await?;
        if current.trim() == want && head.stdout.trim() == rev && update {
            return Ok(());
        }
        let _ = std::fs::remove_file(&marker);
        self.checkout(scope, rev).await?;
        if update {
            self.run_ok(
                scope,
                self.cargo_env(
                    Cmd::new(&self.config.release_plz_bin, self.clone_dir()).arg("update"),
                ),
                ActivityError::permanent,
                "release_plz_update_failed",
            )
            .await?;
        }
        std::fs::write(&marker, want).map_err(|_| ActivityError::retryable("fs_failed"))?;
        Ok(())
    }

    async fn checkout(&self, scope: &StepScope, rev: &str) -> Result<(), ActivityError> {
        self.run_ok(
            scope,
            self.git().args(["checkout", "--force", "-B", "work", rev]),
            ActivityError::retryable,
            "git_checkout_failed",
        )
        .await?;
        self.run_ok(
            scope,
            self.git().args(["clean", "-fdx", "-e", "target"]),
            ActivityError::retryable,
            "git_clean_failed",
        )
        .await
        .map(drop)
    }

    fn timings(&self) -> &Timings {
        &self.config.timings
    }
}

/// Which commit a step works on.
#[derive(Debug, Clone, Copy)]
enum Tree {
    /// `main` as `prepare` found it.
    Main,
    /// What gets published: the merge commit of the release PR; in a dry run, `main`
    /// after `release-plz update`.
    Release,
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

async fn cancellable_sleep(scope: &StepScope, duration: Duration) -> Result<(), ActivityError> {
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        () = scope.cancel.cancelled() => Err(ActivityError::Cancelled),
    }
}

// ---------------------------------------------------------------------------------
// Steps

/// Fresh clone of `origin/main`.
async fn prepare(deps: Arc<Deps>, scope: StepScope, input: ReleaseInput) -> StepResult {
    let clone = deps.clone_dir();
    if !clone.join(".git").exists() {
        let work_dir = PathBuf::from(&deps.config.work_dir);
        std::fs::create_dir_all(&work_dir).map_err(|_| ActivityError::retryable("fs_failed"))?;
        if clone.exists() {
            // A clone interrupted by a restart.
            deps.remove(&scope, &clone).await?;
        }
        let git_clone = Cmd::new(&deps.config.git_bin, &work_dir).args([
            "clone",
            &deps.config.repo_url,
            &clone.display().to_string(),
        ]);
        deps.run_ok(
            &scope,
            deps.with_git_auth(&scope, git_clone).await?,
            ActivityError::retryable,
            "git_clone_failed",
        )
        .await?;
    }
    // gears-rust installs pre-commit/pre-push hooks from its Makefile; `release-pr`
    // commits and pushes, so they are switched off (GEARS_NO_HOOKS stops the install).
    for (key, value) in [("gears.hooks", "false"), ("core.hooksPath", "/dev/null")] {
        deps.run_ok(
            &scope,
            deps.git().args(["config", "--local", key, value]),
            ActivityError::retryable,
            "git_config_failed",
        )
        .await?;
    }
    deps.fetch_tags(&scope).await?;
    deps.checkout(&scope, "origin/main").await?;
    let _ = std::fs::remove_file(PathBuf::from(&deps.config.work_dir).join("tree"));
    let sha = deps
        .run_ok(
            &scope,
            deps.git().args(["rev-parse", "HEAD"]).capture(),
            ActivityError::retryable,
            "git_failed",
        )
        .await?
        .stdout
        .trim()
        .to_owned();
    scope.info(format!("main is at {sha}")).await;
    Ok(ReleaseState {
        dry_run: input.dry_run,
        main_sha: Some(sha),
        ..ReleaseState::default()
    })
}

/// The configured check commands (gears-rust's CI). A failure stops the run until Retry.
async fn checks(deps: Arc<Deps>, scope: StepScope, state: ReleaseState) -> StepResult {
    deps.ensure_tree(&scope, &state, Tree::Main).await?;
    for check in &deps.config.checks {
        let mut words = check.split_whitespace();
        let Some(program) = words.next() else {
            continue;
        };
        let program = if program == "make" {
            deps.config.make_bin.as_str()
        } else {
            program
        };
        scope.info(format!("check: {check}")).await;
        let mut cmd = deps.cargo_env(Cmd::new(program, deps.clone_dir()).args(words));
        let condensed = deps
            .config
            .condensed_checks
            .iter()
            .any(|c| c.split_whitespace().eq(check.split_whitespace()));
        if condensed {
            cmd = cmd.condensed();
        }
        let out = deps.run(&scope, cmd).await?;
        if !out.success() {
            scope
                .info(format!(
                    "`{check}` failed; fix main and start a new release \
                     (Retry checks the same commit again)"
                ))
                .await;
            return Err(ActivityError::permanent("checks_failed"));
        }
    }
    Ok(state)
}

#[derive(Deserialize)]
struct ReleasePrJson {
    #[serde(default)]
    prs: Vec<ReleasePrEntry>,
}
#[derive(Deserialize)]
struct ReleasePrEntry {
    number: u64,
    html_url: String,
    #[serde(default)]
    releases: Vec<Package>,
}

/// Open or update the release PR; in a dry run, only `release-plz update`.
async fn release_pr(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.dry_run {
        deps.ensure_tree(&scope, &state, Tree::Release).await?;
        let diff = deps
            .run_ok(
                &scope,
                deps.git().args(["diff", "--stat"]).capture(),
                ActivityError::retryable,
                "git_failed",
            )
            .await?
            .stdout;
        state.nothing_to_release = diff.trim().is_empty();
        state.update_diff = Some(diff);
        if state.nothing_to_release {
            scope.info("release-plz update changed nothing").await;
        }
        return Ok(state);
    }

    deps.ensure_tree(&scope, &state, Tree::Main).await?;
    let token = deps.gh_token(&scope).await?;
    let out = deps
        .run_ok(
            &scope,
            deps.cargo_env(
                Cmd::new(&deps.config.release_plz_bin, deps.clone_dir())
                    .args(["release-pr", "-o", "json"])
                    .secret("GIT_TOKEN", token)
                    .capture(),
            ),
            ActivityError::retryable,
            "release_pr_failed",
        )
        .await?;
    let run_dir = deps.run_dir(&scope)?;
    let _ = std::fs::write(run_dir.join("release-pr.json"), &out.stdout);
    let parsed: ReleasePrJson = last_json(&out.stdout)
        .ok_or_else(|| ActivityError::permanent("release_pr_output_invalid"))?;
    match parsed.prs.into_iter().next() {
        None => {
            state.nothing_to_release = true;
            scope.info("nothing to release").await;
        }
        Some(pr) => {
            scope
                .info(format!(
                    "release PR #{} ({} packages): {}",
                    pr.number,
                    pr.releases.len(),
                    pr.html_url
                ))
                .await;
            state.nothing_to_release = pr.releases.is_empty();
            state.packages = pr.releases;
            state.pr = Some(PullRequest {
                number: pr.number,
                url: pr.html_url,
            });
        }
    }
    Ok(state)
}

#[derive(Deserialize)]
struct PrView {
    state: String,
    #[serde(rename = "mergeCommit")]
    merge_commit: Option<MergeCommit>,
}
#[derive(Deserialize)]
struct MergeCommit {
    oid: String,
}

/// Poll the release PR until it is merged.
async fn wait_merge(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.dry_run || state.nothing_to_release {
        state.skip("wait_merge");
        return Ok(state);
    }
    let Some(pr) = state.pr.clone() else {
        return Err(ActivityError::permanent("no_release_pr"));
    };
    let poll = Duration::from_secs(deps.timings().merge_poll_secs.max(1));
    let window = Duration::from_secs(deps.timings().merge_attempt_window_secs);
    let started = tokio::time::Instant::now();
    scope
        .info(format!("waiting for {} to be merged", pr.url))
        .await;
    loop {
        let out = deps
            .run_ok(
                &scope,
                deps.gh()
                    .args([
                        "pr",
                        "view",
                        &pr.number.to_string(),
                        "--repo",
                        &deps.config.github_repository,
                        "--json",
                        "state,mergeCommit",
                    ])
                    .capture(),
                ActivityError::retryable,
                "gh_failed",
            )
            .await?;
        let view: PrView = serde_json::from_str(out.stdout.trim())
            .map_err(|_| ActivityError::retryable("gh_output_invalid"))?;
        match view.state.as_str() {
            "MERGED" => {
                let sha = view
                    .merge_commit
                    .map(|c| c.oid)
                    .ok_or_else(|| ActivityError::retryable("merge_commit_unknown"))?;
                scope.info(format!("merged as {sha}")).await;
                state.merge_sha = Some(sha);
                return Ok(state);
            }
            "CLOSED" => {
                scope
                    .info("the release PR was closed without merging")
                    .await;
                return Err(ActivityError::permanent("pr_closed"));
            }
            _ => {}
        }
        if started.elapsed() + poll > window {
            return Err(ActivityError::retryable_after("pr_open", poll));
        }
        cancellable_sleep(&scope, poll).await?;
    }
}

/// Check out the merge commit and record the tags that exist before publishing.
async fn sync_main(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.nothing_to_release {
        state.skip("sync_main");
        return Ok(state);
    }
    if state.merge_sha.is_some() {
        deps.fetch_tags(&scope).await?;
    }
    deps.ensure_tree(&scope, &state, Tree::Release).await?;
    snapshot(&deps, &scope, &deps.phase_dir(&scope, "publish")?).await?;
    Ok(state)
}

const SEEN_DRY_SKIP: &str = "due to dry, skipping";
const SEEN_UNPUBLISHED_DEP: &str = "failed to select a version for the requirement";
const SEEN_429: &str = "429 Too Many Requests";

#[derive(Deserialize)]
struct ReleaseJson {
    #[serde(default)]
    releases: Vec<ReleaseJsonEntry>,
}
#[derive(Deserialize)]
struct ReleaseJsonEntry {
    package_name: String,
    version: String,
    tag: String,
}

fn outcome_path(dir: &Path, attempt: u32) -> PathBuf {
    dir.join(format!("release-{attempt}.outcome"))
}

/// Non-429 failures recorded by earlier attempts.
fn failures(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".outcome"))
        .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|s| s.trim() == "failed"))
        .count()
}

/// GitHub's `release` job on the current `main`, before the release PR: publish
/// whatever the manifests have and crates.io lacks, then `collect`. Normally a no-op;
/// it completes a release that an earlier run or a manual attempt left unfinished, so
/// that `release-pr` never computes versions while a publish is half done.
async fn pre_release(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.dry_run {
        state.skip("pre_release");
        return Ok(state);
    }
    deps.ensure_tree(&scope, &state, Tree::Main).await?;
    let dir = deps.phase_dir(&scope, "pre_release")?;
    snapshot(&deps, &scope, &dir).await?;
    let outcome = release_attempt(&deps, &scope, &dir, false).await?;
    let released = collect(&deps, &scope, &dir, &outcome).await?;
    if !released.is_empty() {
        scope
            .info(format!(
                "finished an earlier release: {} packages",
                released.len()
            ))
            .await;
    }
    state.pre_released = Some(released);
    Ok(state)
}

/// `release-plz release` of the merged release PR, retried through crates.io rate limits.
async fn publish(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.nothing_to_release {
        state.skip("publish");
        return Ok(state);
    }
    deps.ensure_tree(&scope, &state, Tree::Release).await?;
    let dir = deps.phase_dir(&scope, "publish")?;
    state.publish = Some(release_attempt(&deps, &scope, &dir, state.dry_run).await?);
    Ok(state)
}

/// `release_retry.py snapshot` into `<dir>/tags-before.txt`, the baseline of `collect`.
/// The first snapshot counts; a re-run after a restart must not move it.
async fn snapshot(deps: &Deps, scope: &StepScope, dir: &Path) -> Result<(), ActivityError> {
    let tags_before = dir.join("tags-before.txt");
    if tags_before.exists() {
        scope.info("tag snapshot already taken").await;
        return Ok(());
    }
    deps.run_ok(
        scope,
        deps.release_retry(scope, "snapshot")
            .await?
            .env("TAGS_BEFORE", tags_before.display().to_string()),
        ActivityError::retryable,
        "snapshot_failed",
    )
    .await
    .map(drop)
}

/// One `release-plz release` attempt, as in `release-plz.yml`: before a retry
/// `release_retry.py wait` (GitHub API budget, tags); `CARGO_INCREMENTAL=0`, the sparse
/// protocol and a cleared cargo registry. Outputs go to `<dir>/release-N.{json,outcome}`.
async fn release_attempt(
    deps: &Deps,
    scope: &StepScope,
    dir: &Path,
    dry_run: bool,
) -> Result<PublishOutcome, ActivityError> {
    check_publish_budget(deps, scope, dir).await?;
    let attempt = scope.attempt;
    let mut cmd = Cmd::new(&deps.config.release_plz_bin, deps.clone_dir())
        .args(["release", "-o", "json"])
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_REGISTRIES_CRATES_IO_PROTOCOL", "sparse")
        .capture()
        .watch(&[SEEN_DRY_SKIP, SEEN_UNPUBLISHED_DEP, SEEN_429]);
    if dry_run {
        cmd = cmd.arg("--dry-run");
    } else {
        let Some(registry_token) = deps.registry_token() else {
            scope
                .info("CARGO_REGISTRY_TOKEN is not configured (secrets_file)")
                .await;
            return Err(ActivityError::permanent("registry_token_missing"));
        };
        let gh = deps.gh_token(scope).await?;
        report_api_budget(deps, scope).await;
        if attempt > 1 {
            // release-plz checks for an existing tag with a local `git tag -l`.
            deps.run_ok(
                scope,
                deps.release_retry(scope, "wait")
                    .await?
                    .args(["--min-delay", "0", "--cap", "3600"])
                    .secret("API_TOKEN", gh.clone()),
                ActivityError::retryable,
                "retry_wait_failed",
            )
            .await?;
        }
        clean_cargo_registry(deps, scope, dir).await;
        cmd = cmd
            .secret("CARGO_REGISTRY_TOKEN", registry_token)
            .secret("GIT_TOKEN", gh);
    }
    let out = deps.run(scope, deps.cargo_env(cmd)).await?;
    let _ = std::fs::write(dir.join(format!("release-{attempt}.json")), &out.stdout);
    let record = |outcome: &str| {
        let _ = std::fs::write(outcome_path(dir, attempt), outcome);
    };

    if out.success() {
        record("success");
        let releases: Vec<Release> = last_json::<ReleaseJson>(&out.stdout)
            .map(|r| {
                r.releases
                    .into_iter()
                    .map(|e| Release {
                        package_name: e.package_name,
                        version: e.version,
                        tag: e.tag,
                    })
                    .collect()
            })
            .unwrap_or_default();
        scope
            .info(format!("release-plz released {} packages", releases.len()))
            .await;
        return Ok(PublishOutcome {
            attempt,
            releases,
            note: None,
        });
    }

    if dry_run {
        if out.saw(SEEN_DRY_SKIP) && out.saw(SEEN_UNPUBLISHED_DEP) {
            let note = "dry run stopped at the first crate that needs an unpublished \
                        dependency; the crates before it passed verification";
            scope.info(note).await;
            record("success");
            return Ok(PublishOutcome {
                attempt,
                releases: Vec::new(),
                note: Some(note.to_owned()),
            });
        }
        record("failed");
        return Err(ActivityError::permanent("publish_dry_run_failed"));
    }

    if out.saw(SEEN_429) || crates_io::is_rate_limited(&out.tail_text()) {
        record("rate_limited");
        return Err(rate_limited(deps, scope, &out).await);
    }
    record("failed");
    Err(ActivityError::retryable("publish_failed"))
}

/// Log the GitHub API budget of the token in use (`GET /rate_limit` is free).
async fn report_api_budget(deps: &Deps, scope: &StepScope) {
    let _ = deps
        .run(
            scope,
            deps.gh().args([
                "api",
                "rate_limit",
                "--jq",
                r#""GitHub API budget: \(.resources.core.remaining)/\(.resources.core.limit), resets \(.resources.core.reset | todate)""#,
            ]),
        )
        .await;
}

/// `release-plz.yml` removes `~/.cargo/registry` and `~/.cargo/git` before publishing:
/// clearing the index works around release-plz's gix slotmap overflow. Once per phase.
async fn clean_cargo_registry(deps: &Deps, scope: &StepScope, dir: &Path) {
    let Some(cargo_home) = deps
        .config
        .clean_cargo_home
        .as_deref()
        .filter(|h| Path::new(h).is_absolute())
    else {
        return;
    };
    let marker = dir.join("registry-cleaned");
    if marker.exists() {
        return;
    }
    for sub in ["registry", "git"] {
        let path = Path::new(cargo_home).join(sub);
        if deps.remove(scope, &path).await.is_ok() {
            scope.info(format!("removed {}", path.display())).await;
        }
    }
    let _ = std::fs::write(marker, "");
}

/// Fail for good once `Timings::publish_attempts` attempts failed for reasons other
/// than rate limits. A manual Retry then starts a fresh budget.
async fn check_publish_budget(
    deps: &Deps,
    scope: &StepScope,
    dir: &Path,
) -> Result<(), ActivityError> {
    let limit = deps.timings().publish_attempts as usize;
    if failures(dir) < limit {
        return Ok(());
    }
    scope
        .info(format!(
            "{limit} publish attempts failed; fix the cause, then press Retry"
        ))
        .await;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().ends_with(".outcome") {
            let _ = std::fs::rename(entry.path(), entry.path().with_extension("outcome-old"));
        }
    }
    Err(ActivityError::permanent("publish_failed"))
}

/// Wait until crates.io's `try again after`, or a fixed time when it named none.
async fn rate_limited(deps: &Deps, scope: &StepScope, out: &CmdOutput) -> ActivityError {
    let wait = crates_io::rate_limited_until(&out.tail_text()).map_or(
        RETRY_AFTER_429_WITHOUT_TIME,
        |until| {
            crates_io::wait_for(
                until,
                Utc::now(),
                Duration::from_secs(deps.timings().rate_limit_margin_secs),
            )
        },
    );
    scope
        .info(format!(
            "crates.io rate limit; next attempt in {}s",
            wait.as_secs()
        ))
        .await;
    ActivityError::retryable_after("crates_io_429", wait)
}

/// After the merged PR is published: `collect` heals tags and lists everything released.
async fn heal_tags(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.dry_run || state.nothing_to_release {
        state.skip("heal_tags");
        return Ok(state);
    }
    let Some(publish) = state.publish.clone() else {
        return Err(ActivityError::permanent("publish_outcome_missing"));
    };
    deps.ensure_tree(&scope, &state, Tree::Release).await?;
    let dir = deps.phase_dir(&scope, "publish")?;
    state.releases = Some(collect(&deps, &scope, &dir, &publish).await?);
    Ok(state)
}

/// `release_retry.py collect`: tag published crates that have no tag, and compute the
/// full list of releases, including those of failed attempts (from tags after the snapshot).
async fn collect(
    deps: &Deps,
    scope: &StepScope,
    dir: &Path,
    publish: &PublishOutcome,
) -> Result<Vec<Release>, ActivityError> {
    let output = dir.join("collect-output.txt");
    let _ = std::fs::remove_file(&output);
    let releases = serde_json::to_string(&publish.releases)
        .map_err(|_| ActivityError::permanent("encode_failed"))?;
    let mut cmd = deps
        .release_retry(scope, "collect")
        .await?
        .env(
            "TAGS_BEFORE",
            dir.join("tags-before.txt").display().to_string(),
        )
        .env("GITHUB_REPOSITORY", &deps.config.github_repository)
        .env("GITHUB_OUTPUT", output.display().to_string())
        .secret("GH_TOKEN", deps.gh_token(scope).await?);
    // collect knows three attempts. A first-attempt success is used as is; otherwise
    // "failure, then success" makes it recover earlier attempts' releases from tags.
    cmd = if publish.attempt <= 1 {
        cmd.env("OUTCOME_1", "success").env("RELEASES_1", releases)
    } else {
        cmd.env("OUTCOME_1", "failure")
            .env("OUTCOME_2", "success")
            .env("RELEASES_2", releases)
    };
    deps.run_ok(scope, cmd, ActivityError::retryable, "collect_failed")
        .await?;
    let text = std::fs::read_to_string(&output).unwrap_or_default();
    let collected = text
        .lines()
        .find_map(|l| l.strip_prefix("releases="))
        .and_then(|json| serde_json::from_str::<Vec<Release>>(json).ok())
        .ok_or_else(|| ActivityError::retryable("collect_output_invalid"))?;
    scope
        .info(format!("collect: {} packages released", collected.len()))
        .await;
    Ok(collected)
}

/// Compare manifest versions with crates.io and tags.
async fn verify(deps: Arc<Deps>, scope: StepScope, mut state: ReleaseState) -> StepResult {
    if state.nothing_to_release {
        state.skip("verify");
        return Ok(state);
    }
    if !state.dry_run {
        deps.fetch_tags(&scope).await?;
    }
    deps.ensure_tree(&scope, &state, Tree::Release).await?;
    let metadata = deps
        .run_ok(
            &scope,
            Cmd::new(&deps.config.cargo_bin, deps.clone_dir())
                .args(["metadata", "--no-deps", "--format-version", "1"])
                .quiet(),
            ActivityError::retryable,
            "cargo_metadata_failed",
        )
        .await?
        .stdout;
    let git_only = std::fs::read_to_string(deps.clone_dir().join("release-plz.toml"))
        .ok()
        .map(|text| git_only_packages(&text))
        .unwrap_or_default();
    let packages = releasable(&metadata, &git_only)
        .map_err(|_| ActivityError::permanent("cargo_metadata_invalid"))?;
    let tags: BTreeSet<String> = deps
        .run_ok(
            &scope,
            deps.git().args(["tag", "-l"]).quiet(),
            ActivityError::retryable,
            "git_failed",
        )
        .await?
        .stdout
        .lines()
        .map(str::to_owned)
        .collect();

    let lookups = lookup_versions(
        &deps.registry,
        packages.iter().filter(|p| p.crates_io).cloned().collect(),
    )
    .await;

    let mut report = VerifyReport {
        checked: packages.len(),
        git_only: packages
            .iter()
            .filter(|p| !p.crates_io)
            .map(|p| p.name.clone())
            .collect(),
        ..VerifyReport::default()
    };
    for (versions, p) in lookups {
        if let Ok(v) = &versions
            && let Some(previous) = previous_version(v, &p.version)
        {
            report.previous.insert(p.name.clone(), previous);
        }
        match versions {
            Ok(v) if v.contains(&p.version) => {}
            Ok(_) => report
                .missing_on_crates_io
                .push(format!("{} {}", p.name, p.version)),
            Err(error) => report
                .errors
                .push(format!("{} {}: {error}", p.name, p.version)),
        }
    }
    for p in &packages {
        let tag = format!("{}-v{}", p.name, p.version);
        if !tags.contains(&tag) {
            report.missing_tags.push(tag);
        }
    }
    report.missing_on_crates_io.sort();
    report.missing_tags.sort();
    report.errors.sort();

    log_report(&scope, &report).await;
    let clean = report.clean();
    state.verify = Some(report);
    if clean || state.dry_run {
        // A dry run publishes nothing: its differences are what a release would publish.
        return Ok(state);
    }
    deps.journal
        .facts(
            scope.run_id,
            serde_json::to_value(&state).unwrap_or_default(),
        )
        .await;
    Err(ActivityError::retryable("verify_mismatch"))
}

/// The highest of `versions` below `current` (semver order; unparsable ones skipped).
fn previous_version(versions: &[String], current: &str) -> Option<String> {
    let current = semver::Version::parse(current).ok()?;
    versions
        .iter()
        .filter_map(|v| semver::Version::parse(v).ok())
        .filter(|v| *v < current)
        .max()
        .map(|v| v.to_string())
}

async fn lookup_versions(
    registry: &Arc<dyn Registry>,
    packages: Vec<Releasable>,
) -> Vec<(anyhow::Result<Vec<String>>, Releasable)> {
    let jobs = packages.into_iter().map(|p| {
        let registry = Arc::clone(registry);
        async move { (registry.versions(&p.name).await, p) }
    });
    futures_util::stream::iter(jobs)
        .buffer_unordered(16)
        .collect()
        .await
}

async fn log_report(scope: &StepScope, report: &VerifyReport) {
    scope
        .info(format!(
            "checked {}: {} not on crates.io, {} without tag, {} lookup errors",
            report.checked,
            report.missing_on_crates_io.len(),
            report.missing_tags.len(),
            report.errors.len()
        ))
        .await;
    for line in report
        .missing_on_crates_io
        .iter()
        .map(|p| format!("not on crates.io: {p}"))
        .chain(
            report
                .missing_tags
                .iter()
                .map(|t| format!("missing tag: {t}")),
        )
        .chain(report.errors.iter().map(|e| format!("lookup error: {e}")))
    {
        scope.info(line).await;
    }
}

/// A workspace package that a release publishes or tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Releasable {
    pub name: String,
    pub version: String,
    /// Published to crates.io (otherwise only tagged).
    pub crates_io: bool,
}

/// Same rule as `release_retry.py releasable_packages`.
pub fn releasable(
    metadata: &str,
    git_only: &BTreeSet<String>,
) -> Result<Vec<Releasable>, serde_json::Error> {
    let metadata: serde_json::Value = serde_json::from_str(metadata)?;
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let id = package["id"].as_str().unwrap_or_default();
        if !members.is_empty() && !members.contains(id) {
            continue;
        }
        let (Some(name), Some(version)) = (package["name"].as_str(), package["version"].as_str())
        else {
            continue;
        };
        let to_crates_io = package["publish"]
            .as_array()
            .is_none_or(|registries| !registries.is_empty());
        if !to_crates_io && !git_only.contains(name) {
            continue;
        }
        out.push(Releasable {
            name: name.to_owned(),
            version: version.to_owned(),
            crates_io: to_crates_io && !git_only.contains(name),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Package names marked `git_only` in release-plz.toml.
#[must_use]
pub fn git_only_packages(release_plz_toml: &str) -> BTreeSet<String> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(default)]
        package: Vec<PackageConfig>,
    }
    #[derive(Deserialize)]
    struct PackageConfig {
        name: String,
        #[serde(default)]
        git_only: bool,
    }
    toml::from_str::<Config>(release_plz_toml)
        .map(|c| {
            c.package
                .into_iter()
                .filter(|p| p.git_only)
                .map(|p| p.name)
                .collect()
        })
        .unwrap_or_default()
}

/// The last line of `stdout` that parses as `T` (release-plz may print other lines).
fn last_json<T: serde::de::DeserializeOwned>(stdout: &str) -> Option<T> {
    stdout
        .lines()
        .rev()
        .filter(|l| l.trim_start().starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
        .or_else(|| serde_json::from_str(stdout.trim()).ok())
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod tests;
