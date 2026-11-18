//! Спектроанализатор для визуализации звука.
//!
//! Уровни столбиков считает ядро, а UI только рисует готовые числа: DSP не
//! попадает в поток отрисовки, и событие `Spectrum` живёт по тем же правилам,
//! что остальные события ядра.
//!
//! Сэмплы берутся не у декодера, а у источника вывода: декодер убегает вперёд
//! на несколько блоков, и картинка прыгала бы раньше звука. [`SpectrumTap`]
//! оборачивает `rodio`-источник, сводит каналы в моно и пишет их в
//! [`SpectrumRing`] — кольцевой буфер без блокировок. Движок читает оттуда
//! новые сэмплы и раз в 33 мс отдаёт [`SpectrumBars`].
//!
//! Шкала — логарифмическая (40 Гц … 16 кГц), к магнитудам добавляется наклон
//! [`TILT_DB_PER_OCTAVE`]: без него верх спектра у музыки выглядит пустым.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use rodio::{ChannelCount, Sample, SampleRate, Source};

/// Сколько столбиков приходит в одном событии.
pub const SPECTRUM_BARS: usize = 48;

/// Размер окна FFT. Степень двойки — требование радикс-2.
///
/// 2048 точек при 48 кГц дают разрешение ~23 Гц и задержку ~43 мс: достаточно,
/// чтобы различить низкие частоты, и незаметно на глаз.
const FFT_SIZE: usize = 2048;

/// Нижняя граница логарифмической шкалы: ниже в музыке почти ничего нет.
const MIN_FREQ_HZ: f32 = 40.0;

/// Верхняя граница: выше 16 кГц остаётся только шум.
const MAX_FREQ_HZ: f32 = 16_000.0;

/// Наклон частотной компенсации в дБ на октаву.
///
/// Музыка спадает примерно на 6 дБ/октаву; +4.5 дБ/октаву выравнивает
/// картинку, не выпячивая высокие частоты. Отсчёт — от [`TILT_REFERENCE_HZ`].
const TILT_DB_PER_OCTAVE: f32 = 4.5;

/// Частота, на которой компенсация равна нулю.
const TILT_REFERENCE_HZ: f32 = 1_000.0;

/// Нижняя граница отображения в дБFS и верхняя (полная шкала).
const MIN_DB: f32 = -80.0;
const MAX_DB: f32 = 0.0;

/// Доля шага сглаживания за одно обновление: быстрый подъём, медленный спад.
const ATTACK: f32 = 0.6;
const RELEASE: f32 = 0.15;

/// Амплитуда, ниже которой считаем сигнал тишиной (логарифм нуля не берём).
const SILENCE_AMPLITUDE: f32 = 1e-6;

/// Ёмкость кольца в сэмплах: ~170 мс при 48 кГц.
///
/// Движок читает чаще, поэтому такого запаса хватает, а при переполнении
/// читатель просто пропускает старое — картинке нужен свежий звук.
pub(crate) const SAMPLE_RING_CAPACITY: usize = 8_192;

/// Готовые уровни столбиков: `0.0` — тишина, `1.0` — полная шкала.
///
/// Массив фиксированной длины: событие не выделяет память и копируется целиком.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectrumBars([f32; SPECTRUM_BARS]);

impl SpectrumBars {
    /// Все столбики на нуле: тишина, конец файла, сброс после перемотки.
    pub const ZERO: Self = Self([0.0; SPECTRUM_BARS]);

    /// Уровни по порядку: от низких частот к высоким.
    pub fn levels(&self) -> &[f32; SPECTRUM_BARS] {
        &self.0
    }
}

/// Диапазон бинов FFT и центральная частота одной полосы.
#[derive(Debug, Clone, Copy)]
struct Band {
    lo_bin: usize,
    hi_bin: usize,
    center_hz: f32,
}

/// Считает уровни столбиков из потока моно-сэмплов.
///
/// Живёт в потоке ядра: снаружи видны только [`Self::push`], [`Self::update`]
/// и [`Self::reset`].
pub(crate) struct SpectrumAnalyzer {
    /// Последние `FFT_SIZE` моно-сэмплов, самые новые — в конце.
    window: Vec<f32>,
    hann: Vec<f32>,
    hann_sum: f32,
    re: Vec<f32>,
    im: Vec<f32>,
    bands: [Band; SPECTRUM_BARS],
    levels: [f32; SPECTRUM_BARS],
}

