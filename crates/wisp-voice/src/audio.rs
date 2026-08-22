//! Mono `f32` PCM, and the small amount of DSP the rest of the crate needs.
//!
//! Pure: no device, no clock, no allocation surprises. Everything in here is a
//! function of its arguments, which is why the lip-sync tests can assert on
//! exact sample values and the ducking tests never need a sound card.
//!
//! **Mono, `f32`, `-1.0..=1.0`, interleaving never happens.** Every speech
//! engine worth using emits mono float; stereo is the sink's problem, and
//! keeping one representation means the envelope extractor, the resampler and
//! the WAV writer all agree without conversion code between them.

use crate::{Result, VoiceError};

/// What whisper.cpp demands, and therefore what the capture path resamples to.
pub const STT_RATE: u32 = 16_000;

/// A block of mono audio that knows its own sample rate.
///
/// It carries the rate rather than assuming one because the engines disagree:
/// Piper's `low` voices are 16 kHz, its `medium` voices 22.05 kHz, Kokoro is
/// 24 kHz, and whisper only ever wants 16 kHz. A `Pcm` that did not know its own
/// rate would make every one of those a silent pitch bug.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn new(rate: u32, samples: Vec<f32>) -> Self {
        debug_assert!(rate > 0, "a Pcm with no sample rate is a pitch bug waiting");
        Pcm { rate, samples }
    }

    /// `ms` of digital silence. Used as the gap between clauses, and as what a
    /// shed synthesis returns.
    pub fn silence(rate: u32, ms: u32) -> Self {
        let n = (rate as u64 * ms as u64 / 1000) as usize;
        Pcm::new(rate, vec![0.0; n])
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn duration_ms(&self) -> u32 {
        if self.rate == 0 {
            return 0;
        }
        ((self.samples.len() as u64 * 1000) / self.rate as u64) as u32
    }

    /// Sample index for a time offset, clamped to the buffer.
    pub fn index_at_ms(&self, ms: u32) -> usize {
        let i = (self.rate as u64 * ms as u64 / 1000) as usize;
        i.min(self.samples.len())
    }

    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0f32, |m, s| {
            let a = s.abs();
            if a.is_finite() && a > m {
                a
            } else {
                m
            }
        })
    }

    pub fn rms(&self) -> f32 {
        rms(&self.samples)
    }

    /// Append, resampling if the rates differ. This is how the play queue
    /// concatenates clauses that a voice pack change synthesised at two
    /// different rates mid-utterance.
    pub fn append(&mut self, other: &Pcm) {
        if other.rate == self.rate {
            self.samples.extend_from_slice(&other.samples);
        } else {
            let r = other.resampled(self.rate);
            self.samples.extend_from_slice(&r.samples);
        }
    }

    /// Scale in place. Clamps, because a voice pack with `volume = 4.0` should
    /// sound loud and bad rather than produce NaNs downstream.
    pub fn gain(&mut self, g: f32) {
        if !g.is_finite() || (g - 1.0).abs() < f32::EPSILON {
            return;
        }
        for s in &mut self.samples {
            *s = (*s * g).clamp(-1.0, 1.0);
        }
    }

    /// Linear-interpolating resample.
    ///
    /// Deliberately not a windowed-sinc: the two consumers are a level envelope
    /// (which is band-limited to ~30 Hz by the time the rig sees it) and
    /// whisper's 16 kHz front end (whose own mel filterbank throws away
    /// everything this would smear). Paying for `rubato` here would buy quality
    /// nothing downstream can perceive, and would add a dependency to the one
    /// crate that already links two inference runtimes.
    pub fn resampled(&self, to: u32) -> Pcm {
        if to == self.rate || self.samples.is_empty() || to == 0 || self.rate == 0 {
            return Pcm::new(if to == 0 { self.rate } else { to }, self.samples.clone());
        }
        let ratio = self.rate as f64 / to as f64;
        let out_len = ((self.samples.len() as f64) / ratio).round() as usize;
        let mut out = Vec::with_capacity(out_len);
        let last = self.samples.len() - 1;
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let j = pos.floor() as usize;
            if j >= last {
                out.push(self.samples[last]);
            } else {
                let frac = (pos - j as f64) as f32;
                out.push(self.samples[j] + (self.samples[j + 1] - self.samples[j]) * frac);
            }
        }
        Pcm::new(to, out)
    }

    /// A short raised-cosine fade at both ends.
    ///
    /// Every clause boundary in a streamed utterance is a splice, and a splice
    /// on a non-zero sample is an audible click. She would sound like a badly
    /// edited voicemail without this.
    pub fn fade(&mut self, ms: u32) {
        let n = ((self.rate as u64 * ms as u64 / 1000) as usize).min(self.samples.len() / 2);
        if n == 0 {
            return;
        }
        let len = self.samples.len();
        for i in 0..n {
            let w = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / n as f32).cos();
            self.samples[i] *= w;
            self.samples[len - 1 - i] *= w;
        }
    }

    /// Trim leading and trailing samples below `floor`.
    ///
    /// Piper pads every utterance with roughly 100 ms of near-silence. Left in,
    /// that silence lands between every pair of clauses and turns streamed
    /// speech into speech with a stammer.
    pub fn trim_silence(&self, floor: f32) -> Pcm {
        let first = self.samples.iter().position(|s| s.abs() > floor);
        let Some(first) = first else {
            return Pcm::new(self.rate, Vec::new());
        };
        let last = self
            .samples
            .iter()
            .rposition(|s| s.abs() > floor)
            .unwrap_or(first);
        Pcm::new(self.rate, self.samples[first..=last].to_vec())
    }

    /// 16-bit little-endian, for a WAV file or a sink that wants integers.
    pub fn to_i16_le(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.samples.len() * 2);
        for s in &self.samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Write a canonical 16-bit mono WAV.
    ///
    /// This exists so the manual checks can be *listened to by the operator on
    /// their own terms*, later, instead of this crate deciding to make noise on
    /// their speakers during a test run.
    pub fn write_wav(&self, path: &std::path::Path) -> Result<()> {
        let data = self.to_i16_le();
        let mut out = Vec::with_capacity(44 + data.len());
        let chunk = 36 + data.len() as u32;
        let byte_rate = self.rate * 2;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&chunk.to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| VoiceError::io(p.display().to_string(), e))?;
        }
        std::fs::write(path, out).map_err(|e| VoiceError::io(path.display().to_string(), e))
    }

    /// Read back a canonical 16-bit mono WAV. Only used by the manual checks and
    /// by tests that want a fixture round trip — it understands exactly the
    /// files [`Pcm::write_wav`] produces and refuses anything else.
    pub fn read_wav(path: &std::path::Path) -> Result<Pcm> {
        let b = std::fs::read(path).map_err(|e| VoiceError::io(path.display().to_string(), e))?;
        if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
            return Err(VoiceError::Sink(format!("{} is not a RIFF/WAVE file", path.display())));
        }
        let rate = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        let bits = u16::from_le_bytes([b[34], b[35]]);
        if bits != 16 {
            return Err(VoiceError::Sink(format!("{} is {bits}-bit; only 16 is read", path.display())));
        }
        // Walk the chunk list rather than assuming `data` is at 36 — some
        // writers slip a LIST chunk in ahead of it.
        let mut i = 12usize;
        while i + 8 <= b.len() {
            let id = &b[i..i + 4];
            let sz = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
            let body = i + 8;
            if id == b"data" {
                let end = (body + sz).min(b.len());
                let samples = b[body..end]
                    .as_chunks::<2>().0.iter()
                    .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                    .collect();
                return Ok(Pcm::new(rate, samples));
            }
            i = body + sz + (sz & 1);
        }
        Err(VoiceError::Sink(format!("{} has no data chunk", path.display())))
    }
}

