/* Test hook points. The macro CP_TEST_HOOK is defined only by the detached direct-test builds
 * (CROSSPANE_AUDIO_TESTING); the production driver compiles every hook to nothing. */
#ifndef CP_HOOKS_H
#define CP_HOOKS_H

#ifndef CP_TEST_HOOK
#define CP_TEST_HOOK(point, arg) ((void)0)
#endif

enum {
    CP_HOOK_IO_ADMITTED = 1,         /* data callback admitted; arg = device id */
    CP_HOOK_WRITE_BEFORE_CLAIM = 2,  /* writer decided to write, before its CAS; arg = frame time */
    CP_HOOK_WRITE_CLAIMED = 3,       /* writer owns the slot (busy), payload not yet stored */
    CP_HOOK_READ_PAYLOAD_LOADED = 4, /* reader loaded the payload, before re-checking meta */
    CP_HOOK_STOP_DRAINING = 6,       /* hidden StopIO closed the transfer, before draining */
    CP_HOOK_DRAIN_NAP = 8            /* the stop barrier is waiting; arg = accesses still in flight */
};

#endif
