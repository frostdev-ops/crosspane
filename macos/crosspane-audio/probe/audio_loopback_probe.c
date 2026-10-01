/* Crosspane audio loopback probe (WP-3.4): a metadata-and-synthetic-samples-only OWNER probe.
 *
 * Run by the owner AFTER installing CrosspaneAudio.driver, with explicit approval for that run:
 *
 *   CROSSPANE_AUDIO_OWNER_ATTENDED=1 target/macos-audio/audio-loopback-probe --synthetic-speakers
 *
 * It talks ONLY to the four exact virtual UIDs of the driver (translated through the public
 * UID -> device property). It never enumerates, selects or changes any physical or default
 * device, never reads a real microphone and never plays anything but synthetic samples into the
 * visible "Crosspane speakers" device. Starting IO on the hidden loopback INPUT device is an
 * audio input: macOS may show a Microphone permission prompt; if it is denied the loopback
 * delivers no data, the probe reports that and fails. It never retries around a denial.
 *
 * What it checks (every line printed is PASS, FAIL or INFO):
 *   - hidden UID translation, exact UIDs/names/direction/format/hidden/default flags, clock
 *     domain and period for all four objects;
 *   - visible-only running demand: starting the hidden reader never makes the visible device run;
 *     the visible device runs exactly while an app (this probe) plays into it;
 *   - two simultaneous readers of the hidden input receive the same deterministic data, equal to
 *     the synthetic pattern at (input sample time - 1024 frames);
 *   - the timestamp/callback envelope: callback sizes, sample-time continuity, buffer ranges.
 */
#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "../src/crosspane_audio.h"

#define PROBE_ZERO_TIMESTAMP_PERIOD 0x72696e67u /* 'ring': kAudioDevicePropertyZeroTimeStampPeriod */

static int g_failures;
static void report(bool ok, const char *name, const char *detail) {
    printf("%s %s%s%s\n", ok ? "PASS" : "FAIL", name, detail && *detail ? ": " : "", detail ? detail : "");
    if (!ok) ++g_failures;
}
static void info(const char *fmt, unsigned long long a, unsigned long long b, unsigned long long c) {
    char line[256];
    snprintf(line, sizeof(line), fmt, a, b, c);
    printf("INFO %s\n", line);
}
static void sleep_ms(unsigned ms) {
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

/* ---- Small HAL property helpers -------------------------------------------------------------- */
static OSStatus get(AudioObjectID o, UInt32 sel, UInt32 scope, UInt32 size, void *out) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    UInt32 n = size;
    OSStatus st = AudioObjectGetPropertyData(o, &a, 0, NULL, &n, out);
    return st == noErr && n != size ? kAudioHardwareBadPropertySizeError : st;
}
static bool u32(AudioObjectID o, UInt32 sel, UInt32 scope, UInt32 *out) {
    return get(o, sel, scope, sizeof(*out), out) == noErr;
}
static bool translate(const char *uid, AudioObjectID *out) {
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal,
                                    kAudioObjectPropertyElementMain};
    CFStringRef s = CFStringCreateWithCString(NULL, uid, kCFStringEncodingUTF8);
    UInt32 n = sizeof(*out);
    *out = kAudioObjectUnknown;
    OSStatus st = AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof(s), &s, &n, out);
    CFRelease(s);
    return st == noErr && *out != kAudioObjectUnknown;
}
static bool name_is(AudioObjectID o, const char *want) {
    AudioObjectPropertyAddress a = {kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    CFStringRef s = NULL;
    UInt32 n = sizeof(s);
    if (AudioObjectGetPropertyData(o, &a, 0, NULL, &n, &s) != noErr || !s) return false;
    char buf[128];
    bool ok = CFStringGetCString(s, buf, sizeof(buf), kCFStringEncodingUTF8) && strcmp(buf, want) == 0;
    CFRelease(s);
    return ok;
}
static bool in_device_list(AudioObjectID dev) {
    AudioObjectPropertyAddress a = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    UInt32 n = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, NULL, &n) != noErr) return false;
    AudioObjectID *ids = malloc(n ? n : 1);
    if (!ids) return false;
    bool found = false;
    if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &n, ids) == noErr)
        for (UInt32 i = 0; i < n / sizeof(AudioObjectID); ++i) found = found || ids[i] == dev;
    free(ids);
    return found;
}

