/* Direct test 6: the process-lifetime singleton (Release to zero tears nothing down; Factory hands
 * out the same live instance with its data intact; Initialize host rules; the unload hook is a
 * no-op), concurrent client Start/Stop/remove, exact running-edge notifications, the generation
 * oracle, calls and host notifications in flight across a Release to zero, no use-after-free, no
 * retained HAL pointer, no detached thread, no leak. (Run it also under the --asan and --tsan
 * builds.) No HAL registration. */
#include "cp_fixture.h"

#include <malloc/malloc.h>
#include <mach/mach.h>

#define RING() atomic_load(&g_history)

static unsigned thread_count(void) {
    thread_act_array_t list;
    mach_msg_type_number_t count;
    CHECK(task_threads(mach_task_self(), &list, &count) == KERN_SUCCESS, "task_threads");
    for (unsigned i = 0; i < count; ++i) mach_port_deallocate(mach_task_self(), list[i]);
    vm_deallocate(mach_task_self(), (vm_address_t)list, count * sizeof(*list));
    return count;
}

static CFUUIDBytes driver_uuid_bytes(void) { return CFUUIDGetUUIDBytes(kAudioServerPlugInDriverInterfaceUUID); }

/* ---- Factory, reference counting, IUnknown -------------------------------------------------- */
static void test_factory_refcount(void) {
    fake_host *h = fake_host_new();
    CHECK(g_refs == 0 && !g_initialized, "pristine");
    /* Nothing works before Initialize or without an instance. */
    CHECK((*g_driver)->Initialize(g_driver, &h->iface) == kAudioHardwareUnspecifiedError, "no Initialize without a reference");
    UInt32 n;
    CHECK(gsize(2, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareNotRunningError, "properties before Initialize");

    AudioServerPlugInDriverRef a = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    AudioServerPlugInDriverRef b = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(a == g_driver && b == g_driver, "one singleton instance");
    CHECK_EQ(g_refs, 2, "two references");
    CHECK(g_pinned && g_image_pinned, "the executable is pinned once an instance exists");
    CHECK(gsize(2, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == kAudioHardwareNotRunningError, "properties before Initialize (instance exists)");
    CHECK((*a)->Initialize(a, &h->iface) == noErr, "Initialize");
    CHECK(gsize(2, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr, "properties after Initialize");

    /* IUnknown. */
    void *out = NULL;
    CFUUIDBytes unknown_uuid = CFUUIDGetUUIDBytes(IUnknownUUID);
    CFUUIDBytes foreign = CFUUIDGetUUIDBytes(kAudioServerPlugInTypeUUID);
    CHECK((*a)->QueryInterface(a, driver_uuid_bytes(), &out) == S_OK && out == g_driver, "QueryInterface(driver)");
    CHECK_EQ(g_refs, 3, "QueryInterface adds a reference");
    CHECK((*a)->QueryInterface(a, unknown_uuid, &out) == S_OK && out == g_driver, "QueryInterface(IUnknown)");
    CHECK_EQ(g_refs, 4, "...again");
    out = (void *)1;
    CHECK((*a)->QueryInterface(a, foreign, &out) == E_NOINTERFACE && out == NULL, "unknown interface");
    CHECK_EQ(g_refs, 4, "a refused interface takes no reference");
    CHECK((*a)->QueryInterface(a, driver_uuid_bytes(), NULL) == E_POINTER, "NULL out");
    CHECK((*a)->AddRef(a) == 5, "AddRef");
    CHECK((*a)->Release(a) == 4 && (*a)->Release(a) == 3 && (*a)->Release(a) == 2 && (*a)->Release(a) == 1, "non-final releases");
    CHECK(g_initialized && g_pinned, "the instance survives non-final releases");
    CHECK(write_pattern(1000, 8) == noErr, "and still serves callbacks");

    /* Release to zero decrements and nothing else. */
    cp_hist_slot *ring = RING();
    CHECK((*a)->Release(a) == 0, "Release to zero returns 0");
    CHECK(g_refs == 0 && g_initialized && g_pinned && g_image_pinned, "nothing was torn down or unpinned");
    CHECK(RING() == ring && atomic_load(&g_host) == &h->iface, "ring and host are still there");
    CHECK(gsize(2, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, 0, NULL, &n) == noErr, "the control plane still answers");
    CHECK(write_pattern(1100, 8) == noErr, "the data plane still answers");

    /* Over-release never goes below zero. */
    CHECK((*a)->Release(a) == 0 && (*a)->Release(a) == 0 && (*a)->Release(a) == 0, "no underflow");
    CHECK_EQ(g_refs, 0, "still zero");
    /* A reference taken after zero is simply the next reference of the same singleton. */
    CHECK((*a)->AddRef(a) == 1, "AddRef after zero");
    out = NULL;
    CHECK((*a)->QueryInterface(a, driver_uuid_bytes(), &out) == S_OK && out == g_driver && g_refs == 2, "QueryInterface after zero");
    CHECK((*a)->Release(a) == 1 && (*a)->Release(a) == 0, "balanced");

    /* A foreign object is not the driver: nothing changes. */
    AudioServerPlugInDriverRef foreign_obj = (AudioServerPlugInDriverRef)(uintptr_t)0x10;
    CHECK((*a)->AddRef(foreign_obj) == 0 && (*a)->Release(foreign_obj) == 0, "foreign AddRef/Release");
    out = (void *)1;
    CHECK((*a)->QueryInterface(foreign_obj, driver_uuid_bytes(), &out) == E_NOINTERFACE && out == NULL, "foreign QueryInterface");
    CHECK_EQ(g_refs, 0, "no reference changed");

    /* The reference count saturates instead of wrapping; a saturated count refuses the factory. */
    g_refs = UINT32_MAX - 1;
    CHECK((*a)->AddRef(a) == UINT32_MAX && (*a)->AddRef(a) == UINT32_MAX, "AddRef saturates");
    CHECK(CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID) == NULL, "the factory refuses a saturated count");
    CHECK_EQ(g_refs, UINT32_MAX, "...and takes no reference");
    CHECK((*a)->Release(a) == UINT32_MAX - 1, "Release leaves saturation");
    g_refs = 0;

    /* The factory rejects every other type and takes no reference. */
    CHECK(CrosspaneAudioFactory(NULL, NULL) == NULL, "NULL type");
    CHECK(CrosspaneAudioFactory(NULL, kAudioServerPlugInDriverInterfaceUUID) == NULL, "foreign type");
    CHECK_EQ(g_refs, 0, "no reference for a rejected type");

    /* Control calls keep working with zero references. */
    CHECK(add_client(3, 1) == noErr && start_io(3, 1) == noErr && running(3), "control plane at zero references");
    CHECK(stop_io(3, 1) == noErr && remove_client(3, 1) == noErr, "...");
    cp_test_reset();
    free(h);
}

/* ---- Release to zero then Factory: the very same live singleton, data intact ------------------ */
static void test_release_to_zero_keeps_singleton(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef a = cp_open(h);
    cp_hist_slot *ring = RING();
    CHECK(ring != NULL && atomic_load(&g_test_live_rings) == 1, "one ring");
    begin(3, 90); /* the speaker transfer is open */
    begin(2, 91);
    CHECK(write_pattern(8000, 480) == noErr, "publish a window");
    const uint32_t gen = cp_gate_tag_of(atomic_load(&g_xfer.word));
    CHECK(!cp_gate_is_closed(&g_xfer) && gen != 0, "transfer open");
    CHECK(running(2) && running(3), "both devices running");
    CHECK_EQ(notes(h, 2), 1, "one start edge on the app speakers");
    CHECK_EQ(notes(h, 3), 1, "one start edge on the hidden speakers");

    CHECK((*a)->Release(a) == 0, "Release to zero");
    CHECK_EQ(g_refs, 0, "no references");
    CHECK(g_ready && g_initialized && g_pinned && g_image_pinned, "no teardown: still initialized and pinned");
    CHECK(RING() == ring, "the ring was not freed or replaced");
    CHECK_EQ(atomic_load(&g_test_live_rings), 1, "...and is the only ring");
    CHECK(atomic_load(&g_host) == &h->iface, "the host pointer is unchanged");
    CHECK(!cp_gate_is_closed(&g_xfer) && cp_gate_tag_of(atomic_load(&g_xfer.word)) == gen, "the open transfer is untouched");
    CHECK(running(2) && running(3), "still running");
    CHECK(g_clients[0].registered == 1 && g_clients[0].started == 1, "app client still started");
    CHECK(g_clients[1].registered == 1 && g_clients[1].started == 1, "hidden client still started");
    /* The data plane keeps working at zero references: write and read back through the live transfer. */
    CHECK(write_pattern(8480, 480) == noErr, "write at zero references");
    float buf[960];
    CHECK(read_speakers(8000 + 1024, 480, buf) == noErr, "read at zero references");
    for (uint32_t k = 0; k < 480; ++k) CHECK(fbits(buf[2 * k]) == fbits(fl(8000 + k)) && fbits(buf[2 * k + 1]) == fbits(fr(8000 + k)), "the frames published before the Release are intact");

    /* A new Factory call returns the same instance, with the same data. */
    AudioServerPlugInDriverRef b = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(b == a && b == g_driver, "the very same singleton");
    CHECK_EQ(g_refs, 1, "one new reference");
    CHECK(RING() == ring && atomic_load(&g_test_live_rings) == 1, "same ring, nothing re-created");
    CHECK((*b)->Initialize(b, &h->iface) == noErr, "the host's repeated Initialize is a no-op");
    CHECK(RING() == ring && atomic_load(&g_test_live_rings) == 1, "...which re-created nothing");
    CHECK(running(2) && running(3), "still running after the new Factory call");
    CHECK(cp_gate_tag_of(atomic_load(&g_xfer.word)) == gen, "same transfer generation");
    CHECK_EQ(notes(h, 2), 1, "no spurious notification");
    CHECK_EQ(notes(h, 3), 1, "...");
    CHECK(read_speakers(8480 + 1024, 480, buf) == noErr, "read the window written at zero references");
    for (uint32_t k = 0; k < 480; ++k) CHECK(fbits(buf[2 * k]) == fbits(fl(8480 + k)) && fbits(buf[2 * k + 1]) == fbits(fr(8480 + k)), "intact");
    CHECK(stop_io(2, 91) == noErr && stop_io(3, 90) == noErr, "stop the clients registered before the Release");
    CHECK(remove_client(2, 91) == noErr && remove_client(3, 90) == noErr, "remove them");
    CHECK_EQ(notes(h, 2), 2, "the stop edge is delivered to the same host");
    CHECK_EQ(notes(h, 3), 2, "...");
    CHECK(cp_gate_is_closed(&g_xfer), "transfer closed");
    CHECK((*b)->Release(b) == 0, "balanced");
    cp_test_reset();
    CHECK_EQ(atomic_load(&g_test_live_rings), 0, "the test-only reset frees the ring");
    free(h);
}

/* ---- Initialize: one host, for the life of the process --------------------------------------- */
static void test_initialize_host_rules(void) {
    fake_host *h = fake_host_new(), *other = fake_host_new();
    CHECK(g_refs == 0 && !g_initialized, "pristine");
    CHECK((*g_driver)->Initialize(g_driver, &h->iface) == kAudioHardwareUnspecifiedError, "Initialize before Factory");
    CHECK(!g_initialized && RING() == NULL && atomic_load(&g_host) == NULL, "...changed nothing");

    AudioServerPlugInDriverRef d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver, "factory");
    AudioServerPlugInHostInterface no_callback = {0};
    CHECK((*d)->Initialize(d, NULL) == kAudioHardwareIllegalOperationError, "NULL host");
    CHECK((*d)->Initialize(d, &no_callback) == kAudioHardwareIllegalOperationError, "host without PropertiesChanged");
    CHECK((*d)->Initialize((AudioServerPlugInDriverRef)(uintptr_t)0x10, &h->iface) == kAudioHardwareBadObjectError, "not the driver");
    CHECK(!g_initialized && RING() == NULL && atomic_load(&g_host) == NULL, "refused Initializes changed nothing");

    /* A Release to zero before the first Initialize leaves nothing to initialize with. */
    CHECK((*d)->Release(d) == 0, "release before Initialize");
    CHECK((*d)->Initialize(d, &h->iface) == kAudioHardwareUnspecifiedError, "no reference: refused");
    CHECK(!g_initialized && RING() == NULL, "...and nothing was allocated");
    d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver && g_refs == 1, "a new reference");

    CHECK((*d)->Initialize(d, &h->iface) == noErr, "first Initialize");
    cp_hist_slot *ring = RING();
    CHECK(ring != NULL && atomic_load(&g_host) == &h->iface && g_initialized, "state created once");
    CHECK((*d)->Initialize(d, &h->iface) == noErr, "the same host again: a successful no-op");
    CHECK(RING() == ring && atomic_load(&g_test_live_rings) == 1, "...which re-created nothing");
    CHECK((*d)->Initialize(d, &other->iface) == kAudioHardwareIllegalOperationError, "a different host is refused");
    CHECK(atomic_load(&g_host) == &h->iface && RING() == ring, "...and changed nothing");

    /* Notifications still go to the first host, never to the refused one. */
    begin(2, 5);
    CHECK_EQ(notes(h, 2), 1, "the first host is notified");
    CHECK_EQ(notes(other, 2), 0, "the refused host never is");

    /* The same rules hold after a Release to zero and a new Factory call. */
    CHECK((*d)->Release(d) == 0, "Release to zero");
    d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver, "same singleton");
    CHECK((*d)->Initialize(d, &other->iface) == kAudioHardwareIllegalOperationError, "still refused");
    CHECK((*d)->Initialize(d, &h->iface) == noErr, "the first host is still accepted");
    CHECK(running(2) && g_clients[0].started == 1, "the client state survived all of it");
    finish(2, 5);
    CHECK_EQ(notes(h, 2), 2, "stop edge");
    CHECK_EQ(notes(other, 2), 0, "...still never the other host");
    cp_close(d);
    free(h);
    free(other);
}