impl SpectrumAnalyzer {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let hann = hann_window(FFT_SIZE);
        let hann_sum = hann.iter().sum::<f32>().max(f32::EPSILON);

        Self {
            window: vec![0.0; FFT_SIZE],
            hann,
            hann_sum,
            re: vec![0.0; FFT_SIZE],
            im: vec![0.0; FFT_SIZE],
            bands: band_ranges(sample_rate),
            levels: [0.0; SPECTRUM_BARS],
        }
    }

    /// Добавляет новые моно-сэмплы в скользящее окно.
    pub(crate) fn push(&mut self, samples: &[f32]) {
        if samples.len() >= FFT_SIZE {
            self.window
                .copy_from_slice(&samples[samples.len() - FFT_SIZE..]);
            return;
        }

        let shift = samples.len();
        self.window.copy_within(shift.., 0);
        self.window[FFT_SIZE - shift..].copy_from_slice(samples);
    }

    /// Считает следующий кадр столбиков и применяет сглаживание.
    ///
    /// Вызывается ровно один раз на отправленное событие, поэтому сглаживание
    /// не зависит от того, как часто движок успевает заглянуть в кольцо.
    pub(crate) fn update(&mut self) -> SpectrumBars {
        for ((re, im), (sample, hann)) in self
            .re
            .iter_mut()
            .zip(self.im.iter_mut())
            .zip(self.window.iter().zip(&self.hann))
        {
            *re = sample * hann;
            *im = 0.0;
        }

        fft(&mut self.re, &mut self.im);

        let scale = 2.0 / self.hann_sum;
        for (level, band) in self.levels.iter_mut().zip(&self.bands) {
            let peak = self.re[band.lo_bin..band.hi_bin]
                .iter()
                .zip(&self.im[band.lo_bin..band.hi_bin])
                .fold(0.0_f32, |peak, (&re, &im)| peak.max(re.hypot(im)));
            let amplitude = peak * scale;
            let db = 20.0 * amplitude.max(SILENCE_AMPLITUDE).log10() + tilt_db(band.center_hz);
            let target = ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0);

            let previous = *level;
            let step = if target > previous { ATTACK } else { RELEASE };
            *level = previous + (target - previous) * step;
        }

        SpectrumBars(self.levels)
    }

    /// Забывает накопленное окно и текущие уровни (открытие файла, перемотка).
    pub(crate) fn reset(&mut self) {
        self.window.fill(0.0);
        self.levels.fill(0.0);
    }
}

/// Компенсация наклона спектра в дБ для частоты `frequency_hz`.
fn tilt_db(frequency_hz: f32) -> f32 {
    let frequency = frequency_hz.max(1.0);
    TILT_DB_PER_OCTAVE * (frequency / TILT_REFERENCE_HZ).log2()
}

/// Раскладывает диапазон на логарифмические полосы и переводит их в бины.
fn band_ranges(sample_rate: u32) -> [Band; SPECTRUM_BARS] {
    let rate = sample_rate.max(1) as f32;
    let nyquist = rate / 2.0;
    let top_hz = MAX_FREQ_HZ
        .min(nyquist)
        .max(MIN_FREQ_HZ * 2.0)
        .max(MIN_FREQ_HZ);
    let ratio = (top_hz / MIN_FREQ_HZ).powf(1.0 / SPECTRUM_BARS as f32);
    let bin_width = rate / FFT_SIZE as f32;
    let bins = FFT_SIZE / 2;

    let mut bands = [Band {
        lo_bin: 1,
        hi_bin: 2,
        center_hz: MIN_FREQ_HZ,
    }; SPECTRUM_BARS];
    let mut low_hz = MIN_FREQ_HZ;

    for band in &mut bands {
        let high_hz = (low_hz * ratio).min(top_hz);
        let center_hz = (low_hz * high_hz).sqrt();
        // Округление, а не floor/ceil: иначе соседние полосы забирают один и
        // тот же бин, и тон на границе двоится в двух столбиках.
        let lo_bin = ((low_hz / bin_width).round() as usize).clamp(1, bins - 1);
        let hi_bin = ((high_hz / bin_width).round() as usize).clamp(lo_bin + 1, bins);

        *band = Band {
            lo_bin,
            hi_bin,
            center_hz,
        };
        low_hz = high_hz;
    }

    bands
}

