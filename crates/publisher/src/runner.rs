//! Subprocesses with line-by-line logging, secret masking and cancellation.
//!
//! Every command starts from an empty environment: only a small allowlist, the
//! configured `env` and the command's own variables reach it, so the service's
//! `DATABASE_URL` and anything else in its environment never does. Commands that run
//! code from the released repository (everything except `gh`) can run as a separate
//! build user through `sudo`; that user cannot read the service's secrets.
use crate::store::{Journal, NewLine};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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

/// Where a command's output goes: the run, step and attempt it belongs to.
#[derive(Clone)]
pub struct StepScope {
    pub run_id: Uuid,
    pub step: String,
    pub attempt: u32,
    pub cancel: CancellationToken,
    pub journal: Arc<dyn Journal>,
}

impl StepScope {
    /// A note from the publisher itself.
    pub async fn info(&self, line: impl Into<String>) {
        self.journal
            .log(NewLine {
                run_id: self.run_id,
                step: self.step.clone(),
                attempt: self.attempt,
                stream: "info",
                line: line.into(),
            })
            .await;
    }
}

/// A command to run.
#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Passed as environment and masked in every logged line.
    pub secrets: Vec<(String, SecretString)>,
    /// Keep stdout in [`CmdOutput::stdout`].
    pub capture: bool,
    /// Do not log stdout (it carries a secret, e.g. `gh auth token`).
    pub quiet_stdout: bool,
    /// Substrings to look for in every line; found ones end up in [`CmdOutput::seen`].
    pub watch: Vec<&'static str>,
    /// Runs as the service user even when a build user is configured: only for tools
    /// that execute none of the repository's code and need the service's credentials (`gh`).
    pub trusted: bool,
}

impl Cmd {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            secrets: Vec::new(),
            capture: false,
            quiet_stdout: false,
            watch: Vec::new(),
            trusted: false,
        }
    }
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
    #[must_use]
    pub fn secret(mut self, key: impl Into<String>, value: SecretString) -> Self {
        self.secrets.push((key.into(), value));
        self
    }
    #[must_use]
    pub fn capture(mut self) -> Self {
        self.capture = true;
        self
    }
    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.capture = true;
        self.quiet_stdout = true;
        self
    }
    #[must_use]
    pub fn trusted(mut self) -> Self {
        self.trusted = true;
        self
    }
    #[must_use]
    pub fn watch(mut self, patterns: &[&'static str]) -> Self {
        self.watch.extend_from_slice(patterns);
        self
    }
    /// `program arg…` as logged.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A finished command.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    /// Exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    /// Captured stdout (only with [`Cmd::capture`]).
    pub stdout: String,
    /// The last lines of stdout and stderr, masked.
    pub tail: Vec<String>,
    /// The [`Cmd::watch`] patterns that occurred.
    pub seen: Vec<&'static str>,
}

impl CmdOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
    #[must_use]
    pub fn saw(&self, pattern: &str) -> bool {
        self.seen.contains(&pattern)
    }
    #[must_use]
    pub fn tail_text(&self) -> String {
        self.tail.join("\n")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("failed while running {program}: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

/// Runs commands. Faked in workflow tests.
#[async_trait]
pub trait Exec: Send + Sync + 'static {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError>;
}

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

    fn command(&self, cmd: &Cmd) -> tokio::process::Command {
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
        let secrets = cmd
            .secrets
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose_secret()));

        let mut command = match self.run_as(cmd) {
            None => {
                let mut c = tokio::process::Command::new(&cmd.program);
                c.args(&cmd.args);
                c
            }
            Some(run_as) => {
                // sudo resets the environment and sets PATH (secure_path) and HOME for
                // the build user; the names listed here are passed through.
                let mut names: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
                names.extend(cmd.secrets.iter().map(|(k, _)| k.as_str()));
                names.retain(|n| !INHERITED_ENV.contains(n));
                names.sort_unstable();
                names.dedup();
                let mut c = tokio::process::Command::new(&run_as.sudo);
                c.args(["-n", "-u", &run_as.user]);
                if !names.is_empty() {
                    c.arg(format!("--preserve-env={}", names.join(",")));
                }
                c.arg("--").arg(&cmd.program).args(&cmd.args);
                c
            }
        };
        command
            .current_dir(&cmd.cwd)
            .stdin(Stdio::null())
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
        command
    }
}

#[async_trait]
impl Exec for ProcessExec {
    async fn run(&self, scope: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
        let masker = Masker::new(cmd.secrets.iter().map(|(_, v)| v.expose_secret()));
        log(scope, "cmd", format!("$ {}", masker.mask(&cmd.display()))).await;

        let mut command = self.command(&cmd);
        let mut child = command.spawn().map_err(|source| ExecError::Spawn {
            program: cmd.program.clone(),
            source,
        })?;
        let pid = child.id();

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
        let consume = async {
            while let Some(chunk) = rx.recv().await {
                let (stream, text) = match chunk {
                    Chunk::Out(text) => {
                        if cmd.capture && output.stdout.len() < MAX_CAPTURE_BYTES {
                            output.stdout.push_str(&text);
                            output.stdout.push('\n');
                        }
                        if cmd.quiet_stdout {
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
                log(scope, stream, shown).await;
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
            if !drained
                && tokio::time::timeout(DRAIN_AFTER_EXIT, &mut consume)
                    .await
                    .is_err()
            {
                log(
                    scope,
                    "info",
                    "output still open after exit; stopped reading".to_owned(),
                )
                .await;
            }
            status
        };
        let status = tokio::select! {
            status = finish => status,
            () = scope.cancel.cancelled() => {
                terminate(pid, self.run_as(&cmd), &mut child).await;
                for task in [stdout, stderr].into_iter().flatten() {
                    task.abort();
                }
                log(scope, "info", "cancelled: process group terminated".to_owned()).await;
                return Err(ExecError::Cancelled);
            }
        };
        let status = status.map_err(|source| ExecError::Io {
            program: cmd.program.clone(),
            source,
        })?;
        // Finished, or stuck on a pipe a background process keeps open.
        for task in [stdout, stderr].into_iter().flatten() {
            task.abort();
        }
        output.code = status.code();
        output.tail = tail.into_iter().collect();
        let exit = output
            .code
            .map_or_else(|| "by signal".to_owned(), |c| c.to_string());
        log(scope, "info", format!("exit {exit}")).await;
        Ok(output)
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

/// Replaces secret values with `***`.
pub struct Masker {
    secrets: Vec<String>,
}

impl Masker {
    pub fn new<'a>(secrets: impl IntoIterator<Item = &'a str>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|s| s.len() >= 6)
            .map(str::to_owned)
            .collect();
        // Longest first, so a secret containing another is masked whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { secrets }
    }

    #[must_use]
    pub fn mask(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), "***");
            }
        }
        text
    }
}

/// Removes ANSI escape sequences (CSI and OSC).
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryJournal;

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
