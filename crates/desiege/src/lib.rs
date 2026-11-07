//! desiege.dll: all in-process fixes for Dungeon Siege. Currently:
//! - VRAM cap: modern cards report >2 GB, which the game reads as negative (see VRAM_CAP)
//! - system_detail.gas redirect: adds the launch resolution without editing game files
//! - loads dxwrapper (D3D7 -> D3D9) so resolutions above 2048 px wide work
//! - logs DirectDraw7 / Direct3D7 calls and HRESULTs to `%LOCALAPPDATA%\Desiege\desiege.log`
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
    ffi::{c_char, c_void, CStr, CString},
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
        }
        let lib = LoadLibraryW(system_ddraw_path().as_ptr()) as usize;
        if lib != 0 {
            resolve_forwards(lib);
        }
        scale::install_input_hooks();
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

/// Declares the static holding a hook's original method pointer (signature is documentation;
/// `call!` repeats it when transmuting).
macro_rules! orig {
    ($name:ident : fn($($t:ty),*) -> $r:ty) => {
        static $name: AtomicUsize = AtomicUsize::new(0);
    };
}
macro_rules! call {
    ($o:ident as fn($($t:ty),*) -> $r:ty; $($a:expr),*) => {
        std::mem::transmute::<usize, unsafe extern "system" fn($($t),*) -> $r>($o.load(Relaxed))($($a),*)
    };
}

// ---------------------------------------------------------------- system_detail.gas redirect
//
// system_detail.gas maps VRAM tier x color depth to allowed resolutions, maxing at 1024x768;
// anything else triggers "requested display mode ... is unsupported". Rather than edit the
// game's file, the game's CreateFileA is hooked and opens of system_detail.gas are redirected
// to a copy in %LOCALAPPDATA%\Desiege with the command-line width x height added.

#[no_mangle]
pub unsafe extern "system" fn DllMain(module: P, reason: u32, _: P) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        SELF_MODULE.store(module as usize, Relaxed);
        // Only pointer writes here (loader lock): no LoadLibrary, no file I/O.
        let exe = GetModuleHandleW(std::ptr::null()) as usize;
        let k32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
        let target = proc_in(GetModuleHandleW(k32.as_ptr()) as usize, c"CreateFileA");
        hook_iat(exe, target, create_file_a as usize, &CREATE_FILE_A);
        let target = proc_in(GetModuleHandleW(k32.as_ptr()) as usize, c"FindFirstFileA");
        hook_iat(exe, target, find_first_file_a as usize, &FIND_FIRST_FILE_A);

        // Injected by desiege-loader (rather than loaded as the game's ddraw.dll): the game
        // already imported the system ddraw, so route its two imports through our exports.
        // As a drop-in ddraw.dll the system one isn't loaded yet and this finds nothing.
        let sys = GetModuleHandleW(system_ddraw_path().as_ptr()) as usize;
        if sys != 0 {
            static UNUSED: AtomicUsize = AtomicUsize::new(0);
            hook_iat(exe, proc_in(sys, c"DirectDrawCreateEx"), DirectDrawCreateEx as usize, &UNUSED);
            hook_iat(exe, proc_in(sys, c"DirectDrawEnumerateExA"), DirectDrawEnumerateExA as usize, &UNUSED);
        }
    }
    1
}

/// Adds `WxH` to the top 32-bit tier. None = already listed (or file layout unexpected).
// ponytail: only touches [64M32B], the tier any modern card lands in (with the VRAM cap),
// since the launcher always passes bpp=32.
fn add_resolution(gas: &str, w: u32, h: u32) -> Option<String> {
    let tier = gas.find("\t[64M32B]")?;
    let tier_end = gas[tier + 1..].find("\n\t[").map_or(gas.len(), |i| tier + 1 + i);
    let block = &gas[tier..tier_end];
    if block.contains(&format!("[{w}x{h}]")) {
        return None;
    }
    let res = block.find("[resolutions]")?;
    let open = tier + res + block[res..].find('{')? + 1;
    Some(format!("{}\n\t\t\t[{w}x{h}]\t{{ max_back_buffers = 2; }}{}", &gas[..open], &gas[open..]))
}

