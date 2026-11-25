//! HLS: разбор плейлистов, выбор варианта и чтение сегментов.
//!
//! Парсер ничего не качает: он получает текст плейлиста и базовый URL, а
//! возвращает структуру с уже разрешёнными (абсолютными) ссылками. Сетевой
//! слой (`HlsSource`, `TrackSource`, `HlsReader`) отдаёт FFmpeg сегменты как
//! один последовательный поток байтов.
//!
//! Неизвестные теги пропускаются: провайдерские расширения не должны ломать
//! воспроизведение.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, PoisonError, Weak};
use std::time::{Duration, Instant};

use tracing::debug;
use ureq::ResponseExt;

use crate::cache::CacheCounters;
use crate::http::{
    HttpConfig, HttpError, HttpFailure, agent, body_error, get, header, status_error, with_retries,
};
use crate::reader::{MediaReader, Track};
use crate::sync::NetworkStats;
use crate::{PlayerError, Source};

/// Предел размера плейлиста.
const MAX_PLAYLIST_BYTES: u64 = 4 * 1024 * 1024;
/// Предел окна скачанных сегментов на дорожку.
const MAX_WINDOW_BYTES: usize = 32 * 1024 * 1024;
/// Плейлист live обновляем не чаще этого.
const MIN_REFRESH: Duration = Duration::from_secs(1);
/// Насколько отступаем от live-edge при старте.
const LIVE_PRIMING: Duration = Duration::from_secs(4);
/// Размер порции чтения тела ответа.
const READ_CHUNK: usize = 32 * 1024;
/// Чтение дольше этого порога считается медленным.
const SLOW_READ: Duration = Duration::from_millis(100);
/// Сколько секунд эфира prefetch держит впереди читателя.
///
/// Окно считается по `#EXTINF`, а не по числу сегментов: у Twitch сегмент
/// 2 с, у Apple — 6 с, и «пять вперёд» давало разный запас по времени.
const PREFETCH_WINDOW: Duration = Duration::from_secs(16);
/// Верхняя граница окна в сегментах: защита от коротких и нулевых `#EXTINF`.
const PREFETCH_MAX_SEGMENTS: u64 = 8;
/// Загрузчиков для muxed-дорожки: несколько соединений быстрее одного.
const PREFETCH_WORKERS_MUXED: usize = 8;
/// На demuxed-дорожку столько, чтобы суммарно было столько же соединений.
const PREFETCH_WORKERS_DEMUXED: usize = 4;
/// Как часто просыпаются prefetch-воркеры без работы.
const PREFETCH_IDLE: Duration = Duration::from_millis(100);

/// Разобранный плейлист: master со списком вариантов или media с сегментами.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Master-плейлист: варианты качества, из которых выбирается один.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MasterPlaylist {
    pub(crate) variants: Vec<Variant>,
    /// `#EXT-X-MEDIA` с `TYPE=AUDIO`; остальные типы не сохраняются.
    pub(crate) audio_renditions: Vec<AudioRendition>,
}

/// Один вариант master-плейлиста.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Variant {
    /// Абсолютная ссылка на media-плейлист варианта.
    pub(crate) uri: String,
    /// `BANDWIDTH`, бит/с.
    pub(crate) bandwidth: Option<u64>,
    /// `RESOLUTION` в пикселях.
    pub(crate) resolution: Option<(u32, u32)>,
    /// `CODECS` как есть (может содержать запятые внутри кавычек).
    pub(crate) codecs: Option<String>,
    /// `AUDIO` — идентификатор аудиогруппы.
    pub(crate) audio_group: Option<String>,
}

/// Аудиодорожка из `#EXT-X-MEDIA:TYPE=AUDIO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AudioRendition {
    /// `GROUP-ID`, на который ссылается `AUDIO=` варианта.
    pub(crate) group_id: Option<String>,
    /// `NAME` для интерфейса.
    pub(crate) name: Option<String>,
    /// `LANGUAGE` как есть.
    pub(crate) language: Option<String>,
    /// `DEFAULT=YES`: предпочитается при выборе.
    pub(crate) default: bool,
    /// `AUTOSELECT=YES`.
    pub(crate) autoselect: bool,
    /// `CHANNELS`: первое число из значения (`2`, `6/JOC`).
    pub(crate) channels: Option<u32>,
    /// Абсолютная ссылка на media-плейлист дорожки; `None` — muxed-аудио.
    pub(crate) uri: Option<String>,
}

/// Media-плейлист: сегменты и общие параметры потока.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MediaPlaylist {
    /// `#EXT-X-TARGETDURATION`.
    pub(crate) target_duration: Option<Duration>,
    /// `#EXT-X-MEDIA-SEQUENCE`, по умолчанию 0.
    pub(crate) media_sequence: u64,
    /// Есть ли `#EXT-X-ENDLIST` (VOD); иначе плейлист живой.
    pub(crate) end_list: bool,
    /// Сегменты в порядке воспроизведения.
    pub(crate) segments: Vec<Segment>,
    /// Сумма `#EXTINF` всех сегментов.
    pub(crate) total_duration: Duration,
}

/// Один сегмент media-плейлиста.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Segment {
    /// Абсолютная ссылка на сегмент.
    pub(crate) uri: String,
    /// Длительность из `#EXTINF`.
    pub(crate) duration: Duration,
    /// `#EXT-X-BYTERANGE` для этого сегмента.
    pub(crate) byte_range: Option<ByteRange>,
    /// `#EXT-X-MAP`, действующий на этот сегмент (fMP4).
    pub(crate) map: Option<Map>,
    /// Перед сегментом был `#EXT-X-DISCONTINUITY`.
    pub(crate) discontinuity: bool,
    /// Действующий `#EXT-X-KEY`; `None` — сегмент не зашифрован.
    pub(crate) key: Option<Key>,
    /// `#EXT-X-PROGRAM-DATE-TIME` перед сегментом, epoch-секунды.
    pub(crate) program_date_time: Option<i64>,
}

/// Диапазон байтов внутри ресурса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ByteRange {
    pub(crate) length: u64,
    pub(crate) offset: u64,
}

/// Инициализирующий сегмент fMP4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Map {
    /// Абсолютная ссылка на init-сегмент.
    pub(crate) uri: String,
    pub(crate) byte_range: Option<ByteRange>,
}

/// Параметры шифрования сегментов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    /// `METHOD` как есть: `AES-128`, `SAMPLE-AES` и т. п.
    pub(crate) method: String,
    /// Абсолютная ссылка на ключ.
    pub(crate) uri: Option<String>,
    /// `IV` в виде 16 байт.
    pub(crate) iv: Option<[u8; 16]>,
}

/// Почему плейлист не удалось разобрать.
///
/// Текст сообщения — часть будущей ошибки `PlayerError::Hls`, поэтому причина
/// формулируется без префикса «HLS».
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum HlsError {
    /// Плейлист пуст.
    #[error("пустой плейлист")]
    Empty,
    /// Первая значимая строка — не `#EXTM3U`.
    #[error("это не HLS-плейлист: нет #EXTM3U")]
    MissingHeader,
    /// Строка не соответствует формату.
    #[error("строка {line}: {reason}")]
    Syntax {
        /// Номер строки в исходном тексте, начиная с 1.
        line: usize,
        /// Что именно не так.
        reason: &'static str,
    },
}

/// Разбирает плейлист; `base` — URL, относительно которого разрешаются ссылки.
pub(crate) fn parse(text: &str, base: &str) -> Result<Playlist, HlsError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.strip_suffix('\r').unwrap_or(line)))
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();

    let Some((_, first)) = lines.first() else {
        return Err(HlsError::Empty);
    };
    if first.trim() != "#EXTM3U" {
        return Err(HlsError::MissingHeader);
    }

    let master_line = lines
        .iter()
        .find(|(_, line)| line.trim_start().starts_with("#EXT-X-STREAM-INF:"))
        .map(|(number, _)| *number);
    let media_line = lines
        .iter()
        .find(|(_, line)| line.trim_start().starts_with("#EXTINF:"))
        .map(|(number, _)| *number);

    match (master_line, media_line) {
        (Some(line), Some(_)) => Err(HlsError::Syntax {
            line,
            reason: "смешаны master- и media-плейлист",
        }),
        (Some(_), None) => parse_master(&lines, base),
        (None, Some(_)) => parse_media(&lines, base),
        (None, None) => match lines
            .iter()
            .find(|(_, line)| !line.trim_start().starts_with('#'))
        {
            Some((number, _)) => Err(syntax(*number, "URI без #EXT-X-STREAM-INF или #EXTINF")),
            None => Err(syntax(1, "нет ни вариантов, ни сегментов")),
        },
    }
}

fn parse_master(lines: &[(usize, &str)], base: &str) -> Result<Playlist, HlsError> {
    let mut variants = Vec::new();
    let mut audio_renditions = Vec::new();
    let mut pending: Option<(usize, Variant)> = None;
    let mut last_line = 1;

    for (number, line) in lines {
        last_line = *number;
        let line = line.trim();

        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if let Some((previous, _)) = pending.take() {
                return Err(syntax(previous, "у варианта нет URI"));
            }
            let attributes =
                Attributes::parse(attributes).map_err(|reason| syntax(*number, reason))?;
            pending = Some((
                *number,
                Variant {
                    uri: String::new(),
                    bandwidth: attributes
                        .bytes("BANDWIDTH")
                        .map_err(|reason| syntax(*number, reason))?,
                    resolution: attributes
                        .resolution()
                        .map_err(|reason| syntax(*number, reason))?,
                    codecs: attributes.get("CODECS").map(str::to_owned),
                    audio_group: attributes.get("AUDIO").map(str::to_owned),
                },
            ));
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            if let Some(rendition) = parse_audio_rendition(value, base, *number)? {
                audio_renditions.push(rendition);
            }
        } else if line.starts_with('#') {
            // Остальные теги и комментарии к варианту не относятся.
        } else {
            let Some((_, mut variant)) = pending.take() else {
                return Err(syntax(*number, "URI без #EXT-X-STREAM-INF"));
            };
            variant.uri = resolve_uri(base, line);
            variants.push(variant);
        }
    }

    if let Some((number, _)) = pending {
        return Err(syntax(number, "у #EXT-X-STREAM-INF нет URI"));
    }
    if variants.is_empty() {
        return Err(syntax(last_line, "в master-плейлисте нет вариантов"));
    }

    Ok(Playlist::Master(MasterPlaylist {
        variants,
        audio_renditions,
    }))
}

/// Разбирает `#EXT-X-MEDIA`, сохраняя только аудиодорожки.
///
/// Субтитры, закрытые субтитры и I-frame-плейлисты пропускаются: в первой
/// версии плеер их не использует.
fn parse_audio_rendition(
    value: &str,
    base: &str,
    line: usize,
) -> Result<Option<AudioRendition>, HlsError> {
    let attributes = Attributes::parse(value).map_err(|reason| syntax(line, reason))?;
    let Some(media_type) = attributes.get("TYPE") else {
        return Ok(None);
    };
    if !media_type.eq_ignore_ascii_case("AUDIO") {
        return Ok(None);
    }

    let channels = match attributes.get("CHANNELS") {
        Some(value) => value
            .split(['/', ' '])
            .next()
            .and_then(|text| text.parse::<u32>().ok()),
        None => None,
    };

    Ok(Some(AudioRendition {
        group_id: attributes.get("GROUP-ID").map(str::to_owned),
        name: attributes.get("NAME").map(str::to_owned),
        language: attributes.get("LANGUAGE").map(str::to_owned),
        default: attributes.is_yes("DEFAULT"),
        autoselect: attributes.is_yes("AUTOSELECT"),
        channels,
        uri: attributes.get("URI").map(|uri| resolve_uri(base, uri)),
    }))
}

/// Выбранный вариант и, если поток demuxed, его аудиодорожка.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SelectedPlaylist {
    pub(crate) variant: Variant,
    pub(crate) audio: Option<AudioRendition>,
}

/// Выбирает один вариант master-плейлиста (без ABR).
///
/// Правила из SPEC §2.7: варианты одинакового `URI` считаются одним качеством
/// (Apple повторяет видео-URI для разных аудиогрупп), предпочитается AAC,
/// затем — максимальное разрешение не выше 1080p и битрейт.
pub(crate) fn select_variant(master: &MasterPlaylist) -> Result<SelectedPlaylist, String> {
    let deduplicated = deduplicate(&master.variants);

    let capped: Vec<&Variant> = deduplicated
        .iter()
        .filter(|variant| variant.resolution.is_none_or(|(_, height)| height <= 1080))
        .collect();
    let variant = (if capped.is_empty() {
        // Всё выше 1080p: берём самое лёгкое, чтобы не тянуть 4K без нужды.
        deduplicated
            .iter()
            .min_by_key(|variant| variant_score(variant))
            .cloned()
    } else {
        capped
            .into_iter()
            .max_by_key(|variant| variant_score(variant))
            .cloned()
    })
    .ok_or_else(|| "в master-плейлисте нет вариантов".to_owned())?;

    let audio = select_audio(master, &variant)?;

    Ok(SelectedPlaylist { variant, audio })
}

/// Оставляет по одному варианту на `URI`, предпочитая AAC и лучший битрейт.
fn deduplicate(variants: &[Variant]) -> Vec<Variant> {
    let mut result: Vec<Variant> = Vec::new();

    for candidate in variants {
        match result
            .iter_mut()
            .find(|existing| existing.uri == candidate.uri)
        {
            Some(existing) => {
                if better_variant(candidate, existing) {
                    *existing = candidate.clone();
                }
            }
            None => result.push(candidate.clone()),
        }
    }

    result
}

/// Сравнение вариантов одного `URI`: сначала AAC, потом разрешение/битрейт.
fn better_variant(candidate: &Variant, existing: &Variant) -> bool {
    match is_aac(candidate).cmp(&is_aac(existing)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => variant_score(candidate) > variant_score(existing),
    }
}

fn variant_score(variant: &Variant) -> (u32, u32, u64) {
    let (width, height) = variant.resolution.unwrap_or((0, 0));
    (height, width, variant.bandwidth.unwrap_or(0))
}

fn is_aac(variant: &Variant) -> bool {
    variant
        .codecs
        .as_deref()
        .is_some_and(|codecs| codecs.to_ascii_lowercase().contains("mp4a"))
}

/// Выбирает аудиодорожку для варианта.
///
/// `Ok(None)` — аудио muxed внутри варианта (группа не указана или в ней нет
/// `URI`). `Err` — вариант ссылается на несуществующую группу.
fn select_audio(
    master: &MasterPlaylist,
    variant: &Variant,
) -> Result<Option<AudioRendition>, String> {
    let Some(group) = variant.audio_group.as_deref() else {
        return Ok(None);
    };

    let group_renditions: Vec<&AudioRendition> = master
        .audio_renditions
        .iter()
        .filter(|rendition| rendition.group_id.as_deref() == Some(group))
        .collect();
    if group_renditions.is_empty() {
        return Err(format!("в master-плейлисте нет аудиогруппы {group}"));
    }

    let with_uri: Vec<&AudioRendition> = group_renditions
        .into_iter()
        .filter(|rendition| rendition.uri.is_some())
        .collect();
    let Some((first, rest)) = with_uri.split_first() else {
        return Ok(None);
    };

    // Порядок предпочтений: DEFAULT=YES, затем 2 канала, затем AUTOSELECT.
    let score = |rendition: &AudioRendition| {
        (
            u8::from(rendition.default),
            u8::from(rendition.channels == Some(2)),
            u8::from(rendition.autoselect),
        )
    };
    let mut chosen: &AudioRendition = first;
    for rendition in rest {
        if score(rendition) > score(chosen) {
            chosen = rendition;
        }
    }

    Ok(Some((*chosen).clone()))
}

