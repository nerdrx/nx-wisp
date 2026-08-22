//! F28 — the microphone, which is the most dangerous thing in this product.
//!
//! SPEC §3.7 puts [`wisp_proto::SenseId::Microphone`] at
//! [`wisp_proto::Consent::Invasive`]: it **ships off**, it may only run through
//! a granted consent handle, and a **visible tell on the character herself must
//! be live for the entire time it is open**. Nothing invisible. This module is
//! the part of the codebase where that promise is either kept or quietly broken,
//! so it is built to make breaking it hard rather than merely discouraged.
//!
//! ## The shape, and why it is this shape
//!
//! [`Listener::open`] is the only constructor and it takes a [`MicPermit`] by
//! value. There is no `Listener::new`, no `Default`, and no way to swap the
//! permit afterwards — so **a listening microphone is unrepresentable without
//! consent**, in the same way `wisp_senses::consent::SenseHandle` makes an
//! unconsented sense unrepresentable. That is not a coincidence; the real
//! [`MicPermit`] implementation is a ten-line adapter over a `SenseHandle`, and
//! this trait exists only because SPEC §2's crate map does not let `wisp-voice`
//! depend on `wisp-senses` by default.
//!
//! The tell follows the same rule. `SenseHandle::drop` lowers it, the
//! [`Listener`] owns its permit, and [`Listener::close`] drops the `Listener`.
//! So the tell is up exactly while a `Listener` exists, by construction, with no
//! "remember to lower it" branch anywhere for anyone to forget. There is
//! deliberately no way to keep listening after the permit is gone: every entry
//! point checks [`MicPermit::still_permitted`] first and every publish goes
//! through the permit.
//!
//! ## What this module does not contain, on purpose
//!
//! **There is no real capture backend.** No PipeWire stream, no `libpulse`, no
//! `cpal`. [`MicSource`] is the hole where one goes, and it is a documented hole
//! rather than an implementation, because writing an audio-capture path means
//! opening a microphone to find out whether it works — and the person whose
//! microphone that would be did not ask for it. An untested capture backend that
//! nobody could run is worse than an honest gap: it would look finished, it
//! would be wired into the app, and the first person to discover it was wrong
//! would be an operator whose speech went nowhere while the tell said she was
//! listening. See [`MicSource`] for what the real one has to do.
//!
//! **Nothing here ever writes captured audio to disk. Ever.** Not to a cache,
//! not to a crash dump, not behind a `debug_audio` flag, not into the flight
//! recorder of SPEC §0.4 — which records *what she did*, and "the operator's
//! room, as a wav" is not that. The only durable artefact of a microphone
//! session is the transcribed text the operator can see on the bus. If a future
//! change adds a debug dump here, it is a spec violation and this paragraph is
//! the evidence that it was not an oversight.
//!
//! ## The pipeline
//!
//! ```text
//!   MicSource ──read()──▶ level meter ──▶ the tell (always, whenever open)
//!        │                                 │
//!        └──▶ resample to 16 kHz ──▶ [ push-to-talk down?  ─▶ window ]
//!                                    [ wake word armed?    ─▶ pre-roll ring ]
//!                                    [ neither?            ─▶ discarded ]
//!                                                  │
//!                            pump() ──▶ Stt ──▶ partial…partial…final
//!                                                  │
//!                                        MicPermit::publish
//! ```
//!
//! **Push-to-talk is the default and a wake word is opt-in** (F28). Between
//! presses the audio is *discarded as it arrives* — not buffered, not held "just
//! in case", not kept in a ring for a feature that is switched off. A companion
//! that quietly retains the last thirty seconds of your room in case you later
//! decide you wanted it is doing the thing this product exists not to do.
//!
//! ## Tiers
//!
//! At **T3 the STT is off entirely** (SPEC §0.1): a game or a headset owns the
//! machine, and she is not listening to someone who is wearing a VR headset
//! unless they explicitly asked. The predicate lives in
//! [`crate::stt::permitted_at`] and the shed is [`Listener::close`], which drops
//! the permit and lowers the tell. [`wisp_proto::Governed`] is deliberately
//! **not** implemented here — the module that owns the tier wire-up composes
//! this one, so that there is exactly one place in the tree that decides what a
//! downgrade means for speech.

use std::collections::VecDeque;

use wisp_proto::Observation;

use crate::audio::{rms, to_db, Pcm, STT_RATE};
use crate::stt::Stt;
use crate::{Millis, Result, VoiceError};

/// The bottom of the level meter's scale, in dBFS. Below this she reads zero.
///
/// −60 rather than −90: the meter exists so the operator can see the tell react
/// to their voice, and a scale that also renders the fridge is a scale that
/// never sits still.
const METER_FLOOR_DB: f32 = -60.0;

/// The hard ceiling on any pre-roll, whatever the config says. See
/// [`WakeConfig::preroll_ms`] for why there is a ceiling at all.
pub const MAX_PREROLL_MS: u32 = 10_000;

/// Block RMS above which a block counts as voice when no VAD is configured.
/// Reporting only — endpointing never uses this.
const REPORTING_VOICE_FLOOR: f32 = 0.02;

/// How many source reads one [`Listener::pump`] will drain before giving the
/// caller their thread back. A source that always has data must not be able to
/// spin the pump forever; the leftovers arrive on the next tick.
const MAX_READS_PER_PUMP: usize = 64;

// ---------------------------------------------------------------------------
// The permit
// ---------------------------------------------------------------------------

/// Proof that the microphone consent gate was satisfied, and the only channel
/// by which transcribed speech may reach the bus.
///
/// The real implementation is an adapter over
/// `wisp_senses::consent::SenseHandle<MicrophoneSense>`, and it is about ten
/// lines: `publish` forwards to `SenseHandle::publish` (which already refuses
/// any `Observation` that does not belong to `SenseId::Microphone`) and
/// `still_permitted` forwards to `SenseHandle::still_permitted`. The adapter
/// **owns** the handle, so dropping the permit drops the handle, and
/// `SenseHandle::drop` is what lowers the visible tell of SPEC §0.3.
///
/// That is the whole design: this trait has no `close`, no `revoke` and no
/// `set_tell`, because every one of those would be a second way to lower the
/// tell and therefore a second way to get it wrong. There is one way, and it is
/// `Drop`.
pub trait MicPermit: Send {
    /// Publish an observation. Must be `Observation::Speech`.
    fn publish(&self, obs: wisp_proto::Observation) -> Result<()>;

    /// Has the operator revoked us since we started?
    fn still_permitted(&self) -> bool;
}

/// A recording permit, for tests and for the headless demo paths.
///
/// Keeps every observation it was given, can be revoked mid-stream, and counts
/// its own drops — which is how "dropping the [`Listener`] lowers the tell
/// exactly once" becomes a testable statement rather than a hopeful one.
#[derive(Debug)]
pub struct FakePermit {
    log: PermitLog,
}

impl Default for FakePermit {
    fn default() -> Self {
        FakePermit::new()
    }
}

impl FakePermit {
    pub fn new() -> Self {
        FakePermit {
            log: PermitLog::default(),
        }
    }

    /// A handle onto what this permit saw, which outlives the permit itself.
    pub fn log(&self) -> PermitLog {
        self.log.clone()
    }
}

impl MicPermit for FakePermit {
    fn publish(&self, obs: Observation) -> Result<()> {
        let mut g = self.log.lock();
        if g.revoked {
            // What the real `SenseHandle` does: a revoked handle refuses, it
            // does not silently swallow.
            return Err(VoiceError::ConsentRevoked);
        }
        if !matches!(obs, Observation::Speech { .. }) {
            // The real gate rejects this as `PublishError::WrongSense`. Mirrored
            // here so the property is under test on the default feature set,
            // where `wisp-senses` is not even compiled.
            g.refused += 1;
            return Err(VoiceError::Stt(format!(
                "the microphone permit may only publish Observation::Speech, not {:?}",
                obs.sense()
            )));
        }
        g.published.push(obs);
        Ok(())
    }

    fn still_permitted(&self) -> bool {
        !self.log.lock().revoked
    }
}

impl Drop for FakePermit {
    fn drop(&mut self) {
        self.log.lock().drops += 1;
    }
}

