//! HDR replacement pixel shaders for the stages of the engine post chain that clamp colour to [0,1].
//!
//! `install_all` only registers the sources with `shaders::install_substitute`; it does not turn
//! substitution on (`shaders::set_enabled` is not called here).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use crate::shaders::install_substitute;

/// Replacement for `ps#f5cd4832` (#13 tone/exposure remap).
pub const LIGHT_ADJUST_HASH: u32 = 0xf5cd4832;
/// Replacement for `ps#2ed4a75f` (#10/#11 bright-pass downsample).
pub const BRIGHT_PASS_HASH: u32 = 0x2ed4a75f;
/// Replacement for `ps#14b1e90e` (#14 bloom threshold).
pub const BLOOM_THRESHOLD_HASH: u32 = 0x14b1e90e;

pub const LIGHT_ADJUST_HLSL: &str = include_str!("../shaders/light_adjust_hdr.hlsl");
pub const BRIGHT_PASS_HLSL: &str = include_str!("../shaders/bright_pass_hdr.hlsl");

pub const LIGHT_ADJUST_ENTRY: &str = "light_adjust";
pub const BRIGHT_PASS_ENTRY: &str = "bright_pass_downsample";
pub const BLOOM_THRESHOLD_ENTRY: &str = "bloom_threshold";

/// The four engine shaders the mod depends on, as the name logged in `seen engine shader <name>`.
pub fn known_engine_hash(hash: u32) -> Option<&'static str> {
    match hash {
        LIGHT_ADJUST_HASH => Some("f5cd4832"),
        BRIGHT_PASS_HASH => Some("2ed4a75f"),
        BLOOM_THRESHOLD_HASH => Some("14b1e90e"),
        crate::output::FINAL_HASH => Some("afdc14cb"),
        _ => None,
    }
}

static SEEN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Some(name) the first time one of the four engine hashes is recorded in the registry.
pub fn note_engine_hash(hash: u32) -> Option<&'static str> {
    let name = known_engine_hash(hash)?;
    let mut s = SEEN.lock().ok()?;
    crate::shaders::first_time(&mut s, hash).then_some(name)
}

static BLOOM: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32

/// Bloom strength limits (the visibility rule: bloom is capped).
pub const BLOOM_MAX: f32 = 2.0;

/// Clamps to 0..=2; NaN gives 1 (the game's own strength).
pub fn clamp_bloom(s: f32) -> f32 {
    if s.is_nan() { 1.0 } else { s.clamp(0.0, BLOOM_MAX) }
}

pub fn bloom_strength() -> f32 {
    crate::toggles::eff_or(crate::toggles::Fx::Bloom, f32::from_bits(BLOOM.load(Relaxed)), 1.0)
}

/// The bloom-threshold source with `BLOOM_STRENGTH` defined in front (it scales the bloom output).
pub fn bloom_hlsl(strength: f32) -> String {
    format!("#define BLOOM_STRENGTH {:?}\n{BRIGHT_PASS_HLSL}", clamp_bloom(strength))
}

static VANILLA: AtomicBool = AtomicBool::new(false);

/// The mod is switched off by the user: the substitutes then reproduce the original shaders on top of the float
/// scene (every tap clipped at 1.0, bloom strength 1), so the picture is the vanilla one, not "a bit more bloom".
pub fn vanilla() -> bool {
    VANILLA.load(Relaxed)
}

fn bright_pass_source() -> String {
    if vanilla() { format!("#define VANILLA 1\n{BRIGHT_PASS_HLSL}") } else { BRIGHT_PASS_HLSL.to_string() }
}

fn bloom_source() -> String {
    if vanilla() { format!("#define VANILLA 1\n#define BLOOM_STRENGTH 1.0\n{BRIGHT_PASS_HLSL}") } else { bloom_hlsl(bloom_strength()) }
}

/// Switches the vanilla emulation on or off (called at the frame boundary; cheap when nothing changes).
pub fn set_vanilla(on: bool) {
    if VANILLA.swap(on, Relaxed) != on {
        install_substitute(BRIGHT_PASS_HASH, &bright_pass_source(), BRIGHT_PASS_ENTRY);
        install_substitute(BLOOM_THRESHOLD_HASH, &bloom_source(), BLOOM_THRESHOLD_ENTRY);
        crate::log!("hdr: vanilla emulation {} (bloom shaders {})", if on { "on" } else { "off" }, if on { "clip like the 8-bit engine" } else { "HDR" });
    }
}

/// Sets the bloom strength (clamped; returns the applied value) and re-installs the `14b1e90e`
/// replacement. Installing resets the entry to "not created", so the next bind compiles the new
/// source on that device (the previous compiled shader is left behind, never released: one small
/// object per slider change, and it stays valid if the context still has it bound).
pub fn set_bloom_strength(s: f32) -> f32 {
    let s = clamp_bloom(s);
    if BLOOM.swap(s.to_bits(), Relaxed) != s.to_bits() {
        install_substitute(BLOOM_THRESHOLD_HASH, &bloom_source(), BLOOM_THRESHOLD_ENTRY);
    }
    s
}

/// Registers the three replacements (compiled lazily when first bound). Does not enable substitution.
pub fn install_all() {
    install_substitute(LIGHT_ADJUST_HASH, LIGHT_ADJUST_HLSL, LIGHT_ADJUST_ENTRY);
    install_substitute(BRIGHT_PASS_HASH, &bright_pass_source(), BRIGHT_PASS_ENTRY);
    install_substitute(BLOOM_THRESHOLD_HASH, &bloom_source(), BLOOM_THRESHOLD_ENTRY);
}

#[cfg(test)]
#[path = "hdr_shaders_tests.rs"]
mod tests;