/// Окно Ханна: убирает растекание спектра по соседним бинам.
///
/// Периодическая форма (делим на `size`, а не на `size - 1`) — стандарт для
/// FFT: сумма коэффициентов ровно `size / 2`, по ней же нормируем амплитуду.
fn hann_window(size: usize) -> Vec<f32> {
    (0..size)
        .map(|index| {
            let phase = 2.0 * std::f32::consts::PI * index as f32 / size as f32;
            0.5 - 0.5 * phase.cos()
        })
        .collect()
}

/// Быстрое преобразование Фурье на месте (radix-2, итеративное).
///
/// Вход — два массива длины-степени двойки: действительная и мнимая части.
/// Классическая схема: бит-реверс перестановка, затем «бабочки» с поворотными
/// множителями для длин 2, 4, 8, … `n`.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert_eq!(im.len(), n);
    debug_assert!(n.is_power_of_two());

    // Переставляем элементы в порядке бит-реверса индекса.
    let mut reversed = 0;
    for index in 1..n {
        let mut bit = n >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed |= bit;
        if index < reversed {
            re.swap(index, reversed);
            im.swap(index, reversed);
        }
    }

    // Каждый проход объединяет пары блоков длины len / 2.
    let mut len = 2;
    while len <= n {
        let angle = -2.0 * std::f32::consts::PI / len as f32;
        let (twiddle_re, twiddle_im) = (angle.cos(), angle.sin());
        let half = len / 2;

        for start in (0..n).step_by(len) {
            let (mut weight_re, mut weight_im) = (1.0_f32, 0.0_f32);
            for offset in 0..half {
                let even = start + offset;
                let odd = even + half;

                let odd_re = re[odd] * weight_re - im[odd] * weight_im;
                let odd_im = re[odd] * weight_im + im[odd] * weight_re;

                re[odd] = re[even] - odd_re;
                im[odd] = im[even] - odd_im;
                re[even] += odd_re;
                im[even] += odd_im;

                let next_re = weight_re * twiddle_re - weight_im * twiddle_im;
                weight_im = weight_re * twiddle_im + weight_im * twiddle_re;
                weight_re = next_re;
            }
        }

        len <<= 1;
    }
}

/// Кольцо моно-сэмплов между потоком вывода и потоком ядра.
///
/// Один писатель, один читатель. Писатель кладёт сэмпл и публикует номер
/// записи с `Release`; читатель берёт номер с `Acquire` и читает сэмплы.
/// Блокировок нет, поэтому поток вывода не может застрять из-за ядра.
pub(crate) struct SpectrumRing {
    samples: Box<[AtomicU32]>,
    written: AtomicU64,
}

impl SpectrumRing {
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: std::iter::repeat_with(|| AtomicU32::new(0))
                .take(capacity)
                .collect(),
            written: AtomicU64::new(0),
        }
    }

    /// Номер следующей записи: с него начинается «свежий» звук.
    pub(crate) fn latest(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// Забирает всё, что записано после `cursor`, в `out`.
    ///
    /// Если писатель успел обогнать читателя на целое кольцо, старые сэмплы
    /// пропускаются: визуализатору важен текущий звук, а не потерянные блоки.
    pub(crate) fn read(&self, cursor: &mut u64, out: &mut Vec<f32>) -> usize {
        let written = self.written.load(Ordering::Acquire);
        let capacity = self.samples.len() as u64;
        let start = (*cursor).max(written.saturating_sub(capacity));
        *cursor = written;

        out.clear();
        if start >= written {
            return 0;
        }

        out.reserve((written - start) as usize);
        for position in start..written {
            let index = (position % capacity) as usize;
            out.push(f32::from_bits(self.samples[index].load(Ordering::Relaxed)));
        }

        (written - start) as usize
    }

    fn push(&self, sample: f32) {
        let position = self.written.load(Ordering::Relaxed);
        let index = (position % self.samples.len() as u64) as usize;
        self.samples[index].store(sample.to_bits(), Ordering::Relaxed);
        self.written.store(position + 1, Ordering::Release);
    }
}

