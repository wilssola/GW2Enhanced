//! Pure logic of the function-body interception (no D3D, tested on the host).
//!
//! The hooks are inline detours of the native function bodies (`d3d.rs`), so they fire for every object
//! of the class whatever its pointer or COM identity. This module holds the method table (name,
//! vtable slot the address is resolved from), the decision "which slot / inline or vtable fallback",
//! the text of the diagnostics and the re-entrancy guard.

use std::cell::Cell;

use crate::d3d::vt;

/// Detour slots per method: a second class of objects (a wrapper layer next to the native one) gets its own.
pub const NSLOT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Method {
    OmSet,
    OmSetUav,
    Dispatch,
    Draw,
    DrawIndexed,
    DrawInstanced,
    DrawIndexedInstanced,
    PsSetShader,
    CopyResource,
    CopySubresource,
    ResolveSubresource,
    CreateTexture2d,
    CreateRtv,
    CreateSrv,
    CreatePixelShader,
    CreateSwapChain,
    ResizeBuffers,
    CreateVertexShader,
    PsSetSamplers,
}

pub const NMETH: usize = 19;

pub const ALL: [Method; NMETH] = [
    Method::OmSet,
    Method::OmSetUav,
    Method::Dispatch,
    Method::Draw,
    Method::DrawIndexed,
    Method::DrawInstanced,
    Method::DrawIndexedInstanced,
    Method::PsSetShader,
    Method::CopyResource,
    Method::CopySubresource,
    Method::ResolveSubresource,
    Method::CreateTexture2d,
    Method::CreateRtv,
    Method::CreateSrv,
    Method::CreatePixelShader,
    Method::CreateSwapChain,
    Method::ResizeBuffers,
    Method::CreateVertexShader,
    Method::PsSetSamplers,
];

/// The object class a method belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Context,
    Device,
    Factory,
    SwapChain,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::OmSet => "OMSetRenderTargets",
            Method::OmSetUav => "OMSetRenderTargetsAndUnorderedAccessViews",
            Method::Dispatch => "Dispatch",
            Method::Draw => "Draw",
            Method::DrawIndexed => "DrawIndexed",
            Method::DrawInstanced => "DrawInstanced",
            Method::DrawIndexedInstanced => "DrawIndexedInstanced",
            Method::PsSetShader => "PSSetShader",
            Method::CopyResource => "CopyResource",
            Method::CopySubresource => "CopySubresourceRegion",
            Method::ResolveSubresource => "ResolveSubresource",
            Method::CreateTexture2d => "CreateTexture2D",
            Method::CreateRtv => "CreateRenderTargetView",
            Method::CreateSrv => "CreateShaderResourceView",
            Method::CreatePixelShader => "CreatePixelShader",
            Method::CreateSwapChain => "CreateSwapChain",
            Method::ResizeBuffers => "ResizeBuffers",
            Method::CreateVertexShader => "CreateVertexShader",
            Method::PsSetSamplers => "PSSetSamplers",
        }
    }

    /// Vtable slot the function address is resolved from.
    pub fn index(self) -> usize {
        match self {
            Method::OmSet => vt::CTX_OM_SET_RENDER_TARGETS,
            Method::OmSetUav => vt::CTX_OM_SET_RTS_AND_UAVS,
            Method::Dispatch => vt::CTX_DISPATCH,
            Method::Draw => vt::CTX_DRAW,
            Method::DrawIndexed => vt::CTX_DRAW_INDEXED,
            Method::DrawInstanced => vt::CTX_DRAW_INSTANCED,
            Method::DrawIndexedInstanced => vt::CTX_DRAW_INDEXED_INSTANCED,
            Method::PsSetShader => vt::CTX_PS_SET_SHADER,
            Method::CopyResource => vt::CTX_COPY_RESOURCE,
            Method::CopySubresource => vt::CTX_COPY_SUBRESOURCE_REGION,
            Method::ResolveSubresource => vt::CTX_RESOLVE_SUBRESOURCE,
            Method::CreateTexture2d => vt::DEV_CREATE_TEXTURE2D,
            Method::CreateRtv => vt::DEV_CREATE_RTV,
            Method::CreateSrv => vt::DEV_CREATE_SRV,
            Method::CreatePixelShader => vt::DEV_CREATE_PIXEL_SHADER,
            Method::CreateSwapChain => vt::FACTORY_CREATE_SWAP_CHAIN,
            Method::ResizeBuffers => vt::SWAPCHAIN_RESIZE_BUFFERS,
            Method::CreateVertexShader => vt::DEV_CREATE_VERTEX_SHADER,
            Method::PsSetSamplers => vt::CTX_PS_SET_SAMPLERS,
        }
    }

    pub fn class(self) -> Class {
        match self {
            Method::OmSet
            | Method::OmSetUav
            | Method::Dispatch
            | Method::Draw
            | Method::DrawIndexed
            | Method::DrawInstanced
            | Method::DrawIndexedInstanced
            | Method::PsSetShader
            | Method::PsSetSamplers
            | Method::CopyResource
            | Method::CopySubresource
            | Method::ResolveSubresource => Class::Context,
            Method::CreateTexture2d | Method::CreateRtv | Method::CreateSrv | Method::CreatePixelShader | Method::CreateVertexShader => Class::Device,
            Method::CreateSwapChain => Class::Factory,
            Method::ResizeBuffers => Class::SwapChain,
        }
    }

    pub fn is_draw(self) -> bool {
        matches!(self, Method::Draw | Method::DrawIndexed | Method::DrawInstanced | Method::DrawIndexedInstanced)
    }
}

