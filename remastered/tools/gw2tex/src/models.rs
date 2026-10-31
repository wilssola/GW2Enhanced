//! `models`: texture references of every MODL pack file.
//!
//! Port of GW2Viewer's UI/Controls/Controls-Model.ixx reading path:
//! MODL.permutations[*].materials[*].textures[*].{token,filename}, the GEOM
//! meshes (materialIndex/meshName/materialName/boneBindings) and SKEL
//! (fileReference, skeletonData.grannyModel). Older MODL versions keep
//! materials/meshes/model directly in the MODL chunk; those are handled too.

use crate::dat::Dat;
use crate::pe::Image;
use crate::pf::{token64, Layout, Node, PackFile, Reader};
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Instant;

pub fn load_layout(exe: &Path) -> Result<Layout> {
    let img = Image::load(exe)?;
    let lay = Layout::from_exe(&img);
    eprintln!("layouts: {} chunk fourccs, {} types", lay.chunks.len(), lay.types.len());
    Ok(lay)
}

pub fn dump_layout(exe: &Path, fccs: &[String]) -> Result<()> {
    let lay = load_layout(exe)?;
    if fccs.is_empty() {
        let mut v: Vec<_> = lay.chunks.iter().map(|(k, m)| (String::from_utf8_lossy(k).into_owned(), m.len())).collect();
        v.sort();
        for (k, n) in v {
            println!("{k} {n}");
        }
        return Ok(());
    }
    for f in fccs {
        let mut k = [0u8; 4];
        for (i, b) in f.bytes().take(4).enumerate() {
            k[i] = b;
        }
        let Some(m) = lay.chunks.get(&k) else {
            println!("{f}: not found");
            continue;
        };
        let mut vers: Vec<_> = m.iter().collect();
        vers.sort();
        for (v, ts) in vers {
            for &t in ts {
                let mut s = String::new();
                lay.dump(t, 1, &mut Vec::new(), &mut s);
                println!("== {f} v{v}: {} (size {})\n{s}", lay.types[t].name, lay.type_size(t, true));
            }
        }
    }
    Ok(())
}

struct Mesh {
    material_index: i64,
    name: String,
    material_name: String,
    bones: usize,
}

struct TexRef {
    perm: usize,
    mat: usize,
    mat_token: String,
    mat_file: u32,
    token: String,
    file: u32,
}

#[derive(Default)]
struct ModelInfo {
    version: u32,
    has_skel: bool,
    skel_ref: u32,
    granny: String,
    skeleton: String,
    bone_count: usize,
    cloth: usize,
    radius: Option<f32>,
    meshes: Vec<Mesh>,
    texs: Vec<TexRef>,
    anim_only: bool,
}

impl ModelInfo {
    /// Every MODL has a SKEL chunk (granny always has a root bone), so
    /// "skinned" = external skeleton, >1 local bone, or >1 bone binding.
    fn skinned(&self) -> bool {
        self.skel_ref != 0 || self.bone_count > 1 || self.meshes.iter().any(|m| m.bones > 1)
    }
}

fn granny_names(r: &Reader, gm: Node, mi: &mut ModelInfo) {
    if mi.granny.is_empty() {
        mi.granny = r.string(gm, "Name").unwrap_or_default();
    }
    if let Ok(Some(sk)) = r.sub(gm, "Skeleton") {
        if mi.skeleton.is_empty() {
            mi.skeleton = r.string(sk, "Name").unwrap_or_default();
        }
        mi.bone_count = mi.bone_count.max(r.count(sk, "Bones").unwrap_or(0));
    }
}

