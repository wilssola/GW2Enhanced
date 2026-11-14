//! Float scene upgrade: decides which render targets the game creates as 8-bit
//! `B8G8R8A8_UNORM` should be created as `R16G16B16A16_FLOAT` instead, so bright values are
//! not clipped before the post chain. Pure decisions only; `d3d.rs` applies them.
//!
//! The design is stateless: textures are upgraded when created, and views are fixed up by asking
//! the viewed resource for its real format (see `view_format`), so no pointer is ever remembered.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::passes::{FMT_B8G8R8A8_UNORM, FMT_R16G16B16A16_FLOAT};

const FMT_R8G8B8A8_UNORM: u32 = 28;
const FMT_R10G10B10A2_UNORM: u32 = 24;

pub const FMT_BGRA: u32 = FMT_B8G8R8A8_UNORM;
pub const BIND_SHADER_RESOURCE: u32 = 0x8;
pub const BIND_RENDER_TARGET: u32 = 0x20;
pub const USAGE_DEFAULT: u32 = 0;

/// The parts of `D3D11_TEXTURE2D_DESC` the upgrade rule looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TexDesc {
    pub w: u32,
    pub h: u32,
    pub fmt: u32,
    pub bind_flags: u32,
    pub mips: u32,
    pub samples: u32,
    pub usage: u32,
    pub cpu_access: u32,
    pub misc_flags: u32,
    pub quality: u32,
    pub array: u32,
}

/// `Some(R16G16B16A16_FLOAT)` for the scene's colour targets (full screen size or exactly a
/// quarter of it), `None` for everything else.
pub fn target_format(desc: &TexDesc, screen: (u32, u32)) -> Option<u32> {
    let (sw, sh) = screen;
    if sw == 0 || sh == 0 {
        return None;
    }
    let size_ok = (desc.w == sw && desc.h == sh) || (desc.w == sw / 4 && desc.h == sh / 4 && desc.w > 0 && desc.h > 0);
    let both = BIND_RENDER_TARGET | BIND_SHADER_RESOURCE;
    (desc.fmt == FMT_B8G8R8A8_UNORM
        && desc.bind_flags & both == both
        && desc.mips == 1
        && desc.samples == 1
        && desc.usage == USAGE_DEFAULT
        && desc.cpu_access == 0
        && desc.misc_flags == 0
        && desc.quality == 0
        && desc.array == 1
        && size_ok)
        .then_some(FMT_R16G16B16A16_FLOAT)
}

/// The texture slot may only be hooked when both view slots were: otherwise upgraded textures
/// would get 8-bit views and fail.
pub fn may_upgrade(rtv_patched: bool, srv_patched: bool) -> bool {
    rtv_patched && srv_patched
}

