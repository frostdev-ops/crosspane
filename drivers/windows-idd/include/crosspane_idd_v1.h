/* Crosspane IDD v1: fixed little-endian wire ABI. No native handles or pointers. */
#ifndef CROSSPANE_IDD_V1_H
#define CROSSPANE_IDD_V1_H

#include <stddef.h>
#include <stdint.h>

#if defined(__cplusplus)
#define CPD_ALIGN8 alignas(8)
#define CPD_ASSERT(c) static_assert(c, #c)
#elif defined(_MSC_VER)
#define CPD_ALIGN8 __declspec(align(8))
#define CPD_ASSERT(c) static_assert(c, #c)
#else
#define CPD_ALIGN8 __attribute__((aligned(8)))
#define CPD_ASSERT(c) _Static_assert(c, #c)
#endif

#define CPD_MAGIC UINT32_C(0x31445043)
#define CPD_MAJOR UINT16_C(1)
#define CPD_MINOR UINT16_C(0)
#define CPD_DEVICE_TYPE UINT32_C(0x8337)
/* Portable CTL_CODE arithmetic: METHOD_BUFFERED=0; READ=1; WRITE=2. */
#define CPD_IOCTL_ADD ((CPD_DEVICE_TYPE << 16) | (UINT32_C(3) << 14) | (UINT32_C(0x800) << 2))
#define CPD_IOCTL_REMOVE ((CPD_DEVICE_TYPE << 16) | (UINT32_C(3) << 14) | (UINT32_C(0x801) << 2))
#define CPD_IOCTL_LIST ((CPD_DEVICE_TYPE << 16) | (UINT32_C(1) << 14) | (UINT32_C(0x802) << 2))
#define CPD_IOCTL_HEARTBEAT ((CPD_DEVICE_TYPE << 16) | (UINT32_C(3) << 14) | (UINT32_C(0x803) << 2))
#define CPD_MONITOR_ACTIVE UINT32_C(1)
#define CPD_MONITOR_RETIRED UINT32_C(2)
#define CPD_MAX_OPENS UINT32_C(4)
#define CPD_MAX_MONITORS UINT32_C(4)
#define CPD_MONITORS_PER_OPEN UINT32_C(1)
#define CPD_MAX_COMMANDS UINT32_C(16)
#define CPD_LEASE_MS UINT32_C(5000)
#define CPD_HEARTBEAT_INTERVAL_MS UINT32_C(1000)
#define CPD_EXPIRY_SCAN_MS UINT32_C(100)

#pragma pack(push, 8)
typedef struct CPD_ALIGN8 CPD_HEADER {
    uint32_t magic;
    uint16_t major;
    uint16_t minor;
    uint32_t struct_bytes;
    uint32_t flags;
    uint64_t request_id;
    uint64_t reserved;
} CPD_HEADER;

typedef struct CPD_ALIGN8 CPD_MODE {
    uint32_t width;
    uint32_t height;
    uint32_t refresh_numerator;
    uint32_t refresh_denominator;
    uint32_t physical_width_mm;
    uint32_t physical_height_mm;
    uint32_t bits_per_pixel;
    uint32_t reserved;
} CPD_MODE;

typedef struct CPD_ALIGN8 CPD_ADD_REQUEST { CPD_HEADER header; CPD_MODE mode; } CPD_ADD_REQUEST;
typedef struct CPD_ALIGN8 CPD_ADD_RESPONSE {
    CPD_HEADER header; uint64_t monitor_id; uint32_t remaining_ms; uint32_t reserved;
} CPD_ADD_RESPONSE;
typedef struct CPD_ALIGN8 CPD_REMOVE_REQUEST {
    CPD_HEADER header; uint64_t monitor_id; uint64_t reserved;
} CPD_REMOVE_REQUEST;
typedef struct CPD_ALIGN8 CPD_REMOVE_RESPONSE {
    CPD_HEADER header; uint64_t monitor_id; uint32_t state; uint32_t reserved;
} CPD_REMOVE_RESPONSE;
typedef struct CPD_ALIGN8 CPD_LIST_REQUEST { CPD_HEADER header; } CPD_LIST_REQUEST;
typedef struct CPD_ALIGN8 CPD_LIST_ENTRY {
    uint64_t monitor_id; CPD_MODE mode; uint32_t state; uint32_t reserved;
} CPD_LIST_ENTRY;
typedef struct CPD_ALIGN8 CPD_LIST_RESPONSE {
    CPD_HEADER header;
    uint32_t count;
    uint32_t capacity;
    uint32_t remaining_ms;
    uint32_t reserved;
    CPD_LIST_ENTRY entry;
} CPD_LIST_RESPONSE;
typedef struct CPD_ALIGN8 CPD_HEARTBEAT_REQUEST {
    CPD_HEADER header; uint64_t sequence; uint64_t reserved;
} CPD_HEARTBEAT_REQUEST;
typedef struct CPD_ALIGN8 CPD_HEARTBEAT_RESPONSE {
    CPD_HEADER header; uint32_t remaining_ms; uint32_t active_count; uint64_t accepted_sequence;
} CPD_HEARTBEAT_RESPONSE;
#pragma pack(pop)

