//! Guided HDR calibration (`docs/hdr10.md`, section "Calibration"): full-screen test patterns drawn by the mod's OWN final pass.
//!
//! WHY NOT THE OVERLAY'S DRAW LIST. hudhook draws ImGui after the final pass, straight into the back buffer: its colours are 8-bit
//! values 0..1 (80 nits at most in scRGB; in HDR10 they would be PQ code values, i.e. up to 10000 nits). A block of 1000 or 4000
//! nits cannot be expressed that way, so the patterns are made by the final-pass shader (`CALIB_PATTERN` in `final_scrgb.hlsl`), in
//! the same output encoding as the picture (scRGB or PQ) and in absolute nits. The overlay only adds the labels on top (dim text),
//! at the cell positions given here.
//!
//! The flag `calibration_pattern` (0 off, 1 peak, 2 paper white, 3 black level) is runtime only: never saved, never applied at start.
//! It returns to 0 when set to 0, when the overlay closes, when HDR output is not active, and after `WATCHDOG_MS`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed};

/// The pattern turns itself off after this long.
pub const WATCHDOG_MS: u64 = 30_000;

pub const OFF: u8 = 0;
pub const PEAK: u8 = 1;
pub const PAPER: u8 = 2;
pub const BLACK: u8 = 3;

/// Nits of the peak blocks, left to right.
pub const PEAK_NITS: [f32; 8] = [400.0, 600.0, 800.0, 1000.0, 1400.0, 2000.0, 3000.0, 4000.0];
/// Nits of the black-level patches, left to right.
pub const BLACK_NITS: [f32; 8] = [0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0];
/// Shares of paper white of the three paper-white patches: the white UI swatch, an 18 % grey card, a 5 % dark grey.
#[allow(dead_code)] // mirrored by the shader; read by the host reference
pub const PAPER_SHARES: [f32; 3] = [1.0, 0.18, 0.05];

/// Cells of the 8-block patterns: x from `CELL_X0`, each `CELL_W` wide; the block fills `FILL` of the cell, centred.
pub const CELL_X0: f32 = 0.1;
pub const CELL_W: f32 = 0.1;
#[allow(dead_code)]
pub const FILL: f32 = 0.85;
/// Rows (share of the screen height): the blocks are between `ROW.0` and `ROW.1`.
pub const ROW: (f32, f32) = (0.35, 0.65);
/// Paper-white patterns: three cells from `PAPER_X0`, each `PAPER_W` wide.
pub const PAPER_X0: f32 = 0.2;
pub const PAPER_W: f32 = 0.2;
pub const PAPER_ROW: (f32, f32) = (0.3, 0.7);

pub fn name(p: u8) -> &'static str {
    match p {
        PEAK => "peak",
        PAPER => "paper white",
        BLACK => "black level",
        _ => "off",
    }
}

/// How many cells the pattern has.
pub fn cells(p: u8) -> usize {
    match p {
        PEAK | BLACK => 8,
        PAPER => 3,
        _ => 0,
    }
}

#[allow(dead_code)]
/// The cell under the screen position `uv` (0..1 both), or None (background). Mirrors `calib_pattern` of the shader.
pub fn cell_at(p: u8, uv: (f32, f32)) -> Option<usize> {
    let (x0, w, n, row, fill) = match p {
        PEAK | BLACK => (CELL_X0, CELL_W, 8usize, ROW, FILL),
        PAPER => (PAPER_X0, PAPER_W, 3usize, PAPER_ROW, FILL),
        _ => return None,
    };
    if uv.1 < row.0 || uv.1 >= row.1 {
        return None;
    }
    let t = (uv.0 - x0) / w;
    if t < 0.0 {
        return None;
    }
    let i = t.floor() as usize;
    if i >= n {
        return None;
    }
    let f = t - i as f32;
    let margin = (1.0 - fill) * 0.5;
    (f >= margin && f < 1.0 - margin).then_some(i)
}

