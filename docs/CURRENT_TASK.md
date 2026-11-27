# CURRENT_TASK — YouTube VOD (этап 8.0)

**Статус (2026-11-26):** версия **0.4.0**. 8.0.1–8.0.8 реализованы, баги
перемотки и вытеснения HLS-окна исправлены, в оверлей добавлен второй блок.
Установщики пересобраны: `dist/vgpu-player-0.4.0-setup.exe`,
`dist/vgpu-player-0.4.0.msi`, portable `dist/vgpu-player/`.

**План и требования:** `docs/YOUTUBE_INTEGRATE.md`, `docs/SPEC.md` (§1,
§2.9, §3.1, §4, этап 8). История предыдущих задач — `git log`.

## Цель

Открывать `https://youtube.com/watch?v=…`, `https://youtu.be/…`,
`/shorts/`, `/live/`, `/embed/` как обычный HLS VOD — анонимно, без
`yt-dlp`, логина и cookies.

## Область

Входит:

- анонимный VOD; один HLS-вариант ≤ 1080p, SDR, AAC;
- demuxed video/audio playlists;
- `PlayerError::Youtube` с понятными текстами;
- кэш резолвов 4 ч (вместе с `title`), `visitor_id` 6 ч;
- название ролика в заголовке окна.

Не входит:

- YouTube live (понятная ошибка), плейлисты, каналы, ленты Shorts;
- авторизация/cookies, age-restricted;
- `yt-dlp`, DASH/adaptive, ABR, PO-токен;
- субтитры, storyboard, SponsorBlock, скачивание;
- меню качества/аудиодорожек.

## Решения (утверждены 2026-11-26)

1. Только VOD, только анонимно.
2. `serde` (`derive`) + `serde_json` в `player-core`.
3. Новый модуль `player-core/src/youtube.rs`.
4. `PlayerError::Youtube { target, kind }`.
5. `yt-dlp` сейчас не используем; `ResolvedSource::Adaptive` — задел.
6. Качество по умолчанию — 1080p.
7. Live — понятная ошибка.
8. Innertube приемлем.
9. `hl`/`gl` — фиксированные `en`/`US`.
10. Сквозные тесты — новый `crates/player-core/tests/youtube.rs`.
11. `videoDetails.title` показывается в заголовке окна (`MediaReady.title`).
12. Для mock-тестов адреса Innertube подменяет скрытый хук
    `player_core::set_endpoints_for_tests`.

## Архитектура

```text
youtube URL -> youtube::resolve (/player: visionOS -> iOS)
            -> (hlsManifestUrl, title)
            -> Media::open_source -> HlsSource -> декодеры
            -> MediaReady.title -> VideoPlayer -> заголовок окна
```

- `visionOS` (id 101) с `visitorData`; при отказе `iOS` (id 5) до 5 попыток.
- adaptive-URL Apple-клиентов не используются: без PO-токена обрезаются.
- HLS-запросы идут с браузерным `User-Agent`, если пользователь не задал свой.
- `Media::seek_hls` переоткрывает декодеры по разрешённому URL.
- `reader` при `SharedSource::Hls` не смотрит на `.m3u8`.

## Сделано

- `youtube.rs`: детект URL/id, `ResolvedSource::Hls { source, title }`,
  резолвер, `visitor_id`, typed JSON, кэш с `title`, ошибки, mock-тесты,
  ignored network-тест, скрытый хук `set_endpoints_for_tests`.
- `error.rs`: `YoutubeErrorKind` + `PlayerError::Youtube`.
- `media.rs`: `open_source`, резолв до `SharedSource`, `seek_hls`, UA,
  `title` в `MediaReady`.
- `reader.rs`: shared-first dispatch.
- `hls.rs`: `max_height = 1080`, SDR, original-аудио (`acont=original`).
- `video.rs`: резолв в `open_source_with_options` (`--decode-frames`).
- `player-gpui`: `VideoPlayer.title` → заголовок окна (`window_title_for`).
- **Исправлено вытеснение HLS-окна:** `TrackState::evict` больше не смотрит
  только на `order.front()`. После перемотки назад/перезапуска старый код
  видел, что фронт `order` уже впереди курсора, и ничего не выбрасывал —
  окно росло до 109 МиБ и `buf` до 182 с. Теперь вытесняются сначала
  сегменты позади курсора, потом слишком далёкие впереди (остаётся запас
  prefetch), потом самые дальние. Лимит 32 МиБ на дорожку снова держится.
