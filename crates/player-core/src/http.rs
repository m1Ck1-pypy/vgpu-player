//! HTTP-слой для сетевых источников.
//!
//! Байты тянет загрузчик общего кэша (`cache.rs`), а звук и видео читают из
//! него своими курсорами: файл качается один раз на источник. Здесь остаются
//! только запросы, `Range`, повторы, отмена и TLS.
//!
//! Источник с `Range` берёт каждый блок отдельным запросом. Сервер без `Range`
//! отдаёт поток с начала: загрузчик держит соединение, продолжает его, а после
//! перемотки перезапускает и пропускает уже прочитанное (до `resume_limit`).

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::cache::{Cache, CacheConfig, CacheCounters, CacheCursor, FetchError};
use crate::reader::MediaReader;
use crate::sync::NetworkStats;
use crate::{PlayerError, Source};

/// Таймаут на DNS-резолв и установку соединения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Таймаут одного чтения из тела ответа.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Сколько ждём заголовки ответа.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Сколько редиректов разрешено.
const MAX_REDIRECTS: u32 = 10;

/// Сколько всего попыток на один запрос.
const RETRIES: usize = 5;

/// Паузы перед повторами.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// До какой позиции можно перезапускать поток без `Range` (решение B).
const RESUME_LIMIT: u64 = 64 * 1024 * 1024;

/// Настройки HTTP-источника.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HttpConfig {
    /// Таймаут на DNS и соединение.
    pub(crate) connect_timeout: Duration,
    /// Таймаут одного чтения.
    pub(crate) read_timeout: Duration,
    /// Сколько ждём заголовки ответа.
    pub(crate) idle_timeout: Duration,
    /// Сколько всего попыток на запрос.
    pub(crate) retries: usize,
    /// Паузы перед повторами.
    pub(crate) retry_delays: [Duration; 4],
    /// Сколько редиректов разрешено.
    pub(crate) max_redirects: u32,
    /// Размеры общего кэша.
    pub(crate) cache: CacheConfig,
    /// До какой позиции можно перезапускать поток без `Range`.
    pub(crate) resume_limit: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: CONNECT_TIMEOUT,
            read_timeout: READ_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            retries: RETRIES,
            retry_delays: RETRY_DELAYS,
            max_redirects: MAX_REDIRECTS,
            cache: CacheConfig::default(),
            resume_limit: RESUME_LIMIT,
        }
    }
}

/// Агент с явным native-tls и корнями платформы.
///
/// Без явных настроек `ureq` берёт rustls и WebPki-корни: на Windows это
/// ломает `https` за корпоративным прокси.
fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

/// Собирает агент с таймаутами и следованием редиректам.
fn agent(config: &HttpConfig) -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(tls_config())
        .timeout_resolve(Some(config.connect_timeout))
        .timeout_connect(Some(config.connect_timeout))
        .timeout_recv_response(Some(config.idle_timeout))
        .timeout_per_read(Some(config.read_timeout))
        .max_redirects(config.max_redirects)
        // Статусы разбираем сами: 4xx/5xx должны давать понятный текст, а не
        // `ureq::Error::Status`.
        .http_status_as_error(false)
        .build()
        .into()
}

/// Почему сетевое чтение не удалось.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpFailure {
    /// Сервер ответил не-2xx.
    Status(u16),
    /// Сеть: таймаут, DNS, TLS или обрыв.
    Network(String),
    /// Сервер не поддерживает `Range`, а позиция дальше начала.
    NotSeekable,
    /// Пустой ответ.
    Empty,
    /// Ответ не является медиафайлом.
    NotMedia,
    /// Открытие или чтение отменено.
    Cancelled,
}

impl HttpFailure {
    /// В ошибку ядра — для `open` и слоя декодера.
    pub(crate) fn into_player_error(self, source: &Source) -> PlayerError {
        match self {
            Self::Status(status) => PlayerError::HttpStatus {
                target: source.clone(),
                status,
            },
            Self::Network(message) => PlayerError::Network {
                target: source.clone(),
                message,
            },
            Self::NotSeekable => PlayerError::NotSeekable {
                target: source.clone(),
            },
            Self::Empty => PlayerError::EmptySource {
                target: source.clone(),
            },
            Self::NotMedia => PlayerError::NotMedia {
                target: source.clone(),
            },
            Self::Cancelled => PlayerError::Cancelled,
        }
    }

    /// В `io::Error` — для `Read` и `Seek`.
    fn into_io(self) -> io::Error {
        match self {
            Self::Cancelled => io::Error::new(io::ErrorKind::Interrupted, "открытие отменено"),
            Self::Status(status) => io::Error::other(format!("сервер вернул {status}")),
            Self::Network(message) => io::Error::other(format!("сеть: {message}")),
            Self::NotSeekable => io::Error::new(
                io::ErrorKind::Unsupported,
                "источник не поддерживает перемотку",
            ),
            Self::Empty => io::Error::new(io::ErrorKind::UnexpectedEof, "по ссылке нет данных"),
            Self::NotMedia => io::Error::other("по ссылке не медиафайл"),
        }
    }
}

