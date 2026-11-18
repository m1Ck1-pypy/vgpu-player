# SPEC — vgpu-player

Единый источник истины: требования, архитектура, приёмка.
`docs/CURRENT_TASK.md` — состояние последней задачи и следующие шаги.
При расхождении прав этот файл. История сессий — `git log`; план HTTP/HLS —
`docs/upgrade_project.md`.

## 0. Состояние на 2026-11-18

| Этап                     | Состояние                            |
| ------------------------ | ------------------------------------ |
| 0 — Каркас               | ✅ принят                            |
| 1 — Звук                 | ✅ принят                            |
| 2 — Аудио-часы           | ✅ принят                            |
| 3 — Декодирование видео  | ✅ принят                            |
| 4 — Кадр в окне          | ✅ принят                            |
| 5 — A/V-синхронизация    | ✅ принят                            |
| 6 — Управление           | ✅ принят                            |
| 7 — Надёжность           | ✅ принят (2026-11-17)               |
| 8 — Производительность   | ✅ принят (2026-11-18)               |
| 9 — Полировка            | ✅ принят (2026-11-18), тег `v0.1.0` |
| 10 — Ассоциации файлов   | ✅ принят (2026-11-18)               |
| 11 — Автовоспроизведение | ✅ принят (2026-11-18)               |
| 12 — Файл настроек       | ✅ принят (2026-11-18), тег `v0.2.0` |

Работает: воспроизведение локальных файлов (Symphonia → FFmpeg для Opus и
прочих, rodio на выводе), аудио-часы ведут видео, кадры в BGRA через FFmpeg,
letterbox и пейсинг, запасной таймер для файлов без звука, `epoch`-перемотка,
панель поверх кадра, меню «⋯» («Открыть…», оверлей статистики, «Информация о
файле», «О программе», автоскрытие), открытие файла через `O`/`Ctrl+O`, меню и
drag-and-drop, превью первого кадра, fullscreen кнопкой и `F`/`Esc`, колесо
мыши для громкости, библиотека `player-gpui`, `player-app`, CLI, CI,
портативная папка и два установщика, ассоциации файлов для «Открыть с
помощью» и двойного клика (этап 10, §15.5), автовоспроизведение при открытии
— настройка «Запускать сразу» в меню «⋯», по умолчанию включена (этап 11),
сохранение настроек в `%APPDATA%\vgpu-player\settings.cfg` (этап 12, §16).

Репозиторий: <https://github.com/m1Ck1-pypy/vgpu-player>, ветка `main`; релиз
0.2.0 — тег `v0.2.0` (первый — `v0.1.0`). Первый прогон CI по коду зелёный:
<https://github.com/m1Ck1-pypy/vgpu-player/actions/runs/44276878907>
(документационные коммиты CI не запускают — фильтры `paths`).

Тесты: **46** в ядре, **37** в UI, **3** в `player-app`, **8** doc-тестов (94
всего); `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings` и `cargo test --workspace` — чистые (этапы 11–12).

Сознательные упрощения (не баги):

- пороги синхронизации 10 мс вперёд / 50 мс опоздания подобраны замерами;
- точная перемотка на большом GOP занимает ~0.2–0.4 с, звук и видео на это
  время стоят;
- удержание стрелок шлёт `Seek` на каждое нажатие (троттлинга нет);
- состояния `Loading` нет: локальный файл открывается синхронно.

## 1. Назначение и пользователь

Учебный проект владельца: главная цель — **понять каждую строку и научиться
Rust**, а не выпустить готовый плеер. Продукт — плеер локальных видеофайлов со
звуком и плавной картинкой.

Функциональность 0.2.0: play/pause, перемотка, громкость, fullscreen,
открытие файла, меню и информации о файле. Этап 10 добавляет ассоциации
файлов: плеер появляется в «Открыть с помощью» и открывает файл двойным
кликом после выбора «Всегда использовать это приложение». Этап 11 —
автовоспроизведение: открытый файл сразу играет, поведение переключается
пунктом меню «Запускать сразу». Этап 12 — файл настроек `settings.cfg`.

Разработка — Windows; Linux проверяется компиляцией в CI. macOS не планируется.
Вне 0.2.0: плейлист, сохранение позиции, HTTP/HLS, 4K, аппаратное
декодирование, GitHub Releases.

## 2. Правила процесса

1. Один этап за раз; переход — по явному согласию владельца.
2. Сначала план (затронутые файлы), потом код; всё больше ~50 строк — после
   «ок». Git-операции — только по команде владельца.
3. Новые концепции Rust объяснять в 2–4 предложениях, ошибки компилятора — по
   причине.
4. Плейлист и сохранение позиции не делаем; иконки кнопок рисует агент.
5. Новая зависимость — только после согласования (причина + альтернатива).
6. Не переписывать чужой код «заодно»; о найденной проблеме сообщать.
7. Не угадывать API GPUI/FFmpeg/rodio — проверять по docs.rs или исходникам.
8. Ничего не ставить глобально (системные пакеты, `sudo`, конфиги вне
   репозитория) без разрешения.
9. «Готово» — критерии §10; `docs/CURRENT_TASK.md` обновляется после задачи.

## 3. Структура репозитория

