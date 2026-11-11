param(
    [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA 'Programs\ZedLocal'),
    [string]$UserData = (Join-Path $env:LOCALAPPDATA 'ZedLocal\UserData'),
    [switch]$NoShortcuts
)
$ErrorActionPreference = 'Stop'

$installRoot = [IO.Path]::GetFullPath($InstallRoot)
$userData = [IO.Path]::GetFullPath($UserData)
if (-not $NoShortcuts) {
    $shortcutPaths = @()
    $desktop = [Environment]::GetFolderPath('Desktop')
    if ($desktop) { $shortcutPaths += Join-Path $desktop 'Zed Local.lnk' }
    $programs = [Environment]::GetFolderPath('Programs')
    if ($programs) { $shortcutPaths += Join-Path $programs 'Zed Local.lnk' }

    $shell = New-Object -ComObject WScript.Shell
    foreach ($path in $shortcutPaths) {
        if (-not (Test-Path -LiteralPath $path)) { continue }
        try {
            $shortcut = $shell.CreateShortcut($path)
            $ownedTarget = $shortcut.TargetPath -and $shortcut.TargetPath.StartsWith($installRoot, [StringComparison]::OrdinalIgnoreCase)
            $ownedLauncher = $shortcut.Arguments -and $shortcut.Arguments.IndexOf($installRoot, [StringComparison]::OrdinalIgnoreCase) -ge 0
            if ($ownedTarget -or $ownedLauncher) {
                Remove-Item -LiteralPath $path -Force
            }
        } catch {
            Write-Warning "Could not remove shortcut ${path}: $($_.Exception.Message)"
        }
    }
}

if (Test-Path -LiteralPath $installRoot) {
    $builds = Join-Path $installRoot 'builds'
    if (Test-Path -LiteralPath $builds) {
        $removed = $false
        for ($attempt = 0; $attempt -lt 3 -and -not $removed; $attempt++) {
            try {
                Remove-Item -LiteralPath $builds -Recurse -Force -ErrorAction Stop
                $removed = $true
            } catch {
                Start-Sleep -Seconds 2
            }
        }
        if (-not $removed) {
            throw 'Close Zed Local and run Uninstall again. The application directory is still in use.'
        }
    }
    try {
        foreach ($name in @('install.json', 'Uninstall.ps1', 'Uninstall.cmd', 'Update-ZedLocal.ps1', 'ZedLocalLauncher.vbs', 'Check-Update.cmd')) {
            Remove-Item -LiteralPath (Join-Path $installRoot $name) -Force -ErrorAction SilentlyContinue
        }
        if (-not (Get-ChildItem -LiteralPath $installRoot -Force)) {
            Remove-Item -LiteralPath $installRoot -Force
        }
    } catch {
        throw "Could not remove the installation directory. $($_.Exception.Message)"
    }
}

Write-Host 'Zed Local application files removed.'
Write-Host "User data was kept at: $userData"
Write-Host 'Delete that folder manually if you also want to remove settings, history and agents.'
