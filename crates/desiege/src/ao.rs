//! Screen-space ambient occlusion, drawn on the real D3D9 device (d3d9.rs) between the 3D
//! world and the UI. The game's art has almost no contact shadows; AO darkens creases, the
//! ground under objects and the insides of foliage, from the depth buffer alone.
//!
//! - Depth: dxwrapper's depth-stencil surface is created as an `INTZ` texture instead (D24S8
//!   that can also be sampled), so the world's depth can be read. Not with MSAA (INTZ can't be
//!   multisampled); AO is off then.
//! - When: at the first 2D (pre-transformed) draw to the back buffer after a 3D draw in a
//!   frame, i.e. right before the UI. Projection and viewport are the ones the world used.
//! - How: pass 1 computes AO into a texture (12 samples per pixel on a rotated spiral, view-
//!   space positions rebuilt from depth); pass 2 blurs it 4x4, depth-aware, and multiplies it
//!   onto the back buffer. All D3D9 state is captured in a state block and restored, so neither
//!   dxwrapper's state cache nor the game notices. Depth is unbound while it's sampled.
//! - Shaders are compiled at first use with d3dcompiler_47.dll (part of Windows 10/11) and
//!   cached on disk.
//!
//! Any failure turns AO off for the session and logs why.

use crate::{d3d9, gfx, method, HRESULT, P};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed},
        Mutex,
    },
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const FMT_A8R8G8B8: u32 = 21;
const FMT_D24S8: u32 = 75;
const FMT_D24X8: u32 = 77;
const FMT_INTZ: u32 = u32::from_le_bytes(*b"INTZ");
const USAGE_RENDERTARGET: u32 = 1;
const USAGE_DEPTHSTENCIL: u32 = 2;
const POOL_DEFAULT: u32 = 0;

// IDirect3DDevice9 slots
const CREATE_TEXTURE: usize = 23;
const SET_RENDER_TARGET: usize = 37;
const GET_RENDER_TARGET: usize = 38;
const SET_DEPTH_STENCIL: usize = 39;
const GET_DEPTH_STENCIL: usize = 40;
const SET_VIEWPORT: usize = 47;
const SET_RENDER_STATE: usize = 57;
const CREATE_STATE_BLOCK: usize = 59;
const SET_TEXTURE: usize = 65;
const SET_SAMPLER_STATE: usize = 69;
const DRAW_PRIMITIVE_UP: usize = 83;
const SET_FVF: usize = 89;
const CREATE_VERTEX_SHADER: usize = 91;
const SET_VERTEX_SHADER: usize = 92;
const CREATE_PIXEL_SHADER: usize = 106;
const SET_PIXEL_SHADER: usize = 107;
const SET_PS_CONST_F: usize = 109;
// IDirect3DTexture9::GetSurfaceLevel; IDirect3DStateBlock9::Capture / Apply
const GET_SURFACE_LEVEL: usize = 18;
const SB_CAPTURE: usize = 4;
const SB_APPLY: usize = 5;

pub fn enabled() -> bool {
    gfx::config().ao > 0 && !FAILED.load(Relaxed)
}
static FAILED: AtomicBool = AtomicBool::new(false);
fn fail(why: String) {
    if !FAILED.swap(true, Relaxed) {
        log!("ao: off ({why})");
    }
}

// ---------------------------------------------------------------- depth as a texture

/// Our INTZ depth: the surface dxwrapper got (it owns the only reference) and its texture.
/// Weak pointers: only used while that surface is the bound depth buffer.
static DEPTH: Mutex<Option<(usize, usize, u32, u32)>> = Mutex::new(None);

