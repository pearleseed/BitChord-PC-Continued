//! Network side: a small tokio runtime talking to YouTube Music (`ytmapi`/`ytm`).
//!
//! The UI sends `Req`s and drains `Resp`s once per frame; every response wakes the UI.
//! Playback resolution also lives here: resolve stream → start progressive download →
//! build the decoder on a blocking thread → hand it to the audio engine.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crossbeam_channel::Sender;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, watch};

use crate::audio::{AudioCmd, AudioHandle};
use crate::lyrics::{self, LyricsSet};
use crate::model::{self, Card, Target, Track, sized};
use crate::stream::{self, Shared};
use crate::{hires, innertube, jsc, webmusic, ytdlp, ytm, ytmapi};

mod protocol;
pub use protocol::{ArtistPage, Channels, Collection, Explore, Library, PlaybackQuality, Req, Resp, SearchResults};

/// Likes and unlikes go one after another, this far apart. YouTube Music answers faster
/// ones as taken and then drops many: on a test account 9 of 20 unlikes sent back to back
/// stayed liked, against 1 of 20 sent 400 ms apart; on a limited account 576 of 1,296
/// likes sent back to back were dropped, and liking those again took as long as pacing.
pub const LIKE_GAP: std::time::Duration = std::time::Duration::from_millis(400);

/// One run of many likes at a time (a selection here, or a Spotify transfer): run side by
/// side, one undoes the other (a transfer skips songs as liked that an unlike run then
/// takes out).
pub static LIKES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long one like or unlike may take. Taken ones answer in about a quarter second;
/// ones YouTube Music refuses while it limits the account are held 8–9 s before the 403
/// (two of ten unlikes cost 17 of 27 s; in a transfer one like in six or so hangs).
/// Sending it again is harmless (liking a liked song changes nothing) and usually goes
/// straight through.
pub const LIKE_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Like or unlike one song; whether YouTube Music took it. Refusals are retried by the
/// API (after its shared pause while the account is limited); one that hangs or fails is
/// sent again, up to three times in all.
async fn like_one(api: &ytmapi::Api, id: &str, like: bool) -> bool {
    for tries in 0..3 {
        match tokio::time::timeout(LIKE_WAIT, ytm::set_like(api, id, like)).await {
            Ok(Ok(())) => return true,
            Ok(Err(e)) => log::debug!("like={like} {id}: {e:#}"),
            // Not refused yet: again right away.
            Err(_) => {
                log::debug!("like={like} {id}: no answer in {LIKE_WAIT:?}; sending it again");
                continue;
            }
        }
        if tries < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    false
}

/// Like or unlike a selection, one at a time (paced, see `LIKE_GAP`), then check
/// Liked Music and try again what didn't take. `progress(taken, tried)` after each song.
/// Returns the ones taken and the ones refused; unlikes still listed once checking stops
/// getting anywhere count as refused, likes don't (songs that can't be played are liked but not listed).
/// One run at a time, transfers included (`LIKES`); `progress(None, 0)` while waiting its turn.
pub async fn like_many(api: &ytmapi::Api, ids: Vec<String>, like: bool, mut progress: impl FnMut(Option<&str>, usize)) -> (Vec<String>, Vec<String>) {
    let epoch = api.epoch();
    let _turn = match LIKES.try_lock() {
        Ok(turn) => turn,
        Err(_) => {
            progress(None, 0);
            LIKES.lock().await
        }
    };
    // A channel switch went first: these were picked in the other channel's lists.
    if api.epoch() != epoch {
        log::info!("likes: {} not sent, the account or channel changed while they waited", ids.len());
        return (Vec::new(), ids);
    }
    let total = ids.len();
    let mut failed = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let taken = like_one(api, id, like).await;
        if !taken {
            failed.push(id.clone());
        }
        progress(taken.then_some(id.as_str()), i + 1);
        tokio::time::sleep(LIKE_GAP).await;
    }
    // Check and try again while it gets anywhere: a throttled account takes a few rounds.
    let (mut before, mut stuck) = (usize::MAX, 0);
    for round in 0..10 {
        if total < 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let listed: std::collections::HashSet<String> = match ytm::playlist(api, "LM", true, None).await {
            Ok(lm) => lm.tracks.into_iter().map(|t| t.id).collect(),
            Err(e) => {
                log::warn!("checking likes: {e:#}");
                break;
            }
        };
        let left: Vec<String> = ids.iter().filter(|id| listed.contains(*id) != like && !failed.contains(*id)).cloned().collect();
        log::debug!("like={like} check {round}: Liked Music lists {}, {} of {total} not taken yet", listed.len(), left.len());
        if left.is_empty() {
            break;
        }
        stuck = if left.len() < before { 0 } else { stuck + 1 };
        before = left.len();
        if stuck == 2 || round == 9 {
            if !like {
                failed.extend(left);
            }
            break;
        }
        // The second go is paced either way.
        for id in &left {
            like_one(api, id, like).await;
            tokio::time::sleep(LIKE_GAP).await;
        }
    }
    let done: Vec<String> = ids.into_iter().filter(|id| !failed.contains(id)).collect();
    (done, failed)
}

/// A video id and its stream buffer.
type StreamSlot = Arc<std::sync::Mutex<Option<(String, Arc<Shared>)>>>;
type SongCell = Arc<tokio::sync::OnceCell<Option<Track>>>;

struct Prefetch {
    listed: String,
    video_id: String,
    stream: oneshot::Receiver<anyhow::Result<PendingStream>>,
    promote: oneshot::Sender<()>,
}

#[derive(Default)]
struct PrefetchSlot {
    revision: u64,
    pending: Option<Prefetch>,
}

enum StreamUse {
    Play(u64),
    Prefetch(oneshot::Receiver<()>),
}

/// A stream still being prepared. Losing a source race, skipping or cancelling
/// also releases any blocking decoder waiting for bytes from this download.
struct PendingStream(Option<Arc<Shared>>);

impl PendingStream {
    fn new(shared: Arc<Shared>) -> Self {
        Self(Some(shared))
    }

    fn shared(&self) -> &Arc<Shared> {
        self.0.as_ref().unwrap()
    }

    fn into_shared(mut self) -> Arc<Shared> {
        self.0.take().unwrap()
    }

    async fn ready(self) -> anyhow::Result<Self> {
        let shared = self.shared();
        anyhow::ensure!(shared.total > 0, "empty audio stream");
        shared.wait_buffered(1).await;
        if let Some(error) = shared.error() {
            anyhow::bail!("{error}");
        }
        anyhow::ensure!(shared.has_buffered(1), "audio download stopped before startup");
        Ok(self)
    }
}

impl Drop for PendingStream {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            shared.cancel();
        }
    }
}

/// How long a play waits for a music video's song version before playing the video.
const PLAY_SONG_WAIT: std::time::Duration = std::time::Duration::from_millis(350);
/// A prefetch has until the song before it ends.
const PREFETCH_SONG_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long playback waits for the hi-res sources before falling back to YouTube. A cold
/// addon mints a lossless URL in a few seconds, and every source is asked at once.
const HIRES_WAIT: Duration = Duration::from_secs(18);
/// How long automatic playback gives hi-res priority over ready YouTube audio.
const HIRES_GRACE: Duration = Duration::from_millis(750);
const HIRES_DECODE_WAIT: Duration = Duration::from_secs(10);
/// Keep backups for an in-place swap if the best copy can't seek to the live position.
const MAX_PREPARED_ADDONS: usize = 3;

/// Prefer hi-res during the grace period. After it, use whichever source can play
/// first. A YouTube error can still be rescued by an addon.
async fn prefer_stream<T>(
    preferred: impl Future<Output = Option<T>>,
    standard: impl Future<Output = anyhow::Result<T>>,
    grace: impl Future<Output = ()>,
    fallback: impl FnOnce(),
) -> anyhow::Result<T> {
    tokio::pin!(preferred, standard, grace);
    let mut fallback = Some(fallback);
    loop {
        tokio::select! {
            biased;
            stream = &mut preferred => {
                if let Some(stream) = stream {
                    return Ok(stream);
                }
                if let Some(fallback) = fallback.take() {
                    fallback();
                }
                return standard.await;
            }
            stream = &mut standard => {
                return match stream {
                    Ok(stream) if fallback.is_some() => Ok(tokio::select! {
                        biased;
                        preferred = preferred => preferred.unwrap_or(stream),
                        _ = grace => stream,
                    }),
                    Ok(stream) => Ok(stream),
                    Err(error) => preferred.await.ok_or(error),
                };
            }
            _ = &mut grace, if fallback.is_some() => {
                fallback.take().unwrap()();
            }
        }
    }
}

async fn best_ready_sources<K: Ord, T>(
    mut sources: tokio::sync::mpsc::UnboundedReceiver<(K, T)>,
    wait: Duration,
) -> Vec<T> {
    let mut best = Vec::new();
    let _ = tokio::time::timeout(wait, async {
        while let Some(result) = sources.recv().await {
            best.push(result);
            best.sort_by(|a, b| b.0.cmp(&a.0));
            best.truncate(MAX_PREPARED_ADDONS);
        }
    }).await;
    best.into_iter().map(|(_, ready)| ready).collect()
}

fn seek_upgrade(decoder: &mut crate::audio::TrackDecoder, shared: &Shared, from: Duration, at: Duration) -> anyhow::Result<()> {
    anyhow::ensure!(shared.duration_ms == 0 || at.as_millis() < shared.duration_ms as u128, "addon stream is shorter than the playback position");
    if let Some(delta) = at.checked_sub(from)
        && delta <= Duration::from_secs(30)
        && matches!(shared.format, stream::Format::File { hint: "flac", .. })
    {
        // FLAC's random seek probes remote ranges even near the beginning. For a
        // short forward move, consume buffered samples and let the download read ahead.
        use rodio::Source;
        let rate = u128::from(decoder.sample_rate().get());
        let frames = at.as_nanos() * rate / 1_000_000_000 - from.as_nanos() * rate / 1_000_000_000;
        let samples = frames as usize * usize::from(decoder.channels().get());
        if samples > 0 {
            anyhow::ensure!(decoder.nth(samples - 1).is_some(), "addon stream ended before the playback position");
        }
        return Ok(());
    }
    rodio::Source::try_seek(decoder, at).map_err(|e| anyhow::anyhow!("seek: {e}"))?;
    Ok(())
}

