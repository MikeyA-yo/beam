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

/// "Chrome 157 on Android 14" from a User-Agent header. Only for the
/// terminal log, so it covers the common cases and falls back gracefully.
pub fn agent(ua: &str) -> String {
    // Order matters: Edge, Samsung, and Opera all also claim to be Chrome,
    // and Chrome also claims to be Safari.
    const BROWSERS: &[(&str, &str)] = &[
        ("EdgA/", "Edge"),
        ("EdgiOS/", "Edge"),
        ("Edg/", "Edge"),
        ("SamsungBrowser/", "Samsung Internet"),
        ("OPR/", "Opera"),
        ("FxiOS/", "Firefox"),
        ("Firefox/", "Firefox"),
        ("CriOS/", "Chrome"),
        ("Chrome/", "Chrome"),
        ("Version/", "Safari"),
        ("curl/", "curl"),
    ];
    let browser = BROWSERS.iter().find_map(|(marker, name)| {
        let rest = &ua[ua.find(marker)? + marker.len()..];
        let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some(if major.is_empty() {
            (*name).to_owned()
        } else {
            format!("{name} {major}")
        })
    });

    let os = if let Some(i) = ua.find("Android") {
        let version: String = ua[i + "Android".len()..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        Some(if version.is_empty() {
            "Android".to_owned()
        } else {
            format!("Android {version}")
        })
    } else {
        [
            ("iPhone", "iPhone"),
            ("iPad", "iPad"),
            ("Windows", "Windows"),
            ("CrOS", "ChromeOS"),
            ("Mac OS X", "Mac"),
            ("Linux", "Linux"),
        ]
        .iter()
        .find(|(marker, _)| ua.contains(marker))
        .map(|(_, name)| (*name).to_owned())
    };

    match (browser, os) {
        (Some(b), Some(o)) => format!("{b} on {o}"),
        (Some(b), None) => b,
        (None, Some(o)) => format!("a browser on {o}"),
        (None, None) => "an unknown browser".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents() {
        let cases = [
            (
                "Mozilla/5.0 (Linux; Android 14; SM-A546E) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.6778.135 Mobile Safari/537.36",
                "Chrome 131 on Android 14",
            ),
            (
                "Mozilla/5.0 (Linux; Android 13; SAMSUNG SM-G991B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0.0.0 Mobile Safari/537.36",
                "Samsung Internet 23 on Android 13",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1",
                "Safari 17 on iPhone",
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36 Edg/157.0.0.0",
                "Edge 157 on Windows",
            ),
            (
                "Mozilla/5.0 (Android 14; Mobile; rv:132.0) Gecko/132.0 Firefox/132.0",
                "Firefox 132 on Android 14",
            ),
            ("curl/8.14.1", "curl 8"),
            ("", "an unknown browser"),
        ];
        for (ua, want) in cases {
            assert_eq!(agent(ua), want, "{ua}");
        }
    }

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
