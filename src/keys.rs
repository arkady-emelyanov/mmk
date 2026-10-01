//! Linux evdev key codes, key names, and chord parsing.

use std::fmt;

pub const KEY_MAX: usize = 0x300;

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_FN: u16 = 0x1d0;
pub const KEY_A: u16 = 30;
pub const KEY_SPACE: u16 = 57;

/// Every key code treated as a modifier.
pub const MOD_KEYS: [u16; 9] = [
    KEY_LEFTCTRL,
    KEY_RIGHTCTRL,
    KEY_LEFTSHIFT,
    KEY_RIGHTSHIFT,
    KEY_LEFTALT,
    KEY_RIGHTALT,
    KEY_LEFTMETA,
    KEY_RIGHTMETA,
    KEY_FN,
];

/// Physical modifier classes, as written on the left side of rules.
pub type ModMask = u8;
pub const M_SHIFT: ModMask = 1;
pub const M_CTRL: ModMask = 2;
pub const M_OPT: ModMask = 4;
pub const M_CMD: ModMask = 8;
pub const M_FN: ModMask = 16;

pub fn class_of(code: u16) -> ModMask {
    match code {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => M_SHIFT,
        KEY_LEFTCTRL | KEY_RIGHTCTRL => M_CTRL,
        KEY_LEFTALT | KEY_RIGHTALT => M_OPT,
        KEY_LEFTMETA | KEY_RIGHTMETA => M_CMD,
        KEY_FN => M_FN,
        _ => 0,
    }
}

pub fn is_modifier(code: u16) -> bool {
    class_of(code) != 0
}

pub fn is_alt(code: u16) -> bool {
    code == KEY_LEFTALT || code == KEY_RIGHTALT
}

pub fn is_right_side(code: u16) -> bool {
    matches!(code, KEY_RIGHTCTRL | KEY_RIGHTSHIFT | KEY_RIGHTALT | KEY_RIGHTMETA)
}

pub fn right_of(code: u16) -> u16 {
    match code {
        KEY_LEFTCTRL => KEY_RIGHTCTRL,
        KEY_LEFTSHIFT => KEY_RIGHTSHIFT,
        KEY_LEFTALT => KEY_RIGHTALT,
        KEY_LEFTMETA => KEY_RIGHTMETA,
        c => c,
    }
}

const NAMES: &[(&str, u16)] = &[
    ("esc", 1), ("1", 2), ("2", 3), ("3", 4), ("4", 5), ("5", 6), ("6", 7), ("7", 8), ("8", 9),
    ("9", 10), ("0", 11), ("minus", 12), ("equal", 13), ("backspace", 14), ("tab", 15),
    ("q", 16), ("w", 17), ("e", 18), ("r", 19), ("t", 20), ("y", 21), ("u", 22), ("i", 23),
    ("o", 24), ("p", 25), ("leftbrace", 26), ("rightbrace", 27), ("enter", 28), ("leftctrl", 29),
    ("a", 30), ("s", 31), ("d", 32), ("f", 33), ("g", 34), ("h", 35), ("j", 36), ("k", 37),
    ("l", 38), ("semicolon", 39), ("apostrophe", 40), ("grave", 41), ("leftshift", 42),
    ("backslash", 43), ("z", 44), ("x", 45), ("c", 46), ("v", 47), ("b", 48), ("n", 49),
    ("m", 50), ("comma", 51), ("dot", 52), ("slash", 53), ("rightshift", 54),
    ("kpasterisk", 55), ("leftalt", 56), ("space", 57), ("capslock", 58), ("f1", 59),
    ("f2", 60), ("f3", 61), ("f4", 62), ("f5", 63), ("f6", 64), ("f7", 65), ("f8", 66),
    ("f9", 67), ("f10", 68), ("numlock", 69), ("scrolllock", 70), ("kp7", 71), ("kp8", 72),
    ("kp9", 73), ("kpminus", 74), ("kp4", 75), ("kp5", 76), ("kp6", 77), ("kpplus", 78),
    ("kp1", 79), ("kp2", 80), ("kp3", 81), ("kp0", 82), ("kpdot", 83), ("102nd", 86),
    ("f11", 87), ("f12", 88), ("kpenter", 96), ("rightctrl", 97), ("kpslash", 98),
    ("print", 99), ("rightalt", 100), ("home", 102), ("up", 103), ("pageup", 104),
    ("left", 105), ("right", 106), ("end", 107), ("down", 108), ("pagedown", 109),
    ("insert", 110), ("delete", 111), ("mute", 113), ("volumedown", 114), ("volumeup", 115),
    ("power", 116), ("kpequal", 117), ("pause", 119), ("scale", 120), ("kpcomma", 121),
    ("leftmeta", 125), ("rightmeta", 126), ("compose", 127), ("menu", 139),
    ("screenlock", 152), ("eject", 161), ("nextsong", 163), ("playpause", 164),
    ("previoussong", 165), ("f13", 183), ("f14", 184), ("f15", 185), ("f16", 186),
    ("f17", 187), ("f18", 188), ("f19", 189), ("f20", 190), ("f21", 191), ("f22", 192),
    ("f23", 193), ("f24", 194), ("dashboard", 204), ("brightnessdown", 224),
    ("brightnessup", 225), ("kbdillumdown", 229), ("kbdillumup", 230), ("unknown", 240),
    ("fn", KEY_FN),
];

