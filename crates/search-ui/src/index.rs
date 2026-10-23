//! File name index (PLAN §16.11): the files and folders under `[search.files] roots`, in a
//! compact arena that is cached on disk and kept live with inotify.
//!
//! One index thread does all the writing: walks (ignore's parallel walker, whose threads only
//! stat, add watches and send), inotify events (each marks its folder dirty; dirty folders are
//! re-listed and reconciled), periodic rescans of folders left without a watch, and the cache.
//! Readers take the store's read lock briefly.

use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};
use std::{mem, thread};

use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use mmk_common::search_config::Files;

const NONE: u32 = u32::MAX;
const FREE: u8 = 0;
const FILE: u8 = 1;
const DIR: u8 = 2;
/// Symlink, socket, fifo, device.
const OTHER: u8 = 3;

/// Cache layout version: bump on any change to `Entry` or the file format.
const FORMAT: u32 = 1;
const MAGIC: &[u8; 8] = b"MMKFIDX\0";
const HEADER: usize = 48;

/// Minimum time between `on_change` calls, and while a walk is running.
const NOTIFY_GAP: Duration = Duration::from_millis(200);
const NOTIFY_GAP_SCANNING: Duration = Duration::from_secs(1);
/// Events are collected this long before the dirty folders are re-listed.
const SETTLE: Duration = Duration::from_millis(100);
const SAVE_EVERY: Duration = Duration::from_secs(300);
/// Items applied per write lock during a walk.
const BATCH: usize = 4096;
/// Entries copied out per read lock by `for_each`.
const CHUNK: usize = 65536;

/// `inotify_add_watch` fails with EEXIST if the inode is already watched (Linux 4.18).
const IN_MASK_CREATE: u32 = 0x1000_0000;
const WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW
    | libc::IN_EXCL_UNLINK;

/// One file or folder, 24 bytes. Roots have `parent == NONE` and their absolute path as name.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Entry {
    parent: u32,
    /// First child and next sibling: a folder's children are a singly linked list.
    child: u32,
    next: u32,
    /// The name is `names[name..name + len]`.
    name: u32,
    /// Seconds since the epoch.
    mtime: u32,
    len: u16,
    kind: u8,
    _pad: u8,
}

const _: () = assert!(mem::size_of::<Entry>() == 24);

/// The arena. Ids are indexes into `e`; freed ids are reused, but only after the next
/// `on_change` (`pending` -> `free`), so a stale id never names another file unannounced.
#[derive(Default)]
struct Store {
    e: Vec<Entry>,
    names: Vec<u8>,
    /// Bytes of `names` owned by freed entries; compacted when large.
    garbage: usize,
    /// Live entries, roots excluded.
    live: usize,
    free: Vec<u32>,
    pending: Vec<u32>,
    /// Open-addressing table (linear probing) of non-root ids, keyed by (parent, name).
    slots: Vec<u32>,
    used: usize,
    roots: Vec<u32>,
}

impl Store {
    fn new(roots: &[PathBuf]) -> Store {
        let mut s = Store::default();
        s.roots = roots.iter().map(|r| s.insert(NONE, r.as_os_str().as_bytes(), DIR, 0)).collect();
        s
    }

    fn at(&self, id: u32) -> &Entry {
        &self.e[id as usize]
    }

    fn at_mut(&mut self, id: u32) -> &mut Entry {
        &mut self.e[id as usize]
    }

    fn name(&self, id: u32) -> &[u8] {
        let e = self.at(id);
        &self.names[e.name as usize..][..e.len as usize]
    }

    fn is_dir(&self, id: u32) -> bool {
        self.e.get(id as usize).is_some_and(|e| e.kind == DIR)
    }

    fn home(&self, id: u32) -> usize {
        key(self.at(id).parent, self.name(id)) as usize & (self.slots.len() - 1)
    }

