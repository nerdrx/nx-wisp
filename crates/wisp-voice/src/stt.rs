//! F28's second half — the transcription contract, and a fake that can prove a
//! streaming pipeline works without a model, a GPU or a microphone.
//!
//! One trait, [`Stt`], with one real backend behind the `whisper-stt` feature
//! and one deterministic fake that is always compiled. The shape mirrors
//! [`crate::tts`] on purpose: the engine is **blocking and window-at-a-time**,
//! and *streaming is the caller's job*. [`crate::mic::Listener`] owns the
//! window, decides when a partial is due and when an utterance has ended, and
//! calls this trait repeatedly. An engine never has to know what a partial is.
//!
//! ## Why "transcribe the whole window", not "feed me samples"
//!
//! whisper is an encoder–decoder over a fixed 30-second mel window. It has no
//! incremental mode: the honest way to get partials out of it is to re-run it on
//! a growing window and show the operator the newest answer. That is what
//! whisper.cpp's own streaming example does, and pretending otherwise behind a
//! `push(samples)` API would be a lie told in the type signature — it would look
//! cheap and cost a full decode per call anyway.
//!
//! So the cost model is visible: `transcribe` on a growing window is roughly
//! O(n²) over the utterance, which is exactly why
//! [`crate::mic::ListenConfig::partial_interval_ms`] exists and why
//! [`crate::mic::ListenConfig::max_window_ms`] is capped near whisper's own
//! 30-second context rather than at "however long the operator leans on the
//! key".
//!
//! ## Confidence is a proxy, and is documented as one
//!
//! [`Transcript::confidence`] is not calibrated and must never be shown to the
//! operator as a percentage. `wisp-mind` uses it for one decision — whether a
//! partial is worth reacting to yet — and a proxy is enough for that. Anything
//! stronger would need per-token logprobs averaged over a segment, which is a
//! number whisper.cpp can produce but which this crate does not currently read;
//! see the note in the `whisper` submodule below. (That module is behind
//! `whisper-stt`, so rustdoc only renders it with the feature on — which is why
//! this is not a link.)
//!
//! ## What is *not* here
//!
//! No audio capture. No consent. No buffering policy. All three live in
//! [`crate::mic`], because an engine that could open a microphone would be an
//! engine that could open a microphone without a permit.

use std::collections::VecDeque;

use crate::audio::{rms, STT_RATE};
use crate::{Result, VoiceError};

/// One answer from an engine about the window it was just given.
///
/// `text` is always the transcription of the **whole window**, never the delta
/// since the last call. A caller that wants a delta can diff two of these; a
/// caller handed deltas could never reconstruct a correction, and correcting an
/// earlier guess is the single most common thing a streaming ASR does.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// Rough 0..=1 proxy. See the module docs: not calibrated, never a
    /// percentage in the UI.
    pub confidence: f32,
    pub final_: bool,
}

impl Transcript {
    pub fn new(text: impl Into<String>, confidence: f32, final_: bool) -> Self {
        Transcript {
            text: text.into(),
            confidence: if confidence.is_finite() {
                confidence.clamp(0.0, 1.0)
            } else {
                0.0
            },
            final_,
        }
    }

