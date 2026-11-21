//! Декодирование видео через FFmpeg.
//!
//! Наружу ядро отдаёт только [`VideoInfo`] и кадры [`VideoFrame`] в BGRA:
//! UI не должен знать ни про FFmpeg, ни про пиксельный формат контейнера.
//! Этап 3 — только декодирование; очередь кадров и синхронизация с аудио
//! появятся на этапах 4–5.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags};
use ffmpeg::util::frame::video::Video as FfmpegFrame;
use ffmpeg::{Error as FfmpegError, Rational};
use tracing::debug;

use crate::http::{HttpFailure, HttpSource};
use crate::{PlayerError, Source};

/// Допуск при точной перемотке: PTS кадра может быть чуть раньше цели.
const SEEK_EPSILON: Duration = Duration::from_millis(1);

/// Параметры видеопотока, которые нужны UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    /// Ширина кадра в пикселях.
    pub width: u32,
    /// Высота кадра в пикселях.
    pub height: u32,
    /// Частота кадров, если контейнер её сообщает.
    pub frame_rate: Option<f64>,
    /// Длительность файла, если она известна.
    pub duration: Option<Duration>,
}

/// Кадр в BGRA: четыре байта на пиксель, построчно, без паддинга.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Ширина кадра в пикселях.
    pub width: u32,
    /// Высота кадра в пикселях.
    pub height: u32,
    /// Пиксели в порядке B, G, R, A; длина — `width * height * 4`.
    pub bgra: Vec<u8>,
    /// Время показа кадра от начала файла.
    pub pts: Option<Duration>,
}

/// Первый кадр файла — превью для состояния «открыт, но ещё не играет».
///
/// Ядро декодирует видео только по команде `Play`, поэтому в UI до первого
/// нажатия кадра нет. Один кадр дешевле держать отдельным короткоживущим
/// декодером, чем менять цикл воспроизведения: файл открывается ещё раз,
/// декодируется ровно один кадр, декодер удаляется. `None` — в файле нет
/// видео или кадр не декодировался: превью просто не будет.
pub fn preview_frame(path: &Path) -> Option<VideoFrame> {
    let mut decoder = match VideoDecoder::open(path) {
        Ok(decoder) => decoder,
        Err(error) => {
            debug!(%error, "превью: видеодекодер не открылся");
            return None;
        }
    };

    match decoder.next_frame() {
        Ok(frame) => frame,
        Err(error) => {
            debug!(%error, "превью: первый кадр не декодировался");
            None
        }
    }
}

/// Открытый источник: демультиплексор, декодер видеодорожки и конвертер в BGRA.
pub struct VideoDecoder {
    source: Source,
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    time_base: Rational,
    converter: FrameConverter,
    info: VideoInfo,
    /// Название контейнера от FFmpeg — для панели информации о URL.
    container: Option<String>,
    /// Короткое имя кодека от FFmpeg — для панели информации о URL.
    codec: Option<String>,
    /// Флаг отмены: FFmpeg опрашивает его во время блокирующего чтения.
    cancel: Arc<AtomicBool>,
    /// Можно ли перематывать источник.
    seekable: bool,
    /// Слот HTTP-ошибки: им заменяем безликий `invalid data` FFmpeg.
    http_failure: Option<Arc<Mutex<Option<HttpFailure>>>>,
    /// Флаг «ждём сеть» для события `Buffering`.
    buffering: Option<Arc<AtomicBool>>,
    /// `send_eof` уже отработал: дальше только дочитываем кадры из декодера.
    eof_sent: bool,
    /// Кадр, уже декодированный перемоткой и ждущий конвертации.
    pending: Option<FfmpegFrame>,
}