/// CreateDepthStencilSurface replacement. Some = handled (an INTZ texture's surface).
pub unsafe fn create_depth(dev: P, w: u32, h: u32, fmt: u32, ms: u32, out: *mut P) -> Option<HRESULT> {
    if !enabled() || ms != 0 || !(fmt == FMT_D24S8 || fmt == FMT_D24X8) || out.is_null() {
        if enabled() && ms != 0 {
            log!("ao: depth is multisampled (MSAA on); AO needs it single-sampled, so it stays off");
        }
        return None;
    }
    let create: unsafe extern "system" fn(P, u32, u32, u32, u32, u32, u32, *mut P, P) -> HRESULT = method(dev, CREATE_TEXTURE);
    let mut tex = std::ptr::null_mut();
    let hr = create(dev, w, h, 1, USAGE_DEPTHSTENCIL, FMT_INTZ, POOL_DEFAULT, &mut tex, std::ptr::null_mut());
    if hr < 0 || tex.is_null() {
        fail(format!("INTZ depth texture not supported: 0x{:08X}", hr as u32));
        return None;
    }
    let get_level: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(tex, GET_SURFACE_LEVEL);
    let mut surf = std::ptr::null_mut();
    let hr = get_level(tex, 0, &mut surf);
    release(tex); // the surface keeps its texture alive; dxwrapper's Release frees both
    if hr < 0 || surf.is_null() {
        fail(format!("INTZ GetSurfaceLevel failed: 0x{:08X}", hr as u32));
        return None;
    }
    *out = surf;
    if let Ok(mut d) = DEPTH.lock() {
        *d = Some((surf as usize, tex as usize, w, h));
    }
    log!("ao: depth buffer {w}x{h} is an INTZ texture");
    Some(0)
}

// ---------------------------------------------------------------- per-frame tracking

/// D3D7 projection: current, and the one the world was last drawn with
/// (m00, m11, m22, m32, m23, m33; the game's matrix is scaled, so m23 isn't 1).
static PROJ: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
static WORLD_PROJ: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
static VP: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static WORLD_VP: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
static WORLD_DRAWN: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);

/// SetTransform(PROJECTION) on the D3D7 device (row-vector matrix, 16 floats).
pub unsafe fn set_projection(m: *const f32) {
    for (i, k) in [0, 5, 10, 14, 11, 15].into_iter().enumerate() {
        PROJ[i].store((*m.add(k)).to_bits(), Relaxed);
    }
}
/// The viewport really set on the device (native pixels): x, y, w, h.
pub fn set_viewport(x: u32, y: u32, w: u32, h: u32) {
    for (a, v) in VP.iter().zip([x, y, w, h]) {
        a.store(v, Relaxed);
    }
}
/// A 3D draw to the back buffer.
pub fn world_draw() {
    WORLD_DRAWN.store(true, Relaxed);
    for (w, p) in WORLD_PROJ.iter().zip(&PROJ) {
        w.store(p.load(Relaxed), Relaxed);
    }
    for (w, v) in WORLD_VP.iter().zip(&VP) {
        w.store(v.load(Relaxed), Relaxed);
    }
}
/// A 2D draw to the back buffer: if the world was drawn this frame, AO goes in first.
pub unsafe fn before_2d() {
    let perspective = f32::from_bits(WORLD_PROJ[4].load(Relaxed)) != 0.0; // m23: 0 = orthographic (menus)
    if WORLD_DRAWN.load(Relaxed) && perspective && !DONE.swap(true, Relaxed) && enabled() {
        apply();
    }
}
/// Frame boundary (Flip).
pub fn frame_end() {
    WORLD_DRAWN.store(false, Relaxed);
    DONE.store(false, Relaxed);
}

// ---------------------------------------------------------------- shaders

const HLSL: &str = r#"
float4 vp     : register(c0); // world viewport x, y, w, h (pixels)
float4 proj   : register(c1); // m00, m11, m22, m32 of the D3D7 projection
float4 params : register(c2); // radius (view units), strength, bias, max radius (pixels)
float4 rt     : register(c3); // 1/w, 1/h, w, h of the render target
float4 fade   : register(c4); // AO fades out from view depth x to y; z = debug view
float4 proj2  : register(c5); // m23, m33
float4 aort   : register(c6); // 1/w, 1/h, w, h of the (half-size) AO texture
sampler2D depthTex : register(s0);
sampler2D aoTex    : register(s1);

