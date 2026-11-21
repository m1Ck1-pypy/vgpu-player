//! Демультиплексирование и декодирование аудио.
//!
//! Основной путь — Symphonia: он же даёт точную перемотку и длительность для
//! контейнеров, которые умеет читать. Но Symphonia 0.5.5 не декодирует,
//! например, Opus. Поэтому декодер — это один из двух бэкендов:
//!
//! 1. Symphonia — первым;
//! 2. FFmpeg — если Symphonia не смогла открыть или создать декодер.
//!
//! Наружу оба отдают [`AudioChunk`] с `f32`: дальше ядро, часы и rodio не
//! знают, какой бэкенд работает.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ffmpeg::format::Sample as FfmpegSample;
use ffmpeg::format::sample::Type as SampleType;
use ffmpeg::media::Type as MediaType;
use ffmpeg::software::resampling::context::Context as ResamplingContext;
use ffmpeg::util::frame::audio::Audio as FfmpegFrame;
use ffmpeg::{ChannelLayout, Error as FfmpegError, Rational};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;
use tracing::{debug, info};

use crate::http::{HttpFailure, HttpSource};
use crate::video::{codec_name_of, container_name, init_ffmpeg};
use crate::{PlayerError, Source};

/// Готовый блок сэмплов: `f32`, каналы вперемешку.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AudioChunk {
    pub(crate) samples: Vec<f32>,
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
}

/// Декодер звука с автоматическим выбором бэкенда.
pub(crate) enum AudioDecoder {
    /// Основной путь: Symphonia.
    Symphonia(SymphoniaDecoder),
    /// Запасной путь: FFmpeg для кодеков, которых нет в Symphonia.
    Ffmpeg(Box<FfmpegDecoder>),
}

impl AudioDecoder {
    /// Открывает звуковую дорожку: сначала Symphonia, затем FFmpeg.
    ///
    /// Ошибку Symphonia возвращаем, только если FFmpeg тоже не справился:
    /// для битого файла её текст обычно понятнее. Для URL Symphonia не годится
    /// (`MediaSource` требует `Sync`), поэтому там сразу FFmpeg.
    /// Версия для тестов: без контроля отмены.
    #[cfg(test)]
    pub(crate) fn open(source: &Source) -> Result<Self, PlayerError> {
        Self::open_with_cancel(source, Arc::new(AtomicBool::new(false)), None)
    }

    /// То же, но с флагом отмены для сетевого читателя.
    ///
    /// `shared` — уже открытый сетевой источник: кэш у звука и видео общий.
    pub(crate) fn open_with_cancel(
        source: &Source,
        cancel: Arc<AtomicBool>,
        shared: Option<&Arc<HttpSource>>,
    ) -> Result<Self, PlayerError> {
        let Source::File(path) = source else {
            return FfmpegDecoder::open_with_cancel(source, cancel, shared)
                .map(|decoder| Self::Ffmpeg(Box::new(decoder)));
        };

        match SymphoniaDecoder::open(path) {
            Ok(decoder) => Ok(Self::Symphonia(decoder)),
            // Нет файла или нет прав: второй бэкенд даст ту же ошибку.
            Err(error @ PlayerError::OpenFile { .. }) => Err(error),
            Err(primary_error) => match FfmpegDecoder::open_with_cancel(source, cancel, shared) {
                Ok(decoder) => {
                    info!(
                        path = %path.display(),
                        %primary_error,
                        "звук открыт через FFmpeg"
                    );
                    Ok(Self::Ffmpeg(Box::new(decoder)))
                }
                Err(fallback_error) => {
                    debug!(
                        path = %path.display(),
                        %fallback_error,
                        "FFmpeg тоже не открыл звук"
                    );
                    Err(primary_error)
                }
            },
        }
    }

    /// Можно ли перематывать звуковую дорожку.
    pub(crate) fn is_seekable(&self) -> bool {
        match self {
            Self::Symphonia(_) => true,
            Self::Ffmpeg(decoder) => decoder.seekable,
        }
    }

