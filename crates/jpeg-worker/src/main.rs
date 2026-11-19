mod metadata;
use kimbap_worker::sandbox;

use kimbap_worker::{self as ipc, invalid};
use std::{
    fs::File,
    io::{self, Write},
    os::{fd::FromRawFd, unix::net::UnixStream},
    time::Instant,
};
use turbojpeg::{Colorspace, Decompressor, Image, PixelFormat};

fn main() {
    if let Err(error) = run() {
        eprintln!("kimbap JPEG worker: {error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    // FD 3 is the sole inherited IPC endpoint, supplied by the parent through bwrap.
    let mut socket = unsafe { UnixStream::from_raw_fd(3) };
    let mut decoder = Decompressor::new().map_err(|e| invalid(e.to_string()))?;
    decoder
        .set_scan_limit(100)
        .map_err(|e| invalid(e.to_string()))?;
    sandbox::restrict()?;
    socket.write_all(ipc::JPEG_MAGIC)?;
    while let Some(bytes) = ipc::read_request(&mut socket)? {
        let start = Instant::now();
        match decode(&mut decoder, &bytes) {
            Ok((file, width, height)) => ipc::write_frame(
                &mut socket,
                width,
                height,
                start.elapsed().as_micros() as u64,
                &file,
            )?,
            Err(error) => ipc::write_error(&mut socket, &error.to_string())?,
        }
    }
    Ok(())
}
fn decode(decoder: &mut Decompressor, bytes: &[u8]) -> io::Result<(File, u32, u32)> {
    let metadata = metadata::read(bytes)?;
    let header = decoder
        .read_header(bytes)
        .map_err(|e| invalid(format!("JPEG header: {e}")))?;
    let (w, h) = (header.width as u32, header.height as u32);
    let len = ipc::pixel_len(w, h)?;
    if matches!(header.colorspace, Colorspace::CMYK | Colorspace::YCCK) {
        return Err(invalid(
            "CMYK/YCCK JPEGs are not supported yet; no fallback was used",
        ));
    }
    let (width, height) = if metadata.orientation >= 5 {
        (h, w)
    } else {
        (w, h)
    };
    let (file, mut output) = ipc::create_pixels(width, height)?;
    let mut temporary = if metadata.orientation != 1 {
        vec![0; len]
    } else {
        Vec::new()
    };
    let pixels = if temporary.is_empty() {
        &mut output[..]
    } else {
        &mut temporary[..]
    };
    decoder
        .decompress(
            bytes,
            Image {
                pixels: &mut *pixels,
                width: header.width,
                pitch: header.width * 4,
                height: header.height,
                format: PixelFormat::RGBA,
            },
        )
        .map_err(|e| invalid(format!("JPEG decode: {e}")))?;
    if let Some(profile) = metadata.icc {
        let input = lcms2::Profile::new_icc(&profile)
            .map_err(|e| invalid(format!("Invalid ICC profile: {e}")))?;
        let output_profile = lcms2::Profile::new_srgb();
        if input.color_space() == lcms2::ColorSpaceSignature::GrayData {
            if header.colorspace != Colorspace::Gray {
                return Err(invalid("Grayscale ICC profile does not match this JPEG"));
            }
            let gray: Vec<u8> = pixels.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
            let transform = lcms2::Transform::<u8, u8>::new(
                &input,
                lcms2::PixelFormat::GRAY_8,
                &output_profile,
                lcms2::PixelFormat::RGBA_8,
                lcms2::Intent::Perceptual,
            )
            .map_err(|e| invalid(format!("ICC conversion: {e}")))?;
            transform.transform_pixels(&gray, pixels);
        } else {
            let transform = lcms2::Transform::<u8, u8>::new(
                &input,
                lcms2::PixelFormat::RGBA_8,
                &output_profile,
                lcms2::PixelFormat::RGBA_8,
                lcms2::Intent::Perceptual,
            )
            .map_err(|e| invalid(format!("ICC conversion: {e}")))?;
            transform.transform_in_place(pixels);
        }
        // LCMS does not copy alpha by default. JPEG output is always opaque.
        for p in pixels.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
    }
    if !temporary.is_empty() {
        for y in 0..h as usize {
            for x in 0..w as usize {
                let (dx, dy) =
                    metadata::oriented_position(x, y, w as usize, h as usize, metadata.orientation);
                let source = (y * w as usize + x) * 4;
                let dest = (dy * width as usize + dx) * 4;
                output[dest..dest + 4].copy_from_slice(&temporary[source..source + 4]);
            }
        }
    }
    drop(output);
    ipc::seal_pixels(&file)?;
    Ok((file, width, height))
}
