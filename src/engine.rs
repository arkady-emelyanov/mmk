//! The remapping state machine. Pure: key events in, key events out.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::config::{Action, Config, Profile, SearchReq};
use crate::keys::*;

/// Where emitted key events go (the uinput device, or a recorder in tests).
pub trait Sink {
    fn key(&mut self, code: u16, value: i32);
}

/// Makes characters typeable that the keyboard layout lacks (see typist.rs).
pub trait Typist: Send {
    /// Map `chars` ahead of use; called on every config load.
    fn prepare(&mut self, chars: &[char]);
    /// The key to tap for `c`, and whether Shift must be held. `None`: not typeable this way.
    fn key(&mut self, c: char) -> Option<(u16, bool)>;
    /// The key to tap for `c`, typed on a US layout by `key`, so that it types `c` in every
    /// layout. `None`: `key` itself does (it types the same in every layout), or no key can.
    fn layout_proof(&mut self, key: u16, c: char) -> Option<(u16, bool)>;
    /// The keymap changed under us: put the characters back.
    fn refresh(&mut self);
    /// Select a layout group that types Latin letters, if the active one doesn't; `false`
    /// goes back to the group it replaced.
    fn latin(&mut self, on: bool);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Exit,
    Toggled(bool),
}

/// Capacity of the hold buffer (§16.14); a full buffer ends the hold.
pub const HOLD_CAP: usize = 64;

/// A search request for the supervisor. `hold`: the engine is holding keys until it gets
/// `search_focused`, `search_hidden` or `search_timeout(hold)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCall {
    pub req: SearchReq,
    pub hold: Option<u32>,
}

/// The search window's state as last reported by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Search,
    Terminal,
}

/// Physical key events typed while the search window is being shown.
struct Hold {
    active: bool,
    id: u32,
    buf: [(u16, i32); HOLD_CAP],
    len: usize,
    /// Physical key state as of the last buffered event (for the panic chord).
    down: Vec<bool>,
}

/// What a held physical key is currently producing.
#[derive(Clone)]
enum Active {
    Key(u16),
    Seq(Arc<Action>),
}

pub struct Engine {
    cfg: Arc<Config>,
    /// The profile in effect: the tracked one, or the search override.
    profile: Arc<Profile>,
    /// The profile of the focused app, as the focus tracker reports it.
    tracked: Arc<Profile>,
    /// The search window is shown: the profile follows `search_mode`, not the focus tracker.
    search_shown: bool,
    search_mode: SearchMode,
    hold: Hold,
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
    /// Search requests go here (to the supervisor); never blocks.
    pub search: Box<dyn FnMut(SearchCall) + Send>,
    typist: Option<Box<dyn Typist>>,
    /// The typist has a Latin layout group selected, until Cmd goes up.
    latin: bool,
    pub verbose: bool,
}

