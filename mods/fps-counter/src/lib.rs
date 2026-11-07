//! Example Desiege code mod: shows the frame rate in the top-right corner, just left of the
//! game's compass.
//!
//! It only uses the C API from `crates/desiege/include/desiege_mod.h` (mirrored below), the
//! same way a mod written in C or C++ would, and doesn't link against Desiege itself.

use std::{
    ffi::{c_char, c_void, CString},
    sync::atomic::{AtomicPtr, Ordering::Relaxed},
};

/// Mirror of `DesiegeApi` (version 1).
#[repr(C)]
pub struct Api {
    version: u32,
    size: u32,
    log: extern "C" fn(*const c_char),
    on_frame: extern "C" fn(extern "C" fn(*mut c_void), *mut c_void),
    draw_text: extern "C" fn(i32, i32, *const c_char, u32, i32, u32),
    screen_size: extern "C" fn(*mut i32, *mut i32),
    time_seconds: extern "C" fn() -> f64,
    ui_scale: extern "C" fn() -> f32,
}

const ALIGN_RIGHT: u32 = 2;
static API: AtomicPtr<Api> = AtomicPtr::new(std::ptr::null_mut());

/// Frame counting, owned by the render thread (the only thread that calls `frame`).
struct Counter {
    since: f64,
    frames: u32,
    label: CString,
}
static mut COUNTER: Option<Counter> = None;

#[no_mangle]
pub extern "C" fn desiege_mod_init(api: *const Api) -> i32 {
    let Some(a) = (unsafe { api.as_ref() }) else { return 1 };
    if a.version < 1 || (a.size as usize) < std::mem::size_of::<Api>() {
        return 2; // older Desiege than this mod needs
    }
    API.store(api as *mut Api, Relaxed);
    (a.on_frame)(frame, std::ptr::null_mut());
    (a.log)(c"fps-counter: hello from a Desiege code mod".as_ptr());
    0
}

extern "C" fn frame(_user: *mut c_void) {
    let Some(a) = (unsafe { API.load(Relaxed).as_ref() }) else { return };
    let now = (a.time_seconds)();
    // SAFETY: only ever touched here, on the render thread.
    let c = unsafe { (*std::ptr::addr_of_mut!(COUNTER)).get_or_insert_with(|| Counter { since: now, frames: 0, label: CString::new("…").unwrap() }) };
    c.frames += 1;
    if now - c.since >= 0.5 {
        let fps = c.frames as f64 / (now - c.since);
        c.label = CString::new(format!("{fps:.0} FPS")).unwrap();
        c.since = now;
        c.frames = 0;
    }
    let (mut w, mut _h) = (0, 0);
    (a.screen_size)(&mut w, &mut _h);
    // In the game's original UI pixels the compass is ~75 px wide at the top-right corner;
    // ui_scale says how big those pixels are on screen right now.
    let s = (a.ui_scale)();
    let size = (11.0 * s).round().max(12.0) as i32;
    let right = w - (80.0 * s).round() as i32;
    (a.draw_text)(right, (6.0 * s).round() as i32, c.label.as_ptr(), 0xFFE8_D8A0, size, ALIGN_RIGHT);
}
