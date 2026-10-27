//! Compiles regex literals to DFAs at lamda compile time.
//!
//! Patterns are byte-oriented: the parser builds a syntax tree, which becomes a
//! Thompson NFA, which subset construction turns into a DFA table the runtime
//! walks one byte at a time. Matching is leftmost-longest (POSIX); for most
//! patterns that is the same match a backtracking engine would report.

use std::collections::HashMap;

/// DFAs larger than this are rejected at compile time.
const MAX_STATES: usize = 1000;

pub struct Dfa {
    /// `next[state * 256 + byte]`; state 0 is the dead state and 1 the start.
    pub next: Vec<u16>,
    pub accept: Vec<bool>,
    /// Every match starts with these bytes.
    pub prefix: Vec<u8>,
    /// Bytes that can begin a match.
    pub first: [bool; 256],
    pub anchor_start: bool,
    pub anchor_end: bool,
}

enum Node {
    Empty,
    Class(Vec<(u8, u8)>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
}

pub fn compile(pattern: &str) -> Result<Dfa, String> {
    let mut bytes = pattern.as_bytes();
    let anchor_start = bytes.first() == Some(&b'^');
    if anchor_start {
        bytes = &bytes[1..];
    }
    let anchor_end = bytes.last() == Some(&b'$') && !ends_with_escape(&bytes[..bytes.len() - 1]);
    if anchor_end {
        bytes = &bytes[..bytes.len() - 1];
    }

    let mut p = Parser { bytes, pos: 0 };
    let node = p.alt()?;
    if p.pos < bytes.len() {
        return Err(format!("unexpected `{}`", bytes[p.pos] as char));
    }

    let mut nfa = Nfa { states: Vec::new() };
    let accept = nfa.push(NState::Match);
    let start = nfa.compile(&node, accept);
    let dfa = nfa.to_dfa(start)?;
    Ok(Dfa { anchor_start, anchor_end, ..dfa })
}

/// Whether `bytes` ends in an odd number of backslashes, escaping what follows.
fn ends_with_escape(bytes: &[u8]) -> bool {
    bytes.iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Result<u8, String> {
        let b = self.peek().ok_or("pattern ends unexpectedly")?;
        self.pos += 1;
        Ok(b)
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.concat()?];
        while self.peek() == Some(b'|') {
            self.pos += 1;
            alts.push(self.concat()?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap() } else { Node::Alt(alts) })
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(b) = self.peek() {
            if b == b'|' || b == b')' {
                break;
            }
            items.push(self.repeat()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        })
    }

    fn repeat(&mut self) -> Result<Node, String> {
        let mut node = self.atom()?;
        loop {
            let (min, max) = match self.peek() {
                Some(b'*') => (0, None),
                Some(b'+') => (1, None),
                Some(b'?') => (0, Some(1)),
                Some(b'{') => {
                    self.pos += 1;
                    let min = self.number()?;
                    let max = if self.peek() == Some(b',') {
                        self.pos += 1;
                        if self.peek() == Some(b'}') { None } else { Some(self.number()?) }
                    } else {
                        Some(min)
                    };
                    if self.next()? != b'}' {
                        return Err("expected `}` in repetition".to_string());
                    }
                    if max.is_some_and(|max| max < min) || max.unwrap_or(min) > 1000 {
                        return Err("invalid repetition count".to_string());
                    }
                    node = Node::Repeat(Box::new(node), min, max);
                    continue;
                }
                _ => return Ok(node),
            };
            self.pos += 1;
            if self.peek() == Some(b'?') {
                return Err("lazy quantifiers are not supported (matching is leftmost-longest)".to_string());
            }
            node = Node::Repeat(Box::new(node), min, max);
        }
    }

    fn number(&mut self) -> Result<u32, String> {
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .unwrap()
            .parse()
            .map_err(|_| "expected a number in repetition".to_string())
    }

    fn atom(&mut self) -> Result<Node, String> {
        match self.next()? {
            b'(' => {
                if self.bytes[self.pos..].starts_with(b"?:") {
                    self.pos += 2;
                } else if self.peek() == Some(b'?') {
                    return Err("only `(?:...)` group flags are supported".to_string());
                }
                let node = self.alt()?;
                if self.peek() != Some(b')') {
                    return Err("unclosed `(`".to_string());
                }
                self.pos += 1;
                Ok(node)
            }
            b')' => Err("unmatched `)`".to_string()),
            b'[' => self.class(),
            b'.' => Ok(Node::Class(negate(&[(b'\n', b'\n')]))),
            b'^' | b'$' => Err("`^` and `$` are only supported at the start and end of a pattern".to_string()),
            b'*' | b'+' | b'?' => Err("quantifier with nothing to repeat".to_string()),
            b'\\' => Ok(Node::Class(self.escape()?)),
            b => Ok(Node::Class(vec![(b, b)])),
        }
    }

