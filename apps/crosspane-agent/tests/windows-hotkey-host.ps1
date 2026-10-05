param([Parameter(Mandatory=$true)][ValidateSet('before','after')][string]$Mode)
$ErrorActionPreference = 'Stop'
$driver = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../../target/debug/deps/windows-hotkey-host-driver.exe'))
$root = Join-Path ([IO.Path]::GetTempPath()) ('crosspane-hotkey-host-' + [Guid]::NewGuid().ToString('N'))
$oldFlag = $env:CROSSPANE_HOTKEY_HOST_FIXTURE
$child = $null
$failure = $null
$cleanup = $true
try {
    [IO.Directory]::CreateDirectory($root) | Out-Null
    $env:CROSSPANE_HOTKEY_HOST_FIXTURE = '1'
    $child = Start-Process -FilePath $driver -ArgumentList $Mode -PassThru -RedirectStandardOutput (Join-Path $root 'own.stdout') -RedirectStandardError (Join-Path $root 'own.stderr')
    $ownedHandle = $child.Handle # Retain native process association before bounded wait.
    if (-not $child.WaitForExit(60000)) { throw 'owned registration driver exceeded 60 seconds' }
    # Child has exited and all redirected handles are closed before reading these exact own logs.
    [IO.File]::ReadAllText((Join-Path $root 'own.stdout'))
    [IO.File]::ReadAllText((Join-Path $root 'own.stderr'))
    $child.Refresh()
    if ($child.ExitCode -ne 0) { throw 'owned registration driver failed' }
} catch { $failure = $_ }
finally {
    if ($null -ne $child) {
        try {
            if (-not $child.HasExited) { $child.Kill(); if (-not $child.WaitForExit(3000)) { $cleanup = $false } }
        } catch { $cleanup = $false }
        try { $child.Dispose() } catch { $cleanup = $false }
    }
    try { $env:CROSSPANE_HOTKEY_HOST_FIXTURE = $oldFlag } catch { $cleanup = $false }
    try { if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force } } catch { $cleanup = $false }
    "owned registration fixture cleanup=$cleanup"
}
if ($null -ne $failure) { throw $failure }
if (-not $cleanup) { throw 'owned registration fixture cleanup incomplete' }
