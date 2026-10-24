//! The search window (§16.5): entry, grouped results, the inline terminal and the transition
//! between them, driven by requests on the pipe from mmk.

use std::cell::RefCell;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use vte4::prelude::*;

use mmk_common::keys::{self, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT};
use mmk_common::proto::Msg;
use mmk_common::search_config::{self, SearchConfig};

use crate::codes::{self, Ids};
use crate::item::{Kind, Row};
use crate::rank::{Outcome, Searcher};
use crate::files::Files;
use crate::index::FileIndex;
use crate::{display, launch, term};

/// Fraction of the monitor's height above the box, like Spotlight.
const TOP: f64 = 0.2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Search,
    Terminal,
}

struct Session {
    term: vte4::Terminal,
    /// The cheat code that started it, if any.
    code: Option<String>,
}

struct Widgets {
    window: gtk::Window,
    panel: gtk::Overlay,
    column: gtk::Box,
    entry: gtk::Entry,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    term_box: gtk::Box,
}

struct State {
    cfg: SearchConfig,
    searcher: Searcher,
    w: Widgets,
    out: std::fs::File,
    x11: Option<display::X11>,
    composited: bool,
    dark: bool,
    /// Where the box's top edge is centered on this show (X11), kept when its width changes.
    anchor: Option<(i32, i32)>,
    /// The window that had the focus before showing, to hand it back on hide (X11).
    previous: Option<u32>,
    mode: Mode,
    visible: bool,
    /// `focused` was sent for the current show.
    focused_sent: bool,
    rows: Vec<Row>,
    row_widgets: Vec<gtk::ListBoxRow>,
    selected: usize,
    session: Option<Session>,
    /// Mode to show on the next toggle while a session lives.
    resume_mode: Mode,
    ids: Ids,
    files: Option<Files>,
    /// Bumped per query: a pending file-match poll for an older query stops.
    query_gen: u64,
    home: PathBuf,
    anim: u64,
    /// Set while the entry is changed programmatically.
    quiet: bool,
}

type St = Rc<RefCell<State>>;

pub fn run(fd: RawFd, cfg: SearchConfig) -> i32 {
    if let Err(e) = gtk::init() {
        eprintln!("mmk-search-ui: cannot start GTK: {e}");
        return 1;
    }
    let display = gdk::Display::default().expect("display");
    let composited = display.is_composited();
    let x11 = if display.type_().name() == "GdkX11Display" { display::X11::connect() } else { None };
    let out = unsafe { std::fs::File::from_raw_fd(libc::dup(fd)) };
    let w = build(&cfg, composited);
    let home = crate::home();
    let hist = crate::history::History::load(crate::state_dir().join("search-history.tsv"), cfg.ranking.half_life_days, cfg.ranking.history_max);
    let apps = crate::apps::load(&crate::app_dirs(&cfg));
    let hide = Searcher::app_execs(&apps);
    let cmds = crate::commands::load(&std::env::var("PATH").unwrap_or_default(), &hide);
    let st: St = Rc::new(RefCell::new(State {
        searcher: Searcher::new(cfg.clone(), apps, cmds, hist),
        cfg,
        w,
        out,
        x11,
        composited,
        dark: false,
        anchor: None,
        previous: None,
        mode: Mode::Search,
        visible: false,
        focused_sent: false,
        rows: Vec::new(),
        row_widgets: Vec::new(),
        selected: 0,
        session: None,
        resume_mode: Mode::Search,
        ids: Ids::load(crate::state_dir().join("codes.tsv")),
        files: None,
        query_gen: 0,
        home,
        anim: 0,
        quiet: false,
    }));
    theme(&st);
    start_files(&st);
    connect(&st);
    watch_pipe(&st, fd);
    // Realized and hidden: showing is a map plus focus, nothing to build (§16.5.3).
    let win = st.borrow().w.window.clone();
    WidgetExt::realize(&win);
    if let (Some(x), Some(xid)) = (&st.borrow().x11, xid(&win)) {
        x.set_override_redirect(xid);
    }
    glib::MainLoop::new(None, false).run();
    0
}

fn build(cfg: &SearchConfig, composited: bool) -> Widgets {
    let window = gtk::Window::builder().title("mmk search").decorated(false).resizable(false).build();
    window.add_css_class("mmk");
    if composited {
        window.add_css_class("mmk-composited");
    }
    let entry = gtk::Entry::builder().placeholder_text("Search").hexpand(true).build();
    entry.add_css_class("mmk-entry");
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).activate_on_single_click(true).build();
    list.add_css_class("mmk-list");
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).child(&list).visible(false).build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&entry);
    column.append(&scroller);
    let term_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).visible(false).build();
    term_box.add_css_class("mmk-term-box");
    let panel = gtk::Overlay::builder().child(&column).overflow(gtk::Overflow::Hidden).halign(gtk::Align::Center).valign(gtk::Align::Start).build();
    panel.add_overlay(&term_box);
    panel.set_measure_overlay(&term_box, false);
    panel.add_css_class("mmk-panel");
    window.set_child(Some(&panel));
    let w = Widgets { window, panel, column, entry, scroller, list, term_box };
    apply_sizes(&w, cfg);
    w
}

