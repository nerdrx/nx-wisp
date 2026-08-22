//! F45 — the wisp becomes the face of the fleet.
//!
//! A fleet event is `{app, field, value}` and an [`Utterance`] is text plus an
//! [`Urgency`]. The mapping between them is **data**, not `match` arms: a rule
//! file names the app, the field, the condition, what she says and how badly
//! she wants to say it. Teaching her about a new NX app is then a JSON edit —
//! `~/.config/nx-wisp/fleet-rules.json` overrides the authored defaults
//! wholesale — and never a recompile.
//!
//! The authored defaults live in `rules.json` next to this file and cover the
//! four the plan calls for: NX Sentry trips (Alarm), NX Hub has an update
//! (Notable), PulseNX heart-rate spikes (Notable, and sparing), a WiVRn session
//! starting (she waves goodbye and goes quiet — the governor drops her to T3 on
//! its own, this is only the social half).
//!
//! Urgency discipline, because it is the whole point of SPEC §3.4: `Alarm` is
//! free and breaks flow, so exactly one authored rule uses it. Vitals are
//! `Notable` with a long cooldown — ambient, not an emergency. Chatter is
//! `Whim` and gets dropped by `wisp-attn` whenever she is busy.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use serde::Deserialize;
use wisp_proto::{Observation, Urgency, Utterance};

/// The authored mapping, compiled in so she is never mute on a fresh install.
pub const DEFAULT_RULES: &str = include_str!("rules.json");

/// How much history a `rises` rule can look back over, and how many samples we
/// are willing to hold per field.
const MAX_SAMPLES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    pub version: u32,
    /// Display names, so `{app}` reads as "NX Sentry", not "nx-sentry".
    #[serde(default)]
    pub apps: HashMap<String, AppInfo>,
    /// Documentation for whoever edits the file — JSON has no comments.
    #[serde(default)]
    pub notes: Vec<String>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    pub name: String,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UrgencyName {
    Whim,
    Notable,
    Answer,
    Alarm,
}

impl From<UrgencyName> for Urgency {
    fn from(u: UrgencyName) -> Self {
        match u {
            UrgencyName::Whim => Urgency::Whim,
            UrgencyName::Notable => Urgency::Notable,
            UrgencyName::Answer => Urgency::Answer,
            UrgencyName::Alarm => Urgency::Alarm,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// App id, or absent / `"*"` for "any app on the bus".
    #[serde(default)]
    pub app: Option<String>,
    pub field: String,
    pub when: When,
    pub urgency: UrgencyName,
    /// `{app}`, `{value}`, `{prev}`, `{field}` and `{delta}` are substituted.
    pub text: String,
    #[serde(default)]
    pub expression: Option<String>,
    /// Do not fire this rule for this app again for this long.
    #[serde(default)]
    pub cooldown_ms: Option<u64>,
    /// Don't even consider saying it before now + this.
    #[serde(default)]
    pub defer_ms: Option<u64>,
    /// Drop it unsaid after now + this. A fleet fact rots quickly.
    #[serde(default)]
    pub stale_after_ms: Option<u64>,
}

/// The conditions a rule may test. Adding one is a code change *here*; using
/// them to describe a new app is not.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum When {
    /// Any change of value at all.
    Changed,
    /// Changed to something that is not empty, `null`, `none` or `false`.
    Nonempty,
    IsTrue,
    IsFalse,
    Eq { value: String },
    OneOf { values: Vec<String> },
    Above { value: f64 },
    Below { value: f64 },
    /// Was at or below the threshold, now above it. Fires on the crossing only,
    /// which is what stops a hovering value from nagging.
    CrossesAbove { value: f64 },
    CrossesBelow { value: f64 },
    /// Rose by at least `by` compared with the lowest sample inside
    /// `window_ms`. This is the spike detector.
    Rises { by: f64, window_ms: u64 },
}

#[derive(Debug, Default)]
struct FieldState {
    last: Option<String>,
    samples: VecDeque<(u64, f64)>,
}

/// Turns fleet observations into things she might say.
#[derive(Debug)]
pub struct Narrator {
    rules: RuleSet,
    state: HashMap<(String, String), FieldState>,
    fired: HashMap<(String, String), u64>,
    min_urgency: Urgency,
}

impl Default for Narrator {
    fn default() -> Self {
        Self::new(Self::default_rules())
    }
}

impl Narrator {
    pub fn new(rules: RuleSet) -> Self {
        Self {
            rules,
            state: HashMap::new(),
            fired: HashMap::new(),
            min_urgency: Urgency::Whim,
        }
    }

    /// The compiled-in mapping. Panics only if *our own* shipped file is
    /// malformed, which a unit test below makes impossible to ship.
    pub fn default_rules() -> RuleSet {
        serde_json::from_str(DEFAULT_RULES).expect("the authored rules.json must parse")
    }

