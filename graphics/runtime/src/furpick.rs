//! Fur shader picker: the data and the pure logic behind the overlay's "Pick fur shaders" window.
//!
//! WHAT: while the picker window is open, every draw of the opaque pass (full-size colour + depth) whose vertex constant
//! buffer 0 is big enough to be a skinned mesh (`PICK_MIN_VCB`) is summarised into a group keyed by (vertex shader,
//! pixel shader, size of the first bound texture). The frame boundary publishes the finished groups (`frame_begin`), the
//! overlay lists them (`rows`) and lets the player tick the pixel shaders that are fur (`list_toggle`) or hover / select a
//! row to paint ONLY that group magenta in the scene (`set_highlight`, see `furmask::set_highlight`).
//!
//! COST: nothing is read or stored while the window is closed: `drawdiag::on_draw` pays one more atomic load (`is_open`),
//! `note` and `frame_begin` return at once. While open, a draw costs the opaque-pass check, one vertex constant buffer
//! read and, for skinned draws only, a full `drawdiag::read_bound`. Memory is bounded: at most `MAX_GROUPS` groups per
//! frame (more are counted, not stored). Closing the window clears the groups and the highlight.
//!
//! Also here (pure, tested on the host): the fur hash list editing (`list_*`) and the overlay layout numbers.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::d3d::{self, Obj};
use crate::drawdiag::{Bound, DrawArgs};
use crate::{furmask, recorder, shaders};

/// Groups kept per frame.
pub const MAX_GROUPS: usize = 64;
/// Smallest vertex constant buffer 0 (bytes) of a draw the picker lists (skinned meshes carry the bone matrices).
pub const PICK_MIN_VCB: u32 = 1024;
/// The verified charr pixel shaders (`fur_ps` preset).
pub const CHARR_PRESET: &str = "1d50763f,e0189185,5d00827f";

// ---- the hash list (`fur_ps`) ---------------------------------------------------------------------------

/// One hash as typed: 1 to 8 hex digits, optional `0x`.
pub fn parse_hash(text: &str) -> Option<u32> {
    let v = furmask::parse_hashes(text);
    if v.len() == 1 { v.first().copied() } else { None }
}

/// `1d50763f,e0189185`: 8 hex digits each, comma separated.
pub fn format_list(v: &[u32]) -> String {
    furmask::join_hashes(v)
}

pub fn list_has(text: &str, h: u32) -> bool {
    furmask::parse_hashes(text).contains(&h)
}

/// Adds `h` (no duplicate, at most `furmask::MAX_LIST`); the result is normalised.
pub fn list_add(text: &str, h: u32) -> String {
    let mut v = furmask::parse_hashes(text);
    if !v.contains(&h) && v.len() < furmask::MAX_LIST {
        v.push(h);
    }
    format_list(&v)
}

pub fn list_remove(text: &str, h: u32) -> String {
    let mut v = furmask::parse_hashes(text);
    v.retain(|x| *x != h);
    format_list(&v)
}

/// Adds `h` when absent, removes it when present; also says whether it is in the list now.
pub fn list_toggle(text: &str, h: u32) -> (String, bool) {
    if list_has(text, h) { (list_remove(text, h), false) } else { (list_add(text, h), true) }
}

// ---- overlay layout (pure) ---------------------------------------------------------------------------------

/// The scale the overlay uses for fonts and sizes (display height / 1080, 1..2).
pub fn ui_scale(display_h: f32) -> f32 {
    if display_h.is_finite() { (display_h / 1080.0).clamp(1.0, 2.0) } else { 1.0 }
}

/// Default width of the main window: 460 px at 1080p, scaled, never more than 90 % of the display.
pub fn window_width(display_w: f32, display_h: f32) -> f32 {
    let w = 460.0 * ui_scale(display_h);
    if display_w.is_finite() && display_w > 0.0 { w.min(display_w * 0.9) } else { w }
}

