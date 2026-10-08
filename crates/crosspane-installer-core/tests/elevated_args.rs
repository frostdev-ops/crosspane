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

const OUTCOMES: [Outcome; 7] = [
    Outcome::Done,
    Outcome::AlreadyDone,
    Outcome::RebootRequired,
    Outcome::Refused,
    Outcome::NotElevated,
    Outcome::Mismatch,
    Outcome::Failed,
];

#[test]
fn setup_and_teardown_round_trip_through_parse_render_and_command_line() {
    let cases: [(Verb, &[&str]); 4] = [
        (
            Verb::Setup(rule_scope(PROGRAM)),
            &["setup", "--install-id", ID, "--program", PROGRAM],
        ),
        (
            Verb::Teardown(rule_scope(PROGRAM)),
            &["teardown", "--install-id", ID, "--program", PROGRAM],
        ),
        (
            Verb::Setup(rule_scope(SPACED)),
            &["setup", "--install-id", ID, "--program", SPACED],
        ),
        (
            Verb::Teardown(rule_scope(SPACED)),
            &["teardown", "--install-id", ID, "--program", SPACED],
        ),
    ];
    for (verb, tokens) in cases {
        assert_eq!(parse_arguments(tokens), Ok(verb.clone()), "{tokens:?}");
        assert_eq!(render_arguments(&verb), tokens, "{verb:?}");
        let rendered = render_arguments(&verb);
        let refs: Vec<&str> = rendered.iter().map(String::as_str).collect();
        assert_eq!(parse_arguments(&refs), Ok(verb.clone()), "{refs:?}");
        // The program is the last token; the command line quotes it, as `add-firewall` does.
        let line = format!(
            r#"{} --install-id {ID} --program "{}""#,
            tokens[0], tokens[4]
        );
        assert_eq!(command_line(&verb), line, "{verb:?}");
    }
}

#[test]
fn setup_and_teardown_with_missing_reordered_or_extra_tokens_are_arguments() {
    // Each tail follows the verb. None of them is the complete `--install-id <ID> --program <P>`.
    let tails: &[&[&str]] = &[
        &[],
        &[""],
        &["--install-id", ID],
        &["--install-id", ID, "--program"],
        &["--install-id", "--program", PROGRAM],
        &["--install-id=w41c-vm-1", "--program", PROGRAM],
        &["--id", ID, "--program", PROGRAM],
        &["--program", PROGRAM],
        &["--program", PROGRAM, "--install-id", ID],
        &["--install-id", ID, "--program", PROGRAM, ""],
        &["--install-id", ID, "--program", PROGRAM, "extra"],
        &["--install-id", ID, "--install-id", ID, "--program", PROGRAM],
        &[
            "--install-id",
            ID,
            "--program",
            PROGRAM,
            "--program",
            PROGRAM,
        ],
    ];
    for verb in ["setup", "teardown"] {
        for tail in tails {
            let mut arguments = vec![verb];
            arguments.extend_from_slice(tail);
            assert_eq!(
                parse_arguments(&arguments),
                Err(ElevatedError::Arguments),
                "{arguments:?}"
            );
        }
    }
}

#[test]
fn every_pair_code_is_in_128_to_182_and_splits_back_to_its_pair() {
    assert_eq!(Outcome::pair_exit_code(Outcome::Done, Outcome::Done), 128);
    assert_eq!(
        Outcome::pair_exit_code(Outcome::Failed, Outcome::Failed),
        182
    );
    for first in OUTCOMES {
        for second in OUTCOMES {
            let code = Outcome::pair_exit_code(first, second);
            assert!(
                (128..=182).contains(&code),
                "{first:?} {second:?} -> {code}"
            );
            let unsigned = u32::try_from(code).unwrap();
            assert_eq!(
                Outcome::split_pair_exit_code(unsigned),
                Some((first, second)),
                "{first:?} {second:?} -> {code}"
            );
        }
    }
}

#[test]
fn every_single_verb_code_splits_to_the_same_outcome_twice() {
    for outcome in OUTCOMES {
        let code = u32::try_from(outcome.exit_code()).unwrap();
        assert_eq!(
            Outcome::split_pair_exit_code(code),
            Some((outcome, outcome)),
            "{outcome:?}"
        );
    }
}

