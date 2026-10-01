/* Detached direct-test fixture. No HAL registration, no audio I/O, no installed driver.
 *
 * Every test binary (except test_bundle.c) #includes the actual driver translation unit with
 * CROSSPANE_AUDIO_TESTING defined, then drives it ONLY through the real driver vtable
 * (g_driver) with a fake host. The testing build adds hook points, an injectable host clock and
 * access to internals for assertions; the production bundle contains none of that.
 */
#ifndef CP_FIXTURE_H
#define CP_FIXTURE_H

#ifndef CROSSPANE_AUDIO_TESTING
#define CROSSPANE_AUDIO_TESTING 1
#endif
#include "../src/crosspane_audio.c"

/* Each test binary uses a different subset of these helpers. */
#pragma clang diagnostic ignored "-Wunused-function"

#include <inttypes.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

/* ---- Assertions: thread-safe, fail fast ------------------------------------------------------ */
static void cp_fail(const char *file, int line, const char *what) {
    fprintf(stderr, "FAIL %s:%d: %s\n", file, line, what);
    fflush(stderr);
    _exit(1);
}
#define CHECK(cond, what)                                                                    \
    do {                                                                                     \
        if (!(cond)) cp_fail(__FILE__, __LINE__, (what));                                    \
    } while (0)
#define CHECK_EQ(a, b, what)                                                                 \
    do {                                                                                     \
        long long a_ = (long long)(a), b_ = (long long)(b);                                  \
        if (a_ != b_) {                                                                      \
            fprintf(stderr, "FAIL %s:%d: %s (got %lld, want %lld)\n", __FILE__, __LINE__,    \
                    (what), a_, b_);                                                         \
            fflush(stderr);                                                                  \
            _exit(1);                                                                        \
        }                                                                                    \
    } while (0)

/* ---- Fake host ------------------------------------------------------------------------------- */
typedef struct fake_host {
    AudioServerPlugInHostInterface iface; /* first member: the HAL passes this pointer back */
    _Atomic uint32_t notifications;       /* total PropertiesChanged calls */
    _Atomic uint32_t per_object[16];      /* ... per object id */
    _Atomic uint32_t malformed;           /* anything other than one DeviceIsRunning address */
    void (*on_notify)(struct fake_host *, AudioObjectID); /* optional (pause) hook */
} fake_host;

static OSStatus fake_properties_changed(AudioServerPlugInHostRef host, AudioObjectID obj,
                                        UInt32 n, const AudioObjectPropertyAddress *addrs) {
    fake_host *h = (fake_host *)(uintptr_t)host;
    if (n != 1 || !addrs || addrs[0].mSelector != kAudioDevicePropertyDeviceIsRunning ||
        addrs[0].mScope != kAudioObjectPropertyScopeGlobal ||
        addrs[0].mElement != kAudioObjectPropertyElementMain)
        atomic_fetch_add(&h->malformed, 1);
    if (obj < 16) atomic_fetch_add(&h->per_object[obj], 1);
    atomic_fetch_add(&h->notifications, 1);
    if (h->on_notify) h->on_notify(h, obj);
    return noErr;
}

static fake_host *fake_host_new(void) {
    fake_host *h = calloc(1, sizeof(*h));
    CHECK(h != NULL, "fake host alloc");
    h->iface.PropertiesChanged = fake_properties_changed;
    return h;
}
static uint32_t notes(fake_host *h, AudioObjectID obj) { return atomic_load(&h->per_object[obj]); }

/* ---- Driver open/close ----------------------------------------------------------------------- */
static AudioServerPlugInDriverRef cp_open(fake_host *h) {
    AudioServerPlugInDriverRef d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver, "factory returns the driver");
    CHECK((*d)->Initialize(d, &h->iface) == noErr, "Initialize");
    return d;
}
/* The driver is a process-lifetime singleton: Release to zero tears nothing down. Fixture cases
 * start from a clean singleton through the TEST-ONLY reset (single-threaded: every thread a case
 * started has been joined before it calls this). */
static void cp_close(AudioServerPlugInDriverRef d) {
    CHECK((*d)->Release(d) == 0, "Release to zero");
    cp_test_reset();
}

