//! The facade, and the crate's [`wisp_proto::Governed`] implementation.
//!
//! Every part of this crate is separately testable and separately useful, which
//! is why they are separate modules. But exactly one thing has to hold all of
//! them at once, because the interesting properties are the ones that live
//! *between* them:
//!
//! - the duck goes down when she starts and comes back up when the sink has
//!   actually drained, not when the last clause was synthesised;
//! - barge-in has to reach the speaker, the sink and the duck in one step;
//! - and a tier downgrade has to do all of that **synchronously**, plus drop the
//!   microphone permit, which lowers the visible tell.
//!
//! ## Why the tier change is the hard part
//!
//! SPEC §3.1: *"Called on every tier change. Must not block. Must not fail."*
//! and *"A subsystem that cannot honour a downgrade must shed the work, not
//! queue it."* That rules out an awful lot of designs. It means [`Voice`] must
//! own its sink and its mixer, because `set_tier` gets no arguments and cannot
//! return an error — a facade that had to be *handed* a sink to go quiet could
//! not go quiet on demand. It means there is no channel to a worker thread,
//! because a message is a request and this has to be an act. And it means the
//! unsaid half of her sentence is dropped and recorded, never stored for later:
//! the deferred queue of §3.5 belongs to `wisp-mind` and to nothing else.
//!
//! ## The microphone and T3
//!
//! `set_tier(Lobotomised)` drops the [`crate::mic::Listener`], which drops the
//! permit, which — if that permit came from the real consent ledger — lowers the
//! visible tell of SPEC §0.3. She is not listening while the operator is in a
//! headset. Getting there requires no cooperation from the listener and no flag
//! anybody has to remember to check: the capability *is* the object, so
//! destroying the object removes the capability.

use wisp_proto::{Cost, Governed, Observation, Tier, TierReason};

use crate::barge::{BargeIn, BargePolicy, BargeSignal, CancelReason};
use crate::duck::{DuckConfig, Ducker, Mixer};
use crate::lipsync::DriveFrame;
use crate::mic::{Listener, MicPermit, MicSource};
use crate::sink::AudioSink;
use crate::speaker::{SpeakerConfig, SpeechEvent, SpeechId, Speaker, SpeechState};
use crate::stt::Stt;
use crate::tell::{TellDrive, TellFeed};
use crate::tier::{cost_at, policy, VoicePolicy};
use crate::tts::Tts;
use crate::voices::{Mood, VoicePack, VoiceRegistry};
use crate::{Millis, Result, VoiceError};

/// An open microphone, without infecting [`Voice`] with the permit's type.
///
/// [`Listener`] is generic over its permit so that "you cannot build one without
/// consent" is a compile-time fact. That generic must not spread to every type
/// that merely *holds* a listener, or the binary would end up threading a
/// permit type through half the tree. This trait is the seam: object-safe, with
/// one blanket impl, so `Voice` stores a `Box<dyn OpenMic>` and the type-level
/// guarantee stays where it belongs.
pub trait OpenMic: Send {
    fn level(&self) -> f32;
    fn ptt_down(&mut self, now: Millis);
    fn ptt_up(&mut self, now: Millis);
    fn feed(&mut self, samples: &[f32], now: Millis) -> Result<()>;
    fn pump(&mut self, stt: &mut dyn Stt, now: Millis) -> Result<Vec<Observation>>;
    fn still_permitted(&self) -> bool;
}

impl<P: MicPermit + 'static> OpenMic for Listener<P> {
    fn level(&self) -> f32 {
        Listener::level(self)
    }
    fn ptt_down(&mut self, now: Millis) {
        Listener::ptt_down(self, now)
    }
    fn ptt_up(&mut self, now: Millis) {
        Listener::ptt_up(self, now)
    }
    fn feed(&mut self, samples: &[f32], now: Millis) -> Result<()> {
        Listener::feed(self, samples, now)
    }
    fn pump(&mut self, stt: &mut dyn Stt, now: Millis) -> Result<Vec<Observation>> {
        Listener::pump(self, stt, now)
    }
    fn still_permitted(&self) -> bool {
        Listener::still_permitted(self)
    }
}

/// Everything one `tick` produced. Facts about the past, for the flight
/// recorder (SPEC §3.2) — never commands.
#[derive(Debug, Default)]
pub struct VoiceTick {
    pub speech: Vec<SpeechEvent>,
    /// Partials and finals from the microphone, already published through the
    /// consent gate. Returned as well so the host can record them.
    pub heard: Vec<Observation>,
    /// She was cut off during this tick, and why.
    pub cancelled: Option<CancelReason>,
}

impl VoiceTick {
    pub fn is_empty(&self) -> bool {
        self.speech.is_empty() && self.heard.is_empty() && self.cancelled.is_none()
    }
}

