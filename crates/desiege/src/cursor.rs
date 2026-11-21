//! The game draws its cursor by locking the back buffer: copy the block under the mouse out,
//! alpha-blend the cursor onto it, copy it back. Reading a render target back to the CPU
//! stalls the GPU pipeline twice a frame, which halved the frame rate (70 -> 35 fps).
//!
//! Instead, those small locks get a scratch buffer that is thrown away, nothing is written to
//! the back buffer, and the cursor is drawn as a textured quad at Flip. Its image and position
//! come straight from the game's cursor object (see `game`), on the exe builds whose code we
//! have checked.
//!
//! Fallback for other builds: the game is handed a plain background, black on even frames and
//! white on odd ones, and the cursor's color (premultiplied) and alpha are recovered from one
//! of each:  black: c0 = a*F   white: c1 = a*F + (1-a)   =>   a = 1 - (c1 - c0), color = c0.
//! This guesses which background the game composited on, and the game's own erase-and-redraw
//! path breaks that guess (solid black/white boxes, stretched cursor; see `game`).

use crate::{device::Viewport, gpu, rd, scale::*, surface::*, HRESULT, P, DDRAW};
use std::{
    ffi::c_void,
    sync::{atomic::Ordering::Relaxed, Mutex},
};
use windows_sys::Win32::Foundation::RECT;

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
    /// The game's own (straight alpha) image the premultiplied one was made from.
    source: Vec<u32>,
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

/// The game's cursor object (DungeonSiege.exe 1.11.1.1486: Steam and GOG share this code;
/// other builds use the fallback).
///
/// The cursor routine (0x656e02, `this` = the object, called from 0x65118a per frame and from
/// 0x6568fc with an erase-and-redraw flag) works on screen rects through a copy helper that
/// locks the screen surface: save the background under the new rect, copy it to a work
/// surface, blend the image in, write the work surface back. With the flag set it instead
/// reads the union of the old and new rects, restores the previous call's saved background
/// into it, then blends and writes the union back. That's why the black/white guess fails:
/// the background under the cursor is partly (same rect: entirely) last call's, and the
/// written rect is the union, not the cursor.
///
/// What we use, all read while the routine holds the object's critical section (our Lock
/// hook runs inside it):
///   +0x38  image: { +0 width, +4 height, +0xc pixels (ARGB, straight alpha, pitch = width) }
///   +0x114 / +0x118  mouse position (client coords)
///   +0x11c / +0x120  hotspot
/// The routine maps client rects to the surface with ClientToScreen on the game window.
mod game {
    use crate::rd;
    use std::{ffi::c_void, sync::OnceLock};
    use windows_sys::Win32::{Foundation::POINT, Graphics::Gdi::ClientToScreen};

    pub struct Cursor {
        pub x: i32,
        pub y: i32,
        pub w: usize,
        pub h: usize,
        pub pixels: *const u32,
    }

    /// Address of the global holding the cursor object, if this exe's code matches.
    fn object_global() -> Option<usize> {
        static G: OnceLock<Option<usize>> = OnceLock::new();
        *G.get_or_init(|| unsafe {
            let base = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize;
            let at = |va: usize, bytes: &[u8]| std::slice::from_raw_parts((base + va - 0x40_0000) as *const u8, bytes.len()) == bytes;
            // mov ecx,[0x7a0680]; push esi; call 0x656e02 (the per-frame call)
            // mov eax,[ebx+0x114]; sub eax,[ebx+0x11c]; mov ecx,[ebx+0x118] (position)
            // mov esi,[ebx+0x38] (image)
            let ok = base == 0x40_0000
                && at(0x65_1183, &[0x8B, 0x0D, 0x80, 0x06, 0x7A, 0x00, 0x56, 0xE8, 0x73, 0x5C, 0x00, 0x00])
                && at(0x65_7085, &[0x8B, 0x83, 0x14, 0x01, 0, 0, 0x2B, 0x83, 0x1C, 0x01, 0, 0, 0x8B, 0x8B, 0x18, 0x01, 0, 0])
                && at(0x65_70A0, &[0x8B, 0x73, 0x38]);
            log!("cursor: {}", if ok { "image and position from the game's cursor object" } else { "unknown exe build, black/white fallback" });
            ok.then_some(0x7A_0680)
        })
    }

