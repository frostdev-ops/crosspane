/*++

Copyright (c) Microsoft Corporation

Abstract:

    Crosspane private IDD admission/lifetime core and guarded native adapter.
    Retains the Microsoft notice from the admitted MIT VirtualDrivers fork.
    Source/static review is separate from unperformed native activation.

Environment:

    User Mode, UMDF; portable model remains OS-free

--*/

#pragma once

#include <cstdint>
#include <mutex>

namespace crosspane::idd {

struct LifetimeSnapshot {
    std::uint32_t effects_in_flight;
    std::uint32_t callbacks_in_flight;
    bool admission_closed;
    bool framework_cleanup_started;
    bool processor_started;
    bool processor_exit_observed;
    bool protocol_failure;
};

// Bounded bookkeeping for a private monitor state, not a WDF/IddCx handle guard.
// Native code cannot use these counters as permission for post-cleanup DDIs.
// Actual callback ordering and processor/module quiescence are required first.
class MonitorLifetime final {
public:
    MonitorLifetime() = default;
    MonitorLifetime(const MonitorLifetime&) = delete;
    MonitorLifetime& operator=(const MonitorLifetime&) = delete;

    bool begin_effect();
    void end_effect();
    bool begin_callback();
    void end_callback();
    void close_admission();
    bool note_framework_cleanup();
    bool note_processor_started();
    bool note_processor_exit_observed();
    bool quiescent() const;
    LifetimeSnapshot snapshot() const;

private:
    bool quiescent_locked() const;
    static constexpr std::uint32_t MaxInFlight = 16;
    mutable std::mutex mutex_;
    std::uint32_t effects_in_flight_ = 0;
    std::uint32_t callbacks_in_flight_ = 0;
    bool admission_closed_ = false;
    bool framework_cleanup_started_ = false;
    bool processor_started_ = false;
    bool processor_exit_observed_ = false;
    bool protocol_failure_ = false;
};

} // namespace crosspane::idd

#if defined(_WIN32)
#include "Control.h"
#include "Edid.h"
#include <atomic>
#include <memory>

namespace crosspane::idd {
class FrameProcessor;
class NativeMonitor final {
public:
    NativeMonitor(NativeDevice& owner, MonitorToken identity, const CPD_MODE& mode,
                  const EdidResult& description);
    ~NativeMonitor();
    NativeMonitor(const NativeMonitor&) = delete;
    NativeMonitor& operator=(const NativeMonitor&) = delete;
    NativeDevice& owner;
    const MonitorToken identity;
    const CPD_MODE mode;
    const EdidResult description;
    IDDCX_MONITOR handle{};
    bool assign(const IDARG_IN_SETSWAPCHAIN& swapchain);
    void unassign();
    void close_processor_admission();
    bool processor_absent() const;
private:
    mutable std::mutex processor_lock_;
    bool departing_{};
    std::unique_ptr<FrameProcessor> processor_;
};
struct MonitorContext { NativeMonitor* value; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(MonitorContext, monitor_context);
} // namespace crosspane::idd
#endif