// SAFETY: контексты FFmpeg и swscale не потокобезопасны для одновременного
// доступа, но `VideoDecoder` не делится между потоками: он целиком
// перемещается в поток ядра и используется только там. `Send` разрешает именно
// передачу владения, а не общий доступ.
unsafe impl Send for VideoDecoder {}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // Снимаем возможное блокирующее чтение до закрытия контекста FFmpeg.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl VideoDecoder {
    /// Открывает файл и готовит декодер первой видеодорожки.
    pub fn open(path: &Path) -> Result<Self, PlayerError> {
        Self::open_source(&Source::File(path.to_path_buf()))
    }

    /// Открывает источник и готовит декодер первой видеодорожки.
    pub fn open_source(source: &Source) -> Result<Self, PlayerError> {
        Self::open_source_with_cancel(source, Arc::new(AtomicBool::new(false)), None)
    }

    /// Открывает источник с отменой сетевого читателя.
    ///
    /// `shared` — уже открытый сетевой источник: кэш у звука и видео общий.
    pub(crate) fn open_source_with_cancel(
        source: &Source,
        reader_cancel: Arc<AtomicBool>,
        shared: Option<&Arc<HttpSource>>,
    ) -> Result<Self, PlayerError> {
        // Плейлист — не медиапоток: сообщаем это до любых попыток открытия.
        if source.is_hls() {
            return Err(PlayerError::UnsupportedHls {
                target: source.clone(),
            });
        }

        init_ffmpeg(source)?;

        let opened = crate::reader::open_reader(source, reader_cancel, shared)?;
        let seekable = opened.reader.is_seekable();
        let failure = opened.failure;
        let buffering = opened.buffering;
        let reader = opened.reader;
        let hint = reader.hint_name().map(str::to_owned);
        debug!(
            len = reader.len(),
            seekable,
            hint = ?hint,
            "видеоисточник открыт"
        );
        let stream_io = ffmpeg::format::context::StreamIo::from_read_seek(reader)
            .map_err(|error| open_error(source, &failure, error))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::clone(&cancel);
        let input = ffmpeg::format::input_from_stream_with_interrupt(
            stream_io,
            hint.as_deref(),
            None,
            move || interrupt.load(Ordering::Relaxed),
        )
        .map_err(|error| open_error(source, &failure, error))?;

        let (stream_index, time_base, frame_rate, decoder, codec) = {
            let stream =
                input
                    .streams()
                    .best(Type::Video)
                    .ok_or_else(|| PlayerError::NoVideoTrack {
                        target: source.clone(),
                    })?;
            let codec = codec_name_of(&stream.parameters());

            let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                .map_err(|error| open_error(source, &failure, error))?;
            let decoder = context
                .decoder()
                .video()
                .map_err(|error| open_error(source, &failure, error))?;
            let frame_rate = frame_rate_of(&decoder, stream.avg_frame_rate());

            (
                stream.index(),
                stream.time_base(),
                frame_rate,
                decoder,
                codec,
            )
        };

        let info = VideoInfo {
            width: decoder.width(),
            height: decoder.height(),
            frame_rate,
            duration: container_duration(&input),
        };
        let container = container_name(&input);

        Ok(Self {
            source: source.clone(),
            input,
            decoder,
            stream_index,
            time_base,
            converter: FrameConverter::new(),
            info,
            container,
            codec,
            cancel,
            seekable,
            http_failure: failure,
            buffering,
            eof_sent: false,
            pending: None,
        })
    }

    /// Можно ли перематывать источник.
    pub(crate) fn is_seekable(&self) -> bool {
        self.seekable
    }

    /// Ждёт ли читатель сеть прямо сейчас.
    pub(crate) fn is_buffering(&self) -> bool {
        self.buffering
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Забирает сетевую ошибку, если читатель её записал.
    fn take_http_failure(&self) -> Option<HttpFailure> {
        self.http_failure
            .as_ref()
            .and_then(|slot| slot.lock().ok().and_then(|mut guard| guard.take()))
    }

    /// Ошибка FFmpeg с подменой на понятную сетевую, если она есть.
    fn error(&self, error: FfmpegError) -> PlayerError {
        if let Some(failure) = self.take_http_failure() {
            return failure.into_player_error(&self.source);
        }
        video_error(&self.source, error)
    }

    /// Параметры открытого видеопотока.
    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    /// Название контейнера, как его даёт FFmpeg.
    pub fn container_name(&self) -> Option<&str> {
        self.container.as_deref()
    }

    /// Короткое имя кодека: `h264`, `hevc`, `av1`.
    pub fn codec_name(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    /// Декодирует следующий кадр. `None` — файл закончился.
    pub fn next_frame(&mut self) -> Result<Option<VideoFrame>, PlayerError> {
        self.next_frame_with_buffer(None)
    }

    /// Декодирует следующий кадр, переиспользуя переданный BGRA-буфер.
    ///
    /// Буфер приходит из пула ядра: если его ёмкости хватает на кадр, память
    /// (8.3 МБ на кадр 1080p) не выделяется заново. Содержимое буфера не
    /// важно — оно будет полностью перезаписано.
    pub(crate) fn next_frame_with_buffer(
        &mut self,
        buffer: Option<Vec<u8>>,
    ) -> Result<Option<VideoFrame>, PlayerError> {
        let decoded = match self.pending.take() {
            Some(decoded) => decoded,
            // Кадров больше нет: буфер не израсходован и просто освободится.
            None => match self.receive_decoded()? {
                Some(decoded) => decoded,
                None => return Ok(None),
            },
        };

        self.frame_from(&decoded, buffer).map(Some)
    }

    /// Достаёт следующий декодированный кадр без конвертации в BGRA.
    ///
    /// Нужен перемотке: кадры до целевой позиции всё равно выбрасываются, и
    /// платить за их конвертацию незачем.
    fn receive_decoded(&mut self) -> Result<Option<FfmpegFrame>, PlayerError> {
        loop {
            let mut decoded = FfmpegFrame::empty();
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => return Ok(Some(decoded)),
                Err(FfmpegError::Eof) => return Ok(None),
                Err(error) if is_eagain(&error) => {}
                Err(error) => return Err(self.error(error)),
            }

            // После `send_eof` декодер обязан отдать кадры и затем EOF.
            // EAGAIN здесь означал бы, что он ждёт данных, которых больше нет:
            // выходим, чтобы не зациклиться.
            if self.eof_sent {
                return Ok(None);
            }

            match self.next_packet()? {
                Some(packet) => match self.decoder.send_packet(&packet) {
                    Ok(()) => {}
                    Err(error) if is_eagain(&error) => {}
                    Err(error) => return Err(self.error(error)),
                },
                None => match self.decoder.send_eof() {
                    Ok(()) => self.eof_sent = true,
                    // Не все пакеты ещё забраны — сначала дочитаем их.
                    Err(error) if is_eagain(&error) => {}
                    Err(error) => return Err(self.error(error)),
                },
            }
        }
    }

    /// Переходит к позиции точно по кадрам.
    ///
    /// FFmpeg умеет перематывать только на ключевой кадр, поэтому кадры до
    /// целевой позиции декодируются и выбрасываются здесь же, без конвертации
    /// в BGRA. Первый кадр на/после позиции отдаст следующий `next_frame`.
    pub fn seek(&mut self, position: Duration) -> Result<(), PlayerError> {
        self.pending = None;

        // `Input::seek` принимает время в AV_TIME_BASE, то есть в микросекундах.
        let timestamp = i64::try_from(position.as_micros()).unwrap_or(i64::MAX);
        self.input
            .seek(timestamp, ..)
            .map_err(|error| self.error(error))?;
        self.decoder.flush();
        self.eof_sent = false;

        while let Some(decoded) = self.receive_decoded()? {
            let pts = decoded
                .pts()
                .and_then(|timestamp| timestamp_to_duration(timestamp, self.time_base));
            let too_early = pts.is_some_and(|pts| pts.saturating_add(SEEK_EPSILON) < position);
            if too_early {
                continue;
            }

            self.pending = Some(decoded);
            break;
        }

        Ok(())
    }

    /// Читает следующий пакет именно видеодорожки, пропуская остальные.
    ///
    /// Идём через `Packet::read`, а не через `packets()`: итератор молча
    /// превращает ошибки ввода-вывода в конец файла, а нам нужен честный
    /// `Error`.
    fn next_packet(&mut self) -> Result<Option<ffmpeg::Packet>, PlayerError> {
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        return Ok(Some(packet));
                    }
                }
                Err(FfmpegError::Eof) => return Ok(None),
                // Битый пакет не повод останавливать воспроизведение.
                Err(FfmpegError::InvalidData) => {
                    if self.http_failure.is_none() {
                        continue;
                    }
                    return Err(self.error(FfmpegError::InvalidData));
                }
                Err(error) => return Err(self.error(error)),
            }
        }
    }

    /// Конвертирует декодированный кадр в [`VideoFrame`] с PTS на границе ядра.
    fn frame_from(
        &mut self,
        decoded: &FfmpegFrame,
        buffer: Option<Vec<u8>>,
    ) -> Result<VideoFrame, PlayerError> {
        let bgra = self
            .converter
            .convert_into(decoded, buffer)
            .map_err(|error| self.error(error))?;

        Ok(VideoFrame {
            width: decoded.width(),
            height: decoded.height(),
            bgra,
            pts: decoded
                .pts()
                .and_then(|timestamp| timestamp_to_duration(timestamp, self.time_base)),
        })
    }
}

