//! IDirectDrawSurface7 hooks on the primary chain (front, back buffer, z-buffer). The game's
//! calls are dispatched to the features that care:
//! - Blt / BltFast: GPU shadows (corner fills, copy-outs), UI scale (V rects -> N rects)
//! - Lock / Unlock: cursor overlay, UI scale lock emulation
//! - Flip: end of frame (code mods, overlay text, cursor, snapshots)
//! - Release: GPU shadows forget textures that are gone

use crate::{code_mods, cursor, debug::*, device, overlay, patch, rd, scale::*, shadow, HRESULT, P};
use std::{
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};
use windows_sys::Win32::Foundation::RECT;

orig!(pub(crate) SF_BLT: fn(P, *mut RECT, P, *mut RECT, u32, P) -> HRESULT);
orig!(SF_BLTFAST: fn(P, u32, u32, P, *mut RECT, u32) -> HRESULT);
orig!(SF_GET_ATTACHED: fn(P, P, *mut P) -> HRESULT);
orig!(pub(crate) SF_GET_DESC: fn(P, *mut c_void) -> HRESULT);
orig!(pub(crate) SF_LOCK: fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT);
orig!(pub(crate) SF_UNLOCK: fn(P, *mut RECT) -> HRESULT);
orig!(SF_FLIP: fn(P, P, u32) -> HRESULT);
orig!(SF_RELEASE: fn(P) -> u32);

pub const DDLOCK_WAIT: u32 = 1;
pub const DDLOCK_READONLY: u32 = 0x10;
pub const DDLOCK_WRITEONLY: u32 = 0x20;

/// Hook a surface of the primary chain (vtables are per class: this hooks every surface).
pub unsafe fn hook(s: P) {
    patch(s, 2, sf_release as usize, &SF_RELEASE);
    patch(s, 5, sf_blt as usize, &SF_BLT);
    patch(s, 7, sf_bltfast as usize, &SF_BLTFAST);
    patch(s, 11, sf_flip as usize, &SF_FLIP);
    patch(s, 12, sf_get_attached as usize, &SF_GET_ATTACHED);
    patch(s, 22, sf_get_desc as usize, &SF_GET_DESC);
    patch(s, 25, sf_lock as usize, &SF_LOCK);
    patch(s, 32, sf_unlock as usize, &SF_UNLOCK);
}

/// A surface's DDSURFACEDESC2 as dwords (dwHeight = [2], dwWidth = [3], pitch = [4],
/// lpSurface = [9], ddpfPixelFormat = [18..26], ddsCaps = [26..]), unhooked. None on failure.
pub unsafe fn desc(surf: P) -> Option<[u32; 31]> {
    let mut d = [0u32; 31];
    d[0] = 124;
    (call!(SF_GET_DESC as fn(P, *mut c_void) -> HRESULT; surf, d.as_mut_ptr().cast()) >= 0).then_some(d)
}

/// Real (unhooked) lock of a rect: (bits, pitch in bytes, bpp).
pub unsafe fn lock_rect(surf: P, r: Option<&RECT>, flags: u32) -> Option<(*mut u8, usize, u32)> {
    let mut d = [0u32; 31];
    d[0] = 124;
    let mut copy = r.copied();
    let rp = copy.as_mut().map_or(std::ptr::null_mut(), |r| r as *mut RECT);
    let hr = call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; surf, rp, d.as_mut_ptr().cast(), flags | DDLOCK_WAIT, std::ptr::null_mut());
    (hr >= 0).then(|| (d[9] as *mut u8, d[4] as usize, d[21]))
}

/// Real (unhooked) unlock, of a rect or the whole surface.
pub unsafe fn unlock_rect(surf: P, r: Option<&RECT>) {
    let mut copy = r.copied();
    let rp = copy.as_mut().map_or(std::ptr::null_mut(), |r| r as *mut RECT);
    call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; surf, rp);
}

unsafe extern "system" fn sf_release(this: P) -> u32 {
    let left = call!(SF_RELEASE as fn(P) -> u32; this);
    shadow::released(this, left);
    left
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
            let (d, sd) = (desc(this).unwrap_or_default(), if src.is_null() { [0; 31] } else { desc(src).unwrap_or_default() });
            let r = |p: *mut RECT| p.as_ref().map(|r| (r.left, r.top, r.right, r.bottom));
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
            let d = desc(this).unwrap_or_default();
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
            let d = desc(src).unwrap_or_default();
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

unsafe extern "system" fn sf_lock(this: P, r: *mut RECT, desc: *mut c_void, flags: u32, ev: P) -> HRESULT {
    if !is_main(this) || desc.is_null() {
        return call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; this, r, desc, flags, ev);
    }
    stat(LOCK_MAIN);
    if let Some(hr) = cursor::lock(this, r, desc, flags) {
        return hr;
    }
    if let Some(hr) = emulated_lock(this, r, desc, flags) {
        return hr;
    }
    call!(SF_LOCK as fn(P, *mut RECT, *mut c_void, u32, P) -> HRESULT; this, r, desc, flags, ev)
}

unsafe extern "system" fn sf_unlock(this: P, r: *mut RECT) -> HRESULT {
    if cursor::unlock(this) || emulated_unlock(this) {
        return 0;
    }
    call!(SF_UNLOCK as fn(P, *mut RECT) -> HRESULT; this, r)
}

unsafe extern "system" fn sf_flip(this: P, target: P, flags: u32) -> HRESULT {
    trace(|| "Flip".into());
    trace_frame_boundary();
    stat(FLIPS);
    let dev = device::current();
    if !dev.is_null() {
        code_mods::run_frame();
        overlay::draw(dev);
        cursor::draw(dev);
        snapshot_if_due(dev); // after the cursor, so snapshots show it
    }
    call!(SF_FLIP as fn(P, P, u32) -> HRESULT; this, target, flags)
}
