//! Access to the real Direct3D 9 device underneath dxwrapper's D3D7 -> D3D9 translation, for
//! effects the game's fixed-function D3D7 can't express (shaders: see ao.rs).
//!
//! D3D9 COM vtables are shared by every object of a class inside d3d9.dll, so creating our own
//! IDirect3D9(Ex) object and patching its CreateDevice(Ex) slots also catches dxwrapper's.
//! The device's vtable is then patched the same way when dxwrapper creates it.

use crate::{patch, HRESULT, P};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const D3D_SDK_VERSION: u32 = 32;

// IDirect3D9: CreateDevice = 16. IDirect3D9Ex: CreateDeviceEx = 20.
orig!(D9_CREATE_DEVICE: fn(P, u32, u32, P, u32, *mut u32, *mut P) -> HRESULT);
orig!(D9_CREATE_DEVICE_EX: fn(P, u32, u32, P, u32, *mut u32, P, *mut P) -> HRESULT);

// IDirect3DDevice9 slots
const RELEASE: usize = 2;
const RESET: usize = 16;
const CREATE_DEPTH_STENCIL: usize = 29;
orig!(DEV9_RELEASE: fn(P) -> u32);
orig!(DEV9_RESET: fn(P, *mut u32) -> HRESULT);
orig!(DEV9_CREATE_DS: fn(P, u32, u32, u32, u32, u32, i32, *mut P, P) -> HRESULT);

/// The D3D9 device dxwrapper renders the game with (0 = none yet).
static DEVICE9: AtomicUsize = AtomicUsize::new(0);
pub fn device() -> P { DEVICE9.load(Relaxed) as P }

/// Called once dxwrapper is loaded (before the game creates anything).
pub unsafe fn install() {
    let path: Vec<u16> = {
        let mut buf = [0u16; 260];
        let n = windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buf.as_mut_ptr(), 260) as usize;
        buf[..n].iter().copied().chain("\\d3d9.dll\0".encode_utf16()).collect()
    };
    let lib = LoadLibraryW(path.as_ptr());
    if lib.is_null() {
        log!("d3d9: system d3d9.dll not found");
        return;
    }
    if let Some(create) = GetProcAddress(lib, c"Direct3DCreate9".as_ptr().cast()) {
        let create: unsafe extern "system" fn(u32) -> P = std::mem::transmute(create);
        let d3d = create(D3D_SDK_VERSION);
        if !d3d.is_null() {
            patch(d3d, 16, create_device as usize, &D9_CREATE_DEVICE);
            // Keep the object: releasing it is harmless, but there's no need to.
        }
    }
    if let Some(create) = GetProcAddress(lib, c"Direct3DCreate9Ex".as_ptr().cast()) {
        let create: unsafe extern "system" fn(u32, *mut P) -> HRESULT = std::mem::transmute(create);
        let mut d3d = std::ptr::null_mut();
        if create(D3D_SDK_VERSION, &mut d3d) >= 0 && !d3d.is_null() {
            patch(d3d, 16, create_device as usize, &D9_CREATE_DEVICE);
            patch(d3d, 20, create_device_ex as usize, &D9_CREATE_DEVICE_EX);
        }
    }
    log!("d3d9: CreateDevice hooks installed");
}

/// D3DPRESENT_PARAMETERS as dwords: width, height, format, count, multisample, quality,
/// swap effect, window, windowed, auto depth, depth format, flags, refresh, interval.
fn describe(pp: *const u32) -> String {
    let p = unsafe { std::slice::from_raw_parts(pp, 14) };
    format!(
        "{}x{} fmt={} buffers={} msaa={} swap={} windowed={} autodepth={} depthfmt={} flags=0x{:X} interval=0x{:X}",
        p[0], p[1], p[2], p[3], p[4], p[6], p[8], p[9], p[10], p[11], p[13]
    )
}

unsafe extern "system" fn create_device(this: P, adapter: u32, kind: u32, hwnd: P, flags: u32, pp: *mut u32, out: *mut P) -> HRESULT {
    let hr = call!(D9_CREATE_DEVICE as fn(P, u32, u32, P, u32, *mut u32, *mut P) -> HRESULT; this, adapter, kind, hwnd, flags, pp, out);
    log!("d3d9: CreateDevice(flags=0x{flags:X}, {}) -> 0x{:08X}", describe(pp), hr as u32);
    if hr >= 0 {
        created(*out);
    }
    hr
}

unsafe extern "system" fn create_device_ex(this: P, adapter: u32, kind: u32, hwnd: P, flags: u32, pp: *mut u32, mode: P, out: *mut P) -> HRESULT {
    let hr = call!(D9_CREATE_DEVICE_EX as fn(P, u32, u32, P, u32, *mut u32, P, *mut P) -> HRESULT; this, adapter, kind, hwnd, flags, pp, mode, out);
    log!("d3d9: CreateDeviceEx(flags=0x{flags:X}, {}) -> 0x{:08X}", describe(pp), hr as u32);
    if hr >= 0 {
        created(*out);
    }
    hr
}

unsafe fn created(dev: P) {
    crate::ao::device_lost(); // anything of ours belonged to the previous device
    DEVICE9.store(dev as usize, Relaxed);
    patch(dev, RELEASE, dev9_release as usize, &DEV9_RELEASE);
    patch(dev, RESET, dev9_reset as usize, &DEV9_RESET);
    patch(dev, CREATE_DEPTH_STENCIL, dev9_create_ds as usize, &DEV9_CREATE_DS);
}

/// The device's current reference count (unhooked AddRef + Release).
pub unsafe fn refcount(dev: P) -> u32 {
    crate::method::<unsafe extern "system" fn(P) -> u32>(dev, 1)(dev);
    call!(DEV9_RELEASE as fn(P) -> u32; dev)
}

/// dxwrapper destroys its device by releasing it down to zero, even though our objects hold
/// references on it; releasing ours afterwards would touch freed memory. So when a Release
/// would leave only our references, ours go first.
unsafe extern "system" fn dev9_release(this: P) -> u32 {
    if let Some(ours) = crate::ao::refs_held_on(this) {
        if refcount(this) <= ours + 1 {
            log!("d3d9: device {this:?} is being destroyed; releasing our objects first");
            crate::ao::device_lost();
        }
    }
    call!(DEV9_RELEASE as fn(P) -> u32; this)
}

unsafe extern "system" fn dev9_reset(this: P, pp: *mut u32) -> HRESULT {
    crate::ao::device_lost(); // our D3DPOOL_DEFAULT objects must go before a Reset
    let hr = call!(DEV9_RESET as fn(P, *mut u32) -> HRESULT; this, pp);
    log!("d3d9: Reset({}) -> 0x{:08X}", describe(pp), hr as u32);
    hr
}

unsafe extern "system" fn dev9_create_ds(this: P, w: u32, h: u32, fmt: u32, ms: u32, q: u32, discard: i32, out: *mut P, share: P) -> HRESULT {
    if let Some(hr) = crate::ao::create_depth(this, w, h, fmt, ms, out) {
        return hr;
    }
    let hr = call!(DEV9_CREATE_DS as fn(P, u32, u32, u32, u32, u32, i32, *mut P, P) -> HRESULT; this, w, h, fmt, ms, q, discard, out, share);
    log!("d3d9: CreateDepthStencilSurface({w}x{h} fmt={fmt} msaa={ms} discard={discard}) -> 0x{:08X} {:?}", hr as u32, if hr >= 0 { *out } else { std::ptr::null_mut() });
    hr
}
