//! `refs`: every file reference in every PackFile, found by walking the
//! chunk layouts generically (FileName / FileName2 fields), plus the
//! content files (`cntc`), whose objects reference files through
//! `fileIndices` fixups into the root content file's `fileRefs`
//! (GW2Viewer Data/Content/Manager.ixx).

use crate::dat::Dat;
use crate::models::load_layout;
use crate::pf::*;
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Instant;

const TEX_MAGICS: [&[u8; 4]; 6] = [b"ATEX", b"ATTX", b"ATEC", b"ATEP", b"ATEU", b"ATET"];
const MAX_VISITS: u64 = 20_000_000;

fn fcc_str(b: &[u8]) -> String {
    b.iter().take_while(|&&c| c != 0).map(|&c| if c.is_ascii_graphic() { c as char } else { '.' }).collect()
}

/// Per-type flag: can a FileName be reached from this type at all?
fn ref_types(lay: &Layout) -> Vec<bool> {
    let n = lay.types.len();
    let mut has = vec![false; n];
    for (t, ty) in lay.types.iter().enumerate() {
        if ty.fields.iter().any(|f| f.ut == T_FILENAME || f.ut == T_FILENAME2) {
            has[t] = true;
        }
    }
    loop {
        let mut changed = false;
        for t in 0..n {
            if has[t] {
                continue;
            }
            let reach = lay.types[t].fields.iter().any(|f| {
                f.elem.is_some_and(|e| has[e]) || f.variants.iter().flatten().any(|&v| has[v])
            });
            if reach {
                has[t] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    has
}

struct Walker<'a> {
    r: Reader<'a>,
    has: &'a [bool],
    visited: HashSet<(usize, usize)>,
    visits: u64,
    truncated: bool,
    out: Vec<(String, u32)>,
}

impl Walker<'_> {
    fn walk(&mut self, n: Node, path: &str, depth: u32) {
        if depth > 48 || self.truncated {
            return;
        }
        self.visits += 1;
        if self.visits > MAX_VISITS {
            self.truncated = true;
            return;
        }
        let lay = self.r.lay;
        let ty = &lay.types[n.ty];
        let mut off = n.off;
        for f in &ty.fields {
            let fo = off;
            off += lay.field_size(f, self.r.x64) as usize;
            let reach = |e: Option<usize>| e.is_some_and(|e| self.has[e]);
            let sub = || {
                if f.name.is_empty() {
                    path.to_string()
                } else if path.is_empty() {
                    f.name.clone()
                } else {
                    format!("{path}.{}", f.name)
                }
            };
            match f.ut {
                T_FILENAME | T_FILENAME2 => {
                    if let Ok(Some(p)) = self.r.ptr(fo) {
                        if let (Ok(a), Ok(b), Ok(c)) = (self.r.u16(p), self.r.u16(p + 2), self.r.u16(p + 4)) {
                            let id = file_ref_id(a, b, c);
                            if id != 0 {
                                self.out.push((sub(), id));
                            }
                        }
                    }
                }
                T_INLINE_STRUCT | T_INLINE_STRUCT2 if reach(f.elem) => {
                    self.walk(Node { off: fo, ty: f.elem.unwrap() }, &sub(), depth + 1);
                }
                T_PTR if reach(f.elem) => {
                    if let Ok(Some(t)) = self.r.ptr(fo) {
                        let ty = f.elem.unwrap();
                        if self.visited.insert((t, ty)) {
                            self.walk(Node { off: t, ty }, &sub(), depth + 1);
                        }
                    }
                }
                T_VARIANT => {
                    let (Ok(idx), Ok(Some(t))) = (self.r.u32(fo), self.r.ptr(fo + 4)) else { continue };
                    if let Some(Some(ty)) = f.variants.get(idx as usize) {
                        if self.has[*ty] && self.visited.insert((t, *ty)) {
                            self.walk(Node { off: t, ty: *ty }, &sub(), depth + 1);
                        }
                    }
                }
                T_INLINE_ARRAY | T_DWORD_ARRAY | T_DWORD_PTR_ARRAY | T_WORD_ARRAY | T_WORD_PTR_ARRAY | T_BYTE_ARRAY
                | T_BYTE_PTR_ARRAY
                    if reach(f.elem) =>
                {
                    // Element type whose only field is a FileName: an array of file names.
                    if let Ok(elems) = self.r.array(n, &f.name) {
                        // GW2TEX_ARRAY_INDEX=1: keep the element index (e.g. raceSexData[7]), used to
                        // tell races apart; default `[]` keeps refs.jsonl paths stable.
                        let with_index = std::env::var_os("GW2TEX_ARRAY_INDEX").is_some();
                        for (i, e) in elems.into_iter().enumerate() {
                            let Some(e) = e else { continue };
                            let p = if with_index { format!("{}[{i}]", sub()) } else { format!("{}[]", sub()) };
                            if self.truncated {
                                break;
                            }
                            if self.visited.insert((e.off, e.ty)) {
                                self.walk(e, &p, depth + 1);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Chunk footer with an embedded layout (Manager::LoadEmbeddedLayout).
fn embedded_layout(buf: &[u8], c: &Chunk) -> bool {
    if c.end < c.data + 16 {
        return false;
    }
    let f = &buf[c.end - 16..c.end];
    if &f[12..16] != b"FOOT" || u16::from_le_bytes([f[8], f[9]]) != 0 || u16::from_le_bytes([f[10], f[11]]) != 16 {
        return false;
    }
    let lo = u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize;
    if lo == 0 {
        return false;
    }
    let p = c.data + lo + 4;
    p + 2 <= c.end && u16::from_le_bytes([buf[p], buf[p + 1]]) != T_STRUCT
}

#[derive(Default)]
struct FileResult {
    refs: Vec<(String, String, u32)>, // chunk, path, target id
    no_layout: Vec<String>,
    embedded: bool,
    truncated: bool,
    encrypted: bool,
}

fn walk_file(buf: &[u8], lay: &Layout, has: &[bool], content: Option<&Content>) -> Result<FileResult> {
    let pf = PackFile::parse(buf)?;
    let mut res = FileResult::default();
    if pf.chunks.iter().any(|c| embedded_layout(buf, c)) {
        res.embedded = true;
        return Ok(res);
    }
    if &pf.fcc == b"cntc" {
        let c = content.ok_or_else(|| anyhow!("root content file not loaded"))?;
        return content_refs(&pf, c, res);
    }
    for c in &pf.chunks {
        let key = if &c.fcc == b"Main" { pf.fcc } else { c.fcc };
        let prefix = if &pf.fcc == b"MODL" { "ModelFile" } else { "" };
        let Some(ty) = lay.chunk_type(&key, c.version, prefix) else {
            res.no_layout.push(format!("{}v{}", fcc_str(&key), c.version));
            continue;
        };
        if !has[ty] {
            continue;
        }
        let mut w = Walker { r: Reader { buf, x64: pf.x64, lay }, has, visited: HashSet::new(), visits: 0, truncated: false, out: Vec::new() };
        w.walk(Node { off: c.data, ty }, "", 0);
        res.truncated |= w.truncated;
        let cn = fcc_str(&c.fcc);
        let mut seen = HashSet::new();
        for (p, id) in w.out {
            if seen.insert((p.clone(), id)) {
                res.refs.push((cn.clone(), p, id));
            }
        }
    }
    Ok(res)
}

// ------------------------------------------------------------ content

/// Shared data from the root content file (file id 1282830).
pub struct Content {
    file_refs: Vec<u32>,
    /// type index -> nameOffset (-1 = none)
    name_offsets: Vec<i32>,
    /// namespace full paths (root file only)
    namespaces: Vec<String>,
}

const P: usize = 8;

fn arr(buf: &[u8], off: usize) -> Result<(usize, usize)> {
    let r = Reader { buf, x64: true, lay: &EMPTY };
    let n = r.u32(off)? as usize;
    let p = r.ptr(off + 4)?.unwrap_or(0);
    Ok((n, p))
}
static EMPTY: std::sync::LazyLock<Layout> = std::sync::LazyLock::new(Layout::default);

fn rd() -> fn(&[u8]) -> Reader<'_> {
    |b| Reader { buf: b, x64: true, lay: &EMPTY }
}

fn wstr(buf: &[u8], p: usize) -> String {
    let r = rd()(buf);
    let mut v = Vec::new();
    let mut q = p;
    while v.len() < 256 {
        match r.u16(q) {
            Ok(0) | Err(_) => break,
            Ok(c) => v.push(c),
        }
        q += 2;
    }
    String::from_utf16_lossy(&v)
}

// PackContent field offsets (packed, x64).
const O_FLAGS: usize = 0;
const O_TYPES: usize = 4;
const O_NS: usize = 16;
const O_FILEREFS: usize = 28;
const O_ENTRIES: usize = 40;
const O_FILEIDX: usize = 76;
const O_STRIDX: usize = 88;
const O_STRINGS: usize = 112;
const O_CONTENT: usize = 124;

fn main_chunk(pf: &PackFile) -> Result<usize> {
    let c = pf.chunk(b"Main").ok_or_else(|| anyhow!("content file without Main chunk"))?;
    if !pf.x64 {
        bail!("32-bit content file");
    }
    Ok(c.data)
}

impl Content {
    pub fn load(buf: &[u8]) -> Result<Content> {
        let pf = PackFile::parse(buf)?;
        let base = main_chunk(&pf)?;
        let r = rd()(buf);
        let (nf, pfr) = arr(buf, base + O_FILEREFS)?;
        let mut file_refs = Vec::with_capacity(nf);
        for i in 0..nf {
            let id = match r.ptr(pfr + i * P)? {
                Some(p) => file_ref_id(r.u16(p)?, r.u16(p + 2)?, r.u16(p + 4)?),
                None => 0,
            };
            file_refs.push(id);
        }
        let (nt, pt) = arr(buf, base + O_TYPES)?;
        let mut name_offsets = Vec::with_capacity(nt);
        for i in 0..nt {
            name_offsets.push(r.u32(pt + i * 17 + 12)? as i32);
        }
        let (nn, pn) = arr(buf, base + O_NS)?;
        let mut names = Vec::with_capacity(nn);
        let mut parents = Vec::with_capacity(nn);
        for i in 0..nn {
            let e = pn + i * 16;
            names.push(r.ptr(e)?.map(|p| wstr(buf, p)).unwrap_or_default());
            parents.push(r.u32(e + 12)? as i32);
        }
        let mut namespaces = Vec::with_capacity(nn);
        for i in 0..nn {
            let mut parts = vec![names[i].clone()];
            let mut p = parents[i];
            let mut guard = 0;
            while p >= 0 && (p as usize) < nn && guard < 64 {
                parts.push(names[p as usize].clone());
                p = parents[p as usize];
                guard += 1;
            }
            parts.reverse();
            namespaces.push(parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/"));
        }
        if file_refs.is_empty() || name_offsets.is_empty() {
            bail!("not the root content file ({} fileRefs, {} types)", file_refs.len(), name_offsets.len());
        }
        Ok(Content { file_refs, name_offsets, namespaces })
    }
}

/// field_path for content refs: `type<T>/<namespace>/<object name>+0x<offset in object>`.
fn content_refs(pf: &PackFile, c: &Content, mut res: FileResult) -> Result<FileResult> {
    let buf = pf.buf;
    let base = main_chunk(pf)?;
    let r = rd()(buf);
    if r.u32(base + O_FLAGS)? & 1 != 0 {
        res.encrypted = true;
        return Ok(res);
    }
    let (ndata, pdata) = arr(buf, base + O_CONTENT)?;
    let data = buf.get(pdata..pdata + ndata).ok_or_else(|| anyhow!("content out of bounds"))?;
    let (ne, pe) = arr(buf, base + O_ENTRIES)?;
    let mut entries: Vec<(u32, u32, u32, i32)> = Vec::with_capacity(ne); // offset, type, ns, root
    for i in 0..ne {
        let e = pe + i * 16;
        entries.push((r.u32(e + 4)?, r.u32(e)?, r.u32(e + 8)?, r.u32(e + 12)? as i32));
    }
    let by_index = entries.clone();
    let mut sorted: Vec<usize> = (0..ne).collect();
    sorted.sort_by_key(|&i| entries[i].0);
    let (ns_, ps) = arr(buf, base + O_STRINGS)?;
    let (nsi, psi) = arr(buf, base + O_STRIDX)?;
    let mut str_slots: HashMap<usize, String> = HashMap::new();
    for i in 0..nsi {
        let reloc = r.u32(psi + i * 4)? as usize;
        if reloc + 8 <= data.len() {
            let idx = u64::from_le_bytes(data[reloc..reloc + 8].try_into().unwrap()) as usize;
            if idx < ns_ {
                if let Ok(Some(p)) = r.ptr(ps + idx * P) {
                    str_slots.insert(reloc, wstr(buf, p));
                }
            }
        }
    }
    let obj_name = |ei: usize| -> String {
        let (off, ty, _, _) = by_index[ei];
        let no = c.name_offsets.get(ty as usize).copied().unwrap_or(-1);
        if no < 0 {
            return String::new();
        }
        str_slots.get(&(off as usize + no as usize)).cloned().unwrap_or_default()
    };
    let (nfi, pfi) = arr(buf, base + O_FILEIDX)?;
    let mut seen = HashSet::new();
    for i in 0..nfi {
        let reloc = r.u32(pfi + i * 4)? as usize;
        if reloc + 8 > data.len() {
            continue;
        }
        let idx = u64::from_le_bytes(data[reloc..reloc + 8].try_into().unwrap()) as usize;
        let Some(&id) = c.file_refs.get(idx) else { continue };
        if id == 0 {
            continue;
        }
        // Owning object = entry with the greatest offset <= reloc.
        let pos = sorted.partition_point(|&k| entries[k].0 as usize <= reloc);
        let path = if pos == 0 {
            "?".to_string()
        } else {
            let ei = sorted[pos - 1];
            let (off, ty, ns, root) = by_index[ei];
            // Namespace names are mangled in shipped content (flags & 2), so
            // only the namespace index is kept.
            let mut name = obj_name(ei);
            let mut rty = ty;
            let mut obj = ei;
            if root >= 0 && (root as usize) < by_index.len() && root as usize != ei {
                rty = by_index[root as usize].1;
                obj = root as usize;
                if name.is_empty() {
                    name = obj_name(root as usize);
                }
            }
            let clean = |s: &str| s.replace(['\n', '\r', ',', '"', '/'], " ");
            format!("type{ty}/root{rty}/obj{obj}/ns{ns}/{}+0x{:x}", clean(&name), reloc - off as usize)
        };
        if seen.insert((path.clone(), id)) {
            res.refs.push(("Main".into(), path, id));
        }
    }
    Ok(res)
}

// ------------------------------------------------------------ driver

/// mft index -> 4-byte kind ("PF" files: their pack fourcc, prefixed "PF:").
fn mft_kinds(dat_path: &Path, n: usize, threads: usize, cache: &Path) -> Result<Vec<String>> {
    if cache.exists() {
        let mut v = vec![String::new(); n];
        for l in std::fs::read_to_string(cache)?.lines() {
            let Ok(j) = serde_json::from_str::<serde_json::Value>(l) else { continue };
            if let (Some(i), Some(k)) = (j["mft"].as_u64(), j["kind"].as_str()) {
                if (i as usize) < n {
                    v[i as usize] = k.to_string();
                }
            }
        }
        return Ok(v);
    }
    let next = AtomicUsize::new(0);
    let out = Mutex::new(vec![String::new(); n]);
    std::thread::scope(|sc| -> Result<()> {
        let mut hs = Vec::new();
        for _ in 0..threads {
            hs.push(sc.spawn(|| -> Result<()> {
                let mut dat = Dat::open(dat_path)?;
                let mut local = Vec::new();
                loop {
                    let s = next.fetch_add(4096, Ordering::Relaxed);
                    if s >= n {
                        break;
                    }
                    for i in s..(s + 4096).min(n) {
                        if dat.entries[i].offset == 0 {
                            continue;
                        }
                        let Ok(h) = dat.peek(i, 12) else { continue };
                        if h.len() >= 12 && &h[0..2] == b"PF" {
                            local.push((i, format!("PF:{}", fcc_str(&h[8..12]))));
                        } else if h.len() >= 4 {
                            local.push((i, fcc_str(&h[0..4])));
                        }
                    }
                }
                let mut o = out.lock().unwrap();
                for (i, k) in local {
                    o[i] = k;
                }
                Ok(())
            }));
        }
        for h in hs {
            h.join().map_err(|_| anyhow!("scan thread panicked"))??;
        }
        Ok(())
    })?;
    let v = out.into_inner().unwrap();
    let mut s = String::new();
    for (i, k) in v.iter().enumerate() {
        if !k.is_empty() {
            s.push_str(&serde_json::json!({"mft": i, "kind": k}).to_string());
            s.push('\n');
        }
    }
    std::fs::write(cache, s)?;
    Ok(v)
}

pub fn run(dat_path: &Path, exe: &Path, out: &PathBuf, kinds_cache: &Path, threads: usize, only: Vec<u32>) -> Result<()> {
    let t0 = Instant::now();
    let lay = load_layout(exe)?;
    let has = ref_types(&lay);
    let mut dat = Dat::open(dat_path)?;
    let atoc = dat.atoc()?;
    let mut fid_of: HashMap<usize, u32> = HashMap::new();
    for (&id, &mi) in &atoc {
        fid_of.entry(mi).and_modify(|v| *v = (*v).min(id)).or_insert(id);
    }
    let n = dat.entries.len();
    let threads = threads.max(1);
    let kinds = mft_kinds(dat_path, n, threads, kinds_cache)?;
    eprintln!("kinds ready {:.0}s", t0.elapsed().as_secs_f32());
    let is_tex = |k: &str| TEX_MAGICS.iter().any(|m| k.as_bytes() == &m[..]);

    let content = match atoc.get(&1282830).map(|&m| dat.read(m)).transpose()?.map(|b| Content::load(&b)) {
        Some(Ok(c)) => {
            eprintln!("root content: {} fileRefs, {} types, {} namespaces", c.file_refs.len(), c.name_offsets.len(), c.namespaces.len());
            Some(c)
        }
        Some(Err(e)) => {
            eprintln!("root content file failed: {e}");
            None
        }
        None => None,
    };

    let mut list: Vec<usize> = if only.is_empty() {
        (0..n).filter(|&i| kinds[i].starts_with("PF:")).collect()
    } else {
        only.iter().filter_map(|id| atoc.get(id).copied()).collect()
    };
    list.sort_by_key(|&i| dat.entries[i].offset);
    eprintln!("{} pack files", list.len());

    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Result<FileResult, String>)>();
    let mut st_ok = 0u64;
    let mut failed: BTreeMap<String, u64> = BTreeMap::new();
    let mut fail_ex: Vec<String> = Vec::new();
    let mut per_fcc: BTreeMap<String, [u64; 4]> = BTreeMap::new(); // files, ok, rows, embedded
    let mut no_layout: BTreeMap<String, u64> = BTreeMap::new();
    let (mut rows, mut dropped, mut embedded, mut truncated, mut encrypted) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut tex_targets: HashSet<usize> = HashSet::new();
    std::thread::scope(|sc| -> Result<()> {
        for _ in 0..threads {
            let tx = tx.clone();
            let (next, list, lay, has, content) = (&next, &list, &lay, &has[..], content.as_ref());
            sc.spawn(move || {
                let Ok(mut dat) = Dat::open(dat_path) else { return };
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= list.len() {
                        break;
                    }
                    let i = list[k];
                    let res = match dat.read(i) {
                        Ok(buf) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk_file(&buf, lay, has, content))) {
                            Ok(Ok(r)) => Ok(r),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => Err("panic".into()),
                        },
                        Err(e) => Err(format!("read: {e}")),
                    };
                    if tx.send((i, res)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (k, (i, res)) in rx.into_iter().enumerate() {
            if k % 50000 == 0 {
                eprintln!("{k}/{} rows={rows} failed={} {:.0}s", list.len(), failed.values().sum::<u64>(), t0.elapsed().as_secs_f32());
            }
            let sf = kinds[i].trim_start_matches("PF:").to_string();
            let e = per_fcc.entry(sf.clone()).or_default();
            e[0] += 1;
            match res {
                Ok(r) => {
                    st_ok += 1;
                    e[1] += 1;
                    if r.embedded {
                        embedded += 1;
                        e[3] += 1;
                    }
                    truncated += r.truncated as u64;
                    encrypted += r.encrypted as u64;
                    for nl in r.no_layout {
                        *no_layout.entry(format!("{sf}:{nl}")).or_default() += 1;
                    }
                    let sid = fid_of.get(&i).copied().unwrap_or(0);
                    for (chunk, path, tid) in r.refs {
                        let Some(&tm) = atoc.get(&tid) else {
                            dropped += 1;
                            continue;
                        };
                        let tk = &kinds[tm];
                        if !(is_tex(tk) || tk.starts_with("PF:")) {
                            dropped += 1;
                            continue;
                        }
                        if is_tex(tk) {
                            tex_targets.insert(tm);
                        }
                        let rec = serde_json::json!({
                            "src_file_id": sid, "src_mft": i, "src_fourcc": sf, "chunk": chunk,
                            "field_path": path, "target_file_id": tid, "target_mft": tm,
                            "target_kind": tk.trim_start_matches("PF:"),
                        });
                        writeln!(w, "{rec}")?;
                        rows += 1;
                        e[2] += 1;
                    }
                }
                Err(er) => {
                    let key: String = er.split(" at ").next().unwrap_or(&er).chars().take(70).collect();
                    *failed.entry(format!("{sf}: {key}")).or_default() += 1;
                    if fail_ex.len() < 15 {
                        fail_ex.push(format!("mft {i} fid {} {sf}: {er}", fid_of.get(&i).copied().unwrap_or(0)));
                    }
                }
            }
        }
        Ok(())
    })?;
    w.flush()?;
    let n_failed: u64 = failed.values().sum();
    eprintln!("== refs: {} pack files, failed {n_failed}, embedded-layout skipped {embedded}, rows {rows}, distinct texture targets {} ({:.0}s)",
        list.len(), tex_targets.len(), t0.elapsed().as_secs_f32());
    let per: BTreeMap<_, _> = per_fcc
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!({"files": v[0], "ok": v[1], "rows": v[2], "embedded_layout": v[3]})))
        .collect();
    let summary = serde_json::json!({
        "seconds": t0.elapsed().as_secs_f64(),
        "pack_files": list.len(),
        "ok": st_ok,
        "failed": n_failed,
        "failures": failed,
        "failure_examples": fail_ex,
        "embedded_layout_skipped": embedded,
        "encrypted_content_skipped": encrypted,
        "walk_truncated": truncated,
        "rows": rows,
        "dropped_refs": dropped,
        "dropped_rule": "target not in ATOC, or target is neither a texture (ATEX/ATTX/ATEC/ATEP/ATEU/ATET) nor a PF file",
        "distinct_texture_targets": tex_targets.len(),
        "per_source_fourcc": per,
        "chunks_without_layout": no_layout,
    });
    crate::write_summary(out, &summary)?;
    Ok(())
}
