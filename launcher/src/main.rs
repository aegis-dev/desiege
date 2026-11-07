#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};
use tauri::Manager;

mod mods;

const EXE: &str = "DungeonSiege.exe";

/// dxwrapper (https://github.com/elishacloud/dxwrapper, zlib license) translates the game's
/// Direct3D 7 to Direct3D 9. Windows' own D3D7 can't render wider than 2048 px.
/// Pinned version + SHA-256 of the release zip; bump all three together.
const DXWRAPPER_VERSION: &str = "1.9.8700.25";
const DXWRAPPER_URL: &str = "https://github.com/elishacloud/dxwrapper/releases/download/v1.9.8700.25/dxwrapper.zip";
const DXWRAPPER_SHA256: &str = "e6aa4fc685115c8f73dee4ea2d446b6a1f8b57dd04db9889d2c6d1b6d7b8c825";
const DXWRAPPER_LINKS: [&str; 3] = [
    "https://github.com/elishacloud/dxwrapper",
    "https://github.com/elishacloud/dxwrapper/releases/tag/v1.9.8700.25",
    "https://github.com/elishacloud/dxwrapper/blob/master/License.txt",
];
/// dxwrapper.ini, rewritten on every launch (the display mode can change between launches).
fn dxwrapper_ini(borderless: bool) -> String {
    format!(
        "; Written by the Desiege launcher on every launch; edits are overwritten.
[General]
; The game decompresses its archives on demand by catching access violations (thousands while
; loading); dxwrapper would write a log line for every one of them.
DisableLogging = 1

[Compatibility]
; Translate Direct3D 7 to Direct3D 9
Dd7to9 = 1

[ddraw]
; Hook the System32 ddraw.dll, not the Desiege proxy that sits in the game folder as ddraw.dll
DdrawHookSystem32 = 1

[d3d9]
; 1 = borderless window instead of exclusive fullscreen
FullscreenWindowMode = {}
",
        borderless as u8
    )
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Settings {
    game_dir: String,
    width: u32,
    height: u32,
    fullscreen: bool,
    /// User said "not now" to downloading dxwrapper; don't ask again on start.
    #[serde(default)]
    dxwrapper_declined: bool,
    #[serde(default)]
    method: Method,
    /// With `fullscreen`: a borderless window instead of exclusive fullscreen (needs dxwrapper).
    #[serde(default)]
    borderless: bool,
    /// 1 = off. See game_resolution().
    #[serde(default = "one")]
    ui_scale: f32,
    /// Shadow texture size for desiege.dll's GPU shadow redirect; 0 = the game's own (slow) path.
    #[serde(default = "default_shadow_res")]
    shadow_res: u32,
}
fn default_shadow_res() -> u32 {
    128
}
const SHADOW_RES_CHOICES: [u32; 5] = [0, 64, 128, 256, 512];
fn one() -> f32 {
    1.0
}

/// How desiege.dll gets into the game.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
enum Method {
    /// desiege-loader injects it; the game folder stays untouched.
    #[default]
    Inject,
    /// A small ddraw.dll stub in the game folder loads it; also works when started from Steam.
    GameFolder,
}

/// Physical resolution of the primary display. Display modes are reported in real
/// pixels regardless of DPI scaling (e.g. 2560x1440 at 125% stays 2560x1440).
fn native_resolution() -> (u32, u32) {
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    unsafe {
        let mut dm: DEVMODEW = std::mem::zeroed();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm) != 0 {
            (dm.dmPelsWidth, dm.dmPelsHeight)
        } else {
            (1024, 768)
        }
    }
}

/// Library roots listed in Steam's libraryfolders.vdf (`"path"  "D:\\SteamLibrary"`).
fn steam_libraries(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|l| l.trim().strip_prefix("\"path\""))
        .map(|rest| PathBuf::from(rest.trim().trim_matches('"').replace("\\\\", "\\")))
        .collect()
}

// ponytail: assumes Steam lives in the default Program Files (x86) folder; read
// HKCU\Software\Valve\Steam\SteamPath if users report custom installs. GOG detection comes later.
fn detect_game_dir() -> Option<PathBuf> {
    let steam = PathBuf::from(std::env::var("ProgramFiles(x86)").ok()?).join("Steam");
    let vdf = fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    std::iter::once(steam)
        .chain(steam_libraries(&vdf))
        .map(|lib| lib.join(r"steamapps\common\Dungeon Siege 1"))
        .find(|dir| dir.join(EXE).is_file())
}

