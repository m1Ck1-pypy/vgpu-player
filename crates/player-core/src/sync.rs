//! Синхронизация видео с ведущими часами.
//!
//! Ведущие часы — звук: позиция считается по сэмплам, которые устройство уже
//! забрало. Ядро решает, когда отправить кадр в UI:
//!
//! - рано — кадр ждёт следующего тика;
//! - пора — отправляем;
//! - опоздал — пропускаем, чтобы догнать звук.
//!
//! Логика вынесена из потока ядра, чтобы её можно было проверить без окна,
//! звукового устройства и файлов.

use std::time::{Duration, Instant};

/// Насколько кадр может уйти вперёд часов и всё ещё считаться своевременным.
///
/// Небольшой запас компенсирует шаг опроса (10 мс) и делает картинку чуть
/// впереди звука, что воспринимается лучше опоздания.
const EARLY_TOLERANCE: Duration = Duration::from_millis(10);

/// Насколько кадр может опоздать, прежде чем его пропустят.
///
/// Больше кадра при 30 fps: один пропуск не должен превращаться в серию.
const LATE_TOLERANCE: Duration = Duration::from_millis(50);

/// Интервал кадра, если контейнер не сообщил частоту.
const FALLBACK_FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Что делать с уже декодированным кадром.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameAction {
    /// Показать сейчас.
    Show,
    /// Подождать: кадр ещё рано показывать.
    Wait,
    /// Пропустить: кадр безнадёжно опоздал.
    Skip,
}

/// Решает, когда показывать кадры.
#[derive(Debug, Clone)]
pub(crate) struct FrameScheduler {
    /// PTS, оценённый для предыдущего кадра без метки времени.
    last_pts: Option<Duration>,
    /// Номинальный интервал между кадрами.
    frame_interval: Duration,
}

impl FrameScheduler {
    /// Создаёт планировщик для потока с указанной частотой кадров.
    pub(crate) fn new(frame_rate: Option<f64>) -> Self {
        let frame_interval = frame_rate
            .filter(|fps| fps.is_finite() && *fps > 0.0)
            .map(|fps| Duration::from_secs_f64(1.0 / fps))
            .unwrap_or(FALLBACK_FRAME_INTERVAL);

        Self {
            last_pts: None,
            frame_interval,
        }
    }

    /// Готовит планировщик к работе с новой позиции.
    pub(crate) fn reset(&mut self) {
        self.last_pts = None;
    }

    /// Возвращает PTS кадра; для кадров без метки времени оценивает его.
    ///
    /// Вызывается один раз на кадр, при декодировании: повторный вызов на
    /// каждом тике сдвигал бы оценку и приводил к ложным пропускам.
    pub(crate) fn frame_pts(&mut self, pts: Option<Duration>) -> Duration {
        let pts = pts.unwrap_or_else(|| {
            self.last_pts.map_or(Duration::ZERO, |last| {
                last.saturating_add(self.frame_interval)
            })
        });
        self.last_pts = Some(pts);
        pts
    }

    /// Решает судьбу кадра в текущей позиции часов.
    pub(crate) fn decide(pts: Duration, position: Duration) -> FrameAction {
        if pts.saturating_add(LATE_TOLERANCE) < position {
            FrameAction::Skip
        } else if pts > position.saturating_add(EARLY_TOLERANCE) {
            FrameAction::Wait
        } else {
            FrameAction::Show
        }
    }
}

/// Запасной таймер для видео без звука.
///
/// Позиция — база плюс время, прошедшее с момента запуска. Пауза замораживает
/// счёт, перемотка меняет базу. Всё время передаётся параметром, поэтому
/// логика проверяется без `sleep`.
#[derive(Debug, Clone, Default)]
pub(crate) struct VideoTimer {
    base: Duration,
    running_since: Option<Instant>,
}

impl VideoTimer {
    /// Создаёт остановленный таймер в нулевой позиции.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Переводит таймер в новую позицию, сохраняя состояние запуска.
    pub(crate) fn rebase(&mut self, position: Duration, now: Instant) {
        self.base = position;
        if self.running_since.is_some() {
            self.running_since = Some(now);
        }
    }

    /// Запускает счёт, если он ещё не идёт.
    pub(crate) fn start(&mut self, now: Instant) {
        if self.running_since.is_none() {
            self.running_since = Some(now);
        }
    }

