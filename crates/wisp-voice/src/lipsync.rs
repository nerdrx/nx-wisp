//! F32 — turning her voice into a mouth.
//!
//! This module is the whole of the lip-sync analysis, and it deliberately stops
//! one step short of the rig. It emits a plain stream of
//! `(time, openness, emphasis, viseme)` and nothing else; `wisp-rig` is pure and
//! is fed a `RigInput`, so *applying* a [`DriveFrame`] to a jaw bone is the
//! host's job. That split is not tidiness for its own sake: it is what lets the
//! entire mouth pipeline be tested with no GPU, no compositor and no sound card,
//! and it is why this file has no dependency on `wisp-rig` or `wisp-shell`.
//!
//! ## Two signals, not one
//!
//! [`DriveFrame::openness`] is *how far the mouth is open*. It is a normalised
//! level, so it says nothing about whether this moment is loud in absolute
//! terms — a whisper opens her mouth, because a whisper is speech.
//!
//! [`DriveFrame::emphasis`] is *how much this moment stands out*. It is an onset
//! measure against a local running average, not a loudness, and it is what a
//! head nod or a brow raise should be driven from. A steady, loud, sustained
//! vowel has high openness and near-zero emphasis; a consonant that punches out
//! of a quiet passage has modest openness and emphasis near one. Driving a nod
//! from loudness instead produces a companion who bobs continuously through
//! every long vowel, which reads as a bad Muppet.
//!
//! ## The normalisation problem, and the answer this file picked
//!
//! A fixed absolute threshold is the obvious implementation and it is wrong in
//! both directions. Voice packs are data (F35), authored by whoever, and their
//! levels vary by 20 dB or more: a quiet pack leaves her mouth shut through
//! entire sentences, and a hot one pegs her open at frame three and holds her
//! there like a nutcracker. Both are worse than no lip-sync at all, because both
//! look *broken* rather than *absent*.
//!
//! Full normalisation — divide by the utterance's own peak — fixes that and
//! introduces a subtler bug: it destroys relative loudness *within* the level
//! range, so an utterance made entirely of quiet consonants gets pulled up until
//! she is yelling with her mouth. Real speech has 25–35 dB between a stressed
//! vowel and a weak fricative, and that difference is most of what makes a mouth
//! look like it is saying words.
//!
//! So this is a **partial** normalisation, which is really a compressor:
//! [`LipsyncConfig::agc_strength`] is the fraction of an utterance's deviation
//! from [`LipsyncConfig::reference_db`] that gets compensated. At `0.0` it is a
//! fixed absolute threshold; at `1.0` it is full per-utterance normalisation;
//! the default `0.55` gives a 20 dB-quiet pack 11 dB of makeup — clearly moving,
//! still visibly gentler — while a 10 dB gap between a vowel and a consonant
//! survives as roughly 4.5 dB and stays visible.
//!
//! ## Whole-utterance or adaptive, and why both exist
//!
//! [`Normalise::Utterance`] takes the reference from a high percentile of the
//! whole utterance's frame levels. It is the better-looking answer and it needs
//! all the audio up front, which is exactly what F31's streaming synthesis does
//! not have: she starts speaking clause one while clause three is still being
//! generated.
//!
//! [`Normalise::Adaptive`] tracks the reference with an instant-attack,
//! slow-release follower, so it works on a stream at the cost of the first
//! syllable of a cold start being over-open — the follower is seeded low
//! ([`LipsyncConfig::agc_seed_db`]) on purpose, because a mouth that opens a
//! little too eagerly on the first syllable is far less noticeable than a first
//! word delivered with the mouth shut.
//!
//! [`LipsyncStream`] is the streaming front end and carries one follower across
//! every clause of an utterance, so clause boundaries do not produce a step in
//! her mouth the way independently-normalised per-clause tracks would.
//!
//! ## Visemes are used when the engine gives them, and never required
//!
//! Piper phonemises with espeak-ng before it synthesises and the VITS duration
//! predictor already knows where each phone lands, so [`Synthesis::phonemes`]
//! costs nothing and is strictly better than guessing mouth shapes from a level.
//! But it is optional — an engine without a front end we can see into returns an
//! empty vec — and the energy-only path is not a degraded stub. It is what most
//! engines will actually get, so it is held to the same bar: silence closes her,
//! speech opens her, and the viseme channel reports [`Viseme::Sil`] or a neutral
//! open vowel rather than lying about articulation it cannot know.

use serde::{Deserialize, Serialize};

use crate::audio::{rms, to_db, Pcm};
use crate::tts::{PhonemeSpan, Synthesis};

// ---------------------------------------------------------------------------
// Visemes
// ---------------------------------------------------------------------------

/// The mouth shapes a rig has to be able to draw.
///
/// This is the Oculus/OVR fifteen, not a bespoke set, and not the forty-odd
/// phonemes of English. Two reasons. A skin author has to hand-draw or
/// hand-rig every one of these, and fifteen is the largest number anyone
/// actually finishes; and the set is *visually* complete — /p/, /b/ and /m/ are
/// one closed-lips shape no matter how different they sound, so a larger set
/// would buy nothing a viewer could see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Viseme {
    /// Closed and at rest. Also what an unrecognised pause becomes.
    #[default]
    Sil,
    /// `p b m` — lips together.
    PP,
    /// `f v` — lower lip to upper teeth.
    FF,
    /// `θ ð` — tongue at the teeth.
    TH,
    /// `t d` — alveolar stop.
    DD,
    /// `k g` — velar stop, jaw slightly lower than DD.
    KK,
    /// `tʃ dʒ ʃ ʒ` — protruded, small aperture.
    CH,
    /// `s z` — teeth nearly closed.
    SS,
    /// `n l ŋ` — tongue up, jaw mid.
    NN,
    /// `r ɹ ɾ` — the widest consonant.
    RR,
    /// `ɑ a ʌ æ` — open.
    AA,
    /// `ɛ e ə` — mid.
    E,
    /// `ɪ i j` — spread and close.
    I,
    /// `ɔ o ɒ` — rounded and open.
    O,
    /// `ʊ u w` — rounded and close.
    U,
}

impl Viseme {
    /// Every variant, in declaration order. For a skin validator that wants to
    /// prove a pack drew all of them.
    pub const ALL: [Viseme; 15] = [
        Viseme::Sil,
        Viseme::PP,
        Viseme::FF,
        Viseme::TH,
        Viseme::DD,
        Viseme::KK,
        Viseme::CH,
        Viseme::SS,
        Viseme::NN,
        Viseme::RR,
        Viseme::AA,
        Viseme::E,
        Viseme::I,
        Viseme::O,
        Viseme::U,
    ];

    /// How far the jaw goes for this shape when it is spoken at full effort.
    ///
    /// These are a *ceiling*, not a value — see [`Viseme::blend`]. The numbers
    /// are ordered the way a sagittal view of the articulators is ordered
    /// (`PP` shut, `SS` barely cracked, `AA` and `O` wide) rather than measured
    /// off anyone in particular, because the rig applies them through a skin's
    /// own blend shapes and only the *ordering* survives that.
    pub fn target_openness(self) -> f32 {
        match self {
            Viseme::Sil => 0.00,
            Viseme::PP => 0.00,
            Viseme::FF => 0.15,
            Viseme::SS => 0.16,
            Viseme::TH => 0.22,
            Viseme::CH => 0.24,
            Viseme::NN => 0.24,
            Viseme::DD => 0.28,
            Viseme::KK => 0.30,
            Viseme::RR => 0.34,
            Viseme::U => 0.40,
            Viseme::I => 0.48,
            Viseme::E => 0.70,
            Viseme::O => 0.80,
            Viseme::AA => 1.00,
        }
    }

    /// Is this a shape a closed mouth can hold? Used to decide whether a frame
    /// with no energy is genuinely silent or merely a stop consonant.
    pub fn is_closed(self) -> bool {
        matches!(self, Viseme::Sil | Viseme::PP)
    }

    /// The identifier a skin file uses.
    pub fn name(self) -> &'static str {
        match self {
            Viseme::Sil => "sil",
            Viseme::PP => "PP",
            Viseme::FF => "FF",
            Viseme::TH => "TH",
            Viseme::DD => "DD",
            Viseme::KK => "kk",
            Viseme::CH => "CH",
            Viseme::SS => "SS",
            Viseme::NN => "nn",
            Viseme::RR => "RR",
            Viseme::AA => "aa",
            Viseme::E => "E",
            Viseme::I => "I",
            Viseme::O => "O",
            Viseme::U => "U",
        }
    }

    pub fn from_name(s: &str) -> Option<Viseme> {
        Viseme::ALL.iter().copied().find(|v| v.name().eq_ignore_ascii_case(s))
    }

    /// Combine an articulation ceiling with how much energy is actually coming
    /// out right now.
    ///
    /// Multiplicative, and that is the load-bearing decision. The viseme says
    /// *how far this mouth shape can open*; the envelope says *how much of that
    /// is being used*. So a silent frame inside a vowel span closes her (the
    /// engine's span said /ɑ/, but there is no sound, so the mouth is shut and
    /// merely shaped like /ɑ/), and — the case a pure-energy driver always gets
    /// wrong — a *shouted* /m/ keeps her lips together, because no amount of
    /// energy opens a bilabial nasal.
    ///
    /// An additive or `lerp` blend would leak energy into `PP` and give her an
    /// open mouth on every loud "mmm", which is the single most obvious tell
    /// that a companion is faking lip-sync off a volume meter.
    pub fn blend(self, envelope: f32) -> f32 {
        (self.target_openness() * envelope).clamp(0.0, 1.0)
    }
}