fn parse_media(lines: &[(usize, &str)], base: &str) -> Result<Playlist, HlsError> {
    let mut target_duration = None;
    let mut media_sequence = 0;
    let mut end_list = false;
    let mut segments = Vec::new();
    let mut total_duration = Duration::ZERO;

    let mut pending_duration: Option<(usize, Duration)> = None;
    let mut pending_range: Option<ByteRange> = None;
    let mut pending_date_time: Option<i64> = None;
    let mut pending_discontinuity = false;
    let mut map: Option<Map> = None;
    let mut key: Option<Key> = None;
    let mut previous_range_end: Option<u64> = None;
    let mut last_line = 1;

    for (number, line) in lines {
        last_line = *number;
        let line = line.trim();

        if let Some(value) = line.strip_prefix("#EXTINF:") {
            if pending_duration.is_some() {
                return Err(syntax(*number, "два #EXTINF подряд"));
            }
            pending_duration = Some((*number, parse_duration(value, *number)?));
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            let seconds = value
                .trim()
                .parse::<u64>()
                .map_err(|_| syntax(*number, "#EXT-X-TARGETDURATION не число"))?;
            target_duration = Some(Duration::from_secs(seconds));
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value
                .trim()
                .parse::<u64>()
                .map_err(|_| syntax(*number, "#EXT-X-MEDIA-SEQUENCE не число"))?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_byte_range(value, previous_range_end, *number)?);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            map = Some(parse_map(value, base, *number)?);
        } else if let Some(value) = line.strip_prefix("#EXT-X-KEY:") {
            key = parse_key(value, base, *number)?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pending_date_time = Some(parse_program_date_time(value, *number)?);
        } else if line == "#EXT-X-DISCONTINUITY" {
            pending_discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            end_list = true;
        } else if line.starts_with('#') {
            // Неизвестные теги и комментарии не мешают воспроизведению.
        } else {
            let Some((duration_line, duration)) = pending_duration.take() else {
                return Err(syntax(*number, "URI без #EXTINF"));
            };
            let byte_range = pending_range.take();
            segments.push(Segment {
                uri: resolve_uri(base, line),
                duration,
                byte_range,
                map: map.clone(),
                discontinuity: pending_discontinuity,
                key: key.clone(),
                program_date_time: pending_date_time.take(),
            });
            total_duration = total_duration
                .checked_add(duration)
                .ok_or_else(|| syntax(duration_line, "суммарная длительность вне диапазона"))?;
            previous_range_end = byte_range.map(|range| range.offset + range.length);
            pending_discontinuity = false;
        }
    }

    if let Some((number, _)) = pending_duration {
        return Err(syntax(number, "у #EXTINF нет URI"));
    }
    if segments.is_empty() {
        return Err(syntax(last_line, "в media-плейлисте нет сегментов"));
    }

    Ok(Playlist::Media(MediaPlaylist {
        target_duration,
        media_sequence,
        end_list,
        segments,
        total_duration,
    }))
}

fn parse_duration(value: &str, line: usize) -> Result<Duration, HlsError> {
    let seconds_text = value.split(',').next().unwrap_or_default().trim();
    let seconds: f64 = seconds_text
        .parse()
        .map_err(|_| syntax(line, "#EXTINF: длительность не число"))?;
    // Закрывает NaN, бесконечность, отрицательные значения и переполнение
    // `Duration` — без паники на испорченном плейлисте.
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| syntax(line, "#EXTINF: длительность вне допустимого диапазона"))
}

fn parse_map(value: &str, base: &str, line: usize) -> Result<Map, HlsError> {
    let attributes = Attributes::parse(value).map_err(|reason| syntax(line, reason))?;
    let uri = attributes
        .get("URI")
        .ok_or_else(|| syntax(line, "у #EXT-X-MAP нет URI"))?;
    let byte_range = match attributes.get("BYTERANGE") {
        Some(range) => Some(parse_byte_range(range, None, line)?),
        None => None,
    };

    Ok(Map {
        uri: resolve_uri(base, uri),
        byte_range,
    })
}

fn parse_key(value: &str, base: &str, line: usize) -> Result<Option<Key>, HlsError> {
    let attributes = Attributes::parse(value).map_err(|reason| syntax(line, reason))?;
    let method = attributes
        .get("METHOD")
        .ok_or_else(|| syntax(line, "у #EXT-X-KEY нет METHOD"))?;
    if method.eq_ignore_ascii_case("NONE") {
        return Ok(None);
    }

    let uri = attributes
        .get("URI")
        .ok_or_else(|| syntax(line, "у #EXT-X-KEY нет URI"))?;
    let iv = attributes
        .get("IV")
        .map(|text| parse_iv(text, line))
        .transpose()?;

    Ok(Some(Key {
        method: method.to_owned(),
        uri: Some(resolve_uri(base, uri)),
        iv,
    }))
}

fn parse_iv(text: &str, line: usize) -> Result<[u8; 16], HlsError> {
    let text = text.trim();
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .ok_or_else(|| syntax(line, "IV не в hex-формате"))?;
    if hex.is_empty() || hex.len() > 32 {
        return Err(syntax(line, "IV не в hex-формате"));
    }

    let padded = format!("{hex:0>32}");
    if padded.len() != 32 {
        return Err(syntax(line, "IV не в hex-формате"));
    }

    let mut bytes = [0_u8; 16];
    for (index, pair) in padded.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = hex_value(pair[0]).ok_or_else(|| syntax(line, "IV не в hex-формате"))?;
        let low = hex_value(pair[1]).ok_or_else(|| syntax(line, "IV не в hex-формате"))?;
        bytes[index] = high << 4 | low;
    }

    Ok(bytes)
}

fn parse_byte_range(
    value: &str,
    previous_end: Option<u64>,
    line: usize,
) -> Result<ByteRange, HlsError> {
    let value = value.trim();
    let (length, offset) = match value.split_once('@') {
        Some((length, offset)) => (length.trim(), Some(offset.trim())),
        None => (value, None),
    };

    let length: u64 = length
        .parse()
        .map_err(|_| syntax(line, "#EXT-X-BYTERANGE: длина не число"))?;
    if length == 0 {
        return Err(syntax(line, "#EXT-X-BYTERANGE: нулевая длина"));
    }
    let offset = match offset {
        Some(offset) => offset
            .parse()
            .map_err(|_| syntax(line, "#EXT-X-BYTERANGE: смещение не число"))?,
        None => previous_end.unwrap_or(0),
    };
    offset
        .checked_add(length)
        .ok_or_else(|| syntax(line, "#EXT-X-BYTERANGE выходит за u64"))?;

    Ok(ByteRange { length, offset })
}

fn syntax(line: usize, reason: &'static str) -> HlsError {
    HlsError::Syntax { line, reason }
}

/// Разбирает `#EXT-X-PROGRAM-DATE-TIME` в epoch-секунды.
///
/// Поддерживается ISO-8601 без внешних зависимостей:
/// `YYYY-MM-DDThh:mm:ss[.дробь](Z|+hh:mm|-hh:mm)`. Дробная часть
/// отбрасывается: для выравнивания дорожек хватает секунд.
fn parse_program_date_time(value: &str, line: usize) -> Result<i64, HlsError> {
    let invalid = || syntax(line, "#EXT-X-PROGRAM-DATE-TIME не в формате ISO-8601");
    let text = value.trim();
    let (date_part, rest) = text.split_once(['T', 't']).ok_or_else(invalid)?;

    let mut date_numbers = date_part.split('-');
    let year = date_numbers.next().ok_or_else(invalid)?;
    let month = date_numbers.next().ok_or_else(invalid)?;
    let day = date_numbers.next().ok_or_else(invalid)?;
    if date_numbers.next().is_some() {
        return Err(invalid());
    }

    let year: i64 = year.parse().map_err(|_| invalid())?;
    let month: u32 = month.parse().map_err(|_| invalid())?;
    let day: u32 = day.parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid());
    }

    let (time_part, offset) = split_date_offset(rest).ok_or_else(invalid)?;
    let mut time_numbers = time_part.split(':');
    let hour_text = time_numbers.next().ok_or_else(invalid)?;
    let minute_text = time_numbers.next().ok_or_else(invalid)?;
    let second_text = time_numbers.next().ok_or_else(invalid)?;
    if time_numbers.next().is_some() {
        return Err(invalid());
    }

    let hour: i64 = hour_text.parse().map_err(|_| invalid())?;
    let minute: i64 = minute_text.parse().map_err(|_| invalid())?;
    let (second_text, fraction) = second_text
        .split_once('.')
        .map_or((second_text, ""), |(seconds, fraction)| (seconds, fraction));
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let second: i64 = second_text.parse().map_err(|_| invalid())?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return Err(invalid());
    }

    let day_seconds = days_from_civil(year, month, day)
        .checked_mul(86_400)
        .ok_or_else(invalid)?;
    let time_seconds = hour * 3_600 + minute * 60 + second;

    day_seconds
        .checked_add(time_seconds)
        .and_then(|seconds| seconds.checked_sub(offset))
        .ok_or_else(invalid)
}

/// Отделяет в строке времени `±hh:mm`/`±hhmm` или `Z`; возвращает смещение в
/// секундах.
fn split_date_offset(text: &str) -> Option<(&str, i64)> {
    if let Some(time) = text.strip_suffix(['Z', 'z']) {
        return Some((time, 0));
    }

    let index = text.rfind(['+', '-'])?;
    let (time, offset) = text.split_at(index);
    let sign: i64 = if offset.starts_with('-') { -1 } else { 1 };
    let digits = &offset[1..];
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if digits.len() == 4 => digits.split_at(2),
        None => return None,
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }

    Some((time, sign * (hours * 3_600 + minutes * 60)))
}

/// Дни от 1970-01-01 до гражданской даты (алгоритм Говарда Хиннанта).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = i64::from(month) + 9;
    let day_of_year = (153 * (month_index % 12) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// Атрибуты тега: `KEY=VALUE,KEY="VALUE,WITH,COMMAS"`.
struct Attributes {
    pairs: Vec<(String, String)>,
}

impl Attributes {
    fn parse(text: &str) -> Result<Self, &'static str> {
        let bytes = text.as_bytes();
        let mut pairs = Vec::new();
        let mut index = 0;

        while index < bytes.len() {
            while index < bytes.len() && matches!(bytes[index], b' ' | b'\t' | b',') {
                index += 1;
            }
            if index >= bytes.len() {
                break;
            }

            let name_start = index;
            while index < bytes.len() && bytes[index] != b'=' {
                if bytes[index] == b',' {
                    return Err("атрибут без значения");
                }
                index += 1;
            }
            if index >= bytes.len() {
                return Err("атрибут без значения");
            }
            let name = text[name_start..index].trim();
            if name.is_empty() {
                return Err("пустое имя атрибута");
            }
            index += 1;

            let value = if index < bytes.len() && bytes[index] == b'"' {
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != b'"' {
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err("незакрытая кавычка");
                }
                let value = &text[value_start..index];
                index += 1;
                value
            } else {
                let value_start = index;
                while index < bytes.len() && bytes[index] != b',' {
                    index += 1;
                }
                text[value_start..index].trim()
            };

            pairs.push((name.to_owned(), value.to_owned()));
        }

        Ok(Self { pairs })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn bytes(&self, name: &str) -> Result<Option<u64>, &'static str> {
        match self.get(name) {
            Some(value) => value
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|_| "атрибут не число"),
            None => Ok(None),
        }
    }

    /// `YES` без учёта регистра; всё остальное — `false`.
    fn is_yes(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|value| value.eq_ignore_ascii_case("YES"))
    }

    fn resolution(&self) -> Result<Option<(u32, u32)>, &'static str> {
        let Some(value) = self.get("RESOLUTION") else {
            return Ok(None);
        };
        let (width, height) = value
            .split_once(['x', 'X'])
            .ok_or("RESOLUTION не в формате ШxВ")?;
        let width = width
            .trim()
            .parse::<u32>()
            .map_err(|_| "RESOLUTION не в формате ШxВ")?;
        let height = height
            .trim()
            .parse::<u32>()
            .map_err(|_| "RESOLUTION не в формате ШxВ")?;
        Ok(Some((width, height)))
    }
}

/// Разрешает ссылку из плейлиста относительно его адреса.
///
/// Поддерживаются абсолютные URL, пути от корня (`/a/b`), ссылки от протокола
/// (`//host/a`), запросы (`?token=…`) и `..`/`.` в относительных путях.
fn resolve_uri(base: &str, reference: &str) -> String {
    let reference = reference.trim();

    if has_scheme(reference) {
        return reference.to_owned();
    }

    let (base_path, _) = split_suffix(base);

    if let Some(rest) = reference.strip_prefix("//") {
        return match scheme_of(base) {
            Some(scheme) => format!("{scheme}//{rest}"),
            None => format!("//{rest}"),
        };
    }

    if reference.starts_with('?') || reference.starts_with('#') {
        return format!("{base_path}{reference}");
    }

    if reference.starts_with('/') {
        return match origin_of(base) {
            Some(origin) => normalize_dot_segments(&format!("{origin}{reference}")),
            None => reference.to_owned(),
        };
    }

    let origin = origin_of(base_path).unwrap_or("");
    let directory = match base_path.rfind('/') {
        Some(index) if index >= origin.len() => &base_path[..=index],
        // База без пути (`https://host`): сегмент лежит в корне.
        _ if origin.is_empty() => return normalize_dot_segments(reference),
        _ => return normalize_dot_segments(&format!("{origin}/{reference}")),
    };
    normalize_dot_segments(&format!("{directory}{reference}"))
}

