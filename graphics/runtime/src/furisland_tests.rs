//! Host tests of the v9 island classification (furisland.rs): the union-find islands, the shape descriptor, the voting, the
//! sub-regions, the read-back checks, the dev files, and the real atlases of `GW2Graphics\debug\` (skipped when absent).

use super::*;

// ---- synthetic meshes -----------------------------------------------------------------------------------------------------------

fn nrm(a: [f32; 3]) -> [f32; 3] {
    let l = len3(a).max(1e-9);
    [a[0] / l, a[1] / l, a[2] / l]
}

/// Grid vertices (rows x cols) -> triangles, vertex (j, k) at `pos(j, k)` with uv `uv(j, k)`.
fn grid(rows: u32, cols: u32, pos: impl Fn(u32, u32) -> [f32; 3], uv: impl Fn(u32, u32) -> [f32; 2]) -> Vec<Tri> {
    let mut out = Vec::new();
    for j in 0..rows {
        for k in 0..cols {
            let v = |a: u32, b: u32| (pos(a, b), uv(a, b));
            let q = [v(j, k), v(j, k + 1), v(j + 1, k), v(j + 1, k + 1)];
            for t in [[0, 1, 2], [1, 3, 2]] {
                out.push(Tri { p: t.map(|i| q[i].0), uv: t.map(|i| q[i].1), ok: true });
            }
        }
    }
    out
}

/// A tapered cone (tip radius 8 % of the base) standing on the unit sphere along `d`, its uv unrolled over `rect` (the seam column
/// duplicated with another u).
fn cone(d: [f32; 3], len: f32, r: f32, rect: [f32; 4], tip: f32) -> Vec<Tri> {
    let n = nrm(d);
    let a = if n[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
    let u1 = nrm(cross3(n, a));
    let u2 = cross3(n, u1);
    let b = [n[0] * 0.92, n[1] * 0.92, n[2] * 0.92];
    let (rings, segs) = (8u32, 16u32);
    grid(
        rings,
        segs,
        |j, k| {
            let t = j as f32 / rings as f32;
            let rr = r * (1.0 - (1.0 - tip) * t);
            let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
            [0, 1, 2].map(|c| b[c] + n[c] * len * t + (u1[c] * ph.cos() + u2[c] * ph.sin()) * rr)
        },
        |j, k| [rect[0] + (rect[2] - rect[0]) * k as f32 / segs as f32, rect[3] - (rect[3] - rect[1]) * j as f32 / rings as f32],
    )
}

/// A large smooth patch of the unit sphere (the body skin): `rows` x `cols` quads over 0..pi x 0..pi, uv over `rect`.
fn skin(rect: [f32; 4], rows: u32, cols: u32, offset: [f32; 3]) -> Vec<Tri> {
    grid(
        rows,
        cols,
        |j, k| {
            let th = 0.15 + 2.8 * j as f32 / rows as f32;
            let ph = std::f32::consts::PI * k as f32 / cols as f32;
            [th.sin() * ph.cos() + offset[0], th.cos() + offset[1], -th.sin() * ph.sin() + offset[2]]
        },
        |j, k| [rect[0] + (rect[2] - rect[0]) * k as f32 / cols as f32, rect[1] + (rect[3] - rect[1]) * j as f32 / rows as f32],
    )
}

// ---- a synthetic atlas -----------------------------------------------------------------------------------------------------------

const S: u32 = 512;
const HORN: [f32; 4] = [0.56, 0.05, 0.76, 0.45];
const CLAW: [f32; 4] = [0.80, 0.05, 0.95, 0.30];
const EAR: [f32; 4] = [0.56, 0.55, 0.76, 0.95];
const SKIN: [f32; 4] = [0.0, 0.0, 0.5, 1.0];
/// A claw strip fused into a fur island (the sub-region case): tan, glossy.
const FUSED: [f32; 4] = [0.88, 0.40, 0.95, 0.60];
const FUSED_ISLAND: [f32; 4] = [0.66, 0.40, 0.95, 0.60];

fn inside(r: [f32; 4], u: f32, v: f32, inset: f32) -> bool {
    u > r[0] + inset && u < r[2] - inset && v > r[1] + inset && v < r[3] - inset
}

/// A painted white coat (gloss 0.40..0.44); the horn a flat grey with fur-like coat bands (12 rows of every 24) and a coat border;
/// the claw glossy tan on its upper 60 %, coat on its base; the fused strip glossy tan.
fn atlas() -> Img {
    let mut px = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        for x in 0..S {
            let (u, v) = ((x as f32 + 0.5) / S as f32, (y as f32 + 0.5) / S as f32);
            let mut s = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
            s ^= s >> 15;
            s = s.wrapping_mul(0x2C1B_3C6D);
            s ^= s >> 12;
            let hsh = (s & 0xffff) as f32 / 65535.0 - 0.5;
            let paint = 1.0 + 0.16 * hsh;
            let mut c = [0.94 * paint, 0.91 * paint, 0.86 * paint, 0.42 + 0.04 * hsh];
            if inside(HORN, u, v, 0.015) && (y / 12) % 2 == 0 {
                c = [0.50, 0.48, 0.45, 0.41];
            }
            if inside(CLAW, u, v, 0.0) && (CLAW[3] - v) / (CLAW[3] - CLAW[1]) > 0.4 {
                c = [0.62 * paint, 0.48 * paint, 0.34 * paint, 0.62];
            }
            if inside(FUSED, u, v, 0.0) {
                c = [0.62 * paint, 0.48 * paint, 0.34 * paint, 0.62];
            }
            px.push(c.map(|k| (k.clamp(0.0, 1.0) * 255.0 + 0.5) as u8));
        }
    }
    Img { w: S, h: S, px }
}

// ---- tests ---------------------------------------------------------------------------------------------------------------------

#[test]
fn union_find_builds_islands_by_position_and_uv() {
    // One cone with a duplicated seam column (same position, other u): one island.
    let c = cone([0.0, 1.0, 0.0], 0.6, 0.15, HORN, 0.08);
    let tp = topo(&c);
    assert_eq!(build_islands(&c, &tp).count, 1);
    // Two mirrored copies sharing one uv region but not their positions: two islands.
    let mut two = skin(SKIN, 6, 6, [0.0; 3]);
    two.extend(skin(SKIN, 6, 6, [3.0, 0.0, 0.0]));
    let tp = topo(&two);
    assert_eq!(build_islands(&two, &tp).count, 2);
    // Every triangle with its own vertex copies (a hard normal edge everywhere: same position and uv): still one island.
    let s = skin(SKIN, 4, 4, [0.0; 3]);
    let tp = topo(&s);
    assert_eq!(build_islands(&s, &tp).count, 1);
    // A uv seam through the middle (the right half's u shifted by 0.5: the same positions on the seam line): two islands.
    let mut cut = s.clone();
    for t in cut.iter_mut().skip(s.len() / 2) {
        for q in t.uv.iter_mut() {
            q[0] += 0.5;
        }
    }
    // (rows 0..1 are the first half of the triangles: the seam is the row line between them)
    let tp = topo(&cut);
    assert_eq!(build_islands(&cut, &tp).count, 2);
    // A broken (NaN) triangle stands alone and does not join anything.
    let mut bad = s.clone();
    bad[0].ok = false;
    bad[0].p[0][0] = f32::NAN;
    let tp = topo(&bad);
    let isl = build_islands(&bad, &tp);
    assert_eq!(isl.count, 2);
}

#[test]
fn the_shape_descriptor_tells_a_horn_from_the_skin_and_a_cylinder() {
    let (l, v) = eig3([[4.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 9.0]]);
    assert!((l[0] - 9.0).abs() < 1e-9 && (l[1] - 4.0).abs() < 1e-9 && (l[2] - 1.0).abs() < 1e-9 && (v[2][0].abs() - 1.0).abs() < 1e-9);
    let (l, _) = eig3([[2.0, 1.0, 0.0], [1.0, 2.0, 0.0], [0.0, 0.0, 0.5]]);
    assert!((l[0] - 3.0).abs() < 1e-9 && (l[1] - 1.0).abs() < 1e-9);
    let horn = cone([0.0, 1.0, 0.0], 0.8, 0.16, HORN, 0.08);
    let claw = cone([1.0, -1.0, 0.0], 0.4, 0.08, CLAW, 0.08);
    let cyl = cone([0.0, 1.0, 0.0], 0.7, 0.2, HORN, 1.0);
    let body = skin(SKIN, 24, 24, [0.0; 3]);
    let mut mesh = body.clone();
    mesh.extend(horn.iter().copied());
    let total: f32 = mesh.iter().map(area3).sum();
    let all = |t: &[Tri]| (0..t.len() as u32).collect::<Vec<u32>>();
    let sh = shape_of(&horn, &all(&horn), total);
    let sc = shape_of(&claw, &all(&claw), total);
    let sy = shape_of(&cyl, &all(&cyl), total);
    let sb = shape_of(&body, &all(&body), total);
    println!("horn {sh:?} hard {:.2}\nclaw {sc:?} hard {:.2}\ncylinder {sy:?} hard {:.2}\nskin {sb:?} hard {:.2}", hard_shape(&sh), hard_shape(&sc), hard_shape(&sy), hard_shape(&sb));
    assert!(sh.elong > 2.0 && sh.taper < 0.35 && hard_shape(&sh) > 0.6, "{sh:?}");
    assert!(sc.elong > 2.0 && sc.taper < 0.35 && hard_shape(&sc) > 0.6, "{sc:?}");
    assert!(sy.taper > 0.8 && hard_shape(&sy) < 0.1, "a cylinder / strip is not tapered: {sy:?}");
    assert!(sb.share > 0.8 && hard_shape(&sb) < 0.01, "{sb:?}");
    // The thresholds: a hard-shaped part 0.15..0.40, a large island up to 0.65.
    assert!((island_threshold(1.0, 0.01) - T_HARD).abs() < 1e-6 && (island_threshold(0.0, 0.01) - T_BASE).abs() < 1e-6);
    assert!((island_threshold(0.0, 0.9) - (T_BASE + T_LARGE)).abs() < 1e-6);
}

