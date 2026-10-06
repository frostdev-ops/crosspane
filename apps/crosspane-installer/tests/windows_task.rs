#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! In-memory task fixtures only; no COM or scheduled-task operation is executed.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(windows)]
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use native_io::{NativeError, NativeResult};
use service::task::*;
use std::collections::VecDeque;

fn desired() -> Definition {
    Definition {
        name: TASK_NAME.into(),
        principal: "S-1-5-21-101".into(),
        trigger_user: "S-1-5-21-101".into(),
        logon: Logon::InteractiveToken,
        run_level: RunLevel::Limited,
        action: "C:\\fixture\\Programs\\Crosspane\\crosspane-installer.exe".into(),
        arguments: SUPERVISOR_ARGUMENT.into(),
        working_directory: "C:\\fixture\\Programs\\Crosspane".into(),
        logon_trigger_only: true,
        ignore_new_instance: true,
        manager_restart_count: 0,
        enabled: true,
    }
}
fn snapshot(definition: Definition) -> Snapshot {
    Snapshot {
        definition,
        xml: "<fixture-owned-task/>".into(),
    }
}
struct Fake {
    views: VecDeque<NativeResult<Option<Snapshot>>>,
    events: Vec<&'static str>,
    failure: Option<(&'static str, NativeError)>,
    original: Option<Snapshot>,
    retired: bool,
}
impl Fake {
    fn new(views: Vec<Option<Snapshot>>) -> Self {
        Self {
            views: views.into_iter().map(Ok).collect(),
            events: Vec::new(),
            failure: None,
            original: None,
            retired: false,
        }
    }
    fn stage(&mut self, name: &'static str) -> NativeResult<()> {
        self.events.push(name);
        if self.failure.is_some_and(|(stage, _)| stage == name) {
            return Err(self.failure.unwrap().1);
        }
        Ok(())
    }
}
impl TaskPort for Fake {
    fn inspect(&mut self) -> NativeResult<Option<Snapshot>> {
        self.stage("inspect")?;
        self.views.pop_front().unwrap()
    }
    fn record_intent(&mut self, original: Option<&Snapshot>) -> NativeResult<()> {
        self.stage("intent")?;
        self.original = original.cloned();
        Ok(())
    }
    fn register(&mut self, _: &Definition) -> NativeResult<()> {
        self.stage("register")
    }
    fn record_result(&mut self) -> NativeResult<()> {
        self.stage("result")
    }
    fn retire(&mut self) {
        self.events.push("retire");
        self.retired = true;
    }
}

#[test]
fn missing_task_plans_only_exact_limited_interactive_action() {
    assert_eq!(plan(&desired(), None), Ok(Plan::Register));
}
#[test]
fn matching_task_needs_no_registration() {
    assert_eq!(plan(&desired(), Some(&snapshot(desired()))), Ok(Plan::Keep));
}
#[test]
fn known_disabled_task_is_preserved_without_start_or_registration() {
    let mut old = desired();
    old.enabled = false;
    let mut fake = Fake::new(vec![Some(snapshot(old))]);
    assert_eq!(reconcile(&mut fake, &desired()), Ok(Plan::PreserveDisabled));
    assert_eq!(fake.events, ["inspect"]);
}
#[test]
fn foreign_principal_refuses_zero_mutation() {
    let mut old = desired();
    old.principal = "S-1-5-21-202".into();
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn different_logon_trigger_user_refuses() {
    let mut old = desired();
    old.trigger_user = "S-1-5-21-202".into();
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn highest_run_level_refuses() {
    let mut old = desired();
    old.run_level = RunLevel::Highest;
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn password_and_service_account_logons_refuse() {
    for logon in [Logon::Password, Logon::ServiceAccount] {
        let mut old = desired();
        old.logon = logon;
        assert_eq!(
            plan(&desired(), Some(&snapshot(old))),
            Err(NativeError::Foreign)
        );
    }
}
#[test]
fn foreign_action_refuses_without_path_discovery() {
    let mut old = desired();
    old.action = "C:\\foreign\\crosspane-installer.exe".into();
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn extra_supervisor_arguments_and_foreign_working_directory_refuse() {
    let mut old = desired();
    old.arguments.push_str(" --path C:\\foreign");
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
    let mut old = desired();
    old.working_directory = "C:\\foreign".into();
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn unrelated_or_additional_trigger_refuses() {
    let mut old = desired();
    old.logon_trigger_only = false;
    assert_eq!(
        plan(&desired(), Some(&snapshot(old))),
        Err(NativeError::Foreign)
    );
}
#[test]
fn owned_restart_settings_replacement_preserves_exact_original_xml() {
    let mut old = desired();
    old.ignore_new_instance = false;
    old.manager_restart_count = 3;
    let original = snapshot(old);
    let mut fake = Fake::new(vec![Some(original.clone()), Some(original.clone())]);
    assert_eq!(reconcile(&mut fake, &desired()), Ok(Plan::Register));
    assert_eq!(fake.original, Some(original));
    assert_eq!(
        fake.events,
        ["inspect", "intent", "inspect", "register", "result"]
    );
}
#[test]
fn oversized_original_xml_refuses_before_backup_or_task_change() {
    let mut old = snapshot(desired());
    old.xml = "x".repeat(MAX_XML_BYTES + 1);
    assert_eq!(plan(&desired(), Some(&old)), Err(NativeError::Oversize));
}
#[test]
fn task_snapshot_changed_after_intent_refuses_before_dispatch() {
    let mut fake = Fake::new(vec![None, Some(snapshot(desired()))]);
    assert_eq!(reconcile(&mut fake, &desired()), Err(NativeError::Foreign));
    assert_eq!(fake.events, ["inspect", "intent", "inspect"]);
}
#[test]
fn failed_intent_publication_never_registers() {
    let mut fake = Fake::new(vec![None]);
    fake.failure = Some(("intent", NativeError::Unavailable));
    assert_eq!(
        reconcile(&mut fake, &desired()),
        Err(NativeError::Unavailable)
    );
    assert!(!fake.events.contains(&"register"));
}
#[test]
fn ambiguous_native_dispatch_retires_before_another_action() {
    let mut fake = Fake::new(vec![None, None]);
    fake.failure = Some(("register", NativeError::OutcomeUnknown));
    assert_eq!(
        reconcile(&mut fake, &desired()),
        Err(NativeError::OutcomeUnknown)
    );
    assert!(fake.retired);
    assert_eq!(fake.events.last(), Some(&"retire"));
}
#[test]
fn result_publication_failure_after_dispatch_is_unknown_not_rollback() {
    let mut fake = Fake::new(vec![None, None]);
    fake.failure = Some(("result", NativeError::Unavailable));
    assert_eq!(
        reconcile(&mut fake, &desired()),
        Err(NativeError::OutcomeUnknown)
    );
    assert!(fake.events.contains(&"register"));
    assert!(fake.retired);
}
