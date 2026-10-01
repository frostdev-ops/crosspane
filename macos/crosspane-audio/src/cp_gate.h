/* Admission gate: lock-free, wait-free admission counter with a close bit and a 31-bit tag.
 *
 * One 64-bit atomic word: [63] closed | [62..32] tag (transfer generation) | [31..0] in-flight.
 *
 *   enter  : fetch_add(1); admitted only if the word was not closed (the tag is read from the same
 *            atomic snapshot, so the tag and admission can never disagree).
 *   leave  : fetch_sub(1).
 *   close  : fetch_or(closed). New entries are refused from this point on (control plane only).
 *   drain  : poll until the in-flight count reaches zero, bounded by a timeout (control plane
 *            only; the callbacks themselves never wait for anything).
 *   open   : CAS loop installing a new tag and clearing the closed bit (control plane only).
 *
 * Every operation used by an audio callback (enter/leave/still) is a single atomic RMW or load,
 * so the callbacks allocate nothing, lock nothing and have bounded work.
 */
#ifndef CP_GATE_H
#define CP_GATE_H

#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <time.h>

#include "cp_hooks.h"

_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2, "audio-thread atomics must be lock-free");

#define CP_GATE_CLOSED (1ull << 63)
#define CP_GATE_COUNT_MASK 0xffffffffull
#define CP_GATE_TAG_SHIFT 32
#define CP_GATE_TAG_MASK 0x7fffffffull
#define CP_GATE_MAX_INFLIGHT 0x00ffffffull /* far above any thread count; refuses, never wraps */

typedef struct {
    _Atomic uint64_t word;
} cp_gate;

static inline uint32_t cp_gate_tag_of(uint64_t word) {
    return (uint32_t)((word >> CP_GATE_TAG_SHIFT) & CP_GATE_TAG_MASK);
}

/* Callback side. Returns false (and leaves nothing admitted) when the gate is closed. */
static inline bool cp_gate_enter(cp_gate *g, uint32_t *tag) {
    uint64_t old = atomic_fetch_add_explicit(&g->word, 1, memory_order_seq_cst);
    if ((old & CP_GATE_CLOSED) || (old & CP_GATE_COUNT_MASK) >= CP_GATE_MAX_INFLIGHT) {
        atomic_fetch_sub_explicit(&g->word, 1, memory_order_seq_cst);
        return false;
    }
    if (tag) *tag = cp_gate_tag_of(old);
    return true;
}

static inline void cp_gate_leave(cp_gate *g) {
    atomic_fetch_sub_explicit(&g->word, 1, memory_order_seq_cst);
}

/* Callback side: still open, and still the tag this callback was admitted under? */
static inline bool cp_gate_still(const cp_gate *g, uint32_t tag) {
    uint64_t w = atomic_load_explicit(&g->word, memory_order_seq_cst);
    return !(w & CP_GATE_CLOSED) && cp_gate_tag_of(w) == tag;
}

/* Control plane. */
static inline void cp_gate_close(cp_gate *g) {
    atomic_fetch_or_explicit(&g->word, CP_GATE_CLOSED, memory_order_seq_cst);
}

static inline bool cp_gate_is_closed(const cp_gate *g) {
    return (atomic_load_explicit(&g->word, memory_order_seq_cst) & CP_GATE_CLOSED) != 0;
}

static inline uint32_t cp_gate_inflight(const cp_gate *g) {
    return (uint32_t)(atomic_load_explicit(&g->word, memory_order_seq_cst) & CP_GATE_COUNT_MASK);
}

/* Install `tag` and clear the closed bit, preserving the in-flight count. */
static inline void cp_gate_open(cp_gate *g, uint32_t tag) {
    uint64_t old = atomic_load_explicit(&g->word, memory_order_seq_cst);
    uint64_t next;
    do {
        next = (old & CP_GATE_COUNT_MASK) | ((uint64_t)(tag & CP_GATE_TAG_MASK) << CP_GATE_TAG_SHIFT);
    } while (!atomic_compare_exchange_weak_explicit(&g->word, &old, next, memory_order_seq_cst,
                                                    memory_order_seq_cst));
}

static inline uint64_t cp_now_ns(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC_RAW, &ts) != 0) return 0;
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* Wait (control plane only) until nothing is in flight. Returns false on timeout: the caller must
 * then NOT claim a successful barrier and must not release storage the stragglers could touch.
 * Bounded twice over: by the monotonic clock and by an iteration count, so a failing clock cannot
 * turn the wait into an unbounded one. */
#define CP_GATE_NAP_NS 20000ull /* 20 microseconds */
static inline bool cp_gate_drain(const cp_gate *g, uint64_t timeout_ns) {
    const uint64_t start = cp_now_ns();
    const uint64_t max_naps = timeout_ns / CP_GATE_NAP_NS + 1;
    for (uint64_t naps = 0;; ++naps) {
        if (cp_gate_inflight(g) == 0) return true;
        if (naps >= max_naps || cp_now_ns() - start >= timeout_ns) return cp_gate_inflight(g) == 0;
        CP_TEST_HOOK(CP_HOOK_DRAIN_NAP, cp_gate_inflight(g));
        struct timespec nap = {0, (long)CP_GATE_NAP_NS};
        nanosleep(&nap, NULL);
    }
}

#endif