/// The whole voice subsystem, as the binary sees it.
pub struct Voice {
    tier: Tier,
    policy: VoicePolicy,
    voices: VoiceRegistry,
    speaker: Speaker,
    sink: Box<dyn AudioSink>,
    ducker: Ducker,
    barge: BargeIn,
    tell: TellFeed,
    mic: Option<Box<dyn OpenMic>>,
    /// Was the sink draining last tick? Used to notice the moment she really
    /// stopped making noise, which is when the duck may be released.
    was_busy: bool,
}

impl std::fmt::Debug for Voice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Voice")
            .field("tier", &self.tier)
            .field("speaking", &self.speaker.is_speaking())
            .field("ducked", &self.ducker.is_ducked())
            .field("listening", &self.mic.is_some())
            .finish()
    }
}

impl Voice {
    /// Build her. `mixer` is what the duck acts through; `sink` is where audio
    /// goes. Both are owned, because a downgrade has to be able to reach them
    /// with no arguments — see the module docs.
    ///
    /// Starts at [`Tier::Full`]; the governor will correct that on its first
    /// step, and starting anywhere more capable would mean a moment at boot when
    /// she is allowed more than the machine can spare.
    pub fn new(sink: Box<dyn AudioSink>, mixer: Box<dyn Mixer>) -> Self {
        Voice::builder().build(sink, mixer)
    }

    /// Everything that is a choice rather than a dependency.
    pub fn builder() -> VoiceBuilder {
        VoiceBuilder::default()
    }

    /// Pay off a journal left behind by a previous run that was killed
    /// mid-sentence.
    ///
    /// **Call this at startup, before anything else.** A companion that
    /// permanently ducked your music because she was `SIGKILL`ed is worse than
    /// one that never speaks, and this is the only thing that undoes it.
    pub fn recover_ducking(&mut self) -> Result<crate::duck::Recovered> {
        self.ducker.recover_owned()
    }

    pub fn tier(&self) -> Tier {
        self.tier
    }

    pub fn policy(&self) -> &VoicePolicy {
        &self.policy
    }

    pub fn voices(&self) -> &VoiceRegistry {
        &self.voices
    }

    pub fn voices_mut(&mut self) -> &mut VoiceRegistry {
        &mut self.voices
    }

    /// The pack the governor currently allows. `None` means she is silenced.
    pub fn current_pack(&self) -> Option<&VoicePack> {
        self.voices.for_tier(self.tier)
    }

    pub fn is_speaking(&self) -> bool {
        self.speaker.is_speaking()
    }

    pub fn is_listening(&self) -> bool {
        self.mic.is_some()
    }

    pub fn is_ducked(&self) -> bool {
        self.ducker.is_ducked()
    }

    // -- speaking ----------------------------------------------------------

    /// Begin a streamed utterance. `None` when the tier forbids speech.
    pub fn begin(&mut self, mood: Mood) -> Option<SpeechId> {
        if !self.policy.speak {
            return None;
        }
        let pack = self.voices.for_tier(self.tier)?.clone();
        Some(self.speaker.begin(&pack, mood))
    }

    /// Feed text as `wisp-mind` produces it.
    pub fn push(&mut self, text: &str) {
        self.speaker.push(text);
    }

    /// The model has stopped writing.
    pub fn end_text(&mut self) {
        self.speaker.end_text();
    }

    /// One canned line, start to finish.
    pub fn say(&mut self, text: &str, mood: Mood) -> Option<SpeechId> {
        let id = self.begin(mood)?;
        self.push(text);
        self.end_text();
        Some(id)
    }

    /// Advance everything: synthesis, playback, ducking, and the microphone.
    ///
    /// `now` is the host's monotonic clock. Nothing in this crate reads a clock
    /// of its own, so the whole subsystem is replayable from a trace.
    pub fn tick(&mut self, tts: &mut dyn Tts, stt: Option<&mut dyn Stt>, now: Millis) -> VoiceTick {
        let mut out = VoiceTick::default();

        // 1. Ducking is engaged *before* the first sample is written, so her
        //    first syllable does not land on top of the music at full volume.
        if self.policy.speak && self.speaker.is_speaking() && !self.was_busy {
            if self.policy.duck {
                self.ducker.duck(now);
            }
            self.barge.speaking_started(now);
            self.was_busy = true;
        }
        self.ducker.tick(now);

        // 2. Synthesis and playback.
        match self.speaker.pump(tts, self.sink.as_mut(), now) {
            Ok(evs) => out.speech = evs,
            Err(e) => {
                tracing::warn!(error = %e, "speech pump failed");
                out.speech.push(SpeechEvent::Failed { id: 0, why: e.to_string() });
            }
        }

        // 3. She is only really finished when the sink has emptied. Releasing on
        //    the last *synthesised* clause would bring the music back up under
        //    her final word.
        let busy = self.speaker.is_speaking() || self.sink.queued_ms() > 0;
        if self.was_busy && !busy {
            self.ducker.release(now);
            self.barge.speaking_stopped(now);
            self.was_busy = false;
        }

        // 4. The microphone, if one is open and consent still holds.
        if let (Some(mic), Some(stt)) = (self.mic.as_mut(), stt) {
            self.tell.set_level(mic.level());
            match mic.pump(stt, now) {
                Ok(obs) => out.heard = obs,
                Err(VoiceError::ConsentRevoked) => {
                    // The operator flipped the switch. Let go at once — the
                    // drop is what lowers the tell.
                    tracing::info!("microphone consent revoked; closing");
                    self.mic = None;
                    self.tell.set_active(false);
                }
                Err(e) => tracing::warn!(error = %e, "microphone pump failed"),
            }
        }
        out
    }

