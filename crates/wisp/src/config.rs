//! **F54 — the config dir, and never the operator's real one by accident.**
//!
//! `NX_WISP_CONFIG_DIR` wins over XDG, everywhere, in the dev build and in the
//! installed copy alike. That is not a testing convenience bolted on the side;
//! it is the mechanism by which a test run, a second profile and a headless CI
//! run are all isolated from the copy the operator actually uses. SPEC §4 makes
//! it a rule with no exceptions because a sibling project (NX Orbit,
//! 2026-08-20) wrote test fixtures into the operator's real memory.
//!
//! Everything derived from the config dir follows the same rule — the flight
//! recorder, the state file and, importantly, the single-instance lock
//! ([`crate::lock`]). A test run must never be able to steal the lock from an
//! installed copy, or vice versa.
//!
//! # What is *not* here
//!
//! **Per-sense enablement.** `wisp-senses` owns `senses.json` and
//! [`wisp_senses::ConsentLedger`] is the only thing that may write it: consent
//! is enforced by the type system over there, and a second copy of the truth
//! here would be a way around it. [`sense_rows`] and [`set_sense_enabled`] are
//! thin proxies onto the ledger so the CLI has one place to call, and they
//! store nothing.
//!
//! # Failure
//!
//! A corrupt config is not fatal and is not silently swallowed. The bad file is
//! moved aside, the defaults are used, and [`Loaded::note`] returns a sentence
//! saying so — which the CLI prints and the event loop logs. Failing open here
//! is safe: nothing in this file is a permission. (Consent, where failing open
//! *would* be unsafe, is in `wisp-senses` and fails closed.)

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wisp_attn::Chattiness;
use wisp_proto::{SenseId, Tier};

pub const CONFIG_FILE: &str = "config.json";
pub const STATE_FILE: &str = "state.json";
pub const FLIGHT_FILE: &str = "flight.jsonl";
pub const LOCK_FILE: &str = "wisp.lock";

/// Bumped only when a migration is needed. A file from the future is read
/// best-effort rather than refused: every field has a default.
pub const CURRENT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Where it lives
// ---------------------------------------------------------------------------

/// `$NX_WISP_CONFIG_DIR`, else `$XDG_CONFIG_HOME/nx-wisp`, else
/// `~/.config/nx-wisp`.
///
/// Deliberately byte-for-byte the same resolution as
/// [`wisp_senses::consent::config_dir`], so the ledger and everything here
/// always land in the same directory.
pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join(crate::APP_ID);
        }
    }
    home().join(".config").join(crate::APP_ID)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where models and other large generated things go. Never the config dir:
/// a 4 GiB GGUF does not belong in a directory people back up as dotfiles.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("NX_WISP_DATA_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    // An isolated config dir must not leave models pointing at the real one.
    if std::env::var_os("NX_WISP_CONFIG_DIR").is_some() {
        return config_dir().join("data");
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        if !d.is_empty() {
            return PathBuf::from(d).join(crate::APP_ID);
        }
    }
    home().join(".local").join("share").join(crate::APP_ID)
}

// ---------------------------------------------------------------------------
// The config itself
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// F39's dial. How much she volunteers.
    pub chattiness: Chattiness,
    pub appearance: Appearance,
    pub tier: TierPrefs,
    pub senses: SensePrefs,
    pub model: ModelSettings,
    pub recorder: RecorderPrefs,
    pub fleet: FleetPrefs,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CURRENT_VERSION,
            chattiness: Chattiness::default(),
            appearance: Appearance::default(),
            tier: TierPrefs::default(),
            senses: SensePrefs::default(),
            model: ModelSettings::default(),
            recorder: RecorderPrefs::default(),
            fleet: FleetPrefs::default(),
        }
    }
}

/// Which skin, and how big.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub skin: SkinChoice,
    /// Rendered size in surface pixels — F75's slider. `wisp-rig` scales the
    /// canvas onto this, so she is resolution independent.
    pub size_px: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance { skin: SkinChoice::Default, size_px: 128.0 }
    }
}