    fn find(&self, parent: u32, name: &[u8]) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = key(parent, name) as usize & mask;
        loop {
            let s = self.slots[i];
            if s == NONE {
                return None;
            }
            if self.at(s).parent == parent && self.name(s) == name {
                return Some(s);
            }
            i = (i + 1) & mask;
        }
    }

    fn rebuild_table(&mut self) {
        self.slots = vec![NONE; (self.live * 2).next_power_of_two().max(1024)];
        self.used = 0;
        for id in 0..self.e.len() as u32 {
            let e = self.at(id);
            if e.kind != FREE && e.parent != NONE {
                self.probe_insert(id);
            }
        }
    }

    fn probe_insert(&mut self, id: u32) {
        let mask = self.slots.len() - 1;
        let mut i = self.home(id);
        while self.slots[i] != NONE {
            i = (i + 1) & mask;
        }
        self.slots[i] = id;
        self.used += 1;
    }

    fn table_remove(&mut self, id: u32) {
        let mask = self.slots.len() - 1;
        let mut i = self.home(id);
        while self.slots[i] != id {
            if self.slots[i] == NONE {
                return;
            }
            i = (i + 1) & mask;
        }
        // Backward-shift deletion: pull later members of the probe run into the hole.
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let s = self.slots[j];
            if s == NONE {
                break;
            }
            let k = self.home(s);
            if j.wrapping_sub(k) & mask >= j.wrapping_sub(i) & mask {
                self.slots[i] = s;
                i = j;
            }
        }
        self.slots[i] = NONE;
        self.used -= 1;
    }

    /// Add an entry as the first child of `parent` (a root if `NONE`).
    fn insert(&mut self, parent: u32, name: &[u8], kind: u8, mtime: u32) -> u32 {
        let id = self.free.pop().unwrap_or_else(|| {
            self.e.push(Entry::default());
            (self.e.len() - 1) as u32
        });
        let next = if parent == NONE { NONE } else { self.at(parent).child };
        let name_off = self.names.len() as u32;
        self.names.extend_from_slice(name);
        *self.at_mut(id) = Entry { parent, child: NONE, next, name: name_off, mtime, len: name.len() as u16, kind, _pad: 0 };
        if parent != NONE {
            self.at_mut(parent).child = id;
            self.live += 1;
            if (self.used + 1) * 10 > self.slots.len() * 7 {
                self.rebuild_table();
            } else {
                self.probe_insert(id);
            }
        }
        id
    }

    /// Take `id` out of its parent's child list.
    fn unlink(&mut self, id: u32) {
        let (p, next) = (self.at(id).parent, self.at(id).next);
        if self.at(p).child == id {
            self.at_mut(p).child = next;
            return;
        }
        let mut c = self.at(p).child;
        while c != NONE {
            if self.at(c).next == id {
                self.at_mut(c).next = next;
                return;
            }
            c = self.at(c).next;
        }
    }

    /// Free an (unlinked) entry and everything below it; freed folders go to `dirs`.
    fn free(&mut self, id: u32, dirs: &mut Vec<u32>) {
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            let mut c = self.at(x).child;
            while c != NONE {
                stack.push(c);
                c = self.at(c).next;
            }
            if self.at(x).kind == DIR {
                dirs.push(x);
            }
            self.table_remove(x);
            self.live -= 1;
            self.garbage += self.at(x).len as usize;
            let e = self.at_mut(x);
            e.kind = FREE;
            e.child = NONE;
            self.pending.push(x);
        }
    }

    /// Free the children of `dir` for which `keep` is false. Returns whether any was freed.
    fn retain_children(&mut self, dir: u32, keep: impl Fn(u32) -> bool, dirs: &mut Vec<u32>) -> bool {
        let (mut c, mut prev, mut any) = (self.at(dir).child, NONE, false);
        while c != NONE {
            let next = self.at(c).next;
            if keep(c) {
                prev = c;
            } else {
                if prev == NONE {
                    self.at_mut(dir).child = next;
                } else {
                    self.at_mut(prev).next = next;
                }
                self.free(c, dirs);
                any = true;
            }
            c = next;
        }
        any
    }

    /// Path of `id`: absolute, or relative to its root.
    fn path_bytes(&self, id: u32, rel: bool) -> Option<Vec<u8>> {
        let mut chain = Vec::new();
        let mut x = id;
        loop {
            let e = self.e.get(x as usize)?;
            if e.kind == FREE || chain.len() > 4096 {
                return None;
            }
            if e.parent == NONE {
                if !rel {
                    chain.push(x);
                }
                break;
            }
            chain.push(x);
            x = e.parent;
        }
        let mut out = Vec::new();
        for &x in chain.iter().rev() {
            if !out.is_empty() && !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(self.name(x));
        }
        Some(out)
    }

    fn path(&self, id: u32) -> Option<PathBuf> {
        self.path_bytes(id, false).map(|b| PathBuf::from(OsString::from_vec(b)))
    }

    fn compact(&mut self) {
        let mut names = Vec::with_capacity(self.names.len() - self.garbage);
        for x in &mut self.e {
            if x.kind != FREE {
                let off = names.len() as u32;
                names.extend_from_slice(&self.names[x.name as usize..][..x.len as usize]);
                x.name = off;
            }
        }
        self.names = names;
        self.garbage = 0;
    }

    /// Cache file: a 48-byte header (magic, format, entry size, config hash, entry count, name
    /// bytes, checksum), the raw entries, the name buffer. Written to a temporary file and renamed.
    fn save(&self, path: &Path, hash: u64) -> io::Result<()> {
        if let Some(d) = path.parent() {
            fs::create_dir_all(d)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let eb = bytes(&self.e);
        let mut h = Vec::with_capacity(HEADER);
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&FORMAT.to_ne_bytes());
        h.extend_from_slice(&(mem::size_of::<Entry>() as u32).to_ne_bytes());
        h.extend_from_slice(&hash.to_ne_bytes());
        h.extend_from_slice(&(self.e.len() as u64).to_ne_bytes());
        h.extend_from_slice(&(self.names.len() as u64).to_ne_bytes());
        h.extend_from_slice(&checksum(checksum(0, eb), &self.names).to_ne_bytes());
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(&h)?;
        w.write_all(eb)?;
        w.write_all(&self.names)?;
        w.flush()?;
        drop(w);
        fs::rename(&tmp, path)
    }

    /// Load a cache written by `save` for this scope; anything else (other format or config,
    /// truncated, corrupt) is an error.
    fn load(path: &Path, scope: &Scope) -> io::Result<Store> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "stale or corrupt file index cache");
        let mut f = File::open(path)?;
        let size = f.metadata()?.len();
        let mut h = [0u8; HEADER];
        f.read_exact(&mut h)?;
        let u32at = |i: usize| u32::from_ne_bytes(h[i..i + 4].try_into().unwrap());
        let u64at = |i: usize| u64::from_ne_bytes(h[i..i + 8].try_into().unwrap());
        if &h[..8] != MAGIC || u32at(8) != FORMAT || u32at(12) as usize != mem::size_of::<Entry>() || u64at(16) != scope.hash {
            return Err(bad());
        }
        let (n, nn) = (u64at(24), u64at(32));
        if n.checked_mul(24).and_then(|b| b.checked_add(nn + HEADER as u64)) != Some(size) || n >= NONE as u64 {
            return Err(bad());
        }
        let mut st = Store { e: vec![Entry::default(); n as usize], names: vec![0; nn as usize], ..Store::default() };
        f.read_exact(bytes_mut(&mut st.e))?;
        f.read_exact(&mut st.names)?;
        if checksum(checksum(0, bytes(&st.e)), &st.names) != u64at(40) {
            return Err(bad());
        }
        let ok = |x: u32| x == NONE || (x as u64) < n;
        let mut used = 0;
        let mut roots = Vec::new();
        for (i, x) in st.e.iter().enumerate() {
            if x.kind == FREE {
                st.free.push(i as u32);
                continue;
            }
            if x.kind > OTHER || !ok(x.parent) || !ok(x.child) || !ok(x.next) || x.name as usize + x.len as usize > st.names.len() {
                return Err(bad());
            }
            used += x.len as usize;
            if x.parent == NONE {
                roots.push(i as u32);
            } else {
                st.live += 1;
            }
        }
        st.garbage = st.names.len() - used;
        for r in &scope.roots {
            let id = roots.iter().copied().find(|&id| st.name(id) == r.as_os_str().as_bytes()).ok_or_else(bad)?;
            st.roots.push(id);
        }
        st.rebuild_table();
        Ok(st)
    }
}

