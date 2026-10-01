/* Direct tests 2 and 3: timestamp-addressed stereo history, simultaneous readers, reordered and
 * block-partitioned reads, slow-reader expiry, underrun/overrun, frame/channel alignment;
 * paused and concurrent publication, overwrite, older and duplicate writers, failed CAS,
 * generation/version exhaustion, no mixed frames and no stale history after a stop.
 * No HAL registration, no audio I/O. */
#include "cp_fixture.h"

#define GEN_NOW() cp_gate_tag_of(atomic_load(&g_xfer.word))
#define RING() atomic_load(&g_history)
#define NFRAMES CROSSPANE_AUDIO_HISTORY_FRAMES

/* ---- Driver set-up --------------------------------------------------------------------------- */
static fake_host *H;
static void setup(void) {
    H = fake_host_new();
    cp_open(H);
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90); /* the agent starts reading: transfer opens */
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91);      /* an app starts playing */
}
static void teardown(void) {
    cp_close(g_driver);
    free(H);
    H = NULL;
}

/* ---- Range helpers --------------------------------------------------------------------------- */
static uint32_t *words(UInt32 frames) {
    uint32_t *p = calloc((size_t)frames * 2 + 2, sizeof(uint32_t));
    CHECK(p != NULL, "alloc");
    return p;
}
/* Read frames [t, t+frames) (frame time; input time t+1024), in blocks of `block`. */
static void read_range(uint64_t t, UInt32 frames, UInt32 block, uint32_t *out) {
    for (UInt32 done = 0; done < frames;) {
        UInt32 n = frames - done < block ? frames - done : block;
        CHECK(read_speakers(t + done + 1024, n, (float *)(out + (size_t)done * 2)) == noErr, "read");
        done += n;
    }
}
static void write_range(uint64_t t, UInt32 frames, UInt32 block) {
    for (UInt32 done = 0; done < frames;) {
        UInt32 n = frames - done < block ? frames - done : block;
        CHECK(write_pattern(t + done, n) == noErr, "write");
        done += n;
    }
}
static bool frame_is_pattern(const uint32_t *f, uint64_t t) {
    return f[0] == fbits(fl(t)) && f[1] == fbits(fr(t));
}
static bool frame_is_silence(const uint32_t *f) { return f[0] == 0 && f[1] == 0; }
static void expect_pattern(const uint32_t *out, uint64_t t, UInt32 frames, const char *what) {
    for (UInt32 k = 0; k < frames; ++k) CHECK(frame_is_pattern(out + (size_t)k * 2, t + k), what);
}
static void expect_silence(const uint32_t *out, UInt32 frames, const char *what) {
    for (UInt32 k = 0; k < frames; ++k) CHECK(frame_is_silence(out + (size_t)k * 2), what);
}

/* ---- Direct 2: history through the real driver vtable ---------------------------------------- */
static void test_partitioned_roundtrip(void) {
    setup();
    const UInt32 parts[] = {1, 7, 480, 1000, 4096};
    const UInt32 N = 6000;
    uint64_t base = 100000;
    for (unsigned w = 0; w < 5; ++w) {
        write_range(base, N, parts[w]); /* one write partition per region */
        for (unsigned r = 0; r < 5; ++r) { /* every read partition of that region */
            uint32_t *out = words(N);
            read_range(base, N, parts[r], out);
            expect_pattern(out, base, N, "block-partitioned read equals the written frames");
            free(out);
        }
        base += 9000; /* regions do not share ring slots: 5 * 9000 < 16384 * 3 but spaced widely */
    }
    teardown();
}

typedef struct {
    uint64_t base;
    UInt32 frames, block;
    int iterations;
    _Atomic int done;
} reader_job;

static void *reader_thread(void *arg) {
    reader_job *j = arg;
    uint32_t *out = words(j->frames);
    for (int i = 0; i < j->iterations; ++i) {
        read_range(j->base, j->frames, j->block, out);
        expect_pattern(out, j->base, j->frames, "simultaneous readers see identical, intact data");
    }
    free(out);
    atomic_store(&j->done, 1);
    return NULL;
}

static void test_two_simultaneous_readers(void) {
    setup();
    const uint64_t base = 100000;
    const UInt32 N = 4000;
    write_range(base, N, 480);
    reader_job a = {base, N, 480, 300, 0}, b = {base, N, 7, 60, 0}, c = {base, N, 4096, 300, 0};
    pthread_t ta = spawn(reader_thread, &a), tb = spawn(reader_thread, &b), tc = spawn(reader_thread, &c);
    /* The writer keeps publishing newer frames (other slots) while the readers run. */
    for (uint64_t t = base + 6000; t < base + 6000 + 4000; t += 400) CHECK(write_pattern(t, 400) == noErr, "concurrent writes");
    join(ta);
    join(tb);
    join(tc);
    /* Reading never consumed anything: the data is still there. */
    uint32_t *out = words(N);
    read_range(base, N, 1024, out);
    expect_pattern(out, base, N, "retained frames are still readable after many reads");
    free(out);
    teardown();
}

static void test_reordered_reads(void) {
    setup();
    const uint64_t base = 150000;
    const UInt32 N = 5000;
    write_range(base, N, 333);
    uint32_t *whole = words(N), *assembled = words(N);
    read_range(base, N, 4096, whole);
    expect_pattern(whole, base, N, "reference read");
    /* Read the same region as randomly ordered, randomly sized blocks. */
    bool *covered = calloc(N, sizeof(bool));
    CHECK(covered != NULL, "alloc");
    uint64_t x = 0x9e3779b97f4a7c15ull;
    UInt32 remaining = N;
    while (remaining) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        UInt32 start = (UInt32)(x % N), len = 1 + (UInt32)((x >> 20) % 700);
        if (start + len > N) len = N - start;
        CHECK(read_speakers(base + start + 1024, len, (float *)(assembled + (size_t)start * 2)) == noErr, "random read");
        for (UInt32 k = start; k < start + len; ++k)
            if (!covered[k]) { covered[k] = true; --remaining; }
    }
    CHECK(memcmp(whole, assembled, (size_t)N * 8) == 0, "reordered, block-partitioned reads are bit-identical");
    free(covered);
    free(whole);
    free(assembled);
    teardown();
}

