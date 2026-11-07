//! UI scale by resolution decoupling.
//!
//! The game runs at a smaller *virtual* resolution V (e.g. 1280x720 for 2x UI on 2560x1440), so
//! its 640x480-based UI lays out larger relative to the screen. Underneath, everything the
//! picture is made of is native size N:
//! - SetDisplayMode(V) sets N; the primary/back buffer chain and z-buffer are created at N.
//! - 3D: viewports on the main render target are scaled, so the scene renders at full N.
//! - 2D: pre-transformed (XYZRHW) vertices and Clear/Blt rects are scaled by `s`.
//! - Input: mouse messages, GetCursorPos/SetCursorPos, GetClientRect, GetSystemMetrics are
//!   mapped back to V, so the game only ever sees V.
//! If V's aspect differs from N (the 800x600 menus), the image is centered: s = min(Nw/Vw,
//! Nh/Vh) with offsets (ox, oy) and black bars.
//!
//! Enabled by DESIEGE_UI_SCALE (or a `ui_scale` file next to this DLL) > 1. The launcher picks
//! V = N / scale and passes it as width=/height=.
// ponytail: fullscreen only; assumes the game window sits at the desktop origin (screen ==
// client coordinates). Surfaces the game Locks directly (Bink video into the back buffer, if
// any) aren't scaled; they show at V size in the corner. Counted in the stats log.

use super::{hook_iat, patch, proc_in, rd, E_FAIL, HRESULT, P};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed},
        Mutex, OnceLock,
    },
};
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{CallWindowProcA, SetWindowLongA, GWL_WNDPROC},
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
fn borderless() -> bool {
    static B: OnceLock<bool> = OnceLock::new();
    *B.get_or_init(|| {
        let b = std::env::var_os("DESIEGE_BORDERLESS").is_some() || super::self_dir().is_some_and(|d| d.join("borderless").is_file());
        log!("borderless: {b}");
        b
    })
}

/// Resolution decoupling wanted (UI scale or borderless).
fn decoupling() -> bool {
    user_scale() > 0.0 || borderless()
}

/// Desktop resolution when the game started (before any mode change).
fn native() -> (u32, u32) {
    static N: OnceLock<(u32, u32)> = OnceLock::new();
    *N.get_or_init(|| unsafe {
        let mut dm: DEVMODEW = std::mem::zeroed();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm);
        (dm.dmPelsWidth, dm.dmPelsHeight)
    })
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Latest IDirectDraw7 (the game recreates it on mode changes); the cursor texture lives on it.
static DDRAW: AtomicUsize = AtomicUsize::new(0);
static VW: AtomicU32 = AtomicU32::new(0);
static VH: AtomicU32 = AtomicU32::new(0);
static S: AtomicU32 = AtomicU32::new(0); // f32 bits
static OX: AtomicU32 = AtomicU32::new(0);
static OY: AtomicU32 = AtomicU32::new(0);
/// Current device render target is the (upscaled) back buffer.
static RT_MAIN: AtomicBool = AtomicBool::new(false);
/// Surfaces created at N instead of V: primary chain, back buffer, z-buffer.
// ponytail: plain list, cleared on every mode change; a released pointer reused by a texture
// before then would be treated as main. Use AddRef'd handles if that ever bites.
static MAIN_SURFACES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xf { pub s: f32, pub ox: f32, pub oy: f32 }

fn f(a: &AtomicU32) -> f32 { f32::from_bits(a.load(Relaxed)) }
/// Current UI magnification for code mods (1.0 when not scaling).
pub fn ui_scale_factor() -> f32 { if active() { f(&S) } else { 1.0 } }
fn xf() -> Xf { Xf { s: f(&S), ox: f(&OX), oy: f(&OY) } }
fn active() -> bool { ACTIVE.load(Relaxed) }
fn v() -> (u32, u32) { (VW.load(Relaxed), VH.load(Relaxed)) }

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

fn is_main(surf: P) -> bool {
    !surf.is_null() && MAIN_SURFACES.lock().map_or(false, |l| l.contains(&(surf as usize)))
}
fn add_main(surf: P) {
    if let Ok(mut l) = MAIN_SURFACES.lock() {
        if !l.contains(&(surf as usize)) {
            l.push(surf as usize);
        }
    }
}

// ---------------------------------------------------------------- stats

static STATS: [AtomicUsize; 15] = [const { AtomicUsize::new(0) }; 15];
const DP_TL: usize = 0; const DP_3D: usize = 1; const DIP_TL: usize = 2; const DIP_3D: usize = 3;
const STRIDED_TL: usize = 4; const VB_TL: usize = 5; const VB_3D: usize = 6; const LOCK_MAIN: usize = 7;
const BLT_MAIN: usize = 8; const BLTFAST_MAIN: usize = 9; const FRAMES: usize = 10; const DRAW_OFF_MAIN: usize = 11; const BLT_FROM_MAIN: usize = 12; const FLIPS: usize = 13; const PRESENT_BLT: usize = 14;
fn stat(i: usize) { STATS[i].fetch_add(1, Relaxed); }

fn log_stats() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let now = std::time::Instant::now();
    let secs = LAST.lock().ok().and_then(|mut l| l.replace(now)).map_or(0.0, |t| (now - t).as_secs_f32());
    let s: Vec<usize> = STATS.iter().map(|a| a.swap(0, Relaxed)).collect();
    log!("fps: {:.0}", if secs > 0.0 { s[FRAMES] as f32 / secs } else { 0.0 });
    log!(
        "scale stats ({} frames): DrawPrimitive tl={} 3d={} | DrawIndexed tl={} 3d={} | strided tl={} | VB tl={} 3d={} | off-main draws={} | main Lock={} Blt={} BltFast={} copies-from-main={} | flips={} present-blts={} | active={} V={:?} {:?}",
        s[FRAMES], s[DP_TL], s[DP_3D], s[DIP_TL], s[DIP_3D], s[STRIDED_TL], s[VB_TL], s[VB_3D], s[DRAW_OFF_MAIN],
        s[LOCK_MAIN], s[BLT_MAIN], s[BLTFAST_MAIN], s[BLT_FROM_MAIN], s[FLIPS], s[PRESENT_BLT], active(), v(), xf()
    );
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
    // Experiment: shadow textures (mipmapped, complex) as render-target textures, so dxwrapper
    // can copy the back buffer into them on the GPU (StretchRect) instead of via the CPU.
    const COMPLEX: u32 = 0x8; const MIPMAP: u32 = 0x40_0000;
    if experiment("rtshadow") && caps & TEXTURE != 0 && caps & COMPLEX != 0 && caps & MIPMAP != 0 {
        *(desc as *mut u8).add(104).cast::<u32>() = caps | DEVICE3D;
        log!("rtshadow: texture {}x{} caps 0x{caps:X} -> 0x{:X}", rd(desc, 12), rd(desc, 8), caps | DEVICE3D);
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
    hook_surface(surf);
}

/// DDSURFACEDESC2 reported to the game (GetDisplayMode / GetSurfaceDesc on main surfaces).
unsafe fn report_v(desc: *mut c_void) {
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
    DDRAW.store(dd as usize, Relaxed);
    patch(dd, 12, dd_get_display_mode as usize, &DD_GET_MODE);
    patch(dd, 27, dd_get_device_identifier as usize, &DD_GET_ID);
}

// DDDEVICEIDENTIFIER2: szDriver[512], szDescription[512], liDriverVersion u64, then
// dwVendorId @1032, dwDeviceId @1036. system_detail.gas keys per-vendor quirks off these.
orig!(DD_GET_ID: fn(P, *mut c_void, u32) -> HRESULT);
unsafe extern "system" fn dd_get_device_identifier(this: P, id: *mut c_void, flags: u32) -> HRESULT {
    let hr = call!(DD_GET_ID as fn(P, *mut c_void, u32) -> HRESULT; this, id, flags);
    if hr >= 0 && !id.is_null() {
        let desc = std::ffi::CStr::from_ptr((id as *const u8).add(512).cast()).to_string_lossy();
        log!("IDirectDraw7::GetDeviceIdentifier -> vendor=0x{:04X} device=0x{:04X} '{desc}'", rd(id, 1032), rd(id, 1036));
    }
    hr
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
    decoupling().then(super::launch_resolution).flatten()
}

// ---------------------------------------------------------------- IDirectDrawSurface7

orig!(SF_BLT: fn(P, *mut RECT, P, *mut RECT, u32, P) -> HRESULT);
orig!(SF_BLTFAST: fn(P, u32, u32, P, *mut RECT, u32) -> HRESULT);
orig!(SF_GET_ATTACHED: fn(P, P, *mut P) -> HRESULT);
orig!(SF_GET_DESC: fn(P, *mut c_void) -> HRESULT);
orig!(SF_LOCK: fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT);

unsafe fn hook_surface(s: P) {
    patch(s, 5, sf_blt as usize, &SF_BLT);
    patch(s, 7, sf_bltfast as usize, &SF_BLTFAST);
    patch(s, 12, sf_get_attached as usize, &SF_GET_ATTACHED);
    patch(s, 22, sf_get_desc as usize, &SF_GET_DESC);
    patch(s, 25, sf_lock as usize, &SF_LOCK);
    patch(s, 32, sf_unlock as usize, &SF_UNLOCK);
    patch(s, 11, sf_flip as usize, &SF_FLIP);
    patch(s, 2, sf_release as usize, &SF_RELEASE);
}

orig!(SF_RELEASE: fn(P) -> u32);
unsafe extern "system" fn sf_release(this: P) -> u32 {
    let left = call!(SF_RELEASE as fn(P) -> u32; this);
    shadow::released(this, left);
    left
}

fn scale_rect(r: &RECT, t: Xf) -> RECT {
    RECT { left: t.x(r.left), top: t.y(r.top), right: t.x(r.right), bottom: t.y(r.bottom) }
}
fn content_rect(t: Xf) -> RECT {
    let (w, h) = v();
    scale_rect(&RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, t)
}

