//! Local mod library.
//!
//! Mods live in the launcher's data folder, never in the game folder:
//! - `mods\`      enabled `.dsmod` archives (passed to the game as `mod_paths`)
//! - `maps\`      enabled `.dsmap` archives (passed as `map_paths`)
//! - `code-mods\` enabled code mods (`.dll`, loaded by desiege.dll; see desiege_mod.h)
//! - `disabled\`  everything switched off; outside the folders the game scans, because it
//!                also looks into subfolders.
//! Code mods run with the user's rights inside the game, so they always install disabled and
//! can only be enabled with an explicit `trusted` confirmation from the UI.
// ponytail: no load order yet; the game orders tanks by the priority stored in their headers.
// `.dsres`-style mods (the community convention is dropping them into the game's Resources
// folder) aren't supported until we've tested how to load them from here.

use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::Manager;

#[derive(Serialize, Debug, PartialEq)]
pub struct ModFile {
    pub file: String,
    /// "mod", "map" or "code"
    pub kind: &'static str,
    pub enabled: bool,
    pub size: u64,
}

#[derive(Serialize, Debug, Default)]
pub struct InstallReport {
    pub installed: Vec<String>,
    /// (file, reason)
    pub skipped: Vec<(String, String)>,
    /// Code mods that were installed disabled and need the user's trust to enable.
    pub needs_trust: Vec<String>,
}

pub struct Dirs {
    pub mods: PathBuf,
    pub maps: PathBuf,
    pub code_mods: PathBuf,
    pub disabled: PathBuf,
}

pub fn dirs(app: &tauri::AppHandle) -> Result<Dirs, String> {
    let data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let d = Dirs { mods: data.join("mods"), maps: data.join("maps"), code_mods: data.join("code-mods"), disabled: data.join("disabled") };
    for p in [&d.mods, &d.maps, &d.code_mods, &d.disabled] {
        fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    Ok(d)
}

fn kind_of(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".dsmod") {
        Some("mod")
    } else if lower.ends_with(".dsmap") {
        Some("map")
    } else if lower.ends_with(".dll") {
        Some("code")
    } else {
        None
    }
}

/// Why a file can't be installed (None = it's a mod/map archive we install).
fn unsupported(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "dsmod" | "dsmap" | "dll" => None,
        "dsres" => Some(".dsres mods aren't supported yet (only .dsmod and .dsmap)"),
        "dsdll" | "exe" => Some("only .dll code mods built for Desiege can be installed as code mods"),
        _ => Some("not a Dungeon Siege mod or map file"),
    }
}

/// A file name the page sent us: a plain name (no folders) of a mod or map.
fn checked_name(file: &str) -> Result<(&str, &'static str), String> {
    let plain = Path::new(file).file_name().and_then(|n| n.to_str()) == Some(file);
    match kind_of(file) {
        Some(kind) if plain => Ok((file, kind)),
        _ => Err(format!("not a mod file name: {file}")),
    }
}

fn enabled_dir<'a>(d: &'a Dirs, kind: &str) -> &'a Path {
    match kind {
        "map" => &d.maps,
        "code" => &d.code_mods,
        _ => &d.mods,
    }
}

fn scan(dir: &Path, enabled: bool, out: &mut Vec<ModFile>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let (Some(kind), Ok(meta)) = (kind_of(&name), e.metadata()) {
            if meta.is_file() {
                out.push(ModFile { file: name, kind, enabled, size: meta.len() });
            }
        }
    }
}

pub fn list(d: &Dirs) -> Vec<ModFile> {
    let mut out = Vec::new();
    scan(&d.mods, true, &mut out);
    scan(&d.maps, true, &mut out);
    scan(&d.code_mods, true, &mut out);
    scan(&d.disabled, false, &mut out);
    out.sort_by_key(|m| m.file.to_ascii_lowercase());
    out
}

