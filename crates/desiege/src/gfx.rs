//! Optional graphics enhancements, each with its own switch. None of them changes the game's
//! art: they render the original assets the way current hardware can.
//!
//! - `filter`: texture filtering. 0 = the game's own, 1 = trilinear (linear between mipmap
//!   levels), 2..16 = anisotropic with that many samples (implies trilinear). Done by upgrading
//!   the sampler states the game sets (SetTextureStageState).
//! - `hq_formats`: 32-bit textures and a 24-bit or deeper depth buffer. The 2002 hardware tables
//!   put some GPUs (Intel) on 16-bit colour, and the game may pick 16-bit formats from the
//!   enumerations; 16-bit formats are hidden from it when a deeper one exists.
//! - `max_detail`: the per-vendor "this card is too slow" flags in system_detail.gas
//!   (`simple_render`, `no_complex_shadows`, `below_min_spec`) are cleared. Today's AMD cards
//!   fall into the 2002 "ATI" block, which sets all three.
//! - `soft_shadows`: character shadow textures get a blur pass (see scale.rs `shadow`).
//! - `msaa`: multisample anti-aliasing; done by dxwrapper (`AntiAliasing` in its ini, written by
//!   the launcher). Only logged here.
//!
//! Settings come from a `graphics` file next to this DLL (`key=value` lines, written by the
//! launcher), overridden by DESIEGE_GFX (`key=value,key=value`). Missing keys = defaults below.

use super::{patch, rd, HRESULT, P};
use std::{
    ffi::c_void,
    sync::{atomic::{AtomicUsize, Ordering::Relaxed}, Mutex, OnceLock},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Config {
    pub filter: u32,
    pub hq_formats: bool,
    pub max_detail: bool,
    pub soft_shadows: bool,
    pub msaa: u32,
    /// Ambient occlusion (ao.rs): 0 = off, 1 = on. Tuning: radius in view units, strength,
    /// fade-out range in view depth, debug = show the AO term alone.
    pub ao: u32,
    pub ao_radius: f32,
    pub ao_strength: f32,
    pub ao_fade_start: f32,
    pub ao_fade_end: f32,
    pub ao_debug: bool,
    /// Spike: log the engine's lights once in-game (lights.rs).
    pub lights_probe: u32,
    /// Per-pixel dynamic lights (lights.rs + ao.rs): 0 = off, 1 = on; strength of the added
    /// light; debug = show the light term alone.
    pub lights: u32,
    pub lights_strength: f32,
    pub lights_debug: bool,
    /// Extra factor for the light that follows the party (it's white and strong).
    pub lights_party: f32,
    /// Contrast-adaptive sharpening of the 3D image, 0 = off .. 1 = strongest (ao.rs).
    pub sharpen: f32,
    /// Temporary: virtual-key code that toggles AO + per-pixel lights in-game (0 = none; Insert,
    /// which the game leaves unbound; F9 is its quick save).
    pub toggle_key: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            filter: 16, hq_formats: true, max_detail: true, soft_shadows: true, msaa: 0,
            ao: 0, ao_radius: 1.0, ao_strength: 1.3, ao_fade_start: 25.0, ao_fade_end: 40.0, ao_debug: false, lights_probe: 0,
            lights: 0, lights_strength: 0.3, lights_debug: false, lights_party: 0.4, sharpen: 0.0, toggle_key: 0x2D,
        }
    }
}

impl Config {
    /// Applies `key=value` items (lines or comma-separated); unknown keys are ignored.
    pub fn apply<'a>(&mut self, items: impl Iterator<Item = &'a str>) {
        for item in items {
            let Some((k, v)) = item.split_once('=') else { continue };
            let v = v.trim();
            let flag = matches!(v, "1" | "true" | "on" | "yes");
            match k.trim() {
                "filter" => self.filter = v.parse().unwrap_or(self.filter).min(16),
                "hq_formats" => self.hq_formats = flag,
                "max_detail" => self.max_detail = flag,
                "soft_shadows" => self.soft_shadows = flag,
                "msaa" => self.msaa = v.parse().unwrap_or(self.msaa).min(16),
                "ao" => self.ao = v.parse().unwrap_or(self.ao).min(1),
                "ao_radius" => self.ao_radius = v.parse().unwrap_or(self.ao_radius),
                "ao_strength" => self.ao_strength = v.parse().unwrap_or(self.ao_strength),
                "ao_fade_start" => self.ao_fade_start = v.parse().unwrap_or(self.ao_fade_start),
                "ao_fade_end" => self.ao_fade_end = v.parse().unwrap_or(self.ao_fade_end),
                "ao_debug" => self.ao_debug = flag,
                "lights_probe" => self.lights_probe = v.parse().unwrap_or(0),
                "lights" => self.lights = v.parse().unwrap_or(self.lights).min(1),
                "lights_strength" => self.lights_strength = v.parse().unwrap_or(self.lights_strength),
                "lights_debug" => self.lights_debug = flag,
                "lights_party" => self.lights_party = v.parse().unwrap_or(self.lights_party),
                "sharpen" => self.sharpen = v.parse::<f32>().unwrap_or(self.sharpen).clamp(0.0, 1.0),
                "toggle_key" => self.toggle_key = v.strip_prefix("0x").map_or_else(|| v.parse().ok(), |h| u32::from_str_radix(h, 16).ok()).unwrap_or(self.toggle_key),
                _ => {}
            }
        }
    }
}

