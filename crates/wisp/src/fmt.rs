//! Turning the internal types into sentences.
//!
//! DESIGN.md §9: English, short, concrete, sentence case, no exclamation marks,
//! no mascot voice, no jargon in operator-facing strings. Errors say what
//! happened *and what to do next*. This module is the one place that voice is
//! implemented, so it can be reviewed in one sitting.
//!
//! Everything here is a pure function of its argument, which is what lets the
//! `explain` output be asserted on in tests rather than eyeballed.

use wisp_proto::{Consent, EventKind, Observation, SenseId, Tier, TierReason, Urgency};

// ---------------------------------------------------------------------------
// Tiers
// ---------------------------------------------------------------------------

/// `T3`. Short, and what the CLI accepts back.
pub fn tier_name(t: Tier) -> &'static str {
    match t {
        Tier::Feral => "T0",
        Tier::Full => "T1",
        Tier::Reduced => "T2",
        Tier::Lobotomised => "T3",
        Tier::Dormant => "T4",
    }
}

/// `T3 Lobotomised`.
pub fn tier_label(t: Tier) -> String {
    format!("{} {}", tier_name(t), tier_word(t))
}

pub fn tier_word(t: Tier) -> &'static str {
    match t {
        Tier::Feral => "Feral",
        Tier::Full => "Full",
        Tier::Reduced => "Reduced",
        Tier::Lobotomised => "Lobotomised",
        Tier::Dormant => "Dormant",
    }
}

/// What the tier means for the operator, in one clause.
pub fn tier_meaning(t: Tier) -> &'static str {
    match t {
        Tier::Feral => "you are away and the machine is quiet, so she may think in the background",
        Tier::Full => "you are here and nothing heavy is running",
        Tier::Reduced => "something substantial started; the deliberate model is out of VRAM",
        Tier::Lobotomised => "a game or a headset owns the GPU; she is canned speech only",
        Tier::Dormant => "she is silent and costs nothing",
    }
}

pub fn parse_tier(s: &str) -> Option<Tier> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t0" | "0" | "feral" => Some(Tier::Feral),
        "t1" | "1" | "full" => Some(Tier::Full),
        "t2" | "2" | "reduced" => Some(Tier::Reduced),
        "t3" | "3" | "lobotomised" | "lobotomized" => Some(Tier::Lobotomised),
        "t4" | "4" | "dormant" => Some(Tier::Dormant),
        _ => None,
    }
}

/// Why she is where she is. The other half of "she is honest".
pub fn reason(r: &TierReason) -> String {
    match r {
        TierReason::Idle => "nothing notable is running".to_string(),
        TierReason::Pinned => "you pinned it".to_string(),
        TierReason::Fullscreen { app_id } => format!("{app_id} is fullscreen"),
        TierReason::HeavyProcess { name } => format!("{name} is running"),
        TierReason::VrSession => "a VR session is streaming".to_string(),
        TierReason::GpuPressure { busy_pct } => {
            format!("the graphics card has been {busy_pct}% busy")
        }
        TierReason::VramPressure { used_mib, total_mib } => {
            format!("VRAM is nearly full ({used_mib} of {total_mib} MiB)")
        }
        TierReason::PowerCritical => "temperature or battery is critical".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Senses and consent
// ---------------------------------------------------------------------------

pub fn consent_word(c: Consent) -> &'static str {
    match c {
        Consent::Ambient => "ambient",
        Consent::Explicit => "explicit",
        Consent::Invasive => "invasive",
    }
}

pub fn sense_label(id: SenseId) -> &'static str {
    wisp_senses::consent::label_of(id)
}

pub fn urgency_word(u: Urgency) -> &'static str {
    match u {
        Urgency::Whim => "whim",
        Urgency::Notable => "notable",
        Urgency::Answer => "answer",
        Urgency::Alarm => "alarm",
    }
}

// ---------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------

