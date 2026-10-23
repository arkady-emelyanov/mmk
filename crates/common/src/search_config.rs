//! The `[search]` table of mmk's config.
//!
//! As for the rest of the config, the embedded default config is the single source of
//! defaults: the user's `[search]` table is merged over the default one key by key (tables
//! recursively), then decoded strictly. Cheat codes are the exception to "all keys present":
//! a code the user adds only needs `code` and `run`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::keys::parse_chord;

/// The default config, shared with mmk (`src/default.toml`).
pub const DEFAULT_CONFIG: &str = include_str!("../../../src/default.toml");

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    pub enabled: bool,
    pub width: u32,
    pub max_rows: u32,
    pub font_scale: f64,
    pub theme: String,
    pub accent: String,
    pub restore_query: bool,
    pub empty: String,
    pub close_on_focus_loss: bool,
    pub display: String,
    pub external_terminal: String,
    pub hold_ms: u64,
    pub keys: Keys,
    pub apps: Apps,
    pub groups: Groups,
    pub ranking: Ranking,
    pub commands: Commands,
    pub terminal: Terminal,
    #[serde(default)]
    pub codes: BTreeMap<String, Code>,
    pub files: Files,
    pub calc: Calc,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub open: Vec<String>,
    pub reveal: Vec<String>,
    pub alt: Vec<String>,
    pub copy: Vec<String>,
    pub complete: Vec<String>,
    pub close: Vec<String>,
    pub next: Vec<String>,
    pub prev: Vec<String>,
    pub next_group: Vec<String>,
    pub prev_group: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Apps {
    /// Folders whose executables (AppImages, binaries) are listed as apps.
    pub dirs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Groups {
    pub order: Vec<String>,
    pub apps: Group,
    pub commands: Group,
    pub files: Group,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ranking {
    pub half_life_days: f64,
    pub history_max: usize,
    pub weights: Weights,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Weights {
    pub apps: f64,
    pub commands: f64,
    pub files: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Commands {
    pub enabled: bool,
    pub enter: String,
    pub alt: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Terminal {
    pub enabled: bool,
    pub prefix: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub radius: u32,
    pub animation_ms: u64,
    pub cwd: String,
    pub font: String,
    pub palette: String,
    pub scrollback: i64,
    pub linger_ms: u64,
    pub on_exit: String,
    pub close_on_focus_loss: bool,
    pub keys: TerminalKeys,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TerminalKeys {
    pub copy: Vec<String>,
    pub paste: Vec<String>,
    pub end: Vec<String>,
    pub to_search: Vec<String>,
    pub fresh: Vec<String>,
    pub zoom_in: Vec<String>,
    pub zoom_out: Vec<String>,
    pub zoom_reset: Vec<String>,
}

/// A cheat code: typing `code` opens the inline terminal running `run` (empty: a shell).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Code {
    pub code: String,
    pub run: String,
    #[serde(default)]
    pub resume: String,
    #[serde(default = "home")]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub env_files: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub shell: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn home() -> String {
    "~".into()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Files {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub exclude: Vec<String>,
    pub hidden: bool,
    pub respect_ignore: bool,
    pub max_watches: usize,
    pub rescan_minutes: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Calc {
    pub enabled: bool,
}

/// The default config's `[search]` table.
pub fn default_table() -> toml::Table {
    let mut all: toml::Table = DEFAULT_CONFIG.parse().expect("embedded default config must parse");
    match all.remove("search") {
        Some(toml::Value::Table(t)) => t,
        _ => panic!("embedded default config has no [search] table"),
    }
}

/// Merge `over` into `base`: tables recursively, everything else replaced.
pub fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// The effective `[search]` table: the user's (if any) over the default.
pub fn effective_table(user: Option<toml::Table>) -> toml::Table {
    let mut t = default_table();
    if let Some(u) = user {
        merge(&mut t, u);
    }
    t
}

/// Decode and validate an effective `[search]` table (see `effective_table`).
pub fn from_table(t: toml::Table) -> Result<SearchConfig, String> {
    let c: SearchConfig = toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| format!("[search] {e}"))?;
    c.validate()?;
    Ok(c)
}

/// Parse the `[search]` table out of a whole config file.
pub fn parse(src: &str) -> Result<SearchConfig, String> {
    let mut all: toml::Table = src.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let user = match all.remove("search") {
        None => None,
        Some(toml::Value::Table(t)) => Some(t),
        Some(_) => return Err("search: expected a table ([search]); turn search off with `enabled = false` in it".into()),
    };
    from_table(effective_table(user))
}

fn one_of(name: &str, v: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&v) { Ok(()) } else { Err(format!("[search] {name}: {v:?} is not one of {}", allowed.join(", "))) }
}

fn chords(name: &str, list: &[String]) -> Result<(), String> {
    for k in list {
        parse_chord(k).map_err(|e| format!("[search] {name}: {e}"))?;
    }
    Ok(())
}

impl SearchConfig {
    fn validate(&self) -> Result<(), String> {
        one_of("theme", &self.theme, &["auto", "light", "dark"])?;
        one_of("empty", &self.empty, &["nothing", "recent"])?;
        one_of("display", &self.display, &["auto", "x11", "layer-shell", "fallback"])?;
        one_of("terminal.on_exit", &self.terminal.on_exit, &["close", "search"])?;
        one_of("commands.enter", &self.commands.enter, &["inline", "external", "detached"])?;
        one_of("commands.alt", &self.commands.alt, &["inline", "external", "detached"])?;
        for g in &self.groups.order {
            one_of("groups.order", g, &["apps", "commands", "files"])?;
        }
        let k = &self.keys;
        for (n, l) in [
            ("keys.open", &k.open),
            ("keys.reveal", &k.reveal),
            ("keys.alt", &k.alt),
            ("keys.copy", &k.copy),
            ("keys.complete", &k.complete),
            ("keys.close", &k.close),
            ("keys.next", &k.next),
            ("keys.prev", &k.prev),
            ("keys.next_group", &k.next_group),
            ("keys.prev_group", &k.prev_group),
        ] {
            chords(n, l)?;
        }
        let t = &self.terminal.keys;
        for (n, l) in [
            ("terminal.keys.copy", &t.copy),
            ("terminal.keys.paste", &t.paste),
            ("terminal.keys.end", &t.end),
            ("terminal.keys.to_search", &t.to_search),
            ("terminal.keys.fresh", &t.fresh),
            ("terminal.keys.zoom_in", &t.zoom_in),
            ("terminal.keys.zoom_out", &t.zoom_out),
            ("terminal.keys.zoom_reset", &t.zoom_reset),
        ] {
            chords(n, l)?;
        }
        if self.terminal.prefix.is_empty() {
            return Err("[search] terminal.prefix: must not be empty".into());
        }
        let codes: Vec<(&String, &Code)> = self.codes.iter().filter(|(_, c)| c.enabled).collect();
        for (name, c) in &codes {
            if c.code.is_empty() {
                return Err(format!("[search.codes.{name}] code: must not be empty"));
            }
            for (other, o) in &codes {
                if name != other && o.code.starts_with(&c.code) {
                    return Err(format!(
                        "[search.codes] {name} ({:?}) is a prefix of {other} ({:?}): {other} could never be typed",
                        c.code, o.code
                    ));
                }
            }
        }
        Ok(())
    }

    /// The enabled cheat code `text` is exactly, if any.
    pub fn code_for(&self, text: &str) -> Option<(&str, &Code)> {
        self.codes.iter().find(|(_, c)| c.enabled && c.code == text).map(|(n, c)| (n.as_str(), c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parses() {
        let c = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(c.width, 720);
        assert_eq!(c.groups.order, ["apps", "commands", "files"]);
        let (name, claude) = c.code_for("@!").unwrap();
        assert_eq!(name, "claude");
        assert!(claude.run.contains("{uuid}") && !claude.shell);
        assert_eq!(c.code_for("@#").map(|(n, c)| (n, c.run.as_str())), Some(("shell", "")));
    }

    #[test]
    fn missing_table_is_default() {
        assert_eq!(parse("").unwrap(), parse(DEFAULT_CONFIG).unwrap());
    }

    #[test]
    fn user_values_merge_over_default() {
        let c = parse("[search]\nwidth = 900\n[search.terminal]\nheight = 700\n[search.codes.top]\ncode = \"@%\"\nrun = \"htop\"\n").unwrap();
        assert_eq!((c.width, c.max_rows, c.terminal.height, c.terminal.width), (900, 9, 700, 720));
        assert_eq!(c.code_for("@%").unwrap().1.cwd, "~");
        assert!(c.code_for("@!").is_some(), "the default code stays");
        let c = parse("[search.codes.claude]\ncode = \"!@>\"\n").unwrap();
        assert!(c.code_for("@!").is_none());
        assert_eq!(c.code_for("!@>").unwrap().1.run, "claude --session-id {uuid}");
        let c = parse("[search.codes.claude]\nenabled = false\n").unwrap();
        assert!(c.code_for("@!").is_none());
    }

    #[test]
    fn errors() {
        let e = parse("[search]\nbogus = 1\n").unwrap_err();
        assert!(e.contains("bogus"), "{e}");
        let e = parse("[search]\ntheme = \"pink\"\n").unwrap_err();
        assert!(e.contains("theme"), "{e}");
        let e = parse("[search.keys]\nopen = [\"ctrl-nope\"]\n").unwrap_err();
        assert!(e.contains("keys.open") && e.contains("nope"), "{e}");
        let e = parse("[search.codes.a]\ncode = \"@\"\nrun = \"x\"\n").unwrap_err();
        assert!(e.contains("prefix"), "{e}");
        let e = parse("search = 1\n").unwrap_err();
        assert!(e.contains("table"), "{e}");
    }
}