CPD_ASSERT(sizeof(CPD_HEADER) == 32);
CPD_ASSERT(offsetof(CPD_HEADER, magic) == 0);
CPD_ASSERT(offsetof(CPD_HEADER, major) == 4);
CPD_ASSERT(offsetof(CPD_HEADER, minor) == 6);
CPD_ASSERT(offsetof(CPD_HEADER, struct_bytes) == 8);
CPD_ASSERT(offsetof(CPD_HEADER, flags) == 12);
CPD_ASSERT(offsetof(CPD_HEADER, request_id) == 16);
CPD_ASSERT(offsetof(CPD_HEADER, reserved) == 24);
CPD_ASSERT(sizeof(CPD_MODE) == 32);
CPD_ASSERT(offsetof(CPD_MODE, width) == 0);
CPD_ASSERT(offsetof(CPD_MODE, height) == 4);
CPD_ASSERT(offsetof(CPD_MODE, refresh_numerator) == 8);
CPD_ASSERT(offsetof(CPD_MODE, refresh_denominator) == 12);
CPD_ASSERT(offsetof(CPD_MODE, physical_width_mm) == 16);
CPD_ASSERT(offsetof(CPD_MODE, physical_height_mm) == 20);
CPD_ASSERT(offsetof(CPD_MODE, bits_per_pixel) == 24);
CPD_ASSERT(offsetof(CPD_MODE, reserved) == 28);
CPD_ASSERT(sizeof(CPD_ADD_REQUEST) == 64);
CPD_ASSERT(offsetof(CPD_ADD_REQUEST, mode) == 32);
CPD_ASSERT(sizeof(CPD_ADD_RESPONSE) == 48);
CPD_ASSERT(offsetof(CPD_ADD_RESPONSE, monitor_id) == 32);
CPD_ASSERT(offsetof(CPD_ADD_RESPONSE, remaining_ms) == 40);
CPD_ASSERT(offsetof(CPD_ADD_RESPONSE, reserved) == 44);
CPD_ASSERT(sizeof(CPD_REMOVE_REQUEST) == 48);
CPD_ASSERT(offsetof(CPD_REMOVE_REQUEST, monitor_id) == 32);
CPD_ASSERT(offsetof(CPD_REMOVE_REQUEST, reserved) == 40);
CPD_ASSERT(sizeof(CPD_REMOVE_RESPONSE) == 48);
CPD_ASSERT(offsetof(CPD_REMOVE_RESPONSE, monitor_id) == 32);
CPD_ASSERT(offsetof(CPD_REMOVE_RESPONSE, state) == 40);
CPD_ASSERT(offsetof(CPD_REMOVE_RESPONSE, reserved) == 44);
CPD_ASSERT(sizeof(CPD_LIST_REQUEST) == 32);
CPD_ASSERT(sizeof(CPD_LIST_ENTRY) == 48);
CPD_ASSERT(offsetof(CPD_LIST_ENTRY, monitor_id) == 0);
CPD_ASSERT(offsetof(CPD_LIST_ENTRY, mode) == 8);
CPD_ASSERT(offsetof(CPD_LIST_ENTRY, state) == 40);
CPD_ASSERT(offsetof(CPD_LIST_ENTRY, reserved) == 44);
CPD_ASSERT(sizeof(CPD_LIST_RESPONSE) == 96);
CPD_ASSERT(offsetof(CPD_LIST_RESPONSE, count) == 32);
CPD_ASSERT(offsetof(CPD_LIST_RESPONSE, capacity) == 36);
CPD_ASSERT(offsetof(CPD_LIST_RESPONSE, remaining_ms) == 40);
CPD_ASSERT(offsetof(CPD_LIST_RESPONSE, reserved) == 44);
CPD_ASSERT(offsetof(CPD_LIST_RESPONSE, entry) == 48);
CPD_ASSERT(sizeof(CPD_HEARTBEAT_REQUEST) == 48);
CPD_ASSERT(offsetof(CPD_HEARTBEAT_REQUEST, sequence) == 32);
CPD_ASSERT(offsetof(CPD_HEARTBEAT_REQUEST, reserved) == 40);
CPD_ASSERT(sizeof(CPD_HEARTBEAT_RESPONSE) == 48);
CPD_ASSERT(offsetof(CPD_HEARTBEAT_RESPONSE, remaining_ms) == 32);
CPD_ASSERT(offsetof(CPD_HEARTBEAT_RESPONSE, active_count) == 36);
CPD_ASSERT(offsetof(CPD_HEARTBEAT_RESPONSE, accepted_sequence) == 40);
CPD_ASSERT(CPD_IOCTL_ADD == UINT32_C(0x8337e000));
CPD_ASSERT(CPD_IOCTL_REMOVE == UINT32_C(0x8337e004));
CPD_ASSERT(CPD_IOCTL_LIST == UINT32_C(0x83376008));
CPD_ASSERT(CPD_IOCTL_HEARTBEAT == UINT32_C(0x8337e00c));
#if defined(__cplusplus)
CPD_ASSERT(alignof(CPD_HEADER) == 8);
CPD_ASSERT(alignof(CPD_MODE) == 8);
CPD_ASSERT(alignof(CPD_LIST_RESPONSE) == 8);
#endif

#undef CPD_ALIGN8
#undef CPD_ASSERT
#endif
