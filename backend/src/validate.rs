//! Rule-based checks on untrusted English texts and translations.
//!
//! The markup rules are a port of the mod's `src/dict.rs` (`markup_signature`,
//! `unusable`), so the server never accepts a translation the mod would refuse
//! or that would break the game's markup.

/// Largest English text or translation accepted, in bytes (GW2 books are long,
/// but nothing in the game comes close to this).
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Marker the game itself uses for removed or empty strings.
pub const NULL_MARKER: &str = "[null]";

/// Why a text or translation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Empty,
    TooLong,
    ControlChars,
    Link,
    NotText,
    Markup,
    Fixups,
    HidesOriginal,
    Length,
    Source,
    Id,
}

impl Reject {
    pub fn as_str(self) -> &'static str {
        match self {
            Reject::Empty => "empty",
            Reject::TooLong => "too_long",
            Reject::ControlChars => "control_chars",
            Reject::Link => "link",
            Reject::NotText => "not_text",
            Reject::Markup => "markup",
            Reject::Fixups => "fixups",
            Reject::HidesOriginal => "hides_original",
            Reject::Length => "length",
            Reject::Source => "source",
            Reject::Id => "id",
        }
    }
}

/// A translation that would show `[null]` where the original text does not.
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

/// A translation that uses a `[...]` fixup the original does not have (only
/// `[s]`, the plural ending, is valid in any text).
pub fn adds_fixups(original: &str, translation: &str) -> bool {
    let known = fixups(original);
    fixups(translation).iter().any(|f| *f != "s" && !known.contains(f))
}

/// A translation the mod refuses to load.
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

/// Domains a game text may legitimately mention.
const ALLOWED_DOMAINS: [&str; 3] = ["guildwars2.com", "arena.net", "ncsoft.com"];

