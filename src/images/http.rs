//! Bounded download, disk cache and decoding for remote artwork.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use egui::ColorImage;
use egui::load::{ImageLoadResult, ImageLoader, ImagePoll, LoadError, SizeHint};
use image::ImageDecoder;
use tokio::sync::Semaphore;

use crate::backend::Backend;

const RETRY_AFTER: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_IN_FLIGHT: usize = 8;
const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_PIXELS: u32 = 2048 * 2048;
const MAX_ENTRIES: usize = 512;
const DECODED_BUDGET: usize = 24 * 1024 * 1024;
const DISK_MAX_FILE: usize = 256 * 1024;
const DISK_BUDGET: u64 = 64 * 1024 * 1024;
const TRIM_EVERY: usize = 32;

enum Status {
    Loading(Arc<()>),
    Ready(Arc<ColorImage>),
    Failed(String, Instant),
}

struct Entry {
    status: Status,
    pass: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        match &self.status {
            Status::Ready(image) => image.pixels.len() * size_of::<egui::Color32>(),
            _ => 0,
        }
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
    pass: u64,
}

impl Cache {
    fn finish(&mut self, uri: &str, token: &Arc<()>, status: Status) {
        if let Some(entry) = self.entries.get_mut(uri)
            && matches!(&entry.status, Status::Loading(active) if Arc::ptr_eq(active, token))
        {
            entry.status = status;
            self.bytes += entry.bytes();
        }
    }

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn forget(&mut self, uri: &str) {
        if let Some(entry) = self.entries.remove(uri) {
            self.bytes -= entry.bytes();
        }
    }

    fn end_pass(&mut self, pass: u64) {
        self.pass = pass;
        if self.entries.len() > MAX_ENTRIES || self.bytes > DECODED_BUDGET {
            self.entries.retain(|_, entry| {
                let keep = entry.pass.saturating_add(2) >= pass || matches!(entry.status, Status::Loading(_));
                if !keep {
                    self.bytes -= entry.bytes();
                }
                keep
            });
        }
    }
}

pub(super) fn is_http(uri: &str) -> bool {
    uri.starts_with("https://") || uri.starts_with("http://")
}

fn source_uri(uri: &str) -> &str {
    egui::decode_animated_image_uri(uri).map_or(uri, |(source, _)| source)
}

pub struct HttpImages {
    backend: Backend,
    cache: Arc<Mutex<Cache>>,
    disk: Arc<DiskCache>,
    slots: Arc<Semaphore>,
}

impl HttpImages {
    pub fn new(backend: Backend) -> Self {
        let disk = Arc::new(DiskCache {
            dir: backend.data_dir().join("thumbs"),
            writes: AtomicUsize::new(0),
        });
        let startup = disk.clone();
        backend.spawn(async move {
            let _ = tokio::task::spawn_blocking(move || startup.trim()).await;
        });
        Self {
            backend,
            cache: Default::default(),
            disk,
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        }
    }
}

impl ImageLoader for HttpImages {
    fn id(&self) -> &str {
        egui::generate_loader_id!(HttpImages)
    }

    fn load(&self, ctx: &egui::Context, uri: &str, _: SizeHint) -> ImageLoadResult {
        let uri = source_uri(uri);
        if !is_http(uri) {
            return Err(LoadError::NotSupported);
        }
        let mut cache = self.cache.lock().unwrap();
        let pass = cache.pass;
        if let Some(entry) = cache.entries.get_mut(uri) {
            entry.pass = pass;
            match &entry.status {
                Status::Ready(image) => {
                    return Ok(ImagePoll::Ready {
                        image: image.clone(),
                    });
                }
                Status::Loading(_) => return Ok(ImagePoll::Pending { size: None }),
                Status::Failed(error, when) if when.elapsed() < RETRY_AFTER => {
                    ctx.request_repaint_after(RETRY_AFTER.saturating_sub(when.elapsed()));
                    return Err(LoadError::Loading(error.clone()));
                }
                Status::Failed(..) => {}
            }
        }
        // A completion wakes egui. While saturated, avoid queuing work for rows
        // that may already be offscreen by the time a worker is free.
        let Ok(slot) = self.slots.clone().try_acquire_owned() else {
            return Ok(ImagePoll::Pending { size: None });
        };
        let token = Arc::new(());
        cache.entries.insert(
            uri.to_owned(),
            Entry {
                status: Status::Loading(token.clone()),
                pass,
            },
        );
        drop(cache);

        let cache = self.cache.clone();
        let disk = self.disk.clone();
        let backend = self.backend.clone();
        let uri = uri.to_owned();
        self.backend.spawn(async move {
            let result = fetch(&backend.http(), disk, &uri).await;
            let status = match result {
                Ok(image) => Status::Ready(Arc::new(image)),
                Err(err) => Status::Failed(err.to_string(), Instant::now()),
            };
            cache.lock().unwrap().finish(&uri, &token, status);
            drop(slot);
            backend.wake();
        });
        Ok(ImagePoll::Pending { size: None })
    }

    fn forget(&self, uri: &str) {
        self.cache.lock().unwrap().forget(source_uri(uri));
    }

    fn forget_all(&self) {
        let mut cache = self.cache.lock().unwrap();
        cache.entries.clear();
        cache.bytes = 0;
    }

    fn byte_size(&self) -> usize {
        self.cache.lock().unwrap().bytes()
    }

