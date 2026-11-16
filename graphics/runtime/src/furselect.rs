//! Which draws are charr beyond the `fur_ps` list: a rule on what every charr draw shares, not on a shader hash (`fur_auto`,
//! docs/shell-fur.md "v18").
//!
//! `fur_ps` names pixel shaders, and the engine has many permutations of its character shader (per armour set, dye channel count,
//! skinning variant): the verified list was taken from the draws of one charr, so other charr whose body is drawn with another
//! permutation never reached the fur mask. Two facts hold for every charr draw measured so far, whatever its shader:
//!
//! 1. **The material shape.** The pixel shader samples a character composite: `t0` = BC3 colour with the gloss in alpha, `t1` = BC5
//!    normal map (the shape of every verified charr shader's draws, `1d50763f`, `5d00827f`). Skinned draws (vertex constant buffer 0
//!    of 4096 bytes or more: the bone palette) with that shape are candidates whatever their pixel shader (`admit`). Rigid draws are
//!    candidates only on an atlas already proven (below): their shape alone is every static prop's.
//! 2. **The atlas.** One character is ONE composite atlas (`t0`): head, body, arms, legs and tail draws of a charr all sample it. The
//!    shells' per-draw statistics (shellfur `ps_stats`, read back a few frames later) say whether a draw's footprint in that atlas is
//!    charr fur (the gloss band, `draw_weight`). An atlas with at least one such draw is a PROVEN charr atlas (`Book::proven`): its
//!    other draws (a body that is half armour, half bare fur) are MIXED draws and get fur on their fur texels only (the strand
//!    shader's `ft_mixed_weight`, `q4.w` = 1). Human composites never prove (their skin is 0..30 % in the band, the proof needs a draw
//!    of 50 % or more); an atlas measured as not fur is not admitted again by the content rule until `RECHECK_FRAMES` have passed.
//!
//! Every skinned draw the rule refuses, and every atlas verdict, is logged once (`furselect:` lines in gw2graphics_log.txt, at most
//! `MAX_LOG` lines a session), so a session near a charr that still lacks fur tells why.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::log;

/// Smallest vertex constant buffer 0 of a skinned draw (the bone palette; rigid draws bind 192..336 bytes).
pub const MIN_SKINNED_VCB: u32 = 4096;
/// Smallest colour texture of a composite (the charr composites are 2048 / 4096; separately textured parts 512..1024).
pub const MIN_COLOUR_SIZE: u32 = 512;
/// Smallest normal map.
pub const MIN_NORMAL_SIZE: u32 = 128;
/// A draw whose strict draw weight (shellfur `draw_weight`) reaches this proves its atlas (half of the band ramp 0.40..0.60).
pub const PROOF_WEIGHT: f32 = 0.5;
/// v19: and at least this share of that draw's footprint in the fur gloss band. Measured: charr fur 84..100 %, human skin 30 % at
/// most, leather armour up to 60 %; a scaly or leathery creature must not prove, so the bar sits well above leather.
pub const PROOF_BAND: f32 = 0.75;
/// A proof holds this many frames after the last readback that gave it (the readback runs every frame the shells draw; ~10 s at
/// 60 fps: the head may leave the screen while the body stays, and an atlas pointer is not reused for another character that soon).
pub const PROOF_FRAMES: u64 = 600;
/// An atlas measured as not fur is skipped by the content rule for this many frames (~5 min at 60 fps), then measured again. Short
/// rechecks drew the full shells on every non-fur character of a crowd every few seconds (stutter in cities).
pub const RECHECK_FRAMES: u64 = 18_000;
/// Most atlases on trial (unmeasured, or due for a recheck) per frame: the others wait their turn, so a crowd entering the screen
/// costs the shells of a couple of characters per frame instead of all of them at once.
pub const MAX_TRIALS_PER_FRAME: usize = 6; // v19: a trial only measures (no strands): cheap enough for more
/// Frames an atlas may stay on trial without a readback measuring it; then it gives its trial slot to the next atlas and waits
/// `RETRY_FRAMES` for another (never "not fur" unmeasured: its shaders may still be compiling, minutes on a first start).
pub const TRIAL_FRAMES: u64 = 30; // v19 (was 120): the readback lands a few frames after the draw
/// Frames an atlas whose trial ended unmeasured waits before its next trial.
pub const RETRY_FRAMES: u64 = 300;
/// Entries not seen for this many frames are dropped (atlas pointers are reused by the engine after a character unloads).
pub const STALE_FRAMES: u64 = 1800;
/// Most atlases remembered.
pub const MAX_ATLASES: usize = 1024;
/// Most `furselect:` log lines a session.
pub const MAX_LOG: u32 = 96;

