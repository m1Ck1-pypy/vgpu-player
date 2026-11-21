//! Состояние воспроизведения и связь с ядром.
//!
//! [`VideoPlayer`] — единственное место, где UI разговаривает с
//! [`player_core::Player`]. Остальные компоненты (виджет кадра, панель
//! управления) читают его состояние и зовут его методы, поэтому чужому
//! приложению не нужно знать про команды и события ядра.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    App, AppContext, AsyncApp, Bounds, Context, DevicePixels, DynamicTexture, Entity,
    PathPromptOptions, Pixels, Size, Task, WeakEntity, Window, point, size,
};
use player_core::{
    Command, Event, MediaInfo, Player, Source, SpectrumBars, SyncStats, VideoFrame, VideoInfo,
    preview_frame,
};
use tracing::{error, info};

/// Как часто UI проверяет события ядра.
///
/// Чаще, чем кадр: события `PositionChanged` появились на этапе 2, и 15 мс
/// достаточно, чтобы часы в UI не отставали заметно, но не грузили цикл
/// событий впустую.
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(15);

/// В каком состоянии сейчас воспроизведение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    /// Файл ещё не открыт.
    Idle,
    /// Источник открывается: ждём `Ready` или `Error`.
    Loading,
    /// Файл открыт, воспроизведение не начато.
    Ready,
    /// Играет.
    Playing,
    /// Стоит на паузе.
    Paused,
    /// Файл доигран до конца.
    Ended,
    /// Ошибка: файл не открылся или поток ядра упал.
    Failed,
}

impl PlaybackState {
    /// Идёт ли звук прямо сейчас.
    pub fn is_playing(self) -> bool {
        matches!(self, Self::Playing)
    }
}

/// Текстура последнего кадра и её параметры.
///
/// `serial` — номер кадра, который уже загружен в текстуру. Он позволяет не
/// пересылать один и тот же кадр при каждой перерисовке панели или ресайзе.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VideoTexture {
    pub(crate) texture: DynamicTexture,
    pub(crate) width: u32,
    pub(crate) height: u32,
    serial: u64,
}

/// Цель перемотки, которую UI показывает до подтверждения ядром.
///
/// Пока ядро ищет данные, `PositionChanged` из прошлого игнорируются: ползунок
/// стоит на цели и ждёт `Seeked`, а не откатывается назад.
#[derive(Debug, Clone, Copy)]
struct SeekHint {
    target: Duration,
    /// Позиция, от которой считался шаг: задаёт направление.
    from: Duration,
}

impl SeekHint {
    /// Допуск: ядро может встать чуть раньше цели (ключевой кадр).
    const TOLERANCE: Duration = Duration::from_millis(250);

    fn accepts(self, position: Duration) -> bool {
        if self.target >= self.from {
            position + Self::TOLERANCE >= self.target
        } else {
            position <= self.target + Self::TOLERANCE
        }
    }
}

