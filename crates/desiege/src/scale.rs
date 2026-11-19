//! UI scale by resolution decoupling.
//!
//! The game runs at a smaller *virtual* resolution V (e.g. 1280x720 for 2x UI on 2560x1440), so
//! its 640x480-based UI lays out larger relative to the screen. Underneath, everything the
//! picture is made of is native size N:
//! - SetDisplayMode(V) sets N; the primary/back buffer chain and z-buffer are created at N.
//! - 3D: viewports on the main render target are scaled, so the scene renders at full N.
//! - 2D: pre-transformed (XYZRHW) vertices and Clear/Blt rects are scaled by `s`.
//! - Input (input.rs): mouse messages, GetCursorPos/SetCursorPos, GetClientRect,
//!   GetSystemMetrics are mapped back to V, so the game only ever sees V.
//! - Locks of the back buffer get a V-sized buffer (lock emulation, below).
//!
//! If V's aspect differs from N (the 800x600 menus), the image is centered: s = min(Nw/Vw,
//! Nh/Vh) with offsets (ox, oy) and black bars.
//!
//! This file holds the state and the mapping; the hooks that apply it are in surface.rs (Blt,
//! Lock) and device.rs (viewports, clears, 2D draws).
//!
//! Enabled by DESIEGE_UI_SCALE (or a `ui_scale` file next to this DLL) > 1. The launcher picks
//! V = N / scale and passes it as width=/height=.
// ponytail: fullscreen only; assumes the game window sits at the desktop origin (screen ==
// client coordinates). Surfaces the game Locks directly (Bink video into the back buffer, if
// any) aren't scaled; they show at V size in the corner. Counted in the stats log.

use crate::{debug::*, device::*, method, patch, rd, shadow, surface::*, HRESULT, P, E_FAIL};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed},
        Mutex, OnceLock,
    },
};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS},
};

// ---------------------------------------------------------------- state

/// User's UI scale (0 = feature off). Read once.
fn user_scale() -> f32 {
    static S: OnceLock<f32> = OnceLock::new();
    *S.get_or_init(|| {
        let from_file = || std::fs::read_to_string(super::self_dir()?.join("ui_scale")).ok();
        let s = std::env::var("DESIEGE_UI_SCALE").ok().or_else(from_file).and_then(|v| v.trim().parse().ok()).unwrap_or(0.0);
        log!("ui scale: {s}");
        if s > 1.0 { s } else { 0.0 }
    })
}

/// Borderless mode (launcher writes a `borderless` file next to this DLL; dxwrapper's
/// FullscreenWindowMode does the windowing). The monitor must never change mode then, so
/// smaller game modes (the 800x600 menus) are rendered at native size even without UI scale.
pub fn borderless() -> bool {
    static B: OnceLock<bool> = OnceLock::new();
    *B.get_or_init(|| {
        let b = std::env::var_os("DESIEGE_BORDERLESS").is_some() || super::self_dir().is_some_and(|d| d.join("borderless").is_file());
        log!("borderless: {b}");
        b
    })
}

/// Resolution decoupling wanted (UI scale or borderless).
pub fn decoupling() -> bool {
    user_scale() > 0.0 || borderless()
}

/// Desktop resolution when the game started (before any mode change).
pub fn native() -> (u32, u32) {
    static N: OnceLock<(u32, u32)> = OnceLock::new();
    *N.get_or_init(|| unsafe {
        let mut dm: DEVMODEW = std::mem::zeroed();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm);
        (dm.dmPelsWidth, dm.dmPelsHeight)
    })
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static VW: AtomicU32 = AtomicU32::new(0);
static VH: AtomicU32 = AtomicU32::new(0);
static S: AtomicU32 = AtomicU32::new(0); // f32 bits
static OX: AtomicU32 = AtomicU32::new(0);
static OY: AtomicU32 = AtomicU32::new(0);
/// Current device render target is the (upscaled) back buffer.
pub static RT_MAIN: AtomicBool = AtomicBool::new(false);
/// Surfaces created at N instead of V: primary chain, back buffer, z-buffer.
// ponytail: plain list, cleared on every mode change; a released pointer reused by a texture
// before then would be treated as main. Use AddRef'd handles if that ever bites.
static MAIN_SURFACES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xf { pub s: f32, pub ox: f32, pub oy: f32 }

