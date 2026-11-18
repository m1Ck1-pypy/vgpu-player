//! Настройки, которые переживают перезапуск плеера.
//!
//! Формат намеренно простой: по строке `ключ = значение`, `#` — комментарий.
//! Ключей четыре, и ради них не заводим зависимостей вроде `serde` + `toml`:
//! разбор и запись — несколько десятков строк, которые проверяются тестами.
//!
//! Файл лежит у пользователя (`%APPDATA%\vgpu-player\settings.cfg`), а не рядом
//! с exe: портативная папка может быть только для чтения, а настройки логичнее
//! привязать к пользователю, чем к каталогу.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Имя файла настроек.
const FILE_NAME: &str = "settings.cfg";

/// Каталог приложения внутри пользовательского каталога конфигурации.
const APP_DIR: &str = "vgpu-player";

/// Настройки, которые сохраняются между запусками.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Settings {
    /// Начинать воспроизведение сразу после открытия файла.
    pub(crate) autoplay: bool,
    /// Показывать оверлей статистики.
    pub(crate) stats_visible: bool,
    /// Скрывать панель управления после бездействия.
    pub(crate) auto_hide: bool,
    /// Громкость в диапазоне `0..=1`.
    pub(crate) volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autoplay: true,
            stats_visible: false,
            auto_hide: true,
            volume: 1.0,
        }
    }
}

impl Settings {
    /// Путь к файлу настроек пользователя.
    ///
    /// Windows — `%APPDATA%\vgpu-player\settings.cfg`; другие системы —
    /// `XDG_CONFIG_HOME` или `~/.config`. `None` — каталог не определить, и
    /// тогда настройки живут только в памяти.
    pub(crate) fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join(APP_DIR).join(FILE_NAME))
    }

    /// Читает настройки; отсутствие или порча файла дают умолчания.
    pub(crate) fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                warn!(path = %path.display(), %error, "не удалось прочитать настройки");
                Self::default()
            }
        }
    }

    /// Записывает настройки, создавая каталог при необходимости.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.to_text())
    }

    /// Разбирает текст настроек.
    ///
    /// Неизвестные ключи, строки без `=` и значения неверного типа
    /// пропускаются: испорченный файл не должен мешать смотреть видео.
    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "autoplay" => settings.autoplay = parse_bool(value).unwrap_or(settings.autoplay),
                "stats_visible" => {
                    settings.stats_visible = parse_bool(value).unwrap_or(settings.stats_visible);
                }
                "auto_hide" => settings.auto_hide = parse_bool(value).unwrap_or(settings.auto_hide),
                "volume" => settings.volume = parse_volume(value).unwrap_or(settings.volume),
                _ => {}
            }
        }
        settings
    }

    /// Текст файла настроек: ключи в постоянном порядке, чтобы диффы читались.
    fn to_text(self) -> String {
        format!(
            "# VGPU Player settings\n\
             autoplay = {}\n\
             stats_visible = {}\n\
             auto_hide = {}\n\
             volume = {}\n",
            self.autoplay, self.stats_visible, self.auto_hide, self.volume
        )
    }
}

/// `true` или `false` без учёта регистра; остальное — не значение флага.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Громкость из файла: значение вне `0..=1` обрезается, нечисло — пропуск.
fn parse_volume(value: &str) -> Option<f32> {
    let volume: f32 = value.parse().ok()?;
    volume.is_finite().then(|| volume.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "vgpu-player-settings-{}-{name}.cfg",
            std::process::id()
        ))
    }

    #[test]
    fn parses_all_keys() {
        let text = "# настройки\n\
                    autoplay = false\n\
                    stats_visible = true\n\
                    auto_hide = false\n\
                    volume = 0.35\n";

        assert_eq!(
            Settings::parse(text),
            Settings {
                autoplay: false,
                stats_visible: true,
                auto_hide: false,
                volume: 0.35,
            }
        );
    }

    #[test]
    fn ignores_unknown_keys_and_broken_lines() {
        let text = "volume = abc\n\
                    stats_visible = yes\n\
                    auto_hide\n\
                    = 1\n\
                    playlist = none\n\
                    autoplay = TRUE\n";

        let settings = Settings::parse(text);

        // Единственная корректная строка — `autoplay = TRUE`.
        assert_eq!(
            settings,
            Settings {
                autoplay: true,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn clamps_volume_to_range() {
        assert_eq!(Settings::parse("volume = 2").volume, 1.0);
        assert_eq!(Settings::parse("volume = -0.5").volume, 0.0);
        assert_eq!(Settings::parse("volume = NaN").volume, 1.0);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = temp_path("missing");
        let _ = fs::remove_file(&path);

        assert_eq!(Settings::load(&path), Settings::default());
    }

    #[test]
    fn save_then_load_restores_the_same_values() {
        let path = temp_path("roundtrip");
        let settings = Settings {
            autoplay: false,
            stats_visible: true,
            auto_hide: false,
            volume: 0.423,
        };

        settings.save(&path).expect("записать настройки");
        let loaded = Settings::load(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(loaded, settings);
    }
}
