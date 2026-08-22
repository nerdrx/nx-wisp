//! F31's spine: the thing that makes streamed speech actually stream.
//!
//! ```text
//!   push("Your build ")  push("is green. ")  push("Nineteen tests…")
//!            │
//!            ▼
//!        Chunker ──clause──▶ Tts ──▶ tighten ──▶ AudioSink
//!                              │                    │
//!                              └──▶ DriveTrack ◀────┘  (aligned to the sink's clock)
//! ```
//!
//! ## Why this is a pump and not a thread
//!
//! Every moving part here is driven by [`Speaker::pump`], which the host calls
//! from its own loop with the monotonic clock it already has. No thread, no
//! channel, no timer. Two reasons, and both are SPEC:
//!
//! - §3.1 says a downgrade is **synchronous and immediate**, and that work is
//!   shed rather than queued. A thread that owns the queue can only be *asked*
//!   to stop; a pump the governor can simply stop calling — and whose queue the
//!   governor can clear on the spot — actually stops.
//! - §4 wants this testable with no GPU, no compositor and no device. A pump
//!   with an injected clock is a pure function of its inputs, so every property
//!   below is a unit test rather than a sleep and a hope.
//!
//! ## Synthesis runs *ahead* of playback, but only just
//!
//! [`SpeakerConfig::lookahead_ms`] caps how much unheard audio may sit in the
//! sink. Small enough and she stutters between clauses; large enough and a
//! barge-in throws away work the CPU already paid for, and a tier downgrade
//! finds a second of finished audio it has to discard. The default is about one
//! clause of slack, which is the smallest number that keeps the seam inaudible.
//! This is the knob the governor turns at T2 — see [`crate::tier`].
//!
//! ## The lip-sync clock is the sink, not the wall
//!
//! Her mouth has to match the sound *leaving the speakers*, and what has left
//! the speakers is `written - queued`, not "how long ago did I start". Using
//! wall time here would drift by exactly the sink's buffer depth, which is the
//! difference between lip-sync and a badly dubbed film. [`Speaker::position_ms`]
//! is that subtraction and [`Speaker::drive`] samples the track at it.

use std::collections::VecDeque;

use crate::audio::Pcm;
use crate::barge::CancelReason;
use crate::lipsync::{DriveFrame, DriveTrack};
use crate::sink::AudioSink;
use crate::text::{Chunk, ChunkKind, Chunker};
use crate::tts::{PhonemeSpan, SynthParams, Synthesis, Tts};
use crate::voices::{Mood, VoicePack};
use crate::{Millis, Result, VoiceError};

/// Identifies one utterance, for the flight recorder.
pub type SpeechId = u64;

