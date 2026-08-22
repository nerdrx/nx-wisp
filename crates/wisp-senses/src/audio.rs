//! F23 — PipeWire. How loud the machine is, and whether a microphone is open.
//!
//! Two separate questions, answered two separate ways:
//!
//! - **Is a mic live?** From the registry: any node with a capture media class
//!   in the `running` state. Cheap, event driven, and it does not require us to
//!   open the microphone ourselves — which matters, because opening it would
//!   itself be an invasive act and would light up the operator's own mic
//!   indicator.
//! - **How loud is the output?** From a capture stream on the default sink's
//!   monitor, reduced to a peak. This is *output*, not input: it is the sound
//!   the machine is making, which is what ducking (F33) and "something is loud"
//!   need.
//!
//! Nothing here records audio. The stream's buffers are reduced to one number
//! per quantum and discarded, exactly as the clipboard sense discards its bytes.

use wisp_proto::{Observation, SenseId};

use crate::budget;
use crate::consent::{Sense, SenseCtx, SenseHandle, SensePlugin};

#[derive(Debug, Clone)]
pub struct AudioSense {
    /// Level change, in percentage points, worth publishing between ticks.
    /// How *often* those ticks happen is the governor's call — see
    /// [`crate::budget::audio_interval`].
    pub min_delta: u8,
}

impl Default for AudioSense {
    fn default() -> Self {
        AudioSense { min_delta: 8 }
    }
}

impl Sense for AudioSense {
    const ID: SenseId = SenseId::Audio;
    const LABEL: &'static str = crate::consent::label_of(SenseId::Audio);
    const DESCRIPTION: &'static str = crate::consent::description_of(SenseId::Audio);
}

// ---------------------------------------------------------------------------
// Pure: peaks, smoothing, and what counts as a live microphone
// ---------------------------------------------------------------------------

/// PipeWire's `media.class` for a node that is capturing audio.
pub const CAPTURE_CLASSES: &[&str] = &["Stream/Input/Audio", "Audio/Source"];

/// Node states PipeWire reports. Only `running` means audio is actually moving.
pub fn state_is_running(state: &str) -> bool {
    state.eq_ignore_ascii_case("running")
}

/// A capture *node* that is merely idle is a microphone that exists, not one
/// that is on. Getting this wrong would light her eyes up permanently on any
/// machine with a webcam.
pub fn is_live_capture(media_class: &str, state: &str) -> bool {
    CAPTURE_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(media_class))
        && state_is_running(state)
}

/// Linear sample peak (0.0..=1.0) to the 0..=100 the `Observation` carries.
///
/// Deliberately perceptual rather than linear: a -30 dBFS signal is quiet but
/// audible, and on a linear scale it would round to 3 and read as silence.
pub fn peak_to_level(peak: f32) -> u8 {
    if !peak.is_finite() || peak <= 0.0 {
        return 0;
    }
    let peak = peak.min(1.0);
    let db = 20.0 * peak.log10();
    // -60 dBFS maps to 0, 0 dBFS maps to 100.
    let pct = ((db + 60.0) / 60.0 * 100.0).clamp(0.0, 100.0);
    pct.round() as u8
}

/// Peak of one interleaved f32 buffer.
pub fn buffer_peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| {
        let a = s.abs();
        if a.is_finite() && a > m {
            a
        } else {
            m
        }
    })
}

/// Peak of a PipeWire buffer, which arrives as little-endian `f32` bytes.
///
/// This is what the real-time callback runs, so it is the version worth
/// testing. A trailing partial sample is ignored rather than read past.
pub fn peak_of_le_f32_bytes(bytes: &[u8]) -> f32 {
    bytes.as_chunks::<4>().0.iter().fold(0.0f32, |m, c| {
        let v = f32::from_le_bytes(*c).abs();
        if v.is_finite() && v > m {
            v
        } else {
            m
        }
    })
}

/// Rate limits and smooths the level, and decides when it is worth saying.
///
/// The decay is asymmetric on purpose: the level rises instantly so a sudden
/// loud noise is noticed, and falls slowly so a level meter does not flicker
/// between syllables.
#[derive(Debug)]
pub struct AudioMeter {
    level: u8,
    mic_live: bool,
    last_published: Option<(u8, bool)>,
    min_delta: u8,
    decay_per_tick: u8,
}

