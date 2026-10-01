/* Crosspane audio driver (WP-3.4): a fixed public-HAL loopback AudioServerPlugIn.
 *
 * Own original code, written against the public SDK header only
 * (CoreAudio/AudioServerPlugIn.h). No sample or third-party driver code is copied; P9's
 * lifecycle/refcount scaffolding was reused as an idea, with its Mach IPC, SPSC FIFO, clock
 * domain 0 and 480-frame period removed.
 *
 * What it publishes (WP-3.0b inventory freeze): exactly four static devices and four streams.
 *
 *   id  device (UID suffix)                 direction  channels  hidden
 *    2  Crosspane speakers   (speakers.app)       output  stereo    no     apps play into it
 *    3  speakers loopback    (speakers.loopback)  input   stereo    yes    the agent records it
 *    4  Crosspane microphone (microphone.app)     input   mono      no     apps record from it
 *    5  microphone loopback  (microphone.loopback) output mono      yes    the agent plays into it
 *   streams 6..9 belong to devices 2..5 in the same order.
 *
 * There is no IPC at all: no Mach service, no network, no helper process, no PID/Team handshake
 * and no client-HAL API call. The two halves of a pair talk through an in-driver history ring
 * (cp_history.h). In this slice the microphone pair is always silent: ReadInput on the
 * microphone returns zeros and WriteMix to its hidden output is discarded.
 *
 * Lifecycle: a PROCESS-LIFETIME SINGLETON, as in Apple's NullAudio sample. Release only
 * decrements the reference count (never below 0) and returns it; nothing is ever freed, retired or
 * unloaded by Release or by the CFPlugIn unload hook (CrosspaneAudioUnload is an idempotent
 * no-op). The executable is pinned for good (RTLD_NODELETE), and the history ring and the client
 * tables live until the driver host process exits. The host pointer is stored once, by the first
 * successful Initialize, and stays valid for the plug-in's lifetime (the host guarantees that);
 * notifications call it directly. A later Initialize with the same host is a successful no-op,
 * with a different host it is refused (kAudioHardwareIllegalOperationError). A Release to zero
 * followed by a new Factory call hands out the very same live singleton, data intact.
 *
 * Concurrency model
 *   control plane (non-real-time, may lock): Initialize/AddRef/Release, AddDeviceClient /
 *     RemoveDeviceClient, StartIO/StopIO, property queries. Client tables are guarded by one
 *     mutex; host notifications are always made with no driver lock held.
 *   data plane (real-time: GetZeroTimeStamp, WillDo/Begin/Do/EndIOOperation): allocates nothing,
 *     takes no lock, retains no HAL buffer, does bounded work (<= 4096 frames) and talks to the
 *     control plane only through lock-free atomics:
 *       g_ready: published (release) by the first successful Initialize; before it every entry
 *                point answers kAudioHardwareNotRunningError.
 *       g_xfer : the speaker transfer admission gate (cp_gate.h). Open exactly while the hidden
 *                speakers-loopback device is started; its 31-bit tag is the transfer generation.
 *
 * Stop linearization: StopIO of the last hidden-speakers client closes g_xfer first (writers stop
 * publishing at their next frame), then drains admitted old accesses, and only then reports
 * success. If the drain times out the stop is still recorded and publication stays closed, but
 * StopIO returns an error: it never claims a barrier it did not establish. The ring is never
 * reset in place: a restart is a new generation, so older frames are unreadable and old writers
 * can never overwrite newer frames. Readers and writers also re-check their transfer's generation
 * per frame, so an access that survives a stop never publishes or returns the stopped transfer's PCM.
 */
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <math.h>
#include <os/log.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "crosspane_audio.h"

/* ---- Direct-test hooks: compiled only into the detached test builds -------------------------- */
#ifdef CROSSPANE_AUDIO_TESTING
typedef void (*cp_test_hook_fn)(int point, uint64_t arg);
static cp_test_hook_fn g_test_hook;
#define CP_TEST_HOOK(point, arg)                                                            \
    do {                                                                                    \
        cp_test_hook_fn hook_ = g_test_hook;                                                \
        if (hook_) hook_((point), (uint64_t)(arg));                                         \
    } while (0)
static uint64_t (*g_test_clock)(void); /* injected host clock; NULL = mach_absolute_time */
static _Atomic int g_test_live_rings;  /* history rings allocated and not yet freed */
static bool g_test_pin_fail;           /* inject a failed executable pin */
#define CP_TEST_RING_COUNT(delta) atomic_fetch_add(&g_test_live_rings, (delta))
#else
#define CP_TEST_RING_COUNT(delta) ((void)0)
#endif

#include "cp_gate.h"
#include "cp_history.h"
#include "cp_timeline.h"

#define CP_EXPORT __attribute__((visibility("default")))
CP_EXPORT void *CrosspaneAudioFactory(CFAllocatorRef allocator, CFUUIDRef type);
CP_EXPORT void CrosspaneAudioUnload(CFPlugInRef plugin);
#define CP_LOG_ERROR(...) os_log_error(OS_LOG_DEFAULT, __VA_ARGS__)

/* ---- Static object table --------------------------------------------------------------------- */
#define CP_NUM_DEVS 4

typedef struct {
    AudioObjectID device, stream;
    CFStringRef uid, name;
    bool input;  /* the device's single stream is an input stream (the device captures) */
    bool hidden; /* IsHidden; never a default device */
    UInt32 channels;
} cp_dev;

static const cp_dev g_devs[CP_NUM_DEVS] = {
    {CROSSPANE_DEV_SPEAKERS_APP, CROSSPANE_STREAM_SPEAKERS_APP, CFSTR(CROSSPANE_SPEAKERS_APP_UID),
     CFSTR(CROSSPANE_SPEAKERS_APP_NAME), false, false, 2},
    {CROSSPANE_DEV_SPEAKERS_LOOPBACK, CROSSPANE_STREAM_SPEAKERS_LOOPBACK,
     CFSTR(CROSSPANE_SPEAKERS_LOOPBACK_UID), CFSTR(CROSSPANE_SPEAKERS_LOOPBACK_NAME), true, true, 2},
    {CROSSPANE_DEV_MIC_APP, CROSSPANE_STREAM_MIC_APP, CFSTR(CROSSPANE_MIC_APP_UID),
     CFSTR(CROSSPANE_MIC_APP_NAME), true, false, 1},
    {CROSSPANE_DEV_MIC_LOOPBACK, CROSSPANE_STREAM_MIC_LOOPBACK, CFSTR(CROSSPANE_MIC_LOOPBACK_UID),
     CFSTR(CROSSPANE_MIC_LOOPBACK_NAME), false, true, 1},
};

