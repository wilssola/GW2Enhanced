//! Steam's text KeyValues files (`libraryfolders.vdf`, `appmanifest_*.acf`, `localconfig.vdf`).
//!
//! Parsed into a tree that remembers where each value sits in the text, so a value can be changed or added without
//! rewriting the rest of the file (Steam's own files keep their layout).

use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str { text: String, span: Range<usize> },
    /// `{ ... }`: the entries, and the position right after the `{` and of the `}`.
    Block { entries: Vec<(String, Value)>, open_end: usize, close: usize },
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Block { entries, .. } => entries.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v),
            Value::Str { .. } => None,
        }
    }

    /// Follows `path` (keys, case-insensitive).
    pub fn path(&self, path: &[&str]) -> Option<&Value> {
        path.iter().try_fold(self, |v, k| v.get(k))
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str { text, .. } => Some(text),
            Value::Block { .. } => None,
        }
    }

    pub fn entries(&self) -> &[(String, Value)] {
        match self {
            Value::Block { entries, .. } => entries,
            Value::Str { .. } => &[],
        }
    }
}

enum Tok {
    Str(String, Range<usize>),
    Open(usize),
    Close(usize),
}

fn tokens(text: &str) -> Result<Vec<Tok>, String> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                out.push(Tok::Open(i + 1));
                i += 1;
            }
            b'}' => {
                out.push(Tok::Close(i));
                i += 1;
            }
            b'"' => {
                let start = i;
                let mut s = String::new();
                i += 1;
                loop {
                    let Some(&c) = b.get(i) else { return Err(tr!("aspas sem fechar no arquivo da Steam", "unclosed quote in the Steam file")) };
                    match c {
                        b'"' => break,
                        b'\\' if i + 1 < b.len() => {
                            s.push(match b[i + 1] {
                                b'n' => '\n',
                                b't' => '\t',
                                other => other as char,
                            });
                            i += 2;
                        }
                        _ => {
                            // Whole UTF-8 characters.
                            let ch = text[i..].chars().next().unwrap_or('?');
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                i += 1;
                out.push(Tok::Str(s, start..i));
            }
            _ => {
                // Unquoted token (rare in Steam's files): up to whitespace or a brace.
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'{' | b'}' | b'"') {
                    i += 1;
                }
                out.push(Tok::Str(text[start..i].to_string(), start..i));
            }
        }
    }
    Ok(out)
}

/// The whole file as one block (its top-level entries).
pub fn parse(text: &str) -> Result<Value, String> {
    let toks = tokens(text)?;
    let mut pos = 0;
    let entries = parse_entries(&toks, &mut pos, false)?;
    Ok(Value::Block { entries, open_end: 0, close: text.len() })
}

fn parse_entries(toks: &[Tok], pos: &mut usize, nested: bool) -> Result<Vec<(String, Value)>, String> {
    let mut entries = Vec::new();
    loop {
        match toks.get(*pos) {
            None if nested => return Err(tr!("chave sem fechar no arquivo da Steam", "unclosed brace in the Steam file")),
            None => return Ok(entries),
            Some(Tok::Close(_)) if nested => return Ok(entries),
            Some(Tok::Close(_)) | Some(Tok::Open(_)) => return Err(tr!("arquivo da Steam malformado", "malformed Steam file")),
            Some(Tok::Str(key, _)) => {
                *pos += 1;
                let key = key.clone();
                match toks.get(*pos) {
                    Some(Tok::Str(v, span)) => {
                        entries.push((key, Value::Str { text: v.clone(), span: span.clone() }));
                        *pos += 1;
                    }
                    Some(Tok::Open(open_end)) => {
                        let open_end = *open_end;
                        *pos += 1;
                        let inner = parse_entries(toks, pos, true)?;
                        let Some(Tok::Close(close)) = toks.get(*pos) else {
                            return Err(tr!("chave sem fechar no arquivo da Steam", "unclosed brace in the Steam file"));
                        };
                        entries.push((key, Value::Block { entries: inner, open_end, close: *close }));
                        *pos += 1;
                    }
                    _ => return Err(tr!("valor faltando para {key:?} no arquivo da Steam", "missing value for {key:?} in the Steam file")),
                }
            }
        }
    }
}

/// `s` as a quoted KeyValues string.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The indentation (tabs) of the line `at` is on.
fn indent_at(text: &str, at: usize) -> String {
    let line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..].chars().take_while(|c| *c == '\t' || *c == ' ').collect()
}

