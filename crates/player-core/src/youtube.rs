//! YouTube VOD: распознавание ссылок и получение HLS-манифеста.
//!
//! Резолвер не знает про `Media`: он проверяет, что ссылка ведёт на видео,
//! и возвращает [`ResolvedSource::Hls`] с `hlsManifestUrl`. Дальше работает
//! обычный [`crate::hls::HlsSource`]. Adaptive-URL Apple-клиентов не
//! используются: без PO-токена они обрезаются примерно после 1 МБ.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tracing::debug;

use crate::http::{HttpConfig, agent};
use crate::http_options::HttpOptions;
use crate::{PlayerError, Source, YoutubeErrorKind};

const DEFAULT_PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player";
const DEFAULT_VISITOR_URL: &str = "https://www.youtube.com/youtubei/v1/visitor_id";
const WEB_VERSION: &str = "2.20251120.01.00";
/// Браузерный UA для `visitor_id` и HLS-запросов YouTube.
pub(crate) const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

const CACHE_TTL: Duration = Duration::from_secs(4 * 3600);
const VISITOR_TTL: Duration = Duration::from_secs(6 * 3600);
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const CACHE_LIMIT: usize = 64;
const IOS_ATTEMPTS: u8 = 5;

const YOUTUBE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtube-nocookie.com",
    "www.youtube-nocookie.com",
];

/// Один клиент Innertube, который умеет отдавать HLS-манифест.
#[derive(Debug, Clone, Copy)]
struct ClientProfile {
    label: &'static str,
    client_name: &'static str,
    client_id: &'static str,
    version: &'static str,
    user_agent: &'static str,
    device_make: &'static str,
    device_model: &'static str,
    os_name: &'static str,
    os_version: &'static str,
}

const VISIONOS: ClientProfile = ClientProfile {
    label: "visionOS",
    client_name: "VISIONOS",
    client_id: "101",
    version: "1.02",
    user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
    device_make: "Apple",
    device_model: "RealityDevice17,1",
    os_name: "visionOS",
    os_version: "26.5.23O471",
};

const IOS: ClientProfile = ClientProfile {
    label: "iOS",
    client_name: "IOS",
    client_id: "5",
    version: "20.10.4",
    user_agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
    device_make: "Apple",
    device_model: "iPhone16,2",
    os_name: "iPhone",
    os_version: "18.3.2.22D82",
};

/// Разрешённый источник для [`crate::Media`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedSource {
    /// HLS master/media, который открывает существующий `HlsSource`.
    Hls {
        /// Разрешённый HLS-URL.
        source: Source,
        /// Название ролика из `videoDetails.title`, если оно есть.
        title: Option<String>,
    },
    // Задел на будущее: Adaptive { video: Source, audio: Source }.
}

/// Адреса Innertube, подменённые интеграционным тестом.
///
/// В рабочем режиме не задано: резолвер идёт на production-адреса.
static TEST_ENDPOINTS: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Подменяет адреса `player`/`visitor_id` для сквозных тестов.
///
/// Существует только ради интеграционных тестов: `Player` работает в
/// отдельном потоке, поэтому слот должен быть глобальным. В обычной работе
/// никто его не заполняет.
#[doc(hidden)]
pub fn set_endpoints_for_tests(player_url: impl Into<String>, visitor_url: impl Into<String>) {
    *lock(&TEST_ENDPOINTS) = Some((player_url.into(), visitor_url.into()));
}

fn test_endpoints() -> Option<(String, String)> {
    lock(&TEST_ENDPOINTS).clone()
}

/// Настройки YouTube-резолвера. Поля публичные внутри крейта, чтобы тесты
/// подставляли локальный mock-сервер вместо production-адресов.
#[derive(Debug, Clone)]
pub(crate) struct YoutubeOptions {
    pub(crate) player_url: String,
    pub(crate) visitor_url: String,
    pub(crate) language: String,
    pub(crate) region: String,
    pub(crate) max_response_bytes: u64,
}

