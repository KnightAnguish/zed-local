[CmdletBinding()]
param(
    [ValidateSet('dev', 'release')][string]$Profile = 'dev',
    [string]$BuildRoot = 'D:\Build\zed-local',
    [ValidateRange(1, 32)][int]$Jobs = 12
)
$ErrorActionPreference = 'Stop'
$repository = Split-Path $PSScriptRoot -Parent
$buildDirectory = [IO.Path]::GetFullPath($BuildRoot)
if ($buildDirectory -eq [IO.Path]::GetPathRoot($buildDirectory)) {
    throw 'BuildRoot must be a dedicated directory, not a drive root.'
}
New-Item -ItemType Directory -Path $buildDirectory -Force | Out-Null
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
$visualStudio = & $vswhere -latest -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $visualStudio) { throw 'Visual Studio C++ build tools are required.' }
$previousTarget = $env:CARGO_TARGET_DIR
$previousCargoHome = $env:CARGO_HOME
$previousJobs = $env:CARGO_BUILD_JOBS
Push-Location $repository
try {
    $env:CARGO_TARGET_DIR = Join-Path $buildDirectory 'target'
    $env:CARGO_HOME = Join-Path $buildDirectory 'cargo-home'
    $env:CARGO_BUILD_JOBS = "$Jobs"
    $toolchain = Get-ChildItem (Join-Path $visualStudio 'VC\Tools\MSVC') -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'lib\spectre\x64\libcmt.lib') } |
        Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
    if (-not $toolchain) { throw 'Visual Studio x64 Spectre libraries are required.' }
    # Explicit selection avoids a machine-specific default selecting an older toolchain without Spectre libraries.
    Import-Module (Join-Path $visualStudio 'Common7\Tools\Microsoft.VisualStudio.DevShell.dll')
    Enter-VsDevShell -VsInstallPath $visualStudio -SkipAutomaticLocation -DevCmdArguments "-arch=amd64 -host_arch=amd64 -vcvars_ver=$($toolchain.Name)"
    & cargo build --locked -p zed --profile $Profile
    if ($LASTEXITCODE -ne 0) { throw "Cargo build failed ($LASTEXITCODE)." }
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:CARGO_HOME = $previousCargoHome
    $env:CARGO_BUILD_JOBS = $previousJobs
}
