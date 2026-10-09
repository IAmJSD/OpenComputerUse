//! Paths a caller may have written loosely, made into the real thing.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};

/// The user's home directory, under whichever name the platform gives it.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// How to write an absolute path here, for the error message.
fn hint() -> &'static str {
    if cfg!(windows) {
        "start it with C:\\ or ~"
    } else {
        "start it with / or ~"
    }
}

/// Expands `~` and insists on an absolute path.
///
/// Every backend answers file dialogs, and all of them want a real path,
/// so this is checked in one place rather than in each.
pub fn absolute(path: &str) -> Result<PathBuf> {
    let p = match path
        .strip_prefix("~/")
        .or(path.strip_prefix('~').filter(|r| r.is_empty()))
    {
        Some(rest) => Path::new(&home().ok_or_else(|| anyhow!("HOME is not set"))?).join(rest),
        None => PathBuf::from(path),
    };
    if !p.is_absolute() {
        bail!("{path:?} is not an absolute path ({})", hint());
    }
    Ok(p)
}

/// Checks an answer to a file dialog: a save takes one path in a folder that
/// exists, an open takes files that exist, one unless it said `multiple`.
/// No paths is a cancel, which is always fine.
pub fn check_answer(paths: &[PathBuf], save: bool, multiple: bool) -> Result<()> {
    if save {
        return match paths {
            [] => Ok(()),
            [p] if p.parent().is_some_and(Path::is_dir) => Ok(()),
            [p] => bail!("there is no folder to save {} in", p.display()),
            _ => bail!("a save dialog saves to one path"),
        };
    }
    if paths.len() > 1 && !multiple {
        bail!("the app asks for one file, not {}", paths.len());
    }
    for p in paths {
        if !p.exists() {
            bail!("there is no {}", p.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_a_leading_tilde() {
        let home = home().expect("a home directory");
        assert_eq!(absolute("~/Documents").unwrap(), home.join("Documents"));
        assert_eq!(absolute("~").unwrap(), home);
    }

    #[test]
    fn leaves_a_real_path_alone() {
        let p = if cfg!(windows) {
            r"C:\a\b.txt"
        } else {
            "/a/b.txt"
        };
        assert_eq!(absolute(p).unwrap(), PathBuf::from(p));
    }

    #[test]
    fn refuses_a_relative_path() {
        let err = absolute("Documents").unwrap_err().to_string();
        assert!(err.contains("not an absolute path"), "{err}");
    }
}
