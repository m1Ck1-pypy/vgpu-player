//! Демонстрационное окно и запуск приложения.
//!
//! Это удобная обёртка для бинарника и пример того, как собрать плеер из
//! библиотечных частей. Чужому приложению она не обязательна: достаточно
//! [`crate::VideoPlayer`], [`crate::video_view`] и, при желании,
//! [`crate::controls_view`].
//!
//! Здесь же живут вещи уровня окна: горячие клавиши, fullscreen и автоскрытие
//! панели управления.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, AsyncApp, Bounds, ClickEvent, Context, Div, DragMoveEvent, Entity, ExternalPaths,
    FileDropEvent, FocusHandle, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Platform, Render, ScrollDelta, ScrollEvent, SharedString,
    Stateful, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use player_core::{FfmpegVersions, HttpOptions, MediaInfo, Source};
use tracing::{debug, error, info, warn};

use crate::controls::{format_clock, volume_percent};
use crate::icons::{ICON_SIZE, Icon, icon};
use crate::open_panel::{OpenPanel, PanelAction, PanelChoice};
use crate::settings::Settings;
use crate::theme;
use crate::{VideoPlayer, controls_view, video_view};

/// Заголовок окна.
///
/// Отображаемое имя продукта; имена файлов, каталогов и крейтов остаются
/// `vgpu-player`, чтобы не ломать пути и артефакты сборки.
pub const WINDOW_TITLE: &str = "VGPU Player";

/// Через сколько бездействия панель управления скрывается.
const AUTO_HIDE_DELAY: Duration = Duration::from_secs(2);

/// Как часто проверяем, не пора ли скрыть панель.
const AUTO_HIDE_TICK: Duration = Duration::from_millis(250);

/// Сколько висит плашка с громкостью после шага колесом.
const VOLUME_NOTICE_DURATION: Duration = Duration::from_millis(1_500);

/// Шаг перемотки стрелками.
const SEEK_STEP: Duration = Duration::from_secs(5);

/// Окно двойного клика: столько одиночный клик ждёт второй.
///
/// Одиночный клик переключает паузу не сразу: иначе первый клик двойного
/// успевал бы поставить видео на паузу до прихода второго.
const DOUBLE_CLICK_DELAY: Duration = Duration::from_millis(250);

/// Высота панели управления с отступом — насколько меню «⋯» поднимается
/// над нижним краем кадра.
///
/// Панель — наложение: `py_3` (24 px) + строка кнопок (28 px) плюс зазор 8 px.
const CONTROLS_HEIGHT: f32 = 60.0;

/// Ошибка запуска: окно не удалось создать.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct LaunchError {
    message: String,
}

/// Действие, привязанное к клавише.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shortcut {
    TogglePlayback,
    SeekBackward,
    SeekForward,
    ToggleFullscreen,
    OpenFile,
    CloseOverlays,
}

/// Сопоставляет клавишу действию плеера.
///
/// Модификаторы здесь не учитываем: они важны только панели «Открыть…», где
/// `Ctrl+V` вставляет ссылку. Пока поле ввода открыто, этот обработчик вообще
/// не вызывается.
fn shortcut(key: &str) -> Option<Shortcut> {
    match key {
        "space" => Some(Shortcut::TogglePlayback),
        "left" => Some(Shortcut::SeekBackward),
        "right" => Some(Shortcut::SeekForward),
        "f" | "F" => Some(Shortcut::ToggleFullscreen),
        "o" | "O" => Some(Shortcut::OpenFile),
        "escape" => Some(Shortcut::CloseOverlays),
        _ => None,
    }
}

/// Что делает клик по кадру.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickAction {
    TogglePlayback,
    ToggleFullscreen,
}

/// Одиночный клик — пауза или продолжение, двойной — полный экран.
fn click_action(click_count: usize) -> ClickAction {
    if click_count >= 2 {
        ClickAction::ToggleFullscreen
    } else {
        ClickAction::TogglePlayback
    }
}

/// Пора ли скрыть панель управления.
fn should_hide(playing: bool, enabled: bool, idle: Duration) -> bool {
    enabled && playing && idle >= AUTO_HIDE_DELAY
}

/// Следующая громкость после одного щелчка колеса: ровно 1 %.
///
/// Считаем в целых процентах, чтобы серия шагов не накапливала ошибку
/// округления `f32`.
fn volume_after_step(current: f32, up: bool) -> f32 {
    let percent = (current.clamp(0.0, 1.0) * 100.0).round() as i32;
    let next = if up { percent + 1 } else { percent - 1 };
    next.clamp(0, 100) as f32 / 100.0
}

/// Корневое окно демонстрационного приложения.
struct PlayerWindow {
    player: Entity<VideoPlayer>,
    /// Платформа нужна, чтобы прятать курсор, пока панель скрыта по
    /// автоскрытию: вернуть его при движении мыши — задача платформы.
    platform: Rc<dyn Platform>,
    focus_handle: FocusHandle,
    focused: bool,
    controls_visible: bool,
    /// Открыто меню «⋯» в панели управления.
    menu_open: bool,
    /// Показана панель «О программе» поверх кадра.
    about_open: bool,
    /// Показана панель «Информация о файле» поверх кадра.
    file_info_open: bool,
    /// Заголовок, уже установленный окну: `set_window_title` зовём только
    /// при смене, чтобы не трогать окно каждый кадр.
    window_title: String,
    /// Панель «Открыть…»: выбор «Файл»/«URL» и поле ввода ссылки.
    open_panel: OpenPanel,
    /// Текст ошибки разбора ссылки: панель остаётся открытой для правки.
    open_panel_error: Option<String>,
    /// Скрывать панель управления после бездействия (пункт меню).
    auto_hide_enabled: bool,
    /// Файл настроек; `None` — каталог пользователя не определить, и тогда
    /// настройки живут только в памяти.
    settings_path: Option<PathBuf>,
    /// Снимок, который уже лежит в файле (или загружен из него).
    ///
    /// Нужен, чтобы не писать файл на каждый клик и каждый шаг колесом, когда
    /// значение не изменилось.
    saved_settings: Settings,
    /// Версии FFmpeg спрашиваем один раз при старте: они нужны «О программе».
    versions: FfmpegVersions,
    /// Над окном сейчас тащат файлы: показываем подсказку «отпустите файл».
    dragging: bool,
    /// Мышиный клик только что закрыл меню/панели: этот же клик не должен
    /// переключать воспроизведение.
    skip_next_click: bool,
    /// Плашка с громкостью: значение и момент последнего шага колесом.
    volume_notice: Option<(f32, Instant)>,
    /// Отложенное переключение паузы после одиночного клика.
    ///
    /// Задача отменяется, когда приходит второй клик: пара должна уйти в
    /// fullscreen, а не поставить видео на паузу.
    pending_click: Option<Task<()>>,
    last_activity: Instant,
    _auto_hide: Task<()>,
}