/// Неудачный запрос и признак, стоит ли его повторять.
struct HttpError {
    failure: HttpFailure,
    retryable: bool,
}

impl HttpError {
    fn fatal(failure: HttpFailure) -> Self {
        Self {
            failure,
            retryable: false,
        }
    }

    fn retryable(failure: HttpFailure) -> Self {
        Self {
            failure,
            retryable: true,
        }
    }

    fn cancelled() -> Self {
        Self::fatal(HttpFailure::Cancelled)
    }
}

/// Открытый сетевой источник: один кэш и общие счётчики на две дорожки.
///
/// Живёт в `Media`; читатели получают курсоры через [`HttpSource::reader`].
pub(crate) struct HttpSource {
    hint: Option<String>,
    len: Option<u64>,
    seekable: bool,
    buffering: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<HttpFailure>>>,
    counters: Arc<CacheCounters>,
    cache: Arc<Cache>,
}

impl HttpSource {
    /// Открывает URL и выясняет размер и поддержку `Range`.
    ///
    /// Проверочный запрос просит один байт: по ответу `206` видно поддержку
    /// диапазонов и полный размер из `Content-Range`; `200` означает, что
    /// сервер игнорирует `Range`.
    pub(crate) fn open(
        source: &Source,
        config: HttpConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, PlayerError> {
        let Source::Url(url) = source else {
            return Err(PlayerError::UnsupportedSource {
                target: source.clone(),
            });
        };

        let agent = agent(&config);
        let counters = Arc::new(CacheCounters::default());
        let buffering = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));

        let mut probe = || {
            if cancel.load(Ordering::SeqCst) {
                return Err(HttpError::cancelled());
            }
            counters.add_request();
            let response = agent
                .get(url)
                .header("Range", "bytes=0-0")
                .call()
                .map_err(request_error)?;
            let status = response.status().as_u16();
            if status != 200 && status != 206 {
                return Err(status_error(status));
            }
            Ok(response)
        };

        let response = with_retries(
            &config,
            &|| cancel.load(Ordering::SeqCst),
            &counters,
            &mut probe,
        )
        .map_err(|error| error.failure.into_player_error(source))?;

        let status = response.status().as_u16();
        let content_type = header(&response, "content-type").unwrap_or_default();
        let content_type = content_type.to_ascii_lowercase();
        if content_type.starts_with("text/html") || content_type.starts_with("application/xhtml") {
            return Err(HttpFailure::NotMedia.into_player_error(source));
        }

        let content_length =
            header(&response, "content-length").and_then(|value| value.parse::<u64>().ok());
        let (len, seekable) = if status == 206 {
            let total = header(&response, "content-range")
                .as_deref()
                .and_then(parse_content_range)
                .flatten();
            (total, true)
        } else {
            (content_length, false)
        };
        // Тело пробного ответа не нужно: загрузчик запросит данные сам.
        drop(response);

        if len == Some(0) {
            return Err(HttpFailure::Empty.into_player_error(source));
        }

        let loader = HttpLoader {
            agent,
            config,
            url: url.clone(),
            cancel: Arc::clone(&cancel),
            counters: Arc::clone(&counters),
            failure: Arc::clone(&failure),
            buffering: Arc::clone(&buffering),
            mode: if seekable {
                Mode::Range
            } else {
                Mode::Stream(Mutex::new(None))
            },
        };
        let cache = Cache::start(
            config.cache,
            len,
            Arc::clone(&counters),
            move |offset, want, abort| loader.fetch(offset, want, abort),
        )
        .map_err(|error| PlayerError::Network {
            target: source.clone(),
            message: format!("не удалось запустить загрузчик кэша: {error}"),
        })?;

        Ok(Arc::new(Self {
            hint: source.file_name().map(str::to_owned),
            len,
            seekable,
            buffering,
            failure,
            counters,
            cache,
        }))
    }

    /// Новый курсор чтения поверх общего кэша.
    pub(crate) fn reader(self: &Arc<Self>, cancel: Arc<AtomicBool>) -> HttpReader {
        HttpReader {
            cursor: self.cache.cursor(Arc::clone(&cancel)),
            hint: self.hint.clone(),
            len: self.len,
            seekable: self.seekable,
            cancel,
            shared_failure: Arc::clone(&self.failure),
            slot: Arc::new(Mutex::new(None)),
            buffering: Arc::clone(&self.buffering),
            counters: Arc::clone(&self.counters),
        }
    }

    /// Счётчики сети источника: без двойного учёта двух дорожек.
    pub(crate) fn network_stats(&self) -> NetworkStats {
        self.cache.network_stats()
    }
}

/// Как загрузчик берёт байты: диапазонами или одним потоком с начала.
enum Mode {
    /// Сервер понимает `Range`: каждый блок — отдельный запрос.
    Range,
    /// Сервер отдаёт только поток с начала.
    Stream(Mutex<Option<StreamState>>),
}

/// Открытый последовательный поток без `Range`.
struct StreamState {
    body: ureq::BodyReader<'static>,
    /// Абсолютная позиция следующего байта из потока.
    pos: u64,
}