unsafe extern "system" fn sf_blt(this: P, dst: *mut RECT, src: P, src_rect: *mut RECT, flags: u32, fx: P) -> HRESULT {
    trace(|| format!("Blt dst={this:?}{} rect={:?} <- src={src:?}{} rect={:?} flags=0x{flags:X} fill=0x{:X}", mm(this), r_(dst), mm(src), r_(src_rect), if fx.is_null() { 0 } else { rd(fx, 80) }));
    if shadow::on_colorfill(this, dst, flags, fx) {
        return 0;
    }
    let mut dst = dst;
    let mut scaled;
    if active() && is_main(this) {
        stat(BLT_MAIN);
        const COLORFILL: u32 = 0x400;
        if let Some(r) = dst.as_ref() {
            scaled = scale_rect(r, xf());
            dst = &mut scaled;
        } else if flags & COLORFILL == 0 {
            scaled = content_rect(xf()); // whole-surface copy goes to the content area
            dst = &mut scaled;
        }
    }
    // Copies *out of* the back buffer (e.g. character shadows: drawn into a corner of the back
    // buffer, then copied into a shadow texture) must read the native-size region.
    let mut src_rect = src_rect;
    let mut src_scaled;
    if is_main(src) && is_main(this) {
        stat(PRESENT_BLT);
    }
    if is_main(src) && !is_main(this) {
        static N: AtomicUsize = AtomicUsize::new(0);
        if N.fetch_add(1, Relaxed) < 24 {
            let mut d = [0u32; 31];
            d[0] = 124;
            call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; this, d.as_mut_ptr().cast());
            let r = |p: *mut RECT| p.as_ref().map(|r| (r.left, r.top, r.right, r.bottom));
            let mut sd = [0u32; 31];
            sd[0] = 124;
            if !src.is_null() {
                call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; src, sd.as_mut_ptr().cast());
            }
            log!("blt: dst={this:?} main={} {}x{} caps=0x{:X} rect={:?} <- src={src:?} main={} {}x{} caps=0x{:X} rect={:?} flags=0x{flags:X}",
                is_main(this), d[3], d[2], d[26], r(dst), is_main(src), sd[3], sd[2], sd[26], r(src_rect));
        }
    }
    if active() && is_main(src) {
        stat(BLT_FROM_MAIN);
        src_scaled = src_rect.as_ref().map_or_else(|| content_rect(xf()), |r| scale_rect(r, xf()));
        src_rect = &mut src_scaled;
    }
    call!(SF_BLT as fn(P, *mut RECT, P, *mut RECT, u32, P) -> HRESULT; this, dst, src, src_rect, flags, fx)
}

/// DDBLTFAST_SRCCOLORKEY/DESTCOLORKEY/WAIT -> DDBLT_KEYSRC/KEYDEST/WAIT
fn bltfast_to_blt_flags(trans: u32) -> u32 {
    (if trans & 1 != 0 { 0x8000 } else { 0 }) | (if trans & 2 != 0 { 0x2000 } else { 0 }) | (if trans & 0x10 != 0 { 0x0100_0000 } else { 0 })
}

unsafe extern "system" fn sf_bltfast(this: P, x: u32, y: u32, src: P, src_rect: *mut RECT, trans: u32) -> HRESULT {
    trace(|| format!("BltFast dst={this:?}{} at ({x},{y}) <- src={src:?}{} rect={:?} trans=0x{trans:X}", mm(this), mm(src), r_(src_rect)));
    if let Some(hr) = shadow::on_copy_out(this, src) {
        return hr;
    }
    if active() && !is_main(this) && is_main(src) {
        // Copy out of the back buffer into a normal surface: same V-size destination, but the
        // source region is native size, so it needs a (shrinking) Blt.
        stat(BLT_FROM_MAIN);
        static N: AtomicUsize = AtomicUsize::new(0);
        if N.fetch_add(1, Relaxed) < 8 {
            let mut d = [0u32; 31];
            d[0] = 124;
            call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; this, d.as_mut_ptr().cast());
            log!("bltfast from main: dst={this:?} {}x{} caps=0x{:X} caps2=0x{:X} at ({x},{y}) src rect={:?} trans=0x{trans:X}",
                d[3], d[2], d[26], d[27], src_rect.as_ref().map(|r| (r.left, r.top, r.right, r.bottom)));
        }
        let (vw, vh) = v();
        let vr = src_rect.as_ref().copied().unwrap_or(RECT { left: 0, top: 0, right: vw as i32, bottom: vh as i32 });
        let mut dst = RECT { left: x as i32, top: y as i32, right: x as i32 + vr.right - vr.left, bottom: y as i32 + vr.bottom - vr.top };
        let mut sr = scale_rect(&vr, xf());
        return call!(SF_BLT as fn(P, *mut RECT, P, *mut RECT, u32, P) -> HRESULT; this, &mut dst, src, &mut sr, bltfast_to_blt_flags(trans), std::ptr::null_mut());
    }
    if !(active() && is_main(this)) {
        return call!(SF_BLTFAST as fn(P, u32, u32, P, *mut RECT, u32) -> HRESULT; this, x, y, src, src_rect, trans);
    }
    stat(BLTFAST_MAIN);
    // BltFast can't stretch: turn it into a Blt with a scaled destination.
    let (w, h) = match src_rect.as_ref() {
        Some(r) => (r.right - r.left, r.bottom - r.top),
        None => {
            let mut d = [0u32; 31];
            d[0] = 124;
            call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; src, d.as_mut_ptr().cast());
            (d[3] as i32, d[2] as i32)
        }
    };
    let mut dst = scale_rect(&RECT { left: x as i32, top: y as i32, right: x as i32 + w, bottom: y as i32 + h }, xf());
    let mut src_rect = src_rect;
    let mut sr;
    if is_main(src) {
        stat(BLT_FROM_MAIN);
        sr = src_rect.as_ref().map_or_else(|| content_rect(xf()), |r| scale_rect(r, xf()));
        src_rect = &mut sr;
    }
    call!(SF_BLT as fn(P, *mut RECT, P, *mut RECT, u32, P) -> HRESULT; this, &mut dst, src, src_rect, bltfast_to_blt_flags(trans), std::ptr::null_mut())
}

unsafe extern "system" fn sf_get_attached(this: P, caps: P, out: *mut P) -> HRESULT {
    let hr = call!(SF_GET_ATTACHED as fn(P, P, *mut P) -> HRESULT; this, caps, out);
    if hr >= 0 && is_main(this) && !out.is_null() {
        add_main(*out); // back buffer of the primary chain
    }
    hr
}

unsafe extern "system" fn sf_get_desc(this: P, desc: *mut c_void) -> HRESULT {
    let hr = call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; this, desc);
    if hr >= 0 && active() && !desc.is_null() && is_main(this) {
        report_v(desc);
    }
    hr
}

// Lock emulation for scaled surfaces. The game locks the back buffer to draw its software
// cursor (read 64x64 under the mouse, then write it back with the cursor on top) and for
// screenshots, in V coordinates. It gets a V-sized buffer instead: filled from the native
// pixels on lock (nearest-neighbour downscale), written back upscaled on unlock.
// ponytail: one outstanding lock at a time, 32-bit surfaces only (the game's cursor pattern);
// anything else passes through unscaled.
/// Debug: DESIEGE_EXPERIMENT=a,b toggles perf experiments.
fn experiment(name: &str) -> bool {
    static E: OnceLock<String> = OnceLock::new();
    E.get_or_init(|| std::env::var("DESIEGE_EXPERIMENT").unwrap_or_default()).split(',').any(|e| e.trim() == name)
}

struct PendingLock { surf: usize, native: RECT, w: usize, h: usize, buf: Vec<u32>, write: bool, base: Option<Vec<u32>> }
static PENDING: Mutex<Option<PendingLock>> = Mutex::new(None);
/// What the game last read (V rect + pixels). The cursor is read-then-written over the same
/// rect; pixels it writes back unchanged are background, and those keep their native-res
/// originals instead of the low-res copy (which caused the blurry square around the cursor).
static LAST_READ: Mutex<Option<(RECT, Vec<u32>)>> = Mutex::new(None);
fn same_rect(a: &RECT, b: &RECT) -> bool { (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom) }
const DDLOCK_WAIT: u32 = 1; const DDLOCK_READONLY: u32 = 0x10; const DDLOCK_WRITEONLY: u32 = 0x20;

/// Real lock of a native rect: (bits, pitch in bytes, bpp).
unsafe fn real_lock(surf: P, r: &RECT, flags: u32) -> Option<(*mut u8, usize, u32)> {
    let mut d = [0u32; 31];
    d[0] = 124;
    let mut r = *r;
    let hr = call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; surf, &mut r, d.as_mut_ptr().cast(), flags | DDLOCK_WAIT, std::ptr::null_mut());
    (hr >= 0).then(|| (d[9] as *mut u8, d[4] as usize, d[21]))
}

unsafe extern "system" fn sf_lock(this: P, r: *mut RECT, desc: *mut c_void, flags: u32, ev: P) -> HRESULT {
    if !is_main(this) || desc.is_null() {
        return call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; this, r, desc, flags, ev);
    }
    stat(LOCK_MAIN);
    if let Some(hr) = cursor::lock(this, r, desc, flags) {
        return hr;
    }
    if !active() {
        return call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; this, r, desc, flags, ev);
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
        let Some((bits, pitch, bpp)) = real_lock(this, &nr, DDLOCK_READONLY) else { return E_FAIL };
        if bpp == 32 {
            for y in 0..h {
                let ny = (((y as f32 + 0.5) * t.s) as usize).min(nh - 1);
                let row = bits.add(ny * pitch) as *const u32;
                for x in 0..w {
                    buf[y * w + x] = *row.add((((x as f32 + 0.5) * t.s) as usize).min(nw - 1));
                }
            }
        }
        sf_unlock_real(this, &nr);
        if bpp != 32 {
            return call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; this, r, desc, flags, ev);
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
        return hr;
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
    0
}

orig!(SF_UNLOCK: fn(P, *mut RECT) -> HRESULT);
unsafe fn sf_unlock_real(surf: P, r: &RECT) {
    let mut r = *r;
    call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; surf, &mut r);
}

unsafe extern "system" fn sf_unlock(this: P, r: *mut RECT) -> HRESULT {
    if cursor::unlock(this) {
        return 0;
    }
    let pending = PENDING.lock().ok().and_then(|mut p| p.take_if(|p| p.surf == this as usize));
    let Some(p) = pending else {
        return call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; this, r);
    };
    if p.write && p.w > 0 && p.h > 0 && !experiment("nocursor") {
        let t = xf();
        // With a base we skip pixels, so the native ones must survive: lock read-write.
        let mode = if p.base.is_some() { 0 } else { DDLOCK_WRITEONLY };
        if let Some((bits, pitch, 32)) = real_lock(this, &p.native, mode) {
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
            sf_unlock_real(this, &p.native);
        }
    }
    0
}