```text
README.md                     # описание, сборка, запуск, установщики
Cargo.toml                    # workspace
justfile                      # portable / installers / примеры
.github/workflows/ci.yml      # fmt + clippy + test по изменениям кода
crates/
  player-core/                # движок: demux, decode, A/V-синхронизация; без GPUI
  player-gpui/                # библиотека UI: VideoPlayer, video_view, controls_view, settings
    examples/                 # minimal.rs, embed_in_app.rs
  player-app/                 # бинарник + иконка exe
  player-cli/                 # консольный запуск ядра и замеры
installer/                    # Inno Setup (.iss), WiX (.wxs), register-associations.cmd
assets/                       # icon.png, app-icon.ico; assets/test/ — медиа (не в git)
docs/                         # SPEC.md, CURRENT_TASK.md, upgrade_project.md
```

Принцип: `player-core` не знает об UI; общение — только команды и события.

### 3.1 Публичный API

`player-gpui` — библиотека без `main`; бинарник `player-app` — отдельный крейт.
Полная таблица экспортов, подключение и примеры — §14.

Основные методы `VideoPlayer`: `play`, `pause`, `toggle_playback`, `restart`,
`stop`, `seek`, `set_volume`, `toggle_mute`, `open`, `open_dialog`,
`open_dropped`, `retry`, `path`, `video_info`, `media_info`, `frame`, `stats`,
`stats_visible`, `set_stats_visible`, `toggle_stats`, `autoplay`,
`set_autoplay`, `toggle_autoplay`, `fullscreen`, `set_fullscreen`.

`open` и `create` по умолчанию сразу начинают воспроизведение
(`autoplay = true`); выключается `set_autoplay(false)`.

## 4. Архитектура

```text
demux thread ─► audio decode ─► rodio (ведущие аудио-часы)
             └► video decode ─► ограниченная очередь кадров ─► UI забирает кадр по часам
```

Обязательные правила:

- **Ведущие часы — аудио**: число реально отданных сэмплов (`AtomicU64`), не
  таймер. Для файла без звука — монотонный таймер; им же доигрывается хвост
  видео после конца звука.
- **Звук — два бэкенда за одним API**: Symphonia, при отсутствии кодека —
  FFmpeg. Оба отдают `AudioChunk` с `f32`; часы и rodio подмены не видят.
- **Видеодекодер за трейтом** `VideoDecoder` — backend меняется без правок
  остального.
- **Очередь кадров ограничена** (4 кадра); неограниченных каналов для кадров
  нет.
- **UI-поток не блокируется** на кадрах и IO — забирает только готовое.
- **`epoch`**: после seek всё со старым поколением отбрасывается.
- **Перерисовка** только на новый кадр или смену состояния.
- Все таймстемпы приводятся к `Duration` на границе ядра.

## 5. Контракт команд и событий

| Команды UI → ядро | События ядро → UI  |
| ----------------- | ------------------ |
| `Open(path)`      | `DurationKnown`    |
| `Play`            | `PositionChanged`  |
| `Pause`           | `NewFrame`*        |
| `Seek(Duration)`  | `Ended`            |
| `SetVolume(f32)`  | `Error`            |
| `Stop`            | `Stats(SyncStats)` |

\* `NewFrame` зарезервирован, фактически не используется: кадры идут отдельным
ограниченным каналом через `Player::try_recv_frame`, метаданные — через
`Player::video_info()`. `Buffering` удалён (локальные файлы не буферизуются).

Транспорт — `crossbeam-channel`. Каналы команд и событий не ограничены, канал
кадров — ограничен. `Stats` приходит раз в 500 мс во время воспроизведения
(fps, показанные и пропущенные кадры, отставание).

## 6. Стек

- GPUI через `gpui-fast`, в коде — `gpui = { package = "gpui-fast" }`;
  `gpui-fast`/`gpui-fast-platform` **0.1.16**. Не смешивать с upstream `gpui`
  в одном workspace; `player-gpui` реэкспортирует `gpui` и `gpui_platform`.
- `rodio` 0.23 — только вывод (`default-features = false`, `["playback"]`).
  Ловушка версии: бывший `Sink` теперь `Player`, устройство —
  `DeviceSinkBuilder::open_default_sink()`. `cpal` напрямую не подключаем.
- `symphonia` 0.5.5 с `features = ["all"]` — основной демукс и аудиодекодер.
- `ffmpeg-next` 9.1 (dev-библиотеки FFmpeg 9), в Cargo ключ `ffmpeg`
  (`package = "ffmpeg-next"`), код — `ffmpeg::...`. Нужен `libdav1d`: встроенный
  AV1 в FFmpeg умеет только аппаратно.
- `crossbeam-channel`, `thiserror` (библиотеки), `anyhow` (бинарники),
  `tracing` + `tracing-subscriber`, `clap`.
- Версии — в `[workspace.dependencies]`; в крейтах `dep.workspace = true`.
- MSRV — **1.98** (требование `gpui-fast`); edition 2024.

## 7. Ошибки и состояния UI

- `thiserror` в `player-core`/`player-gpui`, `anyhow` только в бинарниках.
  Библиотеки логируют через фасад `tracing`, подписчик (`RUST_LOG`) — в
  бинарниках; по умолчанию `warn`.