/// Загрузчик кэша: один на источник, работает в фоновом потоке кэша.
struct HttpLoader {
    agent: ureq::Agent,
    config: HttpConfig,
    url: String,
    cancel: Arc<AtomicBool>,
    counters: Arc<CacheCounters>,
    failure: Arc<Mutex<Option<HttpFailure>>>,
    buffering: Arc<AtomicBool>,
    mode: Mode,
}

impl HttpLoader {
    fn fetch(&self, offset: u64, want: usize, abort: &AtomicBool) -> Result<Vec<u8>, FetchError> {
        let stop = || self.cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed);
        self.buffering.store(true, Ordering::SeqCst);
        let result = match &self.mode {
            Mode::Range => fetch_range(
                &self.agent,
                &self.config,
                &self.url,
                offset,
                want as u64,
                &self.counters,
                &stop,
            ),
            Mode::Stream(state) => self.fetch_stream(offset, want, &stop, state),
        };
        self.buffering.store(false, Ordering::SeqCst);
        result.map_err(|error| self.record(error))
    }

    /// Сохраняет причину для слоя декодера и переводит её в ошибку кэша.
    fn record(&self, error: HttpError) -> FetchError {
        if error.failure == HttpFailure::Cancelled {
            return FetchError::cancelled();
        }
        if let Ok(mut slot) = self.failure.lock()
            && slot.is_none()
        {
            *slot = Some(error.failure.clone());
        }
        FetchError::other(error.failure.clone().into_io().to_string())
    }

    /// Читает блок из последовательного потока без `Range`.
    ///
    /// Пока поток стоит на нужной позиции, соединение продолжается; иначе —
    /// перезапуск с начала и пропуск до `offset` (не дальше `resume_limit`).
    fn fetch_stream(
        &self,
        offset: u64,
        want: usize,
        stop: &dyn Fn() -> bool,
        state: &Mutex<Option<StreamState>>,
    ) -> Result<Vec<u8>, HttpError> {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stream = guard.take();

        if let Some(active) = stream.as_mut()
            && active.pos == offset
        {
            match read_window(&mut active.body, want, stop) {
                Ok(buffer) => {
                    active.pos += buffer.len() as u64;
                    *guard = stream;
                    return Ok(buffer);
                }
                // Отмена (перемотка или закрытие) не должна рвать открытое
                // соединение: тело не прочитало ни байта, поток можно
                // продолжить с той же позиции.
                Err(error) if error.failure == HttpFailure::Cancelled => {
                    *guard = stream;
                    return Err(error);
                }
                // Поток сломался: дальше только перезапуск с начала.
                Err(_) => {}
            }
        }

        if offset > self.config.resume_limit {
            return Err(HttpError::fatal(HttpFailure::NotSeekable));
        }
        if offset > 0 {
            self.counters.add_restart();
        }

        let mut attempt = || {
            if stop() {
                return Err(HttpError::cancelled());
            }
            let response = self.agent.get(&self.url).call().map_err(request_error)?;
            let status = response.status().as_u16();
            if status != 200 {
                return Err(status_error(status));
            }
            let mut body = response
                .into_body()
                .into_with_config()
                .limit(u64::MAX)
                .reader();
            skip_stream(&mut body, offset, stop)?;
            let buffer = read_window(&mut body, want, stop)?;
            Ok((body, buffer))
        };

        let (body, buffer) = with_retries(&self.config, stop, &self.counters, &mut attempt)?;
        let pos = offset + buffer.len() as u64;
        *guard = Some(StreamState { body, pos });
        Ok(buffer)
    }
}

/// Сетевой читатель одной дорожки: курсор в общем кэше.
pub(crate) struct HttpReader {
    cursor: CacheCursor,
    hint: Option<String>,
    len: Option<u64>,
    seekable: bool,
    cancel: Arc<AtomicBool>,
    /// Терминальная ошибка источника (общая, не сбрасывается).
    shared_failure: Arc<Mutex<Option<HttpFailure>>>,
    /// Слот своей дорожки: декодер забирает его после `invalid data`.
    slot: Arc<Mutex<Option<HttpFailure>>>,
    /// «Ждём сеть»: ядро превращает это в `Buffering`.
    buffering: Arc<AtomicBool>,
    counters: Arc<CacheCounters>,
}

impl HttpReader {
    /// Открывает URL с собственным кэшем (тесты и одиночный читатель).
    pub(crate) fn open(
        source: &Source,
        config: HttpConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, PlayerError> {
        let shared = HttpSource::open(source, config, Arc::clone(&cancel))?;
        Ok(shared.reader(cancel))
    }

    /// Копирует причину сбоя источника в слот этой дорожки.
    fn record_failure(&self) {
        if self.cancel.load(Ordering::SeqCst) {
            self.set_failure(HttpFailure::Cancelled);
            return;
        }
        let failure = self
            .shared_failure
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        if let Some(failure) = failure {
            self.set_failure(failure);
        }
    }

    fn set_failure(&self, failure: HttpFailure) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(failure);
        }
    }

    /// Последняя сетевая ошибка — для подмены текста FFmpeg.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn failure(&self) -> Option<HttpFailure> {
        self.slot.lock().ok().and_then(|slot| slot.clone())
    }

    /// Слот ошибки: декодер читает его, когда FFmpeg вернул `invalid data`.
    pub(crate) fn failure_slot(&self) -> Arc<Mutex<Option<HttpFailure>>> {
        Arc::clone(&self.slot)
    }

    /// Флаг «ждём сеть» для события `Buffering`.
    pub(crate) fn buffering_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.buffering)
    }

    /// Счётчики сети: общие с другой дорожкой.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn counters(&self) -> Arc<CacheCounters> {
        Arc::clone(&self.counters)
    }
}

