//! DirectML mode: any graphics card (NVIDIA, AMD, Intel), through ONNX Runtime.
//!
//! CTranslate2 has no DirectML backend, so this runs the same big model with
//! `encoder.onnx` and `decoder.onnx`: two graphs of half a megabyte whose
//! weights point into the `model.bin` of the CUDA mode (`scripts/export_onnx.py`).
//!
//! The decoder has no cache: each step decodes the whole prefix. That is simple
//! and needs no tensors moved back and forth between the card and the CPU.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, anyhow};
use ort::ep::DirectML;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

/// Next to the translator: `onnxruntime.dll` and `DirectML.dll`.
pub const RUNTIME_DIR: &str = "directml";

/// Is the model folder ready for DirectML?
pub fn has_graphs(model: &Path) -> bool {
    ["encoder.onnx", "decoder.onnx", "model.bin"]
        .iter()
        .all(|f| model.join(f).is_file())
}

pub fn has_runtime(runtime: &Path) -> bool {
    ["onnxruntime.dll", "DirectML.dll"]
        .iter()
        .all(|f| runtime.join(f).is_file())
}

pub struct Model {
    encoder: Session,
    decoder: Session,
    pieces: Vec<String>,
    ids: HashMap<String, i64>,
    unk: i64,
    eos: i64,
    /// The padding token, which also starts every translation. It is not in
    /// the vocabulary file: it comes right after the last entry.
    start: i64,
}

fn session(path: &Path, adapter: u32, threads: usize) -> anyhow::Result<Session> {
    let ep = DirectML::default()
        .with_device_id(adapter as i32)
        .build()
        .error_on_failure();
    // DirectML cannot run nodes in parallel nor use memory patterns.
    let build = || -> anyhow::Result<Session> {
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("{e}"))?
            .with_parallel_execution(false)
            .map_err(|e| anyhow!("{e}"))?
            .with_memory_pattern(false)
            .map_err(|e| anyhow!("{e}"))?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .with_execution_providers([ep])
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(path)?;
        Ok(session)
    };
    build().with_context(|| path.display().to_string())
}

impl Model {
    pub fn load(model: &Path, runtime: &Path, adapter: u32, threads: usize) -> anyhow::Result<Self> {
        // ONNX Runtime asks for `DirectML.dll` by name, and Windows has an older
        // one: loading ours first makes it the one in use.
        anyhow::ensure!(
            crate::gpu::load_library(&runtime.join("DirectML.dll")),
            "could not load DirectML.dll"
        );
        // Microsoft's Windows builds send telemetry by default; this translator stays offline.
        ort::init_from(runtime.join("onnxruntime.dll"))
            .map_err(|e| anyhow!("{e}"))?
            .with_telemetry(false)
            .commit();

        let vocabulary = std::fs::read_to_string(model.join("shared_vocabulary.json"))?;
        let pieces: Vec<String> = serde_json::from_str(&vocabulary)?;
        let ids: HashMap<String, i64> = pieces.iter().enumerate().map(|(i, p)| (p.clone(), i as i64)).collect();
        let id = |piece: &str| {
            ids.get(piece)
                .copied()
                .with_context(|| format!("{piece} is not in the vocabulary"))
        };
        let (unk, eos) = (id("<unk>")?, id("</s>")?);
        let start = pieces.len() as i64;
        Ok(Self {
            encoder: session(&model.join("encoder.onnx"), adapter, threads)?,
            decoder: session(&model.join("decoder.onnx"), adapter, threads)?,
            pieces,
            ids,
            unk,
            eos,
            start,
        })
    }

