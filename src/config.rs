//! Configuration: TOML file -> compiled lookup tables.
//!
//! The embedded default config is the single source of default values. A user
//! config may omit any setting; omitted settings take the default config's value.
//! Rules and app profiles are not merged: the user file defines them completely.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::keys::*;

pub const DEFAULT_CONFIG: &str = include_str!("default.toml");

#[derive(Debug)]
pub enum Action {
    /// Swallow the key.
    None,
    /// Single chord, held as long as the physical key is held.
    Hold(Chord),
    /// Sequence of chords (or a modifier-only chord), tapped.
    Tap(Vec<Chord>),
    /// Shell command, run on press.
    Exec(String),
}

pub fn parse_action(s: &str) -> Result<Action, String> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return Ok(Action::None);
    }
    if let Some(cmd) = s.strip_prefix("exec:") {
        return Ok(Action::Exec(cmd.trim().to_string()));
    }
    let chords = s.split_whitespace().map(parse_chord).collect::<Result<Vec<_>, _>>()?;
    if chords.len() == 1 && chords[0].key != 0 {
        return Ok(Action::Hold(chords.into_iter().next().unwrap()));
    }
    Ok(Action::Tap(chords))
}

/// A compiled app profile. The base profile (no app matched) is named "global".
#[derive(Debug)]
pub struct Profile {
    pub name: String,
    pub globs: Vec<String>,
    pub raw: bool,
    /// What a held Cmd emits for keys without a rule (left-side keys).
    pub cmd: Vec<u16>,
    /// Trigger id -> action; global rules already merged in.
    pub rules: HashMap<u32, Arc<Action>>,
}

#[derive(Debug)]
pub struct Switcher {
    pub enter: HashSet<u32>,
    pub hold: u16,
    pub keys: HashSet<u16>,
}