/// Characters that decorate a phoneme without changing what the mouth does.
///
/// Stress (`ˈ ˌ`), length (`ː ˑ`), the affricate tie (`t͡ʃ`), aspiration and
/// secondary articulation (`ʰ ʲ ʷ`), and the combining diacritics espeak-ng
/// sprinkles on for nasalisation and syllabicity. Dropping them first is what
/// makes the lookup table a table of *phones* rather than a table of every
/// decorated spelling of every phone.
fn is_decoration(c: char) -> bool {
    matches!(
        c,
        'ˈ' | 'ˌ'
            | 'ː'
            | 'ˑ'
            | '\u{0361}' // combining double inverted breve, the affricate tie
            | '\u{035C}' // combining double breve below, same job
            | '‿'
            | 'ʰ'
            | 'ʲ'
            | 'ʷ'
            | 'ˠ'
            | 'ˤ'
            | 'ʼ'
            | '\u{0303}' // nasalised
            | '\u{0329}' // syllabic
            | '\u{032F}' // non-syllabic
            | '\u{0325}' // voiceless
            | '\u{032C}' // voiced
            | '\u{02DE}'
    ) || ('\u{0300}'..='\u{036F}').contains(&c)
}

/// Symbols an engine uses to mean "nothing is being said here".
fn is_pause(c: char) -> bool {
    matches!(c, '_' | '.' | ',' | '|' | '‖' | '#' | '-' | '–' | '—' | '!' | '?' | ';' | ':' | ' ')
}

/// Map one engine symbol onto a mouth shape.
///
/// Total, by construction: every input returns something and nothing panics,
/// because this is fed strings from a model's front end and a new espeak-ng
/// release adding a phone must not be able to take her mouth offline. An
/// unrecognised symbol becomes [`Viseme::E`] rather than [`Viseme::Sil`] —
/// something we do not recognise is far more likely to be a phone we have not
/// listed than a pause, and a neutral half-open mouth is the least wrong guess.
pub fn viseme_for(symbol: &str) -> Viseme {
    let cleaned: String = symbol
        .chars()
        .filter(|c| !is_decoration(*c))
        .flat_map(|c| c.to_lowercase())
        .collect();

    if cleaned.is_empty() || cleaned.chars().all(is_pause) {
        return Viseme::Sil;
    }

    // Multi-character phones first, longest match wins. This covers the
    // affricates once the tie has been stripped (`t͡ʃ` → `tʃ`) and, as a bonus,
    // ARPAbet — some engines report `AH1`/`SH` instead of IPA and the stress
    // digit has already been dropped as a decoration would be.
    let head: String = cleaned.chars().take(2).collect();
    if let Some(v) = two_char_phone(&head) {
        return v;
    }

    match cleaned.chars().next() {
        Some(c) => one_char_phone(c),
        None => Viseme::Sil,
    }
}

fn two_char_phone(s: &str) -> Option<Viseme> {
    Some(match s {
        // Affricates, IPA and its ASCII transliterations.
        "tʃ" | "dʒ" | "tɕ" | "dʑ" | "tʂ" | "dʐ" | "ts" | "dz" | "ch" | "jh" => Viseme::CH,
        "sh" | "zh" => Viseme::CH,
        "th" | "dh" => Viseme::TH,
        "ng" => Viseme::NN,
        "hh" => Viseme::E,
        // ARPAbet vowels and diphthongs. A diphthong takes the shape of its
        // *first* target: the rig is being driven at 15–60 fps and the glide
        // is shorter than the frame it would land in.
        "aa" | "ae" | "ah" | "ay" | "aw" => Viseme::AA,
        "ax" | "eh" | "ey" | "er" => Viseme::E,
        "ih" | "iy" => Viseme::I,
        "ao" | "ow" | "oy" => Viseme::O,
        "uh" | "uw" => Viseme::U,
        _ => return None,
    })
}

fn one_char_phone(c: char) -> Viseme {
    match c {
        // Bilabial and labiodental — the two shapes a viewer notices.
        'p' | 'b' | 'm' | 'ɱ' | 'ʙ' => Viseme::PP,
        'f' | 'v' | 'ʋ' | 'ɸ' | 'β' => Viseme::FF,
        'θ' | 'ð' => Viseme::TH,
        's' | 'z' => Viseme::SS,
        'ʃ' | 'ʒ' | 'ʂ' | 'ʐ' | 'ɕ' | 'ʑ' => Viseme::CH,
        't' | 'd' | 'ʈ' | 'ɖ' | 'c' | 'ɟ' => Viseme::DD,
        'k' | 'g' | 'ɡ' | 'q' | 'ɢ' | 'x' | 'ɣ' | 'χ' => Viseme::KK,
        'n' | 'ŋ' | 'ɲ' | 'ɳ' | 'l' | 'ɭ' | 'ʎ' | 'ɫ' | 'ɬ' => Viseme::NN,
        'r' | 'ɹ' | 'ɾ' | 'ɻ' | 'ʀ' | 'ʁ' | 'ɽ' | 'ɰ' => Viseme::RR,
        // Vowels and the glides that share their shape.
        'ɑ' | 'a' | 'ʌ' | 'ɐ' | 'æ' => Viseme::AA,
        'ɛ' | 'e' | 'ə' | 'ɜ' | 'ɝ' | 'ɚ' | 'ø' | 'œ' | 'ɞ' | 'ɘ' => Viseme::E,
        'ɪ' | 'i' | 'ɨ' | 'y' | 'ʏ' | 'j' => Viseme::I,
        'ɔ' | 'o' | 'ɒ' | 'ɤ' => Viseme::O,
        'ʊ' | 'u' | 'ʉ' | 'ɯ' | 'w' | 'ʍ' => Viseme::U,
        // /h/ and the glottal stop have no shape of their own — the mouth is
        // already wherever the neighbouring vowel put it. Neutral is the
        // cheapest way to say "do not move".
        'h' | 'ɦ' | 'ʔ' => Viseme::E,
        _ => Viseme::E,
    }
}

// ---------------------------------------------------------------------------
// Frames and tracks
// ---------------------------------------------------------------------------

/// One analysed instant of her voice.
///
/// Self-contained on purpose: no handle, no index into anything, no reference
/// to the audio it came from. The host reads a frame, writes three numbers into
/// a `RigInput` and forgets it, and the flight recorder (SPEC §0.4) can store a
/// whole track as plain data.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriveFrame {
    pub t_ms: u32,
    /// `0.0` shut, `1.0` as wide as this rig goes. Always finite.
    pub openness: f32,
    /// `0.0` unremarkable, `1.0` a hard onset. Always finite.
    pub emphasis: f32,
    pub viseme: Viseme,
}

impl DriveFrame {
    /// A mouth at rest. What every out-of-range sample returns.
    pub fn closed(t_ms: u32) -> DriveFrame {
        DriveFrame { t_ms, openness: 0.0, emphasis: 0.0, viseme: Viseme::Sil }
    }

    /// Is this frame's mouth effectively shut? Used by the host to skip a
    /// pointless blend-shape write, and by the tests to find edges.
    pub fn is_closed(&self) -> bool {
        self.openness < 0.02
    }
}

/// Where the normalisation reference comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Normalise {
    /// A high percentile of the whole utterance. Needs all the audio first;
    /// produces the steadiest-looking mouth. The default for a finished
    /// [`Synthesis`].
    #[default]
    Utterance,
    /// An instant-attack, slow-release follower. Works one buffer at a time,
    /// which is what streaming synthesis needs.
    Adaptive,
}

/// Everything the analysis is allowed to be opinionated about.
///
/// A struct rather than constants because the frame rate genuinely is a
/// parameter: `wisp-gov` runs the rig at 60, 30 or 15 fps depending on the tier
/// (`wisp_proto::Tier::target_fps`), and analysing at 60 to feed a 15 fps rig
/// wastes four fifths of the work on a machine the governor has already decided
/// is busy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LipsyncConfig {
    /// Analysis frames per second. Clamped to a sane band on use.
    pub fps: u32,
    /// Length of the RMS window, centred on the frame time.
    ///
    /// 32 ms is two pitch periods at a low male F0 and seven at a high female
    /// one, so the window measures a *level* rather than tracing individual
    /// glottal pulses, and it is still short enough that a 50 ms consonant is
    /// not averaged out of existence.
    pub window_ms: f32,
    /// Time constant of the rise. See the note on [`LipsyncConfig::release_ms`].
    pub attack_ms: f32,
    /// Time constant of the fall.
    ///
    /// **Asymmetric on purpose, and this is the single most visible constant in
    /// the file.** Jaw opening is an active, fast gesture; jaw closing is
    /// largely elastic recoil and takes noticeably longer. A symmetric filter —
    /// any symmetric filter, at any time constant — makes her look like she is
    /// chewing, because the mouth spends equal time on both edges of every
    /// syllable and the eye reads that as mastication rather than speech.
    ///
    /// 18 ms up is faster than one frame at any tier's frame rate, so an onset
    /// is never late; ~95 ms down is roughly the closing gesture into a stop and
    /// is slow enough that the inter-pulse dips inside a sustained vowel do not
    /// show through as flutter.
    pub release_ms: f32,
    /// Below this absolute level a frame is silence, whatever the normalisation
    /// would like to make of it. Without this floor an utterance of pure digital
    /// silence normalises its own noise up to a fully open mouth.
    pub silence_db: f32,
    /// The frame level a *hot, well-mastered* voice pack's loudest moments sit
    /// at. The fixed anchor the partial normalisation pulls towards.
    ///
    /// Deliberately at the top of the plausible range rather than in the middle
    /// of it. Openness is clamped at 1.0, so an anchor set at a typical level
    /// means every loud utterance saturates and the difference between two of
    /// them is thrown away by the clamp — the exact information the partial
    /// normalisation exists to keep. Anchored at the loud end instead, a hot
    /// utterance reaches ~1.0, an ordinary one ~0.85 and a whisper ~0.6, which
    /// is a curve rather than a wall.
    pub reference_db: f32,
    /// How much of an utterance's deviation from `reference_db` is compensated.
    /// `0.0` is a fixed threshold, `1.0` is full per-utterance normalisation.
    /// See the module docs for why neither extreme is right.
    pub agc_strength: f32,
    /// Range below the reference that maps to a shut mouth. 26 dB is a little
    /// narrower than speech's real vowel-to-fricative range, which is deliberate
    /// compression: it keeps weak consonants visible instead of correct.
    pub span_db: f32,
    /// Floor on a voiced frame's openness. A consonant should crack the lips
    /// even when it is 30 dB down, or her mouth appears to stall mid-word.
    pub min_open: f32,
    /// Where the adaptive follower starts, in raw dB, before it has heard
    /// anything. Low on purpose — see the module docs.
    pub agc_seed_db: f32,
    /// How fast the adaptive follower gives back level it is no longer seeing.
    pub agc_release_db_per_s: f32,
    /// How low the adaptive follower may go, so a long quiet passage cannot
    /// wind the gain up until room tone reads as a shout.
    pub agc_min_db: f32,
    /// Time constant of the local average that [`DriveFrame::emphasis`] is
    /// measured against.
    ///
    /// ~110 ms is roughly one syllable: long enough that a transient is
    /// measured against something and short enough that a sustained vowel
    /// stops being an event within its own duration. Anything much longer and
    /// a held note keeps reporting emphasis for half a second after it started,
    /// which on a rig is a companion who nods all the way through "sooooo".
    pub emphasis_avg_ms: f32,
    /// How far above the local average counts as maximum emphasis.
    pub emphasis_span_db: f32,
    pub emphasis_attack_ms: f32,
    /// Longer than the openness release: emphasis drives a nod, and a nod that
    /// decays as fast as a jaw is a twitch. Not *much* longer, though — the
    /// combined decay of this and the local average is what a rig actually
    /// sees, and half a second of lingering emphasis turns one accented
    /// syllable into a nod that outlasts the word it belonged to.
    pub emphasis_release_ms: f32,
    /// Openness below which the energy-only path reports [`Viseme::Sil`].
    pub closed_threshold: f32,
    pub normalise: Normalise,
}

