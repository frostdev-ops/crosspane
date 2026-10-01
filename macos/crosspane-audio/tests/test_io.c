/* Direct test 5: real cycle info and sizes 0, 1, 480, 4096 and 4097, and failure for malformed
 * time, operation, stream, device and pointers. No HAL registration, no audio I/O. */
#include "cp_fixture.h"

#define SENTINEL 0x5a5a5a5au
#define MAXF CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES

static uint32_t buf_words[MAXF * 2 + 64]; /* big enough for 4097 stereo frames plus guard */

static void fill_sentinel(void) {
    for (size_t i = 0; i < sizeof(buf_words) / sizeof(buf_words[0]); ++i) buf_words[i] = SENTINEL;
}
static bool untouched_from(size_t first_word) {
    for (size_t i = first_word; i < sizeof(buf_words) / sizeof(buf_words[0]); ++i)
        if (buf_words[i] != SENTINEL) return false;
    return true;
}
static bool all_zero(size_t words) {
    for (size_t i = 0; i < words; ++i)
        if (buf_words[i] != 0) return false;
    return true;
}

static OSStatus do_io(AudioObjectID dev, AudioObjectID stream, UInt32 op, UInt32 frames,
                      const AudioServerPlugInIOCycleInfo *info, void *buffer) {
    return (*g_driver)->DoIOOperation(g_driver, dev, stream, 3, op, frames, info, buffer, NULL);
}

static void test_will_do_matrix(void) {
    const UInt32 ops[] = {kAudioServerPlugInIOOperationThread, kAudioServerPlugInIOOperationCycle,
        kAudioServerPlugInIOOperationReadInput, kAudioServerPlugInIOOperationConvertInput,
        kAudioServerPlugInIOOperationProcessInput, kAudioServerPlugInIOOperationProcessOutput,
        kAudioServerPlugInIOOperationMixOutput, kAudioServerPlugInIOOperationProcessMix,
        kAudioServerPlugInIOOperationConvertMix, kAudioServerPlugInIOOperationWriteMix, 0, 0x7a7a7a7a};
    /* output devices (2, 5) perform exactly WriteMix, input devices (3, 4) exactly ReadInput. */
    for (AudioObjectID dev = 2; dev <= 5; ++dev) {
        bool input = dev == 3 || dev == 4;
        for (unsigned i = 0; i < sizeof(ops) / sizeof(ops[0]); ++i) {
            Boolean will = 7, in_place = 7;
            CHECK((*g_driver)->WillDoIOOperation(g_driver, dev, 1, ops[i], &will, &in_place) == noErr, "WillDo");
            bool expected = ops[i] == (input ? kAudioServerPlugInIOOperationReadInput : kAudioServerPlugInIOOperationWriteMix);
            CHECK((will != 0) == expected, "exactly the device's operation");
            CHECK(in_place != 0, "in place");
        }
    }
    Boolean will, in_place;
    CHECK((*g_driver)->WillDoIOOperation(g_driver, 99, 1, kAudioServerPlugInIOOperationWriteMix, &will, &in_place) == kAudioHardwareBadObjectError, "unknown device");
    CHECK((*g_driver)->WillDoIOOperation(g_driver, 6, 1, kAudioServerPlugInIOOperationWriteMix, &will, &in_place) == kAudioHardwareBadObjectError, "stream is not a device");
    CHECK((*g_driver)->WillDoIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, NULL, &in_place) == kAudioHardwareIllegalOperationError, "NULL will");
    CHECK((*g_driver)->WillDoIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, &will, NULL) == kAudioHardwareIllegalOperationError, "NULL in-place");
}

