//! F31's streaming half: cutting a token stream into things worth synthesising.
//!
//! `wisp-mind` emits text a token at a time. If we waited for the whole reply
//! before calling the engine, her first word would land a second and a half
//! after the model started — which is the difference between a companion and a
//! progress bar. So this cuts the stream at the first *safe* place and hands
//! that to the engine while the model is still writing.
//!
//! ## What makes a cut safe
//!
//! Not "there is a full stop". `3.5`, `e.g.`, `Dr. Vega`, `main.rs` and
//! `example.com` all contain a full stop and none of them ends a sentence; a
//! naive splitter says "three point", a pause, "five", and she sounds broken.
//! [`Chunker`] refuses a boundary it cannot see the *next* character of, which
//! is what makes this work on a stream rather than only on a finished string.
//!
//! ## Why the first chunk is allowed to be tiny and later ones are not
//!
//! Latency only matters once. The first clause should leave as soon as there is
//! anything at all to say; after that the queue is full and cutting early buys
//! nothing while costing prosody — an engine handed `"and"` on its own produces
//! a flat, clipped word, because a VITS duration predictor has no context to
//! work with. So [`ChunkConfig::first_min_chars`] is small and
//! [`ChunkConfig::min_chars`] is not.
//!
//! ## Code blocks are not speech
//!
//! Reading a fenced block aloud is unbearable, and `wisp-mind` produces them.
//! They come out as [`ChunkKind::Code`] and [`crate::speaker`] substitutes a
//! spoken placeholder from the voice pack, so the decision lives in data (F35)
//! rather than here.

/// Speech, or something that should not be read out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    Speech,
    /// A fenced code block, verbatim and unspoken.
    Code,
}

/// One synthesisable unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub kind: ChunkKind,
}

impl Chunk {
    pub fn speech(text: impl Into<String>) -> Self {
        Chunk { text: text.into(), kind: ChunkKind::Speech }
    }
    pub fn code(text: impl Into<String>) -> Self {
        Chunk { text: text.into(), kind: ChunkKind::Code }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkConfig {
    /// The first chunk of an utterance may be this short. Latency wins once.
    pub first_min_chars: usize,
    /// Every later chunk must reach this before a clause boundary counts.
    /// Sentence boundaries always count regardless.
    pub min_chars: usize,
    /// A run-on with no punctuation is cut here anyway, at the last word break.
    pub max_chars: usize,
    /// Cut at `,` `;` `:` `—` too, not only at sentence ends.
    pub clause_breaks: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig {
            first_min_chars: 12,
            min_chars: 48,
            max_chars: 240,
            clause_breaks: true,
        }
    }
}

/// Words that end in a full stop without ending a sentence.
///
/// Lower-cased, without the stop. Short and English-only on purpose: this is a
/// *precision* list, not a recall one. A missed abbreviation costs one wrong
/// pause; a false positive swallows a sentence boundary and she runs two
/// thoughts together, which is worse.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "vs", "etc", "eg", "ie", "cf", "al",
    "fig", "approx", "dept", "univ", "inc", "ltd", "co", "no", "vol", "pp", "ca", "esp",
    "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "sept", "oct", "nov", "dec",
    "mon", "tue", "tues", "wed", "thu", "thur", "thurs", "fri", "sat", "sun",
    "am", "pm", "min", "max", "sec", "hr", "kb", "mb", "gb", "tb",
];

fn is_sentence_end(c: char) -> bool {
    // The CJK forms are here because `wisp-mind` will happily answer in the
    // language it was asked in, and a Japanese reply with no 。 in the list is
    // one 400-character chunk.
    matches!(c, '.' | '!' | '?' | '…' | '。' | '！' | '？')
}

fn is_clause_end(c: char) -> bool {
    matches!(c, ',' | ';' | ':' | '—' | '–' | '、')
}

/// Closing marks that belong to the sentence they follow: `He said "no." ` must
/// cut after the quote, not between the stop and it.
fn is_trailing_close(c: char) -> bool {
    matches!(c, '"' | '\'' | ')' | ']' | '}' | '»' | '”' | '’')
}

