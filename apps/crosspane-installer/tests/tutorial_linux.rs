#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)] // Scratch and fake assertions only; production retains the deny.
use crosspane_installer::platform::linux::{native_io, tutorial};
use crosspane_installer::{fixture, tutorial_window};
use crosspane_installer_core::AttemptId;
use crosspane_types::id::WindowId;
use fixture::*;
use native_io::tutorial::TutorialIpc;
use native_io::*;
use serde_json::{Value, json};

// WP-4.15b3a: the source include exposes only private cfg(test) memory helpers.
#[path = "../src/platform/linux/tutorial/routing.rs"]
mod routing;
use pipewire as pw;
use routing::test_support::Model as RouteModel;
fn speakers() -> SpeakersSelection {
    let peer = crosspane_types::id::NodeId([0x31; 32]);
    SpeakersSelection {
        peer,
        device_key: format!("crosspane.{peer}.speaker"),
    }
}
fn sink(model: &mut RouteModel, id: u32, serial: &str, role: &str, virtual_: &str) {
    let key = speakers().device_key;
    let info = [
        ("object.serial", serial),
        ("node.name", key.as_str()),
        ("media.class", role),
        ("node.virtual", virtual_),
    ];
    model.global(pw::types::ObjectType::Node, id, &info[..3]);
    model.node_info(id, &info);
}
fn port(model: &mut RouteModel, id: u32, serial: &str, node: &str, direction: &str, channel: &str) {
    model.global(
        pw::types::ObjectType::Port,
        id,
        &[
            ("object.serial", serial),
            ("node.id", node),
            ("port.direction", direction),
            ("audio.channel", channel),
        ],
    );
    // Only the fixture's participating node/direction/channel candidates receive bound info.
    if !matches!(
        (node, direction, channel),
        ("10", "out", "FL" | "FR") | ("20", "in", "FL" | "FR")
    ) {
        return;
    }
    model.port_info(
        id,
        direction == "out",
        &[
            ("object.serial", serial),
            ("node.id", node),
            ("port.direction", direction),
            ("audio.channel", channel),
        ],
    );
}
fn link(model: &mut RouteModel, id: u32, from: &str, to: &str) {
    model.global(
        pw::types::ObjectType::Link,
        id,
        &[
            ("object.serial", "800"),
            ("link.input.port", to),
            ("link.output.port", from),
        ],
    );
}
fn route_model() -> RouteModel {
    let mut model = RouteModel::new(&speakers());
    sink(&mut model, 20, "200", "Audio/Sink", "true");
    port(&mut model, 21, "201", "20", "in", "FL");
    port(&mut model, 22, "202", "20", "in", "FR");
    port(&mut model, 11, "101", "10", "out", "FL");
    port(&mut model, 12, "102", "10", "out", "FR");
    model
}
fn establish(model: &mut RouteModel) {
    model.pin().unwrap();
    assert_eq!(model.attach().unwrap(), [(11, 21), (12, 22)]);
    link(model, 30, "11", "21");
    link(model, 31, "12", "22");
    model.link_state(0, 11, 21, pw::link::LinkState::Active);
    model.link_state(1, 12, 22, pw::link::LinkState::Active);
    assert!(model.established());
}
#[test]
fn b3a_exact_peer_sink_and_two_active_links_are_required() {
    let mut model = route_model();
    model.pin().unwrap();
    assert_eq!(model.attach().unwrap(), [(11, 21), (12, 22)]);
    assert!(
        !model.established(),
        "creation requests are not evidence of linkage"
    );
    link(&mut model, 30, "11", "21");
    link(&mut model, 31, "12", "22");
    model.link_state(0, 11, 21, pw::link::LinkState::Active);
    assert!(!model.established(), "both exact links must be active");
    model.link_state(1, 12, 22, pw::link::LinkState::Active);
    assert!(model.established());
}

#[test]
fn b3a_unrelated_core_serial_zero_is_not_a_route_fact() {
    let mut model = route_model();
    model.global(pw::types::ObjectType::Core, 0, &[("object.serial", "0")]);
    assert!(!model.disabled());
    establish(&mut model);
    assert!(model.established());
}
#[test]
fn r1_extra_stream_ports_cannot_escape_foreign_link_detection() {
    for direction in ["out", "in"] {
        let mut model = route_model();
        establish(&mut model);
        port(&mut model, 13, "103", "10", direction, "FC");
        if direction == "in" {
            assert!(
                !model.disabled(),
                "an unlinked extra input port is only tracked"
            );
            assert!(model.established());
        }
        port(&mut model, 99, "990", "90", "in", "FL");
        link(&mut model, 39, "13", "99");
        assert!(model.disabled(), "foreign link on extra {direction} port");
        assert!(!model.established());
    }
}
#[test]
fn r1_unexpected_stream_output_port_refuses_even_without_a_link() {
    let mut model = route_model();
    establish(&mut model);
    port(&mut model, 13, "103", "10", "out", "FC");
    assert!(model.disabled());
    assert!(!model.established());
}
#[test]
fn r1_retained_port_ids_and_active_links_do_not_survive_property_changes() {
    for (id, serial, node, direction, channel) in [
        (11, "101", "10", "out", "FL"),
        (12, "102", "10", "out", "FR"),
        (21, "201", "20", "in", "FL"),
        (22, "202", "20", "in", "FR"),
    ] {
        let mut model = route_model();
        establish(&mut model);
        model.port_property_change(
            id,
            direction == "out",
            &[
                ("object.serial", serial),
                ("node.id", node),
                ("port.direction", direction),
                ("audio.channel", if channel == "FL" { "FR" } else { "FL" }),
                ("port.name", "SYNTHETIC_CHANGED_NAME"),
            ],
        );
        assert!(
            model.disabled(),
            "port {id} changed with IDs/links retained"
        );
        assert!(!model.established());
    }
}
#[test]
fn r1_unchanged_admitted_port_values_with_reemitted_props_stay_established() {
    let mut model = route_model();
    establish(&mut model);
    model.port_property_change(
        11,
        true,
        &[
            ("object.serial", "101"),
            ("node.id", "10"),
            ("port.direction", "out"),
            ("audio.channel", "FL"),
            ("port.name", "SYNTHETIC_UNRELATED_NAME_CHANGE"),
        ],
    );
    assert!(!model.disabled());
    assert!(model.established());
}
#[test]
fn r1_params_only_empty_dictionary_preserves_the_admitted_baseline() {
    let mut model = route_model();
    establish(&mut model);
    for props in [Some(&[][..]), None] {
        model.port_receipt(11, 11, true, pw::port::PortChangeMask::PARAMS, props);
        assert!(!model.disabled());
        assert!(model.established());
    }
}
#[test]
fn r1_props_disappearance_or_bad_receipt_identity_still_disables() {
    for (reported, output, mask, props) in [
        (11, true, pw::port::PortChangeMask::PROPS, Some(&[][..])),
        (11, true, pw::port::PortChangeMask::PROPS, None),
        (99, true, pw::port::PortChangeMask::PARAMS, Some(&[][..])),
        (11, false, pw::port::PortChangeMask::PARAMS, Some(&[][..])),
    ] {
        let mut model = route_model();
        establish(&mut model);
        model.port_receipt(11, reported, output, mask, props);
        assert!(model.disabled());
        assert!(!model.established());
    }
    let baseline = [
        ("object.serial", "101"),
        ("node.id", "10"),
        ("port.direction", "out"),
        ("audio.channel", "FL"),
    ];
    for props in [Some(&[][..]), Some(&baseline[..]), None] {
        let mut model = RouteModel::new(&speakers());
        model.global(pw::types::ObjectType::Port, 11, &baseline);
        model.port_receipt(11, 11, true, pw::port::PortChangeMask::PARAMS, props);
        assert!(
            model.disabled(),
            "partial receipt cannot supply first baseline"
        );
    }
}
#[test]
fn r1_each_admitted_port_fact_change_or_disappearance_disables() {
    let baseline = [
        ("object.serial", "101"),
        ("port.id", "0"),
        ("node.id", "10"),
        ("port.direction", "out"),
        ("audio.channel", "FL"),
        ("format.dsp", "32 bit float mono audio"),
        ("media.type", "Audio"),
    ];
    for (key, replacement) in [
        ("object.serial", "999"),
        ("port.id", "1"),
        ("node.id", "90"),
        ("port.direction", "in"),
        ("audio.channel", "FR"),
        ("format.dsp", "16 bit int mono audio"),
        ("media.type", "Video"),
    ] {
        for replacement in [Some(replacement), None] {
            let mut model = RouteModel::new(&speakers());
            sink(&mut model, 20, "200", "Audio/Sink", "true");
            for (id, serial, node, direction, channel) in [
                (11, "101", "10", "out", "FL"),
                (12, "102", "10", "out", "FR"),
                (21, "201", "20", "in", "FL"),
                (22, "202", "20", "in", "FR"),
            ] {
                let values = [
                    ("object.serial", serial),
                    ("port.id", if channel == "FL" { "0" } else { "1" }),
                    ("node.id", node),
                    ("port.direction", direction),
                    ("audio.channel", channel),
                    ("format.dsp", "32 bit float mono audio"),
                    ("media.type", "Audio"),
                ];
                model.global(pw::types::ObjectType::Port, id, &values[..6]);
                model.port_info(id, direction == "out", &values);
            }
            establish(&mut model);
            let changed: Vec<_> = baseline
                .iter()
                .filter_map(|(name, value)| {
                    if *name == key {
                        replacement.map(|value| (*name, value))
                    } else {
                        Some((*name, *value))
                    }
                })
                .collect();
            model.port_property_change(11, true, &changed);
            assert!(model.disabled(), "admitted {key} changed/disappeared");
            assert!(!model.established());
        }
    }
}
#[test]
fn r1_all_four_initial_port_infos_are_required_before_any_link_request() {
    let mut model = RouteModel::new(&speakers());
    sink(&mut model, 20, "200", "Audio/Sink", "true");
    let cases = [
        (11, "101", "10", "out", "FL"),
        (12, "102", "10", "out", "FR"),
        (21, "201", "20", "in", "FL"),
        (22, "202", "20", "in", "FR"),
    ];
    for (id, serial, node, direction, channel) in cases {
        model.global(
            pw::types::ObjectType::Port,
            id,
            &[
                ("object.serial", serial),
                ("node.id", node),
                ("port.direction", direction),
                ("audio.channel", channel),
            ],
        );
    }
    model.pin().unwrap();
    for (id, serial, node, direction, channel) in cases {
        assert!(model.pending_attach().is_none());
        assert_eq!(model.requests(), 0);
        model.port_info(
            id,
            direction == "out",
            &[
                ("object.serial", serial),
                ("node.id", node),
                ("port.direction", direction),
                ("audio.channel", channel),
            ],
        );
    }
    assert_eq!(
        model.pending_attach().unwrap().unwrap(),
        [(11, 21), (12, 22)]
    );
    assert_eq!(model.requests(), 2);
    assert!(!model.established());
}
#[test]
fn b3a_virtual_only_in_bound_info_is_required_before_pinning() {
    let mut model = RouteModel::new(&speakers());
    let key = speakers().device_key;
    model.global(
        pw::types::ObjectType::Node,
        20,
        &[
            ("object.serial", "200"),
            ("node.name", &key),
            ("media.class", "Audio/Sink"),
        ],
    );
    for (id, serial, node, direction, channel) in [
        (21, "201", "20", "in", "FL"),
        (22, "202", "20", "in", "FR"),
        (11, "101", "10", "out", "FL"),
        (12, "102", "10", "out", "FR"),
    ] {
        port(&mut model, id, serial, node, direction, channel);
    }
    assert!(
        !model.disabled(),
        "header is only a candidate, not full virtual evidence"
    );
    assert_eq!(model.pin(), Err(FixtureError::OutputUnavailable));
    model.node_info(
        20,
        &[
            ("object.serial", "200"),
            ("node.name", &key),
            ("media.class", "Audio/Sink"),
            ("node.virtual", "true"),
        ],
    );
    model.pin().unwrap();
    assert_eq!(model.attach().unwrap(), [(11, 21), (12, 22)]);
    assert!(
        !model.established(),
        "info and requests alone do not admit samples"
    );
}

#[test]
fn b3b_unrelated_node_description_change_preserves_admitted_identity() {
    let mut model = route_model();
    establish(&mut model);
    let key = speakers().device_key;
    model.node_property_change(
        20,
        &[
            ("object.serial", "200"),
            ("node.name", &key),
            ("media.class", "Audio/Sink"),
            ("node.virtual", "true"),
            ("node.description", "SYNTHETIC_CHANGED_DESCRIPTION"),
        ],
    );
    assert!(!model.disabled());
    assert!(model.established());
}