    /// Silence in, nothing out. An engine that hallucinates "Thank you." over a
    /// room tone — and whisper does, reliably — should be returning this
    /// instead, which is why [`FakeStt`] models the case at all.
    pub fn empty(final_: bool) -> Self {
        Transcript {
            text: String::new(),
            confidence: 0.0,
            final_,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// A local speech recogniser.
///
/// Blocking, like [`crate::tts::Tts`], and for the same reason: the governor has
/// to be able to take the thread away (SPEC §3.1), and a thread this crate hid
/// inside an engine would survive T4.
pub trait Stt: Send {
    /// Stable identifier for the flight recorder and the cost meter.
    fn name(&self) -> &str;

    /// Audio must be mono f32 at 16 kHz.
    fn sample_rate(&self) -> u32 {
        crate::audio::STT_RATE
    }

    /// Transcribe the whole window fed so far. Called repeatedly on a growing
    /// window for partials, then once with `final_ = true`.
    fn transcribe(&mut self, pcm: &[f32], final_: bool) -> Result<Transcript>;

    /// Forget everything about the previous utterance.
    ///
    /// Called between utterances, and after a shed. An engine that keeps decoder
    /// context across utterances would carry the operator's last sentence into
    /// the next one, which is both a quality bug and a small privacy one.
    fn reset(&mut self);

    /// Worst-case resident cost, for `wisp-gov`'s accounting.
    fn resident_cost(&self) -> wisp_proto::Cost {
        wisp_proto::Cost::FREE
    }
}

// ---------------------------------------------------------------------------
// Cost, per model size
// ---------------------------------------------------------------------------

/// The whisper.cpp model sizes worth shipping, and what each costs to hold.
///
/// Sizes are the published `ggml-*.bin` f16 artefact sizes rounded up, because
/// that is what actually lands in memory: whisper.cpp mmaps the file and, with a
/// GPU backend enabled, uploads the weights. **These are derived from artefact
/// size, not measured on this machine** — good enough for the operator-facing
/// cost meter of SPEC §3.1, not good enough to schedule against. A measured
/// number would be better and would need the model present to obtain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttModel {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3,
    /// `large-v3-turbo`: large's encoder with a four-layer decoder. Near-large
    /// quality at roughly small's latency, which is the only reason a companion
    /// that must answer inside a second can consider a large model at all.
    LargeV3Turbo,
}

impl SttModel {
    /// The `ggml` artefact name, without the `ggml-` prefix or `.bin` suffix.
    pub fn id(self) -> &'static str {
        match self {
            SttModel::Tiny => "tiny.en",
            SttModel::Base => "base.en",
            SttModel::Small => "small.en",
            SttModel::Medium => "medium.en",
            SttModel::LargeV3 => "large-v3",
            SttModel::LargeV3Turbo => "large-v3-turbo",
        }
    }

    /// Approximate weight size in MiB (f16 `ggml`).
    pub fn weights_mib(self) -> u32 {
        match self {
            SttModel::Tiny => 78,
            SttModel::Base => 148,
            SttModel::Small => 488,
            SttModel::Medium => 1_536,
            SttModel::LargeV3 => 3_094,
            SttModel::LargeV3Turbo => 1_620,
        }
    }

    /// What holding this model resident costs, with the Vulkan backend live.
    ///
    /// The weights are counted in **both** pools on purpose: whisper.cpp keeps
    /// the mmapped file around while the GPU copy exists, so the RSS does not go
    /// away when the upload succeeds. Over-reporting a cost makes the governor
    /// shed too eagerly; under-reporting it makes her the reason a game stutters,
    /// and SPEC §0.1 is unambiguous about which of those is the worse failure.
    pub fn resident_cost(self) -> wisp_proto::Cost {
        let w = self.weights_mib();
        wisp_proto::Cost {
            // Mel front end, KV cache and the decode scratch, on top of the map.
            ram_mib: w + 64,
            vram_mib: w,
            // A decode burst is one core-ish for a fraction of the interval
            // between partials. Averaged, not peak — the meter shows a rate.
            cpu_centi_pct: 2_500,
        }
    }

    /// Cost when the model is *not* GPU-resident: CPU-only decode, no VRAM.
    ///
    /// This is what T2 looks like — she keeps listening, on the CPU, slowly —
    /// and it is deliberately expensive in CPU so the governor can see the trade
    /// it is making rather than discovering it in a frame-time graph.
    pub fn cpu_only_cost(self) -> wisp_proto::Cost {
        wisp_proto::Cost {
            ram_mib: self.weights_mib() + 64,
            vram_mib: 0,
            cpu_centi_pct: 20_000,
        }
    }
}

/// May speech recognition run at all at this tier?
///
/// SPEC §0.1 / F28: at **T3 the STT is off entirely**. A headset is on the
/// operator's face and a game owns the GPU; a companion that keeps a microphone
/// open through that has decided, on the operator's behalf, that they are still
/// available. They are not, and they did not ask.
///
/// This is a plain predicate rather than a [`wisp_proto::Governed`] impl because
/// the module that owns the tier wire-up is not this one — see the note at the
/// top of [`crate::mic`].
pub fn permitted_at(tier: wisp_proto::Tier) -> bool {
    matches!(
        tier,
        wisp_proto::Tier::Feral | wisp_proto::Tier::Full | wisp_proto::Tier::Reduced
    )
}

/// [`permitted_at`], as an error the caller can propagate.
pub fn require_tier(tier: wisp_proto::Tier) -> Result<()> {
    if permitted_at(tier) {
        Ok(())
    } else {
        Err(VoiceError::Tier {
            tier,
            what: "speech recognition",
        })
    }
}

// ---------------------------------------------------------------------------
// The fake
// ---------------------------------------------------------------------------

/// An engine with no model, no GPU and no network, whose partials genuinely
/// grow into its final.
///
/// It is not a stub that returns a canned string. It is given a target sentence
/// and a duration, and returns **the prefix of that sentence proportional to how
/// much audio it has been fed** — so a test can assert the property that
/// actually matters for F28 ("she reacts before you finish") rather than
/// asserting that a mock was called.
///
/// It also models the three engine behaviours the pipeline has to survive:
///
/// - silence in, empty transcript out ([`FakeStt::silence_floor`]);
/// - a scripted answer, when a test wants exact text ([`FakeStt::script`]);
/// - an outright engine failure ([`FakeStt::fail`]).
///
/// And it records what it saw — [`FakeStt::calls`], [`FakeStt::windows`],
/// [`FakeStt::finals`], [`FakeStt::resets`] — so a test can prove partials were
/// emitted *during* the utterance on a growing window, rather than all at the
/// end on one big one.
#[derive(Debug, Clone)]
pub struct FakeStt {
    /// What this fake "hears" if it is given enough audio.
    pub target: String,
    /// How much audio corresponds to the whole of `target`.
    pub full_ms: u32,
    /// Windows quieter than this transcribe to nothing.
    pub silence_floor: f32,
    /// When set, every call fails. Exercises the caller's shed path.
    pub fail: bool,
    /// A `final_` call returns the whole `target` regardless of how much audio
    /// arrived. True by default: a real engine handed the complete utterance
    /// decodes the complete utterance, and a final that silently truncated would
    /// hide exactly the bug a test of this pipeline is looking for.
    pub complete_on_final: bool,
    /// Exact answers, consumed front-first. When non-empty this wins over the
    /// proportional-prefix behaviour; `final_` is still forced to match the
    /// call, so a script cannot accidentally emit two finals.
    pub script: VecDeque<Transcript>,