fn f(a: &AtomicU32) -> f32 { f32::from_bits(a.load(Relaxed)) }
/// Current UI magnification for code mods (1.0 when not scaling).
pub fn ui_scale_factor() -> f32 { if active() { f(&S) } else { 1.0 } }
pub fn xf() -> Xf { Xf { s: f(&S), ox: f(&OX), oy: f(&OY) } }
pub fn active() -> bool { ACTIVE.load(Relaxed) }
pub fn v() -> (u32, u32) { (VW.load(Relaxed), VH.load(Relaxed)) }

/// Uniform fit of V into N, centered.
pub fn fit(v: (u32, u32), n: (u32, u32)) -> Xf {
    let s = (n.0 as f32 / v.0 as f32).min(n.1 as f32 / v.1 as f32);
    Xf { s, ox: ((n.0 as f32 - v.0 as f32 * s) / 2.0).round(), oy: ((n.1 as f32 - v.1 as f32 * s) / 2.0).round() }
}

impl Xf {
    /// Rect/position in V pixels -> N pixels.
    pub fn x(&self, x: i32) -> i32 { (x as f32 * self.s + self.ox).round() as i32 }
    pub fn y(&self, y: i32) -> i32 { (y as f32 * self.s + self.oy).round() as i32 }
    /// Vertex position (pixel centers stay centered).
    pub fn vx(&self, x: f32) -> f32 { (x + 0.5) * self.s - 0.5 + self.ox }
    pub fn vy(&self, y: f32) -> f32 { (y + 0.5) * self.s - 0.5 + self.oy }
    /// N pixels -> V pixels. Rounds, so back(x(v)) == v exactly (the game recenters the
    /// cursor every frame; any round-trip error shows up as constant drift).
    pub fn back(&self, x: i32, y: i32) -> (i32, i32) {
        (((x as f32 - self.ox) / self.s).round() as i32, ((y as f32 - self.oy) / self.s).round() as i32)
    }
}

/// For the stats line in the log.
pub fn state_summary() -> String {
    format!("active={} V={:?} {:?}", active(), v(), xf())
}

pub fn is_main(surf: P) -> bool {
    !surf.is_null() && MAIN_SURFACES.lock().map_or(false, |l| l.contains(&(surf as usize)))
}
pub fn add_main(surf: P) {
    if let Ok(mut l) = MAIN_SURFACES.lock() {
        if !l.contains(&(surf as usize)) {
            l.push(surf as usize);
        }
    }
}

// ---------------------------------------------------------------- IDirectDraw7 (called from lib.rs)

/// Mode the game asks for -> mode to really set.
pub fn set_display_mode(w: u32, h: u32) -> (u32, u32) {
    let n = native();
    let on = decoupling() && (w < n.0 || h < n.1) && w > 0 && h > 0;
    ACTIVE.store(on, Relaxed);
    if let Ok(mut l) = MAIN_SURFACES.lock() {
        l.clear();
    }
    shadow::reset();
    // Inactive still records V with an identity transform: the cursor overlay uses it too.
    let t = if on { fit((w, h), n) } else { Xf { s: 1.0, ox: 0.0, oy: 0.0 } };
    VW.store(w, Relaxed);
    VH.store(h, Relaxed);
    S.store(t.s.to_bits(), Relaxed);
    OX.store(t.ox.to_bits(), Relaxed);
    OY.store(t.oy.to_bits(), Relaxed);
    if !on {
        return (w, h);
    }
    log!("ui scale: game asked {w}x{h}, rendering {}x{} with {t:?}", n.0, n.1);
    n
}

/// Rewrites a DDSURFACEDESC2 the game passes to CreateSurface. Returns true if it's one of the
/// V-sized render surfaces that must be N instead (or the primary chain, which follows the mode).
pub unsafe fn adjust_create_surface(desc: *mut c_void) -> bool {
    if desc.is_null() {
        return false;
    }
    let (flags, caps) = (rd(desc, 4), rd(desc, 104));
    const DDSD_HEIGHT: u32 = 2; const DDSD_WIDTH: u32 = 4;
    const PRIMARY: u32 = 0x200; const TEXTURE: u32 = 0x1000; const DEVICE3D: u32 = 0x2000; const ZBUFFER: u32 = 0x20000;
    if caps & PRIMARY != 0 {
        return true; // tracked even unscaled: the cursor overlay needs it
    }
    if !active() {
        return false;
    }
    let sized = flags & (DDSD_WIDTH | DDSD_HEIGHT) == DDSD_WIDTH | DDSD_HEIGHT;
    if sized && caps & TEXTURE == 0 && caps & (DEVICE3D | ZBUFFER) != 0 && (rd(desc, 12), rd(desc, 8)) == v() {
        let n = native();
        *(desc as *mut u8).add(12).cast::<u32>() = n.0;
        *(desc as *mut u8).add(8).cast::<u32>() = n.1;
        return true;
    }
    false
}