/// Default width of the picker window (the table is wide).
pub fn picker_width(display_w: f32, display_h: f32) -> f32 {
    let w = 700.0 * ui_scale(display_h);
    if display_w.is_finite() && display_w > 0.0 { w.min(display_w * 0.9) } else { w }
}

/// The value for `push_item_width`: negative = the widget takes the window width minus this, leaving room for the label
/// at the right (45 % of the window, at least 160 px scaled, at most 65 %), so no label is cut.
pub fn item_width(window_w: f32, scale: f32) -> f32 {
    let w = if window_w.is_finite() && window_w > 0.0 { window_w } else { 460.0 };
    let label = (w * 0.45).max(160.0 * scale.clamp(1.0, 2.0)).min(w * 0.65);
    -label
}

// ---- groups --------------------------------------------------------------------------------------------

/// What one skinned draw tells the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub ps: u32,
    pub vs: Option<u32>,
    pub vcb: u32,
    /// Indices (indexed draw) or vertices (plain draw) of this draw.
    pub indices: u32,
    pub vertices: u32,
    /// Width and height of the first bound texture (lowest slot), if any.
    pub tex0: Option<(u32, u32)>,
}

/// One listed group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub ps: u32,
    pub vs: Option<u32>,
    pub tex0: Option<(u32, u32)>,
    pub vcb_max: u32,
    pub draws: u32,
    pub indices: u64,
    pub vertices: u64,
}

impl Row {
    pub fn key(&self) -> HighlightKey {
        HighlightKey { ps: self.ps, vs: self.vs, tex0: self.tex0 }
    }
}

/// Which draws a highlight singles out: this pixel shader, and (when given) this vertex shader and first texture size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightKey {
    pub ps: u32,
    pub vs: Option<u32>,
    pub tex0: Option<(u32, u32)>,
}

impl HighlightKey {
    pub fn matches(&self, ps: u32, vs: Option<u32>, tex0: Option<(u32, u32)>) -> bool {
        self.ps == ps && self.vs.is_none_or(|v| vs == Some(v)) && self.tex0.is_none_or(|t| tex0 == Some(t))
    }

    /// The draw has to be read for its texture to decide.
    pub fn needs_tex(&self) -> bool {
        self.tex0.is_some()
    }
}

/// First bound texture (lowest slot with a known size) of a draw.
pub fn tex0_of(b: &Bound) -> Option<(u32, u32)> {
    b.srvs.iter().filter(|(t, _)| t.w > 0 && t.h > 0).min_by_key(|(t, _)| t.slot).map(|(t, _)| (t.w, t.h))
}

/// Bounded per-frame aggregation.
#[derive(Default)]
pub struct Agg {
    rows: Vec<Row>,
    /// Draws whose group did not fit (the table is full).
    pub overflow: u32,
}

impl Agg {
    pub const fn new() -> Agg {
        Agg { rows: Vec::new(), overflow: 0 }
    }

    pub fn add(&mut self, s: &Sample) {
        let key = (s.ps, s.vs, s.tex0);
        let i = match self.rows.iter().position(|r| (r.ps, r.vs, r.tex0) == key) {
            Some(i) => i,
            None => {
                if self.rows.len() >= MAX_GROUPS {
                    self.overflow += 1;
                    return;
                }
                self.rows.push(Row { ps: s.ps, vs: s.vs, tex0: s.tex0, vcb_max: 0, draws: 0, indices: 0, vertices: 0 });
                self.rows.len() - 1
            }
        };
        let r = &mut self.rows[i];
        r.draws += 1;
        r.vcb_max = r.vcb_max.max(s.vcb);
        r.indices += u64::from(s.indices);
        r.vertices += u64::from(s.vertices);
    }

    /// Biggest group first, ties by key (deterministic).
    pub fn sorted(&self) -> Vec<Row> {
        let mut v = self.rows.clone();
        v.sort_by(|a, b| b.draws.cmp(&a.draws).then_with(|| (a.ps, a.vs, a.tex0).cmp(&(b.ps, b.vs, b.tex0))));
        v
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.overflow = 0;
    }
}