/* ---- The CFPlugIn unload hook is an idempotent no-op ------------------------------------------ */
typedef struct {
    cp_hist_slot *ring;
    AudioServerPlugInHostRef host;
    UInt32 refs;
    bool initialized, ready, pinned, image_pinned;
    uint64_t gate;
    cp_clients clients[CP_NUM_DEVS];
    UInt32 running[CP_NUM_DEVS];
    uint32_t next_generation;
} snapshot_t;
static void take_snapshot(snapshot_t *s) {
    s->ring = RING();
    s->host = atomic_load(&g_host);
    s->refs = g_refs;
    s->initialized = g_initialized;
    s->ready = atomic_load(&g_ready);
    s->pinned = g_pinned;
    s->image_pinned = g_image_pinned;
    s->gate = atomic_load(&g_xfer.word);
    memcpy(s->clients, g_clients, sizeof(g_clients));
    for (unsigned i = 0; i < CP_NUM_DEVS; ++i) s->running[i] = atomic_load(&g_running[i]);
    s->next_generation = g_next_generation;
}
static void expect_same_snapshot(const snapshot_t *a, const char *what) {
    snapshot_t b;
    take_snapshot(&b);
    CHECK(a->ring == b.ring && a->host == b.host && a->refs == b.refs, what);
    CHECK(a->initialized == b.initialized && a->ready == b.ready && a->pinned == b.pinned && a->image_pinned == b.image_pinned, what);
    CHECK(a->gate == b.gate && a->next_generation == b.next_generation, what);
    CHECK(memcmp(a->clients, b.clients, sizeof(b.clients)) == 0, what);
    CHECK(memcmp(a->running, b.running, sizeof(b.running)) == 0, what);
}