#[test]
fn b3a_stream_without_its_own_fl_fr_ports_pends_without_link_requests() {
    let mut model = RouteModel::new(&speakers());
    sink(&mut model, 20, "200", "Audio/Sink", "true");
    port(&mut model, 21, "201", "20", "in", "FL");
    port(&mut model, 22, "202", "20", "in", "FR");
    model.pin().unwrap();
    assert!(model.pending_attach().is_none());
    assert_eq!(model.requests(), 0);
    port(&mut model, 11, "101", "10", "out", "FL");
    assert!(model.pending_attach().is_none());
    port(&mut model, 12, "102", "99", "out", "FR");
    assert!(model.pending_attach().is_none());
    assert_eq!(model.requests(), 0);
    assert!(!model.established());
    port(&mut model, 12, "102", "10", "out", "FR");
    assert_eq!(
        model.pending_attach().unwrap().unwrap(),
        [(11, 21), (12, 22)]
    );
    assert_eq!(model.requests(), 2);
    assert!(!model.established());
}
#[test]
fn b3a_role_virtuality_full_peer_identity_and_ambiguity_fail_closed() {
    for (role, virtual_) in [
        ("Audio/Source", "true"),
        ("Audio/Sink", "false"),
        ("Stream/Output/Audio", "true"),
    ] {
        let mut model = route_model();
        model.remove(20);
        sink(&mut model, 20, "200", role, virtual_);
        assert!(model.pin().is_err());
    }
    let mut other = speakers();
    other.peer = crosspane_types::id::NodeId([0x32; 32]);
    other.device_key = format!("crosspane.{}.speaker", other.peer);
    let mut model = RouteModel::new(&other);
    sink(&mut model, 20, "200", "Audio/Sink", "true");
    assert!(model.pin().is_err());
    let mut model = route_model();
    sink(&mut model, 23, "203", "Audio/Sink", "true");
    assert!(model.pin().is_err());
}
#[test]
fn b3a_missing_duplicate_monitor_or_malformed_ports_are_not_playback() {
    for (direction, channel, parent) in [
        ("out", "FL", "20"),
        ("in", "MONO", "20"),
        ("in", "FL", "+20"),
        ("in", "FL", "4294967316"),
        ("in", "FL", "0"),
    ] {
        let mut model = route_model();
        model.remove(21);
        port(&mut model, 21, "201", parent, direction, channel);
        assert!(model.pin().is_err());
    }
    let mut model = route_model();
    port(&mut model, 23, "203", "20", "in", "FL");
    assert!(model.pin().is_err());
    for serial in ["+200", "-200", "18446744073709551616", "0", "200x", ""] {
        let mut model = route_model();
        model.remove(20);
        sink(&mut model, 20, serial, "Audio/Sink", "true");
        assert!(
            model.pin().is_err(),
            "malformed serial must not identify a sink"
        );
    }
}
#[test]
fn b3a_foreign_or_duplicate_link_never_becomes_an_admitted_route() {
    for (from, to) in [("11", "41"), ("12", "42"), ("50", "11")] {
        let mut model = route_model();
        model.pin().unwrap();
        model.attach().unwrap();
        link(&mut model, 30, from, to);
        assert!(model.disabled());
        assert!(!model.established());
    }
    let mut model = route_model();
    establish(&mut model);
    link(&mut model, 32, "11", "21");
    assert!(
        model.disabled(),
        "a duplicate exact link is still a foreign third link"
    );
}
#[test]
fn b3a_foreign_link_before_attachment_is_detected_before_own_requests() {
    let mut model = route_model();
    link(&mut model, 50, "11", "41");
    model.pin().unwrap();
    assert_eq!(model.attach(), Err(FixtureError::OutputChanged));
    assert!(model.disabled());
    assert!(!model.established());
}
#[test]
fn b3a_sink_or_port_incarnation_changes_never_rebind() {
    for removed in [20, 21, 22, 11, 12, 30] {
        let mut model = route_model();
        establish(&mut model);
        model.remove(removed);
        assert!(model.disabled());
        assert!(!model.established());
    }
    let mut model = route_model();
    model.pin().unwrap();
    model.remove(20);
    sink(&mut model, 20, "999", "Audio/Sink", "true");
    assert_eq!(model.attach(), Err(FixtureError::OutputChanged));
}
#[test]
fn b3a_bound_node_property_change_and_link_state_loss_disable_immediately() {
    let mut model = route_model();
    establish(&mut model);
    let key = speakers().device_key;
    model.node_info(
        20,
        &[
            ("object.serial", "200"),
            ("node.name", &key),
            ("media.class", "Audio/Source"),
            ("node.virtual", "true"),
        ],
    );
    assert!(model.disabled());
    for state in [
        pw::link::LinkState::Unlinked,
        pw::link::LinkState::Error("SYNTHETIC_FIXED_FAILURE"),
        pw::link::LinkState::Paused,
    ] {
        let mut model = route_model();
        establish(&mut model);
        model.link_state(0, 11, 21, state);
        assert!(model.disabled());
    }
    let mut model = route_model();
    model.pin().unwrap();
    model.attach().unwrap();
    model.link_state(0, 11, 41, pw::link::LinkState::Active);
    assert!(model.disabled());
}
#[test]
fn b3a_unrelated_link_removal_is_not_output_replacement() {
    let mut model = route_model();
    link(&mut model, 99, "60", "61");
    establish(&mut model);
    model.remove(99);
    assert!(!model.disabled());
    assert!(model.established());
}
#[test]
fn b3a_missing_or_signed_link_identity_cannot_hide_a_foreign_link() {
    for serial in [
        None,
        Some("+800"),
        Some("-1"),
        Some(""),
        Some("18446744073709551616"),
    ] {
        let mut model = route_model();
        establish(&mut model);
        let mut props = vec![("link.output.port", "11"), ("link.input.port", "41")];
        if let Some(serial) = serial {
            props.push(("object.serial", serial));
        }
        model.global(pw::types::ObjectType::Link, 40, &props);
        assert!(model.disabled());
        assert!(!model.established());
    }
}
#[test]
fn b3a_registry_cardinality_limit_fails_closed() {
    let mut model = RouteModel::new(&speakers());
    for n in 1..=4097 {
        port(&mut model, 10000 + n, "900", "50", "in", "FL");
    }
    assert!(model.disabled());
    assert!(!model.established());
}
#[test]
fn b3a_original_absolute_deadline_never_extends() {
    assert_eq!(
        routing::remaining(Instant::now() - Duration::from_millis(1)),
        Err(FixtureError::TimedOut)
    );
    let deadline = Instant::now() + Duration::from_millis(10);
    assert!(routing::remaining(deadline).unwrap() <= Duration::from_millis(10));
    thread::sleep(Duration::from_millis(12));
    assert_eq!(routing::remaining(deadline), Err(FixtureError::TimedOut));
}
#[test]
fn b3a_socket_admits_only_private_nofollow_runtime_and_same_uid_peer() {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    let root = Scratch::new();
    let listener = UnixListener::bind(root.0.join("pipewire-0")).unwrap();
    fs::set_permissions(root.0.join("pipewire-0"), fs::Permissions::from_mode(0o600)).unwrap();
    let fd = routing::socket(&root.0, Instant::now() + Duration::from_millis(100)).unwrap();
    assert!(
        rustix::fs::fcntl_getfl(&fd)
            .unwrap()
            .contains(rustix::fs::OFlags::NONBLOCK)
    );
    assert!(
        rustix::io::fcntl_getfd(fd.as_fd())
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    drop(fd);
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        routing::socket(&root.0, Instant::now() + Duration::from_millis(100)).unwrap_err(),
        FixtureError::Refused
    );
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(root.0.join("pipewire-0"), fs::Permissions::from_mode(0o622)).unwrap();
    assert_eq!(
        routing::socket(&root.0, Instant::now() + Duration::from_millis(100)).unwrap_err(),
        FixtureError::Refused
    );
    drop(listener);
}
#[test]
fn b3a_socket_rejects_scratch_symlink_ancestors_and_leaf_before_connect() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let root = Scratch::new();
    let runtime = root.0.join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let _listener = UnixListener::bind(runtime.join("pipewire-0")).unwrap();
    fs::set_permissions(
        runtime.join("pipewire-0"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let alias = root.0.join("alias");
    symlink(&runtime, &alias).unwrap();
    assert!(routing::socket(&alias, Instant::now() + Duration::from_millis(100)).is_err());
    fs::remove_file(runtime.join("pipewire-0")).unwrap();
    symlink(
        root.0.join("absent-owner-endpoint"),
        runtime.join("pipewire-0"),
    )
    .unwrap();
    assert_eq!(
        routing::socket(&runtime, Instant::now() + Duration::from_millis(100)).unwrap_err(),
        FixtureError::Refused
    );
    assert!(
        routing::socket(
            Path::new("relative"),
            Instant::now() + Duration::from_millis(100)
        )
        .is_err()
    );
}
#[test]
fn b3a_socket_original_deadline_expires_before_any_scratch_connection() {
    let root = Scratch::new();
    assert_eq!(
        routing::socket(&root.0, Instant::now() - Duration::from_millis(1)).unwrap_err(),
        FixtureError::TimedOut
    );
}
#[derive(Clone, Debug)]
struct PrivatePipewire {
    root: PathBuf,
    runtime: PathBuf,
    home: PathBuf,
    pid: u32,
    start: u64,
    socket: (u64, u64),
}
impl PrivatePipewire {
    fn verify(
        &self,
        values: &BTreeMap<String, String>,
        start: Option<u64>,
        meta: &fs::Metadata,
    ) -> std::result::Result<(), &'static str> {
        for (key, expected) in [
            ("CROSSPANE_PRIVATE_PIPEWIRE", "1"),
            ("B3A_PRIVATE_CLIENT", "1"),
            ("PIPEWIRE_REMOTE", "pipewire-0"),
            ("PIPEWIRE_RUNTIME_DIR", self.runtime.to_str().unwrap()),
            ("XDG_RUNTIME_DIR", self.runtime.to_str().unwrap()),
            ("HOME", self.home.to_str().unwrap()),
            ("PIPEWIRE_CONFIG_DIR", self.root.to_str().unwrap()),
            ("PIPEWIRE_CONFIG_NAME", "client.conf"),
            ("PIPEWIRE_CONFIG_PREFIX", ""),
            ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
            ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
            ("PULSE_SERVER", DEAD_PULSE),
        ] {
            if values.get(key).map(String::as_str) != Some(expected) {
                return Err("private environment mismatch");
            }
        }
        for key in [
            "PIPEWIRE_CORE",
            "PIPEWIRE_AUTOCONNECT",
            "PIPEWIRE_ALSA",
            "PIPEWIRE_PULSE",
            "CROSSPANE_AUDIO",
            "CROSSPANE_LIVE_TESTS",
            "CROSSPANE_REAL_STORE",
            "CROSSPANE_SECRET_SERVICE_LIVE",
            "CROSSPANE_AUDIO_OWNER_ATTENDED",
            "CROSSPANE_NESTED_HYPR",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "DISPLAY",
        ] {
            if values.contains_key(key) {
                return Err("owner or live override");
            }
        }
        if start != Some(self.start) || self.pid == 0 || self.start == 0 {
            return Err("stale owned process");
        }
        let uid = rustix::process::geteuid().as_raw();
        for path in [&self.root, &self.runtime, &self.home] {
            let m = fs::symlink_metadata(path).map_err(|_| "private directory missing")?;
            if !m.is_dir() || m.uid() != uid || m.mode() & 0o7777 != 0o700 {
                return Err("private directory ownership");
            }
        }
        if !self.runtime.starts_with(&self.root)
            || !self.home.starts_with(&self.root)
            || !std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type())
            || meta.uid() != uid
            || meta.mode() & 0o022 != 0
            || (meta.dev(), meta.ino()) != self.socket
        {
            return Err("private socket replaced");
        }
        Ok(())
    }
}
fn private_values(selected: &PrivatePipewire) -> BTreeMap<String, String> {
    [
        ("CROSSPANE_PRIVATE_PIPEWIRE", "1"),
        ("B3A_PRIVATE_CLIENT", "1"),
        ("PIPEWIRE_REMOTE", "pipewire-0"),
        ("PIPEWIRE_RUNTIME_DIR", selected.runtime.to_str().unwrap()),
        ("XDG_RUNTIME_DIR", selected.runtime.to_str().unwrap()),
        ("HOME", selected.home.to_str().unwrap()),
        ("PIPEWIRE_CONFIG_DIR", selected.root.to_str().unwrap()),
        ("PIPEWIRE_CONFIG_NAME", "client.conf"),
        ("PIPEWIRE_CONFIG_PREFIX", ""),
        ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
        ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
        ("PULSE_SERVER", DEAD_PULSE),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
#[test]
fn b3a_private_guard_refuses_stale_records_owner_handles_and_socket_replacement() {
    use std::os::unix::{fs::FileTypeExt, net::UnixListener};
    let root = Scratch::new();
    let runtime = root.0.join("runtime");
    let home = root.0.join("home");
    for p in [&runtime, &home] {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = runtime.join("pipewire-0");
    let _first = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let meta = fs::symlink_metadata(&socket).unwrap();
    assert!(meta.file_type().is_socket());
    let selected = PrivatePipewire {
        root: root.0.clone(),
        runtime,
        home,
        pid: 123,
        start: 456,
        socket: (meta.dev(), meta.ino()),
    };
    let values = private_values(&selected);
    selected.verify(&values, Some(456), &meta).unwrap();
    for start in [None, Some(457)] {
        assert!(selected.verify(&values, start, &meta).is_err());
    }
    for (key, value) in [
        ("PIPEWIRE_REMOTE", "pipewire-0-other"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("PIPEWIRE_RUNTIME_DIR", "/run/user/1000"),
        ("HOME", "/owner"),
        ("CROSSPANE_PRIVATE_PIPEWIRE", "01"),
        ("B3A_PRIVATE_CLIENT", "1-other"),
        (
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/run/dbus/system_bus_socket",
        ),
        ("PIPEWIRE_AUTOCONNECT", "0"),
        ("PIPEWIRE_CORE", "owner"),
        ("WAYLAND_SOCKET", "3"),
        ("CROSSPANE_AUDIO", "1"),
    ] {
        let mut changed = values.clone();
        changed.insert(key.into(), value.into());
        assert!(
            selected.verify(&changed, Some(456), &meta).is_err(),
            "must refuse {key}"
        );
    }
    fs::remove_file(&socket).unwrap();
    let _replacement = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        selected
            .verify(&values, Some(456), &fs::symlink_metadata(&socket).unwrap())
            .is_err()
    );
}
#[derive(Default)]
struct PortDiagnostic {
    events: u32,
    changes: u32,
    facts: String,
    mask: String,
}
fn diagnostic_value(value: &str) -> String {
    value
        .chars()
        .take(16)
        .map(|c| {
            if c.is_ascii_alphanumeric() || " _./-".contains(c) {
                c
            } else {
                '?'
            }
        })
        .collect()
}
fn diagnostic_summary(events: &BTreeMap<u32, PortDiagnostic>) -> String {
    events
        .iter()
        .map(|(id, event)| {
            format!(
                "{id}: n={} changes={} mask={} {}",
                event.events, event.changes, event.mask, event.facts
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}
#[test]
fn r1_private_port_diagnostics_fit_the_unchanged_command_pipe_bound() {
    let value = diagnostic_value("\\\"\n💥ABCDEFGHIJKLMNOPQRSTUV");
    assert!(value.is_ascii() && value.len() <= 16);
    let events: BTreeMap<_, _> = (u32::MAX - 7..=u32::MAX)
        .map(|id| {
            (
                id,
                PortDiagnostic {
                    events: u32::MAX,
                    changes: u32::MAX,
                    facts: "x".repeat(210),
                    mask: "x".repeat(16),
                },
            )
        })
        .collect();
    assert!(diagnostic_summary(&events).len() < 2800);
}
fn private_client(selected: &PrivatePipewire) {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let values: BTreeMap<_, _> = std::env::vars().collect();
    let path = selected.runtime.join("pipewire-0");
    selected
        .verify(
            &values,
            owned_start_ticks(selected.pid).unwrap(),
            &fs::symlink_metadata(&path).unwrap(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let fd = routing::socket(&selected.runtime, deadline).unwrap();
    assert_eq!(
        rustix::net::sockopt::socket_peercred(&fd)
            .unwrap()
            .pid
            .as_raw_nonzero()
            .get() as u32,
        selected.pid
    );
    selected
        .verify(
            &values,
            owned_start_ticks(selected.pid).unwrap(),
            &fs::symlink_metadata(&path).unwrap(),
        )
        .unwrap();
    // Private HOME, explicit private client config and held verified descriptor. No default connect.
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context=pw::context::ContextRc::new(&loop_,Some(pw::properties::properties!{"config.name"=>selected.root.join("client.conf").to_str().unwrap()})).unwrap();
    let core = context.connect_fd_rc(fd, None).unwrap();
    let status = Arc::new(routing::Status::default());
    let mut route = routing::Route::new(core.clone(), &speakers(), status.clone()).unwrap();
    let registry = core.get_registry_rc().unwrap();
    let foreign = Rc::new(RefCell::new(Vec::<u32>::new()));
    let foreign_ports = Rc::new(RefCell::new(Vec::<u32>::new()));
    let links = Rc::new(RefCell::new(BTreeMap::<u32, (u32, u32)>::new()));
    let ports = Rc::new(RefCell::new(Vec::<(u32, String, String, String)>::new()));
    let f = foreign.clone();
    let p = foreign_ports.clone();
    let l = links.clone();
    let removed = links.clone();
    let facts = ports.clone();
    // Read-only diagnostics of this verified synthetic graph, bounded in keys, bytes and events.
    let port_info = Rc::new(RefCell::new(BTreeMap::<u32, PortDiagnostic>::new()));
    let events = port_info.clone();
    let diagnostic_ports = Rc::new(RefCell::new(Vec::<(
        u32,
        pw::port::PortListener,
        pw::port::Port,
    )>::new()));
    let watches = diagnostic_ports.clone();
    let binding = registry.clone();
    let _inventory = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            if g.type_ == pw::types::ObjectType::Port {
                assert!(facts.borrow().len() < 16, "synthetic inventory bound");
                facts.borrow_mut().push((
                    g.id,
                    props.get("node.id").unwrap_or("missing").into(),
                    props.get("port.direction").unwrap_or("missing").into(),
                    props.get("audio.channel").unwrap_or("missing").into(),
                ));
                if !watches.borrow().iter().any(|(id, _, _)| *id == g.id) {
                    assert!(
                        watches.borrow().len() < 8,
                        "synthetic diagnostic port bound"
                    );
                    let bound = binding.bind::<pw::port::Port, _>(g).unwrap();
                    let events = events.clone();
                    let id = g.id;
                    let listener = bound
                        .add_listener_local()
                        .info(move |info| {
                            let values: Vec<_> = [
                                "object.serial",
                                "port.id",
                                "node.id",
                                "port.direction",
                                "audio.channel",
                                "format.dsp",
                                "media.type",
                                "port.monitor",
                                "port.physical",
                            ]
                            .into_iter()
                            .enumerate()
                            .map(|(index, key)| {
                                let value =
                                    info.props().and_then(|p| p.get(key)).unwrap_or("<absent>");
                                format!("{index}={}", diagnostic_value(value))
                            })
                            .collect();
                            let mut events = events.borrow_mut();
                            assert!(events.contains_key(&id) || events.len() < 8);
                            let event = events.entry(id).or_default();
                            let facts = format!(
                                "i={} d={:?} {}",
                                info.id(),
                                info.direction(),
                                values.join(" ")
                            );
                            if event.events > 0 && event.facts != facts {
                                event.changes = event.changes.saturating_add(1);
                            }
                            event.events = event.events.saturating_add(1);
                            event.facts = facts;
                            event.mask = format!("{}", info.change_mask().bits());
                        })
                        .register();
                    watches.borrow_mut().push((id, listener, bound));
                }
            }
            if g.type_ == pw::types::ObjectType::Node {
                if props.get("node.name") == Some("crosspane.private.foreign") {
                    f.borrow_mut().push(g.id);
                }
                if let Some(class) = props.get("media.class") {
                    assert!(
                        matches!(class, "Audio/Sink" | "Stream/Output/Audio"),
                        "no source/hardware node"
                    );
                }
            }
            if g.type_ == pw::types::ObjectType::Port
                && props
                    .get("node.id")
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_some_and(|n| f.borrow().contains(&n))
            {
                p.borrow_mut().push(g.id);
            }
            if g.type_ == pw::types::ObjectType::Link {
                l.borrow_mut().insert(
                    g.id,
                    (
                        props.get("link.output.port").unwrap().parse().unwrap(),
                        props.get("link.input.port").unwrap().parse().unwrap(),
                    ),
                );
            }
        })
        .global_remove(move |id| {
            removed.borrow_mut().remove(&id);
        })
        .register();
    let done = Rc::new(Cell::new(None));
    let finished = done.clone();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 {
                finished.set(Some(seq));
            }
        })
        .register();
    // The second barrier includes binds queued by registry callbacks, within the same deadline.
    for _ in 0..2 {
        let seq = core.sync(0).unwrap();
        while done.get() != Some(seq) {
            routing::remaining(deadline).unwrap();
            loop_
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)));
        }
    }
    assert_eq!(foreign.borrow().len(), 1);
    assert_eq!(foreign_ports.borrow().len(), 2);
    route.pin_sink().unwrap();
    let stream = pw::stream::StreamBox::new(
        &core,
        "b3a-private-zero",
        pw::properties::properties! {
            "media.type"=>"Audio", "media.category"=>"Playback", "media.role"=>"Test",
            "node.name"=>"crosspane.private.zero-output", "node.virtual"=>"true",
            "node.dont-fallback"=>"true", "node.dont-move"=>"true", "node.dont-reconnect"=>"true",
            "adapter.auto-port-config"=>"{ mode=dsp monitor=false position=preserve }",
        },
    )
    .unwrap();
    let _zero = stream
        .add_local_listener_with_user_data(())
        .process(|stream, _| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                for data in buffer.datas_mut() {
                    let n = if let Some(bytes) = data.data() {
                        bytes.fill(0);
                        bytes.len()
                    } else {
                        0
                    };
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = 8;
                    *chunk.size_mut() = n as u32;
                }
            }
        })
        .register()
        .unwrap();
    let mut audio = pw::spa::param::audio::AudioInfoRaw::new();
    audio.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    audio.set_rate(48000);
    audio.set_channels(2);
    let mut positions = [0; pw::spa::param::audio::MAX_CHANNELS];
    positions[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio.set_position(positions);
    let pod = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio.into(),
        }),
    )
    .unwrap()
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&pod).unwrap()];
    stream
        .connect(
            pw::spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )
        .unwrap();
    let mut attached = false;
    while !route.established() {
        assert!(
            routing::remaining(deadline).is_ok(),
            "private exact-link deadline: attached={attached} owned_node={} ports={:?} links={:?}",
            stream.node_id(),
            ports.borrow(),
            links.borrow()
        );
        assert!(
            !status.disabled.load(Ordering::Acquire),
            "routing failed closed: attached={attached} ports={:?} info={}",
            ports.borrow(),
            diagnostic_summary(&port_info.borrow())
        );
        if !attached && let Some(result) = route.attach(&stream) {
            result.unwrap();
            attached = true;
        }
        loop_
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)));
    }
    assert_eq!(links.borrow().len(), 2, "only the two owned links exist");
    println!(
        "private bound-port facts (0=serial 1=local-port 2=node 3=direction 4=channel 5=DSP 6=media 7=monitor 8=physical): {}",
        diagnostic_summary(&port_info.borrow())
    );
    assert!(
        links
            .borrow()
            .values()
            .all(|(_, to)| !foreign_ports.borrow().contains(to))
    );
    assert!(
        !status.rendered.load(Ordering::Acquire),
        "zero-output fixture is not tone evidence"
    );
    stream.disconnect().unwrap();
    drop(route);
    // Flush owned proxy destruction; these events are separate from real tone/drain evidence.
    let seq = core.sync(0).unwrap();
    while done.get() != Some(seq) {
        routing::remaining(deadline).unwrap();
        loop_
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)));
    }
    assert!(
        links.borrow().is_empty(),
        "owned exact links destroyed after route drop"
    );
    println!("private zero-output: exact FL/FR links established, foreign/default sink unlinked");
}
fn private_daemon_config() -> String {
    format!(
        r#"
context.properties = {{ core.daemon=true core.name=pipewire-0 support.dbus=false default.clock.rate=48000 default.clock.quantum=480 default.clock.min-quantum=480 default.clock.max-quantum=480 }}
context.spa-libs = {{ support.*=support/libspa-support audio.convert.*=audioconvert/libspa-audioconvert }}
context.modules = [
 {{ name=libpipewire-module-protocol-native }}
 {{ name=libpipewire-module-access args={{ access.force=unrestricted }} }}
 {{ name=libpipewire-module-metadata }}
 {{ name=libpipewire-module-spa-node-factory }}
 {{ name=libpipewire-module-client-node }}
 {{ name=libpipewire-module-adapter }}
 {{ name=libpipewire-module-link-factory }}
]
context.objects = [
 {{ factory=spa-node-factory args={{ factory.name=support.node.driver node.name=crosspane.private.driver priority.driver=20000 }} }}
 {{ factory=adapter args={{ factory.name=support.null-audio-sink node.name={} node.virtual=true media.class=Audio/Sink audio.format=F32LE audio.rate=48000 audio.channels=2 audio.position=[ FL FR ] adapter.auto-port-config={{ mode=dsp monitor=false position=preserve }} }} }}
 {{ factory=adapter args={{ factory.name=support.null-audio-sink node.name=crosspane.private.foreign node.virtual=true media.class=Audio/Sink audio.format=F32LE audio.rate=48000 audio.channels=2 audio.position=[ FL FR ] adapter.auto-port-config={{ mode=dsp monitor=false position=preserve }} }} }}
 {{ factory=metadata args={{ metadata.name=default metadata.values=[ {{ key=default.audio.sink value={{ name=crosspane.private.foreign }} }} ] }} }}
]
"#,
        speakers().device_key
    )
}

