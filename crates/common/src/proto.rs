//! Line protocol between mmk and mmk-search-ui (the pipe) and between `mmk search` and mmk
//! (the socket at `$XDG_RUNTIME_DIR/mmk/mmk.sock`).
//!
//! One message per line: a verb, then `key=value` arguments separated by spaces. Values are
//! percent-escaped (`%`, space, `=`, control characters), so any text fits on one line.
//!
//! mmk -> search-ui: `toggle [token=] [startup-id=]`, `show [query=] [token=] [startup-id=]`,
//! `hide`, `terminal [line=] [token=] [startup-id=]`, `config toml=<[search] table>`, `reindex`,
//! `status`.
//! search-ui -> mmk: `shown`, `focused`, `hidden`, `mode name=search|terminal`, `ok`,
//! `error msg=`, `status <key=value>...`.
//! client -> mmk: the requests above (forwarded to search-ui), answered with `ok` or `error msg=`.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Msg {
    pub verb: String,
    pub args: Vec<(String, String)>,
}

impl Msg {
    pub fn new(verb: &str) -> Self {
        Msg { verb: verb.into(), args: Vec::new() }
    }

    /// Builder: add an argument.
    pub fn arg(mut self, key: &str, value: impl Into<String>) -> Self {
        self.args.push((key.into(), value.into()));
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.args.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Parse one line (without its newline). `None` for a blank line.
    pub fn parse(line: &str) -> Option<Msg> {
        let mut words = line.split(' ').filter(|w| !w.is_empty());
        let verb = unescape(words.next()?);
        let args = words
            .map(|w| match w.split_once('=') {
                Some((k, v)) => (unescape(k), unescape(v)),
                None => (unescape(w), String::new()),
            })
            .collect();
        Some(Msg { verb, args })
    }
}

impl fmt::Display for Msg {
    /// The wire form, without the trailing newline.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&escape(&self.verb))?;
        for (k, v) in &self.args {
            write!(f, " {}={}", escape(k), escape(v))?;
        }
        Ok(())
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '%' || c == ' ' || c == '=' || c.is_control() {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = Msg::new("config").arg("toml", "a = 1\nb = \"x y\" # 100%\n").arg("query", "=é ");
        let line = m.to_string();
        assert!(!line.contains('\n'));
        assert_eq!(Msg::parse(&line).unwrap(), m);
        assert_eq!(Msg::parse("toggle").unwrap(), Msg::new("toggle"));
        assert_eq!(Msg::parse("mode name=terminal").unwrap().get("name"), Some("terminal"));
        assert_eq!(Msg::parse("  "), None);
        assert_eq!(Msg::parse("x a=%4").unwrap().get("a"), Some("%4"));
    }
}
