#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::*;

const ID: &str = "w41c-vm-1";
const PROGRAM: &str = r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
const SPACED: &str = r"C:\Users\Jane Doe\AppData\Local\Programs\Crosspane\crosspane-agent.exe";

fn rule_scope(program: &str) -> RuleScope {
    RuleScope {
        id: InstallId::parse(ID).unwrap(),
        program: AgentProgram::parse(program).unwrap(),
    }
}

fn parse_with_id(id: &str) -> Result<Verb, ElevatedError> {
    parse_arguments(&["add-firewall", "--install-id", id, "--program", PROGRAM])
}

#[test]
fn six_valid_forms_round_trip_through_parse_and_render() {
    let cases: [(Verb, &[&str]); 6] = [
        (Verb::Status(None), &["status"]),
        (Verb::InstallDriver, &["install-driver"]),
        (Verb::RemoveDriver, &["remove-driver"]),
        (
            Verb::Status(Some(rule_scope(PROGRAM))),
            &["status", "--install-id", ID, "--program", PROGRAM],
        ),
        (
            Verb::AddFirewall(rule_scope(PROGRAM)),
            &["add-firewall", "--install-id", ID, "--program", PROGRAM],
        ),
        (
            Verb::RemoveFirewall(rule_scope(PROGRAM)),
            &["remove-firewall", "--install-id", ID, "--program", PROGRAM],
        ),
    ];
    for (verb, tokens) in cases {
        assert_eq!(parse_arguments(tokens), Ok(verb.clone()), "{tokens:?}");
        assert_eq!(render_arguments(&verb), tokens, "{verb:?}");
        let rendered = render_arguments(&verb);
        let refs: Vec<&str> = rendered.iter().map(String::as_str).collect();
        assert_eq!(parse_arguments(&refs), Ok(verb), "{refs:?}");
    }
}

#[test]
fn wrong_order_duplicate_extra_and_missing_tokens_are_arguments() {
    let bad: &[&[&str]] = &[
        &[],
        &["frobnicate"],
        &["install-driver", "extra"],
        &["install-driver", ""],
        &["remove-driver", "--install-id", ID, "--program", PROGRAM],
        &["status", "extra"],
        &["status", "--program", PROGRAM, "--install-id", ID],
        &["add-firewall", "--program", PROGRAM, "--install-id", ID],
        &[
            "status",
            "--install-id",
            ID,
            "--install-id",
            ID,
            "--program",
            PROGRAM,
        ],
        &[
            "add-firewall",
            "--install-id",
            ID,
            "--program",
            PROGRAM,
            "--program",
            PROGRAM,
        ],
        &["status", "--install-id", ID, "--program", PROGRAM, "extra"],
        &["add-firewall", "--install-id", ID],
        &["add-firewall", "--install-id", ID, "--program"],
        &["remove-firewall", "--program", PROGRAM],
        &["status", "--install-id", ID],
        &["status", "--install-id"],
        &["status", "--install-id", "--program", PROGRAM],
        &["add-firewall", "--id", ID, "--program", PROGRAM],
        &[
            "add-firewall",
            "--install-id=w41c-vm-1",
            "--program",
            PROGRAM,
        ],
    ];
    for arguments in bad {
        assert_eq!(
            parse_arguments(arguments),
            Err(ElevatedError::Arguments),
            "{arguments:?}"
        );
    }
}

#[test]
fn install_ids_of_zero_or_sixty_five_bytes_or_with_underscore_are_rejected() {
    // The frozen bound is 1 to 64 characters of `[A-Za-z0-9-]`.
    let long = "a".repeat(65);
    let bad = ["", "_", "w41c_vm", "w41c vm", "w41c.vm", "é", long.as_str()];
    for id in bad {
        assert_eq!(
            InstallId::parse(id),
            Err(ElevatedError::InstallId),
            "{id:?}"
        );
        assert_eq!(
            InstallId::try_from(id.to_owned()),
            Err(ElevatedError::InstallId),
            "{id:?}"
        );
        assert_eq!(parse_with_id(id), Err(ElevatedError::InstallId), "{id:?}");
    }
}

