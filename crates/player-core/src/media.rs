//! Открытые источники и метаданные о дорожках.
//!
//! [`Media`] — это то, что ядро получает при открытии: демультиплексоры и
//! декодеры обеих дорожек. Звук и видео независимы: источник может содержать
//! только одну из дорожек, и тогда открывается то, что удалось.

use std::path::Path;
use std::time::Duration;

use ffmpeg::media::Type;
use tracing::{debug, error, info};

use crate::PlayerError;
use crate::audio::AudioDecoder;
use crate::source::Source as MediaSource;
use crate::video::{VideoDecoder, VideoInfo};

/// Открытые источники файла. Любой из них может отсутствовать.
pub(crate) struct Media {
    pub(crate) audio: Option<AudioDecoder>,
    pub(crate) video: Option<VideoDecoder>,
}

impl Media {
    /// Открывает то, что удалось: звук и видео независимы.
    ///
    /// Ошибку одной дорожки при живой другой не считаем фатальной: ролик без
    /// звука или со сломанным видео должен всё равно играть. Когда не открылось
    /// ничего, возвращаем ошибку звука — она возникает первой и обычно точнее
    /// описывает проблему с источником. Подробные состояния ошибок — этап 7.
    /// Открывает источник для тестов: без контроля отмены.
    #[cfg(test)]
    pub(crate) fn open(source: &MediaSource) -> Result<Self, PlayerError> {
        Self::open_with(source, &|| false)
    }

    /// Открывает источник и между шагами проверяет, не отменили ли открытие.
    ///
    /// Шаги — звук, видео и заголовок. Внутри самого декодера отмена станет
    /// возможна на этапе 3, когда появится сетевой читатель.
    pub(crate) fn open_with(
        source: &MediaSource,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, PlayerError> {
        if cancelled() {
            return Err(PlayerError::Cancelled);
        }

        // Плейлист — не медиапоток: до него ещё нет HLS-клиента.
        if source.is_hls() {
            return Err(PlayerError::UnsupportedHls {
                target: source.clone(),
            });
        }

        // Пустой файл даёт от Symphonia и FFmpeg разные непонятные тексты,
        // поэтому проверяем его сами и говорим прямо.
        if let Some(path) = source.as_file()
            && let Ok(metadata) = std::fs::metadata(path)
            && metadata.is_file()
            && metadata.len() == 0
        {
            return Err(PlayerError::EmptyFile {
                target: source.clone(),
            });
        }

        let audio = AudioDecoder::open(source);
        if cancelled() {
            return Err(PlayerError::Cancelled);
        }
        let video = VideoDecoder::open_source(source);
        if cancelled() {
            return Err(PlayerError::Cancelled);
        }

        match (audio, video) {
            (Ok(audio), Ok(video)) => {
                info!(source = %source, "открыты звук и видео");
                Ok(Self {
                    audio: Some(audio),
                    video: Some(video),
                })
            }
            (Ok(audio), Err(video_error)) => {
                debug!(source = %source, %video_error, "видеодорожка недоступна");
                Ok(Self {
                    audio: Some(audio),
                    video: None,
                })
            }
            (Err(audio_error), Ok(video)) => {
                debug!(source = %source, %audio_error, "аудиодорожка недоступна");
                Ok(Self {
                    audio: None,
                    video: Some(video),
                })
            }
            (Err(audio_error), Err(video_error)) => {
                error!(
                    source = %source,
                    %audio_error,
                    %video_error,
                    "источник не открылся ни как звук, ни как видео"
                );
                Err(audio_error)
            }
        }
    }

    /// Длительность: из звука, иначе из видео.
    pub(crate) fn duration(&self) -> Option<Duration> {
        self.audio
            .as_ref()
            .and_then(|audio| audio.duration())
            .or_else(|| self.video.as_ref().and_then(|video| video.info().duration))
    }

    /// Метаданные видео, если дорожка открыта.
    pub(crate) fn video_info(&self) -> Option<VideoInfo> {
        self.video.as_ref().map(|video| *video.info())
    }

    /// Собирает событие `Ready`: всё, что UI нужно узнать об открытом источнике.
    pub(crate) fn ready(&self, source: &MediaSource) -> MediaReady {
        MediaReady {
            media_info: self.info(source),
            video_info: self.video_info(),
            duration: self.duration(),
            seekable: is_seekable(source),
            live: false,
        }
    }

    /// Что известно о содержимом источника — для панели «Информация о файле».
    ///
    /// Для локального файла заголовок читается отдельным проходом; для URL
    /// его нет, поэтому контейнер и кодеки берём из уже открытых декодеров.
    pub(crate) fn info(&self, source: &MediaSource) -> MediaInfo {
        let ProbedTracks {
            container,
            video_codec,
            audio_codec,
        } = match source {
            MediaSource::File(path) => probe_tracks(path),
            MediaSource::Url(_) => ProbedTracks {
                container: self
                    .video
                    .as_ref()
                    .and_then(|video| video.container_name())
                    .or_else(|| self.audio.as_ref().and_then(|audio| audio.container_name()))
                    .map(str::to_owned),
                video_codec: self
                    .video
                    .as_ref()
                    .and_then(|video| video.codec_name())
                    .map(str::to_owned),
                audio_codec: self
                    .audio
                    .as_ref()
                    .and_then(|audio| audio.codec_name())
                    .map(str::to_owned),
            },
        };

        MediaInfo {
            container,
            video: self.video.as_ref().map(|video| {
                let info = *video.info();
                VideoTrackInfo {
                    codec: video_codec,
                    width: info.width,
                    height: info.height,
                    frame_rate: info.frame_rate,
                }
            }),
            audio: self.audio.as_ref().map(|audio| AudioTrackInfo {
                codec: audio_codec,
                channels: audio.channels(),
                sample_rate: audio.sample_rate(),
            }),
        }
    }
}

/// Сообщение об успешном открытии источника.
///
/// Всё, что UI узнаёт о файле, приходит одним событием: метаданные дорожек,
/// длительность и возможность перемотки. Для live-потоков `duration` будет
/// `None`, а `live` — `true` (HLS, этап 5).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaReady {
    /// Контейнер, кодеки и параметры звука.
    pub media_info: MediaInfo,
    /// Размеры и частота кадров видеодорожки.
    pub video_info: Option<VideoInfo>,
    /// Длительность источника, если она известна.
    pub duration: Option<Duration>,
    /// Поддерживает ли источник перемотку.
    pub seekable: bool,
    /// Идёт ли поток в реальном времени (пока всегда `false`).
    pub live: bool,
}