/* ---- Per-device metadata ---------------------------------------------------------------------- */
typedef struct {
    const char *uid, *name;
    bool input, hidden;
    UInt32 channels;
    AudioObjectID id;
} dev_info;
static dev_info DEVS[4] = {
    {CROSSPANE_SPEAKERS_APP_UID, CROSSPANE_SPEAKERS_APP_NAME, false, false, 2, 0},
    {CROSSPANE_SPEAKERS_LOOPBACK_UID, CROSSPANE_SPEAKERS_LOOPBACK_NAME, true, true, 2, 0},
    {CROSSPANE_MIC_APP_UID, CROSSPANE_MIC_APP_NAME, true, false, 1, 0},
    {CROSSPANE_MIC_LOOPBACK_UID, CROSSPANE_MIC_LOOPBACK_NAME, false, true, 1, 0},
};
#define SPEAKERS (DEVS[0].id)
#define LOOPBACK (DEVS[1].id)

static void check_metadata(void) {
    for (unsigned i = 0; i < 4; ++i) {
        dev_info *d = &DEVS[i];
        char what[160], detail[200];
        const bool found = translate(d->uid, &d->id);
        snprintf(what, sizeof(what), "translate %s", d->uid);
        snprintf(detail, sizeof(detail), "id=%u", (unsigned)d->id);
        report(found, what, detail);
        if (!d->id) continue;
        UInt32 v = 0;
        const UInt32 own = d->input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
        const UInt32 other = d->input ? kAudioObjectPropertyScopeOutput : kAudioObjectPropertyScopeInput;
        snprintf(what, sizeof(what), "%s metadata", d->name);
        bool ok = name_is(d->id, d->name);
        ok = ok && u32(d->id, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal, &v) && v == kAudioDeviceTransportTypeVirtual;
        ok = ok && u32(d->id, kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal, &v) && (v != 0) == d->hidden;
        ok = ok && u32(d->id, kAudioDevicePropertyClockDomain, kAudioObjectPropertyScopeGlobal, &v) && v == CROSSPANE_AUDIO_CLOCK_DOMAIN;
        Float64 rate = 0;
        ok = ok && get(d->id, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, sizeof(rate), &rate) == noErr && rate == 48000.0;
        /* default eligibility: visible, app-facing direction only */
        ok = ok && u32(d->id, kAudioDevicePropertyDeviceCanBeDefaultDevice, own, &v) && (v != 0) == !d->hidden;
        ok = ok && u32(d->id, kAudioDevicePropertyDeviceCanBeDefaultDevice, other, &v) && v == 0;
        /* exactly one stream, in the device's own direction, packed float32 interleaved */
        AudioObjectPropertyAddress sa = {kAudioDevicePropertyStreams, own, kAudioObjectPropertyElementMain};
        UInt32 n = 0;
        ok = ok && AudioObjectGetPropertyDataSize(d->id, &sa, 0, NULL, &n) == noErr && n == sizeof(AudioStreamID);
        AudioStreamID stream = 0;
        ok = ok && AudioObjectGetPropertyData(d->id, &sa, 0, NULL, &n, &stream) == noErr && stream != 0;
        sa.mScope = other;
        ok = ok && AudioObjectGetPropertyDataSize(d->id, &sa, 0, NULL, &n) == noErr && n == 0;
        AudioStreamBasicDescription f;
        memset(&f, 0, sizeof(f));
        ok = ok && stream && get(stream, kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, sizeof(f), &f) == noErr;
        ok = ok && f.mSampleRate == 48000.0 && f.mFormatID == kAudioFormatLinearPCM && f.mChannelsPerFrame == d->channels &&
             f.mBitsPerChannel == 32 && (f.mFormatFlags & kAudioFormatFlagIsFloat) && !(f.mFormatFlags & kAudioFormatFlagIsNonInterleaved);
        report(ok, what, "name/virtual transport/hidden/clock domain 0x43504130/48 kHz/default flags/one stream/format");
        /* The zero-time-stamp period is a plug-in-side property ('ring'): the HAL may or may not
         * forward it to a client, so it is informational. */
        if (u32(d->id, PROBE_ZERO_TIMESTAMP_PERIOD, kAudioObjectPropertyScopeGlobal, &v))
            info("zero time stamp period: %llu frames (expected 16384)", v, 0, 0);
        /* visible devices appear in the device list, hidden ones only by UID */
        snprintf(what, sizeof(what), "%s listing", d->name);
        report(in_device_list(d->id) == !d->hidden, what, d->hidden ? "hidden: not in the device list" : "visible: in the device list");
    }
    UInt32 loopback_latency = 0;
    report(LOOPBACK && u32(LOOPBACK, kAudioDevicePropertyLatency, kAudioObjectPropertyScopeInput, &loopback_latency) &&
               loopback_latency == CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES,
           "fixed transfer delay reported once", "1024 frames on the hidden speakers input");
}

