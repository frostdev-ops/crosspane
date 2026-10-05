# Limited owned fixture. Run only after the approved loopback bind slice is carried here.
param(
    [Parameter(Mandatory=$true)][string]$Agent,
    [Parameter(Mandatory=$true)][string]$Ctl,
    [Parameter(Mandatory=$true)][string]$CtlTest
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class InstallerStopFixture {
    [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint low, high; public long Value { get { return ((long)high << 32) | low; } } }
    [DllImport("kernel32.dll")] static extern uint GetProcessId(IntPtr process);
    [DllImport("kernel32.dll")] static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder image, ref uint length);
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Entry {
        public uint size, usage, pid; public UIntPtr heap; public uint module, threads, parent;
        public int priority; public uint flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=260)] public string file;
    }
    [DllImport("kernel32.dll")] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32First(IntPtr snapshot, ref Entry entry);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32Next(IntPtr snapshot, ref Entry entry);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static long Created(IntPtr process) {
        FileTime creation, exit, kernel, user;
        return GetProcessTimes(process, out creation, out exit, out kernel, out user) ? creation.Value : 0;
    }
    public static bool AdmitReplacement(IntPtr child, uint pid, IntPtr parent, uint parentPid, long parentCreated, long observedCreation, string observedImage) {
        FileTime creation, exit, kernel, user;
        if (GetProcessId(parent) != parentPid || parentCreated == 0 || !GetProcessTimes(parent, out creation, out exit, out kernel, out user)
            || creation.Value != parentCreated || exit.Value < creation.Value) return false;
        long childCreated = Created(child);
        if (GetProcessId(child) != pid || childCreated == 0 || childCreated / 10 != observedCreation / 10
            || childCreated < creation.Value || childCreated > exit.Value || String.IsNullOrEmpty(observedImage)) return false;
        IntPtr snapshot = CreateToolhelp32Snapshot(2, 0);
        if (snapshot == new IntPtr(-1)) return false;
        bool admitted = false;
        try {
            var entry = new Entry { size = (uint)Marshal.SizeOf(typeof(Entry)) };
            if (Process32First(snapshot, ref entry)) do {
                if (entry.pid != pid) continue;
                admitted = entry.parent == parentPid && String.Equals(entry.file, "crosspane-agent.exe", StringComparison.OrdinalIgnoreCase);
                break;
            } while (Process32Next(snapshot, ref entry));
        } finally { CloseHandle(snapshot); }
        if (!admitted) return false;
        var image = new StringBuilder(32768); uint length = (uint)image.Capacity;
        return QueryFullProcessImageName(child, 0, image, ref length)
            && String.Equals(image.ToString(), observedImage, StringComparison.OrdinalIgnoreCase) && Created(child) == childCreated;
    }
}
'@
function Stop-Owned([Diagnostics.Process]$child, [switch]$KeepPin) {
    $settled = $false
    $ok = $true
    try {
        if ($child.HasExited) { $settled = $true }
        else { $child.Kill(); $settled = $child.WaitForExit(3000) }
    } catch { $ok = $false }
    finally { if ($settled -and -not $KeepPin) { try { $child.Dispose() } catch { $ok = $false } } }
    return $ok -and $settled
}
function Own-Error([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return '' }
    $text = [IO.File]::ReadAllText($path)
    return $text.Substring([Math]::Max(0, $text.Length - 8192))
}
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
        $pin = $child.Handle
        $output = $child.StandardOutput.ReadToEndAsync()
        $error = $child.StandardError.ReadToEndAsync()
        if (-not $child.WaitForExit(15000)) { throw 'owned Status child timed out' }
        if ($child.ExitCode -ne 0) { return $null }
        $response = $output.Result | ConvertFrom-Json
        if ($response.ok -ne $true) { throw 'owned Status envelope was not successful' }
        return $response.result
    } finally { if (-not (Stop-Owned $child)) { throw 'owned Status cleanup failed' } }
}
function Retain-Replacements {
    $rows = @(Get-CimInstance Win32_Process -Filter ("ParentProcessId = " + $process.Id + " AND Name = 'crosspane-agent.exe'"))
    $allAdmitted = $true
    foreach ($row in $rows) {
        $candidate = $null
        try {
            $candidate = [Diagnostics.Process]::GetProcessById([int]$row.ProcessId)
            $pin = $candidate.Handle
            if (-not [InstallerStopFixture]::AdmitReplacement($pin, [uint32]$row.ProcessId, $process.Handle, [uint32]$process.Id, $created, $row.CreationDate.ToFileTimeUtc(), $row.ExecutablePath)) { throw 'replacement native identity admission failed' }
            $same = $false
            foreach ($known in $replacements) {
                if ($known.Id -ne $candidate.Id) { continue }
                try { $knownCreated = [InstallerStopFixture]::Created($known.Handle) } catch { continue }
                if ($knownCreated -ne 0 -and $knownCreated -eq [InstallerStopFixture]::Created($pin)) { $same = $true; break }
            }
            if (-not $same) { $replacements.Add($candidate); $candidate = $null }
        } catch { $allAdmitted = $false }
        finally { if ($null -ne $candidate) { $candidate.Dispose() } }
    }
    if (-not $allAdmitted) { throw 'refused replacement identity; handles disposed only' }
}
$suffix = [Guid]::NewGuid().ToString('N')
$scratch = Join-Path $env:TEMP ('crosspane-WP-W1.5b-' + $suffix)
$process = $null
$helper = $null
$created = 0
$replacements = [Collections.Generic.List[Diagnostics.Process]]::new()
$saved = @{}
$variables = @('APPDATA', 'LOCALAPPDATA', 'CROSSPANE_RUNTIME_DIR', 'CROSSPANE_ACCEPTANCE_E1_ONLY', 'CROSSPANE_DISCOVERY', 'CROSSPANE_AUDIO', 'CROSSPANE_GPU', 'CROSSPANE_INSTALLER_STOP_LIVE', 'CROSSPANE_INSTALLER_STOP_AGENT_PID')
try {
    foreach ($name in $variables) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
    $env:APPDATA = Join-Path $scratch 'roaming'
    $env:LOCALAPPDATA = Join-Path $scratch 'local'
    $env:CROSSPANE_RUNTIME_DIR = Join-Path $scratch 'runtime'
    $env:CROSSPANE_ACCEPTANCE_E1_ONLY = '1'
    $env:CROSSPANE_DISCOVERY = '0'
    $env:CROSSPANE_AUDIO = '0'
    $env:CROSSPANE_GPU = '0'
    $config = Join-Path $env:APPDATA 'Crosspane'
    New-Item -ItemType Directory -Path $config -Force | Out-Null
    # No port picker: port zero is ephemeral; approved seam admits only 127.0.0.1 before any bind.
    [IO.File]::WriteAllText((Join-Path $config 'config.toml'), ('name = "wp-w1-5b-' + $suffix + '"' + "`nport = 0`nforce_file_keystore = true`nacceptance_bind_ip = '127.0.0.1'`n"))
    $process = Start-Process -FilePath $Agent -ArgumentList 'run' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'agent.out') -RedirectStandardError (Join-Path $scratch 'agent.err')
    $pin = $process.Handle
    $created = [InstallerStopFixture]::Created($pin)
    if ($created -eq 0) { throw 'owned agent creation identity unavailable' }
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $status = $null
    do {
        if ($process.HasExited) { throw "owned agent exited before ready: $($process.ExitCode); $(Own-Error (Join-Path $scratch 'agent.err'))" }
        $status = Scratch-Status
        if ($null -eq $status) { Start-Sleep -Milliseconds 100 }
    } while ($null -eq $status -and [DateTime]::UtcNow -lt $deadline)
    if ($null -eq $status -or $status.name -ne ('wp-w1-5b-' + $suffix) -or $status.installer.instance.pid -ne $process.Id -or $null -ne $status.installer.instance.uid -or $status.installer.keystore -ne 'file' -or @($status.peers).Count -ne 0) { throw 'owned scratch Status admission failed' }
    'scratch_status_ready=true; file_keystore=true; peers=0'
    $env:CROSSPANE_INSTALLER_STOP_LIVE = '1'
    $env:CROSSPANE_INSTALLER_STOP_AGENT_PID = [string]$process.Id
    $outputPath = Join-Path $scratch 'helper.out'
    $errorPath = Join-Path $scratch 'helper.err'
    $helper = Start-Process -FilePath $CtlTest -ArgumentList '--exact installer_stop_live_tests::installer_stop_stale_then_matching_ack_and_exact_receipt --ignored --nocapture' -PassThru -WindowStyle Hidden -RedirectStandardOutput $outputPath -RedirectStandardError $errorPath
    $helperPin = $helper.Handle
    $ackBeforeExit = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(50)
    while (-not $process.HasExited -and [DateTime]::UtcNow -lt $deadline) {
        $ackPath = Join-Path $scratch 'stop-ack'
        if (Test-Path -LiteralPath $ackPath) {
            if ([IO.File]::ReadAllText($ackPath) -eq 'ack_received=true' -and -not $process.HasExited) { $ackBeforeExit = $true }
        }
        if ($helper.HasExited -and -not $process.HasExited) { throw "owned stop helper exited early: $($helper.ExitCode); $(Own-Error $outputPath); $(Own-Error $errorPath)" }
        Start-Sleep -Milliseconds 10
    }
    if (-not $process.WaitForExit(1000)) { throw 'owned installer stop did not exit within bound' }
    $process.Refresh()
    if ($process.ExitCode -ne 0) { throw 'owned installer stop did not exit zero' }
    $markerPath = Join-Path $scratch 'agent-exit-observed.tmp'
    $marker = [IO.File]::Open($markerPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try { $bytes = [Text.Encoding]::ASCII.GetBytes('native_exit=0'); $marker.Write($bytes, 0, $bytes.Length) } finally { $marker.Dispose() }
    [IO.File]::Move($markerPath, (Join-Path $scratch 'agent-exit-observed'))
    if (-not $helper.WaitForExit(10000)) { throw 'owned helper did not verify completion within bound' }
    $helper.Refresh()
    if ($helper.ExitCode -ne 0) { throw "owned helper failed: $($helper.ExitCode); $(Own-Error $outputPath); $(Own-Error $errorPath)" }
    $text = Own-Error $outputPath
    if (-not $text.Contains('test result: ok. 1 passed; 0 failed;') -or -not $text.Contains('installer_stop_native_exit_and_receipt=true; exact_u64_instance=true; receipt_clean=true')) { throw 'owned one-test completion proof absent' }
    $text
    Retain-Replacements
    if ($replacements.Count -ne 0) { throw 'accepted installer stop created a replacement child' }
    "ack_before_exit=$($ackBeforeExit.ToString().ToLowerInvariant()); native_exit=0; matching_receipt=true; no_replacement=true"
} finally {
    $cleanupOk = $true
    try {
        if ($null -ne $helper -and -not (Stop-Owned $helper)) { $cleanupOk = $false }
        if ($null -ne $process -and -not (Stop-Owned $process -KeepPin)) { $cleanupOk = $false }
        if ($null -ne $process) { try { Retain-Replacements } catch { $cleanupOk = $false } }
        foreach ($replacement in $replacements) { if (-not (Stop-Owned $replacement)) { $cleanupOk = $false } }
    } finally {
        if ($null -ne $process) { try { $process.Dispose() } catch { $cleanupOk = $false } }
        foreach ($name in $variables) { try { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') } catch { $cleanupOk = $false } }
        try { if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force } } catch { $cleanupOk = $false }
    }
    if (-not $cleanupOk) { throw 'owned cleanup failed; all stages attempted' }
    if (Test-Path -LiteralPath $scratch) { throw 'owned scratch residue remained' }
    'scratch_cleanup=true'
}
