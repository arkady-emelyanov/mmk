mod config;
mod devices;
mod engine;
mod focus;
mod install;
mod keys;
mod sys;
mod tray;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use engine::{Engine, Outcome};
use focus::Focus;

/// Everything the engine thread reacts to. One channel, so a slow producer can
/// never block key handling.
pub enum Msg {
    Key(u16, i32),
    Focus(Focus),
    DevicesChanged,
    DeviceGone,
    Led(u16, i32),
    Reload,
    SetEnabled(bool),
    Quit,
}

const USAGE: &str = "\
mmk - Mac-style keyboard remapper for Apple keyboards

usage: mmk [command] [options]

commands:
  run                 run the remapper (default)
  install             set everything up: permissions, binary, config, autostart
  uninstall [--purge] remove everything install did (--purge: also the config)
  init                write the default config (refuses to overwrite)
  check               validate the config
  devices             list detected Apple keyboards
  focus               print the focused app and matched profile as they change
  doctor              diagnose permissions, devices, focus, conflicts

options:
  -c, --config PATH   config file (default: ~/.config/mmk/config.toml)
  -v, --verbose       log every rule match and profile change
  --log FILE          append log output to FILE
  --device PATH       only use this input device (testing)
  -y, --yes           answer yes to install prompts
  -V, --version       print the version
  -h, --help          show this help
";

struct Args {
    cmd: String,
    config: PathBuf,
    verbose: bool,
    log: Option<PathBuf>,
    device: Option<PathBuf>,
    purge: bool,
    yes: bool,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut a = Args {
        cmd: "run".into(),
        config: config::default_path(),
        verbose: false,
        log: None,
        device: None,
        purge: false,
        yes: false,
    };
    let mut p = lexopt::Parser::from_env();
    while let Some(arg) = p.next()? {
        match arg {
            Short('c') | Long("config") => a.config = p.value()?.into(),
            Short('v') | Long("verbose") => a.verbose = true,
            Long("log") => a.log = Some(p.value()?.into()),
            Long("device") => a.device = Some(p.value()?.into()),
            Long("purge") => a.purge = true,
            Short('y') | Long("yes") => a.yes = true,
            Short('V') | Long("version") => {
                println!("mmk {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Short('h') | Long("help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Value(v) => a.cmd = v.string()?,
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mmk: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Some(log) = &args.log {
        redirect_stderr(log);
    }
    let code = match args.cmd.as_str() {
        "run" => run(&args),
        "install" => install::install(args.yes),
        "uninstall" => install::uninstall(args.purge),
        "init" => init(&args.config),
        "check" => check(&args.config),
        "devices" => list_devices(),
        "focus" => watch_focus(&args.config),
        "doctor" => install::doctor(&args.config),
        other => {
            eprintln!("mmk: unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn run(args: &Args) -> i32 {
    let _lock = match install::lock_instance() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mmk: {e}");
            return 1;
        }
    };
    let (cfg, from_file) = match config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mmk: config error: {e}");
            return 1;
        }
    };
    if !from_file {
        eprintln!("mmk: {} not found, using the built-in default (`mmk init` writes it out)", args.config.display());
    }
    let cfg = Arc::new(cfg);

    // Signals are handled on their own thread; block them before any thread starts.
    let sigs = signal_set();
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &sigs, std::ptr::null_mut()) };

    let mut ui = match sys::Uinput::create() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("mmk: cannot create the virtual keyboard (/dev/uinput): {e}\n     run `mmk doctor` or `mmk install`");
            return 1;
        }
    };

    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_signal_thread(sigs, tx.clone());
    spawn_led_thread(&ui, tx.clone());
    watch_config(&args.config, tx.clone());

    let mgr = devices::Manager::new(tx.clone(), args.device.clone(), cfg.grab_wait_ms, cfg.open_retry_ms);
    mgr.start();

    if let Some(backend) = focus::select(&cfg.window_backend) {
        eprintln!("mmk: focus backend: {}", backend.name());
        let t = tx.clone();
        focus::spawn(backend, move |f| {
            let _ = t.send(Msg::Focus(f));
        });
    }

    let tray = if cfg.tray { tray::spawn(tx.clone(), args.config.clone()) } else { None };

    let mut engine = Engine::new(cfg.clone());
    engine.verbose = args.verbose;
    engine.exec = Box::new(spawn_detached);
    let mut app = String::new();
    let mut error: Option<String> = None;

    let update_tray = |engine: &Engine, app: &str, error: &Option<String>| {
        if let Some(h) = &tray {
            // Config errors and current device problems; device problems vanish with the device.
            let mut errors: Vec<String> = error.iter().cloned().collect();
            errors.extend(mgr.errors());
            let error = if errors.is_empty() { None } else { Some(errors.join("\n")) };
            let (enabled, profile, app, kbds) =
                (engine.enabled(), engine.profile_name().to_string(), app.to_string(), mgr.names());
            h.update(move |t| {
                t.enabled = enabled;
                t.profile = profile;
                t.app = app;
                t.error = error;
                t.keyboards = kbds;
            });
        }
    };

    for msg in rx {
        match msg {
            Msg::Key(code, value) => {
                let out = engine.handle(&mut ui, code, value);
                if let Err(e) = ui.flush() {
                    eprintln!("mmk: write to virtual keyboard: {e}");
                }
                match out {
                    Outcome::Exit => {
                        eprintln!("mmk: panic chord pressed, exiting");
                        break;
                    }
                    Outcome::Toggled(on) => {
                        eprintln!("mmk: remapping {}", if on { "enabled" } else { "disabled" });
                        update_tray(&engine, &app, &error);
                    }
                    Outcome::Continue => {}
                }
            }
            Msg::Focus(f) => {
                let before = engine.profile_name().to_string();
                engine.set_focus(&f.app_id, &f.instance);
                app = f.app_id;
                if before != engine.profile_name() {
                    update_tray(&engine, &app, &error);
                }
            }
            Msg::Led(code, value) => mgr.set_led(code, value),
            Msg::DevicesChanged => update_tray(&engine, &app, &error),
            Msg::DeviceGone => {
                engine.release_all(&mut ui);
                let _ = ui.flush();
            }
            Msg::Reload => {
                match config::load(&args.config) {
                    Ok((c, _)) => {
                        engine.set_config(Arc::new(c));
                        error = None;
                        eprintln!("mmk: config reloaded");
                    }
                    Err(e) => {
                        eprintln!("mmk: config error, keeping the previous config: {e}");
                        error = Some(format!("Config error: {e}"));
                    }
                }
                update_tray(&engine, &app, &error);
            }
            Msg::SetEnabled(on) => {
                engine.set_enabled(&mut ui, on);
                let _ = ui.flush();
                eprintln!("mmk: remapping {}", if on { "enabled" } else { "disabled" });
                update_tray(&engine, &app, &error);
            }
            Msg::Quit => break,
        }
    }

    engine.release_all(&mut ui);
    let _ = ui.flush();
    drop(ui);
    eprintln!("mmk: stopped");
    0
}

