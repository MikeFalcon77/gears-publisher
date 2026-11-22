//! Subprocesses with line-by-line logging, secret masking and cancellation.
//!
//! Every command starts from an empty environment: only a small allowlist, the
//! configured `env` and the command's own variables reach it, so the service's
//! `DATABASE_URL` and anything else in its environment never does. Commands that run
//! code from the released repository (everything except `gh`) can run as a separate
//! build user through `sudo`; that user cannot read the service's secrets.
mod condense;
mod text;

pub use condense::Condenser;
pub use text::{Masker, strip_ansi};

use crate::domain::models::NewLine;
use crate::domain::ports::{Cmd, CmdOutput, Exec, ExecError, Logged, StepScope};
use async_trait::async_trait;
use secrecy::ExposeSecret;
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

/// Lines of combined output kept for error classification.
const TAIL_LINES: usize = 400;
/// How long output is still read after the command exited.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(5);
/// Grace period between SIGTERM and SIGKILL on cancellation.
const KILL_GRACE: Duration = Duration::from_secs(10);
/// Longer lines are split.
const MAX_LINE_BYTES: u64 = 64 * 1024;
/// Captured stdout beyond this is dropped (release-plz's JSON is far smaller).
const MAX_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
/// Variables of the service's own environment that commands may see.
const INHERITED_ENV: [&str; 7] = ["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR", "TZ"];
/// Run as the build user, exports the `NAME=value` lines of stdin, then runs the
/// command (`"$@"`) with stdin closed. sudo logs the variables it passes, values
/// included, to the journal and auth.log; secrets therefore never go through sudo.
const EXPORT_STDIN: &str =
    r#"while IFS= read -r kv; do export "$kv" || exit 126; done; exec "$@" </dev/null"#;

/// Real subprocesses.
pub struct ProcessExec {
    /// Environment added to every command.
    pub base_env: HashMap<String, String>,
    /// Build user for untrusted commands (`sudo -u`); `None` runs everything as the service.
    pub run_as: Option<RunAs>,
}

/// `sudo -n -u <user>`: needs a sudoers rule with `NOPASSWD:SETENV` and `!use_pty`, so
/// that the command stays in sudo's process group (see `deploy/sudoers`).
#[derive(Debug, Clone)]
pub struct RunAs {
    pub user: String,
    pub sudo: String,
    /// `TMPDIR` of the user's commands. Under the service's `PrivateTmp` both build
    /// users share one `/tmp`: a fixed path there made by one user is not writable by
    /// the other, and PR code could plant files the release then uses.
    pub tmp_dir: Option<String>,
}

enum Chunk {
    Out(String),
    Err(String),
}

async fn pump<R: AsyncRead + Unpin>(
    reader: R,
    tx: mpsc::Sender<Chunk>,
    wrap: fn(String) -> Chunk,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut buf)
            .await?
            == 0
        {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&buf);
        // Progress output rewrites one line with `\r`; keep its last state.
        let text = text.trim_end_matches(['\n', '\r']);
        let text = text.rsplit('\r').next().unwrap_or_default();
        if tx.send(wrap(strip_ansi(text))).await.is_err() {
            return Ok(());
        }
    }
}

impl ProcessExec {
    fn run_as(&self, cmd: &Cmd) -> Option<&RunAs> {
        self.run_as.as_ref().filter(|_| !cmd.trusted)
    }

