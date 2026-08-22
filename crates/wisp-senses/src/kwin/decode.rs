//! Turning the KWin script's batches into `Observation`s.
//!
//! Pure and offline-testable: this module never touches D-Bus. Everything that
//! can be got wrong about the terrain feed — epoch changes, windows that vanish
//! without a `closed` signal, duplicate focus events — is decided here against
//! captured fixtures.

use std::collections::BTreeSet;

use serde::Deserialize;
use wisp_proto::Observation;

/// Bumped only when the script's payload shape changes incompatibly. The Rust
/// side refuses a batch it does not understand rather than guessing.
pub const PROTOCOL: u32 = 1;

/// One window's state in a batch: `[id, x, y, w, h, gone]`.
type WinTuple = (u64, i32, i32, u32, u32, bool);

#[derive(Debug, Clone, Deserialize)]
pub struct Batch {
    /// Protocol version.
    pub v: u32,
    /// Script instance epoch. Changes whenever the script is reloaded, which is
    /// also when the dense window ids restart from 1.
    pub e: u64,
    /// Batch sequence number within an epoch. Used to measure the feed's rate
    /// and to notice drops.
    pub s: u64,
    /// `Date.now()` inside KWin, for latency measurement only. Never used for
    /// ordering — SPEC's `Millis` is the only ordering clock.
    #[serde(default)]
    pub t: i64,
    /// Changed windows.
    #[serde(default)]
    pub w: Vec<WinTuple>,
    /// Focus, when it changed. `[app_id, title]`.
    #[serde(default)]
    pub f: Option<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("terrain batch is protocol v{got}, we speak v{want}")]
    Version { got: u32, want: u32 },
    #[error("malformed terrain batch: {0}")]
    Json(#[from] serde_json::Error),
}

/// Stateful decoder. One per running feed.
#[derive(Debug, Default)]
pub struct TerrainDecoder {
    epoch: Option<u64>,
    live: BTreeSet<u64>,
    last_focus: Option<(String, String)>,
    last_seq: u64,
    /// Batches accepted. Public so the smoke example can report the rate.
    pub batches: u64,
    /// Window updates emitted.
    pub window_updates: u64,
    /// Gaps in the sequence number — the script's D-Bus calls are fire and
    /// forget, so this is the only way we would learn the feed lost something.
    pub sequence_gaps: u64,
}

