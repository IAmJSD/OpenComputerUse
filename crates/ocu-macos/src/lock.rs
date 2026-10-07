//! Working while the Mac is locked.
//!
//! macOS only dismisses the lock when `loginwindow` runs the
//! `system.login.screensaver` authorization. We install an authorization
//! plugin (see `packaging/macos/lockplugin/`) as the first, short-circuiting
//! branch of that right's rule: when the plugin returns Allow, loginwindow
//! unlocks with no password.
//!
//! At run time two things make a headless unlock happen:
//!   1. A keystroke posted straight to loginwindow's PID, which is what makes
//!      loginwindow begin the unlock authorization (the secure lock screen
//!      ignores ordinary injected input, but a PID-targeted event reaches it).
//!   2. A local socket the plugin connects to from inside `authorizationhost`
//!      (running as `_securityagent`). While an unlock is pending this answers
//!      "allow", and the plugin turns that into the Allow result.
//!
//! The socket is world-connectable (0600 would lock `_securityagent` out), so
//! both ends check each other: the plugin requires our Developer ID identity,
//! and we require the caller to be one of Apple's authorization agents running
//! as root or `_securityagent`.
//!
//! After an unlock, [`unlock`] arms an input guard: a suppressing event tap on
//! the physical keyboard and mouse. The agent's own input is delivered
//! per-process and never reaches this tap, so the first thing it sees is a
//! real local touch, which it swallows and uses to relock the Mac. The tap
//! lives in this process, so if the app dies the suppression ends with it.

use std::ffi::{c_void, CString};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use objc2_core_graphics::CGEvent;

/// Root-owned home for the rule backup and the socket directory. Not under
/// /var/run, which macOS empties at every boot.
const SUPPORT_DIR: &str = "/Library/Application Support/OpenComputerUse";
/// Where the plugin and the app rendezvous. Created by the privileged
/// installer and owned by the user, so the app can bind here. Must match
/// `kSocketPath` in plugin.m.
pub const SOCKET_DIR: &str = "/Library/Application Support/OpenComputerUse/run";
pub const SOCKET_PATH: &str = "/Library/Application Support/OpenComputerUse/run/unlock.sock";
const PLUGIN_DEST: &str =
    "/Library/Security/SecurityAgentPlugins/OcuLockAuthorizationPlugin.bundle";
/// Who may have signed the plugin `install` copies into the system.
const PLUGIN_REQUIREMENT: &str = r#"anchor apple generic and identifier "com.infrawrench.opencomputeruse.lockplugin" and certificate leaf[subject.OU] = "CS54L4CF2Z""#;
const RIGHT: &str = "system.login.screensaver";
const SUBRULE: &str = "com.infrawrench.opencomputeruse.unlock";
/// `_securityagent`, the user SecurityAgent runs the plugin as.
const SECURITYAGENT_UID: u32 = 92;

/// Set while we want the next loginwindow unlock evaluation to be allowed.
static PENDING: AtomicBool = AtomicBool::new(false);

// ---- private symbols, looked up once (as in sky.rs) ----

type CopySessionDict = unsafe extern "C" fn() -> *const c_void;
type DictGetValue = unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void;
type StrCreate = unsafe extern "C" fn(*const c_void, *const i8, u32) -> *const c_void;
type Release = unsafe extern "C" fn(*const c_void);

struct Syms {
    session_dict: Option<CopySessionDict>,
    dict_get: Option<DictGetValue>,
    str_create: Option<StrCreate>,
    release: Option<Release>,
    boolean_true: *const c_void,
}
unsafe impl Sync for Syms {}
unsafe impl Send for Syms {}

fn sym<T>(handle: *mut c_void, name: &std::ffi::CStr) -> Option<T> {
    if handle.is_null() {
        return None;
    }
    let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

fn syms() -> &'static Syms {
    static S: OnceLock<Syms> = OnceLock::new();
    S.get_or_init(|| unsafe {
        let cg = libc::dlopen(
            c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics".as_ptr(),
            libc::RTLD_LAZY,
        );
        let cf = libc::dlopen(
            c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(),
            libc::RTLD_LAZY,
        );
        let bt = libc::dlsym(cf, c"kCFBooleanTrue".as_ptr());
        Syms {
            session_dict: sym(cg, c"CGSessionCopyCurrentDictionary"),
            dict_get: sym(cf, c"CFDictionaryGetValue"),
            str_create: sym(cf, c"CFStringCreateWithCString"),
            release: sym(cf, c"CFRelease"),
            // dlsym gives the address of the global; deref for the singleton.
            boolean_true: if bt.is_null() {
                std::ptr::null()
            } else {
                *(bt as *const *const c_void)
            },
        }
    })
}