/// The window is exactly the box: the entry alone, growing downward with results or into
/// the terminal, its top edge fixed.
fn apply_sizes(w: &Widgets, cfg: &SearchConfig) {
    w.panel.set_size_request(cfg.width as i32, -1);
    w.scroller.set_max_content_height(cfg.max_rows as i32 * 52 + 3 * 30);
}

const CSS: &str = "
window.mmk { background: transparent; }
window.mmk:not(.mmk-composited) .mmk-panel { border-radius: 0; box-shadow: none; }
.mmk-panel {
  background-color: @theme_base_color;
  border-radius: RADIUSpx;
  border: 1px solid alpha(@theme_fg_color, 0.15);
}
.mmk-entry, .mmk-entry:focus-within {
  font-size: SCALEem; min-height: 56px; padding: 4px 14px;
  background: transparent; border: none; box-shadow: none; outline: none;
}
.mmk-list { background: transparent; padding: 4px 6px 8px 6px; }
.mmk-list row { border-radius: 8px; padding: 6px 10px; }
.mmk-list row:selected { background-color: @theme_selected_bg_color; color: @theme_selected_fg_color; }
.mmk-group { font-size: 0.8em; font-weight: bold; opacity: 0.55; padding: 8px 10px 2px 10px; }
.mmk-detail { font-size: 0.85em; opacity: 0.65; }
.mmk-calc { font-size: 1.6em; font-weight: bold; }
.mmk-term-box { padding: 10px 12px; }
.mmk-term { background: transparent; }
";

fn theme(st: &St) {
    let s = st.borrow();
    let provider = gtk::CssProvider::new();
    let css = CSS.replace("RADIUS", &s.cfg.terminal.radius.to_string()).replace("SCALE", &s.cfg.font_scale.to_string());
    provider.load_from_string(&css);
    gtk::style_context_add_provider_for_display(&WidgetExt::display(&s.w.window), &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    drop(s);
    let dark = match st.borrow().cfg.theme.as_str() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    };
    let dark = dark.unwrap_or_else(portal_dark);
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(dark);
    }
    st.borrow_mut().dark = dark;
}

/// The portal's `color-scheme` (1 = prefer dark), else a GTK theme name ending in "-dark".
fn portal_dark() -> bool {
    let from_portal = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).ok().and_then(|bus| {
        let args = ("org.freedesktop.appearance", "color-scheme").to_variant();
        let r = bus
            .call_sync(Some("org.freedesktop.portal.Desktop"), "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne", Some(&args), None, gio::DBusCallFlags::NONE, 300, None::<&gio::Cancellable>)
            .ok()?;
        let v = r.child_value(0).as_variant()?;
        v.get::<u32>()
    });
    match from_portal {
        Some(v) if v != 0 => v == 1,
        _ => gtk::Settings::default().and_then(|s| s.gtk_theme_name()).is_some_and(|n| n.to_lowercase().ends_with("-dark")),
    }
}

/// The file index starts in the background; until its cache or first scan is in, file rows
/// are simply missing.
fn start_files(st: &St) {
    let cfg = st.borrow().cfg.files.clone();
    if !cfg.enabled {
        st.borrow_mut().files = None;
        return;
    }
    let stale = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let s2 = stale.clone();
    let idx = FileIndex::start(&cfg, crate::cache_dir().join("files.idx"), move || s2.store(true, std::sync::atomic::Ordering::Relaxed));
    st.borrow_mut().files = Some(Files::new(idx, stale));
}

/// Apps and `$PATH` change rarely: reload them while the window is hidden, never while typing.
fn reload_sources(st: &St) {
    let dirs = crate::app_dirs(&st.borrow().cfg);
    let apps = crate::apps::load(&dirs);
    let hide = Searcher::app_execs(&apps);
    let enabled = st.borrow().cfg.commands.enabled;
    let cmds = if enabled { crate::commands::load(&std::env::var("PATH").unwrap_or_default(), &hide) } else { Vec::new() };
    let mut s = st.borrow_mut();
    s.searcher.apps = apps;
    s.searcher.commands = cmds;
}

/// Set the entry's text with no borrow held (its `changed` handler borrows the state);
/// `quiet`: without searching.
fn set_entry(st: &St, text: &str, quiet: bool) {
    let entry = st.borrow().w.entry.clone();
    st.borrow_mut().quiet = quiet;
    entry.set_text(text);
    entry.set_position(-1);
    st.borrow_mut().quiet = false;
}

fn send(st: &St, m: Msg) {
    let mut s = st.borrow_mut();
    let _ = writeln!(s.out, "{m}");
}

/// Requests from mmk, one per line.
fn watch_pipe(st: &St, fd: RawFd) {
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
    let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
    let st = st.clone();
    glib_unix::unix_fd_add_local(fd, glib::IOCondition::IN | glib::IOCondition::HUP, move |fd, _| {
        let mut chunk = [0u8; 65536];
        loop {
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n == 0 {
                // mmk is gone.
                std::process::exit(0);
            }
            if n < 0 {
                break;
            }
            buf.borrow_mut().extend_from_slice(&chunk[..n as usize]);
        }
        loop {
            let line = {
                let mut b = buf.borrow_mut();
                let Some(i) = b.iter().position(|&c| c == b'\n') else { break };
                let line: Vec<u8> = b.drain(..=i).collect();
                String::from_utf8_lossy(&line[..i]).into_owned()
            };
            if let Some(m) = Msg::parse(&line) {
                request(&st, m);
            }
        }
        glib::ControlFlow::Continue
    });
}

