#include "Control.h"

namespace crosspane::idd {
PreparedControl prepare_control_request(DeviceAdmission& admission, LeaseCore& core,
    ClientToken client, std::uint64_t now, std::uint32_t ioctl, const void* bytes,
    std::size_t length, std::size_t output_capacity) noexcept {
    PreparedControl result{};
    result.client = client;
    result.decision = validate_request(ioctl, bytes, length, output_capacity, result.decoded);
    if (result.decision != Decision::Accepted) return result;
    if (!admission.acquire_epoch(result.admission)) {
        result.decision = Decision::Stopped;
        return result;
    }
    const auto admitted = core.admit_command(client, now);
    result.decision = admitted.decision;
    if (result.decision != Decision::Accepted) return result;
    result.command = admitted.command;
    if (result.decoded.operation == Operation::Add) {
        const auto added = core.reserve_add(result.command, now, result.decoded.mode);
        result.decision = added.decision;
        result.monitor = added.monitor;
    } else if (result.decoded.operation == Operation::Remove) {
        result.decision = core.reserve_remove(result.command, now, result.decoded.value);
    }
    // Heartbeat and list are only prepared here; their state transition and
    // wire reply require the native adapter's actual file/request authority.
    if (result.decision != Decision::Accepted) {
        // No effect was started. Preserve the typed refusal; never hide a live
        // command if an unexpected invariant prevents its release.
        const auto refusal = result.decision;
        (void)core.cancel(result.command, now);
        const auto completion = core.claim_completion(result.command, now, false);
        if (!completion.claimed || core.release_command(result.command) != Decision::Accepted) {
            result.decision = Decision::Stopped;
        } else {
            result.command = {};
            result.decision = refusal;
        }
    }
    return result;
}
} // namespace crosspane::idd

#if defined(_WIN32)
#include "Monitor.h"
#include <cstring>

