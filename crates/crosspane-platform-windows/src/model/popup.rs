//! Numeric popup admission and root-local composition. No OS calls or window text.
//!
//! Native observations must corroborate these inputs before acquisition and delivery.
//! Generations describe observed lifetimes, not an atomic Win32 HWND lifetime token.

use super::window::Identity;
use crosspane_types::geom::{PixelRect, PixelSize};
use std::collections::{BTreeMap, BTreeSet};

/// Per active capture stream; there is no aggregate reservation across streams.
pub const SESSION_CAP: usize = 8;
pub const CANDIDATE_CAP: usize = 64;
pub const OWNER_CAP: usize = 8;
pub const EVENT_CAP: usize = 256;
pub const STACK_CAP: usize = 512;
pub const POPUP_BYTES: usize = 16 * 1024 * 1024;
/// Additional composition resources reserved independently by each active capture stream.
pub const WORKING_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub identity: Identity,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub bounds: PixelRect,
    pub content: PixelSize,
    pub dpi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Popup,
    Dialog,
    Ime,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub token: Token,
    /// Immediate owner first, ending with the exact root token.
    pub owners: Vec<Token>,
    pub geometry: Geometry,
    pub kind: Kind,
    pub visible: bool,
    pub topmost: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Refusal {
    #[default]
    None,
    Identity,
    Geometry,
    Cap,
    Budget,
    Stack,
    Events,
    Stale,
    Capture,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Identity => "identity",
            Self::Geometry => "geometry",
            Self::Cap => "cap",
            Self::Budget => "budget",
            Self::Stack => "stack",
            Self::Events => "events",
            Self::Stale => "stale",
            Self::Capture => "capture",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub root: Token,
    pub geometry: Geometry,
    /// Verified bottom-to-top order. Refused candidates never enter this vector.
    pub popups: Vec<Candidate>,
    pub refused: usize,
    pub reason: Refusal,
}

/// Retire on CREATE/DESTROY/HIDE, changed identity, or absence in a complete scan.
#[derive(Debug, Default)]
pub struct Generations {
    entries: BTreeMap<u64, Token>,
    next: u64,
}

impl Generations {
    pub fn observe(&mut self, identity: Identity) -> Result<Token, Refusal> {
        if !valid_identity(identity) {
            return Err(Refusal::Identity);
        }
        if let Some(token) = self.entries.get(&identity.hwnd)
            && token.identity == identity
        {
            return Ok(*token);
        }
        self.next = self.next.checked_add(1).ok_or(Refusal::Identity)?;
        let token = Token {
            identity,
            generation: self.next,
        };
        self.entries.insert(identity.hwnd, token);
        Ok(token)
    }

    pub fn retire(&mut self, hwnd: u64) {
        self.entries.remove(&hwnd);
    }

    pub fn retain(&mut self, observed: &BTreeSet<u64>) {
        self.entries.retain(|hwnd, _| observed.contains(hwnd));
    }
}

fn valid_identity(identity: Identity) -> bool {
    identity.hwnd != 0 && identity.pid != 0 && identity.tid != 0 && identity.process_created != 0
}

pub fn bytes(size: PixelSize) -> Result<usize, Refusal> {
    if size.width == 0 || size.height == 0 {
        return Err(Refusal::Geometry);
    }
    usize::try_from(size.width)
        .ok()
        .and_then(|w| w.checked_mul(usize::try_from(size.height).ok()?))
        .and_then(|n| n.checked_mul(4))
        .ok_or(Refusal::Budget)
}

pub fn geometry_valid(geometry: Geometry) -> bool {
    let width = i64::from(geometry.bounds.max.x) - i64::from(geometry.bounds.min.x);
    let height = i64::from(geometry.bounds.max.y) - i64::from(geometry.bounds.min.y);
    geometry.dpi != 0
        && width > 0
        && height > 0
        && width == i64::from(geometry.content.width)
        && height == i64::from(geometry.content.height)
        && width <= i64::from(i32::MAX)
        && height <= i64::from(i32::MAX)
}

