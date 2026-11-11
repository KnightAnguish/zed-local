param(
    [switch]$Quiet,
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\ZedLocal'),
    [string]$UserData = (Join-Path $env:LOCALAPPDATA 'ZedLocal\UserData'),
    [switch]$NoShortcuts
)
$ErrorActionPreference = 'Stop'

$source = $PSScriptRoot
$manifestPath = Join-Path $source 'manifest.json'
if (-not (Test-Path -LiteralPath $manifestPath)) { throw 'manifest.json is missing.' }
$manifest = Get-Content -Raw -Encoding UTF8 -LiteralPath $manifestPath | ConvertFrom-Json

if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'Zed Local requires 64-bit Windows.'
}

$installRoot = [IO.Path]::GetFullPath($InstallRoot)
$userData = [IO.Path]::GetFullPath($UserData)
$buildDir = Join-Path $installRoot ('builds\' + $manifest.build)
$exe = Join-Path $buildDir 'zedlocal.exe'

foreach ($file in $manifest.files) {
    $path = Join-Path $source $file.path
    if (-not (Test-Path -LiteralPath $path)) { throw "Missing package file: $($file.path)" }
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    if ($actual -ne $file.sha256) { throw "Package integrity check failed: $($file.path)" }
}

if (Test-Path -LiteralPath $buildDir) {
    try {
        Remove-Item -LiteralPath $buildDir -Recurse -Force -ErrorAction Stop
    } catch {
        throw "Close Zed Local before reinstalling. $($_.Exception.Message)"
    }
}
New-Item -ItemType Directory -Path $buildDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $buildDir 'x64') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $buildDir 'bin') -Force | Out-Null
New-Item -ItemType Directory -Path $userData -Force | Out-Null

foreach ($file in $manifest.files) {
    $destination = Join-Path $buildDir $file.path
    $parent = Split-Path -Parent $destination
    if ($parent) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
    Copy-Item -LiteralPath (Join-Path $source $file.path) -Destination $destination -Force
}
# Match both the local side-by-side layout and the official Windows bundle layout.
Copy-Item -LiteralPath (Join-Path $source 'OpenConsole.exe') -Destination (Join-Path $buildDir 'x64\OpenConsole.exe') -Force
Copy-Item -LiteralPath (Join-Path $source 'cli.exe') -Destination (Join-Path $buildDir 'bin\zed.exe') -Force
Copy-Item -LiteralPath (Join-Path $source 'README.txt') -Destination (Join-Path $buildDir 'README.txt') -Force
Copy-Item -LiteralPath (Join-Path $source 'LICENSE-GPL') -Destination (Join-Path $buildDir 'LICENSE-GPL') -Force
Copy-Item -LiteralPath (Join-Path $source 'LICENSE-APACHE') -Destination (Join-Path $buildDir 'LICENSE-APACHE') -Force
Copy-Item -LiteralPath (Join-Path $source 'Uninstall.ps1') -Destination (Join-Path $installRoot 'Uninstall.ps1') -Force
Copy-Item -LiteralPath (Join-Path $source 'Uninstall.cmd') -Destination (Join-Path $installRoot 'Uninstall.cmd') -Force
foreach ($file in @('Update-ZedLocal.ps1', 'ZedLocalLauncher.vbs', 'Check-Update.cmd')) {
    Copy-Item -LiteralPath (Join-Path $source $file) -Destination (Join-Path $installRoot $file) -Force
}

$installInfo = [pscustomobject]@{
    product = 'Zed Local'
    version = $manifest.version
    build = $manifest.build
    release = $manifest.release
    published_at = $manifest.published_at
    source_head = $manifest.source_head
    installed_at = (Get-Date).ToString('o')
    install_dir = $buildDir
    user_data = $userData
} | ConvertTo-Json -Depth 4
$installInfo | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $installRoot 'install.json')

$launcher = Join-Path $installRoot 'ZedLocalLauncher.vbs'

function Set-ZedShortcut([string]$Path) {
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($Path)
    if (Test-Path -LiteralPath $launcher) {
        $shortcut.TargetPath = Join-Path $env:WINDIR 'System32\wscript.exe'
        $shortcut.Arguments = '"' + $launcher + '"'
    } else {
        $shortcut.TargetPath = $exe
        $shortcut.Arguments = '--user-data-dir "' + $userData + '"'
    }
    $shortcut.WorkingDirectory = $buildDir
    $shortcut.IconLocation = $exe + ',0'
    $shortcut.Description = 'Zed Local ' + $manifest.version
    $shortcut.Save()
}

if (-not $NoShortcuts) {
    $desktop = [Environment]::GetFolderPath('Desktop')
    if ($desktop) { Set-ZedShortcut (Join-Path $desktop 'Zed Local.lnk') }
    $programs = [Environment]::GetFolderPath('Programs')
    if ($programs) { Set-ZedShortcut (Join-Path $programs 'Zed Local.lnk') }
}

$vcRuntime = Join-Path $env:WINDIR 'System32\vcruntime140.dll'
if (-not (Test-Path -LiteralPath $vcRuntime)) {
    Write-Warning 'Microsoft Visual C++ Redistributable 2015-2022 (x64) was not found. Install it if Zed Local does not start.'
}
$webView2 = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
if (-not (Test-Path -LiteralPath $webView2)) {
    Write-Warning 'Microsoft Edge WebView2 Runtime was not found. HTML preview may not work until it is installed.'
}

$helpOut = Join-Path $env:TEMP ('zedlocal-help-' + [guid]::NewGuid().ToString('N') + '.txt')
$helpErr = Join-Path $env:TEMP ('zedlocal-help-' + [guid]::NewGuid().ToString('N') + '.err')
try {
    $process = Start-Process -FilePath $exe -ArgumentList '--help' -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput $helpOut -RedirectStandardError $helpErr
    $exitCode = $process.ExitCode
} catch {
    throw "The installed executable could not start. Install Microsoft Visual C++ Redistributable 2015-2022 (x64) and try again. $($_.Exception.Message)"
} finally {
    Remove-Item -LiteralPath $helpOut, $helpErr -Force -ErrorAction SilentlyContinue
}
if ($exitCode -ne 0) {
    throw "The installed executable failed its startup check (exit $exitCode). Install Microsoft Visual C++ Redistributable 2015-2022 (x64) and try again."
}

if (-not $Quiet) {
    Write-Host ''
    Write-Host 'Zed Local installed successfully.'
    Write-Host "Application: $buildDir"
    Write-Host "User data:   $userData"
    if (-not $NoShortcuts) { Write-Host 'Shortcuts:   Desktop and Start Menu -> Zed Local' }
    Write-Host 'Remove:      run Uninstall.cmd from the installed ZedLocal folder.'
}
