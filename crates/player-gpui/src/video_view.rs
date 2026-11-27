//! Виджет кадра.
//!
//! Возвращаем обычный `Div`, чтобы вызывающий код стилизовал его как любой
//! другой элемент:
//!
//! ```no_run
//! # use gpui::prelude::*;
//! # use player_gpui::{video_view, VideoPlayer};
//! # fn example(player: &gpui::Entity<VideoPlayer>, cx: &mut gpui::Context<()>) {
//! video_view(player, cx).size_full().rounded_md();
//! # }
//! ```

use std::time::Duration;

use gpui::{
    App, Bounds, Div, Entity, Pixels, Rgba, SharedString, Stateful, canvas, div, point, prelude::*,
    px, relative, size,
};
use player_core::{MediaInfo, SpectrumBars, SyncStats, VideoInfo};

use crate::VideoPlayer;
use crate::icons::{Icon, LARGE_ICON_SIZE, icon};
use crate::theme;
use crate::video_player::PlaybackState;

/// Отступы и пропорции визуализатора звука.
const SPECTRUM_PADDING: f32 = 24.0;
/// Почти вся высота окна: столбики растут от нижнего края вверх.
const SPECTRUM_HEIGHT_RATIO: f32 = 0.9;
const SPECTRUM_GAP: f32 = 2.0;
/// Минимальная высота столбика: даже в тишине видна ровная полоса-основание.
const SPECTRUM_MIN_BAR: f32 = 0.035;

/// Цвета столбиков взятые с иконки приложения: оранжевый низ, жёлтый верх.
const BAR_LOW_COLOR: u32 = theme::ACCENT;
const BAR_HIGH_COLOR: u32 = theme::HIGHLIGHT;

/// Что делает кнопка под сообщением о конце или ошибке.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Начать файл заново.
    Restart,
    /// Повторить открытие файла.
    Retry,
}

/// Собирает виджет кадра для плеера.
///
/// Кадр рисуется в динамическую BGRA-текстуру `gpui-fast`: она живёт вне
/// атласа, поэтому обновление каждого кадра не пересобирает иконки и глифы.
/// Соотношение сторон сохраняется (letterbox), поэтому ресайз окна не
/// растягивает картинку. Размер и скругления задаёт вызывающий код —
/// `.size_full()`, `.rounded_md()` и так далее.
pub fn video_view(player: &Entity<VideoPlayer>, cx: &App) -> Div {
    let (state, has_video, has_frame, stats_text, can_retry, placeholder, spectrum, buffering) = {
        let video = player.read(cx);
        let details = OverlayDetails {
            position: video.position(),
            duration: video.duration(),
            video_info: video.video_info(),
            media_info: video.media_info(),
            speed: video.network_speed(),
        };
        (
            video.state(),
            video.video_info().is_some(),
            video.frame().is_some(),
            stats_overlay(video.stats(), video.stats_visible(), &details),
            video.can_retry(),
            describe(video.state(), video.error()),
            video.spectrum(),
            video.buffering(),
        )
    };
    let buffering = shows_buffering(state, buffering);
    let live = shows_live(state, player.read(cx).live());

    let action = match state {
        PlaybackState::Ended => Some(action_button("vgpu-restart", Action::Restart, player)),
        PlaybackState::Failed if can_retry => {
            Some(action_button("vgpu-retry", Action::Retry, player))
        }
        _ => None,
    };

    let root = div()
        .relative()
        .overflow_hidden()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(gpui::rgb(theme::BACKGROUND))
        .text_color(gpui::rgb(theme::TEXT));

    // Файл без видео: кадра не будет никогда, поэтому вместо чёрного поля —
    // спектроанализатор. Состояния `Idle` и `Failed` остаются текстовыми.
    if shows_spectrum(state, has_video) {
        let mut root = root.child(spectrum_view(spectrum));
        if live {
            root = root.child(live_badge());
        }
        if buffering {
            root = root.child(buffering_overlay());
        }
        if let Some(action) = action {
            root = root.child(action);
        }
        return root;
    }

    if has_frame {
        let entity = player.clone();
        let mut root = root.child(
            canvas(
                move |_bounds, window, cx| {
                    entity.update(cx, |video: &mut VideoPlayer, _| video.sync_texture(window))
                },
                move |bounds, texture, window, _cx| {
                    let Some(texture) = texture else {
                        return;
                    };
                    let target = fit_rect(bounds, texture.width, texture.height);
                    let _ = window.paint_dynamic_texture(target, texture.texture);
                },
            )
            .size_full(),
        );

        if let Some(stats) = stats_text {
            let mut overlay = div()
                .absolute()
                .left(px(8.0))
                .top(px(8.0))
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(gpui::rgba(theme::OVERLAY_BG))
                .text_xs()
                .flex()
                .flex_col();
            // По строке на ребёнка: переносами внутри одного текста
            // раскладка не управляет.
            for line in stats.lines() {
                overlay = overlay.child(line.to_owned());
            }
            root = root.child(overlay);
        }

        // Кнопка поверх последнего кадра: сам кадр не перерисовывается.
        if let Some(action) = action {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(action),
            );
        }

        if buffering {
            root = root.child(buffering_overlay());
        }
        if live {
            root = root.child(live_badge());
        }

        return root;
    }

    let mut root = root;
    if let Some((title, detail)) = placeholder {
        root = root.child(title);
        if !detail.is_empty() {
            root = root.child(div().text_sm().opacity(0.7).child(detail));
        }
    }
    if let Some(action) = action {
        root = root.child(action);
    }
    if buffering {
        root = root.child(buffering_overlay());
    }
    if live {
        root = root.child(live_badge());
    }
    root
}

