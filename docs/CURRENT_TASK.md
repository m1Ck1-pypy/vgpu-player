# CURRENT_TASK — задача 7.0: заголовки HTTP; этап 6 — PATH и консоль

**Задача 7.0 (2026-11-25):** `Referer`/`User-Agent` для HTTP-источников
(RuTube `play_stream`) — реализовано и принято владельцем 2026-11-25.
Версия `0.3.4`, установщики пересобраны. Полный план — `docs/PLAN.md`;
требования — `docs/SPEC.md` §2.8.

**Этап 5 — HLS:** 5.0–5.6 ✅ (приёмка закрыта, A/V перепроверен владельцем).
**Задача 6:** установщики добавляют приложение в пользовательский `PATH`,
чтобы его можно было запускать из консоли как `vgpu` — реализовано,
осталась ручная приёмка установщиков; 6.1 — проверка запуска по ссылке;
6.2 — полный экран по двойному клику; 6.3 — курсор прячется вместе с панелью;
6.4 — палитра под иконку приложения; 6.5 — тесты заголовков состояний;
6.6 — мин. размер окна и выдвижная громкость; 6.7 — `--help` и `--volume`
в консольном запуске.
**Обновлён:** 2026-11-23 (6.0), 2026-11-24 (6.1–6.6), 2026-11-25 (6.7,
7.0).

Twitch-резолвер — отдельная задача (`docs/TWITCH_INTERGATE.md`), в этом
этапе не ведётся.

Требования — `docs/SPEC.md`. История этапов 1–4 — `docs/CHANGELOG.md`,
разбор mpv — `docs/URL_LOGIC_MPV.md`.

## 7.0 Заголовки HTTP: `Referer`/`User-Agent` (2026-11-25)

**Статус:** реализовано и принято 2026-11-25: владелец подтвердил
работу RuTube. Тесты 208 unit + 10 integration `player-core`,
69 `player-gpui`, 16 + 1 `player-app`, 11 `player-cli`, 8 doc-тестов;
clippy и fmt чистые. Версия `0.3.4`; `just installers` пересобрал
`dist/vgpu-player-0.3.4-setup.exe` (14,0 МБ), `dist/vgpu-player-0.3.4.msi`
(16,5 МБ) и portable `dist/vgpu-player/vgpu.exe` (15,1 МБ). Полный
план — `docs/PLAN.md`; требования — `docs/SPEC.md` §2.8, §3.1, §3.4.

Запрос владельца: RuTube `play_stream` отдаёт `referer` и `user_agent`,
которые нужно передавать в запросах к медиа. Twitch работает без
заголовков.

**Решение:** вариант 2 — заголовки в ядре и CLI, полная замена mpv для
RuTube. Вариант 1 (mpv для RuTube, vgpu для Twitch) отклонён как
долгосрочный: два плеера, две приёмки, разный UX.

**Объём:**

- публичный `HttpOptions` (валидированные `HeaderName`/`HeaderValue`,
  дефолты `User-Agent: vgpu-player/<version>`, `Accept: */*`,
  `Accept-Encoding: identity`);
- `HttpConfig.headers` и единый helper запроса; заголовки на probe,
  `Range`-блоки, поток без `Range`, HLS master/media, `MAP`, сегменты,
  prefetch;
- `Player::open_with_options`, `Command::OpenWith`, `Media::open_with_http`,
  `VideoDecoder::open_source_with_options`;
- CLI `--referrer` (+`--referer`), `--user-agent`; GUI
  `PlayerOptions.http`;
- версия `0.3.4`, установщики, ручная проверка RuTube и Twitch.

**Файлы:** `player-core/src/http_options.rs` (новый), `http.rs`,
`hls.rs`, `media.rs`, `player.rs`, `engine.rs`, `command.rs`, `reader.rs`,
`video.rs`, `lib.rs`; `player-cli/src/main.rs`; `player-app/src/main.rs`;
`player-gpui/src/{app,video_player,lib}.rs`;
`docs/{SPEC,PLAN,CURRENT_TASK,TWITCH_INTERGATE}.md`, `README.md`.

**Тесты (сначала):** валидация и дефолты `HttpOptions`; заголовки на
всех путях запросов; сквозной `Player` с 403 без `Referer`; парсеры
CLI/app.

**Сделано:**

- `http_options.rs`: `HttpOptions` (`with_referer`, `with_user_agent`,
  `with_header`, `header`), `HttpOptionsError`; пустые/CRLF/не-ASCII
  значения отклоняются.
- `http.rs`: `HttpConfig.headers` (дефолты + переопределения),
  единый `get()`; заголовки в probe, `Range`, потоке без `Range`.
- `hls.rs`: master/media, `#EXT-X-MAP` и сегменты идут через `get()`;
  prefetch-воркеры наследуют те же заголовки.
- `Player::open_with_options`, `Command::OpenWith`,
  `Media::open_with_http`, `VideoDecoder::open_source_with_options`,
  `reader::open_reader_with_http`.
- CLI `--referrer`/`--referer`, `--user-agent`; справка, `USAGE`,
  валидация, тесты. GUI: `PlayerOptions.http`,
  `VideoPlayer::open_with_options`/`create_with_options`.
- Сквозной тест `url_with_required_headers_plays_and_without_them_fails`:
  сервер с обязательными `Referer`/`User-Agent`; без них — `403`.
- Версия `0.3.4` (`just set-version 0.3.4`).

**Приёмка:** `vgpu "<rutube-media-url>" --referrer https://rutube.ru/
--user-agent "Mozilla/5.0"` играет в release; без флагов Twitch/HLS и
локальные файлы не регрессируют; `cargo test`, clippy, fmt зелёные;
`docs` обновлены; версия `0.3.4`; установщики собраны.

**Приёмка владельцем (2026-11-25):** RuTube работает — \"все работает\".
Внешняя проверка шла через ссылку rtbcdn HLS; присланный конкретный узел
`river-ntv-mskix-d489` из этой сети не соединялся (TCP timeout), но
рабочая rtbcdn HLS-ссылка с теми же заголовками открылась и декодировалась
через `player-cli`. Коммит `2313580` и тег `v0.3.4` запушены в `origin`
2026-11-25.

## 6.7 `--help` и `--volume` в консольном запуске (2026-11-25)

Запрос владельца: в release добавить `-h`/`--help` со списком текущих
возможностей, расширить набор параметров и задавать громкость
(`--volume 10`) как необязательный компонент наравне с URL.

- **Без `clap`.** Аргументов по-прежнему мало, а help и ошибки нужны на
  русском; `clap` не локализуется и не решает главную проблему — печать из
  GUI-сборки. Разбор вынесен в `parse_args` → `Action`, так что будущая
  миграция на `clap` — замена одной функции и её тестов. Критерий перехода:
  ≥ 4–5 опций, подкоманды, completion/`man`.
- **`vgpu`**: `-h` (`-H`)/`--help`/`/?` (справка побеждает остальные
  аргументы, код `0`), `--volume <0…100>` в любом месте строки. Неизвестный
  параметр, `--volume` без значения или вне диапазона → `USAGE` и код `1`.
  Повторный позиционный аргумент по-прежнему игнорируется (первый
  побеждает): оболочка Windows дописывает остальные файлы мультивыбора
  к `"%1"`, поэтому строгий отказ здесь сломал бы ассоциации и drag&drop.
- **Громкость**: проценты переводятся в `0…1` и перекрывают `settings.cfg`
  только на запуск. Сама по себе в файл не пишется: `saved_settings`
  остаётся снимком файла, поэтому запись случится только если пользователь
  реально изменит настройку или громкость в окне.
- **Release-help без новых зависимостей**: в `player-app` inline-модуль
  `win_console` с `unsafe extern "system"` на пять функций
  `kernel32`/`user32`. Порядок: унаследованный перенаправленный дескриптор
  (UTF-8) → `AttachConsole` + `CONOUT$` (`WriteConsoleW`, UTF-16) →
  `MessageBoxW`. Модуль компилируется во всех сборках под Windows (ловит
  ошибки FFI), вызывается только в release; в debug — `println!`/`eprintln!`.
- **`player-cli`**: те же `-h`/`-H`/`--help`/`/?` и `--volume`; громкость
  уходит в `Command::SetVolume` до `Play`, на `--decode-frames` не влияет.
- **Инсталляторы не меняются.** Inno/MSI/портативный `.cmd` пишут
  ассоциации как `"vgpu.exe" "%1"`, а `[Run]` Inno запускает exe без
  параметров — контракт прежний, состав файлов и `PATH` тоже. Поэтому
  `vgpu --help` и `vgpu "…" --volume N` работают после установки без
  правок инсталляторов (в новой консоли из-за `PATH`).
- **`player-gpui`**: `PlayerOptions { volume: Option<f32> }` и
  `run_app_source_with`; `run_app_source`/`run_app` — прежние обёртки.
  `initial_volume` ограничивает override диапазоном `0…1`.

Файлы: `crates/player-app/src/main.rs`, `crates/player-cli/src/main.rs`,
`crates/player-gpui/src/app.rs`, `crates/player-gpui/src/lib.rs`,
`crates/player-gpui/README.md`, `README.md`, `docs/SPEC.md`,
`Cargo.toml`/`Cargo.lock`, `installer/vgpu-player.iss`,
`installer/vgpu-player.wxs` (только версия).