pub fn admitted(root: Token, candidate: &Candidate) -> bool {
    if !valid_identity(root.identity)
        || root.generation == 0
        || !valid_identity(candidate.token.identity)
        || candidate.token.generation == 0
        || candidate.kind != Kind::Popup
        || !candidate.visible
        || !geometry_valid(candidate.geometry)
        || candidate.owners.is_empty()
        || candidate.owners.len() > OWNER_CAP
        || candidate.owners.last() != Some(&root)
    {
        return false;
    }
    let mut seen = BTreeSet::from([candidate.token.identity.hwnd]);
    for token in std::iter::once(&candidate.token).chain(&candidate.owners) {
        let identity = token.identity;
        if !valid_identity(identity)
            || token.generation == 0
            || identity.pid != root.identity.pid
            || identity.tid != root.identity.tid
            || identity.process_created != root.identity.process_created
        {
            return false;
        }
    }
    candidate
        .owners
        .iter()
        .all(|token| seen.insert(token.identity.hwnd))
}

/// Reservation includes three root auxiliary surfaces/buffers and five per popup.
/// The ordinary root capture output slots are existing resources, outside this budget.
pub fn select(
    root: Token,
    root_size: PixelSize,
    candidates: impl IntoIterator<Item = Candidate>,
) -> (Vec<Candidate>, usize, Refusal) {
    let mut selected = Vec::new();
    let mut refused = 0;
    let mut reason = Refusal::None;
    let mut used = match bytes(root_size).ok().and_then(|n| n.checked_mul(3)) {
        Some(n) if n <= WORKING_BYTES => n,
        _ => return (selected, 1, Refusal::Budget),
    };
    let mut seen = BTreeSet::new();
    for candidate in candidates {
        let refusal = if !admitted(root, &candidate) || !seen.insert(candidate.token.identity.hwnd)
        {
            Some(Refusal::Identity)
        } else if selected.len() == SESSION_CAP {
            Some(Refusal::Cap)
        } else {
            match bytes(candidate.geometry.content) {
                Ok(n) if n <= POPUP_BYTES => {
                    match n.checked_mul(5).and_then(|n| used.checked_add(n)) {
                        Some(total) if total <= WORKING_BYTES => {
                            used = total;
                            None
                        }
                        _ => Some(Refusal::Budget),
                    }
                }
                _ => Some(Refusal::Budget),
            }
        };
        if let Some(why) = refusal {
            refused += 1;
            reason = why;
        } else {
            selected.push(candidate);
        }
    }
    (selected, refused, reason)
}