    /// The process to spawn, and for a build-user command with secrets the
    /// `NAME=value` lines to write to its stdin (see [`EXPORT_STDIN`]).
    fn command(&self, cmd: &Cmd) -> (tokio::process::Command, Option<String>) {
        let mut vars: Vec<(String, String)> = [
            ("CARGO_TERM_COLOR", "never"),
            ("NO_COLOR", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GH_PROMPT_DISABLED", "1"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        vars.extend(self.base_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        vars.extend(cmd.env.iter().cloned());
        let mut secrets: Vec<(&str, &str)> = Vec::new();
        let mut stdin = None;
        let mut command = match self.run_as(cmd) {
            None => {
                secrets.extend(
                    cmd.secrets
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.expose_secret())),
                );
                let mut c = tokio::process::Command::new(&cmd.program);
                c.args(&cmd.args);
                c
            }
            Some(run_as) => {
                // sudo resets the environment and sets PATH (secure_path) and HOME for
                // the build user; the names listed here are passed through.
                if let Some(tmp) = &run_as.tmp_dir {
                    vars.push(("TMPDIR".to_owned(), tmp.clone()));
                }
                let mut names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
                names.retain(|n| !INHERITED_ENV.contains(n));
                if run_as.tmp_dir.is_some() {
                    names.push("TMPDIR");
                }
                names.sort_unstable();
                names.dedup();
                let mut c = tokio::process::Command::new(&run_as.sudo);
                c.args(["-n", "-u", &run_as.user]);
                if !names.is_empty() {
                    c.arg(format!("--preserve-env={}", names.join(",")));
                }
                c.arg("--");
                if !cmd.secrets.is_empty() {
                    // A value with a newline would split into two variables; tokens
                    // have none, so such a value is dropped rather than mangled.
                    let mut lines = String::new();
                    for (k, v) in &cmd.secrets {
                        if !v.expose_secret().contains('\n') {
                            for part in [k.as_str(), "=", v.expose_secret(), "\n"] {
                                lines.push_str(part);
                            }
                        }
                    }
                    stdin = Some(lines);
                    c.args(["/bin/sh", "-c", EXPORT_STDIN, "sh"]);
                }
                c.arg(&cmd.program).args(&cmd.args);
                c
            }
        };
        command
            .current_dir(&cmd.cwd)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear()
            .envs(
                INHERITED_ENV
                    .iter()
                    .filter_map(|k| Some((*k, std::env::var(k).ok()?))),
            )
            .envs(vars)
            .envs(secrets);
        #[cfg(unix)]
        command.process_group(0);
        (command, stdin)
    }
}

#[async_trait]
impl Exec for ProcessExec {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
        let masker = Masker::new(cmd.secrets.iter().map(|(_, v)| v.expose_secret()));
        log(scope, "cmd", format!("$ {}", masker.mask(&cmd.display()))).await;

        let (mut command, secrets) = self.command(&cmd);
        let mut child = command.spawn().map_err(|source| ExecError::Spawn {
            program: cmd.program.clone(),
            source,
        })?;
        let pid = child.id();
        feed(&mut child, secrets);

        let (tx, mut rx) = mpsc::channel::<Chunk>(1024);
        let stdout = child.stdout.take().map(|out| {
            let tx = tx.clone();
            tokio::spawn(pump(out, tx, Chunk::Out))
        });
        let stderr = child.stderr.take().map(|err| {
            let tx = tx.clone();
            tokio::spawn(pump(err, tx, Chunk::Err))
        });
        drop(tx);

        let mut output = CmdOutput::default();
        let mut tail = VecDeque::with_capacity(TAIL_LINES);
        let mut logger = OutputLog::start(scope, &cmd, &masker).await;
        let consume = async {
            while let Some(chunk) = rx.recv().await {
                let (stream, text) = match chunk {
                    Chunk::Out(text) => {
                        if cmd.capture && output.stdout.len() < MAX_CAPTURE_BYTES {
                            output.stdout.push_str(&text);
                            output.stdout.push('\n');
                        }
                        if cmd.logged == Logged::Stderr {
                            continue;
                        }
                        ("stdout", text)
                    }
                    Chunk::Err(text) => ("stderr", text),
                };
                for pattern in &cmd.watch {
                    if !output.seen.contains(pattern) && text.contains(pattern) {
                        output.seen.push(pattern);
                    }
                }
                let shown = masker.mask(&text);
                if tail.len() == TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(shown.clone());
                logger.line(stream, shown).await;
            }
        };

