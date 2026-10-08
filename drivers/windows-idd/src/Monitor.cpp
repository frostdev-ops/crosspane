/*++

Copyright (c) Microsoft Corporation

Abstract:

    Crosspane private IDD admission/lifetime core and guarded native adapter.
    Retains the Microsoft notice from the admitted MIT VirtualDrivers fork.
    Source/static review is separate from unperformed native activation.

Environment:

    User Mode, UMDF; portable model remains OS-free

--*/

#include "Monitor.h"

namespace crosspane::idd {

bool MonitorLifetime::begin_effect() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (admission_closed_ || framework_cleanup_started_ || protocol_failure_ ||
        effects_in_flight_ == MaxInFlight) {
        return false;
    }
    ++effects_in_flight_;
    return true;
}

void MonitorLifetime::end_effect() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (effects_in_flight_ == 0) {
        protocol_failure_ = true;
    } else {
        --effects_in_flight_;
    }
}

bool MonitorLifetime::begin_callback() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (framework_cleanup_started_ || protocol_failure_ ||
        callbacks_in_flight_ == MaxInFlight) {
        return false;
    }
    ++callbacks_in_flight_;
    return true;
}

void MonitorLifetime::end_callback() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (callbacks_in_flight_ == 0) {
        protocol_failure_ = true;
    } else {
        --callbacks_in_flight_;
    }
}

void MonitorLifetime::close_admission() {
    const std::lock_guard<std::mutex> lock(mutex_);
    admission_closed_ = true;
}

bool MonitorLifetime::note_framework_cleanup() {
    const std::lock_guard<std::mutex> lock(mutex_);
    admission_closed_ = true;
    framework_cleanup_started_ = true;
    if (effects_in_flight_ != 0 || callbacks_in_flight_ != 0 ||
        (processor_started_ && !processor_exit_observed_)) {
        protocol_failure_ = true;
    }
    return !protocol_failure_;
}

bool MonitorLifetime::note_processor_started() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (admission_closed_ || framework_cleanup_started_ || protocol_failure_ ||
        processor_started_) {
        return false;
    }
    processor_started_ = true;
    return true;
}

bool MonitorLifetime::note_processor_exit_observed() {
    const std::lock_guard<std::mutex> lock(mutex_);
    if (!processor_started_ || processor_exit_observed_) {
        protocol_failure_ = true;
        return false;
    }
    // Only a native adapter's actual retained-thread exit observation may call
    // this method. No timer, cancellation request or handle close is proof.
    processor_exit_observed_ = true;
    return !protocol_failure_;
}

bool MonitorLifetime::quiescent_locked() const {
    return admission_closed_ && effects_in_flight_ == 0 &&
           callbacks_in_flight_ == 0 &&
           (!processor_started_ || processor_exit_observed_) &&
           !protocol_failure_;
}

bool MonitorLifetime::quiescent() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return quiescent_locked();
}

LifetimeSnapshot MonitorLifetime::snapshot() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return LifetimeSnapshot{effects_in_flight_, callbacks_in_flight_,
                            admission_closed_, framework_cleanup_started_,
                            processor_started_, processor_exit_observed_,
                            protocol_failure_};
}

} // namespace crosspane::idd

#if defined(_WIN32)
#include "Edid.h"
#include <d3d11.h>
#include <dxgi1_4.h>
#include <wrl/client.h>
#include <new>
#include <cstring>