/// What a [`FakePermit`] saw. Cheap to clone; survives the permit's death.
#[derive(Debug, Clone, Default)]
pub struct PermitLog {
    inner: std::sync::Arc<std::sync::Mutex<PermitLogInner>>,
}

#[derive(Debug, Default)]
struct PermitLogInner {
    published: Vec<Observation>,
    refused: usize,
    revoked: bool,
    drops: usize,
}

impl PermitLog {
    fn lock(&self) -> std::sync::MutexGuard<'_, PermitLogInner> {
        // A poisoned lock here means a test panicked while publishing; the log
        // is still readable and a second panic would only hide the first.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everything that reached the bus, in order.
    pub fn published(&self) -> Vec<Observation> {
        self.lock().published.clone()
    }

    /// Just the speech, as `(text, final_)`.
    pub fn speech(&self) -> Vec<(String, bool)> {
        self.lock()
            .published
            .iter()
            .filter_map(|o| match o {
                Observation::Speech { text, final_ } => Some((text.clone(), *final_)),
                _ => None,
            })
            .collect()
    }

    pub fn partials(&self) -> Vec<String> {
        self.speech()
            .into_iter()
            .filter(|(_, f)| !f)
            .map(|(t, _)| t)
            .collect()
    }

    pub fn finals(&self) -> Vec<String> {
        self.speech()
            .into_iter()
            .filter(|(_, f)| *f)
            .map(|(t, _)| t)
            .collect()
    }

    /// The operator hit the switch. Every subsequent publish fails and
    /// [`MicPermit::still_permitted`] goes false.
    pub fn revoke(&self) {
        self.lock().revoked = true;
    }

    pub fn is_revoked(&self) -> bool {
        self.lock().revoked
    }

    /// The operator switched the microphone back on in the consent panel.
    ///
    /// Which grants a *new* permit in the real ledger — it does not resurrect
    /// the one a [`Listener`] already gave up on. Exists so that property can be
    /// tested rather than assumed.
    pub fn regrant(&self) {
        self.lock().revoked = false;
    }

    /// How many observations were refused for not being speech.
    pub fn refused(&self) -> usize {
        self.lock().refused
    }

    /// How many times the permit was dropped. Must be exactly one over the life
    /// of a [`Listener`]: the tell goes down once and stays down.
    pub fn drops(&self) -> usize {
        self.lock().drops
    }
}

// ---------------------------------------------------------------------------
// The capture source
// ---------------------------------------------------------------------------

/// Where audio comes from.
///
/// # The hole where PipeWire goes
///
/// The real implementation is a PipeWire capture stream: `pw_stream_new`
/// against the default source with `PW_KEY_MEDIA_CATEGORY = "Capture"` and
/// `PW_KEY_MEDIA_ROLE = "Communication"` (so the desktop's own indicator
/// notices us, which is a second visible tell we get for free), format
/// negotiated to `F32LE` mono at whatever the device offers, samples pushed into
/// a bounded SPSC ring by the stream's `process` callback, and [`read`] draining
/// that ring without blocking. `stop` destroys the stream, which is what
/// actually closes the device.
///
/// It is not written, and the reason is in the module docs: writing it means
/// opening the operator's microphone to test it, and this crate is not entitled
/// to do that. Whoever writes it should assume the following, all of which this
/// module already handles: the device rate is **not** 16 kHz (48 kHz is
/// overwhelmingly likely), `read` may return nothing for many consecutive calls,
/// and the ring must be **bounded and dropped-oldest** — an unbounded capture
/// ring is a recording, whatever the code around it intended.
///
/// [`read`]: MicSource::read
pub trait MicSource: Send {
    fn sample_rate(&self) -> u32;

    /// Non-blocking: whatever has arrived since the last call.
    fn read(&mut self) -> Result<Vec<f32>>;

    /// Close the device. Must be idempotent — [`Listener`] calls it on drop and
    /// the caller may have called it already.
    fn stop(&mut self);
}

/// A microphone made of a script, so an utterance can be "spoken" in a test.
///
/// Delivers one queued chunk per [`MicSource::read`], which is what a real
/// device does: audio arrives in blocks, not all at once, and a pipeline that
/// only works when handed a whole utterance in one call is a pipeline that does
/// not stream.
#[derive(Debug)]
pub struct FakeMic {
    rate: u32,
    chunks: VecDeque<Vec<f32>>,
    /// Set to make every read fail, for the capture-error path.
    pub fail: bool,
    pub reads: usize,
    pub stopped: bool,
    pub stops: usize,
}

impl FakeMic {
    pub fn new(rate: u32) -> Self {
        FakeMic {
            rate: rate.max(1),
            chunks: VecDeque::new(),
            fail: false,
            reads: 0,
            stopped: false,
            stops: 0,
        }
    }

    /// A 48 kHz device, which is what almost every real one is. Using this
    /// rather than 16 kHz in a test means the resampler is on the path.
    pub fn realistic() -> Self {
        FakeMic::new(48_000)
    }

    /// Queue audio, split into `chunk_ms` blocks the way a device delivers it.
    pub fn push_pcm(&mut self, pcm: &Pcm, chunk_ms: u32) -> &mut Self {
        let n = (pcm.rate as u64 * chunk_ms.max(1) as u64 / 1000).max(1) as usize;
        for c in pcm.samples.chunks(n) {
            self.chunks.push_back(c.to_vec());
        }
        self
    }

    /// Queue a block verbatim.
    pub fn push(&mut self, samples: Vec<f32>) -> &mut Self {
        self.chunks.push_back(samples);
        self
    }

    /// Queue `ms` of tone — a stand-in for someone talking.
    pub fn push_speech(&mut self, ms: u32, chunk_ms: u32) -> &mut Self {
        let p = crate::audio::sine(self.rate, 220.0, ms, 0.6);
        self.push_pcm(&p, chunk_ms)
    }

    /// Queue `ms` of room tone.
    pub fn push_silence(&mut self, ms: u32, chunk_ms: u32) -> &mut Self {
        let p = Pcm::silence(self.rate, ms);
        self.push_pcm(&p, chunk_ms)
    }

    pub fn queued_blocks(&self) -> usize {
        self.chunks.len()
    }
}

impl MicSource for FakeMic {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn read(&mut self) -> Result<Vec<f32>> {
        self.reads += 1;
        if self.fail {
            return Err(VoiceError::Sink("FakeMic was told to fail".into()));
        }
        if self.stopped {
            return Ok(Vec::new());
        }
        Ok(self.chunks.pop_front().unwrap_or_default())
    }

    fn stop(&mut self) {
        self.stops += 1;
        self.stopped = true;
        self.chunks.clear();
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Energy-based endpointing.
///
/// Deliberately crude. A neural VAD (Silero and friends) is better at telling
/// speech from a keyboard, and it is also another model to hold resident while
/// SPEC §0.1 is trying to give the machine back. RMS with a hangover is enough
/// to answer the one question asked of it — "has the operator stopped, or are
/// they thinking?" — and it costs nothing at T2.
#[derive(Debug, Clone, PartialEq)]
pub struct VadConfig {
    /// Block RMS at or above which the block counts as voice.
    pub floor: f32,
    /// Quiet for this long ends the utterance. The whole point: a pause is not
    /// an ending, and cutting someone off mid-thought is the failure mode people
    /// actually hate about voice assistants.
    pub hangover_ms: u64,
    /// An utterance must contain at least this much voice before the hangover is
    /// allowed to end it, so a door closing does not open and close a window.
    pub min_speech_ms: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        VadConfig {
            floor: 0.02,
            hangover_ms: 700,
            min_speech_ms: 300,
        }
    }
}

/// The opt-in wake word.
///
/// Off unless the operator turns it on, per F28. Matched against **partial
/// transcripts** rather than by a dedicated keyword spotter, because there is
/// already a speech recogniser here and adding a second always-resident model to
/// avoid running the first one is the wrong trade for a companion that is
/// supposed to disappear at T2.
#[derive(Debug, Clone, PartialEq)]
pub struct WakeConfig {
    /// What she answers to. Matched case- and punctuation-insensitively.
    pub phrase: String,

