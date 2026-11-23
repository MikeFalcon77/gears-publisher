use super::*;
use crate::domain::ports::journal::MemoryJournal;
use crate::domain::testing::{FakeExec, fail, ok};
use secrecy::ExposeSecret;
use std::path::PathBuf;
use toolkit_durable_execution::testing::{TestOutcome, TestRuntime};

const HEAD: &str = "4ead000000000000000000000000000000000001";
const MERGE: &str = "3e76e00000000000000000000000000000000002";
const BASE: &str = "ba5e000000000000000000000000000000000003";
const SERVICE_TOKEN: &str = "gho_service_token";
const CI_TOKEN_VALUE: &str = "github_pat_read_only";

const CI_YML: &str = r"
on:
  pull_request:
    paths-ignore: ['**/*.md', 'docs/**']
jobs:
  changes:
    steps:
      - id: filter
        with:
          filters: |
            rust: ['**/*.rs', 'Cargo.lock']
            deps: ['**/Cargo.toml', 'Cargo.lock', 'deny.toml']
            ci: ['.github/workflows/**']
";
const FMT_YML: &str = "on:\n  pull_request:\n    paths: ['**/*.rs']\n";

fn pr_view(mergeable: &str) -> String {
    format!(
        r#"{{"number":42,"title":"Add a thing","url":"https://github.com/o/r/pull/42","state":"OPEN","mergeable":"{mergeable}","author":{{"login":"dev"}},"headRefName":"dev/thing","headRefOid":"{HEAD}","baseRefName":"main","isCrossRepository":false}}"#
    )
}

/// A PR changing `files`; the merge ref is current.
fn exec(files: &'static str) -> FakeExec {
    FakeExec::default()
        .on("gh auth token", |_, _| ok(&format!("{SERVICE_TOKEN}\n")))
        .on("gh pr view 42", |_, _| ok(&pr_view("MERGEABLE")))
        .on("^2", |_, _| ok(&format!("{HEAD}\n")))
        .on("^1", |_, _| ok(&format!("{BASE}\n")))
        .on("rev-parse refs/pull/42/merge", |_, _| {
            ok(&format!("{MERGE}\n"))
        })
        .on("diff --name-only", move |_, _| ok(files))
        .on(".github/workflows/ci.yml", |_, _| ok(CI_YML))
        .on(".github/workflows/fmt.yml", |_, _| ok(FMT_YML))
}

struct Harness {
    _dir: tempfile::TempDir,
    work: PathBuf,
    exec: Arc<FakeExec>,
    journal: Arc<MemoryJournal>,
    workflow: Workflow<CiInput, CiState>,
}

impl Harness {
    fn journal_lines(&self) -> Vec<String> {
        self.journal
            .lines
            .lock()
            .iter()
            .map(|l| l.line.clone())
            .collect()
    }
}

fn harness(exec: FakeExec, ci_token: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().to_path_buf();
    // The mirror and the clone exist already.
    std::fs::create_dir_all(work.join("pr-mirror.git")).unwrap();
    std::fs::write(work.join("pr-mirror.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::create_dir_all(work.join("pr-ci/gears-rust/.git")).unwrap();
    let secrets = work.join("secrets.env");
    let text = if ci_token {
        format!("{CI_TOKEN}={CI_TOKEN_VALUE}\n")
    } else {
        String::new()
    };
    std::fs::write(&secrets, text).unwrap();
    let config: PublisherConfig = serde_json::from_value(serde_json::json!({
        "work_dir": work.display().to_string(),
        "target_dir": work.join("target").display().to_string(),
        "secrets_file": secrets.display().to_string(),
        "queue_dsn": "postgres://unused",
        "firebase_project_id": "test",
    }))
    .unwrap();
    let exec = Arc::new(exec);
    let journal = Arc::new(MemoryJournal::default());
    let deps = Arc::new(CiDeps {
        config,
        exec: Arc::clone(&exec) as Arc<dyn Exec>,
        journal: Arc::clone(&journal) as Arc<dyn Journal>,
        slot: Arc::new(BuildSlot::default()),
        clone: Mutex::new(()),
    });
    Harness {
        workflow: definition(&deps).unwrap(),
        _dir: dir,
        work,
        exec,
        journal,
    }
}

fn input() -> CiInput {
    CiInput { pr: 42, jobs: None }
}

fn conclusions(state: &CiState) -> Vec<(&str, Conclusion)> {
    state
        .jobs
        .iter()
        .map(|j| (j.id.as_str(), j.conclusion))
        .collect()
}

#[tokio::test]
async fn a_failed_job_does_not_stop_the_others() {
    let exec = exec("gears/x/src/lib.rs\n")
        .on("make clippy", |_, _| fail(&["error: unused variable"]))
        .reversed();
    let h = harness(exec, true);
    let run = TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "conclusion".into(),
            code: "checks_failed".into()
        }
    );
    // Every job ran once; the failing one was not retried.
    for job in &JOBS {
        assert_eq!(run.attempts(job.id), 1, "{}", job.id);
    }
    assert_eq!(h.exec.calls_matching("make clippy"), 1);
    // After clippy failed, the jobs behind it still ran.
    assert_eq!(h.exec.calls_matching("make test-no-macros"), 1);
    assert_eq!(h.exec.calls_matching("make dylint"), 1);
    // Clippy's own later steps... it has none after `cargo clippy`; its earlier ones ran.
    assert_eq!(h.exec.calls_matching("check-test-container-pins"), 1);
}