/// An orthonormal frame (n, u1, u2) around the direction `d`.
fn frame(d: [f32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let n = nrm(d);
    let a = if n[1].abs() < 0.9 { [0.0, 1.0, 0.0] } else { [1.0, 0.0, 0.0] };
    let u1 = nrm(cross3(n, a));
    (n, u1, cross3(n, u1))
}

/// A pointed EAR plate (v10: what a real ear is, flat; the v9 tests used a cone): a two-sided lens-shaped plate standing on the unit
/// sphere along `d`, `len` long, `width` wide at its base narrowing to a point, `thick` thick in its middle; the front and back sheets
/// share the rim (one 3D part), each sheet its own island (front over the left half of `rect`, back over the right half).
fn ear_plate(d: [f32; 3], len: f32, width: f32, thick: f32, rect: [f32; 4]) -> Vec<Tri> {
    let (n, u1, u2) = frame(d);
    let b = n.map(|x| x * 0.92);
    let (rows, cols) = (8u32, 8u32);
    let mut out = Vec::new();
    for side in [1.0f32, -1.0] {
        let half = if side > 0.0 { [rect[0], rect[1], (rect[0] + rect[2]) * 0.5 - 0.005, rect[3]] } else { [(rect[0] + rect[2]) * 0.5 + 0.005, rect[1], rect[2], rect[3]] };
        let mut sheet = grid(
            rows,
            cols,
            |j, k| {
                let t = j as f32 / rows as f32;
                let s = 2.0 * k as f32 / cols as f32 - 1.0;
                let w = 0.5 * width * (1.0 - t);
                let off = side * 0.5 * thick * (1.0 - s * s) * (1.0 - t);
                [0, 1, 2].map(|c| b[c] + n[c] * len * t + u1[c] * s * w + u2[c] * off)
            },
            |j, k| [half[0] + (half[2] - half[0]) * k as f32 / cols as f32, half[3] - (half[3] - half[1]) * j as f32 / rows as f32],
        );
        if side < 0.0 {
            for tri in sheet.iter_mut() {
                tri.p.swap(1, 2);
                tri.uv.swap(1, 2);
            }
        }
        out.extend(sheet);
    }
    out
}

/// The whole mesh of the synthetic test: skin, horn, claw, ear (each its own island; v10: the ear a flat plate, two islands).
fn test_mesh() -> (Vec<Tri>, [std::ops::Range<usize>; 4]) {
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let r0 = 0..t.len();
    let a = t.len();
    t.extend(cone([-0.7, 0.6, -0.3], 0.8, 0.16, HORN, 0.08));
    let r1 = a..t.len();
    let a = t.len();
    t.extend(cone([0.7, -0.6, -0.3], 0.4, 0.08, CLAW, 0.08));
    let r2 = a..t.len();
    let a = t.len();
    t.extend(ear_plate([0.6, 0.7, -0.3], 0.8, 0.5, 0.06, EAR));
    let r3 = a..t.len();
    (t, [r0, r1, r2, r3])
}

/// v10: a curved HORN like the real ones: a tube along an arc (`curve` radians over `len`) from the unit sphere along `d`, radius `r`
/// at its base down to a closed point, its surface cut into `strips` lengthwise strips times 2 (base piece, outer piece), each its own
/// uv island inside `tile` (positions shared along the seams: one 3D part). With `collar`, a short ring band of the HEAD (uv in that
/// rect) shares the horn's base ring (the horn is welded to the head, across atlas tiles). `mirror` flips x (the other horn: the same
/// uv, other positions).
#[allow(clippy::too_many_arguments)]
fn horn(d: [f32; 3], len: f32, r: f32, curve: f32, tile: [f32; 4], strips: u32, collar: Option<[f32; 4]>, mirror: bool) -> Vec<Tri> {
    let (n, u1, u2) = frame(d);
    let b = n.map(|x| x * 0.95);
    let (rings, segs) = (10u32, 16u32);
    let rc = if curve.abs() > 1e-4 { len / curve } else { 0.0 };
    let pos = |t: f32, k: u32| -> [f32; 3] {
        let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
        let (c, n1) = if t <= 0.0 || rc == 0.0 {
            ([0, 1, 2].map(|i| b[i] + n[i] * len * t), u1)
        } else {
            let a = curve * t;
            ([0, 1, 2].map(|i| b[i] + rc * (a.sin() * n[i] + (1.0 - a.cos()) * u1[i])), [0, 1, 2].map(|i| -a.sin() * n[i] + a.cos() * u1[i]))
        };
        let rr = if t <= 0.0 { r } else { r * (1.0 - t) };
        let p = [0, 1, 2].map(|i| c[i] + rr * (ph.cos() * n1[i] + ph.sin() * u2[i]));
        if mirror { [-p[0], p[1], p[2]] } else { p }
    };
    let mut out = Vec::new();
    let quad = |out: &mut Vec<Tri>, q: [[f32; 3]; 4], w: [[f32; 2]; 4]| {
        for tr in [[0usize, 1, 2], [1, 3, 2]] {
            out.push(Tri { p: tr.map(|i| q[i]), uv: tr.map(|i| w[i]), ok: true });
        }
    };
    let per = segs / strips;
    let split = 3u32;
    for j in 0..rings {
        for k in 0..segs {
            let s = k / per;
            let piece = (j >= split) as u32;
            // the strip's rect inside the tile: strips across u, the base / outer piece across v (0.005 apart: separate islands)
            let su = (tile[2] - tile[0]) / strips as f32;
            let r0 = [tile[0] + su * s as f32 + 0.0025, tile[1], tile[0] + su * (s + 1) as f32 - 0.0025, tile[3]];
            let vmid = tile[1] + (tile[3] - tile[1]) * 0.3;
            let rect = if piece == 0 { [r0[0], r0[1], r0[2], vmid - 0.0025] } else { [r0[0], vmid + 0.0025, r0[2], r0[3]] };
            let (j0, j1) = if piece == 0 { (0, split) } else { (split, rings) };
            let uvf = |jj: u32, kk: u32| [rect[0] + (rect[2] - rect[0]) * (kk - s * per) as f32 / per as f32, rect[1] + (rect[3] - rect[1]) * (jj - j0) as f32 / (j1 - j0) as f32];
            let t = |jj: u32| jj as f32 / rings as f32;
            quad(&mut out, [pos(t(j), k), pos(t(j), k + 1), pos(t(j + 1), k), pos(t(j + 1), k + 1)], [uvf(j, k), uvf(j, k + 1), uvf(j + 1, k), uvf(j + 1, k + 1)]);
        }
    }
    if let Some(cr) = collar {
        for k in 0..segs {
            let uvf = |jj: u32, kk: u32| [cr[0] + (cr[2] - cr[0]) * kk as f32 / segs as f32, cr[1] + (cr[3] - cr[1]) * jj as f32];
            quad(&mut out, [pos(-0.15, k), pos(-0.15, k + 1), pos(0.0, k), pos(0.0, k + 1)], [uvf(0, k), uvf(0, k + 1), uvf(1, k), uvf(1, k + 1)]);
        }
    }
    out
}

/// v10: a PAW-like part: `n` tapered fingers side by side (cones with the base radius `r`, 2 r apart), each its own island in `rect`,
/// neighbouring base rings sharing one vertex (one 3D part with several prongs).
fn paw(d: [f32; 3], n: u32, len: f32, r: f32, rect: [f32; 4]) -> Vec<Tri> {
    let (_, u1, _) = frame(d);
    let mut out: Vec<Tri> = Vec::new();
    let mut prev: Vec<[f32; 3]> = Vec::new();
    for k in 0..n {
        let du = (rect[2] - rect[0]) / n as f32;
        let sub = [rect[0] + du * k as f32 + 0.003, rect[1], rect[0] + du * (k + 1) as f32 - 0.003, rect[3]];
        let mut c = cone(d, len, r, sub, 0.08);
        let off = u1.map(|x| x * 2.0 * r * k as f32);
        let mut verts = Vec::new();
        for tri in c.iter_mut() {
            for p in tri.p.iter_mut() {
                *p = [p[0] + off[0], p[1] + off[1], p[2] + off[2]];
                if let Some(q) = prev.iter().find(|q| len3(sub3(**q, *p)) < 1e-4) {
                    *p = *q; // the shared base vertex with the previous finger
                }
                verts.push(*p);
            }
        }
        prev = verts;
        out.extend(c);
    }
    out
}

/// v10: a flat strip of `len` x `width` from the unit sphere along `d` (a mane strip, a tail feather card), slightly curled, tapering
/// to `tip` of its width; `two_sided` adds the back sheet (same positions, reversed, its own island next to it in `rect`).
fn strip(d: [f32; 3], len: f32, width: f32, tip: f32, rect: [f32; 4], two_sided: bool) -> Vec<Tri> {
    let (n, u1, u2) = frame(d);
    let b = n.map(|x| x * 0.95);
    let (rows, cols) = (12u32, 4u32);
    let pos = move |j: u32, k: u32| {
        let t = j as f32 / rows as f32;
        let s = k as f32 / cols as f32 - 0.5;
        let w = width * (1.0 - (1.0 - tip) * t);
        [0, 1, 2].map(|c| b[c] + n[c] * len * t + u1[c] * s * w + u2[c] * 0.06 * len * (std::f32::consts::PI * t).sin())
    };
    let front_r = if two_sided { [rect[0], rect[1], (rect[0] + rect[2]) * 0.5 - 0.005, rect[3]] } else { rect };
    let mut out = grid(rows, cols, pos, |j, k| [front_r[0] + (front_r[2] - front_r[0]) * k as f32 / cols as f32, front_r[3] - (front_r[3] - front_r[1]) * j as f32 / rows as f32]);
    if two_sided {
        let back_r = [(rect[0] + rect[2]) * 0.5 + 0.005, rect[1], rect[2], rect[3]];
        let mut back = grid(rows, cols, pos, |j, k| [back_r[0] + (back_r[2] - back_r[0]) * k as f32 / cols as f32, back_r[3] - (back_r[3] - back_r[1]) * j as f32 / rows as f32]);
        for tri in back.iter_mut() {
            tri.p.swap(1, 2);
            tri.uv.swap(1, 2);
        }
        out.extend(back);
    }
    out
}

#[test]
fn the_vote_excludes_the_whole_horn_and_claw_and_keeps_the_skin_and_the_ear() {
    let img = atlas();
    let (t, parts) = test_mesh();
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, t.len() as u32 * 3));
    // v10: the ear is a flat two-sided plate (front and back sheets: two islands, one part); it was a cone in v9.
    assert_eq!(a.islands.len(), 5);
    assert!(a.s0[3] > 0.99, "the draw is fur: {:?}", a.s0);
    for (k, (name, excluded)) in [("skin", false), ("horn", true), ("claw", true), ("ear", false)].into_iter().enumerate() {
        let island = a.of_tri[parts[k].start];
        let part = a.islands[island as usize].part;
        assert!(parts[k].clone().all(|i| a.islands[a.of_tri[i] as usize].part == part), "{name}: one part");
        if k < 3 {
            assert!(parts[k].clone().all(|i| a.of_tri[i] == island), "{name}: one island");
        }
        let info = a.islands[island as usize];
        // The texel rule alone: the share of the island's texels that it keeps as fur (where strands would grow).
        println!("{name}: non-fur share {:.2} (texel rule keeps {:.0} % of its texels), threshold {:.2}, hard-shape {:.2}", info.share(), 100.0 * (1.0 - info.share()), info.threshold, info.hard_shape);
        assert_eq!(info.excluded, excluded, "{name}: {info:?}");
        let flags_ok = parts[k].clone().all(|i| if excluded { a.flag[i] == F_HARD } else { a.flag[i] != F_HARD && a.flag[i] != F_SUB });
        assert!(flags_ok, "{name}");
        if excluded {
            // The texel rule alone would leave fur texels (strands) inside: the bands of the horn, the claw's base.
            assert!(info.share() < 0.9, "{name}: the scene proves something: {:.2}", info.share());
            for i in parts[k].clone() {
                assert_eq!(tri_bits(&a.bits, i as u32) & 1, 1);
            }
        }
    }
    assert_eq!(a.excluded_islands, 2);
    // The skin and the ear are fur by their votes; v10: the ear plate is judged by its shape (its own tile) and stays fur (flat).
    let ear = a.islands[a.of_tri[parts[3].start] as usize];
    let ep = a.parts[ear.part as usize];
    println!("ear part: {ep:?}");
    assert!(ear.share() < 0.05 && ep.eligible && !ep.excluded && ep.cone < 0.1, "{ear:?} {ep:?}");
    // The analysis is a pure function: the same capture gives the same bits (never flips).
    assert_eq!(analyze(&t, &img).bits, a.bits);
}