static void test_unload_hook_is_noop(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    begin(3, 1);
    begin(2, 2);
    CHECK(write_pattern(6000, 480) == noErr, "write");
    snapshot_t before;
    take_snapshot(&before);
    const uint32_t notes_before = atomic_load(&h->notifications);

    CrosspaneAudioUnload(NULL);
    expect_same_snapshot(&before, "the unload hook changed nothing");
    CrosspaneAudioUnload(NULL);
    CrosspaneAudioUnload((CFPlugInRef)(uintptr_t)0x10); /* its argument is ignored */
    expect_same_snapshot(&before, "...idempotently");
    CHECK_EQ(atomic_load(&h->notifications), notes_before, "no notification");
    float buf[960];
    CHECK(read_speakers(6000 + 1024, 480, buf) == noErr, "reads still work after the hook");
    for (uint32_t k = 0; k < 480; ++k) CHECK(fbits(buf[2 * k]) == fbits(fl(6000 + k)) && fbits(buf[2 * k + 1]) == fbits(fr(6000 + k)), "data intact after the hook");
    CHECK(write_pattern(6480, 480) == noErr && running(2) && running(3), "the data and control planes still work");

    /* The hook after a Release to zero, then a new Factory call: still the same live singleton. */
    CHECK((*d)->Release(d) == 0, "Release to zero");
    snapshot_t zero;
    take_snapshot(&zero);
    CrosspaneAudioUnload(NULL);
    expect_same_snapshot(&zero, "the hook after Release changed nothing");
    AudioServerPlugInDriverRef again = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(again == g_driver && RING() == before.ring && g_ready, "the factory after the hook returns the same live singleton");
    CHECK(read_speakers(6480 + 1024, 480, buf) == noErr, "read");
    for (uint32_t k = 0; k < 480; ++k) CHECK(fbits(buf[2 * k]) == fbits(fl(6480 + k)), "data intact");
    finish(3, 1);
    finish(2, 2);
    cp_close(again);
    free(h);
}

/* ---- Host references dropped from inside notifications ----------------------------------------- */
typedef struct {
    OSStatus status;
    _Atomic int done;
    uint64_t elapsed_ns;
    AudioObjectID dev;
    UInt32 client;
} entry_job;
static void *timed_starter(void *arg) {
    entry_job *j = arg;
    const uint64_t t0 = cp_now_ns();
    j->status = start_io(j->dev, j->client);
    j->elapsed_ns = cp_now_ns() - t0;
    atomic_store(&j->done, 1);
    return NULL;
}
/* Bounded handshake: a deadlock fails the test instead of hanging it. */
static void wait_entry_done(entry_job *j, const char *what) {
    for (int i = 0; i < 10000 && !atomic_load(&j->done); ++i) sleep_ms(1);
    CHECK(atomic_load(&j->done), what);
}

static AudioServerPlugInDriverRef g_rr_d;
static ULONG g_rr_release, g_rr_addref, g_rr_release2, g_rr_release3;
static void *g_rr_factory;
static OSStatus g_rr_init;
static _Atomic int g_rr_calls;
static fake_host *g_rr_host;
static AudioObjectID g_rr_second;
static UInt32 g_rr_client;
static OSStatus g_rr_nested;
/* The host drops its last reference from INSIDE PropertiesChanged, then re-enters the plug-in:
 * a Release to zero is harmless on the notifying stack, and nothing can deadlock. */
