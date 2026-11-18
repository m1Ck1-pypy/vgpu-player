//! Версии FFmpeg, с которыми работает запущенный процесс.
//!
//! Номера спрашиваются у самих библиотек (`avutil_version` и соседние), а не
//! берутся из момента сборки: рядом с exe лежат DLL, и в «О программе» важно
//! видеть именно загруженные версии — собранный бинарник и подложенные DLL
//! могут разойтись.

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