static void test_begin_end(void) {
    AudioServerPlugInIOCycleInfo info = cycle_info(5000, 5000, 480);
    const UInt32 frame_counts[] = {0, 1, 480, 4096};
    for (unsigned i = 0; i < 4; ++i) {
        CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, frame_counts[i], &info) == noErr, "begin");
        CHECK((*g_driver)->EndIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, frame_counts[i], &info) == noErr, "end");
    }
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 4097, &info) == kAudioHardwareIllegalOperationError, "begin above ceiling");
    CHECK((*g_driver)->EndIOOperation(g_driver, 3, 1, kAudioServerPlugInIOOperationReadInput, 4097, &info) == kAudioHardwareIllegalOperationError, "end above ceiling");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 480, NULL) == kAudioHardwareIllegalOperationError, "begin needs cycle info");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 0, NULL) == noErr, "a 0-frame begin is a no-op");
    CHECK((*g_driver)->EndIOOperation(g_driver, 3, 1, kAudioServerPlugInIOOperationReadInput, 0, NULL) == noErr, "a 0-frame end is a no-op");
    AudioServerPlugInIOCycleInfo nan_info = cycle_info(NAN, NAN, 480);
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 480, &nan_info) == kAudioHardwareIllegalOperationError, "begin validates the output time");
    CHECK((*g_driver)->EndIOOperation(g_driver, 3, 1, kAudioServerPlugInIOOperationReadInput, 480, &nan_info) == kAudioHardwareIllegalOperationError, "end validates the input time");
    AudioServerPlugInIOCycleInfo only_in_bad = cycle_info(NAN, 5000, 480), only_out_bad = cycle_info(5000, NAN, 480);
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 480, &only_in_bad) == noErr, "an output operation ignores the input time");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 3, 1, kAudioServerPlugInIOOperationReadInput, 480, &only_out_bad) == noErr, "an input operation ignores the output time");
    CHECK((*g_driver)->EndIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 480, NULL) == kAudioHardwareIllegalOperationError, "end needs cycle info");
    /* The thread/cycle brackets may arrive without cycle info. */
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationThread, 0, NULL) == noErr, "thread begin");
    CHECK((*g_driver)->EndIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationCycle, 0, NULL) == noErr, "cycle end");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, 0x7a7a7a7a, 480, &info) == kAudioHardwareUnsupportedOperationError, "unknown operation");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 99, 1, kAudioServerPlugInIOOperationWriteMix, 480, &info) == kAudioHardwareBadObjectError, "unknown device");
}

static void test_sizes(void) {
    /* Open the speaker transfer so reads and writes are live. */
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91);
    const UInt32 sizes[] = {0, 1, 480, 4096};
    uint64_t t = 100000;
    for (unsigned i = 0; i < 4; ++i) {
        UInt32 n = sizes[i];
        /* write n frames of the pattern, then read them back (frame time t, input time t+1024) */
        CHECK(write_pattern(t, n) == noErr, "write of a supported size");
        fill_sentinel();
        CHECK(read_speakers(t + 1024, n, (float *)buf_words) == noErr, "read of a supported size");
        for (UInt32 k = 0; k < n; ++k) {
            CHECK(buf_words[2 * k] == fbits(fl(t + k)) && buf_words[2 * k + 1] == fbits(fr(t + k)), "frame content and alignment");
        }
        CHECK(untouched_from((size_t)n * 2), "no byte beyond the requested frames is written");
        t += 10000;
    }

    /* 0 frames is a no-op: nothing validated further, nothing written, even with NULL pointers. */
    fill_sentinel();
    AudioServerPlugInIOCycleInfo info = cycle_info(200000, 200000, 480);
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 0, &info, buf_words) == noErr, "0-frame read");
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 0, NULL, NULL) == noErr, "0-frame read with NULL pointers");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 0, NULL, NULL) == noErr, "0-frame write with NULL pointers");
    CHECK(do_io(4, 8, kAudioServerPlugInIOOperationReadInput, 0, &info, buf_words) == noErr, "0-frame mic read");
    CHECK(untouched_from(0), "0 frames touches nothing");

    /* 4097 frames: an explicit failure for every device, never a successful prefix. */
    const struct { AudioObjectID dev, stream; UInt32 op; } cases[] = {
        {2, 6, kAudioServerPlugInIOOperationWriteMix}, {3, 7, kAudioServerPlugInIOOperationReadInput},
        {4, 8, kAudioServerPlugInIOOperationReadInput}, {5, 9, kAudioServerPlugInIOOperationWriteMix}};
    for (unsigned i = 0; i < 4; ++i) {
        fill_sentinel();
        CHECK(do_io(cases[i].dev, cases[i].stream, cases[i].op, 4097, &info, buf_words) == kAudioHardwareIllegalOperationError, "4097 frames fails");
        CHECK(untouched_from(0), "failure above the ceiling never clears a prefix");
        fill_sentinel();
        CHECK(do_io(cases[i].dev, cases[i].stream, cases[i].op, 0xffffffffu, &info, buf_words) == kAudioHardwareIllegalOperationError, "absurd frame count fails");
        CHECK(untouched_from(0), "absurd count touches nothing");
    }
    /* The failed oversized write published nothing: its range reads back as silence. */
    CHECK(write_pattern(300000, 4097) == kAudioHardwareIllegalOperationError, "oversized write");
    CHECK(read_speakers(300000 + 1024, 4096, (float *)buf_words) == noErr && all_zero(4096 * 2), "an oversized write publishes nothing");

    finish(CROSSPANE_DEV_SPEAKERS_APP, 91);
    finish(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
}

