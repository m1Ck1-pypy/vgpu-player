set shell := ["powershell", "-NoProfile", "-Command"]

# Пересобрать release-exe и портативную папку dist/vgpu-player.
portable:
    $env:RUSTFLAGS = "-Clinker=rust-lld"; cargo build --release -p player-app
    New-Item -ItemType Directory -Force -Path dist/vgpu-player | Out-Null
    if (Test-Path dist/vgpu-player/player-app.exe) { Remove-Item dist/vgpu-player/player-app.exe -Force }
    Copy-Item target/release/vgpu.exe dist/vgpu-player/ -Force
    Copy-Item installer/register-associations.cmd dist/vgpu-player/ -Force
    $dlls = @("avcodec-63.dll", "avdevice-63.dll", "avfilter-12.dll", "avformat-63.dll", "avutil-61.dll", "swresample-7.dll", "swscale-10.dll", "dav1d.dll"); foreach ($dll in $dlls) { Copy-Item ".tools/vcpkg/installed/x64-windows/bin/$dll" dist/vgpu-player/ -Force }

# Собрать установщик Inno Setup (per-user, без UAC).
installer-inno: portable
    & "C:\Program Files\Inno Setup 7\ISCC.exe" installer/vgpu-player.iss

# Собрать MSI через WiX v3 (per-user, без UAC). Имя файла берётся из версии.
installer-msi: portable
    New-Item -ItemType Directory -Force -Path target/wix | Out-Null
    & ".tools/wix/candle.exe" -arch x64 -dSourceDir=dist/vgpu-player -o target/wix/ installer/vgpu-player.wxs installer/AppFiles.wxs
    $version = ((Select-String -Path Cargo.toml -Pattern '^version = "(?<v>\d+\.\d+\.\d+[^"]*)"' | Select-Object -First 1).Matches[0].Groups['v'].Value); & ".tools/wix/light.exe" -ext .tools/wix/WixUIExtension.dll -pdbout target/wix/vgpu-player.wixpdb -o ("dist/vgpu-player-" + $version + ".msi") target/wix/vgpu-player.wixobj target/wix/AppFiles.wixobj

# Пересобрать оба выбранных установщика.
installers: installer-inno installer-msi

# Обновить версию во всех местах, где она зашита (версия из semver, напр. 0.2.0).
set-version version:
    @$v = "{{version}}"; if ($v -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$') { Write-Host "version must look like 1.2.3" -ForegroundColor Red; exit 1 }; $enc = [System.Text.UTF8Encoding]::new($false); function Save($p, $t) { [System.IO.File]::WriteAllText((Resolve-Path $p).Path, $t, $enc) }; $t = [System.IO.File]::ReadAllText('Cargo.toml'); $t = $t -replace '(?m)^(version = ")[^"]*(")(\r?)$', ('${1}' + $v + '${2}${3}'); Save 'Cargo.toml' $t; $t = [System.IO.File]::ReadAllText('installer/vgpu-player.iss'); $t = $t -replace '(?m)^([^\r\n]*MyAppVersion ")[^"]*(")(\r?)$', ('${1}' + $v + '${2}${3}'); Save 'installer/vgpu-player.iss' $t; $t = [System.IO.File]::ReadAllText('installer/vgpu-player.wxs'); $t = $t -replace '(?m)^(\s*Version=")[^"]*(")', ('${1}' + $v + '${2}'); Save 'installer/vgpu-player.wxs' $t; $t = [System.IO.File]::ReadAllText('README.md'); $t = $t -replace '(Версия )\d+\.\d+\.\d+', ('${1}' + $v); $t = $t -replace '(vgpu-player-)\d+\.\d+\.\d+', ('${1}' + $v); Save 'README.md' $t; $t = [System.IO.File]::ReadAllText('crates/player-gpui/README.md'); $t = $t -replace '(v)\d+\.\d+\.\d+', ('${1}' + $v); Save 'crates/player-gpui/README.md' $t; cargo update -w; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Host "version -> $v (docs/ update manually)"

# Создать аннотированный тег v<версия из Cargo.toml>; пуш — отдельной командой.
tag:
    @$version = ((Select-String -Path Cargo.toml -Pattern '^version = "(?<v>\d+\.\d+\.\d+[^"]*)"' | Select-Object -First 1).Matches[0].Groups['v'].Value); if (-not $version) { Write-Host "cannot read version from Cargo.toml" -ForegroundColor Red; exit 1 }; if (git rev-parse -q --verify "refs/tags/v$version") { Write-Host "tag v$version already exists" -ForegroundColor Red; exit 1 }; if (git status --porcelain) { Write-Host "working tree is dirty: commit changes before tagging" -ForegroundColor Red; exit 1 }; git tag -a "v$version" -m "vgpu-player $version"; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Host "created v$version; push: just gp $version"

# Опубликовать main и тег: just gp 0.2.0 (или v0.2.0).
gp tag:
    @$t = "{{tag}}"; if ($t -notmatch '^v?\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$') { Write-Host "tag must look like v1.2.3 or 1.2.3" -ForegroundColor Red; exit 1 }; if (-not $t.StartsWith('v')) { $t = "v$t" }; if (-not (git rev-parse -q --verify "refs/tags/$t")) { Write-Host "tag $t not found locally; run: just tag" -ForegroundColor Red; exit 1 }; $env:GIT_TERMINAL_PROMPT = "0"; git push origin main $t; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; Write-Host "pushed main and $t"

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