/// Format a view must use. A texture we upgraded is float but the game still asks for a view as
/// `B8G8R8A8_UNORM` (or `R8G8B8A8_UNORM`, the back buffer of a float swapchain); only those
/// combinations are rewritten. Typeless / UNKNOWN views and
/// all other textures are left as the game asked.
pub fn view_format(view_fmt: u32, resource_fmt: u32) -> u32 {
    // 87: an upgraded scene target; 28: the back buffer once the swapchain is float16 (scRGB output).
    if (view_fmt == FMT_B8G8R8A8_UNORM || view_fmt == FMT_R8G8B8A8_UNORM) && resource_fmt == FMT_R16G16B16A16_FLOAT {
        FMT_R16G16B16A16_FLOAT
    } else if view_fmt == FMT_R8G8B8A8_UNORM && resource_fmt == FMT_R10G10B10A2_UNORM {
        // the back buffer of an HDR10 swapchain (hdr_format=hdr10)
        FMT_R10G10B10A2_UNORM
    } else {
        view_fmt
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILED: AtomicBool = AtomicBool::new(false);
/// v18: `hdr_upgrade=0` (diagnostic, at start): never upgrade.
static FORCED_OFF: AtomicBool = AtomicBool::new(false);

/// v18: the diagnostic switch (config `hdr_upgrade`, read at start).
pub fn set_forced_off(off: bool) {
    FORCED_OFF.store(off, Relaxed);
}

pub fn forced_off() -> bool {
    FORCED_OFF.load(Relaxed)
}

/// Turns the texture upgrade on or off (from the config; the overlay may flip it later).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// True while new textures may be upgraded: enabled and no upgrade has failed yet.
pub fn active() -> bool {
    ENABLED.load(Relaxed) && !FAILED.load(Relaxed) && !FORCED_OFF.load(Relaxed)
}

static UPGRADED_ANY: AtomicBool = AtomicBool::new(false);

/// A scene target was created as float.
pub fn note_upgraded() {
    UPGRADED_ANY.store(true, Relaxed);
}

/// The enable gate may open: the upgrade is on, nothing failed and at least one target is float.
pub fn ready() -> bool {
    ready_from(active(), UPGRADED_ANY.load(Relaxed))
}

fn ready_from(active: bool, any_upgraded: bool) -> bool {
    active && any_upgraded
}

/// Float scene targets exist (and no upgrade failed), whatever the user's Enabled says: the targets stay float
/// after the switch-off, so the frame still has to be recognised for the vanilla emulation to work.
pub fn float_scene_exists() -> bool {
    !FAILED.load(Relaxed) && UPGRADED_ANY.load(Relaxed)
}

/// An upgraded creation failed: no further textures are upgraded.
pub fn disable_after_failure() {
    FAILED.store(true, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: (u32, u32) = (3440, 1440);

    fn scene(w: u32, h: u32) -> TexDesc {
        TexDesc { w, h, fmt: 87, bind_flags: BIND_RENDER_TARGET | BIND_SHADER_RESOURCE, mips: 1, samples: 1, usage: 0, cpu_access: 0, misc_flags: 0, quality: 0, array: 1 }
    }

    #[test]
    fn the_back_buffer_view_of_an_hdr10_swapchain_follows_the_resource() {
        assert_eq!(view_format(28, 24), 24);
        assert_eq!(view_format(28, 10), 10);
        assert_eq!(view_format(87, 24), 87, "only the back buffer's RGBA8 view is rewritten");
        assert_eq!(view_format(28, 28), 28);
        assert_eq!(view_format(0, 24), 0);
    }

    #[test]
    fn ready_needs_active_upgrade_and_an_upgraded_target() {
        assert!(ready_from(true, true));
        assert!(!ready_from(true, false));
        assert!(!ready_from(false, true));
        assert!(!ready_from(false, false));
    }

    #[test]
    fn upgrades_full_size_scene_target() {
        assert_eq!(target_format(&scene(3440, 1440), SCREEN), Some(10));
    }

    #[test]
    fn upgrades_quarter_size_bloom_target() {
        assert_eq!(target_format(&scene(860, 360), SCREEN), Some(10));
    }

    #[test]
    fn leaves_sixteenth_size_exposure_target() {
        assert_eq!(target_format(&scene(215, 215), SCREEN), None);
    }

    #[test]
    fn leaves_swapchain_format() {
        let d = TexDesc { fmt: 28, ..scene(3440, 1440) };
        assert_eq!(target_format(&d, SCREEN), None);
    }

    #[test]
    fn leaves_depth_and_shadow_maps() {
        // D24_UNORM_S8_UINT depth, typeless shadow map (R32_TYPELESS), depth+SRV binds.
        let depth = TexDesc { fmt: 45, bind_flags: 0x40, ..scene(3440, 1440) };
        let shadow = TexDesc { fmt: 39, bind_flags: 0x40 | BIND_SHADER_RESOURCE, ..scene(2048, 2048) };
        assert_eq!(target_format(&depth, SCREEN), None);
        assert_eq!(target_format(&shadow, SCREEN), None);
        // Right format, wrong binds (render target only / shader resource only).
        assert_eq!(target_format(&TexDesc { bind_flags: BIND_RENDER_TARGET, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { bind_flags: BIND_SHADER_RESOURCE, ..scene(3440, 1440) }, SCREEN), None);
    }

    #[test]
    fn leaves_msaa_and_staging() {
        assert_eq!(target_format(&TexDesc { samples: 4, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { mips: 5, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { usage: 3, cpu_access: 0x20000, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { cpu_access: 0x10000, ..scene(3440, 1440) }, SCREEN), None);
    }

    #[test]
    fn leaves_textures_with_misc_flags() {
        // GDI-compatible (0x200) and shared (0x2) are invalid for float formats.
        assert_eq!(target_format(&TexDesc { misc_flags: 0x200, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { misc_flags: 0x2, ..scene(3440, 1440) }, SCREEN), None);
    }

    #[test]
    fn leaves_nonzero_quality() {
        assert_eq!(target_format(&TexDesc { quality: 1, ..scene(3440, 1440) }, SCREEN), None);
    }

    #[test]
    fn leaves_texture_arrays() {
        assert_eq!(target_format(&TexDesc { array: 6, ..scene(3440, 1440) }, SCREEN), None);
        assert_eq!(target_format(&TexDesc { array: 0, ..scene(3440, 1440) }, SCREEN), None);
    }

    #[test]
    fn odd_screen_size_floors_quarter() {
        assert_eq!(target_format(&scene(860, 360), (3441, 1441)), Some(10));
        assert_eq!(target_format(&scene(861, 361), (3441, 1441)), None);
    }

    #[test]
    fn upgrades_only_with_all_view_patches() {
        assert!(may_upgrade(true, true));
        assert!(!may_upgrade(true, false));
        assert!(!may_upgrade(false, true));
        assert!(!may_upgrade(false, false));
    }

    #[test]
    fn follows_new_screen_size() {
        let t = scene(1920, 1080);
        assert_eq!(target_format(&t, SCREEN), None);
        assert_eq!(target_format(&t, (1920, 1080)), Some(10));
        assert_eq!(target_format(&t, (0, 0)), None);
    }

    #[test]
    fn view_rewritten_only_for_float_resource_with_8bit_view() {
        assert_eq!(view_format(87, 10), 10);
    }

    #[test]
    fn view_left_alone_otherwise() {
        assert_eq!(view_format(87, 87), 87); // ordinary 8-bit target
        assert_eq!(view_format(87, 91), 87);
        assert_eq!(view_format(0, 10), 0); // UNKNOWN: inherits the float format by itself
        assert_eq!(view_format(9, 10), 9); // typeless
        assert_eq!(view_format(10, 10), 10);
        assert_eq!(view_format(28, 87), 28);
        assert_eq!(view_format(28, 28), 28);
        assert_eq!(view_format(41, 10), 41); // other formats are not ours to change
    }

    #[test]
    fn rgba8_view_of_float_backbuffer_is_rewritten() {
        // bgfx creates the back-buffer RTV as R8G8B8A8_UNORM; the swapchain is float in scRGB mode.
        assert_eq!(view_format(28, 10), 10);
    }
}