/// One line describing what a sense actually saw. Never speculative: it says
/// what is in the observation and nothing that is not.
pub fn observation(o: &Observation) -> String {
    match o {
        Observation::Idle { idle: true, for_ms } => {
            format!("you went idle, {} so far", duration(*for_ms))
        }
        Observation::Idle { idle: false, .. } => "you came back to the keyboard".to_string(),
        Observation::Focus { app_id, title } => {
            format!("focus moved to {app_id} — {}", ellipsise(title, 60))
        }
        Observation::Window { id, gone: true, .. } => format!("window {id} closed"),
        Observation::Window { id, x, y, w, h, .. } => {
            format!("window {id} moved to {x},{y} at {w}x{h}")
        }
        Observation::Media { player, title, artist, playing } => {
            let verb = if *playing { "is playing" } else { "paused" };
            if artist.is_empty() {
                format!("{player} {verb} {}", ellipsise(title, 60))
            } else {
                format!("{player} {verb} {} by {artist}", ellipsise(title, 50))
            }
        }
        Observation::AudioLevel { out, mic_live: true } => {
            format!("output at {out}%, and a microphone is open")
        }
        Observation::AudioLevel { out, mic_live: false } => format!("output at {out}%"),
        Observation::Notification { app, summary, .. } => {
            format!("{app} notified: {}", ellipsise(summary, 60))
        }
        Observation::Vitals { cpu_pct, gpu_pct, vram_used_mib, temp_c, on_battery } => {
            let power = if *on_battery { ", on battery" } else { "" };
            format!("cpu {cpu_pct}%, gpu {gpu_pct}%, vram {vram_used_mib} MiB, {temp_c}C{power}")
        }
        Observation::Workspace { index, name } if name.is_empty() => {
            format!("you switched to desktop {index}")
        }
        Observation::Workspace { index, name } => {
            format!("you switched to desktop {index} ({name})")
        }
        Observation::Files { path, dirty: true } => format!("{path} has uncommitted changes"),
        Observation::Files { path, dirty: false } => format!("{path} is clean"),
        Observation::Speech { text, final_: true } => format!("you said: {}", ellipsise(text, 60)),
        Observation::Speech { .. } => "you are speaking".to_string(),
        Observation::Clipboard { len, kind } => format!("you copied {len} bytes of {kind}"),
        Observation::Fleet { app, field, value } => format!("{app} reported {field} = {value}"),
    }
}

/// One line for any event, for `wisp log`.
pub fn event(kind: &EventKind) -> String {
    match kind {
        EventKind::Sensed(o) => observation(o),
        EventKind::TierChanged { from, to, reason: r } => format!(
            "{} -> {} because {}",
            tier_name(*from),
            tier_label(*to),
            reason(r)
        ),
        EventKind::Proposed(u) => {
            format!("proposed ({}): {}", urgency_word(u.urgency), quote(&u.text))
        }
        EventKind::Said { text } => format!("said: {}", quote(text)),
        EventKind::Dropped { text, why } => format!("dropped {} — {why}", quote(text)),
        EventKind::ToolCall { name, args, ok } => {
            let verdict = if *ok { "ok" } else { "failed" };
            format!("tool {name}({}) {verdict}", ellipsise(args, 40))
        }
        EventKind::Deferred { what, queued } => {
            format!("deferred {} — {queued} waiting", ellipsise(what, 50))
        }
        EventKind::Replayed { what, dropped: true } => {
            format!("dropped as stale: {}", ellipsise(what, 50))
        }
        EventKind::Replayed { what, dropped: false } => {
            format!("replayed {}", ellipsise(what, 50))
        }
        EventKind::Model { name, loaded: true, vram_mib } => {
            format!("loaded {name} ({vram_mib} MiB of VRAM)")
        }
        EventKind::Model { name, loaded: false, .. } => format!("evicted {name}"),
        EventKind::InvasiveActive { sense, active: true } => {
            format!("{} is live", sense_label(*sense))
        }
        EventKind::InvasiveActive { sense, active: false } => {
            format!("{} stopped", sense_label(*sense))
        }
    }
}

/// The short tag `wisp log --kind` filters on.
pub fn event_tag(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Sensed(_) => "sensed",
        EventKind::TierChanged { .. } => "tier",
        EventKind::Proposed(_) => "proposed",
        EventKind::Said { .. } => "said",
        EventKind::Dropped { .. } => "dropped",
        EventKind::ToolCall { .. } => "tool",
        EventKind::Deferred { .. } => "deferred",
        EventKind::Replayed { .. } => "replayed",
        EventKind::Model { .. } => "model",
        EventKind::InvasiveActive { .. } => "invasive",
    }
}

// ---------------------------------------------------------------------------
// Numbers and time
// ---------------------------------------------------------------------------

