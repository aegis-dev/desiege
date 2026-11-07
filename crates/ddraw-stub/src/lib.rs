//! ddraw.dll stub for the game folder. The game loads it as its ddraw.dll (DLL search order
//! puts the exe's folder first); it loads desiege.dll and forwards every ddraw export there.
//! desiege.dll comes from DESIEGE_DLL if set, else the launcher's runtime folder
//! (%APPDATA%\dev.desiege.launcher\runtime\desiege.dll). If that's missing (launcher
//! uninstalled), it forwards to the system ddraw.dll, so the game still runs, unmodded.
#![allow(non_snake_case)]

use std::{ffi::CStr, os::windows::ffi::OsStrExt, path::PathBuf};
use windows_sys::Win32::System::{
    LibraryLoader::{GetProcAddress, LoadLibraryW},
    SystemInformation::GetSystemDirectoryW,
};

/// The launcher looks for these bytes to tell our stub from another mod's ddraw.dll.
/// Exported so the linker can't strip it.
#[no_mangle]
pub static DESIEGE_STUB_MARKER: [u8; 13] = *b"DESIEGE-STUB\0";

unsafe fn load(path: &std::ffi::OsStr) -> usize {
    let wide: Vec<u16> = path.encode_wide().chain([0]).collect();
    LoadLibraryW(wide.as_ptr()) as usize
}

fn desiege_dll() -> Option<PathBuf> {
    std::env::var_os("DESIEGE_DLL").map(Into::into).or_else(|| {
        Some(PathBuf::from(std::env::var_os("APPDATA")?).join(r"dev.desiege.launcher\runtime\desiege.dll"))
    })
}

unsafe fn system_ddraw() -> usize {
    let mut buf = [0u16; 260];
    let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
    let path: Vec<u16> = buf[..n].iter().copied().chain("\\ddraw.dll\0".encode_utf16()).collect();
    LoadLibraryW(path.as_ptr()) as usize
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(_: *mut std::ffi::c_void, reason: u32, _: *mut std::ffi::c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        // ponytail: LoadLibrary inside DllMain (under the loader lock) is discouraged but fine
        // here: desiege.dll's own DllMain only patches pointers, and the forwarders must be
        // resolved before the game's first ddraw call. If it ever deadlocks, resolve lazily.
        let lib = desiege_dll().map(|p| load(p.as_os_str())).filter(|&l| l != 0).unwrap_or_else(|| system_ddraw());
        resolve_forwards(lib);
    }
    1
}

unsafe fn proc_in(lib: usize, name: &CStr) -> usize {
    GetProcAddress(lib as _, name.as_ptr().cast()).map_or(0, |f| f as usize)
}

/// Every ddraw export, passed through via `jmp [real]` (signatures don't matter).
macro_rules! forward {
    ($($name:ident),* $(,)?) => {
        #[allow(non_upper_case_globals)]
        mod real { $(pub static mut $name: usize = 0;)* }
        unsafe fn resolve_forwards(lib: usize) {
            $( real::$name = proc_in(lib, CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()).unwrap()); )*
        }
        $(
            #[unsafe(naked)]
            #[no_mangle]
            pub unsafe extern "C" fn $name() {
                core::arch::naked_asm!("jmp dword ptr [{0}]", sym real::$name)
            }
        )*
    };
}
forward!(
    AcquireDDThreadLock, CompleteCreateSysmemSurface, D3DParseUnknownCommand, DDGetAttachedSurfaceLcl,
    DDInternalLock, DDInternalUnlock, DSoundHelp, DirectDrawCreate, DirectDrawCreateClipper,
    DirectDrawCreateEx, DirectDrawEnumerateA, DirectDrawEnumerateExA, DirectDrawEnumerateExW,
    DirectDrawEnumerateW, DllCanUnloadNow, DllGetClassObject, GetDDSurfaceLocal, GetOLEThunkData,
    GetSurfaceFromDC, RegisterSpecialCase, ReleaseDDThreadLock, SetAppCompatData,
);
