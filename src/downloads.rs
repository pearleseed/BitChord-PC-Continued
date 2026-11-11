//! Songs saved to disk: the audio as YouTube serves it (AAC in an .m4a, at the best
//! quality the account gets), tagged with title, artists, album and cover, in the
//! downloads folder. A downloaded song plays from its file, so it works offline.
//!
//! `downloads.json` in the data dir maps video ids to their files; a file that was
//! moved or deleted just stops counting as downloaded.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::model::Track;
use crate::stream::{Format, Shared};

const LIST_REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    path: PathBuf,
    duration_ms: u64,
    loudness_db: Option<f32>,
    /// A song version: the music video it was downloaded for, and the song, so that
    /// video plays it from this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stands_for: Option<String>,
    /// The song as it was downloaded, for the Downloads page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    song: Option<Track>,
}

impl Entry {
    fn open(&self) -> Option<Arc<Shared>> {
        let data = std::fs::read(&self.path).ok()?;
        // Legacy entries have no format field. Sniff the EBML signature for WebM.
        let format = if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) { Format::Opus } else { Format::Aac };
        Some(Shared::from_bytes_with_format(data, self.duration_ms, self.loudness_db, format, None))
    }
}

struct State {
    index: HashMap<String, Entry>,
    /// Where the index is saved (the data dir's downloads.json).
    file: PathBuf,
    folder: PathBuf,
    listed: Mutex<Option<DownloadList>>,
}

struct DownloadList {
    checked_at: Instant,
    tracks: Arc<[Track]>,
}

impl State {
    fn list(&self, now: Instant) -> (Arc<[Track]>, Option<Duration>) {
        let mut listed = self.listed.lock().unwrap();
        // Detect files changed outside BitChord without hitting the disk on every frame.
        if let Some(snapshot) = listed.as_ref()
            && let age = now.duration_since(snapshot.checked_at)
            && age < LIST_REFRESH
        {
            return (snapshot.tracks.clone(), Some(LIST_REFRESH - age));
        }
        let mut present: Vec<_> = self
            .index
            .values()
            .filter_map(|entry| {
                let track = entry.song.as_ref()?;
                let metadata = std::fs::metadata(&entry.path).ok()?;
                metadata.is_file().then(|| (metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), track))
            })
            .collect();
        present.sort_by(|a, b| b.0.cmp(&a.0));
        let tracks: Arc<[Track]> = present.into_iter().map(|(_, t)| t.clone()).collect();
        *listed = Some(DownloadList { checked_at: now, tracks: tracks.clone() });
        (tracks, None)
    }

    fn set_index(&mut self, index: HashMap<String, Entry>) {
        self.index = index;
        *self.listed.get_mut().unwrap() = None;
    }
}

static STATE: LazyLock<RwLock<State>> =
    LazyLock::new(|| RwLock::new(State { index: HashMap::new(), file: PathBuf::new(), folder: default_folder(), listed: Default::default() }));

/// Music\BitChord.
pub fn default_folder() -> PathBuf {
    let base = directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(Path::to_path_buf).or_else(|| Some(u.home_dir().join("Music"))))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("BitChord")
}

/// Load the index; `folder` is the saved setting (empty for the default).
pub fn init(data_dir: &Path, folder: &str) {
    let file = data_dir.join("downloads.json");
    let index = match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("could not read download index {}: {error}", file.display());
            HashMap::new()
        }),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!("could not read download index {}: {error}", file.display());
            }
            HashMap::new()
        }
    };
    let mut st = STATE.write().unwrap();
    st.set_index(index);
    st.file = file;
    st.folder = if folder.is_empty() { default_folder() } else { PathBuf::from(folder) };
}

pub fn folder() -> PathBuf {
    STATE.read().unwrap().folder.clone()
}

pub fn set_folder(folder: PathBuf) {
    STATE.write().unwrap().folder = folder;
}

/// Ask for a new folder (the system picker, on its own thread).
pub fn pick_folder(ctx: &egui::Context) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(dir) = rfd::FileDialog::new().set_title("Download folder").set_directory(folder()).pick_folder() {
            set_folder(dir);
            ctx.request_repaint();
        }
    });
}

pub fn open_folder() {
    let dir = folder();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Open the song's folder in Explorer with the file selected.
pub fn show_file(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }
    // macOS: Finder reveals it.
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    // Linux: the file manager's ShowItems selects it (Nautilus, Dolphin, Nemo, …);
    // without one, just open the folder.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let uri = url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_default();
        let shown = std::process::Command::new("dbus-send")
            .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems"])
            .arg(format!("array:string:{uri}"))
            .arg("string:")
            .output()
            .is_ok_and(|o| o.status.success());
        if !shown && let Some(dir) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}

