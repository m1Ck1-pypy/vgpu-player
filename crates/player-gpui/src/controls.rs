//! Необязательная панель управления.
//!
//! Компонент ничего не знает про ядро: он читает [`VideoPlayer`] и зовёт его
//! методы. Приложение может не использовать его и собрать свои контролы.
//!
//! Время в панели берётся из событий ядра (`PositionChanged` от аудио-часов), а
//! не из таймера UI. Слайдеры перемотки и громкости работают и по клику, и по
//! перетаскиванию: границы элемента сообщает `canvas` при отрисовке.

use std::time::Duration;

use gpui::{
    App, Bounds, Context, Div, Entity, MouseButton, Pixels, Rgba, Stateful, Window, canvas, div,
    prelude::*, px, relative,
};

use crate::VideoPlayer;
use crate::icons::{ICON_SIZE, Icon, icon};
use crate::theme;
use crate::video_player::PlaybackState;

/// Высота полосы слайдера: вся область клика, а не только видимая линия.
const SLIDER_HEIGHT: f32 = 18.0;
/// Толщина видимой линии слайдера.
const SLIDER_TRACK_HEIGHT: f32 = 4.0;

/// Ширина регулятора громкости в выдвижной панели.
const VOLUME_SLIDER_WIDTH: f32 = 120.0;

/// Минимальная ширина таймлайна: даже в самом узком окне по нему можно
/// попасть мышью.
const MIN_PROGRESS_WIDTH: f32 = 120.0;

/// Диаметр бегунка ползунка громкости.
const KNOB_SIZE: f32 = 12.0;

/// Насколько выдвижная панель громкости поднята над рядом кнопок:
/// высота кнопки (20 + 2×4) плюс зазор.
const VOLUME_PANEL_OFFSET: f32 = 36.0;

/// Чем отличается полоса прогресса от регулятора громкости.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliderKind {
    Progress,
    Volume,
}

/// Что делает кнопка Play в текущем состоянии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayAction {
    /// Файла ещё нет: кнопка открывает диалог выбора.
    OpenFile,
    /// Файл открыт: обычное play/pause.
    TogglePlayback,
}

/// Сопоставляет состоянию действие кнопки Play.
///
/// Без открытого файла проигрывать нечего, поэтому Play превращается в
/// «выбрать файл»; после выбора файл попадает в плеер как превью и его можно
/// запустить той же кнопкой.
fn play_action(state: PlaybackState) -> PlayAction {
    if state == PlaybackState::Idle {
        PlayAction::OpenFile
    } else {
        PlayAction::TogglePlayback
    }
}

type BoundsSetter = fn(&mut VideoPlayer, Bounds<Pixels>);
type SliderUpdate = fn(&mut VideoPlayer, Pixels, &mut Context<VideoPlayer>);
type SliderFinish = fn(&mut VideoPlayer, &mut Context<VideoPlayer>);