/// Command-line switches override DungeonSiege.ini, so whatever the in-game
/// options menu writes there can't knock the resolution back down.
fn launch_args(s: &Settings) -> Vec<String> {
    let (w, h) = game_resolution(s);
    vec![format!("width={w}"), format!("height={h}"), "bpp=32".into(), format!("fullscreen={}", s.fullscreen)]
}

/// UI scale > 1 runs the game at display / scale; desiege.dll renders it back at full size,
/// so the UI comes out `scale` times larger while 3D stays sharp. Fullscreen only.
fn game_resolution(s: &Settings) -> (u32, u32) {
    if s.ui_scale > 1.0 && s.fullscreen {
        ((s.width as f32 / s.ui_scale).round() as u32, (s.height as f32 / s.ui_scale).round() as u32)
    } else {
        (s.width, s.height)
    }
}

fn settings_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_config_dir().map_err(|e| e.to_string())?.join("settings.json"))
}

/// Third-party components (and later, mods) live under the launcher's data dir,
/// never in the game folder.
fn dxwrapper_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data.join("deps").join("dxwrapper").join(DXWRAPPER_VERSION))
}

/// Run a tool from System32 by full path (no PATH lookup), hidden, returning stdout.
pub(crate) fn system_tool(name: &str, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let out = Command::new(Path::new(&root).join("System32").join(name))
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("{name}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{name} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// certutil prints "SHA256 hash of <file>:", then the hex digest (older versions space it).
fn parse_certutil_sha256(out: &str) -> Option<String> {
    let hex: String = out.lines().nth(1)?.chars().filter(|c| !c.is_whitespace()).collect();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
}

#[tauri::command]
fn dxwrapper_installed(app: tauri::AppHandle) -> bool {
    dxwrapper_dir(&app).is_ok_and(|d| d.join("dxwrapper.dll").is_file())
}

#[tauri::command]
fn dxwrapper_links() -> [&'static str; 3] {
    DXWRAPPER_LINKS
}

const REPO_URL: &str = "https://github.com/aegis-dev/desiege";
const PROJECT_LINKS: [&str; 3] = [
    REPO_URL,
    "https://github.com/aegis-dev/desiege/releases",
    "https://github.com/aegis-dev/desiege/issues",
];

#[derive(Serialize)]
struct AppInfo {
    version: &'static str,
    links: [&'static str; 3],
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo { version: env!("CARGO_PKG_VERSION"), links: PROJECT_LINKS }
}

/// Only fixed links can be opened (the page can't ask for arbitrary URLs).
#[tauri::command]
fn open_link(url: String) -> Result<(), String> {
    if !DXWRAPPER_LINKS.contains(&url.as_str()) && !PROJECT_LINKS.contains(&url.as_str()) {
        return Err("link not allowed".into());
    }
    Command::new("explorer.exe").arg(&url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Download, verify and unpack dxwrapper. Only called after the user agreed in the UI.
#[tauri::command]
async fn install_dxwrapper(app: tauri::AppHandle) -> Result<(), String> {
    let dir = dxwrapper_dir(&app)?;
    let tmp = dir.with_extension("partial");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let zip = tmp.join("dxwrapper.zip");

    system_tool("curl.exe", &["-fsSL".as_ref(), "--proto".as_ref(), "=https".as_ref(), "-o".as_ref(), zip.as_os_str(), DXWRAPPER_URL.as_ref()])?;
    let hash = parse_certutil_sha256(&system_tool("certutil.exe", &["-hashfile".as_ref(), zip.as_os_str(), "SHA256".as_ref()])?);
    if hash.as_deref() != Some(DXWRAPPER_SHA256) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("Download checksum mismatch (got {hash:?}); nothing was installed."));
    }
    system_tool("tar.exe", &["-xf".as_ref(), zip.as_os_str(), "-C".as_ref(), tmp.as_os_str(), "dxwrapper.dll".as_ref(), "License.txt".as_ref()])?;
    fs::write(tmp.join("dxwrapper.ini"), dxwrapper_ini(false)).map_err(|e| e.to_string())?;
    fs::remove_file(&zip).map_err(|e| e.to_string())?;

    let _ = fs::remove_dir_all(&dir);
    fs::rename(&tmp, &dir).map_err(|e| e.to_string())
}

/// A file shipped next to the launcher exe by build.ps1.
fn bundled(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let path = exe.parent().unwrap().join(name);
    path.is_file().then_some(path).ok_or_else(|| format!("{name} missing next to the launcher (run build.ps1)"))
}

/// %APPDATA%\dev.desiege.launcher\runtime: the copy of desiege.dll + loader that both launch
/// methods use (the game-folder stub can only find it at a fixed path), plus `dxwrapper.path`
/// so a Steam launch, which has no launcher to set DESIEGE_DXWRAPPER, still finds dxwrapper.
fn ensure_runtime(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("runtime");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for name in ["desiege.dll", "desiege-loader.exe"] {
        let (src, dst) = (bundled(name)?, dir.join(name));
        if fs::read(&src).ok() != fs::read(&dst).ok() {
            fs::copy(&src, &dst).map_err(|e| format!("Updating {name} failed (is the game still running?): {e}"))?;
        }
    }
    let dxwrapper = dxwrapper_dir(app)?.join("dxwrapper.dll");
    let marker = dir.join("dxwrapper.path");
    if dxwrapper.is_file() {
        fs::write(&marker, dxwrapper.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    } else {
        let _ = fs::remove_file(&marker);
    }
    Ok(dir)
}

#[derive(PartialEq, Debug)]
enum StubState { Absent, Ours, Foreign }

/// Our stub embeds this marker (crates/ddraw-stub); anything else named ddraw.dll isn't ours to touch.
fn stub_state(path: &Path) -> StubState {
    match fs::read(path) {
        Err(_) => StubState::Absent,
        Ok(bytes) if bytes.windows(STUB_MARKER.len()).any(|w| w == STUB_MARKER) => StubState::Ours,
        Ok(_) => StubState::Foreign,
    }
}
const STUB_MARKER: &[u8] = b"DESIEGE-STUB";

#[tauri::command]
fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let path = settings_path(&app)?;
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_native_resolution() -> (u32, u32) {
    native_resolution()
}

#[tauri::command]
fn load_settings(app: tauri::AppHandle) -> Settings {
    let saved = settings_path(&app)
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    saved.unwrap_or_else(|| {
        let (width, height) = native_resolution();
        Settings {
            game_dir: detect_game_dir().map(|d| d.display().to_string()).unwrap_or_default(),
            width,
            height,
            fullscreen: true,
            dxwrapper_declined: false,
            method: Method::Inject,
            borderless: false,
            ui_scale: 1.0,
            shadow_res: default_shadow_res(),
        }
    })
}

#[tauri::command]
fn launch(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let dir = PathBuf::from(&settings.game_dir);
    if !dir.join(EXE).is_file() {
        return Err(format!("{EXE} not found in \"{}\"", settings.game_dir));
    }
    if !(320..=16384).contains(&settings.width) || !(240..=16384).contains(&settings.height) {
        return Err("Resolution out of range".into());
    }

    save_settings(app.clone(), settings.clone())?;
    if !SHADOW_RES_CHOICES.contains(&settings.shadow_res) {
        return Err("Shadow quality out of range".into());
    }
    if !(1.0..=4.0).contains(&settings.ui_scale) {
        return Err("UI scale out of range".into());
    }
    let runtime = ensure_runtime(&app)?;
    // desiege.dll reads this (Steam launches have no launcher to pass DESIEGE_UI_SCALE).
    // Read by desiege.dll at startup (works for Steam launches too).
    fs::write(runtime.join("shadow_res"), settings.shadow_res.to_string()).map_err(|e| e.to_string())?;
    // Display mode: dxwrapper does the borderless window; desiege.dll keeps the monitor at its
    // native mode (renders the game's smaller modes, like the 800x600 menus, centered at native).
    let borderless = settings.fullscreen && settings.borderless;
    let dxwrapper_dir = dxwrapper_dir(&app)?;
    if dxwrapper_dir.join("dxwrapper.dll").is_file() {
        fs::write(dxwrapper_dir.join("dxwrapper.ini"), dxwrapper_ini(borderless)).map_err(|e| e.to_string())?;
    } else if borderless {
        return Err("Borderless window needs the dxwrapper renderer fix; install it or pick another display mode.".into());
    }
    let marker = runtime.join("borderless");
    if borderless {
        fs::write(&marker, b"").map_err(|e| e.to_string())?;
    } else {
        let _ = fs::remove_file(&marker);
    }
    let scale_file = runtime.join("ui_scale");
    if settings.ui_scale > 1.0 && settings.fullscreen {
        fs::write(&scale_file, settings.ui_scale.to_string()).map_err(|e| e.to_string())?;
    } else {
        let _ = fs::remove_file(&scale_file);
    }

    let game_stub = dir.join("ddraw.dll");
    let stub_state = stub_state(&game_stub);
    let mut cmd = match settings.method {
        Method::GameFolder => {
            if stub_state == StubState::Foreign {
                return Err("The game folder already has a ddraw.dll from something else (another mod?). Remove it, or use the Inject method.".into());
            }
            // (Re)copy every time so an updated launcher also updates the stub.
            fs::copy(bundled("ddraw-stub.dll")?, &game_stub)
                .map_err(|e| format!("Could not put ddraw.dll in the game folder: {e}"))?;
            Command::new(dir.join(EXE))
        }
        Method::Inject => {
            match stub_state {
                // Leaving it would load desiege.dll twice.
                StubState::Ours => fs::remove_file(&game_stub).map_err(|e| format!("Could not remove our ddraw.dll from the game folder: {e}"))?,
                StubState::Foreign => return Err("The game folder has a ddraw.dll from something else (another mod?), which would bypass Desiege. Remove it to use Inject.".into()),
                StubState::Absent => {}
            }
            let mut c = Command::new(runtime.join("desiege-loader.exe"));
            c.arg(runtime.join("desiege.dll")).arg(dir.join(EXE)).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            c
        }
    };
    // Mods are .dsmod tanks; the engine looks for them in `mod_paths` (default: <game>\Mods).
    // ponytail: a profile path with spaces gets quoted on the command line; untested whether
    // the game's own parser strips the quotes.
    let lib = mods::dirs(&app)?;
    cmd.current_dir(&dir)
        .args(launch_args(&settings))
        .arg(format!("mod_paths={}", lib.mods.display()))
        .arg(format!("map_paths={}", lib.maps.display()))
        // Game is DPI-unaware; without this Windows virtualizes it at 125%+ scaling
        // and the fullscreen image comes out blurry or cropped.
        .env("__COMPAT_LAYER", "HighDpiAware");
    if settings.method == Method::GameFolder {
        cmd.spawn().map_err(|e| format!("Failed to start game: {e}"))?;
        return Ok(());
    }
    // The loader exits as soon as the game is running (the game doesn't inherit its pipes).
    let out = cmd.output().map_err(|e| format!("Failed to start loader: {e}"))?;
    if !out.status.success() {
        return Err(format!("Failed to start game: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

#[tauri::command]
fn list_mods(app: tauri::AppHandle) -> Result<Vec<mods::ModFile>, String> {
    Ok(mods::list(&mods::dirs(&app)?))
}

#[tauri::command]
/// `trusted`: the user confirmed enabling a code mod in the UI.
fn set_mod_enabled(app: tauri::AppHandle, file: String, enabled: bool, trusted: Option<bool>) -> Result<(), String> {
    mods::set_enabled(&mods::dirs(&app)?, &file, enabled, trusted.unwrap_or(false))
}

#[tauri::command]
fn remove_mod(app: tauri::AppHandle, file: String) -> Result<(), String> {
    mods::remove(&mods::dirs(&app)?, &file)
}

/// Paths come from files dropped on the launcher window.
#[tauri::command]
async fn install_mods(app: tauri::AppHandle, paths: Vec<String>) -> Result<mods::InstallReport, String> {
    let scratch = app.path().app_local_data_dir().map_err(|e| e.to_string())?.join("tmp");
    Ok(mods::install(&mods::dirs(&app)?, &scratch, &paths))
}

#[tauri::command]
fn open_mods_folder(app: tauri::AppHandle) -> Result<(), String> {
    let d = mods::dirs(&app)?;
    Command::new("explorer.exe").arg(d.mods.parent().unwrap()).spawn().map(|_| ()).map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_native_resolution,
            load_settings,
            save_settings,
            launch,
            dxwrapper_installed,
            dxwrapper_links,
            install_dxwrapper,
            open_link,
            app_info,
            list_mods,
            set_mod_enabled,
            remove_mod,
            install_mods,
            open_mods_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running launcher");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steam_libraries() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}";
        assert_eq!(
            steam_libraries(vdf),
            vec![PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"D:\SteamLibrary")]
        );
    }

    #[test]
    fn builds_launch_args() {
        let mut s = Settings { game_dir: String::new(), width: 2560, height: 1440, fullscreen: true, dxwrapper_declined: false, method: Method::Inject, borderless: false, ui_scale: 1.0, shadow_res: 128 };
        assert_eq!(launch_args(&s), ["width=2560", "height=1440", "bpp=32", "fullscreen=true"]);
        s.ui_scale = 2.0;
        assert_eq!(launch_args(&s)[..2], ["width=1280", "height=720"]);
        s.ui_scale = 1.5;
        assert_eq!(launch_args(&s)[..2], ["width=1707", "height=960"]);
        s.fullscreen = false; // windowed: no scaling
        assert_eq!(launch_args(&s)[..2], ["width=2560", "height=1440"]);
    }

    #[test]
    fn tells_our_stub_from_foreign_ddraw() {
        let dir = std::env::temp_dir().join("desiege-stub-test");
        fs::create_dir_all(&dir).unwrap();
        let (ours, foreign) = (dir.join("ours.dll"), dir.join("foreign.dll"));
        fs::write(&ours, b"MZ...DESIEGE-STUB\0...").unwrap();
        fs::write(&foreign, b"MZ...dgVoodoo...").unwrap();
        assert_eq!(stub_state(&ours), StubState::Ours);
        assert_eq!(stub_state(&foreign), StubState::Foreign);
        assert_eq!(stub_state(&dir.join("missing.dll")), StubState::Absent);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn method_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Method::GameFolder).unwrap(), "\"game_folder\"");
        let old: Settings = serde_json::from_str(r#"{"game_dir":"","width":1,"height":1,"fullscreen":true}"#).unwrap();
        assert_eq!(old.method, Method::Inject); // settings saved before the option existed
        assert_eq!(old.shadow_res, 128);
        assert!(!old.borderless);
    }

    #[test]
    fn dxwrapper_ini_switches_borderless() {
        assert!(dxwrapper_ini(true).contains("FullscreenWindowMode = 1"));
        assert!(dxwrapper_ini(false).contains("FullscreenWindowMode = 0"));
        assert!(dxwrapper_ini(false).contains("DdrawHookSystem32 = 1"));
        assert!(dxwrapper_ini(false).contains("DisableLogging = 1"));
    }

    #[test]
    fn parses_certutil_output() {
        let modern = "SHA256 hash of C:\\x\\dxwrapper.zip:\r\ne6aa4fc685115c8f73dee4ea2d446b6a1f8b57dd04db9889d2c6d1b6d7b8c825\r\nCertUtil: -hashfile command completed successfully.\r\n";
        assert_eq!(parse_certutil_sha256(modern).as_deref(), Some(DXWRAPPER_SHA256));
        let spaced = "SHA256 hash of file x:\r\ne6 aa 4f c6 85 11 5c 8f 73 de e4 ea 2d 44 6b 6a 1f 8b 57 dd 04 db 98 89 d2 c6 d1 b6 d7 b8 c8 25\r\n";
        assert_eq!(parse_certutil_sha256(spaced).as_deref(), Some(DXWRAPPER_SHA256));
        assert_eq!(parse_certutil_sha256("CertUtil: error\r\n"), None);
    }

    #[test]
    fn native_resolution_is_sane() {
        let (w, h) = native_resolution();
        println!("native resolution: {w}x{h}");
        assert!(w >= 640 && h >= 480);
    }
}