fn has_scheme(text: &str) -> bool {
    let Some((scheme, _)) = text.split_once("://") else {
        return false;
    };
    !scheme.is_empty()
        && scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

fn scheme_of(uri: &str) -> Option<&str> {
    let scheme_end = uri.find("://")?;
    Some(&uri[..=scheme_end])
}

fn origin_of(uri: &str) -> Option<&str> {
    let after_scheme = uri.find("://")? + 3;
    let end = uri[after_scheme..]
        .find(['/', '?', '#'])
        .map(|index| after_scheme + index);
    Some(match end {
        Some(end) => &uri[..end],
        None => uri,
    })
}

fn split_suffix(text: &str) -> (&str, &str) {
    match text.find(['?', '#']) {
        Some(index) => (&text[..index], &text[index..]),
        None => (text, ""),
    }
}

fn normalize_dot_segments(uri: &str) -> String {
    let (path_part, suffix) = split_suffix(uri);
    let (origin, path) = match path_start(path_part) {
        Some(index) => (&path_part[..index], &path_part[index..]),
        None => ("", path_part),
    };

    format!("{origin}{}{suffix}", normalize_path(path))
}

fn path_start(uri: &str) -> Option<usize> {
    let after_scheme = uri.find("://")? + 3;
    uri[after_scheme..]
        .find('/')
        .map(|index| after_scheme + index)
}

fn normalize_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut normalized = String::new();
    if absolute {
        normalized.push('/');
    }
    normalized.push_str(&parts.join("/"));
    if path.ends_with('/') && !normalized.ends_with('/') {
        normalized.push('/');
    }

    normalized
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Общий сетевой контекст HLS-источника: один агент, таймауты, отмена и счётчики.
struct HlsContext {
    agent: ureq::Agent,
    config: HttpConfig,
    cancel: Arc<AtomicBool>,
    counters: Arc<CacheCounters>,
}

/// Открытый HLS-источник: плейлисты дорожек, счётчики и признак live.
///
/// Живёт в `Media`; читатели получают сегменты через [`HlsSource::reader`].
pub(crate) struct HlsSource {
    source: Source,
    video: Arc<TrackSource>,
    audio: Option<Arc<TrackSource>>,
    live: bool,
    duration: Option<Duration>,
    counters: Arc<CacheCounters>,
}

impl HlsSource {
    /// Открывает master или media-плейлист и выбирает один вариант.
    pub(crate) fn open(
        source: &Source,
        config: HttpConfig,
        cancel: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, PlayerError> {
        let Source::Url(url) = source else {
            return Err(PlayerError::UnsupportedHls {
                target: source.clone(),
            });
        };

        let context = Arc::new(HlsContext {
            agent: agent(&config),
            config,
            cancel,
            counters: Arc::new(CacheCounters::default()),
        });

        let fetch = |url: &str| {
            fetch_playlist_text(
                &context.agent,
                &context.config,
                &context.counters,
                &context.cancel,
                url,
            )
            .map_err(|error| open_error(error, url))
        };

        let (text, playlist_url) = fetch(url)?;
        let playlist = parse(&text, &playlist_url).map_err(|error| PlayerError::Hls {
            target: Source::Url(url.clone()),
            reason: error.to_string(),
        })?;

        let (video_playlist, video_url, audio_playlist) = match playlist {
            Playlist::Media(media) => (media, playlist_url, None),
            Playlist::Master(master) => {
                let selected = select_variant(&master).map_err(|reason| PlayerError::Hls {
                    target: source.clone(),
                    reason,
                })?;

                let (video_text, video_url) = fetch(&selected.variant.uri)?;
                let video = parse_media_playlist(&video_text, &video_url, &selected.variant.uri)?;

                let audio = match &selected.audio {
                    Some(rendition) => {
                        let audio_uri =
                            rendition.uri.as_deref().ok_or_else(|| PlayerError::Hls {
                                target: source.clone(),
                                reason: "у выбранной аудиодорожки нет URI".to_owned(),
                            })?;
                        let (audio_text, audio_url) = fetch(audio_uri)?;
                        let audio = parse_media_playlist(&audio_text, &audio_url, audio_uri)?;
                        Some((audio, audio_url))
                    }
                    None => None,
                };

                (video, video_url, audio)
            }
        };

        check_unencrypted(&video_playlist, source)?;
        if let Some((audio, _)) = &audio_playlist {
            check_unencrypted(audio, source)?;
        }

        let video_live = !video_playlist.end_list;
        let video_duration = video_playlist.total_duration;
        let demuxed = audio_playlist.is_some();
        let workers = if demuxed {
            PREFETCH_WORKERS_DEMUXED
        } else {
            PREFETCH_WORKERS_MUXED
        };
        debug!(
            workers,
            demuxed,
            window_ms = PREFETCH_WINDOW.as_millis(),
            max_segments = PREFETCH_MAX_SEGMENTS,
            "HLS: prefetch настроен"
        );

        let video = Arc::new(TrackSource::new(
            source.clone(),
            video_url,
            video_playlist,
            Track::Video,
            Arc::clone(&context),
            workers,
        ));
        let audio = audio_playlist.map(|(playlist, url)| {
            Arc::new(TrackSource::new(
                source.clone(),
                url,
                playlist,
                Track::Audio,
                Arc::clone(&context),
                workers,
            ))
        });

        let live = video_live || audio.as_ref().is_some_and(|track| track.is_live());
        if live {
            align_live_tracks(&video, audio.as_ref());
        }

        Ok(Arc::new(Self {
            source: source.clone(),
            video,
            audio,
            live,
            duration: (!live).then_some(video_duration),
            counters: Arc::clone(&context.counters),
        }))
    }

    /// Читатель дорожки. Для muxed обе дорожки читают один и тот же плейлист.
    pub(crate) fn reader(&self, track: Track) -> Option<HlsReader> {
        let source = match track {
            Track::Video => &self.video,
            Track::Audio => self.audio.as_ref().unwrap_or(&self.video),
        };
        Some(HlsReader::new(Arc::clone(source)))
    }

    /// Идёт ли поток в реальном времени.
    pub(crate) fn is_live(&self) -> bool {
        self.live
    }

    /// Длительность VOD; у live её нет.
    pub(crate) fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Звук вынесен в отдельный плейлист.
    pub(crate) fn is_demuxed(&self) -> bool {
        self.audio.is_some()
    }

    /// Ждёт ли хотя бы одна дорожка сеть.
    pub(crate) fn is_buffering(&self) -> bool {
        self.video.is_buffering()
            || self
                .audio
                .as_ref()
                .is_some_and(|track| track.is_buffering())
    }

    /// Перематывает VOD к целевой позиции: новые читатели начнут с сегмента,
    /// содержащего `position`.
    ///
    /// Сами декодеры переоткрывает `Media`: виртуальный поток HLS не имеет
    /// байтового размера, поэтому байтовый `seek` здесь неприменим.
    pub(crate) fn seek_time(&self, position: Duration) -> Result<(), PlayerError> {
        if self.live {
            return Err(PlayerError::NotSeekable {
                target: self.source.clone(),
            });
        }

        let video = self
            .video
            .sequence_at(position)
            .ok_or_else(|| PlayerError::NotSeekable {
                target: self.source.clone(),
            })?;
        self.video.set_start_sequence(video);

        if let Some(audio) = &self.audio {
            let sequence = audio
                .sequence_at(position)
                .ok_or_else(|| PlayerError::NotSeekable {
                    target: self.source.clone(),
                })?;
            audio.set_start_sequence(sequence);
        }

        Ok(())
    }

    /// Счётчики сети по обеим дорожкам.
    pub(crate) fn network_stats(&self) -> NetworkStats {
        self.counters.snapshot()
    }

    /// Сдвиг PTS видео относительно звука для demuxed live, в миллисекундах.
    ///
    /// Выравнивание старта по `PROGRAM-DATE-TIME` оставляет расхождение до
    /// длины сегмента; декодер применяет этот сдвиг к PTS видео. Для muxed,
    /// VOD и плейлистов без времени сдвига нет.
    pub(crate) fn video_pts_shift_ms(&self) -> Option<i64> {
        if !self.live {
            return None;
        }
        let audio = self.audio.as_ref()?;
        let video_date = self.video.start_date()?;
        let audio_date = audio.start_date()?;

        Some(video_date.saturating_sub(audio_date).saturating_mul(1_000))
    }
}

/// Проверяет, что сегменты не зашифрованы: AES-128 — вне первой версии.
fn check_unencrypted(playlist: &MediaPlaylist, source: &Source) -> Result<(), PlayerError> {
    if playlist
        .segments
        .iter()
        .any(|segment| segment.key.is_some())
    {
        return Err(PlayerError::UnsupportedHls {
            target: source.clone(),
        });
    }
    Ok(())
}

fn parse_media_playlist(text: &str, base: &str, url: &str) -> Result<MediaPlaylist, PlayerError> {
    match parse(text, base) {
        Ok(Playlist::Media(media)) => Ok(media),
        Ok(Playlist::Master(_)) => Err(PlayerError::Hls {
            target: Source::Url(url.to_owned()),
            reason: "вместо media-плейлиста пришёл master".to_owned(),
        }),
        Err(error) => Err(PlayerError::Hls {
            target: Source::Url(url.to_owned()),
            reason: error.to_string(),
        }),
    }
}

/// Переводит сетевую неудачу открытия в ошибку ядра.
fn open_error(error: HttpError, url: &str) -> PlayerError {
    let target = Source::Url(url.to_owned());

    match error.failure {
        HttpFailure::Cancelled => PlayerError::Cancelled,
        HttpFailure::Status(status) => PlayerError::HttpStatus { target, status },
        HttpFailure::Network(message) => PlayerError::Network { target, message },
        HttpFailure::NotMedia => PlayerError::NotMedia { target },
        HttpFailure::NotSeekable => PlayerError::NotSeekable { target },
        HttpFailure::Empty => PlayerError::Hls {
            target,
            reason: "пустой плейлист".to_owned(),
        },
        HttpFailure::Hls { url, reason } => PlayerError::Hls {
            target: Source::Url(url),
            reason,
        },
    }
}

/// Загружает текст плейлиста с повторами и отменой.
///
/// Возвращает текст и финальный URL после редиректов: он служит базой для
/// относительных ссылок внутри плейлиста.
fn fetch_playlist_text(
    agent: &ureq::Agent,
    config: &HttpConfig,
    counters: &Arc<CacheCounters>,
    cancel: &Arc<AtomicBool>,
    url: &str,
) -> Result<(String, String), HttpError> {
    let stop = || cancel.load(Ordering::Relaxed);

    let mut attempt = || {
        if stop() {
            return Err(HttpError::cancelled());
        }

        counters.add_request();
        let started = Instant::now();
        let response = get(agent, config, url, &[])?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(status_error(status));
        }
        if let Some(content_type) = header(&response, "content-type") {
            let content_type = content_type.to_ascii_lowercase();
            if content_type.starts_with("text/html")
                || content_type.starts_with("application/xhtml")
            {
                return Err(HttpError::fatal(HttpFailure::NotMedia));
            }
        }

        let final_url = response.get_uri().to_string();
        let body = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        let bytes = read_limit(body, MAX_PLAYLIST_BYTES, &stop, counters)?;
        if bytes.len() as u64 > MAX_PLAYLIST_BYTES {
            return Err(HttpError::fatal(HttpFailure::Hls {
                url: url.to_owned(),
                reason: "плейлист больше 4 МиБ".to_owned(),
            }));
        }

        let elapsed = started.elapsed();
        if elapsed > SLOW_READ {
            counters.add_slow_read(elapsed);
        }
        Ok((bytes, final_url))
    };

    let (bytes, final_url) = with_retries(config, &stop, counters, &mut attempt)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        HttpError::fatal(HttpFailure::Hls {
            url: url.to_owned(),
            reason: "плейлист не в UTF-8".to_owned(),
        })
    })?;

    Ok((text, final_url))
}

/// Один загруженный сегмент и его место в плейлисте.
struct LoadedSegment {
    sequence: u64,
    segment: Segment,
    bytes: Arc<Vec<u8>>,
}

/// Что делать с очередным сегментом читателя.
enum SegmentStep {
    /// Уже скачан и готов.
    Ready(LoadedSegment),
    /// Нужно скачать синхронно (prefetch не успел или не запущен).
    Fetch(u64, Segment),
    /// Качается другим потоком — ждём уведомления.
    WaitSegment,
    /// Сегмента в окне пока нет — ждём обновления live-плейлиста.
    WaitPlaylist,
    /// VOD закончился.
    End,
}

/// Состояние плейлиста дорожки и окна скачанных сегментов.
struct TrackState {
    playlist: MediaPlaylist,
    fetched: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<u64>,
    loaded_bytes: usize,
    /// Сегменты, которые уже качаются (читателем или prefetch-воркером).
    in_flight: HashSet<u64>,
    /// `#EXT-X-MAP` по ключу `URI+диапазон`.
    maps: HashMap<String, Arc<Vec<u8>>>,
    /// Последовательности, на которых стоят читатели (для вытеснения).
    cursors: Vec<Arc<AtomicU64>>,
    last_refresh: Instant,
}

impl TrackState {
    fn find(&self, next: u64) -> Option<(u64, Segment)> {
        let index = next.saturating_sub(self.playlist.media_sequence) as usize;
        let segment = self.playlist.segments.get(index)?;

        Some((self.playlist.media_sequence + index as u64, segment.clone()))
    }

    fn insert(&mut self, sequence: u64, bytes: Arc<Vec<u8>>) -> Arc<Vec<u8>> {
        if let Some(existing) = self.fetched.get(&sequence) {
            return Arc::clone(existing);
        }

        self.loaded_bytes += bytes.len();
        self.fetched.insert(sequence, Arc::clone(&bytes));
        self.order.push_back(sequence);
        bytes
    }

    /// Вытесняет сегменты позади всех курсоров, пока окно не влезет в предел.
    fn evict(&mut self) {
        let min_cursor = self
            .cursors
            .iter()
            .map(|cursor| cursor.load(Ordering::Relaxed))
            .min()
            .unwrap_or(u64::MAX);

        while self.loaded_bytes > MAX_WINDOW_BYTES {
            let Some(&sequence) = self.order.front() else {
                break;
            };
            if sequence >= min_cursor {
                break;
            }
            self.order.pop_front();
            if let Some(bytes) = self.fetched.remove(&sequence) {
                self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes.len());
            }
        }
    }
}

/// Плейлист одной дорожки и загрузчик её сегментов.
struct TrackSource {
    source: Source,
    url: String,
    track: Track,
    context: Arc<HlsContext>,
    state: Mutex<TrackState>,
    /// Будит читателей и prefetch-воркеры при появлении данных.
    work: Condvar,
    refresh_lock: Mutex<()>,
    buffering: Arc<AtomicBool>,
    /// Первая сетевая ошибка для декодеров; они забирают её один раз.
    failure: Arc<Mutex<Option<HttpFailure>>>,
    /// Та же ошибка, но не забирается: по ней prefetch и читатели понимают,
    /// что источник сломан и повторять запросы бессмысленно.
    sticky_failure: Mutex<Option<HttpFailure>>,
    hint: String,
    start_sequence: AtomicU64,
    interval: Duration,
    /// Сколько воркеров запускать при первом читателе.
    workers: usize,
    /// Воркеры поднимаются лениво: без читателей prefetch не нужен.
    prefetch_started: Once,
}

impl TrackSource {
    fn new(
        source: Source,
        url: String,
        playlist: MediaPlaylist,
        track: Track,
        context: Arc<HlsContext>,
        workers: usize,
    ) -> Self {
        let hint = hint_for(&playlist, track);
        let interval = refresh_interval(&playlist);
        let start_sequence = tail_sequence(&playlist);

        Self {
            source,
            url,
            track,
            context,
            state: Mutex::new(TrackState {
                playlist,
                fetched: HashMap::new(),
                order: VecDeque::new(),
                loaded_bytes: 0,
                in_flight: HashSet::new(),
                maps: HashMap::new(),
                cursors: Vec::new(),
                last_refresh: Instant::now(),
            }),
            work: Condvar::new(),
            refresh_lock: Mutex::new(()),
            buffering: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
            sticky_failure: Mutex::new(None),
            hint,
            start_sequence: AtomicU64::new(start_sequence),
            interval,
            workers,
            prefetch_started: Once::new(),
        }
    }

    fn start_sequence(&self) -> u64 {
        self.start_sequence.load(Ordering::Relaxed)
    }

    /// Куда встанут новые читатели: сегмент целевого времени после перемотки.
    fn set_start_sequence(&self, sequence: u64) {
        self.start_sequence.store(sequence, Ordering::Relaxed);
        // Старые читатели будут закрыты сразу после перемотки: их курсоры
        // увели бы prefetch на прежнюю позицию.
        lock(&self.state).cursors.clear();
        self.work.notify_all();
    }

    fn is_live(&self) -> bool {
        !lock(&self.state).playlist.end_list
    }

    fn is_buffering(&self) -> bool {
        self.buffering.load(Ordering::Relaxed)
    }

    fn register_cursor(self: &Arc<Self>, cursor: Arc<AtomicU64>) {
        lock(&self.state).cursors.push(cursor);
        self.work.notify_all();
        // Ленивый старт: до первого читателя качать нечего, а серия
        // `open` + `seek_time` иначе успевала скачать начало VOD.
        self.prefetch_started
            .call_once(|| self.start_prefetch(self.workers));
    }

    fn unregister_cursor(&self, cursor: &Arc<AtomicU64>) {
        lock(&self.state)
            .cursors
            .retain(|existing| !Arc::ptr_eq(existing, cursor));
        self.work.notify_all();
    }

