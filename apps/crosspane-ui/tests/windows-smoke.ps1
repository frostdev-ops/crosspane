# Limited interactive acceptance. Every observed HWND belongs to a child created here.
param(
    [Parameter(Mandatory=$true)][string]$Agent,
    [Parameter(Mandatory=$true)][string]$Ctl,
    [Parameter(Mandatory=$true)][string]$Ui,
    [string]$LaunchTestExe,
    [switch]$PopupWorker,
    [long]$Popup,
    [long]$Tray,
    [uint32]$AgentPid,
    [long]$AgentCreated
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, WindowsBase
Add-Type -ReferencedAssemblies @(
    [System.Windows.Automation.AutomationElement].Assembly.Location,
    [System.Windows.Automation.ControlType].Assembly.Location,
    [System.Windows.Rect].Assembly.Location
) -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Windows.Automation;
public static class CrosspaneUiSmoke {
    public delegate bool EnumProc(IntPtr window, IntPtr arg);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr arg);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr hwnd, StringBuilder text, int length);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern IntPtr GetWindowDpiAwarenessContext(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool AreDpiAwarenessContextsEqual(IntPtr left, IntPtr right);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, UIntPtr wp, IntPtr lp);
    [StructLayout(LayoutKind.Sequential)] public struct Icon { public uint size; public IntPtr hwnd; public uint id; public Guid guid; }
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int left, top, right, bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct Point { public int x, y; }
    [DllImport("shell32.dll")] static extern int Shell_NotifyIconGetRect(ref Icon icon, out Rect rectangle);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd, out Rect rectangle);
    [DllImport("user32.dll")] static extern IntPtr WindowFromPoint(Point point);
    [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint low, high; public long Value { get { return ((long)high << 32) | low; } } }
    [DllImport("kernel32.dll")] static extern uint GetProcessId(IntPtr process);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll")] static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder image, ref uint length);
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct ProcessEntry {
        public uint size, usage, pid; public UIntPtr heap; public uint module, threads, parent;
        public int priority; public uint flags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst=260)] public string file;
    }
    [DllImport("kernel32.dll")] static extern IntPtr CreateToolhelp32Snapshot(uint flags, uint pid);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32First(IntPtr snapshot, ref ProcessEntry entry);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode)] static extern bool Process32Next(IntPtr snapshot, ref ProcessEntry entry);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static long Created(IntPtr process) {
        FileTime creation, exit, kernel, user;
        return GetProcessTimes(process, out creation, out exit, out kernel, out user) ? creation.Value : 0;
    }
    public static bool ImageIs(IntPtr process, string expected) {
        var image = new StringBuilder(32768); uint length = (uint)image.Capacity;
        return QueryFullProcessImageName(process, 0, image, ref length)
            && String.Equals(image.ToString(), expected, StringComparison.OrdinalIgnoreCase);
    }
    public static bool AdmitChild(IntPtr handle, uint pid, uint parent, IntPtr parentHandle, long parentCreated, long observedCreation, string observedPath) {
        long creation = Created(handle);
        FileTime parentCreation, parentExit, kernel, user;
        if (GetProcessId(parentHandle) != parent || parentCreated == 0
            || !GetProcessTimes(parentHandle, out parentCreation, out parentExit, out kernel, out user)
            || parentCreation.Value != parentCreated || creation < parentCreated) return false;
        uint parentState = WaitForSingleObject(parentHandle, 0);
        if (parentState == 0) {
            if (parentExit.Value < parentCreated || creation > parentExit.Value) return false;
        } else if (parentState != 258) return false;
        // CIM dates have microsecond precision. All other checks use the exact retained handle.
        if (GetProcessId(handle) != pid || creation == 0 || creation / 10 != observedCreation / 10 || String.IsNullOrEmpty(observedPath)) return false;
        IntPtr snapshot = CreateToolhelp32Snapshot(2, 0);
        if (snapshot == new IntPtr(-1)) return false;
        bool parentMatches = false;
        try {
            var entry = new ProcessEntry { size = (uint)Marshal.SizeOf(typeof(ProcessEntry)) };
            if (Process32First(snapshot, ref entry)) do {
                if (entry.pid != pid) continue;
                parentMatches = entry.parent == parent && String.Equals(entry.file, "crosspane-ui.exe", StringComparison.OrdinalIgnoreCase);
                break;
            } while (Process32Next(snapshot, ref entry));
        } finally { CloseHandle(snapshot); }
        if (!parentMatches) return false;
        var image = new StringBuilder(32768); uint length = (uint)image.Capacity;
        return QueryFullProcessImageName(handle, 0, image, ref length)
            && String.Equals(image.ToString(), observedPath, StringComparison.OrdinalIgnoreCase)
            && Created(handle) == creation;
    }
    static bool Own(IntPtr hwnd, uint pid) { uint actual; GetWindowThreadProcessId(hwnd, out actual); return hwnd != IntPtr.Zero && actual == pid; }
    static string Class(IntPtr hwnd) { var text = new StringBuilder(256); GetClassName(hwnd, text, text.Capacity); return text.ToString(); }
    public static IntPtr OwnWindow(uint pid, string prefix, bool visible) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, arg) => {
            if (!Own(hwnd, pid)) return true;
            // Only own PID matches reach class/visibility queries. No window titles are read.
            if ((!visible || IsWindowVisible(hwnd)) && Class(hwnd).StartsWith(prefix, StringComparison.Ordinal)) {
                found = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static bool PerMonitorV2(IntPtr hwnd, uint pid) {
        return Own(hwnd, pid) && AreDpiAwarenessContextsEqual(GetWindowDpiAwarenessContext(hwnd), new IntPtr(-4));
    }
    public static bool OpenOwnTray(IntPtr hwnd, uint pid) {
        if (!Own(hwnd, pid) || !Class(hwnd).StartsWith("CrosspaneTray-", StringComparison.Ordinal)) return false;
        var icon = new Icon { size = (uint)Marshal.SizeOf(typeof(Icon)), hwnd = hwnd, id = 1 };
        Rect rectangle;
        if (Shell_NotifyIconGetRect(ref icon, out rectangle) != 0 || rectangle.right <= rectangle.left || rectangle.bottom <= rectangle.top) return false;
        int x = (rectangle.left + rectangle.right) / 2, y = (rectangle.top + rectangle.bottom) / 2;
        uint point = (uint)(ushort)x | ((uint)(ushort)y << 16);
        // Documented version-4 notification callback to this exact owned icon; no global input.
        return PostMessage(hwnd, 0x8000 + 12, new UIntPtr(point), new IntPtr((1 << 16) | 0x400));
    }
    public static IntPtr OwnPopup(IntPtr tray, uint pid) {
        if (!Own(tray, pid)) return IntPtr.Zero;
        uint ignored; uint thread = GetWindowThreadProcessId(tray, out ignored);
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, arg) => {
            if (!Own(hwnd, pid)) return true;
            uint actual; uint candidateThread = GetWindowThreadProcessId(hwnd, out actual);
            if (candidateThread == thread && IsWindowVisible(hwnd) && Class(hwnd) == "#32768" && GetWindow(hwnd, 4) == tray) {
                found = hwnd; return false;
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    static bool PopupAdmitted(IntPtr popup, IntPtr tray, uint pid) {
        if (OwnPopup(tray, pid) != popup || popup == IntPtr.Zero || !Own(GetForegroundWindow(), pid)) return false;
        Rect rectangle;
        if (!GetWindowRect(popup, out rectangle) || rectangle.right <= rectangle.left || rectangle.bottom <= rectangle.top) return false;
        // A covering foreign window causes a conservative refusal; no foreign label is inspected.
        var point = new Point { x = (rectangle.left + rectangle.right) / 2, y = (rectangle.top + rectangle.bottom) / 2 };
        return WindowFromPoint(point) == popup;
    }
    public static bool InvokeOwnSettings(IntPtr popup, IntPtr tray, uint pid) {
        if (!PopupAdmitted(popup, tray, pid)) return false;
        // The caller runs this synchronously in a separately retained, bounded helper process.
        // No UIA work survives helper termination. The tree stays inside the admitted popup.
        try {
                if (!PopupAdmitted(popup, tray, pid)) return false;
                var root = AutomationElement.FromHandle(popup);
                if (root.Current.ProcessId != pid || root.Current.NativeWindowHandle != popup.ToInt32()) return false;
                var items = root.FindAll(TreeScope.Children, Condition.TrueCondition);
                foreach (AutomationElement item in items) {
                    // Read labels only after the element reports the owned popup's exact PID.
                    if (item.Current.ProcessId != pid) return false;
                    if (item.Current.ControlType != ControlType.MenuItem) continue;
                    if (item.Current.Name != "Settings\u2026") continue;
                    object pattern;
                    if (!item.TryGetCurrentPattern(InvokePattern.Pattern, out pattern) || !PopupAdmitted(popup, tray, pid)) return false;
                    ((InvokePattern)pattern).Invoke();
                    return true;
                }
        } catch { return false; }
        return false;
    }
    public static bool EndOwnSession(IntPtr hwnd, uint pid) { return Own(hwnd, pid) && PostMessage(hwnd, 22, new UIntPtr(1u), IntPtr.Zero); }
    public static void CloseOwn(IntPtr hwnd, uint pid) { if (Own(hwnd, pid)) PostMessage(hwnd, 16, UIntPtr.Zero, IntPtr.Zero); }
    public static void CancelOwnPopup(IntPtr tray, uint pid) { if (Own(tray, pid)) PostMessage(tray, 31, UIntPtr.Zero, IntPtr.Zero); }
}
'@
function Stop-OwnedProcess([Diagnostics.Process]$child, [switch]$KeepPin) {
    $ok = $true
    $settled = $false
    try {
        if (-not $child.HasExited) {
            $child.Kill()
            $settled = $child.WaitForExit(3000)
        } else { $settled = $true }
    } catch { $ok = $false }
    finally {
        # Never release a live helper's identity pin on failed termination; outer cleanup retries.
        if ($settled -and -not $KeepPin) { try { $child.Dispose() } catch { $ok = $false } }
    }
    return $ok -and $settled
}
if ($PopupWorker) {
    # Exact parent identity is pinned before the admitted popup is queried. Never terminate it.
    $ownedAgent = $null
    try {
        $ownedAgent = [Diagnostics.Process]::GetProcessById([int]$AgentPid)
        $handle = $ownedAgent.Handle
        if ($AgentCreated -eq 0 -or [CrosspaneUiSmoke]::Created($handle) -ne $AgentCreated -or $ownedAgent.HasExited) { exit 5 }
        if ([CrosspaneUiSmoke]::InvokeOwnSettings([IntPtr]$Popup, [IntPtr]$Tray, $AgentPid)) { exit 0 }
        exit 4
    } finally { if ($null -ne $ownedAgent) { $ownedAgent.Dispose() } }
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
    $handle = $child.Handle
    try {
        $output = $child.StandardOutput.ReadToEndAsync()
        $error = $child.StandardError.ReadToEndAsync()
        if (-not $child.WaitForExit(15000)) { throw 'owned Status child timed out' }
        if ($child.ExitCode -eq 0) {
            $response = $output.Result | ConvertFrom-Json
            if ($response.ok -ne $true -or $null -eq $response.result) { throw 'owned Status response envelope malformed' }
            return $response.result
        }
        return $null
    } finally { if (-not (Stop-OwnedProcess $child)) { throw 'owned Status child cleanup failed' } }
}
function Invoke-OwnMenu([IntPtr]$popup, [IntPtr]$tray) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = Join-Path $PSHOME 'powershell.exe'
    $info.Arguments = '-NoProfile -File "' + $PSCommandPath + '" -Agent "' + $Agent + '" -Ctl "' + $Ctl + '" -Ui "' + $Ui + '" -PopupWorker -Popup ' + $popup.ToInt64() + ' -Tray ' + $tray.ToInt64() + ' -AgentPid ' + $agentProcess.Id + ' -AgentCreated ' + [CrosspaneUiSmoke]::Created($agentProcess.Handle)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $script:popupProcess = [Diagnostics.Process]::Start($info)
    try {
        $handle = $script:popupProcess.Handle
        $output = $script:popupProcess.StandardOutput.ReadToEndAsync()
        $error = $script:popupProcess.StandardError.ReadToEndAsync()
        if (-not $script:popupProcess.WaitForExit(5000)) { return $false }
        if ($script:popupProcess.ExitCode -notin @(0, 4)) { throw 'owned popup helper failed before admission' }
        return $script:popupProcess.ExitCode -eq 0
    } finally {
        # Settle helper before popup cancellation, child discovery, or any outer cleanup.
        if (-not (Stop-OwnedProcess $script:popupProcess)) { throw 'owned popup helper could not be settled' }
        $script:popupProcess = $null
    }
}
function Retain-UiChildren([Diagnostics.Process]$parentProcess, [long]$parentCreated) {
    $candidates = @(Get-CimInstance Win32_Process -Filter ("ParentProcessId = " + $parentProcess.Id + " AND Name = 'crosspane-ui.exe'"))
    $admitted = [Collections.Generic.List[Diagnostics.Process]]::new()
    $allAdmitted = $true
    foreach ($candidate in $candidates) {
        $pinned = $null
        try {
            $pinned = [Diagnostics.Process]::GetProcessById([int]$candidate.ProcessId)
            $pin = $pinned.Handle
            if (-not [CrosspaneUiSmoke]::AdmitChild($pin, [uint32]$candidate.ProcessId, [uint32]$parentProcess.Id, $parentProcess.Handle, $parentCreated, $candidate.CreationDate.ToFileTimeUtc(), $candidate.ExecutablePath)) { throw 'owned UI child native identity admission failed' }
            # Retain every admitted child before any acceptance assertion. Continue after refusals.
            $existing = $null
            $created = [CrosspaneUiSmoke]::Created($pin)
            foreach ($known in $uiChildren) {
                if ($known.Id -ne $pinned.Id) { continue }
                try { $knownCreated = [CrosspaneUiSmoke]::Created($known.Handle) } catch { continue }
                if ($knownCreated -ne 0 -and $knownCreated -eq $created) { $existing = $known; break }
            }
            if ($null -eq $existing) {
                $uiChildren.Add($pinned)
                $admitted.Add($pinned)
                $pinned = $null
            } else { $admitted.Add($existing) }
        } catch { $allAdmitted = $false }
        finally { if ($null -ne $pinned) { $pinned.Dispose() } }
    }
    if (-not $allAdmitted) { throw 'at least one owned UI child failed identity admission; refused handles were disposed only' }
    return $admitted.ToArray()
}
function Own-UiWindow([Diagnostics.Process]$child) {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    do {
        if ($child.HasExited) { throw "owned UI exited before HWND admission: $($child.ExitCode)" }
        $hwnd = [CrosspaneUiSmoke]::OwnWindow([uint32]$child.Id, '', $true)
        if ($hwnd -ne [IntPtr]::Zero) {
            if (-not [CrosspaneUiSmoke]::PerMonitorV2($hwnd, [uint32]$child.Id)) { throw 'owned UI HWND is not per-monitor-v2 DPI aware' }
            return $hwnd
        }
        Start-Sleep -Milliseconds 10
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'owned UI HWND did not become available'
}
function Read-OwnError([string]$path) {
    if (Test-Path -LiteralPath $path) {
        $text = [IO.File]::ReadAllText($path)
        return $text.Substring([Math]::Max(0, $text.Length - 8192))
    }
    return ''
}
$suffix = [Guid]::NewGuid().ToString('N')
$scratch = Join-Path $env:TEMP ('crosspane-WP-W1.5b-' + $suffix)
$agentProcess = $null
$agentCreated = 0
$launcherProcess = $null
$launcherCreated = 0
$script:popupProcess = $null
$uiChildren = [Collections.Generic.List[Diagnostics.Process]]::new()
$saved = @{}
$variables = @('APPDATA', 'LOCALAPPDATA', 'CROSSPANE_RUNTIME_DIR', 'CROSSPANE_ACCEPTANCE_E1_ONLY', 'CROSSPANE_AUDIO', 'CROSSPANE_DISCOVERY', 'CROSSPANE_GPU', 'CROSSPANE_UI_DEMO', 'CROSSPANE_UI_SCREENSHOT', 'CROSSPANE_UI_TAB', 'CROSSPANE_UI_ACCEPTANCE_LIVE', 'CROSSPANE_UI_ACCEPTANCE_AGENT_PID', 'WGPU_TRACE', 'PATH')
try {
    foreach ($variable in $variables) { $saved[$variable] = [Environment]::GetEnvironmentVariable($variable, 'Process') }
    foreach ($variable in @('CROSSPANE_UI_DEMO', 'CROSSPANE_UI_SCREENSHOT', 'CROSSPANE_UI_TAB', 'CROSSPANE_UI_ACCEPTANCE_LIVE', 'CROSSPANE_UI_ACCEPTANCE_AGENT_PID', 'WGPU_TRACE')) { [Environment]::SetEnvironmentVariable($variable, $null, 'Process') }
    $env:APPDATA = Join-Path $scratch 'roaming'
    $env:LOCALAPPDATA = Join-Path $scratch 'local'
    $env:CROSSPANE_RUNTIME_DIR = Join-Path $scratch 'runtime'
    $env:CROSSPANE_ACCEPTANCE_E1_ONLY = '1'
    $env:CROSSPANE_AUDIO = '0'
    $env:CROSSPANE_DISCOVERY = '0'
    $env:CROSSPANE_GPU = '0'
    $config = Join-Path $env:APPDATA 'Crosspane'
    $screenshots = Join-Path $scratch 'screenshots'
    New-Item -ItemType Directory -Path $config, $screenshots -Force | Out-Null
    $socket = [System.Net.Sockets.UdpClient]::new(0)
    $port = $socket.Client.LocalEndPoint.Port
    $socket.Dispose()
    [IO.File]::WriteAllText((Join-Path $config 'config.toml'), ('name = "wp-w1-5b-' + $suffix + '"' + "`nport = $port`nforce_file_keystore = true`n"))
    $agentProcess = Start-Process -FilePath $Agent -ArgumentList 'run' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'agent.out') -RedirectStandardError (Join-Path $scratch 'agent.err')
    $agentHandle = $agentProcess.Handle
    $agentCreated = [CrosspaneUiSmoke]::Created($agentHandle)
    if ($agentCreated -eq 0) { throw 'owned agent creation identity unavailable' }
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    $status = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($agentProcess.HasExited) { throw "fixture agent exited before readiness: $($agentProcess.ExitCode)" }
        $status = Scratch-Status
        if ($null -ne $status) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($null -eq $status -or $status.name -ne ('wp-w1-5b-' + $suffix) -or $status.installer.instance.pid -ne $agentProcess.Id -or $null -ne $status.installer.instance.uid -or $status.installer.keystore -ne 'file' -or @($status.peers).Count -ne 0) { throw 'Status did not admit the exact isolated file-keystore child' }
    'scratch_status_ready=true; windows_uid=null; scratch_keystore=file; peers=0'
    if ($LaunchTestExe) {
        # This branch exercises the actual agent function, with no additional popup attempt.
        $bin = Join-Path $scratch 'bin'
        $fallback = Join-Path $scratch 'path'
        New-Item -ItemType Directory -Path $bin, $fallback | Out-Null
        $testCopy = Join-Path $bin 'launcher-test.exe'
        $sibling = Join-Path $bin 'crosspane-ui.exe'
        Copy-Item -LiteralPath $LaunchTestExe -Destination $testCopy
        Copy-Item -LiteralPath $Ui -Destination $sibling
        Copy-Item -LiteralPath $Ui -Destination (Join-Path $fallback 'crosspane-ui.exe')
        $env:PATH = $fallback + ';' + $saved['PATH']
        $env:CROSSPANE_UI_ACCEPTANCE_LIVE = '1'
        $env:CROSSPANE_UI_ACCEPTANCE_AGENT_PID = [string]$agentProcess.Id
        $env:CROSSPANE_UI_TAB = 'machines'
        $env:CROSSPANE_UI_SCREENSHOT = Join-Path $screenshots 'machines.png'
        $launcherError = Join-Path $scratch 'launcher.err'
        $launcherOutput = Join-Path $scratch 'launcher.out'
        $launcherProcess = Start-Process -FilePath $testCopy -ArgumentList '--exact agent::home_tests::windows_settings_launcher_uses_sibling_and_refuses_missing --ignored --nocapture' -PassThru -RedirectStandardOutput $launcherOutput -RedirectStandardError $launcherError
        $launcherHandle = $launcherProcess.Handle
        $launcherCreated = [CrosspaneUiSmoke]::Created($launcherHandle)
        if ($launcherCreated -eq 0) { throw 'owned launcher creation identity unavailable' }
        $childDeadline = [DateTime]::UtcNow.AddSeconds(20)
        $admitted = @()
        do {
            if ($launcherProcess.HasExited) { throw "actual launcher test exited before child admission: $($launcherProcess.ExitCode); $(Read-OwnError $launcherOutput); $(Read-OwnError $launcherError)" }
            $admitted = @(Retain-UiChildren $launcherProcess $launcherCreated)
            if ($admitted.Count -eq 0) { Start-Sleep -Milliseconds 10 }
        } while ($admitted.Count -eq 0 -and [DateTime]::UtcNow -lt $childDeadline)
        if ($admitted.Count -ne 1 -or -not [CrosspaneUiSmoke]::ImageIs($admitted[0].Handle, $sibling)) { throw 'actual launcher did not create exactly its copied sibling' }
        $launchedUi = $admitted[0]
        $hwnd = Own-UiWindow $launchedUi
        if (-not $launchedUi.WaitForExit(60000)) { throw 'actual launcher UI capture timed out' }
        $launchedUi.Refresh()
        if ($launchedUi.ExitCode -ne 0) { throw 'actual launcher UI exit was not zero' }
        $bytes = [IO.File]::ReadAllBytes($env:CROSSPANE_UI_SCREENSHOT)
        if ($bytes.Length -lt 1024) { throw 'actual launcher own-renderer PNG was too small' }
        'SCREENSHOT:machines:' + [Convert]::ToBase64String($bytes)
        # Signal only after native creation/image admission, PMv2/live capture and child exit zero.
        $markerPath = Join-Path $scratch 'launcher-child-settled.tmp'
        $marker = [IO.File]::Open($markerPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $markerBytes = [Text.Encoding]::ASCII.GetBytes('native_sibling_exit=0'); $marker.Write($markerBytes, 0, $markerBytes.Length) } finally { $marker.Dispose() }
        # Publish only the closed marker, so the test never observes a still-exclusive writer.
        [IO.File]::Move($markerPath, (Join-Path $scratch 'launcher-child-settled'))
        if (-not $launcherProcess.WaitForExit(15000)) { throw 'actual launcher test did not finish its missing-sibling check' }
        $launcherProcess.Refresh()
        if ($launcherProcess.ExitCode -ne 0) { throw "actual launcher test failed: $($launcherProcess.ExitCode); $(Read-OwnError $launcherError)" }
        $errorText = Read-OwnError $launcherError
        if (-not $errorText.Contains('scratch_ui_backend=Dx12; scratch_ui_device=Cpu') -or -not $errorText.Contains('scratch_ui_live_status=true; scratch_ui_capture=true')) { throw 'actual launcher child did not confirm authenticated software DX12 capture' }
        $outputText = Read-OwnError $launcherOutput
        if (-not $outputText.Contains('actual_settings_launcher=true; sibling_exit=0; absent_sibling=NotFound; no_path_fallback=true')) { throw 'actual launcher test confirmation absent' }
        if (-not $outputText.Contains('test result: ok. 1 passed; 0 failed;')) { throw 'actual launcher harness did not confirm exactly one passing test' }
        # Only this exited, retained harness's bounded output; no unrelated test/agent logs.
        $outputText
        if ((Test-Path -LiteralPath $sibling) -or -not (Test-Path -LiteralPath (Join-Path $fallback 'crosspane-ui.exe'))) { throw 'missing-sibling test did not preserve the PATH decoy' }
        $remaining = @(Retain-UiChildren $launcherProcess $launcherCreated)
        if ($remaining.Count -ne 0) { throw 'actual launcher left a PATH fallback or other child running' }
        'actual_settings_launcher=true; sibling_native_admitted=true; sibling_exit=0; absent_sibling=NotFound; no_path_fallback=true; dpi=PerMonitorV2; connected_capture=true'
    } else {
    foreach ($tab in @('machines', 'layout', 'pairing', 'windows')) {
        $env:CROSSPANE_UI_ACCEPTANCE_LIVE = '1'
        $env:CROSSPANE_UI_ACCEPTANCE_AGENT_PID = [string]$agentProcess.Id
        $env:CROSSPANE_UI_TAB = $tab
        $env:CROSSPANE_UI_SCREENSHOT = Join-Path $screenshots ($tab + '.png')
        $errorPath = Join-Path $scratch ($tab + '.err')
        $child = Start-Process -FilePath $Ui -PassThru -RedirectStandardOutput (Join-Path $scratch ($tab + '.out')) -RedirectStandardError $errorPath
        $uiChildren.Add($child)
        $childHandle = $child.Handle
        $hwnd = Own-UiWindow $child
        if (-not $child.WaitForExit(60000)) { throw 'owned UI capture did not finish within sixty seconds' }
        $child.Refresh()
        if ($child.ExitCode -ne 0) { throw "owned UI capture failed: $($child.ExitCode); $(Read-OwnError $errorPath)" }
        $errorText = Read-OwnError $errorPath
        if (-not $errorText.Contains('scratch_ui_backend=Dx12; scratch_ui_device=Cpu') -or -not $errorText.Contains('scratch_ui_live_status=true; scratch_ui_capture=true')) { throw 'owned UI did not confirm software DX12 and authenticated live capture' }
        $bytes = [IO.File]::ReadAllBytes($env:CROSSPANE_UI_SCREENSHOT)
        if ($bytes.Length -lt 1024) { throw 'own-renderer PNG was too small' }
        # Transport only the own renderer's PNG. The launcher redirects this to a local artifact log.
        'SCREENSHOT:' + $tab + ':' + [Convert]::ToBase64String($bytes)
        "scratch_ui_tab=$tab; connected_capture=true; dpi=PerMonitorV2; dx12_cpu=true; own_exit=0"
    }
    foreach ($variable in @('CROSSPANE_UI_SCREENSHOT', 'CROSSPANE_UI_TAB', 'CROSSPANE_UI_ACCEPTANCE_LIVE', 'CROSSPANE_UI_ACCEPTANCE_AGENT_PID')) { [Environment]::SetEnvironmentVariable($variable, $null, 'Process') }
    # The agent inherited this clean UI environment at launch; tray children use production UI mode.
    $tray = [IntPtr]::Zero
    $trayDeadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        $tray = [CrosspaneUiSmoke]::OwnWindow([uint32]$agentProcess.Id, 'CrosspaneTray-', $false)
        if ($tray -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 100 }
    } while ($tray -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $trayDeadline)
    if ($tray -eq [IntPtr]::Zero) { throw 'owned tray HWND absent' }
    $settingsVerified = $false
    $popup = [IntPtr]::Zero
    try {
        if ([CrosspaneUiSmoke]::OpenOwnTray($tray, [uint32]$agentProcess.Id)) {
            $popupDeadline = [DateTime]::UtcNow.AddSeconds(3)
            do {
                $popup = [CrosspaneUiSmoke]::OwnPopup($tray, [uint32]$agentProcess.Id)
                if ($popup -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 50 }
            } while ($popup -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $popupDeadline)
            if ($popup -ne [IntPtr]::Zero -and (Invoke-OwnMenu $popup $tray)) {
                $launchDeadline = [DateTime]::UtcNow.AddSeconds(10)
                $launched = @()
                do {
                    # Query only this owned parent's named UI children, never unrelated command lines.
                    $launched = @(Get-CimInstance Win32_Process -Filter ("ParentProcessId = " + $agentProcess.Id + " AND Name = 'crosspane-ui.exe'"))
                    if ($launched.Count -eq 0) { Start-Sleep -Milliseconds 100 }
                } while ($launched.Count -eq 0 -and [DateTime]::UtcNow -lt $launchDeadline)
                $admitted = @(Retain-UiChildren $agentProcess $agentCreated)
                if ($admitted.Count -ne 1 -or $launched[0].ExecutablePath -ne [IO.Path]::GetFullPath($Ui)) { throw 'own Settings invocation did not launch exactly the sibling UI' }
                $settingsChild = $admitted[0]
                $settingsHwnd = Own-UiWindow $settingsChild
                $status = Scratch-Status
                if ($null -eq $status -or $status.installer.settings_opened -lt 1) { throw 'own tray Settings spawn was not recorded' }
                [CrosspaneUiSmoke]::CloseOwn($settingsHwnd, [uint32]$settingsChild.Id)
                if (-not $settingsChild.WaitForExit(10000)) { throw 'own tray UI did not close' }
                $settingsChild.Refresh()
                if ($settingsChild.ExitCode -ne 0) { throw 'own tray UI close was not clean' }
                $settingsVerified = $true
            }
        }
    } finally { [CrosspaneUiSmoke]::CancelOwnPopup($tray, [uint32]$agentProcess.Id) }
    if ($settingsVerified) { 'scratch_tray_settings=verified; sibling_launch=true; dpi=PerMonitorV2' }
    else { 'scratch_tray_settings=unverified; reason=owned_popup_not_safely_admitted_or_invokable; direct_own_spawn_verified=true' }
    }
    $observer = [CrosspaneUiSmoke]::OwnWindow([uint32]$agentProcess.Id, ('CrosspaneShutdown' + $agentProcess.Id), $false)
    if ($observer -eq [IntPtr]::Zero -or -not [CrosspaneUiSmoke]::EndOwnSession($observer, [uint32]$agentProcess.Id)) { throw 'owned shutdown observer unavailable' }
    if (-not $agentProcess.WaitForExit(7000)) { throw 'scratch agent did not stop within seven seconds' }
    $agentProcess.Refresh()
    if ($agentProcess.ExitCode -ne 0) { throw "scratch clean stop exited $($agentProcess.ExitCode)" }
    $receipt = Get-Content -Raw (Join-Path $env:LOCALAPPDATA 'Crosspane/last_exit.json') | ConvertFrom-Json
    if (-not $receipt.clean) { throw 'scratch receipt was not clean' }
    $agentError = Read-OwnError (Join-Path $scratch 'agent.err')
    if ($agentError.Contains('hotkey chord not configured') -or $agentError.Contains('global hotkeys unavailable')) { throw 'scratch native hotkey subscription unavailable' }
    'scratch_clean_stop=true; scratch_receipt_clean=true; scratch_hotkey_subscription_configured=true'
} finally {
    $cleanupOk = $true
    try {
        if ($null -ne $script:popupProcess -and -not (Stop-OwnedProcess $script:popupProcess)) { $cleanupOk = $false }
        # Settle the parent too: an already-queued Settings action must not spawn after the scan.
        # Retain its handle/creation/exit identity through final child admission and cleanup.
        if ($null -ne $launcherProcess -and -not (Stop-OwnedProcess $launcherProcess -KeepPin)) { $cleanupOk = $false }
        if ($null -ne $agentProcess -and -not (Stop-OwnedProcess $agentProcess -KeepPin)) { $cleanupOk = $false }
        # Discover children only after helper and parent settlement, with this run's lifetime bound.
        if ($null -ne $launcherProcess) {
            try { $null = @(Retain-UiChildren $launcherProcess $launcherCreated) } catch { $cleanupOk = $false }
        }
        if ($null -ne $agentProcess) {
            try { $null = @(Retain-UiChildren $agentProcess $agentCreated) } catch { $cleanupOk = $false }
        }
        foreach ($child in $uiChildren) { if (-not (Stop-OwnedProcess $child)) { $cleanupOk = $false } }
    } finally {
        if ($null -ne $launcherProcess) { try { $launcherProcess.Dispose() } catch { $cleanupOk = $false } }
        if ($null -ne $agentProcess) { try { $agentProcess.Dispose() } catch { $cleanupOk = $false } }
        foreach ($variable in $variables) {
            try { [Environment]::SetEnvironmentVariable($variable, $saved[$variable], 'Process') } catch { $cleanupOk = $false }
        }
        try { if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force } } catch { $cleanupOk = $false }
    }
    if (-not $cleanupOk) { throw 'owned fixture cleanup failed; all cleanup stages were attempted' }
    if (Test-Path -LiteralPath $scratch) { throw 'scratch residue remained' }
    'scratch_cleanup=true'
}
