//! HDR output: scRGB (float16, linear, 1.0 = 80 nits) swapchain when it is safe, SDR otherwise.
//!
//! # Decision ("option 3")
//! `decide` is pure. The output mode is decided ONCE, when the game creates its swapchain
//! (`create_swapchain`, called from the factory `CreateSwapChain` hook, or, for a swapchain created together
//! with the device, `d3d::create_device_and_swap_chain` with `plan_device_swapchain`), and stored in `MODE`.
//! `ScRgb` only when `hdr_output=auto`, Windows HDR is on for the window's output, the requested
//! format is `R8G8B8A8_UNORM` (so the game's own, unchanged request), the effect is not already a flip
//! model (another tool such as ReShade changed it) and the swapchain can be a flip-model one.
//! In every other case the swapchain is created exactly as the game asked and only the HDR OUTPUT is
//! off: the float scene, bloom and tone-map keep working in SDR (headroom 1.0).
//!
//! # How "the swapchain is float" is known
//! State stored at creation: `FLOAT_SC` holds the pointer of the swapchain WE created as float16. The
//! `ResizeBuffers` function body is hooked inline (see `d3d.rs`) and rewrites arguments only when `this == FLOAT_SC`.
//! The pointer is cleared whenever a later swapchain is created as SDR. The same hook follows the game's
//! swapchain in SDR mode too (`GAME_SC`, or any swapchain on the game window), only to keep the screen size
//! current: the game recreates its targets inside the frame that resized, before `Present` could update the size.
//! The game's swapchain is the one on the game's WINDOW (`swapchain_window_is_game`), never decided by device identity.
//!
//! # Failure paths
//! Everything is done inside the hook, before the game sees the swapchain. If creating the float
//! flip swapchain fails, or `IDXGISwapChain3` / `CheckColorSpaceSupport(PRESENT)` / `SetColorSpace1`
//! fails, the float swapchain is released and the creation is retried ONCE with the game's own desc
//! (logged); the mode stays `Sdr`. So a swapchain that reaches the game is either float AND in the
//! scRGB colour space (mode `ScRgb`) or exactly what the game asked for (mode `Sdr`).
//!
//! # Runtime toggles
//! Windows HDR is re-read about once per second (`tick`, from the overlay's render loop) for the log
//! only. A change cannot recreate the swapchain: it takes effect at the next swapchain (re)creation
//! or game restart. Nothing breaks meanwhile: an scRGB swapchain on an SDR display, or SDR content on
//! an HDR display, is composed by Windows.
//!
//! # The final conversion
//! The engine's last pass `ps#afdc14cb` writes gamma-encoded values to the back buffer. In `ScRgb`
//! mode it is replaced by `shaders/final_scrgb.hlsl` (decode * paper_white / 80). The substitute is
//! wanted whenever the mode is `ScRgb`, EVEN WITH THE ENABLE GATE CLOSED (loading screens...): the
//! original would write gamma values into a linear swapchain. With the gate closed nothing else is
//! substituted and the resolve is off, so the scene is plain SDR content, converted and scaled.
//! Only the headroom handed to the resolve depends on the HDR config (`peak / paper_white`). The
//! substitute also clamps its output to `peak / 80`, so nothing can exceed the configured peak whatever
//! the pass sequence. If `afdc14cb` is never created by the game (or the substitute cannot be created)
//! the scRGB picture is wrong: `final_pass_status` reports it (log WARNING after 60 s, overlay line).
//!
//! # HDR10 (experimental, `hdr_format=hdr10`, `hdr10.rs`, `docs/hdr10.md`)
//! `OutputMode::ScRgb` means "HDR output is active" (float scene, headroom, final-pass substitute); WHICH swapchain format carries it
//! is `swap_format()`: float16 scRGB (default) or R10G10B10A2 + colour space G2084/P2020 (the final shader then encodes PQ). The format
//! is chosen when the swapchain is created, never changed afterwards (no format change through `ResizeBuffers`: the game keeps the
//! same swapchain object, and a format switch of buffers it already holds views of is not safe). HDR10 is refused with ReShade present
//! (even with `hdr_output=on`), with Windows HDR off, or after a previous start that did not reach a stable picture (marker file
//! `hdr10.pending`). A failed HDR10 attempt retries ONCE as scRGB, then the existing SDR fallback applies.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

use windows_sys::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use crate::config::{Config, HdrFormat, HdrOutput};
use crate::d3d::{Guid, Obj, SwapChainDesc, ignoring, release, vfn};
use crate::hdr10::{self, FormatRule, SwapFormat};
use crate::log;
use crate::passes::FMT_R16G16B16A16_FLOAT;
use crate::{resolve, shaders};

// ---- values (checked against the Windows SDK 10.0.26100 headers, see the task report) ---------

/// `DXGI_FORMAT_R8G8B8A8_UNORM`.
pub const FMT_R8G8B8A8_UNORM: u32 = 28;
/// `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` / `DXGI_SWAP_EFFECT_FLIP_DISCARD`.
#[allow(dead_code)]
const EFFECT_FLIP_SEQUENTIAL: u32 = 3;
const EFFECT_FLIP_DISCARD: u32 = 4;
/// `DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709` (scRGB, linear): 1, NOT 12.
#[allow(dead_code)] // `hdr10::CS_SCRGB` is the one in use
pub const CS_G10_NONE_P709: u32 = 1;
/// `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (HDR10 as reported by an HDR-on output): 12.
pub const CS_G2084_NONE_P2020: u32 = 12;
/// `DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT`.
const CS_SUPPORT_PRESENT: u32 = 1;
/// Swapchain flags that stay valid for a flip-model swapchain: ALLOW_MODE_SWITCH (2), ALLOW_TEARING (0x800).
const FLIP_FLAGS_KEPT: u32 = 0x2 | 0x800;

/// Vtable slots, counted from the `*Vtbl` structs of `dxgi.h`, `dxgi1_4.h` and `dxgi1_6.h`.
mod vt {
    pub const QUERY_INTERFACE: usize = 0;
    pub const FACTORY_ENUM_ADAPTERS: usize = 7;
    pub const ADAPTER_ENUM_OUTPUTS: usize = 7;
    pub const SWAPCHAIN3_CHECK_COLOR_SPACE_SUPPORT: usize = 37;
    pub const SWAPCHAIN3_SET_COLOR_SPACE1: usize = 38;
    pub const OUTPUT6_GET_DESC1: usize = 27;
}

const IID_SWAPCHAIN3: Guid = Guid(0x94d99bdb, 0xf1f8, 0x4ab0, [0xb2, 0x36, 0x7d, 0xa0, 0x17, 0x0e, 0xda, 0xb1]);
const IID_OUTPUT6: Guid = Guid(0x068346e8, 0xaaec, 0x4b84, [0xad, 0xd7, 0x13, 0x7f, 0x51, 0x3f, 0x77, 0xa1]);
const IID_FACTORY: Guid = Guid(0x7b7166ec, 0x21c7, 0x44ae, [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69]);

// ---- pure decisions --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Sdr,
    ScRgb,
}

/// `ScRgb` only when Windows HDR is on, the swapchain format was still `R8G8B8A8_UNORM` at creation,
/// no other tool changed the swapchain desc, and the request allows it: `Off` never; `Auto` only when
/// no other tool is present (`other_tool_present`: ReShade, whose effects are drawn on the swapchain
/// and are not HDR-safe); `On` ignores that presence (the player opts in). `Sdr` otherwise.
pub fn decide(request: HdrOutput, windows_hdr: bool, swapchain_fmt: u32, other_tool_changed_swapchain: bool, other_tool_present: bool) -> OutputMode {
    let allowed = match request {
        HdrOutput::Off => false,
        HdrOutput::Auto => !other_tool_present,
        HdrOutput::On => true,
    };
    if allowed && windows_hdr && swapchain_fmt == FMT_R8G8B8A8_UNORM && !other_tool_changed_swapchain {
        OutputMode::ScRgb
    } else {
        OutputMode::Sdr
    }
}

/// Which rule decided the mode (logged once at swapchain creation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    RequestOff,
    ViewHooksMissing,
    SwapchainChanged,
    WindowsHdrOff,
    OtherToolPresent,
    Auto,
    ForcedOn,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::RequestOff => "hdr_output=off",
            Rule::ViewHooksMissing => "the view-creation hooks are missing (the float back buffer would get no view)",
            Rule::SwapchainChanged => "another tool changed the swapchain desc (format or flip model); not touching it",
            Rule::WindowsHdrOff => "Windows HDR is off",
            Rule::OtherToolPresent => "ReShade is present and hdr_output=auto (use hdr_output=on to force)",
            Rule::Auto => "hdr_output=auto, Windows HDR on, no other tool present",
            Rule::ForcedOn => "hdr_output=on, Windows HDR on, swapchain unchanged",
        }
    }
}

/// The first rule that applies to the inputs of `decide`; `ScRgb` exactly for `Auto` and `ForcedOn`.
pub fn rule_of(request: HdrOutput, windows_hdr: bool, swapchain_fmt: u32, other_tool_changed_swapchain: bool, other_tool_present: bool) -> Rule {
    if request == HdrOutput::Off {
        Rule::RequestOff
    } else if swapchain_fmt != FMT_R8G8B8A8_UNORM || other_tool_changed_swapchain {
        Rule::SwapchainChanged
    } else if !windows_hdr {
        Rule::WindowsHdrOff
    } else if request == HdrOutput::Auto && other_tool_present {
        Rule::OtherToolPresent
    } else if request == HdrOutput::Auto {
        Rule::Auto
    } else {
        Rule::ForcedOn
    }
}

/// `decide` plus the precondition that the RTV/SRV view hooks are in place (a float16 back buffer
/// needs the R8G8B8A8_UNORM -> float view rewrite), with the rule that decided.
pub fn explain(view_hooks_ok: bool, request: HdrOutput, windows_hdr: bool, swapchain_fmt: u32, changed: bool, other_present: bool) -> (OutputMode, Rule) {
    if !view_hooks_ok && request != HdrOutput::Off {
        return (OutputMode::Sdr, Rule::ViewHooksMissing);
    }
    (decide(request, windows_hdr, swapchain_fmt, changed, other_present), rule_of(request, windows_hdr, swapchain_fmt, changed, other_present))
}

/// A window as the rule for "the game's window" sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: usize,
    /// Created by a thread of this process.
    pub owned: bool,
    pub class: String,
    pub visible: bool,
    /// Client rectangle size.
    pub client: (u32, u32),
}

/// The game's window class starts with this (`ArenaNet_Dx_Window_Class`).
const GAME_CLASS_PREFIX: &str = "ArenaNet";

/// The game's window among the top-level windows of the process: visible, class starting with
/// `ArenaNet`; the biggest client area wins, then the first. Overlay tools create their dummy windows
/// hidden and with their own class.
pub fn pick_game_window(cands: &[WindowInfo]) -> Option<usize> {
    let mut best: Option<&WindowInfo> = None;
    for c in cands.iter().filter(|c| c.owned && c.visible && c.class.starts_with(GAME_CLASS_PREFIX)) {
        let area = |w: &WindowInfo| w.client.0 as u64 * w.client.1 as u64;
        if best.is_none_or(|b| area(c) > area(b)) {
            best = Some(c);
        }
    }
    best.map(|c| c.hwnd)
}