impl Read for HttpReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.cursor.read(buffer) {
            Ok(read) => Ok(read),
            Err(error) => {
                self.record_failure();
                match self.failure() {
                    Some(failure) => Err(failure.into_io()),
                    None => Err(error),
                }
            }
        }
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self.cursor.seek(position) {
            Ok(target) => Ok(target),
            Err(error) => {
                if error.kind() != io::ErrorKind::Unsupported {
                    self.record_failure();
                }
                Err(error)
            }
        }
    }
}

impl MediaReader for HttpReader {
    fn len(&self) -> Option<u64> {
        self.len
    }

    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn hint_name(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

/// Скачивает блок `[start, start + len)` запросом `Range` с повторами.
fn fetch_range(
    agent: &ureq::Agent,
    config: &HttpConfig,
    url: &str,
    start: u64,
    len: u64,
    counters: &CacheCounters,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, HttpError> {
    let len = len.max(1);
    let end = start.saturating_add(len - 1);
    let range = format!("bytes={start}-{end}");
    let expected = len as usize;

    let mut attempt = || {
        if stop() {
            return Err(HttpError::cancelled());
        }
        let response = agent
            .get(url)
            .header("Range", &range)
            .call()
            .map_err(request_error)?;

        let status = response.status().as_u16();
        if status == 200 {
            return Err(HttpError::fatal(HttpFailure::NotSeekable));
        }
        if status != 206 {
            return Err(status_error(status));
        }

        let mut body = response
            .into_body()
            .into_with_config()
            .limit(len + 1)
            .reader();
        read_window(&mut body, expected, stop)
    };

    with_retries(config, stop, counters, &mut attempt)
}

/// Значение заголовка как строка.
fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Полный размер из `Content-Range: bytes 0-0/12345`.
fn parse_content_range(value: &str) -> Option<Option<u64>> {
    let (_, total) = value.rsplit_once('/')?;
    if total.trim() == "*" {
        return Some(None);
    }
    total.trim().parse::<u64>().ok().map(Some)
}

/// Повторяет запрос по решению B и уважает отмену.
///
/// `stop` шире, чем флаг отмены: кэш взводит `abort`, когда блок устарел
/// после перемотки.
fn with_retries<T>(
    config: &HttpConfig,
    stop: &dyn Fn() -> bool,
    counters: &CacheCounters,
    attempt: &mut impl FnMut() -> Result<T, HttpError>,
) -> Result<T, HttpError> {
    let attempts = config.retries.max(1);
    let mut last = None;

    for index in 0..attempts {
        if stop() {
            return Err(HttpError::cancelled());
        }
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !error.retryable || index + 1 == attempts {
                    return Err(error);
                }
                last = Some(error);
                counters.add_retry();

                let delay = config.retry_delays[index.min(config.retry_delays.len() - 1)];
                let deadline = Instant::now() + delay;
                while Instant::now() < deadline {
                    if stop() {
                        return Err(HttpError::cancelled());
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(remaining.min(Duration::from_millis(50)));
                }
            }
        }
    }

    Err(last.unwrap_or_else(HttpError::cancelled))
}

/// Ошибка транспорта: почти всегда имеет смысл повторить.
fn request_error(error: ureq::Error) -> HttpError {
    HttpError::retryable(HttpFailure::Network(error.to_string()))
}

/// Ошибка чтения тела — тоже сетевой обрыв.
fn body_error(error: io::Error) -> HttpError {
    HttpError::retryable(HttpFailure::Network(error.to_string()))
}

/// Статус: 5xx повторяем, 4xx — сразу ошибка.
fn status_error(status: u16) -> HttpError {
    let failure = HttpFailure::Status(status);
    if status >= 500 {
        HttpError::retryable(failure)
    } else {
        HttpError::fatal(failure)
    }
}

/// Пропускает `skip` байт потока без `Range`.
fn skip_stream(
    body: &mut ureq::BodyReader<'static>,
    mut skip: u64,
    stop: &dyn Fn() -> bool,
) -> Result<(), HttpError> {
    let mut scratch = [0_u8; 32 * 1024];
    while skip > 0 {
        if stop() {
            return Err(HttpError::cancelled());
        }
        let want = (skip as usize).min(scratch.len());
        let read = body.read(&mut scratch[..want]).map_err(body_error)?;
        if read == 0 {
            return Err(HttpError::retryable(HttpFailure::Network(
                "поток оборвался до нужной позиции".to_owned(),
            )));
        }
        skip -= read as u64;
    }
    Ok(())
}

/// Читает блок из последовательного потока.
fn read_window(
    body: &mut ureq::BodyReader<'static>,
    cap: usize,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, HttpError> {
    let mut buffer = Vec::with_capacity(cap);
    let mut scratch = [0_u8; 32 * 1024];

    while buffer.len() < cap {
        if stop() {
            // Отмена после частичного чтения: отдаём уже полученное.
            // Кэш сохранит этот неполный блок, и перемотка не потеряет
            // скачанный трафик.
            if !buffer.is_empty() {
                break;
            }
            return Err(HttpError::cancelled());
        }
        let want = (cap - buffer.len()).min(scratch.len());
        let read = body.read(&mut scratch[..want]).map_err(body_error)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&scratch[..read]);
    }
    Ok(buffer)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::Cursor;
    use std::thread::JoinHandle;

    use tiny_http::{Header, Response, Server, StatusCode};

    /// Файл, отданный локальным HTTP-сервером.
    pub(crate) struct ServedFile {
        pub(crate) url: String,
        _thread: JoinHandle<()>,
    }

    /// Поднимает сервер на `127.0.0.1:0` и отдаёт `bytes` по `/{name}`.
    pub(crate) fn serve(
        bytes: Vec<u8>,
        content_type: &'static str,
        name: &'static str,
        honor_range: bool,
    ) -> ServedFile {
        let server = Server::http("127.0.0.1:0").expect("тестовый сервер");
        let address = server.server_addr().to_ip().expect("ip-адрес");
        let url = format!("http://{address}/{name}");

        let thread = std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let body = bytes.clone();
                // Соединение без `Range` держит тело открытым; обработчик
                // должен принимать следующий запрос параллельно.
                std::thread::spawn(move || respond(request, body, content_type, honor_range));
            }
        });