static void test_expiry_underrun_overrun(void) {
    setup();
    const uint64_t t0 = 200000;
    const UInt32 over = 100;
    /* Overrun: the writer laps the ring. Frames [t0, t0+over) are overwritten by
     * [t0+16384, t0+16384+over), so a slow reader finds them expired (silence). */
    write_range(t0, NFRAMES + over, 480);
    uint32_t *out = words(NFRAMES + over);
    read_range(t0, NFRAMES + over, 4096, out);
    expect_silence(out, over, "an expired frame is silence");
    expect_pattern(out + (size_t)over * 2, t0 + over, NFRAMES, "everything within one ring length is retained");
    /* A slow reader far behind sees only silence. */
    uint32_t *old = words(2000);
    read_range(t0 - 50000 + 20000, 2000, 500, old); /* long before the oldest retained frame */
    expect_silence(old, 2000, "a reader far behind the writer reads silence");
    free(old);
    free(out);

    /* Underrun: frames not yet written are silence, frame by frame, at the exact boundary. */
    const uint64_t t1 = 400000;
    write_range(t1, 1000, 100);
    uint32_t *edge = words(2000);
    read_range(t1 - 500, 2000, 333, edge);
    expect_silence(edge, 500, "before the first published frame");
    expect_pattern(edge + 500 * 2, t1, 1000, "published frames");
    expect_silence(edge + 1500 * 2, 500, "after the last published frame (underrun)");
    free(edge);
    teardown();
}

static void test_random_alignment_property(void) {
    setup();
    const uint64_t base = 600000;
    const UInt32 W = 12000;
    bool *pub = calloc(W, sizeof(bool));
    CHECK(pub != NULL, "alloc");
    uint64_t x = 88172645463325252ull;
    for (int i = 0; i < 80; ++i) { /* random, non-overlapping, partially published layout */
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        UInt32 s = (UInt32)(x % W), len = 1 + (UInt32)((x >> 24) % 700);
        if (s + len > W) len = W - s;
        bool free_range = true;
        for (UInt32 k = s; k < s + len; ++k) free_range = free_range && !pub[k];
        if (!free_range) continue;
        CHECK(write_pattern(base + s, len) == noErr, "random segment write");
        for (UInt32 k = s; k < s + len; ++k) pub[k] = true;
    }
    float *got = malloc(1600 * 8);
    CHECK(got != NULL, "alloc");
    for (int i = 0; i < 4000; ++i) { /* random reads, random alignment, straddling published and unpublished */
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        int64_t start = (int64_t)(x % (W + 3000)) - 1500;
        UInt32 len = 1 + (UInt32)((x >> 30) % 1500);
        CHECK(read_speakers(base + (uint64_t)(start + 1024), len, got) == noErr, "read");
        const uint32_t *g = (const uint32_t *)got;
        for (UInt32 k = 0; k < len; ++k) {
            int64_t rel = start + (int64_t)k;
            bool published = rel >= 0 && rel < (int64_t)W && pub[rel];
            uint64_t t = base + (uint64_t)rel;
            if (published) CHECK(frame_is_pattern(g + (size_t)k * 2, t), "published frame: both channels, right time");
            else CHECK(frame_is_silence(g + (size_t)k * 2), "unpublished frame: silence in both channels");
        }
    }
    free(got);
    free(pub);
    teardown();
}

static void test_microphone_stays_silent_beside_speakers(void) {
    setup();
    begin(CROSSPANE_DEV_MIC_APP, 80);
    begin(CROSSPANE_DEV_MIC_LOOPBACK, 81);
    write_range(100000, 2000, 480);
    float mono[480];
    for (int i = 0; i < 480; ++i) mono[i] = 0.75f;
    AudioServerPlugInIOCycleInfo c = cycle_info(100000 + 1024, 100000, 480);
    CHECK((*g_driver)->DoIOOperation(g_driver, 5, 9, 1, kAudioServerPlugInIOOperationWriteMix, 480, &c, mono, NULL) == noErr, "synthetic mic write");
    CHECK((*g_driver)->DoIOOperation(g_driver, 4, 8, 1, kAudioServerPlugInIOOperationReadInput, 480, &c, mono, NULL) == noErr, "mic read");
    for (int i = 0; i < 480; ++i) CHECK(mono[i] == 0.0f, "the microphone is silent while speakers carry audio");
    uint32_t *out = words(2000);
    read_range(100000, 2000, 480, out);
    expect_pattern(out, 100000, 2000, "the speaker history is not affected by the mic pair");
    free(out);
    teardown();
}

/* ---- Direct 3: stop, restart, generations, old writers ---------------------------------------- */
static void test_stop_restart_no_backlog(void) {
    setup();
    const uint32_t g1 = GEN_NOW();
    CHECK(g1 != 0, "a transfer generation is active");
    write_range(300000, 960, 480);
    uint32_t *out = words(2400);
    read_range(300000, 960, 480, out);
    expect_pattern(out, 300000, 960, "data flows while the hidden side runs");

    CHECK(stop_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == noErr, "hidden stop");
    CHECK(!running(3) && running(2), "visible keeps running");
    /* While the hidden side is stopped, speaker writes are discarded and reads are silence. */
    write_range(300960, 960, 480);
    read_range(300000, 1920, 480, out);
    expect_silence(out, 1920, "nothing is readable while stopped (no post-stop stale history)");

    CHECK(start_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == noErr, "hidden restart");
    const uint32_t g2 = GEN_NOW();
    CHECK_EQ(g2, g1 + 1, "a restart is a fresh transfer generation");
    read_range(300000, 1920, 480, out);
    expect_silence(out, 1920, "restart receives no old backlog (neither pre-stop nor during-stop frames)");
    write_range(302000, 480, 480);
    read_range(302000, 480, 480, out);
    expect_pattern(out, 302000, 480, "new frames flow after the restart");
    read_range(300000, 1920, 480, out);
    expect_silence(out, 1920, "old history stays unreadable after new traffic");

    /* A stale writer (old generation) that bypasses admission cannot overwrite a newer frame,
     * and even where it does publish, the new transfer's readers never see its data. */
    cp_hist_slot *ring = RING();
    CHECK_EQ(cp_hist_write(ring, g1, 302000, 0x11111111u, 0x22222222u), CP_HIST_DROP_STALE, "old generation cannot overwrite a newer frame");
    CHECK_EQ(cp_hist_write(ring, g1, 305000, 0x33333333u, 0x44444444u), CP_HIST_PUBLISHED, "an old frame can only land in an empty slot");
    read_range(305000, 8, 8, out);
    expect_silence(out, 8, "...and the restarted transfer never reads it");
    read_range(302000, 480, 480, out);
    expect_pattern(out, 302000, 480, "the newer frames are intact");
    free(out);
    teardown();
}

