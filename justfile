set shell := ["powershell", "-NoProfile", "-Command"]

# Пересобрать release-exe и портативную папку dist/vgpu-player.
portable:
    $env:RUSTFLAGS = "-Clinker=rust-lld"; cargo build --release -p player-app
    New-Item -ItemType Directory -Force -Path dist/vgpu-player | Out-Null
    Copy-Item target/release/player-app.exe dist/vgpu-player/ -Force
    $dlls = @("avcodec-63.dll", "avdevice-63.dll", "avfilter-12.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll", "swscale-10.dll", "dav1d.dll"); foreach ($dll in $dlls) { Copy-Item ".tools/vcpkg/installed/x64-windows/bin/$dll" dist/vgpu-player/ -Force }

# Собрать установщик Inno Setup (per-user, без UAC).
installer-inno: portable
    & "C:\Program Files\Inno Setup 7\ISCC.exe" installer/vgpu-player.iss

# Собрать MSI через WiX v3 (per-user, без UAC).
installer-msi: portable
    New-Item -ItemType Directory -Force -Path target/wix | Out-Null
    & ".tools/wix/candle.exe" -arch x64 -dSourceDir=dist/vgpu-player -o target/wix/ installer/vgpu-player.wxs installer/AppFiles.wxs
    & ".tools/wix/light.exe" -ext .tools/wix/WixUIExtension.dll -pdbout target/wix/vgpu-player.wixpdb -o dist/vgpu-player-0.1.0.msi target/wix/vgpu-player.wixobj target/wix/AppFiles.wixobj

# Пересобрать оба выбранных установщика.
installers: installer-inno installer-msi

# DLL для примеров: cargo кладёт их exe в target/debug/examples.
_example-dlls:
    New-Item -ItemType Directory -Force -Path target/debug/examples | Out-Null
    Copy-Item .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/examples/ -Force

# Пример minimal; свой файл: just example-minimal path/to/file.
example-minimal file="assets/test/sync_test.mp4": _example-dlls
    $env:RUSTFLAGS = "-Clinker=rust-lld"; cargo run -p player-gpui --example minimal -- "{{file}}"

# Пример embed_in_app; свой файл: just example-embed path/to/file.
example-embed file="assets/test/sync_test.mp4": _example-dlls
    $env:RUSTFLAGS = "-Clinker=rust-lld"; cargo run -p player-gpui --example embed_in_app -- "{{file}}"
