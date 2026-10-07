/*++

Copyright (c) Microsoft Corporation

Abstract:

    Crosspane private IDD admission/lifetime core and guarded native adapter.
    Retains the Microsoft notice from the admitted MIT VirtualDrivers fork.
    Source/static review is separate from unperformed native activation.

Environment:

    User Mode, UMDF; portable model remains OS-free

--*/

#include "Driver.h"

#include <limits>

namespace crosspane::idd {

bool DeviceAdmission::publish_ready(AdmissionEpoch starting_epoch) {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ != DevicePhase::Starting || generation_exhausted_ ||
        starting_epoch.generation != generation_) {
        return false;
    }
    phase_ = DevicePhase::Ready;
    return true;
}

bool DeviceAdmission::acquire_epoch(AdmissionEpoch& result) const {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ != DevicePhase::Ready || generation_exhausted_) {
        return false;
    }
    result.generation = generation_;
    return true;
}

bool DeviceAdmission::accepts(AdmissionEpoch epoch) const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return phase_ == DevicePhase::Ready && !generation_exhausted_ &&
           epoch.generation == generation_;
}

void DeviceAdmission::begin_stop() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ == DevicePhase::Stopping || phase_ == DevicePhase::Stopped) {
        return;
    }
    phase_ = DevicePhase::Stopping;
    if (generation_ == std::numeric_limits<std::uint64_t>::max()) {
        generation_exhausted_ = true;
    } else {
        ++generation_;
    }
}

void DeviceAdmission::finish_stop(bool effects_quiescent,
                                  bool processors_exited) {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ == DevicePhase::Stopping && effects_quiescent &&
        processors_exited) {
        phase_ = DevicePhase::Stopped;
    }
}

bool DeviceAdmission::restart() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (phase_ != DevicePhase::Stopped || generation_exhausted_) {
        return false;
    }
    phase_ = DevicePhase::Starting;
    return true;
}

DevicePhase DeviceAdmission::phase() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return phase_;
}

AdmissionEpoch DeviceAdmission::generation() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return AdmissionEpoch{generation_};
}

} // namespace crosspane::idd

#if defined(_WIN32)
#include "Control.h"
#include "Monitor.h"
#include <new>

