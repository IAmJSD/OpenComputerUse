//! Starting a session's process inside a Job Object that kills everything in
//! it when its last handle closes, which is when the session ends or the
//! MCP server dies, however it dies.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;

use anyhow::{bail, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

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
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
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

            let env_block: Option<Vec<u16>> = (!spec.env.is_empty()).then(|| {
                let mut vars: std::collections::BTreeMap<String, String> =
                    std::env::vars().collect();
                vars.extend(spec.env.clone());
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
                // Show, but do not take the foreground.
                wShowWindow: SW_SHOWNOACTIVATE.0 as u16,
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
            ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
            Ok(Self {
                handle: job,
                pid: pi.dwProcessId,
                process: pi.hProcess,
            })
        }
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
            || self.pids().len() > 0
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
