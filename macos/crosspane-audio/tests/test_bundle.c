/* Direct test 6 (bundle part): the PRODUCTION CrosspaneAudio.driver is loaded and unloaded through
 * CFPlugIn exactly as the host does, with a fake host. No HAL registration, no installed driver, no
 * audio I/O, no CoreAudio client API: only CoreFoundation and the driver's own vtable.
 *
 * usage: test_bundle path/to/CrosspaneAudio.driver
 */
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <malloc/malloc.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "../src/crosspane_audio.h"

#define CHECK(cond, what)                                                                    \
    do {                                                                                     \
        if (!(cond)) {                                                                       \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, (what));                 \
            _exit(1);                                                                        \
        }                                                                                    \
    } while (0)
#define CHECK_EQ(a, b, what)                                                                 \
    do {                                                                                     \
        long long a_ = (long long)(a), b_ = (long long)(b);                                  \
        if (a_ != b_) {                                                                      \
            fprintf(stderr, "FAIL %s:%d: %s (got %lld, want %lld)\n", __FILE__, __LINE__,    \
                    (what), a_, b_);                                                         \
            _exit(1);                                                                        \
        }                                                                                    \
    } while (0)

static _Atomic unsigned notes_total;
static _Atomic unsigned notes_obj[16];
static OSStatus changed(AudioServerPlugInHostRef host, AudioObjectID obj, UInt32 n,
                        const AudioObjectPropertyAddress *a);
/* The driver keeps the host pointer for the life of the process (the HAL guarantees the host
 * interface exists as long as the plug-in is loaded), so one host serves every cycle. */
static AudioServerPlugInHostInterface the_host = {.PropertiesChanged = changed};
static AudioServerPlugInHostInterface other_host = {.PropertiesChanged = changed};
static AudioServerPlugInDriverRef g_singleton; /* the one instance every Factory call returns */
static OSStatus changed(AudioServerPlugInHostRef host, AudioObjectID obj, UInt32 n,
                        const AudioObjectPropertyAddress *a) {
    CHECK(host == &the_host, "notifications go to the host that was passed to the first Initialize");
    CHECK(n == 1 && a && a[0].mSelector == kAudioDevicePropertyDeviceIsRunning, "well-formed notification");
    CHECK(obj < 16, "a notification names one of the driver's objects");
    atomic_fetch_add(&notes_obj[obj], 1);
    atomic_fetch_add(&notes_total, 1);
    return noErr;
}

static unsigned thread_count(void) {
    thread_act_array_t list;
    mach_msg_type_number_t count;
    CHECK(task_threads(mach_task_self(), &list, &count) == KERN_SUCCESS, "task threads");
    for (unsigned i = 0; i < count; ++i) mach_port_deallocate(mach_task_self(), list[i]);
    vm_deallocate(mach_task_self(), (vm_address_t)list, count * sizeof(*list));
    return count;
}
static bool image_loaded(const char *path) {
    for (uint32_t i = 0; i < _dyld_image_count(); ++i)
        if (strcmp(_dyld_get_image_name(i), path) == 0) return true;
    return false;
}
static size_t heap_blocks(void) {
    malloc_statistics_t st;
    malloc_zone_statistics(NULL, &st);
    return st.blocks_in_use;
}

static OSStatus prop(AudioServerPlugInDriverRef d, AudioObjectID o, UInt32 sel, UInt32 scope, UInt32 qs,
                     const void *q, UInt32 cap, UInt32 *n, void *out) {
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    return (*d)->GetPropertyData(d, o, 0, &a, qs, q, cap, n, out);
}

