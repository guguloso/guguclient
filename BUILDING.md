# Building Gugu Client from Source

Gugu Client is a native Rust application on [Tauri 2](https://tauri.app). It ships as one executable, `GuguClient.exe`, with the UI embedded.

## Layout

| Path | Contents |
| --- | --- |
| `src-tauri/src/` | Rust backend: IPC router, Roblox API client, launcher services |
| `src-tauri/scripts/prepare-frontend.mjs` | Copies `ui/renderer` to `dist-tauri/`, generates the `window.api` bridge from `ui/preload/index.js`, writes `docs/rust-port-coverage.md` |
| `ui/renderer/` | The launcher UI (prebuilt HTML/JS/CSS) |
| `ui/preload/index.js` | Source of the `window.api` surface the bridge recreates |

## Prerequisites

- Windows 10 or 11 (x64)
- [Rust](https://rustup.rs) stable (MSVC toolchain) and the Visual Studio C++ Build Tools
- Node.js 20+ (only runs `prepare-frontend.mjs`; no npm packages needed)

## Build

```powershell
cd src-tauri
node scripts/prepare-frontend.mjs
cargo build --release --features custom-protocol
```

Output: `src-tauri/target/release/GuguClient.exe`. The `custom-protocol` feature embeds `dist-tauri/` in the binary; without it the app expects a dev server.

On machines with little RAM, add `-j 2` to limit parallel compilation.

## Development

```powershell
cd src-tauri
node scripts/prepare-frontend.mjs
cargo run
```

Set `GUGU_DEBUG_PORT=9333` to expose the WebView2 DevTools protocol on that port.

## Tests

```powershell
cd src-tauri
cargo test --lib
```

## Releases and auto-update

The built-in updater reads the latest release of `guguloso/guguclient` on GitHub and downloads the first `.exe` asset whose version is newer than the running one. To publish an update, bump `version` in `src-tauri/Cargo.toml` and `src-tauri/tauri.conf.json`, build, and attach `GuguClient.exe` to a release tagged `v<version>`.
