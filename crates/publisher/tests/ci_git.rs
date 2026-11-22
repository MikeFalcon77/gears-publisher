//! PR CI against real git: a local repository plays GitHub (with `refs/pull/1/head`
//! and `refs/pull/1/merge`), a script plays `gh`, and `make` is a script that records
//! where it ran. Covers the mirror, the CI clone and the change detection end to end.
#![cfg(unix)]
use async_trait::async_trait;
use parking_lot::Mutex;
use publisher::domain::ci::{CiDeps, CiInput, Conclusion, definition};
use publisher::domain::models::NewLine;
use publisher::domain::ports::Journal;
use publisher::domain::steps::BuildSlot;
use publisher::infra::exec::ProcessExec;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use toolkit_durable_execution::testing::{TestOutcome, TestRuntime};
use uuid::Uuid;

#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

#[async_trait]
impl Journal for Lines {
    async fn log(&self, line: NewLine) {
        self.0.lock().push(format!("{} {}", line.step, line.line));
    }
    async fn facts(&self, _: Uuid, _: serde_json::Value) {}
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn script(path: &Path, body: &str) {
    write(path, &format!("#!/bin/sh\n{body}\n"));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const CI_YML: &str = "on:
  pull_request:
    paths-ignore: ['**/*.md']
jobs:
  changes:
    steps:
      - id: filter
        with:
          filters: |
            rust: ['**/*.rs']
            deps: ['**/Cargo.toml']
            ci: ['.github/workflows/**']
";

/// `origin` with `main`, a PR branch changing a Rust file, and GitHub's PR refs.
fn github(root: &Path) -> (String, String) {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "-b", "main"]);
    write(&origin.join(".github/workflows/ci.yml"), CI_YML);
    write(&origin.join("src/lib.rs"), "pub fn a() {}\n");
    write(&origin.join("README.md"), "hi\n");
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "--quiet", "-m", "main"]);
    git(&origin, &["checkout", "--quiet", "-b", "pr"]);
    write(&origin.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n");
    git(&origin, &["commit", "--quiet", "-am", "pr"]);
    let head = git(&origin, &["rev-parse", "HEAD"]);
    // main moves on after the PR branched off: the merge commit is a real merge.
    git(&origin, &["checkout", "--quiet", "main"]);
    write(&origin.join("README.md"), "hi again\n");
    git(&origin, &["commit", "--quiet", "-am", "main moves"]);
    git(&origin, &["checkout", "--quiet", "--detach", "main"]);
    git(
        &origin,
        &["merge", "--quiet", "--no-ff", "-m", "merge", "pr"],
    );
    let merge = git(&origin, &["rev-parse", "HEAD"]);
    git(&origin, &["checkout", "--quiet", "main"]);
    git(&origin, &["update-ref", "refs/pull/1/head", &head]);
    git(&origin, &["update-ref", "refs/pull/1/merge", &merge]);
    (head, merge)
}

/// Fake `gh` (answers for PR 1 with `head`), and `make`, `cargo` and `python3` that
/// append `tool args HEAD CARGO_TARGET_DIR` to `<root>/made.txt`.
fn deps(root: &Path, head: &str) -> (Arc<CiDeps>, Arc<Lines>) {
    let bin = root.join("bin");
    script(
        &bin.join("gh"),
        &format!(
            r#"case "$*" in
  "auth token") echo gho_local ;;
  "pr view 1 "*) echo '{{"number":1,"title":"Add b","url":"https://example.com/pull/1","state":"OPEN","mergeable":"MERGEABLE","author":{{"login":"dev"}},"headRefName":"pr","headRefOid":"{head}","baseRefName":"main","isCrossRepository":false}}' ;;
  *) echo "unexpected gh $*" >&2; exit 1 ;;