typedef enum { CP_KIND_NONE, CP_KIND_PLUGIN, CP_KIND_DEVICE, CP_KIND_STREAM } cp_kind;

static const cp_dev *cp_dev_by_id(AudioObjectID id) {
    return (id >= CROSSPANE_DEV_SPEAKERS_APP && id <= CROSSPANE_DEV_MIC_LOOPBACK)
               ? &g_devs[id - CROSSPANE_DEV_SPEAKERS_APP]
               : NULL;
}
static const cp_dev *cp_dev_by_stream(AudioObjectID id) {
    return (id >= CROSSPANE_STREAM_SPEAKERS_APP && id <= CROSSPANE_STREAM_MIC_LOOPBACK)
               ? &g_devs[id - CROSSPANE_STREAM_SPEAKERS_APP]
               : NULL;
}
static unsigned cp_index(const cp_dev *dv) { return (unsigned)(dv - g_devs); }
static cp_kind cp_kind_of(AudioObjectID id) {
    if (id == CROSSPANE_OBJ_PLUGIN) return CP_KIND_PLUGIN;
    if (cp_dev_by_id(id)) return CP_KIND_DEVICE;
    if (cp_dev_by_stream(id)) return CP_KIND_STREAM;
    return CP_KIND_NONE;
}

/* ---- Driver state ---------------------------------------------------------------------------- */
typedef struct {
    UInt32 id;
    bool used, started;
} cp_client;
typedef struct {
    cp_client slot[CROSSPANE_AUDIO_MAX_CLIENTS]; /* bounded, preallocated */
    UInt32 registered, started;
} cp_clients;

/* Lifecycle (guarded by g_lifecycle). Nothing here is ever torn down. */
static pthread_mutex_t g_lifecycle = PTHREAD_MUTEX_INITIALIZER;
static UInt32 g_refs;
static bool g_initialized;        /* the first Initialize succeeded (g_lifecycle) */
static _Atomic bool g_ready;      /* ...published for the entry points (release/acquire) */
static bool g_pinned;             /* CFPlugIn instance registration, taken once, never dropped */
static bool g_image_pinned;       /* dyld has been told never to unload this image */
static cp_gate g_xfer = {CP_GATE_CLOSED}; /* the speaker transfer; never goes away */
static _Atomic(AudioServerPlugInHostRef) g_host; /* stored once by the first Initialize */
static _Atomic(cp_hist_slot *) g_history;        /* allocated once, never freed; speakers pair only */

/* Control plane (guarded by g_control). */
static pthread_mutex_t g_control = PTHREAD_MUTEX_INITIALIZER;
static cp_clients g_clients[CP_NUM_DEVS];
static _Atomic UInt32 g_running[CP_NUM_DEVS]; /* mirrors started > 0, lock-free for queries */
/* Transfer generations are never reused for the life of the process and never wrap; when the
 * 31-bit space is exhausted new transfers are refused. Lifecycle changes bump the generation but
 * never the clock epoch. */
static uint32_t g_next_generation = 1;

static uint64_t g_stop_drain_ns = 500ull * 1000 * 1000; /* StopIO barrier bound */

/* The immutable shared timeline: set once when the image loads, read-only afterwards. */
static cp_timeline g_timeline;

/* ---- Driver identity ------------------------------------------------------------------------- */
static AudioServerPlugInDriverInterface g_interface;
static AudioServerPlugInDriverInterface *g_interface_ptr = &g_interface;
static AudioServerPlugInDriverRef g_driver = &g_interface_ptr;

static bool cp_is_driver(const void *d) { return d == (const void *)g_driver; }

static inline uint64_t cp_host_time(void) {
#ifdef CROSSPANE_AUDIO_TESTING
    if (g_test_clock) return g_test_clock();
#endif
    return mach_absolute_time();
}

__attribute__((constructor)) static void cp_load(void) {
    mach_timebase_info_data_t tb;
    if (mach_timebase_info(&tb) != KERN_SUCCESS || tb.numer == 0 || tb.denom == 0) return;
    g_timeline.epoch = mach_absolute_time();
    g_timeline.numer = tb.numer;
    g_timeline.denom = tb.denom;
}

/* Every entry point other than the IUnknown methods and Initialize needs the first Initialize. */
static bool cp_ready(void) { return atomic_load(&g_ready); }

/* ---- Host notification (control plane, called with no driver lock held) ---------------------- */
/* The host pointer was stored by the first Initialize before g_ready was published and stays valid
 * for the plug-in's lifetime, so it is called directly. */
static void cp_notify_running(const cp_dev *dv) {
    AudioServerPlugInHostRef host = atomic_load(&g_host);
    if (!host || !host->PropertiesChanged) return;
    const AudioObjectPropertyAddress addr = {kAudioDevicePropertyDeviceIsRunning,
                                             kAudioObjectPropertyScopeGlobal,
                                             kAudioObjectPropertyElementMain};
    (void)host->PropertiesChanged(host, dv->device, 1, &addr);
}

/* ---- Lifecycle ------------------------------------------------------------------------------- */
static cp_hist_slot *cp_ring_alloc(void) {
    cp_hist_slot *ring = calloc(CROSSPANE_AUDIO_HISTORY_FRAMES, sizeof(*ring));
    if (!ring) return NULL;
    CP_TEST_RING_COUNT(1);
    for (unsigned i = 0; i < CROSSPANE_AUDIO_HISTORY_FRAMES; ++i) {
        atomic_init(&ring[i].meta, 0);
        atomic_init(&ring[i].pcm, 0);
    }
    return ring;
}

/* Pin the executable's code. Two layers:
 *   1. CFPlugInAddInstanceForFactory, taken once at the first instance and never dropped: CFPlugIn
 *      itself will not unload the bundle.
 *   2. a dyld reference with RTLD_NODELETE, taken once and deliberately never dropped: an explicit
 *      CFBundleUnloadExecutable ignores layer 1 (observed: it unmapped the executable under a
 *      live instance and the next call then faulted). Once an instance has existed no code of this
 *      image is ever unmapped, which is what makes a process-lifetime driver safe: no callback can
 *      return into freed code, and no state is ever freed.
 * If layer 2 cannot be taken the factory FAILS: an instance whose code could be unmapped under a
 * callback is never handed out. The detached test builds inject this failure instead. */
