//! Translates pending strings through an OpenAI-compatible API
//! (`/chat/completions`). Inputs are JSON Lines with an `en` field
//! (`discovered.jsonl`, `export_pending` output, or `Translation` lines);
//! the output is JSON Lines of `Translation` tagged with the model in `src`.
//!
//! ```text
//! gw2-translate --input <pending.jsonl>... --output <translated.jsonl> [--existing <dictionary.jsonl>]
//!               [--batch 25] [--workers 8]
//! ```
//!
//! Configured through environment variables (the key is never written to a file):
//!
//! | Variable | Value |
//! |---|---|
//! | `OPENAI_API_KEY` | API key |
//! | `OPENAI_BASE_URL` | Base URL of the API, up to `/v1` |
//! | `GW2TRANSLATED_MODEL` | Model name, as that API expects it |
//!
//! Output is written after every batch; running again resumes where it stopped.
//! Translations whose markup does not match the English are discarded.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use version::dict::{markup_signature, unusable};
use version::records::{self, Appender, Discovered, Translation};

const RETRIES: u32 = 5;

const SYSTEM_PROMPT: &str = "You are a professional video game localizer translating Guild Wars 2 strings from \
English to Brazilian Portuguese (pt-BR). You receive a JSON array of strings. Reply with ONLY a JSON array of \
strings with exactly the same number of elements, in the same order, each element being the translation of the \
corresponding input. Rules: keep every placeholder, tag and markup EXACTLY as written (e.g. %str1%, %num1%, %%, \
<c=@task>, </c>, <br>, [lbracket], [rbracket]); never translate or reorder inside them; keep \
only personal names, place names and the setting's proper nouns (Tyria, Lion's Arch, Charr, Asura, Sylvari, Norn) \
untranslated; ALWAYS translate everything else, including epithets, titles and descriptive words inside names \
(e.g. \"Mikaeel the Trustworthy\" -> \"Mikaeel, o Confiável\", \"Glob of Goo\" -> \"Bolota de Gosma\"); never return \
an English sentence unchanged; never add [...] tags; an input marked \"(singular)\" is followed by the \
same text marked \"(plural)\": translate both with exactly the same words and word order, changing only number \
agreement, and do not include the marks in the reply; the English \
article tags [the], [a], [an] must become the right Portuguese article as plain text (e.g. \"[an] %str1%\" -> \
\"um(a) %str1%\"); use the game's terms: Skin = Visual, Finisher = Finalizador, Dye = Tintura, \
Magic Find = Descoberta Mágica, Runestone = Pedra Rúnica, Tonic = Tônico, Mini = Mini, Glider = Planador, \
Mount = Montaria, Waypoint = Ponto de Teleporte, Hero Point = Ponto de Herói, Vista = Vista, Achievement = \
Conquista, Mastery = Maestria, Commander = Comandante; preserve line breaks; do not add commentary, numbering \
or code fences.";

struct Api {
    agent: ureq::Agent,
    url: String,
    key: String,
    model: String,
}

impl Api {
    fn from_env() -> Result<Self> {
        let key = std::env::var("OPENAI_API_KEY").context("set OPENAI_API_KEY")?;
        let base = std::env::var("OPENAI_BASE_URL").context("set OPENAI_BASE_URL")?;
        let model = std::env::var("GW2TRANSLATED_MODEL").context("set GW2TRANSLATED_MODEL")?;
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(180)).build();
        Ok(Self {
            agent,
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            key,
            model,
        })
    }

    fn translate(&self, texts: &[String]) -> Result<Vec<String>> {
        let mut body = json!({
            "model": self.model,
            "temperature": 0.1,
            "top_p": 0.9,
            "max_tokens": 4096,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": serde_json::to_string(texts)?},
            ],
        });
        if self.model.contains("nemotron") {
            // No model "thinking": just the answer.
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
        let mut last = None;
        for attempt in 0..RETRIES {
            match self
                .agent
                .post(&self.url)
                .set("Authorization", &format!("Bearer {}", self.key))
                .send_json(&body)
            {
                Ok(r) => {
                    let v: Value = r.into_json()?;
                    let content = v["choices"][0]["message"]["content"].as_str().unwrap_or_default();
                    return parse_array(content, texts.len());
                }
                Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => last = Some(format!("HTTP {code}")),
                Err(ureq::Error::Status(code, r)) => bail!("HTTP {code}: {}", r.into_string().unwrap_or_default()),
                Err(e) => last = Some(e.to_string()),
            }
            std::thread::sleep(Duration::from_secs(2u64.pow(attempt + 1)));
        }
        bail!("gave up after {RETRIES} attempts: {}", last.unwrap_or_default())
    }
}

