//! The game draws its cursor by locking the back buffer: read the 64x64 block under the
//! mouse, alpha-blend the cursor onto it, write it back. Reading a render target back to the
//! CPU stalls the GPU pipeline twice a frame, which halved the frame rate (70 -> 35 fps).
//!
//! Instead, the game is handed a plain background: black on even frames, white on odd ones.
//! From one of each, the cursor's exact color (premultiplied) and alpha fall out:
//!   black: c0 = a*F          white: c1 = a*F + (1-a)   =>   a = 1 - (c1 - c0), color = c0
//! Nothing is written to the back buffer; the image is drawn as a textured quad at Flip.

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
}