#ifndef CROSSPANE_AUDIO_TESTING
static void cp_pin_image(void) {
    Dl_info info;
    if (dladdr((const void *)CrosspaneAudioFactory, &info) && info.dli_fname &&
        dlopen(info.dli_fname, RTLD_LAZY | RTLD_NOLOAD | RTLD_NODELETE)) { /* handle kept: the pin */
        g_image_pinned = true;
        return;
    }
    CP_LOG_ERROR("Crosspane audio: could not pin the executable; refusing to create an instance");
}
#else
static void cp_pin_image(void) { g_image_pinned = !g_test_pin_fail; }
#endif

static ULONG cp_add_ref(void *d) {
    if (!cp_is_driver(d)) return 0;
    pthread_mutex_lock(&g_lifecycle);
    if (g_refs != UINT32_MAX) ++g_refs;
    const ULONG n = g_refs;
    pthread_mutex_unlock(&g_lifecycle);
    return n;
}

static HRESULT cp_query_interface(void *d, REFIID uuid, LPVOID *out) {
    if (!out) return E_POINTER;
    *out = NULL;
    if (!cp_is_driver(d)) return E_NOINTERFACE;
    CFUUIDBytes driver_uuid = CFUUIDGetUUIDBytes(kAudioServerPlugInDriverInterfaceUUID);
    CFUUIDBytes unknown_uuid = CFUUIDGetUUIDBytes(IUnknownUUID);
    if (memcmp(&uuid, &driver_uuid, sizeof(uuid)) != 0 &&
        memcmp(&uuid, &unknown_uuid, sizeof(uuid)) != 0)
        return E_NOINTERFACE;
    (void)cp_add_ref(d);
    *out = g_driver;
    return S_OK;
}

/* Only decrements (never below 0) and returns the count. Nothing is freed, retired or unloaded:
 * the driver is a process-lifetime singleton, and a Release to zero followed by a new Factory call
 * hands out the same live instance with its data intact. */
static ULONG cp_release(void *d) {
    if (!cp_is_driver(d)) return 0;
    pthread_mutex_lock(&g_lifecycle);
    if (g_refs != 0) --g_refs;
    const ULONG n = g_refs;
    pthread_mutex_unlock(&g_lifecycle);
    return n;
}

static OSStatus cp_initialize(AudioServerPlugInDriverRef d, AudioServerPlugInHostRef host) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    if (!host || !host->PropertiesChanged) return kAudioHardwareIllegalOperationError;
    pthread_mutex_lock(&g_lifecycle);
    OSStatus result = kAudioHardwareUnspecifiedError;
    if (g_initialized) {
        /* The host reference lives as long as the plug-in: the same host is a no-op, another one
         * is refused (a driver host has exactly one host interface). */
        result = atomic_load(&g_host) == host ? noErr : kAudioHardwareIllegalOperationError;
        goto done;
    }
    if (g_refs == 0 || !cp_timeline_valid(&g_timeline)) goto done;
    cp_hist_slot *ring = cp_ring_alloc();
    if (!ring) {
        CP_LOG_ERROR("Crosspane audio: history allocation failed");
        goto done;
    }
    atomic_store(&g_history, ring);
    atomic_store(&g_host, host);
    cp_gate_close(&g_xfer);
    g_initialized = true;
    atomic_store(&g_ready, true); /* last: publishes the ring and the host to every entry point */
    result = noErr;
done:
    pthread_mutex_unlock(&g_lifecycle);
    return result;
}

CP_EXPORT void *CrosspaneAudioFactory(CFAllocatorRef allocator, CFUUIDRef type) {
    (void)allocator;
    if (!type || !CFEqual(type, kAudioServerPlugInTypeUUID)) return NULL; /* a rejected load pins nothing */
    pthread_mutex_lock(&g_lifecycle);
    if (g_refs == UINT32_MAX) {
        pthread_mutex_unlock(&g_lifecycle);
        return NULL;
    }
    if (!g_image_pinned) cp_pin_image();
    if (!g_image_pinned) { /* the pin failed: no instance, no reference taken */
        pthread_mutex_unlock(&g_lifecycle);
        return NULL;
    }
    if (!g_pinned) {
        /* Layer 1, once: CFPlugIn is told an instance exists, and is never told otherwise. */
        CFUUIDRef uuid = CFUUIDCreateFromString(NULL, CFSTR(CROSSPANE_AUDIO_FACTORY_UUID));
        if (uuid) {
            CFPlugInAddInstanceForFactory(uuid);
            CFRelease(uuid);
            g_pinned = true;
        }
    }
    ++g_refs;
    pthread_mutex_unlock(&g_lifecycle);
    return g_driver;
}

/* The CFPlugIn unload hook stays exported (the plist names it) and does nothing: the singleton is
 * never torn down, so there is nothing to retire. Calling it any number of times is harmless. */
CP_EXPORT void CrosspaneAudioUnload(CFPlugInRef plugin) { (void)plugin; }

#ifdef CROSSPANE_AUDIO_TESTING
/* TEST-ONLY (CROSSPANE_AUDIO_TESTING builds; absent from the production driver): re-create the
 * singleton's state between fixture cases. Call only from single-threaded test setup, with no
 * driver call in flight on any thread. The clock epoch, the transfer generation counter and the
 * image pin are process-lifetime and are deliberately left alone. */
static void cp_ring_free(cp_hist_slot *ring) {
    if (!ring) return;
    CP_TEST_RING_COUNT(-1);
    free(ring);
}
static void cp_test_reset(void) {
    pthread_mutex_lock(&g_lifecycle);
    atomic_store(&g_ready, false);
    g_initialized = false;
    g_refs = 0;
    atomic_store(&g_host, NULL);
    cp_ring_free(atomic_exchange(&g_history, NULL));
    pthread_mutex_lock(&g_control);
    memset(g_clients, 0, sizeof(g_clients));
    for (unsigned i = 0; i < CP_NUM_DEVS; ++i) atomic_store(&g_running[i], 0);
    pthread_mutex_unlock(&g_control);
    atomic_store(&g_xfer.word, CP_GATE_CLOSED);
    pthread_mutex_unlock(&g_lifecycle);
}
#endif

