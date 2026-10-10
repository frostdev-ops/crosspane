#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]
//! GNOME and KDE session evidence, in the pure decisions: which desktop a session is (the
//! agent's own rule), what must agree between the installer's environment and the user manager's,
//! what `session_authority` admits, and what stays exactly as it was for Hyprland.
use crosspane_installer::agent_contract::*;
use crosspane_installer::platform::linux::detect::*;
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
fn env(signature: &str, desktop: Option<&str>, kind: Option<&str>) -> EffectiveEnvironment {
    EffectiveEnvironment {
        runtime_dir: "/run/user/1000".into(),
        wayland_display: "wayland-0".into(),
        hyprland_instance_signature: signature.into(),
        session_id: Some("3".into()),
        xdg_current_desktop: desktop.map(str::to_owned),
        xdg_session_type: kind.map(str::to_owned),
    }
}
fn gnome_env() -> EffectiveEnvironment {
    env("", Some("GNOME"), Some("wayland"))
}
fn session(desktop: Desktop, environment: EffectiveEnvironment) -> SessionFacts {
    SessionFacts {
        uid: 1000,
        os: known(OsFamily::Arch),
        architecture: known(Architecture::X86_64),
        desktop: Ok(desktop),
        compositor_version: match desktop {
            Desktop::Hyprland => known([0, 56, 0]),
            Desktop::Gnome => known([50, 4, 0]),
            Desktop::Kde => Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 17),
        },
        protocols: known(true),
        compositor_managed: known(true),
        graphical_target_active: known(true),
        graphical_sessions: known(1),
        selected_session: known(Some(SelectedSession {
            selection: SessionSelection::Pid,
            session: candidate("wayland"),
        })),
        selected_environment: environment.clone(),
        manager_environment: known(environment),
    }
}
fn runtime() -> RuntimeFacts {
    RuntimeFacts {
        dependency_graph: known(true),
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

#[test]
fn the_desktop_is_the_agents_own_answer_for_the_environment() {
    use Desktop::*;
    for (signature, desktop, kind, expected) in [
        // A Hyprland signature wins over everything else, as in the agent.
        ("sig_1", Some("GNOME"), Some("x11"), Ok(Hyprland)),
        ("", Some("GNOME"), Some("wayland"), Ok(Gnome)),
        ("", Some("ubuntu:GNOME"), Some("wayland"), Ok(Gnome)),
        ("", Some("gnome"), None, Ok(Gnome)),
        ("", Some("KDE"), Some("wayland"), Ok(Kde)),
        ("", Some("KDE"), None, Ok(Kde)),
        // X11 is its own established negative.
        (
            "",
            Some("GNOME"),
            Some("x11"),
            Err(UnsupportedReason::SessionType),
        ),
        (
            "",
            Some("KDE"),
            Some("x11"),
            Err(UnsupportedReason::SessionType),
        ),
        // A desktop the agent has no backend for, or an ambiguous one.
        (
            "",
            Some("XFCE"),
            Some("wayland"),
            Err(UnsupportedReason::Desktop),
        ),
        (
            "",
            Some("sway"),
            Some("wayland"),
            Err(UnsupportedReason::Desktop),
        ),
        (
            "",
            Some("GNOME:KDE"),
            Some("wayland"),
            Err(UnsupportedReason::Desktop),
        ),
        // Hyprland without its signature, or an environment naming no desktop at all, is judged
        // by the original Hyprland pipeline (which stays pending and admits nothing).
        ("", Some("Hyprland"), Some("wayland"), Ok(Hyprland)),
        ("", Some("hyprland:foo"), None, Ok(Hyprland)),
        ("", None, None, Ok(Hyprland)),
        ("", None, Some("x11"), Ok(Hyprland)),
    ] {
        assert_eq!(
            env(signature, desktop, kind).desktop(),
            expected,
            "{signature:?} {desktop:?} {kind:?}"
        );
    }
}

#[test]
fn gnome_or_kde_with_no_sign_of_wayland_is_the_session_type_refusal_not_an_unknown_desktop() {
    for name in ["GNOME", "KDE", "ubuntu:GNOME"] {
        let mut bare = env("", Some(name), None);
        bare.wayland_display.clear();
        assert_eq!(
            bare.desktop(),
            Err(UnsupportedReason::SessionType),
            "{name}"
        );
    }
    // Both at once is the ambiguity the agent refuses by name.
    let mut both = env("", Some("GNOME:KDE"), None);
    both.wayland_display.clear();
    assert_eq!(both.desktop(), Err(UnsupportedReason::Desktop));
}

#[test]
fn hyprland_agreement_is_the_original_three_fields_and_nothing_more() {
    let selected = env("sig_1", Some("Hyprland"), Some("wayland"));
    // The Hyprland compare ignores the two new variables entirely.
    let manager = env("sig_1", None, None);
    assert!(manager.agrees_with(&selected, Desktop::Hyprland));
    for change in 0..3 {
        let mut other = manager.clone();
        match change {
            0 => other.runtime_dir = "/run/user/1001".into(),
            1 => other.wayland_display = "wayland-9".into(),
            _ => other.hyprland_instance_signature = "sig_2".into(),
        }
        assert!(!other.agrees_with(&selected, Desktop::Hyprland), "{change}");
    }
}

#[test]
fn gnome_and_kde_agreement_adds_the_variables_the_agent_reads_and_forbids_a_signature() {
    for desktop in [Desktop::Gnome, Desktop::Kde] {
        let selected = gnome_env();
        assert!(gnome_env().agrees_with(&selected, desktop));
        for change in 0..5 {
            let mut other = gnome_env();
            match change {
                0 => other.runtime_dir = "/run/user/1001".into(),
                1 => other.wayland_display = "wayland-1".into(),
                2 => other.xdg_current_desktop = Some("KDE".into()),
                3 => other.xdg_session_type = Some("x11".into()),
                // A manager holding a Hyprland signature would start a Hyprland agent.
                _ => other.hyprland_instance_signature = "sig_1".into(),
            }
            assert!(!other.agrees_with(&selected, desktop), "{change}");
        }
        let mut missing = gnome_env();
        missing.xdg_current_desktop = None;
        assert!(!missing.agrees_with(&selected, desktop));
        missing = gnome_env();
        missing.xdg_session_type = None;
        assert!(!missing.agrees_with(&selected, desktop));
    }
}

const MANAGER: &[u8] = b"XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-0\n\
XDG_CURRENT_DESKTOP=GNOME\nXDG_SESSION_TYPE=wayland\nXDG_SESSION_ID=3\nPATH=/usr/bin\n\
LANG=en_US.UTF-8\n";

#[test]
fn the_gnome_manager_environment_reads_exactly_the_agents_variables() {
    let parsed = parse_manager_environment_for(MANAGER, Desktop::Gnome).unwrap();
    assert_eq!(
        parsed,
        EffectiveEnvironment {
            runtime_dir: "/run/user/1000".into(),
            wayland_display: "wayland-0".into(),
            hyprland_instance_signature: String::new(),
            session_id: Some("3".into()),
            xdg_current_desktop: Some("GNOME".into()),
            xdg_session_type: Some("wayland".into()),
        }
    );
    assert!(parsed.agrees_with(&gnome_env(), Desktop::Gnome));
    assert_eq!(parsed.desktop(), Ok(Desktop::Gnome));
    // The session type is optional, as in the agent.
    let no_type = b"XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-0\n\
XDG_CURRENT_DESKTOP=ubuntu:GNOME\n";
    let parsed = parse_manager_environment_for(no_type, Desktop::Kde).unwrap();
    assert_eq!(parsed.xdg_session_type, None);
    assert_eq!(parsed.xdg_current_desktop.as_deref(), Some("ubuntu:GNOME"));
}

#[test]
fn a_gnome_manager_environment_missing_or_malformed_is_pending_never_defaulted() {
    let without = |key: &str| -> Vec<u8> {
        String::from_utf8(MANAGER.to_vec())
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with(key))
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .into_bytes()
    };
    for key in ["XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP"] {
        assert_eq!(
            parse_manager_environment_for(&without(key), Desktop::Gnome),
            Err(ProbeIssue::Missing),
            "{key}"
        );
    }
    for (key, bad) in [
        ("XDG_CURRENT_DESKTOP", "GNOME;rm"),
        ("XDG_CURRENT_DESKTOP", "GNOME KDE"),
        ("XDG_SESSION_TYPE", "wayland;x"),
        ("WAYLAND_DISPLAY", "../x"),
        ("WAYLAND_DISPLAY", "a/b"),
    ] {
        let text = String::from_utf8(MANAGER.to_vec())
            .unwrap()
            .lines()
            .map(|line| {
                if line.starts_with(key) {
                    format!("{key}={bad}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect::<String>();
        assert_eq!(
            parse_manager_environment_for(text.as_bytes(), Desktop::Gnome),
            Err(ProbeIssue::Malformed),
            "{key}={bad}"
        );
    }
    // A duplicate of a variable the agent reads is malformed, not "the last one wins".
    let mut doubled = MANAGER.to_vec();
    doubled.extend_from_slice(b"XDG_CURRENT_DESKTOP=KDE\n");
    assert_eq!(
        parse_manager_environment_for(&doubled, Desktop::Gnome),
        Err(ProbeIssue::Malformed)
    );
}

#[test]
fn the_hyprland_read_is_untouched_and_never_looks_at_the_new_variables() {
    // No signature: still the original Missing, whatever else the manager holds.
    assert_eq!(parse_manager_environment(MANAGER), Err(ProbeIssue::Missing));
    // A malformed XDG_SESSION_TYPE does not matter to the Hyprland read (it never reads it).
    let hyprland = b"XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-1\n\
HYPRLAND_INSTANCE_SIGNATURE=sig_9\nXDG_SESSION_TYPE=wayland;x\nXDG_CURRENT_DESKTOP=GNOME;x\n";
    let parsed = parse_manager_environment(hyprland).unwrap();
    assert_eq!(parsed.hyprland_instance_signature, "sig_9");
    assert_eq!(parsed.xdg_current_desktop, None);
    assert_eq!(parsed.xdg_session_type, None);
    assert_eq!(
        parse_manager_environment_for(hyprland, Desktop::Hyprland),
        Ok(parsed)
    );
    // Under GNOME, the same bytes keep the signature visible: it disagrees, it isn't erased.
    assert_eq!(
        parse_manager_environment_for(
            b"XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-0\n\
XDG_CURRENT_DESKTOP=GNOME\nHYPRLAND_INSTANCE_SIGNATURE=sig_9\n",
            Desktop::Gnome
        )
        .unwrap()
        .hyprland_instance_signature,
        "sig_9"
    );
}

#[test]
fn gnome_and_kde_sessions_are_admitted_on_the_same_evidence_hyprland_needs() {
    for desktop in [Desktop::Gnome, Desktop::Kde] {
        let ok = session(desktop, gnome_env());
        assert_eq!(session_authority(&ok), Ok(()), "{desktop:?}");
        assert_eq!(classify(&ok, &runtime()), Eligibility::Supported);
    }
    assert_eq!(
        session_authority(&session(
            Desktop::Hyprland,
            env("sig_1", Some("Hyprland"), Some("wayland"))
        )),
        Ok(())
    );
}

#[test]
fn every_missing_piece_of_gnome_evidence_refuses_authority() {
    type Edit = fn(&mut SessionFacts);
    let cases: [(&str, Edit, ProbeIssue); 12] = [
        (
            "compositor not run by the user manager",
            |s| s.compositor_managed = known(false),
            ProbeIssue::Unverified,
        ),
        (
            "graphical session target inactive",
            |s| s.graphical_target_active = known(false),
            ProbeIssue::Unverified,
        ),
        (
            "two graphical sessions",
            |s| s.graphical_sessions = known(2),
            ProbeIssue::Ambiguous,
        ),
        (
            "no unique session",
            |s| s.selected_session = known(None),
            ProbeIssue::Ambiguous,
        ),
        (
            "another account's session",
            |s| {
                s.selected_session = known(Some(SelectedSession {
                    selection: SessionSelection::Pid,
                    session: SessionCandidate {
                        uid: Some(1001),
                        ..candidate("wayland")
                    },
                }))
            },
            ProbeIssue::Foreign,
        ),
        (
            "x11 session",
            |s| {
                s.selected_session = known(Some(SelectedSession {
                    selection: SessionSelection::Pid,
                    session: candidate("x11"),
                }))
            },
            ProbeIssue::Unverified,
        ),
        (
            "inactive session",
            |s| {
                s.selected_session = known(Some(SelectedSession {
                    selection: SessionSelection::Pid,
                    session: SessionCandidate {
                        active: Some(false),
                        ..candidate("wayland")
                    },
                }))
            },
            ProbeIssue::Unverified,
        ),
        (
            "manager environment unreadable",
            |s| {
                s.manager_environment = Fact::issue(ProbeIssue::Missing, ObservationSource::Demo, 1)
            },
            ProbeIssue::Missing,
        ),
        (
            "manager runs another desktop's backend choice",
            |s| {
                let mut other = gnome_env();
                other.xdg_current_desktop = Some("KDE".into());
                s.manager_environment = known(other)
            },
            ProbeIssue::Foreign,
        ),
        (
            "manager has a Hyprland signature",
            |s| {
                let mut other = gnome_env();
                other.hyprland_instance_signature = "sig_1".into();
                s.manager_environment = known(other)
            },
            ProbeIssue::Foreign,
        ),
        (
            "manager serves another display",
            |s| {
                let mut other = gnome_env();
                other.wayland_display = "wayland-5".into();
                s.manager_environment = known(other)
            },
            ProbeIssue::Foreign,
        ),
        (
            "desktop the agent has no backend for",
            |s| s.desktop = Err(UnsupportedReason::Desktop),
            ProbeIssue::Unverified,
        ),
    ];
    for (name, edit, issue) in cases {
        let mut s = session(Desktop::Gnome, gnome_env());
        edit(&mut s);
        assert_eq!(session_authority(&s), Err(issue), "{name}");
    }
}

#[test]
fn eligibility_for_gnome_and_kde_has_no_version_floor_and_its_own_refusals() {
    // Unknown, old or absent versions never block GNOME or KDE: the agent probes at run time.
    for version in [
        known([45, 0, 0]),
        known([0, 0, 0]),
        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1),
    ] {
        for desktop in [Desktop::Gnome, Desktop::Kde] {
            let mut s = session(desktop, gnome_env());
            s.compositor_version = version.clone();
            assert_eq!(classify(&s, &runtime()), Eligibility::Supported);
        }
    }
    // Hyprland's floor is unchanged.
    let mut hypr = session(
        Desktop::Hyprland,
        env("sig_1", Some("Hyprland"), Some("wayland")),
    );
    hypr.compositor_version = known([0, 55, 9]);
    assert_eq!(
        classify(&hypr, &runtime()),
        Eligibility::NotSupported(UnsupportedReason::HyprlandVersion)
    );
    // Not run by the user manager: uwsm for Hyprland, "session manager" for the others.
    hypr.compositor_version = known([0, 56, 0]);
    hypr.compositor_managed = known(false);
    assert_eq!(
        classify(&hypr, &runtime()),
        Eligibility::NotSupported(UnsupportedReason::Uwsm)
    );
    for desktop in [Desktop::Gnome, Desktop::Kde] {
        let mut s = session(desktop, gnome_env());
        s.compositor_managed = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::SessionManager)
        );
        s.compositor_managed = known(true);
        s.protocols = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::RequiredProtocols)
        );
    }
    // An environment disagreement is pending, never support.
    let mut s = session(Desktop::Gnome, gnome_env());
    let mut other = gnome_env();
    other.xdg_current_desktop = Some("KDE".into());
    s.manager_environment = known(other);
    assert_eq!(
        classify(&s, &runtime()),
        Eligibility::Pending(ProbeIssue::Foreign)
    );
}

