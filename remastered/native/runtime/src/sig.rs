//! Byte signature search (`48 8B ?? …`) in the game's code section.
//!
//! Origin and meaning of each signature: `docs/texture-reverse-engineering.md`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(Vec<Option<u8>>);

impl Pattern {
    /// Converts `"48 8B ?? C3"` into a pattern. `?` and `??` are wildcards.
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = s
            .split_whitespace()
            .map(|t| match t {
                "?" | "??" => Some(None),
                _ => u8::from_str_radix(t, 16).ok().map(Some),
            })
            .collect::<Option<Vec<_>>>()?;
        (!bytes.is_empty() && bytes[0].is_some()).then_some(Self(bytes))
    }

    fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        hay.len() - at >= self.0.len()
            && self.0.iter().zip(&hay[at..]).all(|(p, b)| p.is_none_or(|p| p == *b))
    }

    /// All positions where the pattern matches (the first byte is never a wildcard).
    pub fn find_all(&self, hay: &[u8]) -> Vec<usize> {
        let first = self.0[0].unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(off) = hay.get(i..).and_then(|h| h.iter().position(|&b| b == first)) {
            let at = i + off;
            if self.matches_at(hay, at) {
                out.push(at);
            }
            i = at + 1;
        }
        out
    }

    /// Unique position of the pattern; `None` if not found or ambiguous.
    pub fn find_unique(&self, hay: &[u8]) -> Option<usize> {
        match self.find_all(hay).as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }
}

/// A game site that GW2 Remastered needs to locate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    pub name: &'static str,
    pub sig: &'static str,
}