    fn has_pending(&self) -> bool {
        self.slots.available_permits() < MAX_IN_FLIGHT
    }

    fn end_pass(&self, pass: u64) {
        self.cache.lock().unwrap().end_pass(pass);
    }
}

async fn fetch(
    http: &reqwest::Client,
    disk: Arc<DiskCache>,
    uri: &str,
) -> anyhow::Result<ColorImage> {
    let file = disk.path(uri);
    let cached = file.clone();
    if let Some(image) = tokio::task::spawn_blocking(move || read_cached(&cached)).await? {
        return Ok(image);
    }

    let mut response = http
        .get(uri)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(reqwest::Error::without_url)?;
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|len| len <= MAX_BODY as u64),
        "artwork is too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        anyhow::ensure!(
            chunk.len() <= MAX_BODY - bytes.len(),
            "artwork is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || {
        let image = decode(&bytes)?;
        disk.write(&file, &bytes);
        Ok(image)
    })
    .await?
}

fn read_cached(path: &Path) -> Option<ColorImage> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(DISK_MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > DISK_MAX_FILE {
        return None;
    }
    let image = decode(&bytes).ok()?;
    // Keep frequently viewed artwork near the newest end of the disk cache.
    let _ = std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now()));
    Some(image)
}

fn decode(bytes: &[u8]) -> anyhow::Result<ColorImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    anyhow::ensure!(
        width.saturating_mul(height) <= MAX_PIXELS,
        "artwork has too many pixels"
    );
    let rgba = image::DynamicImage::from_decoder(decoder)?.into_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        rgba.as_raw(),
    ))
}

struct DiskCache {
    dir: PathBuf,
    writes: AtomicUsize,
}

impl DiskCache {
    fn path(&self, uri: &str) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        uri.hash(&mut h);
        self.dir.join(format!("{:016x}", h.finish()))
    }

    fn write(&self, path: &Path, bytes: &[u8]) {
        if bytes.len() > DISK_MAX_FILE || std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let Ok(mut tmp) = tempfile::NamedTempFile::new_in(&self.dir) else {
            return;
        };
        if tmp.write_all(bytes).is_err() || tmp.persist(path).is_err() {
            return;
        }
        if self.writes.fetch_add(1, Ordering::Relaxed) % TRIM_EVERY == TRIM_EVERY - 1 {
            self.trim();
        }
    }

    fn trim(&self) {
        self.trim_to(DISK_BUDGET);
    }

    fn trim_to(&self, budget: u64) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<_> = dir
            .flatten()
            .filter_map(|entry| {
                // Exclude temporary writes and non-cache files.
                let name = entry.file_name();
                let name = name.to_str()?;
                if name.len() != 16
                    || !name.bytes().all(|b| b.is_ascii_hexdigit())
                    || !entry.file_type().ok()?.is_file()
                {
                    return None;
                }
                let metadata = entry.metadata().ok()?;
                Some((metadata.modified().ok()?, metadata.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|file| file.1).sum();
        if total <= budget {
            return;
        }
        files.sort_unstable();
        for (_, len, path) in files {
            if total <= budget {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_bytes_follow_completion_and_eviction() {
        let mut cache = Cache::default();
        let token = Arc::new(());
        for uri in ["old", "recent"] {
            cache.entries.insert(
                uri.into(),
                Entry { status: Status::Loading(token.clone()), pass: if uri == "old" { 0 } else { 4 } },
            );
            cache.finish(
                uri,
                &token,
                Status::Ready(Arc::new(ColorImage::filled([2048, 2048], egui::Color32::BLACK))),
            );
        }
        assert_eq!(cache.bytes(), 32 * 1024 * 1024);
        cache.end_pass(4);
        assert_eq!(cache.bytes(), 16 * 1024 * 1024);
        assert!(!cache.entries.contains_key("old"));
        cache.forget("recent");
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn stale_completion_does_not_replace_new_request() {
        let mut cache = Cache::default();
        let old = Arc::new(());
        let new = Arc::new(());
        cache.entries.insert(
            "cover".into(),
            Entry {
                status: Status::Loading(new.clone()),
                pass: 0,
            },
        );
        cache.finish(
            "cover",
            &old,
            Status::Failed("stale".into(), Instant::now()),
        );
        assert!(
            matches!(&cache.entries["cover"].status, Status::Loading(token) if Arc::ptr_eq(token, &new))
        );
    }

    #[test]
    fn decodes_normal_art_and_rejects_excessive_pixel_count() {
        fn png(width: u32, height: u32) -> Vec<u8> {
            let mut buffer = Cursor::new(Vec::new());
            image::GrayImage::new(width, height)
                .write_to(&mut buffer, image::ImageFormat::Png)
                .unwrap();
            buffer.into_inner()
        }
        assert_eq!(decode(&png(1200, 1200)).unwrap().size, [1200, 1200]);
        assert!(
            decode(&png(2049, 2048))
                .unwrap_err()
                .to_string()
                .contains("too many pixels")
        );
    }

    #[test]
    fn disk_trim_preserves_newer_entries_and_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskCache {
            dir: dir.path().to_owned(),
            writes: AtomicUsize::new(0),
        };
        let old = disk.path("old");
        let new = disk.path("new");
        disk.write(&old, b"old");
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        disk.write(&new, b"new");
        let temp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        disk.trim_to(3);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new).unwrap(), b"new");
        assert!(temp.path().is_file());
    }
}
