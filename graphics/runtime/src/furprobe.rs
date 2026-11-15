//! Per-draw fur probe (docs/shell-fur.md "v4: the evidence per draw"): which fur draw drew which pixel, what its textures really hold.
//!
//! WHY. v3's discriminator (`t0.a < 0.66`) changed nothing in the game: the debug capture said 0.5 % of the fur draws' pixels were "not fur",
//! so the horn, the claws and the nose have the gloss of the coat in the engine's composited 4096 atlas (the dat textures the threshold
//! was measured on are not what is bound). Nothing on disk shows the bound atlas, so the mod reads it back itself.
//!
//! HOW. A frame that starts with a debug-capture probe pending (`frame_begin`) COLLECTS its masked fur draws (`push_draw`: shaders, vcb,
//! the draw call, shells yes / no and the engine's `t0` / `t1` resources with their view's mip range) and TAGS the fur mask: the mask's
//! `b` (1.0 before) becomes `0.5 + k / 64` with k = the draw's index in the frame + 1 (`tag_value`; every reader of `b` only tests
//! `> 0.5`, so the picture is unchanged). `shell_debug=2` tags every frame and the composite paints each shell strand with the colour of
//! the draw its ROOT belongs to (`PALETTE`), the mask texels the discriminator removed black. At the probe (`analyze`, the light-adjust
//! draw, the mask read back by `furmask::probe_channel`) the textures' mips with at most 512 and 1024 texels across (the levels the
//! mask shader reads) are copied back; a background thread decodes them (BC1 / BC3 / BC4 / BC5 / RGBA8), samples them at every fur
//! pixel's mesh uv exactly like the shader (`sample_bilinear`, `detail_at`), and writes the report (`report_text`, in `status_shells=`):
//! the legend (colour -> draw, its textures), per draw 16-bin histograms of t0.a, luminance, saturation and t1 detail, the values under
//! the screen centre and the mouse, and PNGs in `GW2Graphics\debug\` (`furtex-<crc>-*.png`: the atlas colour, alpha and detail at the
//! 1024 mip, written once per texture; `furprobe-uv-<crc>.png`: the atlas with every visible fur pixel plotted in its draw's colour and
//! a cross at the centre / mouse uv). Debug captures only; the render thread pays the copies and one wait, never the decoding.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Mutex;

use crate::d3d::{IgnoreScope, Obj, Tex2dDesc, release, vfn, vt};
use crate::log;
use crate::shellfur::Call;

/// The draw colours of `shell_debug=2` and the legend (index = draw index mod 8). Same table in `light_adjust_hdr.hlsl` (SHELL_PAL).
pub const PALETTE: [[f32; 3]; 8] = [
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.15, 0.35, 1.0],
    [1.0, 1.0, 0.0],
    [0.0, 1.0, 1.0],
    [1.0, 0.0, 1.0],
    [1.0, 0.5, 0.0],
    [0.55, 0.0, 1.0],
];
pub const PALETTE_NAMES: [&str; 8] = ["red", "green", "blue", "yellow", "cyan", "magenta", "orange", "purple"];
/// Draws that get their own tag (k = 1..31); later draws of a frame are untagged (b = 1).
pub const MAX_TAGGED: u32 = 30;
/// `y` of the tag constant: the mask shader only tags when it sees this marker (a foreign b13 never does).
pub const TAG_MARK: f32 = 1234.5;
/// Histogram bins.
pub const BINS: usize = 16;

/// What the mask writes into `b` for draw `idx` (0-based): `0.5 + (idx + 1) / 64`, at most `0.5 + 31 / 64`.
pub fn tag_value(idx: u32) -> f32 {
    0.5 + (idx.min(MAX_TAGGED) + 1) as f32 / 64.0
}

/// The draw index of a mask texel's `b` (R16 UNORM), `None` for untagged (1.0), no uv (0) or a value outside the tags.
pub fn tag_of(b: u16) -> Option<u32> {
    let v = b as f32 / 65535.0;
    if v <= 0.5 {
        return None;
    }
    let k = ((v - 0.5) * 64.0).round();
    if (1.0..=(MAX_TAGGED + 1) as f32).contains(&k) && ((v - 0.5) * 64.0 - k).abs() < 0.25 { Some(k as u32 - 1) } else { None }
}

pub fn colour_name(idx: u32) -> &'static str {
    PALETTE_NAMES[idx as usize % PALETTE.len()]
}

// ---- texture decoding (pure) ------------------------------------------------------------------------------------------------

/// Block-compressed family of a DXGI format: (bytes per 4x4 block, kind) with kind 1 = BC1, 3 = BC3, 4 = BC4, 5 = BC5; 0 bytes = an
/// uncompressed RGBA8 / BGRA8 (kind 8 / 9); `None` = not decodable here.
pub fn fmt_kind(fmt: u32) -> Option<(usize, u32)> {
    Some(match fmt {
        70..=72 => (8, 1),
        76..=78 => (16, 3),
        79 | 80 => (8, 4),
        82 | 83 => (16, 5),
        27..=29 => (0, 8),
        87 | 90 | 91 => (0, 9),
        _ => return None,
    })
}

