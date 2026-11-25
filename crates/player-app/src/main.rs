//! Демонстрационное приложение: открывает окно плеера.
//!
//! Здесь намеренно нет логики — всё живёт в библиотеке `player-gpui`.
//! Приложение нужно, чтобы библиотеку было видно работающей и чтобы у
//! воркспейса был свой запускаемый бинарник.
//!
//! Аргументы разбираются вручную: у CLI по-прежнему мало параметров, `clap`
//! появится вместе с диалогом открытия и плейлистом на этапе 9.

// В release на Windows не показываем консольное окно: это GUI-приложение.
// В debug консоль остаётся, чтобы видеть stderr и логи.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::ffi::OsString;
use std::process::ExitCode;

use player_gpui::{HttpOptions, PlayerOptions, Source, SourceError};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

/// Короткая подсказка — одна для всех ошибок разбора аргументов.
const USAGE: &str = "usage: vgpu [<file|url>] [--volume <0..100>] [--referrer|--referer <url>] [--user-agent <string>] [-h|--help]";

/// Полная справка: только те возможности, что есть в этой версии.
const HELP: &str = "\
VGPU Player — видеоплеер с окном и запуском из консоли

usage: vgpu [<file|url>] [--volume <0..100>] [--referrer <url>] [--user-agent <string>]

Аргументы:
  <file|url>         локальный путь, file: или http(s)-ссылка;
                     MP4/MKV/WebM/MP3 и HLS (VOD и live).
                     Без аргумента открывается пустое окно.
  --volume <0..100>  громкость на этот запуск в процентах;
                     сохранённую настройку не меняет.
  --referrer <url>   заголовок Referer для всех HTTP-запросов источника;
                     алиас --referer. Например, для RuTube:
                     --referrer https://rutube.ru/
  --user-agent <str> заголовок User-Agent; по умолчанию vgpu-player/<версия>.
  -h, -H, --help, /?  эта справка.

Ссылки с пробелами, скобками и «умными» кавычками передавайте одним
аргументом: vgpu \"<URL>\". В PowerShell можно так: vgpu --% <URL>.

Управление в окне:
  Space / клик       пауза и продолжение
  <- / ->            перемотка на 5 секунд
  F                  полноэкранный режим (Esc — выход)
  O / Ctrl+O         открыть файл
  колесо мыши        громкость (±1 %)

Код возврата: 0 — успех, 1 — ошибка запуска или разбора аргументов.";

/// Что делать после разбора аргументов.
#[derive(Debug, PartialEq)]
enum Action {
    Help,
    Play {
        source: Option<Source>,
        volume: Option<f32>,
        http: HttpOptions,
    },
}

/// Причина отказа разбора.
#[derive(Debug)]
enum ArgsError {
    /// Общая ошибка с пояснением; печатается вместе с `USAGE`.
    Usage(String),
    /// Строка не похожа ни на путь, ни на URL.
    Source(SourceError),
}

impl std::fmt::Display for ArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(reason) => f.write_str(reason),
            Self::Source(error) => error.fmt(f),
        }
    }
}