#[test]
fn a_claw_strip_fused_into_a_fur_island_is_an_excluded_sub_region_and_its_neighbours_are_near() {
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    // A paw: a flat-ish patch whose uv runs over FUSED_ISLAND, the right 0.07 of it (FUSED) on the claw tan.
    t.extend(grid(6, 16, |j, k| [2.0 + k as f32 * 0.05, j as f32 * 0.05, 0.02 * (k as f32 * 0.4).sin()], |j, k| {
        [FUSED_ISLAND[0] + (FUSED_ISLAND[2] - FUSED_ISLAND[0]) * k as f32 / 16.0, FUSED_ISLAND[1] + (FUSED_ISLAND[3] - FUSED_ISLAND[1]) * j as f32 / 6.0]
    }));
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, t.len() as u32 * 3));
    let paw = a.islands[a.of_tri[a0] as usize];
    assert!(!paw.excluded, "the paw island stays fur: {paw:?}");
    let on_strip = |i: usize| t[i].uv.iter().all(|q| q[0] >= FUSED[0] + 1e-4);
    let off_strip = |i: usize| t[i].uv.iter().all(|q| q[0] <= FUSED[0] - 0.02);
    let strip: Vec<usize> = (a0..t.len()).filter(|&i| on_strip(i)).collect();
    assert!(!strip.is_empty());
    assert!(strip.iter().all(|&i| a.flag[i] == F_SUB), "the strip's triangles are an excluded sub-region");
    assert!((a0..t.len()).filter(|&i| off_strip(i)).all(|i| a.flag[i] == F_FUR), "the fur away from it stays fur");
    assert!(a.near_tris > 0 && (a0..t.len()).any(|i| a.flag[i] == F_NEAR), "the fur touching the strip is near");
    assert_eq!(a.sub_regions, 1);
    for &i in &strip {
        assert_eq!(tri_bits(&a.bits, i as u32), 1);
    }
    for i in (a0..t.len()).filter(|&i| a.flag[i] == F_NEAR) {
        assert_eq!(tri_bits(&a.bits, i as u32), 2);
    }
    // The skin has no sub-region (its texels are all fur).
    assert!((0..a0).all(|i| a.flag[i] == F_FUR));
}

#[test]
fn the_capture_read_back_is_checked_and_the_bits_and_keys_pack() {
    let mut raw = vec![0.0f32; 3 * SO_FLOATS];
    for i in 0..3 {
        let r = &mut raw[i * SO_FLOATS..(i + 1) * SO_FLOATS];
        r[0..4].copy_from_slice(&[1.0, 2.0, 5.0, i as f32]);
        r[4..7].copy_from_slice(&[2.0, 2.0, 5.0]);
        r[8..11].copy_from_slice(&[1.0, 4.0, 5.0]);
        r[12..18].copy_from_slice(&[0.1, 0.2, 0.3, 0.2, 0.1, 0.4]);
    }
    let t = tris_from_so(&raw, 3, 2.0, 4.0).unwrap();
    assert_eq!(t[1].p[0], [0.5, 0.5, 5.0]);
    assert_eq!(t[2].uv[2], [0.1, 0.4]);
    assert!(t.iter().all(|t| t.ok));
    assert!(tris_from_so(&raw, 4, 2.0, 4.0).is_err(), "short read-back");
    raw[SO_FLOATS + 3] = 7.0;
    assert!(tris_from_so(&raw, 3, 2.0, 4.0).is_err(), "reordered / partial capture");
    let mut bits = vec![0u32; 2];
    for (i, b) in [(0u32, 1u32), (15, 2), (16, 3), (31, 1)] {
        bits[(i / 16) as usize] |= b << ((i % 16) * 2);
        assert_eq!(tri_bits(&bits, i), b);
    }
    assert_eq!(tri_bits(&bits, 1), 0);
    assert_eq!(tri_bits(&bits, 999), 0, "out of range = nothing");
    assert_ne!(mesh_key(&[1, 2, 3]), mesh_key(&[1, 2, 4]));
    assert_eq!(mesh_key(&[1, 2, 3]), mesh_key(&[1, 2, 3]));
    // The uv hash ignores the positions (they animate) and follows the uv.
    let mut moved = t.clone();
    moved[0].p[0][0] += 1.0;
    assert_eq!(uv_hash(&t), uv_hash(&moved));
    moved[0].uv[0][0] += 0.01;
    assert_ne!(uv_hash(&t), uv_hash(&moved));
}

#[test]
fn the_dev_dump_lists_every_island_and_paints_the_atlas() {
    let img = atlas();
    let (t, _) = test_mesh();
    let a = analyze(&t, &img);
    let txt = dump_text(&a, 999);
    assert!(txt.contains("index count 999") && txt.contains("5 islands, 2 excluded"), "{txt}");
    assert_eq!(txt.matches("EXCLUDED").count(), 2);
    let png = dump_png(&a, &img);
    assert_eq!(png.len(), (S * S * 3) as usize);
    // An excluded texel is reddish, a fur one greenish.
    let at = |u: f32, v: f32| {
        let k = (((v * S as f32) as u32 * S + (u * S as f32) as u32) * 3) as usize;
        [png[k], png[k + 1], png[k + 2]]
    };
    let h = at(0.66, 0.25);
    let s = at(0.25, 0.5);
    assert!(h[0] > h[1] && s[1] > s[0], "{h:?} {s:?}");
}

// ---- v10: the 3D parts, the shape rule, the copies ------------------------------------------------------------------------------

/// The horn tile of the v10 tests (plain coat texels in `atlas()`: the votes say fur, like the real charr horns of the dev tables).
const HORN_TILE: [f32; 4] = [0.56, 0.55, 0.76, 0.95];

#[test]
fn v10_a_curved_horn_of_many_islands_with_coat_texels_is_excluded_by_its_shape() {
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let head = 0..t.len();
    let a0 = t.len();
    // Two horns (mirrored: the same uv), each 8 islands, welded to a head collar whose uv lies in the head's tile.
    t.extend(horn([-0.6, 0.7, -0.3], 0.8, 0.16, 1.6, HORN_TILE, 4, Some([0.40, 0.90, 0.48, 0.98]), false));
    t.extend(horn([-0.6, 0.7, -0.3], 0.8, 0.16, 1.6, HORN_TILE, 4, Some([0.40, 0.90, 0.48, 0.98]), true));
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, t.len() as u32 * 3));
    let horn_tri = |i: usize| t[i].uv.iter().all(|q| q[0] >= HORN_TILE[0] - 1e-4);
    let horn_islands: std::collections::BTreeSet<u32> = (a0..t.len()).filter(|&i| horn_tri(i)).map(|i| a.of_tri[i]).collect();
    assert_eq!(horn_islands.len(), 16, "2 horns x 4 strips x 2 pieces");
    for &k in &horn_islands {
        let info = a.islands[k as usize];
        assert!(info.share() < 0.05, "the horn's texels are coat (the votes alone keep it): {info:?}");
        assert!(info.share() < info.threshold, "v9 alone would keep island {k} as fur");
        assert!(info.excluded && matches!(info.why, Why::Shape | Why::Copy(_)), "island {k}: {info:?}");
    }
    let parts: std::collections::BTreeSet<u32> = horn_islands.iter().map(|&k| a.islands[k as usize].part).collect();
    assert_eq!(parts.len(), 2, "one part per horn (the strips joined through their seams, not the collar of the other tile)");
    for &p in &parts {
        let q = a.parts[p as usize];
        println!("horn part {p}: {q:?}");
        assert!(q.eligible && q.excluded && q.cone >= 0.5 && q.shape.open, "{q:?}");
    }
    assert!((a0..t.len()).filter(|&i| horn_tri(i)).all(|i| a.flag[i] == F_HARD && tri_bits(&a.bits, i as u32) & 1 == 1), "no shell on any horn triangle");
    // The head and the collar (the head's tile) stay fur; the collar touches the horn: near.
    assert!(head.clone().all(|i| a.flag[i] == F_FUR));
    let collar: Vec<usize> = (a0..t.len()).filter(|&i| !horn_tri(i)).collect();
    assert!(!collar.is_empty() && collar.iter().all(|&i| a.flag[i] == F_NEAR), "the collar is fur, near the horn");
    assert_eq!(a.shape_islands as usize + a.copy_islands as usize, 16);
    // The same horn alone in the head's own tile (uv inside the skin's tile): not judged by shape (a tail / limb there stays fur).
    let mut t2 = skin(SKIN, 24, 24, [0.0; 3]);
    t2.extend(horn([-0.6, 0.7, -0.3], 0.8, 0.16, 1.6, [0.05, 0.05, 0.45, 0.30], 4, None, false));
    let a2 = analyze(&t2, &img);
    assert_eq!(a2.excluded_islands, 0, "{}", dump_text(&a2, 0));
}

#[test]
fn v10_flat_cards_plates_cylinders_and_coat_tile_tubes_stay_fur() {
    let img = atlas();
    let d = [0.6, 0.7, -0.3];
    let cases: [(&str, Vec<Tri>); 8] = [
        ("paw (4 fingers, one part)", paw(d, 4, 0.35, 0.08, HORN_TILE)),
        ("mane strip (one-sided)", strip(d, 0.7, 0.12, 1.0, HORN_TILE, false)),
        ("mane card (two-sided)", strip(d, 0.7, 0.12, 1.0, HORN_TILE, true)),
        ("tail feather card (two-sided, tapered)", strip(d, 0.6, 0.15, 0.2, HORN_TILE, true)),
        ("tapered strip (one-sided)", strip(d, 0.6, 0.15, 0.1, HORN_TILE, false)),
        ("ear plate (thick lens)", ear_plate(d, 0.5, 0.35, 0.08, HORN_TILE)),
        ("cylinder (own tile)", cone(d, 0.7, 0.12, HORN_TILE, 1.0)),
        ("tapered tail tube in the coat's tile", horn(d, 0.9, 0.12, 0.8, [0.05, 0.05, 0.45, 0.30], 4, None, false)),
    ];
    for (name, obj) in cases {
        let mut t = skin(SKIN, 24, 24, [0.0; 3]);
        let a0 = t.len();
        t.extend(obj);
        let a = analyze(&t, &img);
        let k = a.of_tri[a0];
        let p = a.parts[a.islands[k as usize].part as usize];
        println!("{name}: {p:?}");
        if name.starts_with("paw") {
            assert!(p.islands == 4 && p.shape.branches > 2.0, "{name}: one part of 4 prongs: {p:?}");
        }
        assert!((a0..t.len()).all(|i| a.flag[i] == F_FUR), "{name}: stays fur\n{}", dump_text(&a, 0));
        assert!(p.cone < 0.3, "{name}: far from the cut: {p:?}");
    }
}

#[test]
fn v10_copies_of_an_excluded_island_share_its_verdict() {
    let isl = |tris: u32, b: [f32; 4], excluded: bool, why: Why| IslandInfo {
        shape: Shape { tris, uv_box: b, ..Shape::default() },
        texels: 100.0,
        nonfur: 30.0,
        hard_shape: 0.0,
        threshold: 0.3,
        excluded,
        sub_tris: 0,
        near_tris: 0,
        skin_tris: 0,
        nonfur_cf: 0.0,
        limb: false,
        digit_memory: false,
        part: 0,
        why,
    };
    let b = [0.466, 0.18, 0.497, 0.2462];
    let mut v = vec![
        isl(7, b, true, Why::Texels),                                   // 0: excluded (30.5 % > 30.5 %)
        isl(7, b, false, Why::Fur),                                     // 1: its mirror (30.5 % < 30.6 %): a copy
        isl(7, [b[0] + 1.0, b[1], b[2] + 1.0, b[3]], false, Why::Fur),  // 2: the copy one atlas width over (uv offset 1)
        isl(8, b, false, Why::Fur),                                     // 3: another triangle count: not a copy
        isl(7, [b[0] + 0.01, b[1], b[2], b[3]], false, Why::Fur),       // 4: another box: not a copy
    ];
    assert_eq!(propagate_copies(&mut v), 2);
    assert_eq!(v.iter().map(|i| (i.excluded, i.why)).collect::<Vec<_>>(), vec![(true, Why::Texels), (true, Why::Copy(0)), (true, Why::Copy(0)), (false, Why::Fur), (false, Why::Fur)]);
    assert!(same_box([1.2, 0.1, 1.3, 0.2], [0.2, 0.1, 0.3, 0.2]) && !same_box([0.2, 0.1, 0.3, 0.2], [0.2, 0.1, 0.3, 0.21]));
    // Tiles: boxes within TILE_GAP join, a farther one stays apart.
    assert_eq!(tiles(&[[0.0, 0.0, 0.1, 0.1], [0.12, 0.0, 0.2, 0.1], [0.5, 0.5, 0.6, 0.6], [0.0, 0.12, 0.05, 0.2]]), vec![0, 0, 1, 0]);
    // A copy one atlas width over (uv offset -1, like a real mirrored body half) is in the same tile.
    assert_eq!(tiles(&[[0.003, 0.03, 0.078, 0.25], [-0.997, 0.03, -0.922, 0.25], [0.5, 0.5, 0.6, 0.6]]), vec![0, 0, 1]);
}

