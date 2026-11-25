# PLAN — этап 7.0: заголовки HTTP-источника (`Referer`/`User-Agent`)

**Статус:** реализовано и принято владельцем 2026-11-25; тесты/clippy/fmt
зелёные, версия `0.3.4`, Inno/MSI и portable пересобраны. Коммит `2313580`
и тег `v0.3.4` запушены в `origin`.
**Требования:** `docs/SPEC.md` §2.3, §2.8, §3.1, §3.4, §7–§9.
**Текущая задача:** `docs/CURRENT_TASK.md`, раздел 7.0.
**Версия релиза:** `0.3.4`.

## 1. Зачем

Некоторые источники проверяют не только URL, но и HTTP-заголовки:

- RuTube `play_stream` отдаёт `referer: https://rutube.ru/` и
  `user_agent: Mozilla/5.0`;
- Twitch media-запросы работают без заголовков, но будущему резолверу
  нужны `Origin`, `Referer`, `Client-ID` для token-эндпоинта.

Сейчас `vgpu`/`player-cli` отправить эти заголовки не могут: в `HttpConfig`
нет полей, а все запросы (`probe`, `Range`, HLS) идут без них. Поэтому
RuTube приходится открывать через mpv.

Цель — открывать такие ссылки самим `vgpu`, не заводя второй плеер.

## 2. Выбранный вариант

**Вариант 2: заголовки в ядре + CLI + API.** Вариант «mpv только для
RuTube» отклонён как долгосрочный: два плеера означают две приёмки, два
набора ошибок, разный UX и дублирование обновлений.

Архитектура уже готова к этому: весь HTTP идёт через один `ureq`-агент,
FFmpeg URL не открывает. Достаточно добавить заголовки в `HttpConfig` и
применять их в пяти местах запросов.

## 3. Решения (68–73)

68. **Заголовки — часть ядра.** `HttpOptions` живёт в `player-core`,
    `Referer`/`User-Agent` доступны и `vgpu`, и `player-cli`, и
    библиотечному API. mpv для RuTube — только временный обход.
69. **Публичный `HttpOptions` с валидированными заголовками.**
    Внутри — `HeaderName`/`HeaderValue` из `ureq::http::header`, а не
    «сырые» строки: CR/LF-инъекция и недопустимые имена отсекаются на
    границе API. Это же хранилище переиспользует Twitch-резолвер
    (`Origin`, `Referer`, `Client-ID`, `Content-Type`).
70. **Дефолты:** `User-Agent: vgpu-player/<version>`, `Accept: */*`,
    `Accept-Encoding: identity`. Пользовательские заголовки с тем же
    именем переопределяют дефолт (сравнение без учёта регистра).
71. **CLI-флаги:** `--referrer <строка>` (как у mpv; алиас `--referer`) и
    `--user-agent <строка>`. Произвольный `--header` в CLI пока не
    выводим: для RuTube хватает двух флагов, а лишняя поверхность атаки
    не нужна. Внутренний `HttpOptions::with_header` для будущего есть.
72. **Заголовки — на всё открытие, а не на первый запрос.** Один helper
    применяет их к `probe`, `Range`-блокам, перезапуску потока без
    `Range`, HLS master/media, `#EXT-X-MAP`, сегментам и prefetch.
73. **Версия `0.3.4`.** Изменения аддитивные: новые флаги и методы,
    `Command::OpenWith` рядом с `Command::Open`. Тег и установщики — после
    приёмки.

## 4. Модель заголовков

### 4.1 Публичный тип

Новый модуль `crates/player-core/src/http_options.rs`:

```rust
pub struct HttpOptions {
    headers: Vec<(HeaderName, HeaderValue)>, // приватно
}

impl HttpOptions {
    pub fn new() -> Self;
    pub fn with_referer(self, value: impl AsRef<str>)
        -> Result<Self, HttpOptionsError>;
    pub fn with_user_agent(self, value: impl AsRef<str>)
        -> Result<Self, HttpOptionsError>;
    pub fn with_header(self, name: impl AsRef<str>, value: impl AsRef<str>)
        -> Result<Self, HttpOptionsError>;
    /// Значение заданного заголовка (для тестов/диагностики).
    pub fn header(&self, name: &str) -> Option<&str>;
}

pub enum HttpOptionsError {
    InvalidName { name: String, reason: String },
    InvalidValue { name: String, reason: String },
}
```

- `Clone + Debug + Default + PartialEq + Eq` — нужен для `Command` и
  `PlayerOptions`.
