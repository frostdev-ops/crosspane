#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::Displays;
use crosspane_platform_macos::displays::MacDisplays;
use crosspane_types::geom::PixelSize;
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayMode, CGDisplayRotation,
};

#[test]
fn enumerates_current_displays_read_only() {
    let displays = MacDisplays::new().unwrap().displays().unwrap();
    assert!(!displays.is_empty(), "expected at least one active display");
    for display in &displays {
        assert!(display.geometry.is_valid(), "{display:?}");
    }
    let built_in = displays
        .iter()
        .find(|display| CGDisplayIsBuiltin(display.id.0))
        .expect("expected the built-in 14-inch panel");
    let mode = CGDisplayCopyDisplayMode(built_in.id.0).expect("built-in display mode");
    let mut width = u32::try_from(CGDisplayMode::pixel_width(Some(&mode))).unwrap();
    let mut height = u32::try_from(CGDisplayMode::pixel_height(Some(&mode))).unwrap();
    let rotation = CGDisplayRotation(built_in.id.0).rem_euclid(360.0);
    if rotation == 90.0 || rotation == 270.0 {
        std::mem::swap(&mut width, &mut height);
    }
    println!("built-in display: {built_in:?}; current mode: {width} x {height}");
    assert_eq!(built_in.geometry.scale, 2.0);
    assert_eq!(built_in.geometry.pixel_size, PixelSize::new(width, height));
    assert!(
        (280.0..=320.0).contains(&built_in.geometry.physical_size.width),
        "{built_in:?}"
    );
}
