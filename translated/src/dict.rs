//! Dictionary `English → translation`, loaded from the JSON Lines files
//! (see [`crate::records`]).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use crate::records::{self, Translation};

/// Marker the game itself uses for removed or empty strings.
pub const NULL_MARKER: &str = "[null]";

/// A translation that would show `[null]` where the original text does not:
/// better to keep the game's original.
pub fn hides_original(original: &str, translation: &str) -> bool {
    translation.contains(NULL_MARKER) && !original.contains(NULL_MARKER)
}

/// Names of the `[...]` text fixups in `s` (`[s]`, `[pl:"..."]` → `s`, `pl`).
fn fixups(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        rest = &rest[i + 1..];
        let end = rest.find([']', ':', '[']).unwrap_or(rest.len());
        if rest[end..].starts_with([']', ':']) && end > 0 && rest[..end].chars().all(|c| c.is_ascii_alphabetic()) {
            out.push(&rest[..end]);
        }
    }
    out
}

/// A translation that uses a `[...]` fixup the original does not have (e.g. a
/// made-up `[es]` plural): the game rejects it and logs an error each time.
pub fn adds_fixups(original: &str, translation: &str) -> bool {
    let known = fixups(original);
    // Plural endings (`[s]`, `[pl:"..."]`) are valid in any text.
    fixups(translation)
        .iter()
        .any(|f| *f != "s" && *f != "pl" && !known.contains(f))
}

/// A translation that must not be used: see [`hides_original`] and [`adds_fixups`].
pub fn unusable(original: &str, translation: &str) -> bool {
    hides_original(original, translation) || adds_fixups(original, translation)
}

/// Markup the game interprets and that a translation must keep identical:
/// `<br>`, `<c=` openings and `</c>` closings, and the `%name%` variables.
pub fn markup_signature(s: &str) -> (usize, usize, usize, Vec<String>) {
    let mut vars: Vec<String> = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        let tail = &rest[i + 1..];
        match tail.find('%') {
            Some(j) if j > 0 && tail[..j].chars().all(|c| c.is_ascii_alphanumeric()) => {
                vars.push(tail[..j].to_owned());
                rest = &tail[j + 1..];
            }
            _ => rest = tail,
        }
    }
    vars.sort();
    (
        s.matches("<br>").count(),
        s.matches("<c=").count(),
        s.matches("</c>").count(),
        vars,
    )
}

/// Collapses repeated spaces, trims the ends and replaces typographic quotes with
/// straight ones. `None` when the text is already normalized.
fn normalize(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.trim().chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{00A0}' => ' ',
            c => c,
        };
        if c == ' ' && space {
            continue;
        }
        space = c == ' ';
        out.push(c);
    }
    (out != s).then_some(out)
}

#[derive(Debug, Default)]
pub struct Dict {
    map: HashMap<String, String>,
    /// Normalized form of the keys that were not normalized.
    loose: HashMap<String, String>,
    /// Translation by string ID, for the game in any language.
    by_id: HashMap<u32, String>,
}

impl Dict {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Exact key; otherwise the same key after normalizing spaces and quotes.
    pub fn get(&self, english: &str) -> Option<&str> {
        if let Some(pt) = self.map.get(english) {
            return Some(pt);
        }
        match normalize(english) {
            Some(n) => self.map.get(&n).or_else(|| self.loose.get(&n)),
            None => self.loose.get(english),
        }
        .map(String::as_str)
    }

