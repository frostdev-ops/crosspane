//! E2 acceptance against dedicated nested compositors; never connect to the owner's session.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureTarget, Frame, FrameCapture, FrameEvent, IoGate, PlatformError, PointerInjector,
    StreamEndReason, StreamId,
};
use crosspane_platform_linux::hyprland::frame_capture::{HyprlandFrameCapture, TwinGeometry};
use crosspane_platform_linux::hyprland::inject::connect;
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_types::geom::{PixelRect, PixelSize, PointDevice, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};

fn nested_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh")
}

struct Nest(String);

fn verify_owned() {
    use std::os::unix::fs::FileTypeExt;
    assert_eq!(std::env::var("CROSSPANE_NESTED_HYPR").as_deref(), Ok("1"));
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    let name = std::env::var("CROSSPANE_FRAME_NEST").unwrap();
    let state = runtime.join(format!("crosspane-hypr-{name}"));
    let pid = std::fs::read_to_string(state.join("pid")).unwrap();
    let pid = pid.trim();
    let start = std::fs::read_to_string(state.join("start")).unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    assert_eq!(
        stat.rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap(),
        start.trim()
    );
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap()
            .trim(),
        "Hyprland"
    );
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let display = std::env::var("WAYLAND_DISPLAY").unwrap();
    let native = runtime.join("hypr").join(signature);
    assert_eq!(
        std::fs::read_to_string(native.join("hyprland.lock"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec![pid, display.as_str()]
    );
    assert!(
        std::fs::metadata(native.join(".socket.sock"))
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert!(
        std::fs::metadata(runtime.join(display))
            .unwrap()
            .file_type()
            .is_socket()
    );
}

#[derive(Clone)]
struct GuardedIpc(HyprIpc);
impl GuardedIpc {
    fn native(&self) -> HyprIpc {
        verify_owned();
        self.0.clone()
    }
    fn json(&self, request: &str) -> Result<serde_json::Value, PlatformError> {
        verify_owned();
        self.0.json(request)
    }
    fn monitor_ids(&self) -> Result<Vec<(String, u32)>, PlatformError> {
        verify_owned();
        self.0.monitor_ids()
    }
    fn eval(&self, lua: &str) -> Result<(), PlatformError> {
        verify_owned();
        self.0.eval(lua)
    }
    fn dispatch(&self, lua: &str) -> Result<(), PlatformError> {
        verify_owned();
        self.0.dispatch(lua)
    }
}

impl Drop for Nest {
    fn drop(&mut self) {
        assert!(
            Command::new(nested_script())
                .args(["stop", "--name", &self.0])
                .status()
                .unwrap()
                .success()
        );
    }
}

// Other platform acceptance tests move windows and outputs concurrently. Re-execute this test
// inside its own nest, without changing process-global environment variables.
fn dedicated(test: &str) -> Option<GuardedIpc> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs CROSSPANE_NESTED_HYPR=1 from scripts/hypr-nested.sh env");
        return None;
    }
    if std::env::var("CROSSPANE_FRAME_CHILD_TEST").as_deref() == Ok(test) {
        verify_owned();
        return Some(GuardedIpc(HyprIpc::from_env().unwrap()));
    }
    let nest = Nest(format!("wp-2-8-{test}-{}", std::process::id()));
    let parent = std::env::var("CROSSPANE_PARENT_WAYLAND_DISPLAY")
        .unwrap_or_else(|_| std::env::var("WAYLAND_DISPLAY").unwrap());
    assert!(
        Command::new(nested_script())
            .args(["start", "--name", &nest.0])
            .env("WAYLAND_DISPLAY", parent)
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("WAYLAND_SOCKET")
            .status()
            .unwrap()
            .success()
    );
    let exports = Command::new(nested_script())
        .args(["env", "--name", &nest.0])
        .output()
        .unwrap();
    assert!(exports.status.success());
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env("CROSSPANE_FRAME_CHILD_TEST", test)
        .env("CROSSPANE_FRAME_NEST", &nest.0)
        .env_remove("WAYLAND_SOCKET");
    for line in String::from_utf8(exports.stdout).unwrap().lines() {
        if line == "unset WAYLAND_SOCKET" {
            continue;
        }
        let (key, value) = line
            .strip_prefix("export ")
            .unwrap()
            .split_once('=')
            .unwrap();
        assert!(matches!(
            key,
            "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "CROSSPANE_NESTED_HYPR"
        ));
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        child.env(key, value);
    }
    let result = child.output().unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.status.success(), "dedicated capture test failed");
    None
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "fixture/compositor timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    child: Child,
    ipc: GuardedIpc,
    address: String,
    log: PathBuf,
    reference: PathBuf,
    rgb: Vec<u8>,
}