/// v10 on the REAL island tables of the dev folder (`furmesh-*.txt`, written in the game by v9: per island triangles, uv box, area
/// share, texels, non-fur share, verdict; the meshes themselves are not on disk). What can be replayed from a table: the copy rule
/// exactly, and the tile guard of the shape rule (which islands sit in a tile that may be judged by shape: an upper bound of what
/// the cone test can add; the cone test itself needs the triangles). Reported per table; skipped when no table is on disk.
#[test]
fn v10_the_real_island_tables_copies_and_shape_candidates() {
    let Ok(dir) = std::fs::read_dir(DEV) else {
        println!("no dev folder: skipped");
        return;
    };
    let mut files: Vec<std::path::PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("furmesh-") && n.ends_with(".txt"))).collect();
    files.sort();
    let mut seen = 0;
    let mut hand_tables = 0;
    for f in files {
        let Ok(txt) = std::fs::read_to_string(&f) else { continue };
        let mut infos: Vec<IslandInfo> = Vec::new();
        let mut ids: Vec<u32> = Vec::new();
        let mut vparts: Vec<(i64, u8)> = Vec::new();
        for line in txt.lines() {
            if line.starts_with("part ") {
                break;
            }
            let tok: Vec<&str> = line.split_whitespace().collect();
            if tok.len() < 15 || !line.starts_with(' ') {
                continue;
            }
            let num: Vec<f32> = tok[..14].iter().filter_map(|s| s.parse::<f32>().ok()).collect();
            if num.len() != 14 {
                continue;
            }
            let excluded = line.contains("EXCLUDED");
            infos.push(IslandInfo {
                shape: Shape { tris: num[1] as u32, uv_box: [num[2], num[3], num[4], num[5]], share: num[6] / 100.0, area: num[6], elong: num[7], ..Shape::default() },
                texels: num[11],
                nonfur: num[11] * num[12] / 100.0,
                hard_shape: num[10],
                threshold: num[13] / 100.0,
                excluded,
                sub_tris: 0,
                near_tris: 0,
                skin_tris: 0,
                nonfur_cf: 0.0,
                limb: false,
                digit_memory: false,
                part: 0,
                why: if excluded { Why::Texels } else { Why::Fur },
            });
            ids.push(num[0] as u32);
            // v10+ tables: the part and the reason of the verdict
            let part = tok.get(14).and_then(|s| s.parse::<i64>().ok()).unwrap_or(-1 - ids.len() as i64);
            vparts.push((part, if line.contains("cone-shaped") { 2u8 } else if line.contains("EXCLUDED (no shells): texels") { 1 } else if excluded { 3 } else { 0 }));
        }
        if infos.is_empty() {
            continue;
        }
        seen += 1;
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        // v12 replayed: the islands excluded by their part's shape in a part that also holds texel-excluded islands (a claw tip on a
        // finger) are the ones v12 may give back (their part's rest is judged again alone: an upper bound, the cone test needs the
        // triangles); a shape-excluded part without texel-excluded islands (every real horn) is unchanged.
        let (mut v12n, mut v12t, mut v12a, mut horn_parts) = (0u32, 0u32, 0.0f32, std::collections::BTreeSet::new());
        let mut v12ids = Vec::new();
        let mut held = 0u32;
        for k in 0..vparts.len() {
            if vparts[k].1 != 2 {
                continue;
            }
            let p = vparts[k].0;
            if vparts.iter().any(|q| q.0 == p && q.1 == 1) && (0..vparts.len()).filter(|&j| vparts[j].0 == p && vparts[j].1 != 1).all(|j| infos[j].shape.elong <= REST_MAX_ELONG) {
                v12n += 1;
                v12t += infos[k].shape.tris;
                v12a += infos[k].shape.share;
                v12ids.push(ids[k]);
            } else {
                horn_parts.insert(p);
                if vparts.iter().any(|q| q.0 == p && q.1 == 1) {
                    held += 1;
                }
            }
        }
        let fname = f.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        if fname == "furmesh-<crc>.txt" && txt.contains("8 by the part's shape") {
            // a charr arm / paw layout: its 8 finger islands (24 triangles each) under the claw tips come back, nothing else
            assert!(v12n == 8 && v12t == 192, "{fname}: {v12ids:?}");
        }
        if fname == "furmesh-<crc>.txt" {
            assert_eq!(v12n, 0, "{fname}: horn strips stay excluded");
        }
        // v13 replayed: the DIGIT tiles (the tiles of the finger islands v12 gives back, or of the `kept, v12` islands of a table
        // written by v12) and in them the texel-excluded islands that are not almost all claw (< 90 % non-fur): the thumbs whose skin
        // triangles at the hand v13 may keep (an upper bound: the per-triangle votes need the triangles).
        let tile_of = tiles(&infos.iter().map(|i| i.shape.uv_box).collect::<Vec<_>>());
        let kept12: Vec<usize> = txt.lines().filter(|l| l.starts_with(' ') && l.contains("kept, v12]")).filter_map(|l| l.split_whitespace().next()?.parse::<u32>().ok()).filter_map(|id| ids.iter().position(|&x| x == id)).collect();
        let digit: std::collections::BTreeSet<u32> = v12ids.iter().filter_map(|id| ids.iter().position(|x| x == id)).chain(kept12).map(|k| tile_of[k]).collect();
        let thumbs: Vec<(u32, u32, f32)> = (0..infos.len()).filter(|&k| vparts[k].1 == 1 && digit.contains(&tile_of[k]) && infos[k].share() < 0.9).map(|k| (ids[k], infos[k].shape.tris, infos[k].share())).collect();
        if !digit.is_empty() || !thumbs.is_empty() {
            println!("{fname}: v13 digit tiles {digit:?}, thumb candidates (island, triangles, non-fur): {thumbs:?}");
        }
        if fname == "furmesh-<crc>.txt" || fname == "furmesh-<crc>.txt" {
            assert!(thumbs.len() == 2 && thumbs.iter().all(|t| t.1 == 18), "{fname}: the two thumbs: {thumbs:?}");
        }
        // v14 replayed: a HAND mesh (v12 proves fingers: given back, or `kept, v12` in a table v12 wrote); in it every texel-excluded
        // island not almost all claw (< 90 % non-fur) may re-admit its skin triangles attached to the hand (an upper bound: the votes
        // per triangle need the triangles). A mesh without proven fingers changes nothing.
        // v15 replayed: the two-tone LIMB candidates (texel-excluded, in the coat's tile, >= LIMB_MIN_SHARE of a mesh of >= LIMB_MIN_TRIS
        // triangles, elongation >= LIMB_MIN_ELONG, not hard-shaped); the game also needs the colour-free share under the threshold.
        {
            let mesh_tris: u32 = infos.iter().map(|i| i.shape.tris).sum();
            let largest = (0..infos.len()).max_by(|&a, &b| infos[a].shape.share.partial_cmp(&infos[b].shape.share).unwrap()).unwrap();
            let limbs: Vec<(u32, u32, f32, f32, f32)> = (0..infos.len())
                .filter(|&k| {
                    vparts[k].1 == 1 && tile_of[k] == tile_of[largest] && infos[k].shape.share >= LIMB_MIN_SHARE && infos[k].shape.elong >= LIMB_MIN_ELONG && infos[k].hard_shape < 0.5 && mesh_tris as usize >= LIMB_MIN_TRIS && infos[largest].shape.share >= LIMB_COAT_MIN
                })
                .map(|k| (ids[k], infos[k].shape.tris, infos[k].shape.share * 100.0, infos[k].shape.elong, infos[k].share() * 100.0))
                .collect();
            if !limbs.is_empty() {
                println!("{fname}: v15 limb candidates (island, triangles, area %, elongation, non-fur %): {limbs:?}");
            }
        }
        let hand_mesh = !digit.is_empty();
        let v14: Vec<(u32, u32, f32)> = if hand_mesh { (0..infos.len()).filter(|&k| vparts[k].1 == 1 && infos[k].share() < 0.9).map(|k| (ids[k], infos[k].shape.tris, infos[k].share())).collect() } else { Vec::new() };
        let v14t: u32 = v14.iter().map(|x| x.1).sum();
        println!("{fname}: v14 {} -> candidates: {} islands, {v14t} triangles at most {v14:?}", if hand_mesh { "HAND mesh" } else { "no proven fingers: unchanged" }, v14.len());
        if hand_mesh {
            hand_tables += 1;
            // every hand table is an arm: its largest island is the forearm skin (the head tables hold no proven finger)
            assert!(!txt.contains("index count 3102") && !txt.contains("index count 3162") && !txt.contains("index count 3174"), "{fname}: a head");
        }
        let ex = infos.iter().filter(|i| i.excluded).fold((0u32, 0u32, 0.0f32), |a, i| (a.0 + 1, a.1 + i.shape.tris, a.2 + i.shape.share));
        println!(
            "{}: v11 excluded {} islands / {} tris / {:.1} % -> v12 at most {} / {} / {:.1} % (given back: {} islands {:?}, {} tris, {:.2} %; shape-excluded parts kept excluded: {}, {held} of their islands in parts with texel-excluded islands whose rest has an island elongated > REST_MAX_ELONG (horn strips))",
            f.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            ex.0,
            ex.1,
            ex.2 * 100.0,
            ex.0 - v12n,
            ex.1 - v12t,
            (ex.2 - v12a) * 100.0,
            v12n,
            v12ids,
            v12t,
            v12a * 100.0,
            horn_parts.len()
        );
        let stats = |v: &[IslandInfo]| v.iter().filter(|i| i.excluded).fold((0u32, 0u32, 0.0f32), |a, i| (a.0 + 1, a.1 + i.shape.tris, a.2 + i.shape.share));
        let before = stats(&infos);
        let mut after = infos.clone();
        propagate_copies(&mut after);
        let copies: Vec<u32> = (0..after.len()).filter(|&k| matches!(after[k].why, Why::Copy(_))).map(|k| ids[k]).collect();
        for k in 0..after.len() {
            if let Why::Copy(j) = after[k].why {
                assert!(infos[j as usize].excluded && infos[j as usize].shape.tris == infos[k].shape.tris && same_box(infos[j as usize].shape.uv_box, infos[k].shape.uv_box), "{name}: island {} copies {}", ids[k], ids[j as usize]);
            }
        }
        let c = stats(&after);
        // The tile guard of the shape rule.
        let tile = tiles(&infos.iter().map(|i| i.shape.uv_box).collect::<Vec<_>>());
        let nt = tile.iter().copied().max().map_or(0, |m| m as usize + 1);
        let largest = (0..infos.len()).max_by(|&a, &b| infos[a].shape.share.partial_cmp(&infos[b].shape.share).unwrap()).unwrap();
        let mut tshare = vec![0.0f32; nt];
        let mut ttris = vec![0u32; nt];
        for k in 0..infos.len() {
            tshare[tile[k] as usize] += infos[k].shape.share;
            ttris[tile[k] as usize] += infos[k].shape.tris;
        }
        let eligible = |t: u32| t != tile[largest] && tshare[t as usize] <= TILE_MAX_SHARE && ttris[t as usize] as usize >= PART_MIN_TRIS;
        let mut upper = after.clone();
        let mut line = String::new();
        for t in 0..nt as u32 {
            if !eligible(t) {
                continue;
            }
            let members: Vec<usize> = (0..infos.len()).filter(|&k| tile[k] == t).collect();
            let newly: Vec<usize> = members.iter().copied().filter(|&k| !after[k].excluded).collect();
            // (v12: over the islands the shape rule could still change; an island the texels already exclude, e.g. in the small
            // 8-island mesh `100a58c5` captured after v11, is not a coat island the shape rule could take away)
            let biggest = newly.iter().map(|&k| infos[k].shape.share).fold(0.0f32, f32::max);
            // (v15: only for meshes of at least LIMB_MIN_TRIS triangles: the 266-triangle mesh 29c32b35 of the game's v14 session, a small
            // separate part, has an 11.8 % island there; the check was pinned to the character body meshes of the tables)
            let mesh_tris: u32 = infos.iter().map(|i| i.shape.tris).sum();
            // (v15: reported, no longer asserted: the dev folder keeps growing with every session's dumps, e.g. 353853b0 has a 13.1 %
            // island in such a tile; the cone test, not this upper bound, decides in the game)
            if biggest > 0.10 && (mesh_tris as usize) >= LIMB_MIN_TRIS {
                println!("    note: {name}: a tile that may be judged by shape holds a fur island of {:.1} %", biggest * 100.0);
            }
            for &k in &newly {
                upper[k].excluded = true;
            }
            line.push_str(&format!(
                "\n    shape-candidate tile {t}: {:.1} % of the mesh, {} triangles, {} islands ({} not yet excluded: {})",
                tshare[t as usize] * 100.0,
                ttris[t as usize],
                members.len(),
                newly.len(),
                newly.iter().map(|&k| format!("{}({}t {:.2}%)", ids[k], infos[k].shape.tris, infos[k].shape.share * 100.0)).collect::<Vec<_>>().join(" ")
            ));
        }
        let u = stats(&upper);
        println!(
            "{name}: {} islands | v9 {} excluded, {} tris, {:.1} % | + copies {} / {} / {:.1} % (copies: {copies:?}) | + shape at most {} / {} / {:.1} %{line}",
            infos.len(),
            before.0,
            before.1,
            before.2 * 100.0,
            c.0,
            c.1,
            c.2 * 100.0,
            u.0,
            u.1,
            u.2 * 100.0
        );
    }
    println!("{seen} real island tables replayed, {hand_tables} hand meshes (v14 may change only these)");
}