- `HttpOptionsError` реализует `Display`/`Error` (`thiserror` уже есть);
  текст русский: `некорректное имя заголовка «…»: …`.
- `with_referer`/`with_user_agent` — сахар над `with_header`; повторное
  имя заменяет предыдущее значение (последнее выигрывает).
- Пустое значение отвергается: это почти всегда забытый аргумент.
- Никаких `Option<String>` для `Referer`/`User-Agent`: отсутствие
  заголовка — это отсутствие записи в списке, дефолты добавляет
  `HttpConfig`.

### 4.2 Эффективный набор в `HttpConfig`

`HttpConfig` (`http.rs`):

- теряет `Copy`, остаётся `Clone + Debug`;
- получает `headers: Arc<[(HeaderName, HeaderValue)]>` — эффективный
  набор, посчитанный один раз;
- `HttpConfig::default()` собирает дефолты;
- `HttpConfig::with_http(options: &HttpOptions)`:
  1. берёт дефолты;
  2. для каждого пользовательского заголовка ищет запись с тем же именем
     и заменяет её, иначе добавляет в конец;
- `headers()` — `&[(HeaderName, HeaderValue)]` для helper'а запросов.

`Arc<[..]>` выбран, чтобы `HttpConfig` оставался дешёвым при передаче в
`HttpLoader`/`HlsContext` и не клонировал строки на каждый блок.

### 4.3 Единая точка применения

В `http.rs`:

```rust
pub(crate) fn get(
    agent: &ureq::Agent,
    config: &HttpConfig,
    url: &str,
    extra: &[(&str, &str)],
) -> Result<ureq::http::Response<ureq::Body>, HttpError> {
    let mut request = agent.get(url);
    for (name, value) in config.headers() {
        request = request.header(name.as_str(), value.clone());
    }
    for (name, value) in extra {
        request = request.header(*name, *value);
    }
    request.call().map_err(request_error)
}
```

- Тип builder'а не называем — обходимся без `typestate`-типов `ureq`.
- `extra` — только `Range` и подобные одноразовые заголовки; они идут
  после общих и переопределяют их.
- Заголовки навешиваются на **каждый** запрос, включая повторы и
  редиректы (ureq переносит обычные заголовки через редирект).

Заменяются пять мест:

| Файл      | Место                                             | `extra`                        |
| --------- | ------------------------------------------------- | ------------------------------ |
| `http.rs` | probe в `HttpSource::open`                        | `Range: bytes=0-0`             |
| `http.rs` | `fetch_range`                                     | `Range: bytes=start-end`       |
| `http.rs` | `fetch_stream` (перезапуск без `Range`)           | —                              |
| `hls.rs`  | `fetch_playlist_text`                             | —                              |
| `hls.rs`  | `fetch_resource` (сегмент/`MAP`, в т.ч. prefetch) | `Range` для `#EXT-X-BYTERANGE` |

## 5. Протягивание через API

### 5.1 `player-core`

```rust
// lib.rs
pub use http_options::{HttpOptions, HttpOptionsError};

// media.rs
impl Media {
    pub(crate) fn open_with(source, cancel, cancelled) -> ...; // дефолт
    pub(crate) fn open_with_http(
        source, http: &HttpOptions, cancel, cancelled,
    ) -> ...;
}

// player.rs
impl Player {
    pub fn open(source) -> Result<Self, PlayerError>; // дефолт
    pub fn open_with_options(source, http: HttpOptions) -> Result<Self, PlayerError>;
}

// video.rs
impl VideoDecoder {
    pub fn open_source(source) -> Result<Self, PlayerError>; // дефолт
    pub fn open_source_with_options(source, http: HttpOptions) -> Result<Self, PlayerError>;
}
```

- `Media::open_with_http` передаёт `HttpConfig::with_http(http)` в
  `HttpSource::open`/`HlsSource::open`.
- Старые сигнатуры остаются обёртками — меньше правок в тестах и
  обратная совместимость.
- Для `--decode-frames` (одиночный `VideoDecoder` без общего
  `SharedSource`) добавляется `reader::open_reader_with_http`; старый
  `open_reader` вызывает его с `HttpConfig::default()`.

### 5.2 Команды и движок

`Command` получает аддитивный вариант:

```rust
pub enum Command {
    Open(Source),                                   // заголовки по умолчанию
    OpenWith { source: Source, http: HttpOptions }, // явные заголовки
    // ... остальные без изменений
}
```

`Engine` хранит `http: HttpOptions` и передаёт его в opener:

```rust
pub(crate) type Opener = Box<
    dyn Fn(&MediaSource, &HttpOptions, Arc<AtomicBool>, &dyn Fn() -> bool)
        -> Result<Media, PlayerError> + Send,
>;
```

- `Player::spawn` получает `HttpOptions`; `Player::open` — дефолт,
  `Player::open_with_options` — переданные.
- `Command::Open` сбрасывает `http` на дефолт (панель открытия в GUI),
  `Command::OpenWith` — ставит переданные.
- `Player::open_with` (тестовый) адаптируется под новую сигнатуру
  opener'а; поведение тестов не меняется.

### 5.3 GUI

```rust
pub struct PlayerOptions {
    pub volume: Option<f32>,
    pub http: HttpOptions,
}
```

- `PlayerWindow::new` получает `PlayerOptions` целиком.
- `VideoPlayer` хранит `http: HttpOptions`:
  - `create_with_options`/`open_with_options` — с явными заголовками;
  - `create`/`open` — с `HttpOptions::default()`;
  - при создании ядра — `Player::open_with_options`;
  - при переиспользовании — `Command::OpenWith`;
  - `retry` сохраняет текущие заголовки.
- Панель открытия URL по-прежнему не спрашивает заголовки: она
  использует `Command::Open` (дефолты). Аргументы `--referrer`/
  `--user-agent` влияют на стартовый источник.

### 5.4 CLI

`vgpu` (`player-app`) и `player-cli`:

- `--referrer <строка>` и алиас `--referer <строка>`;
- `--user-agent <строка>`;
- дубликат флага, отсутствие значения и ошибка `HttpOptions` — `USAGE`
  и код `1`;
- значения не логируются (Referer может содержать токен);
- `--help` упоминает оба флага и пример RuTube.

Пример:

```text
vgpu "<rutube-media-url>" --referrer https://rutube.ru/ --user-agent "Mozilla/5.0"
```

`player-cli --decode-frames` использует
`VideoDecoder::open_source_with_options`, чтобы флаги работали и в
режиме замера.

## 6. Порядок работ

Тесты пишутся до реализации; каждый шаг заканчивается зелёным прогоном
затронутых тестов.

- [x] **7.0.1 Контракт и тесты `HttpOptions`.**
  - новый `http_options.rs` с типом и ошибками;
  - тесты: дефолты, переопределение, дубликат имени, CR/LF, не-ASCII,
    недопустимое имя, `Clone`/`Eq`.
- [x] **7.0.2 `HttpConfig` и единый helper.**
  - `headers: Arc<[...]>`, `with_http`, `headers()`, `get()`;
  - перевести probe, `fetch_range`, `fetch_stream` на `get`;
  - тесты сервера: заголовки на probe, `Range`-блоке, перезапуске
    потока; `Range` не потерялся.
- [x] **7.0.3 HLS.**
  - `fetch_playlist_text` и `fetch_resource` через `get`;
  - тесты `RouteServer`: master, media, `MAP`, сегмент, prefetch;
  - регрессия: плейлисты без заголовков работают как раньше.
- [x] **7.0.4 Протягивание API.**
  - `Media::open_with_http`, `Player::open_with_options`,
    `VideoDecoder::open_source_with_options`, `reader::open_reader_with_http`;
  - `Command::OpenWith`, `Opener`, `Engine.http`, `Player::spawn`;
  - адаптировать тестовые opener'ы.
- [x] **7.0.5 CLI `vgpu` и `player-cli`.**
  - разбор флагов, валидация, справка, `USAGE`;
  - тесты парсеров: прямой и обратный порядок, алиас, дубликат,
    пустое значение, недопустимые символы, `--help`.
- [x] **7.0.6 GUI.**
  - `PlayerOptions.http`, `PlayerWindow::new`, `VideoPlayer` с
    `open_with_options`/`OpenWith`;
  - doc-примеры и README библиотеки — если меняется публичный API.
- [x] **7.0.7 Сквозной тест с проверкой заголовков.**
  - `test_support::serve` получает вариант с обязательными заголовками:
    без `Referer`/`User-Agent` → `403`, с ними → `Ready` и кадры;
  - тест на `Player` через `open_with_options`;
  - HLS-вариант: 403 на плейлисте и на сегменте без заголовков.
- [x] **7.0.8 Документация.**
  - `docs/SPEC.md`: §2.3, §2.8, §3.1, §3.2, §3.4, §4, §6–§9;
  - `docs/CURRENT_TASK.md`: 7.0, решения 68–73, файлы, тесты, статус;
  - `docs/TWITCH_INTERGATE.md`: резолвер собирает `HttpOptions` и
    использует `HttpConfig::headers()`, а не свою копию;
  - `README.md`: флаги и пример RuTube.
