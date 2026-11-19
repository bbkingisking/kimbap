use kimbap_worker::{
    self as ipc, invalid,
    pdf::{self, Request},
};
use pdfium_render::prelude::*;
use std::{
    fs::File,
    io::{self, Write},
    os::{fd::FromRawFd, unix::net::UnixStream},
    time::Instant,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("kimbap PDF worker: {error}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let library = std::env::args_os()
        .nth(1)
        .ok_or_else(|| invalid("Missing PDFium library path"))?;
    let bindings = Pdfium::bind_to_library(library)
        .map_err(|e| invalid(format!("Cannot load PDFium: {e}")))?;
    let pdfium = Pdfium::new(bindings);
    // Load trusted native code before seccomp; documents are parsed only after
    // both the namespace and syscall restrictions are active. No V8 is enabled.
    ipc::sandbox::restrict_with_fonts()?;
    let mut socket = unsafe { UnixStream::from_raw_fd(3) };
    socket.write_all(pdf::MAGIC)?;
    let mut document = None;
    while let Some(request) = pdf::read_request_pdf(&mut socket)? {
        match request {
            Request::Open(bytes) => {
                document = None;
                match pdfium.load_pdf_from_byte_vec(bytes, None) {
                    Ok(opened) if !opened.pages().is_empty() => {
                        let count = opened.pages().len() as u32;
                        document = Some(opened);
                        pdf::write_pages(&mut socket, count)?;
                    }
                    Ok(_) => ipc::write_error(&mut socket, "This PDF has no pages")?,
                    Err(PdfiumError::PdfiumLibraryInternalError(
                        PdfiumInternalError::PasswordError,
                    )) => ipc::write_error(
                        &mut socket,
                        "This PDF requires a password; open it in your document viewer",
                    )?,
                    Err(error) => {
                        ipc::write_error(&mut socket, &format!("Cannot open PDF: {error}"))?
                    }
                }
            }
            Request::Render {
                page,
                width,
                height,
            } => {
                let start = Instant::now();
                let result = document
                    .as_ref()
                    .ok_or_else(|| invalid("No PDF is open"))
                    .and_then(|doc| render(doc, page, width, height));
                match result {
                    Ok((file, w, h)) => ipc::write_frame(
                        &mut socket,
                        w,
                        h,
                        start.elapsed().as_micros() as u64,
                        &file,
                    )?,
                    Err(error) => ipc::write_error(&mut socket, &error.to_string())?,
                }
            }
        }
    }
    Ok(())
}
fn render(
    doc: &PdfDocument<'_>,
    index: u32,
    max_width: u32,
    max_height: u32,
) -> io::Result<(File, u32, u32)> {
    if index >= doc.pages().len() as u32 {
        return Err(invalid("PDF page is out of range"));
    }
    let page = doc
        .pages()
        .get(index as i32)
        .map_err(|e| invalid(e.to_string()))?;
    let (w, h) = (page.width().value as f64, page.height().value as f64);
    if !w.is_finite() || !h.is_finite() || w <= 0. || h <= 0. {
        return Err(invalid("Invalid PDF page dimensions"));
    }
    let scale = (max_width as f64 / w).min(max_height as f64 / h);
    let width = (w * scale).round().max(1.) as u32;
    let height = (h * scale).round().max(1.) as u32;
    let (file, mut pixels) = ipc::create_pixels(width, height)?;
    {
        // PDFium writes directly into the shared RGBA mapping. The opaque white
        // background also avoids premultiplied-alpha ambiguity in GTK textures.
        let mut bitmap = PdfBitmap::from_bytes(
            width as i32,
            height as i32,
            PdfBitmapFormat::BGRA,
            &mut pixels,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let config = PdfRenderConfig::new()
            .set_target_size(width as i32, height as i32)
            .set_format(PdfBitmapFormat::BGRA)
            .set_reverse_byte_order(true)
            .set_clear_color(PdfColor::WHITE);
        page.render_into_bitmap_with_config(&mut bitmap, &config)
            .map_err(|e| invalid(e.to_string()))?;
    }
    drop(pixels);
    ipc::seal_pixels(&file)?;
    Ok((file, width, height))
}
