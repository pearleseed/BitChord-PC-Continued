#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod appicon;
mod apple;
mod artist_signature;
mod fullscreen;
mod proxy;
mod renderer;
mod art;
mod audio;
mod backend;
mod bidi;
mod deeplink;
mod discord;
mod dash;
mod downloads;
mod hires;
mod images;
mod jam;
mod innertube;
mod jsc;
mod lastfm;
mod login;
mod lyrics;
#[cfg(target_os = "macos")]
mod macos_chrome;
mod media;
mod model;
mod motion;
mod opus;
mod preset;
mod shader;
mod sponsorblock;
mod stream;
mod theme;
mod transfer;
mod translate;
mod tray;
mod turnstile;
mod ui;
mod update;
mod webmusic;
mod ytdlp;
mod ytm;
mod vault;
mod ytmapi;

pub(crate) use app::{DEFAULT_SIZE, MIN_SIZE, data_dir};

fn main() -> eframe::Result {
    app::run()
}
