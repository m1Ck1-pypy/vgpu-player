//! Ядро воспроизведения: поток, который один владеет декодерами и выводом.
//!
//! Разделение простое: вызывающая сторона получает [`Player`] — только каналы.
//! Всё состояние (демультиплексоры, декодеры, звуковое устройство) живёт в
//! отдельном потоке, поэтому UI не блокируется на декодировании и IO.
//!
//! Видео и звук необязательны по отдельности: файл может содержать только
//! одну из дорожек. Кадры идут в отдельный ограниченный канал, события — в
//! неограниченный.

use std::num::{NonZeroU16, NonZeroU32};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded, unbounded};
use ffmpeg::media::Type;
use rodio::buffer::SamplesBuffer;
use rodio::source::Source;
use rodio::{ChannelCount, MixerDeviceSink, Sample, SampleRate};
use tracing::{debug, error, info};

use crate::audio::{AudioChunk, AudioDecoder};
use crate::clock::AudioClock;
use crate::source::Source as MediaSource;
use crate::spectrum::{
    SAMPLE_RING_CAPACITY, SpectrumAnalyzer, SpectrumBars, SpectrumRing, SpectrumTap,
};
use crate::sync::{FrameAction, FrameScheduler, SyncStats, VideoTimer};
use crate::video::{VideoDecoder, VideoFrame, VideoInfo};
use crate::{Command, Event, PlayerError};

/// Как часто поток ядра просыпается без команд, чтобы добрать звук в очередь.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Сколько блоков звука можно держать в очереди вывода.
///
/// Ограничение нужно и для звука: без него декодер вычитает файл целиком
/// вперёд, и память растёт вместе с длительностью ролика.
const QUEUED_CHUNKS: usize = 4;

/// Сколько готовых кадров ждёт UI.
///
/// Неограниченную очередь кадров держать нельзя: декодер убегает вперёд, и
/// память растёт на гигабайты. 4 кадра — это около 130 мс при 30 fps.
const QUEUED_FRAMES: usize = 4;

/// Сколько свободных BGRA-буферов ядро держит про запас.
///
/// С запасом к очереди кадров: один буфер в работе у декодера, один ждёт
/// своего времени, остальные могут быть у UI. Предел не даёт пулу расти,
/// если UI вернёт буферов больше, чем ядро успевает использовать.
const POOL_LIMIT: usize = QUEUED_FRAMES + 2;

/// Максимальная громкость, которую ядро принимает от UI.
const MAX_VOLUME: f32 = 2.0;

/// Как часто отправлять `PositionChanged`.
///
/// Позиция меняется каждый блок, но UI нужно обновлять реже: 50 мс хватает и
/// для текста, и для полосы прогресса, а цикл событий не забивается.
const POSITION_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

/// Сколько кадров обрабатываем за один тик.
///
/// При догоне (после перемотки или подвисания) декодер может отдать много
/// кадров сразу; ограничение оставляет циклу время на команды и звук.
const MAX_FRAMES_PER_TICK: usize = 4;

/// Как часто отправлять `Stats` в UI.
const SYNC_STATS_INTERVAL: Duration = Duration::from_millis(500);

/// Как часто отправлять `Spectrum` в UI.
///
/// 33 мс ≈ 30 кадров в секунду: столбики двигаются плавно, а цикл событий
/// не забивается лишними перерисовками.
const SPECTRUM_INTERVAL: Duration = Duration::from_millis(33);

/// Ручка управления воспроизведением.
///
/// Каналы команд и событий неограниченные: их мало, а блокировать UI из-за
/// полного канала нельзя. Канал кадров ограничен. Поток ядра завершается,
/// когда `Player` уничтожен и каналы закрылись.
pub struct Player {
    commands: Sender<EngineMessage>,
    events: Receiver<Event>,
    frames: Receiver<TaggedFrame>,
    /// Буферы кадров, которые больше не нужны: ядро переиспользует их.
    recycled: Sender<Vec<u8>>,
    /// Поколение, к которому относятся запрошенные кадры.
    frame_epoch: AtomicU64,
    video_info: Option<VideoInfo>,
    media_info: MediaInfo,
}