- Без `unwrap()`/`expect()` в рабочем коде (в тестах допустимо).
- `PlaybackState`: `Idle`, `Ready`, `Playing`, `Paused`, `Ended` (кнопка
  «Повторить» и `Replay`), `Failed` (текст ошибки и повторное открытие).
  `Loading` не введён.
- `player-cli` при ошибке — код `1` и сообщение в stderr.

## 8. Этапы и приёмка

Обозначения: ✅ готово · 🟡 в работе · ⬜ не начато. Приёмка — после ревью
владельца; подробности последней задачи — `docs/CURRENT_TASK.md`.

| #   | Этап                | Статус | Задача                                                                                             | Приёмка                                                                                                 |
| --- | ------------------- | ------ | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| 0   | Каркас              | ✅     | workspace, CI, тестовые файлы, пустое окно GPUI, `PlayerView` без логики                           | сборка и тесты, окно открывается и закрывается                                                          |
| 1   | Звук                | ✅     | Symphonia + rodio, пауза, громкость, команды ядра                                                  | автотест по числу сэмплов + проверка на слух                                                            |
| 2   | Аудио-часы          | ✅     | счётчик отданных сэмплов, `PositionChanged`, время в UI по событиям                                | время совпадает с реальным, на паузе стоит                                                              |
| 3   | Декодирование видео | ✅     | кадры BGRA, PNG-дампы, `VideoDecoder`/`VideoInfo`/`VideoFrame`                                     | PNG корректны, декодирование быстрее частоты видео                                                      |
| 4   | Кадр в окне         | ✅     | канал кадров, `video_view` на `canvas` + BGRA-текстура, letterbox                                  | цвета и пропорции, нет искажений при ресайзе                                                            |
| 5   | A/V-синхронизация   | ✅     | очередь, показ по аудио-часам, `FrameScheduler`, оверлей `Stats`                                   | писк совпадает со счётчиком, пропусков почти нет                                                        |
| 6   | Управление          | ✅     | `epoch`, полоса прогресса (клик/drag), громкость, mute, fullscreen, клавиши, автоскрытие           | повторные перемотки не ломают плеер, панель скрывается и возвращается                                   |
| 7   | Надёжность          | ✅     | `thiserror`, `tracing`, `EmptyFile`, состояния `Ended`/`Failed`, «Повторить»                       | битый, пустой, несуществующий файл: текст в окне без паники                                             |
| 8   | Производительность  | ✅     | пул BGRA-буферов, `recycle_frame`, замеры 10 минут 1080p                                           | 18000/18000 кадров, `dropped 0`, RSS и CPU не растут (§13)                                              |
| 9   | Полировка           | ✅     | меню «⋯», fullscreen-кнопка и подсказки громкости, превью, иконка exe, портативная папка, Inno/MSI | ручной чек-лист: меню, иконка, установка и удаление, пример `embed_in_app`                              |
| 10  | Ассоциации файлов   | ✅     | ProgID и `Applications\player-app.exe` в Inno/MSI, путь из argv, `.cmd` для портативной папки      | «Открыть с помощью» показывает VGPU Player; после «Всегда» работает двойной клик; удаление чистит ключи |
| 11  | Автовоспроизведение | ✅     | `VideoPlayer::autoplay` (по умолчанию `true`), `play` в `open`, пункт меню «Запускать сразу»       | двойной клик играет сразу; пункт меню выключает и включает это поведение                                |
| 12  | Файл настроек       | ✅     | `settings.rs` (`ключ = значение` без зависимостей), чтение до `open`, запись по меню и громкости   | значения переживают перезапуск; испорченный файл не мешает воспроизведению                              |

Этапы 13+ (HTTP/URL, HLS) в таблицу не входят: план и оценки —
`docs/upgrade_project.md`, строки появятся после решения владельца.

Правило этапа: ядро и UI вместе рабочие; владелец может объяснить, как событие
из ядра доходит до экрана.

## 9. Тестовые данные

Лежат в `assets/test/`, в git не попадают (`/assets/test/*`, кроме `README.md`).
Команды генерации — `assets/test/README.md`. Основные:

```bash
# A/V: счётчик кадров и писк раз в секунду
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 \
       -f lavfi -i "sine=frequency=1000:beep_factor=4" \
       -t 30 -c:v libx264 -pix_fmt yuv420p -c:a aac assets/test/sync_test.mp4
# то же в WebM (VP9 + Opus)
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i "sine=frequency=1000" \
       -t 30 -c:v libvpx-vp9 -c:a libopus assets/test/sync_test.webm
# короткий AV1-клип для libdav1d
ffmpeg -hide_banner -loglevel error -y -i assets/test/test_10min.webm \
       -t 2 -c copy assets/test/av1_test.webm
# 10 минут 1080p30 для замеров (звук — тишина)
ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=30 \
       -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=44100 \
       -t 600 -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p -g 60 \
       -c:a aac -b:a 128k assets/test/bench_10min_1080p.mp4
```

Нужны также: только звук, только видео, пустой, обрезанный, неподдерживаемый
кодек. Если файла нет и `ffmpeg` недоступен, тест пропускается с сообщением.

## 10. Стиль кода и «готово»

- Edition 2024. Без `unsafe`, кроме FFI к FFmpeg, с `// SAFETY:`.
- Публичный API ядра небольшой, с `///`; внутренности — `pub(crate)`.
- Каналы и атомики вместо `Mutex`; `Mutex` — только с объяснением.
- Комментарии и документация на русском, идентификаторы на английском.
- Тесты рядом с кодом (`#[cfg(test)]`), интеграционные — в `tests/`.