static void test_malformed(void) {
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91);
    CHECK(write_pattern(500000, 480) == noErr, "reference write");
    AudioServerPlugInIOCycleInfo good = cycle_info(500000 + 1024, 500000, 480);

    /* Malformed time: reads fail but never leave stale bytes in the HAL's buffer. */
    const double bad_times[] = {NAN, -NAN, INFINITY, -INFINITY, -1.0, -1e300, 1e300, 70368744177664.0 /* 2^46 */,
                                70368744177664.0 - 4096.0 /* too close to the limit for a full block */};
    for (unsigned i = 0; i < sizeof(bad_times) / sizeof(bad_times[0]); ++i) {
        AudioServerPlugInIOCycleInfo bad = cycle_info(bad_times[i], bad_times[i], 480);
        fill_sentinel();
        CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &bad, buf_words) == kAudioHardwareIllegalOperationError, "read with malformed time");
        CHECK(all_zero(480 * 2) && untouched_from(480 * 2), "malformed-time read returns silence, nothing else touched");
        fill_sentinel();
        CHECK(do_io(4, 8, kAudioServerPlugInIOOperationReadInput, 480, &bad, buf_words) == kAudioHardwareIllegalOperationError, "mic read with malformed time");
        CHECK(all_zero(480 * 1) && untouched_from(480), "mic: silence for the requested frames only");
        fill_sentinel();
        CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 480, &bad, buf_words) == kAudioHardwareIllegalOperationError, "write with malformed time");
        CHECK(untouched_from(0), "a failed write never modifies the HAL buffer");
        CHECK(do_io(5, 9, kAudioServerPlugInIOOperationWriteMix, 480, &bad, buf_words) == kAudioHardwareIllegalOperationError, "mic loopback write with malformed time");
    }
    /* The malformed time must also not have disturbed what was written earlier. */
    fill_sentinel();
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == noErr, "good read");
    for (UInt32 k = 0; k < 480; ++k)
        CHECK(buf_words[2 * k] == fbits(fl(500000 + k)) && buf_words[2 * k + 1] == fbits(fr(500000 + k)), "earlier data intact");

    /* The negative start of the timeline is silence, not an error: input time < 1024. */
    AudioServerPlugInIOCycleInfo early = cycle_info(100, 100, 480);
    fill_sentinel();
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &early, buf_words) == noErr && all_zero(480 * 2), "frames before the timeline start are silence");

    /* Pointer failures. */
    fill_sentinel();
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, NULL, buf_words) == kAudioHardwareIllegalOperationError, "NULL cycle info");
    CHECK(untouched_from(0), "NULL cycle info touches nothing");
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &good, NULL) == kAudioHardwareIllegalOperationError, "NULL buffer (read)");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 480, &good, NULL) == kAudioHardwareIllegalOperationError, "NULL buffer (write)");
    CHECK(do_io(5, 9, kAudioServerPlugInIOOperationWriteMix, 480, &good, NULL) == kAudioHardwareIllegalOperationError, "NULL buffer (mic loopback)");

    /* Identity failures: unknown device, wrong stream, wrong operation. */
    fill_sentinel();
    CHECK(do_io(99, 7, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == kAudioHardwareBadObjectError, "unknown device");
    CHECK(do_io(1, 7, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == kAudioHardwareBadObjectError, "the plug-in is not a device");
    CHECK(do_io(6, 6, kAudioServerPlugInIOOperationWriteMix, 480, &good, buf_words) == kAudioHardwareBadObjectError, "a stream is not a device");
    CHECK(do_io(3, 6, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == kAudioHardwareBadStreamError, "wrong stream for the device");
    CHECK(do_io(3, 99, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == kAudioHardwareBadStreamError, "unknown stream");
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationWriteMix, 480, &good, buf_words) == kAudioHardwareUnsupportedOperationError, "WriteMix on an input device");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationReadInput, 480, &good, buf_words) == kAudioHardwareUnsupportedOperationError, "ReadInput on an output device");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationProcessMix, 480, &good, buf_words) == kAudioHardwareUnsupportedOperationError, "other operation");
    CHECK(do_io(2, 6, 0, 480, &good, buf_words) == kAudioHardwareUnsupportedOperationError, "zero operation");
    CHECK(do_io(2, 6, 0x7a7a7a7a, 480, &good, buf_words) == kAudioHardwareUnsupportedOperationError, "unknown operation");
    CHECK(untouched_from(0), "identity failures touch nothing");
    /* A 0-frame call with a wrong identity is still refused. */
    CHECK(do_io(3, 6, kAudioServerPlugInIOOperationReadInput, 0, &good, buf_words) == kAudioHardwareBadStreamError, "0-frame call still validates the stream");

    finish(CROSSPANE_DEV_SPEAKERS_APP, 91);
    finish(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
}

