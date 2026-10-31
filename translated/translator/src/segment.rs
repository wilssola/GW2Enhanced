//! Separates the game's markup from the text that goes to the model.
//!
//! OPUS-MT models mangle markers inserted into the sentence, so no
//! masking: the text is cut into markup pieces (kept as they are)
//! and text pieces (translated), and reassembled in the same order. Markup is never
//! lost; the price is losing context between pieces.
//!
//! Markup: `<br>`, `<c=@…>`, `</c>` and other `<…>` tags; variables
//! `%str1%`/`%num1%`; `%%` (the game's literal `%`); codes `[s]`,
//! `[pl:"…"]`, `[f:"…"]`, `[lbracket]`…; line breaks.
//!
//! The text between markup is further cut into sentences (and at a leading `(emote)`):
//! the small OPUS-MT models translate one sentence at a time and drop the rest.
//! Bigger models can take the whole stretch (`split_sentences: false`).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece<'a> {
    Markup(&'a str),
    Text(&'a str),
}

/// Length of the markup starting at `s`, if any.
fn markup_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    match b.first()? {
        b'\n' | b'\r' => Some(1),
        b'<' => s
            .find('>')
            .map(|i| i + 1)
            .filter(|&n| n > 2 && !s[1..n - 1].contains('<')),
        b'%' if b.get(1) == Some(&b'%') => Some(2),
        b'%' => {
            let end = s[1..].find('%')? + 1;
            (end > 1 && s[1..end].bytes().all(|c| c.is_ascii_alphanumeric())).then_some(end + 1)
        }
        b'[' => {
            let name_end = s[1..].find(|c: char| !c.is_ascii_lowercase())? + 1;
            if name_end == 1 {
                return None;
            }
            match b.get(name_end)? {
                b']' => Some(name_end + 1),
                b':' if b.get(name_end + 1) == Some(&b'"') => {
                    let close = s[name_end + 2..].find("\"]")? + name_end + 2;
                    Some(close + 2)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Cuts a stretch of text into sentences without losing any character: the
/// space between sentences becomes a piece with no letters (not translated).
fn sentences(t: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = t.char_indices().collect();
    for (k, &(i, c)) in chars.iter().enumerate() {
        let ends = matches!(c, '.' | '!' | '?' | '…') || (c == ')' && t[start..i].trim_start().starts_with('('));
        let next_is_space = chars.get(k + 1).is_some_and(|&(_, n)| n == ' ');
        if ends && next_is_space {
            let end = i + c.len_utf8();
            let after_space = chars[k + 1..]
                .iter()
                .find(|&&(_, n)| n != ' ')
                .map_or(t.len(), |&(j, _)| j);
            out.push(&t[start..end]);
            out.push(&t[end..after_space]);
            start = after_space;
        }
    }
    if start < t.len() {
        out.push(&t[start..]);
    }
    out.retain(|p| !p.is_empty());
    out
}

#[cfg(test)]
pub fn split(s: &str) -> Vec<Piece<'_>> {
    split_with(s, true)
}

/// [`split`], optionally keeping each stretch of text between markup whole.
pub fn split_with(s: &str, split_sentences: bool) -> Vec<Piece<'_>> {
    let cut = |t| if split_sentences { sentences(t) } else { vec![t] };
    let mut out = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < s.len() {
        if let Some(n) = markup_len(&s[i..]) {
            if text_start < i {
                out.extend(cut(&s[text_start..i]).into_iter().map(Piece::Text));
            }
            out.push(Piece::Markup(&s[i..i + n]));
            i += n;
            text_start = i;
        } else {
            i += s[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if text_start < s.len() {
        out.extend(cut(&s[text_start..]).into_iter().map(Piece::Text));
    }
    out
}

/// Is it worth sending to the model? (has letters; not just punctuation/numbers)
pub fn translatable(text: &str) -> bool {
    text.chars().any(char::is_alphabetic)
}

/// Spaces and the punctuation that joins a piece to the markup next to it
/// (`, %str1%, are you ready?`): kept out of the model, which drops it.
fn is_joint(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '–' | '—')
}

/// Splits a text piece into (joint before, core, joint after).
fn joints(t: &str) -> (&str, &str, &str) {
    let core = t.trim_start_matches(is_joint);
    let lead = &t[..t.len() - core.len()];
    let trimmed = core.trim_end_matches(is_joint);
    (lead, trimmed, &core[trimmed.len()..])
}

/// The model's output for a core, without joints it may have added and with
/// a lowercase start when the English continued a sentence in lowercase.
fn fit(english: &str, lead: &str, translated: &str) -> String {
    let mut t = translated.trim_matches(is_joint);
    // The model ends fragments with a period ("All right" → "Tudo bem."):
    // keep the final punctuation only if the English had it.
    let ends = |s: &str| s.ends_with(['.', '!', '?', '…']);
    if !ends(english) {
        t = t.trim_end_matches(['.', '…']).trim_end_matches(is_joint);
    }
    let continues = lead.contains([',', ';', ':']) && english.chars().next().is_some_and(char::is_lowercase);
    let mut chars = t.chars();
    match chars.next() {
        Some(first) if continues && first.is_uppercase() => first.to_lowercase().chain(chars).collect(),
        _ => t.to_owned(),
    }
}

/// Translates `s` piece by piece with `tr`, preserving markup and the spaces
/// and joining punctuation around each text piece.
#[cfg(test)]
pub fn translate_with(s: &str, tr: impl FnMut(&[&str]) -> Vec<String>) -> String {
    translate_with_opts(s, true, tr)
}

/// [`translate_with`], with sentence splitting optional (see [`split_with`]).
pub fn translate_with_opts(s: &str, split_sentences: bool, mut tr: impl FnMut(&[&str]) -> Vec<String>) -> String {
    let pieces = split_with(s, split_sentences);
    let cores: Vec<&str> = pieces
        .iter()
        .filter_map(|p| match p {
            Piece::Text(t) if translatable(t) => Some(joints(t).1),
            _ => None,
        })
        .collect();
    let mut done = tr(&cores).into_iter();
    let mut out = String::with_capacity(s.len() * 2);
    for p in pieces {
        match p {
            Piece::Markup(m) => out.push_str(m),
            Piece::Text(t) if translatable(t) => {
                let (lead, core, trail) = joints(t);
                out.push_str(lead);
                match done.next() {
                    Some(translated) if !translated.trim().is_empty() => out.push_str(&fit(core, lead, &translated)),
                    _ => out.push_str(core),
                }
                out.push_str(trail);
            }
            Piece::Text(t) => out.push_str(t),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Piece::*;

    #[test]
    fn splits_game_markup() {
        assert_eq!(
            split("<c=@reminder>Hit %num1% foe[s].</c><br>Next"),
            vec![
                Markup("<c=@reminder>"),
                Text("Hit "),
                Markup("%num1%"),
                Text(" foe"),
                Markup("[s]"),
                Text("."),
                Markup("</c>"),
                Markup("<br>"),
                Text("Next"),
            ]
        );
        assert_eq!(
            split("%num1% Match[pl:\"Matches\"] Won\n"),
            vec![
                Markup("%num1%"),
                Text(" Match"),
                Markup("[pl:\"Matches\"]"),
                Text(" Won"),
                Markup("\n")
            ]
        );
    }

    #[test]
    fn keeps_escaped_percent_and_splits_sentences() {
        assert_eq!(
            split("+15%% Magic Find"),
            vec![Text("+15"), Markup("%%"), Text(" Magic Find")]
        );
        assert_eq!(
            split("(chuckle) Got to be patient. Good fortune is coming."),
            vec![
                Text("(chuckle)"),
                Text(" "),
                Text("Got to be patient."),
                Text(" "),
                Text("Good fortune is coming.")
            ]
        );
        assert_eq!(split("Wait... what?"), vec![Text("Wait..."), Text(" "), Text("what?")]);
    }

    #[test]
    fn leaves_ordinary_brackets_and_percent_alone() {
        assert_eq!(split("50% [Guild] < 3"), vec![Text("50% [Guild] < 3")]);
        assert_eq!(
            split("[lbracket]x[rbracket]"),
            vec![Markup("[lbracket]"), Text("x"), Markup("[rbracket]")]
        );
    }

    #[test]
    fn rebuilds_with_translated_text_only() {
        let out = translate_with("<c=@flavor>Hello there.</c>\n\n%str1% completed.", |xs| {
            assert_eq!(xs, ["Hello there.", "completed."]);
            vec!["Olá.".into(), "concluído.".into()]
        });
        assert_eq!(out, "<c=@flavor>Olá.</c>\n\n%str1% concluído.");
    }

    #[test]
    fn keeps_punctuation_around_variables() {
        let en = "All right, %str1%, are you ready? First mission.";
        let out = translate_with(en, |xs| {
            assert_eq!(xs, ["All right", "are you ready?", "First mission."]);
            vec!["Tudo bem".into(), "Está pronto?".into(), "Primeira missão.".into()]
        });
        assert_eq!(out, "Tudo bem, %str1%, está pronto? Primeira missão.");
        let out = translate_with("All right, %str1%, are you ready?", |_| {
            vec!["Tudo bem.".into(), "Está pronto?".into()]
        });
        assert_eq!(out, "Tudo bem, %str1%, está pronto?");
    }

    #[test]
    fn can_keep_sentences_together() {
        let out = translate_with_opts("<c=@flavor>Hello there. Bye.</c>", false, |xs| {
            assert_eq!(xs, ["Hello there. Bye."]);
            vec!["Olá. Tchau.".into()]
        });
        assert_eq!(out, "<c=@flavor>Olá. Tchau.</c>");
    }

    #[test]
    fn missing_translation_keeps_original_piece() {
        assert_eq!(translate_with("Hi <br> there", |_| vec!["Oi".into()]), "Oi <br> there");
    }
}
