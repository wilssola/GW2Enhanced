//! v9 of the shell fur (docs/shell-fur.md "v9"): which PARTS of a fur mesh are fur, decided per UV island of the mesh itself.
//!
//! WHY. The v7 / v8 discriminator decides per texel (gloss, flatness, keratin colour, red, relative to the draw's coat). Per-texel
//! rules are statistical: the border of a horn whose 5x5 windows still reach into the fur, the matte base of a claw that has the
//! coat's colour and gloss, a claw whose uv footprint spills onto the white texels around its brown blob, fur-like painting inside
//! a horn: each leaks a few strands, and on some coats more (seen in the game: claws and a horn's upper edge with
//! strands, 74: a paw whose claws are clean). A horn, a claw, a fang is a PART of the mesh: its own UV island (or a few). Deciding per
//! island makes the verdict all-or-nothing for the part: one horn texel that looks like fur no longer grows a strand.
//!
//! HOW (all automatic, no config line, fail open = the v8 texel rule alone):
//! 1. Capture (render thread, once per mesh): the recorded draw is replayed once more through the engine's vertex shader and a
//!    stream-output geometry shader (`gs_tri` in shellfur.hlsl) that writes, per triangle (`SV_PrimitiveID`), its three clip positions
//!    and its three ATLAS uv (exactly what the shells see: the engine's vertex shader computes the atlas uv from the mesh uv and cb0,
//!    and skins the positions). The engine's own vertex / index buffers are never mapped (they may be IMMUTABLE or DEFAULT without CPU
//!    access, and an input layout cannot be queried in D3D11); the stream-output buffer is copied into a staging buffer and read with
//!    DO_NOT_WAIT a frame or more later (never a stall). The draw's atlas level at <= 1024 texels (the level the strand shader's
//!    texel tests read) is copied back the same way, once per atlas.
//! 2. Analysis (background thread, `analyze`): islands by union-find over the triangles' vertices welded on (position, uv) (a uv seam
//!    splits, a hard normal edge does not; mirrored halves sharing a uv region stay apart); per triangle the v8 texel verdicts over its
//!    rasterised uv footprint (the CPU mirror of the strand shader's rule against the draw's own coat statistics, `coat_of`); per island
//!    its non-fur share and a shape descriptor of the 3D part (area share of the mesh, PCA elongation, taper along the main axis). An
//!    island is EXCLUDED when its non-fur share exceeds a threshold that is 0.40 for an ordinary island, down to 0.30 for a small,
//!    elongated, tapered part (horn / claw / fang shape) and up to 0.65 for a large island (the body skin): shape alone never excludes
//!    (an ear or a tail is tapered too, and fur). Inside a fur island, a connected group of triangles that is almost all non-fur (a
//!    claw strip fused into a foot island) is excluded as a sub-region. Fur triangles sharing a vertex position with an excluded part
//!    are marked NEAR (its seam / base).
//! 3. Output: 2 bits per triangle in an IMMUTABLE R32_UINT buffer, bound to the shell geometry shader as `t2`: an excluded triangle
//!    emits NO shell at all (every layer, also where its root is hidden or at a silhouette: the comb of 75 is gone by construction);
//!    a NEAR triangle passes a flag to the strand pixel shader, which then also reads the 5x5 windows FT_NEAR texels away (flat / glossy
//!    tests). The v8 texel rule still runs on every strand (island fur AND texel fur), and alone while a mesh is pending or failed.
//! 4. Cache: per mesh (index / vertex buffers, offsets, the call, the atlas view, the vertex shader) the result is kept for the session
//!    (it never flips frame to frame); every REVERIFY_FRAMES the mesh is captured again and its uv hash compared (a pooled buffer
//!    rewritten in place is re-analysed; meanwhile the texel rule alone).
//!
//! v10 (docs/shell-fur.md "v10"): the real island tables (`furmesh-*.txt` of a game session) showed the charr HORNS as 24 small
//! islands in their own atlas tile whose texels the v8 rule calls fur (0..2 % non-fur: ridges read as strands, the dye as coat), so no
//! texel threshold can exclude them. Two more rules: (a) SHAPE: islands sharing vertex positions inside one atlas tile form a 3D
//! PART; a part in a tile that does not hold the mesh's largest island (the coat's), small, is measured in 3D (closure, enclosed
//! volume, roundness and radius of its slices along the geodesic distance from its attachment ring, prongs per slice) and excluded
//! whole when it is a cone (horn, claw, fang), whatever its texels; flat cards / plates, cylinders, paws and anything in the coat's
//! own tile (tail, limbs) are never judged so; (b) COPIES: an island with the triangle count and uv box of an excluded island (its
//! mirror / LOD copy) is excluded too.
//!
//! Dev: `status_shells=` lists per replayed draw the islands total / excluded / sub-regions / near and the state (pending capture,
//! read-back, analysis; ready; failed); with `shell_debug=2` (at the analysis) or at a debug capture, `GW2Graphics\debug\furmesh-<uvcrc>.txt`
//! (per island: triangles, uv box, 3D area share, elongation, taper, non-fur share, threshold, verdict) and `furisland-<uvcrc>.png` (the
//! atlas with the islands painted: green fur, red excluded island, orange excluded sub-region, yellow near).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use crate::d3d::{Obj, Tex2dDesc, release, vfn, vt};
use crate::furprobe::Img;
use crate::log;
use crate::shellfur::{coat_stats, texel_features, texel_weight};

/// Floats per captured triangle (5 float4: clip x y w + primitive id; clip x y w of v1, v2; uv0 uv1; uv2, -, -).
pub const SO_FLOATS: usize = 20;
pub const SO_STRIDE: u32 = (SO_FLOATS * 4) as u32;
/// Largest mesh analysed (triangles; a character draw is up to ~20k): 5.2 MB of stream output.
pub const MAX_TRIS: u32 = 1 << 16;
/// The shell geometry shader's slot of the per-triangle bits (`tri_bits`, shellfur.hlsl).
pub const GS_SLOT: u32 = 2;
/// Bits per triangle in the buffer: bit 0 = excluded (no shells), bit 1 = near an excluded part (stronger texel dilation).
#[allow(dead_code)] // documents the layout (tri_bits, shellfur.hlsl)
pub const BITS_PER_TRI: u32 = 2;
/// Island thresholds of the non-fur share: an ordinary island, a hard-shaped part (horn / claw / fang), the extra for a large island.
pub const T_BASE: f32 = 0.40;
pub const T_HARD: f32 = 0.30;
pub const T_LARGE: f32 = 0.25;
/// A texel is a non-fur vote when the v8 texel weight at it is below this.
pub const VOTE_CUT: f32 = 0.5;
/// Sub-regions inside a fur island: a triangle is a candidate from this non-fur share, the connected group needs this mean (or
/// SUB_MEAN_SHAPED with a hard shape), at least SUB_MIN_TEXELS texels and at most SUB_MAX_SHARE of its island's area.
pub const SUB_TRI: f32 = 0.70;
pub const SUB_MEAN: f32 = 0.85;
pub const SUB_MEAN_SHAPED: f32 = 0.70;
pub const SUB_MIN_TEXELS: f32 = 6.0;
pub const SUB_MAX_SHARE: f32 = 0.5;
/// A Ready mesh is captured again after this many replays to check its uv hash (a pooled buffer rewritten in place).
pub const REVERIFY_FRAMES: u64 = 900;
/// A capture / an atlas read-back not readable after this many replays fails (texel rule alone for that mesh).
pub const TIMEOUT_FRAMES: u64 = 240;
/// Meshes kept per device; analyses running at once.
pub const MAX_ENTRIES: usize = 256;
pub const MAX_JOBS: u32 = 2;

/// Triangle flags of the analysis.
pub const F_FUR: u8 = 0;
pub const F_HARD: u8 = 1;
pub const F_SUB: u8 = 2;
pub const F_NEAR: u8 = 3;

fn ss(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// ---- pure: the captured triangles ---------------------------------------------------------------------------------------------------

/// One captured triangle: view-space positions (rebuilt like the shell geometry shader: x / P00, y / P11, w) and atlas uv.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tri {
    pub p: [[f32; 3]; 3],
    pub uv: [[f32; 2]; 3],
    pub ok: bool,
}

/// The stream output read back (`n` triangles of SO_FLOATS) as triangles; the primitive id of entry i must be i (the order and the
/// count of the capture are checked: a partial or reordered read-back fails).
pub fn tris_from_so(raw: &[f32], n: usize, p00: f32, p11: f32) -> Result<Vec<Tri>, String> {
    if raw.len() < n * SO_FLOATS {
        return Err(format!("short read-back ({} of {} floats)", raw.len(), n * SO_FLOATS));
    }
    let (p00, p11) = (if p00.abs() > 1e-6 { p00 } else { 1.0 }, if p11.abs() > 1e-6 { p11 } else { 1.0 });
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let r = &raw[i * SO_FLOATS..(i + 1) * SO_FLOATS];
        if r[3] != i as f32 {
            return Err(format!("triangle {i} read back as primitive {}", r[3]));
        }
        let c = [[r[0], r[1], r[2]], [r[4], r[5], r[6]], [r[8], r[9], r[10]]];
        let p = c.map(|c| [c[0] / p00, c[1] / p11, c[2]]);
        let uv = [[r[12], r[13]], [r[14], r[15]], [r[16], r[17]]];
        let ok = p.iter().flatten().chain(uv.iter().flatten()).all(|x| x.is_finite());
        out.push(Tri { p, uv, ok });
    }
    Ok(out)
}

/// The atlas-uv hash of a capture (the cache's content check; also the debug files' name). Positions animate, the uv do not.
pub fn uv_hash(t: &[Tri]) -> u32 {
    let mut b = Vec::with_capacity(t.len() * 24);
    for tri in t {
        for q in tri.uv {
            b.extend_from_slice(&q[0].to_le_bytes());
            b.extend_from_slice(&q[1].to_le_bytes());
        }
    }
    crate::debugshot::crc32(&[&b])
}

