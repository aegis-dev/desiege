//! Our own GPU work on the game's device: creating surfaces on the game's DirectDraw object,
//! ARGB textures, drawing textured quads, and a guard that sets render/texture states and puts
//! the game's back afterwards. Used by the cursor overlay, code mods' overlay text and GPU
//! shadows.

use crate::{device::*, method, scale::active, scale::native, scale::v, surface::*, HRESULT, P, DDRAW};
use std::{
    ffi::c_void,
    sync::atomic::Ordering::Relaxed,
};
use windows_sys::Win32::Foundation::RECT;

/// CreateSurface on the game's current DirectDraw object (unhooked) from a DDSURFACEDESC2.
pub unsafe fn create_surface(d: &mut [u32; 31]) -> Result<P, HRESULT> {
    let create: unsafe extern "system" fn(P, *mut c_void, *mut P, P) -> HRESULT = std::mem::transmute(crate::DD_CREATE_SURFACE.load(Relaxed));
    let mut s = std::ptr::null_mut();
    let hr = create(DDRAW.load(Relaxed) as P, d.as_mut_ptr().cast(), &mut s, std::ptr::null_mut());
    if hr < 0 || s.is_null() { Err(hr) } else { Ok(s) }
}

pub unsafe fn release(obj: P) {
    if !obj.is_null() {
        method::<unsafe extern "system" fn(P) -> u32>(obj, 2)(obj);
    }
}

// ---------------------------------------------------------------- state guard

/// Sets render states (IDirect3DDevice7 slot 20) and texture stage states (slot 37) and the
/// stage-0 texture, and restores the game's values on drop.
pub struct States {
    dev: P,
    rs: Vec<(u32, u32)>,
    ts: Vec<(u32, u32, u32)>,
    tex: P,
}

impl States {
    pub unsafe fn set(dev: P, rs: &[(u32, u32)], ts: &[(u32, u32, u32)], tex: P) -> States {
        let get_rs: unsafe extern "system" fn(P, u32, *mut u32) -> HRESULT = method(dev, 21);
        let set_rs: unsafe extern "system" fn(P, u32, u32) -> HRESULT = method(dev, 20);
        let get_ts: unsafe extern "system" fn(P, u32, u32, *mut u32) -> HRESULT = method(dev, 36);
        let set_ts: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = method(dev, 37);
        let get_tex: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(dev, 34);
        let saved_rs = rs.iter().filter_map(|&(k, _)| { let mut v = 0; (get_rs(dev, k, &mut v) >= 0).then_some((k, v)) }).collect();
        let saved_ts = ts.iter().filter_map(|&(st, k, _)| { let mut v = 0; (get_ts(dev, st, k, &mut v) >= 0).then_some((st, k, v)) }).collect();
        let mut old_tex = std::ptr::null_mut();
        get_tex(dev, 0, &mut old_tex); // AddRef'd; released on drop
        for &(k, v) in rs { set_rs(dev, k, v); }
        for &(st, k, v) in ts { set_ts(dev, st, k, v); }
        call!(DEV_SET_TEX as fn(P, u32, P) -> HRESULT; dev, 0, tex);
        States { dev, rs: saved_rs, ts: saved_ts, tex: old_tex }
    }

    /// Change the stage-0 texture inside the guarded block.
    pub unsafe fn texture(&self, tex: P) {
        call!(DEV_SET_TEX as fn(P, u32, P) -> HRESULT; self.dev, 0, tex);
    }
}

impl Drop for States {
    fn drop(&mut self) {
        unsafe {
            let set_rs: unsafe extern "system" fn(P, u32, u32) -> HRESULT = method(self.dev, 20);
            let set_ts: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = method(self.dev, 37);
            call!(DEV_SET_TEX as fn(P, u32, P) -> HRESULT; self.dev, 0, self.tex);
            release(self.tex);
            for &(k, v) in &self.rs { set_rs(self.dev, k, v); }
            for &(st, k, v) in &self.ts { set_ts(self.dev, st, k, v); }
        }
    }
}