    /// How many times [`Stt::transcribe`] was called.
    pub calls: usize,
    /// The length in samples of every window it was given, in order.
    pub windows: Vec<usize>,
    /// How many of those calls asked for a final.
    pub finals: usize,
    /// How many times [`Stt::reset`] was called.
    pub resets: usize,
}

impl Default for FakeStt {
    fn default() -> Self {
        FakeStt {
            target: "hello there wisp".to_string(),
            full_ms: 2_000,
            silence_floor: 0.005,
            fail: false,
            complete_on_final: true,
            script: VecDeque::new(),
            calls: 0,
            windows: Vec::new(),
            finals: 0,
            resets: 0,
        }
    }
}

impl FakeStt {
    pub fn new() -> Self {
        FakeStt::default()
    }

    /// An engine that will hear `target` once it has been fed `full_ms` of
    /// audio, and a proportional prefix of it before that.
    pub fn saying(target: impl Into<String>, full_ms: u32) -> Self {
        FakeStt {
            target: target.into(),
            full_ms: full_ms.max(1),
            ..FakeStt::default()
        }
    }

    /// An engine that returns exactly these, in order.
    pub fn scripted<I: IntoIterator<Item = Transcript>>(items: I) -> Self {
        FakeStt {
            script: items.into_iter().collect(),
            ..FakeStt::default()
        }
    }

