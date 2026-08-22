//! Writing a skin back out — with the author's comments still in it.
//!
//! # The decision, and why
//!
//! `Skin::to_toml` is honest about its limit: serde has nowhere to keep a
//! comment, so a round-trip through it returns a *correct* file that has lost
//! every line beginning with `#`. For most formats that is a shrug. Not for
//! this one.
//!
//! The shipped skin is 2453 lines and **651 of them are comments**. They are
//! not decoration. They record two complete character designs that were built,
//! reviewed and thrown away, and *why* each one failed — "a humanoid figure at
//! sprite scale falls into the uncanny valley", "nothing in this file is
//! allowed to be near-black". A fourth redesign that opens this editor and
//! presses save would delete the institutional memory that stops it repeating
//! attempt 2. The editor exists to make the fourth redesign cheap; silently
//! making it *ignorant* would be a bad trade at any price.
//!
//! So this module preserves them. [`to_toml_preserving`] serialises the
//! document normally, then merges the original file's comments back onto the
//! result by matching structure — each `[[bone]]`, `[[shape]]`, `[[clip]]` and
//! so on is identified by its `name`, so a comment follows the *thing it is
//! about* even when the operator reorders, inserts or deletes around it.
//!
//! # And it is still loud
//!
//! Preservation cannot be total. A comment attached to a bone that the
//! operator deleted has nothing left to attach to; carrying it forward and
//! parking it somewhere arbitrary would be worse than dropping it, because it
//! would then describe the wrong thing. Those are reported, by name, in
//! [`SaveReport::dropped`] — the editor puts them in front of the operator
//! before the write, so "you are about to lose the note on `tail3`" is a
//! sentence they read rather than a diff they find later.
//!
//! Both halves matter: preserve by default, and say out loud what could not be.
//!
//! # Byte stability
//!
//! `save → load → save` produces identical bytes. The merge is idempotent: the
//! text written out parses back to the same structure with the same decor, and
//! copying that decor onto a fresh serialisation of the same document
//! reproduces it exactly. `tests/save.rs` asserts this on the shipped skin.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table};
use wisp_rig::skin::doc::SkinDoc;

use crate::error::EditError;

/// What a save did to the comments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SaveReport {
    /// Comment blocks carried across.
    pub carried: usize,
    /// Comment blocks that had nothing left to attach to, described in the
    /// operator's terms: `bone "tail3"`, `clip "wander"`.
    pub dropped: Vec<String>,
    /// False when the document had no original text to merge from — a skin
    /// created from scratch in the editor. There is nothing to lose in that
    /// case and nothing to warn about.
    pub had_source: bool,
}

impl SaveReport {
    pub fn lost_anything(&self) -> bool {
        !self.dropped.is_empty()
    }

    /// The sentence the editor shows before writing. `None` when there is
    /// nothing to warn about.
    pub fn warning(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        let list = self.dropped.join(", ");
        Some(format!(
            "saving drops the comments on {list} — the thing each one described is no longer in \
             the skin, so there is nowhere to keep them"
        ))
    }
}

/// Serialise a document with no comments at all. This is what
/// `Skin::to_toml` does, exposed here so the editor can offer it deliberately
/// rather than reaching it by accident.
pub fn to_toml(doc: &SkinDoc) -> Result<String, EditError> {
    toml::to_string_pretty(doc).map_err(|e| EditError::Write(e.to_string()))
}

/// Serialise a document, merging `original`'s comments back in.
pub fn to_toml_preserving(
    doc: &SkinDoc,
    original: &str,
) -> Result<(String, SaveReport), EditError> {
    let fresh = to_toml(doc)?;
    let mut new: DocumentMut =
        fresh.parse().map_err(|e: toml_edit::TomlError| EditError::Write(e.to_string()))?;
    let old: DocumentMut =
        original.parse().map_err(|e: toml_edit::TomlError| EditError::Read(e.to_string()))?;

    let mut report = SaveReport { had_source: true, ..Default::default() };
    merge_table(old.as_table(), new.as_table_mut(), "", &mut report);

    // The tail of the file — a licence footer, a "written by hand" note — has
    // no key to hang off, so it is copied wholesale.
    if let Some(t) = old.trailing().as_str() {
        if !t.is_empty() {
            if t.contains('#') {
                report.carried += 1;
            }
            new.set_trailing(t);
        }
    }

    Ok((new.to_string(), report))
}

/// Write a skin to disk, preserving comments when there is an original to
/// preserve them from.
///
/// The write is **atomic**: the bytes land in a sibling temp file and are
/// renamed over the target, so an interrupted save cannot leave a half-written
/// skin where a whole one used to be. A rig file is hours of work.
pub fn write(
    path: &Path,
    doc: &SkinDoc,
    original: Option<&str>,
) -> Result<SaveReport, EditError> {
    let (text, report) = match original {
        Some(src) => to_toml_preserving(doc, src)?,
        None => (to_toml(doc)?, SaveReport::default()),
    };
    let tmp = path.with_extension("toml.part");
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| EditError::Write(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| EditError::Write(e.to_string()))?;
    Ok(report)
}

/// Read a skin file, returning both the document and its original text — the
/// editor needs the second one to give the comments back on save.
pub fn read(path: &Path) -> Result<(SkinDoc, String), EditError> {
    let src = std::fs::read_to_string(path).map_err(|e| EditError::Read(e.to_string()))?;
    let doc: SkinDoc = toml::from_str(&src).map_err(|e| EditError::Read(e.to_string()))?;
    Ok((doc, src))
}

