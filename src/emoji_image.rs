//! Checks for custom emoji images. Only PNG is allowed, and animated PNG (APNG) too. The file's contents are
//! checked, not its name, so a file renamed to `.png` is still refused unless it really is a PNG.

use std::io::Cursor;

use image::codecs::png::PngDecoder;
use image::{AnimationDecoder, ImageDecoder};

/// The largest an emoji image may be, in bytes.
pub const MAX_BYTES: usize = 256 * 1024;
/// The largest width and height of an emoji image, in pixels.
pub const MAX_SIZE: u32 = 512;
/// The most frames an animated emoji may have. Each frame is decoded in full, so this bounds the memory used.
pub const MAX_FRAMES: usize = 60;

/// The first eight bytes of every PNG file.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Checks an emoji image. `Ok` when it may be stored; otherwise the reason, for the member who sent it.
pub fn check(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("emoji images can be at most {} KB", MAX_BYTES / 1024));
    }
    if !bytes.starts_with(&SIGNATURE) {
        return Err("emoji images must be PNG or animated PNG".into());
    }
    let unreadable = || "that PNG could not be read".to_string();
    let decoder = PngDecoder::new(Cursor::new(bytes)).map_err(|_| unreadable())?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
        return Err(format!("emoji images can be at most {MAX_SIZE} by {MAX_SIZE} pixels"));
    }
    let animated = decoder.is_apng().map_err(|_| unreadable())?;
    if animated {
        let decoder = decoder.apng().map_err(|_| unreadable())?;
        let mut frames = 0;
        for frame in decoder.into_frames() {
            frame.map_err(|_| "that animated PNG could not be read".to_string())?;
            frames += 1;
            if frames > MAX_FRAMES {
                return Err(format!("animated emoji can have at most {MAX_FRAMES} frames"));
            }
        }
        if frames == 0 {
            return Err("that animated PNG has no frames".into());
        }
    } else {
        let mut pixels = vec![0; decoder.total_bytes() as usize];
        decoder.read_image(&mut pixels).map_err(|_| unreadable())?;
    }
    Ok(())
}
