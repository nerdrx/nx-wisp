//! **F55 — the model registry.**
//!
//! > *Models are not in the AppImage. First-run downloads them by SHA with a
//! > progress UI; a model registry manifest in the repo pins URLs + hashes.*
//!
//! SPEC §0.2 allows exactly three kinds of network egress, and this is the
//! first of them: *model downloads from pinned URLs with pinned hashes.* Both
//! halves of that are load-bearing. The URL is pinned so nothing can be
//! persuaded to fetch from somewhere else; the hash is pinned so it does not
//! matter if the URL is redirected to a CDN, a mirror, or a proxy — bytes that
//! do not hash to the pin never become a file she will load.
//!
//! The manifest ships in the repository (`models/registry.json`) and is
//! compiled in, so a fresh install has something to fetch *from* before it has
//! fetched anything. An operator can point [`ModelRegistry::load`] at their own
//! file; it is validated the same way, and an entry without a plausible SHA-256
//! is rejected rather than trusted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::Role;
use crate::error::{MindError, Result};

/// The manifest that ships in the repo.
pub const BUILTIN: &str = include_str!("../models/registry.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Stable id used in config and in [`wisp_proto::EventKind::Model`].
    pub name: String,
    pub role: Role,
    /// Filename under the models directory. Never a path — an entry that could
    /// name `../../.ssh/id_ed25519` would be a manifest that can write anywhere.
    pub file: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
    pub size_bytes: u64,
    /// Roughly what it costs on the card, for the governor's accounting. Not
    /// measured here; [`crate::backend::Loaded::vram_mib`] is the truth.
    #[serde(default)]
    pub vram_mib: u64,
    #[serde(default)]
    pub context_max: u32,
    #[serde(default)]
    pub embedding_dim: u32,
    #[serde(default)]
    pub chat_template: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    /// The one picked for this role when config does not say otherwise.
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub notes: Option<String>,
}

impl ModelEntry {
    pub fn local_path(&self, models_dir: impl AsRef<Path>) -> PathBuf {
        models_dir.as_ref().join(&self.file)
    }

    /// Is the file there, and the right size? Cheap; the hash is checked by
    /// [`crate::fetch`], which caches the verdict.
    pub fn looks_present(&self, models_dir: impl AsRef<Path>) -> bool {
        std::fs::metadata(self.local_path(models_dir))
            .map(|m| m.len() == self.size_bytes)
            .unwrap_or(false)
    }

    pub fn size_mib(&self) -> u64 {
        self.size_bytes / (1024 * 1024)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRegistry {
    pub version: u32,
    #[serde(default)]
    pub note: Option<String>,
    pub models: Vec<ModelEntry>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        ModelRegistry::builtin()
    }
}

impl ModelRegistry {
    /// The manifest compiled into the binary. Panics only if the file in the
    /// repository is malformed, which a test catches long before a release.
    pub fn builtin() -> ModelRegistry {
        let r: ModelRegistry =
            serde_json::from_str(BUILTIN).expect("the built-in model registry must parse");
        r.validated().expect("the built-in model registry must be valid");
        r
    }

