//! HTTP-читатель для сетевых источников.
//!
//! Читатель отдаёт байты через [`MediaReader`], поэтому декодеры не знают, что
//! источник сетевой. Для файла, поддерживающего `Range`, читается окно
//! вперёд и перемотка выполняется новым запросом. Сервер без `Range` отдаёт
//! поток только с начала: он играет, но не перематывается.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::reader::MediaReader;
use crate::sync::NetworkStats;
use crate::{PlayerError, Source};

/// Сколько байт читается одним запросом.
const WINDOW_SIZE: usize = 2 * 1024 * 1024;

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

/// С какого времени сетевое чтение считается медленным для статистики.
///
/// 100 мс — примерно три кадра при 30 fps: такие паузы уже видны глазом.
const SLOW_READ: Duration = Duration::from_millis(100);

/// Настройки HTTP-читателя.
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
    /// Размер окна чтения.
    pub(crate) window: usize,
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
            window: WINDOW_SIZE,
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

/// Счётчики сетевого чтения для оверлея статистики.
///
/// Живут в `Arc`: читатель уезжает внутрь FFmpeg, а ядро опрашивает счётчики
/// из своего цикла.
#[derive(Debug, Default)]
pub(crate) struct HttpCounters {
    requests: AtomicU64,
    bytes: AtomicU64,
    slow_reads: AtomicU64,
    slow_ms: AtomicU64,
    retries: AtomicU64,
    restarts: AtomicU64,
}

impl HttpCounters {
    fn add_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    fn add_bytes(&self, len: usize) {
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    fn add_slow_read(&self, elapsed: Duration) {
        self.slow_reads.fetch_add(1, Ordering::Relaxed);
        self.slow_ms
            .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
    }

    fn add_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    fn add_restart(&self) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
    }

    /// Срез для `SyncStats`.
    pub(crate) fn snapshot(&self) -> NetworkStats {
        NetworkStats {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            slow_reads: self.slow_reads.load(Ordering::Relaxed),
            slow_ms: self.slow_ms.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
        }
    }
}

/// Запрос окна фоновому загрузчику.
enum FetchRequest {
    Window {
        start: u64,
        end: u64,
        /// Поколение запроса: после перемотки ответ устаревает.
        generation: u64,
    },
}

/// Ответ фонового загрузчика.
struct FetchResponse {
    start: u64,
    generation: u64,
    result: Result<Vec<u8>, HttpError>,
}

/// Фоновый загрузчик окон.
///
/// Зачем: заливка окна занимает доли секунды, и если делать её в потоке
/// декодера, всё это время FFmpeg стоит, а кадры концу очереди опоздают.
/// Поэтому следующее окно качается параллельно с работой декодера.
struct Prefetch {
    requests: Sender<FetchRequest>,
    responses: Receiver<FetchResponse>,
    /// Начало уже запрошенного окна.
    pending: Option<u64>,
    /// Пришедшее, но ещё не прочитанное окно.
    ready: Option<(u64, Vec<u8>)>,
}

/// Сетевой источник байтов.
pub(crate) struct HttpReader {
    agent: ureq::Agent,
    config: HttpConfig,
    url: String,
    hint: Option<String>,
    len: Option<u64>,
    seekable: bool,
    /// Текущая позиция чтения.
    position: u64,
    /// Окно прочитанных байт.
    window: Vec<u8>,
    /// Абсолютное смещение первого байта окна.
    window_start: u64,
    /// Сколько байт окна действительно заполнено.
    window_len: usize,
    /// Поток без `Range`: сервер отдаёт байты только по порядку.
    stream: Option<ureq::BodyReader<'static>>,
    /// Абсолютная позиция следующего байта из потока.
    stream_pos: u64,
    window_size: usize,
    /// Флаг отмены из ядра.
    cancel: Arc<AtomicBool>,
    /// Читатель ждёт сеть; ядро превращает это в `Buffering`.
    buffering: Arc<AtomicBool>,
    /// Причина последней ошибки — для слоя декодера.
    failure: Arc<Mutex<Option<HttpFailure>>>,
    /// Счётчики для статистики.
    counters: Arc<HttpCounters>,
    /// Фоновый загрузчик окон: есть только у источников с `Range`.
    prefetch: Option<Prefetch>,
    /// Номер поколения: перемотка делает старые запросы негодными.
    generation: Arc<AtomicU64>,
}

