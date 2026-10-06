//! Pure Windows M1 journal and serial operation model. No native queries or mutation.
//! Originals are durable before any mutation; uncertain identity/query failures retain evidence.

use super::{
    geometry::{self, DisplayIds, MonitorProbe},
    journal::{Original, Show},
};
use crosspane_platform::{Parked, ParkingKind, PlatformError};
use crosspane_types::{
    geom::{PixelRect, PixelSize, euclid::point2},
    id::WindowId,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT: &str = "crosspane-win-mirror-v1";
pub const COMMITTED_NAME: &str = "parking-mirror.journal";
pub const PENDING_NAME: &str = "parking-mirror.pending";
pub const MAX_BYTES: usize = 1024 * 1024;
pub const MAX_ENTRIES: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct MirrorJournalImages {
    pub committed: Option<Vec<u8>>,
    pub pending: Option<Vec<u8>>,
}
/// Commit publishes pending, then committed, with both files flushed before returning.
pub trait MirrorJournalStore: Send {
    fn read(&mut self) -> Result<MirrorJournalImages, PlatformError>;
    fn commit(&mut self, document: &[u8]) -> Result<(), PlatformError>;
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MirrorRecovery {
    pub restored: usize,
    pub retired: usize,
    pub pending: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeIdentity {
    pub hwnd: u64,
    pub pid: u32,
    pub tid: u32,
    pub process_created: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorEntry {
    pub identity: NativeIdentity,
    pub original: Original,
    pub visible_original: [i32; 4],
    pub may_have_mutated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    format: String,
    generation: u64,
    entries: Vec<MirrorEntry>,
}
fn invalid() -> PlatformError {
    PlatformError::Backend("invalid or ambiguous mirror journal; retained".into())
}
pub fn rect_size(rect: [i32; 4]) -> Result<PixelSize, PlatformError> {
    let w = i64::from(rect[2]) - i64::from(rect[0]);
    let h = i64::from(rect[3]) - i64::from(rect[1]);
    if w <= 0 || h <= 0 || w > i64::from(i32::MAX) || h > i64::from(i32::MAX) {
        return Err(invalid());
    }
    Ok(PixelSize::new(w as u32, h as u32))
}
fn validate_entry(e: &MirrorEntry) -> Result<(), PlatformError> {
    if e.identity.hwnd == 0
        || e.identity.pid == 0
        || e.identity.tid == 0
        || e.identity.process_created == 0
        || e.original.dpi == 0
        || e.original.monitor_path.is_empty()
        || e.original.monitor_path.len() > 4096
    {
        return Err(invalid());
    }
    rect_size(e.original.rect_physical)?;
    rect_size(e.visible_original)?;
    Ok(())
}
impl Journal {
    pub fn empty() -> Self {
        Self {
            format: FORMAT.into(),
            generation: 0,
            entries: Vec::new(),
        }
    }
    pub fn entries(&self) -> &[MirrorEntry] {
        &self.entries
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn bytes(&self) -> Result<Vec<u8>, PlatformError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| invalid())?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid());
        }
        Ok(bytes)
    }
    fn validate(&self) -> Result<(), PlatformError> {
        if self.format != FORMAT || self.entries.len() > MAX_ENTRIES {
            return Err(invalid());
        }
        let mut previous = None;
        for e in &self.entries {
            validate_entry(e)?;
            // At most one record per HWND; sorted canonical representation disallows duplicates.
            if previous.is_some_and(|hwnd| hwnd >= e.identity.hwnd) {
                return Err(invalid());
            }
            previous = Some(e.identity.hwnd);
        }
        Ok(())
    }
    fn parse(bytes: &[u8]) -> Result<Self, PlatformError> {
        if bytes.len() > MAX_BYTES {
            return Err(invalid());
        }
        let journal: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        journal.validate()?;
        // Fixed canonical documents make same-revision byte equality unambiguous.
        if journal.bytes()? != bytes {
            return Err(invalid());
        }
        Ok(journal)
    }
    /// Validate every present image before selecting any revision. Never partially load errors.
    pub fn load(images: &MirrorJournalImages) -> Result<(Self, bool), PlatformError> {
        let committed = images.committed.as_deref().map(Self::parse).transpose()?;
        let pending = images.pending.as_deref().map(Self::parse).transpose()?;
        match (committed, pending) {
            (None, None) => Ok((Self::empty(), false)),
            (Some(j), None) | (None, Some(j)) => Ok((j, true)),
            (Some(a), Some(b)) if a.generation == b.generation => {
                if a != b {
                    return Err(invalid());
                }
                Ok((a, false))
            }
            (Some(a), Some(b)) => {
                let (older, newer) = if a.generation < b.generation {
                    (a, b)
                } else {
                    (b, a)
                };
                if older.generation.checked_add(1) != Some(newer.generation) {
                    return Err(invalid());
                }
                for old in &older.entries {
                    if let Some(new) = newer.entries.iter().find(|e| e.identity == old.identity)
                        && (old.original != new.original
                            || old.visible_original != new.visible_original
                            || (old.may_have_mutated && !new.may_have_mutated))
                    {
                        return Err(invalid());
                    }
                }
                Ok((newer, true))
            }
        }
    }
    fn next(&self) -> Result<Self, PlatformError> {
        let mut next = self.clone();
        next.generation = next.generation.checked_add(1).ok_or_else(invalid)?;
        Ok(next)
    }
    pub fn insert(&self, entry: MirrorEntry) -> Result<Self, PlatformError> {
        validate_entry(&entry)?;
        if let Some(old) = self
            .entries
            .iter()
            .find(|e| e.identity.hwnd == entry.identity.hwnd)
        {
            if old.identity != entry.identity {
                return Err(PlatformError::NotFound);
            }
            return Ok(self.clone()); // Preserve the first original across repeated parks.
        }
        let mut next = self.next()?;
        next.entries.push(entry);
        next.entries.sort_by_key(|e| e.identity.hwnd);
        next.validate()?;
        Ok(next)
    }
    fn mark_mutation(&self, identity: NativeIdentity) -> Result<Self, PlatformError> {
        let mut next = self.next()?;
        next.entries
            .iter_mut()
            .find(|e| e.identity == identity)
            .ok_or(PlatformError::NotFound)?
            .may_have_mutated = true;
        Ok(next)
    }
    fn remove(&self, identity: NativeIdentity) -> Result<Self, PlatformError> {
        let mut next = self.next()?;
        next.entries.retain(|e| e.identity != identity);
        Ok(next)
    }
}

/// Fresh identity-checked physical native observation; no destination-DPI conversion.
#[derive(Clone, Debug)]
pub struct Observed {
    pub outer: [i32; 4],
    pub visible: [i32; 4],
    pub monitor_path: String,
    pub show: Show,
    pub dpi: u32,
    pub eligible: bool,
    pub fullscreen: bool,
    pub geometry: Option<Parked>,
}
pub fn actual_geometry(
    window: WindowId,
    visible: [i32; 4],
    name: &str,
    monitor_rect: [i32; 4],
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
    fullscreen: bool,
) -> Result<(Parked, String), PlatformError> {
    let mut matching = probes
        .iter()
        .filter(|p| !p.twin && p.name == name && p.rc_monitor == monitor_rect);
    let probe = matching.next().ok_or_else(invalid)?;
    if matching.next().is_some() {
        return Err(invalid());
    }
    let layout = geometry::displays(probes, ids).map_err(|_| invalid())?;
    let display = ids.assign(&probe.device_path).map_err(|_| invalid())?;
    if layout.displays.iter().filter(|d| d.id == display).count() != 1 {
        return Err(invalid());
    }
    let x = i64::from(visible[0]) - i64::from(monitor_rect[0]);
    let y = i64::from(visible[1]) - i64::from(monitor_rect[1]);
    let origin = point2(
        i32::try_from(x).map_err(|_| invalid())?,
        i32::try_from(y).map_err(|_| invalid())?,
    );
    let size = rect_size(visible)?;
    let maximum = point2(
        origin
            .x
            .checked_add(size.width as i32)
            .ok_or_else(invalid)?,
        origin
            .y
            .checked_add(size.height as i32)
            .ok_or_else(invalid)?,
    );
    Ok((
        Parked {
            window,
            kind: ParkingKind::Mirror,
            display,
            content: PixelRect::new(origin, maximum),
            fullscreen,
        },
        probe.device_path.clone(),
    ))
}
pub fn resized_outer(observed: &Observed, size: PixelSize) -> Result<[i32; 4], PlatformError> {
    validate_size_scale(size, 1.0)?;
    let old_visible = rect_size(observed.visible)?;
    let old_outer = rect_size(observed.outer)?;
    let width = i64::from(size.width) + i64::from(old_outer.width) - i64::from(old_visible.width);
    let height =
        i64::from(size.height) + i64::from(old_outer.height) - i64::from(old_visible.height);
    if width <= 0 || height <= 0 {
        return Err(invalid());
    }
    let right = i64::from(observed.outer[0]) + width;
    let bottom = i64::from(observed.outer[1]) + height;
    let rect = [
        observed.outer[0],
        observed.outer[1],
        i32::try_from(right).map_err(|_| invalid())?,
        i32::try_from(bottom).map_err(|_| invalid())?,
    ];
    rect_size(rect)?;
    Ok(rect)
}
pub fn validate_size_scale(size: PixelSize, scale: f64) -> Result<(), PlatformError> {
    if size.width == 0
        || size.height == 0
        || size.width > i32::MAX as u32
        || size.height > i32::MAX as u32
        || !scale.is_finite()
        || scale <= 0.0
    {
        return Err(invalid());
    }
    Ok(())
}

/// Exact static reason carried locally by the agent; it never changes the engine/wire Failure.
pub const PENDING_REPARK_REASON: &str =
    "This window is still waiting to be returned to a monitor that is no longer connected.";
#[derive(Clone, Debug)]
pub enum RestoreOutcome {
    Restored(Observed),
    MonitorGone,
}
#[derive(Clone, Copy)]
enum RestoreDisposition {
    Restored,
    Retired,
    MonitorGone,
}

/// Driver always checks cancellation/deadline, exact identity and secure context before mutation.
/// None means confirmed absent/reused. Unknown native errors must return Err, never None.
pub trait NativePort {
    fn check(&self) -> Result<(), PlatformError>;
    fn resolve(&mut self, id: WindowId) -> Result<NativeIdentity, PlatformError>;
    fn inspect(
        &mut self,
        identity: NativeIdentity,
        id: Option<WindowId>,
    ) -> Result<Option<Observed>, PlatformError>;
    fn resize(
        &mut self,
        identity: NativeIdentity,
        outer: [i32; 4],
        id: WindowId,
    ) -> Result<Observed, PlatformError>;
    fn restore(
        &mut self,
        entry: &MirrorEntry,
        id: Option<WindowId>,
    ) -> Result<RestoreOutcome, PlatformError>;
}
/// One owner invokes this controller serially. Store failure poisons all later work.
pub struct Controller<P> {
    pub port: P,
    store: Box<dyn MirrorJournalStore>,
    journal: Journal,
    bound: bool,
    fault: bool,
    runtime: BTreeMap<WindowId, NativeIdentity>,
    pending: BTreeSet<NativeIdentity>,
}
impl<P> std::fmt::Debug for Controller<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorController")
            .field("entries", &self.journal.entries.len())
            .field("bound", &self.bound)
            .field("fault", &self.fault)
            .finish_non_exhaustive()
    }
}
impl<P: NativePort> Controller<P> {
    pub fn new(mut store: Box<dyn MirrorJournalStore>, port: P) -> Result<Self, PlatformError> {
        let (journal, republish) = Journal::load(&store.read()?)?;
        if republish {
            store.commit(&journal.bytes()?)?;
        }
        Ok(Self {
            port,
            store,
            journal,
            bound: false,
            fault: false,
            runtime: BTreeMap::new(),
            pending: BTreeSet::new(),
        })
    }
    pub fn journal(&self) -> &Journal {
        &self.journal
    }
    /// Decorations must retire immediately when durable publication poisons this stream.
    pub fn faulted(&self) -> bool {
        self.fault
    }
    pub fn bind(&mut self) {
        self.bound = true;
    }
    fn check(&self, bound: bool) -> Result<(), PlatformError> {
        if self.fault {
            return Err(invalid());
        }
        if bound && !self.bound {
            return Err(PlatformError::Unsupported("unbound Windows mirror parking"));
        }
        self.port.check()
    }
    fn publish(&mut self, next: Journal) -> Result<(), PlatformError> {
        if next == self.journal {
            return Ok(());
        }
        self.port.check()?;
        if let Err(error) = self.store.commit(&next.bytes()?) {
            self.fault = true;
            return Err(error);
        }
        self.journal = next;
        self.port.check()
    }
    fn observed(
        &mut self,
        identity: NativeIdentity,
        id: Option<WindowId>,
    ) -> Result<Observed, PlatformError> {
        self.port
            .inspect(identity, id)?
            .ok_or(PlatformError::NotFound)
    }
    pub fn park(
        &mut self,
        id: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.check(true)?;
        validate_size_scale(size, scale)?;
        let identity = self.port.resolve(id)?;
        if self.pending.contains(&identity) {
            return Err(PlatformError::Unsupported(PENDING_REPARK_REASON));
        }
        let actual = self.observed(identity, Some(id))?;
        if !actual.eligible {
            return Err(PlatformError::Locked);
        }
        let entry = MirrorEntry {
            identity,
            original: Original {
                rect_physical: actual.outer,
                monitor_path: actual.monitor_path.clone(),
                show: actual.show,
                dpi: actual.dpi,
            },
            visible_original: actual.visible,
            may_have_mutated: false,
        };
        self.publish(self.journal.insert(entry)?)?;
        self.runtime.insert(id, identity);
        actual.geometry.ok_or(PlatformError::NotFound)
    }
    /// Native decoration follows a real park-in-place. Failed decoration must roll that park
    /// back through the same identity/journal/context checks as an ordinary restore.
    pub fn park_decorated(
        &mut self,
        id: WindowId,
        size: PixelSize,
        scale: f64,
        decorate: impl FnOnce(&mut P) -> Result<(), PlatformError>,
    ) -> Result<Parked, PlatformError> {
        let actual = self.park(id, size, scale)?;
        if let Err(marker_error) = decorate(&mut self.port) {
            return match self.restore(id) {
                Ok(()) => Err(marker_error),
                Err(restore_error) => Err(PlatformError::Backend(format!(
                    "mirror decoration failed: {marker_error}; rollback failed: {restore_error}; journal retained"
                ))),
            };
        }
        Ok(actual)
    }
    fn entry(&mut self, id: WindowId) -> Result<MirrorEntry, PlatformError> {
        let identity = self
            .runtime
            .get(&id)
            .copied()
            .ok_or(PlatformError::NotFound)?;
        if self.port.resolve(id)? != identity {
            return Err(PlatformError::NotFound);
        }
        self.journal
            .entries
            .iter()
            .find(|e| e.identity == identity)
            .cloned()
            .ok_or(PlatformError::NotFound)
    }
    pub fn geometry(&mut self, id: WindowId) -> Result<Parked, PlatformError> {
        self.check(true)?;
        let entry = self.entry(id)?;
        let observed = self.observed(entry.identity, Some(id))?;
        if !observed.eligible {
            return Err(PlatformError::Locked);
        }
        observed.geometry.ok_or(PlatformError::NotFound)
    }
    pub fn resize(
        &mut self,
        id: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.check(true)?;
        validate_size_scale(size, scale)?;
        let entry = self.entry(id)?;
        let actual = self.observed(entry.identity, Some(id))?;
        if !actual.eligible {
            return Err(PlatformError::Locked);
        }
        if actual.show == Show::Maximized || actual.fullscreen {
            if actual.geometry.is_some_and(|g| {
                g.content.width() == size.width as i32 && g.content.height() == size.height as i32
            }) {
                return actual.geometry.ok_or(PlatformError::NotFound);
            }
            return Err(PlatformError::Unsupported(
                "Windows mirror maximized/fullscreen resize",
            ));
        }
        let outer = resized_outer(&actual, size)?;
        self.publish(self.journal.mark_mutation(entry.identity)?)?;
        // The fresh resolver is checked after durable publication and immediately before native work.
        if self.port.resolve(id)? != entry.identity {
            return Err(PlatformError::NotFound);
        }
        self.port.check()?;
        let result = self.port.resize(entry.identity, outer, id)?;
        self.port.check()?;
        if self.port.resolve(id)? != entry.identity {
            return Err(PlatformError::NotFound);
        }
        if !result.eligible {
            return Err(PlatformError::Locked);
        }
        result.geometry.ok_or(PlatformError::NotFound) // Actual app constraints, never requested dimensions.
    }
    pub fn set_fullscreen(&mut self, id: WindowId, requested: bool) -> Result<(), PlatformError> {
        if self.geometry(id)?.fullscreen == requested {
            Ok(())
        } else {
            Err(PlatformError::Unsupported(
                "Windows mirror fullscreen transition",
            ))
        }
    }
    fn restore_entry(
        &mut self,
        entry: MirrorEntry,
        id: Option<WindowId>,
    ) -> Result<RestoreDisposition, PlatformError> {
        let Some(current) = self.port.inspect(entry.identity, id)? else {
            self.publish(self.journal.remove(entry.identity)?)?;
            self.pending.remove(&entry.identity);
            return Ok(RestoreDisposition::Retired);
        };
        if !current.eligible {
            return Err(PlatformError::Locked);
        }
        if current.show != entry.original.show {
            return Err(PlatformError::Unsupported(
                "Windows mirror show-state restoration",
            ));
        }
        if current.outer != entry.original.rect_physical
            || current.visible != entry.visible_original
            || current.show != entry.original.show
        {
            self.publish(self.journal.mark_mutation(entry.identity)?)?;
            self.port.check()?;
            let restored = match self.port.restore(&entry, id)? {
                RestoreOutcome::Restored(actual) => actual,
                RestoreOutcome::MonitorGone => {
                    self.pending.insert(entry.identity);
                    return Ok(RestoreDisposition::MonitorGone);
                }
            };
            if restored.outer != entry.original.rect_physical
                || restored.visible != entry.visible_original
                || restored.show != entry.original.show
            {
                return Err(PlatformError::Backend(
                    "mirror original restore not proven; retained".into(),
                ));
            }
        }
        self.publish(self.journal.remove(entry.identity)?)?;
        self.pending.remove(&entry.identity);
        Ok(RestoreDisposition::Restored)
    }
    pub fn restore(&mut self, id: WindowId) -> Result<(), PlatformError> {
        self.check(true)?;
        let Some(entry) = self
            .runtime
            .get(&id)
            .and_then(|identity| {
                self.journal
                    .entries
                    .iter()
                    .find(|e| e.identity == *identity)
            })
            .cloned()
        else {
            return Ok(());
        };
        if matches!(
            self.restore_entry(entry, Some(id))?,
            RestoreDisposition::MonitorGone
        ) {
            return Err(PlatformError::Unsupported(
                "Windows mirror original monitor unavailable; journal retained",
            ));
        }
        self.runtime.remove(&id);
        Ok(())
    }
    pub fn recover_startup(&mut self) -> Result<MirrorRecovery, PlatformError> {
        self.check(false)?;
        let mut report = MirrorRecovery::default();
        for entry in self.journal.entries.clone() {
            self.port.check()?;
            match self.restore_entry(entry, None)? {
                RestoreDisposition::Restored => report.restored += 1,
                RestoreDisposition::Retired => report.retired += 1,
                RestoreDisposition::MonitorGone => {}
            }
        }
        report.pending = self.journal.entries.len();
        Ok(report)
    }
    pub fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.check(true)?;
        let mut restored = Vec::new();
        for (id, identity) in self.runtime.clone() {
            let Some(entry) = self
                .journal
                .entries
                .iter()
                .find(|e| e.identity == identity)
                .cloned()
            else {
                continue;
            };
            match self.restore_entry(entry, Some(id))? {
                RestoreDisposition::Restored => restored.push(id),
                RestoreDisposition::Retired => {}
                RestoreDisposition::MonitorGone => {
                    return Err(PlatformError::Unsupported(
                        "Windows mirror original monitor unavailable; journal retained",
                    ));
                }
            }
            self.runtime.remove(&id);
        }
        Ok(restored)
    }
}

