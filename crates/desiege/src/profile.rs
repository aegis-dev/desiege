//! Debug sampling profiler (DESIEGE_EXPERIMENT=profile).
//!
//! Once the game has rendered a few thousand frames, a background thread pauses the render
//! thread ~1000 times a second for 10 s, records where it was executing, and logs a histogram:
//! per module, and the hottest 64-byte blocks inside each of the busiest modules. Answers
//! "where does the frame time go" without an external profiler.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
};
use windows_sys::Win32::System::{
    Diagnostics::Debug::{GetThreadContext, CONTEXT},
    LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT},
    Threading::{GetCurrentThreadId, OpenThread, ResumeThread, Sleep, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME},
};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Call from the render thread every frame.
pub fn frame(frame: usize) {
    let start = std::env::var("DESIEGE_PROFILE_FRAME").ok().and_then(|v| v.parse().ok()).unwrap_or(4500);
    if frame < start || STARTED.swap(true, Relaxed) {
        return;
    }
    let tid = unsafe { GetCurrentThreadId() };
    std::thread::spawn(move || unsafe { run(tid) });
}

unsafe fn module_of(addr: usize) -> (usize, String) {
    let mut m = std::ptr::null_mut();
    if GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, addr as *const u16, &mut m) == 0 {
        return (0, "?".into());
    }
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(m, buf.as_mut_ptr(), 260) as usize;
    let name = String::from_utf16_lossy(&buf[..n]);
    (m as usize, name.rsplit('\\').next().unwrap_or("?").to_string())
}

/// [base, end) of a loaded module.
unsafe fn range(base: usize) -> (usize, usize) {
    if base == 0 {
        return (0, 0);
    }
    let pe = base + *((base + 0x3C) as *const u32) as usize;
    (base, base + *((pe + 0x50) as *const u32) as usize) // OptionalHeader.SizeOfImage
}
unsafe fn range_of(name: &str) -> (usize, usize) {
    let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    range(windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(w.as_ptr()) as usize)
}
unsafe fn range_of_exe() -> (usize, usize) {
    range(windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize)
}

unsafe fn run(tid: u32) {
    let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, tid);
    if h.is_null() {
        crate::log_line("profile: OpenThread failed".into());
        return;
    }
    crate::log_line(format!("profile: sampling render thread {tid} for 10 s"));
    let mut samples: HashMap<usize, u32> = HashMap::new();
    // For samples outside game/dxwrapper code: first return address into each on the stack.
    let (dxw, game) = (range_of("dxwrapper.dll"), range_of_exe());
    let mut via_dxw: HashMap<usize, u32> = HashMap::new();
    let mut via_game: HashMap<usize, u32> = HashMap::new();
    let mut total = 0u32;
    for _ in 0..10_000 {
        Sleep(1);
        if SuspendThread(h) == u32::MAX {
            continue;
        }
        let mut ctx: CONTEXT = std::mem::zeroed();
        ctx.ContextFlags = 0x0001_0001; // CONTEXT_i386 | CONTEXT_CONTROL
        let ok = GetThreadContext(h, &mut ctx) != 0;
        let (mut d, mut g) = (None, None);
        if ok {
            // Scan the top of the stack for return addresses (cheap stack walk, no unwinding).
            let sp = ctx.Esp as *const usize;
            for i in 0..512 {
                let v = *sp.add(i);
                if d.is_none() && dxw.0 <= v && v < dxw.1 { d = Some(v); }
                if g.is_none() && game.0 <= v && v < game.1 { g = Some(v); }
                if d.is_some() && g.is_some() { break; }
            }
        }
        ResumeThread(h);
        if ok {
            *samples.entry(ctx.Eip as usize).or_default() += 1;
            let eip = ctx.Eip as usize;
            let outside = !(dxw.0 <= eip && eip < dxw.1) && !(game.0 <= eip && eip < game.1);
            if outside {
                if let Some(d) = d { *via_dxw.entry(d).or_default() += 1; }
                if let Some(g) = g { *via_game.entry(g).or_default() += 1; }
            }
            total += 1;
        }
    }
    // Aggregate per module, and per 64-byte block within each module.
    let mut modules: HashMap<String, (u32, usize, HashMap<usize, u32>)> = HashMap::new();
    let mut cache: HashMap<usize, (usize, String)> = HashMap::new();
    for (&eip, &n) in &samples {
        let (base, name) = cache.entry(eip & !0xFFFF).or_insert_with(|| module_of(eip)).clone();
        let e = modules.entry(name).or_insert((0, base, HashMap::new()));
        e.0 += n;
        *e.2.entry(eip & !0x3F).or_default() += n;
    }
    let mut by_count: Vec<_> = modules.into_iter().collect();
    by_count.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    crate::log_line(format!("profile: {total} samples"));
    for (label, map, base) in [("dxwrapper.dll", &via_dxw, dxw.0), ("DungeonSiege.exe", &via_game, game.0)] {
        let mut v: Vec<_> = map.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        for (addr, c) in v.iter().take(8) {
            crate::log_line(format!("profile: called via {label}+0x{:X}: {:5.1}% of samples (time in driver/OS below it)", **addr - base, **c as f32 * 100.0 / total as f32));
        }
    }
    for (name, (n, base, blocks)) in by_count.iter().take(12) {
        crate::log_line(format!("profile: {:5.1}%  {name}", *n as f32 * 100.0 / total as f32));
        let mut hot: Vec<_> = blocks.iter().collect();
        hot.sort_by(|a, b| b.1.cmp(a.1));
        for (addr, c) in hot.iter().take(6) {
            crate::log_line(format!("profile:          {:5.1}%  {name}+0x{:X} (0x{:08X})", **c as f32 * 100.0 / total as f32, **addr - base, **addr));
        }
    }
}
