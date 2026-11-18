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

use gpui::{
    App, Bounds, Div, Entity, Pixels, SharedString, Stateful, canvas, div, point, prelude::*, px,
    size,
};
use player_core::SyncStats;

use crate::VideoPlayer;
use crate::icons::{Icon, LARGE_ICON_SIZE, icon};
use crate::video_player::PlaybackState;

/// Размер иконки-заглушки для файла без видео.
const AUDIO_ICON_SIZE: f32 = 64.0;

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
    let (state, has_video, has_frame, stats_text, can_retry, placeholder) = {
        let video = player.read(cx);
        (
            video.state(),
            video.video_info().is_some(),
            video.frame().is_some(),
            stats_overlay(video.stats(), video.stats_visible()),
            video.can_retry(),
            describe(video.state(), video.error()),
        )
    };

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
        .bg(gpui::black())
        .text_color(gpui::white());

    // Файл без видео: кадра не будет никогда, поэтому вместо чёрного поля —
    // нота и подпись. Состояния `Idle` и `Failed` остаются текстовыми.
    if !has_video && !matches!(state, PlaybackState::Idle | PlaybackState::Failed) {
        let mut root = root
            .child(icon(Icon::Note, AUDIO_ICON_SIZE, gpui::rgb(0xee_ee_ee)))
            .child(div().text_lg().child("Аудио файл"));
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
            root = root.child(
                div()
                    .absolute()
                    .left(px(8.0))
                    .top(px(8.0))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .bg(gpui::rgba(0x00_00_00_99))
                    .text_xs()
                    .child(stats),
            );
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
    root
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
        .bg(gpui::rgb(0xee_ee_ee))
        .cursor_pointer()
        .aria_label("Повторить")
        .child(icon(Icon::Replay, LARGE_ICON_SIZE, gpui::rgb(0x11_11_11)))
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
fn format_stats(stats: SyncStats) -> String {
    format!(
        "{:.1} fps · drop {} · lag {} ms",
        stats.fps,
        stats.dropped,
        stats.lag.as_millis()
    )
}

/// Текст оверлея с учётом настройки видимости.
///
/// Выключенный оверлей не просто прозрачен — строки нет вовсе, иначе она
/// продолжала бы занимать место в разметке и мешать кадру.
fn stats_overlay(stats: Option<SyncStats>, visible: bool) -> Option<String> {
    if !visible {
        return None;
    }
    stats.map(format_stats)
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
        PlaybackState::Idle => Some(("видео не открыто".into(), "player-gpui <file>".into())),
        PlaybackState::Ready | PlaybackState::Paused => None,
        PlaybackState::Playing => Some(("воспроизведение".into(), "ожидание первого кадра".into())),
        PlaybackState::Ended => Some((
            "воспроизведение завершено".into(),
            "нажмите «Повторить», чтобы начать заново".into(),
        )),
        PlaybackState::Failed => Some((
            "ошибка".into(),
            error.unwrap_or("неизвестная ошибка").to_owned().into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        };
        assert_eq!(format_stats(stats), "30.0 fps · drop 2 · lag 12 ms");
    }

    #[test]
    fn stats_overlay_follows_the_visibility_setting() {
        let stats = SyncStats {
            shown: 100,
            dropped: 0,
            lag: Duration::ZERO,
            fps: 30.0,
        };
        assert_eq!(
            stats_overlay(Some(stats), true).as_deref(),
            Some("30.0 fps · drop 0 · lag 0 ms")
        );
        assert_eq!(stats_overlay(Some(stats), false), None);
        assert_eq!(stats_overlay(None, true), None);
    }

    #[test]
    fn failed_state_shows_the_error_text() {
        let (title, detail) = describe(PlaybackState::Failed, Some("формат не поддерживается"))
            .expect("у ошибки есть текст");
        assert_eq!(title, "ошибка");
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
        assert_eq!(title, "воспроизведение завершено");
        assert!(detail.contains("Повторить"));
    }

    #[test]
    fn ready_state_has_no_placeholder_text() {
        assert!(describe(PlaybackState::Ready, None).is_none());
        assert!(describe(PlaybackState::Paused, None).is_none());
    }
}