fn rgb565(c: u16) -> [u8; 3] {
    let r = ((c >> 11) & 31) as u32;
    let g = ((c >> 5) & 63) as u32;
    let b = (c & 31) as u32;
    [((r * 255 + 15) / 31) as u8, ((g * 255 + 31) / 63) as u8, ((b * 255 + 15) / 31) as u8]
}

/// One BC1 colour block (8 bytes) into 16 RGBA texels; `four` = always the 4-colour mode (BC2 / BC3).
fn bc1_block(b: &[u8], four: bool, out: &mut [[u8; 4]; 16]) {
    let c0 = u16::from_le_bytes([b[0], b[1]]);
    let c1 = u16::from_le_bytes([b[2], b[3]]);
    let (p0, p1) = (rgb565(c0), rgb565(c1));
    let mut pal = [[0u8; 4]; 4];
    pal[0] = [p0[0], p0[1], p0[2], 255];
    pal[1] = [p1[0], p1[1], p1[2], 255];
    let mix = |a: u8, b: u8, wa: u32, wb: u32, d: u32| ((a as u32 * wa + b as u32 * wb + d / 2) / d) as u8;
    if four || c0 > c1 {
        for k in 0..3 {
            pal[2][k] = mix(p0[k], p1[k], 2, 1, 3);
            pal[3][k] = mix(p0[k], p1[k], 1, 2, 3);
        }
        pal[2][3] = 255;
        pal[3][3] = 255;
    } else {
        for k in 0..3 {
            pal[2][k] = mix(p0[k], p1[k], 1, 1, 2);
        }
        pal[2][3] = 255;
        pal[3] = [0, 0, 0, 0];
    }
    let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    for (i, o) in out.iter_mut().enumerate() {
        *o = pal[((idx >> (2 * i)) & 3) as usize];
    }
}

/// One BC4 (unorm) block (8 bytes) into 16 values.
fn bc4_block(b: &[u8], out: &mut [u8; 16]) {
    let (a0, a1) = (b[0] as u32, b[1] as u32);
    let mut pal = [0u32; 8];
    pal[0] = a0;
    pal[1] = a1;
    if a0 > a1 {
        for i in 1..7u32 {
            pal[i as usize + 1] = ((7 - i) * a0 + i * a1 + 3) / 7;
        }
    } else {
        for i in 1..5u32 {
            pal[i as usize + 1] = ((5 - i) * a0 + i * a1 + 2) / 5;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    let mut bits: u64 = 0;
    for k in 0..6 {
        bits |= (b[2 + k] as u64) << (8 * k);
    }
    for (i, o) in out.iter_mut().enumerate() {
        *o = pal[((bits >> (3 * i)) & 7) as usize] as u8;
    }
}

/// Decodes `raw` (a `w` x `h` level of `fmt`, block rows tightly packed for BC) into RGBA8 (BC4: r = value, g = b = 0; BC5: r g,
/// b = 0; alpha 255 where the format has none). `None` for a format it cannot read or too little data.
pub fn decode(fmt: u32, w: u32, h: u32, raw: &[u8]) -> Option<Vec<[u8; 4]>> {
    let (bpb, kind) = fmt_kind(fmt)?;
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![[0u8, 0, 0, 255]; w * h];
    if bpb == 0 {
        if raw.len() < w * h * 4 {
            return None;
        }
        for (i, o) in out.iter_mut().enumerate() {
            let p = &raw[i * 4..i * 4 + 4];
            *o = if kind == 9 { [p[2], p[1], p[0], p[3]] } else { [p[0], p[1], p[2], p[3]] };
        }
        return Some(out);
    }
    let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
    if raw.len() < bw * bh * bpb {
        return None;
    }
    let mut cols = [[0u8; 4]; 16];
    let mut v1 = [0u8; 16];
    let mut v2 = [0u8; 16];
    for by in 0..bh {
        for bx in 0..bw {
            let b = &raw[(by * bw + bx) * bpb..(by * bw + bx + 1) * bpb];
            match kind {
                1 => bc1_block(b, false, &mut cols),
                3 => {
                    bc4_block(&b[0..8], &mut v1);
                    bc1_block(&b[8..16], true, &mut cols);
                    for i in 0..16 {
                        cols[i][3] = v1[i];
                    }
                }
                4 => {
                    bc4_block(b, &mut v1);
                    for i in 0..16 {
                        cols[i] = [v1[i], 0, 0, 255];
                    }
                }
                _ => {
                    bc4_block(&b[0..8], &mut v1);
                    bc4_block(&b[8..16], &mut v2);
                    for i in 0..16 {
                        cols[i] = [v1[i], v2[i], 0, 255];
                    }
                }
            }
            for i in 0..16 {
                let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                if x < w && y < h {
                    out[y * w + x] = cols[i];
                }
            }
        }
    }
    Some(out)
}

/// A decoded level.
#[derive(Clone, Debug, Default)]
pub struct Img {
    pub w: u32,
    pub h: u32,
    pub px: Vec<[u8; 4]>,
}

impl Img {
    pub(crate) fn at(&self, x: i64, y: i64) -> [f32; 4] {
        let (w, h) = (self.w as i64, self.h as i64);
        let p = self.px[(y.rem_euclid(h) * w + x.rem_euclid(w)) as usize];
        [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0, p[3] as f32 / 255.0]
    }
}

/// Mirror of `fd_value`: bilinear, wrapping, at frac(uv).
pub fn sample_bilinear(img: &Img, uv: [f32; 2]) -> [f32; 4] {
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return [0.0; 4];
    }
    let fr = |x: f32| x - x.floor();
    let q = [fr(uv[0]) * img.w as f32 - 0.5, fr(uv[1]) * img.h as f32 - 0.5];
    let b = [q[0].floor(), q[1].floor()];
    let f = [q[0] - b[0], q[1] - b[1]];
    let (x0, y0) = (b[0] as i64, b[1] as i64);
    let (c00, c10, c01, c11) = (img.at(x0, y0), img.at(x0 + 1, y0), img.at(x0, y0 + 1), img.at(x0 + 1, y0 + 1));
    let mut o = [0.0f32; 4];
    for k in 0..4 {
        let a = c00[k] + (c10[k] - c00[k]) * f[0];
        let c = c01[k] + (c11[k] - c01[k]) * f[0];
        o[k] = a + (c - a) * f[1];
    }
    o
}

/// Mirror of `fd_detail`: rms deviation of rg x 2 over the 3x3 texels around the nearest texel, x 2, saturated.
pub fn detail_at(img: &Img, uv: [f32; 2]) -> f32 {
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return 0.0;
    }
    let fr = |x: f32| x - x.floor();
    let c = [(fr(uv[0]) * img.w as f32).floor() as i64, (fr(uv[1]) * img.h as f32).floor() as i64];
    let (mut s, mut s2) = ([0.0f32; 2], 0.0f32);
    for y in -1..=1 {
        for x in -1..=1 {
            let t = img.at(c[0] + x, c[1] + y);
            let v = [t[0] * 2.0, t[1] * 2.0];
            s[0] += v[0];
            s[1] += v[1];
            s2 += v[0] * v[0] + v[1] * v[1];
        }
    }
    let m = [s[0] / 9.0, s[1] / 9.0];
    let var = (s2 / 9.0 - m[0] * m[0] - m[1] * m[1]).max(0.0);
    (2.0 * var.sqrt()).clamp(0.0, 1.0)
}