impl PlayerWindow {
    fn new(
        source: Option<Source>,
        options: PlayerOptions,
        platform: Rc<dyn Platform>,
        cx: &mut Context<Self>,
    ) -> Self {
        let PlayerOptions { volume, http } = options;
        let settings_path = Settings::default_path();
        let settings = settings_path
            .as_deref()
            .map(Settings::load)
            .unwrap_or_default();

        // Настройки применяем до открытия файла: иначе автовоспроизведение
        // успело бы сработать с умолчаниями.
        let player = cx.new(VideoPlayer::new);
        player.update(cx, |video, cx| {
            video.set_autoplay(settings.autoplay, cx);
            video.set_stats_visible(settings.stats_visible, cx);
            video.set_volume(initial_volume(settings.volume, volume), cx);
        });
        if let Some(source) = source {
            player.update(cx, |video, cx| video.open_with_options(source, http, cx));
        }

        let focus_handle = cx.focus_handle();
        let auto_hide = spawn_auto_hide(cx);

        Self {
            player,
            platform,
            focus_handle,
            focused: false,
            controls_visible: true,
            menu_open: false,
            about_open: false,
            file_info_open: false,
            window_title: WINDOW_TITLE.to_owned(),
            open_panel: OpenPanel::default(),
            open_panel_error: None,
            auto_hide_enabled: settings.auto_hide,
            settings_path,
            saved_settings: settings,
            versions: FfmpegVersions::detect(),
            dragging: false,
            skip_next_click: false,
            volume_notice: None,
            pending_click: None,
            last_activity: Instant::now(),
            _auto_hide: auto_hide,
        }
    }

