#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

// Runtime tests are owned by WP-4.8c and will be added in a separate runtime_tests block.
mod session_tests {
    use crosspane_installer::agent_contract::*;
    use crosspane_installer::platform::linux::detect::*;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    fn known<T>(value: T) -> Fact<T> {
        Fact::known(value, ObservationSource::Demo, 17)
    }
    fn candidate(kind: &str) -> SessionCandidate {
        SessionCandidate {
            id: "c7".into(),
            path: "/org/freedesktop/login1/session/c7".into(),
            kind: Some(kind.into()),
            uid: Some(1000),
            seat: Some("seat0".into()),
            active: Some(true),
            locked_hint: Some(false),
        }
    }
    fn environment() -> EffectiveEnvironment {
        EffectiveEnvironment {
            runtime_dir: "/run/user/1000".into(),
            wayland_display: "wayland-2".into(),
            hyprland_instance_signature: "test_1790950000".into(),
            session_id: None,
        }
    }
    fn session() -> SessionFacts {
        SessionFacts {
            uid: 1000,
            os: known(OsFamily::Arch),
            architecture: known(Architecture::X86_64),
            hyprland_version: known([0, 56, 0]),
            protocols: known(true),
            uwsm_managed: known(true),
            graphical_target_active: known(true),
            graphical_sessions: known(1),
            selected_session: known(Some(SelectedSession {
                selection: SessionSelection::Pid,
                session: candidate("wayland"),
            })),
            selected_environment: environment(),
            manager_environment: known(environment()),
        }
    }
    fn runtime() -> RuntimeFacts {
        RuntimeFacts {
            libraries: vec![LibraryFact {
                name: "libavcodec.so.62".into(),
                required: true,
                resolved: known(PathBuf::from("/usr/lib/libavcodec.so.62")),
            }],
            video_feature: known(true),
            ffmpeg: known(true),
            opus: known(true),
            pipewire_library: known(true),
            xkb: known(true),
            wayland_library: known(true),
            software_video: known(true),
            gpu: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 18),
            libei_required: false,
            pipewire: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 19),
            session_manager: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 20),
            secret_service: Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 21),
            keystore: known(KeyStoreProvenance::OsStore),
        }
    }
    struct Lookup {
        own: Result<Option<SessionCandidate>, ProbeIssue>,
        named: Result<Option<SessionCandidate>, ProbeIssue>,
        display: Result<DisplaySessions, ProbeIssue>,
        calls: Vec<String>,
    }
    impl Default for Lookup {
        fn default() -> Self {
            Self {
                own: Ok(None),
                named: Ok(None),
                display: Ok(DisplaySessions {
                    display: "/".into(),
                    sessions: vec![],
                }),
                calls: vec![],
            }
        }
    }
    impl SessionLookup for Lookup {
        fn own_session(&mut self) -> Result<Option<SessionCandidate>, ProbeIssue> {
            self.calls.push("own".into());
            self.own.clone()
        }
        fn named_session(&mut self, id: &str) -> Result<Option<SessionCandidate>, ProbeIssue> {
            self.calls.push(format!("named:{id}"));
            self.named.clone()
        }
        fn display_sessions(&mut self, uid: u32) -> Result<DisplaySessions, ProbeIssue> {
            self.calls.push(format!("display:{uid}"));
            self.display.clone()
        }
    }
    fn display_reader(sessions: Vec<SessionCandidate>) -> Lookup {
        Lookup {
            display: Ok(DisplaySessions {
                display: candidate("wayland").path,
                sessions,
            }),
            ..Lookup::default()
        }
    }
    fn bus_error(name: &str) -> zbus::Error {
        let call = zbus::Message::method_call("/org/freedesktop/login1", "GetSessionByPID")
            .unwrap()
            .build(&(4242u32,))
            .unwrap();
        let reply = zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&"test error")
            .unwrap();
        zbus::Error::MethodError(name.try_into().unwrap(), Some("test error".into()), reply)
    }

    #[test]
    fn facts_preserve_original_source_receipt_and_all_bounded_issues() {
        for issue in [
            ProbeIssue::Missing,
            ProbeIssue::WrongVersion,
            ProbeIssue::Unavailable,
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Oversize,
            ProbeIssue::Malformed,
            ProbeIssue::Foreign,
            ProbeIssue::Ambiguous,
            ProbeIssue::Unverified,
        ] {
            let fact = Fact::<bool>::issue(issue, ObservationSource::Live, u64::MAX);
            assert_eq!(fact.value, Err(issue));
            assert_eq!(fact.source, ObservationSource::Live);
            assert_eq!(fact.observed_at_ms, u64::MAX);
        }
        assert_eq!(known(false).value, Ok(false));
    }

    #[test]
    fn os_release_arch_family_is_token_exact_and_other_os_is_known() {
        for bytes in [
            b"ID=arch\n".as_slice(),
            b"ID='endeavouros'\nID_LIKE=\"arch linux\"\n",
            b"# producer comment\nID=manjaro\nID_LIKE=arch\n",
        ] {
            assert_eq!(parse_os_release(bytes), Ok(OsFamily::Arch));
        }
        for bytes in [
            b"ID=debian\nID_LIKE=archlinux\n".as_slice(),
            b"ID=debian\nID_LIKE=debian\n",
        ] {
            assert_eq!(
                parse_os_release(bytes),
                Ok(OsFamily::Other("debian".into()))
            );
        }
    }

    #[test]
    fn os_release_malformed_duplicate_oversize_and_utf8_never_default_arch() {
        for bytes in [
            b"".as_slice(),
            b"ID=arch\nID=arch",
            b"ID=\"arch",
            b"ID=",
            b"ID=a/b",
            b"ID=arch\nBROKEN",
            b"ID=\xff",
            b"ID=arch\nA=one\0two",
        ] {
            assert_eq!(
                parse_os_release(bytes),
                Err(ProbeIssue::Malformed),
                "{bytes:?}"
            );
        }
        assert_eq!(
            parse_os_release(&vec![b'a'; MAX_PROBE_BYTES + 1]),
            Err(ProbeIssue::Oversize)
        );
        assert!(parse_os_release(format!("ID={}\n", "a".repeat(65)).as_bytes()).is_err());
        assert!(parse_os_release(format!("ID=arch\nX={}\n", "a".repeat(4097)).as_bytes()).is_err());
        assert!(parse_os_release(format!("ID=arch\n{}=x\n", "A".repeat(129)).as_bytes()).is_err());
        let many = format!(
            "ID=arch\n{}",
            (0..256).map(|i| format!("X{i}=y\n")).collect::<String>()
        );
        assert_eq!(
            parse_os_release(many.as_bytes()),
            Err(ProbeIssue::Malformed)
        );
    }

    #[test]
    fn os_release_rejects_broken_quotes_in_any_assignment_and_every_identifier() {
        for assignment in [
            r#"ID_LIKE="arch "broken""#,
            r#"ID_LIKE="arch "broken"""#,
            r#"ID_LIKE='arch 'broken'"#,
            r#"ID_LIKE="arch""linux""#,
            r#"ID_LIKE=arch"broken"#,
            r#"ID_LIKE="arch"suffix"#,
            r#"NAME="Arch "broken""#,
            r#"NAME='Arch"#,
            r#"NAME=Arch\"#,
            r#"NAME=$OS"#,
            r#"NAME="${OS}""#,
            "NAME=`command`",
            "NAME=two words",
            "NAME=Arch;command",
            "1NAME=Arch",
            "NAME=Arch\u{b}",
        ] {
            let bytes = format!("ID=arch\n{assignment}\n");
            assert_eq!(
                parse_os_release(bytes.as_bytes()),
                Err(ProbeIssue::Malformed),
                "{assignment}"
            );
            let mut s = session();
            s.os = Fact {
                value: parse_os_release(bytes.as_bytes()),
                source: ObservationSource::Demo,
                observed_at_ms: 17,
            };
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Malformed)
            );
        }
        for key in ["ID", "ID_LIKE"] {
            for invalid in [
                "ARCH",
                "arch/other",
                "arch+other",
                "árch",
                "arch$",
                "arch\"",
                "a;b",
                &"a".repeat(65),
            ] {
                let bytes = if key == "ID" {
                    format!("ID='{invalid}'\nID_LIKE=arch\n")
                } else {
                    format!("ID=arch\nID_LIKE='arch {invalid}'\n")
                };
                assert_eq!(
                    parse_os_release(bytes.as_bytes()),
                    Err(ProbeIssue::Malformed),
                    "{key}: {invalid}"
                );
            }
        }
    }

    #[test]
    fn os_release_shell_escapes_and_quotes_decode_without_expansion_or_concatenation() {
        for bytes in [
            r#"ID=\a\r\c\h
NAME="Arch \"Linux\" \$OS \`literal\` \\path \q"
"#,
            r#"ID='arch'
NAME='Owner\path $OS `literal` "quoted"'
"#,
            "ID=arch\nID_LIKE=\"linux arch\"\nNAME=\"Αrch Linux; example\"\n",
            "ID=arch\nID_LIKE=\"\"\nNAME=''\n",
        ] {
            assert_eq!(
                parse_os_release(bytes.as_bytes()),
                Ok(OsFamily::Arch),
                "{bytes}"
            );
        }
        assert_eq!(
            parse_os_release(b"ID=vendor.os_1-2\nID_LIKE='linux other.os_2-3'\n"),
            Ok(OsFamily::Other("vendor.os_1-2".into()))
        );
        // Escapes that shell double quotes do not consume remain literal, hence invalid IDs.
        for bytes in [
            br#"ID="\arch""#.as_slice(),
            br#"ID='\arch'"#,
            br#"ID=debian
ID_LIKE="arch \linux""#,
        ] {
            assert_eq!(parse_os_release(bytes), Err(ProbeIssue::Malformed));
        }
        let escaped = MANAGER.replace("wayland-2", r#""wayland\-2""#);
        assert_eq!(
            parse_manager_environment(escaped.as_bytes()),
            Err(ProbeIssue::Malformed)
        );
        let escaped = MANAGER.replace("wayland-2", r#"wayland\-2"#);
        assert_eq!(
            parse_manager_environment(escaped.as_bytes()),
            Ok(environment())
        );
    }

    #[test]
    fn shell_word_decoding_preserves_exact_literals_and_double_quote_backslashes() {
        for (encoded, decoded) in [
            (r#"/run/\$x\`y\`\\path"#, r#"/run/$x`y`\path"#),
            (
                r#""/run/\$x\`y\`\\back\"quote'\q""#,
                r#"/run/$x`y`\back"quote'\q"#,
            ),
            (r#"'/run/$x`y`"quote"\back'"#, r#"/run/$x`y`"quote"\back"#),
        ] {
            let environment =
                parse_manager_environment(MANAGER.replace("/run/user/1000", encoded).as_bytes())
                    .unwrap();
            assert_eq!(environment.runtime_dir, PathBuf::from(decoded), "{encoded}");
        }
    }

    #[test]
    fn architectures_are_explicit_not_host_guesses() {
        assert_eq!(parse_architecture("x86_64"), Ok(Architecture::X86_64));
        assert_eq!(parse_architecture("aarch64"), Ok(Architecture::Aarch64));
        assert_eq!(
            parse_architecture("riscv64"),
            Ok(Architecture::Other("riscv64".into()))
        );
        for value in ["", "a\nb", &"x".repeat(33)] {
            assert_eq!(parse_architecture(value), Err(ProbeIssue::Malformed));
        }
    }

    const MANAGER: &str = "XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-2\nHYPRLAND_INSTANCE_SIGNATURE=test_1790950000\n";
    #[test]
    fn manager_environment_returns_only_selected_fields_without_repair() {
        assert_eq!(
            parse_manager_environment(MANAGER.as_bytes()),
            Ok(environment())
        );
        let bytes = format!(
            "{MANAGER}XDG_SESSION_ID='c7'\nSECRET=sentinel-secret\nDBUS_SESSION_BUS_ADDRESS=sentinel-address\n"
        );
        let mut expected = environment();
        expected.session_id = Some("c7".into());
        let decoded = parse_manager_environment(bytes.as_bytes()).unwrap();
        assert_eq!(decoded, expected);
        assert!(!format!("{decoded:?}").contains("sentinel"));
        assert_eq!(
            parse_manager_environment(
                MANAGER
                    .replace("/run/user/1000", "\"/run/user/1000\"")
                    .as_bytes()
            ),
            Ok(environment())
        );
    }

    #[test]
    fn manager_missing_invalid_alias_duplicate_and_bounds_are_pending_facts() {
        for key in [
            "XDG_RUNTIME_DIR",
            "WAYLAND_DISPLAY",
            "HYPRLAND_INSTANCE_SIGNATURE",
        ] {
            let bytes = MANAGER
                .lines()
                .filter(|line| !line.starts_with(key))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                parse_manager_environment(bytes.as_bytes()),
                Err(ProbeIssue::Missing)
            );
        }
        for path in [
            "relative",
            "/run//user/1000",
            "/run/./1000",
            "/run/../1000",
            "/run/user/1000/",
        ] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("/run/user/1000", path).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for name in ["", ".", "..", "a/b", "a b", &"a".repeat(129)] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("wayland-2", name).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for signature in ["", ".", "..", "../other", &"a".repeat(257)] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("test_1790950000", signature).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for extra in [
            "WAYLAND_DISPLAY=wayland-2\n",
            "XDG_SESSION_ID=\n",
            &format!("XDG_SESSION_ID={}\n", "x".repeat(65)),
        ] {
            assert_eq!(
                parse_manager_environment(format!("{MANAGER}{extra}").as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        assert_eq!(
            parse_manager_environment(&vec![b'a'; MAX_PROBE_BYTES + 1]),
            Err(ProbeIssue::Oversize)
        );
    }

    #[test]
    fn own_pid_graphical_selection_wins_even_inactive_locked_or_x11() {
        for kind in ["wayland", "x11"] {
            let mut own = candidate(kind);
            own.active = Some(false);
            own.locked_hint = Some(true);
            let mut reader = Lookup {
                own: Ok(Some(own.clone())),
                named: Err(ProbeIssue::Foreign),
                ..Lookup::default()
            };
            assert_eq!(
                choose_session(&mut reader, 1000, Some("other")),
                Ok(Some(SelectedSession {
                    selection: SessionSelection::Pid,
                    session: own
                }))
            );
            assert_eq!(reader.calls, ["own"]);
        }
    }

    #[test]
    fn environment_fallback_is_ordered_and_never_compositor_pid_membership() {
        for own in [
            None,
            Some(candidate("tty")),
            Some(SessionCandidate {
                uid: Some(1001),
                ..candidate("wayland")
            }),
        ] {
            let named = candidate("wayland");
            let mut reader = Lookup {
                own: Ok(own),
                named: Ok(Some(named.clone())),
                display: Err(ProbeIssue::Foreign),
                calls: vec![],
            };
            assert_eq!(
                choose_session(&mut reader, 1000, Some("c7")),
                Ok(Some(SelectedSession {
                    selection: SessionSelection::Environment,
                    session: named
                }))
            );
            assert_eq!(reader.calls, ["own", "named:c7"]);
        }
    }

    #[test]
    fn absent_or_empty_environment_uses_only_sole_seated_display_candidate() {
        for id in [None, Some("")] {
            for kind in ["wayland", "x11"] {
                let mut displayed = candidate(kind);
                displayed.active = Some(false);
                let mut reader = display_reader(vec![displayed.clone()]);
                assert_eq!(
                    choose_session(&mut reader, 1000, id),
                    Ok(Some(SelectedSession {
                        selection: SessionSelection::Display,
                        session: displayed
                    }))
                );
                assert_eq!(reader.calls, ["own", "display:1000"]);
            }
        }
        let mut reader = display_reader(vec![candidate("wayland")]);
        assert!(
            choose_session(&mut reader, 1000, Some("c7"))
                .unwrap()
                .is_some()
        );
        assert_eq!(reader.calls, ["own", "named:c7", "display:1000"]);
    }

    #[test]
    fn zero_multiple_foreign_unseated_or_wrong_display_never_choose_active_guess() {
        for sessions in [
            vec![],
            vec![candidate("tty")],
            vec![SessionCandidate {
                uid: Some(1001),
                ..candidate("wayland")
            }],
            vec![SessionCandidate {
                seat: Some("".into()),
                ..candidate("wayland")
            }],
        ] {
            let mut reader = display_reader(sessions);
            assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
        }
        let mut second = candidate("x11");
        second.id = "c8".into();
        second.path = "/org/freedesktop/login1/session/c8".into();
        let mut reader = display_reader(vec![candidate("wayland"), second]);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Ambiguous)
        );
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "/other".into();
        assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "/".into();
        assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
    }

    #[test]
    fn any_listed_unreadable_type_user_or_seat_blocks_display_even_for_other_session() {
        for field in ["type", "user", "seat"] {
            let mut other = candidate("tty");
            other.uid = Some(1001);
            match field {
                "type" => other.kind = None,
                "user" => other.uid = None,
                "seat" => other.seat = None,
                _ => unreachable!(),
            }
            let mut reader = display_reader(vec![candidate("wayland"), other]);
            assert_eq!(
                choose_session(&mut reader, 1000, None),
                Err(ProbeIssue::Unverified),
                "{field}"
            );
        }
    }

    #[test]
    fn errors_stop_at_exact_step_and_selection_bounds_do_not_probe_further() {
        for issue in [
            ProbeIssue::Unavailable,
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Malformed,
        ] {
            let mut reader = Lookup {
                own: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, Some("c7")), Err(issue));
            assert_eq!(reader.calls, ["own"]);
            let mut reader = Lookup {
                named: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, Some("c7")), Err(issue));
            assert_eq!(reader.calls, ["own", "named:c7"]);
            let mut reader = Lookup {
                display: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, None), Err(issue));
            assert_eq!(reader.calls, ["own", "display:1000"]);
        }
        for id in ["bad\nname", &"x".repeat(65)] {
            let mut reader = Lookup::default();
            assert_eq!(
                choose_session(&mut reader, 1000, Some(id)),
                Err(ProbeIssue::Malformed)
            );
            assert_eq!(reader.calls, ["own"]);
        }
        let mut reader = display_reader(vec![candidate("tty"); MAX_SESSIONS + 1]);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Oversize)
        );
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "x".repeat(513);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Malformed)
        );
    }

    #[test]
    fn only_exact_logind_absence_method_errors_permit_fallback() {
        let no_pid = "org.freedesktop.login1.NoSessionForPID";
        let no_named = "org.freedesktop.login1.NoSuchSession";
        assert_eq!(
            lookup_reply(Err(bus_error(no_pid)), SessionSelection::Pid),
            Ok(None)
        );
        assert_eq!(
            lookup_reply(Err(bus_error(no_named)), SessionSelection::Environment),
            Ok(None)
        );
        for (name, step) in [
            (no_pid, SessionSelection::Environment),
            (no_named, SessionSelection::Pid),
            (no_pid, SessionSelection::Display),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                SessionSelection::Pid,
            ),
            (
                "org.freedesktop.login1.NoSessionForPID.extra",
                SessionSelection::Pid,
            ),
        ] {
            assert_eq!(
                lookup_reply(Err(bus_error(name)), step),
                Err(ProbeIssue::Unavailable)
            );
        }
        assert_eq!(
            lookup_reply(
                Err(zbus::Error::Failure("unavailable".into())),
                SessionSelection::Pid
            ),
            Err(ProbeIssue::Unavailable)
        );
        let path = "/org/freedesktop/login1/session/c7";
        assert_eq!(
            lookup_reply(Ok(path.try_into().unwrap()), SessionSelection::Pid),
            Ok(Some(path.into()))
        );
    }

    #[test]
    fn supported_structural_report_ignores_optional_gpu_audio_store_availability() {
        let s = session();
        let mut r = runtime();
        assert_eq!(classify(&s, &r), Eligibility::Supported);
        r.gpu = known(false);
        r.pipewire = known(false);
        r.session_manager = known(false);
        r.secret_service = known(false);
        r.keystore = known(KeyStoreProvenance::File);
        assert_eq!(classify(&s, &r), Eligibility::Supported);
        let report = SupportReport {
            eligibility: classify(&s, &r),
            session: s,
            runtime: r,
            installed_agent: Fact::issue(ProbeIssue::Missing, ObservationSource::Demo, 44),
            reduced_motion: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 45),
        };
        assert_eq!(report.runtime.keystore.value, Ok(KeyStoreProvenance::File));
        assert_eq!(report.reduced_motion.value, Err(ProbeIssue::Unverified));
        assert_eq!(report.installed_agent.value, Err(ProbeIssue::Missing));
    }

    #[test]
    fn each_known_support_mismatch_has_its_exact_reason_and_unknown_stays_pending() {
        for (field, reason) in [
            ("os", UnsupportedReason::OperatingSystem),
            ("arch", UnsupportedReason::Architecture),
            ("version", UnsupportedReason::HyprlandVersion),
            ("protocol", UnsupportedReason::RequiredProtocols),
            ("uwsm", UnsupportedReason::Uwsm),
        ] {
            let mut s = session();
            match field {
                "os" => s.os = known(OsFamily::Other("debian".into())),
                "arch" => s.architecture = known(Architecture::Other("riscv64".into())),
                "version" => s.hyprland_version = known([0, 55, 99]),
                "protocol" => s.protocols = known(false),
                "uwsm" => s.uwsm_managed = known(false),
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::NotSupported(reason),
                "{field}"
            );
            match field {
                "os" => s.os = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1),
                "arch" => {
                    s.architecture =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "version" => {
                    s.hyprland_version =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "protocol" => {
                    s.protocols = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "uwsm" => {
                    s.uwsm_managed =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unavailable),
                "{field}"
            );
        }
        for arch in [Architecture::X86_64, Architecture::Aarch64] {
            let mut s = session();
            s.architecture = known(arch);
            for version in [[0, 56, 0], [0, 57, 0], [1, 0, 0]] {
                s.hyprland_version = known(version);
                assert_eq!(classify(&s, &runtime()), Eligibility::Supported);
            }
        }
    }

    #[test]
    fn target_and_environment_alone_never_prove_uwsm_and_known_negative_precedes_pending() {
        let mut s = session();
        s.uwsm_managed = Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 42);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
        s.os = Fact::issue(ProbeIssue::Timeout, ObservationSource::Demo, 43);
        s.protocols = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::RequiredProtocols)
        );
        s.protocols = known(true);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Timeout)
        );
    }

    #[test]
    fn every_required_runtime_candidate_is_distinct_from_optional_or_unknown() {
        for field in [
            "video", "ffmpeg", "opus", "pipewire", "xkb", "wayland", "software",
        ] {
            let mut r = runtime();
            let fact = match field {
                "video" => &mut r.video_feature,
                "ffmpeg" => &mut r.ffmpeg,
                "opus" => &mut r.opus,
                "pipewire" => &mut r.pipewire_library,
                "xkb" => &mut r.xkb,
                "wayland" => &mut r.wayland_library,
                "software" => &mut r.software_video,
                _ => unreachable!(),
            };
            *fact = known(false);
            assert_eq!(
                classify(&session(), &r),
                Eligibility::NotSupported(if field == "video" {
                    UnsupportedReason::VideoFeature
                } else {
                    UnsupportedReason::RuntimeLibrary
                }),
                "{field}"
            );
            let fact = match field {
                "video" => &mut r.video_feature,
                "ffmpeg" => &mut r.ffmpeg,
                "opus" => &mut r.opus,
                "pipewire" => &mut r.pipewire_library,
                "xkb" => &mut r.xkb,
                "wayland" => &mut r.wayland_library,
                "software" => &mut r.software_video,
                _ => unreachable!(),
            };
            *fact = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 46);
            assert_eq!(
                classify(&session(), &r),
                Eligibility::Pending(ProbeIssue::Unavailable),
                "{field}"
            );
        }
        let mut r = runtime();
        r.libei_required = true;
        assert_eq!(
            classify(&session(), &r),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
    }

    #[test]
    fn unavailable_library_is_not_fabricated_missing_or_wrong_version() {
        for issue in [
            ProbeIssue::Missing,
            ProbeIssue::WrongVersion,
            ProbeIssue::Unavailable,
            ProbeIssue::Malformed,
            ProbeIssue::Timeout,
        ] {
            let mut r = runtime();
            r.libraries[0].resolved = Fact::issue(issue, ObservationSource::Demo, 41);
            assert_eq!(
                classify(&session(), &r),
                if matches!(issue, ProbeIssue::Missing | ProbeIssue::WrongVersion) {
                    Eligibility::NotSupported(UnsupportedReason::RuntimeLibrary)
                } else {
                    Eligibility::Pending(issue)
                }
            );
            r.libraries[0].required = false;
            assert_eq!(
                classify(&session(), &r),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
            r.libraries.push(runtime().libraries.remove(0));
            assert_eq!(classify(&session(), &r), Eligibility::Supported);
        }
    }

    #[test]
    fn producer_selection_and_mutation_eligibility_remain_separate() {
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .kind = Some("x11".into());
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::SessionType)
        );
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .kind = None;
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
        for active in [Some(false), None] {
            let mut s = session();
            s.selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .active = active;
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
        }
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .locked_hint = Some(true);
        assert_eq!(classify(&s, &runtime()), Eligibility::Supported); // lock is readiness/gate, not support
        for seat in [None, Some("".into())] {
            let mut s = session();
            s.selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .seat = seat;
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
        }
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .uid = Some(1001);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Foreign)
        );
        s.selected_session = known(None);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Ambiguous)
        );
    }

    #[test]
    fn manager_runtime_display_signature_mismatch_and_ambiguous_or_unknown_lifecycle_refuse() {
        for field in ["runtime", "display", "signature"] {
            let mut s = session();
            let manager = s.manager_environment.value.as_mut().unwrap();
            match field {
                "runtime" => manager.runtime_dir = "/run/user/1001".into(),
                "display" => manager.wayland_display = "wayland-other".into(),
                "signature" => manager.hyprland_instance_signature = "other".into(),
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Foreign),
                "{field}"
            );
        }
        for count in [0, 2, 3] {
            let mut s = session();
            s.graphical_sessions = known(count);
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Ambiguous)
            );
        }
        for issue in [
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Unavailable,
            ProbeIssue::Malformed,
        ] {
            let mut s = session();
            s.manager_environment = Fact::issue(issue, ObservationSource::Demo, 2);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
            let mut s = session();
            s.graphical_target_active = Fact::issue(issue, ObservationSource::Demo, 3);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
            let mut s = session();
            s.graphical_sessions = Fact::issue(issue, ObservationSource::Demo, 4);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
        }
        let mut s = session();
        s.graphical_target_active = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
    }

    const BOOTSTRAP: &[u8] = br#"{"schema_version":1,"instance_id":18446744073709551615,"pid":4242,
      "started_unix_ms":1790950000000,"phase":"waiting_for_keystore","phase_seq":2,
      "keystore":null,"reason":null,"runtime_dir":"/run/user/1000/crosspane"}"#;
    const STATUS: &[u8] = br#"{"ok":true,"result":{
      "controlling":null,"controlled_by":null,"projections":[],"displays":[],"peers":[],"layout":[],
      "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
      "instance":{"id":18446744073709551615,"pid":4242,"uid":1000,"exe":"/home/u/.local/bin/crosspane-agent",
      "runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790950000000},
      "config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
      "recovery_pending":0,"startup_recovery":"failed",
      "gate":{"open":false,"session":"unknown","active":null,"armed":false,"panic":true},
      "epochs":{"gate":7,"grants":3,"layout":2,"backends":1},"backends":[
      {"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
      {"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
      {"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
      {"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
      {"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
      {"name":"links","state":"ready","reason":null},{"name":"gpu","state":"missing","reason":"disabled"},
      {"name":"home","state":"blocked","reason":"not_supported"},{"name":"audio","state":"failed","reason":"worker_exited"},
      {"name":"discovery","state":"ready","reason":null}],"keystore":"file","permissions":[],
      "discovery":{"enabled":true,"running":false,"candidates":0,"error":null},"tray":{"created":false},
      "audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
    fn agent_facts(
        status: &[u8],
        source: ObservationSource,
        time: u64,
    ) -> Result<InstalledAgentFacts, ProbeIssue> {
        InstalledAgentFacts::from_reply(
            parse_bootstrap(BOOTSTRAP).unwrap(),
            AgentReply {
                id: 999,
                observed_at_ms: time,
                source,
                result: Ok(DecodedReply::Status(
                    parse_status(status, AgentPlatform::Linux).unwrap(),
                )),
            },
        )
    }

    #[test]
    fn installed_wire_facts_preserve_failed_recovery_zero_pending_wait_and_loaded_revision() {
        let facts = agent_facts(STATUS, ObservationSource::Live, 300).unwrap();
        assert_eq!(facts.call_id, 999);
        assert_eq!(facts.received_at_ms, 300);
        assert_eq!(facts.source, ObservationSource::Live);
        assert_eq!(facts.bootstrap.phase, BootstrapPhase::WaitingForKeystore);
        assert_eq!(facts.bootstrap.keystore, None);
        let StatusAdmission::Supported(health) = facts.status else {
            panic!("literal complete contract expected");
        };
        let installer = health.installer();
        assert_eq!(installer.startup_recovery, StartupRecovery::Failed);
        assert_eq!(installer.recovery_pending, 0);
        assert_eq!(installer.config_revision, "9f86d081884c7d65");
        assert_ne!(installer.config_revision, "edited-disk-revision");
        assert_eq!(installer.keystore, KeyStoreProvenance::File);
        assert!(installer.permissions.is_empty());
        assert_eq!(installer.gate.session, SessionState::Unknown);
        assert_eq!(installer.gate.active, None);
        assert!(!installer.gate.open);
        assert!(installer.gate.panic);
        assert_eq!(installer.backends[11].name, BackendName::Gpu);
        assert_eq!(installer.backends[11].state, BackendState::Missing);
        assert_eq!(installer.backends[12].name, BackendName::Home);
        assert_eq!(installer.backends[12].state, BackendState::Blocked);
        assert_eq!(installer.backends[13].name, BackendName::Audio);
        assert_eq!(installer.backends[13].state, BackendState::Failed);
        assert_eq!(
            installer.backends[13].reason,
            Some(BackendReason::WorkerExited)
        );
    }

    #[test]
    fn installed_identity_mismatch_and_nonstatus_reply_never_manufacture_admission() {
        let original: Value = serde_json::from_slice(STATUS).unwrap();
        for field in ["id", "pid", "started_unix_ms", "runtime_dir"] {
            let mut changed = original.clone();
            changed["result"]["installer"]["instance"][field] = match field {
                "id" => json!(1),
                "pid" => json!(4243),
                "started_unix_ms" => json!(1790950000001u64),
                "runtime_dir" => json!("/run/user/1000/other"),
                _ => unreachable!(),
            };
            assert_eq!(
                agent_facts(
                    &serde_json::to_vec(&changed).unwrap(),
                    ObservationSource::Demo,
                    17
                ),
                Err(ProbeIssue::Foreign),
                "{field}"
            );
        }
        for result in [
            Ok(DecodedReply::Acknowledged),
            Err(CallFailure::Unavailable),
            Err(CallFailure::TimeoutOutcomeUnknown),
        ] {
            assert_eq!(
                InstalledAgentFacts::from_reply(
                    parse_bootstrap(BOOTSTRAP).unwrap(),
                    AgentReply {
                        id: 1,
                        observed_at_ms: 2,
                        source: ObservationSource::Demo,
                        result,
                    }
                ),
                Err(ProbeIssue::Unverified)
            );
        }
    }

    #[test]
    fn incomplete_health_remains_contract_pending_with_original_demo_receipt() {
        let facts =
            agent_facts(br#"{"ok":true,"result":{}}"#, ObservationSource::Demo, 11).unwrap();
        assert_eq!(
            facts.status,
            StatusAdmission::PendingHealthContract(PendingHealthReason::Absent)
        );
        assert_eq!(facts.received_at_ms, 11);
        assert_eq!(facts.source, ObservationSource::Demo);
        let mut status: Value = serde_json::from_slice(STATUS).unwrap();
        status["result"]["installer"]
            .as_object_mut()
            .unwrap()
            .remove("gate");
        let facts = agent_facts(
            &serde_json::to_vec(&status).unwrap(),
            ObservationSource::Demo,
            12,
        )
        .unwrap();
        assert_eq!(
            facts.status,
            StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete)
        );
    }
}