/// DXGI BC3 (typeless, unorm, srgb) / BC5 (typeless, unorm, snorm).
pub fn is_bc3(fmt: u32) -> bool {
    matches!(fmt, 76..=78)
}
pub fn is_bc5(fmt: u32) -> bool {
    matches!(fmt, 82..=84)
}

/// A bound texture: DXGI format and size (0 x 0 = not a 2D texture).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Tex {
    pub fmt: u32,
    pub w: u32,
    pub h: u32,
}

/// The content rule: is a draw whose pixel shader is NOT in `fur_ps` a candidate charr draw? `vcb0` = vertex constant buffer 0 bytes,
/// `t0` / `t1` = the pixel shader's textures at slots 0 / 1, `proven` = its `t0` atlas is a proven charr atlas. `Err` = why not.
pub fn admit(vcb0: u32, t0: Option<Tex>, t1: Option<Tex>, proven: bool) -> Result<(), &'static str> {
    let Some(c) = t0.filter(|t| t.w > 0 && t.h > 0) else { return Err("no colour texture at t0") };
    if !is_bc3(c.fmt) {
        return Err("t0 is not a BC3 colour + gloss texture");
    }
    if c.w.min(c.h) < MIN_COLOUR_SIZE {
        return Err("t0 smaller than a character composite");
    }
    let Some(n) = t1.filter(|t| t.w > 0 && t.h > 0) else { return Err("no normal map at t1") };
    if !is_bc5(n.fmt) {
        return Err("t1 is not a BC5 normal map");
    }
    if n.w.min(n.h) < MIN_NORMAL_SIZE {
        return Err("t1 too small");
    }
    if vcb0 < MIN_SKINNED_VCB && !proven {
        return Err("rigid draw (vertex cb0 < 4096) on an atlas not proven charr");
    }
    Ok(())
}

/// What the readbacks said about one atlas.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Atlas {
    /// Frame of the last readback that measured a draw of this atlas (0 = never).
    pub measured: u64,
    /// Frame of the last readback that had a draw of strict weight >= `PROOF_WEIGHT` on it (0 = never).
    pub proved_at: u64,
    /// The best strict weight and band share of the last readback (for the log).
    pub weight: f32,
    pub band: f32,
    /// Frame this atlas was last bound by a candidate draw.
    pub seen: u64,
    /// Frame its current trial began (0 = not on trial).
    pub trial: u64,
    /// No new trial before this frame (a trial that ended unmeasured).
    pub retry_at: u64,
}

/// The per-atlas memory (render thread only).
#[derive(Debug, Default)]
pub struct Book {
    pub frame: u64,
    pub map: HashMap<usize, Atlas>,
    /// Atlases on trial this frame (at most `MAX_TRIALS_PER_FRAME`).
    pub trials: Vec<usize>,
}

impl Book {
    pub fn new() -> Book {
        Book::default()
    }

    /// A new frame; drops stale entries now and then.
    pub fn tick(&mut self) {
        self.frame += 1;
        self.trials.clear();
        if self.frame % 64 == 0 {
            let f = self.frame;
            self.map.retain(|_, a| f.saturating_sub(a.seen.max(a.measured)) <= STALE_FRAMES);
        }
    }

    fn is_proven(&self, a: &Atlas) -> bool {
        a.proved_at > 0 && self.frame.saturating_sub(a.proved_at) <= PROOF_FRAMES
    }