pub unsafe fn created_main_surface(surf: P) {
    add_main(surf);
    crate::surface::hook(surf);
}

/// DDSURFACEDESC2 reported to the game (GetDisplayMode / GetSurfaceDesc on main surfaces).
pub unsafe fn report_v(desc: *mut c_void) {
    let (w, h) = v();
    *(desc as *mut u8).add(12).cast::<u32>() = w;
    *(desc as *mut u8).add(8).cast::<u32>() = h;
}

orig!(DD_GET_MODE: fn(P, *mut c_void) -> HRESULT);
unsafe extern "system" fn dd_get_display_mode(this: P, desc: *mut c_void) -> HRESULT {
    let hr = call!(DD_GET_MODE as fn(P, *mut c_void) -> HRESULT; this, desc);
    if hr >= 0 && active() && !desc.is_null() {
        report_v(desc);
    }
    hr
}

pub unsafe fn hook_ddraw7(dd: P) {
    patch(dd, 12, dd_get_display_mode as usize, &DD_GET_MODE);
}

/// EnumDisplayModes: make sure the game sees the virtual mode it was told to use.
pub struct ModeInject { template: Option<Vec<u8>>, seen: bool }
impl ModeInject {
    pub fn new() -> Self { ModeInject { template: None, seen: false } }
    pub unsafe fn observe(&mut self, desc: *const c_void) {
        let Some((w, h)) = wanted_virtual() else { return };
        if rd(desc, 84) == 32 {
            self.seen |= (rd(desc, 12), rd(desc, 8)) == (w, h);
            self.template = Some(std::slice::from_raw_parts(desc as *const u8, 124).to_vec());
        }
    }
    /// The extra mode to report after enumeration, if needed.
    pub fn missing(&mut self) -> Option<Vec<u8>> {
        let (w, h) = wanted_virtual()?;
        let mut t = self.template.take().filter(|_| !self.seen)?;
        t[12..16].copy_from_slice(&w.to_le_bytes());
        t[8..12].copy_from_slice(&h.to_le_bytes());
        Some(t)
    }
}
fn wanted_virtual() -> Option<(u32, u32)> {
    decoupling().then(crate::system_detail::launch_resolution).flatten()
}

// ---------------------------------------------------------------- 2D: rects, viewports, vertices

pub fn scale_rect(r: &RECT, t: Xf) -> RECT {
    RECT { left: t.x(r.left), top: t.y(r.top), right: t.x(r.right), bottom: t.y(r.bottom) }
}
/// The picture (V scaled into N), excluding letterbox bars.
pub fn content_rect(t: Xf) -> RECT {
    let (w, h) = v();
    scale_rect(&RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, t)
}

/// Last viewport the game set (in V), to answer GetViewport and restore after bar clears.
pub static GAME_VP: Mutex<Option<Viewport>> = Mutex::new(None);

/// Drawing to the back buffer while scaling (the only time V coordinates need mapping).
pub fn scaling() -> bool { active() && RT_MAIN.load(Relaxed) }

pub fn scale_vp(vp: &Viewport, t: Xf) -> Viewport {
    let (x, y) = (t.x(vp.x as i32), t.y(vp.y as i32));
    let (r, b) = (t.x((vp.x + vp.w) as i32), t.y((vp.y + vp.h) as i32));
    Viewport { x: x as u32, y: y as u32, w: (r - x) as u32, h: (b - y) as u32, ..*vp }
}

/// Copy of `count` TL vertices with x/y moved from V to N space.
pub unsafe fn scaled_tl(fvf: u32, verts: *const c_void, count: usize, t: Xf) -> Vec<u8> {
    let stride = fvf_stride(fvf);
    let mut out = std::slice::from_raw_parts(verts as *const u8, stride * count).to_vec();
    for i in 0..count {
        let p = out.as_mut_ptr().add(i * stride) as *mut f32;
        p.write_unaligned(t.vx(p.read_unaligned()));
        p.add(1).write_unaligned(t.vy(p.add(1).read_unaligned()));
    }
    out
}

