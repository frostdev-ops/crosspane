# Crosspane IddCx twin driver

Development snapshot for WP-W3.1a. The current validation covers portable ABI,
lease and EDID models; the native build and loading are pending. The source base
and licences are recorded in NOTICE.md and UPSTREAM.json.

The device interface uses the fixed v1 ABI in include/crosspane_idd_v1.h:
ADD, REMOVE, LIST and HEARTBEAT. Authority belongs to the actual open file
object. Duplicate handles share that capability; independent opens do not. Each
open owns at most one monitor, with four opens and four monitors in total. A
5-second lease requires a strictly increasing heartbeat at least once per second.
Late or replayed heartbeats cannot revive an expired client. LIST returns only
that client's monitor. Resize removes the old monitor and adds a new monitor ID on
the same lane. A slot keeps one EDID identity (serial slot + 1) across that REMOVE
and ADD, so Windows persists at most four display identities.

Supported descriptors are generated 128-byte EDID 1.3 for 1280x720 or 1920x1080
at 60 Hz, 32 bpp, and a physical size of 10–2000 mm. The CPD manufacturer, product
1, serial rule, preferred timing, dummy descriptors and checksum are fixed.
Windows' chosen DPI remains a runtime measurement.

Identity: Crosspane\IddTwinV1, service/DLL CrosspaneIdd, interface
FEA027FB-8535-40EB-B887-1401D63EF273. The installer creates the device node in
W4.1c. The protected per-device DACL grants interactive users read/write and
System/Administrators full access, denying network/anonymous access. Interactive
users include RDP users; this is not a physical-console-only policy.

Each assigned swapchain has its own processor. Unassign and monitor departure
signal and join that processor before returning. Individual event-loop waits
are capped at 100 ms. The final join and OS/DDI teardown latency have no absolute
time bound; this is the explicit WP-W3.1a lifecycle exception. Device stop first
closes control admission, drains already admitted setup callbacks, then stops
framework timer/work and joins processors.

Build inputs are the existing MSVC 14.44.35207 and the locally staged official
WDK.x64, SDK.CPP.x64 and SDK.CPP packages, all 10.0.26100.6584. The first build
is lead-executed through the reviewed scripts/build.ps1, Release/x64 with
SignMode=Off and no online restore. The installed WindowsUserModeDriver10.0 shim imports the staged NuGet kit.
Exact source/kit paths passed the post-resume audit; effective MSBuild properties
will be checked by the fixed script before its first build. SpectreMitigation=false is admitted for these test
builds because the matching Spectre libraries are absent; distribution builds
restore mitigation under W4.2.

Loading, standard-user native IOCTL tests, heartbeat-loss unplug, DPI and kill
recovery remain owner-gated. Portable model checks and compilation do not
establish those runtime behaviors.

Lead d474f300 accepts the pinned WDK build-tool telemetry under existing owner settings; no suppression property was found. No restore/download or sign/install/deploy effect is admitted. The installed shim resolves the staged NuGet kit; fixed script and first lead-only build are pending.