fn request(st: &St, m: Msg) {
    let startup = m.get("token").or(m.get("startup-id")).map(String::from);
    match m.verb.as_str() {
        "toggle" => {
            if st.borrow().visible {
                hide(st);
            } else {
                show(st, m.get("query"), startup);
            }
        }
        "show" => show(st, m.get("query"), startup),
        "hide" => hide(st),
        "terminal" => {
            let line = m.get("line").unwrap_or("").to_string();
            let cwd = st.borrow().cfg.terminal.cwd.clone();
            show(st, None, startup);
            start_session(st, term::Spec { line, cwd, shell: true, env: Vec::new() }, None);
        }
        "config" => {
            let r = m.get("toml").unwrap_or("").parse::<toml::Table>().map_err(|e| e.to_string()).and_then(search_config::from_table);
            match r {
                Ok(c) => {
                    apply_config(st, c);
                    send(st, Msg::new("ok"));
                }
                Err(e) => send(st, Msg::new("error").arg("msg", e)),
            }
        }
        "forget" => {
            let n = st.borrow_mut().searcher.history.forget(m.get("query").unwrap_or(""));
            send(st, Msg::new("ok").arg("forgotten", n.to_string()));
        }
        "status" => {
            let s = st.borrow();
            let window = if !s.visible { "hidden" } else if s.mode == Mode::Terminal { "terminal" } else { "search" };
            let idx = s.files.as_ref().map(|f| f.index.status());
            let m = Msg::new("status")
                .arg("files", idx.as_ref().map_or("off".to_string(), |i| i.entries.to_string()))
                .arg("indexing", idx.as_ref().is_some_and(|i| i.scanning).to_string())
                .arg("watches", idx.as_ref().map_or(0, |i| i.watches).to_string())
                .arg("window", window)
                .arg("session", if s.session.is_some() { "yes" } else { "no" })
                .arg("apps", s.searcher.apps.len().to_string())
                .arg("commands", s.searcher.commands.len().to_string())
                .arg("display", if s.x11.is_some() { "x11" } else { "fallback" })
                .arg("composited", s.composited.to_string());
            drop(s);
            send(st, m);
            send(st, Msg::new("ok"));
        }
        "reindex" => {
            if let Some(f) = &st.borrow().files {
                f.index.reindex();
            }
            send(st, Msg::new("ok"));
        }
        other => send(st, Msg::new("error").arg("msg", format!("unknown request {other:?}"))),
    }
}

fn apply_config(st: &St, c: SearchConfig) {
    let files_changed = st.borrow().cfg.files != c.files;
    if files_changed
        && let Some(f) = st.borrow_mut().files.take() {
            f.index.stop();
        }
    apply_config_inner(st, c);
    if files_changed {
        start_files(st);
    }
}

fn apply_config_inner(st: &St, c: SearchConfig) {
    let mut s = st.borrow_mut();
    s.searcher.history.set_params(c.ranking.half_life_days, c.ranking.history_max);
    s.searcher.cfg = c.clone();
    apply_sizes(&s.w, &c);
    s.cfg = c;
}

fn connect(st: &St) {
    let s = st.borrow();
    let w = &s.w;

    let st2 = st.clone();
    w.entry.connect_changed(move |_| {
        if !st2.borrow().quiet {
            refresh(&st2);
        }
    });
    let st2 = st.clone();
    w.entry.connect_activate(move |_| open_selected(&st2, Action::Open));
    let st2 = st.clone();
    w.list.connect_row_activated(move |_, row| {
        let i = st2.borrow().row_widgets.iter().position(|r| r == row);
        if let Some(i) = i {
            st2.borrow_mut().selected = i;
            open_selected(&st2, Action::Open);
        }
    });

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let st2 = st.clone();
    keys.connect_key_pressed(move |_, _, keycode, state| key(&st2, keycode, state));
    w.window.add_controller(keys);

    let st2 = st.clone();
    w.window.connect_is_active_notify(move |win| {
        if win.is_active() {
            report_focus(&st2);
            return;
        }
        let (visible, focused, focus_loss) = {
            let s = st2.borrow();
            let fl = if s.mode == Mode::Terminal { s.cfg.terminal.close_on_focus_loss } else { s.cfg.close_on_focus_loss };
            (s.visible, s.focused_sent, fl)
        };
        if visible && focused && focus_loss {
            hide(&st2);
        }
    });
    let st2 = st.clone();
    w.window.connect_close_request(move |_| {
        hide(&st2);
        glib::Propagation::Stop
    });

}

/// `focused` once per show: mmk then replays the keys typed meanwhile (§16.5.3).
fn report_focus(st: &St) {
    let send_it = {
        let mut s = st.borrow_mut();
        let ok = s.visible && !s.focused_sent && s.w.window.is_active();
        if ok {
            s.focused_sent = true;
        }
        ok
    };
    if send_it {
        send(st, Msg::new("focused"));
    }
}

