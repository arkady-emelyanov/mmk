//! Finding, grabbing and reading Apple keyboards, with hotplug.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::Msg;
use crate::sys::{self, InputDevice, EV_KEY};

const INPUT_DIR: &str = "/dev/input";

/// All event nodes that are Apple keyboards: (path, name).
pub fn scan() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(INPUT_DIR) else { return out };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event")))
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(d) = InputDevice::open(&p) {
            if d.is_apple_keyboard() {
                out.push((p, d.name));
            }
        }
    }
    out
}

pub struct Manager {
    tx: Sender<Msg>,
    /// Grabbed keyboards: path -> (name, device handle for LED writes).
    devs: Mutex<HashMap<PathBuf, (String, Arc<InputDevice>)>>,
    /// Paths an attach thread is currently working on (one attempt per node at a time).
    claimed: Mutex<HashSet<PathBuf>>,
    /// Current problems per node (e.g. grabbed by another program); cleared when it goes away.
    errors: Mutex<HashMap<PathBuf, String>>,
    /// Restrict to one device node (testing / debugging).
    only: Option<PathBuf>,
    grab_wait: Duration,
    open_retry: Duration,
}

impl Manager {
    pub fn new(tx: Sender<Msg>, only: Option<PathBuf>, grab_wait_ms: u64, open_retry_ms: u64) -> Arc<Self> {
        Arc::new(Manager {
            tx,
            devs: Mutex::new(HashMap::new()),
            claimed: Mutex::new(HashSet::new()),
            errors: Mutex::new(HashMap::new()),
            only,
            grab_wait: Duration::from_millis(grab_wait_ms),
            open_retry: Duration::from_millis(open_retry_ms),
        })
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.devs.lock().unwrap().values().map(|(n, _)| n.clone()).collect();
        v.sort();
        v
    }

    pub fn errors(&self) -> Vec<String> {
        let mut v: Vec<String> = self.errors.lock().unwrap().values().cloned().collect();
        v.sort();
        v
    }

    /// Grab every Apple keyboard present now and watch for new ones.
    pub fn start(self: &Arc<Self>) {
        let paths: Vec<PathBuf> = match &self.only {
            Some(p) => vec![p.clone()],
            None => scan().into_iter().map(|(p, _)| p).collect(),
        };
        for p in paths {
            let m = self.clone();
            thread::spawn(move || m.attach(&p, false));
        }
        let m = self.clone();
        thread::spawn(move || {
            if let Err(e) = m.watch() {
                eprintln!("mmk: hotplug watch failed: {e}");
            }
        });
    }

    /// Mirror an LED change (Caps Lock etc.) to every grabbed keyboard.
    pub fn set_led(&self, code: u16, value: i32) {
        for (_, d) in self.devs.lock().unwrap().values() {
            let _ = d.write_event(sys::EV_LED, code, value);
        }
    }

    fn attach(self: Arc<Self>, path: &Path, hotplug: bool) {
        if let Some(only) = &self.only {
            if only != path {
                return;
            }
        }
        // inotify reports several events per node (create, attribute changes): handle each
        // node once, and never try to grab a node we already hold.
        if self.devs.lock().unwrap().contains_key(path) || !self.claimed.lock().unwrap().insert(path.to_path_buf()) {
            return;
        }
        self.clone().attach_claimed(path, hotplug);
        self.claimed.lock().unwrap().remove(path);
    }

    fn attach_claimed(self: Arc<Self>, path: &Path, hotplug: bool) {
        // Udev may still be setting permissions on a fresh node: retry for a while.
        let deadline = Instant::now() + if hotplug { self.open_retry } else { Duration::ZERO };
        let dev = loop {
            match InputDevice::open(path) {
                Ok(d) => break d,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    if !hotplug {
                        eprintln!("mmk: {}: {e}", path.display());
                    }
                    return;
                }
            }
        };
        if !dev.is_apple_keyboard() && self.only.is_none() {
            return;
        }
        // Grabbing while a key is down would leave it stuck in the desktop.
        let deadline = Instant::now() + self.grab_wait;
        while !dev.all_keys_up() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if let Err(e) = dev.grab() {
            let hint = if e.raw_os_error() == Some(libc::EBUSY) {
                " (already grabbed by another program, e.g. another key remapper?)"
            } else {
                ""
            };
            eprintln!("mmk: cannot grab {} {}: {e}{hint}", dev.name, path.display());
            self.errors.lock().unwrap().insert(path.to_path_buf(), format!("Cannot grab {}{hint}", dev.name));
            let _ = self.tx.send(Msg::DevicesChanged);
            return;
        }
        let dev = Arc::new(dev);
        let name = dev.name.clone();
        {
            let mut devs = self.devs.lock().unwrap();
            if devs.contains_key(path) {
                return;
            }
            devs.insert(path.to_path_buf(), (name.clone(), dev.clone()));
        }
        self.errors.lock().unwrap().remove(path);
        eprintln!("mmk: grabbed {name} ({})", path.display());
        let _ = self.tx.send(Msg::DevicesChanged);

        let mut file = match dev.file.try_clone() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("mmk: {}: {e}", path.display());
                return;
            }
        };
        let tx = self.tx.clone();
        let r = sys::read_events(&mut file, |ty, code, value| {
            if ty == EV_KEY {
                let _ = tx.send(Msg::Key(code, value));
            }
        });
        self.devs.lock().unwrap().remove(path);
        eprintln!("mmk: lost {name} ({}): {}", path.display(), r.err().map(|e| e.to_string()).unwrap_or_default());
        let _ = self.tx.send(Msg::DeviceGone);
        let _ = self.tx.send(Msg::DevicesChanged);
    }

    /// inotify on /dev/input: new event nodes (USB plug-in, Bluetooth reconnect).
    fn watch(self: &Arc<Self>) -> io::Result<()> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let dir = std::ffi::CString::new(INPUT_DIR).unwrap();
        let mask = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE;
        if unsafe { libc::inotify_add_watch(fd, dir.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        for (mask, name) in inotify_events(fd) {
            if !name.starts_with("event") {
                continue;
            }
            let path = Path::new(INPUT_DIR).join(&name);
            if mask & libc::IN_DELETE != 0 {
                if self.errors.lock().unwrap().remove(&path).is_some() {
                    let _ = self.tx.send(Msg::DevicesChanged);
                }
                continue;
            }
            let m = self.clone();
            thread::spawn(move || m.attach(&path, true));
        }
        Ok(())
    }
}

/// Blocking iterator over file names reported by an inotify fd.
pub fn inotify_names(fd: i32) -> impl Iterator<Item = String> {
    inotify_events(fd).map(|(_, name)| name)
}

/// Blocking iterator over (mask, file name) reported by an inotify fd.
pub fn inotify_events(fd: i32) -> impl Iterator<Item = (u32, String)> {
    let mut pending: Vec<(u32, String)> = Vec::new();
    std::iter::from_fn(move || {
        while pending.is_empty() {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
            let mut off = 0;
            while off + 16 <= n as usize {
                let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                let name = &buf[off + 16..off + 16 + len];
                if let Ok(c) = CStr::from_bytes_until_nul(name) {
                    pending.push((mask, c.to_string_lossy().into_owned()));
                }
                off += 16 + len;
            }
        }
        Some(pending.remove(0))
    })
}
