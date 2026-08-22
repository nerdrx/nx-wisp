//! The manual checks. **Every test in this file is `#[ignore]`d.**
//!
//! `cargo test -p wisp-voice` must pass with no GPU, no model, no network and no
//! audio device, so nothing here runs by default. These are the deliberate,
//! operator-invoked runs that prove the *real* paths work — the ones a fake can
//! never prove, because a fake proves only that the fake works.
//!
//! ```console
//! # Download a real voice and synthesise real speech to a WAV file.
//! # Writes into $NX_WISP_CONFIG_DIR, never into the repo, never to a speaker.
//! export NX_WISP_CONFIG_DIR=/tmp/wisp-voice-check
//! cargo test -p wisp-voice --features net,piper-tts --test real_engines -- --ignored --nocapture
//! ```
//!
//! **Nothing in this file plays audio.** The synthesised speech is written to a
//! `.wav` in the config dir and the test asserts on samples; whether to listen
//! to it, and when, is the operator's decision and not this suite's. The one
//! check that would need a microphone is documented at the bottom and is *not*
//! written, for the same reason.

#![cfg(all(feature = "net", feature = "piper-tts"))]

use std::path::PathBuf;

use wisp_voice::models::{ModelStore, Progress};
use wisp_voice::piper::PiperTts;
use wisp_voice::sink::BufferSink;
use wisp_voice::speaker::{BufferedRun, Speaker};
use wisp_voice::tts::{SynthParams, Tts};
use wisp_voice::voices::{Mood, VoiceRegistry};

/// Where this check is allowed to write. Refuses to run without the override,
/// so a stray `--ignored` can never drop 63 MB into the operator's real store or
/// anywhere near the repository.
fn scratch() -> PathBuf {
    let d = std::env::var_os("NX_WISP_CONFIG_DIR").expect(
        "set NX_WISP_CONFIG_DIR before running the manual checks — \
         they download models and must never touch the real store",
    );
    let d = PathBuf::from(d);
    assert!(
        !d.starts_with(env!("CARGO_MANIFEST_DIR")),
        "NX_WISP_CONFIG_DIR points inside the repository; models never live in the repo"
    );
    d
}

/// Fetch the default Piper voice, load it, and make her say something.
#[test]
#[ignore = "downloads ~63 MB and runs ONNX Runtime; run it on purpose"]
fn a_real_voice_downloads_verifies_and_synthesises_real_speech() {
    let dir = scratch();
    let store = ModelStore::at(dir.join("models"));
    let pack = VoiceRegistry::builtin().get("wisp").expect("the default pack").clone();

    let mut last = 0u64;
    let mut on = |p: Progress| {
        // One line per ~8 MB, so the log is readable rather than a flood.
        if p.done.saturating_sub(last) > 8 << 20 || p.done == p.total.unwrap_or(u64::MAX) {
            last = p.done;
            eprintln!(
                "  {} {:>4} MiB{}",
                p.id,
                p.done >> 20,
                if p.resumed { " (resumed)" } else { "" }
            );
        }
    };

    for id in pack.required_models() {
        eprintln!("fetching {id}");
        let path = store.ensure_online(id, &mut on).expect("download and verify");
        assert!(path.exists(), "{id} did not land at {}", path.display());
        // The hash was checked before the rename; check it again from cold, so
        // this really is "the bytes on disk are the bytes we pinned".
        store.verify(id).expect("the installed file matches its pinned sha256");
    }

    // A second run must not touch the network at all.
    for id in pack.required_models() {
        assert!(store.have(id), "{id} should now be installed");
    }

    let mut engine = PiperTts::for_pack(&pack, &store).expect("load the voice");
    eprintln!("engine {} at {} Hz", engine.name(), engine.sample_rate());

    let line = "Your build is green. Nineteen tests passed, and the flaky one behaved itself.";
    let s = engine.synth(line, &pack.params(Mood::Neutral)).expect("synthesis");

    // It is speech, not silence and not noise.
    assert!(s.pcm.duration_ms() > 2_000, "only {} ms came out", s.pcm.duration_ms());
    assert!(s.pcm.duration_ms() < 15_000, "{} ms is not this sentence", s.pcm.duration_ms());
    assert!(s.pcm.peak() > 0.05, "peak {} — that is silence", s.pcm.peak());
    assert!(s.pcm.peak() <= 1.0, "clipping: {}", s.pcm.peak());
    assert!(s.pcm.rms() > 0.01, "rms {} — that is not speech", s.pcm.rms());
    assert!(s.pcm.samples.iter().all(|x| x.is_finite()));
    // Speech has silence in it. A constant tone or a DC block would not.
    let quiet = s.pcm.samples.iter().filter(|x| x.abs() < 0.01).count();
    let frac = quiet as f32 / s.pcm.len() as f32;
    assert!((0.02..0.9).contains(&frac), "{frac} of the samples are near-silent");

    let wav = dir.join("check-neutral.wav");
    s.pcm.write_wav(&wav).unwrap();
    eprintln!(
        "wrote {} ({} ms, peak {:.3}) — listen to it if you like; nothing played it",
        wav.display(),
        s.pcm.duration_ms(),
        s.pcm.peak()
    );

    // Phoneme spans are estimated (see `piper.rs`) but must still tile the audio.
    assert!(!s.phonemes.is_empty(), "no visemes at all from the shipping engine");
    for w in s.phonemes.windows(2) {
        assert!(w[0].end_ms <= w[1].start_ms, "overlapping spans: {w:?}");
    }
    assert!(
        s.phonemes.last().unwrap().end_ms <= s.pcm.duration_ms() + 2,
        "spans run past the audio"
    );
}

