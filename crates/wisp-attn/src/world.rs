//! The digest of the [`Observation`] stream that behaviour-tree conditions
//! read (F50).
//!
//! A condition is data — it cannot run code — so it cannot walk an event log.
//! `World` is the last-known state of everything the senses reported, kept
//! deliberately shallow: current focus, where the windows are (she uses them as
//! terrain), what is playing, what the machine is doing. Nothing is inferred
//! here; inference lives in [`crate::flow`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use wisp_proto::{Millis, Observation};

/// A window, in output coordinates. Terrain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
    /// The point she perches on: the middle of the top edge.
    pub fn top_middle(&self) -> (i32, i32) {
        (self.x + self.w as i32 / 2, self.y)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct World {
    /// When the last observation landed.
    pub updated: Millis,
    pub idle: bool,
    /// How long the idle sense says they have been gone, at `idle_stamp`.
    pub idle_for_ms: u64,
    pub idle_stamp: Millis,
    pub app: String,
    pub title: String,
    pub focus_since: Millis,
    pub workspace: String,
    pub windows: BTreeMap<u64, Rect>,
    pub media_playing: bool,
    pub media_title: String,
    pub media_artist: String,
    pub audio_out: u8,
    pub mic_live: bool,
    pub last_notification: Option<(Millis, String, String)>,
    pub cpu_pct: u8,
    pub gpu_pct: u8,
    pub vram_used_mib: u64,
    pub temp_c: u8,
    pub on_battery: bool,
    pub dirty_paths: BTreeSet<String>,
    pub last_speech: Option<(Millis, String)>,
    /// `app` + `field` -> latest value seen on the NX Connector bus.
    pub fleet: BTreeMap<(String, String), String>,
}

impl World {
    pub fn observe(&mut self, now: Millis, obs: &Observation) {
        self.updated = now;
        match obs {
            Observation::Idle { idle, for_ms } => {
                self.idle = *idle;
                self.idle_for_ms = *for_ms;
                self.idle_stamp = now;
            }
            Observation::Focus { app_id, title } => {
                if *app_id != self.app {
                    self.focus_since = now;
                }
                self.app = app_id.clone();
                self.title = title.clone();
            }
            Observation::Window { id, x, y, w, h, gone } => {
                if *gone {
                    self.windows.remove(id);
                } else {
                    self.windows.insert(*id, Rect { x: *x, y: *y, w: *w, h: *h });
                }
            }
            Observation::Media { title, artist, playing, .. } => {
                self.media_playing = *playing;
                self.media_title = title.clone();
                self.media_artist = artist.clone();
            }
            Observation::AudioLevel { out, mic_live } => {
                self.audio_out = *out;
                self.mic_live = *mic_live;
            }
            Observation::Notification { app, summary, .. } => {
                self.last_notification = Some((now, app.clone(), summary.clone()));
            }
            Observation::Vitals { cpu_pct, gpu_pct, vram_used_mib, temp_c, on_battery } => {
                self.cpu_pct = *cpu_pct;
                self.gpu_pct = *gpu_pct;
                self.vram_used_mib = *vram_used_mib;
                self.temp_c = *temp_c;
                self.on_battery = *on_battery;
            }
            Observation::Workspace { name, .. } => self.workspace = name.clone(),
            Observation::Files { path, dirty } => {
                if *dirty {
                    self.dirty_paths.insert(path.clone());
                } else {
                    self.dirty_paths.remove(path);
                }
            }
            Observation::Speech { text, final_ } => {
                if *final_ {
                    self.last_speech = Some((now, text.clone()));
                }
            }
            // Length and kind only — she never keeps clipboard content.
            Observation::Clipboard { .. } => {}
            Observation::Fleet { app, field, value } => {
                self.fleet.insert((app.clone(), field.clone()), value.clone());
            }
        }
    }

    /// Idle time projected to `now` — the sense samples, it does not stream.
    pub fn idle_ms(&self, now: Millis) -> u64 {
        if !self.idle {
            return 0;
        }
        self.idle_for_ms.saturating_add(now.saturating_sub(self.idle_stamp))
    }

    pub fn focus_held(&self, now: Millis) -> Millis {
        now.saturating_sub(self.focus_since)
    }

    /// The biggest window on screen: the most interesting thing to poke.
    pub fn largest_window(&self) -> Option<(u64, Rect)> {
        self.windows.iter().max_by_key(|(id, r)| (r.area(), **id)).map(|(id, r)| (*id, *r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    #[test]
    fn focus_since_only_moves_on_a_real_change() {
        isolate();
        let mut w = World::default();
        w.observe(1_000, &Observation::Focus { app_id: "kate".into(), title: "a".into() });
        w.observe(5_000, &Observation::Focus { app_id: "kate".into(), title: "b".into() });
        assert_eq!(w.focus_since, 1_000);
        assert_eq!(w.title, "b");
        w.observe(9_000, &Observation::Focus { app_id: "konsole".into(), title: "c".into() });
        assert_eq!(w.focus_since, 9_000);
        assert_eq!(w.focus_held(10_000), 1_000);
    }

    #[test]
    fn windows_are_terrain_and_vanish_when_closed() {
        isolate();
        let mut w = World::default();
        w.observe(0, &Observation::Window { id: 1, x: 0, y: 0, w: 100, h: 100, gone: false });
        w.observe(0, &Observation::Window { id: 2, x: 10, y: 20, w: 800, h: 600, gone: false });
        assert_eq!(w.largest_window().map(|(id, _)| id), Some(2));
        assert_eq!(w.largest_window().unwrap().1.top_middle(), (410, 20));
        w.observe(1, &Observation::Window { id: 2, x: 0, y: 0, w: 0, h: 0, gone: true });
        assert_eq!(w.largest_window().map(|(id, _)| id), Some(1));
    }

    #[test]
    fn idle_is_projected_forward_between_samples() {
        isolate();
        let mut w = World::default();
        w.observe(1_000, &Observation::Idle { idle: true, for_ms: 60_000 });
        assert_eq!(w.idle_ms(31_000), 90_000);
        w.observe(31_000, &Observation::Idle { idle: false, for_ms: 0 });
        assert_eq!(w.idle_ms(60_000), 0);
    }

    #[test]
    fn clipboard_content_is_never_kept() {
        isolate();
        let mut w = World::default();
        let before = w.clone();
        w.observe(5, &Observation::Clipboard { len: 4096, kind: "text/plain".into() });
        assert_eq!(World { updated: 0, ..w.clone() }, before);
    }

    #[test]
    fn fleet_fields_are_last_write_wins() {
        isolate();
        let mut w = World::default();
        w.observe(0, &Observation::Fleet {
            app: "nx-sentry".into(),
            field: "state".into(),
            value: "armed".into(),
        });
        w.observe(1, &Observation::Fleet {
            app: "nx-sentry".into(),
            field: "state".into(),
            value: "tripped".into(),
        });
        assert_eq!(w.fleet.get(&("nx-sentry".into(), "state".into())).map(String::as_str), Some("tripped"));
    }
}