/* A realistic exercise of the whole driver surface through the real bundle's vtable. */
static void exercise(AudioServerPlugInDriverRef d) {
    const unsigned total0 = atomic_load(&notes_total), obj2_0 = atomic_load(&notes_obj[2]), obj3_0 = atomic_load(&notes_obj[3]);
    UInt32 n = 0;
    AudioObjectID ids[4];
    CHECK(prop(d, 1, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ids), &n, ids) == noErr && n == 16, "device list");
    const char *uids[4] = {CROSSPANE_SPEAKERS_APP_UID, CROSSPANE_SPEAKERS_LOOPBACK_UID, CROSSPANE_MIC_APP_UID, CROSSPANE_MIC_LOOPBACK_UID};
    for (unsigned i = 0; i < 4; ++i) {
        CFStringRef uid = CFStringCreateWithCString(NULL, uids[i], kCFStringEncodingUTF8);
        AudioObjectID out = 0;
        CHECK(prop(d, 1, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, sizeof(uid), &uid, sizeof(out), &n, &out) == noErr && out == ids[i], "hidden and visible UIDs translate");
        CFRelease(uid);
    }
    /* Visible demand through the real bundle. */
    AudioServerPlugInClientInfo ci = {.mClientID = 11, .mProcessID = 4242, .mIsNativeEndian = true};
    CHECK((*d)->AddDeviceClient(d, 2, &ci) == noErr, "add");
    CHECK((*d)->StartIO(d, 2, 11) == noErr, "start visible");
    UInt32 running = 0;
    CHECK(prop(d, 2, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(running), &n, &running) == noErr && running == 1, "visible running");
    AudioServerPlugInClientInfo agent = {.mClientID = 12, .mProcessID = 4243, .mIsNativeEndian = true};
    CHECK((*d)->AddDeviceClient(d, 3, &agent) == noErr && (*d)->StartIO(d, 3, 12) == noErr, "hidden start");
    CHECK(prop(d, 2, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(running), &n, &running) == noErr && running == 1, "still running");
    /* Real clock: zero time stamps agree across devices, whichever the device. */
    Float64 s0 = 0;
    UInt64 h0 = 0, seed0 = 0;
    CHECK((*d)->GetZeroTimeStamp(d, 2, 11, &s0, &h0, &seed0) == noErr && seed0 == 1, "zero time");
    for (AudioObjectID dev = 3; dev <= 5; ++dev) {
        Float64 s;
        UInt64 h, seed;
        CHECK((*d)->GetZeroTimeStamp(d, dev, 1, &s, &h, &seed) == noErr && seed == seed0, "common seed");
        CHECK(s >= s0 && ((uint64_t)s % 16384) == 0, "period-aligned");
    }
    /* One audio round trip: write at the output time, read at input time + 1024. */
    float out_buf[480 * 2], in_buf[480 * 2];
    for (unsigned k = 0; k < 480; ++k) { out_buf[2 * k] = (float)k + 0.25f; out_buf[2 * k + 1] = -(float)k - 0.5f; }
    /* Valid cycle info, as the HAL supplies it: the operation time stamps say their sample time is valid. */
    const UInt32 valid = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid;
    AudioServerPlugInIOCycleInfo c;
    memset(&c, 0, sizeof(c));
    c.mIOCycleCounter = 7;
    c.mNominalIOBufferFrameSize = 480;
    c.mCurrentTime = (AudioTimeStamp){.mSampleTime = 100000, .mHostTime = 5000, .mFlags = valid};
    c.mOutputTime = (AudioTimeStamp){.mSampleTime = 100000, .mHostTime = 5000, .mFlags = valid};
    c.mInputTime = (AudioTimeStamp){.mSampleTime = 100000 + 1024, .mHostTime = 5000, .mFlags = valid};
    c.mMainHostTicksPerFrame = 500.0;
    c.mDeviceHostTicksPerFrame = 500.0;
    /* Negative cases through the production bundle: a missing or host-time-only SampleTimeValid
     * flag is malformed for Begin, Do and End alike, and refused Do calls neither publish nor
     * leave stale bytes. */
    const UInt32 bad_flags[] = {0, kAudioTimeStampHostTimeValid, kAudioTimeStampRateScalarValid};
    for (unsigned f = 0; f < 3; ++f) {
        AudioServerPlugInIOCycleInfo bad = c;
        bad.mOutputTime.mFlags = bad_flags[f];
        bad.mInputTime.mFlags = bad_flags[f];
        float junk[480 * 2];
        memset(junk, 0x7f, sizeof(junk));
        CHECK((*d)->DoIOOperation(d, 2, 6, 11, kAudioServerPlugInIOOperationWriteMix, 480, &bad, out_buf, NULL) == kAudioHardwareIllegalOperationError, "write without a valid sample time");
        CHECK((*d)->DoIOOperation(d, 3, 7, 12, kAudioServerPlugInIOOperationReadInput, 480, &bad, junk, NULL) == kAudioHardwareIllegalOperationError, "read without a valid sample time");
        for (unsigned k = 0; k < 480 * 2; ++k) CHECK(junk[k] == 0.0f, "a refused read returns silence, not stale bytes");
        CHECK((*d)->BeginIOOperation(d, 2, 11, kAudioServerPlugInIOOperationWriteMix, 480, &bad) == kAudioHardwareIllegalOperationError, "Begin without a valid sample time");
        CHECK((*d)->EndIOOperation(d, 3, 12, kAudioServerPlugInIOOperationReadInput, 480, &bad) == kAudioHardwareIllegalOperationError, "End without a valid sample time");
    }
    CHECK((*d)->BeginIOOperation(d, 2, 11, kAudioServerPlugInIOOperationWriteMix, 480, &c) == noErr, "Begin with valid info");
    CHECK((*d)->DoIOOperation(d, 2, 6, 11, kAudioServerPlugInIOOperationWriteMix, 480, &c, out_buf, NULL) == noErr, "write");
    CHECK((*d)->EndIOOperation(d, 2, 11, kAudioServerPlugInIOOperationWriteMix, 480, &c) == noErr, "End with valid info");
    memset(in_buf, 0x7f, sizeof(in_buf));
    CHECK((*d)->DoIOOperation(d, 3, 7, 12, kAudioServerPlugInIOOperationReadInput, 480, &c, in_buf, NULL) == noErr, "read");
    CHECK(memcmp(out_buf, in_buf, sizeof(in_buf)) == 0, "audio round trip through the production driver");
    CHECK((*d)->StopIO(d, 3, 12) == noErr && (*d)->StopIO(d, 2, 11) == noErr, "stops");
    CHECK((*d)->RemoveDeviceClient(d, 3, &agent) == noErr && (*d)->RemoveDeviceClient(d, 2, &ci) == noErr, "removes");
    /* Exactly the running edges this exercise caused, per object: visible start/stop and hidden
     * start/stop are two edges each; nothing missing, nothing fabricated. */
    CHECK_EQ(atomic_load(&notes_obj[2]) - obj2_0, 2, "two running edges for the visible speakers");
    CHECK_EQ(atomic_load(&notes_obj[3]) - obj3_0, 2, "two running edges for the hidden speakers");
    CHECK_EQ(atomic_load(&notes_total) - total0, 4, "exactly four running notifications per cycle");
}