impl Default for AudioMeter {
    fn default() -> Self {
        AudioMeter::new(8)
    }
}

impl AudioMeter {
    pub fn new(min_delta: u8) -> Self {
        AudioMeter {
            level: 0,
            mic_live: false,
            last_published: None,
            min_delta,
            decay_per_tick: 12,
        }
    }

    /// Fold in one buffer's peak.
    pub fn observe_peak(&mut self, peak: f32) {
        let l = peak_to_level(peak);
        self.level = self.level.max(l);
    }

    pub fn set_mic_live(&mut self, live: bool) {
        self.mic_live = live;
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    /// Called on the publish interval. Returns an observation only when the
    /// operator would notice a difference.
    pub fn tick(&mut self) -> Option<Observation> {
        let current = (self.level, self.mic_live);
        let publish = match self.last_published {
            None => true,
            // A mic going live or dark is always worth saying: it is the
            // visible-tell input for anything downstream.
            Some((_, was_live)) if was_live != self.mic_live => true,
            Some((last, _)) => last.abs_diff(self.level) >= self.min_delta,
        };
        // Decay after deciding, so the peak that was just measured is the one
        // reported.
        self.level = self.level.saturating_sub(self.decay_per_tick);
        if !publish {
            return None;
        }
        self.last_published = Some(current);
        Some(Observation::AudioLevel { out: current.0, mic_live: current.1 })
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// What the PipeWire thread reports.
#[derive(Debug, Clone, Copy)]
pub enum AudioEvent {
    Peak(f32),
    MicLive(bool),
}

impl SensePlugin for AudioSense {
    fn spawn(self, handle: SenseHandle<Self>, mut ctx: SenseCtx) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AudioEvent>();
            let stop = backend::spawn(tx);

            let mut meter = AudioMeter::new(self.min_delta);
            let mut ticker = tokio::time::interval(budget::audio_interval(ctx.tier()));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut shutdown = ctx.shutdown.clone();

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    Some(tier) = ctx.tier_changed() => {
                        let every = budget::audio_interval(tier);
                        ticker = tokio::time::interval_at(
                            tokio::time::Instant::now() + every,
                            every,
                        );
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                    ev = rx.recv() => match ev {
                        Some(AudioEvent::Peak(p)) => meter.observe_peak(p),
                        Some(AudioEvent::MicLive(live)) => meter.set_mic_live(live),
                        None => {
                            // The backend is gone. Keep the handle so the
                            // consent row is honest, but stop metering.
                            shutdown.wait().await;
                            break;
                        }
                    },
                    _ = ticker.tick() => {
                        if let Some(obs) = meter.tick() {
                            handle.emit(obs);
                        }
                    }
                }
            }
            backend::stop(stop);
        })
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

#[cfg(feature = "pipewire-backend")]
mod backend {
    //! The live backend. Links `libpipewire-0.3`.
    use super::AudioEvent;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    pub struct Stop {
        flag: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    pub fn spawn(tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>) -> Stop {
        let flag = Arc::new(AtomicBool::new(false));
        let f = flag.clone();
        let thread = std::thread::Builder::new()
            .name("wisp-audio".into())
            .spawn(move || {
                if let Err(e) = run(tx, f) {
                    tracing::warn!(error = %e, "audio sense stopped");
                }
            })
            .ok();
        Stop { flag, thread }
    }

    pub fn stop(mut s: Stop) {
        s.flag.store(true, Ordering::Relaxed);
        if let Some(t) = s.thread.take() {
            let _ = t.join();
        }
    }

    fn run(
        tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        use pipewire as pw;
        use pw::spa;

        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;

        // --- mic liveness, from the registry ---------------------------------
        //
        // Reading the graph tells us a capture node is running without us ever
        // opening the microphone. Opening it would itself be invasive and would
        // light the operator's own mic indicator, which would be a lie.
        let live: Arc<std::sync::Mutex<std::collections::BTreeSet<u32>>> =
            Arc::new(std::sync::Mutex::new(Default::default()));
        let tx_reg = tx.clone();
        let live_reg = live.clone();
        let tx_gone = tx.clone();
        let live_gone = live.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else { return };
                let class = props.get("media.class").unwrap_or("");
                let state = props.get("node.state").unwrap_or("");
                if !super::CAPTURE_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(class)) {
                    return;
                }
                let mut set = live_reg.lock().unwrap();
                let before = !set.is_empty();
                if super::state_is_running(state) {
                    set.insert(global.id);
                } else {
                    set.remove(&global.id);
                }
                let now = !set.is_empty();
                if before != now {
                    let _ = tx_reg.send(AudioEvent::MicLive(now));
                }
            })
            .global_remove(move |id| {
                let mut set = live_gone.lock().unwrap();
                if set.remove(&id) && set.is_empty() {
                    let _ = tx_gone.send(AudioEvent::MicLive(false));
                }
            })
            .register();

