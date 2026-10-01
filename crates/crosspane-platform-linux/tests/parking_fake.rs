//! Twin-output parking against a scripted fake Hyprland socket (WP-2.35). No compositor: a thread
//! serves `.socket.sock` from a temporary directory and plays a small model of one window and one
//! twin output. The tests drive `park`/`resize`/`geometry` through the public `WindowParking`
//! trait and assert what they return.
//!
//! **What the model reports.** Like `hyprctl clients`, the fake reports the window's *layout goal*
//! (Hyprland's `GEOMETRIC_GOAL`, HyprCtl.cpp at v0.56.1): the geometry the compositor has laid
//! out for the window, not a committed buffer. Its client behaviours (`Client`) are therefore
//! "how the layout goal changes after the output's mode does": it follows the work area at once or
//! some `clients` polls later (the ordering Hyprland can produce on a shrink), is clamped to a
//! minimum, picks a size of its own, or never changes. The mode itself changes at once, or after
//! some `monitors` queries (`set_mode_lag`). Bars are modelled as reserved areas that tile the
//! window inside the work area, and can be *reported* to `monitors` later than they take effect.
//!
//! The fake logs when it served what, so the tests assert on its own observations (poll counts and
//! timestamps) rather than on wall-clock bounds around the call; the exact timing rules are
//! covered by the deterministic decision tests in `parking.rs`. A bound here is a lower bound
//! only, so a loaded machine can only make a test slower, never fail it.
//!
//! Always runs; it needs no session. `parking_live.rs` is the lead's live counterpart.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_platform_linux::hyprland::parking::HyprlandParking;
use crosspane_types::geom::PixelSize;
use crosspane_types::geom::euclid::point2;
use crosspane_types::id::WindowId;
use serde_json::{Value, json};

const WINDOW: WindowId = WindowId(1);
const ADDRESS: &str = "0xabc";

type Size = (i64, i64);

/// How the fake client's layout goal answers a new work area on the twin output.
#[derive(Clone, Copy, Debug)]
enum Client {
    /// Takes the work area's size after `lag` polls of `clients` that still show the old size.
    Follows { lag: u32 },
    /// Like `Follows`, but never below `min` (a minimum size).
    AtLeast { min: Size, lag: u32 },
    /// Takes `size` (logical pixels) after `lag` stale polls, whatever the work area is.
    Chooses { size: Size, lag: u32 },
    /// Never changes its size.
    Fixed,
}

#[derive(Clone, Debug)]
struct Monitor {
    name: String,
    id: u32,
    x: i64,
    width: i64,
    height: i64,
    scale: f64,
}

/// A mode change on its way to being applied.
#[derive(Clone, Debug)]
struct ModeChange {
    name: String,
    x: i64,
    width: i64,
    height: i64,
    scale: f64,
}

#[derive(Debug)]
struct World {
    client: Client,
    monitors: Vec<Monitor>,
    workspace: String,
    /// The window's layout goal, in logical pixels.
    at: Size,
    size: Size,
    /// A size the client will take after this many more `clients` polls.
    pending: Option<(Size, u32)>,
    /// The area bars take on the twin (left, top, right, bottom; logical pixels), as laid out.
    bar: [i64; 4],
    /// The area `monitors` reports as reserved: `bar`, once `report_in` polls have passed.
    reported: [i64; 4],
    report_in: Option<u32>,
    /// `monitors` queries before a mode change shows (and the window is laid out on it).
    mode_lag: u32,
    pending_mode: Option<(ModeChange, u32)>,
    /// What the fake served, and when: every `clients` poll (the size it showed), every mode change
    /// (when it was asked for, and the mode's dimensions in device pixels).
    served: Vec<(Instant, Size)>,
    mode_changes: Vec<(Instant, Size)>,
}

impl World {
    fn new() -> World {
        World {
            client: Client::Follows { lag: 0 },
            monitors: vec![Monitor {
                name: "DP-1".into(),
                id: 0,
                x: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
            }],
            workspace: "1".into(),
            at: (100, 100),
            size: (1600, 1200),
            pending: None,
            bar: [0; 4],
            reported: [0; 4],
            report_in: None,
            mode_lag: 0,
            pending_mode: None,
            served: Vec::new(),
            mode_changes: Vec::new(),
        }
    }

    fn twin(&self) -> Option<&Monitor> {
        self.monitors
            .iter()
            .find(|m| m.name.starts_with("CROSSPANE-"))
    }