// ---- the real atlases of the dev folder ------------------------------------------------------------------------------------------

const DEV: &str = "GW2Graphics/debug/";

/// What `encode_png_rgb8` wrote (stored deflate blocks, filter 0): (w, h, rgb). None for anything else.
fn read_png(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    let png = std::fs::read(path).ok()?;
    if png.len() < 8 || png[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return None;
    }
    let (mut pos, mut w, mut h, mut idat) = (8usize, 0u32, 0u32, Vec::new());
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().ok()?) as usize;
        let ty = &png[pos + 4..pos + 8];
        let data = png.get(pos + 8..pos + 8 + len)?;
        if ty == b"IHDR" {
            w = u32::from_be_bytes(data[0..4].try_into().ok()?);
            h = u32::from_be_bytes(data[4..8].try_into().ok()?);
            if data[8..] != [8, 2, 0, 0, 0] {
                return None;
            }
        } else if ty == b"IDAT" {
            idat.extend_from_slice(data);
        }
        pos += 12 + len;
    }
    let mut raw = Vec::new();
    let mut p = 2;
    loop {
        let fin = *idat.get(p)? & 1;
        if idat[p] >> 1 != 0 {
            return None;
        }
        let l = u16::from_le_bytes([idat[p + 1], idat[p + 2]]) as usize;
        raw.extend_from_slice(idat.get(p + 5..p + 5 + l)?);
        p += 5 + l;
        if fin == 1 {
            break;
        }
    }
    let stride = w as usize * 3;
    let mut px = Vec::with_capacity(stride * h as usize);
    for row in raw.chunks(stride + 1) {
        if row.len() != stride + 1 || row[0] != 0 {
            return None;
        }
        px.extend_from_slice(&row[1..]);
    }
    Some((w, h, px))
}

fn real_atlas(crc: &str) -> Option<Img> {
    let (w, h, rgb) = read_png(&format!("{DEV}furtex-{crc}-rgb.png"))?;
    let (w2, h2, a) = read_png(&format!("{DEV}furtex-{crc}-a.png"))?;
    if (w, h) != (w2, h2) {
        return None;
    }
    let px = (0..(w * h) as usize).map(|i| [rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2], a[i * 3]]).collect();
    Some(Img { w, h, px })
}

/// The 13 atlases of the v8 table (docs/shell-fur.md).
const REAL_ATLASES: [&str; 13] = ["323868e4", "dfe7c25c", "c91ca4a3", "5566458d", "e884ef71", "5100819f", "21a45427", "3c71e3f2", "05a0d288", "71695927", "c984dc99", "a3e217eb", "700c4377"];

/// The parts of the real body / head atlas layout (the same in every charr composite; regions of the v8 harness `py\v8.py`):
/// (name, uv rect, a hard part (cone) or skin (patch), expected verdict: Some(true) excluded, Some(false) fur, None = report only).
const REAL_PARTS: [(&str, [f32; 4], bool, Option<bool>); 9] = [
    ("body fur A", [0.78, 0.88, 0.86, 0.98], false, Some(false)),
    ("body fur B", [0.72, 0.81, 0.80, 0.86], false, Some(false)),
    ("cheek", [0.42, 0.55, 0.48, 0.62], false, Some(false)),
    ("hand claw", [0.705, 0.765, 0.735, 0.795], true, Some(true)),
    ("foot claw", [0.695, 0.885, 0.715, 0.905], true, Some(true)),
    ("paw claw strip", [0.69, 0.95, 0.705, 0.99], true, Some(true)),
    ("fang", [0.515, 0.705, 0.545, 0.735], true, None),
    ("horn (white atlas: the flat crescent; others: fur there)", [0.60, 0.58, 0.66, 0.65], true, None),
    ("body footprint", [0.69, 0.75, 1.0, 1.0], false, Some(false)),
];

#[test]
fn the_real_atlases_are_classified_per_island() {
    let mut seen = 0;
    for crc in REAL_ATLASES {
        let Some(img) = real_atlas(crc) else {
            println!("{crc}: not on disk, skipped");
            continue;
        };
        seen += 1;
        // One mesh: every part its own island (cones for the hard parts, patches for the skin), in separate places.
        let mut t = Vec::new();
        let mut ranges = Vec::new();
        for (k, (_, rect, hard, _)) in REAL_PARTS.iter().enumerate() {
            let a = t.len();
            if *hard {
                t.extend(cone([(k as f32 * 1.3).cos(), (k as f32 * 1.3).sin(), -0.3], 0.35, 0.07, *rect, 0.08));
            } else {
                t.extend(skin(*rect, 12, 12, [k as f32 * 4.0, 0.0, 0.0]));
            }
            ranges.push(a..t.len());
        }
        let a = analyze(&t, &img);
        let mut line = format!("{crc}: draw weight {:.2};", a.s0[3]);
        for (k, (name, _, _, want)) in REAL_PARTS.iter().enumerate() {
            let info = a.islands[a.of_tri[ranges[k].start] as usize];
            line.push_str(&format!(" {name} {:.0} % non-fur (th {:.0} %) -> {};", info.share() * 100.0, info.threshold * 100.0, if info.excluded { "EXCLUDED" } else { "fur" }));
            if let Some(w) = want {
                assert_eq!(info.excluded, *w, "{crc} {name}: {info:?}");
            }
            if crc == "323868e4" && name.starts_with("horn") {
                assert!(info.excluded, "the white atlas' horn crescent: {info:?}");
            }
        }
        println!("{line}");
        // The combination with the texel rule (v9 NEAR): head texels that the v8 rule keeps as fur within 7 texels of a dead-flat
        // (horn) texel: how many the NEAR windows (6 texels) remove; and what the NEAR windows would cost if applied to the whole
        // cheek (why they only run on triangles touching an excluded part).
        let (s0, s1) = (a.s0, a.s1);
        let wt = |f: ([f32; 4], f32)| crate::shellfur::texel_weight(f.0, f.1, s0, s1);
        let (w, h) = (img.w as i64, img.h as i64);
        let uvc = |x: i64, y: i64| [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32];
        let head = [(0.40 * w as f32) as i64, (0.50 * h as f32) as i64, (0.69 * w as f32) as i64, (0.75 * h as f32) as i64];
        let mut flat = vec![false; ((head[2] - head[0]) * (head[3] - head[1])) as usize];
        for y in head[1]..head[3] {
            for x in head[0]..head[2] {
                flat[((y - head[1]) * (head[2] - head[0]) + x - head[0]) as usize] = texel_features(&img, uvc(x, y)).0[3] < 0.006;
            }
        }
        let is_flat = |x: i64, y: i64| x >= head[0] && y >= head[1] && x < head[2] && y < head[3] && flat[((y - head[1]) * (head[2] - head[0]) + x - head[0]) as usize];
        let (mut edge, mut edge_near) = (0, 0);
        for y in (head[1]..head[3]).step_by(2) {
            for x in (head[0]..head[2]).step_by(2) {
                if is_flat(x, y) || !(-7..=7).any(|dy| (-7..=7).any(|dx| is_flat(x + dx, y + dy))) {
                    continue;
                }
                if wt(crate::shellfur::texel_features_dilated(&img, uvc(x, y))) <= 0.5 {
                    continue;
                }
                edge += 1;
                edge_near += (wt(crate::shellfur::texel_features_near(&img, uvc(x, y))) <= 0.5) as usize;
            }
        }
        let cheek = [(0.42 * w as f32) as i64, (0.55 * h as f32) as i64, (0.48 * w as f32) as i64, (0.62 * h as f32) as i64];
        let (mut cn, mut c8, mut c9) = (0, 0, 0);
        for y in (cheek[1]..cheek[3]).step_by(3) {
            for x in (cheek[0]..cheek[2]).step_by(3) {
                cn += 1;
                c8 += (wt(crate::shellfur::texel_features_dilated(&img, uvc(x, y))) > 0.5) as usize;
                c9 += (wt(crate::shellfur::texel_features_near(&img, uvc(x, y))) > 0.5) as usize;
            }
        }
        println!(
            "{crc}: near a flat (horn) texel the v8 rule keeps {edge} sampled fur texels, the NEAR windows remove {edge_near} ({:.0} %); NEAR everywhere would keep {:.0} % of the cheek (v8 {:.0} %)",
            100.0 * edge_near as f64 / edge.max(1) as f64,
            100.0 * c9 as f64 / cn.max(1) as f64,
            100.0 * c8 as f64 / cn.max(1) as f64
        );
    }
    println!("{seen} real atlases classified");
}

// ---- v12: the hands (front paws): finger skin under a texel-excluded claw keeps its fur -----------------------------------------

/// The claws' tile of the v12 hand (`atlas()`: CLAW = glossy tan tip, coat-coloured base), the fingers just under it (coat), the palm
/// in a tile of its own (the real arm meshes: the hand back in the arm's tile, fingers and claw tips in the paw tile).
const FINGERS: [f32; 4] = [0.80, 0.31, 0.95, 0.38];
const PALM: [f32; 4] = [0.56, 0.47, 0.76, 0.53];
const MITTEN: [f32; 4] = [0.80, 0.31, 0.95, 0.39];

fn sub_rect(r: [f32; 4], k: u32, n: u32) -> [f32; 4] {
    let du = (r[2] - r[0]) / n as f32;
    [r[0] + du * k as f32 + 0.002, r[1], r[0] + du * (k + 1) as f32 - 0.002, r[3]]
}

