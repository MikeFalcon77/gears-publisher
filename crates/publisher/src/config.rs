//! Gear configuration: repository, paths, tools and the user directory.
use secrecy::SecretString;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use uuid::Uuid;

/// `gears.publisher.config` in the application YAML.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct PublisherConfig {
    /// Clone URL of the released repository.
    #[serde(default = "defaults::repo_url")]
    pub repo_url: String,
    /// `owner/name`, passed to `release_retry.py` and `gh`.
    #[serde(default = "defaults::github_repository")]
    pub github_repository: String,
    /// Holds the clone (`<work_dir>/gears-rust`) and per-run files (`<work_dir>/runs/<id>`).
    #[expand_vars]
    pub work_dir: String,
    /// Shared `CARGO_TARGET_DIR` for checks and publishing.
    #[expand_vars]
    pub target_dir: String,
    /// `KEY=value` file with `CARGO_REGISTRY_TOKEN`; read on every publish attempt.
    #[serde(default)]
    #[expand_vars]
    pub secrets_file: Option<String>,
    /// Python with `pyyaml`, used for `release_retry.py`.
    #[serde(default = "defaults::python")]
    #[expand_vars]
    pub python: String,
    #[serde(default = "defaults::release_plz")]
    pub release_plz_bin: String,
    #[serde(default = "defaults::git")]
    pub git_bin: String,
    #[serde(default = "defaults::gh")]
    pub gh_bin: String,
    #[serde(default = "defaults::make")]
    pub make_bin: String,
    #[serde(default = "defaults::cargo")]
    pub cargo_bin: String,
    /// Build user for every command that runs the repository's code (all but `gh`),
    /// through `sudo -n -u`. It must not be able to read `secrets_file`, the service's
    /// home or its binaries. Empty: run everything as the service (local development).
    #[serde(default)]
    #[expand_vars]
    pub run_as: Option<String>,
    #[serde(default = "defaults::sudo")]
    pub sudo_bin: String,
    /// Build user for PR CI: a pull request's code. Must differ from `run_as`, so
    /// that it cannot reach the release's clone and target directory. Empty: PR CI
    /// runs as the service when `run_as` is empty too (local development) and is
    /// refused otherwise.
    #[serde(default)]
    #[expand_vars]
    pub ci_run_as: Option<String>,
    /// The CI clone (`<ci_work_dir>/gears-rust`) and its target directory
    /// (`<ci_work_dir>/target`); writable by `ci_run_as` only. Default `<work_dir>/pr-ci`.
    #[serde(default)]
    #[expand_vars]
    pub ci_work_dir: Option<String>,
    /// The service's bare mirror the CI clone fetches PRs from: written by the service
    /// only, readable by `ci_run_as`. Default `<work_dir>/pr-mirror.git`.
    #[serde(default)]
    #[expand_vars]
    pub ci_mirror_dir: Option<String>,
    /// Git URL rewrites for PR checks, original prefix to replacement (git's
    /// `url.<replacement>.insteadOf <original>`): a git dependency is fetched from a
    /// copy, e.g. a public fork, when no CI token can read the original.
    #[serde(default)]
    pub ci_git_rewrites: BTreeMap<String, String>,
    /// Below this much free disk (GiB) on a build's filesystem, the build's own
    /// `target/debug` is removed before it starts.
    #[serde(default = "defaults::min_free_disk_gb")]
    pub min_free_disk_gb: u64,
    /// Commands of the `checks` step, run in order in the clone (split on whitespace;
    /// `make` resolves to `make_bin`). The default follows the jobs of gears-rust's
    /// `ci.yml` that need no Docker.
    #[serde(default = "defaults::checks")]
    pub checks: Vec<String>,
    /// Checks whose output is logged condensed to errors and warnings with some
    /// context (cargo-deny prints tens of thousands of lines of dependency trees).
    #[serde(default = "defaults::condensed_checks")]
    pub condensed_checks: Vec<String>,
    /// Extra environment for every subprocess (`CARGO_BUILD_JOBS`, `PATH`, ...).
    #[serde(default)]
    #[expand_vars]
    pub env: HashMap<String, String>,
    /// Built UI (`web/dist`).
    #[serde(default = "defaults::web_root")]
    #[expand_vars]
    pub web_root: String,
    /// PostgreSQL DSN of the durable-execution queue (the gear's own database).
    #[expand_vars]
    pub queue_dsn: SecretString,
    /// Durable queue name; unique per database.
    #[serde(default = "defaults::queue")]
    pub queue: String,
    /// `CARGO_HOME` whose `registry` and `git` are removed before `release-plz release`
    /// (as `release-plz.yml` does); unset: leave them.
    #[serde(default)]
    #[expand_vars]
    pub clean_cargo_home: Option<String>,
    /// crates.io sparse index root.
    #[serde(default = "defaults::crates_index")]
    pub crates_index_url: String,
    /// Firebase project and namespace: must match `publisher-authn-plugin`, so the
    /// gear derives the same subject ids.
    #[expand_vars]
    pub firebase_project_id: String,
    #[serde(default = "defaults::subject_namespace")]
    pub subject_namespace: Uuid,
    /// Admins: releases, PR CI and approving access requests. Everyone else asks
    /// for access in the UI. `users` is the name before access requests existed.
    #[serde(default, alias = "users")]
    pub admins: Vec<User>,
    /// Steps' tunables.
    #[serde(default)]
    pub timings: Timings,
}