/// Mirror of `fd_flat` (v5, the colour flatness): rms of rgb over the 5x5 texels around the nearest texel, x2, divided by the rms of
/// the window's mean colour (dye-invariant for a multiplied paint); 1 where the brightest mean channel is above `shellfur::FLAT_CLIP`.
pub fn flat_at(img: &Img, uv: [f32; 2]) -> f32 {
    if img.w == 0 || img.h == 0 || img.px.len() < (img.w * img.h) as usize {
        return 0.0;
    }
    let fr = |x: f32| x - x.floor();
    let c = [(fr(uv[0]) * img.w as f32).floor() as i64, (fr(uv[1]) * img.h as f32).floor() as i64];
    let (mut s, mut s2) = ([0.0f32; 3], 0.0f32);
    for y in -2..=2 {
        for x in -2..=2 {
            let t = img.at(c[0] + x, c[1] + y);
            for k in 0..3 {
                s[k] += t[k];
                s2 += t[k] * t[k];
            }
        }
    }
    let m = [s[0] / 25.0, s[1] / 25.0, s[2] / 25.0];
    let mx = m[0].max(m[1]).max(m[2]);
    if mx > crate::shellfur::FLAT_CLIP {
        return 1.0;
    }
    let mm = m[0] * m[0] + m[1] * m[1] + m[2] * m[2];
    let var = ((s2 / 25.0 - mm).max(0.0)) / 3.0;
    (2.0 * var.sqrt() / (mm / 3.0).sqrt().max(0.05)).clamp(0.0, 1.0)
}

pub fn luminance(c: [f32; 4]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

pub fn saturation(c: [f32; 4]) -> f32 {
    let mx = c[0].max(c[1]).max(c[2]);
    let mn = c[0].min(c[1]).min(c[2]);
    (mx - mn) / mx.max(1e-3)
}

/// Mirror of `fd_level`: the level of a view `w` x `h` with `n` levels that has at most `lim` texels across.
pub fn level_for(w: u32, h: u32, n: u32, lim: u32) -> u32 {
    let mut lv = 0;
    while lv + 1 < n && (w.max(h) >> lv) > lim {
        lv += 1;
    }
    lv
}

// ---- statistics (pure) ----------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hist {
    pub bins: [u64; BINS],
    pub n: u64,
}

impl Hist {
    pub fn add(&mut self, v: f32) {
        if !v.is_finite() {
            return;
        }
        let i = ((v.clamp(0.0, 1.0) * BINS as f32) as usize).min(BINS - 1);
        self.bins[i] += 1;
        self.n += 1;
    }

    /// The value below which `p` (0..1) of the samples lie (the upper edge of that bin).
    pub fn quantile(&self, p: f32) -> f32 {
        if self.n == 0 {
            return 0.0;
        }
        let want = (p.clamp(0.0, 1.0) * self.n as f32).ceil().max(1.0) as u64;
        let mut acc = 0;
        for (i, b) in self.bins.iter().enumerate() {
            acc += b;
            if acc >= want {
                return (i + 1) as f32 / BINS as f32;
            }
        }
        1.0
    }

