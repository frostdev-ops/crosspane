//! M2 twin parking on GNOME (WP-G2.4 task B2): `ParkingKind::Twin`. A parked window is moved onto
//! the twin, a Mutter virtual monitor ([`GnomeTwin`]) that no physical screen shows, resized to
//! the destination's size, and captured from the twin's own stream.
//!
//! This is the M1 mirror parking's engine (`parking::Core`: the journal, bridge-loss reconnection,
//! `restore` and `recover`) with a different placement:
//!
//! - **park(size, scale)**: journal the window's original frame (first, as for the mirror) →
//!   `GnomeTwin::ensure` (creates or grows the twin; may be refused) → `MoveResize` to the twin's
//!   origin at `size / twin scale` logical → read back for up to 300 ms until the window reports
//!   that size → `Parked{kind: Twin, display: the twin's DisplayId, content, fullscreen: false}`.
//!   `content` is the window's frame on the twin in device pixels relative to the twin's origin
//!   (the twin's *effective* scale: device pixels over its logical width), clipped to the twin.
//!   **resize** is the same for a parked window (`NotFound` for one that is not). The destination's
//!   `scale` is used to choose the twin's scale; one twin has one scale, so a window parked later at
//!   another density is placed at the twin's.
//! - **Never** fullscreen (`set_fullscreen` is `Unsupported`), never minimize, never close.
//!   `MoveResize` ends maximize and fullscreen in the Shell and the bridge cannot put them back
//!   (as for the mirror): restore returns the window's rect, not its maximize state.
//! - **restore / restore_at**: exactly the mirror's: `MoveResize` back to the journaled rect (for
//!   `restore_at` then to the requested point, clamped into that physical display), retire the
//!   entry, then release the window from the twin, which starts the linger when it was the last.
//! - **recover** (agent start, before anything else): every journal entry of the bridge's current
//!   epoch is moved back to its journaled rect (the twin died with the previous agent and Mutter
//!   moved the window onto a physical monitor, wherever); entries of another epoch are retired
//!   with a warning, since their windows cannot be identified any more.
//! - **Loss.** When the twin is lost (the user stopped its sharing, the portal or PipeWire failed,
//!   or a growth failed), every window that was on it is restored at once, to its journaled rect,
//!   and the next park makes a new twin. A window on a lost twin is not moved onto a new one until
//!   it has been restored.
//! - **Failures.** `Unsupported` out of park means "mirror this window" (`TwinOrMirror`): no consent
//!   token, a failed layout sequence, a window too big for a virtual monitor, or a window that did
//!   not land on the twin. `Locked` is the closed gate: no mirror fallback while locked. After a
//!   failure past the journal write the window is put back at once, except for a timeout (the
//!   Shell may still carry out the request), where the entry stays for `recover`.
//!
//! # Journal
//!
//! `state_dir/gnome-twin.json`, the format, durability and crash points of `gnome-mirror.json`
//! (see `parking`): `{"version": 1, "entries": [{"epoch", "window", "x", "y", "width", "height",
//! "fullscreen"}]}`, written (temp file 0600, fsync, rename, directory fsync) **before** the first
//! change to a window, retired only after its restore succeeded or the window is gone. A crash
//! after the write and before `MoveResize` leaves a window at its journaled rect (`recover` finds
//! nothing to move); a crash after `MoveResize` leaves one on the twin that died with the agent,
//! which Mutter put on a physical monitor and `recover` puts back. An unreadable journal makes
//! [`GnomeTwinParking::new`] fail rather than being discarded.
//!
//! # Locking
//!
//! The engine sits behind one mutex, because the twin's loss listener (a different thread) needs
//! it. The lock order is parking, then the twin's state; the twin never calls a listener while it
//! holds its own locks.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{PixelRect, PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, WindowId};

use super::parking::{
    Core, Entry, Failure, Frame, Outcome, Placement, Shell, backend, check_size, logical_extent,
    or_not_found, to_device,
};
use super::shell::{ShellBridge, ShellWindow};
use super::twin::{GnomeTwin, TwinView, sizing};
use crate::portal::eis::DisplaysFn;

const NO_FULLSCREEN: &str = "fullscreen on a GNOME virtual monitor";
const NOT_ON_TWIN: &str = "the window is not on the virtual monitor";
const NO_TWIN: &str = "no virtual monitor support in this session";

/// Twin (M2) parking through the Shell bridge.
pub struct GnomeTwinParking {
    shared: Arc<Mutex<Engine<ShellBridge>>>,
}

impl fmt::Debug for GnomeTwinParking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GnomeTwinParking").finish_non_exhaustive()
    }
}

