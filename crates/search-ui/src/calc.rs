//! Calculator and units, through fend (§16.6.4).

use std::time::{Duration, Instant};

struct Deadline(Instant);

impl fend_core::Interrupt for Deadline {
    fn should_interrupt(&self) -> bool {
        Instant::now() > self.0
    }
}

/// The answer, if `query` looks like a calculation and evaluates. Plain numbers and words never
/// count: a calculation has a digit and an operator, parentheses, or a conversion
/// (`5 km to mi`, `3 GiB in MB`).
pub fn eval(query: &str) -> Option<String> {
    let q = query.trim();
    if !looks_like_math(q) {
        return None;
    }
    let ctx = fend_core::Context::new();
    let deadline = Deadline(Instant::now() + Duration::from_millis(100));
    let r = fend_core::evaluate_preview_with_interrupt(q, &ctx, &deadline);
    let out = r.get_main_result().trim();
    if out.is_empty() || out == q || out.contains('\n') {
        return None;
    }
    Some(out.strip_prefix("approx. ").unwrap_or(out).to_string())
}

fn looks_like_math(q: &str) -> bool {
    if !q.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let ops = q.chars().any(|c| "+-*/^()%×÷".contains(c));
    let lower = format!(" {} ", q.to_lowercase());
    ops || [" to ", " in ", " as "].iter().any(|w| lower.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(eval("2+2").as_deref(), Some("4"));
        assert_eq!(eval("(1+2)*3").as_deref(), Some("9"));
        assert!(eval("5 km to mi").unwrap().contains("mi"));
        assert!(eval("0xff to decimal").unwrap().contains("255"));
        assert!(eval("3 GiB in MB").unwrap().contains("MB"));
    }

    #[test]
    fn not_math() {
        for q in ["42", "firefox", "mp3", "4k", "x264", "2+", "pi", "ls -la", "go to bed"] {
            assert_eq!(eval(q), None, "{q}");
        }
    }
}
