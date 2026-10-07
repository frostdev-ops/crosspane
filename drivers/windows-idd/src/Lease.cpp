#include "Lease.h"
#include <limits>

namespace crosspane::idd {
namespace {
std::uint16_t read16(const std::uint8_t* p) noexcept {
    return static_cast<std::uint16_t>(p[0] | (static_cast<std::uint16_t>(p[1]) << 8));
}
std::uint32_t read32(const std::uint8_t* p) noexcept {
    std::uint32_t result = 0;
    for (unsigned i = 0; i < 4; ++i) result |= static_cast<std::uint32_t>(p[i]) << (8 * i);
    return result;
}
std::uint64_t read64(const std::uint8_t* p) noexcept {
    std::uint64_t result = 0;
    for (unsigned i = 0; i < 8; ++i) result |= static_cast<std::uint64_t>(p[i]) << (8 * i);
    return result;
}
}

bool valid_mode(const CPD_MODE& m) noexcept {
    const bool resolution = (m.width == 1280 && m.height == 720) ||
                            (m.width == 1920 && m.height == 1080);
    // Widen before multiplication, then enforce the fixed pixel/representation bound.
    const std::uint64_t pixels = static_cast<std::uint64_t>(m.width) * m.height;
    const std::uint64_t bytes = pixels <= UINT64_MAX / 4 ? pixels * 4 : UINT64_MAX;
    return resolution && m.refresh_numerator == 60 && m.refresh_denominator == 1 &&
           m.physical_width_mm >= 10 && m.physical_width_mm <= 2000 &&
           m.physical_height_mm >= 10 && m.physical_height_mm <= 2000 &&
           m.bits_per_pixel == 32 && m.reserved == 0 && bytes <= UINT32_MAX;
}

void initialize_header(CPD_HEADER& h, std::uint32_t bytes, std::uint64_t id) noexcept {
    h = {}; h.magic = CPD_MAGIC; h.major = CPD_MAJOR; h.minor = CPD_MINOR;
    h.struct_bytes = bytes; h.request_id = id;
}

Decision validate_request(std::uint32_t ioctl, const void* bytes, std::size_t length,
                          std::size_t output_capacity, DecodedRequest& decoded) noexcept {
    decoded = {};
    std::size_t expected = 0, output = 0;
    Operation operation{};
    switch (ioctl) {
    case CPD_IOCTL_ADD: operation = Operation::Add; expected = 64; output = 48; break;
    case CPD_IOCTL_REMOVE: operation = Operation::Remove; expected = 48; output = 48; break;
    case CPD_IOCTL_LIST: operation = Operation::List; expected = 32; output = 96; break;
    case CPD_IOCTL_HEARTBEAT: operation = Operation::Heartbeat; expected = 48; output = 48; break;
    default: return Decision::Invalid;
    }
    if (bytes == nullptr || length != expected || output_capacity < output) return Decision::Invalid;
    const auto* p = static_cast<const std::uint8_t*>(bytes);
    if (read32(p) != CPD_MAGIC || read16(p + 4) != CPD_MAJOR || read16(p + 6) != CPD_MINOR ||
        read32(p + 8) != expected || read32(p + 12) != 0 || read64(p + 24) != 0)
        return Decision::Invalid;
    DecodedRequest result{};
    result.operation = operation;
    initialize_header(result.header, static_cast<std::uint32_t>(expected), read64(p + 16));
    if (operation == Operation::Add) {
        result.mode = {read32(p + 32), read32(p + 36), read32(p + 40), read32(p + 44),
                       read32(p + 48), read32(p + 52), read32(p + 56), read32(p + 60)};
        if (!valid_mode(result.mode)) return Decision::Invalid;
    } else if (operation == Operation::Remove || operation == Operation::Heartbeat) {
        if (read64(p + 40) != 0) return Decision::Invalid;
        result.value = read64(p + 32);
        if (operation == Operation::Heartbeat && result.value == 0) return Decision::Invalid;
    }
    decoded = result;
    return Decision::Accepted;
}

bool same(ClientToken a, ClientToken b) noexcept {
    return a.slot == b.slot && a.generation == b.generation;
}
bool same(MonitorToken a, MonitorToken b) noexcept {
    return a.slot == b.slot && a.generation == b.generation && a.id == b.id;
}
LeaseCore::LeaseCore(std::uint64_t first_id, std::uint64_t first_generation) noexcept
    : next_id_(first_id), next_generation_(first_generation) {}

LeaseCore::File* LeaseCore::find(ClientToken t) noexcept {
    if (t.slot >= files_.size() || t.generation == 0) return nullptr;
    auto& f = files_[t.slot];
    return f.phase != FilePhase::Free && f.generation == t.generation ? &f : nullptr;
}
const LeaseCore::File* LeaseCore::find(ClientToken t) const noexcept {
    if (t.slot >= files_.size() || t.generation == 0) return nullptr;
    const auto& f = files_[t.slot];
    return f.phase != FilePhase::Free && f.generation == t.generation ? &f : nullptr;
}
LeaseCore::Command* LeaseCore::find(CommandToken t) noexcept {
    if (t.slot >= commands_.size() || t.generation == 0) return nullptr;
    auto& c = commands_[t.slot];
    return c.occupied && c.generation == t.generation ? &c : nullptr;
}
const LeaseCore::Command* LeaseCore::find(CommandToken t) const noexcept {
    if (t.slot >= commands_.size() || t.generation == 0) return nullptr;
    const auto& c = commands_[t.slot];
    return c.occupied && c.generation == t.generation ? &c : nullptr;
}
LeaseCore::Monitor* LeaseCore::find(MonitorToken t) noexcept {
    if (t.slot >= monitors_.size() || t.generation == 0 || t.id == 0) return nullptr;
    auto& m = monitors_[t.slot];
    return m.phase != MonitorPhase::Free && m.generation == t.generation && m.id == t.id ? &m : nullptr;
}
const LeaseCore::Monitor* LeaseCore::find(MonitorToken t) const noexcept {
    if (t.slot >= monitors_.size() || t.generation == 0 || t.id == 0) return nullptr;
    const auto& m = monitors_[t.slot];
    return m.phase != MonitorPhase::Free && m.generation == t.generation && m.id == t.id ? &m : nullptr;
}
std::uint64_t LeaseCore::generation() noexcept {
    const auto result = next_generation_;
    if (next_generation_ != 0) next_generation_ = next_generation_ == UINT64_MAX ? 0 : next_generation_ + 1;
    return result;
}
Decision LeaseCore::client_decision(ClientToken t) const noexcept {
    const auto* f = find(t);
    if (!f) return Decision::Invalid;
    if (f->phase == FilePhase::Expired) return Decision::Expired;
    if (f->phase != FilePhase::Open) return Decision::Invalid;
    return running_ && native_allowed_ && !teardown_failed_ ? Decision::Accepted : Decision::Stopped;
}
Decision LeaseCore::advance(std::uint64_t now) noexcept {
    if (now < now_) return Decision::Invalid;
    now_ = now;
    for (std::uint32_t i = 0; i < files_.size(); ++i) {
        const auto& f = files_[i];
        if (f.phase == FilePhase::Open && now >= f.deadline)
            invalidate({i, f.generation}, FilePhase::Expired);
    }
    return Decision::Accepted;
}
void LeaseCore::reap_files() noexcept {
    for (std::uint32_t i = 0; i < files_.size(); ++i) {
        auto& f = files_[i];
        if (f.phase != FilePhase::Closing && f.phase != FilePhase::Expired) continue;
        const ClientToken t{i, f.generation};
        bool held = false;
        for (const auto& m : monitors_) held = held || (m.phase != MonitorPhase::Free && same(m.owner, t));
        for (const auto& c : commands_) held = held || (c.occupied && same(c.client, t));
        // Keep Expired sticky even without effects, so a late heartbeat still reports expiry.
        if (!held && f.phase == FilePhase::Closing) f.phase = FilePhase::Retired;
    }
}
void LeaseCore::drop_monitor(Monitor& m, Decision result) noexcept {
    if (auto* c = find(m.command)) {
        c->monitor = {}; c->ready = true;
        if (c->result == Decision::Accepted) c->result = result;
    }
    m = {};
    reap_files();
}
void LeaseCore::retire(Monitor& m) noexcept {
    if (m.phase == MonitorPhase::Reserved) {
        drop_monitor(m, Decision::Expired);
    } else if (m.phase == MonitorPhase::Creating) {
        if (auto* c = find(m.command)) { c->cancelled = true; c->result = Decision::Expired; }
    } else if (m.phase == MonitorPhase::AwaitingReply || m.phase == MonitorPhase::Active) {
        m.phase = MonitorPhase::RetirePending;
        if (auto* c = find(m.command)) { c->result = Decision::Expired; c->ready = false; }
    }
}
void LeaseCore::invalidate(ClientToken t, FilePhase phase) noexcept {
    auto* f = find(t);
    if (!f || f->phase != FilePhase::Open) return;
    f->phase = phase;
    for (auto& c : commands_) {
        if (c.occupied && same(c.client, t) && !c.completed) {
            c.cancelled = true; c.result = Decision::Expired;
        }
    }
    for (auto& m : monitors_) if (m.phase != MonitorPhase::Free && same(m.owner, t)) retire(m);
    reap_files();
}
Admission LeaseCore::open(std::uint64_t now) noexcept {
    if (advance(now) != Decision::Accepted || now > UINT64_MAX - CPD_LEASE_MS) return {Decision::Invalid, {}};
    if (!running_ || !native_allowed_ || teardown_failed_) return {Decision::Stopped, {}};
    reap_files();
    for (std::uint32_t i = 0; i < files_.size(); ++i) {
        auto& f = files_[i];
        bool reusable = f.phase == FilePhase::Free || f.phase == FilePhase::Retired;
        if (f.phase == FilePhase::Expired) {
            reusable = true;
            const ClientToken old{i, f.generation};
            for (const auto& m : monitors_) if (m.phase != MonitorPhase::Free && same(m.owner, old)) reusable = false;
            for (const auto& c : commands_) if (c.occupied && same(c.client, old)) reusable = false;
        }
        if (!reusable) continue;
        const auto g = generation();
        if (g == 0) return {Decision::Capacity, {}};
        f = {FilePhase::Open, g, now + CPD_LEASE_MS, 0};
        return {Decision::Accepted, {i, g}};
    }
    return {Decision::Capacity, {}};
}
Decision LeaseCore::check(ClientToken t, std::uint64_t now) noexcept {
    if (advance(now) != Decision::Accepted) return Decision::Invalid;
    return client_decision(t);
}
Decision LeaseCore::heartbeat(ClientToken t, std::uint64_t now, std::uint64_t sequence) noexcept {
    const auto decision = check(t, now);
    if (decision != Decision::Accepted) return decision;
    auto* f = find(t);
    if (sequence == 0 || sequence <= f->sequence || now > UINT64_MAX - CPD_LEASE_MS) return Decision::Invalid;
    f->sequence = sequence; f->deadline = now + CPD_LEASE_MS;
    return Decision::Accepted;
}
std::uint32_t LeaseCore::remaining(ClientToken t, std::uint64_t now) const noexcept {
    const auto* f = find(t);
    if (!f || f->phase != FilePhase::Open || now < now_ || now >= f->deadline) return 0;
    const auto delta = f->deadline - now;
    return delta <= CPD_LEASE_MS ? static_cast<std::uint32_t>(delta) : 0;
}
ListSnapshot LeaseCore::list(ClientToken t, std::uint64_t now) noexcept {
    ListSnapshot result{}; result.decision = check(t, now);
    if (result.decision != Decision::Accepted) return result;
    result.remaining_ms = remaining(t, now);
    for (std::uint32_t i = 0; i < monitors_.size(); ++i) {
        const auto& m = monitors_[i];
        if (m.phase == MonitorPhase::Active && same(m.owner, t)) {
            result.count = 1; result.monitor = {i, m.generation, m.id}; result.mode = m.mode; break;
        }
    }
    return result;
}
CommandAdmission LeaseCore::admit_command(ClientToken t, std::uint64_t now) noexcept {
    const auto decision = check(t, now);
    if (decision != Decision::Accepted) return {decision, {}};
    for (std::uint32_t i = 0; i < commands_.size(); ++i) {
        auto& c = commands_[i];
        if (c.occupied) continue;
        const auto g = generation();
        if (g == 0) return {Decision::Capacity, {}};
        c = {}; c.occupied = true; c.generation = g; c.client = t;
        return {Decision::Accepted, {i, g}};
    }
    return {Decision::Busy, {}};
}
AddAdmission LeaseCore::reserve_add(CommandToken t, std::uint64_t now, const CPD_MODE& mode) noexcept {
    auto* c = find(t);
    if (!c || c->completed || c->cancelled || c->result != Decision::Accepted || c->monitor.slot != NoSlot) return {Decision::Invalid, {}};
    const auto decision = check(c->client, now);
    const auto refuse = [c](Decision result) noexcept -> AddAdmission {
        c->result = result; return {result, {}};
    };
    if (decision != Decision::Accepted) return refuse(decision);
    if (!valid_mode(mode)) return refuse(Decision::Invalid);
    for (const auto& m : monitors_) if (m.phase != MonitorPhase::Free && same(m.owner, c->client)) return refuse(Decision::Capacity);
    if (next_id_ == 0 || next_id_ > UINT32_MAX) return refuse(Decision::Capacity);
    for (std::uint32_t i = 0; i < monitors_.size(); ++i) {
        auto& m = monitors_[i];
        if (m.phase != MonitorPhase::Free) continue;
        const auto g = generation();
        if (g == 0) return refuse(Decision::Capacity);
        m = {}; m.phase = MonitorPhase::Reserved; m.generation = g; m.id = next_id_++;
        m.owner = c->client; m.mode = mode; m.command = t;
        c->monitor = {i, g, m.id}; c->ready = false;
        return {Decision::Accepted, c->monitor};
    }
    return refuse(Decision::Capacity);
}
EffectAdmission LeaseCore::begin_create(CommandToken t, std::uint64_t now) noexcept {
    auto* c = find(t);
    if (!c || c->completed || c->cancelled || c->result != Decision::Accepted) return {Decision::Invalid, {}};
    const auto decision = check(c->client, now);
    if (decision != Decision::Accepted) return {decision, {}};
    auto* m = find(c->monitor);
    if (!m || m->phase != MonitorPhase::Reserved) return {Decision::Invalid, {}};
    m->phase = MonitorPhase::Creating; m->effect_kind = EffectKind::Create; m->effect_epoch = epoch_;
    return {Decision::Accepted, {c->monitor, EffectKind::Create, epoch_}};
}
Decision LeaseCore::complete_create(EffectToken effect, std::uint64_t now, bool created, bool arrived) noexcept {
    auto* m = find(effect.monitor);
    if (!m || m->phase != MonitorPhase::Creating || effect.kind != EffectKind::Create ||
        m->effect_epoch != effect.epoch) return Decision::Invalid;
    const bool clock_valid = advance(now) == Decision::Accepted;
    auto* c = find(m->command);
    if (!created && !arrived) { drop_monitor(*m, Decision::Invalid); return Decision::Invalid; }
    m->created = true; m->arrived = arrived;
    if (!created && arrived) {
        teardown_failed_ = true; running_ = false; m->phase = MonitorPhase::RetirePending;
        if (c) c->result = Decision::Stopped;
        return Decision::Stopped;
    }
    const auto decision = clock_valid ? client_decision(m->owner) : Decision::Invalid;
    if (arrived && c && !c->cancelled && !c->completed && c->result == Decision::Accepted && decision == Decision::Accepted) {
        m->phase = MonitorPhase::AwaitingReply; c->ready = true; return Decision::Accepted;
    }
    m->phase = MonitorPhase::RetirePending;
    if (c) {
        if (c->result == Decision::Accepted) c->result = decision == Decision::Accepted ? Decision::Invalid : decision;
        c->ready = false;
    }
    return decision == Decision::Accepted ? Decision::Invalid : decision;
}
Decision LeaseCore::reserve_remove(CommandToken t, std::uint64_t now, std::uint64_t id) noexcept {
    auto* c = find(t);
    if (!c || c->completed || c->cancelled || c->result != Decision::Accepted || c->monitor.slot != NoSlot) return Decision::Invalid;
    const auto decision = check(c->client, now);
    if (decision != Decision::Accepted) { c->result = decision; return decision; }
    for (std::uint32_t i = 0; i < monitors_.size(); ++i) {
        auto& m = monitors_[i];
        if (m.phase == MonitorPhase::Active && same(m.owner, c->client) && m.id == id) {
            c->monitor = {i, m.generation, m.id}; c->ready = false; m.command = t;
            m.phase = MonitorPhase::RetirePending; return Decision::Accepted;
        }
    }
    c->result = Decision::NotFound;
    return Decision::NotFound;
}
Decision LeaseCore::cancel(CommandToken t, std::uint64_t now) noexcept {
    auto* c = find(t);
    if (!c || c->completed) return Decision::NotFound;
    const auto clock = advance(now);
    c->cancelled = true;
    if (c->result == Decision::Accepted) c->result = Decision::Invalid;
    if (auto* m = find(c->monitor)) {
        if (m->phase == MonitorPhase::Reserved) drop_monitor(*m, Decision::Invalid);
        else if (m->phase == MonitorPhase::AwaitingReply) { m->phase = MonitorPhase::RetirePending; c->ready = false; }
    }
    return clock;
}
Completion LeaseCore::claim_completion(CommandToken t, std::uint64_t now, bool deliver_success) noexcept {
    auto* c = find(t);
    if (!c || c->completed) return {};
    const auto clock = advance(now);
    if (deliver_success && !c->ready) return {false, Decision::Busy, {}};
    auto* m = find(c->monitor);
    const auto decision = clock == Decision::Accepted ? client_decision(c->client) : clock;
    if (!deliver_success || decision != Decision::Accepted || c->result != Decision::Accepted) {
        c->cancelled = true;
        if (c->result == Decision::Accepted) c->result = decision == Decision::Accepted ? Decision::Invalid : decision;
        if (m && m->phase == MonitorPhase::Reserved) { drop_monitor(*m, c->result); m = nullptr; }
        if (m && m->phase == MonitorPhase::AwaitingReply) { m->phase = MonitorPhase::RetirePending; c->ready = false; }
    }
    const auto monitor = c->monitor;
    if (deliver_success && c->result == Decision::Accepted && m && m->phase == MonitorPhase::AwaitingReply) {
        m->phase = MonitorPhase::Active; m->command = {}; c->monitor = {};
    }
    c->completed = true;
    return {true, c->result, monitor};
}
Decision LeaseCore::release_command(CommandToken t) noexcept {
    auto* c = find(t);
    if (!c) return Decision::Invalid;
    if (!c->completed || c->monitor.slot != NoSlot) return Decision::Busy;
    *c = {}; reap_files(); return Decision::Accepted;
}
MonitorToken LeaseCore::retirement_candidate() const noexcept {
    for (std::uint32_t i = 0; i < monitors_.size(); ++i) {
        const auto& m = monitors_[i];
        if (m.phase == MonitorPhase::RetirePending) return {i, m.generation, m.id};
    }
    return {};
}
EffectAdmission LeaseCore::begin_retirement(MonitorToken t) noexcept {
    if (!native_allowed_ || teardown_failed_) return {Decision::Stopped, {}};
    auto* m = find(t);
    if (!m || m->phase != MonitorPhase::RetirePending || !m->created) return {Decision::Invalid, {}};
    m->phase = MonitorPhase::Retiring; m->effect_epoch = epoch_;
    m->effect_kind = m->arrived ? EffectKind::Depart : EffectKind::Discard;
    return {Decision::Accepted, {t, m->effect_kind, epoch_}};
}
Decision LeaseCore::complete_retirement(EffectToken effect, bool succeeded) noexcept {
    auto* m = find(effect.monitor);
    if (!m || m->phase != MonitorPhase::Retiring || m->effect_kind != effect.kind ||
        effect.kind == EffectKind::Create || m->effect_epoch != effect.epoch) return Decision::Invalid;
    if (!succeeded) {
        m->phase = MonitorPhase::RetirePending; teardown_failed_ = true; running_ = false;
        if (auto* c = find(m->command)) c->result = Decision::Stopped;
        return Decision::Stopped;
    }
    drop_monitor(*m, Decision::Accepted); return Decision::Accepted;
}
Decision LeaseCore::cleanup(ClientToken t, std::uint64_t now) noexcept {
    const auto clock = advance(now);
    auto* f = find(t);
    if (!f) return Decision::Invalid;
    if (f->phase == FilePhase::Open) invalidate(t, FilePhase::Closing);
    reap_files(); return clock;
}
Decision LeaseCore::expire(std::uint64_t now) noexcept { return advance(now); }
Decision LeaseCore::stop(std::uint64_t now) noexcept {
    const auto clock = advance(now); running_ = false;
    if (epoch_ == UINT64_MAX) { native_allowed_ = false; teardown_failed_ = true; }
    else ++epoch_;
    for (std::uint32_t i = 0; i < files_.size(); ++i) invalidate({i, files_[i].generation}, FilePhase::Closing);
    return clock;
}
Decision LeaseCore::restart(std::uint64_t now) noexcept {
    if (advance(now) != Decision::Accepted) return Decision::Invalid;
    if (running_ || !native_allowed_ || teardown_failed_ || !quiescent() || epoch_ == UINT64_MAX) return Decision::Stopped;
    ++epoch_; running_ = true; return Decision::Accepted;
}
Decision LeaseCore::framework_cleanup() noexcept {
    native_allowed_ = false; running_ = false;
    for (std::uint32_t i = 0; i < files_.size(); ++i) invalidate({i, files_[i].generation}, FilePhase::Closing);
    return quiescent() ? Decision::Accepted : Decision::Busy;
}
bool LeaseCore::effect_authorized(EffectToken effect, std::uint64_t now) noexcept {
    if (advance(now) != Decision::Accepted) return false;
    const auto* m = find(effect.monitor);
    if (!native_allowed_ || teardown_failed_ || !m || m->effect_epoch != effect.epoch ||
        m->effect_kind != effect.kind) return false;
    if (effect.kind == EffectKind::Create) {
        const auto* c = find(m->command);
        const auto* f = find(m->owner);
        return m->phase == MonitorPhase::Creating && effect.epoch == epoch_ && running_ &&
               f && f->phase == FilePhase::Open && now_ < f->deadline &&
               c && !c->cancelled && !c->completed && c->result == Decision::Accepted;
    }
    // Stop closes creation authority but permits already-owned retirement until the
    // separate framework-Cleanup barrier. This check is never a DDI lifetime proof.
    return m->phase == MonitorPhase::Retiring;
}
bool LeaseCore::quiescent() const noexcept {
    for (const auto& m : monitors_) if (m.phase != MonitorPhase::Free) return false;
    for (const auto& c : commands_) if (c.occupied) return false;
    return true;
}
MonitorSnapshot LeaseCore::snapshot(MonitorToken t) const noexcept {
    const auto* m = find(t);
    if (!m) return {};
    return {true, m->phase, m->owner, m->mode, m->created, m->arrived};
}
} // namespace crosspane::idd
