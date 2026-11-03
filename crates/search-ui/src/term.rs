//! Inline terminal sessions (§16.6.5): a VTE widget running a shell or a code's program.

use std::path::Path;

use gtk4 as gtk;
use gtk::{gdk, gio, glib, pango};
use vte4::prelude::*;

use mmk_common::search_config::Terminal as TermCfg;

pub struct Spec {
    /// Shell line to run; empty = an interactive shell.
    pub line: String,
    pub cwd: String,
    /// Feed `line` to an interactive `$SHELL` (true) or exec it with `sh -c` (false).
    pub shell: bool,
    pub env: Vec<(String, String)>,
}

/// Resolve `$VAR`, `${VAR}` and a leading `~` in a code's env value.
pub fn expand_env(v: &str, home: &Path) -> String {
    let v = if v == "~" { home.display().to_string() } else if let Some(r) = v.strip_prefix("~/") { format!("{}/{r}", home.display()) } else { v.to_string() };
    let mut out = String::new();
    let mut it = v.chars().peekable();
    while let Some(c) = it.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = it.peek() == Some(&'{');
        if braced {
            it.next();
        }
        let mut name = String::new();
        while let Some(&n) = it.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                it.next();
            } else {
                break;
            }
        }
        if braced && it.peek() == Some(&'}') {
            it.next();
        }
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&std::env::var(&name).unwrap_or_default());
        }
    }
    out
}

/// Variables read from files at spawn (secrets); a missing or unreadable file leaves the
/// variable unset. Values are never logged.
pub fn env_from_files(files: &std::collections::BTreeMap<String, String>, home: &Path) -> Vec<(String, String)> {
    files
        .iter()
        .filter_map(|(k, f)| {
            let v = std::fs::read_to_string(expand_env(f, home)).ok()?;
            let v = v.trim();
            (!v.is_empty()).then(|| (k.clone(), v.to_string()))
        })
        .collect()
}

pub fn new_terminal(cfg: &TermCfg, dark: bool) -> vte4::Terminal {
    let t = vte4::Terminal::new();
    t.set_hexpand(true);
    t.set_vexpand(true);
    t.set_scrollback_lines(cfg.scrollback);
    t.set_font(Some(&pango::FontDescription::from_string(&monospace_font(&cfg.font))));
    t.set_clear_background(false);
    let fg = if dark { gdk::RGBA::new(0.88, 0.88, 0.88, 1.0) } else { gdk::RGBA::new(0.13, 0.13, 0.13, 1.0) };
    let palette: Vec<gdk::RGBA> = (if dark { &DARK } else { &LIGHT }).iter().map(|&c| rgb(c)).collect();
    let palette: Vec<&gdk::RGBA> = palette.iter().collect();
    t.set_colors(Some(&fg), Some(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0)), &palette);
    t.add_css_class("mmk-term");
    t
}

/// The 16 ANSI colors. VTE's default is xterm's, whose blue (#0000cd) is unreadable on a
/// dark background; these are VS Code's, readable on the box's own background.
const DARK: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
    0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
];
const LIGHT: [u32; 16] = [
    0x000000, 0xcd3131, 0x00bc00, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, //
    0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
];

fn rgb(c: u32) -> gdk::RGBA {
    let ch = |shift: u32| ((c >> shift) & 0xff) as f32 / 255.0;
    gdk::RGBA::new(ch(16), ch(8), ch(0), 1.0)
}

/// `auto`: the desktop's monospace font, else "Monospace 11".
fn monospace_font(setting: &str) -> String {
    if setting != "auto" {
        return setting.to_string();
    }
    let Some(src) = gio::SettingsSchemaSource::default() else { return "Monospace 11".into() };
    for schema in ["org.cinnamon.desktop.interface", "org.gnome.desktop.interface", "org.mate.interface"] {
        if let Some(s) = src.lookup(schema, true)
            && s.has_key("monospace-font-name")
        {
            let v = gio::Settings::new(schema).string("monospace-font-name");
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "Monospace 11".into()
}

/// Start `spec` in `t`. The shell is spawned at once; the line is fed when the shell first
/// prints (its prompt), else after a second. Fed earlier, the tty would echo it before the
/// shell's line editor shows it again (V16).
pub fn spawn(t: &vte4::Terminal, spec: &Spec, home: &Path, on_error: impl Fn(String) + 'static) {
    let shell = crate::launch::login_shell();
    let argv: Vec<String> = if spec.shell || spec.line.trim().is_empty() { vec![shell, "-i".into()] } else { vec!["/bin/sh".into(), "-c".into(), spec.line.clone()] };
    let mut env: Vec<String> = vec!["TERM=xterm-256color".into(), "COLORTERM=truecolor".into()];
    env.extend(spec.env.iter().map(|(k, v)| format!("{k}={}", expand_env(v, home))));
    let cwd = expand_env(&spec.cwd, home);
    let cwd = if Path::new(&cwd).is_dir() { cwd } else { home.display().to_string() };
    let argv_ref: Vec<&str> = argv.iter().map(String::as_str).collect();
    let env_ref: Vec<&str> = env.iter().map(String::as_str).collect();
    let feed = (spec.shell && !spec.line.trim().is_empty()).then(|| format!("{}\n", spec.line));
    let t2 = t.clone();
    t.spawn_async(
        vte4::PtyFlags::DEFAULT,
        Some(&cwd),
        &argv_ref,
        &env_ref,
        glib::SpawnFlags::SEARCH_PATH,
        || {},
        -1,
        None::<&gio::Cancellable>,
        move |r| match r {
            Ok(_) => {
                if let Some(line) = feed {
                    feed_after_prompt(&t2, line);
                }
            }
            Err(e) => on_error(e.to_string()),
        },
    );
}

/// Feed `line` once the shell's output has settled for a moment (prompt drawn; fish first
/// sends terminal queries), at most a second after spawning.
fn feed_after_prompt(t: &vte4::Terminal, line: String) {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    let pending = Rc::new(RefCell::new(Some(line)));
    let quiet = Rc::new(Cell::new(None::<glib::SourceId>));
    let handler = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
    let feed = {
        let (pending, handler, quiet, t) = (pending.clone(), handler.clone(), quiet.clone(), t.clone());
        Rc::new(move || {
            if let Some(l) = pending.borrow_mut().take() {
                t.feed_child(l.as_bytes());
                if let Some(h) = handler.take() {
                    t.disconnect(h);
                }
                if let Some(q) = quiet.take() {
                    q.remove();
                }
            }
        })
    };
    let (f2, q2) = (feed.clone(), quiet.clone());
    handler.set(Some(t.connect_contents_changed(move |_| {
        if let Some(q) = q2.take() {
            q.remove();
        }
        let (f3, q3) = (f2.clone(), q2.clone());
        q2.set(Some(glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
            q3.take();
            f3();
        })));
    })));
    glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || feed());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion() {
        let home = Path::new("/home/u");
        unsafe { std::env::set_var("MMK_T", "x") };
        assert_eq!(expand_env("~/a", home), "/home/u/a");
        assert_eq!(expand_env("$MMK_T-${MMK_T}y $", home), "x-xy $");
        let dir = std::env::temp_dir().join(format!("mmk-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tok"), "secret\n").unwrap();
        let files = [("A".to_string(), dir.join("tok").display().to_string()), ("B".to_string(), "/nonexistent".to_string())].into_iter().collect();
        assert_eq!(env_from_files(&files, home), vec![("A".to_string(), "secret".to_string())]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