/// While alive, stage-0 texture magnification is point-sampled (restored on drop). Used for UI
/// draws at whole-number scales: bilinear blurs 2x-stretched font bitmaps, point keeps them
/// crisp. Fractional scales keep bilinear (point would make glyph strokes uneven).
pub struct PointFilter { dev: P, old: Option<u32> }
impl PointFilter {
    pub unsafe fn new(dev: P) -> Self {
        const MAGFILTER: u32 = 16; const POINT: u32 = 1;
        if xf().s.fract() != 0.0 {
            return PointFilter { dev, old: None };
        }
        let get: unsafe extern "system" fn(P, u32, u32, *mut u32) -> HRESULT = method(dev, 36);
        let set: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = method(dev, 37);
        let mut old = 0;
        if get(dev, 0, MAGFILTER, &mut old) < 0 || old == POINT {
            return PointFilter { dev, old: None };
        }
        set(dev, 0, MAGFILTER, POINT);
        PointFilter { dev, old: Some(old) }
    }
}
impl Drop for PointFilter {
    fn drop(&mut self) {
        if let Some(old) = self.old {
            unsafe { method::<unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT>(self.dev, 37)(self.dev, 0, 16, old) };
        }
    }
}

// ---------------------------------------------------------------- lock emulation
//
// The game locks the back buffer to draw its software cursor (read 64x64 under the mouse, then
// write it back with the cursor on top) and for screenshots, in V coordinates. When scaling it
// gets a V-sized buffer instead: filled from the native pixels on lock (nearest-neighbour
// downscale), written back upscaled on unlock. (The cursor normally takes the cursor overlay
// path in cursor.rs first; this handles the rest: screenshots and bigger locks.)
// ponytail: one outstanding lock at a time, 32-bit surfaces only (the game's cursor pattern);
// anything else passes through unscaled.

struct PendingLock { surf: usize, native: RECT, w: usize, h: usize, buf: Vec<u32>, write: bool, base: Option<Vec<u32>> }
static PENDING: Mutex<Option<PendingLock>> = Mutex::new(None);
/// What the game last read (V rect + pixels). The cursor is read-then-written over the same
/// rect; pixels it writes back unchanged are background, and those keep their native-res
/// originals instead of the low-res copy (which caused the blurry square around the cursor).
static LAST_READ: Mutex<Option<(RECT, Vec<u32>)>> = Mutex::new(None);
fn same_rect(a: &RECT, b: &RECT) -> bool { (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom) }

/// Lock of a main surface. Some(hr) = handled; None = let the real Lock through.
pub unsafe fn emulated_lock(this: P, r: *mut RECT, desc: *mut c_void, flags: u32) -> Option<HRESULT> {
    if !active() {
        return None;
    }
    dbg_input(|| format!("Lock main rect={:?} flags=0x{flags:X}", r.as_ref().map(|r| (r.left, r.top, r.right, r.bottom))));
    let (vw, vh) = v();
    let vr = r.as_ref().copied().unwrap_or(RECT { left: 0, top: 0, right: vw as i32, bottom: vh as i32 });
    let (w, h) = ((vr.right - vr.left).max(0) as usize, (vr.bottom - vr.top).max(0) as usize);
    let t = xf();
    let nr = scale_rect(&vr, t);
    let (nw, nh) = ((nr.right - nr.left).max(1) as usize, (nr.bottom - nr.top).max(1) as usize);
    let mut buf = vec![0u32; w * h];
    let mut base = None;
    if experiment("nocursor") {
        // Perf experiment: hand the game a scratch buffer, never touch the real surface.
    } else if flags & DDLOCK_WRITEONLY != 0 {
        if let Some((_, px)) = LAST_READ.lock().ok().and_then(|l| l.clone()).filter(|(lr, px)| same_rect(lr, &vr) && px.len() == buf.len()) {
            buf.copy_from_slice(&px);
            base = Some(px);
        }
    } else {
        let Some((bits, pitch, bpp)) = lock_rect(this, Some(&nr), DDLOCK_READONLY) else { return Some(E_FAIL) };
        if bpp == 32 {
            for y in 0..h {
                let ny = (((y as f32 + 0.5) * t.s) as usize).min(nh - 1);
                let row = bits.add(ny * pitch) as *const u32;
                for x in 0..w {
                    buf[y * w + x] = *row.add((((x as f32 + 0.5) * t.s) as usize).min(nw - 1));
                }
            }
        }
        unlock_rect(this, Some(&nr));
        if bpp != 32 {
            return None;
        }
        if let Ok(mut l) = LAST_READ.lock() {
            *l = Some((vr, buf.clone()));
        }
        if flags & DDLOCK_READONLY == 0 {
            base = Some(buf.clone()); // read-write lock: same unchanged-pixel rule
        }
    }
    // Describe the surface as the game expects it (V size), pointing at our buffer.
    let hr = call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; this, desc);
    if hr < 0 {
        return Some(hr);
    }
    report_v(desc);
    const DDSD_PITCH: u32 = 8; const DDSD_LPSURFACE: u32 = 0x800;
    let d = desc as *mut u32;
    *d.add(1) |= DDSD_PITCH | DDSD_LPSURFACE;
    *d.add(4) = (w * 4) as u32;
    *d.add(9) = buf.as_mut_ptr() as u32;
    if let Ok(mut p) = PENDING.lock() {
        *p = Some(PendingLock { surf: this as usize, native: nr, w, h, buf, write: flags & DDLOCK_READONLY == 0, base });
    }
    Some(0)
}