/// Debug: DESIEGE_SNAPSHOT=300,900 dumps the native back buffer at those frames to
/// %LOCALAPPDATA%\Desiege\snap_<frame>.bmp (exclusive fullscreen can't be screen-captured).
static DEVICE: AtomicUsize = AtomicUsize::new(0);
static SNAP_FRAME: AtomicUsize = AtomicUsize::new(usize::MAX);

orig!(SF_FLIP: fn(P, P, u32) -> HRESULT);
unsafe extern "system" fn sf_flip(this: P, target: P, flags: u32) -> HRESULT {
    trace(|| "Flip".into());
    trace_frame_boundary();
    stat(FLIPS);
    let dev = DEVICE.load(Relaxed);
    if dev != 0 {
        crate::code_mods::run_frame();
        overlay::draw(dev as P);
        cursor::draw(dev as P);
    }
    let frame = SNAP_FRAME.swap(usize::MAX, Relaxed);
    if dev != 0 && frame != usize::MAX {
        maybe_snapshot(dev as P, frame);
    }
    call!(SF_FLIP as fn(P, P, u32) -> HRESULT; this, target, flags)
}

unsafe fn maybe_snapshot(dev: P, frame: usize) {
    static FRAMES: OnceLock<Vec<usize>> = OnceLock::new();
    let frames = FRAMES.get_or_init(|| {
        std::env::var("DESIEGE_SNAPSHOT").unwrap_or_default().split(',').filter_map(|s| s.trim().parse().ok()).collect()
    });
    if !frames.contains(&frame) {
        return;
    }
    // IDirect3DDevice7::GetRenderTarget = slot 9
    let get_rt: unsafe extern "system" fn(P, *mut P) -> HRESULT = std::mem::transmute(*(*(dev as *const *const usize)).add(9));
    let mut rt = std::ptr::null_mut();
    if get_rt(dev, &mut rt) < 0 || rt.is_null() {
        return;
    }
    let mut d = [0u32; 31];
    d[0] = 124;
    const DDLOCK_WAIT: u32 = 1; const DDLOCK_READONLY: u32 = 0x10;
    if call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; rt, std::ptr::null_mut(), d.as_mut_ptr().cast(), DDLOCK_WAIT | DDLOCK_READONLY, std::ptr::null_mut()) < 0 {
        log!("snapshot: Lock failed");
        return;
    }
    let (h, w, pitch, bits, bpp) = (d[2] as usize, d[3] as usize, d[4] as usize, d[9] as *const u8, d[21]);
    let mut bmp = Vec::with_capacity(54 + w * h * 4);
    let size = (54 + w * h * 4) as u32;
    bmp.extend_from_slice(b"BM");
    for v in [size, 0, 54, 40, w as u32, (-(h as i32)) as u32] { bmp.extend_from_slice(&v.to_le_bytes()); }
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    for _ in 0..6 { bmp.extend_from_slice(&0u32.to_le_bytes()); }
    if bpp == 32 {
        for y in 0..h {
            bmp.extend_from_slice(std::slice::from_raw_parts(bits.add(y * pitch), w * 4));
        }
    }
    let unlock: unsafe extern "system" fn(P, *mut RECT) -> HRESULT = std::mem::transmute(*(*(rt as *const *const usize)).add(32));
    unlock(rt, std::ptr::null_mut());
    let release: unsafe extern "system" fn(P) -> u32 = std::mem::transmute(*(*(rt as *const *const usize)).add(2));
    release(rt);
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        let path = std::path::PathBuf::from(dir).join("Desiege").join(format!("snap_{frame}.bmp"));
        let _ = std::fs::write(&path, &bmp);
        log!("snapshot: {w}x{h} {bpp}bpp -> {}", path.display());
    }
}

// ---------------------------------------------------------------- IDirect3DDevice7

orig!(DEV_END_SCENE: fn(P) -> HRESULT);
orig!(DEV_SET_RT: fn(P, P, u32) -> HRESULT);
orig!(DEV_CLEAR: fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT);
orig!(DEV_SET_VP: fn(P, *const Viewport) -> HRESULT);
orig!(DEV_GET_VP: fn(P, *mut Viewport) -> HRESULT);
orig!(DEV_DP: fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT);
orig!(DEV_DIP: fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT);
orig!(DEV_DPS: fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT);
orig!(DEV_DIPS: fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT);
orig!(DEV_DPVB: fn(P, u32, P, u32, u32, u32) -> HRESULT);
orig!(DEV_DIPVB: fn(P, u32, P, u32, u32, *const u16, u32, u32) -> HRESULT);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Viewport { x: u32, y: u32, w: u32, h: u32, min_z: f32, max_z: f32 }

/// Last viewport the game set (in V), to answer GetViewport and restore after bar clears.
static GAME_VP: Mutex<Option<Viewport>> = Mutex::new(None);

pub unsafe fn created_device(dev: P, rt: P) {
    RT_MAIN.store(is_main(rt), Relaxed);
    DEVICE.store(dev as usize, Relaxed);
    patch(dev, 6, dev_end_scene as usize, &DEV_END_SCENE);
    patch(dev, 8, dev_set_rt as usize, &DEV_SET_RT);
    patch(dev, 10, dev_clear as usize, &DEV_CLEAR);
    patch(dev, 13, dev_set_vp as usize, &DEV_SET_VP);
    patch(dev, 15, dev_get_vp as usize, &DEV_GET_VP);
    patch(dev, 25, dev_dp as usize, &DEV_DP);
    patch(dev, 26, dev_dip as usize, &DEV_DIP);
    patch(dev, 29, dev_dps as usize, &DEV_DPS);
    patch(dev, 30, dev_dips as usize, &DEV_DIPS);
    patch(dev, 31, dev_dpvb as usize, &DEV_DPVB);
    patch(dev, 32, dev_dipvb as usize, &DEV_DIPVB);
    patch(dev, 35, dev_set_texture as usize, &DEV_SET_TEX);
    patch(dev, 11, dev_set_transform as usize, &DEV_SET_XFORM);
    patch(dev, 5, dev_begin_scene as usize, &DEV_BEGIN_SCENE);
}

orig!(DEV_SET_XFORM: fn(P, u32, *const f32) -> HRESULT);
unsafe extern "system" fn dev_set_transform(this: P, kind: u32, m: *const f32) -> HRESULT {
    trace(|| format!("SetTransform({kind}) m00={:.3} m11={:.3} m22={:.3} m32={:.3}", *m, *m.add(5), *m.add(10), *m.add(14)));
    call!(DEV_SET_XFORM as fn(P, u32, *const f32) -> HRESULT; this, kind, m)
}
orig!(DEV_BEGIN_SCENE: fn(P) -> HRESULT);
unsafe extern "system" fn dev_begin_scene(this: P) -> HRESULT {
    trace(|| "BeginScene".into());
    call!(DEV_BEGIN_SCENE as fn(P) -> HRESULT; this)
}
orig!(DEV_SET_TEX: fn(P, u32, P) -> HRESULT);
unsafe extern "system" fn dev_set_texture(this: P, stage: u32, tex: P) -> HRESULT {
    trace(|| format!("SetTexture({stage}, {tex:?})"));
    call!(DEV_SET_TEX as fn(P, u32, P) -> HRESULT; this, stage, shadow::substitute(tex))
}

fn scaling() -> bool { active() && RT_MAIN.load(Relaxed) }

unsafe extern "system" fn dev_end_scene(this: P) -> HRESULT {
    trace(|| "EndScene".into());
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    static TOTAL: AtomicUsize = AtomicUsize::new(0);
    stat(FRAMES);
    if STATS[FRAMES].load(Relaxed) >= 600 && LOGGED.fetch_add(1, Relaxed) < 20 {
        log_stats();
    }
    let hr = call!(DEV_END_SCENE as fn(P) -> HRESULT; this);
    let frame = TOTAL.fetch_add(1, Relaxed);
    if experiment("profile") {
        crate::profile::frame(frame);
    }
    SNAP_FRAME.store(frame, Relaxed); // snapshot happens at Flip, after the cursor is drawn
    hr
}

unsafe extern "system" fn dev_set_rt(this: P, rt: P, flags: u32) -> HRESULT {
    trace(|| format!("SetRenderTarget({rt:?}{})", mm(rt)));
    let hr = call!(DEV_SET_RT as fn(P, P, u32) -> HRESULT; this, rt, flags);
    if hr >= 0 {
        RT_MAIN.store(is_main(rt), Relaxed);
    }
    hr
}

fn scale_vp(vp: &Viewport, t: Xf) -> Viewport {
    let (x, y) = (t.x(vp.x as i32), t.y(vp.y as i32));
    let (r, b) = (t.x((vp.x + vp.w) as i32), t.y((vp.y + vp.h) as i32));
    Viewport { x: x as u32, y: y as u32, w: (r - x) as u32, h: (b - y) as u32, ..*vp }
}

unsafe extern "system" fn dev_set_vp(this: P, vp: *const Viewport) -> HRESULT {
    trace(|| format!("SetViewport({:?})", vp.as_ref()));
    if let Some(hr) = shadow::on_set_viewport(this, vp) {
        return hr;
    }
    if !scaling() || vp.is_null() {
        return call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; this, vp);
    }
    if let Ok(mut g) = GAME_VP.lock() {
        *g = Some(*vp);
    }
    let scaled = scale_vp(&*vp, xf());
    call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; this, &scaled)
}

unsafe extern "system" fn dev_get_vp(this: P, vp: *mut Viewport) -> HRESULT {
    let hr = call!(DEV_GET_VP as fn(P, *mut Viewport) -> HRESULT; this, vp);
    if hr >= 0 && scaling() && !vp.is_null() {
        if let Some(g) = GAME_VP.lock().ok().and_then(|g| *g) {
            *vp = g;
        }
    }
    hr
}