/// What to do with the function address resolved for a method.
#[derive(Debug, PartialEq, Eq)]
pub enum SlotPlan {
    /// The vtable entry is null: nothing to hook.
    NullAddress,
    /// This address is already hooked for the method (or the entry is already our hook): nothing to do.
    AlreadyHooked,
    /// Hook it in `slot`; `inline` false when another method already owns this body (identical code
    /// folded by the linker): only the vtable slot of this method can be patched then.
    Hook { slot: usize, inline: bool },
    /// Both slots of the method are taken by other addresses.
    NoFreeSlot,
}

/// `own`: addresses already hooked for this method (0 = free slot; our own hook addresses count as
/// hooked). `others`: addresses hooked for every other method.
pub fn plan_slot(addr: usize, own: [usize; NSLOT], others: &[usize]) -> SlotPlan {
    if addr == 0 {
        return SlotPlan::NullAddress;
    }
    if own.contains(&addr) {
        return SlotPlan::AlreadyHooked;
    }
    match own.iter().position(|&a| a == 0) {
        Some(slot) => SlotPlan::Hook { slot, inline: !others.contains(&addr) },
        None => SlotPlan::NoFreeSlot,
    }
}

/// How a method ended up hooked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookHow {
    Inline,
    Vtable,
    Failed,
}

/// `inline`: result of the inline attempt (`None` = not attempted). The vtable patch is tried only when
/// the inline hook did not take; `vtable` runs lazily.
pub fn decide_how(inline: Option<Result<(), String>>, vtable: impl FnOnce() -> bool) -> HookHow {
    if matches!(inline, Some(Ok(()))) {
        HookHow::Inline
    } else if vtable() {
        HookHow::Vtable
    } else {
        HookHow::Failed
    }
}

/// Codes stored per slot: 0 not hooked, 1 inline, 2 vtable.
pub fn how_code(h: HookHow) -> u8 {
    match h {
        HookHow::Inline => 1,
        HookHow::Vtable => 2,
        HookHow::Failed => 0,
    }
}

/// Text for the diagnostics from the per-slot codes.
pub fn how_text(codes: [u8; NSLOT]) -> &'static str {
    let (inline, vtable) = (codes.contains(&1), codes.contains(&2));
    match (inline, vtable) {
        (false, false) => "none",
        (true, false) => "inline",
        (false, true) => "vtable",
        (true, true) => "inline+vtable",
    }
}

// ---- re-entrancy guard -------------------------------------------------------------------------

