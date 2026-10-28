//! Compiles regex literals ahead of time, with the semantics of Rust's
//! `regex::bytes`: the same syntax (parsed by `regex-syntax`), Unicode classes,
//! leftmost-first matching, lazy quantifiers and look-around assertions.
//!
//! A pattern becomes three things for the runtime:
//! - a Thompson NFA program with capture slots, run by a Pike VM to find capture
//!   groups, and to search when no DFA could be built;
//! - a forward DFA that scans unanchored and reports where the leftmost-first
//!   match ends;
//! - a reverse DFA that scans back from that end to find where it starts.
//!
//! Look-around assertions are handled the way `regex-automata` does: a DFA state
//! remembers what the previous byte was (start of text, `\n`, a word byte), and a
//! match is reported one byte late, when the next byte (or the end of the text)
//! is known. Unicode word boundaries are decided as ASCII ones while the text is
//! ASCII; the DFA gives up on the first non-ASCII byte and the Pike VM takes over.

use std::collections::HashMap;

use regex_syntax::hir::{self, Class, Hir, HirKind, Look};
use regex_syntax::utf8::Utf8Sequences;

/// Patterns whose DFAs would be larger than this are searched with the Pike VM.
const MAX_DFA_STATES: usize = 2000;
/// Patterns whose NFA would be larger than this are rejected.
const MAX_INSTS: usize = 250_000;

/// DFA state ids with fixed meanings.
const DEAD: usize = 0;
const QUIT: usize = 1;

#[derive(Clone, Copy, Debug)]
pub enum Inst {
    Range(u8, u8, usize),
    /// Try the first branch before the second.
    Split(usize, usize),
    /// Record the position in a capture slot.
    Save(usize, usize),
    Look(Look, usize),
    Match,
    Fail,
}

/// A dense DFA laid out like regex-automata's: states are identified by their
/// offset into `trans` (premultiplied by the stride), so a step is one load, and
/// the special states come first, so one comparison spots them all.
pub struct Dfa {
    /// `trans[state + class]` is the next state; the last class is end of text.
    pub trans: Vec<u32>,
    /// Start states for: start of text, after `\n`, after a word byte, after another byte.
    pub start: [u32; 4],
    /// State 0 is dead and state `stride` gives up (non-ASCII text). States in
    /// `match_lo..match_hi` are reached when a match ended just before the byte
    /// that led to them. States in `match_hi..special` have no match in progress,
    /// so the search may skip ahead.
    pub match_lo: u32,
    pub match_hi: u32,
    pub special: u32,
}

pub struct Regex {
    /// Forward program; `start` is anchored, slots 0 and 1 hold the whole match.
    pub prog: Vec<Inst>,
    pub start: usize,
    pub ngroups: usize,
    pub names: Vec<Option<String>>,
    pub classes: [u8; 256],
    /// Number of byte classes, plus one for end of text.
    pub stride: usize,
    pub fwd: Option<Dfa>,
    pub rev: Option<Dfa>,
    /// Every match begins with these bytes.
    pub prefix: Vec<u8>,
    /// Bytes that can begin a match (all of them when a match can be empty).
    pub first: [bool; 256],
    pub can_be_empty: bool,
    pub anchored_start: bool,
    /// Whether the Pike VM needs the Unicode word-character table.
    pub unicode_word: bool,
}

