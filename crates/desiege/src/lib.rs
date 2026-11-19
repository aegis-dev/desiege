//! desiege.dll: all in-process fixes for Dungeon Siege. This file: the DLL entry, the ddraw
//! exports, vtable/import patching helpers and the IDirectDraw7 / IDirect3D7 hooks (VRAM cap,
//! display modes, device creation); it loads dxwrapper (D3D7 -> D3D9) so resolutions above
//! 2048 px wide work, and logs to `%LOCALAPPDATA%\Desiege\desiege.log`.
//!
//! Interface hooks, which dispatch the game's calls to the features:
//! - `surface`: IDirectDrawSurface7 (Blt, Lock, Flip) of the primary chain
//! - `device`: IDirect3DDevice7 (viewports, clears, draws, render targets, textures)
//! - `system_detail`: CreateFileA, serving a patched copy of the hardware table
//!
//! Features: `scale` (resolution decoupling: UI scale, borderless) with `input` (mouse and
//! window metrics), `shadow` (GPU character shadows), `cursor` (cursor overlay), `dynvb`
//! (dynamic vertex buffer mirrors), `gfx` (graphics options), `overlay` + `code_mods` (code mods).
//! Shared: `gpu` (our own surfaces, quads, state save/restore), `debug` (stats, trace,
//! snapshots, experiments), `profile` (sampling profiler).
//!
//! Two ways in:
//! - injected by `desiege-loader.exe` (game folder untouched); DllMain then reroutes the
//!   game's ddraw imports to the exports below, or
//! - via the `ddraw.dll` stub (crates/ddraw-stub) in the game folder, which forwards the game's
//!   ddraw calls here; works when the game is started from Steam.
//!
//! It exports the full ddraw API and forwards to the system ddraw.dll. The game imports only
//! DirectDrawCreateEx and DirectDrawEnumerateExA (hooked below), but d3dim700.dll (loaded by
//! ddraw for D3D7) imports DDInternalLock & co. *by module name*, which in stub mode resolves
//! to the stub and then here. Those are forwarded with signature-agnostic jmp thunks.
// function_casts_as_integer: vtable slots are plain usize; casting hook fns into them is the point.
#![allow(non_snake_case, clippy::missing_safety_doc, function_casts_as_integer)]

use std::{
    ffi::{c_char, c_void, CStr},
    fmt,
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Mutex, OnceLock,
    },
};
use windows_sys::Win32::System::{
    LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LoadLibraryW},
    Memory::{VirtualProtect, PAGE_READWRITE},
    SystemInformation::GetSystemDirectoryW,
};

type HRESULT = i32;
type P = *mut c_void;
const E_FAIL: HRESULT = 0x80004005u32 as i32;

// ---------------------------------------------------------------- logging

fn log_line(s: String) {
    static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
    let file = LOG.get_or_init(|| {
        // Never in the game folder: %LOCALAPPDATA%\Desiege\desiege.log
        let dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Desiege"));
        let file = dir.and_then(|d| fs::create_dir_all(&d).ok().map(|_| d.join("desiege.log")));
        Mutex::new(file.and_then(|p| File::create(p).ok()))
    });
    if let Ok(mut f) = file.lock() {
        if let Some(f) = f.as_mut() {
            let _ = writeln!(f, "{s}");
            let _ = f.flush(); // game may crash right after; don't lose the last line
        }
    }
}
macro_rules! log { ($($a:tt)*) => { $crate::log_line(format!($($a)*)) } }

struct Hr(HRESULT);
impl fmt::Display for Hr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0 >= 0 { write!(f, "OK") } else { write!(f, "FAIL 0x{:08X}", self.0 as u32) }
    }
}