    fn on_twin(&self) -> bool {
        self.workspace.starts_with("crosspane-")
    }

    /// The compositor lays the window out in the twin's work area (the mode less the bars); how
    /// the client's goal answers is up to its behaviour.
    fn retile(&mut self) {
        let Some(twin) = self.twin() else { return };
        let bar = self.bar;
        let tile = (
            (twin.width as f64 / twin.scale) as i64 - bar[0] - bar[2],
            (twin.height as f64 / twin.scale) as i64 - bar[1] - bar[3],
        );
        let at = (twin.x + bar[0], bar[1]);
        self.at = at;
        self.pending = match self.client {
            Client::Follows { lag } => Some((tile, lag)),
            Client::AtLeast { min, lag } => Some(((tile.0.max(min.0), tile.1.max(min.1)), lag)),
            Client::Chooses { size, lag } => Some((size, lag)),
            Client::Fixed => None,
        };
    }

    fn apply(&mut self, change: ModeChange) {
        let m = self
            .monitors
            .iter_mut()
            .find(|m| m.name == change.name)
            .expect("monitor for a mode change");
        (m.x, m.width, m.height, m.scale) = (change.x, change.width, change.height, change.scale);
        if self.on_twin() {
            self.retile();
        }
    }

    fn monitor_json(&self, m: &Monitor) -> Value {
        let reserved = if m.name.starts_with("CROSSPANE-") {
            self.reported
        } else {
            [0; 4]
        };
        json!({
            "id": m.id, "name": m.name, "x": m.x, "y": 0,
            "width": m.width, "height": m.height, "scale": m.scale,
            "reserved": reserved,
        })
    }

    fn handle(&mut self, request: &str) -> String {
        if request == "j/monitors" {
            if let Some((change, queries)) = self.pending_mode.take() {
                if queries == 0 {
                    self.apply(change);
                } else {
                    self.pending_mode = Some((change, queries - 1));
                }
            }
            let list: Vec<Value> = self.monitors.iter().map(|m| self.monitor_json(m)).collect();
            return Value::Array(list).to_string();
        }
        if request == "j/clients" {
            if let Some(polls) = self.report_in {
                if polls == 0 {
                    self.reported = self.bar;
                    self.report_in = None;
                } else {
                    self.report_in = Some(polls - 1);
                }
            }
            if let Some((size, lag)) = self.pending {
                if lag == 0 {
                    self.size = size;
                    self.pending = None;
                } else {
                    self.pending = Some((size, lag - 1));
                }
            }
            self.served.push((Instant::now(), self.size));
            return json!([{
                "address": ADDRESS, "stableId": "1", "class": "fake",
                "workspace": {"id": 3, "name": self.workspace},
                "floating": false, "fullscreen": 0,
                "at": [self.at.0, self.at.1], "size": [self.size.0, self.size.1],
            }])
            .to_string();
        }
        if let Some(rest) = request.strip_prefix("/output create headless ") {
            self.monitors.push(Monitor {
                name: rest.trim().into(),
                id: 5,
                x: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
            });
        } else if let Some(rest) = request.strip_prefix("/output remove ") {
            self.monitors.retain(|m| m.name != rest.trim());
        } else if request.starts_with("/eval hl.monitor(") {
            let mode = quoted(request, "mode").unwrap();
            let (w, h) = mode.split_once('@').unwrap().0.split_once('x').unwrap();
            let change = ModeChange {
                name: quoted(request, "output").unwrap().to_owned(),
                x: quoted(request, "position")
                    .unwrap()
                    .split_once('x')
                    .unwrap()
                    .0
                    .parse()
                    .unwrap(),
                width: w.parse().unwrap(),
                height: h.parse().unwrap(),
                scale: request
                    .split_once("scale = ")
                    .unwrap()
                    .1
                    .split([' ', '}'])
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap(),
            };
            self.mode_changes
                .push((Instant::now(), (change.width, change.height)));
            if self.mode_lag == 0 {
                self.apply(change);
            } else {
                self.pending_mode = Some((change, self.mode_lag));
            }
        } else if request.starts_with("/dispatch hl.dsp.window.move(")
            && let Some(name) = quoted(request, "workspace")
        {
            self.workspace = name.strip_prefix("name:").unwrap_or(name).to_owned();
            if self.on_twin() {
                self.retile();
            }
        }
        "ok".into()
    }
}

