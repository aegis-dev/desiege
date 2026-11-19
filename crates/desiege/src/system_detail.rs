//! system_detail.gas, the game's 2002 hardware table, maps VRAM tier x colour depth to allowed
//! resolutions (max 1024x768) and per-vendor quirks; anything else triggers "requested display
//! mode ... is unsupported". Rather than edit the game's file, the game's CreateFileA is hooked
//! and opens of system_detail.gas are redirected to a patched copy in %LOCALAPPDATA%\Desiege:
//! the launch resolution added, page flipping on for every vendor, and the graphics options'
//! flags (gfx::patch_gas). Every other file the game opens is logged, as are folder scans.

use crate::{cstr, gfx, P};
use std::{
    ffi::{c_char, CStr, CString},
    fs,
    path::PathBuf,
    sync::{atomic::{AtomicUsize, Ordering::Relaxed}, OnceLock},
};


/// Adds `WxH` to the top 32-bit tier. None = already listed (or file layout unexpected).
// ponytail: only touches [64M32B], the tier any modern card lands in (with the VRAM cap),
// since the launcher always passes bpp=32.
fn add_resolution(gas: &str, w: u32, h: u32) -> Option<String> {
    let tier = gas.find("\t[64M32B]")?;
    let tier_end = gas[tier + 1..].find("\n\t[").map_or(gas.len(), |i| tier + 1 + i);
    let block = &gas[tier..tier_end];
    if block.contains(&format!("[{w}x{h}]")) {
        return None;
    }
    let res = block.find("[resolutions]")?;
    let open = tier + res + block[res..].find('{')? + 1;
    Some(format!("{}\n\t\t\t[{w}x{h}]\t{{ max_back_buffers = 2; }}{}", &gas[..open], &gas[open..]))
}

/// `width=`/`height=` from `key=value` items (command-line args or ini lines; spaces around
/// `=` allowed, as in DungeonSiege.ini). Last one wins, like the game's own parsing.
fn find_resolution<'a>(items: impl Iterator<Item = &'a str>) -> Option<(u32, u32)> {
    let (mut w, mut h) = (None, None);
    for item in items {
        let Some((k, v)) = item.split_once('=') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "width" => w = v.trim().parse().ok(),
            "height" => h = v.trim().parse().ok(),
            _ => {}
        }
    }
    Some((w?, h?))
}

/// The launcher passes width=/height= on the command line. A Steam launch may not, so fall
/// back to what the game itself will use: Documents\Dungeon Siege\DungeonSiege.ini.
// ponytail: assumes Documents is %USERPROFILE%\Documents; use SHGetKnownFolderPath if a user
// with a relocated Documents folder reports the "unsupported mode" warning on Steam launches.
pub fn launch_resolution() -> Option<(u32, u32)> {
    let args: Vec<String> = std::env::args().collect();
    find_resolution(args.iter().map(String::as_str)).or_else(|| {
        let ini = PathBuf::from(std::env::var_os("USERPROFILE")?).join(r"Documents\Dungeon Siege\DungeonSiege.ini");
        find_resolution(fs::read_to_string(ini).ok()?.lines())
    })
}

