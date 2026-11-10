//! Turns a client-supplied filename into one that is safe to create inside
//! the shared folder, on Windows, Linux, and macOS.
//!
//! The result never contains a path separator, so joining it onto the shared
//! folder can't escape it.

/// Byte cap for a sanitized name. Filesystems allow 255, and this leaves room
/// for the `" (123)"` collision suffix and the `.beam-xxxxxxxx.part` temp suffix.
pub const MAX_NAME_BYTES: usize = 200;

/// Extensions longer than this are treated as part of the name when
/// truncating, so a name with a stray dot isn't cut down to a meaningless stem.
const MAX_EXT_BYTES: usize = 20;

/// Characters Windows forbids in filenames. `/` and `\` are already gone by
/// the time this runs, but are listed so the set matches the documentation.
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Device names Windows reserves, with or without an extension (`CON.txt`
/// is as unusable as `CON`). Newer Windows versions also reserve the digit
/// 0 and superscript digits.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
    "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Returns a safe filename. Never fails: if nothing usable is left, it
/// generates a name like `file-1a2b3c4d`.
pub fn sanitize_filename(raw: &str) -> String {
    // Browsers normally send only the base name, but older ones sent full
    // paths and attackers send whatever they like. Keep the last component
    // for either separator style.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");

    let cleaned: String = base.chars().filter(|&c| is_allowed(c)).collect();
    let mut name = trim(&cleaned).to_owned();

    // "." and ".." trim down to nothing, so this also covers traversal names.
    if name.is_empty() {
        return fallback_name();
    }

    if name.len() > MAX_NAME_BYTES {
        name = trim(&truncate_keeping_extension(&name, MAX_NAME_BYTES)).to_owned();
        if name.is_empty() {
            return fallback_name();
        }
    }

    if is_reserved(&name) {
        name.insert(0, '_');
    }
    name
}

/// True for an upload's temp file, which must never be listed or served.
pub fn is_partial(name: &str) -> bool {
    name.ends_with(".part")
}

fn is_allowed(c: char) -> bool {
    !(FORBIDDEN.contains(&c) || c.is_control() || is_invisible_format(c))
}

/// Zero-width and bidirectional control characters. These are invisible
/// but can disguise a name, for example `invoice\u{202E}fdp.exe` displays
/// as `invoiceexe.pdf`.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Leading whitespace is never intentional. Windows silently drops
/// trailing dots and spaces, which would make the saved name differ from
/// what we think it is.
fn trim(s: &str) -> &str {
    s.trim_start()
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace())
}

/// Windows matches device names on the part before the first dot,
/// ignoring case and trailing spaces, so `con .tar.gz` is reserved too.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

fn truncate_keeping_extension(name: &str, max: usize) -> String {
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= MAX_EXT_BYTES
    {
        let stem_max = max - ext.len() - 1;
        return format!("{}.{ext}", truncate_bytes(stem, stem_max));
    }
    truncate_bytes(name, max).to_owned()
}

/// Longest prefix of `s` that fits in `max` bytes without splitting a character.
fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= max)
        .last()
        .unwrap_or(0);
    &s[..end]
}