    /// Записывает текущие настройки в файл.
    ///
    /// Вызывается после каждого изменения настроек — из меню и из изменения
    /// громкости; лишних записей нет, потому что совпадение со снимком в файле
    /// проверяется до записи. Ошибка записи не мешает воспроизведению: она
    /// только логируется, каталог пользователя может быть недоступен.
    fn save_settings(&mut self, cx: &App) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        // Значения копируем, чтобы не держать заимствование плеера на записи.
        let (autoplay, stats_visible, volume) = {
            let video = self.player.read(cx);
            (video.autoplay(), video.stats_visible(), video.volume())
        };
        let settings = Settings {
            autoplay,
            stats_visible,
            auto_hide: self.auto_hide_enabled,
            volume,
        };
        if settings == self.saved_settings {
            return;
        }
        if let Err(error) = settings.save(&path) {
            warn!(path = %path.display(), %error, "не удалось сохранить настройки");
            return;
        }
        self.saved_settings = settings;
    }

    /// Отмечает активность пользователя и возвращает панель.
    fn note_activity(&mut self, cx: &mut Context<Self>) {
        self.last_activity = Instant::now();
        if !self.controls_visible {
            self.controls_visible = true;
            cx.notify();
        }
    }

    fn handle_mouse_move(
        &mut self,
        _event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_activity(cx);
    }

    /// Завершает перетаскивание, даже если кнопку отпустили вне слайдера.
    ///
    /// Заодно снимает подсказку о перетаскивании: если drop почему-то не
    /// пришёл (например, файл отпустили над неокном), она не должна зависнуть.
    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging {
            self.dragging = false;
            cx.notify();
        }
        self.note_activity(cx);
        self.player.update(cx, |video, cx| {
            video.finish_scrub(cx);
            video.finish_volume(cx);
        });
        // Громкость сохраняется здесь, а не по флагу перетаскивания: дочерний
        // слайдер мог уже снять флаг в своём `on_mouse_up`, а этот обработчик
        // вызывается после него. Запись пропускается, если ничего не менялось.
        self.save_settings(cx);
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        // На macOS вставка — Command+V; на Windows и Linux — Control+V.
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;

        // Пока панель «Открыть…» открыта, клавиши принадлежат ей: пробел,
        // стрелки и Esc не должны управлять воспроизведением.
        if self.open_panel.is_open() {
            self.note_activity(cx);
            let action = self
                .open_panel
                .handle_key(key, ctrl, event.keystroke.key_char.as_deref());
            self.apply_panel_action(action, cx);
            // Перерисовка нужна на каждый ввод и каждое переключение пункта:
            // поле рисуем сами, само по себе оно не обновится без `notify`.
            cx.notify();
            return;
        }

        let Some(shortcut) = shortcut(key) else {
            return;
        };

        self.note_activity(cx);
        match shortcut {
            Shortcut::TogglePlayback => {
                self.player
                    .update(cx, |video, cx| video.toggle_playback(cx));
            }
            Shortcut::SeekBackward => self.seek_relative(SEEK_STEP, false, cx),
            Shortcut::SeekForward => self.seek_relative(SEEK_STEP, true, cx),
            Shortcut::ToggleFullscreen => window.toggle_fullscreen(),
            Shortcut::OpenFile => self.show_open_panel(cx),
            Shortcut::CloseOverlays => {
                let closed = self.close_overlays(cx);
                if !closed && window.is_fullscreen() {
                    window.toggle_fullscreen();
                }
            }
        }
    }

    /// Открывает и закрывает меню «⋯».
    fn toggle_menu(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = !self.menu_open;
        // Меню и регулятор громкости не должны висеть одновременно.
        self.player
            .update(cx, |video, cx| video.close_volume_panel(cx));
        self.note_activity(cx);
        cx.notify();
    }

    /// Клик по кадру убирает всё, что открыто поверх него.
    ///
    /// Если меню/панели были открыты, следующий `on_click` только закроет их,
    /// а не переключит паузу: иначе клик «убери меню» заодно ставил бы видео
    /// на паузу.
    fn handle_video_click(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.skip_next_click = false;
        self.note_activity(cx);
        if self.close_overlays(cx) {
            self.skip_next_click = true;
        }
    }

    /// Клик по кадру: один — пауза или продолжение, два — полный экран.
    ///
    /// Одиночный клик откладывается на [`DOUBLE_CLICK_DELAY`], чтобы первый
    /// клик двойного не ставил видео на паузу. `on_click` приходит на каждый
    /// клик серии, поэтому счётчик и таймер вместе дают нужную пару.
    fn handle_video_click_release(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.skip_next_click {
            self.skip_next_click = false;
            return;
        }

        self.note_activity(cx);
        match click_action(event.click_count()) {
            ClickAction::TogglePlayback => {
                self.pending_click = Some(spawn_pending_playback(cx));
            }
            ClickAction::ToggleFullscreen => {
                // Второй клик отменяет ожидающую паузу: иначе двойной клик
                // переключал бы и режим окна, и воспроизведение.
                self.pending_click = None;
                self.toggle_fullscreen(window, cx);
            }
        }
    }

    /// Переключает полный экран и сразу обновляет иконку кнопки.
    ///
    /// Платформа меняет режим асинхронно, поэтому признак ставим по ожидаемому
    /// состоянию — так же, как это делает кнопка панели управления.
    fn toggle_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = !window.is_fullscreen();
        window.toggle_fullscreen();
        self.player
            .update(cx, |video, cx| video.set_fullscreen(target, cx));
    }

    /// Колесо мыши меняет громкость шагами по 1 %.
    fn handle_scroll(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let delta = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y),
        };
        if delta == 0.0 || !delta.is_finite() {
            return;
        }

        self.note_activity(cx);
        let stepped = self.player.update(cx, |video, cx| {
            let (before, was_muted) = (video.volume(), video.muted());
            // Любое изменение громкости колесом включает звук, иначе шаги
            // меняли бы скрытую громкость при выключенном mute.
            if was_muted {
                video.toggle_mute(cx);
            }
            let volume = volume_after_step(video.volume(), delta > 0.0);
            video.set_volume(volume, cx);

            // Плашку показываем только когда что-то действительно изменилось:
            // без открытого файла или на границе диапазона шага нет.
            let changed = video.volume() != before || video.muted() != was_muted;
            changed.then(|| video.volume())
        });

        if let Some(volume) = stepped {
            self.volume_notice = Some((volume, Instant::now()));
            cx.notify();
            // Громкость колесом сохраняем сразу: окно могут закрыть раньше,
            // чем истечёт плашка.
            self.save_settings(cx);
        }
    }

    /// Закрывает меню и панели поверх кадра (`Escape`, клик по кадру).
    ///
    /// Возвращает `true`, если что-то действительно было открыто: по этому
    /// признаку `Escape` переключается на выход из fullscreen.
    fn close_overlays(&mut self, cx: &mut Context<Self>) -> bool {
        let volume_panel_open = self.player.read(cx).volume_panel_open();
        if self.menu_open
            || self.about_open
            || self.file_info_open
            || self.open_panel.is_open()
            || volume_panel_open
        {
            self.menu_open = false;
            self.about_open = false;
            self.file_info_open = false;
            self.open_panel.close();
            self.open_panel_error = None;
            self.player
                .update(cx, |video, cx| video.close_volume_panel(cx));
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Закрывает меню и возвращает таймер автоскрытия к нулю.
    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu_open {
            self.menu_open = false;
            cx.notify();
        }
        self.note_activity(cx);
    }

    /// Открывает панель «Открыть…»: клавиша `O`, кнопка-папка, меню.
    fn show_open_panel(&mut self, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.about_open = false;
        self.file_info_open = false;
        self.open_panel_error = None;
        self.open_panel.reset();
        self.player
            .update(cx, |video, cx| video.close_volume_panel(cx));
        self.note_activity(cx);
        cx.notify();
    }

    /// Выполняет то, что решила панель по нажатой клавише.
    fn apply_panel_action(&mut self, action: PanelAction, cx: &mut Context<Self>) {
        match action {
            PanelAction::Handled | PanelAction::Ignored => {}
            PanelAction::Close => {
                self.open_panel.close();
                self.open_panel_error = None;
                cx.notify();
            }
            PanelAction::OpenFile => self.pick_file(cx),
            PanelAction::OpenUrl(text) => self.open_url(&text, cx),
            PanelAction::Paste => {
                let text = cx.read_from_clipboard().and_then(|item| item.text());
                if let Some(text) = text {
                    self.open_panel.insert(&text);
                    cx.notify();
                }
            }
        }
    }

    /// Открывает системный диалог из панели или из пункта «Файл».
    fn pick_file(&mut self, cx: &mut Context<Self>) {
        self.open_panel.close();
        self.open_panel_error = None;
        self.player.update(cx, |video, cx| video.open_dialog(cx));
        cx.notify();
    }

    /// Открывает ссылку из поля ввода.
    ///
    /// При ошибке разбора панель остаётся открытой: текст можно исправить
    /// без повторного ввода, а ошибка видна под полем.
    fn open_url(&mut self, text: &str, cx: &mut Context<Self>) {
        match Source::parse(text) {
            Ok(source) => {
                self.open_panel.close();
                self.open_panel_error = None;
                self.note_activity(cx);
                self.player.update(cx, |video, cx| video.open(source, cx));
            }
            Err(parse_error) => {
                self.open_panel_error = Some(parse_error.to_string());
                cx.notify();
            }
        }
    }

    /// Клик по «Файл…» в панели.
    fn choose_file(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pick_file(cx);
    }

    /// Клик по «URL…» в панели.
    fn choose_url(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open_panel.select_url();
        self.open_panel_error = None;
        self.note_activity(cx);
        cx.notify();
    }

    /// Меню → «Открыть…»: то же, что клавиша `O` и кнопка-папка.
    fn open_from_menu(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.show_open_panel(cx);
    }

    /// Меню → «Показать/Скрыть оверлей статистики».
    fn toggle_stats(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        self.player.update(cx, |video, cx| video.toggle_stats(cx));
        self.save_settings(cx);
    }

    /// Меню → «О программе».
    fn show_about(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.file_info_open = false;
        self.about_open = true;
        self.note_activity(cx);
        cx.notify();
    }

    /// Меню → «Информация о файле».
    fn show_file_info(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        self.about_open = false;
        self.file_info_open = true;
        self.note_activity(cx);
        cx.notify();
    }

    /// Меню → «Автоскрытие панели».
    fn toggle_auto_hide(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.auto_hide_enabled = !self.auto_hide_enabled;
        self.close_menu(cx);
        self.save_settings(cx);
    }

    /// Меню → «Запускать сразу».
    ///
    /// Настройка живёт в `VideoPlayer`: её читают все пути открытия файла.
    fn toggle_autoplay(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.player
            .update(cx, |video, cx| video.toggle_autoplay(cx));
        self.close_menu(cx);
        self.save_settings(cx);
    }

    /// Перематывает на шаг вперёд или назад.
    ///
    /// Цель считает ядро: при удержании клавиши `PositionChanged` доходят
    /// с задержкой, и абсолютная цель, собранная в UI, откатывалась бы назад.
    fn seek_relative(&mut self, step: Duration, forward: bool, cx: &mut Context<Self>) {
        self.player
            .update(cx, |video, cx| video.seek_relative(step, forward, cx));
    }

    /// Перетаскивание файла в окно: играем первый подходящий.
    fn handle_drop(&mut self, paths: &ExternalPaths, _window: &mut Window, cx: &mut Context<Self>) {
        debug!(count = paths.0.len(), "файлы отпущены над окном");
        self.dragging = false;
        self.note_activity(cx);
        self.player
            .update(cx, |video, cx| video.open_dropped(&paths.0, cx));
        cx.notify();
    }

    /// Файлы ведут над окном: показываем подсказку, куда их отпустить.
    ///
    /// `on_drag_move` не требует наведения на конкретный элемент, поэтому
    /// по логу видно, дошло ли перетаскивание от ОС до приложения.
    fn handle_drag_move(
        &mut self,
        _event: &DragMoveEvent<ExternalPaths>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dragging {
            debug!("файлы над окном");
            self.dragging = true;
            cx.notify();
        }
    }

    /// Файлы увели из окна, не отпустив.
    fn handle_drag_exit(
        &mut self,
        _event: &FileDropEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging {
            debug!("перетаскивание отменено");
            self.dragging = false;
            cx.notify();
        }
    }

    /// Скрывает панель после бездействия, но только во время воспроизведения.
    ///
    /// Пока открыто меню или панели поверх кадра, панель не прячем: иначе меню
    /// исчезло бы вместе с кнопкой, которая его открыла.
    fn tick_auto_hide(&mut self, cx: &mut Context<Self>) {
        if let Some((_, shown_at)) = self.volume_notice
            && shown_at.elapsed() >= VOLUME_NOTICE_DURATION
        {
            self.volume_notice = None;
            cx.notify();
        }

        if self.menu_open
            || self.about_open
            || self.file_info_open
            || self.open_panel.is_open()
            || self.player.read(cx).volume_panel_open()
        {
            return;
        }

        let playing = self.player.read(cx).state().is_playing();
        if self.controls_visible
            && should_hide(
                playing,
                self.auto_hide_enabled,
                self.last_activity.elapsed(),
            )
        {
            self.controls_visible = false;
            // Панель и курсор скрываются вместе: платформа вернёт курсор,
            // когда мышь сдвинется с места.
            self.platform.hide_cursor_until_mouse_moves();
            cx.notify();
        }
    }

    /// Кнопка «⋯» в панели управления.
    fn menu_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        div()
            .id("vgpu-menu-button")
            .flex()
            .items_center()
            .justify_center()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(gpui::rgb(theme::RAISED))
            .cursor_pointer()
            .aria_label("Ещё")
            .child(icon(Icon::More, ICON_SIZE, gpui::rgb(theme::TEXT)))
            .on_click(cx.listener(PlayerWindow::toggle_menu))
    }

    /// Панель управления поверх кадра, у нижнего края.
    ///
    /// Панель — наложение (`.absolute()`), а не элемент раскладки: её появление
    /// и скрытие по автоскрытию не меняют размер кадра, видео всегда занимает
    /// всё окно. Фон полупрозрачный, чтобы под панелью просвечивал кадр.
    ///
    /// `on_mouse_down` с `stop_propagation` обязателен: под панелью лежит
    /// полноэкранная область «клик по кадру», а GPUI считает наведённым любой
    /// хитбокс под курсором, не только верхний. Без этого клик по любой кнопке
    /// панели ещё и переключал бы воспроизведение. Дети панели регистрируют
    /// обработчики раньше (paint родителя идёт до paint детей), поэтому в
    /// bubble-фазе кнопка успевает записать свой клик, а затем панель гасит
    /// всплытие до области кадра.
    fn controls_panel(&self, cx: &mut Context<Self>) -> Div {
        div()
            .absolute()
            .left_0()
            .bottom_0()
            .w_full()
            .px_4()
            .py_3()
            .flex()
            .items_center()
            .gap_4()
            .bg(gpui::rgba(theme::CONTROLS_BG))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(controls_view(&self.player, cx).flex_1())
            .child(self.menu_button(cx))
    }

    /// Меню «⋯»: слой поверх кадра и список у правого нижнего угла.
    ///
    /// Слой — `.absolute().size_full()` внутри области кадра, как у кнопки
    /// «Повторить»: у такого элемента есть точный размер, поэтому и сам слой,
    /// и его содержимое однозначно ловят мышь. Список — обычный flex-ребёнок
    /// слоя (не `absolute` с автовысотой): его размер считает раскладка, а не
    /// абсолютное позиционирование.
    fn menu_panel(&self, cx: &mut Context<Self>) -> Div {
        let stats_visible = self.player.read(cx).stats_visible();
        let autoplay = self.player.read(cx).autoplay();
        let has_file = self.player.read(cx).media_info().is_some();

        let mut items = div()
            .w(px(280.0))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(gpui::rgb(theme::BORDER))
            .bg(gpui::rgb(theme::SURFACE))
            .flex()
            .flex_col()
            // GPUI считает наведённым любой хитбокс под курсором, а не
            // только верхний, поэтому «клик по кадру» сработал бы и по
            // меню (оно лежит поверх области кадра). Гасим всплытие.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(menu_item(
                "vgpu-menu-open",
                "Открыть…",
                None,
                cx.listener(PlayerWindow::open_from_menu),
            ));

        // Информация о файле есть смысл только когда файл открыт.
        if has_file {
            items = items.child(menu_item(
                "vgpu-menu-file",
                "Информация о файле",
                None,
                cx.listener(PlayerWindow::show_file_info),
            ));
        }

        items = items
            .child(menu_item(
                "vgpu-menu-stats",
                stats_item_label(stats_visible),
                Some(stats_visible),
                cx.listener(PlayerWindow::toggle_stats),
            ))
            .child(menu_item(
                "vgpu-menu-autoplay",
                "Запускать сразу",
                Some(autoplay),
                cx.listener(PlayerWindow::toggle_autoplay),
            ))
            .child(menu_item(
                "vgpu-menu-about",
                "О программе",
                None,
                cx.listener(PlayerWindow::show_about),
            ))
            .child(menu_item(
                "vgpu-menu-auto-hide",
                "Автоскрытие панели",
                Some(self.auto_hide_enabled),
                cx.listener(PlayerWindow::toggle_auto_hide),
            ));

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_end()
            .justify_end()
            .pr(px(16.0))
            .pb(px(CONTROLS_HEIGHT))
            .child(items)
    }

    /// Панель «Открыть…»: выбор источника или поле ввода ссылки.
    ///
    /// Как и меню, панель — наложение поверх кадра; нажатия внутри карточки
    /// гасят всплытие, иначе клик по панели закрывал бы её же.
    fn open_panel_view(&self, cx: &mut Context<Self>) -> Div {
        let mut card = div()
            .w(px(520.0))
            .p_4()
            .rounded_md()
            .border_1()
            .border_color(gpui::rgb(theme::BORDER))
            .bg(gpui::rgb(theme::SURFACE))
            .flex()
            .flex_col()
            .gap_3()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        if let Some(selected) = self.open_panel.choice() {
            card = card
                .child(div().text_sm().child("Открыть"))
                .child(panel_item(
                    "vgpu-open-panel-file",
                    "Файл…",
                    selected == PanelChoice::File,
                    cx.listener(Self::choose_file),
                ))
                .child(panel_item(
                    "vgpu-open-panel-url",
                    "URL…",
                    selected == PanelChoice::Url,
                    cx.listener(Self::choose_url),
                ))
                .child(panel_hint(
                    "Enter — выбрать · ←/→ — переключить · Esc — закрыть",
                ));
        }

        if let Some(input) = self.open_panel.input() {
            let mut field = div()
                .flex()
                .items_center()
                .min_h(px(34.0))
                .px_3()
                .py_2()
                .rounded_sm()
                .border_1()
                .border_color(gpui::rgb(theme::BORDER))
                .bg(gpui::rgb(theme::INPUT))
                .text_sm()
                .overflow_hidden()
                .whitespace_nowrap();

            // Пока ничего не введено, показываем пример — но курсор уже
            // стоит в поле, чтобы было видно, куда печатать.
            field = if input.text().is_empty() {
                field.child(panel_caret()).child(
                    div()
                        .text_color(gpui::rgb(theme::FAINT))
                        .child("https://example.com/movie.mp4"),
                )
            } else {
                field
                    .child(div().child(input.visible_before().to_owned()))
                    .child(panel_caret())
                    .child(div().child(input.visible_after().to_owned()))
            };

            card = card
                .child(div().text_sm().child("Ссылка на видео, .m3u8 или YouTube"))
                .child(field)
                .child(panel_hint(
                    "Enter — открыть · Ctrl+V — вставить · Esc — закрыть",
                ));

            if let Some(error) = self.open_panel_error.as_ref() {
                card = card.child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(theme::ERROR))
                        .child(error.clone()),
                );
            }
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(theme::BACKDROP))
            .child(card)
    }

    /// Строки панели «Информация о файле».
    ///
    /// Берём всё, что реально известно: путь и размер с диска, контейнер и
    /// имена кодеков из заголовка, размеры и частоту из декодера, каналы и
    /// частоту звука из аудиодекодера, длительность из звука или видео.
    fn file_info_lines(&self, cx: &App) -> Vec<String> {
        let video = self.player.read(cx);
        let (Some(source), Some(media)) = (video.source(), video.media_info()) else {
            return vec!["файл не открыт".to_owned()];
        };

        match source {
            Source::File(path) => file_info_lines(path, media, video.duration()),
            Source::Url(url) => url_info_lines(url, media, video.duration()),
        }
    }

    /// Панель с несколькими строками текста поверх кадра.
    ///
    /// Клик по ней закрывает панель: так же работает «О программе». Высота не
    /// фиксирована — строкам с длинным путём нужно переноситься.
    fn message_panel(width: f32, lines: &[String]) -> Div {
        let mut content = div()
            .flex()
            .flex_col()
            .gap_1()
            .w(px(width))
            .p_4()
            .rounded_md()
            .border_1()
            .border_color(gpui::rgb(theme::BORDER))
            .bg(gpui::rgb(theme::SURFACE))
            .text_sm();

        for line in lines {
            content = content.child(line.clone());
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(theme::BACKDROP))
            .child(content)
    }
}