unsafe extern "system" fn dev_clear(this: P, count: u32, rects: *const RECT, flags: u32, color: u32, z: f32, stencil: u32) -> HRESULT {
    trace(|| format!("Clear(count={count} rect0={:?} flags=0x{flags:X} color=0x{color:X} z={z})", if count > 0 { r_(rects as *mut RECT) } else { None }));
    if !scaling() {
        return call!(DEV_CLEAR as fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT; this, count, rects, flags, color, z, stencil);
    }
    let t = xf();
    if count == 0 || rects.is_null() {
        // Letterboxed: also blank the bars (Clear is clipped to the viewport, so widen it).
        let (n, game_vp) = (native(), GAME_VP.lock().ok().and_then(|g| *g));
        if (t.ox > 0.0 || t.oy > 0.0) && flags & 1 != 0 {
            let full = Viewport { x: 0, y: 0, w: n.0, h: n.1, min_z: 0.0, max_z: 1.0 };
            call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; this, &full);
            call!(DEV_CLEAR as fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT; this, 0, std::ptr::null(), 1, 0xFF00_0000, z, stencil);
            if let Some(g) = game_vp {
                call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; this, &scale_vp(&g, t));
            }
        }
        return call!(DEV_CLEAR as fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT; this, count, rects, flags, color, z, stencil);
    }
    // D3DRECT has the same layout as RECT.
    let scaled: Vec<RECT> = std::slice::from_raw_parts(rects, count as usize).iter().map(|r| scale_rect(r, t)).collect();
    call!(DEV_CLEAR as fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT; this, count, scaled.as_ptr(), flags, color, z, stencil)
}

/// While alive, stage-0 texture magnification is point-sampled (restored on drop). Used for UI
/// draws at whole-number scales: bilinear blurs 2x-stretched font bitmaps, point keeps them
/// crisp. Fractional scales keep bilinear (point would make glyph strokes uneven).
struct PointFilter { dev: P, old: Option<u32> }
impl PointFilter {
    unsafe fn new(dev: P) -> Self {
        const MAGFILTER: u32 = 16; const POINT: u32 = 1;
        let s = xf().s;
        if s.fract() != 0.0 {
            return PointFilter { dev, old: None };
        }
        let vt = *(dev as *const *const usize);
        let get: unsafe extern "system" fn(P, u32, u32, *mut u32) -> HRESULT = std::mem::transmute(*vt.add(36));
        let set: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = std::mem::transmute(*vt.add(37));
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
            unsafe {
                let set: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = std::mem::transmute(*(*(self.dev as *const *const usize)).add(37));
                set(self.dev, 0, 16, old);
            }
        }
    }
}

/// Bytes per vertex for a flexible vertex format.
pub fn fvf_stride(fvf: u32) -> usize {
    let mut n = match fvf & 0xE { 0x2 => 12, 0x4 => 16, 0x6 => 16, 0x8 => 20, 0xA => 24, 0xC => 28, 0xE => 32, _ => 0 };
    for (bit, size) in [(0x10, 12), (0x20, 4), (0x40, 4), (0x80, 4)] {
        if fvf & bit != 0 { n += size }
    }
    for i in 0..((fvf >> 8) & 0xF) {
        n += match (fvf >> (16 + 2 * i)) & 3 { 0 => 8, 1 => 12, 2 => 16, _ => 4 };
    }
    n
}
const XYZRHW: u32 = 0x4;
fn is_tl(fvf: u32) -> bool { fvf & 0xE == XYZRHW }

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

unsafe extern "system" fn dev_dp(this: P, prim: u32, fvf: u32, verts: *const c_void, count: u32, flags: u32) -> HRESULT {
    trace(|| format!("DrawPrimitive(prim={prim} fvf=0x{fvf:X} n={count}){}", if is_tl(fvf) && !verts.is_null() { format!(" first=({:.1},{:.1})", *(verts as *const f32), *(verts as *const f32).add(1)) } else { String::new() }));
    if !active() || !is_tl(fvf) || verts.is_null() {
        stat(if is_tl(fvf) { DP_TL } else { DP_3D });
        return call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, verts, count, flags);
    }
    stat(DP_TL);
    if !RT_MAIN.load(Relaxed) {
        stat(DRAW_OFF_MAIN);
        return call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, verts, count, flags);
    }
    let v = scaled_tl(fvf, verts, count as usize, xf());
    let _f = PointFilter::new(this);
    call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), count, flags)
}

unsafe extern "system" fn dev_dip(this: P, prim: u32, fvf: u32, verts: *const c_void, vcount: u32, idx: *const u16, icount: u32, flags: u32) -> HRESULT {
    trace(|| format!("DrawIndexedPrimitive(prim={prim} fvf=0x{fvf:X} v={vcount} i={icount})"));
    stat(if is_tl(fvf) { DIP_TL } else { DIP_3D });
    if !scaling() || !is_tl(fvf) || verts.is_null() {
        return call!(DEV_DIP as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, verts, vcount, idx, icount, flags);
    }
    let v = scaled_tl(fvf, verts, vcount as usize, xf());
    let _f = PointFilter::new(this);
    call!(DEV_DIP as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), vcount, idx, icount, flags)
}

// Strided TL draws: only counted so far (the log tells us if the game uses them).
unsafe extern "system" fn dev_dps(this: P, prim: u32, fvf: u32, data: *const c_void, count: u32, flags: u32) -> HRESULT {
    if is_tl(fvf) { stat(STRIDED_TL) }
    call!(DEV_DPS as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, data, count, flags)
}
unsafe extern "system" fn dev_dips(this: P, prim: u32, fvf: u32, data: *const c_void, vcount: u32, idx: *const u16, icount: u32, flags: u32) -> HRESULT {
    if is_tl(fvf) { stat(STRIDED_TL) }
    call!(DEV_DIPS as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, data, vcount, idx, icount, flags)
}

/// IDirect3DVertexBuffer7: GetVertexBufferDesc (slot 6) -> D3DVERTEXBUFFERDESC { size, caps, fvf, count }.
unsafe fn vb_fvf(vb: P) -> u32 {
    let mut d = [16u32, 0, 0, 0];
    let get: unsafe extern "system" fn(P, *mut u32) -> HRESULT = std::mem::transmute(*(*(vb as *const *const usize)).add(6));
    if get(vb, d.as_mut_ptr()) >= 0 { d[2] } else { 0 }
}

/// Copy of vertices [start, start+num) of a TL vertex buffer, scaled. Lock = slot 3, Unlock = 4.
unsafe fn vb_scaled(vb: P, fvf: u32, start: u32, num: u32) -> Option<Vec<u8>> {
    let lock: unsafe extern "system" fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT = std::mem::transmute(*(*(vb as *const *const usize)).add(3));
    let unlock: unsafe extern "system" fn(P) -> HRESULT = std::mem::transmute(*(*(vb as *const *const usize)).add(4));
    let mut ptr = std::ptr::null_mut();
    const DDLOCK_WAIT: u32 = 1; const DDLOCK_READONLY: u32 = 0x10;
    if lock(vb, DDLOCK_WAIT | DDLOCK_READONLY, &mut ptr, std::ptr::null_mut()) < 0 || ptr.is_null() {
        return None;
    }
    let base = (ptr as *const u8).add(start as usize * fvf_stride(fvf));
    let out = scaled_tl(fvf, base.cast(), num as usize, xf());
    unlock(vb);
    Some(out)
}

unsafe extern "system" fn dev_dpvb(this: P, prim: u32, vb: P, start: u32, num: u32, flags: u32) -> HRESULT {
    trace(|| format!("DrawPrimitiveVB(prim={prim} n={num})"));
    if let Some((ptr, fvf)) = dynvb::range(vb, start, num) {
        stat(if is_tl(fvf) { VB_TL } else { VB_3D });
        if scaling() && is_tl(fvf) {
            let v = scaled_tl(fvf, ptr, num as usize, xf());
            return call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), num, flags);
        }
        return call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, ptr, num, flags);
    }
    let fvf = if vb.is_null() { 0 } else { vb_fvf(vb) };
    stat(if is_tl(fvf) { VB_TL } else { VB_3D });
    if scaling() && is_tl(fvf) {
        if let Some(v) = vb_scaled(vb, fvf, start, num) {
            return call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), num, flags);
        }
    }
    call!(DEV_DPVB as fn(P, u32, P, u32, u32, u32) -> HRESULT; this, prim, vb, start, num, flags)
}

unsafe extern "system" fn dev_dipvb(this: P, prim: u32, vb: P, start: u32, num: u32, idx: *const u16, icount: u32, flags: u32) -> HRESULT {
    trace(|| format!("DrawIndexedPrimitiveVB(prim={prim} v={num} i={icount})"));
    if let Some((ptr, fvf)) = dynvb::range(vb, start, num) {
        // Indices are relative to `start`, which is where `ptr` points.
        stat(if is_tl(fvf) { VB_TL } else { VB_3D });
        if scaling() && is_tl(fvf) {
            let v = scaled_tl(fvf, ptr, num as usize, xf());
            return call!(DEV_DIP as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), num, idx, icount, flags);
        }
        return call!(DEV_DIP as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, ptr, num, idx, icount, flags);
    }
    let fvf = if vb.is_null() { 0 } else { vb_fvf(vb) };
    stat(if is_tl(fvf) { VB_TL } else { VB_3D });
    if scaling() && is_tl(fvf) {
        // Indices are relative to `start`, which is where our copy begins.
        if let Some(v) = vb_scaled(vb, fvf, start, num) {
            return call!(DEV_DIP as fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT; this, prim, fvf, v.as_ptr().cast(), num, idx, icount, flags);
        }
    }
    call!(DEV_DIPVB as fn(P, u32, P, u32, u32, *const u16, u32, u32) -> HRESULT; this, prim, vb, start, num, idx, icount, flags)
}

// ---------------------------------------------------------------- dynamic vertex buffers

/// The game streams most geometry through a 16384-vertex ring buffer: Lock(NOOVERWRITE),
/// append, DrawIndexedPrimitiveVB, repeat (DISCARD when full) - the textbook fast path.
/// dxwrapper drops NOOVERWRITE and keeps the buffer in D3D9's managed pool, so every append
/// re-uploads the whole buffer with a sync: ~60% of the frame went to the driver waiting and
/// copying. Buffers the game locks with NOOVERWRITE/DISCARD get a system-memory mirror instead:
/// Lock hands out the mirror, Unlock is free, and draws from them become draw-from-memory calls
/// covering only the vertices used, which D3D9 streams without stalling. Static buffers
/// (terrain, meshes) are untouched.
pub mod dynvb {
    use super::*;
    use std::collections::HashMap;

    struct Mirror { data: Vec<u8>, fvf: u32, stride: usize }
    static MIRRORS: Mutex<Option<HashMap<usize, Mirror>>> = Mutex::new(None);
    fn with<R>(f: impl FnOnce(&mut HashMap<usize, Mirror>) -> R) -> Option<R> {
        MIRRORS.lock().ok().map(|mut m| f(m.get_or_insert_with(HashMap::new)))
    }

    fn enabled() -> bool {
        !experiment("nodynvb")
    }