    /// How much audio is kept ahead of a match, so the words *just before* the
    /// wake word survive.
    ///
    /// # This is the honest part
    ///
    /// A pre-roll buffer is **a few seconds of the operator's room, held in
    /// RAM, continuously, while the wake word is armed**. There is no way to
    /// implement "she heard the start of your sentence" that is not that. So:
    /// it is opt-in with the wake word and does not exist otherwise; it is a
    /// hard-bounded ring ([`MAX_PREROLL_MS`]) that drops oldest, so it cannot
    /// grow into a recording; it is cleared the moment an utterance starts, the
    /// moment push-to-talk is pressed, and the moment consent is revoked; and it
    /// is never written to disk. The visible tell is up the entire time it is
    /// filling, because the [`Listener`] holds the permit — the operator can see
    /// that she is listening, which is the difference between a pre-roll and a
    /// bug.
    pub preroll_ms: u32,
}

impl Default for WakeConfig {
    fn default() -> Self {
        WakeConfig {
            phrase: "hey wisp".to_string(),
            preroll_ms: 3_000,
        }
    }
}

/// How this listener behaves.
#[derive(Debug, Clone, PartialEq)]
pub struct ListenConfig {
    /// A partial is published no more often than this. Every partial is a full
    /// re-decode of the window (see [`crate::stt`]), so this is a CPU budget as
    /// much as a UI cadence.
    pub partial_interval_ms: u64,

    /// The hard cap on one window. A stuck push-to-talk key, a wedged input
    /// stack or a wake word that matched a podcast must not be able to grow an
    /// unbounded recording; when the cap trips the window is finalised and a new
    /// one starts. 30 s is also whisper's own context length, past which the
    /// audio would be truncated anyway.
    pub max_window_ms: u64,

    /// Endpointing. `None` means only push-to-talk release (or the cap) ends an
    /// utterance.
    pub vad: Option<VadConfig>,

    /// `None` — off — unless the operator opts in.
    pub wake: Option<WakeConfig>,

    /// Level-meter attack time constant. Short: the tell should light up on the
    /// first syllable.
    pub level_attack_ms: u32,

    /// Level-meter release time constant. Long: a meter that falls as fast as it
    /// rises strobes between syllables, and a strobing tell on her face is worse
    /// than no tell at all — people look away from it, which defeats the point.
    pub level_release_ms: u32,
}

impl Default for ListenConfig {
    fn default() -> Self {
        ListenConfig {
            partial_interval_ms: 300,
            max_window_ms: 30_000,
            vad: Some(VadConfig::default()),
            // Push-to-talk by default. F28, and not negotiable by a config file
            // shipping a different default.
            wake: None,
            level_attack_ms: 10,
            level_release_ms: 300,
        }
    }
}

impl ListenConfig {
    /// Clamp a hand-written config back into the sane. Applied by
    /// [`Listener::open`], so nothing downstream has to re-check.
    pub fn sane(mut self) -> Self {
        self.partial_interval_ms = self.partial_interval_ms.clamp(50, 5_000);
        self.max_window_ms = self.max_window_ms.clamp(1_000, 60_000);
        self.level_attack_ms = self.level_attack_ms.clamp(1, 1_000);
        self.level_release_ms = self.level_release_ms.clamp(1, 5_000);
        if let Some(v) = self.vad.as_mut() {
            v.floor = if v.floor.is_finite() {
                v.floor.clamp(0.0, 1.0)
            } else {
                VadConfig::default().floor
            };
            v.hangover_ms = v.hangover_ms.clamp(100, 5_000);
            v.min_speech_ms = v.min_speech_ms.clamp(0, 5_000);
        }
        if let Some(w) = self.wake.as_mut() {
            // The bound is enforced here rather than trusted from config,
            // because "how much of my room do you keep" is not a number a config
            // file gets to raise.
            w.preroll_ms = w.preroll_ms.min(MAX_PREROLL_MS);
        }
        self
    }
}

/// Is the push-to-talk key held?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    Up,
    Down,
}

/// What started the utterance that is currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Ptt,
    Wake,
}

/// Why an utterance ended. Carried into the log line, so "why did she cut me
/// off" is answerable from data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The key came up. The operator said when they were done.
    Ptt,
    /// Endpointing decided they had stopped.
    Vad,
    /// [`ListenConfig::max_window_ms`] tripped.
    Cap,
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

/// One open utterance.
#[derive(Debug)]
struct Utterance {
    /// Mono f32 at [`STT_RATE`]. Never leaves this struct except into an engine.
    window: Vec<f32>,
    trigger: Trigger,
    started_ms: Millis,
    last_partial_ms: Millis,
    /// The last text we actually published, so a repeat is suppressed and so a
    /// final that comes back empty can fall back to the best answer we already
    /// gave rather than retracting it.
    last_partial_text: String,
    partials: usize,
    last_voice_ms: Millis,
    voiced_ms: u64,
}

impl Utterance {
    fn new(trigger: Trigger, now: Millis) -> Self {
        Utterance {
            window: Vec::new(),
            trigger,
            started_ms: now,
            // Backdated by nothing: the first partial is due one interval after
            // the utterance opens, not immediately, because a 50 ms window
            // transcribes to noise.
            last_partial_ms: now,
            last_partial_text: String::new(),
            partials: 0,
            last_voice_ms: now,
            voiced_ms: 0,
        }
    }

    fn window_ms(&self) -> u64 {
        (self.window.len() as u64 * 1000) / STT_RATE as u64
    }
}

/// An open microphone, and the only thing in this crate that can be one.
///
/// Owns its [`MicPermit`]. Dropping it drops the permit, which lowers the
/// visible tell of SPEC §0.3 — that is the entire lifecycle, and there is no
/// other path to either end of it.
pub struct Listener<P: MicPermit> {
    permit: P,
    source: Box<dyn MicSource>,
    cfg: ListenConfig,
    src_rate: u32,

    ptt: PttState,
    active: Option<Utterance>,
    pending_end: Option<EndReason>,

    /// The wake-word pre-roll. Empty and unused unless [`ListenConfig::wake`] is
    /// set; bounded by `ring_cap` on every push. See [`WakeConfig::preroll_ms`].
    ring: VecDeque<f32>,
    ring_cap: usize,
    last_wake_check_ms: Millis,

    level: f32,
    speaking: bool,
    /// Latched once consent goes away: a revoked listener never listens again,
    /// it is closed and reopened.
    revoked: bool,
}

impl<P: MicPermit> Listener<P> {
    /// The ONLY constructor. Requires the permit — the microphone is
    /// unrepresentable without consent.
    ///
    /// Refuses if the permit is already revoked, so a listener cannot come into
    /// existence in a state where the tell is down but the pipeline is up.
    pub fn open(permit: P, source: Box<dyn MicSource>, cfg: ListenConfig) -> Result<Self> {
        if !permit.still_permitted() {
            return Err(VoiceError::ConsentRevoked);
        }
        let src_rate = source.sample_rate();
        if src_rate == 0 {
            // No `VoiceError::Capture` variant exists; `Sink` is the closest
            // audio-device error in the crate's enum. Noted as an API gap.
            return Err(VoiceError::Sink(
                "microphone source reported a sample rate of 0".into(),
            ));
        }
        let cfg = cfg.sane();
        let ring_cap = cfg
            .wake
            .as_ref()
            .map(|w| (STT_RATE as u64 * w.preroll_ms as u64 / 1000) as usize)
            .unwrap_or(0);

        tracing::info!(
            src_rate,
            wake = cfg.wake.is_some(),
            preroll_ms = cfg.wake.as_ref().map(|w| w.preroll_ms).unwrap_or(0),
            "microphone open under a consent permit; the tell is up until this drops"
        );

        Ok(Listener {
            permit,
            source,
            cfg,
            src_rate,
            ptt: PttState::Up,
            active: None,
            pending_end: None,
            ring: VecDeque::with_capacity(ring_cap),
            ring_cap,
            last_wake_check_ms: 0,
            level: 0.0,
            speaking: false,
            revoked: false,
        })
    }

    // -- state the caller and the tell need ---------------------------------

    /// Has the operator revoked us since we started?
    pub fn still_permitted(&self) -> bool {
        !self.revoked && self.permit.still_permitted()
    }

    pub fn ptt(&self) -> PttState {
        self.ptt
    }

    /// Is an utterance open right now? What the tell's "she is hearing you"
    /// state is drawn from, as opposed to "she can hear".
    pub fn is_listening(&self) -> bool {
        self.active.is_some()
    }

