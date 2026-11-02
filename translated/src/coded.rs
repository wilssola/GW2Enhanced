//! The game's "coded text" (`TextParser.cpp`): UTF-16 words that reference
//! strings by ID instead of holding the text. Speech bubbles and chat lines
//! store this, and the text system decodes it (through `F_materialize`).
//!
//! - A number is written in base `0x7F00`: every word but the last has the
//!   high bit set and is `digit + 0x8100`; the last is `digit + 0x100`
//!   (decoder `0x10e53f0`, encoder `0x10e21c0`).
//! - A text starts with a string ID, followed by parameters: `0x101..=0x106`
//!   numeric, `0x107..=0x10C` nested text, `0x10D..=0x112` segment (a list of
//!   string IDs ended by `0x0001`).
//! - `0x0000` ends the text; `0x0001`..`0x0003` are terminators/separators.
//! - Literal text (a player's chat message, a color, a chat link) is a
//!   parameter `0x107..=0x10C` followed by its characters (`>= 0x0A`) and
//!   `0x0001` (`0x10e6ef0`).

/// Longest coded text copied (words).
pub const MAX_WORDS: usize = 512;

const HIGH: u16 = 0x8000;
const BASE: u32 = 0x7F00;
const SEGMENT: std::ops::RangeInclusive<u16> = 0x10D..=0x112;
const PARAMETER: std::ops::RangeInclusive<u16> = 0x101..=0x112;

/// Reads one number at `words[i..]`; returns it and the index after it.
fn number(words: &[u16], mut i: usize) -> Option<(u32, usize)> {
    let mut acc: u32 = 0;
    loop {
        let w = *words.get(i)?;
        i += 1;
        let value = u32::from(w & !HIGH);
        if value < 0x100 {
            return None;
        }
        if w & HIGH == 0 {
            return Some((acc.checked_add(value - 0x100)?, i));
        }
        acc = acc.checked_add(value - 0x100)?.checked_mul(BASE)?;
    }
}

/// String IDs referenced by a coded text: the leading one and every ID
/// inside segment parameters (the dialogue line of a speech bubble).
pub fn string_ids(words: &[u16]) -> Vec<u32> {
    let mut out = Vec::new();
    let Some((first, mut i)) = number(words, 0) else {
        return out;
    };
    out.push(first);
    while let Some(&w) = words.get(i) {
        if w == 0 {
            break;
        }
        if SEGMENT.contains(&w) {
            i += 1;
            // The list ends at `0x0001` or at the next parameter, whose marker
            // (`0x101..=0x112`) would otherwise read as a small number.
            while let Some((id, next)) = number(words, i).filter(|_| !PARAMETER.contains(&words[i])) {
                out.push(id);
                i = next;
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Copies a coded text (up to its `0x0000`, inclusive) from game memory.
///
/// # Safety
/// `p` must point to readable memory holding a `0x0000`-terminated text, or
/// at least [`MAX_WORDS`] readable words.
pub unsafe fn copy(p: *const u16, max: usize) -> Option<Vec<u16>> {
    let max = max.min(MAX_WORDS);
    let mut out = Vec::new();
    for i in 0..max {
        let w = unsafe { p.add(i).read() };
        out.push(w);
        if w == 0 {
            return Some(out);
        }
    }
    None
}

/// Committed, readable memory from `p` to the end of its region, in bytes.
pub fn readable(p: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    if unsafe { VirtualQuery(p as *const _, &mut info, size) } != size {
        return 0;
    }
    if info.State != MEM_COMMIT || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0 || info.Protect == 0 {
        return 0;
    }
    (info.BaseAddress as usize + info.RegionSize).saturating_sub(p)
}

/// Hex dump for the log.
pub fn hex(words: &[u16]) -> String {
    words.iter().map(|w| format!("{w:04x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_numbers_like_the_game() {
        assert_eq!(number(&[0x0105], 0), Some((5, 1)));
        assert_eq!(number(&[0x8141, 0x396d], 0), Some((65 * 0x7F00 + 0x386d, 2)));
        assert_eq!(number(&[0x0059], 0), None);
    }

    #[test]
    fn finds_the_dialogue_line_of_a_speech_bubble() {
        // Seen in game: format string, segment with the line, terminator.
        let bubble = [0x8141, 0x396d, 0x010d, 0x8119, 0x077d, 0x0001, 0x0000];
        assert_eq!(string_ids(&bubble), [2_127_725, 25 * 0x7F00 + 0x67d]);
        assert_eq!(25 * 0x7F00 + 0x67d, 814_461);
    }

    #[test]
    fn parameter_markers_are_not_string_ids() {
        // Seen in game: "%str1%?" with a literal parameter (0x0107) after the segment.
        let w = [
            0x8101, 0x448c, 0x010d, 0x8101, 0x447b, 0x0107, 0x0066, 0x0001, 0x0001, 0x0000,
        ];
        assert_eq!(string_ids(&w), [49_804, 49_787]);
        // Two segments in a row (Glint's lines): 0x010d is not the ID 13.
        let w = [
            0x8101, 0x447b, 0x010d, 0x8101, 0x447b, 0x010d, 0x8118, 0x0393, 0x0001, 0x0000,
        ];
        assert_eq!(string_ids(&w), [49_787, 49_787, 24 * 0x7F00 + 0x293]);
    }
}