/// FNV-1a over the parts of a mesh key.
pub fn mesh_key(parts: &[u64]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in parts {
        for b in p.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn len3(a: [f32; 3]) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// 3D area of a triangle (engine units squared).
pub fn area3(t: &Tri) -> f32 {
    if !t.ok {
        return 0.0;
    }
    0.5 * len3(cross3(sub3(t.p[1], t.p[0]), sub3(t.p[2], t.p[0])))
}

/// uv area of a triangle.
pub fn area_uv(t: &Tri) -> f32 {
    let (a, b, c) = (t.uv[0], t.uv[1], t.uv[2]);
    0.5 * ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs()
}

// ---- pure: islands (union-find over welded vertices) --------------------------------------------------------------------------------

/// Vertex ids of every triangle: `vid` welded on (position, uv) (an island's connectivity), `pid` on the position alone (what touches
/// in 3D across a uv seam: the NEAR flag).
pub struct Topo {
    pub vid: Vec<[u32; 3]>,
    pub pid: Vec<[u32; 3]>,
    pub nv: usize,
}

pub fn topo(t: &[Tri]) -> Topo {
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for tri in t.iter().filter(|t| t.ok) {
        for p in tri.p {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
    }
    let diag = if lo[0].is_finite() { len3(sub3(hi, lo)) } else { 1.0 };
    let qp = (diag * 2e-6).max(1e-12);
    let qu = 16384.0f32;
    let mut vmap: HashMap<[i64; 5], u32> = HashMap::with_capacity(t.len() * 2);
    let mut pmap: HashMap<[i64; 3], u32> = HashMap::with_capacity(t.len() * 2);
    let mut next_unique = u32::MAX;
    let (mut vid, mut pid) = (Vec::with_capacity(t.len()), Vec::with_capacity(t.len()));
    for tri in t {
        let mut v3 = [0u32; 3];
        let mut p3 = [0u32; 3];
        for j in 0..3 {
            if !tri.ok {
                // a broken triangle stands alone
                v3[j] = next_unique;
                p3[j] = next_unique;
                next_unique -= 1;
                continue;
            }
            let p = tri.p[j].map(|x| (x / qp).round() as i64);
            let u = tri.uv[j].map(|x| (x * qu).round() as i64);
            let n = vmap.len() as u32;
            v3[j] = *vmap.entry([p[0], p[1], p[2], u[0], u[1]]).or_insert(n);
            let n = pmap.len() as u32;
            p3[j] = *pmap.entry([p[0], p[1], p[2]]).or_insert(n);
        }
        vid.push(v3);
        pid.push(p3);
    }
    // the stand-alone ids above the welded ones: renumber densely
    let nv0 = vmap.len() as u32;
    let mut nv = nv0;
    for v in vid.iter_mut().chain(pid.iter_mut()) {
        for x in v.iter_mut() {
            if *x >= nv0 && *x > u32::MAX / 2 {
                *x = nv;
                nv += 1;
            }
        }
    }
    Topo { vid, pid, nv: nv as usize }
}

fn find(p: &mut [u32], mut x: u32) -> u32 {
    while p[x as usize] != x {
        p[x as usize] = p[p[x as usize] as usize];
        x = p[x as usize];
    }
    x
}

/// The islands: `of_tri[i]` = the island of triangle i (0..count), numbered in the order of their first triangle.
pub struct Islands {
    pub of_tri: Vec<u32>,
    pub count: usize,
}

pub fn build_islands(t: &[Tri], tp: &Topo) -> Islands {
    let mut parent: Vec<u32> = (0..tp.nv as u32).collect();
    for v in &tp.vid {
        let a = find(&mut parent, v[0]);
        for &x in &v[1..] {
            let b = find(&mut parent, x);
            if a != b {
                parent[b as usize] = a;
            }
        }
    }
    let mut ids: HashMap<u32, u32> = HashMap::new();
    let mut of_tri = Vec::with_capacity(t.len());
    for v in &tp.vid {
        let r = find(&mut parent, v[0]);
        let n = ids.len() as u32;
        of_tri.push(*ids.entry(r).or_insert(n));
    }
    Islands { of_tri, count: ids.len() }
}

// ---- pure: the shape of a part ------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shape {
    pub tris: u32,
    /// 3D area (engine units squared) and its share of the whole mesh.
    pub area: f32,
    pub share: f32,
    pub uv_area: f32,
    pub uv_box: [f32; 4],
    /// sqrt(largest / second PCA eigenvalue) of the area-weighted surface points: 1 = round / flat patch, 3+ = a long part.
    pub elong: f32,
    /// Mean distance from the main axis in the thinner end quarter / in the thickest quarter: ~0.2 a cone (horn, claw), ~1 a
    /// cylinder or a strip.
    pub taper: f32,
    /// Share of the area in the thinner end quarter.
    pub tip: f32,
    /// Length along the main axis (engine units).
    pub length: f32,
}

/// Eigenvalues (descending) and eigenvectors (columns, matching) of a symmetric 3x3 matrix (cyclic Jacobi).
pub fn eig3(m: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    let mut a = m;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..32 {
        let off = a[0][1].abs() + a[0][2].abs() + a[1][2].abs();
        if off < 1e-30 {
            break;
        }
        for (p, q) in [(0usize, 1usize), (0, 2), (1, 2)] {
            if a[p][q].abs() < 1e-300 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let t = if theta == 0.0 { 1.0 } else { t };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for k in 0..3 {
                let (akp, akq) = (a[k][p], a[k][q]);
                a[k][p] = c * akp - s * akq;
                a[k][q] = s * akp + c * akq;
            }
            for k in 0..3 {
                let (apk, aqk) = (a[p][k], a[q][k]);
                a[p][k] = c * apk - s * aqk;
                a[q][k] = s * apk + c * aqk;
            }
            for k in 0..3 {
                let (vkp, vkq) = (v[k][p], v[k][q]);
                v[k][p] = c * vkp - s * vkq;
                v[k][q] = s * vkp + c * vkq;
            }
        }
    }
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&i, &j| a[j][j].partial_cmp(&a[i][i]).unwrap_or(std::cmp::Ordering::Equal));
    let vals = idx.map(|i| a[i][i]);
    let mut vecs = [[0.0; 3]; 3];
    for (c, &i) in idx.iter().enumerate() {
        for k in 0..3 {
            vecs[k][c] = v[k][i];
        }
    }
    (vals, vecs)
}

/// The shape of the triangles `members` (of a mesh whose total 3D area is `mesh_area`).
pub fn shape_of(t: &[Tri], members: &[u32], mesh_area: f32) -> Shape {
    let mut s = Shape { tris: members.len() as u32, elong: 1.0, taper: 1.0, uv_box: [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY], ..Shape::default() };
    // weighted samples: the 3 vertices and the centroid of every triangle, a quarter of its area each
    let mut pts: Vec<([f64; 3], f64)> = Vec::with_capacity(members.len() * 4);
    for &i in members {
        let tri = &t[i as usize];
        s.uv_area += area_uv(tri);
        for q in tri.uv {
            s.uv_box = [s.uv_box[0].min(q[0]), s.uv_box[1].min(q[1]), s.uv_box[2].max(q[0]), s.uv_box[3].max(q[1])];
        }
        let a = area3(tri);
        if !(a > 0.0) || !a.is_finite() {
            continue;
        }
        s.area += a;
        let c = [0, 1, 2].map(|k| (tri.p[0][k] + tri.p[1][k] + tri.p[2][k]) as f64 / 3.0);
        for p in tri.p.iter().map(|p| p.map(|x| x as f64)).chain(std::iter::once(c)) {
            pts.push((p, a as f64 * 0.25));
        }
    }
    if !s.uv_box[0].is_finite() {
        s.uv_box = [0.0; 4];
    }
    s.share = if mesh_area > 0.0 { s.area / mesh_area } else { 0.0 };
    let wsum: f64 = pts.iter().map(|p| p.1).sum();
    if !(wsum > 0.0) || pts.len() < 3 {
        return s;
    }
    let mut mean = [0.0f64; 3];
    for (p, w) in &pts {
        for k in 0..3 {
            mean[k] += p[k] * w / wsum;
        }
    }
    let mut cov = [[0.0f64; 3]; 3];
    for (p, w) in &pts {
        let d = [p[0] - mean[0], p[1] - mean[1], p[2] - mean[2]];
        for a in 0..3 {
            for b in 0..3 {
                cov[a][b] += d[a] * d[b] * w / wsum;
            }
        }
    }
    let (l, v) = eig3(cov);
    let l0 = l[0].max(0.0);
    let l1 = l[1].max(l0 * 1e-8).max(1e-300);
    s.elong = ((l0 / l1).sqrt() as f32).min(1000.0);
    let axis = [v[0][0], v[1][0], v[2][0]];
    let (mut tmin, mut tmax) = (f64::INFINITY, f64::NEG_INFINITY);
    let proj: Vec<(f64, f64, f64)> = pts
        .iter()
        .map(|(p, w)| {
            let d = [p[0] - mean[0], p[1] - mean[1], p[2] - mean[2]];
            let tt = d[0] * axis[0] + d[1] * axis[1] + d[2] * axis[2];
            let r = [d[0] - tt * axis[0], d[1] - tt * axis[1], d[2] - tt * axis[2]];
            (tt, (r[0] * r[0] + r[1] * r[1] + r[2] * r[2]).sqrt(), *w)
        })
        .collect();
    for (tt, _, _) in &proj {
        tmin = tmin.min(*tt);
        tmax = tmax.max(*tt);
    }
    s.length = (tmax - tmin) as f32;
    if !(tmax - tmin > 0.0) {
        return s;
    }
    const NB: usize = 4;
    let mut rs = [0.0f64; NB];
    let mut ws = [0.0f64; NB];
    for (tt, r, w) in &proj {
        let b = (((tt - tmin) / (tmax - tmin) * NB as f64) as usize).min(NB - 1);
        rs[b] += r * w;
        ws[b] += w;
    }
    let mean_r: Vec<Option<f64>> = (0..NB).map(|b| (ws[b] > 0.0).then(|| rs[b] / ws[b])).collect();
    let first = mean_r.iter().flatten().next().copied().unwrap_or(0.0);
    let last = mean_r.iter().rev().flatten().next().copied().unwrap_or(0.0);
    let rmax = mean_r.iter().flatten().fold(0.0f64, |a, &b| a.max(b));
    if rmax > 0.0 {
        let (r_end, end_bin) = if first <= last { (first, 0) } else { (last, NB - 1) };
        s.taper = (r_end / rmax) as f32;
        s.tip = (ws[end_bin] / wsum) as f32;
    }
    s
}

/// 0..1: a small, elongated, tapered part (horn, claw, fang, nail). Shape alone never excludes an island (an ear or a tail is
/// tapered too): it only lowers the island's non-fur threshold.
pub fn hard_shape(s: &Shape) -> f32 {
    // (the PCA elongation of a cone surface is lower than its length / radius: its base holds most of the area; a 5:1 horn ~2.3)
    ss(1.4, 2.4, s.elong) * (1.0 - ss(0.35, 0.65, s.taper)) * (1.0 - ss(0.08, 0.20, s.share))
}

/// The non-fur share above which an island is excluded: T_BASE, down to T_HARD for a hard-shaped part, + T_LARGE for a large one.
pub fn island_threshold(hard: f32, share: f32) -> f32 {
    T_BASE + (T_HARD - T_BASE) * hard.clamp(0.0, 1.0) + T_LARGE * ss(0.15, 0.35, share)
}

// ---- pure: v10, the 3D parts (horns made of many islands whose texels look like fur) ------------------------------------------------

/// v10 (docs/shell-fur.md "v10"). Islands whose uv boxes come within TILE_GAP of each other form an atlas TILE (the composite packs
/// each piece of a character, head, horns, body, in its own region).
pub const TILE_GAP: f32 = 0.03;
/// A part is judged by its 3D shape only with at least this many triangles, at most this share of the mesh, in a tile of at most
/// TILE_MAX_SHARE of the mesh that does not hold the mesh's largest island (the coat's own tile: tails, limbs, fingers never qualify).
pub const PART_MIN_TRIS: usize = 16;
pub const PART_MAX_SHARE: f32 = 0.30;
pub const TILE_MAX_SHARE: f32 = 0.45;
/// A qualifying part is excluded when its cone score reaches CONE_CUT, lowered by up to CONE_TEXEL_BONUS when its texels are partly
/// non-fur (smooth over 0.10..0.30 of its texels).
pub const CONE_CUT: f32 = 0.5;
pub const CONE_TEXEL_BONUS: f32 = 0.25;
/// v12: the rest of a cone-shaped part (without its texel-excluded islands) keeps its fur only when none of its islands is longer than
/// this (elongation sqrt(l1 / l2)): the finger islands of every real paw table 1.03..1.66, the horn strips whose tip / side the texels
/// exclude (`da0abef0`) 2.63..3.32.
pub const REST_MAX_ELONG: f32 = 2.0;
/// v13: a triangle of a texel-excluded digit island is skin when at most this share of its texels is non-fur; the kept group's share
/// at most SKIN_MEAN (`analyze`, the thumb).
pub const SKIN_TRI: f32 = 0.30;
pub const SKIN_MEAN: f32 = 0.20;
/// v15: a two-tone LIMB island (`analyze`): at least this share of a mesh of at least LIMB_MIN_TRIS triangles, elongated at least
/// LIMB_MIN_ELONG (the arm strips of the real tables 3.2..6.0, the forearms 2.9..3.6; a face patch / the lips 1.05..1.2).
pub const LIMB_MIN_SHARE: f32 = 0.03;
pub const LIMB_MIN_TRIS: usize = 500;
pub const LIMB_MIN_ELONG: f32 = 2.0;
/// v15: ... and the mesh's largest island (the coat) is at least this share of the mesh (arms 21..42 %, a horn mesh 7 %).
pub const LIMB_COAT_MIN: f32 = 0.15;
/// Slices along the part (geodesic distance from its attachment ring) for the radius / roundness profile.
pub const PART_BINS: usize = 8;
/// More islands than this: no part analysis (the tile clustering is quadratic in the worst case; fail safe = v9).
pub const PART_MAX_ISLANDS: usize = 4096;
/// Two islands are copies (mirror halves, LOD copies) when they have the same triangle count and the same uv box (modulo whole uv
/// units) within this tolerance.
pub const DUP_TOL: f32 = 5e-4;

/// The shape of a 3D part (several islands joined through shared vertex positions inside one atlas tile).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PartShape {
    pub tris: u32,
    pub share: f32,
    /// |sum of area x normal| / area: 0 a closed surface (or a two-sided card), ~0.2 a horn open at its base, 0.5 a bowl, 1 a flat sheet.
    pub closure: f32,
    /// |enclosed volume| / area^1.5 (about the part's centroid): ~0.05 a cone, 0.09 a sphere, 0 a card (one- or two-sided).
    pub solid: f32,
    /// Mean over the slices of sqrt(second / first eigenvalue) of the slice's area-weighted normal moments: 1 a ring (tube, cone),
    /// 0 a flat strip or card (normals all along one axis).
    pub round: f32,
    /// Radius of the far end (last quarter of the slices) / the largest slice radius: ~0.1 a cone's tip, ~1 a cylinder.
    pub taper: f32,
    /// Geodesic length from the attachment ring to the far end / the largest slice radius.
    pub slender: f32,
    /// Area-weighted mean number of separate pieces per slice (each >= 10 % of its slice): 1 a horn, 4..5 a paw's fingers.
    pub branches: f32,
    /// The geodesic distances start at the largest boundary loop (true) or, for a closed part, at an end of its diameter (false).
    pub open: bool,
}

/// 0..1 how much the part is a closed, solid, round, tapered, slender, single protrusion: a horn, a claw, a fang. A flat card (mane
/// strip, tail feather, ear plate) fails round / solid, a skin patch fails closure, a cylinder or a stub fails taper / slender, a paw
/// with its fingers fails branches.
pub fn cone_score(p: &PartShape) -> f32 {
    (1.0 - ss(0.35, 0.55, p.closure))
        * ss(0.015, 0.035, p.solid)
        * ss(0.35, 0.60, p.round)
        * (1.0 - ss(0.30, 0.55, p.taper))
        * ss(1.8, 2.8, p.slender)
        * (1.0 - ss(1.3, 1.8, p.branches))
}

/// The cut a part's cone score must reach, given the non-fur share of its texels.
pub fn cone_cut(nonfur_share: f32) -> f32 {
    CONE_CUT - CONE_TEXEL_BONUS * ss(0.10, 0.30, nonfur_share)
}

/// Gap between two uv boxes (0 when they overlap).
fn box_gap(a: [f32; 4], b: [f32; 4]) -> f32 {
    let dx = (a[0].max(b[0]) - a[2].min(b[2])).max(0.0);
    let dy = (a[1].max(b[1]) - a[3].min(b[3])).max(0.0);
    dx.max(dy)
}

/// The atlas tile of every island (union of islands whose uv boxes, moved into 0..1 by whole uv units (a mirrored copy may sit one
/// atlas width over), are within TILE_GAP), numbered densely.
pub fn tiles(boxes: &[[f32; 4]]) -> Vec<u32> {
    let boxes: Vec<[f32; 4]> = boxes
        .iter()
        .map(|b| {
            let (fu, fv) = (if b[0].is_finite() { b[0].floor() } else { 0.0 }, if b[1].is_finite() { b[1].floor() } else { 0.0 });
            [b[0] - fu, b[1] - fv, b[2] - fu, b[3] - fv]
        })
        .collect();
    let n = boxes.len();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| boxes[a][0].partial_cmp(&boxes[b][0]).unwrap_or(std::cmp::Ordering::Equal));
    for (k, &i) in order.iter().enumerate() {
        for &j in &order[k + 1..] {
            if !(boxes[j][0] <= boxes[i][2] + TILE_GAP) {
                break;
            }
            if box_gap(boxes[i], boxes[j]) <= TILE_GAP {
                let (a, b) = (find(&mut parent, i as u32), find(&mut parent, j as u32));
                if a != b {
                    parent[b as usize] = a;
                }
            }
        }
    }
    let mut ids: HashMap<u32, u32> = HashMap::new();
    (0..n as u32)
        .map(|i| {
            let r = find(&mut parent, i);
            let k = ids.len() as u32;
            *ids.entry(r).or_insert(k)
        })
        .collect()
}

