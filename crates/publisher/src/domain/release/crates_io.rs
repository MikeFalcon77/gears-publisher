//! crates.io: rate-limit answers and the sparse index format.
use chrono::{DateTime, Utc};
use std::time::Duration;

/// The `try again after` time of a crates.io 429 answer in release-plz output.
#[must_use]
pub fn rate_limited_until(output: &str) -> Option<DateTime<Utc>> {
    const MARKER: &str = "try again after ";
    if !output.contains("429") {
        return None;
    }
    output.match_indices(MARKER).find_map(|(at, _)| {
        let rest = &output[at + MARKER.len()..];
        // "Wed, 18 Nov 2026 15:16:16 GMT and see https://…"
        let end = rest.find(" GMT").map(|i| i + " GMT".len())?;
        DateTime::parse_from_rfc2822(&rest[..end])
            .ok()
            .map(|t| t.with_timezone(&Utc))
    })
}

/// Whether the output says crates.io rate-limited the upload, with or without a time.
#[must_use]
pub fn is_rate_limited(output: &str) -> bool {
    output.contains("429 Too Many Requests") || rate_limited_until(output).is_some()
}

/// How long to wait for a 429 that names its retry time.
#[must_use]
pub fn wait_for(until: DateTime<Utc>, now: DateTime<Utc>, margin: Duration) -> Duration {
    let wait = (until - now).to_std().unwrap_or_default() + margin;
    wait.clamp(Duration::from_secs(1), Duration::from_hours(24))
}

/// Path of a crate in the sparse index.
#[must_use]
pub fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// Published, non-yanked versions out of an index file.
#[must_use]
pub fn versions(index_file: &str) -> Vec<String> {
    index_file
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| !v["yanked"].as_bool().unwrap_or(false))
        .filter_map(|v| v["vers"].as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "the remote server responded with an error (status 429 Too Many Requests): You have published too many updates to existing crates in a short period of time. Please try again after Wed, 18 Nov 2026 15:16:16 GMT and see https://crates.io/docs/rate-limits for more details.";

    #[test]
    fn the_retry_time_is_parsed() {
        let until = rate_limited_until(ANSWER).unwrap();
        assert_eq!(until.to_rfc3339(), "2026-11-18T15:16:16+00:00");
        assert!(is_rate_limited(ANSWER));
        assert!(!is_rate_limited("error: failed to verify package"));
    }

    #[test]
    fn the_wait_adds_the_margin_and_is_bounded() {
        let until = rate_limited_until(ANSWER).unwrap();
        let now = until - chrono::Duration::seconds(100);
        assert_eq!(
            wait_for(until, now, Duration::from_secs(15)),
            Duration::from_secs(115)
        );
        let late = until + chrono::Duration::seconds(100);
        assert_eq!(
            wait_for(until, late, Duration::from_secs(15)),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn index_paths_follow_the_cargo_layout() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("cf-gears-toolkit"), "cf/-g/cf-gears-toolkit");
    }

    #[test]
    fn yanked_versions_are_ignored() {
        let file = "{\"vers\":\"0.1.0\",\"yanked\":false}\n{\"vers\":\"0.2.0\",\"yanked\":true}\n";
        assert_eq!(versions(file), vec!["0.1.0"]);
    }
}
