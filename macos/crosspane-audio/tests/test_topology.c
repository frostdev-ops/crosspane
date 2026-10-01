/* Direct test 1: exact four devices/streams, UID translation and qualifier handling,
 * direction/hidden/default/format flags, IsRunning behaviour (visible-first, hidden-first,
 * hidden-only) and the 64-client boundary. No HAL registration, no audio I/O. */
#include "cp_fixture.h"

typedef struct {
    AudioObjectID dev, stream;
    const char *uid, *name;
    bool input, hidden;
    UInt32 channels;
} expect_dev;

static const expect_dev EXPECT[4] = {
    {2, 6, "io.frostdev.crosspane.audio.v0.speakers.app", "Crosspane speakers", false, false, 2},
    {3, 7, "io.frostdev.crosspane.audio.v0.speakers.loopback", "Crosspane speakers loopback", true, true, 2},
    {4, 8, "io.frostdev.crosspane.audio.v0.microphone.app", "Crosspane microphone", true, false, 1},
    {5, 9, "io.frostdev.crosspane.audio.v0.microphone.loopback", "Crosspane microphone loopback", false, true, 1},
};

static const UInt32 SCOPES[4] = {kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
                                 kAudioObjectPropertyScopeOutput, kAudioObjectPropertyScopePlayThrough};

static void test_frozen_constants(void) {
    /* The literals are frozen by WP-3.0b: assert them independently of the driver's headers. */
    CHECK(strcmp(CROSSPANE_AUDIO_BUNDLE_ID, "io.frostdev.crosspane.audio.driver") == 0, "bundle id");
    CHECK(strcmp(CROSSPANE_SPEAKERS_APP_UID, EXPECT[0].uid) == 0, "speakers.app uid");
    CHECK(strcmp(CROSSPANE_SPEAKERS_LOOPBACK_UID, EXPECT[1].uid) == 0, "speakers.loopback uid");
    CHECK(strcmp(CROSSPANE_MIC_APP_UID, EXPECT[2].uid) == 0, "microphone.app uid");
    CHECK(strcmp(CROSSPANE_MIC_LOOPBACK_UID, EXPECT[3].uid) == 0, "microphone.loopback uid");
    CHECK_EQ(CROSSPANE_AUDIO_RATE, 48000, "rate");
    CHECK_EQ(CROSSPANE_AUDIO_HISTORY_FRAMES, 16384, "history frames");
    CHECK_EQ(CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES, 1024, "transfer delay");
    CHECK_EQ(CROSSPANE_AUDIO_TIMESTAMP_PERIOD, 16384, "timestamp period");
    CHECK_EQ(CROSSPANE_AUDIO_CLOCK_DOMAIN, 0x43504130, "clock domain");
    CHECK_EQ(CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES, 4096, "max callback frames");
    CHECK_EQ(CROSSPANE_AUDIO_MAX_CLIENTS, 64, "client bound");
}

