//! High-resolution audio from a compatible addon server.
//!
//! The addon protocol is three GET endpoints returning JSON: `{base}/manifest.json`,
//! `{base}/search?q=…&quality=LOSSLESS` and `{base}/stream/{trackId}?quality=LOSSLESS`.
//! A track found there plays from the addon's own URL instead of YouTube Music; when
//! the addon has no copy, or the answer isn't playable, playback falls back to YouTube.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use serde_json::Value;
use futures_util::stream::{FuturesUnordered, StreamExt};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::stream::Format;

/// Addons key off this prefix; anything else is answered `403 UA_BLOCKED`.
pub const USER_AGENT: &str = concat!("BitChord/v", env!("CARGO_PKG_VERSION"));
const TIER: &str = "LOSSLESS";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const RENDITION_TIMEOUT: Duration = Duration::from_secs(12);
/// How many of the search's candidates get a `/stream` call before giving up.
const STREAM_TRIES: usize = 3;
/// Unknown-length files must finish downloading before a seekable buffer can open.
const MAX_UNSIZED_BYTES: usize = 128 * 1024 * 1024;
const SEARCH_TTL: Duration = Duration::from_secs(600);

/// What the player says about the stream it is playing.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// The addon's own name (or its host when it publishes none).
    pub source: String,
    /// The host the audio comes from, without the path (it can carry a token).
    pub host: String,
    /// `FLAC`, `ALAC`, `MP3`, … as stated by the addon or read off the URL.
    pub codec: String,
    /// The quality label the addon sent, when it sent one.
    pub quality: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u8>,
    /// The stream is a Dolby Atmos rendition.
    pub atmos: bool,
    pub size: u64,
    pub duration_ms: u64,
}

impl Info {
    pub fn lossless(&self) -> bool {
        matches!(self.codec.to_ascii_lowercase().as_str(), "flac" | "alac" | "wav" | "aiff")
    }

    /// More than CD: better than 48 kHz or more than 16 bits.
    pub fn hires(&self) -> bool {
        self.lossless() && (self.sample_rate.is_some_and(|r| r > 48_000)
            || self.bit_depth.is_some_and(|b| b > 16)
            || (self.bit_depth.is_none() && has_hires_word(&self.quality)))
    }

    pub fn preference(&self, prefer_atmos: bool) -> (bool, bool, bool, u32, u32, u32) {
        (prefer_atmos && self.atmos, self.hires(), self.lossless(), self.bit_depth.unwrap_or(0),
            self.sample_rate.unwrap_or(0), self.bitrate_kbps.unwrap_or(0))
    }

    /// The quality mark's text, worded as Apple Music words it.
    pub fn badge(&self) -> &'static str {
        if self.atmos {
            "Dolby Atmos"
        } else if self.hires() {
            "Hi-Res Lossless"
        } else if self.lossless() {
            "Lossless"
        } else if self.bitrate_kbps.is_some_and(|k| k >= 320) {
            "320 kbps"
        } else {
            "High"
        }
    }
}

/// One playable answer from the addon.
pub struct Stream {
    pub url: String,
    pub size: u64,
    pub duration_ms: u64,
    pub format: Format,
    pub info: Info,
    /// Audio already in memory, such as decoded Atmos or a file with no length header.
    pub data: Option<Vec<u8>>,
}

/// One addon server, with its manifest and recent searches remembered.
pub struct Client {
    base: String,
    manifest: tokio::sync::Mutex<Option<Manifest>>,
    searches: tokio::sync::Mutex<HashMap<String, (std::time::Instant, Vec<Candidate>)>>,
}

#[derive(Clone)]
struct Manifest {
    name: Option<String>,
    defaults: Vec<(String, String)>,
    quality: String,
    requires_auth: bool,
}

#[derive(Clone)]
struct Candidate {
    id: String,
    title: String,
    artist: String,
    duration: Option<u32>,
    quality: String,
    format: String,
    atmos: bool,
    stream_url: Option<String>,
}

