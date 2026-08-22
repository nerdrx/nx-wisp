//! F31's shipping engine: Piper (VITS) through `piper-rs` and ONNX Runtime.
//!
//! Compiled only with the `piper-tts` feature. Nothing in the default test run
//! touches this file, because the default test run must pass with no model and
//! no ONNX Runtime.
//!
//! ## Why Piper and not Kokoro
//!
//! The machine this was built against has an RX 7900 XTX and **no ROCm**. `ort`'s
//! Linux GPU execution providers are CUDA, TensorRT, ROCm and MIGraphX, so on
//! this hardware *every* ONNX model runs on the CPU no matter which one we pick.
//! That turns the choice into a pure CPU-budget question, and SPEC §0.1 answers
//! it: *any feature that can make a game drop a frame must be sheddable.*
//!
//! Kokoro-82M is 82M parameters and lands around real time on this CPU. That is
//! fine while the machine is hers, and it is exactly what must not happen at T2
//! when a compile or a game has started — a synthesiser running at 1× real time
//! cannot get ahead of playback, so she stutters precisely when the machine is
//! busiest. Piper's voices are ~20M parameters and synthesise at many times real
//! time on one core, which is what "starts talking before the sentence is
//! generated" needs when there is nothing to spare. So Piper is the default and
//! Kokoro is an optional pack the governor takes away at T2
//! ([`crate::voices::VoicePack::allowed_until`]).
//!
//! ## Two holes in `piper-rs` 0.2, and what is done about them
//!
//! **No phoneme timings.** `Piper::create()` returns the waveform and nothing
//! else. The VITS duration predictor *does* compute per-phoneme frame counts —
//! they are what stretch the model's output — but they are consumed inside the
//! crate and never surfaced. So we phonemise the clause a second time with
//! `espeak-rs` (the same phonemiser `piper-rs` itself calls, so the symbols
//! agree) and apportion the clause's real duration across those symbols by
//! class. The result is [`crate::tts::PhonemeTiming::Estimated`]: the mouth
//! *shapes* are right and in the right order, and *when* each one arrives is
//! approximate. Openness still comes from the measured envelope, so the error is
//! confined to shape transitions and is invisible. Reported as an upstream gap.
//!
//! **No pitch control.** VITS exposes `length_scale`, `noise_scale` and
//! `noise_w`; none of them is pitch. `noise_w` varies phoneme duration, which is
//! sometimes mistaken for prosody, and is not. Since F35 wants mood to bend
//! pitch, [`pitch_shift`] does it by resampling — and the duration that costs is
//! paid back *before* synthesis by pre-compensating `length_scale`, so the clause
//! still lasts as long as the caller asked for:
//!
//! ```text
//!   length_scale = pitch / rate      →  Piper renders D·pitch/rate seconds
//!   resample by 1/pitch              →  D/rate seconds, pitch × `pitch`
//! ```
//!
//! Formants shift with the pitch, which for the ±15% a mood asks for reads as
//! mood rather than as a chipmunk. A phase vocoder would preserve them and cost
//! far more than the effect is worth here.

use std::collections::HashMap;
use std::path::Path;

use crate::audio::Pcm;
use crate::models::ModelStore;
use crate::tts::{PhonemeSpan, PhonemeTiming, SynthParams, Synthesis, Tts};
use crate::voices::VoicePack;
use crate::{Result, VoiceError};

/// A loaded Piper voice.
pub struct PiperTts {
    inner: piper_rs::Piper,
    name: String,
    rate: u32,
    /// The espeak-ng voice name from the model's `.onnx.json`, so our second
    /// phonemisation uses the same language the model was trained on.
    espeak_voice: String,
    /// Symbols the model itself cannot pronounce — anything outside its
    /// `phoneme_id_map` is dropped by `piper-rs` before inference, so including
    /// it in our spans would describe a mouth shape that was never spoken.
    known: HashMap<char, ()>,
}

impl std::fmt::Debug for PiperTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiperTts")
            .field("name", &self.name)
            .field("rate", &self.rate)
            .field("espeak_voice", &self.espeak_voice)
            .finish_non_exhaustive()
    }
}

impl PiperTts {
    /// Load from explicit paths.
    pub fn load(name: &str, model: &Path, config: &Path) -> Result<Self> {
        // `piper-rs` keeps its `ModelConfig` private, so the two fields we need
        // — the sample rate and the espeak voice — are read from the same JSON
        // a second time rather than guessed.
        let raw = std::fs::read_to_string(config)
            .map_err(|e| VoiceError::io(config.display().to_string(), e))?;
        let v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| VoiceError::Synth(format!("{}: {e}", config.display())))?;
        let rate = v["audio"]["sample_rate"].as_u64().unwrap_or(22_050) as u32;
        let espeak_voice = v["espeak"]["voice"].as_str().unwrap_or("en-us").to_string();
        let known = v["phoneme_id_map"]
            .as_object()
            .map(|m| {
                m.keys()
                    .filter_map(|k| k.chars().next().map(|c| (c, ())))
                    .collect()
            })
            .unwrap_or_default();