/// Конвертер кадров в BGRA с переиспользованием swscale-контекста.
///
/// Кадры одного потока обычно одного формата и размера, но динамическое
/// разрешение в контейнере не запрещено — поэтому при смене параметров
/// конвертер пересобирается.
struct FrameConverter {
    scaler: Option<ScalingContext>,
    source: Option<(Pixel, u32, u32)>,
    converted: FfmpegFrame,
}

impl FrameConverter {
    /// Пустой конвертер: swscale-контекст создаётся на первом кадре.
    fn new() -> Self {
        Self {
            scaler: None,
            source: None,
            converted: FfmpegFrame::empty(),
        }
    }

    /// Возвращает плотно упакованные пиксели BGRA без паддинга строк.
    #[cfg(test)]
    fn convert(&mut self, frame: &FfmpegFrame) -> Result<Vec<u8>, FfmpegError> {
        self.convert_into(frame, None)
    }

    /// Как [`Self::convert`], но может дописать пиксели в готовый буфер.
    ///
    /// Буфер приходит из пула ядра. Если его ёмкости хватает, вектор не
    /// выделяет память заново; если нет (другое разрешение) — растёт сам.
    fn convert_into(
        &mut self,
        frame: &FfmpegFrame,
        buffer: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, FfmpegError> {
        let source = (frame.format(), frame.width(), frame.height());
        if self.source != Some(source) {
            self.scaler = Some(ScalingContext::get(
                source.0,
                source.1,
                source.2,
                Pixel::BGRA,
                source.1,
                source.2,
                Flags::BILINEAR,
            )?);
            self.converted = FfmpegFrame::new(Pixel::BGRA, source.1, source.2);
            self.source = Some(source);
        }

        let scaler = self.scaler.as_mut().ok_or(FfmpegError::Bug)?;
        scaler.run(frame, &mut self.converted)?;

        // `sws_scale` выравнивает начало каждой строки, поэтому копируем
        // построчно: `stride` может быть больше `width * 4`.
        let width = usize::try_from(source.1).map_err(|_| FfmpegError::InvalidData)?;
        let height = usize::try_from(source.2).map_err(|_| FfmpegError::InvalidData)?;
        let row_len = width.checked_mul(4).ok_or(FfmpegError::InvalidData)?;
        let capacity = row_len
            .checked_mul(height)
            .ok_or(FfmpegError::InvalidData)?;

        let stride = self.converted.stride(0);
        let data = self.converted.data(0);
        let mut bgra = buffer.unwrap_or_default();
        bgra.clear();
        if bgra.capacity() < capacity {
            bgra.reserve_exact(capacity);
        }
        for row in 0..height {
            let start = row.checked_mul(stride).ok_or(FfmpegError::InvalidData)?;
            let end = start.checked_add(row_len).ok_or(FfmpegError::InvalidData)?;
            bgra.extend_from_slice(data.get(start..end).ok_or(FfmpegError::InvalidData)?);
        }

        Ok(bgra)
    }
}

/// FFmpeg инициализируется один раз на процесс.
///
/// Общая точка для видео и аудио: тесты открывают декодеры параллельно, а
/// `ffmpeg::init()` не обещает потокобезопасности — `OnceLock` снимает этот
/// вопрос.
pub(crate) fn init_ffmpeg(target: &Source) -> Result<(), PlayerError> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    let result = INIT.get_or_init(|| ffmpeg::init().map_err(|error| error.to_string()));