    /// `p10/p50/p90 x/y/z, % per 1/16 bin [..]`.
    pub fn text(&self) -> String {
        if self.n == 0 {
            return "no samples".to_string();
        }
        let pc: Vec<String> = self.bins.iter().map(|&b| format!("{}", ((b as f64) * 100.0 / self.n as f64).round() as u32)).collect();
        format!("p10/50/90 {:.3}/{:.3}/{:.3}, % per 1/16 [{}]", self.quantile(0.1), self.quantile(0.5), self.quantile(0.9), pc.join(" "))
    }
}

/// One collected draw (no COM: what the report needs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawMeta {
    pub idx: u32,
    pub ps: u32,
    pub vs: Option<u32>,
    pub vcb0: u32,
    pub count: u32,
    pub start: u32,
    pub base: i32,
    pub inst: u32,
    pub shells: bool,
    /// Texture of t0 / t1: index into the probe's texture list.
    pub tex: [Option<usize>; 2],
}

/// One texture of the probe: what the draws had bound and the two levels read back (decoded).
#[derive(Clone, Debug, Default)]
pub struct TexData {
    pub res: usize,
    pub w: u32,
    pub h: u32,
    pub mips: u32,
    pub fmt: u32,
    pub view_fmt: u32,
    pub most: u32,
    pub view_mips: u32,
    /// The level the mask shader samples (at most 512 across) and the detail level (at most 1024), resource levels.
    pub l512: u32,
    pub l1024: u32,
    pub img512: Img,
    pub img1024: Img,
    /// CRC of the raw 512 level (a stable id across sessions; 0 = not read).
    pub crc: u32,
    pub why: String,
}

impl TexData {
    pub fn desc(&self) -> String {
        let id = if self.crc != 0 { format!("{:08x}", self.crc) } else { "unread".to_string() };
        format!(
            "tex {id} {}x{} {} m{} (view {} mips {}+{}; sampled at level {} = {}x{}){}",
            self.w,
            self.h,
            crate::passes::format_name(self.fmt),
            self.mips,
            crate::passes::format_name(self.view_fmt),
            self.most,
            self.view_mips,
            self.l512,
            self.img512.w,
            self.img512.h,
            if self.why.is_empty() { String::new() } else { format!(" [{}]", self.why) }
        )
    }
}

/// The statistics of one draw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawStats {
    pub px: u64,
    pub not_fur: u64,
    pub a: Hist,
    pub lum: Hist,
    pub sat: Hist,
    pub det: Hist,
    pub det0: Hist,
    /// v5: the colour flatness of t0 (`flat_at`).
    pub flat0: Hist,
}

/// What a pixel of the probe says (a point: centre / mouse).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointInfo {
    pub draw: Option<u32>,
    pub uv: [f32; 2],
    pub t0: Option<[f32; 4]>,
    pub t1: Option<[f32; 4]>,
    pub det1: Option<f32>,
    pub flat0: Option<f32>,
    pub weight: f32,
    pub dist: f32,
}

/// The values of the probe at one fur pixel.
struct Vals {
    draw: Option<usize>,
    uv: [f32; 2],
    t0: Option<[f32; 4]>,
    t1: Option<[f32; 4]>,
    d1: Option<f32>,
    d0: Option<f32>,
    f0: Option<f32>,
}

fn values_at(texel: [u16; 4], draws: &[DrawMeta], tex: &[TexData]) -> Vals {
    let uv = [texel[0] as f32 / 65535.0, texel[1] as f32 / 65535.0];
    let none = Vals { draw: None, uv, t0: None, t1: None, d1: None, d0: None, f0: None };
    let Some(k) = tag_of(texel[2]) else { return none };
    let Some(di) = draws.iter().position(|d| d.idx == k) else { return none };
    let d = &draws[di];
    let get = |s: usize| d.tex[s].and_then(|i| tex.get(i)).filter(|t| !t.img512.px.is_empty());
    fn hi(t: &TexData) -> &Img {
        if t.img1024.px.is_empty() { &t.img512 } else { &t.img1024 }
    }
    Vals {
        draw: Some(di),
        uv,
        t0: get(0).map(|t| sample_bilinear(&t.img512, uv)),
        t1: get(1).map(|t| sample_bilinear(&t.img512, uv)),
        d1: get(1).map(|t| detail_at(hi(t), uv)),
        d0: get(0).map(|t| detail_at(hi(t), uv)),
        f0: get(0).map(|t| flat_at(hi(t), uv)),
    }
}