/// Cuts a growing string into speakable pieces.
///
/// Push text as it arrives, take whatever is ready, and [`Chunker::finish`] when
/// the model stops. Holds no clock and spawns nothing: entirely a function of
/// what it has been fed, which is why every boundary rule below has a test.
#[derive(Debug, Clone)]
pub struct Chunker {
    cfg: ChunkConfig,
    buf: String,
    /// Have we emitted anything for this utterance yet?
    started: bool,
    /// No more text is coming.
    closed: bool,
    /// Inside a ``` fence: nothing is cut until it closes.
    in_fence: bool,
}

impl Default for Chunker {
    fn default() -> Self {
        Chunker::new(ChunkConfig::default())
    }
}

impl Chunker {
    pub fn new(cfg: ChunkConfig) -> Self {
        Chunker { cfg, buf: String::new(), started: false, closed: false, in_fence: false }
    }

    /// Everything in one go — the non-streaming case, for a canned line.
    pub fn all(text: &str) -> Vec<Chunk> {
        let mut c = Chunker::default();
        c.push(text);
        c.finish();
        c.drain()
    }

    pub fn push(&mut self, text: &str) {
        debug_assert!(!self.closed, "text pushed after finish()");
        self.buf.push_str(text);
    }

    /// No more text is coming; whatever is left is now speakable.
    pub fn finish(&mut self) {
        self.closed = true;
    }

    pub fn is_finished(&self) -> bool {
        self.closed && self.buf.trim().is_empty()
    }

    /// What is still held back, for a "she was cut off mid-word" record.
    pub fn pending(&self) -> &str {
        &self.buf
    }

    /// Throw away everything not yet emitted. Barge-in (F33) calls this: SPEC
    /// §3.1 says shed, and the words she had not reached are exactly the work
    /// that must not be queued for later.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Everything ready right now. Empty is normal — it means "still waiting for
    /// a boundary", not "done".
    pub fn drain(&mut self) -> Vec<Chunk> {
        let mut out = Vec::new();
        while let Some(c) = self.next_chunk() {
            out.push(c);
        }
        out
    }

    /// One chunk, or `None` if nothing is ready.
    pub fn next_chunk(&mut self) -> Option<Chunk> {
        loop {
            if self.buf.trim().is_empty() {
                // Trailing whitespace is not speech. Only drop it once the
                // stream is closed, so a chunk boundary that happens to land on
                // a space does not eat the space the next token needs.
                if self.closed {
                    self.buf.clear();
                }
                return None;
            }
            // Leading whitespace would offset every index below by an amount
            // nothing else tracks. Normalise once, here.
            let lead = self.buf.len() - self.buf.trim_start().len();
            if lead > 0 {
                self.buf.drain(..lead);
                continue;
            }

            if self.buf.starts_with("```") {
                let Some(end) = self.fence_end() else {
                    self.in_fence = true;
                    return None;
                };
                self.in_fence = false;
                let rest = self.buf.split_off(end);
                let block = std::mem::replace(&mut self.buf, rest).trim().to_string();
                if block.trim_matches('`').trim().is_empty() {
                    continue; // an empty fence is not worth a chunk
                }
                self.started = true;
                return Some(Chunk::code(block));
            }

            // A fence ahead is a hard stop: the prose in front of it is
            // speakable *now*, whether or not the model has finished the block.
            let fence = self.buf.find("```");
            let limit = fence.unwrap_or(self.buf.len());
            let terminal = fence.is_some() || self.closed;

            let cut = self.find_cut(limit, terminal)?;
            let rest = self.buf.split_off(cut);
            let piece = std::mem::replace(&mut self.buf, rest);
            let trimmed = piece.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.started = true;
            return Some(Chunk::speech(trimmed));
        }
    }

    /// Byte index one past the closing fence. `None` while a fence is open and
    /// the model is still writing — an unterminated block must not be cut in
    /// half, because the half we emitted could not be un-emitted.
    fn fence_end(&self) -> Option<usize> {
        match self.buf[3..].find("```") {
            Some(i) => Some(3 + i + 3),
            None if self.closed => Some(self.buf.len()),
            None => None,
        }
    }

    fn min_now(&self) -> usize {
        if self.started {
            self.cfg.min_chars
        } else {
            self.cfg.first_min_chars
        }
    }