fn show(st: &St, query: Option<&str>, startup: Option<String>) {
    let mode = {
        let s = st.borrow();
        if s.session.is_some() && query.is_none() { s.resume_mode } else { Mode::Search }
    };
    {
        let mut s = st.borrow_mut();
        s.visible = true;
        s.focused_sent = false;
        if let Some(id) = &startup {
            s.w.window.set_startup_id(id);
        }
    }
    if mode == Mode::Terminal {
        set_mode_now(st, Mode::Terminal);
    } else {
        set_mode_now(st, Mode::Search);
        let restore = st.borrow().cfg.restore_query;
        if let Some(q) = query {
            set_entry(st, q, false);
        } else if restore {
            // The previous query, selected so typing replaces it; results come with typing.
            render(st, Vec::new(), false);
            let entry = st.borrow().w.entry.clone();
            entry.select_region(0, -1);
        } else {
            set_entry(st, "", false);
        }
    }
    // Before `focused`: mmk replays held keys with the profile this selects.
    let terminal = st.borrow().mode == Mode::Terminal;
    send(st, Msg::new("shown"));
    send(st, Msg::new("mode").arg("name", if terminal { "terminal" } else { "search" }));
    let win = st.borrow().w.window.clone();
    {
        let mut s = st.borrow_mut();
        if let Some(x) = &s.x11 {
            s.previous = x.active_window();
        }
        let width = if s.mode == Mode::Terminal { s.cfg.terminal.width } else { s.cfg.width } as i32;
        s.anchor = anchor(&s);
        drop(s);
        place(st, width);
    }
    win.present();
    take_focus(st);
    let s = st.borrow();
    let focus_term = s.mode == Mode::Terminal;
    let term = s.session.as_ref().map(|x| x.term.clone());
    drop(s);
    if focus_term && let Some(t) = term {
        t.grab_focus();
    } else {
        let entry = st.borrow().w.entry.clone();
        entry.grab_focus();
        // Focus selects the entry's text: keep that only for a restored query.
        if query.is_some() {
            entry.set_position(-1);
        }
    }
}

fn xid(win: &gtk::Window) -> Option<u32> {
    let surface = win.surface()?;
    surface.downcast_ref::<gdk4_x11::X11Surface>().map(|s| s.xid() as u32)
}

/// The top-center point of the box on the pointer's monitor, `TOP` down its height, in X
/// pixels.
fn anchor(s: &State) -> Option<(i32, i32)> {
    let x = s.x11.as_ref()?;
    let (px, py) = x.pointer().unwrap_or((0, 0));
    let monitors = WidgetExt::display(&s.w.window).monitors();
    let mut geo = None;
    for i in 0..monitors.n_items() {
        if let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() {
            let g = m.geometry();
            let sc = m.scale_factor();
            let r = (g.x() * sc, g.y() * sc, g.width() * sc, g.height() * sc);
            if geo.is_none() || (px >= r.0 && px < r.0 + r.2 && py >= r.1 && py < r.1 + r.3) {
                geo = Some(r);
            }
        }
    }
    let (gx, gy, gw, gh) = geo?;
    Some((gx + gw / 2, gy + (gh as f64 * TOP) as i32))
}

/// Put the window, `width` wide, with its top edge centered on the anchor.
fn place(st: &St, width: i32) {
    let s = st.borrow();
    if let (Some(x), Some(xid), Some((cx, top))) = (&s.x11, xid(&s.w.window), s.anchor) {
        x.move_to(xid, cx - width * s.w.window.scale_factor() / 2, top);
    }
}