/// `1.5 s`, `2m 04s`, `1h 12m`. Never a bare millisecond count above a second.
pub fn duration(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{:.1} s", ms as f64 / 1000.0);
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m {:02}s", mins, secs % 60);
    }
    format!("{}h {:02}m", mins / 60, mins % 60)
}

/// Monotonic milliseconds as an offset into the run: `+01:23:45.678`.
///
/// The flight recorder stores time as milliseconds since process start (SPEC
/// §3.2 — never wall-clock, so a suspend or an NTP step cannot reorder it), and
/// this is how that reads.
pub fn since_start(ms: u64) -> String {
    let total = ms / 1_000;
    format!("+{:02}:{:02}:{:02}.{:03}", total / 3600, (total / 60) % 60, total % 60, ms % 1000)
}

/// Local wall-clock `HH:MM:SS` from epoch milliseconds. Display only.
pub fn wall_clock(epoch_ms: u64) -> String {
    let t = (epoch_ms / 1000) as libc::time_t;
    // SAFETY: `localtime_r` writes into a zeroed `tm` we own; nothing escapes.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "--:--:--".to_string();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

pub fn bytes(n: u64) -> String {
    const K: u64 = 1024;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KiB", n as f64 / K as f64)
    } else if n < K * K * K {
        format!("{:.1} MiB", n as f64 / (K * K) as f64)
    } else {
        format!("{:.2} GiB", n as f64 / (K * K * K) as f64)
    }
}

/// Hundredths of a percent of one core, as a percentage.
pub fn cpu(centi_pct: u32) -> String {
    if centi_pct < 100 {
        format!("{:.2}%", centi_pct as f32 / 100.0)
    } else {
        format!("{:.1}%", centi_pct as f32 / 100.0)
    }
}

pub fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// `  label   value`, aligned. Used by `status` and `doctor`.
pub fn row(label: &str, value: &str, width: usize) -> String {
    format!("  {label:<width$}  {value}")
}

pub fn quote(s: &str) -> String {
    format!("\u{201c}{}\u{201d}", ellipsise(s, 90))
}

/// Cut on a character boundary, never mid-codepoint.
pub fn ellipsise(s: &str, max: usize) -> String {
    let s = s.replace(['\n', '\r'], " ");
    if s.chars().count() <= max {
        return s;
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}\u{2026}", keep.trim_end())
}