impl Render for PlayerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus_handle, cx);
        }

        // Полноэкранный режим меняет платформа, причём не мгновенно: в `render`
        // берём фактическое состояние окна, чтобы иконка кнопки не врала.
        let fullscreen = window.is_fullscreen();
        self.player
            .update(cx, |video, cx| video.set_fullscreen(fullscreen, cx));

        // Заголовок окна — название ролика YouTube, если ядро его прислало.
        let title = window_title_for(self.player.read(cx).title());
        if self.window_title != title {
            window.set_window_title(&title);
            self.window_title = title;
        }

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(theme::BACKGROUND))
            .text_color(gpui::rgb(theme::TEXT))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(PlayerWindow::handle_key_down))
            .on_mouse_move(cx.listener(PlayerWindow::handle_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(PlayerWindow::handle_mouse_up),
            )
            .on_drop(cx.listener(PlayerWindow::handle_drop))
            .on_drag_move(cx.listener(PlayerWindow::handle_drag_move))
            .on_file_drop_exit(cx.listener(PlayerWindow::handle_drag_exit))
            .on_scroll(cx.listener(PlayerWindow::handle_scroll));

        // Клик по кадру — обычный жест «убрать то, что открыто поверх»:
        // закрывает меню «⋯» и панели с информацией. Сами пункты меню гасят
        // всплытие (`stop_propagation`), иначе этот же обработчик закрывал бы
        // меню уже на mouse down — до того, как придёт `on_click`.
        //
        // Вложенный `div` нужен ради `on_click`: клик по кадру ставит видео
        // на паузу и продолжает его. Кнопка «Повторить» внутри `video_view`
        // гасит всплытие, чтобы не переключать воспроизведение дважды.
        let mut video_area = div()
            .id("vgpu-video-area")
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(PlayerWindow::handle_video_click),
            )
            .child(
                div()
                    .id("vgpu-video-click")
                    .size_full()
                    .on_click(cx.listener(PlayerWindow::handle_video_click_release))
                    .child(video_view(&self.player, cx).size_full()),
            );

        // Плашка громкости — отдельное наложение: она не сдвигает ни панель,
        // ни кадр и видна даже при скрытой панели управления.
        if let Some((volume, _)) = self.volume_notice {
            video_area = video_area.child(
                div()
                    .absolute()
                    .right(px(16.0))
                    .bottom(px(CONTROLS_HEIGHT))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(gpui::rgba(theme::NOTICE_BG))
                    .text_sm()
                    .child(format!("{}%", volume_percent(volume))),
            );
        }

        if self.about_open {
            let lines: Vec<String> = about_text(&self.versions)
                .lines()
                .map(str::to_owned)
                .collect();
            video_area = video_area.child(Self::message_panel(340.0, &lines));
        }

        if self.file_info_open {
            let lines = self.file_info_lines(cx);
            video_area = video_area.child(Self::message_panel(520.0, &lines));
        }

        if self.menu_open {
            video_area = video_area.child(self.menu_panel(cx));
        }

        // Панель — последним ребёнком: она лежит поверх кадра и не участвует
        // в раскладке, поэтому видео не уменьшается.
        if self.controls_visible {
            video_area = video_area.child(self.controls_panel(cx));
        }

        // «Открыть…» перекрывает и управление, и меню: пока она открыта,
        // клавиши принадлежат полю ввода.
        if self.open_panel.is_open() {
            video_area = video_area.child(self.open_panel_view(cx));
        }

        root = root.child(video_area);

        // Подсказка поверх всего, пока файлы ведут над окном. Она же — самый
        // верхний элемент под курсором, поэтому `on_drop` на ней сработает
        // даже если хитбокс контейнера почему-то не считается наведённым.
        if self.dragging {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(theme::BACKDROP))
                    .child(
                        div()
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .bg(gpui::rgb(theme::ACCENT))
                            .text_color(gpui::rgb(theme::ON_ACCENT))
                            .child("Отпустите файл, чтобы открыть"),
                    )
                    .on_drop(cx.listener(PlayerWindow::handle_drop)),
            );
        }

        root
    }
}

