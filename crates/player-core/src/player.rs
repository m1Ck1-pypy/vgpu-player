//! Ручка управления ядром и каналы связи с ним.
//!
//! Разделение простое: вызывающая сторона получает [`Player`] — только каналы.
//! Всё состояние (демультиплексоры, декодеры, звуковое устройство) живёт в
//! отдельном потоке, поэтому UI не блокируется на декодировании и IO. Сам цикл
//! потока — в [`crate::engine`].
//!
//! Видео и звук необязательны по отдельности: файл может содержать только
//! одну из дорожек. Кадры идут в отдельный ограниченный канал, события — в
//! неограниченный.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use tracing::error;

use crate::engine::{EngineChannels, Opener, run_engine};
use crate::media::Media;
use crate::source::Source as MediaSource;
use crate::video::VideoFrame;
use crate::{Command, Event, PlayerError};

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
    /// Взводится при уничтожении: открытие и декодирование должны отмениться.
    cancel: Arc<AtomicBool>,
    /// Номер открытия: `Open` отменяет текущую попытку.
    open_seq: Arc<AtomicU64>,
}

impl Player {
    /// Запускает поток ядра и сразу возвращается.
    ///
    /// Результат открытия приходит событием [`Event::Ready`] или
    /// [`Event::Error`]; ошибка `Result` означает только, что поток не удалось
    /// запустить. Ошибка открытия не завершает поток: ядро продолжает
    /// принимать `Open` и `Stop`.
    pub fn open(source: impl Into<MediaSource>) -> Result<Self, PlayerError> {
        Self::spawn(
            source.into(),
            Box::new(|source, cancel, cancelled| Media::open_with(source, cancel, cancelled)),
        )
    }

    /// Открывает источник с подменённым opener'ом для тестов.
    #[cfg(test)]
    pub(crate) fn open_with(
        source: impl Into<MediaSource>,
        opener: impl Fn(&MediaSource, &dyn Fn() -> bool) -> Result<Media, PlayerError> + Send + 'static,
    ) -> Result<Self, PlayerError> {
        Self::spawn(
            source.into(),
            Box::new(move |source, _cancel, cancelled| opener(source, cancelled)),
        )
    }

