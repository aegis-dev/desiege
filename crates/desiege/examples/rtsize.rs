//! Experiment: which render-target sizes can the native D3D7 path create a device on?
//!   target\i686-pc-windows-msvc\release\examples\rtsize.exe
//! Uses the system ddraw.dll (or $DDRAW_DLL) and an offscreen surface, so the display is untouched.

use std::ffi::c_void;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type P = *mut c_void;
type Hr = i32;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const IID_IDIRECTDRAW7: Guid = Guid(0x15E65EC0, 0x3B9C, 0x11D2, [0xB9, 0x2F, 0x00, 0x60, 0x97, 0x97, 0xEA, 0x5B]);
const IID_IDIRECT3D7: Guid = Guid(0xF5049E77, 0x4861, 0x11D2, [0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8]);
const TNL_HAL: Guid = Guid(0xF5049E78, 0x4861, 0x11D2, [0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8]);
const HAL: Guid = Guid(0x84E63DE0, 0x46AA, 0x11CF, [0x81, 0x6F, 0x00, 0x00, 0xC0, 0x20, 0x15, 0x6E]);

unsafe fn method<F>(obj: P, idx: usize) -> F {
    std::mem::transmute_copy(&*(*(obj as *const *const usize)).add(idx))
}

fn main() {
    unsafe {
        // DDRAW_DLL: test another ddraw.dll (e.g. our proxy, which then uses desiege\dxwrapper.dll).
        let dll = std::env::var("DDRAW_DLL").unwrap_or_else(|_| r"C:\Windows\SysWOW64\ddraw.dll".into());
        let path: Vec<u16> = dll.encode_utf16().chain([0]).collect();
        let lib = LoadLibraryW(path.as_ptr());
        let create: unsafe extern "system" fn(*const Guid, *mut P, *const Guid, P) -> Hr =
            std::mem::transmute(GetProcAddress(lib, c"DirectDrawCreateEx".as_ptr().cast()).unwrap());
        let mut dd: P = std::ptr::null_mut();
        assert!(create(std::ptr::null(), &mut dd, &IID_IDIRECTDRAW7, std::ptr::null_mut()) >= 0);
        let hr = method::<unsafe extern "system" fn(P, P, u32) -> Hr>(dd, 20)(dd, std::ptr::null_mut(), 8); // DDSCL_NORMAL
        assert!(hr >= 0, "SetCooperativeLevel 0x{:08X}", hr as u32);
        let mut d3d: P = std::ptr::null_mut();
        assert!(method::<unsafe extern "system" fn(P, *const Guid, *mut P) -> Hr>(dd, 0)(dd, &IID_IDIRECT3D7, &mut d3d) >= 0);

        for (w, h) in [(800, 600), (2048, 1152), (2048, 1536), (2049, 1152), (2304, 1296), (2560, 1440), (4096, 2160)] {
            let mut desc = [0u32; 31]; // DDSURFACEDESC2, 124 bytes
            desc[0] = 124;
            desc[1] = 1 | 2 | 4; // DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH
            desc[2] = h;
            desc[3] = w;
            desc[26] = 0x40 | 0x2000 | 0x4000; // OFFSCREENPLAIN | 3DDEVICE | VIDEOMEMORY (ddsCaps @104)
            let mut surf: P = std::ptr::null_mut();
            let hr = method::<unsafe extern "system" fn(P, *mut u32, *mut P, P) -> Hr>(dd, 6)(dd, desc.as_mut_ptr(), &mut surf, std::ptr::null_mut());
            if hr < 0 {
                println!("{w}x{h}: CreateSurface FAIL 0x{:08X}", hr as u32);
                continue;
            }
            for (name, dev_id) in [("T&L HAL", &TNL_HAL), ("HAL", &HAL)] {
                let mut dev: P = std::ptr::null_mut();
                let hr = method::<unsafe extern "system" fn(P, *const Guid, P, *mut P) -> Hr>(d3d, 4)(d3d, dev_id, surf, &mut dev);
                println!("{w}x{h}: CreateDevice({name}) {}", if hr >= 0 { "OK".into() } else { format!("FAIL 0x{:08X}", hr as u32) });
                if !dev.is_null() {
                    method::<unsafe extern "system" fn(P) -> u32>(dev, 2)(dev);
                }
            }
            method::<unsafe extern "system" fn(P) -> u32>(surf, 2)(surf);
        }
    }
}