/// Плеер как сущность GPUI.
///
/// Хранит состояние, которое нужно UI, и держит ссылку на ядро. Задачу опроса
/// событий держим в поле, чтобы она жила ровно столько же, сколько сущность.
pub struct VideoPlayer {
    core: Option<Player>,
    /// Источник, который открыт или который не удалось открыть (для повтора).
    source: Option<Source>,
    state: PlaybackState,
    volume: f32,
    duration: Option<Duration>,
    position: Duration,
    /// Цель перемотки, ещё не подтверждённая ядром.
    seek_hint: Option<SeekHint>,
    /// Можно ли перематывать источник.
    seekable: bool,
    /// Идёт ли буферизация (для сети появится на этапе 3).
    buffering: bool,
    error: Option<String>,
    video_info: Option<VideoInfo>,
    /// Контейнер, кодеки и параметры звука открытого файла.
    media_info: Option<MediaInfo>,
    frame: Option<VideoFrame>,
    frame_serial: u64,
    texture: Option<VideoTexture>,
    stats: Option<SyncStats>,
    /// Последние уровни спектроанализатора (файлы без видео).
    ///
    /// Ядро присылает их ~30 раз в секунду; UI только рисует.
    spectrum: Option<SpectrumBars>,
    /// Показывать ли оверлей статистики поверх кадра.
    ///
    /// Это настройка представления, а не воспроизведения: она живёт в плеере,
    /// потому что тот же оверлей рисует [`crate::video_view`].
    stats_visible: bool,
    /// Находится ли окно в полноэкранном режиме.
    ///
    /// Источник истины — окно; демо-обёртка синхронизирует флаг из
    /// `Window::is_fullscreen()`, чтобы панель показывала верную иконку.
    fullscreen: bool,
    /// Доля громкости под курсором, пока он над ползунком: предпросмотр
    /// процента в подсказке. `None` — курсор вне ползунка.
    volume_hover: Option<f32>,
    muted: bool,
    /// Начинать воспроизведение сразу после открытия файла.
    ///
    /// Настройка поведения, а не конкретного файла: живёт в памяти процесса и
    /// меняется пунктом меню «Запускать сразу».
    autoplay: bool,
    /// Позиция, выбранная на полосе прогресса, но ещё не отправленная в ядро.
    scrub: Option<Duration>,
    volume_scrubbing: bool,
    /// Границы слайдеров в координатах окна: нужны, чтобы перевести клик в долю.
    progress_bounds: Option<Bounds<Pixels>>,
    volume_bounds: Option<Bounds<Pixels>>,
    _event_pump: Task<()>,
}

