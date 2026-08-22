//! `say` — make her talk, on purpose, from a terminal.
//!
//! ```console
//! source env.sh
//! export NX_WISP_CONFIG_DIR=~/.config/nx-wisp        # or a scratch dir
//! cargo run -p wisp-voice --features net,piper-tts --example say -- \
//!     "Your build is green. Nineteen tests passed."
//!
//! # Options
//! --voice wisp-warm     pick a pack (see --list)
//! --mood delighted      one of the eight expressions
//! --out speech.wav      where to write the audio (default: $NX_WISP_CONFIG_DIR)
//! --play                actually make a sound through pw-cat
//! --list                show the installed voice packs and exit
//! ```
//!
//! **It writes a WAV and stops there unless you pass `--play`.** Making noise on
//! somebody's desk is something they should have asked for.

use std::path::PathBuf;

use wisp_voice::audio::Pcm;
use wisp_voice::models::{ModelStore, Progress};
use wisp_voice::piper::PiperTts;
use wisp_voice::sink::{AudioSink, BufferSink, PwCatSink};
use wisp_voice::speaker::{BufferedRun, Speaker, SpeechEvent};
use wisp_voice::tts::Tts;
use wisp_voice::voices::{Mood, VoiceRegistry};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut voice_id = "wisp".to_string();
    let mut mood = Mood::Neutral;
    let mut out: Option<PathBuf> = None;
    let mut play = false;
    let mut words: Vec<String> = Vec::new();

    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--voice" => voice_id = it.next().unwrap_or_else(|| die("--voice needs a name")),
            "--mood" => {
                let m = it.next().unwrap_or_else(|| die("--mood needs a name"));
                mood = Mood::parse(&m).unwrap_or_else(|| {
                    die(&format!(
                        "unknown mood {m:?}; one of: {}",
                        Mood::ALL.map(|m| m.as_str()).join(", ")
                    ))
                });
            }
            "--out" => out = Some(PathBuf::from(it.next().unwrap_or_else(|| die("--out needs a path")))),
            "--play" => play = true,
            "--list" => {
                list();
                return;
            }
            "-h" | "--help" => {
                eprintln!("{}", HELP);
                return;
            }
            other => words.push(other.to_string()),
        }
    }

    let text = if words.is_empty() {
        "Your build is green. Nineteen tests passed, and the flaky one behaved itself.".to_string()
    } else {
        words.join(" ")
    };

    let registry = VoiceRegistry::load_dir(&VoiceRegistry::user_dir());
    let pack = registry
        .get(&voice_id)
        .unwrap_or_else(|| die(&format!("no voice pack {voice_id:?}; try --list")))
        .clone();

    // 1. Make sure the model is on disk. Pinned URL, pinned hash, verify then
    //    move — and nothing at all happens if it is already installed.
    let store = ModelStore::open();
    eprintln!("store: {}", store.root().display());
    for id in pack.required_models() {
        if store.have(id) {
            continue;
        }
        eprintln!("fetching {id}…");
        let mut last = 0u64;
        let r = store.ensure_online(id, &mut |p: Progress| {
            if p.done.saturating_sub(last) > 4 << 20 {
                last = p.done;
                match p.total {
                    Some(t) => eprint!("\r  {:>3}%  ", p.done * 100 / t.max(1)),
                    None => eprint!("\r  {} MiB  ", p.done >> 20),
                }
            }
        });
        eprintln!();
        if let Err(e) = r {
            die(&format!("{e}"));
        }
    }

    // 2. Load the engine and stream the line through the real pipeline — the
    //    same `Speaker` the app uses, not a shortcut.
    let mut engine = match PiperTts::for_pack(&pack, &store) {
        Ok(e) => e,
        Err(e) => die(&format!("loading {}: {e}", pack.id)),
    };
    let rate = engine.sample_rate();
    eprintln!("voice: {} ({}, {} Hz), mood: {mood}", pack.name, engine.name(), rate);

    let mut sink = BufferSink::new(rate);
    let mut speaker = Speaker::default();
    speaker.begin(&pack, mood);
    speaker.push(&text);
    speaker.end_text();

    let started = std::time::Instant::now();
    let mut first: Option<std::time::Duration> = None;
    let mut run = BufferedRun::new(&mut sink);
    let events = match run.run(&mut speaker, &mut engine) {
        Ok(evs) => evs,
        Err(e) => die(&format!("{e}")),
    };
    for e in &events {
        if let SpeechEvent::Clause { seq, text, ms, .. } = e {
            if *seq == 0 {
                first = Some(started.elapsed());
            }
            eprintln!("  [{seq}] {ms:>5} ms  {text}");
        }
    }

    let audio: &Pcm = sink.all();
    eprintln!(
        "{} ms of speech in {:?}{}",
        audio.duration_ms(),
        started.elapsed(),
        first
            .map(|d| format!(" (first clause after {d:?})"))
            .unwrap_or_default()
    );

    // 3. The WAV, always.
    let path = out.unwrap_or_else(|| wisp_voice::data_dir().join("say.wav"));
    match audio.write_wav(&path) {
        Ok(()) => eprintln!("wrote {}", path.display()),
        Err(e) => die(&format!("{e}")),
    }

    // 4. Sound, only if asked.
    if play {
        eprintln!("playing through pw-cat…");
        match PwCatSink::open(rate) {
            Ok(mut s) => {
                if let Err(e) = s.write(audio) {
                    eprintln!("playback failed: {e}");
                }
                // `pw-cat` reads from a pipe; give it the audio's own length
                // plus a moment before the child is killed by `Drop`.
                std::thread::sleep(std::time::Duration::from_millis(
                    audio.duration_ms() as u64 + 400,
                ));
            }
            Err(e) => eprintln!("no pw-cat: {e}"),
        }
    } else {
        eprintln!("(nothing was played; pass --play if you want to hear it)");
    }
}

fn list() {
    let store = ModelStore::open();
    let registry = VoiceRegistry::load_dir(&VoiceRegistry::user_dir());
    println!("{:<14} {:<22} {:<8} {:<12} INSTALLED", "ID", "NAME", "ENGINE", "UNTIL");
    for p in registry.all() {
        let installed = p.required_models().iter().all(|id| store.have(id));
        println!(
            "{:<14} {:<22} {:<8} {:<12} {}",
            p.id,
            p.name,
            format!("{:?}", p.engine),
            format!("{:?}", p.allowed_until),
            if p.required_models().is_empty() {
                "n/a".to_string()
            } else if installed {
                "yes".to_string()
            } else {
                let plan = store.plan(p.required_models());
                format!("no ({:.0} MiB)", plan.remaining_mib())
            }
        );
    }
    println!("\npacks are read from {}", VoiceRegistry::user_dir().display());
}

fn die(msg: &str) -> ! {
    eprintln!("say: {msg}");
    std::process::exit(1);
}

const HELP: &str = "\
say — synthesise a line with NX Wisp's voice

USAGE
    say [OPTIONS] [TEXT...]

OPTIONS
    --voice <id>   voice pack (default: wisp)
    --mood <name>  neutral curious delighted smug worried bored sleepy alarmed
    --out <path>   WAV destination
    --play         play it through pw-cat as well as writing the file
    --list         list voice packs and whether their models are installed

Models download on first use from pinned URLs and are verified by sha256.
They live under $NX_WISP_CONFIG_DIR (or ~/.local/share/nx-wisp), never in the
repository.";
