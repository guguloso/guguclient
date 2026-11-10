<h1 align="center">
    Gugu Client
</h1>

<p align="center">
    A custom Roblox launcher &amp; client engineered for performance tuning, clean monochrome aesthetics, and full customization.
</p>

<p align="center">
    <img src="./.resources/logo.png" height="200" alt="Gugu Client logo"/>
</p>

<p align="center">
    If you'd like to support this project, consider giving this repository a star!
</p>

<div align="center">

[![License][badge-repo-license]][repo-license]
[![CI][badge-repo-ci]][repo-actions]
[![Downloads (Total)][badge-repo-downloads-total]][repo-releases]
[![Downloads (Latest)][badge-repo-downloads]][repo-releases]
[![Version][badge-repo-latest]][repo-latest]
[![Stars][badge-repo-stars]][repo-stargazer]

</div>

> [!CAUTION]
> The official repository [guguloso/guguclient](https://github.com/guguloso/guguclient) is the **ONLY PLACE** where you should download the official Gugu Client binary/executable. Any other source is **NOT** affiliated with us and poses a potential security risk.

---

## Performance &amp; Optimization

Gugu Client is structured to keep overhead low between the Electron launcher shell and the Roblox Player process:

- **FastFlag Tuning &amp; Profiles**: Configure and apply custom ClientAppSettings / FastFlags before Roblox initializes, enabling FPS uncap adjustments, VSync toggles, rendering pipeline tweaks, and reduced telemetry overhead per instance.
- **Zero Persistent Background Services**: Unlike traditional installers that register background update daemons or startup services, Gugu Client runs strictly on demand and cleans up its process tree on quit.
- **Native Win32 Process Monitoring**: Uses lightweight native C addons (`fastlist`, `get-windows`, `koffi`) instead of heavy PowerShell/WMI polling loops to detect active Roblox sessions, window focus, and crash/disconnect transitions with minimal CPU usage.
- **Automatic Old-Version Cleanup**: When updating via the built-in GitHub updater, the C# portable bootstrapper automatically removes obsolete `portable-*` runtime directories from `%LOCALAPPDATA%\GuguClientForRoblox\` on first boot, preventing disk bloat while preserving user settings in `%APPDATA%\Gugu Client for Roblox\`.
- **Hardware-Accelerated UI with Dynamic Vector/Alpha Masking**: Launcher branding (`BrandMark`) uses CSS `mask-image` alpha masking rather than redundant per-theme image bundles, reducing renderer memory footprint while adapting seamlessly across opacities and components.

---

## Key Features

### Integrations &amp; Discord Rich Presence
- Custom **Gugu For Roblox** Discord Rich Presence (`1569564533830324315`) displaying:
  - Launcher idle status (`Active in Launcher` with the `logo` artwork)
  - Active Roblox game metadata (experience title, creator name, session timer, and direct **See game page** button)
  - Custom in-game main art (`playing`) paired with the transparent corner badge (`logo2`)
- Full **Bloxstrap RPC** protocol support (`SetRichPresence`, `SetLaunchData` with **Join Server** deep-links, rate-limiting, and asset thumbnail caching)
- System default `roblox://` protocol handler integration with automatic focus restoration

### Native GitHub Auto-Updater
- Custom `UpdaterService` querying the GitHub Releases API (`guguloso/guguclient`) directly — zero external update servers required
- Dedicated updater UI window (`#/updater`) showing current-to-target version transitions and real-time MB/s download progress
- Seamless in-place replacement of the portable launcher binary (`GUGU_PORTABLE_EXE`) followed by automatic extraction and relaunch

### Mods, Macros &amp; Multi-Instance
- Per-instance mod overlays and Roblox asset customization (Player &amp; Studio)
- Built-in Roblox account manager and quick account switching
- Multi-instance launch support with running instance tracking in both the title bar and system tray
- Integrated macro policy &amp; trigger service

---

## Repository Layout

This repository contains the complete source tree, native C reconstructions, installer scripts, documentation, and portable build pipeline:

| Directory / File | Description |
| :--- | :--- |
| `temp_fix/` | Active Gugu Client Electron payload (`out/main`, `out/preload`, `out/renderer`, `resources/`, `package.json`) used to build releases |
| `src/` | Readable, formatted Electron main, preload, and renderer source bundles (`main/index.js`, `renderer/index.js`, `consts.js`) |
| `native/` | Decompiled native Win32 C sources (`owutility`, `integrity`, `fastlist`, `get-windows`, `koffi`, Chromium/NSIS shims) |
| `build/` | Portable bootstrapper (`PortableLauncher.cs`), icon generator (`make_icons.ps1`), and 1-command release compiler (`repack_and_build.ps1`) |
| `installer/` | NSIS installer and uninstaller scripts |
| `docs/` | Technical notes covering IPC channels, service architecture, bundle structure, and native DLL internals |
| `tools/` | Extraction and inspection utilities |
| `.github/` | Automated CI verification workflow (`ci.yml`) and issue templates |

---

## System Requirements

- **Operating System:** Windows 10 or Windows 11 (64-bit / x64)
- **Runtime:** Self-contained (Electron + Chromium embedded inside the portable `.exe`; .NET Framework 4.x ships with Windows)
- **Network:** Required for Roblox game launches, game metadata resolution, Discord RPC thumbnails, and GitHub update checks

---

## Building from Source &amp; Releasing Updates

See [BUILDING.md](./BUILDING.md) for full details. To compile a new portable release locally (Windows 10/11 with Node.js v20+):

```powershell
# Bumps version, regenerates icons & PE headers, packs app.asar, and builds the portable .exe
.\build\repack_and_build.ps1 -Version 1.0.17
```

To push an update to all users:
1. Create a new GitHub Release in `guguloso/guguclient` tagged `v1.0.17`.
2. Attach the generated `GuguClientForRoblox-Portable-1.0.17.exe` binary.
3. Publish — any user clicking **Check for Updates** in the client will automatically download and install the update.

---

## Licensing

Custom additions, build scripts, and launcher modifications in this repository are licensed under the [MIT License](./LICENSE).

## Stargazing

To support the development of Gugu Client, consider giving the repository a star.

<a href="https://www.star-history.com/?repos=guguloso%2Fguguclient&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=guguloso/guguclient&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=guguloso/guguclient&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=guguloso/guguclient&type=date&legend=top-left" />
 </picture>
</a>

<!-- Badge defs -->
[badge-repo-license]: https://img.shields.io/github/license/guguloso/guguclient?style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-ci]: https://img.shields.io/github/actions/workflow/status/guguloso/guguclient/ci.yml?branch=main&style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-downloads]: https://img.shields.io/github/downloads/guguloso/guguclient/latest/total?style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-downloads-total]: https://img.shields.io/github/downloads/guguloso/guguclient/total?style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-latest]: https://img.shields.io/github/v/release/guguloso/guguclient?style=for-the-badge&color=ffffff&labelColor=18181b
[badge-repo-stars]: https://img.shields.io/github/stars/guguloso/guguclient?style=for-the-badge&color=18181b&labelColor=27272a

[repo-license]: https://github.com/guguloso/guguclient/blob/main/LICENSE
[repo-actions]: https://github.com/guguloso/guguclient/actions
[repo-releases]: https://github.com/guguloso/guguclient/releases
[repo-latest]: https://github.com/guguloso/guguclient/releases/latest
[repo-stargazer]: https://github.com/guguloso/guguclient/stargazers
