use super::*;
use crate::domain::ports::journal::MemoryJournal;
use crate::domain::testing::{FakeExec, fail, ok};
use async_trait::async_trait;
use std::collections::HashMap;
use toolkit_durable_execution::testing::{TestOutcome, TestRuntime};

struct FakeRegistry(HashMap<String, Vec<String>>);

#[async_trait]
impl Registry for FakeRegistry {
    async fn versions(&self, name: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.0.get(name).cloned().unwrap_or_default())
    }
}

fn published() -> FakeRegistry {
    FakeRegistry(HashMap::from([
        (
            "cf-a".to_owned(),
            vec!["0.1.0".to_owned(), "0.2.0".to_owned()],
        ),
        ("cf-b".to_owned(), vec!["1.0.1".to_owned()]),
    ]))
}

const METADATA: &str = r#"{"workspace_members":["a","b","app","priv"],"packages":[
  {"id":"a","name":"cf-a","version":"0.2.0","publish":null},
  {"id":"b","name":"cf-b","version":"1.0.1","publish":null},
  {"id":"app","name":"app","version":"0.3.0","publish":[]},
  {"id":"priv","name":"internal","version":"0.0.1","publish":[]}]}"#;

const TAGS: &str = "cf-a-v0.2.0\ncf-b-v1.0.1\napp-v0.3.0\n";

const RELEASE_PR: &str = r#"{"prs":[{"base_branch":"main","head_branch":"release-plz-x","html_url":"https://github.com/o/r/pull/7","number":7,"releases":[{"package_name":"cf-a","version":"0.2.0"},{"package_name":"cf-b","version":"1.0.1"}]}]}"#;

const RELEASED: &str =
    r#"{"releases":[{"package_name":"cf-b","prs":[],"tag":"cf-b-v1.0.1","version":"1.0.1"}]}"#;

const RATE_LIMITED: &str = "the remote server responded with an error (status 429 Too Many Requests): You have published too many updates to existing crates in a short period of time. Please try again after Wed, 18 Nov 2026 15:16:16 GMT and see https://crates.io/docs/rate-limits for more details.";

struct Harness {
    _dir: tempfile::TempDir,
    work: PathBuf,
    exec: Arc<FakeExec>,
    journal: Arc<MemoryJournal>,
    workflow: Workflow<ReleaseInput, ReleaseState>,
}

fn harness(exec: FakeExec, registry: FakeRegistry, timings: &serde_json::Value) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().to_path_buf();
    // A clone that already exists: prepare only fetches.
    std::fs::create_dir_all(work.join("gears-rust/.git")).unwrap();
    std::fs::write(
        work.join("gears-rust/release-plz.toml"),
        "[[package]]\nname = \"app\"\ngit_only = true\n",
    )
    .unwrap();
    let secrets = work.join("secrets.env");
    std::fs::write(&secrets, "CARGO_REGISTRY_TOKEN=cio_test_token\n").unwrap();
    let config: PublisherConfig = serde_json::from_value(serde_json::json!({
        "work_dir": work.display().to_string(),
        "target_dir": work.join("target").display().to_string(),
        "secrets_file": secrets.display().to_string(),
        "queue_dsn": "postgres://unused",
        "firebase_project_id": "test",
        "timings": timings,
    }))
    .unwrap();
    let exec = Arc::new(exec);
    let journal = Arc::new(MemoryJournal::default());
    let deps = Arc::new(Deps {
        config,
        exec: Arc::clone(&exec) as Arc<dyn Exec>,
        journal: Arc::clone(&journal) as Arc<dyn Journal>,
        registry: Arc::new(registry),
        slot: Arc::new(BuildSlot::default()),
    });
    Harness {
        workflow: definition(&deps).unwrap(),
        _dir: dir,
        work,
        exec,
        journal,
    }
}

fn fast() -> serde_json::Value {
    serde_json::json!({ "merge_poll_secs": 1, "merge_attempt_window_secs": 0 })
}