struct V { float4 pos : POSITION; float2 uv : TEXCOORD0; };
V vs_main(float4 pos : POSITION, float2 uv : TEXCOORD0) { V o; o.pos = pos; o.uv = uv; return o; }

float rawDepth(float2 uv) { return tex2Dlod(depthTex, float4(uv, 0, 0)).r; }
// depth = (m22 z + m32) / (m23 z + m33), solved for view z
float viewZ(float d) { return (proj.w - d * proj2.y) / (d * proj2.x - proj.z); }
float3 viewPos(float2 uv) {
    float z = viewZ(rawDepth(uv));
    float w = proj2.x * z + proj2.y;
    float2 ndc = ((uv * rt.zw - vp.xy) / vp.zw) * float2(2, -2) + float2(-1, 1);
    return float3(ndc.x * w / proj.x, ndc.y * w / proj.y, z);
}

// Snap to a depth texel centre: a half-size pixel's centre falls exactly between two
// full-size texels, and point sampling there rounds inconsistently (grid lines).
float2 snap(float2 uv) { return (floor(uv * rt.zw) + 0.5) * rt.xy; }

float4 ps_ao(float2 uv0 : TEXCOORD0, float2 pix : VPOS) : COLOR {
    float2 uv = snap(uv0);
    float d = rawDepth(uv);
    if (d >= 0.99999) return 1;
    float3 P = viewPos(uv);
    // Normal from the neighbours with the smaller depth step (no halos at silhouettes).
    float3 r = viewPos(uv + float2(rt.x, 0)) - P, l = P - viewPos(uv - float2(rt.x, 0));
    float3 t = viewPos(uv + float2(0, rt.y)) - P, b = P - viewPos(uv - float2(0, rt.y));
    float3 dx = abs(r.z) < abs(l.z) ? r : l;
    float3 dy = abs(t.z) < abs(b.z) ? t : b;
    float3 N = normalize(cross(dx, dy));
    if (N.z > 0) N = -N; // face the camera (view space looks down +z)
    float rpx = min(params.x * proj.x / (proj2.x * P.z + proj2.y) * vp.z * 0.5, params.w);
    if (rpx < 1) return 1;
    // 4x4 interleaved rotation; the blur pass averages the pattern out.
    float2 cell = fmod(pix, 4);
    float rot = (cell.x * 4 + cell.y) * (6.2831853 / 16);
    float sum = 0;
    float r2 = params.x * params.x;
    [unroll] for (int i = 0; i < 12; i++) {
        float a = i * 2.3999632 + rot;
        float s = sqrt((i + 0.5) / 12);
        float2 o = float2(cos(a), sin(a)) * s * rpx * rt.xy;
        float3 v = viewPos(snap(uv + o)) - P;
        float vv = dot(v, v);
        float falloff = saturate(1 - vv / r2);
        sum += max(0, dot(v, N) - params.z * P.z) / (vv + 0.01 * r2) * falloff;
    }
    float ao = saturate(1 - params.y * sum * (r2 / 12));
    ao = lerp(ao, 1, saturate((P.z - fade.x) / (fade.y - fade.x)));
    return float4(ao, ao, ao, 1);
}

// Full resolution: 4x4 AO texels (half size) around the pixel, weighted by depth similarity,
// which also undoes the 4x4 rotation pattern and upsamples without halos at silhouettes.
float4 ps_blur(float2 uv : TEXCOORD0) : COLOR {
    float zc = viewZ(rawDepth(uv));
    float sum = 0, w = 0;
    // The 4x4 AO texels around this pixel, addressed at exact texel centres; any 4x4 block
    // holds each rotation of the pattern once.
    float2 base = floor(uv * aort.zw - 1.5);
    [unroll] for (int y = 0; y < 4; y++) [unroll] for (int x = 0; x < 4; x++) {
        float2 t = (base + float2(x, y) + 0.5) * aort.xy;
        float z = viewZ(rawDepth(snap(t)));
        float k = saturate(1 - abs(z - zc) / (0.05 * zc));
        sum += tex2Dlod(aoTex, float4(t, 0, 0)).r * k;
        w += k;
    }
    float ao = w > 0 ? sum / w : 1;
    return float4(ao, ao, ao, 1);
}
"#;

