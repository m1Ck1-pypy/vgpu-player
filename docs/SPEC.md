# SPEC — vgpu-player: источники и HTTP(S)-воспроизведение

План нового цикла: **этапы 1–5**. Прошлый цикл 0–13 (локальные файлы) принят,
его история — `git log` и теги `v0.1.0`…`v0.2.1`. Текущее состояние —
`docs/CURRENT_TASK.md`, разбор mpv — `docs/URL_LOGIC_MPV.md`.

## 1. Рамки

Делаем: HTTP(S) VOD по прямой ссылке (MP4/MKV/WebM/MP3), `file://`,
перемотку через HTTP `Range`, асинхронное открытие, `Loading`/`Buffering`,
понятные ошибки, подготовку контракта под HLS.

Не делаем: DRM, YouTube и подписанные страницы, DASH, live/ABR, субтитры,
офлайн-кэш, авторизацию. HLS — только этап 5; до него `.m3u8` даёт
`UnsupportedHls`.

## 2. Архитектура

```text
UI ── Command::Open(Source) ──► поток ядра
                                   │ Source → ReaderFactory
                                   │   FileReader / HttpReader
                                   ▼
                     FFmpeg: input_from_stream_with_interrupt
                                   │
                     ┌─────────────┴─────────────┐
              аудиодекодер                 видеодекодер
                     │                           │
              rodio (аудио-часы)         очередь кадров ──► UI
```

### 2.1 `Source`

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    File(PathBuf),
    Url(String), // http:// или https://
}
```

- `Source::parse(&str) -> Result<Source, SourceError>`: `http(s)://` → `Url`;
  `file:///C:/…`, `file:///home/…`, `file://server/share` → `File`;
  остальное — путь.
- `Source::from_os_string(OsString)` — для CLI и drag-and-drop; не-Unicode
  путь не портится. URL распознаётся по ASCII-префиксу схемы.
- `file://`: percent-декодирование (`%20`), на Windows `/C:/…` → `C:/…`,
  `file://server/share` → UNC.
- `display()` — для UI; `log_name()` — для логов и ошибок (query/fragment
  заменяются на `?…`, чтобы не светить токены).
- `file_name()` — последний сегмент; расширение используется как `Hint` FFmpeg.
- `is_network()`, `is_hls()` (`.m3u8`, без учёта регистра, и у файлов).
- `From<PathBuf>`, `From<&Path>`, `From<&PathBuf>`. URL с пробелами,
  управляющими символами или `user:pass@` отклоняется.

`SourceError` — публичный `thiserror`-тип с текстом для CLI/UI.

### 2.2 `MediaReader`

```rust
pub(crate) trait MediaReader: Read + Seek + Send {
    fn len(&self) -> Option<u64>;
    fn is_seekable(&self) -> bool;
    fn hint_name(&self) -> Option<&str>;
}
```

- `FileReader` — `std::fs::File`, всегда seekable.
- `HttpReader` — HTTP/1.1 с `Range` (см. 2.3).
- `VideoDecoder::open(&Path)` сохраняется; для обоих источников добавляется
  `VideoDecoder::open_source(&Source)`.

Требования к `Read`/`Seek` для `StreamIo`:

- `Read` блокирующий; `WouldBlock`/`TimedOut` наружу не отдаются.
- При отмене — `io::ErrorKind::Interrupted`; `StreamIo` переиспрашивает его
  и завершает работу по interrupt-колбэку.
- `Ok(0)` — EOF. `SeekFrom::End` используется для `avio_size`; при
  неизвестной длине возвращается ошибка.
- Позиция меняется только после успешного ответа сервера.

### 2.3 `HttpReader`

Один читатель на дорожку (звук и видео независимы).

**Запрос:** `GET`, `Range: bytes=<pos>-`, `Accept-Encoding: identity`,
`User-Agent: vgpu-player/<version>`, `Accept: */*`.