/// `collect` writes its outputs where `GITHUB_OUTPUT` points.
fn collect_writes(cmd: &Cmd, releases: &str) -> CmdOutput {
    let path = cmd
        .env
        .iter()
        .find(|(k, _)| k == "GITHUB_OUTPUT")
        .map(|(_, v)| v.clone())
        .unwrap();
    std::fs::write(
        path,
        format!("releases={releases}\nreleases_created=true\n"),
    )
    .unwrap();
    ok("")
}

fn env<'a>(cmd: &'a Cmd, key: &str) -> Option<&'a str> {
    cmd.env
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn release_exec() -> FakeExec {
    FakeExec::default()
        .on("rev-parse HEAD", |_, _| ok("abc123\n"))
        .on("rev-parse origin/main", |_, _| ok("abc123\n"))
        .on("gh auth token", |_, _| ok("gho_test_token\n"))
        .on("release-plz release-pr", |_, _| ok(RELEASE_PR))
        .on("gh pr view 7", |_, n| {
            if n < 3 {
                ok(r#"{"state":"OPEN","mergeCommit":null}"#)
            } else {
                ok(r#"{"state":"MERGED","mergeCommit":{"oid":"def456"}}"#)
            }
        })
        // 1: the catch-up release on main finds nothing; 2: 429; 3: the merged PR.
        .on("release-plz release -o json", |_, n| match n {
            1 => ok(r#"{"releases":[]}"#),
            2 => fail(&[RATE_LIMITED]),
            _ => ok(RELEASED),
        })
        .on("release_retry.py collect", |cmd, _| {
            if env(cmd, "OUTCOME_2") == Some("success") {
                collect_writes(
                    cmd,
                    r#"[{"package_name":"cf-b","version":"1.0.1","tag":"cf-b-v1.0.1"},{"package_name":"cf-a","version":"0.2.0","tag":"cf-a-v0.2.0"}]"#,
                )
            } else {
                collect_writes(cmd, "[]")
            }
        })
        .on("metadata --no-deps", |_, _| ok(METADATA))
        .on("tag -l", |_, _| ok(TAGS))
}

#[tokio::test]
async fn a_release_goes_through_rate_limits_to_verification() {
    let h = harness(release_exec(), published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: false })
        .await
        .unwrap();
    assert_eq!(run.attempts("pre_release"), 1);
    assert_eq!(run.attempts("wait_merge"), 3);
    assert_eq!(run.attempts("publish"), 2);
    let state = run.into_output().unwrap();
    assert_eq!(state.main_sha.as_deref(), Some("abc123"));
    assert_eq!(state.pr.as_ref().map(|p| p.number), Some(7));
    assert_eq!(state.packages.len(), 2);
    assert_eq!(state.merge_sha.as_deref(), Some("def456"));
    let publish = state.publish.unwrap();
    assert_eq!(publish.attempt, 2);
    assert_eq!(publish.releases.len(), 1);
    assert_eq!(state.releases.map(|r| r.len()), Some(2));
    let verify = state.verify.unwrap();
    assert!(verify.clean(), "{verify:?}");
    assert_eq!(verify.checked, 3);
    // cf-a 0.2.0 replaced 0.1.0; cf-b 1.0.1 is its first version.
    assert_eq!(
        verify.previous,
        BTreeMap::from([("cf-a".to_owned(), "0.1.0".to_owned())])
    );
    // `app` has `publish = []`: released as a tag only.
    assert_eq!(verify.git_only, ["app"]);
    assert!(state.skipped.is_empty());
    assert_eq!(state.pre_released, Some(Vec::new()));
    // Before the retry: release_retry.py wait (API budget, tags), as in release-plz.yml.
    assert_eq!(h.exec.calls_matching("release_retry.py wait"), 1);
    assert_eq!(h.exec.calls_matching("release_retry.py snapshot"), 2);

    // Every step after the merge works on the merge commit, never on main again.
    let calls = h.exec.calls.lock().clone();
    let first_merge = calls
        .iter()
        .position(|c| c.contains("checkout --force -B work def456"))
        .expect("the merge commit is checked out");
    assert!(
        !calls[first_merge..]
            .iter()
            .any(|c| c.contains("checkout --force -B work abc123"))
    );
    // checks, pre_release and release_pr each put the tree at main first.
    assert_eq!(h.exec.calls_matching("checkout --force -B work abc123"), 3);
    // Hooks of the repository are off before anything runs in the clone.
    assert_eq!(
        h.exec
            .calls_matching("config --local core.hooksPath /dev/null"),
        1
    );
    // Both attempts' outputs are kept.
    let dir = h.work.join("runs");
    let run_dir = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(run_dir.join("pre_release/release-1.json").exists());
    assert!(run_dir.join("publish/release-1.json").exists());
    assert!(run_dir.join("publish/release-2.json").exists());
    // Tokens never reach the log.
    assert!(
        !h.journal
            .lines
            .lock()
            .iter()
            .any(|l| l.line.contains("gho_test_token") || l.line.contains("cio_test_token"))
    );
    // Facts are recorded after steps.
    assert!(h.journal.facts.lock().len() >= 8);
}

#[tokio::test]
async fn collect_recovers_earlier_attempts_when_a_retry_succeeded() {
    let exec = release_exec().on("release_retry.py collect", |cmd, _| {
        if env(cmd, "OUTCOME_1") == Some("success") {
            // The catch-up release succeeded at once.
            assert!(env(cmd, "RELEASES_1").is_some());
            return collect_writes(cmd, "[]");
        }
        assert_eq!(env(cmd, "OUTCOME_1"), Some("failure"));
        assert_eq!(env(cmd, "OUTCOME_2"), Some("success"));
        assert!(env(cmd, "RELEASES_2").unwrap().contains("cf-b-v1.0.1"));
        assert!(
            env(cmd, "TAGS_BEFORE")
                .unwrap()
                .ends_with("tags-before.txt")
        );
        assert!(cmd.secrets.iter().any(|(k, _)| k == "GH_TOKEN"));
        collect_writes(cmd, "[]")
    });
    // Rules match in order: put the asserting rule first.
    let exec = exec.reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap();
    assert!(
        matches!(run.outcome(), TestOutcome::Succeeded(_)),
        "{:?}",
        run.outcome()
    );
}

#[tokio::test]
async fn nothing_to_release_skips_the_rest() {
    let exec = FakeExec::default()
        .on("rev-parse origin/main", |_, _| ok("abc123\n"))
        .on("gh auth token", |_, _| ok("gho_x_token\n"))
        .on("release-plz release-pr", |_, _| ok(r#"{"prs":[]}"#))
        .on("release_retry.py collect", |cmd, _| {
            collect_writes(cmd, "[]")
        });
    let h = harness(exec, published(), &fast());
    let state = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap()
        .into_output()
        .unwrap();
    assert!(state.nothing_to_release);
    assert_eq!(
        state.skipped,
        ["wait_merge", "sync_main", "publish", "heal_tags", "verify"]
    );
    // Only the catch-up release on main ran.
    assert_eq!(h.exec.calls_matching("release-plz release -o json"), 1);
}

#[tokio::test]
async fn a_closed_pr_fails_the_run() {
    let exec = release_exec().on("gh pr view", |_, _| ok(r#"{"state":"CLOSED"}"#));
    let exec = exec.reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap();
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "wait_merge".into(),
            code: "pr_closed".into()
        }
    );
}

#[tokio::test]
async fn failed_checks_stop_without_retrying() {
    let exec = release_exec().on("make test-no-macros", |_, _| fail(&["test failed"]));
    let exec = exec.reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap();
    assert_eq!(run.attempts("checks"), 1);
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "checks".into(),
            code: "checks_failed".into()
        }
    );
    assert_eq!(h.exec.calls_matching("make test-macros"), 0);
}

#[tokio::test]
async fn publish_gives_up_after_the_configured_failures() {
    let exec = release_exec().on("release-plz release -o json", |_, _| {
        fail(&["error: failed to verify package"])
    });
    let exec = exec.reversed();
    let h = harness(
        exec,
        published(),
        &serde_json::json!({ "merge_poll_secs": 1, "merge_attempt_window_secs": 0, "publish_attempts": 2 }),
    );
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap();
    assert_eq!(run.attempts("pre_release"), 3);
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "pre_release".into(),
            code: "publish_failed".into()
        }
    );
    assert_eq!(h.exec.calls_matching("release-plz release -o json"), 2);
}