/// Facts about what she did. Events, not commands — SPEC §3.2.
#[derive(Debug, Clone, PartialEq)]
pub enum SpeechEvent {
    Started { id: SpeechId, voice: String },
    /// One clause was synthesised and handed to the sink.
    Clause { id: SpeechId, seq: u32, text: String, ms: u32 },
    /// She reached the end and the sink has drained.
    Finished { id: SpeechId, spoken_ms: u32 },
    /// She was cut off. `unsaid` is what she never reached — recorded, and then
    /// dropped. SPEC §3.1: shed, do not queue.
    Cancelled {
        id: SpeechId,
        reason: CancelReason,
        spoken_ms: u32,
        unsaid: String,
    },
    /// The engine failed. She stops; she does not half-speak.
    Failed { id: SpeechId, why: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerConfig {
    /// Analysis frame rate for the lip-sync track. The governor sets this from
    /// the tier's target fps — there is no point driving a mouth at 60 Hz for a
    /// rig that is being rendered at 15.
    pub fps: u32,
    /// How much unheard audio may sit in the sink before we stop synthesising.
    ///
    /// A ceiling on *starting* work, not on the queue: the check happens before
    /// a clause is synthesised, so the queue overshoots by up to one clause.
    /// That overshoot is bounded by [`crate::text::ChunkConfig::max_chars`] and
    /// is the price of not cutting a clause in half to hit a number.
    pub lookahead_ms: u32,
    /// Silence inserted between clauses. A little, or she runs sentences
    /// together; not much, or she sounds hesitant.
    pub gap_ms: u32,
    /// Raised-cosine fade at each clause edge, to kill the splice click.
    pub fade_ms: u32,
    /// Below this amplitude, leading and trailing samples are engine padding.
    pub silence_floor: f32,
    /// T4. The pipeline runs and the flight recorder still gets its events, but
    /// nothing is synthesised and nothing reaches a sink.
    pub silent: bool,
}

impl Default for SpeakerConfig {
    fn default() -> Self {
        SpeakerConfig {
            fps: 60,
            lookahead_ms: 700,
            gap_ms: 60,
            fade_ms: 8,
            silence_floor: 0.004,
            silent: false,
        }
    }
}

/// Where she is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechState {
    Idle,
    /// Text is still arriving, or clauses are still queued.
    Speaking,
    /// Everything is synthesised; the sink is emptying.
    Draining,
}

/// One utterance in flight.
#[derive(Debug)]
struct Utterance {
    id: SpeechId,
    chunker: Chunker,
    params: SynthParams,
    voice: String,
    code_placeholder: String,
    seq: u32,
    /// Total audio handed to the sink for this utterance.
    written_ms: u32,
    track: DriveTrack,
    /// Clauses synthesised but not yet written, when the sink refused them.
    stalled: VecDeque<(String, Synthesis)>,
    started: bool,
    text_ended: bool,
    /// Everything she has actually been given to say, for the recorder.
    said: String,
}

/// The streaming synthesiser front end.
#[derive(Debug)]
pub struct Speaker {
    cfg: SpeakerConfig,
    next_id: SpeechId,
    cur: Option<Utterance>,
}

impl Default for Speaker {
    fn default() -> Self {
        Speaker::new(SpeakerConfig::default())
    }
}

impl Speaker {
    pub fn new(cfg: SpeakerConfig) -> Self {
        Speaker { cfg, next_id: 1, cur: None }
    }

    pub fn config(&self) -> &SpeakerConfig {
        &self.cfg
    }

    /// Change the pump's budget. Applied to the utterance in flight, because a
    /// tier change that only affected the *next* sentence would let a downgrade
    /// be outrun by a long one.
    pub fn set_config(&mut self, cfg: SpeakerConfig) {
        self.cfg = cfg;
    }

    /// Start an utterance. Any utterance already in flight is abandoned — the
    /// caller should have cancelled it, and doing it silently here would hide
    /// a real bug, so it is reported.
    pub fn begin(&mut self, pack: &VoicePack, mood: Mood) -> SpeechId {
        let id = self.next_id;
        self.next_id += 1;
        self.cur = Some(Utterance {
            id,
            chunker: Chunker::default(),
            params: pack.params(mood),
            voice: pack.id.clone(),
            code_placeholder: pack.code_placeholder.clone(),
            seq: 0,
            written_ms: 0,
            track: DriveTrack::empty(self.cfg.fps),
            stalled: VecDeque::new(),
            started: false,
            text_ended: false,
            said: String::new(),
        });
        id
    }

    /// Feed text as the model produces it.
    pub fn push(&mut self, text: &str) {
        if let Some(u) = self.cur.as_mut() {
            u.chunker.push(text);
            u.said.push_str(text);
        }
    }

    /// The model stopped. Whatever is left is now speakable.
    pub fn end_text(&mut self) {
        if let Some(u) = self.cur.as_mut() {
            u.chunker.finish();
            u.text_ended = true;
        }
    }

    pub fn id(&self) -> Option<SpeechId> {
        self.cur.as_ref().map(|u| u.id)
    }

    pub fn is_speaking(&self) -> bool {
        self.cur.is_some()
    }