/// Edges mean `(below, above)`, supplied by a bounded, freshly corroborated native walk.
/// Owner-before-owned and non-topmost-before-topmost are also established relations.
pub fn order(mut popups: Vec<Candidate>, edges: &[(u64, u64)]) -> Result<Vec<Candidate>, Refusal> {
    if popups.len() > SESSION_CAP || edges.len() > SESSION_CAP * SESSION_CAP {
        return Err(Refusal::Stack);
    }
    let n = popups.len();
    let mut relation = [[false; SESSION_CAP]; SESSION_CAP];
    for (a, lower) in popups.iter().enumerate() {
        for (b, upper) in popups.iter().enumerate() {
            relation[a][b] = a != b
                && (upper.owners.iter().any(|owner| owner == &lower.token)
                    || (!lower.topmost && upper.topmost)
                    || edges.contains(&(lower.token.identity.hwnd, upper.token.identity.hwnd)));
        }
    }
    for k in 0..n {
        for a in 0..n {
            for b in 0..n {
                relation[a][b] |= relation[a][k] && relation[k][b];
            }
        }
    }
    for a in 0..n {
        if relation[a][a] {
            return Err(Refusal::Stack);
        }
        for b in a + 1..n {
            if popups[a]
                .geometry
                .bounds
                .intersection(&popups[b].geometry.bounds)
                .is_some()
                && !relation[a][b]
                && !relation[b][a]
            {
                return Err(Refusal::Stack);
            }
        }
    }
    // Counts provide a topological rank; disjoint incomparable windows commute.
    let ranks: BTreeMap<_, _> = popups
        .iter()
        .enumerate()
        .map(|(b, candidate)| {
            (
                candidate.token.identity.hwnd,
                (0..n).filter(|a| relation[*a][b]).count(),
            )
        })
        .collect();
    popups.sort_by_key(|candidate| {
        (
            ranks[&candidate.token.identity.hwnd],
            candidate.token.generation,
        )
    });
    Ok(popups)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blit {
    pub source: PixelRect,
    pub destination: PixelRect,
}

pub fn clip(root: Geometry, popup: Geometry, crop: PixelRect) -> Result<Option<Blit>, Refusal> {
    if !geometry_valid(root)
        || !geometry_valid(popup)
        || crop.min.x < 0
        || crop.min.y < 0
        || crop.is_empty()
        || i64::from(crop.max.x) > i64::from(root.content.width)
        || i64::from(crop.max.y) > i64::from(root.content.height)
    {
        return Err(Refusal::Geometry);
    }
    let dx = i64::from(popup.bounds.min.x) - i64::from(root.bounds.min.x);
    let dy = i64::from(popup.bounds.min.y) - i64::from(root.bounds.min.y);
    let left = dx.max(i64::from(crop.min.x));
    let top = dy.max(i64::from(crop.min.y));
    let right = (dx + i64::from(popup.content.width)).min(i64::from(crop.max.x));
    let bottom = (dy + i64::from(popup.content.height)).min(i64::from(crop.max.y));
    if left >= right || top >= bottom {
        return Ok(None);
    }
    let rect = |x0, y0, x1, y1| -> Result<PixelRect, Refusal> {
        Ok(PixelRect::new(
            (
                i32::try_from(x0).map_err(|_| Refusal::Geometry)?,
                i32::try_from(y0).map_err(|_| Refusal::Geometry)?,
            )
                .into(),
            (
                i32::try_from(x1).map_err(|_| Refusal::Geometry)?,
                i32::try_from(y1).map_err(|_| Refusal::Geometry)?,
            )
                .into(),
        ))
    };
    Ok(Some(Blit {
        source: rect(left - dx, top - dy, right - dx, bottom - dy)?,
        destination: rect(
            left - i64::from(crop.min.x),
            top - i64::from(crop.min.y),
            right - i64::from(crop.min.x),
            bottom - i64::from(crop.min.y),
        )?,
    }))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Alpha {
    /// Fractional alpha convention is unqualified; this approximation remains `[U]`.
    #[default]
    Threshold128,
    /// Only enabled after the exact owned known-alpha fixture qualifies source bytes.
    Premultiplied,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlphaCounts {
    pub transparent: usize,
    pub opaque: usize,
    pub fractional: usize,
}

#[derive(Debug)]
pub struct Layer<'a> {
    pub pixels: &'a [u8],
    pub size: PixelSize,
    pub blit: Blit,
}

/// Every rebuild begins with the pristine root, including a topology-only dismissal.
/// Borrowed older outputs never become an input or a writable destination.
pub fn rebuild(
    baseline: &[u8],
    output: PixelSize,
    layers: &[Layer<'_>],
    alpha: Alpha,
) -> Result<(zeroize::Zeroizing<Vec<u8>>, Vec<AlphaCounts>), Refusal> {
    if baseline.len() != bytes(output)?
        || baseline.len() > WORKING_BYTES
        || layers.len() > SESSION_CAP
    {
        return Err(Refusal::Budget);
    }
    let mut pixels = zeroize::Zeroizing::new(baseline.to_vec());
    let mut counts = Vec::with_capacity(layers.len());
    for layer in layers {
        counts.push(composite(
            &mut pixels,
            output,
            layer.pixels,
            layer.size,
            layer.blit,
            alpha,
        )?);
    }
    Ok((pixels, counts))
}

pub fn composite(
    destination: &mut [u8],
    output: PixelSize,
    source: &[u8],
    source_size: PixelSize,
    blit: Blit,
    alpha: Alpha,
) -> Result<AlphaCounts, Refusal> {
    let in_bounds = |r: PixelRect, size: PixelSize| {
        r.min.x >= 0
            && r.min.y >= 0
            && !r.is_empty()
            && i64::from(r.max.x) <= i64::from(size.width)
            && i64::from(r.max.y) <= i64::from(size.height)
    };
    if destination.len() != bytes(output)?
        || source.len() != bytes(source_size)?
        || !in_bounds(blit.source, source_size)
        || !in_bounds(blit.destination, output)
        || blit.source.width() != blit.destination.width()
        || blit.source.height() != blit.destination.height()
    {
        return Err(Refusal::Geometry);
    }
    let mut counts = AlphaCounts::default();
    for y in 0..blit.source.height() as usize {
        for x in 0..blit.source.width() as usize {
            let from = ((blit.source.min.y as usize + y) * source_size.width as usize
                + blit.source.min.x as usize
                + x)
                * 4;
            let to = ((blit.destination.min.y as usize + y) * output.width as usize
                + blit.destination.min.x as usize
                + x)
                * 4;
            let a = source[from + 3];
            match a {
                0 => counts.transparent += 1,
                255 => {
                    counts.opaque += 1;
                    destination[to..to + 4].copy_from_slice(&source[from..from + 4]);
                }
                _ => {
                    counts.fractional += 1;
                    match alpha {
                        Alpha::Threshold128 if a >= 128 => {
                            destination[to..to + 3].copy_from_slice(&source[from..from + 3]);
                            destination[to + 3] = 255;
                        }
                        Alpha::Threshold128 => {}
                        Alpha::Premultiplied => {
                            for c in 0..3 {
                                destination[to + c] = (u16::from(source[from + c])
                                    + (u16::from(destination[to + c]) * u16::from(255 - a) + 127)
                                        / 255)
                                    .min(255)
                                    as u8;
                            }
                            destination[to + 3] = 255;
                        }
                    }
                }
            }
        }
    }
    Ok(counts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn token(hwnd: u64) -> Token {
        Token {
            identity: Identity {
                hwnd,
                pid: 7,
                tid: 8,
                process_created: 9,
            },
            generation: hwnd,
        }
    }
    fn geometry(x: i32, y: i32, w: i32, h: i32, dpi: u32) -> Geometry {
        Geometry {
            bounds: PixelRect::new((x, y).into(), (x + w, y + h).into()),
            content: PixelSize::new(w as u32, h as u32),
            dpi,
        }
    }
    fn candidate(hwnd: u64) -> Candidate {
        Candidate {
            token: token(hwnd),
            owners: vec![token(1)],
            geometry: geometry(20, 20, 10, 10, 96),
            kind: Kind::Popup,
            visible: true,
            topmost: false,
        }
    }
    #[test]
    fn only_exact_pid_thread_creation_and_root_owner_are_admitted() {
        assert!(admitted(token(1), &candidate(2)));
        for changed in 0..4 {
            let mut p = candidate(2);
            match changed {
                0 => p.token.identity.pid += 1,
                1 => p.token.identity.tid += 1,
                2 => p.token.identity.process_created += 1,
                _ => p.owners[0].generation += 1,
            }
            assert!(!admitted(token(1), &p));
        }
    }
    #[test]
    fn foreign_intermediate_missing_cycle_and_depth_are_refused() {
        let mut p = candidate(2);
        p.owners.insert(0, token(3));
        assert!(admitted(token(1), &p));
        p.owners[0].identity.pid += 1;
        assert!(!admitted(token(1), &p));
        p.owners.clear();
        assert!(!admitted(token(1), &p));
        p.owners = vec![token(2), token(1)];
        assert!(!admitted(token(1), &p));
        p.owners = (3..12).map(token).chain([token(1)]).collect();
        assert!(!admitted(token(1), &p));
    }
    #[test]
    fn dialogs_ime_hidden_and_class_hints_never_authorize_popup_capture() {
        for kind in [Kind::Dialog, Kind::Ime, Kind::Other] {
            let mut p = candidate(2);
            p.kind = kind;
            assert!(!admitted(token(1), &p));
        }
        let mut p = candidate(2);
        p.visible = false;
        assert!(!admitted(token(1), &p));
    }
    #[test]
    fn generations_retire_on_hide_destroy_and_changed_identity() {
        let mut g = Generations::default();
        let first = g.observe(token(2).identity).unwrap();
        assert_eq!(g.observe(token(2).identity).unwrap(), first);
        g.retire(2);
        let second = g.observe(token(2).identity).unwrap();
        assert_ne!(first, second);
        let mut identity = token(2).identity;
        identity.tid += 1;
        assert_ne!(g.observe(identity).unwrap(), second);
        g.retain(&BTreeSet::new());
        assert_ne!(g.observe(identity).unwrap(), second);
    }
    #[test]
    fn cap_and_checked_resource_budget_refuse_extra_without_removing_root() {
        let (p, n, why) = select(token(1), PixelSize::new(100, 100), (2..12).map(candidate));
        assert_eq!(p.len(), SESSION_CAP);
        assert_eq!(n, 2);
        assert_eq!(why, Refusal::Cap);
        assert_eq!(
            select(token(1), PixelSize::new(u32::MAX, u32::MAX), [candidate(2)]).2,
            Refusal::Budget
        );
        let mut huge = candidate(2);
        huge.geometry = geometry(0, 0, 4096, 4096, 96);
        assert_eq!(
            select(token(1), PixelSize::new(100, 100), [huge]).2,
            Refusal::Budget
        );
    }
    #[test]
    fn physical_negative_origin_mixed_dpi_and_crop_have_no_double_scale() {
        let root = geometry(-100, -50, 100, 100, 144);
        let p = geometry(-120, -20, 80, 90, 192);
        let clip = clip(root, p, PixelRect::new((10, 20).into(), (80, 90).into()))
            .unwrap()
            .unwrap();
        assert_eq!(clip.source, PixelRect::new((30, 0).into(), (80, 60).into()));
        assert_eq!(
            clip.destination,
            PixelRect::new((0, 10).into(), (50, 70).into())
        );
    }
    #[test]
    fn all_overhang_edges_clip_and_nonintersecting_popup_is_absent() {
        let root = geometry(0, 0, 100, 100, 96);
        let crop = root.bounds;
        for p in [
            geometry(-10, 10, 20, 20, 96),
            geometry(90, 10, 20, 20, 96),
            geometry(10, -10, 20, 20, 96),
            geometry(10, 90, 20, 20, 96),
        ] {
            let b = clip(root, p, crop).unwrap().unwrap();
            assert_eq!(b.source.width() * b.source.height(), 200);
        }
        assert_eq!(
            clip(root, geometry(100, 0, 20, 20, 96), crop).unwrap(),
            None
        );
    }
    #[test]
    fn mismatch_zero_dpi_invalid_crop_and_extreme_offset_fail_closed() {
        let root = geometry(0, 0, 100, 100, 96);
        let mut p = candidate(2).geometry;
        p.content.width += 1;
        assert_eq!(clip(root, p, root.bounds), Err(Refusal::Geometry));
        p = geometry(0, 0, 10, 10, 0);
        assert!(!geometry_valid(p));
        assert_eq!(
            clip(
                root,
                geometry(0, 0, 10, 10, 96),
                PixelRect::new((-1, 0).into(), (100, 100).into())
            ),
            Err(Refusal::Geometry)
        );
        assert_eq!(
            clip(
                geometry(i32::MIN, 0, 100, 100, 96),
                geometry(i32::MAX - 100, 0, 100, 100, 96),
                root.bounds
            )
            .unwrap(),
            None
        );
    }
    #[test]
    fn verified_stack_owner_order_and_topmost_are_bottom_to_top() {
        let mut a = candidate(2);
        let mut b = candidate(3);
        b.owners.insert(0, a.token);
        assert_eq!(
            order(vec![b.clone(), a.clone()], &[]).unwrap()[0].token,
            a.token
        );
        b.owners = vec![token(1)];
        a.topmost = true;
        assert_eq!(order(vec![a, b.clone()], &[]).unwrap()[0].token, b.token);
    }
    #[test]
    fn ambiguous_and_cyclic_overlap_rank_is_refused() {
        assert_eq!(
            order(vec![candidate(2), candidate(3)], &[]),
            Err(Refusal::Stack)
        );
        assert_eq!(
            order(vec![candidate(2), candidate(3)], &[(2, 3), (3, 2)]),
            Err(Refusal::Stack)
        );
        assert_eq!(
            order(vec![candidate(3), candidate(2)], &[(2, 3)]).unwrap()[0].token,
            token(2)
        );
    }
    #[test]
    fn alpha_zero_opaque_threshold_and_qualified_premultiplied_are_explicit() {
        let full = PixelRect::new((0, 0).into(), (4, 1).into());
        let b = Blit {
            source: full,
            destination: full,
        };
        let source = [
            255, 0, 255, 0, 255, 0, 255, 255, 64, 0, 64, 127, 128, 0, 128, 128,
        ];
        let baseline = [0, 0, 200, 255].repeat(4);
        let mut pixels = baseline.clone();
        let counts = composite(
            &mut pixels,
            PixelSize::new(4, 1),
            &source,
            PixelSize::new(4, 1),
            b,
            Alpha::Threshold128,
        )
        .unwrap();
        assert_eq!(
            counts,
            AlphaCounts {
                transparent: 1,
                opaque: 1,
                fractional: 2
            }
        );
        assert_eq!(&pixels[..4], &baseline[..4]);
        assert_eq!(&pixels[8..12], &baseline[8..12]);
        assert_eq!(&pixels[12..16], &[128, 0, 128, 255]);
        composite(
            &mut pixels,
            PixelSize::new(4, 1),
            &source,
            PixelSize::new(4, 1),
            b,
            Alpha::Premultiplied,
        )
        .unwrap();
        assert_eq!(&pixels[12..16], &[192, 0, 192, 255]);
    }
    #[test]
    fn dismissal_rebuilds_from_pristine_baseline_without_new_root_frame() {
        let baseline = vec![0, 0, 200, 255];
        let rect = PixelRect::new((0, 0).into(), (1, 1).into());
        let layer = Layer {
            pixels: &[255, 0, 255, 255],
            size: PixelSize::new(1, 1),
            blit: Blit {
                source: rect,
                destination: rect,
            },
        };
        let (open, _) = rebuild(
            &baseline,
            PixelSize::new(1, 1),
            &[layer],
            Alpha::Threshold128,
        )
        .unwrap();
        assert_ne!(&*open, &baseline);
        let (dismissed, _) =
            rebuild(&baseline, PixelSize::new(1, 1), &[], Alpha::Threshold128).unwrap();
        assert_eq!(&*dismissed, &baseline);
        assert_ne!(dismissed, open);
    }
    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
        #[test]
        fn clip_source_and_destination_are_equal_and_fully_contained(x in -400i32..400,y in -400i32..400,w in 1i32..256,h in 1i32..256) {
            let root=geometry(-100,-100,200,200,144);let popup=geometry(x,y,w,h,192);let crop=PixelRect::new((10,20).into(),(170,180).into());
            if let Some(b)=clip(root,popup,crop).unwrap() {
                prop_assert_eq!(b.source.width(),b.destination.width());prop_assert_eq!(b.source.height(),b.destination.height());
                prop_assert!(b.source.min.x>=0 && b.source.min.y>=0 && b.source.max.x<=w && b.source.max.y<=h);
                prop_assert!(b.destination.min.x>=0 && b.destination.min.y>=0 && b.destination.max.x<=160 && b.destination.max.y<=160);
            }
        }
    }
}