/// Shift pitch by resampling, preserving the sample rate.
///
/// Declare the samples to have been recorded at `rate * factor`, then resample
/// to `rate`. `N` samples relabelled at `R'` last `N / R'` seconds, so the result
/// is `factor` times shorter and `factor` times higher. The caller pays the
/// duration back in `length_scale` *before* synthesis — see the module docs.
///
/// **The direction here is easy to get backwards, and was.** `rate / factor`
/// looks equally plausible and does the exact opposite: it lengthens the audio
/// and lowers the pitch, which made `sleepy` come out shorter than `delighted`.
/// Nothing caught it until a real Piper voice was actually synthesised, because
/// this whole module is behind the `piper-tts` feature and its unit tests do not
/// run in the default suite. Hence the arithmetic test below, which does not
/// need the engine.
pub fn pitch_shift(pcm: Pcm, factor: f32) -> Pcm {
    if !factor.is_finite() || factor <= 0.0 || (factor - 1.0).abs() < 1e-3 || pcm.is_empty() {
        return pcm;
    }
    let rate = pcm.rate;
    let pretend = ((rate as f32 * factor).round() as u32).max(1_000);
    Pcm::new(pretend, pcm.samples).resampled(rate)
}

/// Root mean square of a slice, NaN-safe.
pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let sum: f64 = x.iter().map(|s| {
        let s = *s as f64;
        if s.is_finite() {
            s * s
        } else {
            0.0
        }
    }).sum();
    (sum / x.len() as f64).sqrt() as f32
}

