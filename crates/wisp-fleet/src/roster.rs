//! F44, the other half — seeing the rest of the fleet.
//!
//! **The bus has no subscribe verb.** A connector client can publish its own
//! status and nothing else; `getClients()` lives inside the hub's main process.
//! The hub's own out-of-process readers (`nx status`) solve this by reading the
//! snapshot the hub mirrors to disk, and so do we:
//!
//! `<data>/connector-clients.json` — `{ts, clients:[{app, version, pid, since,
//! lastSeen, fields, caps, history}]}`, written atomically, debounced to ~1/s,
//! and re-stamped every 60 s. Anything older than two minutes means the hub is
//! not running (`nx-hub/src/main/ipc.js`).
//!
//! Polling a file the hub already maintains costs a `stat` per tick and cannot
//! perturb the bus. The diff below turns it into [`Observation::Fleet`] — one
//! per field that actually changed, which is what the narrator wants and what
//! keeps the flight recorder readable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use wisp_proto::Observation;

use crate::hub::SNAPSHOT_MAX_AGE_MS;
use crate::status::fmt_num;

/// Synthetic field: an app appeared on, or vanished from, the bus.
pub const FIELD_PRESENT: &str = "present";

/// How many consecutive missing/stale reads before we believe the apps are
/// gone. The snapshot is written tmp+rename, so a single unreadable poll is a
/// race, not an outage.
const GONE_AFTER: u32 = 3;

#[derive(Debug, Deserialize)]
struct Snapshot {
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    clients: Vec<Client>,
}

#[derive(Debug, Deserialize)]
struct Client {
    app: String,
    #[serde(default)]
    fields: BTreeMap<String, serde_json::Value>,
}

/// Diffs successive snapshots into observations.
#[derive(Debug)]
pub struct RosterWatcher {
    path: PathBuf,
    /// Our own app id, so she never narrates herself back to herself.
    me: String,
    seen: BTreeMap<String, BTreeMap<String, String>>,
    misses: u32,
    max_age_ms: u64,
}

impl RosterWatcher {
    pub fn new(path: impl Into<PathBuf>, me: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            me: me.into().to_lowercase(),
            seen: BTreeMap::new(),
            misses: 0,
            max_age_ms: SNAPSHOT_MAX_AGE_MS,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the snapshot and return what changed since last time.
    pub fn poll(&mut self) -> Vec<Observation> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => self.ingest(&raw, now_epoch_ms()),
            Err(_) => self.miss(),
        }
    }

    /// The pure half: a snapshot's text plus the current wall-clock, in.
    /// (Wall clock only to compare against the hub's ISO stamp — ordering
    /// inside wisp is always monotonic.)
    pub fn ingest(&mut self, raw: &str, now_epoch_ms: u64) -> Vec<Observation> {
        let Ok(snap) = serde_json::from_str::<Snapshot>(raw) else {
            return self.miss();
        };
        if let Some(age) = snap.ts.as_deref().and_then(|ts| age_ms(ts, now_epoch_ms)) {
            if age > self.max_age_ms {
                // A stale snapshot means the hub died without cleaning up.
                return self.miss();
            }
        }
        self.misses = 0;

        let mut out = Vec::new();
        let mut present: BTreeSet<String> = BTreeSet::new();

        for client in &snap.clients {
            let app = client.app.to_lowercase();
            if app == self.me {
                continue;
            }
            present.insert(app.clone());
            let is_new = !self.seen.contains_key(&app);
            let known = self.seen.entry(app.clone()).or_default();
            if is_new {
                out.push(fleet(&app, FIELD_PRESENT, "true"));
            }
            for (key, value) in &client.fields {
                let text = scalar(value);
                if known.get(key).map(String::as_str) != Some(text.as_str()) {
                    known.insert(key.clone(), text.clone());
                    out.push(fleet(&app, key, &text));
                }
            }
        }

        let vanished: Vec<String> =
            self.seen.keys().filter(|a| !present.contains(*a)).cloned().collect();
        for app in vanished {
            self.seen.remove(&app);
            out.push(fleet(&app, FIELD_PRESENT, "false"));
        }
        out
    }

    /// A snapshot we could not read or trust.
    fn miss(&mut self) -> Vec<Observation> {
        self.misses = self.misses.saturating_add(1);
        if self.misses < GONE_AFTER {
            return Vec::new();
        }
        let gone: Vec<String> = self.seen.keys().cloned().collect();
        self.seen.clear();
        gone.into_iter().map(|app| fleet(&app, FIELD_PRESENT, "false")).collect()
    }

    /// Forget everything without emitting: used when the watcher is shut down
    /// by the governor, so the next upgrade re-reports the world as it is then
    /// rather than replaying a stale diff.
    pub fn forget(&mut self) {
        self.seen.clear();
        self.misses = 0;
    }
}