/// Рисуем ли спектр: только для файла без видео и не в текстовых состояниях.
fn shows_spectrum(state: PlaybackState, has_video: bool) -> bool {
    !has_video
        && !matches!(
            state,
            PlaybackState::Idle | PlaybackState::Loading | PlaybackState::Failed
        )
}

/// Рисуем ли плашку буферизации: только поверх уже открытого источника.
fn shows_buffering(state: PlaybackState, buffering: bool) -> bool {
    buffering
        && !matches!(
            state,
            PlaybackState::Idle | PlaybackState::Loading | PlaybackState::Failed
        )
}

/// Рисуем ли плашку `LIVE`: только у открытого live-потока.
fn shows_live(state: PlaybackState, live: bool) -> bool {
    live && !matches!(
        state,
        PlaybackState::Idle | PlaybackState::Loading | PlaybackState::Failed
    )
}

/// Плашка `LIVE` в правом верхнем углу кадра.
fn live_badge() -> Div {
    div()
        .absolute()
        .top(px(12.0))
        .right(px(12.0))
        .px_2()
        .py_1()
        .rounded_sm()
        .bg(gpui::rgb(theme::HIGHLIGHT))
        .text_color(gpui::rgb(theme::ON_ACCENT))
        .text_xs()
        .child("LIVE")
}