    fn record_failure(&self, failure: &HttpFailure) {
        if *failure == HttpFailure::Cancelled {
            return;
        }
        let mut sticky = lock(&self.sticky_failure);
        if sticky.is_none() {
            *sticky = Some(failure.clone());
        }
        drop(sticky);

        let mut slot = lock(&self.failure);
        if slot.is_none() {
            *slot = Some(failure.clone());
        }
        drop(slot);
        self.work.notify_all();
    }

    fn recorded_failure(&self) -> Option<HttpFailure> {
        lock(&self.sticky_failure).clone()
    }

    /// Запускает фоновых загрузчиков окна вперёд.
    ///
    /// Одно соединение к CDN часто упирается в ~100 КБ/с; несколько
    /// параллельных дают суммарную полосу, достаточную для live.
    fn start_prefetch(self: &Arc<Self>, workers: usize) {
        debug!(
            workers,
            track = ?self.track,
            window_ms = PREFETCH_WINDOW.as_millis(),
            max_segments = PREFETCH_MAX_SEGMENTS,
            "HLS: prefetch запущен"
        );
        for _ in 0..workers {
            let weak = Arc::downgrade(self);
            let _ = std::thread::Builder::new()
                .name("hls-prefetch".to_owned())
                .spawn(move || prefetch_worker(weak));
        }
    }

    fn wait_for_work(&self) {
        let state = lock(&self.state);
        let _ = self.work.wait_timeout(state, PREFETCH_IDLE);
    }

    /// Одна попытка загрузить сегмент вперёд; `false` — работы нет.
    fn prefetch_once(&self) -> bool {
        if self.recorded_failure().is_some() {
            return false;
        }

        let task = {
            let mut state = lock(&self.state);
            let base = self.prefetch_base(&state);
            let mut task = None;
            let span = prefetch_span(&state.playlist, base);
            for offset in 0..span {
                let Some((sequence, segment)) = state.find(base.saturating_add(offset)) else {
                    break;
                };
                // Зашифрованные сегменты prefetch не трогает: читатель
                // вернёт понятную ошибку, а иначе воркер зациклился бы.
                if state.fetched.contains_key(&sequence)
                    || state.in_flight.contains(&sequence)
                    || segment.key.is_some()
                {
                    continue;
                }
                state.in_flight.insert(sequence);
                task = Some((sequence, segment));
                break;
            }
            task
        };

        let Some((sequence, segment)) = task else {
            return false;
        };

        let result = fetch_resource(&self.context, &segment.uri, segment.byte_range);
        let mut state = lock(&self.state);
        state.in_flight.remove(&sequence);
        match result {
            Ok(bytes) => {
                let len = bytes.len();
                state.insert(sequence, Arc::new(bytes));
                state.evict();
                drop(state);
                debug!(sequence, bytes = len, "HLS: prefetch сегмента");
                self.work.notify_all();
            }
            Err(failure) => {
                drop(state);
                if failure != HttpFailure::Cancelled {
                    debug!(sequence, failure = ?failure, "HLS: prefetch не удался");
                    self.record_failure(&failure);
                }
            }
        }
        true
    }

    /// Откуда начинать prefetch: позиция самого отстающего читателя.
    fn prefetch_base(&self, state: &TrackState) -> u64 {
        state
            .cursors
            .iter()
            .map(|cursor| cursor.load(Ordering::Relaxed))
            .min()
            .unwrap_or_else(|| self.start_sequence())
    }

    /// Следующий сегмент начиная с `next`; `None` — VOD закончился.
    ///
    /// Для live ждёт обновления плейлиста, пока не появится новый сегмент или
    /// не придёт отмена.
    fn next_segment(&self, next: u64) -> Result<Option<LoadedSegment>, HttpFailure> {
        loop {
            if self.context.cancel.load(Ordering::Relaxed) {
                return Err(HttpFailure::Cancelled);
            }

            match self.segment_step(next) {
                SegmentStep::Ready(loaded) => {
                    self.buffering.store(false, Ordering::SeqCst);
                    return Ok(Some(loaded));
                }
                SegmentStep::End => {
                    self.buffering.store(false, Ordering::SeqCst);
                    return Ok(None);
                }
                SegmentStep::Fetch(sequence, segment) => {
                    if segment.key.is_some() {
                        lock(&self.state).in_flight.remove(&sequence);
                        return Err(HttpFailure::Hls {
                            url: segment.uri.clone(),
                            reason: "сегменты зашифрованы (AES-128)".to_owned(),
                        });
                    }

                    debug!(sequence, "HLS: скачиваем сегмент");
                    self.buffering.store(true, Ordering::SeqCst);
                    let result = fetch_resource(&self.context, &segment.uri, segment.byte_range);
                    self.buffering.store(false, Ordering::SeqCst);

                    let mut state = lock(&self.state);
                    state.in_flight.remove(&sequence);
                    match result {
                        Ok(bytes) => {
                            let stored = state.insert(sequence, Arc::new(bytes));
                            state.evict();
                            drop(state);
                            self.work.notify_all();
                            return Ok(Some(LoadedSegment {
                                sequence,
                                segment,
                                bytes: stored,
                            }));
                        }
                        Err(failure) => {
                            drop(state);
                            self.work.notify_all();
                            debug!(
                                source = %self.source,
                                track = ?self.track,
                                failure = ?failure,
                                "сегмент HLS не загрузился"
                            );
                            self.record_failure(&failure);
                            return Err(failure);
                        }
                    }
                }
                SegmentStep::WaitSegment => {
                    if let Some(failure) = self.recorded_failure() {
                        return Err(failure);
                    }
                    // Читатель дошёл до сегмента, который ещё качается
                    // prefetch-воркером: это и есть ожидание сети.
                    self.buffering.store(true, Ordering::SeqCst);
                    self.wait_for_work();
                }
                SegmentStep::WaitPlaylist => {
                    if let Some(failure) = self.recorded_failure() {
                        return Err(failure);
                    }
                    if let Err(failure) = self.wait_for_more() {
                        self.record_failure(&failure);
                        return Err(failure);
                    }
                }
            }
        }
    }

    /// Что делать с `next`: отдать, скачать самому, ждать prefetch/плейлист.
    ///
    /// Синхронная загрузка помечает сегмент `in_flight` под тем же локом,
    /// иначе prefetch-воркер мог начать тот же сегмент параллельно.
    fn segment_step(&self, next: u64) -> SegmentStep {
        let mut state = lock(&self.state);
        match state.find(next) {
            Some((sequence, segment)) => {
                if let Some(bytes) = state.fetched.get(&sequence) {
                    SegmentStep::Ready(LoadedSegment {
                        sequence,
                        segment,
                        bytes: Arc::clone(bytes),
                    })
                } else if state.in_flight.contains(&sequence) {
                    SegmentStep::WaitSegment
                } else {
                    state.in_flight.insert(sequence);
                    SegmentStep::Fetch(sequence, segment)
                }
            }
            None if state.playlist.end_list => SegmentStep::End,
            None => SegmentStep::WaitPlaylist,
        }
    }

    /// Ждёт следующего сегмента live-окна или обновляет плейлист.
    fn wait_for_more(&self) -> Result<(), HttpFailure> {
        let due = lock(&self.state).last_refresh + self.interval;
        let now = Instant::now();

        if now >= due {
            return self.refresh_playlist();
        }

        sleep_with_cancel(
            &self.context.cancel,
            (due - now).min(Duration::from_millis(100)),
        );
        if self.context.cancel.load(Ordering::Relaxed) {
            Err(HttpFailure::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Безусловно обновляет плейлист (расписание соблюдает `wait_for_more`).
    fn refresh_playlist(&self) -> Result<(), HttpFailure> {
        let _guard = lock(&self.refresh_lock);

        // Пока ждали лок, другой читатель или prefetch-воркер мог уже
        // обновить плейлист — повторный запрос не нужен.
        if lock(&self.state).last_refresh + self.interval > Instant::now() {
            return Ok(());
        }

        let (text, _) = fetch_playlist_text(
            &self.context.agent,
            &self.context.config,
            &self.context.counters,
            &self.context.cancel,
            &self.url,
        )
        .map_err(|error| error.failure)?;

        let playlist = match parse(&text, &self.url) {
            Ok(Playlist::Media(playlist)) => playlist,
            Ok(Playlist::Master(_)) => {
                return Err(HttpFailure::Hls {
                    url: self.url.clone(),
                    reason: "при обновлении пришёл master-плейлист".to_owned(),
                });
            }
            Err(error) => {
                return Err(HttpFailure::Hls {
                    url: self.url.clone(),
                    reason: error.to_string(),
                });
            }
        };

        let mut state = lock(&self.state);
        // Откат окна назад не должен переключать уже читаемый поток.
        if playlist.media_sequence < state.playlist.media_sequence {
            state.last_refresh = Instant::now();
            return Ok(());
        }
        debug!(
            media_sequence = playlist.media_sequence,
            segments = playlist.segments.len(),
            "HLS: плейлист обновлён"
        );
        state.playlist = playlist;
        state.last_refresh = Instant::now();
        state.evict();
        drop(state);
        self.work.notify_all();
        Ok(())
    }

    /// Init-секция fMP4; кэшируется по `URI` и диапазону.
    fn map_bytes(&self, map: &Map) -> Result<Arc<Vec<u8>>, HttpFailure> {
        let key = map_key(map);
        {
            let state = lock(&self.state);
            if let Some(bytes) = state.maps.get(&key) {
                return Ok(Arc::clone(bytes));
            }
        }

        self.buffering.store(true, Ordering::SeqCst);
        let result = fetch_resource(&self.context, &map.uri, map.byte_range);
        self.buffering.store(false, Ordering::SeqCst);

        let bytes = match result {
            Ok(bytes) => Arc::new(bytes),
            Err(failure) => {
                self.record_failure(&failure);
                return Err(failure);
            }
        };

        lock(&self.state).maps.insert(key, Arc::clone(&bytes));
        Ok(bytes)
    }

    fn failure_slot(&self) -> Arc<Mutex<Option<HttpFailure>>> {
        Arc::clone(&self.failure)
    }

    fn buffering_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.buffering)
    }

    /// Сегмент с целевым временем в VOD-плейлисте.
    fn sequence_at(&self, position: Duration) -> Option<u64> {
        let state = lock(&self.state);
        if !state.playlist.end_list {
            return None;
        }

        let mut elapsed = Duration::ZERO;
        for (index, segment) in state.playlist.segments.iter().enumerate() {
            let end = elapsed.saturating_add(segment.duration);
            if position < end {
                return Some(state.playlist.media_sequence + index as u64);
            }
            elapsed = end;
        }

        let last = state.playlist.segments.len().checked_sub(1)?;
        Some(state.playlist.media_sequence + last as u64)
    }

    /// Начало live-edge по `PROGRAM-DATE-TIME`.
    fn live_edge_date(&self) -> Option<i64> {
        let state = lock(&self.state);
        let dates = start_dates(&state.playlist);
        let last = state.playlist.segments.len().checked_sub(1)?;
        let start = dates.get(last).copied().flatten()?;
        let duration = state.playlist.segments[last].duration.as_secs();

        start.checked_add(duration as i64)
    }

    /// Время начала первого читаемого сегмента по `PROGRAM-DATE-TIME`.
    fn start_date(&self) -> Option<i64> {
        let state = lock(&self.state);
        let dates = start_dates(&state.playlist);
        let index = self
            .start_sequence()
            .saturating_sub(state.playlist.media_sequence) as usize;

        dates.get(index).copied().flatten()
    }

    /// Ставит старт на сегмент, содержащий момент `date`.
    fn align_to_date(&self, date: i64) {
        let state = lock(&self.state);
        let dates = start_dates(&state.playlist);

        let mut fallback = None;
        for (index, start) in dates.iter().enumerate() {
            let Some(start) = *start else {
                continue;
            };
            let duration = state.playlist.segments[index].duration.as_secs() as i64;
            let end = start.saturating_add(duration);
            if start <= date && date < end {
                self.start_sequence.store(
                    state.playlist.media_sequence + index as u64,
                    Ordering::Relaxed,
                );
                self.work.notify_all();
                return;
            }
            if start >= date && fallback.is_none() {
                fallback = Some(state.playlist.media_sequence + index as u64);
            }
        }

        if let Some(sequence) = fallback {
            self.start_sequence.store(sequence, Ordering::Relaxed);
            self.work.notify_all();
        }
    }
}

/// Фоновый загрузчик окна: качает сегменты впереди читателей.
fn prefetch_worker(weak: Weak<TrackSource>) {
    loop {
        let Some(track) = weak.upgrade() else {
            return;
        };
        if track.context.cancel.load(Ordering::Relaxed) {
            return;
        }

        if track.prefetch_once() {
            continue;
        }

        // Работы нет: для live можно обновить плейлист, иначе просто ждём.
        if track.is_live()
            && let Err(failure) = track.wait_for_more()
        {
            if failure == HttpFailure::Cancelled {
                return;
            }
            track.record_failure(&failure);
        }
        track.wait_for_work();
    }
}

/// Читатель одной дорожки: последовательный поток скачанных сегментов.
pub(crate) struct HlsReader {
    track: Arc<TrackSource>,
    /// Сегмент, к которому вернётся `SeekFrom::Start(0)`.
    initial_sequence: u64,
    next_sequence: u64,
    cursor: Arc<AtomicU64>,
    pending: VecDeque<Arc<Vec<u8>>>,
    pending_offset: usize,
    pos: u64,
    last_map: Option<String>,
    ended: bool,
    /// Пересечён `#EXT-X-DISCONTINUITY`: декодеру пора сбросить состояние.
    discontinuity: Arc<AtomicBool>,
}

impl HlsReader {
    fn new(track: Arc<TrackSource>) -> Self {
        let start = track.start_sequence();
        let cursor = Arc::new(AtomicU64::new(start));
        track.register_cursor(Arc::clone(&cursor));

        Self {
            track,
            initial_sequence: start,
            next_sequence: start,
            cursor,
            pending: VecDeque::new(),
            pending_offset: 0,
            pos: 0,
            last_map: None,
            ended: false,
            discontinuity: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Перематывает VOD к сегменту с целевым временем.
    ///
    /// В рабочем пути перемотка идёт через [`HlsSource::seek_time`]; метод
    /// остаётся для точечных тестов читателя.
    #[cfg(test)]
    pub(crate) fn seek_time(&mut self, position: Duration) -> Result<(), PlayerError> {
        let sequence =
            self.track
                .sequence_at(position)
                .ok_or_else(|| PlayerError::NotSeekable {
                    target: self.track.source.clone(),
                })?;

        self.rewind_to(sequence);
        self.initial_sequence = sequence;
        Ok(())
    }

    /// Слот сетевой ошибки для слоя декодера.
    pub(crate) fn failure_slot(&self) -> Arc<Mutex<Option<HttpFailure>>> {
        self.track.failure_slot()
    }

    /// Флаг «ждём сегмент» для события `Buffering`.
    pub(crate) fn buffering_flag(&self) -> Arc<AtomicBool> {
        self.track.buffering_flag()
    }

    /// Флаг разрыва потока: декодер сбрасывается на следующем пакете.
    pub(crate) fn discontinuity_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.discontinuity)
    }