    /// An engine that always fails.
    pub fn broken() -> Self {
        FakeStt {
            fail: true,
            ..FakeStt::default()
        }
    }

    /// The duration of every window it was handed, in ms at 16 kHz. The thing a
    /// "were partials emitted while she was still talking?" assertion reads.
    pub fn window_ms(&self) -> Vec<u32> {
        self.windows
            .iter()
            .map(|n| ((*n as u64 * 1000) / STT_RATE as u64) as u32)
            .collect()
    }

    /// The prefix of `target` that `ms` of audio is worth.
    fn prefix(&self, ms: u32) -> String {
        let words: Vec<&str> = self.target.split_whitespace().collect();
        if words.is_empty() || ms == 0 {
            return String::new();
        }
        let frac = (ms as f64 / self.full_ms as f64).clamp(0.0, 1.0);
        // `ceil`, so the very first partial carries a word rather than an empty
        // string: an empty partial is indistinguishable from "she heard nothing"
        // and the caller is entitled to suppress it.
        let n = ((words.len() as f64) * frac).ceil() as usize;
        words[..n.clamp(1, words.len())].join(" ")
    }
}

impl Stt for FakeStt {
    fn name(&self) -> &str {
        "fake"
    }

    fn transcribe(&mut self, pcm: &[f32], final_: bool) -> Result<Transcript> {
        self.calls += 1;
        self.windows.push(pcm.len());
        if final_ {
            self.finals += 1;
        }
        if self.fail {
            return Err(VoiceError::Stt(format!(
                "FakeStt was told to fail on a {}-sample window",
                pcm.len()
            )));
        }
        if let Some(mut t) = self.script.pop_front() {
            t.final_ = final_;
            return Ok(t);
        }
        // A window that is only room tone transcribes to nothing. Modelled
        // because whisper does the opposite unless you make it behave, and the
        // pipeline above must not publish the difference.
        if rms(pcm) < self.silence_floor {
            return Ok(Transcript::empty(final_));
        }
        let ms = ((pcm.len() as u64 * 1000) / STT_RATE as u64) as u32;
        let text = if final_ && self.complete_on_final {
            self.target.clone()
        } else {
            self.prefix(ms)
        };
        let frac = (ms as f32 / self.full_ms as f32).clamp(0.0, 1.0);
        let confidence = if final_ { 0.95 } else { 0.55 + 0.40 * frac };
        Ok(Transcript::new(text, confidence, final_))
    }