pub fn compile(pattern: &str) -> Result<Regex, String> {
    let hir = regex_syntax::ParserBuilder::new().utf8(false).build().parse(pattern).map_err(|e| {
        // regex-syntax errors span several lines (pattern, caret, message); keep the message.
        let text = e.to_string();
        text.lines().last().unwrap_or("").trim_start_matches("error: ").to_string()
    })?;
    let props = hir.properties();
    let ngroups = props.explicit_captures_len() + 1;
    let mut names = vec![None; ngroups];
    capture_names(&hir, &mut names);

    let mut fwd = Nfa::default();
    let m = fwd.push(Inst::Match);
    let end = fwd.push(Inst::Save(1, m));
    let body = fwd.compile(&hir, end, false)?;
    let start = fwd.push(Inst::Save(0, body));
    // The unanchored prefix `(?s-u:.)*?`: the loop has the lowest priority, so it
    // stops starting new threads once a match is found.
    let unanchored = fwd.push(Inst::Fail);
    let any = fwd.push(Inst::Range(0, 255, unanchored));
    fwd.insts[unanchored] = Inst::Split(start, any);

    let mut rev = Nfa::default();
    let m = rev.push(Inst::Match);
    let rev_start = rev.compile(&hir, m, true)?;

    let looks = props.look_set();
    let crlf = looks.contains(Look::StartCRLF) || looks.contains(Look::EndCRLF);
    let unicode_word = [
        Look::WordUnicode,
        Look::WordUnicodeNegate,
        Look::WordStartUnicode,
        Look::WordEndUnicode,
        Look::WordStartHalfUnicode,
        Look::WordEndHalfUnicode,
    ]
    .iter()
    .any(|&l| looks.contains(l));
    let has_look = !looks.is_empty();

    let can_be_empty = props.minimum_len() == Some(0);
    let mut first = [true; 256];
    let mut prefix = Vec::new();
    if !can_be_empty {
        first = [false; 256];
        for id in closure(&fwd.insts, &[start], None, None, false).0 {
            if let Inst::Range(lo, hi, _) = fwd.insts[id] {
                (lo..=hi).for_each(|b| first[b as usize] = true);
            }
        }
        let seq = hir::literal::Extractor::new().extract(&hir);
        if let Some(lcp) = seq.longest_common_prefix() {
            prefix = lcp.iter().take(64).copied().collect();
        }
    }

    // Skipping ahead only pays when the prefilter is fast: a literal, or a few bytes.
    let fast_prefilter = !prefix.is_empty() || first.iter().filter(|&&f| f).count() <= 4;

    let (classes, stride, reps) = byte_classes(&fwd.insts, has_look, unicode_word);
    let anchored_start = props.look_set_prefix().contains(Look::Start);
    let dfa_start = if anchored_start { start } else { unanchored };
    let (fwd_dfa, rev_dfa) = if crlf {
        (None, None)
    } else {
        let skippable = !anchored_start && !can_be_empty && fast_prefilter;
        let f = build_dfa(&fwd.insts, dfa_start, true, skippable, stride, &reps, has_look, unicode_word);
        let r = build_dfa(&rev.insts, rev_start, false, false, stride, &reps, has_look, unicode_word);
        match (f, r) {
            (Some(f), Some(r)) => (Some(f), Some(r)),
            _ => (None, None),
        }
    };

    Ok(Regex {
        prog: fwd.insts,
        start,
        ngroups,
        names,
        classes,
        stride,
        fwd: fwd_dfa,
        rev: rev_dfa,
        prefix,
        first,
        can_be_empty,
        anchored_start,
        unicode_word,
    })
}

fn capture_names(h: &Hir, names: &mut [Option<String>]) {
    match h.kind() {
        HirKind::Capture(c) => {
            names[c.index as usize] = c.name.as_ref().map(|n| n.to_string());
            capture_names(&c.sub, names);
        }
        HirKind::Repetition(r) => capture_names(&r.sub, names),
        HirKind::Concat(items) | HirKind::Alternation(items) => items.iter().for_each(|h| capture_names(h, names)),
        _ => {}
    }
}

/// The Unicode `\w` ranges, for Unicode word boundaries in the Pike VM.
pub fn unicode_word_ranges() -> Vec<(u32, u32)> {
    let hir = regex_syntax::Parser::new().parse(r"\w").expect("\\w parses");
    match hir.kind() {
        HirKind::Class(Class::Unicode(c)) => c.ranges().iter().map(|r| (r.start() as u32, r.end() as u32)).collect(),
        _ => unreachable!("\\w is a Unicode class"),
    }
}