static void test_factory_and_plugin(void) {
    /* A wrong plug-in type is refused without taking a reference. */
    CHECK(CrosspaneAudioFactory(NULL, kAudioServerPlugInDriverInterfaceUUID) == NULL, "wrong type refused");
    CHECK(CrosspaneAudioFactory(NULL, NULL) == NULL, "NULL type refused");
    CHECK_EQ(g_refs, 0, "no reference taken by refused factory calls");

    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    const AudioObjectID P = CROSSPANE_OBJ_PLUGIN;
    CHECK_EQ(u32(P, kAudioObjectPropertyBaseClass, kAudioObjectPropertyScopeGlobal), kAudioObjectClassID, "plugin base class");
    CHECK_EQ(u32(P, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal), kAudioPlugInClassID, "plugin class");
    CHECK_EQ(u32(P, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal), kAudioObjectUnknown, "plugin owner");
    CHECK(str_prop_is(P, kAudioPlugInPropertyBundleID, "io.frostdev.crosspane.audio.driver"), "bundle id");
    CHECK(str_prop_is(P, kAudioObjectPropertyManufacturer, "Frostdev"), "manufacturer");

    AudioObjectID ids[8];
    UInt32 n = 0;
    CHECK(gp(P, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ids), &n, ids) == noErr, "device list");
    CHECK_EQ(n, 16, "exactly four devices");
    for (unsigned i = 0; i < 4; ++i) CHECK_EQ(ids[i], EXPECT[i].dev, "device ids in order");
    CHECK(gp(P, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ids), &n, ids) == noErr && n == 16, "plugin owned objects");

    /* Owned-object qualifier filtering: devices only. */
    AudioClassID want_stream = kAudioStreamClassID, want_device = kAudioDeviceClassID, any = kAudioObjectClassID;
    CHECK(gp(P, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, sizeof(want_stream), &want_stream, sizeof(ids), &n, ids) == noErr && n == 0, "no streams owned by the plugin");
    CHECK(gp(P, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, sizeof(want_device), &want_device, sizeof(ids), &n, ids) == noErr && n == 16, "devices owned by the plugin");
    CHECK(gp(P, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, sizeof(any), &any, sizeof(ids), &n, ids) == noErr && n == 16, "base-class qualifier matches all");
    CHECK(gp(P, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, 3, &any, sizeof(ids), &n, ids) == kAudioHardwareBadPropertySizeError, "malformed owned-object qualifier");

    /* Partial list fetch: whole elements only, never an error. */
    CHECK(gp(P, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, 9, &n, ids) == noErr && n == 8, "partial list fetch");
    CHECK(gp(P, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, 3, &n, ids) == noErr && n == 0, "list fetch with no room");
    CHECK(gsize(P, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr && n == 16, "device list size");
    CHECK(gsize(P, kAudioPlugInPropertyBoxList, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr && n == 0, "no boxes");
    CHECK(gsize(P, kAudioPlugInPropertyClockDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr && n == 0, "no clock devices");

    /* Strings are returned retained and sized for a CFStringRef only. */
    CFStringRef s;
    CHECK(gp(P, kAudioPlugInPropertyBundleID, kAudioObjectPropertyScopeGlobal, 0, NULL, 4, &n, &s) == kAudioHardwareBadPropertySizeError, "string needs room for a CFStringRef");
    cp_close(d);
    free(h);
}

static void test_translate(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    const AudioObjectID P = CROSSPANE_OBJ_PLUGIN;
    UInt32 n = 0;
    AudioObjectID out = 0xffff;

    /* All four UIDs, including the hidden devices, translate to their devices. */
    for (unsigned i = 0; i < 4; ++i) {
        CFStringRef uid = CFStringCreateWithCString(NULL, EXPECT[i].uid, kCFStringEncodingUTF8);
        CFIndex retains = CFGetRetainCount(uid);
        out = 0xffff;
        CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, sizeof(out), &n, &out) == noErr, "translate");
        CHECK(n == sizeof(out) && out == EXPECT[i].dev, "translate resolves every UID, hidden included");
        CHECK_EQ(CFGetRetainCount(uid), retains, "qualifier is not retained or released");
        CFRelease(uid);
    }
    /* Unknown, near-miss and case-different UIDs answer "unknown" with success. */
    const char *unknown[] = {"", "io.frostdev.crosspane.audio.v0.speakers", "io.frostdev.crosspane.audio.v0.speakers.app ",
        "IO.FROSTDEV.CROSSPANE.AUDIO.V0.SPEAKERS.APP", "io.frostdev.crosspane.audio.v0.speakers.apx",
        "io.frostdev.crosspane.audio.v1.speakers.app", "BuiltInSpeakerDevice"};
    for (unsigned i = 0; i < sizeof(unknown) / sizeof(unknown[0]); ++i) {
        CFStringRef uid = CFStringCreateWithCString(NULL, unknown[i], kCFStringEncodingUTF8);
        out = 0xffff;
        CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, sizeof(out), &n, &out) == noErr && out == kAudioObjectUnknown, "unknown UID translates to kAudioObjectUnknown");
        CFRelease(uid);
    }
    /* Boxes and clock devices do not exist: always unknown. */
    CFStringRef uid = CFStringCreateWithCString(NULL, EXPECT[0].uid, kCFStringEncodingUTF8);
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToBox, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, sizeof(out), &n, &out) == noErr && out == kAudioObjectUnknown, "box translation");
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToClockDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, sizeof(out), &n, &out) == noErr && out == kAudioObjectUnknown, "clock translation");

    /* Qualifier errors. */
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(out), &n, &out) == kAudioHardwareBadPropertySizeError, "missing qualifier");
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), NULL, sizeof(out), &n, &out) == kAudioHardwareBadPropertySizeError, "NULL qualifier data");
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, 4, &uid, sizeof(out), &n, &out) == kAudioHardwareBadPropertySizeError, "short qualifier");
    char two[16] = {0};
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(two), two, sizeof(out), &n, &out) == kAudioHardwareBadPropertySizeError, "long qualifier");
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, 3, &n, &out) == kAudioHardwareBadPropertySizeError, "output too small");
    CFStringRef null_string = NULL;
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(null_string), &null_string, sizeof(out), &n, &out) == kAudioHardwareIllegalOperationError, "NULL string");
    int number_value = 7;
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &number_value);
    CHECK(gp(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(number), &number, sizeof(out), &n, &out) == kAudioHardwareIllegalOperationError, "wrong CF type");
    CFRelease(number);
    CHECK(gsize(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, &n) == noErr && n == sizeof(AudioObjectID), "translate size");
    CHECK(has(P, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal), "plugin has translate");
    CFRelease(uid);

    /* Translation is a plug-in property only. */
    CHECK(!has(EXPECT[0].dev, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal), "devices do not translate");
    cp_close(d);
    free(h);
}