fn bytes(e: &[Entry]) -> &[u8] {
    // SAFETY: `Entry` is repr(C), all integers, no padding.
    unsafe { std::slice::from_raw_parts(e.as_ptr().cast(), mem::size_of_val(e)) }
}

fn bytes_mut(e: &mut [Entry]) -> &mut [u8] {
    // SAFETY: as in `bytes`; every bit pattern is a valid `Entry`.
    unsafe { std::slice::from_raw_parts_mut(e.as_mut_ptr().cast(), mem::size_of_val(e)) }
}

fn checksum(mut h: u64, b: &[u8]) -> u64 {
    let mut step = |w: u64| h = (h ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(23);
    let mut it = b.chunks_exact(8);
    for c in &mut it {
        step(u64::from_ne_bytes(c.try_into().unwrap()));
    }
    for &x in it.remainder() {
        step(x as u64);
    }
    h
}

fn fnv(mut h: u64, b: &[u8]) -> u64 {
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn key(parent: u32, name: &[u8]) -> u64 {
    let h = fnv(fnv(0xcbf2_9ce4_8422_2325, &parent.to_ne_bytes()), name);
    h ^ (h >> 32)
}

/// `*` and `?` glob over bytes.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => (0..=s.len()).any(|i| glob(rest, &s[i..])),
        Some((&c, rest)) => s.first().is_some_and(|&b| b == c || c == b'?') && glob(rest, &s[1..]),
    }
}

/// `[search.files]`, resolved.
struct Scope {
    roots: Vec<PathBuf>,
    /// Name globs (`.git`, `*.tmp`) and absolute path prefixes (`~/snap`).
    names: Vec<Vec<u8>>,
    prefixes: Vec<PathBuf>,
    hidden: bool,
    respect_ignore: bool,
    max_watches: usize,
    rescan: Duration,
    /// Of everything that decides the contents; stored in the cache.
    hash: u64,
}

impl Scope {
    fn new(c: &Files) -> Scope {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let expand = |s: &str| match s.strip_prefix('~') {
            Some("") => home.clone(),
            Some(r) if r.starts_with('/') => home.join(&r[1..]),
            _ => PathBuf::from(s),
        };
        let mut roots: Vec<PathBuf> = Vec::new();
        for r in c.roots.iter().map(|r| expand(r)) {
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        // A root inside another would be indexed twice.
        let all = roots.clone();
        roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
        let (mut names, mut prefixes) = (Vec::new(), Vec::new());
        for x in &c.exclude {
            if x.starts_with('/') || x == "~" || x.starts_with("~/") {
                prefixes.push(expand(x));
            } else {
                names.push(x.as_bytes().to_vec());
            }
        }
        let mut h = fnv(0xcbf2_9ce4_8422_2325, &FORMAT.to_ne_bytes());
        for r in &roots {
            h = fnv(h, r.as_os_str().as_bytes());
            h = fnv(h, b"\0");
        }
        for x in names.iter().chain(prefixes.iter().map(|p| p.as_os_str().as_bytes().to_vec()).collect::<Vec<_>>().iter()) {
            h = fnv(fnv(h, x), b"\x01");
        }
        h = fnv(h, &[c.hidden as u8, c.respect_ignore as u8]);
        Scope {
            roots,
            names,
            prefixes,
            hidden: c.hidden,
            respect_ignore: c.respect_ignore,
            max_watches: c.max_watches,
            rescan: Duration::from_secs(c.rescan_minutes.max(1) * 60),
            hash: h,
        }
    }

    fn excluded(&self, path: &Path) -> bool {
        let name = path.file_name().map_or(&[][..], |n| n.as_bytes());
        self.names.iter().any(|g| glob(g, name)) || self.prefixes.iter().any(|p| path.starts_with(p))
    }
}

/// What `status` reports (and `mmk doctor` shows).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexStatus {
    pub entries: usize,
    pub scanning: bool,
    pub watches: usize,
    pub watch_limit_hit: bool,
    pub last_error: Option<String>,
}

struct Shared {
    store: RwLock<Store>,
    stop: AtomicBool,
    reindex: AtomicBool,
    scanning: AtomicBool,
    watches: AtomicUsize,
    limit_hit: AtomicBool,
    error: Mutex<Option<String>>,
    /// eventfd waking the index thread.
    wake: i32,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Shared {
    fn set_error(&self, e: String) {
        *self.error.lock().unwrap() = Some(e);
    }

    fn wake(&self) {
        let one = 1u64.to_ne_bytes();
        unsafe { libc::write(self.wake, one.as_ptr().cast(), 8) };
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe { libc::close(self.wake) };
    }
}

/// The inotify fd, shared with the walker threads, which add the watches.
struct Watcher {
    ino: i32,
    max: usize,
    sh: Arc<Shared>,
}

impl Watcher {
    /// Watch a folder; its watch descriptor, or -1 (no inotify, over the limit, error).
    fn add(&self, path: &Path) -> i32 {
        if self.ino < 0 {
            return -1;
        }
        if self.sh.watches.load(Relaxed) >= self.max {
            self.sh.limit_hit.store(true, Relaxed);
            return -1;
        }
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return -1 };
        let wd = unsafe { libc::inotify_add_watch(self.ino, c.as_ptr(), WATCH_MASK | IN_MASK_CREATE) };
        if wd >= 0 {
            self.sh.watches.fetch_add(1, Relaxed);
            return wd;
        }
        match io::Error::last_os_error().raw_os_error() {
            // Already watched: get its descriptor.
            Some(libc::EEXIST) => unsafe { libc::inotify_add_watch(self.ino, c.as_ptr(), WATCH_MASK) },
            Some(libc::ENOSPC) => {
                if !self.sh.limit_hit.swap(true, Relaxed) {
                    self.sh.set_error("inotify watch limit reached (fs.inotify.max_user_watches)".into());
                }
                -1
            }
            _ => -1,
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if self.ino >= 0 {
            unsafe { libc::close(self.ino) };
        }
    }
}

/// Background priority for the calling thread and the threads it creates later: nice 19, idle
/// I/O class and, with `idle_cpu`, SCHED_IDLE. Best effort.
fn lower_priority(idle_cpu: bool) {
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::pid_t;
        libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19);
        libc::syscall(libc::SYS_ioprio_set, 1, tid, 3 << 13); // IOPRIO_WHO_PROCESS, IOPRIO_CLASS_IDLE
        if idle_cpu {
            libc::sched_setscheduler(tid, libc::SCHED_IDLE, &libc::sched_param { sched_priority: 0 });
        }
    }
}