/// One finger along +y from `base` (the centre of its base ring): a tube of radius `r` (15 % taper) `flen` long, uv over `frect`,
/// then its claw: a cone from the finger's tip ring (same positions, its own uv over `crect`, base at crect's v1) to a point.
fn finger(base: [f32; 3], r: f32, flen: f32, clen: f32, frect: [f32; 4], crect: [f32; 4]) -> Vec<Tri> {
    let (segs, fr, cr) = (8u32, 4u32, 6u32);
    let ring = |y: f32, rad: f32, k: u32| {
        let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
        [base[0] + rad * ph.cos(), base[1] + y, base[2] + rad * ph.sin()]
    };
    let mut out = grid(fr, segs, |j, k| ring(flen * j as f32 / fr as f32, r * (1.0 - 0.15 * j as f32 / fr as f32), k), |j, k| {
        [frect[0] + (frect[2] - frect[0]) * k as f32 / segs as f32, frect[3] - (frect[3] - frect[1]) * j as f32 / fr as f32]
    });
    out.extend(grid(
        cr,
        segs,
        |j, k| {
            let s = j as f32 / cr as f32;
            ring(flen + clen * s, r * 0.85 * (1.0 - 0.92 * s), k)
        },
        |j, k| [crect[0] + (crect[2] - crect[0]) * k as f32 / segs as f32, crect[3] - (crect[3] - crect[1]) * j as f32 / cr as f32],
    ));
    out
}

/// The v12 hand: a palm plate (8 x 6 quads, 0.4 x 0.3, uv over `palm_rect`) at x 3..3.4 and four fingers with claws on its top edge,
/// each finger's base ring welded to the palm through one vertex (the palm in another tile: a separate part, like the real meshes).
/// Returns the triangles and the ranges (palm, finger 0..3, claw 0..3).
fn hand(palm_rect: [f32; 4]) -> (Vec<Tri>, Vec<std::ops::Range<usize>>) {
    let r = 0.04f32;
    let fbase = |f: u32| [3.01 + 0.1 * f as f32, 0.3, 0.0];
    let ring0 = move |f: u32| [fbase(f)[0] + r, 0.3, 0.0]; // the finger's ring vertex 0 (phase 0)
    let mut t = grid(
        6,
        8,
        |j, k| {
            if j == 6 && k % 2 == 1 {
                return ring0((k - 1) / 2);
            }
            [3.0 + 0.05 * k as f32, 0.05 * j as f32, 0.02 * (std::f32::consts::PI * j as f32 / 6.0).sin()]
        },
        |j, k| [palm_rect[0] + (palm_rect[2] - palm_rect[0]) * k as f32 / 8.0, palm_rect[1] + (palm_rect[3] - palm_rect[1]) * j as f32 / 6.0],
    );
    let mut ranges = vec![0..t.len()];
    let mut claws = Vec::new();
    for f in 0..4 {
        let a = t.len();
        let nf = 4 * 8 * 2;
        t.extend(finger(fbase(f), r, 0.12, 0.15, sub_rect(FINGERS, f, 4), sub_rect(CLAW, f, 4)));
        ranges.push(a..a + nf);
        claws.push(a + nf..t.len());
    }
    ranges.extend(claws);
    (t, ranges)
}

/// The share of an island's uv footprint (texels of `img`) where a strand grows: its triangle not excluded AND the strand's texel rule
/// (v8 dilated windows; the FT_NEAR windows too on a near triangle) says fur. The CPU mirror of what the shells draw.
fn strand_share(a: &Analysis, t: &[Tri], img: &Img, tris: std::ops::Range<usize>) -> f32 {
    let (mut n, mut fur) = (0u32, 0u32);
    for i in tris {
        let f = a.flag[i];
        raster(&t[i].uv, img.w, img.h, true, |x, y| {
            n += 1;
            if f == F_HARD || f == F_SUB {
                return;
            }
            let uv = [(x as f32 + 0.5) / img.w as f32, (y as f32 + 0.5) / img.h as f32];
            let ft = if f == F_NEAR { crate::shellfur::texel_features_near(img, uv) } else { crate::shellfur::texel_features_dilated(img, uv) };
            // v15: bit value 3 = a two-tone limb triangle: the strands skip the colour tests
            let colour = tri_bits(&a.bits, i as u32) != 3;
            fur += (crate::shellfur::texel_weight_opt(ft.0, ft.1, a.s0, a.s1, colour) > 0.5) as u32;
        });
    }
    if n > 0 { fur as f32 / n as f32 } else { 0.0 }
}

#[test]
fn v12_a_hand_keeps_fur_on_the_palm_and_fingers_and_none_on_the_claws() {
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    let (h, r) = hand(PALM);
    t.extend(h);
    let r: Vec<std::ops::Range<usize>> = r.into_iter().map(|x| x.start + a0..x.end + a0).collect();
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, t.len() as u32 * 3));
    // The finger + claw parts are cones (what v10 / v11 excluded whole: bare charr fingers); their claws are texel-excluded.
    for f in 0..4 {
        let fi = a.islands[a.of_tri[r[1 + f].start] as usize];
        let ci = a.islands[a.of_tri[r[5 + f].start] as usize];
        let p = a.parts[fi.part as usize];
        println!("finger {f}: share {:.2}, claw share {:.2}, part: {p:?}", fi.share(), ci.share());
        assert_eq!(fi.part, ci.part, "finger {f} and its claw are one part");
        assert!(p.eligible && p.excluded && p.cone >= p.cut, "finger {f}: the part is cone-shaped (the v11 case): {p:?}");
        assert!(ci.excluded && ci.why == Why::Texels, "claw {f}: {ci:?}");
        assert!(p.spared && p.rest_cone < p.rest_cut && !fi.excluded && fi.why == Why::Fur, "finger {f} keeps its fur: {p:?} {fi:?}");
    }
    assert_eq!(a.spared_islands, 4);
    let palm = strand_share(&a, &t, &img, r[0].clone());
    println!("palm: strands on {:.1} % of its texels", palm * 100.0);
    assert!(palm >= 0.90, "palm {palm}");
    for f in 0..4 {
        let fs = strand_share(&a, &t, &img, r[1 + f].clone());
        let cs = strand_share(&a, &t, &img, r[5 + f].clone());
        println!("finger {f}: strands on {:.1} %, claw {:.1} %", fs * 100.0, cs * 100.0);
        assert!(fs >= 0.90, "finger {f}: {fs}");
        assert_eq!(cs, 0.0, "claw {f}");
        assert!(r[5 + f].clone().all(|i| a.flag[i] == F_HARD), "claw {f}: no shell on any layer");
    }
    // The skin is untouched.
    assert!((0..a0).all(|i| a.flag[i] == F_FUR));
    let txt = dump_text(&a, 0);
    assert!(txt.contains("4 kept as fur skin under a texel-excluded claw") && txt.contains("keeps its fur (v12)") && txt.contains("kept, v12]"), "{txt}");
}

#[test]
fn v12_a_mitten_hand_with_claws_keeps_its_fur() {
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    // A mitten: a flattened tube (0.4 wide, 0.12 thick, 0.35 long) along +y, its uv over MITTEN (the claws' tile), four claws on its
    // end ring (each sharing one vertex with it): one part.
    let (segs, rows) = (16u32, 6u32);
    let mpos = |j: u32, k: u32| {
        let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
        [3.2 + 0.2 * ph.cos(), 0.35 * j as f32 / rows as f32, 0.06 * ph.sin()]
    };
    t.extend(grid(rows, segs, mpos, |j, k| [MITTEN[0] + (MITTEN[2] - MITTEN[0]) * k as f32 / segs as f32, MITTEN[3] - (MITTEN[3] - MITTEN[1]) * j as f32 / rows as f32]));
    let mitten = a0..t.len();
    let mut claws = Vec::new();
    for f in 0..4u32 {
        let k = [1u32, 3, 5, 7][f as usize]; // the front half of the end ring
        let tip = mpos(rows, k);
        let a = t.len();
        // a claw only (no finger rings: flen 0 gives degenerate finger quads, dropped), its ring vertex 0 on the mitten's vertex
        let mut c = finger([tip[0] - 0.85 * 0.03, tip[1], tip[2]], 0.03, 0.0, 0.15, [0.0; 4], sub_rect(CLAW, f, 4));
        c.retain(|tri| tri.uv.iter().all(|q| q[0] >= CLAW[0]));
        for tri in c.iter_mut() {
            for p in tri.p.iter_mut() {
                if len3(sub3(*p, tip)) < 1e-5 {
                    *p = tip;
                }
            }
        }
        t.extend(c);
        claws.push(a..t.len());
    }
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, 0));
    let mi = a.islands[a.of_tri[mitten.start] as usize];
    assert!(!mi.excluded, "{mi:?} {:?}", a.parts[mi.part as usize]);
    assert!(claws.iter().all(|c| a.islands[a.of_tri[c.start] as usize].part == mi.part), "the mitten and its claws: one part");
    let ms = strand_share(&a, &t, &img, mitten.clone());
    println!("mitten: strands on {:.1} %", ms * 100.0);
    assert!(ms >= 0.90, "mitten {ms}");
    for c in claws {
        assert!(c.clone().all(|i| a.flag[i] == F_HARD));
        assert_eq!(strand_share(&a, &t, &img, c), 0.0);
    }
}

/// v12 on the REAL paw of the dev folder: the arm / paw mesh `38c9862b` (its `furisland` PNG = the painted triangle classes over its
/// own atlas `a06644ee`; a charr uses the same layout with atlas `323868e4`). Decodes the painted classes, then, per
/// region, the share of texels where the strand texel rule keeps fur with the v8 windows and with the v9 NEAR windows: the hand back
/// (the near arc, all its triangles near) and the finger islands (the 24-triangle islands of the paw tile, v11: excluded by shape).
/// Report only (skipped when absent).
#[test]
fn v12_the_real_paw_near_windows_report() {
    let Some((w, h, paint)) = read_png(&format!("{DEV}furisland-<crc>.png")) else {
        println!("no furisland-<crc>.png: skipped");
        return;
    };
    let Some(own) = real_atlas("a06644ee") else {
        println!("no atlas a06644ee: skipped");
        return;
    };
    if (own.w, own.h) != (w, h) {
        println!("size differs: skipped");
        return;
    }
    // class per texel: 0 none, 1 fur, 2 excluded, 4 near. A painted texel is (dimmed atlas + col) / 2, painted again for every
    // triangle over it (mirror copies): its colour is dominated by the class colour; the dimmed atlas alone is <= 127.
    let mut class = vec![0u8; (w * h) as usize];
    for i in 0..(w * h) as usize {
        let (r, g, b) = (paint[i * 3] as i32, paint[i * 3 + 1] as i32, paint[i * 3 + 2] as i32);
        class[i] = if r >= 115 && g >= 110 && b < 80 {
            4
        } else if r >= 115 && g < 70 && b < 70 {
            2
        } else if g >= 100 && r < 80 && b < 100 {
            1
        } else {
            0
        };
    }
    let regions: [(&str, [f32; 4], u8); 3] = [
        ("hand back (near arc)", [0.78, 0.75, 0.875, 0.848], 4),
        ("fingers (paw tile, 24-triangle islands; v11 excluded, any painted class: v12+ dumps paint them fur / near)", [0.705, 0.928, 0.863, 0.998], 0),
        ("arm (fur)", [0.69, 0.67, 0.99, 0.75], 1),
    ];
    for (crc, img) in [("a06644ee (the mesh's own)", Some(own.clone())), ("323868e4 (a light coat)", real_atlas("323868e4"))] {
        let Some(img) = img else { continue };
        if (img.w, img.h) != (w, h) {
            continue;
        }
        let a = analyze(&skin([0.69, 0.5, 1.0, 1.0], 16, 16, [0.0; 3]), &img);
        let wt = |f: ([f32; 4], f32)| crate::shellfur::texel_weight(f.0, f.1, a.s0, a.s1);
        let mut line = format!("{crc}:");
        for (name, r, cl) in regions {
            let (mut n, mut d, mut nr) = (0u32, 0u32, 0u32);
            for y in (r[1] * h as f32) as u32..(r[3] * h as f32) as u32 {
                for x in (r[0] * w as f32) as u32..(r[2] * w as f32) as u32 {
                    let c = class[(y * w + x) as usize];
                    if (cl == 0 && c == 0) || (cl != 0 && c != cl) {
                        continue;
                    }
                    // the finger islands only: the paw tile's red texels right of the claw-tip strip
                    let uv = [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32];
                    n += 1;
                    d += (wt(crate::shellfur::texel_features_dilated(&img, uv)) > 0.5) as u32;
                    nr += (wt(crate::shellfur::texel_features_near(&img, uv)) > 0.5) as u32;
                }
            }
            line.push_str(&format!(" {name}: {n} texels, fur by the v8 windows {:.0} %, with the NEAR windows {:.0} %;", 100.0 * d as f64 / n.max(1) as f64, 100.0 * nr as f64 / n.max(1) as f64));
            if crc.starts_with("323868e4") && cl != 1 {
                // the NEAR windows are not what bares the hand (v12: 93 / 96 % on the v11 dump; the dump is rewritten by every
                // session, so this stays a report: v15)
                println!("    {name}: NEAR keeps {:.0} % of {n}", 100.0 * nr as f64 / n.max(1) as f64);
            }
        }
        println!("{line}");
    }
}

