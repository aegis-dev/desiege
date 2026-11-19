//! The game streams most geometry through a 16384-vertex ring buffer: Lock(NOOVERWRITE),
//! append, DrawIndexedPrimitiveVB, repeat (DISCARD when full) - the textbook fast path.
//! dxwrapper drops NOOVERWRITE and keeps the buffer in D3D9's managed pool, so every append
//! re-uploads the whole buffer with a sync: ~60% of the frame went to the driver waiting and
//! copying. Buffers the game locks with NOOVERWRITE/DISCARD get a system-memory mirror instead:
//! Lock hands out the mirror, Unlock is free, and draws from them become draw-from-memory calls
//! covering only the vertices used, which D3D9 streams without stalling. Static buffers
//! (terrain, meshes) are untouched.

use crate::{debug::experiment, device::fvf_stride, method, patch, E_FAIL, HRESULT, P};
use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{atomic::{AtomicUsize, Ordering::Relaxed}, Mutex},
};


struct Mirror { data: Vec<u8>, fvf: u32, stride: usize }
static MIRRORS: Mutex<Option<HashMap<usize, Mirror>>> = Mutex::new(None);
fn with<R>(f: impl FnOnce(&mut HashMap<usize, Mirror>) -> R) -> Option<R> {
    MIRRORS.lock().ok().map(|mut m| f(m.get_or_insert_with(HashMap::new)))
}

fn enabled() -> bool {
    !experiment("nodynvb")
}

orig!(D3D_CREATE_VB: fn(P, *mut u32, *mut P, u32) -> HRESULT);
orig!(VB_RELEASE: fn(P) -> u32);
orig!(VB_LOCK: fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT);
orig!(VB_UNLOCK: fn(P) -> HRESULT);
orig!(VB_PROCESS: fn(P, u32, u32, u32, P, u32, P, u32) -> HRESULT);

/// Hook IDirect3D7::CreateVertexBuffer (slot 5); vertex buffers get hooked when created.
pub unsafe fn hook_d3d7(d3d: P) {
    patch(d3d, 5, create_vb as usize, &D3D_CREATE_VB);
}

unsafe extern "system" fn create_vb(this: P, desc: *mut u32, out: *mut P, flags: u32) -> HRESULT {
    let hr = call!(D3D_CREATE_VB as fn(P, *mut u32, *mut P, u32) -> HRESULT; this, desc, out, flags);
    if hr >= 0 && !out.is_null() && !(*out).is_null() {
        let vb = *out;
        patch(vb, 2, vb_release as usize, &VB_RELEASE);
        patch(vb, 3, vb_lock as usize, &VB_LOCK);
        patch(vb, 4, vb_unlock as usize, &VB_UNLOCK);
        patch(vb, 5, vb_process as usize, &VB_PROCESS);
    }
    hr
}

/// D3DVERTEXBUFFERDESC { size, caps, fvf, count } via GetVertexBufferDesc (slot 6).
unsafe fn desc(vb: P) -> Option<(u32, u32)> {
    let mut d = [16u32, 0, 0, 0];
    let get: unsafe extern "system" fn(P, *mut u32) -> HRESULT = method(vb, 6);
    (get(vb, d.as_mut_ptr()) >= 0).then_some((d[2], d[3]))
}

unsafe extern "system" fn vb_lock(this: P, flags: u32, out: *mut *mut c_void, size: *mut u32) -> HRESULT {
    const NOOVERWRITE: u32 = 0x1000; const DISCARDCONTENTS: u32 = 0x2000; const READONLY: u32 = 0x10;
    let dynamic_lock = flags & (NOOVERWRITE | DISCARDCONTENTS) != 0;
    if enabled() && !out.is_null() {
        let known = with(|m| m.contains_key(&(this as usize))).unwrap_or(false);
        if known || dynamic_lock {
            if !known {
                // First dynamic lock: start the mirror from the buffer's current contents.
                let Some((fvf, count)) = desc(this) else {
                    return call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, flags, out, size);
                };
                let stride = fvf_stride(fvf);
                let mut data = vec![0u8; stride * count as usize];
                let mut p = std::ptr::null_mut();
                if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, READONLY | 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
                    std::ptr::copy_nonoverlapping(p as *const u8, data.as_mut_ptr(), data.len());
                    call!(VB_UNLOCK as fn(P) -> HRESULT; this);
                }
                let n = with(|m| {
                    m.insert(this as usize, Mirror { data, fvf, stride });
                    m.len()
                });
                log!("dynamic vb #{}: {this:?} {count} verts fvf=0x{fvf:X} mirrored", n.unwrap_or(0));
            }
            return with(|m| {
                let mi = m.get_mut(&(this as usize)).unwrap();
                *out = mi.data.as_mut_ptr().cast();
                if !size.is_null() {
                    *size = mi.data.len() as u32;
                }
                0
            })
            .unwrap_or(E_FAIL);
        }
    }
    call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; this, flags, out, size)
}

unsafe extern "system" fn vb_unlock(this: P) -> HRESULT {
    if with(|m| m.contains_key(&(this as usize))).unwrap_or(false) {
        return 0;
    }
    call!(VB_UNLOCK as fn(P) -> HRESULT; this)
}

unsafe extern "system" fn vb_release(this: P) -> u32 {
    let left = call!(VB_RELEASE as fn(P) -> u32; this);
    if left == 0 {
        with(|m| m.remove(&(this as usize)));
    }
    left
}

/// Copy a mirror into the real buffer (something other than our draws will read it).
unsafe fn sync_real(vb: P) {
    with(|m| {
        let Some(mi) = m.get(&(vb as usize)) else { return };
        let mut p = std::ptr::null_mut();
        if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; vb, 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
            std::ptr::copy_nonoverlapping(mi.data.as_ptr(), p as *mut u8, mi.data.len());
            call!(VB_UNLOCK as fn(P) -> HRESULT; vb);
        }
    });
}
/// Refresh a mirror from the real buffer (something else wrote it).
unsafe fn sync_mirror(vb: P) {
    with(|m| {
        let Some(mi) = m.get_mut(&(vb as usize)) else { return };
        let mut p = std::ptr::null_mut();
        if call!(VB_LOCK as fn(P, u32, *mut *mut c_void, *mut u32) -> HRESULT; vb, 0x10 | 1, &mut p, std::ptr::null_mut()) >= 0 && !p.is_null() {
            std::ptr::copy_nonoverlapping(p as *const u8, mi.data.as_mut_ptr(), mi.data.len());
            call!(VB_UNLOCK as fn(P) -> HRESULT; vb);
        }
    });
}

unsafe extern "system" fn vb_process(this: P, op: u32, dst_index: u32, count: u32, src: P, src_index: u32, dev: P, flags: u32) -> HRESULT {
    if !src.is_null() {
        sync_real(src);
    }
    let hr = call!(VB_PROCESS as fn(P, u32, u32, u32, P, u32, P, u32) -> HRESULT; this, op, dst_index, count, src, src_index, dev, flags);
    sync_mirror(this);
    hr
}

/// For a mirrored buffer: pointer to vertex `start` and the FVF.
pub unsafe fn range(vb: P, start: u32, num: u32) -> Option<(*const c_void, u32)> {
    with(|m| {
        let mi = m.get(&(vb as usize))?;
        let end = (start as usize + num as usize) * mi.stride;
        (end <= mi.data.len()).then(|| (mi.data.as_ptr().add(start as usize * mi.stride).cast(), mi.fvf))
    })
    .flatten()
}