/* Finding: the operation's time stamp must carry kAudioTimeStampSampleTimeValid; a flagless or
 * host-time-only stamp is malformed for Begin, Do and End alike. Only the stamp the operation
 * actually uses counts (input for ReadInput, output for WriteMix). */
static void test_timestamp_flags(void) {
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91);
    const UInt32 SV = kAudioTimeStampSampleTimeValid, HV = kAudioTimeStampHostTimeValid, RV = kAudioTimeStampRateScalarValid;
    const UInt32 bad_flags[] = {0, HV, RV, HV | RV};
    const struct { AudioObjectID dev, stream; UInt32 op; bool input; } ops[] = {
        {2, 6, kAudioServerPlugInIOOperationWriteMix, false}, {3, 7, kAudioServerPlugInIOOperationReadInput, true},
        {4, 8, kAudioServerPlugInIOOperationReadInput, true}, {5, 9, kAudioServerPlugInIOOperationWriteMix, false}};
    for (unsigned f = 0; f < 4; ++f) {
        for (unsigned o = 0; o < 4; ++o) {
            const UInt32 in_f = ops[o].input ? bad_flags[f] : SV | HV, out_f = ops[o].input ? SV | HV : bad_flags[f];
            AudioServerPlugInIOCycleInfo bad = cycle_info_flags(8000 + 1024, 8000, 480, in_f, out_f);
            fill_sentinel();
            CHECK(do_io(ops[o].dev, ops[o].stream, ops[o].op, 480, &bad, buf_words) == kAudioHardwareIllegalOperationError, "DoIO without a valid sample time");
            if (ops[o].input) CHECK(all_zero(480 * (ops[o].dev == 3 ? 2 : 1)), "a refused read returns silence, never stale bytes");
            else CHECK(untouched_from(0), "a refused write never modifies the HAL buffer");
            CHECK((*g_driver)->BeginIOOperation(g_driver, ops[o].dev, 1, ops[o].op, 480, &bad) == kAudioHardwareIllegalOperationError, "Begin without a valid sample time");
            CHECK((*g_driver)->EndIOOperation(g_driver, ops[o].dev, 1, ops[o].op, 480, &bad) == kAudioHardwareIllegalOperationError, "End without a valid sample time");
            /* 0 frames stays a no-op whatever the flags say; the brackets never needed a stamp. */
            CHECK(do_io(ops[o].dev, ops[o].stream, ops[o].op, 0, &bad, buf_words) == noErr, "a 0-frame call ignores the stamp");
            CHECK((*g_driver)->BeginIOOperation(g_driver, ops[o].dev, 1, ops[o].op, 0, &bad) == noErr, "a 0-frame begin ignores the stamp");
            CHECK((*g_driver)->BeginIOOperation(g_driver, ops[o].dev, 1, kAudioServerPlugInIOOperationThread, 0, &bad) == noErr, "the thread bracket ignores the stamp");
            CHECK((*g_driver)->EndIOOperation(g_driver, ops[o].dev, 1, kAudioServerPlugInIOOperationCycle, 0, &bad) == noErr, "the cycle bracket ignores the stamp");
        }
    }
    /* The other direction's flags are irrelevant, and SampleTimeValid alone (no host time) is enough. */
    AudioServerPlugInIOCycleInfo only_out = cycle_info_flags(8000 + 1024, 8000, 480, 0, SV);
    AudioServerPlugInIOCycleInfo only_in = cycle_info_flags(8000 + 1024, 8000, 480, SV, 0);
    CHECK(write_pattern(8000, 480) == noErr, "reference write");
    fill_sentinel();
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &only_in, buf_words) == noErr, "a read needs only the input stamp's sample time");
    for (UInt32 k = 0; k < 480; ++k)
        CHECK(buf_words[2 * k] == fbits(fl(8000 + k)) && buf_words[2 * k + 1] == fbits(fr(8000 + k)), "...and reads the right frames");
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &only_out, buf_words) == kAudioHardwareIllegalOperationError, "a read ignores the output stamp's flags");
    float stereo[960];
    for (int k = 0; k < 960; ++k) stereo[k] = 0.5f;
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 480, &only_out, stereo) == noErr, "a write needs only the output stamp's sample time");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 480, &only_in, stereo) == kAudioHardwareIllegalOperationError, "a write ignores the input stamp's flags");
    CHECK((*g_driver)->BeginIOOperation(g_driver, 2, 1, kAudioServerPlugInIOOperationWriteMix, 480, &only_out) == noErr, "Begin accepts it");
    CHECK((*g_driver)->EndIOOperation(g_driver, 3, 1, kAudioServerPlugInIOOperationReadInput, 480, &only_in) == noErr, "End accepts it");
    /* The refused writes above published nothing: only the reference frames exist at those times. */
    finish(CROSSPANE_DEV_SPEAKERS_APP, 91);
    finish(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
}

