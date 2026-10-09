//! Starting a session's process inside a Job Object that kills everything in
//! it when its last handle closes, which is when the session ends or the
//! MCP server dies, however it dies.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;

use anyhow::{bail, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess,
    QueryFullProcessImageNameW, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, SW_SHOWNORMAL};

use ocu_core::LaunchSpec;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Quotes one argument the way `CommandLineToArgvW` reads it back.
fn quote(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// Resolves a bare name ("notepad") the way the shell would.
fn resolve(app: &str) -> String {
    let p = std::path::Path::new(app);
    if p.is_absolute() || app.contains(['\\', '/']) {
        return app.to_string();
    }
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".EXE;.COM;.BAT;.CMD".into())
        .split(';')
        .map(|e| e.to_lowercase())
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let direct = dir.join(app);
            if direct.is_file() {
                return direct.display().to_string();
            }
            for ext in &exts {
                let c = dir.join(format!("{app}{ext}"));
                if c.is_file() {
                    return c.display().to_string();
                }
            }
        }
    }
    app.to_string()
}

pub struct Job {
    pub handle: HANDLE,
    pub pid: u32,
    process: HANDLE,
    /// Whether the panel hook was queued into the app before it started.
    pub hooked: bool,
}

unsafe impl Send for Job {}

impl Job {
    pub fn spawn(spec: &LaunchSpec) -> Result<Self> {
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null())?;
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;

            let exe = resolve(&spec.app);
            let mut cmdline = String::new();
            quote(&exe, &mut cmdline);
            for a in &spec.args {
                cmdline.push(' ');
                quote(a, &mut cmdline);
            }
            let mut cmdline = wide(OsStr::new(&cmdline));

            // Settled before the environment block is built, because the
            // hook's pipe name has to reach the app through it.
            let hook = Self::plan_hook(spec);

            let mut env = spec.env.clone();
            if let Some((_, pipe)) = &hook {
                // The hook clears this again as it loads, so nothing the app
                // starts inherits it.
                env.insert(crate::hook::PIPE_VAR.into(), (*pipe).to_string());
            }
            let env_block: Option<Vec<u16>> = (!env.is_empty()).then(|| {
                let mut vars: std::collections::BTreeMap<String, String> =
                    std::env::vars().collect();
                vars.extend(env);
                let mut block = Vec::new();
                for (k, v) in vars {
                    block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
                    block.push(0);
                }
                block.push(0);
                block
            });
            let cwd = spec.cwd.as_ref().map(|c| wide(OsStr::new(c)));

            let si = STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                dwFlags: STARTF_USESHOWWINDOW,
                // Show, but do not take the foreground unless asked to.
                wShowWindow: if spec.foreground {
                    SW_SHOWNORMAL.0 as u16
                } else {
                    SW_SHOWNOACTIVATE.0 as u16
                },
                ..Default::default()
            };
            let mut pi = PROCESS_INFORMATION::default();
            let created = CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NEW_PROCESS_GROUP,
                env_block.as_ref().map(|b| b.as_ptr() as *const _),
                cwd.as_ref()
                    .map(|c| PCWSTR(c.as_ptr()))
                    .unwrap_or(PCWSTR::null()),
                &si,
                &mut pi,
            );
            if let Err(e) = created {
                let _ = CloseHandle(job);
                bail!("starting {exe}: {e}");
            }
            // In the job before it runs a single instruction, so nothing it
            // starts can slip out.
            AssignProcessToJobObject(job, pi.hProcess)?;
            // And the hook in before it runs any at all.
            let hooked = match hook {
                // An app without the hook is one whose dialogs the UI
                // Automation path answers instead, so a failure here is never
                // a reason not to start it.
                Some((dll, _)) => crate::inject::queue_load(&dll, pi.hProcess, pi.hThread)
                    .inspect_err(|e| log::warn!("no panel hook in {exe}: {e}"))
                    .is_ok(),
                None => false,
            };
            ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
            Ok(Self {
                handle: job,
                pid: pi.dwProcessId,
                process: pi.hProcess,
                hooked,
            })
        }
    }

    /// The hook to load into a new app, and the pipe it will answer on, if
    /// the setting is on and the hook is there to load.
    ///
    /// Anything missing is a `None` and a warning rather than a failure:
    /// dialogs are answered over UI Automation whether or not the hook
    /// arrives, so the app starts either way.
    fn plan_hook(spec: &LaunchSpec) -> Option<(std::path::PathBuf, &'static str)> {
        // An app that was already running cannot be given a hook, and one
        // being watched through `active_window` never can.
        if !spec.active_window && crate::hook::wanted() {
            match (crate::hook::dll(), crate::hook::pipe()) {
                (Some(dll), Some(pipe)) if crate::hook::reachable(pipe) => {
                    return Some((dll, pipe))
                }
                (None, _) => log::warn!(
                    "the panel hook is on but {} was not found beside this executable",
                    crate::hook::DLL
                ),
                (_, None) => log::warn!("the panel hook could not open its pipe"),
                _ => log::warn!("nothing is listening on the panel hook's pipe"),
            }
        }
        None
    }

    /// Every process in the job: the app and whatever it started.
    pub fn pids(&self) -> Vec<u32> {
        #[repr(C)]
        struct List {
            head: JOBOBJECT_BASIC_PROCESS_ID_LIST,
            more: [usize; 255],
        }
        let mut list: List = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                Some(self.handle),
                JobObjectBasicProcessIdList,
                &mut list as *mut _ as *mut _,
                std::mem::size_of::<List>() as u32,
                None,
            )
        };
        if ok.is_err() {
            return vec![self.pid];
        }
        let n = list.head.NumberOfProcessIdsInList as usize;
        let ids =
            unsafe { std::slice::from_raw_parts(list.head.ProcessIdList.as_ptr(), n.min(256)) };
        ids.iter().map(|&p| p as u32).collect()
    }

    pub fn alive(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code) }.is_ok() && code == 259 // STILL_ACTIVE
            || !self.pids().is_empty()
    }

    pub fn kill(&mut self) {
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
            let _ = CloseHandle(self.process);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A process the session did not start (the app of a window it attached
/// to): watched, never ended.
pub struct Process {
    pub pid: u32,
    handle: HANDLE,
}

unsafe impl Send for Process {}

impl Process {
    pub fn open(pid: u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }?;
        Ok(Self { pid, handle })
    }

    /// The executable's name without its extension: "notepad".
    pub fn name(&self) -> String {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = unsafe {
            QueryFullProcessImageNameW(
                self.handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        if ok.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn alive(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.handle, &mut code) }.is_ok() && code == 259
        // STILL_ACTIVE
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// This process and its ancestors: the client that started the server and
/// the apps it runs inside (a terminal, an editor).
pub fn ancestors() -> Vec<u32> {
    let mut parents = std::collections::HashMap::new();
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snap, &mut entry).is_ok();
            while more {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                more = Process32NextW(snap, &mut entry).is_ok();
            }
            let _ = CloseHandle(snap);
        }
    }
    let mut out = Vec::new();
    let mut pid = unsafe { GetCurrentProcessId() };
    // Parent ids can be stale and reused, so stop at a repeat.
    while pid != 0 && !out.contains(&pid) && out.len() < 64 {
        out.push(pid);
        pid = parents.get(&pid).copied().unwrap_or(0);
    }
    out
}