impl Engine {
    pub fn new(cfg: Arc<Config>) -> Self {
        Engine {
            profile: cfg.base.clone(),
            tracked: cfg.base.clone(),
            search_shown: false,
            search_mode: SearchMode::Search,
            hold: Hold { active: false, id: 0, buf: [(0, 0); HOLD_CAP], len: 0, down: vec![false; KEY_MAX] },
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
            search: Box::new(|_| {}),
            typist: None,
            latin: false,
            verbose: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
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
        if self.verbose && p.name != self.tracked.name {
            eprintln!("focus: {app_id:?} ({instance:?}) -> profile {}", p.name);
        }
        self.tracked = p;
        self.focus = key;
        self.apply_profile();
    }

    /// The tracked profile, unless the search window overrides it (§16.14).
    fn apply_profile(&mut self) {
        self.profile = if !self.search_shown {
            self.tracked.clone()
        } else if self.search_mode == SearchMode::Terminal {
            self.cfg.profile_named(&self.cfg.search.terminal_profile).unwrap_or(&self.cfg.base).clone()
        } else {
            self.cfg.base.clone()
        };
    }

    /// Whether keys are being held for the search window.
    #[cfg(test)]
    pub fn holding(&self) -> bool {
        self.hold.active
    }

    /// The UI showed its window.
    pub fn search_shown(&mut self) {
        self.search_shown = true;
        self.apply_profile();
    }

    /// The window has keyboard focus: replay held keys into it.
    pub fn search_focused(&mut self, out: &mut dyn Sink) -> Outcome {
        self.search_shown();
        self.end_hold(out)
    }

    /// The window was hidden (or its process died): back to the tracked profile.
    pub fn search_hidden(&mut self, out: &mut dyn Sink) -> Outcome {
        self.search_shown = false;
        self.apply_profile();
        self.end_hold(out)
    }

    /// The UI process is gone: drop the override and its mode.
    pub fn search_gone(&mut self, out: &mut dyn Sink) -> Outcome {
        self.search_mode = SearchMode::Search;
        self.search_hidden(out)
    }

    /// The UI switched between search and the inline terminal.
    pub fn set_search_mode(&mut self, mode: SearchMode) {
        self.search_mode = mode;
        self.apply_profile();
    }

    /// `hold_ms` passed since the request that started hold `id`.
    pub fn search_timeout(&mut self, out: &mut dyn Sink, id: u32) -> Outcome {
        if self.hold.active && self.hold.id == id { self.end_hold(out) } else { Outcome::Continue }
    }

    fn request_search(&mut self, req: SearchReq) {
        // Opening the window: hold what is typed next until it has focus. Keys already
        // down are not held (their releases pass, see `hold_event`).
        let hold = if req != SearchReq::Hide && !self.search_shown && !self.hold.active {
            self.hold.id = self.hold.id.wrapping_add(1);
            self.hold.active = true;
            self.hold.len = 0;
            self.hold.down.copy_from_slice(&self.held);
            Some(self.hold.id)
        } else {
            None
        };
        if req == SearchReq::Terminal {
            // The UI reports `mode terminal` after `focused`; the replay must already use it.
            self.search_mode = SearchMode::Terminal;
        }
        if self.verbose {
            eprintln!("search: {} (hold {hold:?})", req.verb());
        }
        (self.search)(SearchCall { req, hold });
    }

    /// Stop holding and replay the held events through the normal pipeline.
    fn end_hold(&mut self, out: &mut dyn Sink) -> Outcome {
        if !self.hold.active {
            return Outcome::Continue;
        }
        self.hold.active = false;
        let (buf, len) = (self.hold.buf, self.hold.len);
        self.hold.len = 0;
        let mut result = Outcome::Continue;
        for &(code, value) in &buf[..len] {
            // A replayed search action may start a new hold: the rest goes into it.
            match self.handle(out, code, value) {
                Outcome::Exit => return Outcome::Exit,
                Outcome::Continue => {}
                o => result = o,
            }
        }
        result
    }

    /// Buffer a physical event during a hold. `None`: handle it live instead.
    fn hold_event(&mut self, out: &mut dyn Sink, code: u16, value: i32) -> Option<Outcome> {
        let c = self.cfg.remap[code as usize] as usize;
        if value == 2 {
            return Some(Outcome::Continue); // repeats: dropped, the replayed press stands for them
        }
        if value == 0 && self.hold.len == 0 {
            // Down since before the trigger: release it now. After a buffered event it has
            // to wait its turn, or the replay would see the modifiers change under it.
            self.hold.down[c] = false;
            return None;
        }
        if self.hold.len == HOLD_CAP {
            // Full: replay what we have, then this event (a replayed search action may have
            // started a new hold that takes it).
            return Some(match self.end_hold(out) {
                Outcome::Exit => Outcome::Exit,
                o => match self.handle(out, code, value) {
                    Outcome::Continue => o,
                    o2 => o2,
                },
            });
        }
        self.hold.down[c] = value == 1;
        if value == 1 && !self.cfg.panic.is_empty() && self.cfg.panic.iter().all(|k| self.hold.down[*k as usize]) {
            // Never held back: the panic chord exits at once.
            self.hold.active = false;
            self.hold.len = 0;
            self.release_all(out);
            return Some(Outcome::Exit);
        }
        self.hold.buf[self.hold.len] = (code, value);
        self.hold.len += 1;
        Some(Outcome::Continue)
    }

    pub fn set_typist(&mut self, mut t: Box<dyn Typist>) {
        t.prepare(&self.cfg.text_chars());
        self.typist = Some(t);
    }

    pub fn keymap_changed(&mut self) {
        if let Some(t) = &mut self.typist {
            t.refresh();
        }
    }

    pub fn set_config(&mut self, cfg: Arc<Config>) {
        if let Some(t) = &mut self.typist {
            t.prepare(&cfg.text_chars());
        }
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
        self.leave_latin();
    }

    fn leave_latin(&mut self) {
        if self.latin {
            self.latin = false;
            if let Some(t) = self.typist.as_mut() {
                t.latin(false);
            }
        }
    }

    pub fn handle(&mut self, out: &mut dyn Sink, code: u16, value: i32) -> Outcome {
        if code as usize >= KEY_MAX || !(0..=2).contains(&value) {
            return Outcome::Continue;
        }
        if self.hold.active
            && let Some(o) = self.hold_event(out, code, value)
        {
            return o;
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
        if !self.cmd_held() {
            self.leave_latin();
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
                        let proof = self.layout_proof(ch);
                        let ch = proof.as_ref().unwrap_or(ch);
                        self.sync(out, &ch.mods, true);
                        self.press(out, ch.key);
                        self.active[c] = Some(Active::Key(ch.key));
                    }
                    Action::Tap(_) | Action::Text(_) => {
                        self.run(out, &action);
                        self.active[c] = Some(Active::Seq(action));
                    }
                    Action::Exec(_) | Action::Search(_) => self.run(out, &action),
                    Action::None => {}
                }
            }
            None => {
                // Until Cmd goes up: the group mustn't change before the X server has read
                // the key from the virtual keyboard.
                if self.profile.cmd_latin
                    && self.cmd_held()
                    && !self.latin
                    && us_char(code, false).is_some()
                    && let Some(t) = self.typist.as_mut()
                {
                    t.latin(true);
                    self.latin = true;
                }
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
                let proof = self.layout_proof(ch);
                let ch = proof.as_ref().unwrap_or(ch);
                self.sync(out, &ch.mods, true);
                out.key(ch.key, 1);
                self.after_press(ch.key);
                out.key(ch.key, 0);
            }
            Action::Tap(chords) => {
                for ch in chords {
                    if ch.key != 0 {
                        let proof = self.layout_proof(ch);
                        let ch = proof.as_ref().unwrap_or(ch);
                        self.sync(out, &ch.mods, true);
                        self.tap(out, ch.key);
                    } else {
                        // Modifier-only tap, e.g. "super": a deliberate lone modifier, unguarded.
                        self.sync(out, &ch.mods, false);
                        for m in &ch.mods {
                            self.release(out, *m, false);
                        }
                    }
                }
            }
            Action::Text(text) => {
                // Text goes out without modifiers. Releasing a bare Alt (Opt held alone) would
                // be a lone Alt tap that focuses menu bars, so press Ctrl first to cover it,
                // unless alt_guard does that.
                if self.cfg.alt_guard.is_none() && self.alt_bare() {
                    self.press(out, KEY_LEFTCTRL);
                }
                for c in text.chars() {
                    match self.typist.as_mut().and_then(|t| t.key(c)) {
                        Some((code, shift)) => {
                            self.sync(out, if shift { &[KEY_LEFTSHIFT] } else { &[] }, false);
                            self.tap(out, code);
                        }
                        None => self.type_hex(out, c),
                    }
                }
                self.sync(out, &[], false);
            }
            Action::Exec(cmd) => (self.exec)(cmd),
            Action::Search(req) => self.request_search(*req),
            Action::None => {}
        }
    }

    /// With the profile's `us_keys`, `ch` with its key replaced by one that types the key's US
    /// character in any layout; `None` to send `ch` as it is.
    fn layout_proof(&mut self, ch: &Chord) -> Option<Chord> {
        if !self.profile.us_keys {
            return None;
        }
        let shift = ch.mods.iter().any(|m| class_of(*m) == M_SHIFT);
        let c = us_char(ch.key, shift)?;
        let (key, shift) = self.typist.as_mut()?.layout_proof(ch.key, c)?;
        let mut mods: Vec<u16> = ch.mods.iter().copied().filter(|m| class_of(*m) != M_SHIFT).collect();
        if shift {
            mods.push(KEY_LEFTSHIFT);
        }
        Some(Chord { mods, key })
    }

    /// Ctrl+Shift+U, the code point in hex, Space: Unicode input in GTK and IBus.
    fn type_hex(&mut self, out: &mut dyn Sink, c: char) {
        self.sync(out, &[KEY_LEFTCTRL, KEY_LEFTSHIFT], false);
        self.tap(out, KEY_U);
        self.sync(out, &[], false);
        for d in format!("{:x}", c as u32).chars() {
            self.tap(out, lookup_key(d.encode_utf8(&mut [0; 4])).expect("hex digit key"));
        }
        self.tap(out, KEY_SPACE);
    }

    fn tap(&mut self, out: &mut dyn Sink, code: u16) {
        if self.emitted[code as usize] {
            self.release(out, code, false);
        }
        out.key(code, 1);
        self.after_press(code);
        out.key(code, 0);
    }

    /// An Alt is down on the output with no other key pressed since.
    fn alt_bare(&self) -> bool {
        [KEY_LEFTALT, KEY_RIGHTALT].iter().enumerate().any(|(i, a)| self.emitted[*a as usize] && self.bare[i])
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
    fn terminal_cmd_k_is_ctrl_k() {
        let mut t = T::new("gnome-terminal-server");
        assert_eq!(t.feed("+leftmeta +k -k -leftmeta"), "+leftctrl +k -k -leftctrl");
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
        // Opt+Left lifts Alt; a following Opt+e passthrough re-presses Alt just before x.
        let s = t.feed("+leftalt +left -left +e -e -leftalt");
        assert_eq!(s, "+leftalt +leftctrl -leftalt +left -left +leftalt -leftctrl +e -e -leftalt");
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

    /// Maps characters to keys like the X11 typist: one per level of keys f13, f14, ...
    struct FakeTypist(Vec<char>);

    impl Typist for FakeTypist {
        fn prepare(&mut self, chars: &[char]) {
            self.0 = chars.to_vec();
        }
        fn key(&mut self, c: char) -> Option<(u16, bool)> {
            let i = self.0.iter().position(|x| *x == c)?;
            Some((lookup_key("f13").unwrap() + i as u16 / 2, i % 2 == 1))
        }
        /// Every letter varies across layouts; lowercase ones go on f20, uppercase on f21.
        fn layout_proof(&mut self, _key: u16, c: char) -> Option<(u16, bool)> {
            c.is_ascii_alphabetic().then(|| (lookup_key("f20").unwrap() + c.is_ascii_uppercase() as u16, false))
        }
        fn refresh(&mut self) {}
        fn latin(&mut self, _on: bool) {}
    }

    /// Records `latin` calls.
    struct LatinTypist(std::sync::Arc<std::sync::Mutex<Vec<bool>>>);

    impl Typist for LatinTypist {
        fn prepare(&mut self, _chars: &[char]) {}
        fn key(&mut self, _c: char) -> Option<(u16, bool)> {
            None
        }
        fn layout_proof(&mut self, _key: u16, _c: char) -> Option<(u16, bool)> {
            None
        }
        fn refresh(&mut self) {}
        fn latin(&mut self, on: bool) {
            self.0.lock().unwrap().push(on);
        }
    }

    #[test]
    fn cmd_latin_spans_the_cmd_hold() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut t = T::with("cmd_latin = true\n[keys]\n\"cmd-left\" = \"home\"\n", "");
        t.e.set_typist(Box::new(LatinTypist(calls.clone())));
        // Keys only, on before the chord and once however many chords, off with Cmd.
        assert_eq!(t.feed("+leftmeta +c -c +v -v"), "+leftctrl +c -c +v -v");
        assert_eq!(*calls.lock().unwrap(), [true]);
        assert_eq!(t.feed("-leftmeta"), "-leftctrl");
        assert_eq!(*calls.lock().unwrap(), [true, false]);
        // Not for rules, plain typing or non-character keys.
        calls.lock().unwrap().clear();
        t.feed("+c -c +leftmeta +left -left +f4 -f4 -leftmeta");
        assert!(calls.lock().unwrap().is_empty());
        // Off in other profiles; a stuck hold ends with release_all.
        let mut t = T::with("", "");
        t.e.set_typist(Box::new(LatinTypist(calls.clone())));
        t.feed("+leftmeta +c -c -leftmeta");
        assert!(calls.lock().unwrap().is_empty());
        let mut t = T::with("cmd_latin = true\n", "");
        t.e.set_typist(Box::new(LatinTypist(calls.clone())));
        t.feed("+leftmeta +c");
        t.e.release_all(&mut Rec::default());
        assert_eq!(*calls.lock().unwrap(), [true, false]);
    }

    #[test]
    fn text_through_typist() {
        let mut t = T::with("[keys]\n\"opt--\" = \"text:–\"\n\"opt-shift--\" = \"text:—\"\n", "");
        t.e.set_typist(Box::new(FakeTypist(Vec::new())));
        // text:– and text:— sort to f13 (plain) and f13 with Shift
        assert_eq!(t.feed("+leftalt +minus"), "+leftalt +leftctrl -leftctrl -leftalt +f13 -f13");
        assert_eq!(t.feed("=minus -minus"), "+f13 -f13");
        assert_eq!(t.feed("+leftshift +minus -minus -leftshift -leftalt"), "+leftshift +f13 -f13 -leftshift");
        // Alt pressed last is bare even with Shift down
        let s = t.feed("+leftshift +leftalt +minus -minus -leftalt -leftshift");
        assert_eq!(s, "+leftshift +leftalt +leftctrl -leftctrl -leftalt +f13 -f13 -leftshift");
        t.assert_clean();
        t.assert_no_lone_alt();
    }

    #[test]
    fn us_keys_are_layout_proof() {
        let cfg = "[keys]\n\"opt-left\" = \"alt-b\"\n\"opt-right\" = \"alt-shift-f\"\n\"opt-up\" = \"ctrl-u ctrl-up\"\n\"opt-dot\" = \"alt-dot\"\n";
        let mut t = T::with(&format!("us_keys = true\n{cfg}"), "");
        t.e.set_typist(Box::new(FakeTypist(Vec::new())));
        assert_eq!(t.feed("+leftalt +left =left -left"), "+leftalt +f20 =f20 -f20");
        assert_eq!(t.feed("+right -right"), "+f21 -f21");
        assert_eq!(t.feed("+up -up"), "+leftctrl -leftalt +f20 -f20 +up -up");
        // dot types "." in every layout (per the fake typist): sent as it is
        assert_eq!(t.feed("+dot -dot -leftalt"), "+leftalt -leftctrl +dot -dot -leftalt");
        t.assert_clean();
        t.assert_no_lone_alt();
        // Off by default
        let mut t = T::with(cfg, "");
        t.e.set_typist(Box::new(FakeTypist(Vec::new())));
        assert_eq!(t.feed("+leftalt +left -left -leftalt"), "+leftalt +b -b -leftalt");
    }

    #[test]
    fn text_without_typist_uses_unicode_input() {
        let mut t = T::with("[keys]\n\"opt-shift--\" = \"text:—\"\n", "");
        let s = t.feed("+leftshift +leftalt +minus -minus -leftalt -leftshift");
        assert_eq!(s, "+leftshift +leftalt +leftctrl -leftalt +u -u -leftctrl -leftshift +2 -2 +0 -0 +1 -1 +4 -4 +space -space");
        t.assert_clean();
        t.assert_no_lone_alt();
    }

    #[test]
    fn remap_applies_first() {
        let mut t = T::with("[remap]\ncapslock = \"esc\"\n", "");
        assert_eq!(t.feed("+capslock -capslock"), "+esc -esc");
    }

    const SEARCH_CFG: &str = "[keys]\n\"cmd-shift-space\" = \"search\"\n\"cmd-shift-enter\" = \"search:terminal\"\n\"cmd-left\" = \"home\"\n[[app]]\nname = \"terminal\"\nclass = [\"kitty\"]\ncmd = \"ctrl-shift\"\n";

    /// An engine whose search requests are recorded.
    fn search_engine(app: &str) -> (T, Arc<std::sync::Mutex<Vec<SearchCall>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut t = T::with(SEARCH_CFG, app);
        let c = calls.clone();
        t.e.search = Box::new(move |call| c.lock().unwrap().push(call));
        (t, calls)
    }

    #[test]
    fn search_holds_keys_until_focused_with_override() {
        let (mut t, calls) = search_engine("kitty");
        assert_eq!(t.feed("+leftmeta +leftshift +space"), "+leftshift");
        assert_eq!(*calls.lock().unwrap(), vec![SearchCall { req: SearchReq::Toggle, hold: Some(1) }]);
        assert!(t.e.holding());
        // Keys down since before the trigger are released live while nothing is buffered.
        assert_eq!(t.feed("-space -leftshift"), "-leftshift");
        assert_eq!(t.feed("+c -c -leftmeta +a =a"), "");
        t.e.search_shown();
        assert_eq!(t.feed("+b"), "", "still holding until focused");
        t.e.search_focused(&mut t.out);
        // Replayed with the global profile (Cmd+C is Ctrl+C, not the terminal's Ctrl+Shift+C).
        assert_eq!(t.out.take(), "+leftctrl +c -c -leftctrl +a +b");
        assert!(!t.e.holding());
        assert_eq!(t.feed("-a -b"), "-a -b");
        t.assert_clean();
        // Focus tracking is ignored while shown, applies again once hidden.
        t.e.set_focus("kitty", "kitty");
        assert_eq!(t.e.profile_name(), "global");
        t.e.search_hidden(&mut t.out);
        assert_eq!(t.e.profile_name(), "terminal");
    }

    #[test]
    fn search_toggle_while_shown_does_not_hold() {
        let (mut t, calls) = search_engine("");
        t.e.search_shown();
        assert_eq!(t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta +x -x"), "+leftshift -leftshift +x -x");
        assert_eq!(calls.lock().unwrap()[0].hold, None);
    }

    #[test]
    fn search_terminal_mode_uses_terminal_profile() {
        let (mut t, _) = search_engine("firefox");
        t.feed("+leftmeta +leftshift +enter -enter -leftshift");
        assert_eq!(t.feed("+c -c"), "");
        t.e.search_focused(&mut t.out);
        assert_eq!(t.out.take(), "+leftctrl +leftshift +c -c");
        assert_eq!(t.e.profile_name(), "terminal");
        assert_eq!(t.feed("-leftmeta"), "-leftctrl -leftshift");
        t.e.set_search_mode(SearchMode::Search);
        assert_eq!(t.e.profile_name(), "global");
        t.e.set_search_mode(SearchMode::Terminal);
        t.e.search_gone(&mut t.out);
        assert_eq!(t.e.profile_name(), "global", "firefox: tracked profile");
        t.e.search_shown();
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "+leftctrl +c -c -leftctrl", "gone resets the mode");
        t.assert_clean();
    }

    #[test]
    fn search_hold_ends_on_timeout_or_hidden() {
        let (mut t, _) = search_engine("kitty");
        t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta");
        assert_eq!(t.feed("+x"), "");
        t.e.search_timeout(&mut t.out, 7); // a stale timer
        assert!(t.e.holding());
        t.e.search_timeout(&mut t.out, 1);
        assert_eq!(t.out.take(), "+x");
        assert_eq!(t.feed("-x"), "-x");
        // Second hold: the UI answers `hidden` (e.g. it could not show).
        t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta");
        assert_eq!(t.feed("+leftmeta +c -c -leftmeta"), "");
        t.e.search_hidden(&mut t.out);
        assert_eq!(t.out.take(), "+leftctrl +leftshift +c -c -leftctrl -leftshift", "no override: kitty");
        t.e.search_timeout(&mut t.out, 2); // late timer: nothing to do
        t.assert_clean();
    }

    #[test]
    fn search_panic_chord_never_held() {
        let (mut t, _) = search_engine("");
        t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta");
        t.feed("+leftctrl +esc +backspace");
        let enter = lookup_key("enter").unwrap();
        assert_eq!(t.e.handle(&mut t.out, enter, 1), Outcome::Exit);
        assert!(!t.e.holding());
        t.out.take();
        t.assert_clean();
        // Also with part of the chord held since before the trigger.
        let (mut t, _) = search_engine("");
        t.feed("+leftctrl +leftmeta +leftshift +space -space");
        t.feed("+esc +backspace");
        assert_eq!(t.e.handle(&mut t.out, enter, 1), Outcome::Exit);
    }

    #[test]
    fn search_hold_overflow_replays() {
        let (mut t, _) = search_engine("");
        t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta");
        let n = HOLD_CAP / 2;
        assert_eq!(t.feed(&"+a -a ".repeat(n)), "");
        assert!(t.e.holding());
        // One more: the buffer is full, everything comes out in order, then the new key.
        assert_eq!(t.feed("+b -b"), format!("{}+b -b", "+a -a ".repeat(n)));
        assert!(!t.e.holding());
        t.assert_clean();
    }

    #[test]
    fn search_every_held_press_gets_its_release() {
        let (mut t, _) = search_engine("");
        t.feed("+leftmeta +leftshift +space -space -leftshift -leftmeta");
        t.feed("+leftmeta +left +q -q");
        t.e.search_focused(&mut t.out);
        assert_eq!(t.out.take(), "+home +leftctrl +q -q");
        assert_eq!(t.feed("-left -leftmeta"), "-home -leftctrl");
        t.assert_clean();
        t.assert_no_lone_alt();
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