#[test]
fn an_unsupported_desktop_is_a_known_negative_with_plain_words() {
    for reason in [UnsupportedReason::Desktop, UnsupportedReason::SessionType] {
        let mut s = session(Desktop::Gnome, gnome_env());
        s.desktop = Err(reason);
        assert_eq!(classify(&s, &runtime()), Eligibility::NotSupported(reason));
        let report = compatibility_report(&s, &runtime());
        assert_eq!(report.eligibility, Eligibility::NotSupported(reason));
        assert!(report.notes.contains(&unsupported_text(reason)));
    }
    assert!(unsupported_text(UnsupportedReason::Desktop).contains("GNOME"));
    assert!(unsupported_text(UnsupportedReason::Desktop).contains("Hyprland"));
    // Hyprland keeps every original sentence; GNOME and KDE name themselves.
    for reason in [
        UnsupportedReason::OperatingSystem,
        UnsupportedReason::Architecture,
        UnsupportedReason::HyprlandVersion,
        UnsupportedReason::RequiredProtocols,
        UnsupportedReason::Uwsm,
        UnsupportedReason::SessionType,
        UnsupportedReason::VideoFeature,
        UnsupportedReason::RuntimeLibrary,
    ] {
        assert_eq!(
            unsupported_text_for(reason, Some(Desktop::Hyprland)),
            unsupported_text(reason)
        );
        assert_eq!(unsupported_text_for(reason, None), unsupported_text(reason));
    }
    assert!(
        unsupported_text_for(UnsupportedReason::SessionManager, Some(Desktop::Gnome))
            .contains("GNOME")
    );
    assert!(
        unsupported_text_for(UnsupportedReason::SessionManager, Some(Desktop::Kde))
            .contains("KDE Plasma")
    );
    assert!(
        !unsupported_text_for(UnsupportedReason::RequiredProtocols, Some(Desktop::Gnome))
            .contains("Hyprland")
    );
}

