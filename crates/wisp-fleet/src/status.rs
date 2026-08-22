//! Her own status on the bus, and the rate discipline that keeps it truthful.
//!
//! **The rule that matters** (learned the hard way in NX Hub, and re-learned in
//! PulseNX): the bus accepts 4 status messages per second and **drops the
//! excess silently** — no error, no disconnect, the update simply never
//! happened. A one-shot terminal update like `connected:false` has no later
//! sample to re-trigger it, so a client that sprays updates can leave every
//! other app on the machine reading stale state about it forever.
//!
//! So: **≤ 1/s, change-only, with a trailing flush.** At most one message per
//! second, carrying only the keys that actually changed, and a value that
//! arrives during the quiet second is *delayed*, never dropped.
//!
//! [`Throttle`] is a pure state machine — no clock, no socket — so all of that
//! is testable without a hub.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, Serializer};

/// The bus's caps (PROTOCOL.md §4 / connector/server.js).
pub const MAX_STATUS_BYTES: usize = 2048;
pub const MAX_STATUS_KEYS: usize = 64;
/// One per second. The bus allows four; the head room is deliberate.
pub const MIN_STATUS_INTERVAL_MS: u64 = 1000;

/// A status value. The bus takes flat scalars only.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Field {
    Bool(bool),
    /// Send a number as a number: the hub draws a sparkline for numeric fields
    /// and drops the history if a key changes type mid-run (PROTOCOL.md §4).
    Num(f64),
    Str(String),
    /// "unknown" — there is no way to delete a key short of reconnecting.
    Null,
}

/// Written by hand for one reason: an integral value must go out as `72`, not
/// `72.0`. Both are legal JSON, but the hub keeps a sparkline per numeric field
/// and the operator reads these values on a card — `72.0 bpm` is wrong-looking
/// in a way that a `#[derive]` would have quietly shipped.
impl Serialize for Field {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Field::Bool(b) => s.serialize_bool(*b),
            Field::Num(n) if n.fract() == 0.0 && n.abs() < 9e15 => s.serialize_i64(*n as i64),
            Field::Num(n) => s.serialize_f64(*n),
            Field::Str(v) => s.serialize_str(v),
            Field::Null => s.serialize_none(),
        }
    }
}

impl PartialEq for Field {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Field::Bool(a), Field::Bool(b)) => a == b,
            // Bit equality, so a NaN that somehow got in compares equal to
            // itself and does not re-send forever.
            (Field::Num(a), Field::Num(b)) => a.to_bits() == b.to_bits(),
            (Field::Str(a), Field::Str(b)) => a == b,
            (Field::Null, Field::Null) => true,
            _ => false,
        }
    }
}

impl From<bool> for Field {
    fn from(v: bool) -> Self {
        Field::Bool(v)
    }
}
impl From<&str> for Field {
    fn from(v: &str) -> Self {
        Field::Str(v.to_string())
    }
}
impl From<String> for Field {
    fn from(v: String) -> Self {
        Field::Str(v)
    }
}
macro_rules! from_num {
    ($($t:ty),*) => {$(
        impl From<$t> for Field {
            fn from(v: $t) -> Self {
                let f = v as f64;
                if f.is_finite() { Field::Num(f) } else { Field::Null }
            }
        }
    )*};
}
from_num!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

impl Field {
    pub fn as_str(&self) -> String {
        match self {
            Field::Bool(b) => b.to_string(),
            Field::Num(n) => fmt_num(*n),
            Field::Str(s) => s.clone(),
            Field::Null => "null".into(),
        }
    }
}

/// `12.0` prints as `12` so change detection over the wire is stable.
pub fn fmt_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n}");
        s
    }
}

/// A status snapshot. Ordered so the serialisation — and therefore the byte
/// cap check — is deterministic.
pub type Fields = BTreeMap<String, Field>;

/// Build a `Fields` map without ceremony.
#[macro_export]
macro_rules! fields {
    ($($k:expr => $v:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut m = $crate::status::Fields::new();
        $( m.insert(String::from($k), $crate::status::Field::from($v)); )*
        m
    }};
}

pub fn encoded_len(fields: &Fields) -> usize {
    serde_json::to_string(fields).map(|s| s.len()).unwrap_or(usize::MAX)
}

/// Would the bus accept this, merged onto what it already holds?
pub fn fits(merged: &Fields) -> bool {
    merged.len() <= MAX_STATUS_KEYS && encoded_len(merged) <= MAX_STATUS_BYTES
}

