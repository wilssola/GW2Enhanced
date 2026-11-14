//! The 78 `wtsapi32.dll` exports, each forwarded to the real copy in System32.
//!
//! We are loaded under the module name `wtsapi32.dll`, so EVERY module of the process that imports from
//! it gets this DLL, not only the exe. `libcef.dll` imports `WTSFreeMemory` and
//! `WTSQuerySessionInformationW`; missing exports made the game abort with
//! `Failed to load CEF DLL. Error code: 0x7f`. So all exports of the real DLL are present, and the
//! export list is checked by `tools/check-exports/check_exports.py`.
//!
//! Each export is a stub `jmp [TABLE + 8*i]` (see `global_asm!` below), so rcx/rdx/r8/r9, xmm0-3, the
//! stack arguments and the return value are untouched whatever the signature. A table entry starts at a
//! per-export thunk that resolves the real function lazily (first call, never under the loader lock),
//! stores it in the table and jumps to it. If the real DLL or the symbol is missing the entry becomes
//! `ret_zero` (returns 0), never a crash. The exact names are exported by `build.rs`.
//!
//! Never `version.dll` (GW2 Translated), `msimg32.dll` (GW2 Remastered) or
//! `dxgi.dll`/`d3d11.dll` (ReShade, arcdps, Nexus).

use std::arch::global_asm;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

/// Calls `$cb!` with `index name` pairs. One list feeds the name table, the stubs and `build.rs`
/// (build.rs parses this file: keep one `index name` pair per line).
macro_rules! with_exports {
    ($cb:ident) => {
        $cb! {
            0 EXPERIMENTAL_WTSCreateHeadlessSession
            1 EXPERIMENTAL_WTSStopHeadlessSession
            2 IsInteractiveUserSession
            3 QueryActiveSession
            4 QueryUserToken
            5 RegisterUsertokenForNoWinlogon
            6 WTSActiveSessionExists
            7 WTSCloseServer
            8 WTSCloudAuthClose
            9 WTSCloudAuthConvertAssertionToSerializedUserCredential
            10 WTSCloudAuthDuplicateSerializedUserCredential
            11 WTSCloudAuthGetServerNonce
            12 WTSCloudAuthNetworkLogonWithSerializedCredential
            13 WTSCloudAuthOpen
            14 WTSConnectSessionA
            15 WTSConnectSessionW
            16 WTSCreateListenerA
            17 WTSCreateListenerW
            18 WTSDisconnectSession
            19 WTSEnableChildSessions
            20 WTSEnumerateListenersA
            21 WTSEnumerateListenersW
            22 WTSEnumerateProcessesA
            23 WTSEnumerateProcessesExA
            24 WTSEnumerateProcessesExW
            25 WTSEnumerateProcessesW
            26 WTSEnumerateServersA
            27 WTSEnumerateServersW
            28 WTSEnumerateSessionsA
            29 WTSEnumerateSessionsExA
            30 WTSEnumerateSessionsExW
            31 WTSEnumerateSessionsW
            32 WTSFreeMemory
            33 WTSFreeMemoryExA
            34 WTSFreeMemoryExW
            35 WTSGetChildSessionId
            36 WTSGetListenerSecurityA
            37 WTSGetListenerSecurityW
            38 WTSIsChildSessionsEnabled
            39 WTSLogoffSession
            40 WTSOpenServerA
            41 WTSOpenServerExA
            42 WTSOpenServerExW
            43 WTSOpenServerW
            44 WTSQueryListenerConfigA
            45 WTSQueryListenerConfigW
            46 WTSQuerySessionInformationA
            47 WTSQuerySessionInformationW
            48 WTSQueryUserConfigA
            49 WTSQueryUserConfigW
            50 WTSQueryUserToken
            51 WTSRegisterSessionNotification
            52 WTSRegisterSessionNotificationEx
            53 WTSSendMessageA
            54 WTSSendMessageW
            55 WTSSetListenerSecurityA
            56 WTSSetListenerSecurityW
            57 WTSSetRenderHint
            58 WTSSetSessionInformationA
            59 WTSSetSessionInformationW
            60 WTSSetUserConfigA
            61 WTSSetUserConfigW
            62 WTSShutdownSystem
            63 WTSStartRemoteControlSessionA
            64 WTSStartRemoteControlSessionW
            65 WTSStopRemoteControlSession
            66 WTSTerminateProcess
            67 WTSUnRegisterSessionNotification
            68 WTSUnRegisterSessionNotificationEx
            69 WTSVirtualChannelClose
            70 WTSVirtualChannelOpen
            71 WTSVirtualChannelOpenEx
            72 WTSVirtualChannelPurgeInput
            73 WTSVirtualChannelPurgeOutput
            74 WTSVirtualChannelQuery
            75 WTSVirtualChannelRead
            76 WTSVirtualChannelWrite
            77 WTSWaitSystemEvent
        }
    };
}

