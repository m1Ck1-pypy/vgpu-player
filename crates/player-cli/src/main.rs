//! Консольный запуск ядра: открыть файл, проиграть его, напечатать события.
//!
//! Аргументы разбираются вручную: у CLI по-прежнему мало параметров, `clap`
//! появится вместе с диалогом открытия и плейлистом на этапе 9.

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use player_core::{
    Command, Event, HttpOptions, HttpOptionsError, Player, PlayerError, Source, SourceError,
    SyncStats, VideoDecoder, VideoInfo,
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Строка подсказки — одна для всех ошибок разбора аргументов.
const USAGE: &str = "usage: player-cli <file|url> [--decode-frames <n> [--seek <seconds>]] [--volume <0..100>] [--referrer|--referer <url>] [--user-agent <string>] [-h|--help]";

/// Полная справка: только те возможности, что есть сейчас.
const HELP: &str = "\
player-cli — запуск ядра без окна: печатает события и метрики

usage: player-cli <file|url> [--decode-frames <n> [--seek <seconds>]] [--volume <0..100>] [--referrer <url>] [--user-agent <string>]

Аргументы:
  <file|url>            локальный путь, file: или http(s)-ссылка;
                       MP4/MKV/WebM/MP3 и HLS (VOD и live).
  --decode-frames <n>  декодировать n кадров без звука и напечатать скорость;
                       вместо воспроизведения.
  --seek <seconds>     перемотка перед замером; только с --decode-frames.
  --volume <0..100>    громкость воспроизведения в процентах;
                       на --decode-frames не влияет.
  --referrer <url>     заголовок Referer для HTTP-запросов; алиас --referer.
  --user-agent <str>   заголовок User-Agent; по умолчанию vgpu-player/<версия>.
  -h, -H, --help, /?  эта справка.

Без --decode-frames источник воспроизводится, а в консоль идут
position, duration, video, sync и ended.";

/// Что делать после разбора аргументов.
#[derive(Debug, PartialEq)]
enum Action {
    Help,
    Run {
        source: Source,
        decode_frames: Option<usize>,
        seek: Option<f64>,
        volume: Option<f32>,
        http: HttpOptions,
    },
}

/// Причина отказа разбора.
#[derive(Debug)]
enum ArgsError {
    /// Общая ошибка с пояснением; печатается вместе с `USAGE`.
    Usage(&'static str),
    /// Некорректный `Referer`/`User-Agent`.
    Header(HttpOptionsError),
    /// Строка не похожа ни на путь, ни на URL.
    Source(SourceError),
}

impl std::fmt::Display for ArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(reason) => f.write_str(reason),
            Self::Header(error) => error.fmt(f),
            Self::Source(error) => error.fmt(f),
        }
    }
}

fn main() -> ExitCode {
    init_tracing();

    let action = match parse_args(std::env::args_os().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("player-cli: {error}");
            eprintln!("{USAGE}");
            eprintln!("подробнее: player-cli --help");
            return ExitCode::FAILURE;
        }
    };

    let Action::Run {
        source,
        decode_frames,
        seek,
        volume,
        http,
    } = action
    else {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    };

    let result = match decode_frames {
        Some(limit) => decode_video(&source, limit, seek, http),
        None => play(&source, volume, http),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            error!(error = %format!("{failure:#}"), "player-cli завершился с ошибкой");
            eprintln!("player-cli: {failure:#}");
            ExitCode::FAILURE
        }
    }
}