/* ---- Properties ------------------------------------------------------------------------------ */
static OSStatus cp_filter_match(UInt32 qsize, const void *q, AudioClassID cls, bool *match) {
    *match = true;
    if (qsize == 0) return noErr;
    if (!q || qsize % sizeof(AudioClassID) != 0) return kAudioHardwareBadPropertySizeError;
    *match = false;
    for (UInt32 i = 0; i < qsize / sizeof(AudioClassID); ++i) {
        AudioClassID c;
        memcpy(&c, (const char *)q + i * sizeof(c), sizeof(c));
        if (c == kAudioObjectClassID || c == kAudioObjectClassIDWildcard || c == cls) *match = true;
    }
    return noErr;
}

/* Qualifier of the UID translation properties: exactly one CFStringRef. */
static OSStatus cp_uid_qualifier(UInt32 qsize, const void *q, UInt32 capacity, CFStringRef *uid) {
    if (!q || qsize != sizeof(CFStringRef) || capacity < sizeof(AudioObjectID))
        return kAudioHardwareBadPropertySizeError;
    CFStringRef s;
    memcpy(&s, q, sizeof(s));
    if (!s || CFGetTypeID(s) != CFStringGetTypeID()) return kAudioHardwareIllegalOperationError;
    *uid = s;
    return noErr;
}

static AudioObjectID cp_translate_uid(CFStringRef uid) {
    for (unsigned i = 0; i < CP_NUM_DEVS; ++i)
        if (CFStringCompare(uid, g_devs[i].uid, 0) == kCFCompareEqualTo) return g_devs[i].device;
    return kAudioObjectUnknown;
}

static AudioStreamBasicDescription cp_format(UInt32 channels) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = CROSSPANE_AUDIO_RATE;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagsNativeFloatPacked;
    f.mBytesPerPacket = channels * (UInt32)sizeof(float);
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = channels * (UInt32)sizeof(float);
    f.mChannelsPerFrame = channels;
    f.mBitsPerChannel = 32;
    return f;
}

/* One routine answers HasProperty, GetPropertyDataSize and GetPropertyData. With dst == NULL it
 * only sizes the property. Scope and element are honoured only where the value depends on them. */
