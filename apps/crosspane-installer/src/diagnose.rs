//! Read-only setup observations. No controller, mutation plan execution or prompts are started.
use serde::Serialize;
use std::{fmt::Debug, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Class {
    S,
    #[cfg(any(test, target_os = "linux", target_os = "macos"))]
    E,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    R,
}
#[derive(Debug, Serialize)]
pub(crate) struct Fact {
    step: &'static str,
    check: String,
    class: Class,
    value: Option<serde_json::Value>,
    issue: Option<String>,
    hard_stop: bool,
}
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    schema_version: u8,
    platform: &'static str,
    read_only: bool,
    facts: Vec<Fact>,
}
impl Report {
    pub(crate) fn new() -> Self {
        Self {
            schema_version: 1,
            platform: std::env::consts::OS,
            read_only: true,
            facts: Vec::new(),
        }
    }
    pub(crate) fn record<T: Serialize, E: Debug>(
        &mut self,
        step: &'static str,
        check: impl Into<String>,
        class: Class,
        value: Result<T, E>,
        blocking: bool,
    ) {
        let (value, issue) = match value {
            Ok(value) => match serde_json::to_value(value) {
                Ok(value) => (Some(value), None),
                Err(_) => (None, Some("Cannot encode observed value".into())),
            },
            Err(error) => (None, Some(format!("{error:?}"))),
        };
        self.facts.push(Fact {
            step,
            check: check.into(),
            class,
            hard_stop: blocking && issue.is_some(),
            value,
            issue,
        });
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn debug<T: Debug, E: Debug>(
        &mut self,
        step: &'static str,
        check: impl Into<String>,
        class: Class,
        value: Result<T, E>,
        blocking: bool,
    ) {
        self.record(
            step,
            check,
            class,
            value.map(|value| format!("{value:?}")),
            blocking,
        );
    }
    pub(crate) fn issue(
        &mut self,
        step: &'static str,
        check: &str,
        class: Class,
        issue: &str,
        blocking: bool,
    ) {
        self.record::<bool, _>(step, check, class, Err(issue), blocking);
    }
    pub(crate) fn unavailable_target(&mut self) {
        for (step, check) in [
            ("support", "selected session authority"),
            ("payload", "owned files"),
            ("service", "selected job"),
            ("agent", "matched Status"),
        ] {
            self.issue(
                step,
                check,
                Class::S,
                "Selected target could not be admitted",
                true,
            );
        }
    }
}
/// Prints one JSON document even when an observation cannot be made. All issues retain their class.
pub fn run(payload: Option<PathBuf>) -> anyhow::Result<()> {
    let mut report = Report::new();
    #[cfg(target_os = "linux")]
    crate::platform::linux::integration::diagnose(payload.as_deref(), &mut report);
    #[cfg(target_os = "macos")]
    crate::platform::macos::integration::diagnose(payload.as_deref(), &mut report);
    #[cfg(all(windows, not(test)))]
    crate::platform::windows::integration::diagnose::run(payload.as_deref(), &mut report);
    #[cfg(all(windows, test))]
    {
        let _ = payload;
        report.unavailable_target();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = payload;
        report.unavailable_target();
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_evidence_is_a_note_and_authority_unknown_remains_a_stop() {
        let mut report = Report::new();
        report.record::<bool, _>("support", "protocols", Class::E, Err("Unavailable"), false);
        report.record::<bool, _>("support", "session", Class::S, Err("Unavailable"), true);
        report.record::<bool, &str>("support", "version", Class::E, Ok(false), false);
        assert!(report.facts[0].value.is_none());
        assert!(!report.facts[0].hard_stop);
        assert!(report.facts[1].hard_stop);
        assert_eq!(report.facts[2].value, Some(serde_json::json!(false)));
        assert!(report.facts[2].issue.is_none());
        report.issue(
            "service",
            "observed compatibility refusal",
            Class::E,
            "A required library is proven missing",
            true,
        );
        assert!(report.facts[3].hard_stop);
        assert_eq!(report.facts[3].class, Class::E);
    }
}