/* ---- Synthetic signal ------------------------------------------------------------------------- */
/* A pure function of the frame time, never zero, exactly reproducible on both sides. */
static float pat_l(int64_t t) { return (float)(t % 4096 + 1) / 8192.0f; }
static float pat_r(int64_t t) { return -(float)((t * 7) % 4096 + 1) / 8192.0f; }

typedef struct {
    _Atomic uint64_t matched, silent, mismatched, callbacks;
    _Atomic uint64_t min_frames, max_frames, discontinuities;
    _Atomic uint32_t flags_and, flags_or; /* mFlags of every input time stamp the IOProc saw */
    double next_time; /* touched by the owning IOProc only */
} reader_stats;
typedef struct {
    _Atomic uint64_t callbacks, min_frames, max_frames, discontinuities;
    _Atomic uint32_t flags_and, flags_or; /* mFlags of every output time stamp the IOProc saw */
    double next_time;
} writer_stats;
static reader_stats RA, RB;
static writer_stats WS;

static void note_frames(_Atomic uint64_t *min, _Atomic uint64_t *max, uint64_t n) {
    uint64_t v = atomic_load(min);
    while (n < v && !atomic_compare_exchange_weak(min, &v, n)) {}
    v = atomic_load(max);
    while (n > v && !atomic_compare_exchange_weak(max, &v, n)) {}
}

static OSStatus reader_proc(AudioObjectID dev, const AudioTimeStamp *now, const AudioBufferList *in,
                            const AudioTimeStamp *in_time, AudioBufferList *out, const AudioTimeStamp *out_time, void *ctx) {
    (void)dev; (void)now; (void)out; (void)out_time;
    reader_stats *st = ctx;
    if (!in || in->mNumberBuffers < 1 || !in->mBuffers[0].mData || !in_time) return noErr;
    const float *p = in->mBuffers[0].mData;
    const UInt32 frames = in->mBuffers[0].mDataByteSize / (2 * sizeof(float));
    atomic_fetch_add(&st->callbacks, 1);
    atomic_fetch_and(&st->flags_and, in_time->mFlags);
    atomic_fetch_or(&st->flags_or, in_time->mFlags);
    note_frames(&st->min_frames, &st->max_frames, frames);
    if (st->next_time != 0 && in_time->mSampleTime != st->next_time) atomic_fetch_add(&st->discontinuities, 1);
    st->next_time = in_time->mSampleTime + frames;
    const int64_t t0 = (int64_t)(in_time->mSampleTime + 0.5) - CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES;
    for (UInt32 k = 0; k < frames; ++k) {
        float l = p[2 * k], r = p[2 * k + 1];
        if (l == 0.0f && r == 0.0f) atomic_fetch_add(&st->silent, 1);
        else if (l == pat_l(t0 + k) && r == pat_r(t0 + k)) atomic_fetch_add(&st->matched, 1);
        else atomic_fetch_add(&st->mismatched, 1);
    }
    return noErr;
}
static OSStatus writer_proc(AudioObjectID dev, const AudioTimeStamp *now, const AudioBufferList *in,
                            const AudioTimeStamp *in_time, AudioBufferList *out, const AudioTimeStamp *out_time, void *ctx) {
    (void)dev; (void)now; (void)in; (void)in_time;
    writer_stats *st = ctx;
    if (!out || out->mNumberBuffers < 1 || !out->mBuffers[0].mData || !out_time) return noErr;
    float *p = out->mBuffers[0].mData;
    const UInt32 frames = out->mBuffers[0].mDataByteSize / (2 * sizeof(float));
    atomic_fetch_add(&st->callbacks, 1);
    atomic_fetch_and(&st->flags_and, out_time->mFlags);
    atomic_fetch_or(&st->flags_or, out_time->mFlags);
    note_frames(&st->min_frames, &st->max_frames, frames);
    if (st->next_time != 0 && out_time->mSampleTime != st->next_time) atomic_fetch_add(&st->discontinuities, 1);
    st->next_time = out_time->mSampleTime + frames;
    const int64_t t0 = (int64_t)(out_time->mSampleTime + 0.5);
    for (UInt32 k = 0; k < frames; ++k) {
        p[2 * k] = pat_l(t0 + k);
        p[2 * k + 1] = pat_r(t0 + k);
    }
    return noErr;
}

