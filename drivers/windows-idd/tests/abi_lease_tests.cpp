// Exact focused portable cases. No driver/OS/device/process/input/capture calls.
#include "../src/Lease.h"
#include "../src/Edid.h"
#include "../src/Driver.h"
#include "../src/Monitor.h"
#include <algorithm>
#include <array>
#include <cstring>
#include <iostream>
#include <limits>
#include <type_traits>
#include <vector>

using namespace crosspane::idd;
namespace {
const char* current_case = "setup";
std::uint64_t checks = 0;
struct Failure { const char* expression; int line; };
#define CHECK(expression) do { ++checks; if (!(expression)) throw Failure{#expression, __LINE__}; } while (false)
CPD_MODE mode(bool hd = true, std::uint32_t width_mm = 160, std::uint32_t height_mm = 90) {
    return {hd ? 1280U : 1920U, hd ? 720U : 1080U, 60, 1, width_mm, height_mm, 32, 0};
}
void put(std::uint8_t* p, std::uint64_t value, unsigned n) {
    for (unsigned i = 0; i < n; ++i) p[i] = static_cast<std::uint8_t>(value >> (8 * i));
}
std::vector<std::uint8_t> request(std::uint32_t ioctl, std::uint64_t id = 7) {
    const std::size_t size = ioctl == CPD_IOCTL_ADD ? 64 : ioctl == CPD_IOCTL_LIST ? 32 : 48;
    std::vector<std::uint8_t> b(size + 1, 0); b.resize(size);
    put(b.data(), CPD_MAGIC, 4); put(b.data() + 4, 1, 2); put(b.data() + 8, size, 4);
    put(b.data() + 16, id, 8);
    if (ioctl == CPD_IOCTL_ADD) {
        constexpr std::array<std::uint32_t, 8> fields{1280,720,60,1,160,90,32,0};
        for (unsigned i = 0; i < fields.size(); ++i) put(b.data() + 32 + 4 * i, fields[i], 4);
    } else if (ioctl != CPD_IOCTL_LIST) put(b.data() + 32, 1, 8);
    return b;
}
ClientToken open(LeaseCore& core, std::uint64_t now = 0) {
    const auto a = core.open(now); CHECK(a.decision == Decision::Accepted); return a.client;
}
CommandToken command(LeaseCore& core, ClientToken client, std::uint64_t now = 0) {
    const auto a = core.admit_command(client, now); CHECK(a.decision == Decision::Accepted); return a.command;
}
MonitorToken reserve(LeaseCore& core, CommandToken c, std::uint64_t now = 0) {
    const auto a = core.reserve_add(c, now, mode()); CHECK(a.decision == Decision::Accepted); return a.monitor;
}
EffectToken create(LeaseCore& core, CommandToken c, std::uint64_t now = 0) {
    const auto a = core.begin_create(c, now); CHECK(a.decision == Decision::Accepted); return a.effect;
}
MonitorToken add(LeaseCore& core, ClientToken client, std::uint64_t now = 0) {
    const auto c = command(core, client, now); const auto m = reserve(core, c, now);
    const auto e = create(core, c, now); CHECK(core.effect_authorized(e, now));
    CHECK(core.complete_create(e, now, true, true) == Decision::Accepted);
    CHECK(core.list(client, now).count == 0); // Arrival alone cannot publish an unreported ADD.
    const auto result = core.claim_completion(c, now, true);
    CHECK(result.claimed && result.decision == Decision::Accepted && same(result.monitor, m));
    CHECK(!core.claim_completion(c, now, true).claimed);
    CHECK(core.release_command(c) == Decision::Accepted);
    return m;
}
void finish_command(LeaseCore& core, CommandToken c, std::uint64_t now, bool success = false) {
    CHECK(core.claim_completion(c, now, success).claimed);
    CHECK(!core.claim_completion(c, now, success).claimed);
    CHECK(core.release_command(c) == Decision::Accepted);
}
void retire_one(LeaseCore& core) {
    const auto m = core.retirement_candidate(); CHECK(m.slot != NoSlot);
    const auto e = core.begin_retirement(m); CHECK(e.decision == Decision::Accepted);
    CHECK(core.begin_retirement(m).decision == Decision::Invalid);
    CHECK(core.complete_retirement(e.effect, true) == Decision::Accepted);
    CHECK(core.complete_retirement(e.effect, true) == Decision::Invalid);
}
void remove(LeaseCore& core, ClientToken client, MonitorToken m, std::uint64_t now = 0) {
    const auto c = command(core, client, now); CHECK(core.reserve_remove(c, now, m.id) == Decision::Accepted);
    CHECK(!core.claim_completion(c, now, true).claimed);
    retire_one(core); finish_command(core, c, now, true);
}
constexpr std::array<std::uint8_t, 128> Golden720{
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x0e, 0x04, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x24, 0x01, 0x03, 0x80, 0x10, 0x09, 0x78, 0x0e, 0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26,
    0x0f, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x1d, 0x00, 0x72, 0x51, 0xd0, 0x1e, 0x20, 0x6e, 0x28,
    0x55, 0x00, 0xa0, 0x5a, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x45,
};
constexpr std::array<std::uint8_t, 128> Golden1080{
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x0e, 0x04, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x00, 0x24, 0x01, 0x03, 0x80, 0x10, 0x09, 0x78, 0x0e, 0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26,
    0x0f, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c,
    0x45, 0x00, 0xa0, 0x5a, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6c,
};

void abi_exact_layout_v1() {
    CHECK(sizeof(CPD_HEADER) == 32 && alignof(CPD_HEADER) == 8);
    CHECK(sizeof(CPD_MODE) == 32 && alignof(CPD_MODE) == 8);
    CHECK(sizeof(CPD_ADD_REQUEST) == 64 && sizeof(CPD_ADD_RESPONSE) == 48);
    CHECK(sizeof(CPD_REMOVE_REQUEST) == 48 && sizeof(CPD_REMOVE_RESPONSE) == 48);
    CHECK(sizeof(CPD_LIST_REQUEST) == 32 && sizeof(CPD_LIST_RESPONSE) == 96);
    CHECK(sizeof(CPD_HEARTBEAT_REQUEST) == 48 && sizeof(CPD_HEARTBEAT_RESPONSE) == 48);
    CHECK(alignof(CPD_ADD_REQUEST) == 8 && alignof(CPD_ADD_RESPONSE) == 8);
    CHECK(alignof(CPD_REMOVE_REQUEST) == 8 && alignof(CPD_REMOVE_RESPONSE) == 8);
    CHECK(alignof(CPD_LIST_REQUEST) == 8 && alignof(CPD_LIST_ENTRY) == 8);
    CHECK(alignof(CPD_HEARTBEAT_REQUEST) == 8 && alignof(CPD_HEARTBEAT_RESPONSE) == 8);
    CHECK(offsetof(CPD_LIST_RESPONSE, entry) == 48 && offsetof(CPD_LIST_ENTRY, mode) == 8);
    CHECK(CPD_IOCTL_ADD == 0x8337e000U && CPD_IOCTL_REMOVE == 0x8337e004U);
    CHECK(CPD_IOCTL_LIST == 0x83376008U && CPD_IOCTL_HEARTBEAT == 0x8337e00cU);
    CHECK(CPD_MONITOR_ACTIVE == 1 && CPD_MONITOR_RETIRED == 2);
    CHECK(CPD_MAX_OPENS == 4 && CPD_MAX_MONITORS == 4 && CPD_MAX_COMMANDS == 16);
    CHECK(std::is_standard_layout<CPD_LIST_RESPONSE>::value);
    CHECK(std::is_trivially_copyable<CPD_LIST_RESPONSE>::value);
    CPD_LIST_RESPONSE response{};
    const auto* bytes = reinterpret_cast<const unsigned char*>(&response);
    for (std::size_t i = 0; i < sizeof(response); ++i) CHECK(bytes[i] == 0);
    initialize_header(response.header, sizeof(response), UINT64_MAX);
    CHECK(response.header.request_id == UINT64_MAX && response.header.flags == 0 && response.header.reserved == 0);
}
void abi_rejects_noncanonical_requests() {
    for (auto ioctl : {CPD_IOCTL_ADD, CPD_IOCTL_REMOVE, CPD_IOCTL_LIST, CPD_IOCTL_HEARTBEAT}) {
        const auto b = request(ioctl, UINT64_MAX);
        const auto output = ioctl == CPD_IOCTL_LIST ? 96U : 48U;
        DecodedRequest decoded{};
        CHECK(validate_request(ioctl, b.data(), b.size(), output, decoded) == Decision::Accepted);
        CHECK(decoded.header.request_id == UINT64_MAX);
        for (auto n : {std::size_t{0}, b.size() - 1, b.size() + 1})
            CHECK(validate_request(ioctl, b.data(), n, output, decoded) == Decision::Invalid);
        CHECK(validate_request(ioctl, nullptr, b.size(), output, decoded) == Decision::Invalid);
        CHECK(validate_request(ioctl, b.data(), b.size(), output - 1, decoded) == Decision::Invalid);
        for (unsigned offset : {0U,1U,2U,3U,4U,5U,6U,7U,8U,9U,10U,11U,12U,13U,14U,15U,
                                24U,25U,26U,27U,28U,29U,30U,31U}) {
            auto mutated = b; mutated[offset] ^= 1;
            CHECK(validate_request(ioctl, mutated.data(), mutated.size(), output, decoded) == Decision::Invalid);
            CHECK(decoded.header.magic == 0 && decoded.value == 0);
        }
        if (ioctl != CPD_IOCTL_LIST) {
            const unsigned first = ioctl == CPD_IOCTL_ADD ? 60 : 40;
            for (unsigned offset = first; offset < b.size(); ++offset) {
                auto mutated = b; mutated[offset] = 1;
                CHECK(validate_request(ioctl, mutated.data(), mutated.size(), output, decoded) == Decision::Invalid);
            }
        }
        std::vector<std::uint8_t> unaligned(b.size() + 1); std::copy(b.begin(), b.end(), unaligned.begin() + 1);
        CHECK(validate_request(ioctl, unaligned.data() + 1, b.size(), output, decoded) == Decision::Accepted);
    }
    auto b = request(CPD_IOCTL_REMOVE); put(b.data() + 32, 0, 8); DecodedRequest decoded{};
    CHECK(validate_request(CPD_IOCTL_REMOVE, b.data(), b.size(), 48, decoded) == Decision::Accepted);
    b = request(CPD_IOCTL_HEARTBEAT); put(b.data() + 32, 0, 8);
    CHECK(validate_request(CPD_IOCTL_HEARTBEAT, b.data(), b.size(), 48, decoded) == Decision::Invalid);
    CHECK(validate_request(CPD_IOCTL_ADD + 1, b.data(), b.size(), 48, decoded) == Decision::Invalid);
}
void mode_whitelist_and_checked_size() {
    for (bool hd : {false, true}) for (auto mm : {10U, 2000U}) CHECK(valid_mode(mode(hd, mm, mm)));
    for (auto mm : {0U, 9U, 2001U, UINT32_MAX}) {
        CHECK(!valid_mode(mode(true, mm, 90))); CHECK(!valid_mode(mode(true, 160, mm)));
    }
    auto m = mode(); m.width = UINT32_MAX; m.height = UINT32_MAX; CHECK(!valid_mode(m));
    m = mode(); m.width = 0; CHECK(!valid_mode(m));
    m = mode(); m.height = 0; CHECK(!valid_mode(m));
    m = mode(); m.width = 1920; CHECK(!valid_mode(m));
    for (auto r : {0U, 59U, 120U, UINT32_MAX}) { m = mode(); m.refresh_numerator = r; CHECK(!valid_mode(m)); }
    for (auto r : {0U, 2U, UINT32_MAX}) { m = mode(); m.refresh_denominator = r; CHECK(!valid_mode(m)); }
    for (auto bpp : {0U, 24U, 64U, UINT32_MAX}) { m = mode(); m.bits_per_pixel = bpp; CHECK(!valid_mode(m)); }
    m = mode(); m.reserved = 1; CHECK(!valid_mode(m));
}
void decode_edid(const EdidResult& e, const CPD_MODE& m, std::uint32_t serial) {
    CHECK(e.decision == Decision::Accepted); const auto& b = e.bytes;
    CHECK(b[0] == 0 && b[7] == 0); for (unsigned i = 1; i < 7; ++i) CHECK(b[i] == 255);
    CHECK(b[8] == 0x0e && b[9] == 4 && b[10] == 1 && b[11] == 0);
    const auto actual_serial = static_cast<std::uint32_t>(b[12]) | (static_cast<std::uint32_t>(b[13]) << 8) |
                               (static_cast<std::uint32_t>(b[14]) << 16) | (static_cast<std::uint32_t>(b[15]) << 24);
    CHECK(actual_serial == serial && b[18] == 1 && b[19] == 3 && b[126] == 0);
    unsigned sum = 0; for (auto byte : b) sum += byte; CHECK(sum % 256 == 0);
    const auto* d = b.data() + 54;
    const std::uint32_t clock = (d[0] | (static_cast<std::uint32_t>(d[1]) << 8)) * 10000;
    const std::uint32_t w = d[2] | ((d[4] >> 4) << 8), hb = d[3] | ((d[4] & 15) << 8);
    const std::uint32_t h = d[5] | ((d[7] >> 4) << 8), vb = d[6] | ((d[7] & 15) << 8);
    const std::uint32_t hf = d[8] | ((d[11] >> 6) << 8), hs = d[9] | (((d[11] >> 4) & 3) << 8);
    const std::uint32_t vf = (d[10] >> 4) | (((d[11] >> 2) & 3) << 4), vs = (d[10] & 15) | ((d[11] & 3) << 4);
    CHECK(w == m.width && h == m.height && clock == (m.width == 1280 ? 74250000U : 148500000U));
    CHECK(w + hb == (m.width == 1280 ? 1650U : 2200U));
    CHECK(h + vb == (m.width == 1280 ? 750U : 1125U));
    CHECK(clock == (w + hb) * (h + vb) * 60);
    CHECK(hf == (m.width == 1280 ? 110U : 88U) && hs == (m.width == 1280 ? 40U : 44U));
    CHECK(vf == (m.width == 1280 ? 5U : 4U) && vs == 5);
    CHECK(static_cast<std::uint32_t>(d[12] | ((d[14] >> 4) << 8)) == m.physical_width_mm);
    CHECK(static_cast<std::uint32_t>(d[13] | ((d[14] & 15) << 8)) == m.physical_height_mm);
    CHECK(b[21] == (m.physical_width_mm + 5) / 10 && b[22] == (m.physical_height_mm + 5) / 10);
    CHECK(d[15] == 0 && d[16] == 0 && d[17] == 0x1e);
    for (unsigned offset : {72U,90U,108U}) for (unsigned i = 0; i < 18; ++i) CHECK(b[offset + i] == (i == 3 ? 0x10 : 0));
}
void edid_720p_1080p_roundtrip() {
    CHECK(generate_edid(mode(true), 1).bytes == Golden720);
    CHECK(generate_edid(mode(false), 1).bytes == Golden1080);
    for (bool hd : {false, true}) for (auto mm : {10U, 2000U}) {
        const auto m = mode(hd, mm, mm); const auto e = generate_edid(m, 1); decode_edid(e, m, 1);
        CHECK(canonical_edid(e.bytes, m, 1));
    }
    decode_edid(generate_edid(mode(), 1), mode(), 1);
    decode_edid(generate_edid(mode(false), 1), mode(false), 1);
    CHECK(generate_edid(mode(true, 9), 1).decision == Decision::Invalid);
}
void edid_serial_identity_no_wrap() {
    const auto a = generate_edid(mode(), 1), b = generate_edid(mode(), 2);
    CHECK(a.bytes != b.bytes); decode_edid(b, mode(), 2);
    CHECK(generate_edid(mode(), 0).decision == Decision::Invalid);
    decode_edid(generate_edid(mode(), UINT32_MAX), mode(), UINT32_MAX);
    CHECK(generate_edid(mode(), UINT64_C(0x100000000)).decision == Decision::Invalid);
    CHECK(generate_edid(mode(), UINT64_MAX).decision == Decision::Invalid);
    for (unsigned i = 0; i < 128; ++i) {
        auto changed = a.bytes; changed[i] ^= 1; CHECK(!canonical_edid(changed, mode(), 1));
        if (i != 127) {
            unsigned sum = 0; for (unsigned j = 0; j < 127; ++j) sum += changed[j];
            changed[127] = static_cast<std::uint8_t>(0U - sum);
            CHECK(!canonical_edid(changed, mode(), 1));
        }
    }
    LeaseCore core(UINT32_MAX); const auto f = open(core); const auto m = add(core, f); CHECK(m.id == UINT32_MAX);
    remove(core, f, m); const auto c = command(core, f);
    CHECK(core.reserve_add(c, 0, mode()).decision == Decision::Capacity); finish_command(core, c, 0);
}
void file_capability_is_not_wire_identity() {
    LeaseCore core; const auto f = open(core), other = open(core); const auto m = add(core, f);
    CHECK(core.list(f, 0).count == 1 && core.list(other, 0).count == 0);
    for (auto id : {m.id, UINT64_C(999), UINT64_C(0)}) {
        const auto c = command(core, other); CHECK(core.reserve_remove(c, 0, id) == Decision::NotFound);
        CHECK(core.claim_completion(c, 0, true).decision == Decision::NotFound); CHECK(core.release_command(c) == Decision::Accepted);
    }
    const ClientToken duplicate = f; CHECK(same(f, duplicate));
    CHECK(core.heartbeat(duplicate, 100, 1) == Decision::Accepted);
    CHECK(core.heartbeat(f, 100, 1) == Decision::Invalid);
    CHECK(core.check({}, 100) == Decision::Invalid);
    CHECK(core.check({f.slot, f.generation + 1}, 100) == Decision::Invalid);
    CHECK(core.cleanup(other, 100) == Decision::Accepted); CHECK(core.check(other, 100) == Decision::Invalid);
    CHECK(core.list(f, 100).count == 1);
}
void admission_caps_and_slot_rollback() {
    LeaseCore core; std::array<ClientToken, 4> files{}; std::array<MonitorToken, 4> monitors{};
    for (auto& f : files) f = open(core);
    CHECK(core.open(0).decision == Decision::Capacity);
    for (unsigned i = 0; i < files.size(); ++i) monitors[i] = add(core, files[i]);
    auto c = command(core, files[0]); CHECK(core.reserve_add(c, 0, mode()).decision == Decision::Capacity);
    CHECK(core.reserve_add(c, 0, mode()).decision == Decision::Invalid); finish_command(core, c, 0);
    std::array<CommandToken, 16> commands{}; for (auto& cmd : commands) cmd = command(core, files[0]);
    CHECK(core.admit_command(files[0], 0).decision == Decision::Busy);
    for (auto cmd : commands) finish_command(core, cmd, 0, true);
    remove(core, files[0], monitors[0]);
    c = command(core, files[0]); const auto failed = reserve(core, c); const auto e = create(core, c);
    CHECK(core.complete_create(e, 0, false, false) == Decision::Invalid); CHECK(!core.snapshot(failed).found);
    finish_command(core, c, 0); CHECK(core.list(files[0], 0).count == 0);
    c = command(core, files[0]); const auto unarrived = reserve(core, c); const auto discard = create(core, c);
    CHECK(unarrived.id != failed.id); CHECK(core.complete_create(discard, 0, true, false) == Decision::Invalid);
    const auto retirement = core.begin_retirement(unarrived); CHECK(retirement.decision == Decision::Accepted);
    CHECK(retirement.effect.kind == EffectKind::Discard); CHECK(core.complete_retirement(retirement.effect, true) == Decision::Accepted);
    finish_command(core, c, 0); CHECK(add(core, files[0]).id != unarrived.id);
}
void heartbeat_strict_sequence_only() {
    LeaseCore core; const auto f = open(core), other = open(core);
    CHECK(core.heartbeat(f, 1000, 1) == Decision::Accepted); CHECK(core.remaining(f, 1000) == 5000);
    CHECK(core.heartbeat(f, 2000, 2) == Decision::Accepted);
    for (auto sequence : {UINT64_C(0), UINT64_C(1), UINT64_C(2)}) CHECK(core.heartbeat(f, 2000, sequence) == Decision::Invalid);
    const auto m = add(core, f, 2000); CHECK(core.list(f, 3000).remaining_ms == 4000);
    remove(core, f, m, 3000); CHECK(core.remaining(f, 3000) == 4000);
    CHECK(core.heartbeat(f, 4000, UINT64_MAX) == Decision::Accepted);
    CHECK(core.heartbeat(f, 4000, 0) == Decision::Invalid); CHECK(core.heartbeat(f, 4000, UINT64_MAX) == Decision::Invalid);
    CHECK(core.check(other, 5000) == Decision::Expired); CHECK(core.list(f, 5000).remaining_ms == 4000);
}
void deadline_boundary_never_resurrects() {
    LeaseCore early; const auto f = open(early); CHECK(early.heartbeat(f, 4999, 1) == Decision::Accepted);
    LeaseCore core; const auto expired = open(core); const auto c = command(core, expired);
    CHECK(core.heartbeat(expired, 5000, 1) == Decision::Expired); CHECK(core.reserve_add(c, 5000, mode()).decision != Decision::Accepted);
    CHECK(core.heartbeat(expired, 5001, 2) == Decision::Expired); finish_command(core, c, 5001);
    const auto next = open(core, 5001); CHECK(next.generation != expired.generation);
    CHECK(core.check(expired, 5001) != Decision::Accepted); CHECK(core.check(next, 5000) == Decision::Invalid);
    LeaseCore overflow; CHECK(overflow.open(UINT64_MAX - 4999).decision == Decision::Invalid);
    LeaseCore hb_overflow; const auto near = open(hb_overflow, UINT64_MAX - 6000);
    CHECK(hb_overflow.heartbeat(near, UINT64_MAX - 4999, 1) == Decision::Invalid);
    CHECK(hb_overflow.check(near, UINT64_MAX - 1000) == Decision::Expired);
    LeaseCore generations(1, UINT64_MAX); CHECK(generations.open(0).decision == Decision::Accepted);
    CHECK(generations.open(0).decision == Decision::Capacity);
}
void last_handle_cleanup_and_duplicate_limit() {
    LeaseCore core; const auto f = open(core); const auto duplicate = f; const auto m = add(core, f);
    CHECK(core.heartbeat(duplicate, 1000, 1) == Decision::Accepted); CHECK(core.check(f, 5000) == Decision::Accepted);
    CHECK(core.cleanup(f, 5000) == Decision::Accepted); CHECK(core.cleanup(duplicate, 5000) == Decision::Accepted);
    CHECK(core.check(duplicate, 5000) != Decision::Accepted);
    CHECK(core.snapshot(m).phase == MonitorPhase::RetirePending); CHECK(!core.quiescent()); retire_one(core);
    CHECK(!core.snapshot(m).found && core.quiescent());
    LeaseCore hung; const auto held = open(hung); const auto copied = held; add(hung, held);
    CHECK(hung.check(copied, 5000) == Decision::Expired); CHECK(!hung.quiescent()); retire_one(hung);
}
void create_expiry_cleanup_interleavings() {
    for (unsigned phase = 0; phase < 3; ++phase) for (unsigned invalidator = 0; invalidator < 3; ++invalidator) {
        LeaseCore core; const auto f = open(core); const auto c = command(core, f); const auto m = reserve(core, c);
        EffectToken e{}; if (phase >= 1) e = create(core, c);
        if (phase == 2) CHECK(core.complete_create(e, 0, true, true) == Decision::Accepted);
        const std::uint64_t now = invalidator == 0 ? 5000 : 0;
        if (invalidator == 0) CHECK(core.expire(now) == Decision::Accepted);
        else if (invalidator == 1) CHECK(core.cleanup(f, now) == Decision::Accepted);
        else CHECK(core.cancel(c, now) == Decision::Accepted);
        if (phase == 1) {
            CHECK(!core.effect_authorized(e, now));
            CHECK(core.snapshot(m).phase == MonitorPhase::Creating);
            CHECK(core.claim_completion(c, now, false).claimed); CHECK(core.release_command(c) == Decision::Busy);
            CHECK(core.complete_create(e, now, true, true) != Decision::Accepted);
            CHECK(core.complete_create(e, now, true, true) == Decision::Invalid);
        }
        CHECK(core.list(f, now).count == 0);
        if (phase > 0) { CHECK(core.snapshot(m).phase == MonitorPhase::RetirePending); retire_one(core); }
        if (phase == 1) CHECK(core.release_command(c) == Decision::Accepted);
        else finish_command(core, c, now);
        CHECK(!core.snapshot(m).found && core.quiescent());
    }
}
void remove_timer_pnp_idempotence() {
    std::array<unsigned, 4> order{0,1,2,3};
    do {
        LeaseCore core; const auto f = open(core); const auto m = add(core, f); const auto c = command(core, f);
        std::uint64_t now = 0;
        for (auto event : order) {
            if (event == 0) {
                const auto result = core.reserve_remove(c, now, m.id); CHECK(result == Decision::Accepted || result == Decision::Expired || result == Decision::Invalid);
            } else if (event == 1) { now = 5000; CHECK(core.expire(now) == Decision::Accepted); }
            else if (event == 2) CHECK(core.cleanup(f, now) == Decision::Accepted);
            else CHECK(core.stop(now) == Decision::Accepted);
        }
        retire_one(core); finish_command(core, c, now); CHECK(core.quiescent());
        CHECK(core.restart(now) == Decision::Accepted); const auto fresh = open(core, now);
        CHECK(core.list(fresh, now).count == 0 && !core.snapshot(m).found);
        CHECK(core.begin_retirement(m).decision == Decision::Invalid);
        CHECK(core.check(f, now) != Decision::Accepted);
    } while (std::next_permutation(order.begin(), order.end()));
}
void request_cancel_completion_once() {
    for (unsigned phase = 0; phase < 4; ++phase) {
        LeaseCore core; const auto f = open(core); const auto c = command(core, f); const auto m = reserve(core, c);
        EffectToken e{}; if (phase >= 1) e = create(core, c);
        if (phase >= 2) CHECK(core.complete_create(e, 0, true, true) == Decision::Accepted);
        if (phase == 3) CHECK(core.claim_completion(c, 0, true).claimed);
        CHECK(core.cancel(c, 0) == (phase == 3 ? Decision::NotFound : Decision::Accepted));
        if (phase == 1) CHECK(!core.effect_authorized(e, 0));
        if (phase < 3) {
            CHECK(core.claim_completion(c, 0, false).claimed); CHECK(!core.claim_completion(c, 0, false).claimed);
            if (phase == 1) { CHECK(core.release_command(c) == Decision::Busy); CHECK(core.complete_create(e, 0, true, true) == Decision::Invalid); }
            if (phase > 0) retire_one(core);
        } else {
            CHECK(core.list(f, 0).count == 1); CHECK(!core.claim_completion(c, 0, false).claimed);
            CHECK(core.cleanup(f, 0) == Decision::Accepted); retire_one(core);
        }
        CHECK(core.release_command(c) == Decision::Accepted); CHECK(!core.claim_completion(c, 0, true).claimed);
        CHECK(core.cancel({}, 0) == Decision::NotFound); CHECK(!core.snapshot(m).found);
    }
}
void dead_handle_epoch_and_teardown_failure() {
    LeaseCore delayed; const auto delayed_file = open(delayed); const auto delayed_command = command(delayed, delayed_file);
    reserve(delayed, delayed_command); const auto delayed_effect = create(delayed, delayed_command);
    CHECK(delayed.effect_authorized(delayed_effect, 4999)); CHECK(!delayed.effect_authorized(delayed_effect, 5000));
    CHECK(delayed.complete_create(delayed_effect, 5000, true, true) == Decision::Expired);
    retire_one(delayed); finish_command(delayed, delayed_command, 5000);
    LeaseCore stopped; const auto stop_file = open(stopped); const auto stop_command = command(stopped, stop_file);
    reserve(stopped, stop_command); const auto stop_effect = create(stopped, stop_command);
    CHECK(stopped.stop(0) == Decision::Accepted); CHECK(!stopped.effect_authorized(stop_effect, 0));
    CHECK(stopped.complete_create(stop_effect, 0, true, true) != Decision::Accepted);
    retire_one(stopped); finish_command(stopped, stop_command, 0);
    LeaseCore refused; const auto refused_file = open(refused); const auto refused_command = command(refused, refused_file);
    CHECK(refused.reserve_add(refused_command, 0, mode(true, 9)).decision == Decision::Invalid);
    CHECK(refused.reserve_add(refused_command, 0, mode()).decision == Decision::Invalid);
    CHECK(refused.begin_create(refused_command, 0).decision == Decision::Invalid);
    CHECK(refused.reserve_remove(refused_command, 0, 1) == Decision::Invalid);
    CHECK(refused.claim_completion(refused_command, 0, true).decision == Decision::Invalid);
    CHECK(refused.release_command(refused_command) == Decision::Accepted && refused.quiescent());
    LeaseCore cleaned; const auto f = open(cleaned); const auto c = command(cleaned, f); const auto m = reserve(cleaned, c);
    const auto e = create(cleaned, c); CHECK(cleaned.effect_authorized(e, 0));
    CHECK(cleaned.framework_cleanup() == Decision::Busy); CHECK(!cleaned.effect_authorized(e, 0));
    CHECK(cleaned.complete_create(e, 0, true, true) != Decision::Accepted);
    CHECK(cleaned.begin_retirement(m).decision == Decision::Stopped); CHECK(!cleaned.quiescent());
    CHECK(cleaned.claim_completion(c, 0, false).claimed); CHECK(cleaned.release_command(c) == Decision::Busy);
    LeaseCore failed; const auto other = open(failed); const auto live = add(failed, other);
    CHECK(failed.cleanup(other, 0) == Decision::Accepted); const auto retirement = failed.begin_retirement(live);
    CHECK(retirement.decision == Decision::Accepted); CHECK(failed.complete_retirement(retirement.effect, false) == Decision::Stopped);
    CHECK(failed.snapshot(live).found && !failed.quiescent()); CHECK(failed.begin_retirement(live).decision == Decision::Stopped);
    CHECK(failed.complete_retirement(retirement.effect, true) == Decision::Invalid);
    MonitorLifetime lifetime; CHECK(lifetime.begin_effect()); CHECK(lifetime.note_processor_started());
    lifetime.close_admission(); CHECK(!lifetime.quiescent()); CHECK(!lifetime.note_framework_cleanup());
    CHECK(!lifetime.begin_effect() && !lifetime.begin_callback()); lifetime.end_effect();
    CHECK(!lifetime.note_processor_exit_observed()); CHECK(lifetime.snapshot().processor_exit_observed);
    CHECK(lifetime.snapshot().protocol_failure && !lifetime.quiescent());
    MonitorLifetime orderly; CHECK(orderly.begin_effect()); CHECK(orderly.begin_callback());
    CHECK(orderly.note_processor_started()); orderly.close_admission(); orderly.end_effect(); orderly.end_callback();
    CHECK(orderly.note_processor_exit_observed()); CHECK(orderly.note_framework_cleanup()); CHECK(orderly.quiescent());
    DeviceAdmission device; const auto starting = device.generation(); CHECK(device.publish_ready(starting));
    AdmissionEpoch token{}; CHECK(device.acquire_epoch(token)); device.begin_stop(); CHECK(!device.accepts(token));
    device.finish_stop(false, false); CHECK(device.phase() == DevicePhase::Stopping);
    device.finish_stop(true, true); CHECK(device.phase() == DevicePhase::Stopped);
    CHECK(device.restart()); CHECK(!device.accepts(token));
}
} // namespace
int main() {
    struct Case { const char* name; void (*run)(); };
    const std::array<Case, 14> cases{{
        {"abi_exact_layout_v1", abi_exact_layout_v1},
        {"abi_rejects_noncanonical_requests", abi_rejects_noncanonical_requests},
        {"mode_whitelist_and_checked_size", mode_whitelist_and_checked_size},
        {"edid_720p_1080p_roundtrip", edid_720p_1080p_roundtrip},
        {"edid_serial_identity_no_wrap", edid_serial_identity_no_wrap},
        {"file_capability_is_not_wire_identity", file_capability_is_not_wire_identity},
        {"admission_caps_and_slot_rollback", admission_caps_and_slot_rollback},
        {"heartbeat_strict_sequence_only", heartbeat_strict_sequence_only},
        {"deadline_boundary_never_resurrects", deadline_boundary_never_resurrects},
        {"last_handle_cleanup_and_duplicate_limit", last_handle_cleanup_and_duplicate_limit},
        {"create_expiry_cleanup_interleavings", create_expiry_cleanup_interleavings},
        {"remove_timer_pnp_idempotence", remove_timer_pnp_idempotence},
        {"request_cancel_completion_once", request_cancel_completion_once},
        {"dead_handle_epoch_and_teardown_failure", dead_handle_epoch_and_teardown_failure},
    }};
    try {
        for (const auto& c : cases) { current_case = c.name; c.run(); std::cout << "PASS " << c.name << '\n'; }
    } catch (const Failure& failure) {
        std::cerr << "FAIL " << current_case << ':' << failure.line << ' ' << failure.expression << '\n'; return 1;
    }
    std::cout << "PASS " << cases.size() << " focused cases, " << checks << " checks (NDEBUG-safe)\n";
    return 0;
}