/// A walked entry, sent from a walker thread to the index thread.
struct Item {
    path: PathBuf,
    depth: usize,
    kind: u8,
    mtime: u32,
    wd: i32,
}

/// Walker visitor: batches items, sending at every folder so that a folder always reaches the
/// index thread before its children (which are only listed after it is visited).
struct Visit {
    tx: mpsc::SyncSender<Vec<Item>>,
    w: Arc<Watcher>,
    buf: Vec<Item>,
}

impl<'s> ParallelVisitorBuilder<'s> for Visit {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Visit { tx: self.tx.clone(), w: self.w.clone(), buf: Vec::new() })
    }
}

impl ParallelVisitor for Visit {
    fn visit(&mut self, r: Result<DirEntry, ignore::Error>) -> WalkState {
        if self.w.sh.stop.load(Relaxed) {
            return WalkState::Quit;
        }
        let Ok(e) = r else { return WalkState::Continue };
        let kind = match e.file_type() {
            Some(t) if t.is_dir() => DIR,
            Some(t) if t.is_file() => FILE,
            _ => OTHER,
        };
        let mtime = e.metadata().map_or(0, |m| m.mtime().clamp(0, u32::MAX as i64) as u32);
        let wd = if kind == DIR { self.w.add(e.path()) } else { -1 };
        let depth = e.depth();
        self.buf.push(Item { path: e.into_path(), depth, kind, mtime, wd });
        if kind == DIR || self.buf.len() >= 256 {
            self.flush();
        }
        WalkState::Continue
    }
}

impl Visit {
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.tx.send(mem::take(&mut self.buf));
        }
    }
}

impl Drop for Visit {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Walker filter, for entries below a walk's roots: `exclude`, other filesystems. Folders that
/// stay are watched here, before the walker lists them, so nothing created meanwhile is missed.
fn keep(scope: &Scope, w: &Watcher, e: &DirEntry) -> bool {
    if scope.excluded(e.path()) {
        return false;
    }
    if e.file_type().is_some_and(|t| t.is_dir()) {
        let dev = |p: &Path| fs::symlink_metadata(p).map(|m| m.dev()).ok();
        if dev(e.path()) != e.path().parent().and_then(dev) {
            return false;
        }
        w.add(e.path());
    }
    true
}

/// Per-walk bookkeeping.
#[derive(Default)]
struct Walked {
    seen: Vec<u64>,
    /// Folders whose children were all listed: unseen children are gone.
    listed: Vec<u32>,
    /// New folders at the depth limit: walked next, fully.
    fresh: Vec<u32>,
    /// Freed folders, to unwatch.
    removed: Vec<u32>,
}

impl Walked {
    fn mark(&mut self, id: u32) {
        let i = id as usize / 64;
        if i >= self.seen.len() {
            self.seen.resize(i + 1, 0);
        }
        self.seen[i] |= 1 << (id % 64);
    }