impl Default for LipsyncConfig {
    fn default() -> Self {
        LipsyncConfig {
            fps: 60,
            window_ms: 32.0,
            attack_ms: 18.0,
            release_ms: 95.0,
            silence_db: -60.0,
            reference_db: -7.0,
            agc_strength: 0.55,
            span_db: 26.0,
            min_open: 0.05,
            agc_seed_db: -30.0,
            agc_release_db_per_s: 10.0,
            agc_min_db: -55.0,
            emphasis_avg_ms: 110.0,
            emphasis_span_db: 16.0,
            emphasis_attack_ms: 8.0,
            emphasis_release_ms: 120.0,
            closed_threshold: 0.10,
            normalise: Normalise::Utterance,
        }
    }
}

impl LipsyncConfig {
    pub fn at(fps: u32) -> LipsyncConfig {
        LipsyncConfig { fps, ..LipsyncConfig::default() }
    }

    /// Analyse at whatever the rig is actually going to render at.
    ///
    /// `Tier::Dormant` reports 0 fps — she is silenced, so there is nothing to
    /// analyse — but a zero here would be a division, so it becomes the lowest
    /// real rate and the caller's decision not to speak stands on its own.
    pub fn for_tier(tier: wisp_proto::Tier) -> LipsyncConfig {
        LipsyncConfig::at(tier.target_fps().max(15))
    }

    /// Streaming variant of this config.
    pub fn streaming(mut self) -> LipsyncConfig {
        self.normalise = Normalise::Adaptive;
        self
    }

    /// Frame rate actually used, guarded against a hand-written `0` or a
    /// nonsense `100_000`.
    fn rate(&self) -> u32 {
        self.fps.clamp(1, 240)
    }

    fn hop_ms(&self) -> f32 {
        1000.0 / self.rate() as f32
    }
}

/// A whole utterance's worth of mouth, on a fixed frame grid.
///
/// The grid is `k * 1000 / fps` milliseconds, computed from the index every
/// time rather than accumulated, which is the entire reason a dozen appended
/// clauses do not drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveTrack {
    fps: u32,
    frames: Vec<DriveFrame>,
    total_ms: u32,
}

impl DriveTrack {
    /// An empty track that still knows what grid it is on, so a clause that
    /// synthesised to nothing can still be appended to.
    pub fn empty(fps: u32) -> DriveTrack {
        DriveTrack { fps: fps.clamp(1, 240), frames: Vec::new(), total_ms: 0 }
    }

    /// Adopt a run of frames that are already on the `fps` grid. Used by
    /// [`LipsyncStream`]; a caller with arbitrary frames should go through
    /// [`DriveTrack::append`] instead, which re-grids them.
    pub fn from_frames(fps: u32, frames: Vec<DriveFrame>, total_ms: u32) -> DriveTrack {
        let fps = fps.clamp(1, 240);
        let last = frames.last().map(|f| f.t_ms).unwrap_or(0);
        DriveTrack { fps, frames, total_ms: total_ms.max(last) }
    }

    /// The full path: audio *and* the engine's phoneme timings.
    pub fn from_synthesis(s: &Synthesis, fps: u32) -> DriveTrack {
        DriveTrack::analyse(&s.pcm, &s.phonemes, &LipsyncConfig::at(fps))
    }

    /// The path every engine without a visible front end gets. Energy only.
    pub fn from_pcm(p: &Pcm, fps: u32) -> DriveTrack {
        DriveTrack::analyse(p, &[], &LipsyncConfig::at(fps))
    }