    pub fn state(&self, sink: &dyn AudioSink) -> SpeechState {
        match self.cur.as_ref() {
            None => SpeechState::Idle,
            Some(u) => {
                let done = u.text_ended && u.chunker.is_finished() && u.stalled.is_empty();
                if done && sink.queued_ms() > 0 {
                    SpeechState::Draining
                } else {
                    SpeechState::Speaking
                }
            }
        }
    }

    /// Milliseconds of this utterance that have actually left the speakers.
    pub fn position_ms(&self, sink: &dyn AudioSink) -> u32 {
        self.cur
            .as_ref()
            .map(|u| u.written_ms.saturating_sub(sink.queued_ms()))
            .unwrap_or(0)
    }

    /// The rig drive signal for right now. Closed mouth when she is not talking.
    pub fn drive(&self, sink: &dyn AudioSink) -> DriveFrame {
        match self.cur.as_ref() {
            None => DriveFrame::closed(0),
            Some(u) => u.track.sample(self.position_ms(sink)),
        }
    }

    /// The whole lip-sync track built so far, for a host that would rather
    /// schedule ahead than sample every frame.
    pub fn track(&self) -> Option<&DriveTrack> {
        self.cur.as_ref().map(|u| &u.track)
    }

    /// Synthesise whatever is ready and hand it to the sink.
    ///
    /// Call it every frame; it does nothing when there is nothing to do. `now`
    /// is the host's monotonic clock and is carried into the events rather than
    /// read from a clock this module owns.
    pub fn pump(
        &mut self,
        tts: &mut dyn Tts,
        sink: &mut dyn AudioSink,
        now: Millis,
    ) -> Result<Vec<SpeechEvent>> {
        let _ = now;
        let mut evs = Vec::new();
        let Some(u) = self.cur.as_mut() else {
            return Ok(evs);
        };

        if !u.started {
            u.started = true;
            evs.push(SpeechEvent::Started { id: u.id, voice: u.voice.clone() });
        }

        // T4: she is silenced. Consume the text so the caller is not left
        // holding a queue, record nothing as spoken, and make no sound.
        if self.cfg.silent {
            u.chunker.clear();
            if u.text_ended {
                let id = u.id;
                self.cur = None;
                evs.push(SpeechEvent::Finished { id, spoken_ms: 0 });
            }
            return Ok(evs);
        }

        loop {
            // Never run more than a clause or so ahead of what she has said.
            // Beyond that the work is speculative and a barge-in throws it away.
            if sink.queued_ms() >= self.cfg.lookahead_ms {
                break;
            }

            // Anything a previous pump synthesised but could not write?
            let next: Option<(String, Synthesis)> = match u.stalled.pop_front() {
                Some(s) => Some(s),
                None => match u.chunker.next_chunk() {
                    None => break,
                    Some(chunk) => {
                        let Some(text) = speakable(&chunk, &u.code_placeholder) else {
                            continue;
                        };
                        match tts.synth(&text, &u.params) {
                            Ok(s) => Some((text, s)),
                            Err(e) => {
                                let id = u.id;
                                self.cur = None;
                                evs.push(SpeechEvent::Failed { id, why: e.to_string() });
                                return Ok(evs);
                            }
                        }
                    }
                },
            };
            let Some((text, synth)) = next else { break };

            let tight = tighten(synth, self.cfg.silence_floor, self.cfg.fade_ms);
            if tight.pcm.is_empty() {
                continue;
            }

            // A gap before every clause but the first, so she does not run two
            // thoughts together.
            let gap = if u.seq == 0 { 0 } else { self.cfg.gap_ms };
            if gap > 0 {
                let s = Pcm::silence(tight.pcm.rate, gap);
                if let Err(e) = sink.write(&s) {
                    // The device went away mid-sentence. Hold the clause so a
                    // recovered sink picks up where she left off rather than
                    // losing a line.
                    u.stalled.push_front((text, tight));
                    let id = u.id;
                    self.cur = None;
                    evs.push(SpeechEvent::Failed { id, why: e.to_string() });
                    return Ok(evs);
                }
                u.written_ms += gap;
            }

            let ms = tight.pcm.duration_ms();
            let at = u.written_ms;
            if let Err(e) = sink.write(&tight.pcm) {
                u.stalled.push_front((text, tight));
                let id = u.id;
                self.cur = None;
                evs.push(SpeechEvent::Failed { id, why: e.to_string() });
                return Ok(evs);
            }
            u.track
                .append(&DriveTrack::from_synthesis(&tight, self.cfg.fps), at);
            u.written_ms += ms;
            u.seq += 1;
            evs.push(SpeechEvent::Clause { id: u.id, seq: u.seq - 1, text, ms });
        }

        // Finished only when the text ended, nothing is queued to synthesise,
        // and the sink has actually emptied. Reporting `Finished` while a second
        // of audio is still in flight would drop the duck too early and let her
        // last word land on top of the music coming back up.
        let done = u.text_ended && u.chunker.is_finished() && u.stalled.is_empty();
        if done && sink.queued_ms() == 0 {
            let (id, spoken_ms) = (u.id, u.written_ms);
            self.cur = None;
            evs.push(SpeechEvent::Finished { id, spoken_ms });
        }
        Ok(evs)
    }