/// Two uv boxes are the same up to whole uv units (a mirrored copy may sit one atlas width over).
pub fn same_box(a: [f32; 4], b: [f32; 4]) -> bool {
    let (fa, ga) = (a[0].floor(), a[1].floor());
    let (fb, gb) = (b[0].floor(), b[1].floor());
    (a[0] - fa - (b[0] - fb)).abs() <= DUP_TOL && (a[1] - ga - (b[1] - gb)).abs() <= DUP_TOL && (a[2] - fa - (b[2] - fb)).abs() <= DUP_TOL && (a[3] - ga - (b[3] - gb)).abs() <= DUP_TOL
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Area-weighted moments of one slice: area, sum of points x weight, second moments of the points, second moments of the normals.
#[derive(Clone, Copy, Default)]
struct Moments {
    w: f64,
    s: [f64; 3],
    ss: [[f64; 3]; 3],
    nn: [[f64; 3]; 3],
}

impl Moments {
    fn add_point(&mut self, p: [f64; 3], w: f64) {
        for a in 0..3 {
            self.s[a] += p[a] * w;
            for b in 0..3 {
                self.ss[a][b] += p[a] * p[b] * w;
            }
        }
    }
    fn merge(&mut self, o: &Moments) {
        self.w += o.w;
        for a in 0..3 {
            self.s[a] += o.s[a];
            for b in 0..3 {
                self.ss[a][b] += o.ss[a][b];
                self.nn[a][b] += o.nn[a][b];
            }
        }
    }
    /// RMS distance of the slice's points from its local axis (the line through its centroid along the direction its normals avoid:
    /// the least eigenvector of the normal moments; a ring's normals are all perpendicular to the tube). The slice's own length along
    /// the axis does not count.
    fn radius(&self) -> f64 {
        if !(self.w > 0.0) {
            return 0.0;
        }
        let m = [self.s[0] / self.w, self.s[1] / self.w, self.s[2] / self.w];
        let mut cov = [[0.0f64; 3]; 3];
        for a in 0..3 {
            for b in 0..3 {
                cov[a][b] = self.ss[a][b] / self.w - m[a] * m[b];
            }
        }
        let (_, v) = eig3(self.nn.map(|r| r.map(|x| x / self.w)));
        let ax = [v[0][2], v[1][2], v[2][2]];
        let tr = cov[0][0] + cov[1][1] + cov[2][2];
        let along: f64 = (0..3).map(|a| (0..3).map(|b| ax[a] * cov[a][b] * ax[b]).sum::<f64>()).sum();
        (tr - along).max(0.0).sqrt()
    }
    fn roundness(&self) -> f64 {
        if !(self.w > 0.0) {
            return 0.0;
        }
        let m = self.nn.map(|r| r.map(|x| x / self.w));
        let (l, _) = eig3(m);
        if l[0] > 1e-12 { (l[1].max(0.0) / l[0]).sqrt() } else { 0.0 }
    }
}

/// Multi-source Dijkstra over the part's vertex graph (local ids, edge lengths).
fn geodesic(adj: &[Vec<(u32, f32)>], sources: &[u32]) -> Vec<f32> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let mut d = vec![f32::INFINITY; adj.len()];
    let mut heap = BinaryHeap::new();
    for &s in sources {
        if (s as usize) < d.len() {
            d[s as usize] = 0.0;
            heap.push(Reverse((0u32, s)));
        }
    }
    while let Some(Reverse((db, v))) = heap.pop() {
        let dv = f32::from_bits(db);
        if dv > d[v as usize] {
            continue;
        }
        for &(w, l) in &adj[v as usize] {
            let nd = dv + l;
            if nd < d[w as usize] {
                d[w as usize] = nd;
                heap.push(Reverse((nd.to_bits(), w)));
            }
        }
    }
    d
}

/// The shape of the triangles `members` (one part) of a mesh whose total 3D area is `mesh_area`.
pub fn part_shape(t: &[Tri], tp: &Topo, members: &[u32], mesh_area: f32) -> PartShape {
    let mut ps = PartShape { tris: members.len() as u32, taper: 1.0, ..PartShape::default() };
    // Local vertices (welded on position: uv seams inside the part join), edges, area, normal sum, centroid.
    let mut local: HashMap<u32, u32> = HashMap::with_capacity(members.len() * 2);
    let mut pos: Vec<[f64; 3]> = Vec::new();
    let mut tri_v: Vec<[u32; 3]> = Vec::with_capacity(members.len());
    let mut tri_a: Vec<f64> = Vec::with_capacity(members.len());
    let mut tri_n: Vec<[f64; 3]> = Vec::with_capacity(members.len());
    let (mut area, mut nsum, mut cen) = (0.0f64, [0.0f64; 3], [0.0f64; 3]);
    for &i in members {
        let tri = &t[i as usize];
        if !tri.ok {
            continue;
        }
        let p = tri.p.map(|q| q.map(|x| x as f64));
        let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
        let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
        let c = [e1[1] * e2[2] - e1[2] * e2[1], e1[2] * e2[0] - e1[0] * e2[2], e1[0] * e2[1] - e1[1] * e2[0]];
        let a = 0.5 * dot3(c, c).sqrt();
        if !(a > 0.0) || !a.is_finite() {
            continue;
        }
        let mut v3 = [0u32; 3];
        for j in 0..3 {
            let n = pos.len() as u32;
            v3[j] = *local.entry(tp.pid[i as usize][j]).or_insert(n);
            if v3[j] == n {
                pos.push(p[j]);
            }
        }
        area += a;
        for k in 0..3 {
            nsum[k] += 0.5 * c[k];
            cen[k] += a * (p[0][k] + p[1][k] + p[2][k]) / 3.0;
        }
        tri_v.push(v3);
        tri_a.push(a);
        tri_n.push([c[0] / (2.0 * a), c[1] / (2.0 * a), c[2] / (2.0 * a)]);
    }
    if !(area > 0.0) || tri_v.len() < 2 {
        return ps;
    }
    ps.share = if mesh_area > 0.0 { (area / mesh_area as f64) as f32 } else { 0.0 };
    ps.closure = (dot3(nsum, nsum).sqrt() / area) as f32;
    let o = cen.map(|x| x / area);
    let mut vol = 0.0f64;
    for v in &tri_v {
        let q = v.map(|k| [pos[k as usize][0] - o[0], pos[k as usize][1] - o[1], pos[k as usize][2] - o[2]]);
        vol += dot3(q[0], [q[1][1] * q[2][2] - q[1][2] * q[2][1], q[1][2] * q[2][0] - q[1][0] * q[2][2], q[1][0] * q[2][1] - q[1][1] * q[2][0]]) / 6.0;
    }
    ps.solid = (vol.abs() / area.powf(1.5)) as f32;
    // Edges: the boundary (used by one triangle) and the graph.
    let nv = pos.len();
    let mut ecount: HashMap<(u32, u32), u32> = HashMap::with_capacity(tri_v.len() * 3);
    for v in &tri_v {
        for (a, b) in [(v[0], v[1]), (v[1], v[2]), (v[2], v[0])] {
            if a != b {
                *ecount.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
    }
    let len = |a: u32, b: u32| {
        let (p, q) = (pos[a as usize], pos[b as usize]);
        dot3([p[0] - q[0], p[1] - q[1], p[2] - q[2]], [p[0] - q[0], p[1] - q[1], p[2] - q[2]]).sqrt() as f32
    };
    let mut adj: Vec<Vec<(u32, f32)>> = vec![Vec::new(); nv];
    let mut badj: Vec<Vec<u32>> = vec![Vec::new(); nv];
    for (&(a, b), &c) in &ecount {
        let l = len(a, b);
        adj[a as usize].push((b, l));
        adj[b as usize].push((a, l));
        if c == 1 {
            badj[a as usize].push(b);
            badj[b as usize].push(a);
        }
    }
    // The attachment: the longest boundary loop (a horn's base ring; a small hole at a tip or an eye socket does not count).
    let mut seen = vec![false; nv];
    let (mut best, mut best_len) = (Vec::new(), 0.0f32);
    for s in 0..nv {
        if seen[s] || badj[s].is_empty() {
            continue;
        }
        let mut comp = vec![s as u32];
        seen[s] = true;
        let (mut q, mut l) = (0, 0.0f32);
        while q < comp.len() {
            let v = comp[q];
            q += 1;
            for &w in &badj[v as usize] {
                if v < w {
                    l += len(v, w);
                }
                if !seen[w as usize] {
                    seen[w as usize] = true;
                    comp.push(w);
                }
            }
        }
        if l > best_len {
            best_len = l;
            best = comp;
        }
    }
    ps.open = !best.is_empty();
    let d = if ps.open {
        geodesic(&adj, &best)
    } else {
        // Closed: from one end of its (approximate) diameter.
        let d0 = geodesic(&adj, &[0]);
        let far = (0..nv).filter(|&k| d0[k].is_finite()).max_by(|&a, &b| d0[a].partial_cmp(&d0[b]).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(0);
        geodesic(&adj, &[far as u32])
    };
    let l_max = d.iter().copied().filter(|x| x.is_finite()).fold(0.0f32, f32::max);
    if !(l_max > 0.0) {
        return ps;
    }
    // Slices by the distance of each triangle's centroid.
    let mut bins = [Moments::default(); PART_BINS];
    let mut tri_bin = vec![u8::MAX; tri_v.len()];
    for (k, v) in tri_v.iter().enumerate() {
        let dd = v.iter().map(|&x| d[x as usize]).filter(|x| x.is_finite()).collect::<Vec<f32>>();
        if dd.len() < 3 {
            continue;
        }
        let tc = (dd[0] + dd[1] + dd[2]) / 3.0;
        let b = ((tc / l_max * PART_BINS as f32) as usize).min(PART_BINS - 1);
        tri_bin[k] = b as u8;
        let a = tri_a[k];
        let m = &mut bins[b];
        m.w += a;
        let c = [0, 1, 2].map(|j| (pos[v[0] as usize][j] + pos[v[1] as usize][j] + pos[v[2] as usize][j]) / 3.0);
        for p in [pos[v[0] as usize], pos[v[1] as usize], pos[v[2] as usize], c] {
            m.add_point(p, a * 0.25);
        }
        let n = tri_n[k];
        for x in 0..3 {
            for y in 0..3 {
                m.nn[x][y] += n[x] * n[y] * a;
            }
        }
    }
    let rmax = bins.iter().map(|m| m.radius()).fold(0.0f64, f64::max);
    if !(rmax > 0.0) {
        return ps;
    }
    let wsum: f64 = bins.iter().map(|m| m.w).sum();
    ps.round = (bins.iter().map(|m| m.roundness() * m.w).sum::<f64>() / wsum.max(1e-300)) as f32;
    // Pieces per slice: triangles of one slice joined through a shared vertex.
    let mut tp_par: Vec<u32> = (0..tri_v.len() as u32).collect();
    let mut first: HashMap<(u32, u8), u32> = HashMap::with_capacity(tri_v.len() * 3);
    for (k, v) in tri_v.iter().enumerate() {
        if tri_bin[k] == u8::MAX {
            continue;
        }
        for &x in v {
            let o = *first.entry((x, tri_bin[k])).or_insert(k as u32);
            let (a, b) = (find(&mut tp_par, o), find(&mut tp_par, k as u32));
            if a != b {
                tp_par[b as usize] = a;
            }
        }
    }
    let mut piece_area: HashMap<u32, f64> = HashMap::new();
    for k in 0..tri_v.len() {
        if tri_bin[k] != u8::MAX {
            *piece_area.entry(find(&mut tp_par, k as u32)).or_insert(0.0) += tri_a[k];
        }
    }
    let mut count = [0u32; PART_BINS];
    for (&root, &a) in &piece_area {
        let b = tri_bin[root as usize] as usize;
        if a >= 0.1 * bins[b].w {
            count[b] += 1;
        }
    }
    ps.branches = ((0..PART_BINS).map(|b| count[b] as f64 * bins[b].w).sum::<f64>() / wsum.max(1e-300)) as f32;
    let q = PART_BINS / 4;
    let mut end = Moments::default();
    for m in &bins[PART_BINS - q..] {
        end.merge(m);
    }
    let mut start = Moments::default();
    for m in &bins[..q] {
        start.merge(m);
    }
    ps.taper = if ps.open {
        (end.radius() / rmax) as f32
    } else {
        // A closed part: the thinner end against the thicker one (a capsule or an eye is not tapered).
        let (a, b) = (start.radius(), end.radius());
        if a.max(b) > 0.0 { (a.min(b) / a.max(b)) as f32 } else { 1.0 }
    };
    ps.slender = (l_max as f64 / rmax) as f32;
    ps
}

// ---- pure: the votes (the v8 texel rule over each triangle's uv footprint) ----------------------------------------------------------

/// Calls `f(x, y)` for every texel (of a `w` x `h` grid, unwrapped coordinates) whose centre lies in the uv triangle; a triangle that
/// covers no centre calls it once at its centroid when `fallback`. Returns the number of calls. Absurdly large footprints are skipped.
pub fn raster(uv: &[[f32; 2]; 3], w: u32, h: u32, fallback: bool, mut f: impl FnMut(i64, i64)) -> u32 {
    let p = uv.map(|q| [q[0] * w as f32, q[1] * h as f32]);
    if !p.iter().flatten().all(|x| x.is_finite() && x.abs() < 1e7) {
        return 0;
    }
    let x0 = p.iter().map(|q| q[0]).fold(f32::INFINITY, f32::min).floor() as i64;
    let x1 = p.iter().map(|q| q[0]).fold(f32::NEG_INFINITY, f32::max).ceil() as i64;
    let y0 = p.iter().map(|q| q[1]).fold(f32::INFINITY, f32::min).floor() as i64;
    let y1 = p.iter().map(|q| q[1]).fold(f32::NEG_INFINITY, f32::max).ceil() as i64;
    if (x1 - x0 + 1) * (y1 - y0 + 1) > 1 << 22 {
        return 0;
    }
    let cr = |a: [f32; 2], b: [f32; 2], q: [f32; 2]| (b[0] - a[0]) * (q[1] - a[1]) - (b[1] - a[1]) * (q[0] - a[0]);
    let area = cr(p[0], p[1], p[2]);
    let mut n = 0u32;
    if area.abs() > 1e-9 {
        for y in y0..=y1 {
            for x in x0..=x1 {
                let q = [x as f32 + 0.5, y as f32 + 0.5];
                let (e0, e1, e2) = (cr(p[0], p[1], q), cr(p[1], p[2], q), cr(p[2], p[0], q));
                if (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) || (e0 <= 0.0 && e1 <= 0.0 && e2 <= 0.0) {
                    f(x, y);
                    n += 1;
                }
            }
        }
    }
    if n == 0 && fallback {
        let c = [(p[0][0] + p[1][0] + p[2][0]) / 3.0, (p[0][1] + p[1][1] + p[2][1]) / 3.0];
        f(c[0].floor() as i64, c[1].floor() as i64);
        n = 1;
    }
    n
}

/// The draw's coat statistics as the GPU computes them (`gs_uv` + `ps_feat` + `ps_stats`): the triangles rasterised into 32 x 32 uv
/// cells, the features of the atlas at each covered cell's centre. Mirror; (row 0, row 1) of the statistics.
pub fn coat_of(t: &[Tri], img: &Img) -> ([f32; 4], [f32; 4]) {
    const N: u32 = crate::shellfur::FEAT_N;
    let mut cov = vec![false; (N * N) as usize];
    for tri in t.iter().filter(|t| t.ok) {
        raster(&tri.uv, N, N, false, |x, y| {
            if x >= 0 && y >= 0 && x < N as i64 && y < N as i64 {
                cov[(y as u32 * N + x as u32) as usize] = true;
            }
        });
    }
    let cells: Vec<([f32; 4], f32)> = (0..N * N)
        .filter(|&i| cov[i as usize])
        .map(|i| texel_features(img, [((i % N) as f32 + 0.5) / N as f32, ((i / N) as f32 + 0.5) / N as f32]))
        .collect();
    coat_stats(&cells)
}

/// The v8 texel verdicts of an atlas level, computed once per texel (non-fur = the strand shader's texel weight below VOTE_CUT).
pub struct Voter<'a> {
    pub img: &'a Img,
    pub s0: [f32; 4],
    pub s1: [f32; 4],
    cache: Vec<u8>,
}

impl<'a> Voter<'a> {
    pub fn new(img: &'a Img, s0: [f32; 4], s1: [f32; 4]) -> Voter<'a> {
        Voter { img, s0, s1, cache: vec![255; (img.w * img.h) as usize] }
    }

    /// v15: texel (x, y) is not fur by the v8 rule WITHOUT its colour tests (flat, glossy only).
    pub fn nonfur_cf(&mut self, x: i64, y: i64) -> bool {
        self.nonfur(x, y);
        let (w, h) = (self.img.w as i64, self.img.h as i64);
        if w == 0 || h == 0 || self.img.px.len() < (w * h) as usize {
            return false;
        }
        self.cache[(y.rem_euclid(h) * w + x.rem_euclid(w)) as usize] & 2 != 0
    }

    /// Texel (x, y) (wrapping) is not fur by the v8 rule (dilated tests, the draw's coat).
    pub fn nonfur(&mut self, x: i64, y: i64) -> bool {
        let (w, h) = (self.img.w as i64, self.img.h as i64);
        if w == 0 || h == 0 || self.img.px.len() < (w * h) as usize {
            return false;
        }
        let (xi, yi) = (x.rem_euclid(w), y.rem_euclid(h));
        let k = (yi * w + xi) as usize;
        if self.cache[k] == 255 {
            let uv = [(xi as f32 + 0.5) / w as f32, (yi as f32 + 0.5) / h as f32];
            let (ft, hue) = crate::shellfur::texel_features_dilated(self.img, uv);
            self.cache[k] = (texel_weight(ft, hue, self.s0, self.s1) < VOTE_CUT) as u8 | ((crate::shellfur::texel_weight_opt(ft, hue, self.s0, self.s1, false) < VOTE_CUT) as u8) << 1;
        }
        self.cache[k] & 1 == 1
    }

    /// (non-fur texels, texels) of a triangle's uv footprint.
    /// (non-fur texels, texels, v15: non-fur texels without the colour tests) of a triangle's uv footprint.
    pub fn tri(&mut self, tri: &Tri) -> [f32; 3] {
        if !tri.ok {
            return [0.0, 0.0, 0.0];
        }
        let (w, h) = (self.img.w, self.img.h);
        let (mut nf, mut cf) = (0u32, 0u32);
        let n = raster(&tri.uv, w, h, true, |x, y| {
            nf += self.nonfur(x, y) as u32;
            cf += self.nonfur_cf(x, y) as u32;
        });
        [nf as f32, n as f32, cf as f32]
    }
}

// ---- pure: the classification ---------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IslandInfo {
    pub shape: Shape,
    /// Texels of its uv footprint and how many of them are non-fur by the v8 rule.
    pub texels: f32,
    pub nonfur: f32,
    pub hard_shape: f32,
    pub threshold: f32,
    pub excluded: bool,
    /// Triangles of excluded sub-regions (a fur island) and near triangles.
    pub sub_tris: u32,
    pub near_tris: u32,
    /// v13: triangles of an excluded digit island (thumb: skin + claw in one island) kept as its fur skin.
    pub skin_tris: u32,
    /// v15: its non-fur texels without the colour tests (flat, glossy only) and whether it is a two-tone LIMB island given back.
    pub nonfur_cf: f32,
    pub limb: bool,
    /// v15: its skin was re-admitted through a learned digit box (`DIGITS`).
    pub digit_memory: bool,
    /// v10: its 3D part (index into `Analysis::parts`) and why it is excluded (or not).
    pub part: u32,
    pub why: Why,
}

/// Why an island has its verdict (the dev dump's reason).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Why {
    /// Fur: the non-fur share is under the threshold and its part is not cone-shaped.
    #[default]
    Fur,
    /// v9: the non-fur share of its texels is above its threshold.
    Texels,
    /// v10: its 3D part is a horn / claw / fang shape in its own atlas tile.
    Shape,
    /// v10: a copy (same triangle count and uv box) of this excluded island.
    Copy(u32),
}

impl IslandInfo {
    pub fn share(&self) -> f32 {
        if self.texels > 0.0 { self.nonfur / self.texels } else { 0.0 }
    }
}

/// v10: one 3D part (its islands joined through shared vertex positions inside one atlas tile) and its verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PartInfo {
    pub shape: PartShape,
    pub islands: u32,
    pub tile: u32,
    pub tile_share: f32,
    /// In its own tile (not the coat's), small enough, enough triangles: judged by its shape.
    pub eligible: bool,
    pub cone: f32,
    pub cut: f32,
    /// Non-fur share of its texels.
    pub nonfur: f32,
    pub excluded: bool,
    /// v12: the part is cone-shaped only WITH islands its texels already exclude (a claw tip on a finger): its other islands (the
    /// rest) judged again alone. `rest_tris` triangles of the rest (0 = no such split), its cone and cut, and whether it was spared.
    pub rest_tris: u32,
    pub rest_cone: f32,
    pub rest_cut: f32,
    /// The largest elongation of the rest's islands (a finger segment 1.0..1.7, a horn strip 2.6..3.3 in the real tables).
    pub rest_elong: f32,
    pub spared: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub tris: usize,
    pub islands: Vec<IslandInfo>,
    #[allow(dead_code)] // tests and the docs' evidence
    pub of_tri: Vec<u32>,
    /// F_FUR / F_HARD / F_SUB / F_NEAR per triangle.
    pub flag: Vec<u8>,
    /// BITS_PER_TRI per triangle, 16 triangles per word (bit 0 excluded, bit 1 near).
    pub bits: Vec<u32>,
    pub s0: [f32; 4],
    pub s1: [f32; 4],
    pub uv_hash: u32,
    pub uvs: Vec<[[f32; 2]; 3]>,
    pub excluded_islands: u32,
    pub sub_regions: u32,
    pub hard_tris: u32,
    pub near_tris: u32,
    /// v10: the parts, and how many islands the shape rule / the copy rule excluded (beyond the texel votes).
    pub parts: Vec<PartInfo>,
    pub shape_islands: u32,
    pub copy_islands: u32,
    /// v12: islands of a cone-shaped part kept as fur (the finger skin under a texel-excluded claw tip).
    pub spared_islands: u32,
    /// v13 / v14: triangles of texel-excluded islands of a hand mesh re-admitted as their skin (the thumb).
    pub skin_tris: u32,
    /// v14: the draw's index count (the dump's header; set by the job).
    pub count: u32,
    /// v15: two-tone limb islands given back (no colour tests on their strands).
    pub limb_islands: u32,
    /// v15: islands that re-admitted their skin through a learned digit box (no proven fingers in this mesh: a gloved hand), whether
    /// the mesh has digit-like texel-excluded islands a later proof could give back, and the digit boxes this hand mesh proves.
    pub digit_islands: u32,
    pub digit_wait: bool,
    pub digit_boxes: Vec<[f32; 4]>,
    /// v15: DIGIT_GEN when it was analysed (set by the job).
    pub digit_gen: u64,
}

