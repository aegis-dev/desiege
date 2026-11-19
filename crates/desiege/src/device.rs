//! IDirect3DDevice7 hooks. The game's calls are dispatched to the features that care:
//! - SetViewport / GetViewport / Clear: UI scale (V -> N), GPU shadows (corner capture)
//! - SetRenderTarget, SetTexture: GPU shadows
//! - Draw*: UI scale (2D vertices V -> N), dynamic vertex buffer mirrors
//! - EndScene: frame counting, stats, profiler, snapshot timing
//!
//! Texture formats and sampler states are hooked by gfx.rs.

use crate::{ao, debug::*, dynvb, method, patch, scale::*, shadow, HRESULT, P};
use std::{
    ffi::c_void,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};
use windows_sys::Win32::Foundation::RECT;

orig!(pub(crate) DEV_END_SCENE: fn(P) -> HRESULT);
orig!(pub(crate) DEV_SET_RT: fn(P, P, u32) -> HRESULT);
orig!(pub(crate) DEV_CLEAR: fn(P, u32, *const RECT, u32, u32, f32, u32) -> HRESULT);
orig!(pub(crate) DEV_SET_VP: fn(P, *const Viewport) -> HRESULT);
orig!(pub(crate) DEV_GET_VP: fn(P, *mut Viewport) -> HRESULT);
orig!(pub(crate) DEV_DP: fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT);
orig!(DEV_DIP: fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT);
orig!(DEV_DPS: fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT);
orig!(DEV_DIPS: fn(P, u32, u32, *const c_void, u32, *const u16, u32, u32) -> HRESULT);
orig!(DEV_DPVB: fn(P, u32, P, u32, u32, u32) -> HRESULT);
orig!(DEV_DIPVB: fn(P, u32, P, u32, u32, *const u16, u32, u32) -> HRESULT);
orig!(DEV_SET_XFORM: fn(P, u32, *const f32) -> HRESULT);
orig!(DEV_BEGIN_SCENE: fn(P) -> HRESULT);
orig!(pub(crate) DEV_SET_TEX: fn(P, u32, P) -> HRESULT);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Viewport { pub x: u32, pub y: u32, pub w: u32, pub h: u32, pub min_z: f32, pub max_z: f32 }

/// The game's (latest) device.
static DEVICE: AtomicUsize = AtomicUsize::new(0);
pub fn current() -> P { DEVICE.load(Relaxed) as P }

pub unsafe fn created(dev: P, rt: P) {
    RT_MAIN.store(is_main(rt), Relaxed);
    DEVICE.store(dev as usize, Relaxed);
    patch(dev, 5, dev_begin_scene as usize, &DEV_BEGIN_SCENE);
    patch(dev, 6, dev_end_scene as usize, &DEV_END_SCENE);
    patch(dev, 8, dev_set_rt as usize, &DEV_SET_RT);
    patch(dev, 10, dev_clear as usize, &DEV_CLEAR);
    patch(dev, 11, dev_set_transform as usize, &DEV_SET_XFORM);
    patch(dev, 13, dev_set_vp as usize, &DEV_SET_VP);
    patch(dev, 15, dev_get_vp as usize, &DEV_GET_VP);
    patch(dev, 25, dev_dp as usize, &DEV_DP);
    patch(dev, 26, dev_dip as usize, &DEV_DIP);
    patch(dev, 29, dev_dps as usize, &DEV_DPS);
    patch(dev, 30, dev_dips as usize, &DEV_DIPS);
    patch(dev, 31, dev_dpvb as usize, &DEV_DPVB);
    patch(dev, 32, dev_dipvb as usize, &DEV_DIPVB);
    patch(dev, 35, dev_set_texture as usize, &DEV_SET_TEX);
}

/// Before every draw: what AO needs to know (3D world drawn to the back buffer; first 2D
/// draw after it = the UI starts, AO goes in).
unsafe fn before_draw(fvf: u32) {
    if RT_MAIN.load(Relaxed) {
        if is_tl(fvf) {
            ao::before_2d()
        } else {
            ao::world_draw();
            crate::lights::world_draw();
        }
    }
}

/// Real SetViewport on the device, remembered for AO (native pixels).
unsafe fn set_viewport(dev: P, vp: *const Viewport) -> HRESULT {
    if let Some(v) = vp.as_ref() {
        if RT_MAIN.load(Relaxed) {
            ao::set_viewport(v.x, v.y, v.w, v.h);
        }
    }
    call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, vp)
}