impl GnomeTwinParking {
    /// Load the journal at `journal` (a missing file is empty; the directory is created with mode
    /// 0700 at the first write). Call [`WindowParking::recover`] before parking anything.
    /// `displays` are the physical displays (they place a `restore_at`). The parking registers
    /// itself with `twin` for the restoring of windows when the twin is lost.
    ///
    /// Without a `twin` (an extension still at version 1, no Mutter DisplayConfig) the parking can
    /// still recover what a previous run left parked, which must not depend on today's
    /// capabilities; it refuses every park as `Unsupported`, so that the window is mirrored.
    pub fn new(
        bridge: ShellBridge,
        twin: Option<GnomeTwin>,
        displays: DisplaysFn,
        journal: PathBuf,
    ) -> Result<GnomeTwinParking, PlatformError> {
        let core = Core::open(bridge, Box::new(ShellBridge::connect), displays, journal)
            .map_err(|error| backend(format!("twin parking: {error}")))?;
        let shared = Arc::new(Mutex::new(Engine {
            core,
            twin: twin.clone(),
        }));
        if let Some(twin) = &twin {
            listen(&shared, twin);
        }
        Ok(GnomeTwinParking { shared })
    }

    fn engine(&self) -> MutexGuard<'_, Engine<ShellBridge>> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Registers the loss listener: the windows of a lost twin are restored from the journal.
fn listen<S: Shell + 'static>(shared: &Arc<Mutex<Engine<S>>>, twin: &GnomeTwin) {
    let weak = Arc::downgrade(shared);
    twin.on_lost(Arc::new(move |victims| {
        if let Some(shared) = weak.upgrade() {
            shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .restore_lost(victims);
        }
    }));
}

impl WindowParking for GnomeTwinParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.engine().park(window, size, scale)
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.engine().resize(window, size, scale)
    }

    fn set_fullscreen(
        &mut self,
        _window: WindowId,
        _fullscreen: bool,
    ) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(NO_FULLSCREEN))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.engine().geometry(window)
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.engine().restore(window, None)
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        self.engine()
            .restore(window, Some(Placement { display, origin }))
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.engine().recover()
    }
}

/// The parking logic, generic over the bridge so tests can drive it with a fake.
struct Engine<S: Shell> {
    core: Core<S>,
    twin: Option<GnomeTwin>,
}

impl<S: Shell> Engine<S> {
    /// `window` is no longer parked on the twin.
    fn release(&self, window: u64) {
        if let Some(twin) = &self.twin {
            twin.release(window);
        }
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.core.sync_epoch();
        let outcome = self.park_op(window, size, scale);
        self.core.sync_epoch();
        or_not_found(outcome)
    }

    fn park_op(&mut self, window: WindowId, size: PixelSize, scale: f64) -> Outcome<Parked> {
        if self.core.journal.contains(self.core.key(window)) {
            return self.resize_op(window, size, scale);
        }
        if self.twin.is_none() {
            return Err(PlatformError::Unsupported(NO_TWIN).into());
        }
        check_size(size)?;
        let current = self.core.window(window.0)?.ok_or(PlatformError::NotFound)?;
        let frame = Frame::of(&current);
        if !frame.is_valid() {
            return Err(backend("the window has no usable frame").into());
        }
        // A refusal that needs no change to the window comes before the journal write.
        if sizing::plan(size, None).is_none() {
            return Err(PlatformError::Unsupported(
                "the window is larger than a virtual monitor can be",
            )
            .into());
        }
        let entry = Entry::new(self.core.epoch(), window.0, frame, current.fullscreen);
        let next = self.core.journal.with(entry);
        self.core.commit(next)?;
        match self.place(window, size, scale) {
            Ok(parked) => Ok(parked),
            // A new epoch: the entry is of the old one and `sync_epoch` retires it.
            Err(Failure::Stale) => {
                self.release(window.0);
                Err(Failure::Stale)
            }
            Err(Failure::Error(PlatformError::Timeout)) => {
                // The Shell may still carry out the request it did not answer, so a read now
                // could show the old rect and a rollback could retire the entry too early. The
                // entry stays; if the window was not changed, `recover` finds nothing to undo.
                tracing::warn!(
                    window = window.0,
                    "twin park timed out; journal retained for recovery"
                );
                Err(Failure::Error(PlatformError::Timeout))
            }
            Err(Failure::Error(error)) => {
                // The window may or may not have changed. Put it back if it did; if that fails
                // too, the entry stays for `recover`.
                self.core.rollback(window.0);
                self.release(window.0);
                Err(Failure::Error(error))
            }
        }
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.core.sync_epoch();
        let outcome = self.resize_op(window, size, scale);
        self.core.sync_epoch();
        or_not_found(outcome)
    }

    fn resize_op(&mut self, window: WindowId, size: PixelSize, scale: f64) -> Outcome<Parked> {
        if !self.core.journal.contains(self.core.key(window)) {
            return Err(PlatformError::NotFound.into());
        }
        check_size(size)?;
        self.core.window(window.0)?.ok_or(PlatformError::NotFound)?;
        self.place(window, size, scale)
    }