/// PR #5724: the disk filled while the tests linked. The job says so, names the
/// test, and the run tells to re-run rather than to fix the PR.
#[tokio::test]
async fn a_full_disk_is_named_as_the_hosts_fault() {
    let exec = exec("gears/x/src/lib.rs\n")
        .on("make test-no-macros", |_, _| {
            fail(&[
                "    warning: unexpected `cfg` condition value: `grpc-client`",
                "              collect2: fatal error: ld terminated with signal 7 [Bus error]",
                "        FAIL [ 715.813s] (27693/27693) cf-gears-toolkit-contract-macros-tests::ui ui",
            ])
        })
        .reversed();
    let h = harness(exec, true);
    let run = TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    assert!(matches!(run.outcome(), TestOutcome::Failed { .. }));
    let lines = h.journal_lines();
    assert!(
        lines.iter().any(|l| l.contains("ran out of disk space")
            && l.contains("cf-gears-toolkit-contract-macros-tests::ui ui")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("not the PR: Re-run failed jobs"))
    );
}

#[tokio::test]
async fn jobs_run_on_the_merge_commit_in_their_own_tree() {
    let h = harness(exec("gears/x/src/lib.rs\nCargo.lock\n"), true);
    let run = TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    let state = run.into_output().unwrap();
    assert_eq!(state.merge_sha.as_deref(), Some(MERGE));
    assert_eq!(state.base_sha.as_deref(), Some(BASE));
    assert_eq!(state.pr.as_ref().map(|p| p.head_sha.as_str()), Some(HEAD));
    assert_eq!(state.changed_files, 2);
    assert_eq!(state.rules.as_deref(), Some(".github/workflows/ci.yml"));
    assert!(
        conclusions(&state)
            .iter()
            .all(|(_, c)| *c == Conclusion::Success),
        "{:?}",
        state.jobs
    );
    // A fresh checkout of the merge commit before every job.
    assert_eq!(
        h.exec
            .calls_matching(&format!("checkout --force --detach {MERGE}")),
        JOBS.len()
    );
    let cmds = h.exec.cmds.lock().clone();
    let clone = h.work.join("pr-ci/gears-rust");
    let target = h.work.join("pr-ci/target").display().to_string();
    for cmd in cmds.iter().filter(|c| c.program == "make") {
        assert_eq!(cmd.cwd, clone);
        assert!(!cmd.trusted, "{}", cmd.display());
        assert!(
            cmd.env
                .iter()
                .any(|(k, v)| k == "CARGO_TARGET_DIR" && *v == target),
            "{}",
            cmd.display()
        );
    }
}

/// A secret as the command receives it, git's extraheader decoded.
fn revealed(secret: &str) -> String {
    use base64::Engine as _;
    secret
        .strip_prefix("AUTHORIZATION: basic ")
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map_or_else(
            || secret.to_owned(),
            |b| String::from_utf8_lossy(&b).into_owned(),
        )
}

#[tokio::test]
async fn the_service_token_never_reaches_the_prs_code() {
    let h = harness(exec("src/lib.rs\n"), true);
    TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    let cmds = h.exec.cmds.lock().clone();
    let mut checks = 0;
    for cmd in &cmds {
        let secrets: Vec<String> = cmd
            .secrets
            .iter()
            .map(|(_, v)| revealed(v.expose_secret()))
            .collect();
        let service = secrets.iter().any(|s| s.contains(SERVICE_TOKEN));
        if service {
            // Only the service's own git on its mirror holds it.
            assert!(cmd.trusted && cmd.program == "git", "{}", cmd.display());
            assert!(cmd.args.contains(&"fetch".to_owned()), "{}", cmd.display());
        }
        if cmd.program == "make" {
            checks += 1;
            // The read-only token, as git's extraheader (base64 of x-access-token:…).
            assert!(
                secrets.iter().any(|s| s.contains(CI_TOKEN_VALUE)),
                "{}",
                cmd.display()
            );
        }
        if !cmd.trusted {
            assert!(!service, "{}", cmd.display());
        }
    }
    assert!(checks > 0);
}

#[tokio::test]
async fn without_a_ci_token_checks_get_none() {
    let h = harness(exec("src/lib.rs\n"), false);
    TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    for cmd in h.exec.cmds.lock().iter().filter(|c| c.program == "make") {
        assert!(cmd.secrets.is_empty(), "{}", cmd.display());
    }
}