// ---- live state -------------------------------------------------------------------------------------------

static OPEN: AtomicBool = AtomicBool::new(false);

struct State {
    cur: Agg,
    last: Vec<Row>,
}

static STATE: Mutex<State> = Mutex::new(State { cur: Agg::new(), last: Vec::new() });

/// The picker window is open (the only thing the draw hooks read when it is not).
pub fn is_open() -> bool {
    OPEN.load(Relaxed)
}

/// The overlay tells whether the picker window is shown this frame. Closing clears the groups and the highlight.
pub fn set_open(open: bool) {
    if OPEN.swap(open, Relaxed) == open {
        return;
    }
    if !open {
        let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
        s.cur.clear();
        s.last = Vec::new();
        drop(s);
        furmask::set_highlight(None);
    }
}

/// Records one skinned draw. Returns at once when the picker is closed; never blocks.
pub fn note(s: &Sample) {
    if !is_open() {
        return;
    }
    if let Ok(mut g) = STATE.try_lock() {
        g.cur.add(s);
    }
}

/// Frame boundary: the groups of the finished frame become the published list. Nothing when closed.
pub fn frame_begin() {
    if !is_open() {
        return;
    }
    if let Ok(mut g) = STATE.try_lock() {
        let done = std::mem::take(&mut g.cur);
        g.last = done.sorted();
    }
}

/// The groups of the last finished frame (empty while closed).
pub fn rows() -> Vec<Row> {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).last.clone()
}

/// Singles out one group in the scene (None clears). Only meaningful while the picker is open.
pub fn set_highlight(k: Option<HighlightKey>) {
    furmask::set_highlight(if is_open() { k } else { None });
}

// ---- the draw hook side -----------------------------------------------------------------------------------

/// A draw is about to be issued (called from `drawdiag::on_draw`, inside the recorder gate). Never unwinds.
pub fn capture(ctx: Obj, args: DrawArgs) {
    if !is_open() {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe { capture_inner(ctx, args) });
}

