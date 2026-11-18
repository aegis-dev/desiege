//! Starts Dungeon Siege suspended, loads the Desiege DLL into it, then lets it run.
//!   desiege-loader.exe <path\to\ddraw.dll> <path\to\DungeonSiege.exe> [game args...]
//! Working directory and environment are inherited from the caller (the launcher).
//!
//! Must be 32-bit like the game: the remote thread starts at *this* process's LoadLibraryW,
//! which has the same address in every 32-bit process (system DLLs share a base per boot).
//! That's also why the 64-bit launcher can't do this itself.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, process::ExitCode, ptr::null};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::{
        Diagnostics::Debug::WriteProcessMemory,
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Memory::{VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE},
        Threading::{
            CreateProcessW, CreateRemoteThread, GetExitCodeThread, ResumeThread, TerminateProcess,
            WaitForSingleObject, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
        },
    },
};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// Quote one argument for a Windows command line.
// ponytail: wraps in quotes when needed and escapes inner quotes; doesn't double backslashes
// that precede a quote, which our args (paths, key=value) never contain.
fn quote(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if !s.is_empty() && !s.contains([' ', '\t', '"']) {
        return s.into_owned();
    }
    format!("\"{}\"", s.replace('"', "\\\""))
}

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [dll, exe, game_args @ ..] = args.as_slice() else {
        eprintln!("usage: desiege-loader <dll> <game exe> [game args...]");
        return ExitCode::from(2);
    };
    match unsafe { run(dll, exe, game_args) } {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

unsafe fn run(dll: &OsStr, exe: &OsStr, game_args: &[std::ffi::OsString]) -> Result<(), String> {
    let last_error = |what: &str| format!("{what} failed: {}", std::io::Error::last_os_error());

    // A foreign ddraw.dll next to the exe (GOG ships its own D3D7->D3D9 wrapper there) would be
    // loaded as the game's ddraw before we get in. Its DllMain hooks the game's mouse input,
    // which fights our own mapping, so rerouting its exports isn't enough: it must not load.
    // The child gets our folder on PATH and, while still suspended, its ddraw import renamed
    // to our DLL's name (see redirect_ddraw_import), so Windows loads us as its ddraw.dll.
    let exe_path = std::path::Path::new(exe);
    let dll_path = std::path::Path::new(dll);
    let vanilla = std::env::var("DESIEGE_EXPERIMENT").is_ok_and(|e| e.split(',').any(|e| e.trim() == "vanilla"));
    let redirect = !vanilla && exe_path.parent().is_some_and(|d| d.join("ddraw.dll").is_file());
    if redirect {
        let dir = dll_path.parent().ok_or("DLL path has no folder")?;
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::set_var("PATH", std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&path))).map_err(|e| e.to_string())?);
    }

    let mut cmdline = wide(OsStr::new(
        &std::iter::once(exe).chain(game_args.iter().map(|a| a.as_os_str())).map(quote).collect::<Vec<_>>().join(" "),
    ));
    let mut si: STARTUPINFOW = std::mem::zeroed();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
    if CreateProcessW(wide(exe).as_ptr(), cmdline.as_mut_ptr(), null(), null(), 0, CREATE_SUSPENDED, null(), null(), &si, &mut pi) == 0 {
        return Err(last_error("Starting the game"));
    }

    let result = (|| {
        if redirect {
            let stem = dll_path.file_stem().and_then(|s| s.to_str()).ok_or("DLL name is not valid")?;
            redirect_ddraw_import(pi.hProcess, exe_path, stem)?;
        }
        let path = wide(dll);
        let bytes = path.len() * 2;
        let mem = VirtualAllocEx(pi.hProcess, null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if mem.is_null() || WriteProcessMemory(pi.hProcess, mem, path.as_ptr().cast(), bytes, std::ptr::null_mut()) == 0 {
            return Err(last_error("Writing the DLL path"));
        }
        let k32 = GetModuleHandleW(wide(OsStr::new("kernel32.dll")).as_ptr());
        let load_library = GetProcAddress(k32, c"LoadLibraryW".as_ptr().cast()).ok_or("LoadLibraryW not found")?;
        let thread = CreateRemoteThread(pi.hProcess, null(), 0, Some(std::mem::transmute(load_library)), mem, 0, std::ptr::null_mut());
        if thread.is_null() {
            return Err(last_error("Starting the loader thread"));
        }
        WaitForSingleObject(thread, 30_000);
        let mut module = 0u32; // LoadLibraryW's return value: our HMODULE, or 0 on failure
        GetExitCodeThread(thread, &mut module);
        CloseHandle(thread);
        VirtualFreeEx(pi.hProcess, mem, 0, MEM_RELEASE);
        if module == 0 {
            return Err(format!("The game could not load {}", dll.to_string_lossy()));
        }
        Ok(())
    })();

    match result {
        Ok(()) => { ResumeThread(pi.hThread); }
        Err(_) => { TerminateProcess(pi.hProcess, 1); }
    }
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    result
}

