//! An app's open or save dialog, answered through UI Automation by setting
//! the file name box and pressing the dialog's own button.
//!
//! The shell draws both `IFileDialog` and `GetOpenFileNameW` with the same
//! automation ids, so nothing here is per app. Controls are found by id,
//! then by their English names.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;

use ocu_core::WindowInfo;

use crate::capture::hwnd;
use crate::uia::Uia;

/// The file name box: the same automation id in the modern dialog and the
/// older one.
const FILE_NAME_ID: &str = "1148";
/// `IDOK`, the Open or Save button.
const ACCEPT_ID: &str = "1";
/// `IDCANCEL`.
const CANCEL_ID: &str = "2";

/// What the shell's buttons are called, save dialogs and open ones apart.
const SAVE_NAMES: [&str; 2] = ["Save", "Save As"];
const OPEN_NAMES: [&str; 1] = ["Open"];

/// A file dialog that is up, with the controls that answer it.
pub struct Dialog {
    pub window: u64,
    /// A save dialog takes one path, where an open one may take several.
    pub save: bool,
    name: IUIAutomationElement,
    accept: Option<IUIAutomationElement>,
    cancel: Option<IUIAutomationElement>,
}

impl Dialog {
    /// What the app is waiting for and how to answer it, worded like the
    /// macOS backend's notice.
    pub fn notice(&self) -> String {
        format!(
            "{} (window {}): answer it with choose_file, giving the {}, or no paths to cancel.",
            if self.save {
                "A save dialog is showing"
            } else {
                "An open dialog is showing"
            },
            self.window,
            if self.save {
                "path to save to"
            } else {
                "paths to pick"
            },
        )
    }

    /// Types `paths` into the file name box and presses the dialog's own
    /// button. No paths cancels it instead.
    pub fn answer(&self, uia: &Uia, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return self.cancel(uia);
        }
        // An open dialog's own multi-select setting is not visible here, so
        // the dialog itself refuses a second file it does not take.
        ocu_core::paths::check_answer(paths, self.save, true)?;
        let text = if paths.len() == 1 {
            paths[0].to_string_lossy().into_owned()
        } else {
            // The shell's own way of naming several files in one go.
            paths
                .iter()
                .map(|p| format!("\"{}\"", p.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        uia.set_element_value(&self.name, &text, "the dialog's file name box")?;
        // The dialog reads the box as it closes, so it needs the button
        // rather than anything we could do to the text alone.
        let accept = self
            .accept
            .as_ref()
            .ok_or_else(|| anyhow!("the dialog has no Open or Save button to press"))?;
        uia.invoke(accept)
    }

    fn cancel(&self, uia: &Uia) -> Result<()> {
        let cancel = self
            .cancel
            .as_ref()
            .ok_or_else(|| anyhow!("the dialog has no Cancel button to press"))?;
        uia.invoke(cancel)
    }
}

/// The file dialog among a session's windows, if one is up. Searched from
/// the last window, since a dialog comes up after its owner.
pub fn locate(uia: &Uia, windows: &[WindowInfo]) -> Option<Dialog> {
    windows
        .iter()
        .rev()
        .filter(|w| is_dialog(w.id))
        .find_map(|w| open(uia, w.id))
}

/// Whether `window` has the dialog class (`#32770`) both file dialogs use.
/// Checked first so an app's own windows are never searched, which is slow
/// for a large tree and could match an app's own "File name:" box.
fn is_dialog(window: u64) -> bool {
    let mut class = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd(window), &mut class) };
    n > 0 && String::from_utf16_lossy(&class[..n as usize]) == "#32770"
}

fn open(uia: &Uia, window: u64) -> Option<Dialog> {
    let name = file_name(uia, window)?;
    let accept = accept(uia, window);
    // Which kind it is shows in the button: Open or Save.
    let save = accept
        .as_ref()
        .and_then(|a| unsafe { a.CurrentName() }.ok())
        .is_some_and(|n| SAVE_NAMES.contains(&n.to_string().as_str()));
    Some(Dialog {
        window,
        save,
        name,
        accept,
        cancel: uia
            .find_by_id(window, CANCEL_ID)
            .or_else(|| uia.find_by_type(window, UIA_ButtonControlTypeId, "Cancel")),
    })
}

/// The file name box, by automation id, then by its label. Never just any
/// edit: an app's own window has those too, and is no file dialog.
fn file_name(uia: &Uia, window: u64) -> Option<IUIAutomationElement> {
    // The edit, not the combo box around it with the same id: the dialog
    // reads the edit, and setting the combo box's value leaves it alone.
    uia.find_by_id_and_type(window, FILE_NAME_ID, UIA_EditControlTypeId)
        .or_else(|| uia.find_by_id(window, FILE_NAME_ID))
        .or_else(|| uia.find_by_type(window, UIA_EditControlTypeId, "File name:"))
}

/// The Open or Save button, by automation id, then by name.
fn accept(uia: &Uia, window: u64) -> Option<IUIAutomationElement> {
    uia.find_by_id(window, ACCEPT_ID).or_else(|| {
        SAVE_NAMES
            .iter()
            .chain(&OPEN_NAMES)
            .find_map(|n| uia.find_by_type(window, UIA_ButtonControlTypeId, n))
    })
}
