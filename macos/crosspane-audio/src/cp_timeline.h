/* Shared immutable 48 kHz software timeline (WP-3.4 clock rules).
 *
 * All four devices answer GetZeroTimeStamp from the same pure function of (epoch, host time):
 * equal supplied host times give equal (sample time, host time, seed) on every device, whatever
 * the IO state. The timeline has a fixed period of CROSSPANE_AUDIO_TIMESTAMP_PERIOD frames,
 * independent of any callback or codec buffer size, and an epoch that is never reset.
 *
 * Period k starts at host tick  epoch + floor(k * PERIOD * 1e9 * denom / (RATE * numer))
 * (denom/numer is the mach timebase: ns = ticks * numer / denom), computed exactly in 128-bit
 * integer arithmetic. EVERY comparison is made in 128 bits and representability is checked before
 * anything is narrowed: the sample time must be an exactly representable double (at most 2^53
 * frames, thousands of years), and the host time of the answer is always <= `now`, so it cannot
 * overflow. A time beyond the representable range is refused, never wrapped.
 */
#ifndef CP_TIMELINE_H
#define CP_TIMELINE_H

#include <stdbool.h>
#include <stdint.h>

#include "crosspane_audio.h"

#define CP_TIMELINE_SEED 1ull /* common to all devices; constant for the life of the process */
/* Largest period index whose sample time (k * PERIOD) is an exactly representable double. */
#define CP_TIMELINE_MAX_PERIOD ((1ull << 53) / CROSSPANE_AUDIO_TIMESTAMP_PERIOD)

typedef struct {
    uint64_t epoch; /* mach_absolute_time() ticks at timeline start */
    uint32_t numer; /* mach_timebase_info: ns = ticks * numer / denom */
    uint32_t denom;
} cp_timeline;

/* Ticks from the epoch to the start of period k, never narrowed. */
static inline unsigned __int128 cp_timeline_period_ticks(const cp_timeline *t, uint64_t k) {
    unsigned __int128 num = (unsigned __int128)k * CROSSPANE_AUDIO_TIMESTAMP_PERIOD *
                            1000000000ull * t->denom;
    unsigned __int128 den = (unsigned __int128)CROSSPANE_AUDIO_RATE * t->numer;
    return num / den;
}

/* A usable timebase has a non-degenerate ratio and a period of at least two host ticks (every
 * real mach timebase has millions), which keeps cp_timeline_zero's settle loops to one step. */
static inline bool cp_timeline_valid(const cp_timeline *t) {
    return t->numer != 0 && t->denom != 0 && cp_timeline_period_ticks(t, 1) >= 2;
}

/* The most recent zero time stamp at host time `now`. Before the epoch the timeline is at 0.
 * Returns false (and writes nothing) if the answer is not representable. */
static inline bool cp_timeline_zero(const cp_timeline *t, uint64_t now, double *sample,
                                    uint64_t *host, uint64_t *seed) {
    const uint64_t dt = now > t->epoch ? now - t->epoch : 0;
    unsigned __int128 num = (unsigned __int128)dt * CROSSPANE_AUDIO_RATE * t->numer;
    unsigned __int128 den = (unsigned __int128)CROSSPANE_AUDIO_TIMESTAMP_PERIOD * 1000000000ull *
                            t->denom;
    const unsigned __int128 estimate = num / den;
    /* The estimate is within one period of the exact answer, so anything this far out is beyond
     * the representable range whatever the exact value is. Check before narrowing. */
    if (estimate > (unsigned __int128)CP_TIMELINE_MAX_PERIOD + 1) return false;
    uint64_t k = (uint64_t)estimate;
    /* Settle against the exact tick boundaries so host(k) <= now < host(k+1) always holds, all in
     * 128 bits. Bounded: at most two steps for a valid timeline; the cap only guards the
     * real-time callback against a corrupt one. */
    for (int i = 0; i < 4 && cp_timeline_period_ticks(t, k + 1) <= dt; ++i) ++k;
    for (int i = 0; i < 4 && k > 0 && cp_timeline_period_ticks(t, k) > dt; ++i) --k;
    if (k > CP_TIMELINE_MAX_PERIOD || cp_timeline_period_ticks(t, k) > dt) return false;
    *sample = (double)(k * (uint64_t)CROSSPANE_AUDIO_TIMESTAMP_PERIOD);
    *host = t->epoch + (uint64_t)cp_timeline_period_ticks(t, k); /* <= now: cannot overflow */
    *seed = CP_TIMELINE_SEED;
    return true;
}

#endif
