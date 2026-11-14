//! Direct3D 11 interception (observation only).
//!
//! GW2 creates its device through `D3D11CreateDevice` in `d3d11.dll` (bgfx). We detour that export
//! (and `D3D11CreateDeviceAndSwapChain`), call the original, and, from the objects it returns, resolve
//! the address of every native FUNCTION BODY we need (the context's draw / render-target methods, the
//! device's creation methods, the DXGI factory's `CreateSwapChain`, the swapchain's `ResizeBuffers`)
//! from the vtable ONCE and put an inline detour on it (`RawDetour`). An inline hook fires for every
//! object of the class, whatever its pointer, COM identity or wrapper, so nothing here decides by
//! identity: a device is any device, a context any IMMEDIATE context (`GetType() == 0`, asked once per
//! `this`). If the inline hook cannot be installed the vtable slot of the object at hand is patched
//! instead (`intercept::decide_how`), and the log says which method was used.
//!
//! A second class of objects (a wrapper layer next to the native one) gets a second detour slot per
//! method (`intercept::NSLOT`); a hook running inside another hook (a wrapper calling the native
//! function, our own resolve) is passed through unrecorded (`intercept::Reentry`).
//!
//! Every hook records and then calls the original with the original arguments; nothing is changed,
//! except the swapchain desc of the game's own swapchain (see `output.rs`) and the float upgrade.
//! `diag.rs` counts every call and remembers the `this` values, the vtable and the slot value seen.
//! COM is called through raw vtables so no D3D/DXGI binding crate is needed.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use retour::{GenericDetour, RawDetour};
use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::ctxid::{self, VerdictTable};
use crate::diag::{self, Budget, Counted, MethodSnap, MethodStats};
use crate::intercept::{self, ALL, HookHow, Method, NMETH, NSLOT, Reentry, SlotPlan};
use crate::ao;
use crate::classify::AoStage;
use crate::log;
use crate::passes::Target;
use crate::output;
use crate::recorder::{self, PassStart};
use crate::resolve;
use crate::shaders;
use crate::upgrade::{self, TexDesc};

pub(crate) type Obj = *mut c_void;

/// Vtable slot indices, checked against the Windows SDK 10.0.26100 headers (`d3d11.h`, `dxgi.h`,
/// `dxgi1_2.h`) by counting the entries of each `*Vtbl` struct.
#[allow(dead_code)]
pub mod vt {
    pub const RELEASE: usize = 2;
    // ID3D11Device
    pub const DEV_QUERY_INTERFACE: usize = 0;
    pub const DEV_CREATE_TEXTURE2D: usize = 5;
    pub const DEV_CREATE_TEXTURE3D: usize = 6;
    pub const DEV_CREATE_SRV: usize = 7;
    pub const DEV_CREATE_RTV: usize = 9;
    pub const DEV_CREATE_PIXEL_SHADER: usize = 15;
    pub const DEV_CREATE_BLEND_STATE: usize = 20;
    pub const DEV_GET_IMMEDIATE_CONTEXT: usize = 40;
    // ID3D11DeviceContext
    pub const DEV_CREATE_BUFFER: usize = 3;
    pub const DEV_CREATE_VERTEX_SHADER: usize = 12;
    pub const CTX_PS_SET_SRVS: usize = 8;
    pub const CTX_PS_SET_SHADER: usize = 9;
    pub const CTX_PS_SET_SAMPLERS: usize = 10;
    pub const CTX_VS_SET_SHADER: usize = 11;
    pub const CTX_MAP: usize = 14;
    pub const CTX_UNMAP: usize = 15;
    pub const CTX_PS_SET_CBS: usize = 16;
    pub const CTX_IA_SET_LAYOUT: usize = 17;
    pub const CTX_GS_SET_SHADER: usize = 23;
    pub const CTX_IA_SET_TOPOLOGY: usize = 24;
    pub const CTX_OM_SET_BLEND: usize = 35;
    pub const CTX_OM_SET_DEPTH: usize = 36;
    pub const CTX_RS_SET_STATE: usize = 43;
    pub const CTX_RS_SET_VIEWPORTS: usize = 44;
    pub const CTX_RS_SET_SCISSORS: usize = 45;
    pub const CTX_VS_GET_CBS: usize = 72;
    pub const CTX_PS_GET_SRVS: usize = 73;
    pub const CTX_PS_GET_SHADER: usize = 74;
    pub const CTX_VS_GET_SHADER: usize = 76;
    pub const CTX_PS_GET_CBS: usize = 77;
    pub const CTX_IA_GET_LAYOUT: usize = 78;
    pub const CTX_IA_GET_VBS: usize = 79;
    pub const CTX_GS_GET_SHADER: usize = 82;
    pub const CTX_IA_GET_TOPOLOGY: usize = 83;
    pub const CTX_OM_GET_RTS: usize = 89;
    pub const CTX_OM_GET_BLEND: usize = 91;
    pub const CTX_OM_GET_DEPTH: usize = 92;
    pub const CTX_RS_GET_STATE: usize = 94;
    pub const CTX_RS_GET_VIEWPORTS: usize = 95;
    pub const CTX_RS_GET_SCISSORS: usize = 96;
    pub const CHILD_GET_DEVICE: usize = 3;
    pub const CTX_GET_TYPE: usize = 112; // counted from ID3D11DeviceContextVtbl in d3d11.h (SDK 10.0.26100)
    pub const CTX_DRAW_INDEXED: usize = 12;
    pub const CTX_DRAW: usize = 13;
    pub const CTX_DRAW_INDEXED_INSTANCED: usize = 20;
    pub const CTX_DRAW_INSTANCED: usize = 21;
    pub const CTX_OM_SET_RENDER_TARGETS: usize = 33;
    pub const CTX_OM_SET_RTS_AND_UAVS: usize = 34;
    pub const CTX_DISPATCH: usize = 41;
    pub const CTX_COPY_SUBRESOURCE_REGION: usize = 46;
    pub const CTX_COPY_RESOURCE: usize = 47;
    pub const CTX_RESOLVE_SUBRESOURCE: usize = 57;
    // ID3D11View / ID3D11Resource / ID3D11Texture2D
    pub const VIEW_GET_RESOURCE: usize = 7;
    pub const VIEW_GET_DESC: usize = 8; // ID3D11RenderTargetView and ID3D11DepthStencilView
    pub const RES_GET_TYPE: usize = 7;
    pub const BUFFER_GET_DESC: usize = 10; // ID3D11Buffer: IUnknown 3 + DeviceChild 4 + Resource 3
    pub const TEX2D_GET_DESC: usize = 10;
    pub const BLEND_GET_DESC: usize = 7; // ID3D11BlendState: IUnknown 3 + DeviceChild 4
    // DXGI
    pub const DXGI_DEVICE_GET_ADAPTER: usize = 7;
    pub const DXGI_OBJECT_GET_PARENT: usize = 6;
    pub const FACTORY_CREATE_SWAP_CHAIN: usize = 10;
    pub const SWAPCHAIN_GET_DESC: usize = 12;
    pub const SWAPCHAIN_RESIZE_BUFFERS: usize = 13;
    pub const DXGI_SUBOBJECT_GET_DEVICE: usize = 7;
}

#[repr(C)]
pub(crate) struct Guid(pub(crate) u32, pub(crate) u16, pub(crate) u16, pub(crate) [u8; 8]);
const IID_DXGI_DEVICE: Guid = Guid(0x54ec77fa, 0x1377, 0x44e6, [0x8c, 0x32, 0x88, 0xfd, 0x5f, 0x44, 0xc8, 0x4c]);
const IID_DXGI_FACTORY: Guid = Guid(0x7b7166ec, 0x21c7, 0x44ae, [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69]);

/// `D3D11_TEXTURE2D_DESC` (44 bytes).
#[repr(C)]
#[derive(Default)]
pub(crate) struct Tex2dDesc {
    pub width: u32,
    pub height: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub samples: u32,
    pub quality: u32,
    pub usage: u32,
    pub bind: u32,
    pub cpu: u32,
    pub misc: u32,
}

/// `DXGI_SWAP_CHAIN_DESC`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SwapChainDesc {
    pub width: u32,
    pub height: u32,
    pub refresh: [u32; 2],
    pub format: u32,
    pub scanline: u32,
    pub scaling: u32,
    pub samples: [u32; 2],
    pub usage: u32,
    pub buffers: u32,
    pub window: usize,
    pub windowed: i32,
    pub effect: u32,
    pub flags: u32,
}

pub(crate) const RESOURCE_TEXTURE2D: u32 = 3;

const IID_ID3D11_DEVICE: Guid = Guid(0xdb6f6ddb, 0xac77, 0x4e88, [0x82, 0x53, 0x81, 0x9d, 0xf9, 0xbb, 0xf1, 0x40]);

/// What the hooks know in common: the back-buffer size. (No device or context pointer is kept: the
/// hooks decide per call, not by identity.)
pub struct Hooks {
    screen: AtomicU64,
}

static HOOKS: Hooks = Hooks { screen: AtomicU64::new(0) };

pub fn hooks() -> &'static Hooks {
    &HOOKS
}

impl Hooks {
    /// Back-buffer size, (0, 0) until a swapchain has been seen.
    pub fn screen(&self) -> (u32, u32) {
        let s = self.screen.load(Relaxed);
        ((s >> 32) as u32, s as u32)
    }

    pub fn set_screen(&self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        if self.screen.swap(((w as u64) << 32) | h as u64, Relaxed) != ((w as u64) << 32) | h as u64 {
            log!("d3d: screen size changed {w}x{h}");
        }
    }
}

// ---- per-method tables -----------------------------------------------------------------------

/// Trampoline (inline) or original function (vtable patch) per method and slot; 0 = not hooked.
static ORIG: [[AtomicUsize; NSLOT]; NMETH] = [const { [const { AtomicUsize::new(0) }; NSLOT] }; NMETH];
/// The native function address each slot was resolved to (what `intercept::plan_slot` compares).
static ADDR: [[AtomicUsize; NSLOT]; NMETH] = [const { [const { AtomicUsize::new(0) }; NSLOT] }; NMETH];
/// `intercept::how_code` per method and slot.
static HOW: [[AtomicU8; NSLOT]; NMETH] = [const { [const { AtomicU8::new(0) }; NSLOT] }; NMETH];
static STATS: [MethodStats; NMETH] = [const { MethodStats::new() }; NMETH];
static FIRST_LOGGED: [AtomicBool; NMETH] = [const { AtomicBool::new(false) }; NMETH];
/// The detours stay alive for the life of the process.
static KEEP: Mutex<Vec<RawDetour>> = Mutex::new(Vec::new());
static INSTALL_LOCK: Mutex<()> = Mutex::new(());
/// CreateTexture2D / CreatePixelShader calls per device pointer (diagnostics only).
static DEV_TEX: Counted<8> = Counted::new();
static DEV_PS: Counted<8> = Counted::new();
/// `GetType() == 0` per context pointer (positive = immediate), asked once per distinct `this`.
static CTX_IMMEDIATE: VerdictTable = VerdictTable::new();
/// Devices a hook was called on (class discovery runs once per pointer).
static DEV_SEEN: VerdictTable = VerdictTable::new();
static CREATE_LOGS: Budget = Budget::new();
static DEV_LOGS: Budget = Budget::new();

fn orig(m: Method, slot: usize) -> usize {
    ORIG[m as usize][slot].load(Relaxed)
}

fn hooked(m: Method) -> bool {
    HOW[m as usize].iter().any(|h| h.load(Relaxed) != 0)
}