/* ---- Property helpers ------------------------------------------------------------------------ */
static OSStatus gp(AudioObjectID o, UInt32 sel, UInt32 scope, UInt32 qs, const void *q,
                   UInt32 cap, UInt32 *n, void *out) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    return (*g_driver)->GetPropertyData(g_driver, o, 0, &a, qs, q, cap, n, out);
}
static OSStatus gsize(AudioObjectID o, UInt32 sel, UInt32 scope, UInt32 qs, const void *q,
                      UInt32 *n) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    return (*g_driver)->GetPropertyDataSize(g_driver, o, 0, &a, qs, q, n);
}
static bool has(AudioObjectID o, UInt32 sel, UInt32 scope) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    return (*g_driver)->HasProperty(g_driver, o, 0, &a);
}
static UInt32 u32(AudioObjectID o, UInt32 sel, UInt32 scope) {
    UInt32 v = 0xdeadbeef, n = 0;
    CHECK(gp(o, sel, scope, 0, NULL, sizeof(v), &n, &v) == noErr && n == sizeof(v), "u32 property");
    return v;
}
static bool cp_str_is(CFStringRef s, const char *literal) {
    char buf[256];
    return s && CFStringGetCString(s, buf, sizeof(buf), kCFStringEncodingUTF8) &&
           strcmp(buf, literal) == 0;
}
static bool str_prop_is(AudioObjectID o, UInt32 sel, const char *literal) {
    CFStringRef s = NULL;
    UInt32 n = 0;
    CHECK(gp(o, sel, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(s), &n, &s) == noErr &&
              n == sizeof(s) && s != NULL,
          "string property");
    bool ok = cp_str_is(s, literal);
    CFRelease(s);
    return ok;
}

/* ---- IO helpers ------------------------------------------------------------------------------ */
static AudioServerPlugInIOCycleInfo cycle_info_flags(double in_time, double out_time, UInt32 frames,
                                                     UInt32 in_flags, UInt32 out_flags);
static AudioServerPlugInIOCycleInfo cycle_info(double in_time, double out_time, UInt32 frames) {
    AudioServerPlugInIOCycleInfo c;
    memset(&c, 0, sizeof(c));
    c.mIOCycleCounter = 1;
    c.mNominalIOBufferFrameSize = frames;
    const UInt32 flags = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid;
    c.mCurrentTime.mSampleTime = out_time;
    c.mCurrentTime.mHostTime = 1000;
    c.mCurrentTime.mFlags = flags;
    c.mInputTime.mSampleTime = in_time;
    c.mInputTime.mHostTime = 1000;
    c.mInputTime.mFlags = flags;
    c.mOutputTime.mSampleTime = out_time;
    c.mOutputTime.mHostTime = 1000;
    c.mOutputTime.mFlags = flags;
    c.mMainHostTicksPerFrame = 500.0;
    c.mDeviceHostTicksPerFrame = 500.0;
    return c;
}
/* Valid cycle info, then the operation time stamps' flags overridden. */
static AudioServerPlugInIOCycleInfo cycle_info_flags(double in_time, double out_time, UInt32 frames,
                                                     UInt32 in_flags, UInt32 out_flags) {
    AudioServerPlugInIOCycleInfo c = cycle_info(in_time, out_time, frames);
    c.mInputTime.mFlags = in_flags;
    c.mOutputTime.mFlags = out_flags;
    return c;
}

/* Deterministic stereo test frame: a pure function of the frame time, with L and R distinct so
 * that any channel swap, misalignment or mixing of two frames is detectable. Exactly
 * representable as float. */
static float fl(uint64_t t) { return (float)(t % 1000003u) + 0.25f; }
static float fr(uint64_t t) { return -(float)((t * 7u + 3u) % 1000003u) - 0.5f; }
static uint32_t fbits(float f) {
    uint32_t b;
    memcpy(&b, &f, sizeof(b));
    return b;
}

static OSStatus write_speakers(uint64_t t, UInt32 frames, const float *stereo) {
    AudioServerPlugInIOCycleInfo c = cycle_info((double)t, (double)t, frames);
    return (*g_driver)->DoIOOperation(g_driver, CROSSPANE_DEV_SPEAKERS_APP, CROSSPANE_STREAM_SPEAKERS_APP,
                                      7, kAudioServerPlugInIOOperationWriteMix, frames, &c,
                                      (void *)(uintptr_t)stereo, NULL);
}
/* Fill and write frames [t, t+frames) of the deterministic pattern. */
static OSStatus write_pattern(uint64_t t, UInt32 frames) {
    float *buf = malloc((size_t)frames * 8 + 8);
    CHECK(buf != NULL, "pattern alloc");
    for (UInt32 k = 0; k < frames; ++k) {
        buf[2 * k] = fl(t + k);
        buf[2 * k + 1] = fr(t + k);
    }
    OSStatus s = write_speakers(t, frames, buf);
    free(buf);
    return s;
}
/* Read what the agent sees for the INPUT sample time `input_time` (frame time input_time-1024). */
static OSStatus read_speakers(uint64_t input_time, UInt32 frames, float *stereo) {
    AudioServerPlugInIOCycleInfo c = cycle_info((double)input_time, (double)input_time, frames);
    return (*g_driver)->DoIOOperation(g_driver, CROSSPANE_DEV_SPEAKERS_LOOPBACK,
                                      CROSSPANE_STREAM_SPEAKERS_LOOPBACK, 9,
                                      kAudioServerPlugInIOOperationReadInput, frames, &c, stereo, NULL);
}