impl Client {
    /// A client for whatever the listener pasted. `None` when it isn't an http(s) URL.
    pub fn new(raw: &str) -> Option<Self> {
        let base = normalize_base(raw);
        let parsed = url::Url::parse(&base).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
            return None;
        }
        Some(Self { base, manifest: Default::default(), searches: Default::default() })
    }

    /// The addon's host, for a log line (the link's path may carry a token).
    pub fn host(&self) -> String {
        host_of(&self.base)
    }

    /// Prepare the best-matching rows from this addon. The callback publishes each
    /// playable answer immediately so a stalled rendition can't hide the others.
    pub async fn find<F>(
        self: &std::sync::Arc<Self>, http: &reqwest::Client, track: &crate::model::Track,
        atmos: bool, fresh: bool, prepare: impl Fn(Stream, crate::stream::Refresh) -> F,
    ) -> anyhow::Result<bool>
    where
        F: std::future::Future<Output = anyhow::Result<()>>,
    {
        let manifest = self.manifest(http).await?;
        anyhow::ensure!(!manifest.requires_auth, "the addon requires official BitChord device authentication");
        let title = &track.title;
        let artist = track.artists.first().map_or("", |a| a.name.as_str());
        let mut queries = Vec::new();
        let searchable = searchable_title(title, artist);
        let primary = primary_artist(artist);
        if !searchable.is_empty() {
            queries.push(if primary.is_empty() { searchable.clone() } else { format!("{searchable} {primary}") });
            if !primary.is_empty() && queries.first().is_none_or(|q| q != &searchable) {
                queries.push(searchable);
            }
        }
        let raw = title.trim().to_lowercase();
        if !raw.is_empty() && !queries.contains(&raw) {
            queries.push(raw);
        }
        let mut tried = BTreeSet::new();
        let mut last_err = None;
        for query in &queries {
            let candidates = match self.search(http, &manifest, query, atmos, fresh).await {
                Ok(c) => c,
                Err(e) => { last_err = Some(e); continue; }
            };
            let mut attempts = FuturesUnordered::new();
            let manifest = &manifest;
            let prepare = &prepare;
            for candidate in rank(candidates, title, artist, track.duration, atmos).into_iter()
                .filter(|c| tried.insert(c.id.clone()))
                .take(STREAM_TRIES)
            {
                attempts.push(tokio::time::timeout(RENDITION_TIMEOUT, async move {
                    let stream = self.stream(http, manifest, &candidate, atmos).await?;
                    let (size, format) = (stream.size, stream.format.clone());
                    let (client, manifest) = (self.clone(), manifest.clone());
                    let refresh: crate::stream::Refresh = std::sync::Arc::new(move || {
                        let (client, manifest, candidate, format) = (client.clone(), manifest.clone(), candidate.clone(), format.clone());
                        Box::pin(async move {
                            let stream = client.stream(&crate::proxy::http(), &manifest, &candidate, atmos).await?;
                            anyhow::ensure!(stream.size == size && stream.format == format, "the addon rendition changed");
                            Ok(stream.url)
                        })
                    });
                    prepare(stream, refresh).await
                }));
            }
            let mut found = false;
            while let Some(result) = attempts.next().await {
                match result.unwrap_or_else(|_| Err(anyhow::anyhow!("addon rendition took too long to prepare"))) {
                    Ok(()) => found = true,
                    Err(e) => {
                        log::debug!("hi-res {}: candidate for \"{title}\" skipped: {e:#}", self.host());
                        last_err = Some(e);
                    }
                }
            }
            if found {
                return Ok(true);
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        Ok(false)
    }

    async fn manifest(&self, http: &reqwest::Client) -> anyhow::Result<Manifest> {
        let mut slot = self.manifest.lock().await;
        if let Some(m) = slot.as_ref() {
            return Ok(m.clone());
        }
        let url = self.endpoint(&["manifest.json"], &[])?;
        let json = get_json(http, &url).await?;
        let id = json.get("id").and_then(Value::as_str).unwrap_or("").trim();
        anyhow::ensure!(!id.is_empty(), "that URL answered, but not with an addon manifest");
        let resources: Vec<&str> = json.get("resources").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        anyhow::ensure!(resources.is_empty() || resources.iter().any(|r| r.eq_ignore_ascii_case("search")), "the addon doesn't offer search");
        let name = json.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        let (defaults, quality) = settings(json.get("settings"));
        let requires_auth = json.get("bitchordAuth").and_then(Value::as_bool) == Some(true);
        let manifest = Manifest { name, defaults, quality, requires_auth };
        *slot = Some(manifest.clone());
        Ok(manifest)
    }

    async fn search(&self, http: &reqwest::Client, manifest: &Manifest, query: &str, atmos: bool, fresh: bool) -> anyhow::Result<Vec<Candidate>> {
        let key = format!("{}|{query}|{atmos}", manifest.quality);
        if !fresh && let Some((at, hit)) = self.searches.lock().await.get(&key)
            && at.elapsed() < SEARCH_TTL
        {
            return Ok(hit.clone());
        }
        let mut params = manifest.params(atmos);
        params.push(("q".to_owned(), query.to_owned()));
        let url = self.endpoint(&["search"], &params)?;
        let json = get_json(http, &url).await?;
        let tracks = json.get("tracks").and_then(Value::as_array).ok_or_else(|| anyhow::anyhow!("the addon returned no track list"))?;
        let found: Vec<Candidate> = tracks.iter().filter_map(candidate).take(400).collect();
        let mut cache = self.searches.lock().await;
        if cache.len() > 64 {
            cache.clear();
        }
        if !found.is_empty() {
            cache.insert(key, (std::time::Instant::now(), found.clone()));
        } else {
            cache.remove(&key);
        }
        Ok(found)
    }

    async fn stream(&self, http: &reqwest::Client, manifest: &Manifest, candidate: &Candidate, atmos: bool) -> anyhow::Result<Stream> {
        let params = manifest.params(atmos);
        let url = self.endpoint(&["stream", &candidate.id], &params)?;
        let json = match get_json(http, &url).await {
            Ok(j) => j,
            // A miss, not a fault: the row may still carry its own URL.
            Err(e) if e.to_string().contains("not found") => Value::Null,
            Err(e) => return Err(e),
        };
        let stated_url = json.get("url").and_then(Value::as_str).unwrap_or("").trim().to_owned();
        let stated_url = if stated_url.is_empty() { candidate.stream_url.clone().unwrap_or_default() } else { stated_url };
        anyhow::ensure!(!stated_url.is_empty(), "the addon returned no stream URL");
        anyhow::ensure!(playable_url(&stated_url), "the addon returned a URL that isn't http(s)");
        anyhow::ensure!(!encrypted(&json), "the addon returned an encrypted stream");
        let transport = json.get("manifest").and_then(Value::as_str).or(json.get("mediaType").and_then(Value::as_str)).unwrap_or("");
        let quality = json.get("quality").and_then(Value::as_str).or(json.get("streamQuality").and_then(Value::as_str)).or(json.get("audioQuality").and_then(Value::as_str)).unwrap_or("").trim().to_owned();
        let label = format!("{quality} {} {}", candidate.quality, candidate.format);
        let codec = json.get("codec").and_then(Value::as_str).or(json.get("fileCodec").and_then(Value::as_str)).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_default();
        let container = json.get("container").and_then(Value::as_str).or(json.get("containerFormat").and_then(Value::as_str)).unwrap_or("");
        let mime = json.get("mimeType").and_then(Value::as_str).unwrap_or("");
        if is_manifest(transport) {
            // Dolby Atmos arrives as a DASH E-AC-3 rendition, not a file: fetch and
            // decode it whole, so it plays (and seeks) like any other stream.
            let dash = matches!(transport.trim().to_ascii_lowercase().as_str(), "dash" | "mpd" | "application/dash+xml");
            let dolby = ["eac3", "ec-3", "ac3", "ac-3", "dolby", "atmos"].iter().any(|w| codec.to_ascii_lowercase().contains(w)) || json_is_atmos(&json);
            anyhow::ensure!(atmos && dash && dolby, "the addon returned an unsupported streaming manifest");
            let audio = crate::dash::fetch(http, &stated_url).await?;
            let size = audio.pcm.len() as u64;
            let info = Info {
                source: manifest.name.clone().unwrap_or_else(|| host_of(&self.base)),
                host: host_of(&stated_url),
                codec: "E-AC-3".to_owned(),
                quality: if quality.is_empty() { candidate.quality.clone() } else { quality },
                bitrate_kbps: Some(audio.bitrate_kbps).filter(|k| *k > 0),
                sample_rate: Some(audio.sample_rate),
                bit_depth: Some(16),
                channels: Some(2),
                atmos: true,
                size,
                duration_ms: audio.duration_ms,
            };
            return Ok(Stream { url: stated_url, size, duration_ms: audio.duration_ms, format: Format::Pcm { rate: audio.sample_rate, channels: 2 }, info, data: Some(audio.pcm) });
        }
        anyhow::ensure!(!atmos, "the addon returned a stereo file");
        let declared_format = json.get("format").and_then(Value::as_str).unwrap_or("");
        let Some(format) = format_of(&codec, &format!("{container} {declared_format}"), mime, &stated_url, &label) else {
            anyhow::bail!("the addon's format isn't playable ({codec} {container} {mime})");
        };
        let (size, data) = probe_media(http, &stated_url, json_size(&json)).await?;
        let sample_rate = json.get("sampleRate").and_then(Value::as_f64).filter(|r| *r > 0.0).map(|r| if r < 1000.0 { (r * 1000.0) as u32 } else { r as u32 })
            .or_else(|| number_before(&label, "khz").map(|r| (r * 1000.0) as u32));
        let bit_depth = json.get("bitDepth").and_then(Value::as_f64).filter(|b| *b > 0.0).map(|b| b as u32)
            .or_else(|| number_before(&label, "bit").filter(|b| (8.0..=32.0).contains(b)).map(|b| b as u32));
        let bitrate_kbps = json.get("bitrate").and_then(Value::as_f64).filter(|k| *k > 0.0).map(|k| if k > 3000.0 { (k / 1000.0) as u32 } else { k as u32 })
            .or_else(|| number_before(&label, "kbps").map(|k| k as u32));
        let duration_ms = candidate.duration.map_or(0, |s| s as u64 * 1000);
        let info = Info {
            source: manifest.name.clone().unwrap_or_else(|| host_of(&self.base)),
            host: host_of(&stated_url),
            codec: codec_name(&codec, &format, &quality, declared_format),
            quality: if quality.is_empty() { candidate.quality.clone() } else { quality },
            bitrate_kbps: bitrate_kbps.or_else(|| size.checked_mul(8)?.checked_div(duration_ms)?.try_into().ok()),
            sample_rate,
            bit_depth,
            channels: None,
            atmos: false,
            size,
            duration_ms,
        };
        Ok(Stream { url: stated_url, size, duration_ms, format, info, data })
    }

    fn endpoint(&self, segments: &[&str], params: &[(String, String)]) -> anyhow::Result<String> {
        let mut url = url::Url::parse(&self.base)?;
        {
            let mut path = url.path_segments_mut().map_err(|_| anyhow::anyhow!("that addon address can't take paths"))?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        Ok(url.into())
    }
}

impl Manifest {
    fn params(&self, atmos: bool) -> Vec<(String, String)> {
        let mut params = self.defaults.clone();
        params.push(("quality".to_owned(), self.quality.clone()));
        if atmos || params.iter().any(|(key, _)| key.eq_ignore_ascii_case("atmos")) {
            params.retain(|(key, _)| !key.eq_ignore_ascii_case("atmos"));
            params.push(("atmos".to_owned(), if atmos { "auto" } else { "stereo" }.to_owned()));
        }
        params
    }
}

/// The base URL with a trailing slash or a pasted `…/manifest.json` taken off.
pub fn normalize_base(raw: &str) -> String {
    let raw = raw.trim();
    let (root, suffix) = raw.split_at(raw.find(['?', '#']).unwrap_or(raw.len()));
    let root = root.trim_end_matches('/');
    let last = root.rsplit('/').next().unwrap_or("");
    let root = if last.to_ascii_lowercase().ends_with(".json") && root.contains("://") {
        root[..root.len() - last.len()].trim_end_matches('/')
    } else {
        root
    };
    format!("{root}{suffix}")
}

fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default()
}

fn settings(value: Option<&Value>) -> (Vec<(String, String)>, String) {
    let mut defaults = Vec::new();
    let mut quality = None;
    let mut alias = None;
    let Some(list) = value.and_then(Value::as_array) else { return (defaults, TIER.to_owned()) };
    for entry in list {
        let Some(key) = entry.get("key").and_then(Value::as_str).map(str::trim).filter(|k| !k.is_empty()) else { continue };
        let default = entry.get("default").and_then(query_value);
        if key.eq_ignore_ascii_case("quality") {
            quality = first_option(entry).or(default);
        } else if key.eq_ignore_ascii_case("audio_quality") {
            alias = first_option(entry).or(default);
        } else if let Some(default) = default {
            defaults.push((key.to_owned(), default));
        }
    }
    if let Some(value) = alias.as_ref() {
        defaults.push(("audio_quality".to_owned(), value.clone()));
    }
    (defaults, quality.or(alias).unwrap_or_else(|| TIER.to_owned()))
}

/// The protocol lists quality options in descending order.
fn first_option(entry: &Value) -> Option<String> {
    entry.get("options")?.as_array()?.iter()
        .find_map(|option| query_value(option.get("value").unwrap_or(option)))
}

fn query_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn candidate(v: &Value) -> Option<Candidate> {
    let id = v.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let title = v.get("title").and_then(Value::as_str).unwrap_or("").trim();
    if id.is_empty() || title.is_empty() {
        return None;
    }
    Some(Candidate {
        id: id.to_owned(),
        title: title.to_owned(),
        artist: v.get("artist").and_then(Value::as_str).unwrap_or("").trim().to_owned(),
        duration: seconds(v.get("duration").unwrap_or(&Value::Null)),
        quality: v.get("audioQuality").and_then(Value::as_str).unwrap_or("").trim().to_owned(),
        format: v.get("format").and_then(Value::as_str).unwrap_or("").trim().to_owned(),
        atmos: json_is_atmos(v),
        stream_url: v.get("streamURL").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
    })
}

fn seconds(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_f64() {
        return (n > 0.0).then_some(n as u32);
    }
    let text = v.as_str()?.trim();
    if let Some((m, s)) = text.split_once(':') {
        let m: u32 = m.parse().ok()?;
        let s: f64 = s.parse().ok()?;
        return Some(m * 60 + s as u32);
    }
    text.parse::<f64>().ok().filter(|n| *n > 0.0).map(|n| n as u32)
}

fn json_size(v: &Value) -> u64 {
    for key in ["fileSize", "filesize", "size", "contentLength", "content_length", "length", "bytes"] {
        if let Some(n) = v.get(key) {
            if let Some(u) = n.as_u64().filter(|u| *u > 0) {
                return u;
            }
            if let Some(f) = n.as_f64().filter(|f| *f > 0.0) {
                return f as u64;
            }
            if let Some(s) = n.as_str().and_then(|s| s.trim().parse::<u64>().ok()).filter(|u| *u > 0) {
                return s;
            }
        }
    }
    0
}

/// A row or stream that says it's Dolby Atmos, under any of the names addons use for it.
fn json_is_atmos(v: &Value) -> bool {
    const KEYS: [&str; 7] = ["audioMode", "audioModes", "quality", "streamQuality", "audioQuality", "format", "codec"];
    v.get("atmos").and_then(Value::as_bool) == Some(true)
        || KEYS.iter().any(|key| v.get(*key).is_some_and(value_is_atmos))
}

fn value_is_atmos(v: &Value) -> bool {
    match v {
        Value::String(s) => {
            let s = s.to_ascii_lowercase();
            ["atmos", "dolby", "ac-3", "ac3", "eac3", "ec-3"].iter().any(|w| s.contains(w))
        }
        Value::Array(items) => items.iter().any(value_is_atmos),
        _ => false,
    }
}

fn encrypted(v: &Value) -> bool {
    match v.get("encrypted") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            let s = s.trim().to_ascii_lowercase();
            !s.is_empty() && s != "false" && s != "none" && s != "0"
        }
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

fn is_manifest(transport: &str) -> bool {
    matches!(transport.trim().to_ascii_lowercase().as_str(), "hls" | "m3u8" | "dash" | "mpd" | "application/x-mpegurl" | "application/vnd.apple.mpegurl" | "application/dash+xml")
}

fn playable_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()))
}

