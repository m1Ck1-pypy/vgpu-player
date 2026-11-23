@echo off
rem Per-user file associations for the portable build (HKCU, no admin rights).
rem Put this file next to vgpu.exe and double-click it.
rem Remove the associations again: register-associations.cmd remove
rem Add/remove this folder in the user PATH: register-associations.cmd path [remove]
rem
rem Installed builds (Inno, MSI) do this themselves; see docs/SPEC.md 15.5.

setlocal
set "APP=%~dp0vgpu.exe"
set "CLASSES=HKCU\Software\Classes"
set "EXE=vgpu.exe"
set "VIDEO_EXTS=.mp4 .m4v .mov .mkv .webm .ogv .avi .ts .m2ts .mts .mpg .mpeg .flv .wmv .3gp .3g2"
set "AUDIO_EXTS=.mp3 .m4a .m4b .mp2 .aac .flac .wav .ogg .oga .opus .wma .ac3 .dts"

if /i "%~1"=="remove" goto remove
if /i "%~1"=="path" goto path

if not exist "%APP%" (
    echo vgpu.exe was not found next to this script: "%APP%"
    exit /b 1
)

call :progid VGPUPlayer.Video "VGPU Player video"
call :progid VGPUPlayer.Audio "VGPU Player audio"
call :application

for %%e in (%VIDEO_EXTS%) do call :assoc "%%e" VGPUPlayer.Video
for %%e in (%AUDIO_EXTS%) do call :assoc "%%e" VGPUPlayer.Audio

echo Done. Right-click a media file, "Open with" -^> "Choose another app" -^> VGPU Player,
echo tick "Always use this app" to make a double-click work from now on.
exit /b 0

:progid
rem %1 - ProgID, %2 - human readable type name.
reg add "%CLASSES%\%~1" /ve /t REG_SZ /d "%~2" /f >nul
reg add "%CLASSES%\%~1\DefaultIcon" /ve /t REG_SZ /d "\"%APP%\",0" /f >nul
reg add "%CLASSES%\%~1\shell\open\command" /ve /t REG_SZ /d "\"%APP%\" \"%%1\"" /f >nul
exit /b 0

:application
rem Older builds used player-app.exe; drop the stale Open-with entry.
reg delete "%CLASSES%\Applications\player-app.exe" /f >nul 2>&1
reg add "%CLASSES%\Applications\%EXE%" /v FriendlyAppName /t REG_SZ /d "VGPU Player" /f >nul
reg add "%CLASSES%\Applications\%EXE%\DefaultIcon" /ve /t REG_SZ /d "\"%APP%\",0" /f >nul
reg add "%CLASSES%\Applications\%EXE%\shell\open\command" /ve /t REG_SZ /d "\"%APP%\" \"%%1\"" /f >nul
exit /b 0

:assoc
rem %1 - extension, %2 - ProgID. Empty values: Windows only checks presence.
reg add "%CLASSES%\Applications\%EXE%\SupportedTypes" /v "%~1" /t REG_SZ /d "" /f >nul
reg add "%CLASSES%\%~1\OpenWithProgids" /v "%~2" /t REG_SZ /d "" /f >nul
exit /b 0

:path
set "APP_DIR=%~dp0"
if "%APP_DIR:~-1%"=="\" set "APP_DIR=%APP_DIR:~0,-1%"
if not exist "%APP_DIR%\vgpu.exe" (
    echo vgpu.exe was not found next to this script: "%APP_DIR%\vgpu.exe"
    exit /b 1
)
powershell -NoProfile -ExecutionPolicy Bypass -Command "$dir = $env:APP_DIR; $key = 'HKCU:\Environment'; $item = Get-ItemProperty -Path $key -Name Path -ErrorAction SilentlyContinue; $parts = @(); if ($item -and $item.Path) { $parts = @($item.Path -split ';' | Where-Object { $_ -ne '' }) }; $exists = @($parts | Where-Object { $_.TrimEnd('\') -ieq $dir }).Count -gt 0; if ('%~2' -ieq 'remove') { if (-not $exists) { Write-Host 'VGPU Player is not in the user PATH.'; exit 0 }; $parts = @($parts | Where-Object { $_.TrimEnd('\') -ine $dir }); if ($parts.Count -eq 0) { Remove-ItemProperty -Path $key -Name Path -ErrorAction SilentlyContinue } else { Set-ItemProperty -Path $key -Name Path -Value ($parts -join ';') -Type ExpandString }; Write-Host 'Removed VGPU Player from the user PATH. Open a new console.' } else { if ($exists) { Write-Host 'VGPU Player is already in the user PATH.'; exit 0 }; $new = if ($parts.Count -eq 0) { $dir } else { ($parts -join ';') + ';' + $dir }; if ($new.Length -gt 2047) { Write-Host 'The user PATH would exceed 2047 characters.'; exit 1 }; Set-ItemProperty -Path $key -Name Path -Value $new -Type ExpandString; Write-Host 'Added VGPU Player to the user PATH. Open a new console.' }"
if errorlevel 1 exit /b 1
exit /b 0

:remove
for %%e in (%VIDEO_EXTS%) do call :unassoc "%%e" VGPUPlayer.Video
for %%e in (%AUDIO_EXTS%) do call :unassoc "%%e" VGPUPlayer.Audio
reg delete "%CLASSES%\VGPUPlayer.Video" /f >nul 2>&1
reg delete "%CLASSES%\VGPUPlayer.Audio" /f >nul 2>&1
reg delete "%CLASSES%\Applications\%EXE%" /f >nul 2>&1
reg delete "%CLASSES%\Applications\player-app.exe" /f >nul 2>&1
echo Associations removed.
exit /b 0

:unassoc
reg delete "%CLASSES%\%~1\OpenWithProgids" /v "%~2" /f >nul 2>&1
exit /b 0
