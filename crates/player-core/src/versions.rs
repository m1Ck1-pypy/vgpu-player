//! Версии FFmpeg, с которыми работает запущенный процесс.
//!
//! Номера спрашиваются у самих библиотек (`avutil_version` и соседние), а не
//! берутся из момента сборки: рядом с exe лежат DLL, и в «О программе» важно
//! видеть именно загруженные версии — собранный бинарник и подложенные DLL
//! могут разойтись.
//!
//! Здесь же проверяются возможности загруженного libavformat (протоколы и
//! демуксеры): приложение линкуется с vcpkg-сборкой, и её набор может
//! отличаться от системного `ffmpeg`.

use std::fmt;

/// Версия одной библиотеки FFmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl FfmpegVersion {
    /// Разбирает `LIBAV*_VERSION_INT`: `major << 16 | minor << 8 | patch`.
    pub fn from_bits(bits: u32) -> Self {
        Self {
            major: (bits >> 16) & 0xff,
            minor: (bits >> 8) & 0xff,
            patch: bits & 0xff,
        }
    }
}

impl fmt::Display for FfmpegVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Версии библиотек FFmpeg, на которые опирается плеер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfmpegVersions {
    /// libavutil — общие утилиты.
    pub util: FfmpegVersion,
    /// libavformat — контейнеры.
    pub format: FfmpegVersion,
    /// libavcodec — кодеки.
    pub codec: FfmpegVersion,
}

impl FfmpegVersions {
    /// Спрашивает версии у загруженных библиотек.
    pub fn detect() -> Self {
        Self {
            util: FfmpegVersion::from_bits(ffmpeg::util::version()),
            format: FfmpegVersion::from_bits(ffmpeg::format::version()),
            codec: FfmpegVersion::from_bits(ffmpeg::codec::version()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_version_bits() {
        // 60.8.100 — так FFmpeg 8.x нумерует libavutil.
        let bits = (60 << 16) | (8 << 8) | 100;
        assert_eq!(FfmpegVersion::from_bits(bits).to_string(), "60.8.100");
    }

    #[test]
    fn high_byte_bits_do_not_leak() {
        let parsed = FfmpegVersion::from_bits(u32::MAX);
        assert_eq!(parsed.to_string(), "255.255.255");
    }

    #[test]
    fn runtime_libraries_report_a_version() {
        let versions = FfmpegVersions::detect();
        assert_ne!(versions.util.major, 0, "libavutil не ответила версией");
        assert_ne!(versions.format.major, 0, "libavformat не ответила версией");
        assert_ne!(versions.codec.major, 0, "libavcodec не ответила версией");
    }
}

/// Возможности связанного FFmpeg, от которых зависит этап 5.
///
/// Проверяются именно библиотеки запущенного процесса, а не CLI: приложение
/// линкуется с vcpkg-сборкой, и её протоколы и демуксеры могут отличаться от
/// системного `ffmpeg`.
#[cfg(test)]
mod capabilities {
    use std::ffi::CStr;
    use std::path::PathBuf;
    use std::process::Command;
    use std::ptr;

    use crate::video::test_support::temp_path;

    /// Имена входных протоколов, зарегистрированных в libavformat.
    fn protocol_names() -> Vec<String> {
        let mut names = Vec::new();
        let mut state = ptr::null_mut();

        loop {
            // SAFETY: `state` starts as null and is passed back to
            // `avio_enum_protocols`, which advances it; a null return means
            // the list is exhausted.
            let name = unsafe { ffmpeg::sys::avio_enum_protocols(&mut state, 0) };
            if name.is_null() {
                break;
            }
            // SAFETY: a non-null result points at a NUL-terminated string
            // owned by libavformat and valid until the next call.
            let text = unsafe { CStr::from_ptr(name) };
            names.push(text.to_string_lossy().into_owned());
        }

        names
    }

    #[test]
    fn linked_ffmpeg_has_the_demuxers_hls_needs() {
        // Сеть и TLS живут в `ureq`: в vcpkg-сборке FFmpeg протоколов
        // `https`/`tls` нет, поэтому вариант B отдаёт демуксеру уже готовый
        // поток байтов. Набор печатается, чтобы это было видно в отчёте.
        let protocols = protocol_names();
        eprintln!("протоколы связанного FFmpeg: {protocols:?}");
        eprintln!(
            "hls-демуксер: {}",
            if has_input_format(c"hls") {
                "есть"
            } else {
                "нет"
            }
        );

        // Варианту B нужны демуксеры самих сегментов: MPEG-TS и fMP4.
        assert!(
            has_input_format(c"mpegts"),
            "в связанном FFmpeg нет демуксера mpegts"
        );
        assert!(
            has_input_format(c"mov"),
            "в связанном FFmpeg нет демуксера mov/fMP4"
        );
    }

    /// Есть ли в libavformat демуксер с таким коротким именем.
    fn has_input_format(name: &std::ffi::CStr) -> bool {
        // SAFETY: `av_find_input_format` only reads the passed C string.
        !unsafe { ffmpeg::sys::av_find_input_format(name.as_ptr()) }.is_null()
    }

    #[test]
    fn linked_ffmpeg_opens_a_local_hls_playlist() {
        let Some(directory) = generate_hls() else {
            eprintln!("нет системного ffmpeg с libx264/aac — пропускаем HLS-проверку");
            return;
        };

        let playlist = directory.join("index.m3u8");
        let input = ffmpeg::format::input(&playlist).expect("FFmpeg должен открыть локальный m3u8");
        assert!(
            input.streams().len() >= 2,
            "в плейлисте ожидались звук и видео"
        );
        assert!(
            input.duration() > 0,
            "длительность плейлиста не определилась"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    /// Генерирует VOD-плейлист и TS-сегменты рядом во временном каталоге.
    fn generate_hls() -> Option<PathBuf> {
        let directory = temp_path("hls");
        std::fs::create_dir_all(&directory).ok()?;
        let playlist = directory.join("index.m3u8");
        let segments = directory.join("seg%d.ts");

        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440"])
            .args(["-t", "1", "-c:v", "libx264", "-preset", "ultrafast"])
            .args(["-pix_fmt", "yuv420p", "-g", "5", "-c:a", "aac"])
            .args([
                "-f",
                "hls",
                "-hls_time",
                "0.5",
                "-hls_list_size",
                "0",
                "-hls_playlist_type",
                "vod",
            ])
            .arg("-hls_segment_filename")
            .arg(&segments)
            .arg(&playlist)
            .status();

        match status {
            Ok(status) if status.success() => Some(directory),
            _ => {
                let _ = std::fs::remove_dir_all(&directory);
                None
            }
        }
    }
}