        // --- output level, from the default sink's monitor --------------------
        let stream = pw::stream::StreamRc::new(
            core.clone(),
            "nx-wisp-monitor",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                *pw::keys::NODE_NAME => "nx-wisp",
            },
        )?;

        let tx_peak = tx.clone();
        let _stream_listener = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let size = data.chunk().size() as usize;
                let Some(bytes) = data.data() else { return };
                let bytes = &bytes[..size.min(bytes.len())];
                // Reduce the buffer to one number, in place. The audio itself
                // is not copied, kept, written anywhere or examined further —
                // the same rule the clipboard sense follows.
                let _ = tx_peak.send(AudioEvent::Peak(super::peak_of_le_f32_bytes(bytes)));
            })
            .register()?;

        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
        let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: audio_info.into(),
            }),
        )
        .map(|(c, _)| c.into_inner())
        .map_err(|e| anyhow::anyhow!("could not build the audio format pod: {e:?}"))?;
        let mut params =
            [spa::pod::Pod::from_bytes(&values).ok_or_else(|| anyhow::anyhow!("bad pod"))?];

        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )?;

        // Wake often enough to notice the stop flag without spinning.
        let ml = mainloop.downgrade();
        let timer = mainloop.loop_().add_timer(move |_| {
            if stop.load(Ordering::Relaxed) {
                if let Some(ml) = ml.upgrade() {
                    ml.quit();
                }
            }
        });
        let tick = std::time::Duration::from_millis(200);
        let _ = timer.update_timer(Some(tick), Some(tick)).into_result();

        mainloop.run();
        Ok(())
    }
}

#[cfg(not(feature = "pipewire-backend"))]
mod backend {
    //! Built without `pipewire-backend`. The sense still holds its handle so the
    //! consent panel is honest, and publishes nothing.
    use super::AudioEvent;

    pub struct Stop;

    pub fn spawn(_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>) -> Stop {
        tracing::warn!("wisp-senses built without the pipewire backend; audio is inert");
        Stop
    }

    pub fn stop(_s: Stop) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_zero_and_full_scale_is_a_hundred() {
        assert_eq!(peak_to_level(0.0), 0);
        assert_eq!(peak_to_level(1.0), 100);
        assert_eq!(peak_to_level(-0.5), 0, "a negative peak is nonsense, not loud");
        assert_eq!(peak_to_level(f32::NAN), 0);
        assert_eq!(peak_to_level(f32::INFINITY), 0);
        assert_eq!(peak_to_level(4.0), 100, "clipped, not overflowed");
    }

    #[test]
    fn quiet_but_audible_does_not_round_to_silence() {
        // -30 dBFS is quiet background music.
        let level = peak_to_level(0.0316);
        assert!(level > 40 && level < 60, "-30 dBFS read as {level}");
        // A linear scale would have made this 3.
        assert!(level > 3);
    }

    #[test]
    fn the_scale_is_monotonic() {
        let mut last = 0;
        for i in 0..=100 {
            let l = peak_to_level(i as f32 / 100.0);
            assert!(l >= last, "level went backwards at {i}");
            last = l;
        }
    }

