//! Smoke test for desiege.dll, no game needed:
//!   cargo build -p desiege --release --target i686-pc-windows-msvc --lib --examples
//!   target\i686-pc-windows-msvc\release\examples\selfcheck.exe width=2560 height=1440
//! Loads the built desiege.dll, makes the calls the game makes, and checks its effects.
//! To test the game-folder stub instead: DDRAW_DLL=<stub ddraw.dll> DESIEGE_DLL=<desiege.dll>.

use std::ffi::{c_char, c_void};
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type P = *mut c_void;
type Hr = i32;

#[link(name = "kernel32")]
extern "system" {
    // Imported through this exe's IAT, exactly like DungeonSiege.exe does.
    fn CreateFileA(name: *const c_char, access: u32, share: u32, sa: P, disp: u32, flags: u32, tmpl: P) -> P;
}

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const IID_IDIRECTDRAW7: Guid = Guid(0x15E65EC0, 0x3B9C, 0x11D2, [0xB9, 0x2F, 0x00, 0x60, 0x97, 0x97, 0xEA, 0x5B]);
const IID_IDIRECT3D7: Guid = Guid(0xF5049E77, 0x4861, 0x11D2, [0xA4, 0x07, 0x00, 0xA0, 0xC9, 0x06, 0x29, 0xA8]);

unsafe fn method<F>(obj: P, idx: usize) -> F {
    std::mem::transmute_copy(&*(*(obj as *const *const usize)).add(idx))
}

unsafe extern "system" fn dev_cb(_: *const i8, _: *const i8, _: P, _: P) -> Hr { 1 } // D3DENUMRET_OK

fn main() {
    let exe = std::env::current_exe().unwrap();
    let out_dir = exe.parent().unwrap();
    let dll = std::env::var_os("DDRAW_DLL").map(Into::into)
        .unwrap_or_else(|| out_dir.parent().unwrap().join("desiege.dll"));
    let log = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join(r"Desiege\desiege.log");
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dll.as_os_str().encode_wide().chain([0]).collect();
        let lib = LoadLibraryW(wide.as_ptr());
        assert!(!lib.is_null(), "load {}", dll.display());
        let create: unsafe extern "system" fn(*const Guid, *mut P, *const Guid, P) -> Hr =
            std::mem::transmute(GetProcAddress(lib, c"DirectDrawCreateEx".as_ptr().cast()).unwrap());

        let mut dd: P = std::ptr::null_mut();
        assert!(create(std::ptr::null(), &mut dd, &IID_IDIRECTDRAW7, std::ptr::null_mut()) >= 0);

        let mut caps = [0x4000u32, 0, 0, 0]; // DDSCAPS_VIDEOMEMORY
        let (mut total, mut free) = (0u32, 0u32);
        method::<unsafe extern "system" fn(P, *mut u32, *mut u32, *mut u32) -> Hr>(dd, 23)(dd, caps.as_mut_ptr(), &mut total, &mut free);
        println!("vidmem total={total} free={free}");
        assert!(total <= i32::MAX as u32 && free <= i32::MAX as u32, "proxy must cap VRAM below 2^31");

        let mut d3d: P = std::ptr::null_mut();
        let hr = method::<unsafe extern "system" fn(P, *const Guid, *mut P) -> Hr>(dd, 0)(dd, &IID_IDIRECT3D7, &mut d3d);
        assert!(hr >= 0, "QueryInterface(IDirect3D7) failed: 0x{:08X}", hr as u32);
        method::<unsafe extern "system" fn(P, unsafe extern "system" fn(*const i8, *const i8, P, P) -> Hr, P) -> Hr>(d3d, 3)(d3d, dev_cb, std::ptr::null_mut());
    }
    // system_detail.gas redirect: open a fake one through CreateFileA and read what we get.
    let fake = out_dir.join("system_detail.gas");
    std::fs::write(&fake, "[system_detail]\n{\n\t[64M32B]\n\t{\n\t\t[resolutions]\n\t\t{\n\t\t\t[1024x768]\t{ max_back_buffers = 2; }\n\t\t}\n\t}\n}\n").unwrap();
    let path = std::ffi::CString::new(fake.to_str().unwrap()).unwrap();
    let h = unsafe { CreateFileA(path.as_ptr(), 0x8000_0000, 1, std::ptr::null_mut(), 3, 0x80, std::ptr::null_mut()) };
    assert!(h as isize != -1, "CreateFileA failed");
    let mut seen = String::new();
    unsafe { std::fs::File::from_raw_handle(h) }.read_to_string(&mut seen).unwrap();
    let want = std::env::args().any(|a| a.starts_with("width=")) as usize;
    assert_eq!(seen.matches("[2560x1440]").count(), want, "redirected content:\n{seen}");
    assert!(!std::fs::read_to_string(&fake).unwrap().contains("2560x1440"), "original must stay untouched");

    let text = std::fs::read_to_string(&log).expect("proxy log missing");
    print!("{text}");
    for needle in ["DirectDrawCreateEx", "GetAvailableVidMem", "QueryInterface", "d3d device", "CreateFileA"] {
        assert!(text.contains(needle), "log lacks {needle}");
    }
    println!("selfcheck OK");
}
