//! Cheat codes (§16.6.6): which line a code runs, and the `{uuid}` each code keeps.

use std::path::{Path, PathBuf};

use mmk_common::search_config::Code;

/// `~/.local/state/mmk/codes.tsv`: `name \t cwd \t uuid` per line.
pub struct Ids {
    path: PathBuf,
    rows: Vec<(String, String, String)>,
}

impl Ids {
    pub fn load(path: PathBuf) -> Ids {
        let rows = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some((f.next()?.to_string(), f.next()?.to_string(), f.next()?.to_string()))
            })
            .collect();
        Ids { path, rows }
    }

    pub fn get(&self, name: &str, cwd: &str) -> Option<&str> {
        self.rows.iter().find(|(n, c, _)| n == name && c == cwd).map(|(_, _, u)| u.as_str())
    }

    pub fn set(&mut self, name: &str, cwd: &str, uuid: &str) {
        self.rows.retain(|(n, c, _)| !(n == name && c == cwd));
        self.rows.push((name.into(), cwd.into(), uuid.into()));
        let text: String = self.rows.iter().map(|(n, c, u)| format!("{n}\t{c}\t{u}\n")).collect();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

pub fn new_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid").map(|s| s.trim().to_string()).unwrap_or_else(|_| {
        // Fallback: time and pid, formatted as a v4-looking UUID.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let x = t ^ ((std::process::id() as u128) << 64);
        format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", (x >> 96) as u32, (x >> 80) as u16, (x >> 68) as u16 & 0xfff, (x >> 56) as u16 & 0xfff, x as u64 & 0xffff_ffff_ffff)
    })
}

/// Claude Code keeps a conversation as `~/.claude/projects/<dir>/<uuid>.jsonl`. Gone (deleted,
/// cleaned up after `cleanupPeriodDays`) means `--resume` would fail.
fn claude_transcript_exists(home: &Path, uuid: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(home.join(".claude/projects")) else { return false };
    rd.flatten().any(|d| d.path().join(format!("{uuid}.jsonl")).exists())
}

/// The line to run for `code` in `cwd`: `resume` when this code ran before (and, for Claude
/// Code, its conversation still exists), else `run` with a new id. `fresh` forces `run`.
/// Returns the line and the uuid to remember.
pub fn line_for(ids: &Ids, name: &str, code: &Code, cwd: &str, fresh: bool, home: &Path) -> (String, String) {
    let prev = ids.get(name, cwd).filter(|_| !fresh && !code.resume.trim().is_empty());
    if let Some(u) = prev {
        let claude = code.resume.split_whitespace().next().is_some_and(|w| w.ends_with("claude"));
        if !claude || claude_transcript_exists(home, u) {
            return (code.resume.replace("{uuid}", u), u.to_string());
        }
    }
    let u = new_uuid();
    (code.run.replace("{uuid}", &u), u)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> Code {
        mmk_common::search_config::parse("").unwrap().codes["claude"].clone()
    }

    #[test]
    fn run_then_resume() {
        let dir = std::env::temp_dir().join(format!("mmk-codes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ids = Ids::load(dir.join("codes.tsv"));
        let c = code();
        let (line, u) = line_for(&ids, "claude", &c, "~", false, &dir);
        assert_eq!(line, format!("claude --session-id {u}"));
        assert_eq!(u.len(), 36);
        ids.set("claude", "~", &u);
        // No transcript yet: start over with a new id.
        let (line2, u2) = line_for(&ids, "claude", &c, "~", false, &dir);
        assert!(line2.starts_with("claude --session-id") && u2 != u);
        // Transcript present: resume it.
        std::fs::create_dir_all(dir.join(".claude/projects/-home-x")).unwrap();
        std::fs::write(dir.join(format!(".claude/projects/-home-x/{u}.jsonl")), "").unwrap();
        assert_eq!(line_for(&ids, "claude", &c, "~", false, &dir).0, format!("claude --resume {u}"));
        assert!(line_for(&ids, "claude", &c, "~", true, &dir).0.starts_with("claude --session-id"));
        // Persisted, per directory.
        let ids2 = Ids::load(dir.join("codes.tsv"));
        assert_eq!(ids2.get("claude", "~"), Some(u.as_str()));
        assert_eq!(ids2.get("claude", "~/x"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