«Готово» для этапа:

1. Приёмка этапа из §8 пройдена.
2. `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace` проходят.
3. Владелец прочитал код и объяснил его (агент задал 2–3 контрольных вопроса).
4. `docs/CURRENT_TASK.md` обновлён; при смене поведения — этот файл.

## 11. Ключевые решения и ловушки

1. **Звук и хвост видео.** `AudioDecoder` — перечисление Symphonia → FFmpeg
   (Opus в WebM/MKV Symphonia 0.5.5 не умеет). Каналы и частота берутся из
   декодированного блока, а не из заголовка MP4. Выходной кадр swresample
   выделяется под каждый вход: переиспользование буфера первого Opus-пакета
   обрезало следующие. Когда звук кончился, видео ведёт таймер,
   перебазированный на последний кадр, иначе файл не доходит до `Ended`.
   `PositionChanged` отправляется и для файлов без звука.
2. **Кадры — отдельным каналом** (4 штуки, `try_recv_frame` отдаёт самый
   свежий), не событием: иначе кадры забивают канал событий.
3. **`epoch` внутренний** (`EngineMessage`, `TaggedFrame`): публичный `Command`
   не менялся. `try_recv_frame` отбрасывает кадры старых поколений, включая уже
   лежащие в канале.
4. **Точная перемотка.** FFmpeg ищет ключевой кадр, `VideoDecoder::seek` сам
   декодирует и выбрасывает кадры до цели без конвертации в BGRA. Звук на время
   поиска на паузе. Drag по полосе не шлёт поток `Seek`: предпросмотр локально,
   команда — один раз при отпускании.
5. **Пул BGRA-буферов.** Кадр 1080p — 8.3 МБ, ~250 МБ/с аллокаций. Пул живёт в
   потоке ядра, буферы возвращают потребители через `Player::recycle_frame`;
   `VideoDecoder::next_frame_with_buffer` — `pub(crate)`. Предел —
   `QUEUED_FRAMES + 2`, на `Open` пул чистится (другое разрешение). Если
   потребитель не вернул буфер, ядро выделит новый — корректность не страдает.
6. **Ловушка GPUI с хитбоксами.** `hitbox.is_hovered()` истинно для **всех**
   хитбоксов под курсором (`Window::hit_test` ставит
   `hover_hitbox_count = ids.len()`), а `on_mouse_down`/`up` фильтруются этим
   предикатом. Следствия: полноэкранную подложку под попапом делать нельзя
   (она всегда «наведена» и перехватывает клики); меню и панель управления
   гасят всплытие через `cx.stop_propagation()` на `on_mouse_down`, иначе
   «клик по кадру» ставил паузу и закрывал меню до `on_click`. Меню — обычный
   flex-ребёнок слоя `.absolute().size_full()` внутри области кадра.
7. **Панель поверх кадра.** `controls_panel()` —
   `.absolute().left_0().bottom_0().w_full()`, фон `rgba(0x11_11_11_cc)`
   (≈80 %). Видео всегда занимает всё окно и не перевёрстывается. Меню «⋯»
   отсчитывает отступ от `CONTROLS_HEIGHT = 60` (высота панели + зазор): при
   смене отступов/кнопок константу править.
8. **Превью при открытии.** Ядро декодирует видео только на `Play`, поэтому
   `player_core::preview_frame(&path)` один раз открывает файл, берёт первый
   кадр и закрывает декодер (~10–30 мс в UI-потоке). Ошибка не фатальна — чёрное
   поле. Буфер превью возвращается в пул как обычный кадр. Файл без видео —
   иконка ноты и «Аудио файл».
9. **Иконки.** Монохромные SVG встроены в `player-gpui/src/icons.rs` (не файлы
   в `assets/`): GPUI растеризует SVG в альфа-маску и красит цветом текста.
   Требования к рисунку: `viewBox="0 0 24 24"`, `width`/`height` = 24.
   Названия кнопок — только в `aria_label`.
10. **Fullscreen.** `VideoPlayer::fullscreen` — копия состояния окна, а не
    источник: бэкенд `gpui-fast` меняет режим асинхронно, демо-окно
    синхронизирует флаг из `Window::is_fullscreen()` в `render`. Потребитель
    `controls_view` должен делать то же. `Esc` сначала закрывает панели, затем
    выходит из fullscreen.
11. **Информация о файле.** `Player::media_info() -> MediaInfo`; контейнер и
    имена кодеков — отдельным проходом по заголовку FFmpeg (`probe_tracks`),
    разрешение/fps — из декодера, каналы — из аудиодекодера. Ошибка чтения
    заголовка не фатальна. Версии FFmpeg — `player_core::FfmpegVersions`
    (старший/средний/младший байт `LIBAV*_VERSION_INT`); UI про FFmpeg не знает.
12. **Настройки**: `stats_visible` и `autoplay` живут в `VideoPlayer`,
    `auto_hide_enabled` — в `PlayerWindow`; файл настроек читается в
    `PlayerWindow::new` **до** `open`, а пишется сразу после изменения (пункты
    меню, шаг громкости колесом, отпускание ползунка) — §16. Снимок уже
    записанных значений сравнивается перед записью. Громкость запоминается и без
    открытого файла: новое ядро получает её на `Open`, поэтому смена файла
    больше не сбрасывает уровень звука.
