//! Enable gate: the HDR substitutes and the resolve only run on frames the classifier recognised.
//! Also the user's `Enabled` switch and what it turns off (`switches`).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::output::OutputMode;

static USER_ENABLED: AtomicBool = AtomicBool::new(true);

/// The user's `Enabled` setting (config / overlay). Applied at the next frame boundary.
pub fn set_user_enabled(on: bool) {
    USER_ENABLED.store(on, Ordering::Relaxed);
}

pub fn user_enabled() -> bool {
    USER_ENABLED.load(Ordering::Relaxed)
}

/// What a frame boundary switches, given the user's `Enabled`, the enable gate and the output mode.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Switches {
    /// New textures may become float.
    pub upgrade_new: bool,
    /// The HDR replacement shaders (light adjust, bright pass, bloom threshold).
    /// The replacement shaders run on recognised frames EVEN WITH `Enabled` off: then they only reproduce the
    /// original look on top of the float scene (vanilla emulation, see `hdr_shaders::set_vanilla`).
    pub substitutes: bool,
    pub resolve: bool,
    /// The final-pass conversion. Stays on in `ScRgb` whatever `Enabled` says: the swapchain is
    /// float already, and the original final shader would write gamma values into it.
    pub final_substitute: bool,
}

pub fn switches(enabled: bool, gate_on: bool, mode: OutputMode) -> Switches {
    let on = enabled && gate_on;
    Switches { upgrade_new: enabled, substitutes: gate_on, resolve: on, final_substitute: crate::output::final_substitute_wanted(mode, gate_on) }
}

/// Enabled after `NEEDED` consecutive matching frames, disabled after `MISSES` consecutive mismatches.
#[derive(Debug, Default)]
pub struct Gate {
    streak: u8,
    misses: u8,
    enabled: bool,
}

pub const NEEDED: u8 = 3;
pub const MISSES: u8 = 2;

impl Gate {
    pub const fn new() -> Self {
        Gate { streak: 0, misses: 0, enabled: false }
    }

    /// Feeds one frame's result; returns the new enabled state.
    pub fn update(&mut self, matched: bool) -> bool {
        if matched {
            self.misses = 0;
            self.streak = (self.streak + 1).min(NEEDED);
            if self.streak >= NEEDED {
                self.enabled = true;
            }
        } else {
            self.streak = 0;
            self.misses = (self.misses + 1).min(MISSES);
            if self.misses >= MISSES {
                self.enabled = false;
            }
        }
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::output::OutputMode;

    #[test]
    fn enabled_off_keeps_final_substitute_in_scrgb() {
        // Even if the gate were somehow on, Enabled off turns every HDR feature off but the final pass.
        for gate_on in [false, true] {
            let s = switches(false, gate_on, OutputMode::ScRgb);
            assert_eq!(s, Switches { upgrade_new: false, substitutes: gate_on, resolve: false, final_substitute: true });
        }
    }

    #[test]
    fn enabled_off_keeps_only_the_vanilla_emulation_in_sdr() {
        for gate_on in [false, true] {
            let s = switches(false, gate_on, OutputMode::Sdr);
            assert_eq!(s, Switches { upgrade_new: false, substitutes: gate_on, resolve: false, final_substitute: false });
        }
    }

    #[test]
    fn enabled_on_follows_the_gate() {
        assert_eq!(switches(true, true, OutputMode::Sdr), Switches { upgrade_new: true, substitutes: true, resolve: true, final_substitute: false });
        assert_eq!(switches(true, false, OutputMode::ScRgb), Switches { upgrade_new: true, substitutes: false, resolve: false, final_substitute: true });
    }

    #[test]
    fn gate_enables_after_three_matches() {
        let mut g = Gate::new();
        assert!(!g.update(true));
        assert!(!g.update(true));
        assert!(g.update(true));
        assert!(g.update(true));
    }

    #[test]
    fn single_mismatch_does_not_disable() {
        let mut g = Gate::new();
        for _ in 0..3 {
            g.update(true);
        }
        assert!(g.update(false));
        // A match in between restarts the count of consecutive mismatches.
        assert!(g.update(true));
        assert!(g.update(false));
        assert!(g.update(true));
    }

    #[test]
    fn gate_disables_after_two_mismatches() {
        let mut g = Gate::new();
        for _ in 0..3 {
            g.update(true);
        }
        assert!(g.update(false));
        assert!(!g.update(false));
        assert!(!g.update(false));
    }

    #[test]
    fn gate_needs_three_again_after_disable() {
        let mut g = Gate::new();
        for _ in 0..3 {
            g.update(true);
        }
        g.update(false);
        g.update(false);
        assert!(!g.update(true));
        assert!(!g.update(true));
        assert!(g.update(true));
        // While not enabled, a mismatch before the third match restarts the count.
        let mut g = Gate::new();
        g.update(true);
        g.update(true);
        g.update(false);
        assert!(!g.update(true));
        assert!(!g.update(true));
        assert!(g.update(true));
    }
}
