//! Private SkyLight and Process Manager calls that make background input
//! land. Every symbol is looked up at run time; when one is missing (a
//! future macOS) callers fall back to the public `CGEventPostToPid`.
//!
//! The event-record layouts are the ones yabai uses for "focus without
//! raise": they tell an app its window is key and active, which AppKit needs
//! before it will act on a click, without the window server raising the
//! window or moving the user's focus.

use std::ffi::{c_void, CStr};
use std::sync::OnceLock;

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::CGEvent;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Psn {
    high: u32,
    low: u32,
}

type PostToPid = unsafe extern "C" fn(i32, *const CGEvent);
type PostRecord = unsafe extern "C" fn(*const Psn, *const u8) -> i32;
type SetWindowLocation = unsafe extern "C" fn(*const CGEvent, CGPoint);
type GetProcessForPid = unsafe extern "C" fn(i32, *mut Psn) -> i32;
type SetFrontProcess = unsafe extern "C" fn(*const Psn, u32, u32) -> i32;
type GetFrontProcess = unsafe extern "C" fn(*mut Psn) -> i32;
type SetIntField = unsafe extern "C" fn(*const CGEvent, u32, i64);

struct Symbols {
    post_to_pid: Option<PostToPid>,
    post_record: Option<PostRecord>,
    set_window_location: Option<SetWindowLocation>,
    get_process_for_pid: Option<GetProcessForPid>,
    set_front_process: Option<SetFrontProcess>,
    get_front_process: Option<GetFrontProcess>,
    set_int_field: Option<SetIntField>,
}

fn sym<T>(handle: *mut c_void, name: &CStr) -> Option<T> {
    if handle.is_null() {
        return None;
    }
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

fn symbols() -> &'static Symbols {
    static S: OnceLock<Symbols> = OnceLock::new();
    S.get_or_init(|| unsafe {
        let sky = libc::dlopen(c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(), libc::RTLD_LAZY);
        let cg = libc::dlopen(
            c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics".as_ptr(),
            libc::RTLD_LAZY,
        );
        let hi = libc::dlopen(
            c"/System/Library/Frameworks/ApplicationServices.framework/Frameworks/HIServices.framework/HIServices".as_ptr(),
            libc::RTLD_LAZY,
        );
        let s = Symbols {
            post_to_pid: sym(sky, c"SLEventPostToPid"),
            post_record: sym(sky, c"SLPSPostEventRecordTo"),
            set_window_location: sym(cg, c"CGEventSetWindowLocation").or_else(|| sym(sky, c"SLEventSetWindowLocation")),
            get_process_for_pid: sym(hi, c"GetProcessForPID"),
            set_front_process: sym(sky, c"_SLPSSetFrontProcessWithOptions"),
            get_front_process: sym(sky, c"_SLPSGetFrontProcess"),
            set_int_field: sym(sky, c"SLEventSetIntegerValueField"),
        };
        log::debug!(
            "SkyLight: post_to_pid={} post_record={} window_location={} psn={} front={}",
            s.post_to_pid.is_some(),
            s.post_record.is_some(),
            s.set_window_location.is_some(),
            s.get_process_for_pid.is_some(),
            s.set_front_process.is_some(),
        );
        s
    })
}

pub fn psn(pid: i32) -> Option<Psn> {
    let f = symbols().get_process_for_pid?;
    let mut psn = Psn::default();
    (unsafe { f(pid, &mut psn) } == 0).then_some(psn)
}

/// Posts to one process through SkyLight, or `CGEventPostToPid` without it.
pub fn post(pid: i32, event: &CGEvent) {
    match symbols().post_to_pid {
        Some(f) => unsafe { f(pid, event) },
        None => CGEvent::post_to_pid(pid, Some(event)),
    }
}

/// Sets where in the window the event lands, as the window server would.
pub fn set_window_location(event: &CGEvent, local: CGPoint) {
    if let Some(f) = symbols().set_window_location {
        unsafe { f(event, local) }
    }
}

/// Sets a raw event field, including ones the public setter rejects.
pub fn set_field(event: &CGEvent, field: u32, value: i64) {
    match symbols().set_int_field {
        Some(f) => unsafe { f(event, field, value) },
        None => CGEvent::set_integer_value_field(Some(event), objc2_core_graphics::CGEventField(field), value),
    }
}

fn front_psn() -> Option<Psn> {
    let f = symbols().get_front_process?;
    let mut psn = Psn::default();
    (unsafe { f(&mut psn) } == 0).then_some(psn)
}

/// The 248-byte focus record: window id at 0x3c, direction at 0x8a
/// (1 gains focus, 2 loses it).
fn focus_record(window_id: u32, direction: u8) -> [u8; 0xf8] {
    let mut bytes = [0u8; 0xf8];
    bytes[0x04] = 0xf8;
    bytes[0x08] = 0x0d;
    bytes[0x3c..0x40].copy_from_slice(&window_id.to_le_bytes());
    bytes[0x8a] = direction;
    bytes
}

/// Who had focus before [`focus_without_raise`], to give it back.
pub struct FocusLease {
    previous: Option<(Psn, u32)>,
    target: Psn,
    window_id: u32,
}

/// Makes the target app treat `window_id` as its active key window, which
/// AppKit requires before it acts on a click, without raising the window or
/// changing the frontmost app. The user's app is told it lost focus until
/// the returned lease is dropped.
pub fn focus_without_raise(pid: i32, window_id: u32, previous: Option<(i32, u32)>) -> Option<FocusLease> {
    let post = symbols().post_record?;
    let target = psn(pid)?;
    let previous = previous
        .filter(|(p, _)| *p != pid)
        .and_then(|(p, wid)| Some((psn(p).or_else(front_psn)?, wid)));
    unsafe {
        if let Some((prev, prev_wid)) = &previous {
            post(prev, focus_record(*prev_wid, 2).as_ptr());
        }
        post(&target, focus_record(window_id, 1).as_ptr());
    }
    // Let AppKit update its key-window routing before events arrive.
    std::thread::sleep(std::time::Duration::from_millis(50));
    Some(FocusLease { previous, target, window_id })
}

impl Drop for FocusLease {
    fn drop(&mut self) {
        let Some(post) = symbols().post_record else { return };
        let Some((prev, prev_wid)) = &self.previous else { return };
        unsafe {
            post(&self.target, focus_record(self.window_id, 2).as_ptr());
            post(prev, focus_record(*prev_wid, 1).as_ptr());
        }
    }
}

/// Brings an app and all its windows back to the front: how focus goes back
/// to the user's app after a launch put a session window on top.
pub fn bring_to_front(pid: i32) -> bool {
    let (Some(front), Some(psn)) = (symbols().set_front_process, psn(pid)) else { return false };
    // kCPSAllWindows: raise every window, not just the key one.
    unsafe { front(&psn, 0, 0x100) == 0 }
}
