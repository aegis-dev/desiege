//! Development aids:
//! - fps and per-600-frame draw/lock stats in the log (first 20 windows)
//! - DESIEGE_EXPERIMENT=a,b switches
//! - DESIEGE_TRACE_FRAME=<n>: every hooked call of frame n
//! - DESIEGE_SNAPSHOT=300,900: the native back buffer at those frames, as BMP files

use crate::{method, scale::is_main, surface, HRESULT, P};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    Mutex, OnceLock,
};
use windows_sys::Win32::Foundation::RECT;

// ---------------------------------------------------------------- experiments

/// DESIEGE_EXPERIMENT=a,b toggles experiments (perf investigations, A/B tests).
pub fn experiment(name: &str) -> bool {
    static E: OnceLock<String> = OnceLock::new();
    E.get_or_init(|| std::env::var("DESIEGE_EXPERIMENT").unwrap_or_default()).split(',').any(|e| e.trim() == name)
}

// ---------------------------------------------------------------- stats

static STATS: [AtomicUsize; 15] = [const { AtomicUsize::new(0) }; 15];
pub const DP_TL: usize = 0; pub const DP_3D: usize = 1; pub const DIP_TL: usize = 2; pub const DIP_3D: usize = 3;
pub const STRIDED_TL: usize = 4; pub const VB_TL: usize = 5; pub const VB_3D: usize = 6; pub const LOCK_MAIN: usize = 7;
pub const BLT_MAIN: usize = 8; pub const BLTFAST_MAIN: usize = 9; pub const FRAMES: usize = 10; pub const DRAW_OFF_MAIN: usize = 11;
pub const BLT_FROM_MAIN: usize = 12; pub const FLIPS: usize = 13; pub const PRESENT_BLT: usize = 14;
pub fn stat(i: usize) { STATS[i].fetch_add(1, Relaxed); }

fn log_stats() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let now = std::time::Instant::now();
    let secs = LAST.lock().ok().and_then(|mut l| l.replace(now)).map_or(0.0, |t| (now - t).as_secs_f32());
    let s: Vec<usize> = STATS.iter().map(|a| a.swap(0, Relaxed)).collect();
    log!("fps: {:.0}", if secs > 0.0 { s[FRAMES] as f32 / secs } else { 0.0 });
    let sc = crate::scale::state_summary();
    log!(
        "scale stats ({} frames): DrawPrimitive tl={} 3d={} | DrawIndexed tl={} 3d={} | strided tl={} | VB tl={} 3d={} | off-main draws={} | main Lock={} Blt={} BltFast={} copies-from-main={} | flips={} present-blts={} | {sc}",
        s[FRAMES], s[DP_TL], s[DP_3D], s[DIP_TL], s[DIP_3D], s[STRIDED_TL], s[VB_TL], s[VB_3D], s[DRAW_OFF_MAIN],
        s[LOCK_MAIN], s[BLT_MAIN], s[BLTFAST_MAIN], s[BLT_FROM_MAIN], s[FLIPS], s[PRESENT_BLT]
    );
}

/// Before the game's EndScene: count the frame, log stats every 600 frames.
pub fn count_frame() {
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    stat(FRAMES);
    if STATS[FRAMES].load(Relaxed) >= 600 && LOGGED.fetch_add(1, Relaxed) < 20 {
        log_stats();
    }
}

/// Frame number of the last finished scene, for the snapshot at the next Flip.
static SNAP_FRAME: AtomicUsize = AtomicUsize::new(usize::MAX);

/// After the game's EndScene.
pub fn scene_ended() {
    static TOTAL: AtomicUsize = AtomicUsize::new(0);
    let frame = TOTAL.fetch_add(1, Relaxed);
    if experiment("profile") {
        crate::profile::frame(frame);
    }
    SNAP_FRAME.store(frame, Relaxed); // snapshot happens at Flip, after the cursor is drawn
}

// ---------------------------------------------------------------- snapshots

