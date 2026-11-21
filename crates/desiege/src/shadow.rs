//! Character shadows, as the game does them (once per character, every frame):
//!   1. Blt colour-fill of a corner of the back buffer, rect (0,0,64,64), white
//!   2. SetViewport(0,0,64,64) and draw the character's silhouette (ordinary 3D draws)
//!   3. BltFast(shadow texture <- that corner); the texture lives in system memory
//!
//! ...and after the last one, a colour-fill of the corner back to the fog colour.
//! Step 3 makes dxwrapper read the whole frame back to the CPU and upload it again, per
//! character: that was the main frame-rate cost (~45 vs ~110 fps).
//!
//! Redirected: (1) is skipped and its colour remembered; at (2) rendering switches to a
//! video-memory render-target texture of `res()` pixels, cleared to that colour, with the
//! viewport stretched over it; (3) switches back to the back buffer and maps the game's shadow
//! texture to ours, without copying anything; SetTexture(game texture) binds ours instead. Bonus:
//! the silhouette is rendered at `res()` instead of 64 px, so shadows can be sharper.

use crate::{debug, device::*, gpu, method, rd, scale::*, surface, HRESULT, P};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    Mutex, OnceLock,
};
use windows_sys::Win32::Foundation::RECT;

/// Shadow texture size in pixels; 0 = feature off (the game's own copy path).
/// DESIEGE_SHADOW_RES, or a `shadow_res` file next to this DLL (launcher writes it).
pub fn res() -> u32 {
    static R: OnceLock<u32> = OnceLock::new();
    let r = *R.get_or_init(|| {
        let from_file = || std::fs::read_to_string(crate::self_dir()?.join("shadow_res")).ok();
        let r = std::env::var("DESIEGE_SHADOW_RES").ok().or_else(from_file).and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(512);
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

unsafe fn release(obj: usize) {
    gpu::release(obj as P);
}

/// New render-target texture in the back buffer's pixel format.
unsafe fn create_target(back: P) -> Option<usize> {
    let b = surface::desc(back)?;
    let r = res();
    let mut d = [0u32; 31];
    d[0] = 124;
    d[1] = 1 | 2 | 4 | 0x1000; // CAPS | HEIGHT | WIDTH | PIXELFORMAT
    d[2] = r;
    d[3] = r;
    d[18..26].copy_from_slice(&b[18..26]); // ddpfPixelFormat @72
    d[26] = 0x1000 | 0x2000 | 0x4000; // TEXTURE | 3DDEVICE | VIDEOMEMORY
    match gpu::create_surface(&mut d) {
        Ok(t) => Some(t as usize),
        Err(hr) => {
            log!("shadow redirect: render target create failed 0x{:08X}; using the game's path", hr as u32);
            DISABLED.store(true, Relaxed);
            None
        }
    }
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
    let dev = crate::device::current();
    with(|s| {
        let (captured, back) = s.capturing?;
        let target = if crate::gfx::config().soft_shadows { soften(dev, s, captured, back as P) } else { captured };
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

/// Blur radius as a fraction of the shadow texture size, so softness looks the same at
/// every shadow quality (0.7 % of 256 px = 1.8 px). The texture is stretched over a large patch
/// of ground, so a little goes a long way; 1.2 % already washed thin limbs out.
const SOFT_RADIUS: f32 = 0.007;
const SOFT_TAPS: usize = 12;

/// Tap offsets (in units of the radius): a golden-angle spiral, evenly covering the disc.
pub fn soft_taps() -> [(f32, f32); SOFT_TAPS] {
    let mut t = [(0.0, 0.0); SOFT_TAPS];
    for (i, p) in t.iter_mut().enumerate() {
        let r = ((i as f32 + 0.5) / SOFT_TAPS as f32).sqrt();
        let a = i as f32 * 2.399_963; // golden angle
        *p = (r * a.cos(), r * a.sin());
    }
    t
}

/// Soft shadows: average SOFT_TAPS offset copies of the captured silhouette into a second
/// render target (running average: tap i is blended in with weight 1/(i+1), which keeps
/// 8-bit rounding error to a level or two). Returns the texture to use; the capture goes
/// back to the pool. Falls back to the sharp capture if anything fails.
unsafe fn soften(dev: P, s: &mut State, src: usize, back: P) -> usize {
    let Some(dst) = s.free.pop().or_else(|| create_target(back)) else { return src };
    if call!(DEV_SET_RT as fn(P, P, u32) -> HRESULT; dev, dst as P, 0) < 0 {
        s.free.push(dst);
        return src;
    }
    let r = res() as f32;
    let vp = Viewport { x: 0, y: 0, w: res(), h: res(), min_z: 0.0, max_z: 1.0 };
    call!(DEV_SET_VP as fn(P, *const Viewport) -> HRESULT; dev, &vp);
    // stage 0: colour = texture, alpha = vertex diffuse (the tap weight), clamp, bilinear,
    // no mips, coord set 0; stage 1 off
    let ts = [(0, 1, 2), (0, 2, 2), (0, 4, 2), (0, 5, 0), (0, 12, 3), (0, 16, 2), (0, 17, 2), (0, 18, 1), (0, 11, 0), (0, 24, 0), (1, 1, 1), (1, 4, 1)];
    let states = gpu::States::set(dev, &gpu::quad_rs(gpu::BLEND_SRCALPHA, gpu::BLEND_INVSRCALPHA), &ts, src as P);
    let begin: unsafe extern "system" fn(P) -> HRESULT = method(dev, 5);
    // The game is normally mid-scene here; if not, open one for our draws.
    let own_scene = begin(dev) >= 0;
    let radius = (r * SOFT_RADIUS).max(1.0);
    for (i, (dx, dy)) in soft_taps().iter().enumerate() {
        let alpha = (255.0 / (i as f32 + 1.0)).round() as u32;
        let c = (alpha << 24) | 0x00FF_FFFF;
        let (u0, v0) = (dx * radius / r, dy * radius / r);
        let (x0, y0, x1, y1) = (-0.5f32, -0.5f32, r - 0.5, r - 0.5);
        let c = f32::from_bits(c);
        // XYZRHW | DIFFUSE | TEX1
        let quad: [f32; 28] = [
            x0, y0, 0.0, 1.0, c, u0, v0,
            x1, y0, 0.0, 1.0, c, 1.0 + u0, v0,
            x0, y1, 0.0, 1.0, c, u0, 1.0 + v0,
            x1, y1, 0.0, 1.0, c, 1.0 + u0, 1.0 + v0,
        ];
        gpu::draw_strip(dev, 0x144, &quad);
    }
    if own_scene {
        call!(DEV_END_SCENE as fn(P) -> HRESULT; dev);
    }
    drop(states); // the game's states and texture back
    s.free.push(src);
    dst
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
    match debug::save_surface(target as P, &format!("shadow_{n}")) {
        Some((w, h)) => log!("shadow dump: {w}x{h} #{n}"),
        None => log!("shadow dump: #{n} failed"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_shadow_taps_cover_the_disc() {
        let t = soft_taps();
        assert!(t.iter().all(|&(x, y)| (x * x + y * y).sqrt() <= 1.0 + 1e-4));
        let (mx, my) = t.iter().fold((0.0, 0.0), |a, &(x, y)| (a.0 + x, a.1 + y));
        assert!((mx / t.len() as f32).abs() < 0.1 && (my / t.len() as f32).abs() < 0.1, "centred: {mx} {my}");
    }
}