/// `"default"` for the shipped skin, anything else is a path to a `.toml`.
///
/// Stored as a plain string so the file stays something a person can edit
/// without knowing serde's tagging conventions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum SkinChoice {
    Default,
    File(PathBuf),
}

impl From<String> for SkinChoice {
    fn from(s: String) -> Self {
        if s.is_empty() || s == "default" {
            SkinChoice::Default
        } else {
            SkinChoice::File(PathBuf::from(s))
        }
    }
}

impl From<SkinChoice> for String {
    fn from(c: SkinChoice) -> String {
        match c {
            SkinChoice::Default => "default".to_string(),
            SkinChoice::File(p) => p.display().to_string(),
        }
    }
}

impl std::fmt::Display for SkinChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkinChoice::Default => f.write_str("default"),
            SkinChoice::File(p) => write!(f, "{}", p.display()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TierPrefs {
    /// The operator pinned a tier by hand. The running instance picks a change
    /// up on its next config poll, so `wisp tier pin T3` works without the CLI
    /// having to talk to the daemon.
    pub pinned: Option<Tier>,
}

/// Sense *settings*. Enablement is `wisp-senses`' ledger — see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SensePrefs {
    /// Project directories she watches (F26).
    pub watch_dirs: Vec<PathBuf>,
    /// Vitals sampling interval at T0/T1. The governor widens it below that.
    pub vitals_interval_secs: u64,
    /// Pin the terrain coalescing interval instead of letting the governor
    /// choose. `None` is correct for the app; the smoke example uses it.
    pub terrain_flush_ms: Option<u32>,
}

impl Default for SensePrefs {
    fn default() -> Self {
        SensePrefs { watch_dirs: Vec::new(), vitals_interval_secs: 5, terrain_flush_ms: None }
    }
}

/// Everything `wisp-mind` will need when it lands. Nothing here is read yet —
/// it is written down now so the config file does not have to change shape
/// under an operator who has already edited it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    /// Where GGUFs live. Not the config dir.
    pub models_dir: PathBuf,
    /// The always-resident small model.
    pub reflex: String,
    /// The one that gets evicted from VRAM at T2 (SPEC §3.1).
    pub deliberate: String,
    pub context_tokens: u32,
    /// Layers offloaded to the GPU. `-1` means as many as fit.
    pub gpu_layers: i32,
    pub temperature: f32,
    pub max_tokens: u32,
    /// SPEC §0.2(a): downloads are allowed only from pinned URLs with pinned
    /// hashes, and only when the operator has said yes. Off until they do.
    pub allow_downloads: bool,
    /// F55's manifest of pinned URLs and SHA-256 hashes.
    pub registry: PathBuf,
}

impl Default for ModelSettings {
    fn default() -> Self {
        ModelSettings {
            models_dir: data_dir().join("models"),
            reflex: "reflex".to_string(),
            deliberate: "deliberate".to_string(),
            context_tokens: 4096,
            gpu_layers: -1,
            temperature: 0.7,
            max_tokens: 256,
            allow_downloads: false,
            registry: data_dir().join("models").join("registry.json"),
        }
    }
}

/// F20. Deliberately has no `enabled` flag: SPEC §0.4 says the recorder holds
/// the real trace, and a switch to turn honesty off is not a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderPrefs {
    /// Rotate when the live file passes this.
    pub max_bytes: u64,
    /// How many rotated generations to keep besides the live one.
    pub keep: u32,
    /// Flush the buffer every this many records. The recorder also flushes on
    /// every read and at shutdown, so this only bounds what a hard kill loses.
    pub flush_every: u32,
    /// How far back `explain` looks for the observations behind an utterance.
    pub explain_window_ms: u64,
}