static OSStatus cp_property(AudioObjectID obj, const AudioObjectPropertyAddress *a, UInt32 qsize,
                            const void *q, UInt32 capacity, UInt32 *size, void *dst) {
    cp_kind kind = cp_kind_of(obj);
    if (kind == CP_KIND_NONE) return kAudioHardwareBadObjectError;
    if (!a || !size) return kAudioHardwareIllegalOperationError;
    const cp_dev *dv = kind == CP_KIND_DEVICE   ? cp_dev_by_id(obj)
                       : kind == CP_KIND_STREAM ? cp_dev_by_stream(obj)
                                                : NULL;
    union {
        UInt32 u32;
        Float64 f64;
        AudioObjectID ids[CP_NUM_DEVS];
        UInt32 pair[2];
        AudioValueRange range;
        AudioStreamBasicDescription asbd;
        AudioStreamRangedDescription ranged;
    } v;
    memset(&v, 0, sizeof(v));
    const void *data = &v;
    UInt32 total = 0, item = 0; /* item != 0: a list whose elements may be fetched partially */
    CFStringRef str = NULL;
    const AudioObjectPropertyScope own_scope =
        (dv && dv->input) ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
    const bool in_own_scope = dv && a->mScope == own_scope;
    OSStatus status;
    bool match;

    switch (a->mSelector) {
    /* -- AudioObject -- */
    case kAudioObjectPropertyBaseClass:
        v.u32 = kAudioObjectClassID;
        total = sizeof(UInt32);
        break;
    case kAudioObjectPropertyClass:
        v.u32 = kind == CP_KIND_PLUGIN   ? kAudioPlugInClassID
                : kind == CP_KIND_DEVICE ? kAudioDeviceClassID
                                         : kAudioStreamClassID;
        total = sizeof(UInt32);
        break;
    case kAudioObjectPropertyOwner:
        v.u32 = kind == CP_KIND_PLUGIN   ? kAudioObjectUnknown
                : kind == CP_KIND_DEVICE ? CROSSPANE_OBJ_PLUGIN
                                         : dv->device;
        total = sizeof(UInt32);
        break;
    case kAudioObjectPropertyName:
        if (kind == CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        str = kind == CP_KIND_PLUGIN ? CFSTR("Crosspane audio") : dv->name;
        break;
    case kAudioObjectPropertyManufacturer:
        if (kind == CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        str = CFSTR(CROSSPANE_AUDIO_MANUFACTURER);
        break;
    case kAudioObjectPropertyOwnedObjects:
        item = sizeof(AudioObjectID);
        if (kind == CP_KIND_PLUGIN) {
            if ((status = cp_filter_match(qsize, q, kAudioDeviceClassID, &match)) != noErr)
                return status;
            for (unsigned i = 0; i < CP_NUM_DEVS; ++i) v.ids[i] = g_devs[i].device;
            total = match ? CP_NUM_DEVS * sizeof(AudioObjectID) : 0;
        } else if (kind == CP_KIND_DEVICE) {
            if ((status = cp_filter_match(qsize, q, kAudioStreamClassID, &match)) != noErr)
                return status;
            v.ids[0] = dv->stream;
            /* Owned objects are scoped like Streams: only the device's own direction (and global). */
            total = (match && (in_own_scope || a->mScope == kAudioObjectPropertyScopeGlobal))
                        ? sizeof(AudioObjectID)
                        : 0;
        } else {
            total = 0;
        }
        break;
    /* -- AudioPlugIn -- */
    case kAudioPlugInPropertyBundleID:
        if (kind != CP_KIND_PLUGIN) return kAudioHardwareUnknownPropertyError;
        str = CFSTR(CROSSPANE_AUDIO_BUNDLE_ID);
        break;
    case kAudioPlugInPropertyDeviceList:
        if (kind != CP_KIND_PLUGIN) return kAudioHardwareUnknownPropertyError;
        for (unsigned i = 0; i < CP_NUM_DEVS; ++i) v.ids[i] = g_devs[i].device;
        total = CP_NUM_DEVS * sizeof(AudioObjectID);
        item = sizeof(AudioObjectID);
        break;
    case kAudioPlugInPropertyBoxList:
    case kAudioPlugInPropertyClockDeviceList:
        if (kind != CP_KIND_PLUGIN) return kAudioHardwareUnknownPropertyError;
        total = 0;
        item = sizeof(AudioObjectID);
        break;
    case kAudioPlugInPropertyTranslateUIDToDevice:
    case kAudioPlugInPropertyTranslateUIDToBox:
    case kAudioPlugInPropertyTranslateUIDToClockDevice:
        if (kind != CP_KIND_PLUGIN) return kAudioHardwareUnknownPropertyError;
        total = sizeof(AudioObjectID);
        if (dst) {
            CFStringRef uid = NULL;
            if ((status = cp_uid_qualifier(qsize, q, capacity, &uid)) != noErr) return status;
            /* Hidden devices resolve too: this is how the agent finds them. Boxes and clock
             * devices do not exist, so those translations always answer "unknown". */
            v.u32 = a->mSelector == kAudioPlugInPropertyTranslateUIDToDevice ? cp_translate_uid(uid)
                                                                              : kAudioObjectUnknown;
        }
        break;
    /* -- AudioDevice -- */
    case kAudioDevicePropertyDeviceUID:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        str = dv->uid;
        break;
    case kAudioDevicePropertyModelUID:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        str = CFSTR(CROSSPANE_AUDIO_MODEL_UID);
        break;
    case kAudioDevicePropertyTransportType:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = kAudioDeviceTransportTypeVirtual;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyRelatedDevices:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.ids[0] = dv->device;
        total = sizeof(AudioObjectID);
        item = sizeof(AudioObjectID);
        break;
    case kAudioDevicePropertyClockDomain:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = CROSSPANE_AUDIO_CLOCK_DOMAIN;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyDeviceIsAlive:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = 1;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyDeviceIsRunning:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = atomic_load(&g_running[cp_index(dv)]) != 0;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyDeviceCanBeDefaultDevice:
    case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        /* Visible devices only, and only in their app-facing direction. Nothing here ever
         * changes a system default. */
        v.u32 = (!dv->hidden && in_own_scope) ? 1 : 0;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyLatency:
        if (kind == CP_KIND_STREAM) {
            v.u32 = 0; /* the fixed transfer delay is reported once, on the device */
        } else if (kind == CP_KIND_DEVICE) {
            v.u32 = (dv->device == CROSSPANE_DEV_SPEAKERS_LOOPBACK && in_own_scope)
                        ? CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES
                        : 0;
        } else {
            return kAudioHardwareUnknownPropertyError;
        }
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyStreams:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.ids[0] = dv->stream;
        total = (in_own_scope || a->mScope == kAudioObjectPropertyScopeGlobal) ? sizeof(AudioObjectID)
                                                                              : 0;
        item = sizeof(AudioObjectID);
        break;
    case kAudioObjectPropertyControlList:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        total = 0;
        item = sizeof(AudioObjectID);
        break;
    case kAudioDevicePropertySafetyOffset:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = 0;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyNominalSampleRate:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.f64 = CROSSPANE_AUDIO_RATE;
        total = sizeof(Float64);
        break;
    case kAudioDevicePropertyAvailableNominalSampleRates:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.range.mMinimum = CROSSPANE_AUDIO_RATE;
        v.range.mMaximum = CROSSPANE_AUDIO_RATE;
        total = sizeof(AudioValueRange);
        item = sizeof(AudioValueRange);
        break;
    case kAudioDevicePropertyIsHidden:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = dv->hidden ? 1 : 0;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyZeroTimeStampPeriod:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = CROSSPANE_AUDIO_TIMESTAMP_PERIOD;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyClockAlgorithm:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = kAudioDeviceClockAlgorithmRaw;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyClockIsStable:
        if (kind != CP_KIND_DEVICE) return kAudioHardwareUnknownPropertyError;
        v.u32 = 1;
        total = sizeof(UInt32);
        break;
    case kAudioDevicePropertyPreferredChannelsForStereo:
        if (kind != CP_KIND_DEVICE || dv->channels != 2 || !in_own_scope)
            return kAudioHardwareUnknownPropertyError;
        v.pair[0] = 1;
        v.pair[1] = 2;
        total = sizeof(v.pair);
        break;
    /* -- AudioStream -- */
    case kAudioStreamPropertyIsActive:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.u32 = 1;
        total = sizeof(UInt32);
        break;
    case kAudioStreamPropertyDirection:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.u32 = dv->input ? 1 : 0; /* 0 = output, 1 = input */
        total = sizeof(UInt32);
        break;
    case kAudioStreamPropertyTerminalType:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.u32 = kAudioStreamTerminalTypeLine;
        total = sizeof(UInt32);
        break;
    case kAudioStreamPropertyStartingChannel:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.u32 = 1;
        total = sizeof(UInt32);
        break;
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.asbd = cp_format(dv->channels);
        total = sizeof(AudioStreamBasicDescription);
        break;
    case kAudioStreamPropertyAvailableVirtualFormats:
    case kAudioStreamPropertyAvailablePhysicalFormats:
        if (kind != CP_KIND_STREAM) return kAudioHardwareUnknownPropertyError;
        v.ranged.mFormat = cp_format(dv->channels);
        v.ranged.mSampleRateRange.mMinimum = CROSSPANE_AUDIO_RATE;
        v.ranged.mSampleRateRange.mMaximum = CROSSPANE_AUDIO_RATE;
        total = sizeof(AudioStreamRangedDescription);
        item = sizeof(AudioStreamRangedDescription);
        break;
    default:
        return kAudioHardwareUnknownPropertyError;
    }

    if (str) {
        data = &str;
        total = sizeof(CFStringRef);
        item = 0;
    }
    if (!dst) {
        *size = total;
        return noErr;
    }
    UInt32 n = total;
    if (item != 0) {
        n = (capacity / item) * item;
        if (n > total) n = total;
    } else if (capacity < total) {
        return kAudioHardwareBadPropertySizeError;
    }
    if (str) CFRetain(str); /* the caller releases returned CF objects */
    if (n) memcpy(dst, data, n);
    *size = n;
    return noErr;
}

static Boolean cp_has_property(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid,
                               const AudioObjectPropertyAddress *a) {
    (void)pid;
    if (!cp_is_driver(d) || !cp_ready()) return false;
    UInt32 n;
    Boolean has = cp_property(obj, a, 0, NULL, 0, &n, NULL) == noErr;
    return has;
}

static bool cp_selector_settable(AudioObjectID obj, AudioObjectPropertySelector sel) {
    cp_kind kind = cp_kind_of(obj);
    if (kind == CP_KIND_DEVICE) return sel == kAudioDevicePropertyNominalSampleRate;
    if (kind == CP_KIND_STREAM)
        return sel == kAudioStreamPropertyVirtualFormat || sel == kAudioStreamPropertyPhysicalFormat;
    return false;
}

static OSStatus cp_is_settable(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid,
                               const AudioObjectPropertyAddress *a, Boolean *out) {
    (void)pid;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    if (!out || !a) return kAudioHardwareIllegalOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    UInt32 n;
    OSStatus status = cp_property(obj, a, 0, NULL, 0, &n, NULL);
    *out = status == noErr && cp_selector_settable(obj, a->mSelector);
    return status;
}

static OSStatus cp_get_size(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid,
                            const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q,
                            UInt32 *out) {
    (void)pid;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = cp_property(obj, a, qsize, q, 0, out, NULL);
    return status;
}

static OSStatus cp_get_data(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid,
                            const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q,
                            UInt32 capacity, UInt32 *out_size, void *out) {
    (void)pid;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    if (!out) return kAudioHardwareIllegalOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = cp_property(obj, a, qsize, q, capacity, out_size, out);
    return status;
}

/* The devices expose exactly one format and one rate. A client may "set" that same value (a
 * no-op); anything else is refused. Nothing here changes IO, so no configuration change request
 * is ever needed. */
static OSStatus cp_set_data(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid,
                            const AudioObjectPropertyAddress *a, UInt32 qsize, const void *q,
                            UInt32 size, const void *data) {
    (void)pid;
    (void)qsize;
    (void)q;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    if (!a || !data) return kAudioHardwareIllegalOperationError;
    cp_kind kind = cp_kind_of(obj);
    if (kind == CP_KIND_NONE) return kAudioHardwareBadObjectError;
    if (!cp_selector_settable(obj, a->mSelector)) return kAudioHardwareUnsupportedOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = noErr;
    if (kind == CP_KIND_DEVICE) {
        Float64 rate;
        if (size != sizeof(rate)) {
            status = kAudioHardwareBadPropertySizeError;
        } else {
            memcpy(&rate, data, sizeof(rate));
            if (rate != (Float64)CROSSPANE_AUDIO_RATE) status = kAudioHardwareIllegalOperationError;
        }
    } else {
        AudioStreamBasicDescription want = cp_format(cp_dev_by_stream(obj)->channels), got;
        if (size != sizeof(got)) {
            status = kAudioHardwareBadPropertySizeError;
        } else {
            memcpy(&got, data, sizeof(got));
            if (got.mSampleRate != want.mSampleRate || got.mFormatID != want.mFormatID ||
                got.mFormatFlags != want.mFormatFlags || got.mBytesPerPacket != want.mBytesPerPacket ||
                got.mFramesPerPacket != want.mFramesPerPacket ||
                got.mBytesPerFrame != want.mBytesPerFrame ||
                got.mChannelsPerFrame != want.mChannelsPerFrame ||
                got.mBitsPerChannel != want.mBitsPerChannel)
                status = kAudioDeviceUnsupportedFormatError;
        }
    }
    return status;
}

/* ---- Devices are static ---------------------------------------------------------------------- */
static OSStatus cp_create_device(AudioServerPlugInDriverRef d, CFDictionaryRef desc,
                                 const AudioServerPlugInClientInfo *c, AudioObjectID *out) {
    (void)d;
    (void)desc;
    (void)c;
    (void)out;
    return kAudioHardwareUnsupportedOperationError;
}
static OSStatus cp_destroy_device(AudioServerPlugInDriverRef d, AudioObjectID obj) {
    (void)d;
    (void)obj;
    return kAudioHardwareUnsupportedOperationError;
}
static OSStatus cp_config_change(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt64 action,
                                 void *info) {
    (void)d;
    (void)obj;
    (void)action;
    (void)info;
    return kAudioHardwareUnsupportedOperationError; /* we never request one */
}

/* ---- Clients and demand (control plane) ------------------------------------------------------ */
/* Demand is the number of registered clients whose StartIO has been balanced by no StopIO, per
 * device, independent of every other device. A hidden device never affects a visible device's
 * state. A "started" client holds demand even if it never produces a sample (a NULL start). */
static cp_client *cp_find_client(cp_clients *c, UInt32 id) {
    for (unsigned i = 0; i < CROSSPANE_AUDIO_MAX_CLIENTS; ++i)
        if (c->slot[i].used && c->slot[i].id == id) return &c->slot[i];
    return NULL;
}

static OSStatus cp_add_client(AudioServerPlugInDriverRef d, AudioObjectID obj,
                              const AudioServerPlugInClientInfo *info) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    /* The host's own client id is not a registrable client. The bundle id string in `info` is
     * deliberately not retained: nothing of the HAL's is kept. */
    if (!info || info->mClientID == kAudioServerPlugInHostClientID)
        return kAudioHardwareIllegalOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = noErr;
    pthread_mutex_lock(&g_control);
    cp_clients *c = &g_clients[cp_index(dv)];
    if (!cp_find_client(c, info->mClientID)) { /* a duplicate registration is a no-op */
        if (c->registered >= CROSSPANE_AUDIO_MAX_CLIENTS) {
            status = kAudioHardwareIllegalOperationError; /* bounded: reject overflow */
        } else {
            for (unsigned i = 0; i < CROSSPANE_AUDIO_MAX_CLIENTS; ++i) {
                if (!c->slot[i].used) {
                    c->slot[i] = (cp_client){info->mClientID, true, false};
                    ++c->registered;
                    break;
                }
            }
        }
    }
    pthread_mutex_unlock(&g_control);
    return status;
}

/* Close the speaker transfer: disable new publication first, then drain admitted old accesses.
 * Caller holds g_control. Returns false if the barrier could not be established. */
static bool cp_close_transfer_locked(void) {
    cp_gate_close(&g_xfer);
    CP_TEST_HOOK(CP_HOOK_STOP_DRAINING, 0);
    return cp_gate_drain(&g_xfer, g_stop_drain_ns);
}

/* Stop one started client of `dv`. Caller holds g_control. Sets *edge when the device's running
 * state really changed and *barrier to false if a transfer drain timed out. */
static void cp_stop_client_locked(const cp_dev *dv, cp_client *cl, bool *edge, bool *barrier) {
    cp_clients *c = &g_clients[cp_index(dv)];
    cl->started = false;
    --c->started;
    if (c->started == 0) {
        atomic_store(&g_running[cp_index(dv)], 0);
        *edge = true;
        if (dv->device == CROSSPANE_DEV_SPEAKERS_LOOPBACK && !cp_close_transfer_locked())
            *barrier = false;
    }
}

static OSStatus cp_remove_client(AudioServerPlugInDriverRef d, AudioObjectID obj,
                                 const AudioServerPlugInClientInfo *info) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (!info) return kAudioHardwareIllegalOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = noErr;
    bool edge = false, barrier = true;
    pthread_mutex_lock(&g_control);
    cp_clients *c = &g_clients[cp_index(dv)];
    cp_client *cl = cp_find_client(c, info->mClientID);
    if (!cl) {
        status = kAudioHardwareIllegalOperationError; /* never removes (or underflows) anything */
    } else {
        /* A client that vanishes (a crash) still holding a start gives up its demand here. */
        if (cl->started) cp_stop_client_locked(dv, cl, &edge, &barrier);
        *cl = (cp_client){0, false, false};
        --c->registered;
    }
    pthread_mutex_unlock(&g_control);
    if (edge) cp_notify_running(dv);
    return status == noErr && !barrier ? kAudioHardwareUnspecifiedError : status;
}

