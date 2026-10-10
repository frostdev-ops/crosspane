#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::Displays;
use crosspane_platform_macos::displays::MacDisplays;
use crosspane_types::geom::PixelSize;
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayRotation};

#[test]
fn enumerates_current_displays_read_only() {
    let displays = MacDisplays::new().unwrap().displays().unwrap();
    assert!(!displays.is_empty(), "expected at least one active display");
    for display in &displays {
        assert!(display.geometry.is_valid(), "{display:?}");
        let mode = CGDisplayCopyDisplayMode(display.id.0).expect("active display mode");
        let mut width = u32::try_from(CGDisplayMode::pixel_width(Some(&mode))).unwrap();
        let mut height = u32::try_from(CGDisplayMode::pixel_height(Some(&mode))).unwrap();
        let rotation = CGDisplayRotation(display.id.0).rem_euclid(360.0);
        if rotation == 90.0 || rotation == 270.0 {
            std::mem::swap(&mut width, &mut height);
        }
        assert_eq!(display.geometry.pixel_size, PixelSize::new(width, height));
    }
}