/// Источник звука, который попутно отдаёт моно-сэмплы в кольцо.
///
/// Необязательное кольцо: для видео с дорожкой звука анализ не нужен, и тап
/// не делает лишней работы. Каналы сводятся усреднением — так стерео и моно
/// дают одинаковую шкалу.
pub(crate) struct SpectrumTap<S> {
    inner: S,
    ring: Option<Arc<SpectrumRing>>,
    channels: usize,
    sum: f32,
    filled: usize,
}

impl<S: Source<Item = Sample>> SpectrumTap<S> {
    pub(crate) fn new(inner: S, ring: Option<Arc<SpectrumRing>>) -> Self {
        let channels = usize::from(inner.channels().get()).max(1);
        Self {
            inner,
            ring,
            channels,
            sum: 0.0,
            filled: 0,
        }
    }
}

impl<S: Source<Item = Sample>> Iterator for SpectrumTap<S> {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;
        if let Some(ring) = self.ring.as_ref() {
            self.sum += sample;
            self.filled += 1;
            if self.filled >= self.channels {
                ring.push(self.sum / self.channels as f32);
                self.sum = 0.0;
                self.filled = 0;
            }
        }
        Some(sample)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Source<Item = Sample>> Source for SpectrumTap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::num::{NonZeroU16, NonZeroU32};

    use rodio::buffer::SamplesBuffer;

    /// Наивная DFT: эталон для проверки FFT, O(n²) здесь допустимо.
    fn naive_dft(samples: &[f32]) -> Vec<(f32, f32)> {
        let n = samples.len();
        (0..n)
            .map(|k| {
                let mut re = 0.0;
                let mut im = 0.0;
                for (index, sample) in samples.iter().enumerate() {
                    let angle = -2.0 * PI * (k * index) as f32 / n as f32;
                    re += sample * angle.cos();
                    im += sample * angle.sin();
                }
                (re, im)
            })
            .collect()
    }

    fn sine(frequency: f32, sample_rate: f32, size: usize) -> Vec<f32> {
        (0..size)
            .map(|index| (2.0 * PI * frequency * index as f32 / sample_rate).sin())
            .collect()
    }

    fn peak_index(levels: &[f32; SPECTRUM_BARS]) -> usize {
        let mut best = 0;
        for (index, level) in levels.iter().enumerate() {
            if *level > levels[best] {
                best = index;
            }
        }
        best
    }

    #[test]
    fn fft_matches_naive_dft() {
        let samples: Vec<f32> = (0..16)
            .map(|index| (index as f32 * 0.7).sin() + 0.3 * (index as f32 * 1.9).cos())
            .collect();
        let expected = naive_dft(&samples);

        let mut re = samples.clone();
        let mut im = vec![0.0; samples.len()];
        fft(&mut re, &mut im);

        for ((&re, &im), &(expected_re, expected_im)) in re.iter().zip(&im).zip(expected.iter()) {
            assert!(
                (re - expected_re).abs() < 1e-3 && (im - expected_im).abs() < 1e-3,
                "бин: получено ({re}, {im}), ожидалось ({expected_re}, {expected_im})"
            );
        }
    }

    #[test]
    fn log_bands_cover_the_range() {
        let bands = band_ranges(48_000);
        let bin_width = 48_000.0 / FFT_SIZE as f32;
        let first = (MIN_FREQ_HZ / bin_width).round() as usize;
        let top = (MAX_FREQ_HZ / bin_width).round() as usize;

        assert_eq!(bands[0].lo_bin, first, "низ шкалы не на 40 Гц");
        assert_eq!(
            bands[SPECTRUM_BARS - 1].hi_bin,
            top,
            "верх шкалы не на 16 кГц"
        );

        for pair in bands.windows(2) {
            assert!(
                pair[0].center_hz < pair[1].center_hz,
                "полосы идут по возрастанию"
            );
            assert!(
                pair[0].hi_bin <= pair[1].hi_bin,
                "полосы не должны перемешиваться"
            );
        }

        for band in &bands {
            assert!(band.lo_bin < band.hi_bin, "полоса должна содержать бин");
            assert!(band.hi_bin <= FFT_SIZE / 2, "бин за пределами спектра");
        }
    }

