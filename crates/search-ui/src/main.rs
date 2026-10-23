//! mmk-search-ui: mmk's search window, run and supervised by mmk (`--fd N`, the pipe), or
//! one-shot `--query TEXT` printing ranked results (debugging, tests).

mod apps;
mod calc;
mod codes;
mod commands;
#[cfg(feature = "gui")]
mod display;
mod files;
mod history;
mod index;
mod item;
mod launch;
mod rank;
#[cfg(feature = "gui")]
mod term;
#[cfg(feature = "gui")]
mod ui;

use std::path::PathBuf;

use mmk_common::search_config::{self, SearchConfig};

const USAGE: &str = "\
mmk-search-ui - mmk's search window (started by mmk)

usage: mmk-search-ui --fd N            serve the window on pipe fd N (mmk does this)
       mmk-search-ui --query TEXT      print ranked results and exit
options:
  -c, --config PATH   mmk config file (default: ~/.config/mmk/config.toml)
  -V, --version
";

struct Args {
    fd: Option<i32>,
    query: Option<String>,
    config: PathBuf,
}

fn parse_args() -> Result<Args, lexopt::Error> {
    use lexopt::prelude::*;
    let mut a = Args { fd: None, query: None, config: default_config_path() };
    let mut p = lexopt::Parser::from_env();
    while let Some(arg) = p.next()? {
        match arg {
            Long("fd") => a.fd = Some(p.value()?.parse()?),
            Long("query") => a.query = Some(p.value()?.string()?),
            Short('c') | Long("config") => a.config = p.value()?.into(),
            Short('V') | Long("version") => {
                println!("mmk-search-ui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Short('h') | Long("help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(a)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` and `~/...` expanded against $HOME.
pub fn expand(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(fallback))
}

fn default_config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("mmk/config.toml")
}

pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("mmk")
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("mmk")
}

/// The `[search]` config from mmk's config file; the default if the file is missing.
fn load_config(path: &std::path::Path) -> Result<SearchConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(src) => search_config::parse(&src).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => search_config::parse(""),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mmk-search-ui: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Some(q) = &args.query {
        std::process::exit(query(&args.config, q));
    }
    clean_env();
    let Some(fd) = args.fd else {
        eprintln!("mmk-search-ui: started by mmk (--fd); try `mmk search`\n\n{USAGE}");
        std::process::exit(2);
    };
    window(fd, &args.config);
}

#[cfg(feature = "gui")]
fn window(fd: i32, config: &std::path::Path) {
    // mmk sends the config first thing; the file's is used until then.
    let cfg = load_config(config).unwrap_or_else(|e| {
        eprintln!("mmk-search-ui: {e}; using the default [search] config");
        search_config::parse("").expect("default config")
    });
    std::process::exit(ui::run(fd, cfg));
}

#[cfg(not(feature = "gui"))]
fn window(_fd: i32, _config: &std::path::Path) {
    eprintln!("mmk-search-ui: built without the window (feature `gui`)");
    std::process::exit(2);
}

/// Programs started from the window must not look like children of whatever started mmk:
/// mmk started from a Claude Code session would pass on its session markers, and Claude Code
/// run from a cheat code would then save no transcript (and `resume` could not work).
fn clean_env() {
    let inherited: Vec<_> = std::env::vars_os()
        .map(|(k, _)| k)
        .filter(|k| {
            let k = k.to_string_lossy();
            k == "CLAUDECODE" || k == "CLAUDE_PID" || k == "CLAUDE_EFFORT" || k.starts_with("CLAUDE_CODE_")
        })
        .collect();
    for k in inherited {
        unsafe { std::env::remove_var(k) };
    }
}

/// `[search.apps] dirs`, expanded.
pub fn app_dirs(cfg: &SearchConfig) -> Vec<PathBuf> {
    cfg.apps.dirs.iter().map(|d| expand(d)).collect()
}

/// Build a searcher with every source loaded synchronously.
fn searcher(cfg: SearchConfig) -> rank::Searcher {
    let apps = apps::load(&app_dirs(&cfg));
    let hide = rank::Searcher::app_execs(&apps);
    let cmds = if cfg.commands.enabled { commands::load(&std::env::var("PATH").unwrap_or_default(), &hide) } else { Vec::new() };
    let hist = history::History::load(state_dir().join("search-history.tsv"), cfg.ranking.half_life_days, cfg.ranking.history_max);
    rank::Searcher::new(cfg, apps, cmds, hist)
}

/// `--query`: one line per row: group, score, name, detail, target.
fn query(config: &std::path::Path, q: &str) -> i32 {
    let cfg = match load_config(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mmk-search-ui: {e}");
            return 1;
        }
    };
    let file_rows = if cfg.files.enabled { query_files(&cfg, q) } else { Vec::new() };
    let mut s = searcher(cfg);
    match s.search(q, &file_rows) {
        rank::Outcome::Code(name) => println!("code\t{name}"),
        rank::Outcome::Results(r) => {
            let line = |g: &str, r: &item::Row| println!("{g}\t{:.1}\t{}\t{}\t{}", r.score, r.name, r.detail, r.target);
            if let Some(t) = &r.top {
                line("top", t);
            }
            for (g, rows) in &r.groups {
                rows.iter().for_each(|r| line(g, r));
            }
        }
    }
    0
}

/// File matches for `--query`: the cached index, or a full scan if there is no cache yet.
fn query_files(cfg: &SearchConfig, q: &str) -> Vec<item::Row> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let idx = index::FileIndex::start(&cfg.files, cache_dir().join("files.idx"), || {});
    let t = std::time::Instant::now();
    while idx.status().scanning && t.elapsed().as_secs() < 60 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut f = files::Files::new(idx.clone(), Arc::new(AtomicBool::new(false)));
    f.set_query(q);
    while f.tick(50) {}
    let rows = f.rows();
    idx.stop();
    rows
}
