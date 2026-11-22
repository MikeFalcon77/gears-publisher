//! Fakes for workflow tests.
use crate::domain::ports::{Cmd, CmdOutput, Exec, ExecError, StepScope};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;

type Handler = Box<dyn Fn(&Cmd, usize) -> CmdOutput + Send + Sync>;

/// Answers commands by the first rule whose pattern occurs in `program args…`.
/// Unmatched commands succeed with no output.
#[derive(Default)]
pub struct FakeExec {
    pub calls: Mutex<Vec<String>>,
    /// Every command as run.
    pub cmds: Mutex<Vec<Cmd>>,
    rules: Vec<(&'static str, Handler)>,
    counts: Mutex<HashMap<&'static str, usize>>,
}

impl FakeExec {
    pub fn on(
        mut self,
        pattern: &'static str,
        handler: impl Fn(&Cmd, usize) -> CmdOutput + Send + Sync + 'static,
    ) -> Self {
        self.rules.push((pattern, Box::new(handler)));
        self
    }
    /// Rules match in order: this puts the ones added last first.
    #[must_use]
    pub fn reversed(mut self) -> Self {
        self.rules.reverse();
        self
    }
    pub fn calls_matching(&self, pattern: &str) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|c| c.contains(pattern))
            .count()
    }
}

#[async_trait]
impl Exec for FakeExec {
    async fn run(&self, _: &StepScope, cmd: Cmd) -> Result<CmdOutput, ExecError> {
        let line = cmd.display();
        self.calls.lock().push(line.clone());
        self.cmds.lock().push(cmd.clone());
        for (pattern, handler) in &self.rules {
            if line.contains(pattern) {
                let n = {
                    let mut counts = self.counts.lock();
                    let n = counts.entry(pattern).or_default();
                    *n += 1;
                    *n
                };
                let mut out = handler(&cmd, n);
                out.seen = cmd
                    .watch
                    .iter()
                    .copied()
                    .filter(|p| out.tail.iter().any(|l| l.contains(p)))
                    .collect();
                return Ok(out);
            }
        }
        Ok(ok(""))
    }
}

pub fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        code: Some(0),
        stdout: stdout.to_owned(),
        tail: Vec::new(),
        seen: Vec::new(),
    }
}

pub fn fail(lines: &[&str]) -> CmdOutput {
    CmdOutput {
        code: Some(1),
        stdout: String::new(),
        tail: lines.iter().map(|l| (*l).to_owned()).collect(),
        seen: Vec::new(),
    }
}