    /// Push-to-talk, and the level meter, only exist while a mic is open.
    pub fn feed_mic(&mut self, samples: &[f32], now: Millis) {
        let Some(m) = self.mic.as_mut() else { return };
        // Checked here as well as inside the listener: this is the point where
        // the operator's room would enter our address space, and it is worth
        // one comparison to be certain it does not happen a buffer late.
        if !m.still_permitted() {
            tracing::info!("microphone consent revoked; dropping the buffer unread");
            self.mic = None;
            self.tell.set_active(false);
            return;
        }
        if let Err(e) = m.feed(samples, now) {
            tracing::warn!(error = %e, "microphone feed failed");
        }
    }

    /// What `wisp_shell::tell::build` should be drawn with this frame.
    pub fn tell(&mut self, now: Millis) -> TellDrive {
        self.tell.tick(now)
    }

    /// The rig drive signal for this frame. A closed mouth when she is silent.
    pub fn drive(&self) -> DriveFrame {
        self.speaker.drive(self.sink.as_ref())
    }

    pub fn state(&self) -> SpeechState {
        self.speaker.state(self.sink.as_ref())
    }

    // -- barge-in ----------------------------------------------------------

    /// The operator did something. Returns the cancellation if it caused one.
    pub fn signal(&mut self, sig: BargeSignal, now: Millis) -> Option<SpeechEvent> {
        let reason = self.barge.observe(sig, now)?;
        self.stop(reason, now)
    }

    /// Stop her now. Flushes the sink, sheds the rest of the utterance and
    /// releases the duck.
    pub fn stop(&mut self, reason: CancelReason, now: Millis) -> Option<SpeechEvent> {
        let ev = self.speaker.cancel(reason, self.sink.as_mut());
        if self.was_busy {
            self.ducker.release_all(now);
            self.barge.speaking_stopped(now);
            self.was_busy = false;
        }
        ev
    }

    // -- listening ---------------------------------------------------------

    /// Open the microphone.
    ///
    /// The only way in, and it needs a [`MicPermit`] — for the real one, see
    /// `consent_adapter::GrantedMic`, whose sole source is
    /// `ConsentLedger::grant`. Refused outright when the tier forbids listening,
    /// which is what makes "no STT at T3" true rather than merely intended.
    pub fn listen<P: MicPermit + 'static>(
        &mut self,
        permit: P,
        source: Box<dyn MicSource>,
        mut cfg: crate::mic::ListenConfig,
    ) -> Result<()> {
        if !self.policy.listen {
            return Err(VoiceError::Tier { tier: self.tier, what: "the microphone" });
        }
        if !self.policy.wake_word {
            // A wake word means a continuously open microphone. At T2 that is a
            // background cost the machine cannot spare, so push-to-talk only.
            cfg.wake = None;
        }
        let listener = Listener::open(permit, source, cfg)?;
        self.mic = Some(Box::new(listener));
        self.tell.set_active(true);
        Ok(())
    }

    /// Close the microphone. Dropping the listener drops the permit, which
    /// lowers the visible tell.
    pub fn stop_listening(&mut self) {
        if self.mic.take().is_some() {
            tracing::info!("microphone closed");
        }
        self.tell.set_active(false);
    }

    pub fn ptt_down(&mut self, now: Millis) {
        if let Some(m) = self.mic.as_mut() {
            m.ptt_down(now);
        }
    }

    pub fn ptt_up(&mut self, now: Millis) {
        if let Some(m) = self.mic.as_mut() {
            m.ptt_up(now);
        }
    }

    // -- the governor ------------------------------------------------------

    /// Push the current [`VoicePolicy`] into every part that has a knob.
    fn apply_policy(&mut self) {
        let p = self.policy;
        let base = SpeakerConfig::default();
        self.speaker.set_config(SpeakerConfig {
            fps: p.drive_fps.max(1),
            lookahead_ms: p.lookahead_ms,
            silent: !p.speak,
            ..base
        });
        let mut bp = self.barge.policy().clone();
        // Barge-in on the operator's voice needs a microphone, and there may
        // not be one. Turning it on with no capture running would be a rule
        // that can never fire, which is worse than an absent one.
        bp.mic_enabled = p.listen && bp.mic_enabled;
        self.barge.set_policy(bp);
    }
}

