//! Search (§16): supervising `mmk-search-ui`, the `mmk.sock` request socket, and the
//! `mmk search` client.
//!
//! The UI runs as a child process connected by a socketpair (its fd 3), spoken to in the
//! line protocol of `mmk_common::proto`. One long-lived supervisor thread starts it, writes
//! to it and restarts it; a reader thread per child turns its messages into engine events.
//! Nothing here ever blocks the engine thread: it only sends on channels.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mmk_common::proto::Msg as Line;

use crate::config::Search;
use crate::engine::{SearchCall, SearchMode};

pub const UI_BIN: &str = "mmk-search-ui";
/// Restart backoff after the UI exits: doubles from the first value up to the second.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A UI that stayed up this long counts as healthy again: backoff and warnings reset.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// At most one "search unavailable" notification this often.
const NOTICE_EVERY: Duration = Duration::from_secs(10);
/// A UI that doesn't take a line within this long is considered hung and killed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a socket client waits for the UI's answer (`status`).
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Requests accepted on the socket.
const CLIENT_VERBS: &[&str] = &["toggle", "show", "hide", "terminal", "reindex", "status", "forget"];

/// Search state worth showing in the tray; `None` from the supervisor means all is well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub text: String,
    /// The process keeps crashing (as opposed to: not installed).
    pub warn: bool,
}

pub fn socket_path() -> PathBuf {
    crate::install::runtime_dir().join("mmk/mmk.sock")
}

/// `mmk-search-ui` next to our own executable, else on `$PATH`.
pub fn ui_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.with_file_name(UI_BIN);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(UI_BIN)).find(|p| p.is_file())
}

enum Cmd {
    Configure(Search),
    Call(SearchCall),
    /// Fire and forget (tray).
    Send(Line),
    /// From a socket client: the answer lines go to the sender; dropping it ends the answer.
    Client(Line, Sender<String>),
    /// The reader of child `generation` saw EOF.
    Exited(u64),
}

/// Handle to the supervisor thread.
#[derive(Clone)]
pub struct Supervisor {
    tx: Sender<Cmd>,
}

impl Supervisor {
    /// Start the supervisor thread (which starts the UI if enabled) and the socket listener.
    pub fn start(events: Sender<crate::Msg>, cfg: Search, verbose: bool) -> Supervisor {
        let (tx, rx) = mpsc::channel();
        let sup = Supervisor { tx: tx.clone() };
        let listener = sup.clone();
        // pdeathsig fires when the thread that forked the child exits: this one never does.
        thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let mut s = State {
                    events,
                    me: tx,
                    cfg,
                    verbose,
                    child: None,
                    generation: 0,
                    backoff: BACKOFF_MIN,
                    next_start: None,
                    status: None,
                    noticed: None,
                    waiter: Arc::new(Mutex::new(None)),
                };
                if s.cfg.enabled {
                    s.start_child();
                }
                // Listen only once the UI is spawned, so a cold `mmk search` finds it.
                thread::spawn(move || listen(listener));
                s.run(rx);
            })
            .expect("spawn search thread");
        sup
    }

    pub fn configure(&self, cfg: &Search) {
        let _ = self.tx.send(Cmd::Configure(cfg.clone()));
    }

    /// From the engine (a `search*` action).
    pub fn call(&self, c: SearchCall) {
        let _ = self.tx.send(Cmd::Call(c));
    }

    pub fn send(&self, l: Line) {
        let _ = self.tx.send(Cmd::Send(l));
    }

    fn client(&self, l: Line) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Cmd::Client(l, tx));
        rx
    }
}

struct Running {
    proc: Child,
    sock: UnixStream,
    generation: u64,
    started: Instant,
    /// Stopped on purpose (disabled, hung): no restart.
    stopping: bool,
}

struct State {
    events: Sender<crate::Msg>,
    me: Sender<Cmd>,
    cfg: Search,
    verbose: bool,
    child: Option<Running>,
    generation: u64,
    backoff: Duration,
    next_start: Option<Instant>,
    status: Option<Status>,
    /// Last "search unavailable" notification, so a hotkey pressed again and again doesn't
    /// stack them up.
    noticed: Option<Instant>,
    /// Where the UI's answer to a client's `status` goes.
    waiter: Arc<Mutex<Option<Sender<String>>>>,
}