    /// Byte index one past the end of the next chunk, or `None` if we should
    /// wait for more text.
    ///
    /// `limit` is how far into the buffer we may look; `terminal` says whether
    /// `limit` can be treated as the end of the stream (because the stream
    /// really ended, or because a code fence starts there).
    fn find_cut(&self, limit: usize, terminal: bool) -> Option<usize> {
        let head = &self.buf[..limit];

        // A blank line is a paragraph break, and she should breathe there
        // regardless of how short the paragraph was.
        if let Some(i) = head.find("\n\n") {
            return Some(i + 2);
        }

        let mut spoken = 0usize;
        for (i, c) in head.char_indices() {
            spoken += 1;
            let sentence = is_sentence_end(c);
            let clause = self.cfg.clause_breaks && is_clause_end(c);
            if !sentence && !clause {
                continue;
            }
            // Swallow a run of marks ("...", "?!") and the closing quote or
            // bracket that belongs to them: `he said "no." ` cuts after the
            // quote, not between the stop and it.
            let mut end = i + c.len_utf8();
            while let Some(d) = self.buf[end..].chars().next() {
                if is_sentence_end(d) || is_trailing_close(d) {
                    end += d.len_utf8();
                } else {
                    break;
                }
            }

            // What follows decides whether this is punctuation or a boundary,
            // and on a stream we may simply not know yet.
            match self.buf[end..].chars().next() {
                None if !terminal => return None,
                None => {}
                Some(d) if d.is_whitespace() => {}
                // Glued to the next token: `3.5`, `1,048,576`, `main.rs`,
                // `example.com`. Never a boundary.
                Some(_) => continue,
            }

            if sentence && c == '.' && self.ends_with_abbreviation(i) {
                continue;
            }

            // A complete sentence is worth speaking almost immediately; a mere
            // clause has to earn its cut, or the engine gets a fragment with no
            // context and she comes out clipped and flat.
            let worth_it = if sentence {
                !self.started || spoken >= self.cfg.first_min_chars
            } else {
                spoken >= self.min_now()
            };
            if worth_it {
                return Some(end);
            }
        }

        if limit > self.cfg.max_chars {
            // A run-on with no punctuation at all. Cut at the last word break
            // inside the limit — never mid-word, and never off a char boundary.
            let mut cut = self.cfg.max_chars.min(limit);
            while cut > 0 && !self.buf.is_char_boundary(cut) {
                cut -= 1;
            }
            if let Some(sp) = self.buf[..cut].rfind(char::is_whitespace) {
                if sp + 1 > 0 {
                    return Some(sp + 1);
                }
            }
            if cut > 0 {
                return Some(cut);
            }
        }

        terminal.then_some(limit)
    }

