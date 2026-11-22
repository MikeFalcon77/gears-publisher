//! Change detection as gears-rust's workflows do it: the `changes` job of `ci.yml`
//! (dorny/paths-filter), `ci.yml`'s workflow-level `paths-ignore` and `fmt.yml`'s
//! `paths`. The rules are read from the PR's own workflow files, so they follow the
//! repository; the built-in copy is used only when those files cannot be read.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Path rules of the workflows PR CI follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// `changes` job filters: name → globs.
    pub filters: BTreeMap<String, Vec<String>>,
    /// `ci.yml` does not run when every changed file matches one of these.
    pub ci_paths_ignore: Vec<String>,
    /// `fmt.yml` runs only when a changed file matches one of these.
    pub fmt_paths: Vec<String>,
    /// Where the rules came from: the PR's workflow files or the built-in copy.
    pub source: String,
}

/// What the rules say about one PR's changed files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    /// Filter name → whether a changed file matches it.
    pub filters: BTreeMap<String, bool>,
    /// `ci.yml` would run (some file is outside its `paths-ignore`).
    pub ci_workflow: bool,
    /// `fmt.yml` would run.
    pub fmt_workflow: bool,
}

impl Rules {
    /// Rules from the text of `ci.yml` and `fmt.yml`. Parts a file lacks keep the
    /// built-in value; a file that does not parse is an error.
    pub fn from_workflows(ci_yml: &str, fmt_yml: Option<&str>) -> Result<Self, String> {
        let ci: serde_yaml::Value =
            serde_yaml::from_str(ci_yml).map_err(|e| format!("ci.yml: {e}"))?;
        let mut rules = Self::builtin();
        rules.source = ".github/workflows/ci.yml".into();
        if let Some(ignore) = strings(&ci["on"]["pull_request"]["paths-ignore"]) {
            rules.ci_paths_ignore = ignore;
        }
        let filters = ci["jobs"]["changes"]["steps"]
            .as_sequence()
            .into_iter()
            .flatten()
            .find(|step| step["id"].as_str() == Some("filter"))
            .and_then(|step| step["with"]["filters"].as_str())
            .ok_or("ci.yml: jobs.changes has no `filter` step with `filters`")?;
        let filters: BTreeMap<String, Vec<String>> =
            serde_yaml::from_str(filters).map_err(|e| format!("ci.yml filters: {e}"))?;
        if filters.is_empty() {
            return Err("ci.yml: the change filters are empty".into());
        }
        rules.filters = filters;
        if let Some(fmt) = fmt_yml {
            let fmt: serde_yaml::Value =
                serde_yaml::from_str(fmt).map_err(|e| format!("fmt.yml: {e}"))?;
            rules.fmt_paths = strings(&fmt["on"]["pull_request"]["paths"]).unwrap_or_default();
        }
        Ok(rules)
    }

    /// gears-rust's rules as of November 2026, for when the workflow files are missing
    /// or unreadable. Only the filters the jobs here depend on.
    #[must_use]
    pub fn builtin() -> Self {
        let list = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let filters = BTreeMap::from([
            (
                "rust".to_owned(),
                list(&[
                    "**/*.rs",
                    "**/Cargo.toml",
                    "Cargo.lock",
                    "Makefile",
                    ".gears-dna/gears.mk",
                    "gears-rust.mk",
                    "rust-toolchain.toml",
                    ".cargo/**",
                    "tools/scripts/check_crypto_facade.py",
                    "tools/scripts/tests/test_crypto_facade.py",
                    "release-plz.toml",
                    "deploy/components.yaml",
                    "tools/scripts/resolve_release.py",
                    "tools/scripts/tests/test_resolve_release.py",
                    "tools/scripts/release_retry.py",
                    "tools/scripts/tests/test_release_retry.py",
                    "testing/e2e/lib/sidecars.py",
                ]),
            ),
            ("dylint".to_owned(), list(&["tools/dylint_lints/**"])),
            (
                "deps".to_owned(),
                list(&["**/Cargo.toml", "Cargo.lock", "deny.toml"]),
            ),
            (
                "ci".to_owned(),
                list(&[".github/workflows/**", ".github/actions/**"]),
            ),
        ]);
        Self {
            filters,
            ci_paths_ignore: list(&["**/*.md", "docs/**"]),
            fmt_paths: list(&[
                "**/*.rs",
                "Makefile",
                ".gears-dna/gears.mk",
                "gears-rust.mk",
            ]),
            source: "built-in".into(),
        }
    }

