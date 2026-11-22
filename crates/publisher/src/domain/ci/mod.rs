//! `gears-pr-ci.v1`: a pull request's CI as gears-rust's GitHub workflows run it.
//!
//! Like GitHub, it tests `refs/pull/<n>/merge` (the PR merged into its base), detects
//! changes with the workflows' own path filters and runs every job whose `if:` they
//! satisfy. Each job is one durable step; its GitHub steps run in order and the first
//! failing one ends the job. A failed job does not stop the others: `conclusion`
//! fails the run once all of them have had their turn.
//!
//! The PR's code is untrusted. It runs as its own build user (`ci_run_as`) in its own
//! clone and target directory, and never sees the service's GitHub token: the
//! service fetches the PR into a bare mirror it owns, and the CI clone fetches from
//! that mirror. Checks get only the optional read-only `GITHUB_CI_TOKEN`.
pub mod changes;
pub mod jobs;

use crate::config::PublisherConfig;
use crate::domain::ports::{Cmd, CmdOutput, Exec, Journal, StepScope};
use crate::domain::steps::{self, BuildSlot, Builder, git_auth, git_env};
use changes::{Changes, Rules};
use jobs::{JOBS, Job};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};
use toolkit_durable_execution::DefinitionError;
use toolkit_durable_execution::prelude::*;

pub const WORKFLOW: &str = "gears-pr-ci.v1";

/// Step ids in execution order: `changes`, the jobs of [`JOBS`], `conclusion`.
pub const STEPS: [&str; 9] = [
    "changes",
    "fmt",
    "repo_checks",
    "security",
    "clippy",
    "lint",
    "test",
    "integration",
    "conclusion",
];

/// Changed files kept in the facts; the count covers all of them.
const FILES_KEPT: usize = 200;
/// GitHub computes merge commits in the background after a push.
const MERGE_REF_WAIT: Duration = Duration::from_secs(30);
/// The read-only token checks may use for git dependencies on GitHub.
pub const CI_TOKEN: &str = "GITHUB_CI_TOKEN";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiInput {
    pub pr: u64,
    /// Run only these jobs (GitHub's "Re-run failed jobs"); `None`: every job.
    #[serde(default)]
    pub jobs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    /// From a fork.
    pub cross_repository: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub id: String,
    pub conclusion: Conclusion,
    /// The GitHub step that failed.
    #[serde(default)]
    pub failed_step: Option<String>,
    /// Why the job was skipped.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Everything the run has learnt so far. Also stored as the run's facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiState {
    pub pr_number: u64,
    pub pr: Option<PullRequest>,
    /// Jobs selected for a re-run.
    pub only: Option<Vec<String>>,
    /// `refs/pull/<n>/merge`: what the jobs test.
    pub merge_sha: Option<String>,
    /// The base branch commit it merges into.
    pub base_sha: Option<String>,
    pub changed_files: usize,
    /// The first changed files (up to 200).
    pub files: Vec<String>,
    pub changes: Option<Changes>,
    /// Where the path rules came from.
    pub rules: Option<String>,
    pub jobs: Vec<JobResult>,
    /// Steps that had nothing to do (skipped jobs).
    pub skipped: Vec<String>,
}

impl CiState {
    fn record(&mut self, result: JobResult) {
        if result.conclusion == Conclusion::Skipped && !self.skipped.contains(&result.id) {
            self.skipped.push(result.id.clone());
        }
        self.jobs.retain(|j| j.id != result.id);
        self.jobs.push(result);
    }

    fn failed(&self) -> Vec<&JobResult> {
        self.jobs
            .iter()
            .filter(|j| j.conclusion == Conclusion::Failure)
            .collect()
    }
}

/// What the steps need.
pub struct CiDeps {
    pub config: PublisherConfig,
    /// Untrusted commands run as `ci_run_as`; `trusted` ones (gh, the mirror) as the service.
    pub exec: Arc<dyn Exec>,
    pub journal: Arc<dyn Journal>,
    /// Shared with the release: one heavy build at a time.
    pub slot: Arc<BuildSlot>,
    /// One run at a time in the CI clone.
    pub clone: Mutex<()>,
}

