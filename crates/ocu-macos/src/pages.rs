//! A browser link that hands us its pages' file choosers, so they are
//! answered without a panel showing: Chromium's DevTools pipe
//! ([`crate::cdp`]) or Firefox's WebDriver BiDi ([`crate::bidi`]).

use std::path::PathBuf;

use anyhow::Result;

pub trait PageFiles: Send {
    /// Whether the link still works.
    fn is_alive(&self) -> bool;

    /// The file chooser a page is waiting on: `Some(multiple)`.
    fn waiting(&self) -> Option<bool>;

    /// Answers the waiting chooser with `paths`; none cancels it.
    fn answer(&self, paths: &[PathBuf]) -> Result<()>;
}

/// Checks paths for a chooser that takes `multiple` or one.
pub fn check(paths: &[PathBuf], multiple: bool) -> Result<()> {
    if paths.len() > 1 && !multiple {
        anyhow::bail!("the page asks for one file, not {}", paths.len());
    }
    for p in paths {
        if !p.exists() {
            anyhow::bail!("there is no {}", p.display());
        }
    }
    Ok(())
}