    /// One statistics readback: (atlas, row 0 of the draw's coat statistics = band share, median flatness, cells, strict weight) for
    /// every recorded draw of that frame. Draws without a footprint (fewer than 16 cells) say nothing. A proof holds `PROOF_FRAMES`
    /// after the last readback that had one (the head may leave the screen while the body stays). Returns the atlases whose verdict
    /// changed: (atlas, its entry, measured for the first time, proven now) (for the log).
    pub fn note(&mut self, draws: &[(usize, [f32; 4])]) -> Vec<(usize, Atlas, bool, bool)> {
        // (atlas, best weight, best band share, ONE draw passed both bars)
        let mut per: Vec<(usize, f32, f32, bool)> = Vec::new();
        for &(atlas, s0) in draws {
            if atlas == 0 || !(s0[2] >= 16.0) {
                continue;
            }
            let w = if s0[3].is_finite() { s0[3] } else { 0.0 };
            let b = if s0[0].is_finite() { s0[0] } else { 0.0 };
            let proof = w >= PROOF_WEIGHT && b >= PROOF_BAND;
            match per.iter_mut().find(|p| p.0 == atlas) {
                Some(p) => {
                    p.1 = p.1.max(w);
                    p.2 = p.2.max(b);
                    p.3 |= proof;
                }
                None => per.push((atlas, w, b, proof)),
            }
        }
        let mut changed = Vec::new();
        let frame = self.frame.max(1);
        for (atlas, w, b, proof) in per {
            if !self.map.contains_key(&atlas) && self.map.len() >= MAX_ATLASES {
                continue;
            }
            let old = self.map.get(&atlas).copied().unwrap_or_default();
            let was = old.measured > 0 && self.is_proven(&old);
            let first = old.measured == 0;
            let proved_at = if proof { frame } else { old.proved_at };
            let e = Atlas { measured: frame, proved_at, weight: w, band: b, seen: old.seen.max(frame), trial: 0, retry_at: 0 };
            let now = self.is_proven(&e);
            self.map.insert(atlas, e);
            if first || was != now {
                changed.push((atlas, e, first, now));
            }
        }
        changed
    }

    /// The atlas is a proven charr atlas (a readback of the last `PROOF_FRAMES` had a fur draw on it).
    pub fn proven(&self, atlas: usize) -> bool {
        self.map.get(&atlas).is_some_and(|a| self.is_proven(a))
    }

    /// The content rule's second half: may a candidate draw on `atlas` go into the fur mask (and be recorded for the shells)?
    /// Proven: yes. Unmeasured (or due for a recheck): on trial, at most `MAX_TRIALS_PER_FRAME` atlases a frame and for
    /// `TRIAL_FRAMES` at most. Measured as not fur: no, until `RECHECK_FRAMES` have passed since.
    pub fn verdict(&mut self, atlas: usize) -> Result<(), &'static str> {
        let frame = self.frame;
        let a = match self.map.get(&atlas).copied() {
            Some(a) => a,
            None => {
                if self.map.len() >= MAX_ATLASES {
                    return Err("too many atlases");
                }
                Atlas::default()
            }
        };
        let mut e = Atlas { seen: frame, ..a };
        let result = if self.is_proven(&a) {
            e.trial = 0;
            Ok(())
        } else if a.measured != 0 && frame.saturating_sub(a.measured) <= RECHECK_FRAMES {
            Err("atlas measured as not fur")
        } else if a.trial != 0 && frame.saturating_sub(a.trial) > TRIAL_FRAMES {
            // Never measured during its trial (shaders still compiling, no footprint on screen, readback lost): another trial later.
            e.trial = 0;
            e.retry_at = frame + RETRY_FRAMES;
            Err("waiting for its turn to be measured")
        } else if frame < a.retry_at {
            Err("waiting for its turn to be measured")
        } else if self.trials.contains(&atlas) {
            Ok(())
        } else if self.trials.len() < MAX_TRIALS_PER_FRAME {
            self.trials.push(atlas);
            if e.trial == 0 {
                e.trial = frame.max(1);
            }
            Ok(())
        } else {
            Err("waiting for its turn to be measured")
        };
        self.map.insert(atlas, e);
        result
    }
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);

fn with_book<R>(f: impl FnOnce(&mut Book) -> R) -> R {
    let mut g = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Book::new))
}

/// Frame tick (furmask::frame_begin).
pub fn frame_begin() {
    with_book(|b| b.tick());
    LAST_ADMIT.store(CUR_ADMIT.swap(0, Relaxed), Relaxed);
    LAST_SHAPE.store(CUR_SHAPE.swap(0, Relaxed), Relaxed);
    LAST_ATLAS.store(CUR_ATLAS.swap(0, Relaxed), Relaxed);
}

pub fn proven(atlas: usize) -> bool {
    atlas != 0 && with_book(|b| b.proven(atlas))
}

pub fn verdict(atlas: usize) -> Result<(), &'static str> {
    if atlas == 0 {
        return Err("no atlas");
    }
    with_book(|b| b.verdict(atlas))
}