/// Assembles a [`Voice`].
///
/// A builder rather than a six-argument constructor mostly because of
/// `journal_in`: the ducking journal has to be per-instance in tests, or two
/// tests running on different threads pay off each other's journals and both
/// fail in ways that look like a bug in the ducker. That is not a hypothetical
/// — it is what happened the first time these tests were written.
#[derive(Debug, Default)]
pub struct VoiceBuilder {
    voices: Option<VoiceRegistry>,
    duck: Option<DuckConfig>,
    barge: Option<BargePolicy>,
    journal_in: Option<std::path::PathBuf>,
}

impl VoiceBuilder {
    pub fn voices(mut self, v: VoiceRegistry) -> Self {
        self.voices = Some(v);
        self
    }
    pub fn duck(mut self, c: DuckConfig) -> Self {
        self.duck = Some(c);
        self
    }
    pub fn barge(mut self, p: BargePolicy) -> Self {
        self.barge = Some(p);
        self
    }
    /// Where the crash-recovery journal lives. Defaults to
    /// [`crate::data_dir`], which is what the binary wants; a test wants its
    /// own directory.
    pub fn journal_in(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.journal_in = Some(dir.into());
        self
    }

    /// Starts at [`Tier::Full`]; the governor corrects that on its first step,
    /// and starting anywhere more capable would mean a moment at boot when she
    /// is allowed more than the machine can spare.
    pub fn build(self, sink: Box<dyn AudioSink>, mixer: Box<dyn Mixer>) -> Voice {
        let tier = Tier::Full;
        let duck_cfg = self.duck.unwrap_or_default();
        let ducker = match self.journal_in {
            Some(dir) => Ducker::new_in(dir, mixer, duck_cfg),
            None => Ducker::new(mixer, duck_cfg),
        };
        let mut v = Voice {
            tier,
            policy: policy(tier),
            voices: self.voices.unwrap_or_else(VoiceRegistry::builtin),
            speaker: Speaker::default(),
            sink,
            ducker,
            barge: BargeIn::new(self.barge.unwrap_or_default()),
            tell: TellFeed::default(),
            mic: None,
            was_busy: false,
        };
        v.apply_policy();
        v
    }
}

impl Governed for Voice {
    /// SPEC §3.1: immediate, synchronous, infallible, and it sheds.
    fn set_tier(&mut self, tier: Tier, reason: &TierReason) {
        let now = 0; // Downgrades cannot wait for a clock. See below.
        let was = self.tier;
        self.tier = tier;
        self.policy = policy(tier);
        self.apply_policy();

        // The microphone goes first. At T3 she stops listening entirely, and
        // dropping the listener drops the permit, which lowers the tell of SPEC
        // §0.3 — no cooperation required from anything.
        if !self.policy.listen && self.mic.is_some() {
            tracing::info!(?tier, ?reason, "tier closed the microphone");
            self.stop_listening();
        }

        // Then her voice. A downgrade that forbids speech must cut the sentence
        // in flight; §3.1 says shed, not queue, and there is deliberately
        // nowhere here to queue it.
        if !self.policy.speak && self.speaker.is_speaking() {
            let ev = self.speaker.cancel(CancelReason::Tier, self.sink.as_mut());
            if let Some(SpeechEvent::Cancelled { unsaid, .. }) = &ev {
                tracing::info!(?tier, unsaid = %unsaid, "tier cut her off mid-sentence");
            }
            let _ = self.barge.cancel_for_tier(now);
        }
        if !self.policy.speak {
            self.sink.flush();
        }

        // A pack the new tier forbids is swapped out for one it allows. The
        // registry answers that; nothing here knows which engines exist.
        if let Some(pack) = self.voices.for_tier(tier) {
            let id = pack.id.clone();
            if self.voices.selected_id() != id && !self.voices.selected().usable_at(tier) {
                tracing::info!(?tier, voice = %id, "tier changed her voice");
            }
        }

        // Finally the duck. Anything below T3 is not allowed to hold the
        // operator's audio down, and a dormant companion that is still ducking
        // is a bug you would have to kill her to fix.
        if !self.policy.duck && self.ducker.is_ducked() {
            self.ducker.release_all(now);
            self.was_busy = false;
        }
        if was != tier {
            tracing::debug!(from = ?was, to = ?tier, ?reason, "voice tier");
        }
    }

    fn cost_at(tier: Tier) -> Cost {
        cost_at(tier)
    }
}

