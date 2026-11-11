use std::time::Duration;

use souvlaki::MediaControlEvent;

use crate::macos_lock_screen::{LockScreen, Snapshot};

use super::App;

impl App {
    pub(super) fn set_lock_screen_enabled(&mut self, ctx: &egui::Context, enabled: bool) {
        self.lock_screen = enabled.then(|| LockScreen::new(ctx)).flatten();
        self.lock_screen_enabled = self.lock_screen.is_some();
        if enabled && !self.lock_screen_enabled {
            self.toast("Lock Screen Player is unavailable on this macOS version");
        }
        ctx.request_repaint();
    }

    fn lock_screen_timeline(&self) -> (bool, Duration) {
        if self.mix_hold().is_some() {
            return (
                self.audio.status.outgoing_playing(),
                self.audio.status.outgoing_position(),
            );
        }
        let position = self
            .play_load
            .map_or_else(|| self.audio.status.position_smooth(), |load| load.at);
        (self.playing(), position)
    }

    fn media_duration(&self) -> Option<Duration> {
        if let Some(mix) = self.mix_hold() {
            return Duration::try_from_secs_f32(mix.total)
                .ok()
                .filter(|d| !d.is_zero());
        }
        self.audio.status.duration().or_else(|| {
            self.playing_track()?
                .duration
                .filter(|&secs| secs > 0)
                .map(|secs| Duration::from_secs(u64::from(secs)))
        })
    }

    fn lock_screen_active(&self) -> bool {
        self.generation != 0 && self.lock_screen_hidden_generation != Some(self.generation)
    }

    pub(super) fn sync_lock_screen(&mut self) {
        let Some(mut panel) = self.lock_screen.take() else {
            return;
        };
        for (generation, event) in panel.take_events() {
            if !self.lock_screen_active() {
                break;
            }
            match event {
                MediaControlEvent::SetPosition(at) => {
                    if generation == self.generation
                        && !self.jam.guest()
                        && self.mix_hold().is_none()
                        && let Some(duration) = self.media_duration()
                    {
                        self.seek_to(at.0.min(duration));
                        self.lyric_view.resume();
                    }
                }
                _ => self.apply_media_event(event),
            }
        }
        // A restored queue is private until the listener starts playback in this session.
        let snapshot = self
            .shown()
            .filter(|_| self.lock_screen_active())
            .map(|track| {
                let (playing, position) = self.lock_screen_timeline();
                let artwork = if self.mix_hold().is_some() {
                    track
                } else {
                    self.current().unwrap_or(track)
                };
                let duration = self.media_duration();
                Snapshot {
                    track,
                    artwork,
                    generation: self.generation,
                    position: duration.map_or(position, |limit| position.min(limit)),
                    duration,
                    playing: playing || self.buffering(),
                    can_seek: duration.is_some() && !self.jam.guest() && self.mix_hold().is_none(),
                }
            });
        if panel.sync(snapshot) {
            self.lock_screen = Some(panel);
        } else {
            self.lock_screen_enabled = false;
            self.toast("Lock Screen Player couldn't open on this macOS version");
        }
    }
}