impl HttpReader {
    /// Открывает URL и выясняет размер и поддержку `Range`.
    ///
    /// Проверочный запрос просит один байт: по ответу `206` видно поддержку
    /// диапазонов и полный размер из `Content-Range`; `200` означает, что
    /// сервер игнорирует `Range`.
    pub(crate) fn open(
        source: &Source,
        config: HttpConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, PlayerError> {
        let Source::Url(url) = source else {
            return Err(PlayerError::UnsupportedSource {
                target: source.clone(),
            });
        };

        let agent = agent(&config);
        let counters = Arc::new(HttpCounters::default());
        let probe_counters = Arc::clone(&counters);
        let mut probe = || {
            if cancel.load(Ordering::SeqCst) {
                return Err(HttpError::cancelled());
            }
            probe_counters.add_request();
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

        let (len, seekable, stream) = if status == 206 {
            let total = header(&response, "content-range")
                .as_deref()
                .and_then(parse_content_range)
                .flatten();
            (total, true, None)
        } else {
            // Ответ на проверочный запрос уже содержит тело с начала файла:
            // не выбрасываем его, а читаем дальше без повторного соединения.
            let body = response
                .into_body()
                .into_with_config()
                .limit(u64::MAX)
                .reader();
            (content_length, false, Some(body))
        };

        if len == Some(0) {
            return Err(HttpFailure::Empty.into_player_error(source));
        }

        let generation = Arc::new(AtomicU64::new(0));
        // Фоновый загрузчик имеет смысл только у источника с `Range`: без него
        // окно неоткуда взять, кроме одного последовательного потока.
        let prefetch = seekable.then(|| {
            let (request_tx, request_rx) = mpsc::channel();
            let (response_tx, response_rx) = mpsc::channel();
            spawn_prefetch(PrefetchContext {
                agent: agent.clone(),
                config,
                url: url.clone(),
                cancel: Arc::clone(&cancel),
                generation: Arc::clone(&generation),
                counters: Arc::clone(&counters),
                requests: request_rx,
                responses: response_tx,
            });
            Prefetch {
                requests: request_tx,
                responses: response_rx,
                pending: None,
                ready: None,
            }
        });

        Ok(Self {
            agent,
            config,
            url: url.clone(),
            hint: source.file_name().map(str::to_owned),
            len,
            seekable,
            position: 0,
            window: Vec::new(),
            window_start: 0,
            window_len: 0,
            stream,
            stream_pos: 0,
            window_size: config.window.max(1),
            cancel,
            buffering: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
            counters,
            prefetch,
            generation,
        })
    }

    /// Догружает окно с текущей позиции.
    fn fill_window(&mut self) -> io::Result<()> {
        if self.seekable {
            self.fill_ranged()
        } else {
            self.fill_stream()
        }
    }

    /// Окно запросом `Range`: сервер обязан ответить `206`.
    ///
    /// С фоновым загрузчиком окно берётся из него, а следующее сразу же
    /// заказывается — декодер не ждёт сеть Почти никогда.
    fn fill_ranged(&mut self) -> io::Result<()> {
        let start = self.position;
        if let Some(len) = self.len
            && start >= len
        {
            self.window_len = 0;
            return Ok(());
        }

        let buffer = if self.prefetch.is_some() {
            self.window_ahead(start)?
        } else {
            self.fetch_sync(start)?
        };

        self.set_window(start, buffer);
        // Пока декодер читает это окно, готовим следующее.
        self.request_window(start + self.window_len as u64);
        Ok(())
    }

    /// Конец диапазона окна с учётом размера источника.
    fn range_end(&self, start: u64) -> u64 {
        let mut end = start.saturating_add(self.window_size as u64 - 1);
        if let Some(len) = self.len {
            end = end.min(len - 1);
        }
        end
    }

    /// Окно из фонового загрузчика.
    ///
    /// Если готового окна нет, ждём его — только это ожидание и есть реальная
    /// просадка, поэтому именно оно попадает в счётчик медленных чтений.
    fn window_ahead(&mut self, start: u64) -> io::Result<Vec<u8>> {
        let ready_start = self
            .prefetch
            .as_ref()
            .and_then(|prefetch| prefetch.ready.as_ref())
            .map(|(ready_start, _)| *ready_start);
        if ready_start == Some(start)
            && let Some((_, buffer)) = self.prefetch.as_mut().and_then(|p| p.ready.take())
        {
            return Ok(buffer);
        }

        self.request_window(start);

        self.buffering.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let result = loop {
            let response = self
                .prefetch
                .as_ref()
                .and_then(|prefetch| prefetch.responses.recv().ok());
            let Some(response) = response else {
                break Err(io::Error::other("фоновое чтение остановлено"));
            };

            if let Some(prefetch) = self.prefetch.as_mut()
                && prefetch.pending == Some(response.start)
            {
                prefetch.pending = None;
            }
            // Ответ на запрос до перемотки больше не нужен.
            if response.generation != self.generation.load(Ordering::SeqCst) {
                continue;
            }

            match response.result {
                Err(error) => break Err(self.fail(error)),
                Ok(buffer) if response.start == start => break Ok(buffer),
                Ok(buffer) => {
                    // Пришло другое окно: оставляем на следующий раз.
                    if let Some(prefetch) = self.prefetch.as_mut() {
                        prefetch.ready = Some((response.start, buffer));
                    }
                }
            }
        };
        let waited = started.elapsed();
        if waited >= SLOW_READ {
            self.counters.add_slow_read(waited);
        }
        self.buffering.store(false, Ordering::SeqCst);
        result
    }

    /// Просит фоновый загрузчик скачать окно с этой позиции.
    fn request_window(&mut self, start: u64) {
        if let Some(len) = self.len
            && start >= len
        {
            return;
        }
        let end = self.range_end(start);
        let generation = self.generation.load(Ordering::SeqCst);
        let Some(prefetch) = self.prefetch.as_mut() else {
            return;
        };
        if prefetch.pending == Some(start) {
            return;
        }
        let request = FetchRequest::Window {
            start,
            end,
            generation,
        };
        if prefetch.requests.send(request).is_ok() {
            prefetch.pending = Some(start);
        }
    }

    /// Синхронная догрузка окна: источник без фонового загрузчика.
    fn fetch_sync(&mut self, start: u64) -> io::Result<Vec<u8>> {
        let end = self.range_end(start);
        let range = format!("bytes={start}-{end}");
        let expected = (end - start + 1) as usize;

        let mut attempt = || {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(HttpError::cancelled());
            }
            self.buffering.store(true, Ordering::SeqCst);
            let started = Instant::now();
            let result = (|| {
                self.counters.add_request();
                let response = self
                    .agent
                    .get(&self.url)
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
                    .limit(expected as u64 + 1)
                    .reader();
                let mut buffer = Vec::with_capacity(expected);
                body.read_to_end(&mut buffer).map_err(body_error)?;
                buffer.truncate(expected);
                Ok(buffer)
            })();
            let elapsed = started.elapsed();
            if elapsed >= SLOW_READ {
                self.counters.add_slow_read(elapsed);
            }
            self.buffering.store(false, Ordering::SeqCst);
            result
        };

        let stop = || self.cancel.load(Ordering::SeqCst);
        match with_retries(&self.config, &stop, &self.counters, &mut attempt) {
            Ok(buffer) => {
                self.counters.add_bytes(buffer.len());
                Ok(buffer)
            }
            Err(error) => Err(self.fail(error)),
        }
    }

    /// Окно из последовательного потока без `Range`.
    fn fill_stream(&mut self) -> io::Result<()> {
        let start = self.position;
        if let Some(len) = self.len
            && start >= len
        {
            self.window_len = 0;
            return Ok(());
        }

        let remaining = self.len.map_or(u64::MAX, |len| len.saturating_sub(start));
        let cap = (self.window_size as u64).min(remaining) as usize;

        // Сначала просто продолжаем уже открытый поток.
        if self.stream.is_some() && self.stream_pos == start {
            self.buffering.store(true, Ordering::SeqCst);
            let started = Instant::now();
            let result = {
                let body = self.stream.as_mut().expect("поток открыт");
                read_window(body, cap, &|| self.cancel.load(Ordering::SeqCst))
            };
            let elapsed = started.elapsed();
            if elapsed >= SLOW_READ {
                self.counters.add_slow_read(elapsed);
            }
            self.buffering.store(false, Ordering::SeqCst);
            match result {
                Ok(buffer) => {
                    self.counters.add_bytes(buffer.len());
                    self.stream_pos += buffer.len() as u64;
                    self.set_window(start, buffer);
                    return Ok(());
                }
                Err(error) => {
                    // Поток сломался: дальше только перезапуск с начала.
                    self.stream = None;
                    if !error.retryable {
                        return Err(self.fail(error));
                    }
                }
            }
        }

        // Без `Range` продолжить с места нельзя: перезапуск с начала и пропуск
        // уже прочитанного. Решение B: только до `resume_limit`.
        if start > self.config.resume_limit {
            return Err(self.fail(HttpError::fatal(HttpFailure::NotSeekable)));
        }

        let mut attempt = || {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(HttpError::cancelled());
            }
            self.buffering.store(true, Ordering::SeqCst);
            self.counters.add_request();
            let started = Instant::now();
            let result = (|| {
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
                skip_stream(&mut body, start, &|| self.cancel.load(Ordering::SeqCst))?;
                let buffer = read_window(&mut body, cap, &|| self.cancel.load(Ordering::SeqCst))?;
                Ok((body, buffer))
            })();
            let elapsed = started.elapsed();
            if elapsed >= SLOW_READ {
                self.counters.add_slow_read(elapsed);
            }
            self.buffering.store(false, Ordering::SeqCst);
            result
        };

        // Без `Range` каждая такая попытка заново читает файл с начала.
        if start > 0 {
            self.counters.add_restart();
        }

        let stop = || self.cancel.load(Ordering::SeqCst);
        match with_retries(&self.config, &stop, &self.counters, &mut attempt) {
            Ok((body, buffer)) => {
                self.counters.add_bytes(buffer.len());
                self.stream = Some(body);
                self.stream_pos = start + buffer.len() as u64;
                self.set_window(start, buffer);
                Ok(())
            }
            Err(error) => Err(self.fail(error)),
        }
    }