/// The value of `key = "…"` in a Lua call.
fn quoted<'a>(request: &'a str, key: &str) -> Option<&'a str> {
    let rest = request.split_once(&format!("{key} = \""))?.1;
    Some(rest.split_once('"')?.0)
}

/// A fake Hyprland instance serving one world.
struct Fake {
    dir: PathBuf,
    world: Arc<Mutex<World>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fake {
    fn start() -> Fake {
        static N: AtomicU32 = AtomicU32::new(0);
        // Unix socket paths are limited to 108 bytes: keep the directory short.
        let dir = std::env::temp_dir().join(format!(
            "cpfk-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("hypr/sig")).unwrap();
        let listener = UnixListener::bind(dir.join("hypr/sig/.socket.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let world = Arc::new(Mutex::new(World::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (world, stop) = (world.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut conn, _)) => {
                            conn.set_nonblocking(false).unwrap();
                            conn.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                            let mut buf = [0u8; 2048];
                            let n = conn.read(&mut buf).unwrap_or(0);
                            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                            let reply = world.lock().unwrap().handle(&request);
                            let _ = conn.write_all(reply.as_bytes());
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(1)),
                    }
                }
            })
        };
        Fake {
            dir,
            world,
            stop,
            thread: Some(thread),
        }
    }

    fn ipc(&self) -> HyprIpc {
        HyprIpc::new("sig", &self.dir, Duration::from_millis(500))
    }

    fn parking(&self) -> HyprlandParking {
        HyprlandParking::new(self.ipc(), self.dir.join("parking.json")).unwrap()
    }

    fn set_client(&self, client: Client) {
        self.world.lock().unwrap().client = client;
    }

    /// Bars take `bar` on the twin from the next layout; `monitors` reports them after
    /// `report_after` more polls of `clients` (at once for 0).
    fn set_bar(&self, bar: [i64; 4], report_after: u32) {
        let mut world = self.world.lock().unwrap();
        world.bar = bar;
        if report_after == 0 {
            world.reported = bar;
            world.report_in = None;
        } else {
            world.report_in = Some(report_after);
        }
    }

    fn set_mode_lag(&self, queries: u32) {
        self.world.lock().unwrap().mode_lag = queries;
    }

    /// The window's goal size and whether the client still has a size on its way, read straight
    /// from the model (no query served, so nothing advances).
    fn client_state(&self) -> (Size, bool) {
        let world = self.world.lock().unwrap();
        (world.size, world.pending.is_some())
    }

    /// Every `clients` poll served so far: when, and the size it showed.
    fn served(&self) -> Vec<(Instant, Size)> {
        self.world.lock().unwrap().served.clone()
    }

    /// The `clients` polls served since the last mode change was asked for.
    fn served_since_last_mode_change(&self) -> Vec<(Instant, Size)> {
        let world = self.world.lock().unwrap();
        let since = world.mode_changes.last().expect("a mode change").0;
        world
            .served
            .iter()
            .filter(|(at, _)| *at >= since)
            .cloned()
            .collect()
    }

    fn mode_changes(&self) -> usize {
        self.world.lock().unwrap().mode_changes.len()
    }

    /// The dimensions of every mode change asked for from the `from`th on, in order.
    fn mode_sizes_from(&self, from: usize) -> Vec<Size> {
        let world = self.world.lock().unwrap();
        world.mode_changes[from..].iter().map(|m| m.1).collect()
    }

    /// The twin's mode (device pixels).
    fn twin_mode(&self) -> Option<(i64, i64)> {
        let world = self.world.lock().unwrap();
        world.twin().map(|m| (m.width, m.height))
    }

    fn journal(&self) -> String {
        std::fs::read_to_string(self.dir.join("parking.json")).unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn size_of(parked: &Parked) -> (i32, i32) {
    (parked.content.width(), parked.content.height())
}

fn origin_of(parked: &Parked) -> (i32, i32) {
    (parked.content.min.x, parked.content.min.y)
}

fn content(x0: i32, y0: i32, x1: i32, y1: i32) -> crosspane_types::geom::PixelRect {
    crosspane_types::geom::PixelRect::new(point2(x0, y0), point2(x1, y1))
}

/// The time between the first and the last of `polls`.
fn span(polls: &[(Instant, Size)]) -> Duration {
    match (polls.first(), polls.last()) {
        (Some((first, _)), Some((last, _))) => last.duration_since(*first),
        _ => Duration::ZERO,
    }
}

#[test]
fn shrink_reports_what_the_client_took_not_its_old_size() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    let parked = parking
        .park(WINDOW, PixelSize::new(1600, 1200), 1.0)
        .unwrap();
    assert_eq!(size_of(&parked), (1600, 1200));
    assert_eq!(origin_of(&parked), (0, 0));

    // The output is 1200x900 at once; the client reports 1600x1200 for six more polls.
    fake.set_client(Client::Follows { lag: 6 });
    let polls = fake.served().len();
    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 1.0)
        .unwrap();
    // Before anything else is asked: the client had really taken its new size by the time
    // `resize` returned, and the last poll it was answered from showed it. (Stale content,
    // clipped to the output, would look the same in `parked`.)
    assert_eq!(fake.client_state(), ((1200, 900), false));
    assert_eq!(fake.served().last().unwrap().1, (1200, 900));
    assert!(
        fake.served().len() - polls >= 7,
        "settled before the client answered: {} polls",
        fake.served().len() - polls
    );
    assert_eq!(size_of(&parked), (1200, 900), "{parked:?}");
    assert_eq!(origin_of(&parked), (0, 0));

    // The same window, later, with nothing changing: the geometry call agrees.
    assert_eq!(size_of(&parking.geometry(WINDOW).unwrap()), (1200, 900));

    // And the window goes home, the journal empties, the twin goes away.
    parking.restore(WINDOW).unwrap();
    assert_eq!(fake.journal().trim(), "[]");
    let world = fake.world.lock().unwrap();
    assert_eq!(world.workspace, "1");
    assert!(world.twin().is_none());
}

