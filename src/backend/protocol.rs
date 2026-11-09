//! Typed messages between the network runtime and the desktop UI.

use std::{path::PathBuf, time::Duration};

use crate::art::Art;
use crate::lyrics::LyricsSet;
use crate::model::{self, Card, Target, Thumb, Track};
use crate::{stream, ytm};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlaybackQuality {
    Auto,
    Standard,
    PreferHiRes,
}

pub enum Req {
    /// Home feed, optionally filtered by a mood chip's params.
    Home(Option<String>),
    HomeMore(String),
    MoreQuickPicks(String),
    Explore(Vec<String>),
    Search(String),
    /// Suggestions for a query; `round` comes back with them, so stale ones can be dropped.
    Suggest {
        query: String,
        round: u64,
    },
    /// Remove a past search (its token) from the account's history.
    ForgetSearch(String),
    /// `load` comes back with the album, so an older answer can't replace a newer one.
    Album {
        id: String,
        load: u64,
    },
    Artist(String),
    /// `stream`: send the first page as soon as it's in and the rest as they come,
    /// rather than all of it at the end.
    Playlist {
        id: String,
        load: u64,
        stream: bool,
    },
    Library,
    History,
    /// Songs the account uploaded to YouTube Music.
    Uploads,
    /// The signed-in account's name and photo.
    Account,
    /// The channels the signed-in Google account can act as.
    Channels,
    /// Act as another of them from now on: a brand channel's page id, or `None` for the
    /// account's own channel. `epoch`: the list's, picked from.
    SwitchChannel {
        page_id: Option<String>,
        epoch: u64,
    },
    /// The queue another device left off with (YouTube Music's own cross-device queue).
    ServerQueue,
    /// Upload audio files to the account's library.
    Upload(Vec<PathBuf>),
    /// Save a song's audio to the downloads folder. `for_video`: the music video it's the
    /// song version of, which then plays it from the file (offline too).
    Download {
        track: Track,
        for_video: Option<String>,
    },
    Lyrics(Track),
    /// SponsorBlock's parts of a video to skip.
    Segments {
        video_id: String,
    },
    /// Romanization and translation for the lyric lines of one source.
    LyricsExtras {
        video_id: String,
        source: String,
        lines: Vec<String>,
        lang: Option<String>,
        target: String,
    },
    /// Process cover art for the background; `key` is echoed back.
    Art {
        key: String,
        url: String,
    },
    /// Apple's artist page (display name and hero clips) for the artist `id`.
    AppleArtist {
        id: String,
        name: String,
    },
    /// Apple's motion sleeve for a release; `key` is what the UI asked for.
    AppleCover {
        key: String,
        album: String,
        artist: String,
        title: Option<String>,
    },
    /// A handwritten signature for `name` from the service at `endpoint`; `key` is
    /// what the UI asked for.
    ArtistSignature {
        key: String,
        endpoint: String,
        name: String,
    },
    /// Radio seeded from a track (`seed`, its id); appended to the queue. `song_of`: the
    /// track, when it's a music video to play as its song: the song's radio is asked for.
    Radio {
        seed: String,
        song_of: Option<Track>,
    },
    /// Fetch an album/playlist and play it from the start.
    PlayCollection {
        target: Target,
    },
    /// `at`: start position (non-zero when resuming last session's song). `report`: tell
    /// YouTube about the play (not when picking up a song already counted). `song_of`:
    /// the track, when it's a music video to play as its song version if one is found.
    /// `song`: the song version already playing for the music video `video_id` (picked up
    /// mid-song): played without a lookup, and the video if it won't. `track`: the song as
    /// the UI lists it, for a hi-res source's search.
    Play {
        video_id: String,
        generation: u64,
        at: Duration,
        report: bool,
        song_of: Option<Track>,
        song: Option<Track>,
        track: Option<Track>,
        quality: PlaybackQuality,
    },
    /// Resolve and download a track ahead of time so skipping to it is instant.
    Prefetch {
        video_id: String,
        generation: u64,
        song_of: Option<Track>,
        track: Option<Track>,
    },
    Like {
        video_id: String,
        like: bool,
    },
    /// Several likes (or unlikes) one after another, for a selection.
    LikeMany {
        video_ids: Vec<String>,
        like: bool,
    },
    Dislike(String),
    CreatePlaylist {
        title: String,
        video_ids: Vec<String>,
    },
    EditPlaylist {
        id: String,
        title: String,
        description: String,
        privacy: Option<String>,
    },
    AddToPlaylist {
        playlist_id: String,
        video_ids: Vec<String>,
    },
    /// `(videoId, setVideoId)` entries to take out of the playlist.
    RemoveFromPlaylist {
        playlist_id: String,
        items: Vec<(String, String)>,
    },
    DeletePlaylist(String),
    /// A playlist's collaboration settings and invite link.
    Collaboration(String),
    SetCollaboration {
        info: ytm::Collaboration,
        change: ytm::CollabChange,
    },
    SaveCollection {
        id: String,
        save: bool,
    },
    SetCookie(String),
    Logout,
}