13. **Play без файла.** `play_action(state)`: `Idle` → `OpenFile`, остальные →
    `TogglePlayback`; решение — по `video.state()` в момент клика.
14. **Диалог выбора файла** — родной `App::prompt_for_paths`, `rfd` не нужен.
    Drag-and-drop — `ExternalPaths`/`on_drop`, открывается первый существующий
    файл. Приём drop не работает в процессе с правами администратора (UIPI):
    запускать без повышения, включая терминал/редактор.
15. **Ассоциации файлов.** Установщики пишут только HKCU и не трогают
    умолчания: назначить себя по умолчанию из установщика нельзя — с Windows 8
    двойной клик решает `UserChoice`, защищённый хэшем (пишет Windows после
    галочки «Всегда»). Поэтому есть два ProgID (`VGPUPlayer.Video`,
    `VGPUPlayer.Audio`), `OpenWithProgids` у расширений (куда Windows и
    записывает выбор) и `Applications\player-app.exe` с `SupportedTypes` (из-за
    него плеер виден в «Открыть с помощью»); подробности — §15.5.
16. **Путь из argv** (`player-app`) — первый аргумент без кавычек: так Windows
    запускает плеер по ассоциации. Аргумент не проверяется на существование —
    отдаётся ядру, и удалённый файл даёт штатный экран ошибки с «Повторить».

## 12. Инфраструктура

### 12.1 CI

`.github/workflows/ci.yml`: один job на `ubuntu-latest`; фильтры `paths` для
`push`/`pull_request` — `crates/**`, `assets/**`, `.github/**`, `Cargo.toml`,
`Cargo.lock`, `.cargo/**`. Ставятся системные библиотеки GPUI (wayland,
xkbcommon, fontconfig, freetype, vulkan, alsa, ssl, zstd), dev-пакеты FFmpeg и
`clang` + `libclang-dev` (bindgen). `gtk`/`webkit` не нужны. Шаги:
`cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`. Windows-раннера пока нет.

### 12.2 Тестовые ассеты

Генерируются локально (§9), в git не попадают. `image` — dev-dependency для
PNG-дампов.

### 12.3 Документация

`PLAN.md` и `RULES.md` — документы владельца; `docs/SPEC.md` — их сводка.
Отдельных заметок сверх `SPEC.md`/`CURRENT_TASK.md` не заводим.

### 12.4 FFmpeg на Windows и сборка

Свой vcpkg-рут в `.tools/vcpkg` (в `.gitignore`), FFmpeg 9.0.2 с `dav1d`,
триплет `x64-windows`; путь к `.pc` — `PKG_CONFIG_PATH` в `.cargo/config.toml`.

```bash
# один раз на машине
git clone --depth 1 https://github.com/microsoft/vcpkg.git .tools/vcpkg
.tools/vcpkg/bootstrap-vcpkg.bat -disableMetrics
.tools/vcpkg/vcpkg install "ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,dav1d]" --triplet x64-windows --recurse
# DLL рядом с бинарниками (после cargo clean — повторить)
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/deps/
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/examples/  # для примеров

cargo run -p player-app -- assets/test/sync_test.mp4
cargo run -p player-cli -- assets/test/audio_only.m4a
```

`--recurse` нужен, чтобы пересобрать уже установленный FFmpeg при добавлении
фичи. Сборка динамическая: FFmpeg и `dav1d` — DLL.

**Git Bash:** первым в `PATH` оказывается `/usr/bin/link.exe`, компоновка
падает с `extra operand …rcgu.o`. Обход:

```bash
RUSTFLAGS="-Clinker=rust-lld" cargo build
RUSTFLAGS="-Clinker=rust-lld" RUSTDOCFLAGS="-Clinker=rust-lld" cargo test --workspace
```

В PowerShell проблемы нет.

### 12.5 Версии и тег

Единственный источник версии — `[workspace.package] version` в корневом
`Cargo.toml`.

- `just set-version 0.2.0` — обновляет `Cargo.toml`, `MyAppVersion` в
  `installer/vgpu-player.iss`, `Version` в `installer/vgpu-player.wxs`, строку
  «Версия» и имена артефактов в `README.md`, примеры тега в
  `crates/player-gpui/README.md`, затем `cargo update -w` (меняет только
  версии воркспейс-членов в `Cargo.lock`). `docs/` правится вручную.
- `just tag` — читает версию из `Cargo.toml`, отказывает при уже существующем
  теге или грязном дереве, создаёт аннотированный `v<версия>`; публикация —
  `just gp <версия>`.
- `just gp 0.2.0` (или `v0.2.0`) — проверяет формат и наличие тега локально,
  затем `git push origin main <тег>` без интерактивного ввода пароля
  (`GIT_TERMINAL_PROMPT=0`).
- Имя MSI в `just installer-msi` вычисляется из версии; Inno берёт имя из
  `{#MyAppVersion}`.

### 12.6 Вне приёмки 0.2.0

4K и аппаратное декодирование (D3D11VA и др.) — отдельное расширение после
релиза.

## 13. Производительность

