use std::time::{SystemTime, UNIX_EPOCH};

pub fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn today() -> String {
    let (y, m, d, _, _, _) = civil_from_epoch(epoch_secs());
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn timestamp() -> String {
    let (y, m, d, hh, mm, ss) = civil_from_epoch(epoch_secs());
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

pub fn civil_from_epoch(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m, d)
}

pub fn truncate_middle(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let head = max_chars * 3 / 4;
    let tail = max_chars - head;
    let total = text.chars().count();
    let skipped = total - head - tail;
    let head_str: String = text.chars().take(head).collect();
    let tail_str: String = text.chars().skip(total - tail).collect();
    format!("{head_str}\n...[{skipped} chars truncated]...\n{tail_str}")
}

pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    truncate_chars(line, 160)
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let s: String = text.chars().take(max_chars).collect();
    format!("{s}…")
}

pub fn args_preview(args: &serde_json::Value) -> String {
    let parts: Vec<String> = args
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}={}", truncate_chars(&val, 80))
                })
                .collect()
        })
        .unwrap_or_default();
    truncate_chars(&parts.join(" "), 200)
}

pub fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn fmt_context(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

pub fn fmt_price(p: Option<f64>) -> String {
    match p {
        Some(v) if v > 0.0 => {
            if v < 0.1 {
                format!("{v:.4}")
            } else if v < 10.0 {
                format!("{v:.2}")
            } else {
                format!("{v:.1}")
            }
        }
        Some(_) => "0".to_string(),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        let (y, m, d, _, _, _) = civil_from_epoch(1_794_096_000);
        assert_eq!((y, m, d), (2026, 11, 8));
    }

    #[test]
    fn middle_truncation() {
        assert_eq!(truncate_middle("short", 10), "short");
        let long = "x".repeat(100);
        let out = truncate_middle(&long, 20);
        assert!(out.contains("truncated"));
        assert!(out.chars().count() < 60);
    }

    #[test]
    fn previews() {
        let args = serde_json::json!({"command": "ls -la", "timeout_ms": 5000});
        assert_eq!(args_preview(&args), "command=ls -la timeout_ms=5000");
        assert_eq!(first_line("\n\nhello\nworld"), "hello");
    }
}
