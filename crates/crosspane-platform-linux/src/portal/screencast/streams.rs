//! Which portal stream shows which display (pure).
//!
//! The portal reports, for each monitor stream, its PipeWire node and the monitor's position and
//! size in the compositor's logical space. A display of the displays snapshot belongs to a stream
//! when its `logical_origin` equals the position and its logical size (`pixel_size / scale`,
//! rounded) equals the size. The pairing must be one-to-one: a display that matches no stream or
//! several, and a stream that matches several displays (mirrored outputs), is unmapped. Never
//! guessed.

use crosspane_types::display::DisplayInfo;
use crosspane_types::id::DisplayId;

/// One monitor stream the portal granted: its PipeWire node and geometry in logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PortalStream {
    pub node_id: u32,
    pub position: (i32, i32),
    pub size: (i32, i32),
}

impl PortalStream {
    /// A stream from what the portal reported. A stream without a position and size (not a
    /// monitor), or with a non-positive size, cannot be matched to a display and is dropped.
    pub(super) fn from_portal(
        node_id: u32,
        position: Option<(i32, i32)>,
        size: Option<(i32, i32)>,
    ) -> Option<PortalStream> {
        let (position, size) = (position?, size?);
        (size.0 > 0 && size.1 > 0).then_some(PortalStream {
            node_id,
            position,
            size,
        })
    }
}

/// Why a display has no stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unmapped {
    /// The display is not in the snapshot.
    NoDisplay,
    /// No granted stream has this display's geometry (the user did not share it, or it moved or
    /// changed size since the session started).
    NoStream,
    /// More than one stream or display has the same geometry.
    Ambiguous,
}

/// The display's size in logical units, as the portal reports it.
fn logical_size(display: &DisplayInfo) -> Option<(i32, i32)> {
    let scale = display.geometry.scale;
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let width = (f64::from(display.geometry.pixel_size.width) / scale).round();
    let height = (f64::from(display.geometry.pixel_size.height) / scale).round();
    let fits = |v: f64| (1.0..=f64::from(i32::MAX)).contains(&v);
    (fits(width) && fits(height)).then_some((width as i32, height as i32))
}

fn same_coordinate(origin: f64, position: i32) -> bool {
    origin.is_finite() && (origin - f64::from(position)).abs() < 1e-6
}

/// Whether `stream` and `display` have the same logical geometry.
fn matches(stream: &PortalStream, display: &DisplayInfo) -> bool {
    let origin = display.geometry.logical_origin;
    same_coordinate(origin.x, stream.position.0)
        && same_coordinate(origin.y, stream.position.1)
        && logical_size(display) == Some(stream.size)
}

/// The stream of display `wanted`.
pub(super) fn stream_for(
    streams: &[PortalStream],
    displays: &[DisplayInfo],
    wanted: DisplayId,
) -> Result<PortalStream, Unmapped> {
    let mut found = displays.iter().filter(|display| display.id == wanted);
    let display = found.next().ok_or(Unmapped::NoDisplay)?;
    if found.next().is_some() {
        return Err(Unmapped::Ambiguous);
    }
    let mut candidates = streams.iter().filter(|stream| matches(stream, display));
    let stream = candidates.next().ok_or(Unmapped::NoStream)?;
    if candidates.next().is_some() {
        return Err(Unmapped::Ambiguous);
    }
    // The stream must not fit a second display either (mirrored outputs).
    if displays
        .iter()
        .filter(|other| matches(stream, other))
        .count()
        != 1
    {
        return Err(Unmapped::Ambiguous);
    }
    Ok(*stream)
}

