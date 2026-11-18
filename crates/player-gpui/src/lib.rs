//! Библиотека UI плеера на GPUI.
//!
//! Три части, которые можно брать по отдельности:
//!
//! - [`VideoPlayer`] — сущность GPUI (`Entity<VideoPlayer>`) с состоянием и
//!   командами; сама общается с ядром `player-core`;
//! - [`video_view`] — виджет кадра; возвращает обычный `Div`, поэтому
//!   стилизуется как любой элемент;
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

/// Типы метаданных файла, которые возвращает [`VideoPlayer::media_info`].
pub use player_core::{AudioTrackInfo, MediaInfo, VideoTrackInfo};

mod app;
mod controls;
mod icons;
mod settings;
mod video_player;
mod video_view;

pub use app::{LaunchError, WINDOW_TITLE, run_app};
pub use controls::controls_view;
pub use video_player::{PlaybackState, VideoPlayer};
pub use video_view::video_view;

/// Примеры кода из README проверяются doc-тестами: они должны компилироваться.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