/// The song's file, if it's downloaded and still there.
pub fn path(video_id: &str) -> Option<PathBuf> {
    STATE.read().unwrap().index.get(video_id).map(|e| e.path.clone()).filter(|p| p.is_file())
}

/// The downloaded songs still on disk, newest first.
pub fn list(ctx: &egui::Context) -> Arc<[Track]> {
    let (tracks, expires_in) = STATE.read().unwrap().list(Instant::now());
    // A repaint that reused the snapshot may have missed an external file change.
    // A fresh scan does not schedule another repaint.
    if let Some(delay) = expires_in {
        ctx.request_repaint_after(delay);
    }
    tracks
}

/// The downloaded song version of the music video `video_id`, if its file is still there.
pub fn song_for(video_id: &str) -> Option<Track> {
    let st = STATE.read().unwrap();
    st.index.values().find(|e| e.stands_for.as_deref() == Some(video_id) && e.path.is_file()).and_then(|e| e.song.clone())
}

/// A downloaded song's audio, as a complete stream buffer for playback.
pub fn open(video_id: &str) -> Option<Arc<Shared>> {
    let entry = STATE.read().unwrap().index.get(video_id).cloned()?;
    let audio = entry.open()?;
    log::info!("{video_id}: playing the downloaded file");
    Some(audio)
}