    /// Ждёт ли звуковой читатель сеть прямо сейчас.
    pub(crate) fn is_buffering(&self) -> bool {
        match self {
            Self::Symphonia(_) => false,
            Self::Ffmpeg(decoder) => decoder
                .buffering
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed)),
        }
    }

    /// Длительность из заголовка, если она там есть.
    pub(crate) fn duration(&self) -> Option<Duration> {
        match self {
            Self::Symphonia(decoder) => decoder.duration(),
            Self::Ffmpeg(decoder) => decoder.duration(),
        }
    }

    /// Число каналов открытого потока.
    pub(crate) fn channels(&self) -> u16 {
        match self {
            Self::Symphonia(decoder) => decoder.channels(),
            Self::Ffmpeg(decoder) => decoder.channels(),
        }
    }

    /// Частота дискретизации открытого потока.
    pub(crate) fn sample_rate(&self) -> u32 {
        match self {
            Self::Symphonia(decoder) => decoder.sample_rate(),
            Self::Ffmpeg(decoder) => decoder.sample_rate(),
        }
    }

    /// Декодирует следующий блок. `None` — файл закончился.
    pub(crate) fn next_chunk(&mut self) -> Result<Option<AudioChunk>, PlayerError> {
        match self {
            Self::Symphonia(decoder) => decoder.next_chunk(),
            Self::Ffmpeg(decoder) => decoder.next_chunk(),
        }
    }

    /// Переходит к позиции.
    pub(crate) fn seek(&mut self, position: Duration) -> Result<(), PlayerError> {
        match self {
            Self::Symphonia(decoder) => decoder.seek(position),
            Self::Ffmpeg(decoder) => decoder.seek(position),
        }
    }

    /// Название контейнера; Symphonia его не отдаёт в нужном виде.
    pub(crate) fn container_name(&self) -> Option<&str> {
        match self {
            Self::Symphonia(_) => None,
            Self::Ffmpeg(decoder) => decoder.container_name(),
        }
    }

    /// Короткое имя кодека.
    pub(crate) fn codec_name(&self) -> Option<&str> {
        match self {
            Self::Symphonia(_) => None,
            Self::Ffmpeg(decoder) => decoder.codec_name(),
        }
    }
}

/// Открытый файл через Symphonia: демультиплексор плюс декодер.
pub(crate) struct SymphoniaDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: u16,
    sample_rate: u32,
    duration: Option<Duration>,
    /// Первый декодированный блок. Нужен, потому что заголовок MP4 не всегда
    /// сообщает число каналов: настоящие каналы и частота берутся из потока.
    pending: Option<AudioChunk>,
}

impl SymphoniaDecoder {
    /// Открывает файл и готовит декодер первой пригодной аудиодорожки.
    fn open(path: &Path) -> Result<Self, PlayerError> {
        let target = Source::File(path.to_path_buf());
        let file = File::open(path).map_err(|source| PlayerError::OpenFile {
            target: target.clone(),
            source,
        })?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
            hint.with_extension(extension);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                stream,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|source| PlayerError::Unsupported {
                target: target.clone(),
                source,
            })?;

        let format = probed.format;

        // Данные дорожки копируем сразу: дальше `format` нужен изменяемым.
        let (track_id, sample_rate, channels, duration, decoder) = {
            // `default_track()` в файле с видео указывает на видеодорожку,
            // поэтому ищем первую дорожку, у которой есть параметры звука.
            let track = format
                .tracks()
                .iter()
                .find(|track| {
                    track.codec_params.codec != CODEC_TYPE_NULL
                        && track.codec_params.sample_rate.is_some()
                })
                .ok_or_else(|| PlayerError::NoPlayableTrack {
                    target: target.clone(),
                })?;

            let params = &track.codec_params;
            let sample_rate = params
                .sample_rate
                .ok_or_else(|| PlayerError::NoPlayableTrack {
                    target: target.clone(),
                })?;
            let channels = params.channels.map_or(1, |channels| channels.count());
            let channels = u16::try_from(channels)
                .ok()
                .filter(|channels| *channels > 0)
                .ok_or(PlayerError::InvalidAudioParams {
                    channels: u16::try_from(channels).unwrap_or(u16::MAX),
                    sample_rate,
                })?;

            let duration = duration_of(params.n_frames, params.time_base, sample_rate);

            let decoder = symphonia::default::get_codecs()
                .make(params, &DecoderOptions::default())
                .map_err(|source| PlayerError::Unsupported {
                    target: target.clone(),
                    source,
                })?;

            (track.id, sample_rate, channels, duration, decoder)
        };

