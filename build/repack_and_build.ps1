param(
    [string]$Version
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$tempFix = Join-Path $root "temp_fix"
$pkgPath = Join-Path $tempFix "package.json"

# Read or update version in package.json (without BOM)
$pkgObj = Get-Content $pkgPath -Raw | ConvertFrom-Json
if ($Version) {
    $pkgObj.version = $Version
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($pkgPath, ($pkgObj | ConvertTo-Json -Depth 20), $utf8NoBom)
    Write-Host "Updated package.json version to $Version"
} else {
    $Version = $pkgObj.version
}

$localApp = [System.Environment]::GetFolderPath([System.Environment+SpecialFolder]::LocalApplicationData)
$appDir = Join-Path $localApp ("GuguClientForRoblox\portable-" + $Version)
if (-not (Test-Path $appDir)) {
    $prevDir = Get-ChildItem (Join-Path $localApp "GuguClientForRoblox") -Directory -Filter "portable-*" | Select-Object -First 1
    if ($prevDir) {
        Write-Host "Copying runtime binaries from $($prevDir.FullName) to $appDir ..."
        Copy-Item -Path $prevDir.FullName -Destination $appDir -Recurse -Force
    }
}

$targetAsar = Join-Path $appDir "resources\app.asar"
$targetExe = Join-Path $appDir "GuguClient.exe"
$ico = Join-Path $tempFix "build\icon.ico"
$rcedit = Join-Path $tempFix "node_modules\rcedit\bin\rcedit-x64.exe"

# 1. Generate icons
Write-Host "Generating icons..."
& "$PSScriptRoot\make_icons.ps1"

# 2. Update PE headers for GuguClient.exe
if ((Test-Path $targetExe) -and (Test-Path $rcedit)) {
    Write-Host "Updating GuguClient.exe PE resources with rcedit..."
    & $rcedit $targetExe `
        --set-icon $ico `
        --set-file-version $Version `
        --set-product-version $Version `
        --set-version-string "FileDescription" "Gugu Client for Roblox" `
        --set-version-string "ProductName" "Gugu Client for Roblox" `
        --set-version-string "CompanyName" "Gugu Client"
}

# 3. Repack ASAR
Write-Host "Repacking ASAR from $tempFix to $targetAsar ..."
npx --yes @electron/asar pack $tempFix $targetAsar
Write-Host "ASAR packed successfully!"

# 4. Rebuild Portable launcher
Write-Host "Rebuilding portable launcher executable (v$Version)..."
& "$PSScriptRoot\build-portable.ps1" -AppDir $appDir -Version $Version

# 5. Update PE headers for Portable launcher
$portableExe = Join-Path $root "GuguClientForRoblox-Portable-$Version.exe"
if ((Test-Path $portableExe) -and (Test-Path $rcedit)) {
    Write-Host "Updating portable launcher PE resources with rcedit..."
    & $rcedit $portableExe `
        --set-icon $ico `
        --set-file-version $Version `
        --set-product-version $Version `
        --set-version-string "FileDescription" "Gugu Client for Roblox" `
        --set-version-string "ProductName" "Gugu Client for Roblox" `
        --set-version-string "CompanyName" "Gugu Client"
}

Write-Host "All tasks completed successfully! Output: $portableExe"