impl Player {
    /// Открывает источник и запускает поток ядра.
    ///
    /// Хотя бы одна дорожка — звук или видео — должна открыться. Ошибки
    /// открытия возвращаются сразу, а не приходят событием: вызывающему нужно
    /// знать о них до того, как он начнёт строить UI.
    pub fn open(source: impl Into<MediaSource>) -> Result<Self, PlayerError> {
        let source = source.into();
        let media = Media::open(&source)?;
        let video_info = media.video_info();
        let media_info = media.info(&source);

        let (command_sender, command_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let (frame_sender, frame_receiver) = bounded(QUEUED_FRAMES);
        let (recycled_sender, recycled_receiver) = unbounded();

        std::thread::Builder::new()
            .name("player-engine".to_owned())
            .spawn(move || {
                if let Err(error) = run_engine(
                    media,
                    &command_receiver,
                    &event_sender,
                    &frame_sender,
                    &recycled_receiver,
                ) {
                    error!(%error, "поток ядра остановлен с ошибкой");
                    let _ = event_sender.send(Event::Error(error.to_string()));
                }
            })
            .map_err(PlayerError::StartEngine)?;

        Ok(Self {
            commands: command_sender,
            events: event_receiver,
            frames: frame_receiver,
            recycled: recycled_sender,
            frame_epoch: AtomicU64::new(0),
            video_info,
            media_info,
        })
    }

    /// Передаёт команду ядру.
    ///
    /// Для `Seek` и `Open` увеличивается поколение кадров: всё, что ядро успело
    /// отправить до обработки команды, будет отброшено в [`Self::try_recv_frame`].
    pub fn send(&self, command: Command) -> Result<(), PlayerError> {
        let epoch = match &command {
            Command::Seek(_) | Command::Open(_) => {
                Some(self.frame_epoch.fetch_add(1, Ordering::Relaxed) + 1)
            }
            _ => None,
        };

        self.commands
            .send(EngineMessage { command, epoch })
            .map_err(|_| PlayerError::EngineGone)
    }

    /// Забирает готовое событие, если оно уже есть. Не блокирует.
    pub fn try_recv_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Ждёт следующее событие. Для консольного запуска; UI так блокироваться
    /// не должен.
    pub fn recv_event(&self) -> Option<Event> {
        self.events.recv().ok()
    }

    /// Забирает самый свежий готовый кадр текущего поколения.
    ///
    /// Промежуточные кадры отбрасываются: UI всё равно успевает показать
    /// только последний. Кадры старых поколений (до перемотки или открытия)
    /// тоже отбрасываются, даже если успели попасть в канал.
    pub fn try_recv_frame(&self) -> Option<VideoFrame> {
        let epoch = self.frame_epoch.load(Ordering::Relaxed);
        latest_frame(&self.frames, epoch, &self.recycled)
    }

    /// Возвращает BGRA-буфер кадра в пул ядра.
    ///
    /// Вызывается, когда кадр больше не нужен: показан и заменён новым или
    /// файл закрыт. Без этого ядро выделяло бы по 8.3 МБ на каждый кадр 1080p
    /// тридцать раз в секунду. Сам [`VideoFrame`] после вызова не используется.
    pub fn recycle_frame(&self, frame: VideoFrame) {
        // Канал неограничен, а ядро само ограничивает пул. Ошибка отправки
        // значит только, что поток ядра уже завершился.
        let _ = self.recycled.send(frame.bgra);
    }

    /// Параметры видеопотока, если в файле есть видео.
    ///
    /// Значение известно сразу после открытия, поэтому UI получает его без
    /// события. `Command::Open` его не обновляет: UI открывает новый `Player`.
    pub fn video_info(&self) -> Option<VideoInfo> {
        self.video_info
    }

    /// Что известно о дорожках: контейнер, кодеки, параметры звука.
    ///
    /// Как и [`Self::video_info`], значение известно сразу после открытия и не
    /// меняется: `Command::Open` создаёт новый [`Player`].
    pub fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }
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

/// Сообщение в поток ядра.
///
/// Для команд, меняющих позицию, указывается поколение кадров: ядро помечает
/// им всё, что отправит после обработки команды. Публичный [`Command`] при
/// этом не меняется.
struct EngineMessage {
    command: Command,
    epoch: Option<u64>,
}

/// Кадр вместе с номером поколения.
struct TaggedFrame {
    epoch: u64,
    frame: VideoFrame,
}

/// Забирает самый свежий кадр указанного поколения.
///
/// Кадры других поколений всё равно извлекаются из канала: иначе после
/// перемотки очередь осталась бы занятой старыми кадрами. Кадры, которые не
/// показываются, отдают свой буфер обратно в пул.
fn latest_frame(
    frames: &Receiver<TaggedFrame>,
    epoch: u64,
    recycled: &Sender<Vec<u8>>,
) -> Option<VideoFrame> {
    let mut latest: Option<VideoFrame> = None;
    while let Ok(tagged) = frames.try_recv() {
        if tagged.epoch != epoch {
            let _ = recycled.send(tagged.frame.bgra);
            continue;
        }

        if let Some(previous) = latest.replace(tagged.frame) {
            let _ = recycled.send(previous.bgra);
        }
    }
    latest
}

/// Кладёт буфер в пул, не позволяя пулу расти без предела.
fn return_buffer(pool: &mut Vec<Vec<u8>>, buffer: Vec<u8>) {
    if pool.len() < POOL_LIMIT {
        pool.push(buffer);
    }
}

/// Открытые источники файла. Любой из них может отсутствовать.
struct Media {
    audio: Option<AudioDecoder>,
    video: Option<VideoDecoder>,
}

impl Media {
    /// Открывает то, что удалось: звук и видео независимы.
    ///
    /// Ошибку одной дорожки при живой другой не считаем фатальной: ролик без
    /// звука или со сломанным видео должен всё равно играть. Когда не открылось
    /// ничего, возвращаем ошибку звука — она возникает первой и обычно точнее
    /// описывает проблему с источником. Подробные состояния ошибок — этап 7.
    fn open(source: &MediaSource) -> Result<Self, PlayerError> {
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
        let video = VideoDecoder::open_source(source);

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
    fn duration(&self) -> Option<Duration> {
        self.audio
            .as_ref()
            .and_then(|audio| audio.duration())
            .or_else(|| self.video.as_ref().and_then(|video| video.info().duration))
    }

    /// Метаданные видео, если дорожка открыта.
    fn video_info(&self) -> Option<VideoInfo> {
        self.video.as_ref().map(|video| *video.info())
    }

    /// Что известно о содержимом источника — для панели «Информация о файле».
    ///
    /// Для локального файла заголовок читается отдельным проходом; для URL
    /// его нет, поэтому контейнер и кодеки берём из уже открытых декодеров.
    fn info(&self, source: &MediaSource) -> MediaInfo {
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

/// Вывод звука вместе с устройством.
///
/// Устройство обязано жить столько же, сколько поток вывода: при его удалении
/// OS-поток закрывается.
struct AudioSink {
    player: rodio::Player,
    _device: MixerDeviceSink,
    /// Кольцо моно-сэмплов для визуализатора: пишет поток вывода, читает движок.
    spectrum: Arc<SpectrumRing>,
}

impl AudioSink {
    /// Открывает устройство по умолчанию и ставит вывод на паузу.
    fn open(volume: f32) -> Result<Self, PlayerError> {
        let mut device =
            rodio::DeviceSinkBuilder::open_default_sink().map_err(PlayerError::AudioDevice)?;
        // Без этого rodio печатает предупреждение в stderr при каждом закрытии.
        device.log_on_drop(false);

        let player = rodio::Player::connect_new(device.mixer());
        player.pause();
        player.set_volume(volume);

        Ok(Self {
            player,
            _device: device,
            spectrum: Arc::new(SpectrumRing::new(SAMPLE_RING_CAPACITY)),
        })
    }
}

/// Цикл потока ядра: команды, дозаправка очередей, конец файла, часы.
fn run_engine(
    mut media: Media,
    commands: &Receiver<EngineMessage>,
    events: &Sender<Event>,
    frames: &Sender<TaggedFrame>,
    recycled: &Receiver<Vec<u8>>,
) -> Result<(), PlayerError> {
    let mut volume = 1.0_f32;
    let mut output = if media.audio.is_some() {
        Some(AudioSink::open(volume)?)
    } else {
        None
    };
    let mut clock = media
        .audio
        .as_ref()
        .map(|audio| AudioClock::new(audio.channels(), audio.sample_rate()));

    // Стартуем на паузе: воспроизведение начинается по команде `Play`.
    let mut paused = true;
    let mut audio_finished = media.audio.is_none();
    let mut video_finished = media.video.is_none();
    let mut end_reported = false;
    let mut last_position = Duration::ZERO;
    let mut last_position_at = Instant::now();
    let mut scheduler = FrameScheduler::new(media.video_info().and_then(|info| info.frame_rate));
    let mut video_timer = VideoTimer::new();
    let mut pending_frame: Option<PendingFrame> = None;
    // PTS последнего отправленного кадра: от него видео продолжается, когда
    // звук уже закончился.
    let mut last_shown_pts: Option<Duration> = None;
    // Запасной таймер уже перехватил видео после конца звука.
    let mut tail_started = false;
    let mut stats = SyncCounters::new(media.duration());
    let mut frame_epoch = 0_u64;
    // Диагностика: одно сообщение, когда обе дорожки декодированы, а очередь
    // звука ещё не опустела.
    let mut waiting_output_logged = false;
    // Диагностика: сколько блоков декодировано и их пик.
    let mut audio_chunks = 0_u64;
    // Свободные BGRA-буферы: кадр пишется в готовый, без новой аллокации.
    let mut buffer_pool: Vec<Vec<u8>> = Vec::new();
    // Визуализатор есть только у файла без видео: у видео картинка своя.
    let mut spectrum = spectrum_analyzer(&media);
    let mut spectrum_cursor = 0_u64;
    let mut spectrum_samples: Vec<f32> = Vec::new();
    let mut last_spectrum_at = Instant::now();

    if let Some(duration) = media.duration() {
        let _ = events.send(Event::DurationKnown(duration));
    }

    loop {
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(EngineMessage {
                command: Command::Play,
                ..
            }) => {
                paused = false;
                video_timer.start(Instant::now());
                if let Some(sink) = output.as_ref() {
                    sink.player.play();
                }
            }
            Ok(EngineMessage {
                command: Command::Pause,
                ..
            }) => {
                paused = true;
                video_timer.pause(Instant::now());
                if let Some(sink) = output.as_ref() {
                    sink.player.pause();
                }
                // Отдать финальную позицию, чтобы UI не замирал на предыдущем
                // тике часов.
                report_position(
                    events,
                    current_position(clock.as_ref(), video_timer_ref(&media, &video_timer)),
                    &mut last_position,
                    &mut last_position_at,
                    true,
                );
            }
            Ok(EngineMessage {
                command: Command::SetVolume(value),
                ..
            }) => {
                if value.is_finite() {
                    volume = value.clamp(0.0, MAX_VOLUME);
                    if let Some(sink) = output.as_ref() {
                        sink.player.set_volume(volume);
                    }
                }
            }
            Ok(EngineMessage {
                command: Command::Seek(position),
                epoch,
            }) => {
                if let Some(epoch) = epoch {
                    frame_epoch = epoch;
                }
                info!(position = ?position, "перемотка");
                tail_started = false;
                last_shown_pts = None;
                // Точная перемотка видео может занять время: останавливаем
                // звук, чтобы часы не убежали вперёд, пока декодер догоняет
                // целевой кадр.
                if let Some(sink) = output.as_ref() {
                    sink.player.pause();
                }
                if let Some(video) = media.video.as_mut() {
                    video.seek(position)?;
                }
                if let Some(audio) = media.audio.as_mut() {
                    audio.seek(position)?;
                }
                if let Some(clock) = clock.as_mut() {
                    clock.rebase(position);
                }
                // Кадры от старой позиции больше не нужны.
                if let Some(pending) = pending_frame.take() {
                    return_buffer(&mut buffer_pool, pending.frame.bgra);
                }
                scheduler.reset();
                video_timer.rebase(position, Instant::now());
                restart(
                    output.as_ref().map(|sink| &sink.player),
                    paused,
                    &mut audio_finished,
                    &mut video_finished,
                    &mut end_reported,
                );
                // Старый звук в кольце к новой позиции не относится.
                reset_spectrum(&mut spectrum, output.as_ref(), &mut spectrum_cursor, events);
                last_spectrum_at = Instant::now();
                report_position(
                    events,
                    current_position(clock.as_ref(), video_timer_ref(&media, &video_timer)),
                    &mut last_position,
                    &mut last_position_at,
                    true,
                );
            }
            Ok(EngineMessage {
                command: Command::Open(source),
                epoch,
            }) => {
                if let Some(epoch) = epoch {
                    frame_epoch = epoch;
                }
                info!(source = %source, "открытие нового источника");
                tail_started = false;
                last_shown_pts = None;
                let opened = Media::open(&source)?;
                if opened.audio.is_some() && output.is_none() {
                    output = Some(AudioSink::open(volume)?);
                }
                media = opened;
                clock = media
                    .audio
                    .as_ref()
                    .map(|audio| AudioClock::new(audio.channels(), audio.sample_rate()));
                spectrum = spectrum_analyzer(&media);
                // У нового файла может быть другое разрешение: старые буферы
                // только займут память.
                pending_frame = None;
                buffer_pool.clear();
                scheduler =
                    FrameScheduler::new(media.video_info().and_then(|info| info.frame_rate));
                video_timer = VideoTimer::new();
                if !paused {
                    video_timer.start(Instant::now());
                }
                stats = SyncCounters::new(media.duration());
                restart(
                    output.as_ref().map(|sink| &sink.player),
                    paused,
                    &mut audio_finished,
                    &mut video_finished,
                    &mut end_reported,
                );
                reset_spectrum(&mut spectrum, output.as_ref(), &mut spectrum_cursor, events);
                last_spectrum_at = Instant::now();
                if let Some(duration) = media.duration() {
                    let _ = events.send(Event::DurationKnown(duration));
                }
                report_position(
                    events,
                    current_position(clock.as_ref(), video_timer_ref(&media, &video_timer)),
                    &mut last_position,
                    &mut last_position_at,
                    true,
                );
            }
            Ok(EngineMessage {
                command: Command::Stop,
                ..
            }) => {
                if let Some(sink) = output.as_ref() {
                    sink.player.stop();
                }
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Забираем буферы, которые вернул UI: следующий кадр запишется в один
        // из них без выделения памяти на 8 МБ.
        while let Ok(buffer) = recycled.try_recv() {
            return_buffer(&mut buffer_pool, buffer);
        }

        if let (Some(audio), Some(sink), Some(clock)) =
            (media.audio.as_mut(), output.as_ref(), clock.as_ref())
        {
            let spectrum_tap = spectrum.as_ref().map(|_| Arc::clone(&sink.spectrum));
            while !audio_finished && sink.player.len() < QUEUED_CHUNKS {
                match audio.next_chunk()? {
                    Some(chunk) => {
                        audio_chunks = audio_chunks.saturating_add(1);
                        if audio_chunks == 1 || audio_chunks.is_multiple_of(200) {
                            let peak = chunk
                                .samples
                                .iter()
                                .fold(0.0_f32, |max, sample| max.max(sample.abs()));
                            debug!(
                                chunks = audio_chunks,
                                channels = chunk.channels,
                                sample_rate = chunk.sample_rate,
                                samples = chunk.samples.len(),
                                peak,
                                "аудиоблок"
                            );
                        }
                        sink.player.append(chunk_to_source(
                            chunk,
                            clock.counter(),
                            spectrum_tap.clone(),
                        )?);
                    }
                    None => {
                        debug!("аудио закончилось");
                        audio_finished = true;
                    }
                }
            }
        }

        // Свежие сэмплы превращаем в столбики не чаще `SPECTRUM_INTERVAL`:
        // декодер отдаёт блоки быстрее, чем UI успевает их нарисовать.
        if let (Some(analyzer), Some(sink)) = (spectrum.as_mut(), output.as_ref()) {
            if sink
                .spectrum
                .read(&mut spectrum_cursor, &mut spectrum_samples)
                > 0
            {
                analyzer.push(&spectrum_samples);
            }
            if !paused && last_spectrum_at.elapsed() >= SPECTRUM_INTERVAL {
                let _ = events.send(Event::Spectrum(analyzer.update()));
                last_spectrum_at = Instant::now();
            }
        }

        // Состояние дорожек нужно и для пейсинга, и для проверки конца файла.
        let audio_done = media.audio.is_none() || audio_finished;
        let output_empty = output.as_ref().is_none_or(|sink| sink.player.empty());
        // Пока звук играет или ещё стоит в очереди, ведут аудио-часы. После
        // конца звука часы больше не идут: если оставить их ведущими, последние
        // кадры видео с большим PTS будут вечно ждать своей позиции.
        let audio_clock_leads = !audio_done || !output_empty;

        if !audio_clock_leads && media.video.is_some() && !tail_started {
            // Перебазируем таймер на последний показанный кадр, а не на
            // «текущее время»: иначе оставшиеся кадры разом опоздают и будут
            // пропущены вместо того, чтобы доиграть.
            let resume = last_shown_pts.unwrap_or(Duration::ZERO);
            video_timer.rebase(resume, Instant::now());
            tail_started = true;
            debug!(?resume, "звук закончился: видео доигрывает по таймеру");
        }

        // Если UI не успевает забирать кадры, не декодируем и не пропускаем
        // следующие: иначе при полной очереди начинается каскад пропусков.
        if !paused
            && !frames.is_full()
            && let Some(video) = media.video.as_mut()
        {
            let mut handled = 0_usize;
            while !video_finished && handled < MAX_FRAMES_PER_TICK {
                if pending_frame.is_none() {
                    match video.next_frame_with_buffer(buffer_pool.pop())? {
                        Some(frame) => {
                            let pts = scheduler.frame_pts(frame.pts);
                            pending_frame = Some(PendingFrame { frame, pts });
                        }
                        None => {
                            debug!("видео закончилось");
                            video_finished = true;
                            break;
                        }
                    }
                }

                let Some(pending) = pending_frame.take() else {
                    break;
                };
                let position = if audio_clock_leads {
                    video_position(clock.as_ref(), &video_timer)
                } else {
                    video_timer.position(Instant::now())
                };

                match FrameScheduler::decide(pending.pts, position) {
                    // Ещё рано: оставляем кадр до следующего тика.
                    FrameAction::Wait => {
                        pending_frame = Some(pending);
                        break;
                    }
                    // Опоздал: догоняем звук, кадр не показываем.
                    FrameAction::Skip => {
                        debug!(pts = ?pending.pts, "кадр пропущен: опоздал к часам");
                        stats.record_dropped();
                        return_buffer(&mut buffer_pool, pending.frame.bgra);
                        handled += 1;
                    }
                    FrameAction::Show => {
                        let lag = position.saturating_sub(pending.pts);
                        let PendingFrame { frame, pts } = pending;
                        match frames.try_send(TaggedFrame {
                            epoch: frame_epoch,
                            frame,
                        }) {
                            Ok(()) => {
                                last_shown_pts = Some(pts);
                                stats.record_shown(lag);
                                handled += 1;
                            }
                            // Очередь полна: вернём кадр и подождём UI.
                            Err(TrySendError::Full(tagged)) => {
                                pending_frame = Some(PendingFrame {
                                    frame: tagged.frame,
                                    pts,
                                });
                                break;
                            }
                            Err(TrySendError::Disconnected(_)) => return Ok(()),
                        }
                    }
                }
            }
        }

        if media.video.is_some() && !paused && stats.report_at.elapsed() >= SYNC_STATS_INTERVAL {
            let snapshot = stats.snapshot();
            debug!(
                shown = snapshot.shown,
                dropped = snapshot.dropped,
                fps = snapshot.fps,
                lag_ms = snapshot.lag.as_millis(),
                "состояние синхронизации"
            );
            let _ = events.send(Event::Stats(snapshot));
        }

        let video_done = media.video.is_none() || video_finished;

        if audio_done && video_done && !output_empty && !end_reported && !waiting_output_logged {
            waiting_output_logged = true;
            debug!("дороги декодированы, ждём опустошения очереди звука");
        }

        if audio_done && video_done && output_empty && !end_reported {
            end_reported = true;
            info!("файл доигран до конца");
            if media.video.is_some() {
                let _ = events.send(Event::Stats(stats.snapshot()));
            }
            // Столбики не должны замереть на последнем уровне.
            if spectrum.is_some() {
                let _ = events.send(Event::Spectrum(SpectrumBars::ZERO));
            }
            let _ = events.send(Event::Ended);
        }

        report_position(
            events,
            current_position(clock.as_ref(), video_timer_ref(&media, &video_timer)),
            &mut last_position,
            &mut last_position_at,
            false,
        );
    }
}

/// Отправляет позицию, если она изменилась и прошёл интервал.
///
/// `force` — для событий, где UI ждёт немедленного обновления (перемотка,
/// пауза), без оглядки на интервал. Без позиции (нет ни звука, ни видео)
/// событие не отправляется.
fn report_position(
    events: &Sender<Event>,
    position: Option<Duration>,
    last_position: &mut Duration,
    last_at: &mut Instant,
    force: bool,
) {
    let Some(position) = position else {
        return;
    };

    if !force && last_at.elapsed() < POSITION_UPDATE_INTERVAL {
        return;
    }

    if position == *last_position {
        return;
    }

    *last_position = position;
    *last_at = Instant::now();
    let _ = events.send(Event::PositionChanged(position));
}

/// Опустошает очередь звука и возвращает плеер в то же состояние.
fn restart(
    output: Option<&rodio::Player>,
    paused: bool,
    audio_finished: &mut bool,
    video_finished: &mut bool,
    end_reported: &mut bool,
) {
    if let Some(output) = output {
        output.stop();
        if paused {
            output.pause();
        } else {
            output.play();
        }
    }
    *audio_finished = false;
    *video_finished = false;
    *end_reported = false;
}

/// Кадр, уже декодированный, но ещё не отправленный в UI.
///
/// PTS вычисляется один раз при декодировании, чтобы оценка для кадров без
/// метки времени не двигалась на каждом тике.
struct PendingFrame {
    frame: VideoFrame,
    pts: Duration,
}

/// Накопительная статистика синхронизации.
struct SyncCounters {
    shown: u64,
    dropped: u64,
    lag: Duration,
    shown_since_report: u64,
    duration: Option<Duration>,
    started_at: Instant,
    report_at: Instant,
}

impl SyncCounters {
    fn new(duration: Option<Duration>) -> Self {
        let now = Instant::now();
        Self {
            shown: 0,
            dropped: 0,
            lag: Duration::ZERO,
            shown_since_report: 0,
            duration,
            started_at: now,
            report_at: now,
        }
    }

    fn record_shown(&mut self, lag: Duration) {
        self.shown = self.shown.saturating_add(1);
        self.shown_since_report = self.shown_since_report.saturating_add(1);
        self.lag = lag;
    }

    fn record_dropped(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Снимает срез для UI и начинает новый интервал измерения fps.
    fn snapshot(&mut self) -> SyncStats {
        let seconds = self.report_at.elapsed().as_secs_f64();
        let fps = if self.shown_since_report > 0 && seconds > 0.0 {
            self.shown_since_report as f64 / seconds
        } else {
            // В последнем интервале кадров не было (видео закончилось раньше
            // звука): отдаём среднее по длительности файла, а не ноль.
            let base = self.duration.map_or_else(
                || self.started_at.elapsed().as_secs_f64(),
                |duration| duration.as_secs_f64(),
            );
            if base > 0.0 {
                self.shown as f64 / base
            } else {
                0.0
            }
        };

        let snapshot = SyncStats {
            shown: self.shown,
            dropped: self.dropped,
            lag: self.lag,
            fps: fps as f32,
        };

        self.shown_since_report = 0;
        self.report_at = Instant::now();
        snapshot
    }
}

/// Позиция ведущих часов: звук, иначе запасной таймер видео.
fn video_position(clock: Option<&AudioClock>, timer: &VideoTimer) -> Duration {
    clock.map_or_else(|| timer.position(Instant::now()), AudioClock::position)
}

/// Текущая позиция для UI: аудио-часы, иначе таймер видео.
fn current_position(clock: Option<&AudioClock>, timer: Option<&VideoTimer>) -> Option<Duration> {
    match clock {
        Some(clock) => Some(clock.position()),
        None => timer.map(|timer| timer.position(Instant::now())),
    }
}

/// Запасной таймер нужен только для видео без звука.
fn video_timer_ref<'a>(media: &Media, timer: &'a VideoTimer) -> Option<&'a VideoTimer> {
    media.video.is_some().then_some(timer)
}

/// Анализатор создаётся только для файла без видео.
///
/// Поверх кадра визуализатор не показывается, а лишние события и FFT в цикле
/// ядра для видео ни к чему.
fn spectrum_analyzer(media: &Media) -> Option<SpectrumAnalyzer> {
    if media.video.is_some() {
        return None;
    }
    let audio = media.audio.as_ref()?;
    Some(SpectrumAnalyzer::new(audio.sample_rate()))
}

/// Сбрасывает визуализатор после открытия файла или перемотки.
///
/// Курсор переставляется на текущую позицию записи, поэтому уже накопленный
/// звук старой позиции в анализ не попадает.
fn reset_spectrum(
    analyzer: &mut Option<SpectrumAnalyzer>,
    sink: Option<&AudioSink>,
    cursor: &mut u64,
    events: &Sender<Event>,
) {
    let Some(analyzer) = analyzer.as_mut() else {
        return;
    };
    analyzer.reset();
    if let Some(sink) = sink {
        *cursor = sink.spectrum.latest();
    }
    let _ = events.send(Event::Spectrum(SpectrumBars::ZERO));
}

/// Источник звука, который считает отданные сэмплы.
///
/// rodio забирает сэмплы из источника в своём потоке вывода, поэтому этот
/// счётчик — честная мера того, что уже прозвучало: он не зависит от того,
/// насколько декодер убежал вперёд.
struct CountingSource {
    inner: SpectrumTap<SamplesBuffer>,
    counter: Arc<AtomicU64>,
}

impl Iterator for CountingSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;
        self.counter.fetch_add(1, Ordering::Relaxed);
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl Source for CountingSource {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

/// Превращает блок ядра в источник звука для rodio.
///
/// Тап ставится здесь, между буфером и счётчиком: он видит ровно те сэмплы,
/// которые rodio забирает на вывод, и потому столбики не убегают вперёд звука.
fn chunk_to_source(
    chunk: AudioChunk,
    counter: Arc<AtomicU64>,
    spectrum: Option<Arc<SpectrumRing>>,
) -> Result<CountingSource, PlayerError> {
    let invalid = || PlayerError::InvalidAudioParams {
        channels: chunk.channels,
        sample_rate: chunk.sample_rate,
    };

    let channels = NonZeroU16::new(chunk.channels).ok_or_else(invalid)?;
    let sample_rate = NonZeroU32::new(chunk.sample_rate).ok_or_else(invalid)?;

    Ok(CountingSource {
        inner: SpectrumTap::new(
            SamplesBuffer::new(channels, sample_rate, chunk.samples),
            spectrum,
        ),
        counter,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::test_support::{sample_av1_video, sample_video, temp_path};

    fn counting_source(samples: usize, counter: Arc<AtomicU64>) -> CountingSource {
        let channels = NonZeroU16::new(2).expect("два канала");
        let sample_rate = NonZeroU32::new(8_000).expect("частота");
        CountingSource {
            inner: SpectrumTap::new(
                SamplesBuffer::new(channels, sample_rate, vec![0.5_f32; samples]),
                None,
            ),
            counter,
        }
    }

    #[test]
    fn counting_source_counts_delivered_samples() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut source = counting_source(8, Arc::clone(&counter));

        assert_eq!(source.next(), Some(0.5));
        assert_eq!(source.next(), Some(0.5));
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        assert_eq!(source.count(), 6);
        assert_eq!(counter.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn exhausted_source_stops_counting() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut source = counting_source(2, Arc::clone(&counter));

        while source.next().is_some() {}
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        assert_eq!(source.next(), None);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn stale_frame_epochs_are_dropped() {
        let (sender, receiver) = bounded(8);
        let (pool_sender, _pool) = unbounded();
        let frame = |width: u32| VideoFrame {
            width,
            height: 1,
            bgra: vec![0; width as usize * 4],
            pts: None,
        };

        sender
            .send(TaggedFrame {
                epoch: 1,
                frame: frame(10),
            })
            .expect("положить старый кадр");
        sender
            .send(TaggedFrame {
                epoch: 2,
                frame: frame(20),
            })
            .expect("положить кадр текущего поколения");
        sender
            .send(TaggedFrame {
                epoch: 2,
                frame: frame(30),
            })
            .expect("положить свежий кадр");

        let latest = latest_frame(&receiver, 2, &pool_sender).expect("кадр текущего поколения");
        assert_eq!(latest.width, 30);

        // Очередь опустошена полностью, включая старые поколения.
        assert!(latest_frame(&receiver, 2, &pool_sender).is_none());
    }

    #[test]
    fn discarded_frames_return_their_buffers_to_the_pool() {
        let (sender, receiver) = bounded(8);
        let (pool_sender, pool) = unbounded();
        let frame = |width: u32| VideoFrame {
            width,
            height: 1,
            bgra: vec![0; width as usize * 4],
            pts: None,
        };

        sender
            .send(TaggedFrame {
                epoch: 1,
                frame: frame(10),
            })
            .expect("положить старый кадр");
        sender
            .send(TaggedFrame {
                epoch: 2,
                frame: frame(20),
            })
            .expect("положить вытесненный кадр");
        sender
            .send(TaggedFrame {
                epoch: 2,
                frame: frame(30),
            })
            .expect("положить свежий кадр");

        let latest = latest_frame(&receiver, 2, &pool_sender).expect("свежий кадр");
        assert_eq!(latest.width, 30);

        // Старое поколение и вытесненный кадр отдают буферы в пул.
        let returned: Vec<Vec<u8>> = pool.try_iter().collect();
        assert_eq!(
            returned.len(),
            2,
            "оба ненужных кадра должны вернуть буферы"
        );
        assert_eq!(returned[0].len(), 10 * 4);
        assert_eq!(returned[1].len(), 20 * 4);
    }

    #[test]
    fn pool_does_not_grow_past_its_limit() {
        let mut pool = Vec::new();
        for _ in 0..(POOL_LIMIT + 3) {
            return_buffer(&mut pool, Vec::with_capacity(8));
        }

        assert_eq!(pool.len(), POOL_LIMIT);
    }

    #[test]
    fn video_only_file_reports_track_metadata() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let player = Player::open(&path).expect("открыть видео без звука");
        let media = player.media_info();

        assert!(
            player.video_info().is_some(),
            "метаданные видео должны быть доступны"
        );
        let video = media.video.as_ref().expect("видеодорожка в MediaInfo");
        assert_eq!(
            (video.width, video.height),
            (
                player.video_info().expect("info").width,
                player.video_info().expect("info").height
            ),
            "размеры берутся из декодера, а не из заголовка"
        );
        assert!(
            video.codec.is_some(),
            "имя кодека должно читаться из заголовка"
        );
        assert!(media.container.is_some(), "контейнер должен определиться");
        assert!(
            media.audio.is_none(),
            "в video_only.mp4 звука нет — аудиодорожки быть не должно"
        );
    }

    #[test]
    fn video_only_file_delivers_frames() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        // Звука нет, поэтому ядро не должно требовать аудиоустройство.
        let player = Player::open(&path).expect("открыть видео без звука");
        assert!(
            player.video_info().is_some(),
            "метаданные видео должны быть доступны"
        );

        player
            .send(Command::Play)
            .expect("запустить воспроизведение");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut frame = None;
        while Instant::now() < deadline && frame.is_none() {
            frame = player.try_recv_frame();
            if frame.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let frame = frame.expect("кадр должен прийти за две секунды");
        let expected = usize::try_from(frame.width).expect("ширина")
            * usize::try_from(frame.height).expect("высота")
            * 4;
        assert_eq!(frame.bgra.len(), expected);
    }

    /// AV1 + Opus: звук заканчивается раньше видео. Оставшиеся кадры должны
    /// доиграть по запасному таймеру, а файл — дойти до `Ended`.
    #[test]
    fn av1_opus_file_reaches_end() {
        let Some(path) = sample_av1_video() else {
            eprintln!("нет assets/test/av1_test.webm — пропускаем проверку AV1+Opus");
            return;
        };

        let player = Player::open(&path).expect("открыть AV1+Opus");
        player
            .send(Command::Play)
            .expect("запустить воспроизведение");

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            match player.try_recv_event() {
                Some(Event::Ended) => return,
                Some(Event::Error(message)) => panic!("ошибка воспроизведения: {message}"),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(10)),
            }
            // Как UI: очередь кадров ограничена, её нужно забирать.
            let _ = player.try_recv_frame();
        }

        panic!("файл не дошёл до Ended за 15 секунд");
    }

    #[test]
    fn video_only_file_reports_position() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let player = Player::open(&path).expect("открыть видео без звука");
        player
            .send(Command::Play)
            .expect("запустить воспроизведение");

        // Для файла без звука позицию ведёт запасной таймер видео, и UI должен
        // получать `PositionChanged` так же, как с аудио-часами.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut position = Duration::ZERO;
        while Instant::now() < deadline && position.is_zero() {
            if let Some(Event::PositionChanged(value)) = player.try_recv_event() {
                position = value;
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        assert!(
            !position.is_zero(),
            "позиция должна идти и без звуковой дорожки"
        );
    }

    #[test]
    fn missing_file_is_reported() {
        let path = temp_path("mp4");
        let Err(error) = Player::open(&path) else {
            panic!("несуществующий файл должен давать ошибку");
        };

        assert!(
            matches!(error, PlayerError::OpenFile { .. }),
            "ожидалась ошибка открытия, получено: {error}"
        );
    }

    #[test]
    fn empty_file_is_reported() {
        let path = temp_path("mp4");
        std::fs::write(&path, []).expect("создать пустой файл");

        let result = Player::open(&path);
        let _ = std::fs::remove_file(&path);

        let Err(error) = result else {
            panic!("пустой файл должен давать ошибку");
        };
        assert!(
            matches!(error, PlayerError::EmptyFile { .. }),
            "ожидалась ошибка пустого файла, получено: {error}"
        );
    }

    #[test]
    fn garbage_file_is_reported_without_panic() {
        let path = temp_path("mp4");
        std::fs::write(&path, b"not a media file").expect("создать мусорный файл");

        let result = Player::open(&path);
        let _ = std::fs::remove_file(&path);

        let Err(error) = result else {
            panic!("мусорный файл должен давать ошибку");
        };
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn url_source_reports_a_clear_error_for_now() {
        let source = MediaSource::Url("https://example.com/video.mp4?token=secret".to_owned());
        let Err(error) = Player::open(source) else {
            panic!("URL пока не должен открываться");
        };

        assert!(matches!(error, PlayerError::UnsupportedSource { .. }));
        assert!(error.to_string().contains("?…"), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");
    }

    #[test]
    fn truncated_file_reports_error_event() {
        let Some(source) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let bytes = std::fs::read(&source).expect("прочитать тестовое видео");
        let path = temp_path("mp4");
        std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("записать обрезанный файл");

        // Обрезанный файл может не открыться сразу или сломаться посреди
        // декодирования. В обоих случаях ошибка должна дойти до вызывающего
        // кода, а не превратиться в панику.
        let result = Player::open(&path);
        let reported = match result {
            Err(error) => {
                assert!(!error.to_string().is_empty());
                true
            }
            Ok(player) => {
                player
                    .send(Command::Play)
                    .expect("запустить воспроизведение");
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut reported = false;
                while Instant::now() < deadline {
                    match player.try_recv_event() {
                        Some(Event::Error(message)) => {
                            assert!(!message.is_empty());
                            reported = true;
                            break;
                        }
                        Some(Event::Ended) => break,
                        Some(_) => {}
                        None => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
                reported
            }
        };

        let _ = std::fs::remove_file(&path);
        assert!(reported, "обрезанный файл должен сообщить об ошибке");
    }
}