        let mut decoder = Self {
            format,
            decoder,
            track_id,
            channels,
            sample_rate,
            duration,
            pending: None,
        };
        // Первый блок сразу декодируем: если заголовок промолчал о каналах,
        // здесь появятся настоящие значения.
        decoder.read_first_chunk()?;
        Ok(decoder)
    }

    /// Декодирует первый блок, уточняет каналы и частоту и запоминает его.
    fn read_first_chunk(&mut self) -> Result<(), PlayerError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(());
                }
                Err(error) => return Err(PlayerError::Decode(error)),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(error) => return Err(PlayerError::Decode(error)),
            };

            let chunk = chunk_from(decoded);
            self.channels = chunk.channels;
            self.sample_rate = chunk.sample_rate;
            self.pending = Some(chunk);
            return Ok(());
        }
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Декодирует следующий блок. `None` — файл закончился.
    ///
    /// Битые пакеты в середине файла пропускаются: один испорченный кадр не
    /// повод останавливать воспроизведение.
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, PlayerError> {
        if let Some(chunk) = self.pending.take() {
            return Ok(Some(chunk));
        }

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::IoError(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(PlayerError::Decode(error)),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(error) => return Err(PlayerError::Decode(error)),
            };

            let chunk = chunk_from(decoded);
            self.channels = chunk.channels;
            self.sample_rate = chunk.sample_rate;
            return Ok(Some(chunk));
        }
    }

    /// Переходит к позиции.
    fn seek(&mut self, position: Duration) -> Result<(), PlayerError> {
        self.pending = None;
        let time = Time {
            seconds: position.as_secs(),
            frac: f64::from(position.subsec_nanos()) / 1e9,
        };
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(PlayerError::Decode)?;
        self.decoder.reset();
        Ok(())
    }
}

/// Аудиодекодер FFmpeg для кодеков, которых нет в Symphonia (например, Opus).
pub(crate) struct FfmpegDecoder {
    target: Source,
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    stream_index: usize,
    time_base: Rational,
    channels: u16,
    sample_rate: u32,
    duration: Option<Duration>,
    /// Название контейнера от FFmpeg.
    container: Option<String>,
    /// Короткое имя кодека от FFmpeg.
    codec: Option<String>,
    /// Флаг отмены: FFmpeg опрашивает его во время блокирующего чтения.
    cancel: Arc<AtomicBool>,
    /// Можно ли перематывать источник.
    seekable: bool,
    /// Слот HTTP-ошибки: им заменяем безликий `invalid data` FFmpeg.
    http_failure: Option<Arc<Mutex<Option<HttpFailure>>>>,
    /// Флаг «ждём сеть» для события `Buffering`.
    buffering: Option<Arc<AtomicBool>>,
    /// Конвертер в `f32` вперемешку; пересобирается при смене формата.
    resampler: Option<ResamplingContext>,
    /// Параметры, под которые настроен `resampler`.
    source: Option<(FfmpegSample, ChannelLayout, u32)>,
    /// После перемотки отбрасываем кадры раньше этой позиции.
    discard_until: Option<Duration>,
    /// `send_eof` уже отработал: дальше только дочитываем кадры.
    eof_sent: bool,
    /// Сколько кадров уже сконвертировано (диагностика).
    converted_chunks: u64,
}