    /// Load the operator's override if it exists, else the authored defaults.
    /// A broken override is ignored (and logged) rather than silencing her.
    pub fn load_or_default(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<RuleSet>(&raw) {
                Ok(rules) => {
                    tracing::info!(path = %path.display(), rules = rules.rules.len(), "fleet rules loaded");
                    Narrator::new(rules)
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "fleet rules are malformed; using the defaults");
                    Narrator::default()
                }
            },
            Err(_) => Narrator::default(),
        }
    }

    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    /// The governor's social half: at T3/T4 only an `Alarm` is worth the
    /// interruption. Nothing is queued — a suppressed line is simply not said,
    /// which is the shedding SPEC §3.1 asks for.
    pub fn set_min_urgency(&mut self, min: Urgency) {
        self.min_urgency = min;
    }

    /// Pretty name for an app id, if the rules declare one.
    pub fn app_name(&self, app: &str) -> String {
        self.rules.apps.get(app).map(|a| a.name.clone()).unwrap_or_else(|| app.to_string())
    }

    /// Feed one observation; get back whatever she would like to say about it.
    pub fn observe(&mut self, obs: &Observation, now_ms: u64) -> Vec<Utterance> {
        let Observation::Fleet { app, field, value } = obs else {
            return Vec::new();
        };

        let key = (app.clone(), field.clone());
        let prev = self.state.get(&key).and_then(|s| s.last.clone());
        let num = value.parse::<f64>().ok();
        {
            let st = self.state.entry(key.clone()).or_default();
            st.last = Some(value.clone());
            if let Some(n) = num {
                st.samples.push_back((now_ms, n));
                if st.samples.len() > MAX_SAMPLES {
                    st.samples.pop_front();
                }
            }
        }
        let samples = self.state.get(&key).map(|s| &s.samples);

        let mut out = Vec::new();
        for rule in &self.rules.rules {
            if !rule_matches(rule, app, field) {
                continue;
            }
            let Some(hit) = evaluate(&rule.when, prev.as_deref(), value, num, samples, now_ms)
            else {
                continue;
            };
            let urgency: Urgency = rule.urgency.into();
            if urgency < self.min_urgency {
                tracing::trace!(rule = %rule.id, "suppressed by tier");
                continue;
            }
            let fire_key = (rule.id.clone(), app.clone());
            if let (Some(cooldown), Some(last)) = (rule.cooldown_ms, self.fired.get(&fire_key)) {
                if now_ms.saturating_sub(*last) < cooldown {
                    continue;
                }
            }
            self.fired.insert(fire_key, now_ms);

            let name = self.app_name(app);
            let text = render(&rule.text, &name, field, value, prev.as_deref(), hit.delta);
            out.push(Utterance {
                text,
                urgency,
                defer_until: rule.defer_ms.map(|d| now_ms.saturating_add(d)),
                stale_after: rule.stale_after_ms.map(|d| now_ms.saturating_add(d)),
                expression: rule.expression.clone(),
            });
        }
        out
    }
}

struct Hit {
    delta: Option<f64>,
}

fn rule_matches(rule: &Rule, app: &str, field: &str) -> bool {
    let app_ok = match rule.app.as_deref() {
        None | Some("*") => true,
        Some(want) => want.eq_ignore_ascii_case(app),
    };
    app_ok && (rule.field == "*" || rule.field == field)
}

fn truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "on" | "yes")
}

fn falsy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "off" | "no")
}

fn evaluate(
    when: &When,
    prev: Option<&str>,
    value: &str,
    num: Option<f64>,
    samples: Option<&VecDeque<(u64, f64)>>,
    now_ms: u64,
) -> Option<Hit> {
    let changed = prev.map(|p| p != value).unwrap_or(true);
    let plain = |cond: bool| cond.then_some(Hit { delta: None });
    match when {
        When::Changed => plain(changed),
        When::Nonempty => plain(
            changed
                && !value.is_empty()
                && !matches!(value.to_ascii_lowercase().as_str(), "null" | "none" | "false" | "0"),
        ),
        // A boolean rule fires on the *edge*: `tripped:true` restated by a
        // reconnect must not re-panic her.
        When::IsTrue => plain(truthy(value) && changed),
        When::IsFalse => plain(falsy(value) && changed),
        When::Eq { value: want } => plain(value == want && changed),
        When::OneOf { values } => plain(values.iter().any(|v| v == value) && changed),
        When::Above { value: t } => plain(num? > *t && changed),
        When::Below { value: t } => plain(num? < *t && changed),
        When::CrossesAbove { value: t } => {
            let now = num?;
            let before = prev.and_then(|p| p.parse::<f64>().ok());
            plain(now > *t && before.map(|b| b <= *t).unwrap_or(true))
        }
        When::CrossesBelow { value: t } => {
            let now = num?;
            let before = prev.and_then(|p| p.parse::<f64>().ok());
            plain(now < *t && before.map(|b| b >= *t).unwrap_or(true))
        }
        When::Rises { by, window_ms } => {
            let now = num?;
            let floor = now_ms.saturating_sub(*window_ms);
            let low = samples?
                .iter()
                .filter(|(at, _)| *at >= floor)
                .map(|(_, v)| *v)
                .fold(f64::INFINITY, f64::min);
            if !low.is_finite() {
                return None;
            }
            let delta = now - low;
            (delta >= *by).then_some(Hit { delta: Some(delta) })
        }
    }
}

