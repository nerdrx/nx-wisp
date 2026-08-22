//! **F15 — the prompt/KV cache.**
//!
//! > *Persistent KV cache for the persona/system prompt so the first token of
//! > every reply is instant; cache slots per conversation.*
//!
//! Prefill is quadratic-ish and decode is linear, so on a two-thousand-token
//! persona prompt the *first* token costs more than the next hundred. The fix
//! is not to shorten the persona; it is to never compute it twice.
//!
//! ## Slot 0 is the persona, and it is never evicted
//!
//! [`SlotId::PERSONA`] holds exactly the persona prefix and nothing else. Every
//! conversation slot is seeded from it — in llama.cpp terms, a sequence copy of
//! the cached cells — so a brand-new conversation starts with the whole persona
//! already prefilled and pays only for what the operator actually said.
//!
//! ## Which is why the persona prompt has to be a *fixed* prefix
//!
//! F19 modulates the system prompt by mood, and that is exactly the thing that
//! would destroy this: change one character of the prefix and every cached cell
//! after it is wrong. So [`crate::prompt`] splits the system message in two —
//! an immutable persona core, then a volatile state block *after* it. The mood
//! still reaches the model; it just does not sit in front of the cache.
//! [`KvCache::persona_is_stable`] is how a test proves it stayed that way.

use std::collections::BTreeMap;

use wisp_proto::Millis;

use crate::backend::{SlotId, Token};

pub type ConversationId = u64;

#[derive(Debug, Clone, PartialEq)]
pub struct CacheSlot {
    pub id: SlotId,
    pub conversation: Option<ConversationId>,
    /// The exact token sequence the backend currently holds for this slot.
    pub tokens: Vec<Token>,
    pub last_used: Millis,
    /// Slot 0. Never handed out, never evicted.
    pub pinned: bool,
}

impl CacheSlot {
    fn empty(id: SlotId, pinned: bool) -> Self {
        CacheSlot {
            id,
            conversation: None,
            tokens: Vec::new(),
            last_used: 0,
            pinned,
        }
    }
}

/// What a turn will cost before it is run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub slot: SlotId,
    /// Tokens already in the slot that the prompt agrees with.
    pub reuse: usize,
    /// Tokens that must actually be prefilled.
    pub prefill: usize,
    /// The slot was empty and is being seeded from [`SlotId::PERSONA`], so
    /// `reuse` is free even though this conversation has never run before.
    pub seeded_from_persona: bool,
    /// A conversation was thrown out to make room. Recorded rather than
    /// silent: "she forgot what we were talking about" should be answerable.
    pub evicted: Option<ConversationId>,
}

impl Plan {
    pub fn hit_rate(&self) -> f32 {
        let total = self.reuse + self.prefill;
        if total == 0 {
            return 0.0;
        }
        self.reuse as f32 / total as f32
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub turns: u64,
    pub reused_tokens: u64,
    pub prefilled_tokens: u64,
    pub evictions: u64,
    pub persona_seeds: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f32 {
        let total = self.reused_tokens + self.prefilled_tokens;
        if total == 0 {
            return 0.0;
        }
        (self.reused_tokens as f64 / total as f64) as f32
    }
}

#[derive(Debug, Clone)]
pub struct KvCache {
    slots: Vec<CacheSlot>,
    by_conversation: BTreeMap<ConversationId, SlotId>,
    persona: Vec<Token>,
    stats: CacheStats,
}

impl KvCache {
    /// `conversations` is how many conversations can be warm at once, over and
    /// above the persona slot. Two is enough for "the operator, and whatever
    /// she was muttering to herself about"; more costs context memory per slot.
    pub fn new(conversations: usize) -> Self {
        let n = conversations.max(1);
        let mut slots = vec![CacheSlot::empty(SlotId::PERSONA, true)];
        for i in 0..n {
            slots.push(CacheSlot::empty(SlotId(i as u32 + 1), false));
        }
        KvCache {
            slots,
            by_conversation: BTreeMap::new(),
            persona: Vec::new(),
            stats: CacheStats::default(),
        }
    }

    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }
    pub fn persona_tokens(&self) -> &[Token] {
        &self.persona
    }
    pub fn slot(&self, id: SlotId) -> Option<&CacheSlot> {
        self.slots.iter().find(|s| s.id == id)
    }
    pub fn conversations(&self) -> Vec<ConversationId> {
        self.by_conversation.keys().copied().collect()
    }

