//! Apple Music's public web data: artist names and wordmarks.
//!
//! Nothing here is authenticated on the user's behalf. The catalog API wants a
//! bearer token even for anonymous reads, and the one the web player mints for
//! visitors is scraped from its own JS bundle, kept in memory and never logged.
//! Artist searches prefer exact names, retaining Apple's result order on ties.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::Value;

const AMP: &str = "https://amp-api.music.apple.com/v1/catalog";
const STOREFRONT: &str = "us";
const WEB_PLAYER: &str = "https://music.apple.com/us/browse";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";

/// Apple's own page for an artist: their display name and wordmark.
#[derive(Clone, Debug, Default)]
pub struct Artwork {
    pub name: String,
    /// The custom wordmark Apple lettered for the artist, when they have one.
    pub logo: Option<LogoSrc>,
}

/// The artist's wordmark as Apple's CDN template, `{w}x{h}{c}.{f}` and all.
#[derive(Clone, Debug)]
pub struct LogoSrc {
    pub template: String,
    pub w: u32,
    pub h: u32,
}

impl LogoSrc {
    /// The wordmark `height` px tall, keeping Apple's aspect, at the `bb` crop.
    pub fn at_height(&self, height: u32) -> String {
        let height = height.max(1);
        let w = (self.w as u64 * height as u64 / self.h.max(1) as u64).max(1);
        self.template
            .replace("{w}", &w.to_string())
            .replace("{h}", &height.to_string())
            .replace("{c}", "bb")
            .replace("{f}", "png")
    }
}

const TOKEN_TTL_MARGIN: i64 = 300;
const TOKEN_RETRY: Duration = Duration::from_secs(30 * 60);

struct Token {
    jwt: String,
    expires: i64,
}

static TOKEN: LazyLock<Mutex<Option<Token>>> = LazyLock::new(|| Mutex::new(None));
static REJECTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
static RETRY_AFTER: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

async fn get(http: &reqwest::Client, url: &str, bearer: Option<&str>) -> anyhow::Result<(u16, String)> {
    let mut r = http.get(url).timeout(Duration::from_secs(12)).header("User-Agent", UA);
    if let Some(b) = bearer {
        r = r.header("Authorization", format!("Bearer {b}")).header("Origin", "https://music.apple.com").header("Referer", "https://music.apple.com/");
    }
    let resp = r.send().await?;
    let code = resp.status().as_u16();
    Ok((code, resp.text().await?))
}

// --------------------------------------------------------------- token

/// The anonymous bearer the web player uses for catalog reads. It isn't
/// published anywhere and rotates, so it's read the way the browser gets it:
/// load the web player, find the JS bundle it pulls in, take the token out of it.
async fn token(http: &reqwest::Client) -> Option<String> {
    let now = now_secs();
    if let Some(t) = TOKEN.lock().ok().and_then(|t| t.as_ref().map(|t| (t.jwt.clone(), t.expires))) {
        if now < t.1 - TOKEN_TTL_MARGIN {
            return Some(t.0);
        }
    }
    if RETRY_AFTER.lock().ok().and_then(|r| *r).is_some_and(|at| Instant::now() < at) {
        return None;
    }

    let mut found = None;
    if let Ok((_, html)) = get(http, WEB_PLAYER, None).await {
        for path in script_paths(&html) {
            let Ok((_, js)) = get(http, &format!("https://music.apple.com{path}"), None).await else { continue };
            let rejected = REJECTED.lock().map(|r| r.clone()).unwrap_or_default();
            let mut best: Option<(String, i64, bool)> = None;
            for jwt in jwt_candidates(&js) {
                if rejected.contains(&jwt) {
                    continue;
                }
                let Some(exp) = jwt_expiry(&jwt) else { continue };
                if exp <= now {
                    continue;
                }
                let web = is_web_player_token(&jwt);
                if best.as_ref().is_none_or(|b| web && !b.2) {
                    best = Some((jwt, exp, web));
                }
            }
            if let Some((jwt, expires, _)) = best {
                found = Some(Token { jwt, expires });
                break;
            }
        }
    }
    match found {
        Some(t) => {
            log::debug!("apple: web player token good for {} min", (t.expires - now).max(0) / 60);
            let jwt = t.jwt.clone();
            if let Ok(mut slot) = TOKEN.lock() {
                *slot = Some(t);
            }
            Some(jwt)
        }
        None => {
            log::debug!("apple: no usable web player token");
            if let Ok(mut r) = RETRY_AFTER.lock() {
                *r = Some(Instant::now() + TOKEN_RETRY);
            }
            None
        }
    }
}

/// `/assets/index~<hash>.js` and its legacy twin, as the page names them.
fn script_paths(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("/assets/index") {
        let tail = &rest[i..];
        if let Some(end) = tail.find(".js") {
            let path = &tail[..end + 3];
            if path.chars().all(|c| !matches!(c, '"' | '\'' | '<' | '>' | ' ')) && !out.iter().any(|p| p == path) {
                out.push(path.to_owned());
            }
            rest = &tail[end..];
        } else {
            break;
        }
    }
    out
}