/// The whole analysis of one captured mesh against its atlas level.
#[allow(dead_code)] // tests (the game uses analyze_with and the session's digit boxes)
pub fn analyze(t: &[Tri], img: &Img) -> Analysis {
    analyze_with(t, img, &[])
}

/// v15: the DIGIT uv boxes learned from the hand meshes (v12 proven fingers) of this session, shared by every mesh (the charr hand
/// layout of the atlas is one template: the gloved arm meshes cut the same finger / thumb islands): a mesh without proven fingers of
/// its own (a gloved hand) re-admits the skin of a texel-excluded island whose uv box overlaps one of them (IoU >=
/// DIGIT_IOU of the smaller box). At most DIGIT_MAX boxes; DIGIT_GEN counts the additions (a Ready mesh that could use them is analysed again).
static DIGITS: Mutex<Vec<[f32; 4]>> = Mutex::new(Vec::new());
static DIGIT_GEN: AtomicU64 = AtomicU64::new(0);
pub const DIGIT_IOU: f32 = 0.7;
pub const DIGIT_MAX: usize = 256;

/// The uv box moved into 0..1 by whole uv units (a mirror copy one atlas width over is the same box).
fn unit_box(b: [f32; 4]) -> [f32; 4] {
    let (du, dv) = (b[0].floor(), b[1].floor());
    [b[0] - du, b[1] - dv, b[2] - du, b[3] - dv]
}

/// How much two uv boxes overlap: the intersection over the SMALLER box (1 = one inside the other: a gloved mesh's thumb island cut
/// larger or smaller than the bare hand's).
pub fn box_iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let (a, b) = (unit_box(a), unit_box(b));
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let i = iw * ih;
    let m = ((a[2] - a[0]) * (a[3] - a[1])).min((b[2] - b[0]) * (b[3] - b[1]));
    if m > 0.0 { i / m } else { 0.0 }
}

/// v15: the session's digit boxes (a copy for one analysis).
pub fn digits_snapshot() -> Vec<[f32; 4]> {
    DIGITS.lock().map(|d| d.clone()).unwrap_or_default()
}

/// v15: adds the digit boxes a hand mesh proved (new ones only, bounded); true when something was added.
pub fn digits_learn(boxes: &[[f32; 4]]) -> bool {
    let Ok(mut d) = DIGITS.lock() else { return false };
    let mut added = false;
    for &b in boxes {
        let b = unit_box(b);
        if d.len() < DIGIT_MAX && !d.iter().any(|&x| box_iou(x, b) >= 0.95) {
            d.push(b);
            added = true;
        }
    }
    if added {
        DIGIT_GEN.fetch_add(1, Relaxed);
    }
    added
}

/// v15: forgets the learned digit boxes (tests).
#[allow(dead_code)]
pub(crate) fn digits_reset() {
    if let Ok(mut d) = DIGITS.lock() {
        d.clear();
    }
    DIGIT_GEN.fetch_add(1, Relaxed);
}