Тесты: 12 в `player-app` (help-флаги, порядок и границы `--volume`,
повторный/неизвестный параметр, мультивыбор, справка содержит все
параметры, прежние тесты кавычек), 8 в `player-cli`,
`cli_volume_overrides_saved_setting` в `player-gpui`. Прогнаны
`cargo test -p player-app -p player-cli -p player-gpui`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all --check`, `cargo check --release -p player-app`
(release-ветка FFI) и doc-тесты `player-gpui`.

Статус: реализовано, проверено в debug (`cargo run -p player-app -- --help`,
`--volume 101`, `player-cli --help`). В release проверено так:
`cmd /c "dist\vgpu-player\vgpu.exe --help > help.txt"` — текст попал в файл
(ветка перенаправленного вывода). Осталось вручную посмотреть
`vgpu.exe --help` в живой консоли `cmd`/PowerShell (приглашение возвращается
раньше текста) и установку новых артефактов.

Версия `0.3.3` (`just set-version 0.3.3`), установщики пересобраны
`just installers`: `dist/vgpu-player-0.3.3-setup.exe` (14 МБ),
`dist/vgpu-player-0.3.3.msi` (16 МБ), portable — `dist/vgpu-player/`.
MSI по-прежнему выдаёт предупреждения ICE91 (per-user каталог) — они
были и раньше, на сборку не влияют. В `vgpu.exe` нет ресурса VERSIONINFO
(только иконка), поэтому `VersionInfo` файла пуст — так было и до 6.7.

## 6.6 Мин. размер окна и выдвижная громкость (2026-11-24)

Запрос владельца: таймлайн сжимается слишком сильно, а громкость должна
открываться по значку и содержать mute и ползунок.

- `app.rs`: `WindowOptions.window_min_size = 560×315` (сначала было
  640×360; по просьбе владельца уменьшено). Ниже этого размера не
  помещаются ни таймлайн, ни время. Дополнительно у таймлайна в
  `controls.rs` `min_w = 120`, чтобы он не схлопывался на границе.
- Громкость: постоянный слайдер убран из ряда кнопок. Значок динамика
  открывает выдвижную панель над рядом (`controls.rs::volume_panel`):
  слева значок mute (клик — 0 %), ползунок с бегунком и процент справа.
  Состояние — `VideoPlayer::volume_panel_open`.
- Панель закрывается по повторному клику по значку, `Esc`, клику по кадру,
  открытию меню/«Открыть…», нажатию Play/fullscreen. Пока она открыта,
  автоскрытие панели управления не срабатывает.
- Величина под курсором показывается прямо в панели; hover-тултип и его
  тест удалены. Колесо мыши и сохранение громкости работают как раньше.
- `video_player.rs`: поле и методы `volume_panel_open` /
  `toggle_volume_panel` / `close_volume_panel`; `reset_video` закрывает
  панель при открытии нового файла.
- Проверка: `cargo fmt`, `clippy -D warnings` чистые; `player-gpui` —
  68/68, `player-app` — 6/6. Визуальная приёмка — за владельцем.

## 6.5 Тесты заголовков состояний (2026-11-24)

Запрос владельца: в тестах должны быть заглавные буквы, как в UI.

- `video_view::tests`: ожидания приведены к текстам из `describe()` —
  `"Ошибка"` и `"Воспроизведение завершено"`. Это те два падения, которые
  оставались в прогоне и не относились к предыдущим задачам; поведение и
  сами тексты не менялись.
- Проверка: `cargo fmt --check` — чисто; `cargo test -p player-gpui --lib` —
  69/69.

## 6.4 Палитра под иконку приложения (2026-11-24)

Запрос владельца: перекрасить интерфейс под иконку — оранжевый корпус,
жёлтые «клавиши» плёнки, белый треугольник Play.

- `lib.rs`: новый приватный модуль `theme` — единственное место с цветами;
  `app.rs`, `controls.rs`, `video_view.rs` берут константы оттуда. Цвета
  спектра (`BAR_LOW/HIGH_COLOR`) теперь тоже ссылаются на палитру, значения
  не изменились.
- Фоны остались тёмными (кадр важнее хрома), но тёплыми: фон `#140F0B`,
  поверхности `#241A12`, кнопки/наведение `#3A281A`, границы и дорожка
  слайдера `#4A3524`, поле ввода `#160F0A`.
- Оранжевый `#EF8A2E` — Play/Replay, заполнение слайдеров, плашка
  drag-and-drop. Жёлтый `#FFD93D` — галочки меню, `LIVE`, каретка ввода;
  текст на акцентах `#1B120C`.
- Подсказки `#B39D8C`, плейсхолдер `#7E6A5B`, ошибка `#FF8A7A`.
- Панель управления, оверлеи статистики/буферизации и плашка громкости —
  тёплые полупрозрачные слои вместо нейтрального чёрного.
- Проверка: `cargo fmt`, `clippy -D warnings` чистые; `player-gpui` —
  67/69 (те же два давних падения `video_view`), `player-app` — 6/6.
- Визуальная приёмка — за владельцем.

## 6.3 Курсор скрывается вместе с панелью (2026-11-24)

Запрос владельца: когда панель управления скрывается по автоскрытию, курсор
тоже должен исчезать; при движении мыши — возвращаться.

- Используется публичный API платформы
  `Platform::hide_cursor_until_mouse_moves()`: возврат курсора при движении
  мыши делает сама платформа (Windows: `SetCursor(None)` + флаг
  `cursor_visible`), свой обработчик движения не нужен.
- `run_app_source` больше не берёт `gpui_platform::application()`: платформа
  создаётся через `gpui_platform::current_platform(false)` и передаётся в
  `gpui::Application::with_platform`. Иначе экземпляр платформы был бы другим
  и не знал бы про окна приложения.
- `PlayerWindow` хранит `Rc<dyn Platform>`; `tick_auto_hide` вызывает
  `hide_cursor_until_mouse_moves()` в тот же момент, когда ставит
  `controls_visible = false`. Условия те же: включённое автоскрытие,
  воспроизведение, нет открытых меню/панелей.
- Курсор остаётся скрытым, если после этого нажать клавишу (панель вернётся,
  курсор — только при движении мыши), — так же ведут себя другие плееры.
- Проверка: `cargo test -p player-gpui --lib` — 67/69, падают те же два
  давних теста в `video_view`; `cargo fmt --check` и `clippy -D warnings`
  чистые. Ручная проверка — за владельцем.

## 6.2 Полный экран по двойному клику (2026-11-24)

Запрос владельца: двойной клик ЛКМ по кадру открывает плеер на весь экран.

- `app.rs`: `ClickAction`/`click_action` — один клик = пауза,
  `click_count >= 2` = `Window::toggle_fullscreen()`; иконка кнопки
  обновляется сразу по ожидаемому состоянию, как у кнопки панели.
- Одиночный клик выполняется задачей с задержкой `DOUBLE_CLICK_DELAY`
  (250 мс): поле `PlayerWindow::pending_click` хранит её, второй клик
  заменяет задачу на `None` и отменяет паузу. Закрытие окна роняет задачу
  (gpui отменяет `Task` при drop).
- `Escape` по-прежнему сначала закрывает оверлеи, потом выходит из
  fullscreen; `F` и кнопка панели работают как раньше. Клик, закрывший
  оверлей, по-прежнему не переключает воспроизведение.
- Тест: `app::tests::double_click_asks_for_fullscreen`.
- Проверка: `cargo test -p player-gpui --lib` — 67/69, новый тест зелёный;
  две давние ошибки в `video_view::tests` (ждут «ошибка» и «воспроизведение
  завершено» строчными, код отдаёт с заглавной) к этой задаче не относятся;
  `cargo fmt --check` и `clippy -D warnings` чистые.

## 6.1 Проверка `vgpu <ссылка>` из консоли (2026-11-24)

Владелец сообщил: `vgpu file:///D:/…` не открывает файл, а через меню
«Открыть…» тот же файл играет. Проверено на этой машине:

- ссылка в ASCII-кавычках в PowerShell
  (`vgpu 'file:///D:/…“FURYON”…(ft.…).mp3'`) работает: `vgpu` получает
  путь, Symphonia/FFmpeg открывают MP3, `Ready` с `duration` и позиция идут;
- без кавычек **PowerShell** разбирает `(` и `“…”` как свой синтаксис и
  `vgpu` вовсе не запускает: `Имя "ft.%20Alaina%20Cross" не распознано
как имя командлета`;
- `cmd` ту же строку передаёт одним аргументом (проверено через `cmd /c`),
  bash без кавычек падает на `(`.

Значит, это не ошибка `Source` и не регресс 6.0: `file://` разбирался и
раньше. Исправлено то, что можно исправить на стороне плеера:

- README: раздел «Ссылки с пробелами, скобками и «умными» кавычками» —
  кавычки для PowerShell/cmd/bash и `--%`;
- `Source` принимает `file:D:/…`, `file:/D:/…`, `file://D:/…`,
  `file://localhost/…` (раньше `file://D:/…` уходил в UNC с хостом `D:`);
- `source_argument` снимает парные `“…”`, `‘…’`, `«…»`;
- тесты: `source::tests` (16) и `player-app` (5 unit + 1 integration)
  зелёные.

Осталось: пересобрать `dist` и переустановить, если нужна проверка
новых `file:` форм; сама ссылка владельца работает и в `v0.3.0`.

## Сейчас

- Этап 4 закрыт: общий кэш на источник, трафик 1,00×, память на плато,
  перемотка с удержанием клавиши и «залипающим» ползунком.
- `.m3u8`-URL теперь открывается как HLS (5.3); локальный `.m3u8`
  (`Source::File`) по-прежнему отсекается как `UnsupportedHls`.
- **5.0 закрыт (2026-11-23).** В процессе проверено: связанный
  vcpkg-FFmpeg видит демуксеры `hls`, `mpegts`, `mov` и открывает
  локальный `.m3u8`; протоколов `https`/`tls` в нём **нет** (только
  `file`, `pipe`, `crypto` и т. п.). CLI-сборка с gnutls к приложению
  отношения не имеет.
- Поэтому выбран **вариант B**: сеть и TLS — на `ureq`, FFmpeg получает
  готовый поток байтов через `StreamIo`; HLS-демуксер FFmpeg в рабочем
  пути не используется.
- Тесты возможностей — `versions::capabilities` (+2 к `player-core`,
  всего 143).
- **5.1 закрыт (2026-11-23).** `player-core/src/hls.rs`: парсер master
  (`#EXT-X-STREAM-INF`) и media (`#EXTINF`, `#EXT-X-ENDLIST`,
  `#EXT-X-TARGETDURATION`, `#EXT-X-MEDIA-SEQUENCE`), теги `#EXT-X-MAP`,
  `#EXT-X-BYTERANGE`, `#EXT-X-KEY` (включая `METHOD=NONE` и `IV`),
  `#EXT-X-DISCONTINUITY`; разрешение относительных ссылок и ошибки с
  номером строки. 19 тестов: unit + плейлисты ffmpeg (TS, fMP4,
  `single_file`) + реальные Apple/rtbcdn/YouTube/Twitch из
  `target/hls-tests`. Всего 162.
- **2026-11-23 владелец утвердил demuxed-аудио:** в этап 5 входит
  `#EXT-X-MEDIA`/`AUDIO`, отдельный audio-rendition и два читателя
  (`Track::Video`/`Track::Audio`). Решение 49 расширено: «один источник»
  остаётся только для muxed. Оценка 5.2/5.3 — +1–2 дня.
- Исследование FFmpeg/hls.js/ExoPlayer/Shaka: стандартный путь — отдельный
  аудио-пайплайн, а не склейка байтов; файлы в `target/hls-research`.
- **5.2 закрыт (2026-11-23).** `hls.rs` — это теперь не только парсер:
  `HlsSource::open` (master/media, редиректы, выбор варианта и аудио),
  `TrackSource` с окном сегментов, загрузчиком, live-refresh и выравниванием
  старта по `PROGRAM-DATE-TIME`, `HlsReader` (`Read`/`Seek`/`MediaReader`,
  `seek_time` для VOD). Парсер знает `#EXT-X-MEDIA`/`AUDIO` и
  `#EXT-X-PROGRAM-DATE-TIME`; `mod hls` вышел из `#[cfg(test)]`
  (временно с `#[allow(dead_code)]` до подключения в 5.3).
- 22 новых теста: парсер `#EXT-X-MEDIA`/даты (синтетика + Apple/Twitch),
  выбор варианта, muxed/demuxed VOD, fMP4 init, BYTERANGE, редирект,
  404/HTML/битый плейлист/AES, live-хвост, refresh с пропуском сегментов,
  отмена ожидания, `seek_time`, выравнивание demuxed live. Всего 184.