- 206 — `Range` поддержан, длина из `Content-Range`.
- 200 — `Range` не поддержан: читаем последовательно, `seekable = false`;
  длина — из `Content-Length`, если она есть (chunked — неизвестна).
- 3xx — только http/https, не больше 10.
- 4xx — сразу ошибка; 5xx и обрыв — повтор.
- `Content-Type` сохраняется; `text/*` при провале FFmpeg даёт `NotMedia`.
- `Content-Encoding: gzip/br` — понятная ошибка (мы просили `identity`).
- Ответ 206 сверяется с запрошенным диапазоном.

**Чтение:** окно `READ_AHEAD = 2 МиБ`; последовательные `read` идут из него,
исчерпание — следующий `Range`. `Seek` внутри окна бесплатен, иначе новый
запрос. Без `Range` произвольный `Seek` — `Unsupported`.

**Prefetch:** у источника с `Range` окна качает фоновый поток: следующее окно
заказывается сразу после выдачи текущего, поэтому декодер ждёт сеть только
когда не успел (медленный канал). Перемотка меняет поколение запросов — старые
ответы отбрасываются, текущая загрузка прерывается. Счётчик «медленных
чтений» считает именно ожидание в декодере, а не время скачивания.

**Надёжность:**

- DNS/connect 5 с, одно чтение 2 с, простой без данных 15 с, 5 повторов
  с backoff 0.2…3.2 с.
- Отмена: `Arc<AtomicBool>`; `Stop`/`Open`/`Drop` прерывают чтение за
  ≤ 2–3 с. DNS/connect не прерываются, но ограничены 5 с.
- `Pause`/`Seek`/`SetVolume` во время чтения применяются после его возврата
  (до 2 с) — известное ограничение одного потока ядра.
- Причина ошибки читателя хранится в `Arc<Mutex<Option<HttpFailure>>>`;
  декодер заменяет безликий `invalid data` FFmpeg на понятный текст.
  `Mutex` — только для этого слота.
- Память: 2 МиБ на читателя (4 МиБ на две дороги) + общий кэш после этапа 4.

**TLS:** в текущей vcpkg-сборке FFmpeg нет TLS-бэкенда, поэтому `https`
через него не работает. Нужен свой клиент (см. §8).

### 2.4 Декодеры

- **Видео** — `StreamIo::from_read_seek` +
  `input_from_stream_with_interrupt`; перемотка через
  `Input::seek` → `MediaReader::seek`.
- **Звук локального файла** — Symphonia, при отказе FFmpeg.
- **Звук по URL** — только FFmpeg: `Symphonia::MediaSource` требует
  `Read + Seek + Send + Sync`, а сетевой читатель таким не является.
- **`probe_tracks`** — только для файлов. Для URL контейнер и кодеки берутся
  из открытых декодеров (`container_name()`, `codec_name()`), поэтому на URL
  два соединения, а не три.
- **`.m3u8`** — `UnsupportedHls` до `Media::open` и
  `VideoDecoder::open_source`.

### 2.5 Что нельзя ломать

- Ведущие часы — аудио; для видео без звука — монотонный таймер.
- Очереди ограничены. Для сети очередь звука — 32 блока (~0,75 с), иначе
  блокирующее чтение видео опустошает звук; при провалах — prefetch-поток
  или раздельные декодеры.
- `epoch` отбрасывает кадры до перемотки/открытия.
- Пул BGRA-буферов и `recycle_frame`.
- UI не блокируется на IO.

## 3. Контракт

### 3.1 Команды и события

| Команды UI → ядро | События ядро → UI           |
| ----------------- | --------------------------- |
| `Open(Source)`    | `Ready(Box<MediaReady>)`    |
| `Play`            | `PositionChanged(Duration)` |
| `Pause`           | `Buffering(bool)`           |
| `Seek(Duration)`  | `Stats(SyncStats)`          |
| `SetVolume(f32)`  | `Spectrum(SpectrumBars)`    |
| `Stop`            | `Ended`                     |
|                   | `Error(String)`             |