static void release_sole_reference_in_host(fake_host *h, AudioObjectID obj) {
    (void)h;
    (void)obj;
    const int n = atomic_fetch_add(&g_rr_calls, 1);
    if (n != 0) return;
    g_rr_release = (*g_rr_d)->Release(g_rr_d);                     /* the host's only reference */
    g_rr_addref = (*g_rr_d)->AddRef(g_rr_d);                       /* 0 -> 1: the same singleton */
    g_rr_release2 = (*g_rr_d)->Release(g_rr_d);
    g_rr_factory = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    g_rr_init = (*g_rr_d)->Initialize(g_rr_d, &g_rr_host->iface); /* same host from inside its own call */
    g_rr_release3 = (*g_rr_d)->Release(g_rr_d);
    g_rr_nested = start_io(g_rr_second, g_rr_client);              /* a nested notification */
}
static void test_host_releases_last_reference_inside_notification(void) {
    fake_host *h = fake_host_new();
    h->on_notify = release_sole_reference_in_host;
    g_rr_host = h;
    AudioServerPlugInDriverRef d = cp_open(h);
    g_rr_d = d;
    CHECK(g_refs == 1, "exactly one reference, held by the 'host'");
    CHECK(add_client(2, 7) == noErr && add_client(4, 9) == noErr, "register");
    atomic_store(&g_rr_calls, 0);
    g_rr_release = g_rr_addref = g_rr_release2 = g_rr_release3 = 99;
    g_rr_factory = (void *)1;
    g_rr_init = 0x7fff;
    g_rr_nested = 0x7fff;
    g_rr_second = 4;
    g_rr_client = 9;
    cp_hist_slot *ring = RING();
    entry_job ej = {0, 0, 0, 2, 7};
    pthread_t t = spawn(timed_starter, &ej); /* a deadlock is caught by the bounded handshake below */
    wait_entry_done(&ej, "StartIO returned: nothing in the Release path waits for the notification");
    join(t);
    CHECK(ej.status == noErr, "the StartIO itself succeeded");
    CHECK_EQ(g_rr_release, 0, "the host's Release was the last one");
    CHECK_EQ(g_rr_addref, 1, "AddRef takes the next reference of the same singleton");
    CHECK_EQ(g_rr_release2, 0, "balanced");
    CHECK(g_rr_factory == g_driver, "the factory hands out the same singleton from inside the notification");
    CHECK_EQ(g_rr_init, noErr, "Initialize with the same host is a no-op, even reentrantly");
    CHECK_EQ(g_rr_release3, 0, "balanced");
    CHECK(g_rr_nested == noErr, "a nested StartIO from inside the host call works");
    CHECK_EQ(atomic_load(&g_rr_calls), 2, "two notifications were delivered");
    CHECK(notes(h, 2) == 1 && notes(h, 4) == 1, "one per device, delivered to the live host");
    CHECK(ej.elapsed_ns < 2000ull * 1000 * 1000, "no waiting: far below any bound");
    CHECK(g_ready && g_initialized && RING() == ring && atomic_load(&g_host) == &h->iface, "nothing was torn down");
    CHECK(running(2) && running(4), "both devices running");
    CHECK(stop_io(2, 7) == noErr && stop_io(4, 9) == noErr, "and they stop normally");
    CHECK(remove_client(2, 7) == noErr && remove_client(4, 9) == noErr, "...");
    h->on_notify = NULL;
    cp_close(d);
    free(h);
}

/* A Release to zero while data callbacks are parked inside the data path and a notification is
 * parked inside the host's PropertiesChanged: Release returns at once (it waits for nothing), the
 * singleton stays live, and every resumed call finishes normally on live storage. */
typedef struct {
    parker *p;
    int is_write;
    OSStatus status;
} cb_job;
static void *parked_callback(void *arg) {
    cb_job *j = arg;
    tl_parker = j->p;
    float buf[480 * 2] = {0};
    j->status = j->is_write ? write_speakers(70000, 480, buf) : read_speakers(70000 + 1024, 480, buf);
    tl_parker = NULL;
    return NULL;
}
typedef struct {
    parker *p;
    AudioObjectID dev;
    UInt32 client;
    OSStatus status;
} start_job;
static void *starter(void *arg) {
    start_job *j = arg;
    tl_parker = j->p;
    j->status = start_io(j->dev, j->client);
    tl_parker = NULL;
    return NULL;
}

/* The fake host parks the FIRST notification it receives, inside PropertiesChanged. */
static _Atomic int g_host_parked, g_host_resume, g_host_claimed;
static void park_in_host(fake_host *h, AudioObjectID obj) {
    (void)h;
    (void)obj;
    if (atomic_exchange(&g_host_claimed, 1) == 0) {
        atomic_store(&g_host_parked, 1);
        while (!atomic_load(&g_host_resume)) sleep_ms(1);
    }
}
static void reset_host_park(void) {
    atomic_store(&g_host_parked, 0);
    atomic_store(&g_host_resume, 0);
    atomic_store(&g_host_claimed, 0);
}
static void wait_host_parked(void) {
    for (int i = 0; i < 10000 && !atomic_load(&g_host_parked); ++i) sleep_ms(1);
    CHECK(atomic_load(&g_host_parked), "StartIO is inside the host's PropertiesChanged");
}

static void test_release_to_zero_with_calls_in_flight(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    begin(3, 90);
    begin(2, 91);
    CHECK(add_client(4, 7) == noErr, "register the client whose start is parked inside the host");
    cp_hist_slot *ring = RING();
    h->on_notify = park_in_host; /* only now: setup is single-threaded and done */
    reset_host_park();

    parker pw, pr;
    parker_init(&pw, CP_HOOK_IO_ADMITTED);
    parker_init(&pr, CP_HOOK_IO_ADMITTED);
    cb_job w = {&pw, 1, 0xffff}, r = {&pr, 0, 0xffff};
    start_job sj = {NULL, 4, 7, 0xffff};
    pthread_t tw = spawn(parked_callback, &w);
    parker_wait(&pw);
    pthread_t tr = spawn(parked_callback, &r);
    parker_wait(&pr);
    pthread_t ts = spawn(starter, &sj);
    wait_host_parked(); /* two admitted data calls and one host notification are in flight */

    const uint64_t t0 = cp_now_ns();
    CHECK((*d)->Release(d) == 0, "Release to zero with three calls in flight");
    CHECK(cp_now_ns() - t0 < 2000ull * 1000 * 1000, "Release waited for nothing");
    CHECK(g_ready && g_initialized && RING() == ring && atomic_load(&g_host) == &h->iface, "nothing was retired");
    CHECK_EQ(atomic_load(&g_test_live_rings), 1, "the ring is still allocated under the stuck calls");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 2, "both data accesses are still admitted");
    /* A new reference to the same live singleton is available at once and works. */
    AudioServerPlugInDriverRef again = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(again == g_driver && g_refs == 1, "the factory hands out the live singleton");
    CHECK((*again)->Initialize(again, &h->iface) == noErr, "Initialize with the same host is a no-op");
    CHECK(write_pattern(9000, 480) == noErr, "new data calls are admitted normally");
    CHECK((*again)->Release(again) == 0, "and the new reference is released again");

    parker_release(&pw);
    parker_release(&pr);
    atomic_store(&g_host_resume, 1);
    join(tw);
    join(tr);
    join(ts);
    CHECK(w.status == noErr, "the parked writer finished normally on live storage");
    CHECK(r.status == noErr, "the parked reader finished normally on live storage");
    CHECK(sj.status == noErr, "the parked StartIO completed");
    CHECK_EQ(notes(h, 4), 1, "its notification reached the live host");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "no access left admitted");
    CHECK(g_ready && RING() == ring, "still the same singleton");
    h->on_notify = NULL;
    CHECK(stop_io(4, 7) == noErr && remove_client(4, 7) == noErr, "the parked client stops normally");
    finish(3, 90);
    finish(2, 91);
    cp_test_reset();
    free(h);
}