const ALIASES: &[(&str, &str)] = &[
    ("escape", "esc"), ("return", "enter"), ("del", "delete"), ("pgup", "pageup"),
    ("pgdn", "pagedown"), ("[", "leftbrace"), ("]", "rightbrace"), (";", "semicolon"),
    ("'", "apostrophe"), ("`", "grave"), ("\\", "backslash"), (",", "comma"), (".", "dot"),
    ("period", "dot"), ("/", "slash"), ("=", "equal"), ("sysrq", "print"), ("prtsc", "print"),
    ("missioncontrol", "scale"), ("launchpad", "dashboard"), ("ins", "insert"),
    ("lock", "screenlock"), ("coffee", "screenlock"),
];

pub fn lookup_key(name: &str) -> Option<u16> {
    let name = name.to_ascii_lowercase();
    let name = ALIASES.iter().find(|(a, _)| *a == name).map(|(_, n)| *n).unwrap_or(&name);
    NAMES.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

pub fn key_name(code: u16) -> String {
    NAMES
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| format!("key{code}"))
}

fn input_mod(word: &str) -> Option<ModMask> {
    Some(match word {
        "cmd" | "command" | "super" | "meta" | "win" => M_CMD,
        "opt" | "option" | "alt" => M_OPT,
        "ctrl" | "control" => M_CTRL,
        "shift" => M_SHIFT,
        "fn" => M_FN,
        _ => return None,
    })
}

/// Output modifier word -> emitted key (left side).
pub fn output_mod(word: &str) -> Option<u16> {
    Some(match word {
        "ctrl" | "control" => KEY_LEFTCTRL,
        "shift" => KEY_LEFTSHIFT,
        "alt" | "opt" | "option" => KEY_LEFTALT,
        "super" | "meta" | "win" | "cmd" => KEY_LEFTMETA,
        _ => return None,
    })
}

/// Split "cmd-shift-left" into parts; a trailing "--" means the minus key.
fn split_chord(s: &str) -> Vec<String> {
    let s = s.trim().to_ascii_lowercase();
    if s == "-" {
        return vec!["minus".into()];
    }
    if let Some(head) = s.strip_suffix("--") {
        let mut parts: Vec<String> = head.split('-').map(String::from).collect();
        parts.push("minus".into());
        return parts;
    }
    s.split('-').map(String::from).collect()
}

/// A physical trigger: modifier mask + key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Trigger {
    pub mods: ModMask,
    pub key: u16,
}

impl Trigger {
    pub fn id(self) -> u32 {
        (self.mods as u32) << 16 | self.key as u32
    }
}

pub fn parse_trigger(s: &str) -> Result<Trigger, String> {
    let parts = split_chord(s);
    let (last, mods) = parts.split_last().ok_or_else(|| format!("{s:?}: empty"))?;
    let mut mask = 0;
    for p in mods {
        mask |= input_mod(p)
            .ok_or_else(|| format!("{s:?}: unknown modifier {p:?} (use cmd, opt, ctrl, shift, fn)"))?;
    }
    if input_mod(last).is_some() {
        return Err(format!("{s:?}: trigger must end with a non-modifier key"));
    }
    let key = lookup_key(last).ok_or_else(|| format!("{s:?}: unknown key {last:?}"))?;
    if is_modifier(key) {
        return Err(format!("{s:?}: trigger must end with a non-modifier key"));
    }
    Ok(Trigger { mods: mask, key })
}

/// One output chord: modifiers held while `key` is pressed. `key == 0` is modifier-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: Vec<u16>,
    pub key: u16,
}

pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let parts = split_chord(s);
    let mut chord = Chord { mods: Vec::new(), key: 0 };
    for (i, p) in parts.iter().enumerate() {
        if let Some(m) = output_mod(p) {
            chord.mods.push(m);
            continue;
        }
        let code = lookup_key(p).ok_or_else(|| format!("{s:?}: unknown key {p:?}"))?;
        if is_modifier(code) {
            chord.mods.push(code);
            continue;
        }
        if i != parts.len() - 1 {
            return Err(format!("{s:?}: {p:?} must be the last part"));
        }
        chord.key = code;
    }
    Ok(chord)
}

/// A set of physical keys held together, e.g. "esc+backspace+enter".
pub fn parse_key_set(s: &str) -> Result<Vec<u16>, String> {
    s.split('+')
        .map(|k| lookup_key(k.trim()).ok_or_else(|| format!("{s:?}: unknown key {k:?}")))
        .collect()
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = self.mods.iter().map(|m| key_name(*m)).collect();
        if self.key != 0 {
            parts.push(key_name(self.key));
        }
        write!(f, "{}", parts.join("-"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(parse_trigger("cmd-shift-left").unwrap(), Trigger { mods: M_CMD | M_SHIFT, key: 105 });
        assert_eq!(parse_trigger("cmd--").unwrap().key, 12);
        assert_eq!(parse_trigger("opt-[").unwrap().key, 26);
        assert!(parse_trigger("cmd-shift").is_err());
        assert!(parse_trigger("cmd-nope").is_err());
        assert!(parse_trigger("hyper-a").is_err());
    }

    #[test]
    fn chords() {
        let c = parse_chord("ctrl-shift-home").unwrap();
        assert_eq!(c.mods, vec![KEY_LEFTCTRL, KEY_LEFTSHIFT]);
        assert_eq!(c.key, 102);
        let s = parse_chord("super").unwrap();
        assert_eq!((s.mods, s.key), (vec![KEY_LEFTMETA], 0));
        assert!(parse_chord("a-ctrl").is_err());
    }
}