#[tokio::test]
async fn a_dry_run_ends_at_the_first_unpublished_dependency() {
    let exec = FakeExec::default()
        .on("rev-parse HEAD", |_, _| ok("abc123\n"))
        .on("rev-parse origin/main", |_, _| ok("abc123\n"))
        .on("gh auth token", |_, _| ok("gho_test_token\n"))
        // git reads gears-rust with the token as an extra header, not anonymously.
        .on("fetch origin", |cmd, _| {
            assert!(cmd.secrets.iter().any(|(k, _)| k == "GIT_CONFIG_VALUE_0"));
            assert!(!cmd.args.iter().any(|a| a.contains("gho_test_token")));
            ok("")
        })
        // The changelog template reads GitHub: update gets the token, as release-pr does.
        .on("release-plz update", |cmd, _| {
            assert!(cmd.secrets.iter().any(|(k, _)| k == "GIT_TOKEN"));
            ok("")
        })
        .on("diff --stat", |_, _| ok(" libs/a/Cargo.toml | 2 +-\n"))
        .on("release-plz release -o json --dry-run", |cmd, _| {
            // The tree carries `release-plz update`'s uncommitted changes.
            assert!(cmd.args.iter().any(|a| a == "--allow-dirty"));
            // release-plz needs a forge even in a dry run (it looks up the release PR).
            assert!(cmd.secrets.iter().any(|(k, _)| k == "GIT_TOKEN"));
            assert!(!cmd.secrets.iter().any(|(k, _)| k == "CARGO_REGISTRY_TOKEN"));
            fail(&[
                "cf-a 0.2.0: due to dry, skipping the following: [\"cargo registry upload\"]",
                "failed to select a version for the requirement `cf-a = \"^0.2.0\"`",
            ])
        })
        .on("metadata --no-deps", |_, _| ok(METADATA))
        .on("tag -l", |_, _| ok("cf-b-v1.0.1\n"));
    // cf-a 0.2.0 is not published yet: expected in a dry run.
    let registry = FakeRegistry(HashMap::from([(
        "cf-b".to_owned(),
        vec!["1.0.1".to_owned()],
    )]));
    let h = harness(exec, registry, &fast());
    let state = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: true })
        .await
        .unwrap()
        .into_output()
        .unwrap();
    assert!(state.update_diff.unwrap().contains("Cargo.toml"));
    assert!(state.publish.unwrap().note.is_some());
    assert_eq!(state.skipped, ["pre_release", "wait_merge", "heal_tags"]);
    let verify = state.verify.unwrap();
    assert_eq!(verify.missing_on_crates_io, ["cf-a 0.2.0"]);
    assert_eq!(h.exec.calls_matching("release-pr"), 0);
    // release_pr, publish and verify share one `release-plz update` of main.
    assert_eq!(h.exec.calls_matching("release-plz update"), 1);
    // A dry run asks gh only for the token git reads with: no PR, no API calls.
    assert_eq!(
        h.exec.calls_matching("gh "),
        h.exec.calls_matching("gh auth token")
    );
    assert_eq!(h.exec.calls_matching("collect"), 0);
}

