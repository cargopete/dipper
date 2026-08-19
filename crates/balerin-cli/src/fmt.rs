//! Small helpers for making numbers fit on a terminal line.

/// Human-readable byte size: 1024-based, because that is what torrent clients
/// have always shown, whatever the SI purists say.
pub fn bytes(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else if value < 10.0 {
        format!("{value:.2} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Trim a string to `width` display columns, with an ellipsis if it was cut.
pub fn truncate(text: &str, width: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(width.saturating_sub(1)).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Collapse whitespace so multi-line descriptions do not wreck a table.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_byte_sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(10_682_344), "10.2 MiB");
        assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn truncates_only_when_needed() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a rather long title", 10), "a rather …");
        // Multi-byte characters must not be split mid-codepoint.
        assert_eq!(truncate("køkken møller", 6), "køkke…");
    }

    #[test]
    fn flattens_whitespace() {
        assert_eq!(one_line("two\n  lines\there"), "two lines here");
    }
}
