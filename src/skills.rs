//! Putting the generic skill into the agent harnesses' skills folders (and
//! taking it out again): `<skills>/opencomputeruse-remote/SKILL.md`. Only
//! that one file is ever written or removed, and only when it is ours.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _, Result};

use crate::clients::{self, Client};
use crate::remote::skill::{render_generic, GENERIC_SKILL_NAME};

/// A harness's skills folder. Claude Desktop has none.
pub fn skills_dir(client: Client) -> Option<PathBuf> {
    clients::config_root(client).map(|root| root.join("skills"))
}

/// Whether to offer the skill buttons: the harness has a skills folder and
/// is on this computer (`found`: its CLI was found; or its config folder
/// exists).
pub fn available(client: Client, found: bool) -> bool {
    clients::config_root(client).is_some_and(|root| root.is_dir() || found)
}

fn skill_file(skills: &Path) -> PathBuf {
    skills.join(GENERIC_SKILL_NAME).join("SKILL.md")
}

/// A regular file (not a link) that starts as our skill does.
fn is_ours(file: &Path) -> bool {
    let regular = std::fs::symlink_metadata(file).is_ok_and(|m| m.is_file());
    regular
        && std::fs::read_to_string(file)
            .is_ok_and(|t| t.starts_with(&format!("---\nname: {GENERIC_SKILL_NAME}\n")))
}

/// Whether the skill is in this harness's skills folder.
pub fn installed(client: Client) -> bool {
    skills_dir(client).is_some_and(|dir| is_ours(&skill_file(&dir)))
}

fn refuse_link(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("{} is a symlink; not touching it", path.display());
    }
    Ok(())
}

fn add_in(skills: &Path, text: &str) -> Result<PathBuf> {
    let dir = skills.join(GENERIC_SKILL_NAME);
    let file = skill_file(skills);
    refuse_link(&dir)?;
    refuse_link(&file)?;
    if file.exists() && !is_ours(&file) {
        bail!("{} is a different skill; not overwriting it", file.display());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

fn remove_in(skills: &Path) -> Result<()> {
    let dir = skills.join(GENERIC_SKILL_NAME);
    let file = skill_file(skills);
    refuse_link(&dir)?;
    if !is_ours(&file) {
        bail!("{} holds no skill of ours to remove", dir.display());
    }
    std::fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
    // Fails, harmlessly, when the folder holds anything else.
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// Writes (or refreshes) the skill for a harness; returns where it went.
pub fn add(client: Client) -> Result<PathBuf> {
    let dir = skills_dir(client)
        .ok_or_else(|| anyhow!("{} has no skills folder", client.label()))?;
    add_in(&dir, &render_generic(&crate::tools::list()))
}

pub fn remove(client: Client) -> Result<()> {
    let dir = skills_dir(client)
        .ok_or_else(|| anyhow!("{} has no skills folder", client.label()))?;
    remove_in(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch skills folder, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("ocu-skills-test-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SKILL: &str = "---\nname: opencomputeruse-remote\ndescription: x\n---\nbody\n";

    #[test]
    fn add_writes_the_skill_and_is_repeatable() {
        let s = Scratch::new("add");
        let file = add_in(&s.0, SKILL).unwrap();
        assert_eq!(file, s.0.join("opencomputeruse-remote/SKILL.md"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), SKILL);
        let newer = format!("{SKILL}more\n");
        add_in(&s.0, &newer).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);
        assert!(is_ours(&file));
    }

    #[test]
    fn remove_deletes_our_file_and_an_empty_folder() {
        let s = Scratch::new("remove");
        add_in(&s.0, SKILL).unwrap();
        remove_in(&s.0).unwrap();
        assert!(!s.0.join("opencomputeruse-remote").exists());
        assert!(s.0.exists());
        assert!(remove_in(&s.0).is_err());
    }

    #[test]
    fn remove_keeps_other_files_in_the_folder() {
        let s = Scratch::new("keep");
        add_in(&s.0, SKILL).unwrap();
        let other = s.0.join("opencomputeruse-remote/notes.txt");
        std::fs::write(&other, "mine").unwrap();
        remove_in(&s.0).unwrap();
        assert!(other.exists());
        assert!(!s.0.join("opencomputeruse-remote/SKILL.md").exists());
    }

    #[test]
    fn a_different_skill_of_that_name_is_never_overwritten_or_removed() {
        let s = Scratch::new("foreign");
        let file = s.0.join("opencomputeruse-remote/SKILL.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "---\nname: something-else\n---\n").unwrap();
        assert!(add_in(&s.0, SKILL).is_err());
        assert!(remove_in(&s.0).is_err());
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "---\nname: something-else\n---\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_folder_is_left_alone() {
        let s = Scratch::new("link");
        let real = s.0.join("dotfiles");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("SKILL.md"), SKILL).unwrap();
        let skills = s.0.join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&real, skills.join("opencomputeruse-remote")).unwrap();
        assert!(add_in(&skills, "new").is_err());
        assert!(remove_in(&skills).is_err());
        assert_eq!(std::fs::read_to_string(real.join("SKILL.md")).unwrap(), SKILL);
    }

    #[test]
    fn claude_desktop_has_no_skills_folder() {
        assert!(skills_dir(Client::ClaudeDesktop).is_none());
        assert!(!available(Client::ClaudeDesktop, true));
        assert!(skills_dir(Client::Kimi).is_some_and(|d| d.ends_with("skills")));
    }
}