    /// Every English text and its translation.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.map.iter().map(|(en, tr)| (en.as_str(), tr.as_str()))
    }

    pub fn into_map(self) -> HashMap<String, String> {
        self.map
    }

    pub fn get_id(&self, id: u32) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    pub fn id_len(&self) -> usize {
        self.by_id.len()
    }

    /// Adds translations. Empty ones and ones that would show `[null]` are
    /// ignored. With `overwrite = false`, an existing entry is kept (the main
    /// dictionary takes priority over the live one). Returns how many were added.
    pub fn add(&mut self, records: impl IntoIterator<Item = Translation>, overwrite: bool) -> usize {
        let mut added = 0;
        for Translation { en, tr, ids, .. } in records {
            if en.is_empty() || tr.is_empty() || en == NULL_MARKER || unusable(&en, &tr) {
                continue;
            }
            for id in ids {
                if overwrite || !self.by_id.contains_key(&id) {
                    self.by_id.insert(id, tr.clone());
                }
            }
            if overwrite || !self.map.contains_key(&en) {
                if let Some(n) = normalize(&en) {
                    self.loose.entry(n).or_insert_with(|| tr.clone());
                }
                self.map.insert(en, tr);
                added += 1;
            }
        }
        added
    }

    /// Reads a [`Translation`] file (`dictionary.jsonl`, `local.jsonl`); a missing
    /// file counts as empty.
    pub fn load_file(&mut self, path: &Path, overwrite: bool) -> io::Result<usize> {
        Ok(self.add(records::read::<Translation>(path)?.records, overwrite))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(en: &str, tr: &str) -> Translation {
        Translation {
            en: en.into(),
            tr: tr.into(),
            ..Default::default()
        }
    }

    #[test]
    fn loads_multiline_and_skips_empty() {
        let mut d = Dict::default();
        let r = [
            tr("Ingredient", "Ingrediente"),
            tr("Empty", ""),
            tr("Multi\nline <br>x", "Várias\nlinhas <br>x"),
        ];
        assert_eq!(d.add(r, true), 2);
        assert_eq!(d.get("Ingredient"), Some("Ingrediente"));
        assert_eq!(d.get("Multi\nline <br>x"), Some("Várias\nlinhas <br>x"));
        assert_eq!(d.get("Empty"), None);
    }

    #[test]
    fn main_dict_wins_over_live() {
        let mut d = Dict::default();
        d.add([tr("Ingredient", "Ingrediente")], true);
        assert_eq!(
            d.add(
                [tr("Ingredient", "Ingrediente ruim"), tr("Novus Laser", "Laser Novus")],
                false
            ),
            1
        );
        assert_eq!(d.get("Ingredient"), Some("Ingrediente"));
        assert_eq!(d.get("Novus Laser"), Some("Laser Novus"));
    }

    #[test]
    fn matches_after_normalizing_spaces_and_quotes() {
        let mut d = Dict::default();
        d.add([tr("It's  done", "Feito")], true);
        assert_eq!(d.get("It\u{2019}s done"), Some("Feito"));
        assert_eq!(d.get(" It's done "), Some("Feito"));
        assert_eq!(d.get("It's done"), Some("Feito"));
        assert_eq!(d.get("Its done"), None);
    }

    #[test]
    fn never_loads_null_marker_over_real_text() {
        let mut d = Dict::default();
        assert_eq!(
            d.add(
                [
                    tr("[null]", "[null]"),
                    tr("Sword", "[null]"),
                    tr(", [null]", ", [null]")
                ],
                true
            ),
            1
        );
        assert_eq!(d.get("[null]"), None);
        assert_eq!(d.get("Sword"), None);
        assert_eq!(d.get(", [null]"), Some(", [null]"));
        let with_ids = |en: &str, t: &str, id| Translation {
            ids: vec![id],
            ..tr(en, t)
        };
        assert_eq!(
            d.add([with_ids("Sword", "[null]", 1), with_ids("Shield", "Escudo", 2)], true),
            1
        );
        assert_eq!(d.get_id(1), None);
        assert_eq!(d.get_id(2), Some("Escudo"));
    }

    #[test]
    fn rejects_made_up_fixups() {
        assert!(adds_fixups(" [null]Raven[s]", " [null]Corvo[es]"));
        assert!(!adds_fixups(" [null]Raven[s]", " [null]Corvo[s]"));
        assert!(!adds_fixups("Hit %num1% foe[s].", "Acerte %num1% inimigo[s]."));
        assert!(!adds_fixups("[pl:\"Wolf\"]", "[pl:\"Lobo\"]"));
        assert!(!adds_fixups("Press [Enter]", "Pressione [Enter]"));
        assert!(!adds_fixups("%num1% Owned", "%num1% Possuído[s]"));
        assert!(adds_fixups("%str1% picks up [the] %str2%.", "%str1% pega [o] %str2%."));
        assert!(!adds_fixups("Bag[s] of Goods", "Saco[pl:\"Sacos\"] de Mercadorias"));
    }

    #[test]
    fn markup_signature_catches_lost_tags() {
        let en = "<c=@flavor>Hit %num1% foe[s].</c><br>";
        assert_eq!(
            markup_signature(en),
            markup_signature("<c=@flavor>Acerte %num1% inimigo[s].</c><br>")
        );
        assert_ne!(
            markup_signature(en),
            markup_signature("<c=@flavor>Acerte inimigos.</c><br>")
        );
        assert_ne!(markup_signature(en), markup_signature("Acerte %num1% inimigo[s].<br>"));
    }

    #[test]
    fn loads_jsonl_with_ids_and_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dictionary.jsonl");
        std::fs::write(
            &p,
            "{\"en\":\"The Cost of Victory\",\"tr\":\"O Custo da Vitória\",\"src\":\"nemotron\",\"ids\":[1601,1602]}\n",
        )
        .unwrap();
        let mut d = Dict::default();
        assert_eq!(d.load_file(&p, true).unwrap(), 1);
        assert_eq!(d.get_id(1602), Some("O Custo da Vitória"));
        assert_eq!(d.load_file(Path::new("does-not-exist.jsonl"), true).unwrap(), 0);
    }

    #[test]
    fn local_ids_do_not_override_the_dictionary() {
        let mut d = Dict::default();
        d.add(
            [Translation {
                ids: vec![7],
                ..tr("Sword", "Espada")
            }],
            true,
        );
        d.add(
            [Translation {
                ids: vec![7, 8],
                ..tr("Sword ", "Espada ruim")
            }],
            false,
        );
        assert_eq!(d.get_id(7), Some("Espada"));
        assert_eq!(d.get_id(8), Some("Espada ruim"));
    }

    /// Checks the repository's real dictionary, when present.
    #[test]
    fn shipped_dictionary_is_valid() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/languages/pt-BR/dictionary.jsonl");
        if !p.exists() {
            return;
        }
        let mut d = Dict::default();
        let n = d.load_file(&p, true).unwrap();
        assert!(n > 200_000, "{n}");
        for (en, pt) in &d.map {
            assert_eq!(en.matches("<br>").count(), pt.matches("<br>").count(), "{en:?}");
        }
    }
}
