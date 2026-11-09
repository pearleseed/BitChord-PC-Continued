//! AppKit controls on the main eframe view. The renderer and navigation model stay in Rust.

use std::ffi::{c_int, c_uint, c_void};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};

unsafe extern "C" {
    fn bitchord_chrome_create(
        view: *mut c_void,
        wake: extern "C" fn(*const c_void),
        wake_context: *const c_void,
    ) -> *mut c_void;
    fn bitchord_chrome_destroy(handle: *mut c_void);
    fn bitchord_chrome_top_inset(handle: *mut c_void) -> f64;
    fn bitchord_chrome_set_compact(handle: *mut c_void, compact: c_int);
    fn bitchord_chrome_appearance(handle: *mut c_void, appearance: c_int);
    fn bitchord_chrome_preferences() -> c_uint;
    fn bitchord_chrome_take_history(handle: *mut c_void) -> c_uint;
    fn bitchord_chrome_history(
        handle: *mut c_void,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        enabled: c_uint,
    );
}

/// Main-thread-only owner of the native controls and their repaint callback.
pub struct Chrome {
    handle: NonNull<c_void>,
    _wake_context: Box<egui::Context>,
    appearance: Option<c_int>,
}

impl Chrome {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let RawWindowHandle::AppKit(view) = cc.window_handle().ok()?.as_raw() else {
            return None;
        };
        let context = Box::new(cc.egui_ctx.clone());
        // SAFETY: eframe creates the app on AppKit's main thread. The boxed context
        // remains valid until after Drop has detached the native callbacks.
        let handle = NonNull::new(unsafe {
            bitchord_chrome_create(
                view.ns_view.as_ptr(),
                wake,
                (&*context as *const egui::Context).cast(),
            )
        })?;
        refresh_preferences();
        Some(Self {
            handle,
            _wake_context: context,
            appearance: None,
        })
    }

    pub fn top_inset(&self, ctx: &egui::Context) -> f32 {
        (unsafe { bitchord_chrome_top_inset(self.handle.as_ptr()) } as f32 / ctx.zoom_factor())
            .max(0.0)
    }

    pub fn set_compact(&self, compact: bool) {
        unsafe { bitchord_chrome_set_compact(self.handle.as_ptr(), compact.into()) };
    }

    pub fn set_appearance(&mut self, light: Option<bool>) {
        let value = match light {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        if self.appearance != Some(value) {
            unsafe { bitchord_chrome_appearance(self.handle.as_ptr(), value) };
            self.appearance = Some(value);
        }
    }

    pub fn take_history(&self) -> (bool, bool) {
        let pending = unsafe { bitchord_chrome_take_history(self.handle.as_ptr()) };
        (pending & 1 != 0, pending & 2 != 0)
    }

    pub fn layout_history(
        &self,
        ctx: &egui::Context,
        rect: Option<egui::Rect>,
        back: bool,
        forward: bool,
    ) {
        let origin = ctx.viewport_rect().min;
        let zoom = f64::from(ctx.zoom_factor());
        let (x, y, w, h) = rect.map_or((0.0, 0.0, 0.0, 0.0), |r| {
            (
                f64::from(r.left() - origin.x) * zoom,
                f64::from(r.top() - origin.y) * zoom,
                f64::from(r.width()) * zoom,
                f64::from(r.height()) * zoom,
            )
        });
        let enabled = c_uint::from(back) | (c_uint::from(forward) << 1);
        unsafe { bitchord_chrome_history(self.handle.as_ptr(), x, y, w, h, enabled) };
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        unsafe { bitchord_chrome_destroy(self.handle.as_ptr()) };
    }
}

extern "C" fn wake(context: *const c_void) {
    // SAFETY: this points to Chrome's boxed context, alive while observers are attached.
    unsafe { &*context.cast::<egui::Context>() }.request_repaint();
}

static PREFERENCES: AtomicU32 = AtomicU32::new(0);

/// Called from eframe on the main thread.
pub fn refresh_preferences() -> bool {
    let current = unsafe { bitchord_chrome_preferences() };
    let previous = PREFERENCES.swap(current, Ordering::Relaxed);
    (current ^ previous) & 0b111 != 0
}

pub fn preferences() -> u32 {
    PREFERENCES.load(Ordering::Relaxed)
}