namespace crosspane::idd {
NTSTATUS decision_status(Decision decision) noexcept {
    switch (decision) {
    case Decision::Accepted: return STATUS_SUCCESS;
    case Decision::Expired: return STATUS_IO_TIMEOUT;
    case Decision::Busy: return STATUS_DEVICE_BUSY;
    case Decision::NotFound: return STATUS_NOT_FOUND;
    case Decision::Capacity: return STATUS_INSUFFICIENT_RESOURCES;
    case Decision::Stopped: return STATUS_DEVICE_NOT_READY;
    default: return STATUS_INVALID_PARAMETER;
    }
}
void NativeDevice::enqueue_locked() {
    if (!stopping && !cleanup_started && work) {
        // WdfWorkItemEnqueue schedules asynchronously; it does not call our
        // worker inline. Keep this nonblocking enqueue within admission lock
        // so pre-cleanup stop cannot flush before a producer's last enqueue.
        WdfWorkItemEnqueue(work);
    }
}
namespace {
class SetupExit final {
public:
    explicit SetupExit(NativeDevice& owner) : owner_(owner) {}
    ~SetupExit() {
        const std::lock_guard<std::mutex> lock(owner_.state_lock);
        --owner_.setups_in_flight;
        owner_.setups_finished.notify_all();
        // No request/device DDI or enqueue occurs after this final decrement.
    }
private:
    NativeDevice& owner_;
};
bool same_command(CommandToken a, CommandToken b) {
    return a.slot == b.slot && a.generation == b.generation;
}
std::size_t response_size(Operation operation) {
    return operation == Operation::List ? sizeof(CPD_LIST_RESPONSE) : sizeof(CPD_ADD_RESPONSE);
}

}

_Use_decl_annotations_
void cpd_file_create(WDFDEVICE device, WDFREQUEST request, WDFFILEOBJECT file) {
    auto* owner = device_context(device)->value;
    // The graphics kernel opens the adapter during adapter start, before the CPD
    // admission epoch is ready. Only user-mode opens are CPD clients, so a kernel
    // open gets an inert file (cleaned, no client, no lease, no open count).
    if (WdfRequestGetRequestorMode(request) == KernelMode) {
        if (file) {
            auto* context = file_context(file);
            context->device = owner;
            context->client = {};
            context->cleaned = true;
        }
        WdfRequestComplete(request, STATUS_SUCCESS);
        return;
    }
    NTSTATUS status = STATUS_DEVICE_NOT_READY;
    if (owner && file) {
        auto* context = file_context(file);
        context->device = owner;
        context->client = {};
        context->cleaned = true;
        {
            const std::lock_guard<std::mutex> lock(owner->state_lock);
            AdmissionEpoch ready{};
            if (!owner->stopping && !owner->cleanup_started && owner->admitted_files < CPD_MAX_OPENS &&
                owner->admission.acquire_epoch(ready)) {
                const auto opened = owner->core.open(GetTickCount64());
                status = decision_status(opened.decision);
                if (opened.decision == Decision::Accepted) {
                    context->client = opened.client;
                    context->cleaned = false;
                    ++owner->admitted_files;
                }
            }
        }
    }
    WdfRequestComplete(request, status);
}
_Use_decl_annotations_
void cpd_file_cleanup(WDFFILEOBJECT file) {
    auto* context = file_context(file);
    auto* owner = context->device;
    if (!owner) return;
    const std::lock_guard<std::mutex> lock(owner->state_lock);
    if (!context->cleaned) {
        context->cleaned = true;
        if (owner->admitted_files != 0) --owner->admitted_files;
        (void)owner->core.cleanup(context->client, GetTickCount64());
        owner->enqueue_locked();
    }
}
_Use_decl_annotations_
void cpd_file_close(WDFFILEOBJECT file) {
    // FileCleanup is the actual last-handle event. Close does not synthesize a
    // process-death/release count or initiate a second native retirement.
    auto* context = file_context(file);
    context->device = nullptr;
}

_Use_decl_annotations_
void cpd_device_io_control(WDFDEVICE handle, WDFREQUEST request, size_t output_length,
                           size_t input_length, ULONG ioctl) {
    auto* owner = device_context(handle)->value;
    if (!owner) { WdfRequestComplete(request, STATUS_DEVICE_NOT_READY); return; }
    bool admitted = false;
    {
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        AdmissionEpoch epoch{};
        if (!owner->stopping && !owner->cleanup_started && owner->admission.acquire_epoch(epoch) &&
            owner->setups_in_flight < CPD_MAX_COMMANDS) {
            ++owner->setups_in_flight;
            admitted = true;
        }
    }
    if (!admitted) { WdfRequestComplete(request, STATUS_DEVICE_NOT_READY); return; }
    SetupExit setup_exit(*owner);
    void* input = nullptr;
    NTSTATUS status = WdfRequestRetrieveInputBuffer(request, input_length, &input, nullptr);
    if (!NT_SUCCESS(status)) { WdfRequestComplete(request, status); return; }
    const WDFFILEOBJECT file = WdfRequestGetFileObject(request);
    if (!file) { WdfRequestComplete(request, STATUS_INVALID_HANDLE); return; }
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, RequestContext);
    RequestContext* context = nullptr;
    status = WdfObjectAllocateContext(request, &attributes, reinterpret_cast<void**>(&context));
    if (!NT_SUCCESS(status)) { WdfRequestComplete(request, status); return; }
    PreparedControl prepared{};
    {
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        const auto* file_state = file_context(file);
        if (file_state->device != owner || file_state->cleaned || owner->stopping || owner->cleanup_started) {
            status = STATUS_INVALID_HANDLE;
        } else {
            prepared = prepare_control_request(owner->admission, owner->core,
                file_state->client, GetTickCount64(), ioctl, input, input_length, output_length);
            status = decision_status(prepared.decision);
            if (prepared.decision != Decision::Accepted && prepared.command.slot != NoSlot) {
                owner->native_failed = true; owner->admission.begin_stop();
                (void)owner->core.stop(GetTickCount64()); owner->enqueue_locked();
            }
        }
    }
    if (!NT_SUCCESS(status)) { WdfRequestComplete(request, status); return; }
    context->device = owner;
    context->command = prepared.command;
    // Retain these exact WDF objects until this command's completion, never
    // PID/token lookup. References alone are not post-Cleanup DDI permission.
    WdfObjectReference(request);
    WdfObjectReference(file);
    {
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        auto& slot = owner->requests[prepared.command.slot];
        slot = {};
        slot.request = request;
        slot.file = file;
        slot.prepared = prepared;
        slot.registering = true;
    }
    status = WdfRequestMarkCancelableEx(request, cpd_request_cancel);
    {
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        auto& slot = owner->requests[prepared.command.slot];
        slot.registering = false;
        slot.marked = NT_SUCCESS(status);
        if (!NT_SUCCESS(status)) {
            slot.cancelled = status == STATUS_CANCELLED;
            slot.cancel_seen = true; // failed registration: callback not called
            slot.native_status = status;
            (void)owner->core.cancel(prepared.command, GetTickCount64());
        }
        owner->enqueue_locked();
    }
    // SetupExit is last: stop waits for every admitted setup's registration
    // and final enqueue. It cannot begin its final Flush before this exit.
}
_Use_decl_annotations_
void cpd_request_cancel(WDFREQUEST request) {
    auto* context = request_context(request);
    auto* owner = context->device;
    if (!owner) return;
    {
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        const auto token = context->command;
        if (token.slot < owner->requests.size()) {
            auto& slot = owner->requests[token.slot];
            if (slot.request == request && same_command(slot.prepared.command, token)) {
                slot.cancelled = true;
                (void)owner->core.cancel(token, GetTickCount64());
                owner->enqueue_locked();
                // This is the callback's final request/model access. A work
                // item may finish cancellation only after observing this bit.
                slot.cancel_seen = true;
                owner->setups_finished.notify_all();
            }
        }
    }
    // Do not complete/free an in-flight effect or touch request after notify.
}

