//! What the virtual screen's one PipeWire stream asks for (pure pod building).
//!
//! Mutter creates the virtual monitor when the consumer negotiates a format and gives it the
//! negotiated size, so the consumer decides the monitor's size by offering **exactly one size**
//! (a `Rectangle` value, not a range) in the pixel layouts monitor capture already reads (BGRx,
//! BGRA, RGBx, RGBA, xRGB; CPU memory only, no `VideoModifier`). Everything but the size is the
//! monitor capture's `EnumFormat` (`screencast::format`), so the two cannot drift apart; the
//! `Buffers` and `Meta` answers are the monitor capture's too.

use crosspane_types::geom::PixelSize;

use crate::portal::screencast::format::{self, FormatError, OfferedSize};

/// The frame rate offered as the maximum. A Mutter virtual monitor runs at 60 Hz; the callers'
/// own `max_fps` is applied per capture, after the stream.
pub(super) const OFFERED_MAX_FPS: u32 = 60;

/// The `EnumFormat` pod offering exactly `size`. [`FormatError::BadSize`] for a size the capture
/// does not read (zero, wider than 16384, or more than 2^26 pixels).
pub(super) fn enum_format(size: PixelSize) -> Result<Vec<u8>, FormatError> {
    format::enum_format_sized(OFFERED_MAX_FPS, OfferedSize::Fixed(size))
}

/// Whether a virtual screen of this size can be asked for.
pub(super) fn size_supported(size: PixelSize) -> bool {
    format::size_supported(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::spa;
    use spa::param::ParamType;
    use spa::param::format::FormatProperties;
    use spa::param::video::VideoFormat;
    use spa::pod::deserialize::PodDeserializer;
    use spa::pod::{ChoiceValue, Object, Value};
    use spa::utils::{Choice, ChoiceEnum, Id, Rectangle, SpaTypes};

    fn object_of(bytes: &[u8]) -> Object {
        match PodDeserializer::deserialize_any_from(bytes) {
            Ok((_, Value::Object(object))) => object,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn property(object: &Object, key: FormatProperties) -> Option<&Value> {
        object
            .properties
            .iter()
            .find(|p| p.key == key.as_raw())
            .map(|p| &p.value)
    }

    #[test]
    fn offers_exactly_one_size_as_a_plain_value() {
        for (width, height) in [(1800, 1169), (64, 64), (3440, 1440), (8192, 8192)] {
            let object = object_of(&enum_format(PixelSize::new(width, height)).unwrap());
            assert_eq!(object.type_, SpaTypes::ObjectParamFormat.as_raw());
            assert_eq!(object.id, ParamType::EnumFormat.as_raw());
            // A fixed Rectangle, not a Choice: nothing for the producer to pick from.
            assert_eq!(
                property(&object, FormatProperties::VideoSize),
                Some(&Value::Rectangle(Rectangle { width, height })),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn offers_the_pixel_layouts_monitor_capture_reads_and_no_dmabuf() {
        let object = object_of(&enum_format(PixelSize::new(800, 600)).unwrap());
        let Some(Value::Choice(ChoiceValue::Id(Choice(
            _,
            ChoiceEnum::Enum {
                default,
                alternatives,
            },
        )))) = property(&object, FormatProperties::VideoFormat)
        else {
            panic!("format is not an id enum");
        };
        assert_eq!(*default, Id(VideoFormat::BGRx.as_raw()));
        let offered: Vec<_> = alternatives
            .iter()
            .map(|id| format::pixel_format(VideoFormat::from_raw(id.0)).is_some())
            .collect();
        assert_eq!(offered, vec![true; 5], "five layouts, all readable");
        assert!(property(&object, FormatProperties::VideoModifier).is_none());
    }

    #[test]
    fn differs_from_the_monitor_format_only_in_the_size_and_the_rate_cap() {
        let fixed = object_of(&enum_format(PixelSize::new(800, 600)).unwrap());
        let any = object_of(&format::enum_format(OFFERED_MAX_FPS).unwrap());
        assert_eq!(fixed.properties.len(), any.properties.len());
        for (a, b) in fixed.properties.iter().zip(&any.properties) {
            assert_eq!(a.key, b.key);
            if a.key == FormatProperties::VideoSize.as_raw() {
                assert_ne!(a.value, b.value);
            } else {
                assert_eq!(a.value, b.value, "property {}", a.key);
            }
        }
    }

    #[test]
    fn carries_the_offered_maximum_frame_rate() {
        let object = object_of(&enum_format(PixelSize::new(800, 600)).unwrap());
        let Some(Value::Choice(ChoiceValue::Fraction(Choice(
            _,
            ChoiceEnum::Range { default, .. },
        )))) = property(&object, FormatProperties::VideoMaxFramerate)
        else {
            panic!("max framerate is not a fraction range");
        };
        assert_eq!((default.num, default.denom), (OFFERED_MAX_FPS, 1));
    }

    #[test]
    fn sizes_the_capture_cannot_read_are_refused() {
        let max = format::MAX_DIMENSION;
        for (width, height) in [
            (0, 600),
            (800, 0),
            (0, 0),
            (max + 1, 10),
            (10, max + 1),
            // Within the dimensions, over the pixel budget (2^26).
            (max, max),
            (8193, 8192),
        ] {
            let size = PixelSize::new(width, height);
            assert!(!size_supported(size), "{width}x{height}");
            assert_eq!(
                enum_format(size),
                Err(FormatError::BadSize),
                "{width}x{height}"
            );
        }
        assert!(size_supported(PixelSize::new(8192, 8192)));
        assert!(size_supported(PixelSize::new(1, 1)));
    }

    #[test]
    fn the_buffer_and_meta_answers_are_the_monitor_captures() {
        // Memory-backed buffers only, plus the header and damage metadata, for every format.
        assert!(format::buffers_param().is_ok());
        assert_eq!(format::meta_params().map(|pods| pods.len()), Ok(2));
    }
}