- **Статистика окна HLS и второй блок оверлея:** `NetworkStats` получил
  `window_bytes`, `window_video_bytes`, `window_audio_bytes`, `buffer_ahead`;
  `HlsSource` заполняет их из окна сегментов. Оверлей показывает второй
  блок: `pos / dur · shown · speed`, кодеки (`h264 1920x1080@25 · aac 2ch
44.1 kHz`), `window v/a` и `buf` (секунды впереди). Скорость сети считает
  `VideoPlayer` по приросту байт между `Stats`.
- `tests/youtube.rs`: mock `/player` + `/visitor_id` + demuxed HLS; успех
  (кадры, seek, `Ended`, `Stats`, `title`) и ошибки live/нет HLS/вход/
  возраст/недоступно/плейлист. Плюс ignored real-seek сценарии: absolute,
  стрелки, в 0, в конец, две подряд.
- **Исправление перемотки HLS:** `HlsSource::seek_time` теперь возвращает
  начало выбранных сегментов (`SeekStart`); `AudioDecoder::skip_to`
  отбрасывает `position - segment_start` по длительности кадров, а не по
  абсолютному PTS, который после переоткрытия обнуляется. Без этого
  аудио уходило в EOF сразу после любой перемотки.
- `Cargo.toml`: `serde`, `serde_json`.
- `README.md`, `player-gpui/src/app.rs`: подсказка YouTube.
- `docs/{SPEC,CURRENT_TASK,YOUTUBE_INTEGRATE}.md`.

## Файлы

- `crates/player-core/src/youtube.rs`
- `crates/player-core/src/{error,media,reader,hls,video,audio,sync,cache,engine,lib}.rs`
- `crates/player-core/tests/youtube.rs` (новый)
- `crates/player-core/Cargo.toml`, `Cargo.toml`
- `crates/player-gpui/src/{video_player,app,video_view}.rs`
- `README.md`
- `docs/{SPEC,CURRENT_TASK,YOUTUBE_INTEGRATE}.md`

## Тесты и проверка

- `cargo test -p player-core --lib` — 222 passed, 3 ignored (network).
  Изредка параллельно флейкает не связанный `cache_reports_resident_bytes`;
  отдельно проходит стабильно.
- `cargo test -p player-core --test hls` — 12 passed (в т.ч. seek вперёд→назад
  для TS и fMP4).
- `cargo test -p player-core --test youtube` — 7 passed, 5 ignored.
- `cargo test -p player-gpui --lib` — 72 passed.
- Вытеснение HLS: `hls::tests::evict_drops_segments_behind_the_cursor`,
  `evict_drops_far_segments_after_backward_jump`.
- `cargo clippy --workspace --all-targets -- -D warnings` — чисто.
- `cargo fmt --all` — применён.
- Ignored: `youtube::live_tests::{resolves_a_real_video, plays_a_real_video,
audio_and_video_survive_seek}` и `real_vod_*`; запускаются вручную при
  рабочем VPN-прокси.
- Живая приёмка ядра (2026-11-26, через `ALL_PROXY=http://127.0.0.1:7890`):
  `video 1920×1080 25 fps`, `aac 2/44100`, `title` ролика; первый кадр и блок
  звука декодированы; после перемотки вперёд (15 с) и назад (5 с) звук и
  видео продолжаются (по 20 аудиочанков на каждой позиции).

## Окружение: блокировка DNS

В этой сети обычный DNS подменён (`NXDOMAIN` для `youtube.com` даже через
`8.8.8.8`/`1.1.1.1`), но HTTPS/DoH ходит через локальный прокси. `ureq` сам
читает `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`, поэтому плееру достаточно
переменных окружения:

```powershell
$env:HTTPS_PROXY="http://127.0.0.1:7890"
$env:HTTP_PROXY="http://127.0.0.1:7890"
$env:ALL_PROXY="http://127.0.0.1:7890"
cargo run -p player-app -- "https://youtu.be/<id>"
```