    /// Translates tokenized sentences in one batch; returns their output tokens.
    pub fn translate(&mut self, sources: &[Vec<String>], beam: usize) -> anyhow::Result<Vec<Vec<String>>> {
        let tokens: Vec<Vec<i64>> = sources
            .iter()
            .map(|s| {
                s.iter()
                    .map(|p| self.ids.get(p).copied().unwrap_or(self.unk))
                    .chain([self.eos])
                    .collect()
            })
            .collect();
        let len = tokens.iter().map(Vec::len).max().unwrap_or(0);
        let mut input = Vec::with_capacity(tokens.len() * len);
        let mut mask = Vec::with_capacity(tokens.len() * len);
        for t in &tokens {
            input.extend(t.iter().copied().chain(std::iter::repeat_n(self.start, len - t.len())));
            mask.extend((0..len).map(|i| i64::from(i < t.len())));
        }
        let shape = [tokens.len(), len];
        let outputs = self
            .encoder
            .run(ort::inputs![
                "input_ids" => Tensor::from_array((shape, input))?,
                "attention_mask" => Tensor::from_array((shape, mask.clone()))?,
            ])
            .map_err(|e| anyhow!("encoder: {e}"))?;
        let (hidden_shape, hidden) = outputs["encoder_hidden_states"].try_extract_tensor::<f32>()?;
        let width = hidden_shape[2] as usize;
        let hidden = hidden.to_vec();
        drop(outputs);

        let decoder = &mut self.decoder;
        // Enough for any real translation; stops the rare input the model repeats forever.
        let limits: Vec<usize> = tokens.iter().map(|t| t.len() * 2 + 16).collect();
        let vocabulary = self.pieces.len() + 1;
        let found = search(
            &limits,
            Tokens {
                start: self.start,
                eos: self.eos,
            },
            beam,
            vocabulary,
            |rows| {
                let (count, target) = (rows.len(), rows[0].1.len());
                let mut ids = Vec::with_capacity(count * target);
                let mut states = Vec::with_capacity(count * len * width);
                let mut masks = Vec::with_capacity(count * len);
                for (sentence, prefix) in rows {
                    ids.extend_from_slice(prefix);
                    states.extend_from_slice(&hidden[sentence * len * width..(sentence + 1) * len * width]);
                    masks.extend_from_slice(&mask[sentence * len..(sentence + 1) * len]);
                }
                let outputs = decoder
                    .run(ort::inputs![
                        "input_ids" => Tensor::from_array(([count, target], ids))?,
                        "encoder_hidden_states" => Tensor::from_array(([count, len, width], states))?,
                        "encoder_attention_mask" => Tensor::from_array(([count, len], masks))?,
                    ])
                    .map_err(|e| anyhow!("decoder: {e}"))?;
                let (_, logits) = outputs["logits"].try_extract_tensor::<f32>()?;
                Ok(logits.to_vec())
            },
        )?;
        Ok(found
            .into_iter()
            .map(|ids| ids.into_iter().map(|i| self.pieces[i as usize].clone()).collect())
            .collect())
    }
}

#[derive(Clone, Copy)]
struct Tokens {
    start: i64,
    eos: i64,
}

struct Hypothesis {
    tokens: Vec<i64>,
    score: f32,
}

struct Sentence {
    index: usize,
    beams: Vec<Hypothesis>,
    /// Best finished translation: score per token, tokens.
    best: Option<(f32, Vec<i64>)>,
    steps_left: usize,
}

impl Sentence {
    /// What this sentence ended with, without the start token and the end token.
    fn result(self, eos: i64) -> Vec<i64> {
        let tokens = match self.best {
            Some((_, tokens)) => tokens,
            None => self.beams.into_iter().next().map(|h| h.tokens).unwrap_or_default(),
        };
        tokens.into_iter().skip(1).filter(|&t| t != eos).collect()
    }
}