    fn spawn(source: MediaSource, opener: Opener) -> Result<Self, PlayerError> {
        let (command_sender, command_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let (frame_sender, frame_receiver) = bounded(QUEUED_FRAMES);
        let (recycled_sender, recycled_receiver) = unbounded();

        let cancel = Arc::new(AtomicBool::new(false));
        let open_seq = Arc::new(AtomicU64::new(0));
        let engine_cancel = Arc::clone(&cancel);
        let engine_seq = Arc::clone(&open_seq);

        std::thread::Builder::new()
            .name("player-engine".to_owned())
            .spawn(move || {
                if let Err(error) = run_engine(
                    source,
                    opener,
                    engine_cancel,
                    engine_seq,
                    EngineChannels {
                        commands: &command_receiver,
                        events: &event_sender,
                        frames: &frame_sender,
                        recycled: &recycled_receiver,
                    },
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
            cancel,
            open_seq,
        })
    }

    /// Передаёт команду ядру.
    ///
    /// `Seek` и `Open` увеличивают поколение кадров: всё, что ядро успело
    /// отправить до обработки команды, будет отброшено в [`Self::try_recv_frame`].
    /// `Open` ещё и отменяет незавершённое открытие, а `Stop` взводит флаг
    /// отмены до отправки: открытие может быть блокирующим.
    pub fn send(&self, command: Command) -> Result<(), PlayerError> {
        if matches!(&command, Command::Stop) {
            self.cancel.store(true, Ordering::SeqCst);
        }

        let epoch = match &command {
            Command::Seek(_) => Some(self.frame_epoch.fetch_add(1, Ordering::Relaxed) + 1),
            Command::Open(_) => {
                self.open_seq.fetch_add(1, Ordering::SeqCst);
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
}

impl Drop for Player {
    fn drop(&mut self) {
        // Открытие может быть блокирующим: без флага поток не узнал бы о
        // закрытии окна до конца чтения.
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Сообщение в поток ядра.
///
/// Для команд, меняющих позицию, указывается поколение кадров: ядро помечает
/// им всё, что отправит после обработки команды. Публичный [`Command`] при
/// этом не меняется.
pub(crate) struct EngineMessage {
    pub(crate) command: Command,
    pub(crate) epoch: Option<u64>,
}

/// Кадр вместе с номером поколения.
pub(crate) struct TaggedFrame {
    pub(crate) epoch: u64,
    pub(crate) frame: VideoFrame,
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
pub(crate) fn return_buffer(pool: &mut Vec<Vec<u8>>, buffer: Vec<u8>) {
    if pool.len() < POOL_LIMIT {
        pool.push(buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use crate::MediaReady;
    use crate::video::test_support::{
        sample_audio, sample_av, sample_av1_video, sample_clip, sample_video, temp_path,
    };

    /// Видеоклип 0.5 с для матрицы контейнеров (MP4, MKV).
    const VIDEO_CLIP: &[&str] = &[
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
    ];

    /// Тот же клип в WebM (нужен кодек VP8/VP9).
    const WEBM_CLIP: &[&str] = &[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=64x48:rate=30",
        "-t",
        "0.5",
        "-c:v",
        "libvpx",
        "-b:v",
        "200k",
    ];

    /// Звук 0.5 с в MP3.
    const MP3_CLIP: &[&str] = &[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=44100",
        "-t",
        "0.5",
        "-c:a",
        "libmp3lame",
        "-q:a",
        "9",
    ];

    /// Ждёт событие, удовлетворяющее условию, и забирает его из канала.
    fn wait_event(
        player: &Player,
        timeout: Duration,
        mut predicate: impl FnMut(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while let Some(event) = player.try_recv_event() {
                if predicate(&event) {
                    return Some(event);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// Ждёт `Ready` и отдаёт содержимое.
    fn wait_ready(player: &Player) -> MediaReady {
        match wait_event(player, Duration::from_secs(5), |event| {
            matches!(event, Event::Ready(_))
        }) {
            Some(Event::Ready(ready)) => *ready,
            _ => panic!("Ready не пришёл за пять секунд"),
        }
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

        let player = Player::open(&path).expect("запустить ядро");
        let ready = wait_ready(&player);

        assert!(ready.video_info.is_some(), "метаданные видео");
        let video = ready.media_info.video.as_ref().expect("видеодорожка");
        let info = ready.video_info.expect("параметры видео");
        assert_eq!(
            (video.width, video.height),
            (info.width, info.height),
            "размеры берутся из декодера, а не из заголовка"
        );
        assert!(
            video.codec.is_some(),
            "имя кодека должно читаться из заголовка"
        );
        assert!(
            ready.media_info.container.is_some(),
            "контейнер должен определиться"
        );
        assert!(
            ready.media_info.audio.is_none(),
            "в video_only.mp4 звука нет — аудиодорожки быть не должно"
        );
        assert!(ready.seekable, "локальный файл перематывается");
        assert!(!ready.live, "файл не является live-потоком");
    }

    #[test]
    fn video_only_file_delivers_frames() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        // Звука нет, поэтому ядро не должно требовать аудиоустройство.
        let player = Player::open(&path).expect("запустить ядро");
        wait_ready(&player);

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

        let player = Player::open(&path).expect("запустить ядро");
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

        let player = Player::open(&path).expect("запустить ядро");
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
        let player = Player::open(&path).expect("поток должен запуститься");

        let Some(Event::Error(message)) = wait_event(&player, Duration::from_secs(5), |event| {
            matches!(event, Event::Error(_))
        }) else {
            panic!("несуществующий файл должен давать Error");
        };

        assert!(message.contains("не удалось открыть"), "{message}");
    }

    #[test]
    fn empty_file_is_reported() {
        let path = temp_path("mp4");
        std::fs::write(&path, []).expect("создать пустой файл");

        let player = Player::open(&path).expect("поток должен запуститься");
        let reported = wait_event(&player, Duration::from_secs(5), |event| {
            matches!(event, Event::Error(_))
        });
        let _ = std::fs::remove_file(&path);

        let Some(Event::Error(message)) = reported else {
            panic!("пустой файл должен давать Error");
        };
        assert!(message.contains("пуст"), "{message}");
    }

    #[test]
    fn garbage_file_is_reported_without_panic() {
        let path = temp_path("mp4");
        std::fs::write(&path, b"not a media file").expect("создать мусорный файл");

        let player = Player::open(&path).expect("поток должен запуститься");
        let reported = wait_event(&player, Duration::from_secs(5), |event| {
            matches!(event, Event::Error(_))
        });
        let _ = std::fs::remove_file(&path);

        let Some(Event::Error(message)) = reported else {
            panic!("мусорный файл должен давать Error");
        };
        assert!(!message.is_empty());
    }

    #[test]
    fn engine_survives_an_open_error() {
        let missing = temp_path("mp4");
        let player = Player::open(&missing).expect("поток должен запуститься");
        assert!(
            wait_event(&player, Duration::from_secs(5), |event| matches!(
                event,
                Event::Error(_)
            ))
            .is_some(),
            "сначала должна прийти ошибка"
        );

        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        player
            .send(Command::Open(MediaSource::File(path)))
            .expect("открыть другой источник");
        let ready = wait_ready(&player);
        assert!(
            ready.video_info.is_some(),
            "второй источник должен открыться"
        );
    }

    #[test]
    fn open_returns_before_the_source_is_ready() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let started = Instant::now();
        let player = Player::open_with(&path, |source, cancelled| {
            std::thread::sleep(Duration::from_millis(300));
            if cancelled() {
                return Err(PlayerError::Cancelled);
            }
            Media::open(source)
        })
        .expect("поток должен запуститься");
        let open_latency = started.elapsed();

        assert!(
            open_latency < Duration::from_millis(50),
            "open вернулся за {open_latency:?}, а должен сразу"
        );

        wait_ready(&player);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "Ready не должен приходить раньше открытия"
        );
    }

    fn served(
        path: &std::path::Path,
        content_type: &'static str,
        name: &'static str,
        honor_range: bool,
    ) -> crate::http::test_support::ServedFile {
        let bytes = std::fs::read(path).expect("прочитать тестовый файл");
        crate::http::test_support::serve(bytes, content_type, name, honor_range)
    }

    #[test]
    fn url_video_opens_and_plays() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };
        let server = served(&path, "video/mp4", "video.mp4", true);
        let player = Player::open(MediaSource::Url(server.url.clone())).expect("поток");

        let ready = wait_ready(&player);
        assert!(ready.seekable, "Range поддерживается");
        assert!(ready.video_info.is_some(), "видеодорожка");
        assert!(ready.media_info.container.is_some(), "контейнер");

        player.send(Command::Play).expect("Play");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frame = None;
        while Instant::now() < deadline && frame.is_none() {
            frame = player.try_recv_frame();
            if frame.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(frame.is_some(), "кадр должен прийти по HTTP");
    }

    #[test]
    fn url_without_range_is_not_seekable() {
        let Some(path) = sample_av() else {
            eprintln!("нет assets/test/sync_test.webm — пропускаем проверку без Range");
            return;
        };
        let server = served(&path, "video/webm", "video.webm", false);
        let player = Player::open(MediaSource::Url(server.url.clone())).expect("поток");

        let ready = wait_ready(&player);
        assert!(!ready.seekable, "сервер без Range не перематывается");
        assert!(ready.video_info.is_some());
        assert!(ready.media_info.audio.is_some());
    }

    #[test]
    fn url_audio_opens_through_ffmpeg() {
        let Some(path) = sample_audio() else {
            eprintln!("нет assets/test/audio_only.m4a — пропускаем проверку звука по URL");
            return;
        };
        let server = served(&path, "audio/mp4", "audio.m4a", true);
        let player = Player::open(MediaSource::Url(server.url.clone())).expect("поток");

        let ready = wait_ready(&player);
        assert!(ready.video_info.is_none(), "в m4a нет видео");
        assert!(ready.media_info.audio.is_some(), "звук открылся");
        assert!(ready.seekable, "Range поддерживается");
    }

    /// Открывает клип по HTTP и ждёт `Ready`; `None` — клип не удалось
    /// подготовить (нет ни ассета, ни `ffmpeg` для нужного кодека).
    fn url_clip_ready(
        asset: &str,
        extension: &str,
        args: &[&str],
        content_type: &'static str,
    ) -> Option<MediaReady> {
        let path = sample_clip(asset, extension, args)?;
        let server = served(&path, content_type, "clip", true);
        let player = Player::open(MediaSource::Url(server.url.clone())).expect("поток");
        Some(wait_ready(&player))
    }

    #[test]
    fn url_mkv_opens_and_plays() {
        let Some(ready) = url_clip_ready("video_only.mkv", "mkv", VIDEO_CLIP, "video/x-matroska")
        else {
            eprintln!("нет mkv и не удалось сгенерировать — пропускаем");
            return;
        };
        assert!(ready.video_info.is_some(), "видеодорожка mkv");
        assert!(ready.seekable, "Range поддерживается");
        assert!(ready.media_info.container.is_some(), "контейнер известен");
    }

    #[test]
    fn url_webm_opens_and_plays() {
        let Some(ready) = url_clip_ready("video_only.webm", "webm", WEBM_CLIP, "video/webm") else {
            eprintln!("нет webm и не удалось сгенерировать — пропускаем");
            return;
        };
        assert!(ready.video_info.is_some(), "видеодорожка webm");
        assert!(ready.seekable, "Range поддерживается");
    }

    #[test]
    fn url_mp3_opens_through_ffmpeg() {
        let Some(ready) = url_clip_ready("audio_only.mp3", "mp3", MP3_CLIP, "audio/mpeg") else {
            eprintln!("нет mp3 и не удалось сгенерировать — пропускаем");
            return;
        };
        assert!(ready.video_info.is_none(), "в mp3 нет видео");
        assert!(ready.media_info.audio.is_some(), "звук открылся");
        assert!(ready.seekable, "Range поддерживается");
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
        // декодирования. В обоих случаях ошибка должна дойти до UI событием,
        // а не превратиться в панику.
        let player = Player::open(&path).expect("поток должен запуститься");
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

        let _ = std::fs::remove_file(&path);
        assert!(reported, "обрезанный файл должен сообщить об ошибке");
    }

    #[test]
    fn stop_cancels_a_slow_open() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let player = Player::open_with(&path, move |source, is_cancelled| {
            std::thread::sleep(Duration::from_millis(300));
            if is_cancelled() {
                flag.store(true, Ordering::SeqCst);
                return Err(PlayerError::Cancelled);
            }
            Media::open(source)
        })
        .expect("поток должен запуститься");

        player.send(Command::Stop).expect("Stop должен дойти");
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            cancelled.load(Ordering::SeqCst),
            "открытие должно увидеть отмену"
        );
        assert!(
            wait_event(&player, Duration::from_millis(200), |event| matches!(
                event,
                Event::Ready(_)
            ))
            .is_none(),
            "отменённое открытие не должно присылать Ready"
        );
    }

    #[test]
    fn reopening_cancels_the_previous_open() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let calls = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&calls);
        let player = Player::open_with(&path, move |source, is_cancelled| {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            if is_cancelled() {
                return Err(PlayerError::Cancelled);
            }
            Media::open(source)
        })
        .expect("поток должен запуститься");

        // Второй `Open` приходит, пока первое открытие ещё «читает» источник.
        std::thread::sleep(Duration::from_millis(50));
        player
            .send(Command::Open(MediaSource::File(path)))
            .expect("Open должен дойти");

        let ready = wait_ready(&player);
        assert!(
            ready.video_info.is_some(),
            "второй источник должен открыться"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "первое открытие отменено, второе выполнено"
        );

        std::thread::sleep(Duration::from_millis(250));
        assert!(
            player.try_recv_event().is_none(),
            "после Ready не должно быть лишних событий"
        );
    }

    #[test]
    fn dropping_the_player_cancels_a_slow_open() {
        let Some(path) = sample_video() else {
            eprintln!("тестовое видео недоступно (нет ассета и системного ffmpeg) — пропускаем");
            return;
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let player = Player::open_with(&path, move |source, is_cancelled| {
            std::thread::sleep(Duration::from_millis(300));
            if is_cancelled() {
                flag.store(true, Ordering::SeqCst);
                return Err(PlayerError::Cancelled);
            }
            Media::open(source)
        })
        .expect("поток должен запуститься");

        drop(player);
        std::thread::sleep(Duration::from_millis(500));

        assert!(
            cancelled.load(Ordering::SeqCst),
            "Drop должен отменить открытие"
        );
    }
}
