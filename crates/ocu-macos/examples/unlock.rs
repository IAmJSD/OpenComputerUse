//! Manual test for lock-screen unlock. Locks the screen, waits, asks the
//! backend to unlock it, and reports whether it worked. Must run signed
//! (Developer ID, identifier com.infrawrench.opencomputeruse) in the user's
//! GUI session, after `install-lock`.
//!
//!   cargo build --example unlock -p ocu-macos
//!   codesign -f -i com.infrawrench.opencomputeruse -s "Developer ID Application: ..." \
//!       target/debug/examples/unlock
//!   launchctl asuser $UID target/debug/examples/unlock [unlock] [watch]
//!
//! `unlock` skips the self-lock (lock the Mac yourself first); `watch` stays
//! alive with the input guard armed, to see a local touch relock.

use std::time::Duration;

use ocu_macos::lock;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("locked before: {}", lock::screen_is_locked());
    if !args.iter().any(|a| a == "unlock") {
        lock::relock();
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("locked now: {}", lock::screen_is_locked());
    match lock::unlock(Duration::from_secs(20)) {
        Ok(ok) => println!("unlock() -> {ok}"),
        Err(e) => println!("unlock() error: {e:#}"),
    }
    println!("locked after: {}", lock::screen_is_locked());
    if args.iter().any(|a| a == "watch") {
        for _ in 0..8 {
            std::thread::sleep(Duration::from_secs(2));
            println!("  locked: {}", lock::screen_is_locked());
        }
    }
}