#[derive(Debug)]
pub struct Config {
    pub remap: Vec<u16>,
    pub alt_guard: Option<u16>,
    pub cmd_tap: Option<Arc<Action>>,
    pub cmd_tap_ms: u64,
    pub panic: Vec<u16>,
    pub toggle: Option<u32>,
    pub tray: bool,
    pub window_backend: String,
    pub grab_wait_ms: u64,
    pub open_retry_ms: u64,
    pub switcher: Switcher,
    pub base: Arc<Profile>,
    pub apps: Vec<Arc<Profile>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawSwitcher {
    enter: Option<Vec<String>>,
    hold: Option<String>,
    keys: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    name: Option<String>,
    #[serde(default)]
    class: Vec<String>,
    #[serde(default)]
    raw: bool,
    cmd: Option<String>,
    #[serde(default)]
    keys: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    cmd: Option<String>,
    alt_guard: Option<String>,
    cmd_tap: Option<String>,
    cmd_tap_ms: Option<u64>,
    panic: Option<String>,
    toggle: Option<String>,
    tray: Option<bool>,
    window_backend: Option<String>,
    grab_wait_ms: Option<u64>,
    open_retry_ms: Option<u64>,
    switcher: Option<RawSwitcher>,
    #[serde(default)]
    remap: HashMap<String, String>,
    #[serde(default)]
    keys: HashMap<String, String>,
    #[serde(default)]
    app: Vec<RawApp>,
}

pub fn default_path() -> PathBuf {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"));
    dir.join("mmk").join("config.toml")
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Load the config at `path`, or the embedded default if the file does not exist.
/// Returns the config and whether it came from the file.
pub fn load(path: &Path) -> Result<(Config, bool), String> {
    match std::fs::read_to_string(path) {
        Ok(src) => parse(&src).map(|c| (c, true)).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => parse(DEFAULT_CONFIG).map(|c| (c, false)),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn decode(src: &str) -> Result<RawConfig, String> {
    toml::from_str(src).map_err(|e| e.to_string())
}

pub fn parse(src: &str) -> Result<Config, String> {
    let user = decode(src)?;
    let def = decode(DEFAULT_CONFIG).expect("embedded default config must parse");
    let ds = def.switcher.unwrap_or_default();
    let us = user.switcher.unwrap_or_default();

    // Settings: user value, else the default config's value.
    let cmd = user.cmd.or(def.cmd).expect("default: cmd");
    let alt_guard = user.alt_guard.or(def.alt_guard).expect("default: alt_guard");
    let cmd_tap = user.cmd_tap.or(def.cmd_tap).expect("default: cmd_tap");
    let cmd_tap_ms = user.cmd_tap_ms.or(def.cmd_tap_ms).expect("default: cmd_tap_ms");
    let panic = user.panic.or(def.panic).expect("default: panic");
    let toggle = user.toggle.or(def.toggle).expect("default: toggle");
    let tray = user.tray.or(def.tray).expect("default: tray");
    let window_backend = user.window_backend.or(def.window_backend).expect("default: window_backend");
    let grab_wait_ms = user.grab_wait_ms.or(def.grab_wait_ms).expect("default: grab_wait_ms");
    let open_retry_ms = user.open_retry_ms.or(def.open_retry_ms).expect("default: open_retry_ms");
    let sw_enter = us.enter.or(ds.enter).expect("default: switcher.enter");
    let sw_hold = us.hold.or(ds.hold).expect("default: switcher.hold");
    let sw_keys = us.keys.or(ds.keys).expect("default: switcher.keys");

    let mut remap: Vec<u16> = (0..KEY_MAX as u16).collect();
    for (from, to) in &user.remap {
        let f = lookup_key(from).ok_or_else(|| format!("[remap] unknown key {from:?}"))?;
        let t = lookup_key(to).ok_or_else(|| format!("[remap] unknown key {to:?}"))?;
        remap[f as usize] = t;
    }

    let alt_guard = match alt_guard.as_str() {
        "" | "none" => None,
        k => Some(lookup_key(k).ok_or_else(|| format!("alt_guard: unknown key {k:?}"))?),
    };
    let cmd_tap = match cmd_tap.trim() {
        "" | "none" => None,
        a => Some(Arc::new(parse_action(a).map_err(|e| format!("cmd_tap: {e}"))?)),
    };
    let panic = parse_key_set(&panic).map_err(|e| format!("panic: {e}"))?;
    let toggle = match toggle.trim() {
        "" | "none" => None,
        t => Some(parse_trigger(t).map_err(|e| format!("toggle: {e}"))?.id()),
    };
    if !matches!(window_backend.as_str(), "auto" | "x11" | "none") {
        return Err(format!("window_backend: unknown backend {window_backend:?} (auto, x11, none)"));
    }

    let switcher = Switcher {
        enter: sw_enter
            .iter()
            .map(|t| parse_trigger(t).map(Trigger::id))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("[switcher] enter: {e}"))?,
        hold: output_mod(&sw_hold).ok_or_else(|| format!("[switcher] hold: unknown modifier {sw_hold:?}"))?,
        keys: sw_keys
            .iter()
            .map(|k| lookup_key(k).ok_or_else(|| format!("[switcher] keys: unknown key {k:?}")))
            .collect::<Result<_, _>>()?,
    };

    let base_rules = parse_rules(&user.keys, &HashMap::new()).map_err(|e| format!("[keys] {e}"))?;
    let base = Arc::new(Profile {
        name: "global".into(),
        globs: Vec::new(),
        raw: false,
        cmd: parse_mods(&cmd).map_err(|e| format!("cmd: {e}"))?,
        rules: base_rules.clone(),
    });

    let mut apps = Vec::new();
    for (i, a) in user.app.iter().enumerate() {
        let name = a.name.clone().unwrap_or_else(|| format!("app#{}", i + 1));
        let rules = parse_rules(&a.keys, &base_rules).map_err(|e| format!("[{name}] {e}"))?;
        let cmd = match &a.cmd {
            Some(c) => parse_mods(c).map_err(|e| format!("[{name}] cmd: {e}"))?,
            None => base.cmd.clone(),
        };
        apps.push(Arc::new(Profile {
            name,
            globs: a.class.iter().map(|c| c.to_lowercase()).collect(),
            raw: a.raw,
            cmd,
            rules,
        }));
    }

    Ok(Config {
        remap,
        alt_guard,
        cmd_tap,
        cmd_tap_ms,
        panic,
        toggle,
        tray,
        window_backend,
        grab_wait_ms,
        open_retry_ms,
        switcher,
        base,
        apps,
    })
}

fn parse_rules(
    keys: &HashMap<String, String>,
    inherit: &HashMap<u32, Arc<Action>>,
) -> Result<HashMap<u32, Arc<Action>>, String> {
    let mut out = inherit.clone();
    for (from, to) in keys {
        let t = parse_trigger(from)?;
        let a = parse_action(to).map_err(|e| format!("{from:?} = {e}"))?;
        out.insert(t.id(), Arc::new(a));
    }
    Ok(out)
}

/// "ctrl-shift" -> [leftctrl, leftshift]; "none" -> [].
fn parse_mods(s: &str) -> Result<Vec<u16>, String> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() || s == "none" {
        return Ok(Vec::new());
    }
    s.split('-').map(|p| output_mod(p).ok_or_else(|| format!("unknown modifier {p:?}"))).collect()
}

impl Config {
    /// Profile for a focused app. `app_id` is the Wayland app id or X11 WM_CLASS class;
    /// `instance` the X11 WM_CLASS instance (may be empty).
    pub fn match_app(&self, app_id: &str, instance: &str) -> Arc<Profile> {
        let app_id = app_id.to_lowercase();
        let instance = instance.to_lowercase();
        for p in &self.apps {
            for g in &p.globs {
                if (!app_id.is_empty() && glob(g, &app_id)) || (!instance.is_empty() && glob(g, &instance)) {
                    return p.clone();
                }
            }
        }
        self.base.clone()
    }
}

/// Minimal glob: `*` any run, `?` any single char.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let c = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(c.base.cmd, vec![KEY_LEFTCTRL]);
        assert!(c.apps.iter().any(|a| a.name == "terminal"));
        assert_eq!(c.alt_guard, None);
    }

