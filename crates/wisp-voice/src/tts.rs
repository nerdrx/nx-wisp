//! F31 — the synthesis contract, and the fake that stands in for a real engine.
//!
//! One trait, [`Tts`], with two real implementations behind cargo features
//! (`piper-tts`, and Kokoro through the same `ort` path) and one deterministic
//! fake that is always compiled. The trait is deliberately **blocking and
//! chunk-at-a-time**: streaming (F31's "starts talking before the sentence is
//! generated") is [`crate::speaker`]'s job, built out of many small synchronous
//! calls, not something an engine has to implement. That split is what lets the
//! whole streaming pipeline be tested without an engine at all.
//!
//! ## Why the engine returns phonemes if it can
//!
//! F32 wants viseme timings, not just an energy envelope. Piper's front end
//! phonemises with espeak-ng before it runs the model, and the VITS duration
//! predictor emits a per-phoneme frame count — so the timings are *already
//! computed*, and throwing them away to re-derive mouth shapes from the audio
//! would be strictly worse. [`Synthesis::phonemes`] is therefore optional but
//! preferred, and [`crate::lipsync`] uses it when present and falls back to the
//! energy envelope when it is not.

use crate::audio::Pcm;
use crate::Result;

/// How much to trust the timings on a [`Synthesis`]'s phoneme spans.
///
/// This distinction is not pedantry. `piper-rs` 0.2 phonemises with espeak-ng
/// and then runs a VITS duration predictor, but its `create()` returns only the
/// waveform — the per-phoneme frame counts the model computed are discarded
/// inside the crate and there is no API to reach them. So the shipping engine
/// can tell us *which* phonemes it said and in what order, but not *when*.
///
/// [`crate::lipsync`] handles both: viseme identity comes from the symbols
/// (which are right either way) and openness comes from the measured energy
/// envelope (which is right either way). Estimated timings only decide when one
/// mouth shape gives way to the next, and being a few tens of milliseconds off
/// there is invisible. Silently presenting an estimate as a measurement is what
/// would not be acceptable, so the engine has to say which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhonemeTiming {
    /// No spans at all.
    None,
    /// Symbols are real; the boundaries are apportioned, not observed.
    Estimated,
    /// The engine reported where each phoneme actually landed.
    Measured,
}

/// One phoneme, and where it lands in the synthesised audio.
///
/// `symbol` is whatever alphabet the engine speaks — IPA for espeak-ng-based
/// engines, an arbitrary token set for anything else. [`crate::lipsync`] maps it
/// to a [`crate::Viseme`] and is the only place that needs to know the alphabet.
/// How far to trust `start_ms`/`end_ms` is [`Tts::phoneme_timing`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhonemeSpan {
    pub symbol: String,
    pub start_ms: u32,
    pub end_ms: u32,
}

impl PhonemeSpan {
    pub fn new(symbol: impl Into<String>, start_ms: u32, end_ms: u32) -> Self {
        PhonemeSpan {
            symbol: symbol.into(),
            start_ms,
            end_ms: end_ms.max(start_ms),
        }
    }

    pub fn duration_ms(&self) -> u32 {
        self.end_ms.saturating_sub(self.start_ms)
    }
}

/// How this particular clause should sound.
///
/// Split from the [`crate::VoicePack`] on purpose: the pack is *data on disk*
/// (F35) and changes when the operator picks a different voice, while these are
/// the per-utterance numbers her mood has already been folded into. An engine
/// only ever sees the resolved values, so no engine has to know what a mood is.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthParams {
    /// Which pack, so a multi-model engine knows what to load.
    pub voice: String,
    /// Speaker index inside a multi-speaker model, if it has one.
    pub speaker: Option<i64>,
    /// Multiplier on the fundamental. `1.0` is the voice's own pitch.
    pub pitch: f32,
    /// Multiplier on speaking rate. `1.0` is the voice's own tempo. Higher is
    /// faster, matching every other TTS in the world and *not* matching Piper's
    /// `length_scale`, which is its reciprocal — the Piper backend inverts it in
    /// exactly one place so nothing else has to think about it.
    pub rate: f32,
    /// Linear gain applied after synthesis.
    pub volume: f32,
}

impl Default for SynthParams {
    fn default() -> Self {
        SynthParams {
            voice: "fake".to_string(),
            speaker: None,
            pitch: 1.0,
            rate: 1.0,
            volume: 1.0,
        }
    }
}

