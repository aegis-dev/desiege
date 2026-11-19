//! The Siege engine's dynamic lights, read from its light database every frame, in world space
//! (for per-pixel lighting, docs/GRAPHICS.md #1). Debug (DESIEGE_GFX):
//! - `lights_probe=1`: log every light once, 5 s into the game, plus a few raw terrain nodes
//! - `lights_probe=2`: draw a marker on screen where each point light is, every frame
//!
//! Engine layout (DungeonSiege.exe 1.11.1.1486, found by disassembling the FuBi light exports):
//! - SiegeEngine singleton: `FUBI_GetClassSingleton@SiegeEngine` (exported) = [0x7A05FC]
//! - camera: [engine + 0x3C]; camera SiegePos at +0x128, world position at +0x148
//! - light database: [engine + 0xAC], an MSVC 6 `std::map<light id, light*>`; tree node: left
//!   +0, parent +4, right +8, key +0xC, light* +0x10, vector of terrain-node handles +0x14..+0x1C
//!   (handle: node guid +0, SiegeNode* +4)
//! - 0x67A985 (thiscall db, out, id): the light's descriptor, 9 dwords: type (0 point, 1
//!   directional), ?, ARGB colour, active, inner radius, outer radius, intensity, flags, ?
//! - 0x67AAAD (thiscall db, out, id): its position as a SiegePos (xyz in node space, node guid)
//! - SiegeNode: world position of the node's origin at +0x48, world orientation 3x3 at +0x54.
//!   "World" is the space of the camera target's node (that node has position 0, identity).

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
        OnceLock,
    },
};
use windows_sys::Win32::System::{
    LibraryLoader::{GetModuleHandleW, GetProcAddress},
    Memory::IsBadReadPtr,
};

fn readable(p: usize, n: usize) -> bool {
    p >= 0x10000 && unsafe { IsBadReadPtr(p as *const c_void, n) } == 0
}
unsafe fn u(p: usize) -> usize { *(p as *const u32) as usize }
unsafe fn f(p: usize) -> f32 { *(p as *const f32) }

/// Exact bytes at an address we call into, so another exe build is refused instead of crashing.
unsafe fn code_is(addr: usize, bytes: &[u8]) -> bool {
    readable(addr, bytes.len()) && std::slice::from_raw_parts(addr as *const u8, bytes.len()) == bytes
}

#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub point: bool,
    /// World position (point lights) or direction (directional).
    pub pos: [f32; 3],
    pub argb: u32,
    pub inner: f32,
    pub outer: f32,
    pub intensity: f32,
    pub active: bool,
    /// Descriptor word 1 set: so far only the light that follows the party has it (torches and
    /// the sun have 0), so it's treated as "the party light".
    pub party: bool,
}

type Get = unsafe extern "thiscall" fn(usize, *mut u32, u32) -> *mut u32;
const GET_DESC: usize = 0x67A985;
const GET_POS: usize = 0x67AAAD;

/// The engine, if this is the exe build we know (checked once).
unsafe fn engine() -> Option<usize> {
    static OK: OnceLock<bool> = OnceLock::new();
    let ok = *OK.get_or_init(|| {
        let ok = code_is(GET_DESC, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x28, 0x56, 0x57])
            && code_is(GET_POS, &[0x55, 0x8B, 0xEC, 0x83, 0xEC, 0x28, 0xA1, 0x7C, 0x68, 0x7A]);
        if !ok {
            log!("lights: unknown DungeonSiege.exe build; light reading off");
        }
        ok
    });
    if !ok {
        return None;
    }
    static SINGLETON: OnceLock<usize> = OnceLock::new();
    let s = *SINGLETON.get_or_init(|| {
        GetProcAddress(GetModuleHandleW(std::ptr::null()), c"?FUBI_GetClassSingleton@SiegeEngine@siege@@CAPAV12@XZ".as_ptr().cast()).map_or(0, |p| p as usize)
    });
    if s == 0 {
        return None;
    }
    let e = std::mem::transmute::<usize, unsafe extern "C" fn() -> usize>(s)();
    readable(e, 0x100).then_some(e)
}