namespace crosspane::idd {
// Original Crosspane adapter code. Microsoft's MS-PL IddSampleDriver at
// d5569c08aa2818c6240744bb47a00f67f20fdb54 is a lifecycle reference only;
// no sample body is transplanted. LEAD c33e0ebe accepts OS-unbounded join.
class FrameProcessor final {
public:
    FrameProcessor(IDDCX_SWAPCHAIN chain, LUID render, HANDLE available)
        : chain_(chain), render_(render), available_(available) {}
    bool launch() {
        terminate_ = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!terminate_) return false;
        thread_ = CreateThread(nullptr, 0, entry, this, 0, nullptr);
        return thread_ != nullptr;
    }
    void join() {
        stop_.store(true, std::memory_order_release);
        if (terminate_) (void)SetEvent(terminate_);
        if (!thread_) return;
        // Each wait is <=100ms; the join has NO hard OS deadline. Never free,
        // detach or claim exit on a timeout/error. The retained creation handle
        // is immutable and only this object's destructor closes it after exit.
        for (;;) {
            const DWORD result = WaitForSingleObject(thread_, 100);
            if (result == WAIT_OBJECT_0) break;
            if (result == WAIT_FAILED) Sleep(100);
        }
    }
    ~FrameProcessor() {
        // The monitor calls join BEFORE deleting this object or departing.
        if (thread_) CloseHandle(thread_);
        if (terminate_) CloseHandle(terminate_);
    }
private:
    IDDCX_SWAPCHAIN chain_{};
    LUID render_{};
    HANDLE available_{}; // borrowed Cx event, never closed by this driver
    HANDLE terminate_{};
    HANDLE thread_{};
    std::atomic<bool> stop_{};
    static DWORD WINAPI entry(void* argument) {
        auto* self = static_cast<FrameProcessor*>(argument);
        self->consume();
        // Assign success transfers this exact chain. Delete only after its
        // processing has stopped, before the thread's actual exit/join.
        WdfObjectDelete(self->chain_);
        self->chain_ = nullptr;
        return 0;
    }
    void consume() {
        using Microsoft::WRL::ComPtr;
        if (stop_.load(std::memory_order_acquire)) return;
        ComPtr<IDXGIFactory4> factory;
        ComPtr<IDXGIAdapter> adapter;
        ComPtr<ID3D11Device> device;
        if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(factory.GetAddressOf()))) ||
            FAILED(factory->EnumAdapterByLuid(render_, IID_PPV_ARGS(adapter.GetAddressOf()))) ||
            FAILED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
                device.GetAddressOf(), nullptr, nullptr))) return;
        ComPtr<IDXGIDevice> dxgi;
        if (FAILED(device.As(&dxgi))) return;
        if (stop_.load(std::memory_order_acquire)) return;
        IDARG_IN_SWAPCHAINSETDEVICE binding{};
        binding.pDevice = dxgi.Get();
        if (FAILED(IddCxSwapChainSetDevice(chain_, &binding))) return;
        while (!stop_.load(std::memory_order_acquire)) {
            IDARG_OUT_RELEASEANDACQUIREBUFFER frame{};
            const HRESULT acquired = IddCxSwapChainReleaseAndAcquireBuffer(chain_, &frame);
            if (acquired == E_PENDING) {
                const HANDLE events[] = {terminate_, available_};
                const DWORD waited = WaitForMultipleObjects(2, events, FALSE, 100);
                if (waited == WAIT_OBJECT_0 || waited == WAIT_FAILED) return;
                if (waited != WAIT_OBJECT_0 + 1 && waited != WAIT_TIMEOUT) return;
                continue;
            }
            if (FAILED(acquired)) return;
            // No pixel read/map/copy/encode/logging. Release the acquired COM
            // reference and notify Cx; a later WP owns frame consumption.
            ComPtr<IDXGIResource> surface;
            surface.Attach(frame.MetaData.pSurface);
            surface.Reset();
            if (stop_.load(std::memory_order_acquire)) return;
            if (FAILED(IddCxSwapChainFinishedProcessingFrame(chain_))) return;
        }
    }
};

NativeMonitor::NativeMonitor(NativeDevice& device, MonitorToken token,
    const CPD_MODE& requested, const EdidResult& edid)
    : owner(device), identity(token), mode(requested), description(edid) {}
NativeMonitor::~NativeMonitor() = default;
void NativeMonitor::close_processor_admission() {
    const std::lock_guard<std::mutex> lock(processor_lock_);
    departing_ = true;
}
bool NativeMonitor::processor_absent() const {
    const std::lock_guard<std::mutex> lock(processor_lock_);
    return !processor_;
}
void NativeMonitor::unassign() {
    // Serializes Assign/Unassign/departure for this monitor, not device state.
    // The worker never takes this lock. No state lock spans this actual join.
    const std::lock_guard<std::mutex> lock(processor_lock_);
    if (processor_) {
        processor_->join();
        processor_.reset();
    }
}
bool NativeMonitor::assign(const IDARG_IN_SETSWAPCHAIN& swapchain) {
    const std::lock_guard<std::mutex> lock(processor_lock_);
    if (processor_) { processor_->join(); processor_.reset(); }
    if (departing_) return false;
    auto* next = new (std::nothrow) FrameProcessor(swapchain.hSwapChain,
        swapchain.RenderAdapterLuid, swapchain.hNextSurfaceAvailable);
    if (!next) return false;
    if (!next->launch()) { delete next; return false; }
    processor_.reset(next);
    return true;
}

