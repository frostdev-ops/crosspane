//! Minimal public CoreAudio output ABI; SDK C/Rust verification belongs to WP-4.16c.
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
use std::ffi::c_void;
pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type AudioDeviceIOProc = unsafe extern "C" fn(
    AudioObjectID,
    *const c_void,
    *const AudioBufferList,
    *const c_void,
    *mut AudioBufferList,
    *const c_void,
    *mut c_void,
) -> OSStatus;
pub type AudioDeviceIOProcID = Option<AudioDeviceIOProc>;
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: u32,
    pub mScope: u32,
    pub mElement: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}
pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}
pub const kAudioObjectSystemObject: u32 = 1;
pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioHardwarePropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
pub const kAudioDevicePropertyIsHidden: u32 = fourcc(b"hidn");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioDeviceClassID: u32 = fourcc(b"adev");
pub const kAudioDevicePropertyTransportType: u32 = fourcc(b"tran");
pub const kAudioDeviceTransportTypeVirtual: u32 = fourcc(b"virt");
pub const kAudioStreamPropertyVirtualFormat: u32 = fourcc(b"sfmt");
pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagsNativeFloatPacked: u32 = (1 << 0) | (1 << 3);
#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    pub fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    pub fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
    pub fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client_data: *mut c_void,
        out_proc_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;
    pub fn AudioDeviceStart(device: AudioObjectID, proc_id: AudioDeviceIOProc) -> OSStatus;
    pub fn AudioDeviceStop(device: AudioObjectID, proc_id: AudioDeviceIOProc) -> OSStatus;
    pub fn AudioDeviceDestroyIOProcID(
        device: AudioObjectID,
        proc_id: AudioDeviceIOProc,
    ) -> OSStatus;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn public_sdk_abi() {
        macro_rules! layout {
            ($ty:ty, $size:expr, $align:expr, $($field:ident: $offset:expr),+ $(,)?) => {
                assert_eq!(size_of::<$ty>(), $size, stringify!($ty));
                assert_eq!(align_of::<$ty>(), $align, stringify!($ty));
                $(assert_eq!(offset_of!($ty, $field), $offset, stringify!($field));)+
            };
        }
        assert_eq!((size_of::<OSStatus>(), align_of::<OSStatus>()), (4, 4));
        assert_eq!(
            (size_of::<AudioObjectID>(), align_of::<AudioObjectID>()),
            (4, 4)
        );
        let _: OSStatus = -1_i32;
        let _: AudioObjectID = 0_u32;
        layout!(AudioObjectPropertyAddress, 12, 4, mSelector: 0, mScope: 4, mElement: 8);
        layout!(AudioBuffer, 16, 8, mNumberChannels: 0, mDataByteSize: 4, mData: 8);
        layout!(AudioBufferList, 24, 8, mNumberBuffers: 0, mBuffers: 8);
        layout!(AudioStreamBasicDescription, 40, 8,
            mSampleRate: 0, mFormatID: 8, mFormatFlags: 12, mBytesPerPacket: 16,
            mFramesPerPacket: 20, mBytesPerFrame: 24, mChannelsPerFrame: 28,
            mBitsPerChannel: 32, mReserved: 36);
        assert_eq!(
            (
                size_of::<AudioDeviceIOProc>(),
                align_of::<AudioDeviceIOProc>()
            ),
            (8, 8)
        );
        assert_eq!(
            (
                size_of::<AudioDeviceIOProcID>(),
                align_of::<AudioDeviceIOProcID>()
            ),
            (8, 8)
        );
        // Constructors type-check actual field widths and the flexible-array prefix.
        let address = AudioObjectPropertyAddress {
            mSelector: 0_u32,
            mScope: 0_u32,
            mElement: 0_u32,
        };
        let buffer = AudioBuffer {
            mNumberChannels: 0_u32,
            mDataByteSize: 0_u32,
            mData: std::ptr::null_mut::<c_void>(),
        };
        let _ = (
            address,
            AudioBufferList {
                mNumberBuffers: 0_u32,
                mBuffers: [buffer],
            },
        );
        let _ = AudioStreamBasicDescription {
            mSampleRate: 0_f64,
            mFormatID: 0_u32,
            mFormatFlags: 0_u32,
            mBytesPerPacket: 0_u32,
            mFramesPerPacket: 0_u32,
            mBytesPerFrame: 0_u32,
            mChannelsPerFrame: 0_u32,
            mBitsPerChannel: 0_u32,
            mReserved: 0_u32,
        };
        for (actual, expected) in [
            (kAudioObjectSystemObject, 1),
            (kAudioObjectPropertyScopeGlobal, 0x676c6f62),
            (kAudioObjectPropertyScopeInput, 0x696e7074),
            (kAudioObjectPropertyScopeOutput, 0x6f757470),
            (kAudioHardwarePropertyTranslateUIDToDevice, 0x75696464),
            (kAudioDevicePropertyDeviceUID, 0x75696420),
            (kAudioDevicePropertyDeviceIsAlive, 0x6c69766e),
            (kAudioDevicePropertyIsHidden, 0x6869646e),
            (kAudioDevicePropertyStreams, 0x73746d23),
            (kAudioDevicePropertyNominalSampleRate, 0x6e737274),
            (kAudioObjectPropertyClass, 0x636c6173),
            (kAudioDeviceClassID, 0x61646576),
            (kAudioDevicePropertyTransportType, 0x7472616e),
            (kAudioDeviceTransportTypeVirtual, 0x76697274),
            (kAudioStreamPropertyVirtualFormat, 0x73666d74),
            (kAudioFormatLinearPCM, 0x6c70636d),
            (kAudioFormatFlagsNativeFloatPacked, 9),
        ] {
            assert_eq!(actual, expected);
        }
        // Function-item assignments check actual declarations without calling SDK functions.
        let _: unsafe extern "C" fn(
            AudioObjectID,
            *const AudioObjectPropertyAddress,
            u32,
            *const c_void,
            *mut u32,
        ) -> OSStatus = AudioObjectGetPropertyDataSize;
        let _: unsafe extern "C" fn(
            AudioObjectID,
            *const AudioObjectPropertyAddress,
            u32,
            *const c_void,
            *mut u32,
            *mut c_void,
        ) -> OSStatus = AudioObjectGetPropertyData;
        let _: unsafe extern "C" fn(
            AudioObjectID,
            AudioDeviceIOProc,
            *mut c_void,
            *mut AudioDeviceIOProcID,
        ) -> OSStatus = AudioDeviceCreateIOProcID;
        let _: [unsafe extern "C" fn(AudioObjectID, AudioDeviceIOProc) -> OSStatus; 3] = [
            AudioDeviceDestroyIOProcID,
            AudioDeviceStart,
            AudioDeviceStop,
        ];
        let _: Option<
            unsafe extern "C" fn(
                AudioObjectID,
                *const c_void,
                *const AudioBufferList,
                *const c_void,
                *mut AudioBufferList,
                *const c_void,
                *mut c_void,
            ) -> OSStatus,
        > = None::<AudioDeviceIOProc>;
        let _: AudioDeviceIOProcID = None::<AudioDeviceIOProc>;
    }
}
