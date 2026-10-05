# Limited, serialized win-gui acceptance. All native operations are restricted to retained child PIDs.
param([Parameter(Mandatory=$true)][string]$Agent,
      [Parameter(Mandatory=$true)][string]$Ctl,
      [Parameter(Mandatory=$true)][string]$Source)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;
public static class OwnedE2 {
    public delegate bool EnumProc(IntPtr window, IntPtr arg);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr arg);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder text, int length);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd, out Rect rectangle);
    [DllImport("user32.dll")] static extern bool PostMessage(IntPtr hwnd, uint message, UIntPtr wp, IntPtr lp);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd, IntPtr dc, uint flags);
    [DllImport("user32.dll")] static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool GetMonitorInfo(IntPtr monitor, ref MonitorInfo info);
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int left, top, right, bottom; }
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct MonitorInfo {
        public uint size; public Rect monitor, work; public uint flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=32)] public string device;
    }
    static void RequireOwn(IntPtr hwnd, uint expected) {
        uint pid; GetWindowThreadProcessId(hwnd, out pid);
        if (hwnd == IntPtr.Zero || pid != expected) throw new InvalidOperationException("owned HWND/PID no longer valid");
    }
    public static IntPtr OwnWindow(uint pid, string prefix, bool visible) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, arg) => {
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (owner != pid) return true;
            // SAFETY: read class/visibility only after exact retained fixture PID match; never titles/content of foreign windows.
            var text = new StringBuilder(256); GetClassName(hwnd, text, text.Capacity);
            if (text.ToString().StartsWith(prefix, StringComparison.Ordinal) && (!visible || IsWindowVisible(hwnd))) {
                found = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero); return found;
    }
    public static string Monitor(IntPtr hwnd, uint pid) {
        RequireOwn(hwnd, pid);
        var info = new MonitorInfo { size = (uint)Marshal.SizeOf(typeof(MonitorInfo)) };
        if (!GetMonitorInfo(MonitorFromWindow(hwnd, 0), ref info)) throw new InvalidOperationException("owned monitor unavailable");
        return info.device;
    }
    public static void Keys(IntPtr hwnd, uint pid) {
        RequireOwn(hwnd, pid);
        // SAFETY: direct messages only to the retained owned proxy, no global input/focus or typed text.
        if (!PostMessage(hwnd, 7, UIntPtr.Zero, IntPtr.Zero) ||
            !PostMessage(hwnd, 256, new UIntPtr(0x87u), new IntPtr(0x00760001)) ||
            !PostMessage(hwnd, 257, new UIntPtr(0x87u), new IntPtr(unchecked((int)0xc0760001))))
            throw new InvalidOperationException("owned key messages failed");
    }
    public static void Close(IntPtr hwnd, uint pid) {
        RequireOwn(hwnd, pid); if (!PostMessage(hwnd, 16, UIntPtr.Zero, IntPtr.Zero)) throw new InvalidOperationException("owned close failed");
    }
    public static void End(IntPtr hwnd, uint pid) {
        RequireOwn(hwnd, pid); if (!PostMessage(hwnd, 22, new UIntPtr(1u), IntPtr.Zero)) throw new InvalidOperationException("owned end-session failed");
    }
    public static string Capture(IntPtr hwnd, uint pid, string file) {
        RequireOwn(hwnd, pid); Rect r;
        if (!GetClientRect(hwnd, out r) || r.right < 32 || r.bottom < 32) throw new InvalidOperationException("owned client size unavailable");
        using (var image = new Bitmap(r.right, r.bottom, PixelFormat.Format32bppArgb)) {
            using (var graphics = Graphics.FromImage(image)) {
                var dc = graphics.GetHdc();
                try {
                    // SAFETY: retained own proxy HWND/PID, client-only/full DXGI content capture;
                    // owned bitmap/DC released by finally/using. Matches the landed W2.4b probe.
                    if (!PrintWindow(hwnd, dc, 3)) throw new InvalidOperationException("owned PrintWindow failed");
                } finally { graphics.ReleaseHdc(dc); }
            }
            var a = image.GetPixel(r.right / 4, r.bottom / 4);
            var b = image.GetPixel(3 * r.right / 4, r.bottom / 4);
            var c = image.GetPixel(r.right / 4, 3 * r.bottom / 4);
            var d = image.GetPixel(3 * r.right / 4, 3 * r.bottom / 4);
            if (!(a.R > 160 && a.G < 70 && a.B < 70 && b.G > 160 && b.R < 70 && b.B < 70 &&
                  c.B > 160 && c.R < 70 && c.G < 70 && d.R > 160 && d.G > 160 && d.B > 160))
                throw new InvalidOperationException("owned screenshot did not match synthetic video quadrants");
            image.Save(file, ImageFormat.Png);
        }
        return r.right + "x" + r.bottom;
    }
}
'@
# Every process here comes from a retained Start result; never rediscover or terminate by PID.
function Stop-OwnedProcess([Diagnostics.Process]$child) {
    if ($null -eq $child) { return $true }
    $stopped = $false
    try {
        if ($child.HasExited) { $stopped = $true }
        else { $child.Kill(); $stopped = $child.WaitForExit(3000) }
    } catch { $stopped = $false }
    if ($stopped) { try { $child.Dispose() } catch { $stopped = $false } }
    return $stopped
}
$script:statusChildren = [Collections.Generic.List[Diagnostics.Process]]::new()
function Owned-Status {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Ctl; $info.Arguments = '--json status'; $info.UseShellExecute = $false
    $info.CreateNoWindow = $true; $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $child = [Diagnostics.Process]::Start($info)
    $script:statusChildren.Add($child)
    try {
        $output = $child.StandardOutput.ReadToEndAsync(); $error = $child.StandardError.ReadToEndAsync()
        if (-not $child.WaitForExit(8000)) { throw 'owned Status timeout' }
        if ($child.ExitCode -eq 0) { return ($output.Result | ConvertFrom-Json) }
        return $null
    } finally {
        if (Stop-OwnedProcess $child) { [void]$script:statusChildren.Remove($child) }
        else { throw 'owned Status cleanup exceeded its three-second bound' }
    }
}
function Read-OwnedText([string]$file) {
    # Active redirected writers require the reader to share write/delete. Read only a bounded
    # tail of this fixture's exact owned path, never a log discovered from another process.
    $stream = [IO.File]::Open($file, [IO.FileMode]::Open, [IO.FileAccess]::Read,
        ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $snapshotLength = $stream.Length
        $capacity = [int][Math]::Min([long]65536, $snapshotLength)
        [void]$stream.Seek($snapshotLength - $capacity, [IO.SeekOrigin]::Begin)
        $bytes = [byte[]]::new($capacity)
        $offset = 0
        while ($offset -lt $capacity) {
            $read = $stream.Read($bytes, $offset, $capacity - $offset)
            if ($read -eq 0) { break }
            $offset += $read
        }
        return [Text.Encoding]::UTF8.GetString($bytes, 0, $offset)
    } finally { $stream.Dispose() }
}
function Read-OwnedJson([string]$file) {
    if (Test-Path -LiteralPath $file) { return (Get-Content -Raw -LiteralPath $file | ConvertFrom-Json) }
    return $null
}
$suffix = [Guid]::NewGuid().ToString('N')
$scratch = Join-Path $env:TEMP ('crosspane-WP-W2.5a-' + $suffix)
$sourceProcess = $null; $agentProcess = $null; $saved = @{}
$variables = @('APPDATA','LOCALAPPDATA','CROSSPANE_RUNTIME_DIR','CROSSPANE_ACCEPTANCE_E1_ONLY',
    'CROSSPANE_ACCEPTANCE_E2_DESTINATION','CROSSPANE_E2_SOURCE_FIXTURE','CROSSPANE_E2_FIXTURE_ROOT',
    'CROSSPANE_AUDIO','CROSSPANE_DISCOVERY','CROSSPANE_GPU','RUST_LOG')