/// Пункт меню: подпись, необязательная галочка и обработчик клика.
///
/// Под галочку всегда отведено место, даже у обычных пунктов: иначе подписи
/// переключателей и действий начинались бы с разного отступа.
///
/// `on_mouse_down` с `stop_propagation` нужен, чтобы нажатие на пункт не
/// всплыло до области кадра: там висит «клик по кадру закрывает меню», и без
/// этого меню исчезало бы на mouse down, не дожив до `on_click`.
fn menu_item(
    id: &'static str,
    label: impl Into<SharedString>,
    checked: Option<bool>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let mark = match checked {
        Some(true) => "✓",
        _ => "",
    };

    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .aria_label(label.clone())
        .hover(|style| style.bg(gpui::rgb(theme::RAISED)))
        .child(
            div()
                .w(px(14.0))
                .text_color(gpui::rgb(theme::HIGHLIGHT))
                .child(mark),
        )
        .child(div().text_sm().child(label))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
}

/// Пункт панели «Открыть…»: подпись и подсветка выбранного.
///
/// `on_mouse_down` с `stop_propagation` нужен по той же причине, что и в меню:
/// под панелью лежит «клик по кадру закрывает наложения».
fn panel_item(
    id: &'static str,
    label: impl Into<SharedString>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label = label.into();
    let mut item = div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .aria_label(label.clone())
        .hover(|style| style.bg(gpui::rgb(theme::RAISED)))
        .child(div().text_sm().child(label))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click);

    if selected {
        item = item.bg(gpui::rgb(theme::RAISED));
    }
    item
}

