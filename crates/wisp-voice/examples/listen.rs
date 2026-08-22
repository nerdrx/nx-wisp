//! `listen` — transcribe a recording through the real consent gate.
//!
//! ```console
//! source env.sh
//! cargo run -p wisp-voice --features net,whisper-stt,consent --example listen -- recording.wav
//! ```
//!
//! ## Why this takes a file and not a microphone
//!
//! There is no capture backend in this crate, deliberately. Writing one means
//! opening the operator's microphone to test that opening the operator's
//! microphone works — which is the exact act SPEC §0.3 exists to make impossible
//! to do quietly. [`wisp_voice::mic::MicSource`] is the seam where a PipeWire
//! capture stream goes, and it is an honest hole rather than untested code.
//!
//! So this example plays the part instead: a [`WavMic`] feeds a file the
//! operator chose through the *entire* real path — the consent ledger, the
//! granted `SenseHandle`, the visible tell, the `Listener`, whisper.cpp, the
//! streaming partials, the bus. Everything except the device.
//!
//! ## It still asks permission
//!
//! `Observation::Speech` carries `SenseId::Microphone`, which is
//! `Consent::Invasive`. That is true whether the audio came from a device or
//! from a file — the *observation* is the sensitive thing, not the wire it
//! arrived on — so this refuses to run until the operator has enabled the
//! microphone in the consent panel. It will not enable it for them. The whole
//! point of the ledger is that only the operator flips that switch.

use std::path::{Path, PathBuf};

use wisp_voice::audio::{Pcm, STT_RATE};
use wisp_voice::consent_adapter::GrantedMic;
use wisp_voice::mic::{ListenConfig, Listener, MicSource};
use wisp_voice::models::{ModelStore, Progress};
use wisp_voice::stt::whisper::WhisperStt;
use wisp_voice::stt::SttModel;
use wisp_voice::tier::policy;
use wisp_voice::Result;
use wisp_proto::{Observation, SenseId, Tier};
use wisp_senses::clock::Clock;
use wisp_senses::consent::ConsentLedger;

/// A [`MicSource`] that reads from a file the operator handed us.
///
/// Chunked at roughly a capture period so the streaming path is exercised the
/// way a real device would exercise it — one long `read` would make partials
/// meaningless and prove nothing.
struct WavMic {
    pcm: Pcm,
    at: usize,
    chunk: usize,
}

impl WavMic {
    fn open(path: &Path) -> Result<Self> {
        let pcm = Pcm::read_wav(path)?;
        let chunk = (pcm.rate as usize / 20).max(1); // ~50 ms
        Ok(WavMic { pcm, at: 0, chunk })
    }
}

