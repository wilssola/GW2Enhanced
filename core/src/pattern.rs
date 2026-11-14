//! Byte patterns with wildcards (`48 8B ?? C3`).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(Vec<Option<u8>>);

impl Pattern {
    /// Converts `"48 8B ?? C3"` into a pattern. `?` and `??` are wildcards. The first byte must be concrete
    /// (it drives the search).
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

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of bytes that are not wildcards: a rough measure of how specific the pattern is.
    pub fn fixed_bytes(&self) -> usize {
        self.0.iter().flatten().count()
    }

    fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        hay.len() - at >= self.0.len() && self.0.iter().zip(&hay[at..]).all(|(p, b)| p.is_none_or(|p| p == *b))
    }

    /// All positions where the pattern matches.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_finds() {
        let p = Pattern::parse("48 ?? C3").unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p.fixed_bytes(), 2);
        assert_eq!(p.find_all(&[0, 0x48, 1, 0xC3, 0x48, 2, 0xC3]), vec![1, 4]);
        assert_eq!(p.find_unique(&[0x48, 9, 0xC3]), Some(0));
        assert_eq!(p.find_unique(&[0x48, 9, 0xC3, 0x48, 9, 0xC3]), None);
    }

    #[test]
    fn rejects_bad_patterns() {
        assert!(Pattern::parse("").is_none());
        assert!(Pattern::parse("?? 48").is_none(), "first byte must be concrete");
        assert!(Pattern::parse("48 ZZ").is_none());
    }
}
