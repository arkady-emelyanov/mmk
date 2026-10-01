//! The remapping state machine. Pure: key events in, key events out.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{Action, Config, Profile};
use crate::keys::*;

/// Where emitted key events go (the uinput device, or a recorder in tests).
pub trait Sink {
    fn key(&mut self, code: u16, value: i32);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
    Toggled(bool),
}

/// What a held physical key is currently producing.
#[derive(Clone)]
enum Active {
    Key(u16),
    Seq(Arc<Action>),
}

pub struct Engine {
    cfg: Arc<Config>,
    profile: Arc<Profile>,
    focus: (String, String),
    cache: HashMap<(String, String), Arc<Profile>>,
    enabled: bool,
    held: Vec<bool>,
    emitted: Vec<bool>,
    /// Alt keys pressed on the output with no other key press since.
    bare: [bool; 2],
    active: Vec<Option<Active>>,
    switcher: bool,
    cmd_down_at: Option<Instant>,
    cmd_alone: bool,
    pub exec: Box<dyn FnMut(&str) + Send>,
    pub verbose: bool,
}

impl Engine {
    pub fn new(cfg: Arc<Config>) -> Self {
        Engine {
            profile: cfg.base.clone(),
            cfg,
            focus: Default::default(),
            cache: HashMap::new(),
            enabled: true,
            held: vec![false; KEY_MAX],
            emitted: vec![false; KEY_MAX],
            bare: [false; 2],
            active: vec![None; KEY_MAX],
            switcher: false,
            cmd_down_at: None,
            cmd_alone: false,
            exec: Box::new(|_| {}),
            verbose: false,
        }
    }