/* A driver call made by a thread that parks inside it (see parker in the fixture). */
typedef struct {
    parker *p;
    uint64_t t;
    UInt32 frames;
    const float *data; /* NULL: the deterministic pattern; else frames*2 floats */
    float *out;        /* reads: result buffer */
    OSStatus status;
} call_job;
static void *job_writer(void *arg) {
    call_job *j = arg;
    tl_parker = j->p;
    j->status = j->data ? write_speakers(j->t, j->frames, j->data) : write_pattern(j->t, j->frames);
    tl_parker = NULL;
    return NULL;
}
static void *job_reader(void *arg) {
    call_job *j = arg;
    tl_parker = j->p;
    j->status = read_speakers(j->t + 1024, j->frames, j->out);
    tl_parker = NULL;
    return NULL;
}

typedef struct {
    _Atomic int returned;
    OSStatus status;
} stop_job;
static void *stopper(void *arg) {
    stop_job *j = arg;
    j->status = stop_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    atomic_store(&j->returned, 1);
    return NULL;
}
/* StopIO of the hidden device on a thread, acknowledged: returns once its drain is demonstrably
 * waiting (not by sleeping), with the result still pending. */
static pthread_t start_stopper_and_wait_in_drain(stop_job *sj) {
    const uint64_t naps = atomic_load(&g_drain_naps);
    sj->returned = 0;
    sj->status = 0xffff;
    pthread_t t = spawn(stopper, sj);
    wait_drain_naps(naps, 3);
    CHECK(!atomic_load(&sj->returned), "StopIO is held at the barrier by the admitted access");
    CHECK(cp_gate_is_closed(&g_xfer), "...but publication is already disabled");
    return t;
}

static void test_old_writer_after_failed_barrier(void) {
    setup();
    g_stop_drain_ns = 50ull * 1000 * 1000; /* a short barrier for this test */
    const uint32_t g1 = GEN_NOW();
    parker p;
    parker_init(&p, CP_HOOK_IO_ADMITTED);
    call_job job = {&p, 600000, 480, NULL, NULL, 0xffff};
    pthread_t w = spawn(job_writer, &job);
    parker_wait(&p); /* the writer is admitted under g1, parked before its first frame */
    CHECK_EQ(cp_gate_inflight(&g_xfer), 1, "one admitted access");

    /* The barrier cannot be established while an old access is admitted: StopIO must say so
     * (never a false success), yet the stop itself is recorded and publication stays closed. */
    CHECK(stop_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == kAudioHardwareUnspecifiedError, "no false successful barrier");
    CHECK(!running(3), "the stop is recorded");
    CHECK(cp_gate_is_closed(&g_xfer), "publication is disabled");
    CHECK(start_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == noErr, "restart with an old access still admitted");
    const uint32_t g2 = GEN_NOW();
    CHECK_EQ(g2, g1 + 1, "fresh generation");

    parker_release(&p); /* the old writer wakes up inside the restarted transfer */
    join(w);
    CHECK(job.status == noErr, "the parked write returns normally");
    /* It must not have published anything, in either generation. */
    cp_hist_slot *ring = RING();
    for (UInt32 k = 0; k < 480; ++k)
        CHECK_EQ(atomic_load(&ring[(600000 + k) & (NFRAMES - 1)].meta), 0, "old writer published nothing");
    uint32_t *out = words(480);
    read_range(600000, 480, 480, out);
    expect_silence(out, 480, "the restarted transfer sees no old-generation data");
    free(out);
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "access retired");
    g_stop_drain_ns = 500ull * 1000 * 1000;
    teardown();
}

/* Finding: an admitted reader that survives a timed-out StopIO and a restart must not return the
 * stopped transfer's PCM. Two exact schedules: parked at admission, and parked mid-block. */
static void test_retired_reader_after_failed_barrier_and_restart(void) {
    for (int mid_block = 0; mid_block < 2; ++mid_block) {
        setup();
        g_stop_drain_ns = 50ull * 1000 * 1000;
        const uint64_t t0 = 720000 + (uint64_t)mid_block * 100000;
        write_range(t0, 480, 480); /* published under the first transfer */
        float *out = malloc(480 * 8);
        CHECK(out != NULL, "alloc");
        memset(out, 0x7f, 480 * 8);
        parker p;
        parker_init(&p, mid_block ? CP_HOOK_READ_PAYLOAD_LOADED : CP_HOOK_IO_ADMITTED);
        call_job job = {&p, t0, 480, NULL, out, 0xffff};
        pthread_t r = spawn(job_reader, &job);
        parker_wait(&p);
        CHECK(stop_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == kAudioHardwareUnspecifiedError, "the barrier times out");
        CHECK(start_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == noErr, "restart with the old reader still admitted");
        parker_release(&p);
        join(r);
        CHECK(job.status == noErr, "the retired read returns normally");
        const uint32_t *w = (const uint32_t *)out;
        for (UInt32 k = mid_block ? 1 : 0; k < 480; ++k)
            CHECK(frame_is_silence(w + (size_t)k * 2), "a retired reader never returns the stopped transfer's PCM");
        if (mid_block) /* frame 0 was already being read when the stop began: pattern or silence */
            CHECK(frame_is_pattern(w, t0) || frame_is_silence(w), "the frame in flight is whole");
        uint32_t *again = words(480);
        read_range(t0, 480, 480, again);
        expect_silence(again, 480, "and a fresh read after the restart has no old backlog either");
        free(again);
        free(out);
        g_stop_drain_ns = 500ull * 1000 * 1000;
        teardown();
    }
}

