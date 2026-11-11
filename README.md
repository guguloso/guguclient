<h1 align="center">
    Gugu Client
</h1>

<p align="center">
    A lightweight, high-performance Roblox launcher &amp; client built for clean aesthetics, FastFlag tuning, and deep customization.
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
> The official repository [guguloso/guguclient](https://github.com/guguloso/guguclient) is the **ONLY PLACE** where you should download the official Gugu Client executable. Any other source is **NOT** affiliated with us and may pose a potential security threat.

---

## Key Features &amp; Improvements

### Performance &amp; Optimization
- **FastFlag Profiles &amp; Editor**: Unlock FPS caps, adjust lighting/rendering quality, tweak textures, and disable unnecessary telemetry before Roblox launches
- **Zero Background Bloat**: Runs strictly when you play — no persistent startup services or hidden background updaters eating RAM
- **Low Overhead**: Fast process detection with minimal CPU/memory footprint while you are in-game
- **Clean Storage**: Automatically cleans up old runtime caches after updates while keeping your settings and profiles safe

### Integrations &amp; Discord RPC
- Custom **Discord Rich Presence** that displays the experience you're playing, creator name, and active session playtime
- Quick **See game page** and **Join Server** buttons directly on your Discord profile
- Full **Bloxstrap RPC** support for dynamic in-game status updates
- Native `roblox://` deep-link support — click Play on the Roblox website and launch seamlessly through Gugu Client

### Mods &amp; Customization
- Customize cursors, sounds, fonts, and game assets per instance
- Sleek dark-themed launcher UI with minimal visual clutter
- Built-in macro engine and Roblox Studio launching support

### Multi-Instance &amp; Accounts
- **Account Switcher**: Manage and switch between multiple Roblox accounts in one click
- **Multi-Instance Support**: Launch and track multiple Roblox sessions simultaneously from the title bar or system tray
- **Automatic Updates**: Built-in **Check for Updates** downloads and installs new releases directly from GitHub with zero manual setup

---

## Built with Rust

Gugu Client is a fully native **Rust + [Tauri 2](https://tauri.app)** application — no Electron, no bundled Chromium, no Node.js runtime.

- **Single executable**: one small `GuguClient.exe` with the UI embedded, rendered by the system's WebView2
- **Native backend**: every launcher feature (accounts, instances, FastFlags, mods, Discord RPC, macros, captures, avatar 3D preview) runs in async Rust on Tokio
- **Lower footprint**: a fraction of the memory and disk space of the old Electron build

```
guguclient/
├── src-tauri/          # Rust backend (Tauri 2)
│   ├── src/
│   │   ├── ipc/        # IPC router: one command serves every `window.api` channel
│   │   ├── roblox/     # Roblox web API client (CSRF, challenges, rate limits)
│   │   └── services/   # Launcher services: launch, versions, mods, macros, avatar render…
│   ├── scripts/        # prepare-frontend.mjs: embeds the UI and generates the bridge
│   └── icons/
├── ui/                 # Launcher UI (renderer + preload API surface)
├── docs/               # IPC coverage report
└── .github/            # CI workflow and issue templates
```

---

## Quick Start

1. Download `GuguClient.exe` from the latest release on the [Releases][repo-releases] page.
2. Run it — a single native executable, no installer and no extraction step. Accounts, settings and instances live in `%USERPROFILE%\.lunar-for-roblox\`.
3. Sign in, configure your FastFlags, Mods or Discord RPC preferences and launch any Roblox game.

### System Requirements
- **OS:** Windows 10 or Windows 11 (64-bit)
- **Runtime:** Microsoft Edge WebView2 (preinstalled on Windows 11 and up-to-date Windows 10)

---

## Building from Source

Gugu Client is a native Rust application built on [Tauri 2](https://tauri.app): the backend lives in `src-tauri/`, the UI in `ui/`.

```powershell
cd src-tauri
node scripts/prepare-frontend.mjs
cargo build --release --features custom-protocol
```

The executable is written to `src-tauri/target/release/GuguClient.exe`. See [BUILDING.md](./BUILDING.md) for details.

---

## Licensing

Custom additions and build tooling in this repository are licensed under the [MIT License](./LICENSE).

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