/// `width=`/`height=` from `key=value` items (command-line args or ini lines; spaces around
/// `=` allowed, as in DungeonSiege.ini). Last one wins, like the game's own parsing.
fn find_resolution<'a>(items: impl Iterator<Item = &'a str>) -> Option<(u32, u32)> {
    let (mut w, mut h) = (None, None);
    for item in items {
        let Some((k, v)) = item.split_once('=') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "width" => w = v.trim().parse().ok(),
            "height" => h = v.trim().parse().ok(),
            _ => {}
        }
    }
    Some((w?, h?))
}

/// The launcher passes width=/height= on the command line. A Steam launch may not, so fall
/// back to what the game itself will use: Documents\Dungeon Siege\DungeonSiege.ini.
// ponytail: assumes Documents is %USERPROFILE%\Documents; use SHGetKnownFolderPath if a user
// with a relocated Documents folder reports the "unsupported mode" warning on Steam launches.
fn launch_resolution() -> Option<(u32, u32)> {
    let args: Vec<String> = std::env::args().collect();
    find_resolution(args.iter().map(String::as_str)).or_else(|| {
        let ini = PathBuf::from(std::env::var_os("USERPROFILE")?).join(r"Documents\Dungeon Siege\DungeonSiege.ini");
        find_resolution(fs::read_to_string(ini).ok()?.lines())
    })
}

/// Sets `no_flip = false` for every GPU vendor. With no_flip (the 2002 setting for nVidia
/// and others) the game presents each frame by Blt-copying the whole back buffer to the front
/// buffer, which under D3D9 translation is a full-screen GPU->CPU->GPU round trip; that halved
/// the frame rate (55 -> 112 fps at 2560x1440). Flipping is free.
fn enable_flip(gas: &str) -> String {
    gas.lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("b no_flip") && t.contains("true") {
                l.replacen("true", "false", 1)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if gas.ends_with('\n') { "\n" } else { "" }
}

/// Writes the patched copy; None means "open the original".
fn patched_gas(original: &str) -> Option<CString> {
    let text = fs::read_to_string(original).ok()?;
    let with_res = launch_resolution().and_then(|(w, h)| add_resolution(&text, w, h)).unwrap_or_else(|| text.clone());
    let patched = enable_flip(&with_res);
    if patched == text {
        return None;
    }
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Desiege");
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("system_detail.gas");
    fs::write(&path, patched).ok()?;
    // ponytail: CreateFileA takes an ANSI path; a non-ASCII profile path just falls back to
    // the original file (warning comes back, nothing breaks).
    CString::new(path.to_str().filter(|s| s.is_ascii())?).ok()
}

orig!(CREATE_FILE_A: fn(*const c_char, u32, u32, P, u32, u32, P) -> P);

unsafe extern "system" fn create_file_a(
    name: *const c_char, access: u32, share: u32, sa: P, disposition: u32, flags: u32, template: P,
) -> P {
    let mut name = name;
    if !name.is_null() && CStr::from_ptr(name).to_bytes().to_ascii_lowercase().ends_with(b"system_detail.gas") {
        static REDIRECT: OnceLock<Option<CString>> = OnceLock::new();
        let original = CStr::from_ptr(name).to_string_lossy().into_owned();
        let redirect = REDIRECT.get_or_init(|| patched_gas(&original));
        log!("CreateFileA({original}) -> {}", redirect.as_ref().map_or("original".into(), |p| p.to_string_lossy()));
        if let Some(p) = redirect {
            name = p.as_ptr();
        }
    } else if !name.is_null() {
        // ponytail: logs every file the game opens (it opens few: tanks are memory-mapped).
        // Useful while mapping out mod loading; gate it behind a setting if it gets noisy.
        log!("CreateFileA({})", cstr(name));
    }
    call!(CREATE_FILE_A as fn(*const c_char, u32, u32, P, u32, u32, P) -> P;
        name, access, share, sa, disposition, flags, template)
}

orig!(FIND_FIRST_FILE_A: fn(*const c_char, P) -> P);

/// Logged to see which folders the game scans for .dsres / .dsmod / .dsmap tanks.
unsafe extern "system" fn find_first_file_a(pattern: *const c_char, data: P) -> P {
    let h = call!(FIND_FIRST_FILE_A as fn(*const c_char, P) -> P; pattern, data);
    log!("FindFirstFileA({}) -> {}", cstr(pattern), if h as isize == -1 { "none" } else { "found" });
    h
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
    scale::hook_ddraw7(dd);
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
        scale::adjust_create_surface(copy.as_mut_ptr().cast())
    };
    // The copy equals the game's desc unless scale adjusted it; always pass it.
    let desc = if desc.is_null() { desc } else { copy.as_mut_ptr().cast() };
    let hr = call!(DD_CREATE_SURFACE as fn(P, *mut c_void, *mut P, P) -> HRESULT; this, desc, out, outer);
    if main && hr >= 0 {
        scale::created_main_surface(*out);
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
        scale::game_window(hwnd);
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
    scale::dynvb::hook_d3d7(d3d);
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
        scale::created_device(*out, surf);
    }
    hr
}