fn render(
    template: &str,
    app_name: &str,
    field: &str,
    value: &str,
    prev: Option<&str>,
    delta: Option<f64>,
) -> String {
    let delta_text = delta.map(crate::status::fmt_num).unwrap_or_default();
    template
        .replace("{app}", app_name)
        .replace("{field}", field)
        .replace("{value}", value)
        .replace("{prev}", prev.unwrap_or(""))
        .replace("{delta}", &delta_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(app: &str, field: &str, value: &str) -> Observation {
        Observation::Fleet {
            app: app.into(),
            field: field.into(),
            value: value.into(),
        }
    }

    #[test]
    fn the_shipped_rules_parse_and_are_sane() {
        let set = Narrator::default_rules();
        assert_eq!(set.version, 1);
        assert!(!set.rules.is_empty());
        let mut ids = std::collections::HashSet::new();
        for rule in &set.rules {
            assert!(ids.insert(rule.id.clone()), "duplicate rule id {}", rule.id);
            assert!(!rule.text.is_empty());
        }
        // SPEC §3.4: an Alarm is free and breaks flow. Exactly one authored
        // rule may use it, and it is the one about something moving on screen.
        let alarms: Vec<_> =
            set.rules.iter().filter(|r| r.urgency == UrgencyName::Alarm).collect();
        assert_eq!(alarms.len(), 1, "only NX Sentry may alarm");
        assert_eq!(alarms[0].app.as_deref(), Some("nx-sentry"));
    }

    #[test]
    fn a_new_app_needs_only_data() {
        // The point of F45: this app does not exist anywhere in the code.
        let set: RuleSet = serde_json::from_str(
            r#"{"version":1,"apps":{"nx-kettle":{"name":"NX Kettle"}},
                "rules":[{"id":"boiled","app":"nx-kettle","field":"boiled",
                          "when":{"op":"is_true"},"urgency":"notable",
                          "text":"{app} says the water is ready."}]}"#,
        )
        .unwrap();
        let mut n = Narrator::new(set);
        let said = n.observe(&obs("nx-kettle", "boiled", "true"), 0);
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].text, "NX Kettle says the water is ready.");
        assert_eq!(said[0].urgency, Urgency::Notable);
    }

    #[test]
    fn a_restated_value_does_not_fire_twice() {
        let mut n = Narrator::default();
        assert_eq!(n.observe(&obs("nx-sentry", "tripped", "true"), 0).len(), 1);
        assert!(n.observe(&obs("nx-sentry", "tripped", "true"), 1_000).is_empty());
    }

    #[test]
    fn spikes_use_the_window_and_the_cooldown() {
        let mut n = Narrator::default();
        for (t, hr) in [(0u64, 62.0), (5_000, 63.0), (10_000, 64.0)] {
            assert!(n.observe(&obs("pulsenx", "hr", &crate::status::fmt_num(hr)), t).is_empty());
        }
        let said = n.observe(&obs("pulsenx", "hr", "95"), 15_000);
        assert_eq!(said.len(), 1, "a 33 bpm jump is worth one remark");
        assert_eq!(said[0].urgency, Urgency::Notable);
        // …and then she shuts up about it for a while.
        assert!(n.observe(&obs("pulsenx", "hr", "120"), 20_000).is_empty());
    }

    #[test]
    fn tier_suppression_keeps_only_alarms() {
        let mut n = Narrator::default();
        n.set_min_urgency(Urgency::Alarm);
        assert!(n.observe(&obs("wivrn-nx", "session", "true"), 0).is_empty());
        assert_eq!(n.observe(&obs("nx-sentry", "tripped", "true"), 0).len(), 1);
    }

    #[test]
    fn crossing_a_threshold_fires_once_not_forever() {
        let set: RuleSet = serde_json::from_str(
            r#"{"version":1,"rules":[{"id":"x","app":"a","field":"n",
                 "when":{"op":"crosses_above","value":100.0},"urgency":"whim","text":"{value}"}]}"#,
        )
        .unwrap();
        let mut n = Narrator::new(set);
        assert!(n.observe(&obs("a", "n", "99"), 0).is_empty());
        assert_eq!(n.observe(&obs("a", "n", "101"), 1).len(), 1);
        assert!(n.observe(&obs("a", "n", "140"), 2).is_empty());
        assert!(n.observe(&obs("a", "n", "50"), 3).is_empty());
        assert_eq!(n.observe(&obs("a", "n", "101"), 4).len(), 1);
    }

    #[test]
    fn a_malformed_override_falls_back_instead_of_going_mute() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-rules.json");
        std::fs::write(&path, "{ nope").unwrap();
        let n = Narrator::load_or_default(&path);
        assert!(!n.rules().rules.is_empty());
    }
}