/// Sets `no_flip = false` for every GPU vendor. With no_flip (the 2002 setting for nVidia
/// and others) the game presents each frame by Blt-copying the whole back buffer to the front
/// buffer, which under D3D9 translation is a full-screen GPU->CPU->GPU round trip; that halved
/// the frame rate (55 -> 112 fps at 2560x1440). Flipping is free.
fn enable_flip(gas: &str) -> String {
    gas.lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("b no_flip") && t.contains("true") {
                l.replacen("true", "false", 1)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if gas.ends_with('\n') { "\n" } else { "" }
}

/// Writes the patched copy; None means "open the original".
fn patched_gas(original: &str) -> Option<CString> {
    let text = fs::read_to_string(original).ok()?;
    let with_res = launch_resolution().and_then(|(w, h)| add_resolution(&text, w, h)).unwrap_or_else(|| text.clone());
    let patched = gfx::patch_gas(&enable_flip(&with_res), gfx::config());
    if patched == text {
        return None;
    }
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Desiege");
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join("system_detail.gas");
    fs::write(&path, patched).ok()?;
    // ponytail: CreateFileA takes an ANSI path; a non-ASCII profile path just falls back to
    // the original file (warning comes back, nothing breaks).
    CString::new(path.to_str().filter(|s| s.is_ascii())?).ok()
}

orig!(pub CREATE_FILE_A: fn(*const c_char, u32, u32, P, u32, u32, P) -> P);

pub unsafe extern "system" fn create_file_a(
    name: *const c_char, access: u32, share: u32, sa: P, disposition: u32, flags: u32, template: P,
) -> P {
    let mut name = name;
    if !name.is_null() && CStr::from_ptr(name).to_bytes().to_ascii_lowercase().ends_with(b"system_detail.gas") {
        static REDIRECT: OnceLock<Option<CString>> = OnceLock::new();
        let original = CStr::from_ptr(name).to_string_lossy().into_owned();
        let redirect = REDIRECT.get_or_init(|| patched_gas(&original));
        log!("CreateFileA({original}) -> {}", redirect.as_ref().map_or("original".into(), |p| p.to_string_lossy()));
        if let Some(p) = redirect {
            name = p.as_ptr();
        }
    } else if !name.is_null() {
        // ponytail: logs every file the game opens (it opens few: tanks are memory-mapped).
        // Useful while mapping out mod loading; gate it behind a setting if it gets noisy.
        log!("CreateFileA({})", cstr(name));
    }
    call!(CREATE_FILE_A as fn(*const c_char, u32, u32, P, u32, u32, P) -> P;
        name, access, share, sa, disposition, flags, template)
}

orig!(pub FIND_FIRST_FILE_A: fn(*const c_char, P) -> P);

/// Logged to see which folders the game scans for .dsres / .dsmod / .dsmap tanks.
pub unsafe extern "system" fn find_first_file_a(pattern: *const c_char, data: P) -> P {
    let h = call!(FIND_FIRST_FILE_A as fn(*const c_char, P) -> P; pattern, data);
    log!("FindFirstFileA({}) -> {}", cstr(pattern), if h as isize == -1 { "none" } else { "found" });
    h
}


#[cfg(test)]
mod tests {
    use super::{add_resolution, find_resolution};

    #[test]
    fn finds_resolution_in_args_and_ini() {
        let args = ["C:\\game\\DungeonSiege.exe", "width=2560", "height=1440", "bpp=32"];
        assert_eq!(find_resolution(args.into_iter()), Some((2560, 1440)));
        let ini = "driver_description = Primary\nwidth = 1920\nheight = 1080\n[multiplayer]\ngun_server = x";
        assert_eq!(find_resolution(ini.lines()), Some((1920, 1080)));
        assert_eq!(find_resolution(["nointro=true"].into_iter()), None);
    }

    #[test]
    fn enables_flip() {
        let gas = "\t[vendor*]\n\t{\n\t\tvendor\t= \"nVidia\";\n\t\tb no_flip\t\t\t\t\t= true;\n\t\tb buffers_frames\t= true;\n\t}\n";
        let out = super::enable_flip(gas);
        assert_eq!(out, gas.replace("no_flip\t\t\t\t\t= true", "no_flip\t\t\t\t\t= false"));
        assert_eq!(super::enable_flip(&out), out);
    }

    #[test]
    fn adds_resolution_to_top_tier_once() {
        let gas = "[system_detail]\n{\n\t[32M16B]\n\t{\n\t\t[resolutions]\n\t\t{\n\t\t\t[800x600]\t{ max_back_buffers = 2; }\n\t\t}\n\t}\n\t[64M32B]\n\t{\n\t\tdetail\t\t\t= 4;\n\t\t[resolutions]\n\t\t{\n\t\t\t[1024x768]\t{ max_back_buffers = 2; }\n\t\t}\n\t}\n\t[64M16B]\n\t{\n\t}\n}\n";
        let out = add_resolution(gas, 2560, 1440).unwrap();
        let tier = &out[out.find("[64M32B]").unwrap()..out.find("[64M16B]").unwrap()];
        assert!(tier.contains("\t\t{\n\t\t\t[2560x1440]\t{ max_back_buffers = 2; }\n\t\t\t[1024x768]"));
        assert_eq!(out.matches("2560x1440").count(), 1);
        assert_eq!(add_resolution(&out, 2560, 1440), None);
        assert_eq!(add_resolution(gas, 1024, 768), None);
    }
}