        // Output is read until EOF, but a background process that inherited stdout must
        // not hold the step after the command itself has exited.
        let finish = async {
            let mut consume = std::pin::pin!(consume);
            let mut drained = false;
            let status = tokio::select! {
                status = child.wait() => status,
                () = &mut consume => {
                    drained = true;
                    child.wait().await
                }
            };
            let stuck = !drained
                && tokio::time::timeout(DRAIN_AFTER_EXIT, &mut consume)
                    .await
                    .is_err();
            (status, stuck)
        };
        // `finish` borrows the logger; it is dropped before the logger is flushed.
        let finished = tokio::select! {
            finished = finish => Some(finished),
            () = scope.cancel.cancelled() => None,
        };
        let Some((status, stuck)) = finished else {
            terminate(pid, self.run_as(&cmd), &mut child).await;
            stop(stdout, stderr);
            logger.finish().await;
            log(
                scope,
                "info",
                "cancelled: process group terminated".to_owned(),
            )
            .await;
            return Err(ExecError::Cancelled);
        };
        // Finished, or stuck on a pipe a background process keeps open.
        stop(stdout, stderr);
        logger.finish().await;
        if stuck {
            log(scope, "info", STUCK.to_owned()).await;
        }
        let status = status.map_err(|source| ExecError::Io {
            program: cmd.program.clone(),
            source,
        })?;
        output.code = status.code();
        output.tail = tail.into_iter().collect();
        let exit = output
            .code
            .map_or_else(|| "by signal".to_owned(), |c| c.to_string());
        log(scope, "info", format!("exit {exit}")).await;
        Ok(output)
    }
}

/// Runs only `trusted` commands; the rest fail to start. For PR CI when it has no
/// build user of its own: a recovered run must not run a PR's code as the service.
pub struct RefuseUntrusted(pub ProcessExec);

#[async_trait]
impl Exec for RefuseUntrusted {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
        if cmd.trusted {
            return self.0.run(scope, cmd).await;
        }
        log(
            scope,
            "info",
            format!(
                "refused `{}`: PR CI has no build user of its own",
                cmd.program
            ),
        )
        .await;
        Err(ExecError::Spawn {
            program: cmd.program,
            source: std::io::Error::other("PR CI is disabled"),
        })
    }
}

/// Writes the secrets to the command's stdin (see [`EXPORT_STDIN`]). A command that
/// exits before reading them gets them no more.
fn feed(child: &mut tokio::process::Child, secrets: Option<String>) {
    if let (Some(secrets), Some(mut stdin)) = (secrets, child.stdin.take()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let _ = stdin.write_all(secrets.as_bytes()).await;
        });
    }
}

const STUCK: &str = "output still open after exit; stopped reading";

/// Stops reading the pipes.
fn stop(stdout: Option<Pump>, stderr: Option<Pump>) {
    for task in [stdout, stderr].into_iter().flatten() {
        task.abort();
    }
}

type Pump = tokio::task::JoinHandle<std::io::Result<()>>;

/// Writes output lines to the run log as [`Cmd::logged`] says.
struct OutputLog<'a> {
    scope: &'a StepScope,
    condenser: Option<Condenser>,
}

impl<'a> OutputLog<'a> {
    async fn start(scope: &'a StepScope, cmd: &Cmd, masker: &Masker) -> Self {
        let condensed = cmd.logged == Logged::Condensed;
        if condensed {
            let note = format!(
                "output condensed to errors and warnings; run `{}` locally for all of it",
                masker.mask(&cmd.display())
            );
            log(scope, "info", note).await;
        }
        Self {
            scope,
            condenser: condensed.then(Condenser::default),
        }
    }

    async fn line(&mut self, stream: &'static str, line: String) {
        match self.condenser.as_mut() {
            Some(condenser) => {
                for (stream, line) in condenser.push(stream, line) {
                    log(self.scope, stream, line).await;
                }
            }
            None => log(self.scope, stream, line).await,
        }
    }

