//! Демонстрационное приложение: открывает окно плеера.
//!
//! Здесь намеренно нет логики — всё живёт в библиотеке `player-gpui`.
//! Приложение нужно, чтобы библиотеку было видно работающей и чтобы у
//! воркспейса был свой запускаемый бинарник.

// В release на Windows не показываем консольное окно: это GUI-приложение.
// В debug консоль остаётся, чтобы видеть stderr и логи.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::ffi::OsString;
use std::process::ExitCode;

use player_gpui::{Source, SourceError};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    init_tracing();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            error!(error = %format!("{failure:#}"), "player-app завершился с ошибкой");
            eprintln!("player-app: {failure:#}");
            ExitCode::FAILURE
        }
    }
}

/// Запускает окно. `anyhow` здесь — чтобы `main` оставался коротким, а цепочка
/// причин печаталась одним `{:#}`.
fn run() -> anyhow::Result<()> {
    let source = match source_argument(std::env::args_os().skip(1)) {
        Ok(source) => source,
        Err(error) => return Err(anyhow::Error::new(error)),
    };
    match source.as_ref() {
        Some(source) => info!(source = %source, "запуск плеера"),
        None => info!("запуск плеера без источника"),
    }

    player_gpui::run_app_source(source)?;
    Ok(())
}

/// Источник из командной строки: первый аргумент без обрамляющих кавычек.
///
/// Так Windows запускает плеер по ассоциации — из «Открыть с помощью» и по
/// двойному клику в Проводнике: `player-app.exe "C:\video\movie.mp4"`.
/// Парсер командной строки Rust кавычки обычно снимает сам; повторное снятие
/// нужно для лаунчеров, которые передают строку целиком и экранируют её сами.
/// `https://…` распознаётся как URL, а не как путь.
fn source_argument(
    args: impl IntoIterator<Item = OsString>,
) -> Result<Option<Source>, SourceError> {
    let Some(argument) = args.into_iter().next() else {
        return Ok(None);
    };
    let text = argument.to_string_lossy();
    let trimmed = text.trim_matches('"');
    if trimmed.is_empty() {
        return Ok(None);
    }

    // Без кавычек отдаём исходный `OsString`: `to_string_lossy` испортил бы
    // путь с символами вне Unicode.
    let value = if trimmed.len() == text.len() {
        argument
    } else {
        OsString::from(trimmed)
    };
    Source::from_os_string(value).map(Some)
}

/// Подписчик логов: по умолчанию только предупреждения и ошибки, уровень
/// меняется переменной `RUST_LOG` (например, `RUST_LOG=info`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::source_argument;
    use player_gpui::Source;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn takes_the_file_passed_by_the_shell() {
        let source = source_argument(args(&[r"C:\video\movie.mp4"])).expect("путь");

        assert_eq!(
            source,
            Some(Source::File(PathBuf::from(r"C:\video\movie.mp4")))
        );
    }

    #[test]
    fn strips_quotes_around_a_path_with_spaces() {
        let source = source_argument(args(&[r#""C:\my video\a b.mkv""#])).expect("путь");

        assert_eq!(
            source,
            Some(Source::File(PathBuf::from(r"C:\my video\a b.mkv")))
        );
    }

    #[test]
    fn url_is_not_treated_as_a_path() {
        let source =
            source_argument(args(&["https://example.com/video.mp4?token=1"])).expect("URL");

        assert_eq!(
            source,
            Some(Source::Url(
                "https://example.com/video.mp4?token=1".to_owned()
            ))
        );
    }

    #[test]
    fn launch_without_a_file_gives_nothing() {
        assert_eq!(source_argument(args(&[])).expect("пусто"), None);
        assert_eq!(source_argument(args(&[""])).expect("пусто"), None);
        assert_eq!(source_argument(args(&[r#"""""#])).expect("пусто"), None);
    }
}