namespace {
void fill_signal(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& signal, const EdidResult& edid,
                 bool monitor) {
    signal = {};
    signal.pixelRate = edid.timing.pixel_clock_hz;
    signal.activeSize = {edid.timing.active_width, edid.timing.active_height};
    signal.totalSize = {edid.timing.total_width, edid.timing.total_height};
    signal.vSyncFreq = {60, 1};
    signal.hSyncFreq = {static_cast<UINT32>(edid.timing.pixel_clock_hz), edid.timing.total_width};
    signal.AdditionalSignalInfo.videoStandard = 255;
    signal.AdditionalSignalInfo.vSyncFreqDivider = monitor ? 0 : 1;
    signal.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
}
bool decode_own_description(const IDDCX_MONITOR_DESCRIPTION& description, EdidResult& edid) {
    if (description.Type != IDDCX_MONITOR_DESCRIPTION_TYPE_EDID ||
        description.DataSize != 128 || !description.pData) return false;
    std::array<std::uint8_t, 128> bytes{};
    std::memcpy(bytes.data(), description.pData, bytes.size());
    const auto* p = bytes.data();
    const std::uint64_t serial = static_cast<std::uint64_t>(p[12]) |
        (static_cast<std::uint64_t>(p[13]) << 8) | (static_cast<std::uint64_t>(p[14]) << 16) |
        (static_cast<std::uint64_t>(p[15]) << 24);
    const CPD_MODE mode{
        static_cast<std::uint32_t>(p[56] | ((p[58] & 0xf0u) << 4)),
        static_cast<std::uint32_t>(p[59] | ((p[61] & 0xf0u) << 4)), 60, 1,
        static_cast<std::uint32_t>(p[66] | ((p[68] & 0xf0u) << 4)),
        static_cast<std::uint32_t>(p[67] | ((p[68] & 0x0fu) << 8)), 32, 0};
    if (!canonical_edid(bytes, mode, serial)) return false;
    edid = generate_edid(mode, serial);
    return edid.decision == Decision::Accepted;
}
}