fn parse_model(buf: &[u8], lay: &Layout) -> Result<ModelInfo> {
    let pf = PackFile::parse(buf)?;
    if &pf.fcc != b"MODL" {
        bail!("not a MODL pack file");
    }
    let r = Reader { buf, x64: pf.x64, lay };
    if pf.chunk(b"MODL").is_none() && !pf.chunks.is_empty() && pf.chunks.iter().all(|c| &c.fcc == b"ANIM") {
        // Animation-only MODL pack (no materials -> no texture refs).
        return Ok(ModelInfo { anim_only: true, ..Default::default() });
    }
    let ch = pf.chunk(b"MODL").ok_or_else(|| {
        let names: Vec<String> = pf.chunks.iter().map(|c| String::from_utf8_lossy(&c.fcc).into_owned()).collect();
        anyhow!("no MODL chunk (chunks: {})", names.join(" "))
    })?;
    let ty = lay.chunk_type(b"MODL", ch.version, "ModelFile").ok_or_else(|| anyhow!("no layout for MODL v{}", ch.version))?;
    let root = Node { off: ch.data, ty };
    let mut mi = ModelInfo { version: ch.version, ..Default::default() };

    // Materials: permutations[*].materials[*] (new) or materials[*] (old).
    let mut mats: Vec<(usize, usize, Node)> = Vec::new();
    if r.has(root, "permutations") {
        for (pi, p) in r.array(root, "permutations")?.into_iter().enumerate() {
            let Some(p) = p else { continue };
            for (k, m) in r.array(p, "materials")?.into_iter().enumerate() {
                if let Some(m) = m {
                    mats.push((pi, k, m));
                }
            }
        }
    } else if r.has(root, "materials") {
        for (k, m) in r.array(root, "materials")?.into_iter().enumerate() {
            if let Some(m) = m {
                mats.push((0, k, m));
            }
        }
    } else {
        bail!("MODL v{} has no materials field", ch.version);
    }
    for (pi, k, m) in mats {
        let mat_token = if r.has(m, "token") { token64(r.qword(m, "token")?) } else { String::new() };
        let mat_file = if r.has(m, "filename") { r.file_id(m, "filename")? } else { 0 };
        for t in r.array(m, "textures")?.into_iter().flatten() {
            mi.texs.push(TexRef {
                perm: pi,
                mat: k,
                mat_token: mat_token.clone(),
                mat_file,
                token: token64(r.qword(t, "token")?),
                file: r.file_id(t, "filename")?,
            });
        }
    }

    // Meshes: GEOM chunk (new) or MODL.meshes (old, granny blobs).
    let mesh_nodes: Vec<Node> = if let Some(g) = pf.chunk(b"GEOM") {
        let gt = lay.chunk_type(b"GEOM", g.version, "ModelFile").ok_or_else(|| anyhow!("no layout for GEOM v{}", g.version))?;
        r.array(Node { off: g.data, ty: gt }, "meshes")?.into_iter().flatten().collect()
    } else if r.has(root, "meshes") {
        r.array(root, "meshes")?.into_iter().flatten().collect()
    } else {
        Vec::new()
    };
    for m in mesh_nodes {
        let bones = if r.has(m, "boneBindings") {
            r.count(m, "boneBindings")?
        } else if r.has(m, "boneNames") {
            r.count(m, "boneNames")?
        } else {
            0
        };
        mi.meshes.push(Mesh {
            material_index: if r.has(m, "materialIndex") { r.int(m, "materialIndex")? } else { -1 },
            name: if r.has(m, "meshName") { token64(r.qword(m, "meshName")?) } else { String::new() },
            material_name: if r.has(m, "materialName") { r.string(m, "materialName")? } else { String::new() },
            bones,
        });
    }

    // Skeleton (SKEL chunk, or the inline granny model of old versions).
    if let Some(s) = pf.chunk(b"SKEL") {
        mi.has_skel = true;
        if let Some(st) = lay.chunk_type(b"SKEL", s.version, "ModelFile") {
            let sn = Node { off: s.data, ty: st };
            if r.has(sn, "fileReference") {
                mi.skel_ref = r.file_id(sn, "fileReference").unwrap_or(0);
            }
            if r.has(sn, "skeletonData") {
                if let Ok(Some(sd)) = r.sub(sn, "skeletonData") {
                    if let Ok(Some(gm)) = r.sub(sd, "grannyModel") {
                        granny_names(&r, gm, &mut mi);
                    }
                }
            }
        }
    }
    if r.has(root, "model") {
        if let Ok(Some(gm)) = r.sub(root, "model") {
            granny_names(&r, gm, &mut mi);
            if mi.bone_count > 0 {
                mi.has_skel = true;
            }
        }
    }
    if r.has(root, "clothData") {
        mi.cloth = r.count(root, "clothData").unwrap_or(0);
    }
    if r.has(root, "boundingSphere") {
        if let Ok(Some(b)) = r.sub(root, "boundingSphere") {
            mi.radius = r.float(b, "radius").ok();
        }
    }
    Ok(mi)
}