impl VideoPlayer {
    /// Создаёт плеер без файла: пригодится для диалога открытия на этапе 9.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            core: None,
            source: None,
            state: PlaybackState::Idle,
            volume: 1.0,
            duration: None,
            position: Duration::ZERO,
            seek_hint: None,
            seekable: false,
            buffering: false,
            error: None,
            video_info: None,
            media_info: None,
            frame: None,
            frame_serial: 0,
            texture: None,
            stats: None,
            spectrum: None,
            stats_visible: false,
            fullscreen: false,
            volume_hover: None,
            muted: false,
            autoplay: true,
            scrub: None,
            volume_scrubbing: false,
            progress_bounds: None,
            volume_bounds: None,
            _event_pump: spawn_event_pump(cx),
        }
    }

    /// Создаёт сущность сразу с открытым файлом и запускает воспроизведение.
    ///
    /// Ошибка открытия не паникует: она попадает в [`Self::error`] и в
    /// [`PlaybackState::Failed`], чтобы приложение могло её показать.
    pub fn create(source: impl Into<Source>, cx: &mut App) -> Entity<Self> {
        let source = source.into();
        cx.new(|cx| {
            let mut player = Self::new(cx);
            player.open(source, cx);
            player
        })
    }

    /// Открывает (или меняет) источник и сразу начинает воспроизведение.
    ///
    /// Источник запоминается в любом случае: после ошибки пользователь может
    /// повторить открытие той же кнопкой, не вызывая диалог.
    pub fn open(&mut self, source: impl Into<Source>, cx: &mut Context<Self>) {
        let source = source.into();
        self.source = Some(source.clone());

        // Новая попытка: старые метаданные и кадр больше не относятся к файлу.
        // `reset_video` ещё видит прежнее ядро и возвращает ему буфер.
        self.reset_video();
        self.stats = None;
        self.spectrum = None;
        self.video_info = None;
        self.media_info = None;
        self.duration = None;
        self.position = Duration::ZERO;
        self.seekable = false;
        self.buffering = false;
        self.error = None;
        self.state = PlaybackState::Loading;

        // Одно ядро на плеер: живой поток получает `Command::Open`, а новый
        // создаётся только если ядра ещё нет или предыдущее уже завершилось.
        let mut reused = false;
        if let Some(core) = self.core.as_ref()
            && core.send(Command::Open(source.clone())).is_ok()
        {
            reused = true;
        }

        if !reused {
            // Старое ядро мертво (или его не было): его `Drop` взведёт отмену.
            self.core = None;
            match Player::open(source.clone()) {
                Ok(core) => {
                    info!(source = %source, "поток ядра запущен");
                    self.core = Some(core);
                }
                Err(open_error) => {
                    error!(source = %source, error = %open_error, "не удалось запустить ядро");
                    self.state = PlaybackState::Failed;
                    self.error = Some(open_error.to_string());
                    cx.notify();
                    return;
                }
            }
        }

        // Громкость запоминается в ядре, но повтор не вредит: при новом потоке
        // уровень мог сброситься.
        self.send(Command::SetVolume(self.engine_volume()), cx);

        // Пока ядро открывает источник, показываем первый кадр, чтобы
        // окно не оставалось чёрным. Для URL и плейлиста превью нет.
        if let Some(path) = source.as_file()
            && !is_playlist(path)
            && let Some(frame) = preview_frame(path)
        {
            self.frame = Some(frame);
            self.frame_serial = self.frame_serial.wrapping_add(1);
        }

        cx.notify();
    }

    /// Открывает системный диалог выбора файла и проигрывает выбранный.
    ///
    /// Диалог родной для платформы (`App::prompt_for_paths`), поэтому отдельная
    /// зависимость вроде `rfd` не нужна. Пока открывается один файл: набор
    /// файлов появится вместе с плейлистом.
    pub fn open_dialog(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Открыть видео".into()),
        });

        cx.spawn(
            async move |this: WeakEntity<VideoPlayer>, cx: &mut AsyncApp| {
                // Отмена диалога и ошибка платформы — не повод что-то менять.
                let Ok(Ok(Some(paths))) = receiver.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let _ = this.update(cx, |video, cx| video.open(path, cx));
            },
        )
        .detach();
    }

    /// Открывает первый перетащенный файл.
    ///
    /// Перетащить в окно можно что угодно, поэтому каталоги и несуществующие
    /// пути пропускаются. Неподдерживаемый файл не страшен: он попадёт в
    /// [`PlaybackState::Failed`] с текстом ошибки от ядра.
    pub fn open_dropped(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if let Some(path) = first_file(paths) {
            self.open(path.clone(), cx);
        }
    }

    /// Начать (или продолжить) воспроизведение.
    pub fn play(&mut self, cx: &mut Context<Self>) {
        if self.state == PlaybackState::Loading {
            return;
        }
        if self.send(Command::Play, cx) {
            self.state = PlaybackState::Playing;
            cx.notify();
        }
    }

    /// Поставить на паузу.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.state == PlaybackState::Loading {
            return;
        }
        if self.send(Command::Pause, cx) {
            self.state = PlaybackState::Paused;
            cx.notify();
        }
    }

    /// Переключить воспроизведение и паузу.
    ///
    /// После конца файла пробел начинает его заново, а не просто продолжает
    /// воспроизведение с упёртого в конец декодера.
    pub fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        match self.state {
            PlaybackState::Loading => {}
            PlaybackState::Playing => self.pause(cx),
            PlaybackState::Ended => self.restart(cx),
            _ => self.play(cx),
        }
    }

    /// Начинает файл с начала (кнопка «Повторить» после завершения).
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        if self.core.is_none() || self.state == PlaybackState::Loading {
            return;
        }
        self.seek(Duration::ZERO, cx);
        self.play(cx);
    }

    /// Повторяет открытие последнего источника (кнопка после ошибки).
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self.source.clone() {
            self.open(source, cx);
        }
    }

    /// Есть ли источник, открытие которого можно повторить.
    pub fn can_retry(&self) -> bool {
        self.source.is_some() && self.state == PlaybackState::Failed
    }

    /// Перейти к позиции.
    pub fn seek(&mut self, position: Duration, cx: &mut Context<Self>) {
        if self.state == PlaybackState::Loading || !self.seekable {
            return;
        }
        let from = self.position();
        if self.send(Command::Seek(position), cx) {
            self.position = position;
            self.seek_hint = Some(SeekHint {
                target: position,
                from,
            });
            cx.notify();
        }
    }

    /// Перемотать на шаг вперёд или назад.
    ///
    /// Цель считает ядро от своей позиции: при удержании клавиши события
    /// `PositionChanged` доходят с задержкой, и абсолютная цель, посчитанная
    /// здесь, откатывалась бы назад. UI показывает накопленную цель и ждёт
    /// `Seeked`.
    pub fn seek_relative(&mut self, step: Duration, forward: bool, cx: &mut Context<Self>) {
        if self.state == PlaybackState::Loading || !self.seekable {
            return;
        }
        let current = self.position();
        let mut target = if forward {
            current.saturating_add(step)
        } else {
            current.saturating_sub(step)
        };
        if let Some(duration) = self.duration {
            target = target.min(duration);
        }
        if self.send(Command::SeekRelative { forward, step }, cx) {
            self.position = target;
            self.seek_hint = Some(SeekHint {
                target,
                from: current,
            });
            cx.notify();
        }
    }

    /// Выставить громкость: `1.0` — исходный уровень.
    ///
    /// Значения вне `[0, 1]` и `NaN` отбрасываются, а не обрезаются молча:
    /// у вызывающего кода должен быть предсказуемый диапазон.
    ///
    /// Громкость запоминается и без открытого файла: новое ядро получит её при
    /// `Open`, поэтому уровень не сбрасывается при смене файла, а сохранённую
    /// настройку можно применить до открытия.
    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return;
        }
        self.send(Command::SetVolume(volume), cx);
        if self.volume != volume {
            self.volume = volume;
            cx.notify();
        }
    }

    /// Остановить воспроизведение и закрыть файл.
    ///
    /// Команда `Stop` взводит флаг отмены внутри [`Player`], поэтому закрытие
    /// не ждёт завершения текущего открытия. Ядро после `Stop` завершается, и
    /// следующее открытие создаст новое.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        // Кадр возвращаем в пул, пока ядро ещё живо.
        self.reset_video();
        if let Some(core) = self.core.take() {
            let _ = core.send(Command::Stop);
        }
        self.state = PlaybackState::Idle;
        self.position = Duration::ZERO;
        self.duration = None;
        self.seekable = false;
        self.buffering = false;
        self.video_info = None;
        self.media_info = None;
        self.stats = None;
        self.spectrum = None;
        cx.notify();
    }

    /// Текущее состояние.
    pub fn state(&self) -> PlaybackState {
        self.state
    }

    /// Громкость в диапазоне `[0, 1]`.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Выключен ли звук.
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Включает и выключает звук, запоминая прежнюю громкость.
    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        let target = if self.muted { self.volume } else { 0.0 };
        if self.send(Command::SetVolume(target), cx) {
            self.muted = !self.muted;
            cx.notify();
        }
    }

    /// Длительность файла, если ядро её сообщило.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Позиция воспроизведения (осмысленна с этапа 2).
    ///
    /// Пока пользователь тянет полосу, отдаём позицию предпросмотра: ядро
    /// в это время ещё не перематывалось.
    pub fn position(&self) -> Duration {
        self.scrub
            .or(self.seek_hint.map(|hint| hint.target))
            .unwrap_or(self.position)
    }

    /// Текст последней ошибки, если она была.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Параметры видеопотока, если в файле есть видео.
    pub fn video_info(&self) -> Option<VideoInfo> {
        self.video_info
    }

    /// Путь открытого (или последнего открывавшегося) файла.
    /// Источник, который открыт или не открылся (для повтора).
    pub fn source(&self) -> Option<&Source> {
        self.source.as_ref()
    }

    /// Путь к файлу, если источник локальный.
    pub fn path(&self) -> Option<&Path> {
        self.source.as_ref().and_then(Source::as_file)
    }

    /// Можно ли перематывать источник.
    pub fn seekable(&self) -> bool {
        self.seekable
    }

    /// Идёт ли буферизация источника.
    pub fn buffering(&self) -> bool {
        self.buffering
    }

    /// Контейнер, кодеки и параметры дорожек открытого файла.
    ///
    /// `None`, пока файл не открыт и если открыть его не удалось.
    pub fn media_info(&self) -> Option<&MediaInfo> {
        self.media_info.as_ref()
    }

    /// Последний полученный кадр.
    pub fn frame(&self) -> Option<&VideoFrame> {
        self.frame.as_ref()
    }

    /// Последняя статистика синхронизации, если ядро её прислало.
    pub fn stats(&self) -> Option<SyncStats> {
        self.stats
    }

    /// Последние уровни спектроанализатора.
    ///
    /// `None` — данных ещё нет (файл не открыт, видеофайл или тишина до первого
    /// события). Для файла без видео уровни приходят во время воспроизведения.
    pub fn spectrum(&self) -> Option<SpectrumBars> {
        self.spectrum
    }

    /// Показывается ли оверлей статистики.
    pub fn stats_visible(&self) -> bool {
        self.stats_visible
    }

    /// Включено ли автовоспроизведение при открытии файла.
    pub fn autoplay(&self) -> bool {
        self.autoplay
    }

    /// Включает и выключает автовоспроизведение.
    ///
    /// Новое значение действует со следующего открытия файла: текущее
    /// воспроизведение не останавливается и не запускается.
    pub fn set_autoplay(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.autoplay != enabled {
            self.autoplay = enabled;
            cx.notify();
        }
    }

    /// Переключает автовоспроизведение (пункт меню «⋯»).
    pub fn toggle_autoplay(&mut self, cx: &mut Context<Self>) {
        self.set_autoplay(!self.autoplay, cx);
    }

    /// Включает и выключает оверлей статистики.
    ///
    /// Настройка не сбрасывается при открытии нового файла: пользователь
    /// выбрал её для плеера, а не для конкретного ролика.
    pub fn set_stats_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.stats_visible != visible {
            self.stats_visible = visible;
            cx.notify();
        }
    }

    /// Переключает оверлей статистики (пункт меню «⋯»).
    pub fn toggle_stats(&mut self, cx: &mut Context<Self>) {
        self.set_stats_visible(!self.stats_visible, cx);
    }

    /// Находится ли окно плеера в полноэкранном режиме.
    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Обновляет признак полноэкранного режима для иконки кнопки.
    ///
    /// Окно остаётся источником истины: демо-обёртка вызывает метод из
    /// `render` по `Window::is_fullscreen()`, потому что платформа меняет
    /// состояние асинхронно после `toggle_fullscreen`.
    pub fn set_fullscreen(&mut self, fullscreen: bool, cx: &mut Context<Self>) {
        if self.fullscreen != fullscreen {
            self.fullscreen = fullscreen;
            cx.notify();
        }
    }

    /// Доля громкости под курсором (`0..=1`), пока он над ползунком.
    pub(crate) fn volume_hover(&self) -> Option<f32> {
        self.volume_hover
    }

    /// Показывает процент громкости для позиции курсора над ползунком.
    pub(crate) fn hover_volume_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(bounds) = self.volume_bounds else {
            return;
        };
        self.set_volume_hover(Some(ratio_at(bounds, x)), cx);
    }

    /// Включает и выключает подсказку громкости при входе/уходе курсора.
    pub(crate) fn set_volume_hover(&mut self, ratio: Option<f32>, cx: &mut Context<Self>) {
        let ratio = ratio.map(|value| value.clamp(0.0, 1.0));
        if self.volume_hover != ratio {
            self.volume_hover = ratio;
            cx.notify();
        }
    }

    /// Синхронизирует динамическую текстуру с последним кадром.
    ///
    /// Вызывается из фазы подготовки элемента: только там есть `Window`,
    /// которому можно поручить загрузку BGRA8. Если кадр не менялся или
    /// текстура уже актуальна, лишней загрузки не будет.
    pub(crate) fn sync_texture(&mut self, window: &mut Window) -> Option<VideoTexture> {
        let frame = self.frame.as_ref()?;
        let width = frame.width;
        let height = frame.height;
        let serial = self.frame_serial;
        let bytes_per_row = width.checked_mul(4)?;

        if let Some(existing) = self.texture {
            if existing.width == width && existing.height == height {
                if existing.serial != serial {
                    let bounds = texture_bounds(width, height)?;
                    if window
                        .update_dynamic_texture(
                            existing.texture,
                            bounds,
                            &frame.bgra,
                            bytes_per_row,
                        )
                        .is_ok()
                    {
                        self.texture = Some(VideoTexture { serial, ..existing });
                    }
                }
                return Some(existing);
            }

            // Разрешение изменилось: старую текстуру освобождаем.
            existing.texture.remove();
            self.texture = None;
        }

        let texture_size = texture_size(width, height)?;
        let texture = window
            .create_dynamic_texture(texture_size, &frame.bgra, bytes_per_row)
            .ok()?;
        let state = VideoTexture {
            texture,
            width,
            height,
            serial,
        };
        self.texture = Some(state);
        Some(state)
    }

    /// Сбрасывает кадр и освобождает текстуру.
    fn reset_video(&mut self) {
        if let Some(texture) = self.texture.take() {
            texture.texture.remove();
        }
        // Последний кадр больше не показывается — его буфер тоже в пул.
        if let Some(frame) = self.frame.take()
            && let Some(core) = self.core.as_ref()
        {
            core.recycle_frame(frame);
        }
        self.frame_serial = 0;
        self.progress_bounds = None;
        self.volume_bounds = None;
        self.scrub = None;
        self.seek_hint = None;
        self.volume_scrubbing = false;
        self.volume_hover = None;
    }

    /// Запоминает границы полосы прогресса (их сообщает canvas при отрисовке).
    pub(crate) fn set_progress_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.progress_bounds = Some(bounds);
    }

    /// Запоминает границы регулятора громкости.
    pub(crate) fn set_volume_bounds(&mut self, bounds: Bounds<Pixels>) {
        self.volume_bounds = Some(bounds);
    }

    /// Начало выбора позиции на полосе: только предпросмотр, без `Seek`.
    pub(crate) fn begin_scrub_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let (Some(bounds), Some(duration)) = (self.progress_bounds, self.duration) else {
            return;
        };
        self.scrub = Some(duration.mul_f32(ratio_at(bounds, x)));
        cx.notify();
    }

    /// Движение по полосе: обновляем только предпросмотр.
    pub(crate) fn update_scrub_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if self.scrub.is_some() {
            self.begin_scrub_from_x(x, cx);
        }
    }

    /// Отпускание мыши: отправляем одно `Seek` вместо потока команд.
    pub(crate) fn finish_scrub(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.scrub.take() {
            self.seek(position, cx);
        }
    }

    /// Начало изменения громкости.
    pub(crate) fn begin_volume_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        self.volume_scrubbing = true;
        self.apply_volume_x(x, cx);
    }

    /// Движение по регулятору громкости.
    pub(crate) fn update_volume_from_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if self.volume_scrubbing {
            self.apply_volume_x(x, cx);
        }
    }

    /// Отпускание мыши на регуляторе громкости.
    pub(crate) fn finish_volume(&mut self, _cx: &mut Context<Self>) {
        self.volume_scrubbing = false;
    }

    /// Выставляет громкость по координате на регуляторе.
    fn apply_volume_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let Some(bounds) = self.volume_bounds else {
            return;
        };
        let volume = ratio_at(bounds, x);
        self.muted = false;
        self.set_volume(volume, cx);
    }

    /// Громкость, которую надо отдать ядру с учётом выключенного звука.
    fn engine_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Отправляет команду ядру; при ошибке переводит плеер в `Failed`.
    fn send(&mut self, command: Command, cx: &mut Context<Self>) -> bool {
        let Some(core) = self.core.as_ref() else {
            return false;
        };
        match core.send(command) {
            Ok(()) => true,
            Err(send_error) => {
                error!(%send_error, "команда ядру не доставлена");
                self.error = Some(send_error.to_string());
                self.seek_hint = None;
                self.state = PlaybackState::Failed;
                cx.notify();
                false
            }
        }
    }

    /// Забирает накопившиеся события и самый свежий кадр. Перерисовку просим
    /// только если что-то действительно изменилось.
    fn drain_events(&mut self, cx: &mut Context<Self>) {
        let (events, frame) = match self.core.as_ref() {
            Some(core) => (
                std::iter::from_fn(|| core.try_recv_event()).collect::<Vec<_>>(),
                core.try_recv_frame(),
            ),
            None => return,
        };

        if events.is_empty() && frame.is_none() {
            return;
        }

        for event in events {
            match event {
                Event::Ready(ready) => {
                    let ready = *ready;
                    self.video_info = ready.video_info;
                    self.media_info = Some(ready.media_info);
                    self.duration = ready.duration;
                    self.seekable = ready.seekable;
                    self.seek_hint = None;
                    self.buffering = false;
                    self.state = PlaybackState::Ready;
                    self.error = None;
                    // Файл открывают, чтобы его посмотреть: при включённой
                    // настройке запускаем сразу, но только теперь, когда ядро
                    // действительно готово. Иначе `Play` терялся бы в `Loading`.
                    if self.autoplay {
                        self.play(cx);
                    }
                }
                Event::PositionChanged(position) => {
                    // Пока идёт перемотка, старые позиции не должны
                    // откатывать ползунок с выбранной цели.
                    if self.seek_hint.is_none() {
                        self.position = position;
                    }
                }
                Event::Seeked(position) => {
                    if let Some(hint) = self.seek_hint {
                        if hint.accepts(position) {
                            self.position = hint.target;
                            self.seek_hint = None;
                        }
                    } else {
                        self.position = position;
                    }
                }
                Event::Buffering(buffering) => self.buffering = buffering,
                Event::Stats(stats) => self.stats = Some(stats),
                Event::Spectrum(bars) => self.spectrum = Some(bars),
                Event::Ended => {
                    info!("воспроизведение завершено");
                    self.buffering = false;
                    self.seek_hint = None;
                    self.state = PlaybackState::Ended;
                }
                Event::Error(message) => {
                    error!(%message, "ошибка воспроизведения");
                    self.buffering = false;
                    self.seek_hint = None;
                    self.error = Some(message);
                    self.state = PlaybackState::Failed;
                }
            }
        }

        if let Some(frame) = frame {
            // Старый кадр больше не нужен: его буфер возвращается в пул ядра,
            // чтобы следующий кадр не выделял 8 МБ памяти заново.
            if let Some(previous) = self.frame.replace(frame)
                && let Some(core) = self.core.as_ref()
            {
                core.recycle_frame(previous);
            }
            self.frame_serial = self.frame_serial.wrapping_add(1);
        }

        cx.notify();
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            texture.texture.remove();
        }
    }
}