    fn seen(&self, id: u32) -> bool {
        self.seen.get(id as usize / 64).is_some_and(|w| w & (1 << (id % 64)) != 0)
    }
}

/// The index thread.
struct Indexer {
    sh: Arc<Shared>,
    scope: Arc<Scope>,
    w: Arc<Watcher>,
    cache: PathBuf,
    on_change: Box<dyn Fn() + Send + Sync>,
    wd_id: HashMap<i32, u32>,
    id_wd: HashMap<u32, i32>,
    /// Folders without a watch (over `max_watches`), re-listed every `rescan`.
    unwatched: HashSet<u32>,
    dirty: HashSet<u32>,
    dirty_since: Option<Instant>,
    overflow: bool,
    /// The set of entries changed since the last `on_change`.
    changed: bool,
    cache_dirty: bool,
    last_notify: Instant,
    last_save: Instant,
    next_rescan: Instant,
    /// `scan` recursion depth.
    nesting: usize,
}

impl Indexer {
    fn run(mut self, loaded: bool) {
        lower_priority(false);
        if loaded {
            (self.on_change)();
        }
        self.full_scan();
        self.changed = true;
        self.save();
        while !self.sh.stop.load(Relaxed) {
            let now = Instant::now();
            let due = [
                self.dirty_since.map(|t| t + SETTLE),
                self.changed.then_some(self.last_notify + NOTIFY_GAP),
                (!self.unwatched.is_empty()).then_some(self.next_rescan),
                self.cache_dirty.then_some(self.last_save + SAVE_EVERY),
            ];
            let timeout = due.iter().flatten().min().map_or(-1, |t| {
                (t.saturating_duration_since(now).as_micros().div_ceil(1000)).min(60_000) as i32
            });
            let mut fds = [
                libc::pollfd { fd: self.w.ino, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.sh.wake, events: libc::POLLIN, revents: 0 },
            ];
            let mut b = [0u8; 8];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 2, timeout);
                libc::read(self.sh.wake, b.as_mut_ptr().cast(), 8);
            }
            if self.sh.stop.load(Relaxed) {
                break;
            }
            self.drain();
            if self.overflow || self.sh.reindex.swap(false, Relaxed) {
                self.full_scan();
            } else if self.dirty_since.is_some_and(|t| t.elapsed() >= SETTLE) {
                self.dirty_since = None;
                let ids = mem::take(&mut self.dirty);
                let t = self.targets(ids);
                self.scan(t, false);
            }
            if !self.unwatched.is_empty() && Instant::now() >= self.next_rescan {
                self.next_rescan = Instant::now() + self.scope.rescan;
                let t = self.targets(self.unwatched.iter().copied().collect::<Vec<_>>());
                self.scan(t, false);
            }
            self.notify();
            if self.cache_dirty && self.last_save.elapsed() >= SAVE_EVERY {
                self.save();
            }
        }
        if self.cache_dirty {
            self.save();
        }
    }

    /// Live folders among `ids`, with their paths.
    fn targets(&self, ids: impl IntoIterator<Item = u32>) -> Vec<(u32, PathBuf)> {
        let st = self.sh.store.read().unwrap();
        ids.into_iter().filter(|&id| st.is_dir(id)).filter_map(|id| Some((id, st.path(id)?))).collect()
    }

    fn full_scan(&mut self) {
        self.overflow = false;
        self.dirty.clear();
        self.dirty_since = None;
        let mut removed = Vec::new();
        let roots: Vec<(u32, PathBuf)> = {
            let sh = self.sh.clone();
            let mut st = sh.store.write().unwrap();
            let roots: Vec<_> = st.roots.clone().into_iter().map(|id| (id, st.path(id).unwrap())).collect();
            for (id, p) in &roots {
                if !p.is_dir() && st.retain_children(*id, |_| false, &mut removed) {
                    self.changed = true;
                    self.cache_dirty = true;
                }
            }
            roots
        };
        self.forget(removed);
        self.scan(roots, true);
    }

    /// Walk `targets` (fully if `deep`, else their direct children) and reconcile the store
    /// with what was found: update, add, and free what is gone.
    fn scan(&mut self, targets: Vec<(u32, PathBuf)>, deep: bool) {
        if targets.is_empty() || self.sh.stop.load(Relaxed) {
            return;
        }
        self.nesting += 1;
        self.sh.scanning.store(true, Relaxed);
        let max_depth = if deep { usize::MAX } else { 1 };
        let mut b = WalkBuilder::new(&targets[0].1);
        for (_, p) in &targets[1..] {
            b.add(p);
        }
        let r = self.scope.respect_ignore;
        b.hidden(!self.scope.hidden)
            .parents(r)
            .ignore(r)
            .git_ignore(r)
            .git_global(r)
            .git_exclude(r)
            .follow_links(false)
            .threads(thread::available_parallelism().map_or(4, |n| n.get().min(8)));
        if !deep {
            b.max_depth(Some(1));
        }
        let (scope, w) = (self.scope.clone(), self.w.clone());
        b.filter_entry(move |e| keep(&scope, &w, e));
        let walk = b.build_parallel();
        let mut dirs: HashMap<PathBuf, u32> = targets.into_iter().map(|(id, p)| (p, id)).collect();
        let mut wk = Walked::default();
        let (tx, rx) = mpsc::sync_channel::<Vec<Item>>(256);
        let w = self.w.clone();
        let sh = self.sh.clone();
        let mut last_drain = Instant::now();
        thread::scope(|s| {
            s.spawn(move || {
                lower_priority(true);
                walk.visit(&mut Visit { tx, w, buf: Vec::new() });
            });
            while let Ok(first) = rx.recv() {
                if sh.stop.load(Relaxed) {
                    continue;
                }
                let mut st = sh.store.write().unwrap();
                let mut n = 0;
                let mut chunk = Some(first);
                while let Some(items) = chunk {
                    n += items.len();
                    for it in items {
                        self.apply(&mut st, &mut dirs, &mut wk, it, max_depth);
                    }
                    chunk = if n < BATCH { rx.try_recv().ok() } else { None };
                }
                drop(st);
                if last_drain.elapsed() >= SETTLE {
                    last_drain = Instant::now();
                    self.drain();
                    self.notify();
                }
            }
        });
        if self.sh.stop.load(Relaxed) {
            // Partial: freeing what wasn't seen would drop most of the index.
            self.nesting -= 1;
            return;
        }
        {
            let mut st = sh.store.write().unwrap();
            let mut removed = mem::take(&mut wk.removed);
            for &d in &wk.listed {
                if st.is_dir(d) && st.retain_children(d, |c| wk.seen(c), &mut removed) {
                    self.changed = true;
                    self.cache_dirty = true;
                }
            }
            drop(st);
            self.forget(removed);
        }
        for d in wk.listed {
            if self.id_wd.contains_key(&d) {
                self.unwatched.remove(&d);
            } else if sh.store.read().unwrap().is_dir(d) {
                self.unwatched.insert(d);
            }
        }
        if self.unwatched.is_empty() {
            sh.limit_hit.store(false, Relaxed);
        }
        let fresh = self.targets(mem::take(&mut wk.fresh));
        self.scan(fresh, true);
        self.nesting -= 1;
        if self.nesting == 0 {
            sh.scanning.store(false, Relaxed);
        }
        let mut st = sh.store.write().unwrap();
        if st.garbage > 1 << 20 && st.garbage * 2 > st.names.len() {
            st.compact();
        }
    }

    fn apply(&mut self, st: &mut Store, dirs: &mut HashMap<PathBuf, u32>, wk: &mut Walked, it: Item, max_depth: usize) {
        let id = if it.depth == 0 {
            let Some(&id) = dirs.get(&it.path) else { return };
            if it.kind != DIR || !st.is_dir(id) {
                return;
            }
            st.at_mut(id).mtime = it.mtime;
            id
        } else {
            let Some(&parent) = it.path.parent().and_then(|p| dirs.get(p)) else { return };
            let Some(name) = it.path.file_name().map(|n| n.as_bytes()) else { return };
            if !st.is_dir(parent) || name.len() > u16::MAX as usize {
                return;
            }
            match st.find(parent, name) {
                Some(id) if st.at(id).kind == it.kind => {
                    st.at_mut(id).mtime = it.mtime;
                    id
                }
                old => {
                    if let Some(old) = old {
                        st.unlink(old);
                        st.free(old, &mut wk.removed);
                    }
                    let id = st.insert(parent, name, it.kind, it.mtime);
                    self.changed = true;
                    self.cache_dirty = true;
                    if it.kind == DIR && it.depth >= max_depth {
                        wk.fresh.push(id);
                    }
                    id
                }
            }
        };
        wk.mark(id);
        if it.kind == DIR {
            self.bind(it.wd, id);
            if it.depth < max_depth {
                wk.listed.push(id);
                dirs.insert(it.path, id);
            }
        }
    }

    /// Record that `wd` watches folder `id`. A renamed folder keeps its watch (same inode): the
    /// new entry takes it over from the old one.
    fn bind(&mut self, wd: i32, id: u32) {
        if wd < 0 {
            return;
        }
        if let Some(old) = self.wd_id.insert(wd, id)
            && old != id
        {
            self.id_wd.remove(&old);
        }
        if let Some(old) = self.id_wd.insert(id, wd)
            && old != wd
        {
            self.wd_id.remove(&old);
            unsafe { libc::inotify_rm_watch(self.w.ino, old) };
        }
    }

    /// Drop watches and bookkeeping of freed folders.
    fn forget(&mut self, dirs: Vec<u32>) {
        for d in dirs {
            self.unwatched.remove(&d);
            self.dirty.remove(&d);
            if let Some(wd) = self.id_wd.remove(&d) {
                self.wd_id.remove(&wd);
                unsafe { libc::inotify_rm_watch(self.w.ino, wd) };
            }
        }
    }

    /// Read pending inotify events: mark folders dirty.
    fn drain(&mut self) {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = unsafe { libc::read(self.w.ino, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0;
            while off + 16 <= n as usize {
                let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
                off += 16 + u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    self.overflow = true;
                } else if mask & libc::IN_IGNORED != 0 {
                    let _ = self.sh.watches.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
                    // Deleted or unmounted: the parent's listing decides what remains.
                    if let Some(id) = self.wd_id.remove(&wd) {
                        self.id_wd.remove(&id);
                        let st = self.sh.store.read().unwrap();
                        if st.is_dir(id) && st.at(id).parent != NONE {
                            self.dirty.insert(st.at(id).parent);
                        }
                    }
                } else if let Some(&id) = self.wd_id.get(&wd) {
                    self.dirty.insert(id);
                }
            }
        }
        if (!self.dirty.is_empty() || self.overflow) && self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Call `on_change` if due. Freed ids become reusable only here, after the walk, so callers
    /// always hear of an id's reuse before it can resolve to another path.
    fn notify(&mut self) {
        let scanning = self.sh.scanning.load(Relaxed);
        let gap = if scanning { NOTIFY_GAP_SCANNING } else { NOTIFY_GAP };
        if !self.changed || self.last_notify.elapsed() < gap {
            return;
        }
        if !scanning {
            let mut st = self.sh.store.write().unwrap();
            let p = mem::take(&mut st.pending);
            st.free.extend(p);
        }
        self.changed = false;
        self.last_notify = Instant::now();
        (self.on_change)();
    }

    fn save(&mut self) {
        let r = self.sh.store.read().unwrap().save(&self.cache, self.scope.hash);
        if let Err(e) = r {
            self.sh.set_error(format!("{}: {e}", self.cache.display()));
        }
        self.cache_dirty = false;
        self.last_save = Instant::now();
    }
}