    pub fn profile_name(&self) -> &str {
        &self.profile.name
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Focus changed. Matching runs here, once per change, never per key.
    pub fn set_focus(&mut self, app_id: &str, instance: &str) {
        let key = (app_id.to_string(), instance.to_string());
        let cfg = &self.cfg;
        let p = self.cache.entry(key.clone()).or_insert_with(|| cfg.match_app(app_id, instance)).clone();
        if self.verbose && p.name != self.profile.name {
            eprintln!("focus: {app_id:?} ({instance:?}) -> profile {}", p.name);
        }
        self.profile = p;
        self.focus = key;
    }

    pub fn set_config(&mut self, cfg: Arc<Config>) {
        self.cfg = cfg;
        self.cache.clear();
        let (a, i) = std::mem::take(&mut self.focus);
        self.set_focus(&a, &i);
    }

    pub fn set_enabled(&mut self, out: &mut dyn Sink, on: bool) {
        if on != self.enabled {
            self.release_all(out);
            self.enabled = on;
        }
    }

    /// Release everything held on the output and forget all key state.
    pub fn release_all(&mut self, out: &mut dyn Sink) {
        for code in 0..KEY_MAX {
            if self.emitted[code] {
                self.emitted[code] = false;
                out.key(code as u16, 0);
            }
        }
        self.held.iter_mut().for_each(|h| *h = false);
        self.active.iter_mut().for_each(|a| *a = None);
        self.bare = [false; 2];
        self.switcher = false;
        self.cmd_down_at = None;
    }

    pub fn handle(&mut self, out: &mut dyn Sink, code: u16, value: i32) -> Outcome {
        if code as usize >= KEY_MAX || !(0..=2).contains(&value) {
            return Outcome::Continue;
        }
        let code = self.cfg.remap[code as usize];
        let c = code as usize;

        if value == 1 {
            self.held[c] = true;
            if !self.cfg.panic.is_empty() && self.cfg.panic.iter().all(|k| self.held[*k as usize]) {
                self.release_all(out);
                return Outcome::Exit;
            }
            if !is_modifier(code) || class_of(code) != M_CMD {
                self.cmd_alone = false;
            }
            if !is_modifier(code) && Some(self.trigger(code).id()) == self.cfg.toggle {
                let on = !self.enabled;
                self.set_enabled(out, on);
                return Outcome::Toggled(on);
            }
        } else if value == 0 {
            self.held[c] = false;
        }

        if !self.enabled || self.profile.raw {
            self.raw(out, code, value);
        } else if is_modifier(code) {
            self.modifier(out, code, value);
        } else {
            self.key(out, code, value);
        }
        Outcome::Continue
    }

    fn trigger(&self, key: u16) -> Trigger {
        let mut mods = 0;
        for m in MOD_KEYS {
            if self.held[m as usize] {
                mods |= class_of(m);
            }
        }
        Trigger { mods, key }
    }

    fn cmd_held(&self) -> bool {
        self.held[KEY_LEFTMETA as usize] || self.held[KEY_RIGHTMETA as usize]
    }

    /// Identity passthrough (raw profile, or remapping disabled).
    fn raw(&mut self, out: &mut dyn Sink, code: u16, value: i32) {
        let c = code as usize;
        match value {
            1 => {
                self.press(out, code);
                self.active[c] = Some(Active::Key(code));
            }
            2 => {
                if self.emitted[c] {
                    out.key(code, 2);
                }
            }
            _ => match self.active[c].take() {
                Some(Active::Key(k)) => self.release(out, k, false),
                _ => self.release(out, code, false),
            },
        }
    }

    fn modifier(&mut self, out: &mut dyn Sink, code: u16, value: i32) {
        let class = class_of(code);
        match value {
            1 => {
                if class == M_CMD {
                    // Lazy: Cmd emits nothing by itself.
                    self.cmd_down_at = Some(Instant::now());
                    self.cmd_alone = true;
                } else if class != M_FN {
                    self.press(out, code);
                }
            }
            0 => {
                if class == M_CMD && !self.cmd_held() {
                    if self.cmd_alone {
                        self.cmd_tap(out);
                    }
                    self.cmd_down_at = None;
                    if self.switcher {
                        self.switcher = false;
                        let hold = self.cfg.switcher.hold;
                        self.release(out, hold, false);
                    }
                }
                if self.switcher {
                    // Keep the switcher's hold modifier; only drop this key's own output.
                    self.release(out, code, true);
                } else {
                    // Release whatever isn't backed by a held Ctrl/Opt/Shift. Modifiers a rule
                    // lifted stay lifted: nothing is re-pressed here.
                    for m in MOD_KEYS {
                        if self.emitted[m as usize] && !(self.held[m as usize] && is_eager(m)) {
                            self.release(out, m, true);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn cmd_tap(&mut self, out: &mut dyn Sink) {
        let (Some(action), Some(at)) = (self.cfg.cmd_tap.clone(), self.cmd_down_at) else { return };
        if at.elapsed().as_millis() as u64 > self.cfg.cmd_tap_ms {
            return;
        }
        self.run(out, &action);
    }

    fn key(&mut self, out: &mut dyn Sink, code: u16, value: i32) {
        let c = code as usize;
        match value {
            0 => {
                if let Some(Active::Key(k)) = self.active[c].take() {
                    self.release(out, k, false);
                }
                return;
            }
            2 => {
                match self.active[c].clone() {
                    Some(Active::Key(k)) if self.emitted[k as usize] => out.key(k, 2),
                    Some(Active::Seq(a)) => self.run(out, &a),
                    _ => {}
                }
                return;
            }
            _ => {}
        }

        let trig = self.trigger(code);
        let sw = &self.cfg.switcher;

        // Switcher mode.
        if self.switcher {
            if self.cmd_held() && sw.keys.contains(&code) {
                let desired = self.switcher_mods();
                self.sync(out, &desired, true);
                self.press(out, code);
                self.active[c] = Some(Active::Key(code));
                return;
            }
            self.switcher = false;
        }
        if trig.mods & M_CMD != 0 && sw.enter.contains(&trig.id()) {
            self.switcher = true;
            let desired = self.switcher_mods();
            self.sync(out, &desired, true);
            self.press(out, code);
            self.active[c] = Some(Active::Key(code));
            if self.verbose {
                eprintln!("key: {} -> switcher", key_name(code));
            }
            return;
        }

        match self.profile.rules.get(&trig.id()).cloned() {
            Some(action) => {
                if self.verbose {
                    eprintln!("key: rule {:?} in {} -> {:?}", trig, self.profile.name, action);
                }
                match &*action {
                    Action::Hold(ch) => {
                        self.sync(out, &ch.mods, true);
                        self.press(out, ch.key);
                        self.active[c] = Some(Active::Key(ch.key));
                    }
                    Action::Tap(_) => {
                        self.run(out, &action);
                        self.active[c] = Some(Active::Seq(action));
                    }
                    Action::Exec(_) => self.run(out, &action),
                    Action::None => {}
                }
            }
            None => {
                let desired = self.passthrough_mods();
                self.sync(out, &desired, true);
                self.press(out, code);
                self.active[c] = Some(Active::Key(code));
            }
        }
    }

    /// Modifiers for a key without a rule: held Ctrl/Opt/Shift as-is, Cmd translated.
    fn passthrough_mods(&self) -> Vec<u16> {
        let mut v = Vec::new();
        for m in MOD_KEYS {
            if !self.held[m as usize] {
                continue;
            }
            if is_eager(m) {
                v.push(m);
            } else if class_of(m) == M_CMD {
                for t in &self.profile.cmd {
                    v.push(if is_right_side(m) { right_of(*t) } else { *t });
                }
            }
        }
        v
    }

    fn switcher_mods(&self) -> Vec<u16> {
        let mut v = vec![self.cfg.switcher.hold];
        for s in [KEY_LEFTSHIFT, KEY_RIGHTSHIFT] {
            if self.held[s as usize] {
                v.push(s);
            }
        }
        v
    }

    fn run(&mut self, out: &mut dyn Sink, action: &Action) {
        match action {
            Action::Hold(ch) => {
                self.sync(out, &ch.mods, true);
                out.key(ch.key, 1);
                self.after_press(ch.key);
                out.key(ch.key, 0);
            }
            Action::Tap(chords) => {
                for ch in chords {
                    if ch.key != 0 {
                        self.sync(out, &ch.mods, true);
                        if self.emitted[ch.key as usize] {
                            self.release(out, ch.key, false);
                        }
                        out.key(ch.key, 1);
                        self.after_press(ch.key);
                        out.key(ch.key, 0);
                    } else {
                        // Modifier-only tap, e.g. "super": a deliberate lone modifier, unguarded.
                        self.sync(out, &ch.mods, false);
                        for m in &ch.mods {
                            self.release(out, *m, false);
                        }
                    }
                }
            }
            Action::Exec(cmd) => (self.exec)(cmd),
            Action::None => {}
        }
    }

    /// Make the emitted modifiers exactly `desired`.
    fn sync(&mut self, out: &mut dyn Sink, desired: &[u16], press_first: bool) {
        if press_first {
            for d in desired {
                self.press(out, *d);
            }
        }
        for m in MOD_KEYS {
            if self.emitted[m as usize] && !desired.contains(&m) {
                self.release(out, m, true);
            }
        }
        if !press_first {
            for d in desired {
                self.press(out, *d);
            }
        }
    }

    fn press(&mut self, out: &mut dyn Sink, code: u16) {
        if !self.emitted[code as usize] {
            self.emitted[code as usize] = true;
            out.key(code, 1);
            self.after_press(code);
        }
    }

    fn after_press(&mut self, code: u16) {
        for (i, a) in [KEY_LEFTALT, KEY_RIGHTALT].into_iter().enumerate() {
            self.bare[i] = a == code;
        }
    }

    /// Release `code` if emitted. With `guard`, a lone Alt gets the guard key tapped first.
    fn release(&mut self, out: &mut dyn Sink, code: u16, guard: bool) {
        if !self.emitted[code as usize] {
            return;
        }
        if guard && is_alt(code) && self.bare[(code == KEY_RIGHTALT) as usize] {
            if let Some(g) = self.cfg.alt_guard.filter(|g| !self.emitted[*g as usize]) {
                out.key(g, 1);
                self.after_press(g);
                out.key(g, 0);
            }
        }
        self.emitted[code as usize] = false;
        out.key(code, 0);
    }
}

/// Modifiers emitted as soon as they are physically pressed (never remapped).
fn is_eager(m: u16) -> bool {
    matches!(class_of(m), M_CTRL | M_OPT | M_SHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{parse, DEFAULT_CONFIG};

    #[derive(Default)]
    struct Rec(Vec<(u16, i32)>);

    impl Sink for Rec {
        fn key(&mut self, code: u16, value: i32) {
            self.0.push((code, value));
        }
    }

    impl Rec {
        fn take(&mut self) -> String {
            let s = self
                .0
                .iter()
                .map(|(c, v)| format!("{}{}", ["-", "+", "="][*v as usize], key_name(*c)))
                .collect::<Vec<_>>()
                .join(" ");
            self.0.clear();
            s
        }
    }

    struct T {
        e: Engine,
        out: Rec,
        log: Vec<(u16, i32)>,
    }

    impl T {
        fn new(app: &str) -> T {
            Self::with(DEFAULT_CONFIG, app)
        }
        fn with(cfg: &str, app: &str) -> T {
            let mut e = Engine::new(Arc::new(parse(cfg).unwrap()));
            e.set_focus(app, app);
            T { e, out: Rec::default(), log: Vec::new() }
        }
        /// Feed "+leftmeta +c -c -leftmeta" style events; returns emitted events.
        fn feed(&mut self, events: &str) -> String {
            for ev in events.split_whitespace() {
                let (v, name) = match ev.split_at(1) {
                    ("+", n) => (1, n),
                    ("-", n) => (0, n),
                    ("=", n) => (2, n),
                    _ => panic!("bad event {ev}"),
                };
                let code = lookup_key(name).unwrap_or_else(|| panic!("key {name}"));
                self.e.handle(&mut self.out, code, v);
            }
            self.log.extend(self.out.0.iter().copied());
            self.out.take()
        }
        fn assert_clean(&self) {
            assert!(self.e.emitted.iter().all(|e| !e), "keys left held");
        }
        /// No Alt press directly followed by its release.
        fn assert_no_lone_alt(&self) {
            for w in self.log.windows(2) {
                if is_alt(w[0].0) && w[0].1 == 1 {
                    assert!(!(w[1].0 == w[0].0 && w[1].1 == 0), "lone alt in {:?}", self.log);
                }
            }
        }
    }

    #[test]
    fn plain_typing() {
        let mut t = T::new("");
        assert_eq!(t.feed("+a -a +leftshift +b -b -leftshift"), "+a -a +leftshift +b -b -leftshift");
        t.assert_clean();
    }

    #[test]
    fn cmd_is_lazy_and_translates() {
        let mut t = T::new("firefox");
        assert_eq!(t.feed("+leftmeta"), "");
        assert_eq!(t.feed("+c -c"), "+leftctrl +c -c");
        assert_eq!(t.feed("-leftmeta"), "-leftctrl");
        assert_eq!(t.feed("+leftmeta -leftmeta"), "");
        t.assert_clean();
    }

    #[test]
    fn cmd_shift_passthrough() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +leftshift +z -z -leftshift -leftmeta"), "+leftshift +leftctrl +z -z -leftctrl -leftshift");
        t.assert_clean();
    }

    #[test]
    fn terminal_cmd_is_ctrl_shift() {
        let mut t = T::new("gnome-terminal-server");
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftctrl +leftshift +c -c -leftctrl -leftshift");
        assert_eq!(t.feed("+leftmeta +t -t -leftmeta"), "+leftctrl +leftshift +t -t -leftctrl -leftshift");
        assert_eq!(t.feed("+leftctrl +c -c -leftctrl"), "+leftctrl +c -c -leftctrl");
        t.assert_clean();
    }

    #[test]
    fn physical_ctrl_untouched() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftctrl"), "+leftctrl");
        assert_eq!(t.feed("+left -left +tab -tab -leftctrl"), "+left -left +tab -tab -leftctrl");
        t.assert_clean();
    }

    #[test]
    fn cmd_left_is_home_and_holds() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +left =left =left -left -leftmeta"), "+home =home =home -home");
        t.assert_clean();
    }

    #[test]
    fn opt_word_navigation_never_lone_alt() {
        let mut t = T::new("code");
        assert_eq!(t.feed("+leftalt"), "+leftalt");
        assert_eq!(t.feed("+left -left"), "+leftctrl -leftalt +left -left");
        assert_eq!(t.feed("-leftalt"), "-leftctrl");
        // with shift: Alt must not be re-pressed after the rule, or releasing Opt taps it
        let s = t.feed("+leftalt +leftshift +left -left +right -right -leftshift -leftalt");
        assert_eq!(s, "+leftalt +leftshift +leftctrl -leftalt +left -left +right -right -leftctrl -leftshift");
        t.assert_clean();
        t.assert_no_lone_alt();
    }

    #[test]
    fn opt_tap_alone_is_plain_alt() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftalt -leftalt"), "+leftalt -leftalt");
        t.assert_clean();
    }

    #[test]
    fn opt_tap_alone_guarded_when_configured() {
        let mut t = T::with("alt_guard = \"f19\"\n", "");
        assert_eq!(t.feed("+leftalt -leftalt"), "+leftalt +f19 -f19 -leftalt");
        t.assert_clean();
        t.assert_no_lone_alt();
    }

    #[test]
    fn opt_letter_passes_alt() {
        let mut t = T::new("");
        assert_eq!(t.feed("+rightalt +e -e -rightalt"), "+rightalt +e -e -rightalt");
        t.assert_clean();
    }

    #[test]
    fn lifted_alt_not_restored_then_needed() {
        let mut t = T::new("");
        // Opt+Left lifts Alt; a following Opt+x passthrough re-presses Alt just before x.
        let s = t.feed("+leftalt +left -left +x -x -leftalt");
        assert_eq!(s, "+leftalt +leftctrl -leftalt +left -left +leftalt -leftctrl +x -x -leftalt");
        t.assert_clean();
        t.assert_no_lone_alt();
    }

    #[test]
    fn sequence_cmd_backspace() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +backspace"), "+leftshift +home -home -leftshift +backspace -backspace");
        assert_eq!(t.feed("-backspace -leftmeta"), "");
        t.assert_clean();
        let mut t = T::new("kitty");
        assert_eq!(t.feed("+leftmeta +backspace -backspace -leftmeta"), "+leftctrl +u -u -leftctrl");
        t.assert_clean();
    }

    #[test]
    fn switcher_cmd_tab_arrows() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +tab -tab"), "+leftalt +tab -tab");
        assert_eq!(t.feed("+tab -tab"), "+tab -tab");
        assert_eq!(t.feed("+right -right +left -left"), "+right -right +left -left");
        assert_eq!(t.feed("+leftshift +tab -tab -leftshift"), "+leftshift +tab -tab -leftshift");
        assert_eq!(t.feed("-leftmeta"), "-leftalt");
        t.assert_clean();
        t.assert_no_lone_alt();
        // after the switcher closed, Cmd+Left is Home again
        assert_eq!(t.feed("+leftmeta +left -left -leftmeta"), "+home -home");
    }

    #[test]
    fn switcher_grave_and_exit_on_other_key() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +grave -grave"), "+leftalt +grave -grave");
        assert_eq!(t.feed("+c -c"), "+leftctrl -leftalt +c -c");
        assert_eq!(t.feed("-leftmeta"), "-leftctrl");
        t.assert_clean();
    }

    #[test]
    fn cmd_space_layout_switch() {
        let mut t = T::new("kitty");
        assert_eq!(t.feed("+leftmeta +space -space +space -space -leftmeta"), "+leftmeta +space -space +space -space -leftmeta");
        t.assert_clean();
    }

    #[test]
    fn right_cmd_uses_right_ctrl() {
        let mut t = T::new("");
        assert_eq!(t.feed("+rightmeta +s -s -rightmeta"), "+rightctrl +s -s -rightctrl");
        t.assert_clean();
    }

    #[test]
    fn raw_profile_identity() {
        let mut t = T::new("remmina");
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftmeta +c -c -leftmeta");
        assert_eq!(t.feed("+leftalt -leftalt"), "+leftalt -leftalt");
        t.assert_clean();
    }

    #[test]
    fn release_follows_original_output_after_profile_change() {
        let mut t = T::new("");
        assert_eq!(t.feed("+leftmeta +left"), "+home");
        t.e.set_focus("kitty", "kitty");
        assert_eq!(t.feed("-left -leftmeta"), "-home");
        t.assert_clean();
    }

    #[test]
    fn jetbrains_clipboard_and_profile_rules() {
        let mut t = T::new("jetbrains-idea");
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftctrl +insert -insert -leftctrl");
        assert_eq!(t.feed("+leftmeta +o -o -leftmeta"), "+leftctrl +n -n -leftctrl");
        assert_eq!(t.feed("+leftmeta +left -left -leftmeta"), "+home -home");
        t.assert_clean();
    }

    #[test]
    fn panic_chord_exits() {
        let mut t = T::new("");
        t.feed("+leftctrl +esc +backspace");
        let code = lookup_key("enter").unwrap();
        assert_eq!(t.e.handle(&mut t.out, code, 1), Outcome::Exit);
        t.out.take();
        t.assert_clean();
    }

    #[test]
    fn toggle_disables_remapping() {
        let mut t = T::with("toggle = \"cmd-opt-ctrl-k\"\n[keys]\n\"cmd-left\" = \"home\"\n", "");
        t.feed("+leftctrl +leftalt +leftmeta");
        let k = lookup_key("k").unwrap();
        assert_eq!(t.e.handle(&mut t.out, k, 1), Outcome::Toggled(false));
        t.feed("-k -leftmeta -leftalt -leftctrl");
        assert_eq!(t.feed("+leftmeta +left -left -leftmeta"), "+leftmeta +left -left -leftmeta");
        t.assert_clean();
    }

    #[test]
    fn exec_action() {
        let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut t = T::with("[keys]\n\"cmd-shift-space\" = \"exec:launcher --x\"\n", "");
        let r = ran.clone();
        t.e.exec = Box::new(move |c| r.lock().unwrap().push(c.to_string()));
        assert_eq!(t.feed("+leftmeta +leftshift +space =space -space -leftshift -leftmeta"), "+leftshift -leftshift");
        assert_eq!(*ran.lock().unwrap(), vec!["launcher --x"]);
        t.assert_clean();
    }

    #[test]
    fn cmd_tap_action() {
        let mut t = T::with("cmd_tap = \"super\"\n", "");
        assert_eq!(t.feed("+leftmeta -leftmeta"), "+leftmeta -leftmeta");
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftctrl +c -c -leftctrl");
        t.assert_clean();
    }

    #[test]
    fn cmd_tap_as_alt_for_menus() {
        let mut t = T::with("cmd_tap = \"alt\"\n", "");
        assert_eq!(t.feed("+leftmeta -leftmeta"), "+leftalt -leftalt");
        // any other key while Cmd is down cancels the tap
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftctrl +c -c -leftctrl");
        assert_eq!(t.feed("+leftmeta +leftshift -leftshift -leftmeta"), "+leftshift -leftshift");
        t.assert_clean();
    }

    #[test]
    fn remap_applies_first() {
        let mut t = T::with("[remap]\ncapslock = \"esc\"\n", "");
        assert_eq!(t.feed("+capslock -capslock"), "+esc -esc");
    }

    /// Every default rule, in every profile: no lone Alt, nothing left held.
    #[test]
    fn all_default_rules_are_clean() {
        let cfg = parse(DEFAULT_CONFIG).unwrap();
        let mut profiles = vec![cfg.base.clone()];
        profiles.extend(cfg.apps.iter().cloned());
        for p in profiles {
            for id in p.rules.keys() {
                let (mods, key) = ((id >> 16) as ModMask, (id & 0xffff) as u16);
                let app = p.globs.first().cloned().unwrap_or_default();
                let mut t = T::new(&app);
                let mut seq = Vec::new();
                for (m, k) in [(M_CTRL, "leftctrl"), (M_OPT, "leftalt"), (M_SHIFT, "leftshift"), (M_CMD, "leftmeta"), (M_FN, "fn")] {
                    if mods & m != 0 {
                        seq.push(format!("+{k}"));
                    }
                }
                let name = key_name(key);
                seq.push(format!("+{name} -{name}"));
                for (m, k) in [(M_CTRL, "leftctrl"), (M_OPT, "leftalt"), (M_SHIFT, "leftshift"), (M_CMD, "leftmeta"), (M_FN, "fn")] {
                    if mods & m != 0 {
                        seq.push(format!("-{k}"));
                    }
                }
                t.feed(&seq.join(" "));
                t.assert_clean();
                t.assert_no_lone_alt();
            }
        }
    }
}