#[derive(Default)]
struct Nfa {
    insts: Vec<Inst>,
}

impl Nfa {
    fn push(&mut self, inst: Inst) -> usize {
        self.insts.push(inst);
        self.insts.len() - 1
    }

    /// Alternation of `starts`, earlier ones first.
    fn alt(&mut self, starts: Vec<usize>) -> usize {
        let mut it = starts.into_iter().rev();
        let mut acc = it.next().unwrap_or_else(|| self.push(Inst::Fail));
        for s in it {
            acc = self.push(Inst::Split(s, acc));
        }
        acc
    }

    /// Compiles `h` so that completing it continues at `next`; returns its start.
    /// In reverse, the program matches the pattern's text backwards.
    fn compile(&mut self, h: &Hir, next: usize, rev: bool) -> Result<usize, String> {
        if self.insts.len() > MAX_INSTS {
            return Err("pattern is too large".to_string());
        }
        Ok(match h.kind() {
            HirKind::Empty => next,
            HirKind::Literal(lit) => self.bytes(lit.0.iter().map(|&b| (b, b)).collect(), next, rev),
            HirKind::Class(Class::Bytes(c)) => {
                let starts = c.ranges().iter().map(|r| self.push(Inst::Range(r.start(), r.end(), next))).collect();
                self.alt(starts)
            }
            HirKind::Class(Class::Unicode(c)) => {
                let mut starts = Vec::new();
                for r in c.ranges() {
                    for seq in Utf8Sequences::new(r.start(), r.end()) {
                        let ranges = seq.as_slice().iter().map(|r| (r.start, r.end)).collect();
                        starts.push(self.bytes(ranges, next, rev));
                    }
                }
                self.alt(starts)
            }
            HirKind::Look(look) => self.push(Inst::Look(if rev { mirror(*look) } else { *look }, next)),
            HirKind::Repetition(r) => {
                // Built the way regex-automata builds them: with leftmost-first
                // matching, the shape decides which of several equal-length paths
                // wins when the repeated part can match the empty string.
                let greedy = r.greedy;
                let split = |body: usize, exit: usize| if greedy { Inst::Split(body, exit) } else { Inst::Split(exit, body) };
                match (r.min, r.max) {
                    (0, None) if r.sub.properties().minimum_len() != Some(0) => {
                        let lp = self.push(Inst::Fail);
                        let body = self.compile(&r.sub, lp, rev)?;
                        self.insts[lp] = split(body, next);
                        lp
                    }
                    (0, None) => {
                        // x* as (x+)?, see rust-lang/regex#779.
                        let plus = self.push(Inst::Fail);
                        let body = self.compile(&r.sub, plus, rev)?;
                        self.insts[plus] = split(body, next);
                        self.push(split(body, next))
                    }
                    (min, None) => {
                        // x{n,}: n - 1 copies, then a last copy that loops onto itself.
                        let lp = self.push(Inst::Fail);
                        let last = self.compile(&r.sub, lp, rev)?;
                        self.insts[lp] = split(last, next);
                        let mut tail = last;
                        for _ in 1..min {
                            tail = self.compile(&r.sub, tail, rev)?;
                        }
                        tail
                    }
                    (min, Some(max)) => {
                        // x{n,m}: n copies, then nested optional copies.
                        let mut tail = next;
                        for _ in min..max {
                            let body = self.compile(&r.sub, tail, rev)?;
                            tail = self.push(split(body, next));
                        }
                        for _ in 0..min {
                            tail = self.compile(&r.sub, tail, rev)?;
                        }
                        tail
                    }
                }
            }
            HirKind::Capture(c) if !rev => {
                let slot = 2 * c.index as usize;
                let end = self.push(Inst::Save(slot + 1, next));
                let body = self.compile(&c.sub, end, rev)?;
                self.push(Inst::Save(slot, body))
            }
            HirKind::Capture(c) => self.compile(&c.sub, next, rev)?,
            HirKind::Concat(items) => {
                let mut next = next;
                if rev {
                    for item in items {
                        next = self.compile(item, next, rev)?;
                    }
                } else {
                    for item in items.iter().rev() {
                        next = self.compile(item, next, rev)?;
                    }
                }
                next
            }
            HirKind::Alternation(alts) => {
                let mut starts = Vec::with_capacity(alts.len());
                for a in alts {
                    starts.push(self.compile(a, next, rev)?);
                }
                self.alt(starts)
            }
        })
    }