/// The file name index; clones share it.
#[derive(Clone)]
pub struct FileIndex(Arc<Shared>);

impl FileIndex {
    /// Load the on-disk cache (if valid for this config), then start the background scan and
    /// inotify watcher. `on_change` is called (from a background thread, debounced ~200 ms)
    /// whenever the set of entries changed, including once after the cache load and once after
    /// the first full scan.
    pub fn start(cfg: &Files, cache_path: PathBuf, on_change: impl Fn() + Send + Sync + 'static) -> FileIndex {
        Self::start_scope(Scope::new(cfg), cache_path, Box::new(on_change))
    }

    fn start_scope(scope: Scope, cache: PathBuf, on_change: Box<dyn Fn() + Send + Sync>) -> FileIndex {
        let loaded = Store::load(&cache, &scope);
        let is_loaded = loaded.is_ok();
        let store = loaded.unwrap_or_else(|_| Store::new(&scope.roots));
        let sh = Arc::new(Shared {
            store: RwLock::new(store),
            stop: AtomicBool::new(false),
            reindex: AtomicBool::new(false),
            scanning: AtomicBool::new(true),
            watches: AtomicUsize::new(0),
            limit_hit: AtomicBool::new(false),
            error: Mutex::new(None),
            wake: unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) },
            thread: Mutex::new(None),
        });
        let ino = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if ino < 0 {
            sh.set_error(format!("inotify: {}", io::Error::last_os_error()));
        }
        let now = Instant::now();
        let ix = Indexer {
            sh: sh.clone(),
            w: Arc::new(Watcher { ino, max: scope.max_watches, sh: sh.clone() }),
            next_rescan: now + scope.rescan,
            scope: Arc::new(scope),
            cache,
            on_change,
            wd_id: HashMap::new(),
            id_wd: HashMap::new(),
            unwatched: HashSet::new(),
            dirty: HashSet::new(),
            dirty_since: None,
            overflow: false,
            changed: false,
            cache_dirty: false,
            last_notify: now,
            last_save: now,
            nesting: 0,
        };
        match thread::Builder::new().name("mmk-files".into()).spawn(move || ix.run(is_loaded)) {
            Ok(h) => *sh.thread.lock().unwrap() = Some(h),
            Err(e) => {
                sh.set_error(format!("cannot start the file index thread: {e}"));
                sh.scanning.store(false, Relaxed);
            }
        }
        FileIndex(sh)
    }

    /// Visit every live entry: (id, name, is_dir). Names are copied out in chunks, so writers
    /// wait at most one chunk.
    pub fn for_each(&self, mut f: impl FnMut(u32, &str, bool)) {
        let (mut start, mut buf, mut items) = (0, String::new(), Vec::new());
        loop {
            {
                let st = self.0.store.read().unwrap();
                if start >= st.e.len() {
                    return;
                }
                let end = (start + CHUNK).min(st.e.len());
                buf.clear();
                items.clear();
                for id in start as u32..end as u32 {
                    let e = st.at(id);
                    if e.kind == FREE || e.parent == NONE {
                        continue;
                    }
                    let a = buf.len();
                    buf.push_str(&String::from_utf8_lossy(st.name(id)));
                    items.push((id, a, buf.len(), e.kind == DIR));
                }
                start = end;
            }
            for &(id, a, b, dir) in &items {
                f(id, &buf[a..b], dir);
            }
        }
    }

    /// Full path of an id, if it still exists in the index.
    pub fn path(&self, id: u32) -> Option<PathBuf> {
        self.0.store.read().unwrap().path(id)
    }

    /// Path relative to its root ("Projects/mmk/src").
    pub fn rel_path(&self, id: u32) -> Option<String> {
        let b = self.0.store.read().unwrap().path_bytes(id, true)?;
        Some(String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    pub fn status(&self) -> IndexStatus {
        IndexStatus {
            entries: self.0.store.read().unwrap().live,
            scanning: self.0.scanning.load(Relaxed),
            watches: self.0.watches.load(Relaxed),
            watch_limit_hit: self.0.limit_hit.load(Relaxed),
            last_error: self.0.error.lock().unwrap().clone(),
        }
    }

    /// Full rescan in the background.
    pub fn reindex(&self) {
        self.0.reindex.store(true, Relaxed);
        self.0.wake();
    }

    /// Stop the index thread (saving the cache if needed) and wait for it.
    pub fn stop(&self) {
        self.0.stop.store(true, Relaxed);
        self.0.wake();
        let h = self.0.thread.lock().unwrap().take();
        if let Some(h) = h
            && h.thread().id() != thread::current().id()
        {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU32;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("mmk-index-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn put(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    fn files(root: &Path, f: impl FnOnce(&mut Files)) -> Files {
        let mut c = mmk_common::search_config::parse("").unwrap().files;
        c.roots = vec![root.to_str().unwrap().into()];
        f(&mut c);
        c
    }

    struct Ix {
        ix: FileIndex,
        calls: Arc<AtomicUsize>,
    }

    impl Ix {
        fn start(c: &Files, cache: &Path) -> Ix {
            let mut scope = Scope::new(c);
            scope.rescan = Duration::from_millis(300);
            let calls = Arc::new(AtomicUsize::new(0));
            let n = calls.clone();
            let ix = FileIndex::start_scope(scope, cache.into(), Box::new(move || _ = n.fetch_add(1, Relaxed)));
            Ix { ix, calls }
        }

        fn ready(self) -> Ix {
            wait("first scan", || !self.ix.status().scanning && self.calls.load(Relaxed) > 0);
            self
        }

        /// rel path -> (id, is_dir)
        fn all(&self) -> BTreeMap<String, (u32, bool)> {
            let mut m = BTreeMap::new();
            self.ix.for_each(|id, _, dir| {
                m.insert(self.ix.rel_path(id).unwrap(), (id, dir));
            });
            m
        }

        fn has(&self, rel: &str) -> bool {
            self.all().contains_key(rel)
        }
    }

    impl Drop for Ix {
        fn drop(&mut self) {
            self.ix.stop();
        }
    }

    fn wait(what: &str, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !ok() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn glob_matches() {
        assert!(glob(b"*.tmp", b"a.tmp") && glob(b"*", b"") && glob(b"a?c", b"abc"));
        assert!(!glob(b"*.tmp", b"a.tmpx") && !glob(b".git", b".github"));
    }

    #[test]
    fn store_table_and_paths() {
        let mut s = Store::new(&[PathBuf::from("/r")]);
        let r = s.roots[0];
        let d = s.insert(r, b"d", DIR, 0);
        let ids: Vec<u32> = (0..3000).map(|i| s.insert(d, format!("f{i}").as_bytes(), FILE, 0)).collect();
        assert_eq!(s.find(d, b"f1234"), Some(ids[1234]));
        assert_eq!(s.path(ids[7]).unwrap(), Path::new("/r/d/f7"));
        assert_eq!(s.path_bytes(ids[7], true).unwrap(), b"d/f7");
        let mut gone = Vec::new();
        assert!(s.retain_children(d, |c| c % 2 == 0, &mut gone));
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(s.find(d, format!("f{i}").as_bytes()) == Some(id), id % 2 == 0, "f{i}");
        }
        assert_eq!(s.live, 1 + ids.iter().filter(|&&i| i % 2 == 0).count());
        s.compact();
        assert_eq!(s.path(ids[2]).unwrap(), Path::new("/r/d/f2"));
        assert!(s.path(ids[1]).is_none());
    }

    #[test]
    fn scope_rules() {
        let t = Tmp::new();
        let r = &t.0;
        fs::create_dir_all(r.join(".git")).unwrap();
        fs::write(r.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        fs::write(r.join(".ignore"), "secret*\n").unwrap();
        for f in ["keep.txt", "sub/a.rs", "app.log", "build/out.bin", "secret.txt", ".dot.txt", ".hid/h.txt", "node_modules/x.js", "junk.tmp", "skip/s.txt", "sub/.git/config"] {
            put(r, f);
        }
        let skip = r.join("skip").to_str().unwrap().to_string();
        let set = |hidden: bool, respect: bool| {
            let c = files(r, |c| {
                c.exclude.extend(["*.tmp".into(), skip.clone()]);
                c.hidden = hidden;
                c.respect_ignore = respect;
            });
            let ix = Ix::start(&c, &r.join("../none.idx")).ready();
            let m = ix.all();
            assert_eq!(m.get("sub"), Some(&(m["sub"].0, true)));
            assert_eq!(ix.ix.path(m["keep.txt"].0).unwrap(), r.join("keep.txt"));
            m.into_keys().collect::<Vec<_>>()
        };
        assert_eq!(set(false, true), ["keep.txt", "sub", "sub/a.rs"]);
        let all = set(true, false);
        for f in [".dot.txt", ".hid/h.txt", "app.log", "build/out.bin", "secret.txt", ".gitignore", "keep.txt"] {
            assert!(all.iter().any(|x| x == f), "{f} missing from {all:?}");
        }
        for f in ["node_modules", "junk.tmp", "skip", ".git", "sub/.git"] {
            assert!(!all.iter().any(|x| x == f), "{f} in {all:?}");
        }
    }

    #[test]
    fn live_updates() {
        let t = Tmp::new();
        let (r, out) = (t.0.join("root"), t.0.join("outside"));
        put(&r, "a.txt");
        put(&r, "d/b.txt");
        fs::create_dir_all(&out).unwrap();
        let ix = Ix::start(&files(&r, |_| {}), &t.0.join("files.idx")).ready();
        assert!(ix.has("a.txt") && ix.has("d/b.txt"));
        assert!(ix.ix.status().watches >= 2);

        put(&r, "c.txt");
        wait("create", || ix.has("c.txt"));
        fs::create_dir_all(r.join("n/m")).unwrap();
        put(&r, "n/m/x.txt");
        wait("new folders", || ix.has("n/m/x.txt"));

        let a = ix.all()["a.txt"].0;
        let calls = ix.calls.load(Relaxed);
        fs::remove_file(r.join("a.txt")).unwrap();
        wait("delete", || !ix.has("a.txt") && ix.calls.load(Relaxed) > calls);
        assert!(ix.ix.path(a).is_none());

        fs::rename(r.join("c.txt"), r.join("c2.txt")).unwrap();
        wait("rename", || ix.has("c2.txt") && !ix.has("c.txt"));

        fs::rename(r.join("n"), r.join("n2")).unwrap();
        wait("folder rename", || ix.has("n2/m/x.txt") && !ix.has("n/m/x.txt") && !ix.has("n"));
        put(&r, "n2/m/y.txt");
        wait("create in renamed folder", || ix.has("n2/m/y.txt"));

        fs::rename(r.join("d"), out.join("d")).unwrap();
        wait("move out", || !ix.has("d") && !ix.has("d/b.txt"));
        fs::rename(out.join("d"), r.join("d3")).unwrap();
        wait("move in", || ix.has("d3/b.txt"));
        put(&out, "later.txt");
        put(&r, "d3/e.txt");
        wait("create in moved-in folder", || ix.has("d3/e.txt"));

        fs::remove_dir_all(r.join("n2")).unwrap();
        wait("delete tree", || !ix.all().keys().any(|k| k.starts_with("n2")));
        assert_eq!(ix.all().keys().collect::<Vec<_>>(), ["c2.txt", "d3", "d3/b.txt", "d3/e.txt"]);
        assert_eq!(ix.ix.status().entries, 4);
        assert_eq!(ix.ix.status().last_error, None);
    }

    #[test]
    fn cache_round_trip() {
        let t = Tmp::new();
        let r = t.0.join("root");
        for f in ["a.txt", "d/b.txt", "d/e/c.txt"] {
            put(&r, f);
        }
        let cache = t.0.join("cache/files.idx");
        let c = files(&r, |_| {});
        let before = {
            let ix = Ix::start(&c, &cache).ready();
            ix.all()
        };
        assert!(cache.exists());
        let st = Store::load(&cache, &Scope::new(&c)).unwrap();
        assert_eq!(st.live, before.len());

        // Loaded before `start` returns, with the same ids.
        let ix = Ix::start(&c, &cache);
        assert_eq!(ix.all(), before);
        let ix = ix.ready();
        wait("on_change after load and after the scan", || ix.calls.load(Relaxed) >= 2);
        drop(ix);

        // Another config, format damage: ignored.
        assert!(Store::load(&cache, &Scope::new(&files(&r, |c| c.hidden = true))).is_err());
        let mut b = fs::read(&cache).unwrap();
        let last = b.len() - 1;
        b[last] ^= 1;
        fs::write(&cache, &b).unwrap();
        assert!(Store::load(&cache, &Scope::new(&c)).is_err());
        fs::write(&cache, &b[..b.len() - 3]).unwrap();
        assert!(Store::load(&cache, &Scope::new(&c)).is_err());
        let ix = Ix::start(&c, &cache).ready();
        assert_eq!(ix.all().keys().collect::<Vec<_>>(), before.keys().collect::<Vec<_>>());
    }

    #[test]
    fn watch_limit() {
        let t = Tmp::new();
        let r = t.0.join("root");
        for i in 0..5 {
            put(&r, &format!("d{i}/f.txt"));
        }
        let ix = Ix::start(&files(&r, |c| c.max_watches = 2), &t.0.join("files.idx")).ready();
        let s = ix.ix.status();
        assert!(s.watch_limit_hit && s.watches <= 2, "{s:?}");
        assert_eq!(s.entries, 10);
        // Unwatched folders are re-listed every `rescan` (300 ms here).
        for i in 0..5 {
            put(&r, &format!("d{i}/new/g.txt"));
        }
        wait("periodic rescan", || (0..5).all(|i| ix.has(&format!("d{i}/new/g.txt"))));
        fs::remove_dir_all(r.join("d4")).unwrap();
        ix.ix.reindex();
        wait("reindex", || !ix.has("d4"));
    }

    /// `cargo test -p mmk-search-ui --no-default-features --release bench_home -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_home() {
        let c = mmk_common::search_config::parse("").unwrap().files;
        let t = Tmp::new();
        let cache = t.0.join("files.idx");
        let rss = || {
            let s = fs::read_to_string("/proc/self/status").unwrap();
            let kb = |k: &str| s.lines().find(|l| l.starts_with(k)).unwrap().split_whitespace().nth(1).unwrap().to_string();
            format!("RSS {} kB, peak {} kB", kb("VmRSS:"), kb("VmHWM:"))
        };
        let t0 = Instant::now();
        let ix = Ix::start(&c, &cache);
        wait_long(|| !ix.ix.status().scanning && ix.calls.load(Relaxed) > 0);
        let s = ix.ix.status();
        eprintln!("scan: {} entries in {:.2?}, {} watches, limit hit {}; {}", s.entries, t0.elapsed(), s.watches, s.watch_limit_hit, rss());
        let t0 = Instant::now();
        let mut n = 0;
        ix.ix.for_each(|_, name, _| n += name.len());
        eprintln!("for_each: {:.2?} ({n} name bytes)", t0.elapsed());
        drop(ix);
        let t0 = Instant::now();
        let st = Store::load(&cache, &Scope::new(&c)).unwrap();
        eprintln!(
            "cache: {} MB, loaded in {:.2?}; arena {} MB + names {} MB + table {} MB",
            fs::metadata(&cache).unwrap().len() >> 20,
            t0.elapsed(),
            (st.e.len() * 24) >> 20,
            st.names.len() >> 20,
            (st.slots.len() * 4) >> 20
        );
    }

    fn wait_long(mut ok: impl FnMut() -> bool) {
        while !ok() {
            thread::sleep(Duration::from_millis(50));
        }
    }
}