        ServedFile {
            url,
            _thread: thread,
        }
    }

    fn respond(
        request: tiny_http::Request,
        bytes: Vec<u8>,
        content_type: &'static str,
        honor_range: bool,
    ) {
        let header = |name: &str, value: &str| {
            Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("заголовок")
        };
        let reply = |status: u16, headers: Vec<Header>, body: Vec<u8>, len: usize| {
            Response::new(
                StatusCode(status),
                headers,
                Cursor::new(body),
                Some(len),
                None,
            )
            .with_chunked_threshold(usize::MAX)
        };

        let range = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Range"))
            .map(|header| header.value.as_str().to_owned());

        let parsed = range.as_deref().and_then(|value| {
            let value = value.strip_prefix("bytes=")?;
            let (start, end) = value.split_once('-')?;
            let start = start.parse::<usize>().ok()?;
            let end = if end.is_empty() {
                bytes.len().saturating_sub(1)
            } else {
                end.parse::<usize>().ok()?
            };
            Some((start, end.min(bytes.len().saturating_sub(1))))
        });

        let response = if honor_range
            && let Some((start, end)) = parsed
            && start < bytes.len()
        {
            reply(
                206,
                vec![
                    header("Content-Type", content_type),
                    header(
                        "Content-Range",
                        &format!("bytes {start}-{end}/{}", bytes.len()),
                    ),
                ],
                bytes[start..=end].to_vec(),
                end - start + 1,
            )
        } else {
            reply(
                200,
                vec![header("Content-Type", content_type)],
                bytes.clone(),
                bytes.len(),
            )
        };

        let _ = request.respond(response);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use tiny_http::{Header, Response, Server, StatusCode};

    use super::*;

    const TEST_WINDOW: usize = 64 * 1024;

    struct TestServer {
        base: String,
        _thread: JoinHandle<()>,
    }

    fn test_config() -> HttpConfig {
        HttpConfig {
            cache: CacheConfig {
                block: TEST_WINDOW,
                capacity: 8 * TEST_WINDOW,
                read_ahead: 8 * TEST_WINDOW as u64,
            },
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_millis(500),
            idle_timeout: Duration::from_millis(500),
            retry_delays: [Duration::from_millis(1); 4],
            resume_limit: 4096,
            ..HttpConfig::default()
        }
    }

    fn open_test(source: &Source) -> Result<HttpReader, PlayerError> {
        HttpReader::open(source, test_config(), Arc::new(AtomicBool::new(false)))
    }

    fn http_header(name: &str, value: &str) -> Header {
        Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("заголовок")
    }

    fn reply(
        status: u16,
        headers: Vec<Header>,
        body: Vec<u8>,
        length: Option<usize>,
    ) -> Response<Cursor<Vec<u8>>> {
        Response::new(StatusCode(status), headers, Cursor::new(body), length, None)
            // Иначе tiny_http сам переходит на chunked для тел больше 32 КБ.
            .with_chunked_threshold(usize::MAX)
    }

    fn spawn(
        ranges: Arc<Mutex<Vec<Option<String>>>>,
        handler: impl Fn(&tiny_http::Request, &mut Vec<Option<String>>) -> Response<Cursor<Vec<u8>>>
        + Send
        + 'static,
    ) -> TestServer {
        let server = Server::http("127.0.0.1:0").expect("тестовый сервер");
        let address = server.server_addr().to_ip().expect("ip-адрес");
        let base = format!("http://{address}/video.mp4");
        let thread = std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let response = {
                    let mut history = ranges.lock().expect("история запросов");
                    handler(&request, &mut history)
                };
                let _ = request.respond(response);
            }
        });
        TestServer {
            base,
            _thread: thread,
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn record_range(
        request: &tiny_http::Request,
        history: &mut Vec<Option<String>>,
    ) -> Option<String> {
        let range = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Range"))
            .map(|header| header.value.as_str().to_owned());
        history.push(range.clone());
        range
    }

    fn parse_range(value: &str) -> Option<(usize, usize)> {
        let range = value.strip_prefix("bytes=")?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse().ok()?;
        let end = if end.is_empty() {
            usize::MAX
        } else {
            end.parse().ok()?
        };
        Some((start, end))
    }

    fn ranged(
        request: &tiny_http::Request,
        history: &mut Vec<Option<String>>,
        data: &[u8],
        honor: bool,
    ) -> Response<Cursor<Vec<u8>>> {
        let range = record_range(request, history);
        if honor && let Some((start, end)) = range.as_deref().and_then(parse_range) {
            if start >= data.len() {
                return reply(416, Vec::new(), Vec::new(), Some(0));
            }
            let end = end.min(data.len() - 1);
            return reply(
                206,
                vec![
                    http_header("Content-Type", "video/mp4"),
                    http_header(
                        "Content-Range",
                        &format!("bytes {start}-{end}/{}", data.len()),
                    ),
                ],
                data[start..=end].to_vec(),
                Some(end - start + 1),
            );
        }

        reply(
            200,
            vec![http_header("Content-Type", "video/mp4")],
            data.to_vec(),
            Some(data.len()),
        )
    }

    #[test]
    fn ranges_and_seeks_use_the_server_ranges() {
        let data = pattern(200 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        assert_eq!(reader.len(), Some(data.len() as u64));
        assert!(reader.is_seekable());
        assert_eq!(reader.hint_name(), Some("video.mp4"));

        let mut buffer = [0_u8; 16];
        reader.read_exact(&mut buffer).expect("прочитать начало");
        assert_eq!(buffer, data[..16]);

        reader
            .seek(SeekFrom::Start(150_000))
            .expect("перемотка вперёд");
        reader
            .read_exact(&mut buffer)
            .expect("прочитать после seek");
        assert_eq!(buffer, data[150_000..150_016]);

        // Чтение через границу блока: второй запрос Range.
        reader.seek(SeekFrom::Start(0)).expect("в начало");
        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("прочитать блок");
        assert_eq!(first, data[..TEST_WINDOW]);
        let mut next = [0_u8; 16];
        reader.read_exact(&mut next).expect("прочитать после блока");
        assert_eq!(next, data[TEST_WINDOW..TEST_WINDOW + 16]);

        let history = ranges.lock().expect("история");
        assert!(
            history
                .iter()
                .flatten()
                .filter_map(|range| parse_range(range))
                .any(|(start, end)| start <= 150_000 && 150_000 <= end),
            "позиция 150000 должна попасть в запрошенный диапазон: {history:?}"
        );
    }

    #[test]
    fn server_without_range_streams_from_the_start() {
        let data = pattern(150 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, false)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        assert_eq!(reader.len(), Some(data.len() as u64));
        assert!(!reader.is_seekable(), "сервер не поддерживает Range");

        let mut buffer = [0_u8; 10];
        reader.read_exact(&mut buffer).expect("прочитать начало");
        assert_eq!(buffer, data[..10]);

        reader.seek(SeekFrom::Start(1_000)).expect("позиция вперёд");
        reader
            .read_exact(&mut buffer)
            .expect("прочитать после seek");
        assert_eq!(buffer, data[1_000..1_010]);

        // Дальше блока: поток продолжается без повторного соединения.
        reader.seek(SeekFrom::Start(0)).expect("в начало");
        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("прочитать блок");
        assert_eq!(first, data[..TEST_WINDOW]);

        let mut second = [0_u8; 1024];
        reader
            .read_exact(&mut second)
            .expect("продолжить после блока");
        assert_eq!(second, data[TEST_WINDOW..TEST_WINDOW + 1024]);

        let history = ranges.lock().expect("история");
        assert_eq!(
            history.first().and_then(|range| range.clone()),
            Some("bytes=0-0".to_owned())
        );
        assert!(
            history.iter().skip(1).any(|range| range.is_none()),
            "без Range должны быть запросы без заголовка: {history:?}"
        );
    }

    #[test]
    fn chunked_response_has_no_length_and_streams() {
        let data = pattern(100 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            reply(
                200,
                vec![http_header("Content-Type", "video/mp4")],
                body.clone(),
                None,
            )
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        assert_eq!(reader.len(), None, "у chunked нет длины");
        assert!(!reader.is_seekable());

        let mut received = Vec::new();
        reader.read_to_end(&mut received).expect("прочитать всё");
        assert_eq!(received, data);
    }

    #[test]
    fn counters_report_requests_and_bytes() {
        let data = pattern(TEST_WINDOW + 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");
        let before = reader.counters().snapshot();
        assert_eq!(before.requests, 1, "только проверочный запрос");
        assert_eq!(before.bytes, 0);

        let mut received = Vec::new();
        reader.read_to_end(&mut received).expect("прочитать всё");
        assert_eq!(received.len(), data.len());

        let after = reader.counters().snapshot();
        // Проверочный запрос и два блока: данных чуть больше одного блока.
        assert_eq!(after.requests, 3);
        assert_eq!(after.bytes, data.len() as u64);
    }

    #[test]
    fn loader_reads_ahead_of_the_cursor() {
        let data = pattern(4 * TEST_WINDOW);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("первый блок");
        assert_eq!(first, data[..TEST_WINDOW]);

        // Кэш читает вперёд: проверка + блок 0 + блок 1.
        let deadline = Instant::now() + Duration::from_secs(2);
        while ranges.lock().expect("история").len() < 3 {
            assert!(Instant::now() < deadline, "чтение вперёд не началось");
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut second = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut second).expect("второй блок");
        assert_eq!(second, data[TEST_WINDOW..2 * TEST_WINDOW]);
    }

    #[test]
    fn seek_while_a_block_is_in_flight_uses_the_target() {
        let data = pattern(4 * TEST_WINDOW);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("первый блок");

        // Прочитанный вперёд блок относится к прежней позиции и не мешает.
        reader
            .seek(SeekFrom::Start(3 * TEST_WINDOW as u64))
            .expect("перемотка");
        let mut buffer = [0_u8; 32];
        reader
            .read_exact(&mut buffer)
            .expect("прочитать после перемотки");
        assert_eq!(buffer, data[3 * TEST_WINDOW..3 * TEST_WINDOW + 32]);
    }

    #[test]
    fn redirects_are_followed() {
        let data = pattern(32 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            if request.url() == "/start" {
                record_range(request, history);
                return reply(
                    302,
                    vec![http_header("Location", "/video.mp4")],
                    Vec::new(),
                    Some(0),
                );
            }
            ranged(request, history, &body, true)
        });
        let base = server.base.replace("/video.mp4", "/start");

        let source = Source::Url(base);
        let mut reader = open_test(&source).expect("открыть редирект");

        let mut buffer = [0_u8; 8];
        reader
            .read_exact(&mut buffer)
            .expect("прочитать после редиректа");
        assert_eq!(buffer, data[..8]);
    }

    #[test]
    fn http_status_is_reported() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            reply(404, Vec::new(), b"not found".to_vec(), Some(9))
        });

        let source = Source::Url(server.base.clone());
        match open_test(&source) {
            Err(PlayerError::HttpStatus { status, .. }) => assert_eq!(status, 404),
            Err(other) => panic!("ожидался HttpStatus, получено: {other}"),
            Ok(_) => panic!("ожидался HttpStatus"),
        }
    }

    #[test]
    fn html_is_not_media() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            reply(
                200,
                vec![http_header("Content-Type", "text/html; charset=utf-8")],
                b"<html></html>".to_vec(),
                Some(13),
            )
        });

        let source = Source::Url(server.base.clone());
        assert!(matches!(
            open_test(&source),
            Err(PlayerError::NotMedia { .. })
        ));
    }

    #[test]
    fn empty_response_is_reported() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            reply(
                200,
                vec![http_header("Content-Type", "video/mp4")],
                Vec::new(),
                Some(0),
            )
        });

        let source = Source::Url(server.base.clone());
        assert!(matches!(
            open_test(&source),
            Err(PlayerError::EmptySource { .. })
        ));
    }

    #[test]
    fn server_errors_are_retried() {
        let data = pattern(32 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            if attempt < 2 {
                record_range(request, history);
                return reply(500, Vec::new(), b"oops".to_vec(), Some(4));
            }
            ranged(request, history, &body, true)
        });

        let reader = open_test(&Source::Url(server.base.clone())).expect("повторы должны спасти");
        assert_eq!(reader.len(), Some(data.len() as u64));
        assert!(
            attempts.load(Ordering::SeqCst) >= 3,
            "должно быть не меньше трёх попыток"
        );
    }

    #[test]
    fn forbidden_is_not_retried() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            counter.fetch_add(1, Ordering::SeqCst);
            record_range(request, history);
            reply(403, Vec::new(), b"denied".to_vec(), Some(6))
        });

        match open_test(&Source::Url(server.base.clone())) {
            Err(PlayerError::HttpStatus { status, .. }) => assert_eq!(status, 403),
            Err(other) => panic!("ожидался HttpStatus, получено: {other}"),
            Ok(_) => panic!("ожидался HttpStatus"),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "403 не повторяем");
    }

    #[test]
    fn range_not_satisfiable_is_reported() {
        // Сервер отвечает 416 на любую попытку: пользователь должен увидеть
        // статус, а не безликий `invalid data` от FFmpeg.
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            reply(416, Vec::new(), Vec::new(), Some(0))
        });

        match open_test(&Source::Url(server.base.clone())) {
            Err(PlayerError::HttpStatus { status, .. }) => assert_eq!(status, 416),
            Err(other) => panic!("ожидался HttpStatus, получено: {other}"),
            Ok(_) => panic!("ожидался HttpStatus"),
        }
    }

    #[test]
    fn not_found_is_not_retried() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            counter.fetch_add(1, Ordering::SeqCst);
            record_range(request, history);
            reply(404, Vec::new(), b"not found".to_vec(), Some(9))
        });

        let result = open_test(&Source::Url(server.base.clone()));
        assert!(matches!(
            result,
            Err(PlayerError::HttpStatus { status: 404, .. })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "404 не повторяем");
    }

    #[test]
    fn range_errors_are_retried_from_the_same_position() {
        let data = pattern(32 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let failed = Arc::new(AtomicBool::new(false));
        let fail_once = Arc::clone(&failed);
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            let range = record_range(request, history);
            if range.as_deref() != Some("bytes=0-0") && !fail_once.swap(true, Ordering::SeqCst) {
                return reply(500, Vec::new(), b"oops".to_vec(), Some(4));
            }
            ranged(request, history, &body, true)
        });

        let mut reader = open_test(&Source::Url(server.base.clone())).expect("открыть URL");
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte).expect("повтор после 500");
        assert_eq!(byte, [data[0]]);

        let history = ranges.lock().expect("история");
        assert!(
            history
                .iter()
                .flatten()
                .any(|range| range.starts_with("bytes=0-")),
            "повтор должен быть с той же позиции: {history:?}"
        );
    }

    #[test]
    fn final_error_is_recorded_for_the_decoder() {
        let data = pattern(8 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            let range = record_range(request, history);
            if range.as_deref() == Some("bytes=0-0") {
                return reply(
                    206,
                    vec![
                        http_header("Content-Type", "video/mp4"),
                        http_header("Content-Range", &format!("bytes 0-0/{}", body.len())),
                    ],
                    vec![body[0]],
                    Some(1),
                );
            }
            reply(404, Vec::new(), b"gone".to_vec(), Some(4))
        });

        let mut reader = open_test(&Source::Url(server.base.clone())).expect("открыть URL");
        let mut byte = [0_u8; 1];
        assert!(reader.read(&mut byte).is_err(), "блок должен упасть");
        assert_eq!(reader.failure(), Some(HttpFailure::Status(404)));
    }

    #[test]
    fn stream_restart_after_resume_limit_is_rejected() {
        let data = pattern(100 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, false)
        });

        let mut reader = open_test(&Source::Url(server.base.clone())).expect("открыть URL");
        // Целевой блок начинается дальше resume_limit (4096) — перезапуск
        // с начала уже недопустим.
        reader
            .seek(SeekFrom::Start(70_000))
            .expect("позиция известна");
        let mut byte = [0_u8; 1];
        let error = reader.read(&mut byte).expect_err("перезапуск запрещён");

        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(reader.failure(), Some(HttpFailure::NotSeekable));
    }

    #[test]
    fn cancel_interrupts_a_silent_server() {
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            std::thread::sleep(Duration::from_secs(2));
            reply(200, Vec::new(), b"late".to_vec(), Some(4))
        });
        let source = Source::Url(server.base.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        let config = HttpConfig {
            connect_timeout: Duration::from_millis(300),
            read_timeout: Duration::from_millis(300),
            idle_timeout: Duration::from_millis(300),
            retry_delays: [Duration::from_millis(1); 4],
            ..HttpConfig::default()
        };

        let (sender, receiver) = std::sync::mpsc::channel();
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            let result = HttpReader::open(&source, config, flag).map(|_| ());
            let _ = sender.send(result);
        });

        std::thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::SeqCst);

        let result = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("отмена должна сработать");
        assert!(matches!(result, Err(PlayerError::Cancelled)));
    }

    #[test]
    fn reader_reports_network_waiting() {
        let data = pattern(32 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            record_range(request, history);
            std::thread::sleep(Duration::from_millis(250));
            ranged(request, history, &body, true)
        });

        let mut reader = open_test(&Source::Url(server.base.clone())).expect("открыть URL");
        let flag = reader.buffering_flag();
        let handle = std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            reader.read_exact(&mut byte)
        });

        std::thread::sleep(Duration::from_millis(100));
        assert!(
            flag.load(Ordering::SeqCst),
            "пока ждём ответ, читатель буферизует"
        );

        handle
            .join()
            .expect("поток чтения")
            .expect("прочитать байт");
        assert!(!flag.load(Ordering::SeqCst), "после ответа флаг снят");
    }

    #[test]
    fn tls_uses_native_provider_and_platform_roots() {
        let tls = tls_config();

        assert_eq!(tls.provider(), ureq::tls::TlsProvider::NativeTls);
        assert!(matches!(
            tls.root_certs(),
            ureq::tls::RootCerts::PlatformVerifier
        ));
    }

    #[test]
    fn content_range_total_is_parsed() {
        assert_eq!(parse_content_range("bytes 0-0/12345"), Some(Some(12_345)));
        assert_eq!(parse_content_range("bytes 0-0/*"), Some(None));
        assert_eq!(parse_content_range("broken"), None);
    }
}
