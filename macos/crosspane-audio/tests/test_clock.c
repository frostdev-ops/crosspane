/* Direct test 4: injected-clock equality across devices, period boundaries, the SDK minimum, seed
 * semantics, hidden stop with visible continuation, and starts that never reset the shared
 * timestamps. The host clock is injected; nothing here touches the HAL or real audio. */
#include "cp_fixture.h"

static _Atomic uint64_t fake_now;
static uint64_t fake_clock(void) { return atomic_load(&fake_now); }

/* Independent oracle, written differently from the driver: exact integer floor of
 * k * PERIOD * 1e9 * denom / (RATE * numer). */
static unsigned __int128 oracle128(uint64_t k, uint32_t numer, uint32_t denom) {
    unsigned __int128 n = (unsigned __int128)k * 16384u * 1000000000u;
    n *= denom;
    return n / ((unsigned __int128)48000u * numer);
}
static uint64_t oracle_period_ticks(uint64_t k, uint32_t numer, uint32_t denom) {
    return (uint64_t)oracle128(k, numer, denom);
}

static void set_clock(uint64_t epoch, uint32_t numer, uint32_t denom) {
    g_timeline.epoch = epoch;
    g_timeline.numer = numer;
    g_timeline.denom = denom;
    g_test_clock = fake_clock;
}

typedef struct {
    Float64 sample;
    UInt64 host, seed;
} stamp;

static stamp zero_time(AudioObjectID dev) {
    stamp s = {-1, 0, 0};
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, dev, 1, &s.sample, &s.host, &s.seed) == noErr, "GetZeroTimeStamp");
    return s;
}
static bool same(stamp a, stamp b) { return a.sample == b.sample && a.host == b.host && a.seed == b.seed; }

static void check_all_devices_equal(uint64_t now, const char *what) {
    atomic_store(&fake_now, now);
    stamp ref = zero_time(2);
    for (AudioObjectID dev = 3; dev <= 5; ++dev) CHECK(same(zero_time(dev), ref), what);
}

/* The answer for `now`, from first principles: the largest period k whose start is <= now, in
 * 128 bits; refused when that period's sample time is not an exactly representable double. */
static void check_against_oracle(uint64_t epoch, uint32_t numer, uint32_t denom, uint64_t now, const char *what) {
    const uint64_t kmax = CP_TIMELINE_MAX_PERIOD;
    const unsigned __int128 dt = now > epoch ? now - epoch : 0;
    uint64_t lo = 0, hi = kmax + 1; /* oracle128 is monotonic: find the largest k <= hi with start(k) <= dt */
    while (lo < hi) {
        uint64_t mid = lo + (hi - lo + 1) / 2;
        if (oracle128(mid, numer, denom) <= dt) lo = mid;
        else hi = mid - 1;
    }
    atomic_store(&fake_now, now);
    Float64 sample = -7;
    UInt64 host = 11, seed = 13;
    OSStatus st = (*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &sample, &host, &seed);
    if (lo > kmax) {
        CHECK(st == kAudioHardwareUnspecifiedError && sample == -7 && host == 11 && seed == 13, what);
    } else {
        CHECK(st == noErr, what);
        CHECK(sample == (Float64)(lo * 16384u), what);
        CHECK(host == epoch + (uint64_t)oracle128(lo, numer, denom), what);
        CHECK(host <= (now > epoch ? now : epoch), "never in the future");
    }
}

