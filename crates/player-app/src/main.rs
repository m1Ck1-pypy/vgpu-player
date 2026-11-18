//! Демонстрационное приложение: открывает окно плеера.
//!
//! Здесь намеренно нет логики — всё живёт в библиотеке `player-gpui`.
//! Приложение нужно, чтобы библиотеку было видно работающей и чтобы у
//! воркспейса был свой запускаемый бинарник.

// В release на Windows не показываем консольное окно: это GUI-приложение.
// В debug консоль остаётся, чтобы видеть stderr и логи.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

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
    // Файл передаётся первым аргументом; диалог выбора появится на этапе 9.
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    match path.as_ref() {
        Some(path) => info!(path = %path.display(), "запуск плеера"),
        None => info!("запуск плеера без файла"),
    }

    player_gpui::run_app(path)?;
    Ok(())
}

/// Подписчик логов: по умолчанию только предупреждения и ошибки, уровень
/// меняется переменной `RUST_LOG` (например, `RUST_LOG=info`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
