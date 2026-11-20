//! Источник воспроизведения: локальный файл или ссылка.
//!
//! Ядро не различает «файл» и «URL» на уровне декодеров: и то и другое
//! превращается в поток байтов через [`crate::reader::MediaReader`]. `Source`
//! отвечает только за разбор строки и за то, как источник показывать в UI,
//! логах и ошибках.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Откуда берём байты.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Локальный файл; сюда же попадает `file://`.
    File(PathBuf),
    /// Сетевой источник (`http://` или `https://`).
    Url(String),
}

impl Source {
    /// Разбирает строку из командной строки или поля ввода.
    ///
    /// Схема распознаётся только явно: строка без `http(s)://`, `file://`
    /// считается путём, как и в mpv. Схема сравнивается без учёта регистра.
    pub fn parse(text: &str) -> Result<Self, SourceError> {
        if text.is_empty() {
            return Err(SourceError::Empty);
        }

        if strip_prefix_ci(text, "http://").is_some() || strip_prefix_ci(text, "https://").is_some()
        {
            validate_url(text)?;
            return Ok(Self::Url(text.to_owned()));
        }

        if let Some(rest) = strip_prefix_ci(text, "file://") {
            return file_url_path(rest).map(Self::File);
        }

        Ok(Self::File(PathBuf::from(text)))
    }

    /// Разбирает `OsString` для CLI и drag-and-drop.
    ///
    /// URL распознаётся по ASCII-префиксу схемы; всё остальное становится
    /// путём без потери символов вне Unicode.
    pub fn from_os_string(value: OsString) -> Result<Self, SourceError> {
        if value.is_empty() {
            return Err(SourceError::Empty);
        }

        match value.to_str() {
            Some(text)
                if strip_prefix_ci(text, "http://").is_some()
                    || strip_prefix_ci(text, "https://").is_some()
                    || strip_prefix_ci(text, "file://").is_some() =>
            {
                Self::parse(text)
            }
            _ => Ok(Self::File(PathBuf::from(value))),
        }
    }

    /// Как источник показывать пользователю: полный URL, включая query.
    pub fn display(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }

    /// Как источник называть в логах и ошибках.
    ///
    /// Query и fragment заменяются на `?…`: в ссылках часто лежат токены,
    /// которым не место в консоли и тексте ошибки.
    pub fn log_name(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Url(url) => match url.find(['?', '#']) {
                Some(index) => format!("{}?…", &url[..index]),
                None => url.clone(),
            },
        }
    }

    /// Последний сегмент — подсказка FFmpeg для определения формата.
    ///
    /// Для не-Unicode имени файла подсказки нет: расширение всё равно
    /// не прочитать как `str`.
    pub fn file_name(&self) -> Option<&str> {
        match self {
            Self::File(path) => path.file_name().and_then(OsStr::to_str),
            Self::Url(url) => {
                let (_, after_scheme) = url.split_once("://")?;
                let rest = after_scheme.split_once('/')?.1;
                let path = rest.split(['?', '#']).next()?;
                let name = path.rsplit('/').next()?;
                (!name.is_empty()).then_some(name)
            }
        }
    }

    /// Нужно ли для источника сетевое соединение.
    pub fn is_network(&self) -> bool {
        matches!(self, Self::Url(_))
    }

    /// Ссылается ли источник на HLS-плейлист (`.m3u8`, без учёта регистра).
    pub fn is_hls(&self) -> bool {
        match self {
            Self::File(path) => path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("m3u8")),
            Self::Url(_) => self.file_name().is_some_and(|name| {
                name.rsplit_once('.')
                    .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("m3u8"))
            }),
        }
    }

    /// Путь, если источник локальный.
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Url(_) => None,
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.log_name())
    }
}