    /// Подгружает следующий сегмент (и init fMP4, если он сменился).
    ///
    /// `Ok(false)` — поток закончился.
    fn fill(&mut self) -> Result<bool, HttpFailure> {
        debug!(next = self.next_sequence, "HLS: читаем следующий сегмент");
        let Some(loaded) = self.track.next_segment(self.next_sequence)? else {
            return Ok(false);
        };
        debug!(
            sequence = loaded.sequence,
            bytes = loaded.bytes.len(),
            "HLS: сегмент получен"
        );

        self.next_sequence = loaded.sequence.saturating_add(1);
        self.cursor.store(self.next_sequence, Ordering::Relaxed);

        // Разрыв потока: init fMP4 нужно повторить, даже если ключ тот же.
        if loaded.segment.discontinuity {
            self.last_map = None;
            self.discontinuity.store(true, Ordering::Relaxed);
        }

        if let Some(map) = &loaded.segment.map {
            let key = map_key(map);
            if self.last_map.as_deref() != Some(key.as_str()) {
                let bytes = self.track.map_bytes(map)?;
                self.pending.push_back(bytes);
                self.last_map = Some(key);
            }
        }

        self.pending.push_back(loaded.bytes);
        Ok(true)
    }

    fn rewind_to(&mut self, sequence: u64) {
        self.pending.clear();
        self.pending_offset = 0;
        self.pos = 0;
        self.last_map = None;
        self.ended = false;
        self.next_sequence = sequence;
        self.cursor.store(sequence, Ordering::Relaxed);
    }

    fn skip(&mut self, mut bytes: u64) -> io::Result<()> {
        let mut scratch = [0_u8; READ_CHUNK];

        while bytes > 0 {
            let want = (bytes as usize).min(scratch.len());
            let read = self.read(&mut scratch[..want])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HLS-поток закончился до нужной позиции",
                ));
            }
            bytes -= read as u64;
        }

        Ok(())
    }
}

impl Drop for HlsReader {
    fn drop(&mut self) {
        self.track.unregister_cursor(&self.cursor);
    }
}

impl Read for HlsReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        loop {
            if let Some(front) = self.pending.front() {
                let available = &front[self.pending_offset..];
                let take = available.len().min(buffer.len());
                buffer[..take].copy_from_slice(&available[..take]);
                self.pending_offset += take;
                self.pos += take as u64;

                if self.pending_offset == front.len() {
                    self.pending.pop_front();
                    self.pending_offset = 0;
                }
                return Ok(take);
            }

            if self.ended {
                return Ok(0);
            }

            match self.fill() {
                Ok(true) => {}
                Ok(false) => {
                    self.ended = true;
                    return Ok(0);
                }
                Err(HttpFailure::Cancelled) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "чтение отменено",
                    ));
                }
                Err(failure) => return Err(failure.into_io()),
            }
        }
    }
}

impl Seek for HlsReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match position {
            SeekFrom::Current(0) => Ok(self.pos),
            SeekFrom::Start(0) => {
                let initial = self.initial_sequence;
                self.rewind_to(initial);
                Ok(0)
            }
            SeekFrom::Start(target) => {
                if target < self.pos {
                    let initial = self.initial_sequence;
                    self.rewind_to(initial);
                }
                self.skip(target.saturating_sub(self.pos))?;
                Ok(self.pos)
            }
            SeekFrom::Current(delta) if delta > 0 => {
                self.skip(delta as u64)?;
                Ok(self.pos)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "HLS-поток не поддерживает байтовую перемотку",
            )),
        }
    }
}

impl MediaReader for HlsReader {
    fn len(&self) -> Option<u64> {
        None
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn hint_name(&self) -> Option<&str> {
        Some(&self.track.hint)
    }
}

/// Загружает сегмент или init-секцию; `Range` — для `#EXT-X-BYTERANGE`.
fn fetch_resource(
    context: &HlsContext,
    url: &str,
    range: Option<ByteRange>,
) -> Result<Vec<u8>, HttpFailure> {
    let stop = || context.cancel.load(Ordering::Relaxed);
    let range_header =
        range.map(|range| format!("bytes={}-{}", range.offset, range.offset + range.length - 1));
    let extra: Vec<(&str, &str)> = range_header
        .as_deref()
        .map(|value| vec![("Range", value)])
        .unwrap_or_default();

    let mut attempt = || {
        if stop() {
            return Err(HttpError::cancelled());
        }

        context.counters.add_request();
        let started = Instant::now();
        let response = get(&context.agent, &context.config, url, &extra)?;
        let status = response.status().as_u16();
        let mut body = response
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();

        let bytes = match range {
            None => {
                if status != 200 {
                    return Err(status_error(status));
                }
                read_limit(body, u64::MAX, &stop, &context.counters)?
            }
            Some(range) => match status {
                206 => read_exact_bytes(&mut body, range.length, url, &stop, &context.counters)?,
                200 => {
                    skip_bytes(&mut body, range.offset, url, &stop, &context.counters)?;
                    read_exact_bytes(&mut body, range.length, url, &stop, &context.counters)?
                }
                _ => return Err(status_error(status)),
            },
        };

        if bytes.is_empty() {
            return Err(HttpError::retryable(HttpFailure::Hls {
                url: url.to_owned(),
                reason: "пустой сегмент".to_owned(),
            }));
        }

        let elapsed = started.elapsed();
        if elapsed > SLOW_READ {
            context.counters.add_slow_read(elapsed);
        }
        Ok(bytes)
    };

    with_retries(&context.config, &stop, &context.counters, &mut attempt)
        .map_err(|error| error.failure)
}

/// Читает тело целиком, но не больше `limit + 1` байт.
fn read_limit(
    mut body: ureq::BodyReader<'static>,
    limit: u64,
    stop: &dyn Fn() -> bool,
    counters: &CacheCounters,
) -> Result<Vec<u8>, HttpError> {
    let mut buffer = Vec::new();
    let mut scratch = [0_u8; READ_CHUNK];

    loop {
        if stop() {
            return Err(HttpError::cancelled());
        }

        let remaining = limit.saturating_add(1).saturating_sub(buffer.len() as u64);
        if remaining == 0 {
            break;
        }

        let want = remaining.min(READ_CHUNK as u64) as usize;
        let read = body.read(&mut scratch[..want]).map_err(body_error)?;
        if read == 0 {
            break;
        }

        counters.add_bytes(read);
        buffer.extend_from_slice(&scratch[..read]);
    }

    Ok(buffer)
}

/// Читает ровно `length` байт; короткий ответ — повторяемый обрыв.
fn read_exact_bytes(
    body: &mut ureq::BodyReader<'static>,
    length: u64,
    url: &str,
    stop: &dyn Fn() -> bool,
    counters: &CacheCounters,
) -> Result<Vec<u8>, HttpError> {
    let mut buffer = Vec::new();
    let mut scratch = [0_u8; READ_CHUNK];

    while (buffer.len() as u64) < length {
        if stop() {
            return Err(HttpError::cancelled());
        }

        let want = (length - buffer.len() as u64).min(READ_CHUNK as u64) as usize;
        let read = body.read(&mut scratch[..want]).map_err(body_error)?;
        if read == 0 {
            return Err(HttpError::retryable(HttpFailure::Hls {
                url: url.to_owned(),
                reason: "сегмент короче BYTERANGE".to_owned(),
            }));
        }

        counters.add_bytes(read);
        buffer.extend_from_slice(&scratch[..read]);
    }

    Ok(buffer)
}

/// Пропускает `length` байт тела (сервер без поддержки `Range`).
fn skip_bytes(
    body: &mut ureq::BodyReader<'static>,
    mut length: u64,
    url: &str,
    stop: &dyn Fn() -> bool,
    counters: &CacheCounters,
) -> Result<(), HttpError> {
    let mut scratch = [0_u8; READ_CHUNK];

    while length > 0 {
        if stop() {
            return Err(HttpError::cancelled());
        }

        let want = (length as usize).min(scratch.len());
        let read = body.read(&mut scratch[..want]).map_err(body_error)?;
        if read == 0 {
            return Err(HttpError::retryable(HttpFailure::Hls {
                url: url.to_owned(),
                reason: "поток оборвался до BYTERANGE".to_owned(),
            }));
        }

        counters.add_bytes(read);
        length -= read as u64;
    }

    Ok(())
}

fn sleep_with_cancel(cancel: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;

    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

/// Ключ init-секции: `URI` плюс байтовый диапазон.
fn map_key(map: &Map) -> String {
    let (offset, length) = map
        .byte_range
        .map_or((0, 0), |range| (range.offset, range.length));
    format!("{}#{offset}#{length}", map.uri)
}

/// Какую подсказку дать FFmpeg для определения формата.
fn hint_for(playlist: &MediaPlaylist, track: Track) -> String {
    if playlist
        .segments
        .iter()
        .any(|segment| segment.map.is_some())
    {
        return if track == Track::Audio {
            "stream.m4a".to_owned()
        } else {
            "stream.mp4".to_owned()
        };
    }

    let extension = playlist
        .segments
        .first()
        .and_then(|segment| segment.uri.rsplit('/').next())
        .and_then(|name| name.split(['?', '#']).next())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, extension)| extension.to_ascii_lowercase());

    match extension.as_deref() {
        Some("aac") => "stream.aac".to_owned(),
        Some("mp3") => "stream.mp3".to_owned(),
        Some("mp4" | "m4s" | "m4a") => "stream.mp4".to_owned(),
        Some("ts") => "stream.ts".to_owned(),
        _ if track == Track::Audio => "stream.aac".to_owned(),
        _ => "stream.ts".to_owned(),
    }
}

/// Как часто обновлять live-плейлист: `TARGETDURATION`, но не чаще раза в секунду.
fn refresh_interval(playlist: &MediaPlaylist) -> Duration {
    let target = playlist.target_duration.unwrap_or_else(|| {
        playlist
            .segments
            .iter()
            .map(|segment| segment.duration)
            .max()
            .unwrap_or(MIN_REFRESH)
    });

    target.max(MIN_REFRESH)
}

/// Сколько сегментов от `base` покрывают [`PREFETCH_WINDOW`].
///
/// Хвост окна может быть ближе: считаем только доступные сегменты.
fn prefetch_span(playlist: &MediaPlaylist, base: u64) -> u64 {
    let first = base.saturating_sub(playlist.media_sequence) as usize;
    let mut covered = Duration::ZERO;
    let mut count = 0;

    for offset in 0..PREFETCH_MAX_SEGMENTS {
        let Some(segment) = first
            .checked_add(offset as usize)
            .and_then(|index| playlist.segments.get(index))
        else {
            break;
        };

        count = offset + 1;
        covered = covered.saturating_add(segment.duration);
        if covered >= PREFETCH_WINDOW {
            break;
        }
    }

    count
}

/// Стартовая последовательность: VOD — с начала, live — с хвоста окна.
fn tail_sequence(playlist: &MediaPlaylist) -> u64 {
    if playlist.end_list || playlist.segments.len() < 2 {
        return playlist.media_sequence;
    }

    let target = playlist.target_duration.unwrap_or_else(|| {
        playlist
            .segments
            .last()
            .map_or(MIN_REFRESH, |last| last.duration)
    });
    let wanted = target.saturating_mul(2).max(LIVE_PRIMING);

    let mut index = playlist.segments.len() - 1;
    let mut accumulated = playlist.segments[index].duration;
    let mut count = 1_usize;
    while index > 0 && (accumulated < wanted || count < 2) {
        index -= 1;
        accumulated = accumulated.saturating_add(playlist.segments[index].duration);
        count += 1;
    }

    playlist.media_sequence + index as u64
}

/// Начало каждого сегмента по `PROGRAM-DATE-TIME` (с достройкой промежутков).
fn start_dates(playlist: &MediaPlaylist) -> Vec<Option<i64>> {
    let mut dates = vec![None; playlist.segments.len()];
    let mut current: Option<i64> = None;

    for (index, segment) in playlist.segments.iter().enumerate() {
        if let Some(date) = segment.program_date_time {
            current = Some(date);
        }
        dates[index] = current;
        if let Some(value) = current {
            current = value.checked_add(segment.duration.as_secs() as i64);
        }
    }

    if let Some(anchor) = dates.iter().position(Option::is_some) {
        for index in (0..anchor).rev() {
            dates[index] = dates[index + 1].and_then(|next| {
                next.checked_sub(playlist.segments[index].duration.as_secs() as i64)
            });
        }
    }

    dates
}

/// Выравнивает старт demuxed live-дорожек по общему моменту времени.
fn align_live_tracks(video: &TrackSource, audio: Option<&Arc<TrackSource>>) {
    let Some(audio) = audio else {
        return;
    };
    let (Some(video_edge), Some(audio_edge)) = (video.live_edge_date(), audio.live_edge_date())
    else {
        return;
    };

    let target = video_edge
        .min(audio_edge)
        .saturating_sub(LIVE_PRIMING.as_secs() as i64);
    video.align_to_date(target);
    audio.align_to_date(target);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tiny_http::{Header, Response, StatusCode};

    use super::*;
    use crate::http_options::HttpOptions;

    const BASE: &str = "https://cdn.example.com/live/stream/index.m3u8?token=abc";

    fn syntax(error: &HlsError) -> (usize, &'static str) {
        match error {
            HlsError::Syntax { line, reason } => (*line, reason),
            other => panic!("ожидалась ошибка Syntax, получено {other:?}"),
        }
    }

    fn media(playlist: Playlist) -> MediaPlaylist {
        match playlist {
            Playlist::Media(media) => media,
            Playlist::Master(master) => panic!("ожидался media-плейлист, получен {master:?}"),
        }
    }

    fn master(playlist: Playlist) -> MasterPlaylist {
        match playlist {
            Playlist::Master(master) => master,
            Playlist::Media(media) => panic!("ожидался master-плейлист, получен {media:?}"),
        }
    }

    /// Каталог для реальных плейлистов и сгенерированных артефактов.
    fn hls_test_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target")
            .join("hls-tests")
    }

    /// Уникальный каталог под `target/`, чтобы артефакты можно было посмотреть.
    fn generated_dir() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

        hls_test_root().join(format!("{}-{nanos}-{id}", std::process::id()))
    }

    /// Генерирует VOD-плейлист с сегментами в `target/hls-tests`.
    ///
    /// `None` — нет системного `ffmpeg` с нужными кодеками: тест пропускается.
    fn generate_hls(extra: &[&str]) -> Option<PathBuf> {
        let directory = generated_dir();
        std::fs::create_dir_all(&directory).ok()?;

        // Относительное имя и `current_dir`: ffmpeg пишет `init.mp4` туда же,
        // где лежит плейлист, а не в текущий каталог тестового процесса.
        let status = Command::new("ffmpeg")
            .current_dir(&directory)
            .args(["-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440"])
            .args(["-t", "2"])
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "5",
            ])
            .args(["-c:a", "aac"])
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
            .args(extra)
            .arg("index.m3u8")
            .status();

        match status {
            Ok(status) if status.success() => Some(directory),
            _ => {
                let _ = std::fs::remove_dir_all(&directory);
                None
            }
        }
    }

    fn parse_generated(directory: &Path) -> MediaPlaylist {
        let text = std::fs::read_to_string(directory.join("index.m3u8"))
            .expect("ffmpeg должен создать плейлист");
        media(
            parse(&text, "https://example.com/hls/index.m3u8").expect("разобрать плейлист ffmpeg"),
        )
    }

    fn file_name(uri: &str) -> &str {
        uri.rsplit('/').next().unwrap_or(uri)
    }

    /// Media-плейлист из одних `#EXTINF` — для проверки окна prefetch.
    fn media_with_durations(media_sequence: u64, durations: &[f64]) -> MediaPlaylist {
        let segments: Vec<Segment> = durations
            .iter()
            .enumerate()
            .map(|(index, seconds)| Segment {
                uri: format!("https://cdn.example.com/seg{index}.ts"),
                duration: Duration::from_secs_f64(*seconds),
                byte_range: None,
                map: None,
                discontinuity: false,
                key: None,
                program_date_time: None,
            })
            .collect();
        let total_duration: Duration = segments.iter().map(|segment| segment.duration).sum();

        MediaPlaylist {
            target_duration: Some(Duration::from_secs(2)),
            media_sequence,
            end_list: false,
            segments,
            total_duration,
        }
    }

    #[test]
    fn prefetch_window_counts_seconds_not_segments() {
        // Twitch: 2-секундные сегменты упираются в верхнюю границу в 8 штук.
        let twitch = media_with_durations(0, &[2.0; 20]);
        assert_eq!(prefetch_span(&twitch, 0), PREFETCH_MAX_SEGMENTS);

        // Apple: 6-секундные сегменты — 16 секунд покрывают три.
        let apple = media_with_durations(0, &[6.0; 10]);
        assert_eq!(prefetch_span(&apple, 0), 3);

        // Нулевые `#EXTINF` не раздувают окно сверх предела.
        let broken = media_with_durations(0, &[0.0; 20]);
        assert_eq!(prefetch_span(&broken, 0), PREFETCH_MAX_SEGMENTS);
    }

    #[test]
    fn prefetch_window_stops_at_the_tail_and_respects_sequence() {
        let playlist = media_with_durations(500, &[6.0; 4]);
        assert_eq!(prefetch_span(&playlist, 502), 2, "хвост окна");
        assert_eq!(
            prefetch_span(&playlist, 400),
            3,
            "устаревший курсор — от начала окна"
        );
    }

    #[test]
    fn master_lists_variants_with_attributes() {
        let text = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="ru",URI="audio/ru.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.4d401e,mp4a.40.2"
low/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,AUDIO="audio"
hd/index.m3u8?token=1
"#;

        let master = master(
            parse(text, "https://cdn.example.com/hls/master.m3u8").expect("разобрать master"),
        );

        assert_eq!(master.variants.len(), 2);

        let low = &master.variants[0];
        assert_eq!(low.uri, "https://cdn.example.com/hls/low/index.m3u8");
        assert_eq!(low.bandwidth, Some(800_000));
        assert_eq!(low.resolution, Some((640, 360)));
        assert_eq!(low.codecs.as_deref(), Some("avc1.4d401e,mp4a.40.2"));

        let hd = &master.variants[1];
        assert_eq!(hd.uri, "https://cdn.example.com/hls/hd/index.m3u8?token=1");
        assert_eq!(hd.bandwidth, Some(2_500_000));
        assert_eq!(hd.resolution, Some((1280, 720)));
        assert_eq!(hd.codecs, None);
    }

    #[test]
    fn relative_and_absolute_uris_are_resolved() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:6