thread_local! {
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Held while a hook runs (including its call of the original): a hooked function called by another
/// hooked one (a wrapper layer above the native object, our own resolve) is passed through unrecorded.
pub struct Reentry(());

impl Reentry {
    /// None when a hook is already running on this thread.
    pub fn enter() -> Option<Reentry> {
        IN_HOOK.with(|c| if c.replace(true) { None } else { Some(Reentry(())) })
    }
}

impl Drop for Reentry {
    fn drop(&mut self) {
        IN_HOOK.with(|c| c.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table_matches_the_spec_slots() {
        let want = [
            (Method::OmSet, 33),
            (Method::OmSetUav, 34),
            (Method::Draw, 13),
            (Method::DrawIndexed, 12),
            (Method::DrawInstanced, 21),
            (Method::DrawIndexedInstanced, 20),
            (Method::Dispatch, 41),
            (Method::PsSetShader, 9),
            (Method::CopyResource, 47),
            (Method::CopySubresource, 46),
            (Method::ResolveSubresource, 57),
            (Method::CreateTexture2d, 5),
            (Method::CreateRtv, 9),
            (Method::CreateSrv, 7),
            (Method::CreatePixelShader, 15),
            (Method::CreateSwapChain, 10),
            (Method::ResizeBuffers, 13),
            (Method::CreateVertexShader, 12),
            (Method::PsSetSamplers, 10),
        ];
        assert_eq!(want.len(), NMETH);
        for (m, idx) in want {
            assert_eq!(m.index(), idx, "{}", m.name());
        }
    }

    #[test]
    fn every_method_is_in_all_once_at_its_own_position() {
        for (i, m) in ALL.iter().enumerate() {
            assert_eq!(*m as usize, i, "{}", m.name());
        }
        let mut names: Vec<_> = ALL.iter().map(|m| m.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), NMETH);
    }

    #[test]
    fn classes_and_draw_flag() {
        assert_eq!(Method::OmSet.class(), Class::Context);
        assert_eq!(Method::CreatePixelShader.class(), Class::Device);
        assert_eq!(Method::CreateSwapChain.class(), Class::Factory);
        assert_eq!(Method::ResizeBuffers.class(), Class::SwapChain);
        assert_eq!(ALL.iter().filter(|m| m.is_draw()).count(), 4);
        assert!(!Method::Dispatch.is_draw());
    }

    #[test]
    fn slot_plan_first_address_takes_slot_zero() {
        assert_eq!(plan_slot(0x1000, [0, 0], &[]), SlotPlan::Hook { slot: 0, inline: true });
    }

    #[test]
    fn slot_plan_same_address_again_is_already_hooked() {
        assert_eq!(plan_slot(0x1000, [0x1000, 0], &[]), SlotPlan::AlreadyHooked);
        assert_eq!(plan_slot(0x1000, [0x2000, 0x1000], &[]), SlotPlan::AlreadyHooked);
    }

    #[test]
    fn slot_plan_second_class_takes_slot_one() {
        assert_eq!(plan_slot(0x2000, [0x1000, 0], &[]), SlotPlan::Hook { slot: 1, inline: true });
    }

    #[test]
    fn slot_plan_third_class_has_no_slot() {
        assert_eq!(plan_slot(0x3000, [0x1000, 0x2000], &[]), SlotPlan::NoFreeSlot);
    }

    #[test]
    fn slot_plan_body_shared_with_another_method_is_not_hooked_inline() {
        // Identical code folded: a second detour on the same body would hook both methods.
        assert_eq!(plan_slot(0x1000, [0, 0], &[0x5000, 0x1000]), SlotPlan::Hook { slot: 0, inline: false });
    }

    #[test]
    fn slot_plan_null_entry() {
        assert_eq!(plan_slot(0, [0, 0], &[]), SlotPlan::NullAddress);
    }

    #[test]
    fn inline_success_never_touches_the_vtable() {
        let how = decide_how(Some(Ok(())), || panic!("vtable must not be tried"));
        assert_eq!(how, HookHow::Inline);
    }

    #[test]
    fn inline_failure_falls_back_to_the_vtable() {
        assert_eq!(decide_how(Some(Err("x".into())), || true), HookHow::Vtable);
        assert_eq!(decide_how(Some(Err("x".into())), || false), HookHow::Failed);
    }

    #[test]
    fn not_attempted_inline_goes_to_the_vtable() {
        assert_eq!(decide_how(None, || true), HookHow::Vtable);
        assert_eq!(decide_how(None, || false), HookHow::Failed);
    }

    #[test]
    fn how_text_of_the_slots() {
        assert_eq!(how_text([0, 0]), "none");
        assert_eq!(how_text([1, 0]), "inline");
        assert_eq!(how_text([0, 2]), "vtable");
        assert_eq!(how_text([1, 2]), "inline+vtable");
        assert_eq!(how_code(HookHow::Inline), 1);
        assert_eq!(how_code(HookHow::Vtable), 2);
        assert_eq!(how_code(HookHow::Failed), 0);
    }

    #[test]
    fn reentry_guard_is_exclusive_per_thread_and_released_on_drop() {
        let g = Reentry::enter();
        assert!(g.is_some());
        assert!(Reentry::enter().is_none());
        assert!(std::thread::spawn(|| Reentry::enter().is_some()).join().unwrap());
        drop(g);
        assert!(Reentry::enter().is_some());
    }

    #[test]
    fn reentry_guard_is_released_by_a_panic() {
        let r = std::panic::catch_unwind(|| {
            let _g = Reentry::enter().unwrap();
            panic!("hook body");
        });
        assert!(r.is_err());
        assert!(Reentry::enter().is_some());
    }
}
