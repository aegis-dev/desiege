//! Mouse and window metrics for resolution decoupling: the game only ever sees V coordinates.

use crate::{debug::dbg_input, hook_import, scale::*, P};
use std::sync::{
    atomic::{AtomicUsize, Ordering::Relaxed},
    Mutex,
};
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{CallWindowProcA, SetWindowLongA, GWL_WNDPROC},
};

static GAME_HWND: AtomicUsize = AtomicUsize::new(0);
static OLD_WNDPROC: AtomicUsize = AtomicUsize::new(0);

/// Called from SetCooperativeLevel: subclass the game window to remap mouse messages.
pub unsafe fn game_window(hwnd: P) {
    if !decoupling() || hwnd.is_null() || GAME_HWND.swap(hwnd as usize, Relaxed) == hwnd as usize {
        return;
    }
    let old = SetWindowLongA(hwnd as _, GWL_WNDPROC, wndproc as usize as i32);
    OLD_WNDPROC.store(old as u32 as usize, Relaxed);
    log!("ui scale: hooked window {hwnd:?}");
}

unsafe extern "system" fn wndproc(hwnd: windows_sys::Win32::Foundation::HWND, msg: u32, wp: usize, lp: isize) -> isize {
    let mut lp = lp;
    // WM_MOUSEMOVE .. WM_MOUSEHWHEEL; wheel messages carry screen coords, == client here.
    if active() && (0x200..=0x20E).contains(&msg) {
        let (x, y) = xf().back(lp as i16 as i32, (lp >> 16) as i16 as i32);
        dbg_input(|| format!("wndproc msg=0x{msg:X} raw=({},{}) -> ({x},{y})", lp as i16, (lp >> 16) as i16));
        lp = ((y as u16 as isize) << 16) | (x as u16 as isize);
    }
    let old: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC = std::mem::transmute(OLD_WNDPROC.load(Relaxed));
    CallWindowProcA(old, hwnd, msg, wp, lp)
}

orig!(GET_CURSOR_POS: fn(*mut POINT) -> i32);
orig!(SET_CURSOR_POS: fn(i32, i32) -> i32);
orig!(GET_CLIENT_RECT: fn(P, *mut RECT) -> i32);
orig!(GET_SYSTEM_METRICS: fn(i32) -> i32);

/// The game reads the mouse as "distance from where I last put the cursor" and recenters it
/// every frame. Native movement is mapped to V movement with the fraction carried over, so
/// slow movements (under one V pixel per frame) still add up instead of being lost.
#[derive(Default)]
pub struct Mouse {
    /// Last SetCursorPos: V position and the native position we really set.
    anchor: Option<((i32, i32), (i32, i32))>,
    /// Fraction of a V pixel not yet delivered, carried into the next anchor.
    carry: (f32, f32),
    pending: (f32, f32),
}
impl Mouse {
    pub fn set(&mut self, v: (i32, i32), t: Xf) -> (i32, i32) {
        let n = (t.x(v.0), t.y(v.1));
        self.carry = self.pending;
        self.anchor = Some((v, n));
        n
    }
    pub fn get(&mut self, raw: (i32, i32), t: Xf) -> (i32, i32) {
        let Some((v, n)) = self.anchor else { return t.back(raw.0, raw.1) };
        let ex = (raw.0 - n.0) as f32 / t.s + self.carry.0;
        let ey = (raw.1 - n.1) as f32 / t.s + self.carry.1;
        self.pending = (ex - ex.trunc(), ey - ey.trunc());
        (v.0 + ex.trunc() as i32, v.1 + ey.trunc() as i32)
    }
}
static MOUSE: Mutex<Option<Mouse>> = Mutex::new(None);

unsafe extern "system" fn get_cursor_pos(p: *mut POINT) -> i32 {
    let ok = call!(GET_CURSOR_POS as fn(*mut POINT) -> i32; p);
    if ok != 0 && active() && !p.is_null() {
        let raw = ((*p).x, (*p).y);
        let (x, y) = MOUSE.lock().map_or(xf().back(raw.0, raw.1), |mut m| m.get_or_insert_with(Mouse::default).get(raw, xf()));
        dbg_input(|| format!("GetCursorPos raw={raw:?} -> ({x},{y})"));
        (*p).x = x;
        (*p).y = y;
    }
    ok
}

unsafe extern "system" fn set_cursor_pos(x: i32, y: i32) -> i32 {
    dbg_input(|| format!("SetCursorPos({x},{y})"));
    let (x, y) = if active() {
        MOUSE.lock().map_or((xf().x(x), xf().y(y)), |mut m| m.get_or_insert_with(Mouse::default).set((x, y), xf()))
    } else {
        (x, y)
    };
    call!(SET_CURSOR_POS as fn(i32, i32) -> i32; x, y)
}

unsafe extern "system" fn get_client_rect(hwnd: P, r: *mut RECT) -> i32 {
    let ok = call!(GET_CLIENT_RECT as fn(P, *mut RECT) -> i32; hwnd, r);
    if ok != 0 && active() && hwnd as usize == GAME_HWND.load(Relaxed) && !r.is_null() {
        let (w, h) = v();
        *r = RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
    }
    ok
}

unsafe extern "system" fn get_system_metrics(i: i32) -> i32 {
    const SM_CXSCREEN: i32 = 0; const SM_CYSCREEN: i32 = 1;
    match i {
        SM_CXSCREEN if active() => v().0 as i32,
        SM_CYSCREEN if active() => v().1 as i32,
        _ => call!(GET_SYSTEM_METRICS as fn(i32) -> i32; i),
    }
}

/// Patch the game's USER32 imports. Called outside DllMain (user32 is loaded by then).
pub unsafe fn install_hooks() {
    if !decoupling() {
        return;
    }
    let exe = GetModuleHandleW(std::ptr::null()) as usize;
    let hits = [
        hook_import(exe, c"user32.dll", c"GetCursorPos", get_cursor_pos as usize, &GET_CURSOR_POS),
        hook_import(exe, c"user32.dll", c"SetCursorPos", set_cursor_pos as usize, &SET_CURSOR_POS),
        hook_import(exe, c"user32.dll", c"GetClientRect", get_client_rect as usize, &GET_CLIENT_RECT),
        hook_import(exe, c"user32.dll", c"GetSystemMetrics", get_system_metrics as usize, &GET_SYSTEM_METRICS),
    ];
    if hits.contains(&false) {
        log!("!! ui scale: some input imports not found (GetCursorPos, SetCursorPos, GetClientRect, GetSystemMetrics): {hits:?}");
    }
    log!("ui scale: input hooks installed (native {:?})", native());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_deltas_keep_fractions() {
        let t = Xf { s: 2.0, ox: 0.0, oy: 0.0 };
        let mut m = Mouse::default();
        let mut total = 0;
        for _ in 0..10 {
            let n = m.set((640, 360), t); // game recenters every frame
            let (x, _) = m.get((n.0 + 1, n.1), t); // user moves 1 native px per frame
            total += x - 640;
        }
        assert_eq!(total, 5); // 10 native px at 2x = 5 V px, nothing lost
        let n = m.set((640, 360), t);
        assert_eq!(m.get(n, t), (640, 360)); // no movement, no drift
        let menu = fit((800, 600), (2560, 1440));
        let mut m = Mouse::default();
        let n = m.set((400, 300), menu);
        assert_eq!(m.get(n, menu), (400, 300));
    }
}