static bool running_somewhere(AudioObjectID dev) {
    UInt32 v = 0;
    return u32(dev, kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioObjectPropertyScopeGlobal, &v) && v != 0;
}
static bool wait_running(AudioObjectID dev, bool want, unsigned timeout_ms) {
    for (unsigned t = 0; t < timeout_ms; t += 20) {
        if (running_somewhere(dev) == want) return true;
        sleep_ms(20);
    }
    return running_somewhere(dev) == want;
}

static _Atomic unsigned visible_notifications;
static OSStatus on_running_changed(AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress *a, void *ctx) {
    (void)o; (void)n; (void)a; (void)ctx;
    atomic_fetch_add(&visible_notifications, 1);
    return noErr;
}

static void reset_stats(void) {
    memset(&RA, 0, sizeof(RA));
    memset(&RB, 0, sizeof(RB));
    memset(&WS, 0, sizeof(WS));
    atomic_store(&RA.min_frames, UINT64_MAX);
    atomic_store(&RB.min_frames, UINT64_MAX);
    atomic_store(&WS.min_frames, UINT64_MAX);
    atomic_store(&RA.flags_and, UINT32_MAX); /* "and" starts all ones, "or" all zeros */
    atomic_store(&RB.flags_and, UINT32_MAX);
    atomic_store(&WS.flags_and, UINT32_MAX);
}

