//! Панель «Открыть…»: выбор «Файл» / «URL» и поле ввода ссылки.
//!
//! Логика здесь чистая: клавиши приходят строками (`key`, `key_char`), а не
//! типами GPUI, поэтому редактирование проверяется обычными тестами. Буфер
//! обмена и рисование панели живут в [`crate::app`]: сама панель ничего не
//! знает про окно.

/// Сколько символов слева от курсора помещается в поле.
///
/// Ссылки длиннее поля не редкость, а курсор должен оставаться на виду:
/// показываем хвост уже введённого и начало остатка.
const VISIBLE_BEFORE: usize = 48;

/// Сколько символов справа от курсора помещается в поле.
const VISIBLE_AFTER: usize = 24;

/// Пункт панели, который можно выбрать.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PanelChoice {
    /// Системный диалог выбора файла.
    #[default]
    File,
    /// Поле ввода ссылки.
    Url,
}

impl PanelChoice {
    /// Другой пункт: панель переключается стрелками и `Tab`.
    fn other(self) -> Self {
        match self {
            Self::File => Self::Url,
            Self::Url => Self::File,
        }
    }
}

/// Поле ввода ссылки: текст и позиция курсора в байтах.
///
/// Курсор всегда стоит на границе символа: `String` в Rust индексируется
/// байтами, и разрезать многобайтовый символ нельзя — это паника.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UrlInput {
    text: String,
    cursor: usize,
}

impl UrlInput {
    /// Введённый текст целиком.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Текст слева от курсора, который помещается в поле.
    pub(crate) fn visible_before(&self) -> &str {
        tail(&self.text[..self.cursor], VISIBLE_BEFORE)
    }

    /// Текст справа от курсора, который помещается в поле.
    pub(crate) fn visible_after(&self) -> &str {
        head(&self.text[self.cursor..], VISIBLE_AFTER)
    }

    /// Вставляет текст в позицию курсора.
    ///
    /// Управляющие символы отбрасываются: из буфера обмена может прийти
    /// перевод строки или табуляция, которым в поле не место.
    fn insert(&mut self, value: &str) {
        let clean: String = value.chars().filter(|ch| !ch.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Удаляет символ слева от курсора.
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = prev_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Удаляет символ справа от курсора.
    fn delete(&mut self) {
        if self.cursor == self.text.len() {
            return;
        }
        let end = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    /// Сдвигает курсор на символ влево.
    fn move_left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    /// Сдвигает курсор на символ вправо.
    fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }
}

/// Состояние панели «Открыть…».
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum OpenPanel {
    /// Панель не показана.
    #[default]
    Closed,
    /// Выбор источника: стрелки переключают, `Enter` подтверждает.
    Choose(PanelChoice),
    /// Ввод ссылки.
    Url(UrlInput),
}

/// Что приложение должно сделать после клавиши в панели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelAction {
    /// Клавиша обработана панелью.
    Handled,
    /// Панель не знает эту клавишу.
    Ignored,
    /// Закрыть панель.
    Close,
    /// Открыть системный диалог выбора файла.
    OpenFile,
    /// Открыть ссылку из поля ввода.
    OpenUrl(String),
    /// Вставить содержимое буфера обмена в поле.
    Paste,
}

impl OpenPanel {
    /// Показана ли панель.
    pub(crate) fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    /// Открывает панель на выборе источника.
    pub(crate) fn reset(&mut self) {
        *self = Self::Choose(PanelChoice::default());
    }

    /// Закрывает панель и забывает введённое.
    pub(crate) fn close(&mut self) {
        *self = Self::Closed;
    }

    /// Выбранный пункт; `None` — панель не на выборе.
    pub(crate) fn choice(&self) -> Option<PanelChoice> {
        match self {
            Self::Choose(choice) => Some(*choice),
            _ => None,
        }
    }

    /// Введённая ссылка; `None` — панель не на вводе.
    pub(crate) fn input(&self) -> Option<&UrlInput> {
        match self {
            Self::Url(input) => Some(input),
            _ => None,
        }
    }

    /// Переключает панель на ввод ссылки.
    pub(crate) fn select_url(&mut self) {
        *self = Self::Url(UrlInput::default());
    }

    /// Вставляет текст в поле; вне режима ввода ничего не делает.
    pub(crate) fn insert(&mut self, value: &str) {
        if let Self::Url(input) = self {
            input.insert(value);
        }
    }

