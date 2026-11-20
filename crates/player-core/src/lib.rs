//! Ядро плеера: демультиплексирование, декодирование, синхронизация A/V
//! и конечный автомат воспроизведения.
//!
//! Крейт не зависит от UI и платформенного кода, поэтому его можно
//! тестировать в headless-режиме.
//!
//! Наружу торчит только небольшой набор типов: [`Player`] для управления,
//! [`Command`] и [`Event`] как контракт с UI, [`PlayerError`] для ошибок.

mod audio;
mod clock;
mod command;
mod engine;
mod error;
mod event;
mod media;
mod player;
mod reader;
mod source;
mod spectrum;
mod sync;
mod versions;
mod video;

pub use command::Command;
pub use error::PlayerError;
pub use event::Event;
pub use media::{AudioTrackInfo, MediaInfo, MediaReady, VideoTrackInfo};
pub use player::Player;
pub use source::{Source, SourceError};
pub use spectrum::{SPECTRUM_BARS, SpectrumBars};
pub use sync::SyncStats;
pub use versions::{FfmpegVersion, FfmpegVersions};
pub use video::{VideoDecoder, VideoFrame, VideoInfo, preview_frame};