/// Разбирает аргументы в [`Action`].
///
/// Источник обязателен, остальные параметры — нет. `--help` побеждает
/// остальные аргументы: справку печатаем, не проверяя их.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, ArgsError> {
    let mut args = args.into_iter();
    let mut source = None;
    let mut decode_frames = None;
    let mut seek = None;
    let mut volume = None;
    let mut referrer = None;
    let mut user_agent = None;

    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("-h" | "-H" | "--help" | "/?") => return Ok(Action::Help),
            Some("--decode-frames") => {
                let limit = args
                    .next()
                    .and_then(|value| value.to_str()?.parse::<usize>().ok())
                    .filter(|limit| *limit > 0)
                    .ok_or(ArgsError::Usage("--decode-frames ждёт положительное целое"))?;
                decode_frames = Some(limit);
            }
            Some("--seek") => {
                let seconds = args
                    .next()
                    .and_then(|value| value.to_str()?.parse::<f64>().ok())
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                    .ok_or(ArgsError::Usage("--seek ждёт неотрицательное число секунд"))?;
                seek = Some(seconds);
            }
            Some("--volume") => {
                let value = args
                    .next()
                    .ok_or(ArgsError::Usage("у --volume нет значения"))?;
                volume = Some(parse_volume(&value)?);
            }
            Some("--referrer" | "--referer") => {
                if referrer.is_some() {
                    return Err(ArgsError::Usage("--referrer указан дважды"));
                }
                let value = args
                    .next()
                    .ok_or(ArgsError::Usage("у --referrer нет значения"))?;
                referrer = Some(value);
            }
            Some("--user-agent") => {
                if user_agent.is_some() {
                    return Err(ArgsError::Usage("--user-agent указан дважды"));
                }
                let value = args
                    .next()
                    .ok_or(ArgsError::Usage("у --user-agent нет значения"))?;
                user_agent = Some(value);
            }
            Some("-") => take_source(&mut source, argument)?,
            Some(flag) if flag.starts_with('-') => {
                return Err(ArgsError::Usage("неизвестный параметр"));
            }
            _ => take_source(&mut source, argument)?,
        }
    }

    let Some(source) = source else {
        return Err(ArgsError::Usage("не указан источник"));
    };
    if seek.is_some() && decode_frames.is_none() {
        return Err(ArgsError::Usage("--seek требует --decode-frames"));
    }

    let mut http = HttpOptions::new();
    if let Some(value) = referrer {
        http = http
            .with_referer(value.to_string_lossy())
            .map_err(ArgsError::Header)?;
    }
    if let Some(value) = user_agent {
        http = http
            .with_user_agent(value.to_string_lossy())
            .map_err(ArgsError::Header)?;
    }

    Ok(Action::Run {
        source,
        decode_frames,
        seek,
        volume,
        http,
    })
}

/// Запоминает источник; второй позиционный аргумент — ошибка.
fn take_source(slot: &mut Option<Source>, argument: OsString) -> Result<(), ArgsError> {
    if slot.is_some() {
        return Err(ArgsError::Usage("источник указан дважды"));
    }
    *slot = Some(Source::from_os_string(argument).map_err(ArgsError::Source)?);
    Ok(())
}

/// `--volume` задаётся процентами `0…100`, а ядро живёт в `0…1`.
fn parse_volume(argument: &OsString) -> Result<f32, ArgsError> {
    const INVALID: &str = "громкость задаётся числом от 0 до 100";

    let percent = argument
        .to_str()
        .and_then(|text| text.parse::<f32>().ok())
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
        .ok_or(ArgsError::Usage(INVALID))?;
    Ok(percent / 100.0)
}

/// Временной режим: декодирует кадры без звука и печатает скорость.
///
/// Нужен для приёмки этапа 3 (скорость декодирования) и этапа 6 (цена точной
/// перемотки).
fn decode_video(
    source: &Source,
    limit: usize,
    seek: Option<f64>,
    http: HttpOptions,
) -> anyhow::Result<()> {
    info!(source = %source, limit, seek, "декодирование кадров");

    let mut decoder = to_anyhow(VideoDecoder::open_source_with_options(source, http))?;
    println!("video: {}", describe_video(*decoder.info()));

    if let Some(seconds) = seek {
        let started = Instant::now();
        to_anyhow(decoder.seek(Duration::from_secs_f64(seconds)))?;
        println!("seek: {:.0?}", started.elapsed());
    }

    let started = Instant::now();
    let mut decoded = 0_usize;
    let mut first_pts = None;
    while decoded < limit {
        let Some(frame) = to_anyhow(decoder.next_frame())? else {
            break;
        };
        first_pts.get_or_insert(frame.pts);
        decoded += 1;
    }
    let elapsed = started.elapsed();

    if let Some(Some(pts)) = first_pts {
        println!("first frame: {pts:?}");
    }

    if decoded == 0 {
        println!("decoded: 0 frames");
        return Ok(());
    }

    let seconds = elapsed.as_secs_f64();
    let fps = if seconds > 0.0 {
        decoded as f64 / seconds
    } else {
        0.0
    };
    println!("decoded: {decoded} frames in {elapsed:.2?} ({fps:.1} fps)");
    Ok(())
}

