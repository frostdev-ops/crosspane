use std::collections::BTreeSet;

use crosspane_platform_windows::model::geometry::{
    DisplayIds, DisplayLayout, GeometryError, MDT_EFFECTIVE_DPI, MonitorProbe, SeamDiagnostics,
    SeamRecord, SeamSide, USER_DEFAULT_SCREEN_DPI, WM_DPICHANGED, display_id, displays,
    edid_physical_size, logical_origins, physical_to_device, work_area_clamp,
};
use crosspane_types::{
    geom::{DisplayGeometry, PixelSize, PointDevice, PointLogical, SizeMm},
    id::DisplayId,
};
use proptest::prelude::*;

// Published Dell P1917S base block, not read from the host:
// https://raw.githubusercontent.com/bsdhw/EDID/master/Digital/Dell/DELD093/A9D3F612C6C3
fn dell_edid() -> Vec<u8> {
    "00 ff ff ff ff ff ff 00 10 ac 93 d0 42 30 39 31
     34 1b 01 03 80 26 1e 78 ea c9 75 a6 57 51 9d 27
     0e 50 54 a5 4b 00 71 4f 81 80 01 01 01 01 01 01
     01 01 01 01 01 01 30 2a 00 98 51 00 2a 40 30 70
     13 00 77 2c 11 00 00 1e 00 00 00 ff 00 39 50 58
     33 47 37 43 50 31 39 30 42 0a 00 00 00 fc 00 44
     45 4c 4c 20 50 31 39 31 37 53 0a 20 00 00 00 fd
     00 38 4c 1e 51 0e 00 0a 20 20 20 20 20 20 01 f2"
        .split_whitespace()
        .map(|s| {
            u8::from_str_radix(s, 16).unwrap_or_else(|error| panic!("fixture byte {s}: {error}"))
        })
        .collect()
}

fn checksum(edid: &mut [u8]) {
    edid[127] = 0;
    edid[127] = 0_u8.wrapping_sub(edid.iter().fold(0_u8, |s, b| s.wrapping_add(*b)));
}

fn probe(path: &str, rect: [i32; 4], dpi: u32) -> MonitorProbe {
    MonitorProbe {
        device_path: path.into(),
        name: path.into(),
        rc_monitor: rect,
        rc_work: rect,
        primary: false,
        dpi,
        refresh_millihz: 60_000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    }
}

fn fixture() -> Vec<MonitorProbe> {
    let mut a = probe("A-1080p", [0, 0, 1920, 1080], 96);
    a.primary = true;
    let b = probe("B-4K", [1920, 0, 5760, 2160], 192);
    let mut c = probe("C-rotated", [0, 1080, 1080, 3000], 120);
    c.quarter_turns = 1;
    c.edid = Some(dell_edid());
    vec![a, b, c]
}

fn render(probes: &[MonitorProbe]) -> Result<DisplayLayout, GeometryError> {
    displays(probes, &mut DisplayIds::default())
}

fn seam_counts(seams: &SeamDiagnostics) -> (usize, usize, usize) {
    (
        seams.records.iter().filter(|s| s.walked).count(),
        seams.records.iter().filter(|s| s.kept).count(),
        seams.records.iter().filter(|s| !s.kept).count(),
    )
}

fn no_overlap(layout: &DisplayLayout) {
    for (i, a) in layout.displays.iter().enumerate() {
        assert!(a.geometry.is_valid());
        let a = a.geometry.logical_bounds();
        for b in &layout.displays[..i] {
            let b = b.geometry.logical_bounds();
            assert!(
                a.max_x() <= b.min_x()
                    || b.max_x() <= a.min_x()
                    || a.max_y() <= b.min_y()
                    || b.max_y() <= a.min_y()
            );
        }
    }
}

#[test]
fn microsoft_numeric_constants_match_documented_contract() {
    assert_eq!(MDT_EFFECTIVE_DPI, 0);
    assert_eq!(USER_DEFAULT_SCREEN_DPI, 96);
    assert_eq!(WM_DPICHANGED, 0x02e0);
}

#[test]
fn published_128_byte_edid_prefers_dtd_millimetres_over_centimetres() {
    let e = dell_edid();
    assert_eq!(e.len(), 128);
    assert_eq!(e.iter().fold(0_u8, |s, b| s.wrapping_add(*b)), 0);
    assert_eq!(edid_physical_size(&e), Some(SizeMm::new(375.0, 300.0)));
    assert_eq!((e[21], e[22]), (38, 30));
}