/// Write a downloaded song into the folder and remember it. Returns the file.
/// `stands_for`: the music video `t` is the song version of.
pub fn save(t: &Track, mut audio: Vec<u8>, duration_ms: u64, loudness_db: Option<f32>, cover: Option<Vec<u8>>, stands_for: Option<String>) -> anyhow::Result<PathBuf> {
    // File naming and index publication are one transaction for concurrent saves.
    static SAVING: Mutex<()> = Mutex::new(());
    let _save = SAVING.lock().unwrap();
    let (folder, index_file, mut index) = {
        let st = STATE.read().unwrap();
        (st.folder.clone(), st.file.clone(), st.index.clone())
    };
    std::fs::create_dir_all(&folder)?;
    let ext = if audio.get(4..8) == Some(b"ftyp") { "m4a" } else { "webm" };
    let mine = index.get(&t.id).map(|e| &e.path);
    let stem = file_stem(t);
    let mut path = folder.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while path.exists() && Some(&path) != mine {
        path = folder.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    let replace = Some(&path) == mine;

    let mut part = tempfile::NamedTempFile::new_in(&folder)?;
    // Change YouTube's DASH brand to M4A for players that rely on it.
    if audio.get(8..12) == Some(b"dash") {
        audio[8..12].copy_from_slice(b"M4A ");
    }
    part.write_all(&audio)?;
    drop(audio);
    if ext == "m4a" && let Err(e) = tag(part.path(), t, cover) {
        log::warn!("couldn't tag {}: {e:#}", path.display());
    }

    index.insert(t.id.clone(), Entry {
        path: path.clone(), duration_ms, loudness_db, stands_for, song: Some(t.clone()),
    });
    let index_dir = index_file.parent().ok_or_else(|| anyhow::anyhow!("download index is not initialized"))?;
    let mut new_index = tempfile::NamedTempFile::new_in(index_dir)?;
    serde_json::to_writer(&mut new_index, &index)?;
    new_index.as_file().sync_all()?;

    if replace {
        part.persist(&path)?;
    } else {
        part.persist_noclobber(&path)?;
    }
    if let Err(error) = new_index.persist(index_file) {
        // The old index still points to an existing replacement. For a new entry,
        // remove the unpublished file so a retry uses the same filename.
        if !replace {
            let _ = std::fs::remove_file(&path);
        }
        return Err(error.into());
    }
    STATE.write().unwrap().set_index(index);
    Ok(path)
}

/// "Artists - Title", safe as a Windows file name.
fn file_stem(t: &Track) -> String {
    let artists = t.artist_line();
    let name = if artists.is_empty() { t.title.clone() } else { format!("{artists} - {}", t.title) };
    let clean: String = name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches('.').trim();
    let clean: String = clean.chars().take(150).collect();
    if clean.is_empty() { t.id.clone() } else { clean }
}

fn tag(path: &Path, t: &Track, cover: Option<Vec<u8>>) -> anyhow::Result<()> {
    let mut tag = mp4ameta::Tag::read_from_path(path)?;
    tag.set_title(&t.title);
    for (i, a) in t.artists.iter().enumerate() {
        if i == 0 {
            tag.set_artist(&a.name);
        } else {
            tag.add_artist(&a.name);
        }
    }
    if let Some(a) = t.artists.first() {
        tag.set_album_artist(&a.name);
    }
    if let Some(album) = &t.album {
        tag.set_album(&album.name);
    }
    if let Some(n) = t.track_nr {
        tag.set_track_number(n);
    }
    let img = cover.and_then(|c| match c.get(..4) {
        Some([0xff, 0xd8, ..]) => Some(mp4ameta::Img::jpeg(c)),
        Some([0x89, b'P', b'N', b'G']) => Some(mp4ameta::Img::png(c)),
        _ => None,
    });
    if let Some(img) = img {
        tag.set_artwork(img);
    }
    tag.write_to_path(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Link;

    #[test]
    fn download_list_reuses_the_snapshot_and_refreshes_external_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.m4a");
        std::fs::write(&path, b"audio").unwrap();
        let song: Track = serde_json::from_value(serde_json::json!({
            "id": "song", "title": "Song", "artists": [], "thumbs": []
        })).unwrap();
        let mut state = State {
            index: HashMap::from([("song".into(), Entry {
                path: path.clone(), duration_ms: 0, loudness_db: None,
                stands_for: None, song: Some(song),
            })]),
            file: PathBuf::new(),
            folder: dir.path().to_owned(),
            listed: Default::default(),
        };
        let now = Instant::now();
        let (first, refresh) = state.list(now);
        assert_eq!(first[0].id, "song");
        assert_eq!(refresh, None);
        std::fs::remove_file(path).unwrap();
        let (cached, refresh) = state.list(now + Duration::from_millis(250));
        assert!(Arc::ptr_eq(&first, &cached));
        assert_eq!(refresh, Some(Duration::from_millis(750)));
        let (expired, refresh) = state.list(now + LIST_REFRESH);
        assert!(expired.is_empty());
        assert_eq!(refresh, None);
        std::fs::write(&state.index["song"].path, b"audio").unwrap();
        state.set_index(state.index.clone());
        assert_eq!(state.list(now + LIST_REFRESH).0[0].id, "song");
    }

    #[test]
    fn old_download_index_reopens_webm_as_opus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("download.webm");
        std::fs::write(&path, [0x1a, 0x45, 0xdf, 0xa3]).unwrap();
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "path": path, "duration_ms": 3200, "loudness_db": -2.0
        })).unwrap();
        let audio = entry.open().unwrap();
        assert_eq!(audio.format, crate::stream::Format::Opus);
        assert_eq!(audio.duration_ms, 3200);
        assert_eq!(audio.loudness_db, Some(-2.0));
        assert_eq!(audio.bytes().unwrap(), [0x1a, 0x45, 0xdf, 0xa3]);
    }

    #[test]
    fn concurrent_saves_keep_distinct_files_and_both_index_entries() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        init(dir.path(), music.to_str().unwrap());
        let track = |id: &str| Track {
            id: id.into(), title: "Shared title".into(), artists: vec![], album: None,
            duration: None, thumbs: vec![], track_nr: None, plays: None, music_video: false,
        };
        let paths = std::thread::scope(|scope| {
            let one = scope.spawn(|| save(&track("one"), vec![1], 0, None, None, None).unwrap());
            let two = scope.spawn(|| save(&track("two"), vec![2], 0, None, None, None).unwrap());
            [one.join().unwrap(), two.join().unwrap()]
        });
        assert_ne!(paths[0], paths[1]);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), [1]);
        assert_eq!(std::fs::read(&paths[1]).unwrap(), [2]);
        let index: HashMap<String, Entry> =
            serde_json::from_slice(&std::fs::read(dir.path().join("downloads.json")).unwrap()).unwrap();
        assert_eq!(index["one"].path, paths[0]);
        assert_eq!(index["two"].path, paths[1]);
    }

    #[test]
    fn file_names_are_safe_on_windows() {
        let track = |title: &str, artists: Vec<Link>| Track {
            id: "abc".into(),
            title: title.into(),
            artists,
            album: None,
            duration: None,
            thumbs: Vec::new(),
            track_nr: None,
            plays: None,
            music_video: false,
        };
        let t = track("What? / Why: \"Now\"...", vec![Link { name: "AC/DC".into(), id: None }]);
        assert_eq!(file_stem(&t), "AC_DC - What_ _ Why_ _Now_");
        assert_eq!(file_stem(&track("...", Vec::new())), "abc");
    }
}
