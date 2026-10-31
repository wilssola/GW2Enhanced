//! Byte signature search (`48 8B ?? …`) in the game's code section.
//!
//! Origin and meaning of each signature: `docs/reverse-engineering.md`.

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
        hay.len() - at >= self.0.len() && self.0.iter().zip(&hay[at..]).all(|(p, b)| p.is_none_or(|p| p == *b))
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

/// A game site that GW2 Translated needs to locate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    pub name: &'static str,
    pub sig: &'static str,
}

/// Where the game decodes each string (`TextDecode.cpp`): this is where the DLL translates.
pub const MATERIALIZE: Site = Site {
    name: "F_materialize",
    sig: "48 89 54 24 10 53 57 41 54 41 56 41 57 48 83 EC 50 49 8B F9 4C 8B E2 4D 85 C9",
};
/// Registration of each `strs` entry in the text cache (seed_decode).
pub const REG_INSERT: Site = Site {
    name: "reg-insert",
    sig: "48 89 6C 24 18 56 57 41 56 48 83 EC 30 41 8B C0 48 8B F1 23 41 78 4D 8B F1 48 83 C1 74 89 44 24 58 41 8B F8 8B EA 3B 01 0F 83 57 01 00 00 8B D0 48 C1 E2 05 48 03 56 68 48 89 5C 24 50 48 8B 42",
};
/// Server message carrying the `keySeed`s (seed_decode).
pub const BULK_INSERT: Site = Site {
    name: "bulk_insert",
    sig: "40 53 55 57 48 83 EC 20 48 8B EA E8 ?? ?? ?? ?? 0F B6 55 02 48 8B 48 50 03 91 04 02 00 00 48 8D 99 00 02 00 00 83 FA 08 77 07 BA 08 00 00 00 EB 18 8D 42 FF 85 C2 74 11 8B CA E8 ?? ?? ?? ?? BA",
};
/// Per-frame tick of the text cache (refresh).
pub const CACHETICK: Site = Site {
    name: "cachetick",
    sig: "40 57 48 83 EC 20 83 79 08 00 48 8B F9 0F 84 ?? ?? ?? ?? 83 79 0C 00 0F 85 ?? ?? ?? ?? 01 91 74 01 00 00 48 8B 41 48",
};
/// Rebuild of the UI with the current language (refresh).
pub const FRAME_REBUILD: Site = Site {
    name: "frame_rebuild",
    sig: "48 89 5C 24 08 57 48 83 EC 20 F6 05 ?? ?? ?? ?? 01 8B DA 8B F9 75 19 41 B8 01 0A 00 00 48 8D 15 ?? ?? ?? ?? 48 8D 0D ?? ?? ?? ?? E8 ?? ?? ?? ??",
};

/// Message handler of the chat control (creates and destroys the tabs).
pub const CHAT_DISPATCH: Site = Site {
    name: "chat dispatch",
    sig: "40 53 55 56 57 41 54 41 56 48 81 EC 88 00 00 00 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 44 24 70 8B 41 04 4D 8B E0 FF C8 4C 8B F2 48 8B F1 83 F8 4A 0F 87",
};
/// Clears and rebuilds all lines of a chat tab.
pub const CHAT_REBUILD: Site = Site {
    name: "chat rebuild",
    sig: "40 55 57 48 83 EC 68 48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 44 24 58 48 8B F9 33 D2 8B 49 18 E8 ?? ?? ?? ?? 8B C8 8B E8 E8 ?? ?? ?? ?? 83 7F 68 00",
};

/// Fills a speech bubble (`AsChatBubble.cpp`, slot 0 of its vtable).
pub const BUBBLE_SET_CONTENT: Site = Site {
    name: "bubble set_content",
    sig: "48 89 5C 24 08 48 89 74 24 10 57 48 83 EC 20 F3 0F 10 4A 24",
};

/// Message handler of the speech bubble control.
pub const BUBBLE_HANDLER: Site = Site {
    name: "bubble handler",
    sig: "48 89 5C 24 18 55 56 57 41 54 41 56 48 83 EC 70 8B 41 04 4D 8B E0 FF C8 0F 29 74 24 60",
};

/// Sets a control's text (control message 0x5d).
pub const CTL_SET_TEXT: Site = Site {
    name: "control set_text",
    sig: "48 89 5C 24 08 57 48 83 EC 20 48 8B DA 8B F9 48 85 D2 75 19 41 B8 5B 0B 00 00",
};

/// Sends a message to a control (`fn(control, message, a, b)`).
pub const CTL_SEND: Site = Site {
    name: "control send",
    sig: "48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 48 83 EC 20 49 8B F1 49 8B E8 8B DA 8B F9 85 C9 75 19 41 B8 1A 0F 00 00",
};

/// Child control by index.
pub const CTL_CHILD: Site = Site {
    name: "control child",
    sig: "40 53 48 83 EC 20 8B DA 85 C9 74 07 E8 ?? ?? ?? ??",
};

/// All sites the DLL uses.
pub const ALL: &[Site] = &[
    MATERIALIZE,
    REG_INSERT,
    BULK_INSERT,
    CACHETICK,
    FRAME_REBUILD,
    CHAT_DISPATCH,
    CHAT_REBUILD,
    BUBBLE_SET_CONTENT,
    BUBBLE_HANDLER,
    CTL_SET_TEXT,
    CTL_SEND,
    CTL_CHILD,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_find() {
        let p = Pattern::parse("48 ?? C3").unwrap();
        assert_eq!(p.find_all(&[0x48, 0x01, 0xC3, 0x48, 0x48, 0xFF, 0xC3]), vec![0, 4]);
        assert_eq!(p.find_unique(&[0x48, 0x01, 0xC3]), Some(0));
        assert_eq!(p.find_unique(&[0x48, 0x01]), None);
        assert!(Pattern::parse("?? 48").is_none());
        assert!(Pattern::parse("4G").is_none());
    }

    #[test]
    fn all_signatures_parse() {
        for s in ALL {
            assert!(Pattern::parse(s.sig).is_some(), "{}", s.name);
        }
    }

    /// Runs against the copy of the exe in `re/` (outside git), when it exists.
    #[test]
    fn all_signatures_unique_in_game_exe() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../re/Gw2-64.exe");
        let Ok(exe) = std::fs::read(&p) else { return };
        for s in ALL {
            let n = Pattern::parse(s.sig).unwrap().find_all(&exe).len();
            assert_eq!(n, 1, "{} matched {n} times", s.name);
        }
    }
}
