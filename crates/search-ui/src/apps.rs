//! Applications: desktop entries, through GIO (§16.6.1).

use std::path::PathBuf;

use gio::prelude::*;

#[derive(Clone, Debug)]
pub struct App {
    /// Desktop file id, e.g. `firefox.desktop`.
    pub id: String,
    pub name: String,
    pub generic: String,
    pub keywords: Vec<String>,
    /// Executable basename, empty for launcher wrappers (flatpak, env, ...).
    pub exec: String,
    pub icon: String,
    /// The `.desktop` file, for reveal.
    pub file: Option<PathBuf>,
    /// An executable from `[search.apps] dirs`, run directly (no desktop entry).
    pub path: Option<PathBuf>,
}

/// Wrappers whose name says nothing about the app they start.
const WRAPPERS: &[&str] = &["flatpak", "env", "sh", "bash", "snap", "gtk-launch", "kioclient5", "kioclient"];

/// Every app the desktop shows in menus, then the executables in `dirs` that no desktop entry
/// already starts.
pub fn load(dirs: &[PathBuf]) -> Vec<App> {
    let mut apps = desktop();
    let known: std::collections::HashSet<String> = apps.iter().map(|a| a.exec.clone()).collect();
    apps.extend(from_dirs(dirs).into_iter().filter(|a| !known.contains(&a.exec)));
    apps
}

/// Executables directly inside `dirs` (AppImages, standalone binaries).
pub fn from_dirs(dirs: &[PathBuf]) -> Vec<App> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = std::fs::metadata(e.path()) else { continue };
            if !md.is_file() || md.permissions().mode() & 0o111 == 0 {
                continue;
            }
            let file = e.file_name().to_string_lossy().into_owned();
            out.push(App {
                id: format!("path:{}", e.path().display()),
                name: pretty_name(&file),
                generic: String::new(),
                keywords: vec![file.clone()],
                exec: file,
                icon: "application-x-executable".into(),
                file: None,
                path: Some(e.path()),
            });
        }
    }
    out
}

/// A readable name from a release file name: `FreeCAD_1.1.4-Linux-x86_64-py311.AppImage` ->
/// `FreeCAD`, `pdf-preview-0.10.1.AppImage` -> `pdf-preview`, `MeshLab2025.07-linux_x86_64`
/// -> `MeshLab`.
pub fn pretty_name(file: &str) -> String {
    let stem = file.strip_suffix(".AppImage").or_else(|| file.strip_suffix(".appimage")).unwrap_or(file);
    const NOISE: &[&str] = &["linux", "x86_64", "x86", "amd64", "aarch64", "arm64", "appimage", "x64", "64"];
    let mut kept: Vec<&str> = Vec::new();
    for tok in stem.split(['-', '_', ' ']) {
        let t = tok.to_lowercase();
        let version = t.starts_with(|c: char| c.is_ascii_digit()) || (t.len() > 1 && t.starts_with('v') && t[1..].starts_with(|c: char| c.is_ascii_digit()));
        if version || NOISE.contains(&t.as_str()) || t.starts_with("ubuntu") || t.starts_with("linux") {
            break;
        }
        kept.push(tok);
    }
    let name = kept.join("-");
    // Version glued to the name: "MeshLab2025.07".
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if name.is_empty() { stem.to_string() } else { name.to_string() }
}

fn desktop() -> Vec<App> {
    gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let id = a.id()?.to_string();
            // GIO keeps the Exec field's quotes: `"/home/u/Apps/x.AppImage"`.
            let exe = a.executable().to_string_lossy().trim_matches(['"', '\'']).to_string();
            let exec = std::path::Path::new(&exe).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let exec = if WRAPPERS.contains(&exec.as_str()) { String::new() } else { exec };
            let icon = a.icon().and_then(|i| IconExt::to_string(&i)).map(|s| s.to_string()).unwrap_or_default();
            let d = a.downcast_ref::<gio_unix::DesktopAppInfo>();
            Some(App {
                name: a.name().to_string(),
                generic: d.and_then(|d| d.generic_name()).map(|s| s.to_string()).unwrap_or_default(),
                keywords: d.map(|d| d.keywords().iter().map(|k| k.to_string()).collect()).unwrap_or_default(),
                file: d.and_then(|d| d.filename()),
                path: None,
                id,
                exec,
                icon,
            })
        })
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_names() {
        for (f, n) in [
            ("FreeCAD_1.1.4-Linux-x86_64-py311.AppImage", "FreeCAD"),
            ("Gyroflow-linux64.AppImage", "Gyroflow"),
            ("HyperFrames-x86_64.AppImage", "HyperFrames"),
            ("kicad-10.0.5-x86_64.AppImage", "kicad"),
            ("LosslessCut-linux-x86_64.AppImage", "LosslessCut"),
            ("MeshLab2025.07-linux_x86_64.AppImage", "MeshLab"),
            ("orca-linux.AppImage", "orca"),
            ("OrcaSlicer_Linux_AppImage_Ubuntu2404_V2.4.1.AppImage", "OrcaSlicer"),
            ("osd-overlay-0.9.0.AppImage", "osd-overlay"),
            ("pdf-preview-0.10.1.AppImage", "pdf-preview"),
            ("difm", "difm"),
        ] {
            assert_eq!(pretty_name(f), n, "{f}");
        }
    }
}