#[allow(dead_code)]
/// The luminance (nits) the pattern shows in cell `i`, `paper_white` being the configured paper white. Mirrors the shader.
pub fn nits_of(p: u8, i: usize, paper_white: f32) -> Option<f32> {
    match p {
        PEAK => PEAK_NITS.get(i).copied(),
        BLACK => BLACK_NITS.get(i).copied(),
        PAPER => PAPER_SHARES.get(i).map(|s| s * paper_white),
        _ => None,
    }
}

/// Where the label of cell `i` goes (centre x, y below the blocks), as shares of the screen.
pub fn label_pos(p: u8, i: usize) -> Option<(f32, f32)> {
    if i >= cells(p) {
        return None;
    }
    let (x0, w, row) = if p == PAPER { (PAPER_X0, PAPER_W, PAPER_ROW) } else { (CELL_X0, CELL_W, ROW) };
    Some((x0 + w * (i as f32 + 0.5), row.1 + 0.02))
}

/// The label text of cell `i`.
pub fn label(p: u8, i: usize) -> Option<String> {
    match p {
        PEAK => PEAK_NITS.get(i).map(|n| format!("{n:.0}")),
        BLACK => BLACK_NITS.get(i).map(|n| format!("{n}")),
        PAPER => ["white UI 100 %", "grey card 18 %", "dark grey 5 %"].get(i).map(|s| s.to_string()),
        _ => None,
    }
}

/// The last block you can still tell apart from the next one (index into `PEAK_NITS`) is the display's real peak. This maps a measured
/// peak (nits) to the block index it falls in (the highest block not above it), for the "use display value" hint.
pub fn peak_block_for(nits: f32) -> usize {
    PEAK_NITS.iter().rposition(|n| *n <= nits).unwrap_or(0)
}

/// `Use display value`: the display's reported peak as a setting (clamped to the range of `peak_nits`), None when unknown.
pub fn peak_from_display(max_luminance: Option<f32>, range: (f32, f32)) -> Option<f32> {
    max_luminance.filter(|v| v.is_finite() && *v > 0.0).map(|v| v.clamp(range.0, range.1))
}

/// Black-level guidance: the patch index (0-based) the user says is the dimmest one they can see, to a sentence.
pub fn black_guidance(dimmest_visible: Option<usize>) -> String {
    match dimmest_visible {
        None => "Nothing is visible: the display (or room) hides everything up to 1 nit; OLED / local dimming normally shows 0.005 to 0.05.".to_string(),
        Some(i) => match BLACK_NITS.get(i) {
            Some(n) if *n <= 0.02 => format!("The {n} nit patch is visible: deep blacks, nothing to change."),
            Some(n) if *n <= 0.2 => format!("The {n} nit patch is the dimmest you see: normal for an LCD with a bright room; nothing to change."),
            Some(n) => format!("Only the {n} nit patch and above are visible: the black level of the display is lifted. Lower the room light or the display's black level / local dimming setting; the mod does not lift blacks."),
            None => String::new(),
        },
    }
}

// ---- state ----------------------------------------------------------------------------------------------------------------------

static PATTERN: AtomicU8 = AtomicU8::new(OFF);
static SINCE_MS: AtomicU64 = AtomicU64::new(0);
/// The pattern was started from the overlay (so it ends when the overlay closes). One started by the config flag (hot reload, the dev
/// flow) has no overlay to close and only ends by the flag or the watchdog.
static VIA_OVERLAY: AtomicBool = AtomicBool::new(false);

pub fn via_overlay() -> bool {
    VIA_OVERLAY.load(Relaxed)
}

pub fn set_via_overlay(v: bool) {
    VIA_OVERLAY.store(v, Relaxed);
}

/// Does the overlay-closed rule apply? Only to a pattern the overlay started: then `overlay_open` is the real state, else it counts as open.
pub fn overlay_counts_as_open(overlay_open: bool, via_overlay: bool) -> bool {
    overlay_open || !via_overlay
}

pub fn pattern() -> u8 {
    PATTERN.load(Relaxed)
}