/// One admin. Fields of the old plugin list (`tenant_id`, `token_scopes`) are ignored.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
pub struct User {
    #[expand_vars]
    pub uid: String,
    #[expand_vars]
    pub email: String,
}

/// Poll and retry intervals, configurable so that tests and dry runs are fast.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timings {
    /// Pause between `gh pr view` polls while the release PR is open.
    pub merge_poll_secs: u64,
    /// How long one `wait_merge` attempt polls before handing back to the engine.
    pub merge_attempt_window_secs: u64,
    /// Margin added to crates.io's `try again after` time.
    pub rate_limit_margin_secs: u64,
    /// Publish attempts for errors other than 429.
    pub publish_attempts: u32,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            merge_poll_secs: 60,
            merge_attempt_window_secs: 3600,
            rate_limit_margin_secs: 15,
            publish_attempts: 6,
        }
    }
}

impl PublisherConfig {
    #[must_use]
    pub fn clone_dir(&self) -> PathBuf {
        PathBuf::from(&self.work_dir).join("gears-rust")
    }
    #[must_use]
    pub fn runs_dir(&self) -> PathBuf {
        PathBuf::from(&self.work_dir).join("runs")
    }
    /// The configured build user, if any.
    #[must_use]
    pub fn build_user(&self) -> Option<&str> {
        non_empty(self.run_as.as_deref())
    }
    /// The configured PR CI build user, if any.
    #[must_use]
    pub fn ci_user(&self) -> Option<&str> {
        non_empty(self.ci_run_as.as_deref())
    }
    /// Why PR CI cannot run with this configuration, if it cannot.
    #[must_use]
    pub fn ci_refusal(&self) -> Option<&'static str> {
        match (self.build_user(), self.ci_user()) {
            (Some(_), None) => Some(
                "PR CI needs its own build user: set publisher.ci_run_as (PUBLISHER_CI_RUN_AS)",
            ),
            (Some(release), Some(ci)) if release == ci => {
                Some("publisher.ci_run_as must differ from run_as: PR code would reach the release")
            }
            _ => None,
        }
    }
    #[must_use]
    pub fn ci_work_dir(&self) -> PathBuf {
        non_empty(self.ci_work_dir.as_deref()).map_or_else(
            || PathBuf::from(&self.work_dir).join("pr-ci"),
            PathBuf::from,
        )
    }
    #[must_use]
    pub fn ci_clone_dir(&self) -> PathBuf {
        self.ci_work_dir().join("gears-rust")
    }
    #[must_use]
    pub fn ci_target_dir(&self) -> PathBuf {
        self.ci_work_dir().join("target")
    }
    /// `TMPDIR` of the release's build user (made by `deploy/provision.sh`).
    #[must_use]
    pub fn tmp_dir(&self) -> PathBuf {
        PathBuf::from(&self.work_dir).join("tmp")
    }
    /// `TMPDIR` of the PR CI build user (made by `deploy/provision.sh`).
    #[must_use]
    pub fn ci_tmp_dir(&self) -> PathBuf {
        self.ci_work_dir().join("tmp")
    }
    /// The service's bare mirror the CI clone fetches PRs from.
    #[must_use]
    pub fn ci_mirror_dir(&self) -> PathBuf {
        non_empty(self.ci_mirror_dir.as_deref()).map_or_else(
            || PathBuf::from(&self.work_dir).join("pr-mirror.git"),
            PathBuf::from,
        )
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(run_as: &str, ci_run_as: &str) -> PublisherConfig {
        serde_json::from_value(serde_json::json!({
            "work_dir": "/w", "target_dir": "/t", "queue_dsn": "postgres://x",
            "firebase_project_id": "p", "run_as": run_as, "ci_run_as": ci_run_as,
        }))
        .unwrap()
    }

    #[test]
    fn pr_ci_needs_its_own_build_user_when_the_release_has_one() {
        assert!(config("", "").ci_refusal().is_none());
        assert!(config("publisher-build", "").ci_refusal().is_some());
        assert!(
            config("publisher-build", "publisher-build")
                .ci_refusal()
                .is_some()
        );
        assert!(
            config("publisher-build", "publisher-ci")
                .ci_refusal()
                .is_none()
        );
        assert_eq!(
            config("", "").ci_clone_dir(),
            PathBuf::from("/w/pr-ci/gears-rust")
        );
    }

    /// deploy's publisher.yaml sets `python: "${PUBLISHER_PYTHON:-python3}"`.
    #[test]
    fn python_is_expanded() {
        use toolkit::var_expand::ExpandVars as _;
        let mut config: PublisherConfig = serde_json::from_value(serde_json::json!({
            "work_dir": "/w", "target_dir": "/t", "queue_dsn": "postgres://x",
            "firebase_project_id": "p",
            "python": "${PUBLISHER_TEST_UNSET_PYTHON:-python3}",
        }))
        .unwrap();
        config.expand_vars().unwrap();
        assert_eq!(config.python, "python3");
    }
}

