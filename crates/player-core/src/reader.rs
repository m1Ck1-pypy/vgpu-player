//! Потоковый слой: то, что демультиплексор читает вместо файла.
//!
//! Декодеры получают [`MediaReader`] и не знают, откуда пришли байты — с
//! диска или из сети. Это позволяет держать один код чтения для файлов и URL
//! и позже заменить `FileReader` на HTTP-читатель без правок декодеров.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::hls::HlsSource;
use crate::http::{HttpConfig, HttpFailure, HttpReader, HttpSource};
use crate::{PlayerError, Source};

/// Источник байтов для FFmpeg.
///
/// Требования к реализации:
///
/// - `Read` блокирующий: `WouldBlock`/`TimedOut` наружу не отдаются;
/// - `SeekFrom::End` поддерживается, если `is_seekable()` — по нему FFmpeg
///   узнаёт размер (`avio_size`);
/// - позиция меняется только после успешного ответа источника.
pub(crate) trait MediaReader: Read + Seek + Send {
    /// Размер источника, если он известен.
    fn len(&self) -> Option<u64>;
    /// Поддерживает ли источник произвольный доступ.
    fn is_seekable(&self) -> bool;
    /// Имя с расширением — подсказка FFmpeg при определении формата.
    fn hint_name(&self) -> Option<&str>;
}

/// Открытый локальный файл.
pub(crate) struct FileReader {
    file: File,
    len: Option<u64>,
    hint: Option<String>,
}

impl FileReader {
    /// Открывает файл и запоминает его размер и имя.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata().ok().map(|metadata| metadata.len());
        let hint = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);

        Ok(Self { file, len, hint })
    }
}

impl Read for FileReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for FileReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