struct ReportedPrivateServer {
    owned: Option<OwnedProcess>,
    pid: u32,
    start: u64,
    record: PathBuf,
    retain: Arc<AtomicBool>,
}
impl ReportedPrivateServer {
    fn child(&mut self) -> &mut Child {
        self.owned.as_mut().unwrap().child()
    }
}
impl Drop for ReportedPrivateServer {
    fn drop(&mut self) {
        drop(self.owned.take());
        let gone = owned_start_ticks(self.pid).is_ok_and(|current| current != Some(self.start));
        let reconciled = fs::symlink_metadata(&self.record)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if gone && reconciled && !self.retain.load(Ordering::Acquire) {
            println!(
                "private owned pid={} start={} disappeared; record reconciled",
                self.pid, self.start
            );
        } else {
            self.retain.store(true, Ordering::Release);
            eprintln!(
                "private owned cleanup UNCONFIRMED pid={} start={} record={}; retaining scratch",
                self.pid,
                self.start,
                self.record.display()
            );
        }
    }
}
#[test]
fn b3a_private_zero_output_routes_only_exact_sink() {
    use std::os::unix::fs::FileTypeExt;
    if std::env::var_os("B3A_PRIVATE_CLIENT").is_some() {
        let root = PathBuf::from(std::env::var_os("PIPEWIRE_CONFIG_DIR").unwrap());
        let proof = root.join("server.json");
        let file = fs::File::from(
            rustix::fs::open(
                &proof,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .unwrap(),
        );
        let meta = file.metadata().unwrap();
        assert!(
            meta.is_file()
                && meta.nlink() == 1
                && meta.uid() == rustix::process::geteuid().as_raw()
                && meta.mode() & 0o7777 == 0o600
                && meta.len() <= 4096
        );
        let record: Value = serde_json::from_reader(file.take(4097)).unwrap();
        let selected = PrivatePipewire {
            root: root.clone(),
            runtime: root.join("runtime"),
            home: root.join("home"),
            pid: u32::try_from(record["pid"].as_u64().unwrap()).unwrap(),
            start: record["start"].as_u64().unwrap(),
            socket: (
                record["dev"].as_u64().unwrap(),
                record["ino"].as_u64().unwrap(),
            ),
        };
        private_client(&selected);
        return;
    }
    if std::env::var("CROSSPANE_PRIVATE_PIPEWIRE").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP private PipeWire: exact CROSSPANE_PRIVATE_PIPEWIRE=1 absent; no audio coverage"
        );
        return;
    }
    if !Path::new("/usr/bin/pipewire").is_file() {
        eprintln!("SKIP private PipeWire: /usr/bin/pipewire missing; no audio coverage");
        return;
    }
    assert_eq!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        DEAD_SESSION
    );
    assert_eq!(
        std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap(),
        DEAD_SYSTEM
    );
    assert_eq!(std::env::var("PULSE_SERVER").unwrap(), DEAD_PULSE);
    for key in [
        "CROSSPANE_AUDIO",
        "CROSSPANE_NESTED_HYPR",
        "PIPEWIRE_AUTOCONNECT",
        "PIPEWIRE_CORE",
        "CROSSPANE_LIVE_TESTS",
        "CROSSPANE_REAL_STORE",
        "CROSSPANE_SECRET_SERVICE_LIVE",
    ] {
        assert!(std::env::var_os(key).is_none(), "unexpected live override");
    }
    let root = Scratch::new();
    let runtime = root.0.join("runtime");
    let home = root.0.join("home");
    for p in [&runtime, &home] {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let daemon = root.0.join("daemon.conf");
    let client = root.0.join("client.conf");
    fs::write(&daemon, private_daemon_config()).unwrap();
    fs::write(&client,"context.properties = { support.dbus=false }\ncontext.spa-libs = { support.*=support/libspa-support audio.convert.*=audioconvert/libspa-audioconvert }\ncontext.modules = [ { name=libpipewire-module-protocol-native } { name=libpipewire-module-client-node } { name=libpipewire-module-adapter } ]\n").unwrap();
    for p in [&daemon, &client] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut command = private_command(&runtime, &home);
    let log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.0.join("server.log"))
        .unwrap();
    command
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    command
        .arg("env")
        .arg(format!("PIPEWIRE_RUNTIME_DIR={}", runtime.display()))
        .arg(format!("PIPEWIRE_CONFIG_DIR={}", root.0.display()))
        .arg("PIPEWIRE_CONFIG_PREFIX=")
        .arg("PIPEWIRE_CONFIG_NAME=daemon.conf")
        .args(["/usr/bin/pipewire", "-c"])
        .arg(&daemon);
    let mut server = OwnedProcess::spawn(command, root.1.clone());
    let pid = server.child().id();
    let start = owned_start_ticks(pid).unwrap().unwrap();
    println!("private owned server pid={pid} start={start}");
    let mut server = ReportedPrivateServer {
        record: server.pending.as_ref().unwrap().record.clone(),
        owned: Some(server),
        pid,
        start,
        retain: root.1.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let socket = runtime.join("pipewire-0");
    while !fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
        assert!(
            server.child().try_wait().unwrap().is_none(),
            "owned private server exited"
        );
        assert!(Instant::now() < deadline, "private server socket deadline");
        thread::sleep(Duration::from_millis(2));
    }
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let meta = fs::symlink_metadata(&socket).unwrap();
    let selected = PrivatePipewire {
        root: root.0.clone(),
        runtime,
        home,
        pid,
        start,
        socket: (meta.dev(), meta.ino()),
    };
    let proof = root.0.join("server.json");
    let mut record = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&proof)
        .unwrap();
    writeln!(
        record,
        "{}",
        json!({"pid":pid,"start":start,"dev":meta.dev(),"ino":meta.ino()})
    )
    .unwrap();
    let mut command = private_command(&selected.runtime, &selected.home);
    command.arg("env");
    for (key, value) in private_values(&selected) {
        command.arg(format!("{key}={value}"));
    }
    command.arg(std::env::current_exe().unwrap()).args([
        "--exact",
        "b3a_private_zero_output_routes_only_exact_sink",
        "--nocapture",
    ]);
    selected
        .verify(
            &private_values(&selected),
            owned_start_ticks(pid).unwrap(),
            &fs::symlink_metadata(&socket).unwrap(),
        )
        .unwrap();
    let output = run_bounded(command, root.1.clone());
    assert!(output.contains("private zero-output: exact FL/FR links established"));
    let record = server.record.clone();
    drop(server);
    assert!(
        !root.1.load(Ordering::Acquire),
        "owned private server cleanup outstanding"
    );
    assert_ne!(
        owned_start_ticks(pid).unwrap(),
        Some(start),
        "owned PID/start still exists"
    );
    assert!(!record.exists(), "owned cleanup evidence retained");
    println!("{output}");
    println!("private server pid={pid} start={start} gone; owned records reconciled");
}

use std::{
    collections::BTreeMap,
    collections::VecDeque,
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const TITLE: &str = "Crosspane practice | TEST_MACHINE | attempt 1 | fixture 1";
static SERIAL: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf, Arc<AtomicBool>);
impl Scratch {
    fn new() -> Self {
        let path = Self::path();
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self::owned(path)
    }
    fn owned(path: PathBuf) -> Self {
        Self(path, Arc::new(AtomicBool::new(false)))
    }
    fn path() -> PathBuf {
        PathBuf::from(format!(
            // Leave room for the compositor signature in Linux's bounded Unix socket path.
            "/tmp/cb{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::SeqCst)
        ))
    }
}

#[test]
fn b2_duplicate_stable_id_is_refused_even_for_an_unrelated_client() {
    let mut foreign = client();
    foreign["pid"] = json!(78);
    foreign["title"] = json!("UNRELATED_SYNTHETIC_TITLE");
    assert_eq!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!([client(), foreign]),
            monitors()
        ),
        Err(FixtureError::AmbiguousWindow)
    );
}

#[test]
fn b2_admitted_executable_replacement_is_refused_before_process_probe() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    fs::rename(&a.path, a.root.0.join("held-executable")).unwrap();
    fs::write(&a.path, b"SUBSTITUTE_INERT_BYTES").unwrap();
    fs::set_permissions(&a.path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        ),
        Err(NativeError::Foreign)
    ));
    assert!(a.probe.facts.lock().unwrap().is_empty());
}

#[test]
fn b2_digest_only_timing_on_same_bounded_artifact() {
    if std::env::var("CROSSPANE_DIGEST_TIMING").as_deref() != Ok("1") {
        return;
    }
    let scratch = Scratch::new();
    let path = scratch.0.join("crosspane-tutorial");
    stage_artifact(
        Path::new(env!("CARGO_BIN_EXE_crosspane-tutorial")),
        &path,
        MAX_FILE_BYTES as u64,
    )
    .unwrap();
    let mut file = fs::File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    );
    let stat = rustix::fs::fstat(&file).unwrap();
    assert_eq!(stat.st_size, 79_095_328);
    assert!(stat.st_size as u64 <= MAX_FILE_BYTES as u64);
    assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
    assert_eq!(stat.st_nlink, 1);
    assert_eq!(stat.st_mode & 0o177022, 0o100000);
    let expected = [
        0x7c, 0x17, 0x83, 0x55, 0x4b, 0x25, 0x8f, 0x8d, 0x53, 0x95, 0xbd, 0x9c, 0x13, 0x36, 0x03,
        0x14, 0x86, 0x38, 0x43, 0x3e, 0x49, 0xac, 0x83, 0xed, 0x7e, 0x55, 0x62, 0xac, 0xa8, 0xdf,
        0x69, 0xb5,
    ];
    // Precisely mirror native_io/tutorial.rs's digest step, not font/ownership preparation,
    // IPC, spawn, Hello, or observation. The artifact, 64KiB reads and 2s bound are unchanged.
    let deadline = Deadline::new(2000, Cancellation::default()).unwrap();
    let started = Instant::now();
    let mut size = 0u64;
    let result = (|| -> std::result::Result<(), NativeError> {
        let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
        let mut buffer = [0; 64 * 1024];
        loop {
            deadline.check()?;
            let n = file
                .read(&mut buffer)
                .map_err(|_| NativeError::Unavailable)?;
            if n == 0 {
                break;
            }
            size = size.checked_add(n as u64).ok_or(NativeError::Oversize)?;
            if size > MAX_FILE_BYTES as u64 {
                return Err(NativeError::Oversize);
            }
            digest.update(&buffer[..n]);
        }
        if size != stat.st_size as u64 || digest.finish().as_ref() != expected {
            return Err(NativeError::Foreign);
        }
        Ok(())
    })();
    let elapsed = started.elapsed();
    eprintln!(
        "digest-only AWS-LC Context/SHA256: elapsed={elapsed:?}, bytes={size}, result={result:?}, expected_sha256=7c1783554b258f8d5395bd9c133603148638433e49ac83ed7e5562aca8df69b5"
    );
    result.unwrap();
}

