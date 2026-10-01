//! M1 parking leaves windows on their screen and journals their original frame before resizing.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::{PixelRect, PixelSize, PointLogical, RectLogical, SizeLogical, euclid};
use crosspane_types::id::{DisplayId, WindowId};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayRotation,
};

use crate::windows::{
    AxWindow, RawWindow, WindowQuery, display_for_frame, require_accessibility, valid_frame,
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
        let deadline = Instant::now() + CALL_WAIT;
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

    fn resize_window(
        &self,
        raw: &RawWindow,
        size: PixelSize,
        deadline: Instant,
    ) -> Result<Parked, PlatformError> {
        let ax = AxWindow::find(raw, deadline)?;
        let frame = ax.frame()?;
        let display = display_for_frame(frame)?;
        let (_, scale) = display_metrics(display)?;
        // AXSize can move a constrained window. Explicitly retain its current top-left.
        ax.restore(RectLogical::new(
            frame.origin,
            SizeLogical::new(
                f64::from(size.width) / scale,
                f64::from(size.height) / scale,
            ),
        ))?;
        parked(raw.id, ax.frame()?)
    }

    fn remove_entry(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let mut entries = self.entries.clone();
        entries.remove(&window);
        write_journal(&self.journal, &entries)?;
        self.entries = entries;
        Ok(())
    }

    fn restore_entry(
        &mut self,
        window: WindowId,
        deadline: Instant,
    ) -> Result<bool, PlatformError> {
        let Some(entry) = self.entries.get(&window).copied() else {
            return Ok(false);
        };
        let raw = match self.window(window, Some(entry.pid), deadline) {
            Ok(raw) => raw,
            Err(PlatformError::NotFound) => {
                self.remove_entry(window)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        require_accessibility()?;
        // A prior park may have failed while persisting its in-memory entry. Ensure that entry
        // is durable even when restore is the very next call after that failure.
        write_journal(&self.journal, &self.entries)?;
        let ax = AxWindow::find(&raw, deadline)?;
        ax.restore(entry.frame)?;
        let actual = ax.frame()?;
        if (actual.origin.x - entry.frame.origin.x).abs() > 2.0
            || (actual.origin.y - entry.frame.origin.y).abs() > 2.0
            || (actual.size.width - entry.frame.size.width).abs() > 2.0
            || (actual.size.height - entry.frame.size.height).abs() > 2.0
        {
            return Err(PlatformError::Backend(
                "window refused its original frame; journal retained".into(),
            ));
        }
        self.remove_entry(window)?;
        Ok(true)
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
        self.entries.entry(window).or_insert(Entry {
            pid: raw.pid,
            frame: raw.frame,
        });
        // Even a repeated park persists the original entry before another native change.
        // Retain it in memory on a write failure: a retry must never replace the original frame.
        write_journal(&self.journal, &self.entries)?;
        self.resize_window(&raw, size, deadline)
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
        self.resize_window(&raw, size, deadline)
    }

    fn geometry(&self, window: WindowId, deadline: Instant) -> Result<Parked, PlatformError> {
        require_accessibility()?;
        let entry = self.entries.get(&window).ok_or(PlatformError::NotFound)?;
        let raw = self.window(window, Some(entry.pid), deadline)?;
        let ax = AxWindow::find(&raw, deadline)?;
        parked(window, ax.frame()?)
    }

    fn recover(&mut self, deadline: Instant) -> Result<Vec<WindowId>, PlatformError> {
        let mut restored = Vec::new();
        for window in self.entries.keys().copied().collect::<Vec<_>>() {
            if self.restore_entry(window, deadline)? {
                restored.push(window);
            }
        }
        Ok(restored)
    }
}

impl WindowParking for MacMirrorParking {
    fn park(&mut self, window: WindowId, size: PixelSize) -> Result<Parked, PlatformError> {
        self.geometry_reply(Command::Park(window, size))
    }

    fn resize(&mut self, window: WindowId, size: PixelSize) -> Result<Parked, PlatformError> {
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

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        match self.request(Command::Recover)? {
            Reply::Restored(windows) => Ok(windows),
            _ => Err(PlatformError::Backend("unexpected parking reply".into())),
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(parking.park(WindowId(42), PixelSize::new(0, 600)).is_err());
        drop(parking);
        fs::write(&path, "crosspane-mirror-v1\n42 123 NaN 0 400 300\n").unwrap();
        assert!(read_journal(&path).is_err());
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