- [x] **7.0.9 Релиз и приёмка.** Версия `0.3.4`; `just installers`:
      `dist/vgpu-player-0.3.4-setup.exe` (14,0 МБ),
      `dist/vgpu-player-0.3.4.msi` (16,5 МБ), portable `vgpu.exe` (15,1 МБ).
      Владелец подтвердил RuTube 2026-11-25. Коммит `2313580`, тег `v0.3.4`
      запушены в `origin`.
  - `just set-version 0.3.4`;
  - `cargo test --workspace`, `clippy -- -D warnings`, `fmt --check`;
  - `just installers`; ручная проверка RuTube и Twitch;
  - коммит в рабочей ветке; merge/тег — только после подтверждения
    владельца.

## 7. Файлы

| Файл                                                                            | Что меняется                                                                          |
| ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `crates/player-core/src/http_options.rs`                                        | новый: `HttpOptions`, `HttpOptionsError`, валидация, `Default`                        |
| `crates/player-core/src/http.rs`                                                | `HttpConfig.headers`, `with_http`, `headers()`, `get()`, перевод трёх запросов, тесты |
| `crates/player-core/src/hls.rs`                                                 | два запроса через `get`, тесты заголовков                                             |
| `crates/player-core/src/media.rs`                                               | `open_with_http`                                                                      |
| `crates/player-core/src/player.rs`                                              | `open_with_options`, `spawn`, тестовые opener'ы                                       |
| `crates/player-core/src/engine.rs`                                              | `Opener`, `Engine.http`, `Open`/`OpenWith`                                            |
| `crates/player-core/src/command.rs`                                             | `OpenWith`                                                                            |
| `crates/player-core/src/reader.rs`                                              | `open_reader_with_http`                                                               |
| `crates/player-core/src/video.rs`                                               | `open_source_with_options`                                                            |
| `crates/player-core/src/lib.rs`                                                 | реэкспорт `HttpOptions`, `HttpOptionsError`                                           |
| `crates/player-core/src/http.rs` (`test_support`)                               | сервер с обязательными заголовками                                                    |
| `crates/player-cli/src/main.rs`                                                 | флаги, `USAGE`/`HELP`, `open_with_options`, `open_source_with_options`                |
| `crates/player-app/src/main.rs`                                                 | флаги, `PlayerOptions.http`                                                           |
| `crates/player-gpui/src/app.rs`                                                 | `PlayerOptions.http`, `PlayerWindow::new`                                             |
| `crates/player-gpui/src/video_player.rs`                                        | `http`, `open_with_options`, `OpenWith`                                               |
| `crates/player-gpui/src/lib.rs`                                                 | реэкспорт `HttpOptions`/`HttpOptionsError`                                            |
| `docs/SPEC.md`, `docs/CURRENT_TASK.md`, `docs/TWITCH_INTERGATE.md`, `README.md` | контракт, флаги, решения, пример                                                      |

## 8. Тесты (до реализации)

### 8.1 Ядро

- `http_options::tests`:
  - `default_is_empty`, `referer_and_user_agent_are_stored`,
    `header_overrides_previous`, `invalid_name_is_rejected`,
    `crlf_in_value_is_rejected`, `non_ascii_value_is_rejected`,
    `empty_value_is_rejected`.
- `http::tests`:
  - `probe_sends_source_headers`;
  - `range_blocks_send_source_headers`;
  - `stream_restart_sends_source_headers`;
  - `defaults_are_sent_without_options` (`vgpu-player/<version>`,
    `accept`, `accept-encoding: identity`);
  - `range_is_not_lost_with_headers`.
- `hls::tests`:
  - `master_and_media_playlists_send_headers`;
  - `map_and_segments_send_headers`;
  - `prefetch_sends_headers`.
- `player::tests`:
  - `url_with_required_headers_plays`: сервер отвечает `403`, если нет
    `Referer`/`User-Agent`; с `Player::open_with_options` приходит
    `Ready` и кадры;
  - `url_without_headers_is_rejected_by_the_server`: обратная проверка.
- `engine`/`command`:
  - `open_with_replaces_headers`, `open_resets_to_defaults`
    (через `Opener`, который записывает полученные `HttpOptions`).

### 8.2 CLI и GUI