Всё измерено на `assets/test/bench_10min_1080p.mp4` (1080p30, 18000 кадров,
H.264), debug-сборка. Повторить: пропускная способность —
`target/debug/player-cli.exe assets/test/bench_10min_1080p.mp4 --decode-frames 18000`,
воспроизведение — `RUST_LOG=info cargo run -p player-app -- <файл>` (CPU/RSS — по
процессу).

### 13.1 Замеры «до/после» пула

| Метрика                     | До пула                  | После пула               |
| --------------------------- | ------------------------ | ------------------------ |
| Кадры                       | 18000/18000, `dropped 0` | 18000/18000, `dropped 0` |
| fps / конец файла           | 30.0, `ended` на 600 с   | 30.0, `ended` на 600 с   |
| CPU-время на 580-й секунде  | 78.8 с                   | 57.8 с (−26.6 %)         |
| CPU в процентах одного ядра | 13.6 %                   | 10.0 %                   |
| RSS                         | 47–55 МБ, колеблется     | 63 МБ, ровно             |
| `--decode-frames 18000`     | 288.6 fps ≈ 3.46 мс/кадр | 283.5 fps ≈ 3.53 мс/кадр |

Последняя строка — путь без пула, разница в пределах шума.

Эталон FFmpeg, один поток: только декод 0.29 мс/кадр, декод + BGRA 0.82 мс,
декод + BGRA + запись кадра 2.11 мс. Вывод: после пула видео-путь близок к
«полу» FFmpeg; остаток CPU — звук, цикл движка и события (~1 мс/кадр).

Debug и release дают одинаковые ~289 fps, значит узкое место — C-вызовы FFmpeg
и память, а не код на Rust. Поэтому сделан пул, а не микрооптимизации.

### 13.2 Что осталось из аллокаций и где запас

- `audio::chunk_from` (Symphonia) и `SamplesBuffer::new` — по копии блока
  (~7.7 КБ × 47 раз/с): шум, убирается своим `Source` (~30 строк).
- `FfmpegFrame::empty()`/`Packet::empty()` — внутри аллокатора FFmpeg, трогать
  бессмысленно.
- Строки UI (`format_clock`, `format!`) — микросекунды; можно кэшировать.
- **Настоящий запас:** копия кадра в GPU-текстуру (8.3 МБ × 30 раз/с, столько
  же пишет swscale) — лечится YUV-текстурой с конвертацией на GPU.
- Не делать: `Vec::with_capacity` вместо переиспользования буфера; замену
  `String` на `Vec<u8>`; `#[inline(always)]`/`target-cpu=native` в видео-пути —
  по цифрам не видно.

### 13.3 Приоритеты, если браться дальше

1. YUV-текстура без конвертации на CPU — самый крупный расход.
2. Свой `Source` в звуке вместо `SamplesBuffer` — ради чистоты.
3. Кэш форматированных строк в UI — ради чистоты.

Пока не понадобилось: 1080p30 тратит ≈10 % одного ядра.

## 14. Использование как библиотека

### 14.1 Что отдаёт библиотека

| Экспорт                                         | Назначение                                                                                                                     |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `VideoPlayer`                                   | сущность GPUI: состояние, команды, кадры; `create(path, cx)` или `cx.new` + `open` (оба стартуют воспроизведение — `autoplay`) |
| `video_view(&player, cx)`                       | кадр с letterbox, оверлей статистики и кнопка «Повторить»                                                                      |
| `controls_view(&player, cx)`                    | необязательная панель: кнопки-иконки и слайдеры                                                                                |
| `run_app(Option<PathBuf>)`                      | готовая оболочка окна для бинарника и примеров                                                                                 |
| `MediaInfo`, `VideoTrackInfo`, `AudioTrackInfo` | метаданные файла для «Информации о файле»                                                                                      |
| `PlaybackState`, `LaunchError`, `WINDOW_TITLE`  | состояния и вспомогательные типы                                                                                               |
| `gpui`, `gpui_platform` (реэкспорт)             | одна версия GPUI и платформенного слоя у библиотеки и потребителя                                                              |

Команды ядра (`player_core::Command`/`Event`) потребителю не нужны.

### 14.2 Как подключить

```toml
[dependencies]
gpui = { package = "gpui-fast", version = "0.1.16" }
player-gpui = { path = "../vgpu-player/crates/player-gpui" }
# после тега: { git = "https://github.com/m1Ck1-pypy/vgpu-player.git", tag = "v0.2.0", package = "player-gpui" }
```

Версия GPUI у потребителя и библиотеки должна совпадать (два GPUI в одном
процессе не соберутся) — импортировать через `player_gpui::gpui`.

### 14.3 Минимальное встраивание

```rust
use gpui::prelude::*;
use player_gpui::{VideoPlayer, controls_view, video_view};

struct Screen {
    player: gpui::Entity<VideoPlayer>,
}

impl gpui::Render for Screen {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::div()
            .flex()
            .flex_col()
            .size_full()
            .child(video_view(&self.player, cx).flex_1())
            .child(controls_view(&self.player, cx))
    }
}
```

Свой UI: `controls_view` не вызывается, всё делается методами `VideoPlayer`
(список — §3.1) плюс `state`, `position`, `duration`, `video_info`, `stats`.

### 14.4 Что нужно на машине потребителя