impl Req {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Req::Home(_) | Req::HomeMore(_) => "Loading home",
            Req::MoreQuickPicks(_) => "Loading more picks",
            Req::Explore(_) => "Loading new",
            Req::Search(_) => "Search",
            Req::Suggest { .. } => "Suggest",
            Req::ForgetSearch(_) => "Removing the search",
            Req::Album { .. } => "Loading album",
            Req::Artist(_) => "Loading artist",
            Req::Playlist { .. } => "Loading playlist",
            Req::Library => "Loading library",
            Req::History => "Loading history",
            Req::Uploads => "Loading uploads",
            Req::Account => "Account",
            Req::Channels => "Channels",
            Req::SwitchChannel { .. } => "Switching channel",
            Req::ServerQueue => "Queue from other devices",
            Req::Upload(_) => "Upload",
            Req::Download { .. } => "Download",
            Req::Lyrics(_) => "Lyrics",
            Req::Segments { .. } => "SponsorBlock",
            Req::LyricsExtras { .. } => "Translation",
            Req::Art { .. } => "Art",
            Req::AppleArtist { .. } => "Apple artist",
            Req::AppleCover { .. } => "Apple artwork",
            Req::ArtistSignature { .. } => "Artist signature",
            Req::Radio { .. } => "Loading radio",
            Req::PlayCollection { .. } => "Playback",
            Req::Play { .. } => "Playback",
            Req::Prefetch { .. } => "Prefetch",
            Req::Like { .. } | Req::LikeMany { .. } => "Like",
            Req::Dislike(_) => "Dislike",
            Req::CreatePlaylist { .. } => "Create playlist",
            Req::EditPlaylist { .. } => "Edit playlist",
            Req::AddToPlaylist { .. } => "Add to playlist",
            Req::RemoveFromPlaylist { .. } => "Remove from playlist",
            Req::DeletePlaylist(_) => "Delete playlist",
            Req::Collaboration(_) => "Loading collaboration",
            Req::SetCollaboration { .. } => "Collaboration",
            Req::SaveCollection { .. } => "Save to library",
            Req::SetCookie(_) => "Sign in",
            Req::Logout => "Sign out",
        }
    }

    /// Background requests fail quietly (logged, no toast).
    pub(super) fn quiet(&self) -> bool {
        matches!(
            self,
            Req::Suggest { .. }
                | Req::Account
                | Req::Channels
                | Req::ServerQueue
                | Req::Art { .. }
                | Req::AppleArtist { .. }
                | Req::AppleCover { .. }
                | Req::ArtistSignature { .. }
                | Req::Prefetch { .. }
                | Req::HomeMore(_)
                | Req::Lyrics(_)
                | Req::Segments { .. }
                | Req::LyricsExtras { .. }
        )
    }
}

pub struct Explore {
    pub new_albums: Vec<Card>,
    pub charts: Vec<Card>,
    pub top: Vec<Track>,
}