/// The response must be a JSON array of strings, the same length as the request.
fn parse_array(content: &str, expected: usize) -> Result<Vec<String>> {
    let t = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let v: Value = match serde_json::from_str(t) {
        Ok(v) => v,
        Err(_) => {
            let (a, b) = (
                t.find('[').context("response has no array")?,
                t.rfind(']').context("response has no array")?,
            );
            serde_json::from_str(&t[a..=b])?
        }
    };
    let arr = v.as_array().context("response is not an array")?;
    if arr.len() != expected {
        bail!("expected {expected} items, got {}", arr.len());
    }
    arr.iter()
        .map(|x| x.as_str().map(str::to_owned).context("item is not a string"))
        .collect()
}

/// Keeps the original's leading/trailing whitespace, which the model tends to drop.
fn keep_edges(en: &str, pt: &str) -> String {
    let lead = &en[..en.len() - en.trim_start().len()];
    let trail = &en[en.trim_end().len()..];
    format!("{lead}{}{trail}", pt.trim())
}

/// The `en` field of every line (any record that has one).
fn read_en(path: &str) -> Result<Vec<String>> {
    let r = records::read::<Discovered>(Path::new(path)).with_context(|| path.to_owned())?;
    if r.skipped > 0 {
        eprintln!("{path}: skipped {} unreadable lines", r.skipped);
    }
    Ok(r.records.into_iter().map(|d| d.en).collect())
}

const SINGULAR: &str = "(singular) ";
const PLURAL: &str = "(plural) ";

/// The text has plural markup (`[s]`, `[pl:"..."]`), which the model can't
/// place reliably: it is translated as a singular/plural pair instead.
fn has_plural(en: &str) -> bool {
    en.contains("[s]") || en.contains("[pl:\"")
}

