//! X11 window management the GTK4 API doesn't cover (§16.10, `x11` backend).
//!
//! The window is override-redirect: the WM never places, animates or re-places it, so it
//! appears exactly where it is put. In exchange it takes the keyboard focus itself on show and
//! hands it back to the previously active window on hide.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, InputFocus, MapState, StackMode};
use x11rb::rust_connection::RustConnection;

pub struct X11 {
    conn: RustConnection,
    root: u32,
}

impl X11 {
    pub fn connect() -> Option<X11> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        Some(X11 { conn, root })
    }

    fn atom(&self, name: &str) -> u32 {
        self.conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom).unwrap_or(0)
    }

    /// Before the first map: keep the WM's hands off `win`.
    pub fn set_override_redirect(&self, win: u32) {
        let _ = self.conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().override_redirect(1));
        let _ = self.conn.flush();
    }

    /// The WM's active window, to give focus back to on hide.
    pub fn active_window(&self) -> Option<u32> {
        let atom = self.atom("_NET_ACTIVE_WINDOW");
        let r = self.conn.get_property(false, self.root, atom, AtomEnum::WINDOW, 0, 1).ok()?.reply().ok()?;
        r.value32()?.next().filter(|&w| w != 0)
    }

    /// The pointer's root coordinates.
    pub fn pointer(&self) -> Option<(i32, i32)> {
        let r = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((r.root_x as i32, r.root_y as i32))
    }

    pub fn move_to(&self, win: u32, x: i32, y: i32) {
        let _ = self.conn.configure_window(win, &ConfigureWindowAux::new().x(x).y(y));
        let _ = self.conn.flush();
    }

    /// Whether the X server shows `win` (mapped, with mapped ancestors).
    pub fn viewable(&self, win: u32) -> bool {
        self.conn.get_window_attributes(win).ok().and_then(|c| c.reply().ok()).is_some_and(|a| a.map_state == MapState::VIEWABLE)
    }

    /// Raise `win` and give it the keyboard (it must be viewable).
    pub fn focus(&self, win: u32) {
        let _ = self.conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = self.conn.set_input_focus(InputFocus::PARENT, win, x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    /// Ask the WM to activate a managed window: `_NET_ACTIVE_WINDOW` with source "pager",
    /// which WMs grant without a user timestamp (V2).
    pub fn activate(&self, win: u32) {
        let active = self.atom("_NET_ACTIVE_WINDOW");
        let ev = ClientMessageEvent::new(32, win, active, [2u32, x11rb::CURRENT_TIME, 0, 0, 0]);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        let _ = self.conn.send_event(false, self.root, mask, ev);
        let _ = self.conn.flush();
    }
}
