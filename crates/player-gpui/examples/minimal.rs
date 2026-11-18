//! Минимальный пример: готовое окно плеера из `run_app`.
//!
//! Запуск:
//!
//! ```bash
//! cargo run -p player-gpui --example minimal -- assets/test/sync_test.mp4
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    match player_gpui::run_app(path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("minimal: {error}");
            ExitCode::FAILURE
        }
    }
}
