# TWITCH_INTERGATE — Twitch-канал по простой ссылке (план)

**Статус:** план, реализация НЕ начата.
**Задача:** `https://twitch.tv/<login>` (и `www.`/`m.`) открывается в плеере
как live-HLS без ручного получения `*.m3u8` через `yt-dlp`.
**Связанные документы:** `docs/SPEC.md` (HLS, этап 5), `docs/CURRENT_TASK.md`.

---

## 1. Область (минимальный вариант)

Входит:

- анонимный playback-токен Twitch (без OAuth и без логина);
- обычные **live**-каналы;
- только H.264 (`supported_codecs=avc1`);
- один вариант: лучший по разрешению, но **не выше 1440p**;
- понятные ошибки: канал офлайн, sub-only/закрытый, канал не найден,
  Twitch API недоступен.

Не входит (осознанно):

- OAuth, подписки, приватные/платные каналы;
- VOD, клипы, highlights, `twitch.tv/videos/...`;
- чат, смайлы, уведомления, список каналов;
- ABR/переключение качества на лету;
- фильтрация рекламы (SSAI играет как часть потока);
- автопереподключение и фоновое обновление токена;
- YouTube и другие сервисы (это отдельные интеграции).

---

## 2. Пользовательский сценарий

1. Пользователь вставляет `https://twitch.tv/higanbanban` в панель URL или
   запускает `player-cli https://twitch.tv/higanbanban`.
2. Ядро распознаёт ссылку канала, получает playback-токен, строит usher-URL
   и открывает его существующим `HlsSource`.
3. В UI: `live=true`, `seekable=false`, плашка `LIVE`, кадры и звук идут.
4. Офлайн/sub-only: `Event::Error` с текстом, поток ядра продолжает жить
   (можно вставить другую ссылку).

Прямые `*.m3u8` (Twitch и любые другие) работают как раньше.

---

## 3. Архитектура

Всё изолировано в новом модуле `crates/player-core/src/twitch.rs`.

### 3.1. Распознавание ссылки

`twitch::channel_login(source: &Source) -> Option<&str>`

- Хосты: `twitch.tv`, `www.twitch.tv`, `m.twitch.tv` (регистр не важен),
  только `https`/`http`.
- Путь: ровно один непустой сегмент; допускается завершающий `/`.
- Зарезервированные сегменты отклоняются: `directory`, `videos`, `clip`,
  `p`, `settings`, `subscriptions`, `wallet`, `drops`, `inventory`,
  `friends`, `search`, `turbo`, `jobs`, `store`, `downloads`.
- Query/fragment игнорируются.
- Возвращается login в нижнем регистре.

### 3.2. Резолвер

`TwitchResolver::resolve(login, cancel) -> Result<String, PlayerError>`

Шаги:

1. **Playback-токен.** `POST https://gql.twitch.tv/gql`, заголовки:
   - `Client-ID: <TWITCH_WEB_CLIENT_ID>` (публичный Client-ID веб-плеера);
   - `Content-Type: application/json`;
   - `User-Agent` плеера, `Origin`/`Referer` `https://www.twitch.tv`.
     Тело — GraphQL `PlaybackAccessToken` (persisted query или полный текст;
     актуальный hash/схему сверить на момент реализации):

   ```graphql
   query PlaybackAccessToken(
     $login: String!
     $isLive: Boolean!
     $vodID: ID!
     $isVod: Boolean!
     $playerType: String!
   ) {
     streamPlaybackAccessToken(
       channelName: $login
       params: {
         platform: "web"
         playerBackend: "mediaplayer"
         playerType: $playerType
       }
     ) {
       value
       signature
     }
   }
   ```

   Ожидаемый ответ: `data.streamPlaybackAccessToken.{value,signature}`.

2. **usher-URL:**

   ```
   https://usher.ttvnw.net/api/channel/hls/<login>.m3u8
     ?allow_source=true
     &allow_audio_only=false
     &allow_spectre=true
     &player=twitchweb
     &platform=web
     &supported_codecs=h264
     &sig=<signature>
     &token=<percent-encoded value>
   ```

   Значение `supported_codecs` сверить на момент реализации: в веб-плеере
   встречается список `av1,h265,h264`. Надёжный путь — запросить без
   жёсткого ограничения и **отфильтровать варианты по `CODECS`**
   (`avc1… = H.264`); если H.264-варианта нет — ошибка `Unsupported`.
   `token` — JSON-строка из ответа; её нужно percent-encode (символы
   `{}`, `"`, `:`, `,` обязательны). Мини-энкодер на ~15 строк, без новой
   зависимости.

