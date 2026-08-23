//! Her voice, mounted.
//!
//! Compiled only with the `voice-piper` feature — the default build has no
//! synthesiser and spawns nothing, so bubbles stay the whole story there.
//!
//! Audio and synthesis live on a dedicated OS thread for the same reason the
//! mind does: a sentence of Piper synthesis is tens of milliseconds and a
//! `pw-cat` write can block, and neither belongs on the tokio runtime.
//!
//! What she says aloud is exactly what the attention budget already approved —
//! this host is fed from the same place the speech bubble is. It decides
//! nothing; SPEC §3.4's chain of custody ends at the budget, and this is just
//! the loudspeaker at the end of it.

use std::sync::mpsc;
use std::thread::JoinHandle;

use wisp_proto::{Governed, Tier, TierReason};
use wisp_voice::{
    duck::PactlMixer,
    piper::PiperTts,
    sink::PwCatSink,
    tts::Tts,
    voices::{Mood, VoiceRegistry},
    ModelStore, Voice,
};

pub enum VoiceMsg {
    /// Approved speech, with the expression the utterance carried — the same
    /// pair the bubble gets.
    Say { text: String, expression: Option<String> },
    Tier(Tier),
    Shutdown,
}

#[derive(Clone)]
pub struct VoiceSender(mpsc::Sender<VoiceMsg>);

impl VoiceSender {
    pub fn send(&self, msg: VoiceMsg) {
        let _ = self.0.send(msg);
    }
}

pub struct VoiceHost {
    tx: mpsc::Sender<VoiceMsg>,
    thread: Option<JoinHandle<()>>,
}

/// The rig expression names double as the voice's mood vocabulary — they are
/// the same eight words by construction (`wisp_voice::voices::Mood` mirrors
/// `REQUIRED_EXPRESSIONS`). An unknown or absent expression is `Neutral`.
fn mood_of(expression: Option<&str>) -> Mood {
    expression
        .and_then(|e| Mood::ALL.into_iter().find(|m| m.as_str() == e))
        .unwrap_or(Mood::Neutral)
}

impl VoiceHost {
    /// Build the whole audio stack, or explain why she is staying quiet.
    ///
    /// Every failure here is a legitimate state, not an error: no PipeWire, no
    /// fetched voice model, no `pactl`. She goes quiet and the log says why —
    /// a companion that crashes because the sound system is odd is worse than
    /// a mute one.
    pub fn spawn(dir: &std::path::Path) -> Result<VoiceHost, String> {
        let store = ModelStore::open();
        let pack = VoiceRegistry::builtin()
            .get("wisp")
            .ok_or("the built-in voice pack is missing")?
            .clone();
        for id in pack.required_models() {
            if !store.have(id) {
                return Err(format!(
                    "voice model {id:?} is not fetched — `nx-wisp models fetch` gets it"
                ));
            }
        }
        let engine = PiperTts::for_pack(&pack, &store).map_err(|e| e.to_string())?;
        let rate = engine.sample_rate();
        let sink = PwCatSink::open(rate).map_err(|e| e.to_string())?;

        let mut voice = Voice::builder()
            .journal_in(dir.join("duck"))
            .build(Box::new(sink), Box::new(PactlMixer::new()));
        // A crash mid-sentence leaves the operator's music ducked; the journal
        // survives, and recovery replays it before she ever speaks again.
        match voice.recover_ducking() {
            Ok(r) => tracing::debug!(?r, "duck journal recovered"),
            Err(e) => tracing::warn!("duck recovery: {e}"),
        }

        let (tx, rx) = mpsc::channel::<VoiceMsg>();
        let thread = std::thread::Builder::new()
            .name("wisp-voice".into())
            .spawn(move || run(voice, engine, rx))
            .map_err(|e| e.to_string())?;

        Ok(VoiceHost { tx, thread: Some(thread) })
    }

    pub fn sender(&self) -> VoiceSender {
        VoiceSender(self.tx.clone())
    }

    pub fn shutdown(mut self) {
        let _ = self.tx.send(VoiceMsg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(mut voice: Voice, mut tts: PiperTts, rx: mpsc::Receiver<VoiceMsg>) {
    let start = std::time::Instant::now();
    let now_ms = move || start.elapsed().as_millis() as u64;

    loop {
        // Idle: block until there is something to do. Speaking: poll fast
        // enough to keep the play queue fed and barge-in responsive.
        let msg = if voice.is_speaking() {
            match rx.recv_timeout(std::time::Duration::from_millis(30)) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        };

        match msg {
            Some(VoiceMsg::Say { text, expression }) => {
                let mood = mood_of(expression.as_deref());
                if voice.say(&text, mood).is_none() {
                    tracing::debug!("voice refused (tier or policy); the bubble stands alone");
                }
            }
            Some(VoiceMsg::Tier(t)) => {
                // The reason is the governor's business; by the time it
                // reaches audio only the tier itself matters.
                voice.set_tier(t, &TierReason::Idle);
            }
            Some(VoiceMsg::Shutdown) => {
                // Dropping the Voice runs the un-duck; that is the ordinary
                // path. The journal covers the extraordinary ones.
                return;
            }
            None => {}
        }

        let tick = voice.tick(&mut tts, None, now_ms());
        if !tick.is_empty() {
            tracing::trace!(events = tick.speech.len(), cancelled = ?tick.cancelled, "voice tick");
        }
    }
}