static AudioServerPlugInClientInfo client_info(UInt32 id) {
    AudioServerPlugInClientInfo ci;
    memset(&ci, 0, sizeof(ci));
    ci.mClientID = id;
    ci.mProcessID = (pid_t)(1000 + id);
    ci.mIsNativeEndian = true;
    ci.mBundleID = NULL;
    return ci;
}
static OSStatus add_client(AudioObjectID dev, UInt32 id) {
    AudioServerPlugInClientInfo ci = client_info(id);
    return (*g_driver)->AddDeviceClient(g_driver, dev, &ci);
}
static OSStatus remove_client(AudioObjectID dev, UInt32 id) {
    AudioServerPlugInClientInfo ci = client_info(id);
    return (*g_driver)->RemoveDeviceClient(g_driver, dev, &ci);
}
static OSStatus start_io(AudioObjectID dev, UInt32 id) {
    return (*g_driver)->StartIO(g_driver, dev, id);
}
static OSStatus stop_io(AudioObjectID dev, UInt32 id) {
    return (*g_driver)->StopIO(g_driver, dev, id);
}
static bool running(AudioObjectID dev) {
    return u32(dev, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal) != 0;
}
/* Register and start a client in one go. */
static void begin(AudioObjectID dev, UInt32 id) {
    CHECK(add_client(dev, id) == noErr, "add client");
    CHECK(start_io(dev, id) == noErr, "start client");
}
static void finish(AudioObjectID dev, UInt32 id) {
    CHECK(stop_io(dev, id) == noErr, "stop client");
    CHECK(remove_client(dev, id) == noErr, "remove client");
}

/* ---- Parking threads inside the driver -------------------------------------------------------- */
/* A thread sets tl_parker (thread-local) and then calls into the driver; the shared hook parks it
 * the first time the driver reaches `point`, or runs `inject` there instead. Any number of threads
 * can be parked at once, each with its own parker. Drain waits are counted so tests can
 * acknowledge "the drain is really waiting" instead of sleeping. */
typedef struct parker {
    int point;
    void (*inject)(void);
    int fired;
    _Atomic int parked, resume;
} parker;
static __thread parker *tl_parker;
static _Atomic uint64_t g_drain_naps;
static void sleep_ms(unsigned ms);
static void fixture_hook(int point, uint64_t arg) {
    (void)arg;
    if (point == CP_HOOK_DRAIN_NAP) atomic_fetch_add(&g_drain_naps, 1);
    parker *p = tl_parker;
    if (!p || p->point != point || p->fired) return;
    p->fired = 1;
    if (p->inject) {
        p->inject();
        return;
    }
    atomic_store(&p->parked, 1);
    while (!atomic_load(&p->resume)) sleep_ms(1);
}
static void install_fixture_hook(void) { g_test_hook = fixture_hook; }
static void parker_init(parker *p, int point) {
    memset(p, 0, sizeof(*p));
    p->point = point;
}
static void parker_wait(parker *p) { /* bounded handshake: the thread has reached the point */
    for (int i = 0; i < 10000 && !atomic_load(&p->parked); ++i) sleep_ms(1);
    CHECK(atomic_load(&p->parked), "thread parked at its hook point");
}
static void parker_release(parker *p) { atomic_store(&p->resume, 1); }
/* Wait (bounded) until some drain has napped at least `n` more times: it is really waiting. */
static void wait_drain_naps(uint64_t since, uint64_t n) {
    for (int i = 0; i < 10000 && atomic_load(&g_drain_naps) < since + n; ++i) sleep_ms(1);
    CHECK(atomic_load(&g_drain_naps) >= since + n, "a drain is waiting");
}

typedef void *(*thread_fn)(void *);
static pthread_t spawn(thread_fn fn, void *arg) {
    pthread_t t;
    CHECK(pthread_create(&t, NULL, fn, arg) == 0, "pthread_create");
    return t;
}
static void join(pthread_t t) { CHECK(pthread_join(t, NULL) == 0, "pthread_join"); }

static void sleep_ms(unsigned ms) {
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

#endif
