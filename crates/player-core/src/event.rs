//! События ядра для UI.
//!
//! UI не опрашивает состояние, а разбирает поток событий: так перерисовка
//! происходит только при реальном изменении (правило из `RULES.md`).

use std::time::Duration;

use crate::SyncStats;

/// Что ядро сообщает UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Известна длительность файла.
    DurationKnown(Duration),
    /// Позиция воспроизведения изменилась (этап 2).
    PositionChanged(Duration),
    /// Статистика синхронизации видео (этап 5).
    Stats(SyncStats),
    /// Файл доигран до конца.
    Ended,
    /// Ошибка во время воспроизведения; текст готов к показу.
    Error(String),
}
