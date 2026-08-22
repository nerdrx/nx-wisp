//! F35 — voices as swappable packs, and how her mood bends one.
//!
//! **A voice is data.** Nothing in this module branches on which voice is
//! selected; a pack is a JSON document naming a model from [`crate::models`],
//! a base pitch/rate/volume, and a table of per-mood multipliers. Adding a
//! voice is shipping a file, not writing code, and the built-in packs are
//! `include_str!`d JSON parsed through exactly the same path as an operator's
//! own file — so the format cannot rot in the direction of "well, the built-ins
//! work". If a pack can express it, an operator can write it.
//!
//! ## Why the moods are named after the rig's expressions
//!
//! `wisp-rig` requires eight expressions of every skin: `neutral`, `curious`,
//! `delighted`, `smug`, `worried`, `bored`, `sleepy`, `alarmed`. Her face and
//! her voice are the same statement about her state, so they use the same
//! eight words. Inventing a parallel vocabulary here would guarantee that some
//! future mood is expressible in one and not the other. We do **not** depend on
//! `wisp-rig` to get them (SPEC §2's crate map does not allow it) — the list is
//! mirrored, and [`Mood::ALL`] carries a comment pointing at the source of
//! truth so a divergence is at least a deliberate one.
//!
//! ## Why the shifts are multipliers and not offsets
//!
//! A pack sets a base rate; a mood scales it. `delighted` on a fast voice
//! should be faster than `delighted` on a slow one, not "both end up at 1.1".
//! Multiplication composes; addition would let two innocuous numbers combine
//! into something inaudible, which is why [`crate::tts::SynthParams::sane`]
//! clamps the product rather than trusting it.
//!
//! ## Tiers
//!
//! A pack declares the weakest tier it may still be used at
//! ([`VoicePack::allowed_until`]). Kokoro-82M is roughly real-time on this CPU,
//! which is fine while the machine is hers and unacceptable once a game has it,
//! so it stops at T1 and the registry falls back to a Piper pack. That is SPEC
//! §0.1 expressed as one number in a data file: *any feature that can make a
//! game drop a frame must be sheddable.*

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use wisp_proto::Tier;

use crate::tts::SynthParams;
use crate::{Result, VoiceError};

/// Which engine a pack wants. The registry never *runs* one — it only records
/// what a pack asked for, and the host decides what it can actually build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    /// VITS through ONNX. Small, fast, streams well, the default.
    Piper,
    /// Kokoro-82M through ONNX. Better, heavier, T0/T1 only.
    Kokoro,
    /// The deterministic in-tree fake. Always available, needs nothing.
    Fake,
}

/// Her eight moods.
///
/// Serialised in lower case, matching `wisp_rig::clip::REQUIRED_EXPRESSIONS`
/// exactly. See the module docs for why this list is mirrored rather than
/// imported.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Mood {
    #[default]
    Neutral,
    Curious,
    Delighted,
    Smug,
    Worried,
    Bored,
    Sleepy,
    Alarmed,
}