#[derive(Clone)]
pub struct Backend {
    rt: tokio::runtime::Handle,
    api: Arc<ytmapi::Api>,
    tx: Sender<Resp>,
    audio: AudioHandle,
    wake: Arc<dyn Fn() + Send + Sync>,
    data_dir: PathBuf,
    lyrics: Arc<lyrics::Client>,
    translate: Arc<crate::translate::Client>,
    /// The upcoming track, including a lookup that is still in progress.
    prefetched: Arc<std::sync::Mutex<PrefetchSlot>>,
    play_generation: watch::Sender<u64>,
    /// The stream most recently started, kept so a replay of a fully downloaded song
    /// (repeat one, restart) doesn't download it again.
    current: StreamSlot,
    /// At most one background quality lookup for a play, including manual retries.
    upgrading: Arc<std::sync::Mutex<Option<(u64, bool)>>>,
    jsc: Arc<jsc::Solver>,
    /// Whether the signed-in account has Premium; checked once per sign-in. A failed
    /// check is retried no sooner than `PREMIUM_RETRY` later.
    premium: Arc<tokio::sync::Mutex<Result<bool, Option<std::time::Instant>>>>,
    /// High quality: with Premium, every song goes through the signed-in web player
    /// for its 256 kbps stream instead of the anonymous 128 kbps one.
    hq: Arc<std::sync::atomic::AtomicBool>,
    /// The listener's hi-res addons, in the order their links were given.
    hires: Arc<std::sync::RwLock<Vec<Arc<hires::Client>>>>,
    /// Ask the addons for Dolby Atmos renditions when they offer one.
    atmos: Arc<std::sync::atomic::AtomicBool>,
    /// Songs the listener sent back to YouTube audio: no addon copy until they leave.
    reverted: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Song versions of music videos looked up this session (`None`: there's none), by
    /// video and whether the account has Premium (which gets YouTube's own pairs). One
    /// cell each, so a prefetch and the play of the same video share a lookup.
    songs: Arc<std::sync::Mutex<std::collections::HashMap<String, SongCell>>>,
}

impl Backend {
    pub fn new(
        storage: PathBuf,
        tx: Sender<Resp>,
        audio: AudioHandle,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<(Self, Runtime)> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .enable_all()
            .build()?;
        std::fs::create_dir_all(&storage)?;
        let data_dir = storage;
        let http = crate::proxy::http();
        let lyrics = Arc::new(lyrics::Client::new(http.clone(), &data_dir));
        let translate = Arc::new(crate::translate::Client::new(http, &data_dir));
        let jsc = Arc::new(jsc::Solver::new(&data_dir));
        let api = Arc::new(ytmapi::Api::new(&data_dir));
        if crate::proxy::needs_relay()
            && let Err(e) = rt.block_on(crate::proxy::start_relay())
        {
            log::warn!("proxy relay: {e:#}");
        }
        let backend = Self { rt: rt.handle().clone(), api, tx, audio, wake, prefetched: Default::default(), play_generation: watch::Sender::new(0), current: Default::default(), upgrading: Default::default(), data_dir, lyrics, translate, jsc, premium: Arc::new(tokio::sync::Mutex::new(Err(None))), hq: Default::default(), hires: Default::default(), atmos: Default::default(), reverted: Default::default(), songs: Default::default() };
        backend.keep_signed_in();
        Ok((backend, rt))
    }

    /// Refresh the session's cookies every few minutes, as an open YouTube tab does:
    /// Google signs out a copy that stops taking new ones.
    fn keep_signed_in(&self) {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);
        let this = self.clone();
        self.rt.spawn(async move {
            loop {
                if this.api.account().is_some() {
                    this.api.ensure_visitor().await;
                    match this.api.rotate().await {
                        Ok(()) => log::debug!("session cookies refreshed"),
                        Err(e) if e.is::<webmusic::Revoked>() => this.check_session().await,
                        Err(e) => log::info!("refreshing session cookies failed: {e:#}"),
                    }
                }
                tokio::time::sleep(EVERY).await;
            }
        });
    }