/// The mood knobs have to do something audible, and Piper has no pitch control
/// — so this is really a check that the `length_scale` / resample trick in
/// `piper.rs` works on a real model rather than only in arithmetic.
#[test]
#[ignore = "needs the voice from the check above"]
fn mood_changes_how_a_real_voice_sounds() {
    let dir = scratch();
    let store = ModelStore::at(dir.join("models"));
    let pack = VoiceRegistry::builtin().get("wisp").unwrap().clone();
    let mut engine = match PiperTts::for_pack(&pack, &store) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: {e} — run the download check first");
            return;
        }
    };

    let line = "I would not do that on a Friday afternoon.";
    let sleepy = engine.synth(line, &pack.params(Mood::Sleepy)).unwrap();
    let bright = engine.synth(line, &pack.params(Mood::Delighted)).unwrap();

    eprintln!(
        "sleepy {} ms, delighted {} ms",
        sleepy.pcm.duration_ms(),
        bright.pcm.duration_ms()
    );
    assert!(
        sleepy.pcm.duration_ms() > bright.pcm.duration_ms(),
        "sleepy must be slower than delighted"
    );
    // Same words, same engine, different audio: the pitch shift did something.
    assert_ne!(sleepy.pcm.samples, bright.pcm.samples);

    sleepy.pcm.write_wav(&dir.join("check-sleepy.wav")).unwrap();
    bright.pcm.write_wav(&dir.join("check-delighted.wav")).unwrap();

    // And the rate really is honoured rather than merely different: the pitch
    // pre-compensation must not have leaked into the duration.
    let flat = engine
        .synth(line, &SynthParams { voice: pack.id.clone(), ..Default::default() })
        .unwrap();
    let ratio = sleepy.pcm.duration_ms() as f32 / flat.pcm.duration_ms() as f32;
    let want = 1.0 / pack.params(Mood::Sleepy).rate;
    assert!(
        (ratio - want).abs() < 0.2,
        "asked for {want:.2}× the length and got {ratio:.2}×"
    );
}

/// The whole F31 pipeline on the real engine: text arrives a fragment at a time,
/// and audio starts leaving before the text has finished arriving.
#[test]
#[ignore = "needs the voice from the download check"]
fn a_real_engine_starts_talking_before_the_sentence_is_finished() {
    let dir = scratch();
    let store = ModelStore::at(dir.join("models"));
    let pack = VoiceRegistry::builtin().get("wisp").unwrap().clone();
    let mut engine = match PiperTts::for_pack(&pack, &store) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: {e} — run the download check first");
            return;
        }
    };

    let mut sink = BufferSink::new(engine.sample_rate());
    let mut sp = Speaker::default();
    sp.begin(&pack, Mood::Neutral);

    // Only the first clause has arrived.
    sp.push("Your build is green. ");
    let t0 = std::time::Instant::now();
    let evs = sp.pump(&mut engine, &mut sink, 0).unwrap();
    let latency = t0.elapsed();

    assert!(
        evs.iter().any(|e| matches!(e, wisp_voice::speaker::SpeechEvent::Clause { .. })),
        "nothing was synthesised while the model was still writing: {evs:?}"
    );
    assert!(sink.written_ms() > 0);
    eprintln!(
        "first audio after {:?} for {} ms of speech",
        latency,
        sink.written_ms()
    );
    // Faster than real time, or streaming buys nothing.
    assert!(
        (latency.as_millis() as u32) < sink.written_ms(),
        "synthesis was slower than playback"
    );

    // The rest of the reply turns up later, as it would from the model.
    sp.push("Nineteen tests passed, and the flaky one behaved itself for once. ");
    sp.end_text();
    let mut run = BufferedRun::new(&mut sink);
    run.run(&mut sp, &mut engine).unwrap();

    sink.all().write_wav(&dir.join("check-streamed.wav")).unwrap();
    assert!(sink.written_ms() > 3_000, "{} ms", sink.written_ms());
    eprintln!("streamed {} ms total", sink.written_ms());
}

// ---------------------------------------------------------------------------
// What is deliberately NOT here
// ---------------------------------------------------------------------------
//
// **There is no microphone check.** Writing one means opening the operator's
// microphone and recording their room to prove that recording their room works,
// which is precisely the act SPEC §0.3 exists to make impossible to do quietly.
// The consent path is proven instead by
// `wisp_voice::consent_adapter`'s tests, which drive a real `ConsentLedger`,
// assert the tell goes up before the permit exists and down when it is dropped,
// and feed a `FakeMic` — every part of the guarantee except the device itself.
//
// The capture backend behind `MicSource` is, for the same reason, an honest
// hole rather than untested code. See `mic.rs`.