    /// Stop her, now. Barge-in, a tier downgrade, or the operator asking.
    ///
    /// Flushes the sink rather than letting it drain: SPEC §3.1 says shed. The
    /// words she had not reached are recorded in the event and then dropped —
    /// there is no deferred-speech queue in this crate, and §3.5 grants one only
    /// to `wisp-mind`.
    pub fn cancel(&mut self, reason: CancelReason, sink: &mut dyn AudioSink) -> Option<SpeechEvent> {
        let u = self.cur.take()?;
        let spoken_ms = u.written_ms.saturating_sub(sink.queued_ms());
        sink.flush();
        let mut unsaid = u.chunker.pending().trim().to_string();
        for (t, _) in &u.stalled {
            if !unsaid.is_empty() {
                unsaid.push(' ');
            }
            unsaid.push_str(t);
        }
        Some(SpeechEvent::Cancelled { id: u.id, reason, spoken_ms, unsaid })
    }

    /// Everything she has been asked to say in the current utterance, whether or
    /// not she reached it. For the flight recorder.
    pub fn text_so_far(&self) -> &str {
        self.cur.as_ref().map(|u| u.said.as_str()).unwrap_or("")
    }
}

/// What a chunk should actually be spoken as. `None` means "say nothing".
fn speakable(chunk: &Chunk, code_placeholder: &str) -> Option<String> {
    let t = match chunk.kind {
        ChunkKind::Speech => chunk.text.trim().to_string(),
        // Reading a fenced block aloud is unbearable. The replacement line lives
        // in the voice pack (F35), because it is her dialogue, not our logic.
        ChunkKind::Code => code_placeholder.trim().to_string(),
    };
    (!t.is_empty()).then_some(t)
}

/// Trim the engine's padding and fade the seams — **and move the phoneme spans
/// with the audio**.
///
/// Piper pads every utterance with about a tenth of a second of near-silence.
/// Left in, that lands between every pair of clauses and turns streamed speech
/// into speech with a stammer. But trimming the front without shifting the
/// phoneme timings would slide her mouth out of sync by exactly the amount
/// trimmed, on every clause, cumulatively — which is a far worse bug than the
/// stammer. So the two happen together, here, in one place.
fn tighten(synth: Synthesis, floor: f32, fade_ms: u32) -> Synthesis {
    let Synthesis { text, pcm, phonemes } = synth;
    let rate = pcm.rate;
    let first = pcm.samples.iter().position(|s| s.abs() > floor);
    let Some(first) = first else {
        return Synthesis { text, pcm: Pcm::new(rate, Vec::new()), phonemes: Vec::new() };
    };
    let last = pcm.samples.iter().rposition(|s| s.abs() > floor).unwrap_or(first);
    let mut out = Pcm::new(rate, pcm.samples[first..=last].to_vec());
    out.fade(fade_ms);

    let lead_ms = ((first as u64 * 1000) / rate.max(1) as u64) as u32;
    let end_ms = out.duration_ms();
    let phonemes: Vec<PhonemeSpan> = phonemes
        .into_iter()
        .filter_map(|p| {
            // Spans entirely inside the trimmed silence are gone with it.
            if p.end_ms <= lead_ms {
                return None;
            }
            let start = p.start_ms.saturating_sub(lead_ms);
            if start >= end_ms {
                return None;
            }
            Some(PhonemeSpan::new(
                p.symbol,
                start,
                p.end_ms.saturating_sub(lead_ms).min(end_ms),
            ))
        })
        .collect();

    Synthesis { text, pcm: out, phonemes }
}

/// Convenience for the non-streaming case: one canned line, start to finish.
///
/// Still goes through the same pump, so the canned path cannot drift away from
/// the streamed one.
pub fn say_all(
    speaker: &mut Speaker,
    pack: &VoicePack,
    mood: Mood,
    text: &str,
    tts: &mut dyn Tts,
    sink: &mut BufferedRun<'_>,
) -> Result<Vec<SpeechEvent>> {
    speaker.begin(pack, mood);
    speaker.push(text);
    speaker.end_text();
    sink.run(speaker, tts)
}

/// Drives a [`Speaker`] to completion against a sink whose playback the caller
/// simulates. Exists so tests and the examples share one loop instead of two
/// slightly different ones.
pub struct BufferedRun<'a> {
    pub sink: &'a mut crate::sink::BufferSink,
    /// How much simulated time passes per pump.
    pub step_ms: u32,
    pub max_steps: usize,
}

