//! Types, clicks at the start of the text, types again: did the click move
//! the insertion point?
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{Action, LaunchSpec, MouseButton, Platform, TreeOptions, UiNode};
    fn text(n: &UiNode) -> Option<(String, ocu_core::Rect)> {
        if n.role == "TextArea" {
            return Some((n.value.clone().unwrap_or_default(), n.frame.unwrap()));
        }
        n.children.iter().find_map(text)
    }
    let p = ocu_macos::MacPlatform;
    let mut s = p.launch(&LaunchSpec { app: "TextEdit".into(), new_instance: true, ..Default::default() })?;
    s.perform(None, &Action::PressKey { keys: "cmd+a".into() })?;
    s.perform(None, &Action::TypeText { text: "world".into() })?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (_, f) = text(&s.ui_tree(None, &TreeOptions::default())?).unwrap();
    s.perform(None, &Action::Click { x: f.x + 2.0, y: f.y + 6.0, button: MouseButton::Left, count: 1, modifiers: None })?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    s.perform(None, &Action::TypeText { text: "hello ".into() })?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("text: {:?}", text(&s.ui_tree(None, &TreeOptions::default())?).unwrap().0);
    std::fs::write("/tmp/ocu-click2.png", s.screenshot(None)?.png)?;
    s.close();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