/// `en` with its plural markup resolved: `Item[s]` → `Item`/`Items`,
/// `Match[pl:"Matches"]` → `Match`/`Matches`.
fn resolve_plural(en: &str, plural: bool) -> String {
    let mut out = String::with_capacity(en.len());
    let mut rest = en;
    while let Some(i) = rest.find('[') {
        let (head, tail) = rest.split_at(i);
        out.push_str(head);
        if let Some(after) = tail.strip_prefix("[s]") {
            if plural {
                out.push('s');
            }
            rest = after;
        } else if let Some(end) = tail.strip_prefix("[pl:\"").and_then(|t| t.find("\"]")) {
            let form = &tail[5..5 + end];
            if plural {
                let start = out.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                out.truncate(start);
                out.push_str(form);
            }
            rest = &tail[5 + end + 2..];
        } else {
            out.push('[');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Splits `word` into (leading punctuation, core, trailing punctuation).
fn split_punct(word: &str) -> (&str, &str, &str) {
    let core_start = word.find(char::is_alphanumeric).unwrap_or(word.len());
    let core_end = word
        .rfind(char::is_alphanumeric)
        .map_or(core_start, |i| i + word[i..].chars().next().map_or(1, char::len_utf8));
    (
        &word[..core_start],
        &word[core_start..core_end.max(core_start)],
        &word[core_end.max(core_start)..],
    )
}

/// Builds the game's plural markup from the singular and plural translations:
/// a word that only gains an "s" gets `[s]`, any other change `[pl:"…"]`.
/// `None` when the two don't line up word by word.
fn plural_markup(singular: &str, plural: &str) -> Option<String> {
    let (s, p): (Vec<&str>, Vec<&str>) = (singular.split(' ').collect(), plural.split(' ').collect());
    if s.len() != p.len() {
        return None;
    }
    let mut out = Vec::with_capacity(s.len());
    for (a, b) in s.iter().zip(&p) {
        if a == b {
            out.push((*a).to_owned());
            continue;
        }
        let ((la, ca, ta), (lb, cb, tb)) = (split_punct(a), split_punct(b));
        if la != lb || ta != tb || ca.is_empty() || cb.is_empty() || ca.contains(['%', '<', '[']) {
            return None;
        }
        if cb.strip_suffix('s') == Some(ca) {
            out.push(format!("{la}{ca}[s]{ta}"));
        } else {
            out.push(format!("{la}{ca}[pl:\"{cb}\"]{ta}"));
        }
    }
    Some(out.join(" "))
}

/// Short tag for `src`: the model family, or the full model id.
fn source_tag(model: &str) -> String {
    if model.contains("nemotron") {
        "nemotron".into()
    } else {
        model.into()
    }
}

fn arg_values(args: &[String], name: &str) -> Vec<String> {
    args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inputs = arg_values(&args, "--input");
    let output = arg_values(&args, "--output").pop().context("missing --output")?;
    let batch: usize = arg_values(&args, "--batch")
        .pop()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25);
    let workers: usize = arg_values(&args, "--workers")
        .pop()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    if inputs.is_empty() {
        bail!(
            "usage: gw2-translate --input <jsonl>... --output <jsonl> [--existing <dictionary.jsonl>] [--batch 25] [--workers 8]"
        );
    }
    let api = Arc::new(Api::from_env()?);

    let mut done: HashSet<String> = HashSet::new();
    for path in arg_values(&args, "--existing").iter().chain(std::iter::once(&output)) {
        done.extend(read_en(path)?);
    }
    let mut todo: Vec<String> = Vec::new();
    for path in &inputs {
        for en in read_en(path)? {
            if !en.trim().is_empty() && done.insert(en.clone()) {
                todo.push(en);
            }
        }
    }
    println!("{} strings to translate with {} at {}", todo.len(), api.model, api.url);

    let src = Arc::new(source_tag(&api.model));
    let w = Appender::open(Path::new(&output)).with_context(|| output.clone())?;
    let out = Arc::new(Mutex::new((w, 0usize, 0usize)));
    let queue = Arc::new(Mutex::new(
        todo.chunks(batch).map(<[String]>::to_vec).collect::<Vec<_>>(),
    ));
    let total = todo.len();
    let threads: Vec<_> = (0..workers)
        .map(|_| {
            let (api, out, queue, src) = (api.clone(), out.clone(), queue.clone(), src.clone());
            std::thread::spawn(move || {
                while let Some(chunk) = queue.lock().unwrap().pop() {
                    let request: Vec<String> = chunk
                        .iter()
                        .flat_map(|en| {
                            if has_plural(en) {
                                vec![
                                    format!("{SINGULAR}{}", resolve_plural(en, false)),
                                    format!("{PLURAL}{}", resolve_plural(en, true)),
                                ]
                            } else {
                                vec![en.clone()]
                            }
                        })
                        .collect();
                    let result = api.translate(&request);
                    let mut g = out.lock().unwrap();
                    match result {
                        Ok(pts) => {
                            let mut pts = pts.into_iter();
                            for en in &chunk {
                                let pt = if has_plural(en) {
                                    let (s, p) = (pts.next().unwrap_or_default(), pts.next().unwrap_or_default());
                                    let unmark =
                                        |t: &str, m: &str| t.trim().trim_start_matches(m.trim()).trim().to_owned();
                                    plural_markup(&unmark(&s, SINGULAR), &unmark(&p, PLURAL))
                                } else {
                                    pts.next()
                                };
                                let Some(pt) = pt else {
                                    g.2 += 1;
                                    continue;
                                };
                                let pt = keep_edges(en, &pt);
                                if markup_signature(en) != markup_signature(&pt) || unusable(en, &pt) {
                                    g.2 += 1;
                                    continue;
                                }
                                let _ = g.0.write(&Translation {
                                    en: en.clone(),
                                    tr: pt,
                                    src: Some(src.to_string()),
                                    ..Default::default()
                                });
                                g.1 += 1;
                            }
                            let _ = g.0.flush();
                        }
                        Err(e) => {
                            eprintln!("batch failed: {e}");
                            g.2 += chunk.len();
                        }
                    }
                    println!("  {}/{total} translated, {} rejected", g.1, g.2);
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
    let g = out.lock().unwrap();
    println!("done: {} translated, {} rejected or failed -> {output}", g.1, g.2);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_or_wrapped_arrays() {
        assert_eq!(parse_array("```json\n[\"a\",\"b\"]\n```", 2).unwrap(), ["a", "b"]);
        assert_eq!(parse_array("Aqui: [\"x\"] fim", 1).unwrap(), ["x"]);
        assert!(parse_array("[\"x\"]", 2).is_err());
    }

    #[test]
    fn keeps_original_edge_whitespace() {
        assert_eq!(keep_edges("\n\nHi there.  ", "Olá."), "\n\nOlá.  ");
    }

    #[test]
    fn resolves_english_plural_markup() {
        assert_eq!(resolve_plural("%num1% Item[s] Salvaged", false), "%num1% Item Salvaged");
        assert_eq!(resolve_plural("%num1% Item[s] Salvaged", true), "%num1% Items Salvaged");
        let en = "%num1% Ranked Match[pl:\"Matches\"] Won";
        assert_eq!(resolve_plural(en, false), "%num1% Ranked Match Won");
        assert_eq!(resolve_plural(en, true), "%num1% Ranked Matches Won");
        assert_eq!(
            resolve_plural("[null]Sword [lbracket]x", true),
            "[null]Sword [lbracket]x"
        );
    }

    #[test]
    fn builds_portuguese_plural_markup_from_both_forms() {
        assert_eq!(
            plural_markup("%num1% Item Recuperado.", "%num1% Itens Recuperados.").unwrap(),
            "%num1% Item[pl:\"Itens\"] Recuperado[s]."
        );
        assert_eq!(
            plural_markup("Baú Exclusivo do Leão Negro", "Baús Exclusivos do Leão Negro").unwrap(),
            "Baú[s] Exclusivo[s] do Leão Negro"
        );
        assert_eq!(
            plural_markup("Saco de Bens", "Sacos de Bens").unwrap(),
            "Saco[s] de Bens"
        );
        assert!(plural_markup("Visual de Machado", "Visuais do Machado Afiado").is_none());
    }

    #[test]
    fn tags_the_source_with_the_model() {
        assert_eq!(source_tag("nvidia/nemotron-3.5-super-vl-120b-a12b"), "nemotron");
        assert_eq!(source_tag("gpt-4o"), "gpt-4o");
    }
}