/// Beam search over several sentences at once, like CTranslate2's: a score is
/// the sum of the log probabilities divided by the length, and a sentence ends
/// when its best hypothesis ends. `step` receives `(sentence, prefix)` rows, all
/// of the same length, and returns `vocabulary` logits per row.
fn search(
    limits: &[usize],
    tokens: Tokens,
    beam: usize,
    vocabulary: usize,
    mut step: impl FnMut(&[(usize, &[i64])]) -> anyhow::Result<Vec<f32>>,
) -> anyhow::Result<Vec<Vec<i64>>> {
    let mut results = vec![Vec::new(); limits.len()];
    let mut active: Vec<Sentence> = limits
        .iter()
        .enumerate()
        .map(|(index, &steps_left)| Sentence {
            index,
            beams: vec![Hypothesis {
                tokens: vec![tokens.start],
                score: 0.0,
            }],
            best: None,
            steps_left,
        })
        .collect();
    while !active.is_empty() {
        let rows: Vec<(usize, &[i64])> = active
            .iter()
            .flat_map(|s| s.beams.iter().map(move |h| (s.index, h.tokens.as_slice())))
            .collect();
        let logits = step(&rows)?;
        anyhow::ensure!(
            logits.len() == rows.len() * vocabulary,
            "unexpected decoder output: {} values",
            logits.len()
        );
        drop(rows);

        let mut row = 0;
        let mut still = Vec::with_capacity(active.len());
        for mut sentence in active {
            let mut candidates: Vec<(f32, usize, i64)> = Vec::with_capacity(sentence.beams.len() * (beam + 1));
            for (from, hypothesis) in sentence.beams.iter().enumerate() {
                let line = &logits[row * vocabulary..(row + 1) * vocabulary];
                row += 1;
                let total = log_sum(line, tokens.start as usize);
                for (token, logit) in top(line, beam + 1, tokens.start as usize) {
                    candidates.push((hypothesis.score + logit - total, from, token as i64));
                }
            }
            candidates.sort_by(|a, b| b.0.total_cmp(&a.0));

            let top_ended = candidates.first().is_some_and(|c| c.2 == tokens.eos);
            let mut next = Vec::with_capacity(beam);
            for &(score, from, token) in &candidates {
                let previous = &sentence.beams[from].tokens;
                if token == tokens.eos {
                    let per_token = score / (previous.len() as f32);
                    if sentence.best.as_ref().is_none_or(|(s, _)| per_token > *s) {
                        sentence.best = Some((per_token, previous.clone()));
                    }
                } else if next.len() < beam {
                    let mut extended = previous.clone();
                    extended.push(token);
                    next.push(Hypothesis {
                        tokens: extended,
                        score,
                    });
                }
                if next.len() >= beam {
                    break;
                }
            }
            sentence.beams = next;
            sentence.steps_left = sentence.steps_left.saturating_sub(1);
            if top_ended || sentence.beams.is_empty() || sentence.steps_left == 0 {
                let index = sentence.index;
                results[index] = sentence.result(tokens.eos);
            } else {
                still.push(sentence);
            }
        }
        active = still;
    }
    Ok(results)
}

/// log(sum(exp(x))), without the column of the token that can't be produced.
fn log_sum(line: &[f32], skip: usize) -> f32 {
    let highest = line
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != skip)
        .map(|(_, &x)| x)
        .fold(f32::MIN, f32::max);
    let sum: f32 = line
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != skip)
        .map(|(_, &x)| (x - highest).exp())
        .sum();
    highest + sum.ln()
}