- Windows: рядом с exe — DLL FFmpeg: `avcodec-63`, `avdevice-63`,
  `avfilter-12`, `avformat-63`, `avutil-61`, `swresample-7`, `swscale-10`,
  `dav1d`.
- Звуковое устройство по умолчанию: без него `open` вернёт ошибку.
- Drop не работает в процессе с правами администратора (UIPI).

### 14.5 Примеры

| Файл                                          | Что показывает                                              | Запуск                                                      |
| --------------------------------------------- | ----------------------------------------------------------- | ----------------------------------------------------------- |
| `crates/player-gpui/examples/embed_in_app.rs` | встраивание `video_view` и свои кнопки, без `controls_view` | `cargo run -p player-gpui --example embed_in_app -- <файл>` |
| `crates/player-gpui/examples/minimal.rs`      | минимум кода: `run_app(Some(path))`                         | `cargo run -p player-gpui --example minimal -- <файл>`      |

Только публичный API библиотеки, без `player-core`. `just example-minimal` /
`just example-embed` копируют DLL в `target/debug/examples/`. Оба примера
входят в `clippy --all-targets`; код из README — в doc-тестах.

## 15. Упаковка и установщики

### 15.1 Портативная папка

`just portable` (или `cargo build --release -p player-app`) собирает:

```text
dist/vgpu-player/
  player-app.exe
  register-associations.cmd
  avcodec-63.dll  avdevice-63.dll  avfilter-12.dll  avformat-63.dll
  avutil-61.dll  swresample-7.dll  swscale-10.dll  dav1d.dll
```

`avdevice` импортирует exe, `avfilter` — `avdevice` (проверено `objdump -p`).
`pkgconf-8.dll` — инструмент сборки, в поставку не входит. `dist/` — артефакт,
в `.gitignore`. Release-exe — Windows GUI без консоли; debug остаётся CUI ради
логов. Проверять запуск двойным кликом из Проворника: только так видно, что DLL
находятся. `register-associations.cmd` копирует `just portable` и в установщики
он не попадает — см. §15.5.

### 15.2 Установщики

Два, оба per-user (без UAC), оба берут файлы из `dist/vgpu-player/` и несут
иконку. Пересборка — `just installers`.

**Inno Setup 7** — `installer/vgpu-player.iss` →
`dist/vgpu-player-0.2.0-setup.exe`; установка в
`%LOCALAPPDATA%\Programs\vgpu-player`, `PrivilegesRequired=lowest`, ярлыки и
деинсталлятор. Сборка:
`"C:\Program Files\Inno Setup 7\ISCC.exe" installer/vgpu-player.iss` (в `PATH`
компилятора нет). Печатает `Non-commercial use only`: для коммерческого
распространения нужна лицензия или NSIS.

**MSI (WiX 3.14.1)** — `installer/vgpu-player.wxs` + `installer/AppFiles.wxs` →
`dist/vgpu-player-0.2.0.msi`; `InstallScope="perUser"`, установка в
`%LOCALAPPDATA%\vgpu-player` (без промежуточного `Programs` — иначе ICE64),
HKCU-keypath у компонентов (ICE38), ICE91 — ожидаемое предупреждение. WiX
скачивается в `.tools/wix` (не в git); `AppFiles.wxs` поддерживается вручную —
список DLL обновлять при изменении набора.

При смене версии правятся `MyAppVersion` (Inno) и `Version` (WiX). Отображаемое
имя — **«VGPU Player»** (окно, «О программе», список программ, ярлыки); имена
каталогов, файлов установщиков, exe и крейтов остаются `vgpu-player` — это
идентификаторы обновления и удаления. NSIS можно взять вместо Inno (свободен
для коммерческого использования), скрипта в репозитории нет.

Оба установщика пишут ассоциации файлов (§15.5): Inno — секция `[Registry]`,
MSI — компонент `FileAssociations`. Список расширений задублирован в двух
файлах — при правке менять оба. `register-associations.cmd` из портативной
папки Inno не ставит (`Excludes`), MSI его не знает.

### 15.3 Иконка приложения

`assets/icon.png` (1023×1023, RGBA) → `assets/app-icon.ico`
(16/24/32/48/64/128/256, Pillow). Вшивается в exe через
`crates/player-app/build.rs` (`embed-resource`) и `app.rc`:
`1 ICON "../../assets/app-icon.ico"`. ID именно числовой: `gpui-fast-windows`
грузит иконку через `LoadImageW(module, MAKEINTRESOURCE(1), ...)`, имя
`IDI_ICON1` не находит. `WindowOptions` иконку не задаёт — её берут из exe
заголовок и панель задач. При замене рисунка пересобрать `.ico` и release.

### 15.4 GitHub Releases (отложено)

Цель: по тегу `v*` собирать Windows (portable zip + Inno + MSI) и Linux
(tar.gz/AppImage). Предлагаемая схема — отдельный `.github/workflows/release.yml`
(текущий `ci.yml` не трогаем): триггер `push: tags: ['v*']` +
`workflow_dispatch`; Windows — vcpkg FFmpeg с кэшем `.tools/vcpkg/installed`,
release-сборка, zip/Inno/MSI; Linux — apt-пакеты как в CI, tar.gz с бинарником,
иконкой и `.desktop`; отдельный job с `permissions: contents: write` и
`gh release create`, артефакты + `SHA256SUMS`.

