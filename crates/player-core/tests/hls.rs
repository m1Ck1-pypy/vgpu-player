//! Сквозные HLS-тесты через публичный контракт `Player`/`Event`.
//!
//! Ассеты генерируются локальным `ffmpeg` в `target/hls-tests` и отдаются
//! HTTP-сервером на `127.0.0.1`; внешняя сеть не нужна. Тесты проверяют то,
//! что видит UI: `Ready`, кадры, `Seeked`, `Ended`, `Stats` и ошибки.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use player_core::{Command, Event, MediaReady, NetworkStats, Player, Source};
use tiny_http::{Header, Response, Server, StatusCode};

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const PLAY_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn muxed_hls_vod_plays_seeks_pauses_and_ends() {
    let Some(directory) = generate_muxed() else {
        eprintln!("ffmpeg недоступен — пропускаем muxed HLS-тест");
        return;
    };
    let server = AssetServer::start(directory.clone());
    let player = Player::open(Source::Url(server.url("/index.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(!ready.live, "VOD не является live");
    assert!(ready.seekable, "VOD перематывается");
    let duration = ready.duration.expect("длительность VOD");
    assert!(duration >= Duration::from_millis(1500), "{duration:?}");
    assert!(ready.video_info.is_some(), "видеодорожка открыта");
    assert!(
        ready.media_info.audio.is_some(),
        "muxed: звук лежит в том же варианте"
    );

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры до паузы не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    player.send(Command::Pause).expect("Pause");
    pump.drain_for(&player, Duration::from_millis(150));
    let paused = pump.last_position;
    pump.drain_for(&player, Duration::from_millis(300));
    assert!(
        pump.last_position.saturating_sub(paused) <= Duration::from_millis(60),
        "позиция на паузе не стоит: {paused:?} -> {:?}",
        pump.last_position
    );

    player.send(Command::Play).expect("Play после паузы");
    let target = duration / 2;
    player.send(Command::Seek(target)).expect("Seek");
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| {
        matches!(event, Event::Seeked(_))
    }) {
        Some(Event::Seeked(position)) => assert!(
            position >= target.saturating_sub(Duration::from_millis(250)),
            "Seeked {position:?}, цель {target:?}"
        ),
        other => panic!(
            "Seeked не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }

    let after_seek = pump.frames;
    assert!(
        pump.wait_frames(&player, after_seek + 2, PLAY_TIMEOUT),
        "кадры после перемотки не пришли"
    );
    // Дочитываем остаток: так все сегменты попадают в кэш и проверка
    // трафика ниже видит каждый из них ровно один раз.
    pump.drain_for(&player, Duration::from_millis(1800));
    // На машине без звукового устройства ядро не присылает `Ended` для
    // источника со звуком (`audio_blocked`); тогда конец проверяет
    // видео-только тест.
    if !pump.audio_device_failed {
        if !pump.ended {
            pump.pump_until(&player, Duration::from_secs(2), |event| {
                matches!(event, Event::Ended)
            });
        }
        assert!(
            pump.ended,
            "Ended не пришёл; последняя ошибка: {:?}",
            pump.last_error
        );
    }
    let _ = player.send(Command::Stop);

    for segment in playlist_segments(&directory.join("index.m3u8")) {
        let path = format!("/{segment}");
        assert_eq!(
            server.hits(&path),
            1,
            "{path}: muxed-сегмент должен качаться один раз"
        );
    }
}

#[test]
fn prefetch_downloads_ahead_while_the_reader_is_paused() {
    let Some(directory) = generate_muxed() else {
        eprintln!("ffmpeg недоступен — пропускаем prefetch-тест");
        return;
    };
    let segments = playlist_segments(&directory.join("index.m3u8"));
    assert!(
        segments.len() >= 3,
        "для prefetch нужно несколько сегментов"
    );

    let server = AssetServer::start(directory.clone());
    for segment in &segments {
        // Медленный CDN: без фоновых загрузчиков окно вперёд не скачается.
        server.set_delayed(&format!("/{segment}"), Duration::from_millis(300));
    }

    let player = Player::open(Source::Url(server.url("/index.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();
    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(ready.video_info.is_some(), "видеодорожка открыта");

    // Пауза: читатель не трогает сегменты, но prefetch-пул должен скачать
    // окно вперёд. Без пула здесь был бы запрошен только первый сегмент.
    let deadline = Instant::now() + Duration::from_secs(5);
    let requested = loop {
        let requested = segments
            .iter()
            .filter(|segment| server.hits(&format!("/{segment}")) > 0)
            .count();
        if requested >= segments.len() || Instant::now() >= deadline {
            break requested;
        }
        pump.drain_for(&player, Duration::from_millis(50));
    };

    assert!(
        requested >= segments.len().min(3),
        "prefetch не качает вперёд: запрошено {requested} из {} сегментов",
        segments.len()
    );
    let _ = player.send(Command::Stop);
}

#[test]
fn demuxed_hls_vod_opens_both_tracks_and_counts_traffic() {
    let Some(directory) = generate_demuxed() else {
        eprintln!("ffmpeg недоступен — пропускаем demuxed HLS-тест");
        return;
    };
    let server = AssetServer::start(directory.clone());
    let player = Player::open(Source::Url(server.url("/master.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(!ready.live);
    assert!(ready.seekable);
    assert_eq!(ready.video_info.map(|info| info.width), Some(64));
    assert!(ready.media_info.video.is_some(), "видеодорожка открыта");
    let audio = ready.media_info.audio.expect("audio-rendition открыт");
    assert!(
        audio.channels > 0 && audio.sample_rate > 0,
        "параметры звука: {audio:?}"
    );

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    // Статистика приходит раз в 500 мс; даём ядру время собрать счётчики
    // обеих дорожек.
    pump.drain_for(&player, Duration::from_millis(700));
    let network = pump.last_network.expect("Stats с сетевыми счётчиками");
    assert!(network.requests >= 5, "{network:?}");
    assert!(network.bytes > 0, "{network:?}");
    assert_eq!(network.cache_bytes, 0, "у HLS нет блочного кэша");

    let target = Duration::from_millis(500);
    player.send(Command::Seek(target)).expect("Seek");
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| {
        matches!(event, Event::Seeked(_))
    }) {
        Some(Event::Seeked(position)) => assert!(
            position >= target.saturating_sub(Duration::from_millis(250)),
            "Seeked {position:?}, цель {target:?}"
        ),
        other => panic!(
            "Seeked не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
    let after_seek = pump.frames;
    assert!(
        pump.wait_frames(&player, after_seek + 2, PLAY_TIMEOUT),
        "кадры после перемотки не пришли"
    );
    // Дочитываем остаток, чтобы видео-сегменты запросились по одному разу.
    pump.drain_for(&player, Duration::from_millis(1800));
    if !pump.audio_device_failed {
        if !pump.ended {
            pump.pump_until(&player, Duration::from_secs(2), |event| {
                matches!(event, Event::Ended)
            });
        }
        assert!(
            pump.ended,
            "Ended не пришёл; последняя ошибка: {:?}",
            pump.last_error
        );
    }
    let _ = player.send(Command::Stop);

    for segment in playlist_segments(&directory.join("v/index.m3u8")) {
        assert_eq!(
            server.hits(&format!("/v/{segment}")),
            1,
            "видео-сегмент {segment} скачан один раз"
        );
    }
    for segment in playlist_segments(&directory.join("a/index.m3u8")) {
        assert!(
            server.hits(&format!("/a/{segment}")) <= 1,
            "аудио-сегмент {segment} не должен качаться дважды"
        );
    }
}

#[test]
fn demuxed_fmp4_hls_plays_and_seeks() {
    let Some(directory) = generate_demuxed_fmp4() else {
        eprintln!("ffmpeg недоступен — пропускаем demuxed fMP4");
        return;
    };
    let server = AssetServer::start(directory);
    let player = Player::open(Source::Url(server.url("/master.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(!ready.live);
    assert!(ready.seekable, "fMP4 VOD перематывается");
    assert_eq!(ready.video_info.map(|info| info.width), Some(64));
    assert!(ready.media_info.audio.is_some(), "fMP4 audio-rendition");

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "fMP4-кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    let target = Duration::from_millis(500);
    player.send(Command::Seek(target)).expect("Seek");
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| {
        matches!(event, Event::Seeked(_))
    }) {
        Some(Event::Seeked(position)) => assert!(
            position >= target.saturating_sub(Duration::from_millis(250)),
            "Seeked {position:?}, цель {target:?}"
        ),
        other => panic!(
            "Seeked не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
    let after_seek = pump.frames;
    assert!(
        pump.wait_frames(&player, after_seek + 2, PLAY_TIMEOUT),
        "fMP4-кадры после перемотки не пришли"
    );
    let _ = player.send(Command::Stop);

    assert_eq!(server.hits("/v/init.mp4"), 1, "видео-init один раз");
    assert_eq!(server.hits("/a/init.mp4"), 1, "аудио-init один раз");
}

#[test]
fn demuxed_live_hls_opens_both_tracks() {
    let Some(directory) = generate_demuxed_live_source() else {
        eprintln!("ffmpeg недоступен — пропускаем demuxed live");
        return;
    };
    let growing = Arc::new(AtomicBool::new(true));
    let server = demuxed_live_server(&directory, growing);
    let player = Player::open(Source::Url(server.url("/master.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(ready.live, "оба плейлиста без ENDLIST");
    assert!(!ready.seekable);
    assert!(ready.video_info.is_some(), "видео-дорожка live");
    assert!(ready.media_info.audio.is_some(), "аудио-дорожка live");

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 2, PLAY_TIMEOUT),
        "demuxed live-кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

#[test]
fn live_fmp4_hls_opens_both_tracks() {
    let Some(directory) = generate_live_fmp4_source() else {
        eprintln!("ffmpeg недоступен — пропускаем live fMP4");
        return;
    };
    let growing = Arc::new(AtomicBool::new(true));
    let server = live_server(&directory, growing);
    let player = Player::open(Source::Url(server.url("/live.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(ready.live);
    assert!(!ready.seekable);
    assert!(ready.video_info.is_some(), "fMP4 видео live");
    assert!(ready.media_info.audio.is_some(), "fMP4 аудио live");

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 2, PLAY_TIMEOUT),
        "live fMP4-кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

#[test]
fn video_only_hls_plays_to_the_end() {
    let Some(directory) = generate_video_only() else {
        eprintln!("ffmpeg недоступен — пропускаем видео-только HLS");
        return;
    };
    let server = AssetServer::start(directory);
    let player = Player::open(Source::Url(server.url("/index.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(!ready.live);
    assert!(ready.seekable);
    assert!(
        ready.media_info.audio.is_none(),
        "видео-только источник не должен иметь звука"
    );

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(event, Event::Ended)) {
        Some(Event::Ended) => {}
        other => panic!(
            "Ended не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
}

#[test]
fn live_hls_reports_live_and_ignores_seek() {
    let Some(directory) = generate_live_source() else {
        eprintln!("ffmpeg недоступен — пропускаем live HLS-тест");
        return;
    };
    let growing = Arc::new(AtomicBool::new(true));
    let server = live_server(&directory, Arc::clone(&growing));
    let player = Player::open(Source::Url(server.url("/live.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(ready.live, "плейлист без ENDLIST — live");
    assert!(!ready.seekable, "live не перематывается");
    assert_eq!(ready.duration, None, "у live нет длительности");

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 2, PLAY_TIMEOUT),
        "live-кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    player.send(Command::Seek(Duration::ZERO)).expect("Seek");
    assert!(
        pump.pump_until(&player, Duration::from_millis(300), |event| {
            matches!(event, Event::Seeked(_))
        })
        .is_none(),
        "live не должен подтверждать перемотку"
    );

    let _ = player.send(Command::Stop);
}

#[test]
fn live_hls_closes_while_reading_segments() {
    let Some(directory) = generate_live_source() else {
        eprintln!("ffmpeg недоступен — пропускаем закрытие live HLS");
        return;
    };
    let growing = Arc::new(AtomicBool::new(true));
    let server = live_server(&directory, Arc::clone(&growing));
    let player = Player::open(Source::Url(server.url("/live.m3u8"))).expect("поток ядра");
    let mut pump = Pump::default();
    pump.ready(&player, READY_TIMEOUT);

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 1, PLAY_TIMEOUT),
        "сегмент live не декодировался"
    );
    // С этого момента окно больше не растёт: следующий сегмент придётся ждать.
    growing.store(false, Ordering::Relaxed);
    pump.drain_for(&player, Duration::from_millis(300));

    drop(player);
    std::thread::sleep(Duration::from_millis(100));
}

#[test]
fn hls_open_errors_reach_the_ui() {
    let server = AssetServer::start(unique_dir("errors"));
    server.set("/missing.m3u8", 404, "", "text/plain");
    server.set("/error.m3u8", 500, "", "text/plain");
    server.set(
        "/broken.m3u8",
        200,
        "#EXTM3U\n#EXTINF:oops\nseg.ts\n",
        "application/vnd.apple.mpegurl",
    );
    server.set(
        "/html.m3u8",
        200,
        "<html><body>nope</body></html>",
        "text/html; charset=utf-8",
    );
    server.set(
        "/aes.m3u8",
        200,
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:1.0,\nseg.ts\n#EXT-X-ENDLIST\n",
        "application/vnd.apple.mpegurl",
    );
    server.set(
        "/bad-audio.m3u8",
        200,
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO=\"nope\"\nv/index.m3u8\n",
        "application/vnd.apple.mpegurl",
    );
    server.set(
        "/empty-master.m3u8",
        200,
        "#EXTM3U\n",
        "application/vnd.apple.mpegurl",
    );
    server.set(
        "/seg-missing.m3u8",
        200,
        "#EXTM3U\n#EXTINF:1.0,\nmissing.ts\n#EXT-X-ENDLIST\n",
        "application/vnd.apple.mpegurl",
    );
    server.set(
        "/seg-error.m3u8",
        200,
        "#EXTM3U\n#EXTINF:1.0,\nerror.ts\n#EXT-X-ENDLIST\n",
        "application/vnd.apple.mpegurl",
    );
    server.set("/error.ts", 500, "", "video/mp2t");
    server.set(
        "/seg-empty.m3u8",
        200,
        "#EXTM3U\n#EXTINF:1.0,\nempty.ts\n#EXT-X-ENDLIST\n",
        "application/vnd.apple.mpegurl",
    );
    server.set("/empty.ts", 200, "", "video/mp2t");

    let cases = [
        ("/missing.m3u8", "404"),
        ("/error.m3u8", "500"),
        ("/broken.m3u8", "строка"),
        ("/html.m3u8", "не медиафайл"),
        ("/aes.m3u8", "HLS"),
        ("/bad-audio.m3u8", "nope"),
        ("/empty-master.m3u8", "нет ни вариантов"),
        ("/seg-missing.m3u8", "404"),
        ("/seg-error.m3u8", "500"),
        ("/seg-empty.m3u8", "пустой сегмент"),
    ];

    for (path, expected) in cases {
        let message = expect_error(path, server.url(path), ERROR_TIMEOUT);
        assert!(
            message.contains(expected),
            "{path}: ожидалось {expected:?}, получено {message:?}"
        );
    }
}

/// События и счётчики, которые накапливаются во время теста.
#[derive(Debug, Default)]
struct Pump {
    frames: u64,
    last_network: Option<NetworkStats>,
    last_position: Duration,
    last_error: Option<String>,
    ended: bool,
    audio_device_failed: bool,
}

impl Pump {
    /// Ждёт `Ready`; ошибка открытия печатается в панике.
    fn ready(&mut self, player: &Player, timeout: Duration) -> MediaReady {
        match self.pump_until(player, timeout, |event| matches!(event, Event::Ready(_))) {
            Some(Event::Ready(ready)) => *ready,
            other => panic!(
                "Ready не пришёл: {other:?}; последняя ошибка: {:?}",
                self.last_error
            ),
        }
    }

    /// Крутит цикл событий/кадров до совпадения или таймаута.
    fn pump_until(
        &mut self,
        player: &Player,
        timeout: Duration,
        mut done: impl FnMut(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(event) = player.try_recv_event() {
                let matched = done(&event);
                self.record(&event);
                if matched {
                    return Some(event);
                }
            }
            self.drain_frames(player);
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ждёт, пока будет получено `target` кадров.
    fn wait_frames(&mut self, player: &Player, target: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.frames < target {
            while let Some(event) = player.try_recv_event() {
                self.record(&event);
            }
            self.drain_frames(player);
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Собирает события и кадры фиксированное время.
    fn drain_for(&mut self, player: &Player, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            while let Some(event) = player.try_recv_event() {
                self.record(&event);
            }
            self.drain_frames(player);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn drain_frames(&mut self, player: &Player) {
        while let Some(frame) = player.try_recv_frame() {
            player.recycle_frame(frame);
            self.frames = self.frames.saturating_add(1);
        }
    }

    fn record(&mut self, event: &Event) {
        match event {
            Event::Stats(stats) => {
                if let Some(network) = stats.network {
                    self.last_network = Some(network);
                }
            }
            Event::PositionChanged(position) => self.last_position = *position,
            Event::Ended => self.ended = true,
            Event::Error(message) => {
                if message.contains("звуковое устройство") {
                    self.audio_device_failed = true;
                }
                self.last_error = Some(message.clone());
            }
            _ => {}
        }
    }
}

fn expect_error(label: &str, url: String, timeout: Duration) -> String {
    let player = Player::open(Source::Url(url)).expect("поток ядра");
    let mut pump = Pump::default();
    match pump.pump_until(&player, timeout, |event| matches!(event, Event::Error(_))) {
        Some(Event::Error(message)) => message,
        other => panic!(
            "{label}: ожидалась ошибка, получено {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
}

// --- сгенерированные ассеты -------------------------------------------------

fn test_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join("hls-tests")
}

fn unique_dir(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let directory = test_root().join(format!("it-{tag}-{}-{nanos}-{id}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("каталог HLS-теста");
    directory
}

fn run_ffmpeg(
    directory: &Path,
    output: &str,
    extra: &[&str],
    segment_pattern: Option<&str>,
    seconds: &str,
    hls_extra: &[&str],
) -> bool {
    let mut command = ProcessCommand::new("ffmpeg");
    command
        .current_dir(directory)
        .args(["-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440"])
        .args(["-t", seconds])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "5",
        ])
        .args(["-c:a", "aac"])
        .args(extra);
    if let Some(pattern) = segment_pattern {
        command.args(["-hls_segment_filename", pattern]);
    }
    command.args(hls_extra).args([
        "-f",
        "hls",
        "-hls_time",
        "0.5",
        "-hls_list_size",
        "0",
        "-hls_playlist_type",
        "vod",
    ]);
    command.arg(output);

    matches!(command.status(), Ok(status) if status.success())
}

fn generate_muxed() -> Option<PathBuf> {
    let directory = unique_dir("muxed");
    run_ffmpeg(&directory, "index.m3u8", &[], None, "2", &[]).then_some(directory)
}

/// Видео-только HLS: `Ended` не зависит от звукового устройства.
fn generate_video_only() -> Option<PathBuf> {
    let directory = unique_dir("video-only");
    run_ffmpeg(&directory, "index.m3u8", &["-an"], None, "2", &[]).then_some(directory)
}

/// Длинный live-источник: уникальные сегменты с непрерывными PTS.
fn generate_live_source() -> Option<PathBuf> {
    let directory = unique_dir("live-source");
    run_ffmpeg(&directory, "index.m3u8", &[], None, "30", &[]).then_some(directory)
}

/// То же в fMP4: `#EXT-X-MAP` и сегменты `.m4s`, как у Twitch.
///
/// PTS сдвинуты на 1000 с: так проверяется нормализация live-потока,
/// где tfdt не начинается с нуля.
fn generate_live_fmp4_source() -> Option<PathBuf> {
    let directory = unique_dir("live-fmp4");
    let fmp4 = [
        "-output_ts_offset",
        "1000",
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
    ];
    run_ffmpeg(&directory, "index.m3u8", &[], None, "30", &fmp4).then_some(directory)
}

fn generate_demuxed() -> Option<PathBuf> {
    let directory = unique_dir("demuxed");
    generate_demuxed_ts(&directory, "2").then_some(directory)
}

fn generate_demuxed_live_source() -> Option<PathBuf> {
    let directory = unique_dir("demuxed-live");
    generate_demuxed_ts(&directory, "30").then_some(directory)
}

fn generate_demuxed_ts(directory: &Path, seconds: &str) -> bool {
    if std::fs::create_dir_all(directory.join("v")).is_err()
        || std::fs::create_dir_all(directory.join("a")).is_err()
    {
        return false;
    }

    if !run_ffmpeg(
        directory,
        "v/index.m3u8",
        &["-an"],
        Some("v/seg%03d.ts"),
        seconds,
        &[],
    ) {
        return false;
    }
    if !run_ffmpeg(
        directory,
        "a/index.m3u8",
        &["-vn"],
        Some("a/seg%03d.ts"),
        seconds,
        &[],
    ) {
        return false;
    }

    write_demuxed_master(directory)
}

fn generate_demuxed_fmp4() -> Option<PathBuf> {
    let directory = unique_dir("demuxed-fmp4");
    std::fs::create_dir_all(directory.join("v")).ok()?;
    std::fs::create_dir_all(directory.join("a")).ok()?;

    let fmp4 = [
        "-hls_segment_type",
        "fmp4",
        "-hls_fmp4_init_filename",
        "init.mp4",
    ];
    if !run_ffmpeg(
        &directory,
        "v/index.m3u8",
        &["-an"],
        Some("v/seg%03d.m4s"),
        "2",
        &fmp4,
    ) {
        return None;
    }
    if !run_ffmpeg(
        &directory,
        "a/index.m3u8",
        &["-vn"],
        Some("a/seg%03d.m4s"),
        "2",
        &fmp4,
    ) {
        return None;
    }

    write_demuxed_master(&directory).then_some(directory)
}

fn write_demuxed_master(directory: &Path) -> bool {
    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,CHANNELS=\"1\",URI=\"a/index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=64x48,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"aud\"\n\
        v/index.m3u8\n";
    std::fs::write(directory.join("master.m3u8"), master).is_ok()
}

/// Собирает live-плейлист без `#EXT-X-ENDLIST` из первых `segments.len()`.
fn live_playlist_body(segments: &[String], map: Option<&str>) -> Vec<u8> {
    let mut text = String::from(
        "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n",
    );
    if let Some(map) = map {
        text.push_str(map);
        text.push('\n');
    }
    for name in segments {
        text.push_str("#EXTINF:0.5,\n");
        text.push_str(name);
        text.push('\n');
    }
    text.into_bytes()
}

/// Строка `#EXT-X-MAP` из исходного плейлиста (fMP4), если она есть.
fn playlist_map(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("#EXT-X-MAP:"))
        .map(str::to_owned)
}

/// Сервер, который, как настоящий live, дописывает сегменты при обновлении.
///
/// `growing = false` замораживает окно: читатель упрётся в конец и будет ждать.
fn live_server(directory: &Path, growing: Arc<AtomicBool>) -> AssetServer {
    let segments = playlist_segments(&directory.join("index.m3u8"));
    let map = playlist_map(&directory.join("index.m3u8"));
    assert!(
        segments.len() >= 24,
        "для live нужно не меньше 24 сегментов"
    );

    let server = AssetServer::start(directory.to_path_buf());
    let extra = Arc::new(AtomicUsize::new(0));
    let handler_extra = Arc::clone(&extra);
    let handler_segments = segments;
    server.set_dynamic("/live.m3u8", move || {
        if growing.load(Ordering::Relaxed) {
            handler_extra.fetch_add(8, Ordering::Relaxed);
        }
        let count = (16 + handler_extra.load(Ordering::Relaxed)).min(handler_segments.len());
        live_playlist_body(&handler_segments[..count], map.as_deref())
    });
    server
}

/// Две независимые дорожки живого эфира: видео и аудио растут по своим запросам.
fn demuxed_live_server(directory: &Path, growing: Arc<AtomicBool>) -> AssetServer {
    let video = playlist_segments(&directory.join("v/index.m3u8"));
    let audio = playlist_segments(&directory.join("a/index.m3u8"));
    assert!(
        video.len() >= 24 && audio.len() >= 24,
        "мало live-сегментов"
    );

    let server = AssetServer::start(directory.to_path_buf());
    let video_extra = Arc::new(AtomicUsize::new(0));
    let audio_extra = Arc::new(AtomicUsize::new(0));

    let handler_growing = Arc::clone(&growing);
    let handler_extra = Arc::clone(&video_extra);
    let handler_video = video;
    server.set_dynamic("/v/index.m3u8", move || {
        if handler_growing.load(Ordering::Relaxed) {
            handler_extra.fetch_add(8, Ordering::Relaxed);
        }
        let count = (16 + handler_extra.load(Ordering::Relaxed)).min(handler_video.len());
        live_playlist_body(&handler_video[..count], None)
    });

    let handler_extra = audio_extra;
    let handler_audio = audio;
    server.set_dynamic("/a/index.m3u8", move || {
        if growing.load(Ordering::Relaxed) {
            handler_extra.fetch_add(8, Ordering::Relaxed);
        }
        let count = (16 + handler_extra.load(Ordering::Relaxed)).min(handler_audio.len());
        live_playlist_body(&handler_audio[..count], None)
    });

    server
}

/// Имена сегментов из media-плейлиста.
fn playlist_segments(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.rsplit('/').next().unwrap_or(line).to_owned())
        .collect()
}

// --- локальный сервер -------------------------------------------------------

#[derive(Clone)]
struct Override {
    status: u16,
    body: Vec<u8>,
    content_type: String,
    delay: Duration,
}

/// HTTP-сервер на `127.0.0.1:0`: файлы из каталога плюс подменяемые маршруты.
struct AssetServer {
    root: PathBuf,
    base: String,
    stop: Arc<AtomicBool>,
    overrides: Arc<Mutex<HashMap<String, Override>>>,
    dynamic: Arc<Mutex<HashMap<String, DynamicRoute>>>,
    hits: Arc<Mutex<Vec<String>>>,
}

type DynamicRoute = Arc<dyn Fn() -> Vec<u8> + Send + Sync>;

impl AssetServer {
    fn start(root: PathBuf) -> Self {
        let server = Server::http("127.0.0.1:0").expect("тестовый сервер");
        let address = server.server_addr().to_ip().expect("ip-адрес");
        let base = format!("http://{address}");
        let stop = Arc::new(AtomicBool::new(false));
        let overrides = Arc::new(Mutex::new(HashMap::new()));
        let dynamic = Arc::new(Mutex::new(HashMap::new()));
        let hits = Arc::new(Mutex::new(Vec::new()));

        {
            let root = root.clone();
            let overrides = Arc::clone(&overrides);
            let dynamic = Arc::clone(&dynamic);
            let hits = Arc::clone(&hits);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match server.recv_timeout(Duration::from_millis(20)) {
                        Ok(Some(request)) => {
                            let root = root.clone();
                            let overrides = Arc::clone(&overrides);
                            let dynamic = Arc::clone(&dynamic);
                            let hits = Arc::clone(&hits);
                            // Медленный ответ не должен блокировать остальные запросы.
                            std::thread::spawn(move || {
                                respond(request, &root, &overrides, &dynamic, &hits);
                            });
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            });
        }

        Self {
            root,
            base,
            stop,
            overrides,
            dynamic,
            hits,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn set(&self, path: &str, status: u16, body: impl Into<Vec<u8>>, content_type: &str) {
        self.overrides.lock().unwrap().insert(
            path.to_owned(),
            Override {
                status,
                body: body.into(),
                content_type: content_type.to_owned(),
                delay: Duration::ZERO,
            },
        );
    }

    /// Отдаёт файл только через `delay` — медленный CDN для prefetch-теста.
    fn set_delayed(&self, path: &str, delay: Duration) {
        let body = read_asset(&self.root, path).expect("файл для задержанного маршрута");
        self.overrides.lock().unwrap().insert(
            path.to_owned(),
            Override {
                status: 200,
                body,
                content_type: content_type_for(path),
                delay,
            },
        );
    }

    /// Маршрут, чьё тело вычисляется на каждом запросе (живой плейлист).
    fn set_dynamic(&self, path: &str, handler: impl Fn() -> Vec<u8> + Send + Sync + 'static) {
        self.dynamic
            .lock()
            .unwrap()
            .insert(path.to_owned(), Arc::new(handler));
    }

    fn hits(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.as_str() == path)
            .count()
    }
}

impl Drop for AssetServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn respond(
    request: tiny_http::Request,
    root: &Path,
    overrides: &Mutex<HashMap<String, Override>>,
    dynamic: &Mutex<HashMap<String, DynamicRoute>>,
    hits: &Mutex<Vec<String>>,
) {
    let url = request.url().to_owned();
    let path = url.split(['?', '#']).next().unwrap_or_default().to_owned();
    hits.lock().unwrap().push(path.clone());

    let handler = dynamic.lock().unwrap().get(&path).cloned();
    if let Some(handler) = handler {
        respond_data(request, 200, handler(), "application/vnd.apple.mpegurl");
        return;
    }

    let overridden = overrides.lock().unwrap().get(&path).cloned();
    let (status, body, content_type) = match overridden {
        Some(route) => {
            if !route.delay.is_zero() {
                std::thread::sleep(route.delay);
            }
            (route.status, route.body, route.content_type)
        }
        None => match read_asset(root, &path) {
            Some(body) => (200, body, content_type_for(&path)),
            None => (404, Vec::new(), "text/plain".to_owned()),
        },
    };

    respond_data(request, status, body, &content_type);
}

fn respond_data(request: tiny_http::Request, status: u16, body: Vec<u8>, content_type: &str) {
    let header = Header::from_bytes("Content-Type".as_bytes(), content_type.as_bytes())
        .expect("заголовок Content-Type");
    let response = Response::from_data(body)
        .with_status_code(StatusCode(status))
        .with_header(header);
    let _ = request.respond(response);
}

fn read_asset(root: &Path, path: &str) -> Option<Vec<u8>> {
    let relative = path.trim_start_matches('/');
    if relative.is_empty() || relative.split('/').any(|part| part == "..") {
        return None;
    }
    std::fs::read(root.join(relative)).ok()
}

fn content_type_for(path: &str) -> String {
    if path.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if path.ends_with(".ts") {
        "video/mp2t"
    } else if path.ends_with(".m4s") || path.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}