/// The `n` highest values and their positions.
fn top(line: &[f32], n: usize, skip: usize) -> Vec<(usize, f32)> {
    let mut best: Vec<(usize, f32)> = Vec::with_capacity(n + 1);
    for (i, &x) in line.iter().enumerate() {
        if i == skip || (best.len() == n && x <= best[n - 1].1) {
            continue;
        }
        let at = best.partition_point(|b| b.1 >= x);
        best.insert(at, (i, x));
        best.truncate(n);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKENS: Tokens = Tokens { start: 4, eos: 3 };
    const VOCABULARY: usize = 5;

    /// A fake decoder: each sentence follows a script of preferred tokens.
    fn run(scripts: &[&[i64]], beam: usize, limit: usize) -> Vec<Vec<i64>> {
        let limits = vec![limit; scripts.len()];
        search(&limits, TOKENS, beam, VOCABULARY, |rows| {
            let mut out = Vec::new();
            for (sentence, prefix) in rows {
                let position = prefix.len() - 1;
                let wanted = scripts[*sentence].get(position).copied().unwrap_or(TOKENS.eos);
                out.extend((0..VOCABULARY as i64).map(|t| if t == wanted { 5.0 } else { 0.0 }));
            }
            Ok(out)
        })
        .unwrap()
    }

    #[test]
    fn sentences_of_different_lengths_end_separately() {
        let found = run(&[&[0, 1, 2, 3], &[2, 3], &[3]], 3, 20);
        assert_eq!(found, [vec![0, 1, 2], vec![2], vec![]]);
    }

    #[test]
    fn greedy_and_beam_agree_on_a_clear_winner() {
        assert_eq!(run(&[&[1, 0, 3]], 1, 20), run(&[&[1, 0, 3]], 4, 20));
    }

    #[test]
    fn a_model_that_never_ends_stops_at_the_limit() {
        let endless: Vec<i64> = vec![1; 100];
        let found = run(&[&endless], 2, 6);
        assert_eq!(found[0].len(), 6);
    }

    #[test]
    fn the_start_token_is_never_produced() {
        // The start token has the highest logit, but must be skipped.
        let found = search(&[3], TOKENS, 2, VOCABULARY, |rows| {
            Ok(rows.iter().flat_map(|_| [1.0, 0.0, 0.0, 2.0, 9.0]).collect())
        })
        .unwrap();
        assert!(!found[0].contains(&TOKENS.start));
    }

    #[test]
    fn beam_search_can_recover_from_a_worse_first_token() {
        // Token 0 starts ahead, but only token 1 leads to a confident ending.
        let found = search(&[5], TOKENS, 2, VOCABULARY, |rows| {
            let mut out = Vec::new();
            for (_, prefix) in rows {
                let line: [f32; VOCABULARY] = match prefix {
                    [_] => [2.0, 1.5, -9.0, -9.0, 0.0],
                    [_, 0] => [0.0, 0.0, 0.0, 0.0, 0.0],
                    [_, 1] => [-9.0, -9.0, -9.0, 9.0, 0.0],
                    _ => [-9.0, -9.0, -9.0, 9.0, 0.0],
                };
                out.extend(line);
            }
            Ok(out)
        })
        .unwrap();
        assert_eq!(found[0], [1]);
    }

    #[test]
    fn log_sum_matches_the_formula() {
        let x = [1.0f32, 2.0, 100.0];
        let expected = (1.0f32.exp() + 2.0f32.exp()).ln();
        assert!((log_sum(&x, 2) - expected).abs() < 1e-4);
    }

    #[test]
    fn top_keeps_the_highest_in_order() {
        assert_eq!(top(&[1.0, 5.0, 3.0, 9.0, 2.0], 2, 3), [(1, 5.0), (2, 3.0)]);
    }

    /// Runs the real model on the card, when it and the runtime are on this machine.
    #[test]
    fn translates_on_the_graphics_card() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let dir = root.join("../data/models/pt-BR-big");
        let runtime = root.join("vendor/directml");
        if !has_graphs(&dir.join("model")) || !has_runtime(&runtime) {
            return;
        }
        let adapter = crate::gpu::best_adapter().expect("a graphics card");
        let mut model = Model::load(&dir.join("model"), &runtime, adapter.index, 2).unwrap();
        let spm =
            ct2rs::tokenizers::sentencepiece::Tokenizer::from_file(dir.join("source.spm"), dir.join("target.spm"))
                .unwrap();
        use ct2rs::Tokenizer;
        let encode = |t: &str| {
            let mut pieces = spm.encode(t).unwrap();
            pieces.insert(0, ">>pob<<".to_owned());
            pieces
        };
        let out = model
            .translate(&[encode("Hello."), encode("Open the door, my friend.")], 4)
            .unwrap();
        let first = spm.decode(out[0].clone()).unwrap();
        let second = spm.decode(out[1].clone()).unwrap();
        assert!(first.contains("Olá"), "{first}");
        assert!(second.contains("porta"), "{second}");
    }
}
