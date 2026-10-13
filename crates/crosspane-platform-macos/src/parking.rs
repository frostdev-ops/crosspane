//! M1 parking leaves windows on their screen and journals their original frame before resizing.
//!
//! The journal's life cycle (04 §8 invariant 4, no window is lost). The decisions behind each rule
//! are in the private `decide` module, which has no native calls:
//! - A window's original frame is durable before the first native write to that window.
//! - A park that fails before it attempted any native write withdraws the entry it created: the
//!   window was never moved. Once a write was attempted (even one that errored or timed out, since
//!   AX may still have applied it) the entry stays, and so does any entry an earlier park made.
//! - Restore retires an entry only when its window is back at its original frame, provably already
//!   there, or gone. It never retires one it could not prove.
//! - `recover` tries every entry. One that cannot be settled stays journaled and never blocks the
//!   rest; the call then fails, naming how many remain.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::{
    PixelRect, PixelSize, PointDevice, PointLogical, RectLogical, SizeLogical, euclid,
};
use crosspane_types::id::{DisplayId, WindowId};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayRotation,
};

use crate::windows::{
    AxLookup, AxWindow, FULLSCREEN_WAIT, RawWindow, WindowQuery, bounds_equal, display_for_frame,
    ensure_fullscreen_with, fullscreen_pause, fullscreen_press_needed, quartz_fullscreen,
    require_accessibility, valid_frame,
};
use decide::{
    AxSight, RESTORE_SLACK, ResizeFailure, RestorePlan, frame_close, leave_fullscreen_first,
    plan_restore, recover_each, withdraw_after_failed_park,
};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Entry {
    pid: i32,
    frame: RectLogical,
}

/// M1 mirror parking. Native AX objects are local to each call, never stored in this Send handle.
#[derive(Debug)]
pub struct MacMirrorParking {
    commands: mpsc::SyncSender<Request>,
}

const CALL_WAIT: Duration = Duration::from_millis(1500);

#[derive(Debug)]
enum Command {
    Park(WindowId, PixelSize),
    Resize(WindowId, PixelSize),
    Geometry(WindowId),
    Fullscreen(WindowId, bool),
    Restore(WindowId),
    Recover,
}

#[derive(Debug)]
enum Reply {
    Geometry(Parked),
    Restored(Vec<WindowId>),
    Done,
}

type Request = (
    Command,
    Instant,
    mpsc::SyncSender<Result<Reply, PlatformError>>,
);

#[derive(Debug)]
struct ParkingState {
    journal: PathBuf,
    entries: BTreeMap<WindowId, Entry>,
    query: WindowQuery,
}

