//! `mmk install` / `uninstall` / `doctor` and the single-instance lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, home};
use crate::{devices, focus, search, sys};

const UDEV_RULE: &str = "/etc/udev/rules.d/70-mmk.rules";
const MODULES_LOAD: &str = "/etc/modules-load.d/mmk.conf";
const UDEV_RULE_BODY: &str = "\
# mmk: give the logged-in user access to Apple keyboards and /dev/uinput (no group membership needed)
KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"
SUBSYSTEM==\"input\", KERNEL==\"event*\", ATTRS{id/vendor}==\"05ac\", TAG+=\"uaccess\"
SUBSYSTEM==\"input\", KERNEL==\"event*\", ATTRS{id/vendor}==\"004c\", TAG+=\"uaccess\"
";
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/mmk-{}", unsafe { libc::getuid() })))
}

fn lock_path() -> PathBuf {
    runtime_dir().join("mmk.lock")
}

pub fn bin_path() -> PathBuf {
    home().join(".local/bin/mmk")
}

pub fn log_path() -> PathBuf {
    home().join(".local/state/mmk/mmk.log")
}

fn state_dir() -> PathBuf {
    home().join(".local/state/mmk")
}

/// Files the search UI writes (§16.11, §16.7.2, §16.6.6).
fn search_data() -> [PathBuf; 3] {
    [
        home().join(".cache/mmk/files.idx"),
        state_dir().join("search-history.tsv"),
        state_dir().join("codes.tsv"),
    ]
}

fn autostart_path() -> PathBuf {
    home().join(".config/autostart/mmk.desktop")
}

/// Hold an exclusive lock for the life of the process; fails if mmk already runs.
pub fn lock_instance() -> Result<File, String> {
    let p = lock_path();
    let _ = fs::create_dir_all(p.parent().unwrap());
    let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut pid = String::new();
        let _ = f.read_to_string(&mut pid);
        return Err(format!("mmk is already running (pid {})", pid.trim()));
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}", std::process::id());
    Ok(f)
}