impl From<PathBuf> for Source {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&Path> for Source {
    fn from(path: &Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

impl From<&PathBuf> for Source {
    fn from(path: &PathBuf) -> Self {
        Self::File(path.clone())
    }
}

/// Почему строку не удалось превратить в источник.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceError {
    /// Пустая строка — это не URL и не путь.
    #[error("пустая строка не является ни URL, ни путём")]
    Empty,
    /// Схема распознана, но в ссылке есть недопустимые данные.
    #[error("не похоже на URL или путь: {0}")]
    InvalidUrl(&'static str),
    /// `file://` не удалось превратить в путь.
    #[error("не удалось разобрать file://: {0}")]
    InvalidFileUrl(&'static str),
}

/// Снимает ASCII-префикс без учёта регистра.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Проверяет то, что FFmpeg и HTTP-клиент всё равно не примут.
fn validate_url(url: &str) -> Result<(), SourceError> {
    if url
        .chars()
        .any(|character| character.is_ascii_whitespace() || character.is_ascii_control())
    {
        return Err(SourceError::InvalidUrl("пробелы и управляющие символы"));
    }

    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();

    if authority.is_empty() {
        return Err(SourceError::InvalidUrl("пустой хост"));
    }
    if authority.contains('@') {
        return Err(SourceError::InvalidUrl("логин и пароль в ссылке"));
    }

    Ok(())
}

/// Превращает часть `file://` после схемы в путь.
fn file_url_path(rest: &str) -> Result<PathBuf, SourceError> {
    if rest.is_empty() {
        return Err(SourceError::InvalidFileUrl("пустой путь"));
    }

    let decoded = percent_decode(rest)?;
    let text =
        String::from_utf8(decoded).map_err(|_| SourceError::InvalidFileUrl("путь не в UTF-8"))?;

    Ok(local_path(&text))
}

/// `file:///C:/…` → `C:/…`, `file:///home/…` → `/home/…`,
/// `file://server/share` → UNC.
fn local_path(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix('/') {
        // `/C:/video.mp4` — диск, а не абсолютный POSIX-путь.
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return PathBuf::from(rest);
        }
        return PathBuf::from(text);
    }

    if text.starts_with("//") {
        return PathBuf::from(text);
    }

    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\{}", text.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(format!("//{text}"))
    }
}

/// Percent-декодирование в байты: результат может быть не UTF-8.
fn percent_decode(text: &str) -> Result<Vec<u8>, SourceError> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes.get(index + 1).copied().and_then(hex_value);
            let low = bytes.get(index + 2).copied().and_then(hex_value);
            let (Some(high), Some(low)) = (high, low) else {
                return Err(SourceError::InvalidFileUrl("некорректный %-код"));
            };
            decoded.push(high << 4 | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    Ok(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Source {
        Source::Url(text.to_owned())
    }

    #[test]
    fn http_and_https_are_urls() {
        assert_eq!(
            Source::parse("https://example.com/video.mp4?token=1").expect("URL"),
            url("https://example.com/video.mp4?token=1")
        );
        assert_eq!(
            Source::parse("HTTP://example.com/a.mp4").expect("URL в верхнем регистре"),
            url("HTTP://example.com/a.mp4")
        );
        assert!(
            Source::parse("https://example.com/a.mp4")
                .expect("URL")
                .is_network()
        );
    }

    #[test]
    fn file_url_with_windows_drive_decodes_percent() {
        assert_eq!(
            Source::parse("file:///C:/my%20video/movie.mp4").expect("file://"),
            Source::File(PathBuf::from("C:/my video/movie.mp4"))
        );
    }

    #[test]
    fn file_url_with_posix_path_stays_absolute() {
        assert_eq!(
            Source::parse("file:///home/user/a%20b.mp4").expect("file://"),
            Source::File(PathBuf::from("/home/user/a b.mp4"))
        );
    }

    #[test]
    fn file_url_with_host_becomes_unc() {
        let source = Source::parse("file://server/share/a.mp4").expect("file://");
        let expected = if cfg!(windows) {
            PathBuf::from(r"\\server\share\a.mp4")
        } else {
            PathBuf::from("//server/share/a.mp4")
        };
        assert_eq!(source, Source::File(expected));
    }

    #[test]
    fn windows_path_is_not_mistaken_for_a_url() {
        assert_eq!(
            Source::parse(r"C:\video\movie.mp4").expect("путь"),
            Source::File(PathBuf::from(r"C:\video\movie.mp4"))
        );
        assert_eq!(
            Source::parse("movie.mp4").expect("относительный путь"),
            Source::File(PathBuf::from("movie.mp4"))
        );
    }

    #[test]
    fn from_os_string_keeps_non_unicode_paths() {
        #[cfg(unix)]
        let raw = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xFF])
        };
        #[cfg(windows)]
        let raw = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[0xD800, u16::from(b'a')])
        };

        let source = Source::from_os_string(raw.clone()).expect("путь");
        assert_eq!(source, Source::File(PathBuf::from(raw)));
    }

    #[test]
    fn from_os_string_recognizes_a_url() {
        let source =
            Source::from_os_string(OsString::from("https://example.com/a.mp4")).expect("URL");
        assert_eq!(source, url("https://example.com/a.mp4"));
    }

    #[test]
    fn invalid_urls_are_rejected() {
        assert_eq!(Source::parse(""), Err(SourceError::Empty));
        assert_eq!(
            Source::parse("https://example.com/a b.mp4"),
            Err(SourceError::InvalidUrl("пробелы и управляющие символы"))
        );
        assert_eq!(
            Source::parse("https://user:pass@example.com/a.mp4"),
            Err(SourceError::InvalidUrl("логин и пароль в ссылке"))
        );
        assert_eq!(
            Source::parse("https:///a.mp4"),
            Err(SourceError::InvalidUrl("пустой хост"))
        );
    }

    #[test]
    fn broken_file_url_is_rejected() {
        assert_eq!(
            Source::parse("file://"),
            Err(SourceError::InvalidFileUrl("пустой путь"))
        );
        assert_eq!(
            Source::parse("file:///tmp/%ZZ.mp4"),
            Err(SourceError::InvalidFileUrl("некорректный %-код"))
        );
    }

    #[test]
    fn log_name_hides_query_and_fragment() {
        let source = url("https://example.com/v.mp4?token=secret#time");
        assert_eq!(
            source.display(),
            "https://example.com/v.mp4?token=secret#time"
        );
        assert_eq!(source.log_name(), "https://example.com/v.mp4?…");
        assert_eq!(source.to_string(), source.log_name());

        let plain = url("https://example.com/v.mp4");
        assert_eq!(plain.log_name(), "https://example.com/v.mp4");
    }

    #[test]
    fn file_name_is_the_last_path_segment() {
        assert_eq!(
            url("https://example.com/dir/video.mp4?token=1").file_name(),
            Some("video.mp4")
        );
        assert_eq!(url("https://example.com").file_name(), None);
        assert_eq!(url("https://example.com/").file_name(), None);
        assert_eq!(url("https://example.com/?a.mp4").file_name(), None);
        assert_eq!(
            Source::File(PathBuf::from("/tmp/video.mp4")).file_name(),
            Some("video.mp4")
        );
    }

    #[test]
    fn hls_is_detected_for_files_and_urls() {
        assert!(Source::File(PathBuf::from("index.m3u8")).is_hls());
        assert!(Source::File(PathBuf::from("INDEX.M3U8")).is_hls());
        assert!(url("https://example.com/live/index.M3U8?token=1").is_hls());
        assert!(!url("https://example.com/video.mp4?next=index.m3u8").is_hls());
        assert!(!Source::File(PathBuf::from("movie.mp4")).is_hls());
    }

    #[test]
    fn path_conversions_build_file_sources() {
        let path = PathBuf::from("movie.mp4");
        assert_eq!(Source::from(path.clone()), Source::File(path.clone()));
        assert_eq!(Source::from(path.as_path()), Source::File(path.clone()));
        assert_eq!(Source::from(&path), Source::File(path.clone()));
        assert!(
            Source::File(path)
                .as_file()
                .is_some_and(|path| path == Path::new("movie.mp4"))
        );
    }
}