/// Every light in the engine's database, positions converted to world space.
pub unsafe fn collect() -> Vec<Light> {
    let mut out = Vec::new();
    let Some(engine) = engine() else { return out };
    let db = u(engine + 0xAC);
    if !readable(db, 16) {
        return out;
    }
    let head = u(db + 4);
    if !readable(head, 12) {
        return out;
    }
    let (get_desc, get_pos) = (std::mem::transmute::<usize, Get>(GET_DESC), std::mem::transmute::<usize, Get>(GET_POS));
    let mut stack = vec![u(head + 4)];
    let mut visited = 0;
    while let Some(n) = stack.pop() {
        // the shared nil node of a VC6 tree has null children
        if visited > 512 || n == head || !readable(n, 0x20) || (u(n) == 0 && u(n + 8) == 0) {
            continue;
        }
        visited += 1;
        stack.push(u(n));
        stack.push(u(n + 8));
        let key = u(n + 0xC) as u32;
        let (mut d, mut p) = ([0u32; 9], [0u32; 4]);
        get_desc(db, d.as_mut_ptr(), key);
        get_pos(db, p.as_mut_ptr(), key);
        let local = [f32::from_bits(p[0]), f32::from_bits(p[1]), f32::from_bits(p[2])];
        let point = d[0] == 0;
        let mut pos = local;
        if point {
            // node-space -> world: the node is in the light's handle list
            let (vb, ve) = (u(n + 0x14), u(n + 0x18));
            let node = (vb..ve.min(vb + 4096)).step_by(4).map(|a| u(a)).find(|&h| readable(h, 8) && u(h) == p[3] as usize).map(|h| u(h + 4));
            let Some(node) = node.filter(|&nd| readable(nd, 0x80)) else { continue };
            let (t, r) = (node + 0x48, node + 0x54);
            // ponytail: row-vector convention assumed (p * R + t); every node seen so far has an
            // identity rotation, so a rotated node will tell whether it's R or R transposed.
            for (i, o) in pos.iter_mut().enumerate() {
                *o = f(t + 4 * i) + (0..3).map(|k| local[k] * f(r + 4 * (3 * k + i))).sum::<f32>();
            }
        }
        out.push(Light {
            point,
            pos,
            argb: d[2],
            inner: f32::from_bits(d[4]),
            outer: f32::from_bits(d[5]),
            intensity: f32::from_bits(d[6]),
            active: d[3] != 0,
            party: d[1] != 0,
        });
    }
    out
}

// ---------------------------------------------------------------- the world's matrices

/// D3D7 view and projection, current and as last used for a world draw (row-vector 4x4).
static VIEW: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
static PROJ: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
static WORLD_VIEW: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];
static WORLD_PROJ: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];

pub unsafe fn set_transform(kind: u32, m: *const f32) {
    const VIEW_T: u32 = 2; const PROJECTION: u32 = 3; const WORLD_T: u32 = 1;
    if kind == WORLD_T && crate::gfx::config().lights_probe == 1 && LOGGING_WORLD.load(Relaxed) {
        let w: Vec<f32> = (0..16).map(|i| *m.add(i)).collect();
        if let Ok(mut seen) = WORLDS.lock() {
            let key: Vec<i32> = w.iter().map(|x| (x * 100.0) as i32).collect();
            if seen.len() < 40 && !seen.contains(&key) {
                seen.push(key);
                log!("lights: WORLD rot [{:.2} {:.2} {:.2} / {:.2} {:.2} {:.2} / {:.2} {:.2} {:.2}] trans [{:.2} {:.2} {:.2}]", w[0], w[1], w[2], w[4], w[5], w[6], w[8], w[9], w[10], w[12], w[13], w[14]);
            }
        }
    }
    let dst = match kind { VIEW_T => &VIEW, PROJECTION => &PROJ, _ => return };
    for (i, a) in dst.iter().enumerate() {
        a.store((*m.add(i)).to_bits(), Relaxed);
    }
}
static LOGGING_WORLD: AtomicBool = AtomicBool::new(false);
static WORLDS: std::sync::Mutex<Vec<Vec<i32>>> = std::sync::Mutex::new(Vec::new());