// SAFETY: контексты FFmpeg не потокобезопасны для одновременного доступа, но
// декодер не делится между потоками: он целиком перемещается в поток ядра и
// используется только там. `Send` разрешает именно передачу владения.
unsafe impl Send for FfmpegDecoder {}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // Снимаем возможное блокирующее чтение до закрытия контекста FFmpeg.
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl FfmpegDecoder {
    /// Открывает звук с отменой сетевого читателя.
    fn open_with_cancel(
        target: &Source,
        reader_cancel: Arc<AtomicBool>,
        shared: Option<&Arc<HttpSource>>,
    ) -> Result<Self, PlayerError> {
        init_ffmpeg(target)?;

        let opened = crate::reader::open_reader(target, reader_cancel, shared)?;
        let seekable = opened.reader.is_seekable();
        let failure = opened.failure;
        let buffering = opened.buffering;
        let reader = opened.reader;
        let hint = reader.hint_name().map(str::to_owned);
        debug!(
            len = reader.len(),
            seekable,
            hint = ?hint,
            "аудиоисточник открыт"
        );
        let stream_io = ffmpeg::format::context::StreamIo::from_read_seek(reader)
            .map_err(|error| open_error(target, &failure, error))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::clone(&cancel);
        let input = ffmpeg::format::input_from_stream_with_interrupt(
            stream_io,
            hint.as_deref(),
            None,
            move || interrupt.load(Ordering::Relaxed),
        )
        .map_err(|error| open_error(target, &failure, error))?;

        let (stream_index, time_base, decoder, codec) = {
            let stream = input.streams().best(MediaType::Audio).ok_or_else(|| {
                PlayerError::NoPlayableTrack {
                    target: target.clone(),
                }
            })?;
            let codec = codec_name_of(&stream.parameters());

            let context = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                .map_err(|error| open_error(target, &failure, error))?;
            let decoder = context
                .decoder()
                .audio()
                .map_err(|error| open_error(target, &failure, error))?;

            (stream.index(), stream.time_base(), decoder, codec)
        };

        let channels = decoder.channels();
        let sample_rate = decoder.rate();
        if channels == 0 || sample_rate == 0 {
            return Err(PlayerError::InvalidAudioParams {
                channels,
                sample_rate,
            });
        }

        let duration = container_duration(&input);
        let container = container_name(&input);

        Ok(Self {
            target: target.clone(),
            input,
            decoder,
            stream_index,
            time_base,
            channels,
            sample_rate,
            duration,
            container,
            codec,
            cancel,
            seekable,
            http_failure: failure,
            buffering,
            resampler: None,
            source: None,
            discard_until: None,
            eof_sent: false,
            converted_chunks: 0,
        })
    }

    fn container_name(&self) -> Option<&str> {
        self.container.as_deref()
    }

    fn codec_name(&self) -> Option<&str> {
        self.codec.as_deref()
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
            return failure.into_player_error(&self.target);
        }
        audio_error(&self.target, error)
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Декодирует следующий блок в `f32` вперемешку.
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, PlayerError> {
        loop {
            let mut frame = FfmpegFrame::empty();
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    if self.should_discard(&frame) {
                        continue;
                    }
                    if let Some(chunk) = self.convert(&frame)? {
                        return Ok(Some(chunk));
                    }
                }
                Err(FfmpegError::Eof) => {
                    debug!("аудио: декодер отдал EOF");
                    return Ok(None);
                }
                Err(error) if is_eagain(&error) => {}
                Err(error) => return Err(self.error(error)),
            }

            if self.eof_sent {
                debug!("аудио: EAGAIN после send_eof");
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

    /// Читает следующий пакет именно аудиодорожки, пропуская остальные.
    fn next_packet(&mut self) -> Result<Option<ffmpeg::Packet>, PlayerError> {
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        return Ok(Some(packet));
                    }
                }
                Err(FfmpegError::Eof) => {
                    debug!("аудио: пакеты закончились");
                    return Ok(None);
                }
                // Битый пакет не повод останавливать воспроизведение.
                Err(FfmpegError::InvalidData) => {
                    if self.http_failure.is_none() {
                        debug!("аудио: битый пакет пропущен");
                        continue;
                    }
                    debug!("аудио: обрыв сети вместо битого пакета");
                    return Err(self.error(FfmpegError::InvalidData));
                }
                Err(error) => return Err(self.error(error)),
            }
        }
    }

    /// Переходит к позиции и отбрасывает кадры до неё.
    fn seek(&mut self, position: Duration) -> Result<(), PlayerError> {
        // `Input::seek` принимает время в AV_TIME_BASE, то есть в микросекундах.
        let timestamp = i64::try_from(position.as_micros()).unwrap_or(i64::MAX);
        self.input
            .seek(timestamp, ..)
            .map_err(|error| self.error(error))?;
        self.decoder.flush();
        self.discard_until = Some(position);
        self.eof_sent = false;
        Ok(())
    }

    /// Нужно ли выбросить кадр: он целиком раньше цели перемотки.
    fn should_discard(&mut self, frame: &FfmpegFrame) -> bool {
        let Some(target) = self.discard_until else {
            return false;
        };

        // Без PTS судить не о чем: не рискуем выбросить весь звук.
        let Some(pts) = frame
            .pts()
            .and_then(|timestamp| timestamp_to_duration(timestamp, self.time_base))
        else {
            self.discard_until = None;
            return false;
        };

        let length = if frame.rate() > 0 {
            Duration::from_secs_f64(f64::from(frame.rate()).recip() * frame.samples() as f64)
        } else {
            Duration::ZERO
        };

        if pts.saturating_add(length) <= target {
            return true;
        }

        self.discard_until = None;
        false
    }

    /// Конвертирует кадр в `f32` без изменения частоты и раскладки каналов.
    fn convert(&mut self, frame: &FfmpegFrame) -> Result<Option<AudioChunk>, PlayerError> {
        let source = (frame.format(), frame.channel_layout(), frame.rate());
        if self.source != Some(source) {
            self.resampler = Some(
                ResamplingContext::get(
                    source.0,
                    source.1,
                    source.2,
                    FfmpegSample::F32(SampleType::Packed),
                    source.1,
                    source.2,
                )
                .map_err(|error| self.error(error))?,
            );
            self.source = Some(source);
        }

        let resampler = self
            .resampler
            .as_mut()
            .ok_or(PlayerError::InvalidAudioParams {
                channels: self.channels,
                sample_rate: self.sample_rate,
            })?;

        // Кадр-приёмник выделяем под каждый вход: swresample пишет не больше,
        // чем помещается в `nb_samples`, и переиспользование меньшего буфера
        // молча обрезало бы звук (первый Opus-пакет после pre-skip короче
        // остальных).
        let mut converted = FfmpegFrame::new(
            FfmpegSample::F32(SampleType::Packed),
            frame.samples(),
            frame.channel_layout(),
        );
        resampler
            .run(frame, &mut converted)
            .map_err(|error| self.error(error))?;

        self.converted_chunks = self.converted_chunks.saturating_add(1);
        if self.converted_chunks == 1 || self.converted_chunks.is_multiple_of(200) {
            debug!(
                in_format = ?frame.format(),
                in_channels = frame.channels(),
                in_rate = frame.rate(),
                in_samples = frame.samples(),
                out_format = ?converted.format(),
                out_channels = converted.channels(),
                out_rate = converted.rate(),
                out_samples = converted.samples(),
                out_planes = converted.planes(),
                "аудио: конвертация"
            );
        }

        if converted.samples() == 0 || converted.planes() == 0 {
            return Ok(None);
        }

        Ok(Some(AudioChunk {
            samples: converted.plane::<f32>(0).to_vec(),
            channels: self.channels,
            sample_rate: self.sample_rate,
        }))
    }
}