#[test]
fn edid_zero_dtd_uses_centimetres_and_zero_unknown_size_uses_96dpi() {
    let mut e = dell_edid();
    e[66..69].fill(0);
    checksum(&mut e);
    assert_eq!(edid_physical_size(&e), Some(SizeMm::new(380.0, 300.0)));
    e[21..23].fill(0);
    checksum(&mut e);
    assert_eq!(edid_physical_size(&e), None);
    let mut p = probe("zero", [0, 0, 1920, 1080], 192);
    p.primary = true;
    p.edid = Some(e);
    let layout = render(&[p]).unwrap();
    assert_eq!(
        layout.displays[0].geometry.physical_size,
        SizeMm::new(508.0, 285.75)
    );
    assert_eq!(layout.displays[0].geometry.scale, 2.0);
}

#[test]
fn edid_truncation_header_checksum_version_and_aspect_only_sizes_are_unknown() {
    let e = dell_edid();
    for length in 0..128 {
        assert_eq!(edid_physical_size(&e[..length]), None);
    }
    for index in [0, 50, 127] {
        let mut broken = e.clone();
        broken[index] ^= 1;
        assert_eq!(edid_physical_size(&broken), None);
    }
    let mut broken = e.clone();
    broken[18] = 2;
    checksum(&mut broken);
    assert_eq!(edid_physical_size(&broken), None);
    broken = e;
    broken[19] = 5;
    checksum(&mut broken);
    assert_eq!(edid_physical_size(&broken), None);
    broken[19] = 4;
    broken[66..69].fill(0);
    broken[22] = 0;
    checksum(&mut broken);
    assert_eq!(edid_physical_size(&broken), None);
}

#[test]
fn fnv_ids_are_stable_and_collision_bumps_avoid_all_taken_ids() {
    let mut taken = BTreeSet::new();
    assert_eq!(display_id("", &taken).unwrap(), DisplayId(0x811c9dc5));
    assert_eq!(display_id("a", &taken).unwrap(), DisplayId(0xe40c292c));
    let first = display_id("costarring", &taken).unwrap();
    assert_eq!(first, display_id("liquid", &taken).unwrap());
    taken.insert(first);
    taken.insert(DisplayId(first.0.wrapping_add(1)));
    assert_eq!(
        display_id("liquid", &taken).unwrap(),
        DisplayId(first.0.wrapping_add(2))
    );
}

#[test]
fn colliding_paths_are_assigned_identically_when_probe_order_changes() {
    let mut p = probe("root", [0, 0, 100, 100], 96);
    p.primary = true;
    let mut probes = vec![
        p,
        probe("costarring", [-100, 0, 0, 100], 96),
        probe("liquid", [100, 0, 200, 100], 96),
    ];
    let first = render(&probes).unwrap();
    probes.reverse();
    assert_eq!(first, render(&probes).unwrap());
    let ids: BTreeSet<_> = first.displays.iter().map(|d| d.id).collect();
    assert_eq!(ids.len(), 3);
    no_overlap(&first);
}

#[test]
fn mixed_dpi_1080p_4k_rotated_fixture_has_shared_right_and_below_seams() {
    let layout = render(&fixture()).unwrap();
    let a = &layout.displays[0].geometry;
    let b = &layout.displays[1].geometry;
    let c = &layout.displays[2].geometry;
    assert_eq!(a.logical_origin, PointLogical::zero());
    assert_eq!(a.logical_bounds().max_x(), b.logical_bounds().min_x());
    assert_eq!(a.logical_bounds().max_y(), c.logical_bounds().min_y());
    assert_eq!(b.pixel_size, PixelSize::new(3840, 2160));
    assert_eq!(b.scale, 2.0);
    assert_eq!(c.pixel_size, PixelSize::new(1080, 1920));
    assert_eq!(c.physical_size, SizeMm::new(300.0, 375.0));
    assert_eq!(c.scale, 1.25);
    assert_eq!(seam_counts(&layout.seams), (2, 2, 0));
    no_overlap(&layout);
}

