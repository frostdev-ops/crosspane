//! Tests of the DisplayConfig client: the wire parsing and encoding on their own, and the client
//! against a fake Mutter on a private `dbus-daemon`.
//!
//! The fake speaks the real protocol (the interface of `org.gnome.Mutter.DisplayConfig` with the
//! argument signatures of mutter 50.4's `data/dbus-interfaces/org.gnome.Mutter.DisplayConfig.xml`,
//! the stale-serial and layout errors Mutter raises, the `MonitorsChanged` signal). Every client
//! here runs on the daemon's own address, never the session bus, and the daemon's config has no
//! service directories, so nothing can be D-Bus-activated and the owner's Shell is out of reach.
//! Tests that need the daemon skip with a printed reason when there is no `dbus-daemon` binary.
//! Whether a real Mutter accepts the layouts is a live check.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use zbus::{fdo, interface};

use super::*;

const WAIT: Duration = Duration::from_secs(10);

// ---- the private bus ---------------------------------------------------------------------------

/// A `dbus-daemon` child with a private socket, killed on drop.
struct Daemon {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl Daemon {
    fn start() -> Option<Daemon> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crosspane-fake-mutter-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("bus");
        let config = dir.join("bus.conf");
        fs::write(
            &config,
            format!(
                "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
                 \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
                 <busconfig>\n\
                 <type>session</type>\n\
                 <listen>unix:path={}</listen>\n\
                 <auth>EXTERNAL</auth>\n\
                 <policy context=\"default\">\n\
                 <allow send_destination=\"*\" eavesdrop=\"true\"/>\n\
                 <allow eavesdrop=\"true\"/>\n\
                 <allow own=\"*\"/>\n\
                 </policy>\n\
                 </busconfig>\n",
                socket.display()
            ),
        )
        .unwrap();
        let child = match Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--nofork")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skipping: cannot run dbus-daemon: {error}");
                let _ = fs::remove_dir_all(&dir);
                return None;
            }
        };
        let mut daemon = Daemon {
            child,
            address: format!("unix:path={}", socket.display()),
            dir,
        };
        let deadline = Instant::now() + WAIT;
        while !socket.exists() {
            assert!(Instant::now() < deadline, "dbus-daemon did not come up");
            if let Ok(Some(status)) = daemon.child.try_wait() {
                panic!("dbus-daemon exited early: {status}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Some(daemon)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ---- the served state --------------------------------------------------------------------------

/// A property value, as far as these tests need to tell them apart.
#[derive(Clone, Debug, PartialEq)]
enum Prop {
    Bool(bool),
    U32(u32),
    Str(String),
}

fn owned(prop: &Prop) -> OwnedValue {
    let value = match prop {
        Prop::Bool(b) => Value::Bool(*b),
        Prop::U32(u) => Value::U32(*u),
        Prop::Str(s) => Value::from(s.as_str()),
    };
    OwnedValue::try_from(value).unwrap()
}

fn prop_of(value: &OwnedValue) -> Prop {
    if let Ok(b) = value.downcast_ref::<bool>() {
        Prop::Bool(b)
    } else if let Ok(u) = value.downcast_ref::<u32>() {
        Prop::U32(u)
    } else {
        Prop::Str(value.downcast_ref::<&str>().unwrap().to_owned())
    }
}

fn props(entries: &[(&str, Prop)]) -> HashMap<String, OwnedValue> {
    entries
        .iter()
        .map(|(key, prop)| ((*key).to_owned(), owned(prop)))
        .collect()
}

#[derive(Clone)]
struct ServedMode {
    id: &'static str,
    width: i32,
    height: i32,
    current: bool,
    scales: Vec<f64>,
}

#[derive(Clone)]
struct ServedMonitor {
    connector: &'static str,
    modes: Vec<ServedMode>,
    props: Vec<(&'static str, Prop)>,
}

#[derive(Clone)]
struct ServedLogical {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: Vec<&'static str>,
}

#[derive(Clone)]
struct Served {
    serial: u32,
    monitors: Vec<ServedMonitor>,
    logical: Vec<ServedLogical>,
    properties: Vec<(&'static str, Prop)>,
}

fn mode(id: &'static str, width: i32, height: i32, current: bool, scales: &[f64]) -> ServedMode {
    ServedMode {
        id,
        width,
        height,
        current,
        scales: scales.to_vec(),
    }
}

fn logical(
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: &[&'static str],
) -> ServedLogical {
    ServedLogical {
        x,
        y,
        scale,
        transform,
        primary,
        connectors: connectors.to_vec(),
    }
}

impl Served {
    fn wire(&self) -> StateWire {
        let monitors = self
            .monitors
            .iter()
            .map(|monitor| {
                let modes = monitor
                    .modes
                    .iter()
                    .map(|mode| {
                        let mut entries = Vec::new();
                        if mode.current {
                            entries.push(("is-current", Prop::Bool(true)));
                        }
                        if mode.id.ends_with("164.999") {
                            entries.push(("is-preferred", Prop::Bool(true)));
                        }
                        (
                            mode.id.to_owned(),
                            mode.width,
                            mode.height,
                            60.0,
                            1.0,
                            mode.scales.clone(),
                            props(&entries),
                        )
                    })
                    .collect();
                (
                    (
                        monitor.connector.to_owned(),
                        "VND".to_owned(),
                        format!("Product {}", monitor.connector),
                        "0x1".to_owned(),
                    ),
                    modes,
                    props(&monitor.props),
                )
            })
            .collect();
        let logical = self
            .logical
            .iter()
            .map(|logical| {
                (
                    logical.x,
                    logical.y,
                    logical.scale,
                    logical.transform,
                    logical.primary,
                    logical
                        .connectors
                        .iter()
                        .map(|c| {
                            (
                                (*c).to_owned(),
                                "VND".to_owned(),
                                format!("Product {c}"),
                                "0x1".to_owned(),
                            )
                        })
                        .collect(),
                    HashMap::new(),
                )
            })
            .collect();
        (self.serial, monitors, logical, props(&self.properties))
    }
}

/// The live probe of 2026-10-10 right after the twin appeared: Mutter's linear layout (rotation
/// dropped) with `Meta-0` at the far right. With `extras`, the physical monitors carry HDR,
/// underscanning and RGB range settings that an apply must repeat.
fn served_with_twin(serial: u32, extras: bool) -> Served {
    let scales = [1.0, 1.25, 1.5, 2.0];
    type Props = Vec<(&'static str, Prop)>;
    let (dp3, dp2, hdmi): (Props, Props, Props) = if extras {
        (
            vec![
                ("is-builtin", Prop::Bool(false)),
                ("display-name", Prop::Str("DP-3 display".to_owned())),
                ("color-mode", Prop::U32(1)),
                ("rgb-range", Prop::U32(0)),
            ],
            vec![("is-underscanning", Prop::Bool(false))],
            vec![
                ("is-underscanning", Prop::Bool(true)),
                ("rgb-range", Prop::U32(2)),
            ],
        )
    } else {
        (vec![], vec![], vec![])
    };
    Served {
        serial,
        monitors: vec![
            ServedMonitor {
                connector: "DP-3",
                modes: vec![
                    mode("3440x1440@59.973", 3440, 1440, false, &scales),
                    mode("3440x1440@164.999", 3440, 1440, true, &scales),
                ],
                props: dp3,
            },
            ServedMonitor {
                connector: "DP-2",
                modes: vec![mode("1920x1080@74.973", 1920, 1080, true, &scales)],
                props: dp2,
            },
            ServedMonitor {
                connector: "HDMI-1",
                modes: vec![mode("1920x1080@60.000", 1920, 1080, true, &scales)],
                props: hdmi,
            },
            ServedMonitor {
                connector: "Meta-0",
                modes: vec![mode("1800x1169@60.000", 1800, 1169, true, &[1.0])],
                props: vec![],
            },
        ],
        logical: vec![
            logical(0, 0, 1.0, 0, true, &["DP-3"]),
            logical(3440, 0, 1.0, 0, false, &["DP-2"]),
            logical(5360, 0, 1.0, 0, false, &["HDMI-1"]),
            logical(7280, 0, 1.0, 0, false, &["Meta-0"]),
        ],
        properties: vec![
            ("layout-mode", Prop::U32(1)),
            ("supports-changing-layout-mode", Prop::Bool(true)),
        ],
    }
}

/// The user's layout before the twin existed (the stored snapshot).
fn user_layout() -> Vec<LogicalState> {
    let state = |x, y, transform, primary, connector: &str| LogicalState {
        x,
        y,
        scale: 1.0,
        transform,
        primary,
        connectors: vec![connector.to_owned()],
    };
    vec![
        state(1080, 1080, 0, true, "DP-3"),
        state(1817, 0, 0, false, "DP-2"),
        state(0, 600, 3, false, "HDMI-1"),
    ]
}

// ---- the fake Mutter ---------------------------------------------------------------------------

/// `(iiduba(ssa{sv}))` as the fake receives it.
type ReceivedLogical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, OwnedValue>)>,
);

/// The same, with the property maps made comparable.
type AppliedLogical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, BTreeMap<String, Prop>)>,
);

/// One `ApplyMonitorsConfig` as the fake received it.
#[derive(Debug, PartialEq)]
struct Applied {
    serial: u32,
    method: u32,
    logical: Vec<AppliedLogical>,
    properties: BTreeMap<String, Prop>,
}

struct FakeState {
    served: Served,
    applies: Vec<Applied>,
    /// Fail the next applies (after the serial check) with `InvalidArgs` and this message.
    refuse: Option<String>,
    /// Make `GetCurrentState` take this long to answer.
    stall: Option<Duration>,
}

struct Fake(Mutex<FakeState>);

impl Fake {
    fn new(served: Served) -> Arc<Fake> {
        Arc::new(Fake(Mutex::new(FakeState {
            served,
            applies: Vec::new(),
            refuse: None,
            stall: None,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }

    fn applies(&self) -> Vec<Applied> {
        std::mem::take(&mut self.lock().applies)
    }
}

struct FakeMutter(Arc<Fake>);

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl FakeMutter {
    #[zbus(out_args("serial", "monitors", "logical_monitors", "properties"))]
    async fn get_current_state(&self) -> fdo::Result<StateWire> {
        let (wire, stall) = {
            let state = self.0.lock();
            (state.served.wire(), state.stall)
        };
        if let Some(stall) = stall {
            // Blocks the fake's executor, which is all a stalled Mutter needs to look like.
            std::thread::sleep(stall);
        }
        Ok(wire)
    }

    async fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<ReceivedLogical>,
        properties: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        let mut state = self.0.lock();
        // Mutter's own order of checks (`meta_monitor_manager_handle_apply_monitors_config`).
        if serial != state.served.serial {
            return Err(fdo::Error::AccessDenied(
                "The requested configuration is based on stale information".to_owned(),
            ));
        }
        if let Some(message) = state.refuse.clone() {
            return Err(fdo::Error::InvalidArgs(message));
        }
        let applied = Applied {
            serial,
            method,
            logical: logical_monitors
                .into_iter()
                .map(|(x, y, scale, transform, primary, monitors)| {
                    (
                        x,
                        y,
                        scale,
                        transform,
                        primary,
                        monitors
                            .into_iter()
                            .map(|(connector, mode, props)| (connector, mode, prop_map(&props)))
                            .collect(),
                    )
                })
                .collect(),
            properties: prop_map(&properties),
        };
        state.applies.push(applied);
        Ok(())
    }
}

/// A fake whose `GetCurrentState` answers with the wrong shape.
struct BrokenMutter;

#[interface(name = "org.gnome.Mutter.DisplayConfig")]
impl BrokenMutter {
    async fn get_current_state(&self) -> u32 {
        7
    }
}

fn prop_map(props: &HashMap<String, OwnedValue>) -> BTreeMap<String, Prop> {
    props
        .iter()
        .map(|(key, value)| (key.clone(), prop_of(value)))
        .collect()
}

/// The service on the daemon's bus, under Mutter's name, until dropped.
fn serve<I: zbus::object_server::Interface>(daemon: &Daemon, iface: I) -> zbus::Connection {
    zbus::block_on(async {
        zbus::connection::Builder::address(daemon.address.as_str())?
            .serve_at(OBJECT_PATH, iface)?
            .name(BUS_NAME)?
            .build()
            .await
    })
    .unwrap()
}

/// A daemon, a fake Mutter serving `served` on it, and a client connected to it.
struct Rig {
    // Dropped in this order: client, service, bus.
    client: DisplayConfig,
    connection: zbus::Connection,
    fake: Arc<Fake>,
    _daemon: Daemon,
}

impl Rig {
    fn start(served: Served) -> Option<Rig> {
        let daemon = Daemon::start()?;
        let fake = Fake::new(served);
        let connection = serve(&daemon, FakeMutter(fake.clone()));
        let client = DisplayConfig::open(Some(daemon.address.clone())).unwrap();
        Some(Rig {
            client,
            connection,
            fake,
            _daemon: daemon,
        })
    }

    fn emit_monitors_changed(&self) {
        zbus::block_on(self.connection.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            MONITORS_CHANGED,
            &(),
        ))
        .unwrap();
    }
}

// ---- wire parsing and encoding (no bus) --------------------------------------------------------

fn expected_state() -> DisplayState {
    let mode = |id: &str, width, height, current, preferred, scales: &[f64]| ModeState {
        id: id.to_owned(),
        width,
        height,
        refresh: 60.0,
        preferred_scale: 1.0,
        supported_scales: scales.to_vec(),
        current,
        preferred,
    };
    let scales = [1.0, 1.25, 1.5, 2.0];
    let monitor = |connector: &str, modes| MonitorState {
        connector: connector.to_owned(),
        vendor: "VND".to_owned(),
        product: format!("Product {connector}"),
        serial: "0x1".to_owned(),
        modes,
        is_builtin: false,
    };
    let logical = |x, y, transform, primary, connector: &str| LogicalState {
        x,
        y,
        scale: 1.0,
        transform,
        primary,
        connectors: vec![connector.to_owned()],
    };
    DisplayState {
        serial: 7,
        monitors: vec![
            monitor(
                "DP-3",
                vec![
                    mode("3440x1440@59.973", 3440, 1440, false, false, &scales),
                    mode("3440x1440@164.999", 3440, 1440, true, true, &scales),
                ],
            ),
            monitor(
                "DP-2",
                vec![mode("1920x1080@74.973", 1920, 1080, true, false, &scales)],
            ),
            monitor(
                "HDMI-1",
                vec![mode("1920x1080@60.000", 1920, 1080, true, false, &scales)],
            ),
            monitor(
                "Meta-0",
                vec![mode("1800x1169@60.000", 1800, 1169, true, false, &[1.0])],
            ),
        ],
        logical: vec![
            logical(0, 0, 0, true, "DP-3"),
            logical(3440, 0, 0, false, "DP-2"),
            logical(5360, 0, 0, false, "HDMI-1"),
            logical(7280, 0, 0, false, "Meta-0"),
        ],
        layout_mode: Some(1),
    }
}

#[test]
fn parse_state_reads_every_field() {
    let (state, carry) = parse_state(served_with_twin(7, false).wire()).unwrap();
    assert_eq!(state, expected_state());
    assert_eq!(carry.layout_mode, Some(1));
    assert!(
        carry
            .extras
            .values()
            .all(|e| *e == MonitorExtras::default())
    );
    assert_eq!(carry.extras.len(), 4);

    // The built-in flag, an absent layout mode, and properties that are not read.
    let mut served = served_with_twin(8, true);
    served.monitors[0]
        .props
        .push(("is-builtin", Prop::Bool(true)));
    served.properties.clear();
    let (state, carry) = parse_state(served.wire()).unwrap();
    assert!(state.monitors[0].is_builtin);
    assert!(!state.monitors[1].is_builtin);
    assert_eq!(state.layout_mode, None);
    assert_eq!(carry.layout_mode, None);
    assert_eq!(
        carry.extras["DP-3"],
        MonitorExtras {
            underscanning: None,
            color_mode: 1,
            rgb_range: 0
        }
    );
    assert_eq!(
        carry.extras["DP-2"],
        MonitorExtras {
            underscanning: Some(false),
            ..MonitorExtras::default()
        }
    );
    assert_eq!(
        carry.extras["HDMI-1"],
        MonitorExtras {
            underscanning: Some(true),
            color_mode: 0,
            rgb_range: 2
        }
    );
}

#[test]
fn parse_state_rejects_malformed_replies() {
    type Break = Box<dyn Fn(&mut Served)>;
    let cases: Vec<(&str, Break)> = vec![
        ("zero width", Box::new(|s| s.monitors[0].modes[0].width = 0)),
        (
            "negative height",
            Box::new(|s| s.monitors[0].modes[0].height = -1),
        ),
        (
            "zero supported scale",
            Box::new(|s| s.monitors[0].modes[0].scales = vec![1.0, 0.0]),
        ),
        (
            "NaN supported scale",
            Box::new(|s| s.monitors[0].modes[0].scales = vec![f64::NAN]),
        ),
        ("zero scale", Box::new(|s| s.logical[0].scale = 0.0)),
        ("NaN scale", Box::new(|s| s.logical[0].scale = f64::NAN)),
        ("negative scale", Box::new(|s| s.logical[0].scale = -2.0)),
        ("transform 8", Box::new(|s| s.logical[0].transform = 8)),
        (
            "logical monitor without monitors",
            Box::new(|s| s.logical[0].connectors.clear()),
        ),
        (
            "layout mode 3",
            Box::new(|s| s.properties = vec![("layout-mode", Prop::U32(3))]),
        ),
        (
            "layout mode 0",
            Box::new(|s| s.properties = vec![("layout-mode", Prop::U32(0))]),
        ),
        (
            "layout mode of the wrong type",
            Box::new(|s| s.properties = vec![("layout-mode", Prop::Str("logical".to_owned()))]),
        ),
        (
            "repeated connector",
            Box::new(|s| s.monitors[1].connector = "DP-3"),
        ),
        (
            "empty connector",
            Box::new(|s| s.monitors[1].connector = ""),
        ),
        (
            "color mode of the wrong type",
            Box::new(|s| {
                s.monitors[0]
                    .props
                    .push(("color-mode", Prop::Str("hdr".to_owned())))
            }),
        ),
        (
            "is-builtin of the wrong type",
            Box::new(|s| s.monitors[0].props.push(("is-builtin", Prop::U32(1)))),
        ),
    ];
    for (name, break_it) in cases {
        let mut served = served_with_twin(7, false);
        break_it(&mut served);
        match parse_state(served.wire()) {
            Err(PlatformError::Backend(message)) => {
                assert!(
                    message.starts_with("DisplayConfig: malformed reply"),
                    "{name}: {message}"
                );
            }
            other => panic!("{name}: expected a Backend error, got {other:?}"),
        }
    }
    // A present but wrongly typed mode flag.
    let mut wire = served_with_twin(7, false).wire();
    wire.1[0].1[0]
        .6
        .insert("is-current".to_owned(), owned(&Prop::U32(1)));
    assert!(matches!(parse_state(wire), Err(PlatformError::Backend(_))));
}

fn plan_for_twin() -> Vec<LogicalConfig> {
    let (state, _) = parse_state(served_with_twin(7, false).wire()).unwrap();
    plan(&state, &user_layout(), "Meta-0", 1.0).unwrap()
}

#[test]
fn the_apply_body_is_method_one_with_the_exact_structure() {
    let plan = plan_for_twin();
    let body = apply_body(7, &plan, &Carry::default());
    assert_eq!(body.0, 7);
    assert_eq!(body.1, 1, "method must be 1 (temporary)");
    assert_eq!(METHOD_TEMPORARY, 1);
    assert!(body.3.is_empty());
    let shape: Vec<_> = body
        .2
        .iter()
        .map(|(x, y, scale, transform, primary, monitors)| {
            (
                *x,
                *y,
                *scale,
                *transform,
                *primary,
                monitors
                    .iter()
                    .map(|(c, m, p)| (c.to_string(), m.to_string(), p.len()))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                1080,
                1080,
                1.0,
                0,
                true,
                vec![("DP-3".to_owned(), "3440x1440@164.999".to_owned(), 0)]
            ),
            (
                1817,
                0,
                1.0,
                0,
                false,
                vec![("DP-2".to_owned(), "1920x1080@74.973".to_owned(), 0)]
            ),
            (
                0,
                600,
                1.0,
                3,
                false,
                vec![("HDMI-1".to_owned(), "1920x1080@60.000".to_owned(), 0)]
            ),
            (
                4520,
                1080,
                1.0,
                0,
                false,
                vec![("Meta-0".to_owned(), "1800x1169@60.000".to_owned(), 0)]
            ),
        ]
    );
    // The signature on the wire is what Mutter's XML says.
    use zbus::zvariant::DynamicType;
    assert_eq!(body.signature().to_string(), "(uua(iiduba(ssa{sv}))a{sv})");
}

#[test]
fn the_apply_body_repeats_what_the_state_showed() {
    let (_, carry) = parse_state(served_with_twin(7, true).wire()).unwrap();
    let plan = plan_for_twin();
    let body = apply_body(7, &plan, &carry);
    assert!(body.3.is_empty(), "logical layout mode needs no property");
    let props_of = |index: usize| -> BTreeMap<&str, String> {
        body.2[index].5[0]
            .2
            .iter()
            .map(|(key, value)| (*key, format!("{value:?}")))
            .collect()
    };
    assert_eq!(
        props_of(0),
        BTreeMap::from([("color-mode", "U32(1)".to_owned())])
    );
    // `underscanning = false` and `rgb-range = unknown` are Mutter's defaults: not sent.
    assert!(props_of(1).is_empty());
    assert_eq!(
        props_of(2),
        BTreeMap::from([
            ("rgb-range", "U32(2)".to_owned()),
            ("underscanning", "Bool(true)".to_owned())
        ])
    );
    // The twin was not in the carried state's physical set of known extras.
    assert!(props_of(3).is_empty());

    // The physical layout mode is the one non-default property, repeated.
    let mut served = served_with_twin(7, false);
    served.properties = vec![("layout-mode", Prop::U32(2))];
    let (_, carry) = parse_state(served.wire()).unwrap();
    let body = apply_body(7, &plan, &carry);
    assert_eq!(body.3.len(), 1);
    assert_eq!(format!("{:?}", body.3["layout-mode"]), "U32(2)");
}

#[test]
fn only_the_two_documented_calls_exist() {
    assert_eq!(Call::GetCurrentState.member(), "GetCurrentState");
    assert_eq!(Call::ApplyMonitorsConfig.member(), "ApplyMonitorsConfig");
    assert_eq!(BUS_NAME, "org.gnome.Mutter.DisplayConfig");
    assert_eq!(INTERFACE, "org.gnome.Mutter.DisplayConfig");
    assert_eq!(OBJECT_PATH, "/org/gnome/Mutter/DisplayConfig");
    // No other Mutter method name, and no way to name another method value, in this module.
    let source = include_str!("../display_config.rs");
    let code = source.replace("#[cfg(test)]\nmod tests;", "");
    for forbidden in [
        "SetOutputCTM",
        "ChangeBacklight",
        "SetBacklight",
        "GetResources",
        "ApplyConfiguration",
        "SetCrtcGamma",
        "GetCrtcGamma",
        "METHOD_PERSISTENT",
        "METHOD_VERIFY",
        "org.gnome.Mutter.ScreenCast",
        "org.gnome.Mutter.RemoteDesktop",
    ] {
        assert!(!code.contains(forbidden), "{forbidden}");
    }
    // The one place an `ApplyMonitorsConfig` body is built takes its method from the constant.
    assert_eq!(
        code.matches("(serial, METHOD_TEMPORARY, logical, properties)")
            .count(),
        1
    );
    assert_eq!(code.matches("const METHOD_TEMPORARY: u32 = 1;").count(), 1);
}

#[test]
fn an_invalid_layout_is_refused_before_anything_is_sent() {
    assert!(check_config(&plan_for_twin()).is_ok());
    assert!(check_config(&[]).is_err());
    let mut bad = plan_for_twin();
    bad[0].monitors.clear();
    assert!(check_config(&bad).is_err());
    let mut bad = plan_for_twin();
    bad[1].scale = 0.0;
    assert!(check_config(&bad).is_err());
    let mut bad = plan_for_twin();
    bad[2].scale = f64::NAN;
    assert!(check_config(&bad).is_err());
    let mut bad = plan_for_twin();
    bad[3].transform = 8;
    assert!(check_config(&bad).is_err());
}

/// A D-Bus error reply of the given name and message, converted by zbus as for a real call.
fn reply(name: &str, detail: &str) -> zbus::Error {
    let call = Message::method_call(OBJECT_PATH, "ApplyMonitorsConfig")
        .unwrap()
        .build(&())
        .unwrap();
    zbus::Error::from(
        Message::error(&call.header(), name)
            .unwrap()
            .build(&detail)
            .unwrap(),
    )
}

#[test]
fn dbus_errors_map_to_platform_errors() {
    for name in [
        "org.freedesktop.DBus.Error.NoReply",
        "org.freedesktop.DBus.Error.Timeout",
        "org.freedesktop.DBus.Error.TimedOut",
    ] {
        assert!(matches!(
            dbus_error(reply(name, "slow")),
            PlatformError::Timeout
        ));
    }
    assert!(matches!(
        dbus_error(zbus::Error::InputOutput(Arc::new(
            std::io::ErrorKind::TimedOut.into()
        ))),
        PlatformError::Timeout
    ));
    assert_eq!(
        dbus_error(reply(NAME_HAS_NO_OWNER, "gone")).to_string(),
        "DisplayConfig: Mutter is not running"
    );
    assert_eq!(
        dbus_error(reply(
            "org.freedesktop.DBus.Error.InvalidArgs",
            "Logical monitors not adjacent"
        ))
        .to_string(),
        "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
    );
    // The same through zbus's own fdo error type.
    assert_eq!(
        dbus_error(zbus::Error::FDO(Box::new(fdo::Error::AccessDenied(
            "no".to_owned()
        ))))
        .to_string(),
        "DisplayConfig: org.freedesktop.DBus.Error.AccessDenied: no"
    );
    // Long messages are cut.
    let long = "x".repeat(500);
    let message = dbus_error(reply("org.freedesktop.DBus.Error.Failed", &long)).to_string();
    assert!(message.chars().count() < 300, "{}", message.len());
}

#[test]
fn only_a_stale_serial_is_a_serial_race() {
    let race = dbus_error(reply(
        "org.freedesktop.DBus.Error.AccessDenied",
        "The requested configuration is based on stale information",
    ));
    assert!(is_serial_race(&race));
    let race = dbus_error(reply(
        "org.freedesktop.DBus.Error.InvalidArgs",
        "wrong serial",
    ));
    assert!(is_serial_race(&race));
    for other in [
        dbus_error(reply(
            "org.freedesktop.DBus.Error.InvalidArgs",
            "Logical monitors not adjacent",
        )),
        dbus_error(reply(
            "org.freedesktop.DBus.Error.AccessDenied",
            "Monitor configuration via D-Bus is disabled",
        )),
        dbus_error(reply(
            "org.freedesktop.DBus.Error.Failed",
            "stale information",
        )),
        PlatformError::Timeout,
        PlatformError::NotFound,
        PlatformError::Backend("stale information".to_owned()),
    ] {
        assert!(!is_serial_race(&other), "{other}");
    }
}

#[test]
fn monitors_changed_is_recognised_by_path_interface_and_member() {
    let signal = |path: &str, interface: &str, member: &str| {
        Message::signal(path, interface, member)
            .unwrap()
            .sender(":1.5")
            .unwrap()
            .build(&())
            .unwrap()
    };
    assert!(is_monitors_changed(&signal(
        OBJECT_PATH,
        INTERFACE,
        "MonitorsChanged"
    )));
    assert!(!is_monitors_changed(&signal(
        OBJECT_PATH,
        INTERFACE,
        "PowerSaveModeChanged"
    )));
    assert!(!is_monitors_changed(&signal(
        "/org/gnome/Mutter/Other",
        INTERFACE,
        "MonitorsChanged"
    )));
    assert!(!is_monitors_changed(&signal(
        OBJECT_PATH,
        "org.gnome.Mutter.Other",
        "MonitorsChanged"
    )));
    let call = Message::method_call(OBJECT_PATH, "MonitorsChanged")
        .unwrap()
        .interface(INTERFACE)
        .unwrap()
        .build(&())
        .unwrap();
    assert!(!is_monitors_changed(&call));
}

// ---- the client against the fake ---------------------------------------------------------------

#[test]
fn connect_without_the_service_is_unsupported() {
    let Some(daemon) = Daemon::start() else {
        return;
    };
    match DisplayConfig::open(Some(daemon.address.clone())) {
        Err(PlatformError::Unsupported(_)) => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[test]
fn connect_to_a_dead_bus_is_an_error_not_a_hang() {
    let started = Instant::now();
    let result = DisplayConfig::open(Some("unix:path=/nonexistent/crosspane-test-bus".to_owned()));
    assert!(
        matches!(result, Err(PlatformError::Backend(_))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn current_state_reads_the_served_state() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    assert_eq!(rig.client.current_state().unwrap(), expected_state());
    // A second read sees what the service says now.
    rig.fake.lock().served.serial = 8;
    assert_eq!(rig.client.current_state().unwrap().serial, 8);
}

#[test]
fn a_reply_of_the_wrong_shape_is_backend() {
    let Some(daemon) = Daemon::start() else {
        return;
    };
    let _service = serve(&daemon, BrokenMutter);
    let client = DisplayConfig::open(Some(daemon.address.clone())).unwrap();
    match client.current_state() {
        Err(PlatformError::Backend(message)) => {
            assert!(message.contains("malformed reply"), "{message}");
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

#[test]
fn apply_sends_method_one_and_the_exact_structure() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    let state = rig.client.current_state().unwrap();
    let plan = plan(&state, &user_layout(), "Meta-0", 1.0).unwrap();
    rig.client.apply_temporary(state.serial, &plan).unwrap();
    let monitor = |connector: &str, mode: &str| {
        vec![(connector.to_owned(), mode.to_owned(), BTreeMap::new())]
    };
    assert_eq!(
        rig.fake.applies(),
        vec![Applied {
            serial: 7,
            method: 1,
            logical: vec![
                (
                    1080,
                    1080,
                    1.0,
                    0,
                    true,
                    monitor("DP-3", "3440x1440@164.999")
                ),
                (1817, 0, 1.0, 0, false, monitor("DP-2", "1920x1080@74.973")),
                (0, 600, 1.0, 3, false, monitor("HDMI-1", "1920x1080@60.000")),
                (
                    4520,
                    1080,
                    1.0,
                    0,
                    false,
                    monitor("Meta-0", "1800x1169@60.000")
                ),
            ],
            properties: BTreeMap::new(),
        }]
    );
    assert_eq!(
        twin_rect(&plan, &state, "Meta-0"),
        Some((4520, 1080, 1800, 1169))
    );
}

#[test]
fn apply_repeats_hdr_underscanning_and_physical_layout_mode() {
    let mut served = served_with_twin(7, true);
    served.properties = vec![("layout-mode", Prop::U32(2))];
    let Some(rig) = Rig::start(served) else {
        return;
    };
    let state = rig.client.current_state().unwrap();
    assert_eq!(state.layout_mode, Some(2));
    // Physical layout mode: no scale division, so the twin's rect is its mode size.
    let plan = plan(&state, &user_layout(), "Meta-0", 1.0).unwrap();
    rig.client.apply_temporary(state.serial, &plan).unwrap();
    let applied = rig.fake.applies();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].method, 1);
    assert_eq!(
        applied[0].properties,
        BTreeMap::from([("layout-mode".to_owned(), Prop::U32(2))])
    );
    let monitor_props = |index: usize| applied[0].logical[index].5[0].2.clone();
    assert_eq!(
        monitor_props(0),
        BTreeMap::from([("color-mode".to_owned(), Prop::U32(1))])
    );
    assert!(monitor_props(1).is_empty());
    assert_eq!(
        monitor_props(2),
        BTreeMap::from([
            ("rgb-range".to_owned(), Prop::U32(2)),
            ("underscanning".to_owned(), Prop::Bool(true))
        ])
    );
    assert!(monitor_props(3).is_empty());
}

#[test]
fn a_stale_serial_is_a_serial_race_and_a_refusal_carries_mutters_message() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    let state = rig.client.current_state().unwrap();
    let plan = plan(&state, &user_layout(), "Meta-0", 1.0).unwrap();

    // The configuration moved on between the read and the apply.
    rig.fake.lock().served.serial = 8;
    let error = rig.client.apply_temporary(state.serial, &plan).unwrap_err();
    assert_eq!(
        error.to_string(),
        "DisplayConfig: org.freedesktop.DBus.Error.AccessDenied: \
         The requested configuration is based on stale information"
    );
    assert!(is_serial_race(&error));
    assert!(rig.fake.applies().is_empty());

    // Re-read and retry, as the caller does; Mutter now refuses the layout itself.
    let state = rig.client.current_state().unwrap();
    rig.fake.lock().refuse = Some("Logical monitors not adjacent".to_owned());
    let error = rig.client.apply_temporary(state.serial, &plan).unwrap_err();
    assert_eq!(
        error.to_string(),
        "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
    );
    assert!(!is_serial_race(&error));
    assert!(rig.fake.applies().is_empty());

    // And once Mutter accepts it, the retry goes through with the fresh serial.
    rig.fake.lock().refuse = None;
    rig.client.apply_temporary(state.serial, &plan).unwrap();
    let applied = rig.fake.applies();
    assert_eq!((applied[0].serial, applied[0].method), (8, 1));
}

#[test]
fn an_invalid_layout_never_reaches_the_bus() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    assert!(rig.client.apply_temporary(7, &[]).is_err());
    let mut plan = plan_for_twin();
    plan[0].monitors.clear();
    assert!(rig.client.apply_temporary(7, &plan).is_err());
    assert!(rig.fake.applies().is_empty());
}

#[test]
fn calls_after_the_service_left_fail_instead_of_hanging() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    assert!(rig.client.current_state().is_ok());
    assert!(zbus::block_on(rig.connection.release_name(BUS_NAME)).unwrap());
    let started = Instant::now();
    match rig.client.current_state() {
        Err(PlatformError::Backend(message)) => {
            assert_eq!(message, "DisplayConfig: Mutter is not running");
        }
        other => panic!("expected Backend, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_stalled_mutter_is_a_timeout_after_two_seconds() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    rig.fake.lock().stall = Some(Duration::from_millis(2600));
    let started = Instant::now();
    assert!(matches!(
        rig.client.current_state(),
        Err(PlatformError::Timeout)
    ));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(1900) && waited < Duration::from_millis(2550),
        "{waited:?}"
    );
}

#[test]
fn the_client_can_be_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DisplayConfig>();
}

#[test]
fn subscribe_delivers_monitors_changed_and_stops_with_the_client() {
    let Some(rig) = Rig::start(served_with_twin(7, false)) else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    let first = tx.clone();
    rig.client
        .subscribe(Arc::new(move || {
            let _ = first.send("first");
        }))
        .unwrap();
    rig.client
        .subscribe(Arc::new(move || {
            let _ = tx.send("second");
        }))
        .unwrap();
    // The match rule is installed when subscribe returns: no signal after it is lost. Mutter
    // emits it once per change, here twice in a row.
    rig.emit_monitors_changed();
    rig.emit_monitors_changed();
    let mut seen = Vec::new();
    while seen.len() < 4 {
        seen.push(rx.recv_timeout(Duration::from_secs(5)).unwrap());
    }
    assert_eq!(seen, ["first", "second", "first", "second"]);

    // Other signals on the bus do not count.
    zbus::block_on(rig.connection.emit_signal(
        None::<&str>,
        OBJECT_PATH,
        INTERFACE,
        "SomethingElse",
        &(),
    ))
    .unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());

    // Dropping the client ends its signal thread (the drop joins it) without a further callback.
    let Rig {
        client, connection, ..
    } = rig;
    let started = Instant::now();
    drop(client);
    assert!(started.elapsed() < Duration::from_secs(2));
    zbus::block_on(connection.emit_signal(
        None::<&str>,
        OBJECT_PATH,
        INTERFACE,
        MONITORS_CHANGED,
        &(),
    ))
    .unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
}

#[test]
fn a_callback_may_hold_the_last_reference_to_the_client() {
    let Some(daemon) = Daemon::start() else {
        return;
    };
    let fake = Fake::new(served_with_twin(7, false));
    let service = serve(&daemon, FakeMutter(fake));
    let client = Arc::new(DisplayConfig::open(Some(daemon.address.clone())).unwrap());
    let (tx, rx) = mpsc::channel();
    let slot = Arc::new(Mutex::new(Some(client.clone())));
    {
        let slot = slot.clone();
        client
            .subscribe(Arc::new(move || {
                // Drops the last reference to the client on its own signal thread.
                let taken = slot.lock().unwrap().take();
                drop(taken);
                let _ = tx.send(());
            }))
            .unwrap();
    }
    drop(client);
    zbus::block_on(service.emit_signal(
        None::<&str>,
        OBJECT_PATH,
        INTERFACE,
        MONITORS_CHANGED,
        &(),
    ))
    .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
}
