//! Public CoreAudio declarations (`CoreAudio/AudioHardware.h`, `AudioHardwareBase.h`,
//! `CoreAudioBaseTypes.h`) used by the native HAL. Declared here by hand because the workspace
//! has no direct CoreAudio binding crate; every prototype and layout below is the exact public SDK
//! ABI, checked three ways: `const` layout assertions in this file, the `abi_report` below (compared
//! with a real C compiler's `sizeof`/`offsetof`/constants by `tests/audio.rs`), and the SDK header
//! text itself (macOS 27 SDK, Xcode 27.0).
//!
//! Only public APIs are declared: `AudioObjectGetPropertyData[Size]`, the block-based
//! `AudioObject{Add,Remove}PropertyListenerBlock`, and `AudioDeviceCreateIOProcID`/`Start`/`Stop`/
//! `DestroyIOProcID`. Nothing here sets a property, changes a default device, or reads an input
//! device that is not named by an exact UID.

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

use std::ffi::c_void;
use std::mem::{offset_of, size_of};

use block2::DynBlock;
use dispatch2::DispatchQueue;

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type AudioDeviceID = u32;
pub type AudioStreamID = u32;
pub type AudioClassID = u32;

/// `AudioDeviceIOProc`, as passed to `AudioDeviceCreateIOProcID`. The timestamp arguments are
/// opaque to the host (it never reads them), so they are declared as plain pointers.
pub type AudioDeviceIOProc = unsafe extern "C" fn(
    device: AudioObjectID,
    now: *const c_void,
    input: *const AudioBufferList,
    input_time: *const c_void,
    output: *mut AudioBufferList,
    output_time: *const c_void,
    client_data: *mut c_void,
) -> OSStatus;

/// `AudioDeviceIOProcID` is the same function-pointer type (a typedef of `AudioDeviceIOProc`).
pub type AudioDeviceIOProcID = Option<AudioDeviceIOProc>;

/// Build a CoreAudio four-character code.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

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

/// `mBuffers` is a C flexible array: `mNumberBuffers` entries start at its address.
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

pub const kAudioObjectUnknown: AudioObjectID = 0;
pub const kAudioObjectSystemObject: AudioObjectID = 1;

pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioObjectPropertyElementMain: u32 = 0;

pub const kAudioHardwarePropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioHardwarePropertyDefaultOutputDevice: u32 = fourcc(b"dOut");
pub const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
pub const kAudioHardwarePropertyServiceRestarted: u32 = fourcc(b"srst");

pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioDeviceClassID: u32 = fourcc(b"adev");
pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyTransportType: u32 = fourcc(b"tran");
pub const kAudioDeviceTransportTypeVirtual: u32 = fourcc(b"virt");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
pub const kAudioDevicePropertyDeviceIsRunningSomewhere: u32 = fourcc(b"gone");
pub const kAudioDevicePropertyIsHidden: u32 = fourcc(b"hidn");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");
pub const kAudioStreamPropertyVirtualFormat: u32 = fourcc(b"sfmt");

pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsBigEndian: u32 = 1 << 1;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;
pub const kAudioFormatFlagIsNonInterleaved: u32 = 1 << 5;
/// `kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked` on a little-endian host.
pub const kAudioFormatFlagsNativeFloatPacked: u32 =
    kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked;

pub const kAudioHardwareNoError: OSStatus = 0;
pub const kAudioHardwareBadObjectError: OSStatus = fourcc(b"!obj") as OSStatus;
pub const kAudioHardwareBadDeviceError: OSStatus = fourcc(b"!dev") as OSStatus;
pub const kAudioHardwareBadStreamError: OSStatus = fourcc(b"!str") as OSStatus;
pub const kAudioHardwareUnknownPropertyError: OSStatus = fourcc(b"who?") as OSStatus;
pub const kAudioHardwareBadPropertySizeError: OSStatus = fourcc(b"!siz") as OSStatus;
pub const kAudioDevicePermissionsError: OSStatus = fourcc(b"!hog") as OSStatus;

