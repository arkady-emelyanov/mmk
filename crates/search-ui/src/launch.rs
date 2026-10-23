//! Opening results: apps, files, commands, external terminals (§16.6).

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run `line` with `sh -c`, detached: own session, no stdio, reaped by a thread.
pub fn detached(line: &str, cwd: &Path) -> Result<(), String> {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(line).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = c.spawn().map_err(|e| format!("{line:?}: {e}"))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// A program to run rather than a document to open: executable, and an ELF binary or an
/// AppImage (scripts still open in the editor).
pub fn is_program(path: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    let Ok(md) = std::fs::metadata(path) else { return false };
    if !md.is_file() || md.permissions().mode() & 0o111 == 0 {
        return false;
    }
    let mut magic = [0u8; 4];
    let elf = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"\x7fELF";
    elf || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
}

/// Run a program file detached, in its own folder.
pub fn program(path: &Path) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    detached(&sh_quote(&path.display().to_string()), dir)
}

fn on_path(cmd: &str) -> bool {
    std::env::var("PATH").unwrap_or_default().split(':').any(|d| Path::new(d).join(cmd).is_file())
}

fn gsettings(schema: &str, key: &str) -> Option<String> {
    let out = Command::new("gsettings").args(["get", schema, key]).stderr(Stdio::null()).output().ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
    (!v.is_empty()).then_some(v)
}

/// Terminals to try, in order, after `xdg-terminal-exec`, `$TERMINAL` and the desktop's own.
const KNOWN: &[&str] = &["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "mate-terminal", "kitty", "alacritty", "foot", "wezterm", "ghostty", "xterm"];

/// The command that runs a program in the user's terminal, as an argv prefix: the program's
/// argv is appended. `setting` is `[search] external_terminal`: "auto" or a command.
pub fn terminal_prefix(setting: &str) -> Option<Vec<String>> {
    if setting != "auto" {
        return Some(setting.split_whitespace().map(String::from).collect());
    }
    if on_path("xdg-terminal-exec") {
        return Some(vec!["xdg-terminal-exec".into()]);
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    let from_desktop = if desktop.contains("cinnamon") {
        gsettings("org.cinnamon.desktop.default-applications.terminal", "exec")
    } else if desktop.contains("gnome") {
        gsettings("org.gnome.desktop.default-applications.terminal", "exec")
    } else {
        None
    };
    let term = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty()).or(from_desktop).or_else(|| KNOWN.iter().find(|t| on_path(t)).map(|t| t.to_string()))?;
    // gnome-terminal and friends want `--`, most others `-e`.
    let sep = match term.rsplit('/').next().unwrap_or(&term) {
        "gnome-terminal" | "mate-terminal" | "kgx" | "ptyxis" => "--",
        "kitty" | "foot" | "ghostty" | "wezterm" => "",
        _ => "-e",
    };
    let mut v = vec![term];
    if !sep.is_empty() {
        v.push(sep.into());
    }
    if v[0] == "wezterm" {
        v.insert(1, "start".into());
    }
    Some(v)
}

/// Run `line` in the user's terminal, kept open after it exits.
pub fn in_external_terminal(setting: &str, line: &str, cwd: &Path) -> Result<(), String> {
    let prefix = terminal_prefix(setting).ok_or("no terminal found; set [search] external_terminal")?;
    let shell = login_shell();
    let inner = format!("{line}; exec {}", sh_quote(&shell));
    let mut argv: Vec<String> = prefix;
    argv.extend([shell.clone(), "-c".into(), inner]);
    let cmd = argv.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ");
    detached(&cmd, cwd)
}

/// The user's login shell (passwd), like terminal apps use; else `$SHELL`, else `/bin/sh`.
pub fn login_shell() -> String {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if !pw.is_null() {
        let sh = unsafe { std::ffi::CStr::from_ptr((*pw).pw_shell) }.to_string_lossy().into_owned();
        if !sh.is_empty() && Path::new(&sh).exists() {
            return sh;
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("ls"), "ls");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(terminal_prefix("kitty --single-instance").unwrap(), ["kitty", "--single-instance"]);
    }
}