#[repr(C)]
pub struct Guid { d1: u32, d2: u16, d3: u16, d4: [u8; 8] }
struct G(*const Guid);
impl fmt::Display for G {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Some(g) = (unsafe { self.0.as_ref() }) else { return write!(f, "NULL") };
        write!(f, "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-", g.d1, g.d2, g.d3, g.d4[0], g.d4[1])?;
        g.d4[2..].iter().try_for_each(|b| write!(f, "{b:02X}"))
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() { "NULL".into() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
}
unsafe fn rd(p: *const c_void, off: usize) -> u32 { *(p as *const u8).add(off).cast::<u32>() }

// ---------------------------------------------------------------- real ddraw.dll

unsafe fn real_proc(name: &CStr) -> usize {
    static LIB: OnceLock<usize> = OnceLock::new();
    let lib = *LIB.get_or_init(|| {
        if let Some(cap) = FPS_CAP_LIFTED.get() {
            log!("GOG exe: lifted its hard-coded 60 fps cap (maxfps now {})", if *cap > 0.0 { cap.to_string() } else { "unlimited".into() });
        }
        let game = GAME_DDRAW.load(Relaxed);
        if game != 0 && GetModuleHandleW(system_ddraw_path().as_ptr()) as usize != game {
            log!("game imported {}; bypassing it (routing to system ddraw)", module_path(game));
        }
        // Windows' own D3D7 runtime can't create a device on render targets wider than 2048 px
        // (see examples/rtsize.rs), so native-resolution play needs dxwrapper (D3D7 -> D3D9).
        // Path comes from DESIEGE_DXWRAPPER (set by the launcher) or, for Steam launches, the
        // `dxwrapper.path` file the launcher keeps next to this DLL. With `DdrawHookSystem32 = 1`
        // in its ini, dxwrapper inline-hooks the *system* ddraw.dll, so we load it first and then
        // keep forwarding to system ddraw as usual. No path: plain system ddraw.
        let from_file = || fs::read_to_string(self_dir()?.join("dxwrapper.path")).ok().map(|s| s.trim().into());
        if let Some(p) = std::env::var_os("DESIEGE_DXWRAPPER").or_else(from_file) {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = p.encode_wide().chain([0]).collect();
            let ok = !LoadLibraryW(wide.as_ptr()).is_null();
            log!("dxwrapper: {} -> {}", p.to_string_lossy(), if ok { "loaded" } else { "FAILED (native D3D7, max 2048 px wide)" });
            if ok {
                d3d9::install();
            }
        }
        let lib = LoadLibraryW(system_ddraw_path().as_ptr()) as usize;
        if lib != 0 {
            resolve_forwards(lib);
        }
        input::install_hooks();
        code_mods::load_all();
        lib
    });
    if lib == 0 {
        log!("!! could not load system ddraw.dll");
        return 0;
    }
    proc_in(lib, name)
}

/// This DLL's HMODULE, saved in DllMain.
static SELF_MODULE: AtomicUsize = AtomicUsize::new(0);

/// Folder this DLL was loaded from.
fn self_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(SELF_MODULE.load(Relaxed) as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    use std::os::windows::ffi::OsStringExt;
    (n > 0).then(|| PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])).parent().map(Into::into))?
}

/// Full path of the real ddraw.dll (System32, which WOW64 maps to SysWOW64), NUL-terminated.
unsafe fn system_ddraw_path() -> Vec<u16> {
    let mut buf = [0u16; 260];
    let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
    buf[..n].iter().copied().chain("\\ddraw.dll\0".encode_utf16()).collect()
}

unsafe fn proc_in(lib: usize, name: &CStr) -> usize {
    GetProcAddress(lib as _, name.as_ptr().cast()).map_or(0, |f| f as usize)
}

/// Exports passed straight through to the system ddraw.dll via `jmp [real]`.
// ponytail: the real pointers are filled when the game first calls one of our two hooked
// exports (that's what loads d3dim700, the only user of these). Calling a forwarder before
// that would jump to null; resolve in DllMain if anything ever needs that.
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
    DirectDrawEnumerateA, DirectDrawEnumerateExW, DirectDrawEnumerateW, DllCanUnloadNow,
    DllGetClassObject, GetDDSurfaceLocal, GetOLEThunkData, GetSurfaceFromDC, RegisterSpecialCase,
    ReleaseDDThreadLock, SetAppCompatData,
);

// ---------------------------------------------------------------- vtable patching

/// Replace slot `idx` of `obj`'s vtable with `hook`, saving the original in `orig`.
/// vtables are shared per class, so this hooks every instance; patching twice is a no-op.
unsafe fn patch(obj: P, idx: usize, hook: usize, orig: &AtomicUsize) {
    let slot = (*(obj as *mut *mut usize)).add(idx);
    if *slot == hook {
        return;
    }
    orig.store(*slot, Relaxed);
    write_ptr(slot, hook);
}

/// COM method `idx` of `obj`, as the function type `F` (read from the vtable each call, so a
/// method we hooked comes back as our hook).
unsafe fn method<F>(obj: P, idx: usize) -> F {
    std::mem::transmute_copy(&*(*(obj as *const *const usize)).add(idx))
}

unsafe fn write_ptr(slot: *mut usize, value: usize) {
    let mut old = 0;
    VirtualProtect(slot.cast(), 4, PAGE_READWRITE, &mut old);
    *slot = value;
    VirtualProtect(slot.cast(), 4, old, &mut old);
}