static OSStatus cp_start_io(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = noErr;
    bool edge = false;
    pthread_mutex_lock(&g_control);
    cp_clients *c = &g_clients[cp_index(dv)];
    cp_client *cl = cp_find_client(c, client);
    if (!cl || cl->started) {
        status = kAudioHardwareIllegalOperationError; /* unregistered or duplicate start */
    } else if (c->started == 0 && dv->device == CROSSPANE_DEV_SPEAKERS_LOOPBACK &&
               g_next_generation > CP_HIST_GEN_MAX) {
        CP_LOG_ERROR("Crosspane audio: transfer generations exhausted; refusing to start");
        status = kAudioHardwareUnspecifiedError; /* exhaustion refuses, it never wraps */
    } else {
        if (c->started == 0) {
            if (dv->device == CROSSPANE_DEV_SPEAKERS_LOOPBACK)
                cp_gate_open(&g_xfer, g_next_generation++); /* a fresh transfer: no old backlog */
            atomic_store(&g_running[cp_index(dv)], 1);
            edge = true;
        }
        cl->started = true;
        ++c->started;
    }
    pthread_mutex_unlock(&g_control);
    if (edge) cp_notify_running(dv);
    return status;
}

static OSStatus cp_stop_io(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    OSStatus status = noErr;
    bool edge = false, barrier = true;
    pthread_mutex_lock(&g_control);
    cp_client *cl = cp_find_client(&g_clients[cp_index(dv)], client);
    if (!cl || !cl->started)
        status = kAudioHardwareIllegalOperationError; /* unmatched stop: nothing is decremented */
    else
        cp_stop_client_locked(dv, cl, &edge, &barrier);
    pthread_mutex_unlock(&g_control);
    if (edge) cp_notify_running(dv);
    return status == noErr && !barrier ? kAudioHardwareUnspecifiedError : status;
}