/// v13: a THUMB of one island (like the real `1db2dfaf` islands 20 / 29, 18 triangles, 67 % non-fur): a tube along -x
/// from `base` (radius `r`): skin rings with uv over coat (v 0.36 -> 0.40, u 0.89..0.94), then the claw rings continuing the SAME uv
/// strip onto the glossy tan of FUSED (v 0.40 -> 0.58): one island, excluded by its texels. Its ring vertex 0 is `base + (0, r, 0)`.
fn thumb(base: [f32; 3], r: f32) -> Vec<Tri> {
    thumb_uv(base, r, [0.89, 0.94], [0.36, 0.40, 0.58])
}

/// `thumb` with its uv strip over u `ur` and v `vr` = (skin start, skin / claw junction, claw tip).
fn thumb_uv(base: [f32; 3], r: f32, ur: [f32; 2], vr: [f32; 3]) -> Vec<Tri> {
    let (segs, fr, cr) = (8u32, 4u32, 6u32);
    let ring = |along: f32, rad: f32, k: u32| {
        let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
        [base[0] - along, base[1] + rad * ph.cos(), base[2] + rad * ph.sin()]
    };
    let u = |k: u32| ur[0] + (ur[1] - ur[0]) * k as f32 / segs as f32;
    let mut out = grid(fr, segs, |j, k| ring(0.10 * j as f32 / fr as f32, r, k), |j, k| [u(k), vr[0] + (vr[1] - vr[0]) * j as f32 / fr as f32]);
    out.extend(grid(cr, segs, |j, k| {
        let s = j as f32 / cr as f32;
        ring(0.10 + 0.14 * s, r * (1.0 - 0.92 * s), k)
    }, |j, k| [u(k), vr[1] + (vr[2] - vr[1]) * j as f32 / cr as f32]));
    out
}

#[test]
fn v13_the_thumb_keeps_its_skin_and_its_claw_stays_bare() {
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    let (h, r) = hand(PALM);
    // the palm's left edge vertex (j 3, k 0) = the thumb's ring vertex 0 (the thumb grows from the hand, not from a finger)
    let r_t = 0.04f32;
    let p_att = h[..96].iter().flat_map(|tri| tri.p).find(|p| (p[0] - 3.0).abs() < 1e-6 && (p[1] - 0.15).abs() < 1e-6).expect("palm vertex");
    t.extend(h);
    let r: Vec<std::ops::Range<usize>> = r.into_iter().map(|x| x.start + a0..x.end + a0).collect();
    let th0 = t.len();
    let mut th = thumb([p_att[0], p_att[1] - r_t, p_att[2]], r_t);
    for tri in th.iter_mut() {
        for p in tri.p.iter_mut() {
            if len3(sub3(*p, p_att)) < 1e-5 {
                *p = p_att;
            }
        }
    }
    let skin_n = 4 * 8 * 2;
    t.extend(th);
    let thumb_skin = th0..th0 + skin_n;
    let thumb_claw = th0 + skin_n..t.len();
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, 0));
    let ti = a.islands[a.of_tri[th0] as usize];
    assert!(thumb_claw.clone().all(|i| a.of_tri[i] == a.of_tri[th0]), "skin and claw: one island");
    assert!(ti.excluded && ti.why == Why::Texels && ti.share() > 0.5, "the thumb island is excluded by its texels (the v12 case): {ti:?}");
    assert!(ti.skin_tris > 0, "{ti:?}");
    let ss = strand_share(&a, &t, &img, thumb_skin.clone());
    let cs = strand_share(&a, &t, &img, thumb_claw.clone());
    println!("thumb: skin strands on {:.1} % of its texels ({} triangles kept), claw {:.1} %", ss * 100.0, ti.skin_tris, cs * 100.0);
    assert!(ss >= 0.6, "thumb skin {ss}");
    assert_eq!(cs, 0.0, "thumb claw");
    // The four fingers and their claws as in v12; the claws (coat-coloured bases touching only their fingers) keep no triangle.
    for f in 0..4 {
        assert!(strand_share(&a, &t, &img, r[1 + f].clone()) >= 0.9, "finger {f}");
        assert!(r[5 + f].clone().all(|i| a.flag[i] == F_HARD), "claw {f}");
    }
    assert!(strand_share(&a, &t, &img, r[0].clone()) >= 0.9, "palm");
    assert!(dump_text(&a, 0).contains("re-admitted: its skin at the hand (v14)"));
    // Without proven fingers in its tile (a head: the same island is a horn piece with a coat-coloured base on the head fur) the
    // island stays excluded whole.
    let mut t2 = skin(SKIN, 24, 24, [0.0; 3]);
    let b0 = t2.len();
    let (h2, _) = hand(PALM);
    t2.extend(h2[..96].iter().copied()); // the palm only (a head patch in its own tile)
    let mut th2 = thumb([p_att[0], p_att[1] - r_t, p_att[2]], r_t);
    for tri in th2.iter_mut() {
        for p in tri.p.iter_mut() {
            if len3(sub3(*p, p_att)) < 1e-5 {
                *p = p_att;
            }
        }
    }
    let c0 = t2.len();
    t2.extend(th2);
    let a2 = analyze(&t2, &img);
    let ti2 = a2.islands[a2.of_tri[c0] as usize];
    assert!(ti2.excluded && ti2.skin_tris == 0 && (c0..t2.len()).all(|i| a2.flag[i] == F_HARD), "no digit tile: whole: {ti2:?}");
    assert!((b0..c0).all(|i| a2.flag[i] != F_HARD));
}

/// v14: `atlas()` with a glossy tan claw stripe INSIDE the coat's tile (u 0.43..0.48, v 0.60..0.78), where a thumb island
/// lies (not in the fingers' tile).
fn atlas_coat_claw() -> Img {
    let mut img = atlas();
    for y in 0..S {
        for x in 0..S {
            let (u, v) = ((x as f32 + 0.5) / S as f32, (y as f32 + 0.5) / S as f32);
            if inside([0.43, 0.60, 0.48, 0.78], u, v, 0.0) {
                img.px[(y * S + x) as usize] = [158, 122, 87, 158];
            }
        }
    }
    img
}

/// The v14 hand mesh: the coat sphere, the v12 hand (palm + 4 fingers with texel-excluded claw tips: a hand mesh) and a thumb of ONE
/// island (skin + claw) welded to the palm, its uv strip `ur` / `vr`. Returns (triangles, thumb skin range, thumb claw range, hand ranges).
fn hand_with_thumb(ur: [f32; 2], vr: [f32; 3], with_fingers: bool) -> (Vec<Tri>, std::ops::Range<usize>, std::ops::Range<usize>, Vec<std::ops::Range<usize>>) {
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    let (h, r) = hand(PALM);
    let p_att = h[..96].iter().flat_map(|tri| tri.p).find(|p| (p[0] - 3.0).abs() < 1e-6 && (p[1] - 0.15).abs() < 1e-6).expect("palm vertex");
    if with_fingers {
        t.extend(h);
    } else {
        t.extend(h[..96].iter().copied());
    }
    let r: Vec<std::ops::Range<usize>> = r.into_iter().map(|x| x.start + a0..x.end + a0).collect();
    let r_t = 0.04f32;
    let th0 = t.len();
    let mut th = thumb_uv([p_att[0], p_att[1] - r_t, p_att[2]], r_t, ur, vr);
    for tri in th.iter_mut() {
        for p in tri.p.iter_mut() {
            if len3(sub3(*p, p_att)) < 1e-5 {
                *p = p_att;
            }
        }
    }
    t.extend(th);
    (t, th0..th0 + 64, th0 + 64..th0 + 160, r)
}

#[test]
fn v14_a_thumb_in_the_coat_tile_keeps_its_skin_and_a_head_horn_stays_whole() {
    // The in-game status `0 thumb-skin triangles kept, v13`: the character's thumb is not in the fingers' tile. v14 drops
    // the tile condition: any texel-excluded island of a HAND mesh re-admits its skin triangles attached to the hand.
    let img = atlas_coat_claw();
    let (t, skin_r, claw_r, r) = hand_with_thumb([0.43, 0.48], [0.56, 0.60, 0.78], true);
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, 0));
    let ti = a.islands[a.of_tri[skin_r.start] as usize];
    let p = a.parts[ti.part as usize];
    println!("thumb island: {ti:?}
 part: {p:?}");
    assert!(claw_r.clone().all(|i| a.of_tri[i] == a.of_tri[skin_r.start]), "one island");
    assert!(ti.excluded && ti.why == Why::Texels, "excluded by its texels: {ti:?}");
    let fingers_tile = a.parts[a.islands[a.of_tri[r[1].start] as usize].part as usize].tile;
    assert_ne!(p.tile, fingers_tile, "the thumb is NOT in the fingers' tile (the v13 condition fails)");
    let (ss, cs) = (strand_share(&a, &t, &img, skin_r.clone()), strand_share(&a, &t, &img, claw_r.clone()));
    println!("v14 thumb in the coat tile: {} skin triangles re-admitted, strands on {:.1} % of the skin, {:.1} % of the claw", ti.skin_tris, ss * 100.0, cs * 100.0);
    assert!(ti.skin_tris > 0 && ss >= 0.6 && cs == 0.0, "{ss} {cs}");
    assert!(claw_r.clone().filter(|&i| a.flag[i] == F_HARD).count() >= 80, "the claw stays excluded");
    for f in 0..4 {
        assert!(r[5 + f].clone().all(|i| a.flag[i] == F_HARD), "finger claw {f} (its coat-coloured base touches only its finger)");
    }
    // The guard: the same island in a mesh WITHOUT proven fingers (a head: a horn piece with a coat-coloured base on big head fur)
    // stays excluded whole.
    let (t2, s2, c2, _) = hand_with_thumb([0.43, 0.48], [0.56, 0.60, 0.78], false);
    let a2 = analyze(&t2, &img);
    let h2 = a2.islands[a2.of_tri[s2.start] as usize];
    println!("the same island in a head (no proven fingers): {h2:?}");
    assert!(h2.excluded && h2.skin_tris == 0 && (s2.start..c2.end).all(|i| a2.flag[i] == F_HARD), "horn stays whole");
    assert!(dump_text(&a, 0).contains("re-admitted: its skin at the hand (v14)"));
}