/// Point every import-address-table slot of `module` that holds `target` at `hook`.
unsafe fn hook_iat(module: usize, target: usize, hook: usize, orig: &AtomicUsize) -> bool {
    // PE32: optional header at e_lfanew+24, data directory at +96, IAT is entry 12.
    let pe = (module + rd(module as P, 0x3C) as usize) as P;
    let (rva, size) = (rd(pe, 24 + 96 + 12 * 8) as usize, rd(pe, 24 + 96 + 12 * 8 + 4) as usize);
    let mut hit = false;
    for i in 0..size / 4 {
        let slot = (module + rva + i * 4) as *mut usize;
        if target != 0 && *slot == target {
            orig.store(target, Relaxed);
            write_ptr(slot, hook);
            hit = true;
        }
    }
    hit
}

/// Point `module`'s import of `dll!func` at `hook`, found by name, saving whatever the slot
/// held (the real function, or an AppCompat shim's wrapper, which `hook_iat` can't match by
/// address: GOG registers a HIGHDPIAWARE layer that shims GetCursorPos & co.).
unsafe fn hook_import(module: usize, dll: &CStr, func: &CStr, hook: usize, orig: &AtomicUsize) -> bool {
    let pe = (module + rd(module as P, 0x3C) as usize) as P;
    let mut desc = module + rd(pe, 24 + 96 + 8) as usize;
    while rd(desc as P, 12) != 0 {
        let name = CStr::from_ptr((module + rd(desc as P, 12) as usize) as *const c_char);
        if name.to_bytes().eq_ignore_ascii_case(dll.to_bytes()) {
            let (names, slots) = (rd(desc as P, 0) as usize, rd(desc as P, 16) as usize);
            for i in 0.. {
                let thunk = rd((module + if names != 0 { names } else { slots }) as P, i * 4);
                if thunk == 0 {
                    break;
                }
                if thunk & 0x8000_0000 == 0 && CStr::from_ptr((module + thunk as usize + 2) as *const c_char) == func {
                    let slot = (module + slots + i * 4) as *mut usize;
                    if *slot != hook {
                        orig.store(*slot, Relaxed);
                        write_ptr(slot, hook);
                    }
                    return true;
                }
            }
        }
        desc += 20;
    }
    false
}

/// Declares the static holding a hook's original method pointer (signature is documentation;
/// `call!` repeats it when transmuting).
macro_rules! orig {
    ($vis:vis $name:ident : fn($($t:ty),*) -> $r:ty) => {
        $vis static $name: AtomicUsize = AtomicUsize::new(0);
    };
}
macro_rules! call {
    ($o:ident as fn($($t:ty),*) -> $r:ty; $($a:expr),*) => {
        std::mem::transmute::<usize, unsafe extern "system" fn($($t),*) -> $r>($o.load(Relaxed))($($a),*)
    };
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(module: P, reason: u32, _: P) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        SELF_MODULE.store(module as usize, Relaxed);
        // Only pointer writes here (loader lock): no LoadLibrary, no file I/O.
        let exe = GetModuleHandleW(std::ptr::null()) as usize;
        // Injected by desiege-loader (rather than loaded via our stub): the game already
        // imported a ddraw.dll, so route its two imports through our exports. That's the system
        // one on Steam, and GOG's own D3D7->D3D9 wrapper (game folder ddraw.dll) on GOG, which
        // we bypass this way: our exports go to system ddraw + dxwrapper, the tested path.
        // Our stub (it exports the marker) already forwards here; leave it alone.
        let name: Vec<u16> = "ddraw.dll\0".encode_utf16().collect();
        let imported = GetModuleHandleW(name.as_ptr()) as usize;
        let ours = imported == 0 || imported == module as usize || proc_in(imported, c"DESIEGE_STUB_MARKER") != 0;
        GAME_DDRAW.store(if ours { 0 } else { imported }, Relaxed);
        if vanilla() {
            // Measure-only: the game keeps its own ddraw (and system_detail.gas); we only count frames.
            if !ours {
                hook_iat(exe, proc_in(imported, c"DirectDrawCreateEx"), vanilla_create_ex as usize, &VANILLA_CREATE);
            }
            // vanilla,uncap: also lift GOG's 60 fps cap, to measure GOG's own renderer.
            if std::env::var("DESIEGE_EXPERIMENT").unwrap_or_default().split(',').any(|e| e.trim() == "uncap") {
                lift_gog_fps_cap(exe);
            }
            return 1;
        }
        lift_gog_fps_cap(exe);
        let k32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
        let target = proc_in(GetModuleHandleW(k32.as_ptr()) as usize, c"CreateFileA");
        hook_iat(exe, target, system_detail::create_file_a as usize, &system_detail::CREATE_FILE_A);
        let target = proc_in(GetModuleHandleW(k32.as_ptr()) as usize, c"FindFirstFileA");
        hook_iat(exe, target, system_detail::find_first_file_a as usize, &system_detail::FIND_FIRST_FILE_A);
        if !ours {
            static UNUSED: AtomicUsize = AtomicUsize::new(0);
            hook_iat(exe, proc_in(imported, c"DirectDrawCreateEx"), DirectDrawCreateEx as usize, &UNUSED);
            hook_iat(exe, proc_in(imported, c"DirectDrawEnumerateExA"), DirectDrawEnumerateExA as usize, &UNUSED);
        }
    }
    1
}