impl Fixture {
    fn new(ipc: &GuardedIpc) -> Self {
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("crosspane-testapp");
        if !binary.is_file() {
            assert!(
                Command::new("cargo")
                    .args(["build", "--locked", "-p", "crosspane-testapp"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let prefix = std::env::temp_dir().join(format!("crosspane-frame-{}", std::process::id()));
        let log = prefix.with_extension("jsonl");
        let reference = prefix.with_extension("ppm");
        assert!(
            Command::new(&binary)
                .args(["pattern", "--size", "300x200", "--out"])
                .arg(&reference)
                .status()
                .unwrap()
                .success()
        );
        let ppm = std::fs::read(&reference).unwrap();
        let header = b"P6\n300 200\n255\n";
        assert!(ppm.starts_with(header));
        verify_owned();
        let child = Command::new(binary)
            .args([
                "window",
                "--title",
                "crosspane-frame-fixture",
                "--size",
                "300x200",
                "--events",
            ])
            .arg(&log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child,
            ipc: ipc.clone(),
            address: String::new(),
            log,
            reference,
            rgb: ppm[header.len()..].to_vec(),
        };
        wait_until(|| {
            fixture.address = ipc
                .json("clients")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|client| client["title"] == "crosspane-frame-fixture")
                .and_then(|client| client["address"].as_str())
                .unwrap_or_default()
                .to_owned();
            !fixture.address.is_empty()
        });
        ipc.dispatch(&format!(
            r#"hl.dsp.window.float({{action="enable",window="address:{}"}})"#,
            fixture.address
        ))
        .unwrap();
        ipc.dispatch(&format!(
            r#"hl.dsp.window.resize({{x=300,y=200,relative=false,window="address:{}"}})"#,
            fixture.address
        ))
        .unwrap();
        fixture.move_to(0, 0);
        wait_until(|| {
            std::fs::read_to_string(&fixture.log)
                .unwrap_or_default()
                .contains("\"event\":\"ready\"")
        });
        std::thread::sleep(Duration::from_millis(200));
        fixture
    }

    fn move_to(&self, x: i32, y: i32) {
        self.ipc
            .dispatch(&format!(
                r#"hl.dsp.window.move({{x={x},y={y},relative=false,window="address:{}"}})"#,
                self.address
            ))
            .unwrap();
    }

    fn reference_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let offset = ((y * 300 + x) * 3) as usize;
        self.rgb[offset..offset + 3].try_into().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_file(&self.reference);
    }
}

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

fn rgb(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    frame
        .with_pixels(|pixels, stride| {
            let offset = (y * stride + x * 4) as usize;
            [pixels[offset + 2], pixels[offset + 1], pixels[offset]]
        })
        .unwrap()
}

fn frame_matching(
    events: &mpsc::Receiver<FrameEvent>,
    stream: StreamId,
    predicate: impl Fn(&Frame) -> bool,
) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { stream: id, frame } if id == stream && predicate(&frame) => {
                return frame;
            }
            FrameEvent::Frame { .. } => (),
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

fn ended(events: &mpsc::Receiver<FrameEvent>, stream: StreamId, reason: StreamEndReason) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { .. } => (),
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            FrameEvent::Ended {
                stream: id,
                reason: why,
            } => {
                assert_eq!((id, why), (stream, reason));
                return;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

// Opaque client content and a clipped xdg_popup give a pixel-for-pixel reference for the
// previous output crop. All pointer movement remains inside this test's owned compositor.
const TWIN_GTK: &str = r#"
import gi, pathlib, sys
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, Gdk, GLib
Gtk.init()
flag = pathlib.Path(sys.argv[1])
epoch_file = flag.parent / 'epoch'
win = Gtk.Window(title='crosspane-twin-gtk')
win.set_default_size(300, 200)
win.set_decorated(False)
fixed = Gtk.Fixed()
fixed.add_css_class('fixture')
win.set_child(fixed)
anchor = Gtk.Box()
anchor.set_size_request(1, 1)
fixed.put(anchor, 5, 10)
popover = Gtk.Popover()
popover.set_parent(anchor)
popover.set_autohide(False)
popover.set_has_arrow(False)
popover.set_position(Gtk.PositionType.BOTTOM)
box = Gtk.Box()
box.add_css_class('popup-color')
box.set_size_request(180, 120)
popover.set_child(box)
css = Gtk.CssProvider()
css.load_from_data(b'.fixture { background: rgb(0, 0, 200); } .popup-color { background: rgb(240, 10, 10); } popover contents { padding: 0; border: 0; border-radius: 0; box-shadow: none; }')
Gtk.StyleContext.add_provider_for_display(Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
last = 'off'
last_epoch = '0'
def tick():
    global last, last_epoch
    wanted = flag.read_text().strip()
    if wanted != last:
        popover.popup() if wanted == 'on' else popover.popdown()
        last = wanted
    epoch = epoch_file.read_text().strip()
    if epoch != last_epoch:
        css.load_from_data(('.fixture { background: rgb(0, ' + epoch + ', 200); } .popup-color { background: rgb(240, 10, 10); } popover contents { padding: 0; border: 0; border-radius: 0; box-shadow: none; }').encode())
        last_epoch = epoch
    fixed.queue_draw()
    return True
GLib.timeout_add(100, tick)
win.present()
GLib.MainLoop().run()
"#;

struct TwinFixture {
    child: Child,
    root: PathBuf,
    ipc: GuardedIpc,
    address: String,
    window: WindowId,
    output: String,
    display: DisplayId,
}

impl TwinFixture {
    fn new(ipc: &GuardedIpc) -> Self {
        let directory = Command::new("mktemp")
            .args(["-d", "/tmp/crosspane-wp249-capture.XXXXXX"])
            .output()
            .unwrap();
        assert!(directory.status.success());
        let root = PathBuf::from(String::from_utf8(directory.stdout).unwrap().trim());
        std::fs::write(root.join("popup"), "off").unwrap();
        std::fs::write(root.join("epoch"), "0").unwrap();
        verify_owned();
        let child = Command::new("python3")
            .args(["-c", TWIN_GTK])
            .arg(root.join("popup"))
            .env("GDK_BACKEND", "wayland")
            .env("GSK_RENDERER", "cairo")
            .env("GTK_A11Y", "none")
            .env("GSETTINGS_BACKEND", "memory")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(root.join("gtk.log")).unwrap())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child,
            root,
            ipc: ipc.clone(),
            address: String::new(),
            window: WindowId(0),
            output: String::new(),
            display: DisplayId(0),
        };
        let mut client = serde_json::Value::Null;
        wait_until(|| {
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "GTK fixture exited"
            );
            client = ipc
                .json("clients")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["title"] == "crosspane-twin-gtk")
                .cloned()
                .unwrap_or_default();
            !client.is_null()
        });
        fixture.address = client["address"].as_str().unwrap().to_owned();
        fixture.window =
            WindowId(u64::from_str_radix(client["stableId"].as_str().unwrap(), 16).unwrap());
        fixture.output = format!("CROSSPANE-{:x}", fixture.window.0);
        let workspace = format!("crosspane-{:x}", fixture.window.0);
        ipc.eval(&format!(r#"hl.config({{cursor={{no_hardware_cursors=1}}}}); hl.workspace_rule({{workspace="name:{workspace}",monitor="{}",default=true,gaps_in=0,gaps_out=0,border_size=0,no_rounding=true,no_shadow=true,decorate=false}})"#, fixture.output)).unwrap();
        hyprctl_output(&["create", "wayland", &fixture.output]);
        fixture.scale(1.0);
        ipc.dispatch(&format!(
            r#"hl.dsp.window.move({{window="address:{}",workspace="name:{workspace}"}})"#,
            fixture.address
        ))
        .unwrap();
        ipc.dispatch(&format!(
            r#"hl.dsp.window.float({{window="address:{}",action="set"}})"#,
            fixture.address
        ))
        .unwrap();
        fixture.resize(300, 200);
        ipc.dispatch(&format!(
            r#"hl.dsp.window.move({{window="address:{}",x=2050,y=50,relative=false}})"#,
            fixture.address
        ))
        .unwrap();
        wait_until(|| {
            let Some((_, id)) = ipc
                .monitor_ids()
                .unwrap()
                .into_iter()
                .find(|(name, _)| name == &fixture.output)
            else {
                return false;
            };
            fixture.display = DisplayId(id);
            verify_owned();
            TwinGeometry::from_env(fixture.window, fixture.display).is_ok()
        });
        std::fs::write(
            fixture.root.join("provenance.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "nest": std::env::var("CROSSPANE_FRAME_NEST").unwrap(),
                "signature": std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap(),
                "display": std::env::var("WAYLAND_DISPLAY").unwrap(),
                "monitors": ipc.json("monitors").unwrap(), "clients": ipc.json("clients").unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();
        fixture
    }

    fn resize(&self, width: i32, height: i32) {
        self.ipc.dispatch(&format!(r#"hl.dsp.window.resize({{window="address:{}",x={width},y={height},relative=false}})"#, self.address)).unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn scale(&self, scale: f64) {
        self.ipc
            .eval(&format!(
                r#"hl.monitor({{output="{}",mode="640x480@60",position="2000x0",scale={scale}}})"#,
                self.output
            ))
            .unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    fn geometry(&self) -> TwinGeometry {
        verify_owned();
        TwinGeometry::from_env(self.window, self.display).unwrap()
    }

    fn crops(&self) -> (PixelRect, PixelRect) {
        let g = self.geometry();
        let r = g.content;
        (r, g.map_crop(r).unwrap())
    }

    fn pointer(&self, on_twin: bool) {
        let monitors = self.ipc.json("monitors").unwrap();
        let name = if on_twin {
            self.output.as_str()
        } else {
            monitors
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] != self.output)
                .unwrap()["name"]
                .as_str()
                .unwrap()
        };
        let monitor = monitors
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap();
        let (x, y) = if on_twin {
            let g = self.geometry();
            let visible = g
                .content
                .intersection(&PixelRect::from_origin_and_size(
                    point2(g.origin.0, g.origin.1),
                    g.size.cast(),
                ))
                .unwrap();
            let scale = monitor["scale"].as_f64().unwrap();
            (
                monitor["x"].as_i64().unwrap()
                    + (f64::from(visible.center().x) / scale).round() as i64,
                monitor["y"].as_i64().unwrap()
                    + (f64::from(visible.center().y) / scale).round() as i64,
            )
        } else {
            (
                monitor["x"].as_i64().unwrap() + 20,
                monitor["y"].as_i64().unwrap() + 20,
            )
        };
        self.ipc
            .dispatch(&format!("hl.dsp.cursor.move({{x={x},y={y}}})"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }

    fn save(&self, label: &str, frame: &Frame) {
        let mut pixels =
            format!("P6\n{} {}\n255\n", frame.size.width, frame.size.height).into_bytes();
        for y in 0..frame.size.height {
            for x in 0..frame.size.width {
                pixels.extend(rgb(frame, x, y));
            }
        }
        std::fs::write(self.root.join(format!("{label}.ppm")), pixels).unwrap();
    }
}

impl Drop for TwinFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eprintln!("WP-2.49 pixel artifacts: {}", self.root.display());
    }
}

fn rgb_differences(a: &Frame, b: &Frame) -> usize {
    assert_eq!(a.size, b.size);
    (0..a.size.height)
        .flat_map(|y| (0..a.size.width).map(move |x| (x, y)))
        .filter(|&(x, y)| rgb(a, x, y) != rgb(b, x, y))
        .count()
}

fn red_pixels(frame: &Frame) -> usize {
    (0..frame.size.height)
        .flat_map(|y| (0..frame.size.width).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let pixel = rgb(frame, x, y);
            pixel[0] > 200 && pixel[1] < 40 && pixel[2] < 40
        })
        .count()
}

fn after_change_pair(
    events: &mpsc::Receiver<FrameEvent>,
    window: StreamId,
    output: StreamId,
    size: PixelSize,
    epoch: u8,
) -> (Frame, Frame) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut video = None;
    let mut reference = None;
    while video.is_none() || reference.is_none() {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { stream, frame } => {
                if stream == window {
                    // Reject *every* post-barrier Window frame, including while awaiting output.
                    assert_eq!(frame.size, size);
                }
                let marked = frame.size == size
                    && (0..size.height)
                        .any(|y| (0..size.width).any(|x| rgb(&frame, x, y) == [0, epoch, 200]));
                if marked && stream == window {
                    video = Some(frame);
                } else if marked && stream == output {
                    reference = Some(frame);
                }
            }
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            other => panic!("unexpected event after resize barrier: {other:?}"),
        }
    }
    (video.unwrap(), reference.unwrap())
}

fn assert_twin_padding(frame: &Frame, geometry: &TwinGeometry) {
    let q = geometry.map_crop(geometry.content).unwrap();
    frame
        .with_pixels(|pixels, stride| {
            for y in 0..frame.size.height {
                for x in 0..frame.size.width {
                    let wx = q.min.x + x as i32;
                    let wy = q.min.y + y as i32;
                    if wx < 0
                        || wy < 0
                        || wx >= geometry.size.width as i32
                        || wy >= geometry.size.height as i32
                    {
                        let at = (y * stride + x * 4) as usize;
                        assert_eq!(&pixels[at..at + 4], &[0, 0, 0, 255]);
                    }
                }
            }
        })
        .unwrap();
}

// The approved external-buffer padding is opaque black even when the compositor's output
// background is another color. Captured window pixels still match the old crop exactly.
fn assert_twin_reference(frame: &Frame, reference: &Frame, geometry: &TwinGeometry) {
    assert_eq!(frame.size, reference.size);
    let q = geometry.map_crop(geometry.content).unwrap();
    for y in 0..frame.size.height {
        for x in 0..frame.size.width {
            let wx = q.min.x + x as i32;
            let wy = q.min.y + y as i32;
            if wx >= 0
                && wy >= 0
                && wx < geometry.size.width as i32
                && wy < geometry.size.height as i32
            {
                assert_eq!(rgb(frame, x, y), rgb(reference, x, y), "pixel ({x},{y})");
            }
        }
    }
    assert_twin_padding(frame, geometry);
}

#[test]
fn twin_cursor_pixels_popup_and_same_session_resize_scale() {
    let Some(ipc) = dedicated("twin_cursor_pixels_popup_and_same_session_resize_scale") else {
        return;
    };
    let fixture = TwinFixture::new(&ipc);
    let (_keys, mut pointer) = connect(open_gate(), ipc.native()).unwrap();
    let g = fixture.geometry();
    pointer
        .move_to(
            fixture.display,
            PointDevice::new(f64::from(g.origin.0 + 10), f64::from(g.origin.1 + 10)),
        )
        .unwrap();
    fixture.pointer(false);
    let (r, q) = fixture.crops();
    let mut capture = HyprlandFrameCapture::new(open_gate(), ipc.native()).unwrap();
    let (send, events) = mpsc::channel();
    let sink = Arc::new(move |e| {
        let _ = send.send(e);
    });
    let window = capture
        .start(
            CaptureTarget::Window(fixture.window),
            Some(q),
            60,
            sink.clone(),
        )
        .unwrap();
    let output = capture
        .start(CaptureTarget::Display(fixture.display), Some(r), 60, sink)
        .unwrap();
    let size = PixelSize::new(r.width() as u32, r.height() as u32);
    let baseline = frame_matching(&events, window, |f| f.size == size);
    let old_crop = frame_matching(&events, output, |f| f.size == size);
    assert_eq!(rgb_differences(&baseline, &old_crop), 0);
    fixture.save("window-elsewhere", &baseline);
    fixture.save("old-crop-elsewhere", &old_crop);
    fixture.pointer(true);
    while events.try_recv().is_ok() {}
    let pointer_window = frame_matching(&events, window, |f| f.size == size);
    let pointer_output = frame_matching(&events, output, |f| {
        f.size == size && rgb_differences(&old_crop, f) > 0
    });
    assert_eq!(rgb_differences(&baseline, &pointer_window), 0);
    let cursor_pixels = rgb_differences(&old_crop, &pointer_output);
    assert!(cursor_pixels > 0);
    fixture.save("window-on-twin", &pointer_window);
    fixture.save("old-crop-on-twin", &pointer_output);
    fixture.pointer(false);
    std::fs::write(fixture.root.join("popup"), "on").unwrap();
    let popup = frame_matching(&events, window, |f| f.size == size && red_pixels(f) > 5);
    let popup_crop = frame_matching(&events, output, |f| f.size == size && red_pixels(f) > 5);
    assert_eq!(rgb_differences(&popup, &popup_crop), 0);
    fixture.save("window-popup", &popup);
    fixture.save("old-crop-popup", &popup_crop);
    std::fs::write(fixture.root.join("popup"), "off").unwrap();
    for (width, height, scale, epoch) in [(360, 240, 1.0, 70_u8), (360, 240, 1.25, 120_u8)] {
        fixture.resize(width, height);
        fixture.scale(scale);
        let geometry = fixture.geometry();
        let (r, q) = fixture.crops();
        capture.set_crop(window, Some(q)).unwrap();
        capture.set_crop(output, Some(r)).unwrap();
        while events.try_recv().is_ok() {}
        // A distinct client color proves the frame was rendered after this resize+scale.
        // The SAME Window session and a live output-crop reference both observe that barrier.
        std::fs::write(fixture.root.join("epoch"), epoch.to_string()).unwrap();
        let expected = PixelSize::new(r.width() as u32, r.height() as u32);
        let (frame, reference) = after_change_pair(&events, window, output, expected, epoch);
        fixture.save(&format!("same-session-{scale}"), &frame);
        fixture.save(&format!("same-session-reference-{scale}"), &reference);
        std::fs::write(
            fixture
                .root
                .join(format!("same-session-geometry-{scale}.txt")),
            format!("{geometry:?}\nR={r:?}\nQ={q:?}\n"),
        )
        .unwrap();
        assert_twin_reference(&frame, &reference, &geometry);
        eprintln!(
            "WP-2.49 SAME session {} scale {scale}: {:?}",
            window.0, frame.size
        );
    }
    eprintln!(
        "WP-2.49: cursor pixels old output={cursor_pixels}, Window=0; baseline and GTK popup pixel differences=0"
    );
    capture.stop(output).unwrap();
    ended(&events, output, StreamEndReason::Requested);
    capture.stop(window).unwrap();
    ended(&events, window, StreamEndReason::Requested);
}

#[test]
fn twin_output_removal_and_window_close_end_target_gone() {
    let Some(ipc) = dedicated("twin_output_removal_and_window_close_end_target_gone") else {
        return;
    };
    let mut fixture = TwinFixture::new(&ipc);
    let (_, q) = fixture.crops();
    let mut capture = HyprlandFrameCapture::new(open_gate(), ipc.native()).unwrap();
    let (send, events) = mpsc::channel();
    let sink = Arc::new(move |e| {
        let _ = send.send(e);
    });
    let window = capture
        .start(
            CaptureTarget::Window(fixture.window),
            Some(q),
            60,
            sink.clone(),
        )
        .unwrap();
    fixture.child.kill().unwrap();
    fixture.child.wait().unwrap();
    ended(&events, window, StreamEndReason::TargetGone);
    hyprctl_output(&["remove", &fixture.output]);
    drop(fixture);
    let mut fixture = TwinFixture::new(&ipc);
    let (_, q) = fixture.crops();
    let window = capture
        .start(CaptureTarget::Window(fixture.window), Some(q), 60, sink)
        .unwrap();
    hyprctl_output(&["remove", &fixture.output]);
    ended(&events, window, StreamEndReason::TargetGone);
    assert!(fixture.child.try_wait().unwrap().is_none());
    assert!(
        ipc.json("clients")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|client| {
                client["stableId"].as_str().is_some_and(|id| {
                    u64::from_str_radix(id.trim_start_matches("0x"), 16).ok()
                        == Some(fixture.window.0)
                }) && client["mapped"].as_bool() == Some(true)
            })
    ); // TargetGone is output loss, while the Window remains mapped and its GTK process alive.
    assert!(capture.set_crop(window, Some(q)).is_err());
}

#[test]
fn pixels_damage_crop_gate_and_stop() {
    let Some(ipc) = dedicated("pixels_damage_crop_gate_and_stop") else {
        return;
    };
    let fixture = Fixture::new(&ipc);
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let gate = open_gate();
    let mut capture = HyprlandFrameCapture::new(gate.clone(), ipc.native()).unwrap();
    let (send, events) = mpsc::channel();
    let sink = Arc::new(move |event| {
        let _ = send.send(event);
    });
    assert!(matches!(
        capture.start(
            CaptureTarget::Window(WindowId(u64::MAX)),
            None,
            60,
            sink.clone()
        ),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        capture.start(
            CaptureTarget::Display(DisplayId(u32::MAX)),
            None,
            60,
            sink.clone()
        ),
        Err(PlatformError::NotFound)
    ));
    let started = Instant::now();
    let stream = capture
        .start(CaptureTarget::Display(display), None, 60, sink.clone())
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let points = [
        (0, 5, [255, 0, 0]),
        (1, 1, [255, 255, 255]),
        (2, 1, [0, 0, 0]),
        (3, 3, [255, 255, 255]),
        (4, 3, [0, 0, 0]),
        (5, 4, [0, 0, 0]),
        (5, 5, [255, 255, 255]),
    ];
    let full = frame_matching(&events, stream, |_| true);
    for &(x, y, expected) in &points {
        assert_eq!(
            rgb(&full, x, y),
            expected,
            "reference position ({x}, {y}), frame {:?}, clients {}",
            full.size,
            ipc.json("clients").unwrap()
        );
    }
    // Parent tiling can resize a nest while this test runs. Sample the visible reference pattern,
    // and use the captured dimensions instead of comparing two asynchronous geometry snapshots.
    assert!(full.size.width >= 12 && full.size.height >= 12);
    let (pixels, stride) = full.cpu_pixels().unwrap();
    assert_eq!(stride, full.size.width * 4);
    assert_eq!(pixels.len(), (stride * full.size.height) as usize);
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let now_ns = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
    assert!(full.at.as_nanos().abs_diff(now_ns) < 2_000_000_000);

    while events.try_recv().is_ok() {}
    fixture.move_to(2, 2);
    let moved = frame_matching(&events, stream, |frame| {
        points
            .iter()
            .all(|&(x, y, expected)| rgb(frame, x + 2, y + 2) == expected)
    });
    assert!(!moved.damage.as_ref().unwrap().is_empty());
    assert!(moved.at >= full.at);

    let crop = PixelRect::new(point2(4, 4), point2(12, 12));
    let started = Instant::now();
    capture.set_crop(stream, Some(crop)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    fixture.move_to(3, 2);
    let cropped = frame_matching(&events, stream, |frame| {
        frame.size == PixelSize::new(8, 8)
            && (0..8)
                .all(|y| (0..8).all(|x| rgb(frame, x, y) == fixture.reference_rgb(x + 1, y + 2)))
    });
    let (pixels, stride) = cropped.cpu_pixels().unwrap();
    assert_eq!(stride, 32);
    assert_eq!(pixels.len(), 256);
    let second = capture
        .start(CaptureTarget::Display(display), None, 60, sink.clone())
        .unwrap();
    frame_matching(&events, second, |_| true);
    while events.try_recv().is_ok() {}
    gate.set_engine_permits(false);
    let mut blocked = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while blocked.len() < 2 {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Ended { stream, reason } => {
                assert_eq!(reason, StreamEndReason::Blocked);
                blocked.push(stream);
            }
            FrameEvent::Frame { .. } if blocked.is_empty() => (),
            FrameEvent::Cursor { .. } if blocked.is_empty() => (),
            FrameEvent::CursorDefault { .. } if blocked.is_empty() => (),
            other => panic!("frame after gate shutdown: {other:?}"),
        }
    }
    blocked.sort();
    assert_eq!(blocked, vec![stream, second]);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(matches!(
        capture.start(CaptureTarget::Display(display), None, 60, sink.clone()),
        Err(PlatformError::Locked)
    ));
    gate.set_engine_permits(true);
    fixture.move_to(4, 2);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());

