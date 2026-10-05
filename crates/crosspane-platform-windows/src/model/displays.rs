//! Coherent monitor snapshots and the retained W0.2c allocator. No Win32 calls.

use super::geometry::{self, DisplayIds, MonitorProbe};
use crosspane_platform::PlatformError;
use crosspane_types::{display::DisplayInfo, id::DisplayId};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

/// An observed handle is transient; only the device path determines the retained ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeMonitor {
    pub handle: usize,
    pub probe: MonitorProbe,
}

/// All fields come from one coherent observation and allocator transaction.
#[derive(Clone, Debug)]
pub struct MonitorSnapshot {
    pub probes: Vec<MonitorProbe>,
    pub ids: DisplayIds,
    pub displays: Vec<DisplayInfo>,
    pub monitors: BTreeMap<DisplayId, usize>,
}

pub fn commit(
    native: Vec<NativeMonitor>,
    ids: &mut DisplayIds,
) -> Result<MonitorSnapshot, PlatformError> {
    let mut handles = BTreeSet::new();
    for monitor in &native {
        if monitor.handle == 0
            || !handles.insert(monitor.handle)
            || monitor.probe.device_path.is_empty()
        {
            return Err(PlatformError::Backend("ambiguous monitor identity".into()));
        }
    }
    let probes: Vec<_> = native.iter().map(|monitor| monitor.probe.clone()).collect();
    let mut next = ids.clone();
    let layout = geometry::displays(&probes, &mut next)
        .map_err(|_| PlatformError::Backend("invalid monitor geometry".into()))?;
    let monitors = native
        .iter()
        .map(|monitor| {
            next.assign(&monitor.probe.device_path)
                .map(|id| (id, monitor.handle))
        })
        .collect::<Result<_, _>>()
        .map_err(|_| PlatformError::Backend("monitor identity exhausted".into()))?;
    *ids = next.clone();
    Ok(MonitorSnapshot {
        probes,
        ids: next,
        displays: layout.displays,
        monitors,
    })
}

/// Native work must finish before the allocator mutex is acquired.
pub fn read_snapshot(
    read: &mut impl FnMut() -> Result<Vec<NativeMonitor>, PlatformError>,
    ids: &Mutex<DisplayIds>,
) -> Result<MonitorSnapshot, PlatformError> {
    let mut native = read()?;
    native.sort_by(|a, b| a.probe.device_path.cmp(&b.probe.device_path));
    for _ in 0..3 {
        let mut next = read()?;
        next.sort_by(|a, b| a.probe.device_path.cmp(&b.probe.device_path));
        if next == native {
            let mut ids = ids
                .lock()
                .map_err(|_| PlatformError::Backend("monitor allocator poisoned".into()))?;
            return commit(next, &mut ids);
        }
        native = next;
    }
    Err(PlatformError::Timeout)
}

/// CCD target fields joined to the observed GDI source, without invented identifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetPath {
    pub source_name: String,
    pub device_path: String,
    pub display_name: String,
    pub refresh_numerator: u32,
    pub refresh_denominator: u32,
    pub quarter_turns: u8,
}

pub fn join_paths(
    mut monitors: Vec<NativeMonitor>,
    paths: &[TargetPath],
) -> Result<Vec<NativeMonitor>, PlatformError> {
    if paths.len() != monitors.len() {
        return Err(PlatformError::Unsupported("ambiguous display topology"));
    }
    for monitor in &mut monitors {
        let mut matching = paths
            .iter()
            .filter(|path| path.source_name == monitor.probe.name);
        let path = matching.next().ok_or(PlatformError::NotFound)?;
        if matching.next().is_some() || path.device_path.is_empty() || path.quarter_turns > 3 {
            return Err(PlatformError::Unsupported("ambiguous display identity"));
        }
        let refresh = u64::from(path.refresh_numerator)
            .checked_mul(1000)
            .and_then(|n| n.checked_div(u64::from(path.refresh_denominator)))
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| PlatformError::Backend("invalid monitor refresh".into()))?;
        monitor.probe.device_path.clone_from(&path.device_path);
        // Frozen consumers match GetMonitorInfoW.szDevice, not the CCD friendly name.
        monitor.probe.refresh_millihz = refresh;
        monitor.probe.quarter_turns = path.quarter_turns;
    }
    Ok(monitors)
}

/// At most three snapshots per subscriber: initial, an unskippable loss barrier, latest.
/// A slow callback never holds the native owner or discards observation loss.
#[derive(Debug)]
pub struct Delivery {
    initial: Option<Vec<DisplayInfo>>,
    lost: bool,
    latest: Option<Vec<DisplayInfo>>,
}

impl Delivery {
    pub fn new(initial: Vec<DisplayInfo>) -> Self {
        Self {
            initial: Some(initial),
            lost: false,
            latest: None,
        }
    }

    pub fn publish(&mut self, displays: Vec<DisplayInfo>) {
        if displays.is_empty() {
            self.lost = true;
            self.latest = None;
        } else {
            self.latest = Some(displays);
        }
    }

    pub fn take(&mut self) -> Option<Vec<DisplayInfo>> {
        if let Some(initial) = self.initial.take() {
            Some(initial)
        } else if std::mem::take(&mut self.lost) {
            Some(Vec::new())
        } else {
            self.latest.take()
        }
    }
}