/// What a `set` request turns into: patterns need HDR output (the final-pass substitute is the only drawer).
pub fn requested(p: u8, hdr_output_active: bool) -> u8 {
    if p > BLACK || !hdr_output_active { OFF } else { p }
}

/// Sets the pattern (clamped; OFF when HDR output is not active). Returns the value in effect. The final-pass substitute is rebuilt by
/// the caller (`output::calibration_changed`) when this returns a different value than before.
pub fn set(p: u8, hdr_output_active: bool, now_ms: u64) -> u8 {
    let v = requested(p, hdr_output_active);
    if PATTERN.swap(v, Relaxed) != v {
        SINCE_MS.store(now_ms, Relaxed);
    }
    v
}

/// What to do about the running pattern this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Keep,
    TurnOff(&'static str),
}

/// The safety rules: the pattern ends when the overlay is closed, when HDR output is gone, and after the watchdog time.
pub fn step(pattern: u8, since_ms: u64, now_ms: u64, overlay_open: bool, hdr_output_active: bool) -> Action {
    if pattern == OFF {
        Action::Keep
    } else if !overlay_open {
        Action::TurnOff("overlay closed")
    } else if !hdr_output_active {
        Action::TurnOff("HDR output is not active")
    } else if now_ms.saturating_sub(since_ms) >= WATCHDOG_MS {
        Action::TurnOff("30 s watchdog")
    } else {
        Action::Keep
    }
}

pub fn since_ms() -> u64 {
    SINCE_MS.load(Relaxed)
}