    /// Evaluates the rules on the changed files.
    #[must_use]
    pub fn evaluate(&self, files: &[String]) -> Changes {
        let any = |globs: &[String]| files.iter().any(|f| matches_any(globs, f));
        Changes {
            filters: self
                .filters
                .iter()
                .map(|(name, globs)| (name.clone(), any(globs)))
                .collect(),
            // An empty diff runs nothing, like a push of no files.
            ci_workflow: files.iter().any(|f| !matches_any(&self.ci_paths_ignore, f)),
            fmt_workflow: self.fmt_paths.is_empty() || any(&self.fmt_paths),
        }
    }
}

fn strings(value: &serde_yaml::Value) -> Option<Vec<String>> {
    value.as_sequence().map(|seq| {
        seq.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

/// A path matches when a positive glob does; `!` globs are not used by these
/// workflows and never match.
fn matches_any(globs: &[String], path: &str) -> bool {
    globs
        .iter()
        .filter(|g| !g.starts_with('!'))
        .any(|g| glob_match(g, path))
}

/// picomatch with `dot: true`, as paths-filter uses it: `**` spans any number of
/// directories (also none), `*` and `?` stay within one path segment.
#[must_use]
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.trim_start_matches("./").split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((first, rest)) => match path.split_first() {
            Some((segment, path_rest)) => {
                segment_match(first.as_bytes(), segment.as_bytes())
                    && segments_match(rest, path_rest)
            }
            None => false,
        },
    }
}

fn segment_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_match(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && segment_match(rest, &text[1..]),
        Some((c, rest)) => text.first() == Some(c) && segment_match(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_follow_picomatch() {
        assert!(glob_match("**/*.rs", "src/lib.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"));
        assert!(glob_match("**/*.rs", "a/b/c/d.rs"));
        assert!(!glob_match("**/*.rs", "src/lib.rsx"));
        assert!(glob_match("**/Cargo.toml", "Cargo.toml"));
        assert!(glob_match("Cargo.lock", "Cargo.lock"));
        assert!(!glob_match("Cargo.lock", "x/Cargo.lock"));
        assert!(glob_match(".cargo/**", ".cargo/config.toml"));
        assert!(glob_match(
            "libs/rustls-*/**",
            "libs/rustls-corecrypto/src/a.rs"
        ));
        assert!(!glob_match("libs/rustls-*/**", "libs/toolkit/src/a.rs"));
        assert!(glob_match("docs/**", "docs/a/b.png"));
        assert!(glob_match("**/*.md", ".github/README.md"));
        assert!(glob_match("file?.txt", "file1.txt"));
    }

    const CI_YML: &str = r"
name: CI
on:
  pull_request:
    branches: [ main ]
    paths-ignore:
      - '**/*.md'
      - 'docs/**'
jobs:
  changes:
    steps:
      - name: Checkout
        uses: actions/checkout@v7
      - name: Detect changed paths
        id: filter
        with:
          token: ''
          filters: |
            rust:
              - '**/*.rs'
              # a comment
              - 'Cargo.lock'
            deps:
              - '**/Cargo.toml'
            ci:
              - '.github/workflows/**'
";

    const FMT_YML: &str =
        "on:\n  pull_request:\n    paths:\n      - '**/*.rs'\n      - 'Makefile'\n";

    #[test]
    fn rules_are_read_from_the_workflows() {
        let rules = Rules::from_workflows(CI_YML, Some(FMT_YML)).unwrap();
        assert_eq!(rules.filters["rust"], ["**/*.rs", "Cargo.lock"]);
        assert_eq!(rules.filters.len(), 3);
        assert_eq!(rules.ci_paths_ignore, ["**/*.md", "docs/**"]);
        assert_eq!(rules.fmt_paths, ["**/*.rs", "Makefile"]);
        assert!(Rules::from_workflows("jobs: {}", None).is_err());
        assert!(Rules::from_workflows(": : :", None).is_err());
    }

    #[test]
    fn changes_are_evaluated() {
        let rules = Rules::from_workflows(CI_YML, Some(FMT_YML)).unwrap();
        let files = |names: &[&str]| names.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let rust = rules.evaluate(&files(&["gears/x/src/lib.rs", "README.md"]));
        assert!(rust.filters["rust"] && !rust.filters["deps"] && !rust.filters["ci"]);
        assert!(rust.ci_workflow && rust.fmt_workflow);

        let docs = rules.evaluate(&files(&["docs/a.md", "gears/x/README.md"]));
        assert!(!docs.ci_workflow && !docs.fmt_workflow);

        let deps = rules.evaluate(&files(&["gears/x/Cargo.toml"]));
        assert!(deps.filters["deps"] && !deps.filters["rust"] && !deps.fmt_workflow);
    }

    #[test]
    fn the_builtin_rules_match_the_workflows_jobs() {
        let rules = Rules::builtin();
        for name in ["rust", "deps", "ci", "dylint"] {
            assert!(rules.filters.contains_key(name), "{name}");
        }
    }
}