`DurationKnown` удаляется: длительность приходит в `Ready`.

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct MediaReady {
    pub media_info: MediaInfo,
    pub video_info: Option<VideoInfo>,
    pub duration: Option<Duration>,
    pub seekable: bool,
    pub live: bool, // пока false; HLS — этап 5
}
```

### 3.2 Открытие

- `Player::open(source) -> Result<Player, PlayerError>` только запускает
  поток и ставит `Open`; ошибка `Result` — только `StartEngine`.
- Результат открытия — `Ready` или `Error`; ошибка не убивает поток.
- `Player::video_info()`/`media_info()` удаляются.
- `Stop`, новый `Open` и `Drop` взводят флаг отмены до отправки команды.
- Ошибка звукового устройства приходит `Error`, поток живёт дальше.

### 3.3 `VideoPlayer`

- `PlaybackState::Loading`; `play`/`toggle_playback`/`restart`/`seek` в нём
  игнорируются.
- `open` ставит `Loading`, сбрасывает кадр и метаданные; превью первого кадра
  только для `Source::File` (`.m3u8` не превьюится).
- По `Ready`: `seekable` сохраняется, состояние `Ready`; `autoplay` → `Play`.
- По `Error`: `Failed`, кадр и текстура сбрасываются, иначе превью скроет
  текст ошибки.
- `seek` при `!seekable` или `duration = None` команду не отправляет.
- `retry()` повторяет тот же `Source`; при `EngineGone` создаёт новое ядро.
- Геттеры: `source()`, `seekable()`, `buffering()`; `path()` — только для файла.

### 3.4 CLI и `run_app`

- `run_app(Option<PathBuf>)` сохраняется; для URL —
  `run_app_source(Option<Source>)`.
- `VideoPlayer::{create,open}` принимают `impl Into<Source>`.
- `player-app`: `Source::from_os_string`, кавычки снимаются, `https://…`
  не считается путём.
- `player-cli`: первый аргумент — `Source`; метаданные из `Ready`, без
  второго декодера; `--decode-frames` — через `open_source`.

## 4. Ошибки

Тексты для файлов не меняются; для URL используется `Source::log_name()`.

| Ситуация         | Вариант             | Текст                                       |
| ---------------- | ------------------- | ------------------------------------------- |
| нет файла / прав | `OpenFile`          | `не удалось открыть <path>: <io>`           |
| пустой файл      | `EmptyFile`         | `файл <path> пуст`                          |
| пустой ответ URL | `EmptySource`       | `по ссылке нет данных: <url>`               |
| 404/403/5xx      | `HttpStatus`        | `сервер вернул 404 для <url>`               |
| таймаут/DNS/TLS  | `Network`           | `сеть: <причина>`                           |
| не медиафайл     | `NotMedia`          | `по ссылке не медиафайл: <url>`             |
| нет `Range`      | `NotSeekable`       | `источник не поддерживает перемотку: <url>` |
| `.m3u8`          | `UnsupportedHls`    | `HLS пока не поддерживается: <url>`         |
| URL до этапа 3   | `UnsupportedSource` | `источник пока не поддерживается: <url>`    |
| кривая строка    | `SourceError`       | `не похоже на URL или путь: …`              |

## 5. UI

- **`Loading`** — «Загрузка…»; кнопки Play/паузы и слайдер неактивны.
  Спектр не рисуется, пока нет `Ready`.
- **`Buffering`** — плашка «Буферизация…»; кадр остаётся, управление доступно.
- **«Открыть…»** в меню — панель с «Файл» / «URL». «Файл» — родной диалог,
  «URL» — поле ввода: символы, `Backspace`/`Delete`, `←`/`→`/`Home`/`End`,
  `Ctrl+V`, `Enter` — открыть, `Esc` — закрыть. Пока поле открыто, горячие
  клавиши плеера не работают.
- **Кнопка Play без файла** по-прежнему открывает родной диалог файла;
  меню «Открыть…» — путь к URL.
