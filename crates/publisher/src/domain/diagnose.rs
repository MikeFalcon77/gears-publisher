//! What a failed check's output says, in a line or two: which tests or crates
//! failed, and whether the host (disk, memory) broke the build rather than the code.
//!
//! A failed `make test` buries its cause: nextest prints a failing test's whole
//! captured output, hundreds of warning lines around the one that matters (a linker
//! killed by a full disk reads `ld terminated with signal 7 [Bus error]`).
use crate::domain::ports::CmdOutput;

/// Host trouble, as cargo, rustc and the linker report it. Watched on every line.
pub const HOST_PATTERNS: [&str; 7] = [
    "No space left on device",
    "(os error 28)",
    "Disk quota exceeded",
    // ld's mmap of its output on a full disk.
    "terminated with signal 7",
    // The kernel's OOM killer, on rustc, ld or a build script.
    "terminated with signal 9",
    "signal: 9, SIGKILL",
    "Cannot allocate memory",
];

/// Less free space than this after a failure: the disk ran out during it.
const FULL_DISK_GIB: u64 = 2;
/// Failures named; the rest are counted.
const MAX_NAMED: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCause {
    DiskFull,
    OutOfMemory,
}

impl HostCause {
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            Self::DiskFull => {
                "the build host ran out of disk space (not the code's fault); \
                 the next run frees space first, re-run it"
            }
            Self::OutOfMemory => {
                "a compiler or linker was killed (signal 9): the build host probably \
                 ran out of memory (not the code's fault); re-run it"
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnosis {
    pub cause: Option<HostCause>,
    /// Failing tests and crates that did not compile, in the order seen.
    pub failures: Vec<String>,
}

impl Diagnosis {
    /// One line for the log.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        let failures = (!self.failures.is_empty()).then(|| self.failures.join("; "));
        match (self.cause, failures) {
            (Some(cause), Some(failures)) => {
                Some(format!("{}. Failed: {failures}", cause.explain()))
            }
            (Some(cause), None) => Some(cause.explain().to_owned()),
            (None, Some(failures)) => Some(format!("failed: {failures}")),
            (None, None) => None,
        }
    }
}

/// `free_gib`: free space on the build's filesystem right after the failure.
#[must_use]
pub fn diagnose(out: &CmdOutput, free_gib: Option<u64>) -> Diagnosis {
    let seen = |patterns: &[&str]| patterns.iter().any(|p| out.saw(p));
    let cause = if seen(&HOST_PATTERNS[..4]) || free_gib.is_some_and(|gib| gib < FULL_DISK_GIB) {
        Some(HostCause::DiskFull)
    } else if seen(&HOST_PATTERNS[4..]) {
        Some(HostCause::OutOfMemory)
    } else {
        None
    };
    Diagnosis {
        cause,
        failures: failures(&out.tail),
    }
}

/// nextest's `FAIL [..] (n/N) crate::binary test`, libtest's `test x ... FAILED`, and
/// cargo's ``error: could not compile `crate` ``.
fn failures(lines: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in lines {
        let line = line.trim();
        let name = nextest_failure(line)
            .or_else(|| {
                line.strip_prefix("error: could not compile `")
                    .and_then(|rest| rest.split('`').next())
                    .map(|krate| format!("{krate} does not compile"))
            })
            .or_else(|| {
                line.strip_prefix("test ")
                    .and_then(|rest| rest.strip_suffix(" ... FAILED"))
                    .map(str::to_owned)
            });
        if let Some(name) = name
            && !found.contains(&name)
        {
            found.push(name);
        }
    }
    if found.len() > MAX_NAMED {
        let more = found.len() - MAX_NAMED;
        found.truncate(MAX_NAMED);
        found.push(format!("and {more} more"));
    }
    found
}

fn nextest_failure(line: &str) -> Option<String> {
    let status = [
        "FAIL",
        "TIMEOUT",
        "SIGSEGV",
        "SIGABRT",
        "SIGKILL",
        "SIGBUS",
        "LEAK-FAIL",
    ]
    .into_iter()
    .find(|s| line.starts_with(&format!("{s} [")))?;
    // `FAIL [ 715.813s] (27693/27693) crate::binary test`
    let rest = line.split_once("] ")?.1;
    let name = rest.split_once(") ").map_or(rest, |(_, name)| name).trim();
    (!name.is_empty()).then(|| match status {
        "FAIL" => name.to_owned(),
        other => format!("{name} ({other})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(tail: &[&str], seen: &[&'static str]) -> CmdOutput {
        CmdOutput {
            code: Some(2),
            tail: tail.iter().map(|l| (*l).to_owned()).collect(),
            seen: seen.to_vec(),
            ..CmdOutput::default()
        }
    }

    /// PR #5724: the disk filled during `make test-no-macros`; trybuild's link died.
    #[test]
    fn a_linker_killed_by_a_full_disk_blames_the_host_and_names_the_test() {
        let out = output(
            &[
                "    error: linking with `cc` failed: exit status: 1",
                "              collect2: fatal error: ld terminated with signal 7 [Bus error]",
                "    error: could not compile `cf-gears-toolkit-contract-macros-tests-tests` (bin \"trybuild012\") due to 1 previous error",
                "        FAIL [ 715.813s] (27693/27693) cf-gears-toolkit-contract-macros-tests::ui ui",
                "error: test run failed",
            ],
            &["terminated with signal 7"],
        );
        let diagnosis = diagnose(&out, Some(40));
        assert_eq!(diagnosis.cause, Some(HostCause::DiskFull));
        assert_eq!(
            diagnosis.failures,
            [
                "cf-gears-toolkit-contract-macros-tests-tests does not compile",
                "cf-gears-toolkit-contract-macros-tests::ui ui"
            ]
        );
        assert!(
            diagnosis
                .summary()
                .unwrap()
                .contains("ran out of disk space")
        );
    }

    #[test]
    fn a_full_disk_shows_after_the_failure_even_without_a_message() {
        assert_eq!(
            diagnose(&output(&[], &[]), Some(0)).cause,
            Some(HostCause::DiskFull)
        );
        assert_eq!(diagnose(&output(&[], &[]), Some(50)).cause, None);
        assert_eq!(diagnose(&output(&[], &[]), None).cause, None);
    }

    #[test]
    fn a_killed_compiler_is_memory() {
        let out = output(&[], &["signal: 9, SIGKILL"]);
        assert_eq!(
            diagnose(&out, Some(100)).cause,
            Some(HostCause::OutOfMemory)
        );
    }

    /// An ordinary failure is the code's: named, no host cause.
    #[test]
    fn failing_tests_are_named_once_and_capped() {
        let mut tail: Vec<String> = vec![
            "        FAIL [   0.120s] (1/9) cf-a::it test_one".into(),
            "     TIMEOUT [ 300.000s] (2/9) cf-b tests::slow".into(),
            "test parser::rejects_garbage ... FAILED".into(),
            "        FAIL [   0.120s] (1/9) cf-a::it test_one".into(),
        ];
        tail.extend((0..10).map(|i| format!("        FAIL [   0.1s] ({i}/9) cf-c t{i}")));
        let refs: Vec<&str> = tail.iter().map(String::as_str).collect();
        let diagnosis = diagnose(&output(&refs, &[]), Some(100));
        assert_eq!(diagnosis.cause, None);
        assert_eq!(
            &diagnosis.failures[..3],
            [
                "cf-a::it test_one",
                "cf-b tests::slow (TIMEOUT)",
                "parser::rejects_garbage"
            ]
        );
        assert_eq!(diagnosis.failures.len(), MAX_NAMED + 1);
        assert_eq!(diagnosis.failures.last().unwrap(), "and 5 more");
        assert!(
            diagnosis
                .summary()
                .unwrap()
                .starts_with("failed: cf-a::it test_one")
        );
    }

    #[test]
    fn nothing_recognised_says_nothing() {
        assert_eq!(
            diagnose(&output(&["error: something"], &[]), Some(100)).summary(),
            None
        );
    }
}
