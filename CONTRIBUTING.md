# Contributing to Gugu Client

Thank you for your interest in contributing to **Gugu Client**!

## Reporting Issues
- Check existing issues before opening a new one.
- Use the **Bug Report** template and include your Windows version, Gugu Client version, and relevant log snippets from `%APPDATA%\Gugu Client for Roblox\logs\`.

## Pull Requests
1. Fork the repository and create a branch from `main`.
2. Make sure any modifications in `temp_fix/out/main/index.js` or `preload/index.js` pass syntax validation (`node -c temp_fix/out/main/index.js`).
3. Ensure JSON files (such as `temp_fix/package.json`) are UTF-8 **without BOM** to prevent Electron startup exceptions.
4. Do not commit compiled `.exe`, `.asar`, `.dll`, or `node_modules/` binaries — only commit source files and scripts.
5. Verify your build locally using `.\build\repack_and_build.ps1`.