/// `analyze` with the session's digit boxes `digits` (v15).
pub fn analyze_with(t: &[Tri], img: &Img, digits: &[[f32; 4]]) -> Analysis {
    let n = t.len();
    let (s0, s1) = coat_of(t, img);
    let mut voter = Voter::new(img, s0, s1);
    let votes: Vec<[f32; 3]> = t.iter().map(|tri| voter.tri(tri)).collect();
    let tp = topo(t);
    let isl = build_islands(t, &tp);
    let mesh_area: f32 = t.iter().map(area3).filter(|a| a.is_finite()).sum();
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); isl.count];
    for (i, &k) in isl.of_tri.iter().enumerate() {
        members[k as usize].push(i as u32);
    }
    let mut flag = vec![F_FUR; n];
    let mut infos = Vec::with_capacity(isl.count);
    for m in &members {
        let shape = shape_of(t, m, mesh_area);
        let (mut nf, mut tx, mut cf) = (0.0f32, 0.0f32, 0.0f32);
        for &i in m {
            nf += votes[i as usize][0];
            tx += votes[i as usize][1];
            cf += votes[i as usize][2];
        }
        let hs = hard_shape(&shape);
        let th = island_threshold(hs, shape.share);
        let share = if tx > 0.0 { nf / tx } else { 0.0 };
        let excluded = tx > 0.0 && share > th;
        if excluded {
            for &i in m {
                flag[i as usize] = F_HARD;
            }
        }
        let why = if excluded { Why::Texels } else { Why::Fur };
        infos.push(IslandInfo { shape, texels: tx, nonfur: nf, hard_shape: hs, threshold: th, excluded, sub_tris: 0, near_tris: 0, skin_tris: 0, nonfur_cf: cf, limb: false, digit_memory: false, part: 0, why });
    }
    // v10: the 3D parts and the shape rule, then the copies.
    let parts = classify_parts(t, &tp, &isl, &members, &mut infos, mesh_area);
    propagate_copies(&mut infos);
    let (mut shape_islands, mut copy_islands) = (0u32, 0u32);
    let spared_islands = infos.iter().filter(|i| !i.excluded && parts.get(i.part as usize).is_some_and(|p| p.spared)).count() as u32;
    for (k, info) in infos.iter().enumerate() {
        match info.why {
            Why::Shape => shape_islands += 1,
            Why::Copy(_) => copy_islands += 1,
            _ => continue,
        }
        for &i in &members[k] {
            flag[i as usize] = F_HARD;
        }
    }
    // Triangle adjacency through welded vertices (CSR): sub-regions.
    let mut start = vec![0u32; tp.nv + 1];
    for v in &tp.vid {
        for &x in v {
            start[x as usize + 1] += 1;
        }
    }
    for k in 0..tp.nv {
        start[k + 1] += start[k];
    }
    let mut fill = start.clone();
    let mut adj = vec![0u32; start[tp.nv] as usize];
    for (i, v) in tp.vid.iter().enumerate() {
        for &x in v {
            adj[fill[x as usize] as usize] = i as u32;
            fill[x as usize] += 1;
        }
    }
    // v15: a LIMB island of a TWO-TONE coat (a rust / tan band on a grey or white coat: `e8bc2bb8` island 66) is fur:
    // an island in the coat's tile (the tile of the mesh's largest island: never a horn's or a claw's), at least LIMB_MIN_SHARE of a
    // mesh of at least LIMB_MIN_TRIS triangles, elongated (>= LIMB_MIN_ELONG: an arm / leg strip, not a face patch, lips, a claw) and
    // not hard-shaped, excluded by its texels ONLY because of the colour tests (without them its non-fur share is under its threshold).
    // Its triangles get bit value 3: no shells removed, and its strands skip the keratin / red colour tests (the flat and glossy tests
    // stay); its sub-regions are judged without the colours too.
    // (the coat's tile only when its largest island is a real coat: >= LIMB_COAT_MIN of the mesh; a separate horn mesh, `c1612da9`,
    // has only horn strips, its largest 7 %)
    let coat_tile = {
        let largest = (0..infos.len()).max_by(|&a, &b| infos[a].shape.area.partial_cmp(&infos[b].shape.area).unwrap_or(std::cmp::Ordering::Equal));
        largest.filter(|&l| infos[l].shape.share >= LIMB_COAT_MIN).and_then(|l| parts.get(infos[l].part as usize)).map(|p| p.tile)
    };
    let mut limb_tri = vec![false; n];
    let mut limb_islands = 0u32;
    if n >= LIMB_MIN_TRIS {
        for k in 0..infos.len() {
            let i = infos[k];
            let in_coat = coat_tile.is_some_and(|ct| parts.get(i.part as usize).is_some_and(|p| p.tile == ct));
            let cf_share = if i.texels > 0.0 { i.nonfur_cf / i.texels } else { 1.0 };
            if i.excluded && i.why == Why::Texels && in_coat && i.shape.share >= LIMB_MIN_SHARE && i.shape.elong >= LIMB_MIN_ELONG && i.hard_shape < 0.5 && cf_share <= i.threshold {
                infos[k].excluded = false;
                infos[k].why = Why::Fur;
                infos[k].limb = true;
                limb_islands += 1;
                for &t_i in &members[k] {
                    flag[t_i as usize] = F_FUR;
                    limb_tri[t_i as usize] = true;
                }
            }
        }
        // its copies (the mirrored limb excluded as a copy of it) follow it
        for k in 0..infos.len() {
            if let Why::Copy(j) = infos[k].why {
                if infos[j as usize].limb {
                    infos[k].excluded = false;
                    infos[k].why = Why::Fur;
                    infos[k].limb = true;
                    limb_islands += 1;
                    copy_islands = copy_islands.saturating_sub(1);
                    for &t_i in &members[k] {
                        flag[t_i as usize] = F_FUR;
                        limb_tri[t_i as usize] = true;
                    }
                }
            }
        }
    }
    // v13 / v14: the THUMB. In a HAND mesh (one where v12 proved finger skin under texel-excluded claw tips: no head, no body without
    // fingers), an island excluded by its texels (a digit whose skin and claw are one island, a knuckle claw with coat around it) keeps
    // the triangles of its skin: a connected group of triangles each texel-proven fur (own non-fur share <= SKIN_TRI, the group's <=
    // SKIN_MEAN, >= SUB_MIN_TEXELS texels) that reaches the island's attachment: it shares a vertex position with a fur triangle of
    // ANOTHER island that is not a finger's skin and is at least as large as the excluded island (the hand / the arm; a claw tip, even
    // one with a coat-coloured base, touches only its finger; a one-triangle sliver is no attachment). The claw triangles stay
    // excluded, the kept ones next to them are NEAR. v13 asked for the paw tile (the fingers' tile) and kept nothing in the game: the
    // character's thumb is elsewhere in the atlas. Meshes without proven fingers (every head: horns, fangs, lips) are
    // unchanged by construction.
    let hand_mesh = parts.iter().any(|p| p.spared);
    let digit_of = |b: [f32; 4]| digits.iter().any(|&d| box_iou(d, b) >= DIGIT_IOU);
    let mut skin_kept = 0u32;
    let mut digit_islands = 0u32;
    let mut digit_wait = false;
    if hand_mesh || infos.iter().any(|i| i.why == Why::Texels) {
        let mut fur_pos_area = vec![0.0f32; tp.nv]; // the largest fur, non-finger island at this position
        for i in 0..n {
            let k = isl.of_tri[i] as usize;
            let finger_skin = parts.get(infos[k].part as usize).is_some_and(|p| p.spared);
            if flag[i] == F_FUR && t[i].ok && !finger_skin {
                for &x in &tp.pid[i] {
                    let o = &mut fur_pos_area[x as usize];
                    *o = o.max(infos[k].shape.area);
                }
            }
        }
        for k in 0..infos.len() {
            let info = infos[k];
            let texel_hard = matches!(info.why, Why::Texels) || matches!(info.why, Why::Copy(j) if infos[j as usize].why == Why::Texels);
            if !texel_hard {
                continue;
            }
            // v15: without proven fingers in this mesh, only an island on a learned digit box (no attachment needed there: a gloved
            // finger / thumb tip may touch nothing but the glove's cut)
            let by_memory = !hand_mesh && digit_of(info.shape.uv_box);
            if !hand_mesh && !by_memory {
                // a small, digit-like island that a later proof could give back: analyse again when the memory grows
                digit_wait |= info.shape.share < 0.05 && info.texels > 0.0 && info.share() < 0.9;
                continue;
            }
            let fur_tri = |i: usize| t[i].ok && votes[i][1] > 0.0 && votes[i][0] / votes[i][1] <= SKIN_TRI;
            let mut seen_k: HashMap<u32, bool> = HashMap::new();
            for &s in &members[k] {
                let s = s as usize;
                if !fur_tri(s) || seen_k.contains_key(&(s as u32)) {
                    continue;
                }
                let mut comp = vec![s as u32];
                seen_k.insert(s as u32, true);
                let mut q = 0;
                while q < comp.len() {
                    let i = comp[q] as usize;
                    q += 1;
                    for &x in &tp.vid[i] {
                        for &j in &adj[start[x as usize] as usize..start[x as usize + 1] as usize] {
                            if isl.of_tri[j as usize] == k as u32 && !seen_k.contains_key(&j) && fur_tri(j as usize) {
                                seen_k.insert(j, true);
                                comp.push(j);
                            }
                        }
                    }
                }
                let (nf, tx) = comp.iter().fold((0.0f32, 0.0f32), |a, &i| (a.0 + votes[i as usize][0], a.1 + votes[i as usize][1]));
                let attached = comp.iter().any(|&i| tp.pid[i as usize].iter().any(|&x| fur_pos_area[x as usize] > 0.0 && fur_pos_area[x as usize] >= info.shape.area));
                if tx >= SUB_MIN_TEXELS && nf <= SKIN_MEAN * tx && (attached || by_memory) {
                    for &i in &comp {
                        flag[i as usize] = F_FUR;
                    }
                    if by_memory && infos[k].skin_tris == 0 {
                        digit_islands += 1;
                    }
                    infos[k].skin_tris += comp.len() as u32;
                    infos[k].digit_memory |= by_memory;
                    skin_kept += comp.len() as u32;
                }
            }
        }
    }
    let vnf = |i: usize| if limb_tri[i] { votes[i][2] } else { votes[i][0] };
    let cand: Vec<bool> = (0..n).map(|i| flag[i] == F_FUR && votes[i][1] > 0.0 && vnf(i) / votes[i][1] >= SUB_TRI).collect();
    let mut seen = vec![false; n];
    let mut sub_regions = 0u32;
    for s in 0..n {
        if !cand[s] || seen[s] {
            continue;
        }
        let island = isl.of_tri[s];
        let mut comp = vec![s as u32];
        seen[s] = true;
        let mut q = 0;
        while q < comp.len() {
            let i = comp[q] as usize;
            q += 1;
            for &x in &tp.vid[i] {
                for &j in &adj[start[x as usize] as usize..start[x as usize + 1] as usize] {
                    let j = j as usize;
                    if !seen[j] && cand[j] && isl.of_tri[j] == island {
                        seen[j] = true;
                        comp.push(j as u32);
                    }
                }
            }
        }
        let (nf, tx) = comp.iter().fold((0.0f32, 0.0f32), |a, &i| (a.0 + vnf(i as usize), a.1 + votes[i as usize][1]));
        let mean = if tx > 0.0 { nf / tx } else { 0.0 };
        let cs = shape_of(t, &comp, mesh_area);
        let ia = infos[island as usize].shape.area;
        let small = !(ia > 0.0) || cs.area <= SUB_MAX_SHARE * ia;
        if tx >= SUB_MIN_TEXELS && small && (mean >= SUB_MEAN || (mean >= SUB_MEAN_SHAPED && hard_shape(&cs) > 0.5)) {
            for &i in &comp {
                flag[i as usize] = F_SUB;
            }
            infos[island as usize].sub_tris += comp.len() as u32;
            sub_regions += 1;
        }
    }
    // NEAR: fur triangles that share a vertex POSITION with an excluded triangle (its seam / base, also across a uv seam).
    let mut hard_pos = vec![false; tp.nv];
    for i in 0..n {
        if flag[i] == F_HARD || flag[i] == F_SUB {
            for &x in &tp.pid[i] {
                hard_pos[x as usize] = true;
            }
        }
    }
    for i in 0..n {
        if flag[i] == F_FUR && t[i].ok && tp.pid[i].iter().any(|&x| hard_pos[x as usize]) {
            flag[i] = F_NEAR;
            infos[isl.of_tri[i] as usize].near_tris += 1;
        }
    }
    let mut bits = vec![0u32; n.div_ceil(16).max(1)];
    for (i, &f) in flag.iter().enumerate() {
        let b = match f {
            F_HARD | F_SUB => 1u32,
            F_NEAR => 2,
            _ if limb_tri[i] => 3, // v15: a two-tone limb triangle (shells, no colour tests)
            _ => 0,
        };
        bits[i / 16] |= b << ((i % 16) * 2);
    }
    // v15: the digit boxes this hand mesh proves (its kept finger skins, its texel-excluded islands that re-admitted their skin)
    let digit_boxes: Vec<[f32; 4]> = if hand_mesh {
        infos.iter().filter(|i| (!i.excluded && parts.get(i.part as usize).is_some_and(|p| p.spared)) || (i.excluded && i.skin_tris > 0)).map(|i| i.shape.uv_box).collect()
    } else {
        Vec::new()
    };
    Analysis {
        tris: n,
        excluded_islands: infos.iter().filter(|i| i.excluded).count() as u32,
        islands: infos,
        of_tri: isl.of_tri,
        hard_tris: flag.iter().filter(|&&f| f == F_HARD || f == F_SUB).count() as u32,
        near_tris: flag.iter().filter(|&&f| f == F_NEAR).count() as u32,
        flag,
        bits,
        s0,
        s1,
        uv_hash: uv_hash(t),
        uvs: t.iter().map(|t| t.uv).collect(),
        sub_regions,
        parts,
        shape_islands,
        copy_islands,
        spared_islands,
        skin_tris: skin_kept,
        count: 0,
        limb_islands,
        digit_islands,
        digit_wait,
        digit_boxes,
        digit_gen: 0,
    }
}