    /// The byte ranges an escape stands for, after the `\`.
    fn escape(&mut self) -> Result<Vec<(u8, u8)>, String> {
        let digit = vec![(b'0', b'9')];
        let word = vec![(b'0', b'9'), (b'A', b'Z'), (b'_', b'_'), (b'a', b'z')];
        let space = vec![(b'\t', b'\r'), (b' ', b' ')];
        Ok(match self.next()? {
            b'd' => digit,
            b'D' => negate(&digit),
            b'w' => word,
            b'W' => negate(&word),
            b's' => space,
            b'S' => negate(&space),
            b'n' => vec![(b'\n', b'\n')],
            b't' => vec![(b'\t', b'\t')],
            b'r' => vec![(b'\r', b'\r')],
            b'x' => {
                let hex = self.bytes.get(self.pos..self.pos + 2).ok_or("expected two hex digits after `\\x`")?;
                self.pos += 2;
                let b = std::str::from_utf8(hex)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or("expected two hex digits after `\\x`")?;
                vec![(b, b)]
            }
            b if b.is_ascii_alphanumeric() => return Err(format!("unknown escape `\\{}`", b as char)),
            b => vec![(b, b)],
        })
    }

    fn class(&mut self) -> Result<Node, String> {
        let negated = self.peek() == Some(b'^');
        if negated {
            self.pos += 1;
        }
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let b = self.next().map_err(|_| "unclosed `[`".to_string())?;
            if b == b']' && !first {
                break;
            }
            first = false;
            if b == b'\\' {
                ranges.extend(self.escape()?);
                continue;
            }
            if self.peek() == Some(b'-') && self.bytes.get(self.pos + 1).is_some_and(|&c| c != b']') {
                self.pos += 1;
                let hi = match self.next()? {
                    b'\\' => match self.escape()?.as_slice() {
                        [(lo, hi)] if lo == hi => *lo,
                        _ => return Err("invalid range end in `[...]`".to_string()),
                    },
                    hi => hi,
                };
                if hi < b {
                    return Err(format!("invalid range `{}-{}`", b as char, hi as char));
                }
                ranges.push((b, hi));
            } else {
                ranges.push((b, b));
            }
        }
        Ok(Node::Class(if negated { negate(&ranges) } else { ranges }))
    }
}

/// The bytes not covered by `ranges`.
fn negate(ranges: &[(u8, u8)]) -> Vec<(u8, u8)> {
    let mut covered = [false; 256];
    for &(lo, hi) in ranges {
        for b in lo..=hi {
            covered[b as usize] = true;
        }
    }
    let mut out = Vec::new();
    let mut b = 0;
    while b < 256 {
        if covered[b] {
            b += 1;
            continue;
        }
        let lo = b;
        while b < 256 && !covered[b] {
            b += 1;
        }
        out.push((lo as u8, (b - 1) as u8));
    }
    out
}

enum NState {
    Class(Vec<(u8, u8)>, usize),
    Split(Vec<usize>),
    Match,
}

struct Nfa {
    states: Vec<NState>,
}

impl Nfa {
    fn push(&mut self, s: NState) -> usize {
        self.states.push(s);
        self.states.len() - 1
    }

    /// Compiles `node` so that completing it continues at `cont`; returns its start.
    fn compile(&mut self, node: &Node, cont: usize) -> usize {
        match node {
            Node::Empty => cont,
            Node::Class(ranges) => self.push(NState::Class(ranges.clone(), cont)),
            Node::Concat(items) => items.iter().rev().fold(cont, |next, item| self.compile(item, next)),
            Node::Alt(alts) => {
                let starts = alts.iter().map(|a| self.compile(a, cont)).collect();
                self.push(NState::Split(starts))
            }
            Node::Repeat(inner, min, max) => {
                let mut tail = match max {
                    None => {
                        let lp = self.push(NState::Split(Vec::new()));
                        let body = self.compile(inner, lp);
                        self.states[lp] = NState::Split(vec![body, cont]);
                        lp
                    }
                    Some(max) => {
                        let mut tail = cont;
                        for _ in *min..*max {
                            let body = self.compile(inner, tail);
                            tail = self.push(NState::Split(vec![body, cont]));
                        }
                        tail
                    }
                };
                for _ in 0..*min {
                    tail = self.compile(inner, tail);
                }
                tail
            }
        }
    }

    /// The non-split states reachable from `from` without consuming input.
    fn closure(&self, from: impl IntoIterator<Item = usize>) -> Vec<usize> {
        let mut seen = vec![false; self.states.len()];
        let mut stack: Vec<usize> = from.into_iter().collect();
        let mut out = Vec::new();
        while let Some(s) = stack.pop() {
            if std::mem::replace(&mut seen[s], true) {
                continue;
            }
            match &self.states[s] {
                NState::Split(next) => stack.extend(next),
                _ => out.push(s),
            }
        }
        out.sort_unstable();
        out
    }