/// DESIEGE_EXPERIMENT=vanilla: only the frame counter (for "before" benchmarks).
pub unsafe fn measure(dev: P) {
    patch(dev, 6, dev_end_scene as usize, &DEV_END_SCENE);
}

// ---------------------------------------------------------------- vertex formats

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
/// Pre-transformed (screen-space) vertices: the game's 2D.
pub fn is_tl(fvf: u32) -> bool { fvf & 0xE == XYZRHW }

/// IDirect3DVertexBuffer7: GetVertexBufferDesc (slot 6) -> D3DVERTEXBUFFERDESC { size, caps, fvf, count }.
unsafe fn vb_fvf(vb: P) -> u32 {
    let mut d = [16u32, 0, 0, 0];
    let get: unsafe extern "system" fn(P, *mut u32) -> HRESULT = method(vb, 6);
    if get(vb, d.as_mut_ptr()) >= 0 { d[2] } else { 0 }
}

/// Copy of vertices [start, start+num) of a TL vertex buffer, scaled. Lock = slot 3, Unlock = 4.
unsafe fn vb_scaled(vb: P, fvf: u32, start: u32, num: u32) -> Option<Vec<u8>> {
    let lock: unsafe extern "system" fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT = method(vb, 3);
    let unlock: unsafe extern "system" fn(P) -> HRESULT = method(vb, 4);
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

// ---------------------------------------------------------------- hooks

unsafe extern "system" fn dev_set_transform(this: P, kind: u32, m: *const f32) -> HRESULT {
    trace(|| format!("SetTransform({kind}) m00={:.3} m11={:.3} m22={:.3} m32={:.3}", *m, *m.add(5), *m.add(10), *m.add(14)));
    const PROJECTION: u32 = 3;
    if kind == PROJECTION && !m.is_null() {
        ao::set_projection(m);
    }
    if !m.is_null() {
        crate::lights::set_transform(kind, m);
    }
    call!(DEV_SET_XFORM as fn(P, u32, *const f32) -> HRESULT; this, kind, m)
}

unsafe extern "system" fn dev_begin_scene(this: P) -> HRESULT {
    trace(|| "BeginScene".into());
    call!(DEV_BEGIN_SCENE as fn(P) -> HRESULT; this)
}

unsafe extern "system" fn dev_set_texture(this: P, stage: u32, tex: P) -> HRESULT {
    trace(|| format!("SetTexture({stage}, {tex:?})"));
    call!(DEV_SET_TEX as fn(P, u32, P) -> HRESULT; this, stage, shadow::substitute(tex))
}

unsafe extern "system" fn dev_end_scene(this: P) -> HRESULT {
    trace(|| "EndScene".into());
    count_frame();
    let hr = call!(DEV_END_SCENE as fn(P) -> HRESULT; this);
    scene_ended();
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

unsafe extern "system" fn dev_set_vp(this: P, vp: *const Viewport) -> HRESULT {
    trace(|| format!("SetViewport({:?})", vp.as_ref()));
    if let Some(hr) = shadow::on_set_viewport(this, vp) {
        return hr;
    }
    if !scaling() || vp.is_null() {
        return set_viewport(this, vp);
    }
    if let Ok(mut g) = GAME_VP.lock() {
        *g = Some(*vp);
    }
    let scaled = scale_vp(&*vp, xf());
    set_viewport(this, &scaled)
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

unsafe extern "system" fn dev_dp(this: P, prim: u32, fvf: u32, verts: *const c_void, count: u32, flags: u32) -> HRESULT {
    before_draw(fvf);
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
    before_draw(fvf);
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

unsafe extern "system" fn dev_dpvb(this: P, prim: u32, vb: P, start: u32, num: u32, flags: u32) -> HRESULT {
    before_draw(dynvb::range(vb, start, num).map_or_else(|| if vb.is_null() { 0 } else { vb_fvf(vb) }, |(_, f)| f));
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
    before_draw(dynvb::range(vb, start, num).map_or_else(|| if vb.is_null() { 0 } else { vb_fvf(vb) }, |(_, f)| f));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fvf_strides() {
        assert_eq!(fvf_stride(0x1C4), 32); // D3DFVF_TLVERTEX: XYZRHW | DIFFUSE | SPECULAR | TEX1
        assert_eq!(fvf_stride(0x112), 32); // D3DFVF_VERTEX:   XYZ | NORMAL | TEX1
        assert_eq!(fvf_stride(0x0244 | (1 << 16)), 16 + 4 + 12 + 8); // XYZRHW|DIFFUSE|TEX2, coord0 3D
    }
}
