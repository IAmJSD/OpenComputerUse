//! Picks options of a <select> in background Firefox with set_value, which
//! a browser in the background never opens a menu for:
//! `cargo run -p ocu-macos --example select` (APP="Google Chrome" for Chrome).
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ocu_core::{Action, LaunchSpec, Platform, TreeOptions, UiNode};
    const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>ready</title>
<select id=s style="font-size:20px;margin:40px"><option>Add a railcard</option><option>16-25 Railcard</option><option>26-30 Railcard</option><option>Senior Railcard</option></select>
<script>let k='';document.addEventListener('keydown',e=>{k+=e.key;document.title=k+'|'+document.getElementById('s').value},true);
document.addEventListener('mousedown',e=>{k+='[md]';document.title=k},true);
document.getElementById('s').addEventListener('focus',()=>{k+='[f]';document.title=k});
document.getElementById('s').addEventListener('change',e=>document.title=k+'|changed:'+e.target.value)</script>"#;
    fn find<'a>(n: &'a UiNode, role: &str) -> Option<&'a UiNode> {
        if n.role == role {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, role))
    }
    let page = std::env::temp_dir().join("ocu-select-test.html");
    std::fs::write(&page, PAGE)?;
    let p = ocu_macos::MacPlatform;
    // With ATTACH set, drives a Firefox already showing the page (opened
    // with `open -g`), so repeated runs launch nothing.
    let attach = std::env::var_os("ATTACH").is_some();
    let app = std::env::var("APP").unwrap_or_else(|_| "Firefox".into());
    let url = format!("file://{}", page.display());
    // Chrome gets an instance and profile of its own, apart from the user's.
    let chrome = app.contains("Chrome");
    let args = match (attach, chrome) {
        (true, _) => vec![],
        (false, true) => vec![
            format!(
                "--user-data-dir={}",
                std::env::temp_dir().join("ocu-select-chrome").display()
            ),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            url,
        ],
        (false, false) => vec![url],
    };
    let mut s = p.launch(&LaunchSpec {
        app,
        args,
        new_instance: chrome && !attach,
        ..Default::default()
    })?;
    if !attach {
        std::thread::sleep(std::time::Duration::from_secs(4));
    }
    let title = |s: &mut Box<dyn ocu_core::Session>| {
        s.windows()
            .ok()
            .and_then(|w| w.first().map(|w| w.title.clone()))
            .unwrap_or_default()
    };
    // Firefox may relaunch itself and load the page late: wait for it.
    for _ in 0..20 {
        let tree = s.ui_tree(None, &TreeOptions::default())?;
        if find(&tree, "PopUpButton").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    for want in [
        "Senior Railcard",
        "16-25 Railcard",
        "26-30 railcard",
        "Gold Card",
    ] {
        let tree = s.ui_tree(None, &TreeOptions::default())?;
        let Some(el) = find(&tree, "WebArea").and_then(|web| find(web, "PopUpButton")) else {
            println!("no PopUpButton in the page's tree");
            break;
        };
        let result = s.perform(
            None,
            &Action::SetValue {
                element: el.id.clone(),
                value: want.into(),
            },
        );
        std::thread::sleep(std::time::Duration::from_millis(300));
        println!(
            "set {want:?}: {:?}, title now {:?}",
            result.map_err(|e| e.to_string()),
            title(&mut s)
        );
    }
    s.close();
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {}