mod defaults {
    pub fn repo_url() -> String {
        "https://github.com/constructorfabric/gears-rust.git".into()
    }
    pub fn github_repository() -> String {
        "constructorfabric/gears-rust".into()
    }
    pub fn python() -> String {
        "python3".into()
    }
    pub fn release_plz() -> String {
        "release-plz".into()
    }
    pub fn git() -> String {
        "git".into()
    }
    pub fn gh() -> String {
        "gh".into()
    }
    pub fn make() -> String {
        "make".into()
    }
    pub fn sudo() -> String {
        "/usr/bin/sudo".into()
    }
    pub fn cargo() -> String {
        "cargo".into()
    }
    pub fn min_free_disk_gb() -> u64 {
        80
    }
    pub fn checks() -> Vec<String> {
        [
            // fmt.yml
            "make fmt",
            // ci.yml `clippy`
            "cargo xtask check-test-container-pins",
            "python3 tools/scripts/check_crypto_facade.py",
            "python3 -m unittest discover -s tools/scripts/tests -p test_crypto_facade.py",
            "make clippy",
            // ci.yml `repo-checks`
            "python3 tools/scripts/validate_gear_names.py",
            "python3 tools/scripts/toolkit-pr-review/lint.py",
            "python3 -m unittest discover -s tools/scripts/toolkit-pr-review/tests",
            "make check-packaging-metadata",
            "make check-release-config",
            "make check-release-resolver",
            // ci.yml `security` and `dylint` (cargo gears lint --dylint)
            "make deny",
            "make dylint",
            // test-suite.yml `nextest`, ci.yml `integration` (without database lanes)
            "make test-no-macros",
            "make test-macros",
        ]
        .map(String::from)
        .to_vec()
    }
    pub fn condensed_checks() -> Vec<String> {
        vec!["make deny".into()]
    }
    pub fn web_root() -> String {
        "web/dist".into()
    }
    pub fn queue() -> String {
        "publisher".into()
    }
    pub fn crates_index() -> String {
        "https://index.crates.io".into()
    }
    pub fn subject_namespace() -> uuid::Uuid {
        uuid::Uuid::NAMESPACE_URL
    }
}