#[test]
fn rotation_changes_only_edid_mm_and_never_rotates_oriented_fallback_twice() {
    for turns in 0..4 {
        let mut p = probe("rotated", [-1080, 0, 0, 1920], 96);
        p.primary = true;
        p.quarter_turns = turns;
        p.edid = Some(dell_edid());
        let layout = render(&[p.clone()]).unwrap();
        let expected = if turns % 2 == 1 {
            SizeMm::new(300.0, 375.0)
        } else {
            SizeMm::new(375.0, 300.0)
        };
        assert_eq!(layout.displays[0].geometry.physical_size, expected);
        assert_eq!(
            layout.displays[0].geometry.pixel_size,
            PixelSize::new(1080, 1920)
        );
        p.edid = None;
        assert_eq!(
            render(&[p]).unwrap().displays[0].geometry.physical_size,
            SizeMm::new(285.75, 508.0)
        );
    }
}

#[test]
fn twins_are_filtered_before_identity_primary_and_geometry_validation() {
    let mut probes = fixture();
    let mut twin = probe("A-1080p", [0; 4], 0);
    twin.primary = true;
    twin.twin = true;
    twin.quarter_turns = 255;
    let expected = render(&probes).unwrap();
    probes.push(twin.clone());
    assert_eq!(render(&probes).unwrap(), expected);
    assert!(render(&[twin]).unwrap().displays.is_empty());
}

#[test]
fn window_conversion_uses_its_own_monitor_geometry_and_roundtrips() {
    let layout = render(&fixture()).unwrap();
    let g = layout.displays[1].geometry;
    let local = physical_to_device((2120, 400), [1920, 0, 5760, 2160]);
    assert_eq!(local, PointDevice::new(200.0, 400.0));
    let logical = g.device_to_logical(local);
    assert_eq!(logical, PointLogical::new(2020.0, 200.0));
    assert_eq!(g.logical_to_device(logical), local);
}

#[test]
fn bfs_longest_shared_edge_precedes_smaller_display_id() {
    let rects = [
        (DisplayId(10), [0, 0, 100, 100], 1.0),
        (DisplayId(20), [100, 0, 200, 75], 2.0),
        (DisplayId(30), [0, 100, 150, 200], 1.0),
        (DisplayId(40), [150, 75, 250, 175], 1.0),
    ];
    let result = logical_origins(&rects, 0).unwrap();
    assert_eq!(result.origins[3], PointLogical::new(150.0, 75.0));
    assert_eq!(seam_counts(&result.seams), (3, 3, 1));
}

#[test]
fn bfs_equal_shared_edges_break_ties_by_id_independently_of_input_order() {
    let rects = [
        (DisplayId(10), [0, 0, 100, 100], 1.0),
        (DisplayId(20), [100, 0, 200, 100], 2.0),
        (DisplayId(30), [0, 100, 100, 200], 1.0),
        (DisplayId(40), [100, 100, 200, 200], 1.0),
    ];
    let first = logical_origins(&rects, 0).unwrap();
    assert_eq!(first.origins[3], PointLogical::new(100.0, 50.0));
    let order = [3, 2, 0, 1];
    let changed: Vec<_> = order.iter().map(|&i| rects[i]).collect();
    let next = logical_origins(&changed, 2).unwrap();
    for (i, &original) in order.iter().enumerate() {
        assert_eq!(next.origins[i], first.origins[original]);
    }
}

#[test]
fn broken_non_tree_seams_are_diagnosed_without_refusing_nonoverlapping_layout() {
    let rects = [
        (DisplayId(10), [0, 0, 100, 100], 1.0),
        (DisplayId(30), [100, 0, 200, 100], 2.0),
        (DisplayId(20), [0, 100, 100, 200], 1.0),
        (DisplayId(40), [100, 100, 200, 200], 1.0),
    ];
    let result = logical_origins(&rects, 0).unwrap();
    assert_eq!(result.origins[3], PointLogical::new(100.0, 100.0));
    assert_eq!(seam_counts(&result.seams), (3, 3, 1));
}

#[test]
fn logical_overlap_refuses_the_complete_layout_including_disconnected_components() {
    let overlapping = [
        (DisplayId(1), [0, 0, 100, 100], 1.0),
        (DisplayId(2), [100, 0, 200, 100], 0.5),
        (DisplayId(3), [0, 100, 200, 200], 1.0),
    ];
    assert_eq!(
        logical_origins(&overlapping, 0),
        Err(GeometryError::Overlap)
    );
    let disconnected = [
        (DisplayId(1), [0, 0, 100, 100], 1.0),
        (DisplayId(2), [150, 0, 250, 100], 0.1),
        (DisplayId(3), [300, 0, 400, 100], 1.0),
    ];
    assert_eq!(
        logical_origins(&disconnected, 0),
        Err(GeometryError::Overlap)
    );
}

