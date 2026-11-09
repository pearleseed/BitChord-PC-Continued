//! Texture cache shared by remote artwork and bundled previews.

mod http;
pub use http::HttpImages;
use http::is_http;

use std::collections::HashMap;
use std::sync::Mutex;

use egui::load::{
    ImagePoll, LoadError, SizeHint, SizedTexture, TextureLoadResult, TextureLoader, TexturePoll,
};
use egui::{TextureHandle, TextureOptions, Vec2};

/// GPU memory the thumbnails may keep before the least recently drawn are dropped.
const TEXTURE_BUDGET: usize = 96 * 1024 * 1024;

struct Texture {
    handle: TextureHandle,
    size: Vec2,
    last_used: u64,
}

#[derive(Default)]
struct TextureState {
    pass: u64,
    cache: HashMap<String, HashMap<TextureOptions, Texture>>,
    bytes: usize,
}

/// Texture loader for `https://` images (and bundled previews). Unlike egui's default one it drops the
/// downloaded and decoded copies once the texture exists (only the GPU copy is
/// needed), and it keeps the textures under [`TEXTURE_BUDGET`] by evicting the ones
/// not drawn for longest, so browsing for hours doesn't grow without bound and nothing
/// on screen is ever thrown away.
#[derive(Default)]
pub struct Textures {
    state: Mutex<TextureState>,
}

impl TextureLoader for Textures {
    fn id(&self) -> &str {
        egui::generate_loader_id!(Textures)
    }

    fn load(
        &self,
        ctx: &egui::Context,
        uri: &str,
        options: TextureOptions,
        size_hint: SizeHint,
    ) -> TextureLoadResult {
        // Also the bundled theme covers and icon previews: egui's own loader would keep
        // each one decoded, as well as its texture, until the app closes.
        if !is_http(uri)
            && !uri.starts_with("bytes://theme-cover/")
            && !uri.starts_with("bytes://app-icon/")
        {
            return Err(LoadError::NotSupported);
        }
        {
            let mut state = self.state.lock().unwrap();
            let pass = state.pass;
            if let Some(t) = state
                .cache
                .get_mut(uri)
                .and_then(|variants| variants.get_mut(&options))
            {
                t.last_used = pass;
                return Ok(TexturePoll::Ready {
                    texture: SizedTexture::new(t.handle.id(), t.size),
                });
            }
        }
        match ctx.try_load_image(uri, size_hint)? {
            ImagePoll::Pending { size } => Ok(TexturePoll::Pending { size }),
            ImagePoll::Ready { image } => {
                let size = image.source_size;
                let handle = ctx.load_texture(uri, image, options);
                let texture = SizedTexture::new(handle.id(), size);
                let loaders = ctx.loaders();
                for l in loaders.bytes.lock().iter() {
                    l.forget(uri);
                }
                for l in loaders.image.lock().iter() {
                    l.forget(uri);
                }
                let mut state = self.state.lock().unwrap();
                let last_used = state.pass;
                state.bytes += handle.byte_size();
                state.cache.entry(uri.to_owned()).or_default().insert(
                    options,
                    Texture {
                        handle,
                        size,
                        last_used,
                    },
                );
                Ok(TexturePoll::Ready { texture })
            }
        }
    }

    fn forget(&self, uri: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(variants) = state.cache.remove(uri) {
            state.bytes -= variants
                .values()
                .map(|t| t.handle.byte_size())
                .sum::<usize>();
        }
    }

    fn forget_all(&self) {
        *self.state.lock().unwrap() = TextureState::default();
    }

    fn end_pass(&self, pass_index: u64) {
        let mut state = self.state.lock().unwrap();
        state.pass = pass_index;
        if state.bytes <= TEXTURE_BUDGET {
            return;
        }
        // Oldest first; anything drawn in the last couple of frames stays.
        let mut old: Vec<_> = state
            .cache
            .iter()
            .flat_map(|(uri, variants)| {
                variants
                    .iter()
                    .filter(|(_, t)| t.last_used.saturating_add(2) < pass_index)
                    .map(move |(&options, t)| (t.last_used, uri.clone(), options))
            })
            .collect();
        old.sort_unstable_by_key(|(last_used, _, _)| *last_used);
        for (_, uri, options) in old {
            if state.bytes <= TEXTURE_BUDGET * 3 / 4 {
                break;
            }
            let variants = state.cache.get_mut(&uri).expect("queued texture URI");
            let removed = variants.remove(&options).expect("queued texture variant");
            if variants.is_empty() {
                state.cache.remove(&uri);
            }
            state.bytes -= removed.handle.byte_size();
        }
    }

    fn byte_size(&self) -> usize {
        self.state.lock().unwrap().bytes
    }
}