// --------------------------------------------------------------- the merge

fn has_comment(s: Option<&str>) -> bool {
    s.is_some_and(|s| s.contains('#'))
}

/// Copy one table's decor — its header comment and its keys' comments — from
/// `old` onto `new`, then recurse into whatever tables they share.
fn merge_table(old: &Table, new: &mut Table, path: &str, report: &mut SaveReport) {
    if let Some(prefix) = old.decor().prefix().and_then(|r| r.as_str()) {
        if !prefix.is_empty() {
            if prefix.contains('#') {
                report.carried += 1;
            }
            new.decor_mut().set_prefix(prefix);
        }
    }

    // Plain keys: copy the comment that sits above each one.
    let old_keys: Vec<(String, String)> = old
        .iter()
        .filter_map(|(k, _)| {
            old.key(k)
                .and_then(|key| key.leaf_decor().prefix())
                .and_then(|r| r.as_str())
                .map(|p| (k.to_string(), p.to_string()))
        })
        .collect();
    for (k, prefix) in &old_keys {
        if prefix.is_empty() {
            continue;
        }
        match new.key_mut(k) {
            Some(mut key) => {
                if prefix.contains('#') {
                    report.carried += 1;
                }
                key.leaf_decor_mut().set_prefix(prefix.as_str());
            }
            None if prefix.contains('#') => {
                report.dropped.push(describe(path, k));
            }
            None => {}
        }
    }

    // Sub-tables and arrays of tables.
    for (k, old_item) in old.iter() {
        let child_path = if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
        match (old_item, new.get_mut(k)) {
            (Item::Table(ot), Some(Item::Table(nt))) => {
                merge_table(ot, nt, &child_path, report);
            }
            (Item::ArrayOfTables(oa), Some(Item::ArrayOfTables(na))) => {
                merge_array(oa, na, k, &child_path, report);
            }
            (Item::Table(ot), None) => count_lost(ot, &child_path, report),
            (Item::ArrayOfTables(oa), None) => {
                for t in oa.iter() {
                    count_lost(t, &child_path, report);
                }
            }
            _ => {}
        }
    }
}

/// Match old entries to new ones by identity, and merge each pair.
fn merge_array(
    old: &toml_edit::ArrayOfTables,
    new: &mut toml_edit::ArrayOfTables,
    section: &str,
    path: &str,
    report: &mut SaveReport,
) {
    // Which new entry each old entry corresponds to. Identity first; failing
    // that — an entry the format does not name — position, so a comment on an
    // anonymous stanza still survives when nothing moved.
    let new_ids: Vec<Option<String>> = new.iter().map(identity).collect();
    let mut taken = vec![false; new.len()];

    let mut plan: Vec<(usize, Option<usize>)> = Vec::with_capacity(old.len());
    for (i, ot) in old.iter().enumerate() {
        let id = identity(ot);
        let hit = match &id {
            Some(id) => (0..new_ids.len())
                .find(|&j| !taken[j] && new_ids[j].as_deref() == Some(id.as_str())),
            None => (i < new_ids.len() && !taken[i] && new_ids[i].is_none()).then_some(i),
        };
        if let Some(j) = hit {
            taken[j] = true;
        }
        plan.push((i, hit));
    }

    for (i, hit) in plan {
        let ot = old.get(i).expect("index from the old array");
        match hit {
            Some(j) => {
                if let Some(nt) = new.get_mut(j) {
                    merge_table(ot, nt, path, report);
                }
            }
            None => {
                let what = identity(ot)
                    .map(|id| format!("{section} {id:?}"))
                    .unwrap_or_else(|| format!("{section} #{}", i + 1));
                if table_has_comment(ot) {
                    report.dropped.push(what);
                }
            }
        }
    }
}

/// What identifies one array-of-tables entry.
///
/// `name` for everything the format names. Tracks are `bone` plus `channel`
/// and weights are `point`, because those are what actually tell two of them
/// apart — a comment on `[[clip.track]] bone = "tail1", channel = "rot"` has
/// to follow that curve and not merely the third track in the list.
fn identity(t: &Table) -> Option<String> {
    if let Some(n) = t.get("name").and_then(Item::as_str) {
        return Some(n.to_string());
    }
    if let (Some(b), Some(c)) =
        (t.get("bone").and_then(Item::as_str), t.get("channel").and_then(Item::as_str))
    {
        return Some(format!("{b}.{c}"));
    }
    if let Some(p) = t.get("point").and_then(Item::as_integer) {
        return Some(format!("point {p}"));
    }
    None
}

fn table_has_comment(t: &Table) -> bool {
    if has_comment(t.decor().prefix().and_then(|r| r.as_str())) {
        return true;
    }
    t.iter().any(|(k, item)| {
        has_comment(t.key(k).and_then(|key| key.leaf_decor().prefix()).and_then(|r| r.as_str()))
            || match item {
                Item::Table(inner) => table_has_comment(inner),
                Item::ArrayOfTables(a) => a.iter().any(table_has_comment),
                _ => false,
            }
    })
}

fn count_lost(t: &Table, path: &str, report: &mut SaveReport) {
    if table_has_comment(t) {
        report.dropped.push(path.to_string());
    }
}

fn describe(path: &str, key: &str) -> String {
    if path.is_empty() {
        format!("{key:?}")
    } else {
        format!("{path}.{key}")
    }
}
