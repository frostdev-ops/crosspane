# Limited interactive acceptance fixture. Only the process started here is observed or stopped.
param([Parameter(Mandatory=$true)][string]$Agent, [Parameter(Mandatory=$true)][string]$Ctl)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class CrosspaneSmoke {
    public static int TrayResult;
    public delegate bool EnumProc(IntPtr window, IntPtr arg);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr arg);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder text, int length);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, UIntPtr wp, IntPtr lp);
    public static bool EndOwnSession(IntPtr hwnd) { return PostMessage(hwnd, 22, new UIntPtr(1u), IntPtr.Zero); }
    [StructLayout(LayoutKind.Sequential)] public struct Icon { public uint size; public IntPtr hwnd; public uint id; public Guid guid; }
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int left, top, right, bottom; }
    [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref Icon icon, out Rect rectangle);
    public static IntPtr OwnWindow(uint pid, string prefix) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, arg) => {
            uint owner; GetWindowThreadProcessId(hwnd, out owner);
            if (owner != pid) return true;
            // Class names are read only after matching the fixture's own child PID; never titles.
            var text = new StringBuilder(256);
            GetClassName(hwnd, text, text.Capacity);
            if (text.ToString().StartsWith(prefix, StringComparison.Ordinal)) { found = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static bool TrayPresent(IntPtr hwnd) {
        var icon = new Icon { size = (uint)Marshal.SizeOf(typeof(Icon)), hwnd = hwnd, id = 1 };
        Rect rectangle;
        TrayResult = Shell_NotifyIconGetRect(ref icon, out rectangle);
        return TrayResult == 0 && rectangle.right > rectangle.left && rectangle.bottom > rectangle.top;
    }
}
'@
function Scratch-Status {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Ctl
    $info.Arguments = '--json status'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $child = [Diagnostics.Process]::Start($info)
    try {
        $output = $child.StandardOutput.ReadToEndAsync()
        $error = $child.StandardError.ReadToEndAsync()
        if (-not $child.WaitForExit(15000)) { $child.Kill(); $child.WaitForExit(); throw 'owned Status child timed out' }
        if ($child.ExitCode -eq 0) { return ($output.Result | ConvertFrom-Json) }
        return $null
    } finally { $child.Dispose() }
}
function Scratch-Diag([string]$archive) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Ctl
    $info.Arguments = 'diag --out "' + $archive + '"'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $child = [Diagnostics.Process]::Start($info)
    try {
        $output = $child.StandardOutput.ReadToEndAsync()
        $error = $child.StandardError.ReadToEndAsync()
        if (-not $child.WaitForExit(30000)) { $child.Kill(); $child.WaitForExit(); throw 'owned diagnostics child timed out' }
        return $child.ExitCode
    } finally { $child.Dispose() }
}
$suffix = [Guid]::NewGuid().ToString('N')
$scratch = Join-Path $env:TEMP ('crosspane-WP-W1.5b-' + $suffix)
$process = $null
$saved = @{}
$variables = @('APPDATA', 'LOCALAPPDATA', 'CROSSPANE_RUNTIME_DIR', 'CROSSPANE_ACCEPTANCE_E1_ONLY', 'CROSSPANE_AUDIO', 'CROSSPANE_DISCOVERY', 'CROSSPANE_GPU')
try {
    foreach ($variable in $variables) { $saved[$variable] = [Environment]::GetEnvironmentVariable($variable, 'Process') }
    $env:APPDATA = Join-Path $scratch 'roaming'
    $env:LOCALAPPDATA = Join-Path $scratch 'local'
    $env:CROSSPANE_RUNTIME_DIR = Join-Path $scratch 'runtime'
    $env:CROSSPANE_ACCEPTANCE_E1_ONLY = '1'
    $env:CROSSPANE_AUDIO = '0'
    $env:CROSSPANE_DISCOVERY = '0'
    $env:CROSSPANE_GPU = '0'
    $config = Join-Path $env:APPDATA 'Crosspane'
    New-Item -ItemType Directory -Path $config -Force | Out-Null
    $socket = [System.Net.Sockets.UdpClient]::new(0)
    $port = $socket.Client.LocalEndPoint.Port
    $socket.Dispose()
    [IO.File]::WriteAllText((Join-Path $config 'config.toml'), ('name = "wp-w1-5b-' + $suffix + '"' + "`nport = $port`nforce_file_keystore = true`n"))
    $process = Start-Process -FilePath $Agent -ArgumentList 'run' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'agent.out') -RedirectStandardError (Join-Path $scratch 'agent.err')
    # Keep the exact launched process handle so its exit status survives rapid native exit.
    $ownedHandle = $process.Handle
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $status = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) { throw "fixture agent exited before readiness with $($process.ExitCode)" }
        $status = Scratch-Status
        if ($null -ne $status) { break }
        Start-Sleep -Milliseconds 200
    }
    if ($null -eq $status) { throw 'scratch Status did not become available' }
    $installer = $status.installer
    if ($null -eq $installer) { $installer = $status.result.installer }
    if ($installer.instance.pid -ne $process.Id) { throw 'Status did not name this owned child' }
    if ($null -ne $installer.instance.uid) { throw 'Windows installer UID must be null' }
    if ($installer.keystore -ne 'file') { throw 'scratch identity must use file keystore' }
    $trayDeadline = [DateTime]::UtcNow.AddSeconds(5)
    $trayReady = $false
    do {
        $tray = [CrosspaneSmoke]::OwnWindow([uint32]$process.Id, 'CrosspaneTray-')
        $trayReady = $tray -ne [IntPtr]::Zero -and [CrosspaneSmoke]::TrayPresent($tray)
        if (-not $trayReady) { Start-Sleep -Milliseconds 100 }
    } while (-not $trayReady -and [DateTime]::UtcNow -lt $trayDeadline)
    if (-not $trayReady) { throw "owned tray absent: hwnd=$tray; hresult=$([CrosspaneSmoke]::TrayResult)" }
    'scratch_status_ready=true; windows_uid=null; owned_tray_present=true'
    $unrelated = Join-Path $scratch 'unrelated.txt'
    [IO.File]::WriteAllText($unrelated, 'owned unrelated fixture')
    $archive = Join-Path $scratch 'diagnostics.tar.gz'
    if ((Scratch-Diag $archive) -ne 0) { throw 'scratch diagnostics creation failed' }
    $original = [IO.File]::ReadAllBytes($archive)
    if ((Scratch-Diag $archive) -eq 0) { throw 'diagnostics overwrote an existing owned archive' }
    if ([Convert]::ToBase64String([IO.File]::ReadAllBytes($archive)) -ne [Convert]::ToBase64String($original)) { throw 'existing archive changed on failed diagnostics' }
    if ([IO.File]::ReadAllText($unrelated) -ne 'owned unrelated fixture') { throw 'unrelated scratch sibling changed' }
    $extracted = Join-Path $scratch 'extracted'
    New-Item -ItemType Directory -Path $extracted | Out-Null
    $tar = Join-Path $env:SystemRoot 'System32/tar.exe'
    & $tar -xzf $archive -C $extracted 2>$null
    if ($LASTEXITCODE -ne 0) { throw 'scratch diagnostics archive was not readable' }
    $folders = @(Get-ChildItem -LiteralPath $extracted -Directory)
    if ($folders.Count -ne 1) { throw 'scratch diagnostics archive folder count differed' }
    $payload = $folders[0].FullName
    $archived = Get-Content -Raw (Join-Path $payload 'status.json') | ConvertFrom-Json
    if ($archived.result.installer.instance.pid -ne $process.Id) { throw 'archive status did not name owned child' }
    if (Get-ChildItem -LiteralPath $payload -Filter 'device-key*') { throw 'diagnostics archive included a key file' }
    if (@(Get-ChildItem -LiteralPath $env:CROSSPANE_RUNTIME_DIR -Directory -Filter 'crosspane-diag-*').Count -ne 0) { throw 'diagnostics staging residue remained' }
    'scratch_diag_readable=true; duplicate_output_preserved=true; unrelated_sibling_preserved=true; scratch_staging_removed=true'
    Start-Sleep -Seconds 20
    $observer = [CrosspaneSmoke]::OwnWindow([uint32]$process.Id, ('CrosspaneShutdown' + $process.Id))
    if ($observer -eq [IntPtr]::Zero) { throw 'owned shutdown observer was not present' }
    # Exercise only this child observer's native session-end path; no real logoff/broadcast.
    if (-not [CrosspaneSmoke]::EndOwnSession($observer)) { throw 'post owned WM_ENDSESSION failed' }
    if (-not $process.WaitForExit(7000)) { throw 'scratch agent did not stop within seven seconds' }
    $process.Refresh()
    if ($process.ExitCode -ne 0) { throw "scratch clean stop exited $($process.ExitCode)" }
    $receipt = Get-Content -Raw (Join-Path $env:LOCALAPPDATA 'Crosspane/last_exit.json') | ConvertFrom-Json
    if (-not $receipt.clean) { throw 'scratch receipt was not clean' }
    # The fixture has no title-reading backends. Inspect only bounded own stderr to prove the
    # native panic/release observer subscribed after its engine chord was configured.
    $stderr = [IO.File]::ReadAllText((Join-Path $scratch 'agent.err'))
    $stderr = $stderr.Substring([Math]::Max(0, $stderr.Length - 8192))
    if ($stderr.Contains('hotkey chord not configured') -or $stderr.Contains('global hotkeys unavailable')) { throw 'scratch native hotkey subscription was unavailable' }
    'scratch_clean_stop=true; scratch_receipt_clean=true'
    'scratch_hotkey_subscription_configured=true'
} catch {
    # This is only our own E1 fixture's stderr, and the fixture omitted title-reading backends.
    $log = Join-Path $scratch 'agent.err'
    if (Test-Path -LiteralPath $log) {
        try {
            $text = [IO.File]::ReadAllText($log)
            $text.Substring([Math]::Max(0, $text.Length - 8192))
        } catch { }
    }
    throw
} finally {
    if ($null -ne $process) {
        if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
    foreach ($variable in $variables) { [Environment]::SetEnvironmentVariable($variable, $saved[$variable], 'Process') }
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}
