# player-gpui

Библиотека UI видеоплеера на [GPUI](https://github.com/longbridge/gpui-fast)
(`gpui-fast`). Внутри — три независимые части:

| Часть                        | Что делает                                                                                                             |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `VideoPlayer`                | сущность GPUI: состояние воспроизведения и команды, сама общается с `player-core`                                      |
| `video_view(&player, cx)`    | виджет кадра; возвращает обычный `Div`, поэтому стилизуется как любой элемент; для файла без видео — спектроанализатор |
| `controls_view(&player, cx)` | необязательная панель: кнопки-иконки (play/pause, звук, fullscreen) и слайдеры                                         |

Движок (`player-core`) — отдельный крейт без GPUI: демультиплексирование,
декодирование, часы и синхронизация.

## Состояние

Реализованы этапы 0–13: звук и видео, A/V-синхронизация по аудио-часам,
перемотка, громкость, fullscreen, горячие клавиши, состояния ошибки/завершения
с кнопкой «Повторить», пул буферов кадров и открытие файла через системный
диалог GPUI по `O`/`Ctrl+O`, из меню и перетаскиванием в окно — без `rfd`.
Кнопки панели и оверлея — иконки (встроенные монохромные SVG); файл без видео
показывает спектроанализатор — 48 логарифмических столбиков в цветах иконки
приложения, с компенсацией наклона спектра. Открытый файл сразу играет
(`autoplay`, по умолчанию `true`; `set_autoplay(false)` — открывать на паузе с
превью первого кадра). В демо-окне есть меню «⋯»: открыть файл, информация о
файле, оверлей статистики, «Запускать сразу», «О программе» с версиями FFmpeg
из рантайма, автоскрытие панели.

Готовая обёртка [`run_app`] хранит автовоспроизведение, оверлей, автоскрытие и
громкость в `%APPDATA%\vgpu-player\settings.cfg`; чужому приложению это не
навязывается — оно само решает, читать ли файл и где хранить настройки.

## Логи

Библиотеки пишут диагностику через `tracing`, но подписчик в них не входит:
его ставит приложение-хозяин. `player-app` и `player-cli` включают
`tracing-subscriber` с `RUST_LOG` (по умолчанию `warn`, ключевые события —
`RUST_LOG=info`).

## Быстрый старт

```toml
[dependencies]
gpui = { package = "gpui-fast", version = "0.1.16" }
player-gpui = { path = "../vgpu-player/crates/player-gpui" }
```

```rust
use gpui::{App, Context, Render, Window, div, prelude::*};
use player_gpui::{VideoPlayer, controls_view, video_view};

struct Screen {
    player: gpui::Entity<VideoPlayer>,
}

impl Screen {
    fn new(path: std::path::PathBuf, cx: &mut App) -> Self {
        Self {
            player: VideoPlayer::create(path, cx),
        }
    }
}

impl Render for Screen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(video_view(&self.player, cx).flex_1().rounded_md())
            .child(controls_view(&self.player, cx))
    }
}
```

Важно: `video_view` не задаёт размер и скругления сам — их задаёт вызывающая
сторона (`size_full`, `absolute`, `rounded_md` и т.д.), как для обычного `div`.

## Как встроить в чужое gpui-приложение

Нужны три шага:

1. создать сущность плеера — `VideoPlayer::create(path, cx)` или
   `cx.new(VideoPlayer::new)` и затем `open(path, cx)`;
2. вставить `video_view(&player, cx)` в свою разметку;
3. управлять воспроизведением методами `VideoPlayer` из своих обработчиков.

`controls_view` при этом необязателен: это удобная готовая панель, а не часть
ядра. Свои кнопки — обычные элементы GPUI, которые зовут методы плеера.

`open`/`create` по умолчанию сразу начинают воспроизведение; чтобы открывать
файл на паузе с превью первого кадра, вызовите `set_autoplay(false)`.

Минимальное приложение с собственным окном и кадром:

```rust,no_run
use player_gpui::gpui::{App, Context, Render, Window, WindowOptions, div, prelude::*};
use player_gpui::gpui_platform;
use player_gpui::{VideoPlayer, video_view};

struct Screen {
    player: gpui::Entity<VideoPlayer>,
}

impl Screen {
    fn new(path: Option<std::path::PathBuf>, cx: &mut App) -> Self {
        let player = match path {
            Some(path) => VideoPlayer::create(path, cx),
            None => cx.new(VideoPlayer::new),
        };
        Self { player }
    }
}

impl Render for Screen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .size_full()
            .child(video_view(&self.player, cx).size_full())
    }
}

fn main() {
    let path = std::env::args_os().nth(1).map(std::path::PathBuf::from);

    gpui_platform::application().run(move |cx: &mut App| {
        if let Err(error) = cx.open_window(WindowOptions::default(), move |_window, cx| {
            cx.new(|cx| Screen::new(path.clone(), cx))
        }) {
            eprintln!("окно не открылось: {error}");
        }
    });
}
```

Цикл событий запускается через реэкспорт `player_gpui::gpui_platform`: так у
потребителя и библиотеки гарантированно одна версия GPUI и один платформенный
слой. Пример помечен `no_run`, потому что открывает настоящее окно; полностью
рабочий запуск — в `examples/embed_in_app.rs`.

Свои контролы вызывают те же методы, что и готовая панель:

```rust
use std::time::Duration;

use player_gpui::VideoPlayer;
use player_gpui::gpui::{App, Entity};

fn own_controls(player: &Entity<VideoPlayer>, cx: &mut App) {
    player.update(cx, |video, cx| video.toggle_playback(cx));
    player.update(cx, |video, cx| video.set_volume(0.5, cx));
    player.update(cx, |video, cx| video.seek(Duration::from_secs(30), cx));
    // системный диалог выбора файла (без `rfd`)
    player.update(cx, |video, cx| video.open_dialog(cx));
}
```

Чтение состояния — без блокировок и без опроса: `player.read(cx).state()`,
`.duration()`, `.position()`, `.volume()`, `.error()`. Перерисовка приходит
сама, потому что `VideoPlayer` уведомляет подписчиков при изменениях.

```rust
use player_gpui::VideoPlayer;
use player_gpui::gpui::{App, Entity};

fn read_state(player: &Entity<VideoPlayer>, cx: &App) {
    let video = player.read(cx);
    let _state = video.state();
    let _position = video.position();
    let _duration = video.duration();
    let _spectrum = video.spectrum(); // Some(SpectrumBars) для файла без видео
}
```

Оверлеем статистики управляет сам плеер, поэтому он есть и у потребителя:

```rust
use player_gpui::VideoPlayer;
use player_gpui::gpui::{App, Entity};

fn stats(player: &Entity<VideoPlayer>, cx: &mut App) {
    player.update(cx, |video, cx| video.toggle_stats(cx)); // или set_stats_visible(bool, cx)
    let _visible = player.read(cx).stats_visible();
}
```

Кнопка fullscreen в `controls_view` переключает окно сама, но иконка берётся
из состояния плеера: если у вас своё окно, синхронизируйте его из `render`.

```rust
use player_gpui::VideoPlayer;
use player_gpui::gpui::{Context, Entity, Window};

struct Screen {
    player: Entity<VideoPlayer>,
}

impl Screen {
    fn sync_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fullscreen = window.is_fullscreen();
        self.player
            .update(cx, |video, cx| video.set_fullscreen(fullscreen, cx));
    }
}
```

Метаданные приходят событием `Ready`: открытие асинхронное, и до него
`video_info()`/`media_info()` пусты. В них — контейнер, кодек и разрешение
видео, кодек/каналы/частота звука, а также `seekable()` и `buffering()`.
Панель «Информация о файле» в демо-окне собрана из них; библиотека сама такой
панели не рисует.

## Публичный API

Сущность `VideoPlayer`:

| Группа     | Методы                                                                                                                                                                        |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Создание   | `new(cx)`, `create(source, cx)`                                                                                                                                               |
| Открытие   | `open(source, cx)`, `open_dialog(cx)`, `open_dropped(paths, cx)`                                                                                                              |
| Управление | `play`, `pause`, `toggle_playback`, `seek`, `set_volume`, `toggle_mute`, `restart`, `retry`, `stop`                                                                           |
| Чтение     | `state`, `position`, `duration`, `volume`, `muted`, `source`, `path`, `seekable`, `buffering`, `video_info`, `media_info`, `frame`, `stats`, `spectrum`, `error`, `can_retry` |
| Статистика | `stats_visible`, `set_stats_visible`, `toggle_stats`                                                                                                                          |
| Окно       | `fullscreen`, `set_fullscreen(bool, cx)` — иконка кнопки в `controls_view`                                                                                                    |

`create` и `open` принимают `impl Into<Source>`: путь, `PathBuf` или
`Source::Url(…)`. `path()` возвращает путь только для локального источника.

Функции и типы:

| Экспорт                                              | Назначение                                                                                                   |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `video_view(&player, cx)`                            | кадр с letterbox, превью, оверлеем статистики и кнопкой «Повторить»; для файла без видео — спектроанализатор |
| `controls_view(&player, cx)`                         | готовая панель с кнопками-иконками (включая fullscreen) и слайдерами                                         |
| `run_app(Option<PathBuf>)`                           | готовое демо-окно — для бинарника и примеров                                                                 |
| `run_app_source(Option<Source>)`                     | то же, но источник — путь или URL                                                                            |
| `run_app_source_with(Option<Source>, PlayerOptions)` | то же плюс параметры, которые не берутся из `settings.cfg`                                                   |
| `PlayerOptions`                                      | параметры запуска окна: `volume: Option<f32>` в `0…1` и `http: HttpOptions`; не сохраняются в `settings.cfg` |
| `HttpOptions`, `HttpOptionsError`                    | заголовки HTTP-источника (`Referer`, `User-Agent`) для RuTube и подобных ссылок                              |
| `Source`, `SourceError`                              | разбор строки на файл/URL (реэкспорт `player-core`)                                                          |
| `PlaybackState`                                      | `Idle`, `Loading`, `Ready`, `Playing`, `Paused`, `Ended`, `Failed`; `is_playing()`                           |
| `SpectrumBars`, `SPECTRUM_BARS`                      | уровни визуализатора: 48 чисел `0…1`; `SpectrumBars::levels()`                                               |
| `MediaInfo`, `VideoTrackInfo`, `AudioTrackInfo`      | метаданные открытого файла: контейнер, кодеки, параметры дорожек                                             |
| `LaunchError`, `WINDOW_TITLE`                        | ошибка запуска окна и заголовок демо-окна                                                                    |
| `gpui`, `gpui_platform`                              | реэкспорт, чтобы у библиотеки и потребителя была одна версия GPUI                                            |

## Примеры

- `crates/player-gpui/examples/embed_in_app.rs` — собственное окно,
  `video_view`, свои кнопки (play/pause, ±5 с, «Открыть…»), без
  `controls_view`; читает `state()`, `position()`, `duration()`;
- `crates/player-gpui/examples/minimal.rs` — минимум кода: `run_app`.

```bash
# один раз: DLL FFmpeg нужны и рядом с примерами
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/examples/

cargo run -p player-gpui --example minimal -- assets/test/sync_test.mp4
cargo run -p player-gpui --example embed_in_app -- assets/test/sync_test.mp4
# то же из корня репозитория:
# just example-minimal
# just example-embed
```

Демо-окно открывает источник из аргумента: путь, `file://` или `https://`.
Сетевой читатель появится на этапе 3; до него URL доходит до состояния
`Loading` и показывает понятную ошибку.

## Подключение по git

Репозиторий: <https://github.com/m1Ck1-pypy/vgpu-player>.

1. Поставить тег, чтобы зависимость была воспроизводимой:
   ```bash
   git tag v0.4.1
   git push origin v0.4.1
   ```
2. В чужом проекте подключить тег (предпочтительно) или ветку:
   ```toml
   [dependencies]
   player-gpui = { git = "https://github.com/m1Ck1-pypy/vgpu-player.git", tag = "v0.4.1", package = "player-gpui" }
   # или
   player-gpui = { git = "https://github.com/m1Ck1-pypy/vgpu-player.git", branch = "main", package = "player-gpui" }
   ```
3. Обновлять зависимость:
   ```bash
   cargo update -p player-gpui
   ```
4. При добавлении тега не забыть, что тег не поедет сам: обновление зависимости
   у потребителей — сознательное действие, а не «последняя версия».

Поля `version`/`edition`/`rust-version` наследуются из корневого `Cargo.toml`
воркспейса; и `path`-, и `git`-зависимость тянут корневой манифест, поэтому
наследование работает. `publish = false` публикацию на crates.io запрещает, на
git-подключение не влияет.

## Требования

- Rust 1.98+ (требование `gpui-fast`).
- Linux: системные библиотеки GPUI (wayland, xkbcommon, fontconfig, vulkan, alsa).
- Windows/macOS: без дополнительной подготовки для сборки, но на Windows
  нужны DLL FFmpeg (ниже).
- Одна версия GPUI у библиотеки и потребителя. Для этого крейт реэкспортирует
  зависимости: используйте `player_gpui::gpui` и
  `player_gpui::gpui_platform` вместо отдельных зависимостей `gpui-fast` и
  `gpui-fast-platform`.

### Windows: что положить рядом со своим exe

Сборка `ffmpeg-next` динамическая, поэтому рядом с exe потребителя нужны те же
DLL, что и у `player-app`:

```text
avcodec-63.dll  avdevice-63.dll  avfilter-12.dll  avformat-63.dll
avutil-61.dll  swresample-7.dll  swscale-10.dll  dav1d.dll
```

Точные имена версий берите из `.tools/vcpkg/installed/x64-windows/bin`, а не по
памяти: после обновления FFmpeg номер в имени изменится. `avdevice` и
`avfilter` тоже нужны: exe импортирует `avdevice-63.dll`, а та — `avfilter-12.dll`.
`pkgconf-8.dll` из того же каталога — инструмент сборки, в поставку не входит.

```bash
cp .tools/vcpkg/installed/x64-windows/bin/*.dll <папка со своим exe>/
```

Если DLL не найдены, приложение не запустится (Windows покажет ошибку о
недостающей библиотеке) — это не ошибка библиотеки.

### Звук и перетаскивание

- Нужно звуковое устройство по умолчанию: без него `open` вернёт ошибку, и
  `video_view` покажет экран ошибки с кнопкой «Повторить».
- Перетаскивание файлов не работает, если процесс запущен с правами
  администратора: Windows (UIPI) не передаёт OLE-drop из Проводника
  повышенному процессу. Собственной проверки в библиотеке нет; ограничение
  важно, если потребитель сам включает приём `ExternalPaths`.

Подробности о встраивании и упаковке — разделы 14–15 `docs/SPEC.md`.