    #[test]
    fn missing_settings_fall_back_to_default() {
        let c = parse("[keys]\n\"cmd-left\" = \"home\"\n").unwrap();
        assert_eq!(c.cmd_tap_ms, 250);
        assert_eq!(c.panic.len(), 3);
        assert!(c.apps.is_empty());
    }

    #[test]
    fn errors_name_the_rule() {
        let e = parse("[keys]\n\"cmd-left\" = \"hom\"\n").unwrap_err();
        assert!(e.contains("cmd-left") && e.contains("hom"), "{e}");
        let e = parse("bogus = 1\n").unwrap_err();
        assert!(e.contains("bogus"), "{e}");
    }

    #[test]
    fn app_matching() {
        let c = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(c.match_app("com.microsoft.VSCode", "com.microsoft.vscode").name, "vscode");
        assert_eq!(c.match_app("Gnome-terminal", "gnome-terminal-server").name, "terminal");
        assert_eq!(c.match_app("jetbrains-idea", "jetbrains-idea").name, "jetbrains");
        assert_eq!(c.match_app("com.mitchellh.ghostty", "").name, "terminal");
        assert_eq!(c.match_app("Nemo", "nemo").name, "files");
        assert_eq!(c.match_app("FreeCAD", "freecad").name, "global");
        assert_eq!(c.match_app("", "").name, "global");
    }

    #[test]
    fn globs() {
        assert!(glob("jetbrains-*", "jetbrains-idea"));
        assert!(glob("*ghostty*", "com.mitchellh.ghostty"));
        assert!(glob("st", "st"));
        assert!(!glob("st", "stterm"));
        assert!(glob("a?c", "abc"));
    }
}