/// A section heading. No decoration beyond the blank line before it.
pub fn heading(s: &str) -> String {
    format!("{s}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_proto::Utterance;

    #[test]
    fn every_tier_name_parses_back_to_itself() {
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            assert_eq!(parse_tier(tier_name(t)), Some(t));
            assert_eq!(parse_tier(tier_word(t)), Some(t));
            assert!(!tier_meaning(t).is_empty());
        }
        assert_eq!(parse_tier("nonsense"), None);
    }

    #[test]
    fn durations_read_like_english() {
        assert_eq!(duration(400), "400 ms");
        assert_eq!(duration(1_500), "1.5 s");
        assert_eq!(duration(124_000), "2m 04s");
        assert_eq!(duration(4_320_000), "1h 12m");
    }

    #[test]
    fn since_start_is_stable_and_ordered() {
        assert_eq!(since_start(0), "+00:00:00.000");
        assert_eq!(since_start(3_723_456), "+01:02:03.456");
        assert!(since_start(1_000) < since_start(2_000), "string order must match time order");
    }

    #[test]
    fn ellipsise_never_splits_a_codepoint() {
        let s = "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}";
        let cut = ellipsise(s, 3);
        assert_eq!(cut.chars().count(), 3);
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(ellipsise("short", 40), "short");
        assert_eq!(ellipsise("two\nlines", 40), "two lines");
    }

    /// DESIGN.md §9, enforced rather than remembered.
    #[test]
    fn nothing_this_module_writes_shouts_or_is_cute() {
        let mut lines: Vec<String> = Vec::new();
        for t in [Tier::Feral, Tier::Full, Tier::Reduced, Tier::Lobotomised, Tier::Dormant] {
            lines.push(tier_label(t));
            lines.push(tier_meaning(t).to_string());
        }
        for r in [
            TierReason::Idle,
            TierReason::Pinned,
            TierReason::VrSession,
            TierReason::PowerCritical,
            TierReason::Fullscreen { app_id: "steam_app_1".into() },
            TierReason::HeavyProcess { name: "cargo".into() },
            TierReason::GpuPressure { busy_pct: 80 },
            TierReason::VramPressure { used_mib: 20, total_mib: 24 },
        ] {
            lines.push(reason(&r));
        }
        for o in sample_observations() {
            lines.push(observation(&o));
        }
        for k in sample_kinds() {
            lines.push(event(&k));
        }
        for l in &lines {
            assert!(!l.contains('!'), "no exclamation marks: {l:?}");
            assert!(!l.is_empty());
            // These are clauses, not sentences: the caller decides where they
            // sit in a line and supplies the punctuation. A trailing full stop
            // here would produce "…because you pinned it.." in `explain`.
            assert!(!l.ends_with('.'), "clauses do not carry their own full stop: {l:?}");
            // Sentence case, with the obvious carve-out for the two things
            // that are names: an acronym or tier token ("VRAM is nearly full",
            // "T1 Full") and a sense's own label ("Clipboard is live").
            let first = l.split_whitespace().next().unwrap_or("");
            let acronym = first.chars().all(|c| !c.is_alphabetic() || c.is_uppercase());
            let a_label = wisp_senses::ALL_SENSES
                .iter()
                .any(|&id| sense_label(id).starts_with(first));
            assert!(
                !first.starts_with(char::is_uppercase) || acronym || a_label,
                "clauses are sentence case unless they start with a name: {l:?}"
            );
        }
    }

    #[test]
    fn every_event_kind_renders_and_has_a_distinct_tag() {
        let mut tags = std::collections::BTreeSet::new();
        for k in sample_kinds() {
            let line = event(&k);
            assert!(!line.is_empty(), "{k:?} renders as nothing");
            tags.insert(event_tag(&k));
        }
        // One tag per variant of EventKind. If proto gains one, this fails.
        assert_eq!(tags.len(), 10, "{tags:?}");
    }

    #[test]
    fn every_observation_variant_renders() {
        // Ten variants of Observation carry a sense; Window and Files are the
        // two that share ids with another. All twelve must render.
        assert_eq!(sample_observations().len(), 12);
        for o in sample_observations() {
            let line = observation(&o);
            assert!(line.len() > 3, "{o:?} renders as {line:?}");
        }
    }

    fn sample_observations() -> Vec<Observation> {
        vec![
            Observation::Idle { idle: true, for_ms: 65_000 },
            Observation::Focus { app_id: "org.kde.kate".into(), title: "lib.rs".into() },
            Observation::Window { id: 7, x: 10, y: 20, w: 800, h: 600, gone: false },
            Observation::Media {
                player: "vlc".into(),
                title: "a song".into(),
                artist: "somebody".into(),
                playing: true,
            },
            Observation::AudioLevel { out: 40, mic_live: true },
            Observation::Notification {
                app: "kmail".into(),
                summary: "one new message".into(),
                body: "".into(),
            },
            Observation::Vitals {
                cpu_pct: 12,
                gpu_pct: 3,
                vram_used_mib: 512,
                temp_c: 45,
                on_battery: false,
            },
            Observation::Workspace { index: 2, name: "code".into() },
            Observation::Files { path: "/home/x/proj".into(), dirty: true },
            Observation::Speech { text: "hello".into(), final_: true },
            Observation::Clipboard { len: 42, kind: "text/plain".into() },
            Observation::Fleet { app: "pulsenx".into(), field: "hr".into(), value: "72".into() },
        ]
    }

    fn sample_kinds() -> Vec<EventKind> {
        vec![
            EventKind::Sensed(Observation::Idle { idle: false, for_ms: 0 }),
            EventKind::TierChanged {
                from: Tier::Full,
                to: Tier::Lobotomised,
                reason: TierReason::VrSession,
            },
            EventKind::Proposed(Utterance::new("a thought", Urgency::Whim)),
            EventKind::Said { text: "a thought".into() },
            EventKind::Dropped { text: "a thought".into(), why: "stale".into() },
            EventKind::ToolCall { name: "nx_list".into(), args: "{}".into(), ok: true },
            EventKind::Deferred { what: "a summary".into(), queued: 3 },
            EventKind::Replayed { what: "a summary".into(), dropped: false },
            EventKind::Model { name: "reflex".into(), loaded: true, vram_mib: 1200 },
            EventKind::InvasiveActive { sense: SenseId::Clipboard, active: true },
        ]
    }
}
