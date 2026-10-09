//! Typing characters the keyboard layout doesn't have (en dash, curly quotes, ...).
//!
//! A virtual keyboard can only send key codes, so on X11 mmk borrows keycodes that have no
//! keysyms in the live keymap and maps each needed character onto one of them, as xdotool
//! does: two characters per keycode (plain and with Shift). Only the in-memory keymap of the
//! running X server changes, never a config file; the borrowed keycodes are cleared on exit
//! and recorded in a root window property, so a killed mmk's leftovers are reclaimed on the
//! next start. Everything is mapped when the config loads; a key press only looks the
//! character up, unless there are more characters than free keycodes (then the least
//! recently used one is remapped on demand) or the keymap was reset under us.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Mapping, PropMode, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::engine::Typist;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const NO_SYMBOL: u32 = 0;
const PROPERTY: &[u8] = b"_MMK_BORROWED_KEYCODES";

/// Evdev codes never borrowed, even if the X keymap leaves them empty: keys that the
/// kernel, logind or the session may act on below X.
const RESERVED: [u16; 10] = [116, 142, 143, 152, 205, 227, 236, 237, 238, 247];

/// Typist for a session without X11 (or where X fails): nothing is mappable, so the
/// engine falls back to Ctrl+Shift+U hex input.
pub fn select() -> Option<Box<dyn Typist>> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|s| s == "wayland") || std::env::var_os("DISPLAY").is_none() {
        return None;
    }
    match X11Typist::connect() {
        Ok(t) => Some(Box::new(t)),
        Err(e) => {
            eprintln!("mmk: X11 keymap unavailable ({e}); typing characters via Ctrl+Shift+U");
            None
        }
    }
}

/// A borrowed keycode and the characters it types without and with Shift.
struct Slot {
    keycode: u8,
    chars: [Option<char>; 2],
    used: [u64; 2],
    /// Keysyms last written to the server; zero when untouched or cleared.
    written: [u32; 2],
}

impl Slot {
    fn new(keycode: u8) -> Self {
        Slot { keycode, chars: [None; 2], used: [0; 2], written: [NO_SYMBOL; 2] }
    }

    fn keysyms(&self) -> [u32; 2] {
        match self.chars {
            // A lone character goes on both levels, so Shift or Caps Lock can't alter it.
            [Some(a), None] => [keysym(a); 2],
            [a, b] => [a.map_or(NO_SYMBOL, keysym), b.map_or(NO_SYMBOL, keysym)],
        }
    }
}

pub struct X11Typist {
    conn: RustConnection,
    root: Window,
    prop: u32,
    slots: Vec<Slot>,
    /// Characters of the current config.
    wanted: Vec<char>,
    tick: u64,
    /// A MappingNotify arrived: check our keycodes before the next use.
    stale: bool,
}

