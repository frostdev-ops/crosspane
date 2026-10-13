#Requires -Version 5.1
# Exercise the real stager with inert PE fixtures and a mocked cargo; no artifact is executed.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$stage = Join-Path $PSScriptRoot 'stage-windows.ps1'
$repo = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$testParent = Join-Path $repo 'target'
if (-not (Test-Path -LiteralPath $testParent -PathType Container)) {
    New-Item -ItemType Directory -Path $testParent | Out-Null
}
$scratch = Join-Path $testParent ('stage-windows-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null
$savedApproved = $env:CROSSPANE_WINDOWS_APPROVED_INVENTORY
$savedElevated = $env:CROSSPANE_WINDOWS_ELEVATED_INVENTORY

function Assert-Test([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "test-stage-windows: $Message" }
}

function Write-Pe([string]$Path, [int]$Machine, [string]$Tail = '') {
    $bytes = New-Object byte[] (512 + $Tail.Length)
    $bytes[0] = 0x4d; $bytes[1] = 0x5a; $bytes[60] = 64
    $bytes[64] = 0x50; $bytes[65] = 0x45
    $bytes[68] = $Machine -band 255; $bytes[69] = $Machine -shr 8
    $bytes[84] = 240; $bytes[88] = 0x0b; $bytes[89] = 2; $bytes[156] = 3
    [System.Text.Encoding]::ASCII.GetBytes($Tail).CopyTo($bytes, 512)
    [System.IO.File]::WriteAllBytes($Path, $bytes)
}

function cargo {
    $global:LASTEXITCODE = 0
    if ($args[0] -ceq 'metadata') {
        return (@{ packages = @(@{ name = 'crosspane-agent'; version = '0.0.0' }); target_directory = $fixtureTarget } | ConvertTo-Json -Compress)
    }
    Assert-Test ($args[0] -ceq 'build') 'unexpected cargo command'
    if ($args -contains 'crosspane-installer') {
        $tail = $env:CROSSPANE_WINDOWS_APPROVED_INVENTORY + $env:CROSSPANE_WINDOWS_ELEVATED_INVENTORY
        Write-Pe (Join-Path $fixtureTarget 'release\crosspane-installer.exe') $installerMachine $tail
    }
}

try {
    $cases = @('x64', 'agent', 'ui', 'ctl', 'helper', 'driver', 'installer', 'all-arm64')
    foreach ($case in $cases) {
        $caseRoot = Join-Path $scratch $case
        $fixtureTarget = Join-Path $caseRoot 'build'
        $release = Join-Path $fixtureTarget 'release'
        $driver = Join-Path $caseRoot 'driver'
        $out = Join-Path $caseRoot 'out'
        New-Item -ItemType Directory -Path $release -Force | Out-Null
        New-Item -ItemType Directory -Path $driver | Out-Null
        $installerMachine = 0x8664
        if ($case -in @('installer', 'all-arm64')) { $installerMachine = 0xaa64 }
        foreach ($role in @('agent', 'ui', 'ctl', 'helper', 'driver')) {
            $machine = 0x8664
            if ($case -eq $role -or $case -eq 'all-arm64') { $machine = 0xaa64 }
            $path = switch ($role) {
                'ctl' { Join-Path $release 'crosspanectl.exe' }
                'helper' { Join-Path $release 'crosspane-elevated-setup.exe' }
                'driver' { Join-Path $driver 'CrosspaneIdd.dll' }
                default { Join-Path $release ("crosspane-$role.exe") }
            }
            Write-Pe $path $machine
        }
        [System.IO.File]::WriteAllText((Join-Path $driver 'CrosspaneIdd.inf'), 'inert-inf')
        [System.IO.File]::WriteAllText((Join-Path $driver 'CrosspaneIdd.cat'), 'inert-catalog')
        $failure = $null
        try { & $stage -Out $out -Driver $driver | Out-Null } catch { $failure = $_ }
        if ($case -eq 'x64') {
            Assert-Test ($null -eq $failure) "x64 kit failed: $failure"
            $inventory = Get-Content -LiteralPath (Join-Path $out 'approved-inventory.json') -Raw | ConvertFrom-Json
            Assert-Test (@($inventory.payloads).Count -eq 3) 'x64 kit did not publish its inventory'
            foreach ($pin in $inventory.payloads) {
                Assert-Test ($pin.machine -eq 0x8664) 'published inventory contains a non-x64 payload'
                $hash = (Get-FileHash -LiteralPath (Join-Path $out ('payload\' + $pin.leaf))).Hash.ToLowerInvariant()
                Assert-Test ($hash -ceq $pin.sha256) 'published payload digest differs from its pin'
            }
            Assert-Test (Test-Path -LiteralPath (Join-Path $out 'crosspane-installer.exe') -PathType Leaf) 'x64 installer was not published'
        } else {
            Assert-Test ($null -ne $failure) "$case architecture was accepted"
            Assert-Test ($failure.Exception.Message -like '*must be an x64 image*') "$case failed for an unrelated reason: $failure"
            Assert-Test (-not (Test-Path -LiteralPath $out)) "$case published a rejected kit"
        }
        Assert-Test (@(Get-ChildItem -LiteralPath $caseRoot -Force | Where-Object { $_.Name -like '.stage-windows.*' }).Count -eq 0) "$case left an unfinished staging directory"
    }
    Write-Output "test-stage-windows: $($cases.Count) isolated packaging cases passed"
} finally {
    $env:CROSSPANE_WINDOWS_APPROVED_INVENTORY = $savedApproved
    $env:CROSSPANE_WINDOWS_ELEVATED_INVENTORY = $savedElevated
    $resolvedScratch = [System.IO.Path]::GetFullPath($scratch)
    $expectedParent = [System.IO.Path]::GetFullPath($testParent).TrimEnd('\') + '\'
    if (-not $resolvedScratch.StartsWith($expectedParent, [System.StringComparison]::OrdinalIgnoreCase) -or
        [System.IO.Path]::GetFileName($resolvedScratch) -cnotmatch '^stage-windows-test-[0-9a-f]{32}$') {
        throw 'test-stage-windows: scratch cleanup escaped the test directory'
    }
    Remove-Item -LiteralPath $resolvedScratch -Recurse -Force
}