/// 32-bit top-down BMP from locked surface rows.
pub unsafe fn bmp(w: usize, h: usize, pitch: usize, bits: *const u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(54 + w * h * 4);
    out.extend_from_slice(b"BM");
    for v in [(54 + w * h * 4) as u32, 0, 54, 40, w as u32, (-(h as i32)) as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    for _ in 0..6 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    for y in 0..h {
        out.extend_from_slice(std::slice::from_raw_parts(bits.add(y * pitch), w * 4));
    }
    out
}

/// Write a whole 32-bit surface to %LOCALAPPDATA%\Desiege\<name>.bmp. Returns its size.
pub unsafe fn save_surface(surf: P, name: &str) -> Option<(usize, usize)> {
    let d = surface::desc(surf)?;
    let (h, w) = (d[2] as usize, d[3] as usize);
    let (bits, pitch, bpp) = surface::lock_rect(surf, None, surface::DDLOCK_READONLY)?;
    let data = (bpp == 32).then(|| bmp(w, h, pitch, bits));
    surface::unlock_rect(surf, None);
    let path = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Desiege").join(format!("{name}.bmp"));
    std::fs::write(&path, data?).ok()?;
    Some((w, h))
}

/// At Flip: DESIEGE_SNAPSHOT=300,900 dumps the native back buffer at those frames to
/// snap_<frame>.bmp (exclusive fullscreen can't be screen-captured).
pub unsafe fn snapshot_if_due(dev: P) {
    let frame = SNAP_FRAME.swap(usize::MAX, Relaxed);
    static FRAMES: OnceLock<Vec<usize>> = OnceLock::new();
    let frames = FRAMES.get_or_init(|| std::env::var("DESIEGE_SNAPSHOT").unwrap_or_default().split(',').filter_map(|s| s.trim().parse().ok()).collect());
    if frame == usize::MAX || !frames.contains(&frame) {
        return;
    }
    // IDirect3DDevice7::GetRenderTarget = slot 9
    let get_rt: unsafe extern "system" fn(P, *mut P) -> HRESULT = method(dev, 9);
    let mut rt = std::ptr::null_mut();
    if get_rt(dev, &mut rt) < 0 || rt.is_null() {
        return;
    }
    match save_surface(rt, &format!("snap_{frame}")) {
        Some((w, h)) => log!("snapshot: frame {frame} {w}x{h} -> snap_{frame}.bmp"),
        None => log!("snapshot: frame {frame} failed"),
    }
    method::<unsafe extern "system" fn(P) -> u32>(rt, 2)(rt); // GetRenderTarget's reference
}

// ---------------------------------------------------------------- one-frame trace

/// DESIEGE_TRACE_FRAME=<n>: log every hooked call of frame n (counted in Flips).
static TRACING: AtomicBool = AtomicBool::new(false);
pub fn trace(msg: impl FnOnce() -> String) {
    if TRACING.load(Relaxed) {
        log!("  trace: {}", msg());
    }
}
pub fn trace_frame_boundary() {
    static FRAME: AtomicUsize = AtomicUsize::new(0);
    static TARGET: OnceLock<Option<usize>> = OnceLock::new();
    let f = FRAME.fetch_add(1, Relaxed) + 1;
    if let Some(t) = *TARGET.get_or_init(|| std::env::var("DESIEGE_TRACE_FRAME").ok().and_then(|v| v.parse().ok())) {
        if f == t { log!("trace: frame {t} begins"); }
        TRACING.store(f == t, Relaxed);
        if f == t + 1 { log!("trace: frame {t} ends"); }
    }
}
/// Trace helpers: "(main)" for primary-chain surfaces, rects as tuples.
pub fn mm(s: P) -> &'static str { if is_main(s) { "(main)" } else { "" } }
pub unsafe fn r_(r: *mut RECT) -> Option<(i32, i32, i32, i32)> { r.as_ref().map(|r| (r.left, r.top, r.right, r.bottom)) }

/// Logs the first 15 input mappings (enough to spot a mapping bug).
pub fn dbg_input(msg: impl FnOnce() -> String) {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Relaxed);
    if n < 15 {
        log!("input #{n}: {}", msg());
    }
}