    /// Install the persona prefix. Returns `true` if it changed — which means
    /// every conversation slot's cache is now wrong and has been dropped.
    ///
    /// A caller that finds this returning `true` on every turn has a persona
    /// that is not actually fixed, and has silently turned F15 off.
    pub fn set_persona(&mut self, tokens: Vec<Token>) -> bool {
        if self.persona == tokens {
            return false;
        }
        tracing::debug!(
            was = self.persona.len(),
            now = tokens.len(),
            "persona prefix changed; every conversation cache is invalidated"
        );
        self.persona = tokens.clone();
        for s in &mut self.slots {
            s.tokens.clear();
            s.conversation = None;
        }
        self.by_conversation.clear();
        if let Some(p) = self.slots.iter_mut().find(|s| s.id == SlotId::PERSONA) {
            p.tokens = tokens;
        }
        true
    }

    /// Does `prompt` still begin with the cached persona? The assertion F15
    /// rests on.
    pub fn persona_is_stable(&self, prompt: &[Token]) -> bool {
        !self.persona.is_empty() && prompt.starts_with(&self.persona)
    }

    /// Get a slot for this conversation, evicting the least recently used one
    /// if every slot is taken.
    pub fn acquire(&mut self, conversation: ConversationId, now: Millis) -> (SlotId, Option<ConversationId>) {
        if let Some(id) = self.by_conversation.get(&conversation).copied() {
            if let Some(s) = self.slots.iter_mut().find(|s| s.id == id) {
                s.last_used = now;
            }
            return (id, None);
        }
        // A free slot first.
        if let Some(s) = self
            .slots
            .iter_mut()
            .find(|s| !s.pinned && s.conversation.is_none())
        {
            s.conversation = Some(conversation);
            s.last_used = now;
            let id = s.id;
            self.by_conversation.insert(conversation, id);
            return (id, None);
        }
        // Otherwise the oldest.
        let victim = self
            .slots
            .iter_mut()
            .filter(|s| !s.pinned)
            .min_by_key(|s| s.last_used)
            .expect("a cache with no unpinned slots cannot be acquired from");
        let evicted = victim.conversation;
        victim.conversation = Some(conversation);
        victim.tokens.clear();
        victim.last_used = now;
        let id = victim.id;
        if let Some(old) = evicted {
            self.by_conversation.remove(&old);
        }
        self.by_conversation.insert(conversation, id);
        self.stats.evictions += 1;
        (id, evicted)
    }

    /// What running `prompt` in `slot` would cost.
    pub fn plan(&self, slot: SlotId, prompt: &[Token]) -> Plan {
        let cached = self
            .slot(slot)
            .map(|s| s.tokens.as_slice())
            .unwrap_or(&[]);
        let mut reuse = common_prefix(cached, prompt);
        let mut seeded = false;
        // An empty (or freshly evicted) slot still gets the persona for free,
        // by copying the cells slot 0 is holding.
        if reuse == 0 && self.persona_is_stable(prompt) {
            reuse = self.persona.len();
            seeded = true;
        }
        Plan {
            slot,
            reuse,
            prefill: prompt.len().saturating_sub(reuse),
            seeded_from_persona: seeded,
            evicted: None,
        }
    }

    /// Convenience: acquire and plan in one step.
    pub fn plan_for(
        &mut self,
        conversation: ConversationId,
        prompt: &[Token],
        now: Millis,
    ) -> Plan {
        let (slot, evicted) = self.acquire(conversation, now);
        let mut plan = self.plan(slot, prompt);
        plan.evicted = evicted;
        plan
    }