// The guarded cases below are window-only. No PipeWire server, input injector, agent,
// Secret Service, tray, or hardware audio endpoint is created or queried.
const DEAD_SESSION: &str = "unix:path=/nonexistent/crosspane-test-bus";
const DEAD_SYSTEM: &str = "unix:path=/nonexistent/crosspane-system-bus";
const DEAD_PIPEWIRE: &str = "/nonexistent/crosspane-audio";
const DEAD_PULSE: &str = "unix:/nonexistent/crosspane-audio";
#[derive(Debug, Clone, PartialEq, Eq)]
struct NestSelection {
    signature: String,
    display: String,
}
impl NestSelection {
    fn verify_fresh(
        &self,
        recorded: (u32, u64),
        current_start: Option<u64>,
        instances: &Value,
    ) -> std::result::Result<(), &'static str> {
        if recorded.0 == 0 || recorded.1 == 0 || current_start != Some(recorded.1) {
            return Err("owned PID/start record is stale");
        }
        let instances = instances
            .as_array()
            .filter(|a| a.len() <= 64)
            .ok_or("invalid current instances")?;
        let mut matches = 0;
        for instance in instances {
            if instance["pid"].as_u64() == Some(u64::from(recorded.0))
                || instance["instance"].as_str() == Some(&self.signature)
                || instance["wl_socket"].as_str() == Some(&self.display)
            {
                if instance["pid"].as_u64() != Some(u64::from(recorded.0))
                    || instance["instance"].as_str() != Some(&self.signature)
                    || instance["wl_socket"].as_str() != Some(&self.display)
                {
                    return Err("current instance/socket does not belong to the recorded child");
                }
                matches += 1;
            }
        }
        if matches != 1 {
            return Err("owned current instance must be unique");
        }
        Ok(())
    }
    fn parse(text: &str) -> std::result::Result<Self, &'static str> {
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 4
            || lines[0] != "unset WAYLAND_SOCKET"
            || lines[3] != "export CROSSPANE_NESTED_HYPR=1"
            || !text.ends_with('\n')
        {
            return Err("not the named harness environment");
        }
        let signature = lines[1]
            .strip_prefix("export HYPRLAND_INSTANCE_SIGNATURE=")
            .ok_or("missing signature")?;
        let display = lines[2]
            .strip_prefix("export WAYLAND_DISPLAY=")
            .ok_or("missing display")?;
        for value in [signature, display] {
            if value.is_empty()
                || value.len() > 160
                || value == "."
                || value == ".."
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("invalid exact harness value");
            }
        }
        Ok(Self {
            signature: signature.into(),
            display: display.into(),
        })
    }
    fn verify(&self, values: &BTreeMap<String, String>) -> std::result::Result<(), &'static str> {
        for (key, expected) in [
            ("CROSSPANE_NESTED_HYPR", "1"),
            ("HYPRLAND_INSTANCE_SIGNATURE", self.signature.as_str()),
            ("WAYLAND_DISPLAY", self.display.as_str()),
            ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
            ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
            ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
            ("PULSE_SERVER", DEAD_PULSE),
        ] {
            if values.get(key).map(String::as_str) != Some(expected) {
                return Err("inherited environment does not equal the owned harness");
            }
        }
        for key in [
            "WAYLAND_SOCKET",
            "DISPLAY",
            "CROSSPANE_AUDIO",
            "CROSSPANE_LIVE_TESTS",
            "CROSSPANE_REAL_STORE",
            "CROSSPANE_SECRET_SERVICE_LIVE",
            "CROSSPANE_LIVE_HYPR",
            "CROSSPANE_PRIVATE_PIPEWIRE",
            "CROSSPANE_INJECT_PRIVATE_NEST",
            "CROSSPANE_LIVE_SNI",
        ] {
            if values.contains_key(key) {
                return Err("unexpected live opt-in");
            }
        }
        Ok(())
    }
    fn connect_explicit(&self, runtime: &Path, home: &Path, retain: Arc<AtomicBool>) {
        // No connect_to_env, descriptor environment, substring guard, or owner fallback.
        let state = runtime.join("crosspane-hypr-wp415");
        let recorded = read_nest_record(&state).unwrap();
        assert_eq!(owned_start_ticks(recorded.0).unwrap(), Some(recorded.1));
        let mut command = private_command(runtime, home);
        command.args(["/usr/bin/hyprctl", "instances", "-j"]);
        let instances = serde_json::from_str(&run_bounded(command, retain)).unwrap();
        // Refresh start time after the bounded inventory command and immediately before connect.
        self.verify_fresh(recorded, owned_start_ticks(recorded.0).unwrap(), &instances)
            .unwrap();
        let address = rustix::net::SocketAddrUnix::new(runtime.join(&self.display)).unwrap();
        let fd = rustix::net::socket_with(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        connect_before(
            Instant::now() + Duration::from_millis(250),
            || match rustix::net::connect(&fd, &address) {
                Ok(()) | Err(rustix::io::Errno::ISCONN) => Ok(()),
                Err(
                    rustix::io::Errno::AGAIN
                    | rustix::io::Errno::INPROGRESS
                    | rustix::io::Errno::ALREADY,
                ) => Err(std::io::ErrorKind::WouldBlock.into()),
                Err(error) => Err(error.into()),
            },
        )
        .unwrap();
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let _connection = wayland_client::Connection::from_socket(stream).unwrap();
    }
}
fn read_nest_record(state: &Path) -> std::io::Result<(u32, u64)> {
    let meta = fs::symlink_metadata(state)?;
    let uid = rustix::process::geteuid().as_raw();
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let read = |name: &str| -> std::io::Result<u64> {
        let file = fs::File::from(rustix::fs::open(
            state.join(name),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        let meta = file.metadata()?;
        if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o022 != 0 || meta.len() > 128 {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
        let mut text = String::new();
        file.take(129).read_to_string(&mut text)?;
        if text.len() > 128 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        text.trim()
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| std::io::ErrorKind::InvalidData.into())
    };
    let pid = u32::try_from(read("pid")?).map_err(|_| std::io::ErrorKind::InvalidData)?;
    Ok((pid, read("start")?))
}
fn isolated_values(selection: &NestSelection) -> BTreeMap<String, String> {
    [
        ("CROSSPANE_NESTED_HYPR", "1"),
        ("HYPRLAND_INSTANCE_SIGNATURE", selection.signature.as_str()),
        ("WAYLAND_DISPLAY", selection.display.as_str()),
        ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
        ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
        ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
        ("PULSE_SERVER", DEAD_PULSE),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
#[test]
fn b2_nested_guard_requires_exact_values_and_rejects_owner_handles_before_connect() {
    let selected = NestSelection::parse("unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=owned_exact\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1\n").unwrap();
    let values = isolated_values(&selected);
    selected.verify(&values).unwrap();
    for (key, bad) in [
        ("HYPRLAND_INSTANCE_SIGNATURE", "owned_exact_neighbor"),
        ("HYPRLAND_INSTANCE_SIGNATURE", "prefix_owned_exact"),
        ("WAYLAND_DISPLAY", "wayland-90"),
        ("WAYLAND_DISPLAY", "prefix-wayland-9"),
        ("CROSSPANE_NESTED_HYPR", "01"),
        ("CROSSPANE_NESTED_HYPR", "1-extra"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        (
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/run/dbus/system_bus_socket",
        ),
        ("PIPEWIRE_REMOTE", "pipewire-0"),
        ("PULSE_SERVER", "unix:/owner/pulse"),
        ("WAYLAND_SOCKET", "3"),
        ("CROSSPANE_AUDIO", "1"),
        ("CROSSPANE_LIVE_SNI", "1"),
    ] {
        let mut changed = values.clone();
        changed.insert(key.into(), bad.into());
        assert!(
            selected.verify(&changed).is_err(),
            "guard must refuse {key}"
        );
    }
    for missing in values.keys() {
        let mut changed = values.clone();
        changed.remove(missing);
        assert!(selected.verify(&changed).is_err());
    }
    for malformed in [
        "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=../foreign\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1\n",
        "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE=owned_exact\nexport WAYLAND_DISPLAY=wayland-9\nexport CROSSPANE_NESTED_HYPR=1",
    ] {
        assert!(NestSelection::parse(malformed).is_err());
    }
}
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .into()
}
fn private_command(runtime: &Path, home: &Path) -> Command {
    let mut command = Command::new(repository().join("scripts/lead/impl-env.sh"));
    command
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("LC_ALL", "C"),
            ("DBUS_SESSION_BUS_ADDRESS", DEAD_SESSION),
            ("DBUS_SYSTEM_BUS_ADDRESS", DEAD_SYSTEM),
            ("PIPEWIRE_REMOTE", DEAD_PIPEWIRE),
            ("PULSE_SERVER", DEAD_PULSE),
            ("CARGO_BUILD_JOBS", "2"),
            ("GALLIUM_DRIVER", "llvmpipe"),
        ])
        .env("XDG_RUNTIME_DIR", runtime)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}
fn reap_before(deadline: Instant, mut probe: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if probe() {
            return Instant::now() < deadline;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}
fn connect_before(
    deadline: Instant,
    mut connect: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let result = connect();
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            result => return result,
        }
    }
}
fn stage_bounded(
    mut source: impl Read,
    target: &Path,
    limit: u64,
) -> std::io::Result<([u8; 32], fs::File)> {
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(target)?;
    let mut digest = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut copied = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        let room = (limit - copied).min(buffer.len() as u64) as usize;
        if room == 0 {
            if source.read(&mut buffer[..1])? != 0 {
                return Err(std::io::ErrorKind::FileTooLarge.into());
            }
            break;
        }
        let n = source.read(&mut buffer[..room])?;
        if n == 0 {
            break;
        }
        target.write_all(&buffer[..n])?;
        digest.update(&buffer[..n]);
        copied += n as u64;
    }
    let digest = digest
        .finish()
        .as_ref()
        .try_into()
        .map_err(|_| std::io::ErrorKind::InvalidData)?;
    Ok((digest, target))
}
fn stage_artifact(source: &Path, target: &Path, limit: u64) -> std::io::Result<[u8; 32]> {
    let mut source = fs::File::from(rustix::fs::open(
        source,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    let before = source.metadata()?;
    if !before.is_file()
        || before.uid() != rustix::process::geteuid().as_raw()
        // Cargo's source and deps executable are hard links. The private copy is single-link.
        || before.nlink() == 0
        || before.mode() & 0o7022 != 0
        || before.len() > limit
    {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let (digest, staged) = stage_bounded(&mut source, target, limit)?;
    let after = source.metadata()?;
    let copied = staged.metadata()?;
    let named = fs::symlink_metadata(target)?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.uid() != after.uid()
        || before.mode() != after.mode()
        || before.nlink() != after.nlink()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || !copied.is_file()
        || copied.uid() != before.uid()
        || copied.nlink() != 1
        || copied.mode() & 0o7777 != 0o700
        || copied.len() != after.len()
        || named.dev() != copied.dev()
        || named.ino() != copied.ino()
    {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(digest)
}
#[test]
fn b2_stale_nested_record_is_refused_before_connect() {
    let root = Scratch::new();
    let selected = NestSelection {
        signature: "owned_exact".into(),
        display: "wayland-9".into(),
    };
    let mapped = json!([{"pid":42,"instance":"owned_exact","wl_socket":"wayland-9"}]);
    for (start, instances) in [
        (None, mapped.clone()),
        (Some(101), mapped.clone()),
        (
            Some(100),
            json!([{"pid":43,"instance":"owned_exact","wl_socket":"wayland-9"}]),
        ),
        (
            Some(100),
            json!([{"pid":42,"instance":"owner_other","wl_socket":"wayland-9"}]),
        ),
        (
            Some(100),
            json!([{"pid":42,"instance":"owned_exact","wl_socket":"wayland-10"}]),
        ),
        (Some(100), json!([mapped[0].clone(), mapped[0].clone()])),
    ] {
        // Scratch records and injected current facts only: no proc probe or socket connection.
        fs::write(root.0.join("pid"), b"42\n").unwrap();
        fs::write(root.0.join("start"), b"100\n").unwrap();
        let recorded = read_nest_record(&root.0).unwrap();
        assert!(selected.verify_fresh(recorded, start, &instances).is_err());
    }
    selected
        .verify_fresh((42, 100), Some(100), &mapped)
        .unwrap();
}
#[test]
fn b2_nonblocking_connect_deadline_refuses_a_stalled_scratch_endpoint() {
    let root = Scratch::new();
    let _listener = UnixListener::bind(root.0.join("inert.sock")).unwrap();
    let deadline = Instant::now() + Duration::from_millis(1);
    let result = connect_before(deadline, || {
        thread::sleep(Duration::from_millis(3));
        Ok(())
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    let mut attempts = 0;
    let result = connect_before(Instant::now() + Duration::from_millis(10), || {
        attempts += 1;
        Err(std::io::ErrorKind::WouldBlock.into())
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(attempts > 1);
}
#[test]
fn b2_reap_deadline_preserves_owned_evidence_until_confirmation() {
    let root = Scratch::new();
    let record = root.0.join("owned-record");
    fs::write(&record, b"42 100\n").unwrap();
    let result = reap_before(Instant::now(), || true);
    assert!(!result, "a probe beyond the grace cannot confirm cleanup");
    assert!(record.exists());
    struct FakeChild {
        reaped: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        signalled: Arc<AtomicBool>,
    }
    impl ReapOwned for FakeChild {
        fn signal_owned(&mut self) {
            self.signalled.store(true, Ordering::Release);
        }
        fn reaped(&mut self) -> bool {
            self.reaped.load(Ordering::Acquire)
        }
    }
    impl Drop for FakeChild {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    let reaped = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let signalled = Arc::new(AtomicBool::new(false));
    let owner = OwnedProcess {
        pending: Some(Cleanup {
            child: FakeChild {
                reaped: reaped.clone(),
                dropped: dropped.clone(),
                signalled: signalled.clone(),
            },
            record: record.clone(),
            _slot: HarnessSlot::admit(),
        }),
        retain: root.1.clone(),
        grace: Duration::ZERO,
    };
    drop(owner);
    assert!(root.1.load(Ordering::Acquire));
    assert!(signalled.load(Ordering::Acquire));
    assert!(!dropped.load(Ordering::Acquire));
    assert!(record.exists());
    assert_eq!(HARNESS_CHILDREN.load(Ordering::Acquire), 1);
    reaped.store(true, Ordering::Release);
    until(|| dropped.load(Ordering::Acquire));
    assert!(!record.exists());
    until(|| HARNESS_CHILDREN.load(Ordering::Acquire) == 0);
    root.1.store(false, Ordering::Release);
}
#[test]
fn b2_held_artifact_growth_cannot_exceed_copy_limit() {
    let root = Scratch::new();
    let source = root.0.join("source");
    fs::write(&source, b"1234").unwrap();
    let held = fs::File::open(&source).unwrap();
    fs::write(&source, b"1234567890123456").unwrap();
    let target = root.0.join("staged");
    assert!(stage_bounded(held, &target, 8).is_err());
    assert!(target.metadata().map_or(true, |m| m.len() <= 8));
}
#[test]
fn b2_bounded_0755_cargo_artifact_stages_into_exact_private_prefix() {
    let a = Admission::new();
    fs::remove_file(&a.path).unwrap();
    let paths = a.io.target().paths();
    for path in [
        &paths.prefix,
        &paths.config_home,
        &paths.state_home,
        &paths.data_home,
        &paths.runtime_home,
        &paths.prefix.join("bin"),
        &paths.runtime_home.join("private"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(a.path, a.root.0.join(".local/bin/crosspane-tutorial"));
    let build = a.root.0.join("build/deps");
    fs::create_dir_all(&build).unwrap();
    let source = a.root.0.join("build/crosspane-tutorial");
    // Use a bounded ELF prefix as synthetic data, independent of debug-profile size.
    // Neither this source nor its staged destination is ever executed by the regression.
    const SOURCE_BYTES: u64 = 8 * 1024 * 1024;
    let current = std::env::current_exe().unwrap();
    let held = fs::File::from(
        rustix::fs::open(
            current,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    );
    let (expected, _held_copy) =
        stage_bounded(held.take(SOURCE_BYTES), &source, SOURCE_BYTES).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
    fs::hard_link(&source, build.join("crosspane_tutorial-test")).unwrap();
    let meta = source.metadata().unwrap();
    assert!(meta.len() > 0 && meta.len() <= SOURCE_BYTES);
    assert_eq!(meta.mode() & 0o7777, 0o755);
    assert_eq!(
        meta.nlink(),
        2,
        "Cargo's executable and deps paths share the source inode"
    );
    assert!(!source.starts_with(&paths.prefix));
    let mut magic = [0; 4];
    fs::File::open(&source)
        .unwrap()
        .read_exact(&mut magic)
        .unwrap();
    assert_eq!(magic, *b"\x7fELF");
    let digest = stage_artifact(&source, &a.path, MAX_FILE_BYTES as u64).unwrap();
    assert_eq!(digest, expected);
    let staged = a.path.metadata().unwrap();
    assert_eq!(staged.mode() & 0o7777, 0o700);
    assert_eq!(staged.nlink(), 1);
    assert_eq!(staged.uid(), paths.uid);
    assert_eq!(staged.len(), meta.len());
    a.io.admit_tutorial(
        &a.proof,
        &a.environment,
        digest,
        &a.font,
        &Deadline::new(2000, Cancellation::default()).unwrap(),
    )
    .unwrap();
    assert!(
        a.probe.facts.lock().unwrap().is_empty(),
        "no process probe or spawn"
    );
}
#[test]
fn b2_stable_id_rejects_signed_and_non_ascii_hex() {
    for id in [
        "+1800000c",
        "0x+1800000c",
        "-1800000c",
        " 1800000c",
        "1800000c ",
        "0x",
        "１８０００００c",
    ] {
        let mut c = client();
        c["stableId"] = json!(id);
        assert!(
            observe(
                &mut tutorial::WindowObserver::default(),
                json!([c]),
                monitors()
            )
            .is_err(),
            "{id}"
        );
    }
}
static HARNESS_CHILDREN: AtomicUsize = AtomicUsize::new(0);
struct HarnessSlot;
impl HarnessSlot {
    fn admit() -> Self {
        HARNESS_CHILDREN
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .unwrap();
        Self
    }
}
impl Drop for HarnessSlot {
    fn drop(&mut self) {
        HARNESS_CHILDREN.fetch_sub(1, Ordering::AcqRel);
    }
}
trait ReapOwned: Send + 'static {
    fn signal_owned(&mut self);
    fn reaped(&mut self) -> bool;
}
impl ReapOwned for Child {
    fn signal_owned(&mut self) {
        // An error (including lost wait ownership) cannot authorize a signal.
        if self.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.kill();
        }
    }
    fn reaped(&mut self) -> bool {
        self.try_wait().is_ok_and(|s| s.is_some())
    }
}
struct Cleanup<C> {
    child: C,
    record: PathBuf,
    _slot: HarnessSlot,
}
struct OwnedProcess<C: ReapOwned = Child> {
    pending: Option<Cleanup<C>>,
    retain: Arc<AtomicBool>,
    grace: Duration,
}
impl OwnedProcess {
    fn spawn(mut command: Command, retain: Arc<AtomicBool>) -> Self {
        let slot = HarnessSlot::admit();
        let home = command
            .get_envs()
            .find(|(key, _)| *key == "HOME")
            .and_then(|(_, value)| value)
            .unwrap();
        let record = Path::new(home).join(format!(
            ".owned-command-{}",
            SERIAL.fetch_add(1, Ordering::SeqCst)
        ));
        let child = command.spawn().unwrap();
        let mut owned = Self {
            pending: Some(Cleanup {
                child,
                record: record.clone(),
                _slot: slot,
            }),
            retain,
            grace: Duration::from_secs(3),
        };
        let pid = owned.child().id();
        let start = owned_start_ticks(pid).ok().flatten();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(record)
            .unwrap();
        writeln!(file, "pid={pid} start={start:?}").unwrap();
        owned
    }
    fn child(&mut self) -> &mut Child {
        &mut self.pending.as_mut().unwrap().child
    }
}
impl<C: ReapOwned> Drop for OwnedProcess<C> {
    fn drop(&mut self) {
        // Only a Child created by this fixture is signalled, never a PID from IPC/environment.
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        if !pending.child.reaped() {
            pending.child.signal_owned();
        }
        if reap_before(Instant::now() + self.grace, || pending.child.reaped()) {
            let _ = fs::remove_file(&pending.record);
            return;
        }
        self.retain.store(true, Ordering::Release);
        eprintln!(
            "owned command reaping outstanding; retaining {}",
            pending.record.display()
        );
        // Retain both Child and its four-slot admission until reaped, without blocking Drop.
        let pending = Arc::new(Mutex::new(Some(pending)));
        let worker = pending.clone();
        if thread::Builder::new()
            .name("owned-harness-reap".into())
            .spawn(move || {
                loop {
                    let mut held = worker.lock().unwrap();
                    if held.as_mut().unwrap().child.reaped() {
                        let done = held.take().unwrap();
                        let _ = fs::remove_file(&done.record);
                        return;
                    }
                    drop(held);
                    thread::sleep(Duration::from_millis(10));
                }
            })
            .is_err()
        {
            // Bounded by HARNESS_CHILDREN; failed thread creation must not lose ownership.
            static RETAINED: OnceLock<Mutex<Vec<Box<dyn Send>>>> = OnceLock::new();
            RETAINED
                .get_or_init(Mutex::default)
                .lock()
                .unwrap()
                .push(Box::new(pending));
        }
    }
}
fn read_bounded(mut pipe: impl Read + std::os::fd::AsFd) -> String {
    let flags = rustix::fs::fcntl_getfl(&pipe).unwrap();
    rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(Instant::now() < deadline, "owned command pipe deadline");
        match pipe.read(&mut buffer) {
            Ok(0) => return String::from_utf8(bytes).unwrap(),
            Ok(n) => {
                assert!(bytes.len() + n <= 4096, "owned command pipe bound");
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(e) => panic!("owned command pipe: {e}"),
        }
    }
}
fn run_bounded(command: Command, retain: Arc<AtomicBool>) -> String {
    let mut child = OwnedProcess::spawn(command, retain);
    let deadline = Instant::now() + Duration::from_secs(35);
    let status = loop {
        if let Some(status) = child.child().try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "owned harness command deadline");
        thread::sleep(Duration::from_millis(10));
    };
    let out = read_bounded(child.child().stdout.take().unwrap());
    let err = read_bounded(child.child().stderr.take().unwrap());
    assert!(
        status.success(),
        "owned harness command failed: {err}; {out}"
    );
    out
}
struct InnerNest {
    runtime: PathBuf,
    home: PathBuf,
    selection: NestSelection,
    retain: Arc<AtomicBool>,
}
impl InnerNest {
    fn start(runtime: &Path, home: &Path, parent: &Path, retain: Arc<AtomicBool>) -> Self {
        let mut owned = Self {
            runtime: runtime.into(),
            home: home.into(),
            selection: NestSelection {
                signature: String::new(),
                display: String::new(),
            },
            retain,
        };
        let mut command = private_command(runtime, home);
        command
            .env("CROSSPANE_PARENT_WAYLAND_DISPLAY", parent)
            .args(["bash", "-c", "umask 077; exec \"$@\"", "--"])
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["start", "--name", "wp415"]);
        run_bounded(command, owned.retain.clone());
        let mut command = private_command(runtime, home);
        command
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["env", "--name", "wp415"]);
        owned.selection =
            NestSelection::parse(&run_bounded(command, owned.retain.clone())).unwrap();
        owned
            .selection
            .verify(&isolated_values(&owned.selection))
            .unwrap();
        owned
            .selection
            .connect_explicit(runtime, home, owned.retain.clone());
        owned
    }
}
impl Drop for InnerNest {
    fn drop(&mut self) {
        let state = self.runtime.join("crosspane-hypr-wp415");
        let recorded = fs::read_to_string(state.join("pid"))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
            .zip(
                fs::read_to_string(state.join("start"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok()),
            );
        if thread::panicking() {
            let path = self.runtime.join("crosspane-hypr-wp415/stdout.log");
            if let Ok(file) = fs::File::open(path) {
                let mut log = String::new();
                let _ = file.take(8192).read_to_string(&mut log);
                eprintln!("owned private nest startup diagnostic: {log}");
            }
        }
        let mut command = private_command(&self.runtime, &self.home);
        command
            .arg(repository().join("scripts/hypr-nested.sh"))
            .args(["stop", "--name", "wp415"]);
        // Finally-style cleanup also runs during unwinding; avoid a second panic.
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_bounded(command, self.retain.clone())
        }))
        .is_ok();
        let deadline = Instant::now() + Duration::from_secs(3);
        let gone = recorded.is_some_and(|(pid, start)| {
            loop {
                match owned_start_ticks(pid) {
                    Ok(None) => return true,
                    Ok(Some(now)) if now != start => return true,
                    _ if Instant::now() >= deadline => return false,
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        if !stopped || !gone {
            self.retain.store(true, Ordering::Release);
            eprintln!(
                "owned nest stop/reaping not confirmed; preserving private records at {}; recorded={recorded:?}",
                state.display()
            );
        } else {
            eprintln!("owned private nest stop and PID/start disappearance confirmed");
        }
    }
}
fn owned_start_ticks(pid: u32) -> std::io::Result<Option<u64>> {
    let file = match fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    let ticks = (text.len() <= 4096)
        .then(|| {
            text.rsplit_once(") ")
                .and_then(|(_, fields)| fields.split_whitespace().nth(19))
                .and_then(|s| s.parse::<u64>().ok())
        })
        .flatten()
        .ok_or(std::io::ErrorKind::InvalidData)?;
    Ok(Some(ticks))
}
struct OwnedTutorialProbe {
    executable: PathBuf,
    uid: u32,
}
impl ProcessProbe for OwnedTutorialProbe {
    fn snapshot(
        &self,
        pid: u32,
        deadline: &Deadline,
    ) -> std::result::Result<ProcessFacts, NativeError> {
        // The only caller is spawn_tutorial, using its held unreaped Child's PID.
        deadline.check()?;
        let path = PathBuf::from(format!("/proc/{pid}"));
        let uid = fs::metadata(&path)
            .map_err(|_| NativeError::Unavailable)?
            .uid();
        let executable = fs::read_link(path.join("exe")).map_err(|_| NativeError::Unavailable)?;
        if uid != self.uid || executable != self.executable {
            return Err(NativeError::Foreign);
        }
        let mut stat = String::new();
        fs::File::open(path.join("stat"))
            .map_err(|_| NativeError::Unavailable)?
            .take(4097)
            .read_to_string(&mut stat)
            .map_err(|_| NativeError::Unavailable)?;
        if stat.len() > 4096 {
            return Err(NativeError::Oversize);
        }
        let generation = stat
            .rsplit_once(") ")
            .and_then(|(_, fields)| fields.split_whitespace().nth(19))
            .and_then(|s| s.parse().ok())
            .filter(|ticks| *ticks > 0)
            .ok_or(NativeError::Invalid)?;
        deadline.check()?;
        Ok(ProcessFacts {
            uid,
            executable,
            generation,
        })
    }
}
fn fixture_reply(port: &mut tutorial::LinuxFixturePort, call: FixtureCall) -> FixtureMessage {
    let id = call.id;
    port.submit(call).unwrap();
    let mut answer = None;
    until(|| {
        for receipt in port.poll_receipts() {
            let packet = receipt.message;
            if packet.call_id == Some(id) {
                answer = Some(packet);
            } else {
                assert!(packet.result.is_ok(), "unexpected fixture error");
            }
        }
        answer.is_some()
    });
    answer.unwrap()
}
#[test]
fn nested_fixture_owned_window_and_closed_evidence() {
    if std::env::var_os("CROSSPANE_NESTED_HYPR").is_none() {
        eprintln!("window-only runtime skipped: named nested guard is unset; no audio coverage");
        return;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let state = runtime.join("crosspane-hypr-wp415");
    let meta = fs::symlink_metadata(&state).unwrap();
    assert!(
        meta.is_dir()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o022 == 0,
        "named harness state must be owned"
    );
    let text = fs::read_to_string(state.join("env")).unwrap();
    let outer = NestSelection::parse(&text).unwrap();
    outer.verify(&std::env::vars().collect()).unwrap();
    // Recheck current owned PID/start and exact instance/display immediately before connecting.
    outer.connect_explicit(&runtime, &state, Arc::new(AtomicBool::new(false)));
    if !Path::new("/usr/bin/dbus-daemon").is_file() {
        eprintln!(
            "window-only runtime skipped: private dbus-daemon is unavailable; no audio coverage"
        );
        return;
    }
    let root_path = Scratch::path();
    let executable = root_path.join(".local/bin/crosspane-tutorial");
    let io = Arc::new(
        LinuxNativeIo::scratch(
            &root_path,
            Arc::new(NoCommands),
            Arc::new(OwnedTutorialProbe {
                executable: executable.clone(),
                uid: rustix::process::geteuid().as_raw(),
            }),
        )
        .unwrap(),
    );
    let root = Scratch::owned(root_path);
    let paths = io.target().paths();
    for path in [
        &paths.prefix,
        &paths.config_home,
        &paths.state_home,
        &paths.data_home,
        &paths.runtime_home,
        &paths.prefix.join("bin"),
        &paths.runtime_home.join("private"),
    ] {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let inner = InnerNest::start(
        &paths.runtime_home,
        &root.0,
        &runtime.join(&outer.display),
        root.1.clone(),
    );
    let bus = paths.runtime_home.join("private/bus");
    let bus_address = format!("unix:path={}", bus.display());
    let mut command = private_command(&paths.runtime_home, &root.0);
    command
        .args([
            "bash",
            "-c",
            "umask 077; exec \"$@\"",
            "--",
            "/usr/bin/dbus-daemon",
            "--session",
            "--nofork",
            "--nopidfile",
            "--nosyslog",
        ])
        .arg(format!("--address={bus_address}"));
    let mut bus_child = OwnedProcess::spawn(command, root.1.clone());
    until(|| {
        assert!(bus_child.child().try_wait().unwrap().is_none());
        bus.exists()
    });
    let source = Path::new(env!("CARGO_BIN_EXE_crosspane-tutorial"));
    let digest = stage_artifact(source, &executable, MAX_FILE_BYTES as u64).unwrap();
    let environment = ChildEnvironment::selected(
        io.target(),
        [
            ("XDG_SESSION_ID".into(), "fixture".into()),
            ("XDG_SESSION_TYPE".into(), "wayland".into()),
            (
                "HYPRLAND_INSTANCE_SIGNATURE".into(),
                inner.selection.signature.clone(),
            ),
            ("WAYLAND_DISPLAY".into(), inner.selection.display.clone()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), bus_address),
        ]
        .into(),
    )
    .unwrap();
    assert_eq!(
        environment.values()["XDG_RUNTIME_DIR"],
        paths.runtime_home.to_str().unwrap()
    );
    assert_eq!(environment.values()["DBUS_SYSTEM_BUS_ADDRESS"], DEAD_SYSTEM);
    for key in [
        "PIPEWIRE_REMOTE",
        "PULSE_SERVER",
        "CROSSPANE_AUDIO",
        "CROSSPANE_PARENT_WAYLAND_DISPLAY",
    ] {
        assert!(!environment.values().contains_key(key));
    }
    let font = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
    let start = Instant::now();
    let clock: FixtureClock = Arc::new(move || start.elapsed().as_millis() as u64);
    let mut preparing =
        tutorial::launch(io.clone(), proof(&io), environment, digest, font, clock).unwrap();
    let mut port = None;
    until(|| {
        if let Some(result) = preparing.poll() {
            port = Some(result.unwrap());
        }
        port.is_some()
    });
    let mut port = port.unwrap();
    let opened = fixture_reply(
        &mut port,
        FixtureCall {
            id: 1,
            attempt: AttemptId(1),
            command: FixtureCommand::Open {
                machine_label: "TEST_MACHINE".into(),
            },
        },
    );
    let (fixture, pid, window) = match opened.result.unwrap() {
        FixtureEvent::Opened {
            fixture,
            pid,
            window,
            ..
        } => (fixture, pid, window),
        other => panic!("expected opened, got {other:?}"),
    };
    let ipc = TutorialIpc::new(&inner.selection.signature, &paths.runtime_home).unwrap();
    let clients = ipc.json("clients").unwrap();
    let own: Vec<_> = clients
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["pid"].as_u64() == Some(u64::from(pid)) && c["title"].as_str() == Some(TITLE))
        .collect();
    assert_eq!(own.len(), 1);
    let stable = own[0]["stableId"].as_str().unwrap();
    assert_eq!(
        window,
        WindowId(u64::from_str_radix(stable.strip_prefix("0x").unwrap_or(stable), 16).unwrap())
    );
    let observed = fixture_reply(
        &mut port,
        FixtureCall {
            id: 2,
            attempt: AttemptId(1),
            command: FixtureCommand::ObserveWindow { fixture },
        },
    );
    assert!(matches!(
        observed.result,
        Ok(FixtureEvent::Snapshot {
            snapshot: FixtureSnapshot {
                window_facts: OwnWindowFacts::Present { .. },
                ..
            }
        })
    ));
    let armed = fixture_reply(
        &mut port,
        FixtureCall {
            id: 3,
            attempt: AttemptId(1),
            command: FixtureCommand::ArmTarget {
                fixture,
                phase: PhaseId(1),
            },
        },
    );
    assert!(matches!(
        armed.result,
        Ok(FixtureEvent::TargetArmed {
            phase: PhaseId(1),
            ..
        })
    ));
    // Close itself is not success. Accept Closed only from the owned cleanup path.
    port.submit(FixtureCall {
        id: 4,
        attempt: AttemptId(1),
        command: FixtureCommand::Close { fixture },
    })
    .unwrap();
    let mut terminal = None;
    until(|| {
        for receipt in port.poll_receipts() {
            if matches!(
                receipt.message.result,
                Ok(FixtureEvent::Closed { .. }) | Err(FixtureError::ChildExited)
            ) {
                assert!(terminal.is_none());
                terminal = Some(receipt.message.result);
            }
        }
        let _ = port.complete_closed(AttemptId(1), fixture);
        terminal.is_some()
    });
    if matches!(terminal, Some(Ok(FixtureEvent::Closed { .. }))) {
        assert!(
            ipc.json("clients")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["pid"].as_u64() != Some(u64::from(pid)))
        );
        eprintln!("window-only owned runtime: Closed confirmed by reaping and fresh absence");
    } else {
        eprintln!(
            "window-only owned runtime: EOF cleanup unconfirmed, honest ChildExited; no Closed claim"
        );
    }
    assert!(port.poll_receipts().is_empty());
    drop(port);
    drop(bus_child);
    drop(inner);
    assert!(
        !root.1.load(Ordering::Acquire),
        "owned inner nest cleanup must be proven before removing its records"
    );
    drop(root);
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.1.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "bounded fake wait");
        thread::sleep(Duration::from_millis(1));
    }
}
fn client() -> Value {
    json!({"pid":77,"title":TITLE,"stableId":"1800000c","address":"0xdeadbeef","mapped":true,"hidden":false,"monitor":2,"workspace":{"id":1,"name":"1"}})
}
fn monitors() -> Value {
    json!([{"id":2,"activeWorkspace":{"id":1},"specialWorkspace":{"id":0}},{"id":3,"activeWorkspace":{"id":2},"specialWorkspace":{"id":0}}])
}
fn observe(
    observer: &mut tutorial::WindowObserver,
    clients: Value,
    monitors: Value,
) -> tutorial_window::WindowObservation {
    observer.observe(&clients, &monitors, 77, TITLE)
}

#[test]
fn b2_stable_id_mapping_uses_exact_pid_title_and_never_address() {
    let mut observer = tutorial::WindowObserver::default();
    let mut foreign = client();
    foreign["pid"] = json!(78);
    foreign["title"] = json!("UNRELATED_SYNTHETIC_TITLE");
    foreign["stableId"] = json!("1800000d");
    let (id, facts) = observe(&mut observer, json!([foreign, client()]), monitors()).unwrap();
    assert_eq!(id, WindowId(0x1800000c));
    assert_eq!(
        facts,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: Some(true)
        }
    );
    for stable in ["0x1800000c", "1800000C"] {
        let mut c = client();
        c["stableId"] = json!(stable);
        c["address"] = json!("INVALID_IGNORED_ADDRESS");
        assert_eq!(
            observe(&mut observer, json!([c]), monitors()).unwrap().0,
            id
        );
    }
}
#[test]
fn b2_missing_before_identity_and_ambiguity_do_not_guess_titles() {
    let mut observer = tutorial::WindowObserver::default();
    assert_eq!(
        observe(&mut observer, json!([]), monitors()),
        Err(FixtureError::UnknownWindow)
    );
    assert_eq!(
        observe(&mut observer, json!([client(), client()]), monitors()),
        Err(FixtureError::AmbiguousWindow)
    );
    let mut c = client();
    c["title"] = json!(format!("{TITLE} suffix"));
    assert_eq!(
        observe(&mut observer, json!([c]), monitors()),
        Err(FixtureError::UnknownWindow)
    );
    assert_eq!(
        observer.observe(&json!([client()]), &monitors(), 77, "uncontrolled"),
        Err(FixtureError::NotOwned)
    );
}
#[test]
fn b2_established_identity_never_rebinds_pid_stable_id_or_title() {
    for field in ["pid", "stableId", "title"] {
        let mut observer = tutorial::WindowObserver::default();
        observe(&mut observer, json!([client()]), monitors()).unwrap();
        let mut changed = client();
        changed[field] = match field {
            "pid" => json!(78),
            "stableId" => json!("ff"),
            _ => json!("OTHER_SYNTHETIC_TITLE"),
        };
        assert_eq!(
            observe(&mut observer, json!([changed]), monitors()),
            Err(FixtureError::UnknownWindow)
        );
        assert_eq!(
            observe(&mut observer, json!([]), monitors()),
            Ok((WindowId(0x1800000c), OwnWindowFacts::Missing))
        );
    }
}
#[test]
fn b2_visibility_parked_hidden_unmapped_and_original_display_are_facts() {
    let mut observer = tutorial::WindowObserver::default();
    observe(&mut observer, json!([client()]), monitors()).unwrap();
    for changed in [
        json!({"mapped":false}),
        json!({"hidden":true}),
        json!({"workspace":{"id":-7,"name":"special:crosspane-test"}}),
    ] {
        let mut c = client();
        for (key, value) in changed.as_object().unwrap() {
            c[key] = value.clone();
        }
        assert_eq!(
            observe(&mut observer, json!([c]), monitors()).unwrap().1,
            OwnWindowFacts::Present {
                visible_on_user_workspace: Some(false),
                on_initial_display: Some(true)
            }
        );
    }
    let mut moved = client();
    moved["monitor"] = json!(3);
    moved["workspace"]["id"] = json!(2);
    assert_eq!(
        observe(&mut observer, json!([moved]), monitors())
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: Some(false)
        }
    );
    assert_eq!(
        observe(&mut observer, json!([client()]), Value::Null)
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: None,
            on_initial_display: None
        }
    );
    let mut unknown = tutorial::WindowObserver::default();
    observe(&mut unknown, json!([client()]), Value::Null).unwrap();
    assert_eq!(
        observe(&mut unknown, json!([client()]), monitors())
            .unwrap()
            .1,
        OwnWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: None
        }
    );
}
#[test]
fn b2_malformed_and_bounded_observations_refuse_without_window_success() {
    for id in [
        json!(""),
        json!("0"),
        json!("not-hex"),
        json!(null),
        json!("fffffffffffffffff"),
    ] {
        let mut c = client();
        c["stableId"] = id;
        assert!(
            observe(
                &mut tutorial::WindowObserver::default(),
                json!([c]),
                monitors()
            )
            .is_err()
        );
    }
    assert!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!({}),
            monitors()
        )
        .is_err()
    );
    assert!(
        observe(
            &mut tutorial::WindowObserver::default(),
            json!(vec![client(); 4097]),
            monitors()
        )
        .is_err()
    );
}