/// The window is override-redirect, so nobody hands it the keyboard: take it as soon as the
/// map has gone through.
fn take_focus(st: &St) {
    if st.borrow().x11.is_none() {
        return report_focus(st);
    }
    let st = st.clone();
    let tries = std::cell::Cell::new(0);
    glib::timeout_add_local(Duration::from_millis(2), move || {
        tries.set(tries.get() + 1);
        let s = st.borrow();
        if !s.visible {
            return glib::ControlFlow::Break;
        }
        if let (Some(x), Some(xid)) = (&s.x11, xid(&s.w.window))
            && x.viewable(xid)
        {
            x.focus(xid);
            drop(s);
            report_focus(&st);
            return glib::ControlFlow::Break;
        }
        if tries.get() > 100 { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
    });
}

fn hide(st: &St) {
    let mut s = st.borrow_mut();
    if !s.visible {
        return;
    }
    s.visible = false;
    s.resume_mode = s.mode;
    let win = s.w.window.clone();
    let previous = s.previous.take();
    drop(s);
    win.set_visible(false);
    if let (Some(x), Some(p)) = (&st.borrow().x11, previous) {
        x.activate(p);
    }
    send(st, Msg::new("hidden"));
    let st = st.clone();
    glib::timeout_add_local_once(Duration::from_millis(300), move || {
        if !st.borrow().visible {
            reload_sources(&st);
        }
    });
}

/// Switch the box without animation (showing, hiding).
fn set_mode_now(st: &St, mode: Mode) {
    let mut s = st.borrow_mut();
    s.anim += 1;
    s.mode = mode;
    let w = &s.w;
    let term = mode == Mode::Terminal;
    w.term_box.set_visible(term);
    w.term_box.set_opacity(1.0);
    w.column.set_opacity(if term { 0.0 } else { 1.0 });
    w.column.set_can_target(!term);
    w.entry.set_can_focus(!term);
    let width = if term { s.cfg.terminal.width } else { s.cfg.width } as i32;
    if term {
        w.panel.set_size_request(width, s.cfg.terminal.height as i32);
        w.scroller.set_visible(false);
    } else {
        w.panel.set_size_request(width, -1);
        w.scroller.set_visible(!s.rows.is_empty());
    }
    drop(s);
    place(st, width);
}

/// Animate the box between search and terminal (§16.6.5): results fade out first, the box
/// grows from its top edge, the entry cross-fades into the terminal's first line.
fn transition(st: &St, to: Mode) {
    let (panel, from_w, from_h, to_w, to_h, ms) = {
        let mut s = st.borrow_mut();
        s.anim += 1;
        s.mode = to;
        let w = &s.w;
        let (fw, fh) = (w.panel.width(), w.panel.height());
        let (tw, th) = if to == Mode::Terminal {
            (s.cfg.terminal.width as i32, s.cfg.terminal.height as i32)
        } else {
            let (_, nat, _, _) = w.column.measure(gtk::Orientation::Vertical, s.cfg.width as i32);
            (s.cfg.width as i32, nat.max(56))
        };
        let ms = if s.composited { s.cfg.terminal.animation_ms } else { 0 };
        w.term_box.set_visible(true);
        w.column.set_can_target(to == Mode::Search);
        w.entry.set_can_focus(to == Mode::Search);
        (w.panel.clone(), fw.max(1), fh.max(1), tw, th, ms)
    };
    let id = st.borrow().anim;
    let finish = {
        let st = st.clone();
        move || {
            if st.borrow().anim == id {
                set_mode_now(&st, to);
            }
        }
    };
    if ms == 0 {
        finish();
        return;
    }
    let st2 = st.clone();
    let start = std::cell::Cell::new(None::<i64>);
    panel.add_tick_callback(move |p, clock| {
        if st2.borrow().anim != id {
            return glib::ControlFlow::Break;
        }
        let now = clock.frame_time();
        let t0 = start.get().unwrap_or(now);
        start.set(Some(t0));
        let t = ((now - t0) as f64 / (ms as f64 * 1000.0)).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        let lerp = |a: i32, b: i32| a + ((b - a) as f64 * e).round() as i32;
        p.set_size_request(lerp(from_w, to_w), lerp(from_h, to_h));
        if from_w != to_w {
            place(&st2, lerp(from_w, to_w));
        }
        let s = st2.borrow();
        let w = &s.w;
        let toward_term = to == Mode::Terminal;
        // Results go in the first ~80 ms.
        let results = if toward_term { (1.0 - t * ms as f64 / 80.0).max(0.0) } else { e };
        w.scroller.set_opacity(results);
        let term_op = if toward_term { e } else { 1.0 - e };
        w.term_box.set_opacity(term_op);
        w.column.set_opacity(1.0 - term_op);
        drop(s);
        if t >= 1.0 {
            finish();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

fn refresh(st: &St) {
    let text = st.borrow().w.entry.text().to_string();
    let (file_rows, running) = {
        let mut s = st.borrow_mut();
        s.query_gen += 1;
        match &mut s.files {
            Some(f) if !text.trim().is_empty() => {
                f.set_query(&text);
                let running = f.tick(2);
                (f.rows(), running)
            }
            _ => (Vec::new(), false),
        }
    };
    if running {
        // Files keep matching on worker threads: poll each frame until they settle.
        let qgen = st.borrow().query_gen;
        let st2 = st.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            if st2.borrow().query_gen != qgen {
                return glib::ControlFlow::Break;
            }
            let (rows, running) = {
                let mut s = st2.borrow_mut();
                let Some(f) = &mut s.files else { return glib::ControlFlow::Break };
                let running = f.tick(2);
                (f.rows(), running)
            };
            if !running {
                show_results(&st2, &rows, true);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
    show_results(st, &file_rows, false);
}

fn show_results(st: &St, file_rows: &[Row], keep: bool) {
    let text = st.borrow().w.entry.text().to_string();
    let outcome = st.borrow_mut().searcher.search(&text, file_rows);
    match outcome {
        Outcome::Code(name) => open_code(st, &name, false),
        Outcome::Results(r) => {
            let rows: Vec<(Option<&'static str>, Row)> = r.top.into_iter().map(|r| (None, r)).chain(r.groups.into_iter().flat_map(|(g, rows)| rows.into_iter().enumerate().map(move |(i, r)| (if i == 0 { Some(g) } else { None }, r)))).collect();
            render(st, rows, keep);
        }
    }
}

fn group_title(g: &str) -> &'static str {
    match g {
        "apps" => "Applications",
        "commands" => "Commands",
        "files" => "Files and folders",
        _ => "",
    }
}

fn render(st: &St, rows: Vec<(Option<&'static str>, Row)>, keep: bool) {
    let mut s = st.borrow_mut();
    // Keep the selection on the same item when late file matches re-render the list.
    let keep = s.rows.get(s.selected).filter(|_| keep).map(|r| (r.id.clone(), r.name.clone()));
    let list = s.w.list.clone();
    list.remove_all();
    s.row_widgets.clear();
    s.rows.clear();
    for (header, row) in rows {
        if let Some(g) = header {
            let l = gtk::Label::builder().label(group_title(g)).xalign(0.0).build();
            l.add_css_class("mmk-group");
            let r = gtk::ListBoxRow::builder().child(&l).selectable(false).activatable(false).build();
            list.append(&r);
        }
        let r = gtk::ListBoxRow::builder().child(&row_widget(&row)).build();
        list.append(&r);
        s.row_widgets.push(r);
        s.rows.push(row);
    }
    let any = !s.rows.is_empty();
    if s.mode == Mode::Search {
        s.w.scroller.set_visible(any);
        s.w.scroller.set_opacity(1.0);
    }
    s.selected = keep.and_then(|k| s.rows.iter().position(|r| (&r.id, &r.name) == (&k.0, &k.1))).unwrap_or(0);
    if let Some(r) = s.row_widgets.get(s.selected) {
        list.select_row(Some(r));
    }
    s.w.scroller.vadjustment().set_value(0.0);
}

fn row_widget(r: &Row) -> gtk::Widget {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let icon = if !r.icon.is_empty() {
        gio::Icon::for_string(&r.icon).ok()
    } else if r.kind == Kind::File {
        // MIME icon from the name alone: no disk access while rendering.
        let (ty, _) = gio::content_type_guess(Some(&r.name), None::<&[u8]>);
        Some(gio::content_type_get_icon(&ty))
    } else {
        None
    };
    let img = match (&icon, r.kind) {
        (Some(i), _) => gtk::Image::from_gicon(i),
        (None, Kind::Dir) => gtk::Image::from_icon_name("folder"),
        (None, Kind::File) => gtk::Image::from_icon_name("text-x-generic"),
        (None, _) => gtk::Image::from_icon_name("system-run"),
    };
    img.set_pixel_size(32);
    b.append(&img);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let name = gtk::Label::builder().label(&r.name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    if r.kind == Kind::Calc {
        name.add_css_class("mmk-calc");
        name.set_selectable(false);
    }
    text.append(&name);
    if !r.detail.is_empty() {
        let d = gtk::Label::builder().label(&r.detail).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        d.add_css_class("mmk-detail");
        text.append(&d);
    }
    b.append(&text);
    b.upcast()
}

fn select(st: &St, i: usize) {
    let mut s = st.borrow_mut();
    if s.row_widgets.is_empty() {
        return;
    }
    let i = i.min(s.row_widgets.len() - 1);
    s.selected = i;
    let r = s.row_widgets[i].clone();
    s.w.list.select_row(Some(&r));
    // Scroll it into view without moving focus out of the entry.
    if let Some(b) = r.compute_bounds(&s.w.list) {
        let adj = s.w.scroller.vadjustment();
        let (top, bottom) = (b.y() as f64, (b.y() + b.height()) as f64);
        if top < adj.value() {
            adj.set_value(top);
        } else if bottom > adj.value() + adj.page_size() {
            adj.set_value(bottom - adj.page_size());
        }
    }
}

/// First row of the next (dir = 1) or previous (-1) group.
fn group_jump(st: &St, dir: i32) {
    let (sel, groups) = {
        let s = st.borrow();
        let g: Vec<&str> = s.rows.iter().map(|r| r.kind.group()).collect();
        let starts: Vec<usize> = (0..g.len()).filter(|&i| i <= 1 || g[i] != g[i - 1]).collect();
        (s.selected, starts)
    };
    let target = if dir > 0 { groups.iter().find(|&&i| i > sel).copied() } else { groups.iter().rev().find(|&&i| i < sel).copied() };
    if let Some(t) = target {
        select(st, t);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Reveal,
    Alt,
}

/// Physical key + modifiers -> chord in config syntax terms.
fn chord_matches(list: &[String], code: u16, mods: u8) -> bool {
    list.iter().any(|c| {
        keys::parse_chord(c).is_ok_and(|ch| {
            let want = ch.mods.iter().fold(0u8, |m, k| {
                m | match *k {
                    KEY_LEFTCTRL => 1,
                    KEY_LEFTSHIFT => 2,
                    KEY_LEFTALT => 4,
                    KEY_LEFTMETA => 8,
                    _ => 0,
                }
            });
            ch.key == code && want == mods
        })
    })
}

fn mods_of(state: gdk::ModifierType) -> u8 {
    let mut m = 0;
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        m |= 1;
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        m |= 2;
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        m |= 4;
    }
    if state.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
        m |= 8;
    }
    m
}

fn key(st: &St, keycode: u32, state: gdk::ModifierType) -> glib::Propagation {
    // GDK keycodes are evdev codes + 8 on Linux: physical positions, any layout (R16).
    let code = keycode.saturating_sub(8) as u16;
    let mods = mods_of(state);
    let (mode, k, tk) = {
        let s = st.borrow();
        (s.mode, s.cfg.keys.clone(), s.cfg.terminal.keys.clone())
    };
    let is = |l: &[String]| chord_matches(l, code, mods);
    if mode == Mode::Terminal {
        let t = st.borrow().session.as_ref().map(|s| s.term.clone());
        let Some(t) = t else { return glib::Propagation::Proceed };
        if is(&tk.copy) {
            t.copy_clipboard_format(vte4::Format::Text);
        } else if is(&tk.paste) {
            t.paste_clipboard();
        } else if is(&tk.end) {
            end_session(st);
            hide(st);
        } else if is(&tk.to_search) {
            st.borrow_mut().resume_mode = Mode::Search;
            transition(st, Mode::Search);
            st.borrow().w.entry.grab_focus();
            send(st, Msg::new("mode").arg("name", "search"));
        } else if is(&tk.fresh) {
            let code = st.borrow().session.as_ref().and_then(|s| s.code.clone());
            if let Some(c) = code {
                open_code(st, &c, true);
            }
        } else if is(&tk.zoom_in) {
            t.set_font_scale(t.font_scale() * 1.1);
        } else if is(&tk.zoom_out) {
            t.set_font_scale(t.font_scale() / 1.1);
        } else if is(&tk.zoom_reset) {
            t.set_font_scale(1.0);
        } else {
            return glib::Propagation::Proceed;
        }
        return glib::Propagation::Stop;
    }
    let sel = st.borrow().selected;
    if is(&k.close) {
        hide(st);
    } else if is(&k.next) {
        select(st, sel + 1);
    } else if is(&k.prev) {
        select(st, sel.saturating_sub(1));
    } else if is(&k.next_group) {
        group_jump(st, 1);
    } else if is(&k.prev_group) {
        group_jump(st, -1);
    } else if is(&k.open) {
        open_selected(st, Action::Open);
    } else if is(&k.reveal) {
        open_selected(st, Action::Reveal);
    } else if is(&k.alt) {
        open_selected(st, Action::Alt);
    } else if is(&k.copy) {
        if st.borrow().w.entry.selection_bounds().is_some() {
            return glib::Propagation::Proceed;
        }
        copy_selected(st);
    } else if is(&k.complete) {
        // `right` completes only with the caret at the end of the entry.
        let s = st.borrow();
        let at_end = s.w.entry.position() == s.w.entry.text().chars().count() as i32 && s.w.entry.selection_bounds().is_none();
        drop(s);
        if code == keys::lookup_key("right").unwrap_or(0) && !at_end {
            return glib::Propagation::Proceed;
        }
        complete(st);
    } else if mods == 1 && (2..=10).contains(&code) {
        // Ctrl+1..9: open the Nth row.
        select(st, (code - 2) as usize);
        open_selected(st, Action::Open);
    } else {
        return glib::Propagation::Proceed;
    }
    glib::Propagation::Stop
}

fn selected_row(st: &St) -> Option<Row> {
    let s = st.borrow();
    s.rows.get(s.selected).cloned()
}

fn complete(st: &St) {
    let Some(r) = selected_row(st) else { return };
    let text = match r.kind {
        Kind::App => r.name.clone(),
        Kind::Dir => format!("{}/", r.target),
        Kind::File | Kind::Command => r.target.clone(),
        Kind::Calc | Kind::Run => return,
    };
    set_entry(st, &text, false);
}

fn copy_selected(st: &St) {
    let Some(r) = selected_row(st) else { return };
    let text = match r.kind {
        Kind::App => r.name.clone(),
        _ => r.target.clone(),
    };
    WidgetExt::display(&st.borrow().w.window).clipboard().set_text(&text);
}

fn open_selected(st: &St, action: Action) {
    let Some(r) = selected_row(st) else { return };
    let query = st.borrow().w.entry.text().to_string();
    let ctx = WidgetExt::display(&st.borrow().w.window).app_launch_context();
    let home = st.borrow().home.clone();
    let mut record = true;
    match r.kind {
        Kind::App => {
            let (file, path) = st.borrow().searcher.apps.iter().find(|a| a.id == r.target).map(|a| (a.file.clone(), a.path.clone())).unwrap_or_default();
            if action == Action::Reveal {
                if let Some(f) = path.as_ref().or(file.as_ref()) {
                    reveal(f);
                }
            } else if let Some(p) = &path {
                if let Err(e) = launch::program(p) {
                    eprintln!("mmk-search-ui: {e}");
                }
            } else if let Some(app) = gio_unix::DesktopAppInfo::new(&r.target)
                && let Err(e) = app.launch(&[], Some(&ctx))
            {
                eprintln!("mmk-search-ui: launch {}: {e}", r.target);
            }
        }
        Kind::Command => {
            let how = {
                let s = st.borrow();
                if action == Action::Alt { s.cfg.commands.alt.clone() } else { s.cfg.commands.enter.clone() }
            };
            match how.as_str() {
                "inline" => {
                    let cwd = st.borrow().cfg.terminal.cwd.clone();
                    st.borrow_mut().searcher.history.record(&query, &r.id);
                    start_session(st, term::Spec { line: r.target.clone(), cwd, shell: true, env: Vec::new() }, None);
                    return;
                }
                "external" => {
                    let setting = st.borrow().cfg.external_terminal.clone();
                    if let Err(e) = launch::in_external_terminal(&setting, &r.target, &home) {
                        eprintln!("mmk-search-ui: {e}");
                    }
                }
                _ => {
                    if let Err(e) = launch::detached(&r.target, &home) {
                        eprintln!("mmk-search-ui: {e}");
                    }
                }
            }
        }
        Kind::File | Kind::Dir => {
            let path = PathBuf::from(&r.target);
            if action == Action::Reveal {
                reveal(&path);
            } else if path.extension().is_some_and(|e| e == "desktop")
                && let Some(app) = gio_unix::DesktopAppInfo::from_filename(&path)
            {
                // A launcher file (e.g. on the desktop): start its app.
                if let Err(e) = app.launch(&[], Some(&ctx)) {
                    eprintln!("mmk-search-ui: launch {}: {e}", path.display());
                }
            } else if launch::is_program(&path) {
                if let Err(e) = launch::program(&path) {
                    eprintln!("mmk-search-ui: {e}");
                }
            } else {
                let uri = gio::File::for_path(&path).uri();
                if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, Some(&ctx)) {
                    eprintln!("mmk-search-ui: open {}: {e}", path.display());
                }
            }
        }
        Kind::Calc => {
            WidgetExt::display(&st.borrow().w.window).clipboard().set_text(&r.target);
            record = false;
        }
        Kind::Run => {
            let cwd = st.borrow().cfg.terminal.cwd.clone();
            start_session(st, term::Spec { line: r.target.clone(), cwd, shell: true, env: Vec::new() }, None);
            return;
        }
    }
    if record && !r.id.is_empty() {
        st.borrow_mut().searcher.history.record(&query, &r.id);
    }
    hide(st);
}

/// Show `path` in the file manager (FileManager1.ShowItems), else open its folder.
fn reveal(path: &std::path::Path) {
    let uri = gio::File::for_path(path).uri().to_string();
    let parent = path.parent().map(|p| gio::File::for_path(p).uri().to_string());
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else { return };
    let args = (vec![uri], "").to_variant();
    bus.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        "ShowItems",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        2000,
        None::<&gio::Cancellable>,
        move |r| {
            if r.is_err()
                && let Some(p) = parent
            {
                let _ = gio::AppInfo::launch_default_for_uri(&p, None::<&gio::AppLaunchContext>);
            }
        },
    );
}

/// A cheat code: bring its session back, or start it (`fresh`: with a new id).
fn open_code(st: &St, name: &str, fresh: bool) {
    let code = st.borrow().cfg.codes.get(name).cloned();
    let Some(code) = code else { return };
    let running = st.borrow().session.as_ref().is_some_and(|s| s.code.as_deref() == Some(name));
    // The code is not a query: clear it, and what its first characters matched, without
    // searching.
    set_entry(st, "", true);
    render(st, Vec::new(), false);
    if running && !fresh {
        st.borrow_mut().resume_mode = Mode::Terminal;
        transition(st, Mode::Terminal);
        if let Some(s) = &st.borrow().session {
            s.term.grab_focus();
        }
        send(st, Msg::new("mode").arg("name", "terminal"));
        return;
    }
    // TODO(§16.6.6): cwd = "ask" shows a folder picker; until then it means home.
    let cwd = if code.cwd == "ask" { "~".to_string() } else { code.cwd.clone() };
    let home = st.borrow().home.clone();
    let (line, uuid) = codes::line_for(&st.borrow().ids, name, &code, &cwd, fresh, &home);
    st.borrow_mut().ids.set(name, &cwd, &uuid);
    let mut env: Vec<(String, String)> = code.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    env.extend(term::env_from_files(&code.env_files, &home));
    start_session(st, term::Spec { line, cwd, shell: code.shell, env }, Some(name.to_string()));
}

fn end_session(st: &St) {
    let s = st.borrow_mut().session.take();
    if let Some(s) = s {
        // Dropping the widget closes the pty: the shell gets SIGHUP.
        s.term.unparent();
    }
    let mut s = st.borrow_mut();
    s.resume_mode = Mode::Search;
}

fn start_session(st: &St, spec: term::Spec, code: Option<String>) {
    end_session(st);
    let (cfg, dark, home) = {
        let s = st.borrow();
        (s.cfg.terminal.clone(), s.dark, s.home.clone())
    };
    let t = term::new_terminal(&cfg, dark);
    let st2 = st.clone();
    let t2 = t.clone();
    t.connect_child_exited(move |_, _| {
        let ours = st2.borrow().session.as_ref().is_some_and(|s| s.term == t2);
        if !ours {
            return;
        }
        end_session(&st2);
        let (visible, close) = {
            let s = st2.borrow();
            (s.visible, s.cfg.terminal.on_exit == "close")
        };
        if close {
            set_mode_now(&st2, Mode::Search);
            set_entry(&st2, "", true);
            render(&st2, Vec::new(), false);
            hide(&st2);
        } else if visible {
            transition(&st2, Mode::Search);
            set_entry(&st2, "", false);
            let entry = st2.borrow().w.entry.clone();
            entry.grab_focus();
            send(&st2, Msg::new("mode").arg("name", "search"));
        } else {
            set_mode_now(&st2, Mode::Search);
        }
    });
    {
        let mut s = st.borrow_mut();
        s.w.term_box.append(&t);
        s.session = Some(Session { term: t.clone(), code });
        s.resume_mode = Mode::Terminal;
    }
    term::spawn(&t, &spec, &home, |e| eprintln!("mmk-search-ui: terminal: {e}"));
    transition(st, Mode::Terminal);
    t.grab_focus();
    // Focus may land before the widget is mapped; retry once it is.
    let t3 = t.clone();
    glib::timeout_add_local_once(Duration::from_millis(30), move || {
        t3.grab_focus();
    });
    send(st, Msg::new("mode").arg("name", "terminal"));
}
