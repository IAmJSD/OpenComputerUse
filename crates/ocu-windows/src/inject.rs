//! Loads the panel hook into an app before its first instruction.
//!
//! The app is created suspended (see [`crate::launch`]). We copy the hook's
//! path into it and queue an APC on its primary thread that calls
//! `LoadLibraryW` on that path; resuming the thread runs it after the loader
//! is up and before the app's entry point. `CreateRemoteThread` would race
//! the loader in a never-resumed process. Nothing in the image is replaced.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use windows::core::{w, BOOL, PCSTR};
use windows::Win32::Foundation::{HANDLE, PAPCFUNC};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process, QueueUserAPC};

/// `LoadLibraryW` as an APC routine. kernel32 sits at the same address in
/// every process of one architecture, so ours is the target's.
fn load_library() -> Result<PAPCFUNC> {
    let kernel32 = unsafe { GetModuleHandleW(w!("kernel32.dll")) }?;
    let f = unsafe { GetProcAddress(kernel32, PCSTR(c"LoadLibraryW".as_ptr().cast())) }
        .ok_or_else(|| anyhow!("kernel32 has no LoadLibraryW"))?;
    // Both take one pointer-sized argument; the return value is dropped.
    Ok(Some(unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(usize)>(
            f,
        )
    }))
}

fn is_wow64(process: HANDLE) -> Result<bool> {
    let mut wow = BOOL(0);
    unsafe { IsWow64Process(process, &mut wow) }?;
    Ok(wow.as_bool())
}

/// Queues `dll` to load into a suspended process when `thread`, its primary
/// thread, is resumed.
pub fn queue_load(dll: &Path, process: HANDLE, thread: HANDLE) -> Result<()> {
    // Our LoadLibraryW is not mapped in a process of the other bitness, and
    // calling it there would crash the app.
    if is_wow64(process)? != is_wow64(unsafe { GetCurrentProcess() })? {
        bail!("the app's architecture differs from the hook's");
    }
    let load = load_library()?;

    // The APC runs in the target, so the path must live there.
    let path: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(dll.as_os_str())
        .chain(std::iter::once(0))
        .collect();
    let bytes = unsafe {
        std::slice::from_raw_parts(path.as_ptr().cast::<u8>(), path.len() * size_of::<u16>())
    };
    let remote = unsafe {
        VirtualAllocEx(
            process,
            None,
            bytes.len(),
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote.is_null() {
        bail!("no room in the app for the hook's path");
    }
    // Not freed on success: we cannot tell when LoadLibraryW has read it,
    // and one leaked path per app is cheaper than a use-after-free.
    let ok =
        unsafe { WriteProcessMemory(process, remote, bytes.as_ptr().cast(), bytes.len(), None) };
    if let Err(e) = ok {
        release(process, remote);
        bail!("could not hand the hook's path to the app: {e}");
    }
    if unsafe { QueueUserAPC(load, thread, remote as usize) } == 0 {
        let e = windows::core::Error::from_win32();
        release(process, remote);
        bail!("the app refused the hook: {e}");
    }
    log::info!("queued {} into the app before it starts", dll.display());
    Ok(())
}

fn release(process: HANDLE, remote: *mut c_void) {
    unsafe {
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    }
}
