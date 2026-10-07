// Crosspane private portable lease/codec core. No WDF, clock queries or native effects.
#pragma once
#include "../include/crosspane_idd_v1.h"
#include <array>
#include <cstddef>
#include <cstdint>

namespace crosspane::idd {
enum class Decision { Accepted, Invalid, Expired, Busy, NotFound, Capacity, Stopped };
enum class Operation { Add, Remove, List, Heartbeat };
struct DecodedRequest {
    Operation operation{};
    CPD_HEADER header{};
    CPD_MODE mode{};
    std::uint64_t value{};
};
bool valid_mode(const CPD_MODE& mode) noexcept;
Decision validate_request(std::uint32_t ioctl, const void* bytes, std::size_t length,
                          std::size_t output_capacity, DecodedRequest& decoded) noexcept;
void initialize_header(CPD_HEADER& header, std::uint32_t bytes, std::uint64_t request_id) noexcept;

inline constexpr std::uint32_t NoSlot = UINT32_MAX;
struct ClientToken { std::uint32_t slot{NoSlot}; std::uint64_t generation{}; };
struct MonitorToken { std::uint32_t slot{NoSlot}; std::uint64_t generation{}; std::uint64_t id{}; };
struct CommandToken { std::uint32_t slot{NoSlot}; std::uint64_t generation{}; };
bool same(ClientToken a, ClientToken b) noexcept;
bool same(MonitorToken a, MonitorToken b) noexcept;
enum class EffectKind { Create, Discard, Depart };
struct EffectToken { MonitorToken monitor{}; EffectKind kind{}; std::uint64_t epoch{}; };
enum class MonitorPhase { Free, Reserved, Creating, AwaitingReply, Active, RetirePending, Retiring };
struct Admission { Decision decision{Decision::Invalid}; ClientToken client{}; };
struct CommandAdmission { Decision decision{Decision::Invalid}; CommandToken command{}; };
struct AddAdmission { Decision decision{Decision::Invalid}; MonitorToken monitor{}; };
struct EffectAdmission { Decision decision{Decision::Invalid}; EffectToken effect{}; };
struct Completion { bool claimed{}; Decision decision{Decision::Invalid}; MonitorToken monitor{}; };
struct ListSnapshot { Decision decision{Decision::Invalid}; std::uint32_t remaining_ms{}; std::uint32_t count{}; MonitorToken monitor{}; CPD_MODE mode{}; };
struct MonitorSnapshot { bool found{}; MonitorPhase phase{MonitorPhase::Free}; ClientToken owner{}; CPD_MODE mode{}; bool created{}; bool arrived{}; };

// Caller serializes this core using a short state lock. Returned tickets contain no native
// authority. Adapter must prove pre-Cleanup quiescence around every DDI; checking a ticket
// alone is NOT a check-then-use lifetime proof. No lock may span a native effect/completion.
// ClientToken is minted only for an actual admitted WDFFILEOBJECT, not a wire/PID token.
// Copying it models duplicated handles sharing one file capability; cleanup is the actual
// last-handle framework event, not inferred process death or a helper reference decrement.
class LeaseCore {
public:
    explicit LeaseCore(std::uint64_t first_id = 1, std::uint64_t first_generation = 1) noexcept;
    Admission open(std::uint64_t now) noexcept;
    Decision check(ClientToken client, std::uint64_t now) noexcept;
    Decision heartbeat(ClientToken client, std::uint64_t now, std::uint64_t sequence) noexcept;
    ListSnapshot list(ClientToken client, std::uint64_t now) noexcept;
    CommandAdmission admit_command(ClientToken client, std::uint64_t now) noexcept;
    AddAdmission reserve_add(CommandToken command, std::uint64_t now, const CPD_MODE& mode) noexcept;
    EffectAdmission begin_create(CommandToken command, std::uint64_t now) noexcept;
    Decision complete_create(EffectToken effect, std::uint64_t now, bool created, bool arrived) noexcept;
    Decision reserve_remove(CommandToken command, std::uint64_t now, std::uint64_t id) noexcept;
    Decision cancel(CommandToken command, std::uint64_t now) noexcept;
    Completion claim_completion(CommandToken command, std::uint64_t now, bool deliver_success) noexcept;
    Decision release_command(CommandToken command) noexcept;
    MonitorToken retirement_candidate() const noexcept;
    EffectAdmission begin_retirement(MonitorToken monitor) noexcept;
    Decision complete_retirement(EffectToken effect, bool succeeded) noexcept;
    Decision cleanup(ClientToken client, std::uint64_t now) noexcept;
    Decision expire(std::uint64_t now) noexcept;
    Decision stop(std::uint64_t now) noexcept;
    Decision restart(std::uint64_t now) noexcept;
    Decision framework_cleanup() noexcept;
    bool effect_authorized(EffectToken effect, std::uint64_t now) noexcept;
    bool quiescent() const noexcept;
    MonitorSnapshot snapshot(MonitorToken monitor) const noexcept;
    std::uint32_t remaining(ClientToken client, std::uint64_t now) const noexcept;
private:
    enum class FilePhase { Free, Open, Expired, Closing, Retired };
    struct File {
        FilePhase phase{FilePhase::Free}; std::uint64_t generation{};
        std::uint64_t deadline{}; std::uint64_t sequence{};
    };
    struct Command {
        bool occupied{}; std::uint64_t generation{}; ClientToken client{};
        MonitorToken monitor{}; bool cancelled{}; bool completed{};
        bool ready{true}; Decision result{Decision::Accepted};
    };
    struct Monitor {
        MonitorPhase phase{MonitorPhase::Free}; std::uint64_t generation{};
        std::uint64_t id{}; ClientToken owner{}; CPD_MODE mode{}; CommandToken command{};
        bool created{}; bool arrived{}; std::uint64_t effect_epoch{};
        EffectKind effect_kind{};
    };
    std::array<File, CPD_MAX_OPENS> files_{};
    std::array<Command, CPD_MAX_COMMANDS> commands_{};
    std::array<Monitor, CPD_MAX_MONITORS> monitors_{};
    std::uint64_t next_id_{}; std::uint64_t next_generation_{};
    std::uint64_t now_{}; std::uint64_t epoch_{1};
    bool running_{true}; bool native_allowed_{true}; bool teardown_failed_{};
    File* find(ClientToken client) noexcept;
    const File* find(ClientToken client) const noexcept;
    Command* find(CommandToken command) noexcept;
    const Command* find(CommandToken command) const noexcept;
    Monitor* find(MonitorToken monitor) noexcept;
    const Monitor* find(MonitorToken monitor) const noexcept;
    Decision advance(std::uint64_t now) noexcept;
    Decision client_decision(ClientToken client) const noexcept;
    std::uint64_t generation() noexcept;
    void invalidate(ClientToken client, FilePhase phase) noexcept;
    void retire(Monitor& monitor) noexcept;
    void drop_monitor(Monitor& monitor, Decision result) noexcept;
    void reap_files() noexcept;
};
} // namespace crosspane::idd