fn globals(names: &[(&str, u32)]) -> Vec<(String, u32)> {
    names.iter().map(|(n, v)| ((*n).to_owned(), *v)).collect()
}

#[test]
fn the_wayland_protocol_lists_are_per_desktop() {
    let portal = globals(&[
        ("wl_compositor", 6),
        ("wl_shm", 1),
        ("wl_seat", 9),
        ("wl_output", 4),
        ("xdg_wm_base", 6),
        ("zxdg_output_manager_v1", 3),
        ("wp_viewporter", 1),
    ]);
    // GNOME and KDE need no wlroots protocol; Hyprland's list is not satisfied by them.
    for desktop in [Desktop::Gnome, Desktop::Kde] {
        assert_eq!(protocols_satisfy_for(&portal, desktop), Ok(true));
        assert!(
            required_protocols(desktop)
                .iter()
                .all(|(name, _)| !name.starts_with("zwlr_"))
        );
    }
    assert_eq!(protocols_satisfy_for(&portal, Desktop::Hyprland), Ok(false));
    assert_eq!(protocols_satisfy(&portal), Ok(false));
    // Every one of them is required, at its minimum version.
    for (index, (name, minimum)) in REQUIRED_PORTAL_PROTOCOLS.iter().enumerate() {
        let mut fewer = portal.clone();
        fewer.retain(|(n, _)| n != name);
        assert_eq!(
            protocols_satisfy_for(&fewer, Desktop::Gnome),
            Ok(false),
            "{name}"
        );
        let mut older = portal.clone();
        for global in &mut older {
            if global.0 == *name {
                global.1 = minimum.saturating_sub(1);
            }
        }
        if *minimum > 1 {
            assert_eq!(
                protocols_satisfy_for(&older, Desktop::Kde),
                // A version of 0 is malformed; any other older version is simply unsatisfied.
                if minimum - 1 == 0 {
                    Err(ProbeIssue::Malformed)
                } else {
                    Ok(false)
                },
                "{index} {name}"
            );
        }
    }
    // The Hyprland list is exactly what it was.
    assert_eq!(required_protocols(Desktop::Hyprland), REQUIRED_PROTOCOLS);
    assert_eq!(REQUIRED_PROTOCOLS.len(), 13);
}

