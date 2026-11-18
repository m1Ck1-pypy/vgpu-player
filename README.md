# VGPU Player

<img src="assets/icon.png" width="96" alt="VGPU Player">

Видеоплеер локальных файлов на Rust и [GPUI](https://github.com/longbridge/gpui-fast)
для Windows. Учебный проект: цель — понять каждую строку и научиться Rust, а не
сделать конкурирующий плеер.

Версия 0.1.0. Этапы 0–8 приняты; этап 9 готов к приёмке: звук и видео,
A/V-синхронизация по аудио-часам, управление, надёжность, пул BGRA-буферов,
полировка интерфейса, библиотека `player-gpui`, установщики.

## Возможности

- воспроизведение локальных видео- и аудиофайлов через Symphonia / FFmpeg;
- пауза, перемотка, громкость, mute, полноэкранный режим;
- клик по кадру — пауза/продолжение; колесо мыши — громкость шагами по 1 %
  с плашкой-индикатором; процент громкости — по наведению на ползунок;
- горячие клавиши: `Space`, `←`/`→`, `F`, `O`, `Esc`;
- открытие файла диалогом, из меню и перетаскиванием в окно;
- превью первого кадра, «Информация о файле», «О программе»;
- оверлей статистики синхронизации, автоскрытие панели;
- библиотека `player-gpui` для встраивания плеера в чужое GPUI-приложение;
- портативная сборка и установщики: Inno Setup и MSI (WiX).

## Структура

| Путь                 | Что внутри                                                                 |
| -------------------- | -------------------------------------------------------------------------- |
| `crates/player-core` | движок без GPUI: демультиплексирование, декодирование, часы, синхронизация |
| `crates/player-gpui` | библиотека UI: `VideoPlayer`, `video_view`, `controls_view`, `run_app`     |
| `crates/player-app`  | демонстрационное приложение                                                |
| `crates/player-cli`  | консольный запуск ядра и замеры                                            |
| `installer/`         | Inno Setup и WiX-исходники установщиков                                    |
| `docs/`              | `SPEC.md`, `CURRENT_TASK.md` и история решений                             |
| `assets/`            | иконка; тестовые медиафайлы генерируются локально                          |

## Требования

- Rust 1.98+ (требование `gpui-fast`);
- Windows 10/11 x64;
- FFmpeg 9 с `libdav1d` — dev-библиотеки для сборки и DLL для запуска;
- `just` — для готовых команд `portable`, `installers` и примеров
  (необязательно, все команды можно выполнить вручную);
- звуковое устройство по умолчанию (без него плеер покажет экран ошибки).

Для сборки библиотеки на Linux нужны системные зависимости GPUI (wayland,
xkbcommon, fontconfig, vulkan, alsa) — они перечислены в CI
(`.github/workflows/ci.yml`).

## Сборка и запуск

FFmpeg ставится через vcpkg один раз на машине:

```bash
git clone https://github.com/m1Ck1-pypy/vgpu-player.git
cd vgpu-player
git clone --depth 1 https://github.com/microsoft/vcpkg.git .tools/vcpkg
.tools/vcpkg/bootstrap-vcpkg.bat -disableMetrics
.tools/vcpkg/vcpkg install "ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,dav1d]" --triplet x64-windows --recurse
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/deps/
```

Сборка и запуск:

```bash
cargo build -p player-app
cargo run -p player-app -- path/to/video.mp4
```

Если сборка идёт из Git Bash и `PATH` подхватывает `/usr/bin/link.exe`, добавьте
`RUSTFLAGS="-Clinker=rust-lld"` (см. `docs/SPEC.md`, раздел 12.4).

Тестовые медиафайлы в git не входят. Команды генерации —
`assets/test/README.md`; для быстрой проверки подойдёт любой локальный файл.

## Управление

| Действие            | Как                                                   |
| ------------------- | ----------------------------------------------------- |
| Пауза / продолжить  | `Space`, кнопка play/pause или клик по кадру          |
| Выбрать видео       | кнопка Play, пока файл не открыт                      |
| Перемотка ±5 с      | `←` / `→`                                             |
| Полноэкранный режим | `F` или кнопка в панели; `Esc` выходит из него        |
| Открыть файл        | `O` / `Ctrl+O`, меню «⋯» → «Открыть…», перетаскивание |
| Громкость           | ползунок или колесо мыши (±1 %)                       |
| Закрыть меню/панели | `Esc` или клик по кадру                               |

Процент громкости не занимает место в панели постоянно: он появляется
подсказкой над ползунком при наведении и плашкой в правом нижнем углу после
шага колесом (исчезает через ~1,5 с).

## Портативная сборка и установщики

```bash
just portable        # release-exe + dist/vgpu-player/ (exe + DLL)
just installer-inno  # dist/vgpu-player-0.1.0-setup.exe
just installer-msi   # dist/vgpu-player-0.1.0.msi
just installers      # portable + оба установщика
```

`dist/` — артефакт сборки, в git не попадает. Inno Setup 7.1.1 на машине
разработчика печатает `Non-commercial use only`; для коммерческого
распространения нужна лицензия Inno Setup или NSIS (в проекте NSIS-скрипта
нет, примечание — `docs/SPEC.md` §15.2). MSI собирается WiX Toolset v3:
скачайте `wix314-binaries.zip` с
<https://github.com/wixtoolset/wix3/releases> и распакуйте в `.tools/wix`.
Для Inno нужен установленный Inno Setup 7 (`ISCC.exe`).

## Использование как библиотека

`player-gpui` даёт готовый `VideoPlayer`, виджет `video_view` и необязательную
панель `controls_view`. Подключение, API и примеры — в
[`crates/player-gpui/README.md`](crates/player-gpui/README.md).

```bash
just example-minimal   # готовое окно через run_app
just example-embed     # своё окно, video_view и свои кнопки
```

## Проверки

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI — `.github/workflows/ci.yml` (Ubuntu, те же проверки).

## Документация

- `docs/SPEC.md` — требования, архитектура, этапы, решения, упаковка;
- `docs/CURRENT_TASK.md` — состояние последней сессии и что осталось;
- `crates/player-gpui/README.md` — встраивание библиотеки;
- `assets/test/README.md` — генерация тестовых файлов.

## Лицензия

MIT OR Apache-2.0. Тексты — `LICENSE-MIT` и `LICENSE-APACHE`.