fn main() -> ExitCode {
    init_tracing();

    let action = match parse_args(std::env::args_os().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            print_error(&format!("vgpu: {error}\n{USAGE}\nподробнее: vgpu --help"));
            return ExitCode::FAILURE;
        }
    };

    let Action::Play {
        source,
        volume,
        http,
    } = action
    else {
        print_help();
        return ExitCode::SUCCESS;
    };

    match run(source, volume, http) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            error!(error = %format!("{failure:#}"), "vgpu завершился с ошибкой");
            print_error(&format!("vgpu: {failure:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Запускает окно. `anyhow` здесь — чтобы `main` оставался коротким, а цепочка
/// причин печаталась одним `{:#}`.
fn run(source: Option<Source>, volume: Option<f32>, http: HttpOptions) -> anyhow::Result<()> {
    match source.as_ref() {
        Some(source) => info!(source = %source, volume = ?volume, "запуск плеера"),
        None => info!(volume = ?volume, "запуск плеера без источника"),
    }

    player_gpui::run_app_source_with(source, PlayerOptions { volume, http })?;
    Ok(())
}

/// Разбирает аргументы в [`Action`].
///
/// Источник и `--volume` необязательны и могут идти в любом порядке.
/// `--help` побеждает остальные аргументы: справку печатаем, не проверяя
/// остальное.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, ArgsError> {
    let mut args = args.into_iter();
    let mut source = None;
    let mut volume = None;
    let mut referrer = None;
    let mut user_agent = None;

    while let Some(argument) = args.next() {
        // Не-UTF-8 строка не может быть флагом — это путь.
        let Some(text) = argument.to_str() else {
            take_source(&mut source, argument)?;
            continue;
        };

        match text {
            // `-H` — вариант из исходного требования; `-h` — стандарт.
            "-h" | "-H" | "--help" | "/?" => return Ok(Action::Help),
            "--volume" => {
                if volume.is_some() {
                    return Err(ArgsError::Usage(
                        "параметр --volume указан дважды".to_owned(),
                    ));
                }
                let value = args
                    .next()
                    .ok_or_else(|| ArgsError::Usage("у --volume нет значения".to_owned()))?;
                volume = Some(parse_volume(&value)?);
            }
            "--referrer" | "--referer" => {
                if referrer.is_some() {
                    return Err(ArgsError::Usage(
                        "параметр --referrer указан дважды".to_owned(),
                    ));
                }
                let value = args
                    .next()
                    .ok_or_else(|| ArgsError::Usage("у --referrer нет значения".to_owned()))?;
                referrer = Some(value);
            }
            "--user-agent" => {
                if user_agent.is_some() {
                    return Err(ArgsError::Usage(
                        "параметр --user-agent указан дважды".to_owned(),
                    ));
                }
                let value = args
                    .next()
                    .ok_or_else(|| ArgsError::Usage("у --user-agent нет значения".to_owned()))?;
                user_agent = Some(value);
            }
            // Дефис в начале — попытка флага; путь так не начинается.
            // Одиночный `-` оставляем источнику, чтобы ошибку выдал `Source`.
            _ if text.starts_with('-') && text != "-" => {
                return Err(ArgsError::Usage(format!("неизвестный параметр {text}")));
            }
            _ => take_source(&mut source, argument)?,
        }
    }

    let mut http = HttpOptions::new();
    if let Some(value) = referrer {
        http = http
            .with_referer(value.to_string_lossy())
            .map_err(|error| ArgsError::Usage(error.to_string()))?;
    }
    if let Some(value) = user_agent {
        http = http
            .with_user_agent(value.to_string_lossy())
            .map_err(|error| ArgsError::Usage(error.to_string()))?;
    }

    Ok(Action::Play {
        source,
        volume,
        http,
    })
}

/// Запоминает источник.
///
/// Повторный позиционный аргумент не ошибка: оболочка Windows дописывает
/// остальные выбранные файлы к `"%1"`, а плеер открывает один. Первый
/// источник побеждает.
fn take_source(slot: &mut Option<Source>, argument: OsString) -> Result<(), ArgsError> {
    if slot.is_some() {
        return Ok(());
    }
    let Some(source) = source_from_argument(argument).map_err(ArgsError::Source)? else {
        return Ok(());
    };
    *slot = Some(source);
    Ok(())
}

/// `--volume` задаётся процентами `0…100`, а ядро и настройки живут в `0…1`.
fn parse_volume(argument: &OsString) -> Result<f32, ArgsError> {
    const INVALID: &str = "громкость задаётся числом от 0 до 100";

    let percent = argument
        .to_str()
        .and_then(|text| text.parse::<f32>().ok())
        .filter(|percent| percent.is_finite() && (0.0..=100.0).contains(percent))
        .ok_or_else(|| ArgsError::Usage(INVALID.to_owned()))?;
    Ok(percent / 100.0)
}

/// Источник из одного аргумента: без обрамляющих кавычек.
///
/// Так Windows запускает плеер по ассоциации — из «Открыть с помощью» и по
/// двойному клику в Проводнике: `vgpu.exe "C:\video\movie.mp4"`.
/// Парсер командной строки Rust кавычки обычно снимает сам; повторное снятие
/// нужно для лаунчеров, которые передают строку целиком и экранируют её сами.
/// Кроме ASCII-кавычек снимаются парные типографские: команду часто копируют
/// из документации или мессенджера.
/// `https://…` и `file:` распознаются как URL, а не как путь.
fn source_from_argument(argument: OsString) -> Result<Option<Source>, SourceError> {
    let text = argument.to_string_lossy();
    let trimmed = strip_typographic_quotes(text.trim_matches('"'));
    if trimmed.is_empty() {
        return Ok(None);
    }

    // Без кавычек отдаём исходный `OsString`: `to_string_lossy` испортил бы
    // путь с символами вне Unicode.
    let value = if trimmed.len() == text.len() {
        argument
    } else {
        OsString::from(trimmed)
    };
    Source::from_os_string(value).map(Some)
}

/// Снимает парные типографские кавычки: `“…”`, `‘…’`, `«…»`.
fn strip_typographic_quotes(text: &str) -> &str {
    const PAIRS: [(char, char); 3] = [('“', '”'), ('‘', '’'), ('«', '»')];

    for (open, close) in PAIRS {
        if let Some(rest) = text.strip_prefix(open)
            && let Some(inner) = rest.strip_suffix(close)
        {
            return inner;
        }
    }

    text
}

/// Печать справки. В release на Windows `println!` ничего не выводит: у
/// GUI-процесса нет консоли, поэтому идём через Win32.
fn print_help() {
    #[cfg(all(windows, not(debug_assertions)))]
    win_console::show(HELP);
    #[cfg(not(all(windows, not(debug_assertions))))]
    println!("{HELP}");
}

/// Печать ошибки — тем же путём, что и справку: в release на Windows иначе её
/// никто не увидит.
fn print_error(text: &str) {
    #[cfg(all(windows, not(debug_assertions)))]
    win_console::show(text);
    #[cfg(not(all(windows, not(debug_assertions))))]
    eprintln!("{text}");
}

/// Вывод в консоль для GUI-сборки Windows.
///
/// `windows_subsystem = "windows"` не даёт процессу консоли: `println!` молча
/// ничего не делает. Подключаемся к консоли родителя, а если её нет (двойной
/// клик в Проводнике) — показываем окно сообщения.
///
/// Внешних крейтов нет: объявлены только те пять функций Win32, которые нужны.
/// Модуль собирается во всех сборках под Windows, чтобы `cargo check` ловил
/// ошибки FFI, но вызывается только из release.
#[cfg(windows)]
mod win_console {
    // Модуль собирается и в debug, чтобы `cargo check` видел ошибки FFI, но
    // вызывается только из release-сборки.
    #![allow(dead_code)]

    use std::ffi::c_void;
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use std::ptr;

    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const STD_OUTPUT_HANDLE: u32 = -11_i32 as u32;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONINFORMATION: u32 = 0x0000_0040;

    /// `WriteConsoleW` получает символы пакетами: слишком длинную строку
    /// разбиваем, чтобы не упираться в ограничение вызова.
    const CHUNK: usize = 8_192;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn WriteConsoleW(
            handle: *mut c_void,
            buffer: *const u16,
            length: u32,
            written: *mut u32,
            reserved: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }

    /// Печатает текст: в перенаправленный вывод, в консоль или в окно.
    pub(super) fn show(text: &str) {
        // `vgpu --help > file.txt` приходит готовым дескриптором файла или
        // канала — пишем UTF-8 в него и консоль не трогаем.
        if let Some(handle) = redirected_output() {
            write_utf8(handle, text);
            return;
        }

        if let Some(handle) = console_output() {
            // Приглашение оболочки уже напечатано — начинаем с новой строки.
            write_wide(handle, "\r\n");
            write_wide(handle, text);
            write_wide(handle, "\r\n");
            unsafe { CloseHandle(handle) };
            return;
        }

        message_box(text);
    }

    /// Дескриптор вывода, если процесс запущен с перенаправлением.
    fn redirected_output() -> Option<*mut c_void> {
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut mode = 0_u32;
        // У консоли режим читается, у файла и канала — нет.
        if unsafe { GetConsoleMode(handle, &mut mode) } != 0 {
            return None;
        }
        Some(handle)
    }

    /// Подключается к консоли родителя и открывает `CONOUT$`.
    ///
    /// `CONOUT$` нужен потому, что стандартные дескрипторы GUI-процесса
    /// остаются пустыми даже после `AttachConsole`.
    fn console_output() -> Option<*mut c_void> {
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
            return None;
        }

        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null_mut(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
            return None;
        }
        Some(handle)
    }

    fn write_wide(handle: *mut c_void, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        for chunk in units.chunks(CHUNK) {
            let mut written = 0_u32;
            let ok = unsafe {
                WriteConsoleW(
                    handle,
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                break;
            }
        }
    }

    fn write_utf8(handle: *mut c_void, text: &str) {
        // Дескриптор принадлежит родителю: закрывать его нельзя, поэтому
        // `ManuallyDrop`.
        let mut file =
            std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(handle) });
        let _ = file.write_all(text.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }

    fn message_box(text: &str) {
        let body: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let caption: Vec<u16> = "vgpu\0".encode_utf16().collect();
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                body.as_ptr(),
                caption.as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
}

/// Подписчик логов: по умолчанию только предупреждения и ошибки, уровень
/// меняется переменной `RUST_LOG` (например, `RUST_LOG=info`).
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// Источник и громкость из удачного разбора.
    fn play(items: &[&str]) -> (Option<Source>, Option<f32>) {
        let (source, volume, _) = play_with_http(items);
        (source, volume)
    }

    fn play_with_http(items: &[&str]) -> (Option<Source>, Option<f32>, HttpOptions) {
        match parse_args(args(items)).expect("аргументы разобраны") {
            Action::Play {
                source,
                volume,
                http,
            } => (source, volume, http),
            Action::Help => panic!("ожидался запуск, а не справка"),
        }
    }

    #[test]
    fn takes_the_file_passed_by_the_shell() {
        let (source, volume) = play(&[r"C:\video\movie.mp4"]);

        assert_eq!(
            source,
            Some(Source::File(PathBuf::from(r"C:\video\movie.mp4")))
        );
        assert_eq!(volume, None);
    }

    #[test]
    fn strips_quotes_around_a_path_with_spaces() {
        let (source, _) = play(&[r#""C:\my video\a b.mkv""#]);

        assert_eq!(
            source,
            Some(Source::File(PathBuf::from(r"C:\my video\a b.mkv")))
        );
    }

    #[test]
    fn strips_typographic_quotes_around_a_source() {
        let (source, _) = play(&["“C:\\video\\a b.mp4”"]);
        assert_eq!(
            source,
            Some(Source::File(PathBuf::from(r"C:\video\a b.mp4")))
        );

        let (source, _) = play(&["«file:///D:/video/a.mp4»"]);
        assert_eq!(source, Some(Source::File(PathBuf::from("D:/video/a.mp4"))));
    }

    #[test]
    fn url_is_not_treated_as_a_path() {
        let (source, _) = play(&["https://example.com/video.mp4?token=1"]);

        assert_eq!(
            source,
            Some(Source::Url(
                "https://example.com/video.mp4?token=1".to_owned()
            ))
        );
    }

    #[test]
    fn launch_without_a_file_gives_nothing() {
        assert_eq!(play(&[]), (None, None));
        assert_eq!(play(&[""]), (None, None));
        assert_eq!(play(&[r#"""""#]), (None, None));
    }

    #[test]
    fn help_flags_do_not_need_a_source() {
        for flag in ["-h", "-H", "--help", "/?"] {
            assert_eq!(parse_args(args(&[flag])).expect("справка"), Action::Help);
        }
        // Справка побеждает остальные аргументы.
        assert_eq!(
            parse_args(args(&["--help", "movie.mp4"])).expect("справка"),
            Action::Help
        );
    }

    #[test]
    fn volume_can_go_before_or_after_the_source() {
        let (source, volume) = play(&["--volume", "30", "https://example.com/a.m3u8"]);
        assert_eq!(
            source,
            Some(Source::Url("https://example.com/a.m3u8".to_owned()))
        );
        assert!((volume.expect("громкость") - 0.30).abs() < f32::EPSILON);

        let (source, volume) = play(&["https://example.com/a.m3u8", "--volume", "0"]);
        assert!(source.is_some());
        assert_eq!(volume, Some(0.0));
    }

    #[test]
    fn volume_without_a_source_is_allowed() {
        let (source, volume) = play(&["--volume", "100"]);

        assert_eq!(source, None);
        assert_eq!(volume, Some(1.0));
    }

    #[test]
    fn volume_accepts_fractional_percent() {
        let (_, volume) = play(&["--volume", "12.5"]);

        assert!((volume.expect("громкость") - 0.125).abs() < f32::EPSILON);
    }

    #[test]
    fn volume_out_of_range_is_an_error() {
        for value in ["101", "-1", "abc", "NaN", "inf", ""] {
            let result = parse_args(args(&["--volume", value]));
            assert!(
                matches!(result, Err(ArgsError::Usage(_))),
                "значение {value:?} должно быть отклонено"
            );
        }

        assert!(matches!(
            parse_args(args(&["--volume"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(args(&["--volume", "10", "--volume", "20"])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn unknown_option_is_rejected_but_extra_sources_are_ignored() {
        assert!(matches!(
            parse_args(args(&["--mute"])),
            Err(ArgsError::Usage(_))
        ));

        // Так оболочка Windows передаёт остальные файлы из мультивыбора.
        assert_eq!(
            play(&["a.mp4", "b.mp4"]),
            (Some(Source::File(PathBuf::from("a.mp4"))), None)
        );
        assert_eq!(
            play(&["a.mp4", "не путь", "--volume", "50"]),
            (Some(Source::File(PathBuf::from("a.mp4"))), Some(0.5))
        );
    }

    #[test]
    fn referrer_and_user_agent_are_parsed() {
        let (source, _, http) = play_with_http(&[
            "https://example.com/video.m3u8",
            "--referrer",
            "https://rutube.ru/",
            "--user-agent",
            "Mozilla/5.0",
        ]);

        assert!(source.is_some());
        assert_eq!(http.header("Referer"), Some("https://rutube.ru/"));
        assert_eq!(http.header("User-Agent"), Some("Mozilla/5.0"));
    }

    #[test]
    fn referrer_alias_is_accepted() {
        let (_, _, http) = play_with_http(&["a.mp4", "--referer", "https://rutube.ru/"]);

        assert_eq!(http.header("Referer"), Some("https://rutube.ru/"));
    }

    #[test]
    fn header_flags_reject_duplicates_and_missing_values() {
        let cases: &[&[&str]] = &[
            &[
                "a.mp4",
                "--referrer",
                "https://rutube.ru/",
                "--referrer",
                "https://x/",
            ],
            &[
                "a.mp4",
                "--referer",
                "https://a/",
                "--referrer",
                "https://b/",
            ],
            &["a.mp4", "--user-agent", "one", "--user-agent", "two"],
            &["a.mp4", "--referrer"],
            &["a.mp4", "--user-agent"],
        ];

        for items in cases {
            assert!(
                matches!(parse_args(args(items)), Err(ArgsError::Usage(_))),
                "{items:?} должно быть отклонено"
            );
        }
    }

    #[test]
    fn invalid_or_empty_header_value_is_an_error() {
        assert!(matches!(
            parse_args(args(&["a.mp4", "--user-agent", "bad\r\nvalue"])),
            Err(ArgsError::Usage(_))
        ));
        assert!(matches!(
            parse_args(args(&["a.mp4", "--referrer", ""])),
            Err(ArgsError::Usage(_))
        ));
    }

    #[test]
    fn help_mentions_every_option() {
        for option in [
            "<file|url>",
            "--volume",
            "--referrer",
            "--referer",
            "--user-agent",
            "--help",
            "-h",
        ] {
            assert!(HELP.contains(option), "в справке нет {option}");
            assert!(USAGE.contains(option), "в usage нет {option}");
        }
    }
}