/// How many displays of the snapshot have a stream.
pub(super) fn mapped_count(streams: &[PortalStream], displays: &[DisplayInfo]) -> usize {
    displays
        .iter()
        .filter(|display| stream_for(streams, displays, display.id).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};

    fn display(id: u32, origin: (f64, f64), pixels: (u32, u32), scale: f64) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("DP-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(pixels.0, pixels.1),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn stream(node_id: u32, position: (i32, i32), size: (i32, i32)) -> PortalStream {
        PortalStream {
            node_id,
            position,
            size,
        }
    }

    #[test]
    fn matches_by_origin_and_logical_size() {
        let displays = [
            display(1, (0.0, 0.0), (3440, 1440), 1.0),
            display(2, (3440.0, -200.0), (2560, 1600), 2.0),
        ];
        let streams = [
            stream(70, (3440, -200), (1280, 800)),
            stream(69, (0, 0), (3440, 1440)),
        ];
        assert_eq!(
            stream_for(&streams, &displays, DisplayId(1)).map(|s| s.node_id),
            Ok(69)
        );
        assert_eq!(
            stream_for(&streams, &displays, DisplayId(2)).map(|s| s.node_id),
            Ok(70)
        );
        assert_eq!(mapped_count(&streams, &displays), 2);
    }

    #[test]
    fn fractional_scales_use_the_rounded_logical_size() {
        // 2560 / 1.5 = 1706.67 -> 1707; 1600 / 1.5 = 1066.67 -> 1067.
        let displays = [display(1, (0.0, 0.0), (2560, 1600), 1.5)];
        let streams = [stream(5, (0, 0), (1707, 1067))];
        assert!(stream_for(&streams, &displays, DisplayId(1)).is_ok());
        let off_by_one = [stream(5, (0, 0), (1706, 1067))];
        assert_eq!(
            stream_for(&off_by_one, &displays, DisplayId(1)),
            Err(Unmapped::NoStream)
        );
    }

    #[test]
    fn unshared_or_moved_displays_are_unmapped() {
        let displays = [
            display(1, (0.0, 0.0), (1920, 1080), 1.0),
            display(2, (1920.0, 0.0), (1920, 1080), 1.0),
        ];
        let streams = [stream(9, (0, 0), (1920, 1080))];
        assert_eq!(
            stream_for(&streams, &displays, DisplayId(2)),
            Err(Unmapped::NoStream)
        );
        assert_eq!(
            stream_for(&streams, &displays, DisplayId(3)),
            Err(Unmapped::NoDisplay)
        );
        assert_eq!(mapped_count(&streams, &displays), 1);
        // The monitor was moved after the session started: its stream no longer matches.
        let moved = [display(1, (10.0, 0.0), (1920, 1080), 1.0)];
        assert_eq!(
            stream_for(&streams, &moved, DisplayId(1)),
            Err(Unmapped::NoStream)
        );
        assert_eq!(mapped_count(&streams, &[]), 0);
    }

    #[test]
    fn duplicated_geometry_is_never_guessed() {
        // Two streams with the same geometry.
        let displays = [display(1, (0.0, 0.0), (1920, 1080), 1.0)];
        let twins = [
            stream(1, (0, 0), (1920, 1080)),
            stream(2, (0, 0), (1920, 1080)),
        ];
        assert_eq!(
            stream_for(&twins, &displays, DisplayId(1)),
            Err(Unmapped::Ambiguous)
        );
        // Two mirrored displays and one stream.
        let mirrored = [
            display(1, (0.0, 0.0), (1920, 1080), 1.0),
            display(2, (0.0, 0.0), (1920, 1080), 1.0),
        ];
        let one = [stream(1, (0, 0), (1920, 1080))];
        assert_eq!(
            stream_for(&one, &mirrored, DisplayId(1)),
            Err(Unmapped::Ambiguous)
        );
        assert_eq!(
            stream_for(&one, &mirrored, DisplayId(2)),
            Err(Unmapped::Ambiguous)
        );
        assert_eq!(mapped_count(&one, &mirrored), 0);
        // The same id twice in a snapshot.
        let same_id = [
            display(1, (0.0, 0.0), (1920, 1080), 1.0),
            display(1, (1920.0, 0.0), (1920, 1080), 1.0),
        ];
        assert_eq!(
            stream_for(&one, &same_id, DisplayId(1)),
            Err(Unmapped::Ambiguous)
        );
    }

    #[test]
    fn only_streams_with_a_geometry_are_kept() {
        assert_eq!(
            PortalStream::from_portal(3, Some((10, -5)), Some((800, 600))),
            Some(stream(3, (10, -5), (800, 600)))
        );
        assert_eq!(PortalStream::from_portal(3, None, Some((800, 600))), None);
        assert_eq!(PortalStream::from_portal(3, Some((0, 0)), None), None);
        assert_eq!(
            PortalStream::from_portal(3, Some((0, 0)), Some((0, 600))),
            None
        );
        assert_eq!(
            PortalStream::from_portal(3, Some((0, 0)), Some((800, -1))),
            None
        );
    }

    #[test]
    fn nonsense_scales_match_nothing() {
        let streams = [stream(1, (0, 0), (1920, 1080))];
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let displays = [display(1, (0.0, 0.0), (1920, 1080), scale)];
            assert_eq!(
                stream_for(&streams, &displays, DisplayId(1)),
                Err(Unmapped::NoStream),
                "scale {scale}"
            );
        }
    }
}
