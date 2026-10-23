//! Matching, scoring and grouping (§16.7).
//!
//! Apps and commands are few (hundreds to a few thousand), so they are scored synchronously
//! on every keystroke. Files are matched on nucleo's worker threads (files.rs) and handed in
//! as already-scored rows; this module weighs, boosts and groups everything.

use std::collections::{HashMap, HashSet};

use mmk_common::search_config::SearchConfig;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};

use crate::apps::App;
use crate::calc;
use crate::commands::{self, Command};
use crate::history::{self, History};
use crate::item::{Kind, Row};

/// Weights of an app's fields (§16.6.1).
const W_NAME: f64 = 1.0;
const W_GENERIC: f64 = 0.7;
const W_EXEC: f64 = 0.6;
const W_ID: f64 = 0.5;

#[derive(Debug)]
pub enum Outcome {
    /// The query is exactly a cheat code: open the inline terminal with it.
    Code(String),
    Results(Results),
}

#[derive(Debug, Default)]
pub struct Results {
    pub top: Option<Row>,
    /// In `groups.order`, empty groups left out.
    pub groups: Vec<(&'static str, Vec<Row>)>,
}

impl Results {
    #[cfg(test)]
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.top.iter().chain(self.groups.iter().flat_map(|(_, r)| r))
    }
}

pub struct Searcher {
    pub cfg: SearchConfig,
    pub apps: Vec<App>,
    pub commands: Vec<Command>,
    pub history: History,
    matcher: Matcher,
    buf: Vec<char>,
    /// The (first word of the) query being scored, lowercased, for `literal_bonus`.
    query_lower: String,
}

/// fzf-style scores favor tight subsequences, so "term" scores Telegram above Terminal.
/// Spotlight users type the start of a name: reward the literal text at the start of the
/// field, at a word start, or anywhere.
fn literal_bonus(field: &str, q: &str) -> f64 {
    if q.is_empty() {
        return 1.0;
    }
    let f = field.to_lowercase();
    if f.starts_with(q) {
        1.5
    } else if f.match_indices(q).any(|(i, _)| !f[..i].ends_with(char::is_alphanumeric)) {
        1.3
    } else if f.contains(q) {
        1.1
    } else {
        1.0
    }
}

impl Searcher {
    pub fn new(cfg: SearchConfig, apps: Vec<App>, commands: Vec<Command>, history: History) -> Self {
        Searcher { cfg, apps, commands, history, matcher: Matcher::new(nucleo::Config::DEFAULT), buf: Vec::new(), query_lower: String::new() }
    }

    /// Names of app executables, hidden from the commands source.
    pub fn app_execs(apps: &[App]) -> HashSet<String> {
        apps.iter().filter(|a| !a.exec.is_empty()).map(|a| a.exec.clone()).collect()
    }

    fn score(&mut self, p: &Pattern, s: &str) -> Option<f64> {
        if s.is_empty() {
            return None;
        }
        let fuzzy = p.score(Utf32Str::new(s, &mut self.buf), &mut self.matcher)? as f64;
        Some(fuzzy * literal_bonus(s, &self.query_lower))
    }