fn unique<'a>(it: impl Iterator<Item = &'a str>, max: usize) -> Vec<&'a str> {
    let mut v: Vec<&str> = Vec::new();
    for s in it {
        if !s.is_empty() && !v.contains(&s) && v.len() < max {
            v.push(s);
        }
    }
    v
}

fn rows(fid: u32, mi: &ModelInfo, atoc: &HashMap<u32, usize>) -> String {
    use std::fmt::Write as _;
    let bb = mi.meshes.iter().map(|m| m.bones).max().unwrap_or(0);
    let mut out = String::new();
    for t in &mi.texs {
        let ms = mi.meshes.iter().filter(|m| m.material_index == t.mat as i64);
        let mesh_names = unique(ms.clone().map(|m| m.name.as_str()), 16);
        let mat_names = unique(ms.map(|m| m.material_name.as_str()), 16);
        let rec = serde_json::json!({
            "model_file_id": fid,
            "has_skel": mi.has_skel,
            "skel_file_ref": mi.skel_ref,
            "bone_binding_count": bb,
            "material_index": t.mat,
            "token": t.token,
            "texture_file_id": t.file,
            "texture_mft": atoc.get(&t.file),
            "permutation": t.perm,
            "material_token": t.mat_token,
            "material_file_id": t.mat_file,
            "mesh_count": mi.meshes.len(),
            "mesh_names": mesh_names,
            "material_names": mat_names,
            "granny_name": mi.granny,
            "skeleton_name": mi.skeleton,
            "bone_count": mi.bone_count,
            "cloth_count": mi.cloth,
            "bound_radius": mi.radius.filter(|r| r.is_finite()).map(|r| (r as f64 * 100.0).round() / 100.0),
            "modl_version": mi.version,
            "skinned": mi.skinned(),
        });
        let _ = writeln!(out, "{rec}");
    }
    out
}

