//! PNGs share the JPEG worker's sealed RGBA transport and resource limits.
use kimbap_worker::{self as ipc, invalid};
use std::{
    fs::File,
    io::{self, Cursor},
};

pub fn decode(bytes: &[u8]) -> io::Result<(File, u32, u32)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| invalid(format!("PNG header: {e}")))?;
    let (width, height) = (reader.info().width, reader.info().height);
    let len = ipc::pixel_len(width, height)?;
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= len)
        .ok_or_else(|| invalid("PNG output exceeds the pixel limit"))?;
    let mut decoded = vec![0; size];
    // next_frame returns the default image for APNG; animation is intentionally
    // not scheduled by the still-image backend.
    let frame = reader
        .next_frame(&mut decoded)
        .map_err(|e| invalid(format!("PNG decode: {e}")))?;
    if frame.width != width || frame.height != height {
        return Err(invalid("PNG default frame does not cover its canvas"));
    }
    let (file, mut pixels) = ipc::create_pixels(width, height)?;
    let channels = frame.color_type.samples();
    for (source, dest) in decoded[..frame.buffer_size()]
        .chunks_exact(channels)
        .zip(pixels.chunks_exact_mut(4))
    {
        let rgba = match frame.color_type {
            png::ColorType::Rgb => [source[0], source[1], source[2], 255],
            png::ColorType::Rgba => [source[0], source[1], source[2], source[3]],
            png::ColorType::Grayscale => [source[0], source[0], source[0], 255],
            png::ColorType::GrayscaleAlpha => [source[0], source[0], source[0], source[1]],
            png::ColorType::Indexed => return Err(invalid("PNG palette was not expanded")),
        };
        dest.copy_from_slice(&rgba);
    }
    if let Some(profile) = &reader.info().icc_profile {
        let input = lcms2::Profile::new_icc(profile)
            .map_err(|e| invalid(format!("Invalid PNG ICC profile: {e}")))?;
        let output = lcms2::Profile::new_srgb();
        // Keep straight alpha untouched: LCMS does not copy it by default.
        let alpha: Vec<u8> = pixels.chunks_exact(4).map(|p| p[3]).collect();
        if input.color_space() == lcms2::ColorSpaceSignature::GrayData {
            if !matches!(
                frame.color_type,
                png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha
            ) {
                return Err(invalid("Grayscale ICC profile does not match this PNG"));
            }
            let gray: Vec<u8> = pixels.chunks_exact(4).map(|p| p[0]).collect();
            let transform = lcms2::Transform::<u8, u8>::new(
                &input,
                lcms2::PixelFormat::GRAY_8,
                &output,
                lcms2::PixelFormat::RGBA_8,
                lcms2::Intent::Perceptual,
            )
            .map_err(|e| invalid(format!("PNG ICC conversion: {e}")))?;
            transform.transform_pixels(&gray, &mut pixels);
        } else {
            let transform = lcms2::Transform::<u8, u8>::new(
                &input,
                lcms2::PixelFormat::RGBA_8,
                &output,
                lcms2::PixelFormat::RGBA_8,
                lcms2::Intent::Perceptual,
            )
            .map_err(|e| invalid(format!("PNG ICC conversion: {e}")))?;
            transform.transform_in_place(&mut pixels);
        }
        for (pixel, alpha) in pixels.chunks_exact_mut(4).zip(alpha) {
            pixel[3] = alpha;
        }
    }
    drop(pixels);
    ipc::seal_pixels(&file)?;
    Ok((file, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn encode(color: png::ColorType, depth: png::BitDepth, data: &[u8], palette: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if palette {
                encoder.set_palette(vec![255, 0, 0, 0, 255, 0]);
                encoder.set_trns(vec![0, 128]);
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        bytes
    }

    #[test]
    fn normalizes_color_depth_and_transparency() {
        for (color, depth, data, expected, palette) in [
            (
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                vec![10, 20, 30, 0, 40, 50, 60, 128],
                vec![10, 20, 30, 0, 40, 50, 60, 128],
                false,
            ),
            (
                png::ColorType::Rgb,
                png::BitDepth::Eight,
                vec![1, 2, 3, 4, 5, 6],
                vec![1, 2, 3, 255, 4, 5, 6, 255],
                false,
            ),
            (
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                vec![12, 0, 34, 128],
                vec![12, 12, 12, 0, 34, 34, 34, 128],
                false,
            ),
            (
                png::ColorType::Grayscale,
                png::BitDepth::One,
                vec![0b01000000],
                vec![0, 0, 0, 255, 255, 255, 255, 255],
                false,
            ),
            (
                png::ColorType::Grayscale,
                png::BitDepth::Sixteen,
                vec![0x12, 0x34, 0xab, 0xcd],
                vec![0x12, 0x12, 0x12, 255, 0xab, 0xab, 0xab, 255],
                false,
            ),
            (
                png::ColorType::Indexed,
                png::BitDepth::One,
                vec![0b01000000],
                vec![255, 0, 0, 0, 0, 255, 0, 128],
                true,
            ),
        ] {
            let bytes = encode(color, depth, &data, palette);
            let (mut file, width, height) = decode(&bytes).unwrap();
            assert_eq!((width, height), (2, 1));
            let mut pixels = Vec::new();
            file.read_to_end(&mut pixels).unwrap();
            assert_eq!(pixels, expected);
        }
    }

    #[test]
    fn converts_icc_without_changing_alpha() {
        for profile in [
            lcms2::Profile::new_srgb().icc().unwrap(),
            b"invalid profile".to_vec(),
        ] {
            let valid = profile.len() > 15;
            let mut bytes = Vec::new();
            {
                let mut info = png::Info::with_size(2, 1);
                info.color_type = png::ColorType::Rgba;
                info.icc_profile = Some(profile.into());
                let mut writer = png::Encoder::with_info(&mut bytes, info)
                    .unwrap()
                    .write_header()
                    .unwrap();
                writer
                    .write_image_data(&[200, 100, 50, 0, 50, 100, 200, 128])
                    .unwrap();
            }
            let result = decode(&bytes);
            if valid {
                let (mut file, _, _) = result.unwrap();
                let mut pixels = Vec::new();
                file.read_to_end(&mut pixels).unwrap();
                assert_eq!(pixels, [200, 100, 50, 0, 50, 100, 200, 128]);
            } else {
                assert!(result.unwrap_err().to_string().contains("ICC"));
            }
        }
    }

    #[test]
    fn rejects_truncated_and_oversized_images() {
        let bytes = encode(png::ColorType::Rgb, png::BitDepth::Eight, &[1; 6], false);
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
        assert!(decode(b"not a PNG").is_err());
        let mut bytes = Vec::new();
        {
            let mut writer = png::Encoder::new(&mut bytes, 20_001, 1)
                .write_header()
                .unwrap();
            writer.write_image_data(&vec![0; 20_001]).unwrap();
        }
        assert!(decode(&bytes).unwrap_err().to_string().contains("limit"));
    }
}