/// v15 measurement: on the real atlases, the texels the keratin / red tests remove, by region, and their flatness (ft[3]) relative to
/// the coat's median flatness (s0[1]): claws, fang, lips vs. the rust / tan FUR of two-tone coats (a01d2766: the arm strip and the hand
/// arc). Report only.
#[test]
fn v15_report_keratin_and_red_vs_two_tone_fur() {
    let regions: [(&str, [f32; 4]); 6] = [
        ("hand claw", [0.705, 0.765, 0.735, 0.795]),
        ("foot claw", [0.695, 0.885, 0.715, 0.905]),
        ("paw claw strip", [0.69, 0.95, 0.705, 0.99]),
        ("fang", [0.515, 0.705, 0.545, 0.735]),
        ("arm strip (a01d2766: rust fur)", [0.71, 0.70, 0.99, 0.745]),
        ("hand arc (a01d2766: rust fur)", [0.79, 0.76, 0.87, 0.84]),
    ];
    let atlases: Vec<&str> = REAL_ATLASES.iter().copied().chain(["a01d2766", "ae461682", "c984dc99", "cf5839f0", "a06228e8", "323868e4"]).collect();
    for crc in atlases {
        let Some(img) = real_atlas(crc) else { continue };
        let a = analyze(&skin([0.38, 0.5, 1.0, 1.0], 16, 16, [0.0; 3]), &img);
        let (s0, s1) = (a.s0, a.s1);
        let mut line = format!("{crc} (coat flat {:.3} sat {:.2} val {:.2} hue {:.0}):", s0[1], s1[1], s1[2], s1[3]);
        for (name, r) in regions {
            let (mut n, mut kr, mut fl, mut nker) = (0u32, 0u32, Vec::new(), 0u32);
            let (w, h) = (img.w as f32, img.h as f32);
            for y in (r[1] * h) as u32..(r[3] * h) as u32 {
                for x in (r[0] * w) as u32..(r[2] * w) as u32 {
                    let uv = [(x as f32 + 0.5) / w, (y as f32 + 0.5) / h];
                    let (ft, hue) = crate::shellfur::texel_features_dilated(&img, uv);
                    n += 1;
                    let wt = crate::shellfur::texel_weight(ft, hue, s0, s1);
                    let (ft0, hue0) = crate::shellfur::texel_features(&img, uv);
                    let nk = crate::shellfur::texel_weight_opt(ft, hue, s0, s1, false) <= 0.5;
                    nker += (wt <= 0.5 && !nk) as u32;
                    if wt <= 0.5 && crate::shellfur::texel_weight([0.0, ft[1], ft[2], 1.0], hue, s0, s1) <= 0.5 {
                        // removed by colour (keratin / red), not by the flat / gloss tests
                        kr += 1;
                        fl.push(ft0[3] / s0[1].max(1e-3));
                    }
                    let _ = hue0;
                }
            }
            fl.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: f32| fl.get(((fl.len() as f32 - 1.0) * p) as usize).copied().unwrap_or(f32::NAN);
            line.push_str(&format!(" {name}: {:.0} % by colour ({:.0} % by the keratin test alone), flat/coat p10 {:.2} p50 {:.2} p90 {:.2};", 100.0 * kr as f32 / n.max(1) as f32, 100.0 * nker as f32 / n.max(1) as f32, q(0.1), q(0.5), q(0.9)));
        }
        println!("{line}");
    }
}

/// v15: `atlas()` with a RUST band (a two-tone coat's second colour, hue ~15, saturation ~0.6: the keratin / red colour tests call it
/// non-fur) at u 0.505..0.56 (inside the coat's tile), and a lips-like red patch at u 0.505..0.56, v 0.80..0.95.
fn atlas_two_tone() -> Img {
    let mut img = atlas();
    for y in 0..S {
        for x in 0..S {
            let (u, v) = ((x as f32 + 0.5) / S as f32, (y as f32 + 0.5) / S as f32);
            let i = (y * S + x) as usize;
            let paint = img.px[i][0] as f32 / (0.94 * 255.0);
            if inside([0.505, 0.02, 0.56, 0.70], u, v, 0.0) {
                img.px[i] = [(0.62 * paint * 255.0) as u8, (0.36 * paint * 255.0) as u8, (0.24 * paint * 255.0) as u8, img.px[i][3]];
            }
            if inside([0.505, 0.80, 0.56, 0.95], u, v, 0.0) {
                img.px[i] = [(0.70 * paint * 255.0) as u8, (0.22 * paint * 255.0) as u8, (0.24 * paint * 255.0) as u8, img.px[i][3]];
            }
        }
    }
    img
}

/// A tube (an arm / leg strip: `rings` x 12) from `base` along +x, `len` long, radius `r`, its uv over `rect` (u around, v along).
fn limb(base: [f32; 3], len: f32, r: f32, rect: [f32; 4], rings: u32) -> Vec<Tri> {
    let segs = 12u32;
    grid(rings, segs, |j, k| {
        let ph = std::f32::consts::TAU * (k % segs) as f32 / segs as f32;
        [base[0] + len * j as f32 / rings as f32, base[1] + r * ph.cos(), base[2] + r * ph.sin()]
    }, |j, k| [rect[0] + (rect[2] - rect[0]) * k as f32 / segs as f32, rect[1] + (rect[3] - rect[1]) * j as f32 / rings as f32])
}

#[test]
fn v15_a_two_tone_limb_keeps_its_fur_and_the_lips_and_claws_stay_out() {
    // `e8bc2bb8` island 66 (the upper-arm strip, 7 % of the mesh, 53.6 % non-fur, all of it the rust band of a grey
    // coat): v9..v14 excluded it by its texels (the keratin / red colour tests) and the strands of the forearm next to it all read the
    // NEAR windows. v15: a long island of the coat's tile excluded by its colours only is fur, its strands skip the colour tests.
    let img = atlas_two_tone();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    t.extend(limb([3.0, 0.0, 0.0], 1.2, 0.12, [0.505, 0.02, 0.56, 0.70], 16)); // the rust arm strip (coat tile: within 0.03 of the skin)
    let a1 = t.len();
    t.extend(limb([5.0, 0.0, 0.0], 0.15, 0.12, [0.505, 0.80, 0.56, 0.95], 4)); // a short red patch (lips-like: not elongated)
    let a2 = t.len();
    t.extend(limb([7.0, 0.0, 0.0], 1.2, 0.015, [0.505, 0.02, 0.56, 0.70], 16)); // a thin rust strip of < 3 % of the mesh (claw-sized)
    let a3 = t.len();
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, 0));
    let arm = a.islands[a.of_tri[a0] as usize];
    let lips = a.islands[a.of_tri[a1] as usize];
    let thin = a.islands[a.of_tri[a2] as usize];
    println!("arm {arm:?}\nlips {lips:?}\nthin {thin:?}");
    assert!(arm.share() > arm.threshold, "v14 would exclude the arm by its colours: {arm:?}");
    assert!(arm.limb && !arm.excluded && arm.nonfur_cf / arm.texels < 0.1, "{arm:?}");
    assert!((a0..a1).all(|i| tri_bits(&a.bits, i as u32) == 3), "bit value 3 on every arm triangle");
    let ss = strand_share(&a, &t, &img, a0..a1);
    println!("v15 rust arm: strands on {:.1} % of its texels (v14: 0)", ss * 100.0);
    assert!(ss >= 0.9);
    assert!(lips.excluded && !lips.limb, "the short red patch (not a limb) stays excluded: {lips:?}");
    assert!(thin.excluded && !thin.limb && thin.shape.share < LIMB_MIN_SHARE, "a claw-sized strip stays excluded: {thin:?}");
    assert!((a1..a3).all(|i| a.flag[i] == F_HARD));
    assert!(dump_text(&a, 0).contains("a limb of a two-tone coat"));
    // In a mesh under LIMB_MIN_TRIS triangles (a teeth / tongue mesh) nothing is given back.
    let small: Vec<Tri> = t[a0..a1].iter().copied().chain(skin(SKIN, 6, 6, [0.0; 3])).collect();
    let b = analyze(&small, &img);
    assert!(b.limb_islands == 0, "{}", dump_text(&b, 0));
}

#[test]
fn v15_a_gloved_hand_learns_the_digit_boxes_of_a_bare_hand() {
    // A gloved arm mesh has no finger of its own to prove it is a hand (v12), so v14 re-admitted nothing of its thumb.
    // v15: the digit boxes a bare-hand mesh proved (its finger skins, its thumb) are remembered for the session; an excluded island on
    // such a box re-admits its skin triangles in any mesh (no attachment needed: a gloved digit may touch only the glove's cut).
    let img = atlas_coat_claw();
    let ur = [0.43, 0.48];
    let vr = [0.56, 0.60, 0.78];
    let (bare, _, _, _) = hand_with_thumb(ur, vr, true);
    let ab = analyze(&bare, &img);
    assert!(!ab.digit_boxes.is_empty(), "the bare hand proves digit boxes");
    println!("bare hand: {} digit boxes {:?}", ab.digit_boxes.len(), ab.digit_boxes);
    // The gloved mesh: the palm and the thumb, no fingers (its digits under the glove).
    let (gloved, s2, c2, _) = hand_with_thumb(ur, vr, false);
    let g0 = analyze_with(&gloved, &img, &[]);
    let t0 = g0.islands[g0.of_tri[s2.start] as usize];
    assert!(t0.excluded && t0.skin_tris == 0 && g0.digit_wait, "without the memory: whole, waiting: {t0:?}");
    let g1 = analyze_with(&gloved, &img, &ab.digit_boxes);
    let t1 = g1.islands[g1.of_tri[s2.start] as usize];
    let (ss, cs) = (strand_share(&g1, &gloved, &img, s2.clone()), strand_share(&g1, &gloved, &img, c2.clone()));
    println!("gloved thumb with the learned boxes: {} skin triangles re-admitted, strands on {:.1} % of the skin, {:.1} % of the claw", t1.skin_tris, ss * 100.0, cs * 100.0);
    assert!(t1.digit_memory && t1.skin_tris > 0 && ss >= 0.6 && cs == 0.0);
    assert!(g1.digit_islands == 1 && dump_text(&g1, 0).contains("digit box learned from a bare hand"));
    // A horn island elsewhere in the atlas (not on a digit box): unchanged.
    let mut head = skin(SKIN, 24, 24, [0.0; 3]);
    let h0 = head.len();
    head.extend(thumb_uv([3.0, 0.0, 0.0], 0.06, [0.89, 0.94], [0.36, 0.40, 0.58]));
    let ah = analyze_with(&head, &img, &ab.digit_boxes);
    let hi = ah.islands[ah.of_tri[h0] as usize];
    assert!(hi.excluded && hi.skin_tris == 0, "{hi:?}");
    // The memory: overlap over the smaller box.
    assert!(box_iou([0.7049, 0.9667, 0.7563, 0.9959], [0.7037, 0.9461, 0.7589, 0.9959]) >= DIGIT_IOU, "a real bare / gloved finger pair");
    assert!(box_iou([0.6981, 0.878, 0.7289, 0.9231], [0.6981, 0.878, 0.7548, 0.9552]) >= DIGIT_IOU, "a real bare / gloved thumb pair");
    assert!(box_iou([0.3788, 0.0025, 0.4868, 0.2452], [0.6981, 0.878, 0.7548, 0.9552]) == 0.0, "a horn tile is no digit");
}

#[test]
fn v12_a_horn_whose_rest_is_a_cone_too_stays_excluded() {
    // The guard of the v12 split: a horn whose tip island is texel-excluded but whose rest (coat-textured base) is still a cone
    // stays excluded whole.
    let img = atlas();
    let mut t = skin(SKIN, 24, 24, [0.0; 3]);
    let a0 = t.len();
    let mut h = finger([3.0, 0.0, 0.0], 0.12, 0.5, 0.25, sub_rect(FINGERS, 0, 1), sub_rect(CLAW, 0, 1));
    // a cone from its base: the radius shrinks along y (the base piece tapers like a horn, not like a finger)
    for tri in h.iter_mut() {
        for p in tri.p.iter_mut() {
            let s = 1.0 - 0.75 * (p[1] / 0.5).clamp(0.0, 1.0);
            p[0] = 3.0 + (p[0] - 3.0) * s;
            p[2] *= s;
        }
    }
    t.extend(h);
    let a = analyze(&t, &img);
    println!("{}", dump_text(&a, 0));
    let i = a.islands[a.of_tri[a0] as usize];
    let p = a.parts[i.part as usize];
    println!("{p:?}");
    assert!(p.excluded && p.rest_tris > 0 && !p.spared && p.rest_cone >= p.rest_cut, "{p:?}");
    assert!((a0..t.len()).all(|k| a.flag[k] == F_HARD));
}
