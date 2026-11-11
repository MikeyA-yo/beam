//! Just enough of the BMP and PNG formats to move images between the
//! phone's browser and the Windows clipboard, without an image library.
//!
//! The clipboard's `CF_DIB` format is a BMP file minus its 14-byte file
//! header. Phone to PC: the page draws the image on a canvas and sends a
//! plain 24-bit BMP, which we check strictly and strip to a DIB. PC to
//! phone: we send the clipboard's PNG if it has one, otherwise we put the
//! file header back on its DIB, and the browser shows either.

use thiserror::Error;

/// Largest image accepted in either direction. A 4K screenshot is about
/// 33 MB as a bitmap, a 12-megapixel photo about 36 MB.
pub const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;

const FILE_HEADER_LEN: usize = 14;
/// `BITMAPINFOHEADER`. The newer V4/V5 headers start with the same fields.
const INFO_HEADER_LEN: usize = 40;
const BI_RGB: u32 = 0;
// Only Windows reads bitmaps off the clipboard; the tests run everywhere.
#[cfg(any(windows, test))]
const BI_BITFIELDS: u32 = 3;
#[cfg(any(windows, test))]
const BI_ALPHABITFIELDS: u32 = 6;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Bmp,
}

impl Format {
    pub fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Bmp => "image/bmp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub format: Format,
    pub bytes: Vec<u8>,
}

impl Image {
    /// Width and height, read from the header.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self.format {
            Format::Png => png_dimensions(&self.bytes),
            Format::Bmp => bmp_dimensions(&self.bytes),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("that isn't a BMP image")]
    NotBmp,
    #[error("unsupported BMP image ({0})")]
    Unsupported(&'static str),
    #[error("the image is cut short")]
    Truncated,
    #[error("the image is too large (over {} MB)", MAX_IMAGE_BYTES / (1024 * 1024))]
    TooLarge,
}

/// Checks a BMP from the phone and returns its DIB, ready for `CF_DIB`,
/// plus its width and height.
///
/// Only the plain layout the page produces is accepted: a 40-byte header,
/// 24 or 32 bits per pixel, uncompressed, no palette, pixels right after
/// the header. Other apps read this straight off the clipboard, so
/// anything unusual is refused rather than passed on.
pub fn dib_from_bmp(bmp: &[u8]) -> Result<(&[u8], (u32, u32)), ImageError> {
    if bmp.len() < FILE_HEADER_LEN + INFO_HEADER_LEN || !bmp.starts_with(b"BM") {
        return Err(ImageError::NotBmp);
    }
    let pixels_at = u32_at(bmp, 10);
    let dib = &bmp[FILE_HEADER_LEN..];
    let header = Header::read(dib).ok_or(ImageError::NotBmp)?;

    if header.size as usize != INFO_HEADER_LEN {
        return Err(ImageError::Unsupported("header"));
    }
    if pixels_at as usize != FILE_HEADER_LEN + INFO_HEADER_LEN {
        return Err(ImageError::Unsupported("layout"));
    }
    if header.width <= 0 || header.height == 0 || header.height == i32::MIN {
        return Err(ImageError::Unsupported("size"));
    }
    if header.planes != 1 {
        return Err(ImageError::Unsupported("planes"));
    }
    if header.bits != 24 && header.bits != 32 {
        return Err(ImageError::Unsupported("bit depth"));
    }
    if header.compression != BI_RGB {
        return Err(ImageError::Unsupported("compression"));
    }
    if header.colors_used != 0 {
        return Err(ImageError::Unsupported("palette"));
    }

    let width = header.width.unsigned_abs();
    let height = header.height.unsigned_abs();
    // Rows are padded to 4 bytes. In u64 nothing here can overflow
    // before the size check: width and height are below 2^31.
    let stride = (u64::from(width) * u64::from(header.bits)).div_ceil(32) * 4;
    let pixel_bytes = stride * u64::from(height);
    if pixel_bytes > MAX_IMAGE_BYTES as u64 {
        return Err(ImageError::TooLarge);
    }
    let needed = INFO_HEADER_LEN + pixel_bytes as usize;
    if dib.len() < needed {
        return Err(ImageError::Truncated);
    }
    Ok((&dib[..needed], (width, height)))
}