/// v10: the parts of the mesh (islands joined through a shared vertex POSITION when they lie in the same atlas tile), their shape,
/// and the shape rule: a part in its own tile (not the coat's), small, cone-shaped, is excluded whole (every island of it), whatever its
/// texels say (a horn whose ridges and dye read as fur). Sets `part` of every island; returns the parts.
pub fn classify_parts(t: &[Tri], tp: &Topo, isl: &Islands, members: &[Vec<u32>], infos: &mut [IslandInfo], mesh_area: f32) -> Vec<PartInfo> {
    let ni = infos.len();
    if ni == 0 {
        return Vec::new();
    }
    if ni > PART_MAX_ISLANDS {
        // every island its own part, none judged by shape
        for (k, info) in infos.iter_mut().enumerate() {
            info.part = k as u32;
        }
        return infos.iter().map(|i| PartInfo { islands: 1, nonfur: i.share(), ..PartInfo::default() }).collect();
    }
    let boxes: Vec<[f32; 4]> = infos.iter().map(|i| i.shape.uv_box).collect();
    let tile = tiles(&boxes);
    let nt = tile.iter().copied().max().map_or(0, |m| m as usize + 1);
    // Islands of one tile sharing a vertex position: one part.
    let mut parent: Vec<u32> = (0..ni as u32).collect();
    let mut owner: HashMap<(u32, u32), u32> = HashMap::with_capacity(t.len() * 2);
    for (i, p3) in tp.pid.iter().enumerate() {
        if !t[i].ok {
            continue;
        }
        let k = isl.of_tri[i];
        for &x in p3 {
            let o = *owner.entry((x, tile[k as usize])).or_insert(k);
            if o != k {
                let (a, b) = (find(&mut parent, o), find(&mut parent, k));
                if a != b {
                    parent[b as usize] = a;
                }
            }
        }
    }
    let mut ids: HashMap<u32, u32> = HashMap::new();
    for k in 0..ni {
        let r = find(&mut parent, k as u32);
        let n = ids.len() as u32;
        infos[k].part = *ids.entry(r).or_insert(n);
    }
    let np = ids.len();
    // Tiles: area share and whether they hold the mesh's largest island (the coat).
    let largest = (0..ni).max_by(|&a, &b| infos[a].shape.area.partial_cmp(&infos[b].shape.area).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(0);
    let mut tile_share = vec![0.0f32; nt];
    for k in 0..ni {
        tile_share[tile[k] as usize] += infos[k].shape.share;
    }
    let mut pm: Vec<Vec<u32>> = vec![Vec::new(); np];
    let mut out = vec![PartInfo::default(); np];
    let mut tex = vec![[0.0f32; 2]; np];
    for k in 0..ni {
        let p = infos[k].part as usize;
        pm[p].extend_from_slice(&members[k]);
        out[p].islands += 1;
        out[p].tile = tile[k];
        tex[p][0] += infos[k].nonfur;
        tex[p][1] += infos[k].texels;
    }
    for p in 0..np {
        let o = &mut out[p];
        o.tile_share = tile_share[o.tile as usize];
        o.nonfur = if tex[p][1] > 0.0 { tex[p][0] / tex[p][1] } else { 0.0 };
        let coat_tile = tile[largest] == o.tile;
        // cheap checks first: the shape only of parts that can qualify
        let pre = !coat_tile && o.tile_share <= TILE_MAX_SHARE && pm[p].len() >= PART_MIN_TRIS;
        if !pre {
            o.shape.tris = pm[p].len() as u32;
            o.shape.share = pm[p].iter().map(|&i| area3(&t[i as usize])).filter(|a| a.is_finite()).sum::<f32>() / mesh_area.max(1e-30);
            o.cut = cone_cut(o.nonfur);
            continue;
        }
        o.shape = part_shape(t, tp, &pm[p], mesh_area);
        o.eligible = o.shape.share <= PART_MAX_SHARE;
        o.cone = cone_score(&o.shape);
        o.cut = cone_cut(o.nonfur);
        o.excluded = o.eligible && o.cone >= o.cut;
        // v12: a finger with its claw. The part is cone-shaped and its texel bonus comes from islands the texels exclude anyway (the
        // claw tip): judge the rest (the finger skin) alone, without that bonus unless its own texels earn it. A rest that is not a
        // cone itself (a finger is an untapered tube, a palm / mitten no protrusion) keeps its fur; a rest that is still a cone (a
        // horn whose tip happens to be dark) stays excluded. Parts without texel-excluded islands (every real horn) are unchanged.
        if o.excluded {
            let mut rest: Vec<u32> = Vec::new();
            let (mut rn, mut rt, mut hard, mut el) = (0.0f32, 0.0f32, 0u32, 0.0f32);
            for k in 0..ni {
                if infos[k].part as usize != p {
                    continue;
                }
                if infos[k].excluded {
                    hard += 1;
                } else {
                    rest.extend_from_slice(&members[k]);
                    el = el.max(infos[k].shape.elong);
                    rn += infos[k].nonfur;
                    rt += infos[k].texels;
                }
            }
            if hard > 0 && !rest.is_empty() {
                let o = &mut out[p];
                o.rest_tris = rest.len() as u32;
                o.rest_cut = cone_cut(if rt > 0.0 { rn / rt } else { 0.0 });
                o.rest_cone = if rest.len() >= PART_MIN_TRIS { cone_score(&part_shape(t, tp, &rest, mesh_area)) } else { 0.0 };
                o.rest_elong = el;
                o.spared = el <= REST_MAX_ELONG && o.rest_cone < o.rest_cut;
            }
        }
    }
    for info in infos.iter_mut() {
        if !info.excluded && out[info.part as usize].spared {
            continue; // v12: the fur skin of a texel-excluded claw's part
        }
        if !info.excluded && out[info.part as usize].excluded {
            info.excluded = true;
            info.why = Why::Shape;
        }
    }
    out
}

/// v10: an island that is a copy of an excluded island (same triangle count, same uv box: a mirrored half or an LOD copy whose
/// shape measure fell on the other side of a threshold) is excluded too. Returns how many.
pub fn propagate_copies(infos: &mut [IslandInfo]) -> u32 {
    let n = infos.len();
    if n > PART_MAX_ISLANDS {
        return 0;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| infos[a].shape.tris.cmp(&infos[b].shape.tris));
    let mut changed = 0;
    let mut k = 0;
    while k < n {
        let mut e = k;
        while e < n && infos[order[e]].shape.tris == infos[order[k]].shape.tris {
            e += 1;
        }
        let group = &order[k..e];
        for &i in group {
            if infos[i].excluded {
                continue;
            }
            let src = group.iter().copied().find(|&j| j != i && infos[j].excluded && !matches!(infos[j].why, Why::Copy(_)) && same_box(infos[j].shape.uv_box, infos[i].shape.uv_box));
            if let Some(j) = src {
                infos[i].excluded = true;
                infos[i].why = Why::Copy(j as u32);
                changed += 1;
            }
        }
        k = e;
    }
    changed
}

/// The 2-bit flag of triangle `i` in a bit buffer (mirror of the shell geometry shader's read).
#[allow(dead_code)] // tests
pub fn tri_bits(bits: &[u32], i: u32) -> u32 {
    bits.get((i / 16) as usize).map_or(0, |w| (w >> ((i % 16) * 2)) & 3)
}

// ---- debug files ------------------------------------------------------------------------------------------------------------------

/// `furmesh-<uvcrc>.txt`.
pub fn dump_text(a: &Analysis, count: u32) -> String {
    let mut t = format!(
        "shell fur v15: the UV islands and 3D parts of one fur mesh (docs/shell-fur.md \"v9\", \"v10\", \"v12\", \"v13\", \"v14\", \"v15\")\nindex count {count}, {} triangles, {} islands, {} excluded ({} by the texels, {} by the part's shape, {} as copies; {} kept as fur skin under a texel-excluded claw; {} skin triangles of texel-excluded islands re-admitted in a hand mesh, v14; {} two-tone limb islands given back, {} digit islands by the learned digit boxes, v15), {} excluded sub-regions, {} excluded triangles, {} near triangles\n\
         coat statistics (CPU mirror): gloss band {:.0} %, median flatness {:.3}, {} cells, draw weight {:.2}; coat gloss {:.2} sat {:.2} value {:.2} hue {:.0}\n\
         rule 1 (texels, v9): excluded when non-fur share > threshold = {T_BASE} + ({T_HARD} - {T_BASE}) x hard-shape + {T_LARGE} x large(area share 0.15..0.35); hard-shape = elongation 1.4..2.4 x (1 - taper 0.35..0.65) x (1 - area share 0.08..0.20)\n\
         rule 2 (shape, v10): a part (islands sharing vertex positions inside one atlas tile, tile gap {TILE_GAP}) in a tile without the mesh's largest island, tile <= {:.0} % and part <= {:.0} % of the mesh, >= {PART_MIN_TRIS} triangles, is excluded whole when cone >= {CONE_CUT} - {CONE_TEXEL_BONUS} x smooth(non-fur 0.10..0.30); cone = (1 - closure 0.35..0.55) x solid 0.015..0.035 x round 0.35..0.60 x (1 - taper 0.30..0.55) x slender 1.8..2.8 x (1 - branches 1.3..1.8); v12: when the part also holds islands rule 1 excludes (a claw tip on a finger), its other islands are judged again alone (cut from their own texels) and keep their fur unless they are a cone themselves or one of them is elongated > {REST_MAX_ELONG} (a horn strip)\n\
         rule 3 (copies, v10): an island with the triangle count and uv box of an excluded island is excluded too\n\
         island  tris  uv box (u0 v0 u1 v1)            area3d%  elong  taper   tip  hardshape  texels  nonfur%  thresh%  part   cone  verdict\n",
        a.tris,
        a.islands.len(),
        a.excluded_islands,
        a.islands.iter().filter(|i| i.why == Why::Texels).count(),
        a.shape_islands,
        a.copy_islands,
        a.spared_islands,
        a.skin_tris,
        a.limb_islands,
        a.digit_islands,
        a.sub_regions,
        a.hard_tris,
        a.near_tris,
        a.s0[0] * 100.0,
        a.s0[1],
        a.s0[2] as u32,
        a.s0[3],
        a.s1[0],
        a.s1[1],
        a.s1[2],
        a.s1[3],
        TILE_MAX_SHARE * 100.0,
        PART_MAX_SHARE * 100.0,
    );
    let mut order: Vec<usize> = (0..a.islands.len()).collect();
    order.sort_by(|&x, &y| a.islands[y].shape.area.partial_cmp(&a.islands[x].shape.area).unwrap_or(std::cmp::Ordering::Equal));
    for k in order {
        let i = &a.islands[k];
        let b = i.shape.uv_box;
        t.push_str(&format!(
            "{k:6} {:5}  {:.4} {:.4} {:.4} {:.4}   {:7.3} {:6.2} {:6.2} {:5.2} {:10.2} {:7.0} {:8.1} {:8.1} {:5} {:6.2}  {}\n",
            i.shape.tris,
            b[0],
            b[1],
            b[2],
            b[3],
            i.shape.share * 100.0,
            i.shape.elong,
            i.shape.taper,
            i.shape.tip,
            i.hard_shape,
            i.texels,
            i.share() * 100.0,
            i.threshold * 100.0,
            i.part,
            a.parts.get(i.part as usize).map_or(0.0, |p| p.cone),
            if i.excluded {
                match i.why {
                    Why::Shape => {
                        let p = a.parts.get(i.part as usize).copied().unwrap_or_default();
                        format!("EXCLUDED (no shells): part {} is cone-shaped ({:.2} >= {:.2}) in its own tile", i.part, p.cone, p.cut)
                    }
                    Why::Copy(j) => format!("EXCLUDED (no shells): copy of excluded island {j}"),
                    _ if i.skin_tris > 0 => format!(
                        "EXCLUDED (no shells): texels {:.1} % non-fur > {:.1} %, except its skin: {} of {} triangles re-admitted: its skin {}",
                        i.share() * 100.0,
                        i.threshold * 100.0,
                        i.skin_tris,
                        i.shape.tris,
                        if i.digit_memory { "on a digit box learned from a bare hand (v15: no proven fingers in this mesh)" } else { "at the hand (v14)" }
                    ),
                    _ => format!("EXCLUDED (no shells): texels {:.1} % non-fur > {:.1} %", i.share() * 100.0, i.threshold * 100.0),
                }
            } else if i.limb {
                format!(
                    "fur (v15: a limb of a two-tone coat: {:.1} % non-fur with the colour tests, {:.1} % without; its strands skip the keratin / red tests){}",
                    i.share() * 100.0,
                    if i.texels > 0.0 { i.nonfur_cf / i.texels * 100.0 } else { 0.0 },
                    if i.near_tris > 0 { format!(", {} near", i.near_tris) } else { String::new() }
                )
            } else if i.sub_tris > 0 {
                format!("fur, {} triangles excluded as sub-regions{}", i.sub_tris, if i.near_tris > 0 { format!(", {} near", i.near_tris) } else { String::new() })
            } else if i.near_tris > 0 {
                format!("fur ({} near an excluded part){}", i.near_tris, fur_why(a, i))
            } else {
                format!("fur{}", fur_why(a, i))
            }
        ));
    }
    // v10: the parts (the larger ones, and every part judged by its shape).
    let mut po: Vec<usize> = (0..a.parts.len()).filter(|&p| a.parts[p].shape.tris as usize >= PART_MIN_TRIS || a.parts[p].eligible).collect();
    po.sort_by(|&x, &y| a.parts[y].shape.share.partial_cmp(&a.parts[x].shape.share).unwrap_or(std::cmp::Ordering::Equal));
    t.push_str("part  islands  tris  tile tile%  area3d%  closure  solid  round  taper  slender branches  open  nonfur%   cone    cut  verdict\n");
    for p in po.into_iter().take(64) {
        let q = &a.parts[p];
        let s = &q.shape;
        let judged = q.eligible || s.round > 0.0 || s.slender > 0.0;
        t.push_str(&format!(
            "{p:4} {:8} {:5} {:5} {:5.1} {:8.3} {} {:8.1} {:6.2} {:6.2}  {}\n",
            q.islands,
            s.tris,
            q.tile,
            q.tile_share * 100.0,
            s.share * 100.0,
            if judged {
                format!("{:8.3} {:6.3} {:6.2} {:6.2} {:8.2} {:8.2} {:5}", s.closure, s.solid, s.round, s.taper, s.slender, s.branches, if s.open { "yes" } else { "no" })
            } else {
                format!("{:>8} {:>6} {:>6} {:>6} {:>8} {:>8} {:>5}", "-", "-", "-", "-", "-", "-", "-")
            },
            q.nonfur * 100.0,
            q.cone,
            q.cut,
            if q.spared {
                format!("cone-shaped only with its texel-excluded islands (claw): the rest ({} triangles) alone cone {:.2} < {:.2}: keeps its fur (v12)", q.rest_tris, q.rest_cone, q.rest_cut)
            } else if q.excluded && q.rest_tris > 0 {
                format!("excluded by shape (every island of it; the rest without its texel-excluded islands: cone {:.2} / cut {:.2}, longest island elongation {:.2} (> {REST_MAX_ELONG} = a horn strip))", q.rest_cone, q.rest_cut, q.rest_elong)
            } else if q.excluded {
                "excluded by shape (every island of it)".to_string()
            } else if q.eligible {
                "judged by shape: fur".to_string()
            } else {
                part_skip(a, q)
            }
        ));
    }
    t
}

/// Why a part is not judged by its shape (the dump).
fn part_skip(a: &Analysis, q: &PartInfo) -> String {
    let largest = (0..a.islands.len()).max_by(|&x, &y| a.islands[x].shape.area.partial_cmp(&a.islands[y].shape.area).unwrap_or(std::cmp::Ordering::Equal));
    if largest.is_some_and(|l| a.parts.get(a.islands[l].part as usize).is_some_and(|p| p.tile == q.tile)) {
        "not judged: in the coat's tile (holds the largest island)".to_string()
    } else if q.tile_share > TILE_MAX_SHARE {
        format!("not judged: tile {:.0} % > {:.0} %", q.tile_share * 100.0, TILE_MAX_SHARE * 100.0)
    } else if (q.shape.tris as usize) < PART_MIN_TRIS {
        format!("not judged: < {PART_MIN_TRIS} triangles")
    } else {
        format!("not judged: part {:.0} % > {:.0} % of the mesh", q.shape.share * 100.0, PART_MAX_SHARE * 100.0)
    }
}

/// The reason of a fur island's verdict beyond its texels (the dump).
fn fur_why(a: &Analysis, i: &IslandInfo) -> String {
    match a.parts.get(i.part as usize) {
        Some(p) if p.spared => format!(" [part {} cone {:.2} >= {:.2} only with its texel-excluded claw; this skin alone cone {:.2} < {:.2}: kept, v12]", i.part, p.cone, p.cut, p.rest_cone, p.rest_cut),
        Some(p) if p.eligible => format!(" [part {} cone {:.2} < {:.2}]", i.part, p.cone, p.cut),
        _ => String::new(),
    }
}

/// `furisland-<uvcrc>.png` (rgb8 of the atlas level's size): the atlas dimmed, each triangle filled half-transparent by its flag
/// (green fur, red excluded island, orange excluded sub-region, yellow near).
pub fn dump_png(a: &Analysis, img: &Img) -> Vec<u8> {
    let (w, h) = (img.w, img.h);
    let mut rgb: Vec<u8> = img.px.iter().flat_map(|p| [p[0] / 2, p[1] / 2, p[2] / 2]).collect();
    if rgb.len() != (w * h * 3) as usize {
        return rgb;
    }
    for (i, uv) in a.uvs.iter().enumerate() {
        let col: [u8; 3] = match a.flag.get(i).copied().unwrap_or(F_FUR) {
            F_HARD => [230, 20, 20],
            F_SUB => [255, 140, 0],
            F_NEAR => [235, 225, 30],
            _ => [30, 200, 60],
        };
        raster(uv, w, h, true, |x, y| {
            let (xi, yi) = (x.rem_euclid(w as i64) as usize, y.rem_euclid(h as i64) as usize);
            let k = (yi * w as usize + xi) * 3;
            for c in 0..3 {
                rgb[k + c] = ((rgb[k + c] as u16 + col[c] as u16) / 2) as u8;
            }
        });
    }
    rgb
}

struct DumpItem {
    key: u64,
    count: u32,
    a: Arc<Analysis>,
    img: Arc<Img>,
    written: bool,
}

/// The newest analyses with their atlas (the debug capture writes the ones not yet written). At most DUMP_KEEP.
static DUMPS: Mutex<Vec<DumpItem>> = Mutex::new(Vec::new());
const DUMP_KEEP: usize = 6;

fn write_dump(a: &Analysis, img: &Img, count: u32) -> Option<String> {
    let dir = crate::grading::dir()?.join("dev");
    let _ = std::fs::create_dir_all(&dir);
    let txt = dir.join(format!("furmesh-{:08x}.txt", a.uv_hash));
    let png = dir.join(format!("furisland-{:08x}.png", a.uv_hash));
    let ok = std::fs::write(&txt, dump_text(a, count)).is_ok() & std::fs::write(&png, crate::debugshot::encode_png_rgb8(img.w, img.h, &dump_png(a, img))).is_ok();
    ok.then(|| format!("furmesh-{:08x}.txt + furisland-{:08x}.png", a.uv_hash, a.uv_hash))
}

/// The debug capture (`status_shells=`): writes the kept analyses not yet written (txt + png), on a background thread. v14: and the TEXT
/// of EVERY finished analysis of this session (the PNG needs the atlas, kept only for the newest DUMP_KEEP): a mesh analysed early in
/// the session (a charr arm) no longer leaves a stale `furmesh-*.txt` of an older version behind.
pub fn dump_on_capture() {
    let all: Vec<Arc<Analysis>> = match RESULTS.try_lock() {
        Ok(r) => r.as_ref().map(|m| m.values().filter_map(|v| v.as_ref().ok().cloned()).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if !all.is_empty() {
        let _ = std::thread::Builder::new().name("gw2g-furisland-txt".into()).spawn(move || {
            let Some(dir) = crate::grading::dir().map(|d| d.join("dev")) else { return };
            let _ = std::fs::create_dir_all(&dir);
            let mut n = 0;
            for a in &all {
                n += std::fs::write(dir.join(format!("furmesh-{:08x}.txt", a.uv_hash)), dump_text(a, a.count)).is_ok() as u32;
            }
            log!("furisland: wrote {n} furmesh-*.txt (every analysis of this session)");
        });
    }
    let todo: Vec<(Arc<Analysis>, Arc<Img>, u32)> = match DUMPS.try_lock() {
        Ok(mut d) => d
            .iter_mut()
            .filter(|i| !i.written)
            .map(|i| {
                i.written = true;
                (i.a.clone(), i.img.clone(), i.count)
            })
            .collect(),
        Err(_) => return,
    };
    if todo.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("gw2g-furisland-dump".into()).spawn(move || {
        for (a, img, count) in todo {
            if let Some(f) = write_dump(&a, &img, count) {
                log!("furisland: wrote dev\\{f}");
            }
        }
    });
}

// ---- background analyses -----------------------------------------------------------------------------------------------------------

/// 0 = normal; tests: 1 = off (never bound, never captured), 2 = every mesh stays pending, 3 = every capture fails.
pub(crate) static MODE: AtomicU8 = AtomicU8::new(0);
static JOBS: AtomicU32 = AtomicU32::new(0);
static RESULTS: Mutex<Option<HashMap<u64, Result<Arc<Analysis>, String>>>> = Mutex::new(None);
static DEBUG2: AtomicU8 = AtomicU8::new(0);

pub fn set_debug2(on: bool) {
    DEBUG2.store(on as u8, Relaxed);
}

pub fn enabled() -> bool {
    MODE.load(Relaxed) != 1
}

/// Tests (with the shell state dropped): forgets every finished analysis and the status (a new device can reuse pointers).
#[allow(dead_code)]
pub(crate) fn reset() {
    wait_jobs(60_000);
    *RESULTS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).1 = 0;
}

/// Tests: waits until no analysis runs (at most `ms`).
#[allow(dead_code)]
pub(crate) fn wait_jobs(ms: u64) {
    let t = std::time::Instant::now();
    while JOBS.load(Relaxed) > 0 && t.elapsed().as_millis() < ms as u128 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The analysis of `key` with the triangles read back and the atlas level (raw, decoded on the thread). Never on the render thread.
#[allow(clippy::too_many_arguments)]
fn spawn_job(key: u64, count: u32, floats: Vec<f32>, n: usize, p00: f32, p11: f32, atlas: Arc<Vec<u8>>, fmt: u32, w: u32, h: u32) -> bool {
    JOBS.fetch_add(1, Relaxed);
    let debug2 = DEBUG2.load(Relaxed) != 0;
    let r = std::thread::Builder::new().name("gw2g-furisland".into()).spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(Arc<Analysis>, Arc<Img>), String> {
            let px = crate::furprobe::decode(fmt, w, h, &atlas).ok_or_else(|| format!("atlas format {fmt} not decodable"))?;
            let img = Img { w, h, px };
            let tris = tris_from_so(&floats, n, p00, p11)?;
            let digits = digits_snapshot();
            let digit_gen = DIGIT_GEN.load(Relaxed);
            let mut a = analyze_with(&tris, &img, &digits);
            a.count = count;
            a.digit_gen = digit_gen;
            if !a.digit_boxes.is_empty() {
                digits_learn(&a.digit_boxes);
            }
            Ok((Arc::new(a), Arc::new(img)))
        }))
        .unwrap_or_else(|_| Err("panic in the analysis".to_string()));
        let out = match res {
            Ok((a, img)) => {
                if debug2 {
                    if let Some(f) = write_dump(&a, &img, count) {
                        log!("furisland: wrote dev\\{f}");
                    }
                }
                if let Ok(mut d) = DUMPS.lock() {
                    d.retain(|i| i.key != key);
                    d.push(DumpItem { key, count, a: a.clone(), img, written: debug2 });
                    while d.len() > DUMP_KEEP {
                        d.remove(0);
                    }
                }
                Ok(a)
            }
            Err(e) => Err(e),
        };
        RESULTS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(key, out);
        JOBS.fetch_sub(1, Relaxed);
    });
    if r.is_err() {
        JOBS.fetch_sub(1, Relaxed);
        return false;
    }
    true
}