    /// A sequence of byte ranges, matched in order (or reversed, backwards).
    fn bytes(&mut self, ranges: Vec<(u8, u8)>, next: usize, rev: bool) -> usize {
        let mut next = next;
        let order: Box<dyn Iterator<Item = &(u8, u8)>> =
            if rev { Box::new(ranges.iter()) } else { Box::new(ranges.iter().rev()) };
        for &(lo, hi) in order {
            next = self.push(Inst::Range(lo, hi, next));
        }
        next
    }
}

/// The assertion that means the same thing when the text is read backwards.
fn mirror(look: Look) -> Look {
    match look {
        Look::Start => Look::End,
        Look::End => Look::Start,
        Look::StartLF => Look::EndLF,
        Look::EndLF => Look::StartLF,
        Look::StartCRLF => Look::EndCRLF,
        Look::EndCRLF => Look::StartCRLF,
        Look::WordStartAscii => Look::WordEndAscii,
        Look::WordEndAscii => Look::WordStartAscii,
        Look::WordStartUnicode => Look::WordEndUnicode,
        Look::WordEndUnicode => Look::WordStartUnicode,
        Look::WordStartHalfAscii => Look::WordEndHalfAscii,
        Look::WordEndHalfAscii => Look::WordStartHalfAscii,
        Look::WordStartHalfUnicode => Look::WordEndHalfUnicode,
        Look::WordEndHalfUnicode => Look::WordStartHalfUnicode,
        other => other,
    }
}

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// What a DFA state knows about the byte before the current position.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct Ctx {
    at_start: bool,
    after_lf: bool,
    after_word: bool,
}

impl Ctx {
    fn after(b: u8) -> Ctx {
        Ctx { at_start: false, after_lf: b == b'\n', after_word: is_word_byte(b) }
    }
}

/// Whether `look` holds between the previous byte (`ctx`) and `next` (None at
/// end of text). Unicode word assertions are answered as ASCII ones: the DFA
/// only runs them on ASCII text.
fn look_holds(look: Look, ctx: Ctx, next: Option<u8>) -> bool {
    let next_word = next.is_some_and(is_word_byte);
    let prev_word = ctx.after_word;
    match look {
        Look::Start => ctx.at_start,
        Look::End => next.is_none(),
        Look::StartLF => ctx.at_start || ctx.after_lf,
        Look::EndLF => next.is_none() || next == Some(b'\n'),
        Look::WordAscii | Look::WordUnicode => prev_word != next_word,
        Look::WordAsciiNegate | Look::WordUnicodeNegate => prev_word == next_word,
        Look::WordStartAscii | Look::WordStartUnicode => !prev_word && next_word,
        Look::WordEndAscii | Look::WordEndUnicode => prev_word && !next_word,
        Look::WordStartHalfAscii | Look::WordStartHalfUnicode => !prev_word,
        Look::WordEndHalfAscii | Look::WordEndHalfUnicode => !next_word,
        Look::StartCRLF | Look::EndCRLF => unreachable!("patterns with CRLF assertions have no DFA"),
    }
}