    fn set_window(&mut self, start: u64, buffer: Vec<u8>) {
        self.window = buffer;
        self.window_start = start;
        self.window_len = self.window.len();
    }

    /// Сохраняет причину ошибки для слоя декодера.
    fn fail(&self, error: HttpError) -> io::Error {
        if let Ok(mut slot) = self.failure.lock() {
            *slot = Some(error.failure.clone());
        }
        error.failure.into_io()
    }

    /// Последняя сетевая ошибка — для подмены текста FFmpeg.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn failure(&self) -> Option<HttpFailure> {
        self.failure.lock().ok().and_then(|slot| slot.clone())
    }

    /// Слот ошибки: декодер читает его, когда FFmpeg вернул `invalid data`.
    pub(crate) fn failure_slot(&self) -> Arc<Mutex<Option<HttpFailure>>> {
        Arc::clone(&self.failure)
    }

    /// Флаг «ждём сеть»: ядро превращает его в `Buffering`.
    pub(crate) fn buffering_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.buffering)
    }

    /// Счётчики сети: ядро опрашивает их для статистики.
    pub(crate) fn counters(&self) -> Arc<HttpCounters> {
        Arc::clone(&self.counters)
    }
}

impl Read for HttpReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(HttpFailure::Cancelled.into_io());
        }
        if buffer.is_empty() {
            return Ok(0);
        }
        if let Some(len) = self.len
            && self.position >= len
        {
            return Ok(0);
        }

        let in_window = self.position >= self.window_start
            && self.position < self.window_start.saturating_add(self.window_len as u64);
        if !in_window {
            self.fill_window()?;
            if self.window_len == 0 {
                return Ok(0);
            }
        }

        let offset = (self.position - self.window_start) as usize;
        let available = self.window_len.saturating_sub(offset);
        if available == 0 {
            return Ok(0);
        }

        let read = available.min(buffer.len());
        buffer[..read].copy_from_slice(&self.window[offset..offset + read]);
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for HttpReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(HttpFailure::Cancelled.into_io());
        }
        let target = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => {
                let Some(len) = self.len else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "размер источника неизвестен",
                    ));
                };
                i128::from(len) + i128::from(delta)
            }
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "позиция до начала источника",
            ));
        }
        let target = target as u64;
        if let Some(len) = self.len
            && target > len
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "позиция за концом источника",
            ));
        }

        self.position = target;
        // Старые ответы фонового загрузчика относятся к другой позиции.
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(prefetch) = self.prefetch.as_mut() {
            prefetch.pending = None;
            prefetch.ready = None;
        }
        // Старое окно относится к другой позиции.
        self.window_len = 0;
        Ok(target)
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

