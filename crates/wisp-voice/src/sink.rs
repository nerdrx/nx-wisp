//! Where the audio goes.
//!
//! The trait exists for one reason above all others: **the test suite must never
//! make a sound.** The operator is at this machine while the suite runs, and a
//! crate that plays audio out of their speakers to prove that it can play audio
//! out of their speakers has misunderstood the assignment. Everything in the
//! suite writes into [`BufferSink`] and asserts on samples.
//!
//! ## Why a sink has to be able to throw audio away
//!
//! Barge-in (F33) is the reason. When the operator starts typing, she has to
//! stop *now* — not at the end of the buffer. A sink that can only be closed
//! gives you a companion who keeps talking for another second and a half after
//! you asked her to stop, which reads as her ignoring you. So [`AudioSink`] has
//! [`AudioSink::flush`], every implementation must make it immediate, and
//! [`AudioSink::queued_ms`] exists so the caller can tell how much unspoken
//! audio it is about to discard — and so [`crate::speaker`] knows where in the
//! utterance she actually *is*, which is what the lip-sync clock runs on.
//!
//! ## On the real backend
//!
//! [`PwCatSink`] shells out to `pw-cat`, which is part of PipeWire and is on
//! the target machine by definition (SPEC §1 makes PipeWire a hard dependency).
//! It is honest, small, and it works — but it buys that simplicity with a pipe
//! buffer it does not control, so `flush` has to kill the child and respawn.
//! That is a real cut-off rather than a graceful one, and it is documented on
//! the method rather than hidden. The better answer is a `pipewire` stream with
//! a process callback pulling from a ring buffer this crate owns, so `flush` is
//! a single atomic store; that is a genuine hole and it is marked as one, not
//! papered over with something untested.

use std::collections::VecDeque;

use crate::audio::Pcm;
use crate::{Millis, Result, VoiceError};

/// Somewhere to put samples.
pub trait AudioSink: Send {
    fn name(&self) -> &str;

    /// The rate this sink consumes. [`crate::speaker`] resamples to match.
    fn sample_rate(&self) -> u32;

    /// Queue audio. Must not block for the duration of the audio.
    fn write(&mut self, pcm: &Pcm) -> Result<()>;

    /// Milliseconds queued but not yet heard. The lip-sync clock is
    /// `written - queued`, so this is not merely diagnostic.
    fn queued_ms(&self) -> u32;

    /// Discard everything queued, immediately. Barge-in depends on it.
    fn flush(&mut self);

    /// Stop and release the device.
    fn stop(&mut self);

    /// Is anything still to be heard?
    fn is_draining(&self) -> bool {
        self.queued_ms() > 0
    }
}

// ---------------------------------------------------------------------------
// BufferSink — the one the tests use
// ---------------------------------------------------------------------------

/// Keeps every sample and models playback against a clock the caller advances.
///
/// Deliberately not "a sink that discards": the point is to be able to assert
/// that what she *would have said* is right — its duration, its envelope, that
/// barge-in truncated it where it should have. `all()` is the whole utterance as
/// one buffer, which the manual checks write out as a WAV for the operator to
/// listen to on their own terms, later, if they want to.
#[derive(Debug, Clone)]
pub struct BufferSink {
    rate: u32,
    /// Everything ever written, in order, never dropped by `flush`.
    written: Pcm,
    /// The unplayed tail, in samples.
    queue: VecDeque<f32>,
    /// Samples the caller has said were played.
    played: usize,
    pub flushes: usize,
    pub stopped: bool,
}

impl BufferSink {
    pub fn new(rate: u32) -> Self {
        BufferSink {
            rate,
            written: Pcm::new(rate, Vec::new()),
            queue: VecDeque::new(),
            played: 0,
            flushes: 0,
            stopped: false,
        }
    }

    /// Pretend `ms` of wall clock passed and that much audio was heard.
    pub fn advance(&mut self, ms: u32) {
        let n = (self.rate as u64 * ms as u64 / 1000) as usize;
        for _ in 0..n.min(self.queue.len()) {
            self.queue.pop_front();
            self.played += 1;
        }
    }

    /// Everything ever written to this sink, including audio a `flush` later
    /// discarded — so a test can tell "she was cut off" from "she never spoke".
    pub fn all(&self) -> &Pcm {
        &self.written
    }

    /// Only the audio that was actually heard before any flush.
    pub fn heard_ms(&self) -> u32 {
        ((self.played as u64 * 1000) / self.rate.max(1) as u64) as u32
    }

    pub fn written_ms(&self) -> u32 {
        self.written.duration_ms()
    }
}