/// The byte-consuming and matching states reachable from `list` without input,
/// in priority order. `ctx`/`next` decide assertions; with `ctx` None every
/// assertion is assumed to hold. With `cut`, everything after the first match
/// is dropped (leftmost-first). Also returns whether a match was reached.
fn closure(insts: &[Inst], list: &[usize], ctx: Option<Ctx>, next: Option<u8>, cut: bool) -> (Vec<usize>, bool) {
    let mut seen = vec![false; insts.len()];
    let mut out = Vec::new();
    let mut matched = false;
    for &s in list {
        let mut stack = vec![s];
        while let Some(id) = stack.pop() {
            if std::mem::replace(&mut seen[id], true) {
                continue;
            }
            match insts[id] {
                Inst::Range(..) => out.push(id),
                Inst::Match => {
                    out.push(id);
                    matched = true;
                    if cut {
                        return (out, true);
                    }
                }
                Inst::Split(a, b) => {
                    stack.push(b);
                    stack.push(a);
                }
                Inst::Save(_, n) => stack.push(n),
                Inst::Look(look, n) => {
                    if ctx.is_none_or(|ctx| look_holds(look, ctx, next)) {
                        stack.push(n);
                    }
                }
                Inst::Fail => {}
            }
        }
    }
    (out, matched)
}

/// Partitions bytes into classes that every transition treats alike. Returns the
/// class of each byte, the stride (classes plus end of text) and a representative
/// byte per class.
fn byte_classes(insts: &[Inst], has_look: bool, quit_non_ascii: bool) -> ([u8; 256], usize, Vec<u8>) {
    let mut boundary = [false; 257];
    let mut mark = |lo: u8, hi: u8| {
        boundary[lo as usize] = true;
        boundary[hi as usize + 1] = true;
    };
    for inst in insts {
        if let Inst::Range(lo, hi, _) = *inst {
            mark(lo, hi);
        }
    }
    if has_look {
        mark(b'\n', b'\n');
        for (lo, hi) in [(b'0', b'9'), (b'A', b'Z'), (b'_', b'_'), (b'a', b'z')] {
            mark(lo, hi);
        }
    }
    if quit_non_ascii {
        mark(0x80, 0xFF);
    }
    let mut classes = [0u8; 256];
    let mut reps = vec![0u8];
    let mut class = 0;
    for b in 0..256 {
        if b > 0 && boundary[b] {
            class += 1;
            reps.push(b as u8);
        }
        classes[b] = class as u8;
    }
    (classes, reps.len() + 1, reps)
}