/// A 3D draw to the back buffer.
pub fn world_draw() {
    for (w, c) in WORLD_VIEW.iter().zip(&VIEW).chain(WORLD_PROJ.iter().zip(&PROJ)) {
        w.store(c.load(Relaxed), Relaxed);
    }
}
fn mat(a: &[AtomicU32; 16]) -> [f32; 16] { std::array::from_fn(|i| f32::from_bits(a[i].load(Relaxed))) }
fn mul(p: [f32; 4], m: &[f32; 16]) -> [f32; 4] {
    std::array::from_fn(|j| (0..4).map(|i| p[i] * m[4 * i + j]).sum())
}
/// World position -> native screen pixels in the given viewport (x, y, w, h); None if behind.
pub fn project(p: [f32; 3], vp: [f32; 4]) -> Option<(f32, f32, f32)> {
    let c = mul(mul([p[0], p[1], p[2], 1.0], &mat(&WORLD_VIEW)), &mat(&WORLD_PROJ));
    if c[3] <= 1e-4 {
        return None;
    }
    let (x, y) = (c[0] / c[3], c[1] / c[3]);
    Some((vp[0] + (x + 1.0) * 0.5 * vp[2], vp[1] + (1.0 - y) * 0.5 * vp[3], c[3]))
}

/// Up to `max` active point lights for the light shader, nearest to the camera first:
/// per light [view-space x, y, z, inner radius, r, g, b (times intensity), outer radius].
pub unsafe fn for_shader(max: usize) -> Vec<[f32; 8]> {
    let view = mat(&WORLD_VIEW);
    let mut ls: Vec<[f32; 8]> = collect()
        .into_iter()
        .filter(|l| l.point && l.active && l.outer > 0.0 && l.intensity > 0.0)
        .map(|l| {
            let v = mul([l.pos[0], l.pos[1], l.pos[2], 1.0], &view);
            let scale = l.intensity * if l.party { crate::gfx::config().lights_party } else { 1.0 };
            let ch = |s: u32| ((l.argb >> s) & 0xFF) as f32 / 255.0 * scale;
            [v[0], v[1], v[2], l.inner, ch(16), ch(8), ch(0), l.outer]
        })
        .collect();
    ls.sort_by(|a, b| a[2].total_cmp(&b[2]));
    ls.truncate(max);
    ls
}

// ---------------------------------------------------------------- debug

/// Called right before the UI is drawn (see ao::before_2d) on in-game frames.
pub unsafe fn debug_frame(vp: [f32; 4]) {
    match crate::gfx::config().lights_probe {
        1 => log_once(vp),
        2 => markers(vp),
        _ => {}
    }
}

unsafe fn markers(vp: [f32; 4]) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, Relaxed);
    let all = collect();
    if n.is_multiple_of(600) {
        log!("lights: markers frame {n}: {} lights, points at {:?}", all.len(), all.iter().filter(|l| l.point).map(|l| project(l.pos, vp)).collect::<Vec<_>>());
    }
    for l in all.iter().filter(|l| l.point) {
        let Some((x, y, _)) = project(l.pos, vp) else { continue };
        let label: Vec<u16> = format!("+ {:.1}/{:.1} x{:.2}", l.inner, l.outer, l.intensity).encode_utf16().collect();
        crate::overlay::queue(crate::overlay::Item { x: x as i32 - 8, y: y as i32 - 16, text: label, argb: l.argb | 0xFF00_0000, size: 24, align: 0 });
    }
}

unsafe fn log_once(vp: [f32; 4]) {
    static DONE: AtomicBool = AtomicBool::new(false);
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    if DONE.load(Relaxed) || START.get_or_init(std::time::Instant::now).elapsed().as_secs() < 5 {
        return;
    }
    DONE.store(true, Relaxed);
    LOGGING_WORLD.store(true, Relaxed); // the next frame's WORLD matrices get logged
    if let Some(engine) = engine() {
        let cam = u(engine + 0x3C);
        if readable(cam, 0x160) {
            log!("lights: camera world {:?}, target world {:?}", [f(cam + 0x148), f(cam + 0x14C), f(cam + 0x150)], [f(cam + 0x154), f(cam + 0x158), f(cam + 0x15C)]);
        }
    }
    let all = collect();
    for l in &all {
        log!("lights: {l:?} -> screen {:?}", if l.point { project(l.pos, vp) } else { None });
    }
    log!("lights: {} lights", all.len());
}
