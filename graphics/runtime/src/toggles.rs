//! v17: per-effect on / off switches (overlay checkbox at the top of each effect section, `<effect>_enabled` in config.ini) and a
//! runtime-only master "all effects off" (A/B against the engine). An effect switched off reads as strength 0 (or its neutral value:
//! bloom 1 = the engine's own, highlight boost 1) through the module's own getter, the same code path as the slider at 0: no passes,
//! nothing bound; its slider values are kept (and saved). A missing key reads as on, so existing configs are unaffected.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// The switchable effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fx {
    Bloom,
    Highlight,
    Ao,
    Halo,
    Ssr,
    Shafts,
    Contact,
    Aerial,
    Exposure,
    LocalTone,
    Lens,
    Grading,
    FurLook,
    Rim,
}

pub const ALL: [Fx; 14] = [Fx::Bloom, Fx::Highlight, Fx::Ao, Fx::Halo, Fx::Ssr, Fx::Shafts, Fx::Contact, Fx::Aerial, Fx::Exposure, Fx::LocalTone, Fx::Lens, Fx::Grading, Fx::FurLook, Fx::Rim];

impl Fx {
    pub fn index(self) -> usize {
        ALL.iter().position(|&f| f == self).unwrap_or(0)
    }

    /// The config.ini key.
    pub fn key(self) -> &'static str {
        match self {
            Fx::Bloom => "bloom_enabled",
            Fx::Highlight => "highlight_enabled",
            Fx::Ao => "ao_enabled",
            Fx::Halo => "halo_enabled",
            Fx::Ssr => "ssr_enabled",
            Fx::Shafts => "shafts_enabled",
            Fx::Contact => "contact_enabled",
            Fx::Aerial => "aerial_enabled",
            Fx::Exposure => "autoexp_enabled",
            Fx::LocalTone => "localtone_enabled",
            Fx::Lens => "lens_enabled",
            Fx::Grading => "grade_enabled",
            Fx::FurLook => "fur_look_enabled",
            Fx::Rim => "rim_enabled",
        }
    }

    /// The name in the status line.
    pub fn name(self) -> &'static str {
        match self {
            Fx::Bloom => "bloom",
            Fx::Highlight => "highlight boost",
            Fx::Ao => "AO",
            Fx::Halo => "halo",
            Fx::Ssr => "SSR",
            Fx::Shafts => "sun shafts",
            Fx::Contact => "contact shadows",
            Fx::Aerial => "aerial",
            Fx::Exposure => "auto exposure",
            Fx::LocalTone => "local tone",
            Fx::Lens => "lens",
            Fx::Grading => "grading",
            Fx::FurLook => "fur look",
            Fx::Rim => "rim light",
        }
    }
}

/// The saved switches (config.ini): true = on.
#[derive(Clone, Debug, PartialEq)]
pub struct Toggles(pub [bool; 14]);

impl Default for Toggles {
    fn default() -> Self {
        Toggles([true; 14])
    }
}

impl Toggles {
    pub fn is_key(k: &str) -> bool {
        ALL.iter().any(|f| f.key() == k)
    }

    /// Parses one key (`0` / `false` / `off` = off, anything else = on); false when the key is not one of ours.
    pub fn set(&mut self, k: &str, v: &str) -> bool {
        match ALL.iter().find(|f| f.key() == k) {
            Some(f) => {
                let v = v.trim().to_ascii_lowercase();
                self.0[f.index()] = !(v == "0" || v == "false" || v == "off");
                true
            }
            None => false,
        }
    }

    pub fn get(&self, f: Fx) -> bool {
        self.0[f.index()]
    }

    /// The config.ini block.
    pub fn ini(&self) -> String {
        let mut t = String::from("# Effect switches (overlay: the checkbox at the top of each section): 1 = on (default), 0 = off; an effect switched off\n# keeps its slider values and costs nothing (as at strength 0). A missing key = on.\n");
        for f in ALL {
            t.push_str(&format!("{}={}\n", f.key(), self.get(f) as u8));
        }
        t
    }

    /// The keys whose value differs (the hot reload's diff).
    pub fn changed(a: &Toggles, b: &Toggles) -> Vec<&'static str> {
        ALL.iter().filter(|f| a.get(**f) != b.get(**f)).map(|f| f.key()).collect()
    }

    /// Takes `key` from `new` (the hot reload's merge).
    pub fn take(&mut self, key: &str, new: &Toggles) {
        if let Some(f) = ALL.iter().find(|f| f.key() == key) {
            self.0[f.index()] = new.get(*f);
        }
    }
}