/// Перематывать можно только локальный источник: у URL это решит `HttpReader`
/// по ответу на `Range` (этап 3).
fn is_seekable(source: &MediaSource) -> bool {
    source.as_file().is_some()
}

/// Что известно о содержимом открытого файла.
///
/// Собирается один раз при открытии: контейнер и имена кодеков — из заголовка
/// файла, размеры и частота — из реального видеодекодера, каналы и частота
/// звука — из открытого аудиодекодера (в MP4 заголовок врёт про каналы).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaInfo {
    /// Контейнер по-человечески, например `QuickTime / MOV`.
    pub container: Option<String>,
    /// Видеодорожка, если она открылась.
    pub video: Option<VideoTrackInfo>,
    /// Аудиодорожка, если она открылась.
    pub audio: Option<AudioTrackInfo>,
}

/// Параметры видеодорожки.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoTrackInfo {
    /// Короткое имя кодека FFmpeg: `h264`, `hevc`, `av1`.
    pub codec: Option<String>,
    /// Ширина кадра в пикселях.
    pub width: u32,
    /// Высота кадра в пикселях.
    pub height: u32,
    /// Частота кадров, если контейнер её сообщает.
    pub frame_rate: Option<f64>,
}

/// Параметры аудиодорожки.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrackInfo {
    /// Короткое имя кодека FFmpeg: `aac`, `opus`.
    pub codec: Option<String>,
    /// Число каналов.
    pub channels: u16,
    /// Частота дискретизации в герцах.
    pub sample_rate: u32,
}

/// Имена контейнера и кодеков: то, чего не знают декодеры сами по себе.
#[derive(Default)]
struct ProbedTracks {
    container: Option<String>,
    video_codec: Option<String>,
    audio_codec: Option<String>,
}

/// Читает заголовок файла ради названия контейнера и имён кодеков.
///
/// Отдельный проход нужен потому, что звук открывает Symphonia (она не даёт
/// короткое имя кодека в том же виде), а у файла без видео нет и
/// `VideoDecoder`. Заголовок читается без декодирования кадров, поэтому это
/// дешевле любого из путей воспроизведения. Ошибка чтения не фатальна: файл
/// уже открыт декодерами, без имён просто не будет строк в информации.
fn probe_tracks(path: &Path) -> ProbedTracks {
    let Ok(input) = ffmpeg::format::input(&path) else {
        return ProbedTracks::default();
    };

    let codec_of = |kind: Type| {
        input
            .streams()
            .best(kind)
            .map(|stream| stream.parameters().id().name().to_owned())
            .filter(|name| !name.is_empty() && name != "none")
    };

    ProbedTracks {
        container: Some(input.format().description().to_owned()),
        video_codec: codec_of(Type::Video),
        audio_codec: codec_of(Type::Audio),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::test_support::sample_video;

    #[test]
    fn ready_describes_an_open_video() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };
        let source = MediaSource::File(path);
        let media = Media::open(&source).expect("открыть тестовое видео");

        let ready = media.ready(&source);

        assert_eq!(ready.video_info, media.video_info());
        assert_eq!(ready.duration, media.duration());
        assert!(ready.media_info.video.is_some(), "метаданные видео");
        assert!(ready.media_info.container.is_some(), "контейнер");
        assert!(ready.seekable, "локальный файл перематывается");
        assert!(!ready.live, "файл не является live-потоком");
    }

    #[test]
    fn url_sources_are_not_seekable_yet() {
        let source = MediaSource::Url("https://example.com/video.mp4".to_owned());

        assert!(!is_seekable(&source));
    }
}