/// The detour function of `m` for `slot` (one monomorphization per slot, so each has its own address).
fn hook_addr(m: Method, slot: usize) -> usize {
    macro_rules! a {
        ($h:ident) => {
            if slot == 0 { $h::<0> as *const () as usize } else { $h::<1> as *const () as usize }
        };
    }
    match m {
        Method::OmSet => a!(h_om_set),
        Method::OmSetUav => a!(h_om_set_uav),
        Method::Dispatch => a!(h_dispatch),
        Method::Draw => a!(h_draw),
        Method::DrawIndexed => a!(h_draw_indexed),
        Method::DrawInstanced => a!(h_draw_instanced),
        Method::DrawIndexedInstanced => a!(h_draw_indexed_instanced),
        Method::PsSetShader => a!(h_ps_set_shader),
        Method::CopyResource => a!(h_copy_resource),
        Method::CopySubresource => a!(h_copy_subresource),
        Method::ResolveSubresource => a!(h_resolve),
        Method::CreateTexture2d => a!(h_create_texture2d),
        Method::CreateRtv => a!(h_create_rtv),
        Method::CreateSrv => a!(h_create_srv),
        Method::CreatePixelShader => a!(h_create_pixel_shader),
        Method::CreateSwapChain => a!(h_create_swap_chain),
        Method::ResizeBuffers => a!(h_resize_buffers),
        Method::CreateVertexShader => a!(h_create_vertex_shader),
        Method::PsSetSamplers => a!(h_ps_set_samplers),
    }
}

/// File name (without the folder) of the module that contains `addr`; empty when none.
fn module_name(addr: usize) -> String {
    if addr == 0 {
        return String::new();
    }
    unsafe {
        let mut m = std::ptr::null_mut();
        if GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, addr as *const u16, &mut m) == 0 || m.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 520];
        let n = GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) as usize;
        diag::file_name_of(&String::from_utf16_lossy(&buf[..n.min(buf.len())]))
    }
}

/// Per-method snapshot for the 5 s line: how it is hooked, calls, distinct `this`, and where the vtable
/// pointer and slot of the last `this` point now.
pub fn method_snaps() -> Vec<MethodSnap> {
    ALL.iter()
        .map(|&m| {
            let (calls, seen, vtbl, slot) = STATS[m as usize].read();
            let codes = [HOW[m as usize][0].load(Relaxed), HOW[m as usize][1].load(Relaxed)];
            MethodSnap { name: m.name(), how: intercept::how_text(codes), calls, seen, last_vtbl: vtbl, vtbl_module: module_name(vtbl), last_slot: slot, slot_module: module_name(slot) }
        })
        .collect()
}

/// The three devices with the most CreateTexture2D / CreatePixelShader calls.
pub fn device_top3() -> (Vec<(usize, u64)>, Vec<(usize, u64)>) {
    (DEV_TEX.top(3), DEV_PS.top(3))
}

// ---- raw COM helpers -------------------------------------------------------------------------

pub(crate) unsafe fn vfn(obj: Obj, idx: usize) -> usize {
    unsafe { *(*(obj as *const *const usize)).add(idx) }
}

pub(crate) unsafe fn release(obj: Obj) {
    if !obj.is_null() {
        let f: unsafe extern "system" fn(Obj) -> u32 = unsafe { std::mem::transmute(vfn(obj, vt::RELEASE)) };
        unsafe { f(obj) };
    }
}

const NO_TARGET: Target = Target { w: 0, h: 0, fmt: 0 };

/// Format and size of what a view renders to: the view's own format wins (the texture may be
/// typeless), UNKNOWN (0) falls back to the texture's; size is that of the viewed mip level.
fn view_target_of(view_fmt: u32, res_fmt: u32, res_w: u32, res_h: u32, mip: u32) -> Target {
    Target {
        w: (res_w >> mip.min(31)).max(1),
        h: (res_h >> mip.min(31)).max(1),
        fmt: if view_fmt != 0 { view_fmt } else { res_fmt },
    }
}

/// Size and format of the texture behind a render-target (`is_depth` false) or depth-stencil view.
/// Null views and non-2D resources give an all-zero target (keeps slot positions).
pub(crate) unsafe fn view_target(view: Obj, is_depth: bool) -> Target {
    if view.is_null() {
        return NO_TARGET;
    }
    unsafe {
        let mut res: Obj = std::ptr::null_mut();
        let get_resource: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(view, vt::VIEW_GET_RESOURCE));
        get_resource(view, &mut res);
        if res.is_null() {
            return NO_TARGET;
        }
        let mut dim = 0u32;
        let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
        get_type(res, &mut dim);
        let mut t = NO_TARGET;
        if dim == RESOURCE_TEXTURE2D {
            let mut d = Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            // D3D11_RENDER_TARGET_VIEW_DESC {Format, ViewDimension, union} (union at word 2);
            // D3D11_DEPTH_STENCIL_VIEW_DESC {Format, ViewDimension, Flags, union} (union at word 3).
            // For the 2D and 2D-array dimensions the union starts with MipSlice; MS views use mip 0.
            let mut v = [0u32; 8];
            let get_view_desc: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(view, vt::VIEW_GET_DESC));
            get_view_desc(view, v.as_mut_ptr());
            let (tex2d, tex2d_array, base) = if is_depth { (3, 4, 3) } else { (4, 5, 2) };
            let mip = if v[1] == tex2d || v[1] == tex2d_array { v[base] } else { 0 };
            t = view_target_of(v[0], d.format, d.width, d.height, mip);
        }
        release(res);
        t
    }
}

pub(crate) unsafe fn swapchain_desc(sc: Obj) -> Option<SwapChainDesc> {
    if sc.is_null() {
        return None;
    }
    let mut d: SwapChainDesc = unsafe { std::mem::zeroed() };
    let get_desc: unsafe extern "system" fn(Obj, *mut SwapChainDesc) -> i32 = unsafe { std::mem::transmute(vfn(sc, vt::SWAPCHAIN_GET_DESC)) };
    (unsafe { get_desc(sc, &mut d) } >= 0).then_some(d)
}

pub(crate) unsafe fn swapchain_size(sc: Obj) -> Option<(u32, u32)> {
    unsafe { swapchain_desc(sc) }.map(|d| (d.width, d.height))
}

/// Replaces vtable slot `idx` of `obj` with `hook`, remembering the original in `orig`. A vtable
/// already patched (or a different vtable for a slot we already took) is left alone, so a hook
/// can never end up calling itself.
pub(crate) unsafe fn patch(obj: Obj, idx: usize, hook: usize, orig: &AtomicUsize, what: &str) -> bool {
    if obj.is_null() {
        return false;
    }
    unsafe {
        let slot = (*(obj as *const *mut usize)).add(idx);
        let cur = *slot;
        if cur == hook {
            return true;
        }
        if orig.load(Relaxed) != 0 {
            log!("d3d: {what}: another vtable for this slot, left alone");
            return false;
        }
        let mut old = 0u32;
        if VirtualProtect(slot as *const c_void, std::mem::size_of::<usize>(), PAGE_READWRITE, &mut old) == 0 {
            log!("d3d: {what}: VirtualProtect failed");
            return false;
        }
        orig.store(cur, Relaxed);
        *slot = hook;
        VirtualProtect(slot as *const c_void, std::mem::size_of::<usize>(), old, &mut old);
        log!("d3d: {what}: patched slot {idx} (original {cur:#x})");
        true
    }
}


// ---- hook installation ------------------------------------------------------------------------

/// Hooks one function body with an inline detour; the trampoline becomes the "original".
unsafe fn inline_hook(m: Method, slot: usize, addr: usize) -> Result<(), String> {
    unsafe {
        let d = RawDetour::new(addr as *const (), hook_addr(m, slot) as *const ()).map_err(|e| e.to_string())?;
        // The trampoline is stored before the detour is enabled: the hook may fire at once.
        ORIG[m as usize][slot].store(d.trampoline() as *const () as usize, Relaxed);
        if let Err(e) = d.enable() {
            ORIG[m as usize][slot].store(0, Relaxed);
            return Err(e.to_string());
        }
        KEEP.lock().unwrap_or_else(|e| e.into_inner()).push(d);
        Ok(())
    }
}

/// Hooks `m` for the class of `obj`: the function address is resolved from the vtable of `obj` once,
/// then detoured inline; when that is impossible the vtable slot of `obj` is patched. True when this
/// class is hooked (now or earlier).
unsafe fn hook_method(m: Method, obj: Obj) -> bool {
    if obj.is_null() {
        return false;
    }
    let mi = m as usize;
    let addr = unsafe { vfn(obj, m.index()) };
    if addr == hook_addr(m, 0) || addr == hook_addr(m, 1) {
        return true; // the vtable already holds our hook
    }
    let _lock = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let own = [ADDR[mi][0].load(Relaxed), ADDR[mi][1].load(Relaxed)];
    let others: Vec<usize> = ALL.iter().filter(|&&o| o != m).flat_map(|&o| ADDR[o as usize].iter().map(|a| a.load(Relaxed))).filter(|&a| a != 0).collect();
    let name = m.name();
    match intercept::plan_slot(addr, own, &others) {
        SlotPlan::NullAddress => {
            log!("d3d: {name}: the vtable entry is null; not hooked");
            false
        }
        SlotPlan::AlreadyHooked => true,
        SlotPlan::NoFreeSlot => {
            log!("d3d: {name}: both detour slots are taken by other function bodies ({addr:#x} is a third class); not hooked");
            false
        }
        SlotPlan::Hook { slot, inline } => {
            let inline_res = inline.then(|| unsafe { inline_hook(m, slot, addr) });
            match &inline_res {
                Some(Ok(())) => log!("d3d: inline hook {name} at {addr:#x} ok (module {}, detour slot {slot})", module_name(addr)),
                Some(Err(e)) => log!("d3d: inline hook {name} at {addr:#x} failed: {e}"),
                None => log!("d3d: inline hook {name} at {addr:#x} skipped: the body is shared with another hooked method"),
            }
            let how = intercept::decide_how(inline_res, || unsafe { patch(obj, m.index(), hook_addr(m, slot), &ORIG[mi][slot], name) });
            match how {
                HookHow::Inline => {}
                HookHow::Vtable => log!("d3d: {name}: vtable slot {} patched instead of the inline hook (original {addr:#x}, module {})", m.index(), module_name(addr)),
                HookHow::Failed => log!("d3d: {name}: not hooked (the inline hook and the vtable patch both failed)"),
            }
            if how != HookHow::Failed {
                ADDR[mi][slot].store(addr, Relaxed);
                HOW[mi][slot].store(intercept::how_code(how), Relaxed);
            }
            how != HookHow::Failed
        }
    }
}

/// PSSetSamplers is hooked (the forced anisotropic filtering needs it, see `aniso.rs`).
pub(crate) fn ps_set_samplers_hooked() -> bool {
    hooked(Method::PsSetSamplers)
}

const CONTEXT_METHODS: [Method; 12] = [
    Method::OmSet,
    Method::OmSetUav,
    Method::Dispatch,
    Method::Draw,
    Method::DrawIndexed,
    Method::DrawInstanced,
    Method::DrawIndexedInstanced,
    Method::CopyResource,
    Method::CopySubresource,
    Method::ResolveSubresource,
    Method::PsSetShader,
    Method::PsSetSamplers,
];

/// Hooks the context methods for the class of `ctx`.
unsafe fn hook_context_class(ctx: Obj) {
    for m in CONTEXT_METHODS {
        unsafe { hook_method(m, ctx) };
    }
    if !shaders::mechanism_usable(hooked(Method::PsSetShader)) {
        shaders::mark_unavailable();
        log!("shaders: PSSetShader not hooked; shader substitution stays off");
    }
}

