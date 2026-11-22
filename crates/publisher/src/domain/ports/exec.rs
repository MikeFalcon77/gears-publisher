//! Commands the steps run, and the port that runs them.
use super::journal::Journal;
use crate::domain::models::NewLine;
use async_trait::async_trait;
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    /// Errors and warnings with some context (`infra::exec::Condenser`): for tools whose full
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