impl SynthParams {
    /// Clamp to a band that cannot make her unintelligible.
    ///
    /// A mood shift is allowed to colour her, not to break her: at `rate = 0.1`
    /// a two-word clause takes twenty seconds and the attention budget's
    /// accounting stops meaning anything, and at `pitch = 3.0` she is a
    /// cartoon. Voice packs are data (F35), and data written by hand needs a
    /// guard rail more than code does.
    pub fn sane(mut self) -> Self {
        self.pitch = if self.pitch.is_finite() { self.pitch.clamp(0.5, 2.0) } else { 1.0 };
        self.rate = if self.rate.is_finite() { self.rate.clamp(0.5, 2.0) } else { 1.0 };
        self.volume = if self.volume.is_finite() { self.volume.clamp(0.0, 2.0) } else { 1.0 };
        self
    }
}

/// What one call to an engine produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Synthesis {
    /// The text that was actually spoken, so the flight recorder (SPEC §0.4)
    /// can answer "what did she say" from the same record that holds the audio.
    pub text: String,
    pub pcm: Pcm,
    /// Empty when the engine cannot tell us. Never partially filled.
    pub phonemes: Vec<PhonemeSpan>,
}

impl Synthesis {
    pub fn duration_ms(&self) -> u32 {
        self.pcm.duration_ms()
    }
}

/// A local speech synthesiser.
///
/// Blocking. Implementations run on a worker thread that the host owns; nothing
/// in this crate spawns one, because the governor has to be able to take that
/// thread away (SPEC §3.1) and a thread this crate hid would survive T4.
pub trait Tts: Send {
    /// Stable identifier for the flight recorder and the cost meter.
    fn name(&self) -> &str;

    /// The rate this engine emits. Constant for the life of the engine.
    fn sample_rate(&self) -> u32;

    /// Synthesise one clause. `text` is already a chunk from [`crate::text`] —
    /// an engine must never be handed an unbounded string.
    fn synth(&mut self, text: &str, p: &SynthParams) -> Result<Synthesis>;

    /// Does [`Synthesis::phonemes`] come back populated?
    fn has_phonemes(&self) -> bool {
        self.phoneme_timing() != PhonemeTiming::None
    }

    /// How much the spans' timings can be trusted. See [`PhonemeTiming`].
    fn phoneme_timing(&self) -> PhonemeTiming {
        PhonemeTiming::None
    }

    /// Worst-case resident cost, for `wisp-gov`'s accounting.
    fn resident_cost(&self) -> wisp_proto::Cost {
        wisp_proto::Cost::FREE
    }
}

// ---------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------

/// A synthesiser with no model, no ONNX runtime and no network.
///
/// It is not a stub that returns silence. It produces audio whose **envelope
/// tracks the text it was given** — vowels loud and long, consonants short and
/// quiet, spaces and punctuation silent — and phoneme spans that line up with
/// that audio to the sample. That is the whole point: it makes F32's "feed known
/// audio, assert the envelope tracks it" a real test rather than a tautology,
/// and it makes the streaming pump, the play queue, the ducker and the barge-in
/// arbiter all testable end to end on a machine with no sound card.
///
/// Deterministic: the same text and params always produce byte-identical audio.
#[derive(Debug, Clone)]
pub struct FakeTts {
    rate: u32,
    /// Milliseconds a vowel is held at `rate = 1.0`.
    pub vowel_ms: u32,
    /// Milliseconds a consonant gets.
    pub consonant_ms: u32,
    /// Milliseconds of silence for a space.
    pub space_ms: u32,
    /// Milliseconds of silence for a sentence-ending mark.
    pub stop_ms: u32,
    /// Set to make every call fail, so callers' error paths get exercised.
    pub fail: bool,
    /// Counts calls, so a test can prove the pump synthesised per clause rather
    /// than once at the end.
    pub calls: usize,
    /// Every text it was ever asked for, in order.
    pub seen: Vec<String>,
}

impl Default for FakeTts {
    fn default() -> Self {
        FakeTts {
            rate: 22_050,
            vowel_ms: 90,
            consonant_ms: 50,
            space_ms: 40,
            stop_ms: 140,
            fail: false,
            calls: 0,
            seen: Vec::new(),
        }
    }
}

/// What the fake makes of one character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Vowel,
    Consonant,
    Space,
    Stop,
}

fn classify(c: char) -> Option<Unit> {
    let c = c.to_ascii_lowercase();
    match c {
        'a' | 'e' | 'i' | 'o' | 'u' => Some(Unit::Vowel),
        '.' | '!' | '?' | ',' | ';' | ':' => Some(Unit::Stop),
        c if c.is_whitespace() => Some(Unit::Space),
        c if c.is_ascii_alphanumeric() => Some(Unit::Consonant),
        _ => None,
    }
}

impl FakeTts {
    pub fn new() -> Self {
        FakeTts::default()
    }