- Итог проверки: `cargo test --workspace` — 184 `player-core` + 67
  `player-gpui` + 4 `player-app` + 8 doc-тестов; clippy и fmt чистые.
- Сеть и TLS — тот же `ureq`-агент и `HttpConfig`, что у `HttpSource`;
  `NetworkStats` общие на источник, счётчики обеих дорожек суммируются.
- **5.3 закрыт (2026-11-23).** `open_reader` получает `Track` и общий
  `SharedSource` (HTTP-кэш или `HlsSource`); URL `.m3u8` больше не
  отсекается, локальный `Source::File` — по-прежнему `UnsupportedHls`.
  `Media::open_with` создаёт `HlsSource` и передаёт его обоим декодерам;
  `Ready` получает `live`, `duration` из video-плейлиста и
  `seekable = !live`; `NetworkStats` — общие счётчики HLS, `is_buffering` —
  OR декодеров и источника.
- `Media::seek` для HLS VOD: `HlsSource::seek_time` ставит новые стартовые
  сегменты, декодеры переоткрываются на них и отбрасывают сэмплы до точной
  позиции; `engine.rs` вызывает `Media::seek` вместо двух декодеров.
- Demuxed live: `video_pts_shift_ms` применяется к PTS видео; признак
  `#EXT-X-DISCONTINUITY` идёт из `HlsReader` в декодеры и сбрасывает их
  перед следующим пакетом (init fMP4 по-прежнему повторяется читателем).
- 4 новых теста: `HlsSource::seek_time` на synthetic VOD и отказ для live,
  URL не отсекается до сети, локальный `.m3u8` — `UnsupportedHls`,
  сквозное открытие HLS VOD в `Media` (muxed TS от ffmpeg) с seek,
  видео-только HLS без звука. `player-core` — 189.
- Итог проверки 5.3: 189 `player-core` + 67 `player-gpui` + 4 `player-app`
  - 8 doc-тестов; clippy и fmt чистые. В Git Bash тесты линкуются с
    `-Clinker=rust-lld` (иначе перехватывается `/usr/bin/link`).
- **5.4 закрыт (2026-11-23).** `VideoPlayer` получает `live` из `Ready`,
  сбрасывает его при `Open`/`Stop`, `Seek`/`SeekRelative` явно игнорируются
  для live. В правом верхнем углу кадра — плашка `LIVE`; слайдер остаётся
  серым через `seekable = false`/`duration = None`.
- `Buffering` по-прежнему приходит из ядра при ожидании сегмента;
  ограничение: сегменты качаются синхронно в потоке ядра, поэтому долгая
  загрузка задерживает тик и плашка может появиться только после неё
  (кандидат на prefetch в 5.5/6, вопрос 8).
- Панель URL подписана «Ссылка на видео или .m3u8»; `vgpu` уже
  принимает HLS-URL без правок, `player-cli` печатает `live: true`.
- Итог проверки 5.4: 189 `player-core` + 68 `player-gpui` + 4 `player-app`
  - 8 doc-тестов; clippy и fmt чистые.
- **5.5 закрыт (2026-11-23).** Добавлен `crates/player-core/tests/hls.rs`:
  8 сквозных тестов через публичный `Player`/`Event` на `127.0.0.1`.
  Ассеты генерирует локальный `ffmpeg`; live-сервер дописывает окно,
  как настоящий эфир.
- Покрыто: muxed TS VOD (Ready, переход, пауза, `Ended`, запросы по
  одному), demuxed TS и fMP4 VOD (обе дорожки, `Seeked`, `net` — сумма),
  видео-только HLS до `Ended`, live muxed и demuxed (`live`, `!seekable`,
  игнор `Seek`), закрытие во время чтения сегментов и ошибки.
- Ошибки end-to-end: 404/500 плейлиста, 404/500 и пустой сегмент, HTML,
  битый плейлист, AES-128, неизвестный `AUDIO`, пустой master.
- `Ended` для источника со звуком проверяется только при живом
  звуковом устройстве (на headless ядро ждёт `output`); сам `Ended`
  гарантированно проверяется видео-только тестом.
- A/V sync в автотестах — по открытым дорожкам и временным меткам;
  слуховая проверка и `drop` на реальных CDN — в 5.6.
- Итог проверки 5.5: 189 unit + 8 integration `player-core`,
  68 `player-gpui`, 4 `player-app`, 8 doc-тестов; clippy и fmt чистые.
- **5.6 (2026-11-23): исправлены два блокера live.** 1) Не-seekable
  источник больше не получает seek-колбэк (`StreamIo::from_read`), иначе
  mov-демуксер на live fMP4 искал конец/размер и бесконечно тянул
  сегменты (Twitch — вечная загрузка). 2) PTS live-видео нормализуются
  по `AVStream.start_time`: у fMP4 tfdt может начинаться не с нуля, и
  кадры вечно ждали часы (`shown=0`).
- Twitch после фикса открытия: `Ready` за ~4 с, `live=true`,
  `1080p60`/`720p60` в зависимости от канала, кадры показываются. До
  prefetch на медленном CDN был сильный `drop` (343 за 13 с) — вопрос 8
  закрыт пулом загрузчиков (см. ниже).
- rtbcdn VOD: играет ~30 fps, `shown` растёт, трафик идёт. Apple fMP4
  master проверен 2026-11-23: demuxed, 1080p60, 10 мин, обе дорожки,
  звук доходит до устройства, `dropped=0` за 53 с. YouTube master
  (`manifest.googlevideo.com`) сбрасывает соединение/таймаутит для
  curl и `ureq` — внешний блокер, см. вопрос 7.
- 9-й сквозной тест — live fMP4 со сдвигом PTS 1000 с: сторожит
  нормализацию и отсутствие seek-колбэка.
- HLS-ссылка из `docs/CHANGELOG.md` подписана и истекает `1795806421`
  ≈ 2026-11-27. Для CI нужны локальные HLS-ассеты, сгенерированные
  `ffmpeg -f hls`; внешняя ссылка — только для ручной проверки.
- Версия `0.3.4` (2026-11-25); последний тег — `v0.3.4`.
- **7.0 реализован и принят:** `HttpOptions`/`HttpConfig.headers`, единый
  `get()`, заголовки на всех запросах, `open_with_options`/`OpenWith`,
  CLI-флаги, GUI `PlayerOptions.http`; версия `0.3.4`, установщики
  пересобраны, RuTube подтверждён владельцем 2026-11-25.
- **5.6, prefetch (валидирован и доработан 2026-11-23).** Замеры показали:
  одно TCP-соединение к Twitch-CDN даёт ~100–120 КБ/с (сегмент 1,9 МБ
  качается 3–13 с), несколько параллельных — ≈1 МБ/с и выше. В `hls.rs`
  работает пул фоновых загрузчиков: окно `PREFETCH_WINDOW = 16 с` эфира
  по `#EXTINF`, но не больше `PREFETCH_MAX_SEGMENTS = 8` сегментов;
  `PREFETCH_WORKERS_MUXED = 8`, `PREFETCH_WORKERS_DEMUXED = 4` на дорожку
  (всего 8), `PREFETCH_IDLE = 100 ms`; `TrackState::in_flight`,
  `TrackSource::work` (`Condvar`), `SegmentStep` + `segment_step`,
  `prefetch_once`, `prefetch_base`, `prefetch_span`,
  `start_prefetch`/`prefetch_worker`. Читатель не ждёт сеть, если сегмент
  уже скачан.
- Окно считается по времени, а не по числу сегментов: у Twitch сегмент
  2 с, у Apple — 6 с, и «пять вперёд» давало разный запас. Хвост плейлиста
  и нулевые `#EXTINF` учтены в `prefetch_span` (+2 unit-теста).
- Воркеры теперь стартуют лениво — при первом читателе (`Once`): без
  читателей качать нечего, а `open` + `seek_time` больше не скачивает
  начало VOD (это поймал unit-тест на перемотку).
- Валидация prefetch: unit `hls::` 46/46, сквозные `tests/hls.rs` 10/10,
  весь workspace (191 unit `player-core`), clippy и fmt — зелёные.
- Найден и исправлен блокер валидации: декодер **забирает** ошибку из
  общего слота, после чего prefetch-воркер видел «ошибки нет» и бесконечно
  повторял упавший сегмент (тест `/seg-error.m3u8` не получал `Event::Error`).
  Теперь `TrackSource` держит «липкую» копию первой ошибки: планировщик и
  новые читатели видят отказ, а слот для декодеров по-прежнему отдаётся
  один раз.
- Добавлен регрессионный тест prefetch:
  `prefetch_downloads_ahead_while_the_reader_is_paused` на медленном
  сервере (`AssetServer::set_delayed`, 300 мс на сегмент) — пока плеер
  стоит на паузе, окно вперёд всё равно скачивается (без пула был бы
  только первый сегмент).
- Дополнительно: синхронный `Fetch` помечает сегмент `in_flight` под тем же
  локом (не бывает двойных запросов); `Buffering` выставляется и при
  ожидании prefetch-сегмента; prefetch пропускает зашифрованные сегменты
  (иначе воркер зацикливался); `refresh_playlist` не повторяет запрос,
  если плейлист уже обновил другой поток; `set_start_sequence` очищает
  курсоры (prefetch не тянет старую позицию после seek).
- **Правки A/V после приёмки (2026-11-23).** По отчёту владельца на Apple
  VOD: 1) звук стартовал раньше видео; 2) после seek звук шёл, а кадр не
  обновлялся. Причина одна: `Play`/`restart_playback` включали `rodio`
  сразу, не дожидаясь первого кадра. Добавлен `waiting_first_frame`:
  после открытия и перемотки звук остаётся на паузе, первый кадр
  показывается без ожидания часов, затем вывод включается. Лог
  "первый кадр показан: запускаем звук" подтверждён на Apple (разница
  ~6 мс). 3) После конца звука позиция в UI теперь идёт за видео-хвостом
  (`tail_started` → `VideoTimer`), а не замерзает на последнем отчёте
  аудио-часов; добавлены логи `аудио закончилось`/`видео закончилось`
  с `position`/`last_shown` — по ним видно длину хвоста.
- Правки A/V проверены `cargo test --workspace` (191 unit + 10
  integration + 68 + 4 + 8 doc), clippy и fmt — зелёные. Владелец
  перепроверил в GUI 2026-11-23: старт, seek и конец — звук и кадр идут
  вместе, таймер останавливается на 10:00. ✅
- В GUI-крейт добавлены логи: `Ready` (duration/seekable/live/video/audio),
  `PositionChanged` раз в секунду, `Seeked`, изменения `Buffering`,
  `Stats` (shown/dropped/fps/lag/network), кадры каждые 30, отклонённые
  `Seek`/`SeekRelative` (`player-gpui/src/video_player.rs`).
- **Новое требование (2026-11-23): PATH установщиками.** Inno и MSI после
  установки добавляют каталог приложения в пользовательский `PATH`
  (`HKCU\Environment`), чтобы в **новой** консоли работало
  `vgpu "<URL>"` без полного пути. Реализовано, см. 6.0.