    let stream = capture
        .start(CaptureTarget::Display(display), None, 60, sink)
        .unwrap();
    frame_matching(&events, stream, |_| true);
    let started = Instant::now();
    capture.stop(stream).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    ended(&events, stream, StreamEndReason::Requested);
    fixture.move_to(5, 2);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

fn hyprctl_output(arguments: &[&str]) {
    verify_owned();
    assert_eq!(std::env::var("CROSSPANE_NESTED_HYPR").as_deref(), Ok("1"));
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    // The spec explicitly calls for hyprctl output create/remove in tests. Product IPC remains
    // exclusively HyprIpc::monitor_ids; every test command names the dedicated nest and times out.
    let result = Command::new("timeout")
        .args(["2", "hyprctl", "-i", &signature, "output"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "ok");
}

struct ExtraOutput(String);
impl Drop for ExtraOutput {
    fn drop(&mut self) {
        hyprctl_output(&["remove", &self.0]);
    }
}

#[test]
fn removed_output_ends_target_gone() {
    let Some(ipc) = dedicated("removed_output_ends_target_gone") else {
        return;
    };
    let gate = open_gate();
    let mut capture = HyprlandFrameCapture::new(gate, ipc.native()).unwrap();
    let before = ipc.monitor_ids().unwrap();
    hyprctl_output(&["create", "wayland"]);
    let mut output = None;
    wait_until(|| {
        output = ipc
            .monitor_ids()
            .unwrap()
            .into_iter()
            .find(|pair| !before.contains(pair));
        output.is_some()
    });
    let (name, id) = output.unwrap();
    let output = ExtraOutput(name);
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(DisplayId(id)),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    frame_matching(&events, stream, |_| true);
    drop(output);
    ended(&events, stream, StreamEndReason::TargetGone);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn continuously_animating_window_at_60_fps() {
    let Some(ipc) = dedicated("continuously_animating_window_at_60_fps") else {
        return;
    };
    let fixture = Fixture::new(&ipc);
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let mut capture = HyprlandFrameCapture::new(open_gate(), ipc.native()).unwrap();
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(display),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send((Instant::now(), event));
            }),
        )
        .unwrap();
    // The reference app is static. Animate its actual window at 125 Hz using a compositor timer,
    // without modifying the fixture or adding an animation dependency to the adapter crate.
    ipc.eval(&format!(
        r#"
        local phase = 0
        crosspane_wp28_timer = hl.timer(function()
            phase = (phase + 1) % 100
            hl.dispatch(hl.dsp.window.move({{x=20+phase,y=20,relative=false,window="address:{}"}}))
        end, {{timeout=8,type="repeat"}})
    "#,
        fixture.address
    ))
    .unwrap();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(3);
    let mut delivered = Vec::new();
    let mut damaged = 0;
    while let Ok((at, event)) =
        events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        match event {
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            FrameEvent::Frame { stream: id, frame } => {
                assert_eq!(id, stream);
                if at >= start {
                    delivered.push(at);
                }
                if frame
                    .damage
                    .as_ref()
                    .is_some_and(|damage| !damage.is_empty())
                {
                    damaged += 1;
                }
            }
            other => panic!("capture ended during animation: {other:?}"),
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    ipc.eval("crosspane_wp28_timer:set_enabled(false); crosspane_wp28_timer = nil")
        .unwrap();
    let fps = delivered.len() as f64 / start.elapsed().as_secs_f64();
    assert!(delivered.len() >= 2 && damaged > 0);
    assert!(
        delivered
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0])
                >= Duration::from_nanos(1_000_000_000_u64.div_ceil(60)))
    );
    assert!(fps <= 60.0, "delivered {fps:.2} fps");
    eprintln!(
        "WP-2.8 max_fps=60: {} frames, {:.3} seconds, {fps:.2} fps",
        delivered.len(),
        start.elapsed().as_secs_f64()
    );
    capture.stop(stream).unwrap();
}