#[test]
fn grow_completes_once_the_client_fills() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking.park(WINDOW, PixelSize::new(800, 600), 1.0).unwrap();
    fake.set_client(Client::Follows { lag: 4 });
    let polls = fake.served().len();
    let parked = parking
        .resize(WINDOW, PixelSize::new(1600, 1200), 1.0)
        .unwrap();
    assert_eq!(fake.client_state(), ((1600, 1200), false));
    assert_eq!(size_of(&parked), (1600, 1200));
    assert!(fake.served().len() - polls >= 5);
}

#[test]
fn shrink_at_double_scale_compares_in_twin_pixels() {
    // Device pixels at scale 2: the window goes from 1600x1200 to 1200x900 logical.
    let fake = Fake::start();
    let mut parking = fake.parking();
    let parked = parking
        .park(WINDOW, PixelSize::new(3200, 2400), 2.0)
        .unwrap();
    assert_eq!(size_of(&parked), (3200, 2400));
    fake.set_client(Client::Follows { lag: 5 });
    let polls = fake.served().len();
    let parked = parking
        .resize(WINDOW, PixelSize::new(2400, 1800), 2.0)
        .unwrap();
    assert_eq!(fake.client_state(), ((1200, 900), false));
    assert_eq!(fake.served().last().unwrap().1, (1200, 900));
    assert!(fake.served().len() - polls >= 6);
    assert_eq!(size_of(&parked), (2400, 1800));
}

#[test]
fn an_app_that_takes_its_own_size_completes_when_it_holds_still() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking
        .park(WINDOW, PixelSize::new(1600, 1200), 1.0)
        .unwrap();
    // 1200x900 is asked for; the app keeps a 1200x700 window (it fits the output).
    fake.set_client(Client::Chooses {
        size: (1200, 700),
        lag: 3,
    });
    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 1.0)
        .unwrap();
    assert_eq!(size_of(&parked), (1200, 700));
    assert_eq!(origin_of(&parked), (0, 0));
    // It waited for the size to hold still: the fake showed 1200x700 over a stretch of polls
    // that lasted (nearly) STABLE before the call returned. A lower bound only.
    let held: Vec<_> = fake
        .served()
        .into_iter()
        .filter(|(_, size)| *size == (1200, 700))
        .collect();
    assert!(held.len() >= 2, "{} polls", held.len());
    assert!(
        span(&held) >= Duration::from_millis(100),
        "held for {:?}",
        span(&held)
    );
}

