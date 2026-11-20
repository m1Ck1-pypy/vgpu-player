//! Ошибки ядра.
//!
//! Свои варианты на каждый этап отказа вместо `String`: вызывающий код должен
//! видеть, что именно случилось. Тексты сообщений — часть контракта с UI,
//! поэтому `thiserror` повторяет их дословно, а источник ошибки остаётся
//! доступен через [`std::error::Error::source`].
//!
//! Ошибка несёт [`Source`], а не голый путь: для файла текст тот же, а для URL
//! подставляется `Source::log_name()` без query и fragment.

use std::io;

use symphonia::core::errors::Error as SymphoniaError;

use crate::{Source, SourceError};

/// Что именно не удалось сделать с источником или устройством вывода.
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    /// Источник не открывается (нет файла, нет прав).
    #[error("не удалось открыть {target}: {source}")]
    OpenFile {
        /// Источник, который пытались открыть.
        target: Source,
        /// Причина от файловой системы.
        #[source]
        source: io::Error,
    },
    /// Контейнер или кодек не распознан.
    #[error("формат {target} не поддерживается: {source}")]
    Unsupported {
        /// Источник.
        target: Source,
        /// Причина от Symphonia.
        #[source]
        source: SymphoniaError,
    },
    /// Файл существует, но в нём нет данных.
    #[error("файл {target} пуст")]
    EmptyFile {
        /// Источник.
        target: Source,
    },
    /// В источнике нет дорожки, которую можно декодировать.
    #[error("в {target} нет пригодной аудиодорожки")]
    NoPlayableTrack {
        /// Источник.
        target: Source,
    },
    /// Параметры потока, с которыми не умеет работать вывод.
    #[error("некорректные параметры звука: {channels} каналов, {sample_rate} Гц")]
    InvalidAudioParams {
        /// Число каналов из заголовка.
        channels: u16,
        /// Частота дискретизации из заголовка.
        sample_rate: u32,
    },
    /// Ошибка декодирования в середине источника.
    #[error("ошибка декодирования: {0}")]
    Decode(#[source] SymphoniaError),
    /// В источнике нет видеодорожки.
    #[error("в {target} нет видеодорожки")]
    NoVideoTrack {
        /// Источник.
        target: Source,
    },
    /// FFmpeg не смог открыть или декодировать видео.
    ///
    /// Текст FFmpeg хранится строкой: у `ffmpeg::Error` нет источника с путём,
    /// а вариант должен нести и источник, и причину в одном сообщении.
    #[error("ошибка видео в {target}: {message}")]
    VideoDecode {
        /// Источник.
        target: Source,
        /// Текст ошибки FFmpeg.
        message: String,
    },
    /// FFmpeg не смог открыть или декодировать звук.
    #[error("ошибка звука в {target}: {message}")]
    AudioDecode {
        /// Источник.
        target: Source,
        /// Текст ошибки FFmpeg.
        message: String,
    },
    /// HLS-плейлист пока не поддерживается.
    #[error("HLS пока не поддерживается: {target}")]
    UnsupportedHls {
        /// Источник.
        target: Source,
    },
    /// Сетевой источник ещё не подключён (этап 3).
    #[error("источник пока не поддерживается: {target}")]
    UnsupportedSource {
        /// Источник.
        target: Source,
    },
    /// По ссылке пришёл пустой ответ.
    #[error("по ссылке нет данных: {target}")]
    EmptySource {
        /// Источник.
        target: Source,
    },
    /// Сервер ответил не-2xx.
    #[error("сервер вернул {status} для {target}")]
    HttpStatus {
        /// Источник.
        target: Source,
        /// Код ответа.
        status: u16,
    },
    /// Сеть: таймаут, DNS, TLS или обрыв.
    #[error("сеть: {message}")]
    Network {
        /// Источник.
        target: Source,
        /// Причина в готовом для показа виде.
        message: String,
    },
    /// Ответ не является медиафайлом (например, `text/html`).
    #[error("по ссылке не медиафайл: {target}")]
    NotMedia {
        /// Источник.
        target: Source,
    },
    /// Источник не поддерживает перемотку.
    #[error("источник не поддерживает перемотку: {target}")]
    NotSeekable {
        /// Источник.
        target: Source,
    },
    /// Строку не удалось разобрать как URL или путь.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// Открытие отменено: пришёл `Stop`, новый `Open` или `Player` уничтожен.
    ///
    /// Внутренний путь: до UI эта ошибка не доходит, ядро молча ждёт
    /// следующую команду.
    #[error("открытие отменено")]
    Cancelled,
    /// Звуковое устройство недоступно.
    #[error("звуковое устройство недоступно: {0}")]
    AudioDevice(#[source] rodio::DeviceSinkError),
    /// Поток воспроизведения не удалось запустить.
    #[error("не удалось запустить поток ядра: {0}")]
    StartEngine(#[source] io::Error),
    /// Ядро уже завершило работу и не принимает команды.
    #[error("ядро воспроизведения уже остановлено")]
    EngineGone,
    /// Ошибка пришла из ядра событием (на этапе 1 — текстом).
    #[error("{0}")]
    Playback(String),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn file(name: &str) -> Source {
        Source::File(PathBuf::from(name))
    }

    /// Тексты ошибок видит пользователь в окне и в консоли, поэтому они
    /// проверяются как часть контракта.
    #[test]
    fn messages_name_the_file_and_reason() {
        let error = PlayerError::NoVideoTrack {
            target: file("movie.mp4"),
        };
        assert_eq!(error.to_string(), "в movie.mp4 нет видеодорожки");

        let error = PlayerError::EmptyFile {
            target: file("empty.mp4"),
        };
        assert_eq!(error.to_string(), "файл empty.mp4 пуст");

        let error = PlayerError::NoPlayableTrack {
            target: file("sound.mp3"),
        };
        assert_eq!(error.to_string(), "в sound.mp3 нет пригодной аудиодорожки");

        let error = PlayerError::VideoDecode {
            target: file("clip.webm"),
            message: "invalid data".to_owned(),
        };
        assert_eq!(error.to_string(), "ошибка видео в clip.webm: invalid data");

        let error = PlayerError::AudioDecode {
            target: file("sound.webm"),
            message: "invalid data".to_owned(),
        };
        assert_eq!(error.to_string(), "ошибка звука в sound.webm: invalid data");

        let error = PlayerError::OpenFile {
            target: file("missing.mp4"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(
            error.to_string(),
            "не удалось открыть missing.mp4: not found"
        );
    }

    #[test]
    fn url_errors_hide_query_and_fragment() {
        let target = Source::Url("https://example.com/v.mp4?token=secret#t".to_owned());

        let error = PlayerError::UnsupportedHls { target };
        assert_eq!(
            error.to_string(),
            "HLS пока не поддерживается: https://example.com/v.mp4?…"
        );
    }

    #[test]
    fn network_errors_name_the_link() {
        let target = Source::Url("https://example.com/v.mkv?token=secret".to_owned());
        let hidden = "https://example.com/v.mkv?…";

        assert_eq!(
            PlayerError::EmptySource {
                target: target.clone()
            }
            .to_string(),
            format!("по ссылке нет данных: {hidden}")
        );
        assert_eq!(
            PlayerError::HttpStatus {
                target: target.clone(),
                status: 404
            }
            .to_string(),
            format!("сервер вернул 404 для {hidden}")
        );
        assert_eq!(
            PlayerError::NotMedia {
                target: target.clone()
            }
            .to_string(),
            format!("по ссылке не медиафайл: {hidden}")
        );
        assert_eq!(
            PlayerError::NotSeekable { target }.to_string(),
            format!("источник не поддерживает перемотку: {hidden}")
        );
    }

    #[test]
    fn network_failure_is_a_plain_reason() {
        let error = PlayerError::Network {
            target: Source::Url("https://example.com/v.mp4".to_owned()),
            message: "connection refused".to_owned(),
        };

        assert_eq!(error.to_string(), "сеть: connection refused");
    }

    #[test]
    fn source_error_is_transparent() {
        let error = PlayerError::from(SourceError::Empty);
        assert_eq!(
            error.to_string(),
            "пустая строка не является ни URL, ни путём"
        );
    }

    #[test]
    fn decode_error_keeps_source_chain() {
        let error = PlayerError::Decode(SymphoniaError::IoError(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "конец файла",
        )));

        assert!(error.to_string().starts_with("ошибка декодирования: "));
        let source = std::error::Error::source(&error).expect("источник ошибки");
        assert!(source.to_string().contains("конец файла"));
    }

    #[test]
    fn engine_gone_is_plain_text() {
        assert_eq!(
            PlayerError::EngineGone.to_string(),
            "ядро воспроизведения уже остановлено"
        );
    }
}