/* ---- Clock (data plane) ---------------------------------------------------------------------- */
static OSStatus cp_zero_time(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client,
                             Float64 *sample, UInt64 *host, UInt64 *seed) {
    (void)client;
    if (!cp_is_driver(d) || !cp_dev_by_id(obj)) return kAudioHardwareBadObjectError;
    if (!sample || !host || !seed) return kAudioHardwareIllegalOperationError;
    if (!cp_ready()) return kAudioHardwareNotRunningError;
    /* The same pure function of (immutable epoch, host time) for all four devices, whatever the
     * IO state: starts, stops and hidden-partner activity never move it. */
    double s;
    uint64_t h, sd;
    const bool ok = cp_timeline_zero(&g_timeline, cp_host_time(), &s, &h, &sd);
    if (ok) {
        *sample = s;
        *host = h;
        *seed = sd;
    }
    return ok ? noErr : kAudioHardwareUnspecifiedError; /* beyond the representable timeline */
}

/* ---- IO operations (data plane) -------------------------------------------------------------- */
static bool cp_known_operation(UInt32 op) {
    switch (op) {
    case kAudioServerPlugInIOOperationThread:
    case kAudioServerPlugInIOOperationCycle:
    case kAudioServerPlugInIOOperationReadInput:
    case kAudioServerPlugInIOOperationConvertInput:
    case kAudioServerPlugInIOOperationProcessInput:
    case kAudioServerPlugInIOOperationProcessOutput:
    case kAudioServerPlugInIOOperationMixOutput:
    case kAudioServerPlugInIOOperationProcessMix:
    case kAudioServerPlugInIOOperationConvertMix:
    case kAudioServerPlugInIOOperationWriteMix:
        return true;
    default:
        return false;
    }
}

/* Each device performs exactly one operation: input devices ReadInput, output devices WriteMix. */
static UInt32 cp_device_operation(const cp_dev *dv) {
    return dv->input ? kAudioServerPlugInIOOperationReadInput : kAudioServerPlugInIOOperationWriteMix;
}

static OSStatus cp_will_do(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client, UInt32 op,
                           Boolean *will, Boolean *in_place) {
    (void)client;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (!will || !in_place) return kAudioHardwareIllegalOperationError;
    *will = cp_known_operation(op) && op == cp_device_operation(dv);
    *in_place = true;
    return noErr;
}

/* The operation's time stamp must say its sample time is valid (kAudioTimeStampSampleTimeValid;
 * a host-time-only or flagless stamp is malformed), and the sample time must be finite,
 * non-negative and addressable (NaN fails every comparison). */
static bool cp_sample_time(const AudioTimeStamp *ts, uint64_t *out) {
    if (!(ts->mFlags & kAudioTimeStampSampleTimeValid)) return false;
    const Float64 x = ts->mSampleTime;
    const Float64 max = (Float64)(CP_HIST_TIME_LIMIT - CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES - 1);
    if (!(x >= 0.0) || !(x <= max)) return false;
    *out = (uint64_t)(x + 0.5);
    return true;
}

/* Begin/End carry no work. They still validate what DoIOOperation validates: device, operation,
 * frame count and, except for the thread/cycle brackets, the cycle info and its sample time. */