#EXTINF:6,
seg1.ts
#EXTINF:6,
/other/seg2.ts
#EXTINF:6,
//backup.example.com/seg3.ts
#EXTINF:6,
../seg4.ts
#EXTINF:6,
?token=xyz
#EXTINF:6,
https://absolute.example.com/seg5.ts
#EXT-X-ENDLIST
"#;

        let media = media(parse(text, BASE).expect("разобрать media"));
        let uris: Vec<&str> = media
            .segments
            .iter()
            .map(|segment| segment.uri.as_str())
            .collect();

        assert_eq!(
            uris,
            [
                "https://cdn.example.com/live/stream/seg1.ts",
                "https://cdn.example.com/other/seg2.ts",
                "https://backup.example.com/seg3.ts",
                "https://cdn.example.com/live/seg4.ts",
                "https://cdn.example.com/live/stream/index.m3u8?token=xyz",
                "https://absolute.example.com/seg5.ts",
            ]
        );
    }

    #[test]
    fn relative_uri_on_a_base_without_a_path() {
        let text = "#EXTM3U\n#EXTINF:4,\nseg.ts\n";
        let media = media(parse(text, "https://cdn.example.com?x=1").expect("разобрать media"));

        assert_eq!(
            media.segments[0].uri, "https://cdn.example.com/seg.ts",
            "без пути в базе сегмент должен оказаться в корне"
        );
    }

    #[test]
    fn media_reports_durations_sequence_and_endlist() {
        let text = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:7
#EXT-X-PLAYLIST-TYPE:VOD
#EXTINF:5.067,первый
seg0.ts
#EXTINF:5.067,
seg1.ts
#EXT-X-ENDLIST
"#;

        let media = media(parse(text, BASE).expect("разобрать media"));

        assert_eq!(media.target_duration, Some(Duration::from_secs(6)));
        assert_eq!(media.media_sequence, 7);
        assert!(media.end_list);
        assert_eq!(media.segments.len(), 2);
        assert_eq!(media.segments[0].duration, Duration::from_secs_f64(5.067));
        assert_eq!(
            media.total_duration,
            media.segments[0].duration + media.segments[1].duration
        );
    }

    #[test]
    fn playlist_without_endlist_is_live() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXTINF:4,
seg1.ts
#EXTINF:4,
seg2.ts
"#;

        let media = media(parse(text, BASE).expect("разобрать live media"));

        assert!(!media.end_list);
        assert_eq!(media.media_sequence, 0);
        assert_eq!(media.segments.len(), 2);
    }

    #[test]
    fn map_byte_ranges_and_discontinuity_reach_segments() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MAP:URI="init.mp4",BYTERANGE="720@0"
#EXTINF:4,
#EXT-X-DISCONTINUITY
main.mp4
#EXTINF:4,
#EXT-X-BYTERANGE:1000@720
main.mp4
#EXTINF:4,
#EXT-X-BYTERANGE:500
main.mp4
#EXT-X-ENDLIST
"#;

        let media = media(parse(text, BASE).expect("разобрать fMP4"));

        let init = Map {
            uri: "https://cdn.example.com/live/stream/init.mp4".to_owned(),
            byte_range: Some(ByteRange {
                length: 720,
                offset: 0,
            }),
        };
        let first = &media.segments[0];
        assert_eq!(first.map, Some(init.clone()));
        assert!(first.discontinuity);
        assert_eq!(first.byte_range, None);

        let second = &media.segments[1];
        assert_eq!(second.map, Some(init.clone()));
        assert!(!second.discontinuity);
        assert_eq!(
            second.byte_range,
            Some(ByteRange {
                length: 1000,
                offset: 720,
            })
        );

        let third = &media.segments[2];
        assert_eq!(third.map, Some(init));
        assert_eq!(
            third.byte_range,
            Some(ByteRange {
                length: 500,
                offset: 1720,
            })
        );
    }

    #[test]
    fn key_applies_until_method_none() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x000102030405060708090a0b0c0d0e0f
