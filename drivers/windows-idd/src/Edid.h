// Canonical own descriptor, not an imported EDID or an ownership proof.
#pragma once
#include "Lease.h"
#include <array>

namespace crosspane::idd {
struct SignalTiming {
    std::uint64_t pixel_clock_hz{};
    std::uint32_t active_width{}, active_height{}, total_width{}, total_height{};
    std::uint32_t horizontal_front{}, horizontal_sync{}, vertical_front{}, vertical_sync{};
};
struct EdidResult {
    Decision decision{Decision::Invalid};
    std::array<std::uint8_t, 128> bytes{};
    SignalTiming timing{};
};
// Serial must be the already-reserved monotonic monitor ID; no wrapping/truncation.
EdidResult generate_edid(const CPD_MODE& mode, std::uint64_t serial) noexcept;
bool canonical_edid(const std::array<std::uint8_t, 128>& bytes,
                    const CPD_MODE& mode, std::uint64_t serial) noexcept;
} // namespace crosspane::idd