/// Pid of the running mmk, if any.
pub fn running_pid() -> Option<i32> {
    let mut f = File::open(lock_path()).ok()?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

fn stop_running() -> bool {
    let Some(pid) = running_pid() else { return false };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(3);
    while running_pid().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

fn confirm(question: &str, yes: bool) -> bool {
    if yes {
        println!("{question} [y/N] y");
        return true;
    }
    if !io::stdin().is_terminal() {
        println!("{question} [y/N] (not a terminal: no; pass --yes to accept)");
        return false;
    }
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

/// Run a shell snippet as root via pkexec (or sudo).
fn run_privileged(script: &str) -> bool {
    let tool = if which("pkexec") { "pkexec" } else { "sudo" };
    Command::new(tool).args(["sh", "-c", script]).status().is_ok_and(|s| s.success())
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

fn uinput_writable() -> bool {
    OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

/// Apple keyboard nodes we cannot open (by vendor id from sysfs, no permission needed).
fn unreadable_apple_nodes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/input") else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let vendor = fs::read_to_string(e.path().join("device/id/vendor")).unwrap_or_default();
        let Ok(v) = u16::from_str_radix(vendor.trim(), 16) else { continue };
        if !sys::APPLE_VENDORS.contains(&v) {
            continue;
        }
        let dev = PathBuf::from("/dev/input").join(&name);
        if OpenOptions::new().read(true).write(true).open(&dev).is_err() {
            out.push(dev);
        }
    }
    out
}

/// Running processes whose command line mentions any of `needles`.
fn processes(needles: &[&str]) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        if pid == me {
            continue;
        }
        let Ok(raw) = fs::read(e.path().join("cmdline")) else { continue };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ").trim().to_string();
        let lower = cmd.to_lowercase();
        if needles.iter().any(|n| lower.contains(n)) && !lower.contains("claude") {
            out.push((pid, cmd));
        }
    }
    out
}

pub fn install(yes: bool) -> i32 {
    println!("mmk install\n");

    // 1. Permissions: the only privileged step, and only if access is missing.
    let unreadable = unreadable_apple_nodes();
    if !uinput_writable() || !unreadable.is_empty() {
        println!("[permissions] mmk needs access to /dev/uinput and your Apple keyboards.");
        println!("This writes, as root:\n  {UDEV_RULE}:\n{}  {MODULES_LOAD}: uinput", indent(UDEV_RULE_BODY));
        if !confirm("Apply with pkexec/sudo?", yes) {
            println!("Skipped. mmk cannot run without this.");
            return 1;
        }
        let script = format!(
            "set -e; printf '%s' '{}' > {UDEV_RULE}; echo uinput > {MODULES_LOAD}; modprobe uinput; \
             udevadm control --reload-rules; udevadm trigger --subsystem-match=input --subsystem-match=misc --action=change; udevadm settle",
            UDEV_RULE_BODY.replace('\'', "'\\''")
        );
        if !run_privileged(&script) {
            println!("Privileged step failed.");
            return 1;
        }
        std::thread::sleep(Duration::from_millis(500));
        if !uinput_writable() || !unreadable_apple_nodes().is_empty() {
            println!("Access still missing; log out and back in, then run `mmk install` again.");
            return 1;
        }
        println!("[permissions] ok");
    } else {
        println!("[permissions] ok (already have access to /dev/uinput and the Apple keyboards)");
    }

    // 2. Binary.
    let bin = bin_path();
    match std::env::current_exe() {
        Ok(exe) if fs::canonicalize(&exe).ok() != fs::canonicalize(&bin).ok() => {
            let tmp = bin.with_extension("new");
            let r = fs::create_dir_all(bin.parent().unwrap())
                .and_then(|_| fs::copy(&exe, &tmp))
                .and_then(|_| fs::rename(&tmp, &bin));
            match r {
                Ok(()) => println!("[binary] installed {}", bin.display()),
                Err(e) => {
                    println!("[binary] {}: {e}", bin.display());
                    return 1;
                }
            }
        }
        _ => println!("[binary] {} (already in place)", bin.display()),
    }
    // The search window, shipped next to mmk; optional (remapping works without it).
    let ui_dst = bin.with_file_name(search::UI_BIN);
    match std::env::current_exe().map(|e| e.with_file_name(search::UI_BIN)) {
        Ok(src) if src.is_file() && fs::canonicalize(&src).ok() != fs::canonicalize(&ui_dst).ok() => {
            let tmp = ui_dst.with_extension("new");
            match fs::copy(&src, &tmp).and_then(|_| fs::rename(&tmp, &ui_dst)) {
                Ok(()) => println!("[binary] installed {}", ui_dst.display()),
                Err(e) => println!("[binary] {}: {e} (search will be unavailable)", ui_dst.display()),
            }
        }
        Ok(src) if src.is_file() => println!("[binary] {} (already in place)", ui_dst.display()),
        _ if ui_dst.is_file() => println!("[binary] {} (kept; none next to this mmk)", ui_dst.display()),
        _ => println!("[binary] {} not found next to this mmk: search will be unavailable", search::UI_BIN),
    }
    // The search window links the system's GTK4 and VTE (GTK4 build): if they are missing it
    // can't start, and says why on stderr.
    if ui_dst.is_file() {
        match Command::new(&ui_dst).arg("--version").stdout(Stdio::null()).stderr(Stdio::piped()).output() {
            Ok(o) if o.status.success() => println!("[search] ok"),
            Ok(o) => {
                let why = String::from_utf8_lossy(&o.stderr);
                println!("[search] the search window can't start: {}", why.lines().next().unwrap_or("").trim());
                println!("         it needs GTK4 and VTE for GTK4, e.g.:");
                println!("           Debian/Ubuntu/Mint: sudo apt install libgtk-4-1 libvte-2.91-gtk4-0");
                println!("           Fedora:             sudo dnf install gtk4 vte291-gtk4");
                println!("           Arch:               sudo pacman -S gtk4 vte4");
                println!("         Remapping works without it.");
            }
            Err(e) => println!("[search] {}: {e}", ui_dst.display()),
        }
    }

    // 3. Config.
    let cfg = config::default_path();
    if cfg.exists() {
        println!("[config] {} (kept)", cfg.display());
    } else if let Err(e) = crate::write_default_config(&cfg) {
        println!("[config] {}: {e}", cfg.display());
    } else {
        println!("[config] wrote {}", cfg.display());
    }

    // 4. Autostart.
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=mmk\nComment=Mac-style keyboard remapper\nExec={} run --log {}\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        bin.display(),
        log_path().display()
    );
    let auto = autostart_path();
    match fs::create_dir_all(auto.parent().unwrap()).and_then(|_| fs::write(&auto, entry)) {
        Ok(()) => println!("[autostart] {}", auto.display()),
        Err(e) => println!("[autostart] {}: {e}", auto.display()),
    }

    // 5. Conflicts.
    for (pid, cmd) in processes(&["keyd", "xremap", "kanata", "kmonad"]) {
        println!("[conflicts] another remapper is running: {pid} {cmd}");
    }

    // 6. (Re)start.
    let restarted = stop_running();
    let log = log_path();
    let _ = fs::create_dir_all(log.parent().unwrap());
    crate::spawn_detached(&format!("exec '{}' run --log '{}'", bin.display(), log.display()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while running_pid().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(800));
    match running_pid() {
        Some(pid) => println!("[start] mmk {} (pid {pid}), log: {}", if restarted { "restarted" } else { "started" }, log.display()),
        None => {
            println!("[start] mmk did not start; see {}", log.display());
            return 1;
        }
    }
    println!("\nKeyboards:");
    for (p, n) in devices::scan() {
        println!("  {}  {n}", p.display());
    }
    if let Some(f) = focus::current_x11() {
        println!("Focused app: {:?} -> profile {}", f.app_id, config::load(&cfg).map(|(c, _)| c.match_app(&f.app_id, &f.instance).name.clone()).unwrap_or_default());
    }
    0
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

pub fn uninstall(purge: bool) -> i32 {
    println!("mmk uninstall\n");
    if stop_running() {
        println!("[stop] stopped mmk");
    }
    let ui = bin_path().with_file_name(search::UI_BIN);
    for p in [autostart_path(), bin_path(), ui].into_iter().chain(search_data()) {
        if fs::remove_file(&p).is_ok() {
            println!("[remove] {}", p.display());
        }
    }
    let _ = fs::remove_dir(home().join(".cache/mmk")); // only if empty
    crate::tray::remove_icons();
    if Path::new(UDEV_RULE).exists() || Path::new(MODULES_LOAD).exists() {
        println!("[permissions] removing {UDEV_RULE} and {MODULES_LOAD} (needs root)");
        if run_privileged(&format!("rm -f {UDEV_RULE} {MODULES_LOAD}; udevadm control --reload-rules")) {
            println!("[permissions] removed");
        }
    }
    if purge {
        if let Some(dir) = config::default_path().parent() {
            if fs::remove_dir_all(dir).is_ok() {
                println!("[purge] {}", dir.display());
            }
        }
        let _ = fs::remove_dir_all(state_dir());
    }
    0
}

pub fn doctor(cfg_path: &Path) -> i32 {
    let mut problems = 0;
    let mut check = |ok: bool, what: String| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            problems += 1;
        }
    };
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into());
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into());
    println!("     session: {session}, desktop: {desktop}");
    check(uinput_writable(), "/dev/uinput writable".into());
    let kbds = devices::scan();
    check(!kbds.is_empty(), format!("Apple keyboards readable: {}", kbds.iter().map(|(p, n)| format!("{n} ({})", p.display())).collect::<Vec<_>>().join(", ")));
    for p in unreadable_apple_nodes() {
        check(false, format!("no access to {} (run `mmk install`)", p.display()));
    }
    let cfg = config::load(cfg_path);
    match &cfg {
        Ok((_, true)) => check(true, format!("config {}", cfg_path.display())),
        Ok((_, false)) => check(true, "config: built-in default (no file)".into()),
        Err(e) => check(false, format!("config: {e}")),
    }
    if let Ok((c, _)) = &cfg {
        if let Some(e) = &c.search_error {
            check(false, format!("config [search]: {e}"));
        }
        if !c.remapping {
            println!("     remapping = false: search only");
        }
        if !c.search.enabled {
            println!("     search: disabled ([search] enabled = false)");
        } else {
            match search::ui_binary() {
                None => check(false, format!("search: {} not found next to mmk or on $PATH", search::UI_BIN)),
                Some(bin) => {
                    let v = Command::new(&bin).arg("--version").output();
                    match v {
                        Ok(o) if o.status.success() => {
                            let ver = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            check(true, format!("search: {} ({ver})", bin.display()));
                        }
                        Ok(o) => check(false, format!("search: {} does not run: {}", bin.display(), String::from_utf8_lossy(&o.stderr).trim())),
                        Err(e) => check(false, format!("search: {}: {e}", bin.display())),
                    }
                }
            }
            let sock = search::socket_path();
            let up = std::os::unix::net::UnixStream::connect(&sock).is_ok();
            println!("     search socket: {} ({})", sock.display(), if up { "answering" } else { "not answering" });
        }
    }
    match focus::current_x11() {
        Some(f) => check(true, format!("focus (x11): {:?} / {:?}", f.app_id, f.instance)),
        None => check(session != "x11", "focus: cannot read the active window from X11".into()),
    }
    let others = processes(&["keyd", "xremap", "kanata", "kmonad"]);
    check(others.is_empty(), format!("no other remapper running{}", others.iter().map(|(p, c)| format!("\n       {p} {c}")).collect::<String>()));
    match running_pid() {
        Some(pid) => println!("     mmk is running (pid {pid}), log: {}", log_path().display()),
        None => println!("     mmk is not running"),
    }
    if problems > 0 { 1 } else { 0 }
}