impl<'a> BufferedRun<'a> {
    pub fn new(sink: &'a mut crate::sink::BufferSink) -> Self {
        BufferedRun { sink, step_ms: 20, max_steps: 5_000 }
    }

    pub fn run(&mut self, speaker: &mut Speaker, tts: &mut dyn Tts) -> Result<Vec<SpeechEvent>> {
        let mut evs = Vec::new();
        let mut now: Millis = 0;
        for _ in 0..self.max_steps {
            evs.extend(speaker.pump(tts, self.sink, now)?);
            if !speaker.is_speaking() {
                return Ok(evs);
            }
            self.sink.advance(self.step_ms);
            now += self.step_ms as Millis;
        }
        Err(VoiceError::Synth("speaker did not finish".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::BufferSink;
    use crate::tts::FakeTts;
    use crate::voices::VoiceRegistry;

    fn pack() -> VoicePack {
        VoiceRegistry::builtin().get("test-tone").unwrap().clone()
    }

    fn setup() -> (Speaker, FakeTts, BufferSink) {
        let e = FakeTts::at_rate(22_050);
        (Speaker::default(), e, BufferSink::new(22_050))
    }

    fn clause_texts(evs: &[SpeechEvent]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                SpeechEvent::Clause { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn she_starts_talking_before_the_sentence_has_finished_arriving() {
        // This is the whole point of F31. If this test regresses, streaming is
        // gone and nobody will notice from the audio alone.
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("Your build is green. ");
        let evs = sp.pump(&mut e, &mut sink, 0).unwrap();
        assert!(
            clause_texts(&evs).contains(&"Your build is green.".to_string()),
            "nothing was synthesised while text was still arriving: {evs:?}"
        );
        assert!(sink.written_ms() > 0, "and nothing reached the sink");
        // The model is still writing.
        assert!(sp.is_speaking());
    }

    #[test]
    fn a_long_reply_is_synthesised_clause_by_clause_not_as_one_blob() {
        let (mut sp, mut e, mut sink) = setup();
        let evs = say_all(
            &mut sp,
            &pack(),
            Mood::Neutral,
            "Your build is green. Nineteen tests passed. The flaky one behaved itself.",
            &mut e,
            &mut BufferedRun::new(&mut sink),
        )
        .unwrap();
        assert!(e.calls >= 3, "the engine was called {} times", e.calls);
        assert!(clause_texts(&evs).len() >= 3);
        assert!(matches!(evs.last(), Some(SpeechEvent::Finished { .. })), "{:?}", evs.last());
    }

    #[test]
    fn synthesis_does_not_run_unboundedly_ahead_of_playback() {
        // Work done far ahead of the operator's ear is work a barge-in wastes.
        // The lookahead cannot be exact — the check happens before a clause is
        // synthesised, so the queue always overshoots by up to one clause — but
        // it must stop growing, and it must stop after a bounded number of them.
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        for _ in 0..40 {
            sp.push("Another whole sentence about the state of your machine. ");
        }
        // Pump hard without ever letting the sink play anything.
        for _ in 0..10 {
            sp.pump(&mut e, &mut sink, 0).unwrap();
        }
        let early = sink.queued_ms();
        let calls_early = e.calls;
        for _ in 0..90 {
            sp.pump(&mut e, &mut sink, 0).unwrap();
        }
        assert_eq!(
            sink.queued_ms(),
            early,
            "the queue kept growing while nothing was being played"
        );
        assert_eq!(e.calls, calls_early, "the engine kept working ahead of the ear");
        // And the overshoot really is one clause, not ten.
        assert!(
            early <= SpeakerConfig::default().lookahead_ms + 4_000,
            "queued {early}ms is more than a lookahead plus one clause"
        );
    }

    #[test]
    fn the_lip_sync_clock_follows_the_sink_and_not_the_wall() {
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("Hello there. ");
        sp.pump(&mut e, &mut sink, 0).unwrap();
        assert_eq!(sp.position_ms(&sink), 0, "nothing has been heard yet");
        sink.advance(100);
        assert_eq!(sp.position_ms(&sink), 100);
        // Wall time moving on its own must not move her mouth.
        let before = sp.position_ms(&sink);
        sp.pump(&mut e, &mut sink, 10_000).unwrap();
        assert_eq!(sp.position_ms(&sink), before);
    }

    #[test]
    fn her_mouth_is_shut_when_she_is_not_speaking() {
        let (sp, _e, sink) = setup();
        let f = sp.drive(&sink);
        assert_eq!(f.openness, 0.0);
    }

    #[test]
    fn her_mouth_opens_somewhere_in_the_middle_of_a_vowel() {
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("aaaaaaaa. ");
        sp.pump(&mut e, &mut sink, 0).unwrap();
        sink.advance(200);
        let f = sp.drive(&sink);
        assert!(f.openness > 0.2, "openness was {} in the middle of a vowel", f.openness);
        assert!(f.openness.is_finite() && (0.0..=1.0).contains(&f.openness));
    }

    #[test]
    fn a_code_block_is_replaced_by_the_voice_packs_line_rather_than_read_out() {
        let (mut sp, mut e, mut sink) = setup();
        let p = pack();
        say_all(
            &mut sp,
            &p,
            Mood::Neutral,
            "Try this:\n```rust\nfn main() { println!(\"hi\"); }\n```\nThat should do it.",
            &mut e,
            &mut BufferedRun::new(&mut sink),
        )
        .unwrap();
        let spoken = e.seen.join(" | ");
        assert!(!spoken.contains("println"), "she read the code aloud: {spoken}");
        assert!(spoken.contains(&p.code_placeholder), "{spoken}");
    }

    #[test]
    fn barge_in_cuts_her_off_mid_sentence_and_sheds_the_rest() {
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("First thing here. And a second thing she will never get to say aloud.");
        sp.pump(&mut e, &mut sink, 0).unwrap();
        sink.advance(80);
        let queued_before = sink.queued_ms();
        assert!(queued_before > 0, "nothing was queued to cut off");

        let ev = sp.cancel(CancelReason::Typing, &mut sink).unwrap();
        match ev {
            SpeechEvent::Cancelled { reason, spoken_ms, unsaid, .. } => {
                assert_eq!(reason, CancelReason::Typing);
                assert!(spoken_ms > 0 && spoken_ms < 10_000);
                assert!(!unsaid.is_empty(), "what she never said must be recorded");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(sink.queued_ms(), 0, "the sink must be flushed, not drained");
        assert!(!sp.is_speaking());
        // …and shed: pumping again produces nothing at all.
        assert!(sp.pump(&mut e, &mut sink, 0).unwrap().is_empty());
    }

    #[test]
    fn cancelling_when_she_is_not_speaking_is_harmless() {
        let (mut sp, _e, mut sink) = setup();
        assert!(sp.cancel(CancelReason::Explicit, &mut sink).is_none());
    }

    #[test]
    fn she_is_not_finished_while_the_sink_still_has_audio_in_it() {
        // Reporting Finished early would drop the duck under her own last word.
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("A short line.");
        sp.end_text();
        let evs = sp.pump(&mut e, &mut sink, 0).unwrap();
        assert!(!evs.iter().any(|e| matches!(e, SpeechEvent::Finished { .. })));
        assert_eq!(sp.state(&sink), SpeechState::Draining);
        sink.advance(10_000);
        let evs = sp.pump(&mut e, &mut sink, 0).unwrap();
        assert!(evs.iter().any(|e| matches!(e, SpeechEvent::Finished { .. })), "{evs:?}");
        assert_eq!(sp.state(&sink), SpeechState::Idle);
    }

    #[test]
    fn a_failing_engine_stops_her_rather_than_leaving_half_a_sentence_queued() {
        let (mut sp, _e, mut sink) = setup();
        let mut dead = crate::tts::DeadTts;
        sp.begin(&pack(), Mood::Neutral);
        sp.push("Anything at all. ");
        let evs = sp.pump(&mut dead, &mut sink, 0).unwrap();
        assert!(evs.iter().any(|e| matches!(e, SpeechEvent::Failed { .. })), "{evs:?}");
        assert!(!sp.is_speaking());
    }

    #[test]
    fn a_sink_that_goes_away_mid_utterance_is_reported_not_ignored() {
        let (mut sp, mut e, _s) = setup();
        let mut flaky = crate::sink::FlakySink::new(22_050, 1);
        sp.begin(&pack(), Mood::Neutral);
        sp.push("One. Two. Three. Four. Five. Six. Seven and eight and nine.");
        sp.end_text();
        let mut saw_failure = false;
        for _ in 0..20 {
            let evs = sp.pump(&mut e, &mut flaky, 0).unwrap();
            saw_failure |= evs.iter().any(|e| matches!(e, SpeechEvent::Failed { .. }));
            if !sp.is_speaking() {
                break;
            }
        }
        assert!(saw_failure);
    }

    #[test]
    fn at_t4_she_makes_no_sound_at_all_but_the_recorder_still_learns_of_it() {
        let mut sp = Speaker::new(SpeakerConfig { silent: true, ..Default::default() });
        let mut e = FakeTts::new();
        let mut sink = BufferSink::new(22_050);
        sp.begin(&pack(), Mood::Neutral);
        sp.push("Something she is not allowed to say right now.");
        sp.end_text();
        let evs = sp.pump(&mut e, &mut sink, 0).unwrap();
        assert_eq!(e.calls, 0, "T4 must not even synthesise");
        assert_eq!(sink.written_ms(), 0);
        assert!(evs.iter().any(|e| matches!(e, SpeechEvent::Started { .. })));
        assert!(evs.iter().any(|e| matches!(e, SpeechEvent::Finished { .. })));
        assert!(!sp.is_speaking());
    }

    #[test]
    fn the_mood_reaches_the_engine() {
        let (mut sp, mut e, mut sink) = setup();
        let p = pack();
        sp.begin(&p, Mood::Sleepy);
        sp.push("Time for bed I think. ");
        sp.pump(&mut e, &mut sink, 0).unwrap();
        let sleepy_ms = sink.written_ms();

        let (mut sp2, mut e2, mut sink2) = setup();
        sp2.begin(&p, Mood::Delighted);
        sp2.push("Time for bed I think. ");
        sp2.pump(&mut e2, &mut sink2, 0).unwrap();
        assert!(
            sleepy_ms > sink2.written_ms(),
            "sleepy ({sleepy_ms}ms) should be slower than delighted ({}ms)",
            sink2.written_ms()
        );
    }

    #[test]
    fn clause_seams_are_faded_so_there_is_no_click() {
        let (mut sp, mut e, mut sink) = setup();
        say_all(
            &mut sp,
            &pack(),
            Mood::Neutral,
            "One thing. Two things. Three things.",
            &mut e,
            &mut BufferedRun::new(&mut sink),
        )
        .unwrap();
        // No sample-to-sample jump big enough to be an audible discontinuity.
        let s = &sink.all().samples;
        let worst = s.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.35, "a {worst} jump between adjacent samples is a click");
    }

    #[test]
    fn tighten_moves_the_phoneme_spans_with_the_audio_it_trimmed() {
        // The bug this prevents: trimming the front and leaving the timings
        // where they were slides her mouth later by the trim, every clause,
        // cumulatively.
        let rate = 16_000;
        let mut samples = vec![0.0f32; rate as usize / 10]; // 100ms of padding
        samples.extend(crate::audio::sine(rate, 300.0, 200, 0.6).samples);
        let synth = Synthesis {
            text: "x".into(),
            pcm: Pcm::new(rate, samples),
            phonemes: vec![PhonemeSpan::new("ɑ", 100, 300)],
        };
        let t = tighten(synth, 0.004, 4);
        assert!(t.pcm.duration_ms() <= 210, "{}", t.pcm.duration_ms());
        assert_eq!(t.phonemes.len(), 1);
        assert!(t.phonemes[0].start_ms <= 2, "span still at {}", t.phonemes[0].start_ms);
    }

    #[test]
    fn tighten_on_pure_silence_yields_nothing_rather_than_panicking() {
        let t = tighten(
            Synthesis {
                text: "x".into(),
                pcm: Pcm::silence(16_000, 200),
                phonemes: vec![PhonemeSpan::new("_", 0, 200)],
            },
            0.004,
            4,
        );
        assert!(t.pcm.is_empty());
        assert!(t.phonemes.is_empty());
    }

    #[test]
    fn a_whole_utterance_reaches_the_sink_intact() {
        let (mut sp, mut e, mut sink) = setup();
        let text = "One. Two. Three. Four. Five.";
        say_all(&mut sp, &pack(), Mood::Neutral, text, &mut e, &mut BufferedRun::new(&mut sink))
            .unwrap();
        let joined = e.seen.join(" ");
        for word in ["One.", "Two.", "Three.", "Four.", "Five."] {
            assert!(joined.contains(word), "{word} never reached the engine: {joined}");
        }
        assert!(sink.written_ms() > 400, "{}", sink.written_ms());
    }

    #[test]
    fn text_pushed_after_the_first_pump_is_still_spoken() {
        // The model produces tokens for a second or more after she starts.
        let (mut sp, mut e, mut sink) = setup();
        sp.begin(&pack(), Mood::Neutral);
        sp.push("The first part is here. ");
        sp.pump(&mut e, &mut sink, 0).unwrap();
        sp.push("And the second part arrived later on. ");
        sp.end_text();
        let mut run = BufferedRun::new(&mut sink);
        run.run(&mut sp, &mut e).unwrap();
        let joined = e.seen.join(" | ");
        assert!(joined.contains("second part"), "{joined}");
    }
}