/// Плашка «Буферизация…» поверх кадра.
fn buffering_overlay() -> Div {
    div()
        .absolute()
        .top(px(12.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .px_3()
                .py_1()
                .rounded_md()
                .bg(gpui::rgba(theme::OVERLAY_BG))
                .text_sm()
                .child("Буферизация…"),
        )
}

/// Визуализатор звука: столбики растут от нижнего края окна вверх, как
/// кирпичики на иконке.
fn spectrum_view(spectrum: Option<SpectrumBars>) -> Div {
    let levels = spectrum.unwrap_or(SpectrumBars::ZERO);
    let mut bars = div()
        .w_full()
        .h(relative(SPECTRUM_HEIGHT_RATIO))
        .px(px(SPECTRUM_PADDING))
        .flex()
        .items_end()
        .gap(px(SPECTRUM_GAP));

    for level in levels.levels() {
        bars = bars.child(
            div()
                .flex_1()
                .h(relative(bar_fraction(*level)))
                .rounded_sm()
                .bg(bar_color(*level)),
        );
    }

    // Абсолютное положение не зависит от выравнивания корня: основание
    // столбиков всегда на нижнем крае окна.
    bars.absolute().left_0().bottom_0()
}

/// Высота столбика: доля высоты ряда, всегда хотя бы видимая полоска.
fn bar_fraction(level: f32) -> f32 {
    level.clamp(0.0, 1.0).max(SPECTRUM_MIN_BAR)
}

/// Цвет столбика по уровню: тихие — оранжевые, громкие — жёлтые.
fn bar_color(level: f32) -> Rgba {
    gpui::rgb(bar_rgb(level))
}

fn bar_rgb(level: f32) -> u32 {
    let ratio = level.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let low = (BAR_LOW_COLOR >> shift) & 0xff;
        let high = (BAR_HIGH_COLOR >> shift) & 0xff;
        (low as f32 + (high as f32 - low as f32) * ratio).round() as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// Круглая кнопка «Повторить» под сообщением о завершении или ошибке.
///
/// Вместо текста — иконка; название остаётся в `aria_label`.
fn action_button(id: &'static str, action: Action, player: &Entity<VideoPlayer>) -> Stateful<Div> {
    let target = player.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(56.0))
        .h(px(56.0))
        .rounded_full()
        .bg(gpui::rgb(theme::ACCENT))
        .cursor_pointer()
        .aria_label("Повторить")
        .child(icon(Icon::Replay, LARGE_ICON_SIZE, gpui::rgb(theme::TEXT)))
        .on_click(move |_, _, cx: &mut App| {
            // Кнопка лежит внутри области клика по кадру: не даём клику
            // «Повторить» всплыть и переключить воспроизведение ещё раз.
            cx.stop_propagation();
            target.update(cx, |video, cx| match action {
                Action::Restart => video.restart(cx),
                Action::Retry => video.retry(cx),
            });
        })
}

/// Строка оверлея: fps, пропуски и отставание последнего кадра.
///
/// Для сетевого источника добавляются ещё две строки: счётчики читателя
/// (`net:`) и занятость общего кэша (`cache:`). По ним видно, откуда берутся
/// `drop` и рост памяти: трафик и резидентный кэш — разные величины.
fn format_stats(stats: SyncStats) -> String {
    let mut text = format!(
        "{:.1} fps · drop {} · lag {} ms",
        stats.fps,
        stats.dropped,
        stats.lag.as_millis()
    );

    if let Some(network) = stats.network {
        text.push('\n');
        text.push_str(&format!(
            "net: {} req · {} · slow {} ({:.1} s) · retries {}",
            network.requests,
            format_mb(network.bytes),
            network.slow_reads,
            network.slow_ms as f64 / 1000.0,
            network.retries
        ));
        // Перезапуски бывают только у источника без `Range`.
        if network.restarts > 0 {
            text.push_str(&format!(" · restarts {}", network.restarts));
        }
        text.push('\n');
        text.push_str(&format!("cache: {}", format_mb(network.cache_bytes)));
    }

    text
}

/// Что показать во втором блоке оверлея: контекст источника.
struct OverlayDetails<'a> {
    position: Duration,
    duration: Option<Duration>,
    video_info: Option<VideoInfo>,
    media_info: Option<&'a MediaInfo>,
    /// Текущая скорость сети, байт/с; 0 — ещё нет двух срезов.
    speed: f32,
}

/// Собирает полный текст оверлея: счётчики и второй блок с контекстом.
fn format_overlay(stats: SyncStats, details: &OverlayDetails<'_>) -> String {
    let mut text = format_stats(stats);
    let extra = format_details(stats, details);
    if !extra.is_empty() {
        text.push('\n');
        text.push_str(&extra);
    }
    text
}