    match result {
        Ok(()) => Ok(()),
        Err(message) => Err(PlayerError::VideoDecode {
            target: target.clone(),
            message: format!("FFmpeg не инициализирован: {message}"),
        }),
    }
}

/// Длительность контейнера: FFmpeg хранит её в AV_TIME_BASE (микросекундах).
fn container_duration(input: &ffmpeg::format::context::Input) -> Option<Duration> {
    let duration = input.duration();
    if duration <= 0 {
        return None;
    }
    u64::try_from(duration).ok().map(Duration::from_micros)
}

/// Название контейнера по-человечески (`QuickTime / MOV`).
pub(crate) fn container_name(input: &ffmpeg::format::context::Input) -> Option<String> {
    let description = input.format().description().to_owned();
    (!description.is_empty()).then_some(description)
}

/// Короткое имя кодека (`h264`, `hevc`, `av1`).
pub(crate) fn codec_name_of(parameters: &ffmpeg::codec::Parameters) -> Option<String> {
    let name = parameters.id().name();
    (!name.is_empty() && name != "none").then(|| name.to_owned())
}

/// Частота кадров: сначала из декодера, затем из среднего по потоку.
fn frame_rate_of(decoder: &ffmpeg::decoder::Video, stream_rate: Rational) -> Option<f64> {
    let rate = decoder.frame_rate().unwrap_or(stream_rate);
    let numerator = f64::from(rate.numerator());
    let denominator = f64::from(rate.denominator());

    (numerator > 0.0 && denominator > 0.0).then(|| numerator / denominator)
}