/* Extremes: no narrowing before the comparison, nothing wraps into a bogus period. */
static void test_extremes(void) {
    const struct { uint32_t numer, denom; } bases[] = {{125, 3}, {1, 1}, {1000, 3}};
    const uint64_t epochs[] = {0, 1000000, UINT64_MAX / 2, UINT64_MAX - 5};
    for (unsigned b = 0; b < 3; ++b) {
        for (unsigned e = 0; e < 4; ++e) {
            fake_host *h = fake_host_new();
            const uint32_t n = bases[b].numer, dn = bases[b].denom;
            const uint64_t epoch = epochs[e];
            set_clock(epoch, n, dn);
            AudioServerPlugInDriverRef d = cp_open(h);
            const uint64_t kmax = CP_TIMELINE_MAX_PERIOD;
            CHECK_EQ(kmax, (1ull << 53) / 16384, "the representability limit");
            /* The finding's example: ratio 125/3, epoch 1000000, now = UINT64_MAX. */
            check_against_oracle(epoch, n, dn, UINT64_MAX, "now = UINT64_MAX");
            check_against_oracle(epoch, n, dn, UINT64_MAX - 1, "now = UINT64_MAX - 1");
            check_against_oracle(epoch, n, dn, epoch, "now = epoch");
            check_against_oracle(epoch, n, dn, epoch > 0 ? epoch - 1 : 0, "now just before the epoch");
            /* The boundary of representability, from both sides, wherever it lies in the 64-bit range. */
            for (uint64_t k = kmax - 2; k <= kmax + 2; ++k) {
                const unsigned __int128 start = oracle128(k, n, dn);
                if (start > UINT64_MAX - epoch) continue; /* beyond the 64-bit host clock anyway */
                const uint64_t t = epoch + (uint64_t)start;
                check_against_oracle(epoch, n, dn, t, "on a boundary near the limit");
                check_against_oracle(epoch, n, dn, t - 1, "just before a boundary near the limit");
                if (t < UINT64_MAX) check_against_oracle(epoch, n, dn, t + 1, "just after a boundary near the limit");
            }
            uint64_t x = 0x9e3779b97f4a7c15ull + e * 31 + b;
            for (unsigned i = 0; i < 3000; ++i) {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                check_against_oracle(epoch, n, dn, x, "a random 64-bit host time");
                check_against_oracle(epoch, n, dn, UINT64_MAX - (x % 100000000ull), "a random time near the top");
            }
            cp_close(d);
            free(h);
        }
    }
    g_test_clock = NULL;

    /* The finding, literally: the old code advanced four extra periods here. */
    fake_host *h = fake_host_new();
    set_clock(1000000, 125, 3);
    AudioServerPlugInDriverRef d = cp_open(h);
    atomic_store(&fake_now, UINT64_MAX);
    Float64 s = -7;
    UInt64 host = 11, seed = 13;
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &s, &host, &seed) == kAudioHardwareUnspecifiedError, "refused");
    CHECK(s == -7 && host == 11 && seed == 13, "and nothing is written");
    cp_close(d);
    free(h);

    /* A timebase with a sub-tick period is not usable: Initialize refuses rather than answer wrongly. */
    h = fake_host_new();
    set_clock(0, 4000000000u, 1); /* 0.08 host ticks per period */
    d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver, "factory");
    CHECK((*d)->Initialize(d, &h->iface) == kAudioHardwareUnspecifiedError, "a degenerate timebase is refused");
    CHECK((*d)->Release(d) == 0, "release");
    free(h);
    g_test_clock = NULL;
}

static void test_boundaries(uint32_t numer, uint32_t denom) {
    fake_host *h = fake_host_new();
    const uint64_t epoch = 1000000;
    set_clock(epoch, numer, denom);
    AudioServerPlugInDriverRef d = cp_open(h);
    const uint64_t ks[] = {0, 1, 2, 3, 7, 100, 1000, 123456, 1ull << 20, 1ull << 30};
    for (unsigned i = 0; i < sizeof(ks) / sizeof(ks[0]); ++i) {
        uint64_t k = ks[i];
        uint64_t start = epoch + oracle_period_ticks(k, numer, denom);
        /* exactly on the boundary: period k */
        atomic_store(&fake_now, start);
        stamp s = zero_time(2);
        CHECK(s.sample == (Float64)(k * 16384u), "sample time on the boundary");
        CHECK(s.host == start, "host time on the boundary");
        CHECK(s.seed == 1, "common seed");
        /* one tick before the boundary: still period k-1 (the timeline starts at 0) */
        if (k > 0 && start > epoch) {
            atomic_store(&fake_now, start - 1);
            stamp before = zero_time(2);
            CHECK(before.sample == (Float64)((k - 1) * 16384u), "one tick before the boundary");
            CHECK(before.host == epoch + oracle_period_ticks(k - 1, numer, denom), "host of the previous period");
            CHECK(before.host <= start - 1, "zero time is never in the future");
        }
        /* one tick after, and just before the next boundary: still period k */
        uint64_t next = epoch + oracle_period_ticks(k + 1, numer, denom);
        atomic_store(&fake_now, start + 1);
        CHECK(zero_time(2).sample == (Float64)(k * 16384u), "one tick after the boundary");
        atomic_store(&fake_now, next - 1);
        stamp late = zero_time(2);
        CHECK(late.sample == (Float64)(k * 16384u) && late.host == start, "just before the next boundary");
        atomic_store(&fake_now, next);
        CHECK(zero_time(2).sample == (Float64)((k + 1) * 16384u), "next boundary");
        /* equal for all four devices at every one of these times */
        check_all_devices_equal(start, "all four devices agree on the boundary");
        check_all_devices_equal(start + 12345, "all four devices agree inside a period");
    }
    /* Before the epoch the timeline is at its origin. */
    atomic_store(&fake_now, epoch - 5);
    stamp pre = zero_time(2);
    CHECK(pre.sample == 0.0 && pre.host == epoch, "before the epoch");
    cp_close(d);
    free(h);
    g_test_clock = NULL;
}

