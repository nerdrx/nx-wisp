//! Similarity, for coalescing (F36).
//!
//! The rule she is judged by: *if three similar thoughts queue up she says one
//! thing, not three*. We need a similarity measure that is cheap, allocation-
//! light, deterministic, language-agnostic enough for her voice packs, and —
//! above all — **explainable**, because a wrong merge is felt as "she ignored
//! me" and a wrong split is felt as "she repeated herself".
//!
//! The measure is a Jaccard index over a normalised content-word set:
//!
//! * lowercase, split on anything that is not alphanumeric,
//! * drop tokens shorter than 3 characters (English function words are short,
//!   and stray numerals like `3` carry no topic),
//! * drop a small stop list,
//! * fold trailing plural `s` so `builds` ~ `build`,
//! * add a synthetic `expr:<name>` token for the rig expression, because two
//!   thoughts that want the same face are usually the same thought.
//!
//! Deliberately *not* used: embeddings (that lives in `wisp-mind`, needs a
//! model, and would make this crate impure), edit distance (merges antonyms:
//! "build passed" vs "build failed" are one character apart in the wrong
//! metric), and prefix matching (merges everything that starts with "your").

use std::collections::BTreeSet;

/// Words that carry no topic. Kept tiny on purpose: an over-eager stop list
/// makes short utterances collapse into each other.
const STOP: &[&str] = &[
    "the", "and", "for", "you", "your", "yours", "that", "this", "with", "was", "were", "are",
    "has", "have", "had", "but", "not", "its", "it's", "just", "now", "then", "there", "here",
    "about", "into", "from", "again", "still", "been", "being", "her", "she", "him", "his", "they",
    "them", "our", "ours", "some", "any", "all", "one", "two", "three", "very", "really", "quite",
    "hey", "well", "okay", "yeah", "hmm", "oh",
];

/// The topic fingerprint of a piece of text. A `BTreeSet` so iteration order —
/// and therefore every downstream decision — is deterministic.
pub fn topic_tokens(text: &str, expression: Option<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lower = raw.to_lowercase();
        if lower.chars().count() < 3 {
            continue;
        }
        if STOP.contains(&lower.as_str()) {
            continue;
        }
        out.insert(fold_plural(&lower));
    }
    if let Some(e) = expression {
        if !e.is_empty() {
            out.insert(format!("expr:{}", e.to_lowercase()));
        }
    }
    out
}

/// `builds` -> `build`, but never `ss` -> `s` ("less", "class") and never down
/// to a stub shorter than three characters.
fn fold_plural(w: &str) -> String {
    let b = w.as_bytes();
    if b.len() > 3 && b[b.len() - 1] == b's' && b[b.len() - 2] != b's' {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

/// Jaccard index of two fingerprints, in `0.0..=1.0`.
///
/// Two thoughts with no content words at all ("ok!" vs "hi!") score 0: we would
/// rather she says both tiny things than silently swallow one.
pub fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    if inter == 0 {
        return 0.0;
    }
    let union = a.len() + b.len() - inter;
    inter as f32 / union as f32
}

/// Convenience for callers that only have the raw strings.
pub fn text_similarity(a: &str, b: &str) -> f32 {
    similarity(&topic_tokens(a, None), &topic_tokens(b, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::isolate;

    #[test]
    fn near_duplicates_merge() {
        isolate();
        assert!(text_similarity("the build failed again", "your build failed") >= 0.5);
        assert!(text_similarity("cargo build finished", "cargo build finished") == 1.0);
    }

    #[test]
    fn different_topics_do_not_merge() {
        isolate();
        assert!(text_similarity("the build failed", "your cpu is running hot") < 0.2);
        assert!(text_similarity("you have been at this for an hour", "kate is open") < 0.2);
    }

    #[test]
    fn antonyms_are_not_similar_enough_to_swallow_each_other() {
        // "build passed" vs "build failed" share exactly one content word.
        isolate();
        let s = text_similarity("the build passed", "the build failed");
        assert!(s < 0.5, "antonyms scored {s}");
    }

    #[test]
    fn plural_folding() {
        isolate();
        assert_eq!(fold_plural("builds"), "build");
        assert_eq!(fold_plural("class"), "class");
        assert_eq!(fold_plural("its"), "its");
        assert!(text_similarity("two builds failed", "the build failed") >= 0.5);
    }

    #[test]
    fn expression_is_part_of_the_fingerprint() {
        isolate();
        let a = topic_tokens("look at this", Some("smug"));
        let b = topic_tokens("look at this", Some("worried"));
        let c = topic_tokens("look at this", Some("smug"));
        assert!(similarity(&a, &c) == 1.0);
        assert!(similarity(&a, &b) < 1.0);
    }

    #[test]
    fn contentless_text_never_merges() {
        isolate();
        assert_eq!(text_similarity("ok!", "hi!"), 0.0);
        assert_eq!(text_similarity("", ""), 0.0);
    }

    #[test]
    fn similarity_is_symmetric_and_bounded() {
        isolate();
        let pairs = [
            ("the build failed", "build failed badly"),
            ("wandering off", "the cpu is hot"),
            ("pomodoro over, stand up", "stand up, pomodoro over"),
        ];
        for (a, b) in pairs {
            let ab = text_similarity(a, b);
            let ba = text_similarity(b, a);
            assert!((ab - ba).abs() < f32::EPSILON);
            assert!((0.0..=1.0).contains(&ab));
        }
        assert_eq!(text_similarity("pomodoro over stand up", "stand up pomodoro over"), 1.0);
    }
}