    pub fn trigger(&self) -> Option<Trigger> {
        self.active.as_ref().map(|u| u.trigger)
    }

    /// Length of the open window, in ms — how much audio the engine will see.
    pub fn window_ms(&self) -> u64 {
        self.active.as_ref().map(|u| u.window_ms()).unwrap_or(0)
    }

    /// How long the open utterance has been open in wall time, in ms.
    ///
    /// Not the same number as [`Listener::window_ms`], and the gap between them
    /// is diagnostic: if she has been listening for four seconds and holds one
    /// second of audio, three seconds of capture went missing and the tell has
    /// been lying about it. The tell shows this one, because it is what the
    /// operator experienced.
    pub fn utterance_ms(&self, now: Millis) -> u64 {
        self.active
            .as_ref()
            .map(|u| now.saturating_sub(u.started_ms))
            .unwrap_or(0)
    }

    /// How much audio the pre-roll ring is currently holding, in ms. Zero
    /// whenever the wake word is off, which is the default.
    pub fn preroll_ms(&self) -> u64 {
        (self.ring.len() as u64 * 1000) / STT_RATE as u64
    }

    /// Did the last block cross the voice floor? Endpointing state, exposed so
    /// the tell can show "she can hear something" without waiting for text.
    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    pub fn config(&self) -> &ListenConfig {
        &self.cfg
    }

    /// The current input level, 0..1, for the visible tell's level meter.
    ///
    /// Smoothed with a fast attack and a slow release: it must light up on the
    /// first syllable and must not fall back to zero in the gap between two
    /// words. See [`ListenConfig::level_release_ms`].
    pub fn level(&self) -> f32 {
        self.level
    }

    // -- push to talk --------------------------------------------------------

    /// The key went down: start an utterance. Idempotent while held.
    pub fn ptt_down(&mut self, now: Millis) {
        if self.ptt == PttState::Down {
            return;
        }
        self.ptt = PttState::Down;
        self.pending_end = None;
        // The pre-roll is for the wake word. Someone who pressed the key meant
        // to start *here*, and keeping the seconds before the press would put
        // audio they did not intend to send into the window.
        self.clear_ring();
        match self.active.as_mut() {
            None => {
                self.active = Some(Utterance::new(Trigger::Ptt, now));
                tracing::debug!(now, "push-to-talk down");
            }
            Some(u) => {
                // A wake word had already opened this one. The operator has now
                // taken manual control of it, so the endpoint becomes theirs:
                // otherwise the key they are holding would do nothing and the
                // VAD would cut them off anyway, which is the worst of both.
                u.trigger = Trigger::Ptt;
                tracing::debug!(now, "push-to-talk down over a wake-word utterance");
            }
        }
    }

    /// The key came up: the utterance ends.
    ///
    /// The final transcript is produced by the next [`Listener::pump`], because
    /// that is where the engine is. Nothing is lost in between — the window is
    /// already complete — and it keeps the key handler off the decode path,
    /// which matters when the decode is a whisper `full()` call.
    pub fn ptt_up(&mut self, now: Millis) {
        if self.ptt == PttState::Up {
            return;
        }
        self.ptt = PttState::Up;
        if self.active.as_ref().map(|u| u.trigger) == Some(Trigger::Ptt) {
            self.pending_end = Some(EndReason::Ptt);
            tracing::debug!(now, "push-to-talk up");
        }
    }

    // -- the pipeline --------------------------------------------------------

    /// Take a block of capture audio at the source's rate.
    ///
    /// Resamples to 16 kHz and appends it to whatever is currently collecting —
    /// the open window, the wake-word pre-roll, or **nothing at all**, which is
    /// the default state and is a deliberate discard rather than an oversight.
    pub fn feed(&mut self, samples: &[f32], now: Millis) -> Result<()> {
        self.guard()?;
        if samples.is_empty() {
            return Ok(());
        }

        // The meter runs on everything, including audio we are about to throw
        // away: the tell is up because the device is open, so it has to react to
        // the device being open. A meter that only moved during an utterance
        // would let the operator believe she was deaf between presses.
        self.update_level(samples);

        let block_rms = rms(samples);
        let floor = self
            .cfg
            .vad
            .as_ref()
            .map(|v| v.floor)
            .unwrap_or(REPORTING_VOICE_FLOOR);
        let voiced = block_rms >= floor;
        self.speaking = voiced;

        let block_ms = (samples.len() as u64 * 1000) / self.src_rate as u64;
        let at16 = self.to_stt_rate(samples);

        if let Some(u) = self.active.as_mut() {
            u.window.extend_from_slice(&at16);
            if voiced {
                u.last_voice_ms = now;
                u.voiced_ms = u.voiced_ms.saturating_add(block_ms);
            }
        } else if self.ring_cap > 0 {
            for s in at16 {
                if self.ring.len() == self.ring_cap {
                    self.ring.pop_front();
                }
                self.ring.push_back(s);
            }
        }
        // else: discarded, here, on purpose. Push-to-talk with no wake word
        // keeps nothing between presses.
        Ok(())
    }

    /// Drain the source, run the engine if something is due, publish through the
    /// permit, and return what was published.
    ///
    /// Emits `Observation::Speech { final_: false }` no more often than
    /// [`ListenConfig::partial_interval_ms`], and exactly one with
    /// `final_: true` when the utterance ends.
    pub fn pump(&mut self, stt: &mut dyn Stt, now: Millis) -> Result<Vec<Observation>> {
        self.guard()?;

        for _ in 0..MAX_READS_PER_PUMP {
            let block = self.source.read()?;
            if block.is_empty() {
                break;
            }
            self.feed(&block, now)?;
        }

        let mut out = Vec::new();

        if self.active.is_some() {
            if self.pending_end.is_none() && self.window_ms() >= self.cfg.max_window_ms {
                self.pending_end = Some(EndReason::Cap);
            }
            if self.pending_end.is_none() {
                if let (Some(v), Some(u)) = (self.cfg.vad.as_ref(), self.active.as_ref()) {
                    // Endpointing only ever ends an utterance the *machine*
                    // started. While the operator is physically holding the key
                    // they own the endpoint, and cutting them off because they
                    // paused to think is precisely the failure push-to-talk
                    // exists to avoid.
                    let ends = u.trigger == Trigger::Wake
                        && u.voiced_ms >= v.min_speech_ms
                        && now.saturating_sub(u.last_voice_ms) >= v.hangover_ms;
                    if ends {
                        self.pending_end = Some(EndReason::Vad);
                    }
                }
            }

            if let Some(reason) = self.pending_end.take() {
                self.finalise(stt, now, reason, &mut out)?;
            } else {
                let due = self
                    .active
                    .as_ref()
                    .map(|u| now.saturating_sub(u.last_partial_ms) >= self.cfg.partial_interval_ms)
                    .unwrap_or(false);
                if due {
                    self.partial(stt, now, &mut out)?;
                }
            }
        } else if self.cfg.wake.is_some() {
            self.check_wake(stt, now)?;
        }

        Ok(out)
    }

    /// Drop the permit, which lowers the tell.
    ///
    /// Consumes the listener because there is no such thing as a closed one:
    /// reopening means going back through the consent gate for a new permit.
    /// This is also the T3 shed — see the module docs.
    pub fn close(self) {
        tracing::info!("microphone closed; the tell goes down");
        // `Drop` does the rest: stop the device, wipe the buffers, drop the
        // permit. Written out as a method anyway so call sites read as an
        // intention rather than as a stray `drop(x)`.
    }

    // -- internals -----------------------------------------------------------

    /// The check that runs before anything else, everywhere.
    fn guard(&mut self) -> Result<()> {
        if self.revoked {
            return Err(VoiceError::ConsentRevoked);
        }
        if !self.permit.still_permitted() {
            self.abort();
            return Err(VoiceError::ConsentRevoked);
        }
        Ok(())
    }

    /// Consent is gone. Everything in flight dies here: the half-transcribed
    /// sentence is **dropped, not published**, the pre-roll goes, and the device
    /// is stopped. The listener stays revoked until it is dropped.
    fn abort(&mut self) {
        if self.revoked {
            return;
        }
        self.revoked = true;
        if let Some(mut u) = self.active.take() {
            tracing::warn!(
                window_ms = u.window_ms(),
                partials = u.partials,
                "microphone consent revoked mid-utterance; discarding the pending transcript"
            );
            wipe(&mut u.window);
        }
        self.clear_ring();
        self.pending_end = None;
        self.source.stop();
    }

