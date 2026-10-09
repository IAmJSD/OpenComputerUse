//! Embeds the panel hook when `OCU_PANEL_HOOK_DLL` is the absolute path of a
//! built one, so a single exe carries it. See `src/hook.rs`.

fn main() {
    println!("cargo::rerun-if-env-changed=OCU_PANEL_HOOK_DLL");
    println!("cargo::rustc-check-cfg=cfg(ocu_panel_hook)");
    let Some(dll) = std::env::var_os("OCU_PANEL_HOOK_DLL") else {
        return;
    };
    let dll = std::path::PathBuf::from(dll);
    assert!(dll.is_file(), "OCU_PANEL_HOOK_DLL: no {}", dll.display());
    println!("cargo::rerun-if-changed={}", dll.display());
    println!("cargo::rustc-env=OCU_PANEL_HOOK_DLL={}", dll.display());
    println!("cargo::rustc-cfg=ocu_panel_hook");
}
