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
    & (Join-Path $visualStudio 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    & cargo build --locked -p zed --profile $Profile
    if ($LASTEXITCODE -ne 0) { throw "Cargo build failed ($LASTEXITCODE)." }
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $previousTarget
    $env:CARGO_HOME = $previousCargoHome
    $env:CARGO_BUILD_JOBS = $previousJobs
}