/// Is the window of a swapchain being created the game's? No device identity is involved.
/// `cached` is the game window found earlier (0 = none yet). `info` describes the desc's window
/// (None = it is not a window). A cached window matches on its handle alone. Otherwise the window must
/// belong to this process and either have the game's class, or (no game window known yet) be visible
/// with a client area of exactly the swapchain's size.
pub fn swapchain_window_is_game(cached: usize, desc_hwnd: usize, desc_size: (u32, u32), info: Option<&WindowInfo>) -> bool {
    if desc_hwnd == 0 {
        return false;
    }
    if cached == desc_hwnd {
        return true;
    }
    let Some(i) = info.filter(|i| i.owned) else { return false };
    if i.class.starts_with(GAME_CLASS_PREFIX) {
        return true;
    }
    cached == 0 && i.visible && desc_size.0 != 0 && desc_size.1 != 0 && i.client == desc_size
}

/// Something other than the game's own request is being created: not `R8G8B8A8_UNORM`. The swap
/// effect is NOT a signal: the current game client itself asks for FLIP_DISCARD (seen in the real
/// run without ReShade), and we replace the desc with our own float16 flip-discard copy anyway.
pub fn other_tool_changed(fmt: u32, _effect: u32) -> bool {
    fmt != FMT_R8G8B8A8_UNORM
}

/// A flip-model swapchain needs one sample per pixel; windowed only (the game runs windowed/borderless).
pub fn flip_possible(d: &SwapChainDesc) -> bool {
    d.samples[0] <= 1 && d.windowed != 0
}

/// A COPY of the game's desc, as a float16 flip-discard swapchain with two buffers. Size, window and
/// windowed are kept; flags incompatible with the flip model (GDI-compatible, ...) are dropped.
#[allow(dead_code)] // the float16 case of `desc_for` (tests)
pub fn scrgb_desc(d: &SwapChainDesc) -> SwapChainDesc {
    desc_for(d, SwapFormat::Float16)
}

/// The same for either HDR format (`R16G16B16A16_FLOAT` or `R10G10B10A2_UNORM`): flip-discard, two buffers, flip-valid flags.
pub fn desc_for(d: &SwapChainDesc, fmt: SwapFormat) -> SwapChainDesc {
    SwapChainDesc { format: fmt.dxgi(), effect: EFFECT_FLIP_DISCARD, buffers: 2, flags: d.flags & FLIP_FLAGS_KEPT, ..*d }
}

/// `ResizeBuffers` arguments for a float swapchain (`is_float`): the game passes its old format
/// (R8G8B8A8_UNORM) or UNKNOWN (keep) and a buffer count of 1 (or 0 = keep). The format is kept
/// float, the count is at least 2 (flip model) and the flags are the ones valid for it.
#[allow(dead_code)] // the float16 case of `hdr10::resize_args_for`, kept as the documented building block (tests)
pub fn resize_args(is_float: bool, count: u32, fmt: u32, flags: u32) -> (u32, u32, u32) {
    if !is_float {
        return (count, fmt, flags);
    }
    let fmt = if fmt == FMT_R8G8B8A8_UNORM { FMT_R16G16B16A16_FLOAT } else { fmt };
    let count = if count == 0 { 0 } else { count.max(2) };
    (count, fmt, flags & FLIP_FLAGS_KEPT)
}

/// The back-buffer size after a `ResizeBuffers` call: the swapchain's own desc (`GetDesc` after the
/// call) wins, the call's arguments are the fallback (0 = "keep the current size" there); zero sizes
/// are never taken. None when neither gives a usable size.
pub fn screen_after_resize(desc: Option<(u32, u32)>, args: (u32, u32)) -> Option<(u32, u32)> {
    [desc.unwrap_or((0, 0)), args].into_iter().find(|&(w, h)| w != 0 && h != 0)
}

/// Is the replacement for the engine's final pass wanted? Whenever the mode is `ScRgb`, whatever the
/// gate says: the original would write gamma-encoded values into a linear swapchain.
pub fn final_substitute_wanted(mode: OutputMode, _gate_on: bool) -> bool {
    mode == OutputMode::ScRgb
}

/// May the substitute for `hash` run? With the gate on, any installed one; with it off, only the
/// final pass and only in `ScRgb`.
pub fn substitute_allowed(hash: u32, gate_on: bool, mode: OutputMode) -> bool {
    gate_on || (hash == FINAL_HASH && final_substitute_wanted(mode, gate_on))
}

/// What the current mode and config ask for.
#[derive(Debug, PartialEq)]
pub struct Plan {
    /// Resolve headroom (`peak / paper_white`; 1.0 in SDR).
    pub headroom: f32,
    pub final_substitute: bool,
}

pub fn plan(mode: OutputMode, gate_on: bool, paper_white_nits: f32, peak_nits: f32) -> Plan {
    let headroom = if mode == OutputMode::ScRgb && paper_white_nits > 0.0 && peak_nits >= paper_white_nits { peak_nits / paper_white_nits } else { 1.0 };
    Plan { headroom, final_substitute: final_substitute_wanted(mode, gate_on) }
}

/// Log line for a change of the Windows HDR state seen by the 1 s re-check.
pub fn toggle_note(was_on: bool, now_on: bool, mode: OutputMode) -> Option<String> {
    (was_on != now_on).then(|| {
        format!("output: Windows HDR is now {}; this swapchain stays {mode:?}, the mode is decided again at the next swapchain (re)creation or game restart", if now_on { "on" } else { "off" })
    })
}

// ---- the final-pass substitute ---------------------------------------------------------------

/// `ps#afdc14cb`: the engine's last pass (full-size R8G8B8A8_UNORM target = the back buffer).
pub const FINAL_HASH: u32 = 0xafdc14cb;
pub const FINAL_ENTRY: &str = "final_scrgb";
const FINAL_HLSL_BODY: &str = include_str!("../shaders/final_scrgb.hlsl");

/// The HLSL with `PAPER_WHITE_SCALE = paper_white_nits / 80` and `PEAK_SCALE = peak_nits / 80` defined
/// in front (the output is clamped to the peak).
pub fn final_hlsl(paper_white_nits: f32, peak_nits: f32) -> String {
    final_hlsl_with(paper_white_nits, peak_nits, FinalOpts::default())
}

/// The optional build switches of the final shader. The default is the plain scRGB shader (no extra define is emitted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinalOpts {
    /// Encode BT.2020 PQ for an R10G10B10A2 swapchain.
    pub hdr10: bool,
    /// Test only: no dither (so the shader can be compared to the host reference at 1e-3).
    pub no_dither: bool,
    /// Calibration test pattern 1..3 (0 = the picture).
    pub pattern: u8,
    /// `sdr_gamma` x 100 (220 = gamma 2.2): the SDR range of the picture is decoded with this pure power instead of the piecewise
    /// sRGB curve (`SDR_GAMMA` in `final_scrgb.hlsl`). 0 = the piecewise sRGB curve (no define: the shader as it always was).
    pub gamma_centi: u16,
}

/// `final_hlsl` with the HDR10 encoding and/or a calibration pattern.
pub fn final_hlsl_with(paper_white_nits: f32, peak_nits: f32, opts: FinalOpts) -> String {
    let (headroom, knee) = shoulder_constants(paper_white_nits, peak_nits);
    let mut defs = format!("#define PAPER_WHITE_SCALE {:?}\n#define PEAK_SCALE {:?}\n#define HEADROOM {:?}\n#define KNEE {:?}\n", paper_white_nits / 80.0, peak_nits / 80.0, headroom, knee);
    if opts.hdr10 {
        defs.push_str("#define HDR10_OUTPUT 1\n");
        if opts.no_dither {
            defs.push_str("#define DITHER 0\n");
        }
    }
    if (1..=3).contains(&opts.pattern) {
        defs.push_str(&format!("#define CALIB_PATTERN {}\n", opts.pattern));
    }
    if opts.gamma_centi > 0 {
        defs.push_str(&format!("#define SDR_GAMMA {:?}\n", opts.gamma_centi as f32 / 100.0));
    }
    format!("{defs}{FINAL_HLSL_BODY}")
}

/// The shader for the CURRENT state (swapchain format, calibration pattern, SDR gamma): what the runtime installs.
fn current_final_hlsl(paper_white_nits: f32, peak_nits: f32) -> String {
    let g = active_sdr_gamma();
    LAST_GAMMA.store(g.to_bits(), Relaxed);
    final_hlsl_with(paper_white_nits, peak_nits, FinalOpts { hdr10: hdr10_active(), no_dither: false, pattern: crate::calib::pattern(), gamma_centi: gamma_centi(g) })
}

// ---- the SDR decode gamma (black level) ------------------------------------------------------------------------------------------

/// `sdr_gamma` default: the power an SDR monitor decodes with (docs/hdr10.md "Black level").
pub const SDR_GAMMA_DEFAULT: f32 = 2.2;
/// Range of a power gamma; below 1 (0 or `srgb` in config.ini) means the piecewise sRGB curve.
pub const SDR_GAMMA_RANGE: (f32, f32) = (1.8, 2.8);

/// The configured `sdr_gamma` (0 = piecewise sRGB), as f32 bits.
static SDR_GAMMA: AtomicU32 = AtomicU32::new(0x400c_cccd); // 2.2
/// The gamma the installed final-pass substitute was built with (f32 bits; u32::MAX = never built).
static LAST_GAMMA: AtomicU32 = AtomicU32::new(u32::MAX);

/// Clamps a configured gamma: NaN = the default, below 1 = 0 (piecewise sRGB), else 1.8..2.8.
pub fn clamp_sdr_gamma(v: f32) -> f32 {
    if v.is_nan() {
        SDR_GAMMA_DEFAULT
    } else if v < 1.0 {
        0.0
    } else {
        v.clamp(SDR_GAMMA_RANGE.0, SDR_GAMMA_RANGE.1)
    }
}

fn gamma_centi(g: f32) -> u16 {
    if g >= 1.0 { (g * 100.0).round() as u16 } else { 0 }
}

/// The configured `sdr_gamma`.
pub fn sdr_gamma() -> f32 {
    f32::from_bits(SDR_GAMMA.load(Relaxed))
}

/// The gamma the final pass uses now: the setting while the mod is on; with the mod switched off the piecewise sRGB curve, exactly
/// how Windows shows an SDR game (the vanilla comparison stays truthful).
pub fn active_sdr_gamma() -> f32 {
    if crate::gate::user_enabled() { sdr_gamma() } else { 0.0 }
}

/// Sets `sdr_gamma` (clamped, returned); the final-pass substitute is rebuilt (lazily) when the gamma in use changes.
pub fn set_sdr_gamma(g: f32) -> f32 {
    let g = clamp_sdr_gamma(g);
    SDR_GAMMA.store(g.to_bits(), Relaxed);
    refresh_gamma();
    g
}