static void test_dense_oracle(void) {
    /* A dense sweep of host times against an independent oracle: host(k) <= now < host(k+1). */
    fake_host *h = fake_host_new();
    const uint32_t numer = 125, denom = 3;
    const uint64_t epoch = 77;
    set_clock(epoch, numer, denom);
    AudioServerPlugInDriverRef d = cp_open(h);
    uint64_t x = 88172645463325252ull;
    for (unsigned i = 0; i < 200000; ++i) {
        x ^= x << 13; x ^= x >> 7; x ^= x << 17; /* xorshift64 */
        uint64_t now = epoch + (x % (1ull << 36));
        atomic_store(&fake_now, now);
        stamp s = zero_time(i % 4 + 2);
        uint64_t k = (uint64_t)(s.sample / 16384.0);
        CHECK((Float64)(k * 16384u) == s.sample, "sample time is a whole number of periods");
        CHECK(s.host == epoch + oracle_period_ticks(k, numer, denom), "host is the exact period start");
        CHECK(s.host <= now, "host <= now");
        CHECK(now < epoch + oracle_period_ticks(k + 1, numer, denom), "now < next period start");
    }
    /* Monotonic under a sorted sweep. */
    Float64 prev = -1;
    for (uint64_t now = epoch; now < epoch + 4 * 8192000ull; now += 99991) {
        atomic_store(&fake_now, now);
        stamp s = zero_time(3);
        CHECK(s.sample >= prev, "monotonic sweep");
        prev = s.sample;
    }
    cp_close(d);
    free(h);
    g_test_clock = NULL;
}

static void test_period_independent_of_buffers(void) {
    /* The timeline period is a property of the clock: it does not follow callback sizes. */
    fake_host *h = fake_host_new();
    set_clock(500, 125, 3);
    AudioServerPlugInDriverRef d = cp_open(h);
    CHECK_EQ(u32(2, kAudioDevicePropertyZeroTimeStampPeriod, kAudioObjectPropertyScopeGlobal), 16384, "period");
    atomic_store(&fake_now, 500 + 3 * 8192000ull + 17);
    stamp before = zero_time(2);
    CHECK_EQ((long long)before.sample, 3 * 16384, "three periods in");
    float buf[4096 * 2];
    const UInt32 sizes[] = {1, 480, 1024, 4096};
    CHECK(add_client(2, 1) == noErr && start_io(2, 1) == noErr, "start visible");
    for (unsigned i = 0; i < 4; ++i) {
        memset(buf, 0, sizeof(buf));
        CHECK(write_speakers(100000, sizes[i], buf) == noErr, "writes of every size");
        CHECK(same(zero_time(2), before), "buffer sizes never move the clock");
    }
    CHECK(stop_io(2, 1) == noErr && remove_client(2, 1) == noErr, "stop");
    cp_close(d);
    free(h);
    g_test_clock = NULL;
}