impl X11Typist {
    pub fn connect() -> Result<Self, BoxError> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let prop = conn.intern_atom(false, PROPERTY)?.reply()?.atom;
        // Keycodes a previous mmk borrowed and didn't return (it was killed): clear them, so
        // the scan finds them free again.
        let prev = conn.get_property(false, root, prop, AtomEnum::CARDINAL, 0, 256)?.reply()?;
        for kc in prev.value32().into_iter().flatten().filter_map(|kc| u8::try_from(kc).ok()) {
            conn.change_keyboard_mapping(1, kc, 2, &[NO_SYMBOL; 2])?;
        }
        Ok(X11Typist { conn, root, prop, slots: Vec::new(), wanted: Vec::new(), tick: 0, stale: false })
    }

    /// Re-read the keymap: drop borrowed keycodes something else took, borrow newly free
    /// ones, place characters that have no keycode, and write what changed.
    fn refresh(&mut self) -> Result<(), BoxError> {
        self.stale = false;
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let modmap = self.conn.get_modifier_mapping()?.reply()?.keycodes;
        let per = map.keysyms_per_keycode as usize;
        let syms = |kc: u8| &map.keysyms[(kc - min) as usize * per..][..per];

        self.slots.retain_mut(|s| {
            let cur = syms(s.keycode);
            if cur.iter().all(|k| *k == NO_SYMBOL) {
                // Free, or our mapping was reset (a layout reload): write it again.
                s.written = [NO_SYMBOL; 2];
                true
            } else {
                // Still ours if it holds what we wrote; anything else means the keymap gave
                // this keycode a real key.
                s.written[0] != NO_SYMBOL && cur[0] == s.written[0]
            }
        });
        for kc in min.max(9)..=max {
            if syms(kc).iter().all(|k| *k == NO_SYMBOL)
                && !modmap.contains(&kc)
                && !RESERVED.contains(&(kc as u16 - 8))
                && !self.slots.iter().any(|s| s.keycode == kc)
            {
                self.slots.push(Slot::new(kc));
            }
        }
        for c in self.wanted.clone() {
            if self.find(c).is_none() && !self.place(c) {
                break; // no room; the rest are placed as they're typed
            }
        }
        self.flush()
    }

    /// Write every slot whose mapping changed, record the borrowed keycodes, and wait until
    /// the server has applied it all, so a key tapped next is read with the new mapping.
    fn flush(&mut self) -> Result<(), BoxError> {
        for s in &mut self.slots {
            let k = s.keysyms();
            if k != s.written {
                self.conn.change_keyboard_mapping(1, s.keycode, 2, &k)?;
                s.written = k;
            }
        }
        let borrowed: Vec<u32> =
            self.slots.iter().filter(|s| s.written != [NO_SYMBOL; 2]).map(|s| s.keycode as u32).collect();
        if borrowed.is_empty() {
            self.conn.delete_property(self.root, self.prop)?;
        } else {
            self.conn.change_property32(PropMode::REPLACE, self.root, self.prop, AtomEnum::CARDINAL, &borrowed)?;
        }
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn find(&self, c: char) -> Option<(usize, usize)> {
        self.slots.iter().enumerate().find_map(|(i, s)| s.chars.iter().position(|x| *x == Some(c)).map(|l| (i, l)))
    }

    /// Put `c` in a free level, else in place of the least recently used character.
    fn place(&mut self, c: char) -> bool {
        // A Shift level must not hold the other level's case pair: X would treat the key as
        // a letter and let Caps Lock swap them.
        let pairs = |s: &Slot, l: usize| s.chars[1 - l].is_some_and(|o| o.to_lowercase().eq(c.to_lowercase()));
        let free = self.slots.iter().enumerate().find_map(|(i, s)| {
            (0..2).find(|&l| s.chars[l].is_none() && (l == 0 || s.chars[0].is_some()) && !pairs(s, l)).map(|l| (i, l))
        });
        let lru = || {
            (0..self.slots.len())
                .flat_map(|i| (0..2).map(move |l| (i, l)))
                .filter(|&(i, l)| self.slots[i].chars[l].is_some() && !pairs(&self.slots[i], l))
                .min_by_key(|&(i, l)| self.slots[i].used[l])
        };
        let Some((i, l)) = free.or_else(lru) else { return false };
        self.slots[i].chars[l] = Some(c);
        self.slots[i].used[l] = self.tick;
        true
    }

    /// Note keymap changes (a layout reload, another device, our own writes).
    fn drain_events(&mut self) -> Result<(), BoxError> {
        while let Some(ev) = self.conn.poll_for_event()? {
            if let Event::MappingNotify(e) = ev {
                if e.request == Mapping::KEYBOARD {
                    self.stale = true;
                }
            }
        }
        Ok(())
    }

    fn try_prepare(&mut self, chars: &[char]) -> Result<(), BoxError> {
        self.wanted = chars.to_vec();
        for s in &mut self.slots {
            for l in 0..2 {
                if s.chars[l].is_some_and(|c| !chars.contains(&c)) {
                    s.chars[l] = None;
                }
            }
            // Keep levels packed: a Shift-level character never sits alone.
            if s.chars[0].is_none() {
                s.chars = [s.chars[1], None];
                s.used = [s.used[1], 0];
            }
        }
        self.refresh()
    }

    fn try_key(&mut self, c: char) -> Result<Option<(u16, bool)>, BoxError> {
        self.drain_events()?;
        if self.stale {
            self.refresh()?;
        }
        self.tick += 1;
        if self.find(c).is_none() {
            if !self.place(c) {
                return Ok(None);
            }
            self.flush()?;
        }
        let (i, l) = self.find(c).expect("placed");
        self.slots[i].used[l] = self.tick;
        // A lone character sits on both levels; it's tapped without Shift.
        Ok(Some((self.slots[i].keycode as u16 - 8, l == 1)))
    }
}

