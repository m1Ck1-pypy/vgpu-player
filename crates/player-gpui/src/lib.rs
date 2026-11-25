//! Библиотека UI плеера на GPUI.
//!
//! Три части, которые можно брать по отдельности:
//!
//! - [`VideoPlayer`] — сущность GPUI (`Entity<VideoPlayer>`) с состоянием и
//!   командами; сама общается с ядром `player-core`;
//! - [`video_view`] — виджет кадра; возвращает обычный `Div`, поэтому
//!   стилизуется как любой элемент; для файла без видео рисует
//!   спектроанализатор.
//! - [`controls_view`] — необязательная панель с кнопками-иконками
//!   (Play/Pause, звук, fullscreen) и слайдерами.
//!
//! Минимальное подключение в чужом приложении:
//!
//! ```no_run
//! use gpui::{App, Context, div, prelude::*};
//! use player_gpui::{VideoPlayer, controls_view, video_view};
//!
//! struct Screen {
//!     player: gpui::Entity<VideoPlayer>,
//! }
//!
//! impl Screen {
//!     fn new(path: std::path::PathBuf, cx: &mut App) -> Self {
//!         Self {
//!             player: VideoPlayer::create(path, cx),
//!         }
//!     }
//! }
//!
//! impl gpui::Render for Screen {
//!     fn render(
//!         &mut self,
//!         _window: &mut gpui::Window,
//!         cx: &mut Context<Self>,
//!     ) -> impl gpui::IntoElement {
//!         div()
//!             .flex()
//!             .flex_col()
//!             .child(video_view(&self.player, cx).size_full().rounded_md())
//!             .child(controls_view(&self.player, cx))
//!     }
//! }
//! ```
//!
//! Готовая обёртка для бинарника — [`run_app`].

pub use gpui;

/// Платформенный слой GPUI: с него начинается цикл событий приложения.
///
/// Реэкспорт гарантирует ту же версию платформы, что и у [`gpui`]: чужому
/// приложению достаточно `player_gpui::gpui_platform::application()`, чтобы
/// показать своё окно с плеером.
pub use gpui_platform;

/// Типы, которые возвращает [`VideoPlayer`]: метаданные файла и уровни спектра.
pub use player_core::{
    AudioTrackInfo, HttpOptions, HttpOptionsError, MediaInfo, SPECTRUM_BARS, Source, SourceError,
    SpectrumBars, VideoTrackInfo,
};

mod app;
mod controls;
mod icons;
mod open_panel;
mod settings;
mod video_player;
mod video_view;

/// Палитра интерфейса, снятая с иконки приложения.
///
/// Оранжевый корпус — основной акцент (Play/Replay, заполнение слайдеров),
/// жёлтые «кнопки» плёнки — вторичный (отметки меню, `LIVE`, каретка), белый
/// треугольник — текст и иконки. Фоны тёмные и тёплые, чтобы кадр оставался
/// главным на экране.
mod theme {
    /// Корпус иконки: основной акцент.
    pub(crate) const ACCENT: u32 = 0xef_8a_2e;
    /// Жёлтые кирпичики плёнки: вторичный акцент.
    pub(crate) const HIGHLIGHT: u32 = 0xff_d9_3d;
    /// Белый треугольник Play: основной текст и иконки.
    pub(crate) const TEXT: u32 = 0xf7_ef_e6;
    /// Тёмный текст на оранжевых и жёлтых плашках.
    pub(crate) const ON_ACCENT: u32 = 0x1b_12_0c;

    /// Фон окна и поля кадра.
    pub(crate) const BACKGROUND: u32 = 0x14_0f_0b;
    /// Меню, карточки диалогов и панель информации.
    pub(crate) const SURFACE: u32 = 0x24_1a_12;
    /// Кнопки, наведение и выбранный пункт.
    pub(crate) const RAISED: u32 = 0x3a_28_1a;
    /// Границы и незаполненная дорожка слайдера.
    pub(crate) const BORDER: u32 = 0x4a_35_24;
    /// Поле ввода ссылки.
    pub(crate) const INPUT: u32 = 0x16_0f_0a;
    /// Подсказки и второстепенный текст.
    pub(crate) const MUTED: u32 = 0xb3_9d_8c;
    /// Плейсхолдер: темнее подсказок.
    pub(crate) const FAINT: u32 = 0x7e_6a_5b;
    /// Текст ошибки в панели «Открыть…».
    pub(crate) const ERROR: u32 = 0xff_8a_7a;

    /// Панель управления поверх кадра.
    pub(crate) const CONTROLS_BG: u32 = 0x1b_12_0c_cc;
    /// Плашки статистики и буферизации.
    pub(crate) const OVERLAY_BG: u32 = 0x1b_12_0c_cc;
    /// Плашка громкости и подсказка над ползунком.
    pub(crate) const NOTICE_BG: u32 = 0x2a_1e_14_ee;
    /// Затемнение под меню, диалогами и подсказкой drag-and-drop.
    pub(crate) const BACKDROP: u32 = 0x14_0f_0b_73;
}

pub use app::{
    LaunchError, PlayerOptions, WINDOW_TITLE, run_app, run_app_source, run_app_source_with,
};
pub use controls::controls_view;
pub use video_player::{PlaybackState, VideoPlayer};
pub use video_view::video_view;

/// Примеры кода из README проверяются doc-тестами: они должны компилироваться.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
