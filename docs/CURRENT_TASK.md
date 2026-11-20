# CURRENT_TASK — этап 1: источники и воспроизведение по URL

**Этап:** 1 — `Source` + `MediaReader` (✅ код готов, ждёт приёмки) · далее
2 (асинхронное открытие), 3 (HTTP VOD), 4 (кэш), 5 (HLS) ·
**обновлён:** 2026-11-20.

Требования — `docs/SPEC.md`, разбор mpv — `docs/URL_LOGIC_MPV.md`.

## Сейчас

- Этап 1 реализован: локальные файлы идут через `Source` → `FileReader` →
  `StreamIo` в FFmpeg; Symphonia осталась на `File`.
- Замер 600 кадров `assets/test/test_10min.webm` (2560×1440@60):
  до — 4.52/4.53/4.57 с, после — 4.40/4.43/4.46/4.48 с. Регрессии нет.
- `cargo test --workspace`: 77 тестов `player-core`, 40 `player-gpui`,
  3 `player-app`, 8 doc-тестов — зелёные. `clippy -D warnings` и
  `fmt --check` проходят (линкер — `rust-lld`).
- URL ещё не читается: `Source::Url` даёт `UnsupportedSource`, `.m3u8` —
  `UnsupportedHls`; HTTP-читатель — этап 3.
- Прошлый цикл 0–13 принят, тег `v0.2.1`, локальные файлы работают.
- Git-операции — только по команде владельца.

## Задача

Добавить открытие видео по URL, не ломая локальные файлы. Взять у mpv идею
потокового слоя: `Source` → `MediaReader` → FFmpeg, без «URL вместо пути».
Первый объём — HTTP(S) VOD с перемоткой через `Range`; HLS — этап 5.

## Решения

1. VOD сейчас; HLS — этап 5. Контракт готовим сразу: `is_hls`, `live`,
   `seekable`, `duration = None` для live.
2. Свой потоковый слой: один код чтения для файла и сети, свои таймауты,
   отмена, повторы, перемотка.
3. FFmpeg получает `StreamIo` через `input_from_stream_with_interrupt`
   (проверено по `ffmpeg-next 9.1.0`).
4. Встроенные протоколы FFmpeg не используем: в vcpkg-сборке нет TLS,
   `https` через FFmpeg не работает.
5. HTTP: `ureq` 3.5.x, `default-features = false`, `features = ["native-tls"]`;
   явно `TlsProvider::NativeTls` и `RootCerts::PlatformVerifier` (иначе
   останутся WebPki-корни). `tokio`/`axum` не нужны.
6. Звук по URL — FFmpeg. Symphonia требует `Read + Seek + Send + Sync`,
   остаётся основным путём для локальных файлов.
7. Открытие асинхронное: `open` возвращается сразу, результат — `Ready`/`Error`,
   в UI `Loading`.
8. «Открыть…» — панель «Файл» / «URL»; URL — поле ввода (`Ctrl+V`, `Enter`,
   `Esc`). Пока поле открыто, горячие клавиши плеера не работают.
9. `tiny_http` — только dev-dependency.
10. Общий кэш — после замера трафика (этап 4).
11. Лимиты: окно 2 МиБ, кэш 64 МиБ, DNS/connect 5/5 с, чтение 2 с, простой
    15 с, отмена ≤ 2–3 с, 5 повторов, 10 редиректов.
12. Очередь звука для сети — 32 блока: один поток ядра не должен терять звук
    из-за блокирующего чтения видео.
13. Для URL в ошибках и логах — `log_name()` без query/fragment.
14. `file://` поддерживаем; слайдер без перемотки выключаем.
15. Версия после приёмки — `0.3.0`.
16. Ошибки несут `Source` (`target`), а не `PathBuf`: тексты для файлов
    дословно прежние, для URL подставляется `log_name()`.
17. До HTTP-читателя `Source::Url` отклоняется `UnsupportedSource`; `.m3u8`
    проверяется до открытия и даёт `UnsupportedHls`.
18. FFmpeg-декодеры открывают `Box<dyn MediaReader>` через
    `StreamIo::from_read_seek`; флаг отмены (`Arc<AtomicBool>`) взводится в
    `Drop`, его опрашивает interrupt-колбэк.
19. `player-gpui` и `player-app` на этапе 1 не менялись: `Player::open`
    принимает `impl Into<Source>`, старые вызовы с `PathBuf` работают.

## План

### Этап 1 — `Source` + `MediaReader` (✅)

- [x] `source.rs`: `Source`, `SourceError`, `parse`, `from_os_string`,
      `display`, `log_name`, `file_name`, `is_network`, `is_hls`; тесты
      `http(s)://`, `file://` (диск/UNC/`%20`), Windows-путь, не-Unicode.
- [x] `reader.rs`: `MediaReader`, `FileReader`; тесты read/seek/len.
- [x] `VideoDecoder::open(&Path)` + `open_source(&Source)`; `FfmpegDecoder`
      через `input_from_stream`; Symphonia — на `File`.