/// Размер текстуры в физических пикселях.
fn texture_size(width: u32, height: u32) -> Option<Size<DevicePixels>> {
    let width = i32::try_from(width).ok().filter(|value| *value > 0)?;
    let height = i32::try_from(height).ok().filter(|value| *value > 0)?;
    Some(size(DevicePixels(width), DevicePixels(height)))
}

/// Границы всей текстуры для загрузки кадра целиком.
fn texture_bounds(width: u32, height: u32) -> Option<Bounds<DevicePixels>> {
    Some(Bounds {
        origin: point(DevicePixels(0), DevicePixels(0)),
        size: texture_size(width, height)?,
    })
}

/// Первый путь из списка, который существует и указывает на файл.
fn first_file(paths: &[PathBuf]) -> Option<&PathBuf> {
    paths.iter().find(|path| path.is_file())
}

/// Плейлист не превьюится: это не медиафайл.
fn is_playlist(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("m3u8"))
}

/// Доля координаты внутри границ элемента, ограниченная диапазоном `[0, 1]`.
///
/// За ширину берём расстояние между центрами крайних пикселей (`width - 1`).
/// Иначе клик по самому правому пикселю даёт около `0.99`, и громкость не
/// доходит до `100 %`.
fn ratio_at(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return 0.0;
    }

    let span = (width - 1.0).max(1.0);
    ((f32::from(x) - f32::from(bounds.origin.x)) / span).clamp(0.0, 1.0)
}