/// Both view-creation methods were hooked (the same condition as `upgrade::may_upgrade`).
static VIEW_HOOKS_OK: AtomicBool = AtomicBool::new(false);

pub(crate) fn view_hooks_ok() -> bool {
    VIEW_HOOKS_OK.load(Relaxed)
}

/// Hooks the device methods for the class of `dev`: the views first, the texture method (which starts
/// the upgrades) only if both views are hooked for this class.
unsafe fn hook_device_class(dev: Obj) {
    unsafe {
        let rtv = hook_method(Method::CreateRtv, dev);
        let srv = hook_method(Method::CreateSrv, dev);
        VIEW_HOOKS_OK.store(upgrade::may_upgrade(hooked(Method::CreateRtv), hooked(Method::CreateSrv)), Relaxed);
        // Independent of the upgrade: records the hash of every pixel shader created.
        if !hook_method(Method::CreatePixelShader, dev) && !hooked(Method::CreatePixelShader) {
            shaders::mark_unavailable();
            log!("shaders: CreatePixelShader not hooked; registry stays empty, shader substitution stays off");
        }
        // Draw diagnostic only: the vertex shader hashes. A failure costs nothing else.
        if !hook_method(Method::CreateVertexShader, dev) && !hooked(Method::CreateVertexShader) {
            log!("drawdiag: CreateVertexShader not hooked; the draw capture will show vs=unknown");
        }
        if upgrade::may_upgrade(rtv, srv) {
            hook_method(Method::CreateTexture2d, dev);
        } else if !hooked(Method::CreateTexture2d) {
            upgrade::disable_after_failure();
            log!("upgrade: could not hook the view creation calls (rtv={rtv} srv={srv}); float upgrade off");
        }
    }
}

/// Finds the DXGI factory of `device` (device -> adapter -> parent) and hooks its `CreateSwapChain`.
unsafe fn hook_factory_of(device: Obj) {
    unsafe {
        let mut dxgi_dev: Obj = std::ptr::null_mut();
        let qi: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(device, vt::DEV_QUERY_INTERFACE));
        if qi(device, &IID_DXGI_DEVICE, &mut dxgi_dev) < 0 || dxgi_dev.is_null() {
            log!("d3d: device has no IDXGIDevice; factory not hooked");
            return;
        }
        let mut adapter: Obj = std::ptr::null_mut();
        let get_adapter: unsafe extern "system" fn(Obj, *mut Obj) -> i32 = std::mem::transmute(vfn(dxgi_dev, vt::DXGI_DEVICE_GET_ADAPTER));
        let mut factory: Obj = std::ptr::null_mut();
        if get_adapter(dxgi_dev, &mut adapter) >= 0 && !adapter.is_null() {
            let get_parent: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(adapter, vt::DXGI_OBJECT_GET_PARENT));
            get_parent(adapter, &IID_DXGI_FACTORY, &mut factory);
        }
        if factory.is_null() {
            log!("d3d: no IDXGIFactory from the device; factory not hooked");
        } else {
            hook_method(Method::CreateSwapChain, factory);
        }
        release(factory);
        release(adapter);
        release(dxgi_dev);
    }
}

/// Hooks the `ResizeBuffers` of the swapchain's class (the game's swapchain, see `output.rs`).
///
/// # Safety
/// `sc` must be a live `IDXGISwapChain*`.
pub(crate) unsafe fn hook_resize_buffers(sc: Obj) -> bool {
    unsafe { hook_method(Method::ResizeBuffers, sc) }
}

/// `device` as an `ID3D11Device` (a new reference, or null); the argument of `CreateSwapChain` may be
/// any `IUnknown` (a D3D12 queue, a wrapper).
unsafe fn as_d3d11_device(o: Obj) -> Obj {
    if o.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let qi: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(o, vt::DEV_QUERY_INTERFACE));
        let mut d: Obj = std::ptr::null_mut();
        if qi(o, &IID_ID3D11_DEVICE, &mut d) < 0 {
            return std::ptr::null_mut();
        }
        d
    }
}

/// The device of a swapchain (a new reference, or null).
pub(crate) unsafe fn device_from_swapchain(sc: Obj) -> Obj {
    if sc.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let get: unsafe extern "system" fn(Obj, *const Guid, *mut Obj) -> i32 = std::mem::transmute(vfn(sc, vt::DXGI_SUBOBJECT_GET_DEVICE));
        let mut d: Obj = std::ptr::null_mut();
        if get(sc, &IID_ID3D11_DEVICE, &mut d) < 0 {
            return std::ptr::null_mut();
        }
        d
    }
}

pub(crate) unsafe fn immediate_context_of(dev: Obj) -> Obj {
    unsafe {
        let get_ctx: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(dev, vt::DEV_GET_IMMEDIATE_CONTEXT));
        let mut ctx: Obj = std::ptr::null_mut();
        get_ctx(dev, &mut ctx);
        ctx
    }
}

/// A device we have not looked at yet reached a hook (or a swapchain creation): make sure its device
/// class, its immediate context's class and its factory are hooked. Once per device pointer. This is
/// what finds the game's own classes if the creation detours saw only another component's probe.
unsafe fn discover_device(dev: Obj) {
    if dev.is_null() || ignoring() || DEV_SEEN.lookup(dev as usize).is_some() {
        return;
    }
    DEV_SEEN.store(dev as usize, true);
    unsafe {
        let ctx = immediate_context_of(dev);
        if DEV_LOGS.take(12) {
            log!("d3d: device {dev:p} reached a hook; checking its classes (immediate context {ctx:p})");
        }
        hook_device_class(dev);
        if !ctx.is_null() {
            hook_context_class(ctx);
            release(ctx);
        }
        hook_factory_of(dev);
    }
}

// ---- instrumentation shared by the hooks --------------------------------------------------------

unsafe fn get_type(ctx: Obj) -> u32 {
    unsafe {
        let f: unsafe extern "system" fn(Obj) -> u32 = std::mem::transmute(vfn(ctx, vt::CTX_GET_TYPE));
        f(ctx)
    }
}

/// `GetType()` of a context, asked once per pointer (0 = immediate, 1 = deferred).
fn ctx_type(ctx: Obj) -> u32 {
    match CTX_IMMEDIATE.lookup(ctx as usize) {
        Some(true) => ctxid::CONTEXT_IMMEDIATE,
        Some(false) => 1,
        None => {
            let t = unsafe { get_type(ctx) };
            CTX_IMMEDIATE.store(ctx as usize, t == ctxid::CONTEXT_IMMEDIATE);
            t
        }
    }
}

fn is_immediate(ctx: Obj) -> bool {
    !ctx.is_null() && std::panic::catch_unwind(|| ctx_type(ctx) == ctxid::CONTEXT_IMMEDIATE).unwrap_or(false)
}

/// Counts a call of `m` on `this` before any filtering: calls, distinct `this`, the vtable pointer and
/// slot value read from the live object, and the first-call line. Never unwinds.
fn note_call(m: Method, this: Obj) {
    if this.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let vtbl = *(this as *const usize);
        let slot = if vtbl != 0 { *(vtbl as *const usize).add(m.index()) } else { 0 };
        let n = STATS[m as usize].note(this as usize, vtbl, slot);
        if n.first_call || n.new_this.is_some() {
            let ty = (m.class() == intercept::Class::Context).then(|| ctx_type(this));
            if let (Some(i), Some(t)) = (n.new_this, ty) {
                STATS[m as usize].set_type(i, t);
            }
            if n.first_call && !FIRST_LOGGED[m as usize].swap(true, Relaxed) {
                log!("{}", diag::first_call_line(m.name(), this as usize, ty));
            }
        }
    });
}

/// A device-side call: counted, and the device's classes are checked once.
fn note_device_call(m: Method, this: Obj) {
    note_call(m, this);
    if !ignoring() {
        let _ = std::panic::catch_unwind(|| unsafe { discover_device(this) });
    }
}

// ---- context hooks (record, then call the original) ------------------------------------------

/// Counts the call and tells whether it must be recorded: not our own call (`ignoring`) and an
/// IMMEDIATE context (any one: no device or identity test). Never unwinds.
fn context_gate(this: Obj, m: Method) -> bool {
    note_call(m, this);
    let om = matches!(m, Method::OmSet | Method::OmSetUav);
    if om {
        diag::OM_CALLS.fetch_add(1, Relaxed);
    } else if m.is_draw() {
        diag::DRAWS.fetch_add(1, Relaxed);
    }
    if ignoring() {
        if om {
            diag::OM_IGNORED.fetch_add(1, Relaxed);
        }
        return false;
    }
    let ok = is_immediate(this);
    if ok && om {
        diag::OM_ACCEPTED.fetch_add(1, Relaxed);
    }
    ok
}

/// Runs `f` for a call on an immediate context. Our own calls (the resolve) are never recorded.
fn record<R>(this: Obj, m: Method, f: impl FnOnce() -> R) -> Option<R> {
    if context_gate(this, m) {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
    } else {
        None
    }
}

unsafe fn record_om(ctx: Obj, num: u32, rtvs: *const Obj, dsv: Obj) -> Option<PassStart> {
    let n = (num as usize).min(8);
    let mut rt = [NO_TARGET; 8];
    for (i, slot) in rt.iter_mut().enumerate().take(n) {
        if !rtvs.is_null() {
            *slot = unsafe { view_target(*rtvs.add(i), false) };
        }
    }
    let depth = if dsv.is_null() { None } else { Some(unsafe { view_target(dsv, true) }) };
    recorder::om_set_render_targets(ctx as usize, &rt[..n], depth, HOOKS.screen())
}

fn maybe_ao(this: Obj, start: Option<PassStart>, num: u32, rtvs: *const Obj) {
    let Some(start) = start else { return };
    if start.ao == AoStage::None || !ao::needs_stages() || num == 0 || rtvs.is_null() || recorder::game_context() != this as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let rtv = *rtvs;
        if rtv.is_null() {
            return;
        }
        let screen = HOOKS.screen();
        match start.ao {
            AoStage::DepthLinear => ao::note_depth(rtv, screen),
            AoStage::Effects => {
                if ao::wants() {
                    ao::apply(this, rtv, screen);
                }
            }
            AoStage::None => {}
        }
    });
}

/// The UI pass is about to start on the game's context: if the resolve applies, run it on the new
/// binding's RT0 before the original call. One atomic load when the resolve is off; never unwinds.
fn maybe_resolve(this: Obj, start: Option<PassStart>, num: u32, rtvs: *const Obj) {
    let Some(start) = start else { return };
    if !resolve::PASS_ENABLED || !start.ui_start || !resolve::wants() || num == 0 || rtvs.is_null() || recorder::game_context() != this as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let rtv = *rtvs;
        if rtv.is_null() {
            return;
        }
        let screen = HOOKS.screen();
        let rt0 = view_target(rtv, false);
        match resolve::decide(rt0, start.prev_rt0, screen) {
            resolve::Decision::Run => {
                resolve::apply(this, rtv, screen);
            }
            resolve::Decision::SelfCheck(why) => resolve::log_self_check_once(why),
        }
    });
}

type OmSetFn = unsafe extern "system" fn(Obj, u32, *const Obj, Obj);
type OmSetUavFn = unsafe extern "system" fn(Obj, u32, *const Obj, Obj, u32, u32, *const Obj, *const u32);