    fn reset(&mut self) {
        self.resets += 1;
    }
}

/// An engine that is always unavailable, for testing the shed path without
/// mutating a [`FakeStt`] mid-test. The analogue of [`crate::tts::DeadTts`].
#[derive(Debug, Default, Clone)]
pub struct DeadStt;

impl Stt for DeadStt {
    fn name(&self) -> &str {
        "dead"
    }
    fn transcribe(&mut self, _pcm: &[f32], _final_: bool) -> Result<Transcript> {
        Err(VoiceError::Stt("no engine".into()))
    }
    fn reset(&mut self) {}
}

/// The engine you get when the build has no STT compiled in.
///
/// It refuses rather than returning empty transcripts, because "she never hears
/// you" and "you are not saying anything" must not look the same to the layer
/// above — one is a build problem the operator can fix and the other is not.
#[derive(Debug, Default, Clone)]
pub struct NoStt;

impl Stt for NoStt {
    fn name(&self) -> &str {
        "none"
    }
    fn transcribe(&mut self, _pcm: &[f32], _final_: bool) -> Result<Transcript> {
        Err(VoiceError::NotCompiled("whisper-stt"))
    }
    fn reset(&mut self) {}
}

// ---------------------------------------------------------------------------
// The real backend
// ---------------------------------------------------------------------------

/// whisper.cpp through `whisper-rs`, on the Vulkan backend.
///
/// **Not compiled on the default feature set, and not compiled by the test run
/// that ships with this file.** Everything below is written against the
/// `whisper-rs 0.16`'s real API, and it **compiles** — the segment accessors
/// were checked against the crate rather than assumed, which is how the two
/// mistakes below were found.
///
/// ## What compiling it actually corrected
///
/// - There is no `full_get_segment_text`. 0.16 hands out a borrowed
///   `WhisperSegment` from `state.get_segment(i)`, and its text comes from
///   `to_str_lossy()` — lossy on purpose, because a decode of a growing window
///   can cut a multi-byte character in half at the edge and dropping the whole
///   segment for that would make partials flicker on any non-ASCII language.
/// - Per-token probabilities *are* reachable (`segment.get_token(i)
///   .token_probability()`), so [`Transcript::confidence`] is the mean over the
///   segment's tokens rather than a flattering constant. `wisp-mind` acts on
///   what she thinks she heard; a made-up confidence would be a lie with
///   consequences.
/// - `FullParams` borrows its language string, so the parameter builder cannot
///   take `&self` — the borrow would still be live across `state.full(..)`,
///   which needs `&mut`.
///
/// Still unrun: no model has been loaded and nothing has been transcribed here,
/// because doing so needs a microphone or a recording, and this crate does not
/// open the operator's microphone. `stt::whisper` is compiled, type-checked and
/// linked against a real whisper.cpp; it is not exercised.
///
/// ## The GPU is the governor's decision, not this module's
///
/// [`whisper::WhisperStt::open_with`] takes `use_gpu`, which must come from
/// [`crate::tier::policy`]. T2 and below forbid the discrete GPU, and whisper's
/// Vulkan backend competing for queues with whatever just started is exactly
/// the dropped frame SPEC §0.1 exists to prevent.
///
/// ## Features
///
/// `whisper-stt` builds whisper.cpp for the CPU — a real shipping
/// configuration, since that is what T2 and T3 use. `whisper-vulkan` adds
/// `whisper-rs/vulkan` on top for T0/T1 and needs the Vulkan headers and a
/// shader compiler at build time.
///
/// ## Where this module lives
///
/// Nested inside [`crate::stt`] rather than given its own file, so it reaches
/// the operator as `wisp_voice::stt::whisper`. It is small and it is meaningless
/// apart from the trait it implements.
#[cfg(feature = "whisper-stt")]
pub mod whisper {
    use super::{SttModel, Stt, Transcript};
    use crate::{Result, VoiceError};
    use std::path::Path;
    use whisper_rs::{
        FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    };

    /// A loaded whisper model plus its decode state.
    ///
    /// `WhisperState` has been lifetime-free since `whisper-rs 0.11` (the
    /// context is refcounted internally), which is what lets these two sit in
    /// one struct. **Hedge:** if a future bump reintroduces `WhisperState<'a>`,
    /// this has to become either self-owning or create-state-per-call — the
    /// latter is a real cost, since state allocation includes the KV cache.
    pub struct WhisperStt {
        _ctx: WhisperContext,
        state: WhisperState,
        model: SttModel,
        name: String,
        threads: i32,
        /// `None` lets whisper auto-detect. F28 pins `en` by default because
        /// auto-detection on a two-second partial is a coin flip, and a partial
        /// decoded as the wrong language is worse than a late one.
        language: Option<String>,
        gpu: bool,
    }

    impl WhisperStt {
        /// Load a `ggml-*.bin` from disk. The path comes from `crate::models`,
        /// which has already verified its pinned sha256 (SPEC §0.2a) — this
        /// function does not re-hash it and must never be handed an unverified
        /// path.
        /// `use_gpu` must come from [`crate::tier::policy`], not from a guess.
        /// T2 and below forbid the discrete GPU outright (SPEC §3.1 via
        /// `tier::VoicePolicy::dgpu`), and whisper's Vulkan backend competing
        /// for queues with whatever just started is exactly the frame the
        /// governor exists to protect. [`WhisperStt::open`] keeps the simple
        /// signature and assumes the GPU is allowed; the governor's path is
        /// [`WhisperStt::open_with`].
        pub fn open(path: &Path, model: SttModel) -> Result<Self> {
            WhisperStt::open_with(path, model, true)
        }