try {
    foreach ($v in $variables) { $saved[$v] = [Environment]::GetEnvironmentVariable($v, 'Process') }
    $env:APPDATA = Join-Path $scratch 'roaming'; $env:LOCALAPPDATA = Join-Path $scratch 'local'
    $env:CROSSPANE_RUNTIME_DIR = Join-Path $scratch 'runtime'
    $env:CROSSPANE_ACCEPTANCE_E1_ONLY = $null; $env:CROSSPANE_ACCEPTANCE_E2_DESTINATION = '1'
    $env:CROSSPANE_E2_SOURCE_FIXTURE = '1'; $env:CROSSPANE_E2_FIXTURE_ROOT = $scratch
    $env:CROSSPANE_AUDIO = '0'; $env:CROSSPANE_DISCOVERY = '0'; $env:CROSSPANE_GPU = '0'
    # Narrow debug placement evidence only; source backends are omitted, so no owner titles can enter this log.
    $env:RUST_LOG = 'info,crosspane_render::proxy::app=debug'
    New-Item -ItemType Directory -Path $scratch -Force | Out-Null
    $sourceProcess = Start-Process -FilePath $Source -ArgumentList '--exact owned_e2_source --nocapture' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'source.out') -RedirectStandardError (Join-Path $scratch 'source.err')
    $sourceHandle = $sourceProcess.Handle
    $deadline = [DateTime]::UtcNow.AddSeconds(15); $ready = $null
    do {
        if ($sourceProcess.HasExited) { throw "owned source exited $($sourceProcess.ExitCode) before readiness" }
        $ready = Read-OwnedJson (Join-Path $scratch 'fixture/source-ready.json')
        if ($null -eq $ready) { Start-Sleep -Milliseconds 100 }
    } while ($null -eq $ready -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $ready) { throw 'owned source readiness timeout' }
    $agentProcess = Start-Process -FilePath $Agent -ArgumentList 'run' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'agent.out') -RedirectStandardError (Join-Path $scratch 'agent.err')
    $agentHandle = $agentProcess.Handle
    $deadline = [DateTime]::UtcNow.AddSeconds(20); $status = $null; $proxy = [IntPtr]::Zero
    do {
        if ($agentProcess.HasExited) { throw "owned destination exited $($agentProcess.ExitCode)" }
        if ($sourceProcess.HasExited) { throw "owned source exited $($sourceProcess.ExitCode)" }
        $status = Owned-Status
        if ($null -ne $status) {
            if ($null -ne $status.result) { $status = $status.result }
            $peers = @($status.installer.peers)
            $projections = @($status.projections)
            if ($peers.Count -eq 1 -and $projections.Count -eq 1 -and
                $projections[0].received.frames -ge 3 -and $peers[0].counters.e2_frames_presented -ge 3) {
                $proxy = [OwnedE2]::OwnWindow([uint32]$agentProcess.Id, 'CrosspaneProxy', $true)
                if ($proxy -ne [IntPtr]::Zero) { break }
            }
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($proxy -eq [IntPtr]::Zero) { throw 'owned decode/render/proxy readiness timeout' }
    if ($status.installer.instance.pid -ne $agentProcess.Id -or $null -ne $status.installer.instance.uid -or $status.installer.keystore -ne 'file') { throw 'destination scratch identity/PID mismatch' }
    foreach ($name in @('windows','frames')) {
        $backend = @($status.installer.backends | Where-Object { $_.name -eq $name })
        if ($backend.Count -ne 1 -or $backend[0].state -eq 'ready') { throw 'source-only destination scratch backend unexpectedly active' }
    }
    if ($status.installer.discovery.enabled -or $status.installer.audio.enabled) { throw 'destination scratch discovery/audio unexpectedly enabled' }

    $native = [OwnedE2]::Monitor($proxy, [uint32]$agentProcess.Id)
    $matching = @($ready.monitors | Where-Object { $_.native_id -eq $native })
    if ($matching.Count -ne 1) { throw 'owned current monitor did not uniquely join fresh display IDs' }
    $expectedId = $matching[0].id
    $agentLog = (Read-OwnedText (Join-Path $scratch 'agent.out')) + (Read-OwnedText (Join-Path $scratch 'agent.err'))
    if ($agentLog -notmatch ('monitor: Some\(' + $expectedId + '\)')) { throw 'production reverse Placed.monitor was not mapped to DisplayId' }
    if ($agentLog -notmatch 'video decoding on' -or $agentLog -notmatch 'Microsoft H\.264 decoder \(software\)') { throw 'destination MF software decoder evidence absent' }
    @($agentLog -split "`n" | Where-Object { $_.Contains('proxy surface format') -or $_.Contains('video decoding on') }) | Select-Object -Last 2
    'scratch_destination=true; windows_uid=null; file_identity=true; source_backends_omitted=true'
    "destination_decoded=$($projections[0].received.frames); destination_presented=$($peers[0].counters.e2_frames_presented); placed_monitor=$expectedId; native_monitor_join=true; explicit_place=none"
    $image = Join-Path $scratch 'owned-proxy.png'
    $dimensions = [OwnedE2]::Capture($proxy, [uint32]$agentProcess.Id, $image)
    "owned_PrintWindow_synthetic_quadrants=true; client_size=$dimensions"
    'owned_screenshot_png_base64=' + [Convert]::ToBase64String([IO.File]::ReadAllBytes($image))
    [OwnedE2]::Keys($proxy, [uint32]$agentProcess.Id)
    $deadline = [DateTime]::UtcNow.AddSeconds(5); $progress = $null
    do {
        $progress = Read-OwnedJson (Join-Path $scratch 'fixture/source-progress.json')
        if ($null -ne $progress -and $progress.key_down -gt 0 -and $progress.key_up -gt 0) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($null -eq $progress -or $progress.key_down -eq 0 -or $progress.key_up -eq 0) { throw 'owned proxy input did not reach production source engine fake injector' }
    "owned_source_input_down=$($progress.key_down); owned_source_input_up=$($progress.key_up); focus_guard_observed=true"
    [OwnedE2]::Close($proxy, [uint32]$agentProcess.Id)
    if (-not $sourceProcess.WaitForExit(10000) -or $sourceProcess.ExitCode -ne 0) { throw 'owned source did not finish cleanly after close return' }
    $progress = Read-OwnedJson (Join-Path $scratch 'fixture/source-progress.json')
    if (-not $progress.returned -or -not $progress.clean -or $progress.restores -ne 1) { throw 'owned source restoration/MF completion failed' }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $status = Owned-Status; if ($null -ne $status.result) { $status = $status.result }
        $peers = @($status.installer.peers)
        if ($peers.Count -eq 1 -and $peers[0].counters.e2_dest_returned -eq 1 -and @($status.projections).Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    if ($peers.Count -ne 1 -or $peers[0].counters.e2_dest_returned -ne 1 -or @($status.projections).Count -ne 0) { throw 'real destination did not account for returned proxy' }
    "source_encoded=$($progress.encoded); source_restore=1; source_MF_clean=true; destination_started=$($peers[0].counters.e2_dest_started); destination_returned=1; proxy_closed=true"
    $observer = [OwnedE2]::OwnWindow([uint32]$agentProcess.Id, ('CrosspaneShutdown' + $agentProcess.Id), $false)
    [OwnedE2]::End($observer, [uint32]$agentProcess.Id)
    if (-not $agentProcess.WaitForExit(7000) -or $agentProcess.ExitCode -ne 0) { throw 'owned destination did not complete host/media shutdown' }
    $receipt = Read-OwnedJson (Join-Path $env:LOCALAPPDATA 'Crosspane/last_exit.json')
    if (-not $receipt.clean) { throw 'destination clean receipt missing' }
    'destination_media_host_shutdown=true; destination_receipt_clean=true'
} catch {
    $originalFailure = $_
    foreach ($name in @('source.err','source.out','agent.out','agent.err')) {
        try {
            $log = Join-Path $scratch $name
            if (Test-Path -LiteralPath $log) { $text = Read-OwnedText $log; $text.Substring([Math]::Max(0, $text.Length - 8192)) }
        } catch { 'owned diagnostic unavailable; original failure retained' }
    }
    throw $originalFailure
} finally {
    # Each cleanup is bounded and exception-contained; one failure cannot skip the other child,
    # environment restoration or owned-directory removal. Preserve handles until stop succeeds.
    $cleanupFailures = [Collections.Generic.List[string]]::new()
    foreach ($child in @($agentProcess, $sourceProcess) + @($script:statusChildren)) {
        try { if (-not (Stop-OwnedProcess $child)) { $cleanupFailures.Add('retained child stop') } }
        catch { $cleanupFailures.Add('retained child cleanup') }
    }
    foreach ($v in $variables) {
        try { [Environment]::SetEnvironmentVariable($v, $saved[$v], 'Process') }
        catch { $cleanupFailures.Add('environment restore') }
    }
    try { if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force } }
    catch { $cleanupFailures.Add('owned scratch removal') }
    if ($cleanupFailures.Count -gt 0) { throw ('owned fixture cleanup incomplete: ' + ($cleanupFailures -join ', ')) }
    'scratch_processes_and_files_removed=true'
}