    /// Results for `query`. `files` are the file rows matched so far, `score` = fuzzy score.
    pub fn search(&mut self, query: &str, files: &[Row]) -> Outcome {
        if let Some((name, _)) = self.cfg.code_for(query) {
            return Outcome::Code(name.to_string());
        }
        let t = &self.cfg.terminal;
        if t.enabled
            && let Some(line) = query.strip_prefix(t.prefix.as_str())
        {
            let line = line.trim();
            let name = if line.is_empty() { "Open a shell".to_string() } else { format!("Run {line}") };
            let row = Row { kind: Kind::Run, id: String::new(), name, detail: "in terminal".into(), icon: "utilities-terminal".into(), target: line.into(), score: 0.0 };
            return Outcome::Results(Results { top: Some(row), groups: Vec::new() });
        }
        if query.trim().is_empty() {
            return Outcome::Results(if self.cfg.empty == "recent" { self.recent(files) } else { Results::default() });
        }

        let now = history::now();
        let boosts: HashMap<String, f64> = self.history.boosts(query, now).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let boost = |id: &str| boosts.get(id).copied().unwrap_or(0.0);
        let w = self.cfg.ranking.weights.clone();
        let mut groups: HashMap<&'static str, Vec<Row>> = HashMap::new();

        let p = Pattern::parse(query.trim(), CaseMatching::Smart, Normalization::Smart);
        self.query_lower = query.trim().to_lowercase();
        let apps = std::mem::take(&mut self.apps);
        for a in &apps {
            let mut best = None::<f64>;
            let mut consider = |s: Option<f64>, weight: f64| {
                if let Some(s) = s {
                    best = Some(best.map_or(s * weight, |b: f64| b.max(s * weight)));
                }
            };
            consider(self.score(&p, &a.name), W_NAME);
            consider(self.score(&p, &a.generic), W_GENERIC);
            for k in &a.keywords {
                consider(self.score(&p, k), W_GENERIC);
            }
            consider(self.score(&p, &a.exec), W_EXEC);
            consider(self.score(&p, a.id.trim_end_matches(".desktop")), W_ID);
            if let Some(s) = best {
                let id = format!("app:{}", a.id);
                let score = s * w.apps + boost(&id);
                groups.entry("apps").or_default().push(Row {
                    kind: Kind::App,
                    detail: a.generic.clone(),
                    icon: a.icon.clone(),
                    target: a.id.clone(),
                    name: a.name.clone(),
                    id,
                    score,
                });
            }
        }
        self.apps = apps;

        let calc = if self.cfg.calc.enabled { calc::eval(query) } else { None };
        // A calculation is not a command line: "5 km to mi" would offer h5cc.
        if self.cfg.commands.enabled && calc.is_none() {
            let (first, rest) = commands::split(query);
            let p = Pattern::parse(first, CaseMatching::Smart, Normalization::Smart);
            self.query_lower = first.to_lowercase();
            let cmds = std::mem::take(&mut self.commands);
            for c in &cmds {
                if let Some(s) = self.score(&p, &c.name) {
                    let id = format!("cmd:{}", c.name);
                    let line = if rest.is_empty() { c.name.clone() } else { format!("{} {rest}", c.name) };
                    let score = s * w.commands + boost(&id);
                    groups.entry("commands").or_default().push(Row {
                        kind: Kind::Command,
                        name: format!("Run {line}"),
                        detail: c.path.display().to_string(),
                        icon: "utilities-terminal".into(),
                        target: line,
                        id,
                        score,
                    });
                }
            }
            self.commands = cmds;
        }

        let app_paths: HashSet<String> = self.apps.iter().filter_map(|a| a.path.as_ref()).map(|p| p.display().to_string()).collect();
        for f in files.iter().filter(|f| !app_paths.contains(&f.target)) {
            let mut r = f.clone();
            r.score = f.score * w.files + boost(&r.id);
            groups.entry("files").or_default().push(r);
        }

        let top_calc = calc.map(|a| Row { kind: Kind::Calc, id: String::new(), name: a.clone(), detail: format!("= {}", query.trim()), icon: "accessories-calculator".into(), target: a, score: 0.0 });
        Outcome::Results(self.assemble(groups, top_calc))
    }