pub struct SearchResults {
    pub query: String,
    pub tracks: Vec<Track>,
    pub albums: Vec<Card>,
    pub artists: Vec<Card>,
    pub playlists: Vec<Card>,
}

/// An album or playlist page.
pub struct Collection {
    pub id: String,
    pub is_album: bool,
    /// Album's playlist ID for saving it to the account library.
    pub save_id: Option<String>,
    pub title: String,
    pub subtitle: String,
    /// Album artists / playlist owner named in the subtitle, clickable.
    pub artists: Vec<model::Link>,
    pub description: Option<String>,
    pub thumbs: Vec<Thumb>,
    pub tracks: Vec<Track>,
    /// Each track's playlist entry id (`setVideoId`), for removing it; empty for albums.
    pub set_ids: Vec<Option<String>>,
    /// How many songs the playlist's header counts (unplayable ones too, so it can be more
    /// than `tracks`); kept in step with removals and unlikes made here.
    pub count: Option<u64>,
    /// The account's own playlist (editable), not one saved from someone else.
    pub owned: bool,
}

impl Collection {
    /// The subtitle, with the song count for a playlist.
    pub fn byline(&self) -> String {
        match self.count {
            Some(n) if self.subtitle.is_empty() => format!("{n} songs"),
            Some(n) => format!("{} • {n} songs", self.subtitle),
            None => self.subtitle.clone(),
        }
    }

    /// Take rows out (sorted indices), and out of the count.
    pub fn remove_rows(&mut self, rows: &[usize]) {
        for &i in rows.iter().rev() {
            if i < self.tracks.len() {
                self.tracks.remove(i);
                if i < self.set_ids.len() {
                    self.set_ids.remove(i);
                }
                self.count = self.count.map(|n| n.saturating_sub(1));
            }
        }
    }
}

pub struct ArtistPage {
    pub id: String,
    pub name: String,
    pub banner: Vec<Thumb>,
    pub subscribers: Option<u64>,
    pub description: Option<String>,
    pub top: Vec<Track>,
    /// "Top songs" playlist behind the shelf's Show all button.
    pub top_playlist: Option<String>,
    pub albums: Vec<Card>,
    pub playlists: Vec<Card>,
    pub similar: Vec<Card>,
}

pub struct Library {
    pub playlists: Vec<Card>,
    pub albums: Vec<Card>,
    pub podcasts: Vec<Card>,
    /// That half didn't load (YouTube Music refused it, say): the one shown stays.
    pub playlists_failed: bool,
    pub albums_failed: bool,
    pub podcasts_failed: bool,
}

/// The signed-in account's channels.
pub struct Channels {
    pub list: Vec<ytm::Channel>,
    /// The page id of the one requests go as (`None`: the account's own).
    pub current: Option<String>,
    /// [`ytmapi::Api::epoch`] when listed: a switch picked from here applies only then.
    pub epoch: u64,
}