/// Подсказка под содержимым панели: серый мелкий текст.
fn panel_hint(text: &'static str) -> Div {
    div()
        .text_sm()
        .text_color(gpui::rgb(theme::MUTED))
        .child(text)
}

/// Курсор поля ввода: тонкая вертикальная черта.
fn panel_caret() -> Div {
    div()
        .flex_shrink_0()
        .w(px(2.0))
        .h(px(16.0))
        .bg(gpui::rgb(theme::HIGHLIGHT))
}

/// Подпись переключателя оверлея: действие, которое сделает клик.
fn stats_item_label(visible: bool) -> &'static str {
    if visible {
        "Скрыть оверлей статистики"
    } else {
        "Показать оверлей статистики"
    }
}

/// Текст панели «О программе».
///
/// Каждая библиотека — отдельной строкой: так сразу видно, какая DLL рядом с
/// exe не та, что ожидалась.
fn about_text(versions: &FfmpegVersions) -> String {
    format!(
        "VGPU Player {}\nlibavutil {}\nlibavformat {}\nlibavcodec {}",
        env!("CARGO_PKG_VERSION"),
        versions.util,
        versions.format,
        versions.codec
    )
}

/// Строки панели «Информация о файле».
///
/// Первая строка — имя файла, вторая — полный путь, третья — факты о файле,
/// дальше по строке на дорожку.
fn file_info_lines(path: &Path, media: &MediaInfo, duration: Option<Duration>) -> Vec<String> {
    let mut lines = Vec::new();

    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    lines.push(name);
    lines.push(path.display().to_string());

    let mut facts = Vec::new();
    if let Ok(metadata) = std::fs::metadata(path) {
        facts.push(format_size(metadata.len()));
    }
    if let Some(container) = &media.container {
        facts.push(container.clone());
    }
    if let Some(duration) = duration {
        facts.push(format!("длительность {}", format_clock(duration)));
    }
    if !facts.is_empty() {
        lines.push(facts.join(" · "));
    }

    lines.extend(track_lines(media));
    lines
}

/// Строки панели «Информация о файле» для URL.
///
/// Размера у ссылки нет, поэтому первая строка — последний сегмент пути, а
/// вторая — полная ссылка, как путь у локального файла.
fn url_info_lines(url: &str, media: &MediaInfo, duration: Option<Duration>) -> Vec<String> {
    let name = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .and_then(|(_, path)| path.split(['?', '#']).next())
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(url)
        .to_owned();

    let mut lines = vec![name, url.to_owned()];

    let mut facts = Vec::new();
    if let Some(container) = &media.container {
        facts.push(container.clone());
    }
    if let Some(duration) = duration {
        facts.push(format!("длительность {}", format_clock(duration)));
    }
    if !facts.is_empty() {
        lines.push(facts.join(" · "));
    }

    lines.extend(track_lines(media));
    lines
}

/// Строки дорожек: кодек, размеры/кадры и параметры звука.
fn track_lines(media: &MediaInfo) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(video) = &media.video {
        let mut parts = Vec::new();
        if let Some(codec) = &video.codec {
            parts.push(codec.clone());
        }
        parts.push(format!("{}×{}", video.width, video.height));
        if let Some(fps) = video.frame_rate {
            parts.push(format!("{} fps", format_frame_rate(fps)));
        }
        lines.push(format!("видео: {}", parts.join(" · ")));
    }

    if let Some(audio) = &media.audio {
        let mut parts = Vec::new();
        if let Some(codec) = &audio.codec {
            parts.push(codec.clone());
        }
        parts.push(channels_label(audio.channels));
        parts.push(format!("{} Гц", audio.sample_rate));
        lines.push(format!("звук: {}", parts.join(" · ")));
    }

    lines
}