    /// Something suggested Google signed the session out. When YouTube confirms it no
    /// longer knows the session, sign out here too and say so, rather than every
    /// signed-in page failing on its own.
    async fn check_session(&self) {
        // Several requests answered at once ask together; one check is enough.
        static CHECKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one = CHECKING.lock().await;
        if self.api.account().is_none() {
            return;
        }
        // Signing out deletes the session, so only once youtube.com shows it signed out
        // twice, a minute apart (each after a rotation, which can bring it back). One
        // signed-out answer can be a hiccup (throttling, a consent or "unusual traffic"
        // page), and a refused rotation alone isn't proof either: the clients whose
        // sessions last never sign out on their own.
        for attempt in 0..2 {
            if let Err(e) = self.api.rotate().await {
                log::info!("rotating cookies while checking the session: {e:#}");
            }
            let Some(account) = self.api.account() else { return };
            match self.api.session_from_cookie(&account.cookie).await {
                Ok(_) => return,
                Err(e) if e.is::<ytmapi::NotSignedIn>() => {
                    log::warn!("signed-in session looks signed out ({}): {e:#}", if attempt == 0 { "checking again in a minute" } else { "twice" });
                    if attempt == 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    }
                }
                // Offline, throttled or similar: nothing learned.
                Err(e) => {
                    log::info!("couldn't check the session: {e:#}");
                    return;
                }
            }
        }
        log::warn!("signed-in session no longer valid");
        self.forget_session().await;
        let _ = self.tx.send(Resp::LoggedIn(false));
        let _ = self.tx.send(Resp::Error("Your YouTube sign-in expired. Sign in again to get your library back".into()));
        self.wake();
    }

    /// Act as another of the account's channels. Not while likes of a selection or a
    /// transfer are running: they'd go on as the new channel.
    async fn switch_channel(&self, page_id: Option<String>, epoch: u64) -> anyhow::Result<u64> {
        let _likes = LIKES.try_lock().map_err(|_| anyhow::anyhow!("Wait until the likes going through are done"))?;
        // A Premium check running now is for the old channel: let it finish, then forget it.
        let mut premium = self.premium.lock().await;
        let epoch = self.api.switch_channel(page_id, epoch)?;
        *premium = Err(None);
        drop(premium);
        self.warm_signed_in();
        log::info!("switched channel");
        Ok(epoch)
    }

    async fn forget_session(&self) {
        self.api.sign_out();
        *self.premium.lock().await = Err(None);
        let profile = self.login_profile();
        let _ = tokio::task::spawn_blocking(move || crate::login::forget(&profile)).await;
    }

    pub fn api(&self) -> Arc<ytmapi::Api> {
        self.api.clone()
    }

    /// The shared client (see `proxy::http`).
    pub fn http(&self) -> reqwest::Client {
        crate::proxy::http()
    }

    pub fn spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        self.rt.spawn(f);
    }

    pub fn wake(&self) {
        (self.wake)()
    }

    pub fn set_play_generation(&self, generation: u64) {
        self.audio.status.wanted_gen.store(generation, Ordering::Relaxed);
        self.play_generation.send_replace(generation);
    }

    pub fn request(&self, req: Req) {
        let (what, quiet) = (req.name(), req.quiet());
        let mut prefetch_revision = 0;
        if let Req::Prefetch { video_id, generation, .. } = &req {
            let mut slot = self.prefetched.lock().unwrap();
            if self.audio.status.wanted_gen.load(Ordering::Relaxed) != *generation
                || slot.pending.as_ref().is_some_and(|p| p.listed == *video_id)
            {
                return;
            }
            slot.pending = None;
            slot.revision += 1;
            prefetch_revision = slot.revision;
        }
        let play_gen = match &req {
            Req::Play { generation, .. } => Some(*generation),
            _ => None,
        };
        let collection = match &req {
            Req::Album { id, load } | Req::Playlist { id, load, .. } => Some((id.clone(), *load)),
            _ => None,
        };
        let this = self.clone();
        let t0 = std::time::Instant::now();
        let epoch = self.api.epoch();
        let task = self.rt.spawn(async move {
            let r = this.handle(req, prefetch_revision).await;
            log::debug!("{what}: done in {:?}", t0.elapsed());
            r
        });
        let this = self.clone();
        self.rt.spawn(async move {
            // A panic in a handler (or a library it calls) fails just this request,
            // reported like any other error.
            let resp = match task.await {
                Ok(resp) => resp,
                Err(e) if e.is_panic() => {
                    log::error!("{what}: request panicked");
                    match play_gen {
                        Some(generation) => Some(Resp::PlayError { generation, msg: "YouTube didn't respond properly".into() }),
                        None => (!quiet).then(|| Resp::Error(format!("{what} failed: YouTube didn't respond properly"))),
                    }
                }
                Err(_) => None,
            };
            // An album or playlist load that failed ends too, so its page stops waiting.
            if let Some((id, load)) = collection
                && !matches!(resp, Some(Resp::Collection { .. } | Resp::CollectionMore { .. }))
            {
                let _ = this.tx.send(Resp::ended(id, load));
            }
            // Asked as another sign-in or channel than the one now used: it isn't this one's.
            let resp = resp.filter(|r| {
                let stale = this.api.epoch() != epoch && r.is_account_scoped();
                if stale {
                    log::debug!("{what}: answer for the previous account or channel dropped");
                }
                !stale
            });
            if let Some(resp) = resp {
                let _ = this.tx.send(resp);
                this.wake();
            }
            if this.api.take_signed_out() {
                this.check_session().await;
            }
        });
    }

    async fn handle(&self, req: Req, prefetch_revision: u64) -> Option<Resp> {
        let api = &*self.api;
        // Personalised pages ask as the account when signed in, like music.youtube.com.
        let me = self.is_logged_in();
        let (what, quiet) = (req.name(), req.quiet());
        let res: anyhow::Result<Option<Resp>> = async {
            Ok(Some(match req {
                Req::Home(params) => Resp::Home(ytm::home(api, params.as_deref(), me).await?),
                Req::HomeMore(token) => Resp::HomeMore(ytm::home_more(api, &token, me).await?),
                Req::MoreQuickPicks(seed) => {
                    let tracks = ytm::radio(api, &seed, me, 0).await?;
                    Resp::QuickPicksMore { seed, tracks }
                }
                Req::Explore(preferred_artists) => self.explore(&preferred_artists, me).await?,
                Req::Search(query) => self.search(query, me).await?,
                Req::Suggest { query, round } => {
                    let terms = ytm::suggestions(api, &query, me).await?.into_iter().take(8).collect();
                    Resp::Suggest { query, round, terms }
                }
                Req::ForgetSearch(token) => {
                    ytm::forget_search(api, &token).await?;
                    Resp::SearchForgotten
                }
                Req::Album { id, load } => Resp::Collection { load, c: ytm::album(api, &id).await?, more: false },
                Req::Playlist { id, load, stream: false } => Resp::Collection { load, c: ytm::playlist(api, &id, me, None).await?, more: false },
                Req::Playlist { id, load, stream: true } => self.stream_playlist(id, load, me).await?,
                Req::Artist(id) => Resp::Artist(ytm::artist(api, &id).await?),
                Req::Library => self.library().await?,
                Req::History => Resp::History(ytm::history(api).await?),
                Req::Uploads => Resp::Uploads(ytm::uploads(api).await?),
                Req::Account => {
                    let (name, photo) = ytm::account(api).await?;
                    Resp::Account { name, photo }
                }
                Req::Channels => {
                    let epoch = api.epoch();
                    let list = ytm::channels(api).await?;
                    Resp::Channels(Channels { list, current: api.account().and_then(|a| a.channel().map(str::to_owned)), epoch })
                }
                Req::SwitchChannel { page_id, epoch } => match self.switch_channel(page_id.clone(), epoch).await {
                    Ok(epoch) => Resp::SwitchedChannel { page_id, epoch, error: None },
                    Err(e) => {
                        log::warn!("switching channel failed: {e:#}");
                        Resp::SwitchedChannel { page_id, epoch, error: Some(e.to_string()) }
                    }
                },
                Req::ServerQueue => match ytm::server_queue(api).await? {
                    Some(q) => Resp::ServerQueue(q),
                    None => return Ok(None),
                },
                Req::Upload(files) => self.upload(&files).await?,
                Req::Lyrics(track) => {
                    let video_id = track.id.clone();
                    Resp::Lyrics { lyrics: self.lyrics_for(&track).await, video_id }
                }
                Req::Segments { video_id } => {
                    let segments = crate::sponsorblock::segments(&self.http(), &video_id).await?;
                    Resp::Segments { video_id, segments }
                }
                Req::LyricsExtras { video_id, source, lines, lang, target } => {
                    let key = format!("{video_id}_{source}");
                    let Some(extras) = self.translate.extras(&key, &lines, lang.as_deref(), &target).await else { return Ok(None) };
                    Resp::LyricsExtras { video_id, source, target, extras }
                }
                Req::Art { key, url } => {
                    // The backdrop is a 64 px blur; fetch near that, not the 544 px art.
                    let url = model::sized(&url, 2 * crate::art::SIZE);
                    let bytes = self.http().get(&url).send().await?.error_for_status()?.bytes().await?;
                    let art = tokio::task::spawn_blocking(move || crate::art::process(&bytes)).await??;
                    Resp::Art { key, art }
                }
                Req::AppleArtist { id, name } => {
                    let info = crate::apple::artist(&self.http(), &name).await;
                    log::info!("apple artist '{name}': {}", match &info {
                        Some(a) => match &a.logo {
                            Some(l) => format!("'{}' wordmark {}x{}", a.name, l.w, l.h),
                            None => format!("'{}' in type (no wordmark)", a.name),
                        },
                        None => "no answer".to_owned(),
                    });
                    Resp::AppleArtist { id, info }
                }
                Req::AppleCover { key, album, artist, title } => {
                    let motion = match title.as_deref() {
                        Some(t) => crate::apple::song_motion(&self.http(), t, &artist, Some(&album)).await,
                        None => crate::apple::album_motion(&self.http(), &album, &artist).await,
                    };
                    if motion.is_none() {
                        log::debug!("apple: no motion sleeve for '{album}' by '{artist}'");
                    }
                    Resp::AppleCover { key, motion }
                }
                Req::ArtistSignature { key, endpoint, name } => {
                    let signature = crate::artist_signature::lookup(&self.http(), &endpoint, &name).await;
                    log::info!("artist signature: {}", match &signature {
                        Some(s) => format!("found one for '{name}' ({}x{})", s.width, s.height),
                        None => format!("none for '{name}'"),
                    });
                    Resp::ArtistSignature { key, signature }
                }
                Req::Radio { seed, song_of } => {
                    // A music video's radio is music videos; its song's is songs, as from the
                    // song itself. The lookup is shared with its play's (one per video).
                    let song = self.song_version(&seed, song_of.as_ref(), PREFETCH_SONG_WAIT).await;
                    let from = song.as_ref().map_or(&seed, |s| &s.id);
                    let mut tracks = ytm::radio(api, from, false, 50).await?;
                    // The radio starts with its seed, which is playing already.
                    tracks.retain(|t| t.id != *from);
                    Resp::Radio { seed, tracks }
                }
                Req::PlayCollection { target } => {
                    let tracks = match &target {
                        Target::Album(id) => ytm::album(api, id).await?.tracks,
                        Target::Playlist(id) => ytm::playlist(api, id, me, None).await?.tracks,
                        Target::Artist(id) => ytm::artist(api, id).await?.top,
                        Target::Song(t) => vec![t.clone()],
                    };
                    Resp::PlayQueue { target, tracks }
                }
                Req::Download { track: t, for_video } => {
                    let path = self.download(&t, for_video).await?;
                    Resp::Downloaded { title: t.title, path }
                }
                Req::Play { video_id, generation, at, report, song_of, song, track, quality } => {
                    let mut wanted = self.play_generation.subscribe();
                    if *wanted.borrow_and_update() != generation {
                        return Ok(None);
                    }
                    tokio::select! {
                        biased;
                        _ = wanted.changed() => return Ok(None),
                        result = self.start_playback(&video_id, song_of.as_ref(), song, track.as_ref(), generation, at, report, quality) => {
                            match result {
                                Ok(()) => return Ok(None),
                                Err(e) => Resp::PlayError { generation, msg: format!("{e:#}") },
                            }
                        }
                    }
                }
                Req::Prefetch { video_id, generation, song_of, track } => {
                    self.prefetch(video_id, generation, song_of, track, prefetch_revision).await;
                    return Ok(None);
                }
                Req::Like { video_id, like } => {
                    ytm::set_like(api, &video_id, like).await?;
                    Resp::Liked { video_id, like }
                }
                Req::LikeMany { video_ids, like } => self.like_many(video_ids, like).await?,
                Req::Dislike(video_id) => {
                    ytm::dislike(api, &video_id).await?;
                    Resp::Liked { video_id, like: false }
                }
                Req::CreatePlaylist { title, video_ids } => {
                    let id = match video_ids.as_slice() {
                        [] => ytm::create_playlist(api, &title, None).await?,
                        [one] => ytm::create_playlist(api, &title, Some(one)).await?,
                        many => ytm::create_playlist_with(api, &title, many).await?,
                    };
                    Resp::PlaylistCreated { id, songs: video_ids.len() }
                }
                Req::EditPlaylist { id, title, description, privacy } => {
                    ytm::edit_playlist(api, &id, &title, &description, privacy.as_deref()).await?;
                    Resp::PlaylistChanged { id, message: "Playlist updated" }
                }
                Req::AddToPlaylist { playlist_id, video_ids } => {
                    ytm::add_videos(api, &playlist_id, &video_ids).await?;
                    Resp::PlaylistChanged { id: playlist_id, message: "Added to playlist" }
                }
                Req::RemoveFromPlaylist { playlist_id, items } => {
                    ytm::remove_videos(api, &playlist_id, &items).await?;
                    Resp::PlaylistChanged { id: playlist_id, message: "Removed from playlist" }
                }
                Req::DeletePlaylist(id) => {
                    ytm::delete_playlist(api, &id).await?;
                    Resp::PlaylistChanged { id, message: "Playlist deleted" }
                }
                Req::Collaboration(id) => Resp::Collaboration(ytm::collaboration(api, &id).await?),
                Req::SetCollaboration { info, change } => {
                    ytm::set_collaboration(api, &info, change).await?;
                    Resp::Collaboration(ytm::collaboration(api, &info.playlist_id).await?)
                }
                Req::SaveCollection { id, save } => {
                    ytm::save_collection(api, &id, save).await?;
                    Resp::PlaylistChanged { id, message: if save { "Saved to library" } else { "Removed from library" } }
                }
                Req::SetCookie(input) => {
                    self.set_cookie(&input).await?;
                    *self.premium.lock().await = Err(None);
                    self.warm_signed_in();
                    log::info!("signed in");
                    Resp::LoggedIn(true)
                }
                Req::Logout => {
                    self.forget_session().await;
                    Resp::LoggedIn(false)
                }
            }))
        }
        .await;
        res.unwrap_or_else(|e| {
            log::warn!("{what} failed: {e:#}");
            (!quiet).then(|| Resp::Error(format!("{what} failed: {e}")))
        })
    }

    /// Explore: the charts, and new albums by charting artists or the listener's own.
    async fn explore(&self, preferred_artists: &[String], me: bool) -> anyhow::Result<Resp> {
        let api = &*self.api;
        let (charts, new) = tokio::join!(ytm::charts(api), ytm::new_albums(api, me));
        let charts = charts?;
        Ok(Resp::Explore(Explore {
            new_albums: popular_new_albums(&new.unwrap_or_default(), &charts.artists, preferred_artists),
            charts: charts.playlists,
            top: charts.top,
        }))
    }

    /// Songs, albums, artists and playlists for a search, asked at once. Only the songs
    /// must arrive.
    async fn search(&self, query: String, me: bool) -> anyhow::Result<Resp> {
        let api = &*self.api;
        let (t, al, ar, pl) = tokio::join!(
            ytm::search_songs(api, &query, me),
            ytm::search_cards(api, &query, ytm::Filter::Albums, me),
            ytm::search_cards(api, &query, ytm::Filter::Artists, me),
            ytm::search_cards(api, &query, ytm::Filter::Playlists, me),
        );
        Ok(Resp::Search(SearchResults {
            tracks: t?,
            albums: al.unwrap_or_default(),
            artists: ar.unwrap_or_default(),
            playlists: pl.unwrap_or_default(),
            query,
        }))
    }

    /// Saved playlists, albums and podcasts.
    async fn library(&self) -> anyhow::Result<Resp> {
        let api = &*self.api;
        // Liked Music is loaded separately. Keep any library section that answers.
        let (pls, albums, podcasts) = tokio::join!(ytm::saved_playlists(api), ytm::saved_albums(api), ytm::saved_podcasts(api));
        let pls = match (pls, &albums, &podcasts) {
            (Err(e), Err(_), Err(_)) => return Err(e),
            (pls, _, _) => pls,
        };
        if let Err(e) = &pls {
            log::warn!("saved playlists didn't load: {e:#}");
        }
        if let Err(e) = &albums {
            log::warn!("saved albums didn't load: {e:#}");
        }
        if let Err(e) = &podcasts {
            log::warn!("saved podcasts didn't load: {e:#}");
        }
        Ok(Resp::Library(Library {
            playlists_failed: pls.is_err(),
            albums_failed: albums.is_err(),
            podcasts_failed: podcasts.is_err(),
            playlists: pls.map(|p| p.into_iter().filter(saved_playlist_visible).collect()).unwrap_or_default(),
            albums: albums.unwrap_or_default(),
            podcasts: podcasts.map(|p| p.into_iter().filter(saved_playlist_visible).collect()).unwrap_or_default(),
        }))
    }

    /// Upload songs, then refresh the uploads while YouTube Music processes them.
    async fn upload(&self, files: &[PathBuf]) -> anyhow::Result<Resp> {
        let api = &*self.api;
        let session = api.account().ok_or_else(|| anyhow::anyhow!("sign in to upload music"))?;
        let (mut done, mut failed) = (0, Vec::new());
        for f in files {
            let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match session.upload(&self.http(), f).await {
                Ok(()) => done += 1,
                Err(e) => {
                    log::warn!("upload {name}: {e:#}");
                    failed.push(name);
                }
            }
        }
        if done > 0 {
            // YouTube Music needs a little while to process new uploads.
            let this = self.clone();
            self.rt.spawn(async move {
                for wait in [20, 40, 60] {
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                    if let Ok(t) = ytm::uploads(&this.api).await {
                        let _ = this.tx.send(Resp::Uploads(t));
                        this.wake();
                    }
                }
            });
        }
        Ok(Resp::Uploaded { done, failed })
    }

    /// Open the next song's stream ahead of time, once the playing one has buffered enough.
    /// `song_of`: the music video it stands for, whose song version is what will play.
    /// `track`: the song as listed, for a hi-res source's search.
    async fn prefetch(&self, listed: String, generation: u64, song_of: Option<Track>, track: Option<Track>, revision: u64) {
        let mut wanted = self.play_generation.subscribe();
        if *wanted.borrow_and_update() != generation {
            return;
        }
        // Looked up now, so the play doesn't wait for it.
        let playing = self.current.lock().unwrap().as_ref().map(|(_, s)| s.clone());
        let prepare = async {
            let song = self.song_version(&listed, song_of.as_ref(), PREFETCH_SONG_WAIT).await;
            if let Some(s) = playing {
                s.wait_buffered(3).await;
            }
            song
        };
        let song = tokio::select! {
            biased;
            _ = wanted.changed() => return,
            song = prepare => song,
        };
        let meta = song.clone().or_else(|| song_of.clone()).or_else(|| track);
        let video_id = song.map_or_else(|| listed.clone(), |s| s.id);
        if self.current.lock().unwrap().as_ref().is_some_and(|(id, stream)| *id == video_id && stream.is_complete()) {
            return;
        }
        let (mut sender, stream) = oneshot::channel();
        let (promote, promoted) = oneshot::channel();
        {
            let mut slot = self.prefetched.lock().unwrap();
            if *wanted.borrow() != generation || slot.revision != revision {
                return;
            }
            slot.pending = Some(Prefetch { listed, video_id: video_id.clone(), stream, promote });
        }
        log::debug!("prefetch {video_id}");
        // The matching play takes the receiver. Dropping it for any other track
        // abandons this lookup and any downloads it has opened.
        tokio::select! {
            biased;
            _ = sender.closed() => {}
            result = self.open_stream(
                &video_id, meta.as_ref(), PlaybackQuality::Auto, StreamUse::Prefetch(promoted),
                |stream| std::future::ready(Ok(stream)),
            ) => {
                let _ = sender.send(result);
            }
        }
    }

    async fn lyrics_for(&self, t: &Track) -> LyricsSet {
        let artist = t.artist_line();
        let query = lyrics::Query {
            video_id: &t.id,
            song: &t.title,
            artist: &artist,
            album: t.album.as_ref().map(|a| a.name.as_str()),
            duration: match t.duration {
                Some(d) => Some(d),
                None => self.stream_duration(&t.id).await,
            },
        };
        let mut set = self.lyrics.fetch(&query).await.unwrap_or_else(|e| {
            log::warn!("lyrics failed: {e:#}");
            LyricsSet::default()
        });
        // Dev hook: YTM_LYRICS_FAIL=<source key>[,<key>…] makes those sources come back empty.
        if let Ok(fail) = std::env::var("YTM_LYRICS_FAIL") {
            set.sources.retain(|l| !fail.split(',').any(|k| k.trim() == l.provider.key()));
        }
        set
    }

    /// The song's length from its stream, for a track listed without one (some home,
    /// radio and queue rows). Without a duration Better Lyrics only finds plain lyrics,
    /// so wait for playback to resolve the stream, up to 15 s. The player's own duration
    /// counts too: starting a song clears it, and a restored session sets it.
    async fn stream_duration(&self, video_id: &str) -> Option<u32> {
        for _ in 0..150 {
            let ms = self.current.lock().unwrap().as_ref().filter(|(id, _)| id == video_id).map(|(_, s)| s.duration_ms);
            let ms = ms.filter(|ms| *ms > 0).or_else(|| self.audio.status.duration().map(|d| d.as_millis() as u64));
            if let Some(ms) = ms.filter(|ms| *ms > 0) {
                return Some((ms / 1000) as u32);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        log::info!("lyrics: no duration for {video_id}");
        None
    }

    /// Sign in with the youtube.com cookies the Google sign-in window hands over.
    async fn set_cookie(&self, cookie: &str) -> anyhow::Result<()> {
        let cookie = cookie.trim();
        anyhow::ensure!(!cookie.is_empty(), "no youtube.com cookies found");
        let session = self.api.session_from_cookie(cookie).await.inspect_err(|e| {
            // Names only, never values: enough to see whether SAPISID etc. arrived.
            let names: Vec<&str> = cookie.split(';').filter_map(|kv| kv.trim().split('=').next()).take(40).collect();
            log::warn!("cookie sign-in rejected: {e:#}; cookie names: {names:?}");
        })?;
        self.api.sign_in(session);
        // The sign-in window's copy of the session would go stale as the app takes new
        // cookies, and opening it again would show Google the old ones, which it treats
        // as stolen and signs the session out. Keep only the app's copy.
        let profile = self.login_profile();
        let _ = tokio::task::spawn_blocking(move || crate::login::forget(&profile)).await;
        Ok(())
    }

    async fn start_playback(&self, listed: &str, song_of: Option<&Track>, carried: Option<Track>, track: Option<&Track>, generation: u64, at: Duration, report: bool, quality: PlaybackQuality) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let was_carried = carried.is_some();
        let mut song = match carried {
            Some(song) => Some(song),
            None => self.song_version(listed, song_of, PLAY_SONG_WAIT).await,
        };
        let meta = song.as_ref().or(song_of).or(track);
        let mut video_id = song.as_ref().map_or(listed, |s| s.id.as_str()).to_owned();
        let (pending, decoder) = match self.prepare_play(&video_id, meta, generation, at, quality).await {
            Ok(ready) => ready,
            Err(e) if song.is_some() => {
                log::warn!("{listed}'s song {video_id} won't start ({e:#}); playing the music video");
                song = None;
                video_id = listed.to_owned();
                self.prepare_play(listed, track.or(song_of), generation, at, quality).await?
            }
            Err(e) => return Err(e),
        };
        if self.audio.status.wanted_gen.load(Ordering::Relaxed) != generation {
            return Ok(());
        }

        if song.is_some() {
            log::info!("play {listed} as its song {video_id}");
        }
        let upgrade_track = (quality == PlaybackQuality::Auto && pending.shared().info.is_none())
            .then(|| song.as_ref().or(song_of).or(track).cloned()).flatten();
        if quality == PlaybackQuality::PreferHiRes && pending.shared().info.is_none() {
            self.quality_unavailable(generation);
        }
        if was_carried != song.is_some() {
            let _ = self.tx.send(Resp::PlayingAs { generation, listed: listed.to_owned(), song });
        }
        let shared = pending.into_shared();
        *self.current.lock().unwrap() = Some((video_id.clone(), shared.clone()));
        self.publish_stream_info(generation, &shared, stream::Stats::of(&shared, &decoder));
        log::debug!("play {video_id} (gen {generation}): decoder ready at {at:?} in {:?}", started.elapsed());
        self.audio.send(AudioCmd::Start { generation, decoder, shared: shared.clone(), at });
        self.wake();

        if let Some(track) = upgrade_track {
            self.schedule_hires_upgrade(generation, video_id.clone(), track, shared, false);
        }
        if report && self.is_logged_in() && reports_plays() && self.audio.status.wanted_gen.load(Ordering::Relaxed) == generation {
            let this = self.clone();
            self.rt.spawn(async move {
                match ytm::report_play(&this.api, &video_id).await {
                    Ok(w) => {
                        log::debug!("reported play of {video_id}");
                        this.report_watchtime(&video_id, generation, w).await;
                    }
                    Err(e) => log::warn!("couldn't report play of {video_id}: {e:#}"),
                }
            });
        }
        Ok(())
    }

    fn publish_stream_info(&self, generation: u64, shared: &Shared, stats: stream::Stats) {
        if let Some(mut info) = shared.info.clone() {
            info.sample_rate = Some(stats.sample_rate);
            info.bit_depth = stats.bit_depth;
            info.channels = Some(stats.channels as u8);
            let _ = self.tx.send(Resp::AudioInfo { generation, info });
        }
        let _ = self.tx.send(Resp::StreamStats { generation, stats });
    }

    async fn prepare_play(&self, video_id: &str, track: Option<&Track>, generation: u64, at: Duration, quality: PlaybackQuality) -> anyhow::Result<(PendingStream, crate::audio::TrackDecoder)> {
        let reverted = self.reverted.lock().unwrap().contains(video_id);
        let replay = self.current.lock().unwrap().as_ref().and_then(|(id, shared)| {
            let suitable = match quality {
                PlaybackQuality::Auto => !reverted || shared.info.is_none(),
                PlaybackQuality::Standard => shared.info.is_none(),
                PlaybackQuality::PreferHiRes => shared.info.is_some(),
            };
            (id == video_id && suitable && shared.is_complete()).then(|| shared.clone())
        });
        let prefetched = {
            let mut slot = self.prefetched.lock().unwrap();
            match slot.pending.as_ref() {
                Some(p) if p.video_id == video_id && quality == PlaybackQuality::Auto && replay.is_none() => slot.pending.take(),
                Some(p) if p.video_id == video_id || replay.is_none() => {
                    slot.pending = None;
                    None
                }
                _ => None,
            }
        };
        let using_prefetch = prefetched.is_some();
        let prepared = match prefetched {
            Some(p) => {
                let _ = p.promote.send(());
                match p.stream.await {
                    Ok(Ok(stream)) if stream.shared().error().is_none() => Some(stream),
                    _ => None,
                }
            }
            None => replay.map(PendingStream::new),
        };
        let pending = match prepared {
            Some(stream) => {
                log::debug!("play {video_id} (gen {generation}): using buffered or prefetched stream");
                stream
            }
            None => return self.open_stream(
                video_id, track, quality, StreamUse::Play(generation),
                move |stream| Self::open_decoder(stream, at),
            ).await,
        };
        let addon = pending.shared().info.is_some();
        match Self::open_decoder(pending, at).await {
            Err(e) if addon || using_prefetch => {
                log::warn!("buffered audio for {video_id} could not start ({e:#}); reopening");
                self.open_stream(
                    video_id, track, quality, StreamUse::Play(generation),
                    move |stream| Self::open_decoder(stream, at),
                ).await
            }
            result => result,
        }
    }

    async fn open_decoder(pending: PendingStream, at: Duration) -> anyhow::Result<(PendingStream, crate::audio::TrackDecoder)> {
        let shared = pending.shared().clone();
        let addon = shared.info.is_some();
        let seek = !at.is_zero();
        let decode = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut decoder = stream::decoder(shared, seek)?;
            if seek {
                rodio::Source::try_seek(&mut decoder, at).map_err(|e| anyhow::anyhow!("seek: {e}"))?;
            }
            Ok(decoder)
        });
        let decoder = if addon {
            tokio::time::timeout(HIRES_DECODE_WAIT, decode).await
                .map_err(|_| anyhow::anyhow!("the hi-res decoder took too long to start"))?
        } else {
            decode.await
        }?.map_err(|e| anyhow::anyhow!("decode: {e}"))?;
        Ok((pending, decoder))
    }

    fn trying_standard(&self, generation: Option<u64>) {
        if let Some(generation) = generation {
            let _ = self.tx.send(Resp::TryingStandard { generation });
            self.wake();
        }
    }

    /// Report how much of play `generation` is heard, on the web player's schedule
    /// (10, 20 and 30 s in, then every 40 s), with a last report once it ends or is left.
    /// Each covers the stretch played since the one before; time paused doesn't count.
    async fn report_watchtime(&self, id: &str, generation: u64, w: ytm::Watchtime) {
        // Reported as the sign-in and channel that started it, or not at all.
        let epoch = self.api.epoch();
        let status = &self.audio.status;
        let started = std::time::Instant::now();
        let mut flush = w.flush_at.iter().copied();
        let mut due = flush.next().unwrap_or(w.flush_every);
        // The stretch not yet reported, and where the song was at the last check.
        let pos = || status.position().as_secs_f64();
        let (mut from, mut at) = (pos(), pos());
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let gone = status.wanted_gen.load(Ordering::Relaxed) != generation;
            if !gone {
                let now = pos();
                // A seek starts a new stretch.
                if now < at || now > at + 5.0 {
                    from = now;
                }
                at = now;
            }
            let elapsed = started.elapsed().as_secs();
            if !gone && elapsed < due {
                continue;
            }
            if self.api.epoch() != epoch {
                log::debug!("stopped reporting listening time of {id}: the account or channel changed");
                return;
            }
            if at > from || gone {
                if let Err(e) = ytm::report_watchtime(&self.api, &w, from, at, started.elapsed().as_secs_f64(), gone).await {
                    log::warn!("couldn't report listening time of {id}: {e:#}");
                }
                from = at;
            }
            if gone {
                log::debug!("reported listening time of {id}");
                return;
            }
            due = flush.next().unwrap_or(due + w.flush_every);
        }
    }

    /// Resolve a stream URL and start downloading it into a new buffer (just its start,
    /// for a prefetch, until playback opens it).
    /// A downloaded song opens from its file.
    /// The song version of the music video `listed`, when `song_of` asks for one: from
    /// this session's lookups, YouTube's own pair (Premium), or a sure signed-out search
    /// match. `None` when there's none, it isn't found within `wait`, or the music video
    /// is downloaded (that file plays, offline too). A failed lookup isn't remembered,
    /// so the next play tries again.
    async fn song_version(&self, listed: &str, song_of: Option<&Track>, wait: std::time::Duration) -> Option<Track> {
        let video = song_of.filter(|t| t.id == listed && t.music_video)?;
        if crate::downloads::path(listed).is_some() {
            return None;
        }
        // Its song version downloaded from Now playing: from the file, no lookup (offline too).
        if let Some(song) = crate::downloads::song_for(listed) {
            return Some(song);
        }
        let start = std::time::Instant::now();
        // Premium gets YouTube's own pair; a search needs the video's length to be sure.
        // The check counts toward the wait (the first one after launch asks YouTube): not
        // back in time, this lookup searches. It finishes anyway, for the next song.
        let premium = match self.api.account() {
            Some(session) => {
                let this = self.clone();
                let check = tokio::spawn(async move { this.is_premium(&session).await });
                tokio::time::timeout(wait, check).await.is_ok_and(|p| p.unwrap_or(false))
            }
            None => false,
        };
        if !premium && (video.duration.is_none() || video.artists.is_empty()) {
            return None;
        }
        let cell = {
            let mut songs = self.songs.lock().unwrap();
            let key = format!("{listed} {premium}");
            match songs.get(&key) {
                Some(cell) => cell.clone(),
                None => {
                    if songs.len() >= 1000 {
                        // Keeps the lookups still running.
                        songs.retain(|_, c| !c.initialized());
                    }
                    songs.entry(key).or_default().clone()
                }
            }
        };
        // Its own task: when the wait runs out the lookup still finishes, so the next play
        // of this video finds the song in the cell.
        let (this, video) = (self.clone(), video.clone());
        let lookup = tokio::spawn(async move {
            cell.get_or_try_init(|| async {
                let found = this.look_up_song(&video, premium).await?;
                match &found {
                    Some(s) => log::info!("song version of {} \"{}\": {} \"{}\"", video.id, video.title, s.id, s.title),
                    None => log::info!("song version of {} \"{}\": none found", video.id, video.title),
                }
                anyhow::Ok(found)
            })
            .await
            .cloned()
        });
        match tokio::time::timeout(wait.saturating_sub(start.elapsed()), lookup).await {
            Ok(Ok(Ok(found))) => found,
            Ok(Ok(Err(e))) => {
                log::debug!("song version of {listed}: {e:#}");
                None
            }
            Ok(Err(e)) => {
                log::debug!("song version of {listed}: lookup stopped: {e}");
                None
            }
            Err(_) => {
                log::debug!("song version of {listed}: not found in {wait:?}, playing the music video");
                None
            }
        }
    }

    async fn look_up_song(&self, video: &Track, premium: bool) -> anyhow::Result<Option<Track>> {
        if premium {
            match ytm::paired_song(&self.api, &video.id).await {
                // YouTube pairs every music video that has a song; trust it when it says none.
                Ok(found) => return Ok(found),
                Err(e) => log::debug!("song pair of {}: {e:#}; searching instead", video.id),
            }
        }
        ytm::searched_song(&self.api, video).await
    }

    /// A play races usable decoders. A prefetch races just the buffered heads.
    async fn open_stream<T: Send + 'static, F: Future<Output = anyhow::Result<T>> + Send + 'static>(
        &self, video_id: &str, track: Option<&Track>, quality: PlaybackQuality, use_for: StreamUse,
        prepare: impl Fn(PendingStream) -> F + Send + Sync + 'static,
    ) -> anyhow::Result<T> {
        let id = video_id.to_owned();
        let downloaded = tokio::task::spawn_blocking(move || crate::downloads::open(&id)).await?;
        if quality != PlaybackQuality::PreferHiRes && let Some(shared) = downloaded.as_ref() {
            return prepare(PendingStream::new(shared.clone())).await;
        }
        let prepare = Arc::new(prepare);
        let standard = async {
            let stream = match downloaded {
                Some(shared) => PendingStream::new(shared),
                None => {
                    let (source, stream) = self.resolve_youtube(video_id, false).await?;
                    let refresh = self.refresher(source, video_id, stream.format.clone());
                    self.download_stream(stream, refresh).ready().await?
                }
            };
            prepare(stream).await
        };
        let reverted = self.reverted.lock().unwrap().contains(video_id);
        let addons = self.hires.read().unwrap().clone();
        let Some(track) = track.filter(|_| quality != PlaybackQuality::Standard && !reverted && !addons.is_empty()) else {
            return standard.await;
        };
        let prepare_hires = prepare.clone();
        let preferred = async {
            self.from_hires(&addons, track, quality == PlaybackQuality::PreferHiRes,
                move |stream| prepare_hires(stream)).await.into_iter().next()
        };
        let generation = match &use_for {
            StreamUse::Play(generation) => Some(*generation),
            StreamUse::Prefetch(_) => None,
        };
        let grace = async {
            match (quality, use_for) {
                // The preferred candidate's own timeout bounds an explicit upgrade.
                (PlaybackQuality::PreferHiRes, _) => std::future::pending().await,
                (_, StreamUse::Prefetch(promoted)) => {
                    // A skip ends the background wait if YouTube is ready.
                    let _ = promoted.await;
                }
                _ => tokio::time::sleep(HIRES_GRACE).await,
            }
        };
        prefer_stream(preferred, standard, grace, || self.trying_standard(generation)).await
    }

    /// Start downloading only the head until the selected decoder opens the stream.
    fn download_stream(&self, stream: ResolvedStream, refresh: stream::Refresh) -> PendingStream {
        let s = stream;
        if let Some(data) = s.data {
            return PendingStream::new(Shared::from_bytes_with_format(data, s.duration_ms, s.loudness_db, s.format, s.info));
        }
        let shared = Shared::new(s.size, s.duration_ms, s.loudness_db, s.format.clone(), s.info, true);
        self.rt.spawn(stream::download(
            self.http(),
            s.url,
            s.user_agent,
            shared.clone(),
            Some(refresh),
            s.ranging,
        ));
        PendingStream::new(shared)
    }

    /// Resolves a fresh URL from the same source and format, for a download that got a 403.
    fn refresher(&self, source: Source, video_id: &str, format: stream::Format) -> stream::Refresh {
        let (this, id) = (self.clone(), video_id.to_owned());
        Arc::new(move || {
            let (this, id) = (this.clone(), id.clone());
            let format = format.clone();
            Box::pin(async move {
                let s = this.resolve_from(source, &id, format.clone()).await?;
                // The buffer is laid out for the format it started with.
                anyhow::ensure!(s.format == format, "the stream changed format");
                Ok(s.url)
            })
        })
    }

    /// Download a song into the downloads folder, at the best quality the account gets.
    async fn download(&self, t: &Track, for_video: Option<String>) -> anyhow::Result<PathBuf> {
        let (source, s) = self.resolve_youtube(&t.id, true).await?;
        log::info!("download {}: {} bytes from {source:?}", t.id, s.size);
        let shared = Shared::new(s.size, s.duration_ms, s.loudness_db, s.format.clone(), s.info, false);
        stream::download(self.http(), s.url, s.user_agent, shared.clone(), Some(self.refresher(source, &t.id, s.format)), s.ranging).await;
        if let Some(e) = shared.error() {
            anyhow::bail!("{e}");
        }
        let audio = shared.bytes().ok_or_else(|| anyhow::anyhow!("the download stopped early"))?;
        let cover = match model::pick(&t.thumbs, 120).map(|u| sized(u, 1200)) {
            Some(url) => async { self.http().get(url).send().await?.error_for_status()?.bytes().await }.await.ok().map(|b| b.to_vec()),
            None => None,
        };
        let (t, duration_ms, loudness) = (t.clone(), shared.duration_ms, shared.loudness_db);
        tokio::task::spawn_blocking(move || crate::downloads::save(&t, audio, duration_ms, loudness, cover, for_video)).await?
    }

    /// Resolve YouTube audio. Downloads use the best AAC the account can play.
    /// Playback uses Opus when available, with the signed-in Premium path for HQ.
    async fn resolve_youtube(&self, video_id: &str, best: bool) -> anyhow::Result<(Source, ResolvedStream)> {
        let hq = best || self.hq.load(Ordering::Relaxed);
        if best
            && !self.jsc.ready()
            && let Some(session) = self.api.account()
            && self.is_premium(&session).await
            && let Err(e) = self.jsc.warm(&self.http()).await
        {
            log::warn!("solver warm-up failed: {e:#}");
        }
        // High quality needs the signed-in player's solver. Until it's prepared (a new
        // install, a new player version: ~5 s or more), play standard quality at once.
        // Without Premium it's never prepared, and standard quality is all there is.
        let solver_ready = self.jsc.ready();
        if hq && !solver_ready && self.api.account().is_some() && !self.known_not_premium() {
            log::info!("{video_id}: solver not ready yet, standard quality for now");
            self.warm_signed_in();
        }
        if hq
            && solver_ready
            && let Some(session) = self.api.account()
            && self.is_premium(&session).await
        {
            match self.resolve_from(Source::WebMusic, video_id, stream::Format::Aac).await {
                Ok(s) => return Ok((Source::WebMusic, s)),
                // Not fatal: the usual 128 kbps stream still plays.
                Err(e) => log::warn!("high quality stream failed for {video_id} ({e:#}); using standard quality"),
            }
        }
        let opus = if best { stream::Format::Aac } else { stream::Format::Opus };
        match self.resolve_from(Source::VisionOs, video_id, opus).await {
            Ok(s) => Ok((Source::VisionOs, s)),
            // Age-restricted or Premium-only: only signed-in web players get it.
            Err(e) if needs_account(&e) => {
                log::info!("{video_id} needs a signed-in player ({e:#})");
                let session = self.api.account().ok_or_else(|| anyhow::anyhow!("sign in to play this song"))?;
                if self.is_premium(&session).await {
                    match self.resolve_from(Source::WebMusic, video_id, stream::Format::Aac).await {
                        Ok(s) => return Ok((Source::WebMusic, s)),
                        Err(e) => log::warn!("web music player failed ({e:#}); using yt-dlp"),
                    }
                }
                Ok((Source::YtDlp, self.resolve_from(Source::YtDlp, video_id, stream::Format::Aac).await?))
            }
            Err(e) => Err(e),
        }
    }

    /// `format`: the container the caller needs (the visionOS player picks Opus or AAC from it).
    async fn resolve_from(&self, source: Source, video_id: &str, format: stream::Format) -> anyhow::Result<ResolvedStream> {
        let plain = |s: innertube::Stream, ua: &str| ResolvedStream {
            url: s.url,
            size: s.size,
            duration_ms: s.duration_ms,
            user_agent: ua.to_owned(),
            loudness_db: s.loudness_db,
            format: stream::Format::from_opus(s.opus),
            info: None,
            ranging: stream::Ranging::Query,
            data: None,
        };
        Ok(match source {
            Source::VisionOs => plain(self.visionos_stream(video_id, format == stream::Format::Opus).await?, innertube::UA),
            Source::WebMusic => {
                let session = self.api.account().ok_or_else(|| anyhow::anyhow!("signed out"))?;
                plain(session.audio_stream(&self.http(), &self.jsc, video_id).await?, webmusic::UA)
            }
            Source::YtDlp => {
                let session = self.api.account().ok_or_else(|| anyhow::anyhow!("sign in to play this song"))?;
                let s = ytdlp::resolve(&self.http(), &self.data_dir, &session.cookie, video_id).await?;
                // Cookies Google replaced while yt-dlp used the session.
                self.api.keep(&session, s.replaced.iter().map(String::as_str));
                // yt-dlp's JSON doesn't carry YouTube's loudness figure.
                ResolvedStream { url: s.url, size: s.size, duration_ms: s.duration_ms, user_agent: s.user_agent, loudness_db: None, format: stream::Format::Aac, info: None, ranging: stream::Ranging::Query, data: None }
            }
        })
    }

    /// Try all sources concurrently. Compare only copies that can reach playback.
    async fn from_hires<T: Send + 'static, F: Future<Output = anyhow::Result<T>> + Send + 'static>(
        &self, sources: &[Arc<hires::Client>], track: &Track, fresh: bool,
        prepare: impl Fn(PendingStream) -> F + Clone + Send + Sync + 'static,
    ) -> Vec<T> {
        let atmos = self.atmos.load(Ordering::Relaxed);
        let mut tasks = tokio::task::JoinSet::new();
        let (send, receive) = tokio::sync::mpsc::unbounded_channel();
        let modes: &[bool] = if atmos { &[false, true] } else { &[false] };
        for ((index, client), &mode) in sources.iter().enumerate().flat_map(|source| modes.iter().map(move |mode| (source, mode))) {
            let (http, track, client) = (self.http(), track.clone(), client.clone());
            let (this, prepare, send) = (self.clone(), prepare.clone(), send.clone());
            tasks.spawn(async move {
                let found = client.find(&http, &track, mode, fresh, |found, refresh| {
                    let qualifies = found.info.lossless() || found.info.atmos
                        || found.info.bitrate_kbps.is_some_and(|rate| rate > 256);
                    let pending = this.download_stream(ResolvedStream {
                        url: found.url,
                        size: found.size,
                        duration_ms: found.duration_ms,
                        user_agent: hires::USER_AGENT.to_owned(),
                        loudness_db: None,
                        format: found.format,
                        info: Some(found.info),
                        ranging: stream::Ranging::Header,
                        data: found.data,
                    }, refresh);
                    let (prepare, send) = (&prepare, &send);
                    async move {
                        anyhow::ensure!(qualifies, "the addon did not provide higher-quality audio");
                        let (rank, ready) = tokio::time::timeout(HIRES_DECODE_WAIT, async {
                            let pending = pending.ready().await?;
                            let mut info = pending.shared().info.clone().unwrap();
                            if let Some((rate, depth)) = pending.shared().flac_quality() {
                                info.sample_rate = Some(rate);
                                info.bit_depth = Some(depth);
                            }
                            let rank = (info.preference(atmos), std::cmp::Reverse(index));
                            Ok::<_, anyhow::Error>((rank, prepare(pending).await?))
                        }).await??;
                        let _ = send.send((rank, ready));
                        Ok(())
                    }
                }).await;
                match found {
                    Ok(true) => {}
                    Ok(false) => log::info!("hi-res {}: no copy of \"{}\"", client.host(), track.title),
                    Err(e) => log::info!("hi-res {}: {e:#}", client.host()),
                }
            });
        }
        drop(send);
        best_ready_sources(receive, HIRES_WAIT).await
    }

    /// Get the signed-in playback path ready in the background, so the first Premium-only
    /// or age-restricted song doesn't wait for the Premium check or the solver setup that
    /// each new YouTube player version needs (~5 s, every few days).
    pub fn warm_signed_in(&self) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(session) = this.api.account() else { return };
            if this.is_premium(&session).await
                && let Err(e) = this.jsc.warm(&this.http()).await
            {
                log::warn!("solver warm-up failed: {e:#}");
            }
        });
    }

    /// The Premium check has answered, and the account doesn't have it. Doesn't ask or
    /// wait: while a check is running, it's not known yet.
    fn known_not_premium(&self) -> bool {
        self.premium.try_lock().is_ok_and(|p| matches!(*p, Ok(false)))
    }

    /// Premium accounts can stream signed-in web player URLs without a PO token.
    async fn is_premium(&self, session: &webmusic::Session) -> bool {
        const PREMIUM_RETRY: std::time::Duration = std::time::Duration::from_secs(300);
        let mut premium = self.premium.lock().await;
        match *premium {
            Ok(p) => return p,
            Err(Some(at)) if at.elapsed() < PREMIUM_RETRY => return false,
            Err(_) => {}
        }
        match session.is_premium(&self.http()).await {
            Ok(p) => {
                log::info!("account has Premium: {p}");
                *premium = Ok(p);
                p
            }
            Err(e) => {
                log::warn!("Premium check failed: {e:#}");
                *premium = Err(Some(std::time::Instant::now()));
                false
            }
        }
    }

    async fn visionos_stream(&self, video_id: &str, opus: bool) -> anyhow::Result<innertube::Stream> {
        let vd = self.api.visitor_data(false).await?;
        match innertube::audio_stream(&self.http(), video_id, &vd, opus).await {
            Ok(s) => Ok(s),
            Err(e) if needs_account(&e) => Err(e),
            Err(e) => {
                // A stale/flagged visitor ID is the usual cause; retry once with a fresh one.
                log::info!("visionOS player: {e:#}; retrying with new visitor data");
                let vd = self.api.visitor_data(true).await?;
                innertube::audio_stream(&self.http(), video_id, &vd, opus).await
            }
        }
    }

    /// Webview profile used by the sign-in window.
    pub fn login_profile(&self) -> PathBuf {
        self.data_dir.join("webview")
    }

    /// Report an error from outside the request flow (e.g. the sign-in window).
    pub fn report_error(&self, msg: String) {
        let _ = self.tx.send(Resp::Error(msg));
        self.wake();
    }

    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    pub fn set_high_quality(&self, on: bool) {
        if self.hq.swap(on, Ordering::Relaxed) != on {
            self.clear_prefetch();
        }
    }

    /// Prefer a Dolby Atmos rendition from the hi-res sources when they offer one.
    pub fn set_prefer_atmos(&self, on: bool) {
        if self.atmos.swap(on, Ordering::Relaxed) != on {
            self.clear_prefetch();
        }
    }

    /// Send a song back to YouTube audio: later plays of it skip the hi-res sources.
    pub fn revert_quality(&self, video_id: &str) {
        self.reverted.lock().unwrap().insert(video_id.to_owned());
    }

    /// Allow hi-res sources again for a song that was sent back to YouTube audio.
    pub fn upgrade_quality(&self, video_id: &str) {
        self.reverted.lock().unwrap().remove(video_id);
    }

    /// The manual quality action can try the addon without restarting a playing song.
    pub fn try_upgrade_active(&self, generation: u64, track: Track) -> bool {
        if !self.audio.status.started(generation) {
            return false;
        }
        let current = self.current.lock().unwrap().clone();
        let Some((id, standard)) = current.filter(|(id, _)| *id == track.id) else { return false };
        self.schedule_hires_upgrade(generation, id, track, standard, true)
    }

    fn schedule_hires_upgrade(&self, generation: u64, id: String, track: Track, standard: Arc<Shared>, manual: bool) -> bool {
        if standard.info.as_ref().is_some_and(|i| i.lossless() || i.atmos)
            || self.reverted.lock().unwrap().contains(&id)
            || (!manual && crate::downloads::path(&id).is_some())
        {
            return false;
        }
        let addons = self.hires.read().unwrap().clone();
        if addons.is_empty() {
            return false;
        }
        {
            let mut upgrading = self.upgrading.lock().unwrap();
            if *self.play_generation.borrow() != generation {
                return false;
            }
            if let Some((active, ref mut notify)) = *upgrading && active == generation {
                *notify |= manual;
                return true;
            }
            *upgrading = Some((generation, manual));
        }
        self.upgrade_status(generation, true);
        let this = self.clone();
        self.rt.spawn(async move {
            let mut fresh = manual;
            loop {
                let upgraded = this.upgrade_in_background(generation, &id, &track, standard.clone(), &addons, fresh).await;
                let mut upgrading = this.upgrading.lock().unwrap();
                let Some((active, notify)) = *upgrading else { break };
                if active != generation {
                    break;
                }
                *upgrading = None;
                if *this.play_generation.borrow() != generation {
                    break;
                }
                if notify && !upgraded && !fresh {
                    *upgrading = Some((generation, true));
                    fresh = true;
                    continue;
                }
                this.upgrade_status(generation, false);
                if notify && !upgraded {
                    this.quality_unavailable(generation);
                }
                break;
            }
        });
        true
    }

    fn upgrade_status(&self, generation: u64, active: bool) {
        let _ = self.tx.send(Resp::UpgradingQuality { generation, active });
        self.wake();
    }

    fn quality_unavailable(&self, generation: u64) {
        let _ = self.tx.send(Resp::QualityUnavailable { generation });
        self.wake();
    }

    async fn upgrade_in_background(&self, generation: u64, id: &str, track: &Track, standard: Arc<Shared>, addons: &[Arc<hires::Client>], manual: bool) -> bool {
        let mut wanted = self.play_generation.subscribe();
        if *wanted.borrow_and_update() != generation {
            return false;
        }
        let prefer_atmos = self.atmos.load(Ordering::Relaxed);
        let prepare = async {
            let audio = self.audio.status.clone();
            let prepare_audio = audio.clone();
            let candidates = self.from_hires(addons, track, manual, move |pending| {
                let shared = pending.shared().clone();
                let decode_shared = shared.clone();
                let audio = prepare_audio.clone();
                async move {
                    let (decoder, at) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        let mut decoder = stream::decoder(decode_shared.clone(), true)?;
                        let at = if audio.started(generation) { audio.position_smooth() } else { Duration::ZERO };
                        if !at.is_zero() {
                            seek_upgrade(&mut decoder, &decode_shared, Duration::ZERO, at)?;
                        }
                        Ok((decoder, at))
                    }).await??;
                    shared.wait_read_ahead(3).await?;
                    Ok((pending, decoder, at))
                }
            }).await;
            anyhow::ensure!(!candidates.is_empty(), "no addon had a playable copy");
            // Let the outgoing song finish before seeking to the live position.
            tokio::time::timeout(Duration::from_secs(50), async {
                while !audio.started(generation) || audio.transitioning() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }).await?;
            for (pending, mut decoder, prepared_at) in candidates {
                let shared = pending.shared().clone();
                if shared.error().is_some() {
                    continue;
                }
                let audio = audio.clone();
                let seek = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                    let mut at = prepared_at;
                    for _ in 0..3 {
                        let position = audio.position_smooth();
                        if at != position {
                            seek_upgrade(&mut decoder, &shared, at, position)?;
                            at = position;
                        }
                        if at.abs_diff(audio.position_smooth()) < Duration::from_millis(80) {
                            break;
                        }
                    }
                    anyhow::ensure!(at.abs_diff(audio.position_smooth()) < Duration::from_millis(120), "addon seek couldn't catch up");
                    Ok((decoder, at))
                });
                if let Ok(Ok(Ok((decoder, at)))) = tokio::time::timeout(HIRES_DECODE_WAIT, seek).await {
                    return Ok((pending, decoder, at));
                }
            }
            anyhow::bail!("no addon copy could seek to the playback position")
        };
        let (pending, decoder, at) = tokio::select! {
            biased;
            _ = wanted.changed() => return false,
            result = prepare => match result {
                Ok(ready) => ready,
                Err(e) => {
                    log::debug!("quality upgrade for {id} kept the original stream: {e:#}");
                    return false;
                }
            }
        };
        if *wanted.borrow() != generation
            || self.reverted.lock().unwrap().contains(id)
            || self.atmos.load(Ordering::Relaxed) != prefer_atmos
            || {
                let current = self.hires.read().unwrap();
                current.len() != addons.len() || !current.iter().zip(addons).all(|(a, b)| Arc::ptr_eq(a, b))
            }
        {
            return false;
        }
        let shared = pending.shared().clone();
        if let (Some(new), Some(old)) = (&shared.info, &standard.info)
            && new.preference(prefer_atmos) <= old.preference(prefer_atmos)
        {
            return false;
        }
        let stats = stream::Stats::of(&shared, &decoder);
        let (accepted, reply) = oneshot::channel();
        self.audio.send(AudioCmd::Upgrade { generation, expected: standard.clone(), decoder, shared, at, accepted });
        if reply.await != Ok(true) {
            return false;
        }
        let shared = pending.into_shared();
        let mut current = self.current.lock().unwrap();
        if *wanted.borrow() == generation && current.as_ref().is_some_and(|(_, old)| Arc::ptr_eq(old, &standard)) {
            self.publish_stream_info(generation, &shared, stats);
            *current = Some((id.to_owned(), shared));
        }
        true
    }

    /// Point the hi-res sources at the links in `links`, one per line (empty: none). The
    /// links themselves aren't logged: they can carry a token.
    pub fn set_hires_source(&self, links: &str) {
        let mut clients: Vec<Arc<hires::Client>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut rejected = 0;
        for line in links.lines() {
            let line = line.trim();
            if line.is_empty() || !seen.insert(line.to_owned()) {
                continue;
            }
            match hires::Client::new(line) {
                Some(client) => clients.push(Arc::new(client)),
                None => rejected += 1,
            }
        }
        let hosts: Vec<String> = clients.iter().map(|c| c.host()).collect();
        match clients.len() {
            0 if rejected == 0 => log::info!("hi-res sources: none"),
            0 => log::warn!("hi-res sources: none usable ({rejected} not an http(s) link)"),
            1 => log::info!("hi-res source set: {}", hosts[0]),
            n => log::info!("{n} hi-res sources set: {}", hosts.join(", ")),
        }
        if rejected > 0 && !clients.is_empty() {
            log::warn!("hi-res sources: {rejected} entry(s) aren't http(s) links and were skipped");
        }
        *self.hires.write().unwrap() = clients;
        self.clear_prefetch();
    }

    fn clear_prefetch(&self) {
        let mut slot = self.prefetched.lock().unwrap();
        slot.revision += 1;
        slot.pending = None;
    }

    /// Like or unlike a selection, one at a time, then check Liked Music and try again
    /// what didn't take. Unlikes still listed after that count as refused; likes aren't
    /// counted so (songs that can't be played are liked but not listed).
    async fn like_many(&self, ids: Vec<String>, like: bool) -> anyhow::Result<Resp> {
        let total = ids.len();
        let (done, failed) = like_many(&self.api, ids, like, |taken, n| {
            let _ = self.tx.send(Resp::LikeProgress { taken: taken.into_iter().map(str::to_owned).collect(), done: n, total, like });
            self.wake();
        })
        .await;
        // Even when all were refused: the app stops waiting on them and shows them as they are.
        Ok(Resp::LikedMany { video_ids: done, failed, like })
    }

    /// Send a playlist's first page right away, then each next page as it arrives
    /// (YouTube takes about half a second per page of 100). The last one is returned.
    async fn stream_playlist(&self, id: String, load: u64, me: bool) -> anyhow::Result<Resp> {
        let (c, mut token, limit) = ytm::playlist_start(&self.api, &id, me, None).await?;
        let mut have = c.tracks.len();
        // A podcast's later pages carry no header: its episodes are credited from the first.
        let show = if ytm::is_podcast(&id) { c.artists.clone() } else { Vec::new() };
        let mut resp = Resp::Collection { load, c, more: token.is_some() && have < limit };
        while let Some(t) = token.take().filter(|_| have < limit) {
            let _ = self.tx.send(resp);
            self.wake();
            // Failing here, the page keeps what came.
            let (mut tracks, mut set_ids, next) = ytm::playlist_more(&self.api, &t, me).await?;
            ytm::credit_episodes(&mut tracks, &show);
            tracks.truncate(limit - have);
            set_ids.truncate(limit - have);
            have += tracks.len();
            token = next.filter(|_| !tracks.is_empty());
            resp = Resp::CollectionMore { id: id.clone(), load, more: token.is_some() && have < limit, tracks, set_ids };
        }
        Ok(resp)
    }

    pub fn is_logged_in(&self) -> bool {
        self.api.account().is_some()
    }
}