static void test_stop_barrier_drains_admitted_access(void) {
    setup();
    g_stop_drain_ns = 5000ull * 1000 * 1000;
    parker p;
    parker_init(&p, CP_HOOK_IO_ADMITTED);
    call_job job = {&p, 700000, 480, NULL, NULL, 0xffff};
    pthread_t w = spawn(job_writer, &job);
    parker_wait(&p);
    stop_job sj;
    pthread_t s = start_stopper_and_wait_in_drain(&sj);
    parker_release(&p);
    join(w);
    join(s);
    CHECK(sj.status == noErr, "the barrier reports success once the old access has drained");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "nothing admitted after a successful barrier");
    cp_hist_slot *ring = RING();
    for (UInt32 k = 0; k < 480; ++k)
        CHECK_EQ(atomic_load(&ring[(700000 + k) & (NFRAMES - 1)].meta), 0, "stop disabled publication before draining");
    g_stop_drain_ns = 500ull * 1000 * 1000;
    teardown();
}

/* ---- Driver-level reproductions of the ring pauses (through DoIOOperation) ------------------- */
static void write_one(uint64_t t, uint32_t lbits, uint32_t rbits) {
    uint32_t w[2] = {lbits, rbits};
    float f[2];
    memcpy(f, w, sizeof(f));
    CHECK(write_speakers(t, 1, f) == noErr, "single-frame write");
}
static void read_one(uint64_t t, uint32_t *l, uint32_t *r) {
    float f[2] = {1.0f, 1.0f};
    CHECK(read_speakers(t + 1024, 1, f) == noErr, "single-frame read");
    uint32_t w[2];
    memcpy(w, f, sizeof(w));
    *l = w[0];
    *r = w[1];
}
static void expect_one(uint64_t t, uint32_t l, uint32_t r, const char *what) {
    uint32_t gl, gr;
    read_one(t, &gl, &gr);
    CHECK(gl == l && gr == r, what);
}

static void test_driver_paused_publication(void) {
    setup();
    const uint64_t t0 = 40000, t1 = t0 + NFRAMES;
    write_one(t0, 0x3f800000u, 0x40000000u); /* 1.0f / 2.0f published at lap 0 */
    expect_one(t0, 0x3f800000u, 0x40000000u, "baseline");
    parker p;
    parker_init(&p, CP_HOOK_WRITE_CLAIMED);
    const float b[2] = {-4.0f, -8.0f};
    call_job w = {&p, t1, 1, b, NULL, 0xffff};
    pthread_t t = spawn(job_writer, &w);
    parker_wait(&p); /* the DoIO write owns the slot but has stored no payload */
    expect_one(t0, 0, 0, "a slot being overwritten reads as silence: not the old frame, not a mix");
    expect_one(t1, 0, 0, "an unpublished frame is silence");
    write_one(t1, 0x41200000u, 0x41a00000u); /* a competing write through the driver */
    expect_one(t1, 0, 0, "the contended write was dropped, not waited for");
    parker_release(&p);
    join(t);
    CHECK(w.status == noErr, "the paused write completes");
    uint32_t bl, br;
    memcpy(&bl, &b[0], 4);
    memcpy(&br, &b[1], 4);
    expect_one(t1, bl, br, "the paused writer's frame is whole and intact (L and R from one write)");
    expect_one(t0, 0, 0, "the replaced frame is gone");
    teardown();
}

/* Paused after the claim, then stop and restart: StopIO waits for the claimant (acknowledged), the
 * claimed frame is the last one published, and nothing of the block after it ever appears. */
static void test_driver_paused_publication_with_stop_and_restart(void) {
    setup();
    g_stop_drain_ns = 5000ull * 1000 * 1000;
    const uint64_t t1 = 90000;
    const uint32_t g1 = GEN_NOW();
    parker p;
    parker_init(&p, CP_HOOK_WRITE_CLAIMED);
    call_job w = {&p, t1, 8, NULL, NULL, 0xffff}; /* frame 0 claimed, 1..7 still to come */
    pthread_t t = spawn(job_writer, &w);
    parker_wait(&p);
    stop_job sj;
    pthread_t s = start_stopper_and_wait_in_drain(&sj);
    parker_release(&p);
    join(t);
    join(s);
    CHECK(sj.status == noErr && w.status == noErr, "barrier reached; the claimant finished");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "barrier ordering: StopIO returned after the claimant left");
    cp_hist_slot *ring = RING();
    CHECK_EQ(atomic_load(&ring[t1 & (NFRAMES - 1)].meta), (long long)cp_hist_key(g1, t1), "the claimed frame was published whole");
    for (UInt32 k = 1; k < 8; ++k)
        CHECK_EQ(atomic_load(&ring[(t1 + k) & (NFRAMES - 1)].meta), 0, "publication stopped at the barrier");
    CHECK(start_io(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90) == noErr, "restart");
    uint32_t *out = words(8);
    read_range(t1, 8, 8, out);
    expect_silence(out, 8, "the restarted transfer sees nothing of the stopped one");
    free(out);
    g_stop_drain_ns = 500ull * 1000 * 1000;
    teardown();
}

/* Paused after the claim, then a Release to ZERO: the driver is a process-lifetime singleton, so
 * Release returns at once and frees nothing; the claimant finishes on live storage and its frames
 * are readable afterwards, from the same ring, through a fresh Factory reference. */
static void test_driver_paused_publication_with_release_to_zero(void) {
    setup();
    parker p;
    parker_init(&p, CP_HOOK_WRITE_CLAIMED);
    call_job w = {&p, 95000, 8, NULL, NULL, 0xffff};
    pthread_t t = spawn(job_writer, &w);
    parker_wait(&p);
    cp_hist_slot *ring = RING();
    CHECK((*g_driver)->Release(g_driver) == 0, "Release to zero returns at once, it waits for nothing");
    CHECK(RING() == ring && atomic_load(&g_test_live_rings) == 1, "storage is not freed under the claimant");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 1, "the claimant is still admitted");
    AudioServerPlugInDriverRef again = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(again == g_driver && RING() == ring, "the same live singleton");
    parker_release(&p);
    join(t);
    CHECK(w.status == noErr, "the claimant finished on live storage");
    CHECK_EQ(cp_gate_inflight(&g_xfer), 0, "and left");
    uint32_t *out = words(8);
    read_range(95000, 8, 8, out);
    expect_pattern(out, 95000, 8, "the frames it published are readable after the Release to zero");
    free(out);
    CHECK(RING() == ring, "same ring");
    teardown();
}