static void cycle(const char *bundle_path, unsigned mode) {
    char image[PATH_MAX];
    snprintf(image, sizeof(image), "%s/Contents/MacOS/CrosspaneAudio", bundle_path);
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)bundle_path, (CFIndex)strlen(bundle_path), true);
    CHECK(url != NULL, "bundle URL");
    CFPlugInRef plugin = CFPlugInCreate(NULL, url);
    CFRelease(url);
    CHECK(plugin != NULL, "CFPlugInCreate parses the plist");
    CFBundleRef bundle = CFPlugInGetBundle(plugin);
    CHECK(CFBundleGetIdentifier(bundle) != NULL && CFStringCompare(CFBundleGetIdentifier(bundle), CFSTR(CROSSPANE_AUDIO_BUNDLE_ID), 0) == kCFCompareEqualTo, "bundle identifier");
    CHECK(CFBundleLoadExecutable(bundle), "load the actual executable");
    CHECK(image_loaded(image), "dyld image loaded");
    if (mode == 2) { /* a rejected load, with no instance ever created, must leave nothing pinned */
        CFUUIDRef f = CFUUIDCreateFromString(NULL, CFSTR(CROSSPANE_AUDIO_FACTORY_UUID));
        CHECK(CFPlugInInstanceCreate(NULL, f, IUnknownUUID) == NULL, "rejected factory load");
        CFRelease(f);
        CFBundleUnloadExecutable(bundle);
        CHECK(!image_loaded(image), "a rejected load unloads cleanly (nothing pinned)");
        CFRelease(plugin);
        return;
    }
    CHECK(CFBundleGetFunctionPointerForName(bundle, CFSTR("CrosspaneAudioFactory")) != NULL, "factory exported");
    CHECK(CFBundleGetFunctionPointerForName(bundle, CFSTR("CrosspaneAudioUnload")) != NULL, "unload hook exported");
    CHECK(CFBundleGetFunctionPointerForName(bundle, CFSTR("g_test_hook")) == NULL && dlsym(RTLD_DEFAULT, "g_test_hook") == NULL, "no test hooks in the production image");

    CFUUIDRef factory = CFUUIDCreateFromString(NULL, CFSTR(CROSSPANE_AUDIO_FACTORY_UUID));
    CFArrayRef factories = CFPlugInFindFactoriesForPlugInTypeInPlugIn(kAudioServerPlugInTypeUUID, plugin);
    CHECK(factories != NULL && CFArrayGetCount(factories) == 1 && CFEqual(CFArrayGetValueAtIndex(factories, 0), factory), "the plist maps the plug-in type to the stable factory UUID");
    CFRelease(factories);

    const unsigned baseline = thread_count();
    /* A foreign type is refused by the factory (rejected load). */
    CHECK(CFPlugInInstanceCreate(NULL, factory, IUnknownUUID) == NULL, "rejected factory load");
    CHECK(thread_count() == baseline, "no thread after a rejected load");

    AudioServerPlugInDriverRef d = CFPlugInInstanceCreate(NULL, factory, kAudioServerPlugInTypeUUID);
    CHECK(d != NULL, "factory instance");
    if (!g_singleton) g_singleton = d;
    CHECK(d == g_singleton, "every Factory call in this process returns the same singleton");
    CHECK(thread_count() == baseline, "the driver creates no thread");
    if (mode == 0) { /* released straight away, without using the reference to Initialize */
        CHECK((*d)->Release(d) == 0, "release of an unused reference");
    } else {
        /* The first Initialize in the process creates the state; every later one, with the same
         * host, is a successful no-op; another host is refused. */
        CHECK((*d)->Initialize(d, &the_host) == noErr, "Initialize");
        CHECK((*d)->Initialize(d, &the_host) == noErr, "Initialize again with the same host");
        CHECK((*d)->Initialize(d, &other_host) == kAudioHardwareIllegalOperationError, "a different host is refused");
        CHECK(thread_count() == baseline, "Initialize creates no thread");
        exercise(d);
        /* The exported unload hook is an idempotent no-op: the instance serves exactly as before. */
        void (*unload)(CFPlugInRef) = (void (*)(CFPlugInRef))CFBundleGetFunctionPointerForName(bundle, CFSTR("CrosspaneAudioUnload"));
        CHECK(unload != NULL, "unload hook");
        unload(plugin);
        unload(plugin);
        unload(NULL);
        exercise(d);
        /* A host that drops its CFPlugIn reference and then explicitly unloads the bundle while an
         * instance is alive (observed to unmap the executable without the dyld pin, so that the
         * final Release faulted) must not unmap the code: CFBundle forgets the executable, but
         * dyld keeps it and the instance keeps working. */
        CFRetain(bundle);
        CFRelease(plugin);
        plugin = NULL;
        CFBundleUnloadExecutable(bundle);
        CHECK(image_loaded(image), "the executable stays mapped while an instance exists");
        {
            UInt32 n = 0;
            AudioObjectID ids[4];
            CHECK(prop(d, 1, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ids), &n, ids) == noErr && n == 16, "the instance still works after an explicit unload");
        }
        CHECK((*d)->AddRef(d) == 2 && (*d)->Release(d) == 1, "non-final reference");
        CHECK((*d)->Release(d) == 0, "Release to zero");
        CHECK((*d)->Release(d) == 0, "the count never goes below zero");
        /* Release to zero tears nothing down: the singleton keeps serving, data and state intact. */
        exercise(d);
    }
    CFRelease(factory);
    CHECK(thread_count() == baseline, "no thread after the cycle");
    CFBundleUnloadExecutable(bundle);
    CHECK(!CFBundleIsExecutableLoaded(bundle), "CFBundle considers the executable unloaded");
    /* By design the image is never unmapped once an instance has existed (see cp_pin_image): a
     * callback can never return into freed code. A rejected load before any instance is the only
     * path that leaves nothing pinned. */
    CHECK(image_loaded(image), "the executable stays pinned in the host process after unload");
    CHECK(thread_count() == baseline, "no surviving thread after unload");
    if (plugin) CFRelease(plugin);
    else CFRelease(bundle); /* the reference taken when the CFPlugIn reference was dropped */
}