3. Вернуть готовый URL; дальше работает `HlsSource::open`.

Токен живёт ограниченное время (обычно часы). Для одной сессии usher-URL
достаточно; повторный `Open` канала резолвит заново. Фонового обновления нет.

### 3.3. Интеграция в `Media::open_with`

Перед выбором источника:

```text
if let Some(login) = twitch::channel_login(source) {
    manifest = resolver.resolve(login, cancel)?;
    open_source = Source::Url(manifest);
} else {
    open_source = source.clone();
}
```

- `HlsSource` получает `open_source`, а `Media::source`, `MediaReady` и
  сообщения об ошибках остаются с исходной ссылкой канала.
- Прямые `.m3u8` и локальные файлы не меняются.
- Сегментные ошибки HLS продолжают указывать на URL сегмента/манифеста,
  но токены в логи не попадают (`Source::log_name`, как сейчас).

### 3.4. Ограничение качества и кодек

Сейчас `select_variant` жёстко ограничивает высоту 1080p и не смотрит на
`CODECS`. Добавить параметры:

- `HlsOptions { max_height: u32, require_avc1: bool }`
  (по умолчанию `1080`, `false`);
- `HlsSource::open` получает options (или `open_with_options`);
- Twitch передаёт `max_height = 1440`, `require_avc1 = true`, остальные
  источники — дефолт;
- варианты без `avc1` в `CODECS` отбрасываются до выбора;
- если после фильтра нет вариантов — `TwitchErrorKind::Unsupported`;
- при отсутствии вариантов ≤1440 выбирается самое лёгкое, как сейчас.

Новый тест: synthetic master с 1440p/1080p/720p и с `avc1`/`hvc1` — для
Twitch выбирается 1440p H.264, для обычного HLS — 1080p.

### 3.5. Ошибки

Добавить типизированный вариант в `PlayerError` (имя согласовать при
реализации), например:

```text
Twitch { channel: String, kind: TwitchErrorKind }
```

Виды и тексты:

| Ситуация                 | kind          | Текст                                     |
| ------------------------ | ------------- | ----------------------------------------- |
| Канал офлайн             | `Offline`     | `канал <login> не в сети`                 |
| Канал не существует      | `NotFound`    | `канал <login> не найден`                 |
| Sub-only / нет прав      | `Restricted`  | `канал <login> требует подписку или вход` |
| Региональная блокировка  | `Blocked`     | `канал <login> недоступен в этом регионе` |
| Ошибка/недоступность GQL | `Api`         | `Twitch API: <причина>`                   |
| Нет H.264-варианта       | `Unsupported` | `Twitch не предложил H.264-вариант`       |

Различение `Offline`/`NotFound`: если токен не получен и GQL не говорит,
что канал запрещён, допускается один текст «не в сети или не найден»;
уточняющий запрос `user(login:)` — опционально, если нужен точный текст.

### 3.6. HTTP и JSON

- HTTP — существующий `ureq`-агент, `HttpConfig`, таймауты и повторы
  (`with_retries`); POST через `agent.post(...)`.
- JSON — **нужен `serde_json`** (в `player-core` его сейчас нет). Это
  новая зависимость; требуется согласие владельца. Альтернатива — свой
  мини-парсер, но для вложенного `data.streamPlaybackAccessToken` это
  лишний риск.
- Endpoints (`gql_url`, `usher_base`, Client-ID) — поля резолвера
  с production-дефолтами, чтобы тесты подставляли `127.0.0.1`.

---

## 4. Тесты

Все тесты — без внешней сети.

### Unit

- `channel_login`: положительные (`twitch.tv/x`, `www.`, `m.`, `/x/`,
  query, uppercase) и отрицательные (`/directory`, `/videos/1`, `/p/...`,
  два сегмента, другие хосты, `file://`).
- Сборка usher-URL: `sig`/`token` экранированы; `supported_codecs=avc1`.
- Разбор ответа GQL из фикстур: успех, `data: null`, `errors`.
- Маппинг ошибок: GQL 401/403 → `Restricted`; usher 404 → `Offline`;
  нет `avc1`-варианта → `Unsupported`; сеть → `Api`.
- `select_variant` с `max_height = 1440` и `require_avc1`: H.264
  выбирается, `hvc1`/`av01` отбрасываются, нет H.264 → `Unsupported`.

### Интеграционные (локальный `tiny_http`)

Mock-сервер отдаёт:

1. `POST /gql` → фикстура токена;
2. `GET /usher` → master с вариантами;
3. media-плейлисты и сегменты (reuse `ffmpeg`-ассеты из `tests/hls.rs`).

Сценарий: `Player::open(Source::Url("http://127.0.0.1:.../twitch/higanbanban"))`
→ `Ready { live: true, seekable: false }` → кадры идут.
Отдельные кейсы: офлайн, sub-only, битый JSON, GQL 500, канал с 1440p.

Ограничение: ссылка `twitch.tv` в тестах заменяется на локальный URL;
резолвер принимает настраиваемые endpoints, поэтому подменяется только
база, логика та же.

---

## 5. Ручная приёмка

1. Живой канал 1080p60: `Ready` ≤ 5 с, `live=true`, `seekable=false`,
   `dropped` в разумных пределах, звук и видео вместе (гейт
   `waiting_first_frame` уже есть).
2. Живой канал 1440p (если есть): выбирается 1440p, а не 1080p.
3. Офлайн-канал: `Error` с текстом «не в сети».
4. Sub-only/закрытый канал: `Error` с текстом «требует подписку или вход».
5. Повторное открытие другого канала без перезапуска приложения.
6. В логах нет `sig`, `token`, `value`.

---

## 6. Файлы (план)

- `crates/player-core/src/twitch.rs` — **новый**: детект, резолвер,
  usher-URL, ошибки, unit-тесты.
- `crates/player-core/src/lib.rs` — `mod twitch`.
- `crates/player-core/src/media.rs` — резолв перед `HlsSource::open`,
  передача `max_height = 1440` для Twitch.
- `crates/player-core/src/hls.rs` — `HlsOptions { max_height, require_avc1 }` /
  `select_variant` без жёстких 1080p и с фильтром `CODECS`.
- `crates/player-core/src/error.rs` — вариант `Twitch`.
- `crates/player-core/Cargo.toml` — `serde_json` (после согласования).
- `crates/player-core/tests/hls.rs` или новый `tests/twitch.rs` — сквозной
  mock-тест (новый файл — только после согласования).
- `README.md` / `player-gpui/src/app.rs` — подпись поля URL:
  «ссылка на видео, `.m3u8` или Twitch-канал».

---

## 7. Риски и ограничения

- **Неофициальный API.** Client-ID и GQL-операция взяты у веб-плеера;
  Twitch может их поменять. Это же решение используют yt-dlp/streamlink.
  Нужно решение владельца с учётом ToS Twitch.
- **Регион/аккаунт.** Геоблок и sub-only дают 401/403 — обрабатываем
  понятным текстом, без обхода.
- **Реклама.** SSAI-вставки играют как сегменты; `#EXT-X-DISCONTINUITY`
  уже сбрасывает декодеры. Фильтрации нет.
- **Качество.** Без ABR на слабом канале возможны дропы; prefetch уже
  помогает, но 1440p на медленной сети — риск.
- **Токен.** Не логируем и не кэшируем на диск; время жизни ограничено,
  при повторном открытии резолвим заново.
- **Rate limit.** Не чаще одного резолва на `Open`; повторы GQL — только
  на сетевые/5xx ошибки.

---

## 8. Оценка и порядок работ

| Шаг                                          | Оценка        |
| -------------------------------------------- | ------------- |
| Тесты (детект, URL, JSON-фикстуры, ошибки)   | 0,5 дня       |
| `twitch.rs` + `serde_json` + ошибки          | 0,5–1 день    |
| Интеграция в `Media`, `HlsOptions`           | 0,25 дня      |
| Mock-интеграционный тест                     | 0,25–0,5 дня  |
| Ручная приёмка (живой/офлайн/sub-only/1440p) | 0,5 дня       |
| **Итого**                                    | **1,5–2 дня** |

Порядок: сначала падающие тесты → резолвер → интеграция → приёмка.
Коммит — только после приёмки и апрува владельца.

## 9. Решения, которые нужны до старта

1. Согласие на зависимость `serde_json`.
2. Согласие на новый модуль `twitch.rs` и (опционально) `tests/twitch.rs`.
3. Формат ошибки: отдельный `PlayerError::Twitch { channel, kind }`
   или переиспользование `PlayerError::Hls` с текстом.
4. Подтверждение лимита качества: «лучший ≤ 1440p» для Twitch,
   обычный HLS остаётся ≤ 1080p.
5. Приемлемость использования публичного Client-ID веб-плеера с учётом
   ToS Twitch.