/// Pure: the per-draw statistics of the read-back mask (`texels`: rg = uv, b = tag, a = fur weight; `stamp` in 0..1 = drawn) and the
/// points. Returns (stats per draw, drawn pixels without a tag, points).
pub fn analyze_pixels(texels: &[[u16; 4]], stamp: &[f32], w: u32, h: u32, draws: &[DrawMeta], tex: &[TexData], points: &[(String, u32, u32)]) -> (Vec<DrawStats>, u64, Vec<(String, u32, u32, PointInfo)>) {
    let n = (w as usize * h as usize).min(texels.len()).min(stamp.len());
    let drawn = |i: usize| (0.0..=1.0).contains(&stamp[i]) && texels[i][2] > 32767;
    let mut st = vec![DrawStats::default(); draws.len()];
    let mut untagged = 0u64;
    for i in 0..n {
        if !drawn(i) {
            continue;
        }
        let v = values_at(texels[i], draws, tex);
        let Some(di) = v.draw else {
            untagged += 1;
            continue;
        };
        let s = &mut st[di];
        s.px += 1;
        s.not_fur += (texels[i][3] <= 32767) as u64;
        if let Some(c) = v.t0 {
            s.a.add(c[3]);
            s.lum.add(luminance(c));
            s.sat.add(saturation(c));
        }
        if let Some(d) = v.d1 {
            s.det.add(d);
        }
        if let Some(d) = v.d0 {
            s.det0.add(d);
        }
        if let Some(f) = v.f0 {
            s.flat0.add(f);
        }
    }
    let mut pts = Vec::new();
    for (name, x, y) in points {
        let mut pi = PointInfo::default();
        if w > 0 && h > 0 && n > 0 {
            let (x0, y0) = ((*x).min(w - 1) as i64, (*y).min(h - 1) as i64);
            let mut best: Option<(i64, usize)> = None;
            for dy in -48i64..=48 {
                for dx in -48i64..=48 {
                    let (px, py) = (x0 + dx, y0 + dy);
                    if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
                        continue;
                    }
                    let i = py as usize * w as usize + px as usize;
                    if i >= n || !drawn(i) {
                        continue;
                    }
                    let dd = dx * dx + dy * dy;
                    if best.is_none_or(|(b, _)| dd < b) {
                        best = Some((dd, i));
                    }
                }
            }
            if let Some((dd, i)) = best {
                let v = values_at(texels[i], draws, tex);
                pi = PointInfo { draw: v.draw.map(|k| draws[k].idx), uv: v.uv, t0: v.t0, t1: v.t1, det1: v.d1, flat0: v.f0, weight: texels[i][3] as f32 / 65535.0, dist: (dd as f32).sqrt() };
            } else {
                pi.dist = -1.0;
            }
        }
        pts.push((name.clone(), *x, *y, pi));
    }
    (st, untagged, pts)
}

fn f4(c: Option<[f32; 4]>, n: usize) -> String {
    match c {
        None => "n/a".to_string(),
        Some(c) => format!("({})", c[..n].iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", ")),
    }
}

/// Pure: the report text.
pub fn report(draws: &[DrawMeta], tex: &[TexData], st: &[DrawStats], untagged: u64, pts: &[(String, u32, u32, PointInfo)], files: &[String]) -> String {
    let total: u64 = st.iter().map(|s| s.px).sum::<u64>() + untagged;
    let mut t = format!(
        "per-draw probe ({} fur draws collected, {total} visible fur-draw px, shell_debug=2 colours; NOTE: measured in the frame right after the PREVIOUS debug capture, so it belongs to that capture's screenshot and to debug\\furprobe-last.txt, written with the PNGs):",
        draws.len()
    );
    for (d, s) in draws.iter().zip(st) {
        let texs: Vec<String> = (0..2).map(|k| format!("t{k} {}", d.tex[k].and_then(|i| tex.get(i)).map_or("unbound".to_string(), |x| x.desc()))).collect();
        let pct = |a: u64| if total == 0 { 0.0 } else { a as f64 * 100.0 / total as f64 };
        t.push_str(&format!(
            " #{} {} [vs {} ps {:08x} vcb {} count {} start {} base {} inst {} -> {}; {}; {} px ({:.1} % of the fur px), not fur {:.1} %; t0.a {}; t0 luminance {}; t0 saturation {}; t1 detail {}; t0 detail {}; t0 flat {}]",
            d.idx,
            colour_name(d.idx),
            d.vs.map_or("unknown".to_string(), |v| format!("{v:08x}")),
            d.ps,
            d.vcb0,
            d.count,
            d.start,
            d.base,
            d.inst,
            if d.shells { "shells" } else { "no shells" },
            texs.join(", "),
            s.px,
            pct(s.px),
            if s.px == 0 { 0.0 } else { s.not_fur as f64 * 100.0 / s.px as f64 },
            s.a.text(),
            s.lum.text(),
            s.sat.text(),
            s.det.text(),
            s.det0.text(),
            s.flat0.text()
        ));
    }
    if untagged > 0 {
        t.push_str(&format!(" untagged fur px {untagged} (draws past #{MAX_TAGGED}, or no probe tag)"));
    }
    for (name, x, y, p) in pts {
        if p.dist < 0.0 {
            t.push_str(&format!("; {name} px ({x}, {y}): no fur draw within 48 px"));
            continue;
        }
        let who = p.draw.map_or("untagged".to_string(), |k| format!("draw #{k} {}", colour_name(k)));
        let near = if p.dist > 0.0 { format!(" (nearest fur-draw pixel, {:.0} px away)", p.dist) } else { String::new() };
        let ls = p.t0.map_or(String::new(), |c| format!(", t0 luminance {:.3} saturation {:.3}", luminance(c), saturation(c)));
        t.push_str(&format!(
            "; {name} px ({x}, {y}): {who}, uv ({:.4}, {:.4}), t0 rgba {}{ls}, t1 rg {}, t1 detail {}, t0 flat {}, fur weight {:.2}{near}",
            p.uv[0],
            p.uv[1],
            f4(p.t0, 4),
            f4(p.t1, 2),
            p.det1.map_or("n/a".to_string(), |v| format!("{v:.3}")),
            p.flat0.map_or("n/a".to_string(), |v| format!("{v:.3}")),
            p.weight
        ));
    }
    if !files.is_empty() {
        t.push_str(&format!("; files in debug\\: {}", files.join(", ")));
    }
    t
}

