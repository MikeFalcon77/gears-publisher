//! Errors and warnings out of output too long to read.
use std::collections::VecDeque;

/// Lines of context kept before a problem.
const CONDENSE_BEFORE: usize = 2;
/// Lines of one diagnostic kept (cargo-deny's inclusion trees run to hundreds).
pub(super) const CONDENSE_BLOCK: usize = 40;
/// Lines kept from the end of the output after the last diagnostic (the summary,
/// `make: ***`).
pub(super) const CONDENSE_TAIL: usize = 15;

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
pub(super) fn is_problem(line: &str) -> bool {
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