unsafe extern "system" fn h_om_set<const S: usize>(this: Obj, num: u32, rtvs: *const Obj, dsv: Obj) {
    let f: OmSetFn = unsafe { std::mem::transmute(orig(Method::OmSet, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, num, rtvs, dsv) } };
    let start = record(this, Method::OmSet, || unsafe { record_om(this, num, rtvs, dsv) }).flatten();
    maybe_ao(this, start, num, rtvs);
    maybe_resolve(this, start, num, rtvs);
    clamp_note_targets(this, num, rtvs);
    unsafe { f(this, num, rtvs, dsv) }
}

unsafe extern "system" fn h_om_set_uav<const S: usize>(this: Obj, num: u32, rtvs: *const Obj, dsv: Obj, uav_start: u32, num_uav: u32, uavs: *const Obj, counts: *const u32) {
    let f: OmSetUavFn = unsafe { std::mem::transmute(orig(Method::OmSetUav, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, num, rtvs, dsv, uav_start, num_uav, uavs, counts) } };
    // D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL (0xffffffff): the render targets are not changed.
    if num != u32::MAX {
        let start = record(this, Method::OmSetUav, || unsafe { record_om(this, num, rtvs, dsv) }).flatten();
        maybe_ao(this, start, num, rtvs);
        maybe_resolve(this, start, num, rtvs);
        clamp_note_targets(this, num, rtvs);
    } else {
        note_call(Method::OmSetUav, this);
    }
    unsafe { f(this, num, rtvs, dsv, uav_start, num_uav, uavs, counts) }
}

unsafe extern "system" fn h_dispatch<const S: usize>(this: Obj, x: u32, y: u32, z: u32) {
    let f: unsafe extern "system" fn(Obj, u32, u32, u32) = unsafe { std::mem::transmute(orig(Method::Dispatch, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, x, y, z) } };
    record(this, Method::Dispatch, || recorder::dispatch(this as usize));
    unsafe { f(this, x, y, z) }
}

/// Debug (only while the debug fill is on): the final pass (afdc14cb) is bound.
static FINAL_BOUND: AtomicBool = AtomicBool::new(false);

/// Debug: logs the texture the final pass samples at slot 0 (compare with the resolve's target).
fn final_probe(this: Obj) {
    if !FINAL_BOUND.load(Relaxed) || crate::resolve::debug_level() != 2 || ignoring() || !is_immediate(this) {
        return;
    }
    static N: AtomicUsize = AtomicUsize::new(0);
    if N.fetch_add(1, Relaxed) % 240 != 0 {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let mut srv: Obj = std::ptr::null_mut();
        let get: unsafe extern "system" fn(Obj, u32, u32, *mut Obj) = std::mem::transmute(vfn(this, 73)); // PSGetShaderResources
        get(this, 0, 1, &mut srv);
        if srv.is_null() {
            log!("final pass draw: no SRV at slot 0");
            return;
        }
        let mut res: Obj = std::ptr::null_mut();
        let gr: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(srv, vt::VIEW_GET_RESOURCE));
        gr(srv, &mut res);
        log!("final pass draw: samples resource {:p} (format {:?})", res, if res.is_null() { None } else { texture_format(res) });
        if !res.is_null() {
            let mut dev: Obj = std::ptr::null_mut();
            let gd: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(this, vt::CHILD_GET_DEVICE));
            gd(this, &mut dev);
            let mut d = Tex2dDesc::default();
            let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
            get_desc(res, &mut d);
            if !dev.is_null() && d.width > 3 {
                // Debug fill: the light-adjust substitute paints the left half red and the top right black;
                // these three pixels say whether that survives to the input of the final pass.
                let (w, h) = (d.width, d.height);
                crate::resolve::probe(dev, this, res, &d, w / 4, h / 2, "final pass input, left half (red if the data path works)");
                crate::resolve::probe(dev, this, res, &d, w * 3 / 4, h / 4, "final pass input, top right (black if the data path works)");
                crate::resolve::probe(dev, this, res, &d, w * 3 / 4, h * 3 / 4, "final pass input, bottom right (the scene)");
            }
            release(dev);
        }
        release(res);
        release(srv);
    });
}

unsafe extern "system" fn h_draw<const S: usize>(this: Obj, count: u32, start: u32) {
    let f: unsafe extern "system" fn(Obj, u32, u32) = unsafe { std::mem::transmute(orig(Method::Draw, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, count, start) } };
    crate::datapath::on_draw(this);
    let _aa = crate::aa::draw_guard(this);
    let _look = crate::look::draw_guard(this);
    crate::furmask::before_draw(this, &|| unsafe { f(this, count, start) }, crate::shellfur::Call::Draw(count, start));
    final_probe(this);
    clamp_before_draw(this);
    record(this, Method::Draw, || {
        recorder::draw(this as usize);
        crate::drawdiag::on_draw(this, crate::drawdiag::DrawArgs::draw(count));
    });
    unsafe { f(this, count, start) }
}

unsafe extern "system" fn h_draw_indexed<const S: usize>(this: Obj, count: u32, start: u32, base: i32) {
    let f: unsafe extern "system" fn(Obj, u32, u32, i32) = unsafe { std::mem::transmute(orig(Method::DrawIndexed, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, count, start, base) } };
    crate::datapath::on_draw(this);
    let _aa = crate::aa::draw_guard(this);
    let _look = crate::look::draw_guard(this);
    crate::furmask::before_draw(this, &|| unsafe { f(this, count, start, base) }, crate::shellfur::Call::Indexed(count, start, base));
    final_probe(this);
    clamp_before_draw(this);
    record(this, Method::DrawIndexed, || {
        recorder::draw(this as usize);
        crate::drawdiag::on_draw(this, crate::drawdiag::DrawArgs::indexed(count));
    });
    unsafe { f(this, count, start, base) }
}

unsafe extern "system" fn h_draw_instanced<const S: usize>(this: Obj, vc: u32, ic: u32, sv: u32, si: u32) {
    let f: unsafe extern "system" fn(Obj, u32, u32, u32, u32) = unsafe { std::mem::transmute(orig(Method::DrawInstanced, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, vc, ic, sv, si) } };
    crate::datapath::on_draw(this);
    let _aa = crate::aa::draw_guard(this);
    let _look = crate::look::draw_guard(this);
    crate::furmask::before_draw(this, &|| unsafe { f(this, vc, ic, sv, si) }, crate::shellfur::Call::Instanced(vc, ic, sv, si));
    final_probe(this);
    clamp_before_draw(this);
    record(this, Method::DrawInstanced, || {
        recorder::draw(this as usize);
        crate::drawdiag::on_draw(this, crate::drawdiag::DrawArgs::instanced(vc, ic));
    });
    unsafe { f(this, vc, ic, sv, si) }
}

unsafe extern "system" fn h_draw_indexed_instanced<const S: usize>(this: Obj, ic: u32, inst: u32, si: u32, bv: i32, sinst: u32) {
    let f: unsafe extern "system" fn(Obj, u32, u32, u32, i32, u32) = unsafe { std::mem::transmute(orig(Method::DrawIndexedInstanced, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, ic, inst, si, bv, sinst) } };
    crate::datapath::on_draw(this);
    let _aa = crate::aa::draw_guard(this);
    let _look = crate::look::draw_guard(this);
    crate::furmask::before_draw(this, &|| unsafe { f(this, ic, inst, si, bv, sinst) }, crate::shellfur::Call::IndexedInstanced(ic, inst, si, bv, sinst));
    clamp_before_draw(this);
    record(this, Method::DrawIndexedInstanced, || {
        recorder::draw(this as usize);
        crate::drawdiag::on_draw(this, crate::drawdiag::DrawArgs::indexed_instanced(ic, inst));
    });
    unsafe { f(this, ic, inst, si, bv, sinst) }
}

// ---- float scene upgrade: device hooks -------------------------------------------------------

static UPGRADED: AtomicUsize = AtomicUsize::new(0);

/// Scene targets created as float so far.
pub fn upgraded_count() -> usize {
    UPGRADED.load(Relaxed)
}

/// Distinct copy format mismatches logged so far.
pub fn copy_mismatches() -> usize {
    SEEN_MISMATCH.lock().map_or(0, |s| s.len())
}

/// Decides, from the caller's desc, whether to create the texture as float. Pure reads only.
unsafe fn upgraded_desc(desc: *const Tex2dDesc, initial: *const c_void) -> Option<Tex2dDesc> {
    if desc.is_null() || !initial.is_null() || !upgrade::active() {
        return None;
    }
    let d = unsafe { &*desc };
    let t = TexDesc { w: d.width, h: d.height, fmt: d.format, bind_flags: d.bind, mips: d.mips, samples: d.samples, usage: d.usage, cpu_access: d.cpu, misc_flags: d.misc, quality: d.quality, array: d.array };
    let screen = HOOKS.screen();
    let fmt = upgrade::target_format(&t, screen).or_else(|| output::game_client_size().and_then(|c| upgrade::target_format(&t, c)));
    // Diagnostic: why do large 8-bit render targets not qualify? (first few distinct only)
    if fmt.is_none() && t.fmt == upgrade::FMT_BGRA && t.bind_flags & upgrade::BIND_RENDER_TARGET != 0 && t.w >= 512 {
        static SEEN: Mutex<Vec<(u32, u32, u32)>> = Mutex::new(Vec::new());
        if let Ok(mut s) = SEEN.lock() {
            if s.len() < 24 && !s.contains(&(t.w, t.h, t.bind_flags)) {
                s.push((t.w, t.h, t.bind_flags));
                log!("upgrade: not upgraded {}x{} bind={:#x} mips={} samples={} usage={} cpu={} misc={:#x} q={} array={} (screen {}x{}, active={})", t.w, t.h, t.bind_flags, t.mips, t.samples, t.usage, t.cpu_access, t.misc_flags, t.quality, t.array, screen.0, screen.1, upgrade::active());
            }
        }
    }
    let fmt = fmt?;
    Some(Tex2dDesc { format: fmt, ..*d })
}

type CreateTexFn = unsafe extern "system" fn(Obj, *const Tex2dDesc, *const c_void, *mut Obj) -> i32;

/// Any device (not one inside our own `ignoring()` scope): the rule only selects textures of the
/// current screen size (or a quarter of it) with the scene's format and bind flags, so other tools'
/// devices are not touched in practice.
unsafe extern "system" fn h_create_texture2d<const S: usize>(this: Obj, desc: *const Tex2dDesc, initial: *const c_void, out: *mut Obj) -> i32 {
    let f: CreateTexFn = unsafe { std::mem::transmute(orig(Method::CreateTexture2d, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, desc, initial, out) } };
    note_device_call(Method::CreateTexture2d, this);
    DEV_TEX.note(this as usize);
    // `out` null is a validation-only call; leave it alone.
    let copy = if !ignoring() && !out.is_null() { std::panic::catch_unwind(|| unsafe { upgraded_desc(desc, initial) }).ok().flatten() } else { None };
    let Some(copy) = copy else {
        return unsafe { f(this, desc, initial, out) };
    };
    let hr = unsafe { f(this, &copy, initial, out) };
    if hr >= 0 {
        upgrade::note_upgraded();
        let _ = std::panic::catch_unwind(|| unsafe { mark_upgraded(*out) });
        let n = UPGRADED.fetch_add(1, Relaxed) + 1;
        if n <= 32 {
            let d = unsafe { &*desc };
            log!("upgrade: texture {}x{} B8G8R8A8_UNORM -> R16G16B16A16_FLOAT (#{n}, device {this:p})", d.width, d.height);
        }
        return hr;
    }
    // The float texture could not be created: the game gets what it asked for, upgrades stop.
    // Nothing was returned in `out` on failure, so there is nothing to release.
    upgrade::disable_after_failure();
    log!("upgrade: float texture creation failed (hr={hr:#x}); upgrades disabled, retrying 8-bit");
    unsafe { f(this, desc, initial, out) }
}

