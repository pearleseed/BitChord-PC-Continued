//! Main-thread AppKit presenter for BitChord's experimental Lock Screen player.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::time::Duration;

use egui::load::{ImagePoll, SizeHint};
use souvlaki::{MediaControlEvent, MediaPosition};

use crate::{backend, model::Track};

unsafe extern "C" {
    fn bitchord_lock_create(
        callback: extern "C" fn(*const c_void, u32, f64),
        context: *const c_void,
    ) -> *mut c_void;
    fn bitchord_lock_destroy(handle: *mut c_void);
    fn bitchord_lock_track(
        handle: *mut c_void,
        title: *const u8,
        title_len: usize,
        artist: *const u8,
        artist_len: usize,
    );
    fn bitchord_lock_artwork(handle: *mut c_void, rgba: *const u8, width: usize, height: usize);
    fn bitchord_lock_update(
        handle: *mut c_void,
        position: f64,
        duration: f64,
        playing: bool,
        can_seek: bool,
    ) -> bool;
}

pub struct Snapshot<'a> {
    pub track: &'a Track,
    pub artwork: &'a Track,
    pub generation: u64,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub playing: bool,
    pub can_seek: bool,
}

pub struct LockScreen {
    handle: NonNull<c_void>,
    callback: Box<Callback>,
    track: Option<(String, String)>,
    pending_artwork: Option<String>,
}

struct Callback {
    ctx: egui::Context,
    generation: Cell<Option<u64>>,
    events: RefCell<Vec<(u64, MediaControlEvent)>>,
}

impl LockScreen {
    pub fn new(ctx: &egui::Context) -> Option<Self> {
        let callback = Box::new(Callback {
            ctx: ctx.clone(),
            generation: Cell::new(None),
            events: RefCell::default(),
        });
        // The callback and presenter live on AppKit's main thread. Drop detaches
        // observers and invalidates the timer before releasing the boxed context.
        let handle = NonNull::new(unsafe {
            bitchord_lock_create(receive, (&*callback as *const Callback).cast())
        })?;
        Some(Self {
            handle,
            callback,
            track: None,
            pending_artwork: None,
        })
    }

    pub fn take_events(&self) -> Vec<(u64, MediaControlEvent)> {
        self.callback.events.take()
    }

    pub fn sync(&mut self, snapshot: Option<Snapshot<'_>>) -> bool {
        let handle = self.handle.as_ptr();
        self.callback
            .generation
            .set(snapshot.as_ref().map(|s| s.generation));
        let Some(snapshot) = snapshot else {
            if self.track.take().is_some() {
                unsafe { bitchord_lock_track(handle, ptr::null(), 0, ptr::null(), 0) };
                self.pending_artwork = None;
            }
            return true;
        };

        let track_id = snapshot.track.id.as_str();
        let artwork_id = snapshot.artwork.id.as_str();
        if self
            .track
            .as_ref()
            .is_none_or(|(track, art)| track != track_id || art != artwork_id)
        {
            self.track = Some((track_id.to_owned(), artwork_id.to_owned()));
            self.pending_artwork = backend::art_url(snapshot.artwork);
            let title = &snapshot.track.title;
            let artist = snapshot.track.artist_line();
            unsafe {
                bitchord_lock_track(
                    handle,
                    title.as_ptr(),
                    title.len(),
                    artist.as_ptr(),
                    artist.len(),
                );
            }
        }
        if let Some(uri) = &self.pending_artwork
            && let Ok(ImagePoll::Ready { image }) =
                self.callback.ctx.try_load_image(uri, SizeHint::default())
        {
            // AppKit copies the premultiplied RGBA data before the image is released.
            let [width, height] = image.size;
            unsafe { bitchord_lock_artwork(handle, image.as_raw().as_ptr(), width, height) };
            self.pending_artwork = None;
        }

        unsafe {
            bitchord_lock_update(
                handle,
                snapshot.position.as_secs_f64(),
                snapshot.duration.map_or(0.0, |d| d.as_secs_f64()),
                snapshot.playing,
                snapshot.can_seek,
            )
        }
    }
}

impl Drop for LockScreen {
    fn drop(&mut self) {
        unsafe { bitchord_lock_destroy(self.handle.as_ptr()) };
    }
}

extern "C" fn receive(context: *const c_void, action: u32, seconds: f64) {
    let callback = unsafe { &*context.cast::<Callback>() };
    let event = match action {
        1 => Some(MediaControlEvent::Toggle),
        2 => Some(MediaControlEvent::Previous),
        3 => Some(MediaControlEvent::Next),
        4 => Duration::try_from_secs_f64(seconds)
            .ok()
            .map(|position| MediaControlEvent::SetPosition(MediaPosition(position))),
        _ => None,
    };
    if let (Some(generation), Some(event)) = (callback.generation.get(), event) {
        callback.events.borrow_mut().push((generation, event));
    }
    callback.ctx.request_repaint();
}