impl Drop for Voice {
    /// Last line of defence for the operator's music. `Ducker` has its own
    /// `Drop` and its own journal, so this is belt and braces — but the belt is
    /// cheap and the failure it prevents is one the operator would have to
    /// reboot to fix.
    fn drop(&mut self) {
        if self.ducker.is_ducked() {
            self.ducker.release_all(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Pcm;
    use crate::duck::FakeMixer;
    use crate::mic::{FakeMic, FakePermit, ListenConfig};
    use crate::sink::BufferSink;
    use crate::stt::FakeStt;
    use crate::tts::FakeTts;
    use std::sync::{Arc, Mutex};

    const RATE: u32 = 22_050;

    /// A second handle on the same sink, so a test can advance playback while
    /// [`Voice`] owns it. The same trick [`FakeMixer`] uses, and for the same
    /// reason: the audio server does not belong to her.
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<BufferSink>>);

    impl SharedSink {
        fn new(rate: u32) -> Self {
            SharedSink(Arc::new(Mutex::new(BufferSink::new(rate))))
        }
        fn advance(&self, ms: u32) {
            self.0.lock().unwrap().advance(ms);
        }
        fn written_ms(&self) -> u32 {
            self.0.lock().unwrap().written_ms()
        }
        fn heard_ms(&self) -> u32 {
            self.0.lock().unwrap().heard_ms()
        }
        fn flushes(&self) -> usize {
            self.0.lock().unwrap().flushes
        }
    }

    impl AudioSink for SharedSink {
        fn name(&self) -> &str {
            "shared"
        }
        fn sample_rate(&self) -> u32 {
            self.0.lock().unwrap().sample_rate()
        }
        fn write(&mut self, pcm: &Pcm) -> Result<()> {
            self.0.lock().unwrap().write(pcm)
        }
        fn queued_ms(&self) -> u32 {
            self.0.lock().unwrap().queued_ms()
        }
        fn flush(&mut self) {
            self.0.lock().unwrap().flush()
        }
        fn stop(&mut self) {
            self.0.lock().unwrap().stop()
        }
    }

    struct Rig {
        voice: Voice,
        tts: FakeTts,
        audio: FakeMixer,
        sink: SharedSink,
        firefox: crate::duck::StreamKey,
        spotify: crate::duck::StreamKey,
        now: Millis,
        journal: std::path::PathBuf,
    }

    fn next_journal_id() -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn rig() -> Rig {
        // SPEC §4: never the operator's real store. Set once, process-wide, and
        // never moved again — see `resolve_data_dir` in lib.rs for why nothing
        // in this crate flips it back and forth.
        let tmp = std::env::temp_dir().join(format!("nx-wisp-voice-facade-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).ok();
        std::env::set_var("NX_WISP_CONFIG_DIR", &tmp);

        let audio = FakeMixer::new();
        let firefox = audio.add("Firefox", 1, 0.8);
        let spotify = audio.add("Spotify", 2, 0.6);
        let mut reg = VoiceRegistry::builtin();
        reg.select("test-tone").unwrap();
        let sink = SharedSink::new(RATE);
        // Its own journal directory. Two rigs on two threads paying off each
        // other's journals is not a bug in the ducker, but it looks exactly
        // like one.
        let journal = tmp.join(format!("j{}", next_journal_id()));
        std::fs::create_dir_all(&journal).ok();
        let voice = Voice::builder()
            .voices(reg)
            // No ramp: this is a test of *what* is set, not of how smoothly.
            .duck(DuckConfig { fade_ms: 0, ..DuckConfig::default() })
            .journal_in(&journal)
            .build(Box::new(sink.clone()), Box::new(audio.clone()));
        Rig { voice, tts: FakeTts::at_rate(RATE), audio, sink, firefox, spotify, now: 0, journal }
    }

    impl Rig {
        fn step(&mut self, ms: u32) -> VoiceTick {
            let mut tts = std::mem::replace(&mut self.tts, FakeTts::at_rate(RATE));
            let t = self.voice.tick(&mut tts, None, self.now);
            self.tts = tts;
            self.sink.advance(ms);
            self.now += ms as Millis;
            t
        }
        /// Run until she stops, or `steps` slices have passed.
        fn run(&mut self, steps: usize) -> Vec<SpeechEvent> {
            let mut evs = Vec::new();
            for _ in 0..steps {
                evs.extend(self.step(50).speech);
                if !self.voice.is_speaking() && self.sink.queued_ms() == 0 {
                    // One more tick so the release path runs.
                    evs.extend(self.step(50).speech);
                    break;
                }
            }
            evs
        }
        fn volumes(&self) -> (f32, f32) {
            (
                self.audio.volume_of(&self.firefox).unwrap(),
                self.audio.volume_of(&self.spotify).unwrap(),
            )
        }
    }

    // -- ducking -----------------------------------------------------------

    #[test]
    fn she_ducks_the_operators_music_before_her_first_syllable_and_puts_it_back() {
        let mut r = rig();
        let before = r.volumes();
        r.voice.say("Your build is green at last.", Mood::Neutral).unwrap();

        r.step(0);
        assert!(r.voice.is_ducked(), "she started talking without ducking");
        let ducked = r.volumes();
        assert!(ducked.0 < before.0 && ducked.1 < before.1, "{ducked:?} vs {before:?}");

        r.run(400);
        assert!(!r.voice.is_ducked(), "the duck outlived the sentence");
        assert_eq!(r.volumes(), before, "the music did not come back");
    }

    #[test]
    fn the_music_stays_down_until_her_last_word_has_actually_been_heard() {
        // Releasing on the last synthesised clause would bring the music up
        // underneath her final word — the audio is still in the sink.
        let mut r = rig();
        r.voice.say("One thing. Two things. Three things.", Mood::Neutral).unwrap();
        r.step(0);
        for _ in 0..200 {
            r.step(50);
            if !r.voice.is_speaking() {
                break;
            }
        }
        // Text is exhausted, but audio is still queued.
        if r.sink.queued_ms() > 0 {
            assert!(r.voice.is_ducked(), "released with {}ms still to play", r.sink.queued_ms());
        }
        r.run(400);
        assert!(!r.voice.is_ducked());
    }

    #[test]
    fn two_sentences_back_to_back_are_one_duck_not_two() {
        let mut r = rig();
        let before = r.volumes();
        r.voice.say("First sentence here.", Mood::Neutral).unwrap();
        r.step(0);
        let once = r.volumes();
        r.step(50);
        assert_eq!(r.volumes(), once, "the second tick ducked again on top of the first");
        r.run(400);
        assert_eq!(r.volumes(), before);
    }

    #[test]
    fn dropping_her_mid_sentence_still_gives_the_operator_their_music_back() {
        let mut r = rig();
        let before = r.volumes();
        r.voice.say("A sentence she will never finish saying.", Mood::Neutral).unwrap();
        r.step(0);
        assert_ne!(r.volumes(), before);
        drop(r.voice);
        assert_eq!(r.audio.volume_of(&r.firefox).unwrap(), before.0, "Drop must restore");
        assert_eq!(r.audio.volume_of(&r.spotify).unwrap(), before.1);
    }

    // -- barge-in ----------------------------------------------------------

    #[test]
    fn typing_cuts_her_off_and_the_music_comes_straight_back() {
        let mut r = rig();
        let before = r.volumes();
        r.voice
            .say("I was going to tell you something quite long about your machine.", Mood::Neutral)
            .unwrap();
        r.step(0);
        // Past the grace window, then a real burst of typing.
        r.step(500);
        assert!(r.voice.is_ducked());

        let mut cut = None;
        for _ in 0..6 {
            cut = cut.or(r.voice.signal(BargeSignal::Keystroke, r.now));
            // Wider than the autorepeat debounce: a held key is not typing.
            r.now += 80;
        }
        let ev = cut.expect("six keystrokes must cancel");
        assert!(matches!(ev, SpeechEvent::Cancelled { reason: CancelReason::Typing, .. }), "{ev:?}");
        assert!(!r.voice.is_speaking());
        assert!(!r.voice.is_ducked(), "barge-in left the music ducked");
        assert_eq!(r.volumes(), before);
        assert_eq!(r.sink.flushes(), 1, "the queued audio must be dropped, not drained");
        assert!(r.sink.heard_ms() < r.sink.written_ms(), "she was not actually cut off");
    }

    #[test]
    fn a_single_stray_keystroke_does_not_stop_her() {
        let mut r = rig();
        r.voice.say("Something worth finishing, all the way to the end.", Mood::Neutral).unwrap();
        r.step(0);
        r.step(500);
        assert!(r.voice.signal(BargeSignal::Keystroke, r.now).is_none());
        assert!(r.voice.is_speaking());
    }

    #[test]
    fn she_does_not_interrupt_herself_on_her_own_first_syllable() {
        // Her audio bleeds into the microphone. Without the grace window she
        // cancels herself the instant she starts. This is the classic bug.
        let mut r = rig();
        r.voice
            .barge
            .set_policy(BargePolicy { mic_enabled: true, ..BargePolicy::default() });
        r.voice.say("Hello there, I have something to say.", Mood::Neutral).unwrap();
        r.step(0);
        for _ in 0..5 {
            assert!(
                r.voice.signal(BargeSignal::MicLevel { peak: 0.9 }, r.now).is_none(),
                "she cancelled herself inside the grace window"
            );
            r.now += 40;
        }
        assert!(r.voice.is_speaking());
    }

    #[test]
    fn asking_her_to_stop_works_immediately_with_no_debounce_at_all() {
        let mut r = rig();
        r.voice.say("A long explanation nobody asked for.", Mood::Neutral).unwrap();
        r.step(0);
        let ev = r.voice.signal(BargeSignal::Explicit, r.now).expect("explicit is immediate");
        assert!(matches!(ev, SpeechEvent::Cancelled { reason: CancelReason::Explicit, .. }));
        assert!(!r.voice.is_ducked());
    }

    // -- the governor ------------------------------------------------------

    #[test]
    fn t4_silences_her_mid_sentence_and_lets_go_of_the_operators_audio() {
        let mut r = rig();
        let before = r.volumes();
        r.voice.say("Something she is halfway through saying right now.", Mood::Neutral).unwrap();
        r.step(0);
        assert!(r.voice.is_ducked());

        r.voice.set_tier(Tier::Dormant, &TierReason::Pinned);

        assert!(!r.voice.is_speaking(), "T4 must cut the sentence, not let it finish");
        assert!(!r.voice.is_ducked(), "a dormant companion must not hold your music down");
        assert_eq!(r.volumes(), before);
        assert!(r.voice.begin(Mood::Neutral).is_none(), "T4 cannot start a new utterance");
        assert!(r.voice.current_pack().is_none());
    }

    #[test]
    fn a_downgrade_sheds_the_rest_of_the_sentence_rather_than_queueing_it() {
        // SPEC §3.1. The deferred queue of §3.5 belongs to wisp-mind alone.
        let mut r = rig();
        r.voice
            .say("The first part. And a second part she never reaches at all.", Mood::Neutral)
            .unwrap();
        r.step(0);
        r.voice.set_tier(Tier::Dormant, &TierReason::PowerCritical);
        assert!(!r.voice.is_speaking());

        // Coming back up must not resurrect it.
        r.voice.set_tier(Tier::Full, &TierReason::Idle);
        let t = r.step(50);
        assert!(t.speech.is_empty(), "the shed sentence came back: {:?}", t.speech);
        assert!(!r.voice.is_speaking());
    }

    #[test]
    fn t2_makes_her_cheaper_without_making_her_mute() {
        let mut r = rig();
        r.voice.set_tier(Tier::Reduced, &TierReason::HeavyProcess { name: "cargo".into() });
        assert!(r.voice.policy().speak);
        assert!(r.voice.current_pack().is_some());
        assert!(!r.voice.policy().dgpu);
        assert_eq!(r.voice.speaker.config().fps, Tier::Reduced.target_fps());
        r.voice.say("Still talking, just cheaper.", Mood::Neutral).unwrap();
        r.run(400);
        assert!(r.sink.written_ms() > 0, "T2 went silent, which it must not");
    }

    #[test]
    fn the_expensive_voice_is_taken_away_at_t2_and_a_cheaper_one_used() {
        let mut r = rig();
        r.voice.voices_mut().select("wisp-fine").unwrap();
        assert_eq!(r.voice.current_pack().unwrap().id, "wisp-fine");
        r.voice.set_tier(Tier::Reduced, &TierReason::GpuPressure { busy_pct: 90 });
        let now = r.voice.current_pack().unwrap();
        assert_ne!(now.id, "wisp-fine", "Kokoro must not survive T2");
        assert!(now.usable_at(Tier::Reduced));
    }

    // -- the microphone ----------------------------------------------------

    fn listen(r: &mut Rig) -> crate::mic::PermitLog {
        let permit = FakePermit::new();
        let log = permit.log();
        r.voice
            .listen(permit, Box::new(FakeMic::new(16_000)), ListenConfig::default())
            .expect("T1 permits listening");
        log
    }

    #[test]
    fn t3_switches_the_microphone_off_and_the_tell_goes_down_with_it() {
        // The headline rule: she is not listening while the operator is in a
        // headset. Dropping the listener drops the permit, and for the real
        // permit that is what lowers the visible tell of SPEC §0.3.
        let mut r = rig();
        let log = listen(&mut r);
        assert!(r.voice.is_listening());
        assert!(r.voice.tell(r.now).active, "the tell must be up while the mic is open");
        assert_eq!(log.drops(), 0);

        r.voice.set_tier(Tier::Lobotomised, &TierReason::VrSession);

        assert!(!r.voice.is_listening(), "T3 left the microphone open");
        assert_eq!(log.drops(), 1, "the permit must be dropped exactly once");
        assert!(!r.voice.tell(r.now + 10).active, "the tell outlived the microphone");
        assert!(r.voice.policy().speak, "…but canned speech survives T3");
    }

    #[test]
    fn the_microphone_cannot_even_be_opened_at_t3() {
        let mut r = rig();
        r.voice.set_tier(Tier::Lobotomised, &TierReason::VrSession);
        let err = r
            .voice
            .listen(FakePermit::new(), Box::new(FakeMic::new(16_000)), ListenConfig::default())
            .unwrap_err();
        assert!(matches!(err, VoiceError::Tier { tier: Tier::Lobotomised, .. }), "{err:?}");
        assert!(!r.voice.is_listening());
    }

    #[test]
    fn the_wake_word_is_dropped_at_t2_but_push_to_talk_survives() {
        let mut r = rig();
        r.voice.set_tier(Tier::Reduced, &TierReason::HeavyProcess { name: "blender".into() });
        assert!(r.voice.policy().listen, "push-to-talk must survive T2");
        assert!(!r.voice.policy().wake_word, "a continuously open mic must not");
        let cfg = ListenConfig {
            wake: Some(crate::mic::WakeConfig::default()),
            ..ListenConfig::default()
        };
        r.voice
            .listen(FakePermit::new(), Box::new(FakeMic::new(16_000)), cfg)
            .expect("T2 still allows push-to-talk");
        assert!(r.voice.is_listening());
    }

    #[test]
    fn revoking_consent_mid_tick_closes_the_microphone_and_lowers_the_tell() {
        let mut r = rig();
        let permit = FakePermit::new();
        let log = permit.log();
        r.voice
            .listen(permit, Box::new(FakeMic::new(16_000)), ListenConfig::default())
            .unwrap();
        r.voice.ptt_down(r.now);
        r.voice.feed_mic(&vec![0.4f32; 1600], r.now);

        log.revoke();
        let mut stt = FakeStt::saying("hello there", 1_000);
        let mut tts = FakeTts::at_rate(RATE);
        let t = r.voice.tick(&mut tts, Some(&mut stt), r.now + 100);

        assert!(t.heard.is_empty(), "a revoked microphone published: {:?}", t.heard);
        assert!(!r.voice.is_listening());
        assert!(!r.voice.tell(r.now + 200).active);
    }

    #[test]
    fn everything_she_hears_arrives_as_a_speech_observation_and_nothing_else() {
        let mut r = rig();
        let log = listen(&mut r);
        r.voice.ptt_down(r.now);
        let mut stt = FakeStt::saying("open the rig editor", 1_200);
        let mut heard = Vec::new();
        for _ in 0..40 {
            r.now += 100;
            r.voice.feed_mic(&vec![0.35f32; 1600], r.now);
            let mut tts = FakeTts::at_rate(RATE);
            heard.extend(r.voice.tick(&mut tts, Some(&mut stt), r.now).heard);
        }
        r.voice.ptt_up(r.now);
        let mut tts = FakeTts::at_rate(RATE);
        heard.extend(r.voice.tick(&mut tts, Some(&mut stt), r.now + 100).heard);

        assert!(!heard.is_empty(), "nothing was transcribed");
        for o in &heard {
            assert!(
                matches!(o, Observation::Speech { .. }),
                "the microphone published {o:?}, which is not speech"
            );
        }
        let finals = log.speech().iter().filter(|(_, f)| *f).count();
        assert_eq!(finals, 1, "exactly one final per utterance");
        assert!(!log.partials().is_empty(), "she must react before you finish");
    }

    #[test]
    fn the_tell_is_up_exactly_while_the_microphone_is_open() {
        let mut r = rig();
        assert!(!r.voice.tell(0).active);
        listen(&mut r);
        assert!(r.voice.tell(10).active);
        r.voice.stop_listening();
        assert!(!r.voice.tell(20).active);
    }

    // -- lip-sync ----------------------------------------------------------

    #[test]
    fn her_mouth_follows_the_audio_that_has_actually_left_the_sink() {
        let mut r = rig();
        assert_eq!(r.voice.drive().openness, 0.0, "a closed mouth when silent");
        r.voice.say("aaaaaaaaaaaa.", Mood::Neutral).unwrap();
        r.step(0);
        let mut widest = 0.0f32;
        for _ in 0..40 {
            r.step(25);
            widest = widest.max(r.voice.drive().openness);
        }
        assert!(widest > 0.2, "her mouth never opened: {widest}");
        assert!(r.voice.drive().openness.is_finite());
    }

    // -- crash recovery ----------------------------------------------------

    #[test]
    fn a_journal_from_a_killed_run_is_paid_off_at_startup() {
        let mut r = rig();
        let before = r.volumes();
        r.voice.say("Killed halfway through this sentence.", Mood::Neutral).unwrap();
        r.step(0);
        assert_ne!(r.volumes(), before, "nothing was ducked, so nothing can be recovered");

        // SIGKILL: no Drop, no release. The damage and the journal both persist.
        let Rig { voice, audio, firefox, spotify, journal, .. } = r;
        std::mem::forget(voice);
        let vols = || {
            (
                audio.volume_of(&firefox).unwrap(),
                audio.volume_of(&spotify).unwrap(),
            )
        };
        assert_ne!(vols(), before, "the killed run left no damage to undo");

        // The next start, over the same journal and the same audio server —
        // PipeWire does not die when she does.
        let mut next = Voice::builder()
            .duck(DuckConfig { fade_ms: 0, ..DuckConfig::default() })
            .journal_in(&journal)
            .build(Box::new(SharedSink::new(RATE)), Box::new(audio.clone()));
        let rec = next.recover_ducking().expect("recovery");
        assert!(rec.found, "no journal was left behind: {rec:?}");
        assert!(rec.restore.restored > 0, "{rec:?}");
        assert_eq!(vols(), before, "the operator's music was not restored");
    }

    #[test]
    fn recovery_with_no_journal_is_a_clean_no_op() {
        let mut r = rig();
        let before = r.volumes();
        let rec = r.voice.recover_ducking().expect("recovery must not fail without a journal");
        assert!(!rec.found);
        assert_eq!(rec.restore.restored, 0);
        assert_eq!(r.volumes(), before);
    }
}