static OSStatus cp_bracket_io(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 op, UInt32 frames,
                              const AudioServerPlugInIOCycleInfo *info) {
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (!cp_known_operation(op)) return kAudioHardwareUnsupportedOperationError;
    if (frames > CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES) return kAudioHardwareIllegalOperationError;
    const bool bracket = op == kAudioServerPlugInIOOperationThread || op == kAudioServerPlugInIOOperationCycle;
    if (bracket || frames == 0) return noErr;
    if (!info) return kAudioHardwareIllegalOperationError;
    uint64_t t;
    if (op == cp_device_operation(dv) &&
        !cp_sample_time(dv->input ? &info->mInputTime : &info->mOutputTime, &t))
        return kAudioHardwareIllegalOperationError;
    return noErr;
}
static OSStatus cp_begin_io(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client, UInt32 op,
                            UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)client;
    return cp_bracket_io(d, obj, op, frames, info);
}
static OSStatus cp_end_io(AudioServerPlugInDriverRef d, AudioObjectID obj, UInt32 client, UInt32 op,
                          UInt32 frames, const AudioServerPlugInIOCycleInfo *info) {
    (void)client;
    return cp_bracket_io(d, obj, op, frames, info);
}

static void cp_write_speakers(uint64_t t, UInt32 frames, const void *buf) {
    uint32_t gen;
    if (!cp_gate_enter(&g_xfer, &gen)) return; /* hidden side stopped: speaker writes are discarded */
    CP_TEST_HOOK(CP_HOOK_IO_ADMITTED, CROSSPANE_DEV_SPEAKERS_APP);
    cp_hist_slot *hist = atomic_load(&g_history);
    const uint8_t *in = buf;
    for (UInt32 k = 0; hist && k < frames; ++k) {
        /* Publication stops at the next frame once a stop or a restart has begun. */
        if (!cp_gate_still(&g_xfer, gen)) break;
        uint32_t l, r;
        memcpy(&l, in + (size_t)k * 8, sizeof(l));
        memcpy(&r, in + (size_t)k * 8 + 4, sizeof(r));
        (void)cp_hist_write(hist, gen, t + k, l, r); /* a dropped frame is simply not published */
    }
    cp_gate_leave(&g_xfer);
}

static void cp_read_speakers(uint64_t t, UInt32 frames, void *buf) {
    uint8_t *out = buf;
    uint32_t gen;
    if (!cp_gate_enter(&g_xfer, &gen)) { /* no active transfer: silence */
        memset(out, 0, (size_t)frames * 8);
        return;
    }
    CP_TEST_HOOK(CP_HOOK_IO_ADMITTED, CROSSPANE_DEV_SPEAKERS_LOOPBACK);
    const cp_hist_slot *hist = atomic_load(&g_history);
    /* The agent reads the input sample time minus the fixed transfer delay. */
    const int64_t base = (int64_t)t - CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES;
    bool live = true; /* this transfer is still the current one */
    for (UInt32 k = 0; k < frames; ++k) {
        uint32_t l = 0, r = 0;
        const int64_t tk = base + (int64_t)k;
        /* A reader admitted under a transfer that has since stopped (or restarted) must not hand
         * out that transfer's PCM: from the moment it retires, the rest of the block is silence. */
        if (live && !cp_gate_still(&g_xfer, gen)) live = false;
        if (!live || !hist || tk < 0 || !cp_hist_read(hist, gen, (uint64_t)tk, &l, &r)) {
            l = 0; /* missing, expired, unpublished or racing: the whole frame is silence */
            r = 0;
        }
        memcpy(out + (size_t)k * 8, &l, sizeof(l));
        memcpy(out + (size_t)k * 8 + 4, &r, sizeof(r));
    }
    cp_gate_leave(&g_xfer);
}

static OSStatus cp_do_io(AudioServerPlugInDriverRef d, AudioObjectID obj, AudioObjectID stream,
                         UInt32 client, UInt32 op, UInt32 frames,
                         const AudioServerPlugInIOCycleInfo *info, void *mainbuf, void *secondary) {
    (void)client;
    (void)secondary;
    if (!cp_is_driver(d)) return kAudioHardwareBadObjectError;
    const cp_dev *dv = cp_dev_by_id(obj);
    if (!dv) return kAudioHardwareBadObjectError;
    if (stream != dv->stream) return kAudioHardwareBadStreamError;
    if (op != cp_device_operation(dv)) return kAudioHardwareUnsupportedOperationError;
    if (frames == 0) return noErr; /* a no-op: nothing is read, written or validated further */
    if (frames > CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES)
        return kAudioHardwareIllegalOperationError; /* explicit failure: never a partial success */
    if (!info || !mainbuf) return kAudioHardwareIllegalOperationError;
    uint64_t t;
    if (!cp_sample_time(dv->input ? &info->mInputTime : &info->mOutputTime, &t)) {
        if (dv->input) /* the buffer is ours to fill: never hand the HAL stale bytes */
            memset(mainbuf, 0, (size_t)frames * dv->channels * sizeof(float));
        return kAudioHardwareIllegalOperationError;
    }
    switch (dv->device) {
    case CROSSPANE_DEV_SPEAKERS_APP:
        cp_write_speakers(t, frames, mainbuf);
        break;
    case CROSSPANE_DEV_SPEAKERS_LOOPBACK:
        cp_read_speakers(t, frames, mainbuf);
        break;
    case CROSSPANE_DEV_MIC_APP: /* the microphone is always silence in this slice */
        memset(mainbuf, 0, (size_t)frames * dv->channels * sizeof(float));
        break;
    case CROSSPANE_DEV_MIC_LOOPBACK: /* ...and its hidden output is always discarded */
    default:
        break;
    }
    return noErr;
}

/* ---- Driver interface ------------------------------------------------------------------------ */
static AudioServerPlugInDriverInterface g_interface = {
    ._reserved = NULL,
    .QueryInterface = cp_query_interface,
    .AddRef = cp_add_ref,
    .Release = cp_release,
    .Initialize = cp_initialize,
    .CreateDevice = cp_create_device,
    .DestroyDevice = cp_destroy_device,
    .AddDeviceClient = cp_add_client,
    .RemoveDeviceClient = cp_remove_client,
    .PerformDeviceConfigurationChange = cp_config_change,
    .AbortDeviceConfigurationChange = cp_config_change,
    .HasProperty = cp_has_property,
    .IsPropertySettable = cp_is_settable,
    .GetPropertyDataSize = cp_get_size,
    .GetPropertyData = cp_get_data,
    .SetPropertyData = cp_set_data,
    .StartIO = cp_start_io,
    .StopIO = cp_stop_io,
    .GetZeroTimeStamp = cp_zero_time,
    .WillDoIOOperation = cp_will_do,
    .BeginIOOperation = cp_begin_io,
    .DoIOOperation = cp_do_io,
    .EndIOOperation = cp_end_io,
};