#[test]
fn an_app_that_never_changes_completes_at_the_deadline_clipped_to_the_output() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking
        .park(WINDOW, PixelSize::new(1600, 1200), 1.0)
        .unwrap();
    fake.set_client(Client::Fixed);
    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 1.0)
        .unwrap();
    // The fake served polls for the whole of the settle deadline (1.5 s), all showing the
    // unchanged window.
    let polls = fake.served_since_last_mode_change();
    assert!(
        span(&polls) >= Duration::from_millis(1400),
        "{:?}",
        span(&polls)
    );
    assert!(polls.iter().all(|(_, size)| *size == (1600, 1200)));
    // The window is 1600x1200 and the output 1200x900: only what is on the output is reported,
    // so a capture crop and an input position can't leave it.
    assert_eq!(size_of(&parked), (1200, 900));
    assert_eq!(origin_of(&parked), (0, 0));
    assert_eq!(size_of(&parking.geometry(WINDOW).unwrap()), (1200, 900));
}

#[test]
fn a_window_that_leaves_its_twin_times_out_at_the_settle_deadline() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking
        .park(WINDOW, PixelSize::new(1600, 1200), 1.0)
        .unwrap();
    // Something moves the window back to a real workspace and the client never shrinks.
    {
        let mut world = fake.world.lock().unwrap();
        world.workspace = "2".into();
        world.client = Client::Fixed;
        world.pending = None;
    }
    let result = parking.resize(WINDOW, PixelSize::new(1200, 900), 1.0);
    assert!(matches!(result, Err(PlatformError::Timeout)), "{result:?}");
    // It was the settle deadline, not an IPC timeout: the fake kept answering polls for the
    // whole of it (an IPC timeout would have ended the call within 500 ms).
    let polls = fake.served_since_last_mode_change();
    assert!(
        span(&polls) >= Duration::from_millis(1400),
        "{:?}",
        span(&polls)
    );
}

#[test]
fn a_minimum_size_larger_than_the_request_grows_the_output_to_the_apps_size() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking.park(WINDOW, PixelSize::new(800, 600), 1.0).unwrap();
    // 300x200 is asked for, from 800x600. The app can't go below 400x300.
    fake.set_client(Client::AtLeast {
        min: (400, 300),
        lag: 2,
    });
    let modes = fake.mode_changes();
    let parked = parking
        .resize(WINDOW, PixelSize::new(300, 200), 1.0)
        .unwrap();
    // The app's own size, unclipped, on an output that was grown to hold it: one mode change for
    // the request and one for the fit.
    assert_eq!(parked.content, content(0, 0, 400, 300));
    assert_eq!(fake.twin_mode(), Some((400, 300)));
    assert_eq!(fake.mode_changes() - modes, 2);
    assert_eq!(fake.client_state().0, (400, 300));
    // The fit waited for the size to hold still first.
    let held: Vec<_> = fake
        .served()
        .into_iter()
        .filter(|(_, size)| *size == (400, 300))
        .collect();
    assert!(held.len() >= 2);
    let world = fake.world.lock().unwrap();
    let fit = &world.mode_changes.last().unwrap().0;
    let first = held.first().unwrap().0;
    assert!(
        fit.duration_since(first) >= Duration::from_millis(100),
        "grew {:?} after the app took its size",
        fit.duration_since(first)
    );
    drop(world);
    assert_eq!(parking.geometry(WINDOW).unwrap().content, parked.content);
}

#[test]
fn growing_for_a_minimum_size_keeps_the_bars_padding() {
    let fake = Fake::start();
    // A bar on the left (10) and the top (26) of every output.
    fake.set_bar([10, 26, 0, 0], 0);
    let mut parking = fake.parking();
    let parked = parking.park(WINDOW, PixelSize::new(800, 600), 1.0).unwrap();
    assert_eq!(parked.content, content(10, 26, 810, 626));
    assert_eq!(fake.twin_mode(), Some((810, 626)));

    fake.set_client(Client::AtLeast {
        min: (400, 300),
        lag: 2,
    });
    let parked = parking
        .resize(WINDOW, PixelSize::new(300, 200), 1.0)
        .unwrap();
    // The output grew to the app's size plus the bars; the content is offset by them.
    assert_eq!(fake.twin_mode(), Some((410, 326)));
    assert_eq!(parked.content, content(10, 26, 410, 326));
}

