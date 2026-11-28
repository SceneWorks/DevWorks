//! Native raster verification for embedded Word pictures. Output is a static PNG without source
//! metadata. Source digests and relationship placements are kept separately by the caller.
use super::{DocxError, WordCoverageIssue};
use crate::documents::raster::{MAX_PIXELS, PngError, encode_png};
use devworks_core::services::documents::extraction::ExtractionControl;
use image::{ImageFormat, ImageReader, Limits};
use std::io::Cursor;

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
    let encoded = encode_png(pixels.as_raw(), width, height);
    control.check()?;
    match encoded {
        Ok(payload) => Ok(Ok(Raster {
            width,
            height,
            payload,
        })),
        Err(PngError::Limit) => Ok(Err(ImageBounds)),
        Err(PngError::Encoding) => Ok(Err(InvalidImage)),
    }
}
