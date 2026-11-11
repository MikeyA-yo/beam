//! Images on the Windows clipboard. Text goes through arboard instead.
//!
//! Reading prefers the "PNG" format that browsers, Snipping Tool and
//! Office put next to the bitmap, because it keeps transparency and is
//! smaller. Otherwise we read `CF_DIB`, which Windows provides for every
//! bitmap on the clipboard. Writing sets `CF_DIB` only. Windows makes the
//! other bitmap formats from it when an app asks, so every app that
//! pastes images can take it.

#![allow(unsafe_code)]

use std::io;
use std::marker::PhantomData;
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

use super::ClipboardError;
use super::image::{self, Format, Image, ImageError, MAX_IMAGE_BYTES};

/// Standard format number, from `winuser.h`. Defined here rather than
/// pulling in all of windows-sys's OLE bindings for one constant.
const CF_DIB: u32 = 8;

/// Another app (a clipboard manager, Office) can hold the clipboard for a
/// moment. Waiting this long in total covers those without stalling.
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(25);

/// The image on the clipboard, if any.
pub fn read_image() -> Result<Option<Image>, ClipboardError> {
    let clipboard = Open::new()?;
    if let Some(format) = png_format()
        && let Some(png) = clipboard.read(format)?
        && image::is_png(&png)
    {
        return Ok(Some(Image {
            format: Format::Png,
            bytes: png,
        }));
    }
    let Some(dib) = clipboard.read(CF_DIB)? else {
        return Ok(None);
    };
    match image::bmp_from_dib(&dib) {
        Some(bmp) => Ok(Some(Image {
            format: Format::Bmp,
            bytes: bmp,
        })),
        None => {
            tracing::debug!("clipboard bitmap has a header we can't read");
            Ok(None)
        }
    }
}

/// Replaces the clipboard with a BMP from the phone.
pub fn write_image(img: &Image) -> Result<(), ClipboardError> {
    if img.format != Format::Bmp {
        return Err(ClipboardError::UnsupportedType);
    }
    let (dib, _) = image::dib_from_bmp(&img.bytes)?;
    let clipboard = Open::new()?;
    clipboard.replace(CF_DIB, dib)
}

/// The id Windows assigns the "PNG" clipboard format. Every app that
/// registers the same name gets the same id.
fn png_format() -> Option<u32> {
    const NAME: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];
    // SAFETY: `NAME` is a null-terminated UTF-16 string that outlives the call.
    match unsafe { RegisterClipboardFormatW(NAME.as_ptr()) } {
        0 => None,
        id => Some(id),
    }
}

/// The clipboard, opened by this thread. Closed on drop.
///
/// Windows ties an open clipboard to the thread that opened it, so this
/// is deliberately not `Send`.
struct Open {
    _not_send: PhantomData<*const ()>,
}

impl Open {
    fn new() -> Result<Self, ClipboardError> {
        for attempt in 1..=OPEN_ATTEMPTS {
            // SAFETY: takes no pointers. A null owner window is allowed and
            // ties the clipboard to this thread. Every successful open is
            // paired with CloseClipboard in Drop.
            if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
                return Ok(Self {
                    _not_send: PhantomData,
                });
            }
            if attempt < OPEN_ATTEMPTS {
                thread::sleep(OPEN_RETRY);
            }
        }
        tracing::debug!("OpenClipboard: {}", io::Error::last_os_error());
        Err(ClipboardError::Busy)
    }

    /// A copy of the clipboard's data in `format`, or `None` if it has none.
    fn read(&self, format: u32) -> Result<Option<Vec<u8>>, ClipboardError> {
        // SAFETY: the clipboard is open on this thread while `self` lives.
        // The handle belongs to the clipboard and stays valid until it is
        // closed or emptied, neither of which happens before we return.
        let handle = unsafe { GetClipboardData(format) };
        if handle.is_null() {
            return Ok(None);
        }
        // The formats we read (CF_DIB and registered ones) are global
        // memory handles, which is what GlobalSize and GlobalLock take.
        // SAFETY: as above, `handle` is a live global memory handle.
        let size = unsafe { GlobalSize(handle) };
        if size == 0 {
            return Ok(None);
        }
        if size > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge.into());
        }
        // SAFETY: as above.
        let data = unsafe { GlobalLock(handle) };
        if data.is_null() {
            return Ok(None);
        }
        // SAFETY: GlobalLock returned a pointer to the block's `size`
        // bytes, readable until GlobalUnlock. We copy them out first.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec();
        // SAFETY: balances the GlobalLock above. The slice is gone by now.
        unsafe { GlobalUnlock(handle) };
        Ok(Some(bytes))
    }

    /// Empties the clipboard and puts `data` on it in `format`.
    fn replace(&self, format: u32, data: &[u8]) -> Result<(), ClipboardError> {
        // SAFETY: the clipboard is open on this thread while `self` lives.
        if unsafe { EmptyClipboard() } == 0 {
            return Err(os_error("EmptyClipboard"));
        }
        // SAFETY: plain allocation, no pointers in. Checked for null below.
        let block = unsafe { GlobalAlloc(GMEM_MOVEABLE, data.len()) };
        if block.is_null() {
            return Err(os_error("GlobalAlloc"));
        }
        // SAFETY: `block` was just allocated by us and isn't shared yet.
        let dest = unsafe { GlobalLock(block) };
        if dest.is_null() {
            let err = os_error("GlobalLock");
            // SAFETY: we still own `block`; nothing else has seen it.
            unsafe { GlobalFree(block) };
            return Err(err);
        }
        // SAFETY: `dest` points to at least `data.len()` writable bytes
        // (GlobalAlloc may round up, never down), and a fresh allocation
        // can't overlap `data`.
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), dest.cast::<u8>(), data.len());
            GlobalUnlock(block);
        }
        // SAFETY: the clipboard is open and was emptied by us, `block` is an
        // unlocked movable global block, which is what SetClipboardData wants.
        // On success the system owns `block` and frees it later.
        if unsafe { SetClipboardData(format, block) }.is_null() {
            let err = os_error("SetClipboardData");
            // SAFETY: on failure ownership stays with us.
            unsafe { GlobalFree(block) };
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: `self` exists only after a successful OpenClipboard on
        // this thread, and is dropped once.
        unsafe { CloseClipboard() };
    }
}

fn os_error(call: &str) -> ClipboardError {
    ClipboardError::Failed(format!("{call}: {}", io::Error::last_os_error()))
}

/// These replace the real clipboard, so they only run on request:
/// `cargo test -- --ignored clipboard`.
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bmp() -> Vec<u8> {
        let (w, h) = (3u32, 2u32);
        let stride = 12usize;
        let mut b = Vec::new();
        b.extend_from_slice(b"BM");
        b.extend_from_slice(&(54 + stride as u32 * h).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&54u32.to_le_bytes());
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(&[0; 24]);
        b.extend((0..stride * h as usize).map(|i| (i * 7 % 256) as u8));
        b
    }

    #[test]
    #[ignore = "replaces the contents of this PC's clipboard"]
    fn clipboard_image_round_trip() {
        let sent = Image {
            format: Format::Bmp,
            bytes: sample_bmp(),
        };
        write_image(&sent).unwrap();
        let got = read_image().unwrap().expect("an image on the clipboard");
        // We put no PNG there, so it comes back as the same bitmap. Windows
        // may round the block size up, so compare the meaningful part.
        assert_eq!(got.format, Format::Bmp);
        assert_eq!(got.dimensions(), Some((3, 2)));
        assert_eq!(&got.bytes[14..14 + 40 + 24], &sent.bytes[14..]);
    }
}