/* A reader parked after loading a payload, with the slot overwritten under it: the public result
 * is silence for that frame; combined with stop, StopIO waits for the reader. */
static void test_driver_overwrite_during_read(void) {
    for (int with_stop = 0; with_stop < 2; ++with_stop) {
        setup();
        g_stop_drain_ns = 5000ull * 1000 * 1000;
        const uint64_t t2 = 130000 + (uint64_t)with_stop * 20000;
        write_range(t2, 16, 16);
        float *out = malloc(16 * 8);
        CHECK(out != NULL, "alloc");
        parker p;
        parker_init(&p, CP_HOOK_READ_PAYLOAD_LOADED);
        call_job r = {&p, t2, 16, NULL, out, 0xffff};
        pthread_t t = spawn(job_reader, &r);
        parker_wait(&p); /* frame 0's payload is loaded, its identity not yet re-checked */
        write_one(t2 + NFRAMES, 0x40a00000u, 0x40c00000u); /* overwrite frame 0's slot: a newer lap */
        stop_job sj;
        pthread_t s = 0;
        if (with_stop) s = start_stopper_and_wait_in_drain(&sj);
        parker_release(&p);
        join(t);
        if (with_stop) {
            join(s);
            CHECK(sj.status == noErr, "StopIO returned only after the reader left");
        }
        CHECK(r.status == noErr, "the racing read returns normally");
        const uint32_t *w = (const uint32_t *)out;
        CHECK(frame_is_silence(w), "the overwritten frame is rejected: silence, never stale or mixed");
        for (UInt32 k = 1; k < 16; ++k) {
            if (with_stop) CHECK(frame_is_silence(w + (size_t)k * 2), "after the stop the rest of the block is silence");
            else CHECK(frame_is_pattern(w + (size_t)k * 2, t2 + k), "without a stop the neighbours are intact");
        }
        free(out);
        g_stop_drain_ns = 500ull * 1000 * 1000;
        teardown();
    }
}

/* A competing write slips in between a writer's decision and its CAS: the CAS fails, the victim's
 * frame is dropped, and the public read shows only the winner. */
static uint64_t g_cas_time;
static void inject_rival_through_driver(void) {
    write_one(g_cas_time, 0x41100000u, 0x41200000u); /* nested driver call from inside the hook */
}
static void test_driver_failed_cas(void) {
    setup();
    const uint64_t t3 = 150000, t4 = t3 + NFRAMES;
    write_one(t3, 0x3f800000u, 0x40000000u);
    g_cas_time = t4;
    parker p;
    parker_init(&p, CP_HOOK_WRITE_BEFORE_CLAIM);
    p.inject = inject_rival_through_driver;
    const float victim[2] = {-1.0f, -2.0f};
    call_job w = {&p, t4, 1, victim, NULL, 0xffff};
    pthread_t t = spawn(job_writer, &w);
    join(t);
    CHECK(w.status == noErr, "the victim's call itself succeeds");
    expect_one(t4, 0x41100000u, 0x41200000u, "only the rival's frame is visible: the failed CAS dropped the victim's");
    teardown();
}

static void test_transfer_inactive_until_hidden_start(void) {
    H = fake_host_new();
    cp_open(H);
    begin(CROSSPANE_DEV_SPEAKERS_APP, 91); /* an app plays; the agent is not reading */
    CHECK(cp_gate_is_closed(&g_xfer), "no transfer without the hidden device");
    write_range(100000, 960, 480); /* discarded */
    begin(CROSSPANE_DEV_SPEAKERS_LOOPBACK, 90);
    uint32_t *out = words(960);
    read_range(100000, 960, 480, out);
    expect_silence(out, 960, "writes made before the hidden side started were discarded");
    free(out);
    teardown();
}

/* ---- Direct 3: the history ring itself --------------------------------------------------------- */
static cp_hist_slot *new_ring(void) {
    cp_hist_slot *r = calloc(NFRAMES, sizeof(*r));
    CHECK(r != NULL, "ring alloc");
    return r;
}
static bool ring_read(const cp_hist_slot *r, uint32_t gen, uint64_t t, uint32_t *l, uint32_t *rr) {
    return cp_hist_read(r, gen, t, l, rr);
}

static void test_ring_older_and_duplicate_writers(void) {
    cp_hist_slot *r = new_ring();
    uint32_t l, rr;
    CHECK_EQ(cp_hist_write(r, 5, 1000, 1, 2), CP_HIST_PUBLISHED, "first write publishes");
    CHECK(ring_read(r, 5, 1000, &l, &rr) && l == 1 && rr == 2, "published");
    CHECK_EQ(cp_hist_write(r, 5, 1000, 3, 4), CP_HIST_DROP_STALE, "a duplicate cannot overwrite a published frame");
    CHECK(ring_read(r, 5, 1000, &l, &rr) && l == 1 && rr == 2, "frames are immutable once published (identical for every reader)");
    CHECK_EQ(cp_hist_write(r, 5, 1000 + NFRAMES, 5, 6), CP_HIST_PUBLISHED, "a newer lap overwrites");
    CHECK(!ring_read(r, 5, 1000, &l, &rr), "the old lap is gone (expired)");
    CHECK(ring_read(r, 5, 1000 + NFRAMES, &l, &rr) && l == 5 && rr == 6, "the new lap is readable");
    CHECK_EQ(cp_hist_write(r, 5, 1000, 7, 8), CP_HIST_DROP_STALE, "an older lap cannot come back");
    CHECK_EQ(cp_hist_write(r, 4, 1000 + NFRAMES, 9, 9), CP_HIST_DROP_STALE, "an older generation cannot overwrite");
    CHECK_EQ(cp_hist_write(r, 4, 1000 + 2 * NFRAMES, 9, 9), CP_HIST_DROP_STALE, "...not even with a newer lap");
    CHECK_EQ(cp_hist_write(r, 6, 1000 + NFRAMES, 10, 11), CP_HIST_PUBLISHED, "a newer generation overwrites the same lap");
    CHECK(!ring_read(r, 5, 1000 + NFRAMES, &l, &rr), "the old generation no longer reads");
    CHECK(ring_read(r, 6, 1000 + NFRAMES, &l, &rr) && l == 10 && rr == 11, "the new generation reads");
    CHECK_EQ(cp_hist_write(r, 7, 1000, 12, 13), CP_HIST_PUBLISHED, "a newer generation wins even with a smaller lap");
    CHECK(!ring_read(r, 6, 1000 + NFRAMES, &l, &rr), "overwritten");
    CHECK(ring_read(r, 7, 1000, &l, &rr) && l == 12 && rr == 13, "readable");
    /* A frame is addressed exactly: another slot, lap or generation reads as silence. */
    CHECK(!ring_read(r, 7, 1001, &l, &rr), "neighbour slot is empty");
    CHECK(!ring_read(r, 7, 1000 + NFRAMES, &l, &rr), "other lap");
    CHECK(!ring_read(r, 8, 1000, &l, &rr), "other generation");
    free(r);
}