struct Server {
    root: Scratch,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    values: Arc<Mutex<(Value, Value)>>,
    calls: Arc<Mutex<Vec<String>>>,
    delay: Arc<AtomicU64>,
}
impl Server {
    fn new() -> Self {
        let root = Scratch::new();
        let parent = root.0.join("hypr/fixture");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(root.0.join("hypr"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join(".socket.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let values = Arc::new(Mutex::new((json!([client()]), monitors())));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let delay = Arc::new(AtomicU64::new(0));
        let (s, v, c, d) = (stop.clone(), values.clone(), calls.clone(), delay.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if s.load(Ordering::Acquire) {
                    return;
                }
                stream
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                let mut bytes = [0; 64];
                let n = stream.read(&mut bytes).unwrap();
                let command = String::from_utf8(bytes[..n].to_vec()).unwrap();
                c.lock().unwrap().push(command.clone());
                thread::sleep(Duration::from_millis(d.load(Ordering::Acquire)));
                let values = v.lock().unwrap();
                let value = match command.as_str() {
                    "j/clients" => &values.0,
                    "j/monitors" => &values.1,
                    _ => panic!("unexpected read-only query"),
                };
                let _ = stream.write_all(serde_json::to_string(value).unwrap().as_bytes());
            }
        });
        Self {
            root,
            path,
            stop,
            thread: Some(worker),
            values,
            calls,
            delay,
        }
    }
    fn ipc(&self) -> TutorialIpc {
        TutorialIpc::new("fixture", &self.root.0).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(worker) = self.thread.take() {
            worker.join().unwrap();
        }
    }
}
#[test]
fn b2_ipc_is_fresh_read_only_and_pins_socket_and_ancestors() {
    let server = Server::new();
    let ipc = server.ipc();
    assert_eq!(ipc.json("clients").unwrap(), json!([client()]));
    ipc.json("monitors").unwrap();
    server.values.lock().unwrap().0 = json!([]);
    assert_eq!(ipc.json("clients").unwrap(), json!([]));
    assert!(ipc.json("dispatch").is_err());
    assert_eq!(
        *server.calls.lock().unwrap(),
        ["j/clients", "j/monitors", "j/clients"]
    );
    let saved = server.path.with_extension("saved");
    fs::rename(&server.path, &saved).unwrap();
    let replacement = UnixListener::bind(&server.path).unwrap();
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(ipc.json("clients").is_err());
    drop(replacement);
    fs::remove_file(&server.path).unwrap();
    fs::rename(saved, &server.path).unwrap();
    let old = server.root.0.join("hypr");
    let saved = server.root.0.join("saved");
    fs::rename(&old, &saved).unwrap();
    symlink(&saved, &old).unwrap();
    assert!(ipc.json("clients").is_err());
    fs::remove_file(&old).unwrap();
    fs::rename(saved, old).unwrap();
}
#[test]
fn b2_ipc_stall_is_bounded_and_signature_or_writable_socket_refused() {
    let server = Server::new();
    for signature in ["", ".", "..", "fixture/other", "fixture\n", "fixture."] {
        assert!(TutorialIpc::new(signature, &server.root.0).is_err());
    }
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(TutorialIpc::new("fixture", &server.root.0).is_err());
    fs::set_permissions(&server.path, fs::Permissions::from_mode(0o700)).unwrap();
    server.delay.store(350, Ordering::Release);
    let started = Instant::now();
    assert!(server.ipc().json("clients").is_err());
    assert!(started.elapsed() < Duration::from_millis(650));
}
#[test]
fn b3b_invalid_selector_and_unowned_stop_refuse_without_output() {
    let mut native = tutorial::native(Path::new("/inert/font"));
    assert_eq!(
        native.play_tone(
            ToneId(1),
            &SpeakersSelection {
                peer: crosspane_types::id::NodeId([1; 32]),
                device_key: "inert".into()
            }
        ),
        Some(Err(FixtureError::NotOwned))
    );
    assert_eq!(
        native.stop_tone(ToneId(1)),
        Some(Err(FixtureError::NotOwned))
    );
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
}

struct NoCommands;
impl CommandRunner for NoCommands {
    fn run(
        &self,
        _: &CommandSpec,
        _: &Deadline,
    ) -> std::result::Result<CommandOutput, NativeError> {
        panic!("native commands forbidden in ordinary test")
    }
}
struct Probe {
    facts: Mutex<VecDeque<ProcessFacts>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, _: &Deadline) -> std::result::Result<ProcessFacts, NativeError> {
        let mut facts = self.facts.lock().unwrap();
        if facts.len() > 1 {
            Ok(facts.pop_front().unwrap())
        } else {
            facts.front().cloned().ok_or(NativeError::Unavailable)
        }
    }
}
struct Admission {
    io: Arc<LinuxNativeIo>,
    root: Scratch,
    probe: Arc<Probe>,
    proof: SupportProof,
    environment: ChildEnvironment,
    font: PathBuf,
    path: PathBuf,
}
fn proof(io: &LinuxNativeIo) -> SupportProof {
    io.scratch_support(SupportObservations {
        uid: io.target().paths().uid,
        architecture: std::env::consts::ARCH.into(),
        arch_based: true,
        hyprland_version: [0, 56, 0],
        protocols_ready: true,
        runtime_libraries_ready: true,
        uwsm_managed: true,
        graphical_target_active: true,
        graphical_sessions: 1,
        session_id: "fixture".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    })
    .unwrap()
}
impl Admission {
    fn new() -> Self {
        let path = Scratch::path();
        let probe = Arc::new(Probe {
            facts: Mutex::new(VecDeque::new()),
        });
        let io =
            Arc::new(LinuxNativeIo::scratch(&path, Arc::new(NoCommands), probe.clone()).unwrap());
        let root = Scratch::owned(path);
        fs::create_dir_all(io.target().paths().prefix.join("bin")).unwrap();
        let path = io.target().paths().prefix.join("bin/crosspane-tutorial");
        fs::write(&path, b"INERT_TUTORIAL_BYTES_NEVER_EXECUTED").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let proof = proof(&io);
        let environment = ChildEnvironment::selected(
            io.target(),
            [
                ("XDG_SESSION_ID".into(), "fixture".into()),
                ("XDG_SESSION_TYPE".into(), "wayland".into()),
                ("HYPRLAND_INSTANCE_SIGNATURE".into(), "fixture".into()),
                ("WAYLAND_DISPLAY".into(), "wayland-fixture".into()),
            ]
            .into(),
        )
        .unwrap();
        let font = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
        Self {
            io,
            root,
            probe,
            proof,
            environment,
            font,
            path,
        }
    }
    fn admit(&self) -> std::result::Result<native_io::tutorial::TutorialCommand, NativeError> {
        self.io.admit_tutorial(
            &self.proof,
            &self.environment,
            crosspane_installer::platform::linux::payload::sha256(
                b"INERT_TUTORIAL_BYTES_NEVER_EXECUTED",
            ),
            &self.font,
            &Deadline::new(2000, Cancellation::default()).unwrap(),
        )
    }
}
#[test]
fn b2_native_admission_pins_digest_prefix_font_and_selected_environment() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    assert!(!format!("{command:?}").contains("INERT_TUTORIAL_BYTES"));
    assert!(
        a.io.admit_tutorial(
            &a.proof,
            &a.environment,
            [0; 32],
            &a.font,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
    for font in [
        a.root.0.join("font.ttf"),
        PathBuf::from("/usr/share/fonts/foreign.ttf"),
    ] {
        assert!(
            a.io.admit_tutorial(
                &a.proof,
                &a.environment,
                [0; 32],
                &font,
                &Deadline::new(2000, Cancellation::default()).unwrap()
            )
            .is_err()
        );
    }
    let other = Admission::new();
    assert!(
        a.io.admit_tutorial(
            &a.proof,
            &other.environment,
            [0; 32],
            &a.font,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
    assert!(
        CommandSpec::new(
            a.path.clone(),
            vec![
                "--controlled".into(),
                "--font".into(),
                a.font.to_str().unwrap().into()
            ],
            a.environment.clone(),
            4096
        )
        .is_err()
    );
}
#[test]
fn b2_native_admission_refuses_links_nonexecutables_writes_and_oversize() {
    let a = Admission::new();
    for mode in [0o600, 0o777, 0o4700, 0o2700] {
        fs::set_permissions(&a.path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(a.admit().is_err());
    }
    fs::set_permissions(&a.path, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = a.root.0.join("alias");
    fs::hard_link(&a.path, &alias).unwrap();
    assert!(a.admit().is_err());
    fs::remove_file(alias).unwrap();
    let saved = a.root.0.join("saved");
    fs::rename(&a.path, &saved).unwrap();
    symlink(&saved, &a.path).unwrap();
    assert!(a.admit().is_err());
    fs::remove_file(&a.path).unwrap();
    fs::rename(saved, &a.path).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&a.path)
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    assert!(a.admit().is_err());
}
#[test]
fn b2_tutorial_process_verifier_has_its_own_uid_executable_and_start_ticks() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    let good = ProcessFacts {
        uid: a.io.target().paths().uid,
        executable: a.path.clone(),
        generation: 123,
    };
    *a.probe.facts.lock().unwrap() = vec![good.clone()].into();
    let identity =
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap(),
        )
        .unwrap();
    assert_eq!(identity.start_ticks, 123);
    assert_eq!(identity.executable, a.path);
    for bad in [
        ProcessFacts {
            uid: good.uid + 1,
            ..good.clone()
        },
        ProcessFacts {
            executable: a.io.target().agent_path(),
            ..good.clone()
        },
        ProcessFacts {
            generation: 0,
            ..good.clone()
        },
    ] {
        *a.probe.facts.lock().unwrap() = vec![bad].into();
        assert!(
            a.io.tutorial_identity(
                &command,
                42,
                &Deadline::new(2000, Cancellation::default()).unwrap()
            )
            .is_err()
        );
    }
    *a.probe.facts.lock().unwrap() = vec![
        good.clone(),
        ProcessFacts {
            generation: 124,
            ..good
        },
    ]
    .into();
    assert!(
        a.io.tutorial_identity(
            &command,
            42,
            &Deadline::new(2000, Cancellation::default()).unwrap()
        )
        .is_err()
    );
}
#[test]
fn b2_cancelled_native_spawn_does_not_execute_inert_bytes_or_probe() {
    let a = Admission::new();
    let command = a.admit().unwrap();
    let cancel = Cancellation::default();
    let deadline = Deadline::new(2000, cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        a.io.spawn_tutorial(command, &deadline),
        Err(NativeError::Cancelled)
    ));
    assert!(a.probe.facts.lock().unwrap().is_empty());
}

#[derive(Default)]
struct PipeState {
    input: Vec<u8>,
    output: VecDeque<u8>,
    eof: bool,
    reaped: bool,
    absent: bool,
    checks: usize,
    retired: bool,
    cleanup_error: Option<FixtureError>,
}
struct MemoryChild(Arc<Mutex<PipeState>>);
impl InheritedFixtureChild for MemoryChild {
    fn pid(&self) -> u32 {
        77
    }
    fn read(&mut self, byte: &mut [u8]) -> std::io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        if let Some(b) = s.output.pop_front() {
            byte[0] = b;
            Ok(1)
        } else if s.eof {
            Ok(0)
        } else {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().input.extend(bytes);
        Ok(bytes.len())
    }
    fn cleanup_confirmed(&mut self) -> std::result::Result<bool, FixtureError> {
        let mut s = self.0.lock().unwrap();
        s.checks += 1;
        if let Some(error) = s.cleanup_error {
            return Err(error);
        }
        Ok(s.reaped && s.absent)
    }
    fn retire(&mut self) {
        self.0.lock().unwrap().retired = true;
    }
}
fn message(sequence: u64, call_id: Option<u64>, event: FixtureEvent) -> Vec<u8> {
    encode_event(&FixtureEventPacket {
        schema_version: 1,
        message: FixtureMessage {
            call_id,
            attempt: AttemptId(1),
            sequence,
            result: Ok(event),
        },
    })
    .unwrap()
}
#[test]
fn b2_accepted_native_close_then_eof_before_parent_poll_reuses_cleanup_and_accept() {
    for (reaped, absent, closed, cleanup_error) in [
        (true, true, true, None),
        (true, false, false, None),
        (false, true, false, None),
        (true, true, false, Some(FixtureError::CleanupFailed)),
        (true, true, false, Some(FixtureError::TimedOut)),
    ] {
        let shared = Arc::new(Mutex::new(PipeState::default()));
        let mut port =
            PipeFixturePort::new(Box::new(MemoryChild(shared.clone())), Arc::new(|| 1)).unwrap();
        port.submit(FixtureCall {
            id: 1,
            attempt: AttemptId(1),
            command: FixtureCommand::Open {
                machine_label: "TEST_MACHINE".into(),
            },
        })
        .unwrap();
        until(|| shared.lock().unwrap().input.ends_with(b"\n"));
        let mut s = shared.lock().unwrap();
        s.output.extend(message(
            1,
            Some(1),
            FixtureEvent::Opened {
                fixture: FixtureId(1),
                pid: 77,
                window: WindowId(12),
                label: "TEST_MACHINE".into(),
            },
        ));
        s.output.extend(message(
            2,
            None,
            FixtureEvent::CloseRequested {
                fixture: FixtureId(1),
            },
        ));
        s.reaped = reaped;
        s.absent = absent;
        s.eof = true;
        s.cleanup_error = cleanup_error;
        drop(s);
        // No parent poll or complete_closed call occurs before the worker consumes EOF.
        until(|| shared.lock().unwrap().retired);
        let receipts = port.poll_receipts();
        assert!(
            receipts
                .iter()
                .any(|r| matches!(r.message.result, Ok(FixtureEvent::CloseRequested { .. })))
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|r| matches!(r.message.result, Ok(FixtureEvent::Closed { .. })))
                .count(),
            usize::from(closed)
        );
        assert_eq!(
            receipts
                .iter()
                .any(|r| r.message.result == Err(FixtureError::ChildExited)),
            !closed
        );
        assert!(shared.lock().unwrap().checks > 0);
        assert!(port.poll_receipts().is_empty());
    }
}