/* ---- No retained HAL pointers ---------------------------------------------------------------- */
static void test_no_retained_hal_pointers(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    CFStringRef bundle = CFStringCreateWithCString(NULL, "com.example.client", kCFStringEncodingUTF8);
    CFIndex before = CFGetRetainCount(bundle);
    AudioServerPlugInClientInfo ci = client_info(5);
    ci.mBundleID = bundle;
    CHECK((*d)->AddDeviceClient(d, 2, &ci) == noErr, "add");
    CHECK_EQ(CFGetRetainCount(bundle), before, "the client's bundle id is not retained");
    CHECK(start_io(2, 5) == noErr, "start");
    CHECK((*d)->RemoveDeviceClient(d, 2, &ci) == noErr, "remove");
    CHECK_EQ(CFGetRetainCount(bundle), before, "...nor released");
    CFRelease(bundle);

    /* HAL buffers and cycle info are used only during the call: free them and keep calling. */
    for (int i = 0; i < 20; ++i) {
        float *buf = malloc(480 * 8);
        AudioServerPlugInIOCycleInfo *info = malloc(sizeof(*info));
        CHECK(buf && info, "alloc");
        *info = cycle_info(50000, 50000, 480);
        memset(buf, 0, 480 * 8);
        begin(3, 60 + (UInt32)i);
        CHECK((*d)->DoIOOperation(d, 2, 6, 1, kAudioServerPlugInIOOperationWriteMix, 480, info, buf, NULL) == noErr, "write");
        CHECK((*d)->DoIOOperation(d, 3, 7, 1, kAudioServerPlugInIOOperationReadInput, 480, info, buf, NULL) == noErr, "read");
        memset(buf, 0xee, 480 * 8); /* poison before freeing */
        memset(info, 0xee, sizeof(*info));
        free(buf);
        free(info);
        finish(3, 60 + (UInt32)i);
    }
    /* The host pointer is the one thing the driver keeps (the host guarantees it for the plug-in's
     * lifetime): it must still be exactly the host passed to Initialize, and nothing else of the
     * caller's was retained. */
    CHECK(atomic_load(&g_host) == &h->iface, "only the host pointer is kept");
    cp_close(d);
    free(h);
}

/* ---- Small helpers ----------------------------------------------------------------------------- */
typedef struct {
    atomic_uint count, phase;
    unsigned n;
} barrier_t;
static void barrier_init(barrier_t *b, unsigned n) {
    atomic_store(&b->count, 0);
    atomic_store(&b->phase, 0);
    b->n = n;
}
static void barrier_wait(barrier_t *b) {
    const unsigned phase = atomic_load(&b->phase);
    if (atomic_fetch_add(&b->count, 1) + 1 == b->n) {
        atomic_store(&b->count, 0);
        atomic_fetch_add(&b->phase, 1);
        return;
    }
    while (atomic_load(&b->phase) == phase) sleep_ms(0);
}

/* ---- Concurrent reference counting ------------------------------------------------------------- */
static void *ref_churn(void *arg) {
    (void)arg;
    AudioServerPlugInDriverRef d = g_driver;
    for (int i = 0; i < 20000; ++i) {
        void *out = NULL;
        CHECK((*d)->AddRef(d) >= 2, "AddRef while the test holds a reference");
        CHECK((*d)->QueryInterface(d, driver_uuid_bytes(), &out) == S_OK && out == g_driver, "QueryInterface");
        CHECK((*d)->Release(d) >= 1, "Release never reaches zero while the test holds a reference");
        CHECK((*d)->Release(d) >= 1, "...");
        AudioServerPlugInDriverRef f = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
        CHECK(f == g_driver, "factory");
        CHECK((*f)->Release(f) >= 1, "...");
    }
    return NULL;
}
static void test_concurrent_refcounts(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    begin(3, 1);
    cp_hist_slot *ring = RING();
    pthread_t th[6];
    for (int i = 0; i < 6; ++i) th[i] = spawn(ref_churn, NULL);
    for (uint64_t t = 20000; t < 20000 + 40 * 480; t += 480) CHECK(write_pattern(t, 480) == noErr, "data plane while the references churn");
    for (int i = 0; i < 6; ++i) join(th[i]);
    CHECK_EQ(g_refs, 1, "balanced: only the test's own reference remains");
    CHECK(g_ready && RING() == ring && running(3), "the singleton is untouched");
    finish(3, 1);
    cp_close(d);
    free(h);
}

/* ---- Concurrent client Start/Stop/remove ------------------------------------------------------- */
#define CHURN_THREADS_PER_DEVICE 4
#define CHURN_CLIENTS 8
typedef struct {
    AudioObjectID dev;
    UInt32 first_id;
    int iterations;
    uint64_t seed;
} churn_ctx;

static void *churn_thread(void *arg) {
    churn_ctx *c = arg;
    bool reg[CHURN_CLIENTS] = {0}, started[CHURN_CLIENTS] = {0};
    uint64_t x = c->seed;
    for (int i = 0; i < c->iterations; ++i) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        unsigned k = (unsigned)(x % CHURN_CLIENTS);
        UInt32 id = c->first_id + k;
        switch ((x >> 20) % 4) {
        case 0: /* add: always succeeds, a duplicate is a no-op */
            CHECK(add_client(c->dev, id) == noErr, "concurrent add");
            reg[k] = true;
            break;
        case 1: /* start */
            if (reg[k] && !started[k]) { CHECK(start_io(c->dev, id) == noErr, "concurrent start"); started[k] = true; }
            else CHECK(start_io(c->dev, id) == kAudioHardwareIllegalOperationError, "start of an unregistered or already started client");
            break;
        case 2: /* stop */
            if (started[k]) { CHECK(stop_io(c->dev, id) == noErr, "concurrent stop"); started[k] = false; }
            else CHECK(stop_io(c->dev, id) == kAudioHardwareIllegalOperationError, "unmatched stop");
            break;
        default: /* remove (a started client is implicitly stopped) */
            if (reg[k]) { CHECK(remove_client(c->dev, id) == noErr, "concurrent remove"); reg[k] = started[k] = false; }
            else CHECK(remove_client(c->dev, id) == kAudioHardwareIllegalOperationError, "remove of an unknown client");
            break;
        }
    }
    for (unsigned k = 0; k < CHURN_CLIENTS; ++k)
        if (reg[k]) CHECK(remove_client(c->dev, c->first_id + k) == noErr, "final remove");
    return NULL;
}

