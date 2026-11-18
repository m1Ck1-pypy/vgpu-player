//! Аудио-часы.
//!
//! Позиция воспроизведения — это не время на настенных часах и не счётчик
//! кадров декодера, а число сэмплов, которые устройство реально забрало из
//! очереди. Поэтому часы не «убегают» при подвисаниях декодера, останавливаются
//! на паузе и служат опорой для видео на этапе 5.
//!
//! Счётчик общий с источником звука: `Arc<AtomicU64>` увеличивает поток вывода,
//! а читает поток ядра. Атомик с `Relaxed` достаточно: нам нужна только
//! монотонность одного значения, без синхронизации других данных.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Количество сэмплов, отданных в устройство, плюс база отсчёта.
#[derive(Debug, Clone)]
pub struct AudioClock {
    delivered: Arc<AtomicU64>,
    base_frames: u64,
    channels: u64,
    sample_rate: u64,
}

impl AudioClock {
    /// Создаёт часы для потока с заданным числом каналов и частотой.
    pub(crate) fn new(channels: u16, sample_rate: u32) -> Self {
        Self {
            delivered: Arc::new(AtomicU64::new(0)),
            base_frames: 0,
            channels: u64::from(channels.max(1)),
            sample_rate: u64::from(sample_rate.max(1)),
        }
    }

    /// Хендл счётчика для источника звука.
    pub(crate) fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.delivered)
    }

    /// Начинает отсчёт с новой позиции.
    ///
    /// Вызывается при открытии файла и при перемотке: всё, что было отдано до
    /// этого момента, к новой позиции отношения не имеет.
    pub(crate) fn rebase(&mut self, position: Duration) {
        self.base_frames = frames_of(position, self.sample_rate);
        self.delivered.store(0, Ordering::Relaxed);
    }

    /// Текущая позиция воспроизведения.
    pub fn position(&self) -> Duration {
        let delivered_frames = self.delivered.load(Ordering::Relaxed) / self.channels;
        let frames = self.base_frames.saturating_add(delivered_frames);
        Duration::from_secs_f64(frames as f64 / self.sample_rate as f64)
    }
}

/// Сколько кадров соответствует позиции.
fn frames_of(position: Duration, sample_rate: u64) -> u64 {
    let seconds = position.as_secs_f64();
    let frames = seconds * sample_rate as f64;
    if frames.is_finite() && frames > 0.0 {
        frames as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_starts_at_zero() {
        let clock = AudioClock::new(2, 48_000);
        assert_eq!(clock.position(), Duration::ZERO);
    }

    #[test]
    fn position_counts_delivered_frames() {
        let clock = AudioClock::new(2, 1_000);
        let counter = clock.counter();

        // 2000 сэмплов при двух каналах — это 1000 кадров, то есть одна секунда.
        counter.fetch_add(2_000, Ordering::Relaxed);

        assert_eq!(clock.position(), Duration::from_secs(1));
    }

    #[test]
    fn rebase_moves_position_and_resets_counter() {
        let mut clock = AudioClock::new(1, 1_000);
        let counter = clock.counter();
        counter.fetch_add(500, Ordering::Relaxed);
        assert_eq!(clock.position(), Duration::from_millis(500));

        clock.rebase(Duration::from_secs(10));
        assert_eq!(clock.position(), Duration::from_secs(10));

        counter.fetch_add(250, Ordering::Relaxed);
        assert_eq!(clock.position(), Duration::from_millis(10_250));
    }

    #[test]
    fn counter_survives_clock_clone() {
        let clock = AudioClock::new(1, 1_000);
        let counter = clock.counter();
        let observer = clock.clone();

        counter.fetch_add(1_000, Ordering::Relaxed);

        assert_eq!(observer.position(), Duration::from_secs(1));
    }
}