impl Mood {
    /// The same eight names, in the same order, as
    /// `wisp_rig::clip::REQUIRED_EXPRESSIONS`. That crate is the source of
    /// truth; this is a mirror, because SPEC §2 does not let `wisp-voice`
    /// depend on `wisp-rig`.
    pub const ALL: [Mood; 8] = [
        Mood::Neutral,
        Mood::Curious,
        Mood::Delighted,
        Mood::Smug,
        Mood::Worried,
        Mood::Bored,
        Mood::Sleepy,
        Mood::Alarmed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Mood::Neutral => "neutral",
            Mood::Curious => "curious",
            Mood::Delighted => "delighted",
            Mood::Smug => "smug",
            Mood::Worried => "worried",
            Mood::Bored => "bored",
            Mood::Sleepy => "sleepy",
            Mood::Alarmed => "alarmed",
        }
    }

    /// Parse one of the eight names, or one of `wisp-mind`'s nine — see
    /// [`Mood::from_mind`].
    pub fn parse(s: &str) -> Option<Mood> {
        let s = s.trim();
        Mood::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s))
            .or_else(|| Mood::from_mind(s))
    }

    /// Map a `wisp_mind::mood::Mood` name onto the nearest expression.
    ///
    /// **This is an adapter around a missing shared type, and it is lossy.**
    /// Three vocabularies for the same idea currently exist in the tree:
    ///
    /// | crate | names |
    /// |---|---|
    /// | `wisp-rig` (`REQUIRED_EXPRESSIONS`) | neutral curious delighted smug worried bored sleepy alarmed |
    /// | `wisp-mind` (`mood::Mood`) | calm curious playful smug sulky focused sleepy alarmed affectionate |
    /// | here | the rig's eight |
    ///
    /// They agree on four names out of nine. `wisp-mind` decides the mood,
    /// `wisp-voice` colours her speech with it and `wisp-rig` draws it, so this
    /// is a type shared across three crate boundaries — which by SPEC §3 means
    /// it belongs in `wisp-proto`, and it is not there. Until it is, somebody
    /// has to do this translation, and doing it here means the loss is written
    /// down in one place with a test rather than improvised at a call site.
    ///
    /// `focused` is the unhappy one: it is a *cognitive* state with no facial
    /// or vocal counterpart in the other two lists, so it lands on `neutral` and
    /// something real is lost. That is the argument for the shared type, not for
    /// a cleverer table.
    pub fn from_mind(name: &str) -> Option<Mood> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "calm" => Mood::Neutral,
            "curious" => Mood::Curious,
            "playful" => Mood::Delighted,
            "smug" => Mood::Smug,
            "sulky" => Mood::Bored,
            // No counterpart anywhere else. See the note above.
            "focused" => Mood::Neutral,
            "sleepy" => Mood::Sleepy,
            "alarmed" => Mood::Alarmed,
            "affectionate" => Mood::Delighted,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Mood {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one mood does to a voice. Multipliers on the pack's own base values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MoodShift {
    #[serde(default = "one")]
    pub pitch: f32,
    #[serde(default = "one")]
    pub rate: f32,
    #[serde(default = "one")]
    pub volume: f32,
}

fn one() -> f32 {
    1.0
}

impl Default for MoodShift {
    fn default() -> Self {
        MoodShift { pitch: 1.0, rate: 1.0, volume: 1.0 }
    }
}

/// One voice, as it appears on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoicePack {
    pub id: String,
    /// What the operator sees in the picker.
    pub name: String,
    pub engine: Engine,
    /// The [`crate::models`] manifest id of the weights. Empty for `Fake`.
    #[serde(default)]
    pub model: String,
    /// The manifest id of the engine's side-car config, if it needs one — Piper
    /// keeps its phoneme table and sample rate in a `.onnx.json` beside the
    /// weights, and the model is unusable without it.
    #[serde(default)]
    pub config: String,
    /// Speaker index inside a multi-speaker model.
    #[serde(default)]
    pub speaker: Option<i64>,

    #[serde(default = "one")]
    pub pitch: f32,
    #[serde(default = "one")]
    pub rate: f32,
    #[serde(default = "one")]
    pub volume: f32,

    /// The weakest tier this pack may still be used at. `tier <= allowed_until`.
    #[serde(default = "allowed_until_default")]
    pub allowed_until: Tier,

    /// Per-mood multipliers. Absent moods fall back to no change, so a pack that
    /// only cares about `sleepy` is three lines long.
    #[serde(default)]
    pub moods: BTreeMap<Mood, MoodShift>,

    /// What she says instead of reading a fenced code block out loud. In the
    /// pack because it is a line of her dialogue, and dialogue is not code.
    #[serde(default = "code_placeholder_default")]
    pub code_placeholder: String,
}

fn allowed_until_default() -> Tier {
    Tier::Lobotomised
}

fn code_placeholder_default() -> String {
    "there's a code block here".to_string()
}

impl VoicePack {
    /// Resolve this pack plus a mood into the numbers an engine understands.
    pub fn params(&self, mood: Mood) -> SynthParams {
        let s = self.moods.get(&mood).copied().unwrap_or_default();
        SynthParams {
            voice: self.id.clone(),
            speaker: self.speaker,
            pitch: self.pitch * s.pitch,
            rate: self.rate * s.rate,
            volume: self.volume * s.volume,
        }
        .sane()
    }

    /// May the governor let her use this pack right now?
    pub fn usable_at(&self, tier: Tier) -> bool {
        // T4 is silence, whatever the pack says.
        tier != Tier::Dormant && tier <= self.allowed_until
    }

