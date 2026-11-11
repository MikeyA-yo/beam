//! QR codes as SVG for the PC's browser. The terminal QR comes from
//! `qr2term` directly; this reuses its matrix so both always agree.

use std::fmt::Write as _;

use qr2term::qr::Qr;
use qr2term::render::{QrDark, QrLight};

/// The spec asks for a 4-module light border; scanners struggle without it.
const QUIET_ZONE: usize = 4;

/// An SVG drawing of the QR code for `data`, or `None` if it doesn't fit.
pub fn svg(data: &str) -> Option<String> {
    let mut matrix = Qr::from(data).ok()?.to_matrix();
    matrix.surround(QUIET_ZONE, QrLight);
    let size = matrix.size();
    let pixels = matrix.pixels();

    // One rectangle per run of dark modules in a row keeps the SVG small.
    let mut path = String::new();
    for (y, row) in pixels.chunks(size).enumerate() {
        let mut x = 0;
        while x < size {
            if row[x] == QrDark {
                let start = x;
                while x < size && row[x] == QrDark {
                    x += 1;
                }
                let _ = write!(path, "M{start} {y}h{}v1h-{}z", x - start, x - start);
            } else {
                x += 1;
            }
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" \
         shape-rendering=\"crispEdges\" role=\"img\" aria-label=\"QR code\">\
         <rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/>\
         <path d=\"{path}\" fill=\"#000\"/></svg>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redraws the SVG path onto a grid and compares it with the matrix
    /// the terminal QR is printed from, module by module.
    #[test]
    fn svg_matches_the_qr_matrix() {
        let data = "http://192.168.1.5:8080/?t=0123456789abcdef0123456789abcdef01234567";
        let svg = svg(data).unwrap();

        let mut matrix = Qr::from(data).unwrap().to_matrix();
        matrix.surround(QUIET_ZONE, QrLight);
        let size = matrix.size();
        assert!(svg.contains(&format!("viewBox=\"0 0 {size} {size}\"")));

        let path = svg.split("d=\"").nth(1).unwrap().split('"').next().unwrap();
        let mut drawn = vec![false; size * size];
        for cmd in path.split('M').filter(|c| !c.is_empty()) {
            let (pos, rest) = cmd.split_once('h').unwrap();
            let (x, y) = pos.split_once(' ').unwrap();
            let (x, y): (usize, usize) = (x.parse().unwrap(), y.parse().unwrap());
            let len: usize = rest.split_once('v').unwrap().0.parse().unwrap();
            for dx in 0..len {
                assert!(!drawn[y * size + x + dx], "module drawn twice");
                drawn[y * size + x + dx] = true;
            }
        }
        let expected: Vec<bool> = matrix.pixels().iter().map(|p| *p == QrDark).collect();
        assert_eq!(drawn, expected);
        // The quiet zone stays light.
        assert!(drawn[..QUIET_ZONE * size].iter().all(|d| !d));
    }

    #[test]
    fn too_much_data_gives_none() {
        assert!(svg(&"a".repeat(8000)).is_none());
    }
}
