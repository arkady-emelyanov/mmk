//! Files and folders (§16.6.3): the index's names matched on nucleo's worker threads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Nucleo, Utf32Str};

use crate::index::FileIndex;
use crate::item::{Kind, Row};

/// Matches taken from nucleo's sorted snapshot and rescored for ranking.
const CANDIDATES: usize = 400;
/// With folder parts in the query (`mmk/src`), how many name matches to filter by path.
const PATH_SCAN: u32 = 50_000;

pub struct Files {
    pub index: FileIndex,
    nucleo: Nucleo<(u32, bool)>,
    /// Set by the index when its entries changed: re-inject before the next query.
    pub stale: Arc<AtomicBool>,
    query: String,
    matcher: Matcher,
}

impl Files {
    pub fn new(index: FileIndex, stale: Arc<AtomicBool>) -> Files {
        let nucleo = Nucleo::new(nucleo::Config::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
        let mut f = Files { index, nucleo, stale, query: String::new(), matcher: Matcher::new(nucleo::Config::DEFAULT.match_paths()) };
        f.reload();
        f
    }

    /// Re-inject every entry (after the index changed).
    pub fn reload(&mut self) {
        self.stale.store(false, Ordering::Relaxed);
        self.nucleo.restart(true);
        let inj = self.nucleo.injector();
        self.index.for_each(|id, name, dir| {
            inj.push((id, dir), |_, cols| cols[0] = name.into());
        });
        let q = std::mem::take(&mut self.query);
        self.set_query(&q);
    }

    /// The last path segment is matched against names; earlier segments must appear, in order,
    /// in the path relative to its root (`mmk/src` finds `~/Projects/mmk/src`).
    pub fn set_query(&mut self, q: &str) {
        if self.stale.load(Ordering::Relaxed) {
            self.query = q.to_string();
            return self.reload();
        }
        let last = q.trim().trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let append = !self.query.is_empty() && q.starts_with(&self.query) && !q.contains('/');
        self.nucleo.pattern.reparse(0, last, CaseMatching::Smart, Normalization::Smart, append);
        self.query = q.to_string();
    }

    /// Let the workers run up to `ms`. Returns whether they are still running.
    pub fn tick(&mut self, ms: u64) -> bool {
        self.nucleo.tick(ms).running
    }

    /// The best matches as rows, with comparable fuzzy scores.
    pub fn rows(&mut self) -> Vec<Row> {
        let q = self.query.trim().trim_end_matches('/').to_string();
        if q.is_empty() {
            return Vec::new();
        }
        let (dirs, last) = match q.rsplit_once('/') {
            Some((d, l)) => (d.to_lowercase(), l.to_string()),
            None => (String::new(), q.clone()),
        };
        let p = Pattern::parse(&last, CaseMatching::Smart, Normalization::Smart);
        let snap = self.nucleo.snapshot();
        let n = snap.matched_item_count().min(if dirs.is_empty() { CANDIDATES as u32 } else { PATH_SCAN });
        let home = crate::home();
        let mut buf = Vec::new();
        let mut out = Vec::new();
        for item in snap.matched_items(0..n) {
            if out.len() >= CANDIDATES {
                break;
            }
            let (id, dir) = *item.data;
            let Some(path) = self.index.path(id) else { continue };
            if !dirs.is_empty() {
                let rel = self.index.rel_path(id).unwrap_or_default().to_lowercase();
                if !in_order(&rel, &dirs) {
                    continue;
                }
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(score) = p.score(Utf32Str::new(&name, &mut buf), &mut self.matcher) else { continue };
            let parent = path.parent().map(|p| abbreviate(p, &home)).unwrap_or_default();
            out.push(Row {
                kind: if dir { Kind::Dir } else { Kind::File },
                id: format!("file:{}", path.display()),
                name,
                detail: parent,
                icon: String::new(),
                target: path.display().to_string(),
                // Equal names: the shallower path first.
                score: score as f64 - 0.5 * path.components().count() as f64,
            });
        }
        out
    }
}

/// Every `/`-separated part of `dirs` appears in `rel`, in order.
fn in_order(rel: &str, dirs: &str) -> bool {
    let mut rest = rel;
    for part in dirs.split('/').filter(|p| !p.is_empty()) {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// `/home/u/Projects/mmk` -> `~/Projects/mmk`.
pub fn abbreviate(p: &std::path::Path, home: &std::path::Path) -> String {
    match p.strip_prefix(home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert!(in_order("projects/mmk/src", "mmk/src"));
        assert!(in_order("projects/mmk/src", "proj/mm"));
        assert!(!in_order("projects/mmk/src", "src/mmk"));
        let h = std::path::Path::new("/home/u");
        assert_eq!(abbreviate(std::path::Path::new("/home/u/a/b"), h), "~/a/b");
        assert_eq!(abbreviate(h, h), "~");
        assert_eq!(abbreviate(std::path::Path::new("/etc"), h), "/etc");
    }
}
