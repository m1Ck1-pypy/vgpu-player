//! Цикл потока ядра: команды, дозаправка очередей, конец файла, часы.
//!
//! Здесь сосредоточено всё, что живёт в потоке [`crate::Player`]: открытие
//! источника, вывод звука, синхронизация A/V, перемотка и визуализатор.
//! Модуль намеренно отделён от [`crate::player`], чтобы публичная ручка
//! управления и каналы не тонули в деталях цикла.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use rodio::buffer::SamplesBuffer;
use rodio::source::Source;
use rodio::{ChannelCount, MixerDeviceSink, Sample, SampleRate};
use tracing::{debug, error, info};

use crate::audio::AudioChunk;
use crate::clock::AudioClock;
use crate::media::Media;
use crate::player::{EngineMessage, TaggedFrame, return_buffer};
use crate::source::Source as MediaSource;
use crate::spectrum::{
    SAMPLE_RING_CAPACITY, SpectrumAnalyzer, SpectrumBars, SpectrumRing, SpectrumTap,
};
use crate::sync::{FrameAction, FrameScheduler, NetworkStats, SyncStats, VideoTimer};
use crate::video::VideoFrame;
use crate::{Command, Event, PlayerError};

/// Как часто поток ядра просыпается без команд, чтобы добрать звук в очередь.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Сколько блоков звука можно держать в очереди вывода.
///
/// Ограничение нужно и для звука: без него декодер вычитает файл целиком
/// вперёд, и память растёт вместе с длительностью ролика.
const QUEUED_CHUNKS: usize = 4;

/// Для сети очередь больше: блокирующее чтение видео не должно опустошать звук.
const QUEUED_CHUNKS_NETWORK: usize = 32;

/// Сколько должна длиться пауза чтения, чтобы показать плашку буферизации.
const BUFFERING_DELAY: Duration = Duration::from_millis(200);

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

/// Как ядро открывает источник.
///
/// В обычной жизни это [`Media::open_with`]; тесты подменяют открытие, чтобы
/// получить «медленный» источник без сети и без новых файлов.
pub(crate) type Opener = Box<
    dyn Fn(&MediaSource, Arc<AtomicBool>, &dyn Fn() -> bool) -> Result<Media, PlayerError> + Send,
>;

/// Состояние и цикл одного потока ядра.
struct Engine<'a> {
    source: MediaSource,
    opener: Opener,
    /// Взводится при `Stop`, новом `Open` и уничтожении [`crate::Player`].
    cancel: Arc<AtomicBool>,
    /// Номер текущего открытия: новый `Open` отменяет предыдущее.
    open_seq: Arc<AtomicU64>,
    media: Option<Media>,
    output: Option<AudioSink>,
    clock: Option<AudioClock>,
    spectrum: Option<SpectrumAnalyzer>,
    spectrum_cursor: u64,
    spectrum_samples: Vec<f32>,
    last_spectrum_at: Instant,
    volume: f32,
    paused: bool,
    audio_finished: bool,
    video_finished: bool,
    end_reported: bool,
    last_position: Duration,
    last_position_at: Instant,
    scheduler: FrameScheduler,
    video_timer: VideoTimer,
    pending_frame: Option<PendingFrame>,
    last_shown_pts: Option<Duration>,
    tail_started: bool,
    stats: SyncCounters,
    frame_epoch: u64,
    waiting_output_logged: bool,
    audio_chunks: u64,
    buffer_pool: Vec<Vec<u8>>,
    buffering: BufferingWatch,
    events: &'a Sender<Event>,
    frames: &'a Sender<TaggedFrame>,
    recycled: &'a Receiver<Vec<u8>>,
}

/// Каналы связи ядра с `Player`: команды, события, кадры и возврат буферов.
pub(crate) struct EngineChannels<'a> {
    pub(crate) commands: &'a Receiver<EngineMessage>,
    pub(crate) events: &'a Sender<Event>,
    pub(crate) frames: &'a Sender<TaggedFrame>,
    pub(crate) recycled: &'a Receiver<Vec<u8>>,
}