Альтернатива — TUN-режим VPN, который перехватывает и DNS. Код плеера менять
не нужно.

## Открытые вопросы

Нет блокирующих. Живая приёмка и замер памяти — по решению владельца.

## Следующий шаг

1. Коммит и тег `v0.4.0` (по решению владельца): `just commit "..."` /
   `just tag` / `just gp 0.4.0`.
2. Авто-повтор при обрыве сети (вариант B) — следующая сессия, план ниже.
3. При желании — снизить лимит окна или агрессивнее вытеснять память.

## Версия и артефакты (0.4.0)

- `Cargo.toml`, `Cargo.lock`, `installer/vgpu-player.iss`,
  `installer/vgpu-player.wxs`, `README.md`, `crates/player-gpui/README.md`
  обновлены `just set-version 0.4.0`.
- `just installers` — portable + Inno + MSI собраны 2026-11-26.
- Артефакты: `dist/vgpu-player/vgpu.exe`, `dist/vgpu-player-0.4.0-setup.exe`,
  `dist/vgpu-player-0.4.0.msi`.

## План следующей сессии: устойчивость к обрывам сети (вариант B)

**Цель:** при кратком/долгом обрыве сети воспроизведение не завершается
ошибкой и не начинается заново: читатель ждёт, показывает `Buffering`, а
после восстановления продолжает с того же сегмента и позиции.

**Текущее поведение (для сравнения):** запрос сегмента делает до 5 попыток
(~4 с), затем ошибка считается фатальной: `record_failure` → декодер `Err`
→ `run_engine` завершается → `Event::Error` → UI `Failed`. Кнопка `retry()`
заново открывает источник с начала.

### Шаги

1. **Классификация ошибок** (`http.rs`): добавить `HttpFailure::is_transient()`.
   Транзиентные: `Network(_)`, `Status(5xx | 408 | 429)`; фатальные:
   `Status(403 | 404 | прочие 4xx)`, `NotSeekable`, `Empty`, `NotMedia`,
   шифрование/битый плейлист. Для HTTP-источника (не HLS) поведение не
   меняем.
2. **Читатель HLS** (`hls.rs`):
   - `record_failure`/`sticky_failure` хранят только фатальные ошибки;
     транзиентная не «отравляет» prefetch и не всплывает в `Wait`-ветках;
   - `next_segment`/`segment_step`: при транзиентной ошибке снять `in_flight`,
     включить `buffering`, поспать с backoff (250 мс → 5 с) и повторить;
   - ограничить общее ожидание (решение владельца: 2–5 мин или без лимита),
     после чего вернуть фатальную ошибку; отмена (`Stop`/`Drop`) прерывает
     ожидание сразу;
   - prefetch-воркеры на транзиентной ошибке делают backoff и продолжают, а не
     останавливаются.
3. **Движок**: ничего не менять — блокирующее чтение уже показывается как
   `Buffering(true)`; движок продолжает тикать и не завершается.
4. **UI**: ничего критично не менять; кнопка «Повторить» остаётся запасным
   вариантом (и может начинать с сохранённой позиции — отдельное мини-улучшение).
5. **Тесты:**
   - mock HLS-сервер отдаёт сегмент `500`/рвёт соединение N раз, затем `200`;
     `Player` не присылает `Error`, приходит `Buffering(true/false)`, кадры
     продолжаются, позиция не откатывается;
   - фатальный `404` по-прежнему даёт `Event::Error` и `Failed`;
   - отмена во время ожидания (`Stop`) завершает чтение ≤ 2 с;
   - live: транзиентный обрыв обновления плейлиста также переживается.
6. **Документация:** `SPEC.md` §2.7 (перечтение/ошибки), §4 (когда ошибка
   показывается), `CURRENT_TASK.md` после выполнения.

**Оценка:** 0.5–1 день; основные риски — бесконечные повторы при реально
мёртвом URL (закрывается лимитом) и аккуратная классификация кодов.

**Минимальная альтернатива (если нужно быстро):** `retry()` запоминает
последнюю позицию и после `Ready` делает `Seek` туда — кнопка больше не
начинает сначала, но обрыв всё равно останавливает движок.