static void test_devices_and_streams(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    for (unsigned i = 0; i < 4; ++i) {
        const expect_dev *e = &EXPECT[i];
        const UInt32 own = e->input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
        const UInt32 other = e->input ? kAudioObjectPropertyScopeOutput : kAudioObjectPropertyScopeInput;
        CHECK_EQ(u32(e->dev, kAudioObjectPropertyBaseClass, kAudioObjectPropertyScopeGlobal), kAudioObjectClassID, "device base class");
        CHECK_EQ(u32(e->dev, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal), kAudioDeviceClassID, "device class");
        CHECK_EQ(u32(e->dev, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal), 1, "device owner is the plug-in");
        CHECK(str_prop_is(e->dev, kAudioDevicePropertyDeviceUID, e->uid), "device UID literal");
        CHECK(str_prop_is(e->dev, kAudioObjectPropertyName, e->name), "device name");
        CHECK(str_prop_is(e->dev, kAudioObjectPropertyManufacturer, "Frostdev"), "device manufacturer");
        CHECK(str_prop_is(e->dev, kAudioDevicePropertyModelUID, "io.frostdev.crosspane.audio.v0.model"), "model UID");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal), kAudioDeviceTransportTypeVirtual, "virtual transport");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyClockDomain, kAudioObjectPropertyScopeGlobal), 0x43504130, "common nonzero clock domain");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal), 1, "alive");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal), e->hidden ? 1 : 0, "hidden flag");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyZeroTimeStampPeriod, kAudioObjectPropertyScopeGlobal), 16384, "period");
        CHECK(u32(e->dev, kAudioDevicePropertyZeroTimeStampPeriod, kAudioObjectPropertyScopeGlobal) >= 10923, "period at or above the SDK minimum");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyClockAlgorithm, kAudioObjectPropertyScopeGlobal), kAudioDeviceClockAlgorithmRaw, "Raw clock algorithm");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertyClockIsStable, kAudioObjectPropertyScopeGlobal), 1, "stable clock");
        CHECK_EQ(u32(e->dev, kAudioDevicePropertySafetyOffset, own), 0, "safety offset");

        Float64 rate = 0;
        UInt32 n = 0;
        CHECK(gp(e->dev, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(rate), &n, &rate) == noErr && rate == 48000.0, "nominal rate");
        AudioValueRange range;
        CHECK(gp(e->dev, kAudioDevicePropertyAvailableNominalSampleRates, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(range), &n, &range) == noErr && range.mMinimum == 48000.0 && range.mMaximum == 48000.0, "only 48 kHz");

        /* Direction: exactly one stream, visible only in the device's own direction (and global). */
        for (unsigned s = 0; s < 4; ++s) {
            AudioObjectID streams[2] = {0, 0};
            CHECK(gp(e->dev, kAudioDevicePropertyStreams, SCOPES[s], 0, NULL, sizeof(streams), &n, streams) == noErr, "streams");
            bool visible = SCOPES[s] == own || SCOPES[s] == kAudioObjectPropertyScopeGlobal;
            CHECK_EQ(n, visible ? 4 : 0, "stream count by scope (no opposite-direction streams)");
            if (visible) CHECK_EQ(streams[0], e->stream, "stream id");
        }
        AudioObjectID none = 0;
        CHECK(gp(e->dev, kAudioDevicePropertyStreams, other, 0, NULL, 4, &n, &none) == noErr && n == 0, "no opposite-direction stream");

        /* Default eligibility: visible devices only, only in the app-facing scope. */
        for (unsigned s = 0; s < 4; ++s) {
            UInt32 want = (!e->hidden && SCOPES[s] == own) ? 1 : 0;
            CHECK_EQ(u32(e->dev, kAudioDevicePropertyDeviceCanBeDefaultDevice, SCOPES[s]), want, "can be default device");
            CHECK_EQ(u32(e->dev, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, SCOPES[s]), want, "can be default system device");
        }

        /* The fixed 1024-frame transfer delay is reported once: on the hidden speakers input. */
        for (unsigned s = 0; s < 4; ++s) {
            UInt32 want = (e->dev == 3 && SCOPES[s] == kAudioObjectPropertyScopeInput) ? 1024 : 0;
            CHECK_EQ(u32(e->dev, kAudioDevicePropertyLatency, SCOPES[s]), want, "device latency");
        }
        CHECK_EQ(u32(e->stream, kAudioStreamPropertyLatency, kAudioObjectPropertyScopeGlobal), 0, "stream latency is not double counted");

        /* Related devices and the (empty) control list. */
        AudioObjectID related;
        CHECK(gp(e->dev, kAudioDevicePropertyRelatedDevices, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(related), &n, &related) == noErr && n == 4 && related == e->dev, "related devices");
        CHECK(gsize(e->dev, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr && n == 0, "no controls");

        /* Preferred stereo channels exist on stereo devices, in their own scope only. */
        UInt32 pair[2] = {0, 0};
        if (e->channels == 2) {
            CHECK(gp(e->dev, kAudioDevicePropertyPreferredChannelsForStereo, own, 0, NULL, sizeof(pair), &n, pair) == noErr && n == 8 && pair[0] == 1 && pair[1] == 2, "stereo channels");
        } else {
            CHECK(!has(e->dev, kAudioDevicePropertyPreferredChannelsForStereo, own), "mono has no stereo pair");
        }
        CHECK(!has(e->dev, kAudioDevicePropertyPreferredChannelsForStereo, other), "stereo pair only in own scope");

        /* Device owned objects: its one stream. */
        AudioObjectID owned;
        CHECK(gp(e->dev, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(owned), &n, &owned) == noErr && n == 4 && owned == e->stream, "device owns its stream");
        AudioClassID device_class = kAudioDeviceClassID, stream_class = kAudioStreamClassID;
        CHECK(gp(e->dev, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, sizeof(device_class), &device_class, sizeof(owned), &n, &owned) == noErr && n == 0, "qualifier filters streams out");
        /* Owned objects are scoped like Streams: the one stream appears only in the device's own
         * direction (and global), in the size query, the data query and under a class qualifier. */
        for (unsigned s = 0; s < 4; ++s) {
            const bool visible = SCOPES[s] == own || SCOPES[s] == kAudioObjectPropertyScopeGlobal;
            AudioObjectID got[2] = {0, 0};
            UInt32 sz = 99;
            CHECK(gsize(e->dev, kAudioObjectPropertyOwnedObjects, SCOPES[s], 0, NULL, &sz) == noErr && sz == (visible ? 4u : 0u), "owned-object size by scope");
            sz = 99;
            CHECK(gp(e->dev, kAudioObjectPropertyOwnedObjects, SCOPES[s], 0, NULL, sizeof(got), &sz, got) == noErr && sz == (visible ? 4u : 0u), "owned-object data by scope");
            if (visible) CHECK_EQ(got[0], e->stream, "the owned stream");
            sz = 99;
            CHECK(gsize(e->dev, kAudioObjectPropertyOwnedObjects, SCOPES[s], sizeof(stream_class), &stream_class, &sz) == noErr && sz == (visible ? 4u : 0u), "qualified owned-object size by scope");
            sz = 99;
            CHECK(gp(e->dev, kAudioObjectPropertyOwnedObjects, SCOPES[s], sizeof(stream_class), &stream_class, sizeof(got), &sz, got) == noErr && sz == (visible ? 4u : 0u), "qualified owned-object data by scope");
            sz = 99;
            CHECK(gp(e->dev, kAudioObjectPropertyOwnedObjects, SCOPES[s], sizeof(device_class), &device_class, sizeof(got), &sz, got) == noErr && sz == 0, "a device-class qualifier never matches a stream");
        }

        /* The stream. */
        CHECK_EQ(u32(e->stream, kAudioObjectPropertyBaseClass, kAudioObjectPropertyScopeGlobal), kAudioObjectClassID, "stream base class");
        CHECK_EQ(u32(e->stream, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal), kAudioStreamClassID, "stream class");
        CHECK_EQ(u32(e->stream, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal), e->dev, "stream owner");
        CHECK_EQ(u32(e->stream, kAudioStreamPropertyDirection, kAudioObjectPropertyScopeGlobal), e->input ? 1 : 0, "stream direction");
        CHECK_EQ(u32(e->stream, kAudioStreamPropertyIsActive, kAudioObjectPropertyScopeGlobal), 1, "stream active");
        CHECK_EQ(u32(e->stream, kAudioStreamPropertyStartingChannel, kAudioObjectPropertyScopeGlobal), 1, "starting channel");
        CHECK_EQ(u32(e->stream, kAudioStreamPropertyTerminalType, kAudioObjectPropertyScopeGlobal), kAudioStreamTerminalTypeLine, "terminal type");
        AudioStreamBasicDescription f, p;
        CHECK(gp(e->stream, kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(f), &n, &f) == noErr && n == sizeof(f), "virtual format");
        CHECK(gp(e->stream, kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(p), &n, &p) == noErr && n == sizeof(p), "physical format");
        CHECK(memcmp(&f, &p, sizeof(f)) == 0, "virtual == physical");
        CHECK(f.mSampleRate == 48000.0 && f.mFormatID == kAudioFormatLinearPCM, "48 kHz linear PCM");
        CHECK(f.mFormatFlags == kAudioFormatFlagsNativeFloatPacked, "packed native-endian float, interleaved");
        CHECK((f.mFormatFlags & kAudioFormatFlagIsNonInterleaved) == 0, "interleaved");
        CHECK(f.mChannelsPerFrame == e->channels && f.mBitsPerChannel == 32 && f.mFramesPerPacket == 1, "channels and bit depth");
        CHECK(f.mBytesPerFrame == 4 * e->channels && f.mBytesPerPacket == 4 * e->channels, "frame bytes");
        AudioStreamRangedDescription ranged;
        CHECK(gp(e->stream, kAudioStreamPropertyAvailableVirtualFormats, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ranged), &n, &ranged) == noErr && n == sizeof(ranged), "available formats");
        CHECK(memcmp(&ranged.mFormat, &f, sizeof(f)) == 0 && ranged.mSampleRateRange.mMinimum == 48000.0 && ranged.mSampleRateRange.mMaximum == 48000.0, "available format matches");
        CHECK(has(e->stream, kAudioStreamPropertyAvailablePhysicalFormats, kAudioObjectPropertyScopeGlobal), "available physical formats");
        CHECK(!has(e->stream, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal), "streams have no name");
        CHECK(!has(e->stream, kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal), "streams have no UID");

        /* HasProperty / GetPropertyDataSize / GetPropertyData agree on a sample of selectors. */
        const UInt32 sels[] = {kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams, kAudioDevicePropertyLatency,
                               kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyOwnedObjects};
        for (unsigned k = 0; k < sizeof(sels) / sizeof(sels[0]); ++k) {
            UInt32 sz = 0, got = 0;
            uint8_t buf[64];
            CHECK(has(e->dev, sels[k], own), "has");
            CHECK(gsize(e->dev, sels[k], own, 0, NULL, &sz) == noErr, "size");
            CHECK(gp(e->dev, sels[k], own, 0, NULL, sizeof(buf), &got, buf) == noErr && got == sz, "size matches data");
            if (sels[k] == kAudioDevicePropertyDeviceUID) {
                CFStringRef s;
                memcpy(&s, buf, sizeof(s));
                CFRelease(s);
            }
        }
    }

    /* Unknown objects and selectors, malformed calls. */
    UInt32 n;
    CHECK(!has(99, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal), "unknown object has nothing");
    CHECK(gsize(99, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareBadObjectError, "unknown object");
    CHECK(gsize(10, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareBadObjectError, "id 10 is not an object");
    CHECK(gsize(0, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareBadObjectError, "id 0 is not an object");
    CHECK(gsize(2, 0x7a7a7a7a, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareUnknownPropertyError, "unknown selector");
    CHECK(!has(2, 0x7a7a7a7a, kAudioObjectPropertyScopeGlobal), "has unknown selector");
    CHECK(!has(CROSSPANE_OBJ_PLUGIN, kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal), "plugin has no device UID");
    AudioObjectPropertyAddress a = {kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 v;
    CHECK((*g_driver)->GetPropertyData(g_driver, 2, 0, &a, 0, NULL, 4, &n, NULL) == kAudioHardwareIllegalOperationError, "NULL output buffer");
    CHECK((*g_driver)->GetPropertyData(g_driver, 2, 0, NULL, 0, NULL, 4, &n, &v) == kAudioHardwareIllegalOperationError, "NULL address");
    CHECK((*g_driver)->GetPropertyDataSize(g_driver, 2, 0, &a, 0, NULL, NULL) == kAudioHardwareIllegalOperationError, "NULL size out");
    CHECK(gp(2, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, 2, &n, &v) == kAudioHardwareBadPropertySizeError, "scalar needs full room");

    /* Settable properties: only the rate and the stream formats, and only to the same value. */
    Boolean settable = true;
    AudioObjectPropertyAddress rate_addr = {kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 0};
    CHECK((*g_driver)->IsPropertySettable(g_driver, 2, 0, &rate_addr, &settable) == noErr && settable, "rate settable");
    AudioObjectPropertyAddress name_addr = {kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, 0};
    CHECK((*g_driver)->IsPropertySettable(g_driver, 2, 0, &name_addr, &settable) == noErr && !settable, "name not settable");
    Float64 rate = 48000.0, bad_rate = 44100.0;
    CHECK((*g_driver)->SetPropertyData(g_driver, 2, 0, &rate_addr, 0, NULL, sizeof(rate), &rate) == noErr, "set same rate");
    CHECK((*g_driver)->SetPropertyData(g_driver, 2, 0, &rate_addr, 0, NULL, sizeof(bad_rate), &bad_rate) == kAudioHardwareIllegalOperationError, "refuse other rate");
    CHECK((*g_driver)->SetPropertyData(g_driver, 2, 0, &rate_addr, 0, NULL, 4, &rate) == kAudioHardwareBadPropertySizeError, "bad rate size");
    CHECK((*g_driver)->SetPropertyData(g_driver, 2, 0, &name_addr, 0, NULL, sizeof(rate), &rate) == kAudioHardwareUnsupportedOperationError, "no other property is settable");
    AudioObjectPropertyAddress fmt_addr = {kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0};
    AudioStreamBasicDescription f;
    CHECK(gp(6, kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(f), &n, &f) == noErr, "format");
    CHECK((*g_driver)->SetPropertyData(g_driver, 6, 0, &fmt_addr, 0, NULL, sizeof(f), &f) == noErr, "set same format");
    f.mChannelsPerFrame = 1;
    CHECK((*g_driver)->SetPropertyData(g_driver, 6, 0, &fmt_addr, 0, NULL, sizeof(f), &f) == kAudioDeviceUnsupportedFormatError, "refuse other format");

    /* Devices are static. */
    CHECK((*g_driver)->CreateDevice(g_driver, NULL, NULL, NULL) == kAudioHardwareUnsupportedOperationError, "no CreateDevice");
    CHECK((*g_driver)->DestroyDevice(g_driver, 2) == kAudioHardwareUnsupportedOperationError, "no DestroyDevice");
    cp_close(d);
    free(h);
}

static void test_running_behaviour(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    const AudioObjectID SA = 2, SL = 3, MA = 4, ML = 5;
    for (unsigned i = 2; i <= 5; ++i) CHECK(!running((AudioObjectID)i), "nothing runs initially");

    /* Visible-first: demand appears on the visible device only; the hidden start adds no demand
     * to it. Each device notifies exactly on its own transitions. */
    CHECK(add_client(SA, 10) == noErr && add_client(SL, 20) == noErr, "register");
    CHECK(start_io(SA, 10) == noErr, "visible start");
    CHECK(running(SA) && !running(SL), "visible running, hidden not");
    CHECK_EQ(notes(h, SA), 1, "visible start notifies once");
    CHECK(start_io(SL, 20) == noErr, "hidden start");
    CHECK(running(SA) && running(SL), "both running");
    CHECK_EQ(notes(h, SA), 1, "hidden start does not notify the visible device");
    CHECK_EQ(notes(h, SL), 1, "hidden start notifies its own device");
    CHECK(stop_io(SL, 20) == noErr, "hidden stop");
    CHECK(running(SA) && !running(SL), "hidden stop leaves visible demand alone");
    CHECK_EQ(notes(h, SA), 1, "no visible notification for hidden stop");
    CHECK_EQ(notes(h, SL), 2, "hidden stop notifies");
    CHECK(stop_io(SA, 10) == noErr, "visible stop");
    CHECK(!running(SA), "visible stopped");
    CHECK_EQ(notes(h, SA), 2, "visible stop notifies");
    CHECK(remove_client(SA, 10) == noErr && remove_client(SL, 20) == noErr, "unregister");

    /* Hidden-first and hidden-only: the visible device never becomes running. */
    CHECK(add_client(SL, 21) == noErr, "register hidden");
    CHECK(start_io(SL, 21) == noErr, "hidden-only start");
    CHECK(running(SL) && !running(SA), "hidden-only never makes visible demand active");
    CHECK(add_client(SA, 11) == noErr && start_io(SA, 11) == noErr, "visible start after hidden");
    CHECK(running(SA), "now visible too");
    CHECK_EQ(notes(h, SL), 3, "hidden notifications are only its own");
    CHECK_EQ(notes(h, SA), 3, "visible notified for its own start");
    CHECK(stop_io(SA, 11) == noErr && stop_io(SL, 21) == noErr, "stops");
    CHECK(remove_client(SA, 11) == noErr && remove_client(SL, 21) == noErr, "remove");

    /* The microphone pair is independent of the speakers pair and notifies the same way. */
    CHECK(add_client(MA, 30) == noErr && add_client(ML, 31) == noErr, "mic register");
    CHECK(start_io(MA, 30) == noErr && start_io(ML, 31) == noErr, "mic starts (NULL starts count as demand)");
    CHECK(running(MA) && running(ML) && !running(SA) && !running(SL), "mic running, speakers not");
    CHECK_EQ(notes(h, MA), 1, "mic notified");
    CHECK_EQ(notes(h, ML), 1, "mic loopback notified");
    CHECK(cp_gate_is_closed(&g_xfer), "microphone activity never opens the speaker transfer");
    CHECK(stop_io(MA, 30) == noErr && stop_io(ML, 31) == noErr, "mic stops");
    CHECK(remove_client(MA, 30) == noErr && remove_client(ML, 31) == noErr, "mic remove");
    CHECK_EQ(atomic_load(&h->malformed), 0, "every notification names DeviceIsRunning, global, main");
    CHECK_EQ(atomic_load(&h->notifications), notes(h, SA) + notes(h, SL) + notes(h, MA) + notes(h, ML), "no other object notified");
    cp_close(d);
    free(h);
}

static void test_client_bounds_and_balance(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    const AudioObjectID SA = 2;

    /* 64 registered clients are supported; the 65th is rejected; duplicates do not consume slots. */
    for (UInt32 id = 1; id <= 64; ++id) CHECK(add_client(SA, id) == noErr, "register up to 64");
    CHECK(add_client(SA, 1) == noErr && add_client(SA, 64) == noErr, "duplicate registration is a no-op");
    CHECK(add_client(SA, 65) == kAudioHardwareIllegalOperationError, "65th client rejected");
    CHECK_EQ(g_clients[0].registered, 64, "registered count is bounded at 64");
    CHECK(add_client(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 65) == noErr, "limits are per device");
    CHECK(remove_client(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 65) == noErr, "remove it");
    CHECK(add_client(SA, 0) == kAudioHardwareIllegalOperationError, "the host client id is not registrable");
    CHECK(remove_client(SA, 64) == noErr, "free a slot");
    CHECK(add_client(SA, 65) == noErr, "a freed slot is reusable");
    CHECK(add_client(SA, 64) == kAudioHardwareIllegalOperationError, "full again");
    CHECK(remove_client(SA, 65) == noErr, "remove 65");
    CHECK(add_client(SA, 64) == noErr, "re-register 64");

    /* All 64 start: one running notification; the last stop gives one more. */
    for (UInt32 id = 1; id <= 64; ++id) CHECK(start_io(SA, id) == noErr, "start all");
    CHECK(running(SA), "running with 64 clients");
    CHECK_EQ(notes(h, SA), 1, "one notification for the whole group");
    CHECK_EQ(g_clients[0].started, 64, "64 started");
    for (UInt32 id = 1; id <= 63; ++id) CHECK(stop_io(SA, id) == noErr, "stop most");
    CHECK(running(SA), "still running with one client left");
    CHECK_EQ(notes(h, SA), 1, "no notification while demand remains");
    CHECK(stop_io(SA, 64) == noErr, "stop last");
    CHECK(!running(SA), "stopped");
    CHECK_EQ(notes(h, SA), 2, "one notification for the real change");

    /* Underflow, duplicates and unknown clients leave nothing fabricated. */
    CHECK(stop_io(SA, 1) == kAudioHardwareIllegalOperationError, "stop of a stopped client");
    CHECK(stop_io(SA, 999) == kAudioHardwareIllegalOperationError, "stop of an unregistered client");
    CHECK(start_io(SA, 999) == kAudioHardwareIllegalOperationError, "start of an unregistered client");
    CHECK(start_io(SA, 0) == kAudioHardwareIllegalOperationError, "start by the host client id");
    CHECK(remove_client(SA, 999) == kAudioHardwareIllegalOperationError, "remove of an unknown client");
    CHECK(!running(SA) && g_clients[0].started == 0, "no fabricated active client");
    CHECK(start_io(SA, 5) == noErr, "start");
    CHECK(start_io(SA, 5) == kAudioHardwareIllegalOperationError, "duplicate start is not counted twice");
    CHECK_EQ(g_clients[0].started, 1, "one started client");
    CHECK(stop_io(SA, 5) == noErr, "single stop balances the start");
    CHECK(!running(SA), "stopped after one stop");
    CHECK(stop_io(SA, 5) == kAudioHardwareIllegalOperationError, "second stop refused");
    CHECK_EQ(notes(h, SA), 4, "exactly the real transitions notified");

    /* A client that disappears (crash) while started gives up its demand. */
    CHECK(start_io(SA, 7) == noErr && start_io(SA, 8) == noErr, "two starts");
    CHECK(remove_client(SA, 7) == noErr, "first crash");
    CHECK(running(SA), "demand remains for the other client");
    CHECK_EQ(notes(h, SA), 5, "no notification for a non-final removal");
    CHECK(remove_client(SA, 8) == noErr, "second crash");
    CHECK(!running(SA), "last removal clears demand");
    CHECK_EQ(notes(h, SA), 6, "final removal notifies once");
    CHECK(stop_io(SA, 8) == kAudioHardwareIllegalOperationError, "a removed client cannot stop");
    CHECK(start_io(SA, 8) == kAudioHardwareIllegalOperationError, "a removed client cannot start");

    /* Bad objects. */
    CHECK(start_io(1, 1) == kAudioHardwareBadObjectError, "the plug-in is not a device");
    CHECK(start_io(6, 1) == kAudioHardwareBadObjectError, "a stream is not a device");
    CHECK(add_client(99, 1) == kAudioHardwareBadObjectError, "unknown device");
    CHECK((*g_driver)->AddDeviceClient(g_driver, SA, NULL) == kAudioHardwareIllegalOperationError, "NULL client info");
    CHECK((*g_driver)->RemoveDeviceClient(g_driver, SA, NULL) == kAudioHardwareIllegalOperationError, "NULL client info (remove)");
    cp_close(d);
    free(h);
}

int main(void) {
    test_frozen_constants();
    test_factory_and_plugin();
    test_translate();
    test_devices_and_streams();
    test_running_behaviour();
    test_client_bounds_and_balance();
    puts("PASS test_topology: 4 devices/4 streams, UID translation+qualifiers, flags, running demand, 64-client bound");
    return 0;
}
