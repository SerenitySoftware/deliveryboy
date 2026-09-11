//! One place that knows every secret value this run resolved, and one scrubber
//! that every output path runs through.
//!
//! Individual sites already take care: `PlannedStep::write_file` keeps rendered
//! contents out of `--json` (`#[serde(skip_serializing)]`) and out of
//! `detail()`, and `deliver secrets` reports state without values. That is
//! per-site discipline, though — it holds only for the paths someone remembered,
//! and a resolved value that reaches a `Command` label, an `anyhow` context, or
//! a subprocess's stderr prints in full. A deploy log is exactly the artifact
//! people paste into bug reports and CI transcripts.
//!
//! So: `Resolver::get` records every value it hands out here, and `plan`
//! (human and `--json`), the executor, the console helpers in `ui`, and the
//! top-level error printer scrub against that record before anything reaches a
//! terminal. Redaction names the secret it replaced — `[redacted:API_TOKEN]` —
//! because the name is already public and knowing *which* value was elided is
//! what makes the output still debuggable.
//!
//! Values shorter than [`MIN_LEN`] are deliberately not recorded: substring
//! replacement of `on` or `1234` would corrupt unrelated output into nonsense
//! while protecting a value that has no entropy to protect. A secret that short
//! is not one this mechanism can defend; it needs to not be a secret.

use std::sync::{OnceLock, RwLock};

/// Below this length, substring scrubbing damages more than it protects.
pub const MIN_LEN: usize = 6;

type Registry = RwLock<Vec<(String, String)>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Remember a resolved value so every later print elides it.
///
/// Called from the one place values are produced ([`crate::secrets::Resolver::get`]),
/// so a new deployer inherits the protection without opting in.
pub fn record(name: &str, value: &str) {
    if value.len() < MIN_LEN {
        return;
    }
    let Ok(mut reg) = registry().write() else {
        return;
    };
    if reg.iter().any(|(known, _)| known == value) {
        return;
    }
    reg.push((value.to_string(), name.to_string()));
}

/// Replace every recorded value in `text` with `[redacted:NAME]`.
///
/// Longest first, so a value that contains another (a URL holding a token) is
/// elided whole rather than leaving its tail behind.
pub fn scrub(text: &str) -> String {
    let Ok(reg) = registry().read() else {
        return text.to_string();
    };
    if reg.is_empty() {
        return text.to_string();
    }
    let mut ordered: Vec<&(String, String)> = reg.iter().collect();
    ordered.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut out = text.to_string();
    for (value, name) in ordered {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("[redacted:{name}]"));
        }
    }
    out
}

/// Format an error chain with its values elided — what `main` prints.
pub fn scrub_error(err: &anyhow::Error) -> String {
    scrub(&format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-wide and tests share a process, so each test uses
    // values no other test registers.

    #[test]
    fn scrubs_a_recorded_value_everywhere_it_appears() {
        record("ALPHA_TOKEN", "zzz-alpha-value-01");
        let text = "url=zzz-alpha-value-01 and again zzz-alpha-value-01";
        let out = scrub(text);
        assert!(!out.contains("zzz-alpha-value-01"), "{out}");
        assert_eq!(out.matches("[redacted:ALPHA_TOKEN]").count(), 2, "{out}");
    }

    #[test]
    fn leaves_unrelated_text_alone() {
        record("BRAVO_TOKEN", "zzz-bravo-value-02");
        let out = scrub("nothing secret here at all");
        assert_eq!(out, "nothing secret here at all");
    }

    #[test]
    fn ignores_values_too_short_to_scrub_safely() {
        record("CHARLIE_FLAG", "on");
        // Scrubbing "on" would mangle every ordinary word containing it.
        let out = scrub("a connection is on");
        assert_eq!(out, "a connection is on");
    }

    #[test]
    fn longer_values_win_over_the_shorter_ones_inside_them() {
        record("DELTA_INNER", "zzz-delta-inner-03");
        record("DELTA_URL", "https://host/zzz-delta-inner-03/path-04");
        let out = scrub("posting to https://host/zzz-delta-inner-03/path-04 now");
        assert_eq!(out, "posting to [redacted:DELTA_URL] now");
    }

    #[test]
    fn recording_the_same_value_twice_keeps_one_entry() {
        record("ECHO_TOKEN", "zzz-echo-value-05");
        record("ECHO_TOKEN", "zzz-echo-value-05");
        let out = scrub("zzz-echo-value-05");
        assert_eq!(out, "[redacted:ECHO_TOKEN]");
    }

    #[test]
    fn scrubs_an_error_chain() {
        record("FOXTROT_TOKEN", "zzz-foxtrot-value-06");
        let err =
            anyhow::anyhow!("could not reach zzz-foxtrot-value-06").context("posting the notice");
        let out = scrub_error(&err);
        assert!(!out.contains("zzz-foxtrot-value-06"), "{out}");
        assert!(out.contains("[redacted:FOXTROT_TOKEN]"), "{out}");
        assert!(out.contains("posting the notice"), "{out}");
    }
}