// Compile-time layout checks against the SDK (values from a C compiler on macOS 27, arm64).
const _: () = {
    assert!(size_of::<AudioObjectPropertyAddress>() == 12);
    assert!(offset_of!(AudioObjectPropertyAddress, mScope) == 4);
    assert!(offset_of!(AudioObjectPropertyAddress, mElement) == 8);
    assert!(size_of::<AudioBuffer>() == 16);
    assert!(offset_of!(AudioBuffer, mDataByteSize) == 4);
    assert!(offset_of!(AudioBuffer, mData) == 8);
    assert!(size_of::<AudioBufferList>() == 24);
    assert!(offset_of!(AudioBufferList, mBuffers) == 8);
    assert!(size_of::<AudioStreamBasicDescription>() == 40);
    assert!(offset_of!(AudioStreamBasicDescription, mFormatID) == 8);
    assert!(offset_of!(AudioStreamBasicDescription, mReserved) == 36);
    assert!(size_of::<AudioDeviceIOProcID>() == 8);
};

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

    /// The listener block is `void (^)(UInt32 inNumberAddresses, const AudioObjectPropertyAddress*)`.
    /// CoreAudio copies the block and retains the queue until the matching remove call.
    pub fn AudioObjectAddPropertyListenerBlock(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        queue: &DispatchQueue,
        listener: &DynBlock<dyn Fn(u32, *const c_void)>,
    ) -> OSStatus;

    pub fn AudioObjectRemovePropertyListenerBlock(
        object: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        queue: &DispatchQueue,
        listener: &DynBlock<dyn Fn(u32, *const c_void)>,
    ) -> OSStatus;

    pub fn AudioDeviceCreateIOProcID(
        device: AudioObjectID,
        proc_: AudioDeviceIOProc,
        client_data: *mut c_void,
        out_proc_id: *mut AudioDeviceIOProcID,
    ) -> OSStatus;

    pub fn AudioDeviceDestroyIOProcID(
        device: AudioObjectID,
        proc_id: AudioDeviceIOProc,
    ) -> OSStatus;

    pub fn AudioDeviceStart(device: AudioObjectID, proc_id: AudioDeviceIOProc) -> OSStatus;

    pub fn AudioDeviceStop(device: AudioObjectID, proc_id: AudioDeviceIOProc) -> OSStatus;
}