// ---- the render-thread side (per device; owned by the shell state) ------------------------------------------------------------

/// What one replayed draw gets.
pub enum Look {
    /// The per-triangle bits (bind as the shell geometry shader's t2).
    Use(Obj),
    /// Use them, and capture again this frame to check the uv hash.
    UseAndVerify(Obj),
    /// Not known yet: capture this mesh (one capture per frame).
    Capture,
    /// Pending or failed: the texel rule alone.
    None,
}

/// Per-draw state for `status_shells=` (Copy, no allocation per frame).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrawIsl {
    pub count: u32,
    /// 0 off, 1 not analysable, 2 waiting for a capture, 3 reading back, 4 analysing, 5 ready, 6 failed.
    pub state: u8,
    pub islands: u32,
    pub excluded: u32,
    pub sub: u32,
    pub hard_tris: u32,
    pub near_tris: u32,
    pub tris: u32,
    /// v10: of `excluded`, the islands excluded by their part's shape and as copies of an excluded island.
    pub shape: u32,
    pub copies: u32,
    /// v12: islands kept as fur although their part is cone-shaped (finger skin under a texel-excluded claw).
    pub spared: u32,
    /// v13: triangles of texel-excluded digit islands kept as their skin (the thumb).
    pub skin: u32,
    /// v15: two-tone limb islands given back.
    pub limb: u32,
}

static STATUS: Mutex<([DrawIsl; crate::shellfur::MAX_RECS], usize)> =
    Mutex::new(([DrawIsl { count: 0, state: 0, islands: 0, excluded: 0, sub: 0, hard_tris: 0, near_tris: 0, tris: 0, shape: 0, copies: 0, spared: 0, skin: 0, limb: 0 }; crate::shellfur::MAX_RECS], 0));
static LAST_FAIL: Mutex<String> = Mutex::new(String::new());

pub fn publish_status(v: &[DrawIsl]) {
    if let Ok(mut s) = STATUS.try_lock() {
        let n = v.len().min(crate::shellfur::MAX_RECS);
        s.0[..n].copy_from_slice(&v[..n]);
        s.1 = n;
    }
}

/// The `status_shells=` part.
pub fn status_text() -> String {
    let (v, n) = *STATUS.lock().unwrap_or_else(|e| e.into_inner());
    let mut t = format!(
        "v9/v10/v12 UV-island classification (per mesh, cached; an island is excluded by its texels (v9), by the cone shape of its 3D part in its own atlas tile (v10: horns; v12: not the fur islands of a part whose cone needs its texel-excluded claw tip) or as a copy of an excluded island (v10); an excluded island / sub-region grows no shell on any layer, the v8 texel rule still runs on every strand and alone while pending / failed; {} analyses running): ",
        JOBS.load(Relaxed)
    );
    if MODE.load(Relaxed) == 1 {
        t.push_str("off");
        return t;
    }
    if n == 0 {
        t.push_str("no replayed draw yet");
    }
    for d in &v[..n] {
        let s = match d.state {
            0 => "off -> texel rule (v8)".to_string(),
            1 => "not analysable (instanced, not a triangle list or too large) -> texel rule (v8)".to_string(),
            2 => "pending: capture -> texel rule (v8)".to_string(),
            3 => "pending: read-back -> texel rule (v8)".to_string(),
            4 => "pending: analysis -> texel rule (v8)".to_string(),
            5 => format!(
                "ready: {} triangles, {} islands, {} excluded ({} by texels, {} by part shape, {} copies; {} kept as fur skin under a texel-excluded claw, v12; {} skin triangles of texel-excluded islands re-admitted, v14; {} two-tone limb islands given back, v15), {} sub-regions ({} triangles without shells), {} near -> islands AND texel rule",
                d.tris,
                d.islands,
                d.excluded,
                d.excluded.saturating_sub(d.shape + d.copies),
                d.shape,
                d.copies,
                d.spared,
                d.skin,
                d.limb,
                d.sub,
                d.hard_tris,
                d.near_tris
            ),
            _ => format!("failed ({}) -> texel rule (v8)", LAST_FAIL.lock().map(|g| g.clone()).unwrap_or_default()),
        };
        t.push_str(&format!("[count {}: {s}] ", d.count));
    }
    t
}

struct Capture {
    so: Obj,
    stage: Obj,
    tris: u32,
    frame: u64,
    p00: f32,
    p11: f32,
    atlas: usize,
    data: Option<Vec<f32>>,
}

impl Capture {
    unsafe fn free(&mut self) {
        unsafe {
            release(self.so);
            release(self.stage);
        }
        self.so = std::ptr::null_mut();
        self.stage = std::ptr::null_mut();
    }
}

struct Ready {
    buf: Obj,
    srv: Obj,
    verified: u64,
    hash: u32,
    sum: DrawIsl,
    verifying: Option<Capture>,
    /// v15: the analysis has digit-like islands a learned digit box could give back, and DIGIT_GEN at its analysis.
    digit_wait: bool,
    digit_gen: u64,
}

enum St {
    Captured(Capture),
    Analyzing,
    Ready(Ready),
    Failed,
}

struct Entry {
    st: St,
    used: u64,
    count: u32,
}

struct AtlasRead {
    stage: Obj,
    w: u32,
    h: u32,
    fmt: u32,
    frame: u64,
    used: u64,
    raw: Option<Arc<Vec<u8>>>,
    failed: bool,
}

pub(crate) struct IslandGpu {
    entries: HashMap<u64, Entry>,
    atlases: HashMap<usize, AtlasRead>,
    frame: u64,
}

// SAFETY: the COM objects are only touched on the render thread (the shell state's mutex parks them).
unsafe impl Send for IslandGpu {}

type MapFn = unsafe extern "system" fn(Obj, Obj, u32, u32, u32, *mut [usize; 3]) -> i32;
type UnmapFn = unsafe extern "system" fn(Obj, Obj, u32);
type CreateBufferFn = unsafe extern "system" fn(Obj, *const [u32; 6], *const SubData, *mut Obj) -> i32;

#[repr(C)]
struct SubData {
    mem: *const std::ffi::c_void,
    pitch: u32,
    slice: u32,
}

const DO_NOT_WAIT: u32 = 0x10_0000;

fn fail(why: &str) {
    if let Ok(mut g) = LAST_FAIL.lock() {
        if *g != why {
            log!("furisland: {why}; that mesh keeps the texel rule alone");
            *g = why.to_string();
        }
    }
}

impl IslandGpu {
    pub fn new() -> IslandGpu {
        IslandGpu { entries: HashMap::new(), atlases: HashMap::new(), frame: 0 }
    }