    pub fn at_rate(rate: u32) -> Self {
        FakeTts { rate, ..FakeTts::default() }
    }

    /// Amplitude of a unit. Vowels are what a mouth opens for.
    fn amplitude(u: Unit) -> f32 {
        match u {
            Unit::Vowel => 0.7,
            Unit::Consonant => 0.22,
            Unit::Space | Unit::Stop => 0.0,
        }
    }

    /// A per-character "formant", so different letters are distinguishable in a
    /// spectrogram and a test can tell two clauses apart by more than length.
    fn hz(c: char) -> f32 {
        180.0 + ((c as u32 % 17) as f32) * 45.0
    }
}

impl Tts for FakeTts {
    fn name(&self) -> &str {
        "fake"
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// The fake is the one engine in the tree whose timings really are exact:
    /// it decides the durations, so it knows them.
    fn phoneme_timing(&self) -> PhonemeTiming {
        PhonemeTiming::Measured
    }

    fn synth(&mut self, text: &str, p: &SynthParams) -> Result<Synthesis> {
        self.calls += 1;
        self.seen.push(text.to_string());
        if self.fail {
            return Err(crate::VoiceError::Synth(format!("FakeTts was told to fail on {text:?}")));
        }
        let p = p.clone().sane();
        let mut samples: Vec<f32> = Vec::new();
        let mut phonemes = Vec::new();
        let ms_at = |n: usize, rate: u32| ((n as u64 * 1000) / rate as u64) as u32;

        for c in text.chars() {
            let Some(unit) = classify(c) else { continue };
            let base = match unit {
                Unit::Vowel => self.vowel_ms,
                Unit::Consonant => self.consonant_ms,
                Unit::Space => self.space_ms,
                Unit::Stop => self.stop_ms,
            };
            let dur_ms = ((base as f32 / p.rate).round() as u32).max(1);
            let start_ms = ms_at(samples.len(), self.rate);
            let n = (self.rate as u64 * dur_ms as u64 / 1000).max(1) as usize;
            let amp = Self::amplitude(unit) * p.volume;
            if amp > 0.0 {
                let hz = Self::hz(c) * p.pitch;
                // Raised-cosine window, so the envelope is smooth and every
                // segment starts and ends at zero: no clicks, and the extracted
                // envelope is a clean function of which unit this was.
                for i in 0..n {
                    let t = i as f32 / self.rate as f32;
                    let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos();
                    samples.push(((std::f32::consts::TAU * hz * t).sin() * amp * w).clamp(-1.0, 1.0));
                }
            } else {
                samples.extend(std::iter::repeat_n(0.0, n));
            }
            let end_ms = ms_at(samples.len(), self.rate);
            phonemes.push(PhonemeSpan::new(fake_symbol(c, unit), start_ms, end_ms));
        }

        Ok(Synthesis {
            text: text.to_string(),
            pcm: Pcm::new(self.rate, samples),
            phonemes,
        })
    }

    fn resident_cost(&self) -> wisp_proto::Cost {
        wisp_proto::Cost::FREE
    }
}

/// Maps an ASCII character onto a symbol from the alphabet a real engine would
/// use, so the viseme mapping in [`crate::lipsync`] is exercised by the fake
/// rather than only by a backend nobody compiles in CI.
fn fake_symbol(c: char, unit: Unit) -> String {
    match unit {
        Unit::Space | Unit::Stop => "_".to_string(),
        Unit::Vowel => match c.to_ascii_lowercase() {
            'a' => "ɑ",
            'e' => "ɛ",
            'i' => "i",
            'o' => "o",
            _ => "u",
        }
        .to_string(),
        Unit::Consonant => c.to_ascii_lowercase().to_string(),
    }
}

/// An engine that always fails, for testing the shed path without having to
/// mutate a [`FakeTts`] mid-test.
#[derive(Debug, Default, Clone)]
pub struct DeadTts;

impl Tts for DeadTts {
    fn name(&self) -> &str {
        "dead"
    }
    fn sample_rate(&self) -> u32 {
        22_050
    }
    fn synth(&mut self, _text: &str, _p: &SynthParams) -> Result<Synthesis> {
        Err(crate::VoiceError::Synth("no engine".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(t: &str) -> Synthesis {
        FakeTts::new().synth(t, &SynthParams::default()).unwrap()
    }

    #[test]
    fn the_fake_is_deterministic() {
        let a = synth("hello there");
        let b = synth("hello there");
        assert_eq!(a, b);
    }

    #[test]
    fn different_text_sounds_different() {
        assert_ne!(synth("hello").pcm, synth("goodbye").pcm);
        // …and not merely because of length.
        assert_ne!(synth("aaa").pcm, synth("iii").pcm);
    }

    #[test]
    fn vowels_are_louder_and_longer_than_consonants() {
        let v = synth("aaaa");
        let c = synth("tttt");
        assert!(v.pcm.rms() > c.pcm.rms() * 2.0, "{} vs {}", v.pcm.rms(), c.pcm.rms());
        assert!(v.duration_ms() > c.duration_ms());
    }

    #[test]
    fn spaces_and_stops_are_silent() {
        let s = synth("  ");
        assert_eq!(s.pcm.peak(), 0.0);
        assert!(s.pcm.duration_ms() >= 70);
        let stop = synth("...");
        assert_eq!(stop.pcm.peak(), 0.0);
    }

    #[test]
    fn phoneme_spans_tile_the_audio_without_gaps_or_overlap() {
        let s = synth("hello there, friend.");
        assert!(!s.phonemes.is_empty());
        assert_eq!(s.phonemes[0].start_ms, 0);
        for w in s.phonemes.windows(2) {
            assert_eq!(w[0].end_ms, w[1].start_ms, "gap or overlap at {w:?}");
        }
        let last = s.phonemes.last().unwrap().end_ms;
        assert!(
            (last as i64 - s.duration_ms() as i64).abs() <= 2,
            "spans end at {last}, audio ends at {}",
            s.duration_ms()
        );
    }

    #[test]
    fn rate_shortens_and_pitch_does_not() {
        let base = SynthParams::default();
        let fast = SynthParams { rate: 2.0, ..base.clone() };
        let high = SynthParams { pitch: 2.0, ..base.clone() };
        let mut e = FakeTts::new();
        let b = e.synth("hello there", &base).unwrap();
        let f = e.synth("hello there", &fast).unwrap();
        let h = e.synth("hello there", &high).unwrap();
        assert!(f.duration_ms() * 2 <= b.duration_ms() + 40, "faster must be shorter");
        assert_eq!(h.duration_ms(), b.duration_ms(), "pitch must not change tempo");
        assert_ne!(h.pcm, b.pcm, "…but it must change the sound");
    }

    #[test]
    fn volume_scales_and_clamps() {
        let mut e = FakeTts::new();
        let quiet = e
            .synth("aaa", &SynthParams { volume: 0.25, ..Default::default() })
            .unwrap();
        let loud = e
            .synth("aaa", &SynthParams { volume: 1.0, ..Default::default() })
            .unwrap();
        assert!(quiet.pcm.peak() < loud.pcm.peak());
        assert!(loud.pcm.peak() <= 1.0);
    }

    #[test]
    fn sane_clamps_a_hand_written_voice_pack_back_into_the_audible() {
        let p = SynthParams { pitch: 40.0, rate: 0.001, volume: -3.0, ..Default::default() }.sane();
        assert_eq!(p.pitch, 2.0);
        assert_eq!(p.rate, 0.5);
        assert_eq!(p.volume, 0.0);
        let nan = SynthParams {
            pitch: f32::NAN,
            rate: f32::INFINITY,
            volume: f32::NAN,
            ..Default::default()
        }
        .sane();
        assert_eq!((nan.pitch, nan.rate, nan.volume), (1.0, 1.0, 1.0));
    }

    #[test]
    fn empty_and_punctuation_only_text_produce_nothing_rather_than_panicking() {
        assert!(synth("").pcm.is_empty());
        assert!(synth("").phonemes.is_empty());
        let weird = synth("→←※");
        assert!(weird.pcm.is_empty(), "unclassifiable characters are skipped");
    }

    #[test]
    fn it_records_what_it_was_asked_for_so_streaming_can_be_proven() {
        let mut e = FakeTts::new();
        e.synth("one.", &Default::default()).unwrap();
        e.synth("two.", &Default::default()).unwrap();
        assert_eq!(e.calls, 2);
        assert_eq!(e.seen, vec!["one.", "two."]);
    }

    #[test]
    fn a_failing_engine_reports_rather_than_returning_silence() {
        let mut e = FakeTts { fail: true, ..FakeTts::new() };
        assert!(e.synth("hello", &Default::default()).is_err());
        assert!(DeadTts.synth("hello", &Default::default()).is_err());
    }

    #[test]
    fn audio_never_leaves_the_legal_range() {
        let mut e = FakeTts::new();
        let s = e
            .synth(
                "the quick brown fox jumps over the lazy dog!",
                &SynthParams { volume: 2.0, ..Default::default() },
            )
            .unwrap();
        assert!(s.pcm.samples.iter().all(|x| x.is_finite() && (-1.0..=1.0).contains(x)));
    }
}