// ---- v18: UNORM clamp emulation (see clampemu.rs) ---------------------------------------------

/// Private-data key marking a texture the mod upgraded to float ({6d1c0f4e-7a2b-4c18-9e55-18b0c3a1f718}).
const UPGRADED_GUID: [u8; 16] = [0x4e, 0x0f, 0x1c, 0x6d, 0x2b, 0x7a, 0x18, 0x4c, 0x9e, 0x55, 0x18, 0xb0, 0xc3, 0xa1, 0xf7, 0x18];
const UPGRADED_MAGIC: u32 = 0x5547_3138;
const CHILD_GET_PRIVATE_DATA: usize = 4;
const CHILD_SET_PRIVATE_DATA: usize = 5;

pub(crate) unsafe fn mark_upgraded(tex: Obj) {
    if tex.is_null() {
        return;
    }
    unsafe {
        let set: unsafe extern "system" fn(Obj, *const u8, u32, *const c_void) -> i32 = std::mem::transmute(vfn(tex, CHILD_SET_PRIVATE_DATA));
        set(tex, UPGRADED_GUID.as_ptr(), 4, &UPGRADED_MAGIC as *const u32 as *const c_void);
    }
}

/// The view's resource is a texture the mod upgraded (a reference is taken and released).
pub(crate) unsafe fn view_is_upgraded(view: Obj) -> bool {
    if view.is_null() {
        return false;
    }
    unsafe {
        let mut res: Obj = std::ptr::null_mut();
        let get_res: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(view, vt::VIEW_GET_RESOURCE));
        get_res(view, &mut res);
        if res.is_null() {
            return false;
        }
        let get: unsafe extern "system" fn(Obj, *const u8, *mut u32, *mut c_void) -> i32 = std::mem::transmute(vfn(res, CHILD_GET_PRIVATE_DATA));
        let mut v = 0u32;
        let mut size = 4u32;
        let hr = get(res, UPGRADED_GUID.as_ptr(), &mut size, &mut v as *mut u32 as *mut c_void);
        release(res);
        hr >= 0 && size == 4 && v == UPGRADED_MAGIC
    }
}

/// The game bound render targets: which colour slots are upgraded textures (only while float targets exist).
fn clamp_note_targets(this: Obj, num: u32, rtvs: *const Obj) {
    if ignoring() || recorder::game_context() != this as usize {
        return;
    }
    if !upgrade::float_scene_exists() || rtvs.is_null() {
        crate::clampemu::note_targets(0, 0);
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let (mut up, mut bound) = (0u8, 0u8);
        for i in 0..(num as usize).min(8) {
            let v = *rtvs.add(i);
            if v.is_null() {
                continue;
            }
            bound |= 1 << i;
            if view_is_upgraded(v) {
                up |= 1 << i;
            }
        }
        crate::clampemu::note_targets(up, bound);
    });
}

/// Before an engine draw on the game context: binds the clamped twin of the current pixel shader when every bound colour
/// target is upgraded, the original otherwise. One atomic swap unless the shader or the targets changed.
fn clamp_before_draw(this: Obj) {
    if !crate::clampemu::needs_check() || ignoring() || recorder::game_context() != this as usize {
        return;
    }
    let _ = std::panic::catch_unwind(|| unsafe {
        let get: unsafe extern "system" fn(Obj, *mut Obj, *mut Obj, *mut u32) = std::mem::transmute(vfn(this, vt::CTX_PS_GET_SHADER));
        let mut cur: Obj = std::ptr::null_mut();
        get(this, &mut cur, std::ptr::null_mut(), std::ptr::null_mut());
        if cur.is_null() {
            return;
        }
        if let Some(next) = crate::clampemu::decide(cur as usize) {
            // Nested in this hook: the PSSetShader hook passes it straight through (the modules keep the original pointer).
            let set: unsafe extern "system" fn(Obj, Obj, *const Obj, u32) = std::mem::transmute(vfn(this, vt::CTX_PS_SET_SHADER));
            set(this, next as Obj, std::ptr::null(), 0);
            crate::clampemu::note_swap();
        }
        release(cur);
    });
}

/// A game pixel shader was created: its clamped twin is created next to it (only while the float upgrade is possible).
unsafe fn create_twin(f: CreatePsFn, dev: Obj, sh: Obj, bytes: &[u8]) {
    if !upgrade::active() && !upgrade::float_scene_exists() {
        return;
    }
    let stale = match crate::clampemu::patch(bytes) {
        Ok(twin) => {
            let mut out: Obj = std::ptr::null_mut();
            let hr = unsafe { f(dev, twin.as_ptr(), twin.len(), std::ptr::null_mut(), &mut out) };
            if hr < 0 || out.is_null() {
                crate::clampemu::note_failed();
                static N: AtomicUsize = AtomicUsize::new(0);
                if N.fetch_add(1, Relaxed) < 8 {
                    log!("clamp: twin of ps#{:08x} refused (hr={hr:#x}); the original is kept", shaders::fnv1a32(bytes));
                }
                crate::clampemu::forget(sh as usize)
            } else {
                // The twin has the original hash: every lookup by pointer (diagnostics, the fur) sees the same shader.
                shaders::registry().record(out as usize, bytes);
                crate::clampemu::remember(sh as usize, out as usize)
            }
        }
        Err(_) => {
            crate::clampemu::note_skipped();
            crate::clampemu::forget(sh as usize)
        }
    };
    if let Some(old) = stale {
        unsafe { release(old as Obj) };
    }
}

/// Real format of a 2D texture resource (no reference taken); None for other resources.
unsafe fn texture_format(res: Obj) -> Option<u32> {
    if res.is_null() {
        return None;
    }
    unsafe {
        let mut dim = 0u32;
        let get_type: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(res, vt::RES_GET_TYPE));
        get_type(res, &mut dim);
        if dim != RESOURCE_TEXTURE2D {
            return None;
        }
        let mut d = Tex2dDesc::default();
        let get_desc: unsafe extern "system" fn(Obj, *mut Tex2dDesc) = std::mem::transmute(vfn(res, vt::TEX2D_GET_DESC));
        get_desc(res, &mut d);
        Some(d.format)
    }
}

/// Same-size copy of a view desc with the format word replaced when `view_format` says so.
/// `N` is the desc size in words (RTV 5, SRV 6); the caller's struct is never written.
unsafe fn fixed_view_desc<const N: usize>(res: Obj, desc: *const [u32; N]) -> Option<[u32; N]> {
    if desc.is_null() {
        return None;
    }
    let mut copy = unsafe { *desc };
    let fmt = upgrade::view_format(copy[0], unsafe { texture_format(res) }?);
    if fmt == copy[0] {
        return None;
    }
    copy[0] = fmt;
    Some(copy)
}

/// Calls a view-creation original. When a REWRITTEN desc made it fail, upgrades are turned off and
/// the call is retried once with the game's own desc (nothing is created on failure, nothing leaks).
macro_rules! view_hook {
    ($name:ident, $method:expr, $n:literal, $what:literal) => {
        unsafe extern "system" fn $name<const S: usize>(this: Obj, res: Obj, desc: *const [u32; $n], out: *mut Obj) -> i32 {
            let f: unsafe extern "system" fn(Obj, Obj, *const [u32; $n], *mut Obj) -> i32 = unsafe { std::mem::transmute(orig($method, S)) };
            let Some(_g) = Reentry::enter() else { return unsafe { f(this, res, desc, out) } };
            note_device_call($method, this);
            let copy = if !ignoring() { std::panic::catch_unwind(|| unsafe { fixed_view_desc(res, desc) }).ok().flatten() } else { None };
            let Some(c) = copy else {
                return unsafe { f(this, res, desc, out) };
            };
            let hr = unsafe { f(this, res, &c, out) };
            if hr >= 0 {
                return hr;
            }
            upgrade::disable_after_failure();
            log!("upgrade: {} with float view failed (hr={hr:#x}); upgrades disabled, retrying with the game's desc", $what);
            unsafe { f(this, res, desc, out) }
        }
    };
}

view_hook!(h_create_rtv, Method::CreateRtv, 5, "CreateRenderTargetView");
view_hook!(h_create_srv, Method::CreateSrv, 6, "CreateShaderResourceView");

// ---- pixel shader registry and substitution --------------------------------------------------

static PS_LOGGED: AtomicUsize = AtomicUsize::new(0);

type CreatePsFn = unsafe extern "system" fn(Obj, *const u8, usize, Obj, *mut Obj) -> i32;

unsafe extern "system" fn h_create_pixel_shader<const S: usize>(this: Obj, code: *const u8, len: usize, linkage: Obj, out: *mut Obj) -> i32 {
    let f: CreatePsFn = unsafe { std::mem::transmute(orig(Method::CreatePixelShader, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, code, len, linkage, out) } };
    note_device_call(Method::CreatePixelShader, this);
    let hr = unsafe { f(this, code, len, linkage, out) };
    if hr >= 0 && !out.is_null() && !code.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe {
            diag::PS_CALLS.fetch_add(1, Relaxed);
            DEV_PS.note(this as usize);
            let sh = *out;
            if sh.is_null() || ignoring() {
                return;
            }
            let bytes = std::slice::from_raw_parts(code, len);
            let hash = shaders::fnv1a32(bytes);
            if crate::hdr_shaders::known_engine_hash(hash).is_some() {
                diag::PS_ENGINE.fetch_add(1, Relaxed);
            }
            // Any device: the registry is keyed by the shader pointer, which is unique across devices.
            diag::PS_RECORDED.fetch_add(1, Relaxed);
            shaders::registry().record(sh as usize, bytes);
            create_twin(f, this, sh, bytes);
            let n = PS_LOGGED.fetch_add(1, Relaxed);
            if n < 8 {
                let dxbc = if len >= 28 { u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) } else { 0 };
                log!("shaders: CreatePixelShader {sh:p} len={len} dxbc_size={dxbc} hash={hash:08x} device={this:p}");
            }
            if hash == output::FINAL_HASH {
                output::note_final_seen();
            }
            if let Some(name) = crate::hdr_shaders::note_engine_hash(hash) {
                log!("seen engine shader {name}");
            }
        });
    }
    hr
}

/// Draw diagnostic: remembers the bytecode hash of every vertex shader created (same keying as the pixel
/// shader registry: by shader pointer, which is unique across devices). Records, then returns the original's result.
unsafe extern "system" fn h_create_vertex_shader<const S: usize>(this: Obj, code: *const u8, len: usize, linkage: Obj, out: *mut Obj) -> i32 {
    let f: CreatePsFn = unsafe { std::mem::transmute(orig(Method::CreateVertexShader, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, code, len, linkage, out) } };
    note_device_call(Method::CreateVertexShader, this);
    let hr = unsafe { f(this, code, len, linkage, out) };
    if hr >= 0 && !out.is_null() && !code.is_null() {
        let _ = std::panic::catch_unwind(|| unsafe {
            let sh = *out;
            if sh.is_null() || ignoring() {
                return;
            }
            let bytes = std::slice::from_raw_parts(code, len);
            shaders::vs_registry().record(sh as usize, bytes);
            crate::furmask::note_vs(sh as usize, bytes); // output signature, for the fur mask's uv
        });
    }
    hr
}