impl MacMirrorParking {
    pub fn new(journal: PathBuf) -> Result<MacMirrorParking, PlatformError> {
        let (commands, rx) = mpsc::sync_channel::<Request>(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        // Disk fsync and WindowServer calls have no cancellable Rust API. One serial worker
        // keeps command waits bounded and preserves journal-before-change ordering. Timed-out
        // commands never start a later AX mutation: every AX call checks the command deadline.
        std::thread::Builder::new()
            .name("mac-parking".into())
            .spawn(move || {
                let state = read_journal(&journal).and_then(|entries| {
                    Ok(ParkingState {
                        journal,
                        entries,
                        query: WindowQuery::new()?,
                    })
                });
                let mut state = match state {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                while let Ok((command, deadline, reply)) = rx.recv() {
                    let result = if Instant::now() >= deadline {
                        Err(PlatformError::Timeout)
                    } else {
                        match command {
                            Command::Park(window, size) => {
                                state.park(window, size, deadline).map(Reply::Geometry)
                            }
                            Command::Resize(window, size) => {
                                state.resize(window, size, deadline).map(Reply::Geometry)
                            }
                            Command::Geometry(window) => {
                                state.geometry(window, deadline).map(Reply::Geometry)
                            }
                            Command::Fullscreen(window, desired) => state
                                .set_fullscreen(window, desired, deadline)
                                .map(|_| Reply::Done),
                            Command::Restore(window) => {
                                state.restore_entry(window, deadline).map(|_| Reply::Done)
                            }
                            Command::Recover => state.recover(deadline).map(Reply::Restored),
                        }
                    };
                    let _ = reply.send(result);
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn parking worker: {e}")))?;
        initialized.recv_timeout(CALL_WAIT).map_err(wait_error)??;
        Ok(Self { commands })
    }

    fn request(&self, command: Command) -> Result<Reply, PlatformError> {
        let wait = if matches!(command, Command::Fullscreen(_, _)) {
            FULLSCREEN_WAIT
        } else {
            CALL_WAIT
        };
        let deadline = Instant::now() + wait;
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .try_send((command, deadline, tx))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => PlatformError::Timeout,
                mpsc::TrySendError::Disconnected(_) => {
                    PlatformError::Backend("parking worker stopped".into())
                }
            })?;
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(wait_error)?
    }

    fn geometry_reply(&self, command: Command) -> Result<Parked, PlatformError> {
        match self.request(command)? {
            Reply::Geometry(geometry) => Ok(geometry),
            _ => Err(PlatformError::Backend("unexpected parking reply".into())),
        }
    }
}

fn wait_error(error: mpsc::RecvTimeoutError) -> PlatformError {
    match error {
        mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
        mpsc::RecvTimeoutError::Disconnected => {
            PlatformError::Backend("parking worker stopped".into())
        }
    }
}

impl ParkingState {
    fn set_fullscreen(
        &self,
        window: WindowId,
        desired: bool,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        require_accessibility()?;
        let pid = self
            .entries
            .get(&window)
            .ok_or(PlatformError::NotFound)?
            .pid;
        ensure_fullscreen_with(
            desired,
            || {
                let raw = self.window(window, Some(pid), deadline)?;
                let fullscreen = quartz_fullscreen(&raw);
                Ok((raw, fullscreen))
            },
            |raw| match AxWindow::find(raw, deadline) {
                Ok(ax) => ax.press_fullscreen_button(|| {
                    self.fullscreen_press_needed_with(
                        window,
                        pid,
                        desired,
                        deadline,
                        quartz_fullscreen,
                    )
                }),
                Err(PlatformError::NotFound) => Ok(false),
                Err(error) => Err(error),
            },
            || fullscreen_pause(deadline),
        )
    }

    fn fullscreen_press_needed_with(
        &self,
        window: WindowId,
        pid: i32,
        desired: bool,
        deadline: Instant,
        classify: impl FnOnce(&RawWindow) -> bool,
    ) -> Result<bool, PlatformError> {
        let raw = self.window(window, Some(pid), deadline)?;
        fullscreen_press_needed(&raw, classify(&raw), desired, deadline)
    }

    fn window(
        &self,
        window: WindowId,
        pid: Option<i32>,
        deadline: Instant,
    ) -> Result<RawWindow, PlatformError> {
        self.query
            .list_until(true, deadline)?
            .into_iter()
            .find(|w| w.id == window && pid.is_none_or(|pid| w.pid == pid))
            .ok_or(PlatformError::NotFound)
    }

    /// Resize `raw`'s window. A failure says whether a native write may have reached the window:
    /// everything before the first AX write only reads, so `park` can then take its entry back.
    fn resize_window(
        &self,
        raw: &RawWindow,
        size: PixelSize,
        deadline: Instant,
    ) -> Result<Parked, ResizeFailure> {
        if quartz_fullscreen(raw) {
            return parked(raw.id, raw.frame).map_err(ResizeFailure::before_write);
        }
        let ax = AxWindow::find(raw, deadline).map_err(ResizeFailure::before_write)?;
        let frame = ax.frame().map_err(ResizeFailure::before_write)?;
        let display = display_for_frame(frame).map_err(ResizeFailure::before_write)?;
        let (_, scale) = display_metrics(display).map_err(ResizeFailure::before_write)?;
        // From here on a write is attempted. Every failure counts as one that may have moved the
        // window, even an error or timeout from the write itself: AX can apply a write and still
        // report failure, and the journal entry is all that brings the window back.
        // AXSize can move a constrained window. Explicitly retain its current top-left.
        ax.restore(RectLogical::new(
            frame.origin,
            SizeLogical::new(
                f64::from(size.width) / scale,
                f64::from(size.height) / scale,
            ),
        ))
        .map_err(ResizeFailure::from_write)?;
        let frame = ax.frame().map_err(ResizeFailure::from_write)?;
        parked(raw.id, frame).map_err(ResizeFailure::from_write)
    }

    fn remove_entry(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let mut entries = self.entries.clone();
        entries.remove(&window);
        write_journal(&self.journal, &entries)?;
        self.entries = entries;
        Ok(())
    }

    /// Take back the entry a failed park made for a window that was never touched. Best effort:
    /// `remove_entry` changes memory only after the journal is rewritten, so if that fails the
    /// entry stays in both places, and recovery retires it (the window is still at its original
    /// frame) instead of blocking anything.
    fn withdraw_unmoved(&mut self, window: WindowId) {
        if let Err(error) = self.remove_entry(window) {
            tracing::warn!(
                window = window.0,
                %error,
                "could not withdraw the journal entry of a window that was never moved; \
                 recovery retires it"
            );
        }
    }

    /// The Quartz window of a journal entry. `None` after retiring the entry because the window
    /// is gone (closed, or its id now belongs to another process): nothing is left to restore.
    fn window_or_retire(
        &mut self,
        window: WindowId,
        pid: i32,
        deadline: Instant,
    ) -> Result<Option<RawWindow>, PlatformError> {
        match self.window(window, Some(pid), deadline) {
            Ok(raw) => Ok(Some(raw)),
            Err(PlatformError::NotFound) => {
                self.remove_entry(window)?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Put the window of a journal entry back and retire the entry. `Ok(true)`: the window was
    /// moved back. `Ok(false)`: the entry was retired with nothing to move, because the window is
    /// gone or is already at its original frame ([`plan_restore`]). Any error leaves the entry
    /// journaled, and it is never removed before the window is known to be where it belongs.
    fn restore_entry(
        &mut self,
        window: WindowId,
        deadline: Instant,
    ) -> Result<bool, PlatformError> {
        let Some(entry) = self.entries.get(&window).copied() else {
            return Ok(false);
        };
        let Some(mut raw) = self.window_or_retire(window, entry.pid, deadline)? else {
            return Ok(false);
        };
        require_accessibility()?;
        // A park that failed while persisting its entry and then could not withdraw it leaves the
        // entry in memory only. Make it durable before any native write below, so a crash during
        // the restore still finds it.
        write_journal(&self.journal, &self.entries)?;
        if leave_fullscreen_first(
            quartz_fullscreen(&raw),
            bounds_equal(raw.frame, entry.frame),
        ) {
            self.set_fullscreen(window, false, deadline)?;
            let Some(fresh) = self.window_or_retire(window, entry.pid, deadline)? else {
                return Ok(false);
            };
            raw = fresh;
        }
        let quartz_at_original = bounds_equal(raw.frame, entry.frame);
        let ax = match AxWindow::find(&raw, deadline) {
            Ok(ax) => Some(ax),
            // AX has no element for this window (another Space, a window the app does not
            // expose). Whether that blocks the restore is `plan_restore`'s decision.
            Err(PlatformError::NotFound) => None,
            Err(error) => return Err(error),
        };
        let sight = match &ax {
            None => AxSight::Missing,
            Some(ax) => match ax.frame() {
                Ok(frame) if frame_close(frame, entry.frame, RESTORE_SLACK) => AxSight::AtOriginal,
                Ok(_) => AxSight::Elsewhere,
                // The element went away while we looked: the same as never having found it.
                Err(PlatformError::NotFound) => AxSight::Missing,
                Err(error) => return Err(error),
            },
        };
        match (plan_restore(quartz_at_original, sight), ax) {
            (RestorePlan::NothingToRestore, _) => {
                tracing::info!(
                    window = window.0,
                    "parked window is already at its original frame; journal entry retired"
                );
                self.remove_entry(window)?;
                Ok(false)
            }
            (RestorePlan::Move, Some(ax)) => {
                ax.restore(entry.frame)?;
                if !frame_close(ax.frame()?, entry.frame, RESTORE_SLACK) {
                    return Err(PlatformError::Backend(
                        "window refused its original frame; journal retained".into(),
                    ));
                }
                self.remove_entry(window)?;
                Ok(true)
            }
            // `Move` without an AX element cannot happen (the plan moves only what AX showed);
            // like `Keep` it leaves the entry alone.
            (RestorePlan::Move, None) | (RestorePlan::Keep, _) => Err(PlatformError::Backend(
                "window is not at its original frame and has no accessible AX element; \
                 journal retained"
                    .into(),
            )),
        }
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        deadline: Instant,
    ) -> Result<Parked, PlatformError> {
        check_size(size)?;
        require_accessibility()?;
        let raw = self.window(
            window,
            self.entries.get(&window).map(|entry| entry.pid),
            deadline,
        )?;
        // Only an entry this call creates may be withdrawn again. An older one belongs to a window
        // an earlier park may have moved, and a retry must never replace its original frame.
        let created = !self.entries.contains_key(&window);
        self.entries.entry(window).or_insert(Entry {
            pid: raw.pid,
            frame: raw.frame,
        });
        // Even a repeated park persists the original entry before another native change.
        if let Err(error) = write_journal(&self.journal, &self.entries) {
            // Nothing has been written to the window yet.
            if withdraw_after_failed_park(created, false) {
                self.withdraw_unmoved(window);
            }
            return Err(error);
        }
        match self.resize_window(&raw, size, deadline) {
            Ok(parked) => Ok(parked),
            Err(failure) => {
                if withdraw_after_failed_park(created, failure.may_have_moved) {
                    self.withdraw_unmoved(window);
                }
                Err(failure.error)
            }
        }
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        deadline: Instant,
    ) -> Result<Parked, PlatformError> {
        check_size(size)?;
        require_accessibility()?;
        let entry = self.entries.get(&window).ok_or(PlatformError::NotFound)?;
        let raw = self.window(window, Some(entry.pid), deadline)?;
        write_journal(&self.journal, &self.entries)?;
        // The entry of a parked window stays whatever the outcome.
        self.resize_window(&raw, size, deadline)
            .map_err(|failure| failure.error)
    }

    fn geometry(&self, window: WindowId, deadline: Instant) -> Result<Parked, PlatformError> {
        require_accessibility()?;
        let entry = self.entries.get(&window).ok_or(PlatformError::NotFound)?;
        let raw = self.window(window, Some(entry.pid), deadline)?;
        if quartz_fullscreen(&raw) {
            return parked(window, raw.frame);
        }
        let ax = AxWindow::find(&raw, deadline)?;
        parked(window, ax.frame()?)
    }

    /// Try every journal entry, in window-id order. An entry that cannot be settled stays
    /// journaled and the rest are still tried (04 §8 invariant 4: one stuck window never keeps
    /// another from being returned, at shutdown or at the next start). When any entry remains the
    /// result is an error naming how many, so the caller reports an incomplete recovery; the next
    /// start tries those again. The deadline is the command's: past it the entries not yet
    /// reached stay journaled too.
    fn recover(&mut self, deadline: Instant) -> Result<Vec<WindowId>, PlatformError> {
        let windows: Vec<WindowId> = self.entries.keys().copied().collect();
        let report = recover_each(
            windows,
            |window| {
                self.restore_entry(window, deadline).inspect_err(|error| {
                    tracing::warn!(
                        window = window.0,
                        %error,
                        "parked window not restored; its journal entry is kept"
                    );
                })
            },
            || Instant::now() >= deadline,
        );
        tracing::info!(
            restored = report.restored.len(),
            retired = report.retired,
            kept = report.kept,
            "parking journal recovery"
        );
        report.into_result()
    }
}

impl WindowParking for MacMirrorParking {
    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        match self.request(Command::Fullscreen(window, fullscreen))? {
            Reply::Done => Ok(()),
            _ => Err(PlatformError::Backend("unexpected parking reply".into())),
        }
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Command::Park(window, size))
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Command::Resize(window, size))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.geometry_reply(Command::Geometry(window))
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        match self.request(Command::Restore(window))? {
            Reply::Done => Ok(()),
            _ => Err(PlatformError::Backend("unexpected parking reply".into())),
        }
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        restore_and_place(
            || self.restore(window),
            || place_restored_window(window, display, origin, false),
        )
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        match self.request(Command::Recover)? {
            Reply::Restored(windows) => Ok(windows),
            _ => Err(PlatformError::Backend("unexpected parking reply".into())),
        }
    }
}

/// Public AX placement shared with twin parking. Restore/journal ownership stays with callers;
/// a placement refusal after successful restore never turns that restore into a failure.
pub(crate) fn restore_and_place(
    restore: impl FnOnce() -> Result<(), PlatformError>,
    place: impl FnOnce() -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    restore()?;
    if let Err(error) = place() {
        tracing::warn!(%error, "restored window placement refused; restore remains successful");
    }
    Ok(())
}

pub(crate) fn place_restored_window(
    window: WindowId,
    display: DisplayId,
    origin: PointDevice,
    twin: bool,
) -> Result<(), PlatformError> {
    let deadline = Instant::now() + CALL_WAIT;
    let (bounds, scale) = display_metrics(display)?;
    let visible = crate::displays::visible_frame(display)?;
    let query = frame_query()?;
    let raw = query
        .list_until(false, deadline)?
        .into_iter()
        .find(|raw| raw.id == window)
        .ok_or(PlatformError::NotFound)?;
    let ax = placement_lookup_with(
        twin,
        || AxWindow::find(&raw, deadline),
        || match AxWindow::lookup(&raw, deadline)? {
            AxLookup::Found(ax) => Ok(ax),
            AxLookup::Missing(_) => Err(PlatformError::NotFound),
        },
    )?;
    placement_with(
        raw.frame,
        bounds,
        visible,
        scale,
        origin,
        || ax.frame(),
        || {
            query.list_until(false, deadline).map(|windows| {
                windows
                    .iter()
                    .any(|now| now.id == raw.id && now.pid == raw.pid && now.on_screen)
            })
        },
        |frame| {
            if ax.position_guarded(frame.origin, &mut || {
                Instant::now() < deadline
                    && query.list_until(false, deadline).is_ok_and(|windows| {
                        windows
                            .iter()
                            .any(|now| now.id == raw.id && now.pid == raw.pid && now.on_screen)
                    })
            })? {
                Ok(())
            } else {
                Err(PlatformError::Timeout)
            }
        },
    )
}

fn placement_lookup_with<T>(
    twin: bool,
    mirror: impl FnOnce() -> Result<T, PlatformError>,
    conservative: impl FnOnce() -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    if twin { conservative() } else { mirror() }
}

#[allow(clippy::too_many_arguments)]
fn placement_with(
    _quartz: RectLogical,
    bounds: RectLogical,
    visible: RectLogical,
    scale: f64,
    origin: PointDevice,
    current_ax: impl FnOnce() -> Result<RectLogical, PlatformError>,
    still_visible: impl FnOnce() -> Result<bool, PlatformError>,
    place: impl FnOnce(RectLogical) -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    let frame = placement_frame(current_ax()?, bounds, visible, scale, origin)?;
    if !still_visible()? {
        return Err(PlatformError::NotFound);
    }
    place(frame)
}

/// Share one read worker across placement/tiling cleanup, including after a WindowServer read
/// times out. A late native read must not create a new blocked query thread on every gesture.
pub(crate) fn frame_query() -> Result<WindowQuery, PlatformError> {
    static QUERY: std::sync::Mutex<Option<WindowQuery>> = std::sync::Mutex::new(None);
    let mut query = QUERY
        .lock()
        .map_err(|_| PlatformError::Backend("frame query store poisoned".into()))?;
    if query.is_none() {
        *query = Some(WindowQuery::new()?);
    }
    query
        .as_ref()
        .cloned()
        .ok_or_else(|| PlatformError::Backend("frame query unavailable".into()))
}

pub(crate) fn set_window_frame(
    raw: &RawWindow,
    frame: RectLogical,
    deadline: Instant,
    mut allowed: impl FnMut() -> bool,
) -> Result<(), PlatformError> {
    if !valid_frame(frame) {
        return Err(PlatformError::Backend("invalid AX placement frame".into()));
    }
    let ax = match AxWindow::lookup_verified_frame(raw, deadline)? {
        AxLookup::Found(ax) => ax,
        AxLookup::Missing(_) => return Err(PlatformError::NotFound),
    };
    if !bounds_equal(ax.frame()?, raw.frame) {
        return Err(PlatformError::NotFound);
    }
    if ax.restore_guarded(frame, &mut allowed)? {
        Ok(())
    } else {
        Err(PlatformError::Timeout)
    }
}

fn placement_frame(
    frame: RectLogical,
    bounds: RectLogical,
    visible: RectLogical,
    scale: f64,
    origin: PointDevice,
) -> Result<RectLogical, PlatformError> {
    if !valid_frame(frame)
        || !valid_frame(bounds)
        || !valid_frame(visible)
        || !scale.is_finite()
        || scale <= 0.0
        || !origin.x.is_finite()
        || !origin.y.is_finite()
    {
        return Err(PlatformError::Backend(
            "invalid restore placement geometry".into(),
        ));
    }
    let x = bounds.min_x() + origin.x / scale;
    let y = bounds.min_y() + origin.y / scale;
    Ok(RectLogical::new(
        PointLogical::new(
            x.clamp(
                visible.min_x(),
                (visible.max_x() - frame.size.width).max(visible.min_x()),
            ),
            y.clamp(
                visible.min_y(),
                (visible.max_y() - frame.size.height).max(visible.min_y()),
            ),
        ),
        frame.size,
    ))
}

#[cfg(test)]
#[path = "../tests/drag/placement.rs"]
mod placement_tests;

fn check_size(size: PixelSize) -> Result<(), PlatformError> {
    if size.width == 0 || size.height == 0 {
        return Err(PlatformError::Backend(
            "window size must be positive".into(),
        ));
    }
    Ok(())
}

fn display_metrics(display: DisplayId) -> Result<(RectLogical, f64), PlatformError> {
    let bounds = CGDisplayBounds(display.0);
    let frame = RectLogical::new(
        PointLogical::new(bounds.origin.x, bounds.origin.y),
        SizeLogical::new(bounds.size.width, bounds.size.height),
    );
    let mode = CGDisplayCopyDisplayMode(display.0).ok_or(PlatformError::NotFound)?;
    let rotation = CGDisplayRotation(display.0).rem_euclid(360.0);
    let width = if rotation == 90.0 || rotation == 270.0 {
        CGDisplayMode::pixel_height(Some(&mode))
    } else {
        CGDisplayMode::pixel_width(Some(&mode))
    };
    let scale = width as f64 / frame.size.width;
    if !valid_frame(frame) || !scale.is_finite() || scale <= 0.0 {
        return Err(PlatformError::Backend(
            "invalid parking display metrics".into(),
        ));
    }
    Ok((frame, scale))
}

fn parked(window: WindowId, frame: RectLogical) -> Result<Parked, PlatformError> {
    let display = display_for_frame(frame)?;
    let (bounds, scale) = display_metrics(display)?;
    Ok(Parked {
        fullscreen: bounds_equal(frame, bounds),
        window,
        kind: ParkingKind::Mirror,
        display,
        content: content(frame, bounds.origin, scale)?,
    })
}

fn content(
    frame: RectLogical,
    origin: PointLogical,
    scale: f64,
) -> Result<PixelRect, PlatformError> {
    if !valid_frame(frame)
        || !origin.x.is_finite()
        || !origin.y.is_finite()
        || !scale.is_finite()
        || scale <= 0.0
    {
        return Err(PlatformError::Backend("invalid parking geometry".into()));
    }
    // Round the two edges outward, retaining every device pixel touched by a fractional frame.
    let edges = [
        ((frame.min_x() - origin.x) * scale).floor(),
        ((frame.min_y() - origin.y) * scale).floor(),
        ((frame.max_x() - origin.x) * scale).ceil(),
        ((frame.max_y() - origin.y) * scale).ceil(),
    ];
    if edges
        .iter()
        .any(|v| !v.is_finite() || *v < f64::from(i32::MIN) || *v > f64::from(i32::MAX))
    {
        return Err(PlatformError::Backend(
            "parking geometry exceeds pixel coordinates".into(),
        ));
    }
    Ok(PixelRect::new(
        euclid::Point2D::new(edges[0] as i32, edges[1] as i32),
        euclid::Point2D::new(edges[2] as i32, edges[3] as i32),
    ))
}

fn io_error(error: std::io::Error) -> PlatformError {
    PlatformError::Backend(format!("parking journal: {error}"))
}

fn read_journal(path: &Path) -> Result<BTreeMap<WindowId, Entry>, PlatformError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(io_error(error)),
    };
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_error)?;
    let invalid =
        || PlatformError::Backend("invalid parking journal; retained for inspection".into());
    let mut lines = text.lines();
    if lines.next() != Some("crosspane-mirror-v1") {
        return Err(invalid());
    }
    let mut entries = BTreeMap::new();
    for line in lines {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() != 6 {
            return Err(invalid());
        }
        let window = WindowId(parts[0].parse::<u64>().map_err(|_| invalid())?);
        let pid = parts[1].parse::<i32>().map_err(|_| invalid())?;
        let numbers: Vec<f64> = parts[2..]
            .iter()
            .map(|part| part.parse().map_err(|_| invalid()))
            .collect::<Result<_, _>>()?;
        let frame = RectLogical::new(
            PointLogical::new(numbers[0], numbers[1]),
            SizeLogical::new(numbers[2], numbers[3]),
        );
        if window.0 == 0
            || window.0 > u64::from(u32::MAX)
            || pid <= 0
            || !valid_frame(frame)
            || entries.insert(window, Entry { pid, frame }).is_some()
        {
            return Err(invalid());
        }
    }
    Ok(entries)
}