/// Всё, что нужно фоновому загрузчику.
struct PrefetchContext {
    agent: ureq::Agent,
    config: HttpConfig,
    url: String,
    cancel: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    counters: Arc<HttpCounters>,
    requests: Receiver<FetchRequest>,
    responses: Sender<FetchResponse>,
}

/// Запускает фоновый загрузчик окон.
///
/// Поток живёт, пока у него есть отправитель запросов: `HttpReader` уронил его при
/// `Drop`, и мы выходим по `recv`. Заодно это и способ остановки при замене
/// источника.
fn spawn_prefetch(context: PrefetchContext) {
    let PrefetchContext {
        agent,
        config,
        url,
        cancel,
        generation,
        counters,
        requests,
        responses,
    } = context;

    std::thread::spawn(move || {
        while let Ok(FetchRequest::Window {
            start,
            end,
            generation: wanted,
        }) = requests.recv()
        {
            // Запрос устарел, если его отменили или сменилось поколение.
            let stop = {
                let cancel = Arc::clone(&cancel);
                let generation = Arc::clone(&generation);
                move || {
                    cancel.load(Ordering::Relaxed) || generation.load(Ordering::Relaxed) != wanted
                }
            };
            let result = fetch_range(&agent, &config, &url, start, end, &counters, &stop);
            if responses
                .send(FetchResponse {
                    start,
                    generation: wanted,
                    result,
                })
                .is_err()
            {
                return;
            }
        }
    });
}