fn fleet(app: &str, field: &str, value: &str) -> Observation {
    Observation::Fleet { app: app.to_string(), field: field.to_string(), value: value.to_string() }
}

/// One status value as text. Numbers keep an integral look (`72`, not `72.0`)
/// so the narrator's thresholds and the flight recorder agree.
fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.as_f64().map(fmt_num).unwrap_or_else(|| n.to_string()),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Age of an ISO-8601 `ts` in ms, or `None` if it is not one we understand.
/// Deliberately minimal: the hub writes `new Date().toISOString()`, which is
/// always `YYYY-MM-DDTHH:MM:SS.sssZ`.
fn age_ms(ts: &str, now_epoch_ms: u64) -> Option<u64> {
    let epoch = parse_iso8601_utc(ts)?;
    Some(now_epoch_ms.saturating_sub(epoch))
}

fn parse_iso8601_utc(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| ts[a..b].parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let ms = if bytes.len() >= 23 && bytes[19] == b'.' { num(20, 23)? } else { 0 };
    // Days since the civil epoch (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(secs * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ts: &str, body: &str) -> String {
        format!("{{\"ts\":\"{ts}\",\"clients\":{body}}}")
    }

    const NOW: &str = "2026-08-22T10:00:00.000Z";
    fn now_ms() -> u64 {
        parse_iso8601_utc(NOW).unwrap()
    }

    #[test]
    fn iso_parses_against_a_known_instant() {
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601_utc("2026-08-22T00:00:00.000Z"), Some(1_787_356_800_000));
    }

    #[test]
    fn first_sight_reports_presence_and_every_field() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        let obs = w.ingest(
            &snap(NOW, r#"[{"app":"pulsenx","fields":{"hr":72,"connected":true}}]"#),
            now_ms(),
        );
        assert_eq!(obs.len(), 3);
        assert!(obs.contains(&fleet("pulsenx", FIELD_PRESENT, "true")));
        assert!(obs.contains(&fleet("pulsenx", "hr", "72")));
        assert!(obs.contains(&fleet("pulsenx", "connected", "true")));
    }

    #[test]
    fn only_changes_are_reported() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        let body = r#"[{"app":"pulsenx","fields":{"hr":72,"connected":true}}]"#;
        w.ingest(&snap(NOW, body), now_ms());
        assert!(w.ingest(&snap(NOW, body), now_ms()).is_empty());
        let obs = w.ingest(
            &snap(NOW, r#"[{"app":"pulsenx","fields":{"hr":88,"connected":true}}]"#),
            now_ms(),
        );
        assert_eq!(obs, vec![fleet("pulsenx", "hr", "88")]);
    }

    #[test]
    fn she_never_narrates_herself() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        let obs =
            w.ingest(&snap(NOW, r#"[{"app":"nx-wisp","fields":{"tier":"T1"}}]"#), now_ms());
        assert!(obs.is_empty());
    }

    #[test]
    fn an_app_that_leaves_is_reported_once() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        w.ingest(&snap(NOW, r#"[{"app":"wivrn-nx","fields":{"session":true}}]"#), now_ms());
        let obs = w.ingest(&snap(NOW, "[]"), now_ms());
        assert_eq!(obs, vec![fleet("wivrn-nx", FIELD_PRESENT, "false")]);
        assert!(w.ingest(&snap(NOW, "[]"), now_ms()).is_empty());
    }

    #[test]
    fn a_stale_snapshot_means_the_hub_died_but_only_after_three_misses() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        w.ingest(&snap(NOW, r#"[{"app":"pulsenx","fields":{"hr":72}}]"#), now_ms());
        let old = snap("2026-08-22T09:00:00.000Z", r#"[{"app":"pulsenx","fields":{"hr":72}}]"#);
        assert!(w.ingest(&old, now_ms()).is_empty());
        assert!(w.ingest(&old, now_ms()).is_empty());
        assert_eq!(w.ingest(&old, now_ms()), vec![fleet("pulsenx", FIELD_PRESENT, "false")]);
    }

    #[test]
    fn a_torn_read_does_not_flap() {
        let mut w = RosterWatcher::new("/nonexistent", "nx-wisp");
        let body = r#"[{"app":"pulsenx","fields":{"hr":72}}]"#;
        w.ingest(&snap(NOW, body), now_ms());
        assert!(w.ingest("{ not json", now_ms()).is_empty());
        assert!(w.ingest(&snap(NOW, body), now_ms()).is_empty(), "nothing changed underneath");
    }

    #[test]
    fn no_hub_at_all_is_silence() {
        let mut w = RosterWatcher::new("/definitely/not/here.json", "nx-wisp");
        for _ in 0..10 {
            assert!(w.poll().is_empty());
        }
    }
}