#[test]
fn b2_written_close_then_eof_completes_once_without_parent_confirmation_queue() {
    let shared = Arc::new(Mutex::new(PipeState::default()));
    let mut port =
        PipeFixturePort::new(Box::new(MemoryChild(shared.clone())), Arc::new(|| 1)).unwrap();
    port.submit(FixtureCall {
        id: 1,
        attempt: AttemptId(1),
        command: FixtureCommand::Open {
            machine_label: "TEST_MACHINE".into(),
        },
    })
    .unwrap();
    until(|| shared.lock().unwrap().input.ends_with(b"\n"));
    shared.lock().unwrap().output.extend(message(
        1,
        Some(1),
        FixtureEvent::Opened {
            fixture: FixtureId(1),
            pid: 77,
            window: WindowId(12),
            label: "TEST_MACHINE".into(),
        },
    ));
    let mut opened = false;
    until(|| {
        opened |= port
            .poll_receipts()
            .iter()
            .any(|r| matches!(r.message.result, Ok(FixtureEvent::Opened { .. })));
        opened
    });
    port.submit(FixtureCall {
        id: 2,
        attempt: AttemptId(1),
        command: FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    })
    .unwrap();
    until(|| {
        shared
            .lock()
            .unwrap()
            .input
            .split_inclusive(|b| *b == b'\n')
            .any(|line| {
                decode_control(line)
                    .is_ok_and(|p| matches!(p.call.command, FixtureCommand::Close { .. }))
            })
    });
    let mut state = shared.lock().unwrap();
    state.reaped = true;
    state.absent = true;
    state.eof = true;
    drop(state);
    // No complete_closed or parent poll occurs after the accepted Close write and before EOF.
    until(|| shared.lock().unwrap().retired);
    let receipts = port.poll_receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].message.call_id, Some(2));
    assert_eq!(receipts[0].message.sequence, 2);
    assert_eq!(
        receipts[0].message.result,
        Ok(FixtureEvent::Closed {
            fixture: FixtureId(1)
        })
    );
    assert!(port.poll_receipts().is_empty());
}

