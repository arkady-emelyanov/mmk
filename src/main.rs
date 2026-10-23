mod config;
mod devices;
mod engine;
mod focus;
mod install;
mod search;
mod sys;
mod tray;
mod typist;

pub use mmk_common::keys;

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use engine::{Engine, Outcome, SearchMode, Sink};
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
    /// The X keymap changed (a keyboard appeared, the layout was reloaded).
    Keymap,
    /// From the search UI (§16.14).
    SearchShown,
    SearchFocused,
    SearchHidden,
    SearchMode(SearchMode),
    /// The UI process is gone.
    SearchGone,
    /// `hold_ms` passed since the request that started hold N.
    SearchTimeout(u32),
    /// For the tray: search unavailable or crashing; `None` when fine.
    SearchStatus(Option<search::Status>),
    /// Tray menu: toggle the search window.
    SearchToggle,
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
  doctor              diagnose permissions, devices, focus, search, conflicts
  search [TEXT]       toggle the search window (with TEXT: show it with TEXT typed);
                      starts mmk if it isn't running
    --terminal [LINE]   open the inline terminal (running LINE)
    --reindex           rebuild the file index
    --forget TEXT       drop the usage history for a query
    --query TEXT        print ranked results without a window

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
    /// Positional arguments after the command.
    rest: Vec<String>,
    terminal: bool,
    reindex: bool,
    forget: Option<String>,
    query: Option<String>,
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
        rest: Vec::new(),
        terminal: false,
        reindex: false,
        forget: None,
        query: None,
        config: config::default_path(),
        verbose: false,
        log: None,
        device: None,
        purge: false,
        yes: false,
    };
    let mut p = lexopt::Parser::from_env();
    let mut have_cmd = false;
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
            Long("terminal") => a.terminal = true,
            Long("reindex") => a.reindex = true,
            Long("forget") => a.forget = Some(p.value()?.string()?),
            Long("query") => a.query = Some(p.value()?.string()?),
            Value(v) if !have_cmd => {
                a.cmd = v.string()?;
                have_cmd = true;
            }
            Value(v) if a.cmd == "search" => a.rest.push(v.string()?),
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
        "search" => search_cmd(&args),
        other => {
            eprintln!("mmk: unknown command {other:?}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// `mmk search ...`: a thin client of the running mmk (see search.rs).
fn search_cmd(a: &Args) -> i32 {
    use mmk_common::proto::Msg as Line;
    if let Some(q) = &a.query {
        return search::query(q, &a.config);
    }
    let text = a.rest.join(" ");
    let req = if a.terminal {
        let l = Line::new("terminal");
        if text.is_empty() { l } else { l.arg("line", text) }
    } else if a.reindex {
        Line::new("reindex")
    } else if let Some(f) = &a.forget {
        Line::new("forget").arg("query", f.as_str())
    } else if text.is_empty() {
        Line::new("toggle")
    } else {
        Line::new("show").arg("query", text)
    };
    search::client(req, &a.config)
}

/// Where the engine's output goes when remapping is off: nowhere.
struct NoOutput;

impl Sink for NoOutput {
    fn key(&mut self, _: u16, _: i32) {}
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
    let mut search_error = cfg.search_error.clone();
    if let Some(e) = &search_error {
        eprintln!("mmk: config error, search uses the default settings: {e}");
    }
    let remapping = cfg.remapping;
    let cfg = Arc::new(cfg);

    // Signals are handled on their own thread; block them before any thread starts.
    let sigs = signal_set();
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &sigs, std::ptr::null_mut()) };

    // Without remapping: no virtual keyboard, no grabbing, no focus tracking; the engine
    // only ever sees search messages and writes nowhere.
    let mut ui = if remapping {
        match sys::Uinput::create() {
            Ok(u) => Some(u),
            Err(e) => {
                eprintln!("mmk: cannot create the virtual keyboard (/dev/uinput): {e}\n     run `mmk doctor` or `mmk install`");
                return 1;
            }
        }
    } else {
        eprintln!("mmk: remapping = false: search only, keyboards are not grabbed");
        None
    };
    let mut no_output = NoOutput;
    macro_rules! out {
        () => {
            match ui.as_mut() {
                Some(u) => u as &mut dyn Sink,
                None => &mut no_output as &mut dyn Sink,
            }
        };
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_signal_thread(sigs, tx.clone());
    watch_config(&args.config, tx.clone());

    let mgr = ui.as_ref().map(|ui| {
        spawn_led_thread(ui, tx.clone());
        let mgr = devices::Manager::new(tx.clone(), args.device.clone(), cfg.grab_wait_ms, cfg.open_retry_ms);
        mgr.start();
        if let Some(backend) = focus::select(&cfg.window_backend) {
            eprintln!("mmk: focus backend: {}", backend.name());
            let t = tx.clone();
            focus::spawn(backend, move |f| {
                let _ = t.send(Msg::Focus(f));
            });
        }
        mgr
    });

    let tray = if cfg.tray { tray::spawn(tx.clone(), args.config.clone(), remapping) } else { None };
    let search = search::Supervisor::start(tx.clone(), cfg.search.clone(), args.verbose);

    let mut engine = Engine::new(cfg.clone());
    engine.verbose = args.verbose;
    engine.exec = Box::new(spawn_detached);
    let s = search.clone();
    engine.search = Box::new(move |c| s.call(c));
    if remapping && let Some(t) = typist::select() {
        // Watch first: the desktop typically re-applies the layout when our virtual keyboard
        // appears, wiping the characters mapped just below.
        let w = tx.clone();
        typist::watch(move || {
            let _ = w.send(Msg::Keymap);
        });
        engine.set_typist(t);
    }
    let mut app = String::new();
    let mut error: Option<String> = None;
    let mut search_status: Option<search::Status> = None;

    let update_tray = |engine: &Engine, app: &str, error: &Option<String>, search_error: &Option<String>, search_status: &Option<search::Status>| {
        if let Some(h) = &tray {
            // Config errors and current device problems; device problems vanish with the device.
            let mut errors: Vec<String> = error.iter().cloned().collect();
            errors.extend(search_error.iter().map(|e| format!("Config error (search): {e}")));
            if let Some(m) = &mgr {
                errors.extend(m.errors());
            }
            let error = if errors.is_empty() { None } else { Some(errors.join("\n")) };
            let (enabled, profile, app) = (engine.enabled(), engine.profile_name().to_string(), app.to_string());
            let kbds = mgr.as_ref().map(|m| m.names()).unwrap_or_default();
            let search = search_status.clone();
            h.update(move |t| {
                t.enabled = enabled;
                t.profile = profile;
                t.app = app;
                t.error = error;
                t.keyboards = kbds;
                t.search = search;
            });
        }
    };
    if search_error.is_some() {
        update_tray(&engine, &app, &error, &search_error, &search_status);
    }

    for msg in rx {
        let outcome = match msg {
            Msg::Key(code, value) => engine.handle(out!(), code, value),
            Msg::Focus(f) => {
                let before = engine.profile_name().as_ptr();
                engine.set_focus(&f.app_id, &f.instance);
                app = f.app_id;
                if before != engine.profile_name().as_ptr() {
                    update_tray(&engine, &app, &error, &search_error, &search_status);
                }
                Outcome::Continue
            }
            Msg::Led(code, value) => {
                if let Some(m) = &mgr {
                    m.set_led(code, value);
                }
                Outcome::Continue
            }
            Msg::DevicesChanged => {
                update_tray(&engine, &app, &error, &search_error, &search_status);
                Outcome::Continue
            }
            Msg::DeviceGone => {
                engine.release_all(out!());
                Outcome::Continue
            }
            Msg::Reload => {
                match config::load(&args.config) {
                    Ok((mut c, _)) => {
                        c.keep_search(engine.config());
                        if let Some(e) = &c.search_error {
                            eprintln!("mmk: config error, keeping the previous search settings: {e}");
                        }
                        if c.remapping != remapping {
                            eprintln!("mmk: remapping = {} takes effect when mmk restarts", c.remapping);
                        }
                        search.configure(&c.search);
                        search_error = c.search_error.clone();
                        engine.set_config(Arc::new(c));
                        error = None;
                        eprintln!("mmk: config reloaded");
                    }
                    Err(e) => {
                        eprintln!("mmk: config error, keeping the previous config: {e}");
                        error = Some(format!("Config error: {e}"));
                    }
                }
                update_tray(&engine, &app, &error, &search_error, &search_status);
                Outcome::Continue
            }
            Msg::SetEnabled(on) => {
                engine.set_enabled(out!(), on);
                eprintln!("mmk: remapping {}", if on { "enabled" } else { "disabled" });
                update_tray(&engine, &app, &error, &search_error, &search_status);
                Outcome::Continue
            }
            Msg::Keymap => {
                engine.keymap_changed();
                Outcome::Continue
            }
            Msg::SearchShown | Msg::SearchFocused | Msg::SearchHidden | Msg::SearchGone | Msg::SearchMode(_) | Msg::SearchTimeout(_) => {
                let before = engine.profile_name().as_ptr();
                let o = match msg {
                    Msg::SearchShown => {
                        engine.search_shown();
                        Outcome::Continue
                    }
                    Msg::SearchFocused => engine.search_focused(out!()),
                    Msg::SearchHidden => engine.search_hidden(out!()),
                    Msg::SearchGone => engine.search_gone(out!()),
                    Msg::SearchMode(m) => {
                        engine.set_search_mode(m);
                        Outcome::Continue
                    }
                    Msg::SearchTimeout(id) => engine.search_timeout(out!(), id),
                    _ => unreachable!(),
                };
                if before != engine.profile_name().as_ptr() {
                    update_tray(&engine, &app, &error, &search_error, &search_status);
                }
                o
            }
            Msg::SearchStatus(s) => {
                search_status = s;
                update_tray(&engine, &app, &error, &search_error, &search_status);
                Outcome::Continue
            }
            Msg::SearchToggle => {
                search.send(mmk_common::proto::Msg::new("toggle"));
                Outcome::Continue
            }
            Msg::Quit => break,
        };
        if let Some(u) = ui.as_mut()
            && let Err(e) = u.flush()
        {
            eprintln!("mmk: write to virtual keyboard: {e}");
        }
        match outcome {
            Outcome::Exit => {
                eprintln!("mmk: panic chord pressed, exiting");
                break;
            }
            Outcome::Toggled(on) => {
                eprintln!("mmk: remapping {}", if on { "enabled" } else { "disabled" });
                update_tray(&engine, &app, &error, &search_error, &search_status);
            }
            Outcome::Continue => {}
        }
    }

    engine.release_all(out!());
    if let Some(mut u) = ui {
        let _ = u.flush();
    }
    let _ = std::fs::remove_file(search::socket_path());
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
            if let Some(e) = &c.search_error {
                eprintln!("mmk: {src}: {e}");
                return 1;
            }
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