    /// Once per replay: reads back finished captures and atlas levels (never waiting), starts analyses, uploads finished ones.
    ///
    /// # Safety
    /// `dev` / `ctx`: the live device and its immediate context (the objects of this state belong to it).
    pub unsafe fn tick(&mut self, dev: Obj, ctx: Obj) {
        self.frame += 1;
        let frame = self.frame;
        unsafe {
            let map: MapFn = std::mem::transmute(vfn(ctx, vt::CTX_MAP));
            let unmap: UnmapFn = std::mem::transmute(vfn(ctx, vt::CTX_UNMAP));
            // Atlas levels.
            for a in self.atlases.values_mut() {
                if a.raw.is_some() || a.failed || a.stage.is_null() || frame <= a.frame {
                    continue;
                }
                let mut m = [0usize; 3];
                if map(ctx, a.stage, 0, 1, DO_NOT_WAIT, &mut m) >= 0 && m[0] != 0 {
                    let (bpb, _) = crate::furprobe::fmt_kind(a.fmt).unwrap_or((4, 8));
                    let (rows, row_bytes) = if bpb > 0 { (a.h.div_ceil(4) as usize, a.w.div_ceil(4) as usize * bpb) } else { (a.h as usize, a.w as usize * 4) };
                    let pitch = m[1] & 0xffff_ffff;
                    let mut out = Vec::with_capacity(rows * row_bytes);
                    for r in 0..rows {
                        out.extend_from_slice(std::slice::from_raw_parts((m[0] + r * pitch) as *const u8, row_bytes));
                    }
                    unmap(ctx, a.stage, 0);
                    release(a.stage);
                    a.stage = std::ptr::null_mut();
                    a.raw = Some(Arc::new(out));
                } else if frame - a.frame > TIMEOUT_FRAMES {
                    release(a.stage);
                    a.stage = std::ptr::null_mut();
                    a.failed = true;
                }
            }
            // Captures.
            let mut reverify_mismatch: Vec<u64> = Vec::new();
            for e in self.entries.values_mut() {
                let cap = match &mut e.st {
                    St::Captured(c) => c,
                    St::Ready(r) => match r.verifying.as_mut() {
                        Some(c) => c,
                        None => continue,
                    },
                    _ => continue,
                };
                if cap.data.is_none() && frame > cap.frame && !cap.stage.is_null() {
                    let mut m = [0usize; 3];
                    if map(ctx, cap.stage, 0, 1, DO_NOT_WAIT, &mut m) >= 0 && m[0] != 0 {
                        let n = cap.tris as usize * SO_FLOATS;
                        cap.data = Some(std::slice::from_raw_parts(m[0] as *const f32, n).to_vec());
                        unmap(ctx, cap.stage, 0);
                        cap.free();
                    } else if frame - cap.frame > TIMEOUT_FRAMES {
                        cap.free();
                        cap.data = Some(Vec::new());
                    }
                }
            }
            for (key, e) in self.entries.iter_mut() {
                match &mut e.st {
                    St::Ready(r) => {
                        let Some(c) = r.verifying.as_mut() else { continue };
                        let Some(d) = c.data.take() else { continue };
                        let same = tris_from_so(&d, c.tris as usize, c.p00, c.p11).map(|t| uv_hash(&t) == r.hash).unwrap_or(false);
                        r.verifying = None;
                        if same {
                            r.verified = frame;
                        } else {
                            reverify_mismatch.push(*key);
                        }
                    }
                    St::Captured(c) => {
                        let Some(d) = c.data.as_ref() else { continue };
                        if d.is_empty() {
                            fail("the stream-output read-back timed out");
                            e.st = St::Failed;
                            continue;
                        }
                        let Some(a) = self.atlases.get_mut(&c.atlas) else {
                            fail("no atlas read-back");
                            e.st = St::Failed;
                            continue;
                        };
                        a.used = frame;
                        if a.failed {
                            fail("the atlas read-back failed");
                            e.st = St::Failed;
                            continue;
                        }
                        let Some(raw) = a.raw.clone() else { continue };
                        if MODE.load(Relaxed) == 2 || JOBS.load(Relaxed) >= MAX_JOBS {
                            continue; // stays pending (tests: forced)
                        }
                        let data = c.data.take().unwrap_or_default();
                        if spawn_job(*key, e.count, data, c.tris as usize, c.p00, c.p11, raw, a.fmt, a.w, a.h) {
                            e.st = St::Analyzing;
                        } else {
                            fail("could not start the analysis thread");
                            e.st = St::Failed;
                        }
                    }
                    St::Analyzing => {
                        let res = RESULTS.lock().ok().and_then(|mut g| g.as_mut().and_then(|m| m.remove(key)));
                        match res {
                            None => {}
                            Some(Err(why)) => {
                                fail(&why);
                                e.st = St::Failed;
                            }
                            Some(Ok(a)) => {
                                e.st = match upload(dev, &a) {
                                    Some((buf, srv)) => St::Ready(Ready {
                                        buf,
                                        srv,
                                        verified: frame,
                                        hash: a.uv_hash,
                                        digit_wait: a.digit_wait,
                                        digit_gen: a.digit_gen,
                                        sum: DrawIsl {
                                            count: e.count,
                                            state: 5,
                                            islands: a.islands.len() as u32,
                                            excluded: a.excluded_islands,
                                            sub: a.sub_regions,
                                            hard_tris: a.hard_tris,
                                            near_tris: a.near_tris,
                                            tris: a.tris as u32,
                                            shape: a.shape_islands,
                                            copies: a.copy_islands,
                                            spared: a.spared_islands,
                                            skin: a.skin_tris,
                                            limb: a.limb_islands,
                                        },
                                        verifying: None,
                                    }),
                                    None => {
                                        fail("could not create the per-triangle buffer");
                                        St::Failed
                                    }
                                };
                            }
                        }
                    }
                    St::Failed => {}
                }
            }
            for k in reverify_mismatch {
                if let Some(mut e) = self.entries.remove(&k) {
                    e.free();
                }
                log!("furisland: a mesh's uv changed in place (a pooled buffer reused): analysed again");
            }
            // Bounded caches: the least recently used meshes / atlases go.
            while self.entries.len() > MAX_ENTRIES {
                let Some((&k, _)) = self.entries.iter().filter(|(_, e)| matches!(e.st, St::Ready(_) | St::Failed)).min_by_key(|(_, e)| e.used) else { break };
                if let Some(mut e) = self.entries.remove(&k) {
                    e.free();
                }
            }
            let needed: Vec<usize> = self.entries.values().filter_map(|e| if let St::Captured(c) = &e.st { Some(c.atlas) } else { None }).collect();
            let old: Vec<usize> = self.atlases.iter().filter(|(k, a)| a.stage.is_null() && !needed.contains(k) && frame.saturating_sub(a.used) > 300).map(|(k, _)| *k).collect();
            for k in old {
                self.atlases.remove(&k);
            }
        }
    }

    /// What the draw with mesh `key` gets this frame; `analysable` = a non-instanced triangle list of at most MAX_TRIS triangles.
    pub fn look(&mut self, key: u64, analysable: bool, out: &mut DrawIsl) -> Look {
        out.state = 0;
        match MODE.load(Relaxed) {
            1 => return Look::None,
            3 => {
                out.state = 6;
                return Look::None;
            }
            _ => {}
        }
        if !analysable {
            out.state = 1;
            return Look::None;
        }
        let frame = self.frame;
        let Some(e) = self.entries.get_mut(&key) else {
            out.state = 2;
            return Look::Capture;
        };
        e.used = frame;
        // v15: the session learned new digit boxes since this mesh was analysed and it has islands they could give back: analyse again
        if let St::Ready(r) = &e.st {
            if r.digit_wait && r.verifying.is_none() && r.digit_gen < DIGIT_GEN.load(Relaxed) {
                unsafe { e.free() };
                self.entries.remove(&key);
                out.state = 2;
                return Look::Capture;
            }
        }
        let Some(e) = self.entries.get_mut(&key) else {
            out.state = 2;
            return Look::Capture;
        };
        match &e.st {
            St::Captured(c) => {
                out.state = if c.data.is_some() { 4 } else { 3 };
                Look::None
            }
            St::Analyzing => {
                out.state = 4;
                Look::None
            }
            St::Failed => {
                out.state = 6;
                Look::None
            }
            St::Ready(r) => {
                *out = r.sum;
                if r.verifying.is_none() && frame.saturating_sub(r.verified) > REVERIFY_FRAMES { Look::UseAndVerify(r.srv) } else { Look::Use(r.srv) }
            }
        }
    }

    /// Starts a capture of mesh `key` (`tris` triangles, the draw's atlas view `tex0`, the projection scale of its viewport): creates
    /// the stream-output buffer (returned: bind it, draw with the capture geometry shader, then `end`) and the atlas read-back if this
    /// atlas has none. Null = cannot (the mesh fails: texel rule alone).
    ///
    /// # Safety
    /// `dev` / `ctx` live; `tex0` a shader-resource view of this device or null.
    pub unsafe fn begin(&mut self, dev: Obj, ctx: Obj, key: u64, count: u32, tex0: Obj, p00: f32, p11: f32) -> Obj {
        let nul = std::ptr::null_mut();
        let tris = count / 3;
        let frame = self.frame;
        unsafe {
            let atlas = self.atlas_read(dev, ctx, tex0);
            let verifying = matches!(self.entries.get(&key).map(|e| &e.st), Some(St::Ready(_)));
            if atlas == 0 && !verifying {
                fail("the draw's atlas cannot be read back (format / size)");
                self.entries.insert(key, Entry { st: St::Failed, used: frame, count });
                return nul;
            }
            let create: CreateBufferFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
            let bytes = tris * SO_STRIDE;
            let (mut so, mut stage) = (nul, nul);
            let ok = create(dev, &[bytes, 0 /* DEFAULT */, 0x10 /* STREAM_OUTPUT */, 0, 0, 0], std::ptr::null(), &mut so) >= 0
                && create(dev, &[bytes, 3 /* STAGING */, 0, 0x20000 /* READ */, 0, 0], std::ptr::null(), &mut stage) >= 0
                && !so.is_null()
                && !stage.is_null();
            if !ok {
                release(so);
                release(stage);
                fail("could not create the stream-output buffers");
                if !verifying {
                    self.entries.insert(key, Entry { st: St::Failed, used: frame, count });
                }
                return nul;
            }
            let cap = Capture { so, stage, tris, frame, p00, p11, atlas, data: None };
            match self.entries.get_mut(&key) {
                Some(Entry { st: St::Ready(r), .. }) => r.verifying = Some(cap),
                _ => {
                    self.entries.insert(key, Entry { st: St::Captured(cap), used: frame, count });
                }
            }
            so
        }
    }

    /// The capture draw of `key` was issued: its stream output goes into the staging buffer (read back in a later frame).
    ///
    /// # Safety
    /// `ctx` live.
    pub unsafe fn end(&mut self, ctx: Obj, key: u64) {
        let cap = match self.entries.get_mut(&key).map(|e| &mut e.st) {
            Some(St::Captured(c)) => c,
            Some(St::Ready(r)) => match r.verifying.as_mut() {
                Some(c) => c,
                None => return,
            },
            _ => return,
        };
        unsafe {
            let copy: unsafe extern "system" fn(Obj, Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_RESOURCE));
            copy(ctx, cap.stage, cap.so);
            release(cap.so);
            cap.so = std::ptr::null_mut();
        }
    }

    /// The read-back of the draw atlas's level with at most 1024 texels across (the level the strand shader's texel tests use),
    /// started once per atlas resource. Returns the atlas key (its resource pointer) or 0.
    unsafe fn atlas_read(&mut self, dev: Obj, ctx: Obj, tex0: Obj) -> usize {
        if tex0.is_null() {
            return 0;
        }
        unsafe {
            let mut sd = [0u32; 8];
            let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 8]) = std::mem::transmute(vfn(tex0, vt::VIEW_GET_DESC));
            get_desc(tex0, &mut sd);
            let mut res: Obj = std::ptr::null_mut();
            let get_res: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(tex0, vt::VIEW_GET_RESOURCE));
            get_res(tex0, &mut res);
            if res.is_null() {
                return 0;
            }
            let key = res as usize;
            if let Some(a) = self.atlases.get_mut(&key) {
                a.used = self.frame;
                release(res);
                return if a.failed { 0 } else { key };
            }
            let mut ty = 0u32;
            let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
            get_type(res, &mut ty);
            let mut d = Tex2dDesc::default();
            if ty == 3 {
                let get_tdesc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
                get_tdesc(res, &mut d);
            }
            let kind = crate::furprobe::fmt_kind(d.format);
            if ty != 3 || sd[1] != 4 || kind.is_none() || d.mips == 0 {
                release(res);
                return 0;
            }
            let most = sd[2].min(d.mips.saturating_sub(1));
            let vm = if sd[3] == u32::MAX || sd[3] == 0 { d.mips - most } else { sd[3].min(d.mips - most) };
            let (vw, vh) = ((d.width >> most).max(1), (d.height >> most).max(1));
            let level = most + crate::furprobe::level_for(vw, vh, vm, 1024);
            let (w, h) = ((d.width >> level).max(1), (d.height >> level).max(1));
            let bc = kind.is_some_and(|k| k.0 > 0);
            if bc && (w < 4 || h < 4) {
                release(res);
                return 0;
            }
            let st = crate::debugshot::create_staging(dev, w, h, d.format);
            if st.is_null() {
                release(res);
                return 0;
            }
            let copy: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const u32) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_SUBRESOURCE_REGION));
            copy(ctx, st, 0, 0, 0, 0, res, level, std::ptr::null());
            release(res);
            self.atlases.insert(key, AtlasRead { stage: st, w, h, fmt: d.format, frame: self.frame, used: self.frame, raw: None, failed: false });
            key
        }
    }
}

/// The per-triangle bits as an IMMUTABLE R32_UINT buffer and its view.
unsafe fn upload(dev: Obj, a: &Analysis) -> Option<(Obj, Obj)> {
    unsafe {
        let create: CreateBufferFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_BUFFER));
        let n = a.bits.len().max(1) as u32;
        let init = SubData { mem: a.bits.as_ptr() as *const std::ffi::c_void, pitch: 0, slice: 0 };
        let mut buf: Obj = std::ptr::null_mut();
        if create(dev, &[n * 4, 1 /* IMMUTABLE */, 0x8 /* SHADER_RESOURCE */, 0, 0, 0], &init, &mut buf) < 0 || buf.is_null() {
            return None;
        }
        let create_srv: unsafe extern "system" fn(Obj, Obj, *const [u32; 6], *mut Obj) -> i32 = std::mem::transmute(vfn(dev, vt::DEV_CREATE_SRV));
        let mut srv: Obj = std::ptr::null_mut();
        // R32_UINT, D3D11_SRV_DIMENSION_BUFFER, FirstElement 0, NumElements n
        if create_srv(dev, buf, &[42, 1, 0, n, 0, 0], &mut srv) < 0 || srv.is_null() {
            release(buf);
            return None;
        }
        Some((buf, srv))
    }
}

impl Entry {
    unsafe fn free(&mut self) {
        unsafe {
            match &mut self.st {
                St::Captured(c) => c.free(),
                St::Ready(r) => {
                    release(r.srv);
                    release(r.buf);
                    if let Some(c) = r.verifying.as_mut() {
                        c.free();
                    }
                }
                _ => {}
            }
        }
        self.st = St::Failed;
    }
}

impl Drop for IslandGpu {
    fn drop(&mut self) {
        unsafe {
            for e in self.entries.values_mut() {
                e.free();
            }
            for a in self.atlases.values() {
                release(a.stage);
            }
        }
    }
}

#[cfg(test)]
#[path = "furisland_tests.rs"]
mod tests;
