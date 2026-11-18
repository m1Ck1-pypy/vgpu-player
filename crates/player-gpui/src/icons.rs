//! Иконки кнопок.
//!
//! Рисунки — монохромные SVG, встроенные в бинарник: внешние файлы и подстановка
//! ассетов не нужны. GPUI растеризует SVG в альфа-маску и закрашивает её цветом
//! текста элемента, поэтому один и тот же рисунок годится и для тёмной, и для
//! светлой кнопки.
//!
//! Заменить рисунок можно, не трогая разметку: достаточно поправить строку в
//! [`Icon::svg`]. Требования — `viewBox="0 0 24 24"`, ширина и высота `24`,
//! заливка или обводка любого непрозрачного цвета.

use gpui::{Rgba, Svg, prelude::*, px, svg};

/// Размер иконки в панели управления.
pub(crate) const ICON_SIZE: f32 = 20.0;

/// Размер иконки на большой кнопке поверх кадра.
pub(crate) const LARGE_ICON_SIZE: f32 = 30.0;

/// Иконки, которые нужны кнопкам.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Icon {
    /// Воспроизвести (треугольник).
    Play,
    /// Пауза (две полосы).
    Pause,
    /// Начать заново (стрелка по кругу).
    Replay,
    /// Звук включён (динамик с волнами).
    Volume,
    /// Звук выключен (динамик с крестом).
    Mute,
    /// Ещё (три точки) — меню панели.
    More,
    /// Развернуть на весь экран (уголки наружу).
    Fullscreen,
    /// Выйти из полноэкранного режима (уголки внутрь).
    FullscreenExit,
}

impl Icon {
    /// SVG-рисунок иконки.
    ///
    /// Цвет заливки в самом файле не важен: `Svg` берёт из рисунка только
    /// альфа-маску, а цвет задаёт элемент (`text_color`).
    fn svg(self) -> &'static [u8] {
        match self {
            Self::Play => br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M8 4.8c0-.83.9-1.34 1.6-.91l9.9 6.2a1.07 1.07 0 0 1 0 1.82l-9.9 6.2c-.7.43-1.6-.08-1.6-.91z"/></svg>"#,
            Self::Pause => br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><rect x="6.5" y="4.5" width="4" height="15" rx="1.5"/><rect x="13.5" y="4.5" width="4" height="15" rx="1.5"/></svg>"#,
            Self::Replay => br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M12 4.6a7.4 7.4 0 1 1-7.4 7.4" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round"/><path d="M12 1.6 7.9 4.6 12 7.6z"/></svg>"##,
            Self::Volume => br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M4 9.5h3.2L12 5.4v13.2L7.2 14.5H4z"/><path d="M15.5 8.7a4.6 4.6 0 0 1 0 6.6" fill="none" stroke="#000" stroke-width="1.9" stroke-linecap="round"/><path d="M18.2 6.2a8 8 0 0 1 0 11.6" fill="none" stroke="#000" stroke-width="1.9" stroke-linecap="round"/></svg>"##,
            Self::Mute => br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M4 9.5h3.2L12 5.4v13.2L7.2 14.5H4z"/><path d="M15.8 9.5l5 5m0-5-5 5" fill="none" stroke="#000" stroke-width="1.9" stroke-linecap="round"/></svg>"##,
            Self::More => br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><circle cx="5" cy="12" r="1.9"/><circle cx="12" cy="12" r="1.9"/><circle cx="19" cy="12" r="1.9"/></svg>"#,
            Self::Fullscreen => br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M9 4H5.5A1.5 1.5 0 0 0 4 5.5V9" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M15 4h3.5A1.5 1.5 0 0 1 20 5.5V9" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M20 15v3.5a1.5 1.5 0 0 1-1.5 1.5H15" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M9 20H5.5A1.5 1.5 0 0 1 4 18.5V15" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"##,
            Self::FullscreenExit => br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24"><path d="M9 4v3.5A1.5 1.5 0 0 1 7.5 9H4" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M15 4v3.5A1.5 1.5 0 0 0 16.5 9H20" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M20 15h-3.5A1.5 1.5 0 0 0 15 16.5V20" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/><path d="M4 15h3.5A1.5 1.5 0 0 1 9 16.5V20" fill="none" stroke="#000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/></svg>"##,
        }
    }
}

/// Иконка заданного размера и цвета.
pub(crate) fn icon(kind: Icon, size: f32, color: Rgba) -> Svg {
    svg()
        .data(kind.svg())
        .w(px(size))
        .h(px(size))
        .text_color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 8] = [
        Icon::Play,
        Icon::Pause,
        Icon::Replay,
        Icon::Volume,
        Icon::Mute,
        Icon::More,
        Icon::Fullscreen,
        Icon::FullscreenExit,
    ];

    #[test]
    fn every_icon_is_a_sized_svg() {
        for icon in ALL {
            let data = std::str::from_utf8(icon.svg()).expect("SVG — это UTF-8");

            assert!(data.starts_with("<svg"), "{icon:?}: нет корневого <svg>");
            assert!(data.ends_with("</svg>"), "{icon:?}: <svg> не закрыт");
            assert!(
                data.contains(r#"viewBox="0 0 24 24""#),
                "{icon:?}: нужен viewBox 24×24"
            );
            assert!(
                data.contains(r#"width="24""#) && data.contains(r#"height="24""#),
                "{icon:?}: без width/height usvg не знает размер"
            );
        }
    }
}
