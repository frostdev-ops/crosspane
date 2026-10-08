use crosspane_platform::PlatformError;
use crosspane_platform_windows::model::twin::{
    HeartbeatSeq, OwnPath, PathWait, Refusal, TWIN_ABSENT_REASON, TWIN_UNAVAILABLE_REASON,
    TwinError, TwinMode, beat_outcome, classify, fallback, hardware_id_matches, interface_path_eq,
    parse_multi_sz, select_control_interface, select_new_path, twin_mode,
};
use crosspane_types::geom::PixelSize;

fn mode(width: u32, height: u32, width_mm: u32, height_mm: u32) -> TwinMode {
    TwinMode {
        width,
        height,
        width_mm,
        height_mm,
    }
}

fn path(monitor: &str, gdi: &str, rect: [i32; 4]) -> OwnPath {
    OwnPath {
        luid: 7,
        target: 3,
        source: 2,
        monitor_path: monitor.into(),
        gdi_name: gdi.into(),
        rect,
    }
}

/// Encodes entries as a REG_MULTI_SZ with the extra final terminator.
fn multi_sz(entries: &[&str]) -> Vec<u16> {
    let mut words: Vec<u16> = Vec::new();
    for entry in entries {
        words.extend(entry.encode_utf16());
        words.push(0);
    }
    words.push(0);
    words
}

