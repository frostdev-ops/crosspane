#Requires -Version 5.1
# WP-W4.1c2 T5 (plan S1): stage a Windows installer kit for the lead's VM dry run.
# Builds the release payload, writes approved-inventory.json and elevated-inventory.json from the
# built and signed files, builds crosspane-installer with both inventories embedded, and publishes
# the result atomically to -Out. Build-time only: no signing, no install, no certificate, no driver
# load. Keep this file ASCII: Windows PowerShell 5.1 reads a script without a BOM as ANSI.
#
# Usage: stage-windows.ps1 -Out C:\fresh\dir -Driver C:\dir\with\signed\driver [-AgentFeatures video]
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Out,
    [Parameter(Mandatory = $true)][string]$Driver,
    [string]$AgentFeatures = ''
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Limits copied from the installer: payload/inventory.rs MAX_IMAGE_BYTES and MAX_STAGING_BYTES
# (512 MiB), and core elevated/kit.rs MAX_KIT_FILE_BYTES (64 MiB).
$MaxImageBytes = 536870912
$MaxStagingBytes = 536870912
$MaxKitFileBytes = 67108864
$InventoryVariables = @('CROSSPANE_WINDOWS_APPROVED_INVENTORY', 'CROSSPANE_WINDOWS_ELEVATED_INVENTORY')

function Stop-Stage([string]$Message) {
    throw "stage-windows: $Message"
}

function Clear-InventoryVariables {
    foreach ($name in $InventoryVariables) {
        Remove-Item -LiteralPath ('Env:\' + $name) -ErrorAction SilentlyContinue
    }
}

# Windows PowerShell 5.1 may surface native stderr as error records, which 'Stop' turns into a
# terminating error before the exit code is read. Only this call runs under 'Continue'; the saved
# preference is restored in finally, and the $LASTEXITCODE check below still decides success.
function Invoke-Cargo([string[]]$Arguments) {
    $savedErrorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $global:LASTEXITCODE = $null
        # Stream the build log to the host; stderr lines are passed on as plain text.
        & cargo @Arguments 2>&1 | ForEach-Object { "$_" }
    } finally { $ErrorActionPreference = $savedErrorPreference }
    if ($LASTEXITCODE -ne 0) {
        Stop-Stage "cargo $($Arguments -join ' ') exited with code $LASTEXITCODE"
    }
}

function Get-U16([byte[]]$Bytes, [int]$Offset) {
    if ($Offset -lt 0 -or $Offset + 2 -gt $Bytes.Length) {
        Stop-Stage 'a PE header read is outside the image'
    }
    return [int]$Bytes[$Offset] + ([int]$Bytes[$Offset + 1] -shl 8)
}

