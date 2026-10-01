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
    tx: Sender<Msg>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

pub fn spawn(tx: Sender<Msg>, config_path: PathBuf) -> Option<Handle> {
    let t = Tray {
        enabled: true,
        profile: "global".into(),
        app: String::new(),
        keyboards: Vec::new(),
        error: None,
        config_path,
        tx,
    };
    match t.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("mmk: no tray: {e}");
            None
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

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (rgb, alpha) = if self.error.is_some() || self.keyboards.is_empty() {
            ([0xf0, 0x9a, 0x2a], 1.0) // problem: amber
        } else if self.enabled {
            ([0xe8, 0xe8, 0xe8], 1.0)
        } else {
            ([0x80, 0x80, 0x80], 0.6) // disabled: dim grey
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
    // Tilt of the ⌘, counter-clockwise.
    const TILT_DEG: f32 = 25.0;
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