/// Форматирует время как `чч:мм:сс`.
///
/// Часы всегда показываем, чтобы ширина текста не менялась после часа
/// воспроизведения и панель не перевёрстывалась.
///
/// Функция нужна и панели, и панели «Информация о файле» в демо-окне,
/// поэтому `pub(crate)`.
pub(crate) fn format_clock(value: Duration) -> String {
    let total_seconds = value.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Процент громкости для подсказок и плашки: `0`, `42`, `100`.
///
/// Считаем в целых процентах, чтобы серия шагов колеса не накапливала
/// ошибку округления `f32`.
pub(crate) fn volume_percent(volume: f32) -> i64 {
    (volume.clamp(0.0, 1.0) * 100.0).round() as i64
}

/// Доля проигранного: позиция, делённая на длительность.
fn progress_ratio(position: Duration, duration: Option<Duration>) -> f32 {
    let Some(duration) = duration.filter(|value| !value.is_zero()) else {
        return 0.0;
    };

    (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0) as f32
}

/// Можно ли тащить полосу прогресса.
///
/// Без известной длительности и без перемотки слайдер бесполезен: ядро всё
/// равно проигнорирует `Seek`, а серый слайдер честно говорит почему.
fn progress_enabled(state: PlaybackState, seekable: bool, duration: Option<Duration>) -> bool {
    state != PlaybackState::Loading && seekable && duration.is_some()
}

/// Собирает панель: кнопки Play/Pause, mute и fullscreen, слайдеры перемотки
/// и громкости.
///
/// На кнопках только иконки: их названия остаются в `aria_label`, чтобы кнопки
/// не были безымянными для скринридера. Процент громкости — не постоянная
/// надпись, а подсказка над ползунком: её видно только при наведении.
///
/// Пока файл не открыт (`PlaybackState::Idle`), кнопка Play открывает диалог
/// выбора файла: проигрывать нечего, и мёртвая кнопка была бы непонятна.
///
/// Если панелей нужно несколько, им понадобятся разные `ElementId`; сейчас
/// предполагается одна панель на плеер.
pub fn controls_view(player: &Entity<VideoPlayer>, cx: &App) -> Div {
    let (
        state,
        volume,
        muted,
        position,
        duration,
        fullscreen,
        volume_hover,
        seekable,
        volume_panel_open,
    ) = {
        let video = player.read(cx);
        (
            video.state(),
            video.volume(),
            video.muted(),
            video.position(),
            video.duration(),
            video.fullscreen(),
            video.volume_hover(),
            video.seekable(),
            video.volume_panel_open(),
        )
    };
    // Пока источник открывается, управление не действует: `Play`, `Seek` и
    // `restart` в этом состоянии игнорируются и в `VideoPlayer`.
    let loading = state == PlaybackState::Loading;

    let time_text = match duration {
        Some(duration) => format!("{} / {}", format_clock(position), format_clock(duration)),
        None => format_clock(position),
    };
    let effective_volume = if muted { 0.0 } else { volume };

    let play_pause_target = player.clone();
    let (play_pause_icon, play_pause_label) = match state {
        PlaybackState::Playing => (Icon::Pause, "Пауза"),
        PlaybackState::Ended => (Icon::Replay, "Начать заново"),
        PlaybackState::Idle => (Icon::Play, "Открыть видео"),
        _ => (Icon::Play, "Воспроизвести"),
    };

    // Значок отражает mute, но кнопка в панели открывает регулятор:
    // сам mute живёт внутри выдвижной панели.
    let volume_icon = if muted { Icon::Mute } else { Icon::Volume };

    let volume_target = player.clone();
    let volume_label = if muted {
        "Громкость: звук выключен"
    } else {
        "Громкость"
    };

    let fullscreen_target = player.clone();
    let (fullscreen_icon, fullscreen_label) = if fullscreen {
        (Icon::FullscreenExit, "Выйти из полноэкранного режима")
    } else {
        (Icon::Fullscreen, "Полный экран")
    };

    let volume_popup = volume_panel_open.then(|| {
        volume_panel(
            player,
            effective_volume,
            volume_hover.unwrap_or(effective_volume),
            muted,
        )
    });

    let play_button = icon_button(
        "vgpu-play-pause",
        play_pause_icon,
        play_pause_label,
        gpui::rgb(theme::ACCENT),
        gpui::rgb(theme::TEXT),
        move |_, cx| {
            play_pause_target.update(cx, |video, cx| {
                video.close_volume_panel(cx);
                match play_action(video.state()) {
                    PlayAction::OpenFile => video.open_dialog(cx),
                    PlayAction::TogglePlayback => video.toggle_playback(cx),
                }
            })
        },
    );
    let play_button = if loading {
        play_button.opacity(0.5).cursor_default()
    } else {
        play_button
    };

    let row = div()
        .flex()
        .items_center()
        .gap_4()
        .w_full()
        .child(play_button)
        .child(
            slider(
                "vgpu-progress",
                progress_ratio(position, duration),
                player,
                SliderKind::Progress,
                progress_enabled(state, seekable, duration),
            )
            .flex_1()
            .min_w(px(MIN_PROGRESS_WIDTH)),
        )
        .child(div().text_sm().child(time_text))
        .child(icon_button(
            "vgpu-volume-toggle",
            volume_icon,
            volume_label,
            gpui::rgb(theme::RAISED),
            gpui::rgb(theme::TEXT),
            move |_, cx| {
                volume_target.update(cx, |video, cx| video.toggle_volume_panel(cx));
            },
        ))
        .child(icon_button(
            "vgpu-fullscreen",
            fullscreen_icon,
            fullscreen_label,
            gpui::rgb(theme::RAISED),
            gpui::rgb(theme::TEXT),
            move |window, cx| {
                // Платформа переключает режим асинхронно, поэтому иконку
                // обновляем сразу по ожидаемому состоянию, а точное значение
                // подтянет `PlayerWindow::render` из `Window::is_fullscreen`.
                window.toggle_fullscreen();
                let target = !fullscreen;
                fullscreen_target.update(cx, |video, cx| {
                    video.close_volume_panel(cx);
                    video.set_fullscreen(target, cx);
                });
            },
        ));

    // Обёртка нужна ради выдвижной панели: она лежит над рядом кнопок и не
    // сдвигает ни кадр, ни панель управления.
    let mut root = div().relative().child(row);
    if let Some(popup) = volume_popup {
        root = root.child(popup);
    }
    root
}

/// Выдвижная панель громкости над рядом кнопок.
///
/// Открывается значком динамика. Внутри — тот же значок для mute (клик
/// выставляет 0 %) и ползунок с бегунком; справа — процент, который на
/// наведении показывает значение под курсором.
fn volume_panel(
    player: &Entity<VideoPlayer>,
    volume: f32,
    shown: f32,
    muted: bool,
) -> Stateful<Div> {
    let mute_target = player.clone();
    let (mute_icon, mute_label) = if muted {
        (Icon::Mute, "Включить звук")
    } else {
        (Icon::Volume, "Выключить звук")
    };

    div()
        .id("vgpu-volume-panel")
        .absolute()
        .right(px(0.0))
        .bottom(px(VOLUME_PANEL_OFFSET))
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(gpui::rgb(theme::BORDER))
        .bg(gpui::rgb(theme::SURFACE))
        // Панель лежит поверх области кадра: не даём нажатию всплыть
        // до «клик по кадру закрывает наложения».
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .id("vgpu-volume-mute")
                .flex()
                .items_center()
                .justify_center()
                .w(px(24.0))
                .h(px(24.0))
                .rounded_sm()
                .cursor_pointer()
                .aria_label(mute_label)
                .hover(|style| style.bg(gpui::rgb(theme::RAISED)))
                .child(icon(mute_icon, ICON_SIZE, gpui::rgb(theme::TEXT)))
                .on_click(move |_, _, cx| {
                    mute_target.update(cx, |video, cx| video.toggle_mute(cx));
                }),
        )
        .child(
            slider("vgpu-volume", volume, player, SliderKind::Volume, true)
                .w(px(VOLUME_SLIDER_WIDTH)),
        )
        .child(
            div()
                .w(px(30.0))
                .flex()
                .justify_end()
                .text_sm()
                .child(volume_percent(shown).to_string()),
        )
}