static _Atomic int churn_stop;
static void *property_poller(void *arg) {
    (void)arg;
    while (!atomic_load(&churn_stop)) {
        for (AudioObjectID dev = 2; dev <= 5; ++dev) {
            UInt32 v = 99, n = 0;
            CHECK(gp(dev, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(v), &n, &v) == noErr && v <= 1, "running property is always 0 or 1");
        }
    }
    return NULL;
}

static void test_concurrent_clients(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    enum { N = 4 * CHURN_THREADS_PER_DEVICE };
    churn_ctx ctx[N];
    pthread_t th[N];
    atomic_store(&churn_stop, 0);
    pthread_t poller = spawn(property_poller, NULL);
    for (unsigned i = 0; i < N; ++i) {
        ctx[i] = (churn_ctx){(AudioObjectID)(2 + i % 4), 100 + (UInt32)(i / 4) * CHURN_CLIENTS, 20000, 0x9e3779b97f4a7c15ull + i * 1000003u};
        th[i] = spawn(churn_thread, &ctx[i]);
    }
    for (unsigned i = 0; i < N; ++i) join(th[i]);
    atomic_store(&churn_stop, 1);
    join(poller);
    /* Quiescent: nothing registered, nothing started, nothing running, no fabricated clients. */
    for (unsigned i = 0; i < 4; ++i) {
        CHECK(g_clients[i].registered == 0 && g_clients[i].started == 0, "no clients left");
        CHECK(!running((AudioObjectID)(i + 2)), "nothing running");
        CHECK_EQ(notes(h, (AudioObjectID)(i + 2)) % 2, 0, "every start edge was matched by a stop edge notification");
    }
    CHECK(cp_gate_is_closed(&g_xfer), "no transfer left open");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "nothing admitted");
    CHECK_EQ(atomic_load(&h->malformed), 0, "well-formed notifications");
    cp_close(d);
    free(h);
}

/* Barrier-controlled batches with KNOWN running-edge counts: in every round all threads start
 * their own clients of one device together (exactly one 0->1 edge in total), the main thread
 * checks the per-round notification delta and the running state, then all stop together (exactly
 * one 1->0 edge). No edge can be missing or fabricated without a round failing. */
#define BATCH_THREADS 8
#define BATCH_CLIENTS 4
#define BATCH_ROUNDS 25
typedef struct {
    AudioObjectID dev;
    UInt32 first_id;
    barrier_t *go, *done;
    int rounds;
} batch_ctx;
static void *batch_thread(void *arg) {
    batch_ctx *c = arg;
    for (unsigned k = 0; k < BATCH_CLIENTS; ++k) CHECK(add_client(c->dev, c->first_id + k) == noErr, "batch register");
    barrier_wait(c->go); /* all registered */
    for (int r = 0; r < c->rounds; ++r) {
        barrier_wait(c->go); /* start phase */
        for (unsigned k = 0; k < BATCH_CLIENTS; ++k) CHECK(start_io(c->dev, c->first_id + k) == noErr, "batch start");
        barrier_wait(c->done);
        barrier_wait(c->go); /* stop phase */
        for (unsigned k = 0; k < BATCH_CLIENTS; ++k) CHECK(stop_io(c->dev, c->first_id + k) == noErr, "batch stop");
        barrier_wait(c->done);
    }
    for (unsigned k = 0; k < BATCH_CLIENTS; ++k) CHECK(remove_client(c->dev, c->first_id + k) == noErr, "batch remove");
    return NULL;
}
static void test_notification_batches(void) {
    for (AudioObjectID dev = 2; dev <= 5; ++dev) {
        fake_host *h = fake_host_new();
        AudioServerPlugInDriverRef d = cp_open(h);
        barrier_t go, done;
        barrier_init(&go, BATCH_THREADS + 1);
        barrier_init(&done, BATCH_THREADS + 1);
        batch_ctx ctx[BATCH_THREADS];
        pthread_t th[BATCH_THREADS];
        for (unsigned i = 0; i < BATCH_THREADS; ++i) {
            ctx[i] = (batch_ctx){dev, 500 + i * 16, &go, &done, BATCH_ROUNDS};
            th[i] = spawn(batch_thread, &ctx[i]);
        }
        barrier_wait(&go); /* registered */
        CHECK_EQ(g_clients[dev - 2].registered, BATCH_THREADS * BATCH_CLIENTS, "all clients registered");
        CHECK_EQ(notes(h, dev), 0, "registration notifies nothing");
        for (int r = 0; r < BATCH_ROUNDS; ++r) {
            const uint32_t before = notes(h, dev);
            barrier_wait(&go);   /* release the start phase */
            barrier_wait(&done); /* every thread has started its clients */
            CHECK_EQ(notes(h, dev) - before, 1, "exactly one 0->1 edge for the whole batch");
            CHECK(running(dev), "running while any client is started");
            CHECK_EQ(g_clients[dev - 2].started, BATCH_THREADS * BATCH_CLIENTS, "every client started");
            barrier_wait(&go);   /* release the stop phase */
            barrier_wait(&done);
            CHECK_EQ(notes(h, dev) - before, 2, "exactly one 1->0 edge for the whole batch");
            CHECK(!running(dev), "stopped once the last client stopped");
        }
        for (unsigned i = 0; i < BATCH_THREADS; ++i) join(th[i]);
        CHECK_EQ(notes(h, dev), 2 * BATCH_ROUNDS, "two edges per round and nothing else");
        for (AudioObjectID other = 2; other <= 5; ++other)
            if (other != dev) CHECK_EQ(notes(h, other), 0, "no other device was notified");
        CHECK_EQ(atomic_load(&h->malformed), 0, "well-formed notifications");
        cp_close(d);
        free(h);
    }
}

typedef struct {
    _Atomic int ok, rejected;
    AudioObjectID dev;
} race_ctx;
static void *registrar(void *arg) {
    race_ctx *r = arg;
    static _Atomic uint32_t next_id = 1000;
    for (int i = 0; i < 8; ++i) {
        OSStatus s = add_client(r->dev, atomic_fetch_add(&next_id, 1));
        if (s == noErr) atomic_fetch_add(&r->ok, 1);
        else {
            CHECK(s == kAudioHardwareIllegalOperationError, "overflow is a clean refusal");
            atomic_fetch_add(&r->rejected, 1);
        }
    }
    return NULL;
}
static void test_registration_boundary_race(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    race_ctx rc = {0, 0, 2};
    pthread_t th[16];
    for (int i = 0; i < 16; ++i) th[i] = spawn(registrar, &rc);
    for (int i = 0; i < 16; ++i) join(th[i]);
    CHECK_EQ(atomic_load(&rc.ok), 64, "exactly 64 registrations win the race");
    CHECK_EQ(atomic_load(&rc.rejected), 64, "the rest are rejected");
    CHECK_EQ(g_clients[0].registered, 64, "bounded table");
    cp_close(d);
    free(h);
}