#[tokio::test]
async fn jobs_follow_the_change_filters() {
    let h = harness(exec("gears/x/Cargo.toml\n"), false);
    let state = TestRuntime::new()
        .run(&h.workflow, input())
        .await
        .unwrap()
        .into_output()
        .unwrap();
    assert_eq!(
        conclusions(&state),
        [
            ("fmt", Conclusion::Skipped),
            ("repo_checks", Conclusion::Skipped),
            ("security", Conclusion::Success),
            ("clippy", Conclusion::Skipped),
            ("lint", Conclusion::Success),
            ("test", Conclusion::Skipped),
            ("integration", Conclusion::Skipped),
        ]
    );
    assert_eq!(
        state.skipped,
        ["fmt", "repo_checks", "clippy", "test", "integration"]
    );
    assert_eq!(h.exec.calls_matching("make clippy"), 0);
    assert_eq!(h.exec.calls_matching("make deny"), 1);
}

#[tokio::test]
async fn docs_only_changes_run_nothing() {
    let h = harness(exec("docs/a.md\nREADME.md\n"), false);
    let state = TestRuntime::new()
        .run(&h.workflow, input())
        .await
        .unwrap()
        .into_output()
        .unwrap();
    assert_eq!(state.skipped.len(), JOBS.len());
    assert_eq!(h.exec.calls_matching("make "), 0);
}

#[tokio::test]
async fn a_rerun_runs_only_the_selected_jobs() {
    let h = harness(exec("docs/a.md\n"), false);
    let state = TestRuntime::new()
        .run(
            &h.workflow,
            CiInput {
                pr: 42,
                jobs: Some(vec!["clippy".into()]),
            },
        )
        .await
        .unwrap()
        .into_output()
        .unwrap();
    // Selected jobs run even when today's filters would skip them.
    let ran: Vec<&str> = conclusions(&state)
        .into_iter()
        .filter(|(_, c)| *c != Conclusion::Skipped)
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ran, ["clippy"]);
    let other = state.jobs.iter().find(|j| j.id == "test").unwrap();
    assert_eq!(
        other.reason.as_deref(),
        Some("not selected for this re-run")
    );
}

#[tokio::test]
async fn a_stale_merge_commit_is_waited_for() {
    let exec = exec("src/lib.rs\n")
        .on("^2", |_, n| {
            ok(if n == 1 {
                "0bad000000000000000000000000000000000000\n"
            } else {
                HEAD
            })
        })
        .reversed();
    let h = harness(exec, false);
    let run = TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    assert_eq!(run.attempts("changes"), 2);
    assert!(matches!(run.outcome(), TestOutcome::Succeeded(_)));
}

#[tokio::test]
async fn a_conflicting_pr_fails_at_once() {
    let exec = exec("src/lib.rs\n")
        .on("gh pr view 42", |_, _| ok(&pr_view("CONFLICTING")))
        .reversed();
    let h = harness(exec, false);
    let run = TestRuntime::new().run(&h.workflow, input()).await.unwrap();
    assert_eq!(
        run.outcome(),
        &TestOutcome::Failed {
            step: "changes".into(),
            code: "merge_conflict".into()
        }
    );
    assert_eq!(run.attempts("changes"), 1);
}

#[tokio::test]
async fn workflow_files_that_do_not_parse_fall_back_to_the_builtin_rules() {
    let exec = exec("src/lib.rs\n")
        .on(".github/workflows/ci.yml", |_, _| ok("jobs: [broken"))
        .reversed();
    let h = harness(exec, false);
    let state = TestRuntime::new()
        .run(&h.workflow, input())
        .await
        .unwrap()
        .into_output()
        .unwrap();
    assert_eq!(state.rules.as_deref(), Some("built-in"));
    assert!(state.changes.unwrap().filters["rust"]);
}

#[test]
fn git_rewrites_and_the_ci_token_share_one_git_config_list() {
    let mut config: PublisherConfig = serde_json::from_value(serde_json::json!({
        "work_dir": "/w",
        "target_dir": "/w/target",
        "queue_dsn": "postgres://unused",
        "firebase_project_id": "test",
        "ci_git_rewrites": { "https://github.com/o/dep": "https://github.com/me/dep" },
    }))
    .unwrap();
    let token = secrecy::SecretString::from(CI_TOKEN_VALUE.to_owned());
    let cmd = super::check_command(&config, "make dylint", false, Some(&token)).unwrap();
    let env: std::collections::HashMap<_, _> = cmd.env.iter().cloned().collect();
    assert_eq!(env["GIT_CONFIG_COUNT"], "2");
    assert_eq!(
        env["GIT_CONFIG_KEY_0"],
        "url.https://github.com/me/dep.insteadOf"
    );
    assert_eq!(env["GIT_CONFIG_VALUE_0"], "https://github.com/o/dep");
    assert_eq!(
        env["GIT_CONFIG_KEY_1"],
        "http.https://github.com/.extraheader"
    );
    let (key, value) = &cmd.secrets[0];
    assert_eq!(key, "GIT_CONFIG_VALUE_1");
    assert!(revealed(value.expose_secret()).contains(CI_TOKEN_VALUE));

    // Neither: no git settings at all.
    config.ci_git_rewrites.clear();
    let cmd = super::check_command(&config, "make dylint", false, None).unwrap();
    assert!(!cmd.env.iter().any(|(k, _)| k.starts_with("GIT_CONFIG")));
}