    orig!(D3D_CREATE_VB: fn(P, *mut u32, *mut P, u32) -> HRESULT);
    orig!(VB_RELEASE: fn(P) -> u32);
    orig!(VB_LOCK: fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT);
    orig!(VB_UNLOCK: fn(P) -> HRESULT);
    orig!(VB_PROCESS: fn(P, u32, u32, u32, P, u32, P, u32) -> HRESULT);

    /// Hook IDirect3D7::CreateVertexBuffer (slot 5); vertex buffers get hooked when created.
    pub unsafe fn hook_d3d7(d3d: P) {
        patch(d3d, 5, create_vb as usize, &D3D_CREATE_VB);
    }

    unsafe extern "system" fn create_vb(this: P, desc: *mut u32, out: *mut P, flags: u32) -> HRESULT {
        let hr = call!(D3D_CREATE_VB as fn(P, *mut u32, *mut P, u32) -> HRESULT; this, desc, out, flags);
        if hr >= 0 && !out.is_null() && !(*out).is_null() {
            let vb = *out;
            patch(vb, 2, vb_release as usize, &VB_RELEASE);
            patch(vb, 3, vb_lock as usize, &VB_LOCK);
            patch(vb, 4, vb_unlock as usize, &VB_UNLOCK);
            patch(vb, 5, vb_process as usize, &VB_PROCESS);
        }
        hr
    }

    /// D3DVERTEXBUFFERDESC { size, caps, fvf, count } via GetVertexBufferDesc (slot 6).
    unsafe fn desc(vb: P) -> Option<(u32, u32)> {
        let mut d = [16u32, 0, 0, 0];
        let get: unsafe extern "system" fn(P, *mut u32) -> HRESULT = std::mem::transmute(*(*(vb as *const *const usize)).add(6));
        (get(vb, d.as_mut_ptr()) >= 0).then_some((d[2], d[3]))
    }

    unsafe extern "system" fn vb_lock(this: P, flags: u32, out: *mut *mut c_void, size: *mut u32) -> HRESULT {
        const NOOVERWRITE: u32 = 0x1000; const DISCARDCONTENTS: u32 = 0x2000; const READONLY: u32 = 0x10;
        let dynamic_lock = flags & (NOOVERWRITE | DISCARDCONTENTS) != 0;
        if enabled() && !out.is_null() {
            let known = with(|m| m.contains_key(&(this as usize))).unwrap_or(false);
            if known || dynamic_lock {
                if !known {
                    // First dynamic lock: start the mirror from the buffer's current contents.
                    let Some((fvf, count)) = desc(this) else {
                        return call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, flags, out, size);
                    };
                    let stride = fvf_stride(fvf);
                    let mut data = vec![0u8; stride * count as usize];
                    let mut p = std::ptr::null_mut();
                    if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, READONLY | 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
                        std::ptr::copy_nonoverlapping(p as *const u8, data.as_mut_ptr(), data.len());
                        call!(VB_UNLOCK as fn(P) -> HRESULT; this);
                    }
                    let n = with(|m| {
                        m.insert(this as usize, Mirror { data, fvf, stride });
                        m.len()
                    });
                    log!("dynamic vb #{}: {this:?} {count} verts fvf=0x{fvf:X} mirrored", n.unwrap_or(0));
                }
                return with(|m| {
                    let mi = m.get_mut(&(this as usize)).unwrap();
                    *out = mi.data.as_mut_ptr().cast();
                    if !size.is_null() {
                        *size = mi.data.len() as u32;
                    }
                    0
                })
                .unwrap_or(E_FAIL);
            }
        }
        call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, flags, out, size)
    }

    unsafe extern "system" fn vb_unlock(this: P) -> HRESULT {
        if with(|m| m.contains_key(&(this as usize))).unwrap_or(false) {
            return 0;
        }
        call!(VB_UNLOCK as fn(P) -> HRESULT; this)
    }

    unsafe extern "system" fn vb_release(this: P) -> u32 {
        let left = call!(VB_RELEASE as fn(P) -> u32; this);
        if left == 0 {
            with(|m| m.remove(&(this as usize)));
        }
        left
    }

    /// Copy a mirror into the real buffer (something other than our draws will read it).
    unsafe fn sync_real(vb: P) {
        with(|m| {
            let Some(mi) = m.get(&(vb as usize)) else { return };
            let mut p = std::ptr::null_mut();
            if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; vb, 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
                std::ptr::copy_nonoverlapping(mi.data.as_ptr(), p as *mut u8, mi.data.len());
                call!(VB_UNLOCK as fn(P) -> HRESULT; vb);
            }
        });
    }
    /// Refresh a mirror from the real buffer (something else wrote it).
    unsafe fn sync_mirror(vb: P) {
        with(|m| {
            let Some(mi) = m.get_mut(&(vb as usize)) else { return };
            let mut p = std::ptr::null_mut();
            if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; vb, 0x10 | 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
                std::ptr::copy_nonoverlapping(p as *const u8, mi.data.as_mut_ptr(), mi.data.len());
                call!(VB_UNLOCK as fn(P) -> HRESULT; vb);
            }
        });
    }

    unsafe extern "system" fn vb_process(this: P, op: u32, dst_index: u32, count: u32, src: P, src_index: u32, dev: P, flags: u32) -> HRESULT {
        if !src.is_null() {
            sync_real(src);
        }
        let hr = call!(VB_PROCESS as fn(P, u32, u32, u32, P, u32, P, u32) -> HRESULT; this, op, dst_index, count, src, src_index, dev, flags);
        sync_mirror(this);
        hr
    }

    /// For a mirrored buffer: pointer to vertex `start` and the FVF.
    pub unsafe fn range(vb: P, start: u32, num: u32) -> Option<(*const c_void, u32)> {
        with(|m| {
            let mi = m.get(&(vb as usize))?;
            let end = (start as usize + num as usize) * mi.stride;
            (end <= mi.data.len()).then(|| (mi.data.as_ptr().add(start as usize * mi.stride).cast(), mi.fvf))
        })
        .flatten()
    }
}

// ---------------------------------------------------------------- our own quads (cursor, overlay)

/// Helpers for drawing our own textured quads on top of the finished frame: ARGB textures on
/// the game's DirectDraw object, and a draw pass that saves and restores every device state it
/// touches. Used by the cursor overlay and by code mods' overlay text.
pub mod gpu {
    use super::*;

    pub unsafe fn method<F>(obj: P, idx: usize) -> F {
        std::mem::transmute_copy(&*(*(obj as *const *const usize)).add(idx))
    }

    /// An ARGB8888 (premultiplied) texture, power-of-two sized.
    pub struct Tex {
        ptr: usize,
        owner: usize,
        pub w: usize,
        pub h: usize,
    }

    impl Tex {
        /// Still usable (the game recreates its DirectDraw object on mode changes).
        pub fn valid(&self) -> bool {
            self.owner == DDRAW.load(Relaxed)
        }
        pub fn fits(&self, w: usize, h: usize) -> bool {
            self.valid() && self.w >= w && self.h >= h
        }
    }

    impl Drop for Tex {
        fn drop(&mut self) {
            // ponytail: a texture whose DirectDraw object is gone is just dropped, not Released.
            if self.valid() {
                unsafe { method::<unsafe extern "system" fn(P) -> u32>(self.ptr as P, 2)(self.ptr as P) };
            }
        }
    }

    pub unsafe fn create(w: usize, h: usize) -> Option<Tex> {
        let pow2 = |n: usize| n.next_power_of_two().max(8);
        let (tw, th) = (pow2(w), pow2(h));
        let mut d = [0u32; 31];
        d[0] = 124;
        d[1] = 1 | 2 | 4 | 0x1000; // CAPS | HEIGHT | WIDTH | PIXELFORMAT
        d[2] = th as u32;
        d[3] = tw as u32;
        d[18] = 32; // ddpfPixelFormat @72: size, flags RGB|ALPHAPIXELS, 32 bpp, ARGB masks
        d[19] = 0x40 | 0x1;
        d[21] = 32;
        d[22] = 0x00FF_0000;
        d[23] = 0x0000_FF00;
        d[24] = 0x0000_00FF;
        d[25] = 0xFF00_0000;
        d[26] = 0x1000; // DDSCAPS_TEXTURE
        d[27] = 0x10; // DDSCAPS2_TEXTUREMANAGE
        let dd = DDRAW.load(Relaxed);
        let create: unsafe extern "system" fn(P, *mut c_void, *mut P, P) -> HRESULT =
            std::mem::transmute(crate::DD_CREATE_SURFACE.load(Relaxed));
        let mut tex = std::ptr::null_mut();
        let hr = create(dd as P, d.as_mut_ptr().cast(), &mut tex, std::ptr::null_mut());
        if hr < 0 || tex.is_null() {
            log!("overlay: texture create failed 0x{:08X}", hr as u32);
            return None;
        }
        Some(Tex { ptr: tex as usize, owner: dd, w: tw, h: th })
    }