type Compile = unsafe extern "system" fn(*const c_void, usize, *const u8, P, P, *const u8, *const u8, u32, u32, *mut P, *mut P) -> HRESULT;

unsafe fn compile(f: Compile, entry: &str, target: &str) -> Result<Vec<u32>, String> {
    let (e, t) = (format!("{entry}\0"), format!("{target}\0"));
    let (mut code, mut errors) = (std::ptr::null_mut(), std::ptr::null_mut());
    const OPTIMIZATION_LEVEL3: u32 = 1 << 15;
    let hr = f(HLSL.as_ptr().cast(), HLSL.len(), c"desiege_ao".as_ptr().cast(), std::ptr::null_mut(), std::ptr::null_mut(), e.as_ptr(), t.as_ptr(), OPTIMIZATION_LEVEL3, 0, &mut code, &mut errors);
    // ID3DBlob: GetBufferPointer = 3, GetBufferSize = 4
    let blob = |b: P| -> Vec<u8> {
        let ptr: unsafe extern "system" fn(P) -> *const u8 = method(b, 3);
        let len: unsafe extern "system" fn(P) -> usize = method(b, 4);
        std::slice::from_raw_parts(ptr(b), len(b)).to_vec()
    };
    let msg = if errors.is_null() { String::new() } else { String::from_utf8_lossy(&blob(errors)).into_owned() };
    release(errors);
    if hr < 0 || code.is_null() {
        return Err(format!("{entry}: 0x{:08X} {msg}", hr as u32));
    }
    let bytes = blob(code);
    release(code);
    Ok(bytes.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap_or([0; 4]))).collect())
}

/// Compiled shaders are cached in %LOCALAPPDATA%\Desiegeo-<hash of the source>.bin
/// (compiling takes ~1.7 s, a visible hitch when the first world frame is drawn).
fn cache_path() -> Option<std::path::PathBuf> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a
    for b in HLSL.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    Some(std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Desiege").join(format!("ao-{h:016x}.bin")))
}
fn load_cache() -> Option<[Vec<u32>; 3]> {
    let bytes = std::fs::read(cache_path()?).ok()?;
    let words: Vec<u32> = bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
    let mut out: [Vec<u32>; 3] = Default::default();
    let mut i = 0;
    for o in &mut out {
        let n = *words.get(i)? as usize;
        *o = words.get(i + 1..i + 1 + n)?.to_vec();
        i += 1 + n;
    }
    Some(out)
}
fn save_cache(c: &[Vec<u32>; 3]) {
    let Some(path) = cache_path() else { return };
    let mut bytes = Vec::new();
    for v in c {
        bytes.extend_from_slice(&(v.len() as u32).to_le_bytes());
        v.iter().for_each(|w| bytes.extend_from_slice(&w.to_le_bytes()));
    }
    let _ = std::fs::write(path, bytes);
}

struct Gpu {
    dev: usize,
    vs: usize,
    ps_ao: usize,
    ps_blur: usize,
    state: usize,
    /// AO render target texture + its surface, and their size.
    target: Option<(usize, usize, u32, u32)>,
    /// References our objects hold on the device (see d3d9::dev9_release).
    refs: u32,
}

/// How many references we hold on `dev`, if it's the device we have objects on. None while
/// our lock is held (that's us, inside a pass or setup, not a teardown).
pub fn refs_held_on(dev: P) -> Option<u32> {
    let g = GPU.try_lock().ok()?;
    g.as_ref().filter(|g| g.dev == dev as usize).map(|g| g.refs)
}
static GPU: Mutex<Option<Gpu>> = Mutex::new(None);

unsafe fn release(p: P) {
    if !p.is_null() {
        method::<unsafe extern "system" fn(P) -> u32>(p, 2)(p);
    }
}