fn play(source: &Source, volume: Option<f32>, http: HttpOptions) -> anyhow::Result<()> {
    // Открытие теперь асинхронное: `open` только запускает поток, а метаданные
    // и длительность приходят событием `Ready`.
    let player = to_anyhow(Player::open_with_options(source.clone(), http))?;

    // Громкость до `Play`: звуковое устройство откроется сразу на нужном
    // уровне, без слышимого скачка.
    if let Some(volume) = volume {
        to_anyhow(player.send(Command::SetVolume(volume)))?;
    }
    to_anyhow(player.send(Command::Play))?;
    info!(source = %source, volume = ?volume, "воспроизведение начато");

    // Позиция приходит на порядок чаще, чем меняется секунда, поэтому печатаем
    // только при смене секунды — иначе консоль превращается в ленту.
    let mut last_second = u64::MAX;
    let mut last_stats: Option<SyncStats> = None;
    let mut duration = None;

    loop {
        while let Some(event) = player.try_recv_event() {
            match event {
                Event::Ready(ready) => {
                    duration = ready.duration;
                    if ready.live {
                        println!("live: true");
                    }
                    if let Some(known) = ready.duration {
                        println!("duration: {}", format_clock(known));
                    }
                    if let Some(info) = ready.video_info {
                        println!("video: {}", describe_video(info));
                    }
                }
                Event::PositionChanged(position) => {
                    let seconds = position.as_secs();
                    if seconds != last_second {
                        last_second = seconds;
                        println!("position: {}", format_clock(position));
                    }
                }
                Event::Ended => {
                    if let Some(stats) = last_stats {
                        // Средняя частота за ролик точнее последнего интервала:
                        // на хвосте после последнего кадра она бывает занижена.
                        let fps = duration
                            .filter(|value| !value.is_zero())
                            .map_or(stats.fps, |value| {
                                (stats.shown as f64 / value.as_secs_f64()) as f32
                            });
                        println!(
                            "sync: {fps:.1} fps, shown {}, dropped {}, lag {} ms",
                            stats.shown,
                            stats.dropped,
                            stats.lag.as_millis()
                        );
                    }
                    info!("воспроизведение завершено");
                    println!("ended");
                    return Ok(());
                }
                Event::Error(message) => {
                    error!(%message, "ошибка воспроизведения");
                    return Err(anyhow::Error::msg(message));
                }
                Event::Stats(stats) => last_stats = Some(stats),
                // CLI не рисует визуализатор: уровни нужны только окну.
                Event::Spectrum(_) => {}
                // Ползунка в CLI нет: подтверждение перемотки не печатаем.
                Event::Seeked(_) => {}
                Event::Buffering(_) => {}
            }
        }

        // Очередь кадров ограничена: если её не забирать, ядро перестанет
        // отправлять новые кадры. В окне это делает опрос `VideoPlayer`.
        if let Some(frame) = player.try_recv_frame() {
            // В CLI кадры не показываются: буфер сразу возвращается в пул.
            player.recycle_frame(frame);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Время как `чч:мм:сс` — так же, как в панели плеера.
fn format_clock(value: Duration) -> String {
    let total_seconds = value.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Строка вида `1280x720 @ 30 fps`; целую частоту печатаем без дробей.
fn describe_video(info: VideoInfo) -> String {
    let mut description = format!("{}x{}", info.width, info.height);
    if let Some(fps) = info.frame_rate {
        let fps = if (fps - fps.round()).abs() < 0.01 {
            format!("{fps:.0}")
        } else {
            format!("{fps:.2}")
        };
        description.push_str(" @ ");
        description.push_str(&fps);
        description.push_str(" fps");
    }
    description
}

/// Подписчик логов: по умолчанию только предупреждения и ошибки, уровень
/// меняется переменной `RUST_LOG` (например, `RUST_LOG=player_core=info`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Переводит ошибку ядра в `anyhow`, сохраняя текст сообщения.
///
/// `PlayerError` остаётся типизированным внутри ядра; CLI — граница, где
/// подробности складываются в одну цепочку.
fn to_anyhow<T>(result: Result<T, PlayerError>) -> anyhow::Result<T> {
    result.map_err(|error| anyhow::Error::msg(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// Разобранный запуск без справки.
    fn run(items: &[&str]) -> (Source, Option<usize>, Option<f64>, Option<f32>) {
        let (source, decode_frames, seek, volume, _) = run_with_http(items);
        (source, decode_frames, seek, volume)
    }

    fn run_with_http(
        items: &[&str],
    ) -> (Source, Option<usize>, Option<f64>, Option<f32>, HttpOptions) {
        match parse_args(args(items)).expect("аргументы разобраны") {
            Action::Run {
                source,
                decode_frames,
                seek,
                volume,
                http,
            } => (source, decode_frames, seek, volume, http),
            Action::Help => panic!("ожидался запуск, а не справка"),
        }
    }

    #[test]
    fn source_is_required() {
        assert!(matches!(parse_args(args(&[])), Err(ArgsError::Usage(_))));
        assert!(matches!(
            parse_args(args(&["--volume", "10"])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn help_flags_win_over_other_arguments() {
        for flag in ["-h", "-H", "--help", "/?"] {
            assert_eq!(parse_args(args(&[flag])).expect("справка"), Action::Help);
        }
        assert_eq!(
            parse_args(args(&["movie.mp4", "--help"])).expect("справка"),
            Action::Help
        );
    }

    #[test]
    fn parses_path_url_and_timing_flags() {
        let (source, decode_frames, seek, volume) = run(&[
            r"C:\video\movie.mp4",
            "--decode-frames",
            "30",
            "--seek",
            "1.5",
        ]);

        assert_eq!(source, Source::File(PathBuf::from(r"C:\video\movie.mp4")));
        assert_eq!(decode_frames, Some(30));
        assert_eq!(seek, Some(1.5));
        assert_eq!(volume, None);

        let (source, _, _, _) = run(&["https://example.com/a.m3u8"]);
        assert_eq!(source, Source::Url("https://example.com/a.m3u8".to_owned()));
    }

    #[test]
    fn volume_can_go_before_or_after_the_source() {
        let (_, _, _, volume) = run(&["--volume", "30", "a.mp4"]);
        assert!((volume.expect("громкость") - 0.30).abs() < f32::EPSILON);

        let (_, _, _, volume) = run(&["a.mp4", "--volume", "0"]);
        assert_eq!(volume, Some(0.0));
    }

    #[test]
    fn volume_out_of_range_is_an_error() {
        for value in ["101", "-1", "abc", "NaN", "inf", ""] {
            assert!(
                matches!(
                    parse_args(args(&["a.mp4", "--volume", value])),
                    Err(ArgsError::Usage(_))
                ),
                "значение {value:?} должно быть отклонено"
            );
        }

        assert!(matches!(
            parse_args(args(&["a.mp4", "--volume"])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn seek_requires_decode_frames() {
        assert!(matches!(
            parse_args(args(&["a.mp4", "--seek", "10"])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn rejects_unknown_options_and_second_source() {
        assert!(matches!(
            parse_args(args(&["a.mp4", "--mute"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(args(&["a.mp4", "b.mp4"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(args(&["a.mp4", "--decode-frames", "0"])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn referrer_and_user_agent_are_parsed() {
        let (_, _, _, _, http) = run_with_http(&[
            "https://example.com/a.m3u8",
            "--referrer",
            "https://rutube.ru/",
            "--user-agent",
            "Mozilla/5.0",
        ]);

        assert_eq!(http.header("Referer"), Some("https://rutube.ru/"));
        assert_eq!(http.header("User-Agent"), Some("Mozilla/5.0"));
    }

    #[test]
    fn referrer_alias_is_accepted_and_duplicates_are_rejected() {
        let (_, _, _, _, http) = run_with_http(&["a.mp4", "--referer", "https://rutube.ru/"]);
        assert_eq!(http.header("Referer"), Some("https://rutube.ru/"));

        let cases: &[&[&str]] = &[
            &[
                "a.mp4",
                "--referrer",
                "https://a/",
                "--referer",
                "https://b/",
            ],
            &["a.mp4", "--user-agent", "one", "--user-agent", "two"],
            &["a.mp4", "--referrer"],
            &["a.mp4", "--user-agent"],
        ];
        for items in cases {
            assert!(
                matches!(parse_args(args(items)), Err(ArgsError::Usage(_))),
                "{items:?} должно быть отклонено"
            );
        }
    }

    #[test]
    fn invalid_header_value_is_rejected() {
        assert!(matches!(
            parse_args(args(&["a.mp4", "--user-agent", "bad\r\nvalue"])),
            Err(ArgsError::Header(_))
        ));
        assert!(matches!(
            parse_args(args(&["a.mp4", "--referrer", ""])),
            Err(ArgsError::Header(_))
        ));
    }

    #[test]
    fn help_mentions_every_option() {
        for option in [
            "<file|url>",
            "--decode-frames",
            "--seek",
            "--volume",
            "--referrer",
            "--referer",
            "--user-agent",
            "--help",
            "-h",
        ] {
            assert!(HELP.contains(option), "в справке нет {option}");
            assert!(USAGE.contains(option), "в usage нет {option}");
        }
    }
}