fn write_journal(path: &Path, entries: &BTreeMap<WindowId, Entry>) -> Result<(), PlatformError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| PlatformError::Backend("journal needs a file name".into()))?;
    let mut temporary_name = name.to_os_string();
    temporary_name.push(format!(".{}.pending", std::process::id()));
    let temporary = parent.join(temporary_name);
    // The journal's directory is supplied by the agent; never create or change other directories.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(io_error)?;
    let result = (|| {
        writeln!(file, "crosspane-mirror-v1").map_err(io_error)?;
        for (window, entry) in entries {
            writeln!(
                file,
                "{} {} {} {} {} {}",
                window.0,
                entry.pid,
                entry.frame.origin.x,
                entry.frame.origin.y,
                entry.frame.size.width,
                entry.frame.size.height
            )
            .map_err(io_error)?;
        }
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, path).map_err(io_error)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// What parking and recovery decide, with no native calls and no I/O, so each rule is tested
/// without WindowServer or AX. `ParkingState` makes the observations and carries the decisions
/// out.
///
/// What restore does with a journal entry (`E` is its original frame):
///
/// | Quartz window | at `E` (Quartz) | AX window | AX frame vs `E` | Decision |
/// |---|---|---|---|---|
/// | gone | | | | retire: nothing left to restore |
/// | present, native fullscreen | no | | | leave fullscreen first, then continue |
/// | present, native fullscreen | yes | | | stay fullscreen (leaving would move it off `E`), continue |
/// | present | any | found | within 2 pt | retire: already home, nothing to write |
/// | present | any | found | elsewhere | write `E`, verify within 2 pt, then retire; else keep |
/// | present | yes | missing | | retire: never moved, or already back, and AX has nothing to say |
/// | present | no | missing | | keep: cannot prove it home, cannot move it |
///
/// Reads that fail (Quartz, AX, journal) keep the entry. Only an entry whose window is proven
/// home, or gone, is ever removed.
mod decide {
    use crosspane_platform::PlatformError;
    use crosspane_types::geom::RectLogical;
    use crosspane_types::id::WindowId;

    /// How far an AX frame may be from the journaled original and still count as there, in
    /// points. It is the tolerance restore always verified its own write with.
    pub(super) const RESTORE_SLACK: f64 = 2.0;

    /// `a` and `b` agree on every edge to within `slack` points.
    pub(super) fn frame_close(a: RectLogical, b: RectLogical, slack: f64) -> bool {
        (a.origin.x - b.origin.x).abs() <= slack
            && (a.origin.y - b.origin.y).abs() <= slack
            && (a.size.width - b.size.width).abs() <= slack
            && (a.size.height - b.size.height).abs() <= slack
    }

    /// A failed resize and whether a native write may have reached the window.
    #[derive(Debug)]
    pub(super) struct ResizeFailure {
        pub(super) error: PlatformError,
        pub(super) may_have_moved: bool,
    }

    impl ResizeFailure {
        /// The failure came from a lookup or a read: the window cannot have been touched.
        pub(super) fn before_write(error: PlatformError) -> Self {
            Self {
                error,
                may_have_moved: false,
            }
        }

        /// The failure came from, or after, the first AX write. A write that errored or timed out
        /// may still have been applied, so the window may have moved.
        pub(super) fn from_write(error: PlatformError) -> Self {
            Self {
                error,
                may_have_moved: true,
            }
        }
    }

    /// Whether a failed park takes its journal entry back out: only an entry this very park
    /// created, and only when no native write may have reached the window. Otherwise the entry
    /// is what lets recovery put a moved window back, or belongs to an earlier park.
    pub(super) fn withdraw_after_failed_park(
        created_by_this_park: bool,
        may_have_moved: bool,
    ) -> bool {
        created_by_this_park && !may_have_moved
    }

    /// Restore leaves native fullscreen first, but not for a window already at its original
    /// frame: leaving fullscreen would move it away from where it belongs.
    pub(super) fn leave_fullscreen_first(fullscreen: bool, quartz_at_original: bool) -> bool {
        fullscreen && !quartz_at_original
    }

    /// What AX showed of the window being restored.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum AxSight {
        /// AX has no element for the window.
        Missing,
        /// An element whose frame is within [`RESTORE_SLACK`] of the original.
        AtOriginal,
        /// An element somewhere else.
        Elsewhere,
    }

    /// What restore does with the entry.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum RestorePlan {
        /// The window is home: retire the entry without writing to the window.
        NothingToRestore,
        /// Write the original frame through AX, verify, then retire the entry.
        Move,
        /// Neither provably home nor movable: keep the entry.
        Keep,
    }

    /// The restore decision for a window Quartz still lists. `quartz_at_original`: WindowServer's
    /// frame equals the journaled one (whole points, so a point of slack at most).
    pub(super) fn plan_restore(quartz_at_original: bool, ax: AxSight) -> RestorePlan {
        match ax {
            AxSight::AtOriginal => RestorePlan::NothingToRestore,
            AxSight::Elsewhere => RestorePlan::Move,
            AxSight::Missing if quartz_at_original => RestorePlan::NothingToRestore,
            AxSight::Missing => RestorePlan::Keep,
        }
    }

    /// What one recovery pass over the journal came to.
    #[derive(Debug, Default)]
    pub(super) struct Recovery {
        /// Windows moved back, in the order tried.
        pub(super) restored: Vec<WindowId>,
        /// Entries retired with nothing to move: window gone, or already home.
        pub(super) retired: usize,
        /// Entries still journaled: they failed, or the deadline came before they were tried.
        pub(super) kept: usize,
        first_error: Option<PlatformError>,
    }

    impl Recovery {
        fn keep(&mut self, error: PlatformError) {
            self.kept += 1;
            self.first_error.get_or_insert(error);
        }

        /// `Ok` with the windows moved back when nothing is left journaled. Otherwise an error
        /// that says how many entries remain and why the first one did.
        pub(super) fn into_result(self) -> Result<Vec<WindowId>, PlatformError> {
            match self.first_error {
                None => Ok(self.restored),
                Some(error) => Err(PlatformError::Backend(format!(
                    "{} parked window(s) not restored and still journaled \
                     ({} restored, {} retired); first error: {error}",
                    self.kept,
                    self.restored.len(),
                    self.retired
                ))),
            }
        }
    }

    /// Run `restore` over every window, whatever happens to the ones before it. `restore` returns
    /// `Ok(true)` for a window moved back, `Ok(false)` for an entry retired without a move, and
    /// an error for one that stays journaled. Once `expired` says so, the windows not yet tried
    /// stay journaled with a timeout.
    pub(super) fn recover_each(
        windows: impl IntoIterator<Item = WindowId>,
        mut restore: impl FnMut(WindowId) -> Result<bool, PlatformError>,
        mut expired: impl FnMut() -> bool,
    ) -> Recovery {
        let mut report = Recovery::default();
        for window in windows {
            if expired() {
                report.keep(PlatformError::Timeout);
                continue;
            }
            match restore(window) {
                Ok(true) => report.restored.push(window),
                Ok(false) => report.retired += 1,
                Err(error) => report.keep(error),
            }
        }
        report
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crosspane_types::geom::{PointLogical, SizeLogical};

        fn rect(x: f64, y: f64, width: f64, height: f64) -> RectLogical {
            RectLogical::new(PointLogical::new(x, y), SizeLogical::new(width, height))
        }

        fn backend(text: &str) -> PlatformError {
            PlatformError::Backend(text.into())
        }

        #[test]
        fn frame_close_checks_every_edge_against_the_slack() {
            let original = rect(100.0, 80.0, 800.0, 600.0);
            assert!(frame_close(original, original, 0.0));
            assert!(frame_close(
                rect(102.0, 78.0, 802.0, 598.0),
                original,
                RESTORE_SLACK
            ));
            for off in [
                rect(102.5, 80.0, 800.0, 600.0),
                rect(100.0, 77.5, 800.0, 600.0),
                rect(100.0, 80.0, 802.5, 600.0),
                rect(100.0, 80.0, 800.0, 597.5),
            ] {
                assert!(!frame_close(off, original, RESTORE_SLACK), "{off:?}");
            }
            assert!(!frame_close(
                rect(f64::NAN, 80.0, 800.0, 600.0),
                original,
                RESTORE_SLACK
            ));
        }

        #[test]
        fn a_failed_park_withdraws_only_an_entry_it_created_before_any_write() {
            // (created by this park, a write may have reached the window) -> withdraw.
            assert!(withdraw_after_failed_park(true, false));
            // A write may have moved it: the entry is what brings the window back.
            assert!(!withdraw_after_failed_park(true, true));
            // An older entry belongs to a window an earlier park may have moved.
            assert!(!withdraw_after_failed_park(false, false));
            assert!(!withdraw_after_failed_park(false, true));
        }

        #[test]
        fn resize_failures_say_whether_a_write_may_have_happened() {
            let read = ResizeFailure::before_write(PlatformError::NotFound);
            assert!(!read.may_have_moved);
            assert!(matches!(read.error, PlatformError::NotFound));
            let write = ResizeFailure::from_write(PlatformError::Timeout);
            assert!(write.may_have_moved);
            assert!(matches!(write.error, PlatformError::Timeout));
            // The two combine as park uses them: an AX lookup failure leaves the window alone, a
            // timed-out write does not.
            assert!(withdraw_after_failed_park(true, read.may_have_moved));
            assert!(!withdraw_after_failed_park(true, write.may_have_moved));
        }

        #[test]
        fn fullscreen_is_left_only_when_the_window_is_away_from_its_original_frame() {
            assert!(leave_fullscreen_first(true, false));
            assert!(!leave_fullscreen_first(true, true));
            assert!(!leave_fullscreen_first(false, false));
            assert!(!leave_fullscreen_first(false, true));
        }

        #[test]
        fn restore_plan_follows_the_rule_table() {
            use AxSight::{AtOriginal, Elsewhere, Missing};
            use RestorePlan::{Keep, Move, NothingToRestore};
            for (quartz_at_original, ax, plan) in [
                // AX found the window and it is home: nothing to write, whatever Quartz says.
                (true, AtOriginal, NothingToRestore),
                (false, AtOriginal, NothingToRestore),
                // AX found it elsewhere: put it back through AX.
                (true, Elsewhere, Move),
                (false, Elsewhere, Move),
                // AX cannot find it: Quartz at the original frame retires the entry, anything
                // else keeps it.
                (true, Missing, NothingToRestore),
                (false, Missing, Keep),
            ] {
                assert_eq!(
                    plan_restore(quartz_at_original, ax),
                    plan,
                    "quartz_at_original={quartz_at_original} ax={ax:?}"
                );
            }
        }

        #[test]
        fn recovery_goes_on_past_an_entry_that_fails() {
            let mut script = vec![
                (
                    WindowId(1),
                    Err(backend("window refused its original frame")),
                ),
                (WindowId(2), Ok(true)),
                (WindowId(3), Ok(false)),
                (WindowId(4), Err(PlatformError::Timeout)),
                (WindowId(5), Ok(true)),
            ];
            let mut tried = Vec::new();
            let report = recover_each(
                script.iter().map(|(window, _)| *window).collect::<Vec<_>>(),
                |window| {
                    tried.push(window);
                    script.remove(0).1
                },
                || false,
            );
            assert_eq!(tried, (1..=5).map(WindowId).collect::<Vec<_>>());
            assert_eq!(report.restored, vec![WindowId(2), WindowId(5)]);
            assert_eq!((report.retired, report.kept), (1, 2));
            let Err(PlatformError::Backend(text)) = report.into_result() else {
                panic!("two entries are still journaled");
            };
            assert!(
                text.starts_with("2 parked window(s) not restored"),
                "{text}"
            );
            assert!(text.contains("2 restored, 1 retired"), "{text}");
            // The first failure is the one reported.
            assert!(
                text.ends_with("window refused its original frame"),
                "{text}"
            );
        }

        #[test]
        fn recovery_that_settles_every_entry_returns_the_restored_windows() {
            let report = recover_each(
                [WindowId(7), WindowId(8), WindowId(9)],
                |window| Ok(window != WindowId(8)),
                || false,
            );
            assert_eq!((report.retired, report.kept), (1, 0));
            assert!(matches!(
                report.into_result().as_deref(),
                Ok([WindowId(7), WindowId(9)])
            ));
        }

        #[test]
        fn recovery_with_nothing_journaled_is_ok_and_empty() {
            let report = recover_each(Vec::new(), |_| Err(PlatformError::NotFound), || false);
            assert!(matches!(report.into_result().as_deref(), Ok([])));
        }

        #[test]
        fn recovery_past_the_deadline_keeps_what_it_did_not_reach() {
            let mut tried = Vec::new();
            let mut polls = 0;
            let report = recover_each(
                [WindowId(1), WindowId(2), WindowId(3)],
                |window| {
                    tried.push(window);
                    Ok(true)
                },
                || {
                    polls += 1;
                    polls > 1
                },
            );
            assert_eq!(tried, vec![WindowId(1)]);
            assert_eq!(report.restored, vec![WindowId(1)]);
            assert_eq!(report.kept, 2);
            let Err(PlatformError::Backend(text)) = report.into_result() else {
                panic!("two entries were never tried");
            };
            assert!(
                text.starts_with("2 parked window(s) not restored"),
                "{text}"
            );
            assert!(text.ends_with("timed out"), "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "private-vdisplay")]
    fn mirror_transition_needs_press(desired: bool, hidden: bool) -> Result<bool, PlatformError> {
        let bounds = RectLogical::new(PointLogical::zero(), SizeLogical::new(1800.0, 1169.0));
        let normal = RectLogical::new(
            PointLogical::new(50.0, 80.0),
            SizeLogical::new(800.0, 600.0),
        );
        let initial = RawWindow::fixture(42, 500, if desired { normal } else { bounds }, true);
        let changed = RawWindow::fixture(42, 500, if desired { bounds } else { normal }, !hidden);
        let state = ParkingState {
            journal: PathBuf::new(),
            entries: BTreeMap::new(),
            query: WindowQuery::scripted(vec![Ok(vec![initial]), Ok(vec![changed])]),
        };
        let deadline = Instant::now() + FULLSCREEN_WAIT;
        let observed = state.window(WindowId(42), Some(500), deadline)?;
        assert_eq!(
            observed.on_screen && bounds_equal(observed.frame, bounds),
            !desired
        );
        state.fullscreen_press_needed_with(WindowId(42), 500, desired, deadline, |raw| {
            raw.on_screen && bounds_equal(raw.frame, bounds)
        })
    }

    #[test]
    #[cfg(feature = "private-vdisplay")]
    fn mirror_fullscreen_entry_rechecks_after_button_lookup() {
        assert!(matches!(
            mirror_transition_needs_press(true, false),
            Ok(false)
        ));
    }

    #[test]
    #[cfg(feature = "private-vdisplay")]
    fn mirror_fullscreen_exit_rechecks_after_button_lookup() {
        assert!(matches!(
            mirror_transition_needs_press(false, false),
            Ok(false)
        ));
    }

    #[test]
    #[cfg(feature = "private-vdisplay")]
    fn mirror_fullscreen_press_refuses_a_newly_hidden_window() {
        for desired in [true, false] {
            assert!(matches!(
                mirror_transition_needs_press(desired, true),
                Err(PlatformError::Unsupported(_))
            ));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn journal_write_read_clear() {
        let directory = std::env::temp_dir().join(format!(
            "crosspane-parking-{}-{}",
            std::process::id(),
            crate::clock::now().as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("journal");
        assert!(read_journal(&path).unwrap().is_empty());
        let entry = Entry {
            pid: 123,
            frame: RectLogical::new(
                PointLogical::new(-100.5, 40.25),
                SizeLogical::new(400.0, 300.0),
            ),
        };
        let entries = BTreeMap::from([(WindowId(42), entry)]);
        write_journal(&path, &entries).unwrap();
        assert_eq!(read_journal(&path).unwrap(), entries);
        write_journal(&path, &BTreeMap::new()).unwrap();
        assert!(read_journal(&path).unwrap().is_empty());
        let mut parking = MacMirrorParking::new(path.clone()).unwrap();
        parking.restore(WindowId(42)).unwrap();
        assert!(parking.recover().unwrap().is_empty());
        assert!(
            parking
                .park(WindowId(42), PixelSize::new(0, 600), 1.0)
                .is_err()
        );
        drop(parking);
        fs::write(&path, "crosspane-mirror-v1\n42 123 NaN 0 400 300\n").unwrap();
        assert!(read_journal(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[allow(clippy::unwrap_used)]
    fn temp_journal(name: &str) -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "crosspane-parking-{name}-{}-{}",
            std::process::id(),
            crate::clock::now().as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("journal");
        (directory, path)
    }

    fn journaled_entry() -> Entry {
        Entry {
            pid: 500,
            frame: RectLogical::new(
                PointLogical::new(40.0, 60.0),
                SizeLogical::new(800.0, 600.0),
            ),
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn withdrawing_an_unmoved_entry_is_durable_and_best_effort() {
        let (directory, path) = temp_journal("withdraw");
        let entries = BTreeMap::from([
            (WindowId(1), journaled_entry()),
            (WindowId(2), journaled_entry()),
        ]);
        write_journal(&path, &entries).unwrap();
        let mut state = ParkingState {
            journal: path.clone(),
            entries,
            query: WindowQuery::new().unwrap(),
        };
        state.withdraw_unmoved(WindowId(2));
        assert_eq!(
            state.entries.keys().copied().collect::<Vec<_>>(),
            vec![WindowId(1)]
        );
        assert_eq!(read_journal(&path).unwrap(), state.entries);
        // A journal that cannot be rewritten: the entry stays, in memory and on disk alike, and
        // recovery retires it later. Nothing fails.
        fs::remove_dir_all(&directory).unwrap();
        state.withdraw_unmoved(WindowId(1));
        assert!(state.entries.contains_key(&WindowId(1)));
    }

    #[test]
    #[cfg(feature = "private-vdisplay")]
    #[allow(clippy::unwrap_used)]
    fn recovery_retires_gone_windows_and_goes_on_past_one_it_cannot_read() {
        let (directory, path) = temp_journal("recover");
        let entries = BTreeMap::from([
            (WindowId(10), journaled_entry()),
            (WindowId(20), journaled_entry()),
            (WindowId(30), journaled_entry()),
        ]);
        write_journal(&path, &entries).unwrap();
        let mut state = ParkingState {
            journal: path.clone(),
            entries,
            // One Quartz answer per entry, in window-id order: 10 is gone, the read for 20 times
            // out, 30 is gone. A gone window needs no AX, so none of this touches Accessibility.
            query: WindowQuery::scripted(vec![
                Ok(Vec::new()),
                Err(PlatformError::Timeout),
                Ok(Vec::new()),
            ]),
        };
        let result = state.recover(Instant::now() + Duration::from_secs(5));
        assert!(
            matches!(
                &result,
                Err(PlatformError::Backend(text))
                    if text.starts_with("1 parked window(s) not restored")
            ),
            "{result:?}"
        );
        let still_journaled = vec![WindowId(20)];
        assert_eq!(
            state.entries.keys().copied().collect::<Vec<_>>(),
            still_journaled
        );
        assert_eq!(
            read_journal(&path)
                .unwrap()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            still_journaled
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn frame_to_content_at_scale_two() {
        let frame = RectLogical::new(
            PointLogical::new(-80.0, 40.0),
            SizeLogical::new(400.0, 300.0),
        );
        let pixels = content(frame, PointLogical::new(-100.0, 10.0), 2.0).unwrap();
        assert_eq!(
            pixels,
            PixelRect::new(euclid::Point2D::new(40, 60), euclid::Point2D::new(840, 660))
        );
        assert!(content(frame, PointLogical::zero(), 0.0).is_err());
    }
}