/// The container/codec rodio will be handed.
fn format_of(codec: &str, container: &str, mime: &str, url: &str, label: &str) -> Option<Format> {
    let codec = codec.to_ascii_lowercase();
    let container = container.to_ascii_lowercase();
    let mime = mime.to_ascii_lowercase();
    let ext = url.split(['?', '#']).next().unwrap_or(url).rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let all = format!("{codec} {container} {mime} {ext}");
    // Symphonia has no AC-3 family decoder, so an Atmos rendition in E-AC-3/TrueHD isn't
    // playable and the next copy is tried instead.
    if ["ac-3", "ac3", "eac-3", "eac3", "ec-3", "truehd", "mlp"].iter().any(|w| all.contains(w)) {
        return None;
    }
    let file = |hint: &'static str, mime: &str| Format::File { hint, mime: Some(mime.to_owned()) };
    if all.contains("flac") {
        return Some(file("flac", "audio/flac"));
    }
    if all.contains("alac") || ext == "m4a" || ext == "mp4" || mime.contains("mp4") {
        return Some(file("m4a", "audio/mp4"));
    }
    if all.contains("wav") {
        return Some(file("wav", "audio/wav"));
    }
    if all.contains("mp3") || mime.contains("mpeg") {
        return Some(file("mp3", "audio/mpeg"));
    }
    if all.contains("vorbis") || ext == "ogg" || mime.contains("ogg") {
        return Some(file("ogg", "audio/ogg"));
    }
    if all.contains("opus") || ext == "webm" || mime.contains("webm") {
        return Some(Format::Opus);
    }
    if all.contains("aac") || all.contains("m4a") || ext == "m4a" {
        return Some(file("m4a", "audio/mp4"));
    }
    if has_lossless_word(label) {
        // A lossless row that names no codec is FLAC in practice.
        return Some(file("flac", "audio/flac"));
    }
    None
}