/// Whether the login session's screen is currently locked.
pub fn screen_is_locked() -> bool {
    let s = syms();
    let (Some(copy), Some(get), Some(mk), Some(release)) =
        (s.session_dict, s.dict_get, s.str_create, s.release)
    else {
        return false;
    };
    unsafe {
        let dict = copy();
        if dict.is_null() {
            return false;
        }
        // kCFStringEncodingUTF8 = 0x08000100.
        let key = mk(
            std::ptr::null(),
            c"CGSSessionScreenIsLocked".as_ptr(),
            0x0800_0100,
        );
        let val = if key.is_null() {
            std::ptr::null()
        } else {
            get(dict, key)
        };
        let locked = !val.is_null() && val == s.boolean_true;
        if !key.is_null() {
            release(key);
        }
        release(dict);
        locked
    }
}

/// This user's loginwindow pid, so a trigger event can be aimed at it. With
/// fast user switching every session has its own loginwindow.
fn loginwindow_pid() -> Option<i32> {
    let uid = unsafe { libc::getuid() }.to_string();
    let out = std::process::Command::new("/usr/bin/pgrep")
        .args(["-x", "-u", &uid, "loginwindow"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
}

/// Posts a benign keystroke (a character, then Return) straight to
/// loginwindow, which makes it begin the unlock authorization.
fn poke_loginwindow(pid: i32) {
    for code in [0u16, 0x24] {
        for down in [true, false] {
            if let Some(e) = CGEvent::new_keyboard_event(None, code, down) {
                CGEvent::post_to_pid(pid, Some(&e));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// True once the installer has put the socket directory and plugin in place.
pub fn installed() -> bool {
    Path::new(SOCKET_DIR).is_dir() && Path::new(PLUGIN_DEST).is_dir()
}

/// Starts the socket responder once. It answers "allow" only while an unlock
/// is pending and only to Apple's authorization agents.
fn ensure_responder() -> Result<()> {
    static STARTED: Mutex<bool> = Mutex::new(false);
    let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
    if *started {
        return Ok(());
    }
    if !Path::new(SOCKET_DIR).is_dir() {
        bail!("lock-screen support is not installed; run `sudo opencomputeruse install-lock`");
    }
    let _ = std::fs::remove_file(SOCKET_PATH);
    let listener =
        UnixListener::bind(SOCKET_PATH).with_context(|| format!("binding {SOCKET_PATH}"))?;
    // _securityagent must be able to connect; peers are verified in serve_one.
    set_mode(SOCKET_PATH, 0o666)?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = serve_one(stream);
        }
    });
    *started = true;
    Ok(())
}

fn serve_one(mut stream: UnixStream) -> Result<()> {
    // The plugin blocks the unlock while it waits on us, and connections are
    // served one at a time, so a stalled peer must not hold either up.
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let trusted = peer_is_apple_agent(&stream);
    let is_pending = PENDING.load(Ordering::SeqCst);
    if debug() {
        eprintln!("[ocu-lock] connection: trusted_peer={trusted} pending={is_pending}");
    }
    if !trusted {
        return Ok(());
    }
    let mut buf = [0u8; 16];
    let _ = stream.read(&mut buf);
    let answer: &[u8] = if is_pending { b"allow\n" } else { b"deny\n" };
    stream.write_all(answer)?;
    Ok(())
}

fn debug() -> bool {
    std::env::var_os("OCU_LOCK_DEBUG").is_some()
}

/// Unlocks the Mac if it is locked, returning whether it ended up unlocked.
/// A no-op (Ok(true)) when the screen is already unlocked.
pub fn unlock(timeout: Duration) -> Result<bool> {
    if !screen_is_locked() {
        return Ok(true);
    }
    ensure_responder()?;
    let Some(pid) = loginwindow_pid() else {
        bail!("loginwindow is not running");
    };
    PENDING.store(true, Ordering::SeqCst);
    // Wake the display so loginwindow shows the lock and will act on the poke.
    if let Ok(mut child) = std::process::Command::new("/usr/bin/caffeinate")
        .args(["-u", "-t", "3"])
        .spawn()
    {
        std::thread::spawn(move || child.wait());
    }
    // Give the display time to wake and loginwindow to be ready to receive the
    // keystroke, or the poke is dropped and loginwindow never evaluates.
    std::thread::sleep(Duration::from_millis(1200));
    // A poke that lands while the display is waking is used up by the wake,
    // so poke again while still locked. Capped, because a poke the plugin
    // doesn't answer counts as a wrong password.
    const POKES: u32 = 4;
    const POKE_EVERY: Duration = Duration::from_secs(3);
    let deadline = Instant::now() + timeout;
    let mut pokes = 0;
    let mut last_poke = Instant::now() - POKE_EVERY;
    let unlocked = loop {
        if !screen_is_locked() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        if pokes < POKES && last_poke.elapsed() >= POKE_EVERY {
            log::info!("poking loginwindow pid {pid} to begin the unlock");
            poke_loginwindow(pid);
            pokes += 1;
            last_poke = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(150));
    };
    PENDING.store(false, Ordering::SeqCst);
    if unlocked {
        // Keep the lock's protection: block local use, and relock the moment
        // someone physically touches the machine.
        arm_input_guard();
    }
    Ok(unlocked)
}

/// Locks the screen immediately. Safe to call when already locked.
pub fn relock() {
    unsafe {
        let login = libc::dlopen(
            c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login".as_ptr(),
            libc::RTLD_LAZY,
        );
        if !login.is_null() {
            let p = libc::dlsym(login, c"SACLockScreenImmediate".as_ptr());
            if !p.is_null() {
                let f: extern "C" fn() -> i32 = std::mem::transmute(p);
                f();
            }
        }
    }
}

// ---- input guard: an event tap that relocks on real local input ----

/// Whether a guard is currently running, so a second unlock doesn't start
/// another.
static GUARDING: AtomicBool = AtomicBool::new(false);
/// The guard thread's run loop, stored so the callback can stop it.
static GUARD_RUNLOOP: AtomicUsize = AtomicUsize::new(0);
/// Unix-millis after which the guard reacts to input. A short grace keeps the
/// unlock's own residual events from relocking immediately.
static GUARD_ARMED_AT: AtomicUsize = AtomicUsize::new(0);

fn now_millis() -> usize {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize)
        .unwrap_or(0)
}

type TapCreate = unsafe extern "C" fn(
    u32,
    u32,
    u32,
    u64,
    extern "C" fn(*const c_void, u32, *const c_void, *mut c_void) -> *const c_void,
    *mut c_void,
) -> *const c_void;
type PortSource = unsafe extern "C" fn(*const c_void, *const c_void, isize) -> *const c_void;
type RunLoopGet = unsafe extern "C" fn() -> *const c_void;
type RunLoopAdd = unsafe extern "C" fn(*const c_void, *const c_void, *const c_void);
type TapEnable = unsafe extern "C" fn(*const c_void, bool);
type RunLoopRun = unsafe extern "C" fn();
type RunLoopStop = unsafe extern "C" fn(*const c_void);
type PortInvalidate = unsafe extern "C" fn(*const c_void);

/// Suppresses the triggering event and relocks: a genuine local touch while
/// the agent holds the machine unlocked. A tap-disabled notification lands
/// here too, and relocks as well, since the guard can no longer protect.
extern "C" fn guard_callback(
    _proxy: *const c_void,
    etype: u32,
    event: *const c_void,
    _user: *mut c_void,
) -> *const c_void {
    // Grace window: let the unlock's own residual events pass through.
    if now_millis() < GUARD_ARMED_AT.load(Ordering::SeqCst) {
        return event;
    }
    if debug() {
        eprintln!("[ocu-lock] guard saw local input (event type {etype}) -> relock");
    }
    if GUARDING.swap(false, Ordering::SeqCst) {
        relock();
        unsafe {
            let cf = libc::dlopen(
                c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(),
                libc::RTLD_LAZY,
            );
            if let Some(stop) = sym::<RunLoopStop>(cf, c"CFRunLoopStop") {
                let rl = GUARD_RUNLOOP.load(Ordering::SeqCst) as *const c_void;
                if !rl.is_null() {
                    stop(rl);
                }
            }
        }
    }
    // Swallow the event so it never reaches the now-unlocked apps.
    std::ptr::null()
}

/// Starts the guard on its own run-loop thread. No-op if one is running.
fn arm_input_guard() {
    if GUARDING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| unsafe {
        let cg = libc::dlopen(
            c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics".as_ptr(),
            libc::RTLD_LAZY,
        );
        let cf = libc::dlopen(
            c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(),
            libc::RTLD_LAZY,
        );
        let create = sym::<TapCreate>(cg, c"CGEventTapCreate");
        let source = sym::<PortSource>(cf, c"CFMachPortCreateRunLoopSource");
        let get = sym::<RunLoopGet>(cf, c"CFRunLoopGetCurrent");
        let add = sym::<RunLoopAdd>(cf, c"CFRunLoopAddSource");
        let enable = sym::<TapEnable>(cg, c"CGEventTapEnable");
        let run = sym::<RunLoopRun>(cf, c"CFRunLoopRun");
        let invalidate = sym::<PortInvalidate>(cf, c"CFMachPortInvalidate");
        let release = sym::<Release>(cf, c"CFRelease");
        let (
            Some(create),
            Some(source),
            Some(get),
            Some(add),
            Some(enable),
            Some(run),
            Some(invalidate),
            Some(release),
        ) = (create, source, get, add, enable, run, invalidate, release)
        else {
            GUARDING.store(false, Ordering::SeqCst);
            return;
        };
        // Deliberate input only: key down, modifier changes, button presses.
        // (Mouse-moved is left out so cursor drift doesn't relock.)
        let mask: u64 = (1 << 10) | (1 << 12) | (1 << 1) | (1 << 3) | (1 << 25);
        // Set before the tap exists, so its first event already sees the grace
        // and the unlock's own residual events don't trip an immediate relock.
        GUARD_ARMED_AT.store(now_millis() + 2500, Ordering::SeqCst);
        // kCGHIDEventTap=0, kCGHeadInsertEventTap=0, default option (suppress)=0.
        let tap = create(0, 0, 0, mask, guard_callback, std::ptr::null_mut());
        if tap.is_null() {
            log::warn!("could not create the input guard event tap (needs Accessibility)");
            GUARDING.store(false, Ordering::SeqCst);
            return;
        }
        let src = source(std::ptr::null(), tap, 0);
        if src.is_null() {
            invalidate(tap);
            release(tap);
            GUARDING.store(false, Ordering::SeqCst);
            return;
        }
        let rl = get();
        GUARD_RUNLOOP.store(rl as usize, Ordering::SeqCst);
        let common = libc::dlsym(cf, c"kCFRunLoopCommonModes".as_ptr());
        let mode = if common.is_null() {
            std::ptr::null()
        } else {
            *(common as *const *const c_void)
        };
        add(rl, src, mode);
        enable(tap, true);
        run();
        // An enabled tap nobody services stalls system input until it times
        // out, so take it down with the run loop.
        enable(tap, false);
        invalidate(tap);
        release(src);
        release(tap);
        GUARD_RUNLOOP.store(0, Ordering::SeqCst);
        GUARDING.store(false, Ordering::SeqCst);
    });
}

// ---- peer verification (responder side; runs as the user) ----

type AuditToken = [u32; 8];
type SecTaskCreate = unsafe extern "C" fn(*const c_void, AuditToken) -> *const c_void;
type SecTaskCopyId = unsafe extern "C" fn(*const c_void, *mut *const c_void) -> *const c_void;
type CfStringGetCString = unsafe extern "C" fn(*const c_void, *mut i8, isize, u32) -> u8;

/// Whether a signing identifier names a process the plugin runs inside while
/// evaluating the unlock. On the locked lock screen that is
/// `com.apple.SecurityAgentHelper.<arch>`; elsewhere `com.apple.authorizationhost`.
fn is_agent_identifier(id: &str) -> bool {
    id == "com.apple.authorizationhost" || id.starts_with("com.apple.SecurityAgent")
}

/// Confirms the connected peer is an Apple authorization agent. Anyone can
/// sign a binary with any identifier, so the peer must also run as root or
/// `_securityagent`, which an ordinary user cannot fake.
fn peer_is_apple_agent(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    unsafe {
        let mut token: AuditToken = [0; 8];
        let mut len = std::mem::size_of::<AuditToken>() as libc::socklen_t;
        // LOCAL_PEERTOKEN = 0x006, SOL_LOCAL = 0.
        if libc::getsockopt(fd, 0, 0x006, token.as_mut_ptr() as *mut c_void, &mut len) != 0 {
            return false;
        }
        // audit_token_t's second word is the effective uid.
        let euid = token[1];
        if euid != 0 && euid != SECURITYAGENT_UID {
            if debug() {
                eprintln!("[ocu-lock] peer euid {euid} is not root or _securityagent");
            }
            return false;
        }
        let sec = libc::dlopen(
            c"/System/Library/Frameworks/Security.framework/Security".as_ptr(),
            libc::RTLD_LAZY,
        );
        let cf = libc::dlopen(
            c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(),
            libc::RTLD_LAZY,
        );
        let (Some(create), Some(copy_id), Some(get_cstr), Some(release)): (
            Option<SecTaskCreate>,
            Option<SecTaskCopyId>,
            Option<CfStringGetCString>,
            Option<Release>,
        ) = (
            sym(sec, c"SecTaskCreateWithAuditToken"),
            sym(sec, c"SecTaskCopySigningIdentifier"),
            sym(cf, c"CFStringGetCString"),
            sym(cf, c"CFRelease"),
        ) else {
            return false;
        };
        let task = create(std::ptr::null(), token);
        if task.is_null() {
            if debug() {
                eprintln!("[ocu-lock] SecTaskCreateWithAuditToken returned null");
            }
            return false;
        }
        let sid = copy_id(task, std::ptr::null_mut());
        let mut ok = false;
        if !sid.is_null() {
            let mut buf = [0i8; 256];
            if get_cstr(sid, buf.as_mut_ptr(), buf.len() as isize, 0x0800_0100) != 0 {
                let got = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy();
                if debug() {
                    eprintln!("[ocu-lock] peer signing id = {got:?}");
                }
                ok = is_agent_identifier(&got);
            } else if debug() {
                eprintln!("[ocu-lock] CFStringGetCString failed");
            }
            release(sid);
        } else if debug() {
            eprintln!("[ocu-lock] SecTaskCopySigningIdentifier returned null");
        }
        release(task);
        ok
    }
}

// ---- privileged install / uninstall (run as root) ----

fn set_mode(path: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {path}"))
}

fn chown(path: &str, uid: u32) -> Result<()> {
    if unsafe { libc::chown(CString::new(path)?.as_ptr(), uid, 0) } != 0 {
        bail!("chown {path}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// The authdb backup written on install, so uninstall can restore it.
fn backup_path() -> PathBuf {
    Path::new(SUPPORT_DIR).join("screensaver-rule.backup.plist")
}

/// Installs the plugin, socket directory and authorization rule. Must run as
/// root; `owner_uid` is the user the app runs as (who owns the socket dir).
pub fn install(plugin_src: &Path, owner_uid: u32) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("install-lock must run as root (use sudo)");
    }
    // Read the current rule first: our branch is only safe ahead of a working
    // fallback, so refuse a rule we don't understand rather than replace it.
    let existing: Vec<String> = authdb_rule_entries(RIGHT)?
        .into_iter()
        .filter(|e| e != SUBRULE)
        .collect();
    if existing.is_empty() {
        bail!("{RIGHT} is not a rule list this installer understands; leaving it unchanged");
    }

    // 1. Socket directory, owned by the user so the app can bind there. Its
    // parent holds the backup uninstall restores as root, so root owns it.
    std::fs::create_dir_all(SOCKET_DIR)?;
    chown(SUPPORT_DIR, 0)?;
    set_mode(SUPPORT_DIR, 0o755)?;
    chown(SOCKET_DIR, owner_uid)?;
    set_mode(SOCKET_DIR, 0o755)?;

    // 2. Plugin bundle into the system plugin directory. It comes from a
    // user-writable place and will run inside the login stack, so check the
    // installed copy's signature and remove it if it isn't ours.
    let dest = Path::new(PLUGIN_DEST);
    let _ = std::fs::remove_dir_all(dest);
    ditto(plugin_src, dest)?;
    // ditto keeps the source's owner; a user-owned plugin could be rewritten
    // by anything running as that user.
    let owned = std::process::Command::new("/usr/sbin/chown")
        .args(["-R", "root:wheel"])
        .arg(dest)
        .status()?
        .success();
    if !owned {
        let _ = std::fs::remove_dir_all(dest);
        bail!("chown {PLUGIN_DEST}");
    }
    let verified = std::process::Command::new("/usr/bin/codesign")
        // `-R=` takes the requirement inline; bare `-R` reads a file.
        .args(["--verify", "--strict", &format!("-R={PLUGIN_REQUIREMENT}")])
        .arg(dest)
        .status()?
        .success();
    if !verified {
        let _ = std::fs::remove_dir_all(dest);
        bail!("{} is not signed by OpenComputerUse", plugin_src.display());
    }

    // 3. Authorization rule: back up, then make our plugin the first branch.
    let backup = backup_path();
    if !backup.exists() {
        std::fs::write(&backup, authdb_read(RIGHT)?)?;
    }
    authdb_write(SUBRULE, SUBRULE_PLIST)?;
    let mut entries = vec![SUBRULE.to_string()];
    entries.extend(existing);
    authdb_write(RIGHT, &screensaver_plist(&entries))?;
    Ok(())
}

/// Reverses [`install`], restoring the backed-up rule.
pub fn uninstall() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("uninstall-lock must run as root (use sudo)");
    }
    let backup = backup_path();
    if backup.exists() {
        authdb_write(RIGHT, &std::fs::read_to_string(&backup)?)?;
        let _ = std::fs::remove_file(&backup);
    } else {
        // No backup: at least drop our branch from the rule.
        let entries: Vec<String> = authdb_rule_entries(RIGHT)?
            .into_iter()
            .filter(|e| e != SUBRULE)
            .collect();
        if !entries.is_empty() {
            authdb_write(RIGHT, &screensaver_plist(&entries))?;
        }
    }
    let _ = std::process::Command::new("/usr/bin/security")
        .args(["authorizationdb", "remove", SUBRULE])
        .output();
    let _ = std::fs::remove_dir_all(PLUGIN_DEST);
    let _ = std::fs::remove_file(SOCKET_PATH);
    let _ = std::fs::remove_dir(SOCKET_DIR);
    Ok(())
}