#[test]
fn install_id_bounds_are_inclusive_and_name_the_rules() {
    let max = "a".repeat(64);
    for id in ["0", "-", "A-z9", ID, max.as_str()] {
        assert!(InstallId::parse(id).is_ok(), "{id:?}");
    }
    let id = InstallId::parse(ID).unwrap();
    assert_eq!(id.as_str(), ID);
    assert_eq!(id.rule_name(), "Crosspane.Agent.UDP.Private.w41c-vm-1");
    assert_eq!(id.rule_group(), "Crosspane.w41c-vm-1");
}

#[test]
fn programs_with_unc_verbatim_relative_dotdot_or_forbidden_characters_are_rejected() {
    let bad = [
        r"\\server\share\Programs\Crosspane\crosspane-agent.exe",
        r"\\?\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        r"\\.\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        r"Programs\Crosspane\crosspane-agent.exe",
        r"C:Programs\Crosspane\crosspane-agent.exe",
        r"1:\Programs\Crosspane\crosspane-agent.exe",
        r"..\Programs\Crosspane\crosspane-agent.exe",
        r"C:\..\Programs\Crosspane\crosspane-agent.exe",
        r"C:\Users\..\Programs\Crosspane\crosspane-agent.exe",
        r"C:\Users\.\Programs\Crosspane\crosspane-agent.exe",
        r"C:\%TEMP%\Programs\Crosspane\crosspane-agent.exe",
        r"C:\*\Programs\Crosspane\crosspane-agent.exe",
        r"C:\a?b\Programs\Crosspane\crosspane-agent.exe",
        r"C:\a<b\Programs\Crosspane\crosspane-agent.exe",
        r"C:\a>b\Programs\Crosspane\crosspane-agent.exe",
        r"C:\a|b\Programs\Crosspane\crosspane-agent.exe",
        r"C:\a:b\Programs\Crosspane\crosspane-agent.exe",
        r"C:/Users/jame/AppData/Local/Programs/Crosspane/crosspane-agent.exe",
        "C:\\\"evil\"\\Programs\\Crosspane\\crosspane-agent.exe",
        // A tab is a control character.
        "C:\\Users\\jame\tx\\Programs\\Crosspane\\crosspane-agent.exe",
        r"C:\Users\jame\\Programs\Crosspane\crosspane-agent.exe",
        r"C:\Users\jame.\Programs\Crosspane\crosspane-agent.exe",
        r"C:\Users\jame \Programs\Crosspane\crosspane-agent.exe",
        r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe.",
        r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.dll",
        r"C:\Users\jame\AppData\Local\Program\Crosspane\crosspane-agent.exe",
        r"C:\Users\jame\crosspane-agent.exe",
        "",
        "C:\\",
    ];
    for text in bad {
        assert_eq!(
            AgentProgram::parse(text),
            Err(ElevatedError::Program),
            "{text:?}"
        );
        assert_eq!(
            AgentProgram::try_from(text.to_owned()),
            Err(ElevatedError::Program),
            "{text:?}"
        );
        assert_eq!(
            parse_arguments(&["add-firewall", "--install-id", ID, "--program", text]),
            Err(ElevatedError::Program),
            "{text:?}"
        );
    }
}

#[test]
fn canonical_and_mixed_case_programs_are_accepted() {
    let good = [
        PROGRAM,
        SPACED,
        r"C:\Programs\Crosspane\crosspane-agent.exe",
        r"c:\Users\JAME\AppData\LOCAL\PROGRAMS\crosspane\Crosspane-Agent.EXE",
    ];
    for text in good {
        assert!(AgentProgram::parse(text).is_ok(), "{text:?}");
        assert!(AgentProgram::try_from(text.to_owned()).is_ok(), "{text:?}");
    }
}

#[test]
fn program_length_bound_is_exactly_32767_bytes() {
    let filler = "a".repeat(MAX_PROGRAM - "C:\\".len() - AGENT_SUFFIX.len());
    let longest = format!("C:\\{filler}{AGENT_SUFFIX}");
    assert_eq!(longest.len(), MAX_PROGRAM);
    assert!(AgentProgram::parse(&longest).is_ok());
    let too_long = format!("C:\\{filler}a{AGENT_SUFFIX}");
    assert_eq!(too_long.len(), MAX_PROGRAM + 1);
    assert_eq!(AgentProgram::parse(&too_long), Err(ElevatedError::Program));
}