- `player-app::tests`:
  - `referrer_and_user_agent_are_parsed`;
  - `referrer_alias_is_accepted`;
  - `duplicate_header_flag_is_an_error`;
  - `missing_header_value_is_an_error`;
  - `invalid_header_value_is_an_error`;
  - `help_mentions_every_option`.
- `player-cli::tests` — те же сценарии плюс сочетание с
  `--decode-frames`/`--seek`.
- `player-gpui`: `PlayerOptions::default()` содержит пустые
  `HttpOptions`; `VideoPlayer::open_with_options` компилируется и
  передаёт заголовки (проверяется косвенно через сквозной тест ядра).

### 8.3 Регрессии

- Twitch live, Apple/rtbcdn HLS, локальные файлы, `--volume`,
  `--decode-frames` — без изменений.
- Отдельный прогон без флагов: запросы содержат только дефолтные
  заголовки, поведение серверов не меняется.

## 9. Приёмка

1. В release:

   ```text
   vgpu "<rutube-media-url>" --referrer https://rutube.ru/ --user-agent "Mozilla/5.0"
   ```

   открывает и играет RuTube-поток без mpv.

2. `player-cli` с теми же флагами печатает `Ready`/`position` и играет.
3. Без флагов Twitch/Apple/локальные файлы работают как раньше.
4. Заголовки уходят на все запросы источника (тесты probe, `Range`,
   потока без `Range`, HLS master/media/`MAP`/сегментов/prefetch).
5. Ошибочные флаги дают понятное сообщение и код `1`, без паники.
6. `cargo test --workspace`, `clippy --workspace --all-targets --
-D warnings`, `cargo fmt --all --check` — зелёные.
7. `docs/SPEC.md` и `docs/CURRENT_TASK.md` обновлены; версия `0.3.4`.

## 10. Проверка

```text
RUSTFLAGS=-Clinker=rust-lld RUSTDOCFLAGS=-Clinker=rust-lld cargo test --workspace
RUSTFLAGS=-Clinker=rust-lld cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
just portable
just installers
```

Ручные проверки:

- RuTube: `play_stream`-URL с `--referrer` и `--user-agent`; пауза,
  перемотка (если VOD), закрытие;
- Twitch live — регрессия без флагов;
- HLS VOD (Apple fMP4) — регрессия;
- `vgpu --help` и `vgpu --help > help.txt` в release.

## 11. Риски и ограничения

- **Редиректы.** Заголовки уходят и на CDN после cross-host редиректа.
  Для `Referer`/`User-Agent` это обычно требуется, но `Cookie`/
  `Authorization` так пробрасывать нельзя: если понадобятся cookie/авторизация,
  нужен allowlist хостов отдельным решением.
- **Не-ASCII.** `HeaderValue` не принимает не-ASCII и управляющие
  символы: `Referer` с кириллицей должен быть percent-encoded.
- **Секреты.** Флаги видны в списке процессов и истории shell; не
  использовать для токенов. Значения не логируем.
- **RuTube.** Если `play_stream` вернёт URL без `.m3u8`, который на
  самом деле HLS, `Source::is_hls()` его не распознает — это отдельная
  проблема (не заголовки). Если поток защищён DRM/авторизацией,
  заголовки не помогут — внешний блокер.
- **`HttpConfig` без `Copy`.** Проверить, что все места передают
  `HttpConfig` по ссылке/перемещением; в тестах используется
  `..HttpConfig::default()`, он остаётся.
- **`Command` без `Eq`.** `HttpOptions` выводит `Eq`, но `SetVolume(f32)`
  по-прежнему не даёт `Eq` у `Command`; менять это не нужно.
- **CLI-поверхность.** Произвольный `--header` не выводим сейчас; при
  необходимости — отдельным решением с allowlist и предупреждением.

## 12. Вне объёма

- Произвольные заголовки в CLI (`--header`, cookies, `Authorization`).
- Реализация Twitch-резолвера (только подготовка `HttpOptions`).
- DRM, авторизация RuTube, обход гео-блокировок.
- Повторная отправка `Origin` для media-запросов: для RuTube достаточно
  `Referer`/`User-Agent`; `Origin` появится вместе с Twitch-резолвером.

## 13. Следующий шаг после 7.0

Twitch-резолвер (`docs/TWITCH_INTERGATE.md`) собирает `HttpOptions` с
`Origin`/`Referer`/`Client-ID`/`Content-Type` и использует
`HttpConfig::headers()` для POST к `gql.twitch.tv`. Отдельного
HTTP-стека и второй валидации заголовков не появляется.
