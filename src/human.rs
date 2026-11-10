//! Readable sizes, speeds, and durations for the terminal.
//!
//! Uses 1024-based units labelled KB/MB/GB, the same as Windows Explorer,
//! so the numbers match what the user sees there.

use std::time::Duration;

const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];

pub fn bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let decimals = if value < 10.0 {
        2
    } else if value < 100.0 {
        1
    } else {
        0
    };
    format!("{value:.decimals$} {}", UNITS[unit])
}

pub fn rate(n: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 0.001 {
        return "instant".to_owned();
    }
    format!("{}/s", bytes((n as f64 / secs) as u64))
}

pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.00 KB");
        assert_eq!(bytes(1536), "1.50 KB");
        assert_eq!(bytes(15 * 1024 * 1024), "15.0 MB");
        assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2.00 GB");
        assert_eq!(bytes(u64::MAX), "16777216 TB");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(10 * 1024 * 1024, Duration::from_secs(2)), "5.00 MB/s");
        assert_eq!(rate(5, Duration::ZERO), "instant");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_millis(1234)), "1.2s");
        assert_eq!(duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(duration(Duration::from_secs(3725)), "1h 02m");
    }
}