impl Default for YoutubeOptions {
    fn default() -> Self {
        Self {
            player_url: DEFAULT_PLAYER_URL.to_owned(),
            visitor_url: DEFAULT_VISITOR_URL.to_owned(),
            language: "en".to_owned(),
            region: "US".to_owned(),
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }
}

/// Возвращает `video_id`, если ссылка ведёт на YouTube-видео.
pub(crate) fn video_id(source: &Source) -> Option<&str> {
    let Source::Url(url) = source else {
        return None;
    };
    video_id_from_url(url)
}

/// Похожа ли ссылка на YouTube, включая плейлисты и каналы.
///
/// Нужно, чтобы `Media` отличил YouTube от обычного URL и вернул понятную
/// ошибку `NotAVideo`, а не пытался читать HTML как медиа.
pub(crate) fn is_youtube(source: &Source) -> bool {
    let Source::Url(url) = source else {
        return false;
    };
    let Some((authority, _)) = split_url(url) else {
        return false;
    };
    authority_host(authority).is_some_and(is_youtube_host)
}

/// Резолвит YouTube-ссылку в HLS-манифест.
pub(crate) fn resolve(
    source: &Source,
    http: &HttpOptions,
    cancel: &Arc<AtomicBool>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ResolvedSource, PlayerError> {
    let options = test_endpoints()
        .map(|(player_url, visitor_url)| YoutubeOptions {
            player_url,
            visitor_url,
            ..YoutubeOptions::default()
        })
        .unwrap_or_default();
    resolve_with_options(source, &options, http, cancel, cancelled)
}

/// То же, но с явными настройками: нужно тестам для mock-сервера.
pub(crate) fn resolve_with_options(
    source: &Source,
    options: &YoutubeOptions,
    http: &HttpOptions,
    cancel: &Arc<AtomicBool>,
    cancelled: &dyn Fn() -> bool,
) -> Result<ResolvedSource, PlayerError> {
    let Some(id) = video_id(source) else {
        return Err(youtube_error(source, YoutubeErrorKind::NotAVideo));
    };
    if let Some((url, title)) = cache_get(id) {
        debug!(video_id = id, "YouTube: манифест из кэша");
        return Ok(ResolvedSource::Hls {
            source: Source::Url(url),
            title,
        });
    }

    let config = HttpConfig::with_http(http);
    let agent = agent(&config);
    let resolver = Resolver {
        options,
        agent: &agent,
        target: source,
        cancel,
        cancelled,
    };
    let (url, title) = resolver.resolve(id)?;
    cache_put(id, url.clone(), title.clone());
    Ok(ResolvedSource::Hls {
        source: Source::Url(url),
        title,
    })
}

struct Resolver<'a> {
    options: &'a YoutubeOptions,
    agent: &'a ureq::Agent,
    target: &'a Source,
    cancel: &'a Arc<AtomicBool>,
    cancelled: &'a dyn Fn() -> bool,
}

struct ClientOutcome {
    hls: String,
    visitor: Option<String>,
    title: Option<String>,
}

impl Resolver<'_> {
    /// visionOS, затем iOS с несколькими `visitorData`.
    fn resolve(&self, id: &str) -> Result<(String, Option<String>), PlayerError> {
        self.check_cancelled()?;
        let visitor = self.visitor_id();
        let vision = match self.try_client(&VISIONOS, id, visitor.as_deref().unwrap_or("")) {
            Ok(outcome) => return Ok((outcome.hls, outcome.title)),
            Err(PlayerError::Cancelled) => return Err(PlayerError::Cancelled),
            Err(error) => error,
        };
        debug!(error = %vision, "visionOS не дал HLS, пробуем iOS");

        let mut last = vision;
        let kept = ios_visitor();
        for attempt in 0..IOS_ATTEMPTS {
            let visitor = if attempt % 2 == 0 { kept.clone() } else { None };
            let visitor = visitor.as_deref().unwrap_or("");
            match self.try_client(&IOS, id, visitor) {
                Ok(outcome) => {
                    if visitor.is_empty()
                        && let Some(found) = outcome.visitor.as_deref()
                    {
                        store_ios_visitor(found);
                    }
                    return Ok((outcome.hls, outcome.title));
                }
                Err(error) => {
                    if matches!(
                        &error,
                        PlayerError::Youtube {
                            kind: YoutubeErrorKind::NoHlsManifest,
                            ..
                        }
                    ) {
                        last = error;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        Err(last)
    }

    fn try_client(
        &self,
        profile: &ClientProfile,
        id: &str,
        visitor: &str,
    ) -> Result<ClientOutcome, PlayerError> {
        self.check_cancelled()?;
        debug!(
            client = profile.label,
            video_id = id,
            "YouTube: запрос /player"
        );
        let body = player_body(profile, id, visitor, self.options);
        let mut headers = vec![
            ("User-Agent", profile.user_agent),
            ("X-YouTube-Client-Name", profile.client_id),
            ("X-YouTube-Client-Version", profile.version),
            ("Origin", "https://www.youtube.com"),
        ];
        if !visitor.is_empty() {
            headers.push(("X-Goog-Visitor-Id", visitor));
        }

        let text = post_json(
            self.agent,
            &self.options.player_url,
            &body,
            &headers,
            self.options.max_response_bytes,
        )
        .map_err(|error| post_error(error, self.target))?;
        self.check_cancelled()?;

        let response: PlayerResponse = serde_json::from_str(&text).map_err(|error| {
            youtube_error(
                self.target,
                YoutubeErrorKind::Api(format!("разбор ответа: {error}")),
            )
        })?;

        if let Some(status) = response.playability_status.as_ref()
            && let Some(kind) = playability_kind(status)
        {
            return Err(youtube_error(self.target, kind));
        }
        if response
            .video_details
            .as_ref()
            .and_then(|details| details.is_live)
            .unwrap_or(false)
        {
            return Err(youtube_error(
                self.target,
                YoutubeErrorKind::LiveUnsupported,
            ));
        }

        let title = response
            .video_details
            .as_ref()
            .and_then(|details| details.title.clone());
        let visitor = response
            .response_context
            .and_then(|context| context.visitor_data);
        let Some(hls) = response
            .streaming_data
            .and_then(|streaming| streaming.hls_manifest_url)
        else {
            return Err(youtube_error(self.target, YoutubeErrorKind::NoHlsManifest));
        };
        Ok(ClientOutcome {
            hls,
            visitor,
            title,
        })
    }

    fn visitor_id(&self) -> Option<String> {
        if let Some(value) = cached_visitor() {
            return Some(value);
        }
        self.check_cancelled().ok()?;
        let body = serde_json::json!({
            "context": { "client": {
                "clientName": "WEB",
                "clientVersion": WEB_VERSION,
                "hl": self.options.language,
                "gl": self.options.region,
            }}
        });
        let text = post_json(
            self.agent,
            &self.options.visitor_url,
            &body,
            &[("User-Agent", BROWSER_UA)],
            self.options.max_response_bytes,
        )
        .ok()?;
        let response: VisitorResponse = serde_json::from_str(&text).ok()?;
        let value = response.response_context?.visitor_data?;
        store_visitor(value.clone());
        Some(value)
    }

    fn check_cancelled(&self) -> Result<(), PlayerError> {
        if (self.cancelled)() || self.cancel.load(Ordering::Relaxed) {
            return Err(PlayerError::Cancelled);
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct PlayerResponse {
    #[serde(rename = "playabilityStatus")]
    playability_status: Option<PlayabilityStatus>,
    #[serde(rename = "streamingData")]
    streaming_data: Option<StreamingData>,
    #[serde(rename = "responseContext")]
    response_context: Option<ResponseContext>,
    #[serde(rename = "videoDetails")]
    video_details: Option<VideoDetails>,
}

#[derive(Debug, Deserialize)]
struct PlayabilityStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamingData {
    #[serde(rename = "hlsManifestUrl")]
    hls_manifest_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseContext {
    #[serde(rename = "visitorData")]
    visitor_data: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct VideoDetails {
    #[serde(rename = "isLive")]
    is_live: Option<bool>,
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "lengthSeconds")]
    length_seconds: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VisitorResponse {
    #[serde(rename = "responseContext")]
    response_context: Option<ResponseContext>,
}

fn player_body(
    profile: &ClientProfile,
    id: &str,
    visitor: &str,
    options: &YoutubeOptions,
) -> serde_json::Value {
    let mut client = serde_json::json!({
        "clientName": profile.client_name,
        "clientVersion": profile.version,
        "deviceMake": profile.device_make,
        "deviceModel": profile.device_model,
        "osName": profile.os_name,
        "osVersion": profile.os_version,
        "userAgent": profile.user_agent,
        "hl": options.language,
        "gl": options.region,
    });
    if !visitor.is_empty() {
        client["visitorData"] = serde_json::Value::String(visitor.to_owned());
    }
    serde_json::json!({
        "videoId": id,
        "contentCheckOk": true,
        "racyCheckOk": true,
        "context": { "client": client }
    })
}

fn playability_kind(status: &PlayabilityStatus) -> Option<YoutubeErrorKind> {
    match status.status.as_str() {
        "OK" => None,
        "LOGIN_REQUIRED" => {
            let reason = status.reason.as_deref().unwrap_or("").to_ascii_lowercase();
            if reason.contains("age") || reason.contains("возраст") {
                Some(YoutubeErrorKind::AgeRestricted)
            } else {
                Some(YoutubeErrorKind::LoginRequired)
            }
        }
        "AGE_CHECK_REQUIRED" | "AGE_VERIFICATION_REQUIRED" => Some(YoutubeErrorKind::AgeRestricted),
        "UNPLAYABLE" => Some(YoutubeErrorKind::Unavailable),
        other => Some(YoutubeErrorKind::Api(format!("playabilityStatus: {other}"))),
    }
}

enum PostError {
    Network(String),
    Status(u16),
    TooLarge,
    Json(String),
}

fn post_json(
    agent: &ureq::Agent,
    url: &str,
    body: &serde_json::Value,
    headers: &[(&str, &str)],
    max_bytes: u64,
) -> Result<String, PostError> {
    let body = serde_json::to_vec(body).map_err(|error| PostError::Json(error.to_string()))?;
    let mut request = agent.post(url).header("Content-Type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send(body)
        .map_err(|error| PostError::Network(error.to_string()))?;
    let status = response.status().as_u16();
    let mut reader = response
        .into_body()
        .into_with_config()
        .limit(max_bytes.saturating_add(1))
        .reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| PostError::Network(error.to_string()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(PostError::TooLarge);
    }
    if !(200..300).contains(&status) {
        return Err(PostError::Status(status));
    }
    String::from_utf8(bytes).map_err(|error| PostError::Json(format!("ответ не UTF-8: {error}")))
}

fn post_error(error: PostError, target: &Source) -> PlayerError {
    let kind = match error {
        PostError::Network(reason) => YoutubeErrorKind::Api(format!("сеть: {reason}")),
        PostError::Status(status) => YoutubeErrorKind::Api(format!("HTTP {status}")),
        PostError::TooLarge => YoutubeErrorKind::Api("ответ больше 8 МиБ".to_owned()),
        PostError::Json(reason) => YoutubeErrorKind::Api(reason),
    };
    youtube_error(target, kind)
}

fn youtube_error(target: &Source, kind: YoutubeErrorKind) -> PlayerError {
    PlayerError::Youtube {
        target: target.clone(),
        kind,
    }
}

fn video_id_from_url(url: &str) -> Option<&str> {
    let (authority, after) = split_url(url)?;
    let host = authority_host(authority)?;
    let (path, query) = split_path_query(after);

    if host.eq_ignore_ascii_case("youtu.be") {
        let id = path.trim_start_matches('/').split('/').next()?;
        return valid_id(id);
    }
    if !is_youtube_host(host) {
        return None;
    }
    if path == "/watch" || path == "/watch/" {
        return query_param(query, "v").and_then(valid_id);
    }
    for prefix in ["/shorts/", "/live/", "/embed/", "/v/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = rest.split('/').next()?;
            return valid_id(id);
        }
    }
    None
}

fn split_url(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((&rest[..end], &rest[end..]))
}

fn authority_host(authority: &str) -> Option<&str> {
    let host = authority.rsplit('@').next()?;
    let host = match host.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => host,
    };
    let host = host.trim_end_matches('.');
    (!host.is_empty()).then_some(host)
}

fn split_path_query(after: &str) -> (&str, Option<&str>) {
    let before_fragment = after.split('#').next().unwrap_or("");
    match before_fragment.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (before_fragment, None),
    }
}

fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn valid_id(id: &str) -> Option<&str> {
    (id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
    .then_some(id)
}

fn is_youtube_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("youtu.be")
        || YOUTUBE_HOSTS
            .iter()
            .any(|known| host.eq_ignore_ascii_case(known))
}

struct CacheEntry {
    url: String,
    title: Option<String>,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(id: &str) -> Option<(String, Option<String>)> {
    let mut cache = lock(cache());
    if let Some(entry) = cache.get(id)
        && entry.at.elapsed() < CACHE_TTL
    {
        return Some((entry.url.clone(), entry.title.clone()));
    }
    cache.remove(id);
    None
}

fn cache_put(id: &str, url: String, title: Option<String>) {
    let mut cache = lock(cache());
    if cache.len() >= CACHE_LIMIT {
        cache.retain(|_, entry| entry.at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
    }
    cache.insert(
        id.to_owned(),
        CacheEntry {
            url,
            title,
            at: Instant::now(),
        },
    );
}

fn visitor_cache() -> &'static Mutex<Option<(String, Instant)>> {
    static VISITOR: OnceLock<Mutex<Option<(String, Instant)>>> = OnceLock::new();
    VISITOR.get_or_init(|| Mutex::new(None))
}

fn cached_visitor() -> Option<String> {
    let mut cache = lock(visitor_cache());
    if let Some((value, at)) = cache.as_ref()
        && at.elapsed() < VISITOR_TTL
    {
        return Some(value.clone());
    }
    *cache = None;
    None
}

fn store_visitor(value: String) {
    *lock(visitor_cache()) = Some((value, Instant::now()));
}

fn ios_visitor_cache() -> &'static Mutex<Option<String>> {
    static VISITOR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    VISITOR.get_or_init(|| Mutex::new(None))
}

fn ios_visitor() -> Option<String> {
    lock(ios_visitor_cache()).clone()
}

fn store_ios_visitor(value: &str) {
    *lock(ios_visitor_cache()) = Some(value.to_owned());
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(value: &str) -> Source {
        Source::Url(value.to_owned())
    }

    #[test]
    fn recognizes_video_urls() {
        for value in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&t=42",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=42",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/v/dQw4w9WgXcQ",
            "http://WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com:443/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com./watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(&url(value)), Some("dQw4w9WgXcQ"), "{value}");
        }
    }

    #[test]
    fn rejects_non_video_urls() {
        for value in [
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/channel/UC123",
            "https://www.youtube.com/@channel",
            "https://www.youtube.com/results?search_query=x",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=too-long-video-id",
            "https://www.youtube.com/watch?v=dQw4w9WgXc!",
            "https://www.youtube.com/watch?t=42",
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "file:///C:/video.mp4",
            "https://youtu.be/",
            "ftp://youtu.be/dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(&url(value)), None, "{value}");
        }
        assert_eq!(video_id(&Source::File("video.mp4".into())), None);
    }

    #[test]
    fn detects_youtube_hosts() {
        assert!(is_youtube(&url("https://www.youtube.com/playlist?list=x")));
        assert!(is_youtube(&url("https://youtu.be/dQw4w9WgXcQ")));
        assert!(!is_youtube(&url("https://example.com/watch?v=dQw4w9WgXcQ")));
        assert!(!is_youtube(&Source::File("video.mp4".into())));
    }

    #[test]
    fn parses_successful_player_response() {
        let text = r#"{
            "playabilityStatus": {"status": "OK"},
            "streamingData": {"hlsManifestUrl": "https://example.com/index.m3u8"},
            "responseContext": {"visitorData": "visitor"},
            "videoDetails": {"isLive": false, "title": "Test", "lengthSeconds": "42"}
        }"#;
        let response: PlayerResponse = serde_json::from_str(text).expect("json");

        assert_eq!(
            response
                .playability_status
                .as_ref()
                .map(|s| s.status.as_str()),
            Some("OK")
        );
        assert_eq!(
            response
                .streaming_data
                .and_then(|s| s.hls_manifest_url)
                .as_deref(),
            Some("https://example.com/index.m3u8")
        );
        assert_eq!(
            response
                .response_context
                .and_then(|c| c.visitor_data)
                .as_deref(),
            Some("visitor")
        );
    }

    #[test]
    fn maps_playability_statuses() {
        assert_eq!(
            playability_kind(&PlayabilityStatus {
                status: "OK".to_owned(),
                reason: None,
            }),
            None
        );
        assert_eq!(
            playability_kind(&PlayabilityStatus {
                status: "LOGIN_REQUIRED".to_owned(),
                reason: Some("Sign in to confirm you are not a bot".to_owned()),
            }),
            Some(YoutubeErrorKind::LoginRequired)
        );
        assert_eq!(
            playability_kind(&PlayabilityStatus {
                status: "AGE_CHECK_REQUIRED".to_owned(),
                reason: None,
            }),
            Some(YoutubeErrorKind::AgeRestricted)
        );
        assert_eq!(
            playability_kind(&PlayabilityStatus {
                status: "UNPLAYABLE".to_owned(),
                reason: Some("Private video".to_owned()),
            }),
            Some(YoutubeErrorKind::Unavailable)
        );
    }

    #[test]
    fn builds_client_body_with_visitor() {
        let options = YoutubeOptions::default();
        let body = player_body(&VISIONOS, "dQw4w9WgXcQ", "visitor", &options);

        assert_eq!(body["videoId"], "dQw4w9WgXcQ");
        assert_eq!(body["context"]["client"]["clientName"], "VISIONOS");
        assert_eq!(body["context"]["client"]["visitorData"], "visitor");
        assert_eq!(body["context"]["client"]["hl"], "en");
        assert_eq!(body["context"]["client"]["gl"], "US");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "network: real YouTube"]
    fn resolves_a_real_video() {
        let source = Source::Url("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let resolved =
            resolve(&source, &HttpOptions::default(), &cancel, &|| false).expect("YouTube resolve");

        match resolved {
            ResolvedSource::Hls {
                source: Source::Url(url),
                title,
            } => {
                assert!(!url.is_empty());
                // В лог не должен попадать query с токенами: только хост.
                let host = split_url(&url)
                    .and_then(|(authority, _)| authority_host(authority))
                    .unwrap_or("?");
                eprintln!("YouTube manifest resolved: host={host}");
                eprintln!("title: {title:?}");
                assert!(
                    title
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty()),
                    "videoDetails.title должен придти для реального ролика"
                );
            }
            other => panic!("unexpected source: {other:?}"),
        }
    }

    /// Полный путь: резолв → HLS → первые кадр и блок звука.
    ///
    /// Запускать вручную при рабочем VPN: сеть блокирует обычный DNS.
    #[test]
    #[ignore = "network: real YouTube"]
    fn plays_a_real_video() {
        let source = Source::Url("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned());
        let mut media = crate::media::Media::open(&source).expect("YouTube HLS open");

        let ready = media.ready(&source);
        eprintln!("video: {:?}", ready.video_info);
        eprintln!("audio: {:?}", ready.media_info.audio);
        eprintln!("title: {:?}", ready.title);
        assert!(ready.video_info.is_some(), "видеодорожка открыта");
        assert!(ready.media_info.audio.is_some(), "аудиодорожка открыта");
        assert!(!ready.live, "VOD");
        assert!(ready.seekable, "VOD перематывается");

        let frame = media
            .video
            .as_mut()
            .expect("video")
            .next_frame_with_buffer(None)
            .expect("decode video frame");
        assert!(frame.is_some(), "первый кадр декодирован");

        let chunk = media
            .audio
            .as_mut()
            .expect("audio")
            .next_chunk()
            .expect("decode audio chunk");
        assert!(chunk.is_some(), "первый блок звука декодирован");
    }

    /// После перемотки вперёд и назад обе дорожки должны ещё отдавать данные.
    #[test]
    #[ignore = "network: real YouTube"]
    fn audio_and_video_survive_seek() {
        fn check(media: &mut crate::media::Media, label: &str) {
            let mut audio_chunks = 0;
            for _ in 0..20 {
                match media.audio.as_mut().unwrap().next_chunk().unwrap() {
                    Some(_) => audio_chunks += 1,
                    None => break,
                }
            }
            eprintln!("{label}: audio chunks = {audio_chunks}");
            assert!(
                audio_chunks > 0,
                "{label}: аудио закончилось после перемотки"
            );

            let frame = media
                .video
                .as_mut()
                .unwrap()
                .next_frame_with_buffer(None)
                .unwrap();
            assert!(frame.is_some(), "{label}: видео после перемотки");
        }

        let source = Source::Url("https://youtu.be/lpEWy4Ipy44".to_owned());
        let mut media = crate::media::Media::open(&source).expect("YouTube HLS open");

        assert!(
            media
                .audio
                .as_mut()
                .unwrap()
                .next_chunk()
                .unwrap()
                .is_some(),
            "звук до перемотки"
        );

        media.seek(Duration::from_secs(15)).expect("seek 15s");
        check(&mut media, "вперёд");

        media.seek(Duration::from_secs(5)).expect("seek 5s");
        check(&mut media, "назад");
    }
}

#[cfg(test)]
mod http_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tiny_http::{Header, Response, Server};

    use super::*;

    #[derive(Clone, Copy)]
    enum Behavior {
        VisionOs,
        IosOnly,
    }

    struct Mock {
        base: String,
        hits: Arc<AtomicUsize>,
    }

    impl Mock {
        fn start(behavior: Behavior) -> Self {
            let server = Server::http("127.0.0.1:0").expect("тестовый сервер");
            let address = server.server_addr().to_ip().expect("ip-адрес");
            let base = format!("http://{address}");
            let hits = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&hits);

            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let client = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("X-YouTube-Client-Name"))
                        .map(|header| header.value.as_str().to_owned())
                        .unwrap_or_default();
                    let path = request.url().to_owned();
                    let body = match path.as_str() {
                        "/visitor" => r#"{"responseContext":{"visitorData":"visitor"}}"#.to_owned(),
                        "/player" => match behavior {
                            Behavior::VisionOs => player_with_hls(),
                            Behavior::IosOnly if client == "5" => player_with_hls(),
                            Behavior::IosOnly => player_without_hls(),
                        },
                        _ => "{}".to_owned(),
                    };
                    let response = Response::from_string(body).with_header(
                        Header::from_bytes("Content-Type", "application/json").expect("header"),
                    );
                    let _ = request.respond(response);
                }
            });

            Self { base, hits }
        }