static void test_ring_version_exhaustion(void) {
    cp_hist_slot *r = new_ring();
    uint32_t l, rr;
    CHECK_EQ(cp_hist_write(r, 0, 10, 1, 1), CP_HIST_DROP_RANGE, "generation 0 means empty and never writes");
    CHECK_EQ(cp_hist_write(r, CP_HIST_GEN_MAX + 1, 10, 1, 1), CP_HIST_DROP_RANGE, "generation beyond 31 bits");
    CHECK_EQ(cp_hist_write(r, 0xffffffffu, 10, 1, 1), CP_HIST_DROP_RANGE, "all-ones generation");
    CHECK_EQ(cp_hist_write(r, CP_HIST_GEN_MAX, 10, 21, 22), CP_HIST_PUBLISHED, "the last generation works");
    CHECK(ring_read(r, CP_HIST_GEN_MAX, 10, &l, &rr) && l == 21 && rr == 22, "last generation reads");
    CHECK(!ring_read(r, CP_HIST_GEN_MAX + 1, 10, &l, &rr) && !ring_read(r, 0, 10, &l, &rr), "out-of-range generations never read");
    /* A 32-bit lap: the last addressable time is 2^46 - 1. Beyond it nothing wraps. */
    const uint64_t last = CP_HIST_TIME_LIMIT - 1;
    CHECK_EQ(cp_hist_write(r, 1, last, 31, 32), CP_HIST_PUBLISHED, "last addressable frame");
    CHECK(ring_read(r, 1, last, &l, &rr) && l == 31 && rr == 32, "last frame reads");
    CHECK_EQ(cp_hist_write(r, 1, CP_HIST_TIME_LIMIT, 1, 1), CP_HIST_DROP_RANGE, "time beyond the lap field is refused, never wrapped");
    CHECK_EQ(cp_hist_write(r, 1, CP_HIST_TIME_LIMIT + 5, 1, 1), CP_HIST_DROP_RANGE, "far beyond");
    CHECK_EQ(cp_hist_write(r, 1, ~0ull, 1, 1), CP_HIST_DROP_RANGE, "all-ones time");
    CHECK(!ring_read(r, 1, CP_HIST_TIME_LIMIT, &l, &rr), "out-of-range read is silence");
    /* The slot of the wrapped time was not touched by the refused writes. */
    CHECK(ring_read(r, 1, last, &l, &rr) && l == 31, "refused writes changed nothing");
    free(r);
}

typedef struct {
    cp_hist_slot *ring;
    uint32_t gen;
    uint64_t t, l, rr;
    cp_hist_result result;
    parker *p;
} wjob;
static wjob *g_inject_job;
static cp_hist_result g_injected_result;
static void inject_competing_write(void) { /* runs inside the first writer, before its CAS */
    wjob *j = g_inject_job;
    g_injected_result = cp_hist_write(j->ring, j->gen, j->t, (uint32_t)j->l, (uint32_t)j->rr);
}
static void *ring_writer(void *arg) {
    wjob *j = arg;
    tl_parker = j->p;
    j->result = cp_hist_write(j->ring, j->gen, j->t, (uint32_t)j->l, (uint32_t)j->rr);
    tl_parker = NULL;
    return NULL;
}

static void test_ring_paused_publication(void) {
    cp_hist_slot *r = new_ring();
    uint32_t l, rr;
    CHECK_EQ(cp_hist_write(r, 1, 2000, 100, 101), CP_HIST_PUBLISHED, "baseline");
    parker pk;
    parker_init(&pk, CP_HOOK_WRITE_CLAIMED);
    wjob w1 = {r, 1, 2000 + NFRAMES, 200, 201, 0, &pk};
    pthread_t t = spawn(ring_writer, &w1);
    parker_wait(&pk); /* W1 owns the slot but has stored no payload */
    CHECK(!ring_read(r, 1, 2000, &l, &rr), "a slot being overwritten is silence: not the old frame, not a mix");
    CHECK(!ring_read(r, 1, 2000 + NFRAMES, &l, &rr), "an unpublished frame is silence");
    CHECK_EQ(cp_hist_write(r, 1, 2000 + NFRAMES, 300, 301), CP_HIST_DROP_CONTENDED, "a contended write is dropped, never waited for");
    CHECK_EQ(cp_hist_write(r, 1, 2000 + 2 * NFRAMES, 400, 401), CP_HIST_DROP_CONTENDED, "even a newer lap is dropped while the slot is claimed");
    parker_release(&pk);
    join(t);
    CHECK_EQ(w1.result, CP_HIST_PUBLISHED, "the paused writer completes");
    CHECK(ring_read(r, 1, 2000 + NFRAMES, &l, &rr) && l == 200 && rr == 201, "its frame is whole and intact (L and R from the same write)");
    CHECK(!ring_read(r, 1, 2000, &l, &rr), "the replaced frame is gone");
    free(r);
}

static void *ring_reader_pausing(void *arg) {
    wjob *j = arg;
    tl_parker = j->p;
    uint32_t l, rr;
    j->result = cp_hist_read(j->ring, j->gen, j->t, &l, &rr) ? CP_HIST_PUBLISHED : CP_HIST_DROP_STALE;
    tl_parker = NULL;
    return NULL;
}