/* ---- Generation oracle: per-cycle payloads, synchronized observations ------------------------- */
/* Every cycle publishes the SAME frame times with a payload that names the cycle, so a frame
 * leaking from another generation is recognizable (a pure function of T could not tell). Readers
 * run through every restart; their observations are only judged when a snapshot of the open cycle
 * is stable across the call, and "published before the call started" demands the exact payload. */
#define ORACLE_BASE 100000u
#define ORACLE_FRAMES 1920u
#define ORACLE_CYCLES 300
static _Atomic int g_open_cycle = -1;  /* the cycle whose transfer is open (set after StartIO returned) */
static _Atomic int g_published = -1;   /* the newest cycle whose window is completely published */
static _Atomic int g_writer_go = -1;
static _Atomic int g_oracle_stop;
static _Atomic int g_reader_verified[2]; /* newest cycle a reader fully verified */
static _Atomic uint64_t g_oracle_nonsilent;
static float pay_l(unsigned c, uint64_t t) { return (float)(c * 4096u + (uint32_t)(t % 4096u)) + 0.25f; }
static float pay_r(unsigned c, uint64_t t) { return -(float)(c * 4096u + (uint32_t)((t * 7u) % 4096u)) - 0.5f; }
static bool is_payload(const uint32_t *f, unsigned c, uint64_t t) {
    return f[0] == fbits(pay_l(c, t)) && f[1] == fbits(pay_r(c, t));
}

static void publish_window(unsigned c) {
    float buf[480 * 2];
    for (uint64_t off = 0; off < ORACLE_FRAMES; off += 480) {
        for (unsigned k = 0; k < 480; ++k) {
            buf[2 * k] = pay_l(c, ORACLE_BASE + off + k);
            buf[2 * k + 1] = pay_r(c, ORACLE_BASE + off + k);
        }
        CHECK(write_speakers(ORACLE_BASE + off, 480, buf) == noErr, "oracle writer");
    }
}
static void *oracle_writer(void *arg) {
    (void)arg;
    int last = -1;
    while (!atomic_load(&g_oracle_stop)) {
        int go = atomic_load(&g_writer_go);
        if (go == last) { sleep_ms(0); continue; }
        publish_window((unsigned)go);
        atomic_store(&g_published, go);
        last = go;
    }
    return NULL;
}
static void *oracle_reader(void *arg) {
    const unsigned me = (unsigned)(uintptr_t)arg;
    float *buf = malloc(480 * 8);
    CHECK(buf != NULL, "alloc");
    uint64_t x = 0x2545f4914f6cdd1dull + me * 977;
    while (!atomic_load(&g_oracle_stop)) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        const int published_before = atomic_load(&g_published);
        const int open_a = atomic_load(&g_open_cycle);
        const uint64_t off = (x % 4) * 480; /* one of the four blocks of the window */
        CHECK(read_speakers(ORACLE_BASE + off + 1024, 480, buf) == noErr, "oracle reader");
        const int open_b = atomic_load(&g_open_cycle);
        if (open_a < 0 || open_a != open_b) continue; /* the transfer changed under the call: not judged */
        const unsigned c = (unsigned)open_a;
        const uint32_t *w = (const uint32_t *)buf;
        unsigned nonsilent = 0;
        for (unsigned k = 0; k < 480; ++k) {
            const uint32_t *f = w + 2 * k;
            const bool silent = f[0] == 0 && f[1] == 0;
            CHECK(silent || is_payload(f, c, ORACLE_BASE + off + k), "a frame is silence or this cycle's exact payload: never another generation's");
            nonsilent += !silent;
        }
        if (published_before == (int)c) { /* the whole window was published before this call began */
            CHECK(nonsilent == 480, "a published window reads back completely");
            atomic_store(&g_reader_verified[me], (int)c);
        }
        atomic_fetch_add(&g_oracle_nonsilent, nonsilent);
    }
    free(buf);
    return NULL;
}
static void wait_until_int(_Atomic int *v, int want, const char *what) {
    for (int i = 0; i < 20000 && atomic_load(v) != want; ++i) sleep_ms(1);
    CHECK_EQ(atomic_load(v), want, what);
}

static void test_generation_oracle(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h);
    g_stop_drain_ns = 5000ull * 1000 * 1000;
    CHECK(add_client(3, 90) == noErr && add_client(2, 91) == noErr && start_io(2, 91) == noErr, "setup");
    atomic_store(&g_open_cycle, -1);
    atomic_store(&g_published, -1);
    atomic_store(&g_writer_go, -1);
    atomic_store(&g_oracle_stop, 0);
    atomic_store(&g_reader_verified[0], -1);
    atomic_store(&g_reader_verified[1], -1);
    atomic_store(&g_oracle_nonsilent, 0);
    pthread_t w = spawn(oracle_writer, NULL), r0 = spawn(oracle_reader, (void *)0), r1 = spawn(oracle_reader, (void *)1);
    uint32_t *win = calloc(ORACLE_FRAMES * 2, sizeof(uint32_t));
    CHECK(win != NULL, "alloc");
    uint32_t last_gen = 0;
    for (int c = 0; c < ORACLE_CYCLES; ++c) {
        CHECK(start_io(3, 90) == noErr, "start the transfer");
        const uint32_t gen = cp_gate_tag_of(atomic_load(&g_xfer.word));
        CHECK(gen > last_gen, "every cycle is a fresh generation");
        last_gen = gen;
        atomic_store(&g_open_cycle, c); /* from here, frames of every older cycle must read as silence */
        /* Reads before fresh publication: the previous cycles' frames are still in the ring (same
         * frame times) but unreadable; this exact synchronous read must be all silence. */
        for (uint64_t off = 0; off < ORACLE_FRAMES; off += 480)
            CHECK(read_speakers(ORACLE_BASE + off + 1024, 480, (float *)(win + off * 2)) == noErr, "pre-publication read");
        for (uint32_t i = 0; i < ORACLE_FRAMES * 2; ++i) CHECK(win[i] == 0, "nothing of an older generation is readable before fresh publication");
        atomic_store(&g_writer_go, c); /* publish this cycle's window */
        wait_until_int(&g_published, c, "the writer published the window");
        for (uint64_t off = 0; off < ORACLE_FRAMES; off += 480)
            CHECK(read_speakers(ORACLE_BASE + off + 1024, 480, (float *)(win + off * 2)) == noErr, "post-publication read");
        for (uint64_t k = 0; k < ORACLE_FRAMES; ++k) CHECK(is_payload(win + 2 * k, (unsigned)c, ORACLE_BASE + k), "the whole window is this cycle's payload");
        /* Both concurrent readers verify a complete read of this cycle before it is stopped. */
        wait_until_int(&g_reader_verified[0], c, "reader 0 verified this cycle");
        wait_until_int(&g_reader_verified[1], c, "reader 1 verified this cycle");
        atomic_store(&g_open_cycle, -1); /* readers stop judging... */
        CHECK(stop_io(3, 90) == noErr, "every stop establishes its barrier"); /* ...then the transfer stops */
        for (uint64_t off = 0; off < ORACLE_FRAMES; off += 480)
            CHECK(read_speakers(ORACLE_BASE + off + 1024, 480, (float *)(win + off * 2)) == noErr, "post-stop read");
        for (uint32_t i = 0; i < ORACLE_FRAMES * 2; ++i) CHECK(win[i] == 0, "nothing is readable after a stop");
    }
    atomic_store(&g_oracle_stop, 1);
    join(w);
    join(r0);
    join(r1);
    fprintf(stderr, "  oracle: %d cycles, %llu non-silent frames verified by concurrent readers\n", ORACLE_CYCLES, (unsigned long long)atomic_load(&g_oracle_nonsilent));
    CHECK(atomic_load(&g_oracle_nonsilent) >= (uint64_t)ORACLE_CYCLES * 2 * 480, "the readers really observed audio in every cycle");
    free(win);
    g_stop_drain_ns = 500ull * 1000 * 1000;
    cp_close(d);
    free(h);
}

