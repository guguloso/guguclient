Add-Type -AssemblyName System.Drawing

$src = "C:\Users\Admin\.gemini\antigravity\brain\c36e3410-a3ca-4bf2-965a-9a1c3f9ced43\.user_uploaded\media_1794284910281_0710db04.png"
$buildDir = "C:\Users\Admin\Desktop\lunar-for-roblox-decompiled-main\temp_fix\build"
$resDir = "C:\Users\Admin\Desktop\lunar-for-roblox-decompiled-main\temp_fix\resources"

# Copy Horns to resources\icon.png and build\icon.png
Copy-Item -Path $src -Destination (Join-Path $resDir "icon.png") -Force
Copy-Item -Path $src -Destination (Join-Path $buildDir "icon.png") -Force

# Generate .ico file
$icoOut = Join-Path $buildDir "icon.ico"
$bmp = [System.Drawing.Bitmap]::FromFile($src)
$thumb = New-Object System.Drawing.Bitmap($bmp, 256, 256)
$hIcon = $thumb.GetHicon()
$icon = [System.Drawing.Icon]::FromHandle($hIcon)

$fs = [System.IO.File]::Create($icoOut)
$icon.Save($fs)
$fs.Close()
$icon.Dispose()
$thumb.Dispose()
$bmp.Dispose()

Write-Host "Icons generated successfully at $icoOut"