/// Pure: the overlay picture: the atlas `img` (dimmed) with every fur pixel's uv plotted in its draw's colour (only the draws whose t0 is
/// this texture), a white cross at the mouse uv and a yellow one at the centre uv. RGB8.
pub fn overlay(img: &Img, texels: &[[u16; 4]], stamp: &[f32], draws: &[DrawMeta], tex_index: usize, pts: &[(String, u32, u32, PointInfo)]) -> Vec<u8> {
    let (w, h) = (img.w as usize, img.h as usize);
    let mut o = vec![0u8; w * h * 3];
    for (i, p) in img.px.iter().enumerate().take(w * h) {
        for k in 0..3 {
            o[i * 3 + k] = (p[k] as f32 * 0.4) as u8;
        }
    }
    let mine: Vec<u32> = draws.iter().filter(|d| d.tex[0] == Some(tex_index)).map(|d| d.idx).collect();
    for (t, &s) in texels.iter().zip(stamp) {
        if !(0.0..=1.0).contains(&s) || t[2] <= 32767 {
            continue;
        }
        let Some(k) = tag_of(t[2]) else { continue };
        if !mine.contains(&k) {
            continue;
        }
        let x = ((t[0] as f32 / 65535.0) * w as f32) as usize;
        let y = ((t[1] as f32 / 65535.0) * h as f32) as usize;
        if x < w && y < h {
            let c = PALETTE[k as usize % PALETTE.len()];
            let notfur = t[3] <= 32767;
            for q in 0..3 {
                o[(y * w + x) * 3 + q] = if notfur { 255 } else { (c[q] * 255.0) as u8 };
            }
        }
    }
    for (name, _, _, p) in pts {
        if p.dist < 0.0 || p.draw.is_none() {
            continue;
        }
        let col: [u8; 3] = if name == "mouse" { [255, 255, 255] } else { [255, 255, 0] };
        let (cx, cy) = ((p.uv[0] * w as f32) as i64, (p.uv[1] * h as f32) as i64);
        for d in -12i64..=12 {
            for (x, y, c) in [(cx + d, cy, col), (cx, cy + d, col), (cx + d, cy + 1, [0, 0, 0]), (cx + 1, cy + d, [0, 0, 0])] {
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && d.abs() > 2 {
                    let i = (y as usize * w + x as usize) * 3;
                    o[i..i + 3].copy_from_slice(&c);
                }
            }
        }
    }
    o
}

// ---- live state ------------------------------------------------------------------------------------------------------------

/// `shell_debug=2`: tag every frame.
static DEBUG2: AtomicBool = AtomicBool::new(false);
/// This frame collects its fur draws (a probe was pending when it began).
static COLLECT: AtomicBool = AtomicBool::new(false);
/// This frame tags the mask (collect or debug 2).
static TAG: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU32 = AtomicU32::new(0);
/// A report is being computed on the background thread (no new collection meanwhile).
static BUSY: AtomicBool = AtomicBool::new(false);
static REPORT: Mutex<String> = Mutex::new(String::new());

/// The engine textures of one draw (t0, t1): counted references, released when dropped.
#[derive(Default)]
pub struct Texs(pub [Option<TexRef>; 2]);

pub struct TexRef {
    pub res: Obj,
    pub d: Tex2dDesc,
    pub view_fmt: u32,
    pub most: u32,
    pub view_mips: u32,
}

impl Drop for TexRef {
    fn drop(&mut self) {
        unsafe { release(self.res) };
    }
}

// SAFETY: the references are only touched on the render thread (collected, read back, dropped there); the mutex only parks them.
unsafe impl Send for TexRef {}

struct Collected {
    meta: DrawMeta,
    tex: Texs,
}

static DRAWS: Mutex<Vec<Collected>> = Mutex::new(Vec::new());

pub fn set_debug2(on: bool) {
    DEBUG2.store(on, Relaxed);
}

/// A frame starts: what the previous one collected is dropped; `probe_pending` = the fur mask's probe is wanted.
pub fn frame_begin(probe_pending: bool) {
    let collect = probe_pending && !BUSY.load(Relaxed);
    COLLECT.store(collect, Relaxed);
    TAG.store(collect || DEBUG2.load(Relaxed), Relaxed);
    NEXT.store(0, Relaxed);
    if let Ok(mut d) = DRAWS.try_lock() {
        d.clear();
    }
}

pub fn tagging() -> bool {
    TAG.load(Relaxed)
}

pub fn collecting() -> bool {
    COLLECT.load(Relaxed)
}

pub fn next_index() -> u32 {
    NEXT.fetch_add(1, Relaxed)
}

