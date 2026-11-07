//! Code mods: 32-bit DLLs from the launcher's `code-mods\` folder, using the C API in
//! `include/desiege_mod.h`. The launcher only puts a DLL there after the user explicitly
//! trusts it; Desiege itself loads whatever is in that folder.

use std::{
    ffi::{c_char, c_void, CStr},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Mutex, OnceLock,
    },
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::FreeLibrary,
    System::LibraryLoader::{GetProcAddress, LoadLibraryW},
};

pub const API_VERSION: u32 = 1;

/// Must match `DesiegeApi` in include/desiege_mod.h. Only ever append fields.
#[repr(C)]
pub struct Api {
    pub version: u32,
    pub size: u32,
    pub log: extern "C" fn(*const c_char),
    pub on_frame: extern "C" fn(FrameFn, *mut c_void),
    pub draw_text: extern "C" fn(i32, i32, *const c_char, u32, i32, u32),
    pub screen_size: extern "C" fn(*mut i32, *mut i32),
    pub time_seconds: extern "C" fn() -> f64,
    pub ui_scale: extern "C" fn() -> f32,
}
pub type FrameFn = extern "C" fn(*mut c_void);

static API: Api = Api {
    version: API_VERSION,
    size: std::mem::size_of::<Api>() as u32,
    log: api_log,
    on_frame: api_on_frame,
    draw_text: api_draw_text,
    screen_size: api_screen_size,
    time_seconds: api_time_seconds,
    ui_scale: api_ui_scale,
};

/// Registered frame callbacks: (fn, user).
static FRAME_FNS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
/// on_frame is only accepted while a mod's init runs.
static IN_INIT: AtomicBool = AtomicBool::new(false);

unsafe fn str_arg(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

extern "C" fn api_log(msg: *const c_char) {
    crate::log_line(format!("mod: {}", unsafe { str_arg(msg) }));
}

extern "C" fn api_on_frame(f: FrameFn, user: *mut c_void) {
    if !IN_INIT.load(Relaxed) {
        crate::log_line("mod: on_frame is only allowed from desiege_mod_init".into());
        return;
    }
    if let Ok(mut l) = FRAME_FNS.lock() {
        l.push((f as usize, user as usize));
    }
}

extern "C" fn api_draw_text(x: i32, y: i32, text: *const c_char, argb: u32, size: i32, align: u32) {
    let text: Vec<u16> = unsafe { str_arg(text) }.encode_utf16().collect();
    if !text.is_empty() {
        crate::scale::overlay::queue(crate::scale::overlay::Item { x, y, text, argb, size, align });
    }
}

extern "C" fn api_screen_size(w: *mut i32, h: *mut i32) {
    let (sw, sh) = crate::scale::gpu::screen();
    unsafe {
        if !w.is_null() { *w = sw as i32 }
        if !h.is_null() { *h = sh as i32 }
    }
}

extern "C" fn api_ui_scale() -> f32 {
    crate::scale::ui_scale_factor()
}

extern "C" fn api_time_seconds() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Load every DLL in `<launcher data>\code-mods` (this DLL lives in `<launcher data>\runtime`).
/// Runs once, outside the loader lock, before the game draws anything.
pub fn load_all() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe { load_dir() });
}

unsafe fn load_dir() {
    let Some(dir) = crate::self_dir().and_then(|d| Some(d.parent()?.join("code-mods"))) else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll"))).collect();
    paths.sort();
    for path in paths {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lib = LoadLibraryW(wide.as_ptr());
        if lib.is_null() {
            crate::log_line(format!("code mod {name}: could not load ({})", std::io::Error::last_os_error()));
            continue;
        }
        let Some(init) = GetProcAddress(lib, c"desiege_mod_init".as_ptr().cast()) else {
            crate::log_line(format!("code mod {name}: no desiege_mod_init export; unloaded"));
            FreeLibrary(lib);
            continue;
        };
        let init: extern "C" fn(*const Api) -> i32 = std::mem::transmute(init);
        let before = FRAME_FNS.lock().map(|l| l.len()).unwrap_or(0);
        IN_INIT.store(true, Relaxed);
        let rc = init(&API);
        IN_INIT.store(false, Relaxed);
        if rc == 0 {
            crate::log_line(format!("code mod {name}: loaded"));
        } else {
            crate::log_line(format!("code mod {name}: init returned {rc}; unloaded"));
            if let Ok(mut l) = FRAME_FNS.lock() {
                l.truncate(before); // drop callbacks it registered before failing
            }
            FreeLibrary(lib);
        }
    }
}

/// Run code mod frame callbacks (render thread, right before the frame is shown).
pub fn run_frame() {
    let fns = FRAME_FNS.lock().map(|l| l.clone()).unwrap_or_default();
    for (f, user) in fns {
        let f: FrameFn = unsafe { std::mem::transmute(f) };
        f(user as *mut c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_layout_matches_header() {
        // DesiegeApi in include/desiege_mod.h: 2 x u32, then 6 function pointers (32-bit).
        assert_eq!(std::mem::size_of::<Api>(), 2 * 4 + 6 * std::mem::size_of::<usize>());
        assert_eq!(std::mem::offset_of!(Api, log), 8);
        assert_eq!(std::mem::offset_of!(Api, time_seconds), 8 + 4 * std::mem::size_of::<usize>());
        let header = include_str!("../include/desiege_mod.h");
        assert!(header.contains(&format!("#define DESIEGE_API_VERSION {API_VERSION}")));
    }
}