unsafe fn capture_inner(ctx: Obj, args: DrawArgs) {
    if d3d::ignoring() || recorder::game_context() != ctx as usize {
        return;
    }
    // Opaque-pass shape: full-size colour target with a full-size depth buffer.
    let Some(site) = recorder::current_site(ctx as usize) else { return };
    let screen = d3d::hooks().screen();
    let full = |t: &crate::passes::Target| t.w == screen.0 && t.h == screen.1;
    if !site.rt0.as_ref().is_some_and(full) || !site.depth.as_ref().is_some_and(full) {
        return;
    }
    let (_, vcb0, _) = unsafe { furmask::read_draw(ctx) };
    if vcb0 < PICK_MIN_VCB {
        return;
    }
    let b = unsafe { crate::drawdiag::read_bound(ctx) };
    if b.ps == 0 {
        return;
    }
    let Some(ps) = shaders::registry().hash_of(b.ps) else { return };
    let vs = if b.vs == 0 { None } else { shaders::vs_registry().hash_of(b.vs) };
    let (indices, vertices) = if args.kind.is_indexed() { (args.count, 0) } else { (0, args.count) };
    note(&Sample { ps, vs, vcb: vcb0, indices, vertices, tex0: tex0_of(&b) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drawdiag::TexSig;

    fn s(ps: u32, vs: u32, tex: Option<(u32, u32)>) -> Sample {
        Sample { ps, vs: Some(vs), vcb: 16464, indices: 300, vertices: 0, tex0: tex }
    }

    #[test]
    fn hash_list_add_remove_dedupe_and_normalise() {
        assert_eq!(list_add("", 0x1d50763f), "1d50763f");
        assert_eq!(list_add("1D50763F", 0x1d50763f), "1d50763f", "no duplicate, normalised to lower case");
        assert_eq!(list_add("1d50763f, 0xe0189185", 0x5d00827f), "1d50763f,e0189185,5d00827f");
        assert_eq!(list_add("zz,1", 2), "00000001,00000002", "bad entries dropped");
        assert_eq!(list_remove("1d50763f,e0189185", 0x1d50763f), "e0189185");
        assert_eq!(list_remove("1d50763f", 0x99), "1d50763f");
        assert_eq!(list_remove("1d50763f", 0x1d50763f), "");
        assert_eq!(list_toggle("", 5), ("00000005".to_string(), true));
        assert_eq!(list_toggle("00000005", 5), (String::new(), false));
        assert!(list_has("A, b", 0xa) && !list_has("A, b", 0xc));
        assert_eq!(format_list(&[1, 0xabcdef12]), "00000001,abcdef12");
        assert_eq!(parse_hash(" 0xE0189185 "), Some(0xe0189185));
        assert_eq!(parse_hash("e0189185,1d50763f"), None);
        assert_eq!(parse_hash("xyz"), None);
        assert_eq!(parse_hash("123456789"), None);
    }

    #[test]
    fn hash_list_is_bounded() {
        let mut t = String::new();
        for i in 0..(furmask::MAX_LIST as u32 + 10) {
            t = list_add(&t, i + 1);
        }
        assert_eq!(furmask::parse_hashes(&t).len(), furmask::MAX_LIST);
    }

    #[test]
    fn charr_preset_is_the_verified_list() {
        assert_eq!(CHARR_PRESET, "1d50763f,e0189185,5d00827f");
        assert_eq!(furmask::parse_hashes(CHARR_PRESET).len(), 3);
        assert_eq!(list_add(CHARR_PRESET, 0xe0189185), CHARR_PRESET);
    }

    #[test]
    fn layout_width_scales_with_the_display_and_never_overflows_it() {
        assert_eq!(window_width(1920.0, 1080.0), 460.0);
        assert_eq!(window_width(1920.0, 720.0), 460.0, "never below the 1080p size");
        assert!((window_width(3440.0, 1440.0) - 460.0 * 1440.0 / 1080.0).abs() < 0.01);
        assert_eq!(window_width(3840.0, 4320.0), 920.0, "scale capped at 2");
        assert_eq!(window_width(400.0, 1080.0), 360.0, "at most 90 % of a narrow display");
        assert_eq!(window_width(f32::NAN, f32::NAN), 460.0);
        assert!(picker_width(1920.0, 1080.0) > window_width(1920.0, 1080.0));
        assert!(picker_width(800.0, 1080.0) <= 720.0);
    }

    #[test]
    fn item_width_leaves_room_for_the_label() {
        for (w, sc) in [(460.0f32, 1.0f32), (613.0, 1.33), (300.0, 1.0), (920.0, 2.0), (0.0, 1.0), (f32::NAN, 1.0)] {
            let iw = item_width(w, sc);
            assert!(iw < 0.0, "negative = the label sits in the remaining width");
            let ww = if w.is_finite() && w > 0.0 { w } else { 460.0 };
            assert!(-iw >= ww * 0.45 - 0.01 || -iw >= 160.0 * sc.clamp(1.0, 2.0) - 0.01, "label room for {w}");
            assert!(-iw <= ww * 0.65 + 0.01, "the widget keeps a share of {w}");
        }
        // 460 px window at 1080p: about 207 px for the label, about 250 px for the slider.
        assert!((item_width(460.0, 1.0) + 207.0).abs() < 0.01);
    }

    #[test]
    fn groups_aggregate_by_shaders_and_first_texture_and_sort() {
        let mut a = Agg::new();
        a.add(&s(1, 10, Some((512, 512))));
        a.add(&s(1, 10, Some((512, 512))));
        a.add(&s(1, 10, Some((256, 256)))); // same shaders, another texture: its own group (head vs hands)
        a.add(&Sample { vcb: 20000, indices: 0, vertices: 6, ..s(2, 11, None) });
        let r = a.sorted();
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].ps, r[0].draws, r[0].indices, r[0].tex0), (1, 2, 600, Some((512, 512))));
        assert_eq!(r[2].ps, 2);
        assert_eq!((r[2].vcb_max, r[2].vertices), (20000, 6));
    }

    #[test]
    fn groups_are_bounded_and_overflow_is_counted_not_stored() {
        let mut a = Agg::new();
        for i in 0..(MAX_GROUPS as u32 + 20) {
            a.add(&s(i + 1, 1, None));
        }
        assert_eq!(a.len(), MAX_GROUPS);
        assert_eq!(a.overflow, 20);
        a.add(&s(1, 1, None)); // an existing group still counts
        assert_eq!(a.sorted()[0].draws, 2);
        assert_eq!(a.overflow, 20);
        a.clear();
        assert!(a.is_empty() && a.overflow == 0);
    }

    #[test]
    fn tex0_is_the_lowest_slot_with_a_size() {
        let t = |slot: u8, w: u32| (TexSig { slot, fmt: 77, w, h: w, mips: 1 }, 1usize);
        let b = Bound { srvs: vec![t(3, 64), t(1, 512), t(2, 128)], ..Default::default() };
        assert_eq!(tex0_of(&b), Some((512, 512)));
        let b = Bound { srvs: vec![t(0, 0), t(2, 128)], ..Default::default() };
        assert_eq!(tex0_of(&b), Some((128, 128)), "slot 0 is not a 2D texture: skipped");
        assert_eq!(tex0_of(&Bound::default()), None);
    }

    #[test]
    fn highlight_filter_singles_out_one_group() {
        let k = HighlightKey { ps: 1, vs: Some(10), tex0: Some((512, 512)) };
        assert!(k.matches(1, Some(10), Some((512, 512))));
        assert!(!k.matches(2, Some(10), Some((512, 512))), "other pixel shader");
        assert!(!k.matches(1, Some(11), Some((512, 512))), "other vertex shader");
        assert!(!k.matches(1, None, Some((512, 512))), "unknown vertex shader");
        assert!(!k.matches(1, Some(10), Some((256, 256))), "other texture (hands, not head)");
        assert!(!k.matches(1, Some(10), None));
        let loose = HighlightKey { ps: 1, vs: None, tex0: None };
        assert!(loose.matches(1, Some(99), None) && loose.matches(1, None, Some((1, 1))));
        assert!(!loose.matches(2, None, None));
        assert!(k.needs_tex() && !loose.needs_tex());
        let r = Row { ps: 1, vs: Some(10), tex0: Some((512, 512)), vcb_max: 0, draws: 1, indices: 0, vertices: 0 };
        assert_eq!(r.key(), k);
    }

    /// The only test touching the globals of the picker (tests run in parallel).
    #[test]
    fn closed_picker_costs_nothing_and_closing_clears_everything() {
        set_open(false);
        note(&s(1, 1, None));
        frame_begin();
        assert!(rows().is_empty(), "closed: nothing stored");
        assert!(STATE.lock().unwrap().cur.is_empty(), "closed: nothing aggregated");
        // capture with a null context must not touch it while closed.
        capture(std::ptr::null_mut(), DrawArgs::indexed(3));

        set_open(true);
        note(&s(1, 1, Some((8, 8))));
        note(&s(1, 1, Some((8, 8))));
        assert!(rows().is_empty(), "published only at the frame boundary");
        frame_begin();
        let r = rows();
        assert_eq!((r.len(), r[0].draws), (1, 2));
        // The next frame starts empty.
        frame_begin();
        assert!(rows().is_empty());

        note(&s(2, 2, None));
        frame_begin();
        assert_eq!(rows().len(), 1);
        furmask::set_highlight(Some(HighlightKey { ps: 2, vs: None, tex0: None }));
        assert!(furmask::highlight().is_some());
        set_open(false);
        assert!(rows().is_empty() && STATE.lock().unwrap().cur.is_empty());
        assert!(furmask::highlight().is_none(), "closing clears the highlight");
        // While closed a highlight request is ignored.
        set_highlight(Some(HighlightKey { ps: 2, vs: None, tex0: None }));
        assert!(furmask::highlight().is_none());
    }
}