- `vgpu` уже принимает URL/путь первым аргументом
  (`parse_args`/`source_from_argument` снимают внешние кавычки), а файловые
  ассоциации передают `%1`; без аргумента открывается пустое окно плеера.
  С 6.7 есть `--volume` и `-h`/`--help`/`/?`.
- `RUSTFLAGS=-Clinker=rust-lld cargo check -p player-core --all-targets`
  — зелёный. Полный прогон тестов и clippy выполнен (итоги выше).

## Задача

Научить плеер открывать `http(s)://…/index.m3u8` и играть HLS. Контракт
`Source` → `Media` → `MediaReady` и события UI не ломаем: меняется только
источник и слой чтения.

Объём — VOD и live, **muxed и demuxed**: отдельный audio-rendition
(`#EXT-X-MEDIA`/`AUDIO`) входит в этап (утверждено 2026-11-23). VOD:
длительность из `#EXTINF`, перемотка, ошибки понятны. Live: `live = true`,
`seekable = false`, следование за окном сегментов. Для CI — только локальные
ассеты и `127.0.0.1`.

## Решения (этап 5; 46–65 утверждены 2026-11-23; 66–67 — этап 6)

46. **Область:** сначала VOD, затем live. ABR в первом проходе не делаем:
    выбираем один вариант из master-плейлиста.
47. **Архитектура — B (утверждено).** Свой HLS-клиент: парсим playlist,
    качаем сегменты нашим `ureq`, отдаём FFmpeg виртуальный поток.
    `HlsSource` общий для звука и видео в muxed; для demuxed — video +
    audio-плейлисты (решение 57). Плюсы: один проход трафика, наши счётчики,
    `Buffering`, кэш и ошибки; тесты без сети; TLS не зависит от сборки
    FFmpeg (у vcpkg-сборки его нет). Минусы: парсер, TS/fMP4, seek по
    времени, AES-128, live — больше работы.
    **A отклонён:** обходит наш `HttpSource`/кэш/`NetworkStats`, звук и
    видео открываются отдельно — возможен двойной трафик; тексты ошибок и
    `Buffering` хуже контролируются.
48. Если **A**: не используется (вариант отклонён). Ветки HLS через
    `format::input(&url)` в `FfmpegDecoder` не будет.
49. **Вариант B:** новый `player-core/src/hls.rs` — `HlsSource` (playlist,
    индекс сегментов, общий загрузчик), `TrackSource` и `HlsReader`.
    Muxed: звук и видео читают один источник, повторной загрузки нет.
    Demuxed: video + audio-плейлисты и по загрузчику на дорожку (57).
    `MediaReader::len` для HLS — `None`, `is_seekable` — false; перемотка
    VOD — через `Media::seek`/`seek_time` (60).
50. **Master-плейлист:** выбираем вариант с максимальным разрешением/битрейтом
    в пределах разумного (например, ≤ 1080p); без переключения на лету.
51. **Сегменты:** MPEG-TS и fMP4 (`#EXT-X-MAP`), `#EXT-X-BYTERANGE`,
    относительные URL и query. Начало — оба типа в VOD.
52. **Шифрование:** `#EXT-X-KEY` (AES-128) — отдельный подэтап; до него
    понятная ошибка `HlsEncrypted`/`UnsupportedHls`, без «мусорного» потока.
53. **Live:** плейлист без `#EXT-X-ENDLIST`: `live = true`,
    `seekable = false`, `duration = None`, старт с последних сегментов окна,
    периодическое обновление плейлиста. VOD: `live = false`, `seekable = true`,
    длительность = сумма `#EXTINF`.
54. **Счётчики и UI:** `NetworkStats` считает запросы плейлиста и сегментов;
    `Buffering` — ожидание сегмента; для live в UI — плашка `LIVE`, слайдер
    серый. Для VOD слайдер и `Seek` работают как у файла.
55. **Тесты:** только локальный HLS (сгенерированный ffmpeg) и `127.0.0.1`;
    внешняя ссылка — ручная проверка. Ошибки: 404 плейлиста/сегмента,
    битый плейлист, неизвестный `#EXT-X-KEY`, конец VOD.
56. Версия/тег после этапа 5 — `0.3.0` (2026-11-23).
57. **Demuxed-аудио (утверждено 2026-11-23).** Отдельный audio-rendition
    полноценный, а не muxed-only: парсим `#EXT-X-MEDIA` и `AUDIO`;
    выбираем DEFAULT/AAC/2 канала. Один `HlsSource` хранит video- и
    audio-плейлист; muxed — один плейлист на обе дорожки. Это расширяет
    49 и отменяет «один источник» для demuxed.
58. **`Track`.** `open_reader(source, track, cancel, shared)`;
    `HttpSource` трек игнорирует, `HlsSource::reader(track)` выбирает
    плейлист/загрузчик. `OpenedReader` не меняется.
59. **Выбор варианта.** ≤ 1080p, затем `BANDWIDTH`; дедупликация по `URI`
    (Apple: 8 URI × 3 аудиогруппы). Аудио: DEFAULT=YES → AAC (`mp4a`) →
    2 канала → первый с `URI`. Нет группы/URI — `Hls` с понятной причиной.
60. **Перемотка VOD.** Не байтовый `seek` (виртуального размера нет):
    `Media::seek` пересоздаёт декодеры на сегменте целевого времени,
    затем отбрасывает сэмплы до точной позиции; `Seeked`/часы как сейчас.
    `MediaReader::is_seekable` у HLS — false, `Media::is_seekable` — true
    для VOD.
61. **Live.** `live = true`, если video- или audio-плейлист без ENDLIST;
    `duration = None`, `seekable = false`. Старт с хвоста окна,
    обновление раз в `TARGETDURATION` (не чаще 1 с), следование
    `MEDIA-SEQUENCE`, `#EXT-X-DISCONTINUITY` — сброс декодера.
62. **Память/трафик.** Muxed: один `TrackSource` на обе дорожки, каждый
    сегмент один раз. Demuxed: по загрузчику на дорожку, счётчики
    суммируются. Окно сегментов ≤ 32 МиБ/дорожку, плейлист ≤ 4 МиБ,
    строку `cache: MB` для HLS не показываем.
63. **Ошибки.** `PlayerError::Hls { target, reason }` — плейлист/вариант/
    сегмент; `UnsupportedHls` — только нереализованное (AES-128 до
    подшага, LL-HLS, локальный `.m3u8`). URL сегмента в причине — через
    `log_name` (query скрыт).
64. **Сеть.** Один `ureq`-агент и тот же `HttpConfig` (таймауты, повторы,
    отмена); финальный URL после редиректов — база для относительных
    ссылок; не более 10 редиректов; `Accept-Encoding: identity`.
65. **Границы.** В этапе 5 — только URL; YouTube watch URL не
    поддерживается, для live нужен master. LL-HLS, AES-128, DASH, DRM,
    ABR — вне первой версии. Версия/тег — после этапа.