impl State {
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            let now = Instant::now();
            let warned = self.status.as_ref().is_some_and(|s| s.warn);
            let healthy_at = self.child.as_ref().filter(|_| warned).map(|c| c.started + HEALTHY_AFTER);
            let wake = [self.next_start, healthy_at].into_iter().flatten().min();
            let cmd = match wake {
                Some(t) => match rx.recv_timeout(t.saturating_duration_since(now)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
            };
            if let Some(c) = cmd {
                self.handle(c);
            }
            let now = Instant::now();
            if self.next_start.is_some_and(|t| t <= now) {
                self.next_start = None;
                if self.cfg.enabled && self.child.is_none() {
                    self.start_child();
                }
            }
            if self.child.as_ref().is_some_and(|c| now >= c.started + HEALTHY_AFTER) {
                self.backoff = BACKOFF_MIN;
                if self.status.as_ref().is_some_and(|s| s.warn) {
                    self.set_status(None);
                }
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Configure(cfg) => {
                let was = self.cfg.enabled;
                self.cfg = cfg;
                if !self.cfg.enabled {
                    if was {
                        eprintln!("mmk: search disabled");
                    }
                    self.stop_child();
                    self.next_start = None;
                    self.set_status(None);
                } else if self.child.is_some() {
                    self.write(&config_line(&self.cfg));
                } else {
                    // Enabled now, or a chance to find a binary installed since.
                    self.backoff = BACKOFF_MIN;
                    self.next_start = None;
                    self.start_child();
                }
            }
            Cmd::Call(c) => {
                let sent = self.child.is_some() && self.write(&Line::new(c.req.verb()));
                if let Some(id) = c.hold {
                    if sent {
                        let (tx, ms) = (self.events.clone(), self.cfg.hold_ms);
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(ms));
                            let _ = tx.send(crate::Msg::SearchTimeout(id));
                        });
                    } else {
                        // Nobody to show a window: stop holding right away.
                        let _ = self.events.send(crate::Msg::SearchTimeout(id));
                    }
                }
                if !sent {
                    self.notice_unavailable();
                }
            }
            Cmd::Send(l) => {
                if self.child.is_some() {
                    self.write(&l);
                }
            }
            Cmd::Client(l, reply) => {
                if !self.cfg.enabled {
                    let _ = reply.send(error_line("search is disabled ([search] enabled = false)"));
                } else if self.child.is_none() {
                    let why = self.status.as_ref().map_or("search is not running".into(), |s| s.text.to_lowercase());
                    let _ = reply.send(error_line(&why));
                } else if l.verb == "status" {
                    *self.waiter.lock().unwrap() = Some(reply);
                    self.write(&l);
                } else if self.write(&l) {
                    let _ = reply.send("ok".into());
                } else {
                    let _ = reply.send(error_line("search is not running"));
                }
            }
            Cmd::Exited(generation) => self.reap(generation),
        }
    }

    /// The hotkey did nothing visible: say why on the desktop (notify-send, if installed).
    fn notice_unavailable(&mut self) {
        if self.noticed.is_some_and(|t| t.elapsed() < NOTICE_EVERY) {
            return;
        }
        self.noticed = Some(Instant::now());
        let why = match (&self.status, self.cfg.enabled) {
            (_, false) => "Search is turned off ([search] enabled = false).".to_string(),
            (Some(s), _) => format!("{}.", s.text),
            (None, _) => "The search window is starting, try again in a moment.".to_string(),
        };
        let q = |t: &str| format!("'{}'", t.replace('\'', "'\\''"));
        crate::spawn_detached(&format!("notify-send -a mmk -i system-search {} {}", q("mmk search"), q(&why)));
    }

    fn set_status(&mut self, s: Option<Status>) {
        if self.status != s {
            self.status = s.clone();
            let _ = self.events.send(crate::Msg::SearchStatus(s));
        }
    }

    fn start_child(&mut self) {
        let Some(bin) = ui_binary() else {
            let text = format!("Search unavailable: {UI_BIN} not found");
            if self.status.as_ref().map(|s| &s.text) != Some(&text) {
                eprintln!("mmk: search unavailable: {UI_BIN} is neither next to mmk nor on $PATH");
            }
            // Not retried until the next config reload: nothing to spin on.
            self.set_status(Some(Status { text, warn: false }));
            return;
        };
        match spawn_ui(&bin) {
            Ok((proc, sock)) => {
                self.generation += 1;
                let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
                if self.verbose {
                    eprintln!("mmk: started {} (pid {})", bin.display(), proc.id());
                }
                match sock.try_clone() {
                    Ok(r) => {
                        let (events, me, waiter, generation) = (self.events.clone(), self.me.clone(), self.waiter.clone(), self.generation);
                        thread::spawn(move || read_ui(r, events, waiter, me, generation));
                    }
                    Err(e) => eprintln!("mmk: search: {e}"),
                }
                self.child = Some(Running { proc, sock, generation: self.generation, started: Instant::now(), stopping: false });
                self.write(&config_line(&self.cfg));
                if self.status.as_ref().is_some_and(|s| !s.warn) {
                    self.set_status(None);
                }
            }
            Err(e) => {
                eprintln!("mmk: cannot start {}: {e}", bin.display());
                self.crashed(format!("cannot start: {e}"));
            }
        }
    }

    /// Write one line to the UI. A failure (hung or gone) kills it; the reader then reaps it.
    fn write(&mut self, l: &Line) -> bool {
        let Some(c) = &mut self.child else { return false };
        let mut s = l.to_string();
        s.push('\n');
        match c.sock.write_all(s.as_bytes()) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("mmk: search-ui not answering ({e}); restarting it");
                unsafe { libc::kill(c.proc.id() as i32, libc::SIGKILL) };
                false
            }
        }
    }

    fn stop_child(&mut self) {
        if let Some(c) = &mut self.child {
            c.stopping = true;
            unsafe { libc::kill(c.proc.id() as i32, libc::SIGTERM) };
        }
    }

    fn reap(&mut self, generation: u64) {
        if self.child.as_ref().is_none_or(|c| c.generation != generation) {
            return;
        }
        let mut c = self.child.take().unwrap();
        // EOF normally means it exited; if it only closed the socket, make sure.
        let status = match c.proc.try_wait() {
            Ok(Some(s)) => s,
            _ => {
                let _ = c.proc.kill();
                match c.proc.wait() {
                    Ok(s) => s,
                    Err(_) => return,
                }
            }
        };
        if !self.cfg.enabled {
            return;
        }
        if c.stopping {
            // Disabled and enabled again before it was gone.
            self.start_child();
            return;
        }
        if c.started.elapsed() >= HEALTHY_AFTER {
            self.backoff = BACKOFF_MIN;
        }
        eprintln!("mmk: search-ui exited ({status}); restarting in {} s", self.backoff.as_secs());
        self.crashed(format!("exited ({status})"));
    }

    fn crashed(&mut self, why: String) {
        self.next_start = Some(Instant::now() + self.backoff);
        self.set_status(Some(Status { text: format!("Search keeps failing: {why}"), warn: true }));
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }
}