/// Переводит PTS из временной базы потока в `Duration`.
fn timestamp_to_duration(timestamp: i64, time_base: Rational) -> Option<Duration> {
    let numerator = i128::from(time_base.numerator());
    let denominator = i128::from(time_base.denominator());
    if timestamp < 0 || numerator <= 0 || denominator <= 0 {
        return None;
    }

    // i128 не переполняется даже для больших PTS: произведение умещается
    // с большим запасом.
    let nanos = i128::from(timestamp) * numerator * 1_000_000_000 / denominator;
    u64::try_from(nanos).ok().map(Duration::from_nanos)
}

/// `ffmpeg-next` отдаёт EAGAIN как `Error::Other` с POSIX-кодом.
fn is_eagain(error: &FfmpegError) -> bool {
    matches!(error, FfmpegError::Other { errno } if *errno == ffmpeg::error::EAGAIN)
}

/// Ошибка FFmpeg с источником.
fn open_error(
    target: &Source,
    failure: &Option<Arc<Mutex<Option<HttpFailure>>>>,
    error: FfmpegError,
) -> PlayerError {
    if let Some(failure) = failure
        .as_ref()
        .and_then(|slot| slot.lock().ok().and_then(|mut guard| guard.take()))
    {
        return failure.into_player_error(target);
    }
    video_error(target, error)
}

fn video_error(target: &Source, error: FfmpegError) -> PlayerError {
    PlayerError::VideoDecode {
        target: target.clone(),
        message: describe_error(&error),
    }
}

