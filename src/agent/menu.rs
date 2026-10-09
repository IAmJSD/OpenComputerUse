//! The app menu and the shortcuts it shows: ⌘Q quits (ending any sessions;
//! the next MCP request starts the app again), ⌘W closes the window.
//! Check for Updates is handled by the agent, which owns the window. Windows
//! and Linux have no app menu, only Ctrl+Q and Ctrl+W.

use gpui::{actions, App, KeyBinding};
#[cfg(target_os = "macos")]
use gpui::{Menu, MenuItem};

actions!(
    opencomputeruse,
    [
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        CloseWindow,
        CheckForUpdates
    ]
);

pub fn install(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
    });
    #[cfg(target_os = "macos")]
    {
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.set_menus(vec![
            Menu {
                name: "OpenComputerUse".into(),
                items: vec![
                    MenuItem::action("Check for Updates…", CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::action("Hide OpenComputerUse", Hide),
                    MenuItem::action("Hide Others", HideOthers),
                    MenuItem::action("Show All", ShowAll),
                    MenuItem::separator(),
                    MenuItem::action("Quit OpenComputerUse", Quit),
                ],
            },
            Menu {
                name: "Window".into(),
                items: vec![MenuItem::action("Close Window", CloseWindow)],
            },
        ]);
    }
}
