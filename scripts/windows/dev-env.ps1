# Dot-source in a PowerShell session before building Crosspane natively on Windows:
#   . .\scripts\windows\dev-env.ps1
# These settings apply only to the current session; nothing is persisted.
# CMake (needed by audiopus_sys) comes from the VS Build Tools install.
$cm = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
if (Test-Path $cm) { $env:Path = "$cm;" + $env:Path } else { Write-Warning "CMake not found at $cm" }
# aws-lc-sys (installer TLS stack) uses its shipped prebuilt NASM objects instead of a NASM install.
$env:AWS_LC_SYS_PREBUILT_NASM = '1'