#[test]
fn b3b_native_invalid_tone_and_selector_refuse_before_any_audio_connection() {
    assert!(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none());
    let mut native = tutorial::native(Path::new("/nonexistent/inert-font"));
    assert_eq!(
        native.play_tone(ToneId(0), &speakers()),
        Some(Err(FixtureError::NotOwned))
    );
    let mut wrong = speakers();
    wrong.device_key.push_str("-other");
    assert_eq!(
        native.play_tone(ToneId(1), &wrong),
        Some(Err(FixtureError::NotOwned))
    );
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
}
#[test]
fn b3b_production_constructs_no_input_stream() {
    for (name, source) in [
        (
            "tutorial",
            include_str!("../src/platform/linux/tutorial.rs"),
        ),
        (
            "routing",
            include_str!("../src/platform/linux/tutorial/routing.rs"),
        ),
        (
            "tone",
            include_str!("../src/platform/linux/tutorial/tone.rs"),
        ),
    ] {
        let production = source.split("\n#[cfg(test)]").next().unwrap();
        let tokens: String = production
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect();
        assert!(
            !tokens.contains("Direction::Input"),
            "{name} constructed a product input stream"
        );
    }
}
#[test]
fn b3b_native_missing_selected_runtime_is_not_a_default_output() {
    assert!(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none());
    let mut native = tutorial::native(Path::new("/nonexistent/inert-font"));
    assert_eq!(
        native.play_tone(ToneId(1), &speakers()),
        Some(Err(FixtureError::OutputUnavailable))
    );
    assert_eq!(
        native.play_tone(ToneId(1), &speakers()),
        Some(Err(FixtureError::OutputUnavailable))
    );
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
}

#[path = "../src/platform/linux/tutorial/tone.rs"]
mod tone;
use tone::test_support as tone_test;
fn b3b_private_fd(selected: &PrivatePipewire, deadline: Instant) -> std::os::fd::OwnedFd {
    let fd = routing::socket(&selected.runtime, deadline).unwrap();
    tone_test::verify_fd(&fd).unwrap();
    fd
}
fn b3b_exact_capture_format(pod: Option<&pw::spa::pod::Pod>) -> bool {
    let Some(pod) = pod else { return false };
    let mut info = pw::spa::param::audio::AudioInfoRaw::new();
    pw::spa::param::format_utils::parse_format(pod).is_ok_and(|media| {
        media
            == (
                pw::spa::param::format::MediaType::Audio,
                pw::spa::param::format::MediaSubtype::Raw,
            )
    }) && info.parse(pod).is_ok()
        && info.format() == pw::spa::param::audio::AudioFormat::F32LE
        && info.rate() == 48000
        && info.channels() == 2
        && !info
            .flags()
            .contains(pw::spa::param::audio::AudioInfoRawFlags::UNPOSITIONED)
        && info.position()[..2]
            == [
                pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
                pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
            ]
}
fn b3b_capture_pod() -> Vec<u8> {
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: tone_test::fixed_format().into(),
        }),
    )
    .unwrap()
    .0
    .into_inner()
}
#[derive(Default)]
struct SyntheticSamples {
    frames: usize,
    nonsilent: usize,
    peak: f64,
    first: Option<Instant>,
    last: Option<Instant>,
    invalid: bool,
}
impl SyntheticSamples {
    fn add(&mut self, bytes: &[u8], now: Instant) {
        if bytes.len() > 16_384 || !bytes.len().is_multiple_of(8) || self.frames > 48000 * 6 {
            self.invalid = true;
            return;
        }
        self.frames += bytes.len() / 8;
        for frame in bytes.as_chunks::<8>().0.iter() {
            let left = f32::from_le_bytes(frame[..4].try_into().unwrap());
            let right = f32::from_le_bytes(frame[4..].try_into().unwrap());
            if !left.is_finite() || !right.is_finite() || left != right {
                self.invalid = true;
                continue;
            }
            self.peak = self.peak.max(f64::from(left.abs()));
            if left != 0.0 {
                self.nonsilent += 1;
                self.first.get_or_insert(now);
                self.last = Some(now);
            }
        }
    }
}
#[test]
fn b3b_synthetic_capture_statistics_reject_wrong_finite_shape_and_bound() {
    let mut samples = SyntheticSamples::default();
    samples.add(&[0; 16_385], Instant::now());
    assert!(samples.invalid);
    let mut samples = SyntheticSamples::default();
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&f32::NAN.to_le_bytes());
    samples.add(&bytes, Instant::now());
    assert!(samples.invalid);
    let mut samples = SyntheticSamples::default();
    bytes[..4].copy_from_slice(&0.001f32.to_le_bytes());
    bytes[4..].copy_from_slice(&0.001f32.to_le_bytes());
    samples.add(&bytes, Instant::now());
    assert_eq!(samples.nonsilent, 1);
    assert!(samples.peak < 0.0316228);
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct SyntheticPort {
    node: u32,
    serial: u64,
    output: bool,
    channel: String,
    monitor: bool,
}
type NodeReceipt = (u32, u64, bool, bool, String, String, Option<u64>);
#[derive(Default)]
struct SyntheticGraph {
    node_receipts: VecDeque<NodeReceipt>,
    nodes: BTreeMap<u32, (String, u64, bool)>,
    ports: BTreeMap<u32, SyntheticPort>,
    links: BTreeMap<u32, (u32, u32)>,
    invalid: bool,
}
impl SyntheticGraph {
    fn ports(&self, node: u32, output: bool, monitor: bool) -> Option<[u32; 2]> {
        let mut pair = [0; 2];
        for (id, port) in &self.ports {
            if port.node != node || port.output != output || port.monitor != monitor {
                continue;
            }
            let channel = match port.channel.as_str() {
                "FL" => 0,
                "FR" => 1,
                _ => panic!("unexpected synthetic channel"),
            };
            assert_eq!(pair[channel], 0, "ambiguous synthetic port");
            pair[channel] = *id;
        }
        pair.iter().all(|id| *id != 0).then_some(pair)
    }
    fn node(&self, name: &str) -> u32 {
        let nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, (n, _, confirmed))| n == name && *confirmed)
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(nodes.len(), 1, "only the exact synthetic node");
        nodes[0]
    }
}
fn b3b_private_tone_client(selected: &PrivatePipewire) {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    let proof = selected.clone();
    tone_test::install_guard(Box::new(move |fd| {
        let values = std::env::vars().collect();
        let path = proof.runtime.join("pipewire-0");
        let verify = || {
            proof
                .verify(
                    &values,
                    owned_start_ticks(proof.pid).map_err(|_| FixtureError::Refused)?,
                    &fs::symlink_metadata(&path).map_err(|_| FixtureError::Refused)?,
                )
                .map_err(|_| FixtureError::Refused)
        };
        verify()?;
        let peer = rustix::net::sockopt::socket_peercred(fd).map_err(|_| FixtureError::Refused)?;
        if peer.pid.as_raw_nonzero().get() as u32 != proof.pid {
            return Err(FixtureError::Refused);
        }
        verify()
    }));
    let deadline = Instant::now() + Duration::from_secs(2);
    let fd = b3b_private_fd(selected, deadline);
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None).unwrap();
    let context = pw::context::ContextRc::new(
        &loop_,
        Some(pw::properties::properties! {
            "config.name" => selected.root.join("client.conf").to_str().unwrap()
        }),
    )
    .unwrap();
    let core = context.connect_fd_rc(fd, None).unwrap();
    let registry = core.get_registry_rc().unwrap();
    let graph = Rc::new(RefCell::new(SyntheticGraph::default()));
    let node_watches = Rc::new(RefCell::new(
        Vec::<(pw::node::NodeListener, pw::node::Node)>::new(),
    ));
    let port_watches = Rc::new(RefCell::new(
        Vec::<(pw::port::PortListener, pw::port::Port)>::new(),
    ));
    let globals = graph.clone();
    let nodes = node_watches.clone();
    let ports = port_watches.clone();
    let bind = registry.clone();
    let removed = graph.clone();
    let exact = speakers().device_key;
    let exact_name = exact.clone();
    let _inventory = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            if g.type_ == pw::types::ObjectType::Node {
                let name = props.get("node.name").unwrap_or("");
                let class = props.get("media.class").unwrap_or("");
                let admitted = match class {
                    "Audio/Sink" => name == exact_name || name == "crosspane.private.foreign",
                    "Stream/Input/Audio" => matches!(
                        name,
                        "crosspane.private.capture.exact" | "crosspane.private.capture.foreign"
                    ),
                    "Stream/Output/Audio" => name == "crosspane.practice.tone",
                    "" => name == "crosspane.private.driver",
                    _ => false,
                };
                if !admitted {
                    globals.borrow_mut().invalid = true;
                    return;
                }
                if class.is_empty() {
                    return;
                }
                let serial = props.get("object.serial").unwrap().parse::<u64>().unwrap();
                assert!(serial > 0 && globals.borrow().nodes.len() < 8);
                globals
                    .borrow_mut()
                    .nodes
                    .insert(g.id, (name.to_owned(), serial, false));
                let bound = bind.bind::<pw::node::Node, _>(g).unwrap();
                let facts = globals.clone();
                let id = g.id;
                let name = name.to_owned();
                let class = class.to_owned();
                let exact_sink = name == exact_name;
                let listener = bound
                    .add_listener_local()
                    .info(move |info| {
                        if exact_sink {
                            let props = info.props();
                            let receipt = (
                                info.id(),
                                info.change_mask().bits(),
                                props.is_some_and(|p| p.iter().next().is_some()),
                                props
                                    .and_then(|p| p.get("node.name"))
                                    .is_some_and(|v| v == name),
                                props
                                    .and_then(|p| p.get("media.class"))
                                    .unwrap_or("absent")
                                    .chars()
                                    .take(32)
                                    .collect(),
                                props
                                    .and_then(|p| p.get("node.virtual"))
                                    .unwrap_or("absent")
                                    .chars()
                                    .take(8)
                                    .collect(),
                                props
                                    .and_then(|p| p.get("object.serial"))
                                    .and_then(|v| v.parse().ok()),
                            );
                            let mut graph = facts.borrow_mut();
                            if graph.node_receipts.len() == 8 {
                                graph.node_receipts.pop_front();
                            }
                            graph.node_receipts.push_back(receipt);
                        }
                        if !info.change_mask().contains(pw::node::NodeChangeMask::PROPS) {
                            return;
                        }
                        let valid = info.props().is_some_and(|p| {
                            p.get("node.name") == Some(name.as_str())
                                && p.get("media.class") == Some(class.as_str())
                                && p.get("node.virtual") == Some("true")
                                && p.get("object.serial").and_then(|s| s.parse::<u64>().ok())
                                    == Some(serial)
                        });
                        if !valid {
                            facts.borrow_mut().invalid = true;
                        }
                        if let Some(node) = facts.borrow_mut().nodes.get_mut(&id) {
                            node.2 = valid;
                        }
                    })
                    .register();
                nodes.borrow_mut().push((listener, bound));
            } else if g.type_ == pw::types::ObjectType::Port {
                assert!(globals.borrow().ports.len() < 24);
                let id = g.id;
                let bound = bind.bind::<pw::port::Port, _>(g).unwrap();
                let facts = globals.clone();
                let listener = bound
                    .add_listener_local()
                    .info(move |info| {
                        if !info.change_mask().contains(pw::port::PortChangeMask::PROPS) {
                            return;
                        }
                        let p = info.props().unwrap();
                        let port = SyntheticPort {
                            node: p.get("node.id").unwrap().parse().unwrap(),
                            serial: p.get("object.serial").unwrap().parse().unwrap(),
                            output: info.direction() == pw::spa::utils::Direction::Output,
                            channel: p.get("audio.channel").unwrap_or("").into(),
                            monitor: p.get("port.monitor") == Some("true"),
                        };
                        let valid = info.id() == id
                            && port.serial > 0
                            && matches!(port.channel.as_str(), "FL" | "FR")
                            && p.get("format.dsp") == Some("32 bit float mono audio")
                            && p.get("port.physical").is_none_or(|v| v == "false");
                        if !valid
                            || facts
                                .borrow()
                                .ports
                                .get(&id)
                                .is_some_and(|old| old != &port)
                        {
                            facts.borrow_mut().invalid = true;
                        }
                        facts.borrow_mut().ports.insert(id, port);
                    })
                    .register();
                ports.borrow_mut().push((listener, bound));
            } else if g.type_ == pw::types::ObjectType::Link {
                assert!(globals.borrow().links.len() < 8);
                let from = props.get("link.output.port").unwrap().parse().unwrap();
                let to = props.get("link.input.port").unwrap().parse().unwrap();
                globals.borrow_mut().links.insert(g.id, (from, to));
            }
        })
        .global_remove(move |id| {
            let mut graph = removed.borrow_mut();
            graph.links.remove(&id);
            graph.nodes.remove(&id);
            graph.ports.remove(&id);
        })
        .register();
    let done = Rc::new(Cell::new(None));
    let arrived = done.clone();
    let error = Rc::new(Cell::new(false));
    let failed = error.clone();
    let _core_events = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 {
                arrived.set(Some(seq));
            }
        })
        .error(move |_, _, _, _| failed.set(true))
        .register();
    let step = || {
        assert!(
            !error.get() && !graph.borrow().invalid,
            "private synthetic graph refused"
        );
        assert!(
            loop_
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)))
                >= 0
        );
    };
    let sync = |deadline: Instant| {
        let seq = core.sync(0).unwrap();
        while done.get() != Some(seq) {
            routing::remaining(deadline).unwrap();
            step();
        }
    };
    for _ in 0..2 {
        sync(deadline);
    }
    let sinks = [
        graph.borrow().node(&exact),
        graph.borrow().node("crosspane.private.foreign"),
    ];
    let samples: Vec<_> = (0..2)
        .map(|_| Rc::new(RefCell::new(SyntheticSamples::default())))
        .collect();
    let mut captures = Vec::new();
    for (channel, name) in [
        "crosspane.private.capture.exact",
        "crosspane.private.capture.foreign",
    ]
    .into_iter()
    .enumerate()
    {
        let stream = pw::stream::StreamBox::new(
            &core,
            name,
            pw::properties::properties! {
                "media.type" => "Audio", "media.category" => "Capture", "media.role" => "Test",
                "node.name" => name, "node.virtual" => "true", "node.dont-fallback" => "true",
                "node.dont-move" => "true", "node.dont-reconnect" => "true",
                "adapter.auto-port-config" => "{ mode=dsp monitor=false position=preserve }",
            },
        )
        .unwrap();
        let evidence = samples[channel].clone();
        let format = Rc::new(Cell::new(false));
        let changed = format.clone();
        let listener = stream
            .add_local_listener_with_user_data(())
            .param_changed(move |_, _, id, pod| {
                if id == pw::spa::sys::SPA_PARAM_Format {
                    let valid = b3b_exact_capture_format(pod);
                    changed.set(valid);
                    if !valid {
                        evidence.borrow_mut().invalid = true;
                    }
                }
            })
            .process({
                let evidence = samples[channel].clone();
                move |stream, _| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    if !format.get() || buffer.datas_mut().len() != 1 {
                        evidence.borrow_mut().invalid = true;
                        return;
                    }
                    let data = &mut buffer.datas_mut()[0];
                    let offset = data.chunk().offset() as usize;
                    let size = data.chunk().size() as usize;
                    let stride = data.chunk().stride();
                    if stride != 8 {
                        evidence.borrow_mut().invalid = true;
                        return;
                    }
                    if let Some(bytes) = data.data() {
                        if let Some(part) = offset
                            .checked_add(size)
                            .filter(|end| *end <= bytes.len())
                            .map(|end| &bytes[offset..end])
                        {
                            evidence.borrow_mut().add(part, Instant::now());
                        } else {
                            evidence.borrow_mut().invalid = true;
                        }
                    } else {
                        evidence.borrow_mut().invalid = true;
                    }
                }
            })
            .register()
            .unwrap();
        let pod = b3b_capture_pod();
        let mut params = [pw::spa::pod::Pod::from_bytes(&pod).unwrap()];
        stream
            .connect(
                pw::spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DONT_RECONNECT,
                &mut params,
            )
            .unwrap();
        captures.push((listener, stream));
    }
    for _ in 0..2 {
        sync(deadline);
    }
    let mut links = Vec::new();
    let active = Rc::new(Cell::new(0u8));
    for sink in 0..2 {
        let (from, to) = loop {
            let from = graph.borrow().ports(sinks[sink], true, true);
            let to = graph
                .borrow()
                .ports(captures[sink].1.node_id(), false, false);
            if let Some(pair) = from.zip(to) {
                break pair;
            }
            assert!(
                routing::remaining(deadline).is_ok(),
                "synthetic capture pending: sink={} capture={} from={from:?} to={to:?} ports={:?}",
                sinks[sink],
                captures[sink].1.node_id(),
                graph.borrow().ports
            );
            step();
        };
        for channel in 0..2 {
            // Only this independently verified synthetic node's monitor -> our own capture.
            let properties = pw::properties::properties! {
                "link.output.node" => sinks[sink].to_string(),
                "link.output.port" => from[channel].to_string(),
                "link.input.node" => captures[sink].1.node_id().to_string(),
                "link.input.port" => to[channel].to_string(),
                "link.passive" => "false", "object.linger" => "false",
            };
            let link: pw::link::Link = core.create_object("link-factory", &properties).unwrap();
            let arrived = active.clone();
            let channel_bit = 1 << (sink * 2 + channel);
            let expected = (
                sinks[sink],
                from[channel],
                captures[sink].1.node_id(),
                to[channel],
            );
            let listener = link
                .add_listener_local()
                .info(move |info| {
                    assert_eq!(
                        (
                            info.output_node_id(),
                            info.output_port_id(),
                            info.input_node_id(),
                            info.input_port_id()
                        ),
                        expected
                    );
                    if matches!(info.state(), pw::link::LinkState::Active) {
                        arrived.set(arrived.get() | channel_bit);
                    }
                })
                .register();
            links.push((listener, link));
        }
    }
    while active.get() != 15 {
        routing::remaining(deadline).unwrap();
        step();
    }
    sync(deadline);
    assert_eq!(graph.borrow().links.len(), 4);
    let mut output = tone::Output::new(Some(selected.runtime.clone()), false);
    for (id, natural) in [(1, true), (2, false)] {
        for evidence in &samples {
            *evidence.borrow_mut() = SyntheticSamples::default();
        }
        let opened = Instant::now();
        let mut started = false;
        while !started {
            match output.play(ToneId(id), &speakers()) {
                None => (),
                Some(result) => {
                    assert!(
                        result.is_ok(),
                        "private tone failed={result:?} phase={} format-buffer={:?} exact-node-mask receipts={:?}",
                        tone_test::phase(),
                        tone_test::diagnostic(),
                        graph.borrow().node_receipts
                    );
                    started = true;
                }
            }
            assert!(Instant::now().duration_since(opened) < Duration::from_secs(2));
            step();
        }
        assert_eq!(output.state(), OwnToneState::Running { tone: ToneId(id) });
        let running = Instant::now();
        while if natural {
            output.state() != OwnToneState::Stopped
        } else {
            samples[0].borrow().nonsilent == 0
        } {
            assert!(
                running.elapsed() < Duration::from_millis(2050),
                "bounded own tone/cleanup"
            );
            step();
        }
        if !natural {
            let stopped = Instant::now();
            while output.stop(ToneId(id)).is_none() {
                assert!(
                    stopped.elapsed() < Duration::from_millis(50),
                    "stop release bound"
                );
                step();
            }
            assert_eq!(output.stop(ToneId(id)), Some(Ok(())));
            assert!(stopped.elapsed() <= Duration::from_millis(50));
        }
        sync(Instant::now() + Duration::from_millis(50));
        let exact = samples[0].borrow();
        let foreign = samples[1].borrow();
        assert!(!exact.invalid && !foreign.invalid);
        assert!(
            exact.frames > 0 && foreign.frames > 0,
            "both captures must actually observe samples"
        );
        assert!(exact.nonsilent > 0 && exact.nonsilent <= 96000 && exact.peak <= 0.0316228);
        assert_eq!(
            foreign.nonsilent, 0,
            "foreign/default synthetic sink receives only silence"
        );
        if natural {
            assert!(
                exact.last.unwrap().duration_since(exact.first.unwrap())
                    <= Duration::from_millis(2050)
            );
        }
        assert!(
            !graph
                .borrow()
                .nodes
                .values()
                .any(|(name, _, _)| name == "crosspane.practice.tone")
        );
        assert_eq!(
            graph.borrow().links.len(),
            4,
            "all production output links removed"
        );
        println!(
            "private exact-node-mask receipts={:?}",
            graph.borrow().node_receipts
        );
        println!(
            "private synthetic sink: tone={id} natural={natural} nonsilent={} peak={:.9} foreign_nonsilent=0 own_output_removed=true",
            exact.nonsilent, exact.peak
        );
    }
    drop(output);
    drop(links);
    for (listener, stream) in captures {
        stream.set_active(false).unwrap();
        stream.flush(false).unwrap();
        stream.disconnect().unwrap();
        drop(listener);
        drop(stream);
    }
    sync(Instant::now() + Duration::from_millis(50));
    assert!(
        graph.borrow().links.is_empty(),
        "all owned monitor/capture links gone"
    );
    assert!(!graph.borrow().nodes.values().any(|(name, _, _)| {
        name.starts_with("crosspane.private.capture.") || name == "crosspane.practice.tone"
    }));
    println!(
        "private tone: exact synthetic delivery bounded; foreign/default silent; all client nodes/links removed"
    );
}