fn codec_name(codec: &str, format: &Format, quality: &str, declared_format: &str) -> String {
    let codec = codec.trim();
    if !codec.is_empty() {
        return codec.to_ascii_uppercase();
    }
    match format {
        Format::Aac => "AAC",
        Format::Opus => "OPUS",
        Format::Pcm { .. } => "PCM",
        Format::File { hint, .. } => match *hint {
            "flac" => "FLAC",
            "wav" => "WAV",
            "mp3" => "MP3",
            "ogg" => "VORBIS",
            "m4a" if declared_format.eq_ignore_ascii_case("alac") || has_lossless_word(quality) => "ALAC",
            _ => "AAC",
        },
    }
    .to_owned()
}

fn has_lossless_word(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["lossless", "flac", "hifi", "hi-res", "hires", "master", "24bit", "24-bit"].iter().any(|w| text.contains(w))
}

fn has_hires_word(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["hi-res", "hi_res", "hires", "24-bit", "24bit"].iter().any(|w| text.contains(w))
}

/// The number just before `unit` in free text (`24-bit`, `96 kHz`, `1411kbps`).
fn number_before(text: &str, unit: &str) -> Option<f64> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(unit) {
        let at = from + found;
        let mut end = at;
        // `24-bit`, `96 kHz` and `1411 kbps` all read the same.
        while end > 0 && matches!(lower.as_bytes()[end - 1], b' ' | b'-') {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && (lower.as_bytes()[start - 1].is_ascii_digit() || lower.as_bytes()[start - 1] == b'.') {
            start -= 1;
        }
        if start < end
            && let Ok(n) = lower[start..end].trim_matches('.').parse::<f64>()
            && n > 0.0
        {
            return Some(n);
        }
        from = at + unit.len();
        if from >= lower.len() {
            break;
        }
    }
    None
}