static void test_ring_overwrite_during_read(void) {
    cp_hist_slot *r = new_ring();
    CHECK_EQ(cp_hist_write(r, 1, 3000, 1000, 1001), CP_HIST_PUBLISHED, "baseline");
    parker pk;
    parker_init(&pk, CP_HOOK_READ_PAYLOAD_LOADED);
    wjob rj = {r, 1, 3000, 0, 0, 0, &pk};
    pthread_t t = spawn(ring_reader_pausing, &rj);
    parker_wait(&pk); /* the reader has loaded the payload but not yet re-checked the identity */
    CHECK_EQ(cp_hist_write(r, 1, 3000 + NFRAMES, 2000, 2001), CP_HIST_PUBLISHED, "overwritten between the payload load and the check");
    parker_release(&pk);
    join(t);
    CHECK_EQ(rj.result, CP_HIST_DROP_STALE, "the racing read is rejected: silence, never a mixed or stale frame");
    free(r);
}

static void test_ring_failed_cas(void) {
    cp_hist_slot *r = new_ring();
    uint32_t l, rr;
    CHECK_EQ(cp_hist_write(r, 1, 4000, 1, 2), CP_HIST_PUBLISHED, "baseline");
    parker pk;
    parker_init(&pk, CP_HOOK_WRITE_BEFORE_CLAIM); /* rival slips in between the victim's check and its CAS */
    pk.inject = inject_competing_write;
    wjob victim = {r, 1, 4000 + NFRAMES, 111, 222, 0, &pk};
    wjob rival = {r, 1, 4000 + NFRAMES, 333, 444, 0, NULL};
    g_inject_job = &rival;
    g_injected_result = CP_HIST_DROP_RANGE;
    tl_parker = &pk;
    victim.result = cp_hist_write(r, 1, victim.t, (uint32_t)victim.l, (uint32_t)victim.rr);
    tl_parker = NULL;
    CHECK_EQ(g_injected_result, CP_HIST_PUBLISHED, "the rival wins the slot");
    CHECK_EQ(victim.result, CP_HIST_DROP_CONTENDED, "the failed CAS drops the write: one attempt, no retry");
    CHECK(ring_read(r, 1, 4000 + NFRAMES, &l, &rr) && l == 333 && rr == 444, "only the winner's frame is visible");
    free(r);
}

/* ---- Concurrent publication stress: no mixed or torn frames ----------------------------------- */
/* Bounded handshakes instead of timing: every reader must collect STRESS_QUOTA coherent frames;
 * the writers keep lapping the ring (at least STRESS_MIN_LAPS, at most STRESS_MAX_LAPS) until
 * every reader has, so the quota cannot depend on how fast the machine is. Reaching the cap with
 * a reader still short is a real failure. */
#define STRESS_WRITERS 4
#define STRESS_READERS 3
#define STRESS_MIN_LAPS 24
#define STRESS_MAX_LAPS 4000
#define STRESS_QUOTA 5000
static _Atomic uint64_t stress_progress; /* newest frame time the leading writer has started */
static _Atomic int stress_readers_done;
static _Atomic uint64_t stress_reads_ok, stress_reads_silent;

static uint32_t stress_l(uint64_t t, uint32_t w) { return (uint32_t)((t & 0x0fffffffu) | (w << 28)); }
static uint32_t stress_r(uint64_t t, uint32_t l) { return ~l ^ (uint32_t)(t * 2654435761u); }

typedef struct {
    cp_hist_slot *ring;
    uint32_t id;
} stress_ctx;

static void *stress_writer(void *arg) {
    stress_ctx *c = arg;
    const uint64_t base = 1u << 20;
    for (uint64_t lap = 0; lap < STRESS_MAX_LAPS; ++lap) {
        for (uint64_t i = 0; i < NFRAMES; ++i) {
            const uint64_t k = lap * NFRAMES + i;
            const uint64_t t = base + k;
            const uint32_t l = stress_l(t, c->id);
            (void)cp_hist_write(c->ring, 1, t, l, stress_r(t, l)); /* contention drops, by design */
            if (c->id == 0 && (k & 63) == 0) atomic_store(&stress_progress, k);
        }
        if (lap + 1 >= STRESS_MIN_LAPS && atomic_load(&stress_readers_done) == STRESS_READERS) break;
    }
    return NULL;
}
static void *stress_reader(void *arg) {
    stress_ctx *c = arg;
    const uint64_t base = 1u << 20;
    uint64_t x = 0x2545f4914f6cdd1dull + c->id * 7919u, ok = 0;
    for (uint64_t attempts = 0; ok < STRESS_QUOTA && attempts < 400ull * 1000 * 1000; ++attempts) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
        uint64_t head = atomic_load(&stress_progress);
        uint64_t back = x % NFRAMES; /* anywhere within one ring length behind the leader, and ahead */
        uint64_t t = base + (head > back ? head - back : 0) + (x >> 40) % 64;
        uint32_t l, r;
        if (cp_hist_read(c->ring, 1, t, &l, &r)) {
            CHECK((l & 0x0fffffffu) == (uint32_t)(t & 0x0fffffffu), "a returned frame is the frame that was asked for");
            CHECK((l >> 28) < STRESS_WRITERS, "a returned frame comes from a real writer");
            CHECK(r == stress_r(t, l), "L and R of a returned frame come from the same write (no mixed frames)");
            ++ok;
            atomic_fetch_add(&stress_reads_ok, 1);
        } else {
            atomic_fetch_add(&stress_reads_silent, 1);
        }
    }
    CHECK(ok >= STRESS_QUOTA, "every stress reader collected its quota of coherent frames");
    atomic_fetch_add(&stress_readers_done, 1);
    return NULL;
}