fn signal_set() -> libc::sigset_t {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGHUP);
        set
    }
}

fn spawn_signal_thread(set: libc::sigset_t, tx: Sender<Msg>) {
    thread::spawn(move || loop {
        let mut sig = 0;
        if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
            return;
        }
        let _ = tx.send(if sig == libc::SIGHUP { Msg::Reload } else { Msg::Quit });
    });
}

/// Caps Lock / Num Lock state written by the desktop to our virtual keyboard.
fn spawn_led_thread(ui: &sys::Uinput, tx: Sender<Msg>) {
    let Ok(mut f) = ui.reader() else { return };
    thread::spawn(move || {
        let _ = sys::read_events(&mut f, |ty, code, value| {
            if ty == sys::EV_LED {
                let _ = tx.send(Msg::Led(code, value));
            }
        });
    });
}

/// Reload when the config file is saved (inotify on its directory: atomic-save safe).
fn watch_config(path: &Path, tx: Sender<Msg>) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else { return };
    let name = name.to_string_lossy().into_owned();
    let Ok(cdir) = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) else { return };
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE;
    if fd < 0 || unsafe { libc::inotify_add_watch(fd, cdir.as_ptr(), mask) } < 0 {
        return; // no config directory yet: nothing to watch
    }
    thread::spawn(move || {
        for n in devices::inotify_names(fd) {
            if n == name {
                let _ = tx.send(Msg::Reload);
            }
        }
    });
}

/// Run a shell command fully detached from mmk.
pub fn spawn_detached(cmd: &str) {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match c.spawn() {
        Ok(mut child) => {
            thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("mmk: exec {cmd:?}: {e}"),
    }
}

pub fn open_config(path: &Path) {
    if !path.exists() {
        let _ = write_default_config(path);
    }
    let p = path.to_string_lossy().replace('\'', "'\\''");
    spawn_detached(&format!("xdg-open '{p}'"));
}

pub fn write_default_config(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, config::DEFAULT_CONFIG)
}

fn redirect_stderr(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Keep the log small: start over once it passes 1 MB.
    if std::fs::metadata(path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(path);
    }
    if let Ok(f) = File::options().create(true).append(true).open(path) {
        unsafe { libc::dup2(f.as_raw_fd(), 2) };
    }
}

fn init(path: &Path) -> i32 {
    if path.exists() {
        eprintln!("mmk: {} already exists", path.display());
        return 1;
    }
    match write_default_config(path) {
        Ok(()) => {
            println!("wrote {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("mmk: {}: {e}", path.display());
            1
        }
    }
}

fn check(path: &Path) -> i32 {
    match config::load(path) {
        Ok((c, from_file)) => {
            let src = if from_file { path.display().to_string() } else { "built-in default".into() };
            let rules: usize = c.base.rules.len();
            println!("ok: {src}: {rules} global rules, {} app profiles", c.apps.len());
            0
        }
        Err(e) => {
            eprintln!("mmk: {e}");
            1
        }
    }
}

fn list_devices() -> i32 {
    let found = devices::scan();
    if found.is_empty() {
        println!("no Apple keyboard found (or no permission to read /dev/input; see `mmk doctor`)");
        return 1;
    }
    for (p, n) in found {
        println!("{}  {n}", p.display());
    }
    0
}

fn watch_focus(path: &Path) -> i32 {
    let cfg = match config::load(path) {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("mmk: {e}");
            return 1;
        }
    };
    let Some(mut backend) = focus::select(&cfg.window_backend) else {
        eprintln!("mmk: no focus backend for this session");
        return 1;
    };
    println!("backend: {}  (Ctrl+C to stop)", backend.name());
    let r = backend.run(&mut |f| {
        let p = cfg.match_app(&f.app_id, &f.instance);
        println!("app_id={:?} instance={:?} -> profile {}", f.app_id, f.instance, p.name);
    });
    if let Err(e) = r {
        eprintln!("mmk: {e}");
        return 1;
    }
    0
}