/// Render states for drawing 2D quads over whatever is there: no z, no fog/specular/colour
/// key/stencil/alpha test, no culling, alpha blending with the given (src, dest) factors.
pub fn quad_rs(src: u32, dest: u32) -> [(u32, u32); 11] {
    // ZENABLE, ZWRITE, ALPHABLEND, SRCBLEND, DESTBLEND, ALPHATEST, CULL=NONE, FOG, SPECULAR, COLORKEY, STENCIL
    [(7, 0), (14, 0), (27, 1), (19, src), (20, dest), (15, 0), (22, 1), (28, 0), (29, 0), (41, 0), (52, 0)]
}
pub const BLEND_ONE: u32 = 2;
pub const BLEND_SRCALPHA: u32 = 5;
pub const BLEND_INVSRCALPHA: u32 = 6;

/// One XYZRHW | TEX1 or XYZRHW | DIFFUSE | TEX1 triangle strip (4 vertices), unhooked.
pub unsafe fn draw_strip(dev: P, fvf: u32, verts: &[f32]) -> HRESULT {
    const TRIANGLESTRIP: u32 = 5;
    call!(DEV_DP as fn(P, u32, u32, *const c_void, u32, u32) -> HRESULT; dev, TRIANGLESTRIP, fvf, verts.as_ptr().cast(), 4, 0)
}

// ---------------------------------------------------------------- textures

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
            unsafe { release(self.ptr as P) };
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
    match create_surface(&mut d) {
        Ok(tex) => Some(Tex { ptr: tex as usize, owner: DDRAW.load(Relaxed), w: tw, h: th }),
        Err(hr) => {
            log!("overlay: texture create failed 0x{:08X}", hr as u32);
            None
        }
    }
}

/// Copy a w x h image into the top-left of the texture (the rest is cleared).
pub unsafe fn upload(t: &Tex, w: usize, h: usize, px: &[u32]) -> bool {
    let Some((bits, pitch, _)) = lock_rect(t.ptr as P, None, DDLOCK_WRITEONLY) else { return false };
    for y in 0..t.h {
        let row = bits.add(y * pitch) as *mut u32;
        for x in 0..t.w {
            *row.add(x) = if x < w && y < h { px[y * w + x] } else { 0 };
        }
    }
    unlock_rect(t.ptr as P, None);
    true
}

// ---------------------------------------------------------------- quads

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
    let begin: unsafe extern "system" fn(P) -> HRESULT = method(dev, 5);
    let filter = if point { 1 } else { 2 };
    // stage 0: COLOROP/ALPHAOP = SELECTARG1 of TEXTURE, clamp, filters, coord set 0; stage 1 off
    let ts = [(0, 1, 2), (0, 2, 2), (0, 4, 2), (0, 5, 2), (0, 12, 3), (0, 16, filter), (0, 17, filter), (0, 18, 1), (0, 11, 0), (0, 24, 0), (1, 1, 1), (1, 4, 1)];
    let mut old_vp = Viewport { x: 0, y: 0, w: 0, h: 0, min_z: 0.0, max_z: 1.0 };
    let have_vp = call!(DEV_GET_VP as fn(P, *mut Viewport) -> HRESULT; dev, &mut old_vp) >= 0;
    if begin(dev) >= 0 {
        let states = States::set(dev, &quad_rs(BLEND_ONE, BLEND_INVSRCALPHA), &ts, std::ptr::null_mut());
        call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &clip);
        for q in quads {
            let (tu, tv) = (q.w as f32 / q.tex.w as f32, q.h as f32 / q.tex.h as f32);
            let r = &q.rect;
            let (x0, y0, x1, y1) = (r.left as f32 - 0.5, r.top as f32 - 0.5, r.right as f32 - 0.5, r.bottom as f32 - 0.5);
            let quad: [f32; 24] = [x0, y0, 0.0, 1.0, 0.0, 0.0, x1, y0, 0.0, 1.0, tu, 0.0, x0, y1, 0.0, 1.0, 0.0, tv, x1, y1, 0.0, 1.0, tu, tv];
            states.texture(q.tex.ptr as P);
            draw_strip(dev, 0x104, &quad); // XYZRHW | TEX1
        }
        call!(DEV_END_SCENE as fn(P) -> HRESULT; dev);
    }
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