    /// Copy a w x h image into the top-left of the texture (the rest is cleared).
    pub unsafe fn upload(t: &Tex, w: usize, h: usize, px: &[u32]) -> bool {
        let mut d = [0u32; 31];
        d[0] = 124;
        let hr = call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT;
            t.ptr as P, std::ptr::null_mut(), d.as_mut_ptr().cast(), DDLOCK_WAIT | DDLOCK_WRITEONLY, std::ptr::null_mut());
        if hr < 0 {
            return false;
        }
        let (bits, pitch) = (d[9] as *mut u8, d[4] as usize);
        for y in 0..t.h {
            let row = bits.add(y * pitch) as *mut u32;
            for x in 0..t.w {
                *row.add(x) = if x < w && y < h { px[y * w + x] } else { 0 };
            }
        }
        call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; t.ptr as P, std::ptr::null_mut());
        true
    }

    /// One quad: where (native screen pixels) and how much of the texture (w x h pixels).
    pub struct Quad<'a> {
        pub tex: &'a Tex,
        pub rect: RECT,
        pub w: usize,
        pub h: usize,
    }

    /// Draw premultiplied-alpha quads in their own scene, clipped to `clip`, restoring every
    /// state we change. `point` = nearest-neighbour sampling (whole-number scales).
    pub unsafe fn draw(dev: P, clip: Viewport, point: bool, quads: &[Quad]) {
        if quads.is_empty() {
            return;
        }
        let get_rs: unsafe extern "system" fn(P, u32, *mut u32) -> HRESULT = method(dev, 21);
        let set_rs: unsafe extern "system" fn(P, u32, u32) -> HRESULT = method(dev, 20);
        let get_ts: unsafe extern "system" fn(P, u32, u32, *mut u32) -> HRESULT = method(dev, 36);
        let set_ts: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = method(dev, 37);
        let get_tex: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(dev, 34);
        let set_tex: unsafe extern "system" fn(P, u32, P) -> HRESULT = method(dev, 35);
        let begin: unsafe extern "system" fn(P) -> HRESULT = method(dev, 5);
        // ZENABLE, ZWRITE, ALPHABLEND, SRCBLEND=ONE, DESTBLEND=INVSRCALPHA, ALPHATEST,
        // CULL=NONE, FOG, SPECULAR, COLORKEY, STENCIL
        let rs: [(u32, u32); 11] = [(7, 0), (14, 0), (27, 1), (19, 2), (20, 6), (15, 0), (22, 1), (28, 0), (29, 0), (41, 0), (52, 0)];
        let filter = if point { 1 } else { 2 };
        // stage 0: COLOROP/ALPHAOP = SELECTARG1 of TEXTURE, clamp, filters, coord set 0; stage 1 off
        let ts: [(u32, u32, u32); 12] =
            [(0, 1, 2), (0, 2, 2), (0, 4, 2), (0, 5, 2), (0, 12, 3), (0, 16, filter), (0, 17, filter), (0, 18, 1), (0, 11, 0), (0, 24, 0), (1, 1, 1), (1, 4, 1)];
        let saved_rs: Vec<(u32, u32)> =
            rs.iter().filter_map(|&(k, _)| { let mut v = 0; (get_rs(dev, k, &mut v) >= 0).then_some((k, v)) }).collect();
        let saved_ts: Vec<(u32, u32, u32)> =
            ts.iter().filter_map(|&(st, k, _)| { let mut v = 0; (get_ts(dev, st, k, &mut v) >= 0).then_some((st, k, v)) }).collect();
        let mut old_tex = std::ptr::null_mut();
        get_tex(dev, 0, &mut old_tex);
        let mut old_vp = Viewport { x: 0, y: 0, w: 0, h: 0, min_z: 0.0, max_z: 1.0 };
        let have_vp = call!(DEV_GET_VP as fn(P, *mut Viewport) -> HRESULT; dev, &mut old_vp) >= 0;

        if begin(dev) >= 0 {
            call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &clip);
            for &(k, v) in &rs { set_rs(dev, k, v); }
            for &(st, k, v) in &ts { set_ts(dev, st, k, v); }
            for q in quads {
                let (tu, tv) = (q.w as f32 / q.tex.w as f32, q.h as f32 / q.tex.h as f32);
                let r = &q.rect;
                let (x0, y0, x1, y1) = (r.left as f32 - 0.5, r.top as f32 - 0.5, r.right as f32 - 0.5, r.bottom as f32 - 0.5);
                let quad: [f32; 24] = [x0, y0, 0.0, 1.0, 0.0, 0.0, x1, y0, 0.0, 1.0, tu, 0.0, x0, y1, 0.0, 1.0, 0.0, tv, x1, y1, 0.0, 1.0, tu, tv];
                set_tex(dev, 0, q.tex.ptr as P);
                const TRIANGLESTRIP: u32 = 5; const FVF: u32 = 0x104; // XYZRHW | TEX1
                call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; dev, TRIANGLESTRIP, FVF, quad.as_ptr().cast(), 4, 0);
            }
            call!(DEV_END_SCENE as fn(P) -> HRESULT; dev);
        }
        set_tex(dev, 0, old_tex);
        if !old_tex.is_null() {
            method::<unsafe extern "system" fn(P) -> u32>(old_tex, 2)(old_tex);
        }
        for (k, v) in saved_rs { set_rs(dev, k, v); }
        for (st, k, v) in saved_ts { set_ts(dev, st, k, v); }
        if have_vp {
            call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &old_vp);
        }
    }

    /// The real screen: native size when scaling, else the game's mode.
    pub fn screen() -> (u32, u32) {
        if active() { native() } else { v() }
    }
    pub fn full_screen() -> Viewport {
        let (w, h) = screen();
        Viewport { x: 0, y: 0, w, h, min_z: 0.0, max_z: 1.0 }
    }
}

// ---------------------------------------------------------------- overlay text (code mods)

/// Text drawn on top of the frame for code mods (`draw_text` in the mod API). Each item is
/// rendered with GDI (Segoe UI, greyscale antialiasing) into a texture with a soft drop
/// shadow, then drawn as a quad at Flip. Items are queued during the frame and drawn once.
pub mod overlay {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::*;

    pub struct Item {
        pub x: i32,
        pub y: i32,
        pub text: Vec<u16>,
        pub argb: u32,
        pub size: i32,
        /// 0 = left, 1 = centre, 2 = right of x
        pub align: u32,
    }
    static QUEUE: Mutex<Vec<Item>> = Mutex::new(Vec::new());
    /// Texture per queue slot, reused frame to frame.
    struct Cache(Vec<Option<gpu::Tex>>);
    static CACHE: Mutex<Cache> = Mutex::new(Cache(Vec::new()));
    // The cache only ever lives on the render thread; the Mutex is just for static storage.
    unsafe impl Send for Cache {}

    pub fn queue(item: Item) {
        if let Ok(mut q) = QUEUE.lock() {
            if q.len() < 64 {
                q.push(item);
            }
        }
    }

    /// Render `text` to premultiplied ARGB: (pixels, width, height).
    unsafe fn rasterize(text: &[u16], size: i32, argb: u32) -> Option<(Vec<u32>, usize, usize)> {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        if dc.is_null() {
            return None;
        }
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        const FW_SEMIBOLD: i32 = 600; const ANTIALIASED_QUALITY: u32 = 4;
        let font = CreateFontW(-size, 0, 0, 0, FW_SEMIBOLD, 0, 0, 0, 1 /* DEFAULT_CHARSET */, 0, 0, ANTIALIASED_QUALITY, 0, face.as_ptr());
        let old_font = SelectObject(dc, font as _);
        let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut sz);
        let pad = (size / 10).max(2) as usize;
        let (w, h) = (sz.cx as usize + 2 * pad, sz.cy as usize + 2 * pad);
        let mut result = None;
        if w > 0 && h > 0 && w <= 4096 && h <= 1024 {
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w as i32;
            bi.bmiHeader.biHeight = -(h as i32); // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            let mut bits = std::ptr::null_mut();
            let dib = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if !dib.is_null() && !bits.is_null() {
                let old_bmp = SelectObject(dc, dib as _);
                std::ptr::write_bytes(bits as *mut u8, 0, w * h * 4);
                SetBkMode(dc, 1 /* TRANSPARENT */);
                SetTextColor(dc, 0x00FF_FFFF);
                TextOutW(dc, pad as i32, pad as i32, text.as_ptr(), text.len() as i32);
                GdiFlush();
                // White-on-black coverage -> alpha mask; shadow = mask offset down-right.
                let src = std::slice::from_raw_parts(bits as *const u32, w * h);
                let cover = |x: isize, y: isize| -> u32 {
                    if x < 0 || y < 0 || x >= w as isize || y >= h as isize { 0 } else { (src[y as usize * w + x as usize] >> 8) & 0xFF }
                };
                let (ca, cr, cg, cb) = ((argb >> 24) & 0xFF, (argb >> 16) & 0xFF, (argb >> 8) & 0xFF, argb & 0xFF);
                let off = (size / 14).max(1) as isize;
                let mut out = vec![0u32; w * h];
                for y in 0..h as isize {
                    for x in 0..w as isize {
                        let t = cover(x, y) * ca / 255; // text alpha
                        let s = cover(x - off, y - off) * 200 / 255; // shadow alpha (black)
                        let a = t + s * (255 - t) / 255;
                        let pm = |c: u32| c * t / 255; // premultiplied text colour (shadow is black)
                        out[y as usize * w + x as usize] = (a << 24) | (pm(cr) << 16) | (pm(cg) << 8) | pm(cb);
                    }
                }
                result = Some((out, w, h));
                SelectObject(dc, old_bmp);
                DeleteObject(dib as _);
            }
        }
        SelectObject(dc, old_font);
        DeleteObject(font as _);
        DeleteDC(dc);
        result
    }

    /// Draw everything queued this frame. Called right before Flip.
    pub unsafe fn draw(dev: P) {
        let items = QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
        if items.is_empty() || DDRAW.load(Relaxed) == 0 {
            return;
        }
        let Ok(mut cache) = CACHE.lock() else { return };
        let mut placed = Vec::new();
        for (i, it) in items.iter().enumerate() {
            let Some((px, w, h)) = rasterize(&it.text, it.size.clamp(6, 200), it.argb) else { continue };
            if cache.0.len() <= i {
                cache.0.resize_with(i + 1, || None);
            }
            if !cache.0[i].as_ref().is_some_and(|t| t.fits(w, h)) {
                cache.0[i] = gpu::create(w, h);
            }
            let Some(tex) = cache.0[i].as_ref() else { continue };
            if !gpu::upload(tex, w, h, &px) {
                continue;
            }
            let x = match it.align { 1 => it.x - w as i32 / 2, 2 => it.x - w as i32, _ => it.x };
            placed.push((i, RECT { left: x, top: it.y, right: x + w as i32, bottom: it.y + h as i32 }, w, h));
        }
        let quads: Vec<gpu::Quad> = placed.iter().filter_map(|&(i, rect, w, h)| Some(gpu::Quad { tex: cache.0[i].as_ref()?, rect, w, h })).collect();
        gpu::draw(dev, gpu::full_screen(), false, &quads);
    }
}

// ---------------------------------------------------------------- cursor overlay

/// The game draws its cursor by locking the back buffer: read the 64x64 block under the
/// mouse, alpha-blend the cursor onto it, write it back. Reading a render target back to the
/// CPU stalls the GPU pipeline twice a frame, which halved the frame rate (70 -> 35 fps).
///
/// Instead, the game is handed a plain background: black on even frames, white on odd ones.
/// From one of each, the cursor's exact color (premultiplied) and alpha fall out:
///   black: c0 = a*F          white: c1 = a*F + (1-a)   =>   a = 1 - (c1 - c0), color = c0
/// Nothing is written to the back buffer; the image is drawn as a textured quad at Flip.
mod cursor {
    use super::*;

    /// Locks up to this size are the cursor; bigger ones (screenshots) take the normal path.
    const MAX: usize = 128;