/// Compiles and creates a replacement pixel shader on `dev`.
unsafe fn create_substitute(dev: Obj, hlsl: &str, entry: &str) -> Result<usize, String> {
    let bytes = shaders::compile_ps_cached(hlsl, entry)?;
    unsafe {
        let create: CreatePsFn = std::mem::transmute(vfn(dev, vt::DEV_CREATE_PIXEL_SHADER));
        let mut out: Obj = std::ptr::null_mut();
        // Not recorded in the registry: it is our own shader.
        let was = ignoring();
        ignore_creations(true);
        let hr = create(dev, bytes.as_ptr(), bytes.len(), std::ptr::null_mut(), &mut out);
        ignore_creations(was);
        if hr < 0 || out.is_null() {
            return Err(format!("CreatePixelShader failed (hr={hr:#x})"));
        }
        Ok(out as usize)
    }
}

/// The substitute for the shader being bound, if one applies on the context's current device.
/// No lock is held across D3D calls.
unsafe fn substitute_for(ctx: Obj, ps: Obj) -> Option<usize> {
    let hash = shaders::registry().hash_of(ps as usize)?;
    if !output::substitute_allowed(hash, shaders::substitution_enabled(), output::mode()) {
        return None;
    }
    unsafe {
        let mut dev: Obj = std::ptr::null_mut();
        let get_device: unsafe extern "system" fn(Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CHILD_GET_DEVICE));
        get_device(ctx, &mut dev);
        if dev.is_null() {
            return None;
        }
        // The substitute is created on the device that owns the BINDING context (cross-device safe:
        // the cache is keyed by device and hash). A replaced shader is released here, outside the
        // registry lock; the context keeps its own reference while it is bound.
        let r = shaders::substitutes().get_or_create_with(dev as usize, hash, |hlsl, entry| create_substitute(dev, hlsl, entry), |p| release(p as Obj));
        release(dev);
        if r.is_some() && shaders::note_applied(hash) {
            log!("substitute applied {hash:08x}");
        }
        r
    }
}

unsafe extern "system" fn h_ps_set_shader<const S: usize>(this: Obj, ps: Obj, inst: *const Obj, n: u32) {
    let f: unsafe extern "system" fn(Obj, Obj, *const Obj, u32) = unsafe { std::mem::transmute(orig(Method::PsSetShader, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, ps, inst, n) } };
    note_call(Method::PsSetShader, this);
    if !ignoring() && recorder::game_context() == this as usize {
        crate::clampemu::note_ps_set();
    }
    crate::furmask::on_ps_set(this, ps);
    if crate::resolve::debug_level() == 2 && !ignoring() {
        FINAL_BOUND.store(shaders::registry().hash_of(ps as usize) == Some(output::FINAL_HASH), Relaxed);
    }
    // With the gate closed only the scRGB final-pass substitute may still run (see output.rs).
    crate::aa::on_ps_set(this, ps);
    crate::look::on_ps_set(this, ps);
    if !(shaders::substitution_enabled() || output::final_active()) || ps.is_null() || ignoring() || !is_immediate(this) {
        // Another shader (or none) replaces the substituted one: its extra inputs are not needed any more.
        if crate::datapath::kind() != crate::datapath::KIND_NONE && !ignoring() && recorder::game_context() == this as usize {
            crate::datapath::set_kind(crate::datapath::KIND_NONE);
        }
        return unsafe { f(this, ps, inst, n) };
    }
    let sub = std::panic::catch_unwind(|| unsafe { substitute_for(this, ps) }).ok().flatten();
    let chosen = shaders::choose(true, sub, ps as usize) as Obj;
    if recorder::game_context() == this as usize {
        let substituted = chosen as usize != ps as usize;
        let kind = match shaders::registry().hash_of(ps as usize) {
            Some(crate::hdr_shaders::LIGHT_ADJUST_HASH) if substituted => crate::datapath::KIND_LIGHT_ADJUST,
            Some(crate::hdr_shaders::BLOOM_THRESHOLD_HASH) if substituted => crate::datapath::KIND_BLOOM,
            Some(crate::bloomfix::COMBINE_HASH) => crate::datapath::KIND_COMBINE,
            _ => crate::datapath::KIND_NONE,
        };
        crate::datapath::set_kind(kind);
    }
    unsafe { f(this, chosen, inst, n) }
}

/// Forced anisotropic filtering / mip bias: the samplers the engine binds are swapped for modified copies of the world
/// ones (`aniso::translate`); everything else, the mod's own calls and a switched-off mod pass through unchanged.
unsafe extern "system" fn h_ps_set_samplers<const S: usize>(this: Obj, start: u32, num: u32, smp: *const Obj) {
    let f: unsafe extern "system" fn(Obj, u32, u32, *const Obj) = unsafe { std::mem::transmute(orig(Method::PsSetSamplers, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, start, num, smp) } };
    note_call(Method::PsSetSamplers, this);
    if !ignoring() && num != 0 && num <= 16 && !smp.is_null() {
        let mut out = [std::ptr::null_mut(); 16];
        let changed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { crate::aniso::translate(smp, num, &mut out) })).unwrap_or(false);
        if changed {
            return unsafe { f(this, start, num, out.as_ptr()) };
        }
    }
    unsafe { f(this, start, num, smp) }
}

// ---- copy format diagnostics (log only) ------------------------------------------------------

