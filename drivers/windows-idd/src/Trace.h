/*++

Module Name:

    Internal.h

Abstract:

    This module contains the local type definitions for the
    driver.

Environment:

    Windows User-Mode Driver Framework 2

--*/

#pragma once

#include <cstdint>

// Crosspane modification of the pinned Trace.h: sample WPP/provider and
// arbitrary formatted strings removed. Native trace emission is not bound
// until the admitted package/target inspection; this header performs no I/O.
namespace crosspane::idd {

enum class NumericTraceCode : std::uint32_t {
    AdmissionRefused = 1,
    LeaseExpired = 2,
    RetirementRequested = 3,
    NativeFailure = 4,
    QuiescenceFailure = 5,
};

struct NumericTraceRecord {
    NumericTraceCode code;
    std::int32_t status;
    std::uint32_t count;
};

} // namespace crosspane::idd