    #[test]
    fn buffer_peak_takes_the_largest_magnitude() {
        assert_eq!(buffer_peak(&[]), 0.0);
        assert_eq!(buffer_peak(&[0.1, -0.9, 0.3]), 0.9);
        assert_eq!(buffer_peak(&[0.0, 0.0]), 0.0);
        // A denormal or NaN in the buffer must not poison the peak.
        assert_eq!(buffer_peak(&[0.5, f32::NAN, 0.2]), 0.5);
    }

    #[test]
    fn the_realtime_path_reads_pipewires_byte_buffers() {
        let mut bytes = Vec::new();
        for s in [0.1f32, -0.75, 0.25] {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(peak_of_le_f32_bytes(&bytes), 0.75);
        assert_eq!(peak_to_level(peak_of_le_f32_bytes(&bytes)), peak_to_level(0.75));
        assert_eq!(peak_of_le_f32_bytes(&[]), 0.0);
        // A short quantum must be ignored, never read past.
        bytes.extend_from_slice(&[0xff, 0x7f]);
        assert_eq!(peak_of_le_f32_bytes(&bytes), 0.75);
        // NaN in the stream must not poison the meter.
        let nan: Vec<u8> = f32::NAN.to_le_bytes().to_vec();
        assert_eq!(peak_of_le_f32_bytes(&nan), 0.0);
    }

    #[test]
    fn an_existing_but_idle_microphone_is_not_live() {
        assert!(!is_live_capture("Stream/Input/Audio", "idle"));
        assert!(!is_live_capture("Stream/Input/Audio", "suspended"));
        assert!(is_live_capture("Stream/Input/Audio", "running"));
        assert!(is_live_capture("Audio/Source", "running"));
        // Playback is not capture.
        assert!(!is_live_capture("Stream/Output/Audio", "running"));
        assert!(!is_live_capture("Video/Source", "running"));
    }

    #[test]
    fn the_meter_publishes_first_then_only_real_changes() {
        let mut m = AudioMeter::new(8);
        m.observe_peak(0.5);
        assert!(matches!(m.tick(), Some(Observation::AudioLevel { .. })));
        // Same loudness again: nothing to say.
        m.observe_peak(0.5);
        assert_eq!(m.tick(), None);
    }

    #[test]
    fn a_mic_going_live_is_always_published() {
        let mut m = AudioMeter::new(100);
        m.observe_peak(0.5);
        m.tick();
        m.set_mic_live(true);
        let obs = m.tick().expect("a mic opening is always news");
        assert!(matches!(obs, Observation::AudioLevel { mic_live: true, .. }));
        // And going dark again.
        m.set_mic_live(false);
        assert!(matches!(m.tick(), Some(Observation::AudioLevel { mic_live: false, .. })));
    }

    #[test]
    fn the_level_rises_instantly_and_falls_gradually() {
        let mut m = AudioMeter::new(1);
        m.observe_peak(1.0);
        assert_eq!(m.tick(), Some(Observation::AudioLevel { out: 100, mic_live: false }));
        // Silence from here on: it decays rather than dropping to zero.
        let a = m.tick();
        assert!(matches!(a, Some(Observation::AudioLevel { out: 88, .. })), "got {a:?}");
        assert!(m.level() < 88);
        // And it does eventually reach silence.
        for _ in 0..20 {
            m.tick();
        }
        assert_eq!(m.level(), 0);
    }

    #[test]
    fn the_loudest_buffer_in_a_tick_wins() {
        let mut m = AudioMeter::new(1);
        m.observe_peak(0.01);
        m.observe_peak(1.0);
        m.observe_peak(0.02);
        assert_eq!(m.tick(), Some(Observation::AudioLevel { out: 100, mic_live: false }));
    }

    #[test]
    fn only_audio_observations_are_produced() {
        let mut m = AudioMeter::new(1);
        m.observe_peak(0.5);
        assert_eq!(m.tick().unwrap().sense(), SenseId::Audio);
    }

    #[test]
    fn audio_is_ambient_not_invasive() {
        // She hears *that* the machine is loud, never what was said. The
        // microphone itself is a different sense entirely, and that one is
        // invasive.
        assert_eq!(SenseId::Audio.consent(), wisp_proto::Consent::Ambient);
        assert_eq!(SenseId::Microphone.consent(), wisp_proto::Consent::Invasive);
    }
}