impl TerrainDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// Windows we currently believe exist.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    pub fn decode_str(&mut self, json: &str) -> Result<Vec<Observation>, DecodeError> {
        let batch: Batch = serde_json::from_str(json)?;
        self.decode(batch)
    }

    pub fn decode(&mut self, batch: Batch) -> Result<Vec<Observation>, DecodeError> {
        if batch.v != PROTOCOL {
            return Err(DecodeError::Version { got: batch.v, want: PROTOCOL });
        }

        let mut out = Vec::with_capacity(batch.w.len() + 1);

        // A new epoch means the script restarted and its dense ids restarted
        // with it. Everything we thought was standing is now a lie; retract it
        // before the new world arrives, or she walks on a window that is not
        // there.
        if self.epoch != Some(batch.e) {
            for id in std::mem::take(&mut self.live) {
                out.push(Observation::Window { id, x: 0, y: 0, w: 0, h: 0, gone: true });
            }
            self.epoch = Some(batch.e);
            self.last_seq = 0;
            self.last_focus = None;
        } else if batch.s > self.last_seq + 1 {
            self.sequence_gaps += batch.s - self.last_seq - 1;
        }
        self.last_seq = batch.s;
        self.batches += 1;

        if let Some((app_id, title)) = batch.f {
            let pair = (app_id, title);
            if self.last_focus.as_ref() != Some(&pair) {
                self.last_focus = Some(pair.clone());
                out.push(Observation::Focus { app_id: pair.0, title: pair.1 });
            }
        }

        for (id, x, y, w, h, gone) in batch.w {
            if gone {
                // Only retract something we actually asserted, so a duplicate
                // close does not produce a phantom event.
                if !self.live.remove(&id) {
                    continue;
                }
                out.push(Observation::Window { id, x: 0, y: 0, w: 0, h: 0, gone: true });
            } else {
                self.live.insert(id);
                out.push(Observation::Window { id, x, y, w, h, gone: false });
            }
        }

        self.window_updates += out
            .iter()
            .filter(|o| matches!(o, Observation::Window { .. }))
            .count() as u64;
        Ok(out)
    }

    /// The feed is going away (KWin died, or we are shutting down). Retract the
    /// whole world so nothing is left standing on stale terrain.
    pub fn retract_all(&mut self) -> Vec<Observation> {
        self.epoch = None;
        self.last_focus = None;
        std::mem::take(&mut self.live)
            .into_iter()
            .map(|id| Observation::Window { id, x: 0, y: 0, w: 0, h: 0, gone: true })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kwin");

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{name}")).expect(name)
    }

    #[test]
    fn decodes_the_captured_resync_batch() {
        let mut d = TerrainDecoder::new();
        let obs = d.decode_str(&fixture("batch_resync.json")).unwrap();

        assert!(matches!(&obs[0], Observation::Focus { app_id, .. } if app_id == "kitty"));
        let windows: Vec<_> = obs
            .iter()
            .filter_map(|o| match o {
                Observation::Window { id, x, y, w, h, gone } => Some((*id, *x, *y, *w, *h, *gone)),
                _ => None,
            })
            .collect();
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0], (1, 0, 1384, 5120, 56, false), "the plasma panel");
        assert!(windows.iter().any(|w| w.0 == 3 && w.3 == 1720));
        assert_eq!(d.live_count(), 4);
        assert_eq!(d.epoch(), Some(7710339455));
    }

    /// A capture of a real window drag, one batch per compositor flush. This is
    /// the terrain case that has to be timely: she is standing on window 3 while
    /// it moves under her.
    #[test]
    fn a_drag_streams_geometry_at_the_captured_rate() {
        let mut d = TerrainDecoder::new();
        d.decode_str(&fixture("batch_resync.json")).unwrap();

        let batches: Vec<Batch> = serde_json::from_str(&fixture("drag_sequence.json")).unwrap();
        assert!(batches.len() >= 8, "capture too short to say anything about rate");
        let span_ms = (batches[batches.len() - 1].t - batches[0].t) as f64;
        let hz = (batches.len() - 1) as f64 * 1000.0 / span_ms;

        let mut moves = 0usize;
        for b in batches {
            for o in d.decode(b).unwrap() {
                match o {
                    Observation::Window { id: 3, gone: false, .. } => moves += 1,
                    other => panic!("a drag must produce only window 3's geometry, got {other:?}"),
                }
            }
        }
        assert_eq!(moves, 12);
        assert!(hz > 30.0, "captured drag feed was only {hz:.1} Hz");
        // Nothing was created or destroyed by a drag.
        assert_eq!(d.live_count(), 4);
    }

    #[test]
    fn closing_a_window_retracts_it_exactly_once() {
        let mut d = TerrainDecoder::new();
        d.decode_str(&fixture("batch_resync.json")).unwrap();
        let obs = d.decode_str(&fixture("batch_close.json")).unwrap();
        assert_eq!(obs, vec![Observation::Window { id: 4, x: 0, y: 0, w: 0, h: 0, gone: true }]);
        assert_eq!(d.live_count(), 3);

        // The same close again must produce nothing.
        let obs = d.decode_str(&fixture("batch_close.json")).unwrap();
        assert!(obs.is_empty());
    }

    #[test]
    fn a_new_epoch_retracts_the_old_world_first() {
        let mut d = TerrainDecoder::new();
        d.decode_str(&fixture("batch_resync.json")).unwrap();
        assert_eq!(d.live_count(), 4);

        let obs = d.decode_str(&fixture("batch_new_epoch.json")).unwrap();
        // First four events are the retraction of epoch 7710339455's ids 1..=4.
        let retracted: Vec<u64> = obs
            .iter()
            .take_while(|o| matches!(o, Observation::Window { gone: true, .. }))
            .map(|o| match o {
                Observation::Window { id, .. } => *id,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(retracted, vec![1, 2, 3, 4]);
        // Then the new epoch's world.
        assert!(obs.iter().any(|o| matches!(o, Observation::Window { id: 1, gone: false, .. })));
        assert_eq!(d.epoch(), Some(4412200191));
    }

    #[test]
    fn repeated_focus_is_not_republished() {
        let mut d = TerrainDecoder::new();
        d.decode_str(&fixture("batch_resync.json")).unwrap();
        let again = d.decode_str(&fixture("batch_focus_same.json")).unwrap();
        assert!(again.is_empty(), "same focus twice must be silent, got {again:?}");
        let moved = d.decode_str(&fixture("batch_focus_change.json")).unwrap();
        assert_eq!(
            moved,
            vec![Observation::Focus {
                app_id: "firefox".into(),
                title: "NX Wisp — SPEC — Mozilla Firefox".into()
            }]
        );
    }

    #[test]
    fn wrong_protocol_is_refused_not_guessed() {
        let mut d = TerrainDecoder::new();
        let e = d.decode_str(r#"{"v":99,"e":1,"s":1,"t":0,"w":[]}"#).unwrap_err();
        assert!(matches!(e, DecodeError::Version { got: 99, want: 1 }));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let mut d = TerrainDecoder::new();
        assert!(d.decode_str("not json at all").is_err());
        assert!(d.decode_str(r#"{"v":1}"#).is_err(), "missing required fields");
        // A window tuple of the wrong arity must not be silently half-read.
        assert!(d.decode_str(r#"{"v":1,"e":1,"s":1,"w":[[1,2,3]]}"#).is_err());
    }

    #[test]
    fn sequence_gaps_are_counted() {
        let mut d = TerrainDecoder::new();
        d.decode_str(r#"{"v":1,"e":5,"s":1,"w":[[1,0,0,10,10,false]]}"#).unwrap();
        d.decode_str(r#"{"v":1,"e":5,"s":4,"w":[[1,1,0,10,10,false]]}"#).unwrap();
        assert_eq!(d.sequence_gaps, 2);
    }

    #[test]
    fn retract_all_empties_the_world() {
        let mut d = TerrainDecoder::new();
        d.decode_str(&fixture("batch_resync.json")).unwrap();
        let gone = d.retract_all();
        assert_eq!(gone.len(), 4);
        assert!(gone.iter().all(|o| matches!(o, Observation::Window { gone: true, .. })));
        assert_eq!(d.live_count(), 0);
        assert_eq!(d.epoch(), None);
        assert!(d.retract_all().is_empty());
    }

    #[test]
    fn every_emitted_observation_belongs_to_a_window_sense() {
        use wisp_proto::SenseId;
        let mut d = TerrainDecoder::new();
        let obs = d.decode_str(&fixture("batch_resync.json")).unwrap();
        for o in obs {
            assert!(
                matches!(o.sense(), SenseId::ActiveWindow | SenseId::WindowGeometry),
                "terrain feed produced {o:?}, which belongs to another sense"
            );
        }
    }
}
