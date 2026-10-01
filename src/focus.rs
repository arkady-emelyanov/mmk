//! Focus tracking backends. Backends run on their own thread and push
//! focus changes; nothing here is ever called on a key press.

use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// The focused window, as far as profile matching is concerned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Focus {
    /// Wayland app id, or X11 WM_CLASS class.
    pub app_id: String,
    /// X11 WM_CLASS instance; empty on Wayland.
    pub instance: String,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub trait FocusBackend: Send {
    fn name(&self) -> &'static str;
    /// Blocking loop: call `sink` on every focus change. Returns on error.
    fn run(&mut self, sink: &mut dyn FnMut(Focus)) -> Result<(), BoxError>;
}

/// Pick the backend for this session. `None` means: no focus tracking (global profile).
pub fn select(setting: &str) -> Option<Box<dyn FocusBackend>> {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    match setting {
        "none" => None,
        "x11" => Some(Box::new(X11)),
        _ if session == "wayland" => {
            eprintln!("mmk: Wayland focus tracking is not implemented yet; using the global profile");
            None
        }
        _ if std::env::var_os("DISPLAY").is_some() || session == "x11" => Some(Box::new(X11)),
        _ => None,
    }
}

/// Run `backend` on its own thread, restarting it on failure, sending changes to `sink`.
pub fn spawn(mut backend: Box<dyn FocusBackend>, sink: impl Fn(Focus) + Send + 'static) {
    thread::spawn(move || {
        let mut last: Option<Focus> = None;
        let mut reported = false;
        loop {
            let r = backend.run(&mut |f| {
                if last.as_ref() != Some(&f) {
                    last = Some(f.clone());
                    sink(f);
                }
            });
            if let Err(e) = r {
                if !reported {
                    eprintln!("mmk: focus backend {}: {e}; retrying", backend.name());
                    reported = true;
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Any X11 window manager that maintains `_NET_ACTIVE_WINDOW` (EWMH).
pub struct X11;

impl FocusBackend for X11 {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn run(&mut self, sink: &mut dyn FnMut(Focus)) -> Result<(), BoxError> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let active = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?.reply()?.atom;
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE))?;
        conn.flush()?;
        sink(current(&conn, root, active)?);
        loop {
            if let Event::PropertyNotify(e) = conn.wait_for_event()? {
                if e.atom == active {
                    sink(current(&conn, root, active)?);
                }
            }
        }
    }
}

fn current(conn: &RustConnection, root: Window, active: u32) -> Result<Focus, BoxError> {
    let win = conn
        .get_property(false, root, active, AtomEnum::WINDOW, 0, 1)?
        .reply()?
        .value32()
        .and_then(|mut v| v.next())
        .unwrap_or(0);
    if win == 0 {
        return Ok(Focus::default());
    }
    // The window may already be gone; treat errors as "no class".
    let Ok(reply) = conn.get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?.reply() else {
        return Ok(Focus::default());
    };
    let mut parts = reply.value.split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned());
    let instance = parts.next().unwrap_or_default();
    let app_id = parts.next().unwrap_or_default();
    Ok(Focus { app_id, instance })
}

/// One-shot read of the focused X11 window (for `doctor` / `install`).
pub fn current_x11() -> Option<Focus> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let active = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
    current(&conn, root, active).ok()
}
