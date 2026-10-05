//! The parser is OS-free so the real launchd shapes also run in Linux's fixture suite.
#[path = "../src/platform/macos/launchd_observation.rs"]
mod parser;
use parser::JobObservation as J;
use std::path::Path;
const LABEL: &str = "io.frostdev.crosspane.agent";
const PLIST: &str = "/Users/test/Library/LaunchAgents/io.frostdev.crosspane.agent.plist";
const PROGRAM: &str = "/Users/test/Applications/Crosspane.app/Contents/MacOS/Crosspane";
fn selected() -> parser::SelectedJob<'static> {
    parser::SelectedJob {
        uid: 501,
        label: LABEL,
        plist: Path::new(PLIST),
        program: Path::new(PROGRAM),
    }
}
fn parse(body: &str) -> J {
    parser::job(Some(0), body.as_bytes(), b"", selected())
}
fn observed(extra: &str) -> String {
    format!("gui/501/{LABEL} = {{\npath = {PLIST}\nprogram = {PROGRAM}\n{extra}\n}}\n")
}
#[test]
fn large_selected_job_ignores_unrelated_fields_and_duplicate_nested_keys() {
    let extra = format!(
        "pid = 123\nenvironment = {{\n arbitrary!key = $'value {{}}'\n repeated = one\n repeated = two\n}}\n{}",
        "unfamiliar field = value\n".repeat(4000)
    );
    assert_eq!(parse(&observed(&extra)), J::Running(123));
}
#[test]
fn stopped_is_observed_without_becoming_absent_or_clean_exit() {
    assert_eq!(parse(&observed("state = not running")), J::LoadedStopped);
    assert_eq!(
        parse(&observed("state = not running\npid = 0")),
        J::LoadedStopped
    );
    assert_eq!(parse(&observed("state = running")), J::Unknown);
    assert_eq!(
        parse(&observed("state = not running\npid = 123")),
        J::Unknown
    );
}
#[test]
fn selected_authority_and_absence_remain_exact() {
    let valid = observed("pid = 123");
    for invalid in [
        valid.replace("gui/501", "gui/502"),
        valid.replace(PLIST, "/other"),
        valid.replace(PROGRAM, "/other"),
        observed("pid = 123\npid = 123"),
        observed("pid = maybe"),
        observed("pid = 123\nextra = {"),
        valid.trim_end_matches("}\n").into(),
    ] {
        assert_eq!(parse(&invalid), J::Unknown);
    }
    let missing = format!("Could not find service \"{LABEL}\" in domain for user gui: 501\n");
    assert_eq!(
        parser::job(Some(113), b"", missing.as_bytes(), selected()),
        J::Absent
    );
    assert_eq!(
        parser::job(Some(113), b"", b"unknown error", selected()),
        J::Unknown
    );
}
#[test]
fn disabled_ignores_other_jobs_but_never_guesses_the_selected_row() {
    let parse = |body: &str| parser::disabled(Some(0), body.as_bytes(), b"", LABEL);
    assert_eq!(
        parse(&format!(
            "disabled services = {{\nunknown syntax for another job\n\"other\" => future-value\n\"other\" => true\n\"{LABEL}\" => false\n}}"
        )),
        Some(false)
    );
    assert_eq!(
        parse("disabled services = {\n\"other\" => true\n}"),
        Some(false)
    );
    for row in [
        format!("\"{LABEL}\" => unknown"),
        format!("\"{LABEL}\" => true\n\"{LABEL}\" => false"),
        format!("{LABEL} malformed"),
    ] {
        assert_eq!(parse(&format!("disabled services = {{\n{row}\n}}")), None);
    }
}

#[test]
fn successful_selected_observations_accept_informational_stderr_but_absence_stays_exact() {
    use parser::{JobObservation, SelectedJob, disabled, job};
    let selected = SelectedJob {
        uid: 501,
        label: "io.test",
        plist: std::path::Path::new("/Users/test/job.plist"),
        program: std::path::Path::new("/Users/test/agent"),
    };
    let output = b"gui/501/io.test = {\npath = /Users/test/job.plist\nprogram = /Users/test/agent\npid = 42\n}\n";
    assert_eq!(
        job(Some(0), output, b"informational warning", selected),
        JobObservation::Running(42)
    );
    assert_eq!(
        disabled(
            Some(0),
            b"disabled services = {\n\"io.test\" => false\n}\n",
            b"warning",
            "io.test"
        ),
        Some(false)
    );
    assert_eq!(
        job(Some(113), b"", b"informational warning", selected),
        JobObservation::Unknown
    );
}

#[test]
fn unreadable_unrelated_bytes_are_ignored_but_selected_authority_remains_exact_utf8() {
    let mut bytes = observed("pid = 123\nunused = PLACEHOLDER").into_bytes();
    let at = bytes.windows(11).position(|w| w == b"PLACEHOLDER").unwrap();
    bytes.splice(at..at + 11, [0xff]);
    assert_eq!(
        parser::job(Some(0), &bytes, b"", selected()),
        J::Running(123)
    );
    bytes.splice(
        bytes.len() - 2..bytes.len() - 2,
        b"path = \xff\n".iter().copied(),
    );
    assert_eq!(parser::job(Some(0), &bytes, b"", selected()), J::Unknown);
    let unrelated = format!("disabled services = {{\n\"{LABEL}\" => false\n").into_bytes();
    let mut unrelated = unrelated;
    unrelated.extend_from_slice(b"other = \xff\n}\n");
    assert_eq!(
        parser::disabled(Some(0), &unrelated, b"", LABEL),
        Some(false)
    );
}