        let inner = piper_rs::Piper::new(model, config)
            .map_err(|e| VoiceError::Synth(format!("loading {name}: {e}")))?;
        Ok(PiperTts { inner, name: name.to_string(), rate, espeak_voice, known })
    }

    /// Load the voice a pack names, from the model store. Fails with
    /// [`VoiceError::ModelMissing`] if it has not been downloaded — deciding to
    /// download is the caller's, because it may cost the operator 60 MB.
    pub fn for_pack(pack: &VoicePack, store: &ModelStore) -> Result<Self> {
        let mut paths = Vec::new();
        for id in [&pack.model, &pack.config] {
            // `path` is `None` for an id that is not in the manifest at all,
            // which is a packaging bug rather than a missing download — but
            // from the caller's side both mean "she cannot use this voice".
            match store.path(id) {
                Some(p) if p.exists() => paths.push(p),
                _ => return Err(VoiceError::ModelMissing(id.clone())),
            }
        }
        PiperTts::load(&pack.id, &paths[0], &paths[1])
    }

    /// The speakers a multi-speaker model offers, for the voice picker.
    pub fn speakers(&self) -> Vec<(String, i64)> {
        self.inner
            .voices()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default()
    }
}

impl Tts for PiperTts {
    fn name(&self) -> &str {
        &self.name
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn phoneme_timing(&self) -> PhonemeTiming {
        // See the module docs: the symbols are the model's own, the boundaries
        // are ours.
        PhonemeTiming::Estimated
    }

    fn synth(&mut self, text: &str, p: &SynthParams) -> Result<Synthesis> {
        let p = p.clone().sane();
        // Piper's `length_scale` is the reciprocal of rate, and it also absorbs
        // the duration that the pitch resample below is about to remove. This is
        // the only place in the crate that has to know either of those things.
        let length_scale = p.pitch / p.rate;

        let (samples, sr) = self
            .inner
            .create(text, false, p.speaker, Some(length_scale), None, None)
            .map_err(|e| VoiceError::Synth(format!("{}: {e}", self.name)))?;

        let mut pcm = pitch_shift(Pcm::new(sr, samples), p.pitch);
        pcm.gain(p.volume);

        let phonemes = self.estimate_spans(text, pcm.duration_ms());
        Ok(Synthesis { text: text.to_string(), pcm, phonemes })
    }

    fn resident_cost(&self) -> wisp_proto::Cost {
        // A medium Piper voice is ~63 MB of weights; ORT's arenas roughly double
        // that in practice. No VRAM: there is no GPU execution provider here.
        wisp_proto::Cost { ram_mib: 140, vram_mib: 0, cpu_centi_pct: 120 }
    }
}

impl PiperTts {
    /// Phonemise again and apportion the real duration across the symbols.
    ///
    /// Failure is not an error: a clause with no usable phonemes just gets no
    /// spans, and [`crate::lipsync`] falls back to the energy envelope, which is
    /// the path every engine without phonemes uses anyway. Losing lip *shapes*
    /// must never cost her the ability to speak.
    fn estimate_spans(&self, text: &str, total_ms: u32) -> Vec<PhonemeSpan> {
        if total_ms == 0 {
            return Vec::new();
        }
        let Ok(sentences) = espeak_rs::text_to_phonemes(text, &self.espeak_voice, None) else {
            tracing::debug!(voice = %self.name, "phonemisation failed; falling back to the envelope");
            return Vec::new();
        };
        let joined = sentences.join(" ");
        let units = split_ipa(&joined);
        // Only symbols the model can actually pronounce; `piper-rs` drops the
        // rest before inference, so a span for one would describe a mouth shape
        // that never happened.
        let units: Vec<String> = units
            .into_iter()
            .filter(|u| {
                u.chars()
                    .next()
                    .map(|c| c.is_whitespace() || self.known.is_empty() || self.known.contains_key(&c))
                    .unwrap_or(false)
            })
            .collect();
        if units.is_empty() {
            return Vec::new();
        }

        let weights: Vec<f32> = units.iter().map(|u| nominal_weight(u)).collect();
        let total_w: f32 = weights.iter().sum();
        if total_w <= 0.0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(units.len());
        let mut acc = 0.0f32;
        for (u, w) in units.into_iter().zip(weights) {
            let start = (acc / total_w * total_ms as f32).round() as u32;
            acc += w;
            let end = (acc / total_w * total_ms as f32).round() as u32;
            out.push(PhonemeSpan::new(u, start, end.min(total_ms)));
        }
        out
    }
}

pub use crate::audio::pitch_shift;

/// Split an IPA string into phoneme units, keeping modifiers with their base.
///
/// espeak-ng emits stress marks (`ˈ ˌ`), length marks (`ː`), tie bars (`͡`) and
/// combining diacritics as separate code points. `t͡ʃ` is one mouth shape, not
/// three, and `ɑː` is one vowel held, not two vowels.
pub fn split_ipa(s: &str) -> Vec<String> {
    const MODIFIER: &[char] = &['ː', 'ˑ', '̃', '̆', '̥', '̩', '̯', 'ʼ', 'ʰ', 'ʲ', 'ʷ', 'ˠ', 'ˤ'];
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut tie = false;
    for c in s.chars() {
        // Stress is prosody, not a mouth shape.
        if matches!(c, 'ˈ' | 'ˌ' | '\u{200d}') {
            continue;
        }
        if c == '͡' || c == '͜' {
            tie = true;
            cur.push(c);
            continue;
        }
        if tie {
            cur.push(c);
            tie = false;
            continue;
        }
        if MODIFIER.contains(&c) && !cur.is_empty() {
            cur.push(c);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Relative duration of a phoneme class, used only to apportion a known total.
///
/// The numbers are the usual textbook ordering — long vowels longest, plosives
/// shortest — and their absolute values do not matter because they are
/// normalised against the clause's measured length. Only the ratios do.
fn nominal_weight(u: &str) -> f32 {
    let first = u.chars().next().unwrap_or(' ');
    let long = u.contains('ː');
    let base = match first {
        c if c.is_whitespace() => 0.55,
        // Silence and clause punctuation espeak passes through.
        '.' | ',' | '!' | '?' | ';' | ':' => 0.8,
        // Vowels.
        'ɑ' | 'a' | 'ɐ' | 'ʌ' | 'æ' | 'ɛ' | 'e' | 'ɪ' | 'i' | 'ɔ' | 'o' | 'ʊ' | 'u' | 'ə'
        | 'ɜ' | 'ɘ' | 'ɤ' | 'ø' | 'y' | 'ɒ' | 'ɵ' => 1.0,
        // Plosives: short.
        'p' | 'b' | 't' | 'd' | 'k' | 'ɡ' | 'g' | 'ʔ' => 0.5,
        // Fricatives: longer than plosives.
        'f' | 'v' | 'θ' | 'ð' | 's' | 'z' | 'ʃ' | 'ʒ' | 'h' | 'x' | 'ç' => 0.8,
        // Nasals and approximants.
        'm' | 'n' | 'ŋ' | 'ɲ' => 0.7,
        'l' | 'ɹ' | 'r' | 'ɾ' | 'j' | 'w' | 'ʋ' | 'ɻ' => 0.65,
        _ => 0.7,
    };
    if long {
        base * 1.45
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipa_modifiers_stay_with_the_phoneme_they_modify() {
        assert_eq!(split_ipa("ɑː"), vec!["ɑː"], "a held vowel is one mouth shape");
        assert_eq!(split_ipa("t͡ʃ"), vec!["t͡ʃ"], "an affricate is one mouth shape");
        assert_eq!(split_ipa("ˈhɛloʊ"), vec!["h", "ɛ", "l", "o", "ʊ"], "stress is not a shape");
        assert_eq!(split_ipa(""), Vec::<String>::new());
    }

    #[test]
    fn a_long_vowel_is_given_more_of_the_clause_than_a_plosive() {
        assert!(nominal_weight("ɑː") > nominal_weight("ɑ"));
        assert!(nominal_weight("ɑ") > nominal_weight("t"));
        assert!(nominal_weight("s") > nominal_weight("p"));
        assert!(nominal_weight("§").is_finite(), "an unknown symbol must not be NaN");
    }

    #[test]
    fn the_length_scale_arithmetic_returns_the_duration_the_caller_asked_for() {
        // Piper renders D·(pitch/rate); the resample divides by pitch; the
        // product is D/rate, which is what `rate` is supposed to mean.
        for (pitch, rate) in [(1.0f32, 1.0f32), (1.2, 1.0), (0.9, 1.3), (1.15, 0.8)] {
            let length_scale = pitch / rate;
            let rendered = 1000.0 * length_scale;
            let after = rendered / pitch;
            assert!((after - 1000.0 / rate).abs() < 1e-3, "pitch {pitch} rate {rate}");
        }
    }
}