/// The engine's t0 / t1 of the draw about to be issued (resources with a counted reference, their descriptions and the views' mips).
///
/// # Safety
/// `ctx` must be a live device context.
pub unsafe fn textures(ctx: Obj) -> Texs {
    let mut out = Texs::default();
    let _ig = IgnoreScope::new();
    unsafe {
        let mut v: [Obj; 2] = [std::ptr::null_mut(); 2];
        let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
        get(ctx, 0, 2, v.as_mut_ptr());
        for (k, view) in v.into_iter().enumerate() {
            if view.is_null() {
                continue;
            }
            let mut sd = [0u32; 8];
            let get_desc: unsafe extern "system" fn(Obj, *mut [u32; 8]) = std::mem::transmute(vfn(view, vt::VIEW_GET_DESC));
            get_desc(view, &mut sd);
            let mut res: Obj = std::ptr::null_mut();
            let get_res: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(view, vt::VIEW_GET_RESOURCE));
            get_res(view, &mut res);
            release(view);
            if res.is_null() {
                continue;
            }
            let mut ty = 0u32;
            let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
            get_type(res, &mut ty);
            // D3D11_RESOURCE_DIMENSION_TEXTURE2D = 3, D3D11_SRV_DIMENSION_TEXTURE2D = 4
            if ty != 3 || sd[1] != 4 {
                release(res);
                continue;
            }
            let mut d = Tex2dDesc::default();
            let get_tdesc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_tdesc(res, &mut d);
            let most = sd[2].min(d.mips.saturating_sub(1));
            let vm = if sd[3] == u32::MAX || sd[3] == 0 { d.mips - most } else { sd[3].min(d.mips - most) };
            out.0[k] = Some(TexRef { res, d, view_fmt: sd[0], most, view_mips: vm });
        }
    }
    out
}

/// A collected fur draw went into the mask (the frame collects).
pub fn push_draw(idx: u32, ps: u32, vs: Option<u32>, vcb0: u32, call: Call, shells: bool, tex: Texs) {
    if !COLLECT.load(Relaxed) {
        return;
    }
    let (start, base) = match call {
        Call::Draw(_, s) => (s, 0),
        Call::Indexed(_, s, b) => (s, b),
        Call::Instanced(_, _, s, _) => (s, 0),
        Call::IndexedInstanced(_, _, s, b, _) => (s, b),
    };
    let meta = DrawMeta { idx, ps, vs, vcb0, count: call.count(), start, base, inst: call.instances(), shells, tex: [None, None] };
    if let Ok(mut d) = DRAWS.try_lock() {
        if d.len() < MAX_TAGGED as usize + 2 {
            d.push(Collected { meta, tex });
        }
    }
}

/// Copies one level of `res` back (waits for the GPU; debug captures only). (w, h, raw bytes).
pub(crate) unsafe fn read_level(dev: Obj, ctx: Obj, res: Obj, d: &Tex2dDesc, level: u32) -> Result<(u32, u32, Vec<u8>), String> {
    let (bpb, _) = fmt_kind(d.format).ok_or_else(|| format!("format {} not decodable", crate::passes::format_name(d.format)))?;
    let (w, h) = ((d.width >> level).max(1), (d.height >> level).max(1));
    if bpb > 0 && (w < 4 || h < 4) {
        return Err("level too small".into());
    }
    unsafe {
        let st = crate::debugshot::create_staging(dev, w, h, d.format);
        if st.is_null() {
            return Err("no staging texture".into());
        }
        let copy: unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const u32) = std::mem::transmute(vfn(ctx, vt::CTX_COPY_SUBRESOURCE_REGION));
        copy(ctx, st, 0, 0, 0, 0, res, level, std::ptr::null());
        let r = if bpb > 0 { crate::debugshot::read_texture(ctx, st, 0, w.div_ceil(4), h.div_ceil(4), bpb) } else { crate::debugshot::read_texture(ctx, st, 0, w, h, 4) };
        release(st);
        r.map(|v| (w, h, v)).ok_or_else(|| "read-back failed".into())
    }
}