    /// Is the word ending at byte `dot` an abbreviation or an initial?
    fn ends_with_abbreviation(&self, dot: usize) -> bool {
        let head = &self.buf[..dot];
        let word_start = head
            .rfind(|c: char| c.is_whitespace() || c == '(' || c == '"')
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &head[word_start..];
        if word.is_empty() {
            return false;
        }
        // A single letter is an initial: "J. R. R. Tolkien".
        let mut cs = word.chars();
        if let (Some(c), None) = (cs.next(), cs.next()) {
            if c.is_alphabetic() {
                return true;
            }
        }
        // "e.g." and "i.e." arrive here as "g" and "e" because of the inner
        // stop, so strip inner stops before matching.
        let flat: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        ABBREVIATIONS.contains(&flat.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(cs: &[Chunk]) -> Vec<&str> {
        cs.iter().map(|c| c.text.as_str()).collect()
    }

    /// Feed one character at a time — the way the model actually produces it.
    fn stream(text: &str) -> Vec<Chunk> {
        let mut c = Chunker::default();
        let mut out = Vec::new();
        for ch in text.chars() {
            c.push(&ch.to_string());
            out.extend(c.drain());
        }
        c.finish();
        out.extend(c.drain());
        out
    }

    #[test]
    fn a_stream_and_a_whole_string_cut_identically() {
        let t = "Your build is green. Nineteen tests passed, and the flaky one behaved.";
        assert_eq!(stream(t), Chunker::all(t), "streaming must not change the cuts");
    }

    #[test]
    fn the_first_chunk_leaves_early_so_she_starts_talking_soon() {
        let mut c = Chunker::default();
        c.push("Hey. ");
        let first = c.drain();
        assert_eq!(texts(&first), vec!["Hey."], "she must not wait for the paragraph");
    }

    #[test]
    fn later_chunks_wait_for_enough_words_to_have_prosody() {
        let mut c = Chunker::default();
        c.push("Hey. ");
        assert_eq!(c.drain().len(), 1);
        // A short clause on its own is held back now that she is already talking.
        c.push("So, ");
        assert!(c.drain().is_empty(), "a two-letter clause is not worth a cut");
        c.push("the reason your frame time doubled is the shader cache, ");
        assert_eq!(c.drain().len(), 1);
    }

    #[test]
    fn a_decimal_point_is_not_a_sentence() {
        assert_eq!(
            texts(&Chunker::all("The delta was 3.5 milliseconds over the budget you set.")),
            vec!["The delta was 3.5 milliseconds over the budget you set."]
        );
    }

    #[test]
    fn a_filename_and_a_domain_are_not_sentences() {
        for t in [
            "I patched main.rs and the tests went green again for you.",
            "It came from example.com and nothing about that looked deliberate.",
        ] {
            assert_eq!(Chunker::all(t).len(), 1, "{t}");
        }
    }

    /// Sentence boundaries only, so a test about abbreviations is not also a
    /// test about where commas fall.
    fn sentences_only(t: &str) -> Vec<Chunk> {
        let mut c = Chunker::new(ChunkConfig { clause_breaks: false, ..Default::default() });
        c.push(t);
        c.finish();
        c.drain()
    }

    #[test]
    fn abbreviations_do_not_end_sentences() {
        for t in [
            "Dr. Vega called about the thing you were dreading all week long.",
            "Bring a laptop, a charger, etc. and we will sort it out then.",
            "The paper by J. R. R. someone argues the opposite of that.",
            "It is 4 p.m. and you have not eaten anything at all today.",
        ] {
            assert_eq!(sentences_only(t).len(), 1, "{t} split into {:?}", sentences_only(t));
        }
    }

    #[test]
    fn an_abbreviation_still_ends_the_utterance_when_it_really_is_the_end() {
        // "…and so on, etc." with nothing after it must still come out.
        let cs = sentences_only("Bring the charger, the dongle, etc.");
        assert_eq!(cs.len(), 1);
        assert!(cs[0].text.ends_with("etc."));
    }

    #[test]
    fn a_real_sentence_boundary_does_split() {
        let cs = Chunker::all(
            "Your tests are green now. The flaky one has not failed in an hour of runs.",
        );
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].text, "Your tests are green now.");
    }

    #[test]
    fn trailing_quotes_and_brackets_stay_with_their_sentence() {
        let cs = Chunker::all("He wrote \"it compiles.\" Then he went to bed for the night.");
        assert_eq!(cs.len(), 2);
        assert!(cs[0].text.ends_with('"'), "the quote belongs to the sentence: {:?}", cs[0].text);
    }

    #[test]
    fn an_ellipsis_is_one_boundary_not_three() {
        let cs = Chunker::all("I would not do that... Not on a Friday afternoon, anyway.");
        assert_eq!(cs.len(), 2, "{:?}", texts(&cs));
        assert!(cs[0].text.ends_with("..."), "{:?}", cs[0].text);
    }

    #[test]
    fn a_thousands_separator_is_not_a_clause_break() {
        let cs = Chunker::all("It allocated 1,048,576 bytes before it settled down again.");
        assert_eq!(cs.len(), 1, "{:?}", texts(&cs));
    }

    #[test]
    fn a_blank_line_is_always_a_boundary() {
        let cs = Chunker::all("Done\n\nAlso: the disk is at ninety percent and climbing steadily.");
        assert!(cs.len() >= 2, "{:?}", texts(&cs));
        assert_eq!(cs[0].text, "Done");
    }

