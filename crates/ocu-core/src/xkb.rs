//! Typing characters a US keyboard has no key for, on a Wayland seat whose
//! keymap the compositor owns. For a run of such characters the keymap is
//! swapped for a copy whose letter and digit keys produce them instead, the
//! keys are pressed, and the US keymap is put back. That is what a person
//! on another layout sends anyway (code "KeyQ", key "é"), so apps take it as
//! ordinary typing.
//!
//! Only text manipulation lives here: the keymap comes from xkbcommon as
//! text, and goes back to it as text.

/// The keys a remapped keymap reuses: an xkb key name and its evdev code.
pub const SLOTS: [(&str, u16); 47] = [
    ("AE01", 2),
    ("AE02", 3),
    ("AE03", 4),
    ("AE04", 5),
    ("AE05", 6),
    ("AE06", 7),
    ("AE07", 8),
    ("AE08", 9),
    ("AE09", 10),
    ("AE10", 11),
    ("AE11", 12),
    ("AE12", 13),
    ("AD01", 16),
    ("AD02", 17),
    ("AD03", 18),
    ("AD04", 19),
    ("AD05", 20),
    ("AD06", 21),
    ("AD07", 22),
    ("AD08", 23),
    ("AD09", 24),
    ("AD10", 25),
    ("AD11", 26),
    ("AD12", 27),
    ("AC01", 30),
    ("AC02", 31),
    ("AC03", 32),
    ("AC04", 33),
    ("AC05", 34),
    ("AC06", 35),
    ("AC07", 36),
    ("AC08", 37),
    ("AC09", 38),
    ("AC10", 39),
    ("AC11", 40),
    ("TLDE", 41),
    ("BKSL", 43),
    ("AB01", 44),
    ("AB02", 45),
    ("AB03", 46),
    ("AB04", 47),
    ("AB05", 48),
    ("AB06", 49),
    ("AB07", 50),
    ("AB08", 51),
    ("AB09", 52),
    ("AB10", 53),
];

/// xkb's name for the keysym of a character: `U00E9`. None for control
/// characters, which have no keysym.
pub fn keysym_name(c: char) -> Option<String> {
    let cp = c as u32;
    if cp < 0x20 || (0x7f..0xa0).contains(&cp) {
        return None;
    }
    Some(format!("U{cp:04X}"))
}

/// A run of characters to type through one remapped keymap: the keymap,
/// and the evdev code to press for each character in order.
#[derive(Debug, PartialEq, Eq)]
pub struct Remapped {
    pub keymap: String,
    pub presses: Vec<u16>,
}

/// Splits `text` (characters with no US key) into runs of at most
/// [`SLOTS`] distinct characters, each with a keymap built from `base` (a
/// keymap as xkbcommon writes it). Characters with no keysym are dropped.
/// None when `base` is not a keymap this understands.
pub fn remap(base: &str, text: &str) -> Option<Vec<Remapped>> {
    let mut runs = Vec::new();
    let mut chars: Vec<char> = Vec::new();
    let mut presses = Vec::new();
    let flush = |chars: &mut Vec<char>, presses: &mut Vec<u16>, runs: &mut Vec<Remapped>| {
        if presses.is_empty() {
            return Some(());
        }
        let keys: Vec<(&str, String)> = chars
            .iter()
            .zip(SLOTS)
            .filter_map(|(&c, (name, _))| Some((name, keysym_name(c)?)))
            .collect();
        runs.push(Remapped {
            keymap: replace_keys(base, &keys)?,
            presses: std::mem::take(presses),
        });
        chars.clear();
        Some(())
    };
    for c in text.chars() {
        if keysym_name(c).is_none() {
            continue;
        }
        let slot = match chars.iter().position(|&o| o == c) {
            Some(i) => i,
            None => {
                if chars.len() == SLOTS.len() {
                    flush(&mut chars, &mut presses, &mut runs)?;
                }
                chars.push(c);
                chars.len() - 1
            }
        };
        presses.push(SLOTS[slot].1);
    }
    flush(&mut chars, &mut presses, &mut runs)?;
    Some(runs)
}

/// `base` with each named key's entry in `xkb_symbols` replaced by one
/// level producing `keysym`, whatever modifiers are held.
fn replace_keys(base: &str, keys: &[(&str, String)]) -> Option<String> {
    let start = base.find("xkb_symbols")?;
    let open = start + base[start..].find('{')?;
    let close = matching_brace(base, open)?;
    let mut symbols = base[open + 1..close].to_string();
    for (name, keysym) in keys {
        let entry = format!("key <{name}> {{ type= \"ONE_LEVEL\", symbols[1]= [ {keysym} ] }};");
        let tag = format!("key <{name}>");
        match symbols.find(&tag) {
            Some(at) => {
                let brace = at + symbols[at..].find('{')?;
                let end = matching_brace(&symbols, brace)?;
                let semi = end + symbols[end..].find(';')?;
                symbols.replace_range(at..=semi, &entry);
            }
            None => {
                symbols.push_str("\n    ");
                symbols.push_str(&entry);
                symbols.push('\n');
            }
        }
    }
    let mut out = String::with_capacity(base.len() + 256);
    out.push_str(&base[..=open]);
    out.push_str(&symbols);
    out.push_str(&base[close..]);
    Some(out)
}

fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, b) in s.bytes().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "xkb_keymap {\nxkb_keycodes \"evdev\" {\n\t<AE01> = 10;\n};\n\
        xkb_symbols \"pc+us\" {\n\tname[1]=\"English (US)\";\n\n\
        \tkey <AE01>               {\n\t\ttype= \"TWO_LEVEL\",\n\t\tsymbols[1]= [               1,          exclam ]\n\t};\n\
        \tkey <AD01> { type= \"ALPHABETIC\", symbols[1]= [ q, Q ] };\n\
        \tmodifier_map Shift { <LFSH> };\n};\n};\n";

    #[test]
    fn keys_are_replaced_in_place() {
        let runs = remap(BASE, "éé€").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].presses, vec![2, 2, 3]);
        let k = &runs[0].keymap;
        assert!(k.contains("key <AE01> { type= \"ONE_LEVEL\", symbols[1]= [ U00E9 ] };"));
        assert!(k.contains("key <AE02> { type= \"ONE_LEVEL\", symbols[1]= [ U20AC ] };"));
        assert!(!k.contains("exclam"));
        // Untouched keys and the rest of the keymap stay as they were.
        assert!(k.contains("symbols[1]= [ q, Q ]"));
        assert!(k.contains("<AE01> = 10;"));
        assert!(k.contains("\tmodifier_map Shift { <LFSH> };\n"));
        assert!(k.ends_with("] };\n};\n};\n"), "{k}");
    }

    #[test]
    fn long_runs_split_when_the_slots_run_out() {
        let text: String = (0..60)
            .map(|i| char::from_u32(0x400 + i).unwrap())
            .collect();
        let runs = remap(BASE, &text).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].presses.len(), SLOTS.len());
        assert_eq!(runs[1].presses.len(), 60 - SLOTS.len());
        assert_eq!(runs[1].presses[0], SLOTS[0].1);
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(keysym_name('\u{85}'), None);
        assert_eq!(keysym_name('😀').as_deref(), Some("U1F600"));
        assert!(remap(BASE, "\u{1}").unwrap().is_empty());
        assert!(remap("garbage", "é").is_none());
    }
}