/// The probe of this frame (the light-adjust draw, after `furmask` read the mask back): the collected draws' textures are read back,
/// the rest goes to a background thread.
///
/// # Safety
/// `dev` / `ctx`: the live device and its immediate context.
pub unsafe fn analyze(dev: Obj, ctx: Obj, texels: Vec<[u16; 4]>, stamp: Vec<f32>, w: u32, h: u32, pts: Vec<(String, u32, u32)>) {
    if !COLLECT.load(Relaxed) || BUSY.load(Relaxed) {
        return;
    }
    let collected = match DRAWS.try_lock() {
        Ok(mut d) => std::mem::take(&mut *d),
        Err(_) => return,
    };
    let _ig = IgnoreScope::new();
    let mut tex: Vec<TexData> = Vec::new();
    let mut raws: Vec<Vec<u8>> = Vec::new();
    let mut metas = Vec::new();
    for c in &collected {
        let mut m = c.meta.clone();
        for k in 0..2 {
            let Some(r) = &c.tex.0[k] else { continue };
            if let Some(i) = tex.iter().position(|t| t.res == r.res as usize) {
                m.tex[k] = Some(i);
                continue;
            }
            let vw = (r.d.width >> r.most).max(1);
            let vh = (r.d.height >> r.most).max(1);
            let mut t = TexData {
                res: r.res as usize,
                w: r.d.width,
                h: r.d.height,
                mips: r.d.mips,
                fmt: r.d.format,
                view_fmt: r.view_fmt,
                most: r.most,
                view_mips: r.view_mips,
                l512: r.most + level_for(vw, vh, r.view_mips, 512),
                l1024: r.most + level_for(vw, vh, r.view_mips, 1024),
                ..TexData::default()
            };
            let mut raw512 = Vec::new();
            match unsafe { read_level(dev, ctx, r.res, &r.d, t.l512) } {
                Ok((lw, lh, raw)) => {
                    t.crc = crate::debugshot::crc32(&[&raw]);
                    t.img512 = Img { w: lw, h: lh, px: Vec::new() };
                    raw512 = raw;
                }
                Err(e) => t.why = e,
            }
            let mut raw1024 = Vec::new();
            if t.why.is_empty() {
                match unsafe { read_level(dev, ctx, r.res, &r.d, t.l1024) } {
                    Ok((lw, lh, raw)) => {
                        t.img1024 = Img { w: lw, h: lh, px: Vec::new() };
                        raw1024 = raw;
                    }
                    Err(e) => t.why = format!("1024 level: {e}"),
                }
            }
            raws.push(raw512);
            raws.push(raw1024);
            m.tex[k] = Some(tex.len());
            tex.push(t);
        }
        metas.push(m);
    }
    drop(collected); // the references go back on the render thread
    BUSY.store(true, Relaxed);
    let dir = crate::grading::dir().map(|d| d.join("debug"));
    let spawned = std::thread::Builder::new().name("gw2g-furprobe".into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut tex = tex;
            for (i, t) in tex.iter_mut().enumerate() {
                let (r5, r1) = (&raws[i * 2], &raws[i * 2 + 1]);
                if !r5.is_empty() {
                    t.img512.px = decode(t.fmt, t.img512.w, t.img512.h, r5).unwrap_or_default();
                }
                if !r1.is_empty() {
                    t.img1024.px = decode(t.fmt, t.img1024.w, t.img1024.h, r1).unwrap_or_default();
                }
            }
            let (st, untagged, pinfo) = analyze_pixels(&texels, &stamp, w, h, &metas, &tex, &pts);
            let files = dir.as_ref().map(|d| write_files(d, &tex, &texels, &stamp, &metas, &pinfo)).unwrap_or_default();
            let text = report(&metas, &tex, &st, untagged, &pinfo, &files);
            // v5: the report next to the PNGs of the same frame (the status text only carries it with the NEXT capture).
            if let Some(d) = &dir {
                let _ = std::fs::write(d.join("furprobe-last.txt"), format!("{}\n", text.replace("; ", ";\n  ").replace(" #", "\n#")));
            }
            text
        }));
        let text = r.unwrap_or_else(|_| "per-draw probe: failed (panic in the analysis)".to_string());
        *REPORT.lock().unwrap_or_else(|e| e.into_inner()) = text;
        BUSY.store(false, Relaxed);
    });
    if spawned.is_err() {
        BUSY.store(false, Relaxed);
        log!("furprobe: could not start the analysis thread");
    }
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> bool {
    std::fs::write(path, crate::debugshot::encode_png_rgb8(w, h, rgb)).is_ok()
}

/// The PNGs (background thread): per texture once (`furtex-<crc>-rgb|a|detail.png`), the uv overlay per t0 every probe.
fn write_files(dir: &std::path::Path, tex: &[TexData], texels: &[[u16; 4]], stamp: &[f32], draws: &[DrawMeta], pts: &[(String, u32, u32, PointInfo)]) -> Vec<String> {
    let _ = std::fs::create_dir_all(dir);
    let mut out = Vec::new();
    for (i, t) in tex.iter().enumerate() {
        let img = if t.img1024.px.is_empty() { &t.img512 } else { &t.img1024 };
        if img.px.is_empty() || t.crc == 0 {
            continue;
        }
        let (w, h) = (img.w, img.h);
        let base = format!("furtex-{:08x}", t.crc);
        let rgb_name = format!("{base}-rgb.png");
        if !dir.join(&rgb_name).exists() {
            let rgb: Vec<u8> = img.px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
            let a: Vec<u8> = img.px.iter().flat_map(|p| [p[3], p[3], p[3]]).collect();
            let det: Vec<u8> = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .flat_map(|(x, y)| {
                    let v = (detail_at(img, [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32]) * 255.0) as u8;
                    [v, v, v]
                })
                .collect();
            let ok = write_png(&dir.join(&rgb_name), w, h, &rgb) & write_png(&dir.join(format!("{base}-a.png")), w, h, &a) & write_png(&dir.join(format!("{base}-detail.png")), w, h, &det);
            if ok {
                log!("furprobe: wrote {}", dir.join(&rgb_name).display());
            }
        }
        out.push(format!("{base}-rgb/-a/-detail.png ({w}x{h}, level {})", t.l1024));
        if draws.iter().any(|d| d.tex[0] == Some(i)) {
            let name = format!("furprobe-uv-{:08x}.png", t.crc);
            if write_png(&dir.join(&name), w, h, &overlay(img, texels, stamp, draws, i, pts)) {
                out.push(name);
            }
        }
    }
    out
}

/// The `status_shells=` part: the last report (a capture after the probe), or what is pending.
pub fn report_text() -> String {
    let t = REPORT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if BUSY.load(Relaxed) {
        return "per-draw probe: computing (the next debug capture reports it)".to_string();
    }
    if t.is_empty() { "per-draw probe: none yet (the next debug capture collects it)".to_string() } else { t }
}

#[cfg(test)]
#[path = "furprobe_tests.rs"]
mod tests;