        fn options(&self) -> YoutubeOptions {
            YoutubeOptions {
                player_url: format!("{}/player", self.base),
                visitor_url: format!("{}/visitor", self.base),
                ..YoutubeOptions::default()
            }
        }
    }

    fn player_with_hls() -> String {
        r#"{"playabilityStatus":{"status":"OK"},"streamingData":{"hlsManifestUrl":"https://example.com/index.m3u8"}}"#.to_owned()
    }

    fn player_without_hls() -> String {
        r#"{"playabilityStatus":{"status":"OK"},"streamingData":{}}"#.to_owned()
    }

    fn resolve_with(options: &YoutubeOptions, id: &str) -> Result<ResolvedSource, PlayerError> {
        let source = Source::Url(format!("https://youtu.be/{id}"));
        let cancel = Arc::new(AtomicBool::new(false));
        resolve_with_options(&source, options, &HttpOptions::default(), &cancel, &|| {
            false
        })
    }

    #[test]
    fn resolves_hls_via_visionos() {
        let mock = Mock::start(Behavior::VisionOs);
        let result = resolve_with(&mock.options(), "visionos001").expect("resolve");

        assert_eq!(
            result,
            ResolvedSource::Hls {
                source: Source::Url("https://example.com/index.m3u8".to_owned()),
                title: None,
            }
        );
        assert!(mock.hits.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn falls_back_to_ios_when_visionos_has_no_hls() {
        let mock = Mock::start(Behavior::IosOnly);
        let result = resolve_with(&mock.options(), "fallback002").expect("resolve");

        assert!(matches!(result, ResolvedSource::Hls { .. }));
        assert!(mock.hits.load(Ordering::SeqCst) >= 2);
    }
}