/// Test instances (`YTM_BACKGROUND`) keep plays out of the account's history unless
/// `YTM_REPORT_PLAYS` is set.
fn reports_plays() -> bool {
    std::env::var_os("YTM_BACKGROUND").is_none() || std::env::var_os("YTM_REPORT_PLAYS").is_some()
}

#[derive(Clone, Copy, Debug)]
enum Source {
    VisionOs,
    WebMusic,
    YtDlp,
}

struct ResolvedStream {
    url: String,
    size: u64,
    duration_ms: u64,
    user_agent: String,
    loudness_db: Option<f32>,
    format: stream::Format,
    /// What the player can say about a hi-res stream (codec, rate, depth, size).
    info: Option<hires::Info>,
    /// How this source takes byte ranges.
    ranging: stream::Ranging,
    /// Audio already in memory, such as decoded Atmos or a file with no length header.
    data: Option<Vec<u8>>,
}

/// The anonymous visionOS player refused because the song needs an account:
/// age-restricted (LOGIN_REQUIRED) or Music Premium only.
fn needs_account(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("LOGIN_REQUIRED") || msg.contains("Premium")
}

/// Keep the signed-in release feed's order, using US charts and the user's home artists.
fn popular_new_albums(albums: &[Card], ranked_artists: &[String], preferred_artists: &[String]) -> Vec<Card> {
    let chart_ids: std::collections::HashSet<&str> = ranked_artists.iter().chain(preferred_artists).map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    albums
        .iter()
        .filter(|album| album.links.iter().any(|a| a.id.as_deref().is_some_and(|id| chart_ids.contains(id))))
        .filter(|album| seen.insert(match &album.target {
            Target::Album(id) => id.as_str(),
            _ => album.title.as_str(),
        }))
        .take(16)
        .cloned()
        .collect()
}