    fn to_dfa(&self, start: usize) -> Result<Dfa, String> {
        let mut ids: HashMap<Vec<usize>, u16> = HashMap::new();
        let mut sets: Vec<Vec<usize>> = vec![Vec::new()];
        ids.insert(Vec::new(), 0);
        let start_set = self.closure([start]);
        ids.insert(start_set.clone(), 1);
        sets.push(start_set);

        let mut next = vec![0u16; 2 * 256];
        let mut i = 1;
        while i < sets.len() {
            for b in 0..=255u8 {
                let targets = sets[i].iter().filter_map(|&s| match &self.states[s] {
                    NState::Class(ranges, to) if ranges.iter().any(|&(lo, hi)| lo <= b && b <= hi) => Some(*to),
                    _ => None,
                });
                let set = self.closure(targets);
                let id = match ids.get(&set) {
                    Some(&id) => id,
                    None => {
                        if sets.len() >= MAX_STATES {
                            return Err(format!("pattern is too complex (over {MAX_STATES} DFA states)"));
                        }
                        let id = sets.len() as u16;
                        ids.insert(set.clone(), id);
                        sets.push(set);
                        next.extend([0u16; 256]);
                        id
                    }
                };
                next[i * 256 + b as usize] = id;
            }
            i += 1;
        }

        let accept: Vec<bool> = sets
            .iter()
            .map(|set| set.iter().any(|&s| matches!(self.states[s], NState::Match)))
            .collect();
        let mut first = [false; 256];
        for b in 0..256 {
            first[b] = next[256 + b] != 0;
        }
        // Follow the start state while exactly one byte leads anywhere.
        let mut prefix = Vec::new();
        let mut state = 1usize;
        while !accept[state] && prefix.len() < 64 {
            let mut live = (0..256).filter(|&b| next[state * 256 + b] != 0);
            match (live.next(), live.next()) {
                (Some(b), None) => {
                    prefix.push(b as u8);
                    state = next[state * 256 + b] as usize;
                }
                _ => break,
            }
        }
        Ok(Dfa { next, accept, prefix, first, anchor_start: false, anchor_end: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leftmost-longest match by brute force over the DFA, as the runtime does.
    fn find(pattern: &str, s: &str) -> Option<(usize, usize)> {
        let dfa = compile(pattern).unwrap();
        let s = s.as_bytes();
        for start in 0..=s.len() {
            if dfa.anchor_start && start > 0 {
                break;
            }
            let mut state = 1usize;
            let mut end = if dfa.accept[1] && (!dfa.anchor_end || start == s.len()) { Some(start) } else { None };
            for (j, &b) in s.iter().enumerate().skip(start) {
                state = dfa.next[state * 256 + b as usize] as usize;
                if state == 0 {
                    break;
                }
                if dfa.accept[state] && (!dfa.anchor_end || j + 1 == s.len()) {
                    end = Some(j + 1);
                }
            }
            if let Some(end) = end {
                return Some((start, end));
            }
        }
        None
    }

    #[test]
    fn matching() {
        let cases: &[(&str, &str, Option<(usize, usize)>)] = &[
            ("abc", "xxabcxx", Some((2, 5))),
            ("a+", "baaab", Some((1, 4))),
            ("a*", "bbb", Some((0, 0))),
            ("colou?r", "my color", Some((3, 8))),
            ("[0-9]+", "ms=1234 ok", Some((3, 7))),
            ("\\d{2,3}", "a1b22c4444", Some((3, 5))),
            ("x{3}", "xxxxx", Some((0, 3))),
            ("(ab|cd)+e", "zzcdabe", Some((2, 7))),
            ("[^a-z ]+", "abc DEF1 g", Some((4, 8))),
            ("^ab", "xab", None),
            ("^ab", "abx", Some((0, 2))),
            ("b$", "abab", Some((3, 4))),
            ("a.c", "a\nc abc", Some((4, 7))),
            ("\\.", "a.b", Some((1, 2))),
            ("[a\\-z]", "b-", Some((1, 2))),
            ("[]x]", "a]", Some((1, 2))),
            ("\\w+@\\w+\\.com", "mail: bob@site.com!", Some((6, 18))),
            ("(?:ERROR|WARN) +x", "INFO x WARN  x", Some((7, 14))),
            ("a|ab", "ab", Some((0, 2))), // leftmost-longest
        ];
        for &(pattern, s, want) in cases {
            assert_eq!(find(pattern, s), want, "`{pattern}` on {s:?}");
        }
    }

    #[test]
    fn literal_prefix() {
        assert_eq!(compile("status=5[0-9]{2}").unwrap().prefix, b"status=5");
        assert_eq!(compile("ms=\\d+").unwrap().prefix, b"ms=");
        assert!(compile("(ERROR|WARN)").unwrap().prefix.is_empty());
        let first = compile("(ERROR|WARN)").unwrap().first;
        assert!(first[b'E' as usize] && first[b'W' as usize] && !first[b'I' as usize]);
    }

    #[test]
    fn errors() {
        for (pattern, want) in [
            ("a(b", "unclosed `(`"),
            ("[ab", "unclosed `[`"),
            ("a)", "unexpected `)`"),
            ("*a", "nothing to repeat"),
            ("a*?", "lazy quantifiers"),
            ("a^b", "only supported at the start and end"),
            ("\\q", "unknown escape"),
            ("(a|b|c|d|e|f|g|h){0,1000}x{1000}", "too complex"),
        ] {
            let err = compile(pattern).err().unwrap_or_else(|| panic!("`{pattern}` compiled"));
            assert!(err.contains(want), "`{pattern}`: {err}");
        }
    }
}