/// Status line.
pub fn status_text(hdr_output_active: bool, now_ms: u64) -> String {
    let p = pattern();
    if !hdr_output_active {
        return "unavailable (HDR output is not active)".to_string();
    }
    if p == OFF {
        return "off".to_string();
    }
    let left = WATCHDOG_MS.saturating_sub(now_ms.saturating_sub(since_ms())) / 1000;
    format!("pattern {p} ({}) on, turns off in {left} s or when the overlay closes", name(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_found_inside_blocks_only() {
        // middle of block 0 and 7 of the peak pattern
        assert_eq!(cell_at(PEAK, (0.15, 0.5)), Some(0));
        assert_eq!(cell_at(PEAK, (0.85, 0.5)), Some(7));
        // gaps between blocks, outside the row, outside the cells
        assert_eq!(cell_at(PEAK, (0.2, 0.5)), None, "boundary of cells 0/1 is a gap");
        assert_eq!(cell_at(PEAK, (0.15, 0.2)), None);
        assert_eq!(cell_at(PEAK, (0.15, 0.7)), None);
        assert_eq!(cell_at(PEAK, (0.05, 0.5)), None);
        assert_eq!(cell_at(PEAK, (0.95, 0.5)), None);
        assert_eq!(cell_at(OFF, (0.5, 0.5)), None);
        assert_eq!(cell_at(PAPER, (0.3, 0.5)), Some(0));
        assert_eq!(cell_at(PAPER, (0.7, 0.5)), Some(2));
        assert_eq!(cell_at(PAPER, (0.85, 0.5)), None);
        // every cell centre maps to itself
        for p in [PEAK, BLACK, PAPER] {
            for i in 0..cells(p) {
                let (x, _) = label_pos(p, i).unwrap();
                let y = (if p == PAPER { PAPER_ROW } else { ROW }).0 + 0.05;
                assert_eq!(cell_at(p, (x, y)), Some(i), "{p} {i}");
            }
            assert!(label_pos(p, cells(p)).is_none());
        }
    }

    #[test]
    fn values_and_labels_follow_the_pattern() {
        assert_eq!(nits_of(PEAK, 0, 203.0), Some(400.0));
        assert_eq!(nits_of(PEAK, 7, 203.0), Some(4000.0));
        assert_eq!(nits_of(PEAK, 8, 203.0), None);
        assert_eq!(nits_of(PAPER, 0, 250.0), Some(250.0), "the white UI swatch is exactly paper white");
        assert!((nits_of(PAPER, 1, 200.0).unwrap() - 36.0).abs() < 1e-4);
        assert_eq!(nits_of(BLACK, 0, 203.0), Some(0.005));
        assert_eq!(nits_of(OFF, 0, 203.0), None);
        assert_eq!(label(PEAK, 3).unwrap(), "1000");
        assert!(label(BLACK, 0).unwrap().starts_with("0.005"));
        assert!(label(PAPER, 0).unwrap().contains("white"));
        assert!(label(PAPER, 3).is_none());
        // ascending, so "the last distinguishable block" is well defined
        for w in PEAK_NITS.windows(2).chain(BLACK_NITS.windows(2)) {
            assert!(w[0] < w[1]);
        }
        assert!(PEAK_NITS[7] <= 4000.0 && PEAK_NITS[0] >= 400.0, "the blocks span the peak_nits range");
    }

    #[test]
    fn display_value_helpers() {
        assert_eq!(peak_block_for(1000.0), 3);
        assert_eq!(peak_block_for(1200.0), 3);
        assert_eq!(peak_block_for(300.0), 0);
        assert_eq!(peak_block_for(9000.0), 7);
        assert_eq!(peak_from_display(Some(1250.0), (400.0, 4000.0)), Some(1250.0));
        assert_eq!(peak_from_display(Some(10000.0), (400.0, 4000.0)), Some(4000.0));
        assert_eq!(peak_from_display(Some(f32::NAN), (400.0, 4000.0)), None);
        assert_eq!(peak_from_display(None, (400.0, 4000.0)), None);
        assert!(black_guidance(None).contains("Nothing is visible"));
        assert!(black_guidance(Some(0)).contains("deep blacks"));
        assert!(black_guidance(Some(7)).contains("lifted"));
        assert!(black_guidance(Some(99)).is_empty());
    }

    #[test]
    fn a_pattern_needs_hdr_output_and_is_clamped() {
        assert_eq!(requested(2, true), 2);
        assert_eq!(requested(2, false), OFF);
        assert_eq!(requested(9, true), OFF);
        assert_eq!(requested(0, true), OFF);
    }

    #[test]
    fn a_config_started_pattern_does_not_need_the_overlay() {
        assert!(overlay_counts_as_open(false, false), "hot-reload pattern with the overlay closed stays");
        assert!(!overlay_counts_as_open(false, true), "overlay pattern ends with the overlay");
        assert!(overlay_counts_as_open(true, true));
        assert_eq!(step(PEAK, 0, 100, overlay_counts_as_open(false, false), true), Action::Keep);
        assert_eq!(step(PEAK, 0, 100, overlay_counts_as_open(false, true), true), Action::TurnOff("overlay closed"));
        assert_eq!(step(PEAK, 0, WATCHDOG_MS, overlay_counts_as_open(false, false), true), Action::TurnOff("30 s watchdog"));
    }

    #[test]
    fn the_pattern_always_ends() {
        // off stays off
        assert_eq!(step(OFF, 0, 1_000_000, false, false), Action::Keep);
        // running, overlay open, HDR on: kept until the watchdog
        assert_eq!(step(PEAK, 1_000, 1_000 + WATCHDOG_MS - 1, true, true), Action::Keep);
        assert_eq!(step(PEAK, 1_000, 1_000 + WATCHDOG_MS, true, true), Action::TurnOff("30 s watchdog"));
        // overlay closes
        assert_eq!(step(PAPER, 1_000, 1_100, false, true), Action::TurnOff("overlay closed"));
        // HDR output gone
        assert_eq!(step(BLACK, 1_000, 1_100, true, false), Action::TurnOff("HDR output is not active"));
        // clock before the start does not trigger the watchdog
        assert_eq!(step(PEAK, 5_000, 4_000, true, true), Action::Keep);
        // a simulated render loop with every pattern: off within the watchdog time whatever else happens
        for p in [PEAK, PAPER, BLACK] {
            let mut now = 0;
            loop {
                now += 16;
                if let Action::TurnOff(_) = step(p, 0, now, true, true) {
                    break;
                }
                assert!(now <= WATCHDOG_MS + 16);
            }
        }
    }
}