static void test_real_cycle_info(void) {
    /* A fully populated cycle info as the HAL would supply it, with fractional-looking but
     * integral sample times and distinct input/output times. */
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91);
    AudioServerPlugInIOCycleInfo c;
    memset(&c, 0, sizeof(c));
    c.mIOCycleCounter = 42;
    c.mNominalIOBufferFrameSize = 512;
    c.mMainHostTicksPerFrame = 500.0;
    c.mDeviceHostTicksPerFrame = 500.0;
    const UInt32 flags = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid | kAudioTimeStampRateScalarValid;
    c.mCurrentTime = (AudioTimeStamp){.mSampleTime = 20480, .mHostTime = 12345678, .mRateScalar = 1.0, .mFlags = flags};
    c.mInputTime = (AudioTimeStamp){.mSampleTime = 20480 - 512 + 1024, .mHostTime = 12000000, .mRateScalar = 1.0, .mFlags = flags};
    c.mOutputTime = (AudioTimeStamp){.mSampleTime = 20480 + 512, .mHostTime = 13000000, .mRateScalar = 1.0, .mFlags = flags};
    float stereo[512 * 2], got[512 * 2];
    for (unsigned k = 0; k < 512; ++k) {
        stereo[2 * k] = fl(20480 + 512 + k);
        stereo[2 * k + 1] = fr(20480 + 512 + k);
    }
    /* Write at the OUTPUT time ... */
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 512, &c, stereo) == noErr, "write uses the output time");
    /* ... and read at the INPUT time minus 1024: input = output + 1024 reads the same frames. */
    c.mInputTime.mSampleTime = 20480 + 512 + 1024;
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 512, &c, got) == noErr, "read uses the input time");
    CHECK(memcmp(stereo, got, sizeof(got)) == 0, "round trip through the history");
    /* The current time is ignored: only the operation's own time matters. */
    c.mCurrentTime.mSampleTime = NAN;
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 512, &c, got) == noErr && memcmp(stereo, got, sizeof(got)) == 0, "the current time plays no part");
    /* A write at the input time field must not land where the output time says. */
    c.mOutputTime.mSampleTime = 90000;
    c.mInputTime.mSampleTime = 12345;
    float other[16] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16};
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 8, &c, other) == noErr, "write elsewhere");
    float peek[16];
    CHECK(read_speakers(90000 + 1024, 8, peek) == noErr && memcmp(peek, other, sizeof(peek)) == 0, "write landed at the output time");
    CHECK(read_speakers(12345 + 1024, 8, peek) == noErr, "read at the input time field");
    for (unsigned i = 0; i < 16; ++i) CHECK(peek[i] == 0.0f, "nothing at the input time field");
    finish(CROSSPANE_DEV_SPEAKERS_APP, 91);
    finish(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
}