/// Puts a BMP file header in front of a clipboard DIB, so a browser can
/// show it. `None` if the DIB's header doesn't add up.
#[cfg(any(windows, test))]
pub fn bmp_from_dib(dib: &[u8]) -> Option<Vec<u8>> {
    let header = Header::read(dib)?;
    let header_len = header.size as usize;
    if header_len < INFO_HEADER_LEN || header_len > dib.len() {
        return None;
    }
    // With the 40-byte header, the color masks follow it. V4/V5 headers
    // carry them inside.
    let masks = match header.compression {
        BI_BITFIELDS if header_len == INFO_HEADER_LEN => 12,
        BI_ALPHABITFIELDS if header_len == INFO_HEADER_LEN => 16,
        _ => 0,
    };
    let palette_entries = match (header.colors_used, header.bits) {
        (0, bits @ 1..=8) => 1u64 << bits,
        (0, _) => 0,
        (n, _) => u64::from(n),
    };
    let pixels_at = (FILE_HEADER_LEN + header_len + masks) as u64 + palette_entries * 4;
    let file_len = (FILE_HEADER_LEN + dib.len()) as u64;
    if pixels_at > file_len {
        return None;
    }
    let file_len = u32::try_from(file_len).ok()?;
    let pixels_at = u32::try_from(pixels_at).ok()?;

    let mut bmp = Vec::with_capacity(file_len as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_len.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]); // reserved
    bmp.extend_from_slice(&pixels_at.to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

pub fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 24 && bytes.starts_with(PNG_SIGNATURE) && &bytes[12..16] == b"IHDR"
}

fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if !is_png(png) {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    Some((be(16), be(20)))
}

fn bmp_dimensions(bmp: &[u8]) -> Option<(u32, u32)> {
    if !bmp.starts_with(b"BM") {
        return None;
    }
    let header = Header::read(bmp.get(FILE_HEADER_LEN..)?)?;
    Some((header.width.unsigned_abs(), header.height.unsigned_abs()))
}

/// The `BITMAPINFOHEADER` fields we look at.
struct Header {
    size: u32,
    width: i32,
    /// Negative means the rows are stored top-down.
    height: i32,
    planes: u16,
    bits: u16,
    compression: u32,
    colors_used: u32,
}

impl Header {
    fn read(dib: &[u8]) -> Option<Self> {
        if dib.len() < INFO_HEADER_LEN {
            return None;
        }
        Some(Self {
            size: u32_at(dib, 0),
            width: u32_at(dib, 4) as i32,
            height: u32_at(dib, 8) as i32,
            planes: u16::from_le_bytes([dib[12], dib[13]]),
            bits: u16::from_le_bytes([dib[14], dib[15]]),
            compression: u32_at(dib, 16),
            colors_used: u32_at(dib, 32),
        })
    }
}