/// "Episodes for later" is listed even when empty; hide it then.
fn saved_playlist_visible(card: &Card) -> bool {
    card.target != Target::Playlist("SE".into()) || card.subtitle.split_whitespace().any(|w| w.parse::<u64>().is_ok_and(|n| n > 0))
}

pub fn compact(n: u64) -> String {
    let (v, unit) = match n {
        0..1_000 => return n.to_string(),
        1_000..1_000_000 => (n as f64 / 1e3, "K"),
        1_000_000..1_000_000_000 => (n as f64 / 1e6, "M"),
        _ => (n as f64 / 1e9, "B"),
    };
    // "6M", not "6.0M"; three significant figures and up need no decimal.
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
}

// Larger art URL for the background / now-playing view.
pub fn art_url(t: &Track) -> Option<String> {
    model::pick(&t.thumbs, 120).map(|u| sized(u, 544))
}

#[cfg(test)]
mod explore_tests {
    use super::*;

    fn card(title: &str, target: Target, artist_id: Option<&str>, subtitle: &str) -> Card {
        Card {
            title: title.into(),
            subtitle: subtitle.into(),
            thumbs: vec![],
            target,
            round: false,
            links: artist_id.map(|id| model::Link { name: "Artist".into(), id: Some(id.into()) }).into_iter().collect(),
        }
    }

