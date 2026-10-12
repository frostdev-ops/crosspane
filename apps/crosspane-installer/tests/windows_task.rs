#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! In-memory task fixtures and inert COM definitions; no registered task is read or mutated.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(windows)]
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use native_io::{NativeError, NativeResult};
use service::task::*;
use std::collections::VecDeque;

#[cfg(windows)]
mod native_template {
    use super::*;
    use windows::{
        Win32::System::{Com::*, TaskScheduler::*, Variant::VARIANT},
        core::{BSTR, Interface},
    };

    struct Apartment;
    impl Apartment {
        fn new() -> Self {
            // SAFETY: the test owns this thread's MTA until every interface is released.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .unwrap();
            Self
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances successful initialization on this test thread.
            unsafe { CoUninitialize() };
        }
    }
    fn service() -> ITaskService {
        // SAFETY: owned test MTA, public in-process SDK object; no registered task is selected.
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }.unwrap();
        // SAFETY: local current-token connection only, with no credentials or task operation.
        unsafe {
            service.Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
        }
        .unwrap();
        service
    }
    fn xml(definition: &ITaskDefinition) -> String {
        let mut xml = BSTR::new();
        // SAFETY: owned inert definition, initialized output; no namespace access.
        unsafe { definition.XmlText(&mut xml) }.unwrap();
        xml.to_string()
    }
    fn parse(service: &ITaskService, xml: &str) -> ITaskDefinition {
        // SAFETY: creates an unregistered memory-only definition on this MTA.
        let definition = unsafe { service.NewTask(0) }.unwrap();
        // SAFETY: bounded authored fixture text; this definition is never registered or run.
        unsafe { definition.SetXmlText(&BSTR::from(xml)) }.unwrap();
        definition
    }
    fn fixture() -> (Definition, String) {
        let mut desired = desired();
        desired.principal = "S-1-5-21-101-202-303-1001".into();
        desired.trigger_user = desired.principal.clone();
        (desired, "FIXTURE\\fixture".into())
    }
    fn independent_xml(desired: &Definition, trigger_user: &str) -> String {
        let trigger_user = trigger_user.replace('&', "&amp;").replace('<', "&lt;");
        format!(
            r#"<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><URI>\Crosspane\Agent</URI></RegistrationInfo>
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{trigger_user}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="Author"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><AllowHardTerminate>false</AllowHardTerminate><IdleSettings><Duration>PT10M</Duration><WaitTimeout>PT1H</WaitTimeout><StopOnIdleEnd>true</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings><AllowStartOnDemand>true</AllowStartOnDemand><Enabled>true</Enabled><RunOnlyIfIdle>false</RunOnlyIfIdle><RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable><WakeToRun>false</WakeToRun><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
<Actions Context="Author"><Exec><Command>{}</Command><Arguments>--windows-supervisor</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>"#,
            desired.principal, desired.action, desired.working_directory,
        )
    }

    #[test]
    fn removal_expected_task_uses_trusted_canonical_programs_and_refuses_alias_drift() {
        let _apartment = Apartment::new();
        let service = service();
        let (_, name) = fixture();
        let user = native_io::identity::Sid::from_bytes(vec![
            1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 101, 0, 0, 0, 202, 0, 0, 0, 47, 1, 0, 0, 233, 3,
            0, 0,
        ])
        .unwrap();
        let desired =
            native_io::removal_task_definition(&user, r"\\?\C:\fixture\Programs").unwrap();
        assert_eq!(
            desired.working_directory,
            r"\\?\C:\fixture\Programs\Crosspane"
        );
        let expected =
            xml(
                &native_io::task::template::definition(&service, &desired, &name, &|| Ok(()))
                    .unwrap(),
            );
        let registered = parse(&service, &independent_xml(&desired, &name));
        assert_eq!(xml(&registered), expected);
        for drift in 0..3 {
            let changed = parse(&service, &independent_xml(&desired, &name));
            // SAFETY: getters/setters affect only this unregistered fixture definition.
            unsafe {
                let action = changed
                    .Actions()
                    .unwrap()
                    .get_Item(1)
                    .unwrap()
                    .cast::<IExecAction>()
                    .unwrap();
                match drift {
                    0 => {
                        action
                            .SetPath(&BSTR::from(
                                r"C:\fixture\Programs\Crosspane\crosspane-installer.exe",
                            ))
                            .unwrap();
                        action
                            .SetWorkingDirectory(&BSTR::from(r"C:\fixture\Programs\Crosspane"))
                            .unwrap();
                    }
                    1 => action
                        .SetPath(&BSTR::from(r"\\?\C:\foreign\crosspane-installer.exe"))
                        .unwrap(),
                    _ => action.SetArguments(&BSTR::from("--foreign")).unwrap(),
                }
            }
            assert!(
                xml(&changed) != expected,
                "removal drift {drift} stays foreign"
            );
        }
        assert!(matches!(
            native_io::removal_task_definition(&user, r"C:\fixture\Programs"),
            Err(NativeError::Foreign)
        ));
    }

    #[test]
    fn generated_definition_matches_independent_registered_representation() {
        let _apartment = Apartment::new();
        let service = service();
        let (desired, name) = fixture();
        let actual =
            native_io::task::template::definition(&service, &desired, &name, &|| Ok(())).unwrap();
        let expected = parse(&service, &independent_xml(&desired, &name));
        assert_eq!(xml(&actual), xml(&expected));
        // SAFETY: getters on owned inert definitions, with initialized scalar/string outputs.
        unsafe {
            let mut id = BSTR::new();
            actual.Principal().unwrap().Id(&mut id).unwrap();
            assert_eq!(id.to_string(), "Author");
            let mut context = BSTR::new();
            actual.Actions().unwrap().Context(&mut context).unwrap();
            assert_eq!(context.to_string(), "Author");
        }
    }

    #[test]
    fn whole_template_comparison_refuses_foreign_identity_and_unrelated_drift() {
        let _apartment = Apartment::new();
        let service = service();
        let (desired, name) = fixture();
        let expected =
            xml(
                &native_io::task::template::definition(&service, &desired, &name, &|| Ok(()))
                    .unwrap(),
            );
        for drift in 0..8 {
            let changed = parse(&service, &independent_xml(&desired, &name));
            // SAFETY: changes only this test-owned unregistered definition; no task namespace API.
            unsafe {
                match drift {
                    0 => changed
                        .Principal()
                        .unwrap()
                        .SetUserId(&BSTR::from("S-1-5-21-7-8-9-1002"))
                        .unwrap(),
                    1 => changed
                        .Triggers()
                        .unwrap()
                        .get_Item(1)
                        .unwrap()
                        .cast::<ILogonTrigger>()
                        .unwrap()
                        .SetUserId(&BSTR::from("S-1-5-21-7-8-9-1002"))
                        .unwrap(),
                    2 => changed.Settings().unwrap().SetPriority(5).unwrap(),
                    3 => changed
                        .RegistrationInfo()
                        .unwrap()
                        .SetAuthor(&BSTR::from("fixture foreign author"))
                        .unwrap(),
                    4 => {
                        changed
                            .Principal()
                            .unwrap()
                            .SetId(&BSTR::from("Other"))
                            .unwrap();
                        changed
                            .Actions()
                            .unwrap()
                            .SetContext(&BSTR::from("Other"))
                            .unwrap();
                    }
                    5 => changed
                        .Actions()
                        .unwrap()
                        .get_Item(1)
                        .unwrap()
                        .cast::<IExecAction>()
                        .unwrap()
                        .SetArguments(&BSTR::from("--foreign"))
                        .unwrap(),
                    6 => changed
                        .Actions()
                        .unwrap()
                        .get_Item(1)
                        .unwrap()
                        .cast::<IExecAction>()
                        .unwrap()
                        .SetPath(&BSTR::from("C:\\fixture\\other.exe"))
                        .unwrap(),
                    _ => changed
                        .RegistrationInfo()
                        .unwrap()
                        .SetDate(&BSTR::from("2026-10-12T12:00:00"))
                        .unwrap(),
                }
            }
            assert!(
                xml(&changed) != expected,
                "drift {drift} must remain foreign"
            );
        }
    }

    #[test]
    fn template_checks_refuse_before_inert_definition_creation() {
        let _apartment = Apartment::new();
        let service = service();
        let (desired, name) = fixture();
        assert!(matches!(
            native_io::task::template::definition(&service, &desired, &name, &|| Err(
                NativeError::Foreign
            )),
            Err(NativeError::Foreign)
        ));
    }
}