macro_rules! names_array {
    ($($i:literal $n:ident)*) => { [$(stringify!($n)),*] };
}

pub const EXPORTS: [&str; 78] = with_exports!(names_array);

macro_rules! stubs {
    ($($i:literal $n:ident)*) => {
        global_asm!(
            ".text",
            $( concat!(".globl ", stringify!($n)), concat!(stringify!($n), ":"),
               concat!("    jmp qword ptr [rip + GW2G_WTS_TABLE + ", $i, "*8]"),
               concat!("gw2g_wts_thunk_", $i, ":"),
               concat!("    mov r10d, ", $i),
               "    jmp gw2g_wts_resolve",
            )*
            // Common lazy path: entered by a jump, so the stack is as at the original call
            // (rsp = 8 mod 16). Save the argument registers, call resolve(index), restore, jump.
            "gw2g_wts_resolve:",
            "    push rcx",
            "    push rdx",
            "    push r8",
            "    push r9",
            "    sub rsp, 104",          // 32 shadow + 4*16 xmm + 8 padding: rsp = 0 mod 16 at the call
            "    movups [rsp + 32], xmm0",
            "    movups [rsp + 48], xmm1",
            "    movups [rsp + 64], xmm2",
            "    movups [rsp + 80], xmm3",
            "    mov rcx, r10",
            "    call {resolve}",
            "    movups xmm0, [rsp + 32]",
            "    movups xmm1, [rsp + 48]",
            "    movups xmm2, [rsp + 64]",
            "    movups xmm3, [rsp + 80]",
            "    add rsp, 104",
            "    pop r9",
            "    pop r8",
            "    pop rdx",
            "    pop rcx",
            "    jmp rax",
            ".data",
            ".p2align 3",
            ".globl GW2G_WTS_TABLE",
            "GW2G_WTS_TABLE:",
            $( concat!("    .quad gw2g_wts_thunk_", $i), )*
            ".text",
            resolve = sym resolve,
        );
    };
}

with_exports!(stubs);

unsafe extern "C" {
    /// One entry per export, defined in the `global_asm!` above.
    static mut GW2G_WTS_TABLE: [usize; 78];
}

/// Stand-in when the real function cannot be found: returns 0 (FALSE / NULL / S_OK-less) in rax.
extern "C" fn ret_zero() -> usize {
    0
}

/// `<System32>\wtsapi32.dll`, NUL-terminated, from `GetSystemDirectoryW` (never from the search order).
pub fn system_dll_path() -> Vec<u16> {
    let mut buf = [0u16; 260];
    let n = (unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize).min(buf.len());
    let mut path: Vec<u16> = buf[..n].to_vec();
    path.extend(r"\wtsapi32.dll".encode_utf16());
    path.push(0);
    path
}

fn real_module() -> HMODULE {
    static REAL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *REAL.get_or_init(|| unsafe { LoadLibraryW(system_dll_path().as_ptr()) as usize }) as HMODULE
}

/// Address of the real export `name`, or `ret_zero` when it does not exist.
fn real_proc(name: &str) -> usize {
    let m = real_module();
    if !m.is_null() {
        let mut c = name.as_bytes().to_vec();
        c.push(0);
        if let Some(f) = unsafe { GetProcAddress(m, c.as_ptr()) } {
            return f as usize;
        }
    }
    ret_zero as *const () as usize
}

/// Called by the thunk on the first call of export `idx`. Must keep the "C" ABI of the asm above.
extern "C" fn resolve(idx: usize) -> usize {
    let p = real_proc(EXPORTS[idx]);
    // Racing threads store the same value.
    unsafe { (&raw mut GW2G_WTS_TABLE as *mut usize).add(idx).write_volatile(p) };
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn export_list_has_78_unique_names() {
        assert_eq!(EXPORTS.len(), 78);
        let set: HashSet<_> = EXPORTS.iter().collect();
        assert_eq!(set.len(), 78);
        assert!(EXPORTS.iter().all(|n| !n.is_empty()));
    }

    #[test]
    fn export_list_covers_known_consumers() {
        // the exe imports the first two, libcef.dll all four
        for n in [
            "WTSRegisterSessionNotification",
            "WTSUnRegisterSessionNotification",
            "WTSFreeMemory",
            "WTSQuerySessionInformationW",
        ] {
            assert!(EXPORTS.contains(&n), "missing {n}");
        }
    }

    #[test]
    fn system_path_is_built_from_the_system_directory() {
        let p = String::from_utf16(&system_dll_path()).unwrap().trim_end_matches('\0').to_lowercase();
        assert!(p.ends_with(r"\system32\wtsapi32.dll") || p.ends_with(r"\syswow64\wtsapi32.dll"), "{p}");
        let literal = ["C:", r"\Windows"].concat();
        assert_eq!(include_str!("proxy.rs").matches(literal.as_str()).count(), 0, "hard-coded windows directory");
    }
}