#[test]
fn disconnected_components_anchor_to_primary_scale_with_negative_origins() {
    let rects = [
        (DisplayId(20), [100, 100, 300, 300], 2.0),
        (DisplayId(10), [-300, 100, -100, 300], 2.0),
        (DisplayId(30), [500, 500, 700, 700], 1.0),
    ];
    let result = logical_origins(&rects, 0).unwrap();
    assert_eq!(
        result.origins,
        vec![
            PointLogical::zero(),
            PointLogical::new(-200.0, 0.0),
            PointLogical::new(200.0, 200.0)
        ]
    );
    assert_eq!(result.seams, SeamDiagnostics::default());
}

#[test]
fn invalid_primary_ids_rectangles_scales_and_unrepresentable_bounds_refuse() {
    assert!(logical_origins(&[], 999).unwrap().origins.is_empty());
    let valid = (DisplayId(1), [0, 0, 100, 100], 1.0);
    assert_eq!(
        logical_origins(&[valid], 1),
        Err(GeometryError::InvalidPrimary)
    );
    assert_eq!(
        logical_origins(&[valid, valid], 0),
        Err(GeometryError::DuplicateId)
    );
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE] {
        let rect = (DisplayId(1), [0, 0, 100, 100], scale);
        assert_eq!(
            logical_origins(&[rect], 0),
            Err(GeometryError::InvalidMonitor)
        );
    }
    for rect in [[0; 4], [100, 0, 0, 100], [0, 10, 100, 10]] {
        assert_eq!(
            logical_origins(&[(DisplayId(1), rect, 1.0)], 0),
            Err(GeometryError::InvalidMonitor)
        );
    }
    let collapsed = [
        (DisplayId(1), [0, 0, 1, 1], 1e-200),
        (DisplayId(2), [3, 3, 4, 4], 1e200),
    ];
    assert_eq!(
        logical_origins(&collapsed, 0),
        Err(GeometryError::InvalidMonitor)
    );
}

#[test]
fn invalid_native_observations_refuse_without_defaulting_dpi_or_primary() {
    assert!(render(&[]).unwrap().displays.is_empty());
    let mut p = fixture()[0].clone();
    p.primary = false;
    assert_eq!(render(&[p.clone()]), Err(GeometryError::InvalidPrimary));
    p.primary = true;
    assert_eq!(
        render(&[p.clone(), fixture()[0].clone()]),
        Err(GeometryError::DuplicateDevicePath)
    );
    let mut other = probe("other", [1920, 0, 3840, 1080], 96);
    other.primary = true;
    assert_eq!(
        render(&[p.clone(), other]),
        Err(GeometryError::InvalidPrimary)
    );
    for field in 0..4 {
        let mut invalid = p.clone();
        match field {
            0 => invalid.dpi = 0,
            1 => invalid.quarter_turns = 4,
            2 => invalid.rc_work[0] = -1,
            _ => invalid.device_path.clear(),
        }
        assert_eq!(render(&[invalid]), Err(GeometryError::InvalidMonitor));
    }
}

#[test]
fn physical_to_device_handles_negative_and_full_i32_coordinates_without_overflow() {
    assert_eq!(
        physical_to_device((-1820, 20), [-1920, -200, 0, 880]),
        PointDevice::new(100.0, 220.0)
    );
    assert_eq!(
        physical_to_device((i32::MAX, i32::MAX), [i32::MIN, i32::MIN, 0, 0]),
        PointDevice::new(4_294_967_295.0, 4_294_967_295.0)
    );
}

#[test]
fn work_area_clamp_uses_monitor_local_coordinates_and_anchors_oversized_windows() {
    let monitor = [-1920, -1080, 0, 0];
    let work = [-1880, -1040, -20, -50];
    let size = PixelSize::new(200, 100);
    assert_eq!(
        work_area_clamp(PointDevice::new(-10.0, -20.0), size, work, monitor).unwrap(),
        PointDevice::new(40.0, 40.0)
    );
    assert_eq!(
        work_area_clamp(PointDevice::new(5000.0, 5000.0), size, work, monitor).unwrap(),
        PointDevice::new(1700.0, 930.0)
    );
    assert_eq!(
        work_area_clamp(PointDevice::new(400.0, 300.0), size, work, monitor).unwrap(),
        PointDevice::new(400.0, 300.0)
    );
    assert_eq!(
        work_area_clamp(
            PointDevice::new(400.0, 300.0),
            PixelSize::new(5000, 5000),
            work,
            monitor
        )
        .unwrap(),
        PointDevice::new(40.0, 40.0)
    );
}