/// Linear amplitude to dBFS, floored so silence is a number and not `-inf`.
pub fn to_db(amp: f32) -> f32 {
    if !amp.is_finite() || amp <= 1e-6 {
        return -120.0;
    }
    20.0 * amp.log10()
}

/// dBFS back to linear amplitude.
pub fn from_db(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    10f32.powf(db / 20.0)
}

/// A test tone. Also the fake engine's raw material, so a "did anything come
/// out" assertion has something with a known peak and a known period to check.
pub fn sine(rate: u32, hz: f32, ms: u32, amp: f32) -> Pcm {
    let n = (rate as u64 * ms as u64 / 1000) as usize;
    let mut s = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        s.push((std::f32::consts::TAU * hz * t).sin() * amp);
    }
    Pcm::new(rate, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_the_obvious_arithmetic() {
        assert_eq!(Pcm::silence(16_000, 250).duration_ms(), 250);
        assert_eq!(Pcm::silence(22_050, 1000).len(), 22_050);
        assert_eq!(Pcm::new(16_000, vec![]).duration_ms(), 0);
    }

    #[test]
    fn resampling_preserves_duration_and_shape() {
        let a = sine(22_050, 440.0, 500, 0.8);
        let b = a.resampled(16_000);
        assert_eq!(b.rate, 16_000);
        // Within one sample of the same wall-clock length.
        assert!(
            (b.duration_ms() as i64 - a.duration_ms() as i64).abs() <= 1,
            "{} vs {}",
            a.duration_ms(),
            b.duration_ms()
        );
        // A 440 Hz tone stays a 440 Hz tone: same RMS to within interpolation
        // loss, which for a tone this far below Nyquist is tiny.
        assert!((a.rms() - b.rms()).abs() < 0.02, "{} vs {}", a.rms(), b.rms());
    }

    #[test]
    fn resampling_up_and_back_is_close_to_identity() {
        let a = sine(16_000, 220.0, 200, 0.5);
        let round = a.resampled(48_000).resampled(16_000);
        assert!((a.rms() - round.rms()).abs() < 0.01);
    }

    #[test]
    fn resampling_to_the_same_rate_is_free_and_exact() {
        let a = sine(16_000, 220.0, 50, 0.5);
        assert_eq!(a.resampled(16_000), a);
    }

    #[test]
    fn append_resamples_rather_than_pitch_shifting() {
        let mut a = sine(22_050, 300.0, 100, 0.5);
        let before = a.duration_ms();
        a.append(&sine(16_000, 300.0, 100, 0.5));
        assert_eq!(a.rate, 22_050);
        assert!((a.duration_ms() as i64 - (before as i64 + 100)).abs() <= 2);
    }

    #[test]
    fn fade_removes_the_splice_click() {
        let mut a = Pcm::new(16_000, vec![1.0; 1600]);
        assert_eq!(a.samples[0], 1.0);
        a.fade(10);
        assert!(a.samples[0].abs() < 1e-6, "the first sample must start at zero");
        assert!(a.samples[a.len() - 1].abs() < 1e-6);
        assert_eq!(a.samples[800], 1.0, "the middle must be untouched");
    }

    #[test]
    fn fade_longer_than_the_buffer_does_not_panic_or_invert() {
        let mut a = Pcm::new(16_000, vec![1.0; 8]);
        a.fade(10_000);
        assert!(a.samples.iter().all(|s| (0.0..=1.0).contains(s)));
    }

    #[test]
    fn trim_silence_removes_the_engine_padding_and_keeps_the_speech() {
        let mut s = vec![0.0f32; 1000];
        s.extend(std::iter::repeat_n(0.5, 500));
        s.extend(std::iter::repeat_n(0.0, 1000));
        let p = Pcm::new(16_000, s).trim_silence(0.01);
        assert_eq!(p.len(), 500);
        assert_eq!(p.peak(), 0.5);
    }

    #[test]
    fn trimming_pure_silence_yields_nothing_rather_than_panicking() {
        let p = Pcm::silence(16_000, 200).trim_silence(0.01);
        assert!(p.is_empty());
    }

    #[test]
    fn gain_clamps_instead_of_producing_garbage() {
        let mut p = Pcm::new(16_000, vec![0.8, -0.8]);
        p.gain(4.0);
        assert_eq!(p.samples, vec![1.0, -1.0]);
        let mut q = Pcm::new(16_000, vec![0.5]);
        q.gain(f32::NAN);
        assert_eq!(q.samples, vec![0.5], "a NaN gain must be ignored, not applied");
    }

    #[test]
    fn rms_and_peak_survive_a_poisoned_buffer() {
        let p = Pcm::new(16_000, vec![f32::NAN, 0.5, f32::INFINITY, -0.25]);
        assert!(p.rms().is_finite());
        assert_eq!(p.peak(), 0.5);
    }

    #[test]
    fn pitch_shift_raises_the_pitch_and_shortens_the_audio() {
        let a = sine(22_050, 220.0, 400, 0.5);
        let up = pitch_shift(a.clone(), 1.25);
        assert_eq!(up.rate, a.rate, "the sample rate must not change");
        // Shorter by the factor. The caller pays it back in `length_scale`
        // before synthesis — see `crate::piper`.
        let want = (400.0 / 1.25) as i64;
        assert!((up.duration_ms() as i64 - want).abs() <= 4, "{}", up.duration_ms());
    }

    /// The one that would have caught the inverted shift. A tone shifted up by
    /// `f` must come out `f` times shorter, and one shifted *down* must come out
    /// longer — the sign, not just the magnitude.
    #[test]
    fn shifting_down_lengthens_and_shifting_up_shortens() {
        let a = sine(22_050, 220.0, 400, 0.5);
        let down = pitch_shift(a.clone(), 0.8);
        let up = pitch_shift(a.clone(), 1.25);
        assert!(down.duration_ms() > a.duration_ms(), "0.8x must be longer: {}", down.duration_ms());
        assert!(up.duration_ms() < a.duration_ms(), "1.25x must be shorter: {}", up.duration_ms());
    }

    /// And the pitch really moves: a 220 Hz tone shifted up crosses zero more
    /// often per second than the original.
    #[test]
    fn shifting_up_really_raises_the_pitch() {
        let a = sine(22_050, 220.0, 500, 0.5);
        let up = pitch_shift(a.clone(), 1.5);
        let crossings = |p: &Pcm| {
            p.samples.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f32
                / (p.duration_ms().max(1) as f32 / 1000.0)
        };
        let (lo, hi) = (crossings(&a), crossings(&up));
        assert!(hi > lo * 1.3, "{lo} -> {hi} is not a pitch rise");
    }

    #[test]
    fn a_pitch_of_one_is_a_no_op() {
        let a = sine(22_050, 220.0, 100, 0.5);
        assert_eq!(pitch_shift(a.clone(), 1.0), a);
        assert_eq!(pitch_shift(a.clone(), f32::NAN), a);
        assert_eq!(pitch_shift(a.clone(), -2.0), a, "a negative factor is nonsense, not a reverse");
    }

    #[test]
    fn pitch_shift_on_an_empty_buffer_does_not_panic() {
        assert!(pitch_shift(Pcm::new(22_050, Vec::new()), 1.5).is_empty());
    }

    #[test]
    fn db_round_trips() {
        for a in [1.0f32, 0.5, 0.1, 0.01] {
            assert!((from_db(to_db(a)) - a).abs() < 1e-4, "{a}");
        }
        assert_eq!(to_db(0.0), -120.0);
        assert_eq!(to_db(f32::NAN), -120.0);
    }

    #[test]
    fn wav_round_trips_through_a_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("tone.wav");
        let a = sine(22_050, 440.0, 120, 0.6);
        a.write_wav(&path).unwrap();
        let b = Pcm::read_wav(&path).unwrap();
        assert_eq!(b.rate, a.rate);
        assert_eq!(b.len(), a.len());
        // 16-bit quantisation is the only difference.
        for (x, y) in a.samples.iter().zip(&b.samples) {
            assert!((x - y).abs() < 1e-4);
        }
    }

    #[test]
    fn a_non_wav_file_is_refused_rather_than_misread() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("not.wav");
        std::fs::write(&p, b"this is not audio at all, not even close").unwrap();
        assert!(Pcm::read_wav(&p).is_err());
    }

    #[test]
    fn index_at_ms_clamps() {
        let p = Pcm::silence(16_000, 100);
        assert_eq!(p.index_at_ms(0), 0);
        assert_eq!(p.index_at_ms(50), 800);
        assert_eq!(p.index_at_ms(10_000), p.len());
    }
}
