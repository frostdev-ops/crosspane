//! What the capture stream asks PipeWire for and how it reads the answer (pure pod building and
//! parsing).
//!
//! - **Formats** (`EnumFormat`): raw video in BGRx, BGRA, RGBx, RGBA or xRGB, any size up to
//!   16384 × 16384, a variable frame rate with the caller's `max_fps` as the maximum. No
//!   `VideoModifier` property is offered, so a producer can only answer with its CPU (SHM) variant,
//!   never a DMA-BUF.
//! - **Buffers**: memory-backed only (`MemFd` or `MemPtr`; `dataType` mask 6), one block, 2 to 32
//!   buffers.
//! - **Metadata** the stream can use: the buffer header (timestamp, flags) and video damage.
//!   Neither is required; a producer that sends none gets whole-frame damage.
//!
//! The cursor is never requested (the session uses `CursorMode::Hidden`), so no cursor metadata is
//! negotiated and none is read.

use std::io::Cursor;

use crosspane_types::geom::PixelSize;
use pipewire::spa;
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::format_utils::parse_format;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::serialize::PodSerializer;
use spa::pod::{self, ChoiceValue, Object, Pod, Property, Value};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use super::pixels::{MAX_PIXELS, PixelFormat};

/// Largest width or height offered.
pub(in crate::portal) const MAX_DIMENSION: u32 = 16384;
/// Highest frame rate offered as a maximum.
const MAX_FRAMERATE: u32 = 1000;
/// `SPA_DATA_MemFd` and `SPA_DATA_MemPtr` as a `dataType` bit mask. DMA-BUF is left out.
const MEMORY_DATA_TYPES: i32 = (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr);
/// Number of damage regions the producer may send per buffer.
const DAMAGE_REGIONS: i32 = 16;
/// Bytes of one `spa_meta_region`.
const REGION_BYTES: i32 = size_of::<spa::sys::spa_meta_region>() as i32;

/// The format a stream settled on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::portal) struct Negotiated {
    pub format: PixelFormat,
    pub size: PixelSize,
}

/// Why a pod could not be built or a negotiated format is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::portal) enum FormatError {
    /// Serializing a pod failed.
    Build,
    /// The pod is not raw video.
    NotVideoRaw,
    /// The pod could not be parsed as a raw video format.
    Unparseable,
    /// A pixel format that was not offered.
    UnsupportedFormat,
    /// Zero, or beyond the supported dimensions.
    BadSize,
}

/// The pixel layout of an SPA video format, for the formats this module offers.
pub(in crate::portal) fn pixel_format(format: VideoFormat) -> Option<PixelFormat> {
    match format {
        VideoFormat::BGRx => Some(PixelFormat::Bgrx),
        VideoFormat::BGRA => Some(PixelFormat::Bgra),
        VideoFormat::RGBx => Some(PixelFormat::Rgbx),
        VideoFormat::RGBA => Some(PixelFormat::Rgba),
        VideoFormat::xRGB => Some(PixelFormat::Xrgb),
        _ => None,
    }
}

fn serialize(object: Object) -> Result<Vec<u8>, FormatError> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| FormatError::Build)
}

/// The video sizes an `EnumFormat` offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::portal) enum OfferedSize {
    /// Any size up to [`MAX_DIMENSION`] (monitor capture takes whatever the compositor streams).
    Any,
    /// Exactly this size and no other: the consumer of a virtual monitor decides its size
    /// (`portal::virtual_screen`).
    Fixed(PixelSize),
}

/// Whether a stream of this size is one this module reads: both sides within `1..=MAX_DIMENSION`
/// and at most `MAX_PIXELS` pixels.
pub(in crate::portal) fn size_supported(size: PixelSize) -> bool {
    let in_range = |v: u32| (1..=MAX_DIMENSION).contains(&v);
    in_range(size.width)
        && in_range(size.height)
        && u64::from(size.width) * u64::from(size.height) <= MAX_PIXELS
}

/// The `EnumFormat` pod for a stream limited to `max_fps` frames per second (1 to 1000).
pub(in crate::portal) fn enum_format(max_fps: u32) -> Result<Vec<u8>, FormatError> {
    enum_format_sized(max_fps, OfferedSize::Any)
}

/// [`enum_format`] with the offered sizes chosen. A `Fixed` size outside [`size_supported`] is
/// [`FormatError::BadSize`].
pub(in crate::portal) fn enum_format_sized(
    max_fps: u32,
    sizes: OfferedSize,
) -> Result<Vec<u8>, FormatError> {
    let max_fps = i32::try_from(max_fps.clamp(1, MAX_FRAMERATE)).unwrap_or(1);
    let size = match sizes {
        OfferedSize::Any => pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: MAX_DIMENSION,
                height: MAX_DIMENSION
            }
        ),
        OfferedSize::Fixed(size) if size_supported(size) => pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.width,
                height: size.height
            }
        ),
        OfferedSize::Fixed(_) => return Err(FormatError::BadSize),
    };
    serialize(pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::xRGB
        ),
        size,
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: MAX_FRAMERATE,
                denom: 1
            }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: max_fps as u32,
                denom: 1
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: MAX_FRAMERATE,
                denom: 1
            }
        ),
    ))
}