_Use_decl_annotations_
NTSTATUS cpd_assign_swapchain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* input) {
    auto* state = monitor_context(monitor)->value;
    if (!input || !input->hSwapChain) return STATUS_SUCCESS;
    // Return success to take responsibility for this chain; failed setup simply
    // releases it, as documented. Never intentionally trigger the Assign error
    // bugcheck path or invoke ReportCriticalError/host termination.
    if (!state || !state->assign(*input)) WdfObjectDelete(input->hSwapChain);
    return STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_unassign_swapchain(IDDCX_MONITOR monitor) {
    if (auto* state = monitor_context(monitor)->value) state->unassign();
    return STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_parse_description(const IDARG_IN_PARSEMONITORDESCRIPTION* input,
    IDARG_OUT_PARSEMONITORDESCRIPTION* output) {
    if (!input || !output) return STATUS_INVALID_PARAMETER;
    EdidResult descriptor{};
    if (!decode_own_description(input->MonitorDescription, descriptor)) return STATUS_INVALID_PARAMETER;
    output->MonitorModeBufferOutputCount = 1;
    output->PreferredMonitorModeIdx = 0;
    if (input->MonitorModeBufferInputCount == 0) return STATUS_SUCCESS;
    if (!input->pMonitorModes) return STATUS_INVALID_PARAMETER;
    IDDCX_MONITOR_MODE mode{};
    mode.Size = sizeof(mode);
    mode.Origin = IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
    fill_signal(mode.MonitorVideoSignalInfo, descriptor, true);
    input->pMonitorModes[0] = mode;
    return STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_default_modes(IDDCX_MONITOR, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES*,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES*) {
    // Every admitted monitor has the fixed canonical descriptor; never invent
    // a default for an absent/unrecognized descriptor.
    return STATUS_NOT_SUPPORTED;
}
_Use_decl_annotations_
NTSTATUS cpd_target_modes(IDDCX_MONITOR monitor, const IDARG_IN_QUERYTARGETMODES* input,
    IDARG_OUT_QUERYTARGETMODES* output) {
    auto* state = monitor_context(monitor)->value;
    if (!state || !input || !output || input->MonitorDescription.Type != IDDCX_MONITOR_DESCRIPTION_TYPE_EDID ||
        input->MonitorDescription.DataSize != state->description.bytes.size() || !input->MonitorDescription.pData ||
        std::memcmp(input->MonitorDescription.pData, state->description.bytes.data(), state->description.bytes.size()) != 0)
        return STATUS_INVALID_PARAMETER;
    output->TargetModeBufferOutputCount = 1;
    if (input->TargetModeBufferInputCount == 0) return STATUS_SUCCESS;
    if (!input->pTargetModes) return STATUS_INVALID_PARAMETER;
    IDDCX_TARGET_MODE mode{};
    mode.Size = sizeof(mode);
    fill_signal(mode.TargetVideoSignalInfo.targetVideoSignalInfo, state->description, false);
    input->pTargetModes[0] = mode;
    return STATUS_SUCCESS;
}
_Use_decl_annotations_
NTSTATUS cpd_commit_modes(IDDCX_ADAPTER, const IDARG_IN_COMMITMODES* input) {
    return input && input->PathCount <= CPD_MAX_MONITORS &&
        (input->PathCount == 0 || input->pPaths) ? STATUS_SUCCESS : STATUS_INVALID_PARAMETER;
}

bool NativeDevice::effect_allowed(EffectToken effect) {
    const std::lock_guard<std::mutex> lock(state_lock);
    // Fresh monotonic sample for every effect. This is a policy check; WDF
    // callback/PnP stop+flush establishes the separate DDI lifetime barrier.
    return !cleanup_started && !native_failed && core.effect_authorized(effect, GetTickCount64());
}
void NativeDevice::forget_monitor(NativeMonitor* monitor, MonitorToken token) {
    const std::lock_guard<std::mutex> lock(state_lock);
    if (token.slot < monitors.size() && monitors[token.slot] == monitor)
        monitors[token.slot] = nullptr;
}
bool NativeDevice::create_monitor(EffectToken effect, const CPD_MODE& mode,
                                  bool& arrived, NTSTATUS& status) {
    arrived = false;
    status = STATUS_DEVICE_NOT_READY;
    // Fixed per-slot serial (slot + 1, nonzero), not the monitor id: a slot keeps one EDID identity
    // across ADD and resize, so Windows persists at most CPD_MAX_MONITORS display identities.
    const auto descriptor = generate_edid(mode, static_cast<std::uint64_t>(effect.monitor.slot) + 1U);
    if (descriptor.decision != Decision::Accepted || !effect_allowed(effect)) return false;
    auto* state = new (std::nothrow) NativeMonitor(*this, effect.monitor, mode, descriptor);
    if (!state) { status = STATUS_INSUFFICIENT_RESOURCES; return false; }
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, MonitorContext);
    attributes.SynchronizationScope = WdfSynchronizationScopeNone;
    attributes.EvtCleanupCallback = [](WDFOBJECT object) {
        auto* monitor = monitor_context(object)->value;
        if (monitor) monitor->owner.forget_monitor(monitor, monitor->identity);
        // No DDIs or delayed shutdown here. Unassign/departure/pre-cleanup
        // device stop have joined the owned processor before object cleanup.
    };
    attributes.EvtDestroyCallback = [](WDFOBJECT object) {
        auto* context = monitor_context(object);
        delete context->value;
        context->value = nullptr;
    };
    IDDCX_MONITOR_INFO info{};
    info.Size = sizeof(info);
    info.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
    info.ConnectorIndex = effect.monitor.slot;
    info.MonitorDescription.Size = sizeof(info.MonitorDescription);
    info.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    info.MonitorDescription.DataSize = static_cast<UINT>(state->description.bytes.size());
    info.MonitorDescription.pData = const_cast<BYTE*>(state->description.bytes.data());
    // A separate generated container belongs to this own logical monitor.
    if (FAILED(CoCreateGuid(&info.MonitorContainerId))) {
        delete state; status = STATUS_UNSUCCESSFUL; return false;
    }
    IDARG_IN_MONITORCREATE create{};
    create.ObjectAttributes = &attributes;
    create.pMonitorInfo = &info;
    IDARG_OUT_MONITORCREATE created{};
    if (!effect_allowed(effect)) { delete state; return false; }
    status = IddCxMonitorCreate(adapter, &create, &created);
    if (!NT_SUCCESS(status)) { delete state; return false; }
    state->handle = created.MonitorObject;
    monitor_context(created.MonitorObject)->value = state;
    {
        const std::lock_guard<std::mutex> lock(state_lock);
        monitors[effect.monitor.slot] = state;
    }
    if (!effect_allowed(effect)) {
        status = STATUS_CANCELLED;
        return true; // actual created object retained for Discard rollback
    }
    IDARG_OUT_MONITORARRIVAL arrival{};
    status = IddCxMonitorArrival(created.MonitorObject, &arrival);
    arrived = NT_SUCCESS(status);
    return true;
}
bool NativeDevice::retire_monitor(EffectToken effect) {
    NativeMonitor* monitor = nullptr;
    {
        const std::lock_guard<std::mutex> lock(state_lock);
        if (cleanup_started || !core.effect_authorized(effect, GetTickCount64()) ||
            effect.monitor.slot >= monitors.size()) return false;
        monitor = monitors[effect.monitor.slot];
        if (!monitor || !same(monitor->identity, effect.monitor)) return false;
    }
    // No state lock across native processor exit or monitor departure. There
    // is one serialized management executor and cleanup is outside its barrier.
    monitor->close_processor_admission();
    monitor->unassign();
    const IDDCX_MONITOR handle = monitor->handle;
    if (effect.kind == EffectKind::Discard) {
        WdfObjectDelete(handle);
        return true;
    }
    // Departure destroys the monitor (IddCx object-model contract). Do not
    // touch the context or issue a second delete after this success.
    return NT_SUCCESS(IddCxMonitorDeparture(handle));
}
} // namespace crosspane::idd
#endif