    /// Makes room on the twin, moves the window to its origin at `size`, and reports what it took.
    fn place(&mut self, window: WindowId, size: PixelSize, scale: f64) -> Outcome<Parked> {
        let twin = self
            .twin
            .as_ref()
            .ok_or(PlatformError::Unsupported(NO_TWIN))?;
        let view = twin.ensure(window.0, size, scale)?;
        let parked = self.move_onto(window.0, size, &view)?;
        if view.changed {
            self.reassert_others(window.0, &view);
        }
        Ok(parked)
    }

    fn move_onto(&self, id: u64, size: PixelSize, view: &TwinView) -> Outcome<Parked> {
        let geometry = &view.display.geometry;
        let (width, height) = logical_extent(size, geometry.scale)?;
        let target = Frame {
            x: origin_of(view).0,
            y: origin_of(view).1,
            width,
            height,
        };
        // A fresh read: the twin's sequence took time.
        let current = self.core.window(id)?.ok_or(PlatformError::NotFound)?;
        if current.fullscreen || Frame::of(&current) != target {
            self.core.call(|shell| {
                shell.move_resize(id, target.x, target.y, target.width, target.height)
            })?;
        }
        let after = self.core.settle(id, target)?;
        Ok(parked_on(&after, view)?)
    }

    /// After the twin was created, grown or moved, the other windows parked on it are put back at
    /// its origin if the relayout moved them. Best effort: the failure of one is logged.
    fn reassert_others(&self, except: u64, view: &TwinView) {
        let Some(twin) = &self.twin else {
            return;
        };
        let others: Vec<u64> = twin
            .parked()
            .into_iter()
            .filter(|id| *id != except)
            .collect();
        if others.is_empty() {
            return;
        }
        let windows = match self.core.call(|shell| shell.list_windows()) {
            Ok(windows) => windows,
            Err(failure) => {
                tracing::warn!(?failure, "twin: windows on the twin were not checked");
                return;
            }
        };
        let (x, y) = origin_of(view);
        for id in others {
            let Some(window) = windows.iter().find(|w| w.id == id) else {
                continue;
            };
            if (window.x, window.y) == (x, y) || window.fullscreen {
                continue;
            }
            let (width, height) = (window.width, window.height);
            if let Err(failure) = self
                .core
                .call(|shell| shell.move_resize(id, x, y, width, height))
            {
                tracing::warn!(
                    ?failure,
                    window = id,
                    "twin: a window was not put back on the twin"
                );
            }
        }
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        or_not_found(self.geometry_op(window))
    }

    fn geometry_op(&self, window: WindowId) -> Outcome<Parked> {
        if !self.core.journal.contains(self.core.key(window)) {
            return Err(PlatformError::NotFound.into());
        }
        let current = self.core.window(window.0)?.ok_or(PlatformError::NotFound)?;
        let view = self
            .twin
            .as_ref()
            .and_then(GnomeTwin::view)
            .ok_or(PlatformError::NotFound)?;
        Ok(parked_on(&current, &view)?)
    }

    fn restore(
        &mut self,
        window: WindowId,
        placement: Option<Placement>,
    ) -> Result<(), PlatformError> {
        let result = self.core.restore_checked(window.0, placement);
        if result.is_ok() {
            self.release(window.0);
        }
        result
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let result = self.core.recover();
        if let Ok(restored) = &result {
            for window in restored {
                self.release(window.0);
            }
        }
        result
    }

    /// The twin is gone: put the windows that were on it back at once.
    fn restore_lost(&mut self, victims: &[u64]) {
        self.core.sync_epoch();
        for &id in victims {
            if !self.core.journal.contains(self.core.key(WindowId(id))) {
                // Already restored (the failed park rolled itself back).
                self.release(id);
                continue;
            }
            match self.restore(WindowId(id), None) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(window = id, %error, "window not restored after the virtual monitor was lost; journal retained");
                }
            }
        }
    }
}

/// The twin's logical origin as the integers the bridge takes.
fn origin_of(view: &TwinView) -> (i32, i32) {
    let origin = view.display.geometry.logical_origin;
    (origin.x.round() as i32, origin.y.round() as i32)
}