/// What [`Throttle::decide`] concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Send exactly these keys now. Only the changed ones — the hub merges.
    Send(Fields),
    /// Something changed but the second is not up yet. Come back in this many
    /// milliseconds and decide again; the *latest* pending value is used then,
    /// so a burst collapses into one message carrying the newest state.
    Wait(u64),
    /// Nothing to say.
    Idle,
}

/// Change-only, ≤ 1/s, trailing-flush status discipline. Pure.
#[derive(Debug, Clone)]
pub struct Throttle {
    min_interval_ms: u64,
    /// What the hub is believed to hold for us, i.e. the merge of everything
    /// we have sent since the current connection said `welcome`.
    sent: Fields,
    sent_at: Option<u64>,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(MIN_STATUS_INTERVAL_MS)
    }
}

impl Throttle {
    pub fn new(min_interval_ms: u64) -> Self {
        Self { min_interval_ms, sent: Fields::new(), sent_at: None }
    }

    /// A fresh connection: the hub starts our slot empty, so the whole status
    /// has to be restated (PROTOCOL.md §8) and the clock starts again.
    pub fn reset(&mut self) {
        self.sent.clear();
        self.sent_at = None;
    }

    /// What the hub currently believes about us.
    pub fn mirror(&self) -> &Fields {
        &self.sent
    }

    pub fn decide(&self, pending: &Fields, now_ms: u64) -> Decision {
        let mut delta = Fields::new();
        for (k, v) in pending {
            if self.sent.get(k) != Some(v) {
                delta.insert(k.clone(), v.clone());
            }
        }
        if delta.is_empty() {
            return Decision::Idle;
        }
        match self.sent_at {
            Some(at) => {
                let due = at.saturating_add(self.min_interval_ms);
                if now_ms < due {
                    Decision::Wait(due - now_ms)
                } else {
                    Decision::Send(delta)
                }
            }
            // First message of a connection goes immediately.
            None => Decision::Send(delta),
        }
    }

    /// Record what actually made it onto the wire.
    pub fn on_sent(&mut self, sent: &Fields, now_ms: u64) {
        for (k, v) in sent {
            self.sent.insert(k.clone(), v.clone());
        }
        self.sent_at = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_update_goes_out_immediately() {
        let t = Throttle::default();
        let want = fields! { "hr" => 72u32, "connected" => true };
        assert_eq!(t.decide(&want, 0), Decision::Send(want));
    }

    #[test]
    fn only_changed_keys_are_sent() {
        let mut t = Throttle::default();
        let first = fields! { "hr" => 72u32, "connected" => true };
        t.on_sent(&first, 0);
        let second = fields! { "hr" => 75u32, "connected" => true };
        match t.decide(&second, 1000) {
            Decision::Send(delta) => {
                assert_eq!(delta.len(), 1);
                assert_eq!(delta.get("hr"), Some(&Field::Num(75.0)));
            }
            other => panic!("expected a delta, got {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_status_says_nothing_at_all() {
        let mut t = Throttle::default();
        let f = fields! { "tier" => "T1" };
        t.on_sent(&f, 0);
        assert_eq!(t.decide(&f, 10_000), Decision::Idle);
    }

    #[test]
    fn a_burst_inside_one_second_waits_rather_than_dropping() {
        let mut t = Throttle::default();
        t.on_sent(&fields! { "hr" => 70u32 }, 0);
        assert_eq!(t.decide(&fields! { "hr" => 71u32 }, 200), Decision::Wait(800));
        // …and the value that eventually goes out is the newest one, because
        // the caller re-decides with whatever `pending` holds at flush time.
        match t.decide(&fields! { "hr" => 99u32 }, 1000) {
            Decision::Send(d) => assert_eq!(d.get("hr"), Some(&Field::Num(99.0))),
            other => panic!("expected send, got {other:?}"),
        }
    }

    #[test]
    fn reconnect_restates_everything() {
        let mut t = Throttle::default();
        let f = fields! { "hr" => 70u32, "connected" => true };
        t.on_sent(&f, 0);
        t.reset();
        assert_eq!(t.decide(&f, 10), Decision::Send(f));
    }

    #[test]
    fn caps_match_the_bus() {
        let mut f = Fields::new();
        for i in 0..64 {
            f.insert(format!("k{i}"), Field::Num(i as f64));
        }
        assert!(fits(&f));
        f.insert("one_too_many".into(), Field::Bool(true));
        assert!(!fits(&f));

        let big = fields! { "x" => "y".repeat(3000) };
        assert!(!fits(&big));
    }
}