impl Typist for X11Typist {
    fn prepare(&mut self, chars: &[char]) {
        if let Err(e) = self.try_prepare(chars) {
            eprintln!("mmk: mapping characters into the X keymap: {e}");
        }
        let mapped = chars.iter().filter(|c| self.find(**c).is_some()).count();
        if mapped < chars.len() {
            eprintln!(
                "mmk: {} characters but room for only {mapped} in the keymap; the rest are mapped as they're typed",
                chars.len()
            );
        }
    }

    fn key(&mut self, c: char) -> Option<(u16, bool)> {
        self.try_key(c).unwrap_or_else(|e| {
            eprintln!("mmk: typing {c:?}: {e}");
            None
        })
    }

    fn refresh(&mut self) {
        let r = self.drain_events().and_then(|_| self.refresh());
        if let Err(e) = r {
            eprintln!("mmk: restoring characters in the X keymap: {e}");
        }
    }
}

/// Call `changed` whenever the X keymap changes, on a thread of its own. A burst of changes
/// (a desktop applying a layout to a new keyboard, or our own writes) is reported once,
/// after it settles.
pub fn watch(changed: impl Fn() + Send + 'static) {
    // Connected before returning, so no change made after this call is missed.
    let Ok((conn, _)) = x11rb::connect(None) else { return };
    std::thread::spawn(move || {
        let keyboard = |ev: &Event| matches!(ev, Event::MappingNotify(e) if e.request == Mapping::KEYBOARD);
        while let Ok(ev) = conn.wait_for_event() {
            if !keyboard(&ev) {
                continue;
            }
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let mut more = false;
                while let Ok(Some(ev)) = conn.poll_for_event() {
                    more |= keyboard(&ev);
                }
                if !more {
                    break;
                }
            }
            changed();
        }
    });
}

impl Drop for X11Typist {
    /// Return the borrowed keycodes. The round trip in flush() matters here: the server
    /// discards requests it hasn't processed when the client disconnects.
    fn drop(&mut self) {
        for s in &mut self.slots {
            s.chars = [None; 2];
        }
        let _ = self.flush();
    }
}

/// The X keysym for a character: Latin-1 keysyms are the code point, others are
/// 0x01000000 + code point.
fn keysym(c: char) -> u32 {
    let u = c as u32;
    if (0x20..=0x7e).contains(&u) || (0xa0..=0xff).contains(&u) { u } else { 0x0100_0000 | u }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(keysym('a'), 0x61);
        assert_eq!(keysym('\u{a0}'), 0xa0);
        assert_eq!(keysym('—'), 0x0100_2014);
    }

    /// Against the running X server: maps characters, checks the keymap, cleans up.
    /// `cargo test -- --ignored x11_round_trip`
    #[test]
    #[ignore]
    fn x11_round_trip() {
        let mut t = X11Typist::connect().unwrap();
        let chars: Vec<char> = "–—“”‘’…".chars().collect();
        t.prepare(&chars);
        for c in &chars {
            let (code, shift) = t.key(*c).unwrap();
            let kc = (code + 8) as u8;
            let map = t.conn.get_keyboard_mapping(kc, 1).unwrap().reply().unwrap();
            let level = map.keysyms[shift as usize];
            assert_eq!(level, keysym(*c), "{c} at keycode {kc}: {:x?}", map.keysyms);
        }
        // A config change frees what it no longer uses.
        t.prepare(&['–']);
        assert!(t.find('—').is_none());
        let codes: Vec<u8> = t.slots.iter().map(|s| s.keycode).collect();
        drop(t);
        let (conn, _) = x11rb::connect(None).unwrap();
        for kc in codes {
            let map = conn.get_keyboard_mapping(kc, 1).unwrap().reply().unwrap();
            assert!(map.keysyms.iter().all(|k| *k == NO_SYMBOL), "keycode {kc} left mapped: {:x?}", map.keysyms);
        }
    }
}
