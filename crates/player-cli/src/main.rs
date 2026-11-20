//! Консольный запуск ядра: открыть файл, проиграть его, напечатать события.
//!
//! Аргументы разбираются вручную: у CLI по-прежнему мало параметров, `clap`
//! появится вместе с диалогом открытия и плейлистом на этапе 9.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use player_core::{
    Command, Event, Player, PlayerError, Source, SyncStats, VideoDecoder, VideoInfo,
};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Строка подсказки — одна для всех ошибок разбора аргументов.
const USAGE: &str = "usage: player-cli <file|url> [--decode-frames <n> [--seek <seconds>]]";

fn main() -> ExitCode {
    init_tracing();

    let mut args = std::env::args_os().skip(1);
    let Some(argument) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let source = match Source::from_os_string(argument) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("player-cli: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut decode_frames = None;
    let mut seek = None;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--decode-frames") => {
                let limit = args
                    .next()
                    .and_then(|value| value.to_str()?.parse::<usize>().ok())
                    .filter(|limit| *limit > 0);
                let Some(limit) = limit else {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                };
                decode_frames = Some(limit);
            }
            Some("--seek") => {
                let seconds = args
                    .next()
                    .and_then(|value| value.to_str()?.parse::<f64>().ok())
                    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0);
                let Some(seconds) = seconds else {
                    eprintln!("{USAGE}");
                    return ExitCode::FAILURE;
                };
                seek = Some(seconds);
            }
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    if seek.is_some() && decode_frames.is_none() {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    }

    let result = match decode_frames {
        Some(limit) => decode_video(&source, limit, seek),
        None => play(&source),
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

/// Временной режим: декодирует кадры без звука и печатает скорость.
///
/// Нужен для приёмки этапа 3 (скорость декодирования) и этапа 6 (цена точной
/// перемотки).
fn decode_video(source: &Source, limit: usize, seek: Option<f64>) -> anyhow::Result<()> {
    info!(source = %source, limit, seek, "декодирование кадров");

    let mut decoder = to_anyhow(VideoDecoder::open_source(source))?;
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

fn play(source: &Source) -> anyhow::Result<()> {
    // Открытие теперь асинхронное: `open` только запускает поток, а метаданные
    // и длительность приходят событием `Ready`.
    let player = to_anyhow(Player::open(source.clone()))?;

    to_anyhow(player.send(Command::Play))?;
    info!(source = %source, "воспроизведение начато");

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