#[test]
fn cursor_image_and_stop() {
    let Some(ipc) = dedicated("cursor_image_and_stop") else {
        return;
    };
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let gate = open_gate();
    // Ensure the nest has a pointer device before the capture worker binds its seat.
    let (_keys, mut pointer) = connect(gate.clone(), ipc.native()).unwrap();
    let mut capture = HyprlandFrameCapture::new(gate, ipc.native()).unwrap();
    capture.set_cursor_capture(true);
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(display),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    // Preserve the first cursor event queued during start: an unchanged cursor may never
    // produce a second frame. Obtain dimensions without consuming capture events.
    let monitors = ipc.json("monitors").unwrap();
    let monitor = monitors
        .as_array()
        .unwrap()
        .iter()
        .find(|monitor| monitor["id"].as_u64() == Some(u64::from(display.0)))
        .unwrap();
    let width = monitor["width"].as_f64().unwrap();
    let height = monitor["height"].as_f64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    pointer
        .move_to(display, PointDevice::new(width / 2.0, height / 2.0))
        .unwrap();
    let image = loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Cursor {
                stream: id,
                cursor: Some(image),
            } => {
                assert_eq!(id, stream);
                break Some(image);
            }
            FrameEvent::CursorDefault { stream: id } => {
                assert_eq!(id, stream);
                break None;
            }
            FrameEvent::Frame { .. } => (),
            other => panic!("unexpected cursor capture event: {other:?}"),
        }
    };
    if let Some(image) = image {
        assert!(image.size.width > 0 && image.size.width <= 256);
        assert!(image.size.height > 0 && image.size.height <= 256);
        assert_eq!(
            image.pixels.len(),
            image.size.width as usize * image.size.height as usize * 4
        );
        assert!(image.hotspot.0 < image.size.width && image.hotspot.1 < image.size.height);
        assert!(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] == 255)
        );
    }

    capture.stop(stream).unwrap();
    // Drain events already delivered before stop; Ended is the stream's terminal barrier.
    ended(&events, stream, StreamEndReason::Requested);
    pointer
        .move_to(display, PointDevice::new(width / 4.0, height / 4.0))
        .unwrap();
    assert!(events.recv_timeout(Duration::from_millis(250)).is_err());
}