/// GOG's DungeonSiege.exe (1.11.1.1486, "AMD RDNA fix") replaces the read of the `maxfps`
/// setting with a hard-coded `mov dword [esi+0x90], 60.0` plus NOPs, so the game never renders
/// above 60 fps whatever the monitor. The Steam exe reads the setting (default: no cap). Put
/// that back: the cap becomes `maxfps=` from the command line, or none. Patched in memory only,
/// and only if these exact bytes are there.
const GOG_FPS_CAP_AT: usize = 0x416177;
const GOG_FPS_CAP: [u8; 24] = [
    0x8D, 0xBE, 0x90, 0, 0, 0, 0xC7, 0x07, 0x00, 0x00, 0x70, 0x42, // lea edi,[esi+90h]; mov [edi], 60.0f
    0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90,
];
static FPS_CAP_LIFTED: OnceLock<f32> = OnceLock::new();

unsafe fn lift_gog_fps_cap(exe: usize) {
    if exe != 0x400000 || std::slice::from_raw_parts(GOG_FPS_CAP_AT as *const u8, GOG_FPS_CAP.len()) != GOG_FPS_CAP {
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    let cap = args.iter().filter_map(|a| a.split_once('=')).filter(|(k, _)| k.eq_ignore_ascii_case("maxfps")).filter_map(|(_, v)| v.trim().parse().ok()).last().unwrap_or(0.0f32);
    let imm = (GOG_FPS_CAP_AT + 8) as *mut u8;
    let mut old = 0;
    VirtualProtect(imm.cast(), 4, PAGE_READWRITE, &mut old);
    std::ptr::copy_nonoverlapping(cap.to_le_bytes().as_ptr(), imm, 4);
    VirtualProtect(imm.cast(), 4, old, &mut old);
    let _ = FPS_CAP_LIFTED.set(cap);
}

/// The ddraw.dll the game imported, if it isn't us or our stub (logged on first ddraw call).
static GAME_DDRAW: AtomicUsize = AtomicUsize::new(0);

fn module_path(m: usize) -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(m as _, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

// ---------------------------------------------------------------- vanilla (measure-only) mode

/// DESIEGE_EXPERIMENT=vanilla: no fixes at all, the game runs on whatever ddraw it imported;
/// only the frame counter is hooked so "before" frame rates can be measured the same way.
fn vanilla() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var("DESIEGE_EXPERIMENT").unwrap_or_default().split(',').any(|e| e.trim() == "vanilla"))
}

static VANILLA_CREATE: AtomicUsize = AtomicUsize::new(0);
orig!(VANILLA_DD_QI: fn(P, *const Guid, *mut P) -> HRESULT);
orig!(VANILLA_D3D_CREATE_DEVICE: fn(P, *const Guid, P, *mut P) -> HRESULT);

unsafe extern "system" fn vanilla_create_ex(guid: *const Guid, out: *mut P, iid: *const Guid, outer: P) -> HRESULT {
    let hr = call!(VANILLA_CREATE as fn(*const Guid, *mut P, *const Guid, P) -> HRESULT; guid, out, iid, outer);
    log!("vanilla: DirectDrawCreateEx via {} -> {} (fps cap lifted: {:?})", module_path(GAME_DDRAW.load(Relaxed)), Hr(hr), FPS_CAP_LIFTED.get());
    if hr >= 0 {
        patch(*out, 0, vanilla_dd_qi as usize, &VANILLA_DD_QI);
    }
    hr
}