/* One synthetic pass: hidden readers first (hidden-only), then the visible app plays. */
static void run_pass(const char *label, unsigned run_ms) {
    char what[160], detail[240];
    reset_stats();
    AudioDeviceIOProcID ra = NULL, rb = NULL, wp = NULL;
    OSStatus s1 = AudioDeviceCreateIOProcID(LOOPBACK, reader_proc, &RA, &ra);
    OSStatus s2 = AudioDeviceCreateIOProcID(LOOPBACK, reader_proc, &RB, &rb);
    OSStatus s3 = AudioDeviceCreateIOProcID(SPEAKERS, writer_proc, &WS, &wp);
    snprintf(what, sizeof(what), "%s: create IOProcs", label);
    report(s1 == noErr && s2 == noErr && s3 == noErr, what, "two readers on the hidden input, one writer on the visible output");
    if (s1 || s2 || s3) goto cleanup;

    report(!running_somewhere(SPEAKERS) && !running_somewhere(LOOPBACK), "idle before the pass", "nothing runs");
    const unsigned notes_before = atomic_load(&visible_notifications);

    /* Hidden-only start: this is the agent, not an app, so there is NO visible demand. */
    OSStatus st = AudioDeviceStart(LOOPBACK, ra);
    snprintf(what, sizeof(what), "%s: start hidden reader", label);
    report(st == noErr, what, st == noErr ? "started (a Microphone permission prompt may have appeared)" : "refused: virtual-input permission denied or driver error; stopping");
    if (st != noErr) goto cleanup;
    st = AudioDeviceStart(LOOPBACK, rb);
    report(st == noErr, "second reader started", "");
    sleep_ms(400);
    snprintf(what, sizeof(what), "%s: hidden-only start is not visible demand", label);
    report(running_somewhere(LOOPBACK) && !running_somewhere(SPEAKERS), what, "hidden running, visible not running");

    /* An app plays into the visible device. */
    st = AudioDeviceStart(SPEAKERS, wp);
    report(st == noErr, "visible app start", "");
    snprintf(what, sizeof(what), "%s: visible demand follows the app", label);
    report(wait_running(SPEAKERS, true, 1000), what, "visible running while the app plays");
    sleep_ms(run_ms);

    OSStatus stop_w = AudioDeviceStop(SPEAKERS, wp);
    report(stop_w == noErr && wait_running(SPEAKERS, false, 1000) && running_somewhere(LOOPBACK), "visible demand ends with the app", "visible stopped; hidden reader still running");
    sleep_ms(200);
    AudioDeviceStop(LOOPBACK, rb);
    AudioDeviceStop(LOOPBACK, ra);
    snprintf(what, sizeof(what), "%s: hidden reader stopped", label);
    report(wait_running(LOOPBACK, false, 1000), what, "hidden not running");
    const unsigned notes_after = atomic_load(&visible_notifications);
    info("visible running notifications during the pass: %llu (HAL-originated; informational)", notes_after - notes_before, 0, 0);

    /* Data: both readers saw the synthetic pattern at (input time - 1024), identically. */
    const uint64_t ma = atomic_load(&RA.matched), mb = atomic_load(&RB.matched);
    const uint64_t xa = atomic_load(&RA.mismatched), xb = atomic_load(&RB.mismatched);
    info("reader A: matched=%llu silent=%llu mismatched=%llu", ma, atomic_load(&RA.silent), xa);
    info("reader B: matched=%llu silent=%llu mismatched=%llu", mb, atomic_load(&RB.silent), xb);
    snprintf(what, sizeof(what), "%s: deterministic data for simultaneous readers", label);
    snprintf(detail, sizeof(detail), "A matched %llu / B matched %llu frames, mismatched %llu/%llu",
             (unsigned long long)ma, (unsigned long long)mb, (unsigned long long)xa, (unsigned long long)xb);
    /* At least one second of audio must have crossed (48000 frames), with nothing wrong. */
    report(ma >= 48000 && mb >= 48000 && xa == 0 && xb == 0, what, detail);
    if (ma == 0 && mb == 0)
        printf("INFO no data crossed the loopback. If a Microphone permission prompt appeared, allow it and run the "
               "probe again; if it was denied the loopback stays silent and this is the expected failure (the probe "
               "never works around a denial).\n");

    /* Envelope. */
    const uint64_t max_r = atomic_load(&RA.max_frames) > atomic_load(&RB.max_frames) ? atomic_load(&RA.max_frames) : atomic_load(&RB.max_frames);
    info("callback frames: reader min=%llu max=%llu; writer min=%llu", atomic_load(&RA.min_frames), max_r, atomic_load(&WS.min_frames));
    info("callback frames: writer max=%llu; callbacks reader=%llu writer=%llu", atomic_load(&WS.max_frames), atomic_load(&RA.callbacks), atomic_load(&WS.callbacks));
    info("sample-time discontinuities: reader A=%llu reader B=%llu writer=%llu", atomic_load(&RA.discontinuities), atomic_load(&RB.discontinuities), atomic_load(&WS.discontinuities));
    /* The driver refuses any Begin/Do/End whose operation time stamp lacks kAudioTimeStampSampleTimeValid.
     * What the IOProcs see is the client view of the same stamps: print the flags and require the bit. */
    const uint32_t in_and = atomic_load(&RA.flags_and) & atomic_load(&RB.flags_and), in_or = atomic_load(&RA.flags_or) | atomic_load(&RB.flags_or);
    info("input time stamp mFlags observed: always set=0x%llx ever set=0x%llx (SampleTimeValid=0x%llx)", in_and, in_or, kAudioTimeStampSampleTimeValid);
    info("output time stamp mFlags observed: always set=0x%llx ever set=0x%llx (SampleTimeValid=0x%llx)", atomic_load(&WS.flags_and), atomic_load(&WS.flags_or), kAudioTimeStampSampleTimeValid);
    snprintf(what, sizeof(what), "%s: SampleTimeValid on every observed time stamp", label);
    report((in_and & kAudioTimeStampSampleTimeValid) && (atomic_load(&WS.flags_and) & kAudioTimeStampSampleTimeValid), what,
           "the real HAL sets the flag the driver requires (client view; the data path itself confirms the driver's view)");
    snprintf(what, sizeof(what), "%s: callback envelope", label);
    report(max_r <= CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES && atomic_load(&WS.max_frames) <= CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES &&
               atomic_load(&WS.callbacks) > 0 && atomic_load(&RA.callbacks) > 0,
           what, "all callbacks within 0..4096 frames");
cleanup:
    if (ra) AudioDeviceDestroyIOProcID(LOOPBACK, ra);
    if (rb) AudioDeviceDestroyIOProcID(LOOPBACK, rb);
    if (wp) AudioDeviceDestroyIOProcID(SPEAKERS, wp);
}