static SEEN_MISMATCH: Mutex<Vec<(&'static str, u32, u32)>> = Mutex::new(Vec::new());

/// Logs once per distinct (call, src, dst) when two textures of a copy differ in format. Only
/// runs after at least one texture was upgraded; it never changes the call.
fn check_copy(this: Obj, m: Method, dst: Obj, src: Obj) {
    note_call(m, this);
    if UPGRADED.load(Relaxed) == 0 || ignoring() || !is_immediate(this) {
        return;
    }
    let what = m.name();
    let _ = std::panic::catch_unwind(|| unsafe {
        let (Some(s), Some(d)) = (texture_format(src), texture_format(dst)) else { return };
        if s == d {
            return;
        }
        let Ok(mut seen) = SEEN_MISMATCH.lock() else { return };
        if seen.len() < 64 && !seen.contains(&(what, s, d)) {
            seen.push((what, s, d));
            log!("copy format mismatch: src={s} dst={d} ({what})");
        }
    });
}

unsafe extern "system" fn h_copy_resource<const S: usize>(this: Obj, dst: Obj, src: Obj) {
    let f: unsafe extern "system" fn(Obj, Obj, Obj) = unsafe { std::mem::transmute(orig(Method::CopyResource, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, dst, src) } };
    check_copy(this, Method::CopyResource, dst, src);
    unsafe { f(this, dst, src) }
}

unsafe extern "system" fn h_copy_subresource<const S: usize>(this: Obj, dst: Obj, dsub: u32, x: u32, y: u32, z: u32, src: Obj, ssub: u32, bx: *const c_void) {
    type F = unsafe extern "system" fn(Obj, Obj, u32, u32, u32, u32, Obj, u32, *const c_void);
    let f: F = unsafe { std::mem::transmute(orig(Method::CopySubresource, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, dst, dsub, x, y, z, src, ssub, bx) } };
    check_copy(this, Method::CopySubresource, dst, src);
    unsafe { f(this, dst, dsub, x, y, z, src, ssub, bx) }
}

unsafe extern "system" fn h_resolve<const S: usize>(this: Obj, dst: Obj, dsub: u32, src: Obj, ssub: u32, fmt: u32) {
    let f: unsafe extern "system" fn(Obj, Obj, u32, Obj, u32, u32) = unsafe { std::mem::transmute(orig(Method::ResolveSubresource, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, dst, dsub, src, ssub, fmt) } };
    check_copy(this, Method::ResolveSubresource, dst, src);
    unsafe { f(this, dst, dsub, src, ssub, fmt) }
}

// ---- DXGI: swapchain creation and resize --------------------------------------------------------

// Set while another overlay creates its own dummy device (see `ignore_creations`).
thread_local! {
    static IGNORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn ignoring() -> bool {
    IGNORE.with(|c| c.get())
}

/// While on, devices/swapchains created ON THIS THREAD are not hooked from (the dummy device is
/// created on the installing thread, so the game's own thread is unaffected). `hudhook` builds a dummy
/// device and swapchain when it installs; and our own resolve calls are not recorded.
pub fn ignore_creations(on: bool) {
    IGNORE.with(|c| c.set(on));
}

fn note_swapchain(sc: Obj) {
    if !ignoring() {
        if let Some((w, h)) = unsafe { swapchain_size(sc) } {
            HOOKS.set_screen(w, h);
        }
    }
}

type CreateSwapChainFn = unsafe extern "system" fn(Obj, Obj, *mut c_void, *mut Obj) -> i32;

/// Logs what a creation call returned (first 40 lines over all creation calls).
fn log_create(kind: &str, hr: i32, sc: Obj, dev: Obj, ctx: Obj, extra: &str) {
    if !CREATE_LOGS.take(40) {
        return;
    }
    let tid = unsafe { GetCurrentThreadId() };
    if hr >= 0 {
        log!("d3d: {kind} returned hr={hr:#x} swapchain={sc:p} device={dev:p} context={ctx:p} thread={tid} ignoring={}{extra}", ignoring());
    } else {
        log!("d3d: {kind} returned hr={hr:#x} (failed) thread={tid} ignoring={}{extra}", ignoring());
    }
}

unsafe extern "system" fn h_create_swap_chain<const S: usize>(this: Obj, device: Obj, desc: *mut c_void, out: *mut Obj) -> i32 {
    let f: CreateSwapChainFn = unsafe { std::mem::transmute(orig(Method::CreateSwapChain, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, device, desc, out) } };
    note_call(Method::CreateSwapChain, this);
    // The game's swapchain is decided by its WINDOW (output.rs), whatever the device argument is.
    // Output mode (scRGB or SDR) is decided in `output::create_swapchain`. Never unwinds.
    let d: Option<SwapChainDesc> = if desc.is_null() { None } else { Some(unsafe { *(desc as *const SwapChainDesc) }) };
    let is_game = !ignoring() && d.as_ref().is_some_and(|d| std::panic::catch_unwind(|| output::desc_is_game(d)).unwrap_or(false));
    let hr = match std::panic::catch_unwind(|| unsafe { output::create_swapchain(this, device, is_game, desc, out, f) }) {
        Ok(hr) => hr,
        Err(_) => {
            // A panic after a swapchain was already created: hand that one back (a second one on the
            // same window would fail and the first would leak); otherwise call the original once.
            let pending = output::take_pending();
            if pending != 0 && !out.is_null() {
                unsafe { *out = pending as Obj };
                0
            } else {
                unsafe { f(this, device, desc, out) }
            }
        }
    };
    let sc = if hr >= 0 && !out.is_null() { unsafe { *out } } else { std::ptr::null_mut() };
    if let Some(d) = &d {
        log_create("IDXGIFactory::CreateSwapChain", hr, sc, device, std::ptr::null_mut(), &format!(" window={:#x} {}x{} fmt={} effect={} game_window={is_game}", d.window, d.width, d.height, d.format, d.effect));
    } else {
        log_create("IDXGIFactory::CreateSwapChain", hr, sc, device, std::ptr::null_mut(), " (no desc)");
    }
    if hr >= 0 && !sc.is_null() && !ignoring() {
        let _ = std::panic::catch_unwind(|| unsafe {
            if is_game {
                // Hooked for the game's swapchain only (the scRGB path already did it before returning).
                let hwnd = d.map_or(0, |d| d.window);
                if output::hook_game_swapchain(sc, hwnd) {
                    note_swapchain(sc);
                }
            }
            // Whatever device the swapchain was created on: make sure its classes are hooked.
            let dev = as_d3d11_device(device);
            discover_device(dev);
            release(dev);
        });
    }
    hr
}

// ---- device creation detours -----------------------------------------------------------------

type CreateDeviceFn = unsafe extern "system" fn(Obj, u32, usize, u32, *const u32, u32, u32, *mut Obj, *mut u32, *mut Obj) -> i32;
type CreateDeviceAndSwapChainFn = unsafe extern "system" fn(Obj, u32, usize, u32, *const u32, u32, u32, *const c_void, *mut Obj, *mut Obj, *mut u32, *mut Obj) -> i32;

static CREATE: OnceLock<GenericDetour<CreateDeviceFn>> = OnceLock::new();
static CREATE_SC: OnceLock<GenericDetour<CreateDeviceAndSwapChainFn>> = OnceLock::new();

/// A creation call returned these objects (`device` / `ctx_out` / `sc` may be null: the call may not
/// have asked for them): hook the classes of the device, its immediate context and its DXGI factory.
/// A missing device is taken from the swapchain. False when there is no immediate context to hook
/// from or the call was made while ignoring.
unsafe fn track(how: &str, device: Obj, ctx_out: *mut Obj, sc: Obj) -> bool {
    if ignoring() {
        log!("d3d: {how}: objects created while ignoring (overlay install); not hooked from them");
        return false;
    }
    unsafe {
        let from_sc = if device.is_null() { device_from_swapchain(sc) } else { std::ptr::null_mut() };
        let device = if device.is_null() { from_sc } else { device };
        let mut ok = false;
        if device.is_null() {
            log!("d3d: {how}: no device to hook from (swapchain {sc:p})");
        } else {
            DEV_SEEN.store(device as usize, true);
            let given = if ctx_out.is_null() { std::ptr::null_mut() } else { *ctx_out };
            let ctx = if given.is_null() { immediate_context_of(device) } else { given };
            log!("d3d: {how}: device {device:p} context {ctx:p}");
            hook_device_class(device);
            if !ctx.is_null() {
                hook_context_class(ctx);
                ok = true;
            }
            hook_factory_of(device);
            if given.is_null() {
                release(ctx); // GetImmediateContext added a reference; the device keeps the context alive
            }
        }
        release(from_sc);
        note_swapchain(sc);
        ok
    }
}

unsafe extern "system" fn h_create_device(adapter: Obj, dtype: u32, sw: usize, flags: u32, fl: *const u32, nfl: u32, sdk: u32, dev: *mut Obj, outfl: *mut u32, ctx: *mut Obj) -> i32 {
    let hr = unsafe { CREATE.get().expect("detour set before enable").call(adapter, dtype, sw, flags, fl, nfl, sdk, dev, outfl, ctx) };
    let _ = std::panic::catch_unwind(|| unsafe {
        let device = if hr >= 0 && !dev.is_null() { *dev } else { std::ptr::null_mut() };
        let context = if hr >= 0 && !ctx.is_null() { *ctx } else { std::ptr::null_mut() };
        log_create("D3D11CreateDevice", hr, std::ptr::null_mut(), device, context, &format!(" flags={flags:#x} driver_type={dtype}"));
        if !device.is_null() {
            track("D3D11CreateDevice", device, ctx, std::ptr::null_mut());
        }
    });
    hr
}

static CDASC_LOGGED: AtomicUsize = AtomicUsize::new(0);

/// Last result of the original call and whether it was made (for the panic fallback).
struct CdascCall {
    called: AtomicBool,
    hr: AtomicI32,
}

/// Releases the objects a creation call returned (swapchain, immediate context, device) and nulls the
/// output pointers, so that nothing half-configured reaches the game.
unsafe fn release_outputs(sc: *mut Obj, dev: *mut Obj, ctx: *mut Obj) {
    unsafe {
        for out in [sc, ctx, dev] {
            if !out.is_null() {
                release(*out);
                *out = std::ptr::null_mut();
            }
        }
    }
}

/// The game's swapchain, now created (SDR or after a fallback): follow its `ResizeBuffers` for the
/// screen size, and take the size from its desc (the creation desc as the fallback).
unsafe fn track_game_swapchain(sc: Obj, d: &SwapChainDesc) {
    unsafe {
        if output::hook_game_swapchain(sc, d.window) {
            if let Some((w, h)) = output::screen_after_resize(swapchain_size(sc), (d.width, d.height)) {
                HOOKS.set_screen(w, h);
            }
        }
    }
}

/// Body of the `D3D11CreateDeviceAndSwapChain` detour. A swapchain created together with the device is
/// decided here with the same rule as the factory path: for the game's creation (decided by the WINDOW
/// of the desc) the call is made with a COPY of the desc as float16 flip (scRGB), everything that can
/// fail is checked before returning, and on any failure the returned objects are released and the call
/// is made once more with the game's own desc. `call` makes the original call with the given desc pointer.
unsafe fn create_device_and_swap_chain(call: &dyn Fn(*const c_void) -> i32, state: &CdascCall, desc: *const c_void, sc: *mut Obj, dev: *mut Obj, ctx: *mut Obj) -> i32 {
    let game_desc: Option<SwapChainDesc> = if desc.is_null() { None } else { Some(unsafe { *(desc as *const SwapChainDesc) }) };
    if CDASC_LOGGED.fetch_add(1, Relaxed) < 8 {
        match &game_desc {
            Some(d) => log!("d3d: D3D11CreateDeviceAndSwapChain called: swapchain {}, device out {}, window {:#x}, {}x{} fmt {} buffers {} effect {} windowed {}", if sc.is_null() { "not requested" } else { "requested" }, if dev.is_null() { "null" } else { "given" }, d.window, d.width, d.height, d.format, d.buffers, d.effect, d.windowed),
            None => log!("d3d: D3D11CreateDeviceAndSwapChain called without a swapchain desc (device only)"),
        }
    }
    let is_game_creation = !ignoring() && !sc.is_null() && game_desc.as_ref().is_some_and(output::desc_is_game);
    let plan = output::plan_device_swapchain(game_desc.as_ref(), !sc.is_null(), is_game_creation, |d| output::evaluate(d, true));
    let source = "D3D11CreateDeviceAndSwapChain";
    let first = |desc: *const c_void| {
        let hr = call(desc);
        state.hr.store(hr, Relaxed);
        state.called.store(true, Relaxed);
        hr
    };
    unsafe {
        let objs = || {
            let swapchain = if sc.is_null() { std::ptr::null_mut() } else { *sc };
            let device = if dev.is_null() { std::ptr::null_mut() } else { *dev };
            (swapchain, device)
        };
        let own = |hr: i32, d: &SwapChainDesc, track_sc: bool| {
            if hr >= 0 {
                let (swapchain, device) = objs();
                track(source, device, ctx, swapchain);
                if track_sc && !swapchain.is_null() {
                    track_game_swapchain(swapchain, d);
                }
            }
            hr
        };
        match plan {
            output::DevicePlan::PassThrough => {
                let hr = first(desc);
                own(hr, &game_desc.unwrap_or_else(|| std::mem::zeroed()), false)
            }
            output::DevicePlan::Sdr(why) => {
                let d = game_desc.expect("planned from a desc");
                output::record_sdr(d.window, &why);
                let hr = first(desc);
                own(hr, &d, true)
            }
            output::DevicePlan::Float { alt, why } => {
                let d = game_desc.expect("planned from a desc");
                let (mut alt, mut why) = (alt, why);
                loop {
                    let hr = first(&alt as *const SwapChainDesc as *const c_void);
                    let (swapchain, device) = if hr >= 0 { objs() } else { (std::ptr::null_mut(), std::ptr::null_mut()) };
                    // A caller that gave no device out pointer expects none back.
                    let device_missing = !dev.is_null() && device.is_null();
                    let problem = output::float_problem(hr, swapchain.is_null(), device_missing, || {
                        // The classes are hooked first: that installs the view hooks the float back buffer needs.
                        if !track(source, device, ctx, swapchain) {
                            return Err("the device could not be tracked (no immediate context)".to_string());
                        }
                        if !view_hooks_ok() {
                            return Err("the view-creation hooks are missing (the float back buffer would get no view)".to_string());
                        }
                        output::finish_float(swapchain)
                    });
                    match problem {
                        None => {
                            output::commit_scrgb(swapchain, d.window, &why);
                            if let Some((w, h)) = output::screen_after_resize(swapchain_size(swapchain), (d.width, d.height)) {
                                HOOKS.set_screen(w, h);
                            }
                            break hr;
                        }
                        Some(p) => {
                            // Nothing half-configured: everything the call returned is released (only
                            // function addresses were hooked from it, no object pointer is kept).
                            if hr >= 0 {
                                release_outputs(sc, dev, ctx);
                            }
                            // An HDR10 attempt gets exactly one scRGB retry before the SDR fallback.
                            if let Some((next, w)) = output::retry_after_failure(&d, &p, &why) {
                                (alt, why) = (next, w);
                                continue;
                            }
                            output::record_sdr(d.window, &format!("{p}; retrying with the game's own swapchain"));
                            let hr2 = first(desc);
                            break own(hr2, &d, true);
                        }
                    }
                }
            }
        }
    }
}

unsafe extern "system" fn h_create_device_and_swap_chain(adapter: Obj, dtype: u32, sw: usize, flags: u32, fl: *const u32, nfl: u32, sdk: u32, desc: *const c_void, sc: *mut Obj, dev: *mut Obj, outfl: *mut u32, ctx: *mut Obj) -> i32 {
    let call = |d: *const c_void| unsafe { CREATE_SC.get().expect("detour set before enable").call(adapter, dtype, sw, flags, fl, nfl, sdk, d, sc, dev, outfl, ctx) };
    let state = CdascCall { called: AtomicBool::new(false), hr: AtomicI32::new(0) };
    let hr = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { create_device_and_swap_chain(&call, &state, desc, sc, dev, ctx) })) {
        Ok(hr) => hr,
        // A panic before the original ran: make the call as the game asked. After it ran: hand back what it gave.
        Err(_) if state.called.load(Relaxed) => state.hr.load(Relaxed),
        Err(_) => call(desc),
    };
    // What the caller gets back (after a retry or a fallback, the final state).
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let rd = |p: *mut Obj| if hr >= 0 && !p.is_null() { *p } else { std::ptr::null_mut() };
        let window = if desc.is_null() { 0 } else { (*(desc as *const SwapChainDesc)).window };
        log_create("D3D11CreateDeviceAndSwapChain", hr, rd(sc), rd(dev), rd(ctx), &format!(" window={window:#x} flags={flags:#x} outputs: swapchain {} device {} context {}", if sc.is_null() { "not requested" } else { "requested" }, if dev.is_null() { "not requested" } else { "requested" }, if ctx.is_null() { "not requested" } else { "requested" }));
    }));
    hr
}

unsafe extern "system" fn h_resize_buffers<const S: usize>(this: Obj, count: u32, w: u32, h: u32, fmt: u32, flags: u32) -> i32 {
    let f: unsafe extern "system" fn(Obj, u32, u32, u32, u32, u32) -> i32 = unsafe { std::mem::transmute(orig(Method::ResizeBuffers, S)) };
    let Some(_g) = Reentry::enter() else { return unsafe { f(this, count, w, h, fmt, flags) } };
    note_call(Method::ResizeBuffers, this);
    let (rc, (c, ft, fl)) = std::panic::catch_unwind(|| output::resize_before(this, count, fmt, flags)).unwrap_or((output::ResizeCall { is_float: false, is_game: false }, (count, fmt, flags)));
    let hr = unsafe { f(this, c, w, h, ft, fl) };
    let _ = std::panic::catch_unwind(|| output::resize_after(this, &rc, hr, w, h));
    hr
}