/// Rebuilds the final-pass substitute when the gamma in use differs from the one it was built with.
fn refresh_gamma() {
    if FINAL_INSTALLED.load(Relaxed) && LAST_GAMMA.load(Relaxed) != active_sdr_gamma().to_bits() {
        shaders::install_substitute(FINAL_HASH, &current_final_hlsl(paper_white(), peak()), FINAL_ENTRY);
    }
}

/// `(headroom, knee)` of the tone shoulder in linear light, 1.0 = paper white: the headroom is
/// `peak / paper white`; the knee is 0.8 of it but never below SDR white, so ordinary content and the UI
/// are untouched. Without headroom (peak at or below paper white) the knee is out of reach: no shoulder,
/// only the hard clamp at the peak.
pub fn shoulder_constants(paper_white_nits: f32, peak_nits: f32) -> (f32, f32) {
    if paper_white_nits > 0.0 && peak_nits > paper_white_nits {
        let h = peak_nits / paper_white_nits;
        (h, (0.8 * h).max(1.0))
    } else {
        (1.0, 1.0e9)
    }
}

/// How long after the scRGB swapchain was created `afdc14cb` must have been seen.
pub const FINAL_WAIT_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalStatus {
    /// Not in scRGB: the original final pass is fine.
    NotUsed,
    /// scRGB, `afdc14cb` not seen yet, still within the wait.
    Waiting,
    Seen,
    /// scRGB and `afdc14cb` was not created by the game within `FINAL_WAIT_MS`: the picture is dark/wrong.
    Missing,
    /// scRGB and the substitute could not be created: the picture is dark/wrong.
    Failed,
}

impl FinalStatus {
    pub fn is_problem(self) -> bool {
        matches!(self, FinalStatus::Missing | FinalStatus::Failed)
    }

    pub fn text(self) -> &'static str {
        match self {
            FinalStatus::NotUsed => "not used (SDR)",
            FinalStatus::Waiting => "waiting for ps#afdc14cb",
            FinalStatus::Seen => "ok (ps#afdc14cb seen)",
            FinalStatus::Missing => "MISSING: ps#afdc14cb never seen, the picture will be dark/wrong (set hdr_output=off)",
            FinalStatus::Failed => "FAILED: substitute could not be created, the picture will be dark/wrong (set hdr_output=off)",
        }
    }
}

/// State of the scRGB final-pass conversion. `elapsed_ms` is the time since the scRGB swapchain
/// was created (None when not known).
pub fn final_status(mode: OutputMode, seen: bool, failed: bool, elapsed_ms: Option<u64>) -> FinalStatus {
    if mode != OutputMode::ScRgb {
        FinalStatus::NotUsed
    } else if failed {
        FinalStatus::Failed
    } else if seen {
        FinalStatus::Seen
    } else if elapsed_ms.is_some_and(|e| e >= FINAL_WAIT_MS) {
        FinalStatus::Missing
    } else {
        FinalStatus::Waiting
    }
}

// ---- state -----------------------------------------------------------------------------------

/// 0 = Off, 1 = Auto, 2 = On.
static REQUEST: AtomicU8 = AtomicU8::new(0);
static PAPER_WHITE: AtomicU32 = AtomicU32::new(0x434b_0000); // 203.0
static FINAL_SEEN: AtomicBool = AtomicBool::new(false);
static FINAL_WARNED: AtomicBool = AtomicBool::new(false);
/// `GetTickCount64` when the scRGB swapchain was created (0 = none).
static SC_CREATED_MS: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU32 = AtomicU32::new(0x447a_0000); // 1000.0
static MODE: AtomicU8 = AtomicU8::new(0);
static FLOAT_SC: AtomicUsize = AtomicUsize::new(0);
static HWND: AtomicUsize = AtomicUsize::new(0);
/// 0 = not read yet, 1 = Windows HDR off, 2 = on.
static WINDOWS_HDR: AtomicU8 = AtomicU8::new(0);

/// `MaxLuminance` (nits) the display reports through `IDXGIOutput6::GetDesc1`, as f32 bits; 0 = unknown.
static DISPLAY_MAX: AtomicU32 = AtomicU32::new(0);

/// The peak luminance the display reports, if known and plausible.
pub fn display_max_luminance() -> Option<f32> {
    let v = f32::from_bits(DISPLAY_MAX.load(Relaxed));
    (v > 0.0).then_some(v)
}

/// The peak the picture is mapped to: the user's setting, but never above what the display itself can show. Above
/// its peak the display (or Windows) clips every channel on its own, so a bright saturated light comes out as a flat
/// neon colour; below it, the tone shoulder keeps the colour. A display that reports nothing plausible (< 400 nits)
/// leaves the setting alone.
pub fn effective_peak(user_peak: f32, display_max: Option<f32>) -> f32 {
    match display_max {
        Some(d) if d.is_finite() && d >= 400.0 => user_peak.min(d),
        _ => user_peak,
    }
}

/// Remembers the display's reported peak; when it changes the final-pass substitute is rebuilt with the new cap.
fn note_display_max(ml: Option<f32>) {
    let Some(ml) = ml else { return };
    if DISPLAY_MAX.swap(ml.to_bits(), Relaxed) != ml.to_bits() {
        log!("output: the display reports a peak of {ml:.0} nits; the peak setting is capped to it ({:.0} now)", effective_peak(user_peak(), Some(ml)));
        if FINAL_INSTALLED.load(Relaxed) {
            shaders::install_substitute(FINAL_HASH, &current_final_hlsl(paper_white(), effective_peak(user_peak(), Some(ml))), FINAL_ENTRY);
        }
    }
}
/// `hdr_format` of the config: 0 = scrgb, 1 = hdr10.
static REQ_FORMAT: AtomicU8 = AtomicU8::new(0);
/// The format the swapchain being created is going to get (0 = float16, 1 = R10G10B10A2).
static PLANNED_FMT: AtomicU8 = AtomicU8::new(0);
/// The format of the swapchain the game got (0 = float16, 1 = R10G10B10A2); meaningful in mode `ScRgb`.
static SWAP_FMT: AtomicU8 = AtomicU8::new(0);
static FORMAT_RULE: Mutex<String> = Mutex::new(String::new());
static DATA_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
static MARKER_ACTIVE: AtomicBool = AtomicBool::new(false);
static PRESENTS: AtomicU64 = AtomicU64::new(0);

fn fmt_of(v: u8) -> SwapFormat {
    if v == 1 { SwapFormat::Rgb10A2 } else { SwapFormat::Float16 }
}

fn fmt_val(f: SwapFormat) -> u8 {
    (f == SwapFormat::Rgb10A2) as u8
}

/// The format the swapchain being created will get.
pub fn planned_format() -> SwapFormat {
    fmt_of(PLANNED_FMT.load(Relaxed))
}

/// The swapchain's HDR format (only meaningful while HDR output is active).
pub fn swap_format() -> SwapFormat {
    fmt_of(SWAP_FMT.load(Relaxed))
}

/// HDR output is active AND it is the HDR10 swapchain (the final shader encodes PQ).
pub fn hdr10_active() -> bool {
    mode() == OutputMode::ScRgb && swap_format() == SwapFormat::Rgb10A2
}

/// Where the crash-guard marker lives (the `GW2Graphics` folder). Call once at start.
pub fn set_data_dir(dir: &std::path::Path) {
    *DATA_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
}

fn marker_file() -> Option<std::path::PathBuf> {
    DATA_DIR.lock().unwrap_or_else(|e| e.into_inner()).as_deref().map(hdr10::marker_path)
}

fn marker_present() -> bool {
    marker_file().is_some_and(|p| p.exists())
}

fn write_marker() {
    if let Some(p) = marker_file() {
        let _ = std::fs::write(&p, "An HDR10 swapchain was created and has not yet proven itself (600 presents, 15 s). If this file is still here after a start, HDR10 stays off: delete it to try again.\n");
        MARKER_ACTIVE.store(true, Relaxed);
    }
}

fn clear_marker(why: &str) {
    if MARKER_ACTIVE.swap(false, Relaxed) {
        if let Some(p) = marker_file() {
            let _ = std::fs::remove_file(p);
        }
        log!("output: HDR10 crash guard cleared ({why})");
    }
}

/// Once per present (overlay render loop): counts presents and clears the crash guard when the HDR10 swapchain has proven itself.
pub fn frame_tick() {
    let n = PRESENTS.fetch_add(1, Relaxed) + 1;
    if MARKER_ACTIVE.load(Relaxed) {
        let created = SC_CREATED_MS.load(Relaxed);
        let elapsed = if created == 0 { 0 } else { unsafe { GetTickCount64() }.saturating_sub(created) };
        if hdr10::marker_may_be_cleared(n, elapsed) {
            clear_marker("stable");
        }
    }
}

/// Status line of the swapchain format and the rule that chose it.
pub fn format_status() -> String {
    let rule = FORMAT_RULE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let what = if mode() == OutputMode::ScRgb { format!("{} active", swap_format().name()) } else { "SDR (no HDR swapchain)".to_string() };
    if rule.is_empty() { what } else { format!("{what}; {rule}") }
}

fn request_format() -> HdrFormat {
    if REQ_FORMAT.load(Relaxed) == 1 { HdrFormat::Hdr10 } else { HdrFormat::Scrgb }
}

/// Appends the format sentence to a rule line when HDR10 was requested (so "why not" is always visible), and records the decision.
fn finish_format(mode: OutputMode, reshade: bool, why: String) -> String {
    let requested = request_format();
    let (fmt, rule) = hdr10::decide_format(requested, mode == OutputMode::ScRgb, reshade, marker_present());
    // The HDR10 shader must compile, or the attempt is not made.
    let (fmt, rule) = if fmt == SwapFormat::Rgb10A2 && shaders::compile_ps(&final_hlsl_with(paper_white(), peak(), FinalOpts { hdr10: true, ..FinalOpts::default() }), FINAL_ENTRY).is_err() {
        (SwapFormat::Float16, FormatRule::FailedThisSession)
    } else {
        (fmt, rule)
    };
    PLANNED_FMT.store(fmt_val(fmt), Relaxed);
    let note = if requested == HdrFormat::Hdr10 { rule.as_str().to_string() } else { String::new() };
    *FORMAT_RULE.lock().unwrap_or_else(|e| e.into_inner()) = note.clone();
    if fmt == SwapFormat::Rgb10A2 {
        write_marker();
    }
    if note.is_empty() { why } else { format!("{why}; {note}") }
}

/// A float swapchain attempt in `planned_format()` failed with `problem`: the ONE retry the fallback chain allows (HDR10 -> scRGB),
/// as the desc to create and the new reason. None = no HDR retry (the caller falls back to the game's own swapchain).
pub fn retry_after_failure(d: &SwapChainDesc, problem: &str, why: &str) -> Option<(SwapChainDesc, String)> {
    let next = hdr10::fallback_after_failure(planned_format())?;
    PLANNED_FMT.store(fmt_val(next), Relaxed);
    clear_marker("the attempt failed cleanly");
    let note = FormatRule::FailedThisSession.as_str();
    *FORMAT_RULE.lock().unwrap_or_else(|e| e.into_inner()) = note.to_string();
    log!("output: HDR10 swapchain attempt failed ({problem}); retrying once as scRGB float16");
    Some((desc_for(d, next), format!("{why}; {note} ({problem})")))
}