fn jwt_candidates(js: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = js.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if &bytes[i..i + 2] == b"ey" {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || matches!(bytes[j], b'-' | b'_' | b'.')) {
                j += 1;
            }
            let cand = &js[i..j];
            if cand.matches('.').count() == 2 && cand.len() > 40 && !out.iter().any(|c| c == cand) {
                out.push(cand.to_owned());
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

fn jwt_parts(jwt: &str) -> Option<(String, String)> {
    let part = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok().and_then(|b| String::from_utf8(b).ok());
    let mut it = jwt.split('.');
    Some((part(it.next()?)?, part(it.next()?)?))
}

fn jwt_expiry(jwt: &str) -> Option<i64> {
    let (_, payload) = jwt_parts(jwt)?;
    let i = payload.find("\"exp\"")?;
    let rest = &payload[i + 5..];
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits: String = rest[start..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn is_web_player_token(jwt: &str) -> bool {
    jwt_parts(jwt).is_some_and(|(header, payload)| header.contains("WebPlayKid") || payload.contains("AMPWebPlay"))
}

fn reject(jwt: &str) {
    if let Ok(mut set) = REJECTED.lock() {
        set.insert(jwt.to_owned());
    }
    if let Ok(mut slot) = TOKEN.lock() {
        if slot.as_ref().is_some_and(|t| t.jwt == jwt) {
            *slot = None;
        }
    }
}

/// An authenticated catalog read. A 401 means the token picked out of the
/// bundle isn't the one this endpoint honours, so it is struck off and the
/// next lookup scrapes a different one.
async fn api(http: &reqwest::Client, path_and_query: &str) -> Option<Value> {
    for _ in 0..2 {
        let jwt = token(http).await?;
        let (code, body) = get(http, &format!("{AMP}/{STOREFRONT}/{path_and_query}"), Some(&jwt)).await.ok()?;
        if code == 200 {
            return serde_json::from_str(&body).ok();
        }
        if code == 401 {
            log::debug!("apple: catalog rejected the token; re-scraping");
            reject(&jwt);
            continue;
        }
        log::debug!("apple: catalog answered {code} for {path_and_query}");
        return None;
    }
    None
}

// ------------------------------------------------------------- matching

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Apple's artist page: their display name and wordmark.
pub async fn artist(http: &reqwest::Client, name: &str) -> Option<Artwork> {
    let q = format!("search?term={}&types=artists&limit=10", url_escape(name));
    let v = api(http, &q).await?;
    let hits = v.pointer("/results/artists/data")?.as_array()?;
    let (id, page_name) = artist_hit(hits, name)?;

    let url = format!("https://music.apple.com/{STOREFRONT}/artist/-/{id}");
    let (code, html) = get(http, &url, None).await.ok()?;
    if code != 200 {
        return Some(Artwork { name: page_name, ..Default::default() });
    }
    let shown = page_title(&html).unwrap_or(page_name);
    let logo = artist_logo(&html);
    Some(Artwork { name: shown, logo })
}

/// The search hit that is this artist: an exact name match first, then one whose
/// name contains the other's. Apple ranks its hits, so on a tie the earliest is the
/// artist people mean — it also lists impostor profiles under famous names, and some
/// of those have no wordmark at all.
fn artist_hit(hits: &[serde_json::Value], name: &str) -> Option<(String, String)> {
    let want = norm(name);
    hits.iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let n = h.pointer("/attributes/name")?.as_str()?;
            let got = norm(n);
            let s = if got == want {
                2
            } else if got.contains(&want) || want.contains(&got) {
                1
            } else {
                0
            };
            let id = h.get("id")?.as_str()?;
            (s > 0).then(|| (s, std::cmp::Reverse(i), id.to_owned(), n.to_owned()))
        })
        .max_by_key(|(s, first, _, _)| (*s, *first))
        .map(|(_, _, id, n)| (id, n))
}

/// The `"artistLogo"` block's CDN template and its natural size.
fn artist_logo(html: &str) -> Option<LogoSrc> {
    // Anchored on the open brace: `"artistLogo":null` must not fall through to some
    // neighbouring artwork's URL and be drawn as the artist's name.
    let at = html.find("\"artistLogo\":{")?;
    // The block is a flat dictionary; a generous window keeps us inside it.
    let block = &html[at..html.floor_char_boundary((at + 1400).min(html.len()))];
    let template = url_after_key(block, "{", "url")?.replace("\\/", "/");
    let (w, h) = (number_after(block, "\"width\"")?, number_after(block, "\"height\"")?);
    // Only a real template can be resized; a plain URL is taken as-is.
    (w > 0 && h > 0 && template.contains("{w}")).then(|| LogoSrc { template, w, h })
}

/// The number after `"key":` in `s`.
fn number_after(s: &str, key: &str) -> Option<u32> {
    let i = s.find(key)? + key.len();
    let tail = s[i..].trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    let end = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
    tail[..end].parse().ok()
}

fn page_title(html: &str) -> Option<String> {
    let i = html.find("<title>")? + 7;
    let rest = &html[i..];
    let end = rest.find("</title>")?;
    let t = rest[..end].trim().trim_start_matches('\u{200e}');
    let t = t.strip_suffix(" - Apple Music").unwrap_or(t);
    Some(t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// The quoted string after `"key":"…"` within the JSON blob that follows
/// `anchor` in the page.
fn url_after_key<'a>(html: &'a str, anchor: &str, key: &str) -> Option<&'a str> {
    let at = html.find(anchor)?;
    let rest = &html[at..];
    let pat = format!("\"{key}\":\"");
    let i = rest.find(&pat)? + pat.len();
    let tail = &rest[i..];
    let end = tail.find('"')?;
    Some(&tail[..end])
}

fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_of_two_profiles_with_one_name_is_the_artist() {
        // Apple lists a second "Charli xcx" (an impostor profile) after the real one,
        // and its page has no wordmark: picking the last equal hit loses the lettering.
        let hit = |id: &str, name: &str| serde_json::json!({"id": id, "attributes": {"name": name}});
        let hits = vec![hit("432942256", "Charli xcx"), hit("1845539402", "Charli xcx"), hit("9", "Charli XCX Tribute")];
        assert_eq!(artist_hit(&hits, "Charli XCX").map(|(id, _)| id).as_deref(), Some("432942256"));
        // An exact name still beats one that merely contains it.
        let hits = vec![hit("1", "Taylor Swift Tribute"), hit("2", "Taylor Swift")];
        assert_eq!(artist_hit(&hits, "Taylor Swift").map(|(id, _)| id).as_deref(), Some("2"));
        assert!(artist_hit(&[hit("3", "Someone Else")], "Charli XCX").is_none());
    }

    /// Live: what the app itself gets back for real artists.
    #[tokio::test]
    #[ignore]
    async fn artist_live() {
        let http = reqwest::Client::builder().build().unwrap();
        for name in ["Charli XCX", "Taylor Swift", "Lady Gaga", "Kendrick Lamar"] {
            match artist(&http, name).await {
                Some(a) => println!("{name}: name={:?} logo={:?}", a.name, a.logo.as_ref().map(|l| (l.w, l.h, l.at_height(144)))),
                None => println!("{name}: no answer at all"),
            }
        }
    }

    #[test]
    fn scripts_and_tokens_are_found_without_a_regex() {
        let html = r#"<script src="/assets/index~abc123.js"></script><script src="/assets/index-legacy~de.js"></script>"#;
        assert_eq!(script_paths(html), vec!["/assets/index~abc123.js", "/assets/index-legacy~de.js"]);
        // Two base64url parts around a payload that names the web player.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"iss":"AMPWebPlay","exp":1900000000}"#);
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"kid":"WebPlayKid"}"#);
        let sig = "A".repeat(43);
        let js = format!("var t=\"{header}.{payload}.{sig}\";var u=\"ey.short\";");
        let found = jwt_candidates(&js);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(is_web_player_token(&found[0]));
        assert_eq!(jwt_expiry(&found[0]), Some(1_900_000_000));
    }

    #[test]
    fn the_artist_page_title_names_the_artist() {
        let page = "<title>\u{200e}Charli xcx - Apple Music</title>";
        assert_eq!(page_title(page).as_deref(), Some("Charli xcx"));
    }

    #[test]
    fn the_artist_logo_template_is_sized_in_proportion() {
        let html = r#"{"artistLogo":{"dictionary":{"bgColor":"ae211a","height":655,"textColor1":"faf3f3","url":"https://is1-ssl.mzstatic.com/image/thumb/Features/v4/ae/d9/28/aed9.png/{w}x{h}{c}.{f}","width":2880},"cropStyle":"bb"}}"#;
        let logo = artist_logo(html).unwrap();
        assert_eq!((logo.w, logo.h), (2880, 655));
        let url = logo.at_height(1310);
        assert_eq!(url, "https://is1-ssl.mzstatic.com/image/thumb/Features/v4/ae/d9/28/aed9.png/5760x1310bb.png");
        assert!(artist_logo("{\"artistLogo\":null}").is_none());
        // A null logo must not borrow the next image on the page.
        let none = r#"{"artistLogo":null}"#.to_owned()
            + r#"{"editorialVideo":{"dictionary":{"url":"https://img/other.png/{w}x{h}{c}.{f}","width":100,"height":100}}}"#;
        assert!(artist_logo(&none).is_none());
    }

    #[test]
    fn artist_logo_handles_unicode_at_the_lookup_boundary() {
        let mut html = r#"{"artistLogo":{"dictionary":{"url":"https://img/{w}x{h}{c}.{f}","width":100,"height":50},"padding":""#.to_owned();
        // The lookup starts at byte 1 and ends at byte 1401, inside this character.
        html.extend(std::iter::repeat_n('x', 1400 - html.len()));
        html.push_str("é\"}}");
        let logo = artist_logo(&html).expect("complete logo metadata before the boundary");
        assert_eq!((logo.w, logo.h), (100, 50));
    }

}