#[test]
fn invalid_work_area_window_size_and_nonfinite_origin_refuse_without_panicking() {
    let monitor = [-100, -100, 100, 100];
    for origin in [
        PointDevice::new(f64::NAN, 0.0),
        PointDevice::new(0.0, f64::INFINITY),
    ] {
        assert_eq!(
            work_area_clamp(origin, PixelSize::new(1, 1), monitor, monitor),
            Err(GeometryError::InvalidMonitor)
        );
    }
    for work in [[0; 4], [-101, -100, 100, 100], [-100, -100, 101, 100]] {
        assert_eq!(
            work_area_clamp(PointDevice::zero(), PixelSize::new(1, 1), work, monitor),
            Err(GeometryError::InvalidMonitor)
        );
    }
    assert_eq!(
        work_area_clamp(PointDevice::zero(), PixelSize::new(0, 1), monitor, monitor),
        Err(GeometryError::InvalidMonitor)
    );
}

#[test]
fn retained_ids_survive_colliding_path_hotplug_empty_snapshots_and_readdition() {
    let mut root = probe("root", [0, 0, 100, 100], 96);
    root.primary = true;
    let costarring = probe("costarring", [-100, 0, 0, 100], 96);
    let liquid = probe("liquid", [100, 0, 200, 100], 96);
    let mut ids = DisplayIds::default();
    let original = displays(
        &[root.clone(), costarring.clone(), liquid.clone()],
        &mut ids,
    )
    .unwrap();
    let named = |layout: &DisplayLayout, name: &str| {
        layout.displays.iter().find(|d| d.name == name).unwrap().id
    };
    let original_liquid = named(&original, "liquid");
    let original_costarring = named(&original, "costarring");
    let removed = displays(&[root.clone(), liquid.clone()], &mut ids).unwrap();
    assert_eq!(named(&removed, "liquid"), original_liquid);
    assert!(displays(&[], &mut ids).unwrap().displays.is_empty());
    let readded = displays(&[liquid, costarring, root], &mut ids).unwrap();
    assert_eq!(named(&readded, "liquid"), original_liquid);
    assert_eq!(named(&readded, "costarring"), original_costarring);
    assert_ne!(original_liquid, original_costarring);
    no_overlap(&readded);
}

#[test]
fn refused_geometry_snapshot_does_not_publish_partial_id_allocations() {
    let mut root = probe("costarring", [0, 0, 100, 100], 96);
    root.primary = true;
    let invalid = probe("liquid", [150, 0, 250, 100], 0);
    let mut ids = DisplayIds::default();
    assert_eq!(
        displays(&[root, invalid], &mut ids),
        Err(GeometryError::InvalidMonitor)
    );
    let assigned = ids.assign("liquid").unwrap();
    assert_eq!(assigned, display_id("liquid", &BTreeSet::new()).unwrap());
    assert_eq!(ids.assign("liquid").unwrap(), assigned);
}

fn review_rounding_fixture(scale: f64) {
    for turns in 0..4 {
        let mut rects = [
            (DisplayId(10), [0, 0, 1920, 1080], scale),
            (DisplayId(20), [1, 1080, 1921, 2160], 1.0),
            (DisplayId(30), [-1919, 1200, 1, 2280], 1.0),
        ];
        for (_, r, _) in &mut rects {
            for _ in 0..turns {
                *r = [-r[3], r[0], -r[1], r[2]];
            }
        }
        let layout = logical_origins(&rects, 0)
            .unwrap_or_else(|error| panic!("scale {scale}, direction {turns}: {error:?}"));
        assert_eq!(
            seam_counts(&layout.seams),
            (2, 2, 0),
            "scale {scale}, direction {turns}"
        );
        let bounds: Vec<_> = rects
            .iter()
            .zip(&layout.origins)
            .map(|((_, r, dpi), &origin)| {
                DisplayGeometry {
                    pixel_size: PixelSize::new((r[2] - r[0]) as u32, (r[3] - r[1]) as u32),
                    physical_size: SizeMm::new(1.0, 1.0),
                    scale: *dpi,
                    logical_origin: origin,
                }
                .logical_bounds()
            })
            .collect();
        for (i, a) in bounds.iter().enumerate() {
            for b in &bounds[..i] {
                assert!(
                    a.max_x() <= b.min_x()
                        || b.max_x() <= a.min_x()
                        || a.max_y() <= b.min_y()
                        || b.max_y() <= a.min_y(),
                    "scale {scale}, direction {turns}: {a:?} overlaps {b:?}"
                );
            }
        }
        let b = bounds[1];
        let c = bounds[2];
        let (outside, inside) = match turns {
            0 => (c.max_x(), b.min_x()),
            1 => (c.max_y(), b.min_y()),
            2 => (b.max_x(), c.min_x()),
            _ => (b.max_y(), c.min_y()),
        };
        assert!(outside <= inside, "outward rounding must never overlap");
        let operands = b
            .min_x()
            .abs()
            .max(b.min_y().abs())
            .max(c.min_x().abs())
            .max(c.min_y().abs())
            .max(b.size.width)
            .max(b.size.height)
            .max(c.size.width)
            .max(c.size.height);
        assert!(inside - outside <= 16.0 * f64::EPSILON * operands.max(1.0));
    }
}