static LAST_CHECK_MS: AtomicU64 = AtomicU64::new(0);
static FINAL_INSTALLED: AtomicBool = AtomicBool::new(false);
/// The swapchain the game owns (SDR and scRGB), whose `ResizeBuffers` keeps the screen size current.
static GAME_SC: AtomicUsize = AtomicUsize::new(0);
/// The swapchain returned by an original call inside `create_swapchain` and not yet handed to the
/// game (or released). If the hook panics meanwhile, the fallback returns THIS one instead of
/// creating another on a window that already has a flip swapchain.
static PENDING_SC: AtomicUsize = AtomicUsize::new(0);

fn set_pending(sc: usize) {
    PENDING_SC.store(sc, Relaxed);
}

/// Takes the pending swapchain (0 when none).
pub(crate) fn take_pending() -> usize {
    PENDING_SC.swap(0, Relaxed)
}

/// Takes the config (call once at start-up).
pub fn init(cfg: &Config) {
    REQUEST.store(match cfg.hdr_output { HdrOutput::Off => 0, HdrOutput::Auto => 1, HdrOutput::On => 2 }, Relaxed);
    PAPER_WHITE.store(cfg.paper_white_nits.to_bits(), Relaxed);
    PEAK.store(cfg.peak_nits.to_bits(), Relaxed);
    REQ_FORMAT.store((cfg.hdr_format == HdrFormat::Hdr10) as u8, Relaxed);
}

fn request() -> HdrOutput {
    match REQUEST.load(Relaxed) {
        1 => HdrOutput::Auto,
        2 => HdrOutput::On,
        _ => HdrOutput::Off,
    }
}

pub fn mode() -> OutputMode {
    if MODE.load(Relaxed) == 1 { OutputMode::ScRgb } else { OutputMode::Sdr }
}

/// The Windows "SDR content brightness" of the active HDR display in nits (what a plain 8-bit SDR game is shown
/// at), read with the display-config API (loaded by name from user32: nothing is linked or loaded). None when it
/// cannot be told. The structures are the documented `DISPLAYCONFIG_*` layouts (SDK 10.0.26100).
pub fn sdr_white_level_nits() -> Option<f32> {
    type GetSizes = unsafe extern "system" fn(u32, *mut u32, *mut u32) -> i32;
    type Query = unsafe extern "system" fn(u32, *mut u32, *mut u8, *mut u32, *mut u8, *mut c_void) -> i32;
    type GetInfo = unsafe extern "system" fn(*mut u8) -> i32;
    unsafe {
        let m = GetModuleHandleW(wide("user32.dll").as_ptr());
        if m.is_null() {
            return None;
        }
        let get_sizes: GetSizes = std::mem::transmute(GetProcAddress(m, c"GetDisplayConfigBufferSizes".as_ptr() as *const u8)?);
        let query: Query = std::mem::transmute(GetProcAddress(m, c"QueryDisplayConfig".as_ptr() as *const u8)?);
        let get_info: GetInfo = std::mem::transmute(GetProcAddress(m, c"DisplayConfigGetDeviceInfo".as_ptr() as *const u8)?);
        const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
        const PATH_SIZE: usize = 72; // DISPLAYCONFIG_PATH_INFO
        const MODE_SIZE: usize = 64; // DISPLAYCONFIG_MODE_INFO
        let (mut n_paths, mut n_modes) = (0u32, 0u32);
        if get_sizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes) != 0 || n_paths == 0 || n_paths > 64 || n_modes > 256 {
            return None;
        }
        let mut paths = vec![0u8; n_paths as usize * PATH_SIZE];
        let mut modes = vec![0u8; (n_modes as usize).max(1) * MODE_SIZE];
        if query(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, paths.as_mut_ptr(), &mut n_modes, modes.as_mut_ptr(), std::ptr::null_mut()) != 0 {
            return None;
        }
        // Prefer the first target with advanced colour (HDR) enabled.
        let mut fallback = None;
        for i in 0..n_paths as usize {
            let p = &paths[i * PATH_SIZE..(i + 1) * PATH_SIZE];
            // target info starts at 20: adapterId (LUID, 8 bytes) at 20, target id at 28.
            let luid: [u8; 8] = p[20..28].try_into().ok()?;
            let id = u32::from_le_bytes(p[28..32].try_into().ok()?);
            let header = |kind: u32, size: u32| {
                let mut v = vec![0u8; size as usize];
                v[0..4].copy_from_slice(&kind.to_le_bytes());
                v[4..8].copy_from_slice(&size.to_le_bytes());
                v[8..16].copy_from_slice(&luid);
                v[16..20].copy_from_slice(&id.to_le_bytes());
                v
            };
            let mut level = header(11, 24); // DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL
            if get_info(level.as_mut_ptr()) != 0 {
                continue;
            }
            let raw = u32::from_le_bytes(level[20..24].try_into().ok()?);
            let nits = raw as f32 * 80.0 / 1000.0;
            if !(nits.is_finite() && (80.0..=1000.0).contains(&nits)) {
                continue;
            }
            let mut color = header(9, 32); // DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO
            let hdr_on = get_info(color.as_mut_ptr()) == 0 && u32::from_le_bytes(color[20..24].try_into().ok()?) & 2 != 0;
            if hdr_on {
                return Some(nits);
            }
            fallback.get_or_insert(nits);
        }
        fallback
    }
}

/// The levels the final pass runs with: the configured ones while the mod is on; while the user switched it
/// OFF (and the swapchain is already scRGB, which cannot be reverted without a restart) the display is shown like
/// a plain SDR game: paper white = the Windows SDR content brightness, no headroom, so nothing of the mod's look
/// (brightness, shoulder, peak) remains. Called at start and when the overlay toggles Enabled or moves a slider.
pub fn apply_user_levels(enabled: bool, paper_white_nits: f32, peak_nits: f32) {
    if enabled {
        set_levels(paper_white_nits, peak_nits);
    } else {
        let sdr = sdr_white_level_nits().unwrap_or(203.0);
        log!("output: mod off, the final pass shows plain SDR at {sdr:.0} nits (Windows SDR content brightness)");
        set_levels(sdr, sdr);
    }
}

/// Live change of the levels (overlay; the caller clamps). The resolve headroom follows at the next
/// `apply`; a paper white or peak change also recompiles the final-pass substitute (both are baked
/// into it; same lazy re-install as the bloom one).
pub fn set_levels(paper_white_nits: f32, peak_nits: f32) {
    let old_peak = PEAK.swap(peak_nits.to_bits(), Relaxed);
    let old = PAPER_WHITE.swap(paper_white_nits.to_bits(), Relaxed);
    if (old != paper_white_nits.to_bits() || old_peak != peak_nits.to_bits()) && FINAL_INSTALLED.load(Relaxed) {
        shaders::install_substitute(FINAL_HASH, &current_final_hlsl(paper_white_nits, effective_peak(peak_nits, display_max_luminance())), FINAL_ENTRY);
    }
    // The gamma follows Enabled (sRGB while the mod is off): also when the levels happen to stay the same.
    refresh_gamma();
}

/// HDR output is active and the final-pass substitute is the picture's last stage (what a calibration pattern needs).
pub fn calibration_available() -> bool {
    mode() == OutputMode::ScRgb && FINAL_INSTALLED.load(Relaxed)
}

/// Sets the calibration pattern (`calib`): the final-pass substitute is rebuilt (lazily, like a paper white change) when it changed.
/// Returns the pattern in effect (0 when HDR output is not active).
pub fn set_calibration(p: u8) -> u8 {
    set_calibration_from(p, false)
}

/// `set_calibration`; `overlay`: the overlay started it (it then ends when the overlay closes).
pub fn set_calibration_from(p: u8, overlay: bool) -> u8 {
    let before = crate::calib::pattern();
    let now = crate::calib::set(p, calibration_available(), crate::log::uptime_ms());
    crate::calib::set_via_overlay(overlay && now != 0);
    if now != before {
        if FINAL_INSTALLED.load(Relaxed) {
            shaders::install_substitute(FINAL_HASH, &current_final_hlsl(paper_white(), peak()), FINAL_ENTRY);
        }
        log!("output: calibration pattern {} ({})", now, crate::calib::name(now));
    }
    now
}

/// The peak setting as the user chose it (not capped).
pub(crate) fn user_peak() -> f32 {
    f32::from_bits(PEAK.load(Relaxed))
}

/// The peak actually used: the setting capped to the display's own (see `effective_peak`).
pub(crate) fn peak() -> f32 {
    effective_peak(user_peak(), display_max_luminance())
}

/// The game created `ps#afdc14cb` (called by the `CreatePixelShader` hook).
pub fn note_final_seen() {
    FINAL_SEEN.store(true, Relaxed);
}

/// Current state of the scRGB final-pass conversion (for the overlay and the warning).
pub fn final_pass_status() -> FinalStatus {
    let created = SC_CREATED_MS.load(Relaxed);
    let elapsed = (created != 0).then(|| unsafe { GetTickCount64() }.saturating_sub(created));
    final_status(mode(), FINAL_SEEN.load(Relaxed), shaders::substitutes().failed(FINAL_HASH), elapsed)
}

/// Logs one loud WARNING when the scRGB picture cannot be right (see `FinalStatus`).
fn warn_if_final_pass_broken() {
    let st = final_pass_status();
    if st.is_problem() && !FINAL_WARNED.swap(true, Relaxed) {
        log!("WARNING: scRGB output is on but the final pass conversion is not working ({}). The picture will be dark or wrong. Set hdr_output=off in GW2Graphics\\config.ini.", st.text());
    }
}

static REASON: Mutex<String> = Mutex::new(String::new());

fn set_reason(why: &str) {
    *REASON.lock().unwrap_or_else(|e| e.into_inner()) = why.to_string();
}