# Mirrors payload/inventory.rs (PeFacts::valid) and pe_header: MZ, PE32+, x64 or ARM64, and subsystem
# GUI (2) or console (3). Machine and subsystem are read little-endian, as the installer reads them.
function Get-PeFacts([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $size = [long]$bytes.Length
    if ($size -lt 128 -or $size -gt $MaxImageBytes) {
        Stop-Stage "$Path is outside the image size bounds"
    }
    if ([int]$bytes[0] -ne 0x4D -or [int]$bytes[1] -ne 0x5A) {
        Stop-Stage "$Path is not an MZ image"
    }
    $pe = [long]$bytes[60] + ([long]$bytes[61] -shl 8) + ([long]$bytes[62] -shl 16) + ([long]$bytes[63] -shl 24)
    if ($pe -lt 64 -or $pe -gt 1048448 -or $pe + 94 -gt $size) {
        Stop-Stage "$Path has an invalid PE header offset"
    }
    $p = [int]$pe
    if ([int]$bytes[$p] -ne 0x50 -or [int]$bytes[$p + 1] -ne 0x45 -or [int]$bytes[$p + 2] -ne 0 -or [int]$bytes[$p + 3] -ne 0) {
        Stop-Stage "$Path has no PE signature"
    }
    $machine = Get-U16 $bytes ($p + 4)
    $optionalSize = Get-U16 $bytes ($p + 20)
    $magic = Get-U16 $bytes ($p + 24)
    $subsystem = Get-U16 $bytes ($p + 92)
    if ($optionalSize -lt 70 -or $pe + 24 + $optionalSize -gt $size -or $magic -ne 0x20b) {
        Stop-Stage "$Path is not a PE32+ image"
    }
    if (@(0x8664, 0xaa64) -notcontains $machine) {
        Stop-Stage "$Path is not an x64 or ARM64 image"
    }
    if (@(2, 3) -notcontains $subsystem) {
        Stop-Stage "$Path is not a GUI or console image"
    }
    return [pscustomobject]@{
        Size      = $size
        Sha256    = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
        Machine   = $machine
        Subsystem = $subsystem
    }
}

# Size and SHA-256 only, for the kit files (kit.rs pins no PE facts).
function Get-DigestFacts([string]$Path) {
    $size = [long]((Get-Item -LiteralPath $Path).Length)
    if ($size -lt 1 -or $size -gt $MaxKitFileBytes) {
        Stop-Stage "$Path is outside the kit file size bounds"
    }
    return [pscustomobject]@{
        Size   = $size
        Sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

# Exact case-insensitive name match, so the catalog is found however its case was written.
function Get-DriverFile([string]$Directory, [string]$Name) {
    $found = @(Get-ChildItem -LiteralPath $Directory -File | Where-Object { $_.Name -ieq $Name })
    if ($found.Count -ne 1) {
        Stop-Stage "the driver directory must hold exactly one $Name (found $($found.Count))"
    }
    return $found[0].FullName
}

# One approved payload entry, in the field order of payload/inventory.rs ManifestPe.
function Format-Payload([string]$Role, [string]$Leaf, $Facts, [string]$Version) {
    return ('{{"role":"{0}","leaf":"{1}","size":{2},"sha256":"{3}","machine":{4},"subsystem":{5},"version":"{6}"}}' -f $Role, $Leaf, $Facts.Size, $Facts.Sha256, $Facts.Machine, $Facts.Subsystem, $Version)
}

# One kit entry, in the field order of core elevated/kit.rs KitDocumentFile.
function Format-KitFile([string]$Name, $Facts) {
    return ('{{"file":"{0}","size":{1},"sha256":"{2}"}}' -f $Name, $Facts.Size, $Facts.Sha256)
}

# UTF-8 without a byte order mark: serde_json rejects a BOM, and Windows PowerShell 5.1 adds one to
# Set-Content and Out-File by default.
function Write-Text([string]$Path, [string]$Text) {
    $encoding = New-Object -TypeName System.Text.UTF8Encoding -ArgumentList $false
    [System.IO.File]::WriteAllText($Path, $Text, $encoding)
}

if ($env:OS -ne 'Windows_NT') { Stop-Stage 'this script stages the Windows kit and runs only on Windows' }
if (-not $PSScriptRoot) { Stop-Stage 'run this file as a script' }
if ($AgentFeatures -notmatch '^[A-Za-z0-9_,-]*$') { Stop-Stage '-AgentFeatures takes a comma-separated list of feature names' }

if ($Out -notmatch '^[A-Za-z]:\\') { Stop-Stage '-Out must be an absolute drive path to a fresh directory' }
$outDir = [System.IO.Path]::GetFullPath($Out)
$outParent = Split-Path -Parent $outDir
if (-not $outParent -or -not (Test-Path -LiteralPath $outParent -PathType Container)) {
    Stop-Stage '-Out must sit inside an existing directory'
}
if (Test-Path -LiteralPath $outDir) {
    $existing = Get-Item -LiteralPath $outDir -Force
    if (-not $existing.PSIsContainer -or ([int]$existing.Attributes -band [int][System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        Stop-Stage '-Out exists and is not a plain directory'
    }
    if (@(Get-ChildItem -LiteralPath $outDir -Force).Count -ne 0) {
        Stop-Stage '-Out exists and is not empty; choose a fresh directory'
    }
}
if (-not (Test-Path -LiteralPath $Driver -PathType Container)) { Stop-Stage '-Driver must be an existing directory' }
$driverDir = (Resolve-Path -LiteralPath $Driver).ProviderPath
$root = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
if (-not (Test-Path -LiteralPath (Join-Path $root 'Cargo.toml') -PathType Leaf)) {
    Stop-Stage 'the repository root was not found next to this script'
}

# Everything is staged in a private sibling of -Out and renamed into place at the end, so -Out never
# appears half-built. The GUID name keeps the cleanup away from anything else in the parent.
$work = Join-Path $outParent ('.stage-windows.' + [guid]::NewGuid().ToString('N'))
Clear-InventoryVariables
Push-Location -LiteralPath $root
try {
    Write-Output 'stage-windows: reading the agent version from cargo metadata'
    # Same native-stderr hazard as Invoke-Cargo: 'Continue' only for this call, restored in finally.
    $savedErrorPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $global:LASTEXITCODE = $null
        $metaOutput = @(& cargo metadata --locked --no-deps --format-version 1 2>&1)
    } finally { $ErrorActionPreference = $savedErrorPreference }
    if ($LASTEXITCODE -ne 0) {
        $metaOutput | ForEach-Object { "$_" }
        Stop-Stage "cargo metadata exited with code $LASTEXITCODE"
    }
    # Only stdout text may reach the JSON. The stderr records are echoed so warnings still reach the log.
    $metaText = @($metaOutput | Where-Object { $_ -is [string] })
    $metaOutput | Where-Object { $_ -isnot [string] } | ForEach-Object { "$_" }
    $meta = ($metaText -join "`n") | ConvertFrom-Json
    $agentPackages = @($meta.packages | Where-Object { $_.name -ceq 'crosspane-agent' })
    if ($agentPackages.Count -ne 1) { Stop-Stage 'cargo metadata does not list exactly one crosspane-agent package' }
    $version = [string]$agentPackages[0].version
    # payload/inventory.rs PeFacts::valid: 1 to 128 ASCII alphanumerics, '.', '-' or '+'.
    if ($version -cnotmatch '^[A-Za-z0-9.+-]{1,128}$') { Stop-Stage "the agent version '$version' is not a valid product version" }
    $release = Join-Path ([string]$meta.target_directory) 'release'

    Write-Output 'stage-windows: building the release payload (no inventory in the bootstrap)'
    $featureArgs = @()
    if ($AgentFeatures -ne '') { $featureArgs = @('--features', $AgentFeatures) }
    Invoke-Cargo (@('build', '--release', '--locked', '-p', 'crosspane-agent') + $featureArgs)
    Invoke-Cargo @('build', '--release', '--locked', '-p', 'crosspane-ui', '-p', 'crosspanectl', '-p', 'crosspane-elevated-setup')

    $agentExe = Join-Path $release 'crosspane-agent.exe'
    $uiExe = Join-Path $release 'crosspane-ui.exe'
    $ctlExe = Join-Path $release 'crosspanectl.exe'
    $helperExe = Join-Path $release 'crosspane-elevated-setup.exe'
    $infSource = Get-DriverFile $driverDir 'CrosspaneIdd.inf'
    $catSource = Get-DriverFile $driverDir 'CrosspaneIdd.cat'
    $dllSource = Get-DriverFile $driverDir 'CrosspaneIdd.dll'

    $agentFacts = Get-PeFacts $agentExe
    $uiFacts = Get-PeFacts $uiExe
    $ctlFacts = Get-PeFacts $ctlExe
    $helperFacts = Get-PeFacts $helperExe
    $infFacts = Get-DigestFacts $infSource
    $catFacts = Get-DigestFacts $catSource
    $dllFacts = Get-DigestFacts $dllSource
    if ($agentFacts.Size + $uiFacts.Size + $ctlFacts.Size -gt $MaxStagingBytes) {
        Stop-Stage 'the approved payloads exceed the installer staging budget'
    }

    # Same roles, leaves and order as payload/inventory.rs. The installer itself is never pinned.
    $approvedSet = @(
        ([pscustomobject]@{ Role = 'agent'; Leaf = 'crosspane-agent.exe'; Source = $agentExe; Facts = $agentFacts }),
        ([pscustomobject]@{ Role = 'ui'; Leaf = 'crosspane-ui.exe'; Source = $uiExe; Facts = $uiFacts }),
        ([pscustomobject]@{ Role = 'ctl'; Leaf = 'crosspanectl.exe'; Source = $ctlExe; Facts = $ctlFacts })
    )
    # Kit files in KitFile::ALL order. Relative is the path under payload\.
    $kitSet = @(
        ([pscustomobject]@{ Name = 'helper'; Relative = 'crosspane-elevated-setup.exe'; Source = $helperExe; Facts = $helperFacts }),
        ([pscustomobject]@{ Name = 'driver-inf'; Relative = 'driver\CrosspaneIdd.inf'; Source = $infSource; Facts = $infFacts }),
        ([pscustomobject]@{ Name = 'driver-catalog'; Relative = 'driver\CrosspaneIdd.cat'; Source = $catSource; Facts = $catFacts }),
        ([pscustomobject]@{ Name = 'driver-binary'; Relative = 'driver\CrosspaneIdd.dll'; Source = $dllSource; Facts = $dllFacts })
    )

    Write-Output 'stage-windows: copying the payload'
    New-Item -ItemType Directory -Path $work | Out-Null
    $payload = Join-Path $work 'payload'
    $payloadDriver = Join-Path $payload 'driver'
    New-Item -ItemType Directory -Path $payload | Out-Null
    New-Item -ItemType Directory -Path $payloadDriver | Out-Null
    foreach ($pin in $approvedSet) {
        Copy-Item -LiteralPath $pin.Source -Destination (Join-Path $payload $pin.Leaf)
    }
    foreach ($file in $kitSet) {
        Copy-Item -LiteralPath $file.Source -Destination (Join-Path $payload $file.Relative)
    }

    $approvedEntries = @()
    foreach ($pin in $approvedSet) {
        $approvedEntries += Format-Payload $pin.Role $pin.Leaf $pin.Facts $version
    }
    $approvedJson = '{{"schema_version":1,"payloads":[{0}]}}' -f ($approvedEntries -join ',')
    $kitEntries = @()
    foreach ($file in $kitSet) {
        $kitEntries += Format-KitFile $file.Name $file.Facts
    }
    $kitJson = '{{"schema_version":1,"files":[{0}]}}' -f ($kitEntries -join ',')
    $approvedPath = Join-Path $work 'approved-inventory.json'
    $kitPath = Join-Path $work 'elevated-inventory.json'
    Write-Text $approvedPath $approvedJson
    Write-Text $kitPath $kitJson

    # option_env! reads the variable as JSON text, not as a path (inventory.rs:141).
    Write-Output 'stage-windows: building crosspane-installer with both inventories embedded'
    $env:CROSSPANE_WINDOWS_APPROVED_INVENTORY = $approvedJson
    $env:CROSSPANE_WINDOWS_ELEVATED_INVENTORY = $kitJson
    try {
        Invoke-Cargo @('build', '--release', '--locked', '-p', 'crosspane-installer', '--bin', 'crosspane-installer')
    } finally {
        Clear-InventoryVariables
    }
    Copy-Item -LiteralPath (Join-Path $release 'crosspane-installer.exe') -Destination (Join-Path $work 'crosspane-installer.exe')

    # The embedded strings must be exactly the JSON that was written. A build that ignored either
    # variable (or one that cargo left stale) fails here, not on the VM.
    $installerText = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes((Join-Path $work 'crosspane-installer.exe')))
    if ($installerText.IndexOf($approvedJson, [System.StringComparison]::Ordinal) -lt 0) {
        Stop-Stage 'crosspane-installer.exe does not embed approved-inventory.json verbatim'
    }
    if ($installerText.IndexOf($kitJson, [System.StringComparison]::Ordinal) -lt 0) {
        Stop-Stage 'crosspane-installer.exe does not embed elevated-inventory.json verbatim'
    }

    # Re-hash the staged copies against both JSON files, parsed back from disk.
    Write-Output 'stage-windows: re-hashing the staged copies against both inventories'
    $approvedCheck = [System.IO.File]::ReadAllText($approvedPath) | ConvertFrom-Json
    if ([int]$approvedCheck.schema_version -ne 1) { Stop-Stage 'approved-inventory.json schema is not 1' }
    $approvedPins = @($approvedCheck.payloads)
    if ($approvedPins.Count -ne 3) { Stop-Stage 'approved-inventory.json must pin exactly three payloads' }
    foreach ($item in $approvedSet) {
        $found = @($approvedPins | Where-Object { $_.role -ceq $item.Role })
        if ($found.Count -ne 1) { Stop-Stage "approved-inventory.json has no single $($item.Role) entry" }
        $entry = $found[0]
        $copy = Get-PeFacts (Join-Path $payload $item.Leaf)
        if ($entry.leaf -cne $item.Leaf -or [long]$entry.size -ne $copy.Size -or $entry.sha256 -cne $copy.Sha256 -or [int]$entry.machine -ne $copy.Machine -or [int]$entry.subsystem -ne $copy.Subsystem -or $entry.version -cne $version) {
            Stop-Stage "$($item.Leaf) does not match approved-inventory.json"
        }
    }
    $kitCheck = [System.IO.File]::ReadAllText($kitPath) | ConvertFrom-Json
    if ([int]$kitCheck.schema_version -ne 1) { Stop-Stage 'elevated-inventory.json schema is not 1' }
    $kitPins = @($kitCheck.files)
    if ($kitPins.Count -ne 4) { Stop-Stage 'elevated-inventory.json must list exactly four files' }
    foreach ($item in $kitSet) {
        $found = @($kitPins | Where-Object { $_.file -ceq $item.Name })
        if ($found.Count -ne 1) { Stop-Stage "elevated-inventory.json has no single $($item.Name) entry" }
        $entry = $found[0]
        $copy = Get-DigestFacts (Join-Path $payload $item.Relative)
        if ([long]$entry.size -ne $copy.Size -or $entry.sha256 -cne $copy.Sha256) {
            Stop-Stage "$($item.Relative) does not match elevated-inventory.json"
        }
    }

    # Publish. An empty -Out is replaced; Directory.Delete without recursion refuses anything else.
    if (Test-Path -LiteralPath $outDir) {
        [System.IO.Directory]::Delete($outDir, $false)
    }
    Move-Item -LiteralPath $work -Destination $outDir -ErrorAction Stop
    Write-Output "stage-windows: staged $outDir (crosspane-installer.exe, approved-inventory.json, elevated-inventory.json, payload)"
} finally {
    Clear-InventoryVariables
    Pop-Location
    if (Test-Path -LiteralPath $work) {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}