    #[derive(Default)]
    struct State {
        parity: usize,
        /// Composites from the last black (0) and white (1) frame, same size.
        pair: [Option<Vec<u32>>; 2],
        size: (usize, usize),
        /// Premultiplied ARGB cursor image, its size, and whether the texture needs refresh.
        image: Vec<u32>,
        image_size: (usize, usize),
        dirty: bool,
        /// Where the game wanted it this frame (V coords); None = not drawn this frame.
        rect: Option<RECT>,
        /// Current lock: surface, V rect, buffer, write?
        pending: Option<(usize, RECT, Vec<u32>, bool)>,
        tex: Option<gpu::Tex>,
    }
    static STATE: Mutex<Option<State>> = Mutex::new(None);
    // Only touched from the render thread; the Mutex is just for static storage.
    unsafe impl Send for State {}

    fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
        STATE.lock().ok().map(|mut s| f(s.get_or_insert_with(State::default)))
    }

    /// Cursor image from a black-background and a white-background composite of the same
    /// cursor. None if they don't pair up (the cursor changed between the two frames).
    pub fn reconstruct(black: &[u32], white: &[u32]) -> Option<Vec<u32>> {
        black
            .iter()
            .zip(white)
            .map(|(&c0, &c1)| {
                let ch = |v: u32, i: u32| (v >> (8 * i)) & 0xFF;
                let (mut lo, mut hi) = (255u32, 0u32);
                for i in 0..3 {
                    let (b, w) = (ch(c0, i), ch(c1, i));
                    if w < b {
                        return None;
                    }
                    lo = lo.min(w - b);
                    hi = hi.max(w - b);
                }
                // All channels see the same background step (1 - alpha); if they disagree, the
                // two frames show different cursor content (animation, or it changed).
                if hi - lo > 3 {
                    return None;
                }
                Some(((255 - lo) << 24) | (c0 & 0x00FF_FFFF))
            })
            .collect()
    }

    pub unsafe fn lock(surf: P, r: *mut RECT, desc: *mut c_void, flags: u32) -> Option<HRESULT> {
        let vr = *r.as_ref()?;
        let (w, h) = ((vr.right - vr.left).max(0) as usize, (vr.bottom - vr.top).max(0) as usize);
        if w == 0 || h == 0 || w > MAX || h > MAX {
            return None;
        }
        let hr = call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; surf, desc);
        if hr < 0 || rd(desc, 84) != 32 {
            return None;
        }
        with(|s| {
            let bg = if s.parity == 0 { 0xFF00_0000 } else { 0xFFFF_FFFF };
            let mut buf = vec![bg; w * h];
            if active() {
                report_v(desc);
            }
            const DDSD_PITCH: u32 = 8; const DDSD_LPSURFACE: u32 = 0x800;
            let d = desc as *mut u32;
            *d.add(1) |= DDSD_PITCH | DDSD_LPSURFACE;
            *d.add(4) = (w * 4) as u32;
            *d.add(9) = buf.as_mut_ptr() as u32;
            s.pending = Some((surf as usize, vr, buf, flags & DDLOCK_READONLY == 0));
            0
        })
    }

    pub unsafe fn unlock(surf: P) -> bool {
        with(|s| {
            let Some((_, vr, buf, write)) = s.pending.take_if(|p| p.0 == surf as usize) else { return false };
            if write {
                let size = ((vr.right - vr.left) as usize, (vr.bottom - vr.top) as usize);
                // At a screen edge the game clips the rect, so the image sits at a different
                // offset in a smaller rect: learn the image only from unclipped rects, and at
                // the edges draw the last full image at its unclipped position (see draw).
                let (vw, vh) = v();
                // The game clips one pixel short of the right/bottom edge (right = 799 of 800),
                // so allow some slack; a missed clip made it learn a shifted, half-white image.
                const SLACK: i32 = 2;
                let clipped = vr.left <= SLACK || vr.top <= SLACK || vr.right >= vw as i32 - SLACK || vr.bottom >= vh as i32 - SLACK;
                if clipped {
                    s.rect = Some(vr);
                    s.pair = [None, None];
                    return true;
                }
                if s.size != size {
                    s.pair = [None, None];
                    s.size = size;
                }
                s.pair[s.parity] = Some(buf);
                s.parity ^= 1;
                if let [Some(b), Some(w)] = &s.pair {
                    if let Some(img) = reconstruct(b, w) {
                        if img != s.image || s.image_size != size {
                            s.image = img;
                            s.image_size = size;
                            s.dirty = true;
                        }
                    }
                }
                s.rect = Some(vr);
            }
            true
        })
        .unwrap_or(false)
    }

    /// Draw this frame's cursor (if the game drew one) as a quad. Called right before Flip.
    pub unsafe fn draw(dev: P) {
        with(|s| {
            let Some(mut vr) = s.rect.take() else { return };
            if s.image.is_empty() || DDRAW.load(Relaxed) == 0 {
                return;
            }
            // Clipped at an edge: extend back to the full image size on the clipped side(s);
            // the part beyond the screen just falls off.
            let (fw, fh) = (s.image_size.0 as i32, s.image_size.1 as i32);
            if vr.right - vr.left < fw {
                if vr.left <= 2 { vr.left = vr.right - fw } else { vr.right = vr.left + fw }
            }
            if vr.bottom - vr.top < fh {
                if vr.top <= 2 { vr.top = vr.bottom - fh } else { vr.bottom = vr.top + fh }
            }
            let (w, h) = s.image_size;
            if !s.tex.as_ref().is_some_and(|t| t.fits(w, h)) {
                s.tex = gpu::create(w, h);
                s.dirty = true;
            }
            let Some(tex) = s.tex.as_ref() else { return };
            if s.dirty {
                s.dirty = !gpu::upload(tex, w, h, &s.image);
            }
            let t = xf();
            // Clip to the picture: in letterboxed modes (4:3 menus) nothing clears the side
            // bars, so a cursor drawn there would leave a trail of copies.
            let c = content_rect(t);
            let clip = Viewport { x: c.left as u32, y: c.top as u32, w: (c.right - c.left) as u32, h: (c.bottom - c.top) as u32, min_z: 0.0, max_z: 1.0 };
            gpu::draw(dev, clip, t.s.fract() == 0.0, &[gpu::Quad { tex, rect: scale_rect(&vr, t), w, h }]);
        });
    }
}

// ---------------------------------------------------------------- shadow redirect

/// Character shadows, as the game does them (once per character, every frame):
///   1. Blt colour-fill of a corner of the back buffer, rect (0,0,64,64), white
///   2. SetViewport(0,0,64,64) and draw the character's silhouette (ordinary 3D draws)
///   3. BltFast(shadow texture <- that corner); the texture lives in system memory
///   ...and after the last one, a colour-fill of the corner back to the fog colour.
/// Step 3 makes dxwrapper read the whole frame back to the CPU and upload it again, per
/// character: that was the main frame-rate cost (~45 vs ~110 fps).
///
/// Redirected: (1) is skipped and its colour remembered; at (2) rendering switches to a
/// video-memory render-target texture of `res()` pixels, cleared to that colour, with the
/// viewport stretched over it; (3) switches back to the back buffer and maps the game's shadow
/// texture to ours, without copying anything; SetTexture(game texture) binds ours instead. Bonus:
/// the silhouette is rendered at `res()` instead of 64 px, so shadows can be sharper.
pub mod shadow {
    use super::*;

    /// Shadow texture size in pixels; 0 = feature off (the game's own copy path).
    /// DESIEGE_SHADOW_RES, or a `shadow_res` file next to this DLL (launcher writes it).
    pub fn res() -> u32 {
        static R: OnceLock<u32> = OnceLock::new();
        let r = *R.get_or_init(|| {
            let from_file = || std::fs::read_to_string(crate::self_dir()?.join("shadow_res")).ok();
            let r = std::env::var("DESIEGE_SHADOW_RES").ok().or_else(from_file).and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(128);
            let r = if r == 0 { 0 } else { r.clamp(32, 1024).next_power_of_two() };
            log!("shadow redirect: {}", if r == 0 { "off".to_string() } else { format!("{r}x{r}") });
            r
        });
        if DISABLED.load(Relaxed) { 0 } else { r }
    }
    /// Set if render-target textures can't be created; the game's path is used from then on.
    static DISABLED: AtomicBool = AtomicBool::new(false);