static void report_buffer_envelope(void) {
    for (unsigned i = 0; i < 2; ++i) { /* the speakers pair only */
        AudioValueRange range = {0, 0};
        OSStatus st = get(DEVS[i].id, kAudioDevicePropertyBufferFrameSizeRange, kAudioObjectPropertyScopeGlobal, sizeof(range), &range);
        char what[160], detail[160];
        snprintf(what, sizeof(what), "%s buffer frame size range", DEVS[i].name);
        snprintf(detail, sizeof(detail), "[%.0f, %.0f] frames (the driver supports 0..4096 and fails above that)", range.mMinimum, range.mMaximum);
        report(st == noErr && range.mMaximum <= CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES, what, detail);
    }
}

static bool set_buffer_size(AudioObjectID dev, UInt32 frames) {
    AudioObjectPropertyAddress a = {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    return AudioObjectSetPropertyData(dev, &a, 0, NULL, sizeof(frames), &frames) == noErr;
}

static void default_devices(AudioObjectID out[3]) {
    const UInt32 sels[3] = {kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDefaultInputDevice,
                            kAudioHardwarePropertyDefaultSystemOutputDevice};
    for (unsigned i = 0; i < 3; ++i) {
        out[i] = kAudioObjectUnknown;
        (void)get(kAudioObjectSystemObject, sels[i], kAudioObjectPropertyScopeGlobal, sizeof(out[i]), &out[i]); /* read only */
    }
}

int main(int argc, char **argv) {
    const char *attended = getenv("CROSSPANE_AUDIO_OWNER_ATTENDED");
    if (argc != 2 || strcmp(argv[1], "--synthetic-speakers") != 0 || !attended || strcmp(attended, "1") != 0) {
        fprintf(stderr, "usage: CROSSPANE_AUDIO_OWNER_ATTENDED=1 %s --synthetic-speakers\n"
                        "Owner-attended only: run after installing the driver, with approval for this run.\n", argv[0]);
        return 2;
    }
    printf("Crosspane audio loopback probe (synthetic, speakers). Only the four Crosspane virtual UIDs are used.\n");
    AudioObjectID defaults_before[3], defaults_after[3];
    default_devices(defaults_before);

    check_metadata();
    if (!SPEAKERS || !LOOPBACK) {
        printf("FAIL driver not found: is CrosspaneAudio.driver installed and coreaudiod restarted?\n");
        return 1;
    }
    report_buffer_envelope();

    AudioObjectPropertyAddress ra = {kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioObjectAddPropertyListener(SPEAKERS, &ra, on_running_changed, NULL);

    run_pass("default buffers", 2500);
    if (g_failures == 0) {
        /* Operating envelope: the largest buffer the driver supports. */
        bool ok = set_buffer_size(SPEAKERS, CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES) && set_buffer_size(LOOPBACK, CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES);
        report(ok, "set both devices to 4096-frame buffers", ok ? "accepted" : "refused by the HAL");
        if (ok) run_pass("4096-frame buffers", 2500);
    }
    AudioObjectRemovePropertyListener(SPEAKERS, &ra, on_running_changed, NULL);

    default_devices(defaults_after);
    bool same = memcmp(defaults_before, defaults_after, sizeof(defaults_before)) == 0;
    bool ours = false;
    for (unsigned i = 0; i < 3; ++i)
        for (unsigned d = 0; d < 4; ++d) ours = ours || defaults_after[i] == DEVS[d].id;
    report(same && !ours, "default devices untouched", "no default changed, none is a Crosspane device");

    printf("%s: %d failure(s)\n", g_failures ? "RESULT FAIL" : "RESULT PASS", g_failures);
    return g_failures ? 1 : 0;
}
