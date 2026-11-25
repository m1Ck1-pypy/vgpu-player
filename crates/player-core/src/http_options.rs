//! HTTP-заголовки сетевого источника.
//!
//! Ссылки вроде RuTube `play_stream` отдают не только адрес, но и заголовки
//! `Referer`/`User-Agent`: без них сервер не отдаёт медиа. Здесь живёт
//! публичный набор таких заголовков; к каждому запросу источника его
//! применяет [`crate::http::HttpConfig`].
//!
//! Значения проверяются сразу при добавлении: `HeaderValue` отвергает CR/LF
//! и управляющие символы, а не-ASCII мы проверяем сами. Заголовок нельзя
//! «подменить» переводом строки, и он остаётся ASCII ради предсказуемости.

use std::fmt;

use ureq::http::{HeaderName, HeaderValue};

/// Заголовки, которые увидят все запросы одного источника.
///
/// Имена сравниваются без учёта регистра: повторное добавление заменяет
/// прежнее значение. Отсутствие заголовка означает «взять значение по
/// умолчанию» из `HttpConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpOptions {
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl HttpOptions {
    /// Пустой набор: работают заголовки по умолчанию.
    pub fn new() -> Self {
        Self::default()
    }

    /// `Referer` для всех запросов источника.
    pub fn with_referer(self, value: impl AsRef<str>) -> Result<Self, HttpOptionsError> {
        self.with_header("Referer", value)
    }

    /// `User-Agent`; отменяет `vgpu-player/<version>`.
    pub fn with_user_agent(self, value: impl AsRef<str>) -> Result<Self, HttpOptionsError> {
        self.with_header("User-Agent", value)
    }

    /// Произвольный заголовок.
    ///
    /// Нужен будущим резолверам (`Origin`, `Client-ID`, `Content-Type`), чтобы
    /// не заводить второй набор правил и валидации.
    pub fn with_header(
        mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<Self, HttpOptionsError> {
        let name = name.as_ref();
        let value = value.as_ref();
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            HttpOptionsError::InvalidName {
                name: name.to_owned(),
                reason: error.to_string(),
            }
        })?;
        if value.is_empty() {
            return Err(HttpOptionsError::InvalidValue {
                name: name.to_owned(),
                reason: "пустое значение".to_owned(),
            });
        }
        // `HeaderValue` допускает obs-text (0x80–0xFF), но нам нужен
        // предсказуемый ASCII-заголовок: не-ASCII URL должен быть
        // percent-encoded.
        if !value.is_ascii() {
            return Err(HttpOptionsError::InvalidValue {
                name: name.to_owned(),
                reason: "не-ASCII символы недопустимы".to_owned(),
            });
        }
        let header_value =
            HeaderValue::from_str(value).map_err(|error| HttpOptionsError::InvalidValue {
                name: name.to_owned(),
                reason: error.to_string(),
            })?;

        if let Some(existing) = self
            .headers
            .iter_mut()
            .find(|(existing, _)| *existing == header_name)
        {
            *existing = (header_name, header_value);
        } else {
            self.headers.push((header_name, header_value));
        }
        Ok(self)
    }

    /// Значение заданного заголовка без учёта регистра.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.as_str().eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.to_str().ok())
    }

    /// Разобранные значения без дефолтов — для `HttpConfig`.
    pub(crate) fn headers(&self) -> &[(HeaderName, HeaderValue)] {
        &self.headers
    }
}

/// Почему заголовок не удалось сохранить.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpOptionsError {
    /// Имя не является допустимым именем HTTP-заголовка.
    InvalidName { name: String, reason: String },
    /// Значение пустое, содержит CR/LF или не-ASCII.
    InvalidValue { name: String, reason: String },
}

impl fmt::Display for HttpOptionsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name, reason } => {
                write!(formatter, "некорректное имя заголовка «{name}»: {reason}")
            }
            Self::InvalidValue { name, reason } => {
                write!(
                    formatter,
                    "некорректное значение заголовка «{name}»: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for HttpOptionsError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored<'a>(options: &'a HttpOptions, name: &str) -> Option<&'a str> {
        options.headers().iter().find_map(|(header, value)| {
            (header.as_str() == name).then(|| value.to_str().expect("значение заголовка"))
        })
    }

    #[test]
    fn empty_by_default() {
        let options = HttpOptions::new();

        assert_eq!(options, HttpOptions::default());
        assert!(options.headers().is_empty());
        assert_eq!(options.header("Referer"), None);
    }

    #[test]
    fn referer_and_user_agent_are_stored() {
        let options = HttpOptions::new()
            .with_referer("https://rutube.ru/")
            .expect("Referer")
            .with_user_agent("Mozilla/5.0")
            .expect("User-Agent");

        assert_eq!(options.header("Referer"), Some("https://rutube.ru/"));
        assert_eq!(options.header("user-agent"), Some("Mozilla/5.0"));
    }

    #[test]
    fn repeated_name_replaces_the_value() {
        let options = HttpOptions::new()
            .with_user_agent("first")
            .expect("первый")
            .with_user_agent("second")
            .expect("второй");

        assert_eq!(options.headers().len(), 1);
        assert_eq!(options.header("User-Agent"), Some("second"));
    }

    #[test]
    fn header_name_case_does_not_create_a_duplicate() {
        let options = HttpOptions::new()
            .with_header("user-agent", "first")
            .expect("первый")
            .with_header("User-Agent", "second")
            .expect("второй");

        assert_eq!(options.headers().len(), 1);
        assert_eq!(stored(&options, "user-agent"), Some("second"));
    }

    #[test]
    fn invalid_name_is_rejected() {
        let error = HttpOptions::new()
            .with_header("bad name", "value")
            .expect_err("пробел в имени недопустим");

        assert!(matches!(error, HttpOptionsError::InvalidName { .. }));
        assert!(error.to_string().contains("bad name"), "{error}");
    }

    #[test]
    fn crlf_in_value_is_rejected() {
        let error = HttpOptions::new()
            .with_user_agent("Mozilla/5.0\r\nX-Evil: 1")
            .expect_err("перевод строки недопустим");

        assert!(matches!(error, HttpOptionsError::InvalidValue { .. }));
    }

    #[test]
    fn non_ascii_value_is_rejected() {
        let error = HttpOptions::new()
            .with_referer("https://example.com/кириллица")
            .expect_err("не-ASCII недопустим");

        assert!(matches!(error, HttpOptionsError::InvalidValue { .. }));
    }

    #[test]
    fn empty_value_is_rejected() {
        let error = HttpOptions::new()
            .with_user_agent("")
            .expect_err("пустое значение недопустимо");

        assert!(matches!(error, HttpOptionsError::InvalidValue { .. }));
    }
}