/// Builds a DFA by subset construction over ordered NFA state lists. With
/// `leftmost_first`, lower-priority threads are dropped once a match is found;
/// otherwise every match is kept (the reverse DFA wants the longest one).
/// `skippable` marks the unanchored start state, where no match is in progress.
#[allow(clippy::too_many_arguments)]
fn build_dfa(
    insts: &[Inst],
    start: usize,
    leftmost_first: bool,
    skippable: bool,
    stride: usize,
    reps: &[u8],
    has_look: bool,
    quit_non_ascii: bool,
) -> Option<Dfa> {
    // A state is its ordered NFA thread list, what the previous byte was, and
    // whether a match ended just before the byte that led to it.
    type Key = (Vec<usize>, Ctx, bool);
    let mut ids: HashMap<Key, usize> = HashMap::new();
    let mut states: Vec<Key> = vec![(Vec::new(), Ctx::default(), false); 2];
    let mut intern = |list: Vec<usize>, ctx: Ctx, matched: bool, states: &mut Vec<Key>| -> Option<usize> {
        if list.is_empty() && !matched {
            return Some(DEAD);
        }
        let key = (list, if has_look { ctx } else { Ctx::default() }, matched);
        if let Some(&id) = ids.get(&key) {
            return Some(id);
        }
        if states.len() >= MAX_DFA_STATES {
            return None;
        }
        ids.insert(key.clone(), states.len());
        states.push(key);
        Some(states.len() - 1)
    };
    let contexts = [
        Ctx { at_start: true, ..Ctx::default() },
        Ctx { after_lf: true, ..Ctx::default() },
        Ctx { after_word: true, ..Ctx::default() },
        Ctx::default(),
    ];
    let mut start_ids = [0usize; 4];
    for (k, ctx) in contexts.iter().enumerate() {
        start_ids[k] = intern(vec![start], *ctx, false, &mut states)?;
    }

    // Transitions by plain state id, filled in breadth-first order.
    let mut raw = vec![DEAD; 2 * stride];
    raw[stride..2 * stride].fill(QUIT);
    let mut i = 2;
    while i < states.len() {
        let (list, ctx, _) = states[i].clone();
        for k in 0..stride {
            let next = reps.get(k).copied().filter(|_| k + 1 < stride);
            let target = if quit_non_ascii && next.is_some_and(|b| b >= 0x80) {
                QUIT
            } else {
                let (out, matched) = closure(insts, &list, Some(ctx), next, leftmost_first);
                let mut stepped = Vec::new();
                if let Some(b) = next {
                    let mut seen = vec![false; insts.len()];
                    for &id in &out {
                        if let Inst::Range(lo, hi, to) = insts[id] {
                            if lo <= b && b <= hi && !std::mem::replace(&mut seen[to], true) {
                                stepped.push(to);
                            }
                        }
                    }
                }
                let ctx = next.map_or(Ctx::default(), Ctx::after);
                intern(stepped, ctx, matched, &mut states)?
            };
            raw.push(target);
        }
        i += 1;
    }

    // Renumber: dead, quit, match states, skip states, then the rest.
    let is_skip = |(list, _, matched): &Key| skippable && !matched && list.as_slice() == [start];
    let mut order: Vec<usize> = vec![DEAD, QUIT];
    order.extend((2..states.len()).filter(|&i| states[i].2));
    let match_hi = order.len();
    order.extend((2..states.len()).filter(|&i| is_skip(&states[i])));
    let special = order.len();
    order.extend((2..states.len()).filter(|&i| !states[i].2 && !is_skip(&states[i])));
    let mut new_id = vec![0usize; states.len()];
    for (new, &old) in order.iter().enumerate() {
        new_id[old] = new;
    }
    let offset = |old: usize| (new_id[old] * stride) as u32;
    let mut trans = Vec::with_capacity(raw.len());
    for &old in &order {
        trans.extend(raw[old * stride..(old + 1) * stride].iter().map(|&t| offset(t)));
    }
    Some(Dfa {
        trans,
        start: start_ids.map(offset),
        match_lo: (2 * stride) as u32,
        match_hi: (match_hi * stride) as u32,
        special: (special * stride) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefix_and_first_bytes() {
        assert_eq!(compile(r"status=5\d{2}").unwrap().prefix, b"status=5");
        assert_eq!(compile(r"ms=\d+").unwrap().prefix, b"ms=");
        let re = compile("(ERROR|WARN)").unwrap();
        assert!(re.prefix.is_empty());
        assert!(re.first[b'E' as usize] && re.first[b'W' as usize] && !re.first[b'I' as usize]);
        assert!(compile("a*").unwrap().can_be_empty);
    }

    #[test]
    fn dfa_or_fallback() {
        assert!(compile(r"\bfoo\b").unwrap().fwd.is_some(), "word boundaries run in the DFA on ASCII");
        assert!(compile(r"(?m)^a$").unwrap().fwd.is_some());
        assert!(compile(r"(?mR)^a$").unwrap().fwd.is_none(), "CRLF mode uses the Pike VM");
        assert!(compile(r"(a|b)*a(a|b){14}").unwrap().fwd.is_none(), "exponential DFAs use the Pike VM");
    }

    #[test]
    fn errors() {
        for (pattern, want) in [("a(b", "unclosed group"), ("[ab", "unclosed character class"), ("*a", "repetition operator missing expression")] {
            let err = compile(pattern).err().unwrap_or_else(|| panic!("`{pattern}` compiled"));
            assert!(err.contains(want), "`{pattern}`: {err}");
        }
    }
}