/// Why the output mode is what it is (decided at swapchain creation); empty before that.
pub fn reason() -> String {
    REASON.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The game's swapchain as last seen (0 = none yet). Used by the dev screenshot at present time.
pub(crate) fn game_swapchain() -> usize {
    GAME_SC.load(Relaxed)
}

pub(crate) fn paper_white() -> f32 {
    f32::from_bits(PAPER_WHITE.load(Relaxed))
}

/// Cheap test for the `PSSetShader` hook: is the final-pass substitute installed and usable?
pub fn final_active() -> bool {
    FINAL_INSTALLED.load(Relaxed) && !shaders::unavailable()
}

/// Applies mode and config: resolve headroom, and install/remove the final-pass substitute.
/// Idempotent and cheap (atomics); called when the mode is set and at every frame boundary.
pub fn apply(gate_on: bool) {
    let p = plan(mode(), gate_on, paper_white(), peak());
    resolve::set_headroom(p.headroom);
    if FINAL_INSTALLED.swap(p.final_substitute, Relaxed) != p.final_substitute {
        if p.final_substitute {
            shaders::install_substitute(FINAL_HASH, &current_final_hlsl(paper_white(), peak()), FINAL_ENTRY);
            log!("output: ps#{FINAL_HASH:08x} substitute installed ({}: decode * {} / 80, clamped to {} / 80)", if hdr10_active() { "HDR10, then BT.2020 PQ" } else { "scRGB" }, paper_white(), peak());
        } else {
            shaders::remove_substitute(FINAL_HASH);
            log!("output: ps#{FINAL_HASH:08x} substitute removed (SDR)");
        }
    }
}

// ---- Windows HDR state -----------------------------------------------------------------------

/// `DXGI_OUTPUT_DESC1`.
#[repr(C)]
struct OutputDesc1 {
    device_name: [u16; 32],
    desktop: [i32; 4],
    attached: i32,
    rotation: u32,
    monitor: usize,
    bits_per_color: u32,
    color_space: u32,
    rest: [f32; 12],
}

/// Desc of an `IDXGIOutput` through `IDXGIOutput6::GetDesc1`; None when the output has no Output6.
unsafe fn output_desc1(output: Obj) -> Option<OutputDesc1> {
    if output.is_null() {
        return None;
    }
    unsafe {
        let qi: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(output, vt::QUERY_INTERFACE));
        let mut o6: Obj = std::ptr::null_mut();
        if qi(output, &IID_OUTPUT6, &mut o6) < 0 || o6.is_null() {
            return None;
        }
        let mut d: OutputDesc1 = std::mem::zeroed();
        let get: unsafe extern "system" fn(Obj, *mut OutputDesc1) -> i32 = std::mem::transmute(vfn(o6, vt::OUTPUT6_GET_DESC1));
        let hr = get(o6, &mut d);
        release(o6);
        (hr >= 0).then_some(d)
    }
}

/// Windows "Use HDR" is on for this output (its colour space is HDR10, `G2084_NONE_P2020`).
///
/// # Safety
/// `output` must be a live `IDXGIOutput*` (or null, which gives false).
#[allow(dead_code)] // `windows_hdr_for_window` is what the hooks use; this is the documented building block
pub unsafe fn windows_hdr_enabled(output: Obj) -> bool {
    unsafe { output_desc1(output) }.is_some_and(|d| d.color_space == CS_G2084_NONE_P2020)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `CreateDXGIFactory` of the already loaded `dxgi.dll` (the game has it; nothing is ever loaded
/// here, so the 1 s re-check cannot hit the loader). Found once, then cached.
fn create_factory_fn() -> Option<usize> {
    static F: AtomicUsize = AtomicUsize::new(0);
    let f = F.load(Relaxed);
    if f != 0 {
        return Some(f);
    }
    let m = unsafe { GetModuleHandleW(wide("dxgi.dll").as_ptr()) };
    if m.is_null() {
        return None;
    }
    let p = unsafe { GetProcAddress(m, c"CreateDXGIFactory".as_ptr() as *const u8) }? as usize;
    F.store(p, Relaxed);
    Some(p)
}

/// ReShade is loaded in this process: one of the modules it hides in exports `ReShadeRegisterAddon`.
/// Its effects/presets are drawn on the swapchain and cannot be seen from here, so its presence alone
/// makes `hdr_output=auto` stay SDR. `GetModuleHandleW` only; nothing is loaded.
pub fn reshade_present() -> bool {
    ["dxgi.dll", "d3d11.dll", "ReShade64.dll"].iter().any(|n| unsafe {
        let m = GetModuleHandleW(wide(n).as_ptr());
        !m.is_null() && GetProcAddress(m, c"ReShadeRegisterAddon".as_ptr() as *const u8).is_some()
    })
}

/// Windows HDR state of the output the window is on (a fresh DXGI factory is used, so no pointer of
/// the game is kept). None when it cannot be told.
pub fn windows_hdr_for_window(hwnd: usize) -> Option<bool> {
    let (hdr, max_luminance) = output_info_for_window(hwnd)?;
    note_display_max(max_luminance);
    Some(hdr)
}

/// (Windows HDR on, the display's reported peak in nits when plausible) of the output the window is on.
fn output_info_for_window(hwnd: usize) -> Option<(bool, Option<f32>)> {
    unsafe {
        let monitor = MonitorFromWindow(hwnd as _, MONITOR_DEFAULTTONEAREST) as usize;
        let create: unsafe extern "system" fn(*const Guid, *mut Obj) -> i32 = std::mem::transmute(create_factory_fn()?);
        let mut factory: Obj = std::ptr::null_mut();
        if create(&IID_FACTORY, &mut factory) < 0 || factory.is_null() {
            return None;
        }
        let mut found = None;
        let enum_adapters: unsafe extern "system" fn(Obj, u32, *mut Obj) -> i32 = std::mem::transmute(vfn(factory, vt::FACTORY_ENUM_ADAPTERS));
        'adapters: for a in 0..16 {
            let mut adapter: Obj = std::ptr::null_mut();
            if enum_adapters(factory, a, &mut adapter) < 0 || adapter.is_null() {
                break;
            }
            let enum_outputs: unsafe extern "system" fn(Obj, u32, *mut Obj) -> i32 = std::mem::transmute(vfn(adapter, vt::ADAPTER_ENUM_OUTPUTS));
            for o in 0..16 {
                let mut output: Obj = std::ptr::null_mut();
                if enum_outputs(adapter, o, &mut output) < 0 || output.is_null() {
                    break;
                }
                let d = output_desc1(output);
                release(output);
                if let Some(d) = d {
                    if d.monitor == monitor {
                        // rest = primaries (8 floats), MinLuminance, MaxLuminance, MaxFullFrameLuminance
                        let ml = d.rest[9];
                        found = Some((d.color_space == CS_G2084_NONE_P2020, (ml.is_finite() && (200.0..=10000.0).contains(&ml)).then_some(ml)));
                        release(adapter);
                        break 'adapters;
                    }
                }
            }
            release(adapter);
        }
        release(factory);
        found
    }
}

/// Re-reads Windows HDR about once per second (overlay render loop) and logs a change. It never
/// changes the mode: the swapchain cannot be recreated at runtime.
pub fn tick() {
    let now = unsafe { GetTickCount64() };
    if now.wrapping_sub(LAST_CHECK_MS.load(Relaxed)) < 1000 {
        return;
    }
    LAST_CHECK_MS.store(now, Relaxed);
    if game_hwnd() == 0 {
        find_game_window();
    }
    let hwnd = if HWND.load(Relaxed) != 0 { HWND.load(Relaxed) } else { game_hwnd() };
    if hwnd == 0 {
        return;
    }
    warn_if_final_pass_broken();
    let Some(cur) = windows_hdr_for_window(hwnd) else { return };
    let prev = WINDOWS_HDR.swap(1 + cur as u8, Relaxed);
    if prev != 0 {
        if let Some(note) = toggle_note(prev == 2, cur, mode()) {
            log!("{note}");
        }
    }
}

// ---- swapchain created together with the device ---------------------------------------------

/// What `D3D11CreateDeviceAndSwapChain` should do with the swapchain it was asked for.
#[derive(Debug, PartialEq)]
pub enum DevicePlan {
    /// Not ours to touch (no swapchain requested, not the game's creation): call the original unchanged.
    PassThrough,
    /// The game's swapchain, kept as the game asked; `why` is the rule.
    Sdr(String),
    /// The game's swapchain as the float16 flip copy `alt`; `why` is the rule.
    Float { alt: SwapChainDesc, why: String },
}

/// Plans the call from the game's desc. `decide` (the impure `evaluate`) runs only for a swapchain
/// that is the game's.
pub fn plan_device_swapchain(d: Option<&SwapChainDesc>, swapchain_requested: bool, is_game_creation: bool, decide: impl FnOnce(&SwapChainDesc) -> (OutputMode, String)) -> DevicePlan {
    let Some(d) = d else { return DevicePlan::PassThrough };
    if !swapchain_requested || !is_game_creation {
        return DevicePlan::PassThrough;
    }
    match decide(d) {
        (OutputMode::Sdr, why) => DevicePlan::Sdr(why),
        (OutputMode::ScRgb, why) => DevicePlan::Float { alt: desc_for(d, planned_format()), why },
    }
}

/// Why a float swapchain attempt has to be undone (the caller then retries once with the game's own
/// desc); None = keep it. `step` (colour space, ResizeBuffers hook, ...) runs only when the call itself
/// returned everything.
pub fn float_problem(hr: i32, swapchain_null: bool, device_null: bool, step: impl FnOnce() -> Result<(), String>) -> Option<String> {
    if hr < 0 || swapchain_null {
        Some(format!("creating the float16 flip swapchain failed (hr={hr:#x})"))
    } else if device_null {
        Some("the call returned no device".to_string())
    } else {
        step().err()
    }
}

// ---- swapchain creation ----------------------------------------------------------------------

pub(crate) type CreateSwapChainFn = unsafe extern "system" fn(Obj, Obj, *mut c_void, *mut Obj) -> i32;

/// Mode and reason for the swapchain the game asks for. `view_hooks_ok`: the RTV/SRV view hooks are
/// in place (for a swapchain created together with the device they are installed right after the
/// call, so the caller passes `true` and verifies afterwards).
pub(crate) fn evaluate(d: &SwapChainDesc, view_hooks_ok: bool) -> (OutputMode, String) {
    let (mode, why) = evaluate_mode(d, view_hooks_ok);
    let why = finish_format(mode, reshade_present(), why);
    (mode, why)
}

fn evaluate_mode(d: &SwapChainDesc, view_hooks_ok: bool) -> (OutputMode, String) {
    if request() == HdrOutput::Off {
        return (OutputMode::Sdr, Rule::RequestOff.as_str().into());
    }
    if shaders::unavailable() {
        return (OutputMode::Sdr, "shader substitution is unavailable (the final pass cannot be converted)".into());
    }
    let changed = other_tool_changed(d.format, d.effect);
    if changed {
        return (OutputMode::Sdr, format!("{} (format {}, swap effect {})", Rule::SwapchainChanged.as_str(), d.format, d.effect));
    }
    if !flip_possible(d) {
        return (OutputMode::Sdr, format!("the swapchain cannot be a flip-model one (samples {}, windowed {})", d.samples[0], d.windowed));
    }
    let Some(hdr) = windows_hdr_for_window(d.window) else {
        return (OutputMode::Sdr, "could not read the Windows HDR state of the output".into());
    };
    WINDOWS_HDR.store(1 + hdr as u8, Relaxed);
    if let Err(e) = shaders::compile_ps(&final_hlsl(paper_white(), peak()), FINAL_ENTRY) {
        return (OutputMode::Sdr, format!("the final-pass shader does not compile: {e}"));
    }
    let reshade = reshade_present();
    let (mode, rule) = explain(view_hooks_ok, request(), hdr, d.format, changed, reshade);
    (mode, format!("rule: {} [request {:?}, windows_hdr {hdr}, reshade {reshade}]", rule.as_str(), request()))
}

/// Verifies the colour space of `fmt` (scRGB or HDR10) on the (new) swapchain and selects it. Err = why not.
unsafe fn configure_colour_space(sc: Obj, fmt: SwapFormat) -> Result<(), String> {
    let cs = fmt.colour_space();
    unsafe {
        let qi: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(sc, vt::QUERY_INTERFACE));
        let mut sc3: Obj = std::ptr::null_mut();
        if qi(sc, &IID_SWAPCHAIN3, &mut sc3) < 0 || sc3.is_null() {
            return Err("no IDXGISwapChain3".into());
        }
        let check: unsafe extern "system" fn(Obj, u32, *mut u32) -> i32 = std::mem::transmute(vfn(sc3, vt::SWAPCHAIN3_CHECK_COLOR_SPACE_SUPPORT));
        let mut support = 0u32;
        let hr = check(sc3, cs, &mut support);
        let r = if hr < 0 {
            Err(format!("CheckColorSpaceSupport failed (hr={hr:#x})"))
        } else if support & CS_SUPPORT_PRESENT == 0 {
            Err(format!("colour space {cs} ({}) is not supported for Present (support flags {support:#x})", fmt.name()))
        } else {
            let set: unsafe extern "system" fn(Obj, u32) -> i32 = std::mem::transmute(vfn(sc3, vt::SWAPCHAIN3_SET_COLOR_SPACE1));
            let hr = set(sc3, cs);
            if hr < 0 { Err(format!("SetColorSpace1 failed (hr={hr:#x})")) } else { Ok(()) }
        };
        release(sc3);
        r
    }
}