/// The `Buffers` pod answering a negotiated format: memory-backed buffers only.
pub(in crate::portal) fn buffers_param() -> Result<Vec<u8>, FormatError> {
    serialize(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 32,
                    },
                ))),
            ),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            // A flags choice with the mask as its only value, as `SPA_POD_CHOICE_FLAGS_Int`
            // builds it; it intersects with the producer's flags (or plain int) by bitwise and.
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: MEMORY_DATA_TYPES,
                        flags: Vec::new(),
                    },
                ))),
            ),
        ],
    })
}

/// The `Meta` pods for the buffer header and for video damage.
pub(in crate::portal) fn meta_params() -> Result<Vec<Vec<u8>>, FormatError> {
    let header = serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    })?;
    let damage = serialize(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_VideoDamage)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: DAMAGE_REGIONS * REGION_BYTES,
                        min: REGION_BYTES,
                        max: DAMAGE_REGIONS * REGION_BYTES,
                    },
                ))),
            ),
        ],
    })?;
    Ok(vec![header, damage])
}

/// Read the `Format` the stream settled on.
pub(in crate::portal) fn parse_negotiated(format: &Pod) -> Result<Negotiated, FormatError> {
    let (media_type, media_subtype) = parse_format(format).map_err(|_| FormatError::NotVideoRaw)?;
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return Err(FormatError::NotVideoRaw);
    }
    let mut info = VideoInfoRaw::new();
    info.parse(format).map_err(|_| FormatError::Unparseable)?;
    let pixel = pixel_format(info.format()).ok_or(FormatError::UnsupportedFormat)?;
    let size = PixelSize::new(info.size().width, info.size().height);
    if !size_supported(size) {
        return Err(FormatError::BadSize);
    }
    Ok(Negotiated {
        format: pixel,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spa::pod::deserialize::PodDeserializer;

    fn object_of(bytes: &[u8]) -> Object {
        match PodDeserializer::deserialize_any_from(bytes) {
            Ok((_, Value::Object(object))) => object,
            other => panic!("not an object: {other:?}"),
        }
    }

    fn property(object: &Object, key: u32) -> &Value {
        &object
            .properties
            .iter()
            .find(|p| p.key == key)
            .unwrap_or_else(|| panic!("no property {key}"))
            .value
    }

    /// A fixed `Format` object as a producer would answer.
    fn answer(media: MediaType, format: VideoFormat, width: u32, height: u32) -> Vec<u8> {
        serialize(pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            pod::property!(FormatProperties::MediaType, Id, media),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(FormatProperties::VideoFormat, Id, format),
            pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle { width, height }
            ),
            pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                Fraction { num: 0, denom: 1 }
            ),
        ))
        .unwrap()
    }

    fn negotiated(bytes: &[u8]) -> Result<Negotiated, FormatError> {
        parse_negotiated(Pod::from_bytes(bytes).unwrap())
    }

    #[test]
    fn enum_format_offers_cpu_formats_only() {
        let object = object_of(&enum_format(30).unwrap());
        assert_eq!(object.type_, SpaTypes::ObjectParamFormat.as_raw());
        assert_eq!(object.id, ParamType::EnumFormat.as_raw());
        let Value::Choice(ChoiceValue::Id(Choice(
            _,
            ChoiceEnum::Enum {
                default,
                alternatives,
            },
        ))) = property(&object, FormatProperties::VideoFormat.as_raw())
        else {
            panic!("format is not an id enum");
        };
        assert_eq!(*default, Id(VideoFormat::BGRx.as_raw()));
        for format in [
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
            VideoFormat::xRGB,
        ] {
            assert!(alternatives.contains(&Id(format.as_raw())), "{format:?}");
            assert!(pixel_format(format).is_some(), "{format:?}");
        }
        assert_eq!(alternatives.len(), 5);
        // No modifier means no DMA-BUF variants.
        assert!(
            !object
                .properties
                .iter()
                .any(|p| p.key == FormatProperties::VideoModifier.as_raw())
        );
        let Value::Choice(ChoiceValue::Rectangle(Choice(_, ChoiceEnum::Range { min, max, .. }))) =
            property(&object, FormatProperties::VideoSize.as_raw())
        else {
            panic!("size is not a rectangle range");
        };
        assert_eq!((min.width, min.height), (1, 1));
        assert_eq!((max.width, max.height), (MAX_DIMENSION, MAX_DIMENSION));
    }

    #[test]
    fn enum_format_carries_the_maximum_frame_rate() {
        for (asked, offered) in [(30, 30), (0, 1), (165, 165), (u32::MAX, MAX_FRAMERATE)] {
            let object = object_of(&enum_format(asked).unwrap());
            let Value::Choice(ChoiceValue::Fraction(Choice(
                _,
                ChoiceEnum::Range { default, min, max },
            ))) = property(&object, FormatProperties::VideoMaxFramerate.as_raw())
            else {
                panic!("max framerate is not a fraction range");
            };
            assert_eq!(default.num, offered, "asked {asked}");
            assert_eq!(default.denom, 1);
            assert_eq!((min.num, max.num), (1, MAX_FRAMERATE));
        }
        // The frame rate itself may be variable (0/1), as compositors offer it.
        let object = object_of(&enum_format(30).unwrap());
        let Value::Choice(ChoiceValue::Fraction(Choice(_, ChoiceEnum::Range { default, min, .. }))) =
            property(&object, FormatProperties::VideoFramerate.as_raw())
        else {
            panic!("framerate is not a fraction range");
        };
        assert_eq!((default.num, min.num), (0, 0));
    }

    #[test]
    fn buffers_ask_for_memory_only() {
        let object = object_of(&buffers_param().unwrap());
        assert_eq!(object.type_, SpaTypes::ObjectParamBuffers.as_raw());
        assert_eq!(object.id, ParamType::Buffers.as_raw());
        let Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Flags { default, flags }))) =
            property(&object, spa::sys::SPA_PARAM_BUFFERS_dataType)
        else {
            panic!("dataType is not a flags choice");
        };
        // MemFd (2) and MemPtr (1): bits 1 and 2. DmaBuf (3) is not in the mask.
        assert_eq!(*default, 0b110);
        assert_eq!(*default & (1 << spa::sys::SPA_DATA_DmaBuf), 0);
        assert!(flags.is_empty());
        assert_eq!(
            property(&object, spa::sys::SPA_PARAM_BUFFERS_blocks),
            &Value::Int(1)
        );
        let Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Range { default, min, max }))) =
            property(&object, spa::sys::SPA_PARAM_BUFFERS_buffers)
        else {
            panic!("buffers is not a range");
        };
        assert!(*min >= 2 && min <= default && default <= max);
    }

    #[test]
    fn metadata_requests_header_and_damage() {
        let pods = meta_params().unwrap();
        assert_eq!(pods.len(), 2);
        let wanted = [spa::sys::SPA_META_Header, spa::sys::SPA_META_VideoDamage];
        for (bytes, kind) in pods.iter().zip(wanted) {
            let object = object_of(bytes);
            assert_eq!(object.type_, SpaTypes::ObjectParamMeta.as_raw());
            assert_eq!(object.id, ParamType::Meta.as_raw());
            assert_eq!(
                property(&object, spa::sys::SPA_PARAM_META_type),
                &Value::Id(Id(kind))
            );
        }
        let damage = object_of(&pods[1]);
        let Value::Choice(ChoiceValue::Int(Choice(_, ChoiceEnum::Range { default, min, max }))) =
            property(&damage, spa::sys::SPA_PARAM_META_size)
        else {
            panic!("damage size is not a range");
        };
        assert_eq!((*min, *default, *max), (16, 256, 256));
    }

    #[test]
    fn a_negotiated_format_is_read() {
        for (format, pixel) in [
            (VideoFormat::BGRx, PixelFormat::Bgrx),
            (VideoFormat::BGRA, PixelFormat::Bgra),
            (VideoFormat::RGBx, PixelFormat::Rgbx),
            (VideoFormat::RGBA, PixelFormat::Rgba),
            (VideoFormat::xRGB, PixelFormat::Xrgb),
        ] {
            let bytes = answer(MediaType::Video, format, 3440, 1440);
            assert_eq!(
                negotiated(&bytes),
                Ok(Negotiated {
                    format: pixel,
                    size: PixelSize::new(3440, 1440)
                })
            );
        }
    }

    #[test]
    fn unusable_answers_are_refused() {
        let video = |format, w, h| answer(MediaType::Video, format, w, h);
        // A layout that was never offered.
        assert_eq!(
            negotiated(&video(VideoFormat::NV12, 1920, 1080)),
            Err(FormatError::UnsupportedFormat)
        );
        assert_eq!(
            negotiated(&video(VideoFormat::RGB, 1920, 1080)),
            Err(FormatError::UnsupportedFormat)
        );
        // Sizes outside what is offered.
        for (w, h) in [
            (0, 1080),
            (1920, 0),
            (MAX_DIMENSION + 1, 10),
            (16384, 16384),
        ] {
            assert_eq!(
                negotiated(&video(VideoFormat::BGRx, w, h)),
                Err(FormatError::BadSize),
                "{w}x{h}"
            );
        }
        // Not video at all.
        assert_eq!(
            negotiated(&answer(MediaType::Audio, VideoFormat::BGRx, 1920, 1080)),
            Err(FormatError::NotVideoRaw)
        );
    }
}