        pub fn open_with(path: &Path, model: SttModel, use_gpu: bool) -> Result<Self> {
            let p = path.to_str().ok_or_else(|| {
                VoiceError::Stt(format!("model path {} is not UTF-8", path.display()))
            })?;

            // **The operator's speech must not reach a terminal or a journal.**
            //
            // `set_print_*(false)` is not enough: those flags govern whisper.cpp's
            // transcript printing, and its *logging* is separate. Left alone, it
            // writes every decoded token — with text — straight to stderr, so
            // running her from a systemd unit would quietly file everything the
            // operator said into the journal. Found by actually running it.
            //
            // This routes both whisper.cpp's and ggml's logs into `tracing`,
            // where the app's own filter decides. Idempotent by contract, so
            // loading a second model is harmless.
            whisper_rs::install_logging_hooks();

            let mut cparams = WhisperContextParameters::default();
            cparams.use_gpu(use_gpu);

            let ctx = WhisperContext::new_with_params(p, cparams)
                .map_err(|e| VoiceError::Stt(format!("loading {}: {e}", path.display())))?;
            let state = ctx
                .create_state()
                .map_err(|e| VoiceError::Stt(format!("whisper state: {e}")))?;

            // Half the cores, floored at one. whisper.cpp scales poorly past
            // that on a desktop part, and SPEC §0.1 says she is a guest on this
            // machine — taking every core to shave 40 ms off a partial is
            // exactly the trade the governor exists to refuse.
            let threads = (std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                / 2)
            .max(1) as i32;

            Ok(WhisperStt {
                _ctx: ctx,
                state,
                model,
                name: format!("whisper:{}", model.id()),
                threads,
                language: Some("en".to_string()),
                gpu: use_gpu,
            })
        }

        /// Let whisper detect the language instead of pinning English.
        pub fn with_language(mut self, lang: Option<&str>) -> Self {
            self.language = lang.map(|s| s.to_string());
            self
        }

        /// Built from plain values rather than from `&self`.
        ///
        /// `FullParams` borrows the language string, so taking `&self` here
        /// would keep an immutable borrow of the whole struct alive across
        /// `self.state.full(..)`, which needs `&mut`. Passing the two fields in
        /// is the fix; threading a lifetime through `WhisperStt` would be the
        /// same fix with more ceremony.
        fn params(threads: i32, language: Option<&str>, final_: bool) -> FullParams<'_, '_> {
            let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            p.set_n_threads(threads);
            p.set_translate(false);
            p.set_language(language);
            // Nothing may reach stdout. She is a desktop app, and whisper.cpp's
            // default is to print the transcript to the terminal — which would
            // also mean the operator's speech landing in a journal.
            p.set_print_special(false);
            p.set_print_progress(false);
            p.set_print_realtime(false);
            p.set_print_timestamps(false);
            // Each call is a fresh decode of the whole window. Carrying decoder
            // context across calls on a *growing* window makes the model
            // condition on its own earlier guess about the same audio, which is
            // how whisper.cpp streaming demos end up looping a phrase forever.
            p.set_no_context(true);
            // One segment: the caller wants a sentence, not a subtitle track.
            p.set_single_segment(true);
            // A final decode may look at the whole window; a partial is allowed
            // to be sloppier, and this is where a future `set_temperature` /
            // beam-size split would go if partial latency ever needs it.
            let _ = final_;
            p
        }
    }

    impl Stt for WhisperStt {
        fn name(&self) -> &str {
            &self.name
        }