/// Размер файла по-человечески: `819 КБ`, `42.7 МБ`, `1.25 ГБ`.
fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} ГБ", value / GIB)
    } else if value >= MIB {
        format!("{:.1} МБ", value / MIB)
    } else if value >= KIB {
        format!("{:.0} КБ", value / KIB)
    } else {
        format!("{bytes} Б")
    }
}

/// Частота кадров без лишних нулей: `30`, `29.97`.
fn format_frame_rate(fps: f64) -> String {
    let rounded = (fps * 100.0).round() / 100.0;
    if rounded.fract().abs() < f64::EPSILON {
        format!("{rounded:.0}")
    } else {
        format!("{rounded:.2}")
    }
}

/// Число каналов по-русски: `1 канал`, `2 канала`, `6 каналов`.
fn channels_label(channels: u16) -> String {
    let tens = channels % 100;
    let word = match channels % 10 {
        1 if tens != 11 => "канал",
        2..=4 if !(12..=14).contains(&tens) => "канала",
        _ => "каналов",
    };
    format!("{channels} {word}")
}

/// Фоновая задача автоскрытия панели.
///
/// Цикл заканчивается сам, когда окно закрыто: `weak.update` вернёт ошибку.
fn spawn_auto_hide(cx: &mut Context<PlayerWindow>) -> Task<()> {
    cx.spawn(
        async move |this: WeakEntity<PlayerWindow>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(AUTO_HIDE_TICK).await;
                if this
                    .update(cx, |window, cx| window.tick_auto_hide(cx))
                    .is_err()
                {
                    return;
                }
            }
        },
    )
}

/// Откладывает переключение паузы после одиночного клика.
///
/// Задача живёт в поле окна: приход второго клика заменяет её на `None` и
/// отменяет таймер, а закрытие окна просто роняет задачу.
fn spawn_pending_playback(cx: &mut Context<PlayerWindow>) -> Task<()> {
    cx.spawn(
        async move |this: WeakEntity<PlayerWindow>, cx: &mut AsyncApp| {
            cx.background_executor().timer(DOUBLE_CLICK_DELAY).await;
            let _ = this.update(cx, |window, cx| {
                window
                    .player
                    .update(cx, |video, cx| video.toggle_playback(cx));
            });
        },
    )
}

/// Громкость на старте: параметр запуска важнее сохранённой настройки.
///
/// Ограничение диапазона страхует библиотечный вызов: `set_volume` молча
/// игнорирует значения вне `0..=1`, а нам нужно применить валидную часть.
fn initial_volume(settings: f32, from_cli: Option<f32>) -> f32 {
    from_cli.map_or(settings, |volume| volume.clamp(0.0, 1.0))
}

/// Заголовок окна: название ролика важнее имени продукта.
fn window_title_for(video_title: Option<&str>) -> String {
    match video_title {
        Some(title) if !title.trim().is_empty() => format!("{title} — {WINDOW_TITLE}"),
        _ => WINDOW_TITLE.to_owned(),
    }
}

