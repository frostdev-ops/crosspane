// Crosspane private control preparation. Tokens are not native handle authority.
#pragma once
#include "Driver.h"
#include "Lease.h"

namespace crosspane::idd {
struct PreparedControl {
    Decision decision{Decision::Invalid};
    DecodedRequest decoded{};
    ClientToken client{};
    CommandToken command{};
    MonitorToken monitor{};
    AdmissionEpoch admission{};
};

// Caller serializes LeaseCore. client must come from the actual admitted file
// context, never a field in the request. No native call, completion or borrowed
// input pointer escapes this function. Accepted means model reservation only.
PreparedControl prepare_control_request(DeviceAdmission& admission, LeaseCore& core,
    ClientToken client, std::uint64_t now, std::uint32_t ioctl, const void* bytes,
    std::size_t length, std::size_t output_capacity) noexcept;
} // namespace crosspane::idd

#if defined(_WIN32)
#ifndef NOMINMAX
#define NOMINMAX 1
#endif
#include <windows.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>
#include <array>
#include <condition_variable>

namespace crosspane::idd {
class NativeMonitor;
struct RequestSlot {
    WDFREQUEST request{};
    WDFFILEOBJECT file{};
    PreparedControl prepared{};
    bool registering{};
    bool marked{};
    bool cancelled{};
    bool cancel_seen{};
    bool unmarking{};
    bool effect_started{};
    bool completion_claimed{};
    Completion completion{};
    ListSnapshot reply_snapshot{};
    void* response_buffer{};
    NTSTATUS native_status{STATUS_SUCCESS};
};
class NativeDevice final {
public:
    explicit NativeDevice(WDFDEVICE handle) : device(handle) {}
    WDFDEVICE device{};
    IDDCX_ADAPTER adapter{};
    WDFWORKITEM work{};
    WDFTIMER timer{};
    std::mutex state_lock;
    std::mutex work_lock;
    std::condition_variable setups_finished;
    std::uint32_t setups_in_flight{};
    std::uint32_t admitted_files{};
    DeviceAdmission admission;
    LeaseCore core;
    std::array<RequestSlot, CPD_MAX_COMMANDS> requests{};
    std::array<NativeMonitor*, CPD_MAX_MONITORS> monitors{};
    bool stopping{};
    bool cleanup_started{};
    bool initializing{};
    bool stop_complete{};
    NTSTATUS stop_status{STATUS_SUCCESS};
    bool native_failed{};
    AdmissionEpoch adapter_epoch{};
    AdmissionEpoch in_flight_adapter_epoch{};

    void enqueue_locked(); // short nonblocking framework enqueue only
    void run_management();
    NTSTATUS start();
    NTSTATUS stop(); // pre-Cleanup only; stop/flush and actual processor joins
    bool create_monitor(EffectToken effect, const CPD_MODE& mode, bool& arrived, NTSTATUS& status);
    bool retire_monitor(EffectToken effect);
    void forget_monitor(NativeMonitor* monitor, MonitorToken token);
    bool effect_allowed(EffectToken effect);
};
struct DeviceContext { NativeDevice* value; };
// WDF zero-initializes this plain mailbox; no C++ object needs construction.
// Every field is protected by the separate native adapter_mailbox_lock.
struct AdapterContext {
    NativeDevice* owner;
    WDFDEVICE owner_device;
    AdmissionEpoch generation;
    NTSTATUS finished_status;
    bool finished;
    bool delivered;
    bool cleaned;
};
struct FileContext { NativeDevice* device; ClientToken client; bool cleaned; };
struct RequestContext { NativeDevice* device; CommandToken command; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DeviceContext, device_context);
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(AdapterContext, adapter_context);
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(FileContext, file_context);
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(RequestContext, request_context);

NTSTATUS decision_status(Decision decision) noexcept;
EVT_WDF_DEVICE_FILE_CREATE cpd_file_create;
EVT_WDF_FILE_CLEANUP cpd_file_cleanup;
EVT_WDF_FILE_CLOSE cpd_file_close;
EVT_IDD_CX_DEVICE_IO_CONTROL cpd_device_io_control;
EVT_WDF_REQUEST_CANCEL cpd_request_cancel;
EVT_WDF_WORKITEM cpd_management;
EVT_WDF_TIMER cpd_expiry;
EVT_IDD_CX_ADAPTER_INIT_FINISHED cpd_adapter_finished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES cpd_commit_modes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION cpd_parse_description;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES cpd_default_modes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES cpd_target_modes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN cpd_assign_swapchain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN cpd_unassign_swapchain;
} // namespace crosspane::idd
#endif