/// Unlock of a main surface. True = it was our emulated lock (written back, done).
pub unsafe fn emulated_unlock(this: P) -> bool {
    let pending = PENDING.lock().ok().and_then(|mut p| p.take_if(|p| p.surf == this as usize));
    let Some(p) = pending else { return false };
    if p.write && p.w > 0 && p.h > 0 && !experiment("nocursor") {
        let t = xf();
        // With a base we skip pixels, so the native ones must survive: lock read-write.
        let mode = if p.base.is_some() { 0 } else { DDLOCK_WRITEONLY };
        if let Some((bits, pitch, 32)) = lock_rect(this, Some(&p.native), mode) {
            let (nw, nh) = ((p.native.right - p.native.left) as usize, (p.native.bottom - p.native.top) as usize);
            for ny in 0..nh {
                let sy = (((ny as f32 + 0.5) / t.s) as usize).min(p.h - 1);
                let row = bits.add(ny * pitch) as *mut u32;
                for nx in 0..nw {
                    let i = sy * p.w + (((nx as f32 + 0.5) / t.s) as usize).min(p.w - 1);
                    if p.base.as_ref().is_some_and(|b| b[i] == p.buf[i]) {
                        continue; // background the game didn't touch: keep native pixel
                    }
                    *row.add(nx) = p.buf[i];
                }
            }
            unlock_rect(this, Some(&p.native));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_and_maps_back() {
        let t = fit((1280, 720), (2560, 1440));
        assert_eq!(t, Xf { s: 2.0, ox: 0.0, oy: 0.0 });
        assert_eq!((t.x(640), t.y(360)), (1280, 720));
        assert_eq!(t.back(1280, 720), (640, 360));
        assert_eq!(t.vx(0.0), 0.5); // pixel 0's center (0) lands between native pixels 0 and 1

        let menu = fit((800, 600), (2560, 1440)); // 4:3 menu: pillarboxed
        assert_eq!(menu.s, 2.4);
        assert_eq!((menu.ox, menu.oy), (320.0, 0.0));
        assert_eq!((menu.x(0), menu.x(800)), (320, 2240));
        assert_eq!(menu.back(320, 0), (0, 0));
        assert_eq!(menu.back(menu.x(400), menu.y(300)), (400, 300)); // the drift bug: was 399
    }

    #[test]
    fn scales_tl_vertices() {
        let verts: [f32; 6] = [10.0, 20.0, 0.5, 1.0, 99.0, 98.0]; // one XYZRHW|TEX1 vertex (24 bytes)
        let out = unsafe { scaled_tl(0x104, verts.as_ptr().cast(), 1, Xf { s: 2.0, ox: 0.0, oy: 0.0 }) };
        let f: Vec<f32> = out.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(f, [20.5, 40.5, 0.5, 1.0, 99.0, 98.0]);
    }
}
