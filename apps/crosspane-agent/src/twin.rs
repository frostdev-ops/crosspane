//! Window parking on a twin display (M2) when one can be made, and mirror in place (M1, the
//! reported fallback) when it can't (`Unsupported`): on the Mac the opt-in private-API virtual
//! display (D7, `mac_virtual_display`) may be missing; on Hyprland a headless output may fail to
//! allocate (a nested Hyprland always does). Each window stays with the backend that parked it
//! until it is restored.

use std::collections::BTreeSet;

use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, WindowId};

pub struct TwinOrMirror {
    twin: Box<dyn WindowParking>,
    mirror: Box<dyn WindowParking>,
    mirrored: BTreeSet<WindowId>,
}

impl TwinOrMirror {
    pub fn new(twin: Box<dyn WindowParking>, mirror: Box<dyn WindowParking>) -> TwinOrMirror {
        TwinOrMirror {
            twin,
            mirror,
            mirrored: BTreeSet::new(),
        }
    }

    fn backend(&mut self, window: WindowId) -> &mut dyn WindowParking {
        if self.mirrored.contains(&window) {
            self.mirror.as_mut()
        } else {
            self.twin.as_mut()
        }
    }
}

impl WindowParking for TwinOrMirror {
    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        self.backend(window).set_fullscreen(window, fullscreen)
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        if self.mirrored.contains(&window) {
            return self.mirror.park(window, size, scale);
        }
        match self.twin.park(window, size, scale) {
            Err(PlatformError::Unsupported(why)) => {
                tracing::warn!(why, "no twin display: this window is mirrored instead (M1)");
                let parked = self.mirror.park(window, size, scale)?;
                self.mirrored.insert(window);
                Ok(parked)
            }
            other => other,
        }
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.backend(window).resize(window, size, scale)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        if self.mirrored.contains(&window) {
            self.mirror.geometry(window)
        } else {
            self.twin.geometry(window)
        }
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let result = self.backend(window).restore(window);
        if result.is_ok() {
            self.mirrored.remove(&window);
        }
        result
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        at: PointDevice,
    ) -> Result<(), PlatformError> {
        let result = self.backend(window).restore_at(window, display, at);
        if result.is_ok() {
            self.mirrored.remove(&window);
        }
        result
    }

    /// Both backends recover, each from its own journal; the first error is returned after both
    /// have run.
    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let twin = self.twin.recover();
        let mirror = self.mirror.recover();
        self.mirrored.clear();
        let mut restored = Vec::new();
        let mut first_error = None;
        for result in [twin, mirror] {
            match result {
                Ok(windows) => restored.extend(windows),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(restored),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crosspane_platform::ParkingKind;
    use crosspane_types::geom::PixelRect;
    use crosspane_types::id::DisplayId;

    use super::*;

    /// Records calls; `unsupported` makes `park` refuse as a vanished API would.
    struct Fake {
        name: &'static str,
        unsupported: bool,
        restore_fails: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        fn parked(&self, window: WindowId) -> Parked {
            Parked {
                fullscreen: false,
                window,
                kind: if self.name == "twin" {
                    ParkingKind::Twin
                } else {
                    ParkingKind::Mirror
                },
                display: DisplayId(1),
                content: PixelRect::zero(),
            }
        }
        fn log(&self, what: &str, window: WindowId) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {what} {}", self.name, window.0));
        }
    }

    impl WindowParking for Fake {
        fn set_fullscreen(&mut self, w: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
            self.log(&format!("fullscreen {fullscreen}"), w);
            Err(PlatformError::Unsupported("fullscreen is not implemented"))
        }

        fn park(&mut self, w: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError> {
            self.log("park", w);
            if self.unsupported {
                return Err(PlatformError::Unsupported("gone"));
            }
            Ok(self.parked(w))
        }
        fn resize(&mut self, w: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError> {
            self.log("resize", w);
            Ok(self.parked(w))
        }
        fn geometry(&self, w: WindowId) -> Result<Parked, PlatformError> {
            Ok(self.parked(w))
        }
        fn restore(&mut self, w: WindowId) -> Result<(), PlatformError> {
            self.log("restore", w);
            Ok(())
        }
        fn restore_at(
            &mut self,
            w: WindowId,
            display: DisplayId,
            at: PointDevice,
        ) -> Result<(), PlatformError> {
            self.log(&format!("restore_at {} {},{}", display.0, at.x, at.y), w);
            if self.restore_fails {
                Err(PlatformError::NotFound)
            } else {
                Ok(())
            }
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} recover", self.name));
            Ok(Vec::new())
        }
    }

    fn parking(twin_unsupported: bool) -> (TwinOrMirror, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let twin = Fake {
            name: "twin",
            unsupported: twin_unsupported,
            restore_fails: false,
            calls: calls.clone(),
        };
        let mirror = Fake {
            name: "mirror",
            unsupported: false,
            restore_fails: false,
            calls: calls.clone(),
        };
        (TwinOrMirror::new(Box::new(twin), Box::new(mirror)), calls)
    }

    #[test]
    fn a_vanished_api_falls_back_to_mirroring_for_that_window() {
        let (mut p, calls) = parking(true);
        let size = PixelSize::new(800, 600);
        let parked = p.park(WindowId(7), size, 2.0).unwrap();
        assert_eq!(parked.kind, ParkingKind::Mirror);
        p.resize(WindowId(7), size, 2.0).unwrap();
        assert_eq!(p.geometry(WindowId(7)).unwrap().kind, ParkingKind::Mirror);
        p.restore(WindowId(7)).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "twin park 7",
                "mirror park 7",
                "mirror resize 7",
                "mirror restore 7"
            ]
        );
    }

    #[test]
    fn fullscreen_routes_to_the_backend_that_parked_the_window() {
        for mirrored in [false, true] {
            let (mut parking, calls) = parking(mirrored);
            parking
                .park(WindowId(8), PixelSize::new(800, 600), 1.0)
                .unwrap();
            calls.lock().unwrap().clear();
            for fullscreen in [true, false] {
                assert!(matches!(
                    parking.set_fullscreen(WindowId(8), fullscreen),
                    Err(PlatformError::Unsupported(_))
                ));
            }
            let backend = if mirrored { "mirror" } else { "twin" };
            assert_eq!(
                *calls.lock().unwrap(),
                [
                    format!("{backend} fullscreen true 8"),
                    format!("{backend} fullscreen false 8")
                ]
            );
        }
    }

    #[test]
    fn twin_parking_is_used_when_available_and_both_recover() {
        let (mut p, calls) = parking(false);
        let size = PixelSize::new(800, 600);
        assert_eq!(
            p.park(WindowId(3), size, 1.0).unwrap().kind,
            ParkingKind::Twin
        );
        p.resize(WindowId(3), size, 1.0).unwrap();
        p.restore(WindowId(3)).unwrap();
        p.recover().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "twin park 3",
                "twin resize 3",
                "twin restore 3",
                "twin recover",
                "mirror recover"
            ]
        );
    }

    #[test]
    fn placed_restore_uses_the_selected_backend_and_failed_mirror_restore_retains_selection() {
        for mirrored in [false, true] {
            let (mut p, calls) = parking(mirrored);
            p.park(WindowId(8), PixelSize::new(800, 600), 1.0).unwrap();
            if mirrored {
                p.mirror = Box::new(Fake {
                    name: "mirror",
                    unsupported: false,
                    restore_fails: true,
                    calls: calls.clone(),
                });
                assert!(
                    p.restore_at(WindowId(8), DisplayId(7), PointDevice::new(30.0, 40.0))
                        .is_err()
                );
                assert!(p.mirrored.contains(&WindowId(8)));
                p.mirror = Box::new(Fake {
                    name: "mirror",
                    unsupported: false,
                    restore_fails: false,
                    calls: calls.clone(),
                });
            }
            p.restore_at(WindowId(8), DisplayId(7), PointDevice::new(30.0, 40.0))
                .unwrap();
            assert!(!p.mirrored.contains(&WindowId(8)));
            assert_eq!(
                calls.lock().unwrap().last().unwrap(),
                if mirrored {
                    "mirror restore_at 7 30,40 8"
                } else {
                    "twin restore_at 7 30,40 8"
                }
            );
        }
    }
}