static ON: [AtomicBool; 14] = [const { AtomicBool::new(true) }; 14];
static MASTER_OFF: AtomicBool = AtomicBool::new(false);

/// Applies the saved switches (start, hot reload, overlay). Returns whether the bloom switch changed (its shader is re-installed).
pub fn apply(t: &Toggles) -> bool {
    let before = on(Fx::Bloom);
    for f in ALL {
        ON[f.index()].store(t.get(f), Relaxed);
    }
    before != on(Fx::Bloom)
}

/// The runtime-only master switch ("all effects off": the per-effect states come back when it is turned off again). Returns whether
/// the bloom's effective state changed.
pub fn set_master_off(off: bool) -> bool {
    let before = on(Fx::Bloom);
    MASTER_OFF.store(off, Relaxed);
    before != on(Fx::Bloom)
}

pub fn master_off() -> bool {
    MASTER_OFF.load(Relaxed)
}

/// The effect runs (its own switch on and the master not off).
pub fn on(f: Fx) -> bool {
    !MASTER_OFF.load(Relaxed) && ON[f.index()].load(Relaxed)
}

/// The effective strength: `v`, or 0 when the effect is off.
pub fn eff(f: Fx, v: f32) -> f32 {
    if on(f) { v } else { 0.0 }
}

/// The effective value of an effect whose neutral is `neutral` (bloom 1 = the engine's own, highlight boost 1).
pub fn eff_or(f: Fx, v: f32, neutral: f32) -> f32 {
    if on(f) { v } else { neutral }
}

/// `status_toggles=`.
pub fn status_text() -> String {
    let off: Vec<&str> = ALL.iter().filter(|f| !ON[f.index()].load(Relaxed)).map(|f| f.name()).collect();
    format!(
        "{}{}",
        if master_off() { "ALL EFFECTS OFF (overlay master switch; the per-effect switches below come back when it is turned on); " } else { "" },
        if off.is_empty() { "every effect on".to_string() } else { format!("{} off", off.join(", ")) }
    )
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    apply(&Toggles::default());
    MASTER_OFF.store(false, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggles_parse_default_on_and_map_to_the_effective_strength() {
        let _g = crate::testlock::lock();
        let d = Toggles::default();
        assert!(ALL.iter().all(|f| d.get(*f)), "a missing key is on");
        let mut t = Toggles::default();
        for (v, want) in [("0", false), ("off", false), ("false", false), ("1", true), ("yes", true), ("", true)] {
            assert!(t.set("aerial_enabled", v));
            assert_eq!(t.get(Fx::Aerial), want, "{v:?}");
        }
        assert!(!t.set("aerial_strength", "0"), "not one of ours");
        t.set("lens_enabled", "0");
        let ini = t.ini();
        assert!(ini.contains("lens_enabled=0") && ini.contains("ao_enabled=1") && ALL.iter().all(|f| ini.contains(f.key())), "{ini}");
        // round trip through the ini text
        let mut back = Toggles::default();
        for l in ini.lines().filter(|l| !l.starts_with('#')) {
            let (k, v) = l.split_once('=').unwrap();
            assert!(back.set(k, v));
        }
        assert_eq!(back, t);
        assert_eq!(Toggles::changed(&Toggles::default(), &t), vec!["lens_enabled"]);
        // the effective values
        apply(&t);
        assert_eq!(eff(Fx::Lens, 0.35), 0.0);
        assert_eq!(eff(Fx::Ao, 1.2), 1.2);
        assert_eq!(crate::lens::strength(), 0.0, "the lens getter reads the switch");
        // master: everything off, the per-effect states come back after
        set_master_off(true);
        assert!(ALL.iter().all(|f| !on(*f)));
        assert_eq!(eff_or(Fx::Bloom, 0.6, 1.0), 1.0);
        assert_eq!(eff_or(Fx::Highlight, 2.0, 1.0), 1.0);
        assert!(status_text().contains("ALL EFFECTS OFF"));
        set_master_off(false);
        assert!(on(Fx::Ao) && !on(Fx::Lens));
        assert!(status_text().contains("lens off"), "{}", status_text());
        reset_for_tests();
        assert!(status_text().contains("every effect on"));
    }
}