impl MicSource for WavMic {
    fn sample_rate(&self) -> u32 {
        self.pcm.rate
    }
    fn read(&mut self) -> Result<Vec<f32>> {
        let end = (self.at + self.chunk).min(self.pcm.samples.len());
        let out = self.pcm.samples[self.at..end].to_vec();
        self.at = end;
        Ok(out)
    }
    fn stop(&mut self) {
        self.at = self.pcm.samples.len();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut path: Option<PathBuf> = None;
    let mut model = SttModel::Base;
    let mut gpu = true;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--model" => {
                let m = args.next().unwrap_or_else(|| die("--model needs a name"));
                model = match m.as_str() {
                    "tiny" | "tiny.en" => SttModel::Tiny,
                    "base" | "base.en" => SttModel::Base,
                    other => die(&format!("unknown model {other:?}; try tiny or base")),
                };
            }
            "--cpu" => gpu = false,
            "-h" | "--help" => {
                eprintln!("{HELP}");
                return;
            }
            other => path = Some(PathBuf::from(other)),
        }
    }
    let Some(path) = path else {
        eprintln!("{HELP}");
        std::process::exit(2);
    };

    // 1. Consent. This is the gate, and it is not ours to open.
    let (bus, mut events) = tokio::sync::broadcast::channel(64);
    let ledger = ConsentLedger::load(bus, Clock::new());
    if !ledger.is_enabled(SenseId::Microphone) {
        eprintln!(
            "The microphone is off, which is how it ships (SPEC §3.7).\n\
             Enable it in NX Wisp's consent panel and run this again.\n\
             This example will not enable it for you — that switch is yours."
        );
        std::process::exit(3);
    }

    // 2. The model.
    let store = ModelStore::open();
    let model_id = format!("whisper-{}", model.id());
    if !store.have(&model_id) {
        eprintln!("fetching {model_id}…");
        let mut last = 0u64;
        if let Err(e) = store.ensure_online(&model_id, &mut |p: Progress| {
            if p.done.saturating_sub(last) > 4 << 20 {
                last = p.done;
                match p.total {
                    Some(t) => eprint!("\r  {:>3}%  ", p.done * 100 / t.max(1)),
                    None => eprint!("\r  {} MiB  ", p.done >> 20),
                }
            }
        }) {
            eprintln!();
            die(&format!("{e}"));
        }
        eprintln!();
    }
    let model_path = store.path(&model_id).unwrap_or_else(|| die("not in the manifest"));

    // The governor's answer, not a guess: T2 and below forbid the discrete GPU.
    let tier = Tier::Full;
    let use_gpu = gpu && policy(tier).dgpu;
    let mut stt = match WhisperStt::open_with(&model_path, model, use_gpu) {
        Ok(s) => s,
        Err(e) => die(&format!("{e}")),
    };
    eprintln!("engine: whisper {} ({})", model.id(), if use_gpu { "gpu" } else { "cpu" });

    // 3. The permit. The tell goes up here, before a single sample is read.
    let permit = match GrantedMic::request(&ledger) {
        Ok(p) => p,
        Err(e) => die(&format!("{e}")),
    };
    let source = match WavMic::open(&path) {
        Ok(s) => s,
        Err(e) => die(&format!("{e}")),
    };
    let total_ms = source.pcm.duration_ms();
    eprintln!(
        "reading {} ({} ms at {} Hz → {} Hz)",
        path.display(),
        total_ms,
        source.pcm.rate,
        STT_RATE
    );

    let mut listener = match Listener::open(permit, Box::new(source), ListenConfig::default()) {
        Ok(l) => l,
        Err(e) => die(&format!("{e}")),
    };

    // 4. Push to talk, and pump until the file runs out.
    let started = std::time::Instant::now();
    let mut now: u64 = 0;
    listener.ptt_down(now);
    let mut last_partial = String::new();
    loop {
        now += 50;
        match listener.pump(&mut stt, now) {
            Ok(obs) => {
                for o in obs {
                    if let Observation::Speech { text, final_ } = o {
                        if final_ {
                            println!("\n{text}");
                        } else if text != last_partial {
                            eprint!("\r… {text}");
                            last_partial = text;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("\n{e}");
                break;
            }
        }
        // Stop once the file is spent and whisper has had a moment to finish
        // the tail. A real capture would run until the operator let go of the
        // key; a file simply ends.
        if now > total_ms as u64 + 2_000 {
            break;
        }
    }
    listener.ptt_up(now);
    if let Ok(obs) = listener.pump(&mut stt, now + 100) {
        for o in obs {
            if let Observation::Speech { text, final_: true } = o {
                println!("\n{text}");
            }
        }
    }

    // 5. Letting go of the listener drops the permit, which lowers the tell.
    listener.close();
    eprintln!(
        "\ndone in {:?}; the microphone row shows {} uses today",
        started.elapsed(),
        ledger.uses_today(SenseId::Microphone)
    );

    // The tell's rise and fall really did go on the bus.
    let mut tells = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if let wisp_proto::EventKind::InvasiveActive { sense, active } = ev.kind {
            tells.push((sense, active));
        }
    }
    eprintln!("tell transitions on the bus: {tells:?}");
}

fn die(msg: &str) -> ! {
    eprintln!("listen: {msg}");
    std::process::exit(1);
}

const HELP: &str = "\
listen — transcribe a recording through NX Wisp's consent gate

USAGE
    listen [OPTIONS] <FILE.wav>

OPTIONS
    --model <tiny|base>   whisper model (default: base)
    --cpu                 force the CPU backend

It takes a file rather than the microphone on purpose: this crate has no capture
backend, because writing one means opening your microphone to prove that opening
your microphone works. Everything else — the consent ledger, the granted handle,
the visible tell, streaming partials, the bus — is the real path.

The microphone must already be enabled in the consent panel. This will not
enable it for you.";
