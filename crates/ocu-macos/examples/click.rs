//! Clicks TextEdit's bold checkbox in the background with raw events, then
//! with AXPress, reading its value after each.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{Action, LaunchSpec, MouseButton, Platform, TreeOptions, UiNode};
    fn find<'a>(n: &'a UiNode, desc: &str) -> Option<&'a UiNode> {
        if n.description.as_deref() == Some(desc) {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, desc))
    }
    let p = ocu_macos::MacPlatform;
    let mut s = p.launch(&LaunchSpec {
        app: "TextEdit".into(),
        new_instance: true,
        ..Default::default()
    })?;
    let tree = s.ui_tree(None, &TreeOptions::default())?;
    let bold = find(&tree, "align centre").unwrap().clone();
    println!("before: {:?}", bold.value);
    let (x, y) = bold.frame.unwrap().center();
    s.perform(
        None,
        &Action::Click {
            x,
            y,
            button: MouseButton::Left,
            count: 1,
            modifiers: None,
        },
    )?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let tree = s.ui_tree(None, &TreeOptions::default())?;
    let bold = find(&tree, "align centre").unwrap().clone();
    println!("after event click: {:?}", bold.value);
    let right = find(&tree, "align right").unwrap().clone();
    s.perform(
        None,
        &Action::ElementAction {
            element: right.id.clone(),
            name: None,
        },
    )?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let tree = s.ui_tree(None, &TreeOptions::default())?;
    println!(
        "after AXPress on right: right={:?} centre={:?}",
        find(&tree, "align right").unwrap().value,
        find(&tree, "align centre").unwrap().value
    );
    s.close();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