        fn transcribe(&mut self, pcm: &[f32], final_: bool) -> Result<Transcript> {
            if pcm.is_empty() {
                return Ok(Transcript::empty(final_));
            }
            let params = Self::params(self.threads, self.language.as_deref(), final_);
            self.state
                .full(params, pcm)
                .map_err(|e| VoiceError::Stt(format!("whisper decode: {e}")))?;

            // whisper-rs 0.16 returns a borrowed `WhisperSegment` rather than
            // an owned `String`, and the text accessor is `to_str_lossy` — a
            // decode can legitimately cut a multi-byte character in half at the
            // window edge, and dropping the whole segment for that would make
            // partials flicker in and out on any non-ASCII language.
            let n = self.state.full_n_segments();
            let mut text = String::new();
            let mut prob_sum = 0.0f64;
            let mut prob_n = 0usize;
            for i in 0..n {
                let Some(seg) = self.state.get_segment(i) else { continue };
                match seg.to_str_lossy() {
                    Ok(s) => text.push_str(&s),
                    Err(e) => {
                        tracing::debug!(segment = i, error = %e, "unreadable segment; skipped");
                        continue;
                    }
                }
                // Confidence, from the thing that actually knows: the mean
                // per-token probability over the segment. This is the standard
                // proxy, and it is a real number rather than a flattering
                // constant — which matters, because `wisp-mind` will act on
                // what she thinks she heard.
                for t in 0..seg.n_tokens() {
                    if let Some(tok) = seg.get_token(t) {
                        let p = tok.token_probability();
                        if p.is_finite() {
                            prob_sum += p as f64;
                            prob_n += 1;
                        }
                    }
                }
            }

            let confidence = if prob_n > 0 {
                (prob_sum / prob_n as f64) as f32
            } else {
                // Nothing decoded. Zero, not a default — "I heard nothing" and
                // "I am fairly sure" must not be the same number.
                0.0
            };
            Ok(Transcript::new(text.trim(), confidence.clamp(0.0, 1.0), final_))
        }

        fn reset(&mut self) {
            // `set_no_context(true)` already means no state crosses a call, so
            // there is nothing to clear. Left explicit rather than absent: if
            // that parameter ever changes, this is where the fix goes.
        }