#[tokio::test]
async fn verification_differences_fail_the_run() {
    let registry = FakeRegistry(HashMap::from([(
        "cf-a".to_owned(),
        vec!["0.2.0".to_owned()],
    )]));
    let h = harness(release_exec(), registry, &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput::default())
        .await
        .unwrap();
    assert_eq!(run.attempts("verify"), 10);
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "verify".into(),
            code: "verify_mismatch".into()
        }
    );
    let facts = h.journal.facts.lock();
    let last = facts.last().unwrap();
    assert_eq!(
        last["verify"]["missing_on_crates_io"],
        serde_json::json!(["cf-b 1.0.1"])
    );
}

#[test]
fn releasable_packages_follow_release_retry() {
    let git_only = git_only_packages("[[package]]\nname = \"app\"\ngit_only = true\n");
    let packages = releasable(METADATA, &git_only).unwrap();
    let names: Vec<_> = packages
        .iter()
        .map(|p| (p.name.as_str(), p.crates_io))
        .collect();
    assert_eq!(names, [("app", false), ("cf-a", true), ("cf-b", true)]);
}

#[test]
fn previous_version_is_the_highest_below_current() {
    let versions = ["0.1.0", "0.10.0", "0.9.0", "0.11.0-rc.1", "0.11.0", "junk"].map(String::from);
    assert_eq!(
        previous_version(&versions, "0.11.0").as_deref(),
        Some("0.11.0-rc.1")
    );
    assert_eq!(
        previous_version(&versions, "0.10.1").as_deref(),
        Some("0.10.0")
    );
    assert_eq!(previous_version(&versions, "0.1.0"), None);
    assert_eq!(previous_version(&versions, "not-semver"), None);
}

