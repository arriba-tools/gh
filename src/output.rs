use anyhow::Result;
use serde::Serialize;

/// Print a value as pretty JSON on stdout.
pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Indent every line of `text` with `prefix`.
pub fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|l| format!("{prefix}{l}").trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `s` with the characters XML reserves as entities, for text and attribute values alike.
pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Keep the first `max` lines of `text`, noting how many were dropped.
pub fn first_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    if lines.len() <= max {
        return lines.join("\n");
    }
    format!(
        "{}\n[+{} lines]",
        lines[..max].join("\n"),
        lines.len() - max
    )
}

/// Align rows into columns separated by two spaces. The last column is left unpadded.
pub fn table(rows: &[Vec<String>], indent: &str) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        cell.clone()
                    } else {
                        let pad = widths[i] - cell.chars().count();
                        format!("{cell}{}", " ".repeat(pad))
                    }
                })
                .collect();
            format!("{indent}{}", cells.join("  ").trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `1 cache` or `3 caches`.
pub fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// A run time such as `45s`, `3m12s`, or `1h05m`. Negative spans, from clock skew, read as `0s`.
pub fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Byte count with a binary unit, as in `512 B` or `1.5 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A span in its largest whole unit, as in `45s`, `12m`, `5h`, or `3d`.
pub fn age(span: chrono::TimeDelta) -> String {
    match span.num_seconds().max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_counts() {
        assert_eq!(plural(1, "worktree"), "1 worktree");
        assert_eq!(plural(0, "worktree"), "0 worktrees");
    }

    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["#3".to_string(), "a".to_string(), "x".to_string()],
            vec!["#12".to_string(), "bbb".to_string(), "y".to_string()],
        ];
        assert_eq!(table(&rows, "  "), "  #3   a    x\n  #12  bbb  y");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(192), "3m12s");
        assert_eq!(duration(3900), "1h05m");
        assert_eq!(duration(-3), "0s");
    }

    #[test]
    fn first_lines_notes_dropped_count() {
        assert_eq!(first_lines("a\nb\nc", 2), "a\nb\n[+1 lines]");
        assert_eq!(first_lines("a\nb", 2), "a\nb");
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