/// Переводит декодированный Symphonia-блок в [`AudioChunk`].
///
/// Число каналов и частоту берём из спецификации блока, а не из заголовка:
/// в MP4 `stsd` не всегда заполняет `channels`, и тогда заголовок врал бы.
fn chunk_from(decoded: symphonia::core::audio::AudioBufferRef<'_>) -> AudioChunk {
    let spec = *decoded.spec();
    let channels = u16::try_from(spec.channels.count())
        .ok()
        .filter(|channels| *channels > 0)
        .unwrap_or(1);

    let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    buffer.copy_interleaved_ref(decoded);

    AudioChunk {
        samples: buffer.samples().to_vec(),
        channels,
        sample_rate: spec.rate,
    }
}

/// Ошибка FFmpeg с путём к файлу.
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
    audio_error(target, error)
}

fn audio_error(target: &Source, error: FfmpegError) -> PlayerError {
    PlayerError::AudioDecode {
        target: target.clone(),
        message: error.to_string(),
    }
}

/// `ffmpeg-next` отдаёт EAGAIN как `Error::Other` с POSIX-кодом.
fn is_eagain(error: &FfmpegError) -> bool {
    matches!(error, FfmpegError::Other { errno } if *errno == ffmpeg::error::EAGAIN)
}

/// Переводит PTS из временной базы потока в `Duration`.
fn timestamp_to_duration(timestamp: i64, time_base: Rational) -> Option<Duration> {
    let numerator = i128::from(time_base.numerator());
    let denominator = i128::from(time_base.denominator());
    if timestamp < 0 || numerator <= 0 || denominator <= 0 {
        return None;
    }

    let nanos = i128::from(timestamp) * numerator * 1_000_000_000 / denominator;
    u64::try_from(nanos).ok().map(Duration::from_nanos)
}

/// Длительность контейнера: FFmpeg хранит её в микросекундах.
fn container_duration(input: &ffmpeg::format::context::Input) -> Option<Duration> {
    let duration = input.duration();
    if duration <= 0 {
        return None;
    }
    u64::try_from(duration).ok().map(Duration::from_micros)
}