fn ditto(src: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg(src)
        .arg(dest)
        .status()?;
    if !status.success() {
        bail!("ditto {} -> {} failed", src.display(), dest.display());
    }
    Ok(())
}

fn authdb_read(right: &str) -> Result<String> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["authorizationdb", "read", right])
        .output()?;
    if !out.status.success() {
        bail!("reading authorization rule {right}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn authdb_write(right: &str, plist: &str) -> Result<()> {
    use std::process::Stdio;
    let mut child = std::process::Command::new("/usr/bin/security")
        .args(["authorizationdb", "write", right])
        .stdin(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(plist.as_bytes())?;
    if !child.wait()?.success() {
        bail!("writing authorization rule {right}");
    }
    Ok(())
}

/// The entries of a `rule`-class right's `rule` array, in order.
fn authdb_rule_entries(right: &str) -> Result<Vec<String>> {
    let plist = authdb_read(right)?;
    // The entries are <string> items inside the <array> under the "rule" key.
    let mut entries = Vec::new();
    if let Some(start) = plist.find("<key>rule</key>") {
        if let Some(arr_start) = plist[start..].find("<array>") {
            let from = start + arr_start;
            if let Some(arr_end) = plist[from..].find("</array>") {
                let body = &plist[from..from + arr_end];
                for piece in body.split("<string>").skip(1) {
                    if let Some(end) = piece.find("</string>") {
                        entries.push(piece[..end].to_string());
                    }
                }
            }
        }
    }
    Ok(entries)
}

const SUBRULE_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>class</key><string>evaluate-mechanisms</string>
  <key>comment</key><string>OpenComputerUse background screen-unlock branch.</string>
  <key>mechanisms</key><array><string>OcuLockAuthorizationPlugin:allow</string></array>
  <key>tries</key><integer>1</integer>
  <key>version</key><integer>1</integer>
</dict></plist>"#;

fn screensaver_plist(entries: &[String]) -> String {
    let items: String = entries
        .iter()
        .map(|e| format!("    <string>{e}</string>\n"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>class</key><string>rule</string>
  <key>comment</key><string>OpenComputerUse unlock branch first; falls through to the normal unlock.</string>
  <key>k-of-n</key><integer>1</integer>
  <key>rule</key><array>
{items}  </array>
  <key>version</key><integer>1</integer>
</dict></plist>"#
    )
}
