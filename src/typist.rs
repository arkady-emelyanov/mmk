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
//!
//! mmk outlives its X server when the session restarts (a logout, a display manager
//! restart), so a lost connection is reopened on next use and the characters mapped again.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, Mapping, ModMask, PropMode, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::engine::Typist;
use crate::keys::us_char;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const NO_SYMBOL: u32 = 0;
const PROPERTY: &[u8] = b"_MMK_BORROWED_KEYCODES";
/// `XkbUseCoreKbd`.
const CORE_KBD: xkb::DeviceSpec = 0x100;
/// The keycode of the A key, which is a Latin letter in any Latin layout.
const KEYCODE_A: u8 = 38;

/// The least time between attempts to reopen a lost X connection, so a key press while
/// no X server is up costs one failed connect at most this often.
const RECONNECT_EVERY: Duration = Duration::from_secs(1);

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
    /// Evdev codes of US character keys that type something else in some layout group.
    varies: Vec<u16>,
    /// The first layout group that types Latin letters, and which groups do.
    latin_groups: Vec<bool>,
    /// The group `latin` replaced, and the one it selected.
    swapped: Option<(u8, u8)>,
    /// The last attempt to reopen a lost connection.
    reconnected: Option<Instant>,
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
        conn.xkb_use_extension(1, 0)?.reply()?;
        Ok(X11Typist {
            conn,
            root,
            prop,
            slots: Vec::new(),
            wanted: Vec::new(),
            tick: 0,
            stale: false,
            varies: Vec::new(),
            latin_groups: Vec::new(),
            swapped: None,
            reconnected: None,
        })
    }

    /// Run `f`; if it fails because the X server went away, open a new connection, map the
    /// characters on it, and run `f` again.
    fn retrying<T>(&mut self, f: impl Fn(&mut Self) -> Result<T, BoxError>) -> Result<T, BoxError> {
        match f(self) {
            Err(e) if is_disconnect(&*e) => {
                if self.reconnected.is_some_and(|t| t.elapsed() < RECONNECT_EVERY) {
                    return Err(e);
                }
                self.reconnected = Some(Instant::now());
                let mut new = X11Typist::connect()?;
                new.try_prepare(&self.wanted)?;
                new.reconnected = self.reconnected;
                // The old one's Drop can't reach its server; nothing to return there.
                *self = new;
                eprintln!("mmk: reconnected to the X server for typing characters");
                f(self)
            }
            r => r,
        }
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

        // The core keymap lists every group's keysyms for a key: a key types its US character
        // in every layout if nothing but that character (and its Shift pair) appears.
        // Each group holds two levels of a letter key, in group order.
        self.latin_groups = syms(KEYCODE_A)
            .chunks_exact(2)
            .map(|levels| u8::try_from(levels[0]).is_ok_and(|k| k.is_ascii_lowercase()))
            .collect();

        self.varies = (min.max(9)..=max)
            .map(|kc| kc as u16 - 8)
            .filter(|&code| {
                let us = [false, true].map(|shift| us_char(code, shift).map_or(NO_SYMBOL, keysym));
                us[0] != NO_SYMBOL && syms((code + 8) as u8).iter().any(|k| *k != NO_SYMBOL && !us.contains(k))
            })
            .collect();

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

    fn try_layout_proof(&mut self, key: u16, c: char) -> Result<Option<(u16, bool)>, BoxError> {
        self.drain_events()?;
        if self.stale {
            self.refresh()?;
        }
        if !self.varies.contains(&key) {
            return Ok(None);
        }
        self.try_key(c)
    }

    fn lock_group(&self, group: u8) -> Result<(), BoxError> {
        let none = ModMask::from(0u16);
        let group = xkb::Group::from(group);
        self.conn.xkb_latch_lock_state(CORE_KBD, none, none, true, group, none, false, 0)?;
        Ok(())
    }

    fn try_latin(&mut self, on: bool) -> Result<(), BoxError> {
        let now = self.conn.xkb_get_state(CORE_KBD)?.reply()?.locked_group;
        let now = u8::from(now);
        if on {
            let is_latin = |g: u8| self.latin_groups.get(g as usize).copied().unwrap_or(false);
            let target = self.latin_groups.iter().position(|l| *l).map(|g| g as u8);
            if let (false, Some(target), None) = (is_latin(now), target, self.swapped) {
                self.lock_group(target)?;
                self.swapped = Some((now, target));
            }
        } else if let Some((old, target)) = self.swapped.take() {
            // Unless the user switched layout in the meantime.
            if now == target {
                self.lock_group(old)?;
            }
        }
        // Applied before the key that follows reaches the server.
        self.conn.get_input_focus()?.reply()?;
        Ok(())
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
        if let Err(e) = self.retrying(|t| t.try_prepare(chars)) {
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
        self.retrying(|t| t.try_key(c)).unwrap_or_else(|e| {
            eprintln!("mmk: typing {c:?}: {e}");
            None
        })
    }

    fn layout_proof(&mut self, key: u16, c: char) -> Option<(u16, bool)> {
        self.retrying(|t| t.try_layout_proof(key, c)).unwrap_or_else(|e| {
            eprintln!("mmk: typing {c:?} layout-independently: {e}");
            None
        })
    }

    fn refresh(&mut self) {
        if let Err(e) = self.retrying(|t| t.drain_events().and_then(|_| t.refresh())) {
            eprintln!("mmk: restoring characters in the X keymap: {e}");
        }
    }

    fn latin(&mut self, on: bool) {
        if let Err(e) = self.retrying(|t| t.try_latin(on)) {
            eprintln!("mmk: switching layout group: {e}");
        }
    }
}

/// Call `changed` whenever the X keymap changes, on a thread of its own. A burst of changes
/// (a desktop applying a layout to a new keyboard, or our own writes) is reported once,
/// after it settles. If the X server goes away, the thread reconnects to the next one and
/// reports that as a change, since its keymap knows nothing of our characters.
pub fn watch(changed: impl Fn() + Send + 'static) {
    // Connected before returning, so no change made after this call is missed.
    let Ok((conn, _)) = x11rb::connect(None) else { return };
    std::thread::spawn(move || {
        let mut conn = Some(conn);
        loop {
            let c = match conn.take() {
                Some(c) => c,
                None => {
                    std::thread::sleep(Duration::from_secs(2));
                    let Ok((c, _)) = x11rb::connect(None) else { continue };
                    changed();
                    c
                }
            };
            watch_until_closed(&c, &changed);
        }
    });
}

fn watch_until_closed(conn: &RustConnection, changed: &impl Fn()) {
    let keyboard = |ev: &Event| matches!(ev, Event::MappingNotify(e) if e.request == Mapping::KEYBOARD);
    while let Ok(ev) = conn.wait_for_event() {
        if !keyboard(&ev) {
            continue;
        }
        loop {
            std::thread::sleep(Duration::from_millis(100));
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
}

/// Whether `e` means the connection to the X server is gone (as opposed to the server
/// refusing a request).
fn is_disconnect(e: &(dyn std::error::Error + 'static)) -> bool {
    e.is::<ConnectionError>()
        || matches!(e.downcast_ref::<ReplyError>(), Some(ReplyError::ConnectionError(_)))
        || matches!(e.downcast_ref::<ReplyOrIdError>(), Some(ReplyOrIdError::ConnectionError(_)))
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

    /// Against an X server with us,ru,us groups (a nested one: this switches layout):
    /// `DISPLAY=:9 cargo test -- --ignored x11_latin`
    #[test]
    #[ignore]
    fn x11_latin() {
        let mut t = X11Typist::connect().unwrap();
        t.prepare(&[]);
        assert_eq!(t.latin_groups[..3], [true, false, true]);
        let group = |t: &X11Typist| u8::from(t.conn.xkb_get_state(CORE_KBD).unwrap().reply().unwrap().locked_group);
        t.lock_group(1).unwrap();
        t.conn.get_input_focus().unwrap().reply().unwrap();
        t.latin(true);
        assert_eq!(group(&t), 0);
        t.latin(false);
        assert_eq!(group(&t), 1);
        // A layout switch during the hold is kept; an already Latin group is left alone.
        t.latin(true);
        t.lock_group(2).unwrap();
        t.conn.get_input_focus().unwrap().reply().unwrap();
        t.latin(false);
        assert_eq!(group(&t), 2);
        t.latin(true);
        assert_eq!(group(&t), 2);
        t.latin(false);
        assert_eq!(group(&t), 2);
        t.lock_group(0).unwrap();
        t.conn.get_input_focus().unwrap().reply().unwrap();
    }

    /// Against the running X server: keys that type other characters in some layout group
    /// get a borrowed keycode with their US character, the rest none.
    /// `cargo test -- --ignored x11_layout_proof`
    #[test]
    #[ignore]
    fn x11_layout_proof() {
        let mut t = X11Typist::connect().unwrap();
        t.prepare(&[]);
        eprintln!("vary across layouts: {:?}", t.varies.iter().map(|k| crate::keys::key_name(*k)).collect::<Vec<_>>());
        for code in 2..=53 {
            let Some(c) = us_char(code, false) else { continue };
            match t.layout_proof(code, c) {
                None => assert!(!t.varies.contains(&code), "{c}"),
                Some((key, shift)) => {
                    let map = t.conn.get_keyboard_mapping((key + 8) as u8, 1).unwrap().reply().unwrap();
                    assert_eq!(map.keysyms[shift as usize], keysym(c), "{c}");
                }
            }
        }
    }
}