    async fn finish(self) {
        for (stream, line) in self.condenser.map(Condenser::finish).unwrap_or_default() {
            log(self.scope, stream, line).await;
        }
    }
}

async fn log(scope: &StepScope, stream: &'static str, line: String) {
    scope
        .journal
        .log(NewLine {
            run_id: scope.run_id,
            step: scope.step.clone(),
            attempt: scope.attempt,
            stream,
            line,
        })
        .await;
}

/// SIGTERM the whole process group (make → cargo → rustc), then SIGKILL. Processes of
/// the build user are signalled as that user; sudo itself as the service.
async fn terminate(pid: Option<u32>, run_as: Option<&RunAs>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        let group = format!("-{pid}");
        let signal_group = async |signal: &str| {
            if let Some(run_as) = run_as {
                let _ = tokio::process::Command::new(&run_as.sudo)
                    .args(["-n", "-u", &run_as.user, "kill", signal, "--", &group])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
            let _ = tokio::process::Command::new("kill")
                .args([signal, "--", &group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        };
        signal_group("-TERM").await;
        if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_ok() {
            return;
        }
        signal_group("-KILL").await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::condense::{CONDENSE_BLOCK, CONDENSE_TAIL, is_problem};
    use super::*;
    use crate::domain::ports::journal::MemoryJournal;
    use secrecy::SecretString;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    fn scope(journal: Arc<MemoryJournal>) -> StepScope {
        StepScope {
            run_id: Uuid::nil(),
            step: "test".into(),
            attempt: 1,
            cancel: CancellationToken::new(),
            journal,
        }
    }

    #[test]
    fn ansi_sequences_are_removed() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[92m   Compiling\u{1b}[0m x"),
            "   Compiling x"
        );
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{7}link"), "link");
    }

    #[test]
    fn secrets_are_masked() {
        let masker = Masker::new(["cio_secret_token", "short"]);
        assert_eq!(
            masker.mask("token=cio_secret_token short"),
            "token=*** short"
        );
    }

    #[tokio::test]
    async fn output_is_logged_masked_and_captured() {
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: None,
        };
        let out = exec
            .run(
                &scope(journal.clone()),
                Cmd::new("sh", ".")
                    .args(["-c", "echo out $TOKEN; echo err >&2; exit 3"])
                    .secret("TOKEN", SecretString::from("abcdef123"))
                    .capture(),
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "out abcdef123\n");
        let lines = journal.lines.lock();
        assert!(
            lines
                .iter()
                .any(|l| l.stream == "stdout" && l.line == "out ***")
        );
        assert!(
            lines
                .iter()
                .any(|l| l.stream == "stderr" && l.line == "err")
        );
        assert!(!lines.iter().any(|l| l.line.contains("abcdef123")));
    }

    #[tokio::test]
    async fn the_service_environment_is_not_inherited() {
        // Cargo sets it for the test process.
        assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::from([("BASE".to_owned(), "base".to_owned())]),
            run_as: None,
        };
        let out = exec
            .run(
                &scope(journal),
                Cmd::new("sh", ".")
                    .args(["-c", "echo \"${CARGO_MANIFEST_DIR:-none} $BASE $OWN\""])
                    .env("OWN", "own")
                    .capture(),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "none base own\n");
    }

    #[tokio::test]
    async fn secrets_reach_the_build_user_without_passing_through_sudo() {
        // A stand-in for sudo: records what it gets, as sudo's log would, then runs
        // the command after `--` like sudo.
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("sudo-saw");
        let sudo = dir.path().join("sudo");
        std::fs::write(
            &sudo,
            format!(
                "#!/bin/sh\n{{ printf '%s\\n' \"$@\"; env; }} > '{}'\n\
                 while [ \"$1\" != -- ]; do shift; done; shift\nexec \"$@\"\n",
                record.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: Some(RunAs {
                user: "build".into(),
                sudo: sudo.display().to_string(),
                tmp_dir: Some("/build/tmp".into()),
            }),
        };
        let out = exec
            .run(
                &scope(Arc::new(MemoryJournal::default())),
                Cmd::new("sh", ".")
                    .args(["-c", "echo \"$TOKEN|$HEADER|$PLAIN|$TMPDIR\"; cat"])
                    .env("PLAIN", "plain")
                    .secret("TOKEN", SecretString::from("abcdef123"))
                    .secret("HEADER", SecretString::from("AUTHORIZATION: basic xyz789"))
                    .capture(),
            )
            .await
            .unwrap();
        // The command gets the secrets, and its stdin is empty as before.
        assert_eq!(out.code, Some(0));
        assert_eq!(
            out.stdout,
            "abcdef123|AUTHORIZATION: basic xyz789|plain|/build/tmp\n"
        );
        let saw = std::fs::read_to_string(&record).unwrap();
        assert!(saw.contains("--preserve-env="), "{saw}");
        // sudo resets the environment: TMPDIR is passed on explicitly.
        assert!(
            saw.lines()
                .any(|l| l.starts_with("--preserve-env=") && l.contains("TMPDIR")),
            "{saw}"
        );
        assert!(saw.contains("PLAIN=plain"), "{saw}");
        assert!(!saw.contains("abcdef123"), "{saw}");
        assert!(!saw.contains("xyz789"), "{saw}");
        assert!(!saw.contains("TOKEN="), "{saw}");
    }

    fn condense(input: &[&str]) -> Vec<String> {
        let mut condenser = Condenser::default();
        let mut out = Vec::new();
        for line in input {
            out.extend(condenser.push("stderr", (*line).to_owned()));
        }
        out.extend(condenser.finish());
        out.into_iter().map(|(_, line)| line).collect()
    }

    fn numbered(prefix: &str, n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("{prefix}{i}")).collect()
    }

    #[test]
    fn problems_are_recognised() {
        for line in [
            "error: failed to load advisory database",
            "error[banned]: crate 'openssl' is explicitly banned",
            "warning[duplicate]: found 2 duplicate entries for crate 'syn'",
            "2026-11-21 10:00:00 [WARN] unable to fetch",
            "make: *** [Makefile:12: deny] Error 4",
            "make[1]: *** [deny] Error 1",
        ] {
            assert!(is_problem(line), "{line}");
        }
        for line in [
            "   ├── errors v1.0.0",
            "warnings ok",
            "error-chain v0.12.4",
            "   │ error: nested",
            "advisories ok, bans FAILED",
        ] {
            assert!(!is_problem(line), "{line}");
        }
    }

    #[test]
    fn diagnostics_are_kept_with_context_and_gaps_noted() {
        let mut input = numbered("noise", 10);
        input.extend(
            [
                "warning[duplicate]: syn",
                "  ┌─ Cargo.lock:1:1",
                "  ├ syn v1",
                "",
            ]
            .map(String::from),
        );
        input.extend(numbered("tree", 30));
        input.push("error[banned]: openssl".into());
        input.push("  └── native-tls".into());
        input.push(String::new());
        input.extend(numbered("more", 20));
        input.push("bans FAILED".into());
        let input: Vec<&str> = input.iter().map(String::as_str).collect();
        let mut expected = vec!["… 8 lines skipped", "noise9", "noise10"];
        expected.extend([
            "warning[duplicate]: syn",
            "  ┌─ Cargo.lock:1:1",
            "  ├ syn v1",
            "",
        ]);
        expected.extend(["… 28 lines skipped", "tree29", "tree30"]);
        expected.extend(["error[banned]: openssl", "  └── native-tls", ""]);
        expected.push("… 6 lines skipped");
        let more = numbered("more", 20);
        expected.extend(more[6..].iter().map(String::as_str));
        expected.push("bans FAILED");
        assert_eq!(condense(&input), expected);
    }

    #[test]
    fn a_gap_of_one_line_is_shown_not_noted() {
        assert_eq!(
            condense(&["a", "b", "c", "error: x", "", "after"]),
            ["a", "b", "c", "error: x", "", "after"]
        );
        let out = condense(&["a", "b", "c", "d", "error: x"]);
        assert_eq!(out, ["… 2 lines skipped", "c", "d", "error: x"]);
    }

    #[test]
    fn a_quiet_command_stays_quiet_when_condensed() {
        assert_eq!(
            Cmd::new("gh", ".").quiet().condensed().logged,
            Logged::Stderr
        );
    }

    #[test]
    fn a_long_diagnostic_is_cut() {
        let mut input = vec!["warning[duplicate]: syn".to_owned()];
        input.extend(numbered("tree", 100));
        input.push(String::new());
        input.extend(numbered("after", 3));
        let input: Vec<&str> = input.iter().map(String::as_str).collect();
        let out = condense(&input);
        assert_eq!(out.len(), CONDENSE_BLOCK + 1 + CONDENSE_TAIL);
        assert_eq!(
            out[CONDENSE_BLOCK - 1],
            format!("tree{}", CONDENSE_BLOCK - 1)
        );
        // The rest of the tree and its blank line, minus the tail shown.
        assert_eq!(
            out[CONDENSE_BLOCK],
            format!(
                "… {} lines skipped",
                100 - (CONDENSE_BLOCK - 1) + 1 - (CONDENSE_TAIL - 3)
            )
        );
        assert_eq!(out.last().unwrap(), "after3");
    }

    #[test]
    fn output_without_problems_keeps_its_tail() {
        let input = numbered("line", 100);
        let input: Vec<&str> = input.iter().map(String::as_str).collect();
        let out = condense(&input);
        assert_eq!(out[0], format!("… {} lines skipped", 100 - CONDENSE_TAIL));
        assert_eq!(out[1..], input[100 - CONDENSE_TAIL..]);
        assert_eq!(condense(&["a", "b"]), ["a", "b"]);
    }

    #[tokio::test]
    async fn condensed_output_is_logged_in_part_but_seen_whole() {
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: None,
        };
        let out = exec
            .run(
                &scope(journal.clone()),
                Cmd::new("sh", ".")
                    .args([
                        "-c",
                        "seq 1 100; echo 'error[banned]: openssl'; echo; seq 1 100; exit 4",
                    ])
                    .watch(&["50"])
                    .condensed(),
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(4));
        assert!(out.saw("50"));
        let lines = journal.lines.lock();
        let text: Vec<&str> = lines.iter().map(|l| l.line.as_str()).collect();
        assert!(text[1].starts_with("output condensed"), "{text:?}");
        assert!(text.contains(&"error[banned]: openssl"));
        assert!(text.contains(&"… 98 lines skipped"));
        assert_eq!(text.last(), Some(&"exit 4"));
        assert!(text.len() < 30, "{text:?}");
    }

    #[tokio::test]
    async fn quiet_stdout_is_not_logged() {
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: None,
        };
        let out = exec
            .run(
                &scope(journal.clone()),
                Cmd::new("sh", ".").args(["-c", "echo hidden"]).quiet(),
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "hidden\n");
        assert!(!journal.lines.lock().iter().any(|l| l.line == "hidden"));
    }

    #[tokio::test]
    async fn a_background_process_holding_stdout_does_not_hang_the_step() {
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: None,
        };
        let started = std::time::Instant::now();
        let out = exec
            .run(
                &scope(journal),
                Cmd::new("sh", ".").args(["-c", "echo before; (sleep 30 &); exit 0"]),
            )
            .await
            .unwrap();
        assert_eq!(out.code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(15));
    }

    #[tokio::test]
    async fn cancellation_kills_the_process_group() {
        let journal = Arc::new(MemoryJournal::default());
        let exec = ProcessExec {
            base_env: HashMap::new(),
            run_as: None,
        };
        let scope = scope(journal);
        let cancel = scope.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let result = exec
            .run(
                &scope,
                Cmd::new("sh", ".").args(["-c", "sleep 30 & sleep 30"]),
            )
            .await;
        assert!(matches!(result, Err(ExecError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