/// Every layout fact and constant this module relies on, keyed by a C expression that evaluates to
/// the SDK's value. `tests/audio.rs` generates a C program from the keys, compiles it with the
/// SDK's headers and compares the printed values with the ones returned here.
pub fn abi_report() -> Vec<(&'static str, u64)> {
    macro_rules! entry {
        ($name:literal, $value:expr) => {
            ($name, $value as u64)
        };
    }
    vec![
        entry!(
            "sizeof(AudioObjectPropertyAddress)",
            size_of::<AudioObjectPropertyAddress>()
        ),
        entry!(
            "offsetof(AudioObjectPropertyAddress, mSelector)",
            offset_of!(AudioObjectPropertyAddress, mSelector)
        ),
        entry!(
            "offsetof(AudioObjectPropertyAddress, mScope)",
            offset_of!(AudioObjectPropertyAddress, mScope)
        ),
        entry!(
            "offsetof(AudioObjectPropertyAddress, mElement)",
            offset_of!(AudioObjectPropertyAddress, mElement)
        ),
        entry!("sizeof(AudioBuffer)", size_of::<AudioBuffer>()),
        entry!(
            "offsetof(AudioBuffer, mNumberChannels)",
            offset_of!(AudioBuffer, mNumberChannels)
        ),
        entry!(
            "offsetof(AudioBuffer, mDataByteSize)",
            offset_of!(AudioBuffer, mDataByteSize)
        ),
        entry!(
            "offsetof(AudioBuffer, mData)",
            offset_of!(AudioBuffer, mData)
        ),
        entry!("sizeof(AudioBufferList)", size_of::<AudioBufferList>()),
        entry!(
            "offsetof(AudioBufferList, mNumberBuffers)",
            offset_of!(AudioBufferList, mNumberBuffers)
        ),
        entry!(
            "offsetof(AudioBufferList, mBuffers)",
            offset_of!(AudioBufferList, mBuffers)
        ),
        entry!(
            "sizeof(AudioStreamBasicDescription)",
            size_of::<AudioStreamBasicDescription>()
        ),
        entry!(
            "_Alignof(AudioStreamBasicDescription)",
            std::mem::align_of::<AudioStreamBasicDescription>()
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mSampleRate)",
            offset_of!(AudioStreamBasicDescription, mSampleRate)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mFormatID)",
            offset_of!(AudioStreamBasicDescription, mFormatID)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mFormatFlags)",
            offset_of!(AudioStreamBasicDescription, mFormatFlags)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mBytesPerPacket)",
            offset_of!(AudioStreamBasicDescription, mBytesPerPacket)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mFramesPerPacket)",
            offset_of!(AudioStreamBasicDescription, mFramesPerPacket)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mBytesPerFrame)",
            offset_of!(AudioStreamBasicDescription, mBytesPerFrame)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mChannelsPerFrame)",
            offset_of!(AudioStreamBasicDescription, mChannelsPerFrame)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mBitsPerChannel)",
            offset_of!(AudioStreamBasicDescription, mBitsPerChannel)
        ),
        entry!(
            "offsetof(AudioStreamBasicDescription, mReserved)",
            offset_of!(AudioStreamBasicDescription, mReserved)
        ),
        entry!("sizeof(AudioObjectID)", size_of::<AudioObjectID>()),
        entry!("sizeof(OSStatus)", size_of::<OSStatus>()),
        entry!(
            "sizeof(AudioDeviceIOProcID)",
            size_of::<AudioDeviceIOProcID>()
        ),
        entry!("sizeof(AudioClassID)", size_of::<AudioClassID>()),
        entry!("kAudioObjectUnknown", kAudioObjectUnknown),
        entry!("kAudioObjectSystemObject", kAudioObjectSystemObject),
        entry!(
            "kAudioObjectPropertyScopeGlobal",
            kAudioObjectPropertyScopeGlobal
        ),
        entry!(
            "kAudioObjectPropertyScopeInput",
            kAudioObjectPropertyScopeInput
        ),
        entry!(
            "kAudioObjectPropertyScopeOutput",
            kAudioObjectPropertyScopeOutput
        ),
        entry!(
            "kAudioObjectPropertyElementMain",
            kAudioObjectPropertyElementMain
        ),
        entry!(
            "kAudioHardwarePropertyTranslateUIDToDevice",
            kAudioHardwarePropertyTranslateUIDToDevice
        ),
        entry!(
            "kAudioHardwarePropertyDefaultOutputDevice",
            kAudioHardwarePropertyDefaultOutputDevice
        ),
        entry!(
            "kAudioHardwarePropertyDevices",
            kAudioHardwarePropertyDevices
        ),
        entry!(
            "kAudioHardwarePropertyServiceRestarted",
            kAudioHardwarePropertyServiceRestarted
        ),
        entry!("kAudioObjectPropertyClass", kAudioObjectPropertyClass),
        entry!("kAudioDeviceClassID", kAudioDeviceClassID),
        entry!(
            "kAudioDevicePropertyDeviceUID",
            kAudioDevicePropertyDeviceUID
        ),
        entry!(
            "kAudioDevicePropertyTransportType",
            kAudioDevicePropertyTransportType
        ),
        entry!(
            "kAudioDeviceTransportTypeVirtual",
            kAudioDeviceTransportTypeVirtual
        ),
        entry!(
            "kAudioDevicePropertyDeviceIsAlive",
            kAudioDevicePropertyDeviceIsAlive
        ),
        entry!(
            "kAudioDevicePropertyDeviceIsRunningSomewhere",
            kAudioDevicePropertyDeviceIsRunningSomewhere
        ),
        entry!("kAudioDevicePropertyIsHidden", kAudioDevicePropertyIsHidden),
        entry!(
            "kAudioDevicePropertyNominalSampleRate",
            kAudioDevicePropertyNominalSampleRate
        ),
        entry!("kAudioDevicePropertyStreams", kAudioDevicePropertyStreams),
        entry!(
            "kAudioDevicePropertyStreamConfiguration",
            kAudioDevicePropertyStreamConfiguration
        ),
        entry!(
            "kAudioStreamPropertyVirtualFormat",
            kAudioStreamPropertyVirtualFormat
        ),
        entry!("kAudioFormatLinearPCM", kAudioFormatLinearPCM),
        entry!("kAudioFormatFlagIsFloat", kAudioFormatFlagIsFloat),
        entry!("kAudioFormatFlagIsBigEndian", kAudioFormatFlagIsBigEndian),
        entry!("kAudioFormatFlagIsPacked", kAudioFormatFlagIsPacked),
        entry!(
            "kAudioFormatFlagIsNonInterleaved",
            kAudioFormatFlagIsNonInterleaved
        ),
        entry!(
            "kAudioFormatFlagsNativeFloatPacked",
            kAudioFormatFlagsNativeFloatPacked
        ),
        entry!("kAudioHardwareNoError", kAudioHardwareNoError as u32),
        entry!(
            "kAudioHardwareBadObjectError",
            kAudioHardwareBadObjectError as u32
        ),
        entry!(
            "kAudioHardwareBadDeviceError",
            kAudioHardwareBadDeviceError as u32
        ),
        entry!(
            "kAudioHardwareBadStreamError",
            kAudioHardwareBadStreamError as u32
        ),
        entry!(
            "kAudioHardwareUnknownPropertyError",
            kAudioHardwareUnknownPropertyError as u32
        ),
        entry!(
            "kAudioHardwareBadPropertySizeError",
            kAudioHardwareBadPropertySizeError as u32
        ),
        entry!(
            "kAudioDevicePermissionsError",
            kAudioDevicePermissionsError as u32
        ),
    ]
}