fn fallback_name() -> String {
    format!("file-{:08x}", rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_fallback(name: &str) -> bool {
        name.strip_prefix("file-")
            .is_some_and(|hex| hex.len() == 8 && hex.chars().all(|c| c.is_ascii_hexdigit()))
    }

    #[test]
    fn ordinary_names_are_untouched() {
        for name in [
            "IMG_20240101_101010.jpg",
            "my file (2).pdf",
            "archive.tar.gz",
            ".gitignore",
            "a..b.txt",
            "CONSOLE.txt",
            "COM10.txt",
        ] {
            assert_eq!(sanitize_filename(name), name);
        }
    }

    #[test]
    fn traversal_keeps_only_the_last_component() {
        assert_eq!(sanitize_filename("../../evil.txt"), "evil.txt");
        assert_eq!(sanitize_filename(r"..\..\evil.txt"), "evil.txt");
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(
            sanitize_filename(r"C:\Windows\System32\evil.dll"),
            "evil.dll"
        );
        assert_eq!(sanitize_filename(r"\\server\share\evil.txt"), "evil.txt");
        assert_eq!(sanitize_filename("photos/../../x/../evil.txt"), "evil.txt");
    }

    #[test]
    fn dot_names_fall_back() {
        for name in ["..", ".", "...", "foo/..", r"foo\..", "../", ". .", "../.."] {
            let got = sanitize_filename(name);
            assert!(is_fallback(&got), "{name:?} gave {got:?}");
        }
    }

    #[test]
    fn reserved_names_get_a_prefix() {
        let cases = [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("CON.tar.gz", "_CON.tar.gz"),
            ("Com1.log", "_Com1.log"),
            ("lpt9", "_lpt9"),
            ("nul ", "_nul"),
            ("AUX .txt", "_AUX .txt"),
            ("PRN.", "_PRN"),
            ("COM0.txt", "_COM0.txt"),
            ("COM¹.txt", "_COM¹.txt"),
            ("conin$", "_conin$"),
            ("../NUL.txt", "_NUL.txt"),
        ];
        for (input, want) in cases {
            assert_eq!(sanitize_filename(input), want, "input {input:?}");
        }
    }

    #[test]
    fn forbidden_characters_are_removed() {
        assert_eq!(sanitize_filename(r#"a<b>c:d"e|f?g*h.txt"#), "abcdefgh.txt");
        // A colon would otherwise create an NTFS alternate data stream.
        assert_eq!(sanitize_filename("notes.txt:hidden"), "notes.txthidden");
    }

    #[test]
    fn control_and_invisible_characters_are_removed() {
        assert_eq!(
            sanitize_filename("a\u{0}b\u{1f}c\u{7f}d\u{9b}.txt"),
            "abcd.txt"
        );
        assert_eq!(sanitize_filename("line\nbreak\r.txt"), "linebreak.txt");
        assert_eq!(
            sanitize_filename("invoice\u{202E}fdp.exe"),
            "invoicefdp.exe"
        );
        assert_eq!(
            sanitize_filename("zero\u{200B}width\u{FEFF}.txt"),
            "zerowidth.txt"
        );
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        assert_eq!(sanitize_filename("report. . ."), "report");
        assert_eq!(sanitize_filename("  name.txt   "), "name.txt");
        assert_eq!(sanitize_filename("name.txt..."), "name.txt");
    }

    #[test]
    fn empty_results_fall_back() {
        for name in ["", "   ", "<>:|?*", "///", r"\\\", "\u{0}\u{1}", "\u{202E}"] {
            let got = sanitize_filename(name);
            assert!(is_fallback(&got), "{name:?} gave {got:?}");
        }
    }

    #[test]
    fn fallback_names_differ() {
        assert_ne!(sanitize_filename(""), sanitize_filename(""));
    }

    #[test]
    fn long_names_are_capped_and_keep_the_extension() {
        let long = format!("{}.jpg", "a".repeat(300));
        let got = sanitize_filename(&long);
        assert!(got.len() <= MAX_NAME_BYTES);
        assert!(got.ends_with(".jpg"));
        assert!(got.starts_with("aaaa"));
    }

    #[test]
    fn long_names_without_extension_are_capped() {
        let got = sanitize_filename(&"b".repeat(1000));
        assert_eq!(got.len(), MAX_NAME_BYTES);
    }

    #[test]
    fn long_extension_counts_as_part_of_the_name() {
        let long = format!("x.{}", "e".repeat(300));
        let got = sanitize_filename(&long);
        assert!(got.len() <= MAX_NAME_BYTES);
        assert!(got.starts_with("x.eee"));
    }

    #[test]
    fn long_multibyte_names_are_cut_on_a_character_boundary() {
        // 4-byte emoji and 3-byte CJK, so a naive byte cut would land mid-character.
        let long = format!("{}{}.mp4", "📷".repeat(60), "日".repeat(30));
        let got = sanitize_filename(&long);
        assert!(got.len() <= MAX_NAME_BYTES);
        assert!(got.ends_with(".mp4"));
        // Still valid UTF-8 (it's a String) and nothing was lost mid-character.
        assert!(
            got.trim_end_matches(".mp4")
                .chars()
                .all(|c| c == '📷' || c == '日')
        );
    }

    #[test]
    fn truncation_that_ends_in_a_dot_is_trimmed() {
        // The "extension" is too long to keep, so the cut lands right after the dot.
        let long = format!("{}.{}", "c".repeat(MAX_NAME_BYTES - 1), " x".repeat(50));
        assert_eq!(sanitize_filename(&long), "c".repeat(MAX_NAME_BYTES - 1));
    }

    #[test]
    fn unicode_names_are_kept() {
        for name in [
            "фото 📷.jpg",
            "日本語のファイル.txt",
            "café déjà vu.mp3",
            "صورة.png",
        ] {
            assert_eq!(sanitize_filename(name), name);
        }
    }

    #[test]
    fn partial_files_are_recognized() {
        assert!(is_partial("video.mp4.beam-1a2b3c4d.part"));
        assert!(!is_partial("video.mp4"));
    }
}
