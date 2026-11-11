# ZedLocal packaging

These files are copied into the Windows release ZIP. The launcher and setup
binaries are built from `crates/zed_local_installer` and are not copied from
this folder.

- `ZedLocalLauncher.exe` (built) reads `install.json` and starts the current
  build with `--user-data-dir`. It never downloads, unpacks, or installs.
- `Install.exe` (built) is the native setup. It verifies `manifest.json`,
  installs a build side-by-side under
  `%LOCALAPPDATA%\Programs\ZedLocal\builds\<build>`, keeps user data separate,
  and points Desktop/Start Menu shortcuts at the launcher.
- The in-app updater downloads and verifies the release ZIP, then calls the
  same native installer. No `powershell.exe`, no `Expand-Archive`, no
  `Install.ps1` in the normal install/update path.
- `Install.ps1` is kept only as a transition for builds whose in-app updater
  still invokes it (the `.1` release). New installs should use `Install.exe`.
- `Uninstall.ps1` / `Uninstall.cmd` are kept for older installs. New installs
  uninstall with `ZedLocalLauncher.exe --uninstall`.

## Release asset contract

A release must publish:

- `ZedLocal-<tag>-win64.zip`
- `latest.json`

`latest.json` contains `release`, `published_at`, `asset_url`, `sha256`,
`build`, and `version`. The updater refuses URLs outside the
`KnightAnguish/zed-local/releases/download/` prefix and verifies the ZIP hash
before running the native installer.

The updater trusts the GitHub account and HTTPS; SHA256 verifies transport and
storage integrity, not release authorship. Do not auto-publish releases from
an unsecured account.