        fn resident_cost(&self) -> wisp_proto::Cost {
            if self.gpu {
                self.model.resident_cost()
            } else {
                self.model.cpu_only_cost()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sine;

    fn speech(ms: u32) -> Vec<f32> {
        sine(STT_RATE, 220.0, ms, 0.5).samples
    }

    #[test]
    fn a_partial_is_a_prefix_of_the_final_and_grows_with_the_audio() {
        let mut e = FakeStt::saying("the quick brown fox jumps", 1_000);
        let a = e.transcribe(&speech(200), false).unwrap();
        let b = e.transcribe(&speech(600), false).unwrap();
        let c = e.transcribe(&speech(1_000), true).unwrap();

        assert!(!a.text.is_empty(), "the first partial must carry a word");
        assert!(b.text.len() > a.text.len(), "{:?} did not grow into {:?}", a, b);
        assert!(c.text.starts_with(&b.text), "{:?} is not a prefix of {:?}", b, c);
        assert_eq!(c.text, "the quick brown fox jumps");
        assert!(!a.final_ && !b.final_ && c.final_);
    }

    #[test]
    fn confidence_rises_with_the_window_and_never_leaves_zero_to_one() {
        let mut e = FakeStt::saying("one two three four", 1_000);
        let a = e.transcribe(&speech(200), false).unwrap();
        let b = e.transcribe(&speech(900), false).unwrap();
        assert!(b.confidence > a.confidence);
        for t in [&a, &b] {
            assert!((0.0..=1.0).contains(&t.confidence), "{t:?}");
        }
        assert_eq!(Transcript::new("x", f32::NAN, false).confidence, 0.0);
    }

    #[test]
    fn silence_transcribes_to_nothing_rather_than_to_a_hallucination() {
        let mut e = FakeStt::new();
        let t = e.transcribe(&crate::audio::Pcm::silence(STT_RATE, 1_500).samples, true).unwrap();
        assert!(t.is_empty(), "{t:?}");
        assert_eq!(t.confidence, 0.0);
    }

    #[test]
    fn an_empty_window_transcribes_to_nothing() {
        let mut e = FakeStt::new();
        assert!(e.transcribe(&[], false).unwrap().is_empty());
    }

    #[test]
    fn a_broken_engine_reports_rather_than_returning_an_empty_transcript() {
        let mut e = FakeStt::broken();
        assert!(matches!(e.transcribe(&speech(500), false), Err(VoiceError::Stt(_))));
        assert!(matches!(DeadStt.transcribe(&speech(500), true), Err(VoiceError::Stt(_))));
    }

    #[test]
    fn an_uncompiled_engine_says_so_instead_of_pretending_to_hear_silence() {
        assert!(matches!(
            NoStt.transcribe(&speech(500), true),
            Err(VoiceError::NotCompiled("whisper-stt"))
        ));
    }

    #[test]
    fn a_script_is_returned_verbatim_but_cannot_forge_a_final() {
        let mut e = FakeStt::scripted([
            Transcript::new("hello", 0.4, true),
            Transcript::new("hello world", 0.9, false),
        ]);
        let a = e.transcribe(&speech(100), false).unwrap();
        assert_eq!(a.text, "hello");
        assert!(!a.final_, "the script must not be able to declare a final");
        let b = e.transcribe(&speech(200), true).unwrap();
        assert_eq!(b.text, "hello world");
        assert!(b.final_);
    }

    #[test]
    fn it_records_every_window_so_streaming_can_be_proven() {
        let mut e = FakeStt::new();
        e.transcribe(&speech(100), false).unwrap();
        e.transcribe(&speech(400), false).unwrap();
        e.transcribe(&speech(400), true).unwrap();
        e.reset();
        assert_eq!(e.calls, 3);
        assert_eq!(e.finals, 1);
        assert_eq!(e.resets, 1);
        assert_eq!(e.window_ms(), vec![100, 400, 400]);
    }

    #[test]
    fn the_fake_is_deterministic() {
        let a = FakeStt::new().transcribe(&speech(700), false).unwrap();
        let b = FakeStt::new().transcribe(&speech(700), false).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn stt_is_off_at_t3_and_below() {
        use wisp_proto::Tier;
        assert!(permitted_at(Tier::Feral));
        assert!(permitted_at(Tier::Full));
        assert!(permitted_at(Tier::Reduced));
        assert!(!permitted_at(Tier::Lobotomised), "T3 must not listen");
        assert!(!permitted_at(Tier::Dormant));
        assert!(matches!(
            require_tier(Tier::Lobotomised),
            Err(VoiceError::Tier { tier: Tier::Lobotomised, .. })
        ));
        assert!(require_tier(Tier::Full).is_ok());
    }

    #[test]
    fn every_model_size_costs_something_and_bigger_costs_more() {
        let sizes = [
            SttModel::Tiny,
            SttModel::Base,
            SttModel::Small,
            SttModel::LargeV3Turbo,
            SttModel::LargeV3,
        ];
        for w in sizes.windows(2) {
            assert!(
                w[0].resident_cost().vram_mib < w[1].resident_cost().vram_mib,
                "{:?} should cost less than {:?}",
                w[0],
                w[1]
            );
        }
        // The CPU-only path trades VRAM for cores, and must report both.
        let c = SttModel::Small.cpu_only_cost();
        assert_eq!(c.vram_mib, 0);
        assert!(c.cpu_centi_pct > SttModel::Small.resident_cost().cpu_centi_pct);
        assert!(c.ram_mib > 0);
    }

    #[test]
    fn the_default_sample_rate_is_what_whisper_demands() {
        assert_eq!(FakeStt::new().sample_rate(), 16_000);
        assert_eq!(FakeStt::new().sample_rate(), STT_RATE);
    }
}