void NativeDevice::run_management() {
    // WorkItem callbacks may overlap after re-enqueue. This executor mutex is
    // separate from state_lock and never acquired by setup/cancel/PnP stop.
    const std::lock_guard<std::mutex> executor(work_lock);
    { const std::lock_guard<std::mutex> lock(state_lock);
      if (cleanup_started) return;
      (void)core.expire(GetTickCount64()); }
    for (std::uint32_t index = 0; index < requests.size(); ++index) {
        EffectAdmission effect{};
        CPD_MODE mode{};
        {
            const std::lock_guard<std::mutex> lock(state_lock);
            auto& slot = requests[index];
            if (!slot.request || slot.registering || slot.effect_started || slot.completion_claimed) continue;
            slot.effect_started = true;
            if (slot.cancelled) {
                (void)core.cancel(slot.prepared.command, GetTickCount64());
                continue; // cancelled HB must not extend the file lease
            }
            const auto operation = slot.prepared.decoded.operation;
            if (operation == Operation::Add) {
                effect = core.begin_create(slot.prepared.command, GetTickCount64());
                mode = slot.prepared.decoded.mode;
                if (effect.decision != Decision::Accepted)
                    (void)core.cancel(slot.prepared.command, GetTickCount64());
            } else if (operation == Operation::Heartbeat) {
                const auto result = core.heartbeat(slot.prepared.client, GetTickCount64(), slot.prepared.decoded.value);
                slot.native_status = decision_status(result);
                if (result != Decision::Accepted) (void)core.cancel(slot.prepared.command, GetTickCount64());
            }
        }
        if (effect.decision == Decision::Accepted && effect.effect.monitor.slot != NoSlot) {
            bool arrived = false;
            NTSTATUS status = STATUS_DEVICE_NOT_READY;
            const bool created = create_monitor(effect.effect, mode, arrived, status);
            const std::lock_guard<std::mutex> lock(state_lock);
            (void)core.complete_create(effect.effect, GetTickCount64(), created, arrived);
            requests[index].native_status = status;
        }
    }
    const auto retire_pending = [this]() {
        // At most four effects per pass; failure is sticky, never a retry loop.
        for (std::uint32_t i = 0; i < CPD_MAX_MONITORS; ++i) {
            EffectAdmission effect{};
            {
                const std::lock_guard<std::mutex> lock(state_lock);
                const auto monitor = core.retirement_candidate();
                if (monitor.slot == NoSlot) break;
                effect = core.begin_retirement(monitor);
            }
            if (effect.decision != Decision::Accepted) break;
            const bool retired = retire_monitor(effect.effect);
            {
                const std::lock_guard<std::mutex> lock(state_lock);
                const auto result = core.complete_retirement(effect.effect, retired);
                if (result != Decision::Accepted) {
                    native_failed = true;
                    admission.begin_stop();
                }
            }
            if (!retired) break;
        }
    };
    retire_pending();
    for (std::uint32_t index = 0; index < requests.size(); ++index) {
        WDFREQUEST request = nullptr;
        bool unmark = false;
        {
            const std::lock_guard<std::mutex> lock(state_lock);
            auto& slot = requests[index];
            if (!slot.request || slot.registering || slot.completion_claimed) continue;
            request = slot.request;
            unmark = slot.marked;
            slot.unmarking = true;
        }
        if (unmark) {
            // Never under state/automatic-serialization lock: Unmark can wait
            // for a concurrently running cancellation callback.
            const auto status = WdfRequestUnmarkCancelable(request);
            std::unique_lock<std::mutex> lock(state_lock);
            auto& slot = requests[index];
            slot.marked = false;
            if (status == STATUS_CANCELLED) {
                slot.cancelled = true;
                (void)core.cancel(slot.prepared.command, GetTickCount64());
                setups_finished.wait(lock, [&slot] { return slot.cancel_seen; });
            } else if (!NT_SUCCESS(status)) {
                slot.native_status = status;
                (void)core.cancel(slot.prepared.command, GetTickCount64());
            }
        }
        void* output = nullptr;
        std::size_t capacity = 0;
        const auto retrieved = WdfRequestRetrieveOutputBuffer(request, sizeof(CPD_ADD_RESPONSE), &output, &capacity);
        {
            const std::lock_guard<std::mutex> lock(state_lock);
            auto& slot = requests[index];
            const auto now = GetTickCount64();
            if (!NT_SUCCESS(retrieved) || capacity < response_size(slot.prepared.decoded.operation)) {
                slot.native_status = NT_SUCCESS(retrieved) ? STATUS_BUFFER_TOO_SMALL : retrieved;
                (void)core.cancel(slot.prepared.command, now);
            }
            const bool success = !slot.cancelled && NT_SUCCESS(slot.native_status) && !stopping &&
                admission.accepts(slot.prepared.admission);
            slot.completion = core.claim_completion(slot.prepared.command, now, success);
            slot.completion_claimed = slot.completion.claimed;
            slot.response_buffer = NT_SUCCESS(retrieved) ? output : nullptr;
            if (slot.completion.claimed && slot.completion.decision == Decision::Accepted)
                slot.reply_snapshot = core.list(slot.prepared.client, now);
        }
    }
    // A failed/cancelled/expired ADD reply must retire the actual created
    // monitor before request/file references or its command slot are released.
    retire_pending();
    for (std::uint32_t index = 0; index < requests.size(); ++index) {
        WDFREQUEST request = nullptr;
        WDFFILEOBJECT file = nullptr;
        PreparedControl prepared{};
        Completion completion{};
        NTSTATUS status = STATUS_DEVICE_NOT_READY;
        ListSnapshot list{};
        std::uint32_t remaining = 0;
        void* output = nullptr;
        {
            const std::lock_guard<std::mutex> lock(state_lock);
            auto& slot = requests[index];
            if (!slot.request || !slot.completion_claimed) continue;
            request = slot.request; file = slot.file; prepared = slot.prepared;
            completion = slot.completion;
            status = slot.cancelled ? STATUS_CANCELLED :
                (NT_SUCCESS(slot.native_status) ? decision_status(completion.decision) : slot.native_status);
            if (core.release_command(prepared.command) != Decision::Accepted)
                status = STATUS_DEVICE_NOT_READY; // retained failed effect, never fake Retired
            if (NT_SUCCESS(status)) {
                list = slot.reply_snapshot;
                remaining = list.remaining_ms;
                if (list.decision != Decision::Accepted) status = decision_status(list.decision);
            }
            output = slot.response_buffer;
            slot = {};
        }
        std::size_t bytes = 0;
        if (NT_SUCCESS(status)) {
            if (output) {
                const auto id = prepared.decoded.header.request_id;
                switch (prepared.decoded.operation) {
                case Operation::Add: {
                    CPD_ADD_RESPONSE reply{}; initialize_header(reply.header, sizeof(reply), id);
                    reply.monitor_id = completion.monitor.id; reply.remaining_ms = remaining;
                    std::memcpy(output, &reply, sizeof(reply)); bytes = sizeof(reply); break;
                }
                case Operation::Remove: {
                    CPD_REMOVE_RESPONSE reply{}; initialize_header(reply.header, sizeof(reply), id);
                    reply.monitor_id = prepared.decoded.value; reply.state = CPD_MONITOR_RETIRED;
                    std::memcpy(output, &reply, sizeof(reply)); bytes = sizeof(reply); break;
                }
                case Operation::List: {
                    CPD_LIST_RESPONSE reply{}; initialize_header(reply.header, sizeof(reply), id);
                    reply.count = list.count; reply.capacity = CPD_MONITORS_PER_OPEN; reply.remaining_ms = remaining;
                    if (list.count) { reply.entry.monitor_id = list.monitor.id; reply.entry.mode = list.mode;
                        reply.entry.state = CPD_MONITOR_ACTIVE; }
                    std::memcpy(output, &reply, sizeof(reply)); bytes = sizeof(reply); break;
                }
                case Operation::Heartbeat: {
                    CPD_HEARTBEAT_RESPONSE reply{}; initialize_header(reply.header, sizeof(reply), id);
                    reply.remaining_ms = remaining; reply.active_count = list.count;
                    reply.accepted_sequence = prepared.decoded.value;
                    std::memcpy(output, &reply, sizeof(reply)); bytes = sizeof(reply); break;
                }
                }
            } else {
                status = STATUS_INVALID_PARAMETER;
            }
        }
        WdfRequestCompleteWithInformation(request, status, bytes);
        WdfObjectDereference(file);
        WdfObjectDereference(request);
    }
}
_Use_decl_annotations_
void cpd_management(WDFWORKITEM work) {
    if (auto* owner = device_context(WdfWorkItemGetParentObject(work))->value)
        owner->run_management();
}
_Use_decl_annotations_
void cpd_expiry(WDFTIMER timer) {
    auto* owner = device_context(WdfTimerGetParentObject(timer))->value;
    if (!owner) return;
    const std::lock_guard<std::mutex> lock(owner->state_lock);
    if (!owner->stopping && !owner->cleanup_started) {
        (void)owner->core.expire(GetTickCount64());
        owner->enqueue_locked();
        // Passive timer is one-shot. Rearm inside the same producer gate;
        // stop closes the gate before TimerStop(TRUE), preventing late rearm.
        (void)WdfTimerStart(timer, WDF_REL_TIMEOUT_IN_MS(CPD_EXPIRY_SCAN_MS));
    }
}
} // namespace crosspane::idd
#endif
