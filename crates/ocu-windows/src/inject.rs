//! Loading the panel hook into an app before it runs a single instruction.
//!
//! The app is created suspended and already inside its Job Object
//! ([`crate::launch`]), which makes this the one moment where code can get
//! in before the app decides what to be. The sequence is:
//!
//! 1. copy the hook's path into the target's address space,
//! 2. queue an APC on the target's only thread that calls `LoadLibraryW` on
//!    that path,
//! 3. let the launch code resume the thread, which runs the APC.
//!
//! An APC rather than `CreateRemoteThread` because a remote thread only
//! starts once the loader has initialised, which has not happened in a
//! process that has never been resumed: it would race the loader and lose.
//! An APC queued on the primary thread runs after the loader is up but
//! before the app's entry point, which is exactly the window wanted here.
//!
//! Nothing in the target is replaced or unmapped: the hook is added to the
//! process, not substituted for its image. That matters beyond tidiness.
//! Mapping a section over a process's image and redirecting its entry
//! point is process hollowing, the signature endpoint protection looks for
//! hardest, and none of it is needed to add a library to a process.

use std::ffi::c_void;
use std::path::Path;
use std::ptr;

use anyhow::{anyhow, bail, Result};
use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};

/// A function pointer as `GetProcAddress` hands it back, with no signature
/// of its own.
type FarProc = unsafe extern "system" fn() -> isize;

/// `NtQueueApcThread` from ntdll, which `QueueUserAPC` does not wrap.
///
/// ```text
/// NTSTATUS NTAPI NtQueueApcThread(HANDLE, PVOID ApcRoutine, PVOID ApcArgument,
///                                 ULONG NumberOfApcsToQueue, PULONG ApcThreadIndex);
/// ```
type NtQueueApcThread =
    unsafe extern "system" fn(HANDLE, *mut c_void, *mut c_void, u32, *mut u32) -> i32;

/// Looks up an exported function by name in a module already loaded here.
fn export(module: &str, name: &str) -> Result<FarProc> {
    let m: Vec<u16> = module.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe { GetModuleHandleW(PCWSTR(m.as_ptr())) }?;
    // `GetProcAddress` takes ANSI, the one relic of this corner of Win32.
    let mut n = name.as_bytes().to_vec();
    n.push(0);
    let f = unsafe { GetProcAddress(handle, PCSTR(n.as_ptr())) }
        .ok_or_else(|| anyhow!("{module} has no {name}"))?;
    Ok(f)
}

/// `LoadLibraryW` itself, to run in the target.
fn load_library() -> Result<FarProc> {
    export("kernel32.dll", "LoadLibraryW")
}

fn nt_queue_apc_thread() -> Result<NtQueueApcThread> {
    // ntdll is mapped into every process, so this cannot fail in practice.
    let f = export("ntdll.dll", "NtQueueApcThread")?;
    Ok(unsafe { std::mem::transmute::<FarProc, NtQueueApcThread>(f) })
}

/// Queues the hook to be loaded into a suspended process.
///
/// `process` and `thread` are the handles from `CreateProcessW` and its
/// primary thread, both still suspended. The APC runs when the thread is
/// next resumed, which is the launch code's next step.
pub fn queue_load(dll: &Path, process: HANDLE, thread: HANDLE) -> Result<()> {
    let queue = nt_queue_apc_thread()?;
    let load = load_library()?;

    // The path has to live in the target's own memory: the APC runs over
    // there, and a pointer from here would mean nothing to it.
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
    // The slot is deliberately not freed on success. LoadLibraryW has read
    // it by the time the hook is asked anything, but nothing here can know
    // that, and freeing too early is a use-after-free in the target. One
    // leaked path per app started, in a process about to be thrown away,
    // is the right side of that trade.
    let ok =
        unsafe { WriteProcessMemory(process, remote, bytes.as_ptr().cast(), bytes.len(), None) };
    if let Err(e) = ok {
        release(process, remote);
        bail!("could not hand the hook's path to the app: {e}");
    }

    // NTSTATUS is signed; anything non-negative is success.
    let status = unsafe {
        queue(
            thread,
            load as *mut c_void,
            remote.cast(),
            1,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        release(process, remote);
        bail!(
            "the app refused the hook (ntstatus 0x{:08x})",
            status as u32
        );
    }
    log::info!("queued {} into the app before it starts", dll.display());
    Ok(())
}

fn release(process: HANDLE, remote: *mut c_void) {
    unsafe {
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    }
}