Риски: Inno 7 в CI (Chocolatey даёт 6.x — нужен пин с jrsoftware или MSI+zip);
vcpkg без кэша 15–25 мин; Linux-сборка не самодостаточна (системные FFmpeg,
Vulkan, Wayland/X11, glibc) и не проверена в окне; неподписанные артефакты
ругает SmartScreen; версию надо передавать параметром и сверять с `Cargo.toml`;
macOS не планируется.

Оценка: Windows 1–2 часа + 2–3 прогона, Linux tar.gz ~1 час, AppImage ~полдня,
release-job ~30 минут. Открытые вопросы: формат Linux-сборки; оставлять ли Inno
в CI; нужны ли SHA256 и что считать версией при ручном запуске.

### 15.5 Ассоциации файлов

Цель: плеер виден в «Открыть с помощью» для видео и аудио, а после галочки
«Всегда использовать это приложение» обычный двойной клик в Проводнике
запускает его с этим файлом.

Всё пишется в HKCU (`Software\Classes`), per-user, без UAC:

| Ключ                                         | Назначение                                                                                            |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `VGPUPlayer.Video`, `VGPUPlayer.Audio`       | ProgID: имя типа, `DefaultIcon` (иконка exe), `shell\open\command` = `"…\player-app.exe" "%1"`        |
| `Applications\player-app.exe`                | `FriendlyAppName`, `DefaultIcon`, `shell\open\command` — из-за него плеер виден в «Открыть с помощью» |
| `Applications\player-app.exe\SupportedTypes` | по значению на расширение — какие типы предлагать                                                     |
| `.<ext>\OpenWithProgids`                     | `VGPUPlayer.Video` / `.Audio` — ProgID, который Windows запишет в `UserChoice` по галочке «Всегда»    |

Расширения (29): видео `.mp4 .m4v .mov .mkv .webm .ogv .avi .ts .m2ts .mts .mpg
.mpeg .flv .wmv .3gp .3g2`; аудио `.mp3 .m4a .m4b .mp2 .aac .flac .wav .ogg
.oga .opus .wma .ac3 .dts`. Список задублирован в `.iss` и `.wxs` — менять
оба.

Установщики: Inno — секция `[Registry]`; MSI — компонент `FileAssociations` с
HKCU-keypath (ICE38/ICE64). Удаление чистит только своё: `uninsdeletekey` на
своих ключах и `uninsdeletevalue uninsdeletekeyifempty` на значениях в чужих
`OpenWithProgids`; в MSI — `RemoveRegistryKey Action="removeOnUninstall"`.

Портативная папка: `installer/register-associations.cmd` (его копирует
`just portable`). Двойной клик — регистрация; снятие — тот же файл с
аргументом `remove`. Обычный `.reg` не годится: он не умеет подставить свой
каталог, а путь в `shell\open\command` должен быть абсолютным.

Умолчание по умолчанию не назначается сознательно: с Windows 8 `UserChoice`
защищён хэшем, прямая запись игнорируется или сбрасывается Проводником —
поведение вышло бы непредсказуемым (§11, п. 15). Проверка вручную: ПКМ по
файлу → «Открыть с помощью» → «Выбрать другое приложение» → VGPU Player →
«Всегда» → двойной клик.

Установленная и портативная копии используют одни и те же ключи (они привязаны
к имени `player-app.exe`): чья регистрация выполнена последней, та и указана
в команде открытия. Одновременно ставить обе копии незачем.

Если после установки плеера всё ещё нет в списке, значит Проводник показал
кеш ассоциаций: помогает перезапустить `explorer.exe` или перелогиниться.

## 16. Настройки

Файл — `%APPDATA%\vgpu-player\settings.cfg` (Linux-сборка:
`XDG_CONFIG_HOME` или `~/.config`), формат `ключ = значение`:

```text
# VGPU Player settings
autoplay = true
stats_visible = false
auto_hide = true
volume = 0.8
```

Три флага — те же пункты меню «⋯», `volume` — громкость `0…1`. Неизвестные
ключи, строки без `=` и значения неверного типа пропускаются, `volume`
обрезается в `0…1`. Отсутствие или порча файла дают умолчания: autoplay вкл.,
оверлей выкл., автоскрытие вкл., громкость `1.0`. Новых зависимостей нет: разбор
— `Settings::parse` в `crates/player-gpui/src/settings.rs`, тесты рядом.

Запись: сразу после переключений меню, сразу после шага громкости колесом и по
отпусканию ползунка громкости. `PlayerWindow` держит снимок того, что уже лежит
в файле, и пишет только при отличии: клики по окну и повторные переключения
файл не трогают. Ошибка записи только логируется — плеер продолжает работать на
текущих значениях.

Порядок запуска: настройки читаются и применяются **до** `open`, иначе
`autoplay = false` не успел бы подействовать. Громкость передаётся новому ядру
на `Open` — уровень больше не сбрасывается при смене файла и учитывает `mute`.

Портативная папка пишет в тот же файл: настройки принадлежат пользователю, а не
каталогу с exe, и чтение из exe-каталога могло бы быть недоступно. Для библиотеки
это поведение `run_app`; чужое приложение с `VideoPlayer` решает само (см. §14).
Удаление плеера настройки не трогает — файл остаётся у пользователя.