/// Второй блок: позиция/длительность, кодек, скорость, окно и буфер.
fn format_details(stats: SyncStats, details: &OverlayDetails<'_>) -> String {
    let mut lines = Vec::new();

    let mut position = format!("pos {}", format_clock(details.position));
    if let Some(duration) = details.duration {
        position.push_str(&format!(" / {}", format_clock(duration)));
    }
    position.push_str(&format!(" · shown {}", stats.shown));
    if details.speed > 0.0 {
        position.push_str(&format!(" · {}", format_speed(details.speed)));
    }
    lines.push(position);

    if let Some(codec) = format_codecs(details) {
        lines.push(codec);
    }

    if let Some(network) = stats.network {
        let mut media = Vec::new();
        if network.window_bytes > 0 {
            media.push(format!(
                "window v {} / a {}",
                format_mb(network.window_video_bytes),
                format_mb(network.window_audio_bytes)
            ));
        }
        if let Some(buffer) = network.buffer_ahead {
            media.push(format!("buf {:.1} s", buffer.as_secs_f64()));
        }
        if !media.is_empty() {
            lines.push(media.join(" · "));
        }
    }

    lines.join("\n")
}

/// `h264 1920x1080@25 · aac 2ch 44.1 kHz`; пропускает неизвестные части.
fn format_codecs(details: &OverlayDetails<'_>) -> Option<String> {
    let mut parts = Vec::new();

    let video = details.media_info.and_then(|media| media.video.as_ref());
    let width = video
        .map(|track| track.width)
        .or(details.video_info.map(|info| info.width));
    let height = video
        .map(|track| track.height)
        .or(details.video_info.map(|info| info.height));
    let frame_rate = video
        .and_then(|track| track.frame_rate)
        .or(details.video_info.and_then(|info| info.frame_rate));
    if let (Some(width), Some(height)) = (width, height) {
        let mut part = String::new();
        if let Some(codec) = video.and_then(|track| track.codec.as_deref()) {
            part.push_str(codec);
            part.push(' ');
        }
        part.push_str(&format!("{width}x{height}"));
        if let Some(fps) = frame_rate {
            part.push_str(&format!("@{}", format_fps(fps)));
        }
        parts.push(part);
    }

    if let Some(audio) = details.media_info.and_then(|media| media.audio.as_ref()) {
        let mut part = String::new();
        if let Some(codec) = audio.codec.as_deref() {
            part.push_str(codec);
            part.push(' ');
        }
        part.push_str(&format!("{}ch", audio.channels));
        if audio.sample_rate > 0 {
            part.push_str(&format!(" {:.1} kHz", audio.sample_rate as f64 / 1000.0));
        }
        parts.push(part);
    }

    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// `m:ss` до часа и `h:mm:ss` дальше.
fn format_clock(position: Duration) -> String {
    let total = position.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Байты в секунду как `12.3 MB/s`.
fn format_speed(bytes_per_second: f32) -> String {
    format!("{:.1} MB/s", bytes_per_second as f64 / (1024.0 * 1024.0))
}

/// Целая частота кадров без дробной части: `25`, `29.97`.
fn format_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.05 {
        format!("{:.0}", fps)
    } else {
        format!("{fps:.2}")
    }
}

/// Байты в мегабайтах с одним знаком: `12.3 MB`.
fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Текст оверлея с учётом настройки видимости.
///
/// Выключенный оверлей не просто прозрачен — строки нет вовсе, иначе она
/// продолжала бы занимать место в разметке и мешать кадру.
fn stats_overlay(
    stats: Option<SyncStats>,
    visible: bool,
    details: &OverlayDetails<'_>,
) -> Option<String> {
    if !visible {
        return None;
    }
    stats.map(|stats| format_overlay(stats, details))
}

/// Прямоугольник кадра внутри `bounds` с сохранением пропорций.
///
/// Кадр вписывается целиком (`contain`), свободные поля остаются чёрными —
/// это и есть letterbox. Центрируем картинку по обеим осям.
fn fit_rect(bounds: Bounds<Pixels>, width: u32, height: u32) -> Bounds<Pixels> {
    let available_width = f32::from(bounds.size.width).max(0.0);
    let available_height = f32::from(bounds.size.height).max(0.0);
    let frame_width = width as f32;
    let frame_height = height as f32;

    if frame_width <= 0.0
        || frame_height <= 0.0
        || available_width <= 0.0
        || available_height <= 0.0
    {
        return Bounds {
            origin: bounds.origin,
            size: size(px(0.0), px(0.0)),
        };
    }

    let scale = (available_width / frame_width).min(available_height / frame_height);
    let fitted_width = frame_width * scale;
    let fitted_height = frame_height * scale;

    Bounds {
        origin: point(
            bounds.origin.x + px((available_width - fitted_width) / 2.0),
            bounds.origin.y + px((available_height - fitted_height) / 2.0),
        ),
        size: size(px(fitted_width), px(fitted_height)),
    }
}

/// Подпись и подробность для состояния без кадра.
///
/// `None` — говорить нечего: превью или последний показанный кадр сами
/// объясняют, что открыто. Именно поэтому у `Ready` нет больше надписи
/// «готов к воспроизведению».
fn describe(state: PlaybackState, error: Option<&str>) -> Option<(SharedString, SharedString)> {
    match state {
        PlaybackState::Idle => Some((
            "Перетащите видео или откройте через меню".into(),
            "В консоле: gpui <file>".into(),
        )),
        PlaybackState::Loading => Some(("Загрузка…".into(), "".into())),
        PlaybackState::Ready | PlaybackState::Paused => None,
        PlaybackState::Playing => Some(("Воспроизведение".into(), "ожидание первого кадра".into())),
        PlaybackState::Ended => Some((
            "Воспроизведение завершено".into(),
            "нажмите «Повторить», чтобы начать заново".into(),
        )),
        PlaybackState::Failed => Some((
            "Ошибка".into(),
            error.unwrap_or("неизвестная ошибка").to_owned().into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use player_core::{AudioTrackInfo, MediaInfo, NetworkStats, VideoTrackInfo};

    fn details<'a>(media: Option<&'a MediaInfo>) -> OverlayDetails<'a> {
        OverlayDetails {
            position: Duration::ZERO,
            duration: None,
            video_info: None,
            media_info: media,
            speed: 0.0,
        }
    }

    fn bounds(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(width), px(height)),
        }
    }

    fn assert_rect(actual: Bounds<Pixels>, x: f32, y: f32, width: f32, height: f32) {
        assert!(
            (f32::from(actual.origin.x) - x).abs() < 0.01
                && (f32::from(actual.origin.y) - y).abs() < 0.01
                && (f32::from(actual.size.width) - width).abs() < 0.01
                && (f32::from(actual.size.height) - height).abs() < 0.01,
            "получено {:?} {:?}, ожидалось ({x}, {y}) {width}x{height}",
            actual.origin,
            actual.size
        );
    }

    #[test]
    fn wide_frame_is_centered_with_letterbox() {
        // Кадр 16:9 в квадратном окне: поля сверху и снизу.
        let fitted = fit_rect(bounds(100.0, 100.0), 1920, 1080);
        assert_rect(fitted, 0.0, 21.875, 100.0, 56.25);
    }

    #[test]
    fn tall_frame_is_centered_with_pillarbox() {
        let fitted = fit_rect(bounds(100.0, 100.0), 1080, 1920);
        assert_rect(fitted, 21.875, 0.0, 56.25, 100.0);
    }

    #[test]
    fn matching_aspect_ratio_fills_bounds() {
        let fitted = fit_rect(bounds(100.0, 100.0), 720, 720);
        assert_rect(fitted, 0.0, 0.0, 100.0, 100.0);
    }

    #[test]
    fn degenerate_sizes_give_empty_rect() {
        let fitted = fit_rect(bounds(100.0, 100.0), 0, 720);
        assert_rect(fitted, 0.0, 0.0, 0.0, 0.0);

        let fitted = fit_rect(bounds(0.0, 100.0), 720, 720);
        assert_rect(fitted, 0.0, 0.0, 0.0, 0.0);
    }

    #[test]
    fn formats_sync_stats() {
        let stats = SyncStats {
            shown: 100,
            dropped: 2,
            lag: Duration::from_millis(12),
            fps: 29.97,
            network: None,
        };
        assert_eq!(format_stats(stats), "30.0 fps · drop 2 · lag 12 ms");
    }

    #[test]
    fn network_counters_get_their_own_line() {
        let stats = SyncStats {
            shown: 100,
            dropped: 40,
            lag: Duration::ZERO,
            fps: 60.0,
            network: Some(NetworkStats {
                requests: 14,
                bytes: 26_000_000,
                slow_reads: 9,
                slow_ms: 2_500,
                retries: 1,
                restarts: 0,
                cache_bytes: 26_000_000,
                window_bytes: 0,
                window_video_bytes: 0,
                window_audio_bytes: 0,
                buffer_ahead: None,
            }),
        };
        assert_eq!(
            format_stats(stats),
            "60.0 fps · drop 40 · lag 0 ms\nnet: 14 req · 24.8 MB · slow 9 (2.5 s) · retries 1\ncache: 24.8 MB"
        );
    }

    #[test]
    fn hls_window_and_buffer_get_their_own_line() {
        let stats = SyncStats {
            shown: 100,
            dropped: 0,
            lag: Duration::ZERO,
            fps: 58.6,
            network: Some(NetworkStats {
                requests: 21,
                bytes: 21_600_000,
                slow_reads: 18,
                slow_ms: 19_400,
                retries: 0,
                restarts: 0,
                cache_bytes: 0,
                window_bytes: 12_300_000,
                window_video_bytes: 9_400_000,
                window_audio_bytes: 2_900_000,
                buffer_ahead: Some(Duration::from_secs_f64(12.4)),
            }),
        };

        let text = format_overlay(stats, &details(None));
        assert!(
            text.contains("window v 9.0 MB / a 2.8 MB · buf 12.4 s"),
            "{text}"
        );
    }

    #[test]
    fn details_show_position_codecs_and_speed() {
        let media = MediaInfo {
            container: Some("HLS".to_owned()),
            video: Some(VideoTrackInfo {
                codec: Some("h264".to_owned()),
                width: 1920,
                height: 1080,
                frame_rate: Some(25.0),
            }),
            audio: Some(AudioTrackInfo {
                codec: Some("aac".to_owned()),
                channels: 2,
                sample_rate: 44_100,
            }),
        };
        let stats = SyncStats {
            shown: 1450,
            dropped: 0,
            lag: Duration::ZERO,
            fps: 60.0,
            network: None,
        };
        let details = OverlayDetails {
            position: Duration::from_secs(83),
            duration: Some(Duration::from_secs(296)),
            video_info: None,
            media_info: Some(&media),
            speed: 4.2 * 1024.0 * 1024.0,
        };

        let text = format_overlay(stats, &details);
        assert!(
            text.contains("pos 1:23 / 4:56 · shown 1450 · 4.2 MB/s"),
            "{text}"
        );
        assert!(
            text.contains("h264 1920x1080@25 · aac 2ch 44.1 kHz"),
            "{text}"
        );
    }

    #[test]
    fn restarts_show_only_when_present() {
        let stats = SyncStats {
            shown: 10,
            dropped: 0,
            lag: Duration::ZERO,
            fps: 30.0,
            network: Some(NetworkStats {
                requests: 3,
                bytes: 0,
                slow_reads: 0,
                slow_ms: 0,
                retries: 0,
                restarts: 2,
                cache_bytes: 0,
                window_bytes: 0,
                window_video_bytes: 0,
                window_audio_bytes: 0,
                buffer_ahead: None,
            }),
        };
        assert!(format_stats(stats).contains("restarts 2"));
    }

    #[test]
    fn stats_overlay_follows_the_visibility_setting() {
        let stats = SyncStats {
            shown: 100,
            dropped: 0,
            lag: Duration::ZERO,
            fps: 30.0,
            network: None,
        };
        let details = details(None);
        assert_eq!(
            stats_overlay(Some(stats), true, &details).as_deref(),
            Some("30.0 fps · drop 0 · lag 0 ms\npos 0:00 · shown 100")
        );
        assert_eq!(stats_overlay(Some(stats), false, &details), None);
        assert_eq!(stats_overlay(None, true, &details), None);
    }

    #[test]
    fn failed_state_shows_the_error_text() {
        let (title, detail) = describe(PlaybackState::Failed, Some("формат не поддерживается"))
            .expect("у ошибки есть текст");
        assert_eq!(title, "Ошибка");
        assert_eq!(detail, "формат не поддерживается");
    }

    #[test]
    fn failed_state_has_a_fallback_text() {
        let (_, detail) = describe(PlaybackState::Failed, None).expect("у ошибки есть текст");
        assert_eq!(detail, "неизвестная ошибка");
    }

    #[test]
    fn ended_state_offers_a_restart() {
        let (title, detail) = describe(PlaybackState::Ended, None).expect("конец файла — текст");
        assert_eq!(title, "Воспроизведение завершено");
        assert!(detail.contains("Повторить"));
    }

    #[test]
    fn ready_state_has_no_placeholder_text() {
        assert!(describe(PlaybackState::Ready, None).is_none());
        assert!(describe(PlaybackState::Paused, None).is_none());
    }

    #[test]
    fn buffering_is_shown_only_for_open_states() {
        assert!(shows_buffering(PlaybackState::Playing, true));
        assert!(shows_buffering(PlaybackState::Paused, true));
        assert!(shows_buffering(PlaybackState::Ready, true));
        assert!(!shows_buffering(PlaybackState::Playing, false));
        assert!(!shows_buffering(PlaybackState::Loading, true));
        assert!(!shows_buffering(PlaybackState::Idle, true));
        assert!(!shows_buffering(PlaybackState::Failed, true));
    }

    #[test]
    fn live_badge_is_shown_only_for_open_live_streams() {
        assert!(shows_live(PlaybackState::Ready, true));
        assert!(shows_live(PlaybackState::Playing, true));
        assert!(shows_live(PlaybackState::Paused, true));
        assert!(!shows_live(PlaybackState::Loading, true));
        assert!(!shows_live(PlaybackState::Idle, true));
        assert!(!shows_live(PlaybackState::Failed, true));
        assert!(!shows_live(PlaybackState::Playing, false));
    }

    #[test]
    fn loading_state_shows_progress_text() {
        let (title, detail) =
            describe(PlaybackState::Loading, None).expect("у загрузки есть текст");
        assert_eq!(title, "Загрузка…");
        assert!(detail.is_empty());
    }

    #[test]
    fn spectrum_is_only_for_audio_files() {
        assert!(shows_spectrum(PlaybackState::Ready, false));
        assert!(shows_spectrum(PlaybackState::Playing, false));
        assert!(shows_spectrum(PlaybackState::Paused, false));
        assert!(shows_spectrum(PlaybackState::Ended, false));
        assert!(!shows_spectrum(PlaybackState::Idle, false));
        assert!(!shows_spectrum(PlaybackState::Loading, false));
        assert!(!shows_spectrum(PlaybackState::Failed, false));
        assert!(!shows_spectrum(PlaybackState::Playing, true));
    }

    #[test]
    fn silent_bars_keep_a_visible_baseline() {
        assert_eq!(bar_fraction(-1.0), SPECTRUM_MIN_BAR);
        assert_eq!(bar_fraction(0.0), SPECTRUM_MIN_BAR);
        assert!(bar_fraction(0.5) > bar_fraction(0.0));
        assert_eq!(bar_fraction(2.0), 1.0);
    }

    #[test]
    fn bar_color_moves_from_orange_to_yellow() {
        assert_eq!(bar_rgb(0.0), BAR_LOW_COLOR);
        assert_eq!(bar_rgb(1.0), BAR_HIGH_COLOR);

        let middle = bar_rgb(0.5);
        assert!(middle != BAR_LOW_COLOR && middle != BAR_HIGH_COLOR);
    }
}