- **Информация о файле**: для URL — ссылка, без размера; метаданные из `Ready`.
- **Оверлей статистики**: `fps · drop · lag`; для URL — вторая строка
  `net: req · MB · slow N (s) · retries M` и `restarts K`, если они были.
- **Слайдер**: при `!seekable` или `duration = None` серый и не перематывает.
- **Ошибка**: текст в кадре + «Повторить».
- **Drag-and-drop** — только файлы.

## 6. Этапы

| #   | Этап                               | Оценка     |
| --- | ---------------------------------- | ---------- |
| 1   | `Source` + `MediaReader`, локально | 3–4 дня    |
| 2   | Асинхронное открытие, `Loading`    | 3–4 дня    |
| 3   | HTTP(S) VOD                        | 6–9 дней   |
| 4   | Трафик и общий кэш                 | 2–4 дня    |
| 5   | HLS                                | по решению |

### Этап 1 — `Source` и потоковый слой

- [x] `source.rs`: `Source`, `SourceError`, `parse`, `from_os_string`,
      `display`, `log_name`, `file_name`, `is_network`, `is_hls`; тесты
      `http(s)://`, `file://` (диск/UNC/`%20`), Windows-путь, не-Unicode.
- [x] `reader.rs`: `MediaReader`, `FileReader`; тесты read/seek/len.
- [x] `VideoDecoder::open(&Path)` + `open_source(&Source)`; `FfmpegDecoder`
      через `input_from_stream`; Symphonia остаётся на `File`.
- [x] `Command::Open(Source)`; ошибки несут `Source`; тексты файлов прежние.
- [x] `probe_tracks` только для файлов; `container_name()`/`codec_name()`.
- [x] `.m3u8` → `UnsupportedHls`.
- [x] `player-cli` принимает `Source`.
- [x] Замер 10-минутного 1080p: регрессия ≤ 5–10 %, иначе для файлов
      остаётся прямой путь.

Приёмка: локальный файл играет и перематывается как раньше; тесты зелёные;
замер не хуже.

### Этап 2 — асинхронное открытие

- [x] `Player::open` только запускает поток и ставит `Open`.
- [x] `Media` открывается в цикле ядра; ошибки — `Event::Error`, поток жив.
- [x] `Event::Ready(Box<MediaReady>)`; `DurationKnown` удаляется.
- [x] `Loading` в `VideoPlayer`, `video_view`, `controls_view`.
- [x] Одно ядро на плеер; `retry`, `EngineGone`, отмена `Stop`/`Open`/`Drop`.
- [x] `preview_frame` только для файлов.
- [x] `run_app_source`; README и примеры (doc-тесты) обновлены.
- [x] Тесты: медленный читатель, `open` < 50 мс; ошибки открытия — событием.
- [x] `player.rs` разделён на `player.rs`/`media.rs`/`engine.rs` (2.0).

Приёмка: окно не замирает; локальные файлы без регрессий; ошибки понятны.

### Этап 3 — HTTP(S) VOD

Подэтапы 3.0–3.6 и открытые вопросы — в `docs/CURRENT_TASK.md`.

- [x] `ureq` 3.5.x + `native-tls`; на агенте явно `TlsProvider::NativeTls`
      и `RootCerts::PlatformVerifier`.
- [x] `HttpReader`: `Range`, 200/206, редиректы, `Content-Length`/chunked,
      `Content-Type`, read-ahead 2 МиБ, слот ошибки.
- [x] Таймауты 5/5/2/15 с, отмена ≤ 2–3 с, 5 повторов, 10 редиректов.
- [x] `Source::Url` в `Media::open`; звук через FFmpeg; без `probe_tracks`.
- [x] `MediaReady { seekable, duration, … }`, `NotSeekable`.
- [x] `Buffering` для аудио и для видео без звука.
- [x] Очередь звука 32 блока; тест «медленное видео не проваливает звук».
- [x] Ошибки 404/403/416/DNS/TLS/`text/html` с URL.
- [x] Панель «Файл» / «URL», поле ввода, `Ctrl+V`.
- [x] `player-app`/`player-cli` разбирают URL; панель информации — URL.
- [x] Тесты на `127.0.0.1`.