#[test]
fn a_repad_after_a_fit_keeps_the_fitted_work_area() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking.park(WINDOW, PixelSize::new(800, 600), 1.0).unwrap();
    // A 26 px bar takes the top of the output when it is laid out, but `monitors` reports it only
    // after the fit: 18 polls is longer than the app's size needs to hold still (STABLE is at
    // least ten polls of 20 ms after it first shows, about the fourth poll), so the fit comes
    // first.
    fake.set_client(Client::AtLeast {
        min: (400, 300),
        lag: 2,
    });
    fake.set_bar([0, 26, 0, 0], 18);
    let modes = fake.mode_changes();
    let parked = parking
        .resize(WINDOW, PixelSize::new(300, 200), 1.0)
        .unwrap();
    // The modes asked for, in order: the request (the bar isn't reported yet, so unpadded); the
    // fit to the app's 400x300 work area; the repad, which pads the *fitted* work area (400x300
    // plus the bar). Repadding the original request would give 300x226 and clip the app.
    assert_eq!(
        fake.mode_sizes_from(modes),
        [(300, 200), (400, 300), (400, 326)]
    );
    assert_eq!(fake.twin_mode(), Some((400, 326)));
    // The app's own size, unclipped, below the bar.
    assert_eq!(parked.content, content(0, 26, 400, 326), "{parked:?}");
    assert_eq!(fake.client_state().0, (400, 300));
}

#[test]
fn a_late_bar_repad_is_not_completed_from_geometry_before_it() {
    let fake = Fake::start();
    let mut parking = fake.parking();
    parking
        .park(WINDOW, PixelSize::new(1000, 800), 1.0)
        .unwrap();
    // A 50 px bar takes the top of the output as soon as it is laid out, but the compositor
    // only reports the reservation to `monitors` three polls later, and a mode change then takes
    // a dozen queries (a quarter of a second) to show. The window is squeezed to 1200x850 first
    // and holds that size longer than STABLE before the repad's mode lands; the repad then
    // makes it 1200x900 a poll after the *monitor* shows the new mode.
    fake.set_client(Client::Follows { lag: 1 });
    fake.set_mode_lag(12);
    fake.set_bar([0, 50, 0, 0], 3);
    let modes = fake.mode_changes();
    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 1.0)
        .unwrap();
    assert_eq!(fake.twin_mode(), Some((1200, 950)));
    // The client had taken the repadded size when `resize` returned, and it wasn't the squeezed
    // one the monitor-before-client ordering left on show right after the repad.
    assert_eq!(fake.client_state(), ((1200, 900), false));
    assert_eq!(fake.served().last().unwrap().1, (1200, 900));
    assert_eq!(parked.content, content(0, 50, 1200, 950), "{parked:?}");
    // Two mode changes: the request and the repad.
    assert_eq!(fake.mode_changes() - modes, 2);
}

#[test]
fn bars_on_every_side_offset_the_content_at_double_scale() {
    let fake = Fake::start();
    // Reserved areas, in logical pixels: left 10, top 26, right 6, bottom 14.
    fake.set_bar([10, 26, 6, 14], 0);
    let mut parking = fake.parking();
    let parked = parking
        .park(WINDOW, PixelSize::new(1600, 1200), 2.0)
        .unwrap();
    // The mode is the request plus the bars (in device pixels); the window tiles inside the work
    // area, offset by the left and top bars.
    assert_eq!(fake.twin_mode(), Some((1632, 1280)));
    assert_eq!(parked.content, content(20, 52, 1620, 1252));
    assert_eq!(fake.client_state().0, (800, 600));

    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 2.0)
        .unwrap();
    assert_eq!(fake.twin_mode(), Some((1232, 980)));
    assert_eq!(parked.content, content(20, 52, 1220, 952));
    assert_eq!(parking.geometry(WINDOW).unwrap().content, parked.content);
}

#[test]
fn a_fixed_size_app_is_clipped_to_the_output_beside_bars_at_double_scale() {
    let fake = Fake::start();
    fake.set_bar([10, 26, 6, 14], 0);
    let mut parking = fake.parking();
    parking
        .park(WINDOW, PixelSize::new(1600, 1200), 2.0)
        .unwrap();
    // The app stays 800x600 logical (1600x1200 device) in a work area that shrinks to 600x450.
    fake.set_client(Client::Fixed);
    let parked = parking
        .resize(WINDOW, PixelSize::new(1200, 900), 2.0)
        .unwrap();
    assert_eq!(fake.twin_mode(), Some((1232, 980)));
    let polls = fake.served_since_last_mode_change();
    assert!(
        span(&polls) >= Duration::from_millis(1400),
        "{:?}",
        span(&polls)
    );
    // Offset by the bars, and cut at the output's right and bottom edges.
    assert_eq!(parked.content, content(20, 52, 1232, 980));
    assert_eq!(parking.geometry(WINDOW).unwrap().content, parked.content);
}
