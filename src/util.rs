pub fn format_time_ago(epoch: f64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let diff = (now - epoch).max(0.0) as u64;
    match diff {
        0..=59 => format!("{diff}s ago"),
        60..=3599 => format!("{}m ago", diff / 60),
        3600..=86399 => format!("{}h ago", diff / 3600),
        86400..=2591999 => format!("{}d ago", diff / 86400),
        _ => format!("{}mo ago", diff / 2592000),
    }
}

pub fn shorten_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = path.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    path.to_string()
}

pub fn clean_text(data: &[u8]) -> String {
    // strip_ansi_escapes removes all C0 controls including \t and \n.
    // Preserve tabs and newlines by replacing with printable placeholders
    // before stripping, then restoring them after.
    const TAB_PLACEHOLDER: &[u8] = b"\xc2\xac"; // U+00AC (¬) as UTF-8
    const LF_PLACEHOLDER: &[u8] = b"\xc2\xa6"; // U+00A6 (¦) as UTF-8

    let mut preserved = Vec::with_capacity(data.len());
    for &b in data {
        match b {
            b'\t' => preserved.extend_from_slice(TAB_PLACEHOLDER),
            b'\n' => preserved.extend_from_slice(LF_PLACEHOLDER),
            _ => preserved.push(b),
        }
    }

    let stripped = strip_ansi_escapes::strip(&preserved);
    let s = String::from_utf8_lossy(&stripped);
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{00ac}' => out.push('\t'),
            '\u{00a6}' => out.push('\n'),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub fn format_duration(seconds: f64) -> String {
    if seconds < 0.0 {
        return "0ms".to_string();
    }
    if seconds < 1.0 {
        return format!("{:.0}ms", seconds * 1000.0);
    }
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    if seconds < 3600.0 {
        let mins = (seconds / 60.0).floor();
        let secs = seconds - mins * 60.0;
        return format!("{mins:.0}m {secs:.0}s");
    }
    let hours = (seconds / 3600.0).floor();
    let mins = ((seconds - hours * 3600.0) / 60.0).floor();
    format!("{hours:.0}h {mins:.0}m")
}

pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max_width {
            break;
        }
        width += w;
        end = i + c.len_utf8();
    }
    let mut out = s[..end].to_string();
    // Pad remaining columns if we stopped short (e.g. wide char didn't fit)
    while width < max_width {
        out.push(' ');
        width += 1;
    }
    out
}

/// Shorten `s` to at most `max_width` display cells, keeping the tail and
/// prefixing an ellipsis. Returns `s` unchanged when it already fits. Unlike
/// `truncate_to_width`, this does not pad.
pub fn elide_start_to_width(s: &str, max_width: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    // Reserve one cell for the leading ellipsis.
    let budget = max_width - 1;
    let mut width = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > budget {
            break;
        }
        width += w;
        start = i;
    }
    let mut out = String::with_capacity(s.len() - start + '\u{2026}'.len_utf8());
    out.push('\u{2026}');
    out.push_str(&s[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_strips_ansi() {
        let input = b"\x1b[31mhello\x1b[0m world";
        assert_eq!(clean_text(input), "hello world");
    }

    #[test]
    fn clean_text_normalizes_crlf() {
        let input = b"line1\r\nline2\r\n";
        assert_eq!(clean_text(input), "line1\nline2\n");
    }

    #[test]
    fn clean_text_strips_bare_cr() {
        let input = b"overwrite\rline";
        assert_eq!(clean_text(input), "overwriteline");
    }

    #[test]
    fn clean_text_preserves_tabs() {
        let input = b"col1\tcol2\n";
        assert_eq!(clean_text(input), "col1\tcol2\n");
    }

    #[test]
    fn clean_text_strips_control_chars() {
        let input = b"he\x07l\x08lo";
        assert_eq!(clean_text(input), "hello");
    }

    #[test]
    fn clean_text_complex_escapes() {
        let input = b"\x1b]0;title\x07\x1b[2K\x1b[1;32mgreen\x1b[0m text";
        assert_eq!(clean_text(input), "green text");
    }

    #[test]
    fn clean_text_empty() {
        assert_eq!(clean_text(b""), "");
    }

    #[test]
    fn format_duration_ms() {
        assert_eq!(format_duration(0.5), "500ms");
        assert_eq!(format_duration(0.001), "1ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(2.5), "2.5s");
        assert_eq!(format_duration(59.9), "59.9s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(90.0), "1m 30s");
        assert_eq!(format_duration(3599.0), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600.0), "1h 0m");
        assert_eq!(format_duration(7260.0), "2h 1m");
    }

    #[test]
    fn format_duration_negative() {
        assert_eq!(format_duration(-1.0), "0ms");
    }

    #[test]
    fn elide_start_fits_unchanged() {
        assert_eq!(elide_start_to_width("~/dejiny", 20), "~/dejiny");
        assert_eq!(elide_start_to_width("~/dejiny", 8), "~/dejiny");
        assert_eq!(elide_start_to_width("", 0), "");
    }

    #[test]
    fn elide_start_keeps_tail() {
        assert_eq!(elide_start_to_width("/a/very/deep/path/src", 10), "\u{2026}/path/src");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(
                elide_start_to_width("/a/very/deep/path/src", 10).as_str()
            ),
            10
        );
    }

    #[test]
    fn elide_start_tiny_widths() {
        assert_eq!(elide_start_to_width("~/dejiny", 0), "");
        assert_eq!(elide_start_to_width("~/dejiny", 1), "\u{2026}");
        assert_eq!(elide_start_to_width("~/dejiny", 2), "\u{2026}y");
    }

    #[test]
    fn elide_start_wide_chars() {
        // Each CJK char is 2 cells; a 3-cell budget fits the ellipsis plus one.
        let s = "\u{4f60}\u{597d}\u{4e16}\u{754c}";
        let out = elide_start_to_width(s, 3);
        assert_eq!(out, "\u{2026}\u{754c}");
        // Never exceeds the budget, even when a wide char straddles the boundary.
        for max in 0..=10 {
            let w = unicode_width::UnicodeWidthStr::width(elide_start_to_width(s, max).as_str());
            assert!(w <= max, "width {w} exceeded max {max}");
        }
    }
}