/// A statistics readback (shellfur `read_stats`): updates the book and logs each atlas verdict that changed.
pub fn note_stats(draws: &[(usize, u32, [f32; 4])]) {
    let v: Vec<(usize, [f32; 4])> = draws.iter().map(|d| (d.0, d.2)).collect();
    let changed = with_book(|b| b.note(&v));
    for (atlas, a, first, proven) in changed {
        let counts: Vec<String> = draws.iter().filter(|d| d.0 == atlas).map(|d| d.1.to_string()).collect();
        log_line(&format!(
            "furselect: atlas {:08x} {}{} (best draw weight {:.2}, best gloss band share {:.0} %, draws of index count {})",
            atlas as u32,
            if proven { "proven charr fur: every draw of it gets fur on its fur texels (mixed skin + armour draws too)" } else { "not proven fur: only draws that pass the draw test on their own get fur" },
            if first { "" } else { " (changed)" },
            a.weight,
            a.band * 100.0,
            counts.join(", ")
        ));
    }
}

// ---- counters and the log -------------------------------------------------------------------------------------------------

static CUR_ADMIT: AtomicU32 = AtomicU32::new(0);
static CUR_SHAPE: AtomicU32 = AtomicU32::new(0);
static CUR_ATLAS: AtomicU32 = AtomicU32::new(0);
static LAST_ADMIT: AtomicU32 = AtomicU32::new(0);
static LAST_SHAPE: AtomicU32 = AtomicU32::new(0);
static LAST_ATLAS: AtomicU32 = AtomicU32::new(0);
static LOGGED: AtomicU32 = AtomicU32::new(0);
/// Draw groups already logged: (ps, vs, t0 format and size, t1 format, verdict).
static SEEN: Mutex<Vec<GroupKey>> = Mutex::new(Vec::new());

/// One logged group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupKey {
    pub ps: u32,
    pub vs: Option<u32>,
    pub t0: Tex,
    pub t1_fmt: u32,
    pub verdict: &'static str,
}

/// What the content rule did with a draw (for the counters of `status_fur=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Admitted,
    Shape,
    Atlas,
}

pub fn count(o: Outcome) {
    match o {
        Outcome::Admitted => CUR_ADMIT.fetch_add(1, Relaxed),
        Outcome::Shape => CUR_SHAPE.fetch_add(1, Relaxed),
        Outcome::Atlas => CUR_ATLAS.fetch_add(1, Relaxed),
    };
}

/// `fur_auto` part of `status_fur=`.
pub fn status_text(on: bool) -> String {
    if !on {
        return "fur_auto off (only fur_ps draws)".to_string();
    }
    format!(
        "fur_auto: {} draws of other shaders admitted by the content rule, {} skinned draws refused by their textures, {} skipped (atlas measured as not fur); {} proven charr atlases",
        LAST_ADMIT.load(Relaxed),
        LAST_SHAPE.load(Relaxed),
        LAST_ATLAS.load(Relaxed),
        with_book(|b| b.map.keys().filter(|k| b.proven(**k)).count())
    )
}

fn log_line(s: &str) {
    if LOGGED.fetch_add(1, Relaxed) < MAX_LOG {
        log!("{s}");
    }
}

/// Whether a group with this key is logged for the first time (the per-session cap included).
pub fn first_time(k: GroupKey) -> bool {
    if LOGGED.load(Relaxed) >= MAX_LOG {
        return false;
    }
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if g.contains(&k) || g.len() >= 256 {
        return false;
    }
    g.push(k);
    true
}

fn tex_text(t: Option<Tex>) -> String {
    t.filter(|t| t.w > 0).map_or("none".to_string(), |t| format!("fmt {} {}x{}", t.fmt, t.w, t.h))
}

/// Logs a candidate draw's verdict once per group: `listed` = its pixel shader is in `fur_ps`.
pub fn log_draw(listed: bool, ps: u32, vs: Option<u32>, vcb0: u32, count: u32, t0: Option<Tex>, t1: Option<Tex>, verdict: &'static str) {
    let k = GroupKey { ps, vs, t0: t0.unwrap_or_default(), t1_fmt: t1.map_or(0, |t| t.fmt), verdict };
    if !first_time(k) {
        return;
    }
    log_line(&format!(
        "furselect: {} draw ps {ps:08x} vs {} vcb0 {vcb0} count {count} t0 {} t1 {} -> {verdict}",
        if listed { "fur_ps" } else { "skinned" },
        vs.map_or("unknown".to_string(), |v| format!("{v:08x}")),
        tex_text(t0),
        tex_text(t1)
    ));
}

#[cfg(test)]
#[path = "furselect_tests.rs"]
mod tests;