/* ---- A failed executable pin fails the factory ------------------------------------------------ */
static void test_pin_failure_fails_factory(void) {
    CHECK(g_refs == 0 && !g_pinned && !g_initialized && !g_image_pinned, "pristine");
    g_test_pin_fail = true; /* inject: the dyld pin cannot be taken */
    CHECK(CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID) == NULL, "no instance without the pin");
    CHECK(g_refs == 0 && !g_pinned && !g_image_pinned, "no reference taken, nothing registered");
    fake_host *h = fake_host_new();
    CHECK((*g_driver)->Initialize(g_driver, &h->iface) == kAudioHardwareUnspecifiedError, "and nothing can be initialized");
    CHECK(CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID) == NULL, "it keeps failing while the pin keeps failing");
    CHECK_EQ(g_refs, 0, "still no reference");
    g_test_pin_fail = false;
    AudioServerPlugInDriverRef d = cp_open(h); /* the pin works again: a normal instance */
    CHECK(g_refs == 1 && g_pinned && g_image_pinned, "the factory succeeds");
    cp_close(d);
    free(h);
}

static size_t heap_blocks(void) {
    malloc_statistics_t st;
    malloc_zone_statistics(NULL, &st);
    return st.blocks_in_use;
}
#if defined(__has_feature)
#if __has_feature(address_sanitizer)
#define CP_UNDER_ASAN 1 /* ASan's quarantine makes whole-heap block counts meaningless */
#endif
#endif
/* The singleton allocates its ring once; any number of Factory/Release cycles allocate nothing more
 * and start no thread. */
static void test_no_leaks_no_threads(void) {
    fake_host *h = fake_host_new();
    AudioServerPlugInDriverRef d = cp_open(h); /* the one reference the test keeps */
    cp_hist_slot *ring = RING();
    CHECK_EQ(atomic_load(&g_test_live_rings), 1, "exactly one ring");
    for (int warm = 0; warm < 3; ++warm) { /* settle one-time CoreFoundation allocations */
        AudioServerPlugInDriverRef x = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
        begin(3, 1);
        begin(2, 2);
        finish(3, 1);
        finish(2, 2);
        CHECK((*x)->Release(x) == 1, "release");
    }
    const size_t blocks = heap_blocks();
    const unsigned threads = thread_count();
    for (int i = 0; i < 300; ++i) {
        AudioServerPlugInDriverRef x = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
        CHECK(x == d, "the same singleton");
        CHECK(thread_count() == threads, "the driver starts no thread");
        CHECK_EQ(atomic_load(&g_test_live_rings), 1, "still exactly one ring");
        CHECK(RING() == ring, "the same ring");
        begin(3, 1);
        begin(2, 2);
        begin(4, 3);
        CHECK(write_pattern(5000, 480) == noErr, "write");
        if (i % 3 == 0) { finish(3, 1); finish(2, 2); finish(4, 3); }              /* stop and remove */
        else { CHECK(stop_io(3, 1) == noErr && stop_io(2, 2) == noErr && stop_io(4, 3) == noErr, "stop only"); } /* stay registered */
        CHECK((*x)->Release(x) == 1, "release the extra reference");
    }
#ifndef CP_UNDER_ASAN
    CHECK_EQ((long long)heap_blocks(), (long long)blocks, "no heap block allocated across 300 Factory/Release cycles");
#else
    (void)blocks;
    fprintf(stderr, "  (ASan: whole-heap block count skipped; the ring allocation count was checked instead)\n");
#endif
    CHECK_EQ(thread_count(), threads, "no thread was started");
    CHECK_EQ(atomic_load(&g_test_live_rings), 1, "the ring was allocated exactly once");
    CHECK(g_clients[0].registered <= 1 && g_clients[1].registered <= 1 && g_clients[2].registered <= 1, "the client tables stay bounded");
    cp_close(d);
    CHECK_EQ(atomic_load(&g_test_live_rings), 0, "the test-only reset freed the ring: no leak");
    free(h);
}

#define RUN(fn) do { fprintf(stderr, "  %s\n", #fn); fn(); } while (0)
int main(void) {
    install_fixture_hook();
    RUN(test_pin_failure_fails_factory);
    RUN(test_factory_refcount);
    RUN(test_release_to_zero_keeps_singleton);
    RUN(test_initialize_host_rules);
    RUN(test_unload_hook_is_noop);
    RUN(test_host_releases_last_reference_inside_notification);
    RUN(test_release_to_zero_with_calls_in_flight);
    RUN(test_no_retained_hal_pointers);
    RUN(test_concurrent_refcounts);
    RUN(test_concurrent_clients);
    RUN(test_notification_batches);
    RUN(test_registration_boundary_race);
    RUN(test_generation_oracle);
    RUN(test_no_leaks_no_threads);
    puts("PASS test_lifecycle: pin failure, refcount/IUnknown, Release to zero keeps the singleton (same ring, host, "
         "clients, transfer; Factory returns it again), Initialize host rules, unload hook no-op, host releases inside a "
         "notification, calls in flight across a Release to zero, concurrent refcounts, concurrent Start/Stop/remove, "
         "exact notification edges, generation oracle, no UAF, no retained HAL pointer, no thread, no leak");
    return 0;
}
