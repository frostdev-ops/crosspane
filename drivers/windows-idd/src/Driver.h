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

enum class DevicePhase : std::uint32_t {
    Starting,
    Ready,
    Stopping,
    Stopped,
};

struct AdmissionEpoch {
    std::uint64_t generation;
};

// Private portable state only: an epoch does not prove that a WDF handle is live.
// The native adapter must establish its pre-cleanup barrier before using DDIs.
class DeviceAdmission final {
public:
    DeviceAdmission() = default;
    DeviceAdmission(const DeviceAdmission&) = delete;
    DeviceAdmission& operator=(const DeviceAdmission&) = delete;

    bool publish_ready(AdmissionEpoch starting_epoch);
    bool acquire_epoch(AdmissionEpoch& result) const;
    bool accepts(AdmissionEpoch epoch) const;
    void begin_stop();
    void finish_stop(bool effects_quiescent, bool processors_exited);
    bool restart();
    DevicePhase phase() const;
    AdmissionEpoch generation() const;

private:
    mutable std::mutex mutex_;
    DevicePhase phase_ = DevicePhase::Starting;
    std::uint64_t generation_ = 1;
    bool generation_exhausted_ = false;
};

} // namespace crosspane::idd