/// Задача, которая раз в `EVENT_POLL_INTERVAL` забирает события и кадры ядра.
///
/// Цикл заканчивается сам, когда сущность удалена: `weak.update` вернёт ошибку.
fn spawn_event_pump(cx: &mut Context<VideoPlayer>) -> Task<()> {
    cx.spawn(
        async move |this: WeakEntity<VideoPlayer>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(EVENT_POLL_INTERVAL).await;
                if this
                    .update(cx, |video: &mut VideoPlayer, cx| video.drain_events(cx))
                    .is_err()
                {
                    return;
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    #[test]
    fn seek_hint_waits_for_the_target() {
        let forward = SeekHint {
            from: Duration::from_secs(10),
            target: Duration::from_secs(20),
        };
        assert!(!forward.accepts(Duration::from_secs(15)));
        assert!(forward.accepts(Duration::from_millis(19_900)));
        assert!(forward.accepts(Duration::from_secs(20)));

        let backward = SeekHint {
            from: Duration::from_secs(20),
            target: Duration::from_secs(10),
        };
        assert!(!backward.accepts(Duration::from_secs(15)));
        assert!(backward.accepts(Duration::from_millis(10_100)));
        assert!(backward.accepts(Duration::from_secs(10)));
    }

    fn slider_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(10.0), px(0.0)),
            size: size(px(100.0), px(10.0)),
        }
    }

    #[test]
    fn dropped_paths_give_the_first_existing_file() {
        let directory = std::env::temp_dir();
        let existing = directory.join(format!("vgpu-player-drop-{}.mp4", std::process::id()));
        std::fs::write(&existing, b"test").expect("создать файл для проверки drop");
        let missing = directory.join("vgpu-player-drop-missing.mp4");

        // Каталог и отсутствующий путь пропускаются, остаётся настоящий файл.
        let candidates = [missing, directory, existing.clone()];
        let picked = first_file(&candidates);
        let _ = std::fs::remove_file(&existing);

        assert_eq!(picked, Some(&existing));
    }

    #[test]
    fn dropped_paths_without_files_give_nothing() {
        assert!(first_file(&[PathBuf::from("missing.mp4")]).is_none());
        assert!(first_file(&[]).is_none());
    }

    #[test]
    fn ratio_maps_to_slider_bounds() {
        assert_eq!(ratio_at(slider_bounds(), px(10.0)), 0.0);
        assert!((ratio_at(slider_bounds(), px(59.5)) - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn right_edge_reaches_full_volume() {
        // Крайний правый пиксель должен давать ровно 1.0: иначе ползунок
        // громкости упирается в 99 %.
        assert_eq!(ratio_at(slider_bounds(), px(109.0)), 1.0);
    }

    #[test]
    fn ratio_clamps_to_range() {
        assert_eq!(ratio_at(slider_bounds(), px(0.0)), 0.0);
        assert_eq!(ratio_at(slider_bounds(), px(200.0)), 1.0);
    }

    #[test]
    fn zero_width_slider_gives_zero_ratio() {
        let empty = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(0.0), px(10.0)),
        };
        assert_eq!(ratio_at(empty, px(50.0)), 0.0);
    }
}