fn hook<F: retour::Function + Copy>(name: &str, module: *mut c_void, cell: &OnceLock<GenericDetour<F>>, detour: F) -> bool {
    let cname = format!("{name}\0");
    let Some(addr) = (unsafe { GetProcAddress(module, cname.as_ptr()) }) else {
        log!("d3d: {name} not exported by d3d11.dll");
        return false;
    };
    let target: F = unsafe { F::from_ptr(addr as *const ()) };
    match unsafe { GenericDetour::new(target, detour) } {
        Ok(d) => {
            let _ = cell.set(d);
            match unsafe { cell.get().expect("just set").enable() } {
                Ok(()) => {
                    log!("d3d: {name} hooked at {:p}", addr as *const ());
                    true
                }
                Err(e) => {
                    log!("d3d: {name}: enable failed: {e}");
                    false
                }
            }
        }
        Err(e) => {
            log!("d3d: {name}: detour failed: {e}");
            false
        }
    }
}

/// Hooks the device-creation exports of `d3d11.dll` (loading it if the game has not yet); the function
/// bodies of the objects they return are hooked from `track`. True when at least one hook is active.
pub fn install() -> bool {
    let name: Vec<u16> = "d3d11.dll\0".encode_utf16().collect();
    let module = unsafe { LoadLibraryW(name.as_ptr()) };
    if module.is_null() {
        log!("d3d: could not load d3d11.dll");
        return false;
    }
    let a = hook("D3D11CreateDevice", module, &CREATE, h_create_device as CreateDeviceFn);
    let b = hook("D3D11CreateDeviceAndSwapChain", module, &CREATE_SC, h_create_device_and_swap_chain as CreateDeviceAndSwapChainFn);
    a || b
}

/// Sets the ignore flag for the scope (our own calls are not recorded or substituted) and restores
/// the previous value on drop.
pub(crate) struct IgnoreScope(bool);

impl IgnoreScope {
    pub fn new() -> Self {
        let was = ignoring();
        ignore_creations(true);
        IgnoreScope(was)
    }
}

impl Drop for IgnoreScope {
    fn drop(&mut self) {
        ignore_creations(self.0);
    }
}

// ---- state guard -----------------------------------------------------------------------------

const MAX_VIEWPORTS: usize = 16;

/// `D3D11_VIEWPORT`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

/// Does NOT save: hull/domain/compute shaders, class-instance arrays, PS SRVs in slots 2 and up
/// (the resolve does not touch them).
/// Saves all the pipeline state the resolve touches and restores it on drop (also on early return),
/// releasing every reference the Get calls took. Never panics: raw calls only. While it lives the
/// ignore flag is on, so our own `OMSetRenderTargets` / `Draw` / `PSSetShader` calls reach the
/// original functions unrecorded.
pub(crate) struct StateGuard {
    ctx: Obj,
    rtvs: [Obj; 8],
    dsv: Obj,
    viewports: [Viewport; MAX_VIEWPORTS],
    n_viewports: u32,
    scissors: [[i32; 4]; MAX_VIEWPORTS],
    n_scissors: u32,
    rasterizer: Obj,
    blend: Obj,
    blend_factor: [f32; 4],
    sample_mask: u32,
    depth_stencil: Obj,
    stencil_ref: u32,
    vs: Obj,
    ps: Obj,
    gs: Obj,
    layout: Obj,
    topology: u32,
    srv0: Obj,
    srv1: Obj,
    cb0: Obj,
    _ignore: IgnoreScope,
}

impl StateGuard {
    /// The depth-stencil view that was bound when the state was saved (borrowed: the guard owns the reference).
    pub fn dsv(&self) -> Obj {
        self.dsv
    }

    /// The depth-stencil state that was bound when the state was saved (null = the default state; borrowed).
    pub fn depth_stencil(&self) -> Obj {
        self.depth_stencil
    }

    /// The blend state that was bound when the state was saved (null = the default state; borrowed).
    pub fn blend(&self) -> Obj {
        self.blend
    }

    /// The rasterizer state that was bound when the state was saved (null = the default state; borrowed).
    pub fn rasterizer(&self) -> Obj {
        self.rasterizer
    }

    /// # Safety
    /// `ctx` must be a live `ID3D11DeviceContext`.
    pub unsafe fn save(ctx: Obj) -> StateGuard {
        let mut g = StateGuard {
            ctx,
            rtvs: [std::ptr::null_mut(); 8],
            dsv: std::ptr::null_mut(),
            viewports: [Viewport::default(); MAX_VIEWPORTS],
            n_viewports: MAX_VIEWPORTS as u32,
            scissors: [[0; 4]; MAX_VIEWPORTS],
            n_scissors: MAX_VIEWPORTS as u32,
            rasterizer: std::ptr::null_mut(),
            blend: std::ptr::null_mut(),
            blend_factor: [1.0; 4],
            sample_mask: 0xffff_ffff,
            depth_stencil: std::ptr::null_mut(),
            stencil_ref: 0,
            vs: std::ptr::null_mut(),
            ps: std::ptr::null_mut(),
            gs: std::ptr::null_mut(),
            layout: std::ptr::null_mut(),
            topology: 0,
            srv0: std::ptr::null_mut(),
            srv1: std::ptr::null_mut(),
            cb0: std::ptr::null_mut(),
            _ignore: IgnoreScope::new(),
        };
        unsafe {
            type F1 = unsafe extern "system" fn(Obj, *mut Obj);
            let f: unsafe extern "system" fn(Obj, u32, *mut Obj, *mut Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_RTS));
            f(ctx, 8, g.rtvs.as_mut_ptr(), &mut g.dsv);
            let f: unsafe extern "system" fn(Obj, *mut u32, *mut Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_VIEWPORTS));
            f(ctx, &mut g.n_viewports, g.viewports.as_mut_ptr());
            g.n_viewports = g.n_viewports.min(MAX_VIEWPORTS as u32);
            let f: unsafe extern "system" fn(Obj, *mut u32, *mut [i32; 4]) = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_SCISSORS));
            f(ctx, &mut g.n_scissors, g.scissors.as_mut_ptr());
            g.n_scissors = g.n_scissors.min(MAX_VIEWPORTS as u32);
            let f: F1 = std::mem::transmute(vfn(ctx, vt::CTX_RS_GET_STATE));
            f(ctx, &mut g.rasterizer);
            let f: unsafe extern "system" fn(Obj, *mut Obj, *mut f32, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_BLEND));
            f(ctx, &mut g.blend, g.blend_factor.as_mut_ptr(), &mut g.sample_mask);
            let f: unsafe extern "system" fn(Obj, *mut Obj, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_GET_DEPTH));
            f(ctx, &mut g.depth_stencil, &mut g.stencil_ref);
            type GetShader = unsafe extern "system" fn(Obj, *mut Obj, *mut Obj, *mut u32);
            let f: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_GET_SHADER));
            f(ctx, &mut g.vs, std::ptr::null_mut(), std::ptr::null_mut());
            let f: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SHADER));
            f(ctx, &mut g.ps, std::ptr::null_mut(), std::ptr::null_mut());
            let f: GetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_GET_SHADER));
            f(ctx, &mut g.gs, std::ptr::null_mut(), std::ptr::null_mut());
            let f: F1 = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_LAYOUT));
            f(ctx, &mut g.layout);
            let f: unsafe extern "system" fn(Obj, *mut u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_GET_TOPOLOGY));
            f(ctx, &mut g.topology);
            type GetSlot0 = unsafe extern "system" fn(Obj, u32, u32, *mut Obj);
            let f: GetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_SRVS));
            f(ctx, 0, 1, &mut g.srv0);
            f(ctx, 1, 1, &mut g.srv1);
            let f: GetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_GET_CBS));
            f(ctx, 0, 1, &mut g.cb0);
        }
        g
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let ctx = self.ctx;
        unsafe {
            let f: unsafe extern "system" fn(Obj, u32, *const Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_RENDER_TARGETS));
            f(ctx, 8, self.rtvs.as_ptr(), self.dsv);
            let f: unsafe extern "system" fn(Obj, u32, *const Viewport) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_VIEWPORTS));
            f(ctx, self.n_viewports, self.viewports.as_ptr());
            let f: unsafe extern "system" fn(Obj, u32, *const [i32; 4]) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_SCISSORS));
            f(ctx, self.n_scissors, self.scissors.as_ptr());
            let f: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_RS_SET_STATE));
            f(ctx, self.rasterizer);
            let f: unsafe extern "system" fn(Obj, Obj, *const f32, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_BLEND));
            f(ctx, self.blend, self.blend_factor.as_ptr(), self.sample_mask);
            let f: unsafe extern "system" fn(Obj, Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_OM_SET_DEPTH));
            f(ctx, self.depth_stencil, self.stencil_ref);
            type SetShader = unsafe extern "system" fn(Obj, Obj, *const Obj, u32);
            let f: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_VS_SET_SHADER));
            f(ctx, self.vs, std::ptr::null(), 0);
            let f: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SHADER));
            f(ctx, self.ps, std::ptr::null(), 0);
            let f: SetShader = std::mem::transmute(vfn(ctx, vt::CTX_GS_SET_SHADER));
            f(ctx, self.gs, std::ptr::null(), 0);
            let f: unsafe extern "system" fn(Obj, Obj) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_LAYOUT));
            f(ctx, self.layout);
            let f: unsafe extern "system" fn(Obj, u32) = std::mem::transmute(vfn(ctx, vt::CTX_IA_SET_TOPOLOGY));
            f(ctx, self.topology);
            type SetSlot0 = unsafe extern "system" fn(Obj, u32, u32, *const Obj);
            let f: SetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_SRVS));
            f(ctx, 0, 1, &self.srv0);
            f(ctx, 1, 1, &self.srv1);
            let f: SetSlot0 = std::mem::transmute(vfn(ctx, vt::CTX_PS_SET_CBS));
            f(ctx, 0, 1, &self.cb0);
            for o in self.rtvs.iter().copied().chain([self.dsv, self.rasterizer, self.blend, self.depth_stencil, self.vs, self.ps, self.gs, self.layout, self.srv0, self.srv1, self.cb0]) {
                release(o);
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_format_wins_over_resource_format() {
        assert_eq!(view_target_of(87, 91, 100, 50, 0), Target { w: 100, h: 50, fmt: 87 });
    }

    #[test]
    fn unknown_view_format_falls_back_to_resource() {
        assert_eq!(view_target_of(0, 45, 100, 50, 0).fmt, 45);
    }

    #[test]
    fn mip_halves_and_clamps_to_one() {
        assert_eq!(view_target_of(87, 87, 100, 50, 1), Target { w: 50, h: 25, fmt: 87 });
        assert_eq!(view_target_of(87, 87, 100, 50, 6), Target { w: 1, h: 1, fmt: 87 });
        assert_eq!(view_target_of(87, 87, 100, 50, 40), Target { w: 1, h: 1, fmt: 87 });
    }

    #[test]
    fn ignore_flag_is_per_thread() {
        ignore_creations(true);
        assert!(ignoring());
        assert!(!std::thread::spawn(ignoring).join().unwrap());
        ignore_creations(false);
        assert!(!ignoring());
    }

    #[test]
    fn every_method_and_slot_has_its_own_detour_function() {
        let mut addrs: Vec<usize> = ALL.iter().flat_map(|&m| (0..NSLOT).map(move |s| hook_addr(m, s))).collect();
        assert!(addrs.iter().all(|&a| a != 0));
        let n = addrs.len();
        addrs.sort();
        addrs.dedup();
        assert_eq!(addrs.len(), n, "two detours share an address");
    }
}