unsafe extern "system" fn vanilla_dd_qi(this: P, iid: *const Guid, out: *mut P) -> HRESULT {
    let hr = call!(VANILLA_DD_QI as fn(P, *const Guid, *mut P) -> HRESULT; this, iid, out);
    if hr >= 0 && iid.as_ref().is_some_and(is_iid_direct3d7) {
        patch(*out, 4, vanilla_create_device as usize, &VANILLA_D3D_CREATE_DEVICE);
    }
    hr
}

unsafe extern "system" fn vanilla_create_device(this: P, clsid: *const Guid, surf: P, out: *mut P) -> HRESULT {
    let hr = call!(VANILLA_D3D_CREATE_DEVICE as fn(P, *const Guid, P, *mut P) -> HRESULT; this, clsid, surf, out);
    log!("vanilla: IDirect3D7::CreateDevice({}) -> {}", G(clsid), Hr(hr));
    if hr >= 0 {
        device::measure(*out);
    }
    hr
}

// ---------------------------------------------------------------- exports

type EnumExA = unsafe extern "system" fn(*const Guid, *const c_char, *const c_char, P, P) -> i32;
struct EnumCtx<F> { cb: F, ctx: P }

unsafe extern "system" fn enum_ex_cb(g: *const Guid, desc: *const c_char, name: *const c_char, ctx: P, mon: P) -> i32 {
    let c = &*(ctx as *const EnumCtx<EnumExA>);
    log!("  driver guid={} desc='{}' name='{}' monitor={mon:?}", G(g), cstr(desc), cstr(name));
    (c.cb)(g, desc, name, c.ctx, mon)
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawEnumerateExA(cb: EnumExA, ctx: P, flags: u32) -> HRESULT {
    let f = real_proc(c"DirectDrawEnumerateExA");
    if f == 0 { return E_FAIL; }
    log!("DirectDrawEnumerateExA(flags=0x{flags:X})");
    let mut c = EnumCtx { cb, ctx };
    let f: unsafe extern "system" fn(EnumExA, P, u32) -> HRESULT = std::mem::transmute(f);
    let hr = f(enum_ex_cb, &mut c as *mut _ as P, flags);
    log!("DirectDrawEnumerateExA -> {}", Hr(hr));
    hr
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawCreateEx(guid: *const Guid, out: *mut P, iid: *const Guid, outer: P) -> HRESULT {
    let f = real_proc(c"DirectDrawCreateEx");
    if f == 0 { return E_FAIL; }
    let f: unsafe extern "system" fn(*const Guid, *mut P, *const Guid, P) -> HRESULT = std::mem::transmute(f);
    let hr = f(guid, out, iid, outer);
    log!("DirectDrawCreateEx(guid={}, iid={}) -> {}", G(guid), G(iid), Hr(hr));
    if hr >= 0 {
        hook_ddraw7(*out);
    }
    hr
}

// ---------------------------------------------------------------- IDirectDraw7

orig!(DD_QI: fn(P, *const Guid, *mut P) -> HRESULT);
orig!(DD_CREATE_SURFACE: fn(P, *mut c_void, *mut P, P) -> HRESULT);
orig!(DD_ENUM_MODES: fn(P, u32, *mut c_void, P, ModeCb) -> HRESULT);
orig!(DD_GET_CAPS: fn(P, *mut c_void, *mut c_void) -> HRESULT);
orig!(DD_SET_COOP: fn(P, P, u32) -> HRESULT);
orig!(DD_SET_MODE: fn(P, u32, u32, u32, u32, u32) -> HRESULT);
orig!(DD_VIDMEM: fn(P, *mut c_void, *mut u32, *mut u32) -> HRESULT);

unsafe fn hook_ddraw7(dd: P) {
    patch(dd, 0, dd_qi as usize, &DD_QI);
    patch(dd, 6, dd_create_surface as usize, &DD_CREATE_SURFACE);
    patch(dd, 8, dd_enum_modes as usize, &DD_ENUM_MODES);
    patch(dd, 11, dd_get_caps as usize, &DD_GET_CAPS);
    patch(dd, 20, dd_set_coop as usize, &DD_SET_COOP);
    patch(dd, 21, dd_set_mode as usize, &DD_SET_MODE);
    patch(dd, 23, dd_vidmem as usize, &DD_VIDMEM);
    patch(dd, 27, dd_get_device_identifier as usize, &DD_GET_ID);
    DDRAW.store(dd as usize, Relaxed);
    scale::hook_ddraw7(dd);
}

/// Latest IDirectDraw7 (the game recreates it on mode changes); our own textures (cursor,
/// overlay text, shadow targets) are created on it and die with it.
static DDRAW: AtomicUsize = AtomicUsize::new(0);

// DDDEVICEIDENTIFIER2: szDriver[512], szDescription[512], liDriverVersion u64, then
// dwVendorId @1032, dwDeviceId @1036. system_detail.gas keys per-vendor quirks off these.
orig!(DD_GET_ID: fn(P, *mut c_void, u32) -> HRESULT);
unsafe extern "system" fn dd_get_device_identifier(this: P, id: *mut c_void, flags: u32) -> HRESULT {
    let hr = call!(DD_GET_ID as fn(P, *mut c_void, u32) -> HRESULT; this, id, flags);
    if hr >= 0 && !id.is_null() {
        let desc = CStr::from_ptr((id as *const u8).add(512).cast()).to_string_lossy();
        log!("IDirectDraw7::GetDeviceIdentifier -> vendor=0x{:04X} device=0x{:04X} '{desc}'", rd(id, 1032), rd(id, 1036));
    }
    hr
}

/// {F5049E77-4861-11D2-A407-00A0C90629A8}
fn is_iid_direct3d7(g: &Guid) -> bool {
    g.d1 == 0xF5049E77 && g.d2 == 0x4861 && g.d3 == 0x11D2 && g.d4 == [0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8]
}

unsafe extern "system" fn dd_qi(this: P, iid: *const Guid, out: *mut P) -> HRESULT {
    let hr = call!(DD_QI as fn(P, *const Guid, *mut P) -> HRESULT; this, iid, out);
    log!("IDirectDraw7::QueryInterface({}) -> {}", G(iid), Hr(hr));
    if hr >= 0 && iid.as_ref().is_some_and(is_iid_direct3d7) {
        hook_d3d7(*out);
    }
    hr
}

static SURFACES_LOGGED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn dd_create_surface(this: P, desc: *mut c_void, out: *mut P, outer: P) -> HRESULT {
    // UI scale: render surfaces get created at native size (on a copy; the game's desc stays V).
    let mut copy = [0u8; 124];
    let main = !desc.is_null() && {
        std::ptr::copy_nonoverlapping(desc as *const u8, copy.as_mut_ptr(), 124);
        shadow::experiment_rt_textures(copy.as_mut_ptr().cast());
        scale::adjust_create_surface(copy.as_mut_ptr().cast())
    };
    // The copy equals the game's desc unless scale adjusted it; always pass it.
    let desc = if desc.is_null() { desc } else { copy.as_mut_ptr().cast() };
    let hr = call!(DD_CREATE_SURFACE as fn(P, *mut c_void, *mut P, P) -> HRESULT; this, desc, out, outer);
    if main && hr >= 0 {
        scale::created_main_surface(*out);
    }
    if hr >= 0 && !desc.is_null() {
        gfx::note_surface(desc);
    }
    // Textures get created by the thousand; log every failure but only the first 40 successes.
    if hr < 0 || SURFACES_LOGGED.fetch_add(1, Relaxed) < 40 {
        // DDSURFACEDESC2: dwFlags@4 dwHeight@8 dwWidth@12 dwBackBufferCount@20 ddsCaps.dwCaps@104
        log!(
            "IDirectDraw7::CreateSurface(flags=0x{:X} {}x{} backbuffers={} caps=0x{:X}) -> {}",
            rd(desc, 4), rd(desc, 12), rd(desc, 8), rd(desc, 20), rd(desc, 104), Hr(hr)
        );
    }
    hr
}

type ModeCb = unsafe extern "system" fn(*mut c_void, P) -> HRESULT;
struct ModeCtx { cb: ModeCb, ctx: P, inject: scale::ModeInject, stopped: bool }

unsafe extern "system" fn mode_cb(desc: *mut c_void, ctx: P) -> HRESULT {
    let c = &mut *(ctx as *mut ModeCtx);
    // dwRefreshRate@24, ddpfPixelFormat.dwRGBBitCount@84
    log!("  mode {}x{}x{} @{}Hz", rd(desc, 12), rd(desc, 8), rd(desc, 84), rd(desc, 24));
    c.inject.observe(desc);
    let r = (c.cb)(desc, c.ctx);
    c.stopped = r == 0; // DDENUMRET_CANCEL
    r
}

unsafe extern "system" fn dd_enum_modes(this: P, flags: u32, desc: *mut c_void, ctx: P, cb: ModeCb) -> HRESULT {
    log!("IDirectDraw7::EnumDisplayModes(flags=0x{flags:X})");
    let mut c = ModeCtx { cb, ctx, inject: scale::ModeInject::new(), stopped: false };
    let hr = call!(DD_ENUM_MODES as fn(P, u32, *mut c_void, P, ModeCb) -> HRESULT;
        this, flags, desc, &mut c as *mut _ as P, mode_cb);
    // UI scale: the game must see the virtual mode it was launched with, even if the monitor
    // doesn't offer it (we really set the native mode).
    if hr >= 0 && !c.stopped {
        if let Some(mut extra) = c.inject.missing() {
            log!("  mode {}x{}x32 (virtual, added for UI scale)", rd(extra.as_ptr().cast(), 12), rd(extra.as_ptr().cast(), 8));
            cb(extra.as_mut_ptr().cast(), ctx);
        }
    }
    log!("IDirectDraw7::EnumDisplayModes -> {}", Hr(hr));
    hr
}

/// Modern GPUs report >2 GB of video memory; the game reads it as a signed int, gets a
/// negative number, matches no VRAM tier in system_detail.gas and refuses to create the 3D
/// device at any resolution it doesn't have a fallback for.
// ponytail: i32::MAX leaves no headroom; if the game ever adds to this value before
// comparing, it wraps negative again. Drop to 1 GB (what dgVoodoo reports) if that shows up.
const VRAM_CAP: u32 = i32::MAX as u32;

unsafe fn cap_vram(p: *mut u32) {
    if !p.is_null() {
        *p = (*p).min(VRAM_CAP);
    }
}

unsafe extern "system" fn dd_get_caps(this: P, drv: *mut c_void, hel: *mut c_void) -> HRESULT {
    let hr = call!(DD_GET_CAPS as fn(P, *mut c_void, *mut c_void) -> HRESULT; this, drv, hel);
    if !drv.is_null() {
        // DDCAPS: dwCaps@4 dwCaps2@8 dwZBufferBitDepths@56 dwVidMemTotal@60 dwVidMemFree@64
        log!(
            "IDirectDraw7::GetCaps -> {} caps=0x{:X} caps2=0x{:X} zdepths=0x{:X} vidmem total={} free={} (capped to {VRAM_CAP})",
            Hr(hr), rd(drv, 4), rd(drv, 8), rd(drv, 56), rd(drv, 60), rd(drv, 64)
        );
    }
    if hr >= 0 {
        for caps in [drv, hel].into_iter().filter(|p| !p.is_null()) {
            cap_vram((caps as *mut u8).add(60).cast());
            cap_vram((caps as *mut u8).add(64).cast());
        }
    }
    hr
}

unsafe extern "system" fn dd_set_coop(this: P, hwnd: P, flags: u32) -> HRESULT {
    let hr = call!(DD_SET_COOP as fn(P, P, u32) -> HRESULT; this, hwnd, flags);
    log!("IDirectDraw7::SetCooperativeLevel(hwnd={hwnd:?}, flags=0x{flags:X}) -> {}", Hr(hr));
    const DDSCL_EXCLUSIVE: u32 = 0x10;
    if hr >= 0 && flags & DDSCL_EXCLUSIVE != 0 {
        input::game_window(hwnd);
    }
    hr
}

unsafe extern "system" fn dd_set_mode(this: P, w: u32, h: u32, bpp: u32, hz: u32, flags: u32) -> HRESULT {
    let (rw, rh) = scale::set_display_mode(w, h);
    let hr = call!(DD_SET_MODE as fn(P, u32, u32, u32, u32, u32) -> HRESULT; this, rw, rh, bpp, hz, flags);
    log!("IDirectDraw7::SetDisplayMode({w}x{h}x{bpp} @{hz}Hz flags=0x{flags:X}) real {rw}x{rh} -> {}", Hr(hr));
    hr
}

unsafe extern "system" fn dd_vidmem(this: P, caps: *mut c_void, total: *mut u32, free: *mut u32) -> HRESULT {
    let hr = call!(DD_VIDMEM as fn(P, *mut c_void, *mut u32, *mut u32) -> HRESULT; this, caps, total, free);
    let get = |p: *mut u32| if p.is_null() { None } else { Some(*p) };
    log!(
        "IDirectDraw7::GetAvailableVidMem(caps=0x{:X}) -> {} total={:?} free={:?} (capped to {VRAM_CAP})",
        if caps.is_null() { 0 } else { rd(caps, 0) }, Hr(hr), get(total), get(free)
    );
    if hr >= 0 {
        cap_vram(total);
        cap_vram(free);
    }
    hr
}

// ---------------------------------------------------------------- IDirect3D7

orig!(D3D_ENUM_DEVICES: fn(P, DevCb, P) -> HRESULT);
orig!(D3D_CREATE_DEVICE: fn(P, *const Guid, P, *mut P) -> HRESULT);
orig!(D3D_ENUM_ZBUF: fn(P, *const Guid, ZCb, P) -> HRESULT);

unsafe fn hook_d3d7(d3d: P) {
    patch(d3d, 3, d3d_enum_devices as usize, &D3D_ENUM_DEVICES);
    patch(d3d, 4, d3d_create_device as usize, &D3D_CREATE_DEVICE);
    patch(d3d, 6, d3d_enum_zbuf as usize, &D3D_ENUM_ZBUF);
    dynvb::hook_d3d7(d3d);
}

type DevCb = unsafe extern "system" fn(*const c_char, *const c_char, *mut c_void, P) -> HRESULT;
unsafe extern "system" fn dev_cb(desc: *const c_char, name: *const c_char, dd: *mut c_void, ctx: P) -> HRESULT {
    let c = &*(ctx as *const EnumCtx<DevCb>);
    log!("  d3d device name='{}' desc='{}'", cstr(name), cstr(desc));
    (c.cb)(desc, name, dd, c.ctx)
}

unsafe extern "system" fn d3d_enum_devices(this: P, cb: DevCb, ctx: P) -> HRESULT {
    log!("IDirect3D7::EnumDevices");
    let mut c = EnumCtx { cb, ctx };
    let hr = call!(D3D_ENUM_DEVICES as fn(P, DevCb, P) -> HRESULT; this, dev_cb, &mut c as *mut _ as P);
    log!("IDirect3D7::EnumDevices -> {}", Hr(hr));
    hr
}

unsafe extern "system" fn d3d_create_device(this: P, clsid: *const Guid, surf: P, out: *mut P) -> HRESULT {
    let hr = call!(D3D_CREATE_DEVICE as fn(P, *const Guid, P, *mut P) -> HRESULT; this, clsid, surf, out);
    log!("IDirect3D7::CreateDevice({}) -> {}", G(clsid), Hr(hr));
    if hr >= 0 {
        device::created(*out, surf);
        gfx::hook_device(*out);
    }
    hr
}

type ZCb = unsafe extern "system" fn(*mut c_void, P) -> HRESULT;
unsafe extern "system" fn z_collect(pf: *mut c_void, ctx: P) -> HRESULT {
    let mut f = [0u8; 32];
    std::ptr::copy_nonoverlapping(pf as *const u8, f.as_mut_ptr(), 32);
    (*(ctx as *mut Vec<[u8; 32]>)).push(f);
    1 // D3DENUMRET_OK
}

/// Collects the formats first so 16-bit depth can be hidden when a deeper one exists (gfx).
unsafe extern "system" fn d3d_enum_zbuf(this: P, dev: *const Guid, cb: ZCb, ctx: P) -> HRESULT {
    log!("IDirect3D7::EnumZBufferFormats({})", G(dev));
    let mut all: Vec<[u8; 32]> = Vec::new();
    let hr = call!(D3D_ENUM_ZBUF as fn(P, *const Guid, ZCb, P) -> HRESULT; this, dev, z_collect, &mut all as *mut _ as P);
    // DDPIXELFORMAT: dwFlags@4 dwZBufferBitDepth@12
    let deeper = all.iter().any(|f| rd(f.as_ptr().cast(), 12) >= 24);
    for f in &mut all {
        let keep = gfx::keep_zbuffer(f.as_ptr().cast(), deeper);
        log!("  zbuffer flags=0x{:X} bits={}{}", rd(f.as_ptr().cast(), 4), rd(f.as_ptr().cast(), 12), if keep { "" } else { " (hidden: hq_formats)" });
        if keep && cb(f.as_mut_ptr().cast(), ctx) == 0 {
            break;
        }
    }
    log!("IDirect3D7::EnumZBufferFormats -> {}", Hr(hr));
    hr
}

// After the macros above: macro_rules are only visible to modules declared after them.
// Interface hooks (entry points from the game's calls):
mod device;
mod surface;
mod system_detail;
mod d3d9; // the real D3D9 device under dxwrapper
mod ao; // ambient occlusion (D3D9 shaders)
mod lights; // the engine's dynamic lights (for per-pixel lighting)
// Features:
mod scale; // resolution decoupling (UI scale, borderless)
mod input; // mouse and window metrics for resolution decoupling
mod shadow; // GPU character shadows
mod cursor; // cursor overlay
mod dynvb; // dynamic vertex buffer mirrors
mod gfx; // graphics options
mod overlay; // code mods' overlay text
mod code_mods;
// Shared:
mod gpu; // our own textures and quads
mod debug;
mod profile;