/// Скачивает окно `[start, end]` с повторами.
///
/// Счётчики медленных чтений здесь не ведутся: медленная загрузка в фоне
/// ничего не блокирует. Ждать её начинает читатель, и он же это считает.
fn fetch_range(
    agent: &ureq::Agent,
    config: &HttpConfig,
    url: &str,
    start: u64,
    end: u64,
    counters: &HttpCounters,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, HttpError> {
    let range = format!("bytes={start}-{end}");
    let expected = (end - start + 1) as usize;

    let mut attempt = || {
        if stop() {
            return Err(HttpError::cancelled());
        }
        counters.add_request();
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
            .limit(expected as u64 + 1)
            .reader();
        let buffer = read_window(&mut body, expected, stop)?;
        counters.add_bytes(buffer.len());
        Ok(buffer)
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
/// `stop` шире, чем флаг отмены: фоновому загрузчику нужно ещё и прекратить
/// работу, когда окно устарело после перемотки.
fn with_retries<T>(
    config: &HttpConfig,
    stop: &dyn Fn() -> bool,
    counters: &HttpCounters,
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

/// Читает окно из последовательного потока.
fn read_window(
    body: &mut ureq::BodyReader<'static>,
    cap: usize,
    stop: &dyn Fn() -> bool,
) -> Result<Vec<u8>, HttpError> {
    let mut buffer = Vec::with_capacity(cap);
    let mut scratch = [0_u8; 32 * 1024];

    while buffer.len() < cap {
        if stop() {
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
                // Соединение без `Range` держит тело открытым; второй читатель
                // (звук и видео) должен обслуживаться параллельно.
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

    use tiny_http::{Header, Response, Server, StatusCode};

    use super::*;

    const TEST_WINDOW: usize = 64 * 1024;

    struct TestServer {
        base: String,
        _thread: JoinHandle<()>,
    }

    fn test_config() -> HttpConfig {
        HttpConfig {
            window: TEST_WINDOW,
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

        // Чтение через границу окна: второй запрос Range.
        reader.seek(SeekFrom::Start(0)).expect("в начало");
        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("прочитать окно");
        assert_eq!(first, data[..TEST_WINDOW]);
        let mut next = [0_u8; 16];
        reader.read_exact(&mut next).expect("прочитать после окна");
        assert_eq!(next, data[TEST_WINDOW..TEST_WINDOW + 16]);

        let history = ranges.lock().expect("история");
        assert!(
            history
                .iter()
                .flatten()
                .any(|range| range.starts_with("bytes=150000-")),
            "должен быть запрос Range с новой позиции: {history:?}"
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

        // Дальше окна: поток продолжается без повторного соединения.
        reader.seek(SeekFrom::Start(0)).expect("в начало");
        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("прочитать окно");
        assert_eq!(first, data[..TEST_WINDOW]);

        let mut second = [0_u8; 1024];
        reader
            .read_exact(&mut second)
            .expect("продолжить после окна");
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
        // Проверочный запрос и два окна: данных чуть больше одного окна.
        assert_eq!(after.requests, 3);
        assert_eq!(after.bytes, data.len() as u64);
    }

    #[test]
    fn prefetch_requests_the_next_window_ahead() {
        let data = pattern(4 * TEST_WINDOW);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("первое окно");
        assert_eq!(first, data[..TEST_WINDOW]);

        // Второе окно заказано заранее: проверка + окно 0 + окно 1.
        let deadline = Instant::now() + Duration::from_secs(2);
        while ranges.lock().expect("история").len() < 3 {
            assert!(Instant::now() < deadline, "фоновая загрузка не началась");
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut second = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut second).expect("второе окно");
        assert_eq!(second, data[TEST_WINDOW..2 * TEST_WINDOW]);
    }

    #[test]
    fn seek_ignores_a_stale_prefetch() {
        let data = pattern(4 * TEST_WINDOW);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, true)
        });

        let source = Source::Url(server.base.clone());
        let mut reader = open_test(&source).expect("открыть URL");

        let mut first = vec![0_u8; TEST_WINDOW];
        reader.read_exact(&mut first).expect("первое окно");

        // Второе окно уже в пути; перемотка делает его негодным.
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
        assert!(reader.read(&mut byte).is_err(), "окно должно упасть");
        assert_eq!(reader.failure(), Some(HttpFailure::Status(404)));
    }

    #[test]
    fn stream_restart_after_resume_limit_is_rejected() {
        let data = pattern(10 * 1024);
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let body = data.clone();
        let server = spawn(Arc::clone(&ranges), move |request, history| {
            ranged(request, history, &body, false)
        });

        let mut reader = open_test(&Source::Url(server.base.clone())).expect("открыть URL");
        reader
            .seek(SeekFrom::Start(5_000))
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