// ── Matching ─────────────────────────────────────────────────────────────

fn rank(mut candidates: Vec<Candidate>, title: &str, artist: &str, duration: Option<u32>, prefer_atmos: bool) -> Vec<Candidate> {
    let mut scored: Vec<(Candidate, i32)> = candidates.drain(..).filter_map(|c| score(&c, title, artist, duration, prefer_atmos).map(|s| (c, s))).collect();
    scored.sort_by_key(|(c, score)| std::cmp::Reverse((prefer_atmos && c.atmos, *score, has_hires_word(&c.quality))));
    scored.into_iter().map(|(c, _)| c).collect()
}

fn score(candidate: &Candidate, title: &str, artist: &str, duration: Option<u32>, prefer_atmos: bool) -> Option<i32> {
    let wanted = title_parts(title, artist);
    let got = title_parts(&candidate.title, &candidate.artist);
    if wanted.0.is_empty() || match_key(&wanted.0) != match_key(&got.0) || wanted.1 != got.1 {
        return None;
    }
    let target_artists = artist_names(artist);
    let candidate_artists = artist_names(&candidate.artist);
    // A copy that doesn't name the artist is not the same song: never swap the
    // listener's stream for it.
    if !target_artists.is_empty()
        && (candidate_artists.is_empty() || !target_artists.iter().any(|w| candidate_artists.iter().any(|g| same_artist(w, g))))
    {
        return None;
    }
    if let (Some(wanted), Some(got)) = (duration, candidate.duration)
        && wanted.abs_diff(got) > 30
    {
        return None;
    }
    let artist_score = match (target_artists.is_empty() || candidate_artists.is_empty(), target_artists == candidate_artists) {
        (true, _) => 0,
        (false, true) => 30,
        (false, false) => 20,
    };
    let duration_score = match (duration, candidate.duration) {
        (Some(wanted), Some(got)) => (20 - wanted.abs_diff(got) as i32).max(0),
        _ => 0,
    };
    let atmos_score = if prefer_atmos && candidate.atmos { 8 } else { 0 };
    let title_score = if wanted.0 == got.0 { 100 } else { 90 };
    Some(title_score + artist_score + duration_score + atmos_score)
}