    /// Every manifest id this pack needs on disk before it can speak.
    pub fn required_models(&self) -> Vec<&str> {
        [self.model.as_str(), self.config.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The built-in packs — data, embedded verbatim
// ---------------------------------------------------------------------------

/// Shipped packs, as the exact JSON an operator would write.
///
/// `include_str!` rather than a `VoicePack { .. }` literal on purpose: it makes
/// the built-ins go through [`serde_json`] like everybody else's, so a field
/// that stops deserialising breaks the built-in packs first, in this crate's own
/// tests, rather than silently in an operator's `voices/` directory.
pub const BUILTIN_PACKS: &[&str] = &[
    include_str!("packs/wisp.json"),
    include_str!("packs/wisp-warm.json"),
    include_str!("packs/wisp-fine.json"),
    include_str!("packs/test-tone.json"),
];

/// Every pack she knows about, and which one is selected.
#[derive(Debug, Clone)]
pub struct VoiceRegistry {
    packs: Vec<VoicePack>,
    selected: String,
}

impl Default for VoiceRegistry {
    fn default() -> Self {
        VoiceRegistry::builtin()
    }
}

impl VoiceRegistry {
    /// The shipped packs only.
    pub fn builtin() -> Self {
        let packs: Vec<VoicePack> = BUILTIN_PACKS
            .iter()
            .map(|s| {
                serde_json::from_str(s).expect("a built-in voice pack failed to parse — see the test")
            })
            .collect();
        let selected = packs
            .first()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "wisp".to_string());
        VoiceRegistry { packs, selected }
    }

    /// The shipped packs plus every `*.json` in `dir`.
    ///
    /// An operator's pack with the same id **replaces** the built-in — that is
    /// how you retune a shipped voice without forking it. A file that does not
    /// parse is logged and skipped: one bad pack must not cost her her voice.
    pub fn load_dir(dir: &Path) -> Self {
        let mut reg = VoiceRegistry::builtin();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return reg;
        };
        let mut found: Vec<VoicePack> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<VoicePack>(&s).map_err(|e| e.to_string()))
            {
                Ok(pack) => found.push(pack),
                Err(why) => tracing::warn!(path = %p.display(), %why, "voice pack ignored"),
            }
        }
        // Deterministic order regardless of what `read_dir` felt like today.
        found.sort_by(|a, b| a.id.cmp(&b.id));
        for pack in found {
            reg.insert(pack);
        }
        reg
    }

    /// Where an operator's packs live.
    pub fn user_dir() -> std::path::PathBuf {
        crate::data_dir().join("voices")
    }

    pub fn insert(&mut self, pack: VoicePack) {
        match self.packs.iter_mut().find(|p| p.id == pack.id) {
            Some(slot) => *slot = pack,
            None => self.packs.push(pack),
        }
    }

    pub fn ids(&self) -> Vec<&str> {
        self.packs.iter().map(|p| p.id.as_str()).collect()
    }

    pub fn all(&self) -> &[VoicePack] {
        &self.packs
    }

    pub fn get(&self, id: &str) -> Option<&VoicePack> {
        self.packs.iter().find(|p| p.id == id)
    }

    pub fn select(&mut self, id: &str) -> Result<()> {
        if self.get(id).is_none() {
            return Err(VoiceError::NoSuchVoice(id.to_string()));
        }
        self.selected = id.to_string();
        Ok(())
    }

    pub fn selected_id(&self) -> &str {
        &self.selected
    }

    /// The selected pack. Never `None`: the registry always holds the built-ins,
    /// and losing her voice because a config file named a pack that no longer
    /// exists would be a spectacularly annoying failure mode.
    pub fn selected(&self) -> &VoicePack {
        self.get(&self.selected)
            .or_else(|| self.packs.first())
            .expect("the registry always holds at least one built-in pack")
    }

    /// What she should actually speak with at `tier`.
    ///
    /// The selected pack if the governor allows it; otherwise the best pack that
    /// is allowed, preferring one from the same engine family and falling back
    /// to something that needs no model at all. Returns `None` at T4, which is
    /// silence by definition.
    pub fn for_tier(&self, tier: Tier) -> Option<&VoicePack> {
        if tier == Tier::Dormant {
            return None;
        }
        let sel = self.selected();
        if sel.usable_at(tier) {
            return Some(sel);
        }
        self.packs
            .iter()
            .filter(|p| p.usable_at(tier))
            // Prefer a real engine over the fake, then the most permissive.
            .max_by_key(|p| {
                let real = u8::from(p.engine != Engine::Fake);
                (real, p.allowed_until as u8)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_pack_parses_and_is_coherent() {
        let reg = VoiceRegistry::builtin();
        assert!(reg.all().len() >= 3, "one voice is not 'multiple voices' (F35)");
        for p in reg.all() {
            assert!(!p.id.is_empty() && !p.name.is_empty(), "{p:?}");
            assert!(p.pitch > 0.0 && p.rate > 0.0 && p.volume >= 0.0, "{}", p.id);
            assert!(!p.code_placeholder.is_empty(), "{} has nothing to say about code", p.id);
            if p.engine != Engine::Fake {
                assert!(!p.model.is_empty(), "{} names no model", p.id);
            }
            if p.engine == Engine::Piper {
                assert!(!p.config.is_empty(), "a Piper voice is unusable without its .onnx.json");
            }
        }
    }

    #[test]
    fn pack_ids_are_unique() {
        let reg = VoiceRegistry::builtin();
        let mut ids = reg.ids();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn a_pack_round_trips_through_json_unchanged() {
        // The built-ins go through serde like everyone else's; prove the trip
        // is lossless so an operator editing an exported pack gets what they saw.
        for p in VoiceRegistry::builtin().all() {
            let s = serde_json::to_string(p).unwrap();
            let back: VoicePack = serde_json::from_str(&s).unwrap();
            assert_eq!(&back, p);
        }
    }

    #[test]
    fn a_minimal_pack_needs_almost_no_fields() {
        // F35 says data, not code. A three-line pack must work, or it is not.
        let p: VoicePack = serde_json::from_str(
            r#"{"id":"tiny","name":"Tiny","engine":"Fake"}"#,
        )
        .unwrap();
        assert_eq!(p.pitch, 1.0);
        assert_eq!(p.rate, 1.0);
        assert_eq!(p.allowed_until, Tier::Lobotomised);
        assert!(p.moods.is_empty());
        assert!(!p.code_placeholder.is_empty());
    }

    #[test]
    fn mood_names_match_the_rigs_eight_expressions() {
        // Mirrored from wisp_rig::clip::REQUIRED_EXPRESSIONS. If that list moves,
        // this is the test that says so.
        let want = ["neutral", "curious", "delighted", "smug", "worried", "bored", "sleepy", "alarmed"];
        let got: Vec<&str> = Mood::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(got, want);
        for w in want {
            assert_eq!(Mood::parse(w).unwrap().as_str(), w);
        }
        assert!(Mood::parse("furious").is_none(), "the set is closed");
    }

    #[test]
    fn every_mood_wisp_mind_can_produce_maps_onto_an_expression() {
        // Mirrored from `wisp_mind::mood::Mood::ALL`. If `wisp-mind` grows a
        // tenth mood, this is the test that notices she has no voice for it.
        let mind = [
            "calm",
            "curious",
            "playful",
            "smug",
            "sulky",
            "focused",
            "sleepy",
            "alarmed",
            "affectionate",
        ];
        for m in mind {
            let got = Mood::from_mind(m).unwrap_or_else(|| panic!("{m} has no expression"));
            assert!(Mood::ALL.contains(&got));
            // …and it round-trips through the general parser too.
            assert_eq!(Mood::parse(m), Some(got));
        }
        assert!(Mood::from_mind("ecstatic").is_none());
    }

    #[test]
    fn the_two_vocabularies_agree_where_they_share_a_name() {
        // The four names both lists happen to use must not be translated into
        // something else by accident.
        for shared in ["curious", "smug", "sleepy", "alarmed"] {
            assert_eq!(Mood::from_mind(shared), Mood::parse(shared), "{shared}");
            assert_eq!(Mood::parse(shared).unwrap().as_str(), shared);
        }
    }

    #[test]
    fn a_mood_name_is_read_case_insensitively() {
        // Config files and CLI arguments are written by hand.
        assert_eq!(Mood::parse("Delighted"), Some(Mood::Delighted));
        assert_eq!(Mood::parse("  SLEEPY "), Some(Mood::Sleepy));
    }

    #[test]
    fn a_mood_bends_the_voice_it_is_applied_to_rather_than_replacing_it() {
        let reg = VoiceRegistry::builtin();
        let p = reg.selected();
        let calm = p.params(Mood::Neutral);
        let up = p.params(Mood::Delighted);
        let down = p.params(Mood::Sleepy);
        assert!(up.rate > calm.rate, "delighted should be quicker");
        assert!(down.rate < calm.rate, "sleepy should be slower");
        assert!(up.pitch > down.pitch);
        assert_eq!(calm.voice, p.id);
    }

    #[test]
    fn the_same_mood_on_two_packs_gives_two_different_voices() {
        // Multipliers compose with the pack's own base; that is the whole reason
        // they are multipliers.
        let reg = VoiceRegistry::builtin();
        let a = reg.get("wisp").unwrap().params(Mood::Delighted);
        let b = reg.get("wisp-warm").unwrap().params(Mood::Delighted);
        assert_ne!((a.pitch, a.rate), (b.pitch, b.rate));
    }

    #[test]
    fn an_absurd_pack_is_clamped_back_into_the_audible() {
        let p: VoicePack = serde_json::from_str(
            r#"{"id":"x","name":"X","engine":"Fake","pitch":9.0,"rate":0.01,
                "moods":{"alarmed":{"pitch":9.0,"rate":9.0}}}"#,
        )
        .unwrap();
        let s = p.params(Mood::Alarmed);
        assert!((0.5..=2.0).contains(&s.pitch), "{}", s.pitch);
        assert!((0.5..=2.0).contains(&s.rate), "{}", s.rate);
    }

    #[test]
    fn the_governor_can_take_the_expensive_voice_away() {
        let mut reg = VoiceRegistry::builtin();
        let heavy = reg
            .all()
            .iter()
            .find(|p| p.engine == Engine::Kokoro)
            .expect("a quality pack should exist")
            .id
            .clone();
        reg.select(&heavy).unwrap();
        assert_eq!(reg.for_tier(Tier::Full).unwrap().id, heavy, "T1 keeps the good voice");
        let reduced = reg.for_tier(Tier::Reduced).unwrap();
        assert_ne!(reduced.id, heavy, "T2 must fall back to something cheaper");
        assert!(reduced.usable_at(Tier::Reduced));
        let lobo = reg.for_tier(Tier::Lobotomised).unwrap();
        assert!(lobo.usable_at(Tier::Lobotomised));
        assert!(reg.for_tier(Tier::Dormant).is_none(), "T4 is silence");
    }

    #[test]
    fn a_pack_is_never_usable_at_t4_however_it_is_written() {
        let p: VoicePack = serde_json::from_str(
            r#"{"id":"x","name":"X","engine":"Fake","allowed_until":"Dormant"}"#,
        )
        .unwrap();
        assert!(!p.usable_at(Tier::Dormant), "T4 is silence, whatever the file says");
        assert!(p.usable_at(Tier::Lobotomised));
    }

    #[test]
    fn selecting_a_pack_that_does_not_exist_is_refused_and_changes_nothing() {
        let mut reg = VoiceRegistry::builtin();
        let before = reg.selected_id().to_string();
        assert!(reg.select("nope").is_err());
        assert_eq!(reg.selected_id(), before);
    }

    #[test]
    fn an_operator_pack_is_picked_up_and_can_replace_a_built_in() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let dir = tmp.path().join("voices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mine.json"),
            r#"{"id":"mine","name":"Mine","engine":"Fake","rate":1.3}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("override.json"),
            r#"{"id":"wisp","name":"Retuned","engine":"Fake","pitch":0.8}"#,
        )
        .unwrap();
        let reg = VoiceRegistry::load_dir(&dir);
        assert_eq!(reg.get("mine").unwrap().rate, 1.3);
        assert_eq!(reg.get("wisp").unwrap().name, "Retuned", "same id replaces");
        assert_eq!(
            reg.ids().iter().filter(|i| **i == "wisp").count(),
            1,
            "replacing must not duplicate"
        );
    }

    #[test]
    fn one_unparseable_pack_does_not_cost_her_her_voice() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("NX_WISP_CONFIG_DIR", tmp.path());
        let dir = tmp.path().join("voices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.json"), "{ not json at all").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();
        let reg = VoiceRegistry::load_dir(&dir);
        assert!(reg.all().len() >= 3);
        assert!(reg.get("wisp").is_some());
    }

    #[test]
    fn a_missing_voices_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = VoiceRegistry::load_dir(&tmp.path().join("does-not-exist"));
        assert!(!reg.all().is_empty());
    }

    #[test]
    fn every_pack_names_only_models_that_exist_in_the_manifest() {
        // A pack that names a model nothing can download is a voice that will
        // fail on first use, on the operator's machine, in front of them.
        for p in VoiceRegistry::builtin().all() {
            for id in p.required_models() {
                assert!(
                    crate::models::MANIFEST.iter().any(|e| e.id == id),
                    "pack {} names unknown model {id}",
                    p.id
                );
            }
        }
    }
}