pub enum Resp {
    Home(ytm::HomePage),
    HomeMore(ytm::HomePage),
    QuickPicksMore {
        seed: String,
        tracks: Vec<Track>,
    },
    Explore(Explore),
    Search(SearchResults),
    Suggest {
        query: String,
        round: u64,
        terms: Vec<ytm::Suggestion>,
    },
    /// An album or playlist; `more`: the rest of its songs follow in `CollectionMore`.
    Collection {
        load: u64,
        c: Collection,
        more: bool,
    },
    /// The next page of a playlist sent as `Collection` with the same `load`.
    CollectionMore {
        id: String,
        load: u64,
        tracks: Vec<Track>,
        set_ids: Vec<Option<String>>,
        more: bool,
    },
    Artist(ArtistPage),
    Library(Library),
    History(Vec<Track>),
    Uploads(Vec<Track>),
    Account {
        name: Option<String>,
        photo: Option<String>,
    },
    Channels(Channels),
    /// A `Req::SwitchChannel` is done (requests go as `epoch` now), or `error` says why it isn't.
    SwitchedChannel {
        page_id: Option<String>,
        epoch: u64,
        error: Option<String>,
    },
    ServerQueue(ytm::ServerQueue),
    /// Upload finished: how many files went up, and which failed.
    Uploaded {
        done: usize,
        failed: Vec<String>,
    },
    Downloaded {
        title: String,
        path: PathBuf,
    },
    /// Every source's lyrics for a song (empty when nobody has them).
    Lyrics {
        video_id: String,
        lyrics: LyricsSet,
    },
    Segments {
        video_id: String,
        segments: Vec<crate::sponsorblock::Segment>,
    },
    /// Romanization/translation for one source's lines, arriving after them.
    LyricsExtras {
        video_id: String,
        source: String,
        target: String,
        extras: crate::translate::Extras,
    },
    Art {
        key: String,
        art: Art,
    },
    /// Apple's page for an artist, when there is one (`None`: no match, or no token).
    AppleArtist {
        id: String,
        info: Option<crate::apple::Artwork>,
    },
    /// Apple's motion sleeve for a release; `key` is what the UI asked it for.
    AppleCover {
        key: String,
        motion: Option<crate::apple::Motion>,
    },
    /// A signature for an artist, already whitened for the header.
    ArtistSignature {
        key: String,
        signature: Option<crate::artist_signature::Signature>,
    },
    Radio {
        seed: String,
        tracks: Vec<Track>,
    },
    /// What was asked to play (a `Req::PlayCollection`), and its songs.
    PlayQueue {
        target: Target,
        tracks: Vec<Track>,
    },
    Liked {
        video_id: String,
        like: bool,
    },
    /// A run of likes or unlikes: `taken` just went through, `done` of `total` tried.
    LikeProgress {
        taken: Vec<String>,
        done: usize,
        total: usize,
        like: bool,
    },
    /// The songs YouTube Music took, and the ones it refused.
    LikedMany {
        video_ids: Vec<String>,
        failed: Vec<String>,
        like: bool,
    },
    PlaylistCreated {
        id: String,
        songs: usize,
    },
    PlaylistChanged {
        id: String,
        message: &'static str,
    },
    Collaboration(ytm::Collaboration),
    /// Playback for `generation` failed before reaching the audio engine.
    PlayError {
        generation: u64,
        msg: String,
    },
    TryingStandard {
        generation: u64,
    },
    /// What `generation` is playing from a hi-res source (codec, rate, depth, size).
    AudioInfo {
        generation: u64,
        info: crate::hires::Info,
    },
    StreamStats {
        generation: u64,
        stats: stream::Stats,
    },
    UpgradingQuality {
        generation: u64,
        active: bool,
    },
    /// Play `generation` is the song version of the music video that was asked for
    /// (`None`: that song wouldn't start, and the music video plays after all).
    PlayingAs {
        generation: u64,
        listed: String,
        song: Option<Track>,
    },
    LoggedIn(bool),
    /// A past search left the account's history; the list already shows it gone.
    SearchForgotten,
    Error(String),
}

impl Resp {
    /// An answer that belongs to the signed-in account and channel it was asked as.
    pub(super) fn is_account_scoped(&self) -> bool {
        matches!(
            self,
            Resp::Home(_)
                | Resp::HomeMore(_)
                | Resp::QuickPicksMore { .. }
                | Resp::Suggest { .. }
                | Resp::Library(_)
                | Resp::History(_)
                | Resp::Uploads(_)
                | Resp::Account { .. }
                | Resp::Channels(_)
                | Resp::ServerQueue(_)
                | Resp::Liked { .. }
                | Resp::LikeProgress { .. }
                | Resp::LikedMany { .. }
        )
    }

    /// An album or playlist load that ends without more songs: also sent when it fails,
    /// so the page stops waiting for it.
    pub(super) fn ended(id: String, load: u64) -> Self {
        Resp::CollectionMore {
            id,
            load,
            tracks: Vec::new(),
            set_ids: Vec::new(),
            more: false,
        }
    }
}