type StepResult = Result<CiState, ActivityError>;

/// Build the workflow definition bound to `deps`.
pub fn definition(deps: &Arc<CiDeps>) -> Result<Workflow<CiInput, CiState>, DefinitionError> {
    let mut workflow = WorkflowBuilder::<CiInput>::new(WORKFLOW).then(step(
        deps,
        "changes",
        Duration::from_hours(1),
        RetryPolicy::exponential(10, Duration::from_secs(30), Duration::from_mins(5)),
        detect_changes,
    ));
    for job in &JOBS {
        workflow = workflow.then(step(
            deps,
            job.id,
            // Includes the wait for the build slot, which release checks can hold for hours.
            Duration::from_hours(20),
            // A failing check fails the job, not the step: only interruptions retry.
            RetryPolicy::exponential(20, Duration::from_secs(30), Duration::from_mins(2)),
            move |deps, scope, state| run_job(deps, scope, state, job),
        ));
    }
    workflow
        .then(step(
            deps,
            "conclusion",
            Duration::from_mins(5),
            RetryPolicy::exponential(3, Duration::from_secs(5), Duration::from_secs(30)),
            conclusion,
        ))
        .build()
}

fn step<I, F, Fut>(
    deps: &Arc<CiDeps>,
    id: &'static str,
    timeout: Duration,
    retry: RetryPolicy,
    f: F,
) -> Step<I, CiState>
where
    I: toolkit_durable_execution::workflow::Payload,
    F: Fn(Arc<CiDeps>, StepScope, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = StepResult> + Send + 'static,
{
    steps::step(deps, &deps.journal, id, timeout, retry, f)
}

impl CiDeps {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ActivityError> {
        steps::run(self.exec.as_ref(), scope, cmd).await
    }

    async fn run_ok(
        &self,
        scope: &StepScope,
        cmd: Cmd,
        fail: fn(&'static str) -> ActivityError,
        code: &'static str,
    ) -> Result<CmdOutput, ActivityError> {
        steps::run_ok(self.exec.as_ref(), scope, cmd, fail, code).await
    }

    /// git on the service's mirror, as the service.
    fn mirror_git(&self) -> Cmd {
        Cmd::new(&self.config.git_bin, self.config.ci_mirror_dir()).trusted()
    }

    /// git in the CI clone, as the CI user. The mirror belongs to the service, so git
    /// is told to trust it (`safe.directory` must come from protected configuration).
    fn clone_git(&self) -> Cmd {
        Cmd::new(&self.config.git_bin, self.config.ci_clone_dir())
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "safe.directory")
            .env(
                "GIT_CONFIG_VALUE_0",
                self.config.ci_mirror_dir().display().to_string(),
            )
    }

    async fn lock_clone(&self, scope: &StepScope) -> Result<MutexGuard<'_, ()>, ActivityError> {
        if let Ok(guard) = self.clone.try_lock() {
            return Ok(guard);
        }
        scope
            .info("waiting for another PR's CI to finish with the CI clone")
            .await;
        tokio::select! {
            guard = self.clone.lock() => Ok(guard),
            () = scope.cancel.cancelled() => Err(ActivityError::Cancelled),
        }
    }

    /// A fresh tree at `rev`, as every GitHub job starts with its own checkout.
    async fn checkout(&self, scope: &StepScope, rev: &str) -> Result<(), ActivityError> {
        self.run_ok(
            scope,
            self.clone_git()
                .args(["checkout", "--force", "--detach", rev]),
            ActivityError::retryable,
            "git_checkout_failed",
        )
        .await?;
        self.run_ok(
            scope,
            self.clone_git().args(["clean", "-ffdx"]),
            ActivityError::retryable,
            "git_clean_failed",
        )
        .await
        .map(drop)
    }

    async fn capture(&self, scope: &StepScope, cmd: Cmd) -> Result<String, ActivityError> {
        Ok(self
            .run_ok(scope, cmd.capture(), ActivityError::retryable, "git_failed")
            .await?
            .stdout
            .trim()
            .to_owned())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrView {
    number: u64,
    title: String,
    url: String,
    state: String,
    #[serde(default)]
    mergeable: String,
    author: Author,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    #[serde(default)]
    is_cross_repository: bool,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

/// GitHub's `changes` job and checkout: the PR, its merge commit, the CI clone at
/// that commit and the change filters.
async fn detect_changes(deps: Arc<CiDeps>, scope: StepScope, input: CiInput) -> StepResult {
    let number = input.pr;
    let mut state = CiState {
        pr_number: number,
        only: input.jobs.clone(),
        ..CiState::default()
    };
    let _clone = deps.lock_clone(&scope).await?;
    let pr = pull_request(&deps, &scope, number).await?;
    scope
        .info(format!(
            "PR #{number} \"{}\" by {}: {} ({}) into {}",
            pr.title, pr.author, pr.head_ref, pr.head_sha, pr.base_ref
        ))
        .await;
    let merge = fetch_into_mirror(&deps, &scope, &pr).await?;
    let base = deps
        .capture(
            &scope,
            deps.mirror_git().args(["rev-parse", &format!("{merge}^1")]),
        )
        .await?;
    scope.info(format!("merge commit is at {merge}")).await;

    update_clone(&deps, &scope, &pr).await?;

    let files: Vec<String> = deps
        .capture(
            &scope,
            deps.mirror_git().args([
                "-c",
                "core.quotePath=false",
                "diff",
                "--name-only",
                "--no-renames",
                &base,
                &merge,
            ]),
        )
        .await?
        .lines()
        .map(str::to_owned)
        .collect();
    let rules = workflow_rules(&deps, &scope, &merge).await;
    let changes = rules.evaluate(&files);
    let filters: Vec<String> = changes
        .filters
        .iter()
        .map(|(name, hit)| format!("{name}={hit}"))
        .collect();
    scope
        .info(format!(
            "{} changed files; filters ({}): {}",
            files.len(),
            rules.source,
            filters.join(" ")
        ))
        .await;
    state.changed_files = files.len();
    state.files = files.into_iter().take(FILES_KEPT).collect();
    state.changes = Some(changes);
    state.rules = Some(rules.source);
    state.merge_sha = Some(merge);
    state.base_sha = Some(base);
    state.pr = Some(pr);
    Ok(state)
}

async fn pull_request(
    deps: &CiDeps,
    scope: &StepScope,
    number: u64,
) -> Result<PullRequest, ActivityError> {
    let out = deps
        .run_ok(
            scope,
            steps::gh(&deps.config)
                .args([
                    "pr",
                    "view",
                    &number.to_string(),
                    "--repo",
                    &deps.config.github_repository,
                    "--json",
                    "number,title,url,state,mergeable,author,headRefName,headRefOid,\
                     baseRefName,isCrossRepository",
                ])
                .capture(),
            ActivityError::retryable,
            "gh_failed",
        )
        .await?;
    let view: PrView = serde_json::from_str(out.stdout.trim())
        .map_err(|_| ActivityError::retryable("gh_output_invalid"))?;
    if view.state != "OPEN" {
        scope
            .info(format!(
                "PR #{number} is {}; CI runs on open PRs",
                view.state.to_lowercase()
            ))
            .await;
        return Err(ActivityError::permanent("pr_not_open"));
    }
    if view.mergeable == "CONFLICTING" {
        scope
            .info(format!(
                "PR #{number} has conflicts with {}: GitHub has no merge commit to test. \
                 Resolve them, then press Retry",
                view.base_ref_name
            ))
            .await;
        return Err(ActivityError::permanent("merge_conflict"));
    }
    Ok(PullRequest {
        number: view.number,
        title: view.title,
        url: view.url,
        author: view.author.login,
        head_ref: view.head_ref_name,
        head_sha: view.head_ref_oid,
        base_ref: view.base_ref_name,
        cross_repository: view.is_cross_repository,
    })
}

/// Fetches the base branch, the PR's head and its merge commit into the service's
/// bare mirror, with the service's token. Returns the merge commit.
async fn fetch_into_mirror(
    deps: &CiDeps,
    scope: &StepScope,
    pr: &PullRequest,
) -> Result<String, ActivityError> {
    let mirror = deps.config.ci_mirror_dir();
    if !mirror.join("HEAD").exists() {
        let parent = mirror.parent().unwrap_or(Path::new("/"));
        std::fs::create_dir_all(parent).map_err(|_| ActivityError::retryable("fs_failed"))?;
        deps.run_ok(
            scope,
            Cmd::new(&deps.config.git_bin, parent)
                .args(["init", "--bare", "--quiet"])
                .arg(mirror.display().to_string())
                .trusted(),
            ActivityError::retryable,
            "git_init_failed",
        )
        .await?;
    }
    let token = steps::gh_token(deps.exec.as_ref(), &deps.config, scope).await?;
    let n = pr.number;
    let fetch = |refspecs: Vec<String>| {
        git_auth(
            deps.mirror_git()
                .args(["fetch", "--force", "--no-tags", &deps.config.repo_url])
                .args(refspecs),
            &token,
        )
    };
    deps.run_ok(
        scope,
        fetch(vec![
            format!("+refs/heads/{0}:refs/heads/{0}", pr.base_ref),
            format!("+refs/pull/{n}/head:refs/pull/{n}/head"),
        ]),
        ActivityError::retryable,
        "git_fetch_failed",
    )
    .await?;
    let merge_ref = format!("refs/pull/{n}/merge");
    let fetched = deps
        .run(scope, fetch(vec![format!("+{merge_ref}:{merge_ref}")]))
        .await?;
    if !fetched.success() {
        scope
            .info("GitHub has no merge commit for this PR yet; trying again shortly")
            .await;
        return Err(ActivityError::retryable_after(
            "merge_ref_pending",
            MERGE_REF_WAIT,
        ));
    }
    let merge = deps
        .capture(scope, deps.mirror_git().args(["rev-parse", &merge_ref]))
        .await?;
    let tested_head = deps
        .capture(
            scope,
            deps.mirror_git().args(["rev-parse", &format!("{merge}^2")]),
        )
        .await?;
    if tested_head != pr.head_sha {
        scope
            .info(format!(
                "GitHub's merge commit is still for {tested_head}, not the head {}; \
                 trying again shortly",
                pr.head_sha
            ))
            .await;
        return Err(ActivityError::retryable_after(
            "merge_ref_stale",
            MERGE_REF_WAIT,
        ));
    }
    Ok(merge)
}

/// The CI clone (the CI user's) gets the PR from the mirror, without credentials.
async fn update_clone(
    deps: &CiDeps,
    scope: &StepScope,
    pr: &PullRequest,
) -> Result<(), ActivityError> {
    let clone = deps.config.ci_clone_dir();
    let mirror = format!("file://{}", deps.config.ci_mirror_dir().display());
    if !clone.join(".git").exists() {
        let parent = clone.parent().unwrap_or(Path::new("/"));
        // Provisioned on the host (owned by the CI user); created here when local.
        let _ = std::fs::create_dir_all(parent);
        if clone.exists() {
            deps.run_ok(
                scope,
                Cmd::new("rm", parent)
                    .args(["-rf", "--"])
                    .arg(clone.display().to_string()),
                ActivityError::retryable,
                "fs_failed",
            )
            .await?;
        }
        let mut git_clone = deps.clone_git();
        git_clone.cwd = parent.to_path_buf();
        deps.run_ok(
            scope,
            git_clone
                .args(["clone", "--no-checkout", "--quiet", &mirror])
                .arg(clone.display().to_string()),
            ActivityError::retryable,
            "git_clone_failed",
        )
        .await?;
    }
    for (key, value) in [("gears.hooks", "false"), ("core.hooksPath", "/dev/null")] {
        deps.run_ok(
            scope,
            deps.clone_git().args(["config", "--local", key, value]),
            ActivityError::retryable,
            "git_config_failed",
        )
        .await?;
    }
    let n = pr.number;
    deps.run_ok(
        scope,
        deps.clone_git().args([
            "fetch".to_owned(),
            "--force".to_owned(),
            "--no-tags".to_owned(),
            mirror,
            format!("+refs/heads/{0}:refs/remotes/origin/{0}", pr.base_ref),
            format!("+refs/pull/{n}/merge:refs/remotes/pull/{n}/merge"),
        ]),
        ActivityError::retryable,
        "git_fetch_failed",
    )
    .await
    .map(drop)
}

/// The path rules from the merge commit's workflow files, or the built-in copy.
async fn workflow_rules(deps: &CiDeps, scope: &StepScope, merge: &str) -> Rules {
    let show = async |path: &str| {
        deps.run(
            scope,
            deps.mirror_git()
                .args(["show", &format!("{merge}:{path}")])
                .quiet(),
        )
        .await
        .ok()
        .filter(CmdOutput::success)
        .map(|out| out.stdout)
    };
    let Some(ci) = show(".github/workflows/ci.yml").await else {
        scope
            .info("no .github/workflows/ci.yml in the merge commit; using the built-in path rules")
            .await;
        return Rules::builtin();
    };
    let fmt = show(".github/workflows/fmt.yml").await;
    match Rules::from_workflows(&ci, fmt.as_deref()) {
        Ok(rules) => rules,
        Err(error) => {
            scope
                .info(format!("{error}; using the built-in path rules"))
                .await;
            Rules::builtin()
        }
    }
}

/// Why `job` does not run in this run, if it does not.
fn skip_reason(state: &CiState, job: &Job) -> Option<String> {
    if let Some(only) = &state.only
        && !only.iter().any(|id| id == job.id)
    {
        return Some("not selected for this re-run".into());
    }
    // Jobs picked for a re-run ran before, so their filters matched.
    if state.only.is_some() {
        return None;
    }
    state.changes.as_ref().and_then(|c| job.skip_reason(c))
}

/// One GitHub job: a fresh checkout, then its steps until one fails.
async fn run_job(
    deps: Arc<CiDeps>,
    scope: StepScope,
    mut state: CiState,
    job: &'static Job,
) -> StepResult {
    if let Some(reason) = skip_reason(&state, job) {
        scope.info(format!("{}: skipped, {reason}", job.name)).await;
        state.record(JobResult {
            id: job.id.into(),
            conclusion: Conclusion::Skipped,
            failed_step: None,
            reason: Some(reason),
        });
        return Ok(state);
    }
    let merge = state
        .merge_sha
        .clone()
        .ok_or_else(|| ActivityError::permanent("commit_unknown"))?;
    let _clone = deps.lock_clone(&scope).await?;
    let _slot = deps
        .slot
        .acquire(
            &scope,
            format!("PR #{} {}", state.pr_number, job.name),
            Builder::PrCi,
        )
        .await?;
    steps::make_room(
        deps.exec.as_ref(),
        &scope,
        &deps.config.ci_target_dir(),
        deps.config.min_free_disk_gb,
    )
    .await;
    scope
        .info(format!("job: {} ({})", job.name, job.workflow))
        .await;
    if let Some(note) = job.note {
        scope.info(note).await;
    }
    let token = steps::secret(&deps.config, CI_TOKEN);
    scope
        .info(match token {
            Some(_) => format!("GitHub token for git dependencies: read-only ({CI_TOKEN})"),
            None => format!(
                "GitHub token for git dependencies: none ({CI_TOKEN} is not configured); \
                 git dependencies on GitHub may fail"
            ),
        })
        .await;
    for (from, to) in &deps.config.ci_git_rewrites {
        scope
            .info(format!("git dependencies: {from} is fetched from {to}"))
            .await;
    }
    deps.checkout(&scope, &merge).await?;
    for job_step in job.steps {
        scope.info(format!("step: {}", job_step.name)).await;
        for run in job_step.run {
            let cmd = check_command(&deps.config, run, job_step.condensed, token.as_ref());
            let Some(cmd) = cmd else { continue };
            let out = deps.run(&scope, cmd).await?;
            if !out.success() {
                scope
                    .info(format!("{}: `{}` failed", job.name, job_step.name))
                    .await;
                state.record(JobResult {
                    id: job.id.into(),
                    conclusion: Conclusion::Failure,
                    failed_step: Some(job_step.name.into()),
                    reason: None,
                });
                return Ok(state);
            }
        }
    }
    state.record(JobResult {
        id: job.id.into(),
        conclusion: Conclusion::Success,
        failed_step: None,
        reason: None,
    });
    Ok(state)
}

/// A `run:` line as a command in the CI clone (split on whitespace; `make` and
/// `cargo` resolve to the configured tools).
fn check_command(
    config: &PublisherConfig,
    run: &str,
    condensed: bool,
    token: Option<&SecretString>,
) -> Option<Cmd> {
    let mut words = run.split_whitespace();
    let program = match words.next()? {
        "make" => config.make_bin.as_str(),
        "cargo" => config.cargo_bin.as_str(),
        other => other,
    };
    // cargo's built-in libgit2 ignores `http.extraheader`; the git CLI uses it.
    let mut cmd = Cmd::new(program, config.ci_clone_dir())
        .args(words)
        .env(
            "CARGO_TARGET_DIR",
            config.ci_target_dir().display().to_string(),
        )
        .env("CARGO_NET_GIT_FETCH_WITH_CLI", "true")
        // As gears-rust's ci.yml: incremental artifacts only fill the disk.
        .env("CARGO_INCREMENTAL", "0")
        .env("CI", "true");
    let rewrites: Vec<(String, String)> = config
        .ci_git_rewrites
        .iter()
        .map(|(from, to)| (format!("url.{to}.insteadOf"), from.clone()))
        .collect();
    cmd = git_env(cmd, &rewrites, token);
    if condensed {
        cmd = cmd.condensed();
    }
    Some(cmd)
}

/// The run's verdict, like the checks list of a PR.
async fn conclusion(_: Arc<CiDeps>, scope: StepScope, state: CiState) -> StepResult {
    for job in &JOBS {
        let line = match state.jobs.iter().find(|j| j.id == job.id) {
            Some(JobResult {
                conclusion: Conclusion::Success,
                ..
            }) => format!("✓ {}", job.name),
            Some(JobResult {
                conclusion: Conclusion::Failure,
                failed_step,
                ..
            }) => format!(
                "✗ {}: {}",
                job.name,
                failed_step.as_deref().unwrap_or("failed")
            ),
            Some(JobResult { reason, .. }) => format!(
                "– {}: skipped{}",
                job.name,
                reason
                    .as_deref()
                    .map(|r| format!(", {r}"))
                    .unwrap_or_default()
            ),
            None => format!("? {}: no result", job.name),
        };
        scope.info(line).await;
    }
    let failed = state.failed();
    if failed.is_empty() {
        return Ok(state);
    }
    let names: Vec<&str> = failed
        .iter()
        .filter_map(|j| jobs::job(&j.id).map(|job| job.name))
        .collect();
    scope
        .info(format!(
            "{} failed: {}. Fix them in the PR, then Re-run",
            if names.len() == 1 { "1 job" } else { "jobs" },
            names.join(", ")
        ))
        .await;
    Err(ActivityError::permanent("checks_failed"))
}

#[cfg(test)]
mod tests;
