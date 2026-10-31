//! GW2 Translated's local translator: from English to the model's language.
//!
//! CTranslate2 (the official 4.8.4 DLL, via `ct2rs`; see `build.rs` and
//! `scripts/fetch-ctranslate2.ps1`) with OPUS-MT models (CC-BY 4.0), or ONNX
//! Runtime for DirectML, which CTranslate2 does not have (see `dml.rs`). Runs
//! outside the game; the DLL talks to it over HTTP on 127.0.0.1, using the
//! LibreTranslate API format.
//!
//! ```text
//! gw2translated-translator serve --model <small model> [--model-big <big model>]
//!     [--device auto|cpu|cuda|directml] [--threads N] [--gpu-runtime <cuBLAS folder>]
//!     --port-file <file> [--parent-pid N]
//! gw2translated-translator bench --model <small model> [--model-big <dir>] [--device ...]
//!     [--threads N] [--beam B] [--split-sentences on|off] [--out <file>] <dictionary.jsonl> [N]
//! ```
//!
//! Devices:
//! - `cpu`: the small model (Argos `translate-en_pb`, int8), the request's beam (default 2).
//! - `cuda`: NVIDIA GPU. Uses the extra speed for quality: the big model
//!   (`--model-big`, opus-mt-tc-big-en-pt, float16) when present, beam 4 for every
//!   request (still sentence by sentence: whole stretches measured worse). Needs
//!   `cublas64_12.dll` + `cublasLt64_12.dll` (in `--gpu-runtime` or on the search path).
//! - `directml`: any graphics card. The same big model and beam, through
//!   `encoder.onnx` + `decoder.onnx` (next to its `model.bin`, whose weights they
//!   use) and `directml\onnxruntime.dll` + `DirectML.dll` next to this program.
//! - `auto`: CUDA when an NVIDIA GPU has ≥ 1.5 GB free and cuBLAS loads, else
//!   DirectML when a card has that free and the big model is there, else CPU.
//!
//! Any GPU failure (at start or later) falls back to the CPU small model, in process.
//!
//! The port file is written atomically: line 1 port, line 2 device (`cpu`|`cuda`|`directml`),
//! line 3 model (`small`|`big`), line 4 compute type. `GET /info` returns the same as JSON.
//!
//! Model folders: the one extracted from the `.argosmodel` (`model/` +
//! `sentencepiece.model`), or `model/` + `source.spm` + `target.spm` (+ optional
//! `gw2translated.json` with `{"source_prefix": ">>pob<<"}`, the target-language
//! token of multilingual models), as written by `scripts/convert-big-model.ps1`.

mod dml;
mod gpu;
mod segment;

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use ct2rs::sys::{ComputeType, Device};
use ct2rs::tokenizers::sentencepiece::Tokenizer as Spm;
use ct2rs::{Config, TranslationOptions, Translator};
use serde_json::{Value, json};

/// Text pieces translated per model call.
const BATCH: usize = 32;

/// Beam size on CPU when the request does not set one (`"beam"`, an extension of the
/// LibreTranslate API).
const DEFAULT_BEAM: usize = 2;

/// Beam size on the GPU, for every request.
const GPU_BEAM: usize = 4;

/// Text pieces per call on DirectML: each one costs a beam of decoder rows on
/// every step, on cards that may have little memory to spare.
const DIRECTML_BATCH: usize = 8;

/// Free GPU memory needed for `auto` to pick a GPU.
const MIN_FREE_MB: u64 = 1536;

/// More CPU threads than this do not make a single request faster.
const MAX_THREADS: usize = 4;

/// SentencePiece, plus the target-language token some models need first.
struct Tok {
    spm: Spm,
    prefix: Option<String>,
}

impl ct2rs::Tokenizer for Tok {
    fn encode(&self, input: &str) -> anyhow::Result<Vec<String>> {
        let mut t = self.spm.encode(input)?;
        if let Some(p) = &self.prefix {
            t.insert(0, p.clone());
        }
        Ok(t)
    }