- [x] `Command::Open(Source)`; ошибки несут `Source`; тексты файлов прежние.
- [x] `probe_tracks` только для файлов; `container_name()`/`codec_name()`.
- [x] `.m3u8` → `UnsupportedHls` до `Media::open`/`open_source`.
- [x] `player-cli` принимает `Source`.
- [x] Замер 10-минутного 1080p: регрессия ≤ 5–10 %, иначе для файлов
      остаётся прямой путь.

Приёмка: локальный файл играет и перематывается как раньше; тесты зелёные;
замер не хуже.

### Этап 2 — асинхронное открытие (⬜, 3–4 дня)

- [ ] `Player::open` только запускает поток и ставит `Open`.
- [ ] `Media` открывается в цикле ядра; ошибки — `Event::Error`, поток жив.
- [ ] `Event::Ready(Box<MediaReady>)`; `DurationKnown` удаляется.
- [ ] `PlaybackState::Loading`; `video_view` (приоритет над спектром) и
      `controls_view`; `play`/`seek` в `Loading` игнорируются.
- [ ] Одно ядро на плеер; `retry`, `EngineGone`, отмена `Stop`/`Open`/`Drop`.
- [ ] `preview_frame` только для файлов.
- [ ] `run_app_source`; README и примеры (doc-тесты).
- [ ] Тесты: медленный читатель, `open` < 50 мс; ошибки — событием.

Приёмка: окно не замирает; локальные файлы без регрессий; ошибки понятны.

### Этап 3 — HTTP(S) VOD (⬜, 6–9 дней)

- [ ] `ureq` + `native-tls` с явными провайдером и корнями.
- [ ] `HttpReader`: `Range`, 200/206, редиректы, `Content-Length`/chunked,
      `Content-Type`, read-ahead 2 МиБ, слот ошибки.
- [ ] Таймауты и отмена по §2.3 SPEC.
- [ ] `Source::Url` в `Media::open`; звук через FFmpeg; без `probe_tracks`.
- [ ] `MediaReady { seekable, duration, … }`, `NotSeekable`.
- [ ] `Buffering` для аудио и видео без звука.
- [ ] Очередь звука 32; тест «медленное видео не проваливает звук».
- [ ] Ошибки 404/403/416/DNS/TLS/`text/html` с URL.
- [ ] Панель «Файл» / «URL», поле ввода, `Ctrl+V`.
- [ ] `player-app`/`player-cli` разбирают URL; панель информации — URL.
- [ ] Тесты на `127.0.0.1`.

Приёмка: MP4/MKV/WebM/MP3 по `http` и `https` играют; `Range` перематывает;
без `Range` играет без перемотки; обрыв продолжается; 404 понятен; закрытие
окна не ждёт сеть.

### Этап 4 — трафик и общий кэш (⬜, 2–4 дня)

- [ ] Замерить трафик двух читателей (MP4, MKV).
- [ ] При дублировании — общий кэш 64 МиБ, блок 256 КиБ, два курсора.
- [ ] Если кэш не оправдался — рассмотреть один FFmpeg-демуксер.
- [ ] Тест на отсутствие роста памяти.

### Этап 5 — HLS (⬜, по решению)

- [ ] До решения `.m3u8` → `UnsupportedHls`.
- [ ] A: отдать плейлист FFmpeg; B: свой HLS-клиент (сегменты, ABR).
- [ ] Подготовка уже в 1–3: `is_hls`, `live`, `seekable`.

## Файлы

- `player-core`: новые `source.rs`, `reader.rs`; правки `command.rs`,
  `error.rs`, `player.rs`, `video.rs`, `audio.rs`, `lib.rs`.
- `player-cli`: `main.rs` — аргумент разбирается как `Source`.
- `player-gpui`, `player-app`: без изменений на этапе 1.
- `Cargo.toml`: новых зависимостей нет; `ureq` + `native-tls` — этап 3;
  `tiny_http` — dev на этапе 3.
- `README.md`: запуск с URL и `file://` — этап 2–3.

## Тесты

- `Source`: URL, `file://` (диск/UNC/`%20`), Windows-путь, не-Unicode,
  мусор, `user:pass@`, `log_name`, `is_hls`.
- `FileReader`: read/seek Start/Current/End, `len`, `hint_name`, отсутствие
  файла.
- `VideoDecoder::open_source(&Source::File)` даёт те же кадры/PTS, что
  `open(&Path)`; `container_name()`/`codec_name()` заполнены.
- `.m3u8` → `UnsupportedHls` до открытия файла.
- `Player::open` на локальных MP4/WebM/Opus — без регрессий.
- Замер: 600 кадров `test_10min.webm` до/после перехода на `StreamIo`.
- Сеть в тестах будет только `127.0.0.1:0` (этап 3).

## Открытые вопросы

1. Поле URL: минимальное (клавиши + `Ctrl+V`) или сразу мышь/IME?
   Предложение: минимальное, остальное — по надобности.
2. Индикатор LIVE: только в этапе 5 (контракт `live` уже есть).

## Следующая сессия

1. Владелец читает этап 1 и подтверждает приёмку.
2. Этап 2: `Player::open` запускает поток, `Media` открывается в цикле ядра,
   `Event::Ready`, `Loading`, отмена `Stop`/`Open`/`Drop`.
3. После приёмки — этап 3 (HTTP-читатель).
