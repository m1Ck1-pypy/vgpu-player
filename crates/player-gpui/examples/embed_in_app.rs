//! Встраивание плеера в собственное окно: `video_view` и свои кнопки.
//!
//! Пример показывает, что готовая панель [`player_gpui::controls_view`] не
//! обязательна: чужое приложение рисует кадр там, где ему нужно, и управляет
//! воспроизведением методами [`player_gpui::VideoPlayer`].
//!
//! Запуск:
//!
//! ```bash
//! cargo run -p player-gpui --example embed_in_app -- assets/test/sync_test.mp4
//! ```

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use player_gpui::gpui::{
    self, App, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, IntoElement, KeyDownEvent,
    Render, SharedString, Stateful, TitlebarOptions, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, size,
};
use player_gpui::gpui_platform;
use player_gpui::{VideoPlayer, video_view};

/// Шаг перемотки стрелками и кнопками.
const SEEK_STEP: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let path = std::env::args_os().nth(1).map(PathBuf::from);

    // `Application::run` ничего не возвращает, поэтому ошибку открытия окна
    // складываем в слот — так же, как это делает `player_gpui::run_app`.
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let failure_slot = Rc::clone(&failure);

    gpui_platform::application().run(move |cx: &mut App| {
        if let Err(error) = open_window(cx, path) {
            *failure_slot.borrow_mut() = Some(error.to_string());
            cx.quit();
        }
    });

    match failure.borrow_mut().take() {
        Some(message) => {
            eprintln!("embed_in_app: {message}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}

fn open_window(cx: &mut App, path: Option<PathBuf>) -> gpui::Result<()> {
    let bounds = Bounds::centered(None, size(px(960.0), px(540.0)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("player-gpui: embed_in_app".into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    cx.open_window(options, |_window, cx| {
        cx.new(|cx| EmbedScreen::new(path, cx))
    })?;
    Ok(())
}

/// Экран целиком: кадр сверху, собственная строка кнопок снизу.
struct EmbedScreen {
    player: Entity<VideoPlayer>,
    focus_handle: FocusHandle,
    focused: bool,
}

impl EmbedScreen {
    fn new(path: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let player = match path {
            Some(path) => VideoPlayer::create(path, cx),
            None => cx.new(VideoPlayer::new),
        };

        Self {
            player,
            focus_handle: cx.focus_handle(),
            focused: false,
        }
    }

    fn toggle_playback(
        &mut self,
        _event: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.player
            .update(cx, |video, cx| video.toggle_playback(cx));
    }

    fn seek_backward(&mut self, _event: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(false, cx);
    }

    fn seek_forward(&mut self, _event: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.seek_relative(true, cx);
    }

    fn open_dialog(&mut self, _event: &ClickEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.player.update(cx, |video, cx| video.open_dialog(cx));
    }

    fn seek_relative(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.player.update(cx, |video, cx| {
            let current = video.position();
            let mut target = if forward {
                current.saturating_add(SEEK_STEP)
            } else {
                current.saturating_sub(SEEK_STEP)
            };
            if let Some(duration) = video.duration() {
                target = target.min(duration);
            }
            video.seek(target, cx);
        });
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "space" => self
                .player
                .update(cx, |video, cx| video.toggle_playback(cx)),
            "left" => self.seek_relative(false, cx),
            "right" => self.seek_relative(true, cx),
            "o" | "O" => self.player.update(cx, |video, cx| video.open_dialog(cx)),
            _ => {}
        }
    }
}

impl Render for EmbedScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus_handle, cx);
        }

        // Всё, что нужно от плеера, читается у сущности: без команд ядра,
        // блокировок и опроса. Перерисовка приходит по `cx.notify()` из плеера.
        let (state, position, duration, file_name) = {
            let video = self.player.read(cx);
            let file_name = video
                .path()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "файл не открыт".to_owned());
            (video.state(), video.position(), video.duration(), file_name)
        };

        let time = match duration {
            Some(duration) => format!("{} / {}", clock(position), clock(duration)),
            None => clock(position),
        };
        let play_label = if state.is_playing() {
            "Пауза"
        } else {
            "Играть"
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(gpui::rgb(0x11_11_11))
            .text_color(gpui::rgb(0xee_ee_ee))
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(video_view(&self.player, cx).flex_1().min_h_0())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .bg(gpui::rgb(0x22_22_22))
                    .child(control_button(
                        "embed-open",
                        "Открыть…",
                        cx.listener(Self::open_dialog),
                    ))
                    .child(control_button(
                        "embed-play",
                        play_label,
                        cx.listener(Self::toggle_playback),
                    ))
                    .child(control_button(
                        "embed-back",
                        "−5 с",
                        cx.listener(Self::seek_backward),
                    ))
                    .child(control_button(
                        "embed-forward",
                        "+5 с",
                        cx.listener(Self::seek_forward),
                    ))
                    .child(div().text_sm().child(time))
                    .child(div().text_sm().opacity(0.7).child(file_name)),
            )
    }
}

/// Кнопка примера: обычный `div`, потому что библиотека своего набора не
/// навязывает.
fn control_button(
    id: &'static str,
    label: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let label = label.into();

    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(gpui::rgb(0x33_33_33))
        .cursor_pointer()
        .aria_label(label.clone())
        .hover(|style| style.bg(gpui::rgb(0x44_44_44)))
        .child(label)
        .on_click(on_click)
}

/// Время как `чч:мм:сс`.
fn clock(value: Duration) -> String {
    let total_seconds = value.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
