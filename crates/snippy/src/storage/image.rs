use std::io::Cursor;

use png::{BitDepth, ColorType, Decoder, Encoder, Transformations};
use snippy_core::config::CaptureLimits;
use zeroize::Zeroizing;

use super::{ContentKind, Payload, StorageError};

pub(super) fn normalize(input: &[u8], limits: &CaptureLimits) -> Result<Payload, StorageError> {
    if input.len() as u64 > limits.max_clip_bytes {
        return Err(StorageError::ClipTooLarge);
    }
    let mut decoder = Decoder::new(Cursor::new(input));
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(limits.max_decoded_image_bytes)
            .map_err(|_| StorageError::InvalidLimits)?,
    });
    decoder.set_transformations(Transformations::EXPAND);
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let mut reader = decoder.read_info().map_err(decode_error)?;
    let info = reader.info();
    // Animation is intentionally unsupported: never silently save just one frame.
    if info.animation_control.is_some() {
        return Err(StorageError::InvalidContent);
    }
    let (width, height) = (info.width, info.height);
    let pixels = u64::from(width) * u64::from(height);
    let depth = if info.bit_depth == BitDepth::Sixteen {
        2
    } else {
        1
    };
    let canonical_size = pixels
        .checked_mul(4 * depth)
        .ok_or(StorageError::ClipTooLarge)?;
    if pixels == 0
        || pixels > limits.max_image_pixels
        || canonical_size > limits.max_decoded_image_bytes
    {
        return Err(StorageError::ClipTooLarge);
    }
    let decoded_size = reader
        .output_buffer_size()
        .ok_or(StorageError::ClipTooLarge)?;
    if decoded_size as u64 > limits.max_decoded_image_bytes {
        return Err(StorageError::ClipTooLarge);
    }
    let mut decoded = Zeroizing::new(vec![0; decoded_size]);
    let output = reader.next_frame(&mut decoded).map_err(decode_error)?;
    // Verify trailing chunk structure/checksums too.
    reader.finish().map_err(decode_error)?;
    let channels = match output.color_type {
        ColorType::Grayscale => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
        ColorType::Indexed => return Err(StorageError::InvalidContent),
    };
    let sample = depth as usize;
    let mut rgba = Zeroizing::new(Vec::with_capacity(canonical_size as usize));
    for pixel in decoded[..output.buffer_size()].chunks_exact(channels * sample) {
        match channels {
            1 | 2 => {
                for _ in 0..3 {
                    rgba.extend_from_slice(&pixel[..sample]);
                }
            }
            3 | 4 => rgba.extend_from_slice(&pixel[..3 * sample]),
            _ => unreachable!(),
        }
        if channels == 2 || channels == 4 {
            rgba.extend_from_slice(&pixel[(channels - 1) * sample..]);
        } else {
            rgba.extend(std::iter::repeat_n(255, sample));
        }
    }
    if rgba.len() as u64 != canonical_size {
        return Err(StorageError::InvalidContent);
    }
    let bit_depth = if sample == 2 {
        BitDepth::Sixteen
    } else {
        BitDepth::Eight
    };
    let bytes = encode(width, height, bit_depth, &rgba)?;
    if bytes.len() as u64 > limits.max_clip_bytes {
        return Err(StorageError::ClipTooLarge);
    }
    let scale = width.max(height).max(256);
    let tw = ((u64::from(width) * 256 / u64::from(scale)) as u32).max(1);
    let th = ((u64::from(height) * 256 / u64::from(scale)) as u32).max(1);
    let mut small = Zeroizing::new(Vec::with_capacity(tw as usize * th as usize * 4 * sample));
    for y in 0..th {
        for x in 0..tw {
            let sx = u64::from(x) * u64::from(width) / u64::from(tw);
            let sy = u64::from(y) * u64::from(height) / u64::from(th);
            let start = (sy * u64::from(width) + sx) as usize * 4 * sample;
            small.extend_from_slice(&rgba[start..start + 4 * sample]);
        }
    }
    let thumbnail = encode(tw, th, bit_depth, &small)?;
    if bytes.len() as u64 + thumbnail.len() as u64 > limits.max_clip_bytes {
        return Err(StorageError::ClipTooLarge);
    }
    Ok(Payload {
        kind: ContentKind::Png,
        bytes,
        thumbnail,
    })
}

fn decode_error(error: png::DecodingError) -> StorageError {
    match error {
        png::DecodingError::LimitsExceeded => StorageError::ClipTooLarge,
        _ => StorageError::InvalidContent,
    }
}

fn encode(
    width: u32,
    height: u32,
    depth: BitDepth,
    pixels: &[u8],
) -> Result<Zeroizing<Vec<u8>>, StorageError> {
    let mut bytes = Zeroizing::new(Vec::new());
    {
        let mut encoder = Encoder::new(&mut *bytes, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(depth);
        let mut writer = encoder
            .write_header()
            .map_err(|_| StorageError::InvalidContent)?;
        writer
            .write_image_data(pixels)
            .map_err(|_| StorageError::InvalidContent)?;
        writer.finish().map_err(|_| StorageError::InvalidContent)?;
    }
    Ok(bytes)
}
