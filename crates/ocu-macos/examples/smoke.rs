//! Drives an app in the background: `cargo run -p ocu-macos --example smoke -- TextEdit`.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{Action, LaunchSpec, Platform, TreeOptions};
    let app = std::env::args().nth(1).unwrap_or_else(|| "TextEdit".into());
    let p = ocu_macos::MacPlatform;
    for perm in p.permissions() {
        println!("{}: {}", perm.name, perm.granted);
    }
    let mut s = p.launch(&LaunchSpec {
        app,
        new_instance: true,
        ..Default::default()
    })?;
    println!("{:?}", s.describe());
    println!("{:#?}", s.windows()?);
    let tree = s.ui_tree(None, &TreeOptions::default())?;
    println!("{}", tree.render());
    s.perform(
        None,
        &Action::TypeText {
            text: "Hello from the background!\n".into(),
        },
    )?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    let shot = s.screenshot(None)?;
    std::fs::write("/tmp/ocu-smoke.png", &shot.png)?;
    println!(
        "screenshot {}x{} -> /tmp/ocu-smoke.png",
        shot.width, shot.height
    );
    if std::env::args().nth(2).as_deref() != Some("keep") {
        s.close();
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