    fn decode(&self, tokens: Vec<String>) -> anyhow::Result<String> {
        self.spm.decode(tokens)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dev {
    Cpu,
    Cuda,
    DirectMl,
}

enum Backend {
    Ct2(Translator<Tok>),
    DirectMl { model: RefCell<dml::Model>, tok: Tok },
}

fn tokenizer(dir: &Path) -> anyhow::Result<Tok> {
    let (src, tgt) = if dir.join("source.spm").is_file() {
        (dir.join("source.spm"), dir.join("target.spm"))
    } else {
        (dir.join("sentencepiece.model"), dir.join("sentencepiece.model"))
    };
    let prefix = std::fs::read_to_string(dir.join("gw2translated.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["source_prefix"].as_str().map(str::to_owned));
    Ok(Tok {
        spm: Spm::from_file(&src, &tgt)?,
        prefix,
    })
}

struct Engine {
    tr: Backend,
    device: Dev,
    /// `small` or `big`.
    model: &'static str,
    compute: &'static str,
    threads: usize,
    /// Every request uses this beam (GPU); `None`: the request's.
    beam: Option<usize>,
    split_sentences: bool,
}

impl Engine {
    fn load(dir: &Path, model: &'static str, device: Dev, threads: usize) -> anyhow::Result<Self> {
        let tokenizer = tokenizer(dir)?;
        let (dev, compute, label) = match device {
            Dev::Cuda => (Device::CUDA, ComputeType::FLOAT16, "float16"),
            _ => (Device::CPU, ComputeType::DEFAULT, "default"),
        };
        let config = Config {
            device: dev,
            compute_type: compute,
            num_threads_per_replica: threads,
            ..Default::default()
        };
        let tr = Translator::with_tokenizer(dir.join("model"), tokenizer, &config)?;
        let gpu = device == Dev::Cuda;
        let e = Self {
            tr: Backend::Ct2(tr),
            device,
            model,
            compute: if gpu { label } else { cpu_compute(dir) },
            threads,
            beam: gpu.then_some(GPU_BEAM),
            // Measured: whole stretches lose ~1.2 chrF on both models (they drop sentences).
            split_sentences: true,
        };
        // cuBLAS is loaded (and the GPU really used) on the first call: fail here, not later.
        e.translate(&["Hello.".to_owned()], 1)?;
        Ok(e)
    }

    /// The big model on any graphics card.
    fn load_directml(dir: &Path, runtime: &Path, adapter: u32, threads: usize) -> anyhow::Result<Self> {
        let tok = tokenizer(dir)?;
        let model = dml::Model::load(&dir.join("model"), runtime, adapter, threads)?;
        let e = Self {
            tr: Backend::DirectMl {
                model: RefCell::new(model),
                tok,
            },
            device: Dev::DirectMl,
            model: "big",
            compute: "float16",
            threads,
            beam: Some(GPU_BEAM),
            split_sentences: true,
        };
        e.translate(&["Hello.".to_owned()], 1)?;
        Ok(e)
    }

    /// Translates several texts at once, preserving the game's markup.
    /// `beam`: the request's (CPU); 1 (greedy) is a bit faster.
    fn translate(&self, texts: &[String], beam: usize) -> anyhow::Result<Vec<String>> {
        let split = self.split_sentences;
        // Gathers the text pieces of all texts into a single batch.
        let mut cores: Vec<String> = Vec::new();
        for t in texts {
            segment::translate_with_opts(t, split, |xs| {
                cores.extend(xs.iter().map(|x| x.to_string()));
                Vec::new()
            });
        }
        let beam = self.beam.unwrap_or(beam).clamp(1, GPU_BEAM);
        let mut done = Vec::with_capacity(cores.len());
        let size = if self.device == Dev::DirectMl {
            DIRECTML_BATCH
        } else {
            BATCH
        };
        for chunk in cores.chunks(size) {
            match &self.tr {
                Backend::Ct2(tr) => {
                    // Caps the output length: on some inputs the model repeats itself up to the limit.
                    let longest = chunk.iter().map(|c| c.chars().count()).max().unwrap_or(0);
                    let options: TranslationOptions<String, String> = TranslationOptions {
                        beam_size: beam,
                        max_decoding_length: (longest + 16).min(512),
                        ..Default::default()
                    };
                    done.extend(
                        tr.translate_batch(chunk, &options, None)?
                            .into_iter()
                            .map(|(s, _)| clean(&s)),
                    );
                }
                Backend::DirectMl { model, tok } => {
                    use ct2rs::Tokenizer;
                    let sources = chunk
                        .iter()
                        .map(|c| tok.encode(c))
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    for tokens in model.borrow_mut().translate(&sources, beam)? {
                        done.push(clean(&tok.decode(tokens)?));
                    }
                }
            }
        }
        let mut it = done.into_iter();
        Ok(texts
            .iter()
            .map(|t| segment::translate_with_opts(t, split, |xs| it.by_ref().take(xs.len()).collect()))
            .collect())
    }

    fn info(&self) -> Value {
        json!({
            "device": match self.device { Dev::Cpu => "cpu", Dev::Cuda => "cuda", Dev::DirectMl => "directml" },
            "model": self.model,
            "compute": self.compute,
            "threads": self.threads,
        })
    }
}

/// The compute type CTranslate2 picks on CPU for the model's stored weights.
fn cpu_compute(dir: &Path) -> &'static str {
    // The Argos model is stored in int8; CTranslate2 runs it as int8 (int8_float32).
    if dir.join("sentencepiece.model").is_file() {
        "int8_float32"
    } else {
        "default"
    }
}

/// The decoder joins the SentencePiece pieces without replacing the
/// word-start marker (`▁`) with a space.
fn clean(s: &str) -> String {
    s.replace('\u{2581}', " ")
        .split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

struct Opts {
    small: PathBuf,
    big: Option<PathBuf>,
    device: String,
    threads: usize,
}

/// The big model on CUDA, if its files are there; else the small one.
fn load_cuda(o: &Opts) -> anyhow::Result<Engine> {
    match o.big.as_ref().filter(|d| d.join("model").join("model.bin").is_file()) {
        Some(dir) => Engine::load(dir, "big", Dev::Cuda, o.threads),
        None => Engine::load(&o.small, "small", Dev::Cuda, o.threads),
    }
}

/// DirectML needs the big model: its graphs read the weights of that `model.bin`.
/// `need_room`: only on a card with enough free memory (`auto`).
fn load_directml(o: &Opts, need_room: bool) -> anyhow::Result<Engine> {
    let dir = o
        .big
        .as_ref()
        .filter(|d| dml::has_graphs(&d.join("model")))
        .context("the big model is not installed")?;
    let exe = std::env::current_exe()?;
    let runtime = exe.parent().context("no folder")?.join(dml::RUNTIME_DIR);
    anyhow::ensure!(dml::has_runtime(&runtime), "no runtime in {}", runtime.display());
    let card = gpu::best_adapter().context("no graphics card found")?;
    eprintln!(
        "DirectML: {} — {} MB free of {} MB ({})",
        card.name, card.free_mb, card.total_mb, card.source
    );
    anyhow::ensure!(!need_room || card.free_mb >= MIN_FREE_MB, "not enough free memory");
    Engine::load_directml(dir, &runtime, card.index, o.threads)
}

/// Picks and loads the engine; never fails over to nothing: CPU is the last resort.
fn start(o: &Opts) -> anyhow::Result<Engine> {
    let (cuda, directml) = match o.device.as_str() {
        "cpu" => (false, None),
        "cuda" => (true, None),
        "directml" => (false, Some(false)),
        "auto" => {
            let gpu = gpu::best_nvidia();
            match &gpu {
                Some(g) => eprintln!(
                    "GPU: {} — {} MB free of {} MB ({})",
                    g.name, g.free_mb, g.total_mb, g.source
                ),
                None => eprintln!("GPU: no NVIDIA GPU found"),
            }
            (
                gpu.is_some_and(|g| g.free_mb >= MIN_FREE_MB) && gpu::cublas_available(),
                Some(true),
            )
        }
        other => anyhow::bail!("unknown --device {other} (auto|cpu|cuda|directml)"),
    };
    if cuda {
        match load_cuda(o) {
            Ok(e) => return Ok(e),
            Err(e) => eprintln!("CUDA unavailable ({e})"),
        }
    }
    if let Some(need_room) = directml {
        match load_directml(o, need_room) {
            Ok(e) => return Ok(e),
            Err(e) => eprintln!("DirectML unavailable ({e})"),
        }
    }
    eprintln!("using the CPU");
    Engine::load(&o.small, "small", Dev::Cpu, o.threads)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let small = PathBuf::from(arg(&args, "--model").ok_or_else(|| anyhow::anyhow!("missing --model"))?);
    let threads = arg(&args, "--threads")
        .and_then(|s| s.parse().ok())
        .unwrap_or(2usize)
        .clamp(1, MAX_THREADS);
    if let Some(dir) = arg(&args, "--gpu-runtime")
        && !gpu::add_dll_dir(Path::new(&dir))
    {
        eprintln!("could not add {dir} to the DLL search path");
    }
    let opts = Opts {
        small,
        big: arg(&args, "--model-big").map(PathBuf::from),
        device: arg(&args, "--device").unwrap_or_else(|| "auto".into()),
        threads,
    };
    let t = Instant::now();
    let mut engine = start(&opts)?;
    // Testing knob: sentence splitting on/off (default on: whole stretches measured worse).
    if let Some(v) = arg(&args, "--split-sentences") {
        engine.split_sentences = v == "on";
    }
    eprintln!("model loaded in {:.2}s: {}", t.elapsed().as_secs_f64(), engine.info());
    match args.first().map(String::as_str) {
        Some("bench") => bench(&engine, &args),
        Some("serve") => serve(engine, &opts, &args),
        _ => anyhow::bail!("usage: gw2translated-translator serve|bench --model <folder> ..."),
    }
}

fn bench(engine: &Engine, args: &[String]) -> anyhow::Result<()> {
    let positional: Vec<&String> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .filter(|a| {
            !args
                .iter()
                .any(|b| b.starts_with("--") && arg(args, b).as_ref() == Some(a))
        })
        .collect();
    let path = positional
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing the JSONL file"))?;
    let n: usize = positional.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let beam = arg(args, "--beam").and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_BEAM);
    // Any JSON Lines file whose objects have an `en` field.
    let texts: Vec<String> = std::fs::read_to_string(path.as_str())?
        .lines()
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l).ok()?["en"]
                .as_str()
                .map(str::to_owned)
        })
        .take(n)
        .collect();
    // One request per text, as the game sends them.
    let t = Instant::now();
    let mut out = Vec::with_capacity(texts.len());
    for text in &texts {
        out.extend(engine.translate(std::slice::from_ref(text), beam)?);
    }
    let secs = t.elapsed().as_secs_f64();
    if let Some(file) = arg(args, "--out") {
        std::fs::write(
            file,
            out.iter()
                .map(|s| Value::String(s.clone()).to_string() + "\n")
                .collect::<String>(),
        )?;
    } else {
        for (en, pt) in texts.iter().zip(&out).take(25) {
            println!("EN {en:?}\nPT {pt:?}\n");
        }
    }
    println!(
        "{} texts in {secs:.2}s ({:.1} ms/text) {}",
        texts.len(),
        1000.0 * secs / texts.len().max(1) as f64,
        engine.info()
    );
    Ok(())
}