static void test_mic_always_silent(void) {
    /* The microphone pair is silent in this slice: reads are always zero and its hidden output is
     * always discarded, including synthetic writers, started or not. */
    for (int started = 0; started < 2; ++started) {
        if (started) {
            begin(CROSSPANE_DEV_MIC_APP, 80);
            begin(CROSSPANE_DEV_MIC_LOOPBACK, 81);
        }
        const UInt32 mic_sizes[] = {1, 9, 480, 4096};
        for (unsigned si = 0; si < 4; ++si) {
            const UInt32 n = mic_sizes[si];
            float loud[4100];
            for (UInt32 k = 0; k < 4100; ++k) loud[k] = 0.5f + (float)k;
            AudioServerPlugInIOCycleInfo c = cycle_info(70000, 70000, n);
            CHECK(do_io(5, 9, kAudioServerPlugInIOOperationWriteMix, n, &c, loud) == noErr, "synthetic write to the hidden mic output is accepted and discarded");
            for (UInt32 k = 0; k < 4100; ++k) loud[k] = 0.5f + (float)k; /* a buffer full of non-silence */
            CHECK(do_io(4, 8, kAudioServerPlugInIOOperationReadInput, n, &c, loud) == noErr, "mic read");
            for (UInt32 k = 0; k < n; ++k) CHECK(loud[k] == 0.0f, "mic read is silence");
            CHECK(loud[n] == 0.5f + (float)n, "mic read does not run past the requested frames");
        }
        if (started) {
            finish(CROSSPANE_DEV_MIC_APP, 80);
            finish(CROSSPANE_DEV_MIC_LOOPBACK, 81);
        }
    }
    /* The mic loopback output can never feed the speakers history or the mic read. */
    CHECK(cp_gate_is_closed(&g_xfer), "no transfer exists for the microphone");
}

int main(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    test_will_do_matrix();
    test_begin_end();
    test_sizes();
    test_malformed();
    test_timestamp_flags();
    test_real_cycle_info();
    test_mic_always_silent();
    cp_close(d);

    /* Before the first Initialize (the test-only reset re-creates that state): the data plane
     * answers silence / discards, never touching any state; the control plane refuses. */
    fill_sentinel();
    AudioServerPlugInIOCycleInfo info = cycle_info(60000, 60000, 480);
    CHECK(do_io(3, 7, kAudioServerPlugInIOOperationReadInput, 480, &info, buf_words) == noErr, "uninitialized read");
    CHECK(all_zero(480 * 2) && untouched_from(480 * 2), "uninitialized read is silence of the requested size");
    CHECK(do_io(2, 6, kAudioServerPlugInIOOperationWriteMix, 480, &info, buf_words) == noErr, "uninitialized write is discarded");
    CHECK(start_io(2, 1) == kAudioHardwareNotRunningError, "uninitialized control plane");
    free(h);
    puts("PASS test_io: sizes 0/1/480/4096/4097, malformed time/operation/stream/device/pointer failure, real cycle info, mic silence");
    return 0;
}
