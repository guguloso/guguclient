<h1 align="center">
    Gugu Client
</h1>

<p align="center">
    A custom Roblox launcher &amp; client, focused on clean aesthetics, performance, and customization.
</p>

<p align="center">
    <img src="./.resources/logo.png" height="200" alt="Gugu Client logo"/>
</p>

<p align="center">
    If you'd like to support this project, consider giving this repository a star!
</p>

<div align="center">

[![Downloads (Total)][badge-repo-downloads-total]][repo-releases]
[![Downloads (Latest)][badge-repo-downloads]][repo-releases]
[![Version][badge-repo-latest]][repo-latest]
[![Stars][badge-repo-stars]][repo-stargazer]

</div>

> [!CAUTION]
> The official repository [guguloso/guguclient](https://github.com/guguloso/guguclient) is the **ONLY PLACE** where you should download the official Gugu Client binary/executable. Any other source is **NOT** affiliated with us and may pose a security risk.

---

## Features

### Integrations &amp; Discord RPC
- Custom **Gugu For Roblox** Discord Rich Presence showing launcher status and active game details
- Automatic game metadata resolution (experience name, creator, session timer, and quick game link)
- Full **Bloxstrap RPC** support for rich in-game status updates
- Native `roblox://` protocol handler integration

### Performance, FastFlags &amp; Mods
- Built-in FastFlag editor and presets for FPS, graphics, and client performance tuning
- Custom mods, cursor, sound, and asset management per instance
- Multiple Roblox versions &amp; channel switching support
- Built-in Macros &amp; Roblox Studio launcher support

### UI &amp; Customization
- Sleek dark-themed interface branded with Gugu Client identity across launcher, taskbar, tray, and title bar
- Custom Gugu Client logo dynamically styled to match interface themes and watermarks
- Portable self-contained Windows executable — zero manual setup required

### Multi-Instance &amp; Accounts
- Built-in Roblox account switcher &amp; session manager
- Multi-instance launching support
- Integrated **Check for Updates** powered directly by GitHub Releases

---

## Auto-Updater &amp; Releases

Gugu Client checks for updates automatically using this repository's **GitHub Releases**:

1. Build a new portable release:
   ```powershell
   .\build\repack_and_build.ps1 -Version 1.0.17
   ```
2. Go to **Releases** &rarr; **Draft a new release**, set tag `v1.0.17`, and attach `GuguClientForRoblox-Portable-1.0.17.exe`.
3. Publish the release — clicking **Check for Updates** in the client will automatically download and apply the new version.

---

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
[badge-repo-downloads]: https://img.shields.io/github/downloads/guguloso/guguclient/latest/total?style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-downloads-total]: https://img.shields.io/github/downloads/guguloso/guguclient/total?style=for-the-badge&color=18181b&labelColor=27272a
[badge-repo-latest]: https://img.shields.io/github/v/release/guguloso/guguclient?style=for-the-badge&color=ffffff&labelColor=18181b
[badge-repo-stars]: https://img.shields.io/github/stars/guguloso/guguclient?style=for-the-badge&color=18181b&labelColor=27272a

[repo-releases]: https://github.com/guguloso/guguclient/releases
[repo-latest]: https://github.com/guguloso/guguclient/releases/latest
[repo-stargazer]: https://github.com/guguloso/guguclient/stargazers
