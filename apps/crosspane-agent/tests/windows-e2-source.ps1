# Limited own-window source acceptance. Never a monitor capture or owner input operation.
param([Parameter(Mandatory=$true)][string]$Agent,[Parameter(Mandatory=$true)][string]$DestinationTest)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class OwnedSourceRun {
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
    public static string Image(IntPtr process) {
        var image = new StringBuilder(32768); uint length = (uint)image.Capacity;
        if (!QueryFullProcessImageName(process, 0, image, ref length)) throw new InvalidOperationException("owned process image query");
        return image.ToString();
    }
    public static uint Pid(IntPtr process) { return GetProcessId(process); }
    public static long Created(IntPtr process) {
        FileTime creation, exit, kernel, user;
        return GetProcessTimes(process, out creation, out exit, out kernel, out user) ? creation.Value : 0;
    }
    public static bool AdmitReplacement(IntPtr child, uint pid, IntPtr parent, uint parentPid, long parentCreated, long observedCreation, string observedImage, string expectedImage) {
        FileTime creation, exit, kernel, user;
        if (GetProcessId(parent) != parentPid || parentCreated == 0 || !GetProcessTimes(parent, out creation, out exit, out kernel, out user)
            || creation.Value != parentCreated || exit.Value < creation.Value) return false;
        long childCreated = Created(child);
        if (GetProcessId(child) != pid || childCreated == 0 || childCreated / 10 != observedCreation / 10
            || childCreated < creation.Value || childCreated > exit.Value || String.IsNullOrEmpty(observedImage) || !String.Equals(observedImage, expectedImage, StringComparison.OrdinalIgnoreCase)) return false;
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
function Read-OwnedText([string]$file) {
    if (-not (Test-Path -LiteralPath $file)) { return '' }
    $stream = [IO.File]::Open($file,[IO.FileMode]::Open,[IO.FileAccess]::Read,([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    try {
        $length = [int][Math]::Min([long]65536,$stream.Length)
        [void]$stream.Seek($stream.Length-$length,[IO.SeekOrigin]::Begin)
        $bytes = [byte[]]::new($length); $offset = 0
        while ($offset -lt $length) { $read=$stream.Read($bytes,$offset,$length-$offset); if($read -eq 0){break}; $offset += $read }
        return [Text.Encoding]::UTF8.GetString($bytes,0,$offset)
    } finally { $stream.Dispose() }
}
function Read-OwnedJson([string]$file) {
    $text = Read-OwnedText $file
    if ($text.Length -eq 0) { return $null }
    return ($text | ConvertFrom-Json)
}
function Stop-Owned([Diagnostics.Process]$child,[switch]$KeepPin) {
    if ($null -eq $child) { return $true }
    $settled=$false; $okay=$true
    try { if ($child.HasExited) { $settled=$true } else { $child.Kill();$settled=$child.WaitForExit(3000) } }
    catch { $okay=$false }
    finally { if ($settled -and -not $KeepPin) { try { $child.Dispose() } catch { $okay=$false } } }
    return $okay -and $settled
}
function Admit-Started([Diagnostics.Process]$child,[string]$expected) {
    $pin=$child.Handle
    if ([OwnedSourceRun]::Pid($pin) -ne $child.Id -or [OwnedSourceRun]::Created($pin) -eq 0 -or
        -not [string]::Equals([OwnedSourceRun]::Image($pin),$expected,[StringComparison]::OrdinalIgnoreCase)) {
        throw 'direct owned child native admission failed'
    }
    return [OwnedSourceRun]::Created($pin)
}
function Retain-Replacements {
    if ($null -eq $source -or $sourceCreated -eq 0) { return }
    $rows=@(Get-CimInstance Win32_Process -Filter ("ParentProcessId = " + $source.Id + " AND Name = 'crosspane-agent.exe'"))
    $allAdmitted=$true
    foreach ($row in $rows) {
        $candidate=$null
        try {
            $candidate=[Diagnostics.Process]::GetProcessById([int]$row.ProcessId)
            $pin=$candidate.Handle
            if (-not [OwnedSourceRun]::AdmitReplacement($pin,[uint32]$row.ProcessId,$source.Handle,[uint32]$source.Id,$sourceCreated,
                $row.CreationDate.ToFileTimeUtc(),$row.ExecutablePath,$sourceExe)) { throw 'replacement identity refused' }
            $same=$false
            foreach($known in $replacements) {
                if ($known.Id -ne $candidate.Id) {continue}
                try { $knownCreated=[OwnedSourceRun]::Created($known.Handle) } catch {continue}
                if ($knownCreated -ne 0 -and $knownCreated -eq [OwnedSourceRun]::Created($pin)) {$same=$true;break}
            }
            if (-not $same) {$replacements.Add($candidate);$candidate=$null}
        } catch {$allAdmitted=$false}
        finally {if($null -ne $candidate){try{$candidate.Dispose()}catch{$allAdmitted=$false}}}
    }
    if(-not $allAdmitted){throw 'replacement native admission refused; disposed only'}
}
$suffix=[Guid]::NewGuid().ToString('N')
$scratch=Join-Path $env:TEMP ('crosspane-WP-W2.5b-'+$suffix)
"exact_owned_scratch=$scratch"
$helper=$null;$source=$null;$sourceCreated=0;$helperCreated=0
$replacements=[Collections.Generic.List[Diagnostics.Process]]::new()
$saved=@{}
$variables=@('APPDATA','LOCALAPPDATA','CROSSPANE_RUNTIME_DIR','CROSSPANE_ACCEPTANCE_E1_ONLY',
    'CROSSPANE_ACCEPTANCE_E2_DESTINATION','CROSSPANE_ACCEPTANCE_E2_SOURCE','CROSSPANE_E2_SOURCE_CLAIM',
    'CROSSPANE_E2_DESTINATION_FIXTURE','CROSSPANE_E2_FIXTURE_ROOT','CROSSPANE_DISCOVERY','CROSSPANE_AUDIO','CROSSPANE_GPU','RUST_LOG')
try {
    foreach($name in $variables){$saved[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
    $env:APPDATA=Join-Path $scratch 'roaming';$env:LOCALAPPDATA=Join-Path $scratch 'local'
    $env:CROSSPANE_RUNTIME_DIR=Join-Path $scratch 'runtime'
    $env:CROSSPANE_ACCEPTANCE_E1_ONLY=$null;$env:CROSSPANE_ACCEPTANCE_E2_DESTINATION=$null
    $env:CROSSPANE_ACCEPTANCE_E2_SOURCE='1';$env:CROSSPANE_E2_SOURCE_CLAIM=$null
    $env:CROSSPANE_E2_DESTINATION_FIXTURE='1';$env:CROSSPANE_E2_FIXTURE_ROOT=$scratch
    $env:CROSSPANE_DISCOVERY='0';$env:CROSSPANE_AUDIO='0';$env:CROSSPANE_GPU='0';$env:RUST_LOG='info'
    New-Item -ItemType Directory -Path (Join-Path $scratch 'fixture'),(Join-Path $scratch 'agent') -Force | Out-Null
    $fixtureExe=Join-Path $scratch 'fixture/owned-window.exe'
    $sourceExe=Join-Path $scratch 'agent/crosspane-agent.exe'
    Copy-Item -LiteralPath $DestinationTest -Destination $fixtureExe
    Copy-Item -LiteralPath $Agent -Destination $sourceExe
    $helper=Start-Process -FilePath $fixtureExe -ArgumentList '--exact owned_e2_destination --nocapture' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'destination.out') -RedirectStandardError (Join-Path $scratch 'destination.err')
    $helperCreated=Admit-Started $helper $fixtureExe
    $deadline=[DateTime]::UtcNow.AddSeconds(15);$ready=$null;$readyText=''
    do {
        if($helper.HasExited){throw 'owned destination exited before setup'}
        $readyText=Read-OwnedText (Join-Path $scratch 'fixture/destination-ready.json')
        if($readyText.Length -gt 0){$ready=$readyText | ConvertFrom-Json}
        if($null -eq $ready){Start-Sleep -Milliseconds 100}
    } while($null -eq $ready -and [DateTime]::UtcNow -lt $deadline)
    if($null -eq $ready){throw 'owned destination readiness timeout'}
    # Compare the integer's original decimal text, never a JSON floating-point conversion.
    $pidMatch=$ready.claim.pid -eq $helper.Id
    $createdText=[regex]::Match($readyText,'"process_created"\s*:\s*([0-9]+)').Groups[1].Value
    $claimCreated=[UInt64]0
    $creationMatch=$createdText.Length -gt 0 -and [UInt64]::TryParse($createdText,[ref]$claimCreated) -and $claimCreated -eq [UInt64]$helperCreated
    $executableMatch=$false
    try {
        $claimed=[string]$ready.claim.executable
        if([IO.Path]::IsPathRooted($claimed)) {
            # Lexical only: never open/query a path supplied by a refused claim.
            $normalizedClaim=[IO.Path]::GetFullPath($claimed).Replace('/','\')
            $normalizedExpected=[IO.Path]::GetFullPath($fixtureExe).Replace('/','\')
            $executableMatch=[string]::Equals($normalizedClaim,$normalizedExpected,[StringComparison]::OrdinalIgnoreCase)
        }
    } catch {$executableMatch=$false}
    "owned_claim_pid_match=$pidMatch; exact_creation_match=$creationMatch; normalized_scratch_executable_match=$executableMatch"
    if(-not $pidMatch -or -not $creationMatch -or -not $executableMatch){throw 'retained fixture process claim mismatch'}
    $env:CROSSPANE_E2_SOURCE_CLAIM=$ready.claim | ConvertTo-Json -Compress
    $source=Start-Process -FilePath $sourceExe -ArgumentList 'run' -PassThru -WindowStyle Hidden -RedirectStandardOutput (Join-Path $scratch 'agent.out') -RedirectStandardError (Join-Path $scratch 'agent.err')
    $sourceCreated=Admit-Started $source $sourceExe
    $claim=@{pid=$source.Id;process_created=[UInt64]$sourceCreated;executable=$sourceExe} | ConvertTo-Json -Compress
    # Closed-before-visible handoff prevents the helper from consuming a partial claim.
    $claimTemp=Join-Path $scratch 'fixture/agent-claim.tmp'
    [IO.File]::WriteAllText($claimTemp,$claim,[Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $claimTemp -Destination (Join-Path $scratch 'fixture/agent-claim.json')
    $deadline=[DateTime]::UtcNow.AddSeconds(75);$result=$null;$resultPath=Join-Path $scratch 'fixture/destination-result.json'
    do {
        $result=Read-OwnedJson $resultPath
        if($null -ne $result){break}
        if($helper.HasExited){throw 'owned one-test destination exited without result'}
        Start-Sleep -Milliseconds 100
    } while([DateTime]::UtcNow -lt $deadline)
    if($null -eq $result){throw 'owned source acceptance timeout'}
    if(-not $result.protocol_clean -or -not $result.restored -or -not $result.journal_empty -or -not $result.source_returned){throw 'owned protocol/restore completion absent'}
    if(-not $source.WaitForExit(8000) -or $source.ExitCode -ne 0){throw 'retained source native clean exit absent'}
    $expected=[regex]::Match((Read-OwnedText $resultPath),'"instance"\s*:\s*([0-9]+)').Groups[1].Value
    if($expected.Length -eq 0){throw 'exact source instance absent from result'}
    [void][UInt64]::Parse($expected,[Globalization.CultureInfo]::InvariantCulture)
    $receiptPath=Join-Path $env:LOCALAPPDATA 'Crosspane/last_exit.json'
    $receiptText=Read-OwnedText $receiptPath;$receipt=$receiptText | ConvertFrom-Json
    $actual=[regex]::Match($receiptText,'"instance_id"\s*:\s*([0-9]+)').Groups[1].Value
    if(-not $receipt.clean -or $actual -cne $expected){throw 'exact same-instance clean receipt absent'}
    Retain-Replacements
    if($replacements.Count -ne 0){throw 'owned source unexpectedly launched replacement'}
    if(-not $helper.WaitForExit(5000) -or $helper.ExitCode -ne 0){throw 'owned destination helper did not settle cleanly'}
    $output=(Read-OwnedText (Join-Path $scratch 'destination.out'))+(Read-OwnedText (Join-Path $scratch 'destination.err'))
    if(-not $output.Contains('test result: ok. 1 passed; 0 failed;') -or $output.Contains('SKIP')){throw 'exact owned destination one-test proof absent'}
    $agentLog=(Read-OwnedText (Join-Path $scratch 'agent.out'))+(Read-OwnedText (Join-Path $scratch 'agent.err'))
    if($agentLog.Contains('hotkey chord not configured') -or $agentLog.Contains('global hotkeys unavailable')){throw 'required source hotkey setup evidence failed'}
    'owned_source_window_only=true; source_native_exit=0; exact_instance_receipt=true; no_replacement=true'
    "actual_cursor_events=$($result.cursors); cursor_default=$($result.cursor_default); cursor_hidden=$($result.cursor_hidden); pointer_movement=false"
    "tile_frames=$($result.tiles); h264_frames=$($result.video); actual_resize_and_mirror=true; restored_original=true; journal_empty=true"
    "installer_stop_ack_received=$($result.ack_received); protocol_clean=true; native_receipt_clean=true; empty_held_heartbeats=$($result.empty_held_heartbeats)"
    $output
} catch {
    $original=$_
    foreach($name in @('destination.out','destination.err','agent.out','agent.err')) {
        try {$log=Read-OwnedText (Join-Path $scratch $name);$log.Substring([Math]::Max(0,$log.Length-8192))}
        catch {'owned diagnostic unavailable'}
    }
    throw $original
} finally {
    $failures=[Collections.Generic.List[string]]::new()
    # Settle the exact parent before the final descendant pass; retain its creation/exit pin.
    try {if(-not (Stop-Owned $source -KeepPin)){$failures.Add('owned source settlement')}}catch{$failures.Add('owned source settlement')}
    try {Retain-Replacements}catch{$failures.Add('owned replacement admission')}
    foreach($child in $replacements){try{if(-not (Stop-Owned $child)){$failures.Add('owned replacement stop')}}catch{$failures.Add('owned replacement cleanup')}}
    try {if(-not (Stop-Owned $helper)){$failures.Add('owned fixture settlement')}}catch{$failures.Add('owned fixture cleanup')}
    if($null -ne $source){try{$source.Dispose()}catch{$failures.Add('owned source handle release')}}
    foreach($name in $variables){try{[Environment]::SetEnvironmentVariable($name,$saved[$name],'Process')}catch{$failures.Add('environment restore')}}
    try{if(Test-Path -LiteralPath $scratch){Remove-Item -LiteralPath $scratch -Recurse -Force}}catch{
        "exact_owned_scratch=$scratch; cleanup_stage=RemoveItem; exception_type=$($_.Exception.GetType().Name); hresult=$($_.Exception.HResult)"
        $failures.Add('owned scratch removal')
    }
    if($failures.Count -gt 0){throw ('owned source cleanup incomplete: '+($failures -join ', '))}
    'owned_processes_and_scratch_removed=true'
}
