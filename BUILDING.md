# Building Gugu Client from Source

Gugu Client ships as a self-extracting portable Windows executable (`GuguClientForRoblox-Portable-<Version>.exe`) with the full Electron runtime, ASAR payload, native libraries, and custom branding embedded.

## Prerequisites
- **OS:** Windows 10 or Windows 11 (x64)
- **Built-in Windows tools:** `tar.exe` and .NET Framework 4.x compiler (`csc.exe`)
- **Node.js:** v20+ (for `@electron/asar` packing and `rcedit` resource editing)

## Quick Build (New Release)

To bump the version, repack `resources/app.asar`, update icons & PE metadata (`FileDescription`, `ProductName`, `ProductVersion`), and compile the portable launcher:

```powershell
.\build\repack_and_build.ps1 -Version 1.0.17
```

This outputs `GuguClientForRoblox-Portable-1.0.17.exe` in the repository root.

## How the Portable Bootstrapper Works

1. `build/PortableLauncher.cs` compiles into a lightweight Win32 C# bootstrapper with `app.zip` embedded as a managed resource.
2. On first launch of a version, it extracts the runtime into `%LOCALAPPDATA%\GuguClientForRoblox\portable-<Version>\`.
3. Older `portable-*` directories from previous versions are automatically cleaned up to save disk space while keeping user settings intact in `%APPDATA%\Gugu Client for Roblox\`.
4. The launcher passes `GUGU_PORTABLE_EXE` in the environment so the integrated GitHub Auto-Updater can update your portable `.exe` in-place.

Test extraction only without launching:

```powershell
.\GuguClientForRoblox-Portable-1.0.16.exe --extract-only
```

## Notes on ASAR Integrity Validation
If modifying `app.asar` on a fresh Electron binary, ensure `EnableEmbeddedAsarIntegrityValidation` is disabled via `@electron/fuses`:

```powershell
npx @electron/fuses write --app GuguClient.exe EnableEmbeddedAsarIntegrityValidation=off
```