    /// Останавливает счёт, запоминая пройденное время.
    pub(crate) fn pause(&mut self, now: Instant) {
        if let Some(since) = self.running_since.take() {
            self.base = self
                .base
                .saturating_add(now.saturating_duration_since(since));
        }
    }

    /// Текущая позиция таймера.
    pub(crate) fn position(&self, now: Instant) -> Duration {
        match self.running_since {
            Some(since) => self
                .base
                .saturating_add(now.saturating_duration_since(since)),
            None => self.base,
        }
    }
}

/// Статистика синхронизации для оверлея.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncStats {
    /// Сколько кадров отправлено в UI за всё время.
    pub shown: u64,
    /// Сколько кадров пропущено как запоздавшие.
    pub dropped: u64,
    /// Насколько последний кадр отставал от часов (ноль, если он впереди).
    pub lag: Duration,
    /// Частота показа с последнего отчёта.
    pub fps: f32,
    /// Сеть: есть только у URL-источников.
    ///
    /// Пропуски кадров на сети — следствие блокирующих чтений, поэтому рядом
    /// с `dropped` полезно видеть, сколько было запросов и медленных чтений.
    pub network: Option<NetworkStats>,
}

/// Счётчики сетевого читателя с начала открытия.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkStats {
    /// Сколько HTTP-запросов сделал читатель (включая проверочный).
    pub requests: u64,
    /// Сколько байт прочитано из сети.
    pub bytes: u64,
    /// Сколько сетевых чтений длились дольше 100 мс и сколько всего времени
    /// они заняли: по этим двум числам видно, стоят ли `drop` сетевого ожидания.
    pub slow_reads: u64,
    /// Суммарное время медленных чтений в миллисекундах.
    pub slow_ms: u64,
    /// Сколько раз запрос повторяли после ошибки.
    pub retries: u64,
    /// Сколько раз поток перезапускали с начала (источник без `Range`).
    pub restarts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn frame_in_window_is_shown() {
        assert_eq!(FrameScheduler::decide(ms(100), ms(100)), FrameAction::Show);
        assert_eq!(FrameScheduler::decide(ms(105), ms(100)), FrameAction::Show);
        assert_eq!(FrameScheduler::decide(ms(95), ms(100)), FrameAction::Show);
    }

    #[test]
    fn early_frame_waits() {
        assert_eq!(FrameScheduler::decide(ms(200), ms(100)), FrameAction::Wait);
    }

    #[test]
    fn late_frame_is_skipped() {
        assert_eq!(FrameScheduler::decide(ms(100), ms(200)), FrameAction::Skip);
    }

    #[test]
    fn missing_pts_is_estimated_from_frame_rate() {
        let mut scheduler = FrameScheduler::new(Some(25.0));

        assert_eq!(scheduler.frame_pts(None), ms(0));
        assert_eq!(scheduler.frame_pts(None), ms(40));
        assert_eq!(scheduler.frame_pts(None), ms(80));
    }

    #[test]
    fn real_pts_resets_estimate() {
        let mut scheduler = FrameScheduler::new(Some(10.0));

        assert_eq!(scheduler.frame_pts(None), ms(0));
        assert_eq!(scheduler.frame_pts(Some(ms(500))), ms(500));
        assert_eq!(scheduler.frame_pts(None), ms(600));
    }

    #[test]
    fn timer_counts_only_while_running() {
        let start = Instant::now();
        let mut timer = VideoTimer::new();

        timer.start(start);
        assert_eq!(timer.position(start + ms(500)), ms(500));

        timer.pause(start + ms(500));
        assert_eq!(timer.position(start + ms(900)), ms(500));

        timer.start(start + ms(900));
        assert_eq!(timer.position(start + ms(1_100)), ms(700));
    }

    #[test]
    fn timer_rebase_keeps_running_state() {
        let start = Instant::now();
        let mut timer = VideoTimer::new();
        timer.start(start);

        timer.rebase(ms(5_000), start + ms(200));
        assert_eq!(timer.position(start + ms(700)), ms(5_500));

        timer.pause(start + ms(700));
        timer.rebase(ms(9_000), start + ms(800));
        assert_eq!(timer.position(start + ms(2_000)), ms(9_000));
    }
}