impl MediaReader for FileReader {
    fn len(&self) -> Option<u64> {
        self.len
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn hint_name(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

/// Какая дорожка запрашивает читатель.
///
/// Файловый и обычный HTTP-источник дорожку игнорируют: звук и видео читают
/// один и тот же поток. HLS выбирает по дорожке свой плейлист.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Track {
    Video,
    Audio,
}

/// Открытый читатель и слот сетевой ошибки, если она есть.
pub(crate) struct OpenedReader {
    pub(crate) reader: Box<dyn MediaReader>,
    /// Слот HTTP-ошибки: декодер заменит им `invalid data` от FFmpeg.
    pub(crate) failure: Option<Arc<Mutex<Option<HttpFailure>>>>,
    /// Флаг «ждём сеть» для события `Buffering`.
    pub(crate) buffering: Option<Arc<AtomicBool>>,
    /// Признак `#EXT-X-DISCONTINUITY`: декодер должен сбросить состояние.
    pub(crate) discontinuity: Option<Arc<AtomicBool>>,
}

/// Уже открытый источник, общий для звука и видео.
///
/// Для обычного URL это кэш [`HttpSource`], для HLS — [`HlsSource`] с двумя
/// плейлистами. Декодеры не различают эти случаи, а читатель выбирает
/// дорожку по [`Track`].
#[derive(Clone)]
pub(crate) enum SharedSource {
    Http(Arc<HttpSource>),
    Hls(Arc<HlsSource>),
}

/// Открывает читатель для источника.
///
/// Файл даёт [`FileReader`], URL — [`HttpReader`] поверх общего кэша или
/// [`crate::hls::HlsReader`] поверх HLS-плейлистов. Декодеры дальше не знают,
/// откуда пришли байты. `cancel` — флаг ядра: сетевой читатель должен
/// прервать блокирующее чтение при `Stop`/`Drop`. `shared` — уже открытый
/// источник: кэш и счётчики у звука и видео общие. `track` важен только для
/// HLS; файл и обычный HTTP-источник дорожку игнорируют.
pub(crate) fn open_reader(
    source: &Source,
    track: Track,
    cancel: Arc<AtomicBool>,
    shared: Option<&SharedSource>,
) -> Result<OpenedReader, PlayerError> {
    match source {
        Source::File(path) => {
            let reader = FileReader::open(path).map_err(|error| PlayerError::OpenFile {
                target: source.clone(),
                source: error,
            })?;
            Ok(OpenedReader {
                reader: Box::new(reader),
                failure: None,
                buffering: None,
                discontinuity: None,
            })
        }
        Source::Url(_) if source.is_hls() => {
            let hls = match shared {
                Some(SharedSource::Hls(hls)) => Arc::clone(hls),
                // Одиночный читатель (тесты, CLI): свой HLS-источник.
                _ => HlsSource::open(source, HttpConfig::default(), Arc::clone(&cancel))?,
            };
            let reader = hls.reader(track).ok_or_else(|| PlayerError::Hls {
                target: source.clone(),
                reason: "у выбранной дорожки нет плейлиста".to_owned(),
            })?;
            let failure = reader.failure_slot();
            let buffering = reader.buffering_flag();
            let discontinuity = reader.discontinuity_flag();
            Ok(OpenedReader {
                reader: Box::new(reader),
                failure: Some(failure),
                buffering: Some(buffering),
                discontinuity: Some(discontinuity),
            })
        }
        Source::Url(_) => {
            let reader = match shared {
                Some(SharedSource::Http(shared)) => shared.reader(Arc::clone(&cancel)),
                // Одиночный читатель (тесты, CLI): свой кэш на источник.
                _ => HttpReader::open(source, HttpConfig::default(), Arc::clone(&cancel))?,
            };
            let failure = reader.failure_slot();
            let buffering = reader.buffering_flag();
            Ok(OpenedReader {
                reader: Box::new(reader),
                failure: Some(failure),
                buffering: Some(buffering),
                discontinuity: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;
    use crate::video::test_support::temp_path;

    fn write_sample(bytes: &[u8]) -> PathBuf {
        let path = temp_path("bin");
        std::fs::write(&path, bytes).expect("создать тестовый файл");
        path
    }

    #[test]
    fn reads_the_whole_file_and_reports_len() {
        let path = write_sample(b"hello world");
        let mut reader = FileReader::open(&path).expect("открыть файл");

        assert_eq!(reader.len(), Some(11));
        assert!(reader.is_seekable());

        let mut text = String::new();
        reader.read_to_string(&mut text).expect("прочитать файл");
        assert_eq!(text, "hello world");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn seeks_from_start_current_and_end() {
        let path = write_sample(b"0123456789");
        let mut reader = FileReader::open(&path).expect("открыть файл");
        let mut byte = [0_u8; 1];

        reader.seek(SeekFrom::Start(2)).expect("начало");
        reader.read_exact(&mut byte).expect("прочитать 2");
        assert_eq!(byte, [b'2']);

        reader.seek(SeekFrom::Current(2)).expect("текущая позиция");
        reader.read_exact(&mut byte).expect("прочитать 5");
        assert_eq!(byte, [b'5']);

        let end = reader.seek(SeekFrom::End(-1)).expect("конец");
        assert_eq!(end, 9);
        reader.read_exact(&mut byte).expect("прочитать 9");
        assert_eq!(byte, [b'9']);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hint_is_the_file_name() {
        let path = temp_path("mp4");
        {
            let mut file = File::create(&path).expect("создать файл");
            file.write_all(b"data").expect("записать");
        }

        let reader = FileReader::open(&path).expect("открыть файл");
        let expected = path.file_name().and_then(|name| name.to_str());
        assert_eq!(reader.hint_name(), expected);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_file_is_reported() {
        let path = temp_path("missing");
        let Err(error) = FileReader::open(&path) else {
            panic!("файла нет, открытие должно провалиться");
        };

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn file_reader_has_no_http_failure() {
        let path = write_sample(b"data");
        let opened = open_reader(
            &Source::File(path.clone()),
            Track::Video,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .expect("открыть файл");

        assert!(opened.failure.is_none());
        assert!(opened.buffering.is_none());
        assert!(opened.discontinuity.is_none());
        let _ = std::fs::remove_file(path);
    }
}