#[test]
fn b3b_private_tone_reaches_only_exact_synthetic_sink() {
    use std::os::unix::fs::FileTypeExt;
    if std::env::var_os("B3A_PRIVATE_CLIENT").is_some() {
        let root = PathBuf::from(std::env::var_os("PIPEWIRE_CONFIG_DIR").unwrap());
        let proof = root.join("server.json");
        let file = fs::File::from(
            rustix::fs::open(
                &proof,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .unwrap(),
        );
        let meta = file.metadata().unwrap();
        assert!(
            meta.is_file()
                && meta.nlink() == 1
                && meta.uid() == rustix::process::geteuid().as_raw()
                && meta.mode() & 0o7777 == 0o600
                && meta.len() <= 4096
        );
        let record: Value = serde_json::from_reader(file.take(4097)).unwrap();
        let selected = PrivatePipewire {
            root: root.clone(),
            runtime: root.join("runtime"),
            home: root.join("home"),
            pid: u32::try_from(record["pid"].as_u64().unwrap()).unwrap(),
            start: record["start"].as_u64().unwrap(),
            socket: (
                record["dev"].as_u64().unwrap(),
                record["ino"].as_u64().unwrap(),
            ),
        };
        b3b_private_tone_client(&selected);
        return;
    }
    if std::env::var("CROSSPANE_PRIVATE_PIPEWIRE").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP private PipeWire: exact CROSSPANE_PRIVATE_PIPEWIRE=1 absent; no audio coverage"
        );
        return;
    }
    if !Path::new("/usr/bin/pipewire").is_file() {
        eprintln!("SKIP private PipeWire: /usr/bin/pipewire missing; no audio coverage");
        return;
    }
    assert_eq!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        DEAD_SESSION
    );
    assert_eq!(
        std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap(),
        DEAD_SYSTEM
    );
    assert_eq!(std::env::var("PULSE_SERVER").unwrap(), DEAD_PULSE);
    for key in [
        "CROSSPANE_AUDIO",
        "CROSSPANE_NESTED_HYPR",
        "PIPEWIRE_AUTOCONNECT",
        "PIPEWIRE_CORE",
        "CROSSPANE_LIVE_TESTS",
        "CROSSPANE_REAL_STORE",
        "CROSSPANE_SECRET_SERVICE_LIVE",
    ] {
        assert!(std::env::var_os(key).is_none(), "unexpected live override");
    }
    let root = Scratch::new();
    let runtime = root.0.join("runtime");
    let home = root.0.join("home");
    for p in [&runtime, &home] {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let daemon = root.0.join("daemon.conf");
    let client = root.0.join("client.conf");
    fs::write(
        &daemon,
        private_daemon_config().replace(
            "mode=dsp monitor=false position=preserve",
            "mode=dsp monitor=true position=preserve",
        ),
    )
    .unwrap();
    fs::write(&client,"context.properties = { support.dbus=false }\ncontext.spa-libs = { support.*=support/libspa-support audio.convert.*=audioconvert/libspa-audioconvert }\ncontext.modules = [ { name=libpipewire-module-protocol-native } { name=libpipewire-module-client-node } { name=libpipewire-module-adapter } ]\n").unwrap();
    for p in [&daemon, &client] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut command = private_command(&runtime, &home);
    let log = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(root.0.join("server.log"))
        .unwrap();
    command
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    command
        .arg("env")
        .arg(format!("PIPEWIRE_RUNTIME_DIR={}", runtime.display()))
        .arg(format!("PIPEWIRE_CONFIG_DIR={}", root.0.display()))
        .arg("PIPEWIRE_CONFIG_PREFIX=")
        .arg("PIPEWIRE_CONFIG_NAME=daemon.conf")
        .args(["/usr/bin/pipewire", "-c"])
        .arg(&daemon);
    let mut server = OwnedProcess::spawn(command, root.1.clone());
    let pid = server.child().id();
    let start = owned_start_ticks(pid).unwrap().unwrap();
    println!("private owned server pid={pid} start={start}");
    let mut server = ReportedPrivateServer {
        record: server.pending.as_ref().unwrap().record.clone(),
        owned: Some(server),
        pid,
        start,
        retain: root.1.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let socket = runtime.join("pipewire-0");
    while !fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()) {
        assert!(
            server.child().try_wait().unwrap().is_none(),
            "owned private server exited"
        );
        assert!(Instant::now() < deadline, "private server socket deadline");
        thread::sleep(Duration::from_millis(2));
    }
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let meta = fs::symlink_metadata(&socket).unwrap();
    let selected = PrivatePipewire {
        root: root.0.clone(),
        runtime,
        home,
        pid,
        start,
        socket: (meta.dev(), meta.ino()),
    };
    let proof = root.0.join("server.json");
    let mut record = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&proof)
        .unwrap();
    writeln!(
        record,
        "{}",
        json!({"pid":pid,"start":start,"dev":meta.dev(),"ino":meta.ino()})
    )
    .unwrap();
    let mut command = private_command(&selected.runtime, &selected.home);
    command.arg("env");
    for (key, value) in private_values(&selected) {
        command.arg(format!("{key}={value}"));
    }
    command.arg(std::env::current_exe().unwrap()).args([
        "--exact",
        "b3b_private_tone_reaches_only_exact_synthetic_sink",
        "--nocapture",
    ]);
    selected
        .verify(
            &private_values(&selected),
            owned_start_ticks(pid).unwrap(),
            &fs::symlink_metadata(&socket).unwrap(),
        )
        .unwrap();
    let output = run_bounded(command, root.1.clone());
    assert!(output.contains("private tone: exact synthetic delivery bounded"));
    let record = server.record.clone();
    drop(server);
    assert!(
        !root.1.load(Ordering::Acquire),
        "owned private server cleanup outstanding"
    );
    assert_ne!(
        owned_start_ticks(pid).unwrap(),
        Some(start),
        "owned PID/start still exists"
    );
    assert!(!record.exists(), "owned cleanup evidence retained");
    println!("{output}");
    println!("private server pid={pid} start={start} gone; owned records reconciled");
}

#[test]
fn b3b_private_capture_pairs_wait_for_bound_facts_before_linking() {
    let mut graph = SyntheticGraph::default();
    assert_eq!(graph.ports(20, true, true), None);
    for (id, channel) in [(21, "FL"), (22, "FR")] {
        graph.ports.insert(
            id,
            SyntheticPort {
                node: 20,
                serial: id as u64,
                output: true,
                channel: channel.into(),
                monitor: true,
            },
        );
    }
    assert_eq!(graph.ports(20, true, true), Some([21, 22]));
    assert_eq!(graph.ports(20, true, false), None);
    assert_eq!(graph.ports(20, false, true), None);
}

#[test]
fn b3b_node_partial_params_and_state_receipts_preserve_full_baseline() {
    for mask in [
        pw::node::NodeChangeMask::PARAMS,
        pw::node::NodeChangeMask::STATE,
    ] {
        for props in [Some(&[][..]), None] {
            let mut model = route_model();
            establish(&mut model);
            model.node_receipt(20, 20, mask, props);
            assert!(!model.disabled());
            assert!(model.established());
        }
    }
}
#[test]
fn b3b_node_props_empty_missing_or_wrong_receipt_id_disable() {
    for (reported, mask, props) in [
        (20, pw::node::NodeChangeMask::PROPS, Some(&[][..])),
        (20, pw::node::NodeChangeMask::PROPS, None),
        (21, pw::node::NodeChangeMask::PARAMS, None),
    ] {
        let mut model = route_model();
        establish(&mut model);
        model.node_receipt(20, reported, mask, props);
        assert!(model.disabled());
        assert!(!model.established());
    }
}
#[test]
fn b3b_each_admitted_node_value_change_or_disappearance_disables() {
    let key = speakers().device_key;
    let baseline = [
        ("object.serial", "200"),
        ("node.name", key.as_str()),
        ("media.class", "Audio/Sink"),
        ("node.virtual", "true"),
    ];
    for index in 0..baseline.len() {
        for removed in [false, true] {
            let mut props = baseline.to_vec();
            if removed {
                props.remove(index);
            } else {
                props[index].1 = "SYNTHETIC_CHANGED";
            }
            let mut model = route_model();
            establish(&mut model);
            model.node_receipt(20, 20, pw::node::NodeChangeMask::PROPS, Some(&props));
            assert!(model.disabled(), "admitted value {index} removed={removed}");
            assert!(!model.established());
        }
    }
}
