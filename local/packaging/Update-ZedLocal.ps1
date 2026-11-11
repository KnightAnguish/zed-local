param(
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\ZedLocal'),
    [string]$UserData = (Join-Path $env:LOCALAPPDATA 'ZedLocal\UserData'),
    [string]$ManifestUrl = 'https://github.com/KnightAnguish/zed-local/releases/latest/download/latest.json',
    [switch]$Launch,
    [switch]$CheckOnly,
    [switch]$Quiet
)
$ErrorActionPreference = 'Stop'
$logRoot = Join-Path $env:LOCALAPPDATA 'ZedLocal\updates'
New-Item -ItemType Directory -Path $logRoot -Force | Out-Null
$logPath = Join-Path $logRoot 'update.log'

function Write-UpdateLog([string]$Message) {
    $line = '{0} {1}' -f (Get-Date).ToUniversalTime().ToString('o'), $Message
    Add-Content -LiteralPath $logPath -Value $line -Encoding UTF8
    if (-not $Quiet) { Write-Host $Message }
}

function Get-InstalledState {
    $installFile = Join-Path $InstallRoot 'install.json'
    if (Test-Path -LiteralPath $installFile) {
        try {
            return Get-Content -Raw -Encoding UTF8 -LiteralPath $installFile | ConvertFrom-Json
        } catch {
            Write-UpdateLog "Cannot read install.json: $($_.Exception.Message)"
        }
    }
    return $null
}

function Get-CurrentExecutable {
    $state = Get-InstalledState
    if ($state -and $state.install_dir) {
        $candidate = Join-Path $state.install_dir 'zedlocal.exe'
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    $builds = Join-Path $InstallRoot 'builds'
    if (Test-Path -LiteralPath $builds) {
        $candidate = Get-ChildItem -LiteralPath $builds -Directory |
            Sort-Object LastWriteTime -Descending |
            ForEach-Object { Join-Path $_.FullName 'zedlocal.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
        if ($candidate) { return $candidate }
    }
    return $null
}

function Start-Zed([string]$Executable) {
    if (-not $Executable) { return }
    Start-Process -FilePath $Executable -ArgumentList @('--user-data-dir', ('"' + $UserData + '"')) -WorkingDirectory (Split-Path -Parent $Executable)
}

$temporary = $null
$launchAfter = [bool]$Launch -and -not [bool]$CheckOnly
$installed = Get-InstalledState
try {
    $manifestResponse = Invoke-WebRequest -Uri $ManifestUrl -UseBasicParsing -TimeoutSec 10
    $manifestJson = if ($manifestResponse.RawContentStream) {
        [Text.Encoding]::UTF8.GetString($manifestResponse.RawContentStream.ToArray())
    } else {
        $manifestResponse.Content
    }
    $manifest = ($manifestJson.TrimStart([char]0xFEFF)) | ConvertFrom-Json
    if (-not $manifest.release -or -not $manifest.asset_url -or -not $manifest.sha256) {
        throw 'Release manifest is missing release, asset_url or sha256.'
    }
    if ($manifest.asset_url -notmatch '^https://github\.com/KnightAnguish/zed-local/releases/download/') {
        throw "Unexpected update asset URL: $($manifest.asset_url)"
    }
    if ($manifest.sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'Release manifest sha256 is invalid.'
    }

    $isNewer = $false
    if (-not $installed -or -not $installed.release) {
        $isNewer = $true
    } elseif ($manifest.release -ne $installed.release) {
        if ($manifest.published_at -and $installed.published_at) {
            $isNewer = ([datetime]$manifest.published_at).ToUniversalTime() -gt ([datetime]$installed.published_at).ToUniversalTime()
        } else {
            $isNewer = $true
        }
    }

    if (-not $isNewer) {
        Write-UpdateLog "Already current: $($installed.release)"
        if ($CheckOnly) { Write-Output "current=$($installed.release)" }
        return
    }

    Write-UpdateLog "Update available: $($manifest.release)"
    if ($CheckOnly) {
        Write-Output "available=$($manifest.release)"
        return
    }

    $temporary = Join-Path ([IO.Path]::GetTempPath()) ('ZedLocalUpdate-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $temporary -Force | Out-Null
    $zipPath = Join-Path $temporary 'update.zip'
    Invoke-WebRequest -Uri $manifest.asset_url -OutFile $zipPath -UseBasicParsing -TimeoutSec 300
    $actualHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash
    if ($actualHash -ne $manifest.sha256.ToUpperInvariant()) {
        throw "Update SHA256 mismatch: expected $($manifest.sha256), got $actualHash"
    }

    Expand-Archive -LiteralPath $zipPath -DestinationPath $temporary -Force
    $installer = Join-Path $temporary 'Install.ps1'
    if (-not (Test-Path -LiteralPath $installer)) {
        throw 'Update package has no Install.ps1.'
    }
    Write-UpdateLog "Installing $($manifest.release)"
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer -InstallRoot $InstallRoot -UserData $UserData -NoShortcuts -Quiet
    if ($LASTEXITCODE -ne 0) {
        throw "Installer failed with exit code $LASTEXITCODE"
    }
    $installed = Get-InstalledState
    Write-UpdateLog "Installed $($installed.release)"
} catch {
    Write-UpdateLog "Update failed: $($_.Exception.Message)"
    if ($CheckOnly) { throw }
} finally {
    if ($temporary -and (Test-Path -LiteralPath $temporary)) {
        Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($launchAfter) {
        $executable = Get-CurrentExecutable
        if ($executable) { Start-Zed $executable }
    }
}