fn lineage(launcher: &str, parent: u32) -> Result<CompositorLineage, ProbeIssue> {
    Ok(CompositorLineage {
        launcher_pid: 100,
        launcher_executable: launcher.into(),
        compositor_pid: 200,
        compositor_parent: parent,
    })
}

#[test]
fn the_compositor_pid_rule_accepts_one_named_launcher_and_no_other() {
    let none = Err(ProbeIssue::Unavailable);
    // The main process itself needs no /proc evidence, whatever launcher is named.
    for launcher in [None, Some(KWIN_WRAPPER), Some(START_HYPRLAND)] {
        assert_eq!(compositor_matches_via(200, 200, &none, launcher), Ok(()));
    }
    // GNOME names no launcher: a child of the unit's process is not the Shell.
    assert_eq!(
        compositor_matches_via(100, 200, &lineage("/usr/bin/anything", 100), None),
        Err(ProbeIssue::Foreign)
    );
    // KDE: exactly the wrapper, exactly one step down.
    assert_eq!(
        compositor_matches_via(100, 200, &lineage(KWIN_WRAPPER, 100), Some(KWIN_WRAPPER)),
        Ok(())
    );
    for (launcher, parent) in [
        ("/usr/bin/other", 100),
        ("/usr/local/bin/kwin_wayland_wrapper", 100),
        ("/usr/bin/kwin_wayland_wrapper (deleted)", 100),
        (KWIN_WRAPPER, 99),
    ] {
        assert_eq!(
            compositor_matches_via(100, 200, &lineage(launcher, parent), Some(KWIN_WRAPPER)),
            Err(ProbeIssue::Foreign),
            "{launcher} {parent}"
        );
    }
    // A read that failed is not a match.
    assert_eq!(
        compositor_matches_via(100, 200, &none, Some(KWIN_WRAPPER)),
        Err(ProbeIssue::Unavailable)
    );
    // The Hyprland wrapper is not KDE's, and the Hyprland rule is unchanged.
    assert_eq!(
        compositor_matches_via(100, 200, &lineage(START_HYPRLAND, 100), Some(KWIN_WRAPPER)),
        Err(ProbeIssue::Foreign)
    );
    assert_eq!(
        compositor_matches(100, 200, &lineage(START_HYPRLAND, 100)),
        Ok(())
    );
    assert_eq!(
        compositor_matches(100, 200, &lineage(KWIN_WRAPPER, 100)),
        Err(ProbeIssue::Foreign)
    );
}
