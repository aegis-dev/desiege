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