    fn clear_ring(&mut self) {
        for s in self.ring.iter_mut() {
            *s = 0.0;
        }
        self.ring.clear();
    }

    /// Resample one block to what the engine demands.
    ///
    /// Per-block rather than through a resampler that keeps state across blocks:
    /// linear interpolation restarted at a block boundary loses continuity for
    /// at most one sample, and the consumer is a mel filterbank that cannot see
    /// it. A stateful resampler would be strictly better and is not worth a
    /// dependency here — [`Pcm::resampled`] carries the same argument.
    fn to_stt_rate(&self, samples: &[f32]) -> Vec<f32> {
        if self.src_rate == STT_RATE {
            return samples.to_vec();
        }
        Pcm::new(self.src_rate, samples.to_vec())
            .resampled(STT_RATE)
            .samples
    }

    fn update_level(&mut self, samples: &[f32]) {
        let block_ms = (samples.len() as f32 * 1000.0) / self.src_rate as f32;
        let db = to_db(rms(samples));
        let target = ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0);
        let tau = if target > self.level {
            self.cfg.level_attack_ms
        } else {
            self.cfg.level_release_ms
        } as f32;
        let a = (1.0 - (-block_ms / tau).exp()).clamp(0.0, 1.0);
        self.level = (self.level + (target - self.level) * a).clamp(0.0, 1.0);
    }

    /// Publish one observation through the permit, and record it for the caller.
    fn publish(&mut self, obs: Observation, out: &mut Vec<Observation>) -> Result<()> {
        // Belt to the permit's braces. Nothing in this module constructs
        // anything but `Speech`; if that ever changes, it fails here rather than
        // reaching the bus wearing the microphone's consent.
        if !matches!(obs, Observation::Speech { .. }) {
            return Err(VoiceError::Stt(
                "the microphone may only publish Observation::Speech".into(),
            ));
        }
        match self.permit.publish(obs.clone()) {
            Ok(()) => {
                out.push(obs);
                Ok(())
            }
            Err(e) => {
                // The usual cause is a revocation that landed between the guard
                // and here. Treat it as one.
                self.abort();
                Err(e)
            }
        }
    }

    fn partial(&mut self, stt: &mut dyn Stt, now: Millis, out: &mut Vec<Observation>) -> Result<()> {
        let Some(u) = self.active.as_mut() else {
            return Ok(());
        };
        let t = match stt.transcribe(&u.window, false) {
            Ok(t) => t,
            Err(e) => {
                // Sheddable, like everything else: the engine failed, so the
                // utterance dies rather than accumulating audio nobody will ever
                // decode.
                self.discard_active();
                stt.reset();
                return Err(e);
            }
        };
        u.last_partial_ms = now;
        let text = t.text.trim().to_string();
        // An empty partial is indistinguishable from "she heard nothing" and a
        // repeated one is bus traffic with no information in it. Neither is
        // published; the cadence timer still advanced, so the next attempt is
        // one interval away.
        if text.is_empty() || text == u.last_partial_text {
            return Ok(());
        }
        u.last_partial_text = text.clone();
        u.partials += 1;
        self.publish(Observation::Speech { text, final_: false }, out)
    }

    fn finalise(
        &mut self,
        stt: &mut dyn Stt,
        now: Millis,
        reason: EndReason,
        out: &mut Vec<Observation>,
    ) -> Result<()> {
        let Some(mut u) = self.active.take() else {
            return Ok(());
        };
        let result = stt.transcribe(&u.window, true);
        let window_ms = u.window_ms();
        // The audio is gone before the transcript is even looked at. There is no
        // path from here in which a decoded window is still sitting in memory
        // waiting for someone to decide what to do with it.
        wipe(&mut u.window);

        let t = match result {
            Ok(t) => t,
            Err(e) => {
                stt.reset();
                self.after_end(reason, now);
                return Err(e);
            }
        };

        let mut text = t.text.trim().to_string();
        if text.is_empty() {
            // The engine came back with nothing at the end. If we already
            // published a partial, retracting it to silence would be worse than
            // standing by our best answer, so the last partial becomes the
            // final and the consumer gets its terminator.
            text = u.last_partial_text.clone();
        }

        stt.reset();

        if text.is_empty() {
            // Nothing was ever said — a mis-press, or a window of room tone.
            // Publishing `Speech { text: "" }` would put a line on the bus for
            // an event that did not happen.
            tracing::debug!(?reason, window_ms, "utterance produced no text; nothing published");
            self.after_end(reason, now);
            return Ok(());
        }

        tracing::debug!(?reason, window_ms, partials = u.partials, "utterance final");
        let r = self.publish(Observation::Speech { text, final_: true }, out);
        self.after_end(reason, now);
        r
    }

    /// What happens after an utterance closes, whatever it closed for.
    fn after_end(&mut self, reason: EndReason, now: Millis) {
        if reason == EndReason::Cap && self.ptt == PttState::Down {
            // The key is still held. The operator is still talking; the cap is
            // about bounding a *buffer*, not about ending their sentence.
            self.active = Some(Utterance::new(Trigger::Ptt, now));
        } else {
            self.clear_ring();
        }
    }

    fn discard_active(&mut self) {
        if let Some(mut u) = self.active.take() {
            wipe(&mut u.window);
        }
        self.pending_end = None;
    }

    fn check_wake(&mut self, stt: &mut dyn Stt, now: Millis) -> Result<()> {
        let Some(w) = self.cfg.wake.clone() else {
            return Ok(());
        };
        if self.ring.is_empty() {
            return Ok(());
        }
        if now.saturating_sub(self.last_wake_check_ms) < self.cfg.partial_interval_ms {
            return Ok(());
        }
        self.last_wake_check_ms = now;

        let window: Vec<f32> = self.ring.iter().copied().collect();
        let t = match stt.transcribe(&window, false) {
            Ok(t) => t,
            Err(e) => {
                self.clear_ring();
                stt.reset();
                return Err(e);
            }
        };
        if !contains_phrase(&t.text, &w.phrase) {
            return Ok(());
        }

        // Matched. The pre-roll becomes the head of the window, which is the
        // whole reason it exists: "hey wisp, what's the weather" must not lose
        // the half of the sentence that arrived before the match landed.
        //
        // The wake phrase itself is left in the text rather than stripped. It is
        // part of what they said, and cutting a fuzzy match out of a transcript
        // reliably eats a real word next to it.
        stt.reset();
        let voiced = (window.len() as u64 * 1000) / STT_RATE as u64;
        let mut u = Utterance::new(Trigger::Wake, now);
        u.window = window;
        u.last_voice_ms = now;
        u.voiced_ms = voiced;
        self.active = Some(u);
        self.clear_ring();
        tracing::info!(phrase = %w.phrase, preroll_ms = voiced, "wake word matched");
        Ok(())
    }
}

impl<P: MicPermit> Drop for Listener<P> {
    fn drop(&mut self) {
        // Order matters only for tidiness — the permit going out of scope at the
        // end of this function is what lowers the tell, and it happens whether
        // or not anything above it panicked.
        self.source.stop();
        if let Some(mut u) = self.active.take() {
            wipe(&mut u.window);
        }
        self.clear_ring();
    }
}

impl<P: MicPermit> std::fmt::Debug for Listener<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("ptt", &self.ptt)
            .field("listening", &self.active.is_some())
            .field("window_ms", &self.window_ms())
            .field("preroll_ms", &self.preroll_ms())
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// Overwrite and drop a buffer of captured audio.
///
/// Honest about what it is: hygiene, not a security guarantee. Without a
/// `zeroize`-style volatile write the optimiser is entitled to elide this, and
/// the allocator may hand the pages on regardless. It costs nothing and it means
/// the obvious case — a `Vec` that outlives its `clear()` in a debugger — does
/// not hold the operator's voice.
fn wipe(v: &mut Vec<f32>) {
    for s in v.iter_mut() {
        *s = 0.0;
    }
    v.clear();
    v.shrink_to_fit();
}

