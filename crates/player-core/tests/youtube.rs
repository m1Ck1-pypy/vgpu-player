//! Сквозные тесты YouTube-пути через публичный контракт `Player`/`Event`.
//!
//! Локальный сервер отвечает на `POST /youtubei/v1/player` и
//! `POST /youtubei/v1/visitor_id` фикстурами Innertube, а HLS-ассеты отдаёт с
//! диска. Внешняя сеть не нужна: `Media` резолвит ссылку на этот же сервер.
//!
//! Адреса Innertube подменяются скрытым хуком
//! [`player_core::set_endpoints_for_tests`]; сервер и ассеты живут в одном
//! `OnceLock`, поэтому тесты можно запускать параллельно.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use player_core::{Command, Event, MediaReady, NetworkStats, Player, Source};
use serde_json::json;
use tiny_http::{Header, Response, Server, StatusCode};

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const PLAY_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_TIMEOUT: Duration = Duration::from_secs(15);

const VIDEO_ID: &str = "dQw4w9WgXcQ";
const LIVE_ID: &str = "live0000001";
const NO_HLS_ID: &str = "nohls000001";
const LOGIN_ID: &str = "login000001";
const AGE_ID: &str = "age00000001";
const UNAVAILABLE_ID: &str = "unavail0001";

#[test]
fn youtube_vod_plays_seeks_and_ends() {
    let Some(fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем сквозной YouTube-тест");
        return;
    };
    let player = Player::open(Source::Url(watch_url(VIDEO_ID))).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, READY_TIMEOUT);
    assert!(!ready.live, "VOD не является live");
    assert!(ready.seekable, "VOD перематывается");
    assert_eq!(ready.video_info.map(|info| info.width), Some(64));
    assert!(ready.media_info.video.is_some(), "видеодорожка открыта");
    let audio = ready.media_info.audio.expect("audio-rendition открыт");
    assert!(
        audio.channels > 0 && audio.sample_rate > 0,
        "параметры звука: {audio:?}"
    );
    assert_eq!(
        ready.title.as_deref(),
        Some("Mock ролик"),
        "название из videoDetails.title"
    );

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    // Статистика приходит раз в 500 мс; даём ядру собрать счётчики обеих дорожек.
    pump.drain_for(&player, Duration::from_millis(700));
    let network = pump.last_network.expect("Stats с сетевыми счётчиками");
    assert!(network.requests >= 5, "{network:?}");
    assert!(network.bytes > 0, "{network:?}");
    assert_eq!(network.cache_bytes, 0, "у HLS нет блочного кэша");
    assert!(
        network.window_bytes > 0,
        "у HLS занято окно сегментов: {network:?}"
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

    assert!(
        fixture.server.hits("/youtubei/v1/player") >= 1,
        "резолвер обратился к mock /player"
    );
    assert!(
        fixture.server.hits("/master.m3u8") >= 1,
        "HLS-мастер скачан с mock-сервера"
    );
    for segment in playlist_segments(&fixture.directory.join("v/index.m3u8")) {
        assert!(
            fixture.server.hits(&format!("/v/{segment}")) >= 1,
            "видео-сегмент {segment} скачан"
        );
    }
}

#[test]
fn youtube_live_is_rejected() {
    let Some(_fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем live-проверку");
        return;
    };
    let message = expect_error("live", watch_url(LIVE_ID));
    assert!(message.contains("live пока не поддерживается"), "{message}");
}

#[test]
fn youtube_without_hls_reports_manifest_error() {
    let Some(_fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем проверку отсутствия HLS");
        return;
    };
    let message = expect_error("no hls", watch_url(NO_HLS_ID));
    assert!(
        message.contains("не удалось получить HLS-манифест"),
        "{message}"
    );
}

#[test]
fn youtube_login_required_is_reported() {
    let Some(_fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем проверку входа");
        return;
    };
    let message = expect_error("login", watch_url(LOGIN_ID));
    assert!(message.contains("требуется вход или проверка"), "{message}");
}

#[test]
fn youtube_age_restricted_is_reported() {
    let Some(_fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем проверку возраста");
        return;
    };
    let message = expect_error("age", watch_url(AGE_ID));
    assert!(message.contains("возраст"), "{message}");
}

#[test]
fn youtube_unavailable_is_reported() {
    let Some(_fixture) = fixture() else {
        eprintln!("ffmpeg недоступен — пропускаем проверку недоступности");
        return;
    };
    let message = expect_error("unavailable", watch_url(UNAVAILABLE_ID));
    assert!(message.contains("видео недоступно"), "{message}");
}