impl AudioSink for BufferSink {
    fn name(&self) -> &str {
        "buffer"
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn write(&mut self, pcm: &Pcm) -> Result<()> {
        if self.stopped {
            return Err(VoiceError::Sink("write after stop".into()));
        }
        let p = pcm.resampled(self.rate);
        self.written.samples.extend_from_slice(&p.samples);
        self.queue.extend(p.samples.iter().copied());
        Ok(())
    }
    fn queued_ms(&self) -> u32 {
        ((self.queue.len() as u64 * 1000) / self.rate.max(1) as u64) as u32
    }
    fn flush(&mut self) {
        self.queue.clear();
        self.flushes += 1;
    }
    fn stop(&mut self) {
        self.queue.clear();
        self.stopped = true;
    }
}

/// Consumes and forgets. For a T4 "she is silent but the pipeline still runs"
/// path, and for benchmarks.
#[derive(Debug, Default, Clone)]
pub struct NullSink {
    pub written_ms: u32,
}

impl AudioSink for NullSink {
    fn name(&self) -> &str {
        "null"
    }
    fn sample_rate(&self) -> u32 {
        22_050
    }
    fn write(&mut self, pcm: &Pcm) -> Result<()> {
        self.written_ms += pcm.duration_ms();
        Ok(())
    }
    fn queued_ms(&self) -> u32 {
        0
    }
    fn flush(&mut self) {}
    fn stop(&mut self) {}
}

// ---------------------------------------------------------------------------
// PwCatSink — the real one
// ---------------------------------------------------------------------------

/// Plays through PipeWire by piping raw `f32` into `pw-cat --playback`.
///
/// **Nothing in the test suite constructs one of these.** It is exercised only
/// by the `say` example, which the operator runs deliberately.
///
/// `pw-cat` is part of PipeWire, which SPEC §1 makes a hard dependency, so this
/// adds no crate and no build-time C. The cost is that the kernel pipe and
/// `pw-cat`'s own buffer sit between us and the speaker, so `queued_ms` is an
/// estimate from what we wrote and how long ago, and `flush` cannot politely
/// drop a buffer we do not own — it kills the child. See the module docs.
#[derive(Debug)]
pub struct PwCatSink {
    rate: u32,
    child: Option<std::process::Child>,
    /// Total audio handed over, and when, so `queued_ms` can estimate.
    written_ms: u32,
    started_at: Option<std::time::Instant>,
    /// Name PipeWire shows for the stream, so ducking can exempt her.
    stream_name: String,
}

/// The `application.name` her own playback stream carries. The ducker exempts
/// it by this exact string; changing one without the other makes her duck
/// herself into inaudibility, which is a very confusing bug to chase.
pub const HER_STREAM_NAME: &str = "nx-wisp-voice";

impl PwCatSink {
    /// Spawn `pw-cat`. Fails if PipeWire is not there, which is a legitimate
    /// state — she should go quiet, not crash.
    pub fn open(rate: u32) -> Result<Self> {
        let mut s = PwCatSink {
            rate,
            child: None,
            written_ms: 0,
            started_at: None,
            stream_name: HER_STREAM_NAME.to_string(),
        };
        s.spawn()?;
        Ok(s)
    }