/// The title with the packaging taken off, version markers kept.
fn searchable_title(title: &str, artist: &str) -> String {
    let (words, versions) = title_parts(title, artist);
    let mut out = words;
    for version in versions {
        out.push(' ');
        out.push_str(&version);
    }
    out.trim().to_owned()
}

fn primary_artist(artist: &str) -> &str {
    artist.split([',', '&', '/', ';', '·', '|']).next().unwrap_or("").trim()
}

type TitleParts = (String, Vec<String>);

/// A title split into its core words and its version markers.
fn title_parts(raw: &str, artist: &str) -> TitleParts {
    let mut versions = BTreeSet::new();
    let mut text = raw.to_lowercase().replace('&', " and ");
    for _ in 0..3 {
        let Some((start, end)) = bracket_span(&text) else { break };
        classify(&text[start + 1..end], &mut versions);
        text.replace_range(start..=end, " ");
    }
    if let Some(at) = text.find(['(', '[']) {
        classify(&text[at..], &mut versions);
        text.truncate(at);
    }
    for _ in 0..3 {
        let Some(at) = text.find(" - ") else { break };
        let (head, tail) = (text[..at].to_owned(), text[at + 3..].to_owned());
        if is_artist_name(&head, artist) {
            classify(&head, &mut versions);
            text = tail;
        } else if is_artist_name(&tail, artist) {
            classify(&tail, &mut versions);
            text = head;
        } else {
            classify(&tail, &mut versions);
            text = head;
        }
    }
    for marker in [" feat.", " feat ", " ft.", " ft ", " featuring ", " with "] {
        if let Some(at) = text.find(marker) {
            text.truncate(at);
        }
    }
    let mut words: Vec<String> = text.split(|c: char| c.is_whitespace() || matches!(c, '.' | '·'))
        .map(clean).filter(|w| !w.is_empty() && w != "and").collect();
    while words.len() > 1 && matches!(words.last().map(String::as_str), Some("song" | "songs" | "video" | "audio" | "lyrics" | "official" | "full" | "hd" | "hq" | "4k" | "mp3" | "ost" | "soundtrack")) {
        words.pop();
    }
    (words.join(" "), versions.into_iter().collect())
}

fn bracket_span(text: &str) -> Option<(usize, usize)> {
    let open = text.find(['(', '['])?;
    let close = text[open..].find([')', ']'])? + open;
    Some((open, close))
}

fn classify(segment: &str, versions: &mut BTreeSet<String>) {
    let words: Vec<String> = segment.split([' ', '.', '·']).map(clean).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return;
    }
    let joined = words.join("");
    if matches!(joined.as_str(), "albumversion" | "originalversion" | "originalmix" | "singleversion" | "radioversion" | "radioedit" | "stereoversion" | "monoversion" | "studioversion" | "fullversion" | "standardversion" | "explicitversion" | "deluxeversion" | "originaltrack") {
        return;
    }
    for word in words {
        if VERSION_WORDS.contains(&word.as_str()) {
            versions.insert(word);
        }
    }
}

fn is_artist_name(text: &str, artist: &str) -> bool {
    if artist.is_empty() {
        return false;
    }
    let words: Vec<String> = text.split([' ', '.', '·']).map(clean).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return false;
    }
    let credited: Vec<String> = artist.to_lowercase().split([' ', '.', '·']).map(clean).filter(|w| !w.is_empty()).collect();
    words.iter().all(|w| credited.contains(w))
}

fn artist_names(value: &str) -> Vec<Vec<String>> {
    let normalized = value.to_lowercase();
    let normalized = normalized.trim();
    if normalized.is_empty() || matches!(normalized, "unknown" | "unknown artist" | "unknown artist(s)") {
        return Vec::new();
    }
    normalized
        .split([',', '&', '/', ';', '·', '|'])
        .map(|name| {
            let mut parts: Vec<String> = name.split([' ', '.']).map(|word| match_key(&clean(word))).filter(|w| w.chars().count() > 1).collect();
            if parts.is_empty() {
                parts = vec![name.trim().to_owned()];
            }
            parts
        })
        .collect::<Vec<Vec<String>>>()
        .into_iter()
        .filter(|parts| parts.iter().any(|w| !w.is_empty()))
        .collect()
}