unsafe fn setup(dev: P) -> Result<Gpu, String> {
    let path: Vec<u16> = "d3dcompiler_47.dll\0".encode_utf16().collect();
    let lib = LoadLibraryW(path.as_ptr());
    if lib.is_null() {
        return Err("d3dcompiler_47.dll not found".into());
    }
    let t0 = std::time::Instant::now();
    let [vs, ps_ao, ps_blur] = match load_cache() {
        Some(c) => c,
        None => {
            let f: Compile = std::mem::transmute(GetProcAddress(lib, c"D3DCompile".as_ptr().cast()).ok_or("D3DCompile missing")?);
            let c = [compile(f, "vs_main", "vs_3_0")?, compile(f, "ps_ao", "ps_3_0")?, compile(f, "ps_blur", "ps_3_0")?];
            save_cache(&c);
            c
        }
    };
    let before = d3d9::refcount(dev);
    let create_vs: unsafe extern "system" fn(P, *const u32, *mut P) -> HRESULT = method(dev, CREATE_VERTEX_SHADER);
    let create_ps: unsafe extern "system" fn(P, *const u32, *mut P) -> HRESULT = method(dev, CREATE_PIXEL_SHADER);
    let mut g = Gpu { dev: dev as usize, vs: 0, ps_ao: 0, ps_blur: 0, state: 0, target: None, refs: 0 };
    let mut p = std::ptr::null_mut();
    if create_vs(dev, vs.as_ptr(), &mut p) < 0 { return Err("CreateVertexShader failed".into()) }
    g.vs = p as usize;
    if create_ps(dev, ps_ao.as_ptr(), &mut p) < 0 { return Err("CreatePixelShader (ao) failed".into()) }
    g.ps_ao = p as usize;
    if create_ps(dev, ps_blur.as_ptr(), &mut p) < 0 { return Err("CreatePixelShader (blur) failed".into()) }
    g.ps_blur = p as usize;
    let create_sb: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(dev, CREATE_STATE_BLOCK);
    const SBT_ALL: u32 = 1;
    if create_sb(dev, SBT_ALL, &mut p) < 0 { return Err("CreateStateBlock failed".into()) }
    g.state = p as usize;
    g.refs = d3d9::refcount(dev).saturating_sub(before);
    log!("ao: shaders ready in {} ms", t0.elapsed().as_millis());
    Ok(g)
}

impl Gpu {
    /// Release everything (before Reset, or when the device goes away).
    unsafe fn free(&mut self) {
        for p in [self.vs, self.ps_ao, self.ps_blur, self.state] {
            release(p as P);
        }
        if let Some((t, s, _, _)) = self.target.take() {
            release(s as P);
            release(t as P);
        }
        self.vs = 0;
        self.ps_ao = 0;
        self.ps_blur = 0;
        self.state = 0;
    }
}

/// The device is about to Reset or be replaced: our D3D9 objects must go first.
pub unsafe fn device_lost() {
    if let Ok(mut g) = GPU.lock() {
        if let Some(mut gpu) = g.take() {
            gpu.free();
        }
    }
    if let Ok(mut d) = DEPTH.lock() {
        *d = None;
    }
}

// ---------------------------------------------------------------- the passes