    pub fn known() -> bool { object_global().is_some() }

    /// The cursor as the game is drawing it right now. None when it has no image (hidden).
    pub unsafe fn current() -> Option<Cursor> {
        let obj = *(object_global()? as *const usize);
        if obj == 0 {
            return None;
        }
        let o = obj as *const c_void;
        let img = rd(o, 0x38) as *const c_void;
        if img.is_null() {
            return None;
        }
        let (w, h) = (rd(img, 0) as usize, rd(img, 4) as usize);
        let pixels = rd(img, 0xC) as *const u32;
        if w == 0 || h == 0 || w > super::MAX || h > super::MAX || pixels.is_null() {
            return None;
        }
        let mut p = POINT { x: rd(o, 0x114) as i32 - rd(o, 0x11C) as i32, y: rd(o, 0x118) as i32 - rd(o, 0x120) as i32 };
        // Same mapping the game applies to its rects ([[0x79cf7c]+0x70] = game window).
        let app = *(0x79_CF7C as *const usize);
        if app != 0 {
            ClientToScreen(rd(app as *const c_void, 0x70) as _, &mut p);
        }
        Some(Cursor { x: p.x, y: p.y, w, h, pixels })
    }
}

/// Straight-alpha ARGB (the game blends `dst + (src - dst) * a`) to premultiplied.
fn premultiply(src: &[u32]) -> Vec<u32> {
    src.iter()
        .map(|&p| {
            let a = p >> 24;
            let ch = |i: u32| (((p >> (8 * i)) & 0xFF) * a + 127) / 255;
            (a << 24) | (ch(2) << 16) | (ch(1) << 8) | ch(0)
        })
        .collect()
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
            if game::known() {
                from_game(s);
            } else {
                from_composite(s, vr, buf);
            }
        }
        true
    })
    .unwrap_or(false)
}

/// The game just wrote its cursor: take image and position from its cursor object.
unsafe fn from_game(s: &mut State) {
    let Some(c) = game::current() else { return };
    let src = std::slice::from_raw_parts(c.pixels, c.w * c.h);
    if s.source != src || s.image_size != (c.w, c.h) {
        s.source = src.to_vec();
        s.image = premultiply(src);
        s.image_size = (c.w, c.h);
        s.dirty = true;
    }
    s.rect = Some(RECT { left: c.x, top: c.y, right: c.x + c.w as i32, bottom: c.y + c.h as i32 });
}

/// Fallback: learn the image from a black-background and a white-background composite.
fn from_composite(s: &mut State, vr: RECT, buf: Vec<u32>) {
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
        return;
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

/// Draw this frame's cursor (if the game drew one) as a quad. Called right before Flip.
pub unsafe fn draw(dev: P) {
    with(|s| {
        let Some(mut vr) = s.rect.take() else { return };
        if s.image.is_empty() || DDRAW.load(Relaxed) == 0 {
            return;
        }
        // Fallback only (the game path always has the full rect): clipped at an edge, extend
        // back to the full image size on the clipped side(s); the part beyond the screen
        // just falls off.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_from_black_and_white() {
        // opaque red, half-transparent white, fully transparent
        let black = [0xFFFF_0000u32, 0xFF80_8080, 0xFF00_0000];
        let white = [0xFFFF_0000u32, 0xFFFF_FFFF, 0xFFFF_FFFF];
        let img = reconstruct(&black, &white).unwrap();
        assert_eq!(img, [0xFFFF_0000, 0x8080_8080, 0x0000_0000]);
        assert_eq!(reconstruct(&[0xFF10_1010], &[0xFF00_0000]), None); // not a pair
        // channels disagree on the background step: different cursor content in the two frames
        assert_eq!(reconstruct(&[0xFF00_0000], &[0xFFFF_0000]), None);
    }

    #[test]
    fn straight_alpha_to_premultiplied() {
        // opaque red stays, half-transparent white halves, transparent (any color) is zero
        assert_eq!(premultiply(&[0xFFFF_0000, 0x80FF_FFFF, 0x00AB_CDEF]), [0xFFFF_0000, 0x8080_8080, 0]);
    }
}