/// Текст ошибки с подсказкой, если сборке FFmpeg не хватает кодека.
///
/// Встроенный в FFmpeg декодер `av1` умеет только аппаратное декодирование.
/// Без `libdav1d` он возвращает `ENOSYS` и текст «Function not implemented»,
/// который ничего не говорит пользователю.
fn describe_error(error: &FfmpegError) -> String {
    match error {
        FfmpegError::Other { errno } if *errno == ffmpeg::error::ENOSYS => {
            "кодек не поддерживается этой сборкой FFmpeg (для AV1 нужен libdav1d)".to_owned()
        }
        _ => error.to_string(),
    }
}

/// Общие помощники тестов: тестовое видео и временные файлы.
///
/// Модуль доступен другим тестам крейта (например, проверке канала кадров),
/// но не попадает в релизную сборку.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Уникальное имя во временном каталоге.
    pub(crate) fn temp_path(extension: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "vgpu-player-video-{}-{nanos}-{id}.{extension}",
            std::process::id()
        ))
    }

    /// Тестовое видео: ассет владельца, иначе крошечный клип через системный
    /// `ffmpeg` (он есть в ubuntu-раннере CI), иначе тест пропускается.
    ///
    /// Файлы в git не лежат (SPEC, раздел 12.2), поэтому жёстко требовать
    /// `assets/test/video_only.mp4` нельзя.
    pub(crate) fn sample_video() -> Option<PathBuf> {
        let asset = asset_path("video_only.mp4");
        if asset.is_file() {
            return Some(asset);
        }

        let path = temp_path("mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=30",
                "-t",
                "0.5",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status();

        matches!(status, Ok(status) if status.success()).then_some(path)
    }

    /// AV1-клип для проверки `libdav1d`.
    ///
    /// Генерируется локально (см. `assets/test/README.md`) и в git не
    /// попадает. Без ассета тест пропускается: в CI AV1 может быть недоступен.
    pub(crate) fn sample_av1_video() -> Option<PathBuf> {
        let asset = asset_path("av1_test.webm");
        asset.is_file().then_some(asset)
    }

    /// Звук без видео: `audio_only.m4a` из `assets/test`.
    pub(crate) fn sample_audio() -> Option<PathBuf> {
        let asset = asset_path("audio_only.m4a");
        asset.is_file().then_some(asset)
    }

    /// Короткий файл со звуком и видео: `sync_test.webm`.
    pub(crate) fn sample_av() -> Option<PathBuf> {
        let asset = asset_path("sync_test.webm");
        asset.is_file().then_some(asset)
    }

    /// Короткий клип для матрицы форматов: ассет из `assets/test`, иначе
    /// генерация системным `ffmpeg`.
    ///
    /// `None` — ни ассета, ни подходящего `ffmpeg`: тест пропускается.
    pub(crate) fn sample_clip(asset: &str, extension: &str, args: &[&str]) -> Option<PathBuf> {
        let asset = asset_path(asset);
        if asset.is_file() {
            return Some(asset);
        }

        let path = temp_path(extension);
        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y"])
            .args(args)
            .arg(&path)
            .status();

        matches!(status, Ok(status) if status.success()).then_some(path)
    }

    /// Путь к файлу в `assets/test/`.
    fn asset_path(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join("test")
            .join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{sample_av1_video, sample_video, temp_path};
    use super::*;

    /// Кадр RGB24 с известными цветами; паддинг строк заполнен мусором.
    fn rgb_frame(width: u32, height: u32) -> FfmpegFrame {
        let mut frame = FfmpegFrame::new(Pixel::RGB24, width, height);
        let row_len = usize::try_from(width).expect("ширина") * 3;
        let stride = frame.stride(0);
        let height = usize::try_from(height).expect("высота");

        let data = frame.data_mut(0);
        for y in 0..height {
            for x in 0..usize::try_from(width).expect("ширина") {
                let color = [10 + x as u8, 20 + y as u8, 30 + (x + y) as u8];
                let start = y * stride + x * 3;
                data[start..start + 3].copy_from_slice(&color);
            }
            // Эти байты не относятся к пикселям и не должны попасть в результат.
            for byte in &mut data[y * stride + row_len..(y + 1) * stride] {
                *byte = 0xAA;
            }
        }

        frame
    }

    #[test]
    fn converts_rgb24_to_bgra_without_padding() {
        let width = 3;
        let height = 2;
        let input = rgb_frame(width, height);

        let mut converter = FrameConverter::new();
        let bgra = converter.convert(&input).expect("конвертировать кадр");

        let width = usize::try_from(width).expect("ширина");
        let height = usize::try_from(height).expect("высота");
        assert_eq!(bgra.len(), width * height * 4);

        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) * 4;
                assert_eq!(
                    &bgra[offset..offset + 4],
                    &[30 + (x + y) as u8, 20 + y as u8, 10 + x as u8, 255],
                    "пиксель ({x}, {y}) сконвертирован неверно"
                );
            }
        }
    }

    #[test]
    fn preview_frame_gives_one_frame_and_skips_broken_files() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let frame = preview_frame(&path).expect("первый кадр файла должен декодироваться");
        assert!(frame.width > 0 && frame.height > 0, "у превью нет размера");
        assert_eq!(
            frame.bgra.len(),
            usize::try_from(frame.width).expect("ширина")
                * usize::try_from(frame.height).expect("высота")
                * 4,
            "BGRA-буфер превью должен быть полным"
        );

        assert!(
            preview_frame(Path::new("missing.mp4")).is_none(),
            "отсутствующий файл не даёт превью"
        );
    }

    #[test]
    fn decodes_video_file() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let mut decoder = VideoDecoder::open(&path).expect("открыть видео");
        let info = *decoder.info();
        assert!(info.width > 0, "ширина кадра не определена");
        assert!(info.height > 0, "высота кадра не определена");
        assert!(
            info.frame_rate.is_some_and(|fps| (fps - 30.0).abs() < 1.0),
            "ожидалось 30 fps, получено {:?}",
            info.frame_rate
        );
        assert!(info.duration.is_some(), "длительность не определена");

        let mut frames = 0_usize;
        let mut last_pts = None;
        while let Some(frame) = decoder.next_frame().expect("декодировать кадр") {
            assert_eq!(frame.width, info.width);
            assert_eq!(frame.height, info.height);
            let expected_len = usize::try_from(frame.width).expect("ширина")
                * usize::try_from(frame.height).expect("высота")
                * 4;
            assert_eq!(frame.bgra.len(), expected_len);

            if let (Some(pts), Some(previous)) = (frame.pts, last_pts) {
                assert!(
                    pts >= previous,
                    "PTS кадров должен расти: {pts:?} < {previous:?}"
                );
            }
            last_pts = frame.pts.or(last_pts);

            frames += 1;
            if frames >= 30 {
                break;
            }
        }

        assert!(frames > 0, "ни одного кадра не декодировано");
    }

    #[test]
    fn source_file_decodes_like_a_path() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let source = Source::File(path.clone());
        let mut from_path = VideoDecoder::open(&path).expect("открыть по пути");
        let mut from_source = VideoDecoder::open_source(&source).expect("открыть по Source");

        assert_eq!(from_path.info(), from_source.info());
        assert!(from_source.container_name().is_some());
        assert!(from_source.codec_name().is_some());

        let path_frame = from_path.next_frame().expect("кадр по пути");
        let source_frame = from_source.next_frame().expect("кадр по Source");
        assert_eq!(path_frame, source_frame);
        assert!(path_frame.is_some(), "кадр по Source должен декодироваться");
    }

    #[test]
    fn hls_is_rejected_before_opening() {
        for source in [
            Source::File(temp_path("m3u8")),
            Source::Url("https://example.com/live/index.m3u8?token=1".to_owned()),
        ] {
            let Err(error) = VideoDecoder::open_source(&source) else {
                panic!("HLS должен быть отклонён");
            };
            assert!(
                matches!(error, PlayerError::UnsupportedHls { .. }),
                "ожидалась ошибка HLS, получено: {error}"
            );
        }
    }

    #[test]
    fn seek_starts_decoding_again() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let mut decoder = VideoDecoder::open(&path).expect("открыть видео");
        decoder
            .next_frame()
            .expect("прочитать первый кадр")
            .expect("кадр до перемотки");

        decoder
            .seek(Duration::from_millis(200))
            .expect("перемотать видео");

        let frame = decoder
            .next_frame()
            .expect("декодировать после перемотки")
            .expect("кадр после перемотки");
        assert_eq!(
            frame.bgra.len(),
            usize::try_from(frame.width).expect("ширина")
                * usize::try_from(frame.height).expect("высота")
                * 4
        );
        if let Some(pts) = frame.pts {
            assert!(
                pts >= Duration::from_millis(199),
                "первый кадр после перемотки должен быть у цели, получено {pts:?}"
            );
        }
    }

    #[test]
    fn decodes_av1_video() {
        let Some(path) = sample_av1_video() else {
            eprintln!("нет assets/test/av1_test.webm — пропускаем проверку AV1");
            return;
        };

        let mut decoder = VideoDecoder::open(&path).expect("открыть AV1");
        let frame = decoder
            .next_frame()
            .expect("декодировать AV1")
            .expect("первый кадр AV1");

        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.bgra.len(),
            usize::try_from(frame.width).expect("ширина")
                * usize::try_from(frame.height).expect("высота")
                * 4
        );
    }

    /// Без libdav1d встроенный декодер AV1 отвечает ENOSYS; пользователь должен
    /// увидеть подсказку, а не «Function not implemented».
    #[test]
    fn missing_decoder_gets_a_hint() {
        let error = FfmpegError::Other {
            errno: ffmpeg::error::ENOSYS,
        };
        assert!(describe_error(&error).contains("libdav1d"));
        assert!(!describe_error(&error).contains("Function not implemented"));
    }

    #[test]
    fn converter_reuses_the_buffer_from_the_pool() {
        let input = rgb_frame(3, 2);
        let mut converter = FrameConverter::new();
        // Ёмкость заведомо достаточная: конвертер обязан писать в неё же.
        let buffer = Vec::with_capacity(3 * 2 * 4);
        let pointer = buffer.as_ptr();

        let bgra = converter
            .convert_into(&input, Some(buffer))
            .expect("конвертировать в готовый буфер");

        assert_eq!(bgra.as_ptr(), pointer, "буфер должен быть переиспользован");
        assert_eq!(bgra.len(), 3 * 2 * 4);
        assert_eq!(&bgra[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn small_buffer_is_reallocated_but_frame_is_correct() {
        let input = rgb_frame(3, 2);
        let mut converter = FrameConverter::new();
        let small = Vec::with_capacity(4);

        let bgra = converter
            .convert_into(&input, Some(small))
            .expect("конвертировать в маленький буфер");

        assert_eq!(bgra.len(), 3 * 2 * 4);
        assert_eq!(&bgra[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn png_roundtrip_preserves_frame() {
        let input = rgb_frame(2, 2);

        let mut converter = FrameConverter::new();
        let bgra = converter.convert(&input).expect("конвертировать кадр");
        let frame = VideoFrame {
            width: 2,
            height: 2,
            bgra,
            pts: Some(Duration::ZERO),
        };

        // BGRA → RGBA: `image` работает в порядке R, G, B, A.
        let (pixels, remainder) = frame.bgra.as_chunks::<4>();
        assert!(
            remainder.is_empty(),
            "кадр должен состоять из целых пикселей"
        );
        let rgba: Vec<u8> = pixels
            .iter()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
            .collect();
        let buffer = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
            .expect("собрать изображение из кадра");

        let path = temp_path("png");
        buffer.save(&path).expect("сохранить png");
        let loaded = image::open(&path).expect("прочитать png").to_rgba8();
        std::fs::remove_file(&path).expect("удалить тестовый png");

        assert_eq!(loaded.dimensions(), (frame.width, frame.height));
        assert_eq!(loaded.get_pixel(1, 1), &image::Rgba([11, 21, 32, 255]));
    }
}