/// Little-endian u32 at `at`. Callers check the length first.
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BMP laid out the way the phone page builds one.
    fn bmp(width: i32, height: i32, bits: u16) -> Vec<u8> {
        let stride = (width as usize * bits as usize).div_ceil(32) * 4;
        let pixels = stride * height.unsigned_abs() as usize;
        let mut b = Vec::new();
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&((54 + pixels) as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(&[0; 24]); // compression .. colors important
        b.extend((0..pixels).map(|i| (i % 256) as u8));
        b
    }

    fn set_u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn accepts_the_pages_bmp_and_strips_the_file_header() {
        let file = bmp(3, 2, 24);
        let (dib, dims) = dib_from_bmp(&file).unwrap();
        assert_eq!(dims, (3, 2));
        // 3 pixels * 3 bytes = 9, padded to 12 per row.
        assert_eq!(dib.len(), 40 + 12 * 2);
        assert_eq!(dib, &file[14..]);
    }

    #[test]
    fn accepts_top_down_and_32_bit() {
        assert_eq!(dib_from_bmp(&bmp(5, -4, 24)).unwrap().1, (5, 4));
        assert_eq!(dib_from_bmp(&bmp(5, 4, 32)).unwrap().0.len(), 40 + 20 * 4);
    }

    #[test]
    fn trailing_bytes_are_not_passed_on() {
        let mut file = bmp(2, 2, 24);
        file.extend_from_slice(b"junk");
        assert_eq!(dib_from_bmp(&file).unwrap().0.len(), 40 + 8 * 2);
    }

    #[test]
    fn refuses_what_isnt_a_plain_bmp() {
        assert_eq!(dib_from_bmp(b"hello"), Err(ImageError::NotBmp));
        let mut png_ish = bmp(2, 2, 24);
        png_ish[0] = b'P';
        assert_eq!(dib_from_bmp(&png_ish), Err(ImageError::NotBmp));

        let cases: [(usize, u32, &str); 6] = [
            (14, 124, "header"),         // V5 header
            (10, 60, "layout"),          // gap before the pixels
            (14 + 16, 1, "compression"), // RLE8
            (14 + 32, 16, "palette"),
            (14 + 4, 0, "size"),              // zero width
            (14 + 4, (-3i32) as u32, "size"), // negative width
        ];
        for (at, value, what) in cases {
            let mut b = bmp(4, 4, 24);
            set_u32(&mut b, at, value);
            assert_eq!(
                dib_from_bmp(&b),
                Err(ImageError::Unsupported(what)),
                "{what}"
            );
        }

        let mut eight_bit = bmp(4, 4, 24);
        eight_bit[14 + 14] = 8;
        assert_eq!(
            dib_from_bmp(&eight_bit),
            Err(ImageError::Unsupported("bit depth"))
        );

        let mut min_height = bmp(4, 4, 24);
        set_u32(&mut min_height, 14 + 8, i32::MIN as u32);
        assert_eq!(
            dib_from_bmp(&min_height),
            Err(ImageError::Unsupported("size"))
        );
    }

    #[test]
    fn refuses_short_and_huge_images() {
        let mut short = bmp(10, 10, 24);
        short.truncate(short.len() - 1);
        assert_eq!(dib_from_bmp(&short), Err(ImageError::Truncated));

        // Claims 2^31-1 square; must fail on size, not overflow or allocate.
        let mut huge = bmp(1, 1, 32);
        set_u32(&mut huge, 14 + 4, i32::MAX as u32);
        set_u32(&mut huge, 14 + 8, i32::MAX as u32);
        assert_eq!(dib_from_bmp(&huge), Err(ImageError::TooLarge));
    }

    #[test]
    fn dib_round_trips_through_a_bmp_file() {
        let file = bmp(7, 3, 24);
        let (dib, _) = dib_from_bmp(&file).unwrap();
        assert_eq!(bmp_from_dib(dib).unwrap(), file);
    }

    #[test]
    fn bmp_from_dib_accounts_for_masks_and_palettes() {
        let pixels_at = |dib: &[u8]| u32_at(&bmp_from_dib(dib).unwrap(), 10);

        // 32-bit with three masks after a 40-byte header, as Windows
        // often stores screenshots.
        let mut masked = bmp(2, 2, 32)[14..].to_vec();
        set_u32(&mut masked, 16, BI_BITFIELDS);
        masked.splice(40..40, [0u8; 12]);
        assert_eq!(pixels_at(&masked), 14 + 40 + 12);

        // A V5 header keeps its masks inside.
        let mut v5 = vec![0u8; 124 + 16];
        set_u32(&mut v5, 0, 124);
        set_u32(&mut v5, 4, 2);
        set_u32(&mut v5, 8, 2);
        v5[14] = 32;
        set_u32(&mut v5, 16, BI_BITFIELDS);
        assert_eq!(pixels_at(&v5), 14 + 124);

        // 8-bit with the default 256-entry palette.
        let mut paletted = vec![0u8; 40 + 1024 + 8];
        set_u32(&mut paletted, 0, 40);
        paletted[14] = 8;
        assert_eq!(pixels_at(&paletted), 14 + 40 + 1024);
    }

    #[test]
    fn bmp_from_dib_rejects_nonsense() {
        assert_eq!(bmp_from_dib(&[0; 10]), None);
        let mut tiny_header = vec![0u8; 64];
        set_u32(&mut tiny_header, 0, 12); // old OS/2 header
        assert_eq!(bmp_from_dib(&tiny_header), None);
        let mut big_header = vec![0u8; 64];
        set_u32(&mut big_header, 0, 500);
        assert_eq!(bmp_from_dib(&big_header), None);
        let mut big_palette = vec![0u8; 64];
        set_u32(&mut big_palette, 0, 40);
        set_u32(&mut big_palette, 32, u32::MAX);
        assert_eq!(bmp_from_dib(&big_palette), None);
    }

    #[test]
    fn reads_dimensions() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        let png = Image {
            format: Format::Png,
            bytes: png,
        };
        assert_eq!(png.dimensions(), Some((1920, 1080)));

        let bmp = Image {
            format: Format::Bmp,
            bytes: bmp(640, -480, 24),
        };
        assert_eq!(bmp.dimensions(), Some((640, 480)));

        let junk = Image {
            format: Format::Png,
            bytes: b"not a png".to_vec(),
        };
        assert_eq!(junk.dimensions(), None);
    }
}