/// mft indices of every MODL pack file (header scan, parallel).
fn find_models(dat_path: &Path, n_entries: usize, threads: usize) -> Result<Vec<usize>> {
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    std::thread::scope(|sc| -> Result<()> {
        let mut hs = Vec::new();
        for _ in 0..threads {
            hs.push(sc.spawn(|| -> Result<()> {
                let mut dat = Dat::open(dat_path)?;
                let mut local = Vec::new();
                loop {
                    let start = next.fetch_add(4096, Ordering::Relaxed);
                    if start >= n_entries {
                        break;
                    }
                    for i in start..(start + 4096).min(n_entries) {
                        if dat.entries[i].offset == 0 {
                            continue;
                        }
                        if let Ok(h) = dat.peek(i, 12) {
                            if h.len() >= 12 && &h[0..2] == b"PF" && &h[8..12] == b"MODL" {
                                local.push(i);
                            }
                        }
                    }
                }
                found.lock().unwrap().extend(local);
                Ok(())
            }));
        }
        for h in hs {
            h.join().map_err(|_| anyhow!("scan thread panicked"))??;
        }
        Ok(())
    })?;
    let mut v = found.into_inner().unwrap();
    v.sort_unstable();
    Ok(v)
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    dat: &mut Dat,
    dat_path: &Path,
    exe: &Path,
    out: &PathBuf,
    ids: Vec<u32>,
    index: Option<PathBuf>,
    threads: usize,
) -> Result<()> {
    let t0 = Instant::now();
    let lay = load_layout(exe)?;
    let atoc = dat.atoc()?;
    let mut fid_of: HashMap<usize, u32> = HashMap::new();
    for (&id, &mi) in &atoc {
        fid_of.entry(mi).and_modify(|v| *v = (*v).min(id)).or_insert(id);
    }
    let threads = threads.max(1);

    let mut list: Vec<usize> = if !ids.is_empty() {
        ids.iter().filter_map(|id| atoc.get(id).copied()).collect()
    } else if let Some(ix) = index.as_ref().filter(|p| p.exists()) {
        std::fs::read_to_string(ix)?.lines().filter_map(|l| l.trim().parse().ok()).collect()
    } else {
        eprintln!("scanning {} entries for MODL headers ({threads} threads)...", dat.entries.len());
        let v = find_models(dat_path, dat.entries.len(), threads)?;
        if let Some(ix) = &index {
            let s: String = v.iter().map(|i| format!("{i}\n")).collect();
            std::fs::write(ix, s)?;
        }
        v
    };
    list.retain(|&i| i < dat.entries.len());
    list.sort_by_key(|&i| dat.entries[i].offset);
    eprintln!("{} MODL files, {:.0}s", list.len(), t0.elapsed().as_secs_f32());

    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Result<ModelInfo, String>)>();
    let mut parsed = 0u64;
    let mut failed: BTreeMap<String, u64> = BTreeMap::new();
    let mut fail_examples: Vec<String> = Vec::new();
    let mut tex_ids: HashSet<u32> = HashSet::new();
    let mut tex_skinned: HashSet<u32> = HashSet::new();
    let mut tex_rigid: HashSet<u32> = HashSet::new();
    let (mut n_rows, mut n_skel, mut no_tex, mut anim_only, mut n_extref) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut versions: BTreeMap<u32, u64> = BTreeMap::new();
    std::thread::scope(|sc| -> Result<()> {
        for _ in 0..threads {
            let tx = tx.clone();
            let (next, list, lay) = (&next, &list, &lay);
            sc.spawn(move || {
                let mut dat = match Dat::open(dat_path) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = tx.send((usize::MAX, Err(format!("open: {e}"))));
                        return;
                    }
                };
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= list.len() {
                        break;
                    }
                    let i = list[k];
                    let res = match dat.read(i) {
                        Ok(buf) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse_model(&buf, lay))) {
                            Ok(Ok(m)) => Ok(m),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => Err("panic".to_string()),
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
            if k % 20000 == 0 {
                eprintln!("{k}/{} parsed={parsed} failed={} {:.0}s", list.len(), failed.values().sum::<u64>(), t0.elapsed().as_secs_f32());
            }
            match res {
                Ok(mi) if mi.anim_only => anim_only += 1,
                Ok(mi) => {
                    parsed += 1;
                    *versions.entry(mi.version).or_default() += 1;
                    let fid = fid_of.get(&i).copied().unwrap_or(0);
                    let skinned = mi.skinned();
                    n_extref += (mi.skel_ref != 0) as u64;
                    n_skel += skinned as u64;
                    if mi.texs.is_empty() {
                        no_tex += 1;
                    }
                    for t in &mi.texs {
                        if t.file == 0 {
                            continue;
                        }
                        let canon = atoc.get(&t.file).and_then(|m| fid_of.get(m)).copied().unwrap_or(t.file);
                        tex_ids.insert(canon);
                        if skinned {
                            tex_skinned.insert(canon);
                        } else {
                            tex_rigid.insert(canon);
                        }
                    }
                    n_rows += mi.texs.len() as u64;
                    w.write_all(rows(fid, &mi, &atoc).as_bytes())?;
                }
                Err(e) => {
                    let key: String = e.split(" at ").next().unwrap_or(&e).chars().take(80).collect();
                    *failed.entry(key).or_default() += 1;
                    if fail_examples.len() < 20 {
                        fail_examples.push(format!("mft {i} (fid {}): {e}", fid_of.get(&i).copied().unwrap_or(0)));
                    }
                }
            }
        }
        Ok(())
    })?;
    w.flush()?;

    let n_failed: u64 = failed.values().sum();
    eprintln!("== models: {} files, parsed {parsed}, anim-only {anim_only}, failed {n_failed}, rows {n_rows}, distinct textures {} ({:.0}s)",
        list.len(), tex_ids.len(), t0.elapsed().as_secs_f32());
    let summary = serde_json::json!({
        "seconds": t0.elapsed().as_secs_f64(),
        "modl_files": list.len(),
        "parsed": parsed,
        "animation_only_skipped": anim_only,
        "failed": n_failed,
        "failures": failed,
        "failure_examples": fail_examples,
        "skinned_models": n_skel,
        "skinned_rule": "skel_file_ref != 0 || bone_count > 1 || bone_binding_count > 1",
        "external_skeleton_models": n_extref,
        "models_without_texture_refs": no_tex,
        "rows": n_rows,
        "distinct_texture_ids": tex_ids.len(),
        "distinct_texture_ids_skinned": tex_skinned.len(),
        "distinct_texture_ids_non_skinned": tex_rigid.len(),
        "distinct_texture_ids_both": tex_skinned.intersection(&tex_rigid).count(),
        "modl_versions": versions.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>(),
    });
    crate::write_summary(out, &summary)?;
    Ok(())
}
