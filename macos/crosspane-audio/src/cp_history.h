/* Bounded, timestamp-addressed, non-destructive stereo history (WP-3.4).
 *
 * 16384 slots; slot index = frame time T mod 16384. Each slot holds ONE complete stereo frame:
 *
 *   pcm  : 64-bit atomic = L float32 bits | R float32 bits << 32   (one atomic store/load, so L and
 *          R can never tear apart from each other)
 *   meta : 64-bit atomic = [63] busy | [62..32] transfer generation | [31..0] lap (T >> 14)
 *
 * The "identity" of a frame is (generation, lap): together with the slot index it names the exact
 * frame time and the exact transfer it was written in. Everything is a sequentially consistent
 * C11 atomic, so there is no data race anywhere (no plain float copy, no sequence-number retry).
 *
 * Writer (bounded, wait-free, never retries):
 *   1. load meta; busy -> drop (contended);
 *   2. identity >= ours (duplicate, older lap, older generation) -> drop (stale): a published
 *      frame is immutable, so every reader of a retained frame gets identical bits;
 *   3. ONE CAS(meta: m -> m|busy); failure -> drop (contended). The slot is now exclusively ours;
 *   4. store pcm, then store meta = our identity (publication).
 * Reader (bounded, wait-free, never steals a sample):
 *   meta must equal the expected identity exactly (this also rejects busy); load pcm; meta must
 *   still be the same, else the frame is not returned (silence). A frame overwritten between the
 *   two meta loads can only be seen as busy or as a strictly newer identity, never as itself.
 *
 * Silence is returned for anything missing, expired, unpublished, torn, from another generation
 * or out of range. Restart = a new generation number: nothing in the ring is ever reset in place,
 * and frames of older generations are simply unreadable and unable to overwrite newer frames.
 */
#ifndef CP_HISTORY_H
#define CP_HISTORY_H

#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>

#include "crosspane_audio.h"

#include "cp_hooks.h"

_Static_assert((CROSSPANE_AUDIO_HISTORY_FRAMES & (CROSSPANE_AUDIO_HISTORY_FRAMES - 1)) == 0,
               "history length must be a power of two");
_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2, "audio-thread atomics must be lock-free");

#define CP_HIST_MASK ((uint64_t)CROSSPANE_AUDIO_HISTORY_FRAMES - 1)
#define CP_HIST_LAP_SHIFT 14 /* log2(history frames) */
_Static_assert((1u << CP_HIST_LAP_SHIFT) == CROSSPANE_AUDIO_HISTORY_FRAMES, "lap shift");
#define CP_HIST_BUSY (1ull << 63)
#define CP_HIST_GEN_MAX 0x7fffffffu /* generation is 31 bits; 0 means "empty slot" */
/* A 32-bit lap addresses 2^46 frames (about 46 years at 48 kHz); beyond that is out of range. */
#define CP_HIST_TIME_LIMIT (1ull << 46)

typedef struct {
    _Atomic uint64_t meta;
    _Atomic uint64_t pcm;
} cp_hist_slot;

typedef enum {
    CP_HIST_PUBLISHED = 0,
    CP_HIST_DROP_CONTENDED = 1, /* slot busy, or lost the single CAS attempt */
    CP_HIST_DROP_STALE = 2,     /* duplicate, older lap or older generation */
    CP_HIST_DROP_RANGE = 3      /* generation 0/exhausted or time beyond the addressable range */
} cp_hist_result;

static inline uint64_t cp_hist_key(uint32_t gen, uint64_t t) {
    return ((uint64_t)gen << 32) | (t >> CP_HIST_LAP_SHIFT);
}

static inline uint64_t cp_hist_pack(uint32_t l_bits, uint32_t r_bits) {
    return (uint64_t)l_bits | ((uint64_t)r_bits << 32);
}

static inline cp_hist_result cp_hist_write(cp_hist_slot *slots, uint32_t gen, uint64_t t,
                                           uint32_t l_bits, uint32_t r_bits) {
    if (gen == 0 || gen > CP_HIST_GEN_MAX || t >= CP_HIST_TIME_LIMIT) return CP_HIST_DROP_RANGE;
    cp_hist_slot *s = &slots[t & CP_HIST_MASK];
    uint64_t key = cp_hist_key(gen, t);
    uint64_t m = atomic_load_explicit(&s->meta, memory_order_seq_cst);
    if (m & CP_HIST_BUSY) return CP_HIST_DROP_CONTENDED;
    if (m >= key) return CP_HIST_DROP_STALE;
    CP_TEST_HOOK(CP_HOOK_WRITE_BEFORE_CLAIM, t);
    /* Single attempt: bounded admission; a contended write is dropped, never waited for. */
    if (!atomic_compare_exchange_strong_explicit(&s->meta, &m, m | CP_HIST_BUSY,
                                                 memory_order_seq_cst, memory_order_seq_cst))
        return CP_HIST_DROP_CONTENDED;
    CP_TEST_HOOK(CP_HOOK_WRITE_CLAIMED, t);
    atomic_store_explicit(&s->pcm, cp_hist_pack(l_bits, r_bits), memory_order_seq_cst);
    atomic_store_explicit(&s->meta, key, memory_order_seq_cst); /* publication */
    return CP_HIST_PUBLISHED;
}

static inline bool cp_hist_read(const cp_hist_slot *slots, uint32_t gen, uint64_t t,
                                uint32_t *l_bits, uint32_t *r_bits) {
    if (gen == 0 || gen > CP_HIST_GEN_MAX || t >= CP_HIST_TIME_LIMIT) return false;
    const cp_hist_slot *s = &slots[t & CP_HIST_MASK];
    uint64_t key = cp_hist_key(gen, t);
    if (atomic_load_explicit(&s->meta, memory_order_seq_cst) != key) return false;
    uint64_t p = atomic_load_explicit(&s->pcm, memory_order_seq_cst);
    CP_TEST_HOOK(CP_HOOK_READ_PAYLOAD_LOADED, t);
    if (atomic_load_explicit(&s->meta, memory_order_seq_cst) != key) return false;
    *l_bits = (uint32_t)p;
    *r_bits = (uint32_t)(p >> 32);
    return true;
}

#endif