#[test]
fn review_rounding_primary_scale_1_5_preserves_all_four_walked_directions() {
    review_rounding_fixture(1.5);
}

#[test]
fn review_rounding_primary_scale_1_25_preserves_all_four_walked_directions() {
    review_rounding_fixture(1.25);
}

#[test]
fn review_seam_diagnostics_identify_pairs_sides_and_walked_edges() {
    let rects = [
        (DisplayId(10), [0, 0, 100, 100], 1.0),
        (DisplayId(30), [100, 0, 200, 100], 2.0),
        (DisplayId(20), [0, 100, 100, 200], 1.0),
        (DisplayId(40), [100, 100, 200, 200], 1.0),
    ];
    let result = logical_origins(&rects, 0).unwrap();
    let expected: Vec<_> = [
        ((10, 20), SeamSide::Below, true, true),
        ((10, 30), SeamSide::Right, true, true),
        ((20, 40), SeamSide::Right, true, true),
        ((30, 40), SeamSide::Below, false, false),
    ]
    .into_iter()
    .map(|((a, b), side, walked, kept)| SeamRecord {
        displays: (DisplayId(a), DisplayId(b)),
        side,
        walked,
        kept,
    })
    .collect();
    assert_eq!(result.seams.records, expected);
    let reversed: Vec<_> = rects.into_iter().rev().collect();
    assert_eq!(
        logical_origins(&reversed, 3).unwrap().seams.records,
        expected
    );
}

proptest! {
    #[test]
    fn right_chain_walked_seams_stay_shared_without_logical_overlap(
        widths in prop::collection::vec(1_i32..10_000, 1..12),
        height in 1_i32..10_000,
        dpis in prop::collection::vec(prop::sample::select(vec![96_u32, 120, 144, 168, 192]), 12),
    ) {
        let mut x = 0;
        let rects: Vec<_> = widths.iter().enumerate().map(|(i, &w)| {
            let r = [x, 0, x + w, height];
            x += w;
            (DisplayId(i as u32), r, f64::from(dpis[i]) / 96.0)
        }).collect();
        let result = logical_origins(&rects, 0).unwrap();
        prop_assert_eq!(seam_counts(&result.seams), (widths.len() - 1, widths.len() - 1, 0));
        for i in 1..rects.len() {
            let previous_right = result.origins[i - 1].x + f64::from(widths[i - 1]) / rects[i - 1].2;
            prop_assert_eq!(previous_right, result.origins[i].x);
        }
    }

    #[test]
    fn arbitrary_edid_bytes_never_panic_or_return_invalid_size(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Some(size) = edid_physical_size(&bytes) {
            prop_assert!(size.width > 0.0 && size.width <= 4095.0);
            prop_assert!(size.height > 0.0 && size.height <= 4095.0);
        }
    }

    #[test]
    fn clamped_window_fits_work_area_or_keeps_oversized_top_left_visible(
        x in -1e6_f64..1e6, y in -1e6_f64..1e6,
        w in 1_u32..5000, h in 1_u32..5000,
    ) {
        let p = work_area_clamp(PointDevice::new(x, y), PixelSize::new(w, h),
            [-1880, -1040, -20, -50], [-1920, -1080, 0, 0]).unwrap();
        prop_assert!(p.x >= 40.0 && p.y >= 40.0);
        if w <= 1860 {
            prop_assert!(p.x + f64::from(w) <= 1900.0);
        } else {
            prop_assert_eq!(p.x, 40.0);
        }
        if h <= 990 {
            prop_assert!(p.y + f64::from(h) <= 1030.0);
        } else {
            prop_assert_eq!(p.y, 40.0);
        }
    }
}