esac"#
        ),
    );
    let made = root.join("made.txt");
    for tool in ["make", "cargo", "python3"] {
        script(
            &bin.join(tool),
            &format!(
                "echo \"{tool} $* $(git rev-parse HEAD) $CARGO_TARGET_DIR\" >> {}",
                made.display()
            ),
        );
    }
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let work = root.join("work");
    let config = serde_json::from_value(serde_json::json!({
        "repo_url": root.join("origin").display().to_string(),
        "work_dir": work.display().to_string(),
        "target_dir": work.join("target").display().to_string(),
        "gh_bin": bin.join("gh").display().to_string(),
        "make_bin": bin.join("make").display().to_string(),
        "cargo_bin": bin.join("cargo").display().to_string(),
        "queue_dsn": "postgres://unused",
        "firebase_project_id": "test",
        "env": { "GIT_CONFIG_GLOBAL": "/dev/null" },
    }))
    .unwrap();
    let journal = Arc::new(Lines::default());
    let deps = Arc::new(CiDeps {
        config,
        exec: Arc::new(ProcessExec {
            base_env: HashMap::from([
                ("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned()),
                ("PATH".to_owned(), path),
            ]),
            run_as: None,
        }),
        journal: Arc::clone(&journal) as Arc<dyn Journal>,
        slot: Arc::new(BuildSlot::default()),
        clone: tokio::sync::Mutex::new(()),
    });
    (deps, journal)
}

#[tokio::test]
async fn a_pr_is_fetched_tested_on_its_merge_commit_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (head, merge) = github(root);
    let (deps, journal) = deps(root, &head);
    let made = root.join("made.txt");
    let work = root.join("work");
    let workflow = definition(&deps).unwrap();
    let input = CiInput { pr: 1, jobs: None };
    let run = TestRuntime::new()
        .run(&workflow, input.clone())
        .await
        .unwrap();
    let TestOutcome::Succeeded(state) = run.outcome() else {
        panic!("{:?}\n{}", run.outcome(), journal.0.lock().join("\n"));
    };
    assert_eq!(state.merge_sha.as_deref(), Some(merge.as_str()));
    assert_eq!(state.files, ["src/lib.rs"]);
    assert_eq!(state.rules.as_deref(), Some(".github/workflows/ci.yml"));
    assert_eq!(state.pr.as_ref().unwrap().head_sha, head);
    let ran: Vec<&str> = state
        .jobs
        .iter()
        .filter(|j| j.conclusion == Conclusion::Success)
        .map(|j| j.id.as_str())
        .collect();
    // No Cargo.toml changed: `security` is skipped, as ci.yml's `deps` filter says.
    assert_eq!(
        ran,
        [
            "fmt",
            "repo_checks",
            "clippy",
            "lint",
            "test",
            "integration"
        ]
    );
    let made = std::fs::read_to_string(&made).unwrap();
    let target = work.join("pr-ci/target").display().to_string();
    for line in made.lines() {
        assert!(line.contains(&format!(" {merge} {target}")), "{line}");
    }
    assert!(made.contains("make clippy "), "{made}");
    assert!(
        made.contains("cargo xtask check-test-container-pins "),
        "{made}"
    );
    assert!(
        made.contains("python3 tools/scripts/validate_gear_names.py "),
        "{made}"
    );
    assert!(!made.contains("make deny "), "{made}");
    // The mirror holds the PR refs; the CI clone has no remote credentials to reuse.
    let mirror = work.join("pr-mirror.git");
    assert_eq!(git(&mirror, &["rev-parse", "refs/pull/1/merge"]), merge);
    let clone = work.join("pr-ci/gears-rust");
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), merge);
    assert_eq!(
        git(&clone, &["config", "--get", "core.hooksPath"]),
        "/dev/null"
    );

    // A second run reuses the mirror and the clone.
    let again = TestRuntime::new().run(&workflow, input).await.unwrap();
    assert!(
        matches!(again.outcome(), TestOutcome::Succeeded(_)),
        "{:?}",
        again.outcome()
    );
}
