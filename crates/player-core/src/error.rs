//! Ошибки ядра.
//!
//! Свои варианты на каждый этап отказа вместо `String`: вызывающий код должен
//! видеть, что именно случилось. Тексты сообщений — часть контракта с UI,
//! поэтому `thiserror` повторяет их дословно, а источник ошибки остаётся
//! доступен через [`std::error::Error::source`].

use std::io;
use std::path::PathBuf;

use symphonia::core::errors::Error as SymphoniaError;

/// Что именно не удалось сделать с файлом или устройством вывода.
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    /// Файл не открывается (нет файла, нет прав).
    #[error("не удалось открыть {}: {source}", path.display())]
    OpenFile {
        /// Путь, который пытались открыть.
        path: PathBuf,
        /// Причина от файловой системы.
        #[source]
        source: io::Error,
    },
    /// Контейнер или кодек не распознан.
    #[error("формат {} не поддерживается: {source}", path.display())]
    Unsupported {
        /// Путь к файлу.
        path: PathBuf,
        /// Причина от Symphonia.
        #[source]
        source: SymphoniaError,
    },
    /// Файл существует, но в нём нет данных.
    #[error("файл {} пуст", path.display())]
    EmptyFile {
        /// Путь к файлу.
        path: PathBuf,
    },
    /// В файле нет дорожки, которую можно декодировать.
    #[error("в {} нет пригодной аудиодорожки", path.display())]
    NoPlayableTrack {
        /// Путь к файлу.
        path: PathBuf,
    },
    /// Параметры потока, с которыми не умеет работать вывод.
    #[error("некорректные параметры звука: {channels} каналов, {sample_rate} Гц")]
    InvalidAudioParams {
        /// Число каналов из заголовка.
        channels: u16,
        /// Частота дискретизации из заголовка.
        sample_rate: u32,
    },
    /// Ошибка декодирования в середине файла.
    #[error("ошибка декодирования: {0}")]
    Decode(#[source] SymphoniaError),
    /// В файле нет видеодорожки.
    #[error("в {} нет видеодорожки", path.display())]
    NoVideoTrack {
        /// Путь к файлу.
        path: PathBuf,
    },
    /// FFmpeg не смог открыть или декодировать видео.
    ///
    /// Текст FFmpeg хранится строкой: у `ffmpeg::Error` нет источника с путём,
    /// а вариант должен нести и путь, и причину в одном сообщении.
    #[error("ошибка видео в {}: {message}", path.display())]
    VideoDecode {
        /// Путь к файлу.
        path: PathBuf,
        /// Текст ошибки FFmpeg.
        message: String,
    },
    /// FFmpeg не смог открыть или декодировать звук.
    #[error("ошибка звука в {}: {message}", path.display())]
    AudioDecode {
        /// Путь к файлу.
        path: PathBuf,
        /// Текст ошибки FFmpeg.
        message: String,
    },
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
    use super::*;

    /// Тексты ошибок видит пользователь в окне и в консоли, поэтому они
    /// проверяются как часть контракта.
    #[test]
    fn messages_name_the_file_and_reason() {
        let error = PlayerError::NoVideoTrack {
            path: PathBuf::from("movie.mp4"),
        };
        assert_eq!(error.to_string(), "в movie.mp4 нет видеодорожки");

        let error = PlayerError::EmptyFile {
            path: PathBuf::from("empty.mp4"),
        };
        assert_eq!(error.to_string(), "файл empty.mp4 пуст");

        let error = PlayerError::NoPlayableTrack {
            path: PathBuf::from("sound.mp3"),
        };
        assert_eq!(error.to_string(), "в sound.mp3 нет пригодной аудиодорожки");

        let error = PlayerError::VideoDecode {
            path: PathBuf::from("clip.webm"),
            message: "invalid data".to_owned(),
        };
        assert_eq!(error.to_string(), "ошибка видео в clip.webm: invalid data");

        let error = PlayerError::AudioDecode {
            path: PathBuf::from("sound.webm"),
            message: "invalid data".to_owned(),
        };
        assert_eq!(error.to_string(), "ошибка звука в sound.webm: invalid data");

        let error = PlayerError::OpenFile {
            path: PathBuf::from("missing.mp4"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        };
        assert_eq!(
            error.to_string(),
            "не удалось открыть missing.mp4: not found"
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
