//! Native raster verification for embedded Word pictures. Output is a static PNG without source
//! metadata. Source digests and relationship placements are kept separately by the caller.
use super::{DocxError, WordCoverageIssue};
use devworks_core::services::documents::extraction::ExtractionControl;
use image::{ImageEncoder, ImageFormat, ImageReader, Limits, codecs::png::PngEncoder};
use std::io::{self, Cursor, Write};

const MAX_PIXELS: u64 = 2_000_000;
const MAX_PAYLOAD: usize = 2 * 1024 * 1024;

pub(super) struct Raster {
    pub width: u32,
    pub height: u32,
    pub payload: Vec<u8>,
}

pub(super) fn read(
    bytes: &[u8],
    media: &str,
    control: &ExtractionControl<'_>,
) -> Result<Result<Raster, WordCoverageIssue>, DocxError> {
    use WordCoverageIssue::*;
    control.check()?;
    let format = match media {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => return Ok(Err(UnsupportedImage)),
    };
    if image::guess_format(bytes).ok() != Some(format) {
        return Ok(Err(InvalidImage));
    }
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_PIXELS as u32);
        limits.max_image_height = Some(MAX_PIXELS as u32);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        reader
    };
    let dimensions = reader().into_dimensions();
    control.check()?;
    let (width, height) = match dimensions {
        Ok(size) => size,
        Err(image::ImageError::Limits(_)) => return Ok(Err(ImageBounds)),
        Err(_) => return Ok(Err(InvalidImage)),
    };
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Ok(Err(ImageBounds));
    }
    let decoded = reader().decode();
    control.check()?;
    let pixels = match decoded {
        Ok(image) => image.into_rgba8(),
        Err(image::ImageError::Limits(_)) => return Ok(Err(ImageBounds)),
        Err(_) => return Ok(Err(InvalidImage)),
    };
    if pixels.dimensions() != (width, height) {
        return Ok(Err(InvalidImage));
    }
    let mut payload = BoundedPng::default();
    let encoded = PngEncoder::new(&mut payload).write_image(
        pixels.as_raw(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    );
    control.check()?;
    if payload.full {
        return Ok(Err(ImageBounds));
    }
    if encoded.is_err() {
        return Ok(Err(InvalidImage));
    }
    Ok(Ok(Raster {
        width,
        height,
        payload: payload.bytes,
    }))
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