pub fn set_enabled(d: &Dirs, file: &str, enabled: bool, trusted: bool) -> Result<(), String> {
    let (file, kind) = checked_name(file)?;
    if kind == "code" && enabled && !trusted {
        return Err(format!("{file} contains code; enabling it needs your explicit trust"));
    }
    let (from, to) = if enabled { (d.disabled.join(file), enabled_dir(d, kind).join(file)) } else { (enabled_dir(d, kind).join(file), d.disabled.join(file)) };
    if !from.is_file() {
        return Ok(()); // already there
    }
    fs::rename(&from, &to).map_err(|e| format!("Could not move {file}: {e}"))
}

pub fn remove(d: &Dirs, file: &str) -> Result<(), String> {
    let (file, kind) = checked_name(file)?;
    for p in [enabled_dir(d, kind).join(file), d.disabled.join(file)] {
        if p.is_file() {
            fs::remove_file(&p).map_err(|e| format!("Could not remove {file}: {e}"))?;
        }
    }
    Ok(())
}

/// Install one archive file (enabled). Replaces an installed copy with the same name.
fn install_file(d: &Dirs, src: &Path, report: &mut InstallReport) {
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if let Some(why) = unsupported(&name) {
        report.skipped.push((name, why.into()));
        return;
    }
    let kind = kind_of(&name).unwrap();
    // Code mods never start enabled; an enabled copy of the same name gets switched off too.
    let dest = if kind == "code" {
        let _ = fs::remove_file(d.code_mods.join(&name));
        d.disabled.join(&name)
    } else {
        let _ = fs::remove_file(d.disabled.join(&name));
        enabled_dir(d, kind).join(&name)
    };
    match fs::copy(src, dest) {
        Ok(_) if kind == "code" => report.needs_trust.push(name),
        Ok(_) => report.installed.push(name),
        Err(e) => report.skipped.push((name, format!("copy failed: {e}"))),
    }
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, files);
        } else {
            files.push(p);
        }
    }
}