#[tokio::test]
async fn checks_follow_main_when_it_moves_before_the_release_pr() {
    // The first `make fmt` is killed (a restart); main gets a fix meanwhile.
    let exec = release_exec()
        .on("make fmt", |_, n| {
            if n == 1 {
                CmdOutput {
                    code: None,
                    ..ok("")
                }
            } else {
                ok("")
            }
        })
        .on("rev-parse origin/main", |_, n| {
            ok(if n == 1 { "abc123\n" } else { "fix789\n" })
        });
    let exec = exec.reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: true })
        .await
        .unwrap();
    assert_eq!(run.attempts("checks"), 2);
    let state = run.into_output().unwrap();
    assert_eq!(state.main_sha.as_deref(), Some("fix789"));
    assert!(h.exec.calls_matching("checkout --force -B work fix789") >= 1);
}

/// The first `make test-no-macros` is killed (a restart); the second passes.
fn interrupted_tests(exec: FakeExec) -> FakeExec {
    exec.on("make test-no-macros", |_, n| {
        if n == 1 {
            CmdOutput {
                code: None,
                ..ok("")
            }
        } else {
            ok("")
        }
    })
}

#[tokio::test]
async fn checks_passed_on_the_same_commit_are_not_run_again() {
    let exec = interrupted_tests(release_exec()).reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: true })
        .await
        .unwrap();
    assert_eq!(run.attempts("checks"), 2);
    assert_eq!(h.exec.calls_matching("make fmt"), 1);
    assert_eq!(h.exec.calls_matching("make clippy"), 1);
    assert_eq!(h.exec.calls_matching("make dylint"), 1);
    assert_eq!(h.exec.calls_matching("make test-no-macros"), 2);
    assert_eq!(h.exec.calls_matching("make test-macros"), 1);
    assert!(
        h.journal
            .lines
            .lock()
            .iter()
            .any(|l| l.attempt == 2 && l.line.contains("make clippy: already passed on abc123"))
    );
}

#[tokio::test]
async fn checks_run_again_when_main_moves() {
    let exec = interrupted_tests(release_exec())
        .on("rev-parse origin/main", |_, n| {
            ok(if n == 1 { "abc123\n" } else { "fix789\n" })
        })
        .reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: true })
        .await
        .unwrap();
    assert_eq!(run.attempts("checks"), 2);
    assert_eq!(h.exec.calls_matching("make clippy"), 2);
    assert_eq!(h.exec.calls_matching("make test-no-macros"), 2);
}

#[tokio::test]
async fn checks_fetch_git_dependencies_with_the_token() {
    // `make dylint` builds its lints from a git dependency on GitHub.
    let exec = release_exec().on("make dylint", |cmd, _| {
        assert!(cmd.secrets.iter().any(|(k, _)| k == "GIT_CONFIG_VALUE_0"));
        assert!(
            cmd.env
                .iter()
                .any(|(k, v)| k == "CARGO_NET_GIT_FETCH_WITH_CLI" && v == "true")
        );
        assert!(
            cmd.env
                .iter()
                .any(|(k, v)| k == "CARGO_INCREMENTAL" && v == "0")
        );
        ok("")
    });
    let exec = exec.reversed();
    let h = harness(exec, published(), &fast());
    let run = TestRuntime::new()
        .run(&h.workflow, ReleaseInput { dry_run: true })
        .await
        .unwrap();
    assert_eq!(run.attempts("checks"), 1);
    assert_eq!(h.exec.calls_matching("make dylint"), 1);
}