    #[test]
    fn a_run_on_with_no_punctuation_is_still_cut_at_a_word_break() {
        let long = "word ".repeat(120);
        let cs = Chunker::all(&long);
        assert!(cs.len() > 1, "a 600-character run-on must not be one chunk");
        for c in &cs {
            assert!(c.text.len() <= ChunkConfig::default().max_chars + 8, "{}", c.text.len());
            assert!(!c.text.starts_with("ord"), "cut mid-word: {:?}", c.text);
        }
    }

    #[test]
    fn nothing_is_emitted_until_a_boundary_is_actually_visible() {
        let mut c = Chunker::default();
        // The stop is there but we cannot yet see what follows it.
        c.push("The value is 3");
        assert!(c.drain().is_empty());
        c.push(".");
        assert!(c.drain().is_empty(), "a trailing stop might be a decimal point");
        c.push("5 and that is under budget for the frame.");
        c.finish();
        assert_eq!(c.drain().len(), 1);
    }

    #[test]
    fn a_code_fence_comes_out_unspoken() {
        let cs = Chunker::all("Try this:\n```rust\nfn main() {}\n```\nThat should build fine now.");
        let kinds: Vec<ChunkKind> = cs.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&ChunkKind::Code), "{:?}", cs);
        let code = cs.iter().find(|c| c.kind == ChunkKind::Code).unwrap();
        assert!(code.text.contains("fn main"));
        assert!(
            cs.iter().any(|c| c.kind == ChunkKind::Speech && c.text.contains("Try this")),
            "the prose before the fence must still be spoken: {:?}",
            texts(&cs)
        );
    }

    #[test]
    fn an_unclosed_fence_holds_until_the_stream_ends() {
        let mut c = Chunker::default();
        c.push("Here you go:\n```\nfn main() {\n");
        let before = c.drain();
        assert!(
            !before.iter().any(|c| c.kind == ChunkKind::Code),
            "an open fence must not be cut in half"
        );
        c.finish();
        let after = c.drain();
        assert!(after.iter().any(|c| c.kind == ChunkKind::Code), "{:?}", after);
    }

    #[test]
    fn finish_flushes_a_fragment_with_no_punctuation_at_all() {
        let mut c = Chunker::default();
        c.push("no punctuation here");
        assert!(c.drain().is_empty());
        c.finish();
        assert_eq!(texts(&c.drain()), vec!["no punctuation here"]);
        assert!(c.is_finished());
    }

    #[test]
    fn clear_sheds_rather_than_queues() {
        // SPEC §3.1: barge-in must drop the rest, not save it for later.
        let mut c = Chunker::default();
        c.push("First thing. And then the second thing, which she never reaches.");
        let _ = c.next_chunk();
        assert!(!c.pending().is_empty());
        c.clear();
        assert!(c.pending().is_empty());
        assert!(c.drain().is_empty());
    }

    #[test]
    fn empty_and_whitespace_only_input_produce_no_chunks() {
        assert!(Chunker::all("").is_empty());
        assert!(Chunker::all("   \n\n  \t ").is_empty());
    }

    #[test]
    fn multibyte_text_never_panics_on_a_byte_boundary() {
        for t in [
            "Grüße. Die Tests sind grün und der Build ist durch.",
            "日本語のテキストです。これは二番目の文です。",
            "emoji 🎧 in the middle of a sentence that keeps going for a while.",
            "—".repeat(400).as_str(),
        ] {
            let cs = Chunker::all(t);
            let joined: String = cs.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("");
            assert!(!joined.is_empty() || t.trim().is_empty(), "{t}");
        }
    }

    #[test]
    fn no_text_is_ever_lost_or_duplicated() {
        let t = "One thing happened. Then another, and a third; finally a fourth thing landed.";
        let got: String = Chunker::all(t)
            .iter()
            .map(|c| c.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(norm(&got), norm(t));
    }

    #[test]
    fn no_text_is_lost_when_streamed_one_character_at_a_time_either() {
        let t = "Careful now. The disk is at 3.5 percent free, i.e. almost nothing; back up.";
        let got: String = stream(t).iter().map(|c| c.text.clone()).collect::<Vec<_>>().join(" ");
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(norm(&got), norm(t));
    }
}
