#include "Edid.h"

namespace crosspane::idd {
EdidResult generate_edid(const CPD_MODE& m, std::uint64_t serial) noexcept {
    EdidResult result{};
    if (!valid_mode(m) || serial == 0 || serial > UINT32_MAX) return result;
    auto& b = result.bytes;
    b[0] = 0; for (unsigned i = 1; i < 7; ++i) b[i] = 0xff; b[7] = 0;
    b[8] = 0x0e; b[9] = 0x04; b[10] = 1; b[11] = 0;
    for (unsigned i = 0; i < 4; ++i) b[12 + i] = static_cast<std::uint8_t>(serial >> (8 * i));
    b[16] = 0; b[17] = 36; b[18] = 1; b[19] = 3; b[20] = 0x80;
    // Base dimensions are centimeters; detailed timing preserves the full 12-bit mm.
    b[21] = static_cast<std::uint8_t>((m.physical_width_mm + 5) / 10);
    b[22] = static_cast<std::uint8_t>((m.physical_height_mm + 5) / 10);
    b[23] = 120; b[24] = 0x0e;
    constexpr std::array<std::uint16_t, 8> chromaticity{655, 338, 307, 614, 154, 61, 320, 337};
    for (unsigned i = 0; i < 8; ++i) {
        b[25 + i / 4] |= static_cast<std::uint8_t>((chromaticity[i] & 3) << (6 - 2 * (i % 4)));
        b[27 + i] = static_cast<std::uint8_t>(chromaticity[i] >> 2);
    }
    for (unsigned i = 38; i < 54; ++i) b[i] = 1; // Unused standard timings.
    const bool hd = m.width == 1280;
    result.timing = {hd ? UINT64_C(74250000) : UINT64_C(148500000), m.width, m.height,
                     hd ? 1650U : 2200U, hd ? 750U : 1125U,
                     hd ? 110U : 88U, hd ? 40U : 44U, hd ? 5U : 4U, 5};
    const auto& t = result.timing;
    const auto hb = t.total_width - t.active_width;
    const auto vb = t.total_height - t.active_height;
    const auto clock = static_cast<std::uint32_t>(t.pixel_clock_hz / 10000);
    auto* d = b.data() + 54;
    d[0] = static_cast<std::uint8_t>(clock); d[1] = static_cast<std::uint8_t>(clock >> 8);
    d[2] = static_cast<std::uint8_t>(m.width); d[3] = static_cast<std::uint8_t>(hb);
    d[4] = static_cast<std::uint8_t>(((m.width >> 8) << 4) | (hb >> 8));
    d[5] = static_cast<std::uint8_t>(m.height); d[6] = static_cast<std::uint8_t>(vb);
    d[7] = static_cast<std::uint8_t>(((m.height >> 8) << 4) | (vb >> 8));
    d[8] = static_cast<std::uint8_t>(t.horizontal_front); d[9] = static_cast<std::uint8_t>(t.horizontal_sync);
    d[10] = static_cast<std::uint8_t>((t.vertical_front << 4) | t.vertical_sync);
    d[11] = static_cast<std::uint8_t>(((t.horizontal_front >> 8) << 6) | ((t.horizontal_sync >> 8) << 4) |
                                      ((t.vertical_front >> 4) << 2) | (t.vertical_sync >> 4));
    d[12] = static_cast<std::uint8_t>(m.physical_width_mm); d[13] = static_cast<std::uint8_t>(m.physical_height_mm);
    d[14] = static_cast<std::uint8_t>(((m.physical_width_mm >> 8) << 4) | (m.physical_height_mm >> 8));
    d[17] = 0x1e; // Non-interlaced separate positive H/V sync, no border.
    for (unsigned offset : {72U, 90U, 108U}) b[offset + 3] = 0x10; // Canonical dummy descriptor.
    b[126] = 0;
    std::uint8_t sum = 0;
    for (unsigned i = 0; i < 127; ++i) sum = static_cast<std::uint8_t>(sum + b[i]);
    b[127] = static_cast<std::uint8_t>(0U - sum);
    result.decision = Decision::Accepted;
    return result;
}
bool canonical_edid(const std::array<std::uint8_t, 128>& bytes,
                    const CPD_MODE& mode, std::uint64_t serial) noexcept {
    const auto expected = generate_edid(mode, serial);
    return expected.decision == Decision::Accepted && expected.bytes == bytes;
}
} // namespace crosspane::idd