/// Whether `s` contains something that looks like a link or an invite.
pub fn has_link(s: &str) -> bool {
    let lower = s.to_lowercase();
    let markers = [
        "http://",
        "https://",
        "www.",
        "discord.gg",
        "discord.com/invite",
        "t.me/",
        "bit.ly",
        ".com/",
        ".gg/",
        ".io/",
        ".xyz",
        "://",
    ];
    if !markers.iter().any(|m| lower.contains(m)) {
        return false;
    }
    // Every link-looking word must point at an allowed domain.
    lower
        .split(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"' || c == '(' || c == ')')
        .filter(|w| markers.iter().any(|m| w.contains(m)))
        .any(|w| !ALLOWED_DOMAINS.iter().any(|d| w.contains(d)))
}

/// Control characters (other than line breaks and tabs), bidi overrides and
/// isolates, directional marks, zero-width characters, line/paragraph
/// separators and the BOM: never needed in game text, and used to spoof how a
/// text displays.
pub fn has_bad_control(s: &str) -> bool {
    s.chars().any(|c| {
        (c.is_control() && c != '\n' && c != '\r' && c != '\t')
            || matches!(c, '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
    })
}

/// Checks an English text sent by a player (it can be forged).
pub fn check_en(en: &str) -> Result<(), Reject> {
    if en.trim().is_empty() {
        return Err(Reject::Empty);
    }
    if en.len() > MAX_TEXT_BYTES {
        return Err(Reject::TooLong);
    }
    if has_bad_control(en) {
        return Err(Reject::ControlChars);
    }
    if has_link(en) {
        return Err(Reject::Link);
    }
    if !looks_like_text(en) {
        return Err(Reject::NotText);
    }
    Ok(())
}

/// Cheap heuristic: game text has some letters, few odd symbols and no long
/// runs of one character (keyboard mashing, binary garbage).
pub fn looks_like_text(s: &str) -> bool {
    let total = s.chars().filter(|c| !c.is_whitespace()).count();
    if total == 0 {
        return false;
    }
    // Tiny texts ("+", "1/2", "%num1%") exist in the game: let them through.
    if total <= 3 {
        return true;
    }
    let letters = s.chars().filter(|c| c.is_alphabetic()).count();
    let odd = s
        .chars()
        .filter(|c| !c.is_alphanumeric() && !c.is_whitespace() && !"%<>=@/.,;:!?'\"()[]-+*#&_~|{}".contains(*c))
        .count();
    let mut run = 1;
    let mut max_run = 1;
    let mut prev = None;
    for c in s.chars() {
        if Some(c) == prev && !c.is_ascii_digit() && c != '.' && c != '-' && c != '=' && c != ' ' {
            run += 1;
            max_run = max_run.max(run);
        } else {
            run = 1;
        }
        prev = Some(c);
    }
    letters * 4 >= total && odd * 5 <= total && max_run <= 6
}

/// Checks a translation of `en` (from a player's PC or from the site).
pub fn check_tr(en: &str, tr: &str) -> Result<(), Reject> {
    if tr.trim().is_empty() {
        return Err(Reject::Empty);
    }
    if tr.len() > MAX_TEXT_BYTES {
        return Err(Reject::TooLong);
    }
    if has_bad_control(tr) {
        return Err(Reject::ControlChars);
    }
    if has_link(tr) && !has_link(en) {
        return Err(Reject::Link);
    }
    if hides_original(en, tr) {
        return Err(Reject::HidesOriginal);
    }
    if adds_fixups(en, tr) {
        return Err(Reject::Fixups);
    }
    if markup_signature(en) != markup_signature(tr) {
        return Err(Reject::Markup);
    }
    let (a, b) = (en.chars().count(), tr.chars().count());
    if b > a * 4 + 24 || (a >= 24 && b * 4 < a) {
        return Err(Reject::Length);
    }
    if en.contains('\n') != tr.contains('\n') && en.matches('\n').count().abs_diff(tr.matches('\n').count()) > 2 {
        return Err(Reject::Markup);
    }
    Ok(())
}

/// Maps an uploaded translation's `src` to the stored source: only `argos` (made on the player's PC) is accepted.
pub fn source(src: Option<&str>) -> Result<&'static str, Reject> {
    match src {
        // Only translations made on the player's PC. The server's AI translations (`ai` / `nemotron`) come from the
        // server itself, never from an upload.
        Some("argos") => Ok("argos"),
        _ => Err(Reject::Source),
    }
}

/// A language tag like `pt-BR` that is in the server's allow list
/// (`LANGS` var, comma separated).
pub fn lang_allowed(lang: &str, allowed: &str) -> bool {
    !lang.is_empty() && allowed.split(',').map(str::trim).any(|l| l == lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ported_fixup_rules() {
        assert!(adds_fixups(" [null]Raven[s]", " [null]Corvo[es]"));
        assert!(!adds_fixups(" [null]Raven[s]", " [null]Corvo[s]"));
        assert!(!adds_fixups("Hit %num1% foe[s].", "Acerte %num1% inimigo[s]."));
        assert!(!adds_fixups("[pl:\"Wolf\"]", "[pl:\"Lobo\"]"));
        assert!(!adds_fixups("Press [Enter]", "Pressione [Enter]"));
        assert!(!adds_fixups("%num1% Owned", "%num1% Possuído[s]"));
        assert!(adds_fixups("%str1% picks up [the] %str2%.", "%str1% pega [o] %str2%."));
        assert!(unusable("Sword", "[null]"));
        assert!(!unusable(", [null]", ", [null]"));
    }

    #[test]
    fn ported_markup_signature() {
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
    fn translation_checks() {
        assert_eq!(check_tr("The Cost of Victory", "O Custo da Vitória"), Ok(()));
        assert_eq!(check_tr("Hit %num1% foe[s].", "Acerte inimigos."), Err(Reject::Markup));
        assert_eq!(check_tr("Raven[s]", "Corvo[es]"), Err(Reject::Fixups));
        assert_eq!(check_tr("Sword", "[null]"), Err(Reject::HidesOriginal));
        assert_eq!(check_tr("Sword", "  "), Err(Reject::Empty));
        assert_eq!(check_tr("Sword", "Espada https://evil.example"), Err(Reject::Link));
        assert_eq!(check_tr("Sword", "Visite discord.gg/abc"), Err(Reject::Link));
        assert_eq!(check_tr("Sword", &"a".repeat(200)), Err(Reject::Length));
        assert_eq!(check_tr("Sword", "Esp\u{0007}ada"), Err(Reject::ControlChars));
        for spoof in [
            '\u{202E}', '\u{2066}', '\u{200F}', '\u{200B}', '\u{061C}', '\u{FEFF}', '\u{2028}',
        ] {
            assert_eq!(
                check_tr("Sword", &format!("Esp{spoof}ada")),
                Err(Reject::ControlChars),
                "{spoof:?}"
            );
        }
        assert_eq!(check_tr("Ok", "Ok"), Ok(()));
        assert_eq!(check_tr("Line one\nLine two", "Linha um\nLinha dois"), Ok(()));
    }

    #[test]
    fn english_checks() {
        assert_eq!(check_en("Ingredient"), Ok(()));
        assert_eq!(check_en("<c=@flavor>Hit %num1% foe[s].</c><br>"), Ok(()));
        assert_eq!(check_en("+5"), Ok(()));
        assert_eq!(check_en("Visit www.guildwars2.com for news"), Ok(()));
        assert_eq!(check_en("Buy gold at www.cheapgold.xyz"), Err(Reject::Link));
        assert_eq!(check_en("aaaaaaaaaaaaaaaaaaaa"), Err(Reject::NotText));
        assert_eq!(check_en("¤¤¤¤§§§§¤¤"), Err(Reject::NotText));
        assert_eq!(check_en(""), Err(Reject::Empty));
        assert_eq!(check_en(&"word ".repeat(2000)), Err(Reject::TooLong));
    }

    #[test]
    fn sources_and_langs() {
        assert_eq!(source(Some("argos")), Ok("argos"));
        assert_eq!(source(Some("nemotron")), Err(Reject::Source));
        assert_eq!(source(Some("ai")), Err(Reject::Source));
        assert_eq!(source(Some("user")), Err(Reject::Source));
        assert!(lang_allowed("pt-BR", "pt-BR, es"));
        assert!(!lang_allowed("pt", "pt-BR"));
    }
}