#[test]
fn same_path_ignores_ascii_case_and_one_verbatim_prefix() {
    let program = AgentProgram::parse(PROGRAM).unwrap();
    assert!(program.same_path(PROGRAM));
    assert!(
        program
            .same_path(r"\\?\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe")
    );
    assert!(
        program
            .same_path(r"\\?\c:\users\JAME\appdata\local\programs\crosspane\CROSSPANE-AGENT.EXE")
    );
    assert!(
        !program
            .same_path(r"\\?\D:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe")
    );
    assert!(
        !program.same_path(
            r"\\?\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe.bak"
        )
    );
    // Only one leading `\\?\` is stripped.
    assert!(
        !program.same_path(
            r"\\?\\\?\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe"
        )
    );
    assert!(!program.same_path(""));
}

#[test]
fn command_line_quotes_the_program_and_argv_keeps_it_bare() {
    let spaced = Verb::AddFirewall(rule_scope(SPACED));
    assert_eq!(
        command_line(&spaced),
        format!(r#"add-firewall --install-id {ID} --program "{SPACED}""#)
    );
    assert_eq!(render_arguments(&spaced)[4], SPACED);
    assert_eq!(
        command_line(&Verb::Status(Some(rule_scope(PROGRAM)))),
        format!(r#"status --install-id {ID} --program "{PROGRAM}""#)
    );
    assert_eq!(command_line(&Verb::Status(None)), "status");
    assert_eq!(command_line(&Verb::InstallDriver), "install-driver");
    assert_eq!(command_line(&Verb::RemoveDriver), "remove-driver");
}

#[test]
fn exit_codes_round_trip_and_only_three_succeed() {
    let all = [
        (Outcome::Done, 0, true),
        (Outcome::AlreadyDone, 16, true),
        (Outcome::RebootRequired, 17, true),
        (Outcome::Refused, 32, false),
        (Outcome::NotElevated, 33, false),
        (Outcome::Mismatch, 48, false),
        (Outcome::Failed, 64, false),
    ];
    for (outcome, code, succeeded) in all {
        assert_eq!(outcome.exit_code(), code, "{outcome:?}");
        assert_eq!(Outcome::from_exit_code(code as u32), Some(outcome));
        assert_eq!(outcome.succeeded(), succeeded, "{outcome:?}");
    }
    // Every other code, including the panic code 101, is not an outcome.
    for code in 0..=300_u32 {
        let expected = all
            .iter()
            .find(|(_, known, _)| *known as u32 == code)
            .map(|(outcome, _, _)| *outcome);
        assert_eq!(Outcome::from_exit_code(code), expected, "{code}");
    }
    assert_eq!(Outcome::from_exit_code(101), None);
    assert_eq!(Outcome::from_exit_code(u32::MAX), None);
}

#[test]
fn published_names_are_oem_digits_inf_ascii_case_insensitive() {
    for name in [
        "oem1.inf",
        "OEM7.INF",
        "Oem42.Inf",
        "oem00001.inf",
        "oem12345.inf",
    ] {
        assert!(is_published_name(name), "{name:?}");
    }
    let bad = [
        "",
        "oem.inf",
        "oem123456.inf",
        "oem1a.inf",
        "oem-1.inf",
        "oem 1.inf",
        "oem1.ini",
        "oem1.infx",
        "oem1.in",
        "oem1.inf.inf",
        "xem1.inf",
        " oem1.inf",
        "oem1.inf ",
        "oem1.inf\n",
        "oem\u{0661}.inf",
        r"C:\Windows\INF\oem1.inf",
    ];
    for name in bad {
        assert!(!is_published_name(name), "{name:?}");
    }
}

#[test]
fn deserialization_applies_the_same_id_and_program_rules() {
    let id: InstallId = serde_json::from_str(&serde_json::to_string(ID).unwrap()).unwrap();
    assert_eq!(id, InstallId::parse(ID).unwrap());
    let program: AgentProgram =
        serde_json::from_str(&serde_json::to_string(PROGRAM).unwrap()).unwrap();
    assert_eq!(program, AgentProgram::parse(PROGRAM).unwrap());

    let long_id = "a".repeat(65);
    for text in ["x_y", "", long_id.as_str()] {
        let json = serde_json::to_string(text).unwrap();
        assert!(
            serde_json::from_str::<InstallId>(&json).is_err(),
            "{text:?}"
        );
    }
    let json = serde_json::to_string(r"\\?\C:\Programs\Crosspane\crosspane-agent.exe").unwrap();
    assert!(serde_json::from_str::<AgentProgram>(&json).is_err());
}
