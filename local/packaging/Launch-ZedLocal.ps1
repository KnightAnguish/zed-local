$ErrorActionPreference = 'Stop'
$installRoot = $PSScriptRoot
$installFile = Join-Path $installRoot 'install.json'
$executable = $null
$userData = $null

if (Test-Path -LiteralPath $installFile) {
    try {
        $state = Get-Content -Raw -Encoding UTF8 -LiteralPath $installFile | ConvertFrom-Json
        if ($state.install_dir) {
            $candidate = Join-Path $state.install_dir 'zedlocal.exe'
            if (Test-Path -LiteralPath $candidate) {
                $executable = $candidate
                $userData = $state.user_data
            }
        }
    } catch {}
}

if (-not $executable) {
    $builds = Join-Path $installRoot 'builds'
    if (Test-Path -LiteralPath $builds) {
        $executable = Get-ChildItem -LiteralPath $builds -Directory |
            Sort-Object LastWriteTime -Descending |
            ForEach-Object { Join-Path $_.FullName 'zedlocal.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } |
            Select-Object -First 1
    }
}

if (-not $executable) {
    throw 'No installed Zed Local build found.'
}
if ($userData) {
    Start-Process -FilePath $executable -ArgumentList @('--user-data-dir', ('"' + $userData + '"')) -WorkingDirectory (Split-Path -Parent $executable)
} else {
    Start-Process -FilePath $executable -WorkingDirectory (Split-Path -Parent $executable)
}
