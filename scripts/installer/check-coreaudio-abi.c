/* Detached SDK ABI assertions only: no executable and no CoreAudio operation. */
#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stddef.h>

#define LAYOUT(type, size, alignment) \
    _Static_assert(sizeof(type) == (size), #type " size"); \
    _Static_assert(_Alignof(type) == (alignment), #type " alignment")
#define OFFSET(type, field, offset) \
    _Static_assert(offsetof(type, field) == (offset), #type "." #field " offset")
#define VALUE(name, value) _Static_assert((name) == (value), #name " value")
#define TYPE(expression, type) \
    _Static_assert(__builtin_types_compatible_p(__typeof__(expression), type), \
                   #expression " type")

LAYOUT(OSStatus, 4, 4);
_Static_assert((OSStatus)-1 < 0, "OSStatus signed");
LAYOUT(AudioObjectID, 4, 4);
LAYOUT(AudioDeviceID, 4, 4);
LAYOUT(AudioStreamID, 4, 4);
LAYOUT(AudioObjectPropertyAddress, 12, 4);
OFFSET(AudioObjectPropertyAddress, mSelector, 0);
OFFSET(AudioObjectPropertyAddress, mScope, 4);
OFFSET(AudioObjectPropertyAddress, mElement, 8);
LAYOUT(AudioBuffer, 16, 8);
OFFSET(AudioBuffer, mNumberChannels, 0);
OFFSET(AudioBuffer, mDataByteSize, 4);
OFFSET(AudioBuffer, mData, 8);
LAYOUT(AudioBufferList, 24, 8);
OFFSET(AudioBufferList, mNumberBuffers, 0);
OFFSET(AudioBufferList, mBuffers, 8);
LAYOUT(AudioStreamBasicDescription, 40, 8);
OFFSET(AudioStreamBasicDescription, mSampleRate, 0);
OFFSET(AudioStreamBasicDescription, mFormatID, 8);
OFFSET(AudioStreamBasicDescription, mFormatFlags, 12);
OFFSET(AudioStreamBasicDescription, mBytesPerPacket, 16);
OFFSET(AudioStreamBasicDescription, mFramesPerPacket, 20);
OFFSET(AudioStreamBasicDescription, mBytesPerFrame, 24);
OFFSET(AudioStreamBasicDescription, mChannelsPerFrame, 28);
OFFSET(AudioStreamBasicDescription, mBitsPerChannel, 32);
OFFSET(AudioStreamBasicDescription, mReserved, 36);
LAYOUT(AudioDeviceIOProc, 8, 8);
LAYOUT(AudioDeviceIOProcID, 8, 8);
VALUE(kAudioObjectSystemObject, 1);
VALUE(kAudioObjectPropertyScopeGlobal, 0x676c6f62);
VALUE(kAudioObjectPropertyScopeInput, 0x696e7074);
VALUE(kAudioObjectPropertyScopeOutput, 0x6f757470);
VALUE(kAudioHardwarePropertyTranslateUIDToDevice, 0x75696464);
VALUE(kAudioDevicePropertyDeviceUID, 0x75696420);
VALUE(kAudioDevicePropertyDeviceIsAlive, 0x6c69766e);
VALUE(kAudioDevicePropertyIsHidden, 0x6869646e);
VALUE(kAudioDevicePropertyStreams, 0x73746d23);
VALUE(kAudioDevicePropertyNominalSampleRate, 0x6e737274);
VALUE(kAudioObjectPropertyClass, 0x636c6173);
VALUE(kAudioDeviceClassID, 0x61646576);
VALUE(kAudioDevicePropertyTransportType, 0x7472616e);
VALUE(kAudioDeviceTransportTypeVirtual, 0x76697274);
VALUE(kAudioStreamPropertyVirtualFormat, 0x73666d74);
VALUE(kAudioFormatLinearPCM, 0x6c70636d);
VALUE(kAudioFormatFlagsNativeFloatPacked, 9);
VALUE(kAudioObjectPropertyElementMain, 0);
VALUE(noErr, 0);

/* SDK timestamps remain typed here; Rust intentionally uses opaque const pointers. */
typedef OSStatus (*ExpectedIOProc)(
    AudioObjectID, const AudioTimeStamp *, const AudioBufferList *,
    const AudioTimeStamp *, AudioBufferList *, const AudioTimeStamp *, void *);
TYPE((AudioDeviceIOProc)0, ExpectedIOProc);
TYPE((AudioDeviceIOProcID)0, ExpectedIOProc);
typedef OSStatus (*PropertySize)(
    AudioObjectID, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *);
typedef OSStatus (*PropertyData)(
    AudioObjectID, const AudioObjectPropertyAddress *, UInt32, const void *, UInt32 *, void *);
typedef OSStatus (*CreateIOProc)(
    AudioObjectID, AudioDeviceIOProc, void *, AudioDeviceIOProcID *);
typedef OSStatus (*DeviceOperation)(AudioObjectID, AudioDeviceIOProcID);
TYPE(&AudioObjectGetPropertyDataSize, PropertySize);
TYPE(&AudioObjectGetPropertyData, PropertyData);
TYPE(&AudioDeviceCreateIOProcID, CreateIOProc);
TYPE(&AudioDeviceDestroyIOProcID, DeviceOperation);
TYPE(&AudioDeviceStart, DeviceOperation);
TYPE(&AudioDeviceStop, DeviceOperation);