/// Открывает главное окно в уже запущенном приложении.
fn open_main_window(
    cx: &mut App,
    source: Option<Source>,
    player_options: PlayerOptions,
    platform: Rc<dyn Platform>,
) -> gpui::Result<()> {
    let bounds = Bounds::centered(None, size(px(960.0), px(540.0)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Уже этого таймлайн и время перестают помещаться: окно можно
        // уменьшить только до разумного предела.
        window_min_size: Some(size(px(560.0), px(315.0))),
        titlebar: Some(TitlebarOptions {
            title: Some(WINDOW_TITLE.into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    info!("открытие главного окна");
    cx.open_window(options, |_window, cx| {
        cx.new(|cx| PlayerWindow::new(source, player_options, platform, cx))
    })?;
    Ok(())
}

/// Параметры запуска окна, которые не сохраняются в `settings.cfg`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerOptions {
    /// Громкость на этот запуск в диапазоне `0..=1`.
    /// `None` — взять сохранённую настройку.
    pub volume: Option<f32>,
    /// Заголовки HTTP для стартового источника (`Referer`, `User-Agent`).
    pub http: HttpOptions,
}

/// Запускает демонстрационное приложение и блокирует поток до закрытия окна.
///
/// `Application::run` не умеет возвращать значение из колбэка, поэтому ошибку
/// создания окна складываем в слот, останавливаем приложение и отдаём её уже
/// после выхода из цикла событий.
pub fn run_app_source(source: Option<Source>) -> Result<(), LaunchError> {
    run_app_source_with(source, PlayerOptions::default())
}

/// То же, что [`run_app_source`], но с параметрами, которые не берутся из
/// `settings.cfg` (сейчас — громкость).
pub fn run_app_source_with(
    source: Option<Source>,
    options: PlayerOptions,
) -> Result<(), LaunchError> {
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let failure_slot = Rc::clone(&failure);

    // Платформу создаём сами, а не через `gpui_platform::application()`:
    // экземпляр должен быть тем же, что у приложения, иначе скрытие курсора
    // не найдёт ни одного окна.
    let platform = gpui_platform::current_platform(false);
    let window_platform = Rc::clone(&platform);

    gpui::Application::with_platform(platform).run(move |cx: &mut App| {
        if let Err(err) = open_main_window(cx, source, options, window_platform) {
            error!(%err, "не удалось открыть окно");
            *failure_slot.borrow_mut() = Some(err.to_string());
            cx.quit();
            return;
        }
        cx.activate(true);
    });

    match failure.borrow_mut().take() {
        Some(message) => Err(LaunchError { message }),
        None => Ok(()),
    }
}

/// Запуск по локальному пути: обёртка над [`run_app_source`].
pub fn run_app(path: Option<PathBuf>) -> Result<(), LaunchError> {
    run_app_source(path.map(Source::File))
}

#[cfg(test)]
mod tests {
    use super::*;
    use player_core::FfmpegVersion;

    #[test]
    fn window_title_uses_video_title() {
        assert_eq!(window_title_for(Some("Ролик")), "Ролик — VGPU Player");
        assert_eq!(window_title_for(None), "VGPU Player");
        assert_eq!(window_title_for(Some("   ")), "VGPU Player");
    }

    #[test]
    fn double_click_asks_for_fullscreen() {
        assert_eq!(click_action(1), ClickAction::TogglePlayback);
        assert_eq!(click_action(2), ClickAction::ToggleFullscreen);
    }

    #[test]
    fn shortcut_maps_player_keys() {
        assert_eq!(shortcut("space"), Some(Shortcut::TogglePlayback));
        assert_eq!(shortcut("left"), Some(Shortcut::SeekBackward));
        assert_eq!(shortcut("right"), Some(Shortcut::SeekForward));
        assert_eq!(shortcut("f"), Some(Shortcut::ToggleFullscreen));
        assert_eq!(shortcut("o"), Some(Shortcut::OpenFile));
        assert_eq!(shortcut("O"), Some(Shortcut::OpenFile));
        assert_eq!(shortcut("escape"), Some(Shortcut::CloseOverlays));
        assert_eq!(shortcut("q"), None);
    }

    #[test]
    fn auto_hide_only_while_enabled_and_playing() {
        assert!(should_hide(true, true, AUTO_HIDE_DELAY));
        assert!(!should_hide(true, false, AUTO_HIDE_DELAY));
        assert!(!should_hide(
            true,
            true,
            AUTO_HIDE_DELAY - Duration::from_millis(1)
        ));
        assert!(!should_hide(false, true, Duration::from_secs(60)));
    }

    #[test]
    fn cli_volume_overrides_saved_setting() {
        assert_eq!(initial_volume(0.8, None), 0.8);
        assert_eq!(initial_volume(0.8, Some(0.1)), 0.1);
        assert_eq!(initial_volume(0.8, Some(0.0)), 0.0);
        // Библиотечный вызов может передать что угодно: применяем только
        // валидную часть, иначе `set_volume` молча проигнорирует значение.
        assert_eq!(initial_volume(0.8, Some(1.5)), 1.0);
        assert_eq!(initial_volume(0.8, Some(-1.0)), 0.0);
    }

    #[test]
    fn wheel_steps_volume_by_one_percent() {
        assert!((volume_after_step(0.50, true) - 0.51).abs() < f32::EPSILON);
        assert!((volume_after_step(0.50, false) - 0.49).abs() < f32::EPSILON);
        assert_eq!(volume_after_step(1.0, true), 1.0);
        assert_eq!(volume_after_step(0.0, false), 0.0);
        assert!((volume_after_step(0.004, false) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stats_item_names_the_action() {
        assert_eq!(stats_item_label(true), "Скрыть оверлей статистики");
        assert_eq!(stats_item_label(false), "Показать оверлей статистики");
    }

    #[test]
    fn about_text_lists_app_and_ffmpeg_versions() {
        let versions = FfmpegVersions {
            util: FfmpegVersion::from_bits((60 << 16) | (8 << 8) | 100),
            format: FfmpegVersion::from_bits((62 << 16) | (3 << 8) | 100),
            codec: FfmpegVersion::from_bits((62 << 16) | (11 << 8) | 100),
        };

        let text = about_text(&versions);

        assert!(text.starts_with("VGPU Player "));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("libavutil 60.8.100"));
        assert!(text.contains("libavformat 62.3.100"));
        assert!(text.contains("libavcodec 62.11.100"));
    }

    fn sample_file() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("vgpu-player-info-{}.mp4", std::process::id()));
        std::fs::write(&path, vec![0_u8; 2048]).expect("создать файл для проверки");
        path
    }

    #[test]
    fn file_info_lists_path_facts_and_tracks() {
        let path = sample_file();
        let media = MediaInfo {
            container: Some("QuickTime / MOV".to_owned()),
            video: Some(player_core::VideoTrackInfo {
                codec: Some("h264".to_owned()),
                width: 1920,
                height: 1080,
                frame_rate: Some(29.97),
            }),
            audio: Some(player_core::AudioTrackInfo {
                codec: Some("aac".to_owned()),
                channels: 2,
                sample_rate: 48_000,
            }),
        };

        let lines = file_info_lines(&path, &media, Some(Duration::from_secs(125)));
        let _ = std::fs::remove_file(&path);

        let text = lines.join("\n");
        assert!(text.contains("vgpu-player-info-"), "имя файла: {text}");
        assert!(text.contains("2 КБ"), "размер файла: {text}");
        assert!(text.contains("QuickTime / MOV"), "контейнер: {text}");
        assert!(text.contains("02:05"), "длительность: {text}");
        assert!(
            text.contains("видео: h264 · 1920×1080 · 29.97 fps"),
            "видео: {text}"
        );
        assert!(
            text.contains("звук: aac · 2 канала · 48000 Гц"),
            "звук: {text}"
        );
    }

    #[test]
    fn file_info_without_tracks_keeps_the_file_lines() {
        let path = PathBuf::from("missing.mp4");
        let lines = file_info_lines(&path, &MediaInfo::default(), None);

        assert_eq!(
            lines,
            vec!["missing.mp4".to_owned(), "missing.mp4".to_owned()]
        );
    }

    #[test]
    fn url_info_has_no_size_and_shows_the_link() {
        let media = MediaInfo {
            container: Some("Matroska / WebM".to_owned()),
            ..MediaInfo::default()
        };
        let lines = url_info_lines(
            "https://example.com/dir/movie.mkv?token=1",
            &media,
            Some(Duration::from_secs(5)),
        );

        assert_eq!(lines[0], "movie.mkv");
        assert_eq!(lines[1], "https://example.com/dir/movie.mkv?token=1");
        assert!(
            lines.iter().any(|line| line.contains("Matroska")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("00:05")), "{lines:?}");
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("КБ") && !line.contains("МБ")),
            "у URL нет размера: {lines:?}"
        );
    }

    #[test]
    fn sizes_use_readable_units() {
        assert_eq!(format_size(512), "512 Б");
        assert_eq!(format_size(2_048), "2 КБ");
        assert_eq!(format_size(4_400_000), "4.2 МБ");
        assert_eq!(format_size(1_342_177_280), "1.25 ГБ");
    }

    #[test]
    fn frame_rate_drops_trailing_zeros() {
        assert_eq!(format_frame_rate(30.0), "30");
        assert_eq!(format_frame_rate(29.97), "29.97");
        assert_eq!(format_frame_rate(23.976), "23.98");
    }

    #[test]
    fn channel_counts_are_pluralized() {
        assert_eq!(channels_label(1), "1 канал");
        assert_eq!(channels_label(2), "2 канала");
        assert_eq!(channels_label(6), "6 каналов");
        assert_eq!(channels_label(11), "11 каналов");
        assert_eq!(channels_label(21), "21 канал");
    }
}