    #[derive(Default)]
    struct State {
        /// Pending corner fill: V rect and colour.
        corner: Option<(RECT, u32)>,
        /// Our texture being rendered into, and the back buffer to return to.
        capturing: Option<(usize, usize)>,
        free: Vec<usize>,
        /// Game shadow texture -> our texture.
        map: Vec<(usize, usize)>,
    }
    static ST: Mutex<Option<State>> = Mutex::new(None);
    fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
        ST.lock().ok().map(|mut s| f(s.get_or_insert_with(State::default)))
    }

    unsafe fn method<F>(obj: P, idx: usize) -> F {
        std::mem::transmute_copy(&*(*(obj as *const *const usize)).add(idx))
    }
    unsafe fn release(obj: usize) {
        if obj != 0 {
            method::<unsafe extern "system" fn(P) -> u32>(obj as P, 2)(obj as P);
        }
    }

    /// New render-target texture in the back buffer's pixel format.
    unsafe fn create_target(back: P) -> Option<usize> {
        let mut b = [0u32; 31];
        b[0] = 124;
        if call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; back, b.as_mut_ptr().cast()) < 0 {
            return None;
        }
        let r = res();
        let mut d = [0u32; 31];
        d[0] = 124;
        d[1] = 1 | 2 | 4 | 0x1000; // CAPS | HEIGHT | WIDTH | PIXELFORMAT
        d[2] = r;
        d[3] = r;
        d[18..26].copy_from_slice(&b[18..26]); // ddpfPixelFormat @72
        d[26] = 0x1000 | 0x2000 | 0x4000; // TEXTURE | 3DDEVICE | VIDEOMEMORY
        let create: unsafe extern "system" fn(P, *mut c_void, *mut P, P) -> HRESULT =
            std::mem::transmute(crate::DD_CREATE_SURFACE.load(Relaxed));
        let mut t = std::ptr::null_mut();
        let hr = create(DDRAW.load(Relaxed) as P, d.as_mut_ptr().cast(), &mut t, std::ptr::null_mut());
        if hr < 0 || t.is_null() {
            log!("shadow redirect: render target create failed 0x{:08X}; using the game's path", hr as u32);
            DISABLED.store(true, Relaxed);
            return None;
        }
        Some(t as usize)
    }

    /// Blt on a main surface. True = handled (skip the real fill).
    pub unsafe fn on_colorfill(this: P, dst: *mut RECT, flags: u32, fx: P) -> bool {
        const COLORFILL: u32 = 0x400;
        if res() == 0 || flags & COLORFILL == 0 || fx.is_null() || !is_main(this) {
            return false;
        }
        let Some(r) = dst.as_ref().copied() else { return false };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if r.left != 0 || r.top != 0 || w <= 0 || h <= 0 || w > 256 || h > 256 {
            return false;
        }
        // Corner fills only ever matter for shadows: with the redirect the corner is never
        // drawn into, so the final "restore to fog colour" fill is skipped too.
        with(|s| s.corner = Some((r, rd(fx, 80)))).is_some()
    }

    fn is_corner(vp: &Viewport, r: &RECT) -> bool {
        vp.x == 0 && vp.y == 0 && vp.w as i32 == r.right - r.left && vp.h as i32 == r.bottom - r.top
    }

    unsafe fn end_capture(dev: P, s: &mut State) {
        if let Some((_, back)) = s.capturing.take() {
            call!(DEV_SET_RT as fn(P, P, u32) -> HRESULT; dev, back as P, 0);
            RT_MAIN.store(is_main(back as P), Relaxed);
            release(back); // GetRenderTarget's reference
        }
    }

    /// SetViewport. Some(hr) = handled.
    pub unsafe fn on_set_viewport(dev: P, vp: *const Viewport) -> Option<HRESULT> {
        if res() == 0 {
            return None;
        }
        let vp = *vp.as_ref()?;
        with(|s| {
            let (corner, color) = s.corner?;
            if !is_corner(&vp, &corner) {
                end_capture(dev, s); // anything else: back to normal rendering
                return None;
            }
            let r = res();
            let ours = Viewport { x: 0, y: 0, w: r, h: r, ..vp };
            if s.capturing.is_none() {
                let get_rt: unsafe extern "system" fn(P, *mut P) -> HRESULT = method(dev, 9);
                let mut back = std::ptr::null_mut();
                if get_rt(dev, &mut back) < 0 || back.is_null() {
                    return None;
                }
                let target = match s.free.pop() {
                    Some(t) => t,
                    None => match create_target(back) {
                        Some(t) => t,
                        None => {
                            release(back as usize);
                            return None;
                        }
                    },
                };
                if call!(DEV_SET_RT as fn(P, P, u32) -> HRESULT; dev, target as P, 0) < 0 {
                    s.free.push(target);
                    release(back as usize);
                    return None;
                }
                RT_MAIN.store(false, Relaxed);
                s.capturing = Some((target, back as usize));
                call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &ours);
                const D3DCLEAR_TARGET: u32 = 1;
                call!(DEV_CLEAR as fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT; dev, 0, std::ptr::null(), D3DCLEAR_TARGET, color, 1.0, 0);
            }
            Some(call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &ours))
        })
        .flatten()
    }

    /// BltFast(texture <- back buffer). Some(hr) = handled (texture now maps to our capture).
    pub unsafe fn on_copy_out(tex: P, src: P) -> Option<HRESULT> {
        if res() == 0 || !is_main(src) || is_main(tex) {
            return None;
        }
        let dev = DEVICE.load(Relaxed) as P;
        with(|s| {
            let (target, _) = s.capturing?;
            end_capture(dev, s);
            match s.map.iter_mut().find(|m| m.0 == tex as usize) {
                Some(m) => {
                    let old = std::mem::replace(&mut m.1, target);
                    s.free.push(old);
                }
                None => s.map.push((tex as usize, target)),
            }
            s.corner = None;
            dump(target);
            Some(0)
        })
        .flatten()
    }

    /// Debug: DESIEGE_DUMP_SHADOWS=1 writes the first few captured shadows to
    /// %LOCALAPPDATA%\Desiege\shadow_<n>.bmp.
    unsafe fn dump(target: usize) {
        static N: AtomicUsize = AtomicUsize::new(0);
        if std::env::var_os("DESIEGE_DUMP_SHADOWS").is_none() || N.load(Relaxed) >= 3 {
            return;
        }
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        if SEEN.fetch_add(1, Relaxed) < 2000 {
            return; // skip loading-time shadows
        }
        let n = N.fetch_add(1, Relaxed);
        let mut d = [0u32; 31];
        d[0] = 124;
        if call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; target as P, std::ptr::null_mut(), d.as_mut_ptr().cast(), DDLOCK_WAIT | DDLOCK_READONLY, std::ptr::null_mut()) < 0 {
            log!("shadow dump: lock failed");
            return;
        }
        let (h, w, pitch, bits) = (d[2] as usize, d[3] as usize, d[4] as usize, d[9] as *const u8);
        let mut bmp = Vec::new();
        bmp.extend_from_slice(b"BM");
        for v in [(54 + w * h * 4) as u32, 0, 54, 40, w as u32, (-(h as i32)) as u32] { bmp.extend_from_slice(&v.to_le_bytes()); }
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        for _ in 0..6 { bmp.extend_from_slice(&0u32.to_le_bytes()); }
        for y in 0..h {
            bmp.extend_from_slice(std::slice::from_raw_parts(bits.add(y * pitch), w * 4));
        }
        call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; target as P, std::ptr::null_mut());
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            let _ = std::fs::write(std::path::PathBuf::from(dir).join("Desiege").join(format!("shadow_{n}.bmp")), &bmp);
            log!("shadow dump: {w}x{h} #{n}");
        }
    }

    /// Texture to really bind when the game binds `tex`.
    pub fn substitute(tex: P) -> P {
        if tex.is_null() {
            return tex;
        }
        with(|s| s.map.iter().find(|m| m.0 == tex as usize).map(|m| m.1 as P)).flatten().unwrap_or(tex)
    }

    /// The game released a surface; if it was a mapped shadow texture that's gone now, recycle
    /// ours (so a new texture reusing the pointer doesn't show a stale shadow).
    pub fn released(surf: P, refs_left: u32) {
        if refs_left != 0 {
            return;
        }
        with(|s| {
            if let Some(i) = s.map.iter().position(|m| m.0 == surf as usize) {
                let (_, ours) = s.map.swap_remove(i);
                s.free.push(ours);
            }
        });
    }

    /// Mode change: our textures died with the old DirectDraw object.
    pub fn reset() {
        with(|s| *s = State::default()); // ponytail: not Released; their owner is gone
    }
}

// ---------------------------------------------------------------- one-frame trace (debug)

/// DESIEGE_TRACE_FRAME=<n>: log every hooked call of frame n (counted in Flips).
static TRACING: AtomicBool = AtomicBool::new(false);
fn trace(msg: impl FnOnce() -> String) {
    if TRACING.load(Relaxed) {
        log!("  trace: {}", msg());
    }
}
fn trace_frame_boundary() {
    static FRAME: AtomicUsize = AtomicUsize::new(0);
    static TARGET: OnceLock<Option<usize>> = OnceLock::new();
    let f = FRAME.fetch_add(1, Relaxed) + 1;
    if let Some(t) = *TARGET.get_or_init(|| std::env::var("DESIEGE_TRACE_FRAME").ok().and_then(|v| v.parse().ok())) {
        if f == t { log!("trace: frame {t} begins"); }
        TRACING.store(f == t, Relaxed);
        if f == t + 1 { log!("trace: frame {t} ends"); }
    }
}
fn mm(s: P) -> &'static str { if is_main(s) { "(main)" } else { "" } }
unsafe fn r_(r: *mut RECT) -> Option<(i32, i32, i32, i32)> { r.as_ref().map(|r| (r.left, r.top, r.right, r.bottom)) }

// ---------------------------------------------------------------- input

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

/// Debug: logs the first 15 input mappings (enough to spot a mapping bug).
fn dbg_input(msg: impl FnOnce() -> String) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Relaxed);
    if n < 15 {
        log!("input #{n}: {}", msg());
    }
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
pub unsafe fn install_input_hooks() {
    if !decoupling() {
        return;
    }
    let exe = GetModuleHandleW(std::ptr::null()) as usize;
    let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
    let u = GetModuleHandleW(user32.as_ptr()) as usize;
    hook_iat(exe, proc_in(u, c"GetCursorPos"), get_cursor_pos as usize, &GET_CURSOR_POS);
    hook_iat(exe, proc_in(u, c"SetCursorPos"), set_cursor_pos as usize, &SET_CURSOR_POS);
    hook_iat(exe, proc_in(u, c"GetClientRect"), get_client_rect as usize, &GET_CLIENT_RECT);
    hook_iat(exe, proc_in(u, c"GetSystemMetrics"), get_system_metrics as usize, &GET_SYSTEM_METRICS);
    log!("ui scale: input hooks installed (native {:?})", native());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_from_black_and_white() {
        // opaque red, half-transparent white, fully transparent
        let black = [0xFFFF_0000u32, 0xFF80_8080, 0xFF00_0000];
        let white = [0xFFFF_0000u32, 0xFFFF_FFFF, 0xFFFF_FFFF];
        let img = cursor::reconstruct(&black, &white).unwrap();
        assert_eq!(img, [0xFFFF_0000, 0x8080_8080, 0x0000_0000]);
        assert_eq!(cursor::reconstruct(&[0xFF10_1010], &[0xFF00_0000]), None); // not a pair
        // channels disagree on the background step: different cursor content in the two frames
        assert_eq!(cursor::reconstruct(&[0xFF00_0000], &[0xFFFF_0000]), None);
    }

    #[test]
    fn fvf_strides() {
        assert_eq!(fvf_stride(0x1C4), 32); // D3DFVF_TLVERTEX: XYZRHW | DIFFUSE | SPECULAR | TEX1
        assert_eq!(fvf_stride(0x112), 32); // D3DFVF_VERTEX:   XYZ | NORMAL | TEX1
        assert_eq!(fvf_stride(0x0244 | (1 << 16)), 16 + 4 + 12 + 8); // XYZRHW|DIFFUSE|TEX2, coord0 3D
    }

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

    #[test]
    fn scales_tl_vertices() {
        let verts: [f32; 6] = [10.0, 20.0, 0.5, 1.0, 99.0, 98.0]; // one XYZRHW|TEX1 vertex (24 bytes)
        let out = unsafe { scaled_tl(0x104, verts.as_ptr().cast(), 1, Xf { s: 2.0, ox: 0.0, oy: 0.0 }) };
        let f: Vec<f32> = out.chunks(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(f, [20.5, 40.5, 0.5, 1.0, 99.0, 98.0]);
    }
}
