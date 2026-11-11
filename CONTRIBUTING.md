# Contributing to Gugu Client

Thank you for your interest in contributing to **Gugu Client**!

## Reporting Issues
- Check existing issues before opening a new one.
- Use the **Bug Report** template and include your Windows version, Gugu Client version, and relevant log snippets from `%USERPROFILE%\.lunar-for-roblox\logs\`.

## Pull Requests
1. Fork the repository and create a branch from `main`.
2. Run `cargo test --lib` and `cargo clippy` in `src-tauri/` before opening the PR.
3. Keep IPC channel names and payload shapes identical to what `ui/preload/index.js` expects; the UI is not modified by the backend.
4. Do not commit build output (`src-tauri/target/`, `dist-tauri/`, `src-tauri/generated/`) or compiled `.exe` files.
5. See [BUILDING.md](./BUILDING.md) to build locally.
