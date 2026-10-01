//! Tray icon: StatusNotifierItem over D-Bus via ksni.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu};

use crate::Msg;

pub struct Tray {
    pub enabled: bool,
    pub profile: String,
    pub app: String,
    pub keyboards: Vec<String>,
    pub error: Option<String>,
    pub config_path: PathBuf,
    /// Whether to offer the named symbolic icon (false: pixmap only, see `spawn`).
    named_icon: bool,
    tx: Sender<Msg>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

pub fn spawn(tx: Sender<Msg>, config_path: PathBuf) -> Option<Handle> {
    let fresh = match install_icons() {
        Ok(changed) => changed,
        Err(e) => {
            eprintln!("mmk: tray icons: {e}");
            false
        }
    };
    let t = Tray {
        enabled: true,
        profile: "global".into(),
        app: String::new(),
        keyboards: Vec::new(),
        error: None,
        config_path,
        // Panels notice new icon files only after their next periodic rescan; until then
        // show the pixmap, then switch to the named icon so the panel looks it up again.
        named_icon: !fresh,
        tx,
    };
    match t.spawn() {
        Ok(h) => {
            if fresh {
                let h2 = h.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    h2.update(|t| t.named_icon = true);
                });
            }
            Some(h)
        }
        Err(e) => {
            eprintln!("mmk: no tray: {e}");
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Active,
    Disabled,
    Warning,
}

impl Tray {
    fn state(&self) -> State {
        if self.error.is_some() || self.keyboards.is_empty() {
            State::Warning
        } else if self.enabled {
            State::Active
        } else {
            State::Disabled
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "mmk".into()
    }

    fn title(&self) -> String {
        "mmk".into()
    }

    /// Left click opens the menu too, same as right click.
    const MENU_ON_ACTIVATE: bool = true;

    /// A symbolic icon by name: panels size it like their other symbolic icons (e.g. volume)
    /// and tint it with the panel's text colour. The pixmap below is the fallback.
    fn icon_name(&self) -> String {
        if self.named_icon { icon_name(self.state()).into() } else { String::new() }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (rgb, alpha) = match self.state() {
            State::Warning => ([0xf0, 0x9a, 0x2a], 1.0), // amber
            State::Active => ([0xe8, 0xe8, 0xe8], 1.0),
            State::Disabled => ([0x80, 0x80, 0x80], 0.6), // dim grey
        };
        [16, 22, 24, 32, 48, 64].iter().map(|s| cmd_icon(*s, rgb, alpha)).collect()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let mut d = Vec::new();
        if let Some(e) = &self.error {
            d.push(e.clone());
        }
        if self.keyboards.is_empty() {
            d.push("No Apple keyboard found".into());
        } else {
            d.push(self.keyboards.join(", "));
        }
        d.push(format!("Profile: {} ({})", self.profile, if self.app.is_empty() { "-" } else { &self.app }));
        if !self.enabled {
            d.push("Remapping disabled".into());
        }
        ksni::ToolTip { title: "mmk".into(), description: d.join("\n"), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let kbds: Vec<MenuItem<Self>> = if self.keyboards.is_empty() {
            vec![StandardItem { label: "none found".into(), enabled: false, ..Default::default() }.into()]
        } else {
            self.keyboards
                .iter()
                .map(|k| StandardItem { label: k.replace('_', "__"), enabled: false, ..Default::default() }.into())
                .collect()
        };
        let mut items = vec![
            CheckmarkItem {
                label: "Enabled".into(),
                checked: self.enabled,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Msg::SetEnabled(!t.enabled));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("Profile: {}", self.profile).replace('_', "__"),
                enabled: false,
                ..Default::default()
            }
            .into(),
            SubMenu { label: "Keyboards".into(), submenu: kbds, ..Default::default() }.into(),
        ];
        if let Some(e) = &self.error {
            for line in e.lines() {
                items.push(StandardItem { label: line.replace('_', "__"), enabled: false, ..Default::default() }.into());
            }
        }
        items.extend([
            MenuItem::Separator,
            StandardItem {
                label: "Reload config".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Msg::Reload);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open config".into(),
                activate: Box::new(|t: &mut Self| crate::open_config(&t.config_path)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Msg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

/// Tilt of the ⌘ in the tray icon, counter-clockwise.
const TILT_DEG: f32 = 25.0;

fn icon_name(state: State) -> &'static str {
    match state {
        State::Active => "mmk-active-symbolic",
        State::Disabled => "mmk-disabled-symbolic",
        State::Warning => "mmk-warning-symbolic",
    }
}

fn icon_dir() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::config::home().join(".local/share"));
    data.join("icons/hicolor")
}

/// Write the symbolic tray icons into the user's icon theme (only when they changed) and
/// refresh its cache, so panels can find them by name. Returns whether anything changed.
pub fn install_icons() -> std::io::Result<bool> {
    let root = icon_dir();
    let dir = root.join("scalable/status");
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    for state in [State::Active, State::Disabled, State::Warning] {
        let path = dir.join(format!("{}.svg", icon_name(state)));
        let svg = symbolic_svg(state);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(&path, svg)?;
            changed = true;
        }
    }
    if changed {
        refresh_icon_cache(&root);
    }
    Ok(changed)
}

pub fn remove_icons() {
    let root = icon_dir();
    for state in [State::Active, State::Disabled, State::Warning] {
        let _ = std::fs::remove_file(root.join(format!("scalable/status/{}.svg", icon_name(state))));
    }
    refresh_icon_cache(&root);
}

/// A stale icon-theme.cache would hide new icons: rebuild it if one exists.
fn refresh_icon_cache(root: &std::path::Path) {
    if root.join("icon-theme.cache").exists() {
        let ok = std::process::Command::new("gtk-update-icon-cache")
            .args(["-q", "-f", "-t"])
            .arg(root)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            // No tool: drop the cache so the theme directory is scanned instead.
            let _ = std::fs::remove_file(root.join("icon-theme.cache"));
        }
    }
}

/// The tray icon as a 16x16 symbolic SVG: a circle with the ⌘ cut out. `#bebebe` and the
/// `warning` class are the symbolic-icon conventions panels recolour. The cut-out uses a
/// mask drawn with polygon/line/polyline, which symbolic recolouring leaves alone.
fn symbolic_svg(state: State) -> String {
    // Same proportions as the pixmap: ⌘ square half-side, loop radius and stroke, relative
    // to the circle's radius.
    let r = 6.75_f32;
    let (a, lr, stroke) = (0.194 * r, 0.194 * r, 2.0 * 0.0764 * r);
    let e = a + lr;
    let (cx, cy) = (8.0_f32, 8.0_f32);
    let mut glyph = String::new();
    for (x1, y1, x2, y2) in [(-a, -e, -a, e), (a, -e, a, e), (-e, -a, e, -a), (-e, a, e, a)] {
        glyph += &format!(
            "<line x1=\"{:.3}\" y1=\"{:.3}\" x2=\"{:.3}\" y2=\"{:.3}\"/>",
            cx + x1, cy + y1, cx + x2, cy + y2
        );
    }
    for (sx, sy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        // Each loop leaves out the quarter facing the centre.
        let base = (-sy).atan2(-sx);
        let pts: Vec<String> = (0..=27)
            .map(|i| {
                let t = base + std::f32::consts::FRAC_PI_4 + i as f32 / 27.0 * 1.5 * std::f32::consts::PI;
                format!("{:.3},{:.3}", cx + sx * e + lr * t.cos(), cy + sy * e + lr * t.sin())
            })
            .collect();
        glyph += &format!("<polyline points=\"{}\"/>", pts.join(" "));
    }
    let (class, opacity) = match state {
        State::Active => ("", ""),
        State::Disabled => ("", " opacity=\"0.45\""),
        State::Warning => (" class=\"warning\"", ""),
    };
    let fill = if state == State::Warning { "#f57900" } else { "#bebebe" };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" viewBox=\"0 0 16 16\">\
<mask id=\"cut\" maskUnits=\"userSpaceOnUse\" x=\"0\" y=\"0\" width=\"16\" height=\"16\">\
<polygon points=\"0,0 16,0 16,16 0,16\" fill=\"#fff\"/>\
<g transform=\"rotate({tilt} {cx} {cy})\" fill=\"none\" stroke=\"#000\" stroke-width=\"{stroke:.3}\">{glyph}</g>\
</mask>\
<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"{fill}\"{class}{opacity} mask=\"url(#cut)\"/>\
</svg>\n",
        tilt = -TILT_DEG,
    )
}

/// A filled circle with the Command symbol (⌘) cut out, tilted left, ARGB32 (network byte
/// order). Drawn in code with 4x4 supersampling so the edges are smooth at any size.
fn cmd_icon(size: i32, rgb: [u8; 3], alpha: f32) -> ksni::Icon {
    // Circle radius as a fraction of half the icon: leaves the same padding as the panel's
    // symbolic icons (e.g. volume), so it doesn't look oversized next to them.
    const RADIUS: f32 = 0.72;
    // ⌘ proportions (see in_command_symbol), in units of the icon's half-size.
    const A: f32 = 0.14;
    const R: f32 = 0.14;
    const HALF_STROKE: f32 = 0.055;
    const SS: i32 = 4;
    let (sin, cos) = TILT_DEG.to_radians().sin_cos();
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut cover = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = ((x * SS + sx) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    let v = ((y * SS + sy) as f32 + 0.5) / (size * SS) as f32 * 2.0 - 1.0;
                    let in_circle = u * u + v * v <= RADIUS * RADIUS;
                    // Rotate the sample point instead of the glyph (y points down, so this
                    // matrix tilts the drawn glyph counter-clockwise).
                    let (gu, gv) = (u * cos - v * sin, u * sin + v * cos);
                    if in_circle && !in_command_symbol(gu, gv, A, R, HALF_STROKE) {
                        cover += 1;
                    }
                }
            }
            let a = (cover as f32 / (SS * SS) as f32 * alpha * 255.0).round() as u8;
            data.extend_from_slice(&[a, rgb[0], rgb[1], rgb[2]]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// ⌘ built from an inner square of half-side `a` whose sides run on past the corners and
/// curl into four loops of radius `r` (centred diagonally outside each corner, so the sides
/// are tangent to them). Each loop omits the quarter facing the centre.
fn in_command_symbol(u: f32, v: f32, a: f32, r: f32, w: f32) -> bool {
    let e = a + r; // where the sides end: the loops' tangent points
    let sides = [((-a, -e), (-a, e)), ((a, -e), (a, e)), ((-e, -a), (e, -a)), ((-e, a), (e, a))]
        .iter()
        .any(|(p, q)| seg_dist((u, v), *p, *q) <= w);
    let loops = [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)].iter().any(|(sx, sy)| {
        let (cx, cy) = (sx * e, sy * e);
        let facing_centre = sx * (u - cx) < 0.0 && sy * (v - cy) < 0.0;
        !facing_centre && (((u - cx).powi(2) + (v - cy).powi(2)).sqrt() - r).abs() <= w
    });
    sides || loops
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    (cx * cx + cy * cy).sqrt()
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn dump_icons() {
        let dir = std::env::var("ICON_DUMP_DIR").unwrap();
        for (name, rgb, a) in [("on", [0xe8, 0xe8, 0xe8], 1.0), ("off", [0x80, 0x80, 0x80], 0.6), ("warn", [0xf0, 0x9a, 0x2a], 1.0)] {
            for s in [22, 64] {
                let i = super::cmd_icon(s, rgb, a);
                std::fs::write(format!("{dir}/{name}-{s}.argb"), &i.data).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod svg_tests {
    #[test]
    #[ignore]
    fn dump_svgs() {
        let dir = std::env::var("ICON_DUMP_DIR").unwrap();
        for s in [super::State::Active, super::State::Disabled, super::State::Warning] {
            std::fs::write(format!("{dir}/{}.svg", super::icon_name(s)), super::symbolic_svg(s)).unwrap();
        }
    }
}
