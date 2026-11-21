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
    /// What of the output reaches the run log.
    pub logged: Logged,
    /// Substrings to look for in every line; found ones end up in [`CmdOutput::seen`].
    pub watch: Vec<&'static str>,
    /// Runs as the service user even when a build user is configured: only for tools
    /// that execute none of the repository's code and need the service's credentials (`gh`).
    pub trusted: bool,
}

/// What of a command's output is logged. Masking, `tail`, `watch` and `capture` see
/// every line regardless.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Logged {
    #[default]
    All,
    /// Stderr only: stdout carries a secret (`gh auth token`).
    Stderr,
    /// Errors and warnings with some context (see [`Condenser`]): for tools whose full
    /// output is too large to read (`cargo deny`).
    Condensed,
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
            logged: Logged::All,
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
        self.logged = Logged::Stderr;
        self
    }
    #[must_use]
    pub fn trusted(mut self) -> Self {
        self.trusted = true;
        self
    }
    #[must_use]
    /// No effect after [`Cmd::quiet`]: its stdout must not be logged at all.
    pub fn condensed(mut self) -> Self {
        if self.logged == Logged::All {
            self.logged = Logged::Condensed;
        }
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

/// Lines of context kept before a problem.
const CONDENSE_BEFORE: usize = 2;
/// Lines of one diagnostic kept (cargo-deny's inclusion trees run to hundreds).
const CONDENSE_BLOCK: usize = 40;
/// Lines kept from the end of the output after the last diagnostic (the summary,
/// `make: ***`).
const CONDENSE_TAIL: usize = 15;

/// Reduces output to its errors and warnings: each diagnostic (up to a blank line or
/// [`CONDENSE_BLOCK`] lines) with [`CONDENSE_BEFORE`] lines before it, plus up to
/// [`CONDENSE_TAIL`] lines of the end that follow the last one; gaps become one
/// `… N lines skipped` note.
#[derive(Default)]
pub struct Condenser {
    /// Recent lines not logged, oldest first.
    held: VecDeque<(&'static str, String)>,
    /// Lines not logged since the last logged one (including `held`).
    skipped: usize,
    block: Block,
}

#[derive(Default, Clone, Copy)]
enum Block {
    /// Outside a diagnostic, or past [`CONDENSE_BLOCK`] lines of one: lines are held.
    #[default]
    None,
    /// Inside a diagnostic, this many lines logged.
    Open(usize),
}

impl Condenser {
    /// Lines to log for one line of output.
    pub fn push(&mut self, stream: &'static str, line: String) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if is_problem(&line) {
            let mut before = self.held.len().min(CONDENSE_BEFORE);
            // One more line costs no more than a note saying it was skipped.
            if self.skipped == before + 1 && self.held.len() > before {
                before += 1;
            }
            self.flush_skipped(before, &mut out);
            let keep = self.held.len() - before;
            out.extend(self.held.drain(..).skip(keep));
            out.push((stream, line));
            self.block = Block::Open(1);
            return out;
        }
        let blank = line.trim().is_empty();
        match self.block {
            Block::Open(n) if blank || n < CONDENSE_BLOCK => {
                self.block = if blank {
                    Block::None
                } else {
                    Block::Open(n + 1)
                };
                out.push((stream, line));
                return out;
            }
            Block::Open(_) => self.block = Block::None,
            Block::None => {}
        }
        self.hold(stream, line);
        out
    }

    /// Lines to log at the end of the output.
    #[must_use]
    pub fn finish(mut self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let tail = self.held.len();
        self.flush_skipped(tail, &mut out);
        out.extend(self.held.drain(..));
        out
    }

    fn hold(&mut self, stream: &'static str, line: String) {
        if self.held.len() == CONDENSE_TAIL {
            self.held.pop_front();
        }
        self.held.push_back((stream, line));
        self.skipped += 1;
    }

    /// Notes the lines skipped, except the `shown` ones about to be logged.
    fn flush_skipped(&mut self, shown: usize, out: &mut Vec<(&'static str, String)>) {
        let n = self.skipped - shown;
        if n > 0 {
            let s = if n == 1 { "" } else { "s" };
            out.push(("info", format!("… {n} line{s} skipped")));
        }
        self.skipped = 0;
    }
}

/// `error: …`, `warning[duplicate]: …`, a logged `ERROR`/`WARN`, `make: *** …`.
fn is_problem(line: &str) -> bool {
    let diagnostic = ["error", "warning"].iter().any(|level| {
        line.strip_prefix(level).is_some_and(|rest| {
            let rest = match rest.strip_prefix('[') {
                Some(code) => code.split_once(']').map_or("", |(_, rest)| rest),
                None => rest,
            };
            rest.starts_with(": ")
        })
    });
    diagnostic
        || ["[ERROR]", "[WARN]", " ERROR ", " WARN "]
            .iter()
            .any(|m| line.contains(m))
        || (line.starts_with("make") && line.contains(": ***"))
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