/// Кнопка с иконкой вместо текста.
///
/// Подпись всё равно нужна: она уходит в `aria_label`, поэтому иконка не
/// остаётся безымянной для скринридера.
fn icon_button(
    id: &'static str,
    kind: Icon,
    label: &'static str,
    background: Rgba,
    foreground: Rgba,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(background)
        .cursor_pointer()
        .aria_label(label)
        .child(icon(kind, ICON_SIZE, foreground))
        .on_click(move |_, window, cx| on_click(window, cx))
}

/// Слайдер: линия, заполнение и прозрачный canvas для запоминания границ.
///
/// Полоса прогресса во время drag только обновляет предпросмотр и шлёт один
/// `Seek` при отпускании: точная перемотка декодера не любит поток команд.
///
/// У ползунка громкости дополнительно обновляется hover-процент и очищается
/// при уходе курсора — по нему панель показывает процент под курсором.
fn slider(
    id: &'static str,
    ratio: f32,
    player: &Entity<VideoPlayer>,
    kind: SliderKind,
    enabled: bool,
) -> Stateful<Div> {
    let ratio = ratio.clamp(0.0, 1.0);
    let top = (SLIDER_HEIGHT - SLIDER_TRACK_HEIGHT) / 2.0;

    let (set_bounds, down, drag, up): (BoundsSetter, SliderUpdate, SliderUpdate, SliderFinish) =
        match kind {
            SliderKind::Progress => (
                VideoPlayer::set_progress_bounds,
                VideoPlayer::begin_scrub_from_x,
                VideoPlayer::update_scrub_from_x,
                VideoPlayer::finish_scrub,
            ),
            SliderKind::Volume => (
                VideoPlayer::set_volume_bounds,
                VideoPlayer::begin_volume_from_x,
                VideoPlayer::update_volume_from_x,
                VideoPlayer::finish_volume,
            ),
        };

    let bounds_entity = player.clone();
    let click_entity = player.clone();
    let drag_entity = player.clone();
    let hover_entity = player.clone();
    let hover_out_entity = player.clone();
    let up_entity = player.clone();
    let up_out_entity = player.clone();

    let mut root = div()
        .id(id)
        .relative()
        .h(px(SLIDER_HEIGHT))
        .opacity(if enabled { 1.0 } else { 0.5 });
    root = if enabled {
        root.cursor_pointer()
    } else {
        root.cursor_default()
    };

    root = root
        .on_mouse_down(MouseButton::Left, move |event, _, cx: &mut App| {
            if !enabled {
                return;
            }
            click_entity.update(cx, |video, cx| down(video, event.position.x, cx));
        })
        .on_mouse_move(move |event, _, cx: &mut App| {
            if !enabled {
                return;
            }
            if event.dragging() {
                drag_entity.update(cx, |video, cx| drag(video, event.position.x, cx));
            }
            if matches!(kind, SliderKind::Volume) {
                hover_entity.update(cx, |video, cx| {
                    video.hover_volume_from_x(event.position.x, cx);
                });
            }
        })
        .on_hover(move |hovered, _, cx: &mut App| {
            if !hovered && matches!(kind, SliderKind::Volume) {
                hover_out_entity.update(cx, |video, cx| video.set_volume_hover(None, cx));
            }
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx: &mut App| {
            if enabled {
                up_entity.update(cx, up);
            }
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, cx: &mut App| {
            if enabled {
                up_out_entity.update(cx, up);
            }
        });

    root = root
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(top))
                .w_full()
                .h(px(SLIDER_TRACK_HEIGHT))
                .rounded_full()
                .bg(gpui::rgb(theme::BORDER)),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(top))
                .w(relative(ratio))
                .h(px(SLIDER_TRACK_HEIGHT))
                .rounded_full()
                .bg(gpui::rgb(theme::ACCENT)),
        );

    if matches!(kind, SliderKind::Volume) {
        // Бегунок центрирован над дорожкой; сдвиг на радиус компенсирует
        // привязку `left` к левому краю кружка.
        root = root.child(
            div()
                .absolute()
                .left(relative(ratio))
                .top(px((SLIDER_HEIGHT - KNOB_SIZE) / 2.0))
                .ml(px(-KNOB_SIZE / 2.0))
                .w(px(KNOB_SIZE))
                .h(px(KNOB_SIZE))
                .rounded_full()
                .bg(gpui::rgb(theme::ACCENT)),
        );
    }

    root.child(
        div().absolute().top_0().left_0().size_full().child(
            canvas(
                move |bounds, _window, cx| {
                    bounds_entity.update(cx, |video, _| set_bounds(video, bounds));
                },
                |_, _, _, _| {},
            )
            .size_full(),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_hours_minutes_and_seconds() {
        assert_eq!(format_clock(Duration::ZERO), "00:00:00");
        assert_eq!(format_clock(Duration::from_secs(9)), "00:00:09");
        assert_eq!(format_clock(Duration::from_secs(65)), "00:01:05");
        assert_eq!(format_clock(Duration::from_secs(3_600)), "01:00:00");
        assert_eq!(format_clock(Duration::from_secs(5_712)), "01:35:12");
    }

    #[test]
    fn play_button_opens_the_dialog_when_nothing_is_loaded() {
        assert_eq!(play_action(PlaybackState::Idle), PlayAction::OpenFile);
        for state in [
            PlaybackState::Loading,
            PlaybackState::Ready,
            PlaybackState::Playing,
            PlaybackState::Paused,
            PlaybackState::Ended,
            PlaybackState::Failed,
        ] {
            assert_eq!(play_action(state), PlayAction::TogglePlayback, "{state:?}");
        }
    }

    #[test]
    fn volume_percent_rounds_and_clamps() {
        assert_eq!(volume_percent(0.0), 0);
        assert_eq!(volume_percent(0.424), 42);
        assert_eq!(volume_percent(0.425), 43);
        assert_eq!(volume_percent(1.0), 100);
        assert_eq!(volume_percent(1.5), 100);
        assert_eq!(volume_percent(-0.2), 0);
    }

    #[test]
    fn progress_ratio_is_bounded() {
        assert_eq!(
            progress_ratio(Duration::from_secs(5), Some(Duration::from_secs(10))),
            0.5
        );
        assert_eq!(
            progress_ratio(Duration::from_secs(20), Some(Duration::from_secs(10))),
            1.0
        );
        assert_eq!(progress_ratio(Duration::from_secs(5), None), 0.0);
        assert_eq!(
            progress_ratio(Duration::from_secs(5), Some(Duration::ZERO)),
            0.0
        );
    }

    #[test]
    fn progress_needs_a_seekable_source_with_duration() {
        let duration = Some(Duration::from_secs(10));
        assert!(progress_enabled(PlaybackState::Playing, true, duration));
        assert!(progress_enabled(PlaybackState::Ended, true, duration));
        // Без `Range` источник играет, но не перематывается.
        assert!(!progress_enabled(PlaybackState::Playing, false, duration));
        // Пока длительность неизвестна, тащить некуда.
        assert!(!progress_enabled(PlaybackState::Playing, true, None));
        // Пока идёт открытие, управление не действует.
        assert!(!progress_enabled(PlaybackState::Loading, true, duration));
    }
}
