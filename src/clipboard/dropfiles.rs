//! The list of files Explorer puts on the clipboard when you copy files
//! (`CF_HDROP`). It's a `DROPFILES` header followed by the paths, each
//! ending in a null, with one more null after the last.
//!
//! Parsed here in plain Rust, from a copy of the clipboard's memory, so
//! no `unsafe` is needed and the tests run on every platform.

/// `DROPFILES`: offset of the path list (u32), the drop point (two i32),
/// a non-client flag (i32), and whether the paths are UTF-16 (i32).
const HEADER_LEN: usize = 20;

/// The copied paths as UTF-16, or `None` if the block isn't a list we
/// can read. Explorer always writes UTF-16. The older single-byte form
/// depends on the PC's code page, so it's refused rather than guessed.
pub fn parse(block: &[u8]) -> Option<Vec<Vec<u16>>> {
    let header = block.get(..HEADER_LEN)?;
    let offset = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let wide = i32::from_le_bytes([header[16], header[17], header[18], header[19]]) != 0;
    if !wide {
        return None;
    }
    let list = block.get(usize::try_from(offset).ok()?..)?;

    let mut paths = Vec::new();
    let mut path = Vec::new();
    let (units, _odd_byte) = list.as_chunks::<2>();
    for &unit in units {
        let unit = u16::from_le_bytes(unit);
        if unit != 0 {
            path.push(unit);
        } else if path.is_empty() {
            break; // the extra null that ends the list
        } else {
            paths.push(std::mem::take(&mut path));
        }
    }
    // A path cut off without its null is dropped: it might be the wrong file.
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(offset: u32, wide: bool, paths: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&offset.to_le_bytes());
        b.extend_from_slice(&[0; 12]); // point and non-client flag
        b.extend_from_slice(&i32::from(wide).to_le_bytes());
        b.resize(offset as usize, 0);
        for p in paths {
            for unit in p.encode_utf16().chain([0]) {
                b.extend_from_slice(&unit.to_le_bytes());
            }
        }
        b.extend_from_slice(&[0, 0]);
        b
    }

    fn strings(paths: Vec<Vec<u16>>) -> Vec<String> {
        paths.iter().map(|p| String::from_utf16_lossy(p)).collect()
    }

    #[test]
    fn reads_the_paths_explorer_writes() {
        let b = block(20, true, &[r"C:\Users\me\Pictures\cat.jpg"]);
        assert_eq!(
            strings(parse(&b).unwrap()),
            [r"C:\Users\me\Pictures\cat.jpg"]
        );

        let b = block(20, true, &[r"C:\a.png", r"D:\фото\b.gif"]);
        assert_eq!(strings(parse(&b).unwrap()), [r"C:\a.png", r"D:\фото\b.gif"]);
    }

    #[test]
    fn honours_the_offset_and_ignores_what_follows_the_list() {
        // Some apps leave a gap after the header, and Windows may round
        // the block up with junk after the final null.
        let mut b = block(32, true, &[r"C:\x.bmp"]);
        b.extend_from_slice(b"j\0u\0n\0k\0\0\0");
        assert_eq!(strings(parse(&b).unwrap()), [r"C:\x.bmp"]);
    }

    #[test]
    fn empty_list() {
        assert_eq!(parse(&block(20, true, &[])), Some(vec![]));
    }

    #[test]
    fn refuses_what_it_cant_read_safely() {
        assert_eq!(parse(&[0; 10]), None);
        assert_eq!(parse(&block(20, false, &["C:\\a.png"])), None);
        let mut far = block(20, true, &["C:\\a.png"]);
        far[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&far), None);
    }

    #[test]
    fn drops_a_path_cut_short() {
        let mut b = block(20, true, &[r"C:\a.png", r"C:\b.png"]);
        b.truncate(b.len() - 4); // lose the last path's null and the end mark
        assert_eq!(strings(parse(&b).unwrap()), [r"C:\a.png"]);
    }
}
