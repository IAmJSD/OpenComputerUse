//! Clicks a test page in background Firefox, whose title logs the mouse
//! events it gets, and reads its web tree:
//! `cargo run -p ocu-macos --example firefox`.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{Action, LaunchSpec, MouseButton, Platform, TreeOptions, UiNode};
    const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>ready</title>
<style>body{margin:0;background:#fff}button{position:absolute;left:100px;top:100px;width:300px;height:120px;font-size:30px}input{position:absolute;left:100px;top:300px;width:300px;height:40px}</style>
<button id=b>Press me</button><input id=i placeholder="field">
<script>
let log=[];function t(s){log.push(s);document.title=log.slice(-4).join(' ')}
for (const ev of ['mousedown','mouseup','click','pointerdown']) document.addEventListener(ev,e=>t(ev[0]+ev[5]+':'+e.clientX+','+e.clientY+(e.pointerType?'/'+e.pointerType:'')),true);
document.getElementById('i').addEventListener('focus',()=>t('FOCUS'));
</script>"#;
    fn count(n: &UiNode) -> usize {
        1 + n.children.iter().map(count).sum::<usize>()
    }
    fn find<'a>(n: &'a UiNode, role: &str) -> Option<&'a UiNode> {
        if n.role == role {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, role))
    }
    let page = std::env::temp_dir().join("ocu-firefox-test.html");
    std::fs::write(&page, PAGE)?;
    let p = ocu_macos::MacPlatform;
    let mut s = p.launch(&LaunchSpec {
        app: "Firefox".into(),
        args: vec![format!("file://{}", page.display())],
        ..Default::default()
    })?;
    std::thread::sleep(std::time::Duration::from_secs(4));
    let tree = s.ui_tree(None, &TreeOptions::default())?;
    println!("tree: {} nodes", count(&tree));
    let title = |s: &mut Box<dyn ocu_core::Session>| {
        s.windows()
            .ok()
            .and_then(|w| w.first().map(|w| w.title.clone()))
            .unwrap_or_default()
    };
    for role in ["Button", "TextField"] {
        // The window's own close button comes after the page in the tree.
        let Some(el) = find(&tree, "WebArea").and_then(|web| find(web, role)) else {
            println!("no {role} in the page's tree");
            continue;
        };
        let (x, y) = el.frame.unwrap().center();
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
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("after clicking the {role}: {}", title(&mut s));
    }
    s.close();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
