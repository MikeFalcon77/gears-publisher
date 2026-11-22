//! The GitHub Actions jobs PR CI runs, as gears-rust's workflows define them on a
//! `pull_request`: names, steps and the change filters in their `if:`. Jobs that need
//! Docker, other operating systems or GitHub itself are not here (see the README).
use super::changes::Changes;

/// One job of a workflow.
#[derive(Debug)]
pub struct Job {
    /// Durable step id.
    pub id: &'static str,
    /// The job's name on GitHub.
    pub name: &'static str,
    /// The workflow file that defines it.
    pub workflow: &'static str,
    pub gate: Gate,
    pub steps: &'static [JobStep],
    /// Said in the log before the job's steps: what of the GitHub job is left out.
    pub note: Option<&'static str>,
}

/// When a job runs.
#[derive(Debug, Clone, Copy)]
pub enum Gate {
    /// A `ci.yml` job without `if:`; it still needs `ci.yml` to run at all.
    CiWorkflow,
    /// A `ci.yml` job whose `if:` requires one of these change filters.
    CiFilters(&'static [&'static str]),
    /// `fmt.yml`, gated by its own `paths`.
    FmtWorkflow,
}

/// One step of a job: the shell commands of its `run:`.
#[derive(Debug)]
pub struct JobStep {
    pub name: &'static str,
    pub run: &'static [&'static str],
    /// Output logged condensed to errors and warnings (see `Logged::Condensed`).
    pub condensed: bool,
}

const fn step(name: &'static str, run: &'static [&'static str]) -> JobStep {
    JobStep {
        name,
        run,
        condensed: false,
    }
}

const RUST: &[&str] = &["rust", "ci"];

/// In the order they run here: quick verdicts first. GitHub runs them side by side.
pub const JOBS: [Job; 7] = [
    Job {
        id: "fmt",
        name: "cargo fmt (check)",
        workflow: "fmt.yml",
        gate: Gate::FmtWorkflow,
        steps: &[step("cargo fmt (check)", &["make fmt"])],
        note: None,
    },
    Job {
        id: "repo_checks",
        name: "Repo checks",
        workflow: "ci.yml",
        gate: Gate::CiFilters(RUST),
        steps: &[
            step(
                "Validate gear naming conventions",
                &["python3 tools/scripts/validate_gear_names.py"],
            ),
            step(
                "Validate PR-review rule set",
                &["python3 tools/scripts/toolkit-pr-review/lint.py"],
            ),
            step(
                "PR-review script tests",
                &["python3 -m unittest discover -s tools/scripts/toolkit-pr-review/tests"],
            ),
            step(
                "Validate packaging metadata (readme/license-file paths)",
                &["make check-packaging-metadata"],
            ),
            step(
                "Validate release configuration (publish flags, release-plz.toml)",
                &["make check-release-config"],
            ),
            step(
                "Release resolver tests (resolve_release.py + components.yaml)",
                &["make check-release-resolver"],
            ),
        ],
        note: None,
    },
    Job {
        id: "security",
        name: "Security (cargo-deny on Ubuntu)",
        workflow: "ci.yml",
        gate: Gate::CiFilters(&["deps", "ci"]),
        steps: &[JobStep {
            name: "cargo deny check",
            run: &["make deny"],
            condensed: true,
        }],
        note: None,
    },
    Job {
        id: "clippy",
        name: "Clippy",
        workflow: "ci.yml",
        gate: Gate::CiFilters(RUST),
        steps: &[
            step(
                "No unpinned database container images",
                &["cargo xtask check-test-container-pins"],
            ),
            step(
                "Application crypto stays behind the facade",
                &[
                    "python3 tools/scripts/check_crypto_facade.py",
                    "python3 -m unittest discover -s tools/scripts/tests -p test_crypto_facade.py",
                ],
            ),
            step("cargo clippy", &["make clippy"]),
        ],
        note: None,
    },
    Job {
        id: "lint",
        name: "Architecture Lints (cargo gears lint)",
        workflow: "ci.yml",
        gate: Gate::CiWorkflow,
        steps: &[step("Run architecture lints", &["make dylint"])],
        note: None,
    },
    Job {
        id: "test",
        name: "Test Suite (ubuntu-latest) / nextest",
        workflow: "test-suite.yml",
        gate: Gate::CiFilters(RUST),
        steps: &[step("cargo test (no macros)", &["make test-no-macros"])],
        note: None,
    },
    Job {
        id: "integration",
        name: "Integration tests",
        workflow: "ci.yml",
        gate: Gate::CiFilters(RUST),
        steps: &[step(
            "UI tests for toolkit macros libraries",
            &["make test-macros"],
        )],
        note: Some(
            "the database lanes (make test-integration LANE=…, test-eb-pg, \
             test-durable-execution-pg, …) need Docker and are not run here",
        ),
    },
];

#[must_use]
pub fn job(id: &str) -> Option<&'static Job> {
    JOBS.iter().find(|j| j.id == id)
}

impl Job {
    /// Why the job does not run for these changes, as GitHub would decide.
    #[must_use]
    pub fn skip_reason(&self, changes: &Changes) -> Option<String> {
        let ci = || {
            (!changes.ci_workflow)
                .then(|| "only files in ci.yml's paths-ignore changed (docs, markdown)".to_owned())
        };
        match self.gate {
            Gate::FmtWorkflow => {
                (!changes.fmt_workflow).then(|| "no change matches fmt.yml's paths".to_owned())
            }
            Gate::CiWorkflow => ci(),
            Gate::CiFilters(filters) => ci().or_else(|| {
                let any = filters
                    .iter()
                    .any(|f| changes.filters.get(*f).copied().unwrap_or(true));
                (!any).then(|| format!("no `{}` changes (ci.yml's if:)", filters.join("` or `")))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn changes(filters: &[(&str, bool)], ci: bool, fmt: bool) -> Changes {
        Changes {
            filters: filters
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect::<BTreeMap<_, _>>(),
            ci_workflow: ci,
            fmt_workflow: fmt,
        }
    }

    #[test]
    fn jobs_are_gated_like_on_github() {
        let deps_only = changes(
            &[("rust", false), ("ci", false), ("deps", true)],
            true,
            false,
        );
        let skipped: Vec<&str> = JOBS
            .iter()
            .filter(|j| j.skip_reason(&deps_only).is_some())
            .map(|j| j.id)
            .collect();
        assert_eq!(
            skipped,
            ["fmt", "repo_checks", "clippy", "test", "integration"]
        );

        let docs = changes(&[("rust", false)], false, false);
        assert!(JOBS.iter().all(|j| j.skip_reason(&docs).is_some()));

        // A filter the workflow no longer defines does not skip anything.
        let unknown = changes(&[], true, true);
        assert!(JOBS.iter().all(|j| j.skip_reason(&unknown).is_none()));
    }

    #[test]
    fn job_ids_are_unique_step_ids() {
        let mut ids: Vec<&str> = JOBS.iter().map(|j| j.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), JOBS.len());
        for id in ids {
            assert!(super::super::STEPS.contains(&id), "{id}");
        }
    }
}