#EXTINF:4,
enc.ts
#EXT-X-KEY:METHOD=NONE
#EXTINF:4,
clear.ts
#EXT-X-ENDLIST
"#;

        let media = media(parse(text, BASE).expect("разобрать ключи"));

        let first = media.segments[0].key.as_ref().expect("сегмент зашифрован");
        assert_eq!(first.method, "AES-128");
        assert_eq!(
            first.uri.as_deref(),
            Some("https://cdn.example.com/live/stream/key.bin")
        );
        assert_eq!(
            first.iv,
            Some([
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f,
            ])
        );

        assert_eq!(media.segments[1].key, None);
    }

    #[test]
    fn comments_blank_lines_crlf_and_bom_are_tolerated() {
        let text =
            "\u{feff}#EXTM3U\r\n\r\n# комментарий\r\n#EXTINF:4,\r\nseg.ts\r\n#EXT-X-ENDLIST\r\n";

        let media = media(parse(text, BASE).expect("разобрать плейлист с BOM и CRLF"));

        assert!(media.end_list);
        assert_eq!(media.segments.len(), 1);
        assert_eq!(
            media.segments[0].uri,
            "https://cdn.example.com/live/stream/seg.ts"
        );
    }

    #[test]
    fn ffmpeg_ts_hls_is_parsed() {
        let Some(directory) = generate_hls(&[]) else {
            eprintln!("нет системного ffmpeg с libx264/aac — пропускаем HLS-парсер");
            return;
        };

        let media = parse_generated(&directory);
        assert!(media.end_list, "ffmpeg пишет VOD-плейлист");
        assert!(media.segments.len() >= 2);
        assert!(media.total_duration >= Duration::from_secs(1));

        for segment in &media.segments {
            let path = directory.join(file_name(&segment.uri));
            assert!(path.is_file(), "сегмент {} не создан", segment.uri);
        }

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ffmpeg_fmp4_hls_has_init_segment() {
        let Some(directory) = generate_hls(&["-hls_segment_type", "fmp4"]) else {
            eprintln!("нет системного ffmpeg с libx264/aac — пропускаем fMP4");
            return;
        };

        let media = parse_generated(&directory);
        assert!(media.segments.len() >= 2);

        for segment in &media.segments {
            let map = segment.map.as_ref().expect("fMP4 должен иметь #EXT-X-MAP");
            assert!(
                directory.join(file_name(&map.uri)).is_file(),
                "init-сегмент {} не найден",
                map.uri
            );
            assert!(
                directory.join(file_name(&segment.uri)).is_file(),
                "сегмент {} не найден",
                segment.uri
            );
        }

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ffmpeg_single_file_hls_uses_byte_ranges() {
        let Some(directory) = generate_hls(&["-hls_flags", "single_file"]) else {
            eprintln!("нет системного ffmpeg с libx264/aac — пропускаем single_file");
            return;
        };

        let media = parse_generated(&directory);
        assert!(media.segments.len() >= 2);

        let mut next_offset = 0;
        for segment in &media.segments {
            let range = segment
                .byte_range
                .expect("single_file даёт #EXT-X-BYTERANGE");
            assert_eq!(range.offset, next_offset, "диапазоны должны идти подряд");
            next_offset = range.offset + range.length;
        }

        let resource = directory.join(file_name(&media.segments[0].uri));
        let size = std::fs::metadata(&resource).expect("файл сегментов").len();
        assert!(
            next_offset <= size,
            "диапазоны выходят за файл: {next_offset} > {size}"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn live_specific_tags_are_ignored() {
        let text = r#"#EXTM3U
#EXT-X-VERSION:6
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:42
#EXT-X-DISCONTINUITY-SEQUENCE:7
#EXT-X-PROGRAM-DATE-TIME:2026-11-23T13:00:00Z
#EXTINF:4.0,
seg42.ts
#EXT-X-PROGRAM-DATE-TIME:2026-11-23T13:00:04Z
#EXTINF:4.0,
seg43.ts
#EXT-X-GAP
#EXTINF:4.0,
seg44.ts
"#;

        let media = media(parse(text, BASE).expect("разобрать live-плейлист"));

        assert!(!media.end_list);
        assert_eq!(media.media_sequence, 42);
        assert_eq!(media.segments.len(), 3);
    }

    #[test]
    fn truncated_playlists_do_not_panic() {
        let valid = r#"#EXTM3U
#EXT-X-TARGETDURATION:4
#EXT-X-MEDIA-SEQUENCE:3
#EXT-X-KEY:METHOD=AES-128,URI="key.bin",IV=0x0102
#EXT-X-MAP:URI="init.mp4",BYTERANGE="100@0"
#EXTINF:4,
#EXT-X-BYTERANGE:10@0
seg.ts
#EXT-X-ENDLIST
"#;

        for (index, _) in valid.char_indices() {
            let _ = parse(&valid[..index], BASE);
        }
        for suffix in ["\u{0}", "&", "@", "\"", "#EXT-X-KEY:"] {
            let _ = parse(&format!("{valid}{suffix}"), BASE);
        }
    }

    /// Разбирает реальные плейлисты, если они лежат в `target/hls-tests`.
    ///
    /// Файлы не в git: тест только для ручной проверки на живых CDN.
    #[test]
    fn real_world_playlists_are_parsed_when_present() {
        let names = [
            "real-master-fmp4.m3u8",
            "real-variant-fmp4.m3u8",
            "real-audio-fmp4.m3u8",
            "real-vod.m3u8",
            "index-video.m3u8",
            "index-audio.m3u8",
            "real-live-twitch.m3u8",
            "real-live-video.m3u8",
            "real-live-audio.m3u8",
        ];
        let mut checked = 0;

        for name in names {
            let path = hls_test_root().join(name);
            if !path.is_file() {
                continue;
            }

            let text = std::fs::read_to_string(&path).expect("прочитать реальный плейлист");
            let parsed = parse(&text, "https://example.com/hls/index.m3u8")
                .unwrap_or_else(|error| panic!("{name}: {error}"));

            match parsed {
                Playlist::Master(master) => {
                    assert!(!master.variants.is_empty(), "{name}: нет вариантов");
                    let audio_groups = text
                        .lines()
                        .filter(|line| {
                            line.starts_with("#EXT-X-MEDIA:") && line.contains("TYPE=AUDIO")
                        })
                        .count();
                    eprintln!(
                        "{name}: master, вариантов {}, #EXT-X-MEDIA audio {}",
                        master.variants.len(),
                        audio_groups
                    );
                }
                Playlist::Media(media) => {
                    assert!(!media.segments.is_empty(), "{name}: нет сегментов");
                    eprintln!(
                        "{name}: media, сегментов {}, live {}, sequence {}, {} мс",
                        media.segments.len(),
                        !media.end_list,
                        media.media_sequence,
                        media.total_duration.as_millis()
                    );
                }
            }

            checked += 1;
        }

        if checked == 0 {
            eprintln!("реальные плейлисты не найдены в target/hls-tests — пропускаем");
        }
    }

    #[test]
    fn empty_playlist_is_an_error() {
        assert_eq!(parse("", BASE), Err(HlsError::Empty));
        assert_eq!(parse(" \r\n\n", BASE), Err(HlsError::Empty));
    }

    #[test]
    fn missing_header_is_reported() {
        assert_eq!(
            parse("#EXTINF:4,\nseg.ts\n", BASE),
            Err(HlsError::MissingHeader)
        );
    }

    #[test]
    fn broken_lines_name_the_line_and_reason() {
        let error = parse("#EXTM3U\n#EXTINF:abc,\nseg.ts\n", BASE).expect_err("битый EXTINF");
        assert_eq!(syntax(&error).0, 2);

        let error = parse("#EXTM3U\nseg.ts\n", BASE).expect_err("URI без EXTINF");
        assert_eq!(syntax(&error).0, 2);

        let error =
            parse("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n", BASE).expect_err("вариант без URI");
        assert_eq!(syntax(&error).0, 2);

        let error = parse(
            "#EXTM3U\n#EXT-X-STREAM-INF:CODECS=\"avc1\nvariant.m3u8\n",
            BASE,
        )
        .expect_err("незакрытая кавычка");
        assert_eq!(syntax(&error).0, 2);

        let error = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\n#EXT-X-BYTERANGE:abc@0\nseg.ts\n",
            BASE,
        )
        .expect_err("битый byte-range");
        assert_eq!(syntax(&error).0, 4);

        let error = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xzz\n#EXTINF:4,\nseg.ts\n",
            BASE,
        )
        .expect_err("битый IV");
        assert_eq!(syntax(&error).0, 3);

        let error = parse("#EXTM3U\n#EXTINF:4,\n", BASE).expect_err("#EXTINF без URI");
        assert_eq!(syntax(&error).0, 2);

        let error = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:4,\nseg.ts\n",
            BASE,
        )
        .expect_err("#EXT-X-KEY без URI");
        assert_eq!(syntax(&error).0, 3);

        let error = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:BYTERANGE=\"10@0\"\n#EXTINF:4,\nseg.ts\n",
            BASE,
        )
        .expect_err("#EXT-X-MAP без URI");
        assert_eq!(syntax(&error).0, 3);

        let error = parse("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=abc\nv.m3u8\n", BASE)
            .expect_err("BANDWIDTH не число");
        assert_eq!(syntax(&error).0, 2);

        let error = parse("#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280\nv.m3u8\n", BASE)
            .expect_err("RESOLUTION без высоты");
        assert_eq!(syntax(&error).0, 2);

        let error = parse("#EXTM3U\n#EXTINF:1e300,\nseg.ts\n", BASE)
            .expect_err("длительность за пределами Duration");
        assert_eq!(syntax(&error).0, 2);

        let error = parse(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-BYTERANGE:18446744073709551615@1\n#EXTINF:4,\nseg.ts\n",
            BASE,
        )
        .expect_err("byte-range выходит за u64");
        assert_eq!(syntax(&error).0, 3);

        let error = parse(
            "#EXTM3U\n#EXTINF:1.8e19,\nseg1.ts\n#EXTINF:1.8e19,\nseg2.ts\n",
            BASE,
        )
        .expect_err("суммарная длительность выходит за Duration");
        assert_eq!(syntax(&error).0, 4);
    }

    #[test]
    fn mixed_playlists_are_rejected() {
        let text = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1
variant.m3u8
#EXTINF:4,
seg.ts
"#;

        let error = parse(text, BASE).expect_err("смешанный плейлист");
        let (line, reason) = syntax(&error);
        assert_eq!(line, 2);
        assert!(reason.contains("смешаны"), "{reason}");
    }

    #[test]
    fn playlist_without_variants_or_segments_is_rejected() {
        let error = parse("#EXTM3U\n#EXT-X-VERSION:3\n", BASE).expect_err("пустой плейлист");
        let (line, reason) = syntax(&error);
        assert_eq!(line, 1);
        assert!(reason.contains("нет ни вариантов"), "{reason}");
    }

    /// Ответ локального сервера по одному пути.
    #[derive(Clone)]
    struct Route {
        status: u16,
        body: Vec<u8>,
        ranges: bool,
        content_type: String,
        location: Option<String>,
    }

    /// Учёт запросов: путь и заголовок `Range`, если он был.
    type Hits = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// Заголовки каждого запроса: путь и пары «имя: значение».
    type SeenHeaders = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

    /// HTTP-сервер на `127.0.0.1:0` с изменяемыми маршрутами и учётом запросов.
    struct RouteServer {
        base: String,
        routes: Arc<Mutex<HashMap<String, Route>>>,
        hits: Hits,
        headers: SeenHeaders,
        stop: Arc<AtomicBool>,
    }

    impl RouteServer {
        fn start() -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").expect("тестовый сервер");
            let address = server.server_addr().to_ip().expect("ip-адрес");
            let base = format!("http://{address}");
            let routes = Arc::new(Mutex::new(HashMap::new()));
            let hits = Arc::new(Mutex::new(Vec::new()));
            let headers = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));

            {
                let routes = Arc::clone(&routes);
                let hits = Arc::clone(&hits);
                let headers = Arc::clone(&headers);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match server.recv_timeout(Duration::from_millis(20)) {
                            Ok(Some(request)) => respond(request, &routes, &hits, &headers),
                            Ok(None) => {}
                            Err(_) => break,
                        }
                    }
                });
            }

            Self {
                base,
                routes,
                hits,
                headers,
                stop,
            }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        fn set(&self, path: &str, body: impl Into<Vec<u8>>) {
            self.set_route(path, 200, body, false, "application/vnd.apple.mpegurl");
        }

        fn set_ranges(&self, path: &str, body: impl Into<Vec<u8>>) {
            self.set_route(path, 200, body, true, "video/mp2t");
        }

        fn set_status(&self, path: &str, status: u16) {
            self.set_route(path, status, Vec::new(), false, "text/plain");
        }

        fn set_redirect(&self, path: &str, location: &str) {
            self.routes.lock().unwrap().insert(
                path.to_owned(),
                Route {
                    status: 302,
                    body: Vec::new(),
                    ranges: false,
                    content_type: "text/plain".to_owned(),
                    location: Some(location.to_owned()),
                },
            );
        }

        fn set_html(&self, path: &str, body: impl Into<Vec<u8>>) {
            self.set_route(path, 200, body, false, "text/html; charset=utf-8");
        }

        fn set_route(
            &self,
            path: &str,
            status: u16,
            body: impl Into<Vec<u8>>,
            ranges: bool,
            content_type: &str,
        ) {
            self.routes.lock().unwrap().insert(
                path.to_owned(),
                Route {
                    status,
                    body: body.into(),
                    ranges,
                    content_type: content_type.to_owned(),
                    location: None,
                },
            );
        }

        fn hits(&self, path: &str) -> usize {
            self.hits
                .lock()
                .unwrap()
                .iter()
                .filter(|(hit, _)| hit == path)
                .count()
        }
    }

    impl Drop for RouteServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn respond(
        request: tiny_http::Request,
        routes: &Mutex<HashMap<String, Route>>,
        hits: &Hits,
        headers: &SeenHeaders,
    ) {
        let url = request.url().to_owned();
        let path = url.split(['?', '#']).next().unwrap_or_default().to_owned();
        let range = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Range"))
            .map(|header| header.value.as_str().to_owned());
        hits.lock().unwrap().push((path.clone(), range.clone()));
        let seen: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|header| {
                (
                    header.field.to_string().to_ascii_lowercase(),
                    header.value.as_str().to_owned(),
                )
            })
            .collect();
        headers.lock().unwrap().push((path.clone(), seen));

        let Some(route) = routes.lock().unwrap().get(&path).cloned() else {
            let _ = request.respond(Response::empty(StatusCode(404)));
            return;
        };

        let header = |name: &str, value: &str| {
            Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("заголовок")
        };

        if route.ranges
            && let Some((start, end)) = range.as_deref().and_then(parse_range)
            && start < route.body.len()
        {
            let end = end.min(route.body.len() - 1);
            let total = route.body.len();
            let response = Response::from_data(route.body[start..=end].to_vec())
                .with_status_code(StatusCode(206))
                .with_header(header("Content-Type", &route.content_type))
                .with_header(header(
                    "Content-Range",
                    &format!("bytes {start}-{end}/{total}"),
                ));
            let _ = request.respond(response);
            return;
        }

        let response = Response::from_data(route.body)
            .with_status_code(StatusCode(route.status))
            .with_header(header("Content-Type", &route.content_type));
        let response = match &route.location {
            Some(location) => response.with_header(header("Location", location)),
            None => response,
        };
        let _ = request.respond(response);
    }

    fn parse_range(value: &str) -> Option<(usize, usize)> {
        let value = value.strip_prefix("bytes=")?;
        let (start, end) = value.split_once('-')?;
        Some((start.parse().ok()?, end.parse().ok()?))
    }

    fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    fn open_hls(server: &RouteServer, path: &str) -> Result<Arc<HlsSource>, PlayerError> {
        HlsSource::open(
            &Source::Url(server.url(path)),
            HttpConfig::default(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn master_parses_audio_renditions_and_audio_attribute() {
        let text = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",LANGUAGE="en",NAME="English",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="a1/index.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="sub",NAME="English",URI="s1/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,CODECS="avc1,mp4a.40.2",AUDIO="aac"
low/index.m3u8
"#;

        let master = master(parse(text, BASE).expect("разобрать master"));

        assert_eq!(master.audio_renditions.len(), 1, "субтитры не сохраняются");
        let rendition = &master.audio_renditions[0];
        assert_eq!(rendition.group_id.as_deref(), Some("aac"));
        assert_eq!(rendition.language.as_deref(), Some("en"));
        assert_eq!(rendition.name.as_deref(), Some("English"));
        assert!(rendition.default);
        assert!(rendition.autoselect);
        assert_eq!(rendition.channels, Some(2));
        assert_eq!(
            rendition.uri.as_deref(),
            Some("https://cdn.example.com/live/stream/a1/index.m3u8")
        );
        assert_eq!(master.variants[0].audio_group.as_deref(), Some("aac"));
    }

    #[test]
    fn program_date_time_is_parsed_into_epoch_seconds() {
        let text = r#"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXT-X-PROGRAM-DATE-TIME:2026-11-23T10:31:52.865Z
#EXTINF:2.0,
seg0.ts
#EXTINF:2.0,
seg1.ts
"#;

        let media = media(parse(text, BASE).expect("разобрать media"));
        assert_eq!(media.segments[0].program_date_time, Some(1_795_429_912));
        assert_eq!(media.segments[1].program_date_time, None);

        let dates = start_dates(&media);
        assert_eq!(dates[0], Some(1_795_429_912));
        assert_eq!(dates[1], Some(1_795_429_914), "промежуток достраивается");
    }

    #[test]
    fn program_date_time_accepts_offsets_and_rejects_garbage() {
        assert_eq!(
            parse_program_date_time("1970-01-01T00:00:00Z", 1).expect("UTC"),
            0
        );
        assert_eq!(
            parse_program_date_time("1970-01-01T03:00:00+03:00", 1).expect("смещение"),
            0
        );
        assert_eq!(
            parse_program_date_time("1970-01-01T00:00:00-01:30", 1).expect("смещение"),
            5_400
        );
        assert!(parse_program_date_time("2026-11-23", 1).is_err());
        assert!(parse_program_date_time("2026-13-01T00:00:00Z", 1).is_err());
    }

    #[test]
    fn real_apple_master_selects_1080p_aac_and_two_channels() {
        let path = hls_test_root().join("real-master-fmp4.m3u8");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };

        let master = master(
            parse(
                &text,
                "https://devstreaming-cdn.apple.com/videos/master.m3u8",
            )
            .expect("разобрать Apple master"),
        );
        assert_eq!(master.audio_renditions.len(), 3, "только аудиогруппы");
        assert!(
            master
                .audio_renditions
                .iter()
                .all(|rendition| rendition.uri.is_some())
        );

        let selected = select_variant(&master).expect("выбрать вариант");
        assert!(
            selected.variant.uri.ends_with("v9/prog_index.m3u8"),
            "{}",
            selected.variant.uri
        );
        assert_eq!(selected.variant.resolution, Some((1920, 1080)));

        let audio = selected.audio.expect("аудиодорожка");
        assert_eq!(
            audio.group_id.as_deref(),
            Some("aud1"),
            "AAC предпочтительнее"
        );
        assert_eq!(audio.channels, Some(2));
    }

    #[test]
    fn variant_selection_deduplicates_uris_and_caps_resolution() {
        let text = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="ac3",CHANNELS="6",DEFAULT=YES,URI="a2/index.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",CHANNELS="2",DEFAULT=YES,URI="a1/index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080,CODECS="avc1,ac-3",AUDIO="ac3"
v/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=8200000,RESOLUTION=1920x1080,CODECS="avc1,mp4a.40.2",AUDIO="aac"
v/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=20000000,RESOLUTION=3840x2160,CODECS="avc1,mp4a.40.2",AUDIO="aac"
uhd/index.m3u8
"#;

        let master = master(parse(text, BASE).expect("разобрать master"));
        let selected = select_variant(&master).expect("выбрать вариант");

        assert!(
            selected.variant.uri.ends_with("v/index.m3u8"),
            "2160p отсечён"
        );
        assert_eq!(selected.variant.resolution, Some((1920, 1080)));
        assert_eq!(
            selected.audio.expect("аудио").group_id.as_deref(),
            Some("aac")
        );
    }

    #[test]
    fn missing_audio_group_is_an_error() {
        let text = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,AUDIO="nope"
v/index.m3u8
"#;

        let master = master(parse(text, BASE).expect("разобрать master"));
        let error = select_variant(&master).expect_err("группы нет");
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn muxed_vod_opens_and_reads_segments_once() {
        let server = RouteServer::start();
        server.set(
            "/master.m3u8",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\nv/index.m3u8\n",
        );
        server.set(
            "/v/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nseg0.ts\n#EXTINF:1.0,\nseg1.ts\n#EXT-X-ENDLIST\n",
        );
        server.set("/v/seg0.ts", "AAA");
        server.set("/v/seg1.ts", "BBB");

        let hls = open_hls(&server, "/master.m3u8").expect("открыть HLS");
        assert!(!hls.is_live());
        assert!(!hls.is_demuxed());
        assert_eq!(hls.duration(), Some(Duration::from_secs(2)));

        let mut video = hls.reader(Track::Video).expect("видеочитатель");
        let mut audio = hls.reader(Track::Audio).expect("аудиочитатель для muxed");
        let mut video_bytes = Vec::new();
        video.read_to_end(&mut video_bytes).expect("видео");
        let mut audio_bytes = Vec::new();
        audio.read_to_end(&mut audio_bytes).expect("звук");

        assert_eq!(video_bytes, b"AAABBB");
        assert_eq!(audio_bytes, b"AAABBB");
        assert_eq!(server.hits("/v/seg0.ts"), 1, "один запрос на сегмент");
        assert_eq!(server.hits("/v/seg1.ts"), 1);

        let stats = hls.network_stats();
        assert!(stats.requests >= 4, "{stats:?}");
        assert!(stats.bytes >= 6, "{stats:?}");
    }

    #[test]
    fn fmp4_init_section_is_prepended_once() {
        let server = RouteServer::start();
        server.set(
            "/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\nseg0.m4s\n#EXTINF:1.0,\nseg1.m4s\n#EXT-X-ENDLIST\n",
        );
        server.set("/init.mp4", "INIT");
        server.set("/seg0.m4s", "one");
        server.set("/seg1.m4s", "two");

        let hls = open_hls(&server, "/index.m3u8").expect("открыть HLS");
        let mut reader = hls.reader(Track::Video).expect("читатель");
        assert_eq!(reader.hint_name(), Some("stream.mp4"));

        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("прочитать");

        assert_eq!(bytes, b"INITonetwo");
        assert_eq!(server.hits("/init.mp4"), 1, "init доставлен один раз");
    }

    #[test]
    fn discontinuity_repeats_the_init_section() {
        let server = RouteServer::start();
        server.set(
            "/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.0,\nseg0.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:1.0,\nseg1.m4s\n#EXT-X-ENDLIST\n",
        );
        server.set("/init.mp4", "INIT");
        server.set("/seg0.m4s", "one");
        server.set("/seg1.m4s", "two");

        let hls = open_hls(&server, "/index.m3u8").expect("открыть HLS");
        let mut bytes = Vec::new();
        hls.reader(Track::Video)
            .expect("читатель")
            .read_to_end(&mut bytes)
            .expect("прочитать");

        assert_eq!(bytes, b"INIToneINITtwo", "init повторён после разрыва");
        assert_eq!(server.hits("/init.mp4"), 1, "но скачан один раз");
    }

    #[test]
    fn byte_ranges_are_fetched_with_range_requests() {
        let server = RouteServer::start();
        server.set(
            "/single.m3u8",
            "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\n#EXT-X-BYTERANGE:4@0\nall.ts\n#EXTINF:1.0,\n#EXT-X-BYTERANGE:6@4\nall.ts\n#EXT-X-ENDLIST\n",
        );
        server.set_ranges("/all.ts", "0123456789");

        let hls = open_hls(&server, "/single.m3u8").expect("открыть HLS");
        let mut reader = hls.reader(Track::Video).expect("читатель");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("прочитать");

        assert_eq!(bytes, b"0123456789");
        assert_eq!(server.hits("/all.ts"), 2, "по запросу на диапазон");
    }

    #[test]
    fn redirects_update_the_playlist_base() {
        let server = RouteServer::start();
        server.set_redirect("/start.m3u8", "/real/index.m3u8");
        server.set(
            "/real/index.m3u8",
            "#EXTM3U\n#EXTINF:1.0,\nseg0.ts\n#EXT-X-ENDLIST\n",
        );
        server.set("/real/seg0.ts", "OK");

        let hls = open_hls(&server, "/start.m3u8").expect("открыть через редирект");
        let mut bytes = Vec::new();
        hls.reader(Track::Video)
            .expect("читатель")
            .read_to_end(&mut bytes)
            .expect("прочитать");

        assert_eq!(bytes, b"OK", "ссылки считаются от финального URL");
        assert_eq!(server.hits("/real/seg0.ts"), 1);
    }

    #[test]
    fn playlist_404_is_an_http_status() {
        let server = RouteServer::start();
        server.set_status("/missing.m3u8", 404);

        let error = match open_hls(&server, "/missing.m3u8") {
            Err(error) => error,
            Ok(_) => panic!("404 должен дать ошибку"),
        };
        assert!(
            matches!(error, PlayerError::HttpStatus { status: 404, .. }),
            "{error}"
        );
    }

    #[test]
    fn html_instead_of_a_playlist_is_not_media() {
        let server = RouteServer::start();
        server.set_html("/index.m3u8", "<html><body>nope</body></html>");

        let error = match open_hls(&server, "/index.m3u8") {
            Err(error) => error,
            Ok(_) => panic!("html не должен открываться"),
        };
        assert!(matches!(error, PlayerError::NotMedia { .. }), "{error}");
    }

    #[test]
    fn broken_playlist_names_the_line() {
        let server = RouteServer::start();
        server.set("/index.m3u8", "#EXTM3U\n#EXTINF:oops\nseg.ts\n");

        let error = match open_hls(&server, "/index.m3u8") {
            Err(error) => error,
            Ok(_) => panic!("битый плейлист не должен открываться"),
        };
        let PlayerError::Hls { reason, .. } = &error else {
            panic!("ожидалась ошибка Hls, получено {error}");
        };
        assert!(reason.contains("строка 2"), "{reason}");
    }

    #[test]
    fn encrypted_playlist_is_unsupported() {
        let server = RouteServer::start();
        server.set(
            "/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:1.0,\nseg.ts\n#EXT-X-ENDLIST\n",
        );

        let error = match open_hls(&server, "/index.m3u8") {
            Err(error) => error,
            Ok(_) => panic!("шифрование пока не поддерживается"),
        };
        assert!(
            matches!(error, PlayerError::UnsupportedHls { .. }),
            "{error}"
        );
    }

    #[test]
    fn demuxed_master_reads_video_and_audio_separately() {
        let server = RouteServer::start();
        server.set(
            "/master.m3u8",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",DEFAULT=YES,CHANNELS=\"2\",URI=\"a/index.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,CODECS=\"avc1,mp4a\",AUDIO=\"aac\"\nv/index.m3u8\n",
        );
        server.set(
            "/v/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\nv0.ts\n#EXT-X-ENDLIST\n",
        );
        server.set(
            "/a/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1.0,\na0.ts\n#EXT-X-ENDLIST\n",
        );
        server.set("/v/v0.ts", "VIDEO");
        server.set("/a/a0.ts", "AUDIO");

        let hls = open_hls(&server, "/master.m3u8").expect("открыть HLS");
        assert!(hls.is_demuxed());
        assert_eq!(hls.duration(), Some(Duration::from_secs(1)));

        let mut video = Vec::new();
        hls.reader(Track::Video)
            .expect("видео")
            .read_to_end(&mut video)
            .expect("прочитать видео");
        let mut audio = Vec::new();
        hls.reader(Track::Audio)
            .expect("звук")
            .read_to_end(&mut audio)
            .expect("прочитать звук");

        assert_eq!(video, b"VIDEO");
        assert_eq!(audio, b"AUDIO");
        assert_eq!(server.hits("/v/v0.ts"), 1);
        assert_eq!(server.hits("/a/a0.ts"), 1);
    }

    #[test]
    fn live_playlist_starts_near_the_edge() {
        let server = RouteServer::start();
        server.set(
            "/live.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:2.0,\ns0.ts\n#EXTINF:2.0,\ns1.ts\n#EXTINF:2.0,\ns2.ts\n#EXTINF:2.0,\ns3.ts\n#EXTINF:2.0,\ns4.ts\n",
        );
        for name in ["s0", "s1", "s2", "s3", "s4"] {
            server.set(&format!("/{name}.ts"), name);
        }

        let hls = open_hls(&server, "/live.m3u8").expect("открыть HLS");
        assert!(hls.is_live());
        assert_eq!(hls.duration(), None);
        assert!(
            hls.seek_time(Duration::from_secs(1)).is_err(),
            "live не перематывается и на уровне источника"
        );

        let mut reader = hls.reader(Track::Video).expect("читатель");
        assert!(!reader.is_seekable());
        assert!(
            reader.seek_time(Duration::from_secs(1)).is_err(),
            "live не перематывается"
        );

        let mut first = [0_u8; 2];
        reader.read_exact(&mut first).expect("первый сегмент");
        assert_eq!(&first, b"s3", "старт с хвоста окна");
        assert_eq!(server.hits("/s0.ts"), 0);
    }

    #[test]
    fn live_refresh_skips_segments_gone_from_the_window() {
        let server = RouteServer::start();
        let live = |base: u64, segments: &[&str]| {
            let mut text =
                format!("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:{base}\n");
            for name in segments {
                text.push_str(&format!("#EXTINF:2.0,\n{name}.ts\n"));
            }
            text
        };
        server.set("/live.m3u8", live(0, &["s0", "s1"]));
        for name in ["s0", "s1", "s3", "s4"] {
            server.set(&format!("/{name}.ts"), name);
        }

        let hls = open_hls(&server, "/live.m3u8").expect("открыть HLS");
        let mut reader = hls.reader(Track::Video).expect("читатель");
        let mut bytes = [0_u8; 2];
        reader.read_exact(&mut bytes).expect("s0");
        reader.read_exact(&mut bytes).expect("s1");

        server.set("/live.m3u8", live(3, &["s3", "s4"]));
        hls.video.refresh_playlist().expect("обновить плейлист");

        reader.read_exact(&mut bytes).expect("s3");
        assert_eq!(&bytes, b"s3", "пропущенные сегменты пропускаются");
        assert_eq!(server.hits("/s3.ts"), 1);
    }

    #[test]
    fn cancel_stops_a_live_wait() {
        let server = RouteServer::start();
        server.set(
            "/live.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\ns0.ts\n",
        );
        server.set("/s0.ts", "s0");

        let cancel = Arc::new(AtomicBool::new(false));
        let source = Source::Url(server.url("/live.m3u8"));
        let hls = HlsSource::open(&source, HttpConfig::default(), Arc::clone(&cancel))
            .expect("открыть HLS");
        let mut reader = hls.reader(Track::Video).expect("читатель");

        let mut bytes = [0_u8; 2];
        reader.read_exact(&mut bytes).expect("s0");

        cancel.store(true, Ordering::Relaxed);
        let mut byte = [0_u8; 1];
        let error = reader.read(&mut byte).expect_err("ожидание отменено");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn seek_time_moves_to_the_target_segment() {
        let server = RouteServer::start();
        server.set(
            "/vod.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5.0,\ns0.ts\n#EXTINF:5.0,\ns1.ts\n#EXTINF:5.0,\ns2.ts\n#EXTINF:5.0,\ns3.ts\n#EXT-X-ENDLIST\n",
        );
        for name in ["s0", "s1", "s2", "s3"] {
            server.set(&format!("/{name}.ts"), name);
        }

        let hls = open_hls(&server, "/vod.m3u8").expect("открыть HLS");
        let mut reader = hls.reader(Track::Video).expect("читатель");

        let mut bytes = [0_u8; 2];
        reader.read_exact(&mut bytes).expect("начало");
        assert_eq!(&bytes, b"s0");

        reader
            .seek_time(Duration::from_secs(11))
            .expect("перемотать на 11 с");
        reader
            .read_exact(&mut bytes)
            .expect("сегмент после перемотки");
        assert_eq!(&bytes, b"s2", "11 с лежит в третьем сегменте");

        reader
            .seek(SeekFrom::Start(0))
            .expect("FFmpeg возвращается в начало виртуального потока");
        reader
            .read_exact(&mut bytes)
            .expect("после SeekFrom::Start(0)");
        assert_eq!(&bytes, b"s2", "начало — целевой сегмент перемотки");
    }

    #[test]
    fn demuxed_live_start_is_aligned_by_program_date_time() {
        let server = RouteServer::start();
        server.set(
            "/master.m3u8",
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",DEFAULT=YES,CHANNELS=\"2\",URI=\"a/index.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,CODECS=\"avc1,mp4a\",AUDIO=\"aac\"\nv/index.m3u8\n",
        );
        server.set(
            "/v/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-PROGRAM-DATE-TIME:2026-11-23T11:59:58Z\n#EXTINF:2.0,\nv0.ts\n#EXTINF:2.0,\nv1.ts\n#EXTINF:2.0,\nv2.ts\n#EXTINF:2.0,\nv3.ts\n",
        );
        server.set(
            "/a/index.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:20\n#EXT-X-PROGRAM-DATE-TIME:2026-11-23T12:00:00Z\n#EXTINF:2.0,\na0.ts\n#EXTINF:2.0,\na1.ts\n#EXTINF:2.0,\na2.ts\n",
        );

        let hls = open_hls(&server, "/master.m3u8").expect("открыть HLS");
        assert!(hls.is_live());
        assert!(hls.is_demuxed());

        let audio = hls.audio.as_ref().expect("аудиодорожка");
        assert_eq!(hls.video.start_sequence(), 12, "видео: сегмент с 12:00:02");
        assert_eq!(audio.start_sequence(), 21, "звук: сегмент с 12:00:02");
        assert_eq!(
            hls.video_pts_shift_ms(),
            Some(0),
            "после выравнивания PTS дорожек совпадают"
        );
    }

    #[test]
    fn source_seek_time_restarts_readers_at_the_target_segment() {
        let server = RouteServer::start();
        server.set(
            "/vod.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5.0,\ns0.ts\n#EXTINF:5.0,\ns1.ts\n#EXTINF:5.0,\ns2.ts\n#EXTINF:5.0,\ns3.ts\n#EXT-X-ENDLIST\n",
        );
        for name in ["s0", "s1", "s2", "s3"] {
            server.set(&format!("/{name}.ts"), name);
        }

        let hls = open_hls(&server, "/vod.m3u8").expect("открыть HLS");
        hls.seek_time(Duration::from_secs(11))
            .expect("перемотать на 11 с");
        assert_eq!(
            hls.video.start_sequence(),
            2,
            "11 с лежит в третьем сегменте"
        );

        let mut reader = hls.reader(Track::Video).expect("новый читатель");
        let mut bytes = [0_u8; 2];
        reader
            .read_exact(&mut bytes)
            .expect("сегмент после перемотки");
        assert_eq!(&bytes, b"s2");
        assert_eq!(server.hits("/s0.ts"), 0, "начало VOD не скачивалось");
    }

    #[test]
    fn hls_url_is_not_rejected_before_the_network() {
        let server = RouteServer::start();
        server.set_status("/index.m3u8", 404);
        let source = Source::Url(server.url("/index.m3u8"));

        let error =
            match crate::media::Media::open_with(&source, Arc::new(AtomicBool::new(false)), &|| {
                false
            }) {
                Err(error) => error,
                Ok(_) => panic!("404 должен дать ошибку"),
            };
        assert!(
            matches!(error, PlayerError::HttpStatus { status: 404, .. }),
            "URL не должен отсекаться как UnsupportedHls: {error}"
        );
    }

    /// Кладёт все файлы сгенерированного HLS в маршруты локального сервера.
    fn serve_directory(server: &RouteServer, directory: &Path) {
        for entry in std::fs::read_dir(directory).expect("каталог HLS") {
            let entry = entry.expect("файл HLS");
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(entry.path()).expect("прочитать файл HLS");
            server.set(&format!("/{name}"), bytes);
        }
    }

    #[test]
    fn media_opens_hls_vod_and_seeks_by_time() {
        let Some(directory) = generate_hls(&[]) else {
            eprintln!("ffmpeg недоступен — пропускаем сквозной HLS-тест");
            return;
        };
        let server = RouteServer::start();
        serve_directory(&server, &directory);

        let source = Source::Url(server.url("/index.m3u8"));
        let mut media =
            crate::media::Media::open_with(&source, Arc::new(AtomicBool::new(false)), &|| false)
                .expect("открыть HLS в Media");

        let ready = media.ready(&source);
        assert!(!ready.live, "у VOD нет признака live");
        assert!(ready.seekable, "VOD перематывается");
        let duration = ready.duration.expect("длительность из плейлиста");
        assert!(duration >= Duration::from_millis(1500), "{duration:?}");

        let video = media.video.as_mut().expect("видеодорожка открыта");
        assert!(video.info().width > 0);
        video
            .next_frame()
            .expect("первый кадр")
            .expect("кадр до перемотки");

        let target = duration / 2;
        media.seek(target).expect("перемотка VOD по времени");
        let frame = media
            .video
            .as_mut()
            .expect("видео после перемотки")
            .next_frame()
            .expect("кадр после перемотки")
            .expect("поток не закончился");
        if let Some(pts) = frame.pts {
            assert!(
                pts.saturating_add(Duration::from_millis(50)) >= target,
                "первый кадр после перемотки должен быть у цели, получено {pts:?}"
            );
        }

        assert!(media.network_stats().is_some(), "счётчики HLS доступны");
    }

    #[test]
    fn video_only_hls_plays_without_an_audio_track() {
        let Some(directory) = generate_hls(&["-an"]) else {
            eprintln!("ffmpeg недоступен — пропускаем HLS без звука");
            return;
        };
        let server = RouteServer::start();
        serve_directory(&server, &directory);

        let source = Source::Url(server.url("/index.m3u8"));
        let mut media =
            crate::media::Media::open_with(&source, Arc::new(AtomicBool::new(false)), &|| false)
                .expect("видео без звука должно открываться");

        let ready = media.ready(&source);
        assert!(ready.media_info.audio.is_none(), "звука нет в метаданных");
        assert!(media.audio.is_none(), "аудиодекодер не открыт");

        let frame = media
            .video
            .as_mut()
            .expect("видеодорожка")
            .next_frame()
            .expect("кадр")
            .expect("первый кадр без звука");
        assert!(frame.width > 0);
    }

    #[test]
    fn playlist_map_and_segments_send_source_headers() {
        let server = RouteServer::start();
        server.set(
            "/master.m3u8",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000\nmedia.m3u8\n",
        );
        server.set(
            "/media.m3u8",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nseg1.ts\n#EXT-X-ENDLIST\n",
        );
        server.set("/init.mp4", "init");
        server.set("/seg1.ts", "segment");

        let options = HttpOptions::new()
            .with_referer("https://rutube.ru/")
            .expect("Referer")
            .with_user_agent("Mozilla/5.0")
            .expect("User-Agent");
        let source = Source::Url(server.url("/master.m3u8"));
        let hls = HlsSource::open(
            &source,
            HttpConfig::with_http(&options),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("открыть HLS");

        let mut reader = hls.reader(Track::Video).expect("читатель");
        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("прочитать ресурсы");
        assert!(!body.is_empty(), "init и сегмент должны прийти");

        let seen = server.headers.lock().unwrap();
        for path in ["/master.m3u8", "/media.m3u8", "/init.mp4", "/seg1.ts"] {
            let requests: Vec<_> = seen
                .iter()
                .filter(|(hit, _)| hit == path)
                .map(|(_, headers)| headers)
                .collect();
            assert!(!requests.is_empty(), "нет запроса {path}");
            for headers in requests {
                assert_eq!(
                    header_of(headers, "referer"),
                    Some("https://rutube.ru/"),
                    "{path}"
                );
                assert_eq!(
                    header_of(headers, "user-agent"),
                    Some("Mozilla/5.0"),
                    "{path}"
                );
            }
        }
    }
}