pub fn config() -> Config {
    static C: OnceLock<Config> = OnceLock::new();
    *C.get_or_init(|| {
        let mut c = Config::default();
        if let Some(text) = crate::self_dir().and_then(|d| std::fs::read_to_string(d.join("graphics")).ok()) {
            c.apply(text.lines());
        }
        if let Ok(env) = std::env::var("DESIEGE_GFX") {
            c.apply(env.split(','));
        }
        log!("graphics: {c:?}");
        c
    })
}

// ---------------------------------------------------------------- system_detail.gas

/// Flags in system_detail.gas that only exist to spare slow 2002 cards. Cleared everywhere
/// (vendor defaults and per-device entries) with `max_detail`; `only_16_bit` with `hq_formats`,
/// and `trilinear_filt` (the default of the game's own "trilinear" option) is set with `filter`.
pub fn patch_gas(gas: &str, c: Config) -> String {
    let mut rules: Vec<(&str, &str, &str)> = Vec::new();
    if c.max_detail {
        rules.extend([("simple_render", "true", "false"), ("no_complex_shadows", "true", "false"), ("below_min_spec", "true", "false")]);
    }
    if c.hq_formats {
        rules.push(("only_16_bit", "true", "false"));
    }
    if c.filter > 0 {
        rules.push(("trilinear_filt", "false", "true"));
    }
    gas.lines()
        .map(|l| {
            let t = l.trim_start();
            for (key, from, to) in &rules {
                if t.strip_prefix("b ").is_some_and(|r| r.trim_start().starts_with(key) && r[key.len()..].trim_start().starts_with('=')) && t.contains(from) {
                    return l.replacen(from, to, 1);
                }
            }
            l.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if gas.ends_with('\n') { "\n" } else { "" }
}

// ---------------------------------------------------------------- format enumerations

/// DDPIXELFORMAT: dwSize, dwFlags@4, dwFourCC@8, dwRGBBitCount/ZBufferBitDepth@12, masks @16..
const DDPF_ALPHAPIXELS: u32 = 0x1; const DDPF_FOURCC: u32 = 0x4; const DDPF_RGB: u32 = 0x40;
const DDPF_ZBUFFER: u32 = 0x400;

/// Plain 16-bit RGB(A) texture format (565, 555, 1555, 4444), the kind with a 32-bit twin.
unsafe fn is_16bit_rgb(pf: *const c_void) -> bool {
    let flags = rd(pf, 4);
    flags & DDPF_RGB != 0 && flags & DDPF_FOURCC == 0 && rd(pf, 12) == 16
}

/// Collects a device's texture formats first, then reports them minus the 16-bit RGB ones if
/// 32-bit RGB (with and without alpha) are there to take their place.
pub type FmtCb = unsafe extern "system" fn(*mut c_void, P) -> HRESULT;
struct Collect(Vec<[u8; 32]>);
unsafe extern "system" fn collect_cb(pf: *mut c_void, ctx: P) -> HRESULT {
    let mut f = [0u8; 32];
    std::ptr::copy_nonoverlapping(pf as *const u8, f.as_mut_ptr(), 32);
    (*(ctx as *mut Collect)).0.push(f);
    1 // D3DENUMRET_OK
}

fn describe(pf: &[u8; 32]) -> String {
    let u = |o: usize| u32::from_le_bytes(pf[o..o + 4].try_into().unwrap());
    if u(4) & DDPF_FOURCC != 0 {
        let cc = pf[8..12].iter().map(|&b| b as char).collect::<String>();
        return format!("fourcc {cc}");
    }
    format!("flags=0x{:X} {}bpp r={:X} g={:X} b={:X} a={:X}", u(4), u(12), u(16), u(20), u(24), u(28))
}

orig!(DEV_ENUM_TEX: fn(P, FmtCb, P) -> HRESULT);
unsafe extern "system" fn dev_enum_tex(this: P, cb: FmtCb, ctx: P) -> HRESULT {
    let mut all = Collect(Vec::new());
    let hr = call!(DEV_ENUM_TEX as fn(P, FmtCb, P) -> HRESULT; this, collect_cb, &mut all as *mut _ as P);
    if hr < 0 {
        return hr;
    }
    let u = |f: &[u8; 32], o: usize| u32::from_le_bytes(f[o..o + 4].try_into().unwrap());
    let rgb32 = |f: &[u8; 32]| u(f, 4) & DDPF_RGB != 0 && u(f, 4) & DDPF_FOURCC == 0 && u(f, 12) == 32;
    let has32 = all.0.iter().any(|f| rgb32(f) && u(f, 4) & DDPF_ALPHAPIXELS != 0) && all.0.iter().any(rgb32);
    let hide = config().hq_formats && has32;
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    let log_it = LOGGED.fetch_add(1, Relaxed) < 2;
    for f in &mut all.0 {
        let skip = hide && is_16bit_rgb(f.as_ptr().cast());
        if log_it {
            log!("  texture format {}{}", describe(f), if skip { " (hidden: hq_formats)" } else { "" });
        }
        if !skip && cb(f.as_mut_ptr().cast(), ctx) == 0 {
            break;
        }
    }
    hr
}

/// EnumZBufferFormats filter (called from lib.rs for each format): false = hide it from the game.
pub unsafe fn keep_zbuffer(pf: *const c_void, deeper_exists: bool) -> bool {
    !(config().hq_formats && deeper_exists && rd(pf, 4) & DDPF_ZBUFFER != 0 && rd(pf, 12) < 24)
}

/// For the CreateSurface log: what pixel format a texture / z-buffer got. Each distinct
/// (caps, format) pair is logged once, with how many surfaces have used it so far.
pub unsafe fn note_surface(desc: *const c_void) {
    const DDSD_PIXELFORMAT: u32 = 0x1000; const TEXTURE: u32 = 0x1000; const ZBUFFER: u32 = 0x20000; const MIPMAP: u32 = 0x40_0000;
    let caps = rd(desc, 104);
    if caps & (TEXTURE | ZBUFFER) == 0 {
        return;
    }
    let pf = (desc as *const u8).add(72);
    let key = if rd(desc, 4) & DDSD_PIXELFORMAT != 0 {
        let mut f = [0u8; 32];
        std::ptr::copy_nonoverlapping(pf, f.as_mut_ptr(), 32);
        describe(&f)
    } else {
        "(no pixel format: primary's)".into()
    };
    let kind = if caps & ZBUFFER != 0 {
        "zbuffer"
    } else if caps & MIPMAP != 0 {
        "texture+mips"
    } else {
        "texture"
    };
    static SEEN: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());
    let Ok(mut seen) = SEEN.lock() else { return };
    let id = format!("{kind} {key}");
    match seen.iter_mut().find(|(k, _)| *k == id) {
        Some((_, n)) => {
            *n += 1;
            if n.is_power_of_two() && *n >= 64 {
                log!("surface format: {id} x{n}");
            }
        }
        None => {
            log!("surface format: {id} (first; {}x{} caps=0x{caps:X})", rd(desc, 12), rd(desc, 8));
            seen.push((id, 1));
        }
    }
}

// ---------------------------------------------------------------- texture filtering

const MAGFILTER: u32 = 16; const MINFILTER: u32 = 17; const MIPFILTER: u32 = 18; const MAXANISOTROPY: u32 = 21;
const TFN_LINEAR: u32 = 2; const TFN_ANISOTROPIC: u32 = 3;
const TFP_POINT: u32 = 2; const TFP_LINEAR: u32 = 3;

/// What a sampler state becomes. Point filtering is the game's (or our UI scaler's) choice for
/// crisp 2D and is left alone, as is "no mipmaps".
pub fn upgrade(state: u32, value: u32, filter: u32) -> u32 {
    match state {
        MINFILTER if filter >= 2 && value == TFN_LINEAR => TFN_ANISOTROPIC,
        MIPFILTER if filter >= 1 && value == TFP_POINT => TFP_LINEAR,
        MAXANISOTROPY if filter >= 2 => filter,
        _ => value,
    }
}

orig!(DEV_SET_TSS: fn(P, u32, u32, u32) -> HRESULT);
unsafe extern "system" fn dev_set_tss(this: P, stage: u32, state: u32, value: u32) -> HRESULT {
    let f = config().filter;
    note_state(stage, state, value);
    let v = upgrade(state, value, f);
    // The game itself sometimes asks for anisotropic (with its trilinear option) but never sets
    // a sample count, so it stays at 1; set ours alongside.
    if f >= 2 && state == MINFILTER && v == TFN_ANISOTROPIC {
        call!(DEV_SET_TSS as fn(P, u32, u32, u32) -> HRESULT; this, stage, MAXANISOTROPY, f);
    }
    call!(DEV_SET_TSS as fn(P, u32, u32, u32) -> HRESULT; this, stage, state, v)
}

/// Log of the filter states the game asks for (each distinct one once; a few dozen lines).
fn note_state(stage: u32, state: u32, value: u32) {
    if !matches!(state, MAGFILTER | MINFILTER | MIPFILTER | MAXANISOTROPY) {
        return;
    }
    static SEEN: Mutex<Vec<(u32, u32, u32)>> = Mutex::new(Vec::new());
    if let Ok(mut s) = SEEN.lock() {
        if s.len() < 64 && !s.contains(&(stage, state, value)) {
            s.push((stage, state, value));
            log!("texture stage {stage}: state {state} = {value} (game)");
        }
    }
}

/// IDirect3DDevice7: EnumTextureFormats = 4, SetTextureStageState = 37.
pub unsafe fn hook_device(dev: P) {
    patch(dev, 4, dev_enum_tex as usize, &DEV_ENUM_TEX);
    patch(dev, 37, dev_set_tss as usize, &DEV_SET_TSS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let mut c = Config::default();
        c.apply("filter=4\nsoft_shadows=0\nmsaa = 8\nbogus=1".lines());
        assert_eq!(c, Config { filter: 4, soft_shadows: false, msaa: 8, ..Config::default() });
        c.apply("filter=99,hq_formats=off".split(','));
        assert_eq!((c.filter, c.hq_formats), (16, false));
    }

    #[test]
    fn parses_ao_settings() {
        let mut c = Config::default();
        assert_eq!(c.ao, 0); // experimental: off unless asked for
        c.apply("ao=1,ao_radius=1.5,ao_strength=2,ao_debug=1,ao=7".split(','));
        assert_eq!((c.ao, c.ao_radius, c.ao_strength, c.ao_debug), (1, 1.5, 2.0, true));
    }

    #[test]
    fn upgrades_only_linear_filters() {
        assert_eq!(upgrade(MINFILTER, TFN_LINEAR, 16), TFN_ANISOTROPIC);
        assert_eq!(upgrade(MINFILTER, 1, 16), 1); // point stays point
        assert_eq!(upgrade(MINFILTER, TFN_LINEAR, 1), TFN_LINEAR); // trilinear only
        assert_eq!(upgrade(MIPFILTER, TFP_POINT, 1), TFP_LINEAR);
        assert_eq!(upgrade(MIPFILTER, 1, 16), 1); // no mipmaps stays none
        assert_eq!(upgrade(MIPFILTER, TFP_POINT, 0), TFP_POINT); // off = untouched
        assert_eq!(upgrade(MAXANISOTROPY, 1, 8), 8);
    }

    #[test]
    fn patches_gas_flags() {
        let gas = "\t\tb simple_render\t\t\t\t= true;\n\t\tb no_complex_shadows\t= true;\n\t\tb only_16_bit = true;\n\t\tb trilinear_filt\t= false;\n\t\tb no_flip = true;\n";
        let all = patch_gas(gas, Config::default());
        assert_eq!(all, "\t\tb simple_render\t\t\t\t= false;\n\t\tb no_complex_shadows\t= false;\n\t\tb only_16_bit = false;\n\t\tb trilinear_filt\t= true;\n\t\tb no_flip = true;\n");
        let off = Config { filter: 0, hq_formats: false, max_detail: false, ..Config::default() };
        assert_eq!(patch_gas(gas, off), gas);
    }
}
