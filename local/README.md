# Local fork notes

This branch is based on Zed 1.27.0 (`c84af5b99cbc366d675064e52b23664a015b6e35`)
with local agent, history, and Windows build changes.

- Branch: `local/update-1270-20261105`
- Prebuilt Windows x64 package:
  https://github.com/KnightAnguish/zed-local/releases
- Build: `script/local-build.ps1` (defaults to `%LOCALAPPDATA%\ZedLocalBuild`,
  override with `-BuildRoot`)
- Run: `script/local-run.ps1 -UserDataDirectory <separate profile>`

The prebuilt package also contains a `codex-acp` bundle. Its source patch is
kept here as `codex-acp-question-fix.patch`; it is applied to the separate
`codex-acp-modern` project and rebuilt with `npm run build`.

The fork keeps upstream as a disabled push remote:

```text
upstream  https://github.com/zed-industries/zed.git
fork      https://github.com/KnightAnguish/zed-local.git
```