impl Default for RecorderPrefs {
    fn default() -> Self {
        RecorderPrefs {
            max_bytes: 4 * 1024 * 1024,
            keep: 3,
            flush_every: 64,
            explain_window_ms: 120_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FleetPrefs {
    /// Off in `--mock` regardless; the bus is a real socket.
    pub enabled: bool,
    /// `~/.local/bin/nx`. `None` lets `wisp-fleet` find it.
    pub nx_binary: Option<PathBuf>,
    pub roster_poll_secs: u64,
}

impl Default for FleetPrefs {
    fn default() -> Self {
        FleetPrefs { enabled: true, nx_binary: None, roster_poll_secs: 2 }
    }
}

// ---------------------------------------------------------------------------
// Load and save
// ---------------------------------------------------------------------------

/// Where the config came from, so the operator is never lied to about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// No file yet. Defaults, and nothing has been written.
    Defaults,
    /// Read cleanly.
    Loaded,
    /// The file was unreadable or did not parse. It was moved aside and the
    /// defaults are in force.
    Recovered { error: String, moved_to: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    pub provenance: Provenance,
}

impl Loaded {
    /// A sentence for the operator when something needs saying, `None` when it
    /// does not. DESIGN.md §9: what happened, and what to do next.
    pub fn note(&self) -> Option<String> {
        match &self.provenance {
            Provenance::Loaded | Provenance::Defaults => None,
            Provenance::Recovered { error, moved_to } => Some(format!(
                "Could not read {} ({error}) — the defaults are in force and the old file \
                 was kept at {}. Run `nx-wisp config show` to see what she is using.",
                self.path.display(),
                moved_to.display()
            )),
        }
    }

    pub fn recovered(&self) -> bool {
        matches!(self.provenance, Provenance::Recovered { .. })
    }
}

/// Load from [`config_dir`].
pub fn load() -> Loaded {
    load_from(&config_dir())
}

/// Load from an explicit directory.
///
/// Never fails. A missing file means defaults; a corrupt one means defaults
/// *and* a note, with the bad bytes preserved next to it rather than
/// overwritten — an operator who hand-edited the file and made a typo should
/// not lose the rest of their edit.
pub fn load_from(dir: &Path) -> Loaded {
    let path = dir.join(CONFIG_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Loaded { config: Config::default(), path, provenance: Provenance::Defaults };
        }
        Err(e) => {
            return recover(&path, &e.to_string());
        }
    };
    match serde_json::from_slice::<Config>(&bytes) {
        Ok(config) => Loaded { config, path, provenance: Provenance::Loaded },
        Err(e) => recover(&path, &e.to_string()),
    }
}

fn recover(path: &Path, error: &str) -> Loaded {
    let moved_to = path.with_extension("json.corrupt");
    // Best effort: if we cannot even move it aside, the defaults still apply
    // and the note still tells the truth about where the file is.
    let _ = std::fs::rename(path, &moved_to);
    Loaded {
        config: Config::default(),
        path: path.to_path_buf(),
        provenance: Provenance::Recovered { error: error.to_string(), moved_to },
    }
}

/// Save to [`config_dir`].
pub fn save(cfg: &Config) -> io::Result<PathBuf> {
    save_to(&config_dir(), cfg)
}

/// Write-then-rename, with both the file and its directory synced.
///
/// `rename(2)` is atomic within a filesystem, so a reader either sees the whole
/// old file or the whole new one. Syncing the *directory* as well is what makes
/// that survive a power cut rather than merely a crash.
pub fn save_to(dir: &Path, cfg: &Config) -> io::Result<PathBuf> {
    use std::io::Write;

    std::fs::create_dir_all(dir)?;
    let path = dir.join(CONFIG_FILE);
    // The pid in the temp name means two processes writing at once cannot
    // truncate each other's half-written file.
    let tmp = dir.join(format!(".{CONFIG_FILE}.{}.tmp", std::process::id()));

    let mut json = serde_json::to_vec_pretty(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    json.push(b'\n');

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// The key/value surface the CLI drives
// ---------------------------------------------------------------------------

/// Every settable key, in the order `config show` prints them.
pub const KEYS: &[&str] = &[
    "chattiness",
    "skin",
    "size",
    "pinned_tier",
    "senses.watch_dirs",
    "senses.vitals_interval_secs",
    "senses.terrain_flush_ms",
    "model.models_dir",
    "model.reflex",
    "model.deliberate",
    "model.context_tokens",
    "model.gpu_layers",
    "model.temperature",
    "model.max_tokens",
    "model.allow_downloads",
    "model.registry",
    "recorder.max_bytes",
    "recorder.keep",
    "recorder.flush_every",
    "recorder.explain_window_ms",
    "fleet.enabled",
    "fleet.nx_binary",
    "fleet.roster_poll_secs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetError {
    UnknownKey(String),
    BadValue { key: String, want: &'static str, got: String },
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::UnknownKey(k) => write!(
                f,
                "There is no setting called {k}. Run `nx-wisp config show` for the list."
            ),
            SetError::BadValue { key, want, got } => {
                write!(f, "{key} takes {want}; got {got:?}.")
            }
        }
    }
}

impl std::error::Error for SetError {}

impl Config {
    /// Read one key as the string `config show` would print.
    pub fn get(&self, key: &str) -> Option<String> {
        let v = match key {
            "chattiness" => chattiness_name(self.chattiness).to_string(),
            "skin" => self.appearance.skin.to_string(),
            "size" => format!("{}", self.appearance.size_px),
            "pinned_tier" => match self.tier.pinned {
                Some(t) => crate::fmt::tier_name(t).to_string(),
                None => "none".to_string(),
            },
            "senses.watch_dirs" => self
                .senses
                .watch_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(","),
            "senses.vitals_interval_secs" => self.senses.vitals_interval_secs.to_string(),
            "senses.terrain_flush_ms" => match self.senses.terrain_flush_ms {
                Some(v) => v.to_string(),
                None => "auto".to_string(),
            },
            "model.models_dir" => self.model.models_dir.display().to_string(),
            "model.reflex" => self.model.reflex.clone(),
            "model.deliberate" => self.model.deliberate.clone(),
            "model.context_tokens" => self.model.context_tokens.to_string(),
            "model.gpu_layers" => self.model.gpu_layers.to_string(),
            "model.temperature" => format!("{}", self.model.temperature),
            "model.max_tokens" => self.model.max_tokens.to_string(),
            "model.allow_downloads" => self.model.allow_downloads.to_string(),
            "model.registry" => self.model.registry.display().to_string(),
            "recorder.max_bytes" => self.recorder.max_bytes.to_string(),
            "recorder.keep" => self.recorder.keep.to_string(),
            "recorder.flush_every" => self.recorder.flush_every.to_string(),
            "recorder.explain_window_ms" => self.recorder.explain_window_ms.to_string(),
            "fleet.enabled" => self.fleet.enabled.to_string(),
            "fleet.nx_binary" => match &self.fleet.nx_binary {
                Some(p) => p.display().to_string(),
                None => "auto".to_string(),
            },
            "fleet.roster_poll_secs" => self.fleet.roster_poll_secs.to_string(),
            _ => return None,
        };
        Some(v)
    }

    /// Set one key from the string the operator typed.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), SetError> {
        let bad = |want: &'static str| SetError::BadValue {
            key: key.to_string(),
            want,
            got: value.to_string(),
        };
        match key {
            "chattiness" => {
                self.chattiness = parse_chattiness(value)
                    .ok_or_else(|| bad("silent, occasional, chatty or insufferable"))?
            }
            "skin" => self.appearance.skin = SkinChoice::from(value.to_string()),
            "size" => {
                let v: f32 = value.parse().map_err(|_| bad("a number of pixels"))?;
                if !(16.0..=1024.0).contains(&v) {
                    return Err(bad("a size between 16 and 1024 pixels"));
                }
                self.appearance.size_px = v;
            }
            "pinned_tier" => {
                self.tier.pinned = if matches!(value, "none" | "" | "auto" | "unpin") {
                    None
                } else {
                    Some(crate::fmt::parse_tier(value).ok_or_else(|| bad("T0..T4"))?)
                }
            }
            "senses.watch_dirs" => {
                self.senses.watch_dirs = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect()
            }
            "senses.vitals_interval_secs" => {
                let v: u64 = value.parse().map_err(|_| bad("a number of seconds"))?;
                self.senses.vitals_interval_secs = v.clamp(1, 3600);
            }
            "senses.terrain_flush_ms" => {
                self.senses.terrain_flush_ms = if value == "auto" {
                    None
                } else {
                    Some(value.parse().map_err(|_| bad("milliseconds, or `auto`"))?)
                }
            }
            "model.models_dir" => self.model.models_dir = PathBuf::from(value),
            "model.reflex" => self.model.reflex = value.to_string(),
            "model.deliberate" => self.model.deliberate = value.to_string(),
            "model.context_tokens" => {
                self.model.context_tokens = value.parse().map_err(|_| bad("a token count"))?
            }
            "model.gpu_layers" => {
                self.model.gpu_layers =
                    value.parse().map_err(|_| bad("a layer count, or -1 for as many as fit"))?
            }
            "model.temperature" => {
                let v: f32 = value.parse().map_err(|_| bad("a number between 0 and 2"))?;
                if !(0.0..=2.0).contains(&v) {
                    return Err(bad("a number between 0 and 2"));
                }
                self.model.temperature = v;
            }
            "model.max_tokens" => {
                self.model.max_tokens = value.parse().map_err(|_| bad("a token count"))?
            }
            "model.allow_downloads" => {
                self.model.allow_downloads = parse_bool(value).ok_or_else(|| bad("true or false"))?
            }
            "model.registry" => self.model.registry = PathBuf::from(value),
            "recorder.max_bytes" => {
                let v: u64 = value.parse().map_err(|_| bad("a number of bytes"))?;
                // Below a few KiB the recorder would rotate faster than it
                // records and `explain` would have nothing to walk back through.
                self.recorder.max_bytes = v.max(16 * 1024);
            }
            "recorder.keep" => {
                self.recorder.keep = value.parse().map_err(|_| bad("a number of files"))?
            }
            "recorder.flush_every" => {
                let v: u32 = value.parse().map_err(|_| bad("a number of records"))?;
                self.recorder.flush_every = v.max(1);
            }
            "recorder.explain_window_ms" => {
                self.recorder.explain_window_ms =
                    value.parse().map_err(|_| bad("a number of milliseconds"))?
            }
            "fleet.enabled" => {
                self.fleet.enabled = parse_bool(value).ok_or_else(|| bad("true or false"))?
            }
            "fleet.nx_binary" => {
                self.fleet.nx_binary =
                    if value == "auto" { None } else { Some(PathBuf::from(value)) }
            }
            "fleet.roster_poll_secs" => {
                let v: u64 = value.parse().map_err(|_| bad("a number of seconds"))?;
                self.fleet.roster_poll_secs = v.clamp(1, 3600);
            }
            _ => return Err(SetError::UnknownKey(key.to_string())),
        }
        Ok(())
    }
}

pub fn chattiness_name(c: Chattiness) -> &'static str {
    match c {
        Chattiness::Silent => "silent",
        Chattiness::Occasional => "occasional",
        Chattiness::Chatty => "chatty",
        Chattiness::Insufferable => "insufferable",
    }
}

