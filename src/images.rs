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

/// Soft GPU budget for artwork. The visible working set is kept even under pressure.
const TEXTURE_BUDGET: usize = 48 * 1024 * 1024;

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
        self.state.lock().unwrap().end_pass(pass_index, TEXTURE_BUDGET);
    }

    fn byte_size(&self) -> usize {
        self.state.lock().unwrap().bytes
    }
}

impl TextureState {
    fn end_pass(&mut self, pass_index: u64, budget: usize) {
        self.pass = pass_index;
        if self.bytes <= budget {
            return;
        }
        // Oldest first; anything drawn in the last couple of frames stays.
        let mut old: Vec<_> = self
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
            if self.bytes <= budget * 3 / 4 {
                break;
            }
            let variants = self.cache.get_mut(&uri).expect("queued texture URI");
            let removed = variants.remove(&options).expect("queued texture variant");
            if variants.is_empty() {
                self.cache.remove(&uri);
            }
            self.bytes -= removed.handle.byte_size();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_old_textures_first_and_keeps_the_visible_working_set() {
        let ctx = egui::Context::default();
        let options = TextureOptions::LINEAR;
        let mut state = TextureState::default();
        for (name, last_used) in [("old", 0), ("less old", 1), ("visible", 5)] {
            let handle = ctx.load_texture(name, egui::ColorImage::filled([4, 4], egui::Color32::WHITE), options);
            state.bytes += handle.byte_size();
            let texture = Texture { handle, size: Vec2::splat(4.0), last_used };
            state.cache.insert(name.into(), HashMap::from([(options, texture)]));
        }
        state.end_pass(5, 128);
        assert_eq!(state.bytes, 64);
        assert!(state.cache.contains_key("visible"));
        state.end_pass(5, 32);
        assert_eq!(state.bytes, 64);
    }
}