#[test]
fn youtube_playlist_is_not_a_video() {
    // Плейлист даже не доходит до сети: резолвер сразу отвечает `NotAVideo`.
    let message = expect_error(
        "playlist",
        "https://www.youtube.com/playlist?list=PL1234567890".to_owned(),
    );
    assert!(message.contains("ссылка не является видео"), "{message}");
}

/// Реальная сеть: перемотка вперёд, затем назад должна вернуть кадры.
///
/// Запускать вручную при рабочем VPN/прокси:
/// `HTTPS_PROXY=... cargo test -p player-core --test youtube real_vod_seek -- --ignored --nocapture`.
#[test]
#[ignore = "network: real YouTube"]
fn real_vod_seek_forward_then_back_resumes() {
    let player =
        Player::open(Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned())).expect("поток ядра");
    let mut pump = Pump::default();

    let ready = pump.ready(&player, Duration::from_secs(20));
    let duration = ready.duration.expect("длительность VOD");
    eprintln!("duration: {duration:?}, title: {:?}", ready.title);

    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры до перемотки не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    let forward = Duration::from_secs_f64(duration.as_secs_f64() * 0.6);
    player.send(Command::Seek(forward)).expect("Seek вперёд");
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| {
        matches!(event, Event::Seeked(_))
    }) {
        Some(Event::Seeked(position)) => eprintln!("Seeked вперёд: {position:?}"),
        other => panic!(
            "Seeked вперёд не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
    let after_forward = pump.frames;
    assert!(
        pump.wait_frames(&player, after_forward + 2, PLAY_TIMEOUT),
        "кадры после перемотки вперёд не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    let back = Duration::from_secs(5);
    player.send(Command::Seek(back)).expect("Seek назад");
    match pump.pump_until(&player, PLAY_TIMEOUT, |event| {
        matches!(event, Event::Seeked(_))
    }) {
        Some(Event::Seeked(position)) => eprintln!("Seeked назад: {position:?}"),
        other => panic!(
            "Seeked назад не пришёл: {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
    let after_back = pump.frames;
    assert!(
        pump.wait_frames(&player, after_back + 2, PLAY_TIMEOUT),
        "кадры после перемотки назад не пришли (баг); последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

/// То же стрелками: `SeekRelative` вперёд, затем назад.
#[test]
#[ignore = "network: real YouTube"]
fn real_vod_relative_seek_forward_then_back_resumes() {
    let player =
        Player::open(Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned())).expect("поток ядра");
    let mut pump = Pump::default();
    pump.ready(&player, Duration::from_secs(20));
    player.send(Command::Play).expect("Play");
    assert!(
        pump.wait_frames(&player, 3, PLAY_TIMEOUT),
        "кадры до перемотки не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    player
        .send(Command::SeekRelative {
            forward: true,
            step: Duration::from_secs(5),
        })
        .expect("SeekRelative вперёд");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked вперёд не пришёл; последняя ошибка: {:?}",
        pump.last_error
    );
    let after_forward = pump.frames;
    assert!(
        pump.wait_frames(&player, after_forward + 2, PLAY_TIMEOUT),
        "кадры после стрелки вперёд не пришли; последняя ошибка: {:?}",
        pump.last_error
    );

    player
        .send(Command::SeekRelative {
            forward: false,
            step: Duration::from_secs(10),
        })
        .expect("SeekRelative назад");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked назад не пришёл; последняя ошибка: {:?}",
        pump.last_error
    );
    let after_back = pump.frames;
    assert!(
        pump.wait_frames(&player, after_back + 2, PLAY_TIMEOUT),
        "кадры после стрелки назад не пришли (баг); последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

/// Перемотка в самый конец (до `Ended`), затем назад.
#[test]
#[ignore = "network: real YouTube"]
fn real_vod_seek_end_then_back_resumes() {
    let player =
        Player::open(Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned())).expect("поток ядра");
    let mut pump = Pump::default();
    let ready = pump.ready(&player, Duration::from_secs(20));
    let duration = ready.duration.expect("длительность");
    player.send(Command::Play).expect("Play");
    assert!(pump.wait_frames(&player, 3, PLAY_TIMEOUT));

    player.send(Command::Seek(duration)).expect("Seek в конец");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked в конец не пришёл; последняя ошибка: {:?}",
        pump.last_error
    );
    pump.drain_for(&player, Duration::from_millis(1500));
    eprintln!(
        "после конца: ended={}, ошибка={:?}",
        pump.ended, pump.last_error
    );

    let back = Duration::from_secs(5);
    player.send(Command::Seek(back)).expect("Seek назад");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked назад не пришёл; последняя ошибка: {:?}",
        pump.last_error
    );
    let after_back = pump.frames;
    assert!(
        pump.wait_frames(&player, after_back + 2, PLAY_TIMEOUT),
        "кадры после конца и перемотки назад не пришли (баг); последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

/// Перемотка в 0 после вперёд.
#[test]
#[ignore = "network: real YouTube"]
fn real_vod_seek_to_zero_resumes() {
    let player =
        Player::open(Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned())).expect("поток ядра");
    let mut pump = Pump::default();
    pump.ready(&player, Duration::from_secs(20));
    player.send(Command::Play).expect("Play");
    assert!(pump.wait_frames(&player, 3, PLAY_TIMEOUT));

    player
        .send(Command::Seek(Duration::from_secs(20)))
        .expect("Seek вперёд");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked вперёд не пришёл"
    );
    let after_forward = pump.frames;
    assert!(pump.wait_frames(&player, after_forward + 2, PLAY_TIMEOUT));

    player
        .send(Command::Seek(Duration::ZERO))
        .expect("Seek в 0");
    assert!(
        pump.pump_until(&player, PLAY_TIMEOUT, |event| matches!(
            event,
            Event::Seeked(_)
        ))
        .is_some(),
        "Seeked в 0 не пришёл; последняя ошибка: {:?}",
        pump.last_error
    );
    let after_zero = pump.frames;
    assert!(
        pump.wait_frames(&player, after_zero + 2, PLAY_TIMEOUT),
        "кадры после перемотки в 0 не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

/// Две перемотки подряд без ожидания между ними.
#[test]
#[ignore = "network: real YouTube"]
fn real_vod_rapid_seeks_resume() {
    let player =
        Player::open(Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned())).expect("поток ядра");
    let mut pump = Pump::default();
    pump.ready(&player, Duration::from_secs(20));
    player.send(Command::Play).expect("Play");
    assert!(pump.wait_frames(&player, 3, PLAY_TIMEOUT));

    player
        .send(Command::Seek(Duration::from_secs(15)))
        .expect("Seek вперёд");
    player
        .send(Command::Seek(Duration::from_secs(5)))
        .expect("Seek назад");

    let mut seeked = 0;
    let deadline = Instant::now() + Duration::from_secs(15);
    while seeked < 2 && Instant::now() < deadline {
        while let Some(event) = player.try_recv_event() {
            if matches!(event, Event::Seeked(_)) {
                seeked += 1;
            }
            pump.record(&event);
        }
        pump.drain_frames(&player);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        seeked, 2,
        "оба Seeked должны прийти; ошибка: {:?}",
        pump.last_error
    );

    let after = pump.frames;
    assert!(
        pump.wait_frames(&player, after + 2, PLAY_TIMEOUT),
        "кадры после двух перемоток не пришли; последняя ошибка: {:?}",
        pump.last_error
    );
    let _ = player.send(Command::Stop);
}

fn watch_url(id: &str) -> String {
    format!("https://youtu.be/{id}")
}

// --- mock: Innertube + HLS --------------------------------------------------

struct Fixture {
    directory: PathBuf,
    server: MockServer,
}

/// Один сервер и один набор HLS-ассетов на весь тестовый бинарник.
fn fixture() -> Option<&'static Fixture> {
    static FIXTURE: OnceLock<Option<Fixture>> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let directory = generate_demuxed()?;
            let server = MockServer::start(directory.clone());
            player_core::set_endpoints_for_tests(
                server.url("/youtubei/v1/player"),
                server.url("/youtubei/v1/visitor_id"),
            );
            Some(Fixture { directory, server })
        })
        .as_ref()
}

struct MockServer {
    base: String,
    stop: Arc<AtomicBool>,
    hits: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    fn start(root: PathBuf) -> Self {
        let server = Server::http("127.0.0.1:0").expect("тестовый сервер");
        let address = server.server_addr().to_ip().expect("ip-адрес");
        let base = format!("http://{address}");
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(Mutex::new(Vec::new()));

        let thread_base = base.clone();
        let thread_stop = Arc::clone(&stop);
        let thread_hits = Arc::clone(&hits);
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(20)) {
                    Ok(Some(request)) => {
                        let root = root.clone();
                        let base = thread_base.clone();
                        let hits = Arc::clone(&thread_hits);
                        // Медленный ответ не должен блокировать остальные запросы.
                        std::thread::spawn(move || respond(request, &root, &base, &hits));
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });

        Self { base, stop, hits }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
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

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn respond(mut request: tiny_http::Request, root: &Path, base: &str, hits: &Mutex<Vec<String>>) {
    let url = request.url().to_owned();
    let path = url.split(['?', '#']).next().unwrap_or_default().to_owned();
    hits.lock().unwrap().push(path.clone());

    if request.method().as_str() == "POST" {
        let mut body = String::new();
        let _ = request.as_reader().take(1 << 20).read_to_string(&mut body);
        match path.as_str() {
            "/youtubei/v1/player" => {
                let id = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("videoId")
                            .and_then(|id| id.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                respond_data(request, 200, player_body(&id, base), "application/json");
            }
            "/youtubei/v1/visitor_id" => respond_data(
                request,
                200,
                r#"{"responseContext":{"visitorData":"mock-visitor"}}"#
                    .as_bytes()
                    .to_vec(),
                "application/json",
            ),
            _ => respond_data(request, 404, Vec::new(), "text/plain"),
        }
        return;
    }

    match read_asset(root, &path) {
        Some(body) => respond_data(request, 200, body, content_type_for(&path)),
        None => respond_data(request, 404, Vec::new(), "text/plain"),
    }
}

/// Ответ `POST /youtubei/v1/player` под конкретный сценарий теста.
fn player_body(id: &str, base: &str) -> Vec<u8> {
    let manifest = format!("{base}/master.m3u8");
    let value = match id {
        LIVE_ID => json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"isLive": true},
            "streamingData": {"hlsManifestUrl": manifest},
        }),
        NO_HLS_ID => json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"isLive": false},
        }),
        LOGIN_ID => json!({
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "Sign in to confirm you're not a bot",
            },
        }),
        AGE_ID => json!({
            "playabilityStatus": {
                "status": "LOGIN_REQUIRED",
                "reason": "Sign in to confirm your age",
            },
        }),
        UNAVAILABLE_ID => json!({
            "playabilityStatus": {
                "status": "UNPLAYABLE",
                "reason": "Video unavailable",
            },
        }),
        _ => json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"isLive": false, "title": "Mock ролик"},
            "streamingData": {"hlsManifestUrl": manifest},
        }),
    };
    value.to_string().into_bytes()
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

fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if path.ends_with(".ts") {
        "video/mp2t"
    } else if path.ends_with(".m4s") || path.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
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
    let directory = test_root().join(format!("yt-{tag}-{}-{nanos}-{id}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("каталог YouTube-теста");
    directory
}

fn run_ffmpeg(
    directory: &Path,
    output: &str,
    extra: &[&str],
    segment_pattern: &str,
    seconds: &str,
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
        .args(extra)
        .args(["-hls_segment_filename", segment_pattern])
        .args([
            "-f",
            "hls",
            "-hls_time",
            "0.5",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
        ])
        .arg(output);

    matches!(command.status(), Ok(status) if status.success())
}

fn generate_demuxed() -> Option<PathBuf> {
    let directory = unique_dir("demuxed");
    std::fs::create_dir_all(directory.join("v")).ok()?;
    std::fs::create_dir_all(directory.join("a")).ok()?;

    if !run_ffmpeg(&directory, "v/index.m3u8", &["-an"], "v/seg%03d.ts", "2") {
        return None;
    }
    if !run_ffmpeg(&directory, "a/index.m3u8", &["-vn"], "a/seg%03d.ts", "2") {
        return None;
    }

    let master = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"main\",DEFAULT=YES,CHANNELS=\"1\",URI=\"a/index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=64x48,CODECS=\"avc1.42c01e,mp4a.40.2\",AUDIO=\"aud\"\n\
        v/index.m3u8\n";
    let written = std::fs::write(directory.join("master.m3u8"), master).is_ok();
    written.then_some(directory)
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

// --- события ----------------------------------------------------------------

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
    fn ready(&mut self, player: &Player, timeout: Duration) -> MediaReady {
        match self.pump_until(player, timeout, |event| matches!(event, Event::Ready(_))) {
            Some(Event::Ready(ready)) => *ready,
            other => panic!(
                "Ready не пришёл: {other:?}; последняя ошибка: {:?}",
                self.last_error
            ),
        }
    }

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

fn expect_error(label: &str, url: String) -> String {
    let player = Player::open(Source::Url(url)).expect("поток ядра");
    let mut pump = Pump::default();
    match pump.pump_until(&player, ERROR_TIMEOUT, |event| {
        matches!(event, Event::Error(_))
    }) {
        Some(Event::Error(message)) => message,
        other => panic!(
            "{label}: ожидалась ошибка, получено {other:?}; последняя ошибка: {:?}",
            pump.last_error
        ),
    }
}