#[test]
fn no_cursor_session_by_default() {
    let Some(ipc) = dedicated("no_cursor_session_by_default") else {
        return;
    };
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let gate = open_gate();
    let (_keys, mut pointer) = connect(gate.clone(), ipc.native()).unwrap();
    let mut capture = HyprlandFrameCapture::new(gate, ipc.native()).unwrap();
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(display),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    pointer
        .move_to(display, PointDevice::new(100.0, 100.0))
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut frames = 0;
    while let Ok(event) = events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        match event {
            FrameEvent::Frame { .. } => frames += 1,
            other => panic!("unexpected event without cursor capture: {other:?}"),
        }
    }
    assert!(frames > 0, "no frames delivered");
    capture.stop(stream).unwrap();
    ended(&events, stream, StreamEndReason::Requested);
}

// This fixture uses the release binary required by WP-2.18a and never targets the live session.
#[test]
fn window_pixels_resize_close_and_unknown_id() {
    let Some(ipc) = dedicated("window_pixels_resize_close_and_unknown_id") else {
        return;
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = root.join("target/release/crosspane-testapp");
    if !binary.is_file() {
        assert!(
            Command::new("cargo")
                .current_dir(&root)
                .args(["build", "--release", "--locked", "-p", "crosspane-testapp"])
                .status()
                .unwrap()
                .success()
        );
    }
    let title = format!("wp218a-window-{}", std::process::id());
    let child = Command::new(binary)
        .current_dir(&root)
        .args(["window", "--title", &title, "--size", "320x240"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(child);
    let mut client = serde_json::Value::Null;
    wait_until(|| {
        client = ipc
            .json("clients")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["title"] == title)
            .cloned()
            .unwrap_or_default();
        !client.is_null()
    });
    let address = client["address"].as_str().unwrap();
    let id = WindowId(u64::from_str_radix(client["stableId"].as_str().unwrap(), 16).unwrap());
    ipc.dispatch(&format!(
        r#"hl.dsp.window.float({{action="enable",window="address:{address}"}})"#
    ))
    .unwrap();
    ipc.dispatch(&format!(
        r#"hl.dsp.window.resize({{x=320,y=240,relative=false,window="address:{address}"}})"#
    ))
    .unwrap();
    wait_until(|| {
        ipc.json("clients")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["address"] == address && c["size"] == serde_json::json!([320, 240]))
    });
    let mut capture = HyprlandFrameCapture::new(open_gate(), ipc.native()).unwrap();
    // Even an explicitly enabled output cursor option must never create a window cursor session.
    capture.set_cursor_capture(true);
    let (send, events) = mpsc::channel();
    let sink = Arc::new(move |event| {
        let _ = send.send(event);
    });
    assert!(matches!(
        capture.start(
            CaptureTarget::Window(WindowId(u64::MAX)),
            None,
            60,
            sink.clone()
        ),
        Err(PlatformError::NotFound)
    ));
    let stream = capture
        .start(CaptureTarget::Window(id), None, 60, sink)
        .unwrap();
    let wait_frame = |size| {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                FrameEvent::Frame {
                    stream: event_stream,
                    frame,
                } => {
                    assert_eq!(event_stream, stream);
                    if frame.size == size {
                        break frame;
                    }
                }
                other => panic!("unexpected window capture event: {other:?}"),
            }
        }
    };
    let first = wait_frame(PixelSize::new(320, 240));
    let (first_pixels, _) = first.to_cpu().unwrap();
    let pixels = first_pixels.as_chunks::<4>().0;
    assert!(
        pixels.iter().any(|p| p != &pixels[0]),
        "window pixels are uniform"
    );
    ipc.dispatch(&format!(
        r#"hl.dsp.window.resize({{x=400,y=280,relative=false,window="address:{address}"}})"#
    ))
    .unwrap();
    wait_frame(PixelSize::new(400, 280));
    // The testapp stays static for more than a second: no input, redraw trigger or
    // IPC update is needed to keep a capture waiting for the one subsequent resize.
    let idle_deadline = Instant::now() + Duration::from_millis(1100);
    while let Ok(event) =
        events.recv_timeout(idle_deadline.saturating_duration_since(Instant::now()))
    {
        match event {
            FrameEvent::Frame { frame, .. } => assert_eq!(frame.size, PixelSize::new(400, 280)),
            other => panic!("unexpected idle window event: {other:?}"),
        }
    }
    ipc.dispatch(&format!(
        r#"hl.dsp.window.resize({{x=480,y=320,relative=false,window="address:{address}"}})"#
    ))
    .unwrap();
    wait_frame(PixelSize::new(480, 320));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { .. } => (),
            FrameEvent::Ended {
                stream: event_stream,
                reason,
            } => {
                assert_eq!(
                    (event_stream, reason),
                    (stream, StreamEndReason::TargetGone)
                );
                break;
            }
            other => panic!("unexpected window capture event: {other:?}"),
        }
    }
}