/// Rewrites the name of the exe's `ddraw.dll` import, in the suspended child's memory, to
/// `name` (our DLL's file stem; the loader adds ".dll" and finds it on PATH). Runs before the
/// process's first thread starts, i.e. before any import is loaded. The exe on disk is
/// untouched. Layout comes from the file on disk; Dungeon Siege has no ASLR (always at its
/// preferred base), which is checked against the "MZ" header in memory.
unsafe fn redirect_ddraw_import(process: windows_sys::Win32::Foundation::HANDLE, exe: &std::path::Path, name: &str) -> Result<(), String> {
    use windows_sys::Win32::System::{Diagnostics::Debug::ReadProcessMemory, Memory::VirtualProtectEx};
    let file = std::fs::read(exe).map_err(|e| format!("Reading {}: {e}", exe.display()))?;
    let (base, name_rva, old_len) = find_import_name(&file, "ddraw.dll").ok_or("Could not find the game's ddraw.dll import")?;
    if name.len() > old_len {
        return Err(format!("DLL name {name} is longer than {old_len} characters"));
    }
    let mut mz = [0u8; 2];
    if ReadProcessMemory(process, base as _, mz.as_mut_ptr().cast(), 2, std::ptr::null_mut()) == 0 || &mz != b"MZ" {
        return Err("The game is not loaded at its usual address".into());
    }
    let at = (base + name_rva) as *mut std::ffi::c_void;
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0);
    let mut old = 0;
    VirtualProtectEx(process, at, bytes.len(), PAGE_READWRITE, &mut old);
    let ok = WriteProcessMemory(process, at, bytes.as_ptr().cast(), bytes.len(), std::ptr::null_mut());
    VirtualProtectEx(process, at, bytes.len(), old, &mut old);
    if ok == 0 {
        return Err(format!("Redirecting the ddraw import failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

/// PE32: (ImageBase, RVA of the import's name string, its length) for the import named `dll`.
fn find_import_name(pe: &[u8], dll: &str) -> Option<(usize, usize, usize)> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(pe.get(o..o + 2)?.try_into().ok()?) as usize);
    let u32_at = |o: usize| Some(u32::from_le_bytes(pe.get(o..o + 4)?.try_into().ok()?) as usize);
    let nt = u32_at(0x3C)?;
    let opt = nt + 24;
    let base = u32_at(opt + 28)?;
    let sections = opt + u16_at(nt + 20)?;
    let file_offset = |rva: usize| {
        (0..u16_at(nt + 6)?).find_map(|i| {
            let s = sections + 40 * i;
            let (vsize, va, raw_size, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 16)?, u32_at(s + 20)?);
            (va..va + vsize.max(raw_size)).contains(&rva).then(|| rva - va + raw)
        })
    };
    let mut desc = file_offset(u32_at(opt + 96 + 8)?)?;
    loop {
        let name_rva = u32_at(desc + 12)?;
        if name_rva == 0 {
            return None;
        }
        let start = file_offset(name_rva)?;
        let len = pe.get(start..)?.iter().position(|&b| b == 0)?;
        if pe[start..start + len].eq_ignore_ascii_case(dll.as_bytes()) {
            return Some((base, name_rva, len));
        }
        desc += 20;
    }
}