fn same_artist(first: &[String], second: &[String]) -> bool {
    if contains_words(first, second) || contains_words(second, first) {
        return true;
    }
    // Addons sometimes drop a middle name; accept only when every token of the shorter
    // credited name appears in order in the longer one.
    ordered_subset(first, second) || ordered_subset(second, first)
}

fn ordered_subset(shorter: &[String], longer: &[String]) -> bool {
    if shorter.is_empty() || shorter.len() >= longer.len() {
        return false;
    }
    let mut at = 0;
    for word in shorter {
        let Some(pos) = longer[at..].iter().position(|w| w == word) else { return false };
        at += pos + 1;
    }
    true
}

fn contains_words(outer: &[String], inner: &[String]) -> bool {
    !inner.is_empty() && inner.len() <= outer.len() && (0..=outer.len() - inner.len()).any(|start| outer[start..start + inner.len()] == *inner)
}

fn clean(word: &str) -> String {
    word.nfc().filter(|c| c.is_alphanumeric() || is_combining_mark(*c)).collect()
}

/// Fold Latin accents for catalogues with unaccented metadata. Keep other scripts' marks.
fn match_key(word: &str) -> String {
    let mut latin = false;
    word.nfd().filter(|c| {
        if is_combining_mark(*c) {
            return !latin;
        }
        latin = c.is_ascii_alphabetic();
        true
    }).map(|c| if c == 'đ' { 'd' } else { c }).collect()
}

const VERSION_WORDS: &[&str] = &[
    "remix", "remixes", "rmx", "refix", "flip", "bootleg", "mashup", "medley", "live", "concert", "unplugged", "acoustic", "instrumental", "karaoke", "vocals", "vocal",
    "acapella", "acappella", "backing", "stems", "stem", "cover", "demo", "reprise", "remake", "rework", "extended", "edit", "version", "mix", "dub", "vip", "session",
    "sessions", "sped", "slowed", "reverb", "nightcore", "lofi", "orchestral", "symphonic", "part", "pt", "chapter",
];

// ── HTTP ─────────────────────────────────────────────────────────────────

async fn get_json(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    let response = http.get(url).timeout(REQUEST_TIMEOUT).header("Accept", "application/json").header("User-Agent", USER_AGENT).send().await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("not found");
    }
    anyhow::ensure!(status.is_success(), "HTTP {}", status.as_u16());
    let bytes = response.bytes().await.map_err(reqwest::Error::without_url)?;
    anyhow::ensure!(!bytes.is_empty(), "the addon answered with nothing");
    Ok(serde_json::from_slice(&bytes)?)
}