/// Цикл потока ядра: команды, дозаправка очередей, конец файла, часы.
pub(crate) fn run_engine(
    source: MediaSource,
    opener: Opener,
    cancel: Arc<AtomicBool>,
    open_seq: Arc<AtomicU64>,
    channels: EngineChannels<'_>,
) -> Result<(), PlayerError> {
    let commands = channels.commands;
    let mut engine = Engine::new(source, opener, cancel, open_seq, channels);

    // Открытие не блокирует вызывающего: код `Player::open` уже вернулся.
    engine.open_current();

    loop {
        match commands.recv_timeout(POLL_INTERVAL) {
            Ok(message) => {
                // Если в очереди уже есть более новый `Open`, промежуточные
                // открытия не нужны: пользователь успел сменить источник.
                let mut messages = vec![message];
                while messages.len() < 64 {
                    let Ok(next) = commands.try_recv() else {
                        break;
                    };
                    messages.push(next);
                }
                let messages = coalesce_seeks(messages);
                let last_open = messages
                    .iter()
                    .rposition(|message| matches!(message.command, Command::Open(_)));

                for (index, message) in messages.into_iter().enumerate() {
                    if matches!(message.command, Command::Open(_)) && Some(index) != last_open {
                        debug!("открытие пропущено: в очереди есть более новый источник");
                        continue;
                    }
                    if engine.handle(message)? {
                        return Ok(());
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        engine.tick()?;
    }
}

/// Склеивает подряд идущие относительные перемотки одного направления.
///
/// Удержание `←`/`→` присылает десятки команд. Если выполнять их по одной,
/// картинка отстаёт от UI и «догоняет» шагами. Сумма шагов даёт ту же
/// конечную позицию одним seek. Разные направления не смешиваем: на границах
/// файла промежуточный клип меняет результат.
fn coalesce_seeks(messages: Vec<EngineMessage>) -> Vec<EngineMessage> {
    let mut merged = Vec::with_capacity(messages.len());
    // Направление, суммарный шаг в миллисекундах и эпоха кадров.
    let mut run: Option<(bool, u64, Option<u64>)> = None;

    for message in messages {
        match message.command {
            Command::SeekRelative { forward, step } => {
                let step = u64::try_from(step.as_millis()).unwrap_or(u64::MAX);
                if step == 0 {
                    continue;
                }
                match &mut run {
                    Some((run_forward, total, epoch)) if *run_forward == forward => {
                        *total = total.saturating_add(step);
                        *epoch = message.epoch;
                    }
                    Some(_) => {
                        flush_seek_run(&mut merged, &mut run);
                        run = Some((forward, step, message.epoch));
                    }
                    None => run = Some((forward, step, message.epoch)),
                }
            }
            command => {
                flush_seek_run(&mut merged, &mut run);
                merged.push(EngineMessage {
                    command,
                    epoch: message.epoch,
                });
            }
        }
    }
    flush_seek_run(&mut merged, &mut run);
    merged
}

/// Добавляет накопленный шаг в список.
fn flush_seek_run(merged: &mut Vec<EngineMessage>, run: &mut Option<(bool, u64, Option<u64>)>) {
    let Some((forward, step, epoch)) = run.take() else {
        return;
    };
    merged.push(EngineMessage {
        command: Command::SeekRelative {
            forward,
            step: Duration::from_millis(step),
        },
        epoch,
    });
}

impl<'a> Engine<'a> {
    fn new(
        source: MediaSource,
        opener: Opener,
        cancel: Arc<AtomicBool>,
        open_seq: Arc<AtomicU64>,
        channels: EngineChannels<'a>,
    ) -> Self {
        let EngineChannels {
            events,
            frames,
            recycled,
            ..
        } = channels;
        Self {
            source,
            opener,
            cancel,
            open_seq,
            media: None,
            output: None,
            clock: None,
            spectrum: None,
            spectrum_cursor: 0,
            spectrum_samples: Vec::new(),
            last_spectrum_at: Instant::now(),
            // Стартуем на паузе: воспроизведение начинается по команде `Play`.
            volume: 1.0,
            paused: true,
            audio_finished: true,
            video_finished: true,
            end_reported: false,
            last_position: Duration::ZERO,
            last_position_at: Instant::now(),
            scheduler: FrameScheduler::new(None),
            video_timer: VideoTimer::new(),
            pending_frame: None,
            last_shown_pts: None,
            tail_started: false,
            stats: SyncCounters::new(None),
            frame_epoch: 0,
            waiting_output_logged: false,
            audio_chunks: 0,
            buffer_pool: Vec::new(),
            buffering: BufferingWatch::default(),
            events,
            frames,
            recycled,
        }
    }

    /// Открывает текущий источник и сообщает результат событием.
    ///
    /// Ошибка не завершает поток: ядро остаётся ждать `Open`/`Stop`.
    fn open_current(&mut self) {
        self.reset_for_open();
        debug!(source = %self.source, "открытие источника");

        let seq = self.open_seq.load(Ordering::SeqCst);
        let cancelled =
            || self.cancel.load(Ordering::SeqCst) || self.open_seq.load(Ordering::SeqCst) != seq;

        match (self.opener)(&self.source, Arc::clone(&self.cancel), &cancelled) {
            Ok(opened) => self.apply(opened),
            Err(PlayerError::Cancelled) => debug!("открытие отменено"),
            Err(error) => {
                error!(source = %self.source, %error, "источник не открылся");
                let _ = self.events.send(Event::Error(error.to_string()));
            }
        }
    }

    /// Готовит состояние к новой попытке: старые декодеры и кадры не нужны.
    fn reset_for_open(&mut self) {
        self.media = None;
        self.clock = None;
        self.spectrum = None;
        self.pending_frame = None;
        self.buffer_pool.clear();
        self.tail_started = false;
        self.last_shown_pts = None;
        self.end_reported = false;
        self.audio_finished = true;
        self.video_finished = true;
        self.waiting_output_logged = false;
        self.scheduler = FrameScheduler::new(None);
        self.video_timer = VideoTimer::new();
        self.stats = SyncCounters::new(None);
        self.buffering.reset();
        if let Some(sink) = self.output.as_ref() {
            sink.player.stop();
            sink.player.pause();
        }
    }

    /// Принимает открытый источник: устройство, часы, событие `Ready`.
    fn apply(&mut self, opened: Media) {
        // Звуковое устройство открываем лениво. Отказ устройства — событие,
        // а не конец потока: видео должно доиграть и без него.
        if opened.audio.is_some() && self.output.is_none() {
            match AudioSink::open(self.volume) {
                Ok(sink) => self.output = Some(sink),
                Err(error) => {
                    error!(%error, "звуковое устройство недоступно");
                    let _ = self.events.send(Event::Error(error.to_string()));
                }
            }
        }

        self.clock = opened
            .audio
            .as_ref()
            .map(|audio| AudioClock::new(audio.channels(), audio.sample_rate()));
        self.scheduler = FrameScheduler::new(opened.video_info().and_then(|info| info.frame_rate));
        self.spectrum = spectrum_analyzer(&opened);
        self.stats = SyncCounters::new(opened.duration());
        self.audio_finished = opened.audio.is_none();
        self.video_finished = opened.video.is_none();
        self.end_reported = false;
        self.waiting_output_logged = false;
        self.audio_chunks = 0;
        self.tail_started = false;
        self.last_shown_pts = None;
        self.pending_frame = None;
        self.buffer_pool.clear();
        self.video_timer = VideoTimer::new();
        if !self.paused {
            self.video_timer.start(Instant::now());
        }

        self.media = Some(opened);

        if let Some(sink) = self.output.as_ref() {
            sink.player.stop();
            if self.paused {
                sink.player.pause();
            } else {
                sink.player.play();
            }
        }

        reset_spectrum(
            &mut self.spectrum,
            self.output.as_ref(),
            &mut self.spectrum_cursor,
            self.events,
        );
        self.last_spectrum_at = Instant::now();

        if let Some(media) = self.media.as_ref() {
            let ready = media.ready(&self.source);
            let _ = self.events.send(Event::Ready(Box::new(ready)));
        }
        info!(source = %self.source, "источник готов");
        self.report(true);
    }

    /// Обрабатывает одну команду. `true` — поток пора завершить.
    fn handle(&mut self, message: EngineMessage) -> Result<bool, PlayerError> {
        let EngineMessage { command, epoch } = message;

        match command {
            Command::Play => {
                if self.media.is_none() {
                    debug!("Play без открытого источника");
                    return Ok(false);
                }
                self.paused = false;
                self.video_timer.start(Instant::now());
                if let Some(sink) = self.output.as_ref() {
                    sink.player.play();
                }
            }
            Command::Pause => {
                if self.media.is_none() {
                    debug!("Pause без открытого источника");
                    return Ok(false);
                }
                self.paused = true;
                self.video_timer.pause(Instant::now());
                if let Some(sink) = self.output.as_ref() {
                    sink.player.pause();
                }
                // Отдать финальную позицию, чтобы UI не замирал на предыдущем
                // тике часов.
                self.report(true);
            }
            Command::SetVolume(value) => {
                if value.is_finite() {
                    self.volume = value.clamp(0.0, MAX_VOLUME);
                    if let Some(sink) = self.output.as_ref() {
                        sink.player.set_volume(self.volume);
                    }
                }
            }
            Command::Seek(position) => {
                if let Some(epoch) = epoch {
                    self.frame_epoch = epoch;
                }
                if self.media.is_none() {
                    debug!("Seek без открытого источника");
                    return Ok(false);
                }
                if !self.media.as_ref().is_some_and(Media::is_seekable) {
                    debug!("Seek без поддержки перемотки");
                    return Ok(false);
                }
                self.seek(position)?;
            }
            Command::SeekRelative { forward, step } => {
                if let Some(epoch) = epoch {
                    self.frame_epoch = epoch;
                }
                if self.media.is_none() {
                    debug!("SeekRelative без открытого источника");
                    return Ok(false);
                }
                if !self.media.as_ref().is_some_and(Media::is_seekable) {
                    debug!("SeekRelative без поддержки перемотки");
                    return Ok(false);
                }
                let current = current_position(
                    self.clock.as_ref(),
                    video_timer_ref(self.media.as_ref(), &self.video_timer),
                );
                let Some(current) = current else {
                    debug!("SeekRelative без текущей позиции");
                    return Ok(false);
                };
                let target = if forward {
                    current.saturating_add(step)
                } else {
                    current.saturating_sub(step)
                };
                let target = self
                    .media
                    .as_ref()
                    .and_then(Media::duration)
                    .map_or(target, |duration| target.min(duration));
                self.seek(target)?;
            }
            Command::Open(source) => {
                if let Some(epoch) = epoch {
                    self.frame_epoch = epoch;
                }
                info!(source = %source, "открытие нового источника");
                self.source = source;
                self.open_current();
            }
            Command::Stop => {
                if let Some(sink) = self.output.as_ref() {
                    sink.player.stop();
                }
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Перемотка внутри открытого источника.
    fn seek(&mut self, position: Duration) -> Result<(), PlayerError> {
        info!(position = ?position, "перемотка");
        self.tail_started = false;
        self.last_shown_pts = None;
        // Точная перемотка видео может занять время: останавливаем звук,
        // чтобы часы не убежали вперёд, пока декодер догоняет целевой кадр.
        if let Some(sink) = self.output.as_ref() {
            sink.player.pause();
        }
        if let Some(video) = self.media.as_mut().and_then(|media| media.video.as_mut()) {
            video.seek(position)?;
        }
        if let Some(audio) = self.media.as_mut().and_then(|media| media.audio.as_mut()) {
            audio.seek(position)?;
        }
        if let Some(clock) = self.clock.as_mut() {
            clock.rebase(position);
        }
        // Кадры от старой позиции больше не нужны.
        if let Some(pending) = self.pending_frame.take() {
            return_buffer(&mut self.buffer_pool, pending.frame.bgra);
        }
        self.scheduler.reset();
        self.video_timer.rebase(position, Instant::now());
        self.restart_playback();
        // Старый звук в кольце к новой позиции не относится.
        reset_spectrum(
            &mut self.spectrum,
            self.output.as_ref(),
            &mut self.spectrum_cursor,
            self.events,
        );
        self.last_spectrum_at = Instant::now();
        self.report(true);
        let _ = self.events.send(Event::Seeked(position));
        Ok(())
    }

    /// Опустошает очередь звука и возвращает состояние после перемотки.
    fn restart_playback(&mut self) {
        if let Some(sink) = self.output.as_ref() {
            sink.player.stop();
            if self.paused {
                sink.player.pause();
            } else {
                sink.player.play();
            }
        }
        self.audio_finished = false;
        self.video_finished = false;
        self.end_reported = false;
        self.waiting_output_logged = false;
    }

    /// Один тик цикла: звук, спектр, кадры, конец источника, позиция.
    fn tick(&mut self) -> Result<(), PlayerError> {
        // Забираем буферы, которые вернул UI: следующий кадр запишется в один
        // из них без выделения памяти на 8 МБ.
        while let Ok(buffer) = self.recycled.try_recv() {
            return_buffer(&mut self.buffer_pool, buffer);
        }

        let Some(media) = self.media.as_mut() else {
            return Ok(());
        };

        if let Some(buffering) = self.buffering.update(media.is_buffering(), Instant::now()) {
            let _ = self.events.send(Event::Buffering(buffering));
        }

        if let (Some(audio), Some(sink), Some(clock)) = (
            media.audio.as_mut(),
            self.output.as_ref(),
            self.clock.as_ref(),
        ) {
            let spectrum_tap = self.spectrum.as_ref().map(|_| Arc::clone(&sink.spectrum));
            let queued_chunks = queued_chunks(self.source.is_network());
            while !self.audio_finished && sink.player.len() < queued_chunks {
                match audio.next_chunk()? {
                    Some(chunk) => {
                        self.audio_chunks = self.audio_chunks.saturating_add(1);
                        if self.audio_chunks == 1 || self.audio_chunks.is_multiple_of(200) {
                            let peak = chunk
                                .samples
                                .iter()
                                .fold(0.0_f32, |max, sample| max.max(sample.abs()));
                            debug!(
                                chunks = self.audio_chunks,
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
                        self.audio_finished = true;
                    }
                }
            }
        }

        // Свежие сэмплы превращаем в столбики не чаще `SPECTRUM_INTERVAL`:
        // декодер отдаёт блоки быстрее, чем UI успевает их нарисовать.
        if let (Some(analyzer), Some(sink)) = (self.spectrum.as_mut(), self.output.as_ref()) {
            if sink
                .spectrum
                .read(&mut self.spectrum_cursor, &mut self.spectrum_samples)
                > 0
            {
                analyzer.push(&self.spectrum_samples);
            }
            if !self.paused && self.last_spectrum_at.elapsed() >= SPECTRUM_INTERVAL {
                let _ = self.events.send(Event::Spectrum(analyzer.update()));
                self.last_spectrum_at = Instant::now();
            }
        }

        // Состояние дорожек нужно и для пейсинга, и для проверки конца файла.
        let audio_done = media.audio.is_none() || self.audio_finished;
        let output_empty = self.output.as_ref().is_none_or(|sink| sink.player.empty());
        // Пока звук играет или ещё стоит в очереди, ведут аудио-часы. После
        // конца звука часы больше не идут: если оставить их ведущими, последние
        // кадры видео с большим PTS будут вечно ждать своей позиции.
        let audio_clock_leads = !audio_done || !output_empty;

        if !audio_clock_leads && media.video.is_some() && !self.tail_started {
            // Перебазируем таймер на последний показанный кадр, а не на
            // «текущее время»: иначе оставшиеся кадры разом опоздают и будут
            // пропущены вместо того, чтобы доиграть.
            let resume = self.last_shown_pts.unwrap_or(Duration::ZERO);
            self.video_timer.rebase(resume, Instant::now());
            self.tail_started = true;
            debug!(?resume, "звук закончился: видео доигрывает по таймеру");
        }

        // Если UI не успевает забирать кадры, не декодируем и не пропускаем
        // следующие: иначе при полной очереди начинается каскад пропусков.
        if !self.paused
            && !self.frames.is_full()
            && let Some(video) = media.video.as_mut()
        {
            let mut handled = 0_usize;
            while !self.video_finished && handled < MAX_FRAMES_PER_TICK {
                if self.pending_frame.is_none() {
                    match video.next_frame_with_buffer(self.buffer_pool.pop())? {
                        Some(frame) => {
                            let pts = self.scheduler.frame_pts(frame.pts);
                            self.pending_frame = Some(PendingFrame { frame, pts });
                        }
                        None => {
                            debug!("видео закончилось");
                            self.video_finished = true;
                            break;
                        }
                    }
                }

                let Some(pending) = self.pending_frame.take() else {
                    break;
                };
                let position = if audio_clock_leads {
                    video_position(self.clock.as_ref(), &self.video_timer)
                } else {
                    self.video_timer.position(Instant::now())
                };

                match FrameScheduler::decide(pending.pts, position) {
                    // Ещё рано: оставляем кадр до следующего тика.
                    FrameAction::Wait => {
                        self.pending_frame = Some(pending);
                        break;
                    }
                    // Опоздал: догоняем звук, кадр не показываем.
                    FrameAction::Skip => {
                        debug!(pts = ?pending.pts, "кадр пропущен: опоздал к часам");
                        self.stats.record_dropped();
                        return_buffer(&mut self.buffer_pool, pending.frame.bgra);
                        handled += 1;
                    }
                    FrameAction::Show => {
                        let lag = position.saturating_sub(pending.pts);
                        let PendingFrame { frame, pts } = pending;
                        match self.frames.try_send(TaggedFrame {
                            epoch: self.frame_epoch,
                            frame,
                        }) {
                            Ok(()) => {
                                self.last_shown_pts = Some(pts);
                                self.stats.record_shown(lag);
                                handled += 1;
                            }
                            // Очередь полна: вернём кадр и подождём UI.
                            Err(TrySendError::Full(tagged)) => {
                                self.pending_frame = Some(PendingFrame {
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

        if media.video.is_some()
            && !self.paused
            && self.stats.report_at.elapsed() >= SYNC_STATS_INTERVAL
        {
            let snapshot = self.stats.snapshot(media.network_stats());
            // Счётчики сети — только у URL: по ним видно, откуда `drop`.
            let network = snapshot.network;
            debug!(
                shown = snapshot.shown,
                dropped = snapshot.dropped,
                fps = snapshot.fps,
                lag_ms = snapshot.lag.as_millis(),
                requests = network.map(|stats| stats.requests),
                bytes = network.map(|stats| stats.bytes),
                slow_reads = network.map(|stats| stats.slow_reads),
                slow_ms = network.map(|stats| stats.slow_ms),
                retries = network.map(|stats| stats.retries),
                cache_bytes = network.map(|stats| stats.cache_bytes),
                "состояние синхронизации"
            );
            let _ = self.events.send(Event::Stats(snapshot));
        }

        let video_done = media.video.is_none() || self.video_finished;
        // Звук есть, а устройства нет: ошибка уже отправлена, `Ended` врал бы.
        let audio_blocked = media.audio.is_some() && self.output.is_none();

        if audio_done
            && video_done
            && output_empty
            && !self.end_reported
            && !self.waiting_output_logged
        {
            self.waiting_output_logged = true;
            debug!("дороги декодированы, ждём опустошения очереди звука");
        }

        if audio_done && video_done && output_empty && !self.end_reported && !audio_blocked {
            self.end_reported = true;
            info!("файл доигран до конца");
            if media.video.is_some() {
                let snapshot = self.stats.snapshot(media.network_stats());
                let _ = self.events.send(Event::Stats(snapshot));
            }
            // Столбики не должны замереть на последнем уровне.
            if self.spectrum.is_some() {
                let _ = self.events.send(Event::Spectrum(SpectrumBars::ZERO));
            }
            let _ = self.events.send(Event::Ended);
        }

        self.report(false);
        Ok(())
    }

    /// Отправляет текущую позицию в UI.
    fn report(&mut self, force: bool) {
        let position = current_position(
            self.clock.as_ref(),
            video_timer_ref(self.media.as_ref(), &self.video_timer),
        );
        report_position(
            self.events,
            position,
            &mut self.last_position,
            &mut self.last_position_at,
            force,
        );
    }
}

/// Сглаживает флаг буферизации: короткие паузы плашку не показывают.
#[derive(Debug, Default)]
struct BufferingWatch {
    since: Option<Instant>,
    shown: bool,
}

impl BufferingWatch {
    fn reset(&mut self) {
        self.since = None;
        self.shown = false;
    }

    /// Возвращает новое состояние плашки, если его нужно отправить в UI.
    fn update(&mut self, buffering: bool, now: Instant) -> Option<bool> {
        if buffering {
            if self.shown {
                return None;
            }
            let since = *self.since.get_or_insert(now);
            if now.saturating_duration_since(since) >= BUFFERING_DELAY {
                self.shown = true;
                Some(true)
            } else {
                None
            }
        } else {
            self.since = None;
            if self.shown {
                self.shown = false;
                Some(false)
            } else {
                None
            }
        }
    }
}

/// Размер очереди звука: для сети больше, чтобы видео не опустошало её.
fn queued_chunks(network: bool) -> usize {
    if network {
        QUEUED_CHUNKS_NETWORK
    } else {
        QUEUED_CHUNKS
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
    ///
    /// `network` — счётчики читателей: у файла их нет, у URL есть.
    fn snapshot(&mut self, network: Option<NetworkStats>) -> SyncStats {
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
            network,
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

/// Запасной таймер нужен только для открытого видео.
fn video_timer_ref<'a>(media: Option<&Media>, timer: &'a VideoTimer) -> Option<&'a VideoTimer> {
    media
        .is_some_and(|media| media.video.is_some())
        .then_some(timer)
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
    fn relative_seek_runs_are_coalesced() {
        let seek = |forward, seconds, epoch| EngineMessage {
            command: Command::SeekRelative {
                forward,
                step: Duration::from_secs(seconds),
            },
            epoch: Some(epoch),
        };

        let merged = coalesce_seeks(vec![
            seek(true, 5, 1),
            seek(true, 5, 2),
            seek(false, 3, 3),
            EngineMessage {
                command: Command::Pause,
                epoch: None,
            },
            seek(false, 2, 4),
        ]);

        assert_eq!(merged.len(), 4);
        assert_eq!(
            merged[0].command,
            Command::SeekRelative {
                forward: true,
                step: Duration::from_secs(10),
            }
        );
        assert_eq!(merged[0].epoch, Some(2));
        assert_eq!(
            merged[1].command,
            Command::SeekRelative {
                forward: false,
                step: Duration::from_secs(3),
            }
        );
        assert_eq!(merged[1].epoch, Some(3));
        assert_eq!(merged[2].command, Command::Pause);
        assert_eq!(
            merged[3].command,
            Command::SeekRelative {
                forward: false,
                step: Duration::from_secs(2),
            }
        );
    }

    #[test]
    fn opposite_relative_seeks_are_not_merged() {
        let seek = |forward| EngineMessage {
            command: Command::SeekRelative {
                forward,
                step: Duration::from_secs(5),
            },
            epoch: Some(1),
        };

        let merged = coalesce_seeks(vec![seek(true), seek(false)]);
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged[0].command,
            Command::SeekRelative {
                forward: true,
                step: Duration::from_secs(5),
            }
        );
        assert_eq!(
            merged[1].command,
            Command::SeekRelative {
                forward: false,
                step: Duration::from_secs(5),
            }
        );
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
    fn buffering_waits_for_the_delay_before_showing() {
        let start = Instant::now();
        let mut watch = BufferingWatch::default();

        assert_eq!(watch.update(true, start), None);
        assert_eq!(watch.update(true, start + Duration::from_millis(199)), None);
        assert_eq!(watch.update(true, start + BUFFERING_DELAY), Some(true));
        assert_eq!(
            watch.update(true, start + Duration::from_secs(1)),
            None,
            "пока буферизация идёт, повторно не шлём"
        );
        assert_eq!(
            watch.update(false, start + Duration::from_secs(1)),
            Some(false)
        );
        assert_eq!(watch.update(false, start + Duration::from_secs(2)), None);
    }

    #[test]
    fn short_buffering_does_not_flash() {
        let start = Instant::now();
        let mut watch = BufferingWatch::default();

        assert_eq!(watch.update(true, start), None);
        assert_eq!(
            watch.update(false, start + Duration::from_millis(100)),
            None,
            "короткая пауза не должна показывать плашку"
        );
    }

    #[test]
    fn network_audio_queue_is_larger() {
        assert_eq!(queued_chunks(false), QUEUED_CHUNKS);
        assert_eq!(queued_chunks(true), QUEUED_CHUNKS_NETWORK);
        const { assert!(QUEUED_CHUNKS_NETWORK > QUEUED_CHUNKS) };
    }
}
