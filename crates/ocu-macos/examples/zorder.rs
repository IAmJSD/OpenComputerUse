//! Launches an app and reports whether its window ended up above the
//! frontmost app's windows.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{LaunchSpec, Platform};
    use std::process::Command;
    let front = || {
        String::from_utf8(Command::new("osascript").args(["-e", "tell application \"System Events\" to get unix id of first process whose frontmost is true"]).output().unwrap().stdout).unwrap().trim().parse::<i64>().unwrap()
    };
    let before = front();
    let p = ocu_macos::MacPlatform;
    let mut s = p.launch(&LaunchSpec { app: std::env::args().nth(1).unwrap_or("TextEdit".into()), new_instance: true, ..Default::default() })?;
    let pid = s.describe().pid.unwrap() as i64;
    let out = Command::new("python3").args(["-c", &format!(r#"
import Quartz
ws = Quartz.CGWindowListCopyWindowInfo(Quartz.kCGWindowListOptionOnScreenOnly | Quartz.kCGWindowListExcludeDesktopElements, 0)
order = [w['kCGWindowOwnerPID'] for w in ws if w['kCGWindowLayer'] == 0]
print('session index', order.index({pid}) if {pid} in order else None, 'user app index', order.index({before}) if {before} in order else None)
"#)]).output()?;
    print!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("front before {before}, after {}", front());
    s.close();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