/// Confirm the signed media URL is usable and find its real length.
async fn probe_media(http: &reqwest::Client, url: &str, stated_size: u64) -> anyhow::Result<(u64, Option<Vec<u8>>)> {
    use reqwest::{StatusCode, header};
    let request = || http.get(url).timeout(PROBE_TIMEOUT).header("User-Agent", USER_AGENT).header(header::ACCEPT_ENCODING, "identity");
    let mut response = request().header(header::RANGE, "bytes=0-0").send().await.map_err(reqwest::Error::without_url)?;
    if response.status() == StatusCode::PARTIAL_CONTENT {
        let total = response.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes 0-0/")).and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(stated_size);
        if total > 0 {
            return Ok((total, None));
        }
    }
    if matches!(response.status(), StatusCode::PARTIAL_CONTENT | StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN | StatusCode::METHOD_NOT_ALLOWED | StatusCode::RANGE_NOT_SATISFIABLE | StatusCode::TOO_MANY_REQUESTS) {
        response = request().send().await.map_err(reqwest::Error::without_url)?;
    }
    anyhow::ensure!(response.status() == StatusCode::OK, "addon media returned HTTP {}", response.status().as_u16());
    if let Some(size) = response.content_length().filter(|n| *n > 0) {
        return Ok((size, None));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
        anyhow::ensure!(data.len() + chunk.len() <= MAX_UNSIZED_BYTES, "addon media without a length is too large");
        data.extend_from_slice(&chunk);
    }
    anyhow::ensure!(!data.is_empty(), "the addon media is empty");
    Ok((data.len() as u64, Some(data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_preserves_vietnamese_titles_and_other_scripts() {
        assert_eq!(searchable_title("Rực Rỡ (Official Audio)", "Mỹ Tâm"), "rực rỡ");
        assert_eq!(searchable_title("夜に駆ける", "YOASOBI"), "夜に駆ける");
        let hit = candidate(&serde_json::json!({"id": "1", "title": "Kho Bau", "artist": "(S)TRONG", "duration": 197})).unwrap();
        assert!(score(&hit, "Kho Báu", "(S)TRONG", Some(197), false).is_some());
        assert!(score(&hit, "Kho Báu (Live)", "(S)TRONG", Some(197), false).is_none());
    }

    #[test]
    fn best_quality_uses_the_addons_first_option_even_without_a_default() {
        let config = serde_json::json!([{
            "key": "quality", "options": [{"value": "HI_RES_LOSSLESS"}, {"value": "LOSSLESS"}]
        }]);
        assert_eq!(settings(Some(&config)).1, "HI_RES_LOSSLESS");
        let config = serde_json::json!([{
            "key": "audio_quality", "options": [{"value": "HI_RES_LOSSLESS"}, {"value": "LOSSLESS"}]
        }]);
        assert_eq!(settings(Some(&config)).1, "HI_RES_LOSSLESS");
    }

    #[test]
    fn a_lossy_stream_does_not_inherit_a_lossless_or_atmos_badge() {
        let info = Info {
            source: "Addon".into(), host: "example".into(), codec: "AAC".into(),
            quality: "LOSSLESS".into(), bitrate_kbps: Some(320), sample_rate: Some(48_000),
            bit_depth: None, channels: None, atmos: false, size: 1, duration_ms: 1,
        };
        assert_eq!(info.badge(), "320 kbps");
    }

    #[test]
    fn base_is_normalized_like_the_addon_protocol() {
        assert_eq!(normalize_base(" https://music.example/addon/ "), "https://music.example/addon");
        assert_eq!(normalize_base("https://music.example/addon/manifest.json"), "https://music.example/addon");
        assert_eq!(normalize_base("https://music.example/addon/MANIFEST.JSON"), "https://music.example/addon");
        assert_eq!(normalize_base("https://music.example/addon/manifest.json?key=test"), "https://music.example/addon?key=test");
        assert_eq!(normalize_base("https://music.example"), "https://music.example");
    }

    #[test]
    fn quality_labels_are_read() {
        assert_eq!(number_before("Hi-Res FLAC 24-bit 96 kHz", "khz"), Some(96.0));
        assert_eq!(number_before("Hi-Res FLAC 24-bit 96 kHz", "bit"), Some(24.0));
        assert_eq!(number_before("320 kbps", "kbps"), Some(320.0));
        assert_eq!(number_before("lossless", "kbps"), None);
    }

    #[test]
    fn a_row_matches_only_its_own_recording() {
        let target = |title: &str| Candidate {
            id: "1".into(),
            title: title.into(),
            artist: "Carly Rae Jepsen".into(),
            duration: Some(200),
            quality: String::new(),
            format: String::new(),
            atmos: false,
            stream_url: None,
        };
        let pick = |title: &str| rank(vec![target(title)], "Call Me Maybe", "Carly Rae Jepsen", Some(202), false).len();
        assert_eq!(pick("Call Me Maybe"), 1);
        assert_eq!(pick("Call Me Maybe (Live at Wembley)"), 0);
        assert_eq!(pick("Call Me Maybe (Official Audio)"), 1);
    }

    #[test]
    fn a_copy_without_the_artist_is_not_the_song() {
        let row = |artist: &str| Candidate {
            id: "1".into(),
            title: "Call Me Maybe".into(),
            artist: artist.into(),
            duration: Some(200),
            quality: String::new(),
            format: String::new(),
            atmos: false,
            stream_url: None,
        };
        let pick = |artist: &str| rank(vec![row(artist)], "Call Me Maybe", "Carly Rae Jepsen", Some(202), false).len();
        assert_eq!(pick(""), 0);
        assert_eq!(pick("Someone Else"), 0);
        assert_eq!(pick("Carly Rae Jepsen"), 1);
        assert_eq!(pick("Carly Jepsen"), 1);
    }

    #[test]
    fn the_badge_says_what_the_stream_is() {
        let info = |codec: &str, rate: Option<u32>, bits: Option<u32>| Info {
            source: "Addon".into(),
            host: "example".into(),
            codec: codec.into(),
            quality: String::new(),
            bitrate_kbps: None,
            sample_rate: rate,
            bit_depth: bits,
            channels: None,
            atmos: false,
            size: 1,
            duration_ms: 1,
        };
        assert_eq!(info("FLAC", Some(44_100), Some(16)).badge(), "Lossless");
        assert_eq!(info("FLAC", Some(96_000), Some(24)).badge(), "Hi-Res Lossless");
        assert_eq!(info("MP3", None, None).badge(), "High");
        let atmos = Info { atmos: true, ..info("EAC3", None, None) };
        assert_eq!(atmos.badge(), "Dolby Atmos");
    }

    #[test]
    fn atmos_rows_are_read_off_the_names_addons_use() {
        let row = |json: Value| json_is_atmos(&json);
        assert!(row(serde_json::json!({"audioMode": "DOLBY_ATMOS"})));
        assert!(row(serde_json::json!({"audioModes": ["STEREO", "DOLBY_ATMOS"]})));
        assert!(row(serde_json::json!({"quality": "E-AC-3 JOC"})));
        assert!(!row(serde_json::json!({"audioMode": "STEREO", "format": "FLAC"})));
    }
}
