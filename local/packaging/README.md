# ZedLocal packaging

These files are copied into the Windows release ZIP. They are intentionally
outside the Rust build:

- `Install.ps1` installs a build side-by-side under
  `%LOCALAPPDATA%\Programs\ZedLocal\builds\<build>`, keeps user data separate,
  and points shortcuts at the launcher.
- `Update-ZedLocal.ps1` checks
  `https://github.com/KnightAnguish/zed-local/releases/latest/download/latest.json`,
  verifies the published ZIP SHA256, installs it, and starts the new build.
- `ZedLocalLauncher.vbs` runs the updater hidden and launches the current or
  newly installed executable.
- `Check-Update.cmd` is the manual check entry point.
- `Portable.cmd` is the no-install path; it does not use auto-update.

## Release asset contract

A release must publish:

- `ZedLocal-<tag>-win64.zip`
- `latest.json`

`latest.json` contains `release`, `published_at`, `asset_url`, `sha256`,
`build`, and `version`. The updater refuses URLs outside the
`KnightAnguish/zed-local/releases/download/` prefix and verifies the ZIP hash
before running the installer.

The updater trusts the GitHub account and HTTPS; SHA256 verifies transport and
storage integrity, not release authorship. Do not auto-publish releases from
an unsecured account.