/// `Parked` for a window on the twin: its frame in the twin's device pixels, clipped to the twin.
fn parked_on(window: &ShellWindow, view: &TwinView) -> Result<Parked, PlatformError> {
    let geometry = &view.display.geometry;
    let content = to_device(Frame::of(window), geometry)?;
    let extent = PixelRect::new(
        point2(0, 0),
        point2(
            i32::try_from(geometry.pixel_size.width).map_err(|_| backend("twin too large"))?,
            i32::try_from(geometry.pixel_size.height).map_err(|_| backend("twin too large"))?,
        ),
    );
    let content = content
        .intersection(&extent)
        .filter(|clipped| clipped.width() > 0 && clipped.height() > 0)
        .ok_or(PlatformError::Unsupported(NOT_ON_TWIN))?;
    Ok(Parked {
        window: WindowId(window.id),
        kind: ParkingKind::Twin,
        display: view.display.id,
        content,
        fullscreen: false,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use crosspane_types::color::ColorSpace;
    use crosspane_types::display::DisplayInfo;
    use crosspane_types::geom::{DisplayGeometry, PointLogical, SizeMm};

    use super::super::parking::{Connect, Settle};
    use super::super::twin::testing::{Rig, wait_until};
    use super::*;
    use crate::wayland_outputs::display_id;

    // ---- fixtures ----

    const W: u64 = 7;
    const EPOCH: u64 = 41;
    const FAST: Settle = Settle {
        interval: Duration::from_millis(1),
        budget: Duration::from_millis(30),
    };

    fn frame(x: i32, y: i32, width: i32, height: i32) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    fn px(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    fn rect(min: (i32, i32), max: (i32, i32)) -> PixelRect {
        PixelRect::new(point2(min.0, min.1), point2(max.0, max.1))
    }

    fn shell_window(id: u64, f: Frame) -> ShellWindow {
        ShellWindow {
            id,
            app_id: "org.example.App.desktop".into(),
            title: "title".into(),
            pid: 100 + id as u32,
            x: f.x,
            y: f.y,
            width: f.width,
            height: f.height,
            focused: false,
            minimized: false,
            fullscreen: false,
        }
    }

    /// DP-3 as the physical displays the engine places a `restore_at` with.
    fn physical() -> Vec<DisplayInfo> {
        vec![DisplayInfo {
            id: display_id("DP-3"),
            name: "DP-3".into(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(800.0, 340.0),
                pixel_size: px(3440, 1440),
                scale: 1.0,
                logical_origin: PointLogical::new(0.0, 0.0),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }]
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "crosspane-gnome-twin-parking-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn journal(&self) -> PathBuf {
            self.0.join("gnome-twin.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// What the fake Shell holds. A `MoveResize` takes effect at once, ends fullscreen, and gives
    /// a window at least `min_size`.
    #[derive(Default)]
    struct ShellWorld {
        epoch: u64,
        windows: Vec<ShellWindow>,
        moves: Vec<(u64, Frame)>,
        min_size: (i32, i32),
        fail_move: Option<PlatformError>,
    }

    #[derive(Clone)]
    struct FakeShell {
        world: Arc<Mutex<ShellWorld>>,
    }

    impl FakeShell {
        fn new(epoch: u64, windows: Vec<ShellWindow>) -> FakeShell {
            FakeShell {
                world: Arc::new(Mutex::new(ShellWorld {
                    epoch,
                    windows,
                    ..ShellWorld::default()
                })),
            }
        }

        fn world(&self) -> MutexGuard<'_, ShellWorld> {
            self.world.lock().unwrap()
        }

        fn frame_of(&self, id: u64) -> Frame {
            let world = self.world();
            Frame::of(world.windows.iter().find(|w| w.id == id).unwrap())
        }

        fn set_frame(&self, id: u64, f: Frame) {
            let mut world = self.world();
            let w = world.windows.iter_mut().find(|w| w.id == id).unwrap();
            (w.x, w.y, w.width, w.height) = (f.x, f.y, f.width, f.height);
        }

        fn moves(&self) -> Vec<(u64, Frame)> {
            self.world().moves.clone()
        }
    }

    impl Shell for FakeShell {
        fn epoch(&self) -> u64 {
            self.world().epoch
        }

        fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
            Ok(self.world().windows.clone())
        }

        fn move_resize(
            &self,
            id: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> Result<(), PlatformError> {
            let mut world = self.world();
            if let Some(error) = world.fail_move.take() {
                return Err(error);
            }
            let min = world.min_size;
            world.moves.push((id, frame(x, y, width, height)));
            let Some(window) = world.windows.iter_mut().find(|w| w.id == id) else {
                return Err(PlatformError::NotFound);
            };
            window.x = x;
            window.y = y;
            window.width = width.max(min.0);
            window.height = height.max(min.1);
            window.fullscreen = false;
            Ok(())
        }
    }

    struct Harness {
        dir: TempDir,
        rig: Rig,
        shell: FakeShell,
        engine: Arc<Mutex<Engine<FakeShell>>>,
    }

    /// An engine over `shell`, `twin` and the journal, listening to the twin's loss.
    fn engine_over(
        shell: &FakeShell,
        twin: &GnomeTwin,
        journal: PathBuf,
    ) -> Result<Arc<Mutex<Engine<FakeShell>>>, PlatformError> {
        engine_with(shell, Some(twin), journal)
    }

    /// The same, with or without a twin.
    fn engine_with(
        shell: &FakeShell,
        twin: Option<&GnomeTwin>,
        journal: PathBuf,
    ) -> Result<Arc<Mutex<Engine<FakeShell>>>, PlatformError> {
        let world = Arc::clone(&shell.world);
        let connect: Connect<FakeShell> = Box::new(move || {
            Ok(FakeShell {
                world: Arc::clone(&world),
            })
        });
        let displays: DisplaysFn = {
            let displays = physical();
            Arc::new(move || displays.clone())
        };
        let mut core = Core::open(shell.clone(), connect, displays, journal)?;
        core.settle = FAST;
        core.reconnect_gap = Duration::ZERO;
        let shared = Arc::new(Mutex::new(Engine {
            core,
            twin: twin.cloned(),
        }));
        if let Some(twin) = twin {
            listen(&shared, twin);
        }
        Ok(shared)
    }

    impl Harness {
        fn new() -> Harness {
            Harness::with_windows(vec![shell_window(W, frame(100, 100, 800, 600))])
        }

        fn with_windows(windows: Vec<ShellWindow>) -> Harness {
            let dir = TempDir::new();
            let rig = Rig::new();
            let shell = FakeShell::new(EPOCH, windows);
            let engine = engine_over(&shell, &rig.twin, dir.journal()).unwrap();
            Harness {
                dir,
                rig,
                shell,
                engine,
            }
        }

        fn engine(&self) -> MutexGuard<'_, Engine<FakeShell>> {
            self.engine.lock().unwrap()
        }

        fn park(&self, id: u64, width: u32, height: u32) -> Result<Parked, PlatformError> {
            self.engine().park(WindowId(id), px(width, height), 2.0)
        }

        fn journal_text(&self) -> String {
            fs::read_to_string(self.dir.journal()).unwrap_or_default()
        }

        /// The journal file holds no entry.
        fn journal_is_empty(&self) -> bool {
            let text = self.journal_text();
            text.is_empty() || !text.contains("\"window\"")
        }

        /// A new twin and engine over the same Shell and journal, as after an agent restart (the
        /// old twin's screen closes with it).
        fn restart(&mut self) {
            let rig = Rig::new();
            self.engine = engine_over(&self.shell, &rig.twin, self.dir.journal()).unwrap();
            self.rig = rig;
        }
    }

    // ---- park ----

    #[test]
    fn park_moves_the_window_to_the_twin_origin_and_reports_twin_pixels() {
        let h = Harness::new();
        let parked = h.park(W, 1800, 1169).unwrap();
        // 1800x1169 device pixels at the twin's scale 2: 900 x 585 logical (584.5 rounds up).
        assert_eq!(h.shell.frame_of(W), frame(6440, 0, 900, 585));
        assert_eq!(parked.window, WindowId(W));
        assert_eq!(parked.kind, ParkingKind::Twin);
        assert_eq!(parked.display, display_id("Meta-0"));
        assert_eq!(parked.content, rect((0, 0), (1800, 1170)));
        assert!(!parked.fullscreen);
        assert_eq!(h.rig.twin.parked(), [W]);
    }

    #[test]
    fn the_journal_is_written_before_the_twin_is_touched() {
        let h = Harness::new();
        let path = h.dir.journal();
        h.rig.world.set_probe(move || {
            fs::read_to_string(&path)
                .is_ok_and(|text| text.contains("\"window\": 7") && text.contains("\"width\": 800"))
        });
        h.park(W, 1800, 1169).unwrap();
        let probed = h.rig.world.probed();
        // The first call of the sequence already sees the window's original frame on disk.
        assert_eq!(probed.first().unwrap().0, "save_layout 1");
        assert!(probed.iter().all(|(_, journaled)| *journaled), "{probed:?}");
        // And the journal still holds the original rect, not the twin's.
        let text = h.journal_text();
        assert!(
            text.contains("\"x\": 100") && text.contains("\"fullscreen\": false"),
            "{text}"
        );
    }

    #[test]
    fn a_second_window_is_stacked_at_the_origin_without_a_new_twin() {
        let h = Harness::with_windows(vec![
            shell_window(W, frame(100, 100, 800, 600)),
            shell_window(8, frame(300, 200, 640, 480)),
        ]);
        h.park(W, 1800, 1169).unwrap();
        let second = h.park(8, 1000, 800).unwrap();
        assert_eq!(h.shell.frame_of(8), frame(6440, 0, 500, 400));
        assert_eq!(second.content, rect((0, 0), (1000, 800)));
        assert_eq!(h.rig.world.screens_made(), 1);
        assert_eq!(h.rig.twin.parked(), [W, 8]);
    }

    #[test]
    fn growing_the_twin_puts_the_windows_already_on_it_back() {
        let h = Harness::with_windows(vec![
            shell_window(W, frame(100, 100, 800, 600)),
            shell_window(8, frame(300, 200, 640, 480)),
        ]);
        h.park(W, 1800, 1169).unwrap();
        // The relayout of the growth displaces the first window.
        h.shell.set_frame(W, frame(0, 0, 900, 585));
        let second = h.park(8, 3000, 1000).unwrap();
        assert_eq!(second.content, rect((0, 0), (3000, 1000)));
        assert_eq!(h.shell.frame_of(W), frame(6440, 0, 900, 585));
        // Moved when it was parked, and once more to undo the displacement.
        let moves = h.shell.moves();
        assert_eq!(moves.iter().filter(|(id, _)| *id == W).count(), 2);
    }

    #[test]
    fn resize_changes_the_content_and_regrows_the_twin_when_needed() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        let smaller = h.engine().resize(WindowId(W), px(1000, 700), 2.0).unwrap();
        assert_eq!(smaller.content, rect((0, 0), (1000, 700)));
        assert_eq!(h.shell.frame_of(W), frame(6440, 0, 500, 350));
        assert_eq!(h.rig.world.screens_made(), 1);
        let bigger = h.engine().resize(WindowId(W), px(2600, 1000), 2.0).unwrap();
        assert_eq!(bigger.content, rect((0, 0), (2600, 1000)));
        assert!(h.rig.twin.view().unwrap().display.geometry.pixel_size.width >= 2600);
        // A window that is not parked is `NotFound`.
        let other = h.engine().resize(WindowId(99), px(100, 100), 2.0);
        assert!(matches!(other, Err(PlatformError::NotFound)));
    }

    #[test]
    fn the_content_is_clipped_to_the_twin_when_the_app_refuses_the_size() {
        let h = Harness::new();
        h.shell.world().min_size = (3000, 100);
        let parked = h.park(W, 1800, 1169).unwrap();
        // The app took 3000 logical wide (6000 device pixels) on a 2304 pixel twin.
        assert_eq!(h.shell.frame_of(W).width, 3000);
        assert_eq!(parked.content, rect((0, 0), (2304, 1170)));
    }

    #[test]
    fn geometry_reads_the_window_again() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        h.shell.set_frame(W, frame(6440 + 10, 20, 500, 300));
        let parked = h.engine().geometry(WindowId(W)).unwrap();
        assert_eq!(parked.content, rect((20, 40), (1020, 640)));
        assert!(matches!(
            h.engine().geometry(WindowId(99)),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn a_window_that_is_not_on_the_twin_is_unsupported() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        h.shell.set_frame(W, frame(100, 100, 800, 600));
        assert!(matches!(
            h.engine().geometry(WindowId(W)),
            Err(PlatformError::Unsupported(_))
        ));
    }

    #[test]
    fn a_fullscreen_window_is_journaled_as_it_was_and_leaves_fullscreen_on_the_twin() {
        let mut full = shell_window(W, frame(0, 0, 3440, 1440));
        full.fullscreen = true;
        let h = Harness::with_windows(vec![full]);
        let parked = h.park(W, 1800, 1169).unwrap();
        assert!(!parked.fullscreen);
        assert!(h.journal_text().contains("\"fullscreen\": true"));
        // Restoring puts its rect back (the bridge cannot make it fullscreen again).
        h.engine().restore(WindowId(W), None).unwrap();
        assert_eq!(h.shell.frame_of(W), frame(0, 0, 3440, 1440));
        assert!(h.journal_is_empty());
    }

    // ---- failures ----

    #[test]
    fn a_layout_failure_puts_the_window_back_and_is_unsupported() {
        let h = Harness::new();
        h.rig
            .world
            .lock()
            .fail_apply
            .push_back(PlatformError::Backend(
            "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
                .into(),
        ));
        let error = h.park(W, 1800, 1169).unwrap_err();
        assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
        assert_eq!(h.shell.frame_of(W), frame(100, 100, 800, 600));
        assert!(h.journal_is_empty());
        assert!(h.rig.twin.parked().is_empty());
        assert!(h.rig.twin.view().is_none());
        // The window never moved at all.
        assert!(h.shell.moves().is_empty());
    }

    #[test]
    fn a_window_too_big_for_a_twin_is_refused_before_the_journal() {
        let h = Harness::new();
        let error = h.park(W, 9000, 100).unwrap_err();
        assert!(matches!(error, PlatformError::Unsupported(_)));
        assert!(h.journal_text().is_empty(), "no journal was written");
        assert!(h.shell.moves().is_empty());
        assert!(h.rig.world.log().is_empty());
    }

    #[test]
    fn a_closed_gate_is_locked_and_leaves_the_window_alone() {
        let h = Harness::new();
        h.rig.world.lock().fail_open = Some(PlatformError::Locked);
        assert!(matches!(h.park(W, 1800, 1169), Err(PlatformError::Locked)));
        assert_eq!(h.shell.frame_of(W), frame(100, 100, 800, 600));
        assert!(h.journal_is_empty());
        assert!(h.shell.moves().is_empty());
    }

    #[test]
    fn a_missing_consent_is_unsupported_so_that_the_window_is_mirrored() {
        let h = Harness::new();
        h.rig.world.lock().fail_open = Some(PlatformError::Unsupported(
            "no stored consent for a virtual screen",
        ));
        assert!(matches!(
            h.park(W, 1800, 1169),
            Err(PlatformError::Unsupported(
                "no stored consent for a virtual screen"
            ))
        ));
        assert!(h.journal_is_empty());
    }

    #[test]
    fn a_move_that_times_out_keeps_the_journal_for_recovery() {
        let h = Harness::new();
        h.shell.world().fail_move = Some(PlatformError::Timeout);
        assert!(matches!(h.park(W, 1800, 1169), Err(PlatformError::Timeout)));
        assert!(h.journal_text().contains("\"window\": 7"));
        // `recover` finds the window where it was and retires the entry.
        let restored = h.engine().recover().unwrap();
        assert_eq!(restored, [WindowId(W)]);
        assert!(h.journal_is_empty());
    }

    #[test]
    fn a_missing_window_is_not_found_without_a_journal() {
        let h = Harness::new();
        assert!(matches!(h.park(99, 800, 600), Err(PlatformError::NotFound)));
        assert!(h.journal_text().is_empty());
    }

    // ---- restore ----

    #[test]
    fn restore_moves_the_window_back_retires_the_entry_and_lets_the_twin_go() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        h.engine().restore(WindowId(W), None).unwrap();
        assert_eq!(h.shell.frame_of(W), frame(100, 100, 800, 600));
        assert!(h.journal_is_empty());
        assert!(h.rig.twin.parked().is_empty());
        wait_until("the twin to go", || h.rig.world.screens_dropped() == 1);
        // Idempotent.
        h.engine().restore(WindowId(W), None).unwrap();
    }

    #[test]
    fn restore_at_places_the_window_on_the_requested_physical_display() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        h.engine()
            .restore(
                WindowId(W),
                Some(Placement {
                    display: display_id("DP-3"),
                    origin: PointDevice::new(500.0, 300.0),
                }),
            )
            .unwrap();
        assert_eq!(h.shell.frame_of(W), frame(500, 300, 800, 600));
        assert!(h.journal_is_empty());
    }

    #[test]
    fn a_window_that_closed_while_parked_restores_as_gone() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        h.shell.world().windows.clear();
        h.engine().restore(WindowId(W), None).unwrap();
        assert!(h.journal_is_empty());
        assert!(h.rig.twin.parked().is_empty());
    }

    // ---- recover ----

    #[test]
    fn recover_puts_back_a_window_a_dead_agent_left_on_the_twin() {
        let mut h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        // The agent dies; the twin goes with its portal session and Mutter moves the window onto
        // a physical monitor, wherever.
        h.shell.set_frame(W, frame(40, 50, 900, 585));
        h.restart();
        let restored = h.engine().recover().unwrap();
        assert_eq!(restored, [WindowId(W)]);
        assert_eq!(h.shell.frame_of(W), frame(100, 100, 800, 600));
        assert!(h.journal_is_empty());
    }

    #[test]
    fn a_crash_between_the_journal_and_the_move_leaves_nothing_to_undo() {
        let dir = TempDir::new();
        let rig = Rig::new();
        let shell = FakeShell::new(EPOCH, vec![shell_window(W, frame(100, 100, 800, 600))]);
        fs::write(
            dir.journal(),
            format!(
                "{{\"version\": 1, \"entries\": [{{\"epoch\": {EPOCH}, \"window\": {W}, \
                 \"x\": 100, \"y\": 100, \"width\": 800, \"height\": 600, \"fullscreen\": false}}]}}"
            ),
        )
        .unwrap();
        let engine = engine_over(&shell, &rig.twin, dir.journal()).unwrap();
        let restored = engine.lock().unwrap().recover().unwrap();
        assert_eq!(restored, [WindowId(W)]);
        assert!(
            shell.moves().is_empty(),
            "the window was already at its rect"
        );
        assert!(
            !fs::read_to_string(dir.journal())
                .unwrap()
                .contains("\"window\"")
        );
    }

    #[test]
    fn entries_of_another_shell_epoch_are_retired_without_moving_anything() {
        let dir = TempDir::new();
        let rig = Rig::new();
        let shell = FakeShell::new(EPOCH + 1, vec![shell_window(W, frame(5, 5, 800, 600))]);
        fs::write(
            dir.journal(),
            format!(
                "{{\"version\": 1, \"entries\": [{{\"epoch\": {EPOCH}, \"window\": {W}, \
                 \"x\": 100, \"y\": 100, \"width\": 800, \"height\": 600, \"fullscreen\": false}}]}}"
            ),
        )
        .unwrap();
        let engine = engine_over(&shell, &rig.twin, dir.journal()).unwrap();
        let restored = engine.lock().unwrap().recover().unwrap();
        assert!(restored.is_empty());
        assert!(shell.moves().is_empty());
        assert_eq!(shell.frame_of(W), frame(5, 5, 800, 600));
        assert!(
            !fs::read_to_string(dir.journal())
                .unwrap()
                .contains("\"window\"")
        );
    }

    #[test]
    fn without_a_twin_recovery_still_runs_and_parks_are_refused_so_that_they_are_mirrored() {
        let dir = TempDir::new();
        let shell = FakeShell::new(
            EPOCH,
            vec![
                shell_window(W, frame(40, 50, 900, 585)),
                shell_window(8, frame(300, 200, 640, 480)),
            ],
        );
        fs::write(
            dir.journal(),
            format!(
                "{{\"version\": 1, \"entries\": [{{\"epoch\": {EPOCH}, \"window\": {W}, \
                 \"x\": 100, \"y\": 100, \"width\": 800, \"height\": 600, \"fullscreen\": false}}]}}"
            ),
        )
        .unwrap();
        let engine = engine_with(&shell, None, dir.journal()).unwrap();
        // What a previous run left parked is put back, whatever this run can do.
        let restored = engine.lock().unwrap().recover().unwrap();
        assert_eq!(restored, [WindowId(W)]);
        assert_eq!(shell.frame_of(W), frame(100, 100, 800, 600));
        // A new park is refused before anything is journaled or moved.
        let before = shell.moves().len();
        let refused = engine.lock().unwrap().park(WindowId(8), px(800, 600), 2.0);
        assert!(matches!(
            refused,
            Err(PlatformError::Unsupported(reason)) if reason == NO_TWIN
        ));
        assert_eq!(shell.moves().len(), before);
        assert!(
            !fs::read_to_string(dir.journal())
                .unwrap()
                .contains("\"window\"")
        );
        assert!(matches!(
            engine.lock().unwrap().geometry(WindowId(8)),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn an_unreadable_journal_is_an_error_not_a_blank_slate() {
        let dir = TempDir::new();
        let rig = Rig::new();
        let shell = FakeShell::new(EPOCH, Vec::new());
        fs::write(dir.journal(), "not json").unwrap();
        assert!(engine_over(&shell, &rig.twin, dir.journal()).is_err());
        fs::write(dir.journal(), "{\"version\": 2, \"entries\": []}").unwrap();
        assert!(engine_over(&shell, &rig.twin, dir.journal()).is_err());
    }

    // ---- loss ----

    #[test]
    fn a_lost_twin_restores_every_window_that_was_on_it_at_once() {
        let h = Harness::with_windows(vec![
            shell_window(W, frame(100, 100, 800, 600)),
            shell_window(8, frame(300, 200, 640, 480)),
        ]);
        h.park(W, 1800, 1169).unwrap();
        h.park(8, 1000, 800).unwrap();
        h.rig.world.lose_screen(0);
        wait_until("both windows to be restored", || {
            h.shell.frame_of(W) == frame(100, 100, 800, 600)
                && h.shell.frame_of(8) == frame(300, 200, 640, 480)
                && h.journal_is_empty()
        });
        assert!(h.rig.twin.view().is_none());
        // The listener has finished (it held the engine).
        drop(h.engine());
        // The next park makes a new twin.
        let parked = h.park(W, 1800, 1169).unwrap();
        assert_eq!(parked.kind, ParkingKind::Twin);
        assert_eq!(h.rig.world.screens_made(), 2);
    }

    #[test]
    fn a_failed_growth_gives_the_windows_back() {
        let h = Harness::with_windows(vec![
            shell_window(W, frame(100, 100, 800, 600)),
            shell_window(8, frame(300, 200, 640, 480)),
        ]);
        h.park(W, 1800, 1169).unwrap();
        h.rig
            .world
            .lock()
            .fail_apply
            .push_back(PlatformError::Backend(
            "DisplayConfig: org.freedesktop.DBus.Error.InvalidArgs: Logical monitors not adjacent"
                .into(),
        ));
        let error = h.park(8, 3000, 1000).unwrap_err();
        assert!(matches!(error, PlatformError::Unsupported(_)), "{error:?}");
        // Both windows are where they were before: the new one rolled back, the one on the twin
        // restored because the twin is gone.
        wait_until("the windows to be restored", || {
            h.shell.frame_of(W) == frame(100, 100, 800, 600)
                && h.shell.frame_of(8) == frame(300, 200, 640, 480)
                && h.journal_is_empty()
        });
        assert!(h.rig.twin.view().is_none());
    }

    #[test]
    fn a_window_on_a_lost_twin_is_not_resized_onto_a_new_one() {
        let h = Harness::new();
        h.park(W, 1800, 1169).unwrap();
        // The loss is known to the twin but its listener has not restored the window yet: hold
        // the engine so that the listener waits.
        let guard = h.engine();
        h.rig.world.lose_screen(0);
        wait_until("the twin to notice", || h.rig.twin.view().is_none());
        drop(guard);
        let resize = h.engine().resize(WindowId(W), px(1000, 800), 2.0);
        // Either the listener got there first (the window is not parked: `NotFound`) or the twin
        // refuses to move it; it is never silently moved onto a new twin.
        assert!(resize.is_err(), "{resize:?}");
        wait_until("the window to be restored", || {
            h.shell.frame_of(W) == frame(100, 100, 800, 600) && h.journal_is_empty()
        });
        assert_eq!(h.rig.world.screens_made(), 1);
    }
}