    /// Record what the backend now holds, after a turn. `tokens` is the prompt
    /// *plus* whatever was generated, because that is what is in the cache.
    pub fn commit(&mut self, plan: &Plan, tokens: Vec<Token>, now: Millis) {
        self.stats.turns += 1;
        self.stats.reused_tokens += plan.reuse as u64;
        self.stats.prefilled_tokens += plan.prefill as u64;
        if plan.seeded_from_persona {
            self.stats.persona_seeds += 1;
        }
        if let Some(s) = self.slots.iter_mut().find(|s| s.id == plan.slot) {
            s.tokens = tokens;
            s.last_used = now;
        }
    }

    /// Forget one conversation — "start again" — without touching the persona.
    pub fn forget(&mut self, conversation: ConversationId) -> Option<SlotId> {
        let id = self.by_conversation.remove(&conversation)?;
        if let Some(s) = self.slots.iter_mut().find(|s| s.id == id) {
            s.tokens.clear();
            s.conversation = None;
        }
        Some(id)
    }

    /// Everything except the persona. What a T3 downgrade does: the contexts
    /// are gone with the model, but the persona prefix is text and survives.
    pub fn clear_conversations(&mut self) {
        for s in &mut self.slots {
            if !s.pinned {
                s.tokens.clear();
                s.conversation = None;
            }
        }
        self.by_conversation.clear();
    }
}

fn common_prefix(a: &[Token], b: &[Token]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(base: i32, n: usize) -> Vec<Token> {
        (0..n).map(|i| base + i as i32).collect()
    }

    #[test]
    fn a_new_conversation_still_gets_the_persona_for_free() {
        let mut c = KvCache::new(2);
        let persona = toks(1, 100);
        c.set_persona(persona.clone());

        let mut prompt = persona.clone();
        prompt.extend(toks(9000, 7));
        let plan = c.plan_for(1, &prompt, 10);
        assert!(plan.seeded_from_persona);
        assert_eq!(plan.reuse, 100);
        assert_eq!(plan.prefill, 7);
    }

    #[test]
    fn a_second_turn_reuses_the_whole_first_turn() {
        let mut c = KvCache::new(2);
        let persona = toks(1, 50);
        c.set_persona(persona.clone());

        let mut first = persona.clone();
        first.extend(toks(500, 10));
        let p1 = c.plan_for(7, &first, 10);
        let mut after = first.clone();
        after.extend(toks(700, 12)); // what she said
        c.commit(&p1, after.clone(), 11);

        let mut second = after.clone();
        second.extend(toks(900, 4)); // and what he said next
        let p2 = c.plan_for(7, &second, 20);
        assert!(!p2.seeded_from_persona);
        assert_eq!(p2.reuse, after.len());
        assert_eq!(p2.prefill, 4);
        assert!(p2.hit_rate() > 0.9);
    }

    #[test]
    fn the_least_recently_used_conversation_is_the_one_that_goes() {
        let mut c = KvCache::new(2);
        c.set_persona(toks(1, 4));
        let p = |n: u64| {
            let mut v = toks(1, 4);
            v.push(n as i32 + 100);
            v
        };
        let a = c.plan_for(1, &p(1), 10);
        c.commit(&a, p(1), 10);
        let b = c.plan_for(2, &p(2), 20);
        c.commit(&b, p(2), 20);
        // Touch conversation 1 so 2 is the oldest.
        let a2 = c.plan_for(1, &p(1), 30);
        c.commit(&a2, p(1), 30);

        let third = c.plan_for(3, &p(3), 40);
        assert_eq!(third.evicted, Some(2));
        assert_eq!(c.stats().evictions, 1);
        assert!(c.conversations().contains(&1));
        assert!(!c.conversations().contains(&2));
    }

    #[test]
    fn changing_the_persona_invalidates_everything_and_says_so() {
        let mut c = KvCache::new(2);
        assert!(c.set_persona(toks(1, 10)));
        let mut prompt = toks(1, 10);
        prompt.extend(toks(50, 3));
        let plan = c.plan_for(1, &prompt, 5);
        c.commit(&plan, prompt.clone(), 5);

        // Same persona: no invalidation, cache intact.
        assert!(!c.set_persona(toks(1, 10)));
        assert_eq!(c.plan_for(1, &prompt, 6).reuse, prompt.len());

        // Different persona: everything goes.
        assert!(c.set_persona(toks(2, 10)));
        assert!(c.conversations().is_empty());
    }
}