    /// Analyse, with everything spelled out.
    ///
    /// `spans` may be empty, need not cover the audio, and is not trusted to be
    /// sorted or non-overlapping — an engine's front end is not our code.
    pub fn analyse(pcm: &Pcm, spans: &[PhonemeSpan], cfg: &LipsyncConfig) -> DriveTrack {
        let fps = cfg.rate();
        if pcm.samples.is_empty() || pcm.rate == 0 {
            return DriveTrack::empty(fps);
        }
        let total_ms = pcm.duration_ms();
        let n = frame_count(total_ms, fps);

        // Pass one: the raw level of every frame. Cheap to keep — 60 floats a
        // second — and it is what `Normalise::Utterance` needs before it can
        // decide anything.
        let mut dbs = Vec::with_capacity(n as usize);
        for k in 0..n {
            dbs.push(frame_db(&pcm.samples, pcm.rate, frame_time_ms(k, fps), cfg.window_ms));
        }

        let mut det = Detector::new(cfg);
        if cfg.normalise == Normalise::Utterance {
            det.pin_ceiling(utterance_ceiling(&dbs, cfg));
        }

        let mut lookup = SpanLookup::new(spans);
        let mut frames = Vec::with_capacity(n as usize);
        for (k, db) in dbs.into_iter().enumerate() {
            let t = frame_time_ms(k as u64, fps);
            let viseme = lookup.at(t);
            frames.push(det.step(t.round() as u32, db, viseme, cfg));
        }
        DriveTrack { fps, frames, total_ms }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn frames(&self) -> &[DriveFrame] {
        &self.frames
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn duration_ms(&self) -> u32 {
        self.total_ms
    }

    pub fn peak_openness(&self) -> f32 {
        self.frames.iter().fold(0.0f32, |m, f| m.max(f.openness))
    }

    pub fn mean_openness(&self) -> f32 {
        if self.frames.is_empty() {
            return 0.0;
        }
        self.frames.iter().map(|f| f.openness as f64).sum::<f64>() as f32 / self.frames.len() as f32
    }

    /// The mouth at an arbitrary instant.
    ///
    /// The compositor hands the rig a frame callback at whatever rate the
    /// output is running — 59.951 Hz on one monitor, 143.98 on another, and
    /// neither of them is the analysis rate. Snapping to the nearest analysis
    /// frame puts a visible stair-step in her jaw at 15 fps, so openness and
    /// emphasis are interpolated. The viseme is not: it is a discrete channel
    /// (there is no shape half way between `PP` and `SS`) and takes the nearer
    /// frame's value, exactly as `wisp_rig::ease::Ease::Step` would.
    ///
    /// Outside the track she is **closed**, not held at the first or last
    /// frame. Holding the last frame is the classic lip-sync bug where an
    /// utterance that ends on a vowel leaves her gaping until the next one
    /// starts.
    pub fn sample(&self, t_ms: u32) -> DriveFrame {
        if self.frames.is_empty() {
            return DriveFrame::closed(t_ms);
        }
        let last = self.frames[self.frames.len() - 1];
        if t_ms >= self.total_ms.max(last.t_ms) {
            return DriveFrame::closed(t_ms);
        }

        let pos = t_ms as f64 * self.fps as f64 / 1000.0;
        let i = pos.floor().max(0.0) as usize;
        if i + 1 >= self.frames.len() {
            // The sliver between the last analysis frame and the end of the
            // audio. Ease to closed across it rather than stepping, so a track
            // whose final frame is still open does not snap shut.
            let span = (self.total_ms.max(last.t_ms) - last.t_ms) as f32;
            let f = if span <= 0.0 {
                0.0
            } else {
                1.0 - ((t_ms.saturating_sub(last.t_ms)) as f32 / span).clamp(0.0, 1.0)
            };
            return DriveFrame {
                t_ms,
                openness: last.openness * f,
                emphasis: last.emphasis * f,
                viseme: if f > 0.5 { last.viseme } else { Viseme::Sil },
            };
        }

        let a = self.frames[i];
        let b = self.frames[i + 1];
        let f = (pos - i as f64) as f32;
        DriveFrame {
            t_ms,
            openness: (a.openness + (b.openness - a.openness) * f).clamp(0.0, 1.0),
            emphasis: (a.emphasis + (b.emphasis - a.emphasis) * f).clamp(0.0, 1.0),
            viseme: if f < 0.5 { a.viseme } else { b.viseme },
        }
    }

    /// Splice another track in at `at_ms`.
    ///
    /// `other` is **re-sampled onto this track's grid** rather than having an
    /// offset added to its frame times. That costs one interpolation per frame
    /// and buys the thing the streaming path actually needs: after twelve
    /// clauses the frame times are still exactly `k * 1000 / fps`, because they
    /// are computed from `k` and never accumulated. Adding offsets instead
    /// accumulates a rounding error per clause, and a track that is 40 ms out
    /// by the end of a paragraph is a mouth that finishes after the audio.
    ///
    /// It also makes an `fps` mismatch a non-event, which matters because the
    /// governor can change tier mid-utterance and the next clause will be
    /// analysed at the new rate.
    ///
    /// Where the two overlap the louder mouth wins. Overlap only happens when a
    /// caller deliberately crossfades two clauses, and in that case the audible
    /// result is the sum, so showing the quieter of the two would be wrong.
    pub fn append(&mut self, other: &DriveTrack, at_ms: u32) {
        if other.is_empty() {
            self.total_ms = self.total_ms.max(at_ms);
            return;
        }
        let fps = self.fps;
        let end_ms = at_ms.saturating_add(other.duration_ms());
        let first = ((at_ms as u64 * fps as u64) as f64 / 1000.0).ceil() as u64;
        let cap = fps as u64 * MAX_TRACK_SECONDS;
        let last = frame_count(end_ms, fps).saturating_sub(1).min(cap);
        if first > last {
            self.total_ms = self.total_ms.max(end_ms);
            return;
        }

        // Grow the grid to reach `last`, filling any gap between the old end and
        // `at_ms` with a closed mouth rather than leaving a hole.
        while (self.frames.len() as u64) <= last {
            let k = self.frames.len() as u64;
            self.frames.push(DriveFrame::closed(frame_time_ms(k, fps).round() as u32));
        }

        for k in first..=last {
            let t = frame_time_ms(k, fps);
            let local = (t - at_ms as f64).max(0.0).round() as u32;
            let s = other.sample(local);
            let slot = &mut self.frames[k as usize];
            if s.openness >= slot.openness {
                slot.openness = s.openness;
                slot.viseme = s.viseme;
            }
            slot.emphasis = slot.emphasis.max(s.emphasis);
        }
        self.total_ms = self.total_ms.max(end_ms);
    }

    /// Append at the end of what is already here. The streaming case.
    pub fn push(&mut self, other: &DriveTrack) {
        let at = self.total_ms;
        self.append(other, at);
    }
}

// ---------------------------------------------------------------------------
// The analysis itself
// ---------------------------------------------------------------------------

/// Ceiling on how long a single track may grow to. An hour of speech is far
/// more than any utterance, and the alternative to a cap is that an `at_ms` a
/// caller got wrong allocates until the machine swaps.
const MAX_TRACK_SECONDS: u64 = 3600;

fn frame_time_ms(k: u64, fps: u32) -> f64 {
    k as f64 * 1000.0 / fps as f64
}

/// Frames needed to cover `total_ms` inclusive, so the last frame lands on or
/// just before the end of the audio and never past it.
fn frame_count(total_ms: u32, fps: u32) -> u64 {
    (total_ms as u64 * fps as u64) / 1000 + 1
}

/// RMS of a window centred on `t_ms`, in dBFS.
///
/// Centred rather than trailing because a trailing window makes the mouth lag
/// the sound by half a window, and half of 32 ms is already a third of a frame
/// at 60 fps. [`LipsyncStream`] pays for this by holding back half a window of
/// lookahead, which is the only latency this module adds.
fn frame_db(samples: &[f32], rate: u32, t_ms: f64, window_ms: f32) -> f32 {
    let half = (window_ms.max(1.0) as f64 * 0.5) * rate as f64 / 1000.0;
    let centre = t_ms * rate as f64 / 1000.0;
    let lo = (centre - half).max(0.0) as usize;
    let hi = ((centre + half).max(0.0) as usize).min(samples.len());
    if lo >= hi {
        return -120.0;
    }
    to_db(rms(&samples[lo..hi]))
}

/// The loud end of an utterance, robustly.
///
/// The 95th percentile of the voiced frames, not the maximum. A single clipped
/// sample or one plosive burst would set a maximum-based reference several dB
/// too high and leave the whole utterance looking mumbled; a percentile ignores
/// it. Frames below the silence floor are excluded entirely — leading and
/// trailing engine padding would otherwise drag the reference down.
fn utterance_ceiling(dbs: &[f32], cfg: &LipsyncConfig) -> f32 {
    let mut voiced: Vec<f32> = dbs.iter().copied().filter(|d| *d > cfg.silence_db).collect();
    if voiced.is_empty() {
        return cfg.silence_db;
    }
    voiced.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = (((voiced.len() - 1) as f32) * 0.95).round() as usize;
    voiced[i.min(voiced.len() - 1)]
}

/// One-pole coefficient for a time constant and a step. `tau <= 0` means snap.
fn coeff(tau_ms: f32, dt_ms: f32) -> f32 {
    if tau_ms <= 0.0 || !tau_ms.is_finite() {
        return 1.0;
    }
    (1.0 - (-dt_ms / tau_ms).exp()).clamp(0.0, 1.0)
}

/// The per-frame state machine. Identical code drives the offline and the
/// streaming paths; the only difference is whether the ceiling is pinned.
#[derive(Debug, Clone)]
struct Detector {
    ceiling_db: f32,
    pinned: bool,
    avg_db: f32,
    open: f32,
    emph: f32,
    attack: f32,
    release: f32,
    e_attack: f32,
    e_release: f32,
    avg_coeff: f32,
    decay_db: f32,
}

impl Detector {
    fn new(cfg: &LipsyncConfig) -> Detector {
        let hop = cfg.hop_ms();
        Detector {
            ceiling_db: cfg.agc_seed_db,
            pinned: false,
            // Seeded at the silence floor, not at the first frame's level, so
            // the very first syllable of an utterance registers as the onset it
            // audibly is instead of being averaged away before it is measured.
            avg_db: cfg.silence_db,
            open: 0.0,
            emph: 0.0,
            attack: coeff(cfg.attack_ms, hop),
            release: coeff(cfg.release_ms, hop),
            e_attack: coeff(cfg.emphasis_attack_ms, hop),
            e_release: coeff(cfg.emphasis_release_ms, hop),
            avg_coeff: coeff(cfg.emphasis_avg_ms, hop),
            decay_db: cfg.agc_release_db_per_s.max(0.0) * hop / 1000.0,
        }
    }

    fn pin_ceiling(&mut self, db: f32) {
        self.ceiling_db = db;
        self.pinned = true;
    }

    fn step(&mut self, t_ms: u32, db: f32, viseme: Option<Viseme>, cfg: &LipsyncConfig) -> DriveFrame {
        let db = if db.is_finite() { db } else { -120.0 };
        let voiced = db > cfg.silence_db;
        let gated = db.max(cfg.silence_db);

        // The adaptive follower. Instant attack, because a reference that lags
        // a louder frame means that frame reads as more than fully open and
        // clips; slow release, only while there is something to release
        // against, so a pause between clauses does not reset the gain and make
        // the next clause start too wide.
        if !self.pinned && voiced {
            if gated > self.ceiling_db {
                self.ceiling_db = gated;
            } else {
                self.ceiling_db = (self.ceiling_db - self.decay_db).max(cfg.agc_min_db);
            }
        }

        // Partial normalisation: pull the reference `agc_strength` of the way
        // from the fixed anchor towards what this utterance is actually doing.
        let strength = cfg.agc_strength.clamp(0.0, 1.0);
        let reference = cfg.reference_db + strength * (self.ceiling_db - cfg.reference_db);
        let span = cfg.span_db.max(1.0);
        let norm = (1.0 + (gated - reference) / span).clamp(0.0, 1.0);

        let envelope = if voiced {
            cfg.min_open.clamp(0.0, 1.0) + (1.0 - cfg.min_open.clamp(0.0, 1.0)) * norm
        } else {
            0.0
        };

        let target = match viseme {
            Some(v) => v.blend(envelope),
            None => envelope,
        };

        let c = if target > self.open { self.attack } else { self.release };
        self.open += (target - self.open) * c;
        self.open = self.open.clamp(0.0, 1.0);

        // With phonemes, the engine's shape stands even when the mouth is shut:
        // a `PP` frame at zero openness is lips *pressed together*, which is a
        // different thing for a rig to draw than lips at rest, and collapsing it
        // to `Sil` would throw away the one cue that makes /m/ read as /m/.
        //
        // Without them, the shape is read back off the smoothed openness rather
        // than off the raw envelope, so the viseme channel and the openness
        // channel can never disagree about whether her mouth is open.
        let viseme = viseme.unwrap_or(if self.open < cfg.closed_threshold {
            Viseme::Sil
        } else if self.open < 0.45 {
            // Neutral and half open. With no phonemes we know she is making a
            // sound and nothing at all about her tongue, so the honest shape is
            // the one that commits to least.
            Viseme::E
        } else {
            Viseme::AA
        });

        // Emphasis: how far above its own recent history this frame is. Note
        // the average is updated *after* the comparison and is causal, which is
        // what makes a sustained vowel decay to nothing — the average walks up
        // to meet it — while a transient stays a transient.
        let e_target = if voiced {
            ((gated - self.avg_db) / cfg.emphasis_span_db.max(1.0)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.avg_db += (gated - self.avg_db) * self.avg_coeff;
        let ec = if e_target > self.emph { self.e_attack } else { self.e_release };
        self.emph += (e_target - self.emph) * ec;
        self.emph = self.emph.clamp(0.0, 1.0);

        DriveFrame {
            t_ms,
            openness: if self.open.is_finite() { self.open } else { 0.0 },
            emphasis: if self.emph.is_finite() { self.emph } else { 0.0 },
            viseme,
        }
    }
}

/// A cursor over an engine's phoneme spans.
///
/// Sorts a copy on construction and then walks forward, so the common case —
/// frames asked for in order — is O(1) per frame and a front end that emitted
/// spans out of order still works instead of silently reporting `Sil`.
struct SpanLookup<'a> {
    spans: Vec<&'a PhonemeSpan>,
    cursor: usize,
    empty: bool,
}

impl<'a> SpanLookup<'a> {
    fn new(spans: &'a [PhonemeSpan]) -> SpanLookup<'a> {
        let mut v: Vec<&PhonemeSpan> = spans.iter().filter(|s| s.end_ms > s.start_ms).collect();
        v.sort_by_key(|s| s.start_ms);
        SpanLookup { empty: v.is_empty(), spans: v, cursor: 0 }
    }

    /// `None` means "no phonemes at all" — the energy-only path. A time that
    /// simply falls in a gap between spans returns `Some(Viseme::Sil)`, which is
    /// a different thing: the engine told us there is nothing being articulated
    /// there.
    fn at(&mut self, t_ms: f64) -> Option<Viseme> {
        if self.empty {
            return None;
        }
        let t = t_ms.max(0.0) as u32;
        while self.cursor > 0 && self.spans[self.cursor - 1].start_ms > t {
            self.cursor -= 1;
        }
        while self.cursor + 1 < self.spans.len() && self.spans[self.cursor + 1].start_ms <= t {
            self.cursor += 1;
        }
        let s = self.spans[self.cursor];
        if t >= s.start_ms && t < s.end_ms {
            Some(viseme_for(&s.symbol))
        } else {
            Some(Viseme::Sil)
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Lip-sync for audio that does not exist yet.
///
/// F31 synthesises clause by clause and starts playing clause one while clause
/// two is still in the engine, so by the time her mouth needs to move there is
/// no "whole utterance" to normalise against. This is the same analysis with
/// [`Normalise::Adaptive`] and a half-window of lookahead: push PCM as it comes
/// out of the engine, get back the frames that are now fully determined, and
/// call [`LipsyncStream::finish`] to flush the tail.
///
/// One instance spans a whole utterance rather than a clause, which is the
/// point — the follower, the local average and the attack/release state all
/// carry across a clause boundary, so a splice in the audio does not become a
/// step in her jaw.
#[derive(Debug, Clone)]
pub struct LipsyncStream {
    cfg: LipsyncConfig,
    rate: u32,
    det: Detector,
    /// Samples not yet consumed, plus the absolute index of `buf[0]`.
    buf: Vec<f32>,
    buf_start: u64,
    pushed: u64,
    next_frame: u64,
    spans: Vec<PhonemeSpan>,
    span_cursor: usize,
    frames: Vec<DriveFrame>,
    done: bool,
}

impl LipsyncStream {
    pub fn new(rate: u32, cfg: LipsyncConfig) -> LipsyncStream {
        let cfg = LipsyncConfig { normalise: Normalise::Adaptive, ..cfg };
        LipsyncStream {
            det: Detector::new(&cfg),
            rate: rate.max(1),
            cfg,
            buf: Vec::new(),
            buf_start: 0,
            pushed: 0,
            next_frame: 0,
            spans: Vec::new(),
            span_cursor: 0,
            frames: Vec::new(),
            done: false,
        }
    }

    pub fn config(&self) -> &LipsyncConfig {
        &self.cfg
    }

    /// Milliseconds of audio pushed so far. Where the next clause will land.
    pub fn pushed_ms(&self) -> u32 {
        ((self.pushed * 1000) / self.rate as u64) as u32
    }

    /// Feed one clause, phonemes and all. The spans are offset by however much
    /// audio has already gone in, so an engine that reports spans relative to
    /// its own clause needs no bookkeeping from the caller.
    pub fn push_synthesis(&mut self, s: &Synthesis) -> Vec<DriveFrame> {
        let offset = self.pushed_ms();
        for p in &s.phonemes {
            if p.end_ms > p.start_ms {
                self.spans.push(PhonemeSpan::new(
                    p.symbol.clone(),
                    p.start_ms.saturating_add(offset),
                    p.end_ms.saturating_add(offset),
                ));
            }
        }
        self.push(&s.pcm)
    }

    /// Feed one buffer of audio. Resamples if the engine changed rate
    /// mid-utterance, which a voice-pack switch can do.
    pub fn push(&mut self, pcm: &Pcm) -> Vec<DriveFrame> {
        if pcm.rate != 0 && pcm.rate != self.rate {
            let r = pcm.resampled(self.rate);
            return self.push_samples(&r.samples);
        }
        self.push_samples(&pcm.samples)
    }

    pub fn push_samples(&mut self, samples: &[f32]) -> Vec<DriveFrame> {
        if self.done || samples.is_empty() {
            return Vec::new();
        }
        self.buf.extend_from_slice(samples);
        self.pushed += samples.len() as u64;
        self.drain(false)
    }

    /// No more audio. Emits every remaining frame, analysing the tail against
    /// the samples that exist rather than waiting for lookahead that will never
    /// arrive — which is exactly what the offline path does at the end of a
    /// buffer, so the two agree on the final frames too.
    pub fn finish(&mut self) -> Vec<DriveFrame> {
        if self.done {
            return Vec::new();
        }
        let out = self.drain(true);
        self.done = true;
        out
    }

    pub fn is_finished(&self) -> bool {
        self.done
    }

    /// Everything emitted so far, as a track.
    pub fn track(&self) -> DriveTrack {
        DriveTrack::from_frames(self.cfg.rate(), self.frames.clone(), self.pushed_ms())
    }

    fn drain(&mut self, flush: bool) -> Vec<DriveFrame> {
        if self.pushed == 0 {
            return Vec::new();
        }
        let fps = self.cfg.rate();
        let total_ms = self.pushed_ms();
        let half_samples = (self.cfg.window_ms.max(1.0) as f64 * 0.5 * self.rate as f64 / 1000.0) as u64;
        let mut out = Vec::new();

        loop {
            let k = self.next_frame;
            let t = frame_time_ms(k, fps);
            if t > total_ms as f64 {
                break;
            }
            let centre = (t * self.rate as f64 / 1000.0) as u64;
            // Without the full trailing half of the window this frame's level
            // would change once more audio arrived, and a frame we might have
            // to take back is not a frame we may emit.
            if !flush && centre + half_samples > self.pushed {
                break;
            }
            let lo = centre.saturating_sub(half_samples).max(self.buf_start);
            let hi = (centre + half_samples).min(self.pushed);
            let db = if lo >= hi {
                -120.0
            } else {
                let a = (lo - self.buf_start) as usize;
                let b = (hi - self.buf_start) as usize;
                to_db(rms(&self.buf[a..b.min(self.buf.len())]))
            };
            let viseme = self.span_at(t);
            let f = self.det.step(t.round() as u32, db, viseme, &self.cfg);
            self.frames.push(f);
            out.push(f);
            self.next_frame += 1;

            // Everything before the next frame's window can go.
            let keep = frame_time_ms(self.next_frame, fps) * self.rate as f64 / 1000.0;
            let keep = (keep as u64).saturating_sub(half_samples);
            if keep > self.buf_start {
                let drop = (keep - self.buf_start) as usize;
                let drop = drop.min(self.buf.len());
                self.buf.drain(..drop);
                self.buf_start += drop as u64;
            }
        }
        out
    }

    fn span_at(&mut self, t_ms: f64) -> Option<Viseme> {
        if self.spans.is_empty() {
            return None;
        }
        let t = t_ms.max(0.0) as u32;
        while self.span_cursor + 1 < self.spans.len() && self.spans[self.span_cursor + 1].start_ms <= t {
            self.span_cursor += 1;
        }
        let s = &self.spans[self.span_cursor];
        if t >= s.start_ms && t < s.end_ms {
            Some(viseme_for(&s.symbol))
        } else {
            Some(Viseme::Sil)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::sine;
    use crate::tts::{FakeTts, SynthParams, Tts};

    fn say(text: &str) -> Synthesis {
        FakeTts::new().synth(text, &SynthParams::default()).unwrap()
    }

    fn say_at(text: &str, volume: f32) -> Synthesis {
        FakeTts::new()
            .synth(text, &SynthParams { volume, ..Default::default() })
            .unwrap()
    }

    /// Mean openness over the frames that are not silence, so a comparison
    /// between two utterances of different length is not really a comparison of
    /// how much padding each had.
    fn mean_voiced(t: &DriveTrack) -> f32 {
        let v: Vec<f32> = t.frames().iter().map(|f| f.openness).filter(|o| *o > 0.02).collect();
        if v.is_empty() {
            return 0.0;
        }
        v.iter().sum::<f32>() / v.len() as f32
    }

    // -- the envelope ------------------------------------------------------

    #[test]
    fn vowels_open_her_mouth_further_than_consonants() {
        // The energy path, deliberately: this is the claim that the envelope
        // tracks the audio, and it must hold with no phoneme help at all.
        let v = DriveTrack::from_pcm(&say("aaaa").pcm, 60);
        let c = DriveTrack::from_pcm(&say("tttt").pcm, 60);
        assert!(
            v.peak_openness() > c.peak_openness() + 0.1,
            "peak {} vs {}",
            v.peak_openness(),
            c.peak_openness()
        );
        assert!(
            mean_voiced(&v) > mean_voiced(&c) + 0.1,
            "mean {} vs {}",
            mean_voiced(&v),
            mean_voiced(&c)
        );
    }

    #[test]
    fn the_envelope_follows_the_text_within_a_single_utterance() {
        // "tatatata" — the vowel frames must beat the consonant frames even
        // when both are normalised against the same reference.
        let s = say("tatatata");
        let t = DriveTrack::from_pcm(&s.pcm, 60);
        // Every 'a' span is a vowel; find the frames inside them.
        let mut vowel = Vec::new();
        let mut cons = Vec::new();
        for f in t.frames() {
            let span = s.phonemes.iter().find(|p| f.t_ms >= p.start_ms && f.t_ms < p.end_ms);
            match span.map(|p| p.symbol.as_str()) {
                Some("ɑ") => vowel.push(f.openness),
                Some("t") => cons.push(f.openness),
                _ => {}
            }
        }
        assert!(!vowel.is_empty() && !cons.is_empty());
        let vm = vowel.iter().sum::<f32>() / vowel.len() as f32;
        let cm = cons.iter().sum::<f32>() / cons.len() as f32;
        assert!(vm > cm + 0.15, "vowel frames {vm} vs consonant frames {cm}");
    }

    #[test]
    fn silence_produces_a_closed_mouth_and_a_loud_vowel_a_wide_one() {
        let quiet = DriveTrack::from_pcm(&Pcm::silence(22_050, 500), 60);
        assert!(!quiet.is_empty());
        assert!(quiet.frames().iter().all(|f| f.openness == 0.0), "{:?}", quiet.frames()[10]);
        assert!(quiet.frames().iter().all(|f| f.viseme == Viseme::Sil));

        let loud = DriveTrack::from_pcm(&say("aaaa").pcm, 60);
        assert!(loud.peak_openness() > 0.9, "{}", loud.peak_openness());
    }

    #[test]
    fn a_whisper_quiet_utterance_still_opens_her_mouth() {
        let normal = say("hello there, how are you");
        let mut whisper = normal.pcm.clone();
        whisper.gain(0.08); // 22 dB down: a real whisper, not a rounding error.

        let a = DriveTrack::from_pcm(&normal.pcm, 60);
        let b = DriveTrack::from_pcm(&whisper, 60);
        assert!(b.peak_openness() > 0.5, "whisper peaked at {}", b.peak_openness());
        // …and the partial normalisation still lets it read as quieter than the
        // full-voice take. Both halves of that sentence are the design.
        assert!(
            mean_voiced(&b) < mean_voiced(&a),
            "{} should be under {}",
            mean_voiced(&b),
            mean_voiced(&a)
        );
    }

    #[test]
    fn a_shout_does_not_sit_pegged_open_for_the_whole_utterance() {
        let normal = DriveTrack::from_pcm(&say_at("hello there, how are you", 1.0).pcm, 60);
        let shout = DriveTrack::from_pcm(&say_at("hello there, how are you", 2.0).pcm, 60);

        let pegged = shout.frames().iter().filter(|f| f.openness > 0.98).count() as f32
            / shout.len() as f32;
        assert!(pegged < 0.35, "{:.0}% of frames were fully open", pegged * 100.0);
        assert!(
            mean_voiced(&shout) - mean_voiced(&normal) < 0.2,
            "shouting moved the mean from {} to {}",
            mean_voiced(&normal),
            mean_voiced(&shout)
        );
    }

    #[test]
    fn attack_is_faster_than_release() {
        // A hard step: silence, a sustained tone, silence.
        let mut p = Pcm::silence(22_050, 200);
        p.append(&sine(22_050, 220.0, 400, 0.6));
        p.append(&Pcm::silence(22_050, 400));
        let t = DriveTrack::from_pcm(&p, 60);
        let peak = t.peak_openness();
        let half = peak * 0.5;

        let rise = t.frames().iter().find(|f| f.openness >= half).map(|f| f.t_ms).unwrap();
        let fall = t
            .frames()
            .iter()
            .find(|f| f.t_ms > 600 && f.openness < half)
            .map(|f| f.t_ms)
            .unwrap();
        let rise_ms = rise as i64 - 200;
        let fall_ms = fall as i64 - 600;
        assert!(rise_ms >= 0 && fall_ms >= 0, "rise {rise_ms} fall {fall_ms}");
        assert!(
            fall_ms > rise_ms * 2,
            "she must close slower than she opens: rose in {rise_ms} ms, fell in {fall_ms} ms"
        );
    }

    #[test]
    fn every_value_is_finite_and_inside_the_unit_range() {
        for text in ["", "a", "hello there, friend.", "→←※", "  ", "..."] {
            let s = say(text);
            for track in [
                DriveTrack::from_synthesis(&s, 60),
                DriveTrack::from_pcm(&s.pcm, 15),
                DriveTrack::from_pcm(&s.pcm, 30),
            ] {
                for f in track.frames() {
                    assert!(f.openness.is_finite() && (0.0..=1.0).contains(&f.openness), "{f:?}");
                    assert!(f.emphasis.is_finite() && (0.0..=1.0).contains(&f.emphasis), "{f:?}");
                }
                for t in [0u32, 1, 7, 100, 5_000, u32::MAX] {
                    let f = track.sample(t);
                    assert!(f.openness.is_finite() && (0.0..=1.0).contains(&f.openness));
                    assert!(f.emphasis.is_finite() && (0.0..=1.0).contains(&f.emphasis));
                }
            }
        }
    }

    #[test]
    fn a_poisoned_buffer_does_not_poison_her_mouth() {
        let p = Pcm::new(22_050, [f32::NAN, f32::INFINITY, 0.5, -0.5, f32::NAN].repeat(2000));
        let t = DriveTrack::from_pcm(&p, 60);
        assert!(t.frames().iter().all(|f| f.openness.is_finite() && f.emphasis.is_finite()));
    }

    // -- emphasis ----------------------------------------------------------

    #[test]
    fn emphasis_is_higher_at_an_onset_than_in_the_middle_of_a_sustained_vowel() {
        let mut p = Pcm::silence(22_050, 300);
        p.append(&sine(22_050, 200.0, 900, 0.6));
        let t = DriveTrack::from_pcm(&p, 60);

        let onset = t
            .frames()
            .iter()
            .filter(|f| (300..380).contains(&f.t_ms))
            .fold(0.0f32, |m, f| m.max(f.emphasis));
        let sustained = t
            .frames()
            .iter()
            .filter(|f| (900..1150).contains(&f.t_ms))
            .fold(0.0f32, |m, f| m.max(f.emphasis));
        assert!(onset > 0.5, "the start of a sound is an event: {onset}");
        assert!(
            sustained < 0.15,
            "a steady vowel is not an event: {sustained} (onset was {onset})"
        );
    }

    #[test]
    fn emphasis_is_not_merely_loudness() {
        // Two tones at very different levels; both are onsets, so both should
        // read as emphatic even though their openness differs a lot.
        let mut quiet = Pcm::silence(22_050, 200);
        quiet.append(&sine(22_050, 200.0, 300, 0.05));
        let mut loud = Pcm::silence(22_050, 200);
        loud.append(&sine(22_050, 200.0, 300, 0.9));

        let a = DriveTrack::from_pcm(&quiet, 60);
        let b = DriveTrack::from_pcm(&loud, 60);
        let peak = |t: &DriveTrack| t.frames().iter().fold(0.0f32, |m, f| m.max(f.emphasis));
        assert!(peak(&a) > 0.5 && peak(&b) > 0.5, "{} vs {}", peak(&a), peak(&b));
    }

    // -- visemes -----------------------------------------------------------

    #[test]
    fn m_closes_the_lips_and_a_opens_wide_and_f_is_a_lip_bite() {
        let m = DriveTrack::from_synthesis(&say("m"), 60);
        assert!(m.frames().iter().any(|f| f.viseme == Viseme::PP));
        assert!(
            m.peak_openness() < 0.02,
            "a bilabial nasal keeps the lips together however loud it is: {}",
            m.peak_openness()
        );

        let a = DriveTrack::from_synthesis(&say("a"), 60);
        assert!(a.frames().iter().any(|f| f.viseme == Viseme::AA));
        assert!(a.peak_openness() > 0.8, "{}", a.peak_openness());

        let f = DriveTrack::from_synthesis(&say("f"), 60);
        assert!(f.frames().iter().any(|x| x.viseme == Viseme::FF));
        assert!(f.peak_openness() < a.peak_openness());
    }

    #[test]
    fn the_espeak_inventory_lands_on_the_shape_a_viewer_would_expect() {
        let cases: &[(&str, Viseme)] = &[
            ("p", Viseme::PP),
            ("b", Viseme::PP),
            ("m", Viseme::PP),
            ("f", Viseme::FF),
            ("v", Viseme::FF),
            ("θ", Viseme::TH),
            ("ð", Viseme::TH),
            ("t", Viseme::DD),
            ("d", Viseme::DD),
            ("s", Viseme::SS),
            ("z", Viseme::SS),
            ("n", Viseme::NN),
            ("l", Viseme::NN),
            ("ŋ", Viseme::NN),
            ("r", Viseme::RR),
            ("ɹ", Viseme::RR),
            ("k", Viseme::KK),
            ("ɡ", Viseme::KK),
            ("g", Viseme::KK),
            ("ʃ", Viseme::CH),
            ("ʒ", Viseme::CH),
            ("tʃ", Viseme::CH),
            ("dʒ", Viseme::CH),
            ("j", Viseme::I),
            ("w", Viseme::U),
            ("h", Viseme::E),
            ("ɑ", Viseme::AA),
            ("a", Viseme::AA),
            ("ɐ", Viseme::AA),
            ("ʌ", Viseme::AA),
            ("æ", Viseme::AA),
            ("ɛ", Viseme::E),
            ("e", Viseme::E),
            ("ə", Viseme::E),
            ("ɜ", Viseme::E),
            ("ɪ", Viseme::I),
            ("i", Viseme::I),
            ("ɔ", Viseme::O),
            ("o", Viseme::O),
            ("ʊ", Viseme::U),
            ("u", Viseme::U),
        ];
        for (sym, want) in cases {
            assert_eq!(viseme_for(sym), *want, "{sym}");
        }
    }

    #[test]
    fn stress_length_and_tie_marks_do_not_change_the_mouth() {
        assert_eq!(viseme_for("ˈɑ"), Viseme::AA);
        assert_eq!(viseme_for("ˌɛ"), Viseme::E);
        assert_eq!(viseme_for("iː"), Viseme::I);
        assert_eq!(viseme_for("ˈuːˑ"), Viseme::U);
        assert_eq!(viseme_for("t͡ʃ"), Viseme::CH, "a tied affricate is still an affricate");
        assert_eq!(viseme_for("d͡ʒ"), Viseme::CH);
        assert_eq!(viseme_for("pʰ"), Viseme::PP);
        assert_eq!(viseme_for("ˈmː"), Viseme::PP);
    }

    #[test]
    fn pauses_are_silence_and_nonsense_is_a_neutral_mouth_rather_than_a_panic() {
        for s in ["_", "", ".", "|", "‖", "   ", "ˈ", "ː"] {
            assert_eq!(viseme_for(s), Viseme::Sil, "{s:?}");
        }
        // Anything we have never seen is much more likely to be a phone than a
        // pause, so it gets a neutral mouth rather than shutting her up.
        for s in ["ʡ", "🙂", "QQQ", "\u{0}", "ǃ", "ʘ"] {
            let _ = viseme_for(s);
        }
        assert_eq!(viseme_for("🙂"), Viseme::E);
    }

    #[test]
    fn arpabet_works_too_even_though_espeak_is_the_engine_we_ship() {
        assert_eq!(viseme_for("AA1"), Viseme::AA);
        assert_eq!(viseme_for("SH"), Viseme::CH);
        assert_eq!(viseme_for("NG"), Viseme::NN);
        assert_eq!(viseme_for("IY0"), Viseme::I);
        assert_eq!(viseme_for("OW2"), Viseme::O);
    }

    #[test]
    fn a_silent_frame_inside_a_vowel_span_still_closes_her() {
        // A span that claims a vowel over audio that is silent for its second
        // half. The engine's timing is a ceiling, not an instruction.
        let mut samples = sine(22_050, 200.0, 200, 0.7).samples;
        samples.extend(std::iter::repeat_n(0.0, 22_050 * 600 / 1000));
        let pcm = Pcm::new(22_050, samples);
        let spans = vec![PhonemeSpan::new("ɑ", 0, 800)];
        let t = DriveTrack::analyse(&pcm, &spans, &LipsyncConfig::at(60));

        let early = t.sample(100);
        let late = t.sample(700);
        assert_eq!(early.viseme, Viseme::AA, "the engine said /ɑ/ and it is still /ɑ/");
        assert!(early.openness > 0.5, "{early:?}");
        assert_eq!(late.viseme, Viseme::AA, "…shaped like /ɑ/ even with the mouth shut");
        assert!(late.openness < 0.05, "no sound means no mouth, span or not: {late:?}");
        // …and the closing is the release, not a cliff.
        assert!(t.sample(260).openness > t.sample(400).openness);
    }

    #[test]
    fn with_no_phonemes_she_still_gets_sil_when_shut_and_a_vowel_when_not() {
        let mut pcm = Pcm::silence(22_050, 300);
        pcm.append(&say("hello there, how are you").pcm);
        pcm.append(&Pcm::silence(22_050, 300));
        let t = DriveTrack::from_pcm(&pcm, 60);
        assert!(t.frames().iter().any(|f| f.viseme == Viseme::Sil));
        assert!(t.frames().iter().any(|f| matches!(f.viseme, Viseme::AA | Viseme::E)));
        // Nothing may claim an articulation the energy path cannot know about.
        assert!(
            t.frames()
                .iter()
                .all(|f| matches!(f.viseme, Viseme::Sil | Viseme::AA | Viseme::E)),
            "the energy path must not invent consonants"
        );
        for f in t.frames() {
            if f.openness < 0.02 {
                assert_eq!(f.viseme, Viseme::Sil, "{f:?}");
            }
        }
    }

    #[test]
    fn out_of_order_and_overlapping_spans_from_a_front_end_never_panic() {
        let pcm = say("hello").pcm;
        let spans = vec![
            PhonemeSpan::new("u", 300, 320),
            PhonemeSpan::new("ɑ", 0, 500),
            PhonemeSpan::new("m", 100, 150),
            PhonemeSpan::new("", 0, 0),
            PhonemeSpan::new("ʃ", 900, 40), // end before start; `new` repairs it
        ];
        let t = DriveTrack::analyse(&pcm, &spans, &LipsyncConfig::at(60));
        assert!(t.frames().iter().all(|f| f.openness.is_finite()));
    }

    // -- sampling ----------------------------------------------------------

    #[test]
    fn sample_interpolates_between_frames_and_clamps_outside_the_track() {
        let t = DriveTrack::from_pcm(&say("hello there").pcm, 15); // 66.7 ms apart
        assert!(t.len() > 4);

        // Somewhere strictly between two analysis frames, the value must sit
        // between them rather than snapping to one.
        let a = t.frames()[2];
        let b = t.frames()[3];
        let mid = (a.t_ms + b.t_ms) / 2;
        let s = t.sample(mid);
        let (lo, hi) = if a.openness <= b.openness {
            (a.openness, b.openness)
        } else {
            (b.openness, a.openness)
        };
        assert!(s.openness >= lo - 1e-4 && s.openness <= hi + 1e-4, "{s:?} between {a:?} {b:?}");
        if (a.openness - b.openness).abs() > 0.02 {
            assert!(s.openness != a.openness && s.openness != b.openness, "it snapped: {s:?}");
        }

        assert_eq!(t.sample(0).t_ms, 0);
        let past = t.sample(t.duration_ms() + 1000);
        assert_eq!(past.openness, 0.0);
        assert_eq!(past.emphasis, 0.0);
        assert_eq!(past.viseme, Viseme::Sil);
        assert_eq!(t.sample(u32::MAX).openness, 0.0);
        assert_eq!(DriveTrack::empty(60).sample(50), DriveFrame::closed(50));
    }

    #[test]
    fn sampling_on_a_frame_time_returns_that_frame() {
        let t = DriveTrack::from_pcm(&say("hello there").pcm, 50); // exactly 20 ms
        for f in t.frames().iter().take(8) {
            let s = t.sample(f.t_ms);
            assert!((s.openness - f.openness).abs() < 1e-4, "{s:?} vs {f:?}");
            assert_eq!(s.viseme, f.viseme);
        }
    }

    // -- concatenation -----------------------------------------------------

    #[test]
    fn appended_clauses_do_not_drift_against_a_single_analysis() {
        let whole = say("one. two. three.");
        let one = DriveTrack::from_synthesis(&whole, 60);

        let mut joined = DriveTrack::empty(60);
        for clause in ["one. ", "two. ", "three."] {
            joined.push(&DriveTrack::from_synthesis(&say(clause), 60));
        }

        assert!(
            (joined.duration_ms() as i64 - one.duration_ms() as i64).abs() <= 4,
            "{} vs {}",
            joined.duration_ms(),
            one.duration_ms()
        );
        assert!((joined.len() as i64 - one.len() as i64).abs() <= 1);

        // The grid itself, which is the thing that would drift.
        for (a, b) in one.frames().iter().zip(joined.frames()) {
            assert_eq!(a.t_ms, b.t_ms, "the frame grid moved");
        }

        let n = one.len().min(joined.len());
        let err: f32 = (0..n)
            .map(|i| (one.frames()[i].openness - joined.frames()[i].openness).abs())
            .sum::<f32>()
            / n as f32;
        assert!(err < 0.15, "mean openness error across the splice was {err}");
    }

    #[test]
    fn a_dozen_clauses_still_end_where_the_audio_ends() {
        let mut joined = DriveTrack::empty(60);
        let mut audio = Pcm::new(22_050, Vec::new());
        for i in 0..12 {
            let s = say(if i % 2 == 0 { "hello there. " } else { "and again, yes. " });
            audio.append(&s.pcm);
            joined.push(&DriveTrack::from_synthesis(&s, 60));
        }
        assert!(
            (joined.duration_ms() as i64 - audio.duration_ms() as i64).abs() <= 12,
            "after twelve clauses: {} vs {} ms of audio",
            joined.duration_ms(),
            audio.duration_ms()
        );
        for (k, f) in joined.frames().iter().enumerate() {
            assert_eq!(f.t_ms, (k as f64 * 1000.0 / 60.0).round() as u32, "frame {k} drifted");
        }
    }

    #[test]
    fn appending_at_an_explicit_time_leaves_a_closed_gap_rather_than_a_hole() {
        let mut t = DriveTrack::from_synthesis(&say("hi."), 60);
        let end = t.duration_ms();
        t.append(&DriveTrack::from_synthesis(&say("hi."), 60), end + 500);
        assert!(t.duration_ms() >= end + 500);
        // The gap is a shut mouth, and it is continuous — no missing frames.
        let gap = t.sample(end + 250);
        assert_eq!(gap.openness, 0.0);
        assert_eq!(gap.viseme, Viseme::Sil);
        for (k, f) in t.frames().iter().enumerate() {
            assert_eq!(f.t_ms, (k as f64 * 1000.0 / 60.0).round() as u32);
        }
    }

    #[test]
    fn appending_across_a_tier_change_re_grids_rather_than_dropping_frames() {
        let mut t = DriveTrack::from_synthesis(&say("hello there."), 60);
        let end = t.duration_ms();
        // The governor dropped her to T2 mid-utterance; the next clause was
        // analysed at 30 fps.
        t.push(&DriveTrack::from_synthesis(&say("and hello again."), 30));
        assert_eq!(t.fps(), 60, "the track keeps its own grid");
        assert!(t.duration_ms() > end);
        assert!(t.sample(end + 200).openness > 0.0, "the second clause is still there");
    }

    #[test]
    fn an_absurd_splice_time_is_capped_rather_than_allocating_an_hour_of_mouth() {
        let mut t = DriveTrack::from_synthesis(&say("hi."), 60);
        t.append(&DriveTrack::from_synthesis(&say("hi."), 60), u32::MAX);
        assert!(t.len() as u64 <= 60 * 3600 + 1, "{} frames", t.len());
        assert!(t.frames().iter().all(|f| f.openness.is_finite()));
    }

    #[test]
    fn appending_nothing_is_harmless() {
        let mut t = DriveTrack::from_synthesis(&say("hi."), 60);
        let before = t.clone();
        t.push(&DriveTrack::empty(60));
        assert_eq!(t.frames(), before.frames());
        let mut e = DriveTrack::empty(60);
        e.push(&DriveTrack::from_synthesis(&say(""), 60));
        assert!(e.is_empty());
    }

    // -- streaming ---------------------------------------------------------

    #[test]
    fn streaming_and_whole_utterance_analysis_agree_on_the_same_audio() {
        let s = say("hello there, how are you today");
        let offline = DriveTrack::analyse(&s.pcm, &s.phonemes, &LipsyncConfig::at(60));

        let mut st = LipsyncStream::new(s.pcm.rate, LipsyncConfig::at(60));
        // Fed in ~20 ms bites, which is smaller than one analysis window, so
        // the lookahead logic is genuinely exercised.
        let bite = (s.pcm.rate as usize) / 50;
        let mut spans_pushed = false;
        for c in s.pcm.samples.chunks(bite) {
            if !spans_pushed {
                spans_pushed = true;
                st.push_synthesis(&Synthesis {
                    text: s.text.clone(),
                    pcm: Pcm::new(s.pcm.rate, c.to_vec()),
                    phonemes: s.phonemes.clone(),
                });
            } else {
                st.push_samples(c);
            }
        }
        st.finish();
        let stream = st.track();

        assert_eq!(stream.len(), offline.len(), "frame counts must match exactly");
        for (a, b) in offline.frames().iter().zip(stream.frames()) {
            assert_eq!(a.t_ms, b.t_ms);
        }
        // The normalisation genuinely differs — one had the whole utterance and
        // one did not — so this is an agreement bound, not equality. Skip the
        // first 300 ms, which is where the adaptive follower is still learning
        // and is *supposed* to disagree.
        let tail: Vec<(f32, f32)> = offline
            .frames()
            .iter()
            .zip(stream.frames())
            .filter(|(a, _)| a.t_ms > 300)
            .map(|(a, b)| (a.openness, b.openness))
            .collect();
        assert!(!tail.is_empty());
        let err = tail.iter().map(|(a, b)| (a - b).abs()).sum::<f32>() / tail.len() as f32;
        assert!(err < 0.1, "streaming drifted from offline by {err} on average");

        // The visemes are not a matter of normalisation and must be identical.
        let same = offline
            .frames()
            .iter()
            .zip(stream.frames())
            .filter(|(a, b)| a.viseme == b.viseme)
            .count();
        assert!(same * 20 >= offline.len() * 19, "{same} of {} visemes agreed", offline.len());
    }

    #[test]
    fn the_stream_emits_frames_before_the_utterance_is_over() {
        let s = say("one two three four five six seven eight");
        let mut st = LipsyncStream::new(s.pcm.rate, LipsyncConfig::at(60));
        let half = s.pcm.samples.len() / 2;
        let early = st.push_samples(&s.pcm.samples[..half]);
        assert!(
            early.len() > 10,
            "she has to move her mouth during clause one, not after clause eight"
        );
        assert!(early.iter().all(|f| f.openness.is_finite()));
        let rest = st.push_samples(&s.pcm.samples[half..]);
        let tail = st.finish();
        assert!(!rest.is_empty());
        // Every frame, exactly once, in order.
        let all: Vec<u32> = early
            .iter()
            .chain(&rest)
            .chain(&tail)
            .map(|f| f.t_ms)
            .collect();
        assert!(all.windows(2).all(|w| w[0] < w[1]), "frames came out out of order");
        assert_eq!(all.len(), st.track().len());
    }

    #[test]
    fn a_stream_carries_its_normalisation_across_clause_boundaries() {
        // Three clauses through one stream: no step in her mouth at a splice
        // that three independently-normalised tracks would show.
        let mut st = LipsyncStream::new(22_050, LipsyncConfig::at(60));
        for clause in ["one. ", "two. ", "three."] {
            st.push_synthesis(&say(clause));
        }
        st.finish();
        let t = st.track();
        assert!(!t.is_empty());
        // No frame-to-frame jump larger than what one attack step can produce.
        let worst = t
            .frames()
            .windows(2)
            .map(|w| (w[1].openness - w[0].openness).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 1.0 && worst.is_finite(), "{worst}");
        assert!(t.frames().iter().all(|f| (0.0..=1.0).contains(&f.openness)));
    }

    #[test]
    fn a_stream_that_is_fed_nothing_produces_nothing_and_does_not_panic() {
        let mut st = LipsyncStream::new(22_050, LipsyncConfig::at(60));
        assert!(st.push_samples(&[]).is_empty());
        assert!(st.finish().is_empty());
        assert!(st.is_finished());
        assert!(st.finish().is_empty(), "finishing twice is not an error");
        assert!(st.track().is_empty());
    }

    #[test]
    fn a_stream_resamples_a_voice_pack_change_instead_of_pitching_her_mouth() {
        let mut st = LipsyncStream::new(22_050, LipsyncConfig::at(60));
        st.push(&say("hello. ").pcm);
        let before = st.pushed_ms();
        let other = FakeTts::at_rate(16_000).synth("goodbye.", &Default::default()).unwrap();
        st.push(&other.pcm);
        st.finish();
        let grew = st.pushed_ms() - before;
        assert!(
            (grew as i64 - other.pcm.duration_ms() as i64).abs() <= 4,
            "{grew} ms added for {} ms of 16 kHz audio",
            other.pcm.duration_ms()
        );
    }

    // -- configuration -----------------------------------------------------

    #[test]
    fn the_frame_rate_is_a_parameter_and_the_shape_survives_it() {
        let s = say("hello there, how are you");
        let fast = DriveTrack::from_pcm(&s.pcm, 60);
        let slow = DriveTrack::from_pcm(&s.pcm, 15);
        assert_eq!(fast.fps(), 60);
        assert_eq!(slow.fps(), 15);
        assert!(fast.len() > slow.len() * 3);
        assert_eq!(fast.duration_ms(), slow.duration_ms());
        // Sampled at the same instants the two must broadly agree — a tier
        // change must not visibly change how open her mouth is.
        let mut err = 0.0f32;
        let mut n = 0;
        let mut t = 0;
        while t < s.pcm.duration_ms() {
            err += (fast.sample(t).openness - slow.sample(t).openness).abs();
            n += 1;
            t += 33;
        }
        assert!(err / (n as f32) < 0.2, "{}", err / n as f32);
    }

    #[test]
    fn a_nonsense_frame_rate_is_clamped_rather_than_dividing_by_zero() {
        let s = say("hi.");
        assert_eq!(DriveTrack::from_pcm(&s.pcm, 0).fps(), 1);
        assert_eq!(DriveTrack::from_pcm(&s.pcm, 100_000).fps(), 240);
        assert!(!DriveTrack::from_pcm(&s.pcm, 0).is_empty());
    }

    #[test]
    fn the_tier_decides_the_analysis_rate() {
        use wisp_proto::Tier;
        assert_eq!(LipsyncConfig::for_tier(Tier::Full).fps, 60);
        assert_eq!(LipsyncConfig::for_tier(Tier::Reduced).fps, 30);
        assert_eq!(LipsyncConfig::for_tier(Tier::Lobotomised).fps, 15);
        // Dormant means she is not speaking at all; it must still be a legal
        // config rather than a division by zero waiting to happen.
        assert_eq!(LipsyncConfig::for_tier(Tier::Dormant).fps, 15);
    }

    #[test]
    fn full_normalisation_and_none_at_all_bracket_the_default() {
        let mut quiet = say("hello there, how are you").pcm;
        quiet.gain(0.08);
        let peak_at = |strength: f32| {
            let cfg = LipsyncConfig { agc_strength: strength, ..LipsyncConfig::at(60) };
            DriveTrack::analyse(&quiet, &[], &cfg).peak_openness()
        };
        let none = peak_at(0.0);
        let default = peak_at(0.55);
        let full = peak_at(1.0);
        assert!(none < default, "no makeup gain leaves a quiet pack mumbling: {none}");
        assert!(default < full, "{default} vs {full}");
        assert!(full > 0.95, "full normalisation should reach the top: {full}");
    }

    #[test]
    fn an_empty_track_answers_every_question_without_panicking() {
        let t = DriveTrack::empty(60);
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.duration_ms(), 0);
        assert_eq!(t.peak_openness(), 0.0);
        assert_eq!(t.mean_openness(), 0.0);
        assert_eq!(t.sample(0), DriveFrame::closed(0));
    }

    #[test]
    fn a_track_round_trips_through_json_for_the_flight_recorder() {
        let t = DriveTrack::from_synthesis(&say("hello there."), 30);
        let j = serde_json::to_string(&t).unwrap();
        let back: DriveTrack = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn viseme_names_round_trip_so_a_skin_file_can_name_them() {
        for v in Viseme::ALL {
            assert_eq!(Viseme::from_name(v.name()), Some(v), "{}", v.name());
        }
        assert_eq!(Viseme::from_name("aa"), Some(Viseme::AA));
        assert_eq!(Viseme::from_name("not a viseme"), None);
    }

    #[test]
    fn the_articulation_ceiling_orders_the_way_a_mouth_does() {
        assert_eq!(Viseme::Sil.target_openness(), 0.0);
        assert_eq!(Viseme::PP.target_openness(), 0.0);
        assert!(Viseme::AA.target_openness() > Viseme::E.target_openness());
        assert!(Viseme::E.target_openness() > Viseme::I.target_openness());
        assert!(Viseme::I.target_openness() > Viseme::SS.target_openness());
        assert!(Viseme::O.target_openness() > Viseme::U.target_openness());
        for v in Viseme::ALL {
            assert!((0.0..=1.0).contains(&v.target_openness()));
            assert!((0.0..=1.0).contains(&v.blend(1.0)));
            assert_eq!(v.blend(0.0), 0.0, "no energy is a closed mouth for every shape");
        }
    }
}


