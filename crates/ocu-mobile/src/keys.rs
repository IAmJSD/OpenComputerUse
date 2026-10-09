//! `press_key` on a phone: the desktop's chords, plus the phone's own
//! buttons. On a phone "home" is the Home button, not the start of a line.

use anyhow::Result;

use ocu_core::keys::{parse_chord, Chord};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Home,
    Back,
    Recents,
    Power,
    VolumeUp,
    VolumeDown,
    Menu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Button(Button),
    Chord(Chord),
}

pub const HELP: &str = "On phones and simulators, press_key also takes the device's buttons: home, back (Android), recents (Android), power, volume_up, volume_down, menu (Android).";

pub fn parse(keys: &str) -> Result<Vec<Press>> {
    let presses: Vec<Press> = keys
        .split_whitespace()
        .map(|part| {
            let button = match part.to_ascii_lowercase().replace('-', "_").as_str() {
                "home" | "home_button" | "homescreen" | "home_screen" => Some(Button::Home),
                "back" => Some(Button::Back),
                "recents" | "app_switch" | "appswitch" | "overview" => Some(Button::Recents),
                "power" | "lock" => Some(Button::Power),
                "volume_up" | "volumeup" => Some(Button::VolumeUp),
                "volume_down" | "volumedown" => Some(Button::VolumeDown),
                "menu" => Some(Button::Menu),
                _ => None,
            };
            Ok(match button {
                Some(b) => Press::Button(b),
                None => Press::Chord(parse_chord(part)?),
            })
        })
        .collect::<Result<_>>()?;
    anyhow::ensure!(!presses.is_empty(), "no keys given");
    Ok(presses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocu_core::keys::{Key, NamedKey};

    #[test]
    fn buttons_and_chords() {
        let p = parse("home ctrl+a backspace Back").unwrap();
        assert_eq!(p[0], Press::Button(Button::Home));
        assert!(
            matches!(p[1], Press::Chord(c) if c.modifiers.ctrl && c.key == Some(Key::Char('a')))
        );
        assert!(matches!(p[2], Press::Chord(c) if c.key == Some(Key::Named(NamedKey::Backspace))));
        assert_eq!(p[3], Press::Button(Button::Back));
    }
}