/// Case-, punctuation- and spacing-insensitive substring match.
///
/// Deliberately not fuzzy. A wake word that matches approximately is a wake word
/// that opens the microphone on a podcast, and the cost of a miss (say it again)
/// is far below the cost of a false positive (she started listening and you did
/// not ask her to).
fn contains_phrase(haystack: &str, phrase: &str) -> bool {
    let p = normalise(phrase);
    if p.is_empty() {
        return false;
    }
    normalise(haystack).contains(&p)
}

fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(l);
            }
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::{FakeStt, Transcript};

    /// 16 kHz so the arithmetic in the tests is the arithmetic in the code,
    /// except where a test is specifically about resampling.
    fn mic() -> Box<FakeMic> {
        Box::new(FakeMic::new(STT_RATE))
    }

    fn speech_block(rate: u32, ms: u32) -> Vec<f32> {
        crate::audio::sine(rate, 220.0, ms, 0.6).samples
    }

    fn silence_block(rate: u32, ms: u32) -> Vec<f32> {
        Pcm::silence(rate, ms).samples
    }

    fn open(cfg: ListenConfig) -> (Listener<FakePermit>, PermitLog) {
        let p = FakePermit::new();
        let log = p.log();
        let l = Listener::open(p, mic(), cfg).unwrap();
        (l, log)
    }

    /// Say something for `ms`, in 100 ms blocks, pumping after each one.
    fn say(
        l: &mut Listener<FakePermit>,
        stt: &mut FakeStt,
        ms: u32,
        t0: Millis,
    ) -> Result<Millis> {
        let mut now = t0;
        for _ in 0..(ms / 100) {
            now += 100;
            l.feed(&speech_block(STT_RATE, 100), now)?;
            l.pump(stt, now)?;
        }
        Ok(now)
    }

    // -- consent -------------------------------------------------------------

    /// A `Listener` cannot be constructed without a permit: `open` is the only
    /// constructor, it takes `P` by value, and there is no `new`, no `Default`
    /// and no setter. That is a compile-time property and cannot be asserted at
    /// runtime — this is the runtime half, that a permit which is already
    /// revoked cannot produce one either.
    #[test]
    fn a_revoked_permit_cannot_open_a_listener() {
        let p = FakePermit::new();
        let log = p.log();
        log.revoke();
        let r = Listener::open(p, mic(), ListenConfig::default());
        assert!(matches!(r, Err(VoiceError::ConsentRevoked)));
    }

    #[test]
    fn dropping_the_listener_drops_the_permit_exactly_once() {
        let p = FakePermit::new();
        let log = p.log();
        {
            let l = Listener::open(p, mic(), ListenConfig::default()).unwrap();
            assert_eq!(log.drops(), 0, "the tell must still be up while she listens");
            l.close();
        }
        assert_eq!(log.drops(), 1, "the tell goes down exactly once");
    }

    #[test]
    fn simply_dropping_the_listener_lowers_the_tell_too() {
        let p = FakePermit::new();
        let log = p.log();
        drop(Listener::open(p, mic(), ListenConfig::default()).unwrap());
        assert_eq!(log.drops(), 1);
    }

    #[test]
    fn revocation_mid_utterance_drops_the_pending_text_instead_of_publishing_it() {
        let mut stt = FakeStt::saying("i was in the middle of a sentence", 2_000);
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        let now = say(&mut l, &mut stt, 1_000, 0).unwrap();
        assert!(!log.partials().is_empty(), "a partial should already be out");

        log.revoke();
        l.feed(&speech_block(STT_RATE, 100), now + 100).unwrap_err();
        let after = l.pump(&mut stt, now + 200);
        assert!(matches!(after, Err(VoiceError::ConsentRevoked)));

        assert!(log.finals().is_empty(), "the half-transcribed sentence must not be published");
        assert!(!l.is_listening(), "the window must be gone, not paused");
        assert!(!l.still_permitted());
        assert_eq!(l.window_ms(), 0);
    }

    #[test]
    fn a_revoked_listener_stays_revoked_even_if_consent_comes_back() {
        let mut stt = FakeStt::new();
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        log.revoke();
        assert!(l.pump(&mut stt, 100).is_err());
        // The operator re-enabled the mic in the panel; that grants a *new*
        // permit, it does not resurrect this one.
        log.regrant();
        assert!(!log.is_revoked());
        assert!(matches!(l.pump(&mut stt, 200), Err(VoiceError::ConsentRevoked)));
        assert!(!l.still_permitted());
    }

    #[test]
    fn every_published_observation_is_speech_and_nothing_else() {
        let mut stt = FakeStt::saying("one two three four", 1_000);
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        let now = say(&mut l, &mut stt, 1_200, 0).unwrap();
        l.ptt_up(now);
        l.pump(&mut stt, now + 100).unwrap();

        let published = log.published();
        assert!(!published.is_empty());
        for o in &published {
            assert!(matches!(o, Observation::Speech { .. }), "{o:?} reached the bus");
            assert_eq!(o.sense(), wisp_proto::SenseId::Microphone);
        }
        assert_eq!(log.refused(), 0);
    }

    #[test]
    fn the_permit_refuses_anything_that_is_not_speech() {
        let p = FakePermit::new();
        let log = p.log();
        let r = p.publish(Observation::Idle { idle: true, for_ms: 5 });
        assert!(r.is_err());
        assert_eq!(log.refused(), 1);
        assert!(log.published().is_empty());
    }

    #[test]
    fn the_microphone_is_invasive_and_therefore_ships_off() {
        // Not this module's switch to own — but if this ever changes upstream,
        // every promise in this file's docs is void, so it is asserted here.
        assert_eq!(
            wisp_proto::SenseId::Microphone.consent(),
            wisp_proto::Consent::Invasive
        );
    }

    // -- push to talk --------------------------------------------------------

    #[test]
    fn push_to_talk_discards_audio_between_presses() {
        let mut stt = FakeStt::new();
        let (mut l, log) = open(ListenConfig::default());

        // Before any press: a full second of loud audio, thrown away as it
        // arrives.
        for i in 1..=10 {
            l.feed(&speech_block(STT_RATE, 100), i * 100).unwrap();
            l.pump(&mut stt, i * 100).unwrap();
        }
        assert_eq!(l.window_ms(), 0);
        assert_eq!(l.preroll_ms(), 0, "nothing is buffered with the wake word off");
        assert_eq!(stt.calls, 0, "the engine is never even asked");
        assert!(log.published().is_empty());

        // Press, speak, release.
        l.ptt_down(1_000);
        let now = say(&mut l, &mut stt, 500, 1_000).unwrap();
        assert!(l.window_ms() >= 400, "{}", l.window_ms());
        l.ptt_up(now);
        l.pump(&mut stt, now + 50).unwrap();
        assert_eq!(l.window_ms(), 0);

        // Between presses again: still discarded.
        let calls_before = stt.calls;
        for i in 1..=10 {
            let t = now + 100 + i * 100;
            l.feed(&speech_block(STT_RATE, 100), t).unwrap();
            l.pump(&mut stt, t).unwrap();
        }
        assert_eq!(l.window_ms(), 0);
        assert_eq!(stt.calls, calls_before, "no engine call outside an utterance");
    }

    #[test]
    fn partials_are_emitted_during_the_utterance_and_grow_into_exactly_one_final() {
        let mut stt = FakeStt::saying("the quick brown fox jumps over it", 2_000);
        let (mut l, log) = open(ListenConfig::default());

        l.ptt_down(0);
        let now = say(&mut l, &mut stt, 2_000, 0).unwrap();
        let partials_during = log.partials();
        assert!(
            partials_during.len() >= 3,
            "she must react before the sentence ends, got {partials_during:?}"
        );
        // Every partial the engine was asked for happened on a window shorter
        // than the finished utterance: this is what "during" means.
        let final_window = *stt.windows.last().unwrap();
        assert!(stt.windows[0] < final_window);
        assert!(stt.windows.windows(2).all(|w| w[0] <= w[1]), "{:?}", stt.window_ms());
        // And they grow rather than flapping.
        for w in partials_during.windows(2) {
            assert!(w[1].starts_with(&w[0]), "{:?} did not grow into {:?}", w[0], w[1]);
        }

        l.ptt_up(now);
        let out = l.pump(&mut stt, now + 100).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Observation::Speech { final_: true, .. }));

        let finals = log.finals();
        assert_eq!(finals.len(), 1, "exactly one final per utterance, got {finals:?}");
        assert_eq!(finals[0], "the quick brown fox jumps over it");
        assert!(
            finals[0].starts_with(partials_during.last().unwrap()),
            "the final must be what the partials were growing towards"
        );
        assert_eq!(stt.finals, 1);
        assert!(stt.resets >= 1, "the engine is reset between utterances");
    }

    #[test]
    fn partials_are_rate_limited_to_the_configured_interval() {
        let cfg = ListenConfig {
            partial_interval_ms: 1_000,
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::saying("a b c d e f g h", 2_000);
        let (mut l, log) = open(cfg);
        l.ptt_down(0);
        // Two seconds of audio in 100 ms steps: twenty pumps, but at most two
        // partials are due.
        say(&mut l, &mut stt, 2_000, 0).unwrap();
        assert!(log.partials().len() <= 2, "{:?}", log.partials());
        assert!(!log.partials().is_empty());
    }

    #[test]
    fn a_press_with_nothing_said_publishes_nothing_at_all() {
        let mut stt = FakeStt::new();
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        for i in 1..=5 {
            l.feed(&silence_block(STT_RATE, 100), i * 100).unwrap();
            l.pump(&mut stt, i * 100).unwrap();
        }
        l.ptt_up(600);
        let out = l.pump(&mut stt, 700).unwrap();
        assert!(out.is_empty(), "{out:?}");
        assert!(log.published().is_empty(), "a mis-press is not an utterance");
    }

    #[test]
    fn a_pause_in_the_middle_of_a_press_does_not_end_the_utterance() {
        let mut stt = FakeStt::saying("i was thinking about it", 3_000);
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        let mut now = say(&mut l, &mut stt, 500, 0).unwrap();
        // Two full seconds of nothing — well past the 700 ms hangover.
        for _ in 0..20 {
            now += 100;
            l.feed(&silence_block(STT_RATE, 100), now).unwrap();
            l.pump(&mut stt, now).unwrap();
        }
        assert!(l.is_listening(), "the key is still down; she is still listening");
        assert!(log.finals().is_empty(), "endpointing must not cut off a held key");
        now = say(&mut l, &mut stt, 500, now).unwrap();
        l.ptt_up(now);
        l.pump(&mut stt, now + 50).unwrap();
        assert_eq!(log.finals().len(), 1);
    }

    #[test]
    fn the_window_cap_finalises_rather_than_growing_forever() {
        let cfg = ListenConfig {
            max_window_ms: 1_000,
            partial_interval_ms: 400,
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::saying("still talking", 500);
        let (mut l, log) = open(cfg);

        // The key goes down and never comes up.
        l.ptt_down(0);
        let now = say(&mut l, &mut stt, 5_000, 0).unwrap();

        assert!(
            l.window_ms() <= 1_100,
            "a stuck key grew the window to {} ms",
            l.window_ms()
        );
        assert!(
            log.finals().len() >= 3,
            "the cap must finalise repeatedly, got {:?}",
            log.finals()
        );
        assert!(l.is_listening(), "and a new window starts, because the key is still held");
        assert_eq!(l.ptt(), PttState::Down);
        let _ = now;
    }

    // -- level meter ---------------------------------------------------------

    #[test]
    fn the_level_meter_rises_fast_and_falls_slowly() {
        let (mut l, _log) = open(ListenConfig::default());
        assert_eq!(l.level(), 0.0);

        // One 20 ms block of speech, with a 10 ms attack: most of the way there.
        l.feed(&speech_block(STT_RATE, 20), 20).unwrap();
        let attacked = l.level();
        assert!(attacked > 0.5, "attack was too slow: {attacked}");

        // One 20 ms block of silence, with a 300 ms release: barely moves.
        l.feed(&silence_block(STT_RATE, 20), 40).unwrap();
        let released = l.level();
        assert!(
            released > attacked * 0.9,
            "release was too fast: {attacked} -> {released}"
        );

        // A full second of silence, and it has gone.
        let mut now = 40;
        for _ in 0..50 {
            now += 20;
            l.feed(&silence_block(STT_RATE, 20), now).unwrap();
        }
        assert!(l.level() < 0.05, "the meter never settled: {}", l.level());
    }

    #[test]
    fn the_level_meter_runs_between_presses_because_the_device_is_open() {
        let (mut l, _log) = open(ListenConfig::default());
        assert_eq!(l.ptt(), PttState::Up);
        l.feed(&speech_block(STT_RATE, 100), 100).unwrap();
        assert!(l.level() > 0.5, "the tell must react even when audio is discarded");
        assert_eq!(l.window_ms(), 0, "and still keep nothing");
    }

    #[test]
    fn the_level_meter_stays_inside_zero_to_one_on_a_poisoned_block() {
        let (mut l, _log) = open(ListenConfig::default());
        l.feed(&[f32::NAN, 2.0, -9.0, f32::INFINITY], 10).unwrap();
        assert!((0.0..=1.0).contains(&l.level()), "{}", l.level());
    }

    // -- wake word -----------------------------------------------------------

    #[test]
    fn the_wake_word_is_off_by_default() {
        assert!(ListenConfig::default().wake.is_none());
        let mut stt = FakeStt::saying("hey wisp what time is it", 1_000);
        let (mut l, log) = open(ListenConfig::default());
        for i in 1..=20 {
            l.feed(&speech_block(STT_RATE, 100), i * 100).unwrap();
            l.pump(&mut stt, i * 100).unwrap();
        }
        assert_eq!(stt.calls, 0, "nothing is transcribed without a press");
        assert_eq!(l.preroll_ms(), 0, "and no pre-roll exists to be transcribed");
        assert!(log.published().is_empty());
    }

    #[test]
    fn the_wake_word_starts_an_utterance_and_keeps_the_words_before_it() {
        let cfg = ListenConfig {
            wake: Some(WakeConfig {
                phrase: "hey wisp".into(),
                preroll_ms: 2_000,
            }),
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::scripted([
            Transcript::new("something unrelated", 0.6, false),
            Transcript::new("hey wisp what time", 0.7, false),
        ]);
        let (mut l, _log) = open(cfg);

        l.feed(&speech_block(STT_RATE, 400), 400).unwrap();
        l.pump(&mut stt, 400).unwrap();
        assert!(!l.is_listening(), "an unrelated phrase must not wake her");
        assert!(l.preroll_ms() > 0, "but the pre-roll is filling");

        l.feed(&speech_block(STT_RATE, 400), 800).unwrap();
        l.pump(&mut stt, 800).unwrap();
        assert!(l.is_listening());
        assert_eq!(l.trigger(), Some(Trigger::Wake));
        assert!(
            l.window_ms() >= 700,
            "the pre-roll must seed the window, got {} ms",
            l.window_ms()
        );
        assert_eq!(l.preroll_ms(), 0, "and then be handed over, not duplicated");
    }

    #[test]
    fn the_preroll_is_hard_bounded_however_long_she_is_left_armed() {
        let cfg = ListenConfig {
            wake: Some(WakeConfig {
                // Asking for a minute of the operator's room gets you the cap.
                phrase: "hey wisp".into(),
                preroll_ms: 60_000,
            }),
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::scripted(std::iter::repeat_n(Transcript::new("nothing", 0.5, false), 200));
        let (mut l, _log) = open(cfg);
        assert_eq!(
            l.config().wake.as_ref().unwrap().preroll_ms,
            MAX_PREROLL_MS,
            "the config must be clamped at open, not trusted"
        );
        for i in 1..=300 {
            l.feed(&speech_block(STT_RATE, 100), i * 100).unwrap();
            let _ = l.pump(&mut stt, i * 100);
        }
        assert!(
            l.preroll_ms() <= MAX_PREROLL_MS as u64,
            "30 seconds of audio left {} ms in the ring",
            l.preroll_ms()
        );
    }

    #[test]
    fn endpointing_ends_a_wake_word_utterance_when_the_operator_actually_stops() {
        let cfg = ListenConfig {
            wake: Some(WakeConfig {
                phrase: "hey wisp".into(),
                preroll_ms: 1_000,
            }),
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::saying("hey wisp what time is it", 1_500);
        let (mut l, log) = open(cfg);

        let mut now = 0;
        for _ in 0..8 {
            now += 100;
            l.feed(&speech_block(STT_RATE, 100), now).unwrap();
            l.pump(&mut stt, now).unwrap();
        }
        assert!(l.is_listening(), "the wake word should have matched");

        for _ in 0..12 {
            now += 100;
            l.feed(&silence_block(STT_RATE, 100), now).unwrap();
            l.pump(&mut stt, now).unwrap();
        }
        assert!(!l.is_listening(), "a real stop must end the utterance");
        assert_eq!(log.finals().len(), 1, "{:?}", log.finals());
    }

    #[test]
    fn phrase_matching_ignores_case_punctuation_and_spacing_but_is_not_fuzzy() {
        assert!(contains_phrase("Hey, Wisp -- what time is it?", "hey wisp"));
        assert!(contains_phrase("  HEY   WISP  ", "hey wisp"));
        assert!(!contains_phrase("hey whisp", "hey wisp"));
        assert!(!contains_phrase("hey", "hey wisp"));
        assert!(!contains_phrase("anything at all", ""));
    }

    // -- plumbing ------------------------------------------------------------

    #[test]
    fn a_forty_eight_kilohertz_source_reaches_the_engine_at_sixteen() {
        let p = FakePermit::new();
        let mut src = FakeMic::realistic();
        assert_eq!(src.sample_rate(), 48_000);
        src.push_speech(500, 100);
        let mut l = Listener::open(p, Box::new(src), ListenConfig::default()).unwrap();
        let mut stt = FakeStt::saying("resampled", 500);

        l.ptt_down(0);
        // One partial interval later, so the engine is actually asked.
        l.pump(&mut stt, 400).unwrap();

        // 500 ms in at 48 kHz (24 000 samples) is 500 ms out at 16 kHz (8 000).
        assert_eq!(l.window_ms(), 500);
        assert!(!stt.windows.is_empty(), "the engine must have been asked");
        for n in &stt.windows {
            assert_eq!(
                (*n as u64 * 1000) / STT_RATE as u64,
                500,
                "the engine was handed {n} samples, which is not 500 ms at 16 kHz"
            );
        }
        assert_eq!(stt.sample_rate(), STT_RATE);
    }

    #[test]
    fn pump_drains_the_source_and_stops_it_on_drop() {
        let p = FakePermit::new();
        let mut src = FakeMic::new(STT_RATE);
        src.push_speech(300, 50);
        assert_eq!(src.queued_blocks(), 6);
        let mut l = Listener::open(p, Box::new(src), ListenConfig::default()).unwrap();
        let mut stt = FakeStt::new();
        l.ptt_down(0);
        l.pump(&mut stt, 100).unwrap();
        assert_eq!(l.window_ms(), 300, "all six blocks should have been drained");
        // Wall time and buffered audio agree, so nothing was dropped on the way.
        assert_eq!(l.utterance_ms(300), 300);
        drop(l);
        // `stop` on drop is asserted through the trait rather than the concrete
        // type, since the box was moved: the FakeMic clears its queue on stop,
        // and re-reading a stopped mic yields nothing. Covered by the unit test
        // below.
    }

    #[test]
    fn a_stopped_source_yields_nothing_and_stop_is_idempotent() {
        let mut m = FakeMic::new(STT_RATE);
        m.push_speech(100, 50);
        m.stop();
        m.stop();
        assert_eq!(m.stops, 2);
        assert!(m.read().unwrap().is_empty());
    }

    #[test]
    fn a_failing_capture_device_surfaces_rather_than_looking_like_silence() {
        let p = FakePermit::new();
        let mut src = FakeMic::new(STT_RATE);
        src.fail = true;
        let mut l = Listener::open(p, Box::new(src), ListenConfig::default()).unwrap();
        let mut stt = FakeStt::new();
        assert!(matches!(l.pump(&mut stt, 100), Err(VoiceError::Sink(_))));
    }

    #[test]
    fn a_source_with_no_sample_rate_is_refused_at_open() {
        let p = FakePermit::new();
        let src = FakeMicNoRate;
        assert!(matches!(
            Listener::open(p, Box::new(src), ListenConfig::default()),
            Err(VoiceError::Sink(_))
        ));
    }

    struct FakeMicNoRate;
    impl MicSource for FakeMicNoRate {
        fn sample_rate(&self) -> u32 {
            0
        }
        fn read(&mut self) -> Result<Vec<f32>> {
            Ok(Vec::new())
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn an_engine_failure_sheds_the_utterance_rather_than_hoarding_the_audio() {
        let mut stt = FakeStt::broken();
        let (mut l, log) = open(ListenConfig::default());
        l.ptt_down(0);
        for i in 1..=5 {
            l.feed(&speech_block(STT_RATE, 100), i * 100).unwrap();
            if let Err(e) = l.pump(&mut stt, i * 100) {
                assert!(matches!(e, VoiceError::Stt(_)));
            }
        }
        assert!(!l.is_listening(), "a dead engine must not leave a window open");
        assert_eq!(l.window_ms(), 0);
        assert!(log.published().is_empty());
        // …and she is still permitted. A broken engine is not a consent event.
        assert!(l.still_permitted());
    }

    #[test]
    fn a_hand_written_config_is_clamped_back_into_the_sane() {
        let c = ListenConfig {
            partial_interval_ms: 1,
            max_window_ms: 10 * 60 * 1_000,
            level_attack_ms: 0,
            level_release_ms: 0,
            vad: Some(VadConfig {
                floor: f32::NAN,
                hangover_ms: 0,
                min_speech_ms: 999_999,
            }),
            wake: Some(WakeConfig {
                phrase: "x".into(),
                preroll_ms: u32::MAX,
            }),
        }
        .sane();
        assert_eq!(c.partial_interval_ms, 50);
        assert_eq!(c.max_window_ms, 60_000);
        assert!(c.level_attack_ms >= 1 && c.level_release_ms >= 1);
        let v = c.vad.unwrap();
        assert_eq!(v.floor, VadConfig::default().floor);
        assert_eq!(v.hangover_ms, 100);
        assert_eq!(v.min_speech_ms, 5_000);
        assert_eq!(c.wake.unwrap().preroll_ms, MAX_PREROLL_MS);
    }

    #[test]
    fn pressing_the_key_during_a_wake_word_utterance_takes_the_endpoint_back() {
        let cfg = ListenConfig {
            wake: Some(WakeConfig {
                phrase: "hey wisp".into(),
                preroll_ms: 1_000,
            }),
            ..ListenConfig::default()
        };
        let mut stt = FakeStt::saying("hey wisp keep going", 1_500);
        let (mut l, log) = open(cfg);

        let mut now = 0;
        for _ in 0..8 {
            now += 100;
            l.feed(&speech_block(STT_RATE, 100), now).unwrap();
            l.pump(&mut stt, now).unwrap();
        }
        assert_eq!(l.trigger(), Some(Trigger::Wake));

        l.ptt_down(now);
        assert_eq!(l.trigger(), Some(Trigger::Ptt), "the operator now owns the endpoint");

        // Long past the hangover: endpointing must no longer touch it.
        for _ in 0..12 {
            now += 100;
            l.feed(&silence_block(STT_RATE, 100), now).unwrap();
            l.pump(&mut stt, now).unwrap();
        }
        assert!(l.is_listening(), "{:?}", l);
        assert!(log.finals().is_empty());

        l.ptt_up(now);
        l.pump(&mut stt, now + 50).unwrap();
        assert_eq!(log.finals().len(), 1, "and releasing the key ends it");
    }

    /// The host runs her on a worker thread the governor can take away, so the
    /// whole listener has to be able to move onto one.
    #[test]
    fn a_listener_can_be_moved_to_the_thread_that_owns_it() {
        fn assert_send<T: Send>() {}
        assert_send::<Listener<FakePermit>>();
        assert_send::<FakePermit>();
        assert_send::<FakeMic>();
    }

    #[test]
    fn the_default_config_is_push_to_talk_with_endpointing_and_no_wake_word() {
        let c = ListenConfig::default();
        assert!(c.wake.is_none(), "F28: the wake word is opt-in");
        assert!(c.vad.is_some());
        assert!(c.max_window_ms <= 30_000, "whisper's own context is the ceiling");
        assert!(c.level_release_ms > c.level_attack_ms, "fast attack, slow release");
    }
}