/// Exits this process when the game ends.
fn watch_parent(pid: u32) {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    std::thread::spawn(move || unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() || WaitForSingleObject(h, INFINITE) == WAIT_OBJECT_0 {
            std::process::exit(0);
        }
    });
}

/// Writes the port file (to a temporary file, then renamed: readers never see half of it).
fn write_status(path: &str, port: u16, e: &Engine) -> anyhow::Result<()> {
    let info = e.info();
    let body = format!(
        "{port}\n{}\n{}\n{}\n",
        info["device"].as_str().unwrap_or(""),
        e.model,
        e.compute
    );
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn serve(mut engine: Engine, opts: &Opts, args: &[String]) -> anyhow::Result<()> {
    if let Some(pid) = arg(args, "--parent-pid").and_then(|s| s.parse().ok()) {
        watch_parent(pid);
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let port_file = arg(args, "--port-file");
    if let Some(p) = &port_file {
        write_status(p, port, &engine)?;
    }
    eprintln!("listening on 127.0.0.1:{port}");
    for stream in listener.incoming().flatten() {
        let device = engine.device;
        if let Err(e) = handle(&mut engine, opts, stream) {
            eprintln!("request failed: {e}");
        }
        if device != engine.device
            && let Some(p) = &port_file
        {
            write_status(p, port, &engine)?;
        }
    }
    Ok(())
}

/// Translates; when the GPU fails, switches to the CPU small model and retries.
fn translate(engine: &mut Engine, opts: &Opts, texts: &[String], beam: usize) -> anyhow::Result<Vec<String>> {
    match engine.translate(texts, beam) {
        Err(e) if engine.device != Dev::Cpu => {
            eprintln!("{:?} failed ({e}); switching to the CPU", engine.device);
            *engine = Engine::load(&opts.small, "small", Dev::Cpu, opts.threads)?;
            engine.translate(texts, beam)
        }
        r => r,
    }
}

fn respond(mut s: TcpStream, status: &str, body: &Value) -> anyhow::Result<()> {
    let b = body.to_string();
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    )?;
    Ok(())
}

/// `POST /translate {"q": "..." | ["..."], "source": "en", "target": "pb", "beam": 1}`
/// `GET /info` → `{"device", "model", "compute", "threads"}`
fn handle(engine: &mut Engine, opts: &Opts, stream: TcpStream) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    if line.starts_with("GET /info ") {
        return respond(stream, "200 OK", &engine.info());
    }
    if !line.starts_with("POST /translate ") {
        return respond(
            stream,
            "404 Not Found",
            &json!({"error": "use POST /translate or GET /info"}),
        );
    }
    let mut body = vec![0u8; len.min(4 << 20)];
    reader.read_exact(&mut body)?;
    let req: Value = serde_json::from_slice(&body)?;
    let (texts, many) = match &req["q"] {
        Value::String(s) => (vec![s.clone()], false),
        Value::Array(a) => (a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(), true),
        _ => return respond(stream, "400 Bad Request", &json!({"error": "missing q"})),
    };
    let beam = req["beam"].as_u64().map_or(DEFAULT_BEAM, |b| b as usize);
    match translate(engine, opts, &texts, beam) {
        Ok(out) if many => respond(stream, "200 OK", &json!({ "translatedText": out })),
        Ok(out) => respond(
            stream,
            "200 OK",
            &json!({ "translatedText": out.into_iter().next().unwrap_or_default() }),
        ),
        Err(e) => respond(stream, "500 Internal Server Error", &json!({"error": e.to_string()})),
    }
}