/* CFBundle/dyld keep a little bookkeeping per load/unload that is not the driver's: measure that
 * control (load + unload with no driver call at all) and require the full cycles, which run the
 * driver's factory, Initialize, I/O and Release, to add nothing on top of it (the singleton's
 * state is allocated once, by the first cycle, and reused). */
static void load_unload_only(const char *bundle_path) {
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)bundle_path, (CFIndex)strlen(bundle_path), true);
    CHECK(url != NULL, "bundle URL");
    CFPlugInRef plugin = CFPlugInCreate(NULL, url);
    CFRelease(url);
    CHECK(plugin != NULL, "CFPlugInCreate");
    CFBundleRef bundle = CFPlugInGetBundle(plugin);
    CHECK(CFBundleLoadExecutable(bundle), "load");
    CFBundleUnloadExecutable(bundle);
    CFRelease(plugin);
}

int main(int argc, char **argv) {
    CHECK(argc == 2, "usage: test_bundle CrosspaneAudio.driver");
    char path[PATH_MAX];
    CHECK(realpath(argv[1], path) != NULL, "bundle path");
    const unsigned N = 40;
    cycle(path, 2); /* first, while nothing is pinned yet: a rejected load unloads cleanly */
    cycle(path, 2);
    for (unsigned warm = 0; warm < 3; ++warm) { cycle(path, 0); cycle(path, 1); load_unload_only(path); }
    size_t b0 = heap_blocks();
    for (unsigned i = 0; i < N; ++i) load_unload_only(path);
    size_t b1 = heap_blocks();
    for (unsigned i = 0; i < N / 2; ++i) { cycle(path, 0); cycle(path, 1); }
    size_t b2 = heap_blocks();
    const long control = (long)b1 - (long)b0, full = (long)b2 - (long)b1;
    fprintf(stderr, "  heap blocks: load/unload control %+ld over %u cycles; driver cycles %+ld over %u cycles\n", control, N, full, N);
#if defined(__has_feature)
#if __has_feature(address_sanitizer)
    fprintf(stderr, "  (ASan: heap block comparison skipped; its quarantine makes the counts meaningless)\n");
#else
    CHECK(full <= control + 8, "the driver's own lifecycle adds no per-cycle heap growth beyond the CFBundle/dyld control");
#endif
#else
    CHECK(full <= control + 8, "the driver's own lifecycle adds no per-cycle heap growth beyond the CFBundle/dyld control");
#endif
    puts("PASS test_bundle: production bundle loads through CFPlugIn, serves the vtable as a process-lifetime singleton "
         "(same instance every cycle, Release to zero and the unload hook tear nothing down), stays pinned after unload "
         "(no thread, no driver heap growth)");
    return 0;
}
