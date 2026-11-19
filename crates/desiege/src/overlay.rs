//! Text drawn on top of the frame for code mods (`draw_text` in the mod API). Each item is
//! rendered with GDI (Segoe UI, greyscale antialiasing) into a texture with a soft drop
//! shadow, then drawn as a quad at Flip. Items are queued during the frame and drawn once.

use crate::{gpu, P, DDRAW};
use std::sync::{atomic::Ordering::Relaxed, Mutex};
use windows_sys::Win32::{Foundation::RECT, Graphics::Gdi::*};


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
