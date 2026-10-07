//! Keysyms and the server's keyboard map: which keycode (and whether
//! shift) produces a keysym, borrowing a spare keycode for keysyms the map
//! lacks, the way xdotool types characters a layout cannot.

use anyhow::{bail, Result};
use x11rb::connection::Connection as _;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Keycode, Keysym};
use x11rb::rust_connection::RustConnection;

use ocu_core::keys::{Key, Modifiers, NamedKey};

pub fn keysym(key: Key) -> Keysym {
    match key {
        Key::Named(k) => match k {
            NamedKey::Enter => 0xff0d,
            NamedKey::Tab => 0xff09,
            NamedKey::Escape => 0xff1b,
            NamedKey::Backspace => 0xff08,
            NamedKey::Delete => 0xffff,
            NamedKey::Space => 0x20,
            NamedKey::Home => 0xff50,
            NamedKey::Left => 0xff51,
            NamedKey::Up => 0xff52,
            NamedKey::Right => 0xff53,
            NamedKey::Down => 0xff54,
            NamedKey::PageUp => 0xff55,
            NamedKey::PageDown => 0xff56,
            NamedKey::End => 0xff57,
            NamedKey::Insert => 0xff63,
            NamedKey::CapsLock => 0xffe5,
            NamedKey::F(n) => 0xffbe + (n as u32 - 1),
        },
        Key::Char(c) => {
            let cp = c as u32;
            // Latin-1 keysyms are the code points; the rest are offset.
            if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 | cp
            }
        }
    }
}

pub fn modifier_keysyms(m: Modifiers) -> Vec<Keysym> {
    let mut v = Vec::new();
    if m.ctrl {
        v.push(0xffe3);
    }
    if m.alt {
        v.push(0xffe9);
    }
    if m.meta {
        v.push(0xffeb);
    }
    if m.shift {
        v.push(0xffe1);
    }
    v
}

pub struct Keymap {
    min: Keycode,
    per: usize,
    syms: Vec<Keysym>,
    /// Keycodes with nothing on them, lent to missing keysyms in turn.
    spare: Vec<Keycode>,
    next_spare: usize,
}

impl Keymap {
    pub fn read(conn: &RustConnection) -> Result<Self> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = reply.keysyms_per_keycode as usize;
        let spare = (0..=(max - min) as usize)
            .filter(|&i| reply.keysyms[i * per..(i + 1) * per].iter().all(|&s| s == 0))
            .map(|i| min + i as u8)
            .collect();
        Ok(Self { min, per, syms: reply.keysyms, spare, next_spare: 0 })
    }

    /// The keycode for `sym`, and whether it needs shift.
    pub fn code_for(&mut self, conn: &RustConnection, sym: Keysym) -> Result<(Keycode, bool)> {
        for (i, chunk) in self.syms.chunks(self.per).enumerate() {
            if chunk.first() == Some(&sym) {
                return Ok((self.min + i as u8, false));
            }
        }
        for (i, chunk) in self.syms.chunks(self.per).enumerate() {
            if chunk.get(1) == Some(&sym) {
                return Ok((self.min + i as u8, true));
            }
        }
        // Not on the keyboard: put it on a spare key.
        if self.spare.is_empty() {
            bail!("no key can produce keysym {sym:#x}");
        }
        let code = self.spare[self.next_spare % self.spare.len()];
        self.next_spare += 1;
        let row = vec![sym; self.per];
        conn.change_keyboard_mapping(1, code, self.per as u8, &row)?;
        conn.sync()?;
        let at = (code - self.min) as usize * self.per;
        self.syms[at..at + self.per].copy_from_slice(&row);
        Ok((code, false))
    }
}