type ZCb = unsafe extern "system" fn(*mut c_void, P) -> HRESULT;
unsafe extern "system" fn z_cb(pf: *mut c_void, ctx: P) -> HRESULT {
    let c = &*(ctx as *const EnumCtx<ZCb>);
    // DDPIXELFORMAT: dwFlags@4 dwZBufferBitDepth@12
    log!("  zbuffer flags=0x{:X} bits={}", rd(pf, 4), rd(pf, 12));
    (c.cb)(pf, c.ctx)
}

unsafe extern "system" fn d3d_enum_zbuf(this: P, dev: *const Guid, cb: ZCb, ctx: P) -> HRESULT {
    log!("IDirect3D7::EnumZBufferFormats({})", G(dev));
    let mut c = EnumCtx { cb, ctx };
    let hr = call!(D3D_ENUM_ZBUF as fn(P, *const Guid, ZCb, P) -> HRESULT; this, dev, z_cb, &mut c as *mut _ as P);
    log!("IDirect3D7::EnumZBufferFormats -> {}", Hr(hr));
    hr
}

// After the macros above: macro_rules are only visible to modules declared after them.
mod scale;
mod profile;
mod code_mods;

#[cfg(test)]
mod tests {
    use super::{add_resolution, find_resolution};

    #[test]
    fn finds_resolution_in_args_and_ini() {
        let args = ["C:\\game\\DungeonSiege.exe", "width=2560", "height=1440", "bpp=32"];
        assert_eq!(find_resolution(args.into_iter()), Some((2560, 1440)));
        let ini = "driver_description = Primary\nwidth = 1920\nheight = 1080\n[multiplayer]\ngun_server = x";
        assert_eq!(find_resolution(ini.lines()), Some((1920, 1080)));
        assert_eq!(find_resolution(["nointro=true"].into_iter()), None);
    }

    #[test]
    fn enables_flip() {
        let gas = "\t[vendor*]\n\t{\n\t\tvendor\t= \"nVidia\";\n\t\tb no_flip\t\t\t\t\t= true;\n\t\tb buffers_frames\t= true;\n\t}\n";
        let out = super::enable_flip(gas);
        assert_eq!(out, gas.replace("no_flip\t\t\t\t\t= true", "no_flip\t\t\t\t\t= false"));
        assert_eq!(super::enable_flip(&out), out);
    }

    #[test]
    fn adds_resolution_to_top_tier_once() {
        let gas = "[system_detail]\n{\n\t[32M16B]\n\t{\n\t\t[resolutions]\n\t\t{\n\t\t\t[800x600]\t{ max_back_buffers = 2; }\n\t\t}\n\t}\n\t[64M32B]\n\t{\n\t\tdetail\t\t\t= 4;\n\t\t[resolutions]\n\t\t{\n\t\t\t[1024x768]\t{ max_back_buffers = 2; }\n\t\t}\n\t}\n\t[64M16B]\n\t{\n\t}\n}\n";
        let out = add_resolution(gas, 2560, 1440).unwrap();
        let tier = &out[out.find("[64M32B]").unwrap()..out.find("[64M16B]").unwrap()];
        assert!(tier.contains("\t\t{\n\t\t\t[2560x1440]\t{ max_back_buffers = 2; }\n\t\t\t[1024x768]"));
        assert_eq!(out.matches("2560x1440").count(), 1);
        assert_eq!(add_resolution(&out, 2560, 1440), None);
        assert_eq!(add_resolution(gas, 1024, 768), None);
    }
}