pub(crate) fn record_sdr(hwnd: usize, why: &str) {
    MODE.store(0, Relaxed);
    FLOAT_SC.store(0, Relaxed);
    SWAP_FMT.store(0, Relaxed);
    PLANNED_FMT.store(0, Relaxed);
    clear_marker("fell back to SDR");
    set_reason(why);
    if hwnd != 0 {
        HWND.store(hwnd, Relaxed);
    }
    apply(shaders::substitution_enabled());
    log!("output: SDR ({why})");
}

/// Everything that can fail on a freshly created float16 swapchain, done before the game sees it:
/// the colour space, then the ResizeBuffers hook (a float swapchain whose ResizeBuffers is not rewritten
/// would break on the game's first resize).
///
/// # Safety
/// `sc` must be a live `IDXGISwapChain*`.
pub(crate) unsafe fn finish_float(sc: Obj) -> Result<(), String> {
    unsafe { configure_colour_space(sc, planned_format()) }?;
    if unsafe { crate::d3d::hook_resize_buffers(sc) } {
        Ok(())
    } else {
        Err("the ResizeBuffers hook could not be installed".to_string())
    }
}

/// The float swapchain `sc` is what the game gets: mode, tracking and the final-pass substitute follow.
pub(crate) fn commit_scrgb(sc: Obj, hwnd: usize, why: &str) {
    MODE.store(1, Relaxed);
    SWAP_FMT.store(PLANNED_FMT.load(Relaxed), Relaxed);
    set_reason(why);
    FLOAT_SC.store(sc as usize, Relaxed);
    SC_CREATED_MS.store(unsafe { GetTickCount64() }.max(1), Relaxed);
    GAME_SC.store(sc as usize, Relaxed);
    HWND.store(hwnd, Relaxed);
    apply(shaders::substitution_enabled());
    if swap_format() == SwapFormat::Rgb10A2 {
        log!("output: HDR10 [experimental] ({why}): swapchain {sc:p} created as R10G10B10A2_UNORM flip-discard, 2 buffers, colour space G2084_NONE_P2020");
    } else {
        log!("output: scRGB ({why}): swapchain {sc:p} created as R16G16B16A16_FLOAT flip-discard, 2 buffers, colour space G10_NONE_P709");
    }
}

/// The game's window, cached at the first match.
static GAME_HWND: AtomicUsize = AtomicUsize::new(0);

pub fn game_hwnd() -> usize {
    GAME_HWND.load(Relaxed)
}

/// Current client size of the game window, read live (the screen size the hooks know lags behind a
/// resize: the game creates its new render targets BEFORE the next frame tick updates it).
pub fn game_client_size() -> Option<(u32, u32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;
    let hwnd = game_hwnd();
    if hwnd == 0 {
        return None;
    }
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: plain query on a window handle; a stale handle just fails.
    if unsafe { GetClientRect(hwnd as _, &mut r) } == 0 {
        return None;
    }
    let s = ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32);
    (s.0 > 0 && s.1 > 0).then_some(s)
}

fn note_game_window(hwnd: usize) {
    if hwnd != 0 && GAME_HWND.swap(hwnd, Relaxed) != hwnd {
        log!("output: game window {hwnd:#x}");
    }
}

/// What the rule needs to know about `hwnd`; None when it is not a window.
fn window_info(hwnd: usize) -> Option<WindowInfo> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetClientRect, IsWindow, IsWindowVisible};
    if hwnd == 0 {
        return None;
    }
    unsafe {
        if IsWindow(hwnd as _) == 0 {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd as _, &mut pid);
        let mut buf = [0u16; 128];
        let n = GetClassNameW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let client = if GetClientRect(hwnd as _, &mut r) != 0 { ((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32) } else { (0, 0) };
        Some(WindowInfo { hwnd, owned: pid == GetCurrentProcessId(), class: String::from_utf16_lossy(&buf[..n.min(buf.len())]), visible: IsWindowVisible(hwnd as _) != 0, client })
    }
}

/// Does the swapchain desc belong to the game's window? Caches the window at the first match.
pub(crate) fn desc_is_game(d: &SwapChainDesc) -> bool {
    if d.window == 0 || ignoring() {
        return false;
    }
    let info = window_info(d.window);
    let ok = swapchain_window_is_game(game_hwnd(), d.window, (d.width, d.height), info.as_ref());
    if ok {
        note_game_window(d.window);
    }
    ok
}

/// Looks for the game's window among the process's top-level windows (until one is found).
fn find_game_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;
    unsafe extern "system" fn each(hwnd: windows_sys::Win32::Foundation::HWND, lparam: isize) -> i32 {
        let list = unsafe { &mut *(lparam as *mut Vec<WindowInfo>) };
        if let Some(i) = window_info(hwnd as usize) {
            if i.owned {
                list.push(i);
            }
        }
        1
    }
    let mut list: Vec<WindowInfo> = Vec::new();
    unsafe { EnumWindows(Some(each), &mut list as *mut Vec<WindowInfo> as isize) };
    if let Some(h) = pick_game_window(&list) {
        note_game_window(h);
    }
}

/// The factory `CreateSwapChain` hook body. Creates the game's swapchain as float16 scRGB when
/// `decide` says so; every failure falls back to one retry with the game's own desc (see the module
/// docs).
///
/// # Safety
/// Arguments as for `IDXGIFactory::CreateSwapChain`; `orig` is the original method.
pub(crate) unsafe fn create_swapchain(factory: Obj, device: Obj, is_game: bool, desc: *mut c_void, out: *mut Obj, orig: CreateSwapChainFn) -> i32 {
    set_pending(0);
    if desc.is_null() || out.is_null() || ignoring() {
        return unsafe { orig(factory, device, desc, out) };
    }
    let d: SwapChainDesc = unsafe { *(desc as *const SwapChainDesc) };
    // Same decision as for a swapchain created with the device (`plan_device_swapchain`); foreign
    // swapchains (overlay/capture tools, second windows) are untouched and change no state.
    let view_hooks_ok = crate::d3d::view_hooks_ok();
    let plan = std::panic::catch_unwind(|| plan_device_swapchain(Some(&d), true, is_game, |d| evaluate(d, view_hooks_ok))).unwrap_or_else(|_| DevicePlan::Sdr("internal error while deciding".into()));
    let why = match plan {
        DevicePlan::PassThrough => return unsafe { orig(factory, device, desc, out) },
        DevicePlan::Sdr(why) => {
            record_sdr(d.window, &why);
            return unsafe { orig(factory, device, desc, out) };
        }
        DevicePlan::Float { why, .. } => why,
    };
    let (mut alt, mut why) = (desc_for(&d, planned_format()), why);
    loop {
        let hr = unsafe { orig(factory, device, &alt as *const SwapChainDesc as *mut c_void, out) };
        let sc = unsafe { *out };
        let problem = float_problem(hr, sc.is_null(), false, || {
            set_pending(sc as usize);
            let r = unsafe { finish_float(sc) };
            if r.is_err() {
                set_pending(0);
                unsafe {
                    release(sc);
                    *out = std::ptr::null_mut();
                }
            }
            r
        });
        match problem {
            None => {
                commit_scrgb(sc, d.window, &why);
                set_pending(0);
                return hr;
            }
            Some(p) => {
                // HDR10 gets exactly one scRGB retry before the SDR fallback.
                if let Some((next, w)) = retry_after_failure(&d, &p, &why) {
                    (alt, why) = (next, w);
                    continue;
                }
                record_sdr(d.window, &format!("{p}; retrying with the game's own swapchain"));
                return unsafe { orig(factory, device, desc, out) };
            }
        }
    }
}

/// The game's swapchain was created as it asked (SDR mode, or after an scRGB fallback): follow its
/// `ResizeBuffers` too, for the screen size only. Nothing but `GetDesc` is called; a failure to hook
/// only costs the size following (the overlay's per-frame size is the fallback). Only the game window's
/// swapchain is hooked.
///
/// # Safety
/// `sc` must be a live `IDXGISwapChain*`.
/// Returns whether `sc` is the game's swapchain (the caller then records its size).
pub(crate) unsafe fn hook_game_swapchain(sc: Obj, desc_hwnd: usize) -> bool {
    if sc.is_null() || ignoring() || desc_hwnd == 0 || desc_hwnd != game_hwnd() {
        return false;
    }
    GAME_SC.store(sc as usize, Relaxed);
    if !unsafe { crate::d3d::hook_resize_buffers(sc) } {
        log!("output: ResizeBuffers not hooked; the screen size follows the overlay's per-frame size only");
    }
    true
}

/// What `ResizeBuffers` has to know about the swapchain it was called on.
pub(crate) struct ResizeCall {
    pub is_float: bool,
    pub is_game: bool,
}

/// Before the original `ResizeBuffers`: which swapchain this is and the arguments to pass. The game's
/// swapchain is known by the pointer of its creation or by its window.
pub(crate) fn resize_before(this: Obj, count: u32, fmt: u32, flags: u32) -> (ResizeCall, (u32, u32, u32)) {
    let live = this as usize != 0 && !ignoring();
    let is_float = live && FLOAT_SC.load(Relaxed) == this as usize;
    let is_game = live && (GAME_SC.load(Relaxed) == this as usize || unsafe { crate::d3d::swapchain_desc(this) }.is_some_and(|d| d.window != 0 && d.window == game_hwnd()));
    (ResizeCall { is_float, is_game }, hdr10::resize_args_for(is_float, swap_format(), count, fmt, flags))
}