static void test_hidden_stop_visible_continues(void) {
    fake_host *h = fake_host_new();
    const uint64_t epoch = 4000;
    set_clock(epoch, 125, 3);
    AudioServerPlugInDriverRef d = cp_open(h);
    const uint64_t t1 = epoch + 5 * 8192000ull + 100, t2 = epoch + 9 * 8192000ull + 5;

    /* Starts, stops and restarts, in every pairing, never move the shared time stamps. */
    stamp a1[4], a2[4];
    for (unsigned i = 0; i < 4; ++i) {
        atomic_store(&fake_now, t1);
        a1[i] = zero_time((AudioObjectID)(i + 2));
        atomic_store(&fake_now, t2);
        a2[i] = zero_time((AudioObjectID)(i + 2));
    }
    CHECK(a1[0].sample == 5 * 16384.0 && a2[0].sample == 9 * 16384.0, "expected periods");
    for (unsigned i = 1; i < 4; ++i) CHECK(same(a1[i], a1[0]) && same(a2[i], a2[0]), "pairs equal");

    begin(2, 1);
    begin(3, 2);
    begin(4, 3);
    begin(5, 4);
    atomic_store(&fake_now, t1);
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "starts do not reset the clock");
    CHECK(stop_io(3, 2) == noErr, "hidden speakers stops");
    CHECK(stop_io(5, 4) == noErr, "hidden mic stops");
    CHECK(!running(3) && !running(5) && running(2) && running(4), "hidden stopped, visible running");
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "hidden stop does not move the clock");
    atomic_store(&fake_now, t2);
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a2[dev - 2]), "visible clocks keep progressing");
    CHECK(start_io(3, 2) == noErr && start_io(5, 4) == noErr, "hidden restart");
    atomic_store(&fake_now, t1);
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "restart does not reset the clock");
    atomic_store(&fake_now, t2);
    CHECK(stop_io(2, 1) == noErr && stop_io(4, 3) == noErr && stop_io(3, 2) == noErr && stop_io(5, 4) == noErr, "stop all");
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a2[dev - 2]), "all stopped, clock unchanged");
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(remove_client(dev, dev - 1) == noErr, "remove");

    /* The timeline is process-wide and immutable: a Release to zero, a new Factory call and even
     * the test-only state reset leave the clock epoch alone. */
    CHECK((*d)->Release(d) == 0, "Release to zero");
    atomic_store(&fake_now, t1);
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "Release to zero does not reset the clock epoch");
    d = CrosspaneAudioFactory(NULL, kAudioServerPlugInTypeUUID);
    CHECK(d == g_driver, "the factory hands out the same singleton");
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "a new Factory call does not reset it either");
    cp_close(d);
    d = cp_open(h);
    atomic_store(&fake_now, t1);
    for (AudioObjectID dev = 2; dev <= 5; ++dev) CHECK(same(zero_time(dev), a1[dev - 2]), "nor does re-creating the state (test-only reset)");
    cp_close(d);
    free(h);
    g_test_clock = NULL;
}

static void test_malformed_and_closed(void) {
    fake_host *h = fake_host_new();
    set_clock(10, 1, 1);
    AudioServerPlugInDriverRef d = cp_open(h);
    Float64 s;
    UInt64 host, seed;
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, NULL, &host, &seed) == kAudioHardwareIllegalOperationError, "NULL sample");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &s, NULL, &seed) == kAudioHardwareIllegalOperationError, "NULL host");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &s, &host, NULL) == kAudioHardwareIllegalOperationError, "NULL seed");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 1, 1, &s, &host, &seed) == kAudioHardwareBadObjectError, "plug-in is not a device");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 6, 1, &s, &host, &seed) == kAudioHardwareBadObjectError, "stream is not a device");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 99, 1, &s, &host, &seed) == kAudioHardwareBadObjectError, "unknown device");
    CHECK((*d)->Release(d) == 0, "Release to zero");
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &s, &host, &seed) == noErr, "still answers after a Release to zero: the singleton is never closed");
    cp_test_reset(); /* back to the state before the first Initialize */
    CHECK((*g_driver)->GetZeroTimeStamp(g_driver, 2, 1, &s, &host, &seed) == kAudioHardwareNotRunningError, "not running before the first Initialize");
    free(h);
    g_test_clock = NULL;
}

static void test_real_clock(void) {
    /* No injection: the real mach clock, one call each side of a short sleep. */
    fake_host *h = fake_host_new();
    cp_load(); /* restore the real timeline (the tests above replaced it) */
    g_test_clock = NULL;
    AudioServerPlugInDriverRef d = cp_open(h);
    stamp a = zero_time(2);
    const uint64_t now = mach_absolute_time();
    CHECK(a.host <= now && a.sample >= 0 && fmod(a.sample, 16384.0) == 0.0, "real zero time is in the past and aligned");
    for (AudioObjectID dev = 2; dev <= 5; ++dev) {
        stamp x = zero_time(dev);
        CHECK(x.seed == 1 && x.sample >= a.sample, "real clock, common seed");
    }
    sleep_ms(5);
    stamp b = zero_time(3);
    CHECK(b.sample >= a.sample && b.host >= a.host, "real clock is monotonic");
    cp_close(d);
    free(h);
}

int main(void) {
    test_boundaries(125, 3); /* Apple silicon: 24 MHz ticks */
    test_boundaries(1, 1);   /* 1 ns ticks: the period is not a whole number of ticks */
    test_boundaries(1000, 3); /* an exotic ratio: about a million ticks per period */
    test_dense_oracle();
    test_extremes();
    test_period_independent_of_buffers();
    test_hidden_stop_visible_continues();
    test_malformed_and_closed();
    test_real_clock();
    puts("PASS test_clock: shared timeline equal across pairs, period boundaries/minimum/seed, hidden stop + visible continuation, no reset");
    return 0;
}