    fn spawn(&mut self) -> Result<()> {
        use std::process::{Command, Stdio};
        let child = Command::new("pw-cat")
            .args([
                "--playback",
                "-",
                "--format",
                "f32",
                "--rate",
                &self.rate.to_string(),
                "--channels",
                "1",
                "--media-role",
                "Notification",
                "--target",
                "auto",
                "-P",
                &format!("{{ node.name = \"{0}\" application.name = \"{0}\" }}", self.stream_name),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| VoiceError::io("spawning pw-cat", e))?;
        self.child = Some(child);
        self.started_at = None;
        self.written_ms = 0;
        Ok(())
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl AudioSink for PwCatSink {
    fn name(&self) -> &str {
        "pw-cat"
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn write(&mut self, pcm: &Pcm) -> Result<()> {
        use std::io::Write;
        let Some(child) = self.child.as_mut() else {
            return Err(VoiceError::Sink("pw-cat is not running".into()));
        };
        let Some(stdin) = child.stdin.as_mut() else {
            return Err(VoiceError::Sink("pw-cat has no stdin".into()));
        };
        let p = pcm.resampled(self.rate);
        let mut bytes = Vec::with_capacity(p.samples.len() * 4);
        for s in &p.samples {
            bytes.extend_from_slice(&s.clamp(-1.0, 1.0).to_le_bytes());
        }
        stdin
            .write_all(&bytes)
            .map_err(|e| VoiceError::io("writing to pw-cat", e))?;
        if self.started_at.is_none() {
            self.started_at = Some(std::time::Instant::now());
        }
        self.written_ms += p.duration_ms();
        Ok(())
    }

    /// An estimate. See the module docs: we do not own the buffer, so this is
    /// "how much we handed over minus how long ago we started", floored at zero.
    fn queued_ms(&self) -> u32 {
        let Some(t0) = self.started_at else { return 0 };
        let elapsed = t0.elapsed().as_millis() as u32;
        self.written_ms.saturating_sub(elapsed)
    }

    /// Kills and respawns `pw-cat`. Abrupt on purpose — see the module docs.
    fn flush(&mut self) {
        self.kill();
        if let Err(e) = self.spawn() {
            tracing::warn!(error = %e, "could not restart pw-cat after a barge-in");
        }
    }

    fn stop(&mut self) {
        self.kill();
    }
}

impl Drop for PwCatSink {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A sink that fails on the nth write, so the caller's error path is real code
/// rather than an `unwrap` nobody ever reached.
#[derive(Debug)]
pub struct FlakySink {
    pub inner: BufferSink,
    pub fail_after: usize,
    writes: usize,
}

impl FlakySink {
    pub fn new(rate: u32, fail_after: usize) -> Self {
        FlakySink { inner: BufferSink::new(rate), fail_after, writes: 0 }
    }
}

impl AudioSink for FlakySink {
    fn name(&self) -> &str {
        "flaky"
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn write(&mut self, pcm: &Pcm) -> Result<()> {
        self.writes += 1;
        if self.writes > self.fail_after {
            return Err(VoiceError::Sink("the device went away".into()));
        }
        self.inner.write(pcm)
    }
    fn queued_ms(&self) -> u32 {
        self.inner.queued_ms()
    }
    fn flush(&mut self) {
        self.inner.flush()
    }
    fn stop(&mut self) {
        self.inner.stop()
    }
}

/// Milliseconds of audio a sink is holding, as a `Millis` for arithmetic
/// against the monotonic clock the rest of the crate uses.
pub fn queued(sink: &dyn AudioSink) -> Millis {
    sink.queued_ms() as Millis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sine;

    #[test]
    fn a_buffer_sink_keeps_what_it_was_given() {
        let mut s = BufferSink::new(22_050);
        s.write(&sine(22_050, 440.0, 200, 0.5)).unwrap();
        assert_eq!(s.written_ms(), 200);
        assert_eq!(s.queued_ms(), 200);
        assert!(s.is_draining());
    }

    #[test]
    fn playing_drains_the_queue_but_not_the_record() {
        let mut s = BufferSink::new(16_000);
        s.write(&sine(16_000, 300.0, 500, 0.5)).unwrap();
        s.advance(200);
        assert_eq!(s.queued_ms(), 300);
        assert_eq!(s.heard_ms(), 200);
        assert_eq!(s.written_ms(), 500, "the record of what she said is not consumed");
    }

    #[test]
    fn advancing_past_the_end_does_not_underflow() {
        let mut s = BufferSink::new(16_000);
        s.write(&Pcm::silence(16_000, 50)).unwrap();
        s.advance(10_000);
        assert_eq!(s.queued_ms(), 0);
        assert!(!s.is_draining());
    }

    #[test]
    fn flush_discards_the_unheard_tail_immediately() {
        // This is barge-in. If it is not immediate she talks over the operator.
        let mut s = BufferSink::new(16_000);
        s.write(&sine(16_000, 300.0, 1000, 0.5)).unwrap();
        s.advance(120);
        s.flush();
        assert_eq!(s.queued_ms(), 0);
        assert_eq!(s.heard_ms(), 120, "she got 120ms out before being cut off");
        assert_eq!(s.written_ms(), 1000, "…of an utterance that was a second long");
        assert_eq!(s.flushes, 1);
    }

    #[test]
    fn a_sink_resamples_rather_than_pitch_shifting_what_it_is_handed() {
        let mut s = BufferSink::new(48_000);
        s.write(&sine(16_000, 300.0, 250, 0.5)).unwrap();
        assert!((s.written_ms() as i64 - 250).abs() <= 2, "{}", s.written_ms());
        assert_eq!(s.all().rate, 48_000);
    }

    #[test]
    fn writing_after_stop_is_an_error_rather_than_silent_loss() {
        let mut s = BufferSink::new(16_000);
        s.stop();
        assert!(s.write(&Pcm::silence(16_000, 10)).is_err());
    }

    #[test]
    fn the_null_sink_swallows_everything_and_is_never_draining() {
        let mut s = NullSink::default();
        s.write(&sine(22_050, 440.0, 300, 0.5)).unwrap();
        assert_eq!(s.written_ms, 300);
        assert_eq!(s.queued_ms(), 0);
        assert!(!s.is_draining());
    }

    #[test]
    fn a_flaky_sink_fails_where_it_was_told_to() {
        let mut s = FlakySink::new(16_000, 1);
        assert!(s.write(&Pcm::silence(16_000, 10)).is_ok());
        assert!(s.write(&Pcm::silence(16_000, 10)).is_err());
    }

    /// Not a test of `pw-cat` — a test that we never accidentally start one.
    /// If this file ever grows a test that constructs a `PwCatSink`, that test
    /// makes noise on the operator's desk, and this is the tripwire.
    #[test]
    fn nothing_in_the_suite_opens_a_real_audio_device() {
        let src = include_str!("sink.rs");
        let tests = src
            .split_once("mod tests {")
            .map(|(_, t)| t)
            .unwrap_or_default();
        // Built at runtime so the needle does not appear in the haystack.
        let needle = format!("{}::{}", "PwCatSink", "open");
        assert!(
            !tests.contains(&needle),
            "a test opened a real sink — the operator is using this machine"
        );
        assert_eq!(HER_STREAM_NAME, "nx-wisp-voice");
    }
}