/// Длительность из числа кадров: либо через временную базу, либо через частоту.
fn duration_of(
    frames: Option<u64>,
    time_base: Option<symphonia::core::units::TimeBase>,
    sample_rate: u32,
) -> Option<Duration> {
    let frames = frames?;

    if let Some(time_base) = time_base {
        let time = time_base.calc_time(frames);
        return Some(Duration::from_secs_f64(time.seconds as f64 + time.frac));
    }

    (sample_rate > 0).then(|| Duration::from_secs_f64(frames as f64 / f64::from(sample_rate)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    const SAMPLE_RATE: u32 = 8_000;
    const CHANNELS: u16 = 2;
    /// Четыре секунды: на коротком файле Symphonia округляет перемотку до
    /// целого пакета, и точность проверить нельзя.
    const FRAMES: u32 = SAMPLE_RATE * 4;

    /// Генерируем WAV сами: так тест не зависит от сгенерированных вручную
    /// файлов из `assets/test/` и от наличия ffmpeg.
    fn write_wav(path: &Path) {
        let mut data = Vec::with_capacity(FRAMES as usize * usize::from(CHANNELS) * 2);
        for frame in 0..FRAMES {
            let value = i16::try_from(frame % 64).unwrap_or(0) * 500 - 16_000;
            for _ in 0..CHANNELS {
                data.extend_from_slice(&value.to_le_bytes());
            }
        }

        let block_align = CHANNELS * 2;
        let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * 2;

        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + u32::try_from(data.len()).unwrap_or(u32::MAX)).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(u32::try_from(data.len()).unwrap_or(u32::MAX)).to_le_bytes());
        wav.extend_from_slice(&data);

        let mut file = File::create(path).expect("создать тестовый wav");
        file.write_all(&wav).expect("записать тестовый wav");
    }

    /// Уникальное имя в системном временном каталоге.
    ///
    /// Имя потока не годится как часть пути: в нём есть `::`, а Windows такие
    /// имена в файлах не допускает.
    fn temp_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "vgpu-player-test-{}-{nanos}.wav",
            std::process::id()
        ))
    }

    #[test]
    fn decodes_known_number_of_samples() {
        let path = temp_path();
        write_wav(&path);

        let mut decoder = AudioDecoder::open(&Source::from(&path)).expect("открыть wav");
        assert_eq!(decoder.duration(), Some(Duration::from_secs(4)));

        let mut total = 0;
        while let Some(chunk) = decoder.next_chunk().expect("декодировать") {
            assert_eq!(chunk.channels, CHANNELS);
            assert_eq!(chunk.sample_rate, SAMPLE_RATE);
            total += chunk.samples.len();
        }

        fs::remove_file(&path).expect("удалить тестовый wav");
        assert_eq!(total, FRAMES as usize * usize::from(CHANNELS));
    }

    #[test]
    fn seek_moves_playback_position() {
        let path = temp_path();
        write_wav(&path);

        let mut decoder = AudioDecoder::open(&Source::from(&path)).expect("открыть wav");
        decoder
            .seek(Duration::from_secs(2))
            .expect("перемотать на середину");

        let mut total = 0;
        while let Some(chunk) = decoder.next_chunk().expect("декодировать") {
            total += chunk.samples.len();
        }

        fs::remove_file(&path).expect("удалить тестовый wav");

        let frames_per_second = SAMPLE_RATE as usize;
        let channels = usize::from(CHANNELS);
        let expected = frames_per_second * 2 * channels;
        // Перемотка привязана к границе пакета, поэтому допускаем половину секунды.
        let tolerance = frames_per_second / 2 * channels;
        assert!(
            total.abs_diff(expected) <= tolerance,
            "после перемотки на 2 с ожидалось около {expected} сэмплов, осталось {total}"
        );
    }

    #[test]
    fn missing_file_is_a_error() {
        let path = std::env::temp_dir().join("vgpu-player-definitely-missing.wav");
        assert!(matches!(
            AudioDecoder::open(&Source::from(&path)),
            Err(PlayerError::OpenFile { .. })
        ));
    }

    /// MP4 с видео и звуком: видео в таком файле идёт первой дорожкой.
    fn temp_a_v_file() -> Option<std::path::PathBuf> {
        let path = temp_path().with_extension("mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=32x24:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "0.3",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&path)
            .status();

        matches!(status, Ok(status) if status.success()).then_some(path)
    }

    #[test]
    fn opens_audio_track_in_a_file_with_video() {
        let Some(path) = temp_a_v_file() else {
            eprintln!("системный ffmpeg недоступен — пропускаем проверку A/V-файла");
            return;
        };

        let decoder =
            AudioDecoder::open(&Source::from(&path)).expect("найти аудиодорожку в mp4 с видео");
        assert!(decoder.sample_rate() > 0);

        fs::remove_file(&path).expect("удалить тестовый mp4");
    }

    /// MP4 с AAC, у которого заголовок может не сообщать каналы: настоящие
    /// значения должны браться из первого декодированного блока.
    fn temp_stereo_aac_file() -> Option<std::path::PathBuf> {
        let path = temp_path().with_extension("m4a");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.5",
                "-ac",
                "2",
                "-c:a",
                "aac",
            ])
            .arg(&path)
            .status();

        matches!(status, Ok(status) if status.success()).then_some(path)
    }

    #[test]
    fn stereo_aac_reports_two_channels() {
        let Some(path) = temp_stereo_aac_file() else {
            eprintln!("системный ffmpeg недоступен — пропускаем проверку AAC");
            return;
        };

        let mut decoder = AudioDecoder::open(&Source::from(&path)).expect("открыть AAC");
        assert_eq!(decoder.channels(), 2, "AAC-файл stereo, а не mono");
        assert_eq!(decoder.sample_rate(), 44_100);

        let chunk = decoder
            .next_chunk()
            .expect("декодировать AAC")
            .expect("первый блок");
        assert_eq!(chunk.channels, 2);
        assert_eq!(
            chunk.samples.len() % usize::from(chunk.channels),
            0,
            "сэмплы должны быть упакованы по каналам"
        );

        drop(decoder);
        fs::remove_file(&path).expect("удалить тестовый m4a");
    }

    /// WebM с Opus: Symphonia 0.5.5 его не декодирует, должен включиться FFmpeg.
    fn temp_opus_file() -> Option<std::path::PathBuf> {
        let path = temp_path().with_extension("webm");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.5",
                "-c:a",
                "libopus",
            ])
            .arg(&path)
            .status();

        matches!(status, Ok(status) if status.success()).then_some(path)
    }

    #[test]
    fn opus_audio_uses_ffmpeg_fallback() {
        let Some(path) = temp_opus_file() else {
            eprintln!("системный ffmpeg без libopus — пропускаем проверку Opus");
            return;
        };

        let mut decoder = AudioDecoder::open(&Source::from(&path)).expect("открыть Opus");
        assert_eq!(decoder.sample_rate(), 48_000);

        let mut total = 0;
        while let Some(chunk) = decoder.next_chunk().expect("декодировать Opus") {
            assert!(chunk.channels > 0);
            assert_eq!(chunk.sample_rate, 48_000);
            total += chunk.samples.len();
        }

        // FFmpeg держит дескриптор файла до уничтожения `Input`, а Windows не
        // даёт удалить занятый файл.
        drop(decoder);
        fs::remove_file(&path).expect("удалить тестовый webm");
        assert!(total > 0, "Opus должен дать сэмплы");
    }

    /// После перемотки FFmpeg-декодер сам выбрасывает кадры до цели, иначе
    /// звук играл бы из более раннего места, чем показывает позиция.
    #[test]
    fn opus_seek_discards_audio_before_target() {
        let Some(path) = temp_opus_file() else {
            eprintln!("системный ffmpeg без libopus — пропускаем проверку Opus");
            return;
        };

        let mut decoder = AudioDecoder::open(&Source::from(&path)).expect("открыть Opus");
        decoder
            .seek(Duration::from_millis(300))
            .expect("перемотать Opus");

        let mut total = 0;
        while let Some(chunk) = decoder.next_chunk().expect("декодировать после перемотки")
        {
            total += chunk.samples.len();
        }

        drop(decoder);
        fs::remove_file(&path).expect("удалить тестовый webm");

        // Файл 0.5 с, моно, 48 кГц. После цели 0.3 с должно остаться около
        // 0.2 с, а не весь файл.
        assert!(total > 0, "после перемотки не осталось сэмплов");
        assert!(
            total < 15_000,
            "перемотка не отбросила звук до цели: {total} сэмплов"
        );
    }
}
