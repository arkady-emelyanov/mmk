//! Usage history and frecency (§16.7.2): every open records (time, query, item id).

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
struct Pick {
    /// Unix seconds.
    time: u64,
    query: String,
    id: String,
}

pub struct History {
    path: PathBuf,
    picks: Vec<Pick>,
    half_life_days: f64,
    max: usize,
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Scale of the frecency boost against fuzzy scores (a good match scores ~50-150).
const BOOST: f64 = 40.0;
/// Extra weight of picks made with a query the current one is a prefix of.
const PREFIX_BONUS: f64 = 2.0;

impl History {
    /// Load `path` (missing = empty), keeping the newest `max` picks, and rewrite it compacted.
    pub fn load(path: PathBuf, half_life_days: f64, max: usize) -> History {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut picks: Vec<Pick> = text
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                let time = f.next()?.parse().ok()?;
                let query = f.next()?.to_string();
                let id = f.next()?.to_string();
                Some(Pick { time, query, id })
            })
            .collect();
        let over = picks.len().saturating_sub(max);
        let compact = over > 0 || picks.len() != text.lines().count();
        picks.drain(..over);
        let h = History { path, picks, half_life_days, max };
        if compact {
            h.rewrite();
        }
        h
    }

    pub fn set_params(&mut self, half_life_days: f64, max: usize) {
        self.half_life_days = half_life_days;
        self.max = max;
    }

    pub fn record(&mut self, query: &str, id: &str) {
        let p = Pick { time: now(), query: clean(query), id: clean(id) };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{}\t{}\t{}", p.time, p.query, p.id);
        }
        self.picks.push(p);
        if self.picks.len() > self.max + self.max / 4 {
            let over = self.picks.len() - self.max;
            self.picks.drain(..over);
            self.rewrite();
        }
    }

    /// Drop every pick made with `query` (case-insensitive). Returns how many.
    pub fn forget(&mut self, query: &str) -> usize {
        let q = query.trim().to_lowercase();
        let before = self.picks.len();
        self.picks.retain(|p| p.query.to_lowercase() != q);
        let n = before - self.picks.len();
        if n > 0 {
            self.rewrite();
        }
        n
    }

    /// Frecency boost per item id for `query` at time `now`.
    pub fn boosts(&self, query: &str, now: u64) -> HashMap<&str, f64> {
        let q = query.trim().to_lowercase();
        let mut f: HashMap<&str, f64> = HashMap::new();
        for p in &self.picks {
            let age_days = now.saturating_sub(p.time) as f64 / 86400.0;
            let mut w = 0.5f64.powf(age_days / self.half_life_days.max(0.01));
            if !q.is_empty() && p.query.to_lowercase().starts_with(&q) {
                w *= 1.0 + PREFIX_BONUS;
            }
            *f.entry(p.id.as_str()).or_default() += w;
        }
        f.values_mut().for_each(|v| *v = BOOST * (1.0 + *v).ln());
        f
    }

    /// Item ids by frecency, best first (for `empty = "recent"`).
    pub fn recent(&self, now: u64) -> Vec<String> {
        let b = self.boosts("", now);
        let mut ids: Vec<(&str, f64)> = b.into_iter().collect();
        ids.sort_by(|a, b| b.1.total_cmp(&a.1));
        ids.into_iter().map(|(id, _)| id.to_string()).collect()
    }

    fn rewrite(&self) {
        let mut s = String::new();
        for p in &self.picks {
            s.push_str(&format!("{}\t{}\t{}\n", p.time, p.query, p.id));
        }
        let tmp = self.path.with_extension("tmp");
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mmk-hist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn frecency_and_prefix() {
        let p = tmp("f");
        let mut h = History::load(p.clone(), 14.0, 100);
        h.record("fire", "app:firefox");
        h.record("fi", "app:files");
        let t = now();
        let b = h.boosts("fir", t);
        assert!(b["app:firefox"] > b["app:files"], "picked with a query starting with the current one");
        let b = h.boosts("", t);
        assert!((b["app:firefox"] - b["app:files"]).abs() < 1e-9);
        // Decay: a pick two half-lives old weighs a quarter.
        let old = History { path: p.clone(), picks: vec![Pick { time: t - 28 * 86400, query: "x".into(), id: "a".into() }], half_life_days: 14.0, max: 10 };
        assert!((old.boosts("", t)["a"] - BOOST * 1.25f64.ln()).abs() < 1e-6);
        // Persisted and reloaded.
        let h2 = History::load(p.clone(), 14.0, 100);
        assert_eq!(h2.picks, h.picks);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn cap_and_forget() {
        let p = tmp("c");
        let mut h = History::load(p.clone(), 14.0, 4);
        for i in 0..10 {
            h.record(&format!("q{}", i % 2), &format!("id{i}"));
        }
        assert!(h.picks.len() <= 5);
        let h = History::load(p.clone(), 14.0, 4);
        assert_eq!(h.picks.len(), 4);
        assert_eq!(h.picks.last().unwrap().id, "id9");
        let mut h = h;
        assert_eq!(h.forget("Q1"), 2);
        assert_eq!(History::load(p.clone(), 14.0, 4).picks.len(), 2);
        std::fs::remove_file(&p).unwrap();
    }
}