namespace crosspane::idd {
const GUID ControlInterface = {0xfea027fb, 0x8535, 0x40eb, {0xb8,0x87,0x14,0x01,0xd6,0x3e,0xf2,0x73}};
EVT_WDF_DRIVER_DEVICE_ADD cpd_device_add;
EVT_WDF_DEVICE_D0_ENTRY cpd_d0_entry;
EVT_WDF_DEVICE_D0_EXIT cpd_d0_exit;
EVT_WDF_DEVICE_RELEASE_HARDWARE cpd_release_hardware;

namespace {
// Synchronization only, not a global owner/device identity registry. The CRT
// constructs this existing-runtime mutex before DriverEntry. No C++ mutex is
// placed into raw, zero-initialized WDF context memory.
std::mutex adapter_mailbox_lock;
struct AdapterDelivery {
    NativeDevice* owner{};
    WDFDEVICE owner_device{};
    IDDCX_ADAPTER adapter{};
    AdmissionEpoch generation{};
    NTSTATUS status{STATUS_SUCCESS};
};
AdapterDelivery claim_adapter_delivery(AdapterContext& context, IDDCX_ADAPTER adapter) {
    // Caller holds adapter_mailbox_lock. Cleanup cannot clear this binding
    // between acquiring the exact parent device memory pin and copying owner.
    if (context.cleaned || context.delivered || !context.finished || !context.owner) return {};
    context.delivered = true;
    // ROOT narrow exception: nonblocking reference only under mailbox mutex.
    // It prevents device Destroy/NativeDevice deletion, NOT device Cleanup.
    WdfObjectReference(context.owner_device);
    return {context.owner, context.owner_device, adapter, context.generation, context.finished_status};
}
void deliver_adapter_result(const AdapterDelivery& delivery) {
    if (!delivery.owner) return;
    {
        auto* owner = delivery.owner;
        const std::lock_guard<std::mutex> lock(owner->state_lock);
        // Memory pin is not DDI permission. The pre-cleanup stop gate and the
        // exact initiating generation independently prevent late effects.
        if (!owner->stopping && !owner->cleanup_started && !owner->native_failed &&
            owner->initializing && owner->adapter == delivery.adapter &&
            delivery.generation.generation == owner->adapter_epoch.generation &&
            delivery.generation.generation == owner->admission.generation().generation) {
            owner->initializing = false;
            if (!NT_SUCCESS(delivery.status) || !owner->admission.publish_ready(delivery.generation)) {
                owner->native_failed = true;
                owner->admission.begin_stop();
                (void)owner->core.stop(GetTickCount64());
            } else {
                (void)WdfTimerStart(owner->timer, WDF_REL_TIMEOUT_IN_MS(CPD_EXPIRY_SCAN_MS));
            }
        }
    }
    // No owner/context access follows this final reference release. Destroy
    // can run before Dereference returns. No mailbox/state lock is held.
    WdfObjectDereference(delivery.owner_device);
}
EVT_WDF_OBJECT_CONTEXT_CLEANUP cleanup_adapter_mailbox;
_Use_decl_annotations_
void cleanup_adapter_mailbox(WDFOBJECT object) {
    NativeDevice* owner = nullptr;
    WDFDEVICE owner_device = nullptr;
    {
        const std::lock_guard<std::mutex> lock(adapter_mailbox_lock);
        auto* context = adapter_context(object);
        context->cleaned = true;
        owner = context->owner;
        owner_device = context->owner_device;
        if (owner) WdfObjectReference(owner_device);
        context->owner = nullptr;
        context->owner_device = nullptr;
        // A cleaned context is never rebound or used to claim another result.
    }
    if (owner) {
        {
            const std::lock_guard<std::mutex> lock(owner->state_lock);
            if (owner->adapter == reinterpret_cast<IDDCX_ADAPTER>(object)) owner->adapter = nullptr;
        }
        WdfObjectDereference(owner_device);
    }
}
} // namespace

NTSTATUS NativeDevice::start() {
    AdmissionEpoch initiating_epoch{};
    {
        const std::lock_guard<std::mutex> lock(state_lock);
        if (cleanup_started || native_failed || initializing ||
            in_flight_adapter_epoch.generation != 0) return STATUS_DEVICE_NOT_READY;
        if (admission.phase() == DevicePhase::Stopped) {
            if (core.restart(GetTickCount64()) != Decision::Accepted || !admission.restart())
                return STATUS_DEVICE_NOT_READY;
        }
        stopping = false;
        stop_complete = false;
        adapter_epoch = admission.generation();
        if (adapter) {
            if (!admission.publish_ready(adapter_epoch)) return STATUS_DEVICE_NOT_READY;
            (void)WdfTimerStart(timer, WDF_REL_TIMEOUT_IN_MS(CPD_EXPIRY_SCAN_MS));
            return STATUS_SUCCESS;
        }
        initializing = true;
        initiating_epoch = adapter_epoch;
        in_flight_adapter_epoch = initiating_epoch;
    }
    IDDCX_ADAPTER_CAPS caps{};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = CPD_MAX_MONITORS;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = L"Crosspane IDD twin v1";
    caps.EndPointDiagnostics.pEndPointManufacturerName = L"Crosspane";
    caps.EndPointDiagnostics.pEndPointModelName = L"CrosspaneIdd";
    IDDCX_ENDPOINT_VERSION version{};
    version.Size = sizeof(version);
    version.MajorVer = 1;
    caps.EndPointDiagnostics.pFirmwareVersion = &version;
    caps.EndPointDiagnostics.pHardwareVersion = &version;
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, AdapterContext);
    attributes.ParentObject = device;
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    attributes.SynchronizationScope = WdfSynchronizationScopeNone;
    attributes.EvtCleanupCallback = cleanup_adapter_mailbox;
    IDARG_IN_ADAPTER_INIT input{};
    input.WdfDevice = device;
    input.pCaps = &caps;
    input.ObjectAttributes = &attributes;
    IDARG_OUT_ADAPTER_INIT output{};
    const NTSTATUS status = IddCxAdapterInitAsync(&input, &output);
    AdapterDelivery delivery{};
    bool binding_refused = false;
    if (NT_SUCCESS(status)) {
        {
            const std::lock_guard<std::mutex> lock(state_lock);
            adapter = output.AdapterObject;
        }
        {
            const std::lock_guard<std::mutex> lock(adapter_mailbox_lock);
            auto* context = adapter_context(output.AdapterObject);
            if (context->cleaned || context->owner || context->generation.generation != 0) {
                binding_refused = true;
            } else {
                // Output binds THIS exact adapter to the captured initiating
                // epoch. An early callback only recorded its genuine status;
                // it never guessed owner or borrowed a current mutable epoch.
                context->owner = this;
                context->owner_device = device;
                context->generation = initiating_epoch;
                delivery = claim_adapter_delivery(*context, output.AdapterObject);
            }
        }
    }
    {
        const std::lock_guard<std::mutex> lock(state_lock);
        if (!NT_SUCCESS(status) || binding_refused) {
            initializing = false;
            native_failed = true;
            admission.begin_stop();
            (void)core.stop(GetTickCount64());
            if (binding_refused) adapter = nullptr;
        }
        // A second start cannot overlap Async or output owner/epoch binding.
        in_flight_adapter_epoch = {};
    }
    deliver_adapter_result(delivery);
    return binding_refused ? STATUS_DEVICE_NOT_READY : status;
}
NTSTATUS NativeDevice::stop() {
    {
        std::unique_lock<std::mutex> lock(state_lock);
        if (stop_complete) return stop_status;
        stopping = true;
        admission.begin_stop();
        (void)core.stop(GetTickCount64());
        // LEAD6c268 explicitly admits this passive setup-count barrier. Each
        // admitted IOCTL setup performs final registration/enqueue before its
        // last decrement. SynchronizationScope=None avoids holding a WDF
        // automatic-serialization lock needed by callbacks finishing here.
        // <=16 admitted setups; OS callback/wait latency is not hard bounded.
        setups_finished.wait(lock, [this] { return setups_in_flight == 0; });
    }
    if (timer) (void)WdfTimerStop(timer, TRUE);
    if (work) {
        WdfWorkItemEnqueue(work); // final drain after every producer gate is closed
        WdfWorkItemFlush(work);  // accepted framework barrier; no timeout/free
    }
    // Surviving processors (including a failed native retirement) are joined
    // before D0Exit/ReleaseHardware returns, never from late EvtCleanup.
    bool processors_exited = true;
    for (std::uint32_t index = 0; index < monitors.size(); ++index) {
        NativeMonitor* monitor = nullptr;
        { const std::lock_guard<std::mutex> lock(state_lock); monitor = monitors[index]; }
        if (monitor) {
            monitor->close_processor_admission(); monitor->unassign();
            processors_exited = processors_exited && monitor->processor_absent();
        }
    }
    {
        const std::lock_guard<std::mutex> lock(state_lock);
        const bool quiescent = core.quiescent();
        admission.finish_stop(quiescent, processors_exited);
        stop_status = quiescent && processors_exited && !native_failed ? STATUS_SUCCESS : STATUS_DEVICE_NOT_READY;
        stop_complete = true;
        return stop_status;
    }
}
_Use_decl_annotations_
NTSTATUS cpd_adapter_finished(IDDCX_ADAPTER adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* input) {
    if (!input) return STATUS_INVALID_PARAMETER;
    AdapterDelivery delivery{};
    {
        const std::lock_guard<std::mutex> lock(adapter_mailbox_lock);
        auto* context = adapter_context(adapter);
        if (context->cleaned || context->finished) return STATUS_SUCCESS;
        context->finished_status = input->AdapterInitStatus;
        context->finished = true;
        delivery = claim_adapter_delivery(*context, adapter);
    }
    // Early/unbound notification only records status and acknowledges receipt.
    // Owner publication or a late notification claims delivery exactly once.
    // STATUS_SUCCESS is that acknowledgement, never a fabricated Ready state.
    deliver_adapter_result(delivery);
    return STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_d0_entry(WDFDEVICE device, WDF_POWER_DEVICE_STATE) {
    auto* owner = device_context(device)->value;
    return owner ? owner->start() : STATUS_DEVICE_NOT_READY;
}
_Use_decl_annotations_
NTSTATUS cpd_d0_exit(WDFDEVICE device, WDF_POWER_DEVICE_STATE) {
    auto* owner = device_context(device)->value;
    return owner ? owner->stop() : STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_release_hardware(WDFDEVICE device, WDFCMRESLIST) {
    auto* owner = device_context(device)->value;
    return owner ? owner->stop() : STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_device_add(WDFDRIVER, PWDFDEVICE_INIT initialization) {
    WDF_PNPPOWER_EVENT_CALLBACKS power;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&power);
    power.EvtDeviceD0Entry = cpd_d0_entry;
    power.EvtDeviceD0Exit = cpd_d0_exit;
    power.EvtDeviceReleaseHardware = cpd_release_hardware;
    WdfDeviceInitSetPnpPowerEventCallbacks(initialization, &power);
    WDF_FILEOBJECT_CONFIG files;
    WDF_FILEOBJECT_CONFIG_INIT(&files, cpd_file_create, cpd_file_close, cpd_file_cleanup);
    files.AutoForwardCleanupClose = WdfFalse;
    WDF_OBJECT_ATTRIBUTES file_attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&file_attributes, FileContext);
    file_attributes.ExecutionLevel = WdfExecutionLevelPassive;
    file_attributes.SynchronizationScope = WdfSynchronizationScopeNone;
    WdfDeviceInitSetFileObjectConfig(initialization, &files, &file_attributes);
    IDD_CX_CLIENT_CONFIG client;
    IDD_CX_CLIENT_CONFIG_INIT(&client);
    client.EvtIddCxDeviceIoControl = cpd_device_io_control;
    client.EvtIddCxAdapterInitFinished = cpd_adapter_finished;
    client.EvtIddCxAdapterCommitModes = cpd_commit_modes;
    client.EvtIddCxParseMonitorDescription = cpd_parse_description;
    client.EvtIddCxMonitorGetDefaultDescriptionModes = cpd_default_modes;
    client.EvtIddCxMonitorQueryTargetModes = cpd_target_modes;
    client.EvtIddCxMonitorAssignSwapChain = cpd_assign_swapchain;
    client.EvtIddCxMonitorUnassignSwapChain = cpd_unassign_swapchain;
    NTSTATUS status = IddCxDeviceInitConfig(initialization, &client);
    if (!NT_SUCCESS(status)) return status;
    WDF_OBJECT_ATTRIBUTES device_attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&device_attributes, DeviceContext);
    device_attributes.ExecutionLevel = WdfExecutionLevelPassive;
    device_attributes.SynchronizationScope = WdfSynchronizationScopeNone;
    device_attributes.EvtCleanupCallback = [](WDFOBJECT object) {
        auto* owner = device_context(object)->value;
        if (owner) {
            const std::lock_guard<std::mutex> lock(owner->state_lock);
            owner->cleanup_started = true;
            owner->admission.begin_stop();
            (void)owner->core.framework_cleanup();
            // No worker/DDI/late join. Pre-cleanup stop/flush joined processors
            // and completed all owned requests before this callback.
        }
    };
    device_attributes.EvtDestroyCallback = [](WDFOBJECT object) {
        auto* context = device_context(object);
        delete context->value;
        context->value = nullptr;
    };
    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&initialization, &device_attributes, &device);
    if (!NT_SUCCESS(status)) return status;
    auto* owner = new (std::nothrow) NativeDevice(device);
    if (!owner) return STATUS_INSUFFICIENT_RESOURCES;
    device_context(device)->value = owner;
    WDF_OBJECT_ATTRIBUTES child;
    WDF_OBJECT_ATTRIBUTES_INIT(&child);
    child.ParentObject = device;
    child.ExecutionLevel = WdfExecutionLevelPassive;
    child.SynchronizationScope = WdfSynchronizationScopeNone;
    WDF_WORKITEM_CONFIG management;
    WDF_WORKITEM_CONFIG_INIT(&management, cpd_management);
    management.AutomaticSerialization = FALSE;
    status = WdfWorkItemCreate(&management, &child, &owner->work);
    if (!NT_SUCCESS(status)) return status;
    WDF_TIMER_CONFIG expiry;
    WDF_TIMER_CONFIG_INIT(&expiry, cpd_expiry); // passive one-shot, Period=0
    expiry.AutomaticSerialization = FALSE;
    status = WdfTimerCreate(&expiry, &child, &owner->timer);
    if (!NT_SUCCESS(status)) return status;
    status = WdfDeviceCreateDeviceInterface(device, &ControlInterface, nullptr);
    if (!NT_SUCCESS(status)) return status;
    return IddCxDeviceInitialize(device);
}
} // namespace crosspane::idd
extern "C" DRIVER_INITIALIZE DriverEntry;
_Use_decl_annotations_
extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT object, PUNICODE_STRING path) {
    WDF_DRIVER_CONFIG configuration;
    WDF_DRIVER_CONFIG_INIT(&configuration, crosspane::idd::cpd_device_add);
    return WdfDriverCreate(object, path, WDF_NO_OBJECT_ATTRIBUTES, &configuration, WDF_NO_HANDLE);
}
extern "C" BOOL WINAPI DllMain(HINSTANCE, DWORD, void*) { return TRUE; }
#endif