/// Sets `path` (the last element is the key of a string value) to `value`, creating the missing blocks under the
/// deepest one that exists. Returns the new text.
pub fn set(text: &str, path: &[&str], value: &str) -> Result<String, String> {
    let root = parse(text)?;
    let (key, blocks) = path.split_last().ok_or_else(|| tr!("caminho vazio", "empty path"))?;
    // Deepest existing block along the path.
    let mut cur = &root;
    let mut depth = 0;
    for k in blocks {
        match cur.get(k) {
            Some(v @ Value::Block { .. }) => {
                cur = v;
                depth += 1;
            }
            Some(Value::Str { .. }) => return Err(tr!("{k:?} não é um bloco no arquivo da Steam", "{k:?} is not a block in the Steam file")),
            None => break,
        }
    }
    if depth == blocks.len()
        && let Some(existing) = cur.get(key)
    {
        let Value::Str { span, .. } = existing else {
            return Err(tr!("{key:?} é um bloco no arquivo da Steam", "{key:?} is a block in the Steam file"));
        };
        return Ok(format!("{}{}{}", &text[..span.start], quote(value), &text[span.end..]));
    }
    let Value::Block { close, .. } = cur else { unreachable!() };
    // Insert before the closing brace of the deepest block (at the end of the file for the root, depth 0).
    let (at, base) = if depth == 0 { (text.len(), String::new()) } else { (*close, indent_at(text, *close) + "\t") };
    let mut add = String::new();
    let mut ind = base.clone();
    for k in &blocks[depth..] {
        add.push_str(&format!("{ind}{}\n{ind}{{\n", quote(k)));
        ind.push('\t');
    }
    add.push_str(&format!("{ind}{}\t\t{}\n", quote(key), quote(value)));
    for _ in &blocks[depth..] {
        ind.pop();
        add.push_str(&format!("{ind}}}\n"));
    }
    // The closing brace sits after its own indentation: insert at the start of that line.
    let line_start = text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let insert_at = if text[line_start..at].trim().is_empty() { line_start } else { at };
    let mut out = String::with_capacity(text.len() + add.len() + 1);
    out.push_str(&text[..insert_at]);
    if insert_at > 0 && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&add);
    out.push_str(&text[insert_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"1284210\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1700000000\"\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-provider Portal\"\n\t\t\t\t\t}\n\t\t\t\t\t\"440\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";
    const PATH: &[&str] = &["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", "1284210", "LaunchOptions"];

    #[test]
    fn parses_nested_blocks_and_escapes() {
        let v = parse("\"a\"\n{\n\t\"b\"\t\t\"x \\\"q\\\" \\\\ y\"\n\t// comment\n\t\"c\" { \"d\" \"1\" }\n}\n").unwrap();
        assert_eq!(v.path(&["a", "b"]).and_then(Value::str), Some("x \"q\" \\ y"));
        assert_eq!(v.path(&["A", "C", "d"]).and_then(Value::str), Some("1"));
        assert!(parse("\"a\" { \"b\" \"c\"").is_err());
        assert!(parse("\"a\" { \"b\" \"c").is_err());
    }

    #[test]
    fn replaces_an_existing_value_in_place() {
        let out = set(LOCALCONFIG, PATH, "WINEDLLOVERRIDES=\"msimg32=n,b\" %command% -provider Portal").unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(
            v.path(PATH).and_then(Value::str),
            Some("WINEDLLOVERRIDES=\"msimg32=n,b\" %command% -provider Portal")
        );
        assert!(out.contains("\"LaunchOptions\"\t\t\"WINEDLLOVERRIDES=\\\"msimg32=n,b\\\" %command% -provider Portal\""));
        // Everything else is byte for byte the same.
        assert_eq!(out.replace("WINEDLLOVERRIDES=\\\"msimg32=n,b\\\" %command% ", ""), LOCALCONFIG);
    }

    #[test]
    fn adds_missing_keys_and_blocks() {
        let no_opts = LOCALCONFIG.replace("\t\t\t\t\t\t\"LaunchOptions\"\t\t\"-provider Portal\"\n", "");
        let out = set(&no_opts, PATH, "x %command%").unwrap();
        assert_eq!(parse(&out).unwrap().path(PATH).and_then(Value::str), Some("x %command%"));
        assert_eq!(parse(&out).unwrap().path(&PATH[..5]).unwrap().entries().len(), 2);

        let mut other = PATH.to_vec();
        other[5] = "999";
        let out = set(LOCALCONFIG, &other, "y").unwrap();
        let v = parse(&out).unwrap();
        assert_eq!(v.path(&other).and_then(Value::str), Some("y"));
        assert_eq!(v.path(PATH).and_then(Value::str), Some("-provider Portal"));
        assert!(out.contains("\t\t\t\t\t\"999\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LaunchOptions\"\t\t\"y\"\n\t\t\t\t\t}\n"), "{out}");

        let out = set("", &["a", "b"], "c").unwrap();
        assert_eq!(parse(&out).unwrap().path(&["a", "b"]).and_then(Value::str), Some("c"));
    }
}