    #[test]
    fn tilt_is_zero_at_the_reference_and_grows_with_frequency() {
        assert!(tilt_db(TILT_REFERENCE_HZ).abs() < 1e-6);
        assert!(tilt_db(8_000.0) > tilt_db(2_000.0));
        assert!(tilt_db(100.0) < 0.0);
    }

    #[test]
    fn silence_gives_zero_bars() {
        let mut analyzer = SpectrumAnalyzer::new(48_000);
        analyzer.push(&vec![0.0; FFT_SIZE]);

        let bars = analyzer.update();
        assert_eq!(*bars.levels(), [0.0; SPECTRUM_BARS]);
    }

    #[test]
    fn a_sine_lands_in_the_band_that_contains_it() {
        let sample_rate = 48_000;
        let bands = band_ranges(sample_rate);
        // Центр полосы гарантированно далеко от границ: растекание Hann
        // (соседние бины) не перекинет пик в другую полосу.
        let band = 24;
        let frequency = bands[band].center_hz;

        let mut analyzer = SpectrumAnalyzer::new(sample_rate);
        analyzer.push(&sine(frequency, sample_rate as f32, FFT_SIZE));

        let bars = analyzer.update();

        assert_eq!(
            peak_index(bars.levels()),
            band,
            "пик оказался не там: {:?}",
            bars.levels()
        );
        assert!(bars.levels()[band] > 0.5, "уровень слишком тихий");
    }

    #[test]
    fn levels_decay_after_the_signal_stops() {
        let mut analyzer = SpectrumAnalyzer::new(48_000);
        analyzer.push(&sine(1_000.0, 48_000.0, FFT_SIZE));
        let loud = analyzer.update();
        let peak = peak_index(loud.levels());
        let start = loud.levels()[peak];

        analyzer.push(&vec![0.0; FFT_SIZE]);
        let mut previous = start;
        for _ in 0..30 {
            let level = analyzer.update().levels()[peak];
            assert!(level <= previous, "спад должен быть монотонным");
            previous = level;
        }

        assert!(previous < start * 0.01, "столбик не затух: {previous}");
    }

    #[test]
    fn ring_keeps_the_latest_samples() {
        let ring = SpectrumRing::new(4);
        for value in 0..6 {
            ring.push(value as f32);
        }

        let mut cursor = 0;
        let mut out = Vec::new();
        assert_eq!(ring.read(&mut cursor, &mut out), 4);
        assert_eq!(out, vec![2.0, 3.0, 4.0, 5.0]);
        assert_eq!(cursor, 6);

        // Повторное чтение без новых записей ничего не даёт.
        assert_eq!(ring.read(&mut cursor, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn tap_downmixes_stereo_to_mono() {
        let ring = Arc::new(SpectrumRing::new(8));
        let channels = NonZeroU16::new(2).expect("два канала");
        let sample_rate = NonZeroU32::new(48_000).expect("частота");
        let samples = vec![1.0, 0.0, 0.0, 1.0, 0.5, 0.5];
        let tap = SpectrumTap::new(
            SamplesBuffer::new(channels, sample_rate, samples),
            Some(Arc::clone(&ring)),
        );

        let delivered = tap.count();
        assert_eq!(delivered, 6);

        let mut cursor = 0;
        let mut out = Vec::new();
        ring.read(&mut cursor, &mut out);
        assert_eq!(out, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn tap_without_ring_only_passes_samples_through() {
        let channels = NonZeroU16::new(1).expect("один канал");
        let sample_rate = NonZeroU32::new(8_000).expect("частота");
        let tap = SpectrumTap::new(
            SamplesBuffer::new(channels, sample_rate, vec![0.25; 3]),
            None,
        );

        assert_eq!(tap.count(), 3);
    }
}