#[test]
fn only_single_and_pair_codes_split_and_101_and_300_are_none() {
    let mut valid: Vec<u32> = OUTCOMES
        .iter()
        .map(|outcome| u32::try_from(outcome.exit_code()).unwrap())
        .collect();
    for first in OUTCOMES {
        for second in OUTCOMES {
            valid.push(u32::try_from(Outcome::pair_exit_code(first, second)).unwrap());
        }
    }
    // Every code from 0 to 300 that is neither a single-verb code nor a pair code is `None`.
    for code in 0..=300_u32 {
        assert_eq!(
            Outcome::split_pair_exit_code(code).is_some(),
            valid.contains(&code),
            "{code}"
        );
    }
    assert_eq!(Outcome::split_pair_exit_code(101), None);
    assert_eq!(Outcome::split_pair_exit_code(300), None);
}

#[test]
fn from_random_gives_thirty_two_lowercase_hex_digits_and_a_valid_install_id() {
    let random: [u8; 16] = [
        0x00, 0x01, 0x0f, 0x10, 0x7f, 0x80, 0xab, 0xcd, 0xef, 0xf0, 0xfe, 0xff, 0x09, 0x9a, 0xa0,
        0x5c,
    ];
    let id = InstallId::from_random(random);
    let text = id.as_str();
    assert_eq!(text, "00010f107f80abcdeff0feff099aa05c");
    assert_eq!(text.len(), 32);
    assert!(
        text.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{text:?}"
    );
    assert_eq!(InstallId::parse(text), Ok(id.clone()));
    assert_eq!(
        id.rule_name(),
        format!("Crosspane.Agent.UDP.Private.{text}")
    );
    assert_eq!(InstallId::from_random([0; 16]).as_str(), "0".repeat(32));
}

#[test]
fn local_drive_paths_are_accepted_and_unc_device_share_and_relative_text_are_refused() {
    let accepted = [
        r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        r"d:\Programs\Crosspane\crosspane-agent.exe",
        r"\\?\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        r"\\?\z:\Programs",
    ];
    for text in accepted {
        assert!(is_local_drive_path(text), "{text:?}");
    }
    let refused = [
        r"\\server\share",
        r"\\server\share\Programs\Crosspane\crosspane-agent.exe",
        r"\\?\UNC\server\share\Programs\Crosspane\crosspane-agent.exe",
        r"\\.\C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        r"\\?\",
        r"\\?\C:",
        r"Programs\Crosspane\crosspane-agent.exe",
        r"..\Programs\Crosspane\crosspane-agent.exe",
        r"C:Programs\Crosspane\crosspane-agent.exe",
        r"C:/Users/jame/AppData/Local/Programs/Crosspane/crosspane-agent.exe",
        r"1:\Programs\Crosspane\crosspane-agent.exe",
        "é:\\Programs",
        "C:",
        "",
    ];
    for text in refused {
        assert!(!is_local_drive_path(text), "{text:?}");
    }
}

#[test]
fn combined_verbs_run_their_parts_in_the_frozen_order() {
    let scope = rule_scope(PROGRAM);
    assert_eq!(
        Verb::Setup(scope.clone()).parts(),
        vec![Verb::AddFirewall(scope.clone()), Verb::InstallDriver]
    );
    assert_eq!(
        Verb::Teardown(scope.clone()).parts(),
        vec![Verb::RemoveDriver, Verb::RemoveFirewall(scope.clone())]
    );
    assert!(Verb::Setup(scope.clone()).is_combined());
    assert!(Verb::Teardown(scope.clone()).is_combined());
    // Every other verb is its own single part.
    let single = [
        Verb::Status(None),
        Verb::InstallDriver,
        Verb::RemoveDriver,
        Verb::AddFirewall(scope.clone()),
        Verb::RemoveFirewall(scope),
    ];
    for verb in single {
        assert_eq!(verb.parts(), vec![verb.clone()], "{verb:?}");
        assert!(!verb.is_combined(), "{verb:?}");
    }
}