#[test]
fn twin_mode_table() {
    // The twin is always 1920x1080, whatever the window's size. Its mm come from the mode and the
    // scale alone: 1920x1080 at 96 dpi is 508x286 mm (285.75 mm rounds to 286).
    assert_eq!(
        twin_mode(PixelSize::new(1920, 1080), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    assert_eq!(
        twin_mode(PixelSize::new(800, 600), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    assert_eq!(
        twin_mode(PixelSize::new(1281, 720), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    assert_eq!(
        twin_mode(PixelSize::new(1280, 720), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    assert_eq!(
        twin_mode(PixelSize::new(4000, 3000), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    assert_eq!(
        twin_mode(PixelSize::new(1, 1), 1.0).ok(),
        Some(mode(1920, 1080, 508, 286))
    );
    // Scale 2 halves the mm: 254.0 and 142.875 (rounds to 143).
    assert_eq!(
        twin_mode(PixelSize::new(800, 600), 2.0).ok(),
        Some(mode(1920, 1080, 254, 143))
    );
    // The mm are clamped to MM_MIN..=MM_MAX on both ends.
    assert_eq!(
        twin_mode(PixelSize::new(1920, 1080), 0.01).ok(),
        Some(mode(1920, 1080, 2000, 2000))
    );
    assert_eq!(
        twin_mode(PixelSize::new(1920, 1080), 1000.0).ok(),
        Some(mode(1920, 1080, 10, 10))
    );
}

#[test]
fn twin_mode_refuses_bad_size_and_scale() {
    let refused = [
        (PixelSize::new(0, 600), 1.0),
        (PixelSize::new(800, 0), 1.0),
        (PixelSize::new(800, 600), 0.0),
        (PixelSize::new(800, 600), -1.0),
        (PixelSize::new(800, 600), f64::NAN),
        (PixelSize::new(800, 600), f64::INFINITY),
    ];
    for (size, scale) in refused {
        assert!(matches!(
            twin_mode(size, scale).err(),
            Some(PlatformError::Backend(_))
        ));
    }
}

#[test]
fn classify_table() {
    let table = [
        (121, Refusal::Expired),
        (170, Refusal::Busy),
        (1168, Refusal::NotFound),
        (1450, Refusal::Capacity),
        (21, Refusal::Stopped),
        (87, Refusal::Invalid),
        (995, Refusal::Cancelled),
        (6, Refusal::Closed),
    ];
    for (code, refusal) in table {
        assert_eq!(classify(code), refusal);
    }
    for code in [0, 5, 122, 1451] {
        assert_eq!(classify(code), Refusal::Other(code));
    }
}

#[test]
fn fallback_mapping() {
    assert!(matches!(
        fallback(&TwinError::Absent),
        PlatformError::Unsupported(reason) if reason == TWIN_ABSENT_REASON
    ));
    assert!(matches!(
        fallback(&TwinError::Journal("ledger".into())),
        PlatformError::Backend(message) if message == "ledger"
    ));
    let others = [
        TwinError::Ambiguous,
        TwinError::Foreign,
        TwinError::Open(5),
        TwinError::Refused(Refusal::Busy),
        TwinError::Protocol("x"),
        TwinError::Timeout("x"),
        TwinError::LeaseLost,
        TwinError::NotOnDesktop,
        TwinError::Stale(1),
        TwinError::UnknownKey,
        TwinError::Native("x", 5),
    ];
    for error in others {
        assert!(matches!(
            fallback(&error),
            PlatformError::Unsupported(reason) if reason == TWIN_UNAVAILABLE_REASON
        ));
    }
}

#[test]
fn multi_sz_accepts_a_bounded_list() {
    assert_eq!(
        parse_multi_sz(&multi_sz(&["\\\\?\\a", "b"])).ok(),
        Some(vec!["\\\\?\\a".to_string(), "b".to_string()])
    );
    assert_eq!(parse_multi_sz(&[0, 0]).ok(), Some(Vec::new()));
    // Zero padding after the extra terminator is allowed.
    let mut padded = multi_sz(&["a"]);
    padded.extend([0, 0, 0]);
    assert_eq!(parse_multi_sz(&padded).ok(), Some(vec!["a".to_string()]));
    // Exactly 16 entries, and entries of exactly 512 units, are the bound.
    let names: Vec<String> = (0..16).map(|i| format!("d{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(
        parse_multi_sz(&multi_sz(&refs)).ok().map(|v| v.len()),
        Some(16)
    );
    let long = "a".repeat(512);
    assert!(parse_multi_sz(&multi_sz(&[long.as_str()])).is_ok());
    // A list of exactly 8192 words is the size bound.
    let mut at_bound = multi_sz(&["a"]);
    at_bound.resize(8_192, 0);
    assert!(parse_multi_sz(&at_bound).is_ok());
}

#[test]
fn multi_sz_refuses_out_of_bounds_and_malformed_lists() {
    // Empty input, and a list with no terminator at all.
    assert!(parse_multi_sz(&[]).is_err());
    assert!(parse_multi_sz(&[u16::from(b'a')]).is_err());
    // A lone NUL is not an empty list; the extra terminator is missing.
    assert!(parse_multi_sz(&[0]).is_err());
    // An entry without the extra terminator.
    assert!(parse_multi_sz(&[u16::from(b'a'), 0]).is_err());
    // Nonzero data after the final terminator.
    assert!(parse_multi_sz(&[u16::from(b'a'), 0, 0, 1]).is_err());
    // A control character in an entry.
    assert!(parse_multi_sz(&multi_sz(&["a\u{1}b"])).is_err());
    // Case-insensitive duplicates.
    assert!(parse_multi_sz(&multi_sz(&["Abc", "aBC"])).is_err());
    // More than 16 entries.
    let names: Vec<String> = (0..17).map(|i| format!("d{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert!(parse_multi_sz(&multi_sz(&refs)).is_err());
    // An entry longer than 512 units.
    let long = "a".repeat(513);
    assert!(parse_multi_sz(&multi_sz(&[long.as_str()])).is_err());
    // More than 8192 words.
    let mut over = multi_sz(&["a"]);
    over.resize(8_193, 0);
    assert!(parse_multi_sz(&over).is_err());
    // An unpaired surrogate is not valid UTF-16.
    assert!(parse_multi_sz(&[0xD800, 0, 0]).is_err());
}

#[test]
fn select_control_interface_counts() {
    assert_eq!(select_control_interface(&[]), Err(TwinError::Absent));
    let one = vec!["\\\\?\\one".to_string()];
    assert_eq!(select_control_interface(&one), Ok("\\\\?\\one"));
    let two = vec!["\\\\?\\one".to_string(), "\\\\?\\two".to_string()];
    assert_eq!(select_control_interface(&two), Err(TwinError::Ambiguous));
}

#[test]
fn hardware_id_must_match() {
    assert!(hardware_id_matches(&[r"Crosspane\IddTwinV1".to_string()]));
    assert!(hardware_id_matches(&[
        r"Root\Other".to_string(),
        r"crosspane\iddtwinv1".to_string(),
    ]));
    assert!(!hardware_id_matches(&[r"Root\Other".to_string()]));
    assert!(!hardware_id_matches(&[]));
}

#[test]
fn interface_paths_compare_case_insensitively_and_nonempty() {
    assert!(interface_path_eq("\\\\?\\Abc", "\\\\?\\aBC"));
    assert!(!interface_path_eq("", ""));
    assert!(!interface_path_eq("a", ""));
    assert!(!interface_path_eq("a", "ab"));
}

#[test]
fn select_new_path_outcomes() {
    let mode_720 = mode(1280, 720, 339, 191);
    let mode_1080 = mode(1920, 1080, 508, 286);
    let old = path("\\\\?\\old", "\\\\.\\DISPLAY2", [0, 0, 1280, 720]);
    let new_720 = path("\\\\?\\new", "\\\\.\\DISPLAY3", [0, 0, 1280, 720]);
    let other_new = path("\\\\?\\other", "\\\\.\\DISPLAY4", [0, 0, 1280, 720]);

    // Nothing new: Pending. Case differences do not make a path new.
    assert_eq!(select_new_path(&[], &[], mode_720), PathWait::Pending);
    let same_but_upper = path("\\\\?\\OLD", "\\\\.\\display2", [0, 0, 1280, 720]);
    assert_eq!(
        select_new_path(std::slice::from_ref(&old), &[same_but_upper], mode_720),
        PathWait::Pending
    );

    // Exactly one new path of the mode's size: Found, at its index in `after`.
    assert_eq!(
        select_new_path(&[], std::slice::from_ref(&new_720), mode_720),
        PathWait::Found(0)
    );
    assert_eq!(
        select_new_path(
            std::slice::from_ref(&old),
            &[old.clone(), new_720.clone()],
            mode_720
        ),
        PathWait::Found(1)
    );

    // Two new paths: Ambiguous.
    assert_eq!(
        select_new_path(&[], &[new_720.clone(), other_new], mode_720),
        PathWait::Ambiguous
    );

    // One new path with the wrong size: Pending.
    let big = path("\\\\?\\new", "\\\\.\\DISPLAY3", [0, 0, 1920, 1080]);
    assert_eq!(
        select_new_path(&[], std::slice::from_ref(&big), mode_720),
        PathWait::Pending
    );
    // A zero-sized rect is not a valid rect.
    let empty = path("\\\\?\\new", "\\\\.\\DISPLAY3", [5, 5, 5, 5]);
    assert_eq!(
        select_new_path(&[], std::slice::from_ref(&empty), mode_720),
        PathWait::Pending
    );

    // A removed twin's path comes back in the same slot: it is new again, and it is Found at
    // 1080p once its size matches.
    let reused = path("\\\\?\\new", "\\\\.\\DISPLAY3", [100, 100, 2020, 1180]);
    assert_eq!(
        select_new_path(&[], std::slice::from_ref(&reused), mode_1080),
        PathWait::Found(0)
    );
}

#[test]
fn heartbeat_sequence_increases_and_closes() {
    let mut seq = HeartbeatSeq::default();
    // The first value is 1, and each call adds exactly one.
    for expected in 1..=1_000_u64 {
        assert_eq!(seq.next().ok(), Some(expected));
    }
    seq.close();
    assert!(seq.next().is_err());
    assert!(seq.next().is_err());
}

#[test]
fn beat_outcome_marks_lost_on_misses_or_other_errors() {
    let timeout = Err(TwinError::Timeout("heartbeat"));
    assert_eq!(beat_outcome(&Ok(()), 1), (0, false));
    assert_eq!(beat_outcome(&timeout, 0), (1, false));
    assert_eq!(beat_outcome(&timeout, 1), (2, true));
    assert_eq!(beat_outcome(&Err(TwinError::LeaseLost), 0), (0, true));
    assert_eq!(beat_outcome(&Err(TwinError::Protocol("x")), 0), (0, true));
}