pub fn parse_chattiness(s: &str) -> Option<Chattiness> {
    match s.trim().to_ascii_lowercase().as_str() {
        "silent" => Some(Chattiness::Silent),
        "occasional" | "default" => Some(Chattiness::Occasional),
        "chatty" => Some(Chattiness::Chatty),
        "insufferable" => Some(Chattiness::Insufferable),
        _ => None,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Consent proxies
// ---------------------------------------------------------------------------

/// The consent panel's rows, read straight from `wisp-senses`' ledger.
///
/// Nothing is cached and nothing is copied into this crate's config: the ledger
/// is the only truth about what she may see (SPEC §3.7).
pub fn sense_rows(dir: &Path) -> Vec<wisp_senses::ConsentRow> {
    ledger_at(dir).rows()
}

/// Flip one row. Persists through the ledger's own atomic write.
pub fn set_sense_enabled(dir: &Path, id: SenseId, on: bool) -> io::Result<()> {
    ledger_at(dir).set_enabled(id, on)
}

fn ledger_at(dir: &Path) -> wisp_senses::ConsentLedger {
    // The bus is only used for the invasive tell of SPEC §0.3, which a
    // read-only CLI query never raises. A detached sender is correct here.
    let (tx, _rx) = tokio::sync::broadcast::channel(1);
    wisp_senses::ConsentLedger::load_from(dir, tx, wisp_senses::Clock::new())
}

/// `SenseId` has no `FromStr`; the CLI needs one. These are the same keys
/// `wisp-senses` writes into `senses.json`.
pub fn parse_sense(s: &str) -> Option<SenseId> {
    let k = s.trim().to_ascii_lowercase().replace('-', "_");
    Some(match k.as_str() {
        "idle" => SenseId::Idle,
        "active_window" | "focus" | "window" => SenseId::ActiveWindow,
        "window_geometry" | "geometry" | "terrain" => SenseId::WindowGeometry,
        "media" => SenseId::Media,
        "audio" => SenseId::Audio,
        "notifications" | "notification" => SenseId::Notifications,
        "vitals" => SenseId::Vitals,
        "workspace" | "workspaces" => SenseId::Workspace,
        "clipboard" => SenseId::Clipboard,
        "microphone" | "mic" => SenseId::Microphone,
        "screen" => SenseId::Screen,
        "fleet" => SenseId::Fleet,
        _ => return None,
    })
}

/// The canonical name, matching `senses.json`'s keys.
pub fn sense_key(id: SenseId) -> &'static str {
    match id {
        SenseId::Idle => "idle",
        SenseId::ActiveWindow => "active_window",
        SenseId::WindowGeometry => "window_geometry",
        SenseId::Media => "media",
        SenseId::Audio => "audio",
        SenseId::Notifications => "notifications",
        SenseId::Vitals => "vitals",
        SenseId::Workspace => "workspace",
        SenseId::Clipboard => "clipboard",
        SenseId::Microphone => "microphone",
        SenseId::Screen => "screen",
        SenseId::Fleet => "fleet",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempConfig;

    #[test]
    fn config_dir_honours_the_override_before_anything_else() {
        let tmp = TempConfig::new();
        assert_eq!(config_dir(), tmp.path());
        // And it agrees with the crate that owns consent, or the ledger and
        // the config would live in different directories.
        assert_eq!(config_dir(), wisp_senses::consent::config_dir());
    }

    #[test]
    fn data_dir_follows_an_isolated_config_dir() {
        let tmp = TempConfig::new();
        assert!(
            data_dir().starts_with(tmp.path()),
            "an isolated run must not write models into the operator's real data dir: {}",
            data_dir().display()
        );
    }

    #[test]
    fn defaults_round_trip_and_the_file_is_readable_prose() {
        let tmp = TempConfig::new();
        let cfg = Config::default();
        let path = save_to(tmp.path(), &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"chattiness\": \"Occasional\""), "{text}");
        assert!(text.contains("\"skin\": \"default\""), "{text}");
        assert_eq!(load_from(tmp.path()).config, cfg);
        assert_eq!(load_from(tmp.path()).provenance, Provenance::Loaded);
    }

    #[test]
    fn a_missing_file_is_defaults_and_says_nothing() {
        let tmp = TempConfig::new();
        let l = load_from(tmp.path());
        assert_eq!(l.provenance, Provenance::Defaults);
        assert_eq!(l.config, Config::default());
        assert!(l.note().is_none());
        assert!(!tmp.path().join(CONFIG_FILE).exists(), "loading must not write");
    }

    #[test]
    fn corruption_fails_safe_with_defaults_and_says_so() {
        let tmp = TempConfig::new();
        std::fs::write(tmp.path().join(CONFIG_FILE), b"{ not json at all").unwrap();
        let l = load_from(tmp.path());
        assert_eq!(l.config, Config::default());
        assert!(l.recovered());
        let note = l.note().expect("a recovered config must say so");
        assert!(note.contains("defaults are in force"), "{note}");
        let Provenance::Recovered { moved_to, .. } = &l.provenance else { unreachable!() };
        assert!(moved_to.exists(), "the operator's bytes must be kept, not overwritten");
        assert_eq!(std::fs::read(moved_to).unwrap(), b"{ not json at all");
    }

    #[test]
    fn a_partial_file_keeps_what_it_says_and_defaults_the_rest() {
        let tmp = TempConfig::new();
        std::fs::write(
            tmp.path().join(CONFIG_FILE),
            br#"{"chattiness":"Chatty","appearance":{"size_px":64.0}}"#,
        )
        .unwrap();
        let l = load_from(tmp.path());
        assert_eq!(l.provenance, Provenance::Loaded);
        assert_eq!(l.config.chattiness, Chattiness::Chatty);
        assert_eq!(l.config.appearance.size_px, 64.0);
        assert_eq!(l.config.appearance.skin, SkinChoice::Default);
        assert_eq!(l.config.recorder, RecorderPrefs::default());
    }

    #[test]
    fn saving_leaves_no_temporary_behind() {
        let tmp = TempConfig::new();
        save_to(tmp.path(), &Config::default()).unwrap();
        let strays: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "{strays:?}");
    }

    #[test]
    fn every_key_reads_back_and_round_trips_through_set() {
        let _tmp = TempConfig::new();
        let cfg = Config::default();
        for key in KEYS {
            let v = cfg.get(key).unwrap_or_else(|| panic!("{key} has no getter"));
            let mut c2 = Config::default();
            c2.set(key, &v).unwrap_or_else(|e| panic!("{key} = {v:?}: {e}"));
            assert_eq!(c2.get(key).as_deref(), Some(v.as_str()), "{key} did not round trip");
        }
    }

    #[test]
    fn set_rejects_nonsense_with_a_sentence_that_says_what_to_do() {
        let _tmp = TempConfig::new();
        let mut cfg = Config::default();
        let e = cfg.set("chattiness", "loud").unwrap_err();
        assert!(e.to_string().contains("insufferable"), "{e}");
        let e = cfg.set("nope", "1").unwrap_err();
        assert!(e.to_string().contains("config show"), "{e}");
        assert!(cfg.set("size", "4").is_err(), "a 4px wisp is not a size");
        assert!(cfg.set("model.temperature", "9").is_err());
        assert_eq!(cfg, Config::default(), "a rejected set must change nothing");
    }

    #[test]
    fn pinned_tier_takes_the_names_the_cli_prints() {
        let _tmp = TempConfig::new();
        let mut cfg = Config::default();
        cfg.set("pinned_tier", "T3").unwrap();
        assert_eq!(cfg.tier.pinned, Some(Tier::Lobotomised));
        assert_eq!(cfg.get("pinned_tier").as_deref(), Some("T3"));
        cfg.set("pinned_tier", "none").unwrap();
        assert_eq!(cfg.tier.pinned, None);
        cfg.set("pinned_tier", "lobotomised").unwrap();
        assert_eq!(cfg.tier.pinned, Some(Tier::Lobotomised));
    }

    #[test]
    fn config_stores_no_sense_enablement() {
        let tmp = TempConfig::new();
        let cfg = Config::default();
        let json = serde_json::to_string(&cfg).unwrap();
        for id in wisp_senses::ALL_SENSES {
            // A sense name may legitimately appear as a section (`"fleet"` is
            // one). What must never appear is a name bound to a boolean, which
            // is what an enablement flag looks like.
            let key = sense_key(id);
            for flag in [format!("\"{key}\":true"), format!("\"{key}\":false")] {
                assert!(
                    !json.contains(&flag),
                    "{id:?} enablement leaked into config.json; the ledger owns it"
                );
            }
        }
        // …and the proxy really does reach the ledger's file.
        set_sense_enabled(tmp.path(), SenseId::Clipboard, true).unwrap();
        assert!(tmp.path().join("senses.json").exists());
        let rows = sense_rows(tmp.path());
        assert!(rows.iter().find(|r| r.id == SenseId::Clipboard).unwrap().enabled);
    }

    #[test]
    fn every_sense_name_parses_back_to_itself() {
        for id in wisp_senses::ALL_SENSES {
            assert_eq!(parse_sense(sense_key(id)), Some(id));
        }
        assert_eq!(parse_sense("mic"), Some(SenseId::Microphone));
        assert_eq!(parse_sense("not-a-sense"), None);
    }
}