fn config_line(cfg: &Search) -> Line {
    Line::new("config").arg("toml", cfg.toml.as_str())
}

fn error_line(msg: &str) -> String {
    Line::new("error").arg("msg", msg).to_string()
}

/// Start the UI with one end of a socketpair as its fd 3.
fn spawn_ui(bin: &Path) -> io::Result<(Child, UnixStream)> {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (ours, theirs) = unsafe { (UnixStream::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let fd = theirs.as_raw_fd();
    let mut c = Command::new(bin);
    c.args(["--fd", "3"]).stdin(Stdio::null());
    unsafe {
        c.pre_exec(move || {
            if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0);
            } else if libc::dup2(fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Die with mmk. And mmk blocks SIGTERM & co. for its signal thread: unblock.
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = c.spawn()?;
    drop(theirs);
    Ok((child, ours))
}

/// Turn the UI's lines into engine events and client answers, until it goes away.
fn read_ui(sock: UnixStream, events: Sender<crate::Msg>, waiter: Arc<Mutex<Option<Sender<String>>>>, me: Sender<Cmd>, generation: u64) {
    use crate::Msg as M;
    for line in BufReader::new(sock).lines() {
        let Ok(line) = line else { break };
        let Some(m) = Line::parse(&line) else { continue };
        let ev = match m.verb.as_str() {
            "shown" => Some(M::SearchShown),
            "focused" => Some(M::SearchFocused),
            "hidden" => Some(M::SearchHidden),
            "mode" => Some(M::SearchMode(if m.get("name") == Some("terminal") { SearchMode::Terminal } else { SearchMode::Search })),
            "status" | "ok" | "error" => {
                let mut w = waiter.lock().unwrap();
                match w.as_ref() {
                    Some(tx) => {
                        let _ = tx.send(line.clone());
                        if m.verb != "status" {
                            *w = None; // end of the answer
                        }
                    }
                    None if m.verb == "error" => eprintln!("mmk: search-ui: {}", m.get("msg").unwrap_or(&line)),
                    None => {}
                }
                None
            }
            _ => None,
        };
        if let Some(ev) = ev {
            let _ = events.send(ev);
        }
    }
    waiter.lock().unwrap().take();
    let _ = events.send(M::SearchGone);
    let _ = me.send(Cmd::Exited(generation));
}

/// Accept `mmk search` clients on the socket.
fn listen(sup: Supervisor) {
    let path = socket_path();
    let dir = path.parent().unwrap();
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // We hold the instance lock: any socket file there is a stale one.
    let _ = std::fs::remove_file(&path);
    let l = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mmk: {}: {e}; `mmk search` won't reach this mmk", path.display());
            return;
        }
    };
    for conn in l.incoming().flatten() {
        let sup = sup.clone();
        thread::spawn(move || serve(conn, &sup));
    }
}

fn serve(conn: UnixStream, sup: &Supervisor) {
    let _ = conn.set_read_timeout(Some(REPLY_TIMEOUT));
    let Ok(mut w) = conn.try_clone() else { return };
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        let Some(m) = Line::parse(&line) else { continue };
        let mut answer = String::new();
        if !CLIENT_VERBS.contains(&m.verb.as_str()) {
            answer = error_line(&format!("unknown request {:?}", m.verb)) + "\n";
        } else {
            let rx = sup.client(m);
            let deadline = Instant::now() + REPLY_TIMEOUT;
            loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(l) => {
                        answer += &l;
                        answer.push('\n');
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        answer += &(error_line("search did not answer") + "\n");
                        break;
                    }
                }
            }
        }
        if w.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

/// `mmk search ...`: send one request to the running mmk (starting it if needed) and
/// print the answer. Returns the exit code.
pub fn client(mut req: Line, config: &Path) -> i32 {
    if matches!(req.verb.as_str(), "toggle" | "show" | "terminal") {
        if let Ok(t) = std::env::var("XDG_ACTIVATION_TOKEN") {
            req = req.arg("token", t);
        }
        if let Ok(id) = std::env::var("DESKTOP_STARTUP_ID") {
            req = req.arg("startup-id", id);
        }
    }
    let path = socket_path();
    let conn = match UnixStream::connect(&path) {
        Ok(c) => c,
        Err(_) => match start_mmk(&path, config) {
            Some(c) => c,
            None => return 1,
        },
    };
    match exchange(conn, &req) {
        Ok(answer) => {
            let mut code = 0;
            for l in answer.lines() {
                match Line::parse(l) {
                    Some(m) if m.verb == "error" => {
                        eprintln!("mmk: {}", m.get("msg").unwrap_or("error"));
                        code = 1;
                    }
                    Some(m) if m.verb == "ok" => {}
                    Some(_) => println!("{l}"),
                    None => {}
                }
            }
            code
        }
        Err(e) => {
            eprintln!("mmk: {}: {e}", path.display());
            1
        }
    }
}

fn exchange(mut conn: UnixStream, req: &Line) -> io::Result<String> {
    conn.set_read_timeout(Some(REPLY_TIMEOUT + Duration::from_secs(1)))?;
    writeln!(conn, "{req}")?;
    conn.shutdown(std::net::Shutdown::Write)?;
    let mut s = String::new();
    conn.read_to_string(&mut s)?;
    Ok(s)
}

/// No mmk answering: start one detached, as autostart does, and wait for its socket.
fn start_mmk(path: &Path, config: &Path) -> Option<UnixStream> {
    let running = crate::install::running_pid();
    if running.is_none() {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("mmk: {e}");
                return None;
            }
        };
        let log = crate::install::log_path();
        let _ = std::fs::create_dir_all(log.parent().unwrap());
        let q = |p: &Path| p.to_string_lossy().replace('\'', "'\\''");
        crate::spawn_detached(&format!("exec '{}' -c '{}' run --log '{}'", q(&exe), q(config), q(&log)));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(c) = UnixStream::connect(path) {
            return Some(c);
        }
        if Instant::now() >= deadline {
            match running {
                Some(pid) => eprintln!("mmk: mmk (pid {pid}) is running but not answering on {}; restart it", path.display()),
                None => eprintln!("mmk: started mmk, but it did not open {} (see {})", path.display(), crate::install::log_path().display()),
            }
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// `mmk search --query TEXT`: ranked results without a window, straight from the UI binary.
pub fn query(text: &str, config: &Path) -> i32 {
    let Some(bin) = ui_binary() else {
        eprintln!("mmk: {UI_BIN} not found (next to mmk or on $PATH)");
        return 1;
    };
    let e = Command::new(&bin).arg("--query").arg(text).arg("--config").arg(config).exec();
    eprintln!("mmk: {}: {e}", bin.display());
    1
}