unsafe fn apply() {
    let dev = d3d9::device();
    if dev.is_null() {
        return;
    }
    let Some((depth_surf, depth_tex, w, h)) = DEPTH.lock().ok().and_then(|d| *d) else { return };
    // Only while our INTZ surface is the bound depth buffer.
    let get_ds: unsafe extern "system" fn(P, *mut P) -> HRESULT = method(dev, GET_DEPTH_STENCIL);
    let mut ds = std::ptr::null_mut();
    if get_ds(dev, &mut ds) < 0 || ds.is_null() {
        return;
    }
    release(ds);
    if ds as usize != depth_surf {
        return;
    }
    let Ok(mut guard) = GPU.lock() else { return };
    if guard.as_ref().is_some_and(|g| g.dev != dev as usize) {
        if let Some(mut old) = guard.take() {
            old.free();
        }
    }
    if guard.is_none() {
        match setup(dev) {
            Ok(g) => *guard = Some(g),
            Err(e) => {
                fail(e);
                return;
            }
        }
    }
    let g = guard.as_mut().unwrap();
    let (aw, ah) = (w.div_ceil(2), h.div_ceil(2));
    if !g.target.is_some_and(|t| t.2 == aw && t.3 == ah) {
        if let Some((t, s, _, _)) = g.target.take() {
            let before = d3d9::refcount(dev);
            release(s as P);
            release(t as P);
            g.refs = g.refs.saturating_sub(before - d3d9::refcount(dev));
        }
        let create: unsafe extern "system" fn(P, u32, u32, u32, u32, u32, u32, *mut P, P) -> HRESULT = method(dev, CREATE_TEXTURE);
        let before = d3d9::refcount(dev);
        let old_refs = g.refs;
        let mut tex = std::ptr::null_mut();
        if create(dev, aw, ah, 1, USAGE_RENDERTARGET, FMT_A8R8G8B8, POOL_DEFAULT, &mut tex, std::ptr::null_mut()) < 0 {
            fail("AO render target create failed".into());
            return;
        }
        let get_level: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(tex, GET_SURFACE_LEVEL);
        let mut surf = std::ptr::null_mut();
        get_level(tex, 0, &mut surf);
        g.target = Some((tex as usize, surf as usize, aw, ah));
        g.refs = old_refs + d3d9::refcount(dev).saturating_sub(before);
    }
    let (ao_tex, ao_surf, _, _) = g.target.unwrap();

    let capture: unsafe extern "system" fn(P) -> HRESULT = method(g.state as P, SB_CAPTURE);
    let restore: unsafe extern "system" fn(P) -> HRESULT = method(g.state as P, SB_APPLY);
    let get_rt: unsafe extern "system" fn(P, u32, *mut P) -> HRESULT = method(dev, GET_RENDER_TARGET);
    let set_rt: unsafe extern "system" fn(P, u32, P) -> HRESULT = method(dev, SET_RENDER_TARGET);
    let set_ds: unsafe extern "system" fn(P, P) -> HRESULT = method(dev, SET_DEPTH_STENCIL);
    let set_vp: unsafe extern "system" fn(P, *const u32) -> HRESULT = method(dev, SET_VIEWPORT);
    let set_rs: unsafe extern "system" fn(P, u32, u32) -> HRESULT = method(dev, SET_RENDER_STATE);
    let set_tex: unsafe extern "system" fn(P, u32, P) -> HRESULT = method(dev, SET_TEXTURE);
    let set_ss: unsafe extern "system" fn(P, u32, u32, u32) -> HRESULT = method(dev, SET_SAMPLER_STATE);
    let set_fvf: unsafe extern "system" fn(P, u32) -> HRESULT = method(dev, SET_FVF);
    let set_vs: unsafe extern "system" fn(P, P) -> HRESULT = method(dev, SET_VERTEX_SHADER);
    let set_ps: unsafe extern "system" fn(P, P) -> HRESULT = method(dev, SET_PIXEL_SHADER);
    let set_psc: unsafe extern "system" fn(P, u32, *const f32, u32) -> HRESULT = method(dev, SET_PS_CONST_F);
    let draw_up: unsafe extern "system" fn(P, u32, u32, *const c_void, u32) -> HRESULT = method(dev, DRAW_PRIMITIVE_UP);

    capture(g.state as P);
    let mut back = std::ptr::null_mut();
    get_rt(dev, 0, &mut back);

    let c = gfx::config();
    let f = |a: &AtomicU32| f32::from_bits(a.load(Relaxed));
    let wvp: Vec<f32> = WORLD_VP.iter().map(|a| a.load(Relaxed) as f32).collect();
    let consts: [f32; 28] = [
        wvp[0], wvp[1], wvp[2].max(1.0), wvp[3].max(1.0),
        f(&WORLD_PROJ[0]), f(&WORLD_PROJ[1]), f(&WORLD_PROJ[2]), f(&WORLD_PROJ[3]),
        c.ao_radius, c.ao_strength, 0.004, 80.0 * w as f32 / 1920.0,
        1.0 / w as f32, 1.0 / h as f32, w as f32, h as f32,
        c.ao_fade_start, c.ao_fade_end, c.ao_debug as u32 as f32, 0.0,
        f(&WORLD_PROJ[4]), f(&WORLD_PROJ[5]), 0.0, 0.0,
        1.0 / aw as f32, 1.0 / ah as f32, aw as f32, ah as f32,
    ];
    // Unbind depth (it's sampled), no z, no blending (pass 1), no fog/stencil/alpha test.
    set_ds(dev, std::ptr::null_mut());
    for (k, v) in [(7, 0), (14, 0), (27, 0), (15, 0), (22, 1), (28, 0), (52, 0), (168, 0xF), (174, 0), (194, 0)] {
        set_rs(dev, k, v);
    }
    for s in 0..2 {
        for (k, v) in [(1, 3), (2, 3), (5, 1), (6, 1), (7, 0)] { // clamp, point, no mips
            set_ss(dev, s, k, v);
        }
    }
    set_fvf(dev, 0x102); // XYZ | TEX1 (the vertex shader takes clip-space positions)
    set_vs(dev, g.vs as P);
    set_psc(dev, 0, consts.as_ptr(), 7);
    // Full-target quad, shifted half a pixel of that target (D3D9 pixel centres).
    let quad = |w: u32, h: u32| -> [f32; 20] {
        let (hx, hy) = (1.0 / w as f32, 1.0 / h as f32);
        [
            -1.0 - hx, 1.0 + hy, 0.5, 0.0, 0.0,
            1.0 - hx, 1.0 + hy, 0.5, 1.0, 0.0,
            -1.0 - hx, -1.0 + hy, 0.5, 0.0, 1.0,
            1.0 - hx, -1.0 + hy, 0.5, 1.0, 1.0,
        ]
    };
    let full = [0u32, 0, w, h, 0f32.to_bits(), 1f32.to_bits()];
    let half = [0u32, 0, aw, ah, 0f32.to_bits(), 1f32.to_bits()];
    const TRIANGLESTRIP: u32 = 5;

    // Pass 1: AO into our half-size texture.
    set_rt(dev, 0, ao_surf as P);
    set_vp(dev, half.as_ptr());
    set_tex(dev, 0, depth_tex as P);
    set_tex(dev, 1, std::ptr::null_mut());
    set_ps(dev, g.ps_ao as P);
    draw_up(dev, TRIANGLESTRIP, 2, quad(aw, ah).as_ptr().cast(), 20);

    // Pass 2: blur and multiply onto the back buffer (or show it, for tuning).
    set_rt(dev, 0, back);
    set_vp(dev, full.as_ptr());
    set_tex(dev, 1, ao_tex as P);
    if !c.ao_debug {
        const BLEND_ZERO: u32 = 1; const BLEND_SRCCOLOR: u32 = 3;
        set_rs(dev, 27, 1);
        set_rs(dev, 19, BLEND_ZERO);
        set_rs(dev, 20, BLEND_SRCCOLOR);
    }
    set_ps(dev, g.ps_blur as P);
    draw_up(dev, TRIANGLESTRIP, 2, quad(w, h).as_ptr().cast(), 20);

    set_tex(dev, 0, std::ptr::null_mut());
    set_tex(dev, 1, std::ptr::null_mut());
    set_ds(dev, depth_surf as P);
    restore(g.state as P);
    release(back);
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    let n = LOGGED.fetch_add(1, Relaxed);
    if n == 0 || n == 3000 {
        let (m22, m32, m23, m33) = (consts[6], consts[7], consts[20], consts[21]);
        let z = |d: f32| (m32 - d * m33) / (d * m23 - m22);
        log!("ao: frame {n}: viewport {:?} projection m00={:.3} m11={:.3} m22={m22:.5} m32={m32:.5} m23={m23:.5} m33={m33:.5}; view z at depth 0 / 0.5 / 0.9 / 0.99: {:.2} / {:.2} / {:.2} / {:.2}",
            &wvp, consts[4], consts[5], z(0.0), z(0.5), z(0.9), z(0.99));
    }
}