    /// Обрабатывает клавишу окна.
    ///
    /// `ctrl` — нажата управляющая клавиша (на macOS — командная); из неё
    /// сейчас имеет значение только `Ctrl+V`.
    pub(crate) fn handle_key(
        &mut self,
        key: &str,
        ctrl: bool,
        key_char: Option<&str>,
    ) -> PanelAction {
        match self {
            Self::Closed => PanelAction::Ignored,
            Self::Choose(choice) => match key {
                "escape" => {
                    self.close();
                    PanelAction::Close
                }
                "left" | "right" | "up" | "down" | "tab" => {
                    *choice = choice.other();
                    PanelAction::Handled
                }
                "enter" => match choice {
                    PanelChoice::File => PanelAction::OpenFile,
                    PanelChoice::Url => {
                        self.select_url();
                        PanelAction::Handled
                    }
                },
                _ => PanelAction::Ignored,
            },
            Self::Url(input) => match key {
                "escape" => {
                    self.close();
                    PanelAction::Close
                }
                "enter" => {
                    let text = input.text().trim();
                    if text.is_empty() {
                        // Пустая строка не открывает ничего: панель остаётся.
                        PanelAction::Handled
                    } else {
                        PanelAction::OpenUrl(text.to_owned())
                    }
                }
                "backspace" => {
                    input.backspace();
                    PanelAction::Handled
                }
                "delete" => {
                    input.delete();
                    PanelAction::Handled
                }
                "left" => {
                    input.move_left();
                    PanelAction::Handled
                }
                "right" => {
                    input.move_right();
                    PanelAction::Handled
                }
                "home" => {
                    input.cursor = 0;
                    PanelAction::Handled
                }
                "end" => {
                    input.cursor = input.text.len();
                    PanelAction::Handled
                }
                "v" | "V" if ctrl => PanelAction::Paste,
                _ => match key_char {
                    Some(value) => {
                        input.insert(value);
                        PanelAction::Handled
                    }
                    None => PanelAction::Ignored,
                },
            },
        }
    }
}

/// Начало строки не длиннее `limit` символов.
fn head(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Конец строки не длиннее `limit` символов.
fn tail(text: &str, limit: usize) -> &str {
    let skip = text.chars().count().saturating_sub(limit);
    match text.char_indices().nth(skip) {
        Some((index, _)) => &text[index..],
        None => text,
    }
}

/// Ближайшая граница символа слева от `index`.
fn prev_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// Ближайшая граница символа справа от `index`.
fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(i, _)| index + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Поле с введённым текстом и курсором в конце.
    fn typed(text: &str) -> UrlInput {
        let mut value = UrlInput::default();
        value.insert(text);
        value
    }

    /// Панель в режиме ввода с текстом.
    fn url_panel(text: &str) -> OpenPanel {
        let mut panel = OpenPanel::default();
        panel.select_url();
        panel.insert(text);
        panel
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut input = typed("https://a.b/");
        input.move_left();
        input.insert("x");
        assert_eq!(input.text(), "https://a.bx/");
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut input = typed("abc");
        input.cursor = 0;
        input.backspace();
        assert_eq!(input.text(), "abc");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn backspace_removes_a_whole_char() {
        let mut input = typed("файл");
        input.backspace();
        assert_eq!(input.text(), "фай");
        assert_eq!(input.cursor, "фай".len());
    }

    #[test]
    fn delete_at_end_does_nothing() {
        let mut input = typed("abc");
        input.delete();
        assert_eq!(input.text(), "abc");
    }

    #[test]
    fn delete_removes_the_char_under_the_cursor() {
        let mut input = typed("абв");
        input.cursor = "а".len();
        input.delete();
        assert_eq!(input.text(), "ав");
        assert_eq!(input.cursor, "а".len());
    }

    #[test]
    fn arrows_step_over_multibyte_chars() {
        let mut input = typed("фыв");
        input.move_left();
        assert_eq!(input.cursor, "фы".len());
        input.move_left();
        assert_eq!(input.cursor, "ф".len());
        input.move_left();
        assert_eq!(input.cursor, 0);
        input.move_right();
        assert_eq!(input.cursor, "ф".len());
    }

    #[test]
    fn home_and_end_move_to_the_edges() {
        let mut input = typed("https://a.b/c");
        input.move_left();
        assert_eq!(input.cursor, "https://a.b/".len());

        let mut panel = url_panel("https://a.b/c");
        assert_eq!(panel.handle_key("home", false, None), PanelAction::Handled);
        assert_eq!(panel.input().unwrap().cursor, 0);
        assert_eq!(panel.handle_key("end", false, None), PanelAction::Handled);
        assert_eq!(panel.input().unwrap().cursor, "https://a.b/c".len());
    }

    #[test]
    fn paste_drops_control_chars() {
        let mut input = UrlInput::default();
        input.insert("https://a.b/c\n\t ");
        assert_eq!(input.text(), "https://a.b/c ");
    }

    #[test]
    fn long_text_shows_the_cursor_neighbourhood() {
        let long = "x".repeat(100);
        let input = typed(&long);
        assert_eq!(input.visible_before().len(), VISIBLE_BEFORE);
        assert!(input.visible_after().is_empty());

        let mut input = typed(&long);
        input.cursor = 50;
        assert_eq!(input.visible_before(), &long[2..50]);
        assert_eq!(input.visible_after(), &long[50..74]);
    }

    #[test]
    fn short_text_is_visible_whole() {
        let input = typed("ab");
        assert_eq!(input.visible_before(), "ab");
        assert_eq!(input.visible_after(), "");
    }

    #[test]
    fn typing_letters_reaches_the_panel() {
        let mut panel = OpenPanel::default();
        panel.reset();
        // Стрелка выбирает «URL», `Enter` открывает поле ввода.
        assert_eq!(panel.handle_key("right", false, None), PanelAction::Handled);
        assert_eq!(panel.handle_key("enter", false, None), PanelAction::Handled);
        assert_eq!(
            panel.handle_key("h", false, Some("h")),
            PanelAction::Handled
        );
        assert_eq!(
            panel.handle_key("t", false, Some("t")),
            PanelAction::Handled
        );
        assert_eq!(panel.input().unwrap().text(), "ht");
    }

    #[test]
    fn space_is_typed() {
        let mut panel = url_panel("a");
        assert_eq!(
            panel.handle_key("space", false, Some(" ")),
            PanelAction::Handled
        );
        assert_eq!(panel.input().unwrap().text(), "a ");
    }

    #[test]
    fn enter_with_empty_text_does_nothing() {
        let mut panel = url_panel("   ");
        assert_eq!(panel.handle_key("enter", false, None), PanelAction::Handled);
        assert!(panel.is_open());
    }

    #[test]
    fn enter_returns_the_trimmed_link() {
        let mut panel = url_panel("  https://a.b/c  ");
        assert_eq!(
            panel.handle_key("enter", false, None),
            PanelAction::OpenUrl("https://a.b/c".to_owned())
        );
    }

    #[test]
    fn escape_closes_from_both_states() {
        let mut panel = OpenPanel::default();
        panel.reset();
        assert_eq!(panel.handle_key("escape", false, None), PanelAction::Close);
        assert!(!panel.is_open());

        let mut panel = url_panel("https://a.b");
        assert_eq!(panel.handle_key("escape", false, None), PanelAction::Close);
        assert!(!panel.is_open());
    }

    #[test]
    fn arrows_toggle_the_choice() {
        let mut panel = OpenPanel::default();
        panel.reset();
        assert_eq!(panel.choice(), Some(PanelChoice::File));
        assert_eq!(panel.handle_key("right", false, None), PanelAction::Handled);
        assert_eq!(panel.choice(), Some(PanelChoice::Url));
        assert_eq!(panel.handle_key("tab", false, None), PanelAction::Handled);
        assert_eq!(panel.choice(), Some(PanelChoice::File));
    }

    #[test]
    fn enter_on_file_asks_for_the_dialog() {
        let mut panel = OpenPanel::default();
        panel.reset();
        assert_eq!(
            panel.handle_key("enter", false, None),
            PanelAction::OpenFile
        );
    }

    #[test]
    fn ctrl_v_asks_for_the_clipboard() {
        let mut panel = url_panel("");
        assert_eq!(panel.handle_key("v", true, None), PanelAction::Paste);
        // Без Ctrl это обычная буква.
        assert_eq!(
            panel.handle_key("v", false, Some("v")),
            PanelAction::Handled
        );
        assert_eq!(panel.input().unwrap().text(), "v");
    }

    #[test]
    fn unknown_keys_are_ignored_while_closed() {
        let mut panel = OpenPanel::default();
        assert_eq!(
            panel.handle_key("space", false, Some(" ")),
            PanelAction::Ignored
        );
        assert!(!panel.is_open());
    }

    #[test]
    fn closed_panel_ignores_paste() {
        let mut panel = OpenPanel::default();
        panel.insert("https://a.b");
        assert_eq!(panel, OpenPanel::Closed);
    }
}