/// Pure M1 decoration facts. Native code supplies them only after exact source admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerObservation {
    pub identity: NativeIdentity,
    pub frame: [i32; 4],
    pub dpi: u32,
    pub visible: bool,
    pub minimized: bool,
    pub cloaked: bool,
    pub topmost: bool,
    pub session_allowed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerFrame {
    pub rect: [i32; 4],
    pub border: u32,
    /// Lead c6471971: match the admitted source band; never promote a non-topmost source.
    pub topmost: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkerState {
    #[default]
    Absent,
    Hidden,
    Shown(MarkerFrame),
}
/// Virtual-desktop queries are advisory only for this owned marker's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerDesktopMembership {
    Current,
    Other,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerDesktopGuid {
    Known,
    Error,
    Zero,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerDesktopPlacement {
    Follow,
    Keep,
    Hide,
}
/// W2.7b: an unguessable GUID never requires moving an owned marker already created here.
pub fn marker_desktop_placement(
    initial: bool,
    membership: MarkerDesktopMembership,
    guid: MarkerDesktopGuid,
    visible: bool,
    cloaked: bool,
) -> MarkerDesktopPlacement {
    use MarkerDesktopGuid::Known;
    use MarkerDesktopMembership::{Current, Other, Unknown};
    use MarkerDesktopPlacement::{Follow, Hide, Keep};
    if !visible || cloaked {
        return Hide;
    }
    match membership {
        Current if guid == Known => Follow,
        Current => Keep,
        Unknown if initial => Keep,
        Unknown | Other => Hide,
    }
}
/// Unknown own membership is acceptable only for an initially untouched marker kept here.
pub fn marker_desktop_ready(
    initial: bool,
    placement: MarkerDesktopPlacement,
    own: MarkerDesktopMembership,
) -> bool {
    placement != MarkerDesktopPlacement::Hide
        && match own {
            MarkerDesktopMembership::Current => true,
            MarkerDesktopMembership::Other => false,
            MarkerDesktopMembership::Unknown => {
                initial && placement == MarkerDesktopPlacement::Keep
            }
        }
}
/// One actual runtime park owns one decoration. Loss never changes the parking journal.
#[derive(Clone, Debug, Default)]
pub struct MarkerModel {
    identity: Option<NativeIdentity>,
    state: MarkerState,
    generation: u64,
    retry: bool,
}
impl MarkerModel {
    pub fn state(&self) -> MarkerState {
        self.state
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn park(&mut self, observed: MarkerObservation) -> Result<MarkerState, PlatformError> {
        if self.identity != Some(observed.identity) {
            self.generation = self.generation.checked_add(1).ok_or_else(invalid)?;
        }
        self.identity = Some(observed.identity);
        self.retry = true;
        self.observe(observed, true)
    }
    pub fn observe(
        &mut self,
        observed: MarkerObservation,
        changed: bool,
    ) -> Result<MarkerState, PlatformError> {
        let Some(identity) = self.identity else {
            return Ok(MarkerState::Absent);
        };
        if identity != observed.identity || !observed.session_allowed {
            self.remove();
            return Ok(self.state);
        }
        if changed {
            self.retry = true;
        }
        if !observed.visible || observed.minimized || observed.cloaked || !self.retry {
            self.state = MarkerState::Hidden;
            return Ok(self.state);
        }
        let size = rect_size(observed.frame)?;
        if observed.dpi == 0 {
            return Err(invalid());
        }
        // Two device-independent pixels, rounded upward at the source's DPI, inside its frame.
        let border = (u64::from(observed.dpi) * 2).div_ceil(96);
        let border = u32::try_from(border)
            .map_err(|_| invalid())?
            .max(1)
            .min(size.width.div_ceil(2))
            .min(size.height.div_ceil(2));
        self.state = MarkerState::Shown(MarkerFrame {
            rect: observed.frame,
            border,
            topmost: observed.topmost,
        });
        Ok(self.state)
    }
    /// A failed native adjacency proof stays hidden until a new admitted change event/fact.
    pub fn adjacency_failed(&mut self) {
        if self.identity.is_some() {
            self.state = MarkerState::Hidden;
            self.retry = false;
        }
    }
    pub fn remove(&mut self) {
        self.identity = None;
        self.state = MarkerState::Absent;
        self.retry = false;
    }
}