static void test_ring_concurrent_stress(void) {
    cp_hist_slot *r = new_ring();
    stress_ctx wc[STRESS_WRITERS], rc[STRESS_READERS];
    pthread_t wt[STRESS_WRITERS], rt[STRESS_READERS];
    atomic_store(&stress_readers_done, 0);
    atomic_store(&stress_progress, 0);
    atomic_store(&stress_reads_ok, 0);
    atomic_store(&stress_reads_silent, 0);
    for (unsigned i = 0; i < STRESS_READERS; ++i) { rc[i].ring = r; rc[i].id = i; rt[i] = spawn(stress_reader, &rc[i]); }
    for (unsigned i = 0; i < STRESS_WRITERS; ++i) { wc[i].ring = r; wc[i].id = i; wt[i] = spawn(stress_writer, &wc[i]); }
    for (unsigned i = 0; i < STRESS_WRITERS; ++i) join(wt[i]);
    for (unsigned i = 0; i < STRESS_READERS; ++i) join(rt[i]);
    CHECK_EQ(atomic_load(&stress_readers_done), STRESS_READERS, "every reader finished its quota");
    /* Whatever the final state is, every published slot is a coherent, identified frame. */
    unsigned published = 0;
    for (uint64_t s = 0; s < NFRAMES; ++s) {
        uint64_t m = atomic_load(&r[s].meta);
        CHECK(!(m & CP_HIST_BUSY), "no slot is left claimed");
        if (!m) continue;
        ++published;
        CHECK((m >> 32) == 1, "generation 1 only");
        const uint64_t t = ((((uint64_t)(uint32_t)m) << CP_HIST_LAP_SHIFT) | s); /* this slot's frame time */
        uint64_t p = atomic_load(&r[s].pcm);
        uint32_t l = (uint32_t)p, rr = (uint32_t)(p >> 32);
        CHECK((l & 0x0fffffffu) == (uint32_t)(t & 0x0fffffffu) && rr == stress_r(t, l), "published slot is coherent");
    }
    CHECK(published > NFRAMES / 2, "most of the final lap is published");
    fprintf(stderr, "  stress: %llu coherent reads, %llu silent reads, %u slots published\n",
           (unsigned long long)atomic_load(&stress_reads_ok), (unsigned long long)atomic_load(&stress_reads_silent), published);
    free(r);
}

/* ---- Direct 3: generation exhaustion through the driver -------------------------------------- */
static void test_generation_exhaustion(void) {
    H = fake_host_new();
    cp_open(H);
    g_next_generation = CP_HIST_GEN_MAX; /* one generation left */
    CHECK(add_client(3, 1) == noErr && add_client(3, 2) == noErr, "register");
    CHECK(start_io(3, 1) == noErr, "the last generation can start");
    CHECK_EQ(GEN_NOW(), CP_HIST_GEN_MAX, "last generation active");
    begin(2, 91);
    CHECK(write_pattern(100000, 480) == noErr, "write at the last generation");
    uint32_t *out = words(480);
    read_range(100000, 480, 480, out);
    expect_pattern(out, 100000, 480, "the last generation carries data");
    CHECK(stop_io(3, 1) == noErr, "stop");
    const uint32_t before = notes(H, 3);
    CHECK(start_io(3, 1) == kAudioHardwareUnspecifiedError, "exhaustion refuses to start a new transfer");
    CHECK(start_io(3, 2) == kAudioHardwareUnspecifiedError, "...for every client");
    CHECK(!running(3) && g_clients[1].started == 0, "a refused start leaves no running state or fabricated client");
    CHECK_EQ(notes(H, 3), before, "a refused start does not notify");
    CHECK(cp_gate_is_closed(&g_xfer), "no transfer");
    CHECK_EQ(g_next_generation, (uint32_t)CP_HIST_GEN_MAX + 1, "the counter never wraps");
    CHECK(write_pattern(200000, 480) == noErr, "writes are discarded");
    read_range(200000, 480, 480, out);
    expect_silence(out, 480, "no transfer: silence");
    /* Visible demand is unaffected by hidden exhaustion. */
    CHECK(running(2), "visible demand still works");
    free(out);
    teardown();
    g_next_generation = 1; /* the counter is process-wide; later tests need fresh generations */
}

static void test_time_range_through_driver(void) {
    setup();
    /* The last addressable block: frame times up to 2^46 - 2 publish; one frame later is refused. */
    const uint64_t last_ok = CP_HIST_TIME_LIMIT - CROSSPANE_AUDIO_MAX_CALLBACK_FRAMES - 1;
    float buf[4096 * 2];
    for (unsigned k = 0; k < 4096; ++k) { buf[2 * k] = 1.5f; buf[2 * k + 1] = -2.5f; }
    const uint64_t w = last_ok - CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES; /* read input time == last_ok */
    CHECK(write_speakers(last_ok, 4096, buf) == noErr, "the last addressable write");
    CHECK(write_speakers(last_ok + 1, 4096, buf) == kAudioHardwareIllegalOperationError, "beyond the addressable range fails explicitly");
    CHECK(write_speakers(w, 4096, buf) == noErr, "a write whose read time is still addressable");
    float got[4096 * 2];
    CHECK(read_speakers(w + CROSSPANE_AUDIO_TRANSFER_DELAY_FRAMES, 4096, got) == noErr, "the last addressable read");
    CHECK(memcmp(got, buf, sizeof(got)) == 0, "the last block round-trips");
    CHECK(read_speakers(last_ok + 1, 4096, got) == kAudioHardwareIllegalOperationError, "a read beyond the range fails explicitly");
    teardown();
}

#define RUN(fn) do { fprintf(stderr, "  %s\n", #fn); fn(); } while (0)
int main(void) {
    install_fixture_hook();
    RUN(test_partitioned_roundtrip);
    RUN(test_two_simultaneous_readers);
    RUN(test_reordered_reads);
    RUN(test_expiry_underrun_overrun);
    RUN(test_random_alignment_property);
    RUN(test_microphone_stays_silent_beside_speakers);
    RUN(test_stop_restart_no_backlog);
    RUN(test_old_writer_after_failed_barrier);
    RUN(test_retired_reader_after_failed_barrier_and_restart);
    RUN(test_stop_barrier_drains_admitted_access);
    RUN(test_driver_paused_publication);
    RUN(test_driver_paused_publication_with_stop_and_restart);
    RUN(test_driver_paused_publication_with_release_to_zero);
    RUN(test_driver_overwrite_during_read);
    RUN(test_driver_failed_cas);
    RUN(test_transfer_inactive_until_hidden_start);
    RUN(test_ring_older_and_duplicate_writers);
    RUN(test_ring_version_exhaustion);
    RUN(test_ring_paused_publication);
    RUN(test_ring_overwrite_during_read);
    RUN(test_ring_failed_cas);
    RUN(test_ring_concurrent_stress);
    RUN(test_generation_exhaustion);
    RUN(test_time_range_through_driver);
    puts("PASS test_history: partitioned/reordered/simultaneous reads, expiry, under/overrun, alignment, "
         "paused+concurrent publication, stale writers, failed CAS, exhaustion, stop barrier");
    return 0;
}