/// Install dropped files: `.dsmod` / `.dsmap` directly, `.zip` archives by extracting them
/// (Windows' own tar.exe) and installing the mod/map files inside.
pub fn install(d: &Dirs, scratch: &Path, paths: &[String]) -> InstallReport {
    let mut report = InstallReport::default();
    for p in paths.iter().map(PathBuf::from) {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if p.is_dir() {
            report.skipped.push((name, "folders aren't supported; drop the files or a .zip".into()));
        } else if name.to_ascii_lowercase().ends_with(".zip") {
            let tmp = scratch.join("unzip");
            let _ = fs::remove_dir_all(&tmp);
            if let Err(e) = fs::create_dir_all(&tmp) {
                report.skipped.push((name, e.to_string()));
                continue;
            }
            if let Err(e) = crate::system_tool("tar.exe", &["-xf".as_ref(), p.as_os_str(), "-C".as_ref(), tmp.as_os_str()]) {
                report.skipped.push((name, format!("could not extract: {e}")));
                continue;
            }
            let mut files = Vec::new();
            walk(&tmp, &mut files);
            let mut relevant = 0;
            for f in &files {
                let n = f.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                // Inside archives, readmes and screenshots are expected; only report what matters.
                let ext = n.rsplit('.').next().unwrap_or("");
                if matches!(ext, "dsmod" | "dsmap" | "dsres" | "dll" | "dsdll" | "exe") {
                    relevant += 1;
                    install_file(d, f, &mut report);
                }
            }
            if relevant == 0 {
                report.skipped.push((name, "no .dsmod or .dsmap files inside".into()));
            }
            let _ = fs::remove_dir_all(&tmp);
        } else {
            install_file(d, &p, &mut report);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dirs(tag: &str) -> (PathBuf, Dirs) {
        let root = std::env::temp_dir().join(format!("desiege-mods-test-{tag}"));
        let _ = fs::remove_dir_all(&root);
        let d = Dirs { mods: root.join("mods"), maps: root.join("maps"), code_mods: root.join("code-mods"), disabled: root.join("disabled") };
        for p in [&d.mods, &d.maps, &d.code_mods, &d.disabled] {
            fs::create_dir_all(p).unwrap();
        }
        (root, d)
    }

    #[test]
    fn installs_toggles_and_removes() {
        let (root, d) = temp_dirs("lifecycle");
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        for n in ["Cool.dsmod", "World.dsmap", "Old.dsres", "hack.dll", "tool.exe", "readme.txt"] {
            fs::write(src.join(n), b"x").unwrap();
        }
        let paths: Vec<String> = fs::read_dir(&src).unwrap().map(|e| e.unwrap().path().display().to_string()).collect();
        let r = install(&d, &root, &paths);
        let mut installed = r.installed.clone();
        installed.sort();
        assert_eq!(installed, ["Cool.dsmod", "World.dsmap"]);
        assert_eq!(r.skipped.len(), 3); // .dsres, .exe, .txt
        assert!(d.mods.join("Cool.dsmod").is_file() && d.maps.join("World.dsmap").is_file());

        // Code: installed disabled, enabling needs trust.
        assert_eq!(r.needs_trust, ["hack.dll"]);
        assert!(d.disabled.join("hack.dll").is_file() && !d.code_mods.join("hack.dll").exists());
        assert!(set_enabled(&d, "hack.dll", true, false).is_err());
        set_enabled(&d, "hack.dll", true, true).unwrap();
        assert!(d.code_mods.join("hack.dll").is_file());
        // Reinstalling an enabled code mod switches it back off.
        install(&d, &root, &[src.join("hack.dll").display().to_string()]);
        assert!(d.disabled.join("hack.dll").is_file() && !d.code_mods.join("hack.dll").exists());

        set_enabled(&d, "Cool.dsmod", false, false).unwrap();
        assert!(d.disabled.join("Cool.dsmod").is_file() && !d.mods.join("Cool.dsmod").exists());
        assert_eq!(list(&d).iter().filter(|m| !m.enabled).count(), 2); // Cool + hack
        set_enabled(&d, "Cool.dsmod", true, false).unwrap();
        assert!(d.mods.join("Cool.dsmod").is_file());

        remove(&d, "World.dsmap").unwrap();
        assert_eq!(list(&d).len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn installs_from_zip() {
        let (root, d) = temp_dirs("zip");
        let src = root.join("pack").join("Data");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Big.dsmod"), b"x").unwrap();
        fs::write(src.join("readme.txt"), b"x").unwrap();
        let zip = root.join("pack.zip");
        crate::system_tool("tar.exe", &["-a".as_ref(), "-cf".as_ref(), zip.as_os_str(), "-C".as_ref(), root.join("pack").as_os_str(), "Data".as_ref()]).unwrap();
        let r = install(&d, &root, &[zip.display().to_string()]);
        assert_eq!(r.installed, ["Big.dsmod"]);
        assert!(r.skipped.is_empty(), "{:?}", r.skipped); // readme inside an archive is fine
        assert!(d.mods.join("Big.dsmod").is_file());

        let empty = root.join("empty.zip");
        crate::system_tool("tar.exe", &["-a".as_ref(), "-cf".as_ref(), empty.as_os_str(), "-C".as_ref(), src.as_os_str(), "readme.txt".as_ref()]).unwrap();
        let r = install(&d, &root, &[empty.display().to_string()]);
        assert_eq!(r.skipped.len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_paths_from_the_page() {
        let (root, d) = temp_dirs("names");
        assert!(set_enabled(&d, r"..\..\settings.json", true, true).is_err());
        assert!(remove(&d, r"C:\Windows\x.dsmod").is_err());
        assert!(remove(&d, "notamod.txt").is_err());
        let _ = fs::remove_dir_all(root);
    }
}
