//! Commands: executables on `$PATH` (§16.6.2).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Command {
    pub name: String,
    pub path: PathBuf,
}

/// Executables in `path_var`'s directories, first match wins like the shell; `hide` names are
/// left out (the executables of listed apps).
pub fn load(path_var: &str, hide: &HashSet<String>) -> Vec<Command> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if seen.contains(&name) {
                continue;
            }
            // Follows symlinks, like the shell.
            let Ok(md) = std::fs::metadata(e.path()) else { continue };
            if !md.is_file() || md.permissions().mode() & 0o111 == 0 {
                continue;
            }
            seen.insert(name.clone());
            if !hide.contains(&name) {
                out.push(Command { name, path: e.path() });
            }
        }
    }
    out
}

/// `"ping 8.8.8.8"` -> `("ping", "8.8.8.8")`: the first word is matched, the rest passed through.
pub fn split(query: &str) -> (&str, &str) {
    let q = query.trim_start();
    match q.find(char::is_whitespace) {
        Some(i) => (&q[..i], q[i..].trim()),
        None => (q, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits() {
        assert_eq!(split("ping 8.8.8.8"), ("ping", "8.8.8.8"));
        assert_eq!(split("  ls   -la /tmp "), ("ls", "-la /tmp"));
        assert_eq!(split("htop"), ("htop", ""));
    }

    #[test]
    fn loads_first_match_and_hides() {
        let t = std::env::temp_dir().join(format!("mmk-cmds-{}", std::process::id()));
        let (a, b) = (t.join("a"), t.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mk = |p: PathBuf, mode| {
            std::fs::write(&p, "").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mk(a.join("tool"), 0o755);
        mk(b.join("tool"), 0o755);
        mk(a.join("data"), 0o644);
        mk(b.join("firefox"), 0o755);
        let hide = HashSet::from(["firefox".to_string()]);
        let path = format!("{}:{}", a.display(), b.display());
        let cmds = load(&path, &hide);
        let names: Vec<_> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["tool"]);
        assert_eq!(cmds[0].path, a.join("tool"));
        std::fs::remove_dir_all(&t).unwrap();
    }
}
