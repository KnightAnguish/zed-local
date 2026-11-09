[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$UserDataDirectory,
    [ValidateSet('debug', 'release')][string]$Profile = 'debug',
    [string]$BuildRoot = (Join-Path $env:LOCALAPPDATA 'ZedLocalBuild')
)
$ErrorActionPreference = 'Stop'
$profileDirectory = [IO.Path]::GetFullPath($UserDataDirectory).TrimEnd('\', '/')
$forbiddenDirectories = @(
    [IO.Path]::GetPathRoot($profileDirectory).TrimEnd('\', '/'),
    [Environment]::GetFolderPath('UserProfile'),
    (Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Zed'),
    (Join-Path ([Environment]::GetFolderPath('ApplicationData')) 'Zed')
)
foreach ($forbidden in $forbiddenDirectories) {
    if ($profileDirectory -ieq $forbidden -or
        (($forbidden -match '\\Zed$') -and $profileDirectory.StartsWith($forbidden + '\', [StringComparison]::OrdinalIgnoreCase))) {
        throw 'Use a separate profile directory, not the official Zed profile or a broad directory.'
    }
}
$executable = Join-Path $BuildRoot "target\$Profile\zedlocal.exe"
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw 'Build Zed Local first.' }
$cliExecutable = Join-Path $BuildRoot "target\$Profile\cli.exe"
if (-not (Test-Path -LiteralPath $cliExecutable -PathType Leaf)) { throw 'Build the companion CLI with script/local-build.ps1 first.' }
New-Item -ItemType Directory -Path $profileDirectory -Force | Out-Null
# Also protect a stock baseline binary if the fork has not been built yet.
$previousExplanation = $env:ZED_UPDATE_EXPLANATION
try {
    $env:ZED_UPDATE_EXPLANATION = 'Local test build: installer updates are disabled. Merge and rebuild upstream releases instead.'
    & $executable --user-data-dir $profileDirectory
} finally {
    $env:ZED_UPDATE_EXPLANATION = $previousExplanation
}