/// After the original `ResizeBuffers` returned `hr` for the requested size `w` x `h`.
pub(crate) fn resize_after(this: Obj, rc: &ResizeCall, hr: i32, w: u32, h: u32) {
    if rc.is_game && hr >= 0 {
        // The game recreates its targets right after this call, before Present: the size must be
        // current by then (`upgrade::target_format` compares against it).
        let _ = std::panic::catch_unwind(|| {
            if let Some((sw, sh)) = screen_after_resize(unsafe { crate::d3d::swapchain_size(this) }, (w, h)) {
                crate::d3d::hooks().set_screen(sw, sh);
            }
        });
    }
    if rc.is_float && hr >= 0 {
        // The colour space is set again to be safe; a failure only costs a log line (the default
        // colour space of a float16 swapchain is already scRGB).
        let _ = std::panic::catch_unwind(|| {
            if let Err(e) = unsafe { configure_colour_space(this, swap_format()) } {
                log!("output: colour space after ResizeBuffers: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HdrOutput;

    const RGBA8: u32 = 28;
    const FLOAT16: u32 = 10;

    #[test]
    fn sdr_when_windows_hdr_off() {
        assert_eq!(decide(HdrOutput::Auto, false, RGBA8, false, false), OutputMode::Sdr);
    }

    #[test]
    fn scrgb_when_all_conditions_hold() {
        assert_eq!(decide(HdrOutput::Auto, true, RGBA8, false, false), OutputMode::ScRgb);
    }

    #[test]
    fn sdr_when_request_off() {
        assert_eq!(decide(HdrOutput::Off, true, RGBA8, false, false), OutputMode::Sdr);
    }

    #[test]
    fn sdr_when_another_tool_changed_swapchain() {
        assert_eq!(decide(HdrOutput::Auto, true, RGBA8, true, false), OutputMode::Sdr);
    }

    #[test]
    fn sdr_when_swapchain_already_float() {
        assert_eq!(decide(HdrOutput::Auto, true, FLOAT16, false, false), OutputMode::Sdr);
    }

    #[test]
    fn redecides_when_windows_hdr_toggles() {
        assert_eq!(decide(HdrOutput::Auto, false, RGBA8, false, false), OutputMode::Sdr);
        assert_eq!(decide(HdrOutput::Auto, true, RGBA8, false, false), OutputMode::ScRgb);
        assert_eq!(decide(HdrOutput::Auto, false, RGBA8, false, false), OutputMode::Sdr);
    }

    #[test]
    fn final_substitute_wanted_whenever_scrgb() {
        assert!(final_substitute_wanted(OutputMode::ScRgb, true));
        assert!(final_substitute_wanted(OutputMode::ScRgb, false));
        assert!(!final_substitute_wanted(OutputMode::Sdr, true));
        assert!(!final_substitute_wanted(OutputMode::Sdr, false));
    }

    #[test]
    fn only_the_final_substitute_survives_a_closed_gate() {
        // Gate on: everything installed may run.
        assert!(substitute_allowed(0x1234, true, OutputMode::Sdr));
        assert!(substitute_allowed(0x1234, true, OutputMode::ScRgb));
        // Gate off, Sdr: nothing.
        assert!(!substitute_allowed(FINAL_HASH, false, OutputMode::Sdr));
        // Gate off, ScRgb: only afdc14cb (the original would write gamma values into a linear swapchain).
        assert!(substitute_allowed(FINAL_HASH, false, OutputMode::ScRgb));
        assert!(!substitute_allowed(0xf5cd4832, false, OutputMode::ScRgb));
    }

    #[test]
    fn plan_headroom_and_final_substitute() {
        let p = plan(OutputMode::ScRgb, false, 200.0, 1000.0);
        assert_eq!((p.headroom, p.final_substitute), (5.0, true));
        let p = plan(OutputMode::Sdr, true, 200.0, 1000.0);
        assert_eq!((p.headroom, p.final_substitute), (1.0, false));
        // Degenerate config never gives a headroom below 1.
        assert_eq!(plan(OutputMode::ScRgb, true, 400.0, 300.0).headroom, 1.0);
    }

    #[test]
    fn other_tool_detection() {
        assert!(!other_tool_changed(28, 0)); // the game's own: RGBA8, DISCARD
        assert!(!other_tool_changed(28, 1)); // SEQUENTIAL
        assert!(other_tool_changed(87, 0)); // not the format the game asks for
        assert!(other_tool_changed(10, 0));
        assert!(!other_tool_changed(28, 3)); // FLIP_SEQUENTIAL: not a signal
        assert!(!other_tool_changed(28, 4)); // FLIP_DISCARD: the game's own request
    }

    fn game_desc() -> crate::d3d::SwapChainDesc {
        crate::d3d::SwapChainDesc { width: 3440, height: 1440, refresh: [60, 1], format: 28, scanline: 0, scaling: 0, samples: [1, 0], usage: 0x20, buffers: 1, window: 0x1234, windowed: 1, effect: 0, flags: 2 | 4 | 0x100 }
    }

    #[test]
    fn scrgb_desc_rewrites_only_what_flip_float_needs() {
        let d = game_desc();
        let n = scrgb_desc(&d);
        assert_eq!((n.format, n.effect, n.buffers), (10, 4, 2));
        assert_eq!(n.flags, 2); // ALLOW_MODE_SWITCH kept, GDI_COMPATIBLE and FULLSCREEN_VIDEO dropped
        assert_eq!((n.width, n.height, n.window, n.windowed, n.samples, n.usage), (3440, 1440, 0x1234, 1, [1, 0], 0x20));
    }

    #[test]
    fn msaa_swapchain_cannot_be_flip() {
        assert!(!flip_possible(&crate::d3d::SwapChainDesc { samples: [4, 0], ..game_desc() }));
        assert!(!flip_possible(&crate::d3d::SwapChainDesc { windowed: 0, ..game_desc() }));
        assert!(flip_possible(&game_desc()));
    }

    #[test]
    fn resize_keeps_the_float_format() {
        // (is_float, count, fmt, flags) -> (count, fmt, flags)
        assert_eq!(resize_args(true, 1, 28, 2 | 4), (2, 10, 2));
        assert_eq!(resize_args(true, 0, 0, 2), (0, 0, 2)); // keep count and format
        assert_eq!(resize_args(true, 2, 10, 2), (2, 10, 2));
        assert_eq!(resize_args(false, 1, 28, 2 | 4), (1, 28, 2 | 4)); // not ours: untouched
    }

    #[test]
    fn final_status_cases() {
        use OutputMode::{ScRgb, Sdr};
        assert_eq!(final_status(Sdr, false, false, Some(999_999)), FinalStatus::NotUsed);
        assert_eq!(final_status(Sdr, false, true, None), FinalStatus::NotUsed);
        assert_eq!(final_status(ScRgb, false, false, Some(1_000)), FinalStatus::Waiting);
        assert_eq!(final_status(ScRgb, false, false, None), FinalStatus::Waiting);
        assert_eq!(final_status(ScRgb, true, false, Some(1_000)), FinalStatus::Seen);
        assert_eq!(final_status(ScRgb, false, false, Some(FINAL_WAIT_MS - 1)), FinalStatus::Waiting);
        assert_eq!(final_status(ScRgb, false, false, Some(FINAL_WAIT_MS)), FinalStatus::Missing);
        assert_eq!(final_status(ScRgb, true, true, Some(10)), FinalStatus::Failed);
        assert_eq!(final_status(ScRgb, false, true, None), FinalStatus::Failed);
        for st in [FinalStatus::Missing, FinalStatus::Failed] {
            assert!(st.is_problem() && st.text().contains("dark/wrong"));
        }
        assert!(![FinalStatus::NotUsed, FinalStatus::Waiting, FinalStatus::Seen].iter().any(|s| s.is_problem()));
    }

    #[test]
    fn screen_after_resize_prefers_the_desc_and_ignores_zero_sizes() {
        assert_eq!(screen_after_resize(Some((1920, 1080)), (0, 0)), Some((1920, 1080))); // 0 = keep: the desc has the size
        assert_eq!(screen_after_resize(Some((1920, 1080)), (1280, 720)), Some((1920, 1080)));
        assert_eq!(screen_after_resize(None, (1280, 720)), Some((1280, 720)));
        assert_eq!(screen_after_resize(Some((0, 0)), (1280, 720)), Some((1280, 720)));
        assert_eq!(screen_after_resize(Some((1920, 0)), (1280, 720)), Some((1280, 720)));
        assert_eq!(screen_after_resize(None, (0, 0)), None);
        assert_eq!(screen_after_resize(Some((0, 0)), (1280, 0)), None);
    }

    #[test]
    fn final_hlsl_bakes_the_paper_white_and_peak_scales() {
        let s = final_hlsl(203.0, 1000.0);
        assert!(s.starts_with("#define PAPER_WHITE_SCALE 2.5375
#define PEAK_SCALE 12.5
"), "{}", &s[..60]);
    }

    #[test]
    fn the_peak_is_capped_to_the_display() {
        assert_eq!(effective_peak(2800.0, Some(1000.0)), 1000.0);
        assert_eq!(effective_peak(800.0, Some(1000.0)), 800.0);
        assert_eq!(effective_peak(2800.0, None), 2800.0);
        // An implausible report (under 400 nits) is ignored rather than trusted.
        assert_eq!(effective_peak(2800.0, Some(100.0)), 2800.0);
        assert_eq!(effective_peak(2800.0, Some(f32::NAN)), 2800.0);
    }

    #[test]
    fn sdr_white_level_query_is_sane() {
        // Environment dependent (headless CI may have no display): only that it returns and, when it does, is plausible.
        if let Some(n) = sdr_white_level_nits() {
            assert!((80.0..=1000.0).contains(&n), "{n}");
        }
    }

    #[test]
    fn windows_hdr_query_runs_without_crashing() {
        // Environment dependent (headless CI may have no output): only that it returns and does not crash.
        println!("windows_hdr_for_window(desktop) = {:?}", windows_hdr_for_window(0));
        assert!(!unsafe { windows_hdr_enabled(std::ptr::null_mut()) });
    }

    #[test]
    fn sdr_when_reshade_present_in_auto() {
        assert_eq!(decide(HdrOutput::Auto, true, RGBA8, false, true), OutputMode::Sdr);
        assert_eq!(rule_of(HdrOutput::Auto, true, RGBA8, false, true), Rule::OtherToolPresent);
    }

    #[test]
    fn scrgb_when_reshade_present_but_forced_on() {
        assert_eq!(decide(HdrOutput::On, true, RGBA8, false, true), OutputMode::ScRgb);
        assert_eq!(rule_of(HdrOutput::On, true, RGBA8, false, true), Rule::ForcedOn);
        assert_eq!(decide(HdrOutput::On, true, RGBA8, false, false), OutputMode::ScRgb);
    }

    #[test]
    fn sdr_when_forced_on_but_windows_hdr_off() {
        assert_eq!(decide(HdrOutput::On, false, RGBA8, false, true), OutputMode::Sdr);
        assert_eq!(rule_of(HdrOutput::On, false, RGBA8, false, false), Rule::WindowsHdrOff);
    }

    #[test]
    fn sdr_when_forced_on_but_swapchain_changed() {
        assert_eq!(decide(HdrOutput::On, true, RGBA8, true, false), OutputMode::Sdr);
        assert_eq!(decide(HdrOutput::On, true, FLOAT16, false, false), OutputMode::Sdr);
        assert_eq!(rule_of(HdrOutput::On, true, RGBA8, true, false), Rule::SwapchainChanged);
    }

    #[test]
    fn sdr_when_off_even_if_everything_else_is_fine() {
        assert_eq!(decide(HdrOutput::Off, true, RGBA8, false, false), OutputMode::Sdr);
        assert_eq!(rule_of(HdrOutput::Off, true, RGBA8, false, false), Rule::RequestOff);
    }

    #[test]
    fn rule_and_mode_agree_for_every_input() {
        for req in [HdrOutput::Auto, HdrOutput::Off, HdrOutput::On] {
            for hdr in [false, true] {
                for fmt in [RGBA8, FLOAT16, 87] {
                    for changed in [false, true] {
                        for other in [false, true] {
                            let scrgb = decide(req, hdr, fmt, changed, other) == OutputMode::ScRgb;
                            let r = rule_of(req, hdr, fmt, changed, other);
                            assert_eq!(scrgb, matches!(r, Rule::Auto | Rule::ForcedOn), "{req:?} {hdr} {fmt} {changed} {other}: {r:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sdr_when_view_hooks_missing() {
        // Without the RTV/SRV hooks the back-buffer view (R8G8B8A8_UNORM on a float16 swapchain) fails.
        assert_eq!(explain(false, HdrOutput::On, true, RGBA8, false, false), (OutputMode::Sdr, Rule::ViewHooksMissing));
        assert_eq!(explain(true, HdrOutput::On, true, RGBA8, false, false), (OutputMode::ScRgb, Rule::ForcedOn));
        assert_eq!(explain(true, HdrOutput::Auto, true, RGBA8, false, false), (OutputMode::ScRgb, Rule::Auto));
    }

    #[test]
    fn reshade_check_runs_without_loading_anything() {
        // Environment dependent; only that it returns (it uses GetModuleHandleW, never LoadLibrary).
        println!("reshade_present() = {}", reshade_present());
    }

    // ---- swapchain created together with the device ----

    fn fixed(mode: OutputMode) -> impl FnOnce(&SwapChainDesc) -> (OutputMode, String) {
        move |_| (mode, "rule".to_string())
    }

    #[test]
    fn device_plan_float_copy_of_the_games_desc() {
        let d = game_desc();
        match plan_device_swapchain(Some(&d), true, true, fixed(OutputMode::ScRgb)) {
            DevicePlan::Float { alt, why } => {
                assert_eq!(alt, scrgb_desc(&d));
                assert_eq!((alt.format, alt.effect, alt.buffers), (10, 4, 2));
                assert_eq!(why, "rule");
            }
            p => panic!("{p:?}"),
        }
        // The game's own desc (the real log: RGBA8, 1 buffer, discard, windowed) is not modified.
        assert_eq!((d.format, d.buffers, d.effect, d.windowed), (28, 1, 0, 1));
    }

    #[test]
    fn device_plan_sdr_keeps_the_game_desc_and_reports_the_rule() {
        assert_eq!(plan_device_swapchain(Some(&game_desc()), true, true, fixed(OutputMode::Sdr)), DevicePlan::Sdr("rule".into()));
    }

    #[test]
    fn device_plan_passes_through_what_is_not_the_games_swapchain() {
        let d = game_desc();
        let never = |_: &SwapChainDesc| -> (OutputMode, String) { panic!("must not evaluate") };
        assert_eq!(plan_device_swapchain(None, true, true, never), DevicePlan::PassThrough); // no desc
        assert_eq!(plan_device_swapchain(Some(&d), false, true, never), DevicePlan::PassThrough); // no swapchain requested
        assert_eq!(plan_device_swapchain(Some(&d), true, false, never), DevicePlan::PassThrough); // another tool's dummy
    }

    #[test]
    fn every_failure_of_the_float_attempt_means_retry_with_the_games_desc() {
        let ok = || -> Result<(), String> { Ok(()) };
        assert_eq!(float_problem(0, false, false, ok), None);
        assert_eq!(float_problem(1, false, false, ok), None); // S_FALSE style success codes
        let p = float_problem(-2005270523, false, false, ok).unwrap();
        assert!(p.contains("creating the float16 flip swapchain failed") && p.contains("hr="), "{p}");
        assert!(float_problem(0, true, false, ok).unwrap().contains("float16 flip swapchain failed")); // success but nothing returned
        assert!(float_problem(0, false, true, ok).unwrap().contains("no device"));
        assert_eq!(float_problem(0, false, false, || Err("SetColorSpace1 failed (hr=0x887a0001)".to_string())).unwrap(), "SetColorSpace1 failed (hr=0x887a0001)");
        assert_eq!(float_problem(0, false, false, || Err("the ResizeBuffers hook could not be installed".to_string())).unwrap(), "the ResizeBuffers hook could not be installed");
    }

    #[test]
    fn a_failed_call_does_not_run_the_later_steps() {
        let step = || -> Result<(), String> { panic!("must not run") };
        assert!(float_problem(-1, false, false, step).is_some());
        assert!(float_problem(0, true, false, step).is_some());
        assert!(float_problem(0, false, true, step).is_some());
    }

    fn win(hwnd: usize, class: &str, visible: bool, client: (u32, u32)) -> WindowInfo {
        WindowInfo { hwnd, owned: true, class: class.to_string(), visible, client }
    }

    const GAME_CLASS: &str = "ArenaNet_Dx_Window_Class";

    #[test]
    fn game_window_is_the_visible_arenanet_window() {
        let list = vec![
            win(0x10, "ReShade", false, (1, 1)),
            win(0x20, "Hudhook Dummy", true, (3440, 1440)), // visible but not the game's class
            win(0x30, GAME_CLASS, true, (3440, 1440)),
        ];
        assert_eq!(pick_game_window(&list), Some(0x30));
    }

    #[test]
    fn game_window_needs_visibility_class_and_ownership() {
        assert_eq!(pick_game_window(&[win(0x30, GAME_CLASS, false, (3440, 1440))]), None); // hidden
        assert_eq!(pick_game_window(&[win(0x30, "Other", true, (3440, 1440))]), None);
        assert_eq!(pick_game_window(&[WindowInfo { owned: false, ..win(0x30, GAME_CLASS, true, (3440, 1440)) }]), None); // another process
        assert_eq!(pick_game_window(&[]), None);
    }

    #[test]
    fn biggest_arenanet_window_wins_then_the_first() {
        let list = vec![win(0x10, GAME_CLASS, true, (200, 100)), win(0x20, GAME_CLASS, true, (3440, 1440)), win(0x30, GAME_CLASS, true, (3440, 1440))];
        assert_eq!(pick_game_window(&list), Some(0x20));
    }

    #[test]
    fn swapchain_window_by_class_by_size_and_by_cache() {
        let size = (3440, 1440);
        // Game class: matches before it was ever shown.
        assert!(swapchain_window_is_game(0, 0x30, size, Some(&win(0x30, GAME_CLASS, false, (0, 0)))));
        // Other class: only when visible and the client rect equals the swapchain size.
        assert!(swapchain_window_is_game(0, 0x40, size, Some(&win(0x40, "Custom", true, size))));
        assert!(!swapchain_window_is_game(0, 0x40, size, Some(&win(0x40, "Custom", true, (800, 600)))));
        assert!(!swapchain_window_is_game(0, 0x40, size, Some(&win(0x40, "Custom", false, size)))); // hidden dummy
        // Another process, no window, no handle.
        assert!(!swapchain_window_is_game(0, 0x30, size, Some(&WindowInfo { owned: false, ..win(0x30, GAME_CLASS, true, size) })));
        assert!(!swapchain_window_is_game(0, 0x30, size, None));
        assert!(!swapchain_window_is_game(0x30, 0, size, None));
        // A zero size never matches by size.
        assert!(!swapchain_window_is_game(0, 0x40, (0, 0), Some(&win(0x40, "Custom", true, (0, 0)))));
    }

    #[test]
    fn once_the_game_window_is_cached_a_second_window_is_foreign() {
        let size = (3440, 1440);
        assert!(swapchain_window_is_game(0x30, 0x30, size, None)); // the handle alone
        // A second visible window of the same size is not the game's any more.
        assert!(!swapchain_window_is_game(0x30, 0x40, size, Some(&win(0x40, "Custom", true, size))));
        // A re-created game window (new handle, game class) is.
        assert!(swapchain_window_is_game(0x30, 0x50, size, Some(&win(0x50, GAME_CLASS, true, size))));
    }

    #[test]
    fn both_creation_paths_decide_the_same_way_for_the_games_window() {
        // Path 1: D3D11CreateDeviceAndSwapChain; path 2: factory CreateSwapChain (any device identity).
        let d = game_desc();
        let is_game = swapchain_window_is_game(0, d.window, (d.width, d.height), Some(&win(d.window, GAME_CLASS, true, (d.width, d.height))));
        assert!(is_game);
        for _path in 0..2 {
            match plan_device_swapchain(Some(&d), true, is_game, fixed(OutputMode::ScRgb)) {
                DevicePlan::Float { alt, .. } => assert_eq!(alt, scrgb_desc(&d)),
                p => panic!("{p:?}"),
            }
            assert_eq!(plan_device_swapchain(Some(&d), true, is_game, fixed(OutputMode::Sdr)), DevicePlan::Sdr("rule".into()));
        }
        // A foreign window is passed through on both paths.
        let never = |_: &SwapChainDesc| -> (OutputMode, String) { panic!("must not evaluate") };
        let foreign = swapchain_window_is_game(0x30, 0x99, (64, 64), Some(&win(0x99, "hudhook", false, (64, 64))));
        assert_eq!(plan_device_swapchain(Some(&d), true, foreign, never), DevicePlan::PassThrough);
    }

    #[test]
    fn creation_size_comes_from_the_swapchain_then_the_desc() {
        let d = game_desc();
        assert_eq!(screen_after_resize(Some((3424, 1369)), (d.width, d.height)), Some((3424, 1369)));
        assert_eq!(screen_after_resize(None, (d.width, d.height)), Some((3440, 1440)));
    }

    #[test]
    fn pending_swapchain_is_handed_back_once() {
        set_pending(0x1234);
        assert_eq!(take_pending(), 0x1234);
        assert_eq!(take_pending(), 0);
    }

    #[test]
    fn toggle_note_only_on_change() {
        assert_eq!(toggle_note(true, true, OutputMode::Sdr), None);
        assert!(toggle_note(false, true, OutputMode::Sdr).unwrap().contains("next swapchain"));
        assert!(toggle_note(true, false, OutputMode::ScRgb).unwrap().contains("next swapchain"));
    }
}