#[test]
fn current_xml_principal_sid_allows_its_observed_bare_account_name() {
    assert_eq!(
        native_io::task::normalize_account_ids(
            "alex",
            "LOCAL\\alex",
            "S-1-5-21-101",
            Some("LOCAL\\alex"),
            true,
        ),
        Ok(("S-1-5-21-101".into(), "S-1-5-21-101".into()))
    );
    assert_eq!(
        native_io::task::normalize_account_ids(
            "S-1-5-21-101",
            "S-1-5-21-101",
            "S-1-5-21-101",
            None,
            true,
        ),
        Ok(("S-1-5-21-101".into(), "S-1-5-21-101".into()))
    );
}

#[test]
fn foreign_xml_principal_sid_rejects_the_same_bare_name_before_task_mutation() {
    let observed = native_io::task::normalize_account_ids(
        "alex",
        "LOCAL\\alex",
        "S-1-5-21-101",
        Some("LOCAL\\alex"),
        false,
    );
    assert_eq!(observed, Err(NativeError::Foreign));
    let mut fake = Fake::new(Vec::new());
    fake.views
        .push_back(observed.map(|(principal, trigger_user)| {
            let mut current = desired();
            current.principal = principal;
            current.trigger_user = trigger_user;
            Some(snapshot(current))
        }));
    assert_eq!(reconcile(&mut fake, &desired()), Err(NativeError::Foreign));
    assert_eq!(fake.events, ["inspect"]);
}

#[test]
fn foreign_qualified_principal_or_trigger_is_not_mapped_to_the_current_sid() {
    for (principal, trigger_user) in [("DOMAIN\\alex", "LOCAL\\alex"), ("alex", "DOMAIN\\alex")] {
        assert_eq!(
            native_io::task::normalize_account_ids(
                principal,
                trigger_user,
                "S-1-5-21-101",
                Some("LOCAL\\alex"),
                true,
            ),
            Err(NativeError::Foreign)
        );
    }
}

#[test]
fn bare_trigger_name_or_unresolved_principal_name_never_grants_account_identity() {
    for (principal, trigger_user, own_name) in [
        ("alex", "alex", Some("LOCAL\\alex")),
        ("alex", "S-1-5-21-101", None),
    ] {
        assert_eq!(
            native_io::task::normalize_account_ids(
                principal,
                trigger_user,
                "S-1-5-21-101",
                own_name,
                true,
            ),
            Err(NativeError::Foreign)
        );
    }
}

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