    #[test]
    fn new_releases_only_include_charting_artists_and_deduplicate() {
        let album = |id: &str, artist_id: &str| card(id, Target::Album(id.into()), Some(artist_id), "Album");
        let albums = vec![album("obscure", "other"), album("favorite-release", "favorite"), album("first-release", "popular"), album("second-release", "popular"), album("first-release", "popular")];
        let found = popular_new_albums(&albums, &["popular".into(), "popular".into()], &["favorite".into()]);
        assert_eq!(found.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["favorite-release", "first-release", "second-release"]);
    }

    #[test]
    fn empty_episodes_for_later_is_hidden() {
        let item = |id: &str, subtitle: &str| card("Episodes for later", Target::Playlist(id.into()), None, subtitle);
        assert!(!saved_playlist_visible(&item("SE", "Auto playlist")));
        assert!(!saved_playlist_visible(&item("SE", "Auto playlist • 0 episodes")));
        assert!(saved_playlist_visible(&item("SE", "Auto playlist • 2 episodes")));
        assert!(saved_playlist_visible(&item("PLcustom", "Playlist")));
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use std::future::{pending, ready};

    #[tokio::test]
    async fn best_playable_source_wins_even_if_another_finishes_first() {
        let (send, receive) = tokio::sync::mpsc::unbounded_channel();
        send.send((1, "CD lossless")).unwrap();
        let slower = send.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            slower.send((2, "hi-res lossless")).unwrap();
        });
        assert_eq!(best_ready_sources(receive, Duration::from_millis(25)).await, ["hi-res lossless", "CD lossless"]);
    }

    #[tokio::test]
    async fn stalled_hires_does_not_hold_ready_youtube() {
        let chosen = tokio::time::timeout(
            Duration::from_millis(200),
            prefer_stream(pending(), ready(Ok("YouTube")), tokio::time::sleep(Duration::from_millis(20)), || {}),
        )
        .await
        .expect("a stalled addon delayed an already playable stream")
        .unwrap();
        assert_eq!(chosen, "YouTube");
    }

    #[tokio::test]
    async fn fast_hires_keeps_priority() {
        let chosen = prefer_stream(
            ready(Some("lossless")),
            ready(Ok("YouTube")),
            tokio::time::sleep(Duration::from_millis(20)),
            || {},
        )
        .await
        .unwrap();
        assert_eq!(chosen, "lossless");
    }

    #[tokio::test]
    async fn hires_can_play_if_youtube_is_unavailable() {
        let hires = async {
            tokio::task::yield_now().await;
            Some("lossless")
        };
        let chosen = prefer_stream(
            hires,
            ready(Err(anyhow::anyhow!("unavailable"))),
            ready(()),
            || {},
        )
        .await
        .unwrap();
        assert_eq!(chosen, "lossless");
    }

    #[tokio::test]
    async fn skipping_to_a_prefetch_uses_its_ready_youtube_stream() {
        let (promote, promoted) = oneshot::channel();
        let choose = prefer_stream(pending(), ready(Ok("YouTube")), async {
            promoted.await.unwrap();
        }, || {});
        tokio::pin!(choose);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(choose.as_mut().poll(&mut cx).is_pending());
        promote.send(()).unwrap();
        assert_eq!(choose.await.unwrap(), "YouTube");
    }

    #[tokio::test]
    async fn dropping_an_unselected_stream_releases_its_reader() {
        let shared = Shared::new(1024, 0, None, stream::Format::Aac, None, true);
        let pending = PendingStream::new(shared.clone());
        let reader = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            stream::StreamReader::new(shared).read(&mut [0; 1])
        });
        drop(pending);
        let error = tokio::time::timeout(Duration::from_secs(1), reader).await.unwrap().unwrap().unwrap_err();
        assert_eq!(error.to_string(), "download cancelled");
    }
}