    pub fn load(path: impl AsRef<Path>) -> Result<ModelRegistry> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| MindError::io(path, e))?;
        let r: ModelRegistry = serde_json::from_str(&text)
            .map_err(|e| MindError::BadRegistry(format!("{}: {e}", path.display())))?;
        r.validated()?;
        Ok(r)
    }

    /// The built-in manifest, unless the operator has put one at `path`.
    pub fn load_or_builtin(path: impl AsRef<Path>) -> ModelRegistry {
        match ModelRegistry::load(&path) {
            Ok(r) => r,
            Err(e) => {
                if path.as_ref().exists() {
                    // A broken override is worth saying out loud; falling back
                    // silently would hide a typo until a 18 GiB download went
                    // to the wrong place.
                    tracing::warn!(error = %e, "model registry override is unusable; using the built-in one");
                }
                ModelRegistry::builtin()
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.name == name)
    }

    /// The default entry for a role, or the only one if there is exactly one.
    pub fn default_for(&self, role: Role) -> Option<&ModelEntry> {
        self.models
            .iter()
            .find(|m| m.role == role && m.default)
            .or_else(|| self.models.iter().find(|m| m.role == role))
    }

    /// Resolve a config value: either a registry name, or a role word
    /// (`"reflex"`), or empty for the default.
    pub fn resolve(&self, role: Role, want: &str) -> Result<&ModelEntry> {
        let want = want.trim();
        if want.is_empty() || want == role.as_str() {
            return self
                .default_for(role)
                .ok_or_else(|| MindError::UnknownModel(role.as_str().to_string()));
        }
        self.get(want)
            .ok_or_else(|| MindError::UnknownModel(want.to_string()))
    }

    pub fn for_role(&self, role: Role) -> impl Iterator<Item = &ModelEntry> {
        self.models.iter().filter(move |m| m.role == role)
    }

    /// Everything an entry has to satisfy before a byte is fetched for it.
    pub fn validated(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for m in &self.models {
            let bad = |why: &str| MindError::BadRegistry(format!("{}: {why}", m.name));
            if m.name.trim().is_empty() {
                return Err(MindError::BadRegistry("an entry has no name".into()));
            }
            if !seen.insert(m.name.as_str()) {
                return Err(bad("appears twice"));
            }
            // SPEC §0.2a. Plain HTTP would make the pinned hash the *only*
            // defence; there is no reason to give up the first one.
            if !m.url.starts_with("https://") {
                return Err(bad("the URL is not https"));
            }
            if m.sha256.len() != 64 || !m.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(bad("the sha256 is not 64 hex characters"));
            }
            if m.sha256.chars().any(|c| c.is_ascii_uppercase()) {
                return Err(bad("the sha256 must be lowercase, so comparison is a memcmp"));
            }
            if m.size_bytes == 0 {
                return Err(bad("the size is zero"));
            }
            // A filename, not a path. This is the difference between a manifest
            // and an arbitrary write primitive.
            if m.file.is_empty()
                || m.file.contains('/')
                || m.file.contains('\\')
                || m.file.starts_with('.')
            {
                return Err(bad("`file` must be a bare filename"));
            }
        }
        for role in Role::ALL {
            if self.models.iter().filter(|m| m.role == role && m.default).count() > 1 {
                return Err(MindError::BadRegistry(format!(
                    "{} has more than one default",
                    role.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Total bytes a first run would have to fetch for the defaults.
    pub fn first_run_bytes(&self) -> u64 {
        Role::ALL
            .iter()
            .filter_map(|r| self.default_for(*r))
            .map(|m| m.size_bytes)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_manifest_is_valid() {
        let r = ModelRegistry::builtin();
        r.validated().expect("valid");
        assert!(r.default_for(Role::Reflex).is_some());
        assert!(r.default_for(Role::Deliberate).is_some());
        assert!(r.default_for(Role::Embed).is_some());
    }

    #[test]
    fn every_pinned_url_is_https_and_every_hash_is_a_hash() {
        for m in &ModelRegistry::builtin().models {
            assert!(m.url.starts_with("https://"), "{}", m.name);
            assert_eq!(m.sha256.len(), 64, "{}", m.name);
            assert!(m.size_bytes > 0, "{}", m.name);
        }
    }

    #[test]
    fn a_manifest_that_could_write_outside_the_models_directory_is_refused() {
        let mut r = ModelRegistry::builtin();
        r.models[0].file = "../../.ssh/authorized_keys".into();
        let err = r.validated().unwrap_err();
        assert!(err.to_string().contains("bare filename"), "{err}");
    }

    #[test]
    fn a_plain_http_url_is_refused() {
        let mut r = ModelRegistry::builtin();
        r.models[0].url = "http://example.invalid/model.gguf".into();
        assert!(r.validated().is_err());
    }

    #[test]
    fn there_is_a_tiny_model_so_the_real_backend_can_be_exercised_cheaply() {
        let r = ModelRegistry::builtin();
        let small = r
            .models
            .iter()
            .filter(|m| m.role == Role::Reflex)
            .min_by_key(|m| m.size_bytes)
            .expect("a reflex model exists");
        assert!(
            small.size_bytes < 200 * 1024 * 1024,
            "the smoke-test model must stay small enough to fetch casually: {} is {} MiB",
            small.name,
            small.size_mib()
        );
        assert!(!small.default, "the tiny model must never be the default");
    }

    #[test]
    fn resolve_accepts_a_role_word_a_name_or_nothing() {
        let r = ModelRegistry::builtin();
        assert_eq!(
            r.resolve(Role::Reflex, "").expect("default").name,
            r.default_for(Role::Reflex).expect("default").name
        );
        assert_eq!(
            r.resolve(Role::Reflex, "reflex").expect("role word").name,
            r.default_for(Role::Reflex).expect("default").name
        );
        assert_eq!(
            r.resolve(Role::Reflex, "smollm2-135m-q4km")
                .expect("by name")
                .name,
            "smollm2-135m-q4km"
        );
        assert!(r.resolve(Role::Reflex, "gpt-9").is_err());
    }
}