    /// Sort, pick the top hit (calculator answer, else the best row anywhere), limit groups.
    fn assemble(&self, mut groups: HashMap<&'static str, Vec<Row>>, top: Option<Row>) -> Results {
        for rows in groups.values_mut() {
            rows.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.name.len().cmp(&b.name.len())));
        }
        let top = top.or_else(|| {
            let best = groups
                .iter()
                .filter_map(|(g, r)| r.first().map(|r| (*g, r.score, r.name.len())))
                .max_by(|a, b| a.1.total_cmp(&b.1).then(b.2.cmp(&a.2)))?;
            Some(groups.get_mut(best.0)?.remove(0))
        });
        let g = &self.cfg.groups;
        let out = self
            .cfg
            .groups
            .order
            .iter()
            .filter_map(|name| {
                let (key, limit) = match name.as_str() {
                    "apps" => ("apps", g.apps.limit),
                    "commands" => ("commands", g.commands.limit),
                    "files" => ("files", g.files.limit),
                    _ => return None,
                };
                let mut rows = groups.remove(key)?;
                rows.truncate(limit);
                (!rows.is_empty()).then_some((key, rows))
            })
            .collect();
        Results { top, groups: out }
    }

    /// Most used items, for an empty query with `empty = "recent"`.
    fn recent(&self, files: &[Row]) -> Results {
        let mut groups: HashMap<&'static str, Vec<Row>> = HashMap::new();
        let ids = self.history.recent(history::now());
        for (rank, id) in ids.iter().enumerate() {
            let score = (ids.len() - rank) as f64;
            let row = if let Some(a) = id.strip_prefix("app:").and_then(|i| self.apps.iter().find(|a| a.id == i)) {
                Row { kind: Kind::App, id: id.clone(), name: a.name.clone(), detail: a.generic.clone(), icon: a.icon.clone(), target: a.id.clone(), score }
            } else if let Some(c) = id.strip_prefix("cmd:").and_then(|n| self.commands.iter().find(|c| c.name == n)) {
                Row { kind: Kind::Command, id: id.clone(), name: format!("Run {}", c.name), detail: c.path.display().to_string(), icon: "utilities-terminal".into(), target: c.name.clone(), score }
            } else if let Some(f) = files.iter().find(|f| &f.id == id) {
                Row { score, ..f.clone() }
            } else {
                continue;
            };
            groups.entry(row.kind.group()).or_default().push(row);
        }
        self.assemble(groups, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn app(id: &str, name: &str, generic: &str, exec: &str) -> App {
        App { id: id.into(), name: name.into(), generic: generic.into(), keywords: vec![], exec: exec.into(), icon: String::new(), file: None, path: None }
    }

    fn searcher(name: &str) -> Searcher {
        let cfg = mmk_common::search_config::parse("").unwrap();
        let p = std::env::temp_dir().join(format!("mmk-rank-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let apps = vec![
            app("firefox.desktop", "Firefox", "Web Browser", "firefox"),
            app("nemo.desktop", "Files", "File Manager", "nemo"),
            app("org.gnome.Terminal.desktop", "Terminal", "Terminal", "gnome-terminal"),
            app("org.telegram.desktop.desktop", "Telegram", "", "telegram-desktop"),
        ];
        let cmds = vec![Command { name: "ping".into(), path: PathBuf::from("/usr/bin/ping") }, Command { name: "fish".into(), path: PathBuf::from("/usr/bin/fish") }];
        Searcher::new(cfg, apps, cmds, History::load(p, 14.0, 100))
    }

    fn results(o: Outcome) -> Results {
        match o {
            Outcome::Results(r) => r,
            Outcome::Code(c) => panic!("code {c}"),
        }
    }

    #[test]
    fn top_hit_and_groups() {
        let mut s = searcher("a");
        let r = results(s.search("fire", &[]));
        assert_eq!(r.top.as_ref().unwrap().name, "Firefox");
        assert!(r.rows().filter(|x| x.name == "Firefox").count() == 1, "top hit not repeated in its group");
        let r = results(s.search("term", &[]));
        assert_eq!(r.top.as_ref().unwrap().name, "Terminal", "literal prefix beats a tighter subsequence");
        let r = results(s.search("browser", &[]));
        assert_eq!(r.top.unwrap().name, "Firefox", "generic name matches");
        let r = results(s.search("ping 8.8.8.8", &[]));
        let top = r.top.unwrap();
        assert_eq!((top.kind, top.target.as_str()), (Kind::Command, "ping 8.8.8.8"));
    }

    #[test]
    fn history_reorders() {
        let mut s = searcher("b");
        let first = results(s.search("fi", &[])).top.unwrap().name;
        let other = if first == "Files" { "app:firefox.desktop" } else { "app:nemo.desktop" };
        for _ in 0..3 {
            s.history.record("fi", other);
        }
        let now = results(s.search("fi", &[])).top.unwrap().name;
        assert_ne!(now, first);
    }

    #[test]
    fn calc_code_and_prefix() {
        let mut s = searcher("c");
        let r = results(s.search("2*21", &[]));
        assert_eq!(r.top.unwrap().name, "42");
        assert!(matches!(s.search("@!", &[]), Outcome::Code(c) if c == "claude"));
        let r = results(s.search("> ls | wc -l", &[]));
        let top = r.top.unwrap();
        assert_eq!((top.kind, top.target.as_str()), (Kind::Run, "ls | wc -l"));
        assert!(results(s.search("", &[])).top.is_none());
    }
}