66. **PATH и запуск из консоли (утверждено 2026-11-23).** Установщики
    Inno и MSI добавляют каталог установки в **пользовательский** `PATH`
    (`HKCU\Environment`, `REG_EXPAND_SZ`), без UAC. После установки в
    **новой** консоли работает `vgpu "<URL>"`. Правила:
    - идемпотентность: повторная установка и обновление не добавляют
      запись дважды; сравнение без учёта регистра и завершающего `\`;
    - удаление убирает только свой каталог и не трогает остальные
      значения `PATH`; пустое значение после удаления удаляется;
    - установщик рассылает `WM_SETTINGCHANGE` (Inno:
      `ChangesEnvironment=yes`; MSI: штатный `Environment`), но уже
      открытые консоли сохраняют старое окружение — нужна новая;
    - лимит пользовательского `PATH` — 2047 символов: Inno проверяет и
      предупреждает, не ломая существующее значение; MSI ограничение
      документирует;
    - URL передаётся в кавычках: в `cmd` символ `&` разделяет команды;
    - портативная сборка `PATH` автоматически не меняет;
      `register-associations.cmd path` добавляет каталог,
      `path remove` — убирает;
    - release-сборка `vgpu` — GUI-подсистема: консольный запуск
      возвращает приглашение сразу и не печатает stderr; ошибки видны
      в окне плеера. Консоль — способ запуска, а не режим вывода.
67. **Имя команды `vgpu` (утверждено 2026-11-23).** Release-бинарник —
    `vgpu.exe` в пакете `player-app`; `player-app.exe` больше не
    выпускается. Установщики обновляют ассоциации на `vgpu.exe` и удаляют
    старый `player-app.exe` при установке/обновлении. `vgpu` без
    аргумента открывает пустое окно, `vgpu <URL>` — сразу открывает
    источник.
68. **Заголовки — в ядре (7.0, утверждено 2026-11-25).** `HttpOptions`
    живёт в `player-core`; `Referer`/`User-Agent` доступны `vgpu`,
    `player-cli` и библиотечному API. mpv для RuTube — только временный
    обход, не долгосрочная архитектура.
69. **`HttpOptions` с валидацией.** Внутри — `HeaderName`/`HeaderValue`
    из `ureq::http::header`: CR/LF, не-ASCII и недопустимые имена
    отсекаются на границе API. То же хранилище переиспользует будущий
    Twitch-резолвер (`Origin`, `Referer`, `Client-ID`, `Content-Type`).
70. **Дефолты:** `User-Agent: vgpu-player/<version>`, `Accept: */*`,
    `Accept-Encoding: identity`; пользовательский заголовок с тем же
    именем переопределяет дефолт. `Cookie`/`Authorization` не шлём.
71. **CLI:** `--referrer` (как у mpv; алиас `--referer`) и
    `--user-agent`. Произвольный `--header` пока не выводим; внутренний
    `HttpOptions::with_header` для будущего есть.
72. **Область действия:** заголовки на всё открытие, а не на первый
    запрос: probe, `Range`, поток без `Range`, HLS master/media, `MAP`,
    сегменты и prefetch. Смена на лету — `Command::OpenWith`.
73. **Версия `0.3.4`.** Изменения аддитивные: новые флаги и методы,
    `Command::OpenWith` рядом с `Command::Open`. Тег и установщики —
    после приёмки.

## План

### 5.0 Проверка возможностей и выбор A/B (0,5 дня) — ✅

- [x] Проверено **в процессе** (через FFI/`format`): `av_find_input_format`
      находит `hls`, `mpegts`, `mov`; `avio_enum_protocols` показывает
      `file`, `pipe`, `crypto`, `cache`, `concat`, `async`, `data`, `fd`,
      `subfile` — **`https` и `tls` отсутствуют**.
- [x] Локальный `.m3u8` (сгенерирован `ffmpeg -f hls`) открывается через
      `ffmpeg::format::input`: две дорожки, длительность определяется.
- [x] Решение **B** записано в этот файл и в SPEC §2.7; потеря статистики
      и двойной трафик не грозят. Тесты — `versions::capabilities`.

### 5.1 Парсер плейлистов (1 день) — ✅

- [x] `hls.rs`: master (`#EXT-X-STREAM-INF`) и media (`#EXTINF`,
      `#EXT-X-ENDLIST`, `#EXT-X-TARGETDURATION`, `#EXT-X-MEDIA-SEQUENCE`).
      Атрибуты разбираются с кавычками (`CODECS="avc1,mp4a"`), BOM, CRLF
      и комментарии допускаются.
- [x] `#EXT-X-MAP`, `#EXT-X-BYTERANGE` (с продолжением offset при пропуске),
      `#EXT-X-KEY` (включая `METHOD=NONE` и `IV` в hex),
      `#EXT-X-DISCONTINUITY`.
- [x] Относительные URL: абсолютные, от корня, protocol-relative, `?query`,
      `..`/`.`; редиректы остаются на `ureq` в 5.2.
- [x] Ошибки парсера: `Empty`, `MissingHeader`, `Syntax { line, reason }`;
      без паник (проверено на битых `#EXTINF`, byte-range, IV, кавычках,
      смешанных master/media).
- [x] Аудит и доп. тесты: плейлисты от ffmpeg (TS, fMP4, `single_file`),
      обрезанные/испорченные строки, переполнения `Duration` и `u64` в
      `#EXTINF`/`#EXT-X-BYTERANGE` — ошибка вместо паники.

#### Аудит 5.1

Исправлено после перепроверки:

- база без пути (`https://host`) — сегмент теперь даёт `https://host/seg.ts`;
- `#EXTINF` с гигантским/нечисловым значением — `Duration::try_from_secs_f64`,
  без паники;
- переполнение `offset + length` в `#EXT-X-BYTERANGE` и суммы `#EXTINF` —
  `Syntax` с номером строки;
- тесты ffmpeg запускают его с `current_dir` внутри `target/hls-tests`:
  при абсолютном Windows-пути `init.mp4` уходит в CWD, а не рядом с
  плейлистом.

Осознанные послабления (не ошибки, но надо знать):

- `#EXT-X-TARGETDURATION` не обязателен; для live 5.2 нужен fallback на
  максимум `#EXTINF`;
- `#EXT-X-BYTERANGE` без `offset` продолжает предыдущий конец без проверки,
  что предыдущий сегмент — тот же ресурс (RFC считает такой плейлист
  невалидным);
- `#EXT-X-MAP` с `BYTERANGE` без `offset` получает смещение 0;
- неизвестные теги игнорируются; схема URI не проверяется (это делает 5.2
  перед загрузкой: только http/https).

#### Реальные плейлисты (2026-11-23)

Скачаны в `target/hls-tests` (в git не попадают), разбор —
`real_world_playlists_are_parsed_when_present`:

- **Apple fMP4 master:** 24 варианта, 3 группы `#EXT-X-MEDIA:TYPE=AUDIO`
  (`aud1/aud2/aud3`), I-frame и subtitles игнорируются. 24 варианта — это
  8 видео-URI × 3 аудиогруппы с одинаковым `URI`: при выборе варианта
  нужно дедуплицировать по URI и учитывать `AUDIO`. Парсер `AUDIO` пока
  не сохраняет — см. вопрос 4.
- **Apple fMP4 variant:** 100 сегментов, 600 с, `#EXT-X-MAP`;
  **audio rendition:** 101 сегмент, 600,043 с.
- **rtbcdn VOD:** 46 сегментов, 228,315 с, muxed TS (`v1-a1.ts`).
- **YouTube live (файлы владельца `index-video.m3u8` / `index-audio.m3u8`):**
  721 сегмент в каждом, `live = true`, `MEDIA-SEQUENCE` 1012360 и 1012384
  (окна сдвинуты), `PROGRAM-DATE-TIME`, `DISCONTINUITY-SEQUENCE`,
  `DATERANGE`/`CUEPOINT` разбираются без ошибок (неизвестные теги
  игнорируются). Манифест из этой сети не скачивается (`curl` reset), но
  тексты владельца парсятся.
- **Twitch live (`real-live-twitch.m3u8`, канал `streamdatabase`):**
  15 сегментов по 2 с, окно 30 с, `live = true`, muxed TS;
  `TWITCH-*`, `DATERANGE`, `PROGRAM-DATE-TIME` игнорируются.

#### Как решают другие (2026-11-23)

Исходники скачаны в `target/hls-research` (в git не попадают):

- **FFmpeg `libavformat/hls.c`:** `#EXT-X-MEDIA` → rendition-группы;
  `add_renditions_to_variant` добавляет внешние плейлисты к варианту, а
  `update_streams_from_subdemuxer` заводит потоки каждого плейлиста в один
  `AVFormatContext` и выравнивает `start_time` по первому timestamp. То есть
  аудио и видео — отдельные playlists/потоки внутри одного демуксера.
- **hls.js:** отдельный `AudioStreamController` и `audioTracks`/levels,
  выравнивание через `initPTS`/`alignStream`, старт с `startPosition`.
- **ExoPlayer:** `HlsSampleQueue[]` — очередь на каждый rendition; muxed-аудио
  передаётся отдельным `muxedAudioFormat`.
- **Shaka Player:** `createStreamInfosForVariantTags_` создаёт отдельные
  audio stream info из `#EXT-X-MEDIA`; дальше — раздельные пайплайны.
- **mpv/VLC:** используют модель FFmpeg (renditions как отдельные потоки).

Вывод: стандартный путь — **отдельный аудио-пайплайн**, а не склейка байтов.
Наш аналог: `HlsSource` хранит video- и audio-плейлист, `open_reader`
получает `Track`, у каждой дорожки свой курсор/загрузчик, счётчики общие.
Синхронизация — по PTS, при расхождении окон — по `PROGRAM-DATE-TIME`
(как `initPTS` в hls.js).

### 5.2 `HlsSource` и чтение сегментов (2–3 дня) — ✅

#### 5.2.0 Парсер (расширение 5.1)

- [x] `#EXT-X-MEDIA`: `TYPE`, `GROUP-ID`, `NAME`, `LANGUAGE`, `DEFAULT`,
      `AUTOSELECT`, `CHANNELS`, `URI`; `MasterPlaylist.audio_renditions`.
- [x] `Variant.audio_group` из атрибута `AUDIO=`.
- [x] `#EXT-X-PROGRAM-DATE-TIME` в `Segment`: epoch-секунды (свой мини-парсер
      ISO-8601 без новых crate) + тесты на реальных файлах.
- [x] Вывести `mod hls` из `#[cfg(test)]`: парсер входит в релизную сборку.

#### 5.2.1 Загрузка плейлистов и выбор

- [x] `HlsSource::open(source, config, cancel)`: GET master/media через
      `ureq`, финальный URL после редиректов — база; лимит 4 МиБ;
      404/5xx/таймаут/пусто → `Hls`/`HttpStatus`/`Network`.
- [x] Master: выбор ≤ 1080p, дедупликация по URI, аудио DEFAULT/AAC/2ch;
      ошибка при ссылке на несуществующую группу.
- [x] Media без master — muxed-источник.
- [x] Тесты на текстах Apple-мастера (24 варианта, 3 аудиогруппы).

#### 5.2.2 `TrackSource`, загрузчик и читатели

- [x] `TrackSource { playlist, loader, buffering, failure }`;
      `HlsSource { video, audio: Option, live, duration, counters }`.
- [x] `HlsReader`: `Read`, `MediaReader` (`len = None`,
      `is_seekable = false`), `seek_time(Duration)`, `failure_slot`,
      `buffering_flag`.
- [x] Сегменты: MPEG-TS (конкатенация), fMP4 (`#EXT-X-MAP` + media),
      `#EXT-X-BYTERANGE`, повторы/таймауты/отмена.
- [x] Окно сегментов ≤ 32 МиБ/дорожку; muxed — один `TrackSource`, каждый
      сегмент скачивается один раз.

#### 5.2.3 Live и перемотка VOD

- [x] Refresh плейлиста раз в `TARGETDURATION` (не чаще 1 с), окно по
      `MEDIA-SEQUENCE`, старт с хвоста, `#EXT-X-DISCONTINUITY` — сброс
      (init fMP4 повторяется; сброс декодера — 5.3).
- [x] Выравнивание demuxed live по `PROGRAM-DATE-TIME`/live-edge; хук
      сдвига видео (`video_pts_shift_ms`, применение — 5.3).
- [x] `seek_time` для VOD: сегмент по целевому времени; `Media::seek`
      переоткрывает декодеры и отбрасывает сэмплы до позиции (5.3).

#### 5.2.4 Счётчики, `Buffering`, ошибки, отмена

- [x] `NetworkStats` складывает обе дорожки; запросы/байты не двоятся.
- [x] `Buffering` — ожидание сегмента; слот ошибки для декодеров.
- [x] `Stop`/`Open`/`Drop` и перемотка прерывают загрузку сегмента.
- [x] Юнит-тесты загрузчика и читателей; полный локальный сервер — в 5.5.

#### Аудит 5.2 (2026-11-23)

- Счётчики: один `CacheCounters` на источник, поэтому muxed-дорожки не
  двоят ни запросы, ни байты; `cache_bytes` для HLS остаётся 0 (`cache: MB`
  не показываем).
- Хвост live: отступаем на `max(2 × TARGETDURATION, 4 с)`, но не меньше двух
  сегментов; при demuxed обе дорожки выравниваются на общий момент по
  `PROGRAM-DATE-TIME`.
- `#EXT-X-BYTERANGE`: сервер с `206` читается точно по длине; сервер с `200`
  пропускает смещение и читает длину (так же работает `single_file`).
- Ошибки сегмента/плейлиста пишутся в слот как `HttpFailure::Hls` → позже
  станут `PlayerError::Hls` с `log_name` (query скрыт).
- `Seek` виртуального потока: `Start(0)`/`Current(0)`/вперёд; `End` и назад
  не поддерживаются — перемотка VOD идёт через `seek_time`.
- Ограничения, зафиксированные на будущее: PTS-сдвиг и сброс декодера по
  `DISCONTINUITY` требуют интеграции с `video.rs`/`audio.rs` (5.3), а тесты
  трафика на большом окне — в 5.5.

### 5.3 `Media`, `MediaReady`, ошибки (1–1,5 дня) — ✅

- [x] `is_hls` больше не отсекает URL; `Media::open_with` создаёт
      `HlsSource` и передаёт его обоим декодерам.
- [x] `Track` в `open_reader`: `AudioDecoder` → `Track::Audio`,
      `VideoDecoder` → `Track::Video`; `HttpSource` трек игнорирует.
- [x] HLS demuxed: звук из audio-rendition, видео из variant; muxed — один
      плейлист.
- [x] `Media::seek` для HLS VOD: переоткрытие на сегменте + skip до точной
      позиции; `engine.rs` вызывает `Media::seek`, а не два декодера.
- [x] `MediaReady`: `duration` — VOD из video-плейлиста, `seekable` —
      только VOD, `live` — если любой плейлист без ENDLIST.
- [x] `Media::network_stats` — сумма; `is_buffering` — OR декодеров.
- [x] `PlayerError::Hls`/`UnsupportedHls`; `Source::File` с `.m3u8` —
      `UnsupportedHls` (URL-only), URL — открывается. Тест
      `video::tests::local_hls_is_rejected_before_opening` для файлов +
      тест `hls_url_is_not_rejected_before_the_network` для URL.
- [x] Звук без аудиодорожки (video-only media) — видео играет молча, без
      паники; в `Ready` `audio = None`.

#### Аудит 5.3 (2026-11-23)

- `open_reader` теперь принимает `Track` и `Option<&SharedSource>`; файл и
  обычный HTTP дорожку игнорируют, HLS выбирает плейлист/загрузчик.
- `HlsSource::seek_time` меняет только стартовые последовательности
  `TrackSource`; новые читатели создаются уже на целевом сегменте. Старые
  декодеры отпускаются до переоткрытия, их курсоры снимаются в `Drop`.
- `VideoDecoder::skip_to`/`FfmpegDecoder::skip_to` не дергают `Input::seek`,
  а отбрасывают сэмплы до PTS-цели. Это опирается на непрерывные
  таймстемпы сегментов (ffmpeg/Apple и типовые CDN) — ограничение
  зафиксировано для 5.6.
- PTS-сдвиг и `DISCONTINUITY`-сброс меняют состояние только декодеров;
  FFmpeg-демуксер продолжает читать тот же виртуальный поток.
- Ошибка перемотки HLS отдаётся ядру как `PlayerError` (как и ошибка
  байтовой перемотки), поэтому сетевая неудача по-прежнему завершает
  поток с `Error` — решение не меняли.
- Локальный `.m3u8` отсекается в `Media` и `VideoDecoder` до открытия;
  `AudioDecoder` для файла остаётся на Symphonia/FFmpeg.

### 5.4 UI и движок (0,5–1 день) — ✅

- [x] `Buffering` при ожидании сегмента; `Ready` с `live`.
      Оговорка: событие приходит, когда флаг успевает подняться до тика;
      синхронная загрузка сегмента может его задержать (вопрос 8).
- [x] Для live: плашка `LIVE`, слайдер серый, `Seek` игнорируется.
- [x] Для VOD: обычные перемотка/слайдер, `Seeked` и `SeekHint` работают.
- [x] `.m3u8` в панели URL и в `player-app`/`player-cli`.

#### Аудит 5.4 (2026-11-23)

- `VideoPlayer.live` — отдельное поле состояния UI; из `Ready` оно
  попадает и в плашку, и в защиту `Seek`. Дополнительно к
  `seekable = false` явная проверка не даёт отправить `Seek` в live даже
  при ошибочном `seekable`.
- Плашка `LIVE` живёт в `video_view` и рисуется поверх кадра, спектра и
  текстового плейсхолдера; в `Idle`/`Loading`/`Failed` её нет.
- VOD-перемотка не менялась: `SeekHint`/`Seeked` работают как в 4-м этапе;
  HLS VOD отдаёт `duration` и `seekable = true` из 5.3.
- `player-cli` не меняет контракт событий: только печатает `live: true`
  для ручной проверки. Панель URL не фильтрует расширения — `.m3u8`
  принимается как обычная ссылка.
- Остаётся ограничение 5.3: перемотка HLS рассчитывает на непрерывные PTS
  сегментов; UI это не компенсирует.

### 5.5 Тесты (1,5–2 дня) — ✅

- [x] Парсер: `#EXT-X-MEDIA`/`AUDIO`, `PROGRAM-DATE-TIME`, относительные
      URL, byte-range, fMP4, ключи, битые строки; реальные тексты из
      `target/hls-tests`.
- [x] Локальный сервер (`127.0.0.1`): muxed TS VOD — длительность, seek,
      пауза, `Ended`, один запрос на сегмент.
- [x] Demuxed fMP4 и TS VOD: звук и видео играют, seek работает, `Seeked`,
      A/V sync (по дорожкам/PTS), `net` — сумма двух дорожек.
- [x] Live muxed и demuxed: `live = true`, `seekable = false`, обновление
      окна, старт с хвоста, закрытие во время сегмента.
- [x] Ошибки: 404/500 плейлиста и сегмента, обрыв (пустой сегмент),
      битый плейлист, неизвестный `AUDIO`, AES-128, пустой вариант.
- [x] Трафик: muxed — без двойной загрузки; demuxed — по одному разу на
      дорожку; счётчики не врут.

#### Аудит 5.5 (2026-11-23)

- Новый файл `crates/player-core/tests/hls.rs` — интеграционный тест на
  публичном API: свой `tiny_http`-сервер с файлами, подменами и
  динамическими плейлистами; `ffmpeg`-ассеты — в `target/hls-tests`.
- Live-сервер растёт при обновлении (16 + 8 сегментов на запрос), пока
  тест не заморозит окно: так FFmpeg-проба не упирается в конец
  плейлиста, как в реальном эфире.
- Для `Ended` со звуком есть осознанное условие: на headless-машине
  `AudioSink` не открывается, и ядро не отправляет `Ended`
  (`audio_blocked`). Видео-только HLS проверяет `Ended` без условий.
- A/V sync автоматически не измеряется на слух: проверены открытые
  обе дорожки, `Seeked` и суммарные счётчики; реальное звучание — 5.6.
- Ограничение синхронной загрузки сегментов (вопрос 8) не изменилось:
  тесты трафика и завершения не зависят от плавности UI.

### 5.6 Приёмка (0,5–1 день) — ✅ (YouTube — внешний блокер)

- [x] rtbcdn VOD (muxed TS): играет, `shown` растёт; seek проверен
      автотестами (`Seeked`, переоткрытие).
- [x] Apple fMP4 VOD (demuxed): открытие, обе дорожки, звук, 1080p60,
      `dropped=0` за 53 с — проверено CLI 2026-11-23.
- [x] Twitch live (muxed fMP4+TARGETDURATION 6): открывается, `live`,
      1080p60/720p60 в зависимости от канала, кадры идут.
- [x] Twitch live после prefetch (2026-11-23, 37 с, старые 5/5):
      `shown` 1376→2234 (≈60 fps), `dropped = 3` за весь прогон (до
      prefetch было 343 за 13 с), `lag = 0`, `retries = 0`. Сегменты
      ~875 КБ приходят раз в 2 с, позиция растёт ровно, обновление
      плейлиста — каждые 6 с.
- [x] Prefetch-пул: unit `hls::` 46/46, сквозные 10/10, весь workspace
      (191 unit `player-core`), clippy и fmt — зелёные. Добавлен
      регрессионный тест `prefetch_downloads_ahead_while_the_reader_is_paused`
      на медленном сервере (300 мс на сегмент).
- [x] Окно 16 с / 8 воркеров (2026-11-23): unit-тесты `prefetch_span`
      (2 с → 8, 6 с → 3, нулевые `#EXTINF` → предел), ленивый старт
      подтверждён тестом перемотки. Живой 1080p60-прогон с новыми
      настройками (higanbanban, 43 с): `shown=2613`, `dropped=0`,
      `fps≈60`, `lag≈0`, `retries=0`, `slow_ms` всего 5 с. Первый прогон
      пришёлся на провал CDN (14 с без данных) — это сеть, не код.
- [x] Обычный HTTP/MP4 (2026-11-23): selstorage-ссылка открывается и играет,
      но бэкенд холодного хранения отдаёт ~61 КБ/с (256-КБ диапазон до
      80 с, TTFB до 3,8 с, `ffprobe` не успевает за 90 с). HTTP-путь с
      этапа 4 не менялся, `cache.rs` в диффе нет; регрессии нет.
- [x] Apple fMP4 master (demuxed, 2026-11-23): `demuxed=true`,
      `live=false`, `duration=00:10:00`, `1920x1080@60`; обе дорожки
      открыты, звук доходит до устройства (`peak=0.71`). За 53 с:
      `shown≈2574`, `dropped=0`, `fps≈60`, `lag≈5 мс`, `retries=0`.
      Prefetch — по 4 воркера на дорожку.
- [x] A/V на Apple VOD (demuxed): старт, seek и конец проверены
      владельцем в GUI 2026-11-23 — звук и кадр меняются вместе, таймер
      останавливается на 10:00. Правки `waiting_first_frame` и клампа
      позиции — в аудите ниже.
- [ ] YouTube live через master: HLS есть (`yt-dlp -f 270/233` даёт
      `hls_variant` с `demuxed=1`), но `manifest.googlevideo.com` из этой
      сети сбрасывает соединение/таймаутит (`curl`: reset; плеер: `сеть:
timeout: per read`). yt-dlp проходит только своим клиентом; браузерные
      заголовки не помогают — внешний блокер (вопрос 7).
- [x] Закрытие во время загрузки сегментов — без паники (автотест).
- [x] docs обновлены (SPEC §2.7: prefetch и `Buffering`); версия/тег —
      ждут решения владельца.

#### Аудит 5.6 (2026-11-23)

- **Причина бесконечной загрузки Twitch:** мы всегда отдавали FFmpeg
  `StreamIo::from_read_seek`, даже при `MediaReader::is_seekable() == false`.
  На live fMP4 mov-демуксер пытался узнать размер/конец и читал сегменты
  до бесконечности. Теперь не-seekable источник идёт через
  `StreamIo::from_read`; реальная перемотка HLS всё равно идёт через
  `Media::seek`.
- **Причина `shown=0` после открытия:** у live fMP4 PTS (tfdt) не
  начинаются с нуля, а видео-часы ядра — от 0 по аудио. `VideoDecoder`
  для live HLS вычитает `AVStream.start_time`; для VOD/файлов — 0.
  Для live fMP4 со сдвигом 1000 с добавлен regression-тест.
- `StreamIo::from_read` теперь используется и для HTTP без `Range`;
  все тесты этапов 1–4 зелёные.
- Телеметрия: debug-логи показывают последовательности HLS-сегментов,
  `stream_start`/`pts_origin`, обновления плейлиста и
  `состояние синхронизации` (`shown/dropped/fps/lag/network`); URL
  сегмента в лог не пишется (там могут быть токены).
- **Prefetch-пул валидирован.** Проверено тестами: «один запрос на
  сегмент» (`muxed_hls_vod_plays_seeks_pauses_and_ends`), видео-сегменты
  demuxed — ровно по разу, отмена и live-refresh не ломаются, VOD после
  `Ended` не виснет, окно не превышает `MAX_WINDOW_BYTES`.
- **Окно по времени (2026-11-23).** `PREFETCH_WINDOW = 16 с`,
  `PREFETCH_MAX_SEGMENTS = 8`; выборка вынесена в `prefetch_span`. На
  Twitch (2 с) это 8 сегментов, на Apple (6 с) — 3; нулевые `#EXTINF` и
  хвост плейлиста упираются в предел. Память: 8 сегментов в окне + до 8
  в полёте ≈ 31 МБ на 1080p60-дорожку; `MAX_WINDOW_BYTES` считает только
  скачанные, поэтому в пике возможно ~2× лимита воркеров.
- **Ленивый старт воркеров.** `Once` в `register_cursor`: до первого
  читателя сеть не трогается. Это убрало гонку в
  `source_seek_time_restarts_readers_at_the_target_segment` (eager-prefetch
  с 8 воркерами успевал скачать начало VOD до `seek_time`).
- **Apple fMP4 (demuxed), 2026-11-23.** В этот раз CDN не тормозил:
  master открылся сразу, `duration=600 с`, 1080p60, обе дорожки; звук
  дошёл до устройства (`peak=0.71`), 53 с — `dropped=0`, `retries=0`.
  Seek и A/V sync на слух остаются за владельцем (CLI не умеет `Seek`
  в режиме воспроизведения; автотесты покрывают переоткрытие).
- **YouTube live.** HLS-форматы есть и `yt-dlp -f 270` отдаёт
  `hls_variant` с `demuxed=1`, но `manifest.googlevideo.com` из этой сети
  сбрасывает соединение curl (даже с браузерными заголовками), а плеер
  получает `сеть: timeout: per read`. yt-dlp проходит только своим
  клиентом — это TLS/бот-защита Google, а не ошибка HLS-слоя. Нужен
  отдельный клиент/сессия, иначе остаётся ограничением (вопрос 7).
- **Блокер валидации — «исчезающая» ошибка.** Декодер забирает ошибку из
  общего `Arc<Mutex<Option<HttpFailure>>>` (`take_http_failure`), после
  чего `recorded_failure()` возвращал `None`, и prefetch-воркер снова
  качал упавший сегмент: тест `/seg-error.m3u8` не получал `Event::Error`
  (запросы шли бесконечно). Исправление: `TrackSource::sticky_failure`
  хранит первую ошибку для планировщика и новых читателей, а слот для
  декодеров по-прежнему отдаётся один раз.
- Вопрос 8 закрыт: prefetch дал `dropped = 3` вместо 343, `retries = 0`;
  ABR/буфер в отдельный этап не выносим.

#### Правки A/V после приёмки (2026-11-23)

По отчёту владельца на Apple VOD:

1. **Звук раньше видео на старте.** `Play` включал `rodio` сразу, а первый
   кадр приходил позже (декодер, seek-переоткрытие, сеть). Исправление:
   `waiting_first_frame` — после `apply`/`seek` звук ждёт первый кадр;
   первый кадр показывается по `Show` без ожидания часов, затем sink
   запускается. Гейт снимается и при `video_finished`, чтобы битая
   видеодорожка не глушила звук навсегда.
2. **После перемотки звук есть, кадр старый.** То же окно: `seek` делал
   `restart_playback` и включал звук до первого кадра новой позиции. Теперь
   `restart_playback` учитывает `waiting_first_frame`, а кадр после
   перемотки уходит в UI первым.
3. **После 10 минут звук кончается, видео идёт.** Позиция в UI считалась
   только по аудио-часам и замирала на последнем аудио-отсчёте, пока
   видео-хвост продолжается. Теперь при `tail_started` `report` берёт
   `VideoTimer`. Длину хвоста видно по новым логам
   `аудио закончилось`/`видео закончилось` (`position`, `last_shown`).
4. **Таймер шёл после конца.** После `Ended` видео-таймер продолжал
   считать, и позиция уходила за длительность (10:10 у 10-минутного
   ролика). Теперь таймер останавливается на `Ended`, а любая позиция
   клампится к `Media::duration`; при перемотке после `Ended` таймер
   снова запускается.

Проверка: `cargo test --workspace` — 191 unit + 10 integration + 68
`player-gpui` + 4 `player-app` + 8 doc; clippy и fmt чистые. Гейт старта
подтверждён логом на Apple (`первый кадр показан: запускаем звук`, ~6 мс
после `Ready`). Владелец перепроверил в GUI 2026-11-23: старт, перемотка
и конец — звук и кадр идут вместе, таймер упирается в 10:00 и стоит. ✅

## Файлы

- `player-core`:
  - `hls.rs` (✅ 5.2–5.6): парсер (playlist, `#EXT-X-MEDIA`,
    `PROGRAM-DATE-TIME`), выбор варианта/аудио, `HlsSource`,
    `TrackSource`, загрузчик сегментов, `HlsReader`; `HlsSource::seek_time`,
    флаг `DISCONTINUITY`, без `#[allow(dead_code)]` в `lib.rs`; пул
    prefetch-воркеров (`PREFETCH_WINDOW`, `PREFETCH_MAX_SEGMENTS`,
    `SegmentStep`, `prefetch_span`, `prefetch_worker`, ленивый старт через
    `Once`) и `sticky_failure` — «липкая» ошибка для планировщика.
  - `reader.rs` (✅): `Track`, `SharedSource`; `open_reader` с дорожкой.
  - `http.rs` (✅): `agent`, `with_retries`, `HttpError`/статусы открыты для
    `hls.rs`; `HttpFailure::Hls { url, reason }`.
  - `error.rs` (✅): `PlayerError::Hls { target, reason }` + тест текста.
  - `lib.rs` (✅): `mod hls` в релизной сборке.
  - `media.rs` (✅ 5.3): создание `HlsSource`, `live`/`duration`/`seekable`,
    `Media::seek`, сумма `NetworkStats`.
  - `video.rs` (✅ 5.3): `Track::Video`, `pts_shift_ms`, `skip_to`,
    `DISCONTINUITY`-flush, отказ только для локального `.m3u8`.
  - `audio.rs` (✅ 5.3): `Track::Audio`, `skip_to`,
    `DISCONTINUITY`-flush.
  - `engine.rs` (✅): перемотка через `Media::seek`.
  - `versions.rs` — проверка возможностей FFmpeg (5.0).
  - `tests/hls.rs` (✅ 5.5–5.6): 10 сквозных `Player`/`Event`-тестов на
    `127.0.0.1`, генерация ассетов `ffmpeg`, live-сервер с растущим окном;
    `AssetServer::set_delayed` и регрессия
    `prefetch_downloads_ahead_while_the_reader_is_paused`; метка кейса в
    сообщении `expect_error`.
- `player-gpui`: `video_player.rs` — `live` из `Ready`, защита `Seek`;
  `video_view.rs` — плашка `LIVE`; `app.rs` — панель URL для `.m3u8`;
  `controls.rs` — без правок (серый слайдер уже от `seekable`/`duration`).
- `player-cli`: печать `live: true` при `Ready`; HLS-ссылка как аргумент.
- 7.0 (сделано): `player-core/src/http_options.rs` (новый), `http.rs`
  (`HttpConfig.headers`, единый `get`), `hls.rs` (плейлисты/сегменты),
  `media.rs`/`player.rs`/`engine.rs`/`command.rs`/`reader.rs`/`video.rs`
  (`open_with_options`/`OpenWith`/`open_source_with_options`);
  `player-cli`/`player-app` — флаги; `player-gpui` — `PlayerOptions.http`;
  `docs/PLAN.md` — план этапа.
- `Cargo.toml`: этап 5 без новых зависимостей.
- `crates/player-app/Cargo.toml` (6.0): `autobins = false`, `[[bin]]`
  с именем `vgpu` и путём `src/main.rs`.
- `crates/player-app/src/main.rs` (6.0): тексты логов/ошибок — `vgpu`;
  разбор URL/пути не менялся.
- `crates/player-app/tests/binary.rs` (6.0): integration-тест, что Cargo
  выпускает `vgpu` (через `CARGO_BIN_EXE_vgpu`).
- `installer/vgpu-player.iss` (6.0): `ChangesEnvironment`, `[InstallDelete]`
  старого `player-app.exe`, `[Code]` для PATH (добавление, удаление,
  идемпотентность, лимит 2047), все иконки/ассоциации — `vgpu.exe`.
- `installer/AppFiles.wxs` (6.0): `<Environment ... System="no">`
  и `<RemoveFile Name="player-app.exe">`; `installer/vgpu-player.wxs` —
  ярлык и ассоциации на `vgpu.exe`.
- `installer/register-associations.cmd` (6.0): `vgpu.exe` и команды
  `path` / `path remove` для портативной сборки.
- `justfile` (6.0): копирование `target/release/vgpu.exe`, удаление
  старого `player-app.exe` из портативной папки.
- `README.md` (6.0): сборка, установка, `vgpu "<URL>"`, PATH и кавычки.
- `target/hls-tests`: реальные плейлисты и `ffmpeg`-артефакты (в git не
  попадают). `target/hls-research`: исходники FFmpeg/hls.js/ExoPlayer/Shaka
  для сверки (в git не попадают).

## Тесты

- Парсер: все теги из 5.1 + `#EXT-X-MEDIA`/`AUDIO`/`PROGRAM-DATE-TIME`,
  относительные URL, query, ошибки; реальные Apple/YouTube/Twitch тексты.
- Локальный HLS VOD muxed (TS): master → variant → segments, длительность,
  seek, пауза, `Ended`, один запрос на сегмент.
- VOD demuxed (fMP4 и TS): видео + audio-rendition, seek, A/V sync,
  `NetworkStats` — сумма.
- Live muxed/demuxed: `live = true`, `seekable = false`, обновление окна,
  старт с хвоста, закрытие без паники.
- Ошибки: 404/500, обрыв, AES-128, битый плейлист, неизвестный `AUDIO`.
- Prefetch: при паузе скачивается окно вперёд на медленном сервере;
  один запрос на сегмент; отмена/live-refresh/VOD-конец не виснут;
  ошибка сегмента доходит до UI один раз и не перезапускается;
  `prefetch_span` считает секунды (2 с → 8, 6 с → 3), упирается в предел
  и в хвост плейлиста, не скачивает начало VOD до первого читателя.
- Регрессии этапов 1–4: 191 unit + 10 integration `player-core`
  (+2 возможностей FFmpeg, +21 парсера/окна HLS, +22 в 5.2, +5 в 5.3,
  +9 в 5.5, +1 live fMP4 с PTS-сдвигом, +1 prefetch и +2 окна в 5.6)
  - 68 `player-gpui` (+1 к 5.4: плашка `LIVE`) + 4 `player-app`
  - 8 doc-тестов.
- 6.0: `vgpu` без аргумента и с URL (4 unit-теста), имя бинарника —
  `crates/player-app/tests/binary.rs`; `where vgpu` через PATH, компиляция
  Inno и WiX; установка, upgrade, удаление и чистота `PATH` — ручная
  матрица.
- 6.7: разбор аргументов `vgpu` (12 тестов) и `player-cli` (8 тестов),
  `cli_volume_overrides_saved_setting` в `player-gpui`; ручная проверка
  release-help — в «Следующей сессии».
- 7.0 (сделано): `HttpOptions` (валидация/дефолты/переопределение),
  заголовки на probe, `Range`, потоке без `Range`, HLS master/media/`MAP`/
  сегментах/prefetch; 403 без `Referer` и успешный `Ready` с ним;
  парсеры флагов `vgpu`/`player-cli` (16 `player-app`, 11 `player-cli`).
  Итог: 208 unit + 10 integration `player-core`, 69 + 16 + 1 + 11,
  8 doc-тестов; clippy и fmt — зелёные.

## Проверка (повторить перед приёмкой)

```bash
RUSTFLAGS="-Clinker=rust-lld" RUSTDOCFLAGS="-Clinker=rust-lld" cargo test --workspace
RUSTFLAGS="-Clinker=rust-lld" cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
RUSTFLAGS="-Clinker=rust-lld" cargo run -q -p player-cli -- <hls-url>
```

После установки — только в **новой** консоли:

```bat
where vgpu
vgpu "https://…"
vgpu
```

Кавычки обязательны для URL с `&` (в `cmd` это разделитель команд).
Ручная матрица установщиков — в 6.0.

## Тестовые ссылки

- **VOD HLS (проверена 2026-11-23, подписана, истекает ≈ 2026-11-27):**
  `https://river-4-475.rtbcdn.ru/hls-vod/6ive4Lpe4RnuiuYTfYtSQw/1795806421/2922/0x5000c500e9ebda34/815dd847c8014f91af8186212a9c72f7.mp4.m3u8?i=360x640_735`
  `200`, `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-ENDLIST`, TS-сегменты по ~5,067 с,
  звук и видео в одном сегменте (`v1-a1.ts`). На 2026-11-23: 46 сегментов,
  228,3 с. Годится для ручной проверки 5.6; после 27 ноября нужна свежая
  подпись.
- **Apple fMP4 (VOD, проверена 2026-11-23):** master
  `https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8` —
  24 варианта, раздельные `#EXT-X-MEDIA:TYPE=AUDIO` (aud1/aud2/aud3), fMP4
  с `#EXT-X-MAP`; годится для проверки demuxed-аудио. Вариант/аудио для
  ручной проверки — `v9/prog_index.m3u8`, `a1/prog_index.m3u8`.
- **YouTube live (файлы владельца, 2026-11-23):**
  `target/hls-tests/index-video.m3u8` и `index-audio.m3u8` — реальные
  media-плейлисты (721 сегмент, DVR ~1 ч, окна сдвинуты). Это не master:
  для end-to-end нужен master с `#EXT-X-MEDIA`. Получить:
  ```bash
  yt-dlp --print "%(manifest_url)s" "https://www.youtube.com/watch?v=nD1RB9cv0QA"
  ```
  Сохранить тело master в `target/hls-tests/index-master.m3u8` (или из
  браузера DevTools). Из этой сети `googlevideo` может отдавать reset —
  тогда только тексты от владельца.
- **Twitch live (muxed, проверен 2026-11-23):** канал
  `https://www.twitch.tv/streamdatabase`, формат `720p`; URL брать через
  `yt-dlp -g -f 720p https://www.twitch.tv/streamdatabase`, сохранять в
  `target/hls-tests/real-live-twitch.m3u8`. 15 сегментов по 2 с, окно 30 с.
- **rtbcdn VOD (muxed TS):** ссылка выше; на 2026-11-23 — 46 сегментов,
  228,3 с.
- Для CI — локальная генерация: `ffmpeg -f hls` (TS и fMP4) в `assets/test`;
  внешняя сеть в тестах запрещена. Нужны и master-плейлист (варианты), и
  media (`#EXT-X-ENDLIST`).

## Следующая задача — PATH установщиков и запуск из консоли (реализовано, ждёт приёмки)

### 6.0 Установщики: PATH и запуск из консоли (0,25–0,5 дня)

**Статус (2026-11-23):** реализовано — release-бинарник `vgpu.exe`,
PATH в Inno/MSI, `register-associations.cmd path`; оба установщика
компилируются. Локально: `cargo test --workspace` — 191 + 10 + 68 +
5 (4 unit + 1 integration) + 8 doc-тестов, clippy и fmt зелёные;
`where vgpu` через PATH находит `dist\vgpu-player\vgpu.exe`.

**Приёмка Inno (2026-11-23):** установлено в `D:\test\vgpu-player`.
`PATH` (`REG_EXPAND_SZ`) получил ровно одну запись, `where vgpu` находит
её; `vgpu`, `vgpu <файл>` и `vgpu <HLS URL>` запускаются; ассоциации
видео/аудио и `Applications\vgpu.exe` указывают на `vgpu.exe`, старого
`player-app.exe` и его ключа нет. Удаление установщика ещё не проверено.

**Цель:** после установки Inno/MSI в новой консоли работает
`vgpu "https://…"` без указания полного пути; `vgpu` без аргумента
открывает пустое окно.

- [x] Inno (`installer/vgpu-player.iss`):
  - `[Setup] ChangesEnvironment=yes` — установщик рассылает
    `WM_SETTINGCHANGE` после изменения;
  - `[Code]` `CurStepChanged(ssPostInstall)`: читает
    `HKCU\Environment\Path` (`REG_EXPAND_SZ`), добавляет `{app}` через `;`
    только если записи нет (регистр, завершающий `\`), при отсутствии
    значения пишет только `{app}`; `RegWriteExpandStringValue`; лимит 2047;
  - `[Code]` `CurUninstallStepChanged(usPostUninstall)`: удаляет ровно свою
    запись и не трогает остальные; пустое значение удаляет;
  - `{app}` имеет вид `%LOCALAPPDATA%\Programs\vgpu-player`, поэтому
    пробелы допустимы, а `;` в пути невозможен.
- [x] MSI (`installer/AppFiles.wxs` или `vgpu-player.wxs`) — элемент внутри
      per-user компонента с HKCU-keypath:

  ```xml
  <Environment Id="PathEnv" Name="PATH" Value="[INSTALLFOLDER]"
               Part="last" Action="set" System="no" Permanent="no" />
  ```

  Проверить upgrade и uninstall на чистой машине.

- [x] Портативная сборка: `register-associations.cmd path`
      добавляет/убирает каталог в `HKCU\Environment`, без аргумента поведение
      прежнее; путь в `PATH` не дублируется.
- [x] README: пример `vgpu "https://…"`, про кавычки, новую
      консоль и GUI-подсистему (приглашение не блокируется).
- [ ] Ручная проверка на чистой Windows:
  1. установка → новая консоль → `where vgpu` → `vgpu "<URL>"`;
  2. чужие записи `PATH` сохранены, дубликата нет, повторная установка
     идемпотентна;
  3. удаление → запись исчезла, остальные значения остались;
  4. путь с пробелами и кириллицей; `PATH` у лимита 2047;
  5. без прав администратора; уже открытая консоль — старое значение
     (ожидаемо), новая — новое.
- [x] Автотестов для `.iss`/`.wxs` нет: проверка сборкой `just installers`
      и компиляцией Inno/WiX; ручная матрица выше.

## Открытые вопросы

Все решения 46–65 утверждены владельцем 2026-11-23, включая demuxed-аудио.
Решение 66 (PATH) и 67 (`vgpu`) реализованы; 68–73 (7.0) утверждены
2026-11-25, реализация не начата. Незакрытые детали — вопрос 9.

1. **Ссылки для ручной проверки:** свежий rtbcdn VOD и Apple fMP4;
   YouTube master — если нужен end-to-end. CI — только локальные ассеты
   и `127.0.0.1`.
2. ~~Версия/тег~~ — закрыто: `0.3.0`, тег `v0.3.0` (2026-11-23).
3. Во время полного прогона `player-core` один раз упал
   `cache::tests::cache_reports_resident_bytes` (64 vs 128), при повторном
   запуске и отдельном прогоне проходит — тайминг фонового загрузчика,
   к правкам 5.0/5.1 отношения не имеет. Не чиним без отдельного решения.
4. **Строгость `#EXT-X-BYTERANGE`.** Сделать ли ошибкой пропущенный
   `offset`, если предыдущий сегмент — другой ресурс (по RFC невалидно)?
   Сейчас продолжаем от предыдущего конца.
5. **Локальные `.m3u8`** (`Source::File`): в SPEC зафиксировано
   `UnsupportedHls` для этапа 5. Если нужны — отдельным решением, с
   нормализацией Windows-путей.
6. **LL-HLS** (`#EXT-X-PART`, `#EXT-X-PRELOAD-HINT`, `#EXT-X-SKIP`) — вне
   первой версии; подтверждено, что провайдеры с LL-HLS не входят в
   приёмку.
7. **YouTube end-to-end:** `yt-dlp -f 270` даёт `hls_variant` с
   `demuxed=1`, но `manifest.googlevideo.com` сбрасывает соединение
   curl/`ureq` (таймаут `per read`), а yt-dlp проходит только своим
   клиентским стеком. Нужен совместимый TLS/HTTP-клиент или сессия
   браузера — отдельное решение, в этап 5 не входит.
8. ~~Синхронная загрузка HLS-сегментов~~ — **закрыто 2026-11-23:**
   prefetch-пул валидирован, live HLS даёт `dropped = 3` вместо 343
   и `retries = 0`. ABR/буфер в отдельный этап не выносим.
9. **PATH установщиков:** проверить на чистой Windows per-user
   `HKCU\Environment`, лимит 2047 и удаление только своей записи
   (Inno код и WiX `Environment`; портативно — `register-associations.cmd
path`).
10. ~~**RuTube (7.0):** ...~~ — **закрыто 2026-11-25:** владелец
    подтвердил работу; заголовки уходят на все запросы источника.
11. **CLI `--header` (7.0):** произвольные заголовки не выводим; вернуться,
    если появятся источники с `Origin`/`Cookie` в media-запросах.

## Следующая сессия

**7.0 (принято):** реализовано по `docs/PLAN.md`, тесты/clippy/fmt
зелёные, версия `0.3.4`, установщики пересобраны, RuTube подтверждён
владельцем 2026-11-25. Коммит `2313580` и тег `v0.3.4` в `origin`.

Ниже — незакрытые хвосты 6.0; они не блокируют 7.0.
Реализация 6.0 сделана; установка Inno проверена на этой машине.
Осталось проверить удаление и MSI на чистой Windows.

1. **Inno:** удаление — `PATH` и записи ассоциаций должны вернуться к
   прежнему состоянию; повторная установка и upgrade с `player-app.exe`.
2. **MSI на чистой Windows:** установка → новая консоль → `where vgpu` →
   `vgpu "<URL>"`; удаление; чужие записи `PATH`.
3. **Портативная сборка:** `register-associations.cmd path` и
   `path remove`; ассоциации на `vgpu.exe`.
4. **Регрессии:** `cargo test --workspace`, clippy, fmt — пройдены
   перед коммитом 7.0.
5. **Коммит/тег** — `v0.3.4` после 7.0; ветка `feat/url-vipeo`, `main`
   не трогаем.
6. **6.7:** `vgpu.exe --help` и `vgpu "…" --volume 10` в release из `cmd`
   и PowerShell (приглашение возвращается раньше текста) и
   `vgpu --help > help.txt` (перенаправленный вывод).
