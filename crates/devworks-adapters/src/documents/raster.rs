use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use std::io::{self, Write};

pub(super) const MAX_PIXELS: u64 = 2_000_000;
const MAX_PAYLOAD: usize = 2 * 1024 * 1024;

pub(super) enum PngError {
    Limit,
    Encoding,
}

/// Encodes a static raster without copying its source metadata.
pub(super) fn encode_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, PngError> {
    let mut output = BoundedPng::default();
    let result =
        PngEncoder::new(&mut output).write_image(bytes, width, height, ExtendedColorType::Rgba8);
    if output.full {
        return Err(PngError::Limit);
    }
    result.map_err(|_| PngError::Encoding)?;
    Ok(output.bytes)
}

#[derive(Default)]
struct BoundedPng {
    bytes: Vec<u8>,
    full: bool,
}

impl Write for BoundedPng {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > MAX_PAYLOAD - self.bytes.len() {
            self.full = true;
            return Err(io::Error::other("PNG exceeds payload bound"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