Приёмка: MP4/MKV/WebM/MP3 по `http` и `https` играют; `Range` перематывает;
без `Range` играет без перемотки; обрыв продолжается; 404 понятен; закрытие
окна не ждёт сеть.

### Этап 4 — трафик и общий кэш

- [ ] Замерить трафик двух читателей (MP4, MKV).
- [ ] При дублировании — общий кэш 64 МиБ, блок 256 КиБ, два курсора.
- [ ] Если кэш не оправдался — рассмотреть один FFmpeg-демуксер.
- [ ] Тест на отсутствие роста памяти.

### Этап 5 — HLS

- [ ] До решения `.m3u8` → `UnsupportedHls`.
- [ ] A: отдать плейлист FFmpeg (нужны `hls` + `http(s)`; TLS нет).
- [ ] B: свой HLS-клиент (сегменты, таймстемпы, ABR) — отдельный проект.
- [ ] Подготовка уже в 1–3: `is_hls`, `live`, `seekable`, `duration = None`.

## 7. Тесты

- `Source`: URL, `file://`, UNC, `%20`, Windows-путь, не-Unicode, мусор.
- `FileReader`; кастомный IO для локального MP4 — те же кадры/PTS.
- `HttpReader`: 206, 200, chunked, 404, `text/html`, редирект, обрыв,
  задержка; два соединения без третьего `probe_tracks`.
- Отмена ≤ 2–3 с; `open` без блокировки UI.
- `.m3u8` → `UnsupportedHls`; `Loading`/`Buffering` в UI.
- README/примеры компилируются (doc-тесты).
- Память: 10 минут HTTP, ручной замер.

Сеть в тестах — только `127.0.0.1:0`.

## 8. Зависимости и лимиты

- `ureq` 3.5.x: `default-features = false`, `features = ["native-tls"]`.
  По умолчанию у `ureq` провайдер `Rustls` и корни `WebPki`; для Windows
  нужны явные `TlsProvider::NativeTls` и `RootCerts::PlatformVerifier`.
- `timeout_resolve`/`timeout_connect` 5 с, `timeout_per_read` 2 с;
  `timeout_global`/`timeout_recv_body` не задаём.
- `http_status_as_error(false)`; `max_redirects(10)`.
- Заголовки: `User-Agent: vgpu-player/<version>`, `Accept: */*`,
  `Accept-Encoding: identity`, `Range`. `Referer`/`Cookie` не шлём.
- `tiny_http` — только dev-dependency; `tokio`/`axum` не нужны.
- Лимиты: окно 2 МиБ, кэш 64 МиБ, простой 15 с, открытие ≤ 30 с.
- `file://` поддерживается; слайдер без перемотки выключается.

## 9. Риски и ограничения

- Кастомный IO может быть медленнее прямого файла — замер на этапе 1,
  иначе для файлов остаётся прямой путь.
- Два читателя дают два соединения и возможное дублирование трафика —
  этап 4.
- Один поток ядра: блокирующее чтение видео может опустошить звук
  (лечится очередью 32, затем prefetch/раздельные декодеры).
- Нет TLS в FFmpeg — `https` только через свой клиент.
- Без `Range` нет перемотки; MP4 с `moov` в конце открывается дольше.
- `Pause`/`Seek` во время чтения задерживаются до 2 с.
- Ресурс может измениться между `Range`; в v1 без гарантии консистентности.
- Что не обещаем: DRM, YouTube, DASH, live/ABR до этапа 5, субтитры,
  офлайн-кэш, авторизацию.

## 10. Готово для этапа

1. Приёмка этапа из §6.
2. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings`, `cargo test --workspace`.
3. Владелец прочитал код и объяснил его.
4. `docs/CURRENT_TASK.md` обновлён; README/примеры — если менялся API.
